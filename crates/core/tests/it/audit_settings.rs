//! `UpdateProjectSettings.audit`: the thresholds object is replaced whole,
//! `null` removes it, a non-object is rejected, and a plugged validator
//! (the hosted audit engine's) turns its error into a validation error.

use std::sync::Arc;

use daruma_core::{AuditSettingsValidator, Command, CommandHandler};
use daruma_domain::{Actor, AutoAppendPatch};
use daruma_events::{Event, EventBus, EventStore};
use daruma_shared::{CoreError, ProjectId};
use daruma_storage::{
    ActivityRepo, CommentRepo, Db, ProjectRepo, ProjectSettingsRepo, SqliteEventStore, TaskRepo,
};
use serde_json::{json, Value};

async fn stack(
    validator: Option<AuditSettingsValidator>,
) -> (CommandHandler, Arc<ProjectSettingsRepo>, ProjectId) {
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let settings = Arc::new(ProjectSettingsRepo::new(pool.clone()));
    let mut handler = CommandHandler::new(
        store,
        Arc::new(TaskRepo::new(pool.clone())),
        Arc::new(ProjectRepo::new(pool.clone())),
        Arc::new(CommentRepo::new(pool.clone())),
        Arc::new(ActivityRepo::new(pool.clone())),
        EventBus::default(),
    )
    .with_project_settings(settings.clone());
    if let Some(validator) = validator {
        handler = handler.with_audit_settings_validator(validator);
    }
    let envs = handler
        .handle(
            Command::CreateProject {
                title: "Audit".into(),
                description: None,
            },
            Actor::user(),
        )
        .await
        .unwrap();
    let Event::ProjectCreated { project } = &envs[0].payload else {
        panic!("expected ProjectCreated");
    };
    (handler, settings, project.id)
}

fn set_audit(project: ProjectId, audit: Option<Option<Value>>) -> Command {
    Command::UpdateProjectSettings {
        project_id: project,
        auto_append: AutoAppendPatch::default(),
        intake_source: None,
        audit,
    }
}

#[tokio::test]
async fn audit_object_replaced_whole_kept_and_removed() {
    let (handler, settings, project) = stack(None).await;
    let first = json!({"usage_p95_bytes": 32768, "usage_retention": {"error_rate_pp": 1}});
    handler
        .handle(set_audit(project, Some(Some(first.clone()))), Actor::user())
        .await
        .unwrap();
    assert_eq!(settings.audit(project).await.unwrap(), Some(first.clone()));

    // A patch without `audit` (wire: key absent) keeps the stored object.
    let wire = json!({
        "type": "update_project_settings",
        "project_id": project,
        "auto_append": {"interview": false},
    });
    let command: Command = serde_json::from_value(wire).unwrap();
    handler.handle(command, Actor::user()).await.unwrap();
    assert_eq!(settings.audit(project).await.unwrap(), Some(first));

    // Wire `null` removes it.
    let wire = json!({
        "type": "update_project_settings",
        "project_id": project,
        "audit": null,
    });
    let command: Command = serde_json::from_value(wire).unwrap();
    assert!(matches!(
        &command,
        Command::UpdateProjectSettings {
            audit: Some(None),
            ..
        }
    ));
    handler.handle(command, Actor::user()).await.unwrap();
    assert!(settings.audit(project).await.unwrap().is_none());
}

#[tokio::test]
async fn audit_non_object_and_validator_errors_are_rejected() {
    let validator: AuditSettingsValidator = Arc::new(|value: &Value| {
        if value.get("usage_p95_bytes").is_some_and(|v| !v.is_u64()) {
            return Err("usage_p95_bytes must be an unsigned integer".into());
        }
        Ok(())
    });
    let (handler, settings, project) = stack(Some(validator)).await;

    for bad in [json!(32768), json!("x"), json!([1])] {
        let err = handler
            .handle(set_audit(project, Some(Some(bad))), Actor::user())
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)), "{err}");
    }
    let err = handler
        .handle(
            set_audit(project, Some(Some(json!({"usage_p95_bytes": "32k"})))),
            Actor::user(),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("audit settings: usage_p95_bytes must be an unsigned integer"),
        "{err}"
    );
    assert!(settings.audit(project).await.unwrap().is_none());

    handler
        .handle(
            set_audit(project, Some(Some(json!({"usage_p95_bytes": 32768})))),
            Actor::user(),
        )
        .await
        .unwrap();
    assert_eq!(
        settings.audit(project).await.unwrap(),
        Some(json!({"usage_p95_bytes": 32768}))
    );
}
