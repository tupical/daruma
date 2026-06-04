// Cursor-side detection for taskagent-claude: probes the taskagent HTTP server,
// the taskagent-mcp binary on PATH, and the presence of Cursor's mcp.json.
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

async function probeTaskagentHttp(baseUrl) {
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
      && agents.includes("<!-- taskagent-claude:begin -->"),
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
      present: Boolean(global.mcpServers?.taskagent),
      entry: global.mcpServers?.taskagent ?? null,
    },
    project: {
      path: projectPath,
      present: Boolean(project.mcpServers?.taskagent),
      entry: project.mcpServers?.taskagent ?? null,
    },
  };
}

export async function detectTaskagent({ projectDir, remote } = {}) {
  const probeUrl = await resolveHttpProbeUrl({ remote });
  const creds = await loadCredentials();
  const profile = creds ? resolveActiveProfile(creds) : null;

  const [mcpCli, http, registration, projectRules, projectCommands] = await Promise.all([
    tryExecAny(["taskagent-mcp", "taskagent"], ["--version"]),
    probeTaskagentHttp(probeUrl),
    detectMcpRegistration({ projectDir }),
    detectProjectRules({ projectDir }),
    detectProjectCommands({ projectDir }),
  ]);

  const registered = registration.global.present || registration.project.present;
  const mcpReady = registered && http.ok;

  return {
    name: "taskagent",
    installed: mcpCli.ok || registered || http.ok,
    mcpReady,
    cli: mcpCli.ok ? `${mcpCli.cmd}: ${mcpCli.output}` : null,
    http,
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
      "Build taskagent from source (workspace at github.com/tupical/taskagent):",
      "  cargo build --release -p taskagent-server -p taskagent-mcp-bin",
      "Start the HTTP server (keep this running):",
      "  ./target/release/taskagent-server  # data: ~/.agents/taskagent/data",
      "Register the MCP stdio shim with Cursor:",
      "  taskagent-cursor install --global",
      "Or click the deeplink from the marketplace card (Add to Cursor).",
    ].join("\n"),
    mcpHint: [
      "taskagent is not yet wired into Cursor.",
      `HTTP probe: GET ${probeUrl}/v1/healthz`,
      profile?.token
        ? `credentials: ${credentialsLocationHint()} (${profile.mode ?? "?"}/${profile.name ?? "?"})`
        : `credentials: none at ${credentialsLocationHint()} — set TASKAGENT_API_URL + TASKAGENT_TOKEN or save a local profile, then re-run install`,
      http.ok
        ? `HTTP server: ${http.status}${http.version ? ` (v${http.version})` : ""}`
        : `HTTP server unreachable: ${http.error}`,
      registered
        ? `mcp.json: registered (global=${registration.global.present}, project=${registration.project.present})`
        : "mcp.json: taskagent entry missing — run `taskagent-cursor install --global`",
    ].join("\n"),
    updateHint: "cd <taskagent repo> && git pull && cargo build --release -p taskagent-server -p taskagent-mcp-bin",
  };
}

export async function detectAll({ projectDir } = {}) {
  const [cursor, taskagent, omc] = await Promise.all([
    detectCursor(),
    detectTaskagent({ projectDir }),
    detectOmc({ projectDir }),
  ]);
  return {
    cursor,
    taskagent,
    omc,
    ready: cursor.installed && taskagent.mcpReady,
  };
}

export function formatReport(report) {
  const lines = [];
  for (const tool of [report.cursor, report.taskagent]) {
    const status = tool.installed ? "OK" : "MISSING";
    lines.push(`[${status}] ${tool.name}`);
    if (tool.cli) lines.push(`       cli: ${tool.cli}`);
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
      lines.push(`       taskagent guard: ${o.guardInstalled ? "installed" : "MISSING — run omc-guard"}`);
    }
  }
  lines.push("");
  lines.push(report.ready ? "READY" : "NOT READY — see hints above");
  return lines.join("\n");
}
