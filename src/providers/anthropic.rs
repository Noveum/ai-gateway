use super::Provider;
use crate::error::AppError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tracing::{debug, error};

pub struct AnthropicProvider {
    base_url: String,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn transform_path(&self, path: &str) -> String {
        if path.contains("/chat/completions") {
            "/v1/messages".to_string()
        } else {
            path.to_string()
        }
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Anthropic request headers");
        let mut headers = HeaderMap::new();

        // Add content type
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Add Anthropic version header
        headers.insert(
            http::header::HeaderName::from_static("anthropic-version"),
            http::header::HeaderValue::from_static("2023-06-01"),
        );

        // Process authentication
        if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Converting Bearer token to x-api-key format");
            let api_key = auth.trim_start_matches("Bearer ");
            headers.insert(
                http::header::HeaderName::from_static("x-api-key"),
                http::header::HeaderValue::from_str(api_key).map_err(|_| {
                    error!("Failed to process Anthropic authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for Anthropic request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}
