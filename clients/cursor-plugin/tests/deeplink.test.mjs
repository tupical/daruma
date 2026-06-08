import { test } from "node:test";
import assert from "node:assert/strict";

import {
  buildCursorDeeplink,
  buildHttpsInstallUrl,
  buildTaskagentInstallLinks,
  decodeConfig,
  defaultTaskagentConfig,
  defaultTaskagentHttpConfig,
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

test("buildHttpsInstallUrl is a legacy alias for the official Cursor deeplink", () => {
  const url = buildHttpsInstallUrl("taskagent", { command: "x" });
  assert.match(url, /^cursor:\/\/anysphere\.cursor-deeplink\/mcp\/install\?/);
});

test("defaultTaskagentConfigSync produces hosted HTTP config by default", () => {
  const cfg = defaultTaskagentConfigSync({ apiUrl: "http://localhost:8080" });
  assert.deepEqual(cfg, {
    type: "http",
    url: "http://localhost:8080/v1/mcp",
  });
});

test("defaultTaskagentConfigSync supports explicit stdio fallback", () => {
  const cfg = defaultTaskagentConfigSync({
    transport: "stdio",
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
  assert.deepEqual(cfg, defaultTaskagentHttpConfig({ apiUrl: DEFAULT_API_URL }));
});

test("buildTaskagentInstallLinks returns the official Cursor deeplink", async () => {
  const links = await buildTaskagentInstallLinks({ remote: "prod" });
  assert.equal(links.name, "taskagent");
  assert.match(links.deeplink, /^cursor:\/\/anysphere\.cursor-deeplink\/mcp\/install/);
  assert.equal(links.httpsUrl, links.deeplink);
  const decoded = decodeConfig(
    new URL(links.httpsUrl).searchParams.get("config"),
  );
  assert.deepEqual(decoded, links.config);
  assert.deepEqual(links.config, {
    type: "http",
    url: `${DEFAULT_API_URL}/v1/mcp`,
  });
});
