<p align="right">  <a href="./README.md">English</a> | <strong>Русский</strong>
</p>

<p align="center">
  <br/>
  ◯ ─────────── ◯
  <br/><br/>
  <strong>daruma-cursor</strong>
  <br/>
  <sub>tupical/daruma × Cursor</sub>
  <br/><br/>
  ◯ ─────────── ◯
  <br/>
</p>

<p align="center">
  <strong>Один клик подключает daruma MCP к Cursor.</strong>
  <br/>
  <sub>Готовая Cursor-обвязка для hosted daruma MCP-сервера.</sub>
</p>

---

## Add to Cursor

Жми кнопку в любом браузере, где установлен Cursor — он перехватит deeplink,
покажет диалог подтверждения и сам пропишет сервер в `~/.cursor/mcp.json`.

<p align="center">
  <a href="cursor://anysphere.cursor-deeplink/mcp/install?name=daruma&config=eyJ0eXBlIjoiaHR0cCIsInVybCI6Imh0dHA6Ly9sb2NhbGhvc3Q6ODA4MC92MS9tY3AifQ%3D%3D">
    <img src="https://img.shields.io/badge/Add%20to-Cursor-000000?style=for-the-badge&logo=cursor&logoColor=white" alt="Add to Cursor">
  </a>
</p>

Или скопируй официальный Cursor deeplink:

```
cursor://anysphere.cursor-deeplink/mcp/install?name=daruma&config=eyJ0eXBlIjoiaHR0cCIsInVybCI6Imh0dHA6Ly9sb2NhbGhvc3Q6ODA4MC92MS9tY3AifQ%3D%3D
```

Default-путь Cursor использует HTTP MCP endpoint Daruma. Для локальной
разработки сначала запусти сервер:

```bash
./target/release/daruma-server   # данные: ~/.agents/daruma/data
```

---

## Что делает

`daruma-cursor` — тонкий компаньон Cursor для
[`tupical/daruma`](https://github.com/tupical/daruma). Делает три вещи:

1. **Регистрирует MCP-сервер** в `mcp.json` Cursor — глобально
   (`~/.cursor/mcp.json`) или для проекта (`./.cursor/mcp.json`).
2. **Генерирует ссылку «Add to Cursor»** для установки HTTP MCP в один клик.
3. **Кладёт три правила** в `.cursor/rules/`, которые учат агента Cursor
   работать с `daruma_*`-инструментами (parse → decompose → plan →
   execute) вместо самодельных тудушек и держат его на экономном пути
   `list active`:
   - `daruma-policy.mdc` (`alwaysApply`) — daruma как трекер по
     умолчанию + правила экономии токенов (list-first, без graph-поиска для
     инвентаря).
   - `daruma.mdc` — полный контракт инструментов + audit/close workflow.
   - `workspacegraph.mdc` — guardrails: `daruma_workspacegraph_*` для
     связей/impact, а не для списка открытых задач.

Сам по себе плагин **не содержит логики исполнения**. Cursor-агент общается с
сервером daruma напрямую через MCP — здесь только обвязка.

---

## Установка

### Через npm

```bash
npm i -g daruma-cursor
daruma-cursor install --global   # пишет ~/.cursor/mcp.json
daruma-cursor doctor             # проверка
```

### Вручную

Скопируй [`cursor/mcp.example.json`](./cursor/mcp.example.json) в
`~/.cursor/mcp.json` (или влей запись `daruma` в существующий файл).

---

## CLI

| Команда                                                          | Эффект                                                                  |
| ---------------------------------------------------------------- | ----------------------------------------------------------------------- |
| `daruma-cursor install [--global\|--project DIR]`      | Прописать daruma MCP в `mcp.json` (только если записи нет) + `.cursor/rules/` + OMC-guard. Слэш-команды теперь идут с MCP-сервера как prompts; `--commands` — доложить локальные `.cursor/commands/`. |
| `daruma-cursor uninstall [--global\|--project DIR]`    | Удалить запись.                                                         |
| `daruma-cursor deeplink [--print-scheme]`              | Напечатать официальный Cursor Add-to-Cursor deeplink.                   |
| `daruma-cursor rules [--project DIR] [--force]`        | Положить три `.cursor/rules/*.mdc` (policy + контракт + workspacegraph) в проект. |
| `daruma-cursor doctor [--json\|--quiet]`               | Проверить Cursor MCP config + HTTP-сервер. Exit 0 ⇒ READY.              |
| `daruma-cursor setup`                                  | Подсказки по установке отсутствующего.                                  |
| `daruma-cursor marketplace`                            | Напечатать plugin-манифест daruma (со встроенным актуальным deeplink).  |
| `daruma-cursor --version` / `--help`                   |                                                                         |

### Флаги install

| Флаг                         | По умолчанию               | Заметки                                                     |
| ---------------------------- | -------------------------- | ----------------------------------------------------------- |
| `--global` / `--project DIR` | `--global`                 | В какой `mcp.json` писать.                                  |
| `--transport http\|stdio`    | `http`                     | Cursor по умолчанию использует hosted HTTP MCP.             |
| `--command CMD`              | `daruma` (запуск `daruma mcp`) | Включает stdio fallback и задаёт путь к бинарю.         |
| `--base-url URL`             | `https://daruma.mcpbox.ru` (облако) | Origin HTTP MCP сервера. Для self-host укажи свой (напр. `http://localhost:8080`). |
| `--token T`                  | (нет)                      | Authorization header для self-host scoped-токена (облако — через OAuth). |
| `--name NAME`                | `daruma`                | Переименовать запись (если запускаешь несколько инстансов). |

---

## Как работает deeplink-flow

```
deeplink                                                Cursor
┌─────────────────────────┐    cursor://...  ┌─────────────────────┐
│ daruma               │ ───────────────▶ │ «Установить MCP?»   │
│ [ Add to Cursor ]       │   deeplink       │ пишет mcp.json      │
└─────────────────────────┘                  └─────────────────────┘
        │
        │ использует /clients/cursor-plugin/.daruma-plugin/plugin.json
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
daruma-cursor deeplink
```

---

## Структура

```text
clients/cursor-plugin/
├── package.json                          # npm-пакет + CLI bin
├── bin/daruma-cursor.mjs       # точка входа CLI
├── lib/
│   ├── deeplink.mjs                      # генератор cursor:// install link
│   ├── detect.mjs                        # readiness-проба Cursor + daruma
│   ├── mcp-config.mjs                    # чтение/запись ~/.cursor/mcp.json
│   └── rules.mjs                         # установка .cursor/rules/*.mdc
├── cursor/
│   ├── mcp.example.json                  # эталон для ручной установки
│   └── rules/                            # policy + контракт + workspacegraph guardrails
│       ├── daruma-policy.mdc          # alwaysApply policy (list-first / экономия токенов)
│       ├── daruma.mdc                 # контракт + audit/close workflow
│       └── workspacegraph.mdc            # граф-инструменты: связи/impact, не инвентарь
├── .daruma/plugin.json                   # манифест маркетплейса daruma
└── tests/                                # node --test
```

---

## Требования

- Cursor (любой свежий, с поддержкой MCP)
- Node.js ≥ 20 (только для CLI; в рантайме не нужен)
- Доступный daruma HTTP server — по умолчанию облако (`https://daruma.mcpbox.ru`,
  OAuth), либо self-host, собранный из
  [tupical/daruma](https://github.com/tupical/daruma):
  `cargo build --release -p daruma-server`.
- Бинарь `daruma` (`cargo build --release -p daruma-cli`) нужен только для
  явного fallback `--transport stdio` — он запускается как `daruma mcp`.

---

## Лицензия

MIT — см. [LICENSE](./LICENSE).
