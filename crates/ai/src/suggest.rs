//! Next-action suggestion: context → human-readable `String`.

use serde::Serialize;
use serde_json::Value;
use taskagent_shared::CoreError;

use crate::{
    client::{OpenAiClient, ResponseOutput, ResponseRequest},
    prompts::PromptRegistry,
};

#[derive(Serialize)]
struct SuggestCtx<'a> {
    context: &'a str,
}

/// Suggest the single most impactful next action given the current context.
///
/// `context` is a free-form text description of the current project or task
/// state (e.g. a rendered task list with statuses). Returns one short
/// suggestion as a plain string — no command is emitted.
pub async fn suggest_next_action(
    client: &OpenAiClient,
    context: &str,
) -> Result<String, CoreError> {
    let context = crate::untrusted::wrap_untrusted("project context", context);
    let prompt = PromptRegistry::load("suggest", "default", &SuggestCtx { context: &context })?;

    let req = ResponseRequest {
        input: Value::String(prompt),
        tools: vec![],
        tool_choice: None,
    };

    let outputs = client.respond(req).await.map_err(CoreError::from)?;

    outputs
        .into_iter()
        .find_map(|o| match o {
            ResponseOutput::Message(text) => Some(text),
            _ => None,
        })
        .ok_or_else(|| CoreError::ai("suggest_next_action: model returned no text"))
}
