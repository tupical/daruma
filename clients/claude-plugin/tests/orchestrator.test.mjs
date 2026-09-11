import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

import { _internal } from "../lib/orchestrator.mjs";

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
      if (name === "daruma_get") {
        const task = tasks[args.id] ?? { id: args.id, title: args.id };
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
