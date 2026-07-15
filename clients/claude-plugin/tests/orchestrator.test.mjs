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
