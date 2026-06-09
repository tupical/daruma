//! Task scope adjustment (§3.8.7) — broaden or narrow an existing task.
//!
//! The model rewrites the task's title + description at a target
//! complexity level (`up` = broader / epic-style, `down` = narrower /
//! one concrete action) and the server turns the result into a
//! `Command::UpdateTask`.
//!
//! `strength` (light/regular/heavy) is intentionally absent here — the
//! task description marks it as deferred to §3.8.7a. The wire shape
//! accepts the field today only at the HTTP / MCP boundary so callers
//! can author against the final shape; this function does not consume
//! it yet.

use serde::Serialize;
use serde_json::Value;
use taskagent_core::Command;
use taskagent_domain::{Task, TaskPatch};
use taskagent_shared::CoreError;

use crate::{client::OpenAiClient, prompts::PromptRegistry, provider::AiProvider, tools};

/// Direction the rewrite should move in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeDirection {
    Up,
    Down,
}

impl ScopeDirection {
    pub fn as_variant(self) -> &'static str {
        match self {
            ScopeDirection::Up => "up",
            ScopeDirection::Down => "down",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, CoreError> {
        match raw {
            "up" | "broaden" => Ok(ScopeDirection::Up),
            "down" | "narrow" => Ok(ScopeDirection::Down),
            other => Err(CoreError::validation(format!(
                "unknown scope direction: {other} (expected 'up' or 'down')"
            ))),
        }
    }
}

#[derive(Serialize)]
struct ScopeCtx<'a> {
    title: &'a str,
    description: &'a str,
}

/// Build the scope prompt. Pure — exposed for tests.
pub fn build_scope_prompt(task: &Task, direction: ScopeDirection) -> String {
    let title = crate::untrusted::wrap_untrusted("task title", &task.title);
    let description = crate::untrusted::wrap_untrusted("task description", &task.description);
    PromptRegistry::load(
        "scope",
        direction.as_variant(),
        &ScopeCtx {
            title: &title,
            description: &description,
        },
    )
    .expect("bundled scope prompt is well-formed")
}

/// Ask the model to rescope `task`, returning a `Command::UpdateTask`
/// patch with the rewritten title + description.
pub async fn scope_task(
    client: &OpenAiClient,
    task: &Task,
    direction: ScopeDirection,
) -> Result<Command, CoreError> {
    let prompt = build_scope_prompt(task, direction);
    let args: Value = client
        .generate_object(prompt, vec![tools::rescope_task_tool()], "rescope_task")
        .await?;

    let title = args["title"]
        .as_str()
        .ok_or_else(|| CoreError::ai("rescope_task: missing 'title' in tool args"))?
        .trim()
        .to_owned();
    if title.is_empty() {
        return Err(CoreError::ai("rescope_task: empty title"));
    }
    let description = args["description"].as_str().unwrap_or("").to_owned();

    let patch = TaskPatch {
        title: Some(title),
        description: Some(description),
        ..TaskPatch::default()
    };
    Ok(Command::UpdateTask { id: task.id, patch })
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskagent_domain::{Priority, Status};
    use taskagent_shared::{time, ProjectId, TaskId};

    fn sample_task() -> Task {
        let now = time::now();
        Task {
            id: TaskId::new(),
            project_id: Some(ProjectId::new()),
            title: "Wire login form".into(),
            description: "Connect to /v1/auth/login and store the bearer token.".into(),
            status: Status::Todo,
            priority: Priority::P2,
            triage_state: None,
            due_at: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
            created_by: None,
            completed_by: None,
            updated_by: None,
            updated_event_id: None,
            updated_event_seq: None,
            source_event_id: None,
        }
    }

    #[test]
    fn parse_direction_accepts_canonical_and_synonyms() {
        assert_eq!(ScopeDirection::parse("up").unwrap(), ScopeDirection::Up);
        assert_eq!(
            ScopeDirection::parse("broaden").unwrap(),
            ScopeDirection::Up
        );
        assert_eq!(ScopeDirection::parse("down").unwrap(), ScopeDirection::Down);
        assert_eq!(
            ScopeDirection::parse("narrow").unwrap(),
            ScopeDirection::Down
        );
        assert!(ScopeDirection::parse("sideways").is_err());
    }

    #[test]
    fn up_prompt_contains_task_body_and_broaden_framing() {
        let t = sample_task();
        let p = build_scope_prompt(&t, ScopeDirection::Up);
        assert!(p.contains("Wire login form"));
        assert!(p.contains("/v1/auth/login"));
        assert!(p.contains("Broaden"));
    }

    #[test]
    fn down_prompt_contains_task_body_and_narrow_framing() {
        let t = sample_task();
        let p = build_scope_prompt(&t, ScopeDirection::Down);
        assert!(p.contains("Wire login form"));
        assert!(p.contains("Narrow"));
    }
}
