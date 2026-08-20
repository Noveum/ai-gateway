//! The gateway's unified error type ([`AppError`]) and its mapping to HTTP
//! responses via [`axum::response::IntoResponse`].

use aws_sigv4::http_request::SigningError;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use http::header::InvalidHeaderValue;
use http::status::InvalidStatusCode;
use serde_json::json;
use std::{convert::Infallible, io};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Request to provider failed: {0}")]
    ReqwestError(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    IoError(#[from] io::Error),

    #[error("Axum error: {0}")]
    AxumError(#[from] axum::Error),

    #[error("Invalid HTTP method")]
    InvalidMethod,

    #[error("Invalid status code: {0}")]
    InvalidStatus(#[from] InvalidStatusCode),

    #[error("Invalid header value")]
    InvalidHeader,

    #[error("Unsupported provider")]
    UnsupportedProvider,

    #[error("Missing or invalid API key")]
    MissingApiKey,

    #[error("Invalid request format")]
    InvalidRequestFormat,

    #[error("Unsupported model")]
    UnsupportedModel,

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("AWS signing error: {0}")]
    AwsSigningError(#[from] SigningError),

    #[error("AWS params error: {0}")]
    AwsParamsError(String),

    #[error("Invalid header value: {0}")]
    InvalidHeaderValue(#[from] InvalidHeaderValue),

    #[error("Request error: {0}")]
    RequestError(String),

    #[error("Failed to parse event stream: {0}")]
    EventStreamError(String),

    #[error("UTF-8 conversion error: {0}")]
    Utf8Error(#[from] std::string::FromUtf8Error),

    #[error("HTTP error: {0}")]
    HttpError(String),

    #[error("Failed to build HTTP response: {0}")]
    HttpBuildError(#[from] http::Error),

    #[error("JSON parse error: {0}")]
    JsonParseError(String),

    #[error("JSON serialize error: {0}")]
    JsonSerializeError(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match &self {
            AppError::ReqwestError(e) => (
                StatusCode::BAD_GATEWAY,
                format!("Provider request failed: {}", e),
            ),
            AppError::IoError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Internal server error: {}", e),
            ),
            AppError::AxumError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Server error: {}", e),
            ),
            AppError::InvalidMethod => (StatusCode::BAD_REQUEST, "Invalid HTTP method".to_string()),
            AppError::InvalidStatus(_) => (
                StatusCode::BAD_GATEWAY,
                "Invalid status code from provider".to_string(),
            ),
            AppError::InvalidHeader => {
                (StatusCode::BAD_REQUEST, "Invalid header value".to_string())
            }
            AppError::UnsupportedProvider => (
                StatusCode::BAD_REQUEST,
                "Unsupported AI provider".to_string(),
            ),
            AppError::MissingApiKey => (
                StatusCode::UNAUTHORIZED,
                "Missing or invalid API key".to_string(),
            ),
            AppError::InvalidRequestFormat => (
                StatusCode::BAD_REQUEST,
                "Invalid request format".to_string(),
            ),
            AppError::UnsupportedModel => {
                (StatusCode::BAD_REQUEST, "Unsupported model".to_string())
            }
            AppError::JsonError(e) => (
                StatusCode::BAD_REQUEST,
                format!("JSON parsing error: {}", e),
            ),
            AppError::AwsSigningError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("AWS signing error: {}", e),
            ),
            AppError::AwsParamsError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("AWS params build error: {}", e),
            ),
            AppError::InvalidHeaderValue(e) => (
                StatusCode::BAD_REQUEST,
                format!("Invalid header value: {}", e),
            ),
            AppError::RequestError(e) => (StatusCode::BAD_REQUEST, format!("Request error: {}", e)),
            AppError::EventStreamError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to parse event stream: {}", e),
            ),
            AppError::Utf8Error(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("UTF-8 conversion error: {}", e),
            ),
            AppError::HttpError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("HTTP error: {}", e),
            ),
            AppError::HttpBuildError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build HTTP response: {}", e),
            ),
            AppError::JsonParseError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("JSON parse error: {}", e),
            ),
            AppError::JsonSerializeError(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("JSON serialize error: {}", e),
            ),
        };

        let body = Json(json!({
            "error": {
                "message": error_message,
                "type": format!("{:?}", self),
            }
        }));

        (status, body).into_response()
    }
}

impl From<Infallible> for AppError {
    fn from(_: Infallible) -> Self {
        unreachable!("Infallible error cannot occur")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn status_and_body(e: AppError) -> (StatusCode, serde_json::Value) {
        let resp = e.into_response();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn maps_client_errors_to_4xx() {
        assert_eq!(
            status_and_body(AppError::UnsupportedProvider).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_and_body(AppError::MissingApiKey).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status_and_body(AppError::InvalidHeader).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_and_body(AppError::InvalidRequestFormat).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn error_body_has_message_and_type() {
        let (status, body) = status_and_body(AppError::UnsupportedProvider).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["message"], "Unsupported AI provider");
        assert_eq!(body["error"]["type"], "UnsupportedProvider");
    }
}
