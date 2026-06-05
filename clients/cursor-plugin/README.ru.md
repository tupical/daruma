<p align="right">  <a href="./README.md">English</a> | <strong>Русский</strong>
</p>

<p align="center">
  <br/>
  ◯ ─────────── ◯
  <br/><br/>
  <strong>taskagent-cursor</strong>
  <br/>
  <sub>tupical/taskagent × Cursor</sub>
  <br/><br/>
  ◯ ─────────── ◯
  <br/>
</p>

<p align="center">
  <strong>Один клик подключает taskagent MCP к Cursor.</strong>
  <br/>
  <sub>Готовая Cursor-обвязка для локального taskagent MCP-сервера.</sub>
</p>

---

## Add to Cursor

Жми кнопку в любом браузере, где установлен Cursor — он перехватит deeplink,
покажет диалог подтверждения и сам пропишет сервер в `~/.cursor/mcp.json`.

<p align="center">
  <a href="cursor://anysphere.cursor-deeplink/mcp/install?name=taskagent&config=eyJ0eXBlIjoic3RkaW8iLCJjb21tYW5kIjoidGFza2FnZW50LW1jcCIsImVudiI6eyJUQVNLQUdFTlRfQkFTRV9VUkwiOiJodHRwOi8vbG9jYWxob3N0OjgwODAifX0%3D">
    <img src="https://img.shields.io/badge/Add%20to-Cursor-000000?style=for-the-badge&logo=cursor&logoColor=white" alt="Add to Cursor">
  </a>
</p>

HTTPS-зеркало (если deeplink не открывается напрямую):

```
https://cursor.com/install-mcp?name=taskagent&config=eyJ0eXBlIjoic3RkaW8iLCJjb21tYW5kIjoidGFza2FnZW50LW1jcCIsImVudiI6eyJUQVNLQUdFTlRfQkFTRV9VUkwiOiJodHRwOi8vbG9jYWxob3N0OjgwODAifX0%3D
```

Deeplink ожидает `taskagent-mcp` в `PATH`. Собрать из репозитория
[taskagent](https://github.com/tupical/taskagent):

```bash
cargo build --release -p taskagent-server -p taskagent-mcp-bin
# положить target/release/taskagent-mcp в $PATH (symlink или cp)
./target/release/taskagent-server   # данные: ~/.agents/taskagent/data
```

---

## Что делает

`taskagent-cursor` — тонкий компаньон Cursor для
[`tupical/taskagent`](https://github.com/tupical/taskagent). Делает три вещи:

1. **Регистрирует MCP-сервер** в `mcp.json` Cursor — глобально
   (`~/.cursor/mcp.json`) или для проекта (`./.cursor/mcp.json`).
2. **Генерирует deeplink «Add to Cursor»** для установки локального MCP в один клик.
3. **Кладёт три правила** в `.cursor/rules/`, которые учат агента Cursor
   работать с `taskagent_*`-инструментами (parse → decompose → plan →
   execute) вместо самодельных тудушек и держат его на экономном пути
   `list active`:
   - `taskagent-policy.mdc` (`alwaysApply`) — taskagent как трекер по
     умолчанию + правила экономии токенов (list-first, без graph-поиска для
     инвентаря).
   - `taskagent.mdc` — полный контракт инструментов + audit/close workflow.
   - `workspacegraph.mdc` — guardrails: `taskagent_workspacegraph_*` для
     связей/impact, а не для списка открытых задач.

Сам по себе плагин **не содержит логики исполнения**. Cursor-агент общается с
сервером taskagent напрямую через MCP — здесь только обвязка.

---

## Установка

### Через npm

```bash
npm i -g taskagent-cursor
taskagent-cursor install --global   # пишет ~/.cursor/mcp.json
taskagent-cursor doctor             # проверка
```

### Вручную

Скопируй [`cursor/mcp.example.json`](./cursor/mcp.example.json) в
`~/.cursor/mcp.json` (или влей запись `taskagent` в существующий файл).

---

## CLI

| Команда                                                          | Эффект                                                                  |
| ---------------------------------------------------------------- | ----------------------------------------------------------------------- |
| `taskagent-cursor install [--global\|--project DIR]`      | Прописать taskagent MCP в выбранный `mcp.json`.                         |
| `taskagent-cursor uninstall [--global\|--project DIR]`    | Удалить запись.                                                         |
| `taskagent-cursor deeplink [--print-url]`                 | Напечатать `cursor://`-deeplink (и HTTPS-зеркало).                      |
| `taskagent-cursor rules [--project DIR] [--force]`        | Положить три `.cursor/rules/*.mdc` (policy + контракт + workspacegraph) в проект. |
| `taskagent-cursor doctor [--json\|--quiet]`               | Проверить Cursor + `taskagent-mcp` + HTTP-сервер. Exit 0 ⇒ READY.       |
| `taskagent-cursor setup`                                  | Подсказки по установке отсутствующего.                                  |
| `taskagent-cursor marketplace`                            | Напечатать plugin-манифест taskagent (со встроенным актуальным deeplink).  |
| `taskagent-cursor --version` / `--help`                   |                                                                         |

### Флаги install

| Флаг                         | По умолчанию               | Заметки                                                     |
| ---------------------------- | -------------------------- | ----------------------------------------------------------- |
| `--global` / `--project DIR` | `--global`                 | В какой `mcp.json` писать.                                  |
| `--command CMD`              | `taskagent-mcp`            | Свой бинарь stdio (можно абсолютный путь).                  |
| `--base-url URL`             | `http://localhost:8080`    | `env.TASKAGENT_BASE_URL` для сервера.                       |
| `--token T`                  | (нет)                      | `env.TASKAGENT_TOKEN`.                                      |
| `--name NAME`                | `taskagent`                | Переименовать запись (если запускаешь несколько инстансов). |

---

## Как работает deeplink-flow

```
deeplink                                                Cursor
┌─────────────────────────┐    cursor://...  ┌─────────────────────┐
│ taskagent               │ ───────────────▶ │ «Установить MCP?»   │
│ [ Add to Cursor ]       │   deeplink       │ пишет mcp.json      │
└─────────────────────────┘                  └─────────────────────┘
        │
        │ использует /clients/cursor-plugin/.taskagent-plugin/plugin.json
        │ из этого репо (или npm-тарбола)
        ▼
┌─────────────────────────┐
│ manifest маркетплейса   │
│  name, version, deeplink│
│  install hints, rules   │
└─────────────────────────┘
```

Формат deeplink — официальный
[Cursor MCP install link](https://cursor.com/docs/context/mcp/install-links):

```
cursor://anysphere.cursor-deeplink/mcp/install?name=<NAME>&config=<BASE64_JSON>
```

`config` — base64 от JSON-объекта с одной записью `mcpServers`. Сгенерировать
свой:

```bash
taskagent-cursor deeplink --print-url
```

---

## Структура

```text
clients/cursor-plugin/
├── package.json                          # npm-пакет + CLI bin
├── bin/taskagent-cursor.mjs       # точка входа CLI
├── lib/
│   ├── deeplink.mjs                      # генератор cursor:// install link
│   ├── detect.mjs                        # readiness-проба Cursor + taskagent
│   ├── mcp-config.mjs                    # чтение/запись ~/.cursor/mcp.json
│   └── rules.mjs                         # установка .cursor/rules/*.mdc
├── cursor/
│   ├── mcp.example.json                  # эталон для ручной установки
│   └── rules/                            # policy + контракт + workspacegraph guardrails
│       ├── taskagent-policy.mdc          # alwaysApply policy (list-first / экономия токенов)
│       ├── taskagent.mdc                 # контракт + audit/close workflow
│       └── workspacegraph.mdc            # граф-инструменты: связи/impact, не инвентарь
├── .taskagent/plugin.json                   # манифест маркетплейса taskagent
└── tests/                                # node --test
```

---

## Требования

- Cursor (любой свежий, с поддержкой MCP)
- Node.js ≥ 20 (только для CLI; в рантайме не нужен)
- `taskagent-mcp` и `taskagent-server` из
  [tupical/taskagent](https://github.com/tupical/taskagent), собранные через
  `cargo build --release -p taskagent-server -p taskagent-mcp-bin`

---

## Лицензия

MIT — см. [LICENSE](./LICENSE).
