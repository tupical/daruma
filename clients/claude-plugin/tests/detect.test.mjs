// Unit tests for detect.mjs pure helpers.
// Run with `npm test` or `node --test tests/`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  parseClaudeMcpList,
  parseSemver,
  cliReadinessSummary,
  darumaMcpChildEnv,
  darumaMcpLaunch,
  darumaMcpReady,
  detectDaruma,
  probeDarumaLauncher,
  formatReport,
} from "../lib/detect.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const fixture = (name) => readFileSync(join(__dirname, "fixtures", name), "utf8");

test("parseClaudeMcpList: daruma connected", () => {
  const got = parseClaudeMcpList(fixture("claude-mcp-list-connected.txt"));
  assert.equal(got.present, true);
  assert.equal(got.connected, true);
  assert.match(got.command, /daruma-mcp$/);
});

test("parseClaudeMcpList: daruma registered but disconnected", () => {
  const got = parseClaudeMcpList(fixture("claude-mcp-list-disconnected.txt"));
  assert.equal(got.present, true);
  assert.equal(got.connected, false);
  assert.match(got.command, /daruma-mcp$/);
});

test("parseClaudeMcpList: daruma not registered", () => {
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
  // The serverName argument lets us look up something other than daruma
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
  const text = "daruma: /bin/daruma-mcp - ✗ Failed to connect\n";
  const got = parseClaudeMcpList(text);
  assert.equal(got.present, true);
  assert.equal(got.connected, false);
});

test("parseClaudeMcpList: heavy ✔/✘ markers of current claude mcp list", () => {
  const connected = parseClaudeMcpList("Checking MCP server health…\n\ndaruma: /usr/bin/daruma mcp - ✔ Connected\n");
  assert.deepEqual(connected, { present: true, connected: true, command: "/usr/bin/daruma mcp" });
  const failed = parseClaudeMcpList("daruma: /bin/true  - ✘ Failed to connect — CONNECTION_CLOSED: Connection closed\n");
  assert.deepEqual(failed, { present: true, connected: false, command: "/bin/true" });
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
    daruma: {
      installed: true,
      mcpReady: true,
      cli: "daruma-mcp: 0.1.0",
      http: { ok: true, baseUrl: "http://localhost:8080", status: "ok", version: "0.1.0" },
      claudeMcp: { present: true, connected: true, command: "/bin/daruma-mcp" },
      installHint: "ignored",
      mcpHint: "ignored",
    },
  };
  const got = cliReadinessSummary(fakeReport);
  assert.equal(got.ready, true);
  assert.equal(got.omc.cli, "4.13.6");
  assert.equal(got.daruma.mcpReady, true);
  assert.equal(got.daruma.claudeMcp.connected, true);
  assert.equal(got.daruma.claudeMcp.command, "/bin/daruma-mcp");
  assert.equal(got.hints.omcInstall, null);
  assert.equal(got.hints.darumaInstall, null);
  assert.equal(got.hints.darumaMcp, null);

  // Round-trip through JSON to confirm no functions / circular refs leaked.
  const json = JSON.parse(JSON.stringify(got));
  assert.deepEqual(json, got);
});

test("cliReadinessSummary: hints surface first line when not ready", () => {
  const fakeReport = {
    ready: false,
    omc: { installed: true, cli: "4.13.6", npmVersion: null, installHint: "" },
    daruma: {
      installed: true,
      mcpReady: false,
      cli: "daruma-mcp: 0.1.0",
      http: { ok: false, baseUrl: "http://localhost:8080", error: "ECONNREFUSED" },
      claudeMcp: { present: false, connected: false, command: null },
      installHint: "Recommended (build from source — server + MCP shim):\n  git clone ...",
      mcpHint: "daruma server or MCP shim is not ready.\nServer probe: ...",
    },
  };
  const got = cliReadinessSummary(fakeReport);
  assert.equal(got.ready, false);
  assert.equal(got.hints.omcInstall, null);
  assert.equal(
    got.hints.darumaMcp,
    "daruma server or MCP shim is not ready.",
  );
});

test("cliReadinessSummary: tolerates missing claudeMcp on report", () => {
  // Defensive: detectDaruma always sets claudeMcp, but if a future
  // refactor omits it the summary should still be JSON-friendly.
  const fakeReport = {
    ready: false,
    omc: { installed: false, cli: null, npmVersion: null, installHint: "install omc" },
    daruma: {
      installed: false, mcpReady: false, cli: null, http: null,
      installHint: "install daruma", mcpHint: "register mcp",
    },
  };
  const got = cliReadinessSummary(fakeReport);
  assert.equal(got.daruma.claudeMcp.present, false);
  assert.equal(got.daruma.claudeMcp.connected, false);
  assert.equal(got.daruma.claudeMcp.command, null);
  assert.equal(got.hints.omcInstall, "install omc");
  assert.equal(got.hints.darumaInstall, "install daruma");
});

test("darumaMcpLaunch: start spawns the unified binary as `daruma mcp`", () => {
  assert.deepEqual(darumaMcpLaunch({}), { command: "daruma", args: ["mcp"] });
  assert.deepEqual(
    darumaMcpLaunch({ DARUMA_MCP_BIN: " /opt/daruma/bin/daruma " }),
    { command: "/opt/daruma/bin/daruma", args: ["mcp"] },
  );
  assert.deepEqual(darumaMcpLaunch({ DARUMA_MCP_BIN: "  " }).command, "daruma");
});

test("darumaMcpReady: connected Claude entry is not READY without a runnable launcher", () => {
  const claudeMcp = { present: true, connected: true, command: "daruma mcp" };
  const http = { ok: true };
  assert.equal(darumaMcpReady({ launcher: { ok: false }, claudeMcp, http }), false);
  assert.equal(darumaMcpReady({ launcher: { ok: true }, claudeMcp, http }), true);
  assert.equal(
    darumaMcpReady({ launcher: { ok: true }, claudeMcp: { connected: false }, http }),
    false,
  );
  assert.equal(darumaMcpReady({ launcher: { ok: true }, claudeMcp, http: { ok: false } }), false);
});

async function withTempDir(fn) {
  const dir = await mkdtemp(join(tmpdir(), "daruma-detect-test-"));
  try {
    return await fn(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

test("probeDarumaLauncher: runs --version on the exact command start spawns", { skip: process.platform === "win32" }, async () => {
  await withTempDir(async (dir) => {
    const bin = join(dir, "daruma");
    await writeFile(bin, "#!/bin/sh\n[ \"$1\" = --version ] && echo 'daruma 0.2.0'\n");
    await chmod(bin, 0o755);
    const got = await probeDarumaLauncher({ DARUMA_MCP_BIN: bin });
    assert.deepEqual(got, { ok: true, command: bin, args: ["mcp"], version: "daruma 0.2.0" });
  });
});

test("probeDarumaLauncher: missing launcher fails the probe and the verdict", async () => {
  const missing = join(tmpdir(), "daruma-detect-missing", "daruma");
  const launcher = await probeDarumaLauncher({ DARUMA_MCP_BIN: missing });
  assert.equal(launcher.ok, false);
  assert.equal(launcher.command, missing);

  const claudeMcp = { present: true, connected: true, command: "daruma mcp" };
  const http = { ok: true, baseUrl: "http://localhost:8080", status: "ok", version: null };
  const report = {
    ready: false,
    omc: { name: "oh-my-claudecode", installed: true, cli: "4.13.6", npmVersion: null, installHint: "" },
    daruma: {
      name: "daruma",
      installed: true,
      mcpReady: darumaMcpReady({ launcher, claudeMcp, http }),
      cli: null,
      launcher,
      http,
      claudeMcp,
      installHint: "",
      mcpHint: "daruma server or MCP shim is not ready.",
    },
  };
  assert.equal(report.daruma.mcpReady, false);
  const text = formatReport(report);
  assert.match(text, new RegExp(`start launcher: ${missing} mcp → NOT RUNNABLE`));
  assert.doesNotMatch(text, /^READY$/m);
  assert.deepEqual(cliReadinessSummary(report).daruma.launcher, {
    ok: false, command: missing, args: ["mcp"],
  });
});

const DARUMA_ENV_KEYS = [
  "DARUMA_AGENT_DIR", "DARUMA_API_URL", "DARUMA_BASE_URL", "DARUMA_TOKEN",
  "DARUMA_WORKSPACE_ID", "DARUMA_MCP_BIN", "PATH",
];

// Saved remote profile in a temp agent dir; process env scrubbed of Daruma
// vars and restored afterwards.
async function withSavedProfile(fn) {
  const dir = await mkdtemp(join(tmpdir(), "daruma-detect-creds-"));
  const saved = Object.fromEntries(DARUMA_ENV_KEYS.map((k) => [k, process.env[k]]));
  try {
    for (const k of DARUMA_ENV_KEYS) if (k !== "PATH") delete process.env[k];
    process.env.DARUMA_AGENT_DIR = dir;
    await writeFile(join(dir, "credentials.json"), JSON.stringify({
      schema_version: 1,
      active_profile: "remote-default",
      profiles: {
        "remote-default": {
          mode: "remote",
          server_url: "https://saved.example",
          token: "ta_pat_saved",
          workspace_id: "019e5fc5-0000-7000-8000-00000000000a",
        },
      },
    }));
    await fn(dir);
  } finally {
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
    await rm(dir, { recursive: true, force: true });
  }
}

test("darumaMcpChildEnv: explicit DARUMA_* env outranks the saved profile", async () => {
  await withSavedProfile(async () => {
    const env = await darumaMcpChildEnv({}, {
      DARUMA_API_URL: "http://shell.example/",
      DARUMA_TOKEN: "ta_pat_shell",
      DARUMA_WORKSPACE_ID: "019e5fc5-0000-7000-8000-00000000000b",
      OTHER: "kept",
    });
    assert.equal(env.DARUMA_API_URL, "http://shell.example");
    assert.equal(env.DARUMA_TOKEN, "ta_pat_shell");
    assert.equal(env.DARUMA_WORKSPACE_ID, "019e5fc5-0000-7000-8000-00000000000b");
    assert.equal(env.OTHER, "kept");
  });
});

test("darumaMcpChildEnv: profile fills only what env leaves unset or empty", async () => {
  await withSavedProfile(async () => {
    const tokenOnly = await darumaMcpChildEnv({}, { DARUMA_TOKEN: "ta_pat_shell" });
    assert.equal(tokenOnly.DARUMA_TOKEN, "ta_pat_shell");
    assert.equal(tokenOnly.DARUMA_API_URL, "https://saved.example");

    const blank = await darumaMcpChildEnv({}, { DARUMA_TOKEN: "  ", DARUMA_API_URL: "" });
    assert.equal(blank.DARUMA_TOKEN, "ta_pat_saved");
    assert.equal(blank.DARUMA_API_URL, "https://saved.example");
    assert.equal(blank.DARUMA_WORKSPACE_ID, "019e5fc5-0000-7000-8000-00000000000a");

    const legacy = await darumaMcpChildEnv({}, { DARUMA_BASE_URL: "http://legacy.example" });
    assert.equal(legacy.DARUMA_API_URL, "http://legacy.example");
  });
});

test("detectDaruma probes the URL start hands to `daruma mcp` and reports the env token", async () => {
  await withSavedProfile(async (dir) => {
    // Closed loopback port: the probe fails fast; PATH without `claude`.
    process.env.DARUMA_API_URL = "http://127.0.0.1:9";
    process.env.DARUMA_TOKEN = "ta_pat_shell";
    process.env.DARUMA_MCP_BIN = join(dir, "missing-daruma");
    process.env.PATH = dir;
    const child = await darumaMcpChildEnv();
    const report = await detectDaruma();
    assert.equal(report.http.baseUrl, child.DARUMA_API_URL);
    assert.equal(report.http.baseUrl, "http://127.0.0.1:9");
    assert.equal(report.credentials.tokenSource, "env");
    assert.match(report.mcpHint, /DARUMA_TOKEN from env/);
    const text = formatReport({ ready: false, omc: { name: "oh-my-claudecode", installed: true }, daruma: report });
    assert.match(text, /token: DARUMA_TOKEN from env/);

    delete process.env.DARUMA_API_URL;
    delete process.env.DARUMA_TOKEN;
    const saved = await detectDaruma();
    assert.equal(saved.http.baseUrl, "https://saved.example");
    assert.equal(saved.credentials.tokenSource, "credentials");
  });
});
