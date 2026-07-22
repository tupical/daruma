---
description: Show or set the daruma intake strictness mode.
---

The user invoked `/daruma-codex:mode $ARGUMENTS`.

## Steps

1. Run `daruma-codex mode $ARGUMENTS`.
   - Empty `$ARGUMENTS` → shows the current mode.
   - `$ARGUMENTS` is one of `off`, `lite`, `full` → sets it.
2. Report the result, then explain the three levels:

   ```
   off  — work directly, no forced planning.
   lite — decompose into a plan (daruma_plan_materialize) on explicit
          request or for obviously multi-step work. (default)
   full — assess every substantive input for "rawness": a raw idea or
          undetermined direction gets materialized into a plan first;
          a concrete bounded task is worked directly.
   ```

3. Note that the mode persists in `~/.daruma/mode`, shared across all
   daruma clients (Claude, Codex, Cursor, …) — not per-project, not
   per-session.
