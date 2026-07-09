import { test } from "node:test";
import assert from "node:assert/strict";
import { stopHookMessage } from "../hooks/stop.mjs";

function run(env = {}) {
  return { stdout: stopHookMessage(env.DARUMA_ACTIVE_TASK ?? ""), code: 0 };
}

test("exits 0 and prints nothing when DARUMA_ACTIVE_TASK is absent", () => {
  const { stdout, code } = run({ DARUMA_ACTIVE_TASK: "" });
  assert.equal(code, 0);
  assert.equal(stdout.trim(), "");
});

test("exits 0 and prints nothing when env var is unset", () => {
  const { stdout, code } = run({ DARUMA_ACTIVE_TASK: undefined });
  assert.equal(code, 0);
  assert.equal(stdout.trim(), "");
});

test("prints lesson nudge when DARUMA_ACTIVE_TASK is set", () => {
  const { stdout, code } = run({ DARUMA_ACTIVE_TASK: "abc-123-task-id" });
  assert.equal(code, 0);
  assert.match(stdout, /auto-record/);
  assert.match(stdout, /abc-123-task-id/);
  assert.match(stdout, /lesson:/);
  assert.match(stdout, /daruma_comment/);
});

test("output includes capture command hint", () => {
  const { stdout } = run({ DARUMA_ACTIVE_TASK: "task-xyz" });
  assert.match(stdout, /\/daruma-claude:capture/);
});
