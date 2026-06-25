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
