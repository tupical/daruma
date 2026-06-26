# Changelog

Все заметные изменения проекта собраны здесь. Записи сгруппированы по
датам, новые сверху. Формат основан на
[Keep a Changelog](https://keepachangelog.com/) и здравом смысле:
рассказываем «что это даёт пользователю», а технические детали
оставляем в `git log` и human_log Changelog в Daruma tracker.

## 2026-06-09 — релиз 0.2.0

Главное: вместо россыпи бинарей и обёрток теперь **один бинарь
`daruma`**, через который настраивается и обслуживается всё. Облако в
OSS больше не зашито — бинарь работает с любым сервером по
`credentials.json`.

### Единый бинарь `daruma`

stdio MCP-сервер стал подкомандой CLI: один артефакт `daruma` — это и
CLI, и лаунчер, и MCP-сервер.

- `daruma mcp` поднимает stdio MCP-сервер вместо отдельного бинаря
  `daruma-mcp`. Регистрация: `claude mcp add daruma -- daruma mcp`.
- Голый `daruma` (без подкоманды) печатает инструкцию подключения
  HTTP MCP. При наличии кред — готовый сниппет под тот сервер, на который
  указывает `~/.agents/daruma/credentials.json`.
- `daruma install --claude` пишет project-policy (`CLAUDE.md`) и
  `.omc`-guard — теперь это единственный источник этого текста (совпадает
  байт-в-байт с плагином `daruma-claude`).

### Cloud-agnostic ядро

- Бинарь `daruma` не упоминает никакой хостинг: читает generic
  `server_url` + `token` из кред и работает против любого сервера —
  self-host или иного.
- Из OSS-репозитория убраны cloud-ориентированный `install.sh` и его
  GitHub Pages workflow; онбординг в облако из OSS больше не поставляется.

### Сервер

- Endpoint скачивания бинаря переехал с
  `/v1/downloads/daruma-mcp/{platform}` на
  `/v1/downloads/daruma/{platform}` и отдаёт единый бинарь.

### Клиентские плагины

- `daruma-claude` делегирует запись policy бинарю `daruma`, когда он
  на `PATH`; иначе — bundled node-writer (вывод идентичен).
- `daruma-cursor` больше не хардкодит хостинг-URL в подсказках.

### Документация

- README переписан в более лаконичный, обзорный вид.

## 2026-05-29

### Архитектура: OSS core как версионированная зависимость

- Добавлен [docs/RELEASES.md](docs/RELEASES.md): релизный контракт OSS core,
  git-tag формат `daruma-vMAJOR.MINOR.PATCH`, стабильные surfaces
  (`/v1/*`, WS, MCP, events, публичные Rust crates) и чеклист релиза.
- `MODULE_CONTRACT` и `MODULES` теперь требуют, чтобы standalone apps
  фиксировали потребляемый OSS core в `module.toml [core]`; `vendor/oss`
  считается только локальным dev override.
- README уточняет, что базовый `daruma-web` — read-only observability UI,
  а write/admin workflows принадлежат MCP/CLI/desktop/embed.

### Fix: список задач снова открывается (миграция 18, `projects.slug`)

Миграция `0018_project_slug` падала с `UNIQUE constraint failed:
projects.slug`, из-за чего сервер не мог открыть БД и список задач
возвращал ошибку (`storage error: while executing
migration 18`). Причина: backfill брал слаг из `substr(id, 1, 8)`, но
`id` — это UUIDv7 c префиксом `prj_`, поэтому у всех проектов, созданных
в одном временном окне, префикс совпадал и слаги дублировались.

- Backfill переписан на `'p-' || replace(id, 'prj_', '')` — слаг теперь
  выводится из полного `id` (первичный ключ), коллизии исключены.
- `Db::migrate()` стал устойчив к правке уже применённой миграции:
  при несовпадении контрольной суммы он один раз сверяет checksum
  применённых миграций с эталонными и повторяет запуск, поэтому БД, где
  старая 0018 успела примениться, не ломаются при обновлении.
- Добавлен регрессионный тест на уникальность слагов для id с общим
  префиксом.

### Вынос web в отдельный репозиторий `daruma-web`

Браузерный UI (`apps/web-leptos`, Leptos CSR → WASM) вынесен из монорепо
в самостоятельный репозиторий `daruma-web`. Теперь OSS-сервер —
голый backend: только API (`/v1/*`, `/v1/ws`) и MCP, без раздачи
статики.

- Из `apps/server` убран `ServeDir` на `/web`.
- `apps/web-leptos` удалён из воркспейса (members) и профиль
  `release-wasm` перенесён в новый репозиторий.
- `daruma-web` — отдельный Cargo-воркспейс; OSS-крейты
  (`shared`/`domain`/`events`/`api-dto`) потребляются read-only через
  `vendor/oss` local development override.
- Документация (`README`, `ARCHITECTURE`, `MODULES`, `Justfile`) обновлена
  под новый расклад.

## 2026-05-20

Большой день: подчищена архитектура модулей, поднята AI-обвязка, и
сервер обзавёлся набором новых ручек для агентов.

### Бэклог перенесён в Daruma tracker

Локальные `docs/ROADMAP.md`, research-планы (Plane/Linear/CTM/TON) и
`docs/mcp/MCP-ROADMAP.md` / `docs/integrations/CLAUDE-PLUGIN-ROADMAP.md`
удалены из репозитория. В трекере проекта Daruma:

- sub-plan **MCP Roadmap** — 24 задачи M1.1–M7.6 (server-side tools);
- sub-plan **Claude Plugin (out-of-repo)** — P1–P6 с `relates_to` на MCP-зависимости;
- human_log **Research archive** — сжатая карта A/B/C.

В `docs/` остались только контрактные файлы; см. [docs/README.ru.md](docs/README.ru.md).

### Уборка markdown в корне

- Полный [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) перенесён из корня; policy — в
  [docs/architecture-policy.ru.md](docs/architecture-policy.ru.md).
- Удалены корневые `code-style.md` и `ai-agent-system.md` (содержимое в
  `CONTRIBUTING.md` и [docs/guides/ai-agent.md](docs/guides/ai-agent.md)).
- В корне остались: README, CHANGELOG, CONTRIBUTING, CODE_OF_CONDUCT,
  LICENSE.commons-clause.

### Default-проект для `plan_list` и `doc_list`

`daruma_list` давно подставлял в запрос проект из workspace
(`daruma_project_use` или env `DARUMA_PROJECT_ID`), но
`daruma_plan_list` и `daruma_doc_list` про это «забывали»:
агент без явного `project_id` либо получал планы из всех проектов
сразу, либо упирался в 400. Теперь обе ручки симметричны:

- `daruma_plan_list` без `project_id` использует default
  проект; чтобы явно перейти кросс-проект, нужно передать
  `project_id: "all"`.
- `daruma_doc_list` без `project_id` тоже подставляет default;
  если ни параметра, ни default'а нет — MCP сразу отвечает понятной
  ошибкой, не отправляя сломанный URL на сервер.

Контракт описан в `description` каждого инструмента — агент видит
ожидание прямо в каталоге `tools/list`.

### Обязательный `status` для `daruma_list` и `daruma_plan_list`

`GET /v1/tasks`, `GET /v1/plans`, MCP `daruma_list` и
`daruma_plan_list` **требуют** явный параметр `status`. Без него —
`400 validation`.

Поддерживается:

- одиночное значение (`inbox`/`todo`/`in_progress`/`in_review`/
  `done`/`cancelled` для задач; `draft`/`active`/`completed`/
  `abandoned` для планов),
- список через запятую (`status=todo,in_progress`),
- шорткат `status=active` для задач — все нетерминальные статусы,
- `status=all` — явный запрос полного архива (включая `done`).

**Для агентов:** `status=all` у `daruma_list` / `daruma_plan_list`
вызывать только после явного подтверждения пользователя в этом же turn —
ответ может быть очень большим и съесть контекст. По умолчанию —
`active` (задачи) или узкий фильтр статусов (планы). То же правило
прописано в managed-блоках `daruma-claude` (`CLAUDE.md`) и
`daruma-codex` (`AGENTS.md`) после `init`.

Зачем: без обязательного фильтра агенты непредсказуемо тащили весь
бэклог в контекст. Теперь выбор осознанный: `active` для «что
осталось», `all` для аудита.

Неизвестный статус → 400. Пустой `status` (после `trim`) → 400.
Фильтрация задач — на уровне SQL (`status IN (…)`).

### Что нового для пользователей

- **Новые AI-инструменты в MCP и REST.** Появилось четыре «AI-ручки»,
  которыми можно пользоваться из любого MCP-клиента (Claude Code,
  Cursor и других) и напрямую по HTTP:
  - `daruma_research { query, context_task_ids?, save_to_task_id? }`
    — задаёт модели вопрос, при желании опираясь на текст конкретных
    задач, и может сразу сохранить ответ как комментарий типа
    «research» на нужной задаче.
  - `daruma_ai_scope { task_id, direction: up|down }` — модель
    переписывает заголовок и описание задачи «шире» (эпик) или «уже»
    (одно конкретное действие). Возвращает готовый
    `Command::UpdateTask`, применять или нет — решает вызывающая
    сторона.
  - У всех AI-вызовов появился флаг `use_research_provider`. Пока он
    игнорируется (один провайдер), но клиенты уже могут писать
    интеграции под итоговую форму, не дожидаясь, пока добавят второго
    провайдера.

- **Промпты вынесены в TOML-файлы.** Раньше тексты промптов были
  «зашиты» в Rust-код. Теперь они лежат в `crates/ai/prompts/*.toml`
  и подгружаются через единый `PromptRegistry`. Для пользователя это
  означает: будущие правки формулировок не будут требовать перекомпиляции
  всего сервера, и стало проще видеть, *что именно* спрашивают у модели.

- **Расширенный health-check.** Эндпоинт `/v1/healthz` теперь отвечает
  не только `status` и `version`, но ещё и:
  - `core_version` — версия ядра (`daruma-core`),
  - `api_version` — версия REST-контракта (сейчас `"v1"`).
  Это нужно мониторингам и клиентам, чтобы детектить рассинхрон между
  собранным бинарником и поддерживаемой версией API без копания в
  манифестах.

- **Провенанс задач и планов.** На уровне базы добавились два опциональных
  поля:
  - `Plan.source_brief` — необязательный «бриф», который породил план
    (свободный текст);
  - `Task.source_event_id` — ссылка на событие, из которого родилась
    задача. Сейчас заполняется для подзадач, полученных через
    `SplitTask`. Будущие AI-флоу смогут привязывать сюда свои события
    и тем самым «трассировать» происхождение задачи.

- **Документ для контрибьюторов.** Появились `CONTRIBUTING.md` (как
  открывать issues и PR), `CODE_OF_CONDUCT.md`
  (Contributor Covenant 2.1) и DCO-чек в GitHub Actions — каждый
  коммит должен быть подписан через `git commit -s`. Никаких CLA не
  требуется. Лицензия осталась прежней: Apache-2.0 WITH
  Commons-Clause (своими руками хостить можно, перепродавать как сервис
  третьим лицам — нет).

### Изменения в архитектуре

- **Поделили «ядро» и «модули».** В `docs/MODULES.md` появился реестр
  всех приложений и крейтов с пометкой «kind» (core / transport /
  client / embed / integration). В `docs/MODULE_CONTRACT.md` —
  формальный SLA: что ядро гарантирует модулям (стабильность `/v1/*`,
  правила ритуала при `breaking-change`, error-контракт). В
  `ARCHITECTURE.md` появилась соответствующая секция с диаграммой
  «Module → Core» и описанием embed-режима.

- **Десктоп переехал на публичный фасад ядра.** Появился
  `daruma_core::embed::*` — единственная точка, через которую
  embed-клиенты (сейчас — `apps/desktop`) могут дотянуться до рантайма
  (`Db`, `EventBus`, `CommandBus`, `Command`, репозитории и
  `SqliteEventStore`). `apps/desktop` больше не зависит напрямую от
  `daruma-storage` / `daruma-events`. Для будущих клиентов это
  правило: ходить *только* через `embed`.

- **CI-аудит границ.** Появился новый workflow
  `.github/workflows/audit-imports.yml`, который на каждом PR
  проверяет:
  - Что ядро (`crates/{shared,domain,events,core,storage,auth}` и
    `apps/server/src/`) не импортирует ничего из `apps/*`.
  - Что embed-клиенты не лезут в `daruma_storage::*` /
    `daruma_events::*` напрямую (только через `embed`).
  Если правило нарушено — PR красный с указанием конкретного файла.

- **Web-клиент стал «полноценным модулем».** Добавился
  `apps/web-leptos/README.md` и машинный манифест
  `apps/web-leptos/module.toml` (kind=client, contract=`/v1/* + WS`).
  Сборка идёт через `trunk build --release`, артефакт раздаёт
  `apps/server` под `/web`.

- **AiProvider trait.** В `crates/ai/` появился общий trait
  `AiProvider { generate_text, generate_object }`. Сегодняшний
  `OpenAiClient` его реализует. Это «фундамент» для будущего
  Ollama / research-провайдера: переключение между провайдерами не
  потребует переписывать каждый вызывающий файл.

### Производственная инфраструктура

- Серверный healthcheck теперь отдаёт новый формат с `core_version` и
  `api_version`.

### Внутренние «закрылись задачи»

Закрыто 14 задач из `daruma_list`. По разделам ROADMAP:

- §3.4 Modular Architecture: W1.1, W1.2, W1.3, W2.1, W2.2, W3.2, W4.1 —
  то есть весь каркас, кроме W3.1 (mobile-scaffold на Tauri 2,
  отдельный большой пакет работ).
- §3.8 Claude-task-master-derived deltas: §3.8.5 (prompt registry),
  §3.8.6 (research), §3.8.7 (scope), §3.8.9 (provider trait),
  §3.8.10 (provenance fields), §3.8.13 (use_research_provider).

Что осталось в backlog после сегодняшнего дня:

- §3.3 Device Sync — phases 1–7 (мульти-неделя на каждую фазу).
- §3.4 W3.1 — Tauri 2 scaffold для мобилки.
- §3.7.3 / .8 / .9 / .10 — автор пометил «defer до запроса».
- §3.8.12 — async AI ops как типизированные WS-события.
- §3.9.6 — снапшоты агрегатов для проекций.
- Auto-append в Interview/HumanLog — отдельный фича-пакет с настройками
  в web UI.

Эти задачи никуда не делись, просто не вошли в сегодняшний сприн — они
требуют либо длительной работы (`§3.3` Device Sync),
либо отдельной инфраструктуры (Tauri), либо явного «дай
команду» от автора (`§3.7.x` defer).

---

История до 2026-05-20 — см. `git log` и Daruma tracker (human_log Changelog). До
сегодняшнего дня формального changelog не велось.
