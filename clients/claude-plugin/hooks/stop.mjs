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

const activeTask = process.env.DARUMA_ACTIVE_TASK ?? "";

if (!activeTask) {
  // No active task tracked in this session — skip quietly.
  process.exit(0);
}

// Emit the auto-record nudge. Claude Code will re-inject this as a
// system-reminder via the rewakeMessage path.
process.stdout.write(
  `[daruma-claude/auto-record] Active task: ${activeTask}\n` +
  `If this session produced a concrete reusable lesson (command, invariant, bug pattern, file path), ` +
  `capture it now via /daruma-claude:capture or call daruma_comment directly:\n` +
  `  daruma_comment task_id="${activeTask}" body="lesson: <short durable lesson>"\n` +
  `Skip if there is nothing durable to record.\n`
);
process.exit(0);
