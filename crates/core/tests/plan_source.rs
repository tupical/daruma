//! ADR-0009 plan source at intake: `source_ref` resolution order, the
//! project `intake_source` policy (off / warn / enforce, channels,
//! note_required), exemptions for internal producers, and auto-parent of a
//! repeated source.

use std::sync::Arc;

use daruma_api_dto::MutationWarning;
use daruma_core::{repos::PlanRepository, Command, CommandHandler};
use daruma_domain::{
    Actor, AutoAppendPatch, GitContext, IntakeSourceMode, IntakeSourcePolicy, NewPlan, NewTask,
    Plan, SourceChannel, SourceDeriveRule,
};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{CoreError, ProjectId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, PlanRepo, ProjectRepo, ProjectSettingsRepo, SqliteEventStore,
    TaskRepo,
};
use serde_json::json;

struct Stack {
    handler: CommandHandler,
    plans: Arc<PlanRepo>,
    settings: Arc<ProjectSettingsRepo>,
    project: ProjectId,
}

async fn stack() -> Stack {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let plans = Arc::new(PlanRepo::new(pool.clone()));
    let settings = Arc::new(ProjectSettingsRepo::new(pool.clone()));
    let handler = CommandHandler::new(
        store,
        Arc::new(TaskRepo::new(pool.clone())),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool.clone())),
        EventBus::default(),
    )
    .with_plans(plans.clone() as Arc<dyn PlanRepository>)
    .with_project_settings(settings.clone());
    let envs = handler
        .handle(
            Command::CreateProject {
                title: "Source".into(),
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
        plans,
        settings,
    }
}

fn set_policy(project: ProjectId, policy: Option<IntakeSourcePolicy>) -> Command {
    Command::UpdateProjectSettings {
        project_id: project,
        auto_append: AutoAppendPatch::default(),
        intake_source: Some(policy),
    }
}

fn policy(mode: IntakeSourceMode) -> IntakeSourcePolicy {
    IntakeSourcePolicy {
        mode,
        ..IntakeSourcePolicy::default()
    }
}

fn gitlab_channel() -> SourceChannel {
    SourceChannel {
        scheme: "https".into(),
        pattern: Some(r"^https://gitlab\.x/.+/-/issues/\d+$".into()),
        label: Some("Issue в GitLab".into()),
        note_required: false,
    }
}

impl Stack {
    async fn policy(&self, policy: IntakeSourcePolicy) {
        self.handler
            .handle(set_policy(self.project, Some(policy)), Actor::user())
            .await
            .unwrap();
    }

    fn new_plan(&self) -> NewPlan {
        NewPlan::new("Plan", self.project, Actor::user())
    }

    async fn try_materialize(
        &self,
        plan: NewPlan,
        actor: Actor,
    ) -> Result<(Plan, Vec<MutationWarning>), CoreError> {
        let outcome = self
            .handler
            .handle_with_warnings(
                Command::MaterializePlan {
                    plan,
                    tasks: vec![NewTask::new("t")],
                },
                actor,
            )
            .await?;
        let plan = outcome
            .events
            .iter()
            .find_map(|e| match &e.payload {
                Event::PlanCreated { plan } => Some(plan.clone()),
                _ => None,
            })
            .expect("PlanCreated");
        let stored = self.plans.get(plan.id).await.unwrap().unwrap();
        assert_eq!(stored.source_ref, plan.source_ref, "projection = event");
        Ok((stored, outcome.warnings))
    }

    async fn materialize(&self, plan: NewPlan) -> (Plan, Vec<MutationWarning>) {
        self.try_materialize(plan, Actor::user()).await.unwrap()
    }
}

fn codes(warnings: &[MutationWarning]) -> Vec<&str> {
    warnings.iter().map(|w| w.code.as_str()).collect()
}

#[tokio::test]
async fn explicit_ref_is_normalised_and_no_policy_warns_on_missing() {
    let s = stack().await;
    let mut plan = s.new_plan();
    plan.source_ref = Some(" HTTPS://u:t@GitLab.X/g/p/-/issues/1/?token=x ".into());
    let (stored, warnings) = s.materialize(plan).await;
    assert_eq!(
        stored.source_ref.as_deref(),
        Some("https://gitlab.x/g/p/-/issues/1")
    );
    assert!(warnings.is_empty(), "{warnings:?}");

    // No `intake_source` key = warn without channels: the plan is created.
    let (stored, warnings) = s.materialize(s.new_plan()).await;
    assert!(stored.source_ref.is_none());
    assert_eq!(codes(&warnings), ["plan_source_missing"]);
}

#[tokio::test]
async fn invalid_explicit_ref_is_a_validation_error() {
    let s = stack().await;
    let mut plan = s.new_plan();
    plan.source_ref = Some("issue 12".into());
    let err = s.try_materialize(plan, Actor::user()).await.unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");
}

#[tokio::test]
async fn resolution_order_parent_then_derive_then_default() {
    let s = stack().await;
    s.policy(IntakeSourcePolicy {
        mode: IntakeSourceMode::Warn,
        default_ref: Some("self://solo".into()),
        derive: vec![
            // Group 2 only participates for `NNNN-x_…`: otherwise no fire.
            SourceDeriveRule {
                from: "branch".into(),
                pattern: r"^(\d{4,6})(-x)?_".into(),
                ref_template: "https://other.x/$1$2".into(),
            },
            SourceDeriveRule {
                from: "branch".into(),
                pattern: r"^(\d{4,6})_".into(),
                ref_template: "https://gitlab.x/g/p/-/issues/$1".into(),
            },
        ],
        channels: vec![],
    })
    .await;
    let branch = |b: &str| {
        Some(GitContext {
            branch: Some(b.into()),
            ..GitContext::default()
        })
    };

    // 3. derive from the branch.
    let mut plan = s.new_plan();
    plan.git_context = branch("11084_av_ai_assistant");
    let (derived, _) = s.materialize(plan).await;
    assert_eq!(
        derived.source_ref.as_deref(),
        Some("https://gitlab.x/g/p/-/issues/11084")
    );

    // 1. explicit beats derive.
    let mut plan = s.new_plan();
    plan.source_ref = Some("mailto:c@acme.ru".into());
    plan.git_context = branch("11084_x");
    let (explicit, _) = s.materialize(plan).await;
    assert_eq!(explicit.source_ref.as_deref(), Some("mailto:c@acme.ru"));

    // 2. parent's ref beats derive and default.
    let mut plan = s.new_plan();
    plan.parent_plan_id = Some(explicit.id);
    plan.git_context = branch("11084_x");
    let (child, warnings) = s.materialize(plan).await;
    assert_eq!(child.source_ref.as_deref(), Some("mailto:c@acme.ru"));
    assert_eq!(child.parent_plan_id, Some(explicit.id));
    assert!(warnings.is_empty(), "{warnings:?}");

    // 4. default_ref, used literally; a non-matching branch falls through.
    let mut plan = s.new_plan();
    plan.git_context = branch("feature/no-number");
    let (defaulted, warnings) = s.materialize(plan).await;
    assert_eq!(defaulted.source_ref.as_deref(), Some("self://solo"));
    // A default ref never auto-parents: plans without a source stay apart.
    let (second, warnings2) = s.materialize(s.new_plan()).await;
    assert_eq!(second.source_ref.as_deref(), Some("self://solo"));
    assert!(defaulted.parent_plan_id.is_none() && second.parent_plan_id.is_none());
    assert!(warnings.is_empty() && warnings2.is_empty(), "{warnings2:?}");

    // A derived ref does auto-parent onto the first plan of that issue.
    let mut plan = s.new_plan();
    plan.git_context = branch("11084_again");
    let (again, warnings) = s.materialize(plan).await;
    assert_eq!(again.parent_plan_id, Some(derived.id));
    assert_eq!(codes(&warnings), ["plan_auto_parented"]);
}

#[tokio::test]
async fn repeated_source_is_auto_parented_to_the_first_plan() {
    let s = stack().await;
    let source = "https://gitlab.x/g/p/-/issues/5";
    let with_ref = || {
        let mut plan = s.new_plan();
        plan.source_ref = Some(source.into());
        plan
    };
    let (first, warnings) = s.materialize(with_ref()).await;
    assert!(first.parent_plan_id.is_none());
    assert!(warnings.is_empty(), "{warnings:?}");

    for _ in 0..2 {
        let (next, warnings) = s.materialize(with_ref()).await;
        assert_eq!(next.parent_plan_id, Some(first.id));
        assert_eq!(codes(&warnings), ["plan_auto_parented"]);
        assert_eq!(warnings[0].details["parent_plan_id"], json!(first.id));
    }

    // An explicit parent is never overridden.
    let (other, _) = s.materialize(s.new_plan()).await;
    let mut plan = with_ref();
    plan.parent_plan_id = Some(other.id);
    let (explicit, warnings) = s.materialize(plan).await;
    assert_eq!(explicit.parent_plan_id, Some(other.id));
    assert!(!codes(&warnings).contains(&"plan_auto_parented"));
}

#[tokio::test]
async fn warn_mode_creates_the_plan_with_channel_warnings() {
    let s = stack().await;
    s.policy(IntakeSourcePolicy {
        channels: vec![gitlab_channel()],
        ..policy(IntakeSourceMode::Warn)
    })
    .await;

    let mut plan = s.new_plan();
    plan.source_ref = Some("https://github.com/o/r/issues/1".into());
    let (stored, warnings) = s.materialize(plan).await;
    assert!(stored.source_ref.is_some());
    assert_eq!(codes(&warnings), ["plan_source_channel_mismatch"]);
    assert_eq!(
        warnings[0].details["channels"][0]["label"],
        "Issue в GitLab"
    );

    let (_, warnings) = s.materialize(s.new_plan()).await;
    assert_eq!(codes(&warnings), ["plan_source_missing"]);

    let mut plan = s.new_plan();
    plan.source_ref = Some("https://gitlab.x/g/p/-/issues/9".into());
    let (_, warnings) = s.materialize(plan).await;
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[tokio::test]
async fn off_mode_never_warns() {
    let s = stack().await;
    s.policy(IntakeSourcePolicy {
        channels: vec![gitlab_channel()],
        ..policy(IntakeSourceMode::Off)
    })
    .await;
    let (_, warnings) = s.materialize(s.new_plan()).await;
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[tokio::test]
async fn enforce_mode_rejects_with_channels_and_creates_nothing() {
    let s = stack().await;
    s.policy(IntakeSourcePolicy {
        channels: vec![gitlab_channel()],
        ..policy(IntakeSourceMode::Enforce)
    })
    .await;

    for source in [None, Some("self://me")] {
        let mut plan = s.new_plan();
        plan.source_ref = source.map(Into::into);
        let err = s.try_materialize(plan, Actor::user()).await.unwrap_err();
        let CoreError::Unprocessable { code, details, .. } = &err else {
            panic!("expected Unprocessable, got {err:?}");
        };
        assert_eq!(*code, "plan_source_required");
        assert_eq!(
            details["channels"],
            json!([{
                "scheme": "https",
                "pattern": r"^https://gitlab\.x/.+/-/issues/\d+$",
                "label": "Issue в GitLab"
            }])
        );
        let reason = if source.is_none() {
            "plan_source_missing"
        } else {
            "plan_source_channel_mismatch"
        };
        assert_eq!(details["reason"], reason);
    }
    assert!(s
        .plans
        .list_by_project(s.project, None)
        .await
        .unwrap()
        .is_empty());

    let mut plan = s.new_plan();
    plan.source_ref = Some("https://gitlab.x/g/p/-/issues/3".into());
    let (_, warnings) = s.materialize(plan).await;
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[tokio::test]
async fn note_required_channel_needs_a_source_brief() {
    let s = stack().await;
    s.policy(IntakeSourcePolicy {
        channels: vec![SourceChannel {
            scheme: "self".into(),
            pattern: None,
            label: Some("Своя инициатива".into()),
            note_required: true,
        }],
        ..policy(IntakeSourceMode::Enforce)
    })
    .await;

    let mut plan = s.new_plan();
    plan.source_ref = Some("SELF://me".into());
    let err = s
        .try_materialize(plan.clone(), Actor::user())
        .await
        .unwrap_err();
    let CoreError::Unprocessable { code, details, .. } = &err else {
        panic!("expected Unprocessable, got {err:?}");
    };
    assert_eq!(*code, "plan_source_required");
    assert_eq!(details["reason"], "plan_source_note_required");

    plan.source_brief = Some("owner asked in chat".into());
    let (stored, warnings) = s.materialize(plan).await;
    assert_eq!(stored.source_ref.as_deref(), Some("self://me"));
    assert!(warnings.is_empty(), "{warnings:?}");

    // Same channel under warn → plan created + note warning.
    s.policy(IntakeSourcePolicy {
        channels: vec![SourceChannel {
            scheme: "self".into(),
            pattern: None,
            label: None,
            note_required: true,
        }],
        ..policy(IntakeSourceMode::Warn)
    })
    .await;
    let mut plan = s.new_plan();
    plan.source_ref = Some("self://other".into());
    let (_, warnings) = s.materialize(plan).await;
    assert_eq!(codes(&warnings), ["plan_source_note_required"]);
}

#[tokio::test]
async fn intake_marker_is_reserved_and_create_plan_follows_policy() {
    let s = stack().await;
    s.policy(IntakeSourcePolicy {
        channels: vec![gitlab_channel()],
        ..policy(IntakeSourceMode::Enforce)
    })
    .await;
    let create = |plan: NewPlan, external_ref: Option<(String, String, String)>| {
        s.handler
            .handle_with_warnings(Command::CreatePlan { plan, external_ref }, Actor::user())
    };

    // The legacy intake marker is storage-only: clients cannot claim it.
    let mut plan = s.new_plan();
    plan.source_brief = Some(PlanRepo::INTAKE_MARKER.into());
    let err = s
        .try_materialize(plan.clone(), Actor::user())
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");
    let err = create(plan, None).await.unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");

    // CreatePlan without external_ref is intake too: enforce applies.
    let err = create(s.new_plan(), None).await.unwrap_err();
    assert!(
        matches!(
            err,
            CoreError::Unprocessable {
                code: "plan_source_required",
                ..
            }
        ),
        "{err:?}"
    );
    let mut plan = s.new_plan();
    plan.source_ref = Some("https://gitlab.x/g/p/-/issues/8".into());
    let first = create(plan.clone(), None).await.unwrap();
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    let second = create(plan, None).await.unwrap();
    assert_eq!(
        codes(&second.warnings),
        ["plan_auto_parented"],
        "CreatePlan auto-parents like materialize"
    );

    // Invalid git_context is rejected, not silently dropped.
    let mut plan = s.new_plan();
    plan.source_ref = Some("https://gitlab.x/g/p/-/issues/8".into());
    plan.git_context = Some(GitContext::default());
    let err = create(plan, None).await.unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");

    // `CreatePlan` with `external_ref` (idempotent producer): no policy, the
    // explicit ref is still normalised and stored.
    let mut plan = s.new_plan();
    plan.source_ref = Some("MCPBOX:run:42".into());
    let outcome = create(plan, Some(("t".into(), "k".into(), "e".into())))
        .await
        .unwrap();
    assert!(outcome.warnings.is_empty());
    let Event::PlanCreated { plan } = &outcome.events[0].payload else {
        panic!("expected PlanCreated");
    };
    let stored = s.plans.get(plan.id).await.unwrap().unwrap();
    assert_eq!(stored.source_ref.as_deref(), Some("mcpbox:run:42"));
    let err = create(s.new_plan(), Some(("t".into(), "k".into(), "e2".into())))
        .await
        .map(|_| ());
    assert!(
        err.is_ok(),
        "no source + external_ref is not checked: {err:?}"
    );
}

#[tokio::test]
async fn invalid_policy_is_rejected_and_null_removes_it() {
    let s = stack().await;
    let bad = [
        IntakeSourcePolicy {
            derive: vec![SourceDeriveRule {
                from: "branch".into(),
                pattern: "(".into(),
                ref_template: "https://x/$1".into(),
            }],
            ..IntakeSourcePolicy::default()
        },
        IntakeSourcePolicy {
            derive: vec![SourceDeriveRule {
                from: "title".into(),
                pattern: ".".into(),
                ref_template: "https://x".into(),
            }],
            ..IntakeSourcePolicy::default()
        },
        IntakeSourcePolicy {
            channels: vec![SourceChannel {
                scheme: " ".into(),
                pattern: None,
                label: None,
                note_required: false,
            }],
            ..IntakeSourcePolicy::default()
        },
        IntakeSourcePolicy {
            channels: vec![SourceChannel {
                pattern: Some("[".into()),
                ..gitlab_channel()
            }],
            ..IntakeSourcePolicy::default()
        },
        IntakeSourcePolicy {
            default_ref: Some("no scheme".into()),
            ..IntakeSourcePolicy::default()
        },
        IntakeSourcePolicy {
            channels: vec![SourceChannel {
                pattern: Some("a".repeat(513)),
                ..gitlab_channel()
            }],
            ..IntakeSourcePolicy::default()
        },
        IntakeSourcePolicy {
            channels: vec![gitlab_channel(); 33],
            ..IntakeSourcePolicy::default()
        },
        IntakeSourcePolicy {
            derive: vec![
                SourceDeriveRule {
                    from: "branch".into(),
                    pattern: "x".into(),
                    ref_template: "https://x".into(),
                };
                33
            ],
            ..IntakeSourcePolicy::default()
        },
        IntakeSourcePolicy {
            // Compiles past the size limit.
            channels: vec![SourceChannel {
                pattern: Some(r"\w{500}\w{500}".into()),
                ..gitlab_channel()
            }],
            ..IntakeSourcePolicy::default()
        },
    ];
    for policy in bad {
        let err = s
            .handler
            .handle(set_policy(s.project, Some(policy.clone())), Actor::user())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Unprocessable {
                    code: "invalid_intake_source",
                    ..
                }
            ),
            "{policy:?}: {err:?}"
        );
    }
    assert!(s.settings.intake_source(s.project).await.unwrap().is_none());

    // An unknown mode never becomes a command.
    let wire = json!({
        "type": "update_project_settings",
        "project_id": s.project,
        "intake_source": {"mode": "strict"}
    });
    assert!(serde_json::from_value::<Command>(wire).is_err());
    // Unknown fields are refused at every level (typos must not pass silently).
    for intake_source in [
        json!({"mode": "warn", "chanels": []}),
        json!({"channels": [{"scheme": "https", "lable": "x"}]}),
        json!({"derive": [{"from": "branch", "pattern": "x", "ref": "https://x", "to": 1}]}),
    ] {
        let wire = json!({
            "type": "update_project_settings",
            "project_id": s.project,
            "intake_source": intake_source,
        });
        assert!(serde_json::from_value::<Command>(wire).is_err());
    }

    s.policy(policy(IntakeSourceMode::Enforce)).await;
    // A settings patch that does not mention `intake_source` keeps it.
    s.handler
        .handle(
            Command::UpdateProjectSettings {
                project_id: s.project,
                auto_append: AutoAppendPatch {
                    interview: Some(false),
                    human_log: None,
                },
                intake_source: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(
        s.settings.intake_source(s.project).await.unwrap(),
        Some(policy(IntakeSourceMode::Enforce))
    );
    s.handler
        .handle(set_policy(s.project, None), Actor::user())
        .await
        .unwrap();
    assert!(s.settings.intake_source(s.project).await.unwrap().is_none());
}
