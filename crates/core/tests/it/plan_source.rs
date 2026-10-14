//! ADR-0009 plan source at intake: `source_ref` resolution order, the
//! project `intake_source` policy (off / warn / enforce, channels,
//! note_required), exemptions for internal producers, auto-parent of a
//! repeated source, and `plan_choice_required` next to plans a new one may
//! continue.

use std::sync::Arc;

use daruma_api_dto::MutationWarning;
use daruma_core::{repos::PlanRepository, Command, CommandHandler};
use daruma_domain::{
    Actor, AutoAppendPatch, GitContext, IntakeSourceMode, IntakeSourcePolicy, NewPlan, NewTask,
    Plan, PlanStatus, SourceChannel, SourceDeriveRule, SourceInput, Status,
};
use daruma_events::{Event, EventBus, EventEnvelope, EventStore};
use daruma_shared::{CoreError, PlanId, ProjectId};
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
                    plan_id: None,
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
async fn materialize_inherits_git_context_without_overwriting_reused_tasks() {
    let s = stack().await;
    let task_ids = [daruma_shared::TaskId::new(), daruma_shared::TaskId::new()];
    let make = |branch: &str| {
        let mut plan = s.new_plan();
        plan.source = Some(SourceInput {
            label: Some("Chat request".into()),
            ..Default::default()
        });
        plan.git_context = Some(GitContext {
            branch: Some(branch.into()),
            ..Default::default()
        });
        Command::MaterializePlan {
            plan,
            tasks: task_ids
                .iter()
                .map(|id| NewTask {
                    id: Some(*id),
                    external_key: Some(id.to_string()),
                    ..NewTask::new("Task")
                })
                .collect(),
            plan_id: None,
        }
    };
    s.handler
        .handle(make(" feat/x "), Actor::user())
        .await
        .unwrap();
    for id in task_ids {
        let task = s.handler.tasks.get(id).await.unwrap().unwrap();
        assert_eq!(task.git_context.unwrap().branch.as_deref(), Some("feat/x"));
    }

    // NewTask has no context field; give an existing task its own context.
    s.handler
        .handle(
            Command::UpdateTask {
                id: task_ids[0],
                patch: daruma_domain::TaskPatch {
                    git_context: Some(Some(GitContext {
                        branch: Some("feat/own".into()),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let before = s.handler.tasks.get_many(&task_ids).await.unwrap();
    let events = s
        .handler
        .handle(make("feat/other"), Actor::user())
        .await
        .unwrap();
    assert!(events.iter().all(|e| !matches!(
        e.payload,
        Event::TaskCreated { .. } | Event::TaskUpdated { .. }
    )));
    assert_eq!(s.handler.tasks.get_many(&task_ids).await.unwrap(), before);
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
    plan.separate = true;
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
    plan.separate = true;
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
        plan.separate = true;
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

/// `note_required` is also met by the nearest node's `note` (incoming or
/// stored), or a `label` on a `note:` node, on materialize, plan_create and
/// extend. A label on any other node explains nothing.
#[tokio::test]
async fn note_required_channel_is_met_by_node_note_or_label() {
    let s = stack().await;
    // Source-less plans for the extend path, before enforce forbids them.
    let (bare_ok, _) = s.materialize(s.new_plan()).await;
    let (bare_bad, _) = s.materialize(s.new_plan()).await;
    s.policy(IntakeSourcePolicy {
        channels: ["self", "note"]
            .map(|scheme| SourceChannel {
                scheme: scheme.into(),
                pattern: None,
                label: None,
                note_required: true,
            })
            .into(),
        ..policy(IntakeSourceMode::Enforce)
    })
    .await;
    let node = |r: &str, label: Option<&str>, note: Option<&str>| SourceInput {
        source_ref: Some(r.into()),
        label: label.map(Into::into),
        note: note.map(Into::into),
        ..SourceInput::default()
    };
    let with = |source: SourceInput| {
        let mut plan = s.new_plan();
        plan.source = Some(source);
        plan
    };
    let reason = |err: CoreError| match err {
        CoreError::Unprocessable { code, details, .. } => {
            assert_eq!(code, "plan_source_required");
            details["reason"].as_str().unwrap().to_owned()
        }
        other => panic!("expected Unprocessable, got {other:?}"),
    };

    // Incoming node: a note passes; a label passes only on a `note:` node.
    let (_, warnings) = s
        .materialize(with(node("self://a", None, Some("owner asked"))))
        .await;
    assert!(warnings.is_empty(), "{warnings:?}");
    let err = s
        .try_materialize(with(node("self://b", Some("Chat"), None)), Actor::user())
        .await
        .unwrap_err();
    assert_eq!(reason(err), "plan_source_note_required");
    let (stored, warnings) = s
        .materialize(with(SourceInput {
            label: Some("Call with the owner".into()),
            ..SourceInput::default()
        }))
        .await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(stored.source_ref.unwrap().starts_with("note:"));

    // Stored node: a bare ref to `self://a` carries its stored note (plan_create).
    let mut plan = s.new_plan();
    plan.source_ref = Some("self://a".into());
    s.handler
        .handle(
            Command::CreatePlan {
                plan,
                external_ref: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();

    // Nothing explains it → 422.
    let err = s
        .try_materialize(with(node("self://c", None, None)), Actor::user())
        .await
        .unwrap_err();
    assert_eq!(reason(err), "plan_source_note_required");

    // Extend `{plan_id, source}`: a note passes, a bare ref is a 422.
    let set = |plan_id, source| Command::ExtendSource {
        plan_id: Some(plan_id),
        source_ref: None,
        source: Some(source),
        upstream: vec![],
    };
    s.handler
        .handle(
            set(bare_ok.id, node("self://d", None, Some("why"))),
            Actor::user(),
        )
        .await
        .unwrap();
    let err = s
        .handler
        .handle(
            set(bare_bad.id, node("self://e", None, None)),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert_eq!(reason(err), "plan_source_note_required");
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
    // The ref-only source node rides ahead of the plan (ADR-0009 chain).
    let plan = outcome
        .events
        .iter()
        .find_map(|e| match &e.payload {
            Event::PlanCreated { plan } => Some(plan),
            _ => None,
        })
        .expect("PlanCreated");
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

#[tokio::test]
async fn raw_looking_plan_gets_plan_looks_raw_warning_only() {
    let s = stack().await;
    let raw = |plan: NewPlan, tasks: Vec<NewTask>| {
        let h = &s.handler;
        async move {
            h.handle_with_warnings(
                Command::MaterializePlan {
                    plan,
                    tasks,
                    plan_id: None,
                },
                Actor::user(),
            )
            .await
            .unwrap()
            .warnings
        }
    };
    let mut p = s.new_plan();
    p.title = "идея: подумать о кэше".into();
    let w = raw(p, vec![NewTask::new("исследовать")]).await;
    assert!(codes(&w).contains(&"plan_looks_raw"), "{w:?}");

    // A goal, or more than one task, means decided work.
    let mut p = s.new_plan();
    p.title = "idea: cache".into();
    p.goal = Some("ship cache".into());
    let w = raw(p, vec![NewTask::new("explore")]).await;
    assert!(!codes(&w).contains(&"plan_looks_raw"), "{w:?}");
    let mut p = s.new_plan();
    p.title = "idea: cache".into();
    let w = raw(p, vec![NewTask::new("a"), NewTask::new("b")]).await;
    assert!(!codes(&w).contains(&"plan_looks_raw"), "{w:?}");
}

// ── plan_choice_required ─────────────────────────────────────────────────────

impl Stack {
    async fn try_materialize_into(
        &self,
        plan: NewPlan,
        plan_id: Option<PlanId>,
    ) -> Result<Vec<Event>, CoreError> {
        let envs = self
            .handler
            .handle(
                Command::MaterializePlan {
                    plan,
                    tasks: vec![NewTask::new("next")],
                    plan_id,
                },
                Actor::user(),
            )
            .await?;
        Ok(envs.into_iter().map(|e| e.payload).collect())
    }

    async fn plan_count(&self) -> usize {
        self.plans
            .list_by_project(self.project, None)
            .await
            .unwrap()
            .len()
    }

    async fn complete(&self, plan_id: PlanId) {
        for member in self.plans.list_tasks_ordered(plan_id).await.unwrap() {
            self.handler
                .handle(
                    Command::SetStatus {
                        id: member.task_id,
                        status: Status::Done,
                        force: false,
                        override_reason: None,
                        comment: None,
                    },
                    Actor::user(),
                )
                .await
                .unwrap();
        }
        if self.plans.get(plan_id).await.unwrap().unwrap().status == PlanStatus::Completed {
            return;
        }
        self.handler
            .handle(
                Command::SetPlanStatus {
                    plan_id,
                    status: PlanStatus::Completed,
                    force: false,
                    override_reason: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let stored = self.plans.get(plan_id).await.unwrap().unwrap();
        assert_eq!(stored.status, PlanStatus::Completed);
    }
}

const ISSUE: &str = "https://gitlab.x/g/p/-/issues/11542";

/// A label-only node (chat/meeting) under the issue, on a git branch.
fn under_issue(s: &Stack, label: &str, branch: &str) -> NewPlan {
    let mut plan = s.new_plan();
    plan.source = Some(SourceInput {
        label: Some(label.into()),
        upstream: vec![SourceInput {
            source_ref: Some(ISSUE.into()),
            ..SourceInput::default()
        }],
        ..SourceInput::default()
    });
    plan.git_context = Some(GitContext {
        repo: Some("g/p".into()),
        branch: Some(branch.into()),
        ..GitContext::default()
    });
    plan
}

fn choice_candidates(err: &CoreError) -> Vec<serde_json::Value> {
    match err {
        CoreError::CodedConflict {
            code: "plan_choice_required",
            details,
            ..
        } => details["candidates"].as_array().unwrap().clone(),
        other => panic!("expected plan_choice_required, got {other:?}"),
    }
}

/// The incident: plan «statuses» was completed minutes ago on branch
/// 11542_*; a second materialize on that branch without a choice lists it
/// (the server does not decide), and `plan.id` keeps one plan, reopened.
#[tokio::test]
async fn plan_choice_required_lists_recent_completed_plan_on_same_branch() {
    let s = stack().await;
    let (statuses, _) = s
        .materialize(under_issue(
            &s,
            "Совещание с директором",
            "11542_av_statuses",
        ))
        .await;
    s.complete(statuses.id).await;

    let err = s
        .try_materialize_into(under_issue(&s, "Бэклог диалога", "11542_av_statuses"), None)
        .await
        .unwrap_err();
    let candidates = choice_candidates(&err);
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(candidates[0]["plan_id"], json!(statuses.id));
    assert_eq!(candidates[0]["status"], "completed");
    assert_eq!(candidates[0]["recent_tasks"], json!(["t"]));
    assert_eq!(
        candidates[0]["source_label_chain"],
        json!(["Совещание с директором", ISSUE])
    );
    assert!(err.to_string().contains("plan.separate"), "{err}");
    assert_eq!(s.plan_count().await, 1, "a refusal writes nothing");

    // Same subject: append by id — still one plan, active again.
    let events = s
        .try_materialize_into(
            under_issue(&s, "Бэклог диалога", "11542_av_statuses"),
            Some(statuses.id),
        )
        .await
        .unwrap();
    assert!(events
        .iter()
        .all(|e| !matches!(e, Event::PlanCreated { .. })));
    assert_eq!(s.plan_count().await, 1);
    let stored = s.plans.get(statuses.id).await.unwrap().unwrap();
    assert_eq!(stored.status, PlanStatus::Active);
    assert_eq!(
        s.plans.list_tasks_ordered(statuses.id).await.unwrap().len(),
        2
    );
}

/// Another subject next to candidates: `plan.separate` creates a new plan.
#[tokio::test]
async fn separate_plan_is_created_next_to_candidates() {
    let s = stack().await;
    s.materialize(under_issue(&s, "Совещание", "11542_av_statuses"))
        .await;
    let mut plan = under_issue(&s, "Резолвер", "11542_av_statuses");
    plan.separate = true;
    let events = s.try_materialize_into(plan, None).await.unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::PlanCreated { .. })));
    assert_eq!(s.plan_count().await, 2);
}

/// A trunk branch says nothing about the subject, and an unrelated source
/// chain is no evidence; a plan whose source is an ancestor of the incoming
/// one is a candidate.
#[tokio::test]
async fn trunk_branch_and_foreign_chain_give_no_candidates() {
    let s = stack().await;
    let on_main = |source_ref: &str| {
        let mut plan = s.new_plan();
        plan.source_ref = Some(source_ref.into());
        plan.git_context = Some(GitContext {
            repo: Some("g/p".into()),
            branch: Some("main".into()),
            ..GitContext::default()
        });
        plan
    };
    s.materialize(on_main("https://gitlab.x/g/p/-/issues/1"))
        .await;
    s.try_materialize_into(on_main("https://gitlab.x/g/p/-/issues/2"), None)
        .await
        .unwrap();
    for branch in ["master", "Develop", "dev", "trunk"] {
        let mut plan = on_main(&format!("https://gitlab.x/g/p/-/issues/{branch}"));
        plan.git_context.as_mut().unwrap().branch = Some(branch.into());
        s.try_materialize_into(plan, None).await.unwrap();
    }
    assert_eq!(s.plan_count().await, 6);

    // Same chain, upward: issue 1 is the upstream of the incoming note.
    let mut plan = under_issue(&s, "Обсуждение", "main");
    plan.source.as_mut().unwrap().upstream[0].source_ref =
        Some("https://gitlab.x/g/p/-/issues/1".into());
    let err = s.try_materialize_into(plan, None).await.unwrap_err();
    assert_eq!(choice_candidates(&err).len(), 1);
}

/// A completed plan older than the window is history, not a choice.
#[tokio::test]
async fn completed_plan_older_than_a_week_is_no_candidate() {
    let s = stack().await;
    let (old, _) = s
        .materialize(under_issue(&s, "Совещание", "11542_av_statuses"))
        .await;
    s.complete(old.id).await;
    let mut env = EventEnvelope::new(
        Actor::user(),
        Event::PlanStatusChanged {
            plan_id: old.id,
            from: PlanStatus::Completed,
            to: PlanStatus::Completed,
        },
    );
    env.occurred_at = daruma_shared::time::now() - chrono::Duration::days(8);
    s.plans.apply_event(&env).await.unwrap();

    s.try_materialize_into(under_issue(&s, "Бэклог", "11542_av_statuses"), None)
        .await
        .unwrap();
    assert_eq!(s.plan_count().await, 2);
}
