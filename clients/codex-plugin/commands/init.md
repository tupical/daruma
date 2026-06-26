---
description: Install the managed Daruma policy block into this project's AGENTS.md.
---

The user invoked `/daruma:init`. Run the Codex plugin initializer so
agents default to daruma for tasks and plans.

## Steps

1. From the project root, run:

   ```
   if [ -n "$CODEX_PLUGIN_ROOT" ]; then node "$CODEX_PLUGIN_ROOT/bin/daruma-codex.mjs" init; else daruma-codex init; fi
   ```

2. Show the CLI output verbatim (`installed` / `updated` / `appended` /
   `unchanged`).

3. Tell the user the managed block lives in `AGENTS.md` between
   `daruma-codex:policy:begin/end` markers. Re-run this command after
   plugin updates to refresh the block.

4. Do not hand-edit the managed block — use `daruma-codex uninit` to
   remove it if needed.
