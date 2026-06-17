//! Project summarisation: event log → human-readable `String`.

use serde::Serialize;
use serde_json::Value;
use taskagent_events::EventEnvelope;
use taskagent_shared::{CoreError, ProjectId};

use taskagent_ai_infra::{
    client::{OpenAiClient, ResponseOutput, ResponseRequest},
    untrusted::wrap_untrusted,
};

use crate::prompts::PromptRegistry;

#[derive(Serialize)]
struct SummarizeCtx<'a> {
    project_id: String,
    events_json: &'a str,
}

/// Summarise the event history of a project into a concise narrative string.
///
/// `events` should be the full (or recent) event log for the project,
/// sorted chronologically. The function is read-only — it never emits
/// a `Command` or touches storage.
pub async fn summarize_project(
    client: &OpenAiClient,
    project_id: ProjectId,
    events: &[EventEnvelope],
) -> Result<String, CoreError> {
    let events_json =
        serde_json::to_string_pretty(events).map_err(|e| CoreError::serde(e.to_string()))?;

    let prompt = PromptRegistry::load(
        "summarize",
        "default",
        &SummarizeCtx {
            project_id: project_id.to_string(),
            events_json: &wrap_untrusted("event log", &events_json),
        },
    )?;

    let req = ResponseRequest {
        input: Value::String(prompt),
        tools: vec![],
        tool_choice: None,
    };

    let outputs = client.respond(req).await.map_err(CoreError::from)?;

    extract_first_message(outputs)
        .ok_or_else(|| CoreError::ai("summarize_project: model returned no text"))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_first_message(outputs: Vec<ResponseOutput>) -> Option<String> {
    outputs.into_iter().find_map(|o| match o {
        ResponseOutput::Message(text) => Some(text),
        _ => None,
    })
}
