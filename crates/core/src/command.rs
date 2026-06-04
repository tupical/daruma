//! Canonical command schema. Every mutation in the system originates as
//! one of these. The AI layer emits commands; UI emits commands; the
//! handler turns them into events.
//!
//! Types are now defined in `crates/api-dto` (wasm-compatible) and
//! re-exported here for backward compatibility.

pub use taskagent_api_dto::command::{Command, CommandEnvelope};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use taskagent_domain::NewTask;
    use taskagent_shared::{AgentId, PlanId, ProjectId, RunId, TaskId};

    #[test]
    fn serde_roundtrip() {
        let cmd = Command::CreateTask {
            task: NewTask::new("Prepare taxes"),
        };
        let v = serde_json::to_value(&cmd).unwrap();
        assert_eq!(v["type"], "create_task");
        let back: Command = serde_json::from_value(v).unwrap();
        assert_eq!(back.kind(), "create_task");
    }

    #[test]
    fn complete_task_uses_snake_case_tag() {
        let id = TaskId::new();
        let cmd = Command::CompleteTask { id };
        let v = serde_json::to_value(&cmd).unwrap();
        assert_eq!(v["type"], "complete_task");
        assert_eq!(v["id"], json!(id));
    }

    #[test]
    fn create_plan_roundtrip() {
        use taskagent_domain::Actor;
        let cmd = Command::CreatePlan {
            plan: taskagent_domain::NewPlan::new("My plan", ProjectId::new(), Actor::user()),
            external_ref: None,
        };
        let v = serde_json::to_value(&cmd).unwrap();
        assert_eq!(v["type"], "create_plan");
        let back: Command = serde_json::from_value(v).unwrap();
        assert_eq!(back.kind(), "create_plan");
    }

    #[test]
    fn command_envelope_client_id_roundtrip() {
        let cmd = Command::CompleteRun {
            run_id: RunId::new(),
        };
        let key = uuid::Uuid::new_v4();
        let env = CommandEnvelope::by_user(cmd).with_idempotency_key(key);
        let json = serde_json::to_string(&env).unwrap();
        let back: CommandEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.client_command_id, Some(key));
    }

    #[test]
    fn acquire_claim_roundtrip() {
        let cmd = Command::AcquireClaim {
            agent_id: AgentId::new(),
            task_id: TaskId::new(),
            ttl_secs: 60,
        };
        let v = serde_json::to_value(&cmd).unwrap();
        assert_eq!(v["type"], "acquire_claim");
    }

    #[test]
    fn all_kinds_are_snake_case() {
        use taskagent_domain::Actor;
        let cmds = vec![
            Command::StartRun {
                plan_id: PlanId::new(),
                agent_id: AgentId::new(),
                parent_run_id: None,
            },
            Command::CompleteRun {
                run_id: RunId::new(),
            },
            Command::StartAgentSession {
                agent_id: AgentId::new(),
                parent_agent_id: None,
                metadata: None,
            },
            Command::AcquireClaim {
                agent_id: AgentId::new(),
                task_id: TaskId::new(),
                ttl_secs: 30,
            },
            Command::CreatePlan {
                plan: taskagent_domain::NewPlan::new("t", ProjectId::new(), Actor::user()),
                external_ref: None,
            },
        ];
        for cmd in &cmds {
            let k = cmd.kind();
            assert!(
                k.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "not snake_case: {k}"
            );
        }
    }
}
