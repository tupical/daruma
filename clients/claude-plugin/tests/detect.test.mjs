// Unit tests for detect.mjs pure helpers.
// Run with `npm test` or `node --test tests/`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  parseClaudeMcpList,
  parseSemver,
  cliReadinessSummary,
} from "../lib/detect.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const fixture = (name) => readFileSync(join(__dirname, "fixtures", name), "utf8");

test("parseClaudeMcpList: taskagent connected", () => {
  const got = parseClaudeMcpList(fixture("claude-mcp-list-connected.txt"));
  assert.equal(got.present, true);
  assert.equal(got.connected, true);
  assert.match(got.command, /taskagent-mcp$/);
});

test("parseClaudeMcpList: taskagent registered but disconnected", () => {
  const got = parseClaudeMcpList(fixture("claude-mcp-list-disconnected.txt"));
  assert.equal(got.present, true);
  assert.equal(got.connected, false);
  assert.match(got.command, /taskagent-mcp$/);
});

test("parseClaudeMcpList: taskagent not registered", () => {
  const got = parseClaudeMcpList(fixture("claude-mcp-list-absent.txt"));
  assert.equal(got.present, false);
  assert.equal(got.connected, false);
  assert.equal(got.command, null);
});

test("parseClaudeMcpList: empty input returns empty entry", () => {
  const got = parseClaudeMcpList("");
  assert.deepEqual(got, { present: false, connected: false, command: null });
});

test("parseClaudeMcpList: respects custom serverName argument", () => {
  // The serverName argument lets us look up something other than taskagent
  // (used in detect to keep the parser reusable). The fixture's
  // 'other-server' line should match when we ask for it explicitly.
  const got = parseClaudeMcpList(
    fixture("claude-mcp-list-connected.txt"),
    "other-server",
  );
  assert.equal(got.present, true);
  assert.equal(got.connected, true);
});

test("parseClaudeMcpList: status with 'fail' wins over 'connected' substring", () => {
  // Guard against a regex regression that would treat 'Failed to connect'
  // as connected because it contains the substring 'connect'.
  const text = "taskagent: /bin/taskagent-mcp - ✗ Failed to connect\n";
  const got = parseClaudeMcpList(text);
  assert.equal(got.present, true);
  assert.equal(got.connected, false);
});

test("parseSemver: extracts version from arbitrary CLI output", () => {
  assert.equal(parseSemver("oh-my-claudecode version 4.13.6"), "4.13.6");
  assert.equal(parseSemver("4.13.6"), "4.13.6");
  assert.equal(parseSemver("v1.2.3-beta.4"), "1.2.3-beta.4");
  assert.equal(parseSemver(null), null);
  assert.equal(parseSemver(""), null);
  assert.equal(parseSemver("no version here"), null);
});

test("cliReadinessSummary: shape is flat and JSON-friendly when ready", () => {
  const fakeReport = {
    ready: true,
    omc: { installed: true, cli: "4.13.6", npmVersion: null, installHint: "ignored" },
    taskagent: {
      installed: true,
      mcpReady: true,
      cli: "taskagent-mcp: 0.1.0",
      http: { ok: true, baseUrl: "http://localhost:8080", status: "ok", version: "0.1.0" },
      claudeMcp: { present: true, connected: true, command: "/bin/taskagent-mcp" },
      installHint: "ignored",
      mcpHint: "ignored",
    },
  };
  const got = cliReadinessSummary(fakeReport);
  assert.equal(got.ready, true);
  assert.equal(got.omc.cli, "4.13.6");
  assert.equal(got.taskagent.mcpReady, true);
  assert.equal(got.taskagent.claudeMcp.connected, true);
  assert.equal(got.taskagent.claudeMcp.command, "/bin/taskagent-mcp");
  assert.equal(got.hints.omcInstall, null);
  assert.equal(got.hints.taskagentInstall, null);
  assert.equal(got.hints.taskagentMcp, null);

  // Round-trip through JSON to confirm no functions / circular refs leaked.
  const json = JSON.parse(JSON.stringify(got));
  assert.deepEqual(json, got);
});

test("cliReadinessSummary: hints surface first line when not ready", () => {
  const fakeReport = {
    ready: false,
    omc: { installed: true, cli: "4.13.6", npmVersion: null, installHint: "" },
    taskagent: {
      installed: true,
      mcpReady: false,
      cli: "taskagent-mcp: 0.1.0",
      http: { ok: false, baseUrl: "http://localhost:8080", error: "ECONNREFUSED" },
      claudeMcp: { present: false, connected: false, command: null },
      installHint: "Recommended (build from source — server + MCP shim):\n  git clone ...",
      mcpHint: "taskagent server or MCP shim is not ready.\nServer probe: ...",
    },
  };
  const got = cliReadinessSummary(fakeReport);
  assert.equal(got.ready, false);
  assert.equal(got.hints.omcInstall, null);
  assert.equal(
    got.hints.taskagentMcp,
    "taskagent server or MCP shim is not ready.",
  );
});

test("cliReadinessSummary: tolerates missing claudeMcp on report", () => {
  // Defensive: detectTaskagent always sets claudeMcp, but if a future
  // refactor omits it the summary should still be JSON-friendly.
  const fakeReport = {
    ready: false,
    omc: { installed: false, cli: null, npmVersion: null, installHint: "install omc" },
    taskagent: {
      installed: false, mcpReady: false, cli: null, http: null,
      installHint: "install taskagent", mcpHint: "register mcp",
    },
  };
  const got = cliReadinessSummary(fakeReport);
  assert.equal(got.taskagent.claudeMcp.present, false);
  assert.equal(got.taskagent.claudeMcp.connected, false);
  assert.equal(got.taskagent.claudeMcp.command, null);
  assert.equal(got.hints.omcInstall, "install omc");
  assert.equal(got.hints.taskagentInstall, "install taskagent");
});
