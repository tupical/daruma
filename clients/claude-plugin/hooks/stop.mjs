#!/usr/bin/env node
// Stop hook: auto-record skill trigger.
// Outputs a <system-reminder>-style nudge so Claude considers capturing a
// reusable lesson via daruma_comment at session end.
//
// This hook intentionally prints nothing when there is no active daruma
// session (DARUMA_ACTIVE_TASK env var absent), so it doesn't fire on
// every session — only when the agent was working on a tracked task.
//
// asyncRewake: true in hooks.json means Claude Code re-wakes Claude with the
// rewakeMessage only when this script exits 0 AND prints non-empty output.

import { pathToFileURL } from "node:url";

export function stopHookMessage(activeTask = "") {
  if (!activeTask) return "";
  return (
    `[daruma-claude/auto-record] Active task: ${activeTask}\n` +
    `If this session produced a concrete reusable lesson (command, invariant, bug pattern, file path), ` +
    `capture it now via /daruma-claude:capture or call daruma_comment directly:\n` +
    `  daruma_comment task_id="${activeTask}" body="lesson: <short durable lesson>"\n` +
    `Skip if there is nothing durable to record.\n`
  );
}

function main() {
  const message = stopHookMessage(process.env.DARUMA_ACTIVE_TASK ?? "");
  if (message) process.stdout.write(message);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main();
}
