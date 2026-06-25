//! Axum HTTP handlers: the health check and the catch-all proxy entry point that
//! resolves the target provider from the `x-provider` header and forwards the
//! request through [`crate::proxy`].

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
use tracing::{debug, error, info, Instrument};
use uuid;

pub async fn health_check() -> impl IntoResponse {
    info!("Health check endpoint called");
    Json(json!({ "status": "healthy", "version": env!("CARGO_PKG_VERSION") }))
}

pub async fn proxy_request(
    State(config): State<Arc<AppConfig>>,
    headers: HeaderMap,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let provider = headers
        .get("x-provider")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("openai");

    let client_addr = connect_info
        .map(|ConnectInfo(addr)| addr.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let path = request.uri().path();
    let method = request.method().as_str();

    // Extract potentially useful headers for logging
    let organization = headers
        .get("x-organisation-id")
        .or_else(|| headers.get("x-organization-id")) // Try both British and American spelling
        .and_then(|h| h.to_str().ok())
        .unwrap_or("none");

    let project = headers
        .get("x-project-id")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("none");

    let user = headers
        .get("x-user-id")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("none");

    // Log request at info level for operational visibility
    info!(
        provider = %provider,
        path = %path,
        method = %method,
        client = %client_addr,
        org_id = %organization,
        project_id = %project,
        user_id = %user,
        "Received API request"
    );

    debug!(
        "Request details: provider={}, client={}, path={}, method={}, org={}, project={}, user={}",
        provider, client_addr, path, method, organization, project, user
    );

    // Clone values needed for logging inside the async block
    let provider_clone = provider.to_string();
    let path_clone = path.to_string();
    let method_clone = method.to_string();

    let span = tracing::info_span!(
        "proxy_request",
        provider = provider,
        method = %method,
        path = %path,
        client = %client_addr,
        org_id = %organization,
        project_id = %project,
        user_id = %user
    );

    async move {
        let start_time = std::time::Instant::now();
        let result = proxy_request_to_provider(config, provider, request).await;
        let elapsed = start_time.elapsed();

        match result {
            Ok(response) => {
                let status = response.status().as_u16();

                // Extract x-request-id header from response for tracking
                let request_id = response
                    .headers()
                    .get("x-request-id")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("none");

                // Also try to get provider-specific request ID
                let provider_request_id = match provider_clone.as_str() {
                    "openai" => response
                        .headers()
                        .get("x-request-id")
                        .or_else(|| response.headers().get("openai-request-id"))
                        .and_then(|h| h.to_str().ok()),
                    "anthropic" => response
                        .headers()
                        .get("anthropic-request-id")
                        .and_then(|h| h.to_str().ok()),
                    "groq" => response
                        .headers()
                        .get("groq-request-id")
                        .and_then(|h| h.to_str().ok()),
                    _ => None,
                };

                // Use provider request ID if available, otherwise use our internal ID
                let tracking_id = provider_request_id.unwrap_or(request_id);

                info!(
                    provider = %provider_clone,
                    path = %path_clone,
                    method = %method_clone,
                    status = status,
                    latency_ms = %elapsed.as_millis(),
                    request_id = %tracking_id,
                    "Request completed successfully"
                );
                response
            }
            Err(e) => {
                // For errors, generate a unique ID to help with debugging
                let error_id = format!("err-{}", &uuid::Uuid::new_v4().to_string()[..8]);

                error!(
                    provider = %provider_clone,
                    path = %path_clone,
                    method = %method_clone,
                    error = %e,
                    latency_ms = %elapsed.as_millis(),
                    request_id = %error_id,
                    "Request failed"
                );
                e.into_response()
            }
        }
    }
    .instrument(span)
    .await
}
