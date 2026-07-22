---
name: daruma-mode
description: Show or set the intake strictness mode (off | lite | full).
---

# /daruma-mode

Show or set how aggressively raw input gets decomposed into a plan via
`daruma_plan_materialize` (plan-only intake, ADR-0007) before becoming a
task.

## Steps

1. Run `daruma-cursor mode $ARGUMENTS`.
   - No argument → prints the current mode.
   - `off` | `lite` | `full` → persists the new mode and confirms it.
   - Anything else → the CLI rejects it with the valid choices; report
     the error, do not guess.
2. The mode is persisted to `~/.daruma/mode`, shared across every daruma
   client (Claude, Cursor, ...) — setting it here also applies there.
3. Levels:
   - **off** — direct daruma work; never force decomposition into a plan.
   - **lite** (default) — decompose into a plan only on explicit request
     or for work that is obviously multi-step.
   - **full** — assess every substantive request for "rawness": raw
     ideas get materialized into a plan first, concrete bounded tasks
     go to daruma directly.
4. Read-only with no argument; a mode change with an argument is the
   only write this command performs.
