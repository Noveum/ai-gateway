use crate::{config::AppConfig, proxy::proxy_request_to_provider};
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, Request},
    response::IntoResponse,
    Json,
};
use serde_json::json;
use std::{net::SocketAddr, sync::Arc};
use tracing::{debug, error, Instrument};

pub async fn health_check() -> impl IntoResponse {
    debug!("Health check endpoint called");
    Json(json!({ "status": "healthy", "version": env!("CARGO_PKG_VERSION") }))
}

pub async fn proxy_request(
    State(config): State<Arc<AppConfig>>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> impl IntoResponse {
    let provider = headers
        .get("x-provider")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("openai");

    debug!(
        "Received request for provider: {}, client: {}, path: {}",
        provider,
        addr,
        request.uri().path()
    );

    let span = tracing::info_span!(
        "proxy_request",
        provider = provider,
        method = %request.method(),
        path = %request.uri().path(),
        client = %addr
    );

    async move {
        match proxy_request_to_provider(config, provider, request).await {
            Ok(response) => response,
            Err(e) => {
                error!(error = %e, "Proxy request failed");
                e.into_response()
            }
        }
    }
    .instrument(span)
    .await
}
