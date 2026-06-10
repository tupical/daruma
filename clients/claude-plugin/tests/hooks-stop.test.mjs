import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const HOOK = join(__dirname, "..", "hooks", "stop.mjs");

function run(env = {}) {
  const r = spawnSync(process.execPath, [HOOK], {
    encoding: "utf8",
    env: { ...process.env, ...env },
  });
  return { stdout: r.stdout ?? "", stderr: r.stderr ?? "", code: r.status };
}

test("exits 0 and prints nothing when TASKAGENT_ACTIVE_TASK is absent", () => {
  const { stdout, code } = run({ TASKAGENT_ACTIVE_TASK: "" });
  assert.equal(code, 0);
  assert.equal(stdout.trim(), "");
});

test("exits 0 and prints nothing when env var is unset", () => {
  // Remove the variable entirely by filtering it out
  const env = Object.fromEntries(
    Object.entries(process.env).filter(([k]) => k !== "TASKAGENT_ACTIVE_TASK")
  );
  const r = spawnSync(process.execPath, [HOOK], { encoding: "utf8", env });
  assert.equal(r.status, 0);
  assert.equal((r.stdout ?? "").trim(), "");
});

test("prints lesson nudge when TASKAGENT_ACTIVE_TASK is set", () => {
  const { stdout, code } = run({ TASKAGENT_ACTIVE_TASK: "abc-123-task-id" });
  assert.equal(code, 0);
  assert.match(stdout, /auto-record/);
  assert.match(stdout, /abc-123-task-id/);
  assert.match(stdout, /lesson:/);
  assert.match(stdout, /taskagent_comment/);
});

test("output includes capture command hint", () => {
  const { stdout } = run({ TASKAGENT_ACTIVE_TASK: "task-xyz" });
  assert.match(stdout, /\/taskagent-claude:capture/);
});
