# MCP feature tiers

АВТО-СГЕНЕРИРОВАНО из `crates/mcp/src/tools.rs` — не редактировать вручную; regenerate: `UPDATE_GOLDEN=1 cargo test -p daruma-mcp --test feature_tiers_doc`

Рамка: [feature-tiers.md](../../../meisei-research/docs/canon/feature-tiers.md).

Легенда: `Core` = основные; `Enhancing` = усиливающие; `Extending` = расширяющие.

## Сводная матрица

| domain | Core | Enhancing | Extending |
|---|---:|---:|---:|
| Tasks | 8 | 5 | 5 |
| Projects | 3 | 4 | 2 |
| Admin | 0 | 7 | 2 |
| Coordination | 0 | 19 | 1 |
| Ai | 0 | 0 | 1 |
| Events | 0 | 2 | 0 |
| Plans | 7 | 6 | 2 |
| WorkspaceGraph | 0 | 0 | 8 |
| Runs | 0 | 8 | 0 |
| Sessions | 0 | 0 | 7 |
| Signals | 0 | 2 | 0 |
| Relations | 0 | 3 | 0 |
| Documents | 0 | 0 | 9 |
| History | 0 | 0 | 6 |
| ИТОГО | 18 | 56 | 43 |

## Tasks

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_create` | Core / основные | `default` | Create task | write |
| `daruma_capture` | Extending / расширяющие | `default` | Capture inbox task | write |
| `daruma_capture_batch` | Extending / расширяющие | `full` | Capture multiple inbox tasks | write |
| `daruma_get` | Core / основные | `default` | Get task | read |
| `daruma_update` | Core / основные | `default` | Update task | write |
| `daruma_list` | Core / основные | `default` | List tasks | read |
| `daruma_search` | Extending / расширяющие | `default` | Search tasks and comments | read |
| `daruma_lesson_recall` | Extending / расширяющие | `full` | Recall lessons | read |
| `daruma_set_status` | Core / основные | `default` | Set task status | write |
| `daruma_set_priority` | Enhancing / усиливающие | `default` | Set task priority | write |
| `daruma_move_project` | Core / основные | `full` | Move task to another project | write |
| `daruma_complete` | Core / основные | `default` | Complete task | write |
| `daruma_reopen` | Enhancing / усиливающие | `default` | Reopen task | write |
| `daruma_delete` | Core / основные | `full` | Delete task | write,destructive |
| `daruma_split` | Enhancing / усиливающие | `full` | Split task into subtasks | write |
| `daruma_bulk_set_status` | Enhancing / усиливающие | `full` | Bulk set task status | write |
| `daruma_comment` | Enhancing / усиливающие | `default` | Comment on task | write |
| `daruma_can_start` | Extending / расширяющие | `default` | Check task readiness | read |

## Projects

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_project_list` | Core / основные | `default` | List projects | read |
| `daruma_project_create` | Core / основные | `full` | Create project | write |
| `daruma_project_use` | Core / основные | `default` | Bind workspace to project | write |
| `daruma_project_delete` | Enhancing / усиливающие | `full` | Delete project | write,destructive |
| `daruma_workspace_resolve` | Extending / расширяющие | `full` | Resolve/bind workspace for a path | write |
| `daruma_workspace_list` | Extending / расширяющие | `full` | List logical workspaces | read |
| `daruma_project_move_workspace` | Enhancing / усиливающие | `full` | Move project to workspace | write |
| `daruma_project_settings_get` | Enhancing / усиливающие | `full` | Get project settings | read |
| `daruma_project_settings_update` | Enhancing / усиливающие | `full` | Update project settings | write |

## Admin

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_workspace_info` | Extending / расширяющие | `default` | Show workspace info | read |
| `daruma_rule_list` | Enhancing / усиливающие | `full` | List lifecycle rules | read |
| `daruma_rule_get` | Enhancing / усиливающие | `full` | Get a lifecycle rule | read |
| `daruma_rule_create` | Enhancing / усиливающие | `full` | Create a lifecycle rule | write |
| `daruma_rule_update` | Enhancing / усиливающие | `full` | Update a lifecycle rule | write |
| `daruma_rule_disable` | Enhancing / усиливающие | `full` | Disable a lifecycle rule | write,destructive |
| `daruma_evidence_submit` | Enhancing / усиливающие | `full` | Record lifecycle evidence | write |
| `daruma_evidence_list` | Enhancing / усиливающие | `full` | List lifecycle evidence | read |
| `daruma_healthz` | Extending / расширяющие | `default` | Server health check | read |

## Coordination

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_audit_findings` | Enhancing / усиливающие | `full` | List audit findings | read |
| `daruma_audit_finding_ack` | Enhancing / усиливающие | `full` | Acknowledge/mute/resolve a finding | write |
| `daruma_audit_stuck_tasks` | Enhancing / усиливающие | `full` | Tasks stuck in a status | read |
| `daruma_audit_duplicate_tasks` | Enhancing / усиливающие | `full` | Duplicate-task candidates | read |
| `daruma_audit_unread_documents` | Enhancing / усиливающие | `full` | Documents not read recently | read |
| `daruma_inbox_pull` | Enhancing / усиливающие | `full` | Pull agent inbox | write |
| `daruma_claim` | Enhancing / усиливающие | `default` | Claim task | write |
| `daruma_release` | Enhancing / усиливающие | `default` | Release task claim | write |
| `daruma_reserve_files` | Enhancing / усиливающие | `full` | Reserve file paths | write |
| `daruma_release_files` | Enhancing / усиливающие | `full` | Release file leases | write |
| `daruma_active_work` | Enhancing / усиливающие | `full` | List active file leases | read |
| `daruma_ready` | Enhancing / усиливающие | `full` | List project ready pool | read |
| `daruma_ready_drain` | Enhancing / усиливающие | `full` | Claim next ready task (project-wide) | write |
| `daruma_doctor` | Enhancing / усиливающие | `full` | Reconcile stuck parallel work | read |
| `daruma_suggest_files` | Extending / расширяющие | `full` | Suggest paths to reserve | read |
| `daruma_work_unit_create` | Enhancing / усиливающие | `full` | Create work unit | write |
| `daruma_work_unit_list` | Enhancing / усиливающие | `full` | List task work units | read |
| `daruma_work_unit_drain_next` | Enhancing / усиливающие | `full` | Claim next work unit | write |
| `daruma_work_unit_complete` | Enhancing / усиливающие | `full` | Complete work unit | write |
| `daruma_work_unit_release` | Enhancing / усиливающие | `full` | Release work unit claim | write |

## Ai

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_ai_analyze_complexity` | Extending / расширяющие | `full` | AI: analyze plan complexity | write |

## Events

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_subscribe_project` | Enhancing / усиливающие | `full` | Snapshot project events | read |
| `daruma_events_since` | Enhancing / усиливающие | `full` | Load events since seq | read |

## Plans

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_plan_create` | Core / основные | `default` | Create plan | write |
| `daruma_plan_update` | Core / основные | `full` | Update plan | write |
| `daruma_plan_get` | Core / основные | `default` | Get plan | read |
| `daruma_plan_list` | Core / основные | `default` | List plans | read |
| `daruma_plan_add_task` | Core / основные | `default` | Attach task to plan | write |
| `daruma_plan_remove_task` | Core / основные | `full` | Detach task from plan | write |
| `daruma_plan_reorder` | Core / основные | `full` | Reorder plan tasks | write |
| `daruma_plan_archive` | Enhancing / усиливающие | `full` | Archive plan | write,destructive |
| `daruma_plan_set_status` | Enhancing / усиливающие | `default` | Set plan status | write |
| `daruma_plan_next_task` | Enhancing / усиливающие | `full` | Peek next eligible plan task | write |
| `daruma_plan_progress` | Enhancing / усиливающие | `default` | Plan progress snapshot | read |
| `daruma_plan_drain_next` | Enhancing / усиливающие | `default` | Claim next plan task | write |
| `daruma_plan_graph` | Extending / расширяющие | `full` | Read plan DAG | read |
| `daruma_plan_fanout` | Extending / расширяющие | `full` | Plan execution waves | read |
| `daruma_bulk_attach_to_plan` | Enhancing / усиливающие | `full` | Bulk attach tasks to plan | write |

## WorkspaceGraph

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_artifact_register` | Extending / расширяющие | `full` | Register artifact | write |
| `daruma_artifact_list` | Extending / расширяющие | `full` | List artifacts | read |
| `daruma_artifact_impact` | Extending / расширяющие | `full` | Artifact impact analysis | read |
| `daruma_workspacegraph_status` | Extending / расширяющие | `full` | WorkspaceGraph index health | read |
| `daruma_workspacegraph_context` | Extending / расширяющие | `full` | Graph node neighborhood | read |
| `daruma_workspacegraph_related` | Extending / расширяющие | `full` | Graph related nodes | read |
| `daruma_workspacegraph_search` | Extending / расширяющие | `full` | Search WorkspaceGraph nodes | read |
| `daruma_workspacegraph_impact` | Extending / расширяющие | `full` | Graph impact analysis | read |

## Runs

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_run_start` | Enhancing / усиливающие | `default` | Start run | write |
| `daruma_run_start_step` | Enhancing / усиливающие | `full` | Start run step | write |
| `daruma_run_finish_step` | Enhancing / усиливающие | `full` | Finish run step | write |
| `daruma_run_complete` | Enhancing / усиливающие | `default` | Complete run | write |
| `daruma_run_abort` | Enhancing / усиливающие | `default` | Abort run | write |
| `daruma_run_note_append` | Enhancing / усиливающие | `default` | Append run note | write |
| `daruma_run_log` | Enhancing / усиливающие | `full` | Append run log entry | write |
| `daruma_run_notes_list` | Enhancing / усиливающие | `full` | List run notes | read |

## Sessions

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_session_start` | Extending / расширяющие | `full` | Start agent session | write |
| `daruma_session_get` | Extending / расширяющие | `full` | Get agent session | read |
| `daruma_session_list` | Extending / расширяющие | `full` | List agent sessions | read |
| `daruma_session_end` | Extending / расширяющие | `full` | End agent session | write |
| `daruma_session_set_plan` | Extending / расширяющие | `full` | Set session plan steps | write |
| `daruma_session_artifact` | Extending / расширяющие | `full` | Attach session artifact | write |
| `daruma_session_artifacts_list` | Extending / расширяющие | `full` | List session artifacts | read |

## Signals

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_signal_send` | Enhancing / усиливающие | `full` | Send run signal | write |
| `daruma_signal_respond` | Enhancing / усиливающие | `full` | Respond to run signal | write |

## Relations

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_link` | Enhancing / усиливающие | `default` | Link tasks | write |
| `daruma_unlink` | Enhancing / усиливающие | `full` | Delete task relation | write,destructive |
| `daruma_relations` | Enhancing / усиливающие | `default` | Read task relations | read |

## Documents

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_doc_create` | Extending / расширяющие | `full` | Create document | write |
| `daruma_doc_get` | Extending / расширяющие | `full` | Get document | read |
| `daruma_doc_append` | Extending / расширяющие | `full` | Append to document | write |
| `daruma_doc_replace` | Extending / расширяющие | `full` | Replace document body | write |
| `daruma_doc_rename` | Extending / расширяющие | `full` | Rename document | write |
| `daruma_doc_archive` | Extending / расширяющие | `full` | Archive document | write,destructive |
| `daruma_doc_set_status` | Extending / расширяющие | `full` | Set document status | write |
| `daruma_doc_link_task` | Extending / расширяющие | `full` | Link document to task | write |
| `daruma_doc_list` | Extending / расширяющие | `full` | List documents | read |

## History

| tool | tier | profile | title | hints |
|---|---|---|---|---|
| `daruma_history_list` | Extending / расширяющие | `full` | List version history | read |
| `daruma_history_get` | Extending / расширяющие | `full` | Get version record | read |
| `daruma_history_compare` | Extending / расширяющие | `full` | Compare versions | read |
| `daruma_history_latest` | Extending / расширяющие | `full` | List latest versions | read |
| `daruma_history_summary` | Extending / расширяющие | `full` | Version summary timeline | read |
| `daruma_history_rollback` | Extending / расширяющие | `full` | Rollback to version | write,destructive |

