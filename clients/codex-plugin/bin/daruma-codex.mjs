#!/usr/bin/env node
// `daruma-codex` — project init for the Codex daruma plugin.
//
// Subcommands:
//   daruma-codex init [--project DIR]     Drop managed policy in AGENTS.md
//   daruma-codex uninit [--project DIR] Remove managed policy block
//   daruma-codex --version | -v
//   daruma-codex --help    | -h

import { installPolicy, removePolicy } from "../lib/policy.mjs";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf8"));

const HELP = `daruma-codex v${pkg.version} — Codex project init for Daruma

Usage:
  daruma-codex init [--project DIR]
        Drop a managed policy block in <DIR>/AGENTS.md so this project
        defaults to daruma for tasks and plans. Idempotent.
  daruma-codex uninit [--project DIR]
        Remove the managed policy block. Surrounding content is preserved.
  daruma-codex --version | -v   Print version
  daruma-codex --help    | -h   This message
`;

const POLICY_VERB = {
  installed: "Created AGENTS.md with policy block",
  updated: "Refreshed policy block in",
  appended: "Appended policy block to",
  unchanged: "Policy block already current in",
  "removed-block": "Removed policy block from",
  "removed-file": "Removed",
  missing: "No policy block found at",
};

function parseProjectFlag(rest) {
  let projectDir = undefined;
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (a === "--project") {
      const v = rest[++i];
      if (!v || v.startsWith("--")) throw new Error("--project requires a directory");
      projectDir = v;
    } else {
      throw new Error(`Unknown flag: ${a}`);
    }
  }
  return projectDir;
}

async function cmdInit(rest = []) {
  let projectDir;
  try {
    projectDir = parseProjectFlag(rest);
  } catch (err) {
    process.stderr.write(`daruma-codex init: ${err.message}\n`);
    process.exit(2);
  }
  const result = await installPolicy({ projectDir });
  const verb = POLICY_VERB[result.action] ?? result.action;
  process.stdout.write(`${verb} ${result.path}\n`);
}

async function cmdUninit(rest = []) {
  let projectDir;
  try {
    projectDir = parseProjectFlag(rest);
  } catch (err) {
    process.stderr.write(`daruma-codex uninit: ${err.message}\n`);
    process.exit(2);
  }
  const result = await removePolicy({ projectDir });
  const verb = POLICY_VERB[result.action] ?? result.action;
  process.stdout.write(`${verb} ${result.path}\n`);
}

async function main() {
  const [cmd, ...rest] = process.argv.slice(2);
  if (!cmd || cmd === "--help" || cmd === "-h") {
    process.stdout.write(HELP);
    return;
  }
  if (cmd === "--version" || cmd === "-v") {
    process.stdout.write(`${pkg.version}\n`);
    return;
  }
  if (cmd === "init") {
    await cmdInit(rest);
    return;
  }
  if (cmd === "uninit") {
    await cmdUninit(rest);
    return;
  }
  process.stderr.write(`Unknown command: ${cmd}\n\n${HELP}`);
  process.exit(2);
}

main().catch((err) => {
  process.stderr.write(`daruma-codex: ${err.message}\n`);
  process.exit(1);
});
