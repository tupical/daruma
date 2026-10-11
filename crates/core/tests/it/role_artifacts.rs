//! ADR-0011 п.3: structured role results as registry artifacts. Generation
//! per (task, role, kind), a separate role status axis with only
//! `proposed → applied | rejected | superseded`, and the programmatic query
//! "all kind=X status=proposed for task N".

use std::sync::Arc;

use daruma_core::{Command, CommandHandler};
use daruma_domain::{
    Actor, ArtifactRelationKind, ArtifactStatus, NewArtifact, NewTask, RoleArtifactStatus,
};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{ArtifactId, TaskId};
use daruma_storage::{
    ActivityRepo, ArtifactRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo,
};
use serde_json::json;

async fn build_stack() -> (CommandHandler, Arc<ArtifactRepo>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let artifacts = Arc::new(ArtifactRepo::new(pool.clone()));
    let handler = CommandHandler::new(
        store,
        Arc::new(TaskRepo::new(pool.clone())),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool)),
        EventBus::default(),
    )
    .with_artifacts(artifacts.clone());
    (handler, artifacts)
}

async fn create_task(handler: &CommandHandler) -> TaskId {
    let envs = handler
        .handle(
            Command::CreateTask {
                task: NewTask::new("Role artifacts"),
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

fn propose(task_id: TaskId, role: &str, kind: &str, supersedes: Option<ArtifactId>) -> Command {
    Command::ProposeRoleArtifact {
        task_id,
        role: role.into(),
        kind: kind.into(),
        payload: json!({"file": "src/lib.rs", "anchor": "fn main", "diff": "+x"}),
        title: None,
        supersedes,
        artifact_id: None,
    }
}

async fn propose_ok(
    handler: &CommandHandler,
    task_id: TaskId,
    role: &str,
    kind: &str,
    supersedes: Option<ArtifactId>,
) -> ArtifactId {
    let envs = handler
        .handle(propose(task_id, role, kind, supersedes), Actor::agent(role))
        .await
        .unwrap();
    match &envs[0].payload {
        Event::ArtifactRegistered { artifact } => artifact.id,
        other => panic!("expected ArtifactRegistered, got: {other:?}"),
    }
}

fn set(artifact_id: ArtifactId, to: RoleArtifactStatus) -> Command {
    Command::SetRoleArtifactStatus { artifact_id, to }
}

async fn role_status(repo: &ArtifactRepo, id: ArtifactId) -> RoleArtifactStatus {
    repo.get(id)
        .await
        .unwrap()
        .unwrap()
        .role_artifact
        .unwrap()
        .status
}

#[tokio::test]
async fn parallel_drafts_get_own_generations_and_stay_proposed() {
    let (handler, repo) = build_stack().await;
    let task = create_task(&handler).await;
    let a = propose_ok(&handler, task, "draft", "patch", None).await;
    let b = propose_ok(&handler, task, "draft", "patch", None).await;

    let rows = repo
        .list_role_artifacts(
            task,
            None,
            Some("patch"),
            Some(RoleArtifactStatus::Proposed),
        )
        .await
        .unwrap();
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![a, b]);
    let gens: Vec<i64> = rows
        .iter()
        .map(|r| r.role_artifact.as_ref().unwrap().generation)
        .collect();
    assert_eq!(gens, vec![1, 2]);

    let first = &rows[0];
    assert_eq!(
        first.uri,
        format!("artifact://task/{task}/role/draft/patch/1")
    );
    assert_eq!(first.task_id, Some(task));
    assert_eq!(first.status, ArtifactStatus::Committed);
    let role = first.role_artifact.as_ref().unwrap();
    assert_eq!(role.role, "draft");
    assert_eq!(role.payload["anchor"], "fn main");
}

#[tokio::test]
async fn query_by_task_kind_status_returns_exact_set() {
    let (handler, repo) = build_stack().await;
    let task = create_task(&handler).await;
    let other_task = create_task(&handler).await;

    let p1 = propose_ok(&handler, task, "draft", "patch", None).await;
    let p2 = propose_ok(&handler, task, "draft", "patch", None).await;
    let review = propose_ok(&handler, task, "reviewer", "review", None).await;
    let _foreign = propose_ok(&handler, other_task, "draft", "patch", None).await;
    handler
        .handle(
            set(p2, RoleArtifactStatus::Applied),
            Actor::agent("builder"),
        )
        .await
        .unwrap();

    let proposed_patches = repo
        .list_role_artifacts(
            task,
            None,
            Some("patch"),
            Some(RoleArtifactStatus::Proposed),
        )
        .await
        .unwrap();
    assert_eq!(
        proposed_patches.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![p1]
    );

    let applied = repo
        .list_role_artifacts(task, None, None, Some(RoleArtifactStatus::Applied))
        .await
        .unwrap();
    assert_eq!(applied.iter().map(|a| a.id).collect::<Vec<_>>(), vec![p2]);

    let by_role = repo
        .list_role_artifacts(task, Some("reviewer"), None, None)
        .await
        .unwrap();
    assert_eq!(
        by_role.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![review]
    );

    assert_eq!(
        repo.list_role_artifacts(task, None, None, None)
            .await
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn supersedes_retires_previous_generation_and_links_it() {
    let (handler, repo) = build_stack().await;
    let task = create_task(&handler).await;
    let g1 = propose_ok(&handler, task, "builder", "patch", None).await;
    let envs = handler
        .handle(
            propose(task, "builder", "patch", Some(g1)),
            Actor::agent("builder"),
        )
        .await
        .unwrap();
    let g2 = match &envs[0].payload {
        Event::ArtifactRegistered { artifact } => {
            assert_eq!(artifact.role_artifact.as_ref().unwrap().generation, 2);
            artifact.id
        }
        other => panic!("expected ArtifactRegistered, got: {other:?}"),
    };
    assert!(envs.iter().any(|e| matches!(
        &e.payload,
        Event::RoleArtifactStatusChanged { artifact_id, from: RoleArtifactStatus::Proposed, to: RoleArtifactStatus::Superseded, .. } if *artifact_id == g1
    )));

    assert_eq!(role_status(&repo, g1).await, RoleArtifactStatus::Superseded);
    assert_eq!(role_status(&repo, g2).await, RoleArtifactStatus::Proposed);
    let rels = repo.relations_for(g2).await.unwrap();
    assert_eq!(rels.len(), 1);
    assert_eq!(
        (rels[0].from_id, rels[0].to_id, rels[0].kind),
        (g2, g1, ArtifactRelationKind::Supersedes)
    );

    // A superseded generation cannot be superseded again; the generation
    // counter is not consumed by the failed attempt.
    let err = handler
        .handle(
            propose(task, "builder", "patch", Some(g1)),
            Actor::agent("builder"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("only proposed"), "{err}");
    let g3 = propose_ok(&handler, task, "builder", "patch", Some(g2)).await;
    assert_eq!(
        repo.get(g3)
            .await
            .unwrap()
            .unwrap()
            .role_artifact
            .unwrap()
            .generation,
        3
    );
}

#[tokio::test]
async fn supersedes_must_point_at_same_task_role_kind() {
    let (handler, _repo) = build_stack().await;
    let task = create_task(&handler).await;
    let draft = propose_ok(&handler, task, "draft", "patch", None).await;
    for cmd in [
        propose(task, "builder", "patch", Some(draft)),
        propose(task, "draft", "review", Some(draft)),
        propose(create_task(&handler).await, "draft", "patch", Some(draft)),
    ] {
        let err = handler.handle(cmd, Actor::agent("x")).await.unwrap_err();
        assert!(err.to_string().contains("is not a"), "{err}");
    }
}

#[tokio::test]
async fn invalid_transitions_are_rejected() {
    let (handler, repo) = build_stack().await;
    let task = create_task(&handler).await;
    let a = propose_ok(&handler, task, "draft", "patch", None).await;
    handler
        .handle(
            set(a, RoleArtifactStatus::Rejected),
            Actor::agent("builder"),
        )
        .await
        .unwrap();
    // Repeating the current status is a no-op.
    let envs = handler
        .handle(
            set(a, RoleArtifactStatus::Rejected),
            Actor::agent("builder"),
        )
        .await
        .unwrap();
    assert!(envs.is_empty());
    for to in [
        RoleArtifactStatus::Proposed,
        RoleArtifactStatus::Applied,
        RoleArtifactStatus::Superseded,
    ] {
        let err = handler
            .handle(set(a, to), Actor::agent("builder"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }
    assert_eq!(role_status(&repo, a).await, RoleArtifactStatus::Rejected);
}

#[tokio::test]
async fn plain_registry_artifacts_are_untouched() {
    let (handler, repo) = build_stack().await;
    let task = create_task(&handler).await;
    let envs = handler
        .handle(
            Command::RegisterArtifact {
                artifact: NewArtifact {
                    id: None,
                    uri: "artifact://api/users".into(),
                    title: "Users API".into(),
                    description: None,
                    task_id: Some(task),
                    project_id: None,
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let plain = match &envs[0].payload {
        Event::ArtifactRegistered { artifact } => artifact.id,
        other => panic!("expected ArtifactRegistered, got: {other:?}"),
    };
    let row = repo.get(plain).await.unwrap().unwrap();
    assert!(row.role_artifact.is_none());
    assert_eq!(row.status, ArtifactStatus::Pending);
    assert!(repo
        .list_role_artifacts(task, None, None, None)
        .await
        .unwrap()
        .is_empty());
    let err = handler
        .handle(set(plain, RoleArtifactStatus::Applied), Actor::user())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a role artifact"), "{err}");
    let _ = propose_ok(&handler, task, "draft", "patch", None).await;
    assert_eq!(repo.list(None, Some(task), None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn propose_validates_input() {
    let (handler, _repo) = build_stack().await;
    let task = create_task(&handler).await;
    for (role, kind) in [("", "patch"), ("draft", " "), ("dr/aft", "patch")] {
        let err = handler
            .handle(propose(task, role, kind, None), Actor::agent("x"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("role artifact"), "{err}");
    }
    let err = handler
        .handle(
            propose(TaskId::new(), "draft", "patch", None),
            Actor::agent("x"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}
