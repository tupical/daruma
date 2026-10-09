//! Единый бинарник интеграционных тестов крейта: каждый бывший
//! `tests/<name>.rs` — модуль `<name>`. Один бинарник вместо десятков
//! линковок экономит место в target/ и время сборки.

mod ai_analyze_complexity;
mod feature_tiers_doc;
mod get_task_comments;
mod list_project_selection;
mod mutation_response_projection;
mod plan_only_intake;
mod plan_parent_patch;
mod plan_readiness_tools;
mod run_steps;
mod session_artifacts;
mod set_status_comment_wire;
mod toolslist_size;
mod workspacegraph_tools;
