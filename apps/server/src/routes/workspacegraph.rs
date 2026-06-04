//! HTTP handlers for WorkspaceGraph read endpoints (P3).

use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use taskagent_auth::{AuthContext, Capability};
use taskagent_shared::{CoreError, ProjectId};

use crate::{error::ApiError, state::AppState};

#[derive(Deserialize, Default)]
pub struct NodeIdQuery {
    pub node_id: Option<String>,
    pub kind: Option<String>,
    pub source_id: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct ContextQuery {
    #[serde(flatten)]
    pub node: NodeIdQuery,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Default)]
pub struct RelatedQuery {
    #[serde(flatten)]
    pub node: NodeIdQuery,
    pub depth: Option<u32>,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Default)]
pub struct ImpactQuery {
    #[serde(flatten)]
    pub node: NodeIdQuery,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Default)]
pub struct SearchQuery {
    pub query: String,
    pub limit: Option<u32>,
    pub project_id: Option<String>,
}

fn kind_prefix(kind: &str) -> Option<&'static str> {
    match kind.to_ascii_lowercase().as_str() {
        "task" => Some("tsk"),
        "project" => Some("prj"),
        "plan" => Some("pln"),
        "document" => Some("doc"),
        "comment" => Some("cmt"),
        _ => None,
    }
}

fn normalize_source_id(kind: &str, source_id: &str) -> String {
    let Some(prefix) = kind_prefix(kind) else {
        return source_id.to_string();
    };
    let marker = format!("{prefix}_");
    if source_id.starts_with(&marker) {
        source_id.to_string()
    } else {
        format!("{marker}{source_id}")
    }
}

fn normalize_graph_node_id(node_id: &str) -> String {
    let Some((kind, source_id)) = node_id.split_once(':') else {
        return node_id.to_string();
    };
    if source_id.contains('_') {
        return node_id.to_string();
    }
    format!("{kind}:{}", normalize_source_id(kind, source_id))
}

fn resolve_node_id(q: &NodeIdQuery) -> Result<String, ApiError> {
    if let Some(id) = q.node_id.as_deref().filter(|s| !s.is_empty()) {
        return Ok(normalize_graph_node_id(id));
    }
    match (
        q.kind.as_deref().filter(|s| !s.is_empty()),
        q.source_id.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(kind), Some(source_id)) => Ok(format!(
            "{}:{}",
            kind.to_ascii_lowercase(),
            normalize_source_id(kind, source_id)
        )),
        _ => Err(ApiError::from(CoreError::validation(
            "node_id or both kind and source_id are required",
        ))),
    }
}

fn parse_project_id(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    match raw {
        None | Some("") | Some("all") => Ok(None),
        Some(pid) => pid
            .parse::<ProjectId>()
            .map(|id| Some(id.to_string()))
            .map_err(|_| {
                ApiError::from(CoreError::validation(format!("invalid project id: {pid}")))
            }),
    }
}

fn default_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(20).clamp(1, 200)
}

pub async fn status(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let status = state
        .workspace_graph
        .status()
        .await
        .map_err(ApiError::from)?;
    Ok(Json(status))
}

pub async fn context(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ContextQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let node_id = resolve_node_id(&q.node)?;
    let limit = default_limit(q.limit);
    let items = state
        .workspace_graph
        .context(&node_id, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(items))
}

pub async fn related(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<RelatedQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let node_id = resolve_node_id(&q.node)?;
    let depth = q.depth.unwrap_or(1).clamp(1, 5);
    let limit = default_limit(q.limit);
    let neighborhood = state
        .workspace_graph
        .related(&node_id, depth, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(neighborhood))
}

pub async fn search(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let query = q.query.trim();
    if query.is_empty() {
        return Err(ApiError::from(CoreError::validation(
            "query must not be empty",
        )));
    }
    let limit = default_limit(q.limit);
    let project_id = parse_project_id(q.project_id.as_deref())?;
    let hits = state
        .workspace_graph
        .search(query, limit, project_id.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(hits))
}

pub async fn impact(
    auth: axum::Extension<AuthContext>,
    State(state): State<AppState>,
    Query(q): Query<ImpactQuery>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;
    let node_id = resolve_node_id(&q.node)?;
    let limit = default_limit(q.limit);
    let neighborhood = state
        .workspace_graph
        .impact(&node_id, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(neighborhood))
}
