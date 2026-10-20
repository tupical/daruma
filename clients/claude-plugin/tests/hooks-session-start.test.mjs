import { test } from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { classifyListResult, formatTask, renderSummary, sessionStartLaunch } from "../hooks/session-start.mjs";

const execFileAsync = promisify(execFile);
const HOOK = fileURLToPath(new URL("../hooks/session-start.mjs", import.meta.url));

test("default launcher is the unified `daruma mcp`, without `serve`", () => {
  assert.deepEqual(sessionStartLaunch({}), { command: "daruma", args: ["mcp"] });
});

test("DARUMA_MCP_BIN picks the binary, as for start and doctor", () => {
  assert.deepEqual(
    sessionStartLaunch({ DARUMA_MCP_BIN: " /opt/daruma/bin/daruma " }),
    { command: "/opt/daruma/bin/daruma", args: ["mcp"] },
  );
});

test("DARUMA_MCP_CMD / DARUMA_MCP_ARGS override verbatim; blank ARGS keeps the default", () => {
  assert.deepEqual(
    sessionStartLaunch({ DARUMA_MCP_CMD: "npx", DARUMA_MCP_ARGS: " daruma  mcp --profile full " }),
    { command: "npx", args: ["daruma", "mcp", "--profile", "full"] },
  );
  assert.deepEqual(sessionStartLaunch({ DARUMA_MCP_ARGS: "   " }).args, ["mcp"]);
});

test("priority shows once whether the server sends \"p1\" or 1", () => {
  assert.match(formatTask({ title: "a", status: "todo", priority: "p1" }), /\[p1\] a$/);
  assert.match(formatTask({ title: "b", status: "todo", priority: 2 }), /\[p2\] b$/);
  assert.match(formatTask({ title: "c", status: "todo" }), /\[  \] c$/);
});

test("only a real empty list reads as \"no open tasks\"", () => {
  const empty = classifyListResult({ isError: false, text: '{"items":[]}', parsed: { items: [] } });
  assert.deepEqual(empty, { kind: "tasks", tasks: [] });
  assert.match(renderSummary({ list: empty }), /No open tasks/);
});

test("a tool error shows its cause instead of \"no open tasks\"", () => {
  const list = classifyListResult({ isError: true, text: "401 Unauthorized: token expired", parsed: null });
  assert.deepEqual(list, { kind: "error", message: "401 Unauthorized: token expired" });
  const out = renderSummary({ list });
  assert.doesNotMatch(out, /No open tasks/);
  assert.match(out, /Could not list open tasks: 401 Unauthorized: token expired/);
});

test("an unselected project lists the choices instead of \"no open tasks\"", () => {
  const parsed = {
    needs_project_selection: true,
    reason: "No default Daruma project is resolved for this MCP workspace.",
    projects: [{ id: "prj_a", title: "Alpha" }, { id: "prj_b", title: null, slug: "beta" }],
  };
  const list = classifyListResult({ isError: false, text: JSON.stringify(parsed), parsed });
  assert.equal(list.kind, "needs_project");
  const out = renderSummary({ list, scopePath: "/home/you/projects" });
  assert.doesNotMatch(out, /No open tasks/);
  assert.match(out, /No project is selected/);
  assert.match(out, /Alpha \(prj_a\)/);
  assert.match(out, /beta \(prj_b\)/);
  assert.match(out, /daruma_project_use \{"project_id": "<id>", "scope_path": "<absolute repo root>"\}/);
  assert.match(out, /Session cwd: \/home\/you\/projects/);
  assert.doesNotMatch(renderSummary({ list }), /Session cwd/);
});

test("an unrecognised payload is not taken for an empty list", () => {
  for (const result of [
    { isError: false, text: "", parsed: null },
    { isError: false, text: "rate limited", parsed: null },
    { isError: false, text: '{"ok":true}', parsed: { ok: true } },
  ]) {
    const list = classifyListResult(result);
    assert.equal(list.kind, "error", JSON.stringify(result));
    assert.doesNotMatch(renderSummary({ list }), /No open tasks/);
  }
});

// A stand-in for the unified binary: like clap, it rejects any argv other than
// `mcp`, then answers initialize, daruma_workspace_info and daruma_list.
const FAKE_DARUMA = `#!/usr/bin/env node
const fs = require("node:fs");
fs.writeFileSync(process.env.FAKE_ARGV_OUT, JSON.stringify(process.argv.slice(2)));
if (process.argv.slice(2).join(" ") !== "mcp") {
  process.stderr.write("error: unexpected argument '" + process.argv[3] + "' found\\n");
  process.exit(2);
}
const tools = {
  daruma_workspace_info: { default_project: "prj-1", project_title: "Demo" },
  daruma_list: { items: [{ title: "Fix the session hook", status: "todo", priority: "p2" }] },
};
// FAKE_LIST_RESULT replaces the whole daruma_list tool result (e.g. isError).
const listResult = process.env.FAKE_LIST_RESULT ? JSON.parse(process.env.FAKE_LIST_RESULT) : null;
let buf = "";
process.stdin.on("data", (chunk) => {
  buf += chunk;
  let nl;
  while ((nl = buf.indexOf("\\n")) !== -1) {
    const msg = JSON.parse(buf.slice(0, nl));
    buf = buf.slice(nl + 1);
    if (msg.id === undefined) continue;
    const result = msg.method === "initialize"
      ? { protocolVersion: "2024-11-05", capabilities: {}, serverInfo: { name: "fake-daruma", version: "0" } }
      : msg.params.name === "daruma_list" && listResult
        ? listResult
        : { content: [{ type: "text", text: JSON.stringify(tools[msg.params.name] ?? {}) }] };
    process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result }) + "\\n");
  }
});
`;

async function runHook(extraEnv = {}) {
  const dir = await mkdtemp(join(tmpdir(), "daruma-session-start-"));
  try {
    const bin = join(dir, "daruma");
    const argvOut = join(dir, "argv.json");
    await writeFile(bin, FAKE_DARUMA);
    await chmod(bin, 0o755);
    const env = { ...process.env, DARUMA_MCP_BIN: bin, FAKE_ARGV_OUT: argvOut, ...extraEnv };
    for (const key of ["DARUMA_MCP_CMD", "DARUMA_MCP_ARGS", "DARUMA_SCOPE"]) delete env[key];
    if (!extraEnv.FAKE_LIST_RESULT) delete env.FAKE_LIST_RESULT;

    const { stdout } = await execFileAsync(process.execPath, [HOOK], { cwd: dir, env, timeout: 20_000 });
    return { stdout, argv: JSON.parse(await readFile(argvOut, "utf8")) };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

test("hook prints the open-task summary through `daruma mcp`", { skip: process.platform === "win32" }, async () => {
  const { stdout, argv } = await runHook();
  assert.deepEqual(argv, ["mcp"]);
  assert.match(stdout, /\[daruma\] 1 open task in "Demo":/);
  assert.match(stdout, /\[p2\] Fix the session hook/);
});

test("hook reports a daruma_list tool error, not \"no open tasks\"", { skip: process.platform === "win32" }, async () => {
  const { stdout } = await runHook({
    FAKE_LIST_RESULT: JSON.stringify({
      content: [{ type: "text", text: "cloud API returned 401 Unauthorized" }],
      isError: true,
    }),
  });
  assert.doesNotMatch(stdout, /No open tasks/);
  assert.match(stdout, /Could not list open tasks: cloud API returned 401 Unauthorized/);
});

test("hook asks for a project when daruma_list needs one, not \"no open tasks\"", { skip: process.platform === "win32" }, async () => {
  const { stdout } = await runHook({
    FAKE_LIST_RESULT: JSON.stringify({
      content: [{
        type: "text",
        text: JSON.stringify({
          needs_project_selection: true,
          reason: "No default Daruma project is resolved for this MCP workspace.",
          projects: [{ id: "prj_demo", title: "Demo" }],
        }),
      }],
    }),
  });
  assert.doesNotMatch(stdout, /No open tasks/);
  assert.match(stdout, /No project is selected/);
  assert.match(stdout, /Demo \(prj_demo\)/);
});
