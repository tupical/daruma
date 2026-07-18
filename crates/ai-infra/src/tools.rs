//! Flat function-tool JSON schemas for the OpenAI Responses API.
//!
//! Each function mirrors a `Command` variant. When the schema changes
//! (new fields, renamed variants), update here first — events win over AI.
//!
//! Wire format:
//! ```json
//! { "type": "function", "name": "…", "description": "…", "parameters": { …JSON Schema… } }
//! ```

use serde_json::{json, Value};

/// Tool schema for §3.8.7 `daruma_ai_scope`. The model returns the
/// rewritten task body — the server turns it into `Command::UpdateTask`.
pub fn rescope_task_tool() -> Value {
    json!({
        "type": "function",
        "name": "rescope_task",
        "description": "Rewrite a task's title and description at a target complexity. `up` broadens scope into an epic-style framing; `down` narrows it into a single concrete action.",
        "parameters": {
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "New short, imperative title (≤120 chars)."
                },
                "description": {
                    "type": "string",
                    "description": "New body — acceptance criteria, steps, context. May be empty."
                }
            },
            "required": ["title", "description"],
            "additionalProperties": false
        }
    })
}

/// Tool schema for `Command::SplitTask`.
pub fn split_task_tool() -> Value {
    json!({
        "type": "function",
        "name": "split_task",
        "description": "Decompose a parent task into an ordered list of concrete sub-tasks.",
        "parameters": {
            "type": "object",
            "properties": {
                "subtasks": {
                    "type": "array",
                    "description": "Ordered sub-tasks the parent should be split into (at least 2).",
                    "minItems": 2,
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "Short, imperative sub-task title."
                            },
                            "description": {
                                "type": "string",
                                "description": "Optional detail or acceptance criteria."
                            }
                        },
                        "required": ["title"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["subtasks"],
            "additionalProperties": false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_task_tool_is_valid_schema() {
        let t = split_task_tool();
        assert_eq!(t["type"], "function");
        assert_eq!(t["name"], "split_task");
        assert_eq!(t["parameters"]["required"][0], "subtasks");
    }
}
