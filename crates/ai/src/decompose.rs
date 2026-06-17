//! Task decomposition: natural language → `Command::SplitTask`.

use serde::Serialize;
use serde_json::Value;
use taskagent_core::Command;
use taskagent_domain::NewTask;
use taskagent_shared::{CoreError, TaskId};

use taskagent_ai_infra::{
    client::{OpenAiClient, ResponseOutput, ResponseRequest},
    tools::split_task_tool,
    untrusted::wrap_untrusted,
};

use crate::prompts::PromptRegistry;

#[derive(Serialize)]
struct DecomposeCtx<'a> {
    task_context: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
}

/// Build the decomposition prompt. Pure — kept separate so callers and tests
/// can inspect the exact string sent to the model without going through
/// `OpenAiClient`.
///
/// When `hint` is `Some`, the `with_hint` variant (which appends an
/// "Additional guidance" block) is rendered. When `None`, the default
/// variant is rendered — byte-identical to the pre-§3.8.4 / pre-§3.8.5
/// version (back-compat).
///
/// Panics only if the bundled `prompts/decompose.toml` is malformed — a
/// build-time invariant covered by `PromptRegistry`'s test suite.
pub fn build_decompose_prompt(task_context: &str, hint: Option<&str>) -> String {
    let task_context = &wrap_untrusted("task context", task_context);
    let trimmed = hint.map(str::trim).filter(|s| !s.is_empty());
    let (variant, ctx) = match trimmed {
        Some(h) => (
            "with_hint",
            DecomposeCtx {
                task_context,
                hint: Some(h),
            },
        ),
        None => (
            "default",
            DecomposeCtx {
                task_context,
                hint: None,
            },
        ),
    };
    PromptRegistry::load("decompose", variant, &ctx)
        .expect("bundled decompose prompt is well-formed (verified by PromptRegistry tests)")
}

/// Decompose a parent task into sub-tasks using the AI model.
///
/// `task_context` should contain enough information for the model to produce
/// meaningful sub-tasks (e.g. the task title + description).
///
/// `hint` is an optional free-form guidance string (e.g. an `expansion_hint`
/// from §3.8.3 `taskagent_ai_analyze_complexity`). When supplied, it is
/// surfaced to the model as an "Additional guidance" block; when `None`, the
/// prompt is unchanged from the pre-hint baseline.
///
/// Returns `Command::SplitTask { parent, subtasks }`.
pub async fn decompose_task(
    client: &OpenAiClient,
    parent: TaskId,
    task_context: &str,
    hint: Option<&str>,
) -> Result<Command, CoreError> {
    let prompt = build_decompose_prompt(task_context, hint);

    let req = ResponseRequest {
        input: Value::String(prompt),
        tools: vec![split_task_tool()],
        tool_choice: Some("required".into()),
    };

    let outputs = client.respond(req).await.map_err(CoreError::from)?;

    let tc = outputs
        .into_iter()
        .find_map(|o| match o {
            ResponseOutput::ToolCall(tc) if tc.name == "split_task" => Some(tc),
            _ => None,
        })
        .ok_or_else(|| CoreError::ai("decompose_task: model returned no split_task call"))?;

    let args: Value =
        serde_json::from_str(&tc.arguments).map_err(|e| CoreError::serde(e.to_string()))?;

    let raw_subtasks = args["subtasks"]
        .as_array()
        .ok_or_else(|| CoreError::validation("split_task: missing 'subtasks' array"))?;

    if raw_subtasks.len() < 2 {
        return Err(CoreError::validation(
            "split_task: must produce at least 2 sub-tasks",
        ));
    }

    let subtasks: Vec<NewTask> = raw_subtasks
        .iter()
        .map(|item| {
            let title = item["title"].as_str().unwrap_or("(untitled)").to_owned();
            let mut t = NewTask::new(title);
            if let Some(desc) = item["description"].as_str() {
                t.description = Some(desc.to_owned());
            }
            t
        })
        .collect();

    Ok(Command::SplitTask { parent, subtasks })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the `default` variant head of crates/ai/prompts/decompose.toml
    // up to (and including) the `Task:` label — the standard instruction
    // framing every decompose prompt opens with, including the Rules block.
    const BASE_HEAD: &str = "You are a project-management assistant. Decompose the following task \
         into 2–6 concrete, actionable sub-tasks. Call split_task with the \
         result.\n\n\
         Rules:\n\
         - Each subtask should be independently executable and verifiable.\n\
         - Write subtask titles as short imperative actions.\n\
         - Preserve dependencies, constraints, links, and acceptance criteria in the relevant descriptions.\n\
         - Do not create meta-subtasks for maintaining TODO.md, scratchpads, or in-chat checklists unless the user explicitly requested those artifacts.\n\n\
         Task:\n";

    #[test]
    fn prompt_without_hint_keeps_legacy_framing_and_fences_context() {
        let p = build_decompose_prompt("Build login page", None);
        // Same instruction head as the pre-§3.8.4 prompt; the task body is
        // now fenced as untrusted data (prompt-injection hardening).
        assert!(p.starts_with(BASE_HEAD));
        assert!(p.contains("Build login page"));
        assert!(p.contains(taskagent_ai_infra::untrusted::UNTRUSTED_OPEN));
        assert!(p.contains(taskagent_ai_infra::untrusted::UNTRUSTED_CLOSE));
        assert!(!p.contains("Additional guidance"));
    }

    #[test]
    fn prompt_with_hint_appends_guidance_block() {
        let p = build_decompose_prompt(
            "Build login page",
            Some("Focus on OAuth flows before form-based fallback."),
        );
        assert!(p.starts_with(BASE_HEAD));
        assert!(p.contains("Build login page"));
        assert!(p.contains("\n\nAdditional guidance:\n"));
        assert!(p.contains("Focus on OAuth flows before form-based fallback."));
    }

    #[test]
    fn empty_or_whitespace_hint_is_treated_as_none() {
        let baseline = build_decompose_prompt("ctx", None);
        assert_eq!(build_decompose_prompt("ctx", Some("")), baseline);
        assert_eq!(build_decompose_prompt("ctx", Some("   \n\t  ")), baseline);
    }
}
