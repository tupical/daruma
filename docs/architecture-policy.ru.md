# Architecture policy (решения и инварианты)

> Дополнение к полному контракту в [ARCHITECTURE.md](ARCHITECTURE.md).
> Здесь — зафиксированные policy-решения (backfill, cascade, sequence_id).
> Бэклог — TaskAgent tracker ([README.md](README.md)).

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
| `Actor::User { id }` | PAT | `AuthContext::actor()` + `TokenKind::Pat` |
| `Actor::Agent { name }` | Bot (MCP/SDK/CLI) | `TokenKind::Bot` |
| `Actor::System` | Служебные события | Явно в handler |

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

Монотонный рост, **без переиспользования** номеров после delete. См. §3.7.11 в tracker.

---

## 5. Миграции (краткий реестр)

`0001`…`0010` — см. `crates/storage/migrations/`. Полная таблица в истории git.

---

## 6. WS Protocol v2

`GET /v1/ws`, `Hello` + `Subscribe`, каналы `Tasks` / `Plans` / `Runs`.

---

_Последнее обновление: 2026-05-20._
