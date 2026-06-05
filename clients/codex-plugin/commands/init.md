---
description: Install the managed TaskAgent policy block into this project's AGENTS.md.
---

The user invoked `/taskagent:init`. Run the Codex plugin initializer so
agents default to taskagent for tasks and plans.

## Steps

1. From the project root, run:

   ```
   if [ -n "$CODEX_PLUGIN_ROOT" ]; then node "$CODEX_PLUGIN_ROOT/bin/taskagent-codex.mjs" init; else taskagent-codex init; fi
   ```

2. Show the CLI output verbatim (`installed` / `updated` / `appended` /
   `unchanged`).

3. Tell the user the managed block lives in `AGENTS.md` between
   `taskagent-codex:policy:begin/end` markers. Re-run this command after
   plugin updates to refresh the block.

4. Do not hand-edit the managed block — use `taskagent-codex uninit` to
   remove it if needed.
