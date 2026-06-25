import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  installOmcGuard,
  omcDirExists,
  removeOmcGuard,
} from "../lib/omc-guard.mjs";

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "daruma-omc-guard-test-"));
  try { return await fn(dir); }
  finally { await rm(dir, { recursive: true, force: true }); }
}

const BEGIN = "<!-- daruma-claude:begin -->";
const END = "<!-- daruma-claude:end -->";

test("installOmcGuard no-ops when .omc/ is absent", async () => {
  await withTempDir(async (dir) => {
    assert.equal(await omcDirExists(dir), false);
    const result = await installOmcGuard({ projectDir: dir });
    assert.equal(result.action, "skipped-no-omc");
    const stat = await fs.stat(join(dir, ".omc", "AGENTS.md")).catch(() => null);
    assert.equal(stat, null);
  });
});

test("installOmcGuard creates AGENTS.md with managed block when .omc/ exists", async () => {
  await withTempDir(async (dir) => {
    await fs.mkdir(join(dir, ".omc"), { recursive: true });
    const result = await installOmcGuard({ projectDir: dir });
    assert.equal(result.action, "installed");

    const body = await fs.readFile(join(dir, ".omc", "AGENTS.md"), "utf8");
    assert.ok(body.includes(BEGIN));
    assert.ok(body.includes(END));
    assert.match(body, /daruma_plan_create/);
    assert.match(body, /\.omc\/plans\//);
  });
});

test("installOmcGuard appends managed block to existing AGENTS.md without overwriting", async () => {
  await withTempDir(async (dir) => {
    await fs.mkdir(join(dir, ".omc"), { recursive: true });
    const target = join(dir, ".omc", "AGENTS.md");
    await fs.writeFile(target, "# Existing notes\nKeep me.\n");

    const result = await installOmcGuard({ projectDir: dir });
    assert.equal(result.action, "appended");

    const body = await fs.readFile(target, "utf8");
    assert.match(body, /^# Existing notes/);
    assert.match(body, /Keep me\./);
    assert.ok(body.includes(BEGIN));
    assert.ok(body.includes(END));
  });
});

test("installOmcGuard refreshes existing managed block in place", async () => {
  await withTempDir(async (dir) => {
    await fs.mkdir(join(dir, ".omc"), { recursive: true });
    const target = join(dir, ".omc", "AGENTS.md");
    await fs.writeFile(
      target,
      `# Preamble\n\n${BEGIN}\nold stale content\n${END}\n\nAfter\n`,
    );

    const result = await installOmcGuard({ projectDir: dir });
    assert.equal(result.action, "updated");

    const body = await fs.readFile(target, "utf8");
    assert.match(body, /^# Preamble/);
    assert.match(body, /\nAfter\n?$/);
    assert.ok(!body.includes("old stale content"));
    assert.match(body, /daruma_plan_create/);
  });
});

test("installOmcGuard returns unchanged when content matches", async () => {
  await withTempDir(async (dir) => {
    await fs.mkdir(join(dir, ".omc"), { recursive: true });
    const first = await installOmcGuard({ projectDir: dir });
    assert.equal(first.action, "installed");
    const again = await installOmcGuard({ projectDir: dir });
    assert.equal(again.action, "unchanged");
  });
});

test("removeOmcGuard deletes file when managed block was the only content", async () => {
  await withTempDir(async (dir) => {
    await fs.mkdir(join(dir, ".omc"), { recursive: true });
    await installOmcGuard({ projectDir: dir });
    const result = await removeOmcGuard({ projectDir: dir });
    assert.equal(result.action, "removed-file");
    const stat = await fs.stat(join(dir, ".omc", "AGENTS.md")).catch(() => null);
    assert.equal(stat, null);
  });
});

test("removeOmcGuard preserves surrounding content", async () => {
  await withTempDir(async (dir) => {
    await fs.mkdir(join(dir, ".omc"), { recursive: true });
    const target = join(dir, ".omc", "AGENTS.md");
    await fs.writeFile(target, "# Keep me\n\n");
    await installOmcGuard({ projectDir: dir });
    const result = await removeOmcGuard({ projectDir: dir });
    assert.equal(result.action, "removed-block");
    const body = await fs.readFile(target, "utf8");
    assert.match(body, /^# Keep me/);
    assert.ok(!body.includes(BEGIN));
  });
});
