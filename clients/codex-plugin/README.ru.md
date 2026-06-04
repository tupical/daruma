# taskagent Codex plugin

Codex-плагин для `tupical/taskagent`, собранный по аналогии с `clients/claude-plugin` и `clients/cursor-plugin`.

## Что внутри

- `.codex-plugin/plugin.json` — manifest Codex plugin.
- `commands/` — slash-команды taskagent (`tasks`, `plan`, `next`, `mine`, `branch-tasks`, `start`, `doctor`, `setup`).
- `skills/` — skills для сценариев setup/start/doctor, branch-tasks и lesson-capture/lesson-recall.

## Структура

```
clients/codex-plugin/
├── .codex-plugin/plugin.json
├── commands/
├── skills/
├── package.json
├── README.md
└── LICENSE
```
