# Gap-анализ по списку продуктовых возможностей

Контекст: TaskAgent — таск-менеджер для совместной работы агентов с минимальным участием пользователя. Ниже — оценка из текущего списка: что уже есть в core, что можно добавлять следующим приоритетом, и как это лучше адаптировать под агентный сценарий.

## Кратко по текущему состоянию

Уже реализованы базовые строительные блоки для «автономной» работы агентов:

- realtime-шина событий + inbox/pull для агентов;
- планы/иерархия работ + lifecycle;
- bulk-операции;
- документы (markdown), комментарии, связи задач;
- webhooks как интеграционная основа.

Это означает, что следующий рывок — не в «ещё один таск-лист», а в слой orchestration: intake, approvals, и портфельный уровень (epics/initiatives/dashboard).

## Оценка списка: уже есть / частично / в backlog

| Возможность | Статус | Почему |
|---|---|---|
| Requests via Slack/email/threads + Intake forms | **Частично** | Есть webhooks и API-интеграционный контур, но нет готовых коннекторов и формы intake «из коробки». |
| Workflows and Approvals | **Частично** | Есть lifecycle/status у планов и signal-механика для run, но нет отдельного approval engine с SLA/escalation. |
| Business dashboards | **Частично** | Есть события/проекции и данные для метрик, но нет продуктового BI-слоя с permalink/interactive визуализацией. |
| Customers profiles | **Нет** | Нет доменной сущности customer/account и привязки work items к customer impact. |
| Nested pages | **Частично** | Есть documents (markdown), но нет древовидной wiki-навигации и иерархии страниц. |
| Custom Work Item Types | **Нет** | Есть задачи/планы/relations, но нет настраиваемых типов и полей под org-специфику. |
| Project templates | **Нет** | Нет шаблонизатора bootstrap проекта с преднастроенными workflow/views/roles. |
| Time tracking | **Частично** | Есть runs/steps/notes для операционного трейсинга, но не полноценный timesheet-слой. |
| Wiki unified workspace | **Частично** | Документы присутствуют, но нет полноценного wiki UX и knowledge graph. |
| Epics | **Частично** | Иерархия plans есть, можно трактовать как proto-epics, но нет отдельной сущности/представления epic-level. |
| Initiatives | **Нет/Частично** | Портфельного уровня (cross-project strategic layer) как отдельной модели пока нет. |
| Project states (moving/blocked) | **Частично** | На уровне plan/task статусы есть, но единый «health/state» проекта не вычисляется и не отображается как first-class. |
| Bulk ops (50+) | **Да** | Есть bulk_set_status и bulk_attach_to_plan в API/MCP. |

## Что добавлять в разработку в первую очередь (с учётом agent-first)

### P0 — Intake + Approvals (максимальный эффект на «минимум ручного участия»)

1. **Unified Intake Gateway**
   - Каналы: email, Slack, web-form, webhook.
   - На входе агент нормализует payload в единый `IntakeRequest` и сразу создаёт task/plan draft.
   - Авто-классификация приоритета/домена + dedup похожих запросов.

2. **Approval Orchestrator**
   - Multi-step approvals (например: PM -> Security -> Legal).
   - SLA timers + escalation rules + auto-reminders.
   - Агент сам двигает заявку по маршруту, человек подключается только на gate-этапах.

### P1 — Portfolio Layer (Epics/Initiatives + States + Dashboards)

3. **Epics как отдельный уровень UX**
   - Поверх текущих plans сделать epic-представление: прогресс, риски, блокеры, cross-team зависимости.

4. **Initiatives (cross-project)**
   - Стратегический объект, агрегирующий epics/проекты.
   - KPI-прогресс и health-score по инициативе.

5. **Project Health States + Dashboard**
   - Автовычисление: `On track / At risk / Blocked`.
   - Метрики из event stream: cycle time, approval latency, reopen rate, WIP pressure.

### P2 — Data Model расширения для B2B масштаба

6. **Custom Work Item Types + Custom Fields**
   - Типы: `QA Blocker`, `Campaign`, `Infra Update`, ...
   - Схемы полей per type + policy constraints.

7. **Customers entity**
   - Customer profile + priority tier + ARR/segment tags.
   - Линковка задач/эпиков к customer impact для приоритизации агентом.

8. **Project Templates**
   - Готовые шаблоны под recurring workflows.
   - Автосоздание структур: планы, статусы, правила approval, дефолтные docs.

### P3 — Knowledge/Operations слой

9. **Nested Wiki Pages**
   - Иерархия страниц, backlinks, «docs linked to work».

10. **Time Tracking 2.0**
   - Перевести run/step в человеко-понятные timesheets.
   - План-факт по оценкам + экспорт отчётов.

## Почему такой приоритет

Для сервиса «агенты делают большую часть работы» самый дорогой разрыв сейчас — между внешним хаотичным входом и внутренним исполняемым workflow.

- **Intake + Approvals** напрямую убирают ручные пинги и потери в коммуникациях.
- **Portfolio layer** даёт управляемость без ежедневных статус-митингов.
- **Custom model** нужен после стабилизации потока, чтобы масштабироваться по командам и типам работ.

## Предложение по ближайшему инкременту (2–3 спринта)

1. Спринт 1: Intake Gateway MVP (webhook + form + email parser) + auto-triage.
2. Спринт 2: Approval Orchestrator MVP (1–2 routing templates, SLA, escalation).
3. Спринт 3: Project Health Dashboard v1 + Epic view поверх существующих plans.

Ожидаемый эффект: снижение ручных апдейтов, меньше «потерянных» запросов, ускорение lead time от входящего запроса до первого принятого решения.
