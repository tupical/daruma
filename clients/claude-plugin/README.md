<p align="right">
  <strong>English</strong> | <a href="./README.ru.md">RU</a>
</p>

<p align="center">
  <br/>
  ◯ ─────────── ◯
  <br/><br/>
  <strong>daruma-claude</strong>
  <br/>
  <sub>tupical/daruma × oh-my-claudecode</sub>
  <br/><br/>
  ◯ ─────────── ◯
  <br/>
</p>

<p align="center">
  <strong>Glue, not fork.</strong>
  <br/>
  <sub>One shell command drives the <code>tupical/daruma</code> pipeline (parse → decompose → plan → execute) with <code>omc team</code> as the parallel-agent executor for each task.</sub>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/daruma-claude"><img src="https://img.shields.io/npm/v/daruma-claude?color=blue" alt="npm"></a>
  <a href="https://www.npmjs.com/package/daruma-claude"><img src="https://img.shields.io/npm/dm/daruma-claude" alt="downloads"></a>
  <img src="https://img.shields.io/node/v/daruma-claude" alt="node">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-green" alt="license"></a>
</p>

<p align="center">
  <a href="#quick-start">Quick Start</a> ·
  <a href="#why-daruma-claude">Why</a> ·
  <a href="#how-it-works">How It Works</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#the-caveat">Caveat</a> ·
  <a href="#limitations">Limitations</a>
</p>

---

> ⚠️ **Disclaimer.** This is 100% AI-slop. Use at your own risk.

---

**One shell command drives the full `tupical/daruma` pipeline — parse a task, optionally AI-decompose it into a plan, then execute every eligible task with parallel oh-my-claudecode `/team` agents. No upstream forks, no glue prompts, no copy-paste between sessions.**

`daruma-claude` is a thin Claude Code plugin plus an npm CLI that composes two existing projects:

- [**tupical/daruma**](https://github.com/tupical/daruma) — owns **projects / tasks / plans / AI decomposition** (the MCP-driven workflow store).
- [**oh-my-claudecode**](https://github.com/Yeachan-Heo/oh-my-claudecode) — owns **task execution**, replaced with `omc team` so each task runs on parallel specialized agents instead of one sequential pass.

`daruma-claude` adds **nothing of its own**. It detects both, points the user at the official install commands when they're missing, and orchestrates them.

---

## Quick Start

> **On Windows: run from WSL.** `omc team` relies on a Unix-y tmux + bash environment. On Windows-native PowerShell + Git Bash tmux workers spawn but their I/O bleeds into the leader pane and the session dies when tmux exits. From WSL it works as designed.

```bash
# 1. daruma — build from source (Rust workspace)
git clone https://github.com/tupical/daruma.git
cd daruma
cargo build --release -p daruma-server -p daruma-cli

# 2. start the HTTP server (keep this running)
./target/release/daruma-server

# 3. register the MCP stdio shim with Claude Code
claude mcp add daruma -- /abs/path/daruma/target/release/daruma-mcp

# 4. oh-my-claudecode (the `omc team` executor)
npm i -g oh-my-claude-sisyphus@latest
omc setup
# enable native teams in ~/.claude/settings.json:
#   { "env": { "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS": "1" } }

# 5. daruma-claude (the glue + CLI)
npm i -g daruma-claude

# 6. verify
daruma-claude doctor          # should print READY

# 7. go
daruma-claude start "refactor the auth module to use OAuth2 with PKCE"
```

That's the whole workflow. Inside Claude Code, the equivalent slash commands are `/daruma-claude:start <task>`, `/daruma-claude:doctor`, `/daruma-claude:setup`.

> **Requirements.** Node.js ≥ 20, Rust toolchain (for daruma build), Claude Code on `PATH`.

---

## Why daruma-claude

|                                          | Without `daruma-claude`                                  | With `daruma-claude`                                                            |
| ---------------------------------------- | ----------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| **Driving daruma**                    | Hand-call MCP tools (`workspace_info`, `create`, `plan_*`)  | One `daruma-claude start "<task>"` walks the whole pipeline                     |
| **Decomposition**                        | Optional, opt-in via `--plan` flag                          | `daruma_ai_decompose` + `plan_create` + `plan_add_task` in one go               |
| **Execute step**                         | One sequential agent per task                               | Each task runs on **N parallel agents** via `omc team`                             |
| **Setup**                                | Three installs + manual orchestration                       | One `daruma-claude start "<task>"`                                              |

---

## How it works

```
┌──────────────────────────────┐
│ daruma-claude start <T>   │ shell
└──────────────┬───────────────┘
               │ spawn daruma-mcp (stdio JSON-RPC)
               ▼
┌──────────────────────────────────────────────────┐
│ 1. parse        → derive {title, description}    │
│ 2. project      → workspace_info / project_list  │
│ 3. seed         → daruma_plan_materialize     │
│                   (plan + root task, one atomic  │
│                   call — plan-only intake)       │
│ 5. execute loop                                  │
│      a. plan_next_task (or just the root)        │
│      b. omc team N:claude "<title>\n<desc>"      │
│      c. daruma_comment(artifact)              │
│      d. complete / retry up to --max-retries     │
│ 6. report      → plan_get progress + summaries   │
└──────────────────────────────────────────────────┘
```

`daruma-claude` never opens a nested Claude Code session at the orchestrator level — `omc team` workers are the only Claude Code panes.

---

## Commands

| Shell                                              | Effect                                                                |
| -------------------------------------------------- | --------------------------------------------------------------------- |
| `daruma-claude start "<task>"`                  | Full pipeline (parse → project → seed → [plan] → execute → report)    |
| `daruma-claude team-from-plan <plan_id>`        | Execute an existing plan by dependency fanout waves through `omc team` |
| `daruma-claude doctor`                          | Detect both deps + MCP tool / `omc team` readiness                    |
| `daruma-claude setup`                           | Print install hints for missing dependencies                          |
| `daruma-claude update`                          | Self-update + omc update; print upgrade hint for daruma            |
| `daruma-claude platform`                        | Print execution mode (`omc-team` or `task-fallback`)                  |
| `daruma-claude --version` / `--help`            |                                                                       |

Inside a Claude Code REPL session:

| Slash                                  | Effect                              |
| -------------------------------------- | ----------------------------------- |
| `/daruma-claude:start <task>`       | Same as `daruma-claude start`    |
| `/daruma-claude:team-from-plan <plan_id>` | Same as `daruma-claude team-from-plan` |
| `/daruma-claude:doctor`             | Same as `daruma-claude doctor`   |
| `/daruma-claude:setup`              | Same as `daruma-claude setup`    |
| `/daruma-claude:branch-tasks`       | Show tasks linked to the current git branch |

Bundled skills:

| Skill | Effect |
| ----- | ------ |
| `team-from-plan` | Execute an existing daruma plan wave-by-wave through `omc team`. |
| `branch-tasks` | Find tasks linked to the current git branch through `branch:` comments. |
| `lesson-capture` | Save a durable reusable lesson as a `lesson:` task comment. |
| `lesson-recall` | Search captured lessons through `daruma_lesson_recall`. |

---

## Start flags

| Flag                       | Effect                                                                                                                |
| -------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `--workers N`              | Parallel agents per `omc team` invocation. Integer 1-20. Default `3`.                                                 |
| `--max-retries M`          | Retries after the first attempt for each task (total attempts = `M + 1`). Default `2`.                                |
| `--agent claude\|codex\|gemini` | Agent type used for `omc team` workers. Default `claude`.                                                        |
| `--plan`                   | AI-decompose the root task into subtasks via `daruma_ai_decompose`, then execute each subtask. See [Caveat](#the-caveat). |
| `--project ID`             | Override default project resolution (workspace info / cwd basename).                                                  |
| `--yes` / `-y`             | Skip y/n confirmations (implied when stdin is not a TTY).                                                             |

---

## The caveat

`--plan` depends on the `daruma_ai_decompose` tool actually being registered on the server you're connected to. It's part of SaaS/Meisei's tool catalog, but the **OSS daruma server does not register it** — `daruma-claude` checks `tools/list` before calling it, and if it's absent, prints a `[decompose]` notice and silently falls back to single-task execution on the root task, same as the API-key case below.

On servers that do carry the tool, AI decomposition additionally requires `OPENAI_API_KEY` set on the **daruma server**. Without it, `daruma_ai_decompose` returns `502 ai_unavailable` and `daruma-claude` falls back the same way. To use real decomposition, export the key before starting the server:

```bash
OPENAI_API_KEY=sk-... ./target/release/daruma-server
```

---

## Limitations

- Single fixed role: `--agent` selects one role for all workers; mixed roles (`1:planner + 2:executor + 1:verifier`) are TODO.
- Artifact capture from `omc team` relies on text summaries written as daruma comments — not yet structured.
- No `daruma-claude cancel`. Use the `cancelomc` keyword or interrupt the shell.
- `daruma-claude doctor` only probes the shell it's run from. On a Windows host run it from inside WSL.
- Plan-mode retries reset task status to `todo` and re-execute — they do **not** mutate the plan (no re-decomposition on repeated failure in v1).

---

## Project layout

```text
.
├── .claude-plugin/plugin.json          # Claude Code plugin manifest
├── package.json                        # npm package + `daruma-claude` bin
├── bin/daruma-claude.mjs            # CLI entry point
├── lib/
│   ├── detect.mjs                      # cross-platform dependency detection
│   ├── orchestrator.mjs                # daruma pipeline driver
│   ├── mcp-client.mjs                  # stdio JSON-RPC client for daruma-mcp
│   ├── omc-team-runner.mjs             # spawns `omc team` per task
│   └── update.mjs                      # self-update via npm registry
├── commands/                           # /daruma-claude:{start,team-from-plan,doctor,setup}
└── skills/                             # the actual contracts
    ├── start/SKILL.md                  # parse → project → seed → [plan] → execute
    ├── team-from-plan/SKILL.md         # execute an existing plan by fanout waves
    ├── doctor/SKILL.md                 # readiness contract
    ├── setup/SKILL.md                  # install-hint contract
    ├── branch-tasks/SKILL.md           # find tasks by git branch
    ├── lesson-capture/SKILL.md         # save durable lesson comments
    └── lesson-recall/SKILL.md          # recall lesson comments
```

---

## Updates

```bash
daruma-claude update                                  # daruma-claude + omc
cd /path/to/daruma && git pull \
  && cargo build --release -p daruma-server -p daruma-cli   # daruma
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

Auth uses npm **Trusted Publishing** (OIDC). One-time setup: on npmjs.com → `daruma-claude` → Settings → Trusted Publishers → add GitHub Actions with org=`tupical`, repo=`daruma-claude`, workflow=`publish.yml`. No secrets are stored in GitHub.

## License

MIT — see [LICENSE](./LICENSE).

This project is unaffiliated with the upstream projects. Full credit to [tupical/daruma](https://github.com/tupical/daruma) and [Yeachan-Heo/oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode).
