use axum::http::HeaderMap;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub model: String,
    pub request_body: Value,
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
