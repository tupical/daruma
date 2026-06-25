# taskagent Codex plugin

Codex-плагин для `tupical/daruma`, собранный по аналогии с `clients/claude-plugin` и `clients/cursor-plugin`.

## Что внутри

- `.codex-plugin/plugin.json` — manifest Codex plugin.
- `commands/` — slash-команды taskagent (`tasks`, `plan`, `next`, `mine`, `branch-tasks`, `start`, `doctor`, `setup`, `init`).
- `skills/` — skills для сценариев setup/start/doctor, branch-tasks и lesson-capture/lesson-recall.
- `lib/policy.mjs` + `bin/taskagent-codex.mjs` — managed-блок в `AGENTS.md` (в т.ч. правило: спрашивать пользователя перед `status=all` у list/plan_list).

Один раз на репозиторий:

```bash
taskagent-codex init
```

## Структура

```
clients/codex-plugin/
├── .codex-plugin/plugin.json
├── bin/taskagent-codex.mjs
├── lib/policy.mjs
├── commands/
├── skills/
├── tests/
├── package.json
├── README.md
└── LICENSE
```
