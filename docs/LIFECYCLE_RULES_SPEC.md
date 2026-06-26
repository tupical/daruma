# Спецификация модели правил жизненного цикла (v1)

- **Статус:** v1 — маппинг сверен с кодом ядра; двойное ревью пройдено
  (code-consistency: approve-with-fixes; coverage: needs-rework → все 17
  находок закрыты правками, 2026-06-11)
- **Дата:** 2026-06-11
- **Контекст:** план «Правила жизненного цикла задач и триггеры» (`019eb654-f391`),
  задача Этапа 1 (`019eb655-92f7`); ADR — your-project/docs/adr-lifecycle-rules.md
- **Источники:** tupical-personal/docs/{manifest,vision,architecture,plan}.md
- **Реализация:** OSS rule engine (`019eb659-daf5`), pre-transition hooks (`019eb659-74e6`),
  evidence registry (`019eb65a-3185`), inheritance (`019eb65a-e5cd`)

## 0. Принцип и анти-цели

Модель строго одна:

```text
событие ЖЦ → проверка правил → требование evidence → allowed | warning | blocked
```

**Анти-цель: это НЕ workflow-builder.** У правила нет действий (actions), нет
side-effects, нет цепочек «if X then do Y». Единственные выходы проверки:
`EnforcementResult` в ответе мутации, событие `RuleFired` в event log и
производная запись Violation. Любое расширение модели, добавляющее правилам
исполняемое поведение, противоречит манифесту и отклоняется на ревью.

Контракт един для OSS и Cloud: Cloud не расширяет enum'ы и не интерпретирует
поля иначе; неизвестные значения отвергаются валидацией ядра.

Чек-лист ревью на соответствие анти-цели (для будущих расширений):
(1) requirement определяет доказательство (Evidence), а не действие;
(2) у Rule нет полей actions/side_effects/webhooks;
(3) requirement не порождает вложенных команд или мутаций;
(4) проверка правила read-only — не меняет другие сущности;
(5) RuleFired/RuleOverridden — только audit-события, не точки запуска логики.

## 1. Типы

Нотация Rust-ориентированная; wire-формат — JSON (snake_case).

```rust
enum RuleMode { Off, Recommendation, Required }

/// Где правило ОПРЕДЕЛЕНО. Run-уровень правил не имеет:
/// run наследует effective rules своей task (или plan для plan-run).
enum RuleScope {
    Tenant,                // = workspace в Cloud (OSS self-hosted: tenant 'self-hosted')
    Project { id: ProjectId },
    Plan    { id: PlanId },
    Task    { id: TaskId },
}

struct Rule {
    id: RuleId,
    rule_key: String,        // стабильный ключ для наследования/переопределения,
                             // напр. "completion-note" или "read-architecture-md"
    title: String,
    scope: RuleScope,
    trigger: TriggerEvent,
    condition: Option<Condition>,
    requirement: Requirement,
    mode: RuleMode,
    message: String,         // что показать исполнителю при warn/block
    override_allowed: bool,  // можно ли (а) ослабить mode ниже по иерархии,
                             // (б) пройти block через force + override_reason
    enabled: bool,
    created_by: Actor, created_at: Timestamp,
    updated_by: Actor, updated_at: Timestamp,
}
```

### 1.1 TriggerEvent — таксономия v1 и маппинг на ядро

Все точки — **pre-persist гейты** в `CommandHandler` (до `persist` в
`handle()`, crates/core/src/handler.rs): для `*.created` — до записи сущности,
для `*.before_*` — до записи событий перехода.

| TriggerEvent           | Команда ядра (гейт)                                   | v1 |
|------------------------|--------------------------------------------------------|----|
| `project.created`      | `CreateProject`                                        | ✓ |
| `plan.created`         | `CreatePlan`                                           | ✓ |
| `plan.before_approve`  | `SetPlanStatus` draft→active                           | ✓ |
| `plan.before_start`    | reserved: в текущей модели совпадает с before_approve  | — |
| `task.created`         | `CreateTask` (и capture-пути, проходящие через него)   | ✓ |
| `task.before_start`    | переход status→`in_progress` ЛЮБЫМ путём: `SetStatus`, claim/drain — проверено: `plan_drain_next` диспатчит `Command::SetStatus` (routes/mod.rs:~3859), гейт в CommandHandler покрывает все пути | ✓ |
| `task.before_complete` | переход в терминальный статус: `SetStatus`(done) И `CompleteTask` — оба пути через один гейт | ✓ |
| `run.created`          | reserved: синоним before_execute в текущей модели      | — |
| `run.before_execute`   | `StartRun`                                             | ✓ |
| `run.before_complete`  | `CompleteRun` (также `FailRun`/`AbortRun` — без гейта, терминальные отказы не блокируются) | ✓ |
| `artifact.created`     | reserved: события `ArtifactRegistered` и проекция (0036) есть, но Command-поверхности артефактов в ядре ещё нет — точка активируется вместе с Artifact Registry (план WorkUnit `019ead4b`, P4) | — |
| `artifact.updated`     | reserved: аналогично — `ArtifactChanged`/`ArtifactWriteCommitted` без командного пути | — |
| `decision.created`     | reserved: сущности Decision в ядре нет; решения v1 живут как Evidence kind=`decision_record` | — |

`reserved`-события входят в enum (валидны при создании правила), но до
реализации соответствующей точки правило на них не срабатывает; UI Cloud
помечает их как «не активно в этой версии».

### 1.2 Condition — таргетирование

Все поля опциональны; пустой condition = правило срабатывает на каждое
событие триггера в своём scope. Семантика: AND между полями, OR внутри списка.

```rust
struct Condition {
    // v1 (реализуемо на текущем ядре):
    status_from: Option<Vec<TaskStatus>>,     // для before_* переходов
    status_to:   Option<Vec<TaskStatus>>,
    priority:    Option<Vec<Priority>>,        // p0..p3
    changed_paths:  Option<Vec<GlobPattern>>,  // матчится по artifact uri,
                                               // work_lease target_uri, reserve_files
    // reserved (в ядре нет носителя; включаются после появления):
    task_labels:      Option<Vec<String>>,     // reserved — у Task нет labels
    affected_modules: Option<Vec<String>>,     // reserved — нет понятия module
    artifact_kinds:   Option<Vec<String>>,     // reserved — у Artifact нет поля kind
                                               // (0036: только status); активируется
                                               // с Command-поверхностью артефактов
}
```

**Reserved-политика:** reserved-поля входят в контракт (имена зарезервированы),
но правило, использующее их, отклоняется валидацией ядра v1 с понятной ошибкой —
правило не должно создавать ложное чувство защиты. Когда носитель появится в
ядре (labels, modules, artifact.kind), поля активируются без смены формата;
до этого `affected_modules` выражается через `changed_paths`.

### 1.3 Requirement и Evidence

`Requirement` — что должно быть доказано; `Evidence` — иммутабельная запись
доказательства (OSS-задача `019eb65a-3185`). Соответствие типов 1:1:

| Requirement.type                | Evidence.kind                  | params (Requirement)                            |
|---------------------------------|--------------------------------|-------------------------------------------------|
| `read_artifact`                 | `document_read_ack`            | `doc_ref`, `min_version` (`latest` \| version)  |
| `create_artifact`               | `artifact_created`             | `artifact_kind`                                 |
| `impact_check`                  | `impact_assessment`            | `target`, `required_fields[]`                   |
| `decision_record`               | `decision_record`              | `required_fields[]`                             |
| `completion_note`               | `completion_note`              | `required_fields[]` (см. §1.4)                  |
| `owner_required`                | `owner_assigned`               | —                                               |
| `acceptance_criteria_required`  | `acceptance_criteria_defined`  | —                                               |
| `risk_check`                    | `risk_check_completed`         | `target`, `required_fields[]`                   |

```rust
struct Evidence {
    id: EvidenceId,
    kind: EvidenceKind,
    // привязки (минимум одна сущностная + rule опционально):
    project_id: ProjectId,
    plan_id: Option<PlanId>, task_id: Option<TaskId>, run_id: Option<RunId>,
    artifact_ref: Option<String>,
    rule_id: Option<RuleId>,        // None для evidence «впрок», вне правила
    document_version: Option<u64>,  // для document_read_ack (entity_versions)
    payload: Json,                  // required_fields → значения
    reason: String,                 // «зачем» (vision.md, правило 5)
    actor: Actor, created_at: Timestamp,
    superseded_by: Option<EvidenceId>, // иммутабельность: только supersede
}
```

**Satisfaction (когда requirement выполнен):** существует не-superseded
Evidence с соответствующим `kind`, привязанный к проверяемой сущности
(task/plan/run) либо к её родителю по цепочке run→task→plan, у которого:
`payload` содержит все `required_fields` непустыми; для `read_artifact` с
`min_version=latest` — `document_version` равен текущей версии документа на
момент проверки (устаревшее прочтение не засчитывается); evidence записан
позже последнего «сброса» (см. §3, инвалидация).

`document_version` заполняется из `entity_versions.version_number`
(migration 0020) в момент записи evidence; доменные сущности кэшированного
поля версии НЕ имеют — проверка «latest» делает lookup последней версии
документа (`version_number DESC LIMIT 1`), а не читает поле сущности.

### 1.4 Completion note

`required_fields` по умолчанию (plan.md, пример 3): `actor`, `completed_at`,
`reason`, `result_summary`, `acceptance_criteria_status`, `related_artifacts`.
Транспорт: опциональное поле `completion_note` в `CompleteTask`/`SetStatus(done)`
(OSS-задача `019eb65a-86d0`); ядро при наличии правила создаёт из него
Evidence kind=`completion_note` атомарно с переходом.

### 1.5 EnforcementResult и wire-контракт

```rust
enum Decision { Allowed, Warning, Blocked }

struct RuleCheckOutcome { rule_id, rule_key, decision, message, requirement }

struct EnforcementResult {           // агрегат по всем сработавшим правилам
    decision: Decision,              // max(blocked > warning > allowed)
    outcomes: Vec<RuleCheckOutcome>,
}
```

- `allowed` — мутация проходит, ответ без изменений.
- `warning` — мутация проходит; в ответе мутации (HTTP и MCP) добавляется
  `rule_warnings: [{rule_id, rule_key, message}]`. Это поле опциональное —
  существующие клиенты не ломаются.
- `blocked` — мутация НЕ выполняется; ошибка с кодом `rule_blocked`,
  телом `{rule_id, rule_key, message, requirement}` — исполнителю понятно,
  что сделать, чтобы пройти.
- Несколько сработавших правил: `outcomes[]` содержит ВСЕ результаты
  (blocked — первыми, затем warnings; внутри группы — по уровню scope от
  ближнего к дальнему, далее по `rule_key`). Для прохода нужно удовлетворить
  все blocked-правила; тело ошибки несёт полный список, чтобы агент/UI
  показали все требования сразу, а не по одному.

**Override:** если `override_allowed=true`, мутация с `force=true` +
непустым `override_reason` проходит сквозь `blocked`; ядро пишет событие
`RuleOverridden {rule_id, actor, reason}`. Поле `SetStatus.force` уже
существует и документировано как мягкий обход can_start-блокеров
(api-dto/src/command.rs:36-43); rules-override расширяет ту же семантику с
ужесточением: для прохода required-правила `force` без `override_reason`
НЕ работает — молчаливый force пропускает только can_start-предупреждение,
но не правило.

Уточнения: override применим только к `blocked` (warning не блокирует и
override не требует); если среди blocked есть хотя бы одно правило с
`override_allowed=false`, мутация отклоняется независимо от force;
can_start-семантика force остаётся отдельным, совместимым путём. Сегодня
handler `force` не инспектирует (handler.rs:898-906) — вся описанная
семантика реализуется вместе с гейтом (задача `019eb659-74e6`).

### 1.6 RuleFired / Violation

Каждая проверка с решением warning/blocked/overridden пишет в event log:

```text
RuleFired { rule_id, rule_key, trigger_event, decision, actor, entity_ref, occurred_at }
RuleOverridden { rule_id, actor, reason, entity_ref, occurred_at }
```

`allowed` НЕ логируется (шум). Violation — производное понятие (не отдельная
сущность): проекция `rule_firings` строится из этих событий; «проигнорированная
рекомендация» = RuleFired(warning), за которым мутация состоялась. Журнал
срабатываний и панель нарушений Cloud читают эту проекцию/события через
`events_since`/webhooks — отдельного cloud-хранилища нет (ADR).

## 2. Наследование и переопределение

Уровни определения: `tenant → project → plan → task`; run наследует от task
(plan-run — от plan). **Маппинг Cloud:** workspace Cloud = tenant OSS
(migration 0024); self-hosted OSS — один tenant `self-hosted`, т.е.
tenant-правила там играют роль «правил инсталляции».

Effective rules для сущности E:

1. Собрать правила всех уровней цепочки E (tenant, project, plan, task).
2. Сгруппировать по `rule_key`; правило нижнего уровня **переопределяет**
   правило того же `rule_key` верхнего уровня.
3. Политика ослабления: переопределение, понижающее строгость
   (`required → recommendation/off`), допустимо только если у
   переопределяемого (родительского) правила `override_allowed = true`.
   Усиление (`off → recommendation → required`) допустимо всегда.
4. `enabled=false` у родителя = правило не участвует (но `rule_key` нижнего
   уровня может ввести своё).

Конфликт одного `rule_key` на одном уровне запрещён (уникальный индекс
`(scope, rule_key)`).

Граничные случаи:
- `enabled=false` у родителя не мешает дочернему уровню ввести собственное
  правило с тем же `rule_key` (это усиление — разрешено всегда);
- выключение унаследованного правила дочерним уровнем — это ослабление до
  `off` и подчиняется той же политике: требует `override_allowed=true` у
  родительского правила;
- политика ослабления применяется к `mode` и `enabled` одинаково.

## 3. Инварианты

1. **Никаких действий у правил.** Выход проверки — только
   EnforcementResult + RuleFired/RuleOverridden + проекция. (анти-Zapier)
2. **off не вычисляется**; правило `enabled=false` не загружается в проверку.
3. **Zero-cost без правил:** effective-rules per scope кэшируются; при пустом
   наборе гейт — один lookup в кэш, ноль SQL на горячем пути. Инвалидация
   синхронна с persist Rule*-события: сбрасывается кэш scope правила и всех
   дочерних scope. Гонки исключены архитектурно: workspace обслуживается
   единственным in-process AppState (WorkspaceRouter держит один handle на
   workspace), распределённых копий кэша нет.
4. **Evidence иммутабелен:** правка запрещена, supersede разрешён; satisfaction
   учитывает только не-superseded записи.
5. **Каждый warn/block/override оставляет след** (RuleFired/RuleOverridden) с
   actor из EventEnvelope.
6. **Мутации правил и evidence — только event-sourced** через Command
   (RuleCreated/RuleUpdated/RuleDisabled, EvidenceRecorded, EvidenceSuperseded);
   прямой SQL запрещён (ADR).
7. **Гейт стоит до persist** и покрывает ВСЕ пути перехода (SetStatus,
   CompleteTask, drain) через общую точку — нельзя «обойти» правило другим
   эндпойнтом. Нюанс drain: `AcquireClaim` диспатчится ДО `SetStatus`
   (routes/mod.rs:3840-3860), а claim сам по себе не гейтится (claim — не
   переход состояния). Требование к реализации (`019eb659-74e6`): при
   blocked-переходе drain обязан компенсировать — освободить claim
   (`ReleaseClaim`), иначе задача останется заклеймленной без работы.
8. **Детерминизм:** проверка не делает сетевых вызовов и не зависит от
   времени, кроме сравнения версий/наличия evidence.

## 4. Хранение и API (эскиз, детали — в задачах реализации)

- Таблицы (per-workspace OSS SQLite, миграции `crates/storage/migrations/`):
  `lifecycle_rules` (колоночная, по образцу artifact registry; уникальный
  индекс `(scope_kind, scope_id, rule_key)`), `evidence`,
  `rule_firings` (проекция RuleFired/RuleOverridden).
- События: `RuleCreated/RuleUpdated/RuleDisabled`, `EvidenceRecorded/
  EvidenceSuperseded`, `RuleFired/RuleOverridden` — в общий event log.
- HTTP: `GET/POST/PATCH /v1/rules` (tenant-scope),
  `GET/POST/PATCH /v1/projects/{id}/rules` (+ plan/task scope через query
  или вложенные пути) — по образцу project_settings (Command-based).
- MCP: чтение effective rules + запись evidence — в default-профиль
  (агенту нужно видеть, что от него требуют, и оставлять след);
  CRUD правил — full/admin (согласовать с планом `019ead63`).
- RBAC мутаций правил — на стороне Cloud (`require_rules_manage()`, ADR §2.3).
- Cloud НЕ хранит копию правил в своей БД: кабинет читает и пишет их напрямую
  в workspace через `/v1` (ADR) — задачи «синхронизации Cloud↔OSS» не
  существует by design; self-hosted OSS управляет правилами тем же API.

## 5. Примеры (правила 1–3 из plan.md в формате v1)

```json
{
  "rule_key": "read-architecture-md",
  "title": "Перед утверждением плана прочитать architecture.md",
  "scope": {"project": "your-project"},
  "trigger": "plan.before_approve",
  "requirement": {"type": "read_artifact",
                  "doc_ref": "architecture.md", "min_version": "latest"},
  "mode": "required", "override_allowed": false,
  "message": "Перед утверждением плана подтвердите чтение актуальной версии architecture.md."
}
```

```json
{
  "rule_key": "auth-impact-check",
  "title": "Проверка влияния задачи на модуль авторизации",
  "scope": {"project": "your-project"},
  "trigger": "task.before_start",
  "condition": {"changed_paths": ["crates/auth/**", "**/users/**", "**/permissions/**"]},
  "requirement": {"type": "impact_check", "target": "auth-module",
    "required_fields": ["affected_components","risk_level","migration_needed","tests_needed","reasoning"]},
  "mode": "required", "override_allowed": true,
  "message": "Перед стартом задачи нужно проверить влияние изменений на авторизацию, пользователей и права доступа."
}
```

```json
{
  "rule_key": "completion-note",
  "title": "Запрет завершения задачи без who/when/why",
  "scope": {"tenant": true},
  "trigger": "task.before_complete",
  "requirement": {"type": "completion_note",
    "required_fields": ["actor","completed_at","reason","result_summary",
                        "acceptance_criteria_status","related_artifacts"]},
  "mode": "required", "override_allowed": true,
  "message": "Задачу нельзя завершить без отметки: кто, когда, зачем, что сделано и почему результат принят."
}
```

Примечания к примерам:
- пример 2: условие исходного примера (`affected_modules: [auth, users,
  permissions]`) в v1 выражается через `changed_paths` — носителя «модулей»
  в ядре нет (см. reserved-политику §1.2);
- нормализация имён: в plan.md пример 1 называет requirement именем evidence
  (`document_read_ack`); в v1 requirement.type = `read_artifact`, а
  `document_read_ack` — kind его evidence (таблица соответствия §1.3).

## 6. Покрытие правил манифеста (vision.md 1–15)

| vision-правило | механизм v1 |
|---|---|
| 1 цель задачи | Cloud-валидация (UI/шаблон): непустая цель в description; отдельного requirement-типа в ядре v1 НЕТ (кандидат v2: `description_required`) |
| 2 критерии завершения | `acceptance_criteria_required` на `task.before_start`/`before_complete` |
| 3 контекст задачи | Cloud-шаблон: `task.before_start` + `read_artifact`/`impact_check` |
| 4–5 исполнитель и причина действия | EventEnvelope.actor (есть всегда) + Evidence.reason |
| 6 документ привязан к задаче | OSS doc↔task binding (`019eb65b-4cc2`) + правило на `artifact.created` |
| 7–8 триггер и потребитель документа | носитель — метаданные документа (OSS-задача `019eb65b-4cc2`); проверка — Cloud-шаблон (Cloud-only в v1, ядро поля не валидирует) |
| 9 ЖЦ артефакта | статусы документов/артефактов (`019eb65b-4cc2`, registry 0036) |
| 10–11 завершение с результатом и who/when/why | `completion_note` (пример 3) |
| 12 решения фиксируются | Evidence kind=`decision_record` (+ reserved `decision.created`) |
| 13 допущения явные | Cloud-шаблоны задают `required_fields` с полем `assumptions` для `decision_record`/`impact_check`; ядро конкретный состав полей не фиксирует |
| 14 блокер имеет владельца | `owner_required` + существующий Relation::Blocks |
| 15 формализованная передача | Cloud-шаблон handoff на `task.before_start` после смены исполнителя; синергия с P5 Handoff contracts (`019ead4c-dc63`) — на общем гейте |

## 7. Открытые вопросы (не блокируют v1)

- `plan.before_start` как отдельная точка — нужна ли двухфазность
  draft→approved→active у планов (продуктовое решение Cloud).
- Decision как полноценная сущность ядра (сейчас — Evidence).
- `task_labels`/`affected_modules` в Condition — ждут носителя в ядре.
- Command-поверхность артефактов (план WorkUnit `019ead4b`, P4) — её
  появление активирует триггеры `artifact.*`.
