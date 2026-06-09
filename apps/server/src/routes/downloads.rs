//! Authenticated download of the unified `taskagent` binary (serves the CLI,
//! launcher, and `taskagent mcp` stdio server in one artifact).

use std::path::Path;

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use taskagent_auth::{AuthContext, Capability};
use taskagent_shared::CoreError;

use crate::{error::ApiError, state::AppState};

/// `GET /v1/downloads/taskagent/{platform}` — `linux` or `windows`.
pub async fn download_taskagent_mcp(
    auth: Extension<AuthContext>,
    State(state): State<AppState>,
    AxumPath(platform): AxumPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;

    let path = state
        .mcp_downloads
        .path_for(platform.as_str())
        .ok_or_else(|| {
            ApiError(CoreError::not_found(
                "taskagent binary is not available for this platform",
            ))
        })?;

    serve_binary(path, filename_for(&platform)).await
}

async fn serve_binary(path: &Path, filename: &str) -> Result<impl IntoResponse, ApiError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| ApiError(CoreError::io(format!("read mcp binary: {e}"))))?;

    let disposition = format!("attachment; filename=\"{filename}\"");
    let content_type = header::HeaderValue::from_static("application/octet-stream");
    let content_disposition = header::HeaderValue::from_str(&disposition)
        .map_err(|e| ApiError(CoreError::validation(e.to_string())))?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, content_disposition),
        ],
        Body::from(bytes),
    ))
}

fn filename_for(platform: &str) -> &'static str {
    match platform {
        "windows" => "taskagent.exe",
        _ => "taskagent",
    }
}

/// `GET /v1/downloads/taskagent` — which platforms are bundled.
pub async fn mcp_download_info(
    auth: Extension<AuthContext>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Capability::TaskRead)
        .map_err(ApiError::from_missing_cap)?;

    Ok(Json(serde_json::json!({
        "platforms": {
            "linux": state.mcp_downloads.linux.is_some(),
            "windows": state.mcp_downloads.windows.is_some(),
        }
    })))
}
