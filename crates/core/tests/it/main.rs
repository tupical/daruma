//! Единый бинарник интеграционных тестов крейта: каждый бывший
//! `tests/<name>.rs` — модуль `<name>`. Один бинарник вместо десятков
//! линковок экономит место в target/ и время сборки.

mod audit_settings;
mod bulk_ops;
mod delete_task_plan_cascade;
mod document_commands;
mod handoff_commands;
mod lifecycle_gate;
mod pipeline_fan_in;
mod plan_cycle;
mod plan_reconciliation;
mod plan_reparent;
mod plan_source;
mod relations_cycle;
mod relations_enforcement;
mod relations_link;
mod relations_was_blocking;
mod role_artifacts;
mod rule_engine;
mod run_liveness;
mod run_notes;
mod run_postconditions;
mod run_retry;
mod semantic_events;
mod set_status_comment;
mod source_chain;
mod task_git_context;
mod task_pipeline_position;
