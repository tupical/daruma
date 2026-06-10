#!/usr/bin/env node
// UserPromptSubmit hook: detect capture / sync / status intent in the user's
// prompt and prepend a short routing hint so Claude routes to the right
// slash command rather than improvising.
//
// Output goes to stdout — Claude Code injects it as a <system-reminder>.
// Exit 0 always; this hook must never block a prompt from being submitted.
//
// Reads the prompt text from CLAUDE_USER_PROMPT env var (set by Claude Code).

const prompt = (process.env.CLAUDE_USER_PROMPT ?? "").toLowerCase().trim();

// Intent patterns.  Order matters — first match wins.
// Note: \b word boundaries only work for ASCII. For Cyrillic keywords we use
// a lookahead/lookbehind based on whitespace or string edges instead.
const PATTERNS = [
  {
    // Explicit capture / record lesson
    // ASCII keywords use \b; Cyrillic uses (?<![а-яёА-ЯЁ]) prefix guard.
    re: /\b(capture|record|lesson)\b|(?<![а-яёА-ЯЁ])(сохрани|запомни|урок)(?![а-яёА-ЯЁ])/,
    hint: "[taskagent-claude] Detected lesson-capture intent → /taskagent-claude:capture",
  },
  {
    // Sync / refresh tasks from server
    re: /\b(sync|refresh\s+tasks)\b|(?<![а-яёА-ЯЁ])(синх|обнови\s+задачи)(?![а-яёА-ЯЁ])/,
    hint: "[taskagent-claude] Detected sync intent → /taskagent-claude:sync",
  },
  {
    // Status / progress check
    re: /\b(status|progress|what.?s\s+(open|next|left))\b|(?<![а-яёА-ЯЁ])(статус|прогресс|что\s+(открыто|осталось|дальше))(?![а-яёА-ЯЁ])/,
    hint: "[taskagent-claude] Detected status query → /taskagent-claude:status",
  },
  {
    // Close / complete / done
    re: /\b(close|complete|mark\s+.*done)\b|(?<![а-яёА-ЯЁ])(закрой|закрыть|завершить|пометь\s+.*выполненной)(?![а-яёА-ЯЁ])/,
    hint: "[taskagent-claude] Detected close intent → /taskagent-claude:close",
  },
];

for (const { re, hint } of PATTERNS) {
  if (re.test(prompt)) {
    process.stdout.write(hint + "\n");
    process.exit(0);
  }
}

// No match — exit cleanly with no output.
process.exit(0);
