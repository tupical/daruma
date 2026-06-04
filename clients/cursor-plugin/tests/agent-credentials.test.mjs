import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile, mkdir } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  agentDirRoot,
  credentialsPath,
  loadCredentials,
  migrateLegacyCredentialsIfNeeded,
  resolveMcpEnvFromCredentials,
} from "../lib/agent-credentials.mjs";

async function withAgentDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "taskagent-agent-test-"));
  const prev = process.env.TASKAGENT_AGENT_DIR;
  process.env.TASKAGENT_AGENT_DIR = dir;
  try {
    return await fn(dir);
  } finally {
    if (prev === undefined) delete process.env.TASKAGENT_AGENT_DIR;
    else process.env.TASKAGENT_AGENT_DIR = prev;
    await rm(dir, { recursive: true, force: true });
  }
}

test("agentDirRoot honours TASKAGENT_AGENT_DIR", async () => {
  await withAgentDir(async (dir) => {
    assert.equal(agentDirRoot(), dir);
    assert.equal(credentialsPath(), join(dir, "credentials.json"));
  });
});

test("resolveMcpEnvFromCredentials reads remote profile", async () => {
  await withAgentDir(async (dir) => {
    await mkdir(dir, { recursive: true });
    await writeFile(
      join(dir, "credentials.json"),
      JSON.stringify({
        schema_version: 1,
        active_profile: "remote-default",
        profiles: {
          "remote-default": {
            mode: "remote",
            server_url: "https://remote.example",
            token: "ta_pat_test",
            workspace_id: "ws-uuid",
          },
        },
      }),
      "utf8",
    );
    const env = await resolveMcpEnvFromCredentials();
    assert.equal(env.TASKAGENT_API_URL, "https://remote.example");
    assert.equal(env.TASKAGENT_TOKEN, "ta_pat_test");
    assert.equal(env.TASKAGENT_WORKSPACE_ID, "ws-uuid");
  });
});

test("migrateLegacyCredentialsIfNeeded copies XDG file", async () => {
  const root = await mkdtemp(join(tmpdir(), "taskagent-migrate-"));
  const legacyDir = join(root, ".config", "taskagent");
  const prevAgent = process.env.TASKAGENT_AGENT_DIR;
  const prevHome = process.env.HOME;
  const prevXdg = process.env.XDG_CONFIG_HOME;
  process.env.HOME = root;
  delete process.env.TASKAGENT_AGENT_DIR;
  delete process.env.XDG_CONFIG_HOME;
  try {
    await mkdir(legacyDir, { recursive: true });
    await writeFile(
      join(legacyDir, "credentials.json"),
      JSON.stringify({ schema_version: 1, profiles: {} }),
      "utf8",
    );
    const migrated = await migrateLegacyCredentialsIfNeeded();
    assert.equal(migrated, true);
    const creds = await loadCredentials();
    assert.ok(creds);
    const again = await migrateLegacyCredentialsIfNeeded();
    assert.equal(again, false);
  } finally {
    if (prevAgent === undefined) delete process.env.TASKAGENT_AGENT_DIR;
    else process.env.TASKAGENT_AGENT_DIR = prevAgent;
    if (prevHome === undefined) delete process.env.HOME;
    else process.env.HOME = prevHome;
    if (prevXdg === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = prevXdg;
    await rm(root, { recursive: true, force: true });
  }
});
