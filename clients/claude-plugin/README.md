<p align="right">
  <strong>English</strong> | <a href="./README.ru.md">RU</a>
</p>

<p align="center">
  <br/>
  ◯ ─────────── ◯
  <br/><br/>
  <strong>taskagent-claude</strong>
  <br/>
  <sub>tupical/taskagent × oh-my-claudecode</sub>
  <br/><br/>
  ◯ ─────────── ◯
  <br/>
</p>

<p align="center">
  <strong>Glue, not fork.</strong>
  <br/>
  <sub>One shell command drives the <code>tupical/taskagent</code> pipeline (parse → decompose → plan → execute) with <code>omc team</code> as the parallel-agent executor for each task.</sub>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/taskagent-claude"><img src="https://img.shields.io/npm/v/taskagent-claude?color=blue" alt="npm"></a>
  <a href="https://www.npmjs.com/package/taskagent-claude"><img src="https://img.shields.io/npm/dm/taskagent-claude" alt="downloads"></a>
  <img src="https://img.shields.io/node/v/taskagent-claude" alt="node">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-green" alt="license"></a>
</p>

<p align="center">
  <a href="#quick-start">Quick Start</a> ·
  <a href="#why-taskagent-claude">Why</a> ·
  <a href="#how-it-works">How It Works</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#the-caveat">Caveat</a> ·
  <a href="#limitations">Limitations</a>
</p>

---

> ⚠️ **Disclaimer.** This is 100% AI-slop. Use at your own risk.

---

**One shell command drives the full `tupical/taskagent` pipeline — parse a task, optionally AI-decompose it into a plan, then execute every eligible task with parallel oh-my-claudecode `/team` agents. No upstream forks, no glue prompts, no copy-paste between sessions.**

`taskagent-claude` is a thin Claude Code plugin plus an npm CLI that composes two existing projects:

- [**tupical/taskagent**](https://github.com/tupical/taskagent) — owns **projects / tasks / plans / AI decomposition** (the MCP-driven workflow store).
- [**oh-my-claudecode**](https://github.com/Yeachan-Heo/oh-my-claudecode) — owns **task execution**, replaced with `omc team` so each task runs on parallel specialized agents instead of one sequential pass.

`taskagent-claude` adds **nothing of its own**. It detects both, points the user at the official install commands when they're missing, and orchestrates them.

---

## Quick Start

> **On Windows: run from WSL.** `omc team` relies on a Unix-y tmux + bash environment. On Windows-native PowerShell + Git Bash tmux workers spawn but their I/O bleeds into the leader pane and the session dies when tmux exits. From WSL it works as designed.

```bash
# 1. taskagent — build from source (Rust workspace)
git clone https://github.com/tupical/taskagent.git
cd taskagent
cargo build --release -p taskagent-server -p taskagent-mcp-bin

# 2. start the HTTP server (keep this running)
./target/release/taskagent-server

# 3. register the MCP stdio shim with Claude Code
claude mcp add taskagent -- /abs/path/taskagent/target/release/taskagent-mcp

# 4. oh-my-claudecode (the `omc team` executor)
npm i -g oh-my-claude-sisyphus@latest
omc setup
# enable native teams in ~/.claude/settings.json:
#   { "env": { "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS": "1" } }

# 5. taskagent-claude (the glue + CLI)
npm i -g taskagent-claude

# 6. verify
taskagent-claude doctor          # should print READY

# 7. go
taskagent-claude start "refactor the auth module to use OAuth2 with PKCE"
```

That's the whole workflow. Inside Claude Code, the equivalent slash commands are `/taskagent-claude:start <task>`, `/taskagent-claude:doctor`, `/taskagent-claude:setup`.

> **Requirements.** Node.js ≥ 20, Rust toolchain (for taskagent build), Claude Code on `PATH`.

---

## Why taskagent-claude

|                                          | Without `taskagent-claude`                                  | With `taskagent-claude`                                                            |
| ---------------------------------------- | ----------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| **Driving taskagent**                    | Hand-call MCP tools (`workspace_info`, `create`, `plan_*`)  | One `taskagent-claude start "<task>"` walks the whole pipeline                     |
| **Decomposition**                        | Optional, opt-in via `--plan` flag                          | `taskagent_ai_decompose` + `plan_create` + `plan_add_task` in one go               |
| **Execute step**                         | One sequential agent per task                               | Each task runs on **N parallel agents** via `omc team`                             |
| **Setup**                                | Three installs + manual orchestration                       | One `taskagent-claude start "<task>"`                                              |

---

## How it works

```
┌──────────────────────────────┐
│ taskagent-claude start <T>   │ shell
└──────────────┬───────────────┘
               │ spawn taskagent-mcp (stdio JSON-RPC)
               ▼
┌──────────────────────────────────────────────────┐
│ 1. parse        → derive {title, description}    │
│ 2. project      → workspace_info / project_list  │
│ 3. seed         → taskagent_create(root task)    │
│ 4. [--plan]     → taskagent_ai_decompose         │
│                   + plan_create + plan_add_task  │
│ 5. execute loop                                  │
│      a. plan_next_task (or just the root)        │
│      b. omc team N:claude "<title>\n<desc>"      │
│      c. taskagent_comment(artifact)              │
│      d. complete / retry up to --max-retries     │
│ 6. report      → plan_get progress + summaries   │
└──────────────────────────────────────────────────┘
```

`taskagent-claude` never opens a nested Claude Code session at the orchestrator level — `omc team` workers are the only Claude Code panes.

---

## Commands

| Shell                                              | Effect                                                                |
| -------------------------------------------------- | --------------------------------------------------------------------- |
| `taskagent-claude start "<task>"`                  | Full pipeline (parse → project → seed → [plan] → execute → report)    |
| `taskagent-claude doctor`                          | Detect both deps + MCP tool / `omc team` readiness                    |
| `taskagent-claude setup`                           | Print install hints for missing dependencies                          |
| `taskagent-claude update`                          | Self-update + omc update; print upgrade hint for taskagent            |
| `taskagent-claude platform`                        | Print execution mode (`omc-team` or `task-fallback`)                  |
| `taskagent-claude --version` / `--help`            |                                                                       |

Inside a Claude Code REPL session:

| Slash                                  | Effect                              |
| -------------------------------------- | ----------------------------------- |
| `/taskagent-claude:start <task>`       | Same as `taskagent-claude start`    |
| `/taskagent-claude:doctor`             | Same as `taskagent-claude doctor`   |
| `/taskagent-claude:setup`              | Same as `taskagent-claude setup`    |
| `/taskagent-claude:branch-tasks`       | Show tasks linked to the current git branch |

Bundled skills:

| Skill | Effect |
| ----- | ------ |
| `branch-tasks` | Find tasks linked to the current git branch through `branch:` comments. |
| `lesson-capture` | Save a durable reusable lesson as a `lesson:` task comment. |
| `lesson-recall` | Search captured lessons through `taskagent_lesson_recall`. |

---

## Start flags

| Flag                       | Effect                                                                                                                |
| -------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `--workers N`              | Parallel agents per `omc team` invocation. Integer 1-20. Default `3`.                                                 |
| `--max-retries M`          | Retries after the first attempt for each task (total attempts = `M + 1`). Default `2`.                                |
| `--agent claude\|codex\|gemini` | Agent type used for `omc team` workers. Default `claude`.                                                        |
| `--plan`                   | AI-decompose the root task into subtasks via `taskagent_ai_decompose`, then execute each subtask. See [Caveat](#the-caveat). |
| `--project ID`             | Override default project resolution (workspace info / cwd basename).                                                  |
| `--yes` / `-y`             | Skip y/n confirmations (implied when stdin is not a TTY).                                                             |

---

## The caveat

AI decomposition (`--plan`) requires `OPENAI_API_KEY` set on the **taskagent server**. Without it, `taskagent_ai_decompose` returns `502 ai_unavailable` and `taskagent-claude` silently falls back to single-task execution on the root task. To use real decomposition, export the key before starting the server:

```bash
OPENAI_API_KEY=sk-... ./target/release/taskagent-server
```

---

## Limitations

- Single fixed role: `--agent` selects one role for all workers; mixed roles (`1:planner + 2:executor + 1:verifier`) are TODO.
- Artifact capture from `omc team` relies on text summaries written as taskagent comments — not yet structured.
- No `taskagent-claude cancel`. Use the `cancelomc` keyword or interrupt the shell.
- `taskagent-claude doctor` only probes the shell it's run from. On a Windows host run it from inside WSL.
- Plan-mode retries reset task status to `todo` and re-execute — they do **not** mutate the plan (no re-decomposition on repeated failure in v1).

---

## Project layout

```text
.
├── .claude-plugin/plugin.json          # Claude Code plugin manifest
├── package.json                        # npm package + `taskagent-claude` bin
├── bin/taskagent-claude.mjs            # CLI entry point
├── lib/
│   ├── detect.mjs                      # cross-platform dependency detection
│   ├── orchestrator.mjs                # taskagent pipeline driver
│   ├── mcp-client.mjs                  # stdio JSON-RPC client for taskagent-mcp
│   ├── omc-team-runner.mjs             # spawns `omc team` per task
│   └── update.mjs                      # self-update via npm registry
├── commands/                           # /taskagent-claude:{start,doctor,setup}
└── skills/                             # the actual contracts
    ├── start/SKILL.md                  # parse → project → seed → [plan] → execute
    ├── doctor/SKILL.md                 # readiness contract
    ├── setup/SKILL.md                  # install-hint contract
    ├── branch-tasks/SKILL.md           # find tasks by git branch
    ├── lesson-capture/SKILL.md         # save durable lesson comments
    └── lesson-recall/SKILL.md          # recall lesson comments
```

---

## Updates

```bash
taskagent-claude update                                  # taskagent-claude + omc
cd /path/to/taskagent && git pull \
  && cargo build --release -p taskagent-server -p taskagent-mcp-bin   # taskagent
npm i -g oh-my-claude-sisyphus@latest                    # oh-my-claudecode (manual)
```

---

## Contributing

Issues and PRs welcome. The whole point of this plugin is that it stays thin — patches that grow it into its own thing (extra reasoning steps, hardcoded heuristics, new agents) are likely to be declined. Patches that make the glue more robust (better detection, clearer error messages, cross-platform fixes) are very welcome.

---

## Releasing

Releases are automated via [GitHub Actions](.github/workflows/publish.yml). To cut a new version:

```bash
npm run release:patch   # 0.1.0 → 0.1.1
# or release:minor / release:major
```

This bumps `package.json`, creates a `vX.Y.Z` git tag, and pushes both. The workflow then:

1. Verifies the tag matches `package.json`.
2. Publishes to npm with `--provenance` (signed attestation).
3. Creates a GitHub Release with auto-generated notes.

Auth uses npm **Trusted Publishing** (OIDC). One-time setup: on npmjs.com → `taskagent-claude` → Settings → Trusted Publishers → add GitHub Actions with org=`tupical`, repo=`taskagent-claude`, workflow=`publish.yml`. No secrets are stored in GitHub.

## License

MIT — see [LICENSE](./LICENSE).

This project is unaffiliated with the upstream projects. Full credit to [tupical/taskagent](https://github.com/tupical/taskagent) and [Yeachan-Heo/oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode).
