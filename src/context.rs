//! Per-request context: the parsed model, request body, and headers carried
//! alongside a proxied request.

use axum::http::HeaderMap;
use serde_json::Value;

/// Request-scoped context assembled from the inbound request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Target model id parsed from the request body.
    pub model: String,
    /// The parsed JSON request body.
    pub request_body: Value,
    /// The inbound request headers (provider selection, auth, tracking).
    pub headers: HeaderMap,
}

impl RequestContext {
    pub fn new(model: String, request_body: Value, headers: HeaderMap) -> Self {
        Self {
            model,
            request_body,
            headers,
        }
    }
}
