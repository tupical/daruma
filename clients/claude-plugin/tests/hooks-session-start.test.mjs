import { test } from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { formatTask, sessionStartLaunch } from "../hooks/session-start.mjs";

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
      : { content: [{ type: "text", text: JSON.stringify(tools[msg.params.name] ?? {}) }] };
    process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result }) + "\\n");
  }
});
`;

test("hook prints the open-task summary through `daruma mcp`", { skip: process.platform === "win32" }, async () => {
  const dir = await mkdtemp(join(tmpdir(), "daruma-session-start-"));
  try {
    const bin = join(dir, "daruma");
    const argvOut = join(dir, "argv.json");
    await writeFile(bin, FAKE_DARUMA);
    await chmod(bin, 0o755);
    const env = { ...process.env, DARUMA_MCP_BIN: bin, FAKE_ARGV_OUT: argvOut };
    for (const key of ["DARUMA_MCP_CMD", "DARUMA_MCP_ARGS", "DARUMA_SCOPE"]) delete env[key];

    const { stdout } = await execFileAsync(process.execPath, [HOOK], { cwd: dir, env, timeout: 20_000 });

    assert.deepEqual(JSON.parse(await readFile(argvOut, "utf8")), ["mcp"]);
    assert.match(stdout, /\[daruma\] 1 open task in "Demo":/);
    assert.match(stdout, /\[p2\] Fix the session hook/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
