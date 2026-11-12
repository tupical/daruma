import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, chmod, readFile, rm } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const cli = fileURLToPath(new URL("../bin/daruma-cursor.mjs", import.meta.url));

test("install and deeplink preserve explicit commands and existing entries", async () => {
  const dir = await mkdtemp(join(tmpdir(), "cursor-command-"));
  try {
    const localBin = join(dir, ".local/bin/daruma");
    await mkdir(join(dir, ".local/bin"), { recursive: true });
    await mkdir(join(dir, "bin"), { recursive: true });
    await writeFile(localBin, "#!/bin/sh\nexit 0\n");
    await chmod(localBin, 0o755);
    const custom = join(dir, "bin/custom");
    await writeFile(custom, "#!/bin/sh\nexit 0\n");
    await chmod(custom, 0o755);
    const env = { ...process.env, HOME: dir, USERPROFILE: dir, CARGO_HOME: join(dir, "cargo"), DARUMA_MCP_BIN: "" };
    const run = (...args) => {
      const result = spawnSync(process.execPath, [cli, ...args], { cwd: dir, env, encoding: "utf8" });
      assert.equal(result.status, 0, result.stderr);
      return result.stdout;
    };
    const configPath = join(dir, ".cursor/mcp.json");
    const readCommand = async () => JSON.parse(await readFile(configPath, "utf8")).mcpServers.daruma.command;
    for (const [input, expected] of [[custom, custom], ["./bin/custom", custom], ["custom-name", "custom-name"], ["./bin/missing", resolve(dir, "bin/missing")]]) {
      run("install", "--project", dir, "--no-rules", "--no-omc-guard", "--force", "--command", input);
      assert.equal(await readCommand(), expected);
      const link = run("deeplink", "--command", input).trim();
      const config = JSON.parse(Buffer.from(new URL(link).searchParams.get("config"), "base64").toString());
      assert.equal(config.command, expected);
    }
    run("install", "--project", dir, "--no-rules", "--no-omc-guard", "--command", "another");
    assert.equal(await readCommand(), resolve(dir, "bin/missing"));
    run("install", "--project", dir, "--no-rules", "--no-omc-guard", "--force", "--transport", "stdio");
    assert.equal(await readCommand(), localBin);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
