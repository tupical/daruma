//! Clearing `triage_state` through an explicit `null` must survive the event
//! log: the projection rebuilt by replaying `TaskUpdated` equals the live one
//! instead of resurrecting the cleared triage value.

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, NewTask, TaskPatch, TriageState};
use daruma_events::{Event, EventBus, EventEnvelope, EventStore};
use daruma_storage::{ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo};

#[tokio::test]
async fn cleared_triage_stays_cleared_after_replay() {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let handler = CommandHandler::new(
        store.clone(),
        tasks.clone(),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool.clone())),
        EventBus::default(),
    );

    let envs = handler
        .handle(
            Command::CreateTask {
                task: NewTask::new("Triage clear"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let Event::TaskCreated { task } = &envs[0].payload else {
        panic!("expected TaskCreated");
    };
    let id = task.id.unwrap();

    for value in [Some(TriageState::Accepted), None] {
        handler
            .handle(
                Command::UpdateTask {
                    id,
                    patch: TaskPatch {
                        triage_state: Some(value),
                        ..TaskPatch::default()
                    },
                },
                Actor::user(),
            )
            .await
            .unwrap();
    }
    assert!(tasks.get(id).await.unwrap().unwrap().triage_state.is_none());

    // The stored event carries the clear as `null` ...
    let envs: Vec<EventEnvelope> = store.load_since(0, 100_000).await.unwrap();
    let last_patch = envs
        .iter()
        .rev()
        .find_map(|env| match &env.payload {
            Event::TaskUpdated { patch, .. } => Some(patch.clone()),
            _ => None,
        })
        .expect("TaskUpdated in the log");
    assert_eq!(last_patch.triage_state, Some(None));

    // ... and a projection rebuilt from the log agrees with the live one.
    let fresh = Db::memory().await.unwrap();
    fresh.migrate().await.unwrap();
    let replayed = TaskRepo::new(fresh.pool().clone());
    for env in &envs {
        replayed.apply_event(env).await.unwrap();
    }
    let rebuilt = replayed.get(id).await.unwrap().expect("replayed task");
    assert!(
        rebuilt.triage_state.is_none(),
        "replay resurrected triage_state: {:?}",
        rebuilt.triage_state
    );
}
