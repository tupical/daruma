import { test } from "node:test";
import assert from "node:assert/strict";
import { promptSubmitHint } from "../hooks/user-prompt-submit.mjs";

function run(prompt) {
  const hint = promptSubmitHint(prompt);
  return { stdout: hint ? `${hint}\n` : "", code: 0 };
}

test("exits 0 and prints nothing for unrelated prompt", () => {
  const { stdout, code } = run("refactor the auth module");
  assert.equal(code, 0);
  assert.equal(stdout.trim(), "");
});

test("detects 'capture' keyword", () => {
  const { stdout, code } = run("capture this lesson about the build");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:capture/);
});

test("detects 'record' keyword", () => {
  const { stdout, code } = run("record what we learned today");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:capture/);
});

test("detects Russian capture keyword 'сохрани'", () => {
  const { stdout, code } = run("сохрани этот урок");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:capture/);
});

test("detects 'sync' keyword", () => {
  const { stdout, code } = run("sync tasks please");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:sync/);
});

test("detects 'status' keyword", () => {
  const { stdout, code } = run("what is the status of the project");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:status/);
});

test("detects 'progress' keyword", () => {
  const { stdout, code } = run("show me the progress");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:status/);
});

test("detects 'close' keyword", () => {
  const { stdout, code } = run("close this task");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:close/);
});

test("detects Russian 'закрой' keyword", () => {
  const { stdout, code } = run("закрой задачу");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:close/);
});

test("detects 'complete' keyword", () => {
  const { stdout, code } = run("complete the current task");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:close/);
});

test("handles empty CLAUDE_USER_PROMPT gracefully", () => {
  const { stdout, code } = run("");
  assert.equal(code, 0);
  assert.equal(stdout.trim(), "");
});

test("capture pattern takes priority over close for 'capture and record lesson'", () => {
  const { stdout, code } = run("please capture and record this lesson");
  assert.equal(code, 0);
  assert.match(stdout, /\/daruma-claude:capture/);
});
