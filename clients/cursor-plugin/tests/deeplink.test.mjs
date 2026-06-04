import { test } from "node:test";
import assert from "node:assert/strict";

import {
  buildCursorDeeplink,
  buildHttpsInstallUrl,
  buildTaskagentInstallLinks,
  decodeConfig,
  defaultTaskagentConfig,
  defaultTaskagentConfigSync,
  encodeConfig,
  DEFAULT_API_URL,
} from "../lib/deeplink.mjs";

test("encodeConfig / decodeConfig round-trip", () => {
  const cfg = { type: "stdio", command: "taskagent-mcp", env: { X: "1" } };
  const b64 = encodeConfig(cfg);
  assert.equal(typeof b64, "string");
  assert.deepEqual(decodeConfig(b64), cfg);
});

test("encodeConfig rejects non-objects", () => {
  assert.throws(() => encodeConfig(null), TypeError);
  assert.throws(() => encodeConfig("x"), TypeError);
});

test("buildCursorDeeplink uses the official anysphere scheme", () => {
  const url = buildCursorDeeplink("taskagent", { command: "x" });
  assert.match(url, /^cursor:\/\/anysphere\.cursor-deeplink\/mcp\/install\?/);
  assert.match(url, /name=taskagent/);
  assert.match(url, /config=/);
});

test("buildCursorDeeplink rejects bad names", () => {
  assert.throws(() => buildCursorDeeplink("", { command: "x" }), TypeError);
  assert.throws(() => buildCursorDeeplink("bad name!", { command: "x" }), RangeError);
});

test("buildHttpsInstallUrl uses cursor.com mirror", () => {
  const url = buildHttpsInstallUrl("taskagent", { command: "x" });
  assert.match(url, /^https:\/\/cursor\.com\/install-mcp\?/);
});

test("defaultTaskagentConfigSync produces TASKAGENT_API_URL", () => {
  const cfg = defaultTaskagentConfigSync();
  assert.equal(cfg.type, "stdio");
  assert.equal(cfg.command, "taskagent-mcp");
  assert.equal(cfg.env.TASKAGENT_API_URL, "http://localhost:8080");
  assert.equal(cfg.env.TASKAGENT_TOKEN, undefined);
});

test("defaultTaskagentConfigSync honours overrides", () => {
  const cfg = defaultTaskagentConfigSync({
    command: "/usr/local/bin/taskagent-mcp",
    apiUrl: "https://taskagent.example",
    token: "t0p",
    workspaceId: "ws-1",
  });
  assert.equal(cfg.command, "/usr/local/bin/taskagent-mcp");
  assert.equal(cfg.env.TASKAGENT_API_URL, "https://taskagent.example");
  assert.equal(cfg.env.TASKAGENT_TOKEN, "t0p");
  assert.equal(cfg.env.TASKAGENT_WORKSPACE_ID, "ws-1");
});

test("defaultTaskagentConfig uses remote prod preset", async () => {
  const cfg = await defaultTaskagentConfig({ remote: "prod" });
  assert.equal(cfg.env.TASKAGENT_API_URL, DEFAULT_API_URL);
});

test("buildTaskagentInstallLinks returns deeplink + https mirror", async () => {
  const links = await buildTaskagentInstallLinks({ remote: "prod" });
  assert.equal(links.name, "taskagent");
  assert.match(links.deeplink, /^cursor:\/\/anysphere\.cursor-deeplink\/mcp\/install/);
  assert.match(links.httpsUrl, /^https:\/\/cursor\.com\/install-mcp/);
  const decoded = decodeConfig(
    new URL(links.httpsUrl).searchParams.get("config"),
  );
  assert.deepEqual(decoded, links.config);
  assert.equal(links.config.env.TASKAGENT_API_URL, DEFAULT_API_URL);
});
