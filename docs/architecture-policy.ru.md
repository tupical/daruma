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
| `Actor::User` | Человек (PAT-токен, desktop/web) | `AuthContext::actor()` + `TokenKind::Pat` |
| `Actor::Agent { id, name }` | AI-агент (MCP/SDK/CLI) | `TokenKind::Bot` / `TokenKind::Svc` |

### 2.2 Backfill policy

Исторические задачи **не трогаем**: `0010_tasks_actors.sql` без backfill; UI показывает `actor_unknown` для NULL.

### 2.3 actor_strict

По умолчанию `false`. При `true` — `403`, если bot передаёт `Actor::User`.

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
