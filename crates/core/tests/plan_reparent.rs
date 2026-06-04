//! Integration tests for UpdatePlan re-parenting + cycle-detection (W1).
//!
//! Covers:
//! - Valid re-parent succeeds and emits PlanUpdated.
//! - Unparent (explicit null) succeeds.
//! - Self-parent is rejected with a Validation error.
//! - Ancestor-cycle is rejected with a Validation error.

use std::sync::Arc;

use taskagent_core::{Command, CommandHandler};
use taskagent_domain::{Actor, NewPlan, PlanPatch};
use taskagent_events::{Event, EventBus, EventStore};
use taskagent_shared::{CoreError, PlanId, ProjectId};
use taskagent_storage::{
    ActivityRepo, CommentRepo, Db, PlanRepo, ProjectRepo, RelationRepo, SqliteEventStore, TaskRepo,
};

use taskagent_core::repos::PlanRepository;

// ── Stack builder ──────────────────────────────────────────────────────────────

async fn build_stack() -> (CommandHandler, Arc<PlanRepo>) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();

    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let activity = Arc::new(ActivityRepo::new(pool.clone()));
    let relations = Arc::new(RelationRepo::new(pool.clone()));
    let plans = Arc::new(PlanRepo::new(pool.clone()));
    let bus = EventBus::default();

    let handler = CommandHandler::new(store, tasks, projects, comments, activity, bus)
        .with_relations(relations)
        .with_plans(plans.clone() as Arc<dyn PlanRepository>);

    (handler, plans)
}

// ── Helpers ────────────────────────────────────────────────────────────────────

async fn create_plan(handler: &CommandHandler, project_id: ProjectId) -> PlanId {
    let envs = handler
        .handle(
            Command::CreatePlan {
                plan: NewPlan::new("Plan", project_id, Actor::user()),
                external_ref: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    match &envs[0].payload {
        Event::PlanCreated { plan } => plan.id,
        _ => panic!("expected PlanCreated"),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// Valid re-parent: child.parent = parent → succeeds, PlanUpdated emitted.
#[tokio::test]
async fn update_plan_valid_reparent() {
    let (handler, _plans) = build_stack().await;
    let project_id = ProjectId::new();

    let parent_id = create_plan(&handler, project_id).await;
    let child_id = create_plan(&handler, project_id).await;

    let result = handler
        .handle(
            Command::UpdatePlan {
                id: child_id,
                patch: PlanPatch {
                    parent_plan_id: Some(Some(parent_id)),
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await;

    assert!(
        result.is_ok(),
        "valid re-parent should succeed, got: {:?}",
        result
    );
    let envs = result.unwrap();
    assert!(
        envs.iter().any(
            |e| matches!(&e.payload, Event::PlanUpdated { plan_id, .. } if *plan_id == child_id)
        ),
        "PlanUpdated must be emitted"
    );
}

/// Unparent via explicit null: patch.parent_plan_id = Some(None) → succeeds.
#[tokio::test]
async fn update_plan_unparent() {
    let (handler, plans) = build_stack().await;
    let project_id = ProjectId::new();

    let parent_id = create_plan(&handler, project_id).await;
    let child_id = create_plan(&handler, project_id).await;

    // First, attach child to parent.
    handler
        .handle(
            Command::UpdatePlan {
                id: child_id,
                patch: PlanPatch {
                    parent_plan_id: Some(Some(parent_id)),
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Verify parent is set in projection.
    let child = plans.get(child_id).await.unwrap().unwrap();
    assert_eq!(
        child.parent_plan_id,
        Some(parent_id),
        "parent should be set"
    );

    // Now unparent.
    let result = handler
        .handle(
            Command::UpdatePlan {
                id: child_id,
                patch: PlanPatch {
                    parent_plan_id: Some(None),
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await;

    assert!(result.is_ok(), "unparent should succeed, got: {:?}", result);

    let child = plans.get(child_id).await.unwrap().unwrap();
    assert_eq!(
        child.parent_plan_id, None,
        "parent_plan_id must be NULL after unparent"
    );
}

/// Self-parent: patch sets plan as its own parent → Validation error.
#[tokio::test]
async fn update_plan_self_parent_rejected() {
    let (handler, _plans) = build_stack().await;
    let project_id = ProjectId::new();

    let plan_id = create_plan(&handler, project_id).await;

    let result = handler
        .handle(
            Command::UpdatePlan {
                id: plan_id,
                patch: PlanPatch {
                    parent_plan_id: Some(Some(plan_id)), // self-reference
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await;

    assert!(
        matches!(result, Err(CoreError::Validation(_))),
        "self-parent must be rejected with Validation error, got: {:?}",
        result
    );
}

/// Ancestor cycle: A → B (valid), then try B → A → cycle rejected.
#[tokio::test]
async fn update_plan_cycle_rejected() {
    let (handler, _plans) = build_stack().await;
    let project_id = ProjectId::new();

    let a_id = create_plan(&handler, project_id).await;
    let b_id = create_plan(&handler, project_id).await;

    // Set B.parent = A (valid — no cycle).
    handler
        .handle(
            Command::UpdatePlan {
                id: b_id,
                patch: PlanPatch {
                    parent_plan_id: Some(Some(a_id)),
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Try to set A.parent = B → would create A ↔ B cycle.
    let result = handler
        .handle(
            Command::UpdatePlan {
                id: a_id,
                patch: PlanPatch {
                    parent_plan_id: Some(Some(b_id)),
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await;

    assert!(
        matches!(result, Err(CoreError::Validation(_))),
        "cycle must be rejected with Validation error, got: {:?}",
        result
    );
}
