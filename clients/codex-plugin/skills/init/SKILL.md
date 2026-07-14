---
name: init
description: Add or refresh the managed Daruma policy block in the current project's AGENTS.md. Invoke explicitly with $daruma:init.
---

# Daruma project init

From the project root, run:

```bash
if [ -n "$CODEX_PLUGIN_ROOT" ]; then node "$CODEX_PLUGIN_ROOT/bin/daruma-codex.mjs" init; else node "$HOME/plugins/daruma/bin/daruma-codex.mjs" init; fi
```

Show the command output. The managed block lives in `AGENTS.md` between the
`daruma-codex:policy:begin/end` markers and must not be edited by hand.
