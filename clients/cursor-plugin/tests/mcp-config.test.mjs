import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  listServers,
  readMcp,
  removeServer,
  resolveMcpPath,
  upsertServer,
} from "../lib/mcp-config.mjs";

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "taskagent-cursor-test-"));
  try { return await fn(dir); }
  finally { await rm(dir, { recursive: true, force: true }); }
}

test("resolveMcpPath returns project path under project scope", async () => {
  await withTempDir(async (dir) => {
    const path = resolveMcpPath({ scope: "project", projectDir: dir });
    assert.equal(path, join(dir, ".cursor", "mcp.json"));
  });
});

test("resolveMcpPath rejects unknown scope", () => {
  assert.throws(() => resolveMcpPath({ scope: "weird" }), RangeError);
});

test("readMcp returns empty mcpServers when file missing", async () => {
  await withTempDir(async (dir) => {
    const doc = await readMcp(join(dir, "nope.json"));
    assert.deepEqual(doc, { mcpServers: {} });
  });
});

test("upsertServer adds, replaces, and stays unchanged", async () => {
  await withTempDir(async (dir) => {
    const path = join(dir, ".cursor", "mcp.json");
    const entry = { type: "stdio", command: "taskagent-mcp" };

    const added = await upsertServer(path, "taskagent", entry);
    assert.equal(added.action, "added");

    const unchanged = await upsertServer(path, "taskagent", entry);
    assert.equal(unchanged.action, "unchanged");

    const replaced = await upsertServer(path, "taskagent", { ...entry, env: { X: "1" } });
    assert.equal(replaced.action, "replaced");

    const doc = JSON.parse(await fs.readFile(path, "utf8"));
    assert.deepEqual(doc.mcpServers.taskagent, { type: "stdio", command: "taskagent-mcp", env: { X: "1" } });
  });
});

test("upsertServer preserves unrelated mcpServers entries", async () => {
  await withTempDir(async (dir) => {
    const path = join(dir, ".cursor", "mcp.json");
    await fs.mkdir(join(dir, ".cursor"), { recursive: true });
    await fs.writeFile(path, JSON.stringify({
      mcpServers: { other: { type: "stdio", command: "other-bin" } },
    }));

    await upsertServer(path, "taskagent", { type: "stdio", command: "taskagent-mcp" });
    const list = await listServers(path);
    const names = list.map((x) => x.name).sort();
    assert.deepEqual(names, ["other", "taskagent"]);
  });
});

test("removeServer removes and reports missing", async () => {
  await withTempDir(async (dir) => {
    const path = join(dir, ".cursor", "mcp.json");
    await upsertServer(path, "taskagent", { type: "stdio", command: "x" });
    const removed = await removeServer(path, "taskagent");
    assert.equal(removed.action, "removed");
    const again = await removeServer(path, "taskagent");
    assert.equal(again.action, "missing");
  });
});
