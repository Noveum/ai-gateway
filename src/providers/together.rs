use super::Provider;
use crate::error::AppError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tracing::{debug, error};

pub struct TogetherProvider {
    base_url: String,
}

impl TogetherProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.together.xyz".to_string(),
        }
    }
}

#[async_trait]
impl Provider for TogetherProvider {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn name(&self) -> &str {
        "together"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Together request headers");
        let mut headers = HeaderMap::new();

        // Add content type
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Process authentication
        if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Using provided authorization header for Together");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process Together authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for Together request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}
