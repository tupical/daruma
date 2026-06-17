//! Natural language → `Command::CreateTask` parser.

use serde::Serialize;
use serde_json::Value;
use taskagent_core::Command;
use taskagent_domain::{NewTask, Priority, Status};
use taskagent_shared::CoreError;

use taskagent_ai_infra::{
    client::{OpenAiClient, ResponseOutput, ResponseRequest},
    tools::create_task_tool,
    wrap_untrusted,
};

use crate::prompts::PromptRegistry;

#[derive(Serialize)]
struct ParseCtx<'a> {
    input: &'a str,
}

/// Parse a natural language task description into a `Command::CreateTask`.
///
/// Calls the OpenAI Responses API with the `create_task` function tool and
/// maps the returned arguments onto [`NewTask`].
pub async fn parse_task(client: &OpenAiClient, input: &str) -> Result<Command, CoreError> {
    let input = wrap_untrusted("task description to parse", input);
    let prompt = PromptRegistry::load("parse", "default", &ParseCtx { input: &input })?;

    let req = ResponseRequest {
        input: Value::String(prompt),
        tools: vec![create_task_tool()],
        tool_choice: Some("required".into()),
    };

    let outputs = client.respond(req).await.map_err(CoreError::from)?;

    let tc = outputs
        .into_iter()
        .find_map(|o| match o {
            ResponseOutput::ToolCall(tc) if tc.name == "create_task" => Some(tc),
            _ => None,
        })
        .ok_or_else(|| CoreError::ai("parse_task: model returned no create_task call"))?;

    let args: Value =
        serde_json::from_str(&tc.arguments).map_err(|e| CoreError::serde(e.to_string()))?;

    let title = args["title"]
        .as_str()
        .ok_or_else(|| CoreError::validation("create_task: missing required field 'title'"))?
        .to_owned();

    let mut task = NewTask::new(title);

    if let Some(desc) = args["description"].as_str() {
        task.description = Some(desc.to_owned());
    }
    if let Some(p) = args["priority"].as_str() {
        task.priority = parse_priority(p);
    }
    if let Some(s) = args["status"].as_str() {
        task.status = parse_status(s);
    }

    Ok(Command::CreateTask { task })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_priority(s: &str) -> Option<Priority> {
    match s {
        "p0" => Some(Priority::P0),
        "p1" => Some(Priority::P1),
        "p2" => Some(Priority::P2),
        "p3" => Some(Priority::P3),
        _ => None,
    }
}

fn parse_status(s: &str) -> Option<Status> {
    match s {
        "inbox" => Some(Status::Inbox),
        "todo" => Some(Status::Todo),
        "in_progress" => Some(Status::InProgress),
        "in_review" => Some(Status::InReview),
        "done" => Some(Status::Done),
        "cancelled" => Some(Status::Cancelled),
        _ => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_priority_roundtrip() {
        assert!(matches!(parse_priority("p0"), Some(Priority::P0)));
        assert!(matches!(parse_priority("p1"), Some(Priority::P1)));
        assert!(matches!(parse_priority("p2"), Some(Priority::P2)));
        assert!(matches!(parse_priority("p3"), Some(Priority::P3)));
        assert!(parse_priority("unknown").is_none());
    }

    #[test]
    fn parse_status_roundtrip() {
        assert!(matches!(parse_status("inbox"), Some(Status::Inbox)));
        assert!(matches!(parse_status("todo"), Some(Status::Todo)));
        assert!(matches!(
            parse_status("in_progress"),
            Some(Status::InProgress)
        ));
        assert!(matches!(parse_status("in_review"), Some(Status::InReview)));
        assert!(matches!(parse_status("done"), Some(Status::Done)));
        assert!(matches!(parse_status("cancelled"), Some(Status::Cancelled)));
        assert!(parse_status("other").is_none());
    }

    #[test]
    fn status_as_str_matches_parse_input() {
        // Lock the wire-format symmetry: as_str produces strings parse_status accepts.
        for &s in &[
            Status::Inbox,
            Status::Todo,
            Status::InProgress,
            Status::InReview,
            Status::Done,
            Status::Cancelled,
        ] {
            assert_eq!(parse_status(s.as_str()), Some(s), "{s:?} round-trip");
        }
    }
}
