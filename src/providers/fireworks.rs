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
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "fireworks"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Fireworks request headers");
        let mut headers = HeaderMap::new();

        // Add standard headers
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        headers.insert(
            http::header::ACCEPT,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Process authentication
        if let Some(auth) = original_headers
            .get(http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
        {
            // Validate token is not empty
            if auth.trim().is_empty() {
                error!("Empty authorization token provided for Fireworks");
                return Err(AppError::InvalidHeader);
            }

            // Validate token format
            if !auth.starts_with("Bearer ") {
                error!("Invalid authorization format for Fireworks - must start with 'Bearer'");
                return Err(AppError::InvalidHeader);
            }

            // Validate token is not just "Bearer "
            if auth.len() <= 7 {
                error!("Empty Bearer token in Fireworks authorization header");
                return Err(AppError::InvalidHeader);
            }

            debug!("Using provided authorization header for Fireworks");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Invalid characters in Fireworks authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("Missing 'Authorization' header for Fireworks API request");
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
