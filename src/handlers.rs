use crate::{config::AppConfig, proxy::proxy_request_to_provider};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request},
    response::IntoResponse,
    Json,
};
use serde_json::json;
use std::sync::Arc;
use tracing::{error, info};

pub async fn health_check() -> impl IntoResponse {
    Json(json!({
        "status": "healthy",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

pub async fn proxy_request(
    State(config): State<Arc<AppConfig>>,
    headers: HeaderMap,
    request: Request<Body>,
) -> impl IntoResponse {
    let provider = headers
        .get("x-provider")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("openai");

    info!(
        provider = provider,
        method = %request.method(),
        path = %request.uri().path(),
        "Incoming proxy request"
    );

    match proxy_request_to_provider(config, provider, request).await {
        Ok(response) => response,
        Err(e) => {
            error!(
                error = %e,
                provider = provider,
                "Proxy request failed"
            );
            e.into_response()
        }
    }
}
