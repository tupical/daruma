import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { RULE_FILES, installRules } from "../lib/rules.mjs";

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "daruma-cursor-rules-test-"));
  try { return await fn(dir); }
  finally { await rm(dir, { recursive: true, force: true }); }
}

test("installRules drops both bundled rules with alwaysApply policy", async () => {
  await withTempDir(async (dir) => {
    const results = await installRules({ projectDir: dir });
    assert.equal(results.length, RULE_FILES.length);
    for (const r of results) {
      assert.equal(r.action, "installed");
      const body = await fs.readFile(r.path, "utf8");
      assert.ok(body.startsWith("---"), `${r.name} should start with frontmatter`);
    }

    const policy = await fs.readFile(
      join(dir, ".cursor", "rules", "daruma-policy.mdc"),
      "utf8",
    );
    assert.match(policy, /alwaysApply:\s*true/);
    assert.match(policy, /\.omc\/plans\//);
    // Trigger-word guard: must mention both Russian and English forms
    // so the agent reaches for daruma on either side.
    assert.match(policy, /трекер/);
    assert.match(policy, /tracker/);
    // Token-economy guard: the always-applied policy must steer toward
    // list-first and must NOT carry the old "use search for lookups" hint
    // that pushed the agent into expensive search/graph dumps.
    assert.match(policy, /status:\s*"active"/);
    assert.doesNotMatch(policy, /for targeted lookups/);

    const contract = await fs.readFile(
      join(dir, ".cursor", "rules", "daruma.mdc"),
      "utf8",
    );
    assert.match(contract, /daruma-policy\.mdc/);
    // The on-demand contract must document the lean audit/close workflow
    // and drop the old "Prefer search over bulk list" guidance.
    assert.match(contract, /Audit & close workflow/);
    assert.doesNotMatch(contract, /Prefer search over bulk list/);

    const graph = await fs.readFile(
      join(dir, ".cursor", "rules", "workspacegraph.mdc"),
      "utf8",
    );
    // workspacegraph guardrail: never use graph search to list open tasks.
    assert.match(graph, /Never use `daruma_workspacegraph_search` to list open tasks/);
  });
});

test("installRules is idempotent without --force", async () => {
  await withTempDir(async (dir) => {
    await installRules({ projectDir: dir });
    const again = await installRules({ projectDir: dir });
    for (const r of again) {
      assert.equal(r.action, "skipped-exists");
    }
  });
});

test("installRules overwrites with overwrite: true", async () => {
  await withTempDir(async (dir) => {
    await installRules({ projectDir: dir });
    const target = join(dir, ".cursor", "rules", "daruma-policy.mdc");
    await fs.writeFile(target, "stale\n");

    const results = await installRules({ projectDir: dir, overwrite: true });
    for (const r of results) {
      assert.equal(r.action, "overwritten");
    }
    const restored = await fs.readFile(target, "utf8");
    assert.match(restored, /alwaysApply:\s*true/);
  });
});

test("RULE_FILES lists all managed rule names", () => {
  assert.deepEqual([...RULE_FILES].sort(), [
    "daruma-policy.mdc",
    "daruma.mdc",
    "workspacegraph.mdc",
  ]);
});
