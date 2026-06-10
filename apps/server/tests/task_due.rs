//! Due-date watchdog (`task.due`): `tick_due_tasks` emits exactly one
//! `TaskDueElapsed` per (task, due_at) value for overdue active tasks.

use chrono::{Duration, Utc};
use taskagent_core::Command;
use taskagent_domain::{Actor, NewTask};
use taskagent_events::Event;
use taskagent_shared::TaskId;

mod common;
use common::test_app;

async fn create_task(app: &common::TestApp, title: &str, due_in: Option<Duration>) -> TaskId {
    let id = TaskId::new();
    let mut task = NewTask::new(title);
    task.id = Some(id);
    task.due_at = due_in.map(|d| Utc::now() + d);
    app.state
        .commands
        .handler()
        .handle(Command::CreateTask { task }, Actor::user())
        .await
        .expect("create task");
    id
}

#[tokio::test]
async fn overdue_active_task_emits_task_due_exactly_once() {
    let app = test_app().await;
    let handler = app.state.commands.handler();

    let overdue = create_task(&app, "overdue", Some(Duration::seconds(-60))).await;
    let _future = create_task(&app, "not due yet", Some(Duration::hours(1))).await;
    let _no_due = create_task(&app, "no deadline", None).await;

    let mut rx = app.bus.subscribe();

    let emitted = handler.tick_due_tasks(Utc::now()).await.expect("tick");
    assert_eq!(emitted, 1, "only the overdue task fires");

    let env = rx.recv().await.expect("event on bus");
    assert_eq!(env.payload.kind(), "task.due");
    match &env.payload {
        Event::TaskDueElapsed { task_id, .. } => assert_eq!(*task_id, overdue),
        other => panic!("unexpected event: {other:?}"),
    }

    // Second tick: deduped by the task_due_notifications projection.
    let again = handler.tick_due_tasks(Utc::now()).await.expect("tick 2");
    assert_eq!(again, 0, "no duplicate notification for the same deadline");
}

#[tokio::test]
async fn closed_tasks_do_not_fire() {
    let app = test_app().await;
    let handler = app.state.commands.handler();

    let id = create_task(&app, "done and overdue", Some(Duration::seconds(-60))).await;
    handler
        .handle(Command::CompleteTask { id }, Actor::user())
        .await
        .expect("complete");

    let emitted = handler.tick_due_tasks(Utc::now()).await.expect("tick");
    assert_eq!(emitted, 0, "terminal statuses are exempt");
}

#[tokio::test]
async fn moving_the_deadline_rearms_the_notification() {
    let app = test_app().await;
    let handler = app.state.commands.handler();

    let id = create_task(&app, "slipping task", Some(Duration::seconds(-60))).await;
    assert_eq!(handler.tick_due_tasks(Utc::now()).await.unwrap(), 1);

    // Push the deadline into the past again with a *different* value:
    // the (task, due_at) pair changed, so the watchdog fires once more.
    let patch = serde_json::json!({ "due_at": (Utc::now() - Duration::seconds(30)).to_rfc3339() });
    let patch = serde_json::from_value(patch).expect("patch shape");
    handler
        .handle(Command::UpdateTask { id, patch }, Actor::user())
        .await
        .expect("update due_at");

    assert_eq!(
        handler.tick_due_tasks(Utc::now()).await.unwrap(),
        1,
        "changed deadline re-arms task.due"
    );
    assert_eq!(handler.tick_due_tasks(Utc::now()).await.unwrap(), 0);
}
