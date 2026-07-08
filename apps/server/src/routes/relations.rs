//! HTTP handlers for task relation endpoints (§3.2 W3.1).
//!
//! ## URL layout
//!
//! | Method | Path                       | Capability          | Description           |
//! |--------|----------------------------|---------------------|-----------------------|
//! | POST   | /v1/relations              | TaskRelationWrite   | Create a relation     |
//! | POST   | /v1/relations/query        | TaskRelationRead    | Bulk-read relations   |
//! | DELETE | /v1/relations/{id}         | TaskRelationWrite   | Remove a relation     |
//! | GET    | /v1/tasks/{id}/relations   | TaskRelationRead    | 5-group projection    |

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use daruma_auth::{AuthContext, Capability};
use daruma_core::Command;
use daruma_domain::{Relation, RelationKind, TaskRelations};
use daruma_events::Event;
use daruma_shared::{CoreError, RelationId, TaskId};
use serde::Deserialize;
use serde_json::json;

use crate::{error::ApiError, routes::MutationResponse, state::AppState};

// ── Request bodies ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LinkRelationBody {
    pub from: TaskId,
    pub to: TaskId,
    pub kind: RelationKind,
    #[serde(default)]
    pub client_command_id: Option<uuid::Uuid>,
}

/// Query for `GET /v1/relations?task_ids=<csv>`.
#[derive(Deserialize)]
pub struct ListRelationsQuery {
    /// Comma-separated list of task ids.  Empty or missing → empty result.
    #[serde(default)]
    pub task_ids: Option<String>,
}

/// Body for `POST /v1/relations/query`.
#[derive(Deserialize)]
pub struct ListRelationsBody {
    /// Task ids whose incoming or outgoing relations should be returned.
    #[serde(default)]
    pub task_ids: Vec<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `POST /v1/relations` — create a typed relation between two tasks.
///
/// Returns 201 + [`MutationResponse`] with `data.relation_id`.
/// Supports idempotency via `client_command_id`.
pub async fn link(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<LinkRelationBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRelationWrite)
        .map_err(ApiError::from_missing_cap)?;

    // Idempotency check — return cached result if seen before.
    if let Some(ccid) = body.client_command_id {
        if let Some((eid, eseq)) = state
            .idempotency
            .lookup(ccid)
            .await
            .map_err(ApiError::from)?
        {
            let relation_id = load_cached_relation_id(&state, eid, eseq).await?;
            return Ok((
                StatusCode::CREATED,
                Json(MutationResponse {
                    success: true,
                    event_id: Some(eid),
                    event_seq: Some(eseq),
                    data: json!({ "relation_id": relation_id }),
                    warnings: vec![],
                    client_command_id: Some(ccid),
                }),
            ));
        }
    }

    let envs = state
        .commands
        .dispatch(
            Command::LinkTasks {
                from: body.from,
                to: body.to,
                kind: body.kind,
            },
            super::actor_from(&auth, None),
        )
        .await
        .map_err(ApiError::from)?;

    // Extract relation_id from the emitted TaskLinked event.
    let relation_id = envs
        .iter()
        .find_map(|e| {
            if let Event::TaskLinked { relation_id, .. } = &e.payload {
                Some(*relation_id)
            } else {
                None
            }
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("expected TaskLinked event")))?;

    // Persist idempotency record.
    if let Some(ccid) = body.client_command_id {
        if let Some(last) = envs.last() {
            state
                .idempotency
                .insert(ccid, last.id, last.seq)
                .await
                .map_err(ApiError::from)?;
        }
    }

    let last = envs.last();
    Ok((
        StatusCode::CREATED,
        Json(MutationResponse {
            success: true,
            event_id: last.map(|e| e.id),
            event_seq: last.map(|e| e.seq),
            data: json!({ "relation_id": relation_id }),
            warnings: vec![],
            client_command_id: body.client_command_id,
        }),
    ))
}

async fn load_cached_relation_id(
    state: &AppState,
    event_id: daruma_shared::EventId,
    event_seq: u64,
) -> Result<RelationId, ApiError> {
    let events = state
        .store
        .load_since(event_seq.saturating_sub(1), 1)
        .await
        .map_err(ApiError::from)?;
    events
        .into_iter()
        .find(|event| event.id == event_id)
        .and_then(|event| match event.payload {
            Event::TaskLinked { relation_id, .. } => Some(relation_id),
            _ => None,
        })
        .ok_or_else(|| ApiError::from(CoreError::storage("expected cached TaskLinked event")))
}

/// `DELETE /v1/relations/{id}` — remove a typed relation by id.
///
/// Returns 200 + [`MutationResponse`] with `data.relation_id`.
pub async fn unlink(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRelationWrite)
        .map_err(ApiError::from_missing_cap)?;

    let id = id_str.parse::<RelationId>().map_err(|_| {
        ApiError::from(CoreError::validation(format!(
            "invalid relation id: {id_str}"
        )))
    })?;

    let envs = state
        .commands
        .dispatch(Command::UnlinkTasks { id }, super::actor_from(&auth, None))
        .await
        .map_err(ApiError::from)?;

    let last = envs.last();
    Ok(Json(MutationResponse {
        success: true,
        event_id: last.map(|e| e.id),
        event_seq: last.map(|e| e.seq),
        data: json!({ "relation_id": id }),
        warnings: vec![],
        client_command_id: None,
    }))
}

/// `GET /v1/tasks/{id}/relations` — five-group relation projection for a task.
///
/// Groups: `blocks`, `blocked_by`, `relates_to`, `duplicates`, `duplicated_by`.
pub async fn list_for_task(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id_str): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRelationRead)
        .map_err(ApiError::from_missing_cap)?;

    let task_id = id_str
        .parse::<TaskId>()
        .map_err(|_| ApiError::from(CoreError::validation(format!("invalid task id: {id_str}"))))?;

    let all: Vec<Relation> = state
        .relations
        .list_by_task(task_id)
        .await
        .map_err(ApiError::from)?;

    // Distribute into 5 groups relative to task_id.
    let mut blocks = Vec::new();
    let mut blocked_by = Vec::new();
    let mut relates_to = Vec::new();
    let mut duplicates = Vec::new();
    let mut duplicated_by = Vec::new();

    for rel in all {
        match rel.kind {
            RelationKind::Blocks => {
                if rel.from == task_id {
                    blocks.push(rel);
                } else {
                    blocked_by.push(rel);
                }
            }
            RelationKind::RelatesTo => {
                relates_to.push(rel);
            }
            RelationKind::Duplicates => {
                if rel.from == task_id {
                    duplicates.push(rel);
                } else {
                    duplicated_by.push(rel);
                }
            }
            // §3.7.2: `WasBlocking` is a historical/audit-only relation kind and
            // is intentionally absent from the five active projection groups.
            RelationKind::WasBlocking => {}
        }
    }

    Ok(Json(TaskRelations {
        blocks,
        blocked_by,
        relates_to,
        duplicates,
        duplicated_by,
    }))
}

/// `GET /v1/relations?task_ids=a,b,c` — flat list of relations whose either
/// endpoint matches any of the given task ids.
///
/// Used by the web UI to render blocker/blocked-by indicators in collapsed
/// task rows without N+1 round-trips.
pub async fn list_for_tasks(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ListRelationsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRelationRead)
        .map_err(ApiError::from_missing_cap)?;

    let raw = q.task_ids.unwrap_or_default();
    let ids = parse_task_ids(raw.split(','))?;

    let rels = state
        .relations
        .list_by_task_ids(&ids)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(rels))
}

/// `POST /v1/relations/query` — flat list of relations whose either endpoint
/// matches any of the given task ids.
///
/// This is the preferred bulk-read endpoint for clients with large task lists:
/// it keeps the id set in a JSON body instead of expanding the URL query string.
pub async fn query_for_tasks(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Json(body): Json<ListRelationsBody>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRelationRead)
        .map_err(ApiError::from_missing_cap)?;

    let ids = parse_task_ids(body.task_ids.iter().map(String::as_str))?;

    let rels = state
        .relations
        .list_by_task_ids(&ids)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(rels))
}

fn parse_task_ids<'a>(raw_ids: impl IntoIterator<Item = &'a str>) -> Result<Vec<TaskId>, ApiError> {
    let mut ids: Vec<TaskId> = Vec::new();
    for raw in raw_ids {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let id = s
            .parse::<TaskId>()
            .map_err(|_| ApiError::from(CoreError::validation(format!("invalid task id: {s}"))))?;
        ids.push(id);
    }
    Ok(ids)
}
