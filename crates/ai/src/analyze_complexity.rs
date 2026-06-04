//! Batch task complexity analysis (§3.8.3).
//!
//! Given a slice of `TaskBrief` rows, issue **one** LLM call and return a
//! `ComplexityHint` per task. The whole point of batching is to amortise
//! one prompt across N tasks rather than calling decompose N times — see
//! ROADMAP §3.8 (CTM A.1).

use serde_json::{json, Value};
use std::collections::HashMap;
use taskagent_domain::{ComplexityHint, TaskBrief};
use taskagent_shared::{time, CoreError, TaskId};

use crate::client::{OpenAiClient, ResponseOutput, ResponseRequest};

/// Hard cap on tasks per batch. Keeps prompt size predictable and the
/// model's per-task attention non-degenerate. Callers with more tasks
/// should chunk; we do not split for them here so the contract stays
/// "one LLM call per call".
pub const MAX_BATCH_TASKS: usize = 50;

/// Function tool the model is forced to call. Mirrors the projection
/// row but is structurally separate so future schema drift in storage
/// doesn't accidentally reshape the LLM contract.
fn analyze_complexity_tool() -> Value {
    json!({
        "type": "function",
        "name": "report_complexity",
        "description": "Report a complexity score for each task in the batch. \
                        Higher score => larger decomposition warranted.",
        "parameters": {
            "type": "object",
            "properties": {
                "hints": {
                    "type": "array",
                    "description": "One entry per input task, in the same order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task_id":              {"type": "string"},
                            "score":                {"type": "integer", "minimum": 1, "maximum": 10},
                            "recommended_subtasks": {"type": "integer", "minimum": 0, "maximum": 20},
                            "expansion_hint":       {"type": "string"},
                            "reasoning":            {"type": "string"}
                        },
                        "required": [
                            "task_id", "score", "recommended_subtasks",
                            "expansion_hint", "reasoning"
                        ],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["hints"],
            "additionalProperties": false
        }
    })
}

/// Build the system+user prompt fed to the model. The framing is loaded
/// from `prompts/analyze_complexity.toml` (§3.8.5); the per-task list is
/// rendered here and substituted into the `tasks_list` template variable.
fn build_prompt(tasks: &[TaskBrief]) -> String {
    let mut tasks_list = String::new();
    for (i, t) in tasks.iter().enumerate() {
        tasks_list.push_str(&format!("{}. [{}] {}\n", i + 1, t.task_id, t.title.trim()));
        if !t.description.is_empty() {
            // Indent so the model can see this is body, not a new task.
            for line in t.description.lines() {
                tasks_list.push_str("    ");
                tasks_list.push_str(line);
                tasks_list.push('\n');
            }
        }
    }
    #[derive(serde::Serialize)]
    struct Ctx<'a> {
        tasks_list: &'a str,
    }
    crate::prompts::PromptRegistry::load(
        "analyze_complexity",
        "default",
        &Ctx {
            tasks_list: &tasks_list,
        },
    )
    .expect("bundled analyze_complexity prompt is well-formed")
}

/// Run one batch complexity analysis. Returns hints in the same order as
/// `tasks` (when the model returns a row per input). Tasks the model
/// omits from its response are simply absent in the returned vec.
pub async fn analyze_complexity_batch(
    client: &OpenAiClient,
    tasks: Vec<TaskBrief>,
) -> Result<Vec<ComplexityHint>, CoreError> {
    if tasks.is_empty() {
        return Ok(vec![]);
    }
    if tasks.len() > MAX_BATCH_TASKS {
        return Err(CoreError::validation(format!(
            "analyze_complexity_batch: batch size {} exceeds MAX_BATCH_TASKS={}",
            tasks.len(),
            MAX_BATCH_TASKS
        )));
    }

    let prompt = build_prompt(&tasks);
    let req = ResponseRequest {
        input: Value::String(prompt),
        tools: vec![analyze_complexity_tool()],
        tool_choice: Some("required".into()),
    };

    let outputs = client.respond(req).await.map_err(CoreError::from)?;
    let tc = outputs
        .into_iter()
        .find_map(|o| match o {
            ResponseOutput::ToolCall(tc) if tc.name == "report_complexity" => Some(tc),
            _ => None,
        })
        .ok_or_else(|| {
            CoreError::ai("analyze_complexity_batch: model returned no report_complexity call")
        })?;

    let args: Value =
        serde_json::from_str(&tc.arguments).map_err(|e| CoreError::serde(e.to_string()))?;
    let raw_hints = args["hints"]
        .as_array()
        .ok_or_else(|| CoreError::validation("report_complexity: missing 'hints' array"))?;

    let batch_id = uuid::Uuid::now_v7().to_string();
    let generated_at = time::now();

    // Index inputs by id so we can validate the model's `task_id`s.
    let valid_ids: HashMap<String, TaskId> = tasks
        .iter()
        .map(|t| (t.task_id.to_string(), t.task_id))
        .collect();

    let mut out = Vec::with_capacity(raw_hints.len());
    for item in raw_hints {
        let Some(id_s) = item["task_id"].as_str() else {
            continue;
        };
        // Accept either prefixed (tsk_...) or bare UUID; resolve against
        // the input set so the model can't invent task ids.
        let task_id = match valid_ids.get(id_s) {
            Some(id) => *id,
            None => match id_s.parse::<TaskId>() {
                Ok(parsed) if valid_ids.values().any(|v| *v == parsed) => parsed,
                _ => continue,
            },
        };

        let score = item["score"].as_i64().unwrap_or(0).clamp(1, 10) as u8;
        let recommended_subtasks = item["recommended_subtasks"]
            .as_i64()
            .unwrap_or(0)
            .clamp(0, 20) as u8;
        let expansion_hint = item["expansion_hint"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_owned();
        let reasoning = item["reasoning"].as_str().unwrap_or("").trim().to_owned();

        out.push(ComplexityHint {
            task_id,
            score,
            recommended_subtasks,
            expansion_hint,
            reasoning,
            generated_at,
            batch_id: batch_id.clone(),
        });
    }

    Ok(out)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_schema_has_required_fields() {
        let t = analyze_complexity_tool();
        assert_eq!(t["type"], "function");
        assert_eq!(t["name"], "report_complexity");
        let req = t["parameters"]["properties"]["hints"]["items"]["required"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = req.iter().filter_map(|v| v.as_str()).collect();
        for f in [
            "task_id",
            "score",
            "recommended_subtasks",
            "expansion_hint",
            "reasoning",
        ] {
            assert!(names.contains(&f), "schema missing field: {f}");
        }
    }

    #[test]
    fn prompt_lists_every_task_with_id() {
        let a = TaskId::new();
        let b = TaskId::new();
        let prompt = build_prompt(&[
            TaskBrief {
                task_id: a,
                title: "Wire DB layer".into(),
                description: "".into(),
            },
            TaskBrief {
                task_id: b,
                title: "Add MCP tool".into(),
                description: "two-line\nbody".into(),
            },
        ]);
        assert!(prompt.contains(&a.to_string()));
        assert!(prompt.contains(&b.to_string()));
        assert!(prompt.contains("Wire DB layer"));
        assert!(prompt.contains("two-line"));
    }

    #[test]
    fn max_batch_constant_is_reasonable() {
        // Stability guard: if we ever change this, callers need to know.
        const _: () = assert!(MAX_BATCH_TASKS >= 10 && MAX_BATCH_TASKS <= 200);
    }
}
