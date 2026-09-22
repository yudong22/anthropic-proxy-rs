use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Application-specific errors
#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Request transformation error: {0}")]
    Transform(String),

    #[error("Upstream API error: {0}")]
    Upstream(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            ProxyError::Config(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ProxyError::Transform(msg) => (StatusCode::BAD_REQUEST, msg),
            ProxyError::Upstream(msg) => (StatusCode::BAD_GATEWAY, msg),
            ProxyError::Serialization(err) => {
                (StatusCode::BAD_REQUEST, format!("JSON error: {}", err))
            }
            ProxyError::Http(err) => (StatusCode::BAD_GATEWAY, format!("HTTP error: {}", err)),
        };

        let body = Json(json!({
            "error": {
                "type": "proxy_error",
                "message": error_message,
            }
        }));

        (status, body).into_response()
    }
}

/// Result type for proxy operations
pub type ProxyResult<T> = Result<T, ProxyError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    fn status_of(error: ProxyError) -> StatusCode {
        error.into_response().status()
    }

    #[test]
    fn config_error_returns_500() {
        assert_eq!(
            status_of(ProxyError::Config("bad".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn transform_error_returns_400() {
        assert_eq!(
            status_of(ProxyError::Transform("bad".into())),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn upstream_error_returns_502() {
        assert_eq!(
            status_of(ProxyError::Upstream("bad".into())),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn serialization_error_returns_400() {
        let err: serde_json::Error = serde_json::from_str::<String>("not json").unwrap_err();
        assert_eq!(
            status_of(ProxyError::Serialization(err)),
            StatusCode::BAD_REQUEST
        );
    }
}
