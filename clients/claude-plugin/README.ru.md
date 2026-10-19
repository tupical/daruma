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
  <sub>Одна команда в шелле прогоняет пайплайн <code>tupical/daruma</code> (parse → plan → execute), где исполнителем каждой задачи выступает <code>omc team</code> — параллельные агенты.</sub>
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
  <a href="#режим-плана">Режим плана</a> ·
  <a href="#ограничения">Ограничения</a>
</p>

---

> ⚠️ **Дисклеймер.** Это 100% AI-Slop, используйте на свой страх и риск.

---

**Одна команда в шелле прогоняет весь пайплайн `tupical/daruma` — парсит задачу, записывает её планом в daruma и выполняет параллельными `/team`-агентами oh-my-claudecode — либо выполняет все задачи готового плана через `team-from-plan`. Никаких форков апстрима, склейных промптов или копи-пейста между сессиями.**

`daruma-claude` — это тонкий плагин для Claude Code и npm-CLI, который связывает два уже существующих проекта:

- [**tupical/daruma**](https://github.com/tupical/daruma) — владеет **проектами / задачами / планами** (MCP-управляемое хранилище воркфлоу). Принимает готовую структуру плана и не разбивает задачи на подзадачи.
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

# 3. регистрируем MCP stdio-сервер в Claude Code
claude mcp add daruma -- /abs/path/daruma/target/release/daruma mcp

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

Это весь воркфлоу в shell.

**Для slash-команд плагин ставится отдельно.** `npm i -g daruma-claude` только кладёт CLI `daruma-claude` в `PATH`, а шаг 3 регистрирует только MCP-сервер daruma — ни то, ни другое не регистрирует плагин Claude Code. Чтобы появились `/daruma-claude:start <задача>`, `/daruma-claude:doctor`, `/daruma-claude:setup` и хуки SessionStart/Stop, добавь этот репозиторий как marketplace плагинов и установи плагин из него:

```bash
claude plugin marketplace add tupical/daruma
claude plugin install daruma-claude@daruma
```

Или внутри сессии Claude Code: `/plugin marketplace add tupical/daruma`, затем `/plugin install daruma-claude@daruma`. После этого перезапусти Claude Code; `claude plugin list` должен показать `daruma-claude@daruma` как enabled.

> **Требования.** Node.js ≥ 20, Rust toolchain (для сборки daruma), Claude Code на `PATH`.

---

## Зачем daruma-claude

|                                      | Без `daruma-claude`                                          | С `daruma-claude`                                                                            |
| ------------------------------------ | --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| **Управление daruma**             | Руками дёргаешь MCP-инструменты (`workspace_info`, `create`, …) | Один `daruma-claude start "<задача>"` прогоняет весь пайплайн                                |
| **Приём плана**                      | Руками вызываешь `daruma_plan_materialize` на каждую задачу     | Корневая задача становится планом одним вызовом `daruma_plan_materialize`                    |
| **Шаг Execute**                      | Один последовательный агент на задачу                           | Каждая задача — **N параллельных агентов** через `omc team`                                     |
| **Сетап**                            | Три инсталла + ручная оркестрация                               | Один `daruma-claude start "<задача>"`                                                        |

---

## Как это работает

```
┌──────────────────────────────┐
│ daruma-claude start <T>   │ shell
└──────────────┬───────────────┘
               │ spawn daruma MCP (stdio JSON-RPC)
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

`daruma-claude` никогда не открывает вложенную сессию Claude Code на уровне оркестратора — единственные панели Claude Code это воркеры `omc team`.

---

## Команды

| Шелл                                              | Что делает                                                            |
| ------------------------------------------------- | --------------------------------------------------------------------- |
| `daruma-claude start "<задача>"`               | Полный пайплайн (parse → project → seed → [plan] → execute → отчёт)   |
| `daruma-claude team-from-plan <plan_id>`       | Исполнить существующий план волнами по зависимостям через `omc team`  |
| `daruma-claude doctor`                         | Детект обеих зависимостей + готовность MCP-инструментов и `omc team` |
| `daruma-claude setup`                          | Подсказки по установке отсутствующего                                 |
| `daruma-claude update`                         | Самообновление + обновление omc; подсказка для daruma              |
| `daruma-claude platform`                       | Печатает режим исполнения (`omc-team` или `task-fallback`)            |
| `daruma-claude --version` / `--help`           |                                                                       |

Внутри сессии Claude Code (после [установки плагина](#быстрый-старт) — один `npm i -g` их не регистрирует):

| Slash                                  | Что делает                              |
| -------------------------------------- | --------------------------------------- |
| `/daruma-claude:start <задача>`     | То же, что `daruma-claude start`     |
| `/daruma-claude:team-from-plan <plan_id>` | То же, что `daruma-claude team-from-plan` |
| `/daruma-claude:doctor`             | То же, что `daruma-claude doctor`    |
| `/daruma-claude:setup`              | То же, что `daruma-claude setup`     |
| `/daruma-claude:branch-tasks`       | Задачи, привязанные к текущей git-ветке |

---

## Флаги `start`

| Флаг                            | Что делает                                                                                                                                       |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--workers N`                   | Сколько параллельных агентов в каждом вызове `omc team`. Целое 1-20. По умолчанию `3`.                                                            |
| `--max-retries M`               | Сколько повторов после первой попытки на каждую задачу (всего попыток = `M + 1`). По умолчанию `2`.                                              |
| `--agent claude\|codex\|gemini` | Тип агента для воркеров `omc team`. По умолчанию `claude`.                                                                                       |
| `--plan`                        | Исполнить корневую задачу через цикл плана (claim, run в daruma), а не напрямую. Не декомпозирует. См. [Режим плана](#режим-плана).          |
| `--project ID`                  | Переопределяет авто-резолв проекта (workspace info / basename текущего каталога).                                                                |
| `--yes` / `-y`                  | Пропустить подтверждения y/n (подразумевается, когда stdin не TTY).                                                                              |

---

## Режим плана

`start` всегда записывает корневую задачу планом из одной задачи (`daruma_plan_materialize`). `--plan` меняет только способ исполнения: без флага корневая задача исполняется напрямую; с флагом цикл плана открывает run в daruma, берёт задачу через `daruma_plan_drain_next` и закрывает run через `daruma_run_complete` или `daruma_run_abort`.

`daruma-claude` никогда не разбивает задачу на подзадачи: он не вызывает инструментов AI-декомпозиции и не требует AI-провайдера на сервере, поэтому `--plan` ведёт себя одинаково на OSS- и SaaS-серверах. Чтобы исполнить несколько задач, сначала сложи их в один план (planning-слой или один вызов `daruma_plan_materialize` со списком `tasks`), затем запусти `daruma-claude team-from-plan <plan_id>`.

---

## Ограничения

- Одна фиксированная роль: `--agent` выбирает одну роль для всех воркеров; микс ролей (`1:planner + 2:executor + 1:verifier`) — TODO.
- Сбор артефактов из `omc team` опирается на текстовые summary, записанные комментариями в daruma — пока не структурирован.
- Нет `daruma-claude cancel`. Нажми Ctrl-C в терминале `start` / `team-from-plan`: daruma-claude выполнит `omc team shutdown` для запущенной команды, подождёт её остановки до 60 с, запишет попытку блокером `cancelled` (в режиме плана снимет claim и прервёт run) и выйдет с кодом 130. Второй Ctrl-C — немедленный выход; если команда не остановилась, сообщение назовёт её — доверши `omc team shutdown <team>`.
- `daruma-claude doctor` проверяет только тот шелл, из которого запущен. На Windows-хосте запускай из WSL.
- Повторы в plan-режиме сбрасывают статус задачи в `todo` и пере-исполняют её — план при этом **не** мутирует (никакого перепланирования при повторных провалах в v1).

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
│   ├── mcp-client.mjs                  # stdio JSON-RPC клиент к MCP-серверу daruma
│   ├── omc-team-runner.mjs             # спавнит `omc team` под каждую задачу
│   └── update.mjs                      # самообновление через npm registry
├── commands/                           # slash-команды /daruma-claude:*
└── skills/                             # сами контракты (SKILL.md на команду/скилл)
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

## Выпуск версии

CI-workflow для публикации нет: `daruma-claude` публикуется в npm вручную — мейнтейнером с правами на публикацию пакета. Из `clients/claude-plugin`:

```bash
npm run release:patch   # 0.3.1 → 0.3.2 (или release:minor / release:major)
npm test
```

`release:*` только поднимает версию — в `package.json` и, через хук `version`, в `.claude-plugin/plugin.json`. Коммита, тега и push он не делает: плагин лежит в подкаталоге монорепо daruma, поэтому бамп попадает в `main` обычным путём, как любая другая правка. После этого:

```bash
npm publish             # из clients/claude-plugin, на смёрженном main
```

Сам плагин Claude Code публиковать не нужно — `/plugin install` читает `clients/claude-plugin` прямо из этого репозитория.

---

## Лицензия

MIT — см. [LICENSE](./LICENSE).

Проект не аффилирован с апстрим-проектами. Полный кредит — [tupical/daruma](https://github.com/tupical/daruma) и [Yeachan-Heo/oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode).
