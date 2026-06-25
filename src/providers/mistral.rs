//! Mistral provider — OpenAI-compatible chat completions.
//!
//! Mistral's API mirrors the OpenAI `/v1/chat/completions` shape, so request and
//! response handling is a straight pass-through with Bearer auth. Cost is
//! computed from the shared pricing table.

use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use tracing::{debug, error};

pub struct MistralProvider {
    base_url: String,
}

impl MistralProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.mistral.ai".to_string(),
        }
    }
}

impl Default for MistralProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for MistralProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "mistral"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Mistral request headers");
        let mut headers = HeaderMap::new();
        log_tracking_headers(original_headers);

        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process Mistral authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for Mistral request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_and_name() {
        let p = MistralProvider::new();
        assert_eq!(p.base_url(), "https://api.mistral.ai");
        assert_eq!(p.name(), "mistral");
    }

    #[test]
    fn requires_authorization() {
        let p = MistralProvider::new();
        let empty = HeaderMap::new();
        assert!(matches!(p.process_headers(&empty), Err(AppError::MissingApiKey)));
    }

    #[test]
    fn forwards_authorization() {
        let p = MistralProvider::new();
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer mk-123".parse().unwrap());
        let out = p.process_headers(&h).unwrap();
        assert_eq!(out.get(http::header::AUTHORIZATION).unwrap(), "Bearer mk-123");
        assert_eq!(out.get(http::header::CONTENT_TYPE).unwrap(), "application/json");
    }
}
