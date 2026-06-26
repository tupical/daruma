<p align="right">
  <a href="./README.md">English</a> | <strong>Русский</strong>
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
  <strong>Склеиваем, не форкаем.</strong>
  <br/>
  <sub>Одна команда в шелле прогоняет пайплайн <code>tupical/daruma</code> (parse → decompose → plan → execute), где исполнителем каждой задачи выступает <code>omc team</code> — параллельные агенты.</sub>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/daruma-claude"><img src="https://img.shields.io/npm/v/daruma-claude?color=blue" alt="npm"></a>
  <a href="https://www.npmjs.com/package/daruma-claude"><img src="https://img.shields.io/npm/dm/daruma-claude" alt="downloads"></a>
  <img src="https://img.shields.io/node/v/daruma-claude" alt="node">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-green" alt="license"></a>
</p>

<p align="center">
  <a href="#быстрый-старт">Быстрый старт</a> ·
  <a href="#зачем-daruma-claude">Зачем</a> ·
  <a href="#как-это-работает">Как это работает</a> ·
  <a href="#команды">Команды</a> ·
  <a href="#заметка">Заметка</a> ·
  <a href="#ограничения">Ограничения</a>
</p>

---

> ⚠️ **Дисклеймер.** Это 100% AI-Slop, используйте на свой страх и риск.

---

**Одна команда в шелле прогоняет весь пайплайн `tupical/daruma` — парсит задачу, опционально декомпозирует её AI в план, а затем выполняет каждую готовую таску параллельными `/team`-агентами oh-my-claudecode. Никаких форков апстрима, склейных промптов или копи-пейста между сессиями.**

`daruma-claude` — это тонкий плагин для Claude Code и npm-CLI, который связывает два уже существующих проекта:

- [**tupical/daruma**](https://github.com/tupical/daruma) — владеет **проектами / задачами / планами / AI-декомпозицией** (MCP-управляемое хранилище воркфлоу).
- [**oh-my-claudecode**](https://github.com/Yeachan-Heo/oh-my-claudecode) — владеет **исполнением задач**, которое мы заменяем на `omc team`: каждая задача выполняется параллельными специализированными агентами вместо одного последовательного прохода.

`daruma-claude` **не добавляет ничего своего**. Детектит обе зависимости, подсказывает официальные команды установки если чего-то не хватает, и связывает их вместе.

---

## Быстрый старт

> **На Windows — запускай из WSL.** `omc team` опирается на Unix-овый tmux + bash. На Windows-native PowerShell + Git Bash tmux воркеры спавнятся, но их вывод смешивается с панелью leader'а, а сессия падает при выходе из tmux. Из WSL всё работает как задумано.

```bash
# 1. daruma — собираем из исходников (Rust workspace)
git clone https://github.com/tupical/daruma.git
cd daruma
cargo build --release -p daruma-server -p daruma-cli

# 2. поднимаем HTTP-сервер (оставляем висеть)
./target/release/daruma-server

# 3. регистрируем MCP stdio-шим в Claude Code
claude mcp add daruma -- /abs/path/daruma/target/release/daruma-mcp

# 4. oh-my-claudecode (исполнитель через `omc team`)
npm i -g oh-my-claude-sisyphus@latest
omc setup
# включить нативные team в ~/.claude/settings.json:
#   { "env": { "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS": "1" } }

# 5. daruma-claude (склейка + CLI)
npm i -g daruma-claude

# 6. проверить
daruma-claude doctor          # должен напечатать READY

# 7. погнали
daruma-claude start "переписать модуль аутентификации на OAuth2 с PKCE"
```

Это весь воркфлоу. Внутри Claude Code эквивалентные slash-команды: `/daruma-claude:start <задача>`, `/daruma-claude:doctor`, `/daruma-claude:setup`.

> **Требования.** Node.js ≥ 20, Rust toolchain (для сборки daruma), Claude Code на `PATH`.

---

## Зачем daruma-claude

|                                      | Без `daruma-claude`                                          | С `daruma-claude`                                                                            |
| ------------------------------------ | --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| **Управление daruma**             | Руками дёргаешь MCP-инструменты (`workspace_info`, `create`, …) | Один `daruma-claude start "<задача>"` прогоняет весь пайплайн                                |
| **Декомпозиция**                     | Опционально, по флагу `--plan`                                  | `daruma_ai_decompose` + `plan_create` + `plan_add_task` одной командой                       |
| **Шаг Execute**                      | Один последовательный агент на задачу                           | Каждая задача — **N параллельных агентов** через `omc team`                                     |
| **Сетап**                            | Три инсталла + ручная оркестрация                               | Один `daruma-claude start "<задача>"`                                                        |

---

## Как это работает

```
┌──────────────────────────────┐
│ daruma-claude start <T>   │ shell
└──────────────┬───────────────┘
               │ spawn daruma-mcp (stdio JSON-RPC)
               ▼
┌──────────────────────────────────────────────────┐
│ 1. parse        → derive {title, description}    │
│ 2. project      → workspace_info / project_list  │
│ 3. seed         → daruma_create(root task)    │
│ 4. [--plan]     → daruma_ai_decompose         │
│                   + plan_create + plan_add_task  │
│ 5. execute loop                                  │
│      a. plan_next_task (or just the root)        │
│      b. omc team N:claude "<title>\n<desc>"      │
│      c. daruma_comment(artifact)              │
│      d. complete / retry up to --max-retries     │
│ 6. report      → plan_get progress + summaries   │
└──────────────────────────────────────────────────┘
```

`daruma-claude` никогда не открывает вложенную сессию Claude Code на уровне оркестратора — единственные панели Claude Code это воркеры `omc team`.

---

## Команды

| Шелл                                              | Что делает                                                            |
| ------------------------------------------------- | --------------------------------------------------------------------- |
| `daruma-claude start "<задача>"`               | Полный пайплайн (parse → project → seed → [plan] → execute → отчёт)   |
| `daruma-claude doctor`                         | Детект обеих зависимостей + готовность MCP-инструментов и `omc team` |
| `daruma-claude setup`                          | Подсказки по установке отсутствующего                                 |
| `daruma-claude update`                         | Самообновление + обновление omc; подсказка для daruma              |
| `daruma-claude platform`                       | Печатает режим исполнения (`omc-team` или `task-fallback`)            |
| `daruma-claude --version` / `--help`           |                                                                       |

Внутри сессии Claude Code:

| Slash                                  | Что делает                              |
| -------------------------------------- | --------------------------------------- |
| `/daruma-claude:start <задача>`     | То же, что `daruma-claude start`     |
| `/daruma-claude:doctor`             | То же, что `daruma-claude doctor`    |
| `/daruma-claude:setup`              | То же, что `daruma-claude setup`     |

---

## Флаги `start`

| Флаг                            | Что делает                                                                                                                                       |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--workers N`                   | Сколько параллельных агентов в каждом вызове `omc team`. Целое 1-20. По умолчанию `3`.                                                            |
| `--max-retries M`               | Сколько повторов после первой попытки на каждую задачу (всего попыток = `M + 1`). По умолчанию `2`.                                              |
| `--agent claude\|codex\|gemini` | Тип агента для воркеров `omc team`. По умолчанию `claude`.                                                                                       |
| `--plan`                        | AI-декомпозиция корневой задачи на подзадачи через `daruma_ai_decompose`, дальше исполняем каждую. См. [Заметку](#заметка).                  |
| `--project ID`                  | Переопределяет авто-резолв проекта (workspace info / basename текущего каталога).                                                                |
| `--yes` / `-y`                  | Пропустить подтверждения y/n (подразумевается, когда stdin не TTY).                                                                              |

---

## Заметка

AI-декомпозиция (`--plan`) требует, чтобы `OPENAI_API_KEY` был выставлен **на стороне daruma-сервера**. Без него `daruma_ai_decompose` возвращает `502 ai_unavailable`, а `daruma-claude` молча откатывается к исполнению одной корневой задачи. Чтобы получить настоящую декомпозицию — экспортни ключ перед запуском сервера:

```bash
OPENAI_API_KEY=sk-... ./target/release/daruma-server
```

---

## Ограничения

- Одна фиксированная роль: `--agent` выбирает одну роль для всех воркеров; микс ролей (`1:planner + 2:executor + 1:verifier`) — TODO.
- Сбор артефактов из `omc team` опирается на текстовые summary, записанные комментариями в daruma — пока не структурирован.
- Нет `daruma-claude cancel`. Используй кейворд `cancelomc` или прерывай шелл.
- `daruma-claude doctor` проверяет только тот шелл, из которого запущен. На Windows-хосте запускай из WSL.
- Повторы в plan-режиме сбрасывают статус задачи в `todo` и пере-исполняют её — план при этом **не** мутирует (никакой re-decomposition при повторных провалах в v1).

---

## Структура проекта

```text
.
├── .claude-plugin/plugin.json          # манифест плагина Claude Code
├── package.json                        # npm-пакет + бинарь `daruma-claude`
├── bin/daruma-claude.mjs            # точка входа CLI
├── lib/
│   ├── detect.mjs                      # кросс-платформенная детекция зависимостей
│   ├── orchestrator.mjs                # драйвер пайплайна daruma
│   ├── mcp-client.mjs                  # stdio JSON-RPC клиент к daruma-mcp
│   ├── omc-team-runner.mjs             # спавнит `omc team` под каждую задачу
│   └── update.mjs                      # самообновление через npm registry
├── commands/                           # /daruma-claude:{start,doctor,setup}
└── skills/                             # сами контракты
    ├── start/SKILL.md                  # parse → project → seed → [plan] → execute
    ├── doctor/SKILL.md                 # контракт готовности
    └── setup/SKILL.md                  # контракт подсказок установки
```

---

## Обновления

```bash
daruma-claude update                                  # daruma-claude + omc
cd /path/to/daruma && git pull \
  && cargo build --release -p daruma-server -p daruma-cli   # daruma
npm i -g oh-my-claude-sisyphus@latest                    # oh-my-claudecode (вручную)
```

---

## Контрибьют

Issues и PR приветствуются. Идея плагина в том, чтобы он оставался тонким, поэтому патчи, превращающие его в самостоятельную сущность (дополнительные шаги «рассуждения», захардкоженные эвристики, новые агенты) скорее всего будут отклонены. Патчи, делающие склейку надёжнее (детекция, понятные ошибки, кросс-платформенные фиксы) — очень welcome.

---

## Релизы

Релизы автоматизированы через [GitHub Actions](.github/workflows/publish.yml). Чтобы выпустить новую версию:

```bash
npm run release:patch   # 0.1.0 → 0.1.1
# либо release:minor / release:major
```

Скрипт бампает `package.json`, создаёт git-тег `vX.Y.Z` и пушит и то и другое. Workflow дальше:

1. Проверяет совпадение тега и `package.json`.
2. Публикует в npm с `--provenance` (подписанная attestation).
3. Создаёт GitHub Release с auto-generated notes.

Аутентификация — npm **Trusted Publishing** (OIDC). Однократная настройка: на npmjs.com → пакет `daruma-claude` → Settings → Trusted Publishers → добавить GitHub Actions с org=`tupical`, repo=`daruma-claude`, workflow=`publish.yml`. Никаких секретов в GitHub.

## Лицензия

MIT — см. [LICENSE](./LICENSE).

Проект не аффилирован с апстрим-проектами. Полный кредит — [tupical/daruma](https://github.com/tupical/daruma) и [Yeachan-Heo/oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode).
