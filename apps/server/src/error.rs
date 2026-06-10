//! Axum-compatible error wrapper for [`taskagent_shared::CoreError`].

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use taskagent_auth::MissingCapability;
use taskagent_shared::CoreError;

/// HTTP error response produced by route handlers.
pub struct ApiError(pub CoreError);

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        Self(e)
    }
}

impl ApiError {
    /// Convert a capability-check failure into a 403 ApiError.
    pub fn from_missing_cap(m: MissingCapability) -> Self {
        Self(CoreError::forbidden(m.to_string()))
    }

    /// Build an `ApiError` that maps to a specific HTTP status code.
    ///
    /// Useful when the domain error hierarchy does not have a matching variant
    /// (e.g., a custom 403 with a fingerprint-mismatch message).
    pub fn status(code: StatusCode, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        let core = match code {
            StatusCode::NOT_FOUND => CoreError::not_found(msg),
            StatusCode::CONFLICT => CoreError::conflict(msg),
            StatusCode::UNAUTHORIZED => CoreError::unauthorized(msg),
            StatusCode::FORBIDDEN => CoreError::forbidden(msg),
            StatusCode::BAD_REQUEST => CoreError::validation(msg),
            _ => CoreError::storage(msg),
        };
        Self(core)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            CoreError::NotFound(_) => StatusCode::NOT_FOUND,
            CoreError::Validation(_) => StatusCode::BAD_REQUEST,
            CoreError::Conflict(_) => StatusCode::CONFLICT,
            CoreError::Ai(_) => StatusCode::BAD_GATEWAY,
            CoreError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            CoreError::Forbidden(_) => StatusCode::FORBIDDEN,
            CoreError::QuotaExceeded { .. } => StatusCode::PAYMENT_REQUIRED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        // "field" is an extension point for validation errors targeting a specific field.
        // "request_id" remains None until a future patch threads `RequestId` from request
        // extensions into the error constructor.
        let body = match &self.0 {
            CoreError::QuotaExceeded {
                resource,
                limit,
                current,
            } => json!({
                "error": {
                    "code": "quota_exceeded",
                    "message": self.0.to_string(),
                    "resource": resource,
                    "limit": limit,
                    "current": current,
                }
            }),
            _ => json!({
                "error": {
                    "code": self.0.code(),
                    "message": self.0.to_string(),
                }
            }),
        };

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn error_response_has_structured_shape() {
        let err = ApiError(CoreError::not_found("task xyz"));
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(json["error"]["code"], "not_found");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("task xyz"),
            "message should contain the original detail"
        );
        // field and request_id are absent until populated by later waves
        assert!(json["error"].get("field").is_none());
        assert!(json["error"].get("request_id").is_none());
    }

    #[tokio::test]
    async fn validation_error_returns_400() {
        let err = ApiError(CoreError::validation("title is required"));
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(json["error"]["code"], "validation");
    }

    #[tokio::test]
    async fn forbidden_error_returns_403() {
        let err = ApiError(CoreError::forbidden("missing capability: task:write"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["code"], "forbidden");
    }

    #[tokio::test]
    async fn unauthorized_error_returns_401() {
        let err = ApiError(CoreError::unauthorized("missing bearer"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["code"], "unauthorized");
    }
}
