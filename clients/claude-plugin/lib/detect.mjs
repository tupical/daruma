// Detection helpers for tupical/daruma and oh-my-claudecode.
// Used by both the `daruma-claude` CLI and the /daruma-claude:doctor skill.
//
// readiness gate = omc CLI present + daruma MCP server registered in Claude
// + daruma HTTP server healthy + the stdio launcher `daruma-claude start`
// spawns is runnable. The MCP probe parses `claude mcp list` output; the HTTP
// probe hits DARUMA_BASE_URL/v1/healthz (default http://localhost:8080).

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

import {
  credentialsLocationHint,
  loadCredentials,
  resolveActiveProfile,
  resolveHttpProbeUrl,
  resolveMcpEnvFromCredentials,
} from "./agent-credentials.mjs";

const exec = promisify(execFile);

const WIN_SHELL = process.platform === "win32";
const PROBE_TIMEOUT_MS = 4_000;
const HTTP_TIMEOUT_MS = 3_000;


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

// The command `daruma-claude start` / `team-from-plan` spawn for their private
// stdio MCP connection: the unified `daruma` binary serves it as `daruma mcp`
// (the standalone `daruma-mcp` binary is retired). Doctor probes exactly this
// command, so READY cannot outlive what start actually launches.
// DARUMA_MCP_BIN overrides the binary path, as in the Cursor plugin.
export function darumaMcpLaunch(env = process.env) {
  return { command: env.DARUMA_MCP_BIN?.trim() || "daruma", args: ["mcp"] };
}

// Runs `<launcher> --version` for the command darumaMcpLaunch resolves.
export async function probeDarumaLauncher(env = process.env) {
  const launch = darumaMcpLaunch(env);
  const r = await tryExec(launch.command, ["--version"]);
  return r.ok
    ? { ok: true, command: launch.command, args: launch.args, version: r.output }
    : { ok: false, command: launch.command, args: launch.args, error: r.error };
}

// Claude Code's registered `daruma` server serves the session; start opens its
// own connection through the launcher. Both, plus the HTTP backend behind
// them, must work — a connected Claude entry alone does not make start work.
export function darumaMcpReady({ launcher, claudeMcp, http }) {
  return Boolean(launcher?.ok && claudeMcp?.connected && http?.ok);
}

async function pathExists(p) {
  try { await fs.access(p); return true; } catch { return false; }
}

// AbortSignal.timeout polyfill — Node 20 ships it but Node 18 LTS doesn't,
// and we advertise engines ">=20" so this is mostly defensive.
function timeoutSignal(ms) {
  if (typeof AbortSignal !== "undefined" && AbortSignal.timeout) {
    return AbortSignal.timeout(ms);
  }
  const ac = new AbortController();
  setTimeout(() => ac.abort(), ms);
  return ac.signal;
}

export async function detectOMC() {
  const [cliVersion, npmCheck, omcDir, claudePluginsDir] = await Promise.all([
    tryExec("omc", ["--version"]),
    tryExec("npm", ["ls", "-g", "--depth=0", "oh-my-claude-sisyphus", "--json"]),
    pathExists(join(homedir(), ".omc")),
    pathExists(join(homedir(), ".claude", "plugins")),
  ]);
  let npmVersion = null;
  if (npmCheck.ok) {
    try {
      const parsed = JSON.parse(npmCheck.output);
      npmVersion = parsed?.dependencies?.["oh-my-claude-sisyphus"]?.version ?? null;
    } catch { /* ignore */ }
  }
  return {
    name: "oh-my-claudecode",
    installed: cliVersion.ok || npmVersion !== null,
    cli: cliVersion.ok ? cliVersion.output : null,
    npmVersion,
    markers: { userDotOmc: omcDir, claudePluginsDir },
    installHint: [
      "Recommended (npm CLI):  npm i -g oh-my-claude-sisyphus@latest",
      "Or via Claude Code marketplace, in a /claude session:",
      "  /plugin marketplace add https://github.com/Yeachan-Heo/oh-my-claudecode",
      "  /plugin install oh-my-claudecode",
      "Then enable native teams in ~/.claude/settings.json:",
      '  { "env": { "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS": "1" } }',
    ].join("\n"),
    updateHint: "npm i -g oh-my-claude-sisyphus@latest",
  };
}

// Parses `claude mcp list` looking for a `daruma:` line and its status.
// Output format (claude 2.1.x):
//   Checking MCP server health…
//
//   daruma: /path/to/daruma-mcp - ✓ Connected
//   other-server: ... - ✗ Failed to connect
//
// Returns { present, connected, command }.
export function parseClaudeMcpList(text, serverName = "daruma") {
  const empty = { present: false, connected: false, command: null };
  if (!text) return empty;
  const lines = text.split(/\r?\n/);
  // Server lines look like:
  //   <name>: <command…> - <status-marker> <status-text>
  // where the status-marker is one of ✓ / ✔ / ✗ / ✘ / ! (`claude mcp list`
  // switched from ✓/✗ to the heavy ✔/✘ glyphs; accept both).
  // The command itself can contain dashes (e.g. `daruma-mcp`), so we
  // anchor the separator on the trailing status marker rather than the
  // first ` - ` we encounter.
  const escaped = serverName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const re = new RegExp(
    `^${escaped}\\s*:\\s*(.+?)\\s+-\\s+([✓✔✗✘!]\\s*[^\\n]+)$`,
    "i",
  );
  for (const raw of lines) {
    const line = raw.trim();
    const m = re.exec(line);
    if (!m) continue;
    const command = m[1].trim();
    const status = m[2].trim();
    return {
      present: true,
      connected: /connected/i.test(status) && !/fail/i.test(status),
      command,
    };
  }
  return empty;
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
    return { ok: true, status: parsed?.status ?? text.trim(), version: parsed?.version ?? null };
  } catch (err) {
    return { ok: false, error: err.message ?? String(err) };
  }
}

export async function detectDaruma() {
  const probeUrl = await resolveHttpProbeUrl();
  const creds = await loadCredentials();
  const profile = creds ? resolveActiveProfile(creds) : null;

  // `claude mcp list` runs a full health check against every registered MCP
  // server. On a setup with several servers it routinely takes 5-15s — the
  // standard PROBE_TIMEOUT_MS would clip it. Give it a wider budget.
  const CLAUDE_MCP_LIST_TIMEOUT_MS = 20_000;
  const [launcher, mcpList, httpProbe] = await Promise.all([
    probeDarumaLauncher(),
    tryExec("claude", ["mcp", "list"], { timeout: CLAUDE_MCP_LIST_TIMEOUT_MS }),
    probeDarumaHttp(probeUrl),
  ]);

  const mcpEntry = mcpList.ok
    ? parseClaudeMcpList(mcpList.output, "daruma")
    : { present: false, connected: false, command: null };

  // mcpReady = MCP server registered AND HTTP backend healthy AND the start
  // launcher runnable. The MCP shim is just a transport into the HTTP
  // server — without the server, all tools/call requests fail at runtime.
  const mcpReady = darumaMcpReady({ launcher, claudeMcp: mcpEntry, http: httpProbe });
  const installed = launcher.ok || mcpEntry.present || httpProbe.ok;
  const launchLine = `${launcher.command} ${launcher.args.join(" ")}`;

  return {
    name: "daruma",
    installed,
    mcpReady,
    cli: launcher.ok ? `${launcher.command}: ${launcher.version}` : null,
    launcher,
    http: httpProbe.ok
      ? { ok: true, baseUrl: probeUrl, status: httpProbe.status, version: httpProbe.version }
      : { ok: false, baseUrl: probeUrl, error: httpProbe.error },
    credentials: {
      path: credentialsLocationHint(),
      present: Boolean(profile?.token),
      mode: profile?.mode ?? null,
      profile: profile?.name ?? null,
    },
    claudeMcp: mcpEntry,
    installHint: [
      "Self-host (build from source — server + MCP shim):",
      "  git clone https://github.com/tupical/daruma && cd daruma",
      "  cargo build --release -p daruma-server -p daruma-cli",
      "  ./target/release/daruma-server  # data: ~/.agents/daruma/data",
      "Register the MCP shim with Claude Code:",
      "  claude mcp add daruma -- daruma mcp",
      "Set DARUMA_API_URL and DARUMA_TOKEN if you do not use credentials.json.",
      "Override agent dir: DARUMA_AGENT_DIR (default ~/.agents/daruma/).",
    ].join("\n"),
    mcpHint: [
      "daruma server or MCP shim is not ready.",
      `Server probe: GET ${probeUrl}/v1/healthz`,
      profile?.token
        ? `credentials: ${credentialsLocationHint()} (${profile.mode ?? "?"}/${profile.name ?? "?"})`
        : `credentials: none at ${credentialsLocationHint()} — set DARUMA_API_URL + DARUMA_TOKEN or save a local profile`,
      mcpEntry.present
        ? `MCP shim registered (${mcpEntry.command}): ${mcpEntry.connected ? "connected" : "disconnected"}`
        : "MCP shim not registered. Add it:  claude mcp add daruma -- daruma mcp",
      launcher.ok
        ? `start launcher: ${launchLine} (${launcher.version})`
        : `start launcher not runnable: ${launchLine} (${launcher.error}) — put the unified daruma binary on PATH or set DARUMA_MCP_BIN to it`,
      httpProbe.ok
        ? `HTTP server: ${httpProbe.status}${httpProbe.version ? ` (v${httpProbe.version})` : ""}`
        : `HTTP server unreachable: ${httpProbe.error}`,
    ].join("\n"),
    updateHint: "cd <daruma repo> && git pull && cargo build --release -p daruma-server -p daruma-cli",
  };
}

export async function detectAll() {
  const [omc, daruma] = await Promise.all([detectOMC(), detectDaruma()]);
  return {
    omc,
    daruma,
    ready: omc.installed && daruma.mcpReady,
  };
}

export function parseSemver(text) {
  if (!text) return null;
  const m = /(\d+\.\d+\.\d+(?:[-.\w]+)?)/.exec(text);
  return m ? m[1] : null;
}

export function cliReadinessSummary(report) {
  const t = report.daruma;
  const mcp = t.claudeMcp ?? {};
  return {
    ready: report.ready,
    omc: {
      installed: report.omc.installed,
      cli: report.omc.cli,
      npm: report.omc.npmVersion,
    },
    daruma: {
      installed: t.installed,
      mcpReady: t.mcpReady,
      cli: t.cli,
      http: t.http,
      claudeMcp: {
        present: mcp.present ?? false,
        connected: mcp.connected ?? false,
        command: mcp.command ?? null,
      },
      launcher: t.launcher
        ? { ok: t.launcher.ok, command: t.launcher.command, args: t.launcher.args }
        : null,
    },
    hints: {
      omcInstall: report.omc.installed ? null : firstLine(report.omc.installHint),
      darumaInstall: t.installed ? null : firstLine(t.installHint),
      darumaMcp: t.mcpReady ? null : firstLine(t.mcpHint),
    },
  };
}

function firstLine(text) {
  if (!text) return null;
  const i = text.indexOf("\n");
  return i === -1 ? text : text.slice(0, i);
}

export function formatReport(report) {
  const lines = [];
  for (const tool of [report.omc, report.daruma]) {
    const status = tool.installed ? "OK" : "MISSING";
    lines.push(`[${status}] ${tool.name}`);
    if (tool.cli) lines.push(`       cli: ${tool.cli}`);
    if (tool.npmVersion) lines.push(`       npm:  ${tool.npmVersion}`);
    if (tool.http) {
      lines.push(
        `       http: ${tool.http.ok
          ? `${tool.http.baseUrl} → ${tool.http.status}${tool.http.version ? ` (v${tool.http.version})` : ""}`
          : `${tool.http.baseUrl} → ${tool.http.error}`}`,
      );
    }
    if (tool.credentials) {
      const c = tool.credentials;
      lines.push(
        `       credentials: ${c.present ? `${c.mode ?? "?"} (${c.profile ?? "?"})` : "absent"} — ${c.path}`,
      );
    }
    if (tool.claudeMcp) {
      const mcp = tool.claudeMcp;
      const summary = mcp.present
        ? `${mcp.connected ? "connected" : "DISCONNECTED"} (${mcp.command ?? "?"})`
        : "not registered";
      lines.push(`       claude mcp: ${summary}`);
    }
    if (tool.launcher) {
      const l = tool.launcher;
      lines.push(
        `       start launcher: ${l.command} ${l.args.join(" ")} → ${l.ok ? l.version : `NOT RUNNABLE (${l.error})`}`,
      );
    }
    if (!tool.installed) {
      lines.push("       install:");
      for (const ln of tool.installHint.split("\n")) lines.push("         " + ln);
    } else if (tool.mcpReady === false && tool.mcpHint) {
      lines.push("       mcp:");
      for (const ln of tool.mcpHint.split("\n")) lines.push("         " + ln);
    }
  }
  lines.push("");
  if (report.ready) {
    lines.push("READY");
  } else if (report.omc.installed && report.daruma.installed && !report.daruma.mcpReady) {
    lines.push("NOT READY — daruma MCP/server not ready (see mcp hint above)");
  } else {
    lines.push("NOT READY — install missing dependencies above");
  }
  return lines.join("\n");
}

// --- Doctor result cache (unchanged shape, just renamed product) ------------

const CACHE_TTL_MS = 30_000;

function cachePath() {
  const xdg = process.env.XDG_CACHE_HOME;
  const base = xdg && xdg.length > 0 ? xdg : join(homedir(), ".cache");
  return join(base, "daruma-claude", "doctor.json");
}

export async function loadCachedDoctor({ cliVersion } = {}) {
  try {
    const raw = await fs.readFile(cachePath(), "utf8");
    const parsed = JSON.parse(raw);
    if (parsed?.cliVersion !== cliVersion) return null;
    if (typeof parsed.ts !== "number") return null;
    if (Date.now() - parsed.ts > CACHE_TTL_MS) return null;
    if (parsed.ready !== true) return null;
    if (typeof parsed.formatted !== "string") return null;
    if (typeof parsed.summary !== "object" || parsed.summary === null) return null;
    return parsed;
  } catch {
    return null;
  }
}

export async function saveCachedDoctor(report, { cliVersion } = {}) {
  if (!report.ready) return;
  const payload = {
    cliVersion,
    ts: Date.now(),
    ttlMs: CACHE_TTL_MS,
    ready: report.ready,
    summary: cliReadinessSummary(report),
    formatted: formatReport(report),
  };
  try {
    const path = cachePath();
    await fs.mkdir(join(path, ".."), { recursive: true });
    const tmp = join(tmpdir(), `daruma-claude-doctor.${process.pid}.${Date.now()}.json`);
    await fs.writeFile(tmp, JSON.stringify(payload));
    await fs.rename(tmp, path);
  } catch {
    /* cache is best-effort */
  }
}

/** Env vars for spawning `daruma mcp` (credentials + process env). */
export async function darumaMcpChildEnv(extra = {}) {
  const fromCreds = await resolveMcpEnvFromCredentials();
  return { ...process.env, ...fromCreds, ...extra };
}

export async function detectAllCached({ bypass = false, cliVersion }) {
  if (!bypass) {
    const cached = await loadCachedDoctor({ cliVersion });
    if (cached) return { source: "cache", payload: cached };
  }
  const report = await detectAll();
  await saveCachedDoctor(report, { cliVersion });
  return { source: "live", report };
}
