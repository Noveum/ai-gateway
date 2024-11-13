use super::Provider;
use crate::error::AppError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tracing::{debug, error};

pub struct OpenAIProvider {
    base_url: String,
}

impl OpenAIProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.openai.com".to_string(),
        }
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing OpenAI request headers");
        let mut headers = HeaderMap::new();

        // Add content type
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Process authentication
        if let Some(api_key) = original_headers
            .get("x-magicapi-api-key")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Using x-magicapi-api-key for authentication");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(&format!("Bearer {}", api_key)).map_err(
                    |_| {
                        error!("Failed to create authorization header from x-magicapi-api-key");
                        AppError::InvalidHeader
                    },
                )?,
            );
        } else if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Using provided authorization header");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for OpenAI request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}
