import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { installPolicy, removePolicy } from "../lib/policy.mjs";

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "taskagent-claude-policy-test-"));
  try { return await fn(dir); }
  finally { await rm(dir, { recursive: true, force: true }); }
}

const BEGIN = "<!-- taskagent-claude:policy:begin -->";
const END = "<!-- taskagent-claude:policy:end -->";

test("installPolicy creates CLAUDE.md with policy block when missing", async () => {
  await withTempDir(async (dir) => {
    const result = await installPolicy({ projectDir: dir });
    assert.equal(result.action, "installed");
    const body = await fs.readFile(join(dir, "CLAUDE.md"), "utf8");
    assert.ok(body.includes(BEGIN));
    assert.ok(body.includes(END));
    assert.match(body, /taskagent_plan_create/);
    assert.match(body, /\.omc\/plans\//);
    assert.match(body, /\/taskagent-claude:tasks/);
    // Trigger-word guard.
    assert.match(body, /трекер/);
    assert.match(body, /tracker/);
    assert.match(body, /status=all/);
    // Token-economy guard: list-first, no "Prefer search" default.
    assert.match(body, /Go straight to the goal/);
    assert.doesNotMatch(body, /Prefer `taskagent_search`/);
  });
});

test("installPolicy appends to existing CLAUDE.md without overwriting", async () => {
  await withTempDir(async (dir) => {
    const target = join(dir, "CLAUDE.md");
    await fs.writeFile(target, "# Project notes\nkeep me.\n");

    const result = await installPolicy({ projectDir: dir });
    assert.equal(result.action, "appended");

    const body = await fs.readFile(target, "utf8");
    assert.match(body, /^# Project notes/);
    assert.match(body, /keep me\./);
    assert.ok(body.includes(BEGIN));
    assert.ok(body.includes(END));
  });
});

test("installPolicy refreshes managed block in place", async () => {
  await withTempDir(async (dir) => {
    const target = join(dir, "CLAUDE.md");
    await fs.writeFile(
      target,
      `# Preamble\n\n${BEGIN}\nstale content\n${END}\n\nAfter\n`,
    );

    const result = await installPolicy({ projectDir: dir });
    assert.equal(result.action, "updated");

    const body = await fs.readFile(target, "utf8");
    assert.match(body, /^# Preamble/);
    assert.match(body, /\nAfter\n?$/);
    assert.ok(!body.includes("stale content"));
    assert.match(body, /taskagent_plan_create/);
  });
});

test("installPolicy returns unchanged when content already current", async () => {
  await withTempDir(async (dir) => {
    const first = await installPolicy({ projectDir: dir });
    assert.equal(first.action, "installed");
    const again = await installPolicy({ projectDir: dir });
    assert.equal(again.action, "unchanged");
  });
});

test("removePolicy deletes CLAUDE.md when block was the only content", async () => {
  await withTempDir(async (dir) => {
    await installPolicy({ projectDir: dir });
    const result = await removePolicy({ projectDir: dir });
    assert.equal(result.action, "removed-file");
    const stat = await fs.stat(join(dir, "CLAUDE.md")).catch(() => null);
    assert.equal(stat, null);
  });
});

test("removePolicy preserves surrounding content", async () => {
  await withTempDir(async (dir) => {
    const target = join(dir, "CLAUDE.md");
    await fs.writeFile(target, "# Keep me\n\n");
    await installPolicy({ projectDir: dir });
    const result = await removePolicy({ projectDir: dir });
    assert.equal(result.action, "removed-block");
    const body = await fs.readFile(target, "utf8");
    assert.match(body, /^# Keep me/);
    assert.ok(!body.includes(BEGIN));
  });
});

test("removePolicy is a no-op when no managed block exists", async () => {
  await withTempDir(async (dir) => {
    const result = await removePolicy({ projectDir: dir });
    assert.equal(result.action, "missing");
  });
});
