use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
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
