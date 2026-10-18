import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  detectDaruma,
  probeMcpAuth,
  resolveDoctorTarget,
  serverOriginFromMcpUrl,
} from "../lib/detect.mjs";

const GOOD_TOKEN = "good-token";

// Stand-in daruma server: healthz is open, /v1/mcp demands `Bearer good-token`.
async function withServer(fn) {
  const seen = [];
  const server = createServer((req, res) => {
    seen.push({ method: req.method, url: req.url, auth: req.headers.authorization ?? null });
    if (req.method === "GET" && req.url === "/v1/healthz") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ status: "ok", version: "9.9.9" }));
      return;
    }
    if (req.method === "POST" && req.url === "/v1/mcp") {
      req.resume();
      req.on("end", () => {
        if (req.headers.authorization !== `Bearer ${GOOD_TOKEN}`) {
          res.writeHead(401, { "WWW-Authenticate": "Bearer" });
          res.end();
          return;
        }
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} }));
      });
      return;
    }
    res.writeHead(404);
    res.end();
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const origin = `http://127.0.0.1:${server.address().port}`;
  try {
    return await fn(origin, seen);
  } finally {
    await new Promise((r) => server.close(r));
  }
}

// Isolated HOME/agent dir/cwd project; PATH emptied so no real `daruma` or
// `cursor` binary is probed. DARUMA_API_URL points at a dead port — the old
// doctor probed it; the fixed one must not.
async function withIsolatedEnv(fn) {
  const root = await mkdtemp(join(tmpdir(), "daruma-cursor-detect-"));
  const keys = ["HOME", "PATH", "DARUMA_AGENT_DIR", "DARUMA_API_URL", "DARUMA_BASE_URL"];
  const prev = Object.fromEntries(keys.map((k) => [k, process.env[k]]));
  const home = join(root, "home");
  const project = join(root, "project");
  await mkdir(join(home, ".cursor"), { recursive: true });
  await mkdir(join(project, ".cursor"), { recursive: true });
  process.env.HOME = home;
  process.env.PATH = join(root, "empty-bin");
  process.env.DARUMA_AGENT_DIR = join(root, "agent");
  process.env.DARUMA_API_URL = "http://127.0.0.1:9";
  delete process.env.DARUMA_BASE_URL;
  try {
    return await fn({ home, project });
  } finally {
    for (const k of keys) {
      if (prev[k] === undefined) delete process.env[k];
      else process.env[k] = prev[k];
    }
    await rm(root, { recursive: true, force: true });
  }
}

async function writeMcpJson(dir, entry) {
  await writeFile(
    join(dir, ".cursor", "mcp.json"),
    JSON.stringify({ mcpServers: { daruma: entry } }),
  );
}

test("serverOriginFromMcpUrl strips the MCP resource path and query", () => {
  assert.equal(serverOriginFromMcpUrl("https://daruma.example/v1/mcp"), "https://daruma.example");
  assert.equal(serverOriginFromMcpUrl("https://h.example/base/v1/mcp/?profile=full"), "https://h.example/base");
  assert.equal(serverOriginFromMcpUrl("http://127.0.0.1:8080/mcp"), "http://127.0.0.1:8080");
  assert.equal(serverOriginFromMcpUrl("not a url"), null);
});

test("resolveDoctorTarget uses the http entry's url and headers", async () => {
  const target = await resolveDoctorTarget({
    scope: "global",
    entry: {
      type: "http",
      url: "https://cloud.example/v1/mcp",
      headers: { authorization: "Bearer t", "X-Daruma-Workspace-Id": "ws" },
    },
  });
  assert.equal(target.source, "mcp.json (global)");
  assert.equal(target.baseUrl, "https://cloud.example");
  assert.equal(target.mcpUrl, "https://cloud.example/v1/mcp");
  assert.deepEqual(target.headers, { Authorization: "Bearer t", "X-Daruma-Workspace-Id": "ws" });
});

test("resolveDoctorTarget uses a stdio entry's DARUMA_API_URL and DARUMA_TOKEN", async () => {
  const target = await resolveDoctorTarget({
    scope: "project",
    entry: {
      type: "stdio",
      command: "daruma",
      args: ["mcp"],
      env: { DARUMA_API_URL: "https://self.example/", DARUMA_TOKEN: "s" },
    },
  });
  assert.equal(target.transport, "stdio");
  assert.equal(target.baseUrl, "https://self.example");
  assert.equal(target.mcpUrl, "https://self.example/v1/mcp");
  assert.deepEqual(target.headers, { Authorization: "Bearer s" });
});

test("resolveDoctorTarget falls back to env/credentials without a configured url", async () => {
  await withIsolatedEnv(async () => {
    const none = await resolveDoctorTarget({ entry: null });
    assert.equal(none.source, "env/credentials");
    assert.equal(none.baseUrl, "http://127.0.0.1:9");
    assert.equal(none.mcpUrl, null);
    const bareStdio = await resolveDoctorTarget({ entry: { command: "daruma", args: ["mcp"] }, scope: "global" });
    assert.equal(bareStdio.source, "env/credentials");
  });
});

test("probeMcpAuth: accepted, rejected, missing and OAuth cases", async () => {
  await withServer(async (origin) => {
    const mcpUrl = `${origin}/v1/mcp`;
    const good = await probeMcpAuth({ transport: "http", mcpUrl, headers: { Authorization: `Bearer ${GOOD_TOKEN}` } });
    assert.deepEqual(good, { ok: true, status: "token accepted" });
    const bad = await probeMcpAuth({ transport: "http", mcpUrl, headers: { Authorization: "Bearer nope" } });
    assert.equal(bad.ok, false);
    assert.equal(bad.status, "token rejected");
    assert.equal(bad.error, "HTTP 401");
    const oauth = await probeMcpAuth({ transport: "http", mcpUrl, headers: {} });
    assert.equal(oauth.ok, true);
    assert.match(oauth.status, /oauth/);
    const stdioNoToken = await probeMcpAuth({ transport: "stdio", mcpUrl, headers: {} });
    assert.equal(stdioNoToken.ok, false);
    assert.equal(stdioNoToken.status, "token missing");
  });
  assert.deepEqual(await probeMcpAuth({ mcpUrl: null }), { ok: true, status: "unchecked" });
});

test("detectDaruma probes the server and token from the selected Cursor config", async () => {
  await withServer(async (origin, seen) => {
    await withIsolatedEnv(async ({ home, project }) => {
      // Global entry points at a dead address; the project entry wins.
      await writeMcpJson(home, { type: "http", url: "http://127.0.0.1:9/v1/mcp" });
      await writeMcpJson(project, {
        type: "http",
        url: `${origin}/v1/mcp`,
        headers: { Authorization: `Bearer ${GOOD_TOKEN}` },
      });
      const report = await detectDaruma({ projectDir: project });
      assert.equal(report.http.ok, true, JSON.stringify(report.http));
      assert.equal(report.http.baseUrl, origin);
      assert.equal(report.http.source, "mcp.json (project)");
      assert.equal(report.auth.ok, true);
      assert.equal(report.auth.status, "token accepted");
      assert.equal(report.mcpReady, true);
      assert.ok(seen.some((r) => r.url === "/v1/mcp" && r.auth === `Bearer ${GOOD_TOKEN}`));
    });
  });
});

test("detectDaruma is not ready when the configured token is rejected", async () => {
  await withServer(async (origin) => {
    await withIsolatedEnv(async ({ project }) => {
      await writeMcpJson(project, {
        type: "http",
        url: `${origin}/v1/mcp`,
        headers: { Authorization: "Bearer stale" },
      });
      const report = await detectDaruma({ projectDir: project });
      assert.equal(report.http.ok, true);
      assert.equal(report.auth.status, "token rejected");
      assert.equal(report.mcpReady, false);
      assert.match(report.mcpHint, /auth: token rejected/);
    });
  });
});
