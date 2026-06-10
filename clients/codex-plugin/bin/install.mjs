#!/usr/bin/env node
// `npx taskagent-codex-install` — one-shot MCP setup for multiple IDE targets.
//
// Detects the running IDE (Codex, Cursor, Windsurf, Claude Code) and writes
// the appropriate MCP config, then drops AGENTS.md policy for Codex.
//
// Usage:
//   npx taskagent-codex install [--ide auto|codex|cursor|windsurf|claude] [--project DIR]
//                               [--base-url URL] [--token TOKEN] [--force]
//   npx taskagent-codex install --help

import { promises as fs } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";

import { installPolicy } from "../lib/policy.mjs";
import {
  resolveToken,
  resolveMcpEnvFromCredentials,
  credentialsLocationHint,
} from "../lib/agent-credentials.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf8"));

const DEFAULT_URL = process.env.TASKAGENT_API_URL ?? "http://localhost:8080";
const MCP_RESOURCE_PATH = "/v1/mcp";

const HELP = `taskagent-codex v${pkg.version} — Multi-IDE MCP installer for TaskAgent

Usage:
  npx taskagent-codex install [options]

Options:
  --ide <target>     IDE to configure: auto (default), codex, cursor, windsurf,
                     claude. "auto" detects from environment variables.
  --project DIR      Write project-scoped MCP config instead of global.
  --base-url URL     TaskAgent server origin (default: ${DEFAULT_URL}).
  --token TOKEN      Bearer token. Resolved from TASKAGENT_TOKEN env or
                     credentials file if omitted.
  --force            Overwrite existing MCP config entries.
  --help | -h        This message.

Token discovery order:
  1. --token flag / TASKAGENT_TOKEN env var
  2. ~/.agents/taskagent/credentials.json  (active profile)
  3. ~/.config/taskagent/credentials.json  (legacy XDG, auto-migrated)

Examples:
  npx taskagent-codex install                    # auto-detect IDE
  npx taskagent-codex install --ide cursor       # Cursor global mcp.json
  npx taskagent-codex install --ide windsurf     # Windsurf mcp_config.json
  npx taskagent-codex install --ide codex        # AGENTS.md policy only
  npx taskagent-codex install --ide claude       # Claude Code settings.json
`;

// ---------------------------------------------------------------------------
// IDE detection
// ---------------------------------------------------------------------------

function detectIde() {
  // Codex sets CODEX_SANDBOX or similar; Cursor sets CURSOR_TRACE_ID
  if (process.env.CODEX_SANDBOX || process.env.CODEX_SESSION_ID) return "codex";
  if (process.env.CURSOR_TRACE_ID || process.env.CURSOR_SESSION_ID) return "cursor";
  if (process.env.WINDSURF_SESSION_ID || process.env.CODEIUM_API_KEY) return "windsurf";
  if (process.env.ANTHROPIC_API_KEY && process.env.CLAUDE_CODE_ENTRYPOINT) return "claude";
  // Fallback: check for IDE-specific config directories
  return "codex";
}

// ---------------------------------------------------------------------------
// MCP config helpers
// ---------------------------------------------------------------------------

function mcpServerEntry({ baseUrl, token }) {
  const url = `${(baseUrl ?? DEFAULT_URL).replace(/\/$/, "")}${MCP_RESOURCE_PATH}`;
  const entry = { type: "http", url };
  if (token) {
    entry.headers = { Authorization: `Bearer ${token}` };
  }
  return entry;
}

async function writeJsonAtomic(path, data) {
  await fs.mkdir(dirname(path), { recursive: true });
  const tmp = `${path}.tmp.${process.pid}`;
  await fs.writeFile(tmp, JSON.stringify(data, null, 2) + "\n", "utf8");
  try {
    await fs.rename(tmp, path);
  } catch (err) {
    if (err?.code === "EXDEV") {
      await fs.copyFile(tmp, path);
      await fs.unlink(tmp);
    } else {
      throw err;
    }
  }
}

async function readJsonOrEmpty(path) {
  try {
    return JSON.parse(await fs.readFile(path, "utf8"));
  } catch (err) {
    if (err?.code === "ENOENT") return null;
    throw err;
  }
}

// ---------------------------------------------------------------------------
// Per-IDE installers
// ---------------------------------------------------------------------------

async function installCursor({ projectDir, baseUrl, token, force }) {
  const configPath = projectDir
    ? join(resolve(projectDir), ".cursor", "mcp.json")
    : join(homedir(), ".cursor", "mcp.json");

  const existing = (await readJsonOrEmpty(configPath)) ?? { mcpServers: {} };
  if (!existing.mcpServers) existing.mcpServers = {};

  if (existing.mcpServers.taskagent && !force) {
    console.log(`  already present: ${configPath} (use --force to overwrite)`);
    return;
  }
  existing.mcpServers.taskagent = mcpServerEntry({ baseUrl, token });
  await writeJsonAtomic(configPath, existing);
  console.log(`  wrote Cursor MCP entry → ${configPath}`);
}

async function installWindsurf({ projectDir, baseUrl, token, force }) {
  const configPath = projectDir
    ? join(resolve(projectDir), ".windsurf", "mcp_config.json")
    : join(homedir(), ".codeium", "windsurf", "mcp_config.json");

  const existing = (await readJsonOrEmpty(configPath)) ?? { mcpServers: {} };
  if (!existing.mcpServers) existing.mcpServers = {};

  if (existing.mcpServers.taskagent && !force) {
    console.log(`  already present: ${configPath} (use --force to overwrite)`);
    return;
  }
  existing.mcpServers.taskagent = mcpServerEntry({ baseUrl, token });
  await writeJsonAtomic(configPath, existing);
  console.log(`  wrote Windsurf MCP entry → ${configPath}`);
}

async function installClaude({ projectDir, baseUrl, token, force }) {
  // Claude Code reads MCP servers from ~/.claude/settings.json
  const configPath = projectDir
    ? join(resolve(projectDir), ".claude", "settings.json")
    : join(homedir(), ".claude", "settings.json");

  const existing = (await readJsonOrEmpty(configPath)) ?? {};
  if (!existing.mcpServers) existing.mcpServers = {};

  if (existing.mcpServers.taskagent && !force) {
    console.log(`  already present: ${configPath} (use --force to overwrite)`);
    return;
  }
  existing.mcpServers.taskagent = mcpServerEntry({ baseUrl, token });
  await writeJsonAtomic(configPath, existing);
  console.log(`  wrote Claude Code MCP entry → ${configPath}`);
}

async function installCodex({ projectDir }) {
  const result = await installPolicy({ projectDir });
  const verb = {
    installed: "created AGENTS.md with policy",
    updated: "updated policy block in",
    appended: "appended policy block to",
    unchanged: "policy already current in",
  }[result.action] ?? result.action;
  console.log(`  ${verb} ${result.path}`);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

async function main(argv) {
  const args = argv.slice(2);
  const rest = args.filter((a) => !a.startsWith("-"));
  const cmd = rest[0];

  if (!cmd || cmd === "install" || cmd === "--help" || cmd === "-h" || args.includes("--help") || args.includes("-h")) {
    if (args.includes("--help") || args.includes("-h") || (!cmd && args.length === 0)) {
      process.stdout.write(HELP);
      return;
    }
  }

  // Parse flags
  let ide = "auto";
  let projectDir;
  let baseUrl;
  let token;
  let force = false;

  for (let i = (cmd === "install" ? 1 : 0); i < args.length; i++) {
    const a = args[i];
    switch (a) {
      case "--ide":       ide       = args[++i]; break;
      case "--project":   projectDir = args[++i]; break;
      case "--base-url":  baseUrl   = args[++i]; break;
      case "--token":     token     = args[++i]; break;
      case "--force":
      case "-f":          force = true; break;
      case "--help":
      case "-h":
        process.stdout.write(HELP);
        return;
      default:
        if (!a.startsWith("-")) break; // positional already consumed
        throw new Error(`Unknown flag: ${a}`);
    }
  }

  if (ide === "auto") ide = detectIde();

  // Resolve token using unified discovery
  const resolvedToken = token ?? await resolveToken({ token });
  if (!resolvedToken) {
    const hint = credentialsLocationHint();
    console.warn(
      `  no token found — set TASKAGENT_TOKEN or run \`taskagent-cursor pair\` (creds: ${hint})`
    );
  }

  console.log(`taskagent-codex install — IDE: ${ide}`);

  const opts = { projectDir, baseUrl, token: resolvedToken, force };

  switch (ide) {
    case "cursor":
      await installCursor(opts);
      break;
    case "windsurf":
      await installWindsurf(opts);
      break;
    case "claude":
      await installClaude(opts);
      break;
    case "codex":
      await installCodex({ projectDir });
      break;
    case "all": {
      await installCodex({ projectDir });
      await installCursor(opts);
      await installWindsurf(opts);
      await installClaude(opts);
      break;
    }
    default:
      throw new Error(`Unknown IDE target: ${ide}. Use: auto, codex, cursor, windsurf, claude, all`);
  }

  console.log("Done. Run `taskagent-codex doctor` to verify.");
}

main(process.argv).catch((err) => {
  process.stderr.write(`taskagent-codex install: ${err.message ?? err}\n`);
  if (process.env.TASKAGENT_DEBUG) process.stderr.write(`${err.stack}\n`);
  process.exit(1);
});
