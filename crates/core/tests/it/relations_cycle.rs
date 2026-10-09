//! AC-5: Cycle detection tests for `detect_cycle` (§3.2 W2.1).

use daruma_core::relation_enforcement::detect_cycle;
use daruma_domain::{Actor, Relation, RelationKind};
use daruma_shared::{time, CoreError, RelationId, TaskId};
use daruma_storage::{Db, RelationRepo};

async fn make_repo() -> RelationRepo {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    RelationRepo::new(db.pool().clone())
}

fn rel(from: TaskId, to: TaskId, kind: RelationKind) -> Relation {
    Relation {
        id: RelationId::new(),
        from,
        to,
        kind,
        created_at: time::now(),
        created_by: Actor::user(),
    }
}

// ── AC-5a: self-loop ──────────────────────────────────────────────────────────

#[tokio::test]
async fn self_loop() {
    let repo = make_repo().await;
    let a = TaskId::new();
    let err = detect_cycle(&repo, a, a, RelationKind::Blocks)
        .await
        .unwrap_err();
    match err {
        CoreError::Validation(msg) => {
            assert!(
                msg.contains("cycle_detected"),
                "expected cycle_detected, got: {msg}"
            );
        }
        other => panic!("expected Validation error, got: {other:?}"),
    }
}

// ── AC-5b: chain cycle ────────────────────────────────────────────────────────

#[tokio::test]
async fn chain_cycle() {
    // A blocks B, B blocks C → adding C blocks A must be rejected.
    let repo = make_repo().await;
    let a = TaskId::new();
    let b = TaskId::new();
    let c = TaskId::new();

    repo.insert(&rel(a, b, RelationKind::Blocks)).await.unwrap();
    repo.insert(&rel(b, c, RelationKind::Blocks)).await.unwrap();

    let err = detect_cycle(&repo, c, a, RelationKind::Blocks)
        .await
        .unwrap_err();
    match err {
        CoreError::Validation(msg) => {
            assert!(
                msg.contains("cycle_detected"),
                "expected cycle_detected, got: {msg}"
            );
        }
        other => panic!("expected Validation error, got: {other:?}"),
    }
}

// ── non_blocks_no_cycle_check: RelatesTo / Duplicates are exempt ──────────────

#[tokio::test]
async fn non_blocks_no_cycle_check() {
    // A→B RelatesTo, B→A RelatesTo — must be accepted without cycle error.
    let repo = make_repo().await;
    let a = TaskId::new();
    let b = TaskId::new();

    repo.insert(&rel(a, b, RelationKind::RelatesTo))
        .await
        .unwrap();
    // Would be a "cycle" if we checked, but cycle detection is Blocks-only.
    detect_cycle(&repo, b, a, RelationKind::RelatesTo)
        .await
        .unwrap();

    // Same for Duplicates.
    detect_cycle(&repo, b, a, RelationKind::Duplicates)
        .await
        .unwrap();
}

// ── diamond (non-cycle): A→B, A→C, B→D, C→D is a DAG — should be OK ─────────

#[tokio::test]
async fn diamond_is_not_a_cycle() {
    let repo = make_repo().await;
    let a = TaskId::new();
    let b = TaskId::new();
    let c = TaskId::new();
    let d = TaskId::new();

    repo.insert(&rel(a, b, RelationKind::Blocks)).await.unwrap();
    repo.insert(&rel(a, c, RelationKind::Blocks)).await.unwrap();
    repo.insert(&rel(b, d, RelationKind::Blocks)).await.unwrap();
    // Adding C→D should be fine (not a cycle).
    detect_cycle(&repo, c, d, RelationKind::Blocks)
        .await
        .unwrap();
}
