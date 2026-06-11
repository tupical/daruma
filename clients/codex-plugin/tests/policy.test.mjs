import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { installPolicy, removePolicy } from "../lib/policy.mjs";

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "taskagent-codex-policy-test-"));
  try {
    return await fn(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

const BEGIN = "<!-- taskagent-codex:policy:begin -->";
const END = "<!-- taskagent-codex:policy:end -->";

test("installPolicy creates AGENTS.md with policy block when missing", async () => {
  await withTempDir(async (dir) => {
    const result = await installPolicy({ projectDir: dir });
    assert.equal(result.action, "installed");
    const body = await fs.readFile(join(dir, "AGENTS.md"), "utf8");
    assert.ok(body.includes(BEGIN));
    assert.ok(body.includes(END));
    assert.match(body, /taskagent_plan_create/);
    assert.match(body, /status=all/);
    assert.match(body, /трекер/);
    assert.match(body, /Verify real taskagent state/);
    assert.match(body, /checklist/);
    // Token-economy guard: list-first, no "Prefer search" default.
    assert.match(body, /Go straight to the goal/);
    assert.doesNotMatch(body, /Prefer `taskagent_search`/);
  });
});

test("installPolicy appends to existing AGENTS.md without overwriting", async () => {
  await withTempDir(async (dir) => {
    const target = join(dir, "AGENTS.md");
    await fs.writeFile(target, "# Project notes\nkeep me.\n");

    const result = await installPolicy({ projectDir: dir });
    assert.equal(result.action, "appended");

    const body = await fs.readFile(target, "utf8");
    assert.match(body, /^# Project notes/);
    assert.match(body, /keep me\./);
    assert.ok(body.includes(BEGIN));
  });
});

test("removePolicy preserves surrounding content", async () => {
  await withTempDir(async (dir) => {
    const target = join(dir, "AGENTS.md");
    await fs.writeFile(target, "# Keep me\n\n");
    await installPolicy({ projectDir: dir });
    const result = await removePolicy({ projectDir: dir });
    assert.equal(result.action, "removed-block");
    const body = await fs.readFile(target, "utf8");
    assert.match(body, /^# Keep me/);
    assert.ok(!body.includes(BEGIN));
  });
});
