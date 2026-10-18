// Cursor-side detection for daruma-claude: probes the daruma HTTP server,
// the `daruma` binary on PATH (stdio MCP = `daruma mcp`), and the presence of
// Cursor's mcp.json.
//
// This is intentionally lighter than the claude-plugin detect.mjs — Cursor
// doesn't expose a "cursor mcp list" CLI, so we infer registration from the
// on-disk config rather than from the running app.

import { execFile } from "node:child_process";
import { promises as fs } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import {
  credentialsLocationHint,
  loadCredentials,
  resolveActiveProfile,
  resolveHttpProbeUrl,
} from "./agent-credentials.mjs";
import { globalMcpPath, projectMcpPath, readMcp } from "./mcp-config.mjs";
import { RULE_FILES } from "./rules.mjs";
import { COMMAND_FILES } from "./commands.mjs";
import { resolveMcpCommand } from "./resolve-mcp-command.mjs";

const exec = promisify(execFile);

const PROBE_TIMEOUT_MS = 4_000;
const HTTP_TIMEOUT_MS = 3_000;
const WIN_SHELL = process.platform === "win32";


function stripAnsi(text) {
  if (!text) return text;
  // eslint-disable-next-line no-control-regex
  return text.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "");
}

async function tryExec(cmd, args, opts = {}) {
  try {
    const { stdout } = await exec(cmd, args, {
      timeout: PROBE_TIMEOUT_MS,
      shell: WIN_SHELL,
      windowsHide: true,
      ...opts,
    });
    return { ok: true, output: stripAnsi(stdout).trim() };
  } catch (err) {
    return { ok: false, error: err.code ?? err.message ?? String(err) };
  }
}

async function tryExecAny(cmds, args) {
  for (const cmd of cmds) {
    const r = await tryExec(cmd, args);
    if (r.ok) return { ...r, cmd };
  }
  return { ok: false, error: `none of [${cmds.join(", ")}] succeeded` };
}

async function pathExists(p) {
  try { await fs.access(p); return true; } catch { return false; }
}

function timeoutSignal(ms) {
  if (typeof AbortSignal !== "undefined" && AbortSignal.timeout) {
    return AbortSignal.timeout(ms);
  }
  const ac = new AbortController();
  setTimeout(() => ac.abort(), ms);
  return ac.signal;
}

async function probeDarumaHttp(baseUrl) {
  try {
    const res = await fetch(`${baseUrl}/v1/healthz`, {
      signal: timeoutSignal(HTTP_TIMEOUT_MS),
    });
    if (!res.ok) return { ok: false, error: `HTTP ${res.status}` };
    const text = await res.text();
    let parsed = null;
    try { parsed = JSON.parse(text); } catch { /* leave as text */ }
    return {
      ok: true,
      baseUrl,
      status: parsed?.status ?? text.trim(),
      version: parsed?.version ?? null,
    };
  } catch (err) {
    return { ok: false, baseUrl, error: err.message ?? String(err) };
  }
}

const MCP_PATH_SUFFIX = /\/(v1\/)?mcp\/?$/;

function trimUrl(url) {
  return String(url).trim().replace(/\/$/, "");
}

/** Server origin behind a Cursor `url` entry: `https://h/v1/mcp?x` → `https://h`. */
export function serverOriginFromMcpUrl(mcpUrl) {
  try {
    const u = new URL(mcpUrl);
    u.search = "";
    u.hash = "";
    u.pathname = u.pathname.replace(MCP_PATH_SUFFIX, "");
    return trimUrl(u.toString());
  } catch {
    return null;
  }
}

function headerValue(headers, name) {
  if (!headers || typeof headers !== "object") return null;
  const key = Object.keys(headers).find((k) => k.toLowerCase() === name.toLowerCase());
  return key ? headers[key] : null;
}

/**
 * Where `doctor` must probe: the server and credentials of the Cursor
 * mcp.json entry Cursor will actually use (project wins over global), so the
 * address `install` wrote is the one verified. Without such an entry (or a
 * stdio entry with no URL in `env`) fall back to env/credentials/localhost.
 */
export async function resolveDoctorTarget({ entry = null, scope = null, remote } = {}) {
  const isHttp = entry?.type === "http" || Boolean(entry?.url);
  if (entry && isHttp && entry.url) {
    const origin = serverOriginFromMcpUrl(entry.url);
    if (origin) {
      const headers = {};
      const auth = headerValue(entry.headers, "Authorization");
      const workspace = headerValue(entry.headers, "X-Daruma-Workspace-Id");
      if (auth) headers.Authorization = auth;
      if (workspace) headers["X-Daruma-Workspace-Id"] = workspace;
      return {
        source: `mcp.json (${scope})`,
        transport: "http",
        baseUrl: origin,
        mcpUrl: String(entry.url).trim(),
        headers,
      };
    }
  }
  const envUrl = entry?.env?.DARUMA_API_URL ?? entry?.env?.DARUMA_BASE_URL;
  if (entry && !isHttp && envUrl?.trim()) {
    const baseUrl = trimUrl(envUrl);
    const headers = {};
    if (entry.env.DARUMA_TOKEN) headers.Authorization = `Bearer ${entry.env.DARUMA_TOKEN}`;
    if (entry.env.DARUMA_WORKSPACE_ID) headers["X-Daruma-Workspace-Id"] = entry.env.DARUMA_WORKSPACE_ID;
    return {
      source: `mcp.json (${scope})`,
      transport: "stdio",
      baseUrl,
      mcpUrl: `${baseUrl}/v1/mcp`,
      headers,
    };
  }
  const baseUrl = await resolveHttpProbeUrl({ remote });
  return {
    source: "env/credentials",
    transport: entry ? (isHttp ? "http" : "stdio") : null,
    baseUrl,
    mcpUrl: null,
    headers: null,
  };
}

/**
 * Check auth the way Cursor will: an MCP `initialize` to the configured URL
 * with the configured headers. 401/403 without our own bearer on an http
 * entry is the one-click OAuth case — Cursor signs in itself, so it is not
 * a failure; with a bearer it means the token is rejected.
 */
export async function probeMcpAuth(target) {
  if (!target?.mcpUrl) return { ok: true, status: "unchecked" };
  const hasToken = Boolean(target.headers?.Authorization);
  try {
    const res = await fetch(target.mcpUrl, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json, text/event-stream",
        ...target.headers,
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2025-06-18",
          capabilities: {},
          clientInfo: { name: "daruma-cursor-doctor", version: "0" },
        },
      }),
      signal: timeoutSignal(HTTP_TIMEOUT_MS),
    });
    await res.text().catch(() => "");
    if (res.ok) return { ok: true, status: hasToken ? "token accepted" : "no token required" };
    if (res.status === 401 || res.status === 403) {
      if (hasToken) {
        const rejected = { ok: false, status: "token rejected", error: `HTTP ${res.status}` };
        if (!target.headers["X-Daruma-Workspace-Id"]) {
          rejected.hint = "no X-Daruma-Workspace-Id header — a workspace-scoped token (cloud PAT) needs it";
        }
        return rejected;
      }
      if (target.transport === "http") return { ok: true, status: "oauth (Cursor signs in)" };
      return { ok: false, status: "token missing", error: `HTTP ${res.status}` };
    }
    return { ok: false, status: "error", error: `HTTP ${res.status}` };
  } catch (err) {
    return { ok: false, status: "error", error: err.message ?? String(err) };
  }
}

export async function detectOmc({ projectDir } = {}) {
  const dir = projectDir ? resolve(projectDir) : process.cwd();
  const omcDir = join(dir, ".omc");
  const dirExists = await fs.stat(omcDir).then((s) => s.isDirectory()).catch(() => false);
  if (!dirExists) {
    return {
      name: "omc",
      present: false,
      plansDir: false,
      ultragoalDir: false,
      agentsMd: false,
      guardInstalled: false,
    };
  }
  const [plans, ultragoal, agents] = await Promise.all([
    pathExists(join(omcDir, "plans")),
    pathExists(join(omcDir, "ultragoal")),
    fs.readFile(join(omcDir, "AGENTS.md"), "utf8").catch(() => null),
  ]);
  return {
    name: "omc",
    present: true,
    plansDir: plans,
    ultragoalDir: ultragoal,
    agentsMd: agents !== null,
    guardInstalled: typeof agents === "string"
      && agents.includes("<!-- daruma-claude:begin -->"),
  };
}

async function detectProjectRules({ projectDir } = {}) {
  const dir = projectDir ? resolve(projectDir) : process.cwd();
  const rulesDir = join(dir, ".cursor", "rules");
  const installed = await Promise.all(
    RULE_FILES.map(async (name) => ({
      name,
      path: join(rulesDir, name),
      present: await pathExists(join(rulesDir, name)),
    })),
  );
  return {
    dir: rulesDir,
    rules: installed,
    allPresent: installed.every((r) => r.present),
  };
}

async function detectProjectCommands({ projectDir } = {}) {
  const dir = projectDir ? resolve(projectDir) : process.cwd();
  const commandsDir = join(dir, ".cursor", "commands");
  const installed = await Promise.all(
    COMMAND_FILES.map(async (name) => ({
      name,
      path: join(commandsDir, name),
      present: await pathExists(join(commandsDir, name)),
    })),
  );
  return {
    dir: commandsDir,
    commands: installed,
    allPresent: installed.every((c) => c.present),
  };
}

export async function detectCursor() {
  const [cli, cursorDir] = await Promise.all([
    tryExec("cursor", ["--version"]),
    pathExists(join(homedir(), ".cursor")),
  ]);
  return {
    name: "cursor",
    installed: cli.ok || cursorDir,
    cli: cli.ok ? cli.output : null,
    markers: { userDotCursor: cursorDir },
    installHint: [
      "Install Cursor: https://cursor.com/download",
      "Then sign in and ensure the `cursor` CLI is on your PATH.",
    ].join("\n"),
  };
}

async function detectMcpRegistration({ projectDir } = {}) {
  const globalPath = globalMcpPath();
  const projectPath = projectMcpPath(projectDir);
  const [global, project] = await Promise.all([
    readMcp(globalPath).catch(() => ({ mcpServers: {} })),
    readMcp(projectPath).catch(() => ({ mcpServers: {} })),
  ]);
  return {
    global: {
      path: globalPath,
      present: Boolean(global.mcpServers?.daruma),
      entry: global.mcpServers?.daruma ?? null,
    },
    project: {
      path: projectPath,
      present: Boolean(project.mcpServers?.daruma),
      entry: project.mcpServers?.daruma ?? null,
    },
  };
}

export async function detectDaruma({ projectDir, remote } = {}) {
  const creds = await loadCredentials();
  const profile = creds ? resolveActiveProfile(creds) : null;

  const [mcpCli, registration, projectRules, projectCommands] = await Promise.all([
    tryExecAny(["daruma", "daruma-mcp"], ["--version"]),
    detectMcpRegistration({ projectDir }),
    detectProjectRules({ projectDir }),
    detectProjectCommands({ projectDir }),
  ]);

  const registered = registration.global.present || registration.project.present;
  const activeEntry =
    registration.project.entry ??
    registration.global.entry ??
    null;
  const activeScope = registration.project.entry ? "project" : registration.global.entry ? "global" : null;
  const target = await resolveDoctorTarget({ entry: activeEntry, scope: activeScope, remote });
  const probeUrl = target.baseUrl;
  const [http, auth] = await Promise.all([
    probeDarumaHttp(probeUrl),
    probeMcpAuth(target),
  ]);
  const isHttpEntry = activeEntry?.type === "http" || Boolean(activeEntry?.url);
  const configuredCommand = activeEntry?.command ?? null;
  const resolvedCommand = isHttpEntry
    ? { command: null, resolved: true, source: "http" }
    : await resolveMcpCommand({ command: configuredCommand ?? "daruma" });
  const commandReady = isHttpEntry || resolvedCommand.resolved || mcpCli.ok;
  const mcpReady = registered && http.ok && commandReady && auth.ok;

  return {
    name: "daruma",
    installed: mcpCli.ok || registered || http.ok,
    mcpReady,
    cli: mcpCli.ok ? `${mcpCli.cmd}: ${mcpCli.output}` : null,
    mcpCommand: resolvedCommand,
    http: { ...http, source: target.source },
    auth: { ...auth, url: target.mcpUrl },
    credentials: {
      path: credentialsLocationHint(),
      present: Boolean(profile?.token),
      mode: profile?.mode ?? null,
      profile: profile?.name ?? null,
    },
    cursorMcp: registration,
    projectRules,
    projectCommands,
    installHint: [
      "Register HTTP MCP with Cursor (use your own server origin):",
      "  npx daruma-cursor install --transport http --global --api-url <your-daruma-server>",
      "Or open an Add-to-Cursor link from your Daruma Connect page.",
      "For local self-host, also keep daruma-server running:",
      "  ./target/release/daruma-server  # data: ~/.agents/daruma/data",
    ].join("\n"),
    mcpHint: [
      "daruma is not yet wired into Cursor.",
      `HTTP probe: GET ${probeUrl}/v1/healthz (from ${target.source})`,
      profile?.token
        ? `credentials: ${credentialsLocationHint()} (${profile.mode ?? "?"}/${profile.name ?? "?"})`
        : `credentials: none at ${credentialsLocationHint()} — run your Daruma installer to save credentials.json, then re-run install`,
      commandReady
        ? isHttpEntry
          ? `mcp transport: http (${activeEntry.url ?? "url configured"})`
          : `mcp command: ${resolvedCommand.command}${resolvedCommand.source === "discovered" ? " (auto-discovered)" : ""}`
        : [
            `mcp command missing: ${configuredCommand ?? "daruma"} not executable`,
            "  cargo build --release -p daruma-cli",
            "  daruma-cursor install --transport stdio --global --command \"$PWD/target/release/daruma\"",
          ].join("\n         "),
      http.ok
        ? `HTTP server: ${http.status}${http.version ? ` (v${http.version})` : ""}`
        : `HTTP server unreachable: ${http.error}`,
      auth.ok
        ? `auth: ${auth.status}`
        : `auth: ${auth.status} (${auth.error}) at ${target.mcpUrl}${auth.hint ? `; ${auth.hint}` : ""} — fix the entry in ${activeScope ? registration[activeScope].path : "mcp.json"} or re-run \`daruma-cursor install --force\``,
      registered
        ? `mcp.json: registered (global=${registration.global.present}, project=${registration.project.present})`
        : "mcp.json: daruma entry missing — run `daruma-cursor install --global`",
    ].join("\n"),
    updateHint: "cd <daruma repo> && git pull && cargo build --release -p daruma-server -p daruma-cli",
  };
}

export async function detectAll({ projectDir } = {}) {
  const [cursor, daruma, omc] = await Promise.all([
    detectCursor(),
    detectDaruma({ projectDir }),
    detectOmc({ projectDir }),
  ]);
  return {
    cursor,
    daruma,
    omc,
    ready: cursor.installed && daruma.mcpReady,
  };
}

export function formatReport(report) {
  const lines = [];
  for (const tool of [report.cursor, report.daruma]) {
    const status = tool.installed ? "OK" : "MISSING";
    lines.push(`[${status}] ${tool.name}`);
    if (tool.cli) lines.push(`       cli: ${tool.cli}`);
    if (tool.mcpCommand) {
      lines.push(
        `       mcp command: ${tool.mcpCommand.command}${tool.mcpCommand.resolved ? "" : " (MISSING)"}`,
      );
    }
    if (tool.http) {
      lines.push(
        `       http: ${tool.http.ok
          ? `${tool.http.baseUrl} → ${tool.http.status}${tool.http.version ? ` (v${tool.http.version})` : ""}`
          : `${tool.http.baseUrl} → ${tool.http.error}`}${tool.http.source ? ` [${tool.http.source}]` : ""}`,
      );
    }
    if (tool.auth && tool.auth.status !== "unchecked") {
      lines.push(
        `       auth: ${tool.auth.status}${tool.auth.ok ? "" : ` (${tool.auth.error})`}`,
      );
    }
    if (tool.credentials) {
      const c = tool.credentials;
      lines.push(
        `       credentials: ${c.present ? `${c.mode ?? "?"} (${c.profile ?? "?"})` : "absent"} — ${c.path}`,
      );
    }
    if (tool.cursorMcp) {
      const g = tool.cursorMcp.global;
      const p = tool.cursorMcp.project;
      lines.push(`       mcp.json (global):  ${g.present ? "registered" : "absent"}  — ${g.path}`);
      lines.push(`       mcp.json (project): ${p.present ? "registered" : "absent"}  — ${p.path}`);
    }
    if (tool.projectRules) {
      const r = tool.projectRules;
      lines.push(`       rules (project):    ${r.allPresent ? "installed" : "missing"} — ${r.dir}`);
      for (const item of r.rules) {
        lines.push(`         ${item.present ? "✓" : "✗"} ${item.name}`);
      }
    }
    if (tool.projectCommands) {
      const c = tool.projectCommands;
      lines.push(`       commands (project): ${c.allPresent ? "installed" : "missing"} — ${c.dir}`);
      for (const item of c.commands) {
        lines.push(`         ${item.present ? "✓" : "✗"} ${item.name}`);
      }
    }
    if (!tool.installed) {
      lines.push("       install:");
      for (const ln of tool.installHint.split("\n")) lines.push("         " + ln);
    } else if (tool.mcpReady === false && tool.mcpHint) {
      lines.push("       mcp:");
      for (const ln of tool.mcpHint.split("\n")) lines.push("         " + ln);
    }
  }
  if (report.omc) {
    const o = report.omc;
    const status = o.present ? "present" : "absent";
    lines.push(`[INFO] oh-my-claudecode (.omc/): ${status}`);
    if (o.present) {
      lines.push(`       plans dir:    ${o.plansDir ? "EXISTS (review for stale plans)" : "absent"}`);
      lines.push(`       ultragoal:    ${o.ultragoalDir ? "EXISTS (review for stale plans)" : "absent"}`);
      lines.push(`       AGENTS.md:    ${o.agentsMd ? "exists" : "absent"}`);
      lines.push(`       daruma guard: ${o.guardInstalled ? "installed" : "MISSING — run omc-guard"}`);
    }
  }
  lines.push("");
  lines.push(report.ready ? "READY" : "NOT READY — see hints above");
  return lines.join("\n");
}
