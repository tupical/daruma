import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, symlink, mkdir, writeFile, chmod } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  candidateMcpCommandPaths,
  resolveMcpCommand,
} from "../lib/resolve-mcp-command.mjs";

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "taskagent-mcp-cmd-"));
  try {
    return await fn(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

test("resolveMcpCommand keeps explicit absolute executable", async () => {
  await withTempDir(async (dir) => {
    const bin = join(dir, "taskagent-mcp");
    await writeFile(bin, "#!/bin/sh\nexit 0\n", "utf8");
    await chmod(bin, 0o755);
    const resolved = await resolveMcpCommand({ command: bin, cwd: dir });
    assert.equal(resolved.command, bin);
    assert.equal(resolved.resolved, true);
    assert.equal(resolved.source, "explicit-absolute");
  });
});

test("resolveMcpCommand discovers release binary near cwd", async () => {
  await withTempDir(async (dir) => {
    const releaseDir = join(dir, "target", "release");
    await mkdir(releaseDir, { recursive: true });
    const bin = join(releaseDir, "taskagent-mcp");
    await writeFile(bin, "#!/bin/sh\nexit 0\n", "utf8");
    await chmod(bin, 0o755);
    const resolved = await resolveMcpCommand({
      cwd: dir,
      env: {
        HOME: dir,
        CARGO_HOME: join(dir, ".cargo"),
      },
    });
    assert.equal(resolved.command, bin);
    assert.equal(resolved.resolved, true);
    assert.equal(resolved.source, "discovered");
  });
});

test("resolveMcpCommand honours TASKAGENT_MCP_BIN", async () => {
  await withTempDir(async (dir) => {
    const bin = join(dir, "custom-mcp");
    await writeFile(bin, "#!/bin/sh\nexit 0\n", "utf8");
    await chmod(bin, 0o755);
    const resolved = await resolveMcpCommand({
      cwd: dir,
      env: { TASKAGENT_MCP_BIN: bin },
    });
    assert.equal(resolved.command, bin);
    assert.equal(resolved.resolved, true);
  });
});

test("candidateMcpCommandPaths includes local bin and release dirs", async () => {
  await withTempDir(async (dir) => {
    const paths = candidateMcpCommandPaths({ cwd: dir, env: {} });
    assert.ok(paths.includes("taskagent-mcp"));
    assert.ok(paths.some((p) => p.endsWith("/target/release/taskagent-mcp")));
  });
});
