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
  resolveProfileForInstall,
} from "../lib/agent-credentials.mjs";

async function withAgentDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "daruma-agent-test-"));
  const prev = process.env.DARUMA_AGENT_DIR;
  process.env.DARUMA_AGENT_DIR = dir;
  try {
    return await fn(dir);
  } finally {
    if (prev === undefined) delete process.env.DARUMA_AGENT_DIR;
    else process.env.DARUMA_AGENT_DIR = prev;
    await rm(dir, { recursive: true, force: true });
  }
}

test("agentDirRoot honours DARUMA_AGENT_DIR", async () => {
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
    assert.equal(env.DARUMA_API_URL, "https://remote.example");
    assert.equal(env.DARUMA_TOKEN, "ta_pat_test");
    assert.equal(env.DARUMA_WORKSPACE_ID, "ws-uuid");
  });
});

test("resolveProfileForInstall prefers cloud profile for cloud api-url", async () => {
  await withAgentDir(async () => {
    const creds = {
      schema_version: 1,
      active_profile: "selfhost-local",
      profiles: {
        "selfhost-local": {
          mode: "self-host",
          server_url: "http://127.0.0.1:8080",
          token: "ta_svc_local",
        },
        "cloud-default": {
          mode: "cloud",
          server_url: "https://cloud.example.com",
          token: "ta_pat_cloud",
          workspace_id: "ws-cloud",
        },
      },
    };
    const profile = resolveProfileForInstall(creds, {
      apiUrl: "https://cloud.example.com",
    });
    assert.equal(profile.name, "cloud-default");
    assert.equal(profile.token, "ta_pat_cloud");
    assert.equal(profile.workspace_id, "ws-cloud");
  });
});

test("resolveMcpEnvFromCredentials uses cloud profile for cloud api-url", async () => {
  await withAgentDir(async (dir) => {
    await mkdir(dir, { recursive: true });
    await writeFile(
      join(dir, "credentials.json"),
      JSON.stringify({
        schema_version: 1,
        active_profile: "selfhost-local",
        profiles: {
          "selfhost-local": {
            mode: "self-host",
            server_url: "http://127.0.0.1:8080",
            token: "ta_svc_local",
          },
          "cloud-default": {
            mode: "cloud",
            server_url: "https://cloud.example.com",
            token: "ta_pat_cloud",
            workspace_id: "ws-cloud",
          },
        },
      }),
      "utf8",
    );
    const env = await resolveMcpEnvFromCredentials({
      apiUrl: "https://cloud.example.com",
    });
    assert.equal(env.DARUMA_API_URL, "https://cloud.example.com");
    assert.equal(env.DARUMA_TOKEN, "ta_pat_cloud");
    assert.equal(env.DARUMA_WORKSPACE_ID, "ws-cloud");
  });
});

test("migrateLegacyCredentialsIfNeeded copies XDG file", async () => {
  const root = await mkdtemp(join(tmpdir(), "daruma-migrate-"));
  const legacyDir = join(root, ".config", "daruma");
  const prevAgent = process.env.DARUMA_AGENT_DIR;
  const prevHome = process.env.HOME;
  const prevXdg = process.env.XDG_CONFIG_HOME;
  process.env.HOME = root;
  delete process.env.DARUMA_AGENT_DIR;
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
    if (prevAgent === undefined) delete process.env.DARUMA_AGENT_DIR;
    else process.env.DARUMA_AGENT_DIR = prevAgent;
    if (prevHome === undefined) delete process.env.HOME;
    else process.env.HOME = prevHome;
    if (prevXdg === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = prevXdg;
    await rm(root, { recursive: true, force: true });
  }
});
