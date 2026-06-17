//! Integration tests for §3.5 backfill policy (T9 decision).
//!
//! Verifies:
//! * Historical tasks (rows inserted without actor columns, simulating
//!   pre-`0010_tasks_actors.sql` rows) have `created_by = None` and
//!   `completed_by = None` — no automatic backfill occurs.
//! * New tasks created via `TaskRepo::apply_event(TaskCreated)` after
//!   the migration get `created_by` populated from the event actor.
//! * Completing a task via `TaskCompleted` event populates `completed_by`.
//! * Creating new tasks does NOT mutate historical rows.

use std::sync::Arc;

use taskagent_domain::{Actor, NewTask};
use taskagent_events::{Event, EventEnvelope, EventStore};
use taskagent_shared::{AgentId, TaskId};
use taskagent_storage::{Db, SqliteEventStore, TaskRepo};

// ── helpers ───────────────────────────────────────────────────────────────────

async fn build_stack() -> (Arc<TaskRepo>, Arc<dyn EventStore>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool));
    (tasks, store)
}

/// Apply a batch of envelopes: persist them then replay into the TaskRepo.
async fn apply(store: &Arc<dyn EventStore>, tasks: &Arc<TaskRepo>, envelopes: Vec<EventEnvelope>) {
    let persisted = store.append_batch(envelopes).await.unwrap();
    for env in &persisted {
        tasks.apply_event(env).await.unwrap();
    }
}

// ── Historical row (pre-migration simulation) ─────────────────────────────────

/// Insert a task row directly into SQLite without actor columns.
/// This simulates a row that existed before `0010_tasks_actors.sql` was applied.
async fn insert_legacy_row(pool: &sqlx::SqlitePool, task_id: TaskId) {
    let now = taskagent_shared::time::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO tasks \
         (id, title, status, priority, description, created_at, updated_at) \
         VALUES (?, 'Legacy task', 'todo', 'p2', '', ?, ?)",
    )
    .bind(task_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Scenario 1: A legacy row has NULL actors — TaskRepo returns created_by: None.
#[tokio::test]
async fn historical_task_has_null_actors() {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let tasks = Arc::new(TaskRepo::new(pool.clone()));

    let task_id = TaskId::new();
    insert_legacy_row(&pool, task_id).await;

    let task = tasks
        .get(task_id)
        .await
        .unwrap()
        .expect("legacy task must be readable");

    assert!(
        task.created_by.is_none(),
        "historical task must have created_by = None (backfill policy: do not touch past)"
    );
    assert!(
        task.completed_by.is_none(),
        "historical task must have completed_by = None"
    );
}

/// Scenario 2: A new task created via apply_event has created_by populated.
#[tokio::test]
async fn new_task_gets_created_by_from_event_actor() {
    let (tasks, store) = build_stack().await;
    let task_id = TaskId::new();
    let actor = Actor::Agent {
        id: AgentId::new(),
        name: "bot.test-agent".into(),
    };

    let mut new_task = NewTask::new("New task after migration");
    new_task.id = Some(task_id);

    apply(
        &store,
        &tasks,
        vec![EventEnvelope::new(
            actor.clone(),
            Event::TaskCreated { task: new_task },
        )],
    )
    .await;

    let task = tasks.get(task_id).await.unwrap().expect("task must exist");
    assert_eq!(
        task.created_by.as_ref(),
        Some(&actor),
        "new task must have created_by set from the event actor"
    );
    assert!(
        task.completed_by.is_none(),
        "newly created task must not have completed_by set"
    );
}

/// Scenario 3: Completing a task sets completed_by from the completing actor.
#[tokio::test]
async fn completing_task_populates_completed_by() {
    let (tasks, store) = build_stack().await;
    let task_id = TaskId::new();
    let creator = Actor::User;
    let completer = Actor::Agent {
        id: AgentId::new(),
        name: "bot.completer".into(),
    };

    // Create the task.
    let mut new_task = NewTask::new("Task to complete");
    new_task.id = Some(task_id);
    apply(
        &store,
        &tasks,
        vec![EventEnvelope::new(
            creator.clone(),
            Event::TaskCreated { task: new_task },
        )],
    )
    .await;

    // Complete the task with a different actor.
    let now = taskagent_shared::time::now();
    apply(
        &store,
        &tasks,
        vec![EventEnvelope::new(
            completer.clone(),
            Event::TaskCompleted {
                task_id,
                completed_at: now,
                completion_note: None,
            },
        )],
    )
    .await;

    let task = tasks.get(task_id).await.unwrap().unwrap();
    assert_eq!(
        task.completed_by.as_ref(),
        Some(&completer),
        "completed_by must be set to the completing actor"
    );
    // created_by is unchanged.
    assert_eq!(
        task.created_by.as_ref(),
        Some(&creator),
        "created_by must remain the creator actor after completion"
    );
}

/// Scenario 4: Creating new tasks does NOT mutate historical rows (no backfill).
#[tokio::test]
async fn no_backfill_of_historical_rows_when_new_tasks_created() {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));

    // Insert a legacy row.
    let legacy_id = TaskId::new();
    insert_legacy_row(&pool, legacy_id).await;

    // Create a new task with an explicit actor.
    let new_id = TaskId::new();
    let actor = Actor::Agent {
        id: AgentId::new(),
        name: "bot.new".into(),
    };
    let mut new_task = NewTask::new("Post-migration task");
    new_task.id = Some(new_id);
    apply(
        &store,
        &tasks,
        vec![EventEnvelope::new(
            actor.clone(),
            Event::TaskCreated { task: new_task },
        )],
    )
    .await;

    // Legacy row must remain unmodified — no backfill.
    let legacy = tasks
        .get(legacy_id)
        .await
        .unwrap()
        .expect("legacy task must still exist");
    assert!(
        legacy.created_by.is_none(),
        "legacy row must not be backfilled after new tasks are created"
    );

    // New row must have the actor.
    let new_task = tasks
        .get(new_id)
        .await
        .unwrap()
        .expect("new task must exist");
    assert!(
        new_task.created_by.is_some(),
        "new task must have created_by populated"
    );
}
