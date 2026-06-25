#!/usr/bin/env node
// `npx daruma-codex-install` — thin delegate to `daruma install`.
//
// All install logic lives in the unified `daruma` binary (apps/cli).
// This wrapper detects the running IDE from env vars and maps the legacy
// --ide flag to the binary's per-target flags, then execs the binary.
//
// Usage (same surface as before):
//   npx daruma-codex install [--ide auto|codex|cursor|windsurf|claude|all]
//                               [--project DIR] [--base-url URL]
//                               [--token TOKEN] [--force] [--help]

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf8"));

const HELP = `daruma-codex v${pkg.version} — Multi-IDE MCP installer for Daruma
(thin delegate — requires the \`daruma\` binary on PATH)

Usage:
  npx daruma-codex install [options]

Options:
  --ide <target>     IDE to configure: auto (default), codex, cursor, windsurf,
                     claude, all. "auto" detects from environment variables.
  --project DIR      Write project-scoped config instead of global.
  --base-url URL     Daruma server origin (default: http://localhost:8080).
  --token TOKEN      Bearer token. Resolved from DARUMA_TOKEN env or
                     credentials file if omitted.
  --force            Overwrite existing MCP config entries.
  --help | -h        This message.

Install the \`daruma\` binary first:
  curl -fsSL https://raw.githubusercontent.com/tupical/daruma/main/install.sh | sh
  # or: cargo install daruma-cli
  # or: download from https://github.com/tupical/daruma/releases
`;

// ---------------------------------------------------------------------------
// IDE detection from env (caller's env — must stay in JS)
// ---------------------------------------------------------------------------

function detectIde() {
  if (process.env.CODEX_SANDBOX || process.env.CODEX_SESSION_ID) return "codex";
  if (process.env.CURSOR_TRACE_ID || process.env.CURSOR_SESSION_ID) return "cursor";
  if (process.env.WINDSURF_SESSION_ID || process.env.CODEIUM_API_KEY) return "windsurf";
  if (process.env.ANTHROPIC_API_KEY && process.env.CLAUDE_CODE_ENTRYPOINT) return "claude";
  return "codex";
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

function main(argv) {
  const args = argv.slice(2);

  if (args.length === 0 || args.includes("--help") || args.includes("-h")) {
    process.stdout.write(HELP);
    return;
  }

  // Strip leading positional "install" if present.
  const rest = args[0] === "install" ? args.slice(1) : args.slice(0);

  // Parse the JS-side flags we need to translate.
  let ide = "auto";
  let projectDir;
  let baseUrl;
  let token;
  let force = false;

  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    switch (a) {
      case "--ide":       ide        = rest[++i]; break;
      case "--project":   projectDir = rest[++i]; break;
      case "--base-url":  baseUrl    = rest[++i]; break;
      case "--token":     token      = rest[++i]; break;
      case "--force":
      case "-f":          force = true; break;
      case "--help":
      case "-h":
        process.stdout.write(HELP);
        return;
      default:
        if (!a.startsWith("-")) break; // positional — ignore
        process.stderr.write(`daruma-codex install: unknown flag: ${a}\n`);
        process.exit(1);
    }
  }

  if (ide === "auto") ide = detectIde();

  // Map --ide value to binary flag(s).
  const ideToFlag = {
    codex:    ["--codex"],
    cursor:   ["--cursor"],
    windsurf: ["--windsurf"],
    claude:   ["--claude"],
    all:      ["--all"],
  };
  const ideFlags = ideToFlag[ide];
  if (!ideFlags) {
    process.stderr.write(
      `daruma-codex install: unknown IDE target: ${ide}. ` +
      `Use: auto, codex, cursor, windsurf, claude, all\n`
    );
    process.exit(1);
  }

  // Build the binary invocation.
  const binaryArgs = ["install", ...ideFlags];
  if (projectDir) binaryArgs.push("--project", projectDir);
  if (force)      binaryArgs.push("--force");
  // Pass api-url and token as global flags (before subcommand).
  const globalArgs = [];
  if (baseUrl) globalArgs.push("--api-url", baseUrl);
  if (token)   globalArgs.push("--token", token);

  const finalArgs = [...globalArgs, ...binaryArgs];

  console.log(`daruma-codex install — IDE: ${ide}`);

  const result = spawnSync("daruma", finalArgs, { stdio: "inherit", shell: false });

  if (result.error) {
    if (result.error.code === "ENOENT") {
      process.stderr.write(
        `\ndaruma-codex install: 'daruma' binary not found on PATH.\n` +
        `Install it first:\n` +
        `  curl -fsSL https://raw.githubusercontent.com/tupical/daruma/main/install.sh | sh\n` +
        `  # or: cargo install daruma-cli\n` +
        `  # or: https://github.com/tupical/daruma/releases\n`
      );
    } else {
      process.stderr.write(`daruma-codex install: ${result.error.message}\n`);
    }
    process.exit(1);
  }

  process.exit(result.status ?? 0);
}

main(process.argv);
