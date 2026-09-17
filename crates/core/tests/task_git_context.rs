//! Git work context on a task: set/replace/clear through `update_task`,
//! validated once in the handler, projected into the task row, and
//! independent of the plan-ownership gate (it is execution-owned like
//! `due_at`).

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{Actor, GitContext, NewTask, TaskPatch};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::TaskId;
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo,
};

async fn build_stack() -> (CommandHandler, Arc<TaskRepo>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool.clone()));
    let handler =
        CommandHandler::new(store, tasks.clone(), projects, comments, activity, EventBus::default());
    (handler, tasks)
}

async fn create_task(handler: &CommandHandler) -> TaskId {
    let envs = handler
        .handle(
            Command::CreateTask {
                task: NewTask::new("Git context"),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::TaskCreated { task } => task.id.unwrap(),
        other => panic!("expected TaskCreated, got: {other:?}"),
    }
}

fn patch(ctx: Option<GitContext>) -> TaskPatch {
    TaskPatch {
        git_context: Some(ctx),
        ..TaskPatch::default()
    }
}

#[tokio::test]
async fn git_context_is_set_replaced_whole_and_cleared() {
    let (handler, tasks) = build_stack().await;
    let id = create_task(&handler).await;
    assert!(tasks.get(id).await.unwrap().unwrap().git_context.is_none());

    let ctx = GitContext {
        repo: Some(" tupical/mcpbox.ru ".into()),
        branch: Some("work/01a0a96a".into()),
        head_sha: Some("6E840C6".into()),
        mr_url: None,
    };
    let envs = handler
        .handle(Command::UpdateTask { id, patch: patch(Some(ctx)) }, Actor::user())
        .await
        .unwrap();
    // The event carries the normalised context, so every projection agrees.
    let Event::TaskUpdated { patch: emitted, .. } = &envs[0].payload else {
        panic!("expected TaskUpdated");
    };
    let emitted = emitted.git_context.clone().flatten().expect("context in event");
    assert_eq!(emitted.repo.as_deref(), Some("tupical/mcpbox.ru"));
    assert_eq!(emitted.head_sha.as_deref(), Some("6e840c6"));

    let stored = tasks.get(id).await.unwrap().unwrap().git_context.expect("projected");
    assert_eq!(stored, emitted);

    // Replace whole: a patch with only mr_url drops the earlier branch/sha.
    handler
        .handle(
            Command::UpdateTask {
                id,
                patch: patch(Some(GitContext {
                    mr_url: Some("https://github.com/tupical/mcpbox.ru/pull/7".into()),
                    ..GitContext::default()
                })),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let stored = tasks.get(id).await.unwrap().unwrap().git_context.unwrap();
    assert!(stored.branch.is_none() && stored.head_sha.is_none());
    assert_eq!(stored.mr_url.as_deref(), Some("https://github.com/tupical/mcpbox.ru/pull/7"));

    // Clear.
    handler
        .handle(Command::UpdateTask { id, patch: patch(None) }, Actor::user())
        .await
        .unwrap();
    assert!(tasks.get(id).await.unwrap().unwrap().git_context.is_none());
}

#[tokio::test]
async fn git_context_is_validated_and_is_execution_owned() {
    let (handler, _tasks) = build_stack().await;
    let id = create_task(&handler).await;
    // Ownership gate is a runtime flag; creation above ran with it off.
    let handler = handler.with_plan_only_intake(true);

    for (ctx, needle) in [
        (GitContext::default(), "at least one"),
        (
            GitContext { head_sha: Some("xyz".into()), ..GitContext::default() },
            "head_sha",
        ),
        (
            GitContext { mr_url: Some("javascript:alert(1)".into()), ..GitContext::default() },
            "mr_url",
        ),
        (
            GitContext { branch: Some("bad\nbranch".into()), ..GitContext::default() },
            "control",
        ),
    ] {
        let err = handler
            .handle(Command::UpdateTask { id, patch: patch(Some(ctx)) }, Actor::user())
            .await
            .unwrap_err();
        assert!(err.to_string().contains(needle), "{err}");
    }

    // Under plan-only intake update_task still accepts git_context alone
    // (execution-owned), while a plan-owned field in the same patch is refused.
    handler
        .handle(
            Command::UpdateTask {
                id,
                patch: patch(Some(GitContext { branch: Some("work/x".into()), ..GitContext::default() })),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let err = handler
        .handle(
            Command::UpdateTask {
                id,
                patch: TaskPatch {
                    title: Some("renamed".into()),
                    ..patch(Some(GitContext { branch: Some("work/y".into()), ..GitContext::default() }))
                },
            },
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("plan_owned_immutable"), "{err}");
}
