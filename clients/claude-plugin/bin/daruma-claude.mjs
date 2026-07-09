#!/usr/bin/env node
// `daruma-claude` — single-shell entry point for the
// tupical/daruma × oh-my-claudecode workflow.
//
// Subcommands:
//   daruma-claude doctor                      Check whether daruma + omc are ready.
//   daruma-claude setup                       Print install instructions for missing deps.
//   daruma-claude start "<task description>"  Drive daruma via MCP and run each
//                                                eligible task as `omc team`.
//   daruma-claude team-from-plan <plan_id>    Execute an existing plan by
//                                                dependency fanout waves.
//   daruma-claude update                      Check + update daruma-claude / omc;
//                                                print manual hint for daruma.
//   daruma-claude platform                    Print execution mode (omc-team | task-fallback)
//   daruma-claude --version                   Print version.
//
// `start` opens its own JSON-RPC connection to `daruma-mcp`, creates/picks a
// project, creates a root task (optionally decomposing into a plan), then uses
// `omc team` as the executor for each eligible task. No nested Claude Code
// session is opened at the daruma-claude level — `omc team` workers are the
// only Claude Code panes.

import {
  cliReadinessSummary,
  detectAll,
  detectAllCached,
  detectOMC,
  detectDaruma,
  formatReport,
  parseSemver,
} from "../lib/detect.mjs";
import { runDarumaStart, runDarumaTeamFromPlan } from "../lib/orchestrator.mjs";
import { installPolicy, removePolicy } from "../lib/policy.mjs";
import { installOmcGuard, removeOmcGuard } from "../lib/omc-guard.mjs";
import {
  fetchLatestVersion,
  installLatest,
  isNewer,
} from "../lib/update.mjs";
import { createCliUi } from "../lib/cli-ui.mjs";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

// Single source of truth for the policy + OMC-guard text is the unified
// `daruma` binary (`daruma install --claude`). When it is on PATH we
// delegate to it; otherwise we fall back to the byte-identical bundled Node
// writers below (so `init` still works before the binary is installed).
function delegatePolicyToBinary(dir) {
  const r = spawnSync("daruma", ["install", "--claude", "--project", dir], {
    encoding: "utf8",
  });
  if (r.error) return null; // ENOENT — binary not on PATH
  return { ok: r.status === 0, stdout: r.stdout ?? "", stderr: r.stderr ?? "" };
}
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf8"));

const HELP = `daruma-claude v${pkg.version} — tupical/daruma × oh-my-claudecode glue

Usage:
  daruma-claude start "<task description>" [--workers N] [--max-retries M]
                                              [--agent T] [--plan] [--project ID] [--yes]
                                    Drive daruma (project → task [→ plan])
                                    via MCP and run each eligible task as
                                    \`omc team\`. No nested Claude Code session.
  daruma-claude team-from-plan <plan_id> [--workers N] [--max-retries M]
                                              [--agent T] [--yes]
                                    Execute an existing daruma plan wave-by-wave
                                    via \`daruma_plan_fanout\` + \`omc team\`.
  daruma-claude init [--dir DIR] [--no-policy] [--no-omc-guard]
                                    Drop project-scoped artifacts: a managed
                                    policy block in <DIR>/CLAUDE.md so this
                                    project defaults to daruma for tasks
                                    and plans, plus the OMC guard in
                                    <DIR>/.omc/AGENTS.md when oh-my-claudecode
                                    is present. Idempotent.
  daruma-claude uninit [--dir DIR]
                                    Remove the managed policy block and OMC
                                    guard. Surrounding content is preserved.
  daruma-claude doctor [--json|--quiet] [--no-cache]
                                    Check dependency status (exit 0 = READY)
  daruma-claude setup            Print install instructions for missing deps
  daruma-claude update           Check + update daruma-claude and omc to latest;
                                    print manual upgrade hint for daruma.
  daruma-claude platform         Print execution mode (omc-team | task-fallback)
  daruma-claude --version | -v   Print version
  daruma-claude --help    | -h   This message

daruma-claude start flags:
  --workers N         Number of parallel agents in each \`omc team\` invocation.
                      Integer 1-20. Default 3.
  --max-retries M     Retries after the first attempt for each task (so total
                      attempts = M + 1). Non-negative integer. Default 2.
  --agent T           Agent type for \`omc team\` workers (claude | codex | gemini).
                      Default claude.
  --plan              Ask daruma to AI-decompose the root task into a plan
                      of subtasks, then execute each subtask via \`omc team\`.
  --project ID        Use this daruma project id instead of auto-resolving
                      from workspace info / cwd basename.
  --yes               Skip y/n confirmation prompts (implied when stdin is
                      not a TTY).

daruma-claude team-from-plan flags:
  --workers N         Number of concurrent plan tasks per wave. Integer 1-20.
                      Default 3.
  --max-retries M     Retries after the first attempt for each task. Default 2.
  --agent T           Agent type for \`omc team\` workers (claude | codex | gemini).
                      Default claude.
  --yes               Skip y/n confirmation prompts (implied when stdin is
                      not a TTY).

daruma-claude doctor flags:
  --json      Emit machine-readable readiness summary on stdout. Mutually
              exclusive with --quiet. Exits 0/1 the same way as plain doctor.
  --quiet     Print nothing on stdout; only the exit code carries info.
              Useful as a cheap preflight gate from shell scripts.
  --no-cache  Force live detection. Default behaviour caches a successful
              READY result for 30s in ~/.cache/daruma-claude/doctor.json so
              that a subsequent \`daruma-claude start\` preflight returns
              near-instantly.
`;

function parseDoctorFlags(rest) {
  const flags = { json: false, quiet: false, noCache: false };
  for (const arg of rest) {
    if (arg === "--json") flags.json = true;
    else if (arg === "--quiet") flags.quiet = true;
    else if (arg === "--no-cache") flags.noCache = true;
    else throw new Error(`Unknown daruma-claude doctor flag: ${arg}`);
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
    process.stderr.write(`daruma-claude doctor: ${err.message}\n`);
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
  const ui = createCliUi({ title: "Daruma Claude Setup" });
  ui.header();
  const report = await detectAll();
  if (report.ready) {
    ui.success("Both dependencies present. Nothing to install.");
    process.stdout.write(formatReport(report) + "\n");
    return;
  }
  ui.warn("Install the missing dependencies below, then re-run `daruma-claude doctor`.");
  for (const tool of [report.omc, report.daruma]) {
    if (tool.installed) continue;
    ui.section(tool.name);
    process.stdout.write(`${tool.installHint}\n`);
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

function actionKind(action) {
  if (action === "installed" || action === "updated" || action === "appended" || action === "removed-block" || action === "removed-file") {
    return "ok";
  }
  if (action === "unchanged" || action === "skipped-no-omc" || action === "missing") {
    return "warn";
  }
  return "dot";
}

function parseInitFlags(rest) {
  const opts = { projectDir: undefined, noPolicy: false, noOmcGuard: false };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    switch (a) {
      case "--dir":
      case "--project": {
        const v = rest[++i];
        if (!v || v.startsWith("--")) throw new Error(`${a} requires a directory`);
        if (a === "--project") {
          // `--project` here is the target DIRECTORY, which clashes with
          // `start --project ID` (a daruma project id). Keep it as a
          // back-compat alias for `--dir` but steer callers away.
          process.stderr.write(
            "daruma-claude init: --project is deprecated and means the target DIRECTORY, " +
              "not a daruma project. Use --dir <directory> (default: current directory).\n",
          );
        }
        opts.projectDir = v;
        break;
      }
      case "--no-policy":
        opts.noPolicy = true; break;
      case "--no-omc-guard":
        opts.noOmcGuard = true; break;
      default:
        throw new Error(`Unknown daruma-claude init flag: ${a}`);
    }
  }
  return opts;
}

async function cmdInit(rest = []) {
  const ui = createCliUi({ title: "Daruma Claude Initializer" });
  let opts;
  try {
    opts = parseInitFlags(rest);
  } catch (err) {
    process.stderr.write(`daruma-claude init: ${err.message}\n`);
    process.exit(2);
  }
  const dir = opts.projectDir ?? process.cwd();
  ui.header();

  // Prefer the unified `daruma` binary as the single source of policy text.
  // It writes both the CLAUDE.md policy and the .omc guard in one call; only
  // delegate when neither block is opted out so per-block flags keep working.
  if (!opts.noPolicy && !opts.noOmcGuard) {
    const delegated = delegatePolicyToBinary(dir);
    if (delegated && delegated.ok) {
      ui.item("policy + OMC guard written by daruma (single source)", {
        kind: "ok",
      });
      ui.success("Project is now defaulted to daruma.");
      ui.detail("  Open Claude Code in this directory to pick it up.");
      return;
    }
    // binary absent or failed → fall through to the bundled Node writers.
  }

  if (!opts.noPolicy) {
    const result = await ui.task(
      "Installing Claude project policy...",
      () => installPolicy({ projectDir: dir }),
      "Claude project policy ready",
    );
    const verb = POLICY_VERB[result.action] ?? result.action;
    ui.item(`${verb} ${result.path}`, { kind: actionKind(result.action) });
  }

  if (!opts.noOmcGuard) {
    const result = await ui.task(
      "Refreshing OMC guard...",
      () => installOmcGuard({ projectDir: dir }),
      "OMC guard checked",
    );
    const verb = OMC_VERB[result.action] ?? result.action;
    ui.item(`OMC guard: ${verb} ${result.path}`, { kind: actionKind(result.action) });
    if (result.action === "skipped-no-omc") {
      ui.detail("  no oh-my-claudecode artifacts in this project; nothing to override");
    }
  }

  ui.success("Project is now defaulted to daruma.");
  ui.detail("  Open Claude Code in this directory to pick it up.");
}

async function cmdUninit(rest = []) {
  const ui = createCliUi({ title: "Daruma Claude Uninitializer" });
  let opts;
  try {
    opts = parseInitFlags(rest);
  } catch (err) {
    process.stderr.write(`daruma-claude uninit: ${err.message}\n`);
    process.exit(2);
  }
  const dir = opts.projectDir ?? process.cwd();
  ui.header();

  const policy = await ui.task(
    "Removing Claude project policy...",
    () => removePolicy({ projectDir: dir }),
    "Claude project policy checked",
  );
  ui.item(`${POLICY_VERB[policy.action] ?? policy.action} ${policy.path}`, {
    kind: actionKind(policy.action),
  });

  const guard = await ui.task(
    "Removing managed OMC guard...",
    () => removeOmcGuard({ projectDir: dir }),
    "OMC guard checked",
  );
  ui.item(`OMC guard: ${OMC_VERB[guard.action] ?? guard.action} ${guard.path}`, {
    kind: actionKind(guard.action),
  });
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
  const ui = createCliUi();
  if (!current) {
    ui.warn(`${label}: could not parse local version`);
    if (manualHint) ui.detail(`  install/upgrade: ${manualHint}`);
    return false;
  }
  if (!latest) {
    ui.warn(`${label}: could not reach registry`);
    return false;
  }
  if (current === latest) {
    ui.success(`${label}: ${current} (latest)`);
    return true;
  }
  if (!isNewer(latest, current)) {
    ui.warn(`${label}: local ${current} > registry ${latest}, skipping`);
    return true;
  }
  ui.step(`${label}: ${current} -> ${latest}`);
  if (upgradeFn) {
    try {
      await upgradeFn();
      ui.success(`${label}: updated`);
      return true;
    } catch (err) {
      ui.error(`${label}: ${err.message}`);
      if (manualHint) ui.detail(`  try manually: ${manualHint}`);
      return false;
    }
  }
  if (manualHint) ui.detail(`  run: ${manualHint}`);
  return true;
}

async function tryFetchNpm(pkgName) {
  try { return await fetchLatestVersion(pkgName); }
  catch { return null; }
}

async function cmdUpdate() {
  const ui = createCliUi({ title: "Daruma Claude Updater" });
  ui.header();
  // 1. daruma-claude (self) — npm.
  const selfLatest = await ui.task(
    "Checking daruma-claude...",
    () => tryFetchNpm("daruma-claude"),
    "Checked daruma-claude",
  );
  await processComponent({
    label: "daruma-claude",
    current: pkg.version,
    latest: selfLatest,
    upgradeFn: () => installLatest("daruma-claude"),
    manualHint: "npm i -g daruma-claude@latest",
  });

  // 2. omc (oh-my-claude-sisyphus) — npm.
  const omc = await ui.task(
    "Checking oh-my-claudecode (omc)...",
    () => detectOMC(),
    "Checked oh-my-claudecode (omc)",
  );
  if (!omc.installed) {
    ui.warn("oh-my-claudecode not installed");
  } else {
    const omcLatest = await ui.task(
      "Checking oh-my-claudecode registry version...",
      () => tryFetchNpm("oh-my-claude-sisyphus"),
      "Checked oh-my-claudecode registry version",
    );
    const omcCurrent = parseSemver(omc.cli) ?? parseSemver(omc.npmVersion);
    await processComponent({
      label: "oh-my-claudecode (omc)",
      current: omcCurrent,
      latest: omcLatest,
      upgradeFn: () => installLatest("oh-my-claude-sisyphus"),
      manualHint: "npm i -g oh-my-claude-sisyphus@latest",
    });
  }

  // 3. daruma — built from source, no registry to query. Just print the
  // canonical manual upgrade hint pulled from detect.mjs.
  const daruma = await ui.task(
    "Checking daruma...",
    () => detectDaruma(),
    "Checked daruma",
  );
  if (!daruma.installed) {
    ui.warn("daruma not installed");
    ui.detail(`  install: ${daruma.installHint.split("\n")[0]}`);
  } else {
    const current = parseSemver(daruma.cli) ?? parseSemver(daruma.http?.version);
    ui.warn(`daruma${current ? `: ${current}` : ""} — built from source`);
    ui.detail(`  run: ${daruma.updateHint}`);
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
      throw new Error(`Unknown daruma-claude start flag: ${a}`);
    } else {
      taskParts.push(a);
    }
  }
  opts.task = taskParts.join(" ").trim();
  return opts;
}

function parseTeamFromPlanArgs(argv) {
  const opts = {
    planId: "",
    workers: undefined,
    maxRetries: undefined,
    agent: undefined,
    yes: false,
  };
  const parts = [];
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
    } else if (a === "--yes" || a === "-y") {
      opts.yes = true;
    } else if (a === "--") {
      parts.push(...argv.slice(i + 1));
      break;
    } else if (a.startsWith("--")) {
      throw new Error(`Unknown daruma-claude team-from-plan flag: ${a}`);
    } else {
      parts.push(a);
    }
  }
  opts.planId = parts.join(" ").trim();
  return opts;
}

async function cmdStart(rest) {
  let opts;
  try {
    opts = parseStartArgs(rest);
  } catch (err) {
    process.stderr.write(`daruma-claude start: ${err.message}\n`);
    process.exit(2);
  }
  if (!opts.task) {
    process.stderr.write("daruma-claude start requires a task description.\nExample: daruma-claude start \"refactor auth module to use OAuth2\"\n");
    process.exit(2);
  }
  const report = await detectAll();
  if (!report.ready) {
    process.stderr.write("Cannot start — missing dependencies:\n\n");
    process.stderr.write(formatReport(report) + "\n\n");
    process.stderr.write("Run `daruma-claude setup` for install instructions.\n");
    process.exit(1);
  }
  try {
    const result = await runDarumaStart({
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
    process.stderr.write(`daruma-claude start failed: ${err.message}\n`);
    if (process.env.DARUMA_DEBUG || process.env.OMO_DEBUG) {
      process.stderr.write(`${err.stack}\n`);
    }
    process.exit(1);
  }
}

async function cmdTeamFromPlan(rest) {
  let opts;
  try {
    opts = parseTeamFromPlanArgs(rest);
  } catch (err) {
    process.stderr.write(`daruma-claude team-from-plan: ${err.message}\n`);
    process.exit(2);
  }
  if (!opts.planId) {
    process.stderr.write("daruma-claude team-from-plan requires a plan id.\nExample: daruma-claude team-from-plan pln_123 --yes\n");
    process.exit(2);
  }
  const report = await detectAll();
  if (!report.ready) {
    process.stderr.write("Cannot start — missing dependencies:\n\n");
    process.stderr.write(formatReport(report) + "\n\n");
    process.stderr.write("Run `daruma-claude setup` for install instructions.\n");
    process.exit(1);
  }
  try {
    const result = await runDarumaTeamFromPlan({
      planId: opts.planId,
      cwd: process.cwd(),
      workers: opts.workers,
      maxRetries: opts.maxRetries,
      agentType: opts.agent,
      autoYes: opts.yes,
      stdin: process.stdin,
      stdout: process.stdout,
    });
    if (result?.cancelled) process.exit(130);
    if (result?.ok === false) process.exit(1);
  } catch (err) {
    process.stderr.write(`daruma-claude team-from-plan failed: ${err.message}\n`);
    if (process.env.DARUMA_DEBUG || process.env.OMO_DEBUG) {
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
    case "team-from-plan":
      return cmdTeamFromPlan(rest);
    default:
      process.stderr.write(`Unknown command: ${cmd}\n\n${HELP}`);
      process.exit(2);
  }
}

main(process.argv).catch((err) => {
  process.stderr.write(`daruma-claude: ${err.stack ?? err.message ?? err}\n`);
  process.exit(1);
});
