//! ADR-0009 source chain: nodes addressed by URI (`note:<uuid>` without a
//! link), fill-only upserts, links written with the plan, `ExtendSource`,
//! cycle/depth limits, and replay of the `sources` projection.

use std::sync::Arc;

use daruma_api_dto::MutationWarning;
use daruma_core::{repos::PlanRepository, Command, CommandHandler};
use daruma_domain::{
    Actor, AutoAppendPatch, IntakeSourceMode, IntakeSourcePolicy, NewPlan, NewTask, Plan,
    SourceChannel, SourceInput,
};
use daruma_events::{Event, EventBus, EventEnvelope, EventStore};
use daruma_shared::{CoreError, PlanId, ProjectId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, PlanRepo, ProjectRepo, ProjectSettingsRepo, SqliteEventStore,
    TaskRepo,
};

const ISSUE_1: &str = "https://gitlab.x/g/p/-/issues/1";
const ISSUE_2: &str = "https://gitlab.x/g/p/-/issues/2";
const EMAIL: &str = "mailto:dev@x.ru#<msg-1@x>";

struct Stack {
    handler: CommandHandler,
    store: Arc<dyn EventStore>,
    plans: Arc<PlanRepo>,
    project: ProjectId,
}

async fn stack() -> Stack {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let plans = Arc::new(PlanRepo::new(pool.clone()));
    let handler = CommandHandler::new(
        store.clone(),
        Arc::new(TaskRepo::new(pool.clone())),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool.clone())),
        EventBus::default(),
    )
    .with_plans(plans.clone() as Arc<dyn PlanRepository>)
    .with_project_settings(Arc::new(ProjectSettingsRepo::new(pool.clone())));
    let envs = handler
        .handle(
            Command::CreateProject {
                title: "Chain".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let Event::ProjectCreated { project } = &envs[0].payload else {
        panic!("expected ProjectCreated");
    };
    Stack {
        project: project.id,
        handler,
        store,
        plans,
    }
}

fn node(source_ref: Option<&str>, label: Option<&str>) -> SourceInput {
    SourceInput {
        source_ref: source_ref.map(str::to_string),
        label: label.map(str::to_string),
        ..SourceInput::default()
    }
}

fn extend(
    plan_id: Option<PlanId>,
    source_ref: Option<&str>,
    upstream: Vec<SourceInput>,
) -> Command {
    Command::ExtendSource {
        plan_id,
        source_ref: source_ref.map(str::to_string),
        source: None,
        upstream,
    }
}

fn kept(warnings: &[MutationWarning]) -> &MutationWarning {
    warnings
        .iter()
        .find(|w| w.code == "source_upstream_kept")
        .unwrap_or_else(|| panic!("no source_upstream_kept in {warnings:?}"))
}

fn code(err: &CoreError) -> &'static str {
    err.code()
}

impl Stack {
    async fn materialize(&self, source: Option<SourceInput>) -> Result<Plan, CoreError> {
        Ok(self.materialize_warned(source).await?.0)
    }

    async fn materialize_warned(
        &self,
        source: Option<SourceInput>,
    ) -> Result<(Plan, Vec<MutationWarning>), CoreError> {
        let mut plan = NewPlan::new("Plan", self.project, Actor::user());
        plan.source = source;
        let outcome = self
            .handler
            .handle_with_warnings(
                Command::MaterializePlan {
                    plan,
                    tasks: vec![NewTask::new("t")],
                },
                Actor::user(),
            )
            .await?;
        let id = outcome
            .events
            .iter()
            .find_map(|e| match &e.payload {
                Event::PlanCreated { plan } => Some(plan.id),
                _ => None,
            })
            .expect("PlanCreated");
        Ok((self.plans.get(id).await.unwrap().unwrap(), outcome.warnings))
    }

    async fn chain(&self, source_ref: &str) -> Vec<(String, Option<String>)> {
        self.plans
            .source_chain(source_ref)
            .await
            .unwrap()
            .into_iter()
            .map(|n| (n.source_ref, n.label))
            .collect()
    }

    /// Distinct nodes ever written — every `sources` row comes from one.
    async fn source_count(&self) -> usize {
        let refs: std::collections::HashSet<String> = self
            .store
            .load_since(0, 100_000)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                Event::SourceUpserted { source } => Some(source.source_ref),
                _ => None,
            })
            .collect();
        refs.len()
    }
}

/// The ADR example: issue → email → discussion → client problem, written
/// atomically with the plan; label-only nodes get `note:` refs.
#[tokio::test]
async fn materialize_writes_four_link_chain() {
    let s = stack().await;
    let mut source = node(Some(ISSUE_1), Some("GitLab issue #1"));
    source.upstream = vec![
        node(Some(EMAIL), Some("Email from dev@")),
        SourceInput {
            occurred_at: Some("2026-01-01T18:30:00+03:00".into()),
            ..node(None, Some("Discussion with the director"))
        },
        node(None, Some("Client problem")),
    ];
    let plan = s.materialize(Some(source)).await.unwrap();
    assert_eq!(plan.source_ref.as_deref(), Some(ISSUE_1));

    let chain = s.chain(ISSUE_1).await;
    assert_eq!(chain.len(), 4, "{chain:?}");
    assert_eq!(chain[1].0, EMAIL);
    assert!(chain[2].0.starts_with("note:") && chain[3].0.starts_with("note:"));
    assert_ne!(chain[2].0, chain[3].0);
    assert_eq!(chain[3].1.as_deref(), Some("Client problem"));
    let discussion = s.plans.get_source(&chain[2].0).await.unwrap().unwrap();
    assert_eq!(
        discussion.occurred_at.as_deref(),
        Some("2026-01-01T18:30:00+03:00")
    );
}

/// A label-only nearest node gets a synthetic `note:` ref that becomes the
/// plan's `source_ref`; two such plans are not auto-parented.
#[tokio::test]
async fn synthetic_note_ref_is_the_plan_source() {
    let s = stack().await;
    let a = s.materialize(Some(node(None, Some("Call")))).await.unwrap();
    let b = s.materialize(Some(node(None, Some("Call")))).await.unwrap();
    let a_ref = a.source_ref.unwrap();
    assert!(a_ref.starts_with("note:"), "{a_ref}");
    assert_ne!(Some(a_ref.clone()), b.source_ref);
    assert_eq!(b.parent_plan_id, None, "note: refs never auto-parent");
    assert_eq!(s.chain(&a_ref).await, [(a_ref, Some("Call".into()))]);

    let err = s
        .materialize(Some(SourceInput::default()))
        .await
        .unwrap_err();
    assert_eq!(code(&err), "validation", "{err}");
}

/// One email → two issues: one email node with two children; the second
/// upsert fills empty fields only; a different upstream is a 409.
#[tokio::test]
async fn shared_node_is_one_row_and_fill_only() {
    let s = stack().await;
    let mut one = node(Some(ISSUE_1), None);
    one.upstream = vec![node(Some(EMAIL), None)];
    s.materialize(Some(one)).await.unwrap();
    let mut two = node(Some(ISSUE_2), None);
    two.upstream = vec![SourceInput {
        note: Some("forwarded".into()),
        ..node(Some(EMAIL), Some("Email"))
    }];
    s.materialize(Some(two)).await.unwrap();
    assert_eq!(s.source_count().await, 3);
    let email = s.plans.get_source(EMAIL).await.unwrap().unwrap();
    assert_eq!(email.label.as_deref(), Some("Email"), "empty label filled");
    assert_eq!(email.note.as_deref(), Some("forwarded"));

    // A set label is not overwritten.
    let mut three = node(Some(EMAIL), Some("Other label"));
    three.upstream = vec![node(None, Some("Call"))];
    s.materialize(Some(three)).await.unwrap();
    let email = s.plans.get_source(EMAIL).await.unwrap().unwrap();
    assert_eq!(email.label.as_deref(), Some("Email"));
    let top = email.upstream_ref.clone().unwrap();

    // Issue 1 already points at the email: at intake the stored link wins,
    // the other upstream is dropped with a warning, the plan is created.
    let mut conflict = node(Some(ISSUE_1), None);
    conflict.upstream = vec![node(Some("mailto:other@x.ru"), None)];
    let before = s.source_count().await;
    let (_, warnings) = s.materialize_warned(Some(conflict)).await.unwrap();
    let kept = kept(&warnings);
    assert_eq!(kept.details["kept_upstream"], EMAIL);
    assert_eq!(kept.details["dropped_upstream"], "mailto:other@x.ru");
    assert_eq!(s.source_count().await, before, "dropped node not written");
    // Explicit extend: the email (already linked to the top note) given
    // another upstream is a 409.
    let err = s
        .handler
        .handle(
            extend(
                None,
                Some(ISSUE_1),
                vec![node(Some(EMAIL), None), node(Some("self://other"), None)],
            ),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "source_upstream_conflict", "{err}");
    assert_eq!(s.source_count().await, before, "409 writes nothing");
    assert_eq!(
        s.plans
            .get_source(EMAIL)
            .await
            .unwrap()
            .unwrap()
            .upstream_ref,
        Some(top)
    );
}

/// Repeating a chain whose label-only top already exists (issue 2 → the
/// same email → «Обсуждение»): the stored link wins, no second
/// «Обсуждение» node, a warning instead of a 409 — also on a plain retry.
#[tokio::test]
async fn intake_keeps_existing_upstream_and_warns() {
    let s = stack().await;
    let chain = |issue: &str| {
        let mut source = node(Some(issue), None);
        source.upstream = vec![node(Some(EMAIL), None), node(None, Some("Обсуждение"))];
        source
    };
    let (_, warnings) = s.materialize_warned(Some(chain(ISSUE_1))).await.unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (_, warnings) = s.materialize_warned(Some(chain(ISSUE_2))).await.unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(kept(&warnings).details["ref"], EMAIL);
    // Retry: no 409 (auto-parented to the first issue-2 plan, same warning).
    let (_, warnings) = s.materialize_warned(Some(chain(ISSUE_2))).await.unwrap();
    assert_eq!(kept(&warnings).details["ref"], EMAIL);

    assert_eq!(
        s.source_count().await,
        4,
        "issue 1, issue 2, email, one note"
    );
    let two = s.chain(ISSUE_2).await;
    assert_eq!(two.len(), 3);
    assert_eq!(two[1..], s.chain(ISSUE_1).await[1..]);
    assert_eq!(two[2].1.as_deref(), Some("Обсуждение"));
}

/// `ExtendSource` by plan attaches to the top node; by ref only to the top.
#[tokio::test]
async fn extend_attaches_to_top_node() {
    let s = stack().await;
    let mut source = node(Some(ISSUE_1), None);
    source.upstream = vec![node(Some(EMAIL), None)];
    let plan = s.materialize(Some(source)).await.unwrap();

    s.handler
        .handle(
            extend(Some(plan.id), None, vec![node(None, Some("Discussion"))]),
            Actor::user(),
        )
        .await
        .unwrap();
    // By a node below the top: 409 naming the top, nothing written.
    let top = s.chain(ISSUE_1).await[2].0.clone();
    let err = s
        .handler
        .handle(
            extend(
                None,
                Some(ISSUE_1),
                vec![node(None, Some("Client problem"))],
            ),
            Actor::user(),
        )
        .await
        .unwrap_err();
    let CoreError::CodedConflict { details, .. } = &err else {
        panic!("expected CodedConflict, got {err:?}");
    };
    assert_eq!(code(&err), "source_upstream_conflict");
    assert_eq!(details["top"], top.as_str(), "{err}");
    assert!(err.to_string().contains(&top), "{err}");
    assert_eq!(s.chain(ISSUE_1).await.len(), 3);
    // By the top itself.
    s.handler
        .handle(
            extend(None, Some(&top), vec![node(None, Some("Client problem"))]),
            Actor::user(),
        )
        .await
        .unwrap();
    let labels: Vec<_> = s.chain(ISSUE_1).await.into_iter().map(|(_, l)| l).collect();
    assert_eq!(
        labels,
        [
            None,
            None,
            Some("Discussion".into()),
            Some("Client problem".into())
        ]
    );

    let err = s
        .handler
        .handle(extend(None, Some(ISSUE_1), vec![]), Actor::user())
        .await
        .unwrap_err();
    assert_eq!(code(&err), "validation", "upstream required: {err}");
    let err = s
        .handler
        .handle(
            extend(Some(plan.id), Some(ISSUE_1), vec![node(None, Some("x"))]),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        code(&err),
        "validation",
        "exactly one of plan_id/ref: {err}"
    );
    let err = s
        .handler
        .handle(
            extend(None, Some("mailto:nobody@x"), vec![node(None, Some("x"))]),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "not_found", "{err}");
}

/// A source-less plan gets its source via `PlanSourceSet` (checked against
/// the plan project's channels); a second `source` is a 409.
#[tokio::test]
async fn extend_sets_source_of_sourceless_plan_once() {
    let s = stack().await;
    let plan = s.materialize(None).await.unwrap();
    assert_eq!(plan.source_ref, None);

    s.handler
        .handle(
            Command::UpdateProjectSettings {
                project_id: s.project,
                auto_append: AutoAppendPatch::default(),
                intake_source: Some(Some(IntakeSourcePolicy {
                    mode: IntakeSourceMode::Enforce,
                    channels: vec![SourceChannel {
                        scheme: "https".into(),
                        pattern: None,
                        label: None,
                        note_required: false,
                    }],
                    ..IntakeSourcePolicy::default()
                })),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let set = |source: SourceInput| Command::ExtendSource {
        plan_id: Some(plan.id),
        source_ref: None,
        source: Some(source),
        upstream: vec![node(Some(EMAIL), None)],
    };
    let err = s
        .handler
        .handle(set(node(Some("mailto:a@x"), None)), Actor::user())
        .await
        .unwrap_err();
    assert_eq!(
        code(&err),
        "plan_source_required",
        "channel mismatch: {err}"
    );
    assert_eq!(s.source_count().await, 0, "enforce 422 writes nothing");

    let envs = s
        .handler
        .handle(set(node(Some(ISSUE_1), None)), Actor::user())
        .await
        .unwrap();
    assert!(envs
        .iter()
        .any(|e| matches!(e.payload, Event::PlanSourceSet { .. })));
    let stored = s.plans.get(plan.id).await.unwrap().unwrap();
    assert_eq!(stored.source_ref.as_deref(), Some(ISSUE_1));
    assert_eq!(s.chain(ISSUE_1).await.len(), 2);

    let err = s
        .handler
        .handle(set(node(Some(ISSUE_2), None)), Actor::user())
        .await
        .unwrap_err();
    assert_eq!(code(&err), "plan_source_already_set", "{err}");
}

/// Enforce 422 at intake writes neither the plan nor any node.
#[tokio::test]
async fn enforce_rejection_writes_no_nodes() {
    let s = stack().await;
    s.handler
        .handle(
            Command::UpdateProjectSettings {
                project_id: s.project,
                auto_append: AutoAppendPatch::default(),
                intake_source: Some(Some(IntakeSourcePolicy {
                    mode: IntakeSourceMode::Enforce,
                    channels: vec![SourceChannel {
                        scheme: "https".into(),
                        pattern: None,
                        label: None,
                        note_required: false,
                    }],
                    ..IntakeSourcePolicy::default()
                })),
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let mut source = node(Some(EMAIL), None);
    source.upstream = vec![node(None, Some("Call"))];
    let err = s.materialize(Some(source)).await.unwrap_err();
    assert_eq!(code(&err), "plan_source_required", "{err}");
    assert_eq!(s.source_count().await, 0);
}

/// A link that would close a cycle, or a chain longer than 32 nodes, is a
/// 422 `source_chain_invalid` and writes nothing.
#[tokio::test]
async fn cycle_and_depth_are_rejected() {
    let s = stack().await;
    let mut source = node(Some(ISSUE_1), None);
    source.upstream = vec![node(Some(EMAIL), None)];
    s.materialize(Some(source)).await.unwrap();

    // email → issue 1 would close issue 1 → email → issue 1.
    let before = s.source_count().await;
    let err = s
        .handler
        .handle(
            extend(None, Some(EMAIL), vec![node(Some(ISSUE_1), None)]),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "source_chain_invalid", "{err}");
    let mut looped = node(Some(ISSUE_2), None);
    looped.upstream = vec![node(Some("mailto:b@x"), None), node(Some(ISSUE_2), None)];
    let err = s.materialize(Some(looped)).await.unwrap_err();
    assert_eq!(code(&err), "source_chain_invalid", "{err}");
    assert_eq!(s.source_count().await, before);

    // 1 + 31 nodes fit; one more above the top (`self://n29`) does not.
    let upstream: Vec<_> = (0..30)
        .map(|i| node(Some(&format!("self://n{i}")), None))
        .collect();
    s.handler
        .handle(extend(None, Some(EMAIL), upstream), Actor::user())
        .await
        .unwrap();
    assert_eq!(s.chain(ISSUE_1).await.len(), 32);
    let err = s
        .handler
        .handle(
            extend(
                None,
                Some("self://n29"),
                vec![node(Some("self://over"), None)],
            ),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "source_chain_invalid", "{err}");
    let too_long: Vec<_> = (0..33)
        .map(|i| node(Some(&format!("self://m{i}")), None))
        .collect();
    let err = s
        .handler
        .handle(extend(None, Some("self://n29"), too_long), Actor::user())
        .await
        .unwrap_err();
    assert_eq!(code(&err), "source_chain_invalid", "{err}");
}

/// Refs from elsewhere (bare `source_ref`) get a ref-only node, and the
/// `sources` projection rebuilt from the event log equals the live one.
#[tokio::test]
async fn replay_rebuilds_the_same_chain() {
    let s = stack().await;
    let mut plan = NewPlan::new("Bare", s.project, Actor::user());
    plan.source_ref = Some(ISSUE_2.into());
    s.handler
        .handle(
            Command::MaterializePlan {
                plan,
                tasks: vec![NewTask::new("t")],
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(s.chain(ISSUE_2).await, [(ISSUE_2.to_string(), None)]);

    let mut source = node(Some(ISSUE_1), Some("Issue"));
    source.upstream = vec![node(Some(EMAIL), None), node(None, Some("Call"))];
    s.materialize(Some(source)).await.unwrap();
    s.handler
        .handle(
            extend(None, Some(ISSUE_2), vec![node(Some(EMAIL), Some("Email"))]),
            Actor::user(),
        )
        .await
        .unwrap();

    let fresh = Db::memory().await.unwrap();
    fresh.migrate().await.unwrap();
    let replayed = PlanRepo::new(fresh.pool().clone());
    let envs: Vec<EventEnvelope> = s.store.load_since(0, 100_000).await.unwrap();
    for env in &envs {
        replayed.apply_event(env).await.unwrap();
    }
    for start in [ISSUE_1, ISSUE_2] {
        assert_eq!(
            replayed.source_chain(start).await.unwrap(),
            s.plans.source_chain(start).await.unwrap(),
            "{start}"
        );
    }
    assert_eq!(replayed.source_chain(ISSUE_2).await.unwrap().len(), 3);
}
