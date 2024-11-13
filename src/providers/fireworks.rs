use super::Provider;
use crate::error::AppError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tracing::{debug, error};

pub struct FireworksProvider {
    base_url: String,
}

impl FireworksProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.fireworks.ai/inference/v1".to_string(),
        }
    }
}

#[async_trait]
impl Provider for FireworksProvider {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn name(&self) -> &str {
        "fireworks"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Fireworks request headers");
        let mut headers = HeaderMap::new();

        // Add content type
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Add accept header
        headers.insert(
            http::header::ACCEPT,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Process authentication
        if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Using provided authorization header for Fireworks");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process Fireworks authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for Fireworks request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }

    fn transform_path(&self, path: &str) -> String {
        // The incoming path is /v1/chat/completions
        // We want to strip the /v1 prefix since it's already in the base_url
        if path.starts_with("/v1/") {
            path.trim_start_matches("/v1").to_string()
        } else {
            path.to_string()
        }
    }
}
