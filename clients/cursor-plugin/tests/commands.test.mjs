import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { COMMAND_FILES, installCommands } from "../lib/commands.mjs";

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "taskagent-cursor-commands-test-"));
  try { return await fn(dir); }
  finally { await rm(dir, { recursive: true, force: true }); }
}

test("installCommands drops every bundled slash command", async () => {
  await withTempDir(async (dir) => {
    const results = await installCommands({ projectDir: dir });
    assert.equal(results.length, COMMAND_FILES.length);
    for (const r of results) {
      assert.equal(r.action, "installed");
      const body = await fs.readFile(r.path, "utf8");
      assert.ok(body.startsWith("---"), `${r.name} missing frontmatter`);
      assert.match(body, /name:\s*taskagent-/);
    }
  });
});

test("installCommands is idempotent without --force", async () => {
  await withTempDir(async (dir) => {
    await installCommands({ projectDir: dir });
    const again = await installCommands({ projectDir: dir });
    for (const r of again) {
      assert.equal(r.action, "skipped-exists");
    }
  });
});

test("installCommands overwrites with overwrite: true", async () => {
  await withTempDir(async (dir) => {
    await installCommands({ projectDir: dir });
    const target = join(dir, ".cursor", "commands", "taskagent-tasks.md");
    await fs.writeFile(target, "stale\n");

    const results = await installCommands({ projectDir: dir, overwrite: true });
    for (const r of results) {
      assert.equal(r.action, "overwritten");
    }
    const restored = await fs.readFile(target, "utf8");
    assert.match(restored, /name:\s*taskagent-tasks/);
  });
});

test("COMMAND_FILES covers tasks/plan/next/mine", () => {
  assert.deepEqual([...COMMAND_FILES].sort(), [
    "taskagent-mine.md",
    "taskagent-next.md",
    "taskagent-plan.md",
    "taskagent-tasks.md",
  ]);
});

test("each shipped command has a non-empty body after frontmatter", async () => {
  await withTempDir(async (dir) => {
    const results = await installCommands({ projectDir: dir });
    for (const r of results) {
      const body = await fs.readFile(r.path, "utf8");
      const match = body.match(/^---[\s\S]*?\n---\n([\s\S]*)$/);
      assert.ok(match, `${r.name} should have valid frontmatter delimiters`);
      assert.ok(match[1].trim().length > 100, `${r.name} body looks too short`);
    }
  });
});
