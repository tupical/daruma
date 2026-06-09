#!/usr/bin/env bash
# TaskAgent installer (MCPBox ecosystem tool) — Node-free, curl | bash
#   curl -fsSL https://mcpbox.ru/taskagent/install.sh | bash
#   curl -fsSL https://mcpbox.ru/taskagent/install.sh | bash -s -- --claude
#   curl -fsSL https://mcpbox.ru/taskagent/install.sh | bash -s -- --self-host
#   curl -fsSL https://mcpbox.ru/taskagent/install.sh | bash -s -- doctor
#
# Installs the Rust `taskagent-mcp` stdio binary and pairs it with a TaskAgent
# server via the OAuth device flow. No Node.js / npx required — only curl and a
# POSIX shell. `jq` is used when present but is optional.
set -euo pipefail

# ── Configuration ──────────────────────────────────────────────────────────
CLOUD_URL="${TASKAGENT_CLOUD_URL:-https://taskagent.vskideas.ru}"
SELFHOST_URL="${TASKAGENT_SELFHOST_URL:-${TASKAGENT_BASE_URL:-http://127.0.0.1:8080}}"
INSTALL_URL="${TASKAGENT_INSTALL_URL:-https://mcpbox.ru/taskagent/install.sh}"
CLIENT_ID="${TASKAGENT_OAUTH_CLIENT_ID:-claude-code-plugin}"
CONTRACT_HEADER="x-taskagent-plugin-contract: 1"
DEVICE_GRANT="urn:ietf:params:oauth:grant-type:device_code"

MODE="${TASKAGENT_MODE:-cloud}"          # cloud | self-host
INSTALL_CURSOR=false
INSTALL_CLAUDE=false
NO_OPEN=false
SKIP_LOGIN=false
SKIP_BINARY=false
INSTALL_SCOPE="${TASKAGENT_INSTALL_SCOPE:-}"   # project | global | skip
PROJECT_DIR="${TASKAGENT_PROJECT_DIR:-$PWD}"
INTERACTIVE_DEFAULT=false
DETECTED_PLUGINS_ACCEPTED=false

# Server we authenticate against / download from (set by mode).
SERVER_URL="${CLOUD_URL}"

usage() {
  cat <<EOF
TaskAgent installer — Node-free MCPBox tool (${INSTALL_URL})
  Platform: https://mcpbox.ru · GitHub github.com/tupical/taskagent

Usage:
  curl -fsSL ${INSTALL_URL} | bash
  curl -fsSL ${INSTALL_URL} | bash -s -- [options]
  curl -fsSL ${INSTALL_URL} | bash -s -- doctor [options]

Options:
  --cloud              Pair with TaskAgent Cloud (default)
  --self-host          Pair with a local OSS server (TASKAGENT_TOKEN required)
  --cursor             Install the Cursor MCP entry + rules
  --claude             Install Claude project policy + OMC guard
  --all                Install all client integrations
  --global             Save Cursor config globally (~/.cursor/mcp.json)
  --project [DIR]      Save client config in a project (default: current dir)
  --no-open            Do not open the browser during cloud device login
  --skip-login         Skip auth pairing (reuse existing credentials)
  --skip-binary        Do not download taskagent-mcp (config only)
  -h, --help           Show this help

Subcommands:
  doctor               Readiness check (exit 0 = READY)

Environment:
  TASKAGENT_CLOUD_URL       Cloud API origin (default: ${CLOUD_URL})
  TASKAGENT_SELFHOST_URL    OSS API origin for --self-host
  TASKAGENT_AGENT_DIR       Credentials dir (default: ~/.agents/taskagent)
  TASKAGENT_TOKEN           PAT for --self-host (skips device login)
  TASKAGENT_WORKSPACE_ID    Workspace id for --self-host
  TASKAGENT_INSTALL_SCOPE   Client install scope: project | global | skip
  TASKAGENT_PROJECT_DIR     Project dir for project-scoped config

Credentials: \$(agent_dir)/credentials.json   ·   Binary: \$(agent_dir)/bin/taskagent-mcp
EOF
}

# ── Small helpers ──────────────────────────────────────────────────────────
need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: required command not found: $1" >&2
    exit 1
  fi
}

agent_dir() {
  if [[ -n "${TASKAGENT_AGENT_DIR:-}" ]]; then
    printf '%s' "${TASKAGENT_AGENT_DIR%/}"
  else
    printf '%s' "${HOME}/.agents/taskagent"
  fi
}
credentials_file() { printf '%s/credentials.json' "$(agent_dir)"; }
bin_dir() { printf '%s/bin' "$(agent_dir)"; }

# Extract a flat string field from a JSON object on stdin (no jq needed).
json_str() {
  sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1
}
# Extract a flat numeric field from a JSON object on stdin.
json_num() {
  sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n1
}

maybe_open() {
  [[ "${NO_OPEN}" == true ]] && return 0
  local url="$1"
  if command -v xdg-open >/dev/null 2>&1; then xdg-open "$url" >/dev/null 2>&1 || true
  elif command -v open >/dev/null 2>&1; then open "$url" >/dev/null 2>&1 || true
  fi
}

download_platform() {
  local os; os="$(uname -s)"
  case "$os" in
    Linux) printf 'linux' ;;
    MINGW*|MSYS*|CYGWIN*) printf 'windows' ;;
    Darwin) printf 'darwin' ;;
    *) printf 'unknown' ;;
  esac
}

# ── Credentials ────────────────────────────────────────────────────────────
save_credentials() {
  local mode="$1" server_url="$2" token="$3" workspace_id="$4"
  local dir file
  dir="$(agent_dir)"
  file="$(credentials_file)"
  mkdir -p "$dir"
  chmod 700 "$dir" 2>/dev/null || true
  # Confine the restrictive umask to this write so it does not leak into the
  # rest of the run (later mkdir calls must keep their execute bit).
  ( umask 177
    cat >"$file" <<EOF
{
  "schema_version": 1,
  "active_profile": "${mode}-default",
  "profiles": {
    "${mode}-default": {
      "mode": "${mode}",
      "server_url": "${server_url%/}",
      "token": "${token}",
      "workspace_id": "${workspace_id}"
    }
  }
}
EOF
  )
  chmod 600 "$file" 2>/dev/null || true
}

cred_field() {  # $1 = token | workspace_id | server_url
  local file; file="$(credentials_file)"
  [[ -f "$file" ]] || return 1
  json_str "$1" <"$file"
}

# ── Cloud device-flow login (pure curl) ────────────────────────────────────
cloud_login() {
  need_cmd curl
  echo "→ Cloud login (${SERVER_URL})"
  local authz user_code device_code verify interval expires deadline poll token wid err

  authz="$(curl -sS -X POST "${SERVER_URL}/oauth/device/authorize" \
    -H 'content-type: application/json' -H "${CONTRACT_HEADER}" \
    -d "{\"client_id\":\"${CLIENT_ID}\",\"scope\":\"workspace:default\"}" 2>/dev/null || true)"
  device_code="$(printf '%s' "$authz" | json_str device_code)"
  if [[ -z "$device_code" ]]; then
    echo "error: device authorize failed: ${authz:-no response}" >&2
    return 1
  fi
  user_code="$(printf '%s' "$authz" | json_str user_code)"
  verify="$(printf '%s' "$authz" | json_str verification_uri_complete)"
  interval="$(printf '%s' "$authz" | json_num interval)"; interval="${interval:-5}"
  expires="$(printf '%s' "$authz" | json_num expires_in)"; expires="${expires:-600}"

  echo "  User code: ${user_code}"
  echo "  Open:      ${verify}"
  maybe_open "$verify"

  deadline=$(( $(date +%s) + expires ))
  while [[ "$(date +%s)" -lt "$deadline" ]]; do
    sleep "$interval"
    poll="$(curl -sS -X POST "${SERVER_URL}/oauth/device/token" \
      -H 'content-type: application/json' -H "${CONTRACT_HEADER}" \
      -d "{\"grant_type\":\"${DEVICE_GRANT}\",\"device_code\":\"${device_code}\",\"client_id\":\"${CLIENT_ID}\"}" 2>/dev/null || true)"
    token="$(printf '%s' "$poll" | json_str access_token)"
    if [[ -n "$token" ]]; then
      wid="$(printf '%s' "$poll" | json_str workspace_id)"
      case "$token" in ta_pat_*) : ;; *) echo "error: unexpected token from device flow" >&2; return 1 ;; esac
      [[ -n "$wid" ]] || { echo "error: missing workspace_id from device flow" >&2; return 1; }
      save_credentials cloud "$SERVER_URL" "$token" "$wid"
      echo "✓ paired ($(credentials_file))"
      return 0
    fi
    err="$(printf '%s' "$poll" | json_str error)"
    case "$err" in
      authorization_pending|"") : ;;
      slow_down) interval=$(( interval + 5 )) ;;
      *) echo "error: ${err}" >&2; return 1 ;;
    esac
  done
  echo "error: device code expired before approval" >&2
  return 1
}

# ── Self-host login (token-based; no Node) ─────────────────────────────────
selfhost_login() {
  echo "→ Self-host login (${SERVER_URL})"
  local token="${TASKAGENT_TOKEN:-}" wid="${TASKAGENT_WORKSPACE_ID:-}"
  if [[ -z "$token" && -r /dev/tty ]]; then
    printf '  Paste a self-host PAT (ta_pat_…): ' >/dev/tty
    IFS= read -r token </dev/tty || token=""
    token="${token//$'\r'/}"
  fi
  if [[ -z "$token" ]]; then
    cat >&2 <<EOF
error: self-host pairing needs a token.
  Start the OSS server, then mint a PAT and re-run with:
    TASKAGENT_TOKEN=ta_pat_… TASKAGENT_WORKSPACE_ID=… \\
      curl -fsSL ${INSTALL_URL} | bash -s -- --self-host

  OSS server:
    git clone https://github.com/tupical/taskagent
    cd taskagent && cargo build --release -p taskagent-server
    TASKAGENT_DATA_DIR=./data ./target/release/taskagent-server
EOF
    return 1
  fi
  save_credentials self-host "$SERVER_URL" "$token" "$wid"
  echo "✓ credentials saved ($(credentials_file))"
}

# ── Binary install ─────────────────────────────────────────────────────────
install_binary() {
  [[ "${SKIP_BINARY}" == true ]] && { echo "○ binary download skipped"; return 0; }
  need_cmd curl
  local platform token wid dest
  platform="$(download_platform)"
  if [[ "$platform" == darwin ]]; then
    echo "○ no prebuilt taskagent-mcp for macOS yet — build it from source:" >&2
    echo "    cargo install --git https://github.com/tupical/taskagent taskagent-mcp-bin" >&2
    return 0
  fi
  if [[ "$platform" == unknown ]]; then
    echo "○ unknown platform ($(uname -s)); skipping binary download" >&2
    return 0
  fi
  token="$(cred_field token || true)"
  wid="$(cred_field workspace_id || true)"
  if [[ -z "$token" || -z "$wid" ]]; then
    echo "○ no credentials yet — run login first to download the binary" >&2
    return 0
  fi

  local out_name="taskagent-mcp"
  [[ "$platform" == windows ]] && out_name="taskagent-mcp.exe"
  dest="$(bin_dir)/${out_name}"
  mkdir -p "$(bin_dir)"
  echo "→ Downloading taskagent-mcp (${platform})"
  if ! curl -fSL \
      -H "Authorization: Bearer ${token}" \
      -H "X-TaskAgent-Workspace-Id: ${wid}" \
      -H "${CONTRACT_HEADER}" \
      "${SERVER_URL}/v1/downloads/taskagent-mcp/${platform}" \
      -o "${dest}"; then
    echo "error: failed to download taskagent-mcp from ${SERVER_URL}" >&2
    return 1
  fi
  chmod +x "${dest}" 2>/dev/null || true
  ensure_bin_on_path
  echo "✓ installed ${dest}"
}

# ── PATH wiring ────────────────────────────────────────────────────────────
shell_rc_file() {
  if [[ -n "${TASKAGENT_SHELL_RC:-}" ]]; then printf '%s' "${TASKAGENT_SHELL_RC}"; return; fi
  case "${SHELL:-}" in
    */zsh) printf '%s' "${HOME}/.zshrc" ;;
    */bash)
      if [[ -f "${HOME}/.bashrc" || ! -f "${HOME}/.bash_profile" ]]; then printf '%s' "${HOME}/.bashrc"
      else printf '%s' "${HOME}/.bash_profile"; fi ;;
    */fish) printf '%s' "${HOME}/.config/fish/config.fish" ;;
    *) printf '%s' "${HOME}/.profile" ;;
  esac
}

ensure_bin_on_path() {
  local dir rc line
  dir="$(bin_dir)"
  case ":${PATH}:" in *":${dir}:"*) return ;; esac
  rc="$(shell_rc_file)"
  mkdir -p "$(dirname "$rc")"; touch "$rc"
  grep -Fq "$dir" "$rc" && return
  if [[ "$rc" == */config.fish ]]; then line="fish_add_path ${dir}"
  else line="export PATH=\"${dir}:\$PATH\""; fi
  { echo; echo "# taskagent PATH"; echo "$line"; } >>"$rc"
  echo "  added ${dir} to PATH in ${rc} (open a new shell)"
}

# ── Managed-block writer (pure awk, no Node) ───────────────────────────────
write_managed_block() {
  local file="$1" begin="$2" end="$3" body="$4" tmp
  mkdir -p "$(dirname "$file")"
  tmp="$(mktemp)"
  if [[ -f "$file" ]] && grep -qF "$begin" "$file" && grep -qF "$end" "$file"; then
    awk -v b="$begin" -v e="$end" -v body="$body" '
      $0==b { print b; print body; skip=1; next }
      $0==e { print e; skip=0; next }
      skip!=1 { print }
    ' "$file" >"$tmp"
  else
    { [[ -f "$file" ]] && { cat "$file"; echo; }; printf '%s\n%s\n%s\n' "$begin" "$body" "$end"; } >"$tmp"
  fi
  mv "$tmp" "$file"
}

write_file_if_missing() {
  local file="$1" content="$2"
  mkdir -p "$(dirname "$file")"
  [[ -f "$file" ]] && return 0
  printf '%s\n' "$content" >"$file"
}

# ── Claude integration ─────────────────────────────────────────────────────
claude_policy_content() {
  cat <<'EOF'
# TaskAgent — default tracker (project policy)

This project uses the taskagent MCP server as the source of truth for durable
tasks, plans, subtasks, progress, and decomposition.

Hard rules:
- Do not persist tasks or plans in markdown scratchpads as the source of truth.
- Do not create or update `.omc/plans/`, `.omc/ultragoal/`, or
  `.omc/state/plans*` for planning.
- Use `taskagent_workspace_info`, `taskagent_create`, `taskagent_plan_create`,
  `taskagent_plan_add_task`, `taskagent_comment`, and `taskagent_complete`.
- If TaskAgent is unreachable, stop and ask the user to start it.
EOF
}

omc_guard_content() {
  cat <<'EOF'
# TaskAgent override — task & plan persistence

Route durable task and plan state through taskagent. OMC may execute work, but
must not author plans under `.omc/plans/`, `.omc/ultragoal/`, or
`.omc/state/plans*`. Use `taskagent_plan_*` instead.
EOF
}

install_claude_client() {
  echo "→ Installing Claude project policy (${PROJECT_DIR})"
  write_managed_block \
    "${PROJECT_DIR}/CLAUDE.md" \
    "<!-- mcpbox-taskagent:policy:begin -->" \
    "<!-- mcpbox-taskagent:policy:end -->" \
    "$(claude_policy_content)"
  if [[ -d "${PROJECT_DIR}/.omc" ]]; then
    write_managed_block \
      "${PROJECT_DIR}/.omc/AGENTS.md" \
      "<!-- mcpbox-taskagent:begin -->" \
      "<!-- mcpbox-taskagent:end -->" \
      "$(omc_guard_content)"
  fi
  # Best-effort MCP registration when the Claude CLI is available.
  if command -v claude >/dev/null 2>&1; then
    claude mcp add taskagent taskagent-mcp >/dev/null 2>&1 \
      && echo "  registered taskagent-mcp with Claude Code" \
      || echo "  (register manually: claude mcp add taskagent taskagent-mcp)"
  else
    echo "  next: claude mcp add taskagent taskagent-mcp"
  fi
}

# ── Cursor integration ─────────────────────────────────────────────────────
cursor_mcp_path() {
  if [[ "${INSTALL_SCOPE}" == global ]]; then printf '%s' "${HOME}/.cursor/mcp.json"
  else printf '%s' "${PROJECT_DIR}/.cursor/mcp.json"; fi
}

cursor_policy_content() {
  cat <<'EOF'
---
description: Workspace policy — TaskAgent is the default tracker for tasks and plans.
globs: ["**/*"]
alwaysApply: true
---

# TaskAgent — default task & plan tracker

Use the taskagent MCP server as the source of truth for durable tasks, plans,
subtasks, progress, and decomposition. Do not create shadow task lists in
markdown or `.omc/plans/`.
EOF
}

upsert_cursor_mcp() {
  local file token wid server
  file="$(cursor_mcp_path)"
  token="$(cred_field token || true)"
  wid="$(cred_field workspace_id || true)"
  server="$(cred_field server_url || true)"; server="${server:-$SERVER_URL}"
  mkdir -p "$(dirname "$file")"
  if command -v jq >/dev/null 2>&1; then
    local base tmp; tmp="$(mktemp)"; base='{"mcpServers":{}}'
    [[ -f "$file" ]] && base="$(cat "$file")"
    printf '%s' "$base" | jq \
      --arg url "$server" --arg token "$token" --arg wid "$wid" '
      .mcpServers.taskagent = {
        type: "stdio",
        command: "taskagent-mcp",
        env: ({ TASKAGENT_API_URL: $url }
          + (if $token == "" then {} else { TASKAGENT_TOKEN: $token } end)
          + (if $wid   == "" then {} else { TASKAGENT_WORKSPACE_ID: $wid } end))
      }' >"$tmp" && mv "$tmp" "$file"
  elif [[ ! -f "$file" ]]; then
    cat >"$file" <<EOF
{
  "mcpServers": {
    "taskagent": {
      "type": "stdio",
      "command": "taskagent-mcp",
      "env": { "TASKAGENT_API_URL": "${server}" }
    }
  }
}
EOF
  else
    echo "○ ${file} exists and jq is unavailable — add the taskagent MCP entry manually" >&2
    return 0
  fi
}

install_cursor_client() {
  echo "→ Installing Cursor MCP + rules"
  upsert_cursor_mcp
  write_file_if_missing "${PROJECT_DIR}/.cursor/rules/taskagent-policy.mdc" "$(cursor_policy_content)"
}

# ── doctor ─────────────────────────────────────────────────────────────────
check_session() {
  local token wid server
  token="$(cred_field token || true)"
  wid="$(cred_field workspace_id || true)"
  server="$(cred_field server_url || true)"; server="${server:-$SERVER_URL}"
  if [[ -z "$token" || -z "$wid" ]]; then
    echo "✗ credentials missing at $(credentials_file)"; return 1
  fi
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer ${token}" \
    -H "X-TaskAgent-Workspace-Id: ${wid}" \
    -H "${CONTRACT_HEADER}" \
    "${server%/}/v1/cloud/session" 2>/dev/null || echo 000)"
  if [[ "$code" == 200 ]]; then echo "✓ session OK ($(credentials_file))"; return 0; fi
  echo "✗ session check failed (HTTP ${code}) — re-run: curl -fsSL ${INSTALL_URL} | bash"
  return 1
}

cmd_doctor() {
  local ok=true
  check_session || ok=false

  if command -v taskagent-mcp >/dev/null 2>&1 || [[ -x "$(bin_dir)/taskagent-mcp" ]]; then
    echo "✓ taskagent-mcp binary present"
  else
    echo "○ taskagent-mcp not found on PATH (re-run install, or open a new shell)"
  fi

  if [[ -f "${PROJECT_DIR}/CLAUDE.md" ]] && grep -q 'mcpbox-taskagent:policy:begin' "${PROJECT_DIR}/CLAUDE.md"; then
    echo "✓ claude policy installed"
  else
    echo "○ claude policy not installed (add: bash -s -- --claude)"
  fi

  if { [[ -f "${HOME}/.cursor/mcp.json" ]] && grep -q '"taskagent"' "${HOME}/.cursor/mcp.json"; } \
    || { [[ -f "${PROJECT_DIR}/.cursor/mcp.json" ]] && grep -q '"taskagent"' "${PROJECT_DIR}/.cursor/mcp.json"; }; then
    echo "✓ cursor MCP installed"
  else
    echo "○ cursor MCP not installed (add: bash -s -- --cursor)"
  fi

  echo
  if [[ "$ok" == true ]]; then echo "READY"; exit 0; fi
  echo "NOT READY"; exit 1
}

# ── Interactive plugin detection ───────────────────────────────────────────
prompt_detected_plugins() {
  [[ -r /dev/tty ]] || return 0
  local detected=()
  [[ -d "${HOME}/.cursor" ]] && detected+=("Cursor")
  [[ -d "${HOME}/.claude" ]] && detected+=("Claude Code")
  [[ "${#detected[@]}" -eq 0 ]] && return 0
  local answer
  printf '\nDetected %s. Install integration(s)? [y/N]: ' "${detected[*]}" >/dev/tty
  IFS= read -r answer </dev/tty || answer=""
  case "${answer//$'\r'/}" in
    y|Y|yes|YES|Yes)
      [[ -d "${HOME}/.cursor" ]] && INSTALL_CURSOR=true
      [[ -d "${HOME}/.claude" ]] && INSTALL_CLAUDE=true
      DETECTED_PLUGINS_ACCEPTED=true
      [[ -z "${INSTALL_SCOPE}" ]] && INSTALL_SCOPE=project
      ;;
  esac
}

# ── Orchestration ──────────────────────────────────────────────────────────
apply_mode() {
  if [[ "${MODE}" == self-host ]]; then SERVER_URL="${SELFHOST_URL}"; else SERVER_URL="${CLOUD_URL}"; fi
}

run_install() {
  apply_mode
  [[ -z "${INSTALL_SCOPE}" ]] && INSTALL_SCOPE=project

  if [[ "${SKIP_LOGIN}" != true ]]; then
    if [[ "${MODE}" == self-host ]]; then selfhost_login; else cloud_login; fi
  fi

  install_binary

  if [[ "${INSTALL_SCOPE}" != skip ]]; then
    [[ "${INSTALL_CURSOR}" == true ]] && install_cursor_client
    [[ "${INSTALL_CLAUDE}" == true ]] && install_claude_client
  fi

  echo
  echo "Next:"
  echo "  Verify install:  curl -fsSL ${INSTALL_URL} | bash -s -- doctor"
  [[ "${INSTALL_CLAUDE}" != true ]] && echo "  Claude policy:   curl -fsSL ${INSTALL_URL} | bash -s -- --claude"
  [[ "${INSTALL_CURSOR}" != true ]] && echo "  Cursor MCP:      curl -fsSL ${INSTALL_URL} | bash -s -- --cursor"
}

parse_args() {
  local cmd=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      doctor) cmd=doctor; shift ;;
      --cloud) MODE=cloud; shift ;;
      --self-host|--selfhost) MODE=self-host; shift ;;
      --cursor) INSTALL_CURSOR=true; shift ;;
      --claude) INSTALL_CLAUDE=true; shift ;;
      --all) INSTALL_CURSOR=true; INSTALL_CLAUDE=true; shift ;;
      --global) INSTALL_SCOPE=global; shift ;;
      --project)
        INSTALL_SCOPE=project
        if [[ $# -gt 1 && "$2" != --* ]]; then PROJECT_DIR="$2"; shift 2; else shift; fi ;;
      --no-open) NO_OPEN=true; shift ;;
      --skip-login) SKIP_LOGIN=true; shift ;;
      --skip-binary) SKIP_BINARY=true; shift ;;
      -h|--help) usage; exit 0 ;;
      --) shift; break ;;
      -*) echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
      *) echo "error: unexpected argument: $1" >&2; usage >&2; exit 2 ;;
    esac
  done

  apply_mode
  if [[ "${cmd}" == doctor ]]; then cmd_doctor; else run_install; cmd_doctor; fi
}

main() {
  if [[ $# -eq 0 ]]; then
    INTERACTIVE_DEFAULT=true
    prompt_detected_plugins
    run_install
    cmd_doctor
    return
  fi
  parse_args "$@"
}

main "$@"
