#!/usr/bin/env node
// UserPromptSubmit hook: detect capture / sync / status intent in the user's
// prompt and prepend a short routing hint so Claude routes to the right
// slash command rather than improvising.
//
// Output goes to stdout — Claude Code injects it as a <system-reminder>.
// Exit 0 always; this hook must never block a prompt from being submitted.
//
// Reads the prompt text from CLAUDE_USER_PROMPT env var (set by Claude Code).

import { pathToFileURL } from "node:url";

// Intent patterns.  Order matters — first match wins.
// Note: \b word boundaries only work for ASCII. For Cyrillic keywords we use
// a lookahead/lookbehind based on whitespace or string edges instead.
const PATTERNS = [
  {
    // Explicit capture / record lesson
    // ASCII keywords use \b; Cyrillic uses (?<![а-яёА-ЯЁ]) prefix guard.
    re: /\b(capture|record|lesson)\b|(?<![а-яёА-ЯЁ])(сохрани|запомни|урок)(?![а-яёА-ЯЁ])/,
    hint: "[daruma-claude] Detected lesson-capture intent → /daruma-claude:capture",
  },
  {
    // Sync / refresh tasks from server
    re: /\b(sync|refresh\s+tasks)\b|(?<![а-яёА-ЯЁ])(синх|обнови\s+задачи)(?![а-яёА-ЯЁ])/,
    hint: "[daruma-claude] Detected sync intent → /daruma-claude:sync",
  },
  {
    // Status / progress check
    re: /\b(status|progress|what.?s\s+(open|next|left))\b|(?<![а-яёА-ЯЁ])(статус|прогресс|что\s+(открыто|осталось|дальше))(?![а-яёА-ЯЁ])/,
    hint: "[daruma-claude] Detected status query → /daruma-claude:status",
  },
  {
    // Close / complete / done
    re: /\b(close|complete|mark\s+.*done)\b|(?<![а-яёА-ЯЁ])(закрой|закрыть|завершить|пометь\s+.*выполненной)(?![а-яёА-ЯЁ])/,
    hint: "[daruma-claude] Detected close intent → /daruma-claude:close",
  },
];

export function promptSubmitHint(promptText = "") {
  const prompt = promptText.toLowerCase().trim();
  for (const { re, hint } of PATTERNS) {
    if (re.test(prompt)) return hint;
  }
  return "";
}

function main() {
  const hint = promptSubmitHint(process.env.CLAUDE_USER_PROMPT ?? "");
  if (hint) process.stdout.write(hint + "\n");
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main();
}
