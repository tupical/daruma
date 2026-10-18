# Architecture policy (решения и инварианты)

> Дополнение к полному контракту в [ARCHITECTURE.md](ARCHITECTURE.md).
> Здесь — зафиксированные policy-решения (backfill, cascade, sequence_id).

---

## 1. Command / Event Core

Система построена на CQRS+ES: `Command → CommandHandler → Vec<Event> → EventStore.append_batch → projections → EventBus`.

- **CommandHandler** (`crates/core/src/handler.rs`) — единственная точка мутации.
- **EventStore** — append-only лог, монотонный `seq`.
- **Projections** — `TaskRepo`, `ProjectRepo`, `CommentRepo`, `ActivityRepo`, `RelationRepo`, `PlanRepo`.
- **EventBus** — in-process broadcast; WS Hub подписан.

---

## 2. Authorship & Actor Propagation

### 2.1 Модель Actor

| Вариант | Когда | Как выводится |
|---|---|---|
| `Actor::User { id?, name? }` | Человек — сам (desktop/web, PAT) или через агента по его токену. `id` — аутентифицированный принципал (`agent_id` токена либо аккаунт хостинг-платформы), `name` — необязательное отображаемое имя; оба отсутствуют у анонимных/старых записей, `{"kind":"user"}` по-прежнему валиден | `AuthContext::actor()` для `TokenKind::Pat/Svc/Usr/License` → `User { id: agent_id }` |
| `Actor::Agent { id, name }` | Бот (собственный токен агента, не человека) | `TokenKind::Bot` |

Смысл: журнал должен отвечать «кто», а не «через что». Агент, работающий по
токену человека, действует от его имени — авторство у человека; поверхность
вызова (`mcp`, `app`) хостинг может нести отдельным полем актора, домен её не
хранит.

### 2.2 Backfill policy

Исторические задачи **не трогаем**: `0010_tasks_actors.sql` без backfill; UI показывает `actor_unknown` для NULL.

### 2.3 actor_strict

Cargo-feature `actor_strict` сервера `daruma-server`, по умолчанию выключена.

- **Выключена:** клиентский актор, если он передан (поле `actor` конверта
  `POST /v1/commands`), записывается как есть; иначе актор выводится из токена.
- **Включена:** актор всегда выводится из токена (`AuthContext::actor()`),
  клиентский игнорируется — одинаково на `POST /v1/commands`, на
  специализированных маршрутах и в WS. Bot, приславший `Actor::User`, получает
  в журнале `Actor::Agent` своего токена; запрос не отклоняется (`403` нет),
  потому что `actor` по умолчанию равен `{"kind":"user"}` и явную подмену
  нельзя отличить от пропущенного поля.

---

## 3. Task Deletion Cascade

Порядок перед `TaskDeleted`: `TaskUnlinked` → `TaskUnblocked` → `PlanTaskRemoved` → `TaskDeleted`.

Pre-cascade `plan_tasks` — ручной SQL при необходимости, без автоматической миграции.

---

## 4. sequence_id policy

Монотонный рост, **без переиспользования** номеров после delete.

---

## 5. Миграции (краткий реестр)

Нумерованные `.sql`-файлы — см. `crates/storage/migrations/` (живой
реестр). Полная таблица в истории git.

---

## 6. WS Protocol v2

`GET /v1/ws`, `Hello` + `Subscribe`; полный набор каналов — `Channel`
в `crates/events/src/event.rs` (Tasks, Comments, AgentStatus, Presence,
Webhooks, Plans, Runs, Documents, AiOps, WorkUnits, Artifacts, Rules).
`?visibility=mine` на апгрейде оставляет в снимке и live-ленте только
события, записанные принципалом, и события его задач (создал, завершил или
держит живой claim, чьё `agent_claimed` записал он). Принципал — `HostPrincipal`
встраивающего хоста (аккаунт облака), иначе `agent_id` токена;
`all`/без параметра — лента без изменений, иное значение — 400 до апгрейда.
