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
| `Actor::User { id?, name?, client? }` | Человек — сам (desktop/web, PAT) или через агента по его токену. `id` — аутентифицированный принципал (`agent_id` токена либо аккаунт хостинг-платформы), `name` — необязательное отображаемое имя; оба отсутствуют у анонимных/старых записей, `{"kind":"user"}` по-прежнему валиден; необязательный `client` — см. ниже | `AuthContext::actor()` для `TokenKind::Pat/Svc/Usr/License` → `User { id: principal_id ?? agent_id }`; `principal_id` — `HostPrincipal` встраивающего хоста, переносится одинаково в HTTP и при апгрейде WS |
| `Actor::Agent { id, name, client? }` | Бот (собственный токен агента, не человека) | `TokenKind::Bot` |

Смысл: журнал должен отвечать «кто», а не «через что». Агент, работающий по
токену человека, действует от его имени — авторство у человека; поверхность
вызова (`mcp`, `app`) хостинг может нести отдельным полем актора, домен её не
хранит.

**Поле `client`** (оба варианта, необязательное) — приложение-источник вызова:
MCP-клиент, через который пришла команда (Claude Code, Codex, Cursor,
Hermes…). Источник — заголовок запроса `X-Daruma-Client`
(`daruma_auth::CLIENT_HEADER`): хостинговый MCP-шлюз копирует в него
`clientInfo.name` из `initialize`, иначе продукт из User-Agent (например,
`codex-mcp-client`). Auth-middleware (`insert_auth_context` в
`apps/server/src/middleware/auth.rs`) кладёт очищенное значение
(`sanitize_client`: обрезка пробелов, без управляющих символов, не длиннее
64 символов) в `AuthContext::client` на каждый запрос, не из кеша;
`AuthContext::actor()` проставляет его в выведенный из токена актор. Нет
заголовка — нет `client`; старые записи без поля читаются как прежде.

`client` — атрибуция для замеров по клиентам, **не auth-утверждение**: значение
сообщает сам вызывающий, оно не подтверждает личность пользователя или агента и
не участвует в проверке прав. Кто действует, отвечают `id`/вариант актора из
токена; `client` отвечает только «через какое приложение».

### 2.2 Backfill policy

Исторические задачи **не трогаем**: `0010_tasks_actors.sql` без backfill; UI показывает `actor_unknown` для NULL.

### 2.3 actor_strict

Cargo-feature `actor_strict` сервера `daruma-server`, по умолчанию выключена.

- **Выключена:** клиентский актор, если он передан (поле `actor` конверта
  `POST /v1/commands`), записывается как передан, с одним дополнением: если в
  нём нет `client`, `client` заполняется из контекста запроса
  (`AuthContext::client`, т.е. заголовок `X-Daruma-Client`; переданный
  клиентом `client` сохраняется) — `actor_from` в
  `apps/server/src/routes/mod.rs`, `actor.or_client(auth.client)`. Если поле
  `actor` пропущено или равно `null`, актор выводится из токена
  (`AuthContext::actor()`, с тем же `client`) — анонимного `{"kind":"user"}`
  по умолчанию нет. Специализированные маршруты и WS клиентского актора не
  принимают и всегда берут токен.
- **Включена:** актор всегда выводится из токена (`AuthContext::actor()`,
  `client` — из заголовка запроса), клиентский игнорируется целиком, включая
  его `client` — одинаково на `POST /v1/commands`, на
  специализированных маршрутах и в WS. Bot, приславший `Actor::User`, получает
  в журнале `Actor::Agent` своего токена; запрос не отклоняется (`403` нет):
  результат тот же, что при пропущенном поле, так что для совместимости
  со старыми клиентами подмена молча нейтрализуется, а не запрещается.

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
Тот же принципал — автор команд, отправленных `Dispatch` по этому соединению
(как в HTTP); claims и leases по-прежнему привязаны к `agent_id` токена.
