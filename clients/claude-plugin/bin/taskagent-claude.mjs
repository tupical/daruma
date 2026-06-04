#!/usr/bin/env node
// `taskagent-claude` — single-shell entry point for the
// tupical/taskagent × oh-my-claudecode workflow.
//
// Subcommands:
//   taskagent-claude doctor                      Check whether taskagent + omc are ready.
//   taskagent-claude setup                       Print install instructions for missing deps.
//   taskagent-claude start "<task description>"  Drive taskagent via MCP and run each
//                                                eligible task as `omc team`.
//   taskagent-claude update                      Check + update taskagent-claude / omc;
//                                                print manual hint for taskagent.
//   taskagent-claude platform                    Print execution mode (omc-team | task-fallback)
//   taskagent-claude --version                   Print version.
//
// `start` opens its own JSON-RPC connection to `taskagent-mcp`, creates/picks a
// project, creates a root task (optionally decomposing into a plan), then uses
// `omc team` as the executor for each eligible task. No nested Claude Code
// session is opened at the taskagent-claude level — `omc team` workers are the
// only Claude Code panes.

import {
  cliReadinessSummary,
  detectAll,
  detectAllCached,
  detectOMC,
  detectTaskagent,
  formatReport,
  parseSemver,
} from "../lib/detect.mjs";
import { runTaskagentStart } from "../lib/orchestrator.mjs";
import { installPolicy, removePolicy } from "../lib/policy.mjs";
import { installOmcGuard, removeOmcGuard } from "../lib/omc-guard.mjs";
import {
  fetchLatestVersion,
  installLatest,
  isNewer,
} from "../lib/update.mjs";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf8"));

const HELP = `taskagent-claude v${pkg.version} — tupical/taskagent × oh-my-claudecode glue

Usage:
  taskagent-claude start "<task description>" [--workers N] [--max-retries M]
                                              [--agent T] [--plan] [--project ID] [--yes]
                                    Drive taskagent (project → task [→ plan])
                                    via MCP and run each eligible task as
                                    \`omc team\`. No nested Claude Code session.
  taskagent-claude init [--project DIR] [--no-policy] [--no-omc-guard]
                                    Drop project-scoped artifacts: a managed
                                    policy block in <DIR>/CLAUDE.md so this
                                    project defaults to taskagent for tasks
                                    and plans, plus the OMC guard in
                                    <DIR>/.omc/AGENTS.md when oh-my-claudecode
                                    is present. Idempotent.
  taskagent-claude uninit [--project DIR]
                                    Remove the managed policy block and OMC
                                    guard. Surrounding content is preserved.
  taskagent-claude doctor [--json|--quiet] [--no-cache]
                                    Check dependency status (exit 0 = READY)
  taskagent-claude setup            Print install instructions for missing deps
  taskagent-claude update           Check + update taskagent-claude and omc to latest;
                                    print manual upgrade hint for taskagent.
  taskagent-claude platform         Print execution mode (omc-team | task-fallback)
  taskagent-claude --version | -v   Print version
  taskagent-claude --help    | -h   This message

taskagent-claude start flags:
  --workers N         Number of parallel agents in each \`omc team\` invocation.
                      Integer 1-20. Default 3.
  --max-retries M     Retries after the first attempt for each task (so total
                      attempts = M + 1). Non-negative integer. Default 2.
  --agent T           Agent type for \`omc team\` workers (claude | codex | gemini).
                      Default claude.
  --plan              Ask taskagent to AI-decompose the root task into a plan
                      of subtasks, then execute each subtask via \`omc team\`.
  --project ID        Use this taskagent project id instead of auto-resolving
                      from workspace info / cwd basename.
  --yes               Skip y/n confirmation prompts (implied when stdin is
                      not a TTY).

taskagent-claude doctor flags:
  --json      Emit machine-readable readiness summary on stdout. Mutually
              exclusive with --quiet. Exits 0/1 the same way as plain doctor.
  --quiet     Print nothing on stdout; only the exit code carries info.
              Useful as a cheap preflight gate from shell scripts.
  --no-cache  Force live detection. Default behaviour caches a successful
              READY result for 30s in ~/.cache/taskagent-claude/doctor.json so
              that a subsequent \`taskagent-claude start\` preflight returns
              near-instantly.
`;

function parseDoctorFlags(rest) {
  const flags = { json: false, quiet: false, noCache: false };
  for (const arg of rest) {
    if (arg === "--json") flags.json = true;
    else if (arg === "--quiet") flags.quiet = true;
    else if (arg === "--no-cache") flags.noCache = true;
    else throw new Error(`Unknown taskagent-claude doctor flag: ${arg}`);
  }
  if (flags.json && flags.quiet) {
    throw new Error("--json and --quiet are mutually exclusive");
  }
  return flags;
}

async function cmdDoctor(rest = []) {
  let flags;
  try {
    flags = parseDoctorFlags(rest);
  } catch (err) {
    process.stderr.write(`taskagent-claude doctor: ${err.message}\n`);
    process.exit(2);
  }
  const result = await detectAllCached({
    bypass: flags.noCache,
    cliVersion: pkg.version,
  });
  // Cache hit reuses pre-rendered strings/summary; live run formats on the fly.
  const ready = result.source === "cache" ? result.payload.ready : result.report.ready;
  const formatted = result.source === "cache"
    ? result.payload.formatted
    : formatReport(result.report);
  const summary = result.source === "cache"
    ? result.payload.summary
    : cliReadinessSummary(result.report);

  if (flags.json) {
    process.stdout.write(JSON.stringify(summary) + "\n");
  } else if (!flags.quiet) {
    process.stdout.write(formatted + "\n");
  }
  process.exit(ready ? 0 : 1);
}

async function cmdSetup() {
  const report = await detectAll();
  if (report.ready) {
    process.stdout.write("Both dependencies present. Nothing to install.\n");
    process.stdout.write(formatReport(report) + "\n");
    return;
  }
  process.stdout.write("Install the missing dependencies below, then re-run `taskagent-claude doctor`.\n\n");
  for (const tool of [report.omc, report.taskagent]) {
    if (tool.installed) continue;
    process.stdout.write(`# ${tool.name}\n${tool.installHint}\n\n`);
  }
}

const POLICY_VERB = {
  installed: "Created CLAUDE.md with policy block",
  updated: "Refreshed policy block in",
  appended: "Appended policy block to",
  unchanged: "Policy block already current in",
  "removed-block": "Removed policy block from",
  "removed-file": "Removed",
  missing: "No policy block found at",
};

const OMC_VERB = {
  installed: "Created",
  updated: "Refreshed",
  appended: "Appended block to",
  unchanged: "Already current",
  "skipped-no-omc": "No .omc/ detected, skipped",
  "removed-block": "Removed managed block from",
  "removed-file": "Removed",
  missing: "Nothing to remove at",
};

function parseInitFlags(rest) {
  const opts = { projectDir: undefined, noPolicy: false, noOmcGuard: false };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    switch (a) {
      case "--project": {
        const v = rest[++i];
        if (!v || v.startsWith("--")) throw new Error("--project requires a directory");
        opts.projectDir = v;
        break;
      }
      case "--no-policy":
        opts.noPolicy = true; break;
      case "--no-omc-guard":
        opts.noOmcGuard = true; break;
      default:
        throw new Error(`Unknown taskagent-claude init flag: ${a}`);
    }
  }
  return opts;
}

async function cmdInit(rest = []) {
  let opts;
  try {
    opts = parseInitFlags(rest);
  } catch (err) {
    process.stderr.write(`taskagent-claude init: ${err.message}\n`);
    process.exit(2);
  }
  const dir = opts.projectDir ?? process.cwd();

  if (!opts.noPolicy) {
    const result = await installPolicy({ projectDir: dir });
    const verb = POLICY_VERB[result.action] ?? result.action;
    process.stdout.write(`${verb} ${result.path}\n`);
  }

  if (!opts.noOmcGuard) {
    const result = await installOmcGuard({ projectDir: dir });
    const verb = OMC_VERB[result.action] ?? result.action;
    process.stdout.write(`OMC guard: ${verb} ${result.path}\n`);
    if (result.action === "skipped-no-omc") {
      process.stdout.write(
        "  (no oh-my-claudecode artifacts in this project — nothing to override)\n",
      );
    }
  }

  process.stdout.write(
    "\nProject is now defaulted to taskagent. Open Claude Code in this directory to pick it up.\n",
  );
}

async function cmdUninit(rest = []) {
  let opts;
  try {
    opts = parseInitFlags(rest);
  } catch (err) {
    process.stderr.write(`taskagent-claude uninit: ${err.message}\n`);
    process.exit(2);
  }
  const dir = opts.projectDir ?? process.cwd();

  const policy = await removePolicy({ projectDir: dir });
  process.stdout.write(`${POLICY_VERB[policy.action] ?? policy.action} ${policy.path}\n`);

  const guard = await removeOmcGuard({ projectDir: dir });
  process.stdout.write(`OMC guard: ${OMC_VERB[guard.action] ?? guard.action} ${guard.path}\n`);
}

async function cmdPlatform() {
  // Decides which execution mode SKILL Step 4a should use.
  // Windows-native tmux + Git Bash makes `omc team` worker spawn unreliable
  // (worker output bleeds into leader pane, sessions die on tmux exit), so
  // on win32 we route to in-session parallel Task agents instead.
  const mode = process.platform === "win32" ? "task-fallback" : "omc-team";
  process.stdout.write(mode + "\n");
}

// Print one component's update status. If `upgradeFn` is provided and an
// update is available, runs it; otherwise prints `manualHint` for the user.
async function processComponent({ label, current, latest, upgradeFn, manualHint }) {
  if (!current) {
    process.stdout.write(`[?] ${label}: could not parse local version\n`);
    if (manualHint) process.stdout.write(`    install/upgrade: ${manualHint}\n`);
    return false;
  }
  if (!latest) {
    process.stdout.write(`[?] ${label}: could not reach registry\n`);
    return false;
  }
  if (current === latest) {
    process.stdout.write(`[ok] ${label}: ${current} (latest)\n`);
    return true;
  }
  if (!isNewer(latest, current)) {
    process.stdout.write(`[?] ${label}: local ${current} > registry ${latest}, skipping\n`);
    return true;
  }
  process.stdout.write(`[update] ${label}: ${current} -> ${latest}\n`);
  if (upgradeFn) {
    try {
      await upgradeFn();
      process.stdout.write(`         done.\n`);
      return true;
    } catch (err) {
      process.stdout.write(`         FAILED: ${err.message}\n`);
      if (manualHint) process.stdout.write(`         try manually: ${manualHint}\n`);
      return false;
    }
  }
  if (manualHint) process.stdout.write(`         run:  ${manualHint}\n`);
  return true;
}

async function tryFetchNpm(pkgName) {
  try { return await fetchLatestVersion(pkgName); }
  catch { return null; }
}

async function cmdUpdate() {
  // 1. taskagent-claude (self) — npm.
  process.stdout.write(`Checking taskagent-claude...\n`);
  const selfLatest = await tryFetchNpm("taskagent-claude");
  await processComponent({
    label: "taskagent-claude",
    current: pkg.version,
    latest: selfLatest,
    upgradeFn: () => installLatest("taskagent-claude"),
    manualHint: "npm i -g taskagent-claude@latest",
  });

  // 2. omc (oh-my-claude-sisyphus) — npm.
  process.stdout.write(`\nChecking oh-my-claudecode (omc)...\n`);
  const omc = await detectOMC();
  if (!omc.installed) {
    process.stdout.write(`[skip] oh-my-claudecode not installed\n`);
  } else {
    const omcLatest = await tryFetchNpm("oh-my-claude-sisyphus");
    const omcCurrent = parseSemver(omc.cli) ?? parseSemver(omc.npmVersion);
    await processComponent({
      label: "oh-my-claudecode (omc)",
      current: omcCurrent,
      latest: omcLatest,
      upgradeFn: () => installLatest("oh-my-claude-sisyphus"),
      manualHint: "npm i -g oh-my-claude-sisyphus@latest",
    });
  }

  // 3. taskagent — built from source, no registry to query. Just print the
  // canonical manual upgrade hint pulled from detect.mjs.
  process.stdout.write(`\nChecking taskagent...\n`);
  const taskagent = await detectTaskagent();
  if (!taskagent.installed) {
    process.stdout.write(`[skip] taskagent not installed\n`);
    process.stdout.write(`       install: ${taskagent.installHint.split("\n")[0]}\n`);
  } else {
    const current = parseSemver(taskagent.cli) ?? parseSemver(taskagent.http?.version);
    process.stdout.write(`[manual] taskagent${current ? `: ${current}` : ""} — built from source\n`);
    process.stdout.write(`         run:  ${taskagent.updateHint}\n`);
  }
}

function parseStartArgs(argv) {
  const opts = {
    task: "",
    workers: undefined,
    maxRetries: undefined,
    agent: undefined,
    plan: false,
    projectId: undefined,
    yes: false,
  };
  const taskParts = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--workers") {
      const v = argv[++i];
      const n = parseInt(v, 10);
      if (!Number.isInteger(n) || n < 1 || n > 20) throw new Error(`--workers must be an integer 1-20, got '${v}'`);
      opts.workers = n;
    } else if (a === "--max-retries") {
      const v = argv[++i];
      const n = parseInt(v, 10);
      if (!Number.isInteger(n) || n < 0) throw new Error(`--max-retries must be a non-negative integer, got '${v}'`);
      opts.maxRetries = n;
    } else if (a === "--agent") {
      const v = argv[++i];
      if (!/^(claude|codex|gemini)$/.test(v)) throw new Error(`--agent must be claude|codex|gemini, got '${v}'`);
      opts.agent = v;
    } else if (a === "--plan") {
      opts.plan = true;
    } else if (a === "--project") {
      const v = argv[++i];
      if (!v || v.startsWith("--")) throw new Error(`--project requires an id argument`);
      opts.projectId = v;
    } else if (a === "--yes" || a === "-y") {
      opts.yes = true;
    } else if (a === "--") {
      taskParts.push(...argv.slice(i + 1));
      break;
    } else if (a.startsWith("--")) {
      throw new Error(`Unknown taskagent-claude start flag: ${a}`);
    } else {
      taskParts.push(a);
    }
  }
  opts.task = taskParts.join(" ").trim();
  return opts;
}

async function cmdStart(rest) {
  let opts;
  try {
    opts = parseStartArgs(rest);
  } catch (err) {
    process.stderr.write(`taskagent-claude start: ${err.message}\n`);
    process.exit(2);
  }
  if (!opts.task) {
    process.stderr.write("taskagent-claude start requires a task description.\nExample: taskagent-claude start \"refactor auth module to use OAuth2\"\n");
    process.exit(2);
  }
  const report = await detectAll();
  if (!report.ready) {
    process.stderr.write("Cannot start — missing dependencies:\n\n");
    process.stderr.write(formatReport(report) + "\n\n");
    process.stderr.write("Run `taskagent-claude setup` for install instructions.\n");
    process.exit(1);
  }
  try {
    const result = await runTaskagentStart({
      task: opts.task,
      cwd: process.cwd(),
      workers: opts.workers,
      maxRetries: opts.maxRetries,
      agentType: opts.agent,
      planMode: opts.plan,
      projectId: opts.projectId,
      autoYes: opts.yes,
      stdin: process.stdin,
      stdout: process.stdout,
    });
    if (result?.cancelled) process.exit(130);
    if (result?.ok === false) process.exit(1);
  } catch (err) {
    process.stderr.write(`taskagent-claude start failed: ${err.message}\n`);
    if (process.env.TASKAGENT_DEBUG || process.env.OMO_DEBUG) {
      process.stderr.write(`${err.stack}\n`);
    }
    process.exit(1);
  }
}

async function main(argv) {
  const [, , cmd, ...rest] = argv;
  switch (cmd) {
    case undefined:
    case "--help":
    case "-h":
    case "help":
      process.stdout.write(HELP);
      return;
    case "--version":
    case "-v":
      process.stdout.write(pkg.version + "\n");
      return;
    case "doctor":
      return cmdDoctor(rest);
    case "setup":
      return cmdSetup();
    case "update":
      return cmdUpdate();
    case "platform":
      return cmdPlatform();
    case "init":
      return cmdInit(rest);
    case "uninit":
      return cmdUninit(rest);
    case "start":
      return cmdStart(rest);
    default:
      process.stderr.write(`Unknown command: ${cmd}\n\n${HELP}`);
      process.exit(2);
  }
}

main(process.argv).catch((err) => {
  process.stderr.write(`taskagent-claude: ${err.stack ?? err.message ?? err}\n`);
  process.exit(1);
});
