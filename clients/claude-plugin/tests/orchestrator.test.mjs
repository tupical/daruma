import { test } from "node:test";
import assert from "node:assert/strict";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

import { _internal, runDarumaStart } from "../lib/orchestrator.mjs";

const execFileAsync = promisify(execFile);

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "daruma-orchestrator-test-"));
  try {
    return await fn(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

test("currentGitBranch returns the checked-out branch name", async () => {
  await withTempDir(async (dir) => {
    await execFileAsync("git", ["init", "-b", "feature/branch-awareness"], { cwd: dir });

    const branch = await _internal.currentGitBranch(dir);

    assert.equal(branch, "feature/branch-awareness");
  });
});

test("currentGitBranch returns null outside a git worktree", async () => {
  await withTempDir(async (dir) => {
    const branch = await _internal.currentGitBranch(dir);

    assert.equal(branch, null);
  });
});

function mockMcp({ drains, tasks = {}, tools = [] } = {}) {
  const calls = [];
  return {
    calls,
    async callTool(name, args) {
      calls.push({ name, args });
      if (name === "daruma_workspace_info") return { parsed: { mcp_agent_id: "agent" } };
      if (name === "daruma_run_start") return { parsed: { data: { run_id: "00000000-0000-4000-8000-000000000001" } } };
      if (name === "daruma_plan_drain_next") {
        const next = drains.shift();
        return { parsed: next ?? null, text: JSON.stringify(next ?? null) };
      }
      if (name === "daruma_run_notes_list") return { parsed: { notes: [{ id: "note", body: "Previous check passed", author: { kind: "agent" }, created_at: "2026-09-11" }] } };
      if (name === "daruma_get") {
        const task = { status: "todo", ...(tasks[args.id] ?? { id: args.id, title: args.id }) };
        return { parsed: task, text: JSON.stringify(task) };
      }
      if (name === "daruma_plan_get") {
        return { parsed: { id: args.id, status: "active" }, text: "{}" };
      }
      return { parsed: { ok: true }, text: "{}" };
    },
    async listTools() {
      calls.push({ name: "tools/list", args: {} });
      return tools;
    },
  };
}

test("team-from-plan executes wave 2 only after wave 1 completes", async () => {
  const mcp = mockMcp({
    drains: [{ task_id: "a" }, { task_id: "b" }, { task_id: "c" }],
    tasks: {
      a: { id: "a", title: "A" },
      b: { id: "b", title: "B" },
      c: { id: "c", title: "C" },
    },
  });
  const done = new Set();

  await _internal.runTeamFromPlanWaves({
    mcp,
    planId: "pln_1",
    waves: [{ wave: 0, tasks: ["a", "b"] }, { wave: 1, tasks: ["c"] }],
    maxRetries: 0,
    workers: 2,
    agentType: "claude",
    cwd: "/tmp",
    stderrLog: null,
    stdout: { write() {}, isTTY: false },
    write() {},
    agentId: "agent_1",
    async executeTask({ task }) {
      if (task.id === "c") assert.deepEqual([...done].sort(), ["a", "b"]);
      await new Promise((r) => setTimeout(r, 5));
      done.add(task.id);
      return { ok: true, attempts: 1, result: { teamName: task.id, counts: { total: 1, completed: 1, failed: 0 } } };
    },
  });

  assert.deepEqual([...done].sort(), ["a", "b", "c"]);
});

test("team-from-plan releases and comments blocker on failed task", async () => {
  const mcp = mockMcp({
    drains: [{ task_id: "a" }, { task_id: "b" }],
    tasks: { a: { id: "a", title: "A" }, b: { id: "b", title: "B" } },
  });

  const result = await _internal.runTeamFromPlanWaves({
    mcp,
    planId: "pln_1",
    waves: [{ wave: 0, tasks: ["a"] }, { wave: 1, tasks: ["b"] }],
    maxRetries: 0,
    workers: 1,
    agentType: "claude",
    cwd: "/tmp",
    stderrLog: null,
    stdout: { write() {}, isTTY: false },
    write() {},
    agentId: "agent_1",
    async executeTask() {
      return { ok: false, attempts: 1, result: { teamName: "bad", counts: { total: 1, completed: 0, failed: 1 } } };
    },
  });

  assert.equal(result.summaries.length, 1);
  assert.equal(result.summaries[0].ok, false);
  assert(mcp.calls.some((c) => c.name === "daruma_release" && c.args.agent_id === "agent_1" && c.args.task_id === "a"));
  assert(mcp.calls.some((c) => c.name === "daruma_comment" && c.args.task_id === "a" && c.args.kind === "blocker"));
  assert(!mcp.calls.some((c) => c.name === "daruma_get" && c.args.id === "b"));
});

test("team-from-plan drains, fetches, executes, and completes claimed task", async () => {
  const mcp = mockMcp({
    drains: [{ task_id: "claimed" }],
    tasks: { claimed: { id: "claimed", title: "Claimed" } },
  });
  let executed = null;

  const result = await _internal.runTeamFromPlanWaves({
    mcp,
    planId: "pln_1",
    waves: [{ wave: 0, tasks: ["fanout"] }],
    maxRetries: 0,
    workers: 1,
    agentType: "claude",
    cwd: "/tmp",
    stderrLog: null,
    stdout: { write() {}, isTTY: false },
    write() {},
    agentId: "agent_1",
    async executeTask({ task }) {
      executed = task.id;
      return { ok: true, attempts: 1, result: { teamName: "ok", counts: { total: 1, completed: 1, failed: 0 } } };
    },
  });

  assert.equal(executed, "claimed");
  assert.equal(result.summaries[0].taskId, "claimed");
  assert(mcp.calls.some((c) => c.name === "daruma_plan_drain_next" && c.args.plan_id === "pln_1"));
  assert(mcp.calls.some((c) => c.name === "daruma_complete" && c.args.id === "claimed" && c.args.result_summary.includes("completed=1")));
});


test("executor circuit breaker bounds retries and never completes failed work", async () => {
  for (const [errors, remaining, expected, attempts] of [
    [["same", "same", "unused"], 100, "repeated_error", 2],
    [["one", "two", "three", "unused"], 100, "consecutive_failures", 3],
    [["one", "unused"], 1, "iteration_budget_exhausted", 1],
    [["unused"], 0, "iteration_budget_exhausted", 0],
  ]) {
    const mcp = mockMcp();
    const budget = { remaining, reason: null };
    let executions = 0;
    const outcome = await _internal.executeTaskWithRetries({
      mcp, task: { id: "task", title: "Task" }, maxRetries: 100,
      stdout: { write() {}, isTTY: false }, write() {}, budget,
      async execute() {
        const error = errors[executions++];
        return { ok: false, counts: { completed: 0, failed: 1 }, artifact: error,
          tasks: [{ status: "failed", result: error }] };
      },
    });
    assert.equal(outcome.reason, expected);
    assert.equal(outcome.attempts, attempts);
    assert.equal(executions, attempts);
    assert(!mcp.calls.some((call) => call.name === "daruma_complete"));
    assert(mcp.calls.some((call) => call.name === "daruma_comment" && call.args.kind === "blocker" && call.args.body.includes(expected)));
  }
});

test("executor errors stop immediately; success uses remaining shared budget", async () => {
  const mcp = mockMcp();
  const base = { mcp, task: { id: "task" }, maxRetries: 100, stdout: {}, write() {} };
  const failed = await _internal.executeTaskWithRetries({ ...base, async execute() { throw new Error("spawn failed"); } });
  assert.equal(failed.reason, "executor_error");
  assert.equal(failed.attempts, 1);
  const budget = { remaining: 1, reason: null };
  const success = await _internal.executeTaskWithRetries({ ...base, budget, async execute() {
    return { ok: true, counts: { completed: 1, failed: 0 }, artifact: "Tests passed" };
  } });
  assert.equal(success.ok, true);
  assert.equal(budget.remaining, 0);
  await assert.rejects(_internal.executeTaskWithRetries({ ...base, maxRetries: Infinity }), /nonnegative integer/);
});


test("claimed execution exception releases claim, aborts real run and stops waves", async () => {
  const mcp = mockMcp({ drains: [{ task_id: "a" }, { task_id: "b" }] });
  await assert.rejects(_internal.runTeamFromPlanWaves({
    mcp, planId: "plan", waves: [{ wave: 0, tasks: ["a"] }, { wave: 1, tasks: ["b"] }],
    maxRetries: 2, workers: 1, agentId: "agent", write() {},
    async executeTask() { throw new Error("test executor failed"); },
  }), /test executor failed/);
  assert(mcp.calls.some((call) => call.name === "daruma_release" && call.args.task_id === "a"));
  assert(mcp.calls.some((call) => call.name === "daruma_run_abort" && call.args.run_id === "00000000-0000-4000-8000-000000000001"));
  assert(!mcp.calls.some((call) => call.name === "daruma_get" && call.args.id === "b"));
});


test("sequential plan executor activates draft, drains claims and completes its run", async () => {
  const mcp = mockMcp({ drains: [{ task_id: "a" }, null] });
  const outcome = await _internal.runPlanLoop({
    mcp, plan: { id: "plan", status: "draft" }, cwd: "/tmp", write() {},
    async executeTask() { return { ok: true, attempts: 1, result: { counts: { completed: 1 } } }; },
  });
  assert.equal(outcome.summaries.length, 1);
  const names = mcp.calls.map((call) => call.name);
  assert(names.indexOf("daruma_plan_set_status") < names.indexOf("daruma_run_start"));
  assert(names.includes("daruma_complete"));
  assert(names.includes("daruma_run_complete"));
  assert(!names.includes("daruma_run_abort"));
  assert(mcp.calls.filter((call) => call.name === "daruma_plan_drain_next").every((call) => call.args.run_id === outcome.runId));
});


test("each retry reads fresh Daruma state and run journal into executor prompt", async () => {
  const mcp = mockMcp({ tasks: { task: { id: "task", title: "Server title", description: "Server statement" } } });
  let calls = 0;
  await _internal.executeTaskWithRetries({
    mcp, task: { id: "task", title: "Stale title" }, maxRetries: 1,
    runId: "run", stdout: {}, write() {},
    async execute({ task }) {
      const prompt = _internal.executePromptFor(task);
      assert(prompt.includes("Server title"));
      assert(!prompt.includes("Stale title"));
      assert(prompt.includes("Previous check passed"));
      assert(prompt.includes('"run_id":"run"'));
      return { ok: ++calls === 2, counts: { completed: 0, failed: 1 }, artifact: "attempt" };
    },
  });
  assert.equal(calls, 2);
  assert.equal(mcp.calls.filter((call) => call.name === "daruma_get").length, 2);
  assert.equal(mcp.calls.filter((call) => call.name === "daruma_run_notes_list").length, 2);
  assert.equal(mcp.calls.filter((call) => call.name === "daruma_run_note_append").length, 2);
});


test("terminal task in server projection cannot be reopened by stale executor", async () => {
  const mcp = mockMcp({ tasks: { task: { id: "task", status: "done" } } });
  const outcome = await _internal.executeTaskWithRetries({
    mcp, task: { id: "task", status: "todo" }, maxRetries: 2,
    stdout: {}, write() {}, async execute() { assert.fail("terminal task executed"); },
  });
  assert.equal(outcome.reason, "task_no_longer_active");
  assert.equal(outcome.attempts, 0);
  assert(!mcp.calls.some((call) => ["daruma_set_status", "daruma_complete"].includes(call.name)));
});

function projectCreateMcp(createResult) {
  const calls = [];
  return {
    calls,
    async callTool(name, args) {
      calls.push({ name, args });
      if (name === "daruma_workspace_info") return { parsed: { scopes: [] } };
      if (name === "daruma_project_list") return { parsed: [{ id: "other", title: "unrelated" }] };
      if (name === "daruma_project_create") return { parsed: createResult, text: JSON.stringify(createResult) };
      return { parsed: { ok: true }, text: "{}" };
    },
  };
}

test("missing project is created and its project_id is used for the run", async () => {
  // Same shape crates/mcp returns for daruma_project_create.
  const mcp = projectCreateMcp({ project_id: "prj-new", events: { data: [] } });
  const lines = [];
  const projectId = await _internal.resolveProject({
    mcp, cwd: "/work/fresh-repo", write: (line) => lines.push(line),
  });
  assert.equal(projectId, "prj-new");
  const create = mcp.calls.find((call) => call.name === "daruma_project_create");
  assert.equal(create.args.title, "fresh-repo");
  assert.match(lines.join("\n"), /created new project "fresh-repo" \(prj-new\)/);
});

test("project creation without a returned project_id fails loudly", async () => {
  const mcp = projectCreateMcp({ project_id: null, events: { data: [] } });
  await assert.rejects(
    _internal.resolveProject({ mcp, cwd: "/work/fresh-repo", write() {} }),
    /daruma_project_create returned no project_id/,
  );
});

test("start spawns the same `daruma mcp` launcher doctor probes", { skip: process.platform === "win32" }, async () => {
  await withTempDir(async (dir) => {
    const bin = join(dir, "daruma");
    const argvOut = join(dir, "argv.txt");
    // Records its argv, reads the initialize request, exits: start fails
    // right after the spawn without the client writing into a closed pipe.
    await writeFile(bin, `#!/bin/sh\nprintf '%s\\n' "$@" > '${argvOut}'\nread -r _\nexit 3\n`);
    await chmod(bin, 0o755);
    const saved = { bin: process.env.DARUMA_MCP_BIN, agentDir: process.env.DARUMA_AGENT_DIR };
    process.env.DARUMA_MCP_BIN = bin;
    process.env.DARUMA_AGENT_DIR = join(dir, "agent");
    const lines = [];
    const stdout = { isTTY: false, write: (s) => { lines.push(s); return true; } };
    try {
      await assert.rejects(runDarumaStart({ task: "probe launcher", cwd: dir, stdout, autoYes: true }));
    } finally {
      for (const [key, value] of [["DARUMA_MCP_BIN", saved.bin], ["DARUMA_AGENT_DIR", saved.agentDir]]) {
        if (value === undefined) delete process.env[key];
        else process.env[key] = value;
      }
    }
    assert.equal(await readFile(argvOut, "utf8"), "mcp\n");
    assert.match(lines.join(""), new RegExp(`starting ${bin} mcp`));
  });
});

// `--plan` executes the materialized one-task plan; nothing decomposes it, so
// a server without any AI tool or provider runs it to completion (README "Plan mode").
test("start --plan runs the root task as a one-task plan and never asks for AI decomposition", { skip: process.platform === "win32" }, async () => {
  await withTempDir(async (dir) => {
    const bin = join(dir, "bin");
    const repo = join(dir, "repo");
    const log = join(dir, "requests.log");
    await mkdir(bin);
    await mkdir(repo);
    // Fake `daruma mcp`: logs every JSON-RPC request, answers the plan-mode calls.
    await writeFile(join(bin, "daruma"), `#!/usr/bin/env node
const { appendFileSync } = require("node:fs");
const readline = require("node:readline");
let drains = 0;
const answers = {
  daruma_plan_materialize: () => ({ data: [
    { payload: { type: "plan_created", plan: { id: "pln", status: "active" } } },
    { payload: { type: "task_created", task: { id: "tsk", title: "root" } } },
  ] }),
  daruma_workspace_info: () => ({ mcp_agent_id: "agent" }),
  daruma_run_start: () => ({ data: { run_id: "00000000-0000-4000-8000-000000000002" } }),
  daruma_plan_drain_next: () => (drains++ === 0 ? { task_id: "tsk" } : null),
  daruma_get: () => ({ id: "tsk", title: "root", status: "todo" }),
  daruma_run_notes_list: () => ({ notes: [] }),
  daruma_plan_get: () => ({ id: "pln", status: "completed" }),
};
readline.createInterface({ input: process.stdin }).on("line", (line) => {
  const msg = JSON.parse(line);
  appendFileSync(${JSON.stringify(log)}, JSON.stringify({ method: msg.method, params: msg.params }) + "\\n");
  if (msg.id === undefined) return;
  let result;
  if (msg.method === "initialize") {
    result = { protocolVersion: "2025-06-18", capabilities: {}, serverInfo: { name: "fake", version: "0" } };
  } else if (msg.method === "tools/call") {
    const body = (answers[msg.params.name] ?? (() => ({ ok: true })))();
    result = { content: [{ type: "text", text: JSON.stringify(body) }] };
  } else {
    result = { tools: [] };
  }
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result }) + "\\n");
});
`);
    await writeFile(join(bin, "omc"), `#!/bin/sh
if [ "$1 $2" = "team api" ]; then
  case "$3" in
    get-summary) echo '{"ok":true,"data":{"summary":{"tasks":{"total":1,"completed":1}}}}' ;;
    *) echo '{"ok":true,"data":{"tasks":[]}}' ;;
  esac
  exit 0
fi
if [ "$1 $2" = "team shutdown" ]; then exit 0; fi
mkdir -p .omc/state/team/plan-mode-team
`);
    await chmod(join(bin, "daruma"), 0o755);
    await chmod(join(bin, "omc"), 0o755);
    const saved = { bin: process.env.DARUMA_MCP_BIN, agentDir: process.env.DARUMA_AGENT_DIR, path: process.env.PATH };
    process.env.DARUMA_MCP_BIN = join(bin, "daruma");
    process.env.DARUMA_AGENT_DIR = join(dir, "agent");
    process.env.PATH = `${bin}:${saved.path}`;
    const lines = [];
    const stdout = { isTTY: false, write: (s) => { lines.push(s); return true; } };
    let result;
    try {
      result = await runDarumaStart({
        task: "plan mode root", cwd: repo, stdout, autoYes: true, planMode: true, projectId: "prj", maxRetries: 0,
      });
    } finally {
      for (const [key, value] of [["DARUMA_MCP_BIN", saved.bin], ["DARUMA_AGENT_DIR", saved.agentDir], ["PATH", saved.path]]) {
        if (value === undefined) delete process.env[key];
        else process.env[key] = value;
      }
    }
    const requests = (await readFile(log, "utf8")).split("\n").filter(Boolean).map((l) => JSON.parse(l));
    const tools = requests.filter((r) => r.method === "tools/call").map((r) => r.params.name);
    assert.equal(result.ok, true);
    assert.equal(result.planId, "pln");
    assert.deepEqual(result.summaries.map((s) => s.taskId), ["tsk"]);
    const materialize = requests.find((r) => r.params?.name === "daruma_plan_materialize");
    assert.equal(materialize.params.arguments.tasks.length, 1);
    assert(!tools.some((name) => /decompose/.test(name)), `unexpected decomposition call in ${tools}`);
    assert(!requests.some((r) => r.method === "tools/list"));
    assert(tools.includes("daruma_run_complete"));
    assert.doesNotMatch(lines.join(""), /\[decompose\]/);
  });
});

function cancellationHarness({ graceMs = 60_000 } = {}) {
  const lines = [];
  const events = [];
  const cancel = _internal.createCancellation({
    write: (s) => lines.push(s),
    stop: async () => { events.push("stop"); },
    exit: (code) => { events.push(`exit:${code}`); },
    graceMs,
  });
  return { cancel, lines, events };
}

test("SIGINT with no omc team running closes MCP and exits 130 at once", async () => {
  const { cancel, events } = cancellationHarness();
  await cancel.onSignal();
  assert.deepEqual(events, ["stop", "exit:130"]);
  assert.equal(cancel.signal.aborted, true);
});

test("SIGINT with omc team running aborts it and waits instead of exiting", async () => {
  const { cancel, lines, events } = cancellationHarness();
  let finish;
  cancel.teams.add("team-a");
  cancel.track(new Promise((resolve) => { finish = resolve; }));
  cancel.onSignal();
  assert.equal(cancel.signal.aborted, true);
  assert.equal(cancel.interrupted, true);
  assert.deepEqual(events, []);
  assert.match(lines.join(""), /stopping omc team team-a/);
  finish();
  cancel.dispose();
  await new Promise((r) => setTimeout(r, 10));
  assert.deepEqual(events, []);
});

test("second SIGINT or grace timeout exits and names the team to shut down", async () => {
  const twice = cancellationHarness();
  twice.cancel.teams.add("team-a");
  twice.cancel.track(new Promise(() => {}));
  twice.cancel.onSignal();
  await twice.cancel.onSignal();
  assert.deepEqual(twice.events, ["stop", "exit:130"]);
  assert.match(twice.lines.join(""), /omc team shutdown team-a/);

  const slow = cancellationHarness({ graceMs: 5 });
  slow.cancel.teams.add("team-b");
  slow.cancel.track(new Promise(() => {}));
  slow.cancel.onSignal();
  await new Promise((r) => setTimeout(r, 30));
  assert.deepEqual(slow.events, ["stop", "exit:130"]);
  assert.match(slow.lines.join(""), /did not stop within .*omc team shutdown team-b/);
});

test("cancelled execution records a cancelled blocker and never retries", async () => {
  const mcp = mockMcp();
  const { cancel } = cancellationHarness();
  let executions = 0;
  const outcome = await _internal.executeTaskWithRetries({
    mcp, task: { id: "task", title: "Task" }, maxRetries: 2,
    stdout: { write() {}, isTTY: false }, write() {}, cancel,
    async execute({ cancel: received }) {
      executions++;
      assert.equal(received, cancel);
      cancel.track(new Promise(() => {}));
      cancel.onSignal();
      throw new Error("omc team poll aborted");
    },
  });
  cancel.dispose();
  assert.equal(outcome.ok, false);
  assert.equal(outcome.reason, "cancelled");
  assert.equal(executions, 1);
  assert(!mcp.calls.some((call) => call.name === "daruma_complete"));
  assert(mcp.calls.some((call) => call.name === "daruma_comment" && call.args.kind === "blocker" && /cancelled by the operator/.test(call.args.body)));
});

test("cancelled plan run releases the claim and aborts the run as cancelled", async () => {
  const mcp = mockMcp({ drains: [{ task_id: "a" }, { task_id: "b" }] });
  const { cancel } = cancellationHarness();
  const outcome = await _internal.runPlanLoop({
    mcp, plan: { id: "plan", status: "active" }, cwd: "/tmp", write() {}, stdout: {}, maxRetries: 2, cancel,
    executeTask: (args) => _internal.executeTaskWithRetries({ ...args, async execute() {
      cancel.track(new Promise(() => {}));
      cancel.onSignal();
      throw new Error("omc team poll aborted");
    } }),
  });
  cancel.dispose();
  assert.equal(outcome.reason, "cancelled");
  assert(mcp.calls.some((call) => call.name === "daruma_release" && call.args.task_id === "a"));
  assert(mcp.calls.some((call) => call.name === "daruma_run_abort" && call.args.reason === "cancelled"));
  assert(!mcp.calls.some((call) => call.name === "daruma_get" && call.args.id === "b"));
});

test("Ctrl-C while the final answer is pending wins over a late success", async () => {
  const mcp = mockMcp();
  const { cancel } = cancellationHarness();
  const outcome = await _internal.executeTaskWithRetries({
    mcp, task: { id: "task", title: "Task" }, maxRetries: 2,
    stdout: { write() {}, isTTY: false }, write() {}, cancel,
    async execute() {
      // The team already finished; Ctrl-C lands during list-tasks/shutdown.
      cancel.track(new Promise(() => {}));
      cancel.onSignal();
      return { ok: true, teamName: "team-a", counts: { total: 1, completed: 1, failed: 0 }, artifact: "done", shutdownError: null };
    },
  });
  cancel.dispose();
  assert.equal(outcome.ok, false);
  assert.equal(outcome.reason, "cancelled");
  assert(!mcp.calls.some((call) => call.name === "daruma_complete"));
  const blocker = mcp.calls.find((call) => call.name === "daruma_comment" && call.args.kind === "blocker");
  assert.match(blocker.args.body, /cancelled by the operator \(Ctrl-C\); attempts=1\. omc team was shut down\./);
});

test("plan task whose success arrives after Ctrl-C is released, not completed", async () => {
  const mcp = mockMcp({ drains: [{ task_id: "a" }, { task_id: "b" }] });
  const { cancel } = cancellationHarness();
  const outcome = await _internal.runPlanLoop({
    mcp, plan: { id: "plan", status: "active" }, cwd: "/tmp", write() {}, stdout: {}, maxRetries: 2, cancel,
    async executeTask() {
      cancel.track(new Promise(() => {}));
      cancel.onSignal();
      return { ok: true, attempts: 1, result: { teamName: "team-a", counts: { total: 1, completed: 1, failed: 0 } } };
    },
  });
  cancel.dispose();
  assert.equal(outcome.reason, "cancelled");
  assert(!mcp.calls.some((call) => call.name === "daruma_complete"));
  assert(mcp.calls.some((call) => call.name === "daruma_release" && call.args.task_id === "a"));
  assert(mcp.calls.some((call) => call.name === "daruma_run_abort" && call.args.reason === "cancelled"));
  assert.equal(mcp.calls.filter((call) => call.name === "daruma_plan_drain_next").length, 1);
});

test("failed omc team shutdown after Ctrl-C is named in the blocker", async () => {
  const mcp = mockMcp();
  const { cancel } = cancellationHarness();
  const outcome = await _internal.executeTaskWithRetries({
    mcp, task: { id: "task", title: "Task" }, maxRetries: 2,
    stdout: { write() {}, isTTY: false }, write() {}, cancel,
    async execute() {
      cancel.track(new Promise(() => {}));
      cancel.onSignal();
      throw Object.assign(new Error("omc team poll aborted"), {
        teamName: "team-x", shutdownError: "omc team shutdown team-x failed: timeout",
      });
    },
  });
  cancel.dispose();
  assert.equal(outcome.reason, "cancelled");
  assert.equal(outcome.result.shutdownError, "omc team shutdown team-x failed: timeout");
  const blocker = mcp.calls.find((call) => call.name === "daruma_comment" && call.args.kind === "blocker");
  assert.match(blocker.args.body, /omc team shutdown team-x failed: timeout — omc team may still be running; stop it with: omc team shutdown team-x\./);
  assert.doesNotMatch(blocker.args.body, /was shut down/);
});

test("executeOnce keeps a team whose shutdown failed registered for the exit message", { skip: process.platform === "win32" }, async () => {
  await withTempDir(async (dir) => {
    const bin = join(dir, "bin");
    const cwd = join(dir, "repo");
    await mkdir(bin);
    await mkdir(cwd);
    await writeFile(join(bin, "omc"), `#!/bin/sh
if [ "$1 $2" = "team api" ]; then
  case "$3" in
    get-summary) echo '{"ok":true,"data":{"summary":{"tasks":{"total":1,"completed":1}}}}' ;;
    *) echo '{"ok":true,"data":{"tasks":[]}}' ;;
  esac
  exit 0
fi
if [ "$1 $2" = "team shutdown" ]; then echo "tmux: no server" >&2; exit 1; fi
mkdir -p .omc/state/team/stuck-team
`);
    await chmod(join(bin, "omc"), 0o755);
    const savedPath = process.env.PATH;
    process.env.PATH = `${bin}:${savedPath}`;
    const { cancel, lines: cancelLines } = cancellationHarness();
    const lines = [];
    let result;
    try {
      result = await _internal.executeOnce({
        task: { id: "task", title: "Task" }, workers: 1, agentType: "claude", cwd, stderrLog: null,
        stdout: { isTTY: false, write() {} }, write: (s) => lines.push(s), cancel,
      });
    } finally {
      process.env.PATH = savedPath;
    }
    assert.equal(result.ok, true);
    assert.match(result.shutdownError, /^omc team shutdown stuck-team failed: tmux: no server/);
    assert.match(lines.join("\n"), /\[omc team\] omc team shutdown stuck-team failed/);
    assert.deepEqual([...cancel.teams], ["stuck-team"]);
    await cancel.onSignal();
    assert.match(cancelLines.join(""), /SIGINT — shutting down; stop it with: omc team shutdown stuck-team/);
  });
});

test("a failing MCP stop on Ctrl-C is reported, and the process still exits 130", async () => {
  const lines = [];
  const events = [];
  const cancel = _internal.createCancellation({
    write: (s) => lines.push(s),
    stop: async () => { throw new Error("server pipe closed"); },
    exit: (code) => { events.push(`exit:${code}`); },
  });
  await cancel.onSignal();
  assert.deepEqual(events, ["exit:130"]);
  assert.match(lines.join(""), /failed to stop the daruma MCP server: server pipe closed/);
});

test("Ctrl-C typed into the TTY prompt reaches the process SIGINT handler", async () => {
  const { PassThrough } = await import("node:stream");
  const prompt = _internal.makePrompt(new PassThrough(), new PassThrough());
  let received = 0;
  const handler = () => { received++; };
  process.on("SIGINT", handler);
  try {
    // readline emits this for ^C on a raw-mode terminal instead of a signal.
    prompt.rl.emit("SIGINT");
  } finally {
    process.off("SIGINT", handler);
    prompt.close();
  }
  assert.equal(received, 1);
});
