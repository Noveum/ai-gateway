use crate::providers::Provider;
use axum::body::to_bytes;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Request, Response, StatusCode},
};
use futures_util::StreamExt;
use reqwest::Method;
use std::sync::Arc;
use tracing::{debug, error};

use crate::{config::AppConfig, error::AppError, providers::create_provider};

mod client;
pub use client::CLIENT;
mod signing;

pub async fn proxy_request_to_provider(
    config: Arc<AppConfig>,
    provider_name: &str,
    mut original_request: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let provider = create_provider(provider_name)?;

    // Extract body bytes
    let body = std::mem::replace(original_request.body_mut(), Body::empty());
    let body_bytes = to_bytes(body, usize::MAX)
        .await
        .map_err(|e| AppError::AxumError(e.into()))?;

    // Call before_request first to set up any provider state
    provider
        .before_request(original_request.headers(), &body_bytes)
        .await?;

    // Process headers and transform path
    let headers = provider.process_headers(original_request.headers())?;
    let path = original_request.uri().path();
    let modified_path = provider.transform_path(path);

    // Prepare request body
    let prepared_body = provider.prepare_request_body(body_bytes).await?;

    // Construct final URL
    let query = original_request
        .uri()
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    let url = format!("{}{}{}", provider.base_url(), modified_path, query);
    debug!("Using URL: {}", url);

    // Handle AWS signing if required
    let final_headers = if provider.requires_signing() {
        if let Some((access_key, secret_key, region)) = provider.get_signing_credentials(&headers) {
            signing::sign_aws_request(
                original_request.method().as_str(),
                &url,
                &prepared_body,
                &access_key,
                &secret_key,
                &region,
                "bedrock",
            )
            .await?
        } else {
            headers
        }
    } else {
        headers
    };

    debug!(
        "Final headers in proxy_request_to_provider: {:?}",
        final_headers
    );

    // Send the request with signed headers
    let response = send_provider_request(
        original_request.method().clone(),
        url,
        final_headers,
        prepared_body,
        &provider,
        config,
    )
    .await?;

    provider.process_response(response).await
}

pub async fn send_provider_request(
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Bytes,
    provider: &Box<dyn Provider>,
    config: Arc<AppConfig>,
) -> Result<Response<Body>, AppError> {
    let client = &*CLIENT;

    let reqwest_headers = headers
        .iter()
        .filter_map(|(name, value)| {
            name.as_str()
                .parse::<reqwest::header::HeaderName>()
                .ok()
                .and_then(|name_str| {
                    reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                        .ok()
                        .map(|v| (name_str, v))
                })
        })
        .collect::<reqwest::header::HeaderMap>();

    debug!(
        "Final headers in send_provider_request: {:?}",
        reqwest_headers
    );

    let response = client
        .request(method, url)
        .headers(reqwest_headers)
        .body(body)
        .send()
        .await?;

    process_response(response, config).await
}

async fn process_response(
    response: reqwest::Response,
    config: Arc<AppConfig>,
) -> Result<Response<Body>, AppError> {
    let status = StatusCode::from_u16(response.status().as_u16())?;
    let mut response_builder = Response::builder().status(status);

    // Efficiently copy headers
    for (name, value) in response.headers() {
        if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
            response_builder = response_builder.header(name.clone(), v);
        }
    }

    // Fast path for non-streaming responses
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map_or(false, |ct| {
            ct.contains("application/vnd.amazon.eventstream") || ct.contains("text/event-stream")
        })
    {
        let body = response.bytes().await?;
        return Ok(response_builder.body(Body::from(body)).unwrap());
    }

    // Optimized streaming response handling
    debug!("Processing streaming response");

    let stream = response.bytes_stream().map(|result| match result {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            error!("Stream error: {}", e);
            Err(std::io::Error::new(std::io::ErrorKind::Other, e))
        }
    });

    // Add streaming headers once
    response_builder = response_builder
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("transfer-encoding", "chunked")
        .header("x-accel-buffering", "no");

    Ok(response_builder.body(Body::from_stream(stream)).unwrap())
}
