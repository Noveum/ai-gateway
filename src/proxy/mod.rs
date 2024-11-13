use axum::{
    body::{self, Body, Bytes},
    http::{HeaderMap, HeaderValue, Request, Response, StatusCode},
};
use futures_util::StreamExt;
use reqwest::Method;
use std::sync::Arc;
use tracing::{debug, error, info};

use crate::{config::AppConfig, error::AppError, providers::create_provider};

mod client;
pub use client::CLIENT;

pub async fn proxy_request_to_provider(
    _config: Arc<AppConfig>,
    provider_name: &str,
    original_request: Request<Body>,
) -> Result<Response<Body>, AppError> {
    info!(
        provider = provider_name,
        method = %original_request.method(),
        path = %original_request.uri().path(),
        "Incoming request"
    );

    // Create provider instance
    let provider = create_provider(provider_name)?;

    // Call before request hook
    provider.before_request(&original_request).await?;

    let path = original_request.uri().path();
    let modified_path = provider.transform_path(path);

    let query = original_request
        .uri()
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();

    let url = format!("{}{}{}", provider.base_url(), modified_path, query);

    debug!(
        provider = provider_name,
        url = %url,
        "Preparing proxy request"
    );

    // Process headers
    let headers = provider.process_headers(original_request.headers())?;

    // Create and send request
    let response = send_provider_request(
        original_request.method().clone(),
        url,
        headers,
        original_request.into_body(),
    )
    .await?;

    // Process response
    let processed_response = provider.process_response(response).await?;

    // Call after request hook
    provider.after_request(&processed_response).await?;

    Ok(processed_response)
}

// Helper function to send the actual request
async fn send_provider_request(
    method: http::Method,
    url: String,
    headers: HeaderMap,
    body: Body,
) -> Result<Response<Body>, AppError> {
    let body_bytes = body::to_bytes(body, usize::MAX).await?;

    let client = &*CLIENT;
    let method =
        Method::from_bytes(method.as_str().as_bytes()).map_err(|_| AppError::InvalidMethod)?;

    // Convert http::HeaderMap to reqwest::HeaderMap
    let mut reqwest_headers = reqwest::header::HeaderMap::new();
    for (name, value) in headers.iter() {
        if let Ok(v) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
            // Convert the header name to a string first
            if let Ok(name_str) = name.as_str().parse::<reqwest::header::HeaderName>() {
                reqwest_headers.insert(name_str, v);
            }
        }
    }

    let response = client
        .request(method, url)
        .headers(reqwest_headers) // Now using the converted reqwest::HeaderMap
        .body(body_bytes.to_vec())
        .send()
        .await?;

    process_response(response).await
}

// Add this function after the send_provider_request function

async fn process_response(response: reqwest::Response) -> Result<Response<Body>, AppError> {
    let status = StatusCode::from_u16(response.status().as_u16())?;

    // Check if response is a stream
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map_or(false, |ct| ct.contains("text/event-stream"))
    {
        debug!("Processing streaming response");

        // Convert headers
        let mut response_headers = HeaderMap::new();
        for (name, value) in response.headers() {
            if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
                if let Ok(header_name) = http::HeaderName::from_bytes(name.as_ref()) {
                    response_headers.insert(header_name, v);
                }
            }
        }

        // Set up streaming response
        let stream = response.bytes_stream().map(|result| match result {
            Ok(bytes) => {
                debug!("Streaming chunk: {} bytes", bytes.len());
                Ok(bytes)
            }
            Err(e) => {
                error!("Stream error: {}", e);
                Err(std::io::Error::new(std::io::ErrorKind::Other, e))
            }
        });

        Ok(Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .extension(response_headers)
            .body(Body::from_stream(stream))
            .unwrap())
    } else {
        debug!("Processing regular response");

        // Convert headers
        let mut response_headers = HeaderMap::new();
        for (name, value) in response.headers() {
            if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
                if let Ok(header_name) = http::HeaderName::from_bytes(name.as_ref()) {
                    response_headers.insert(header_name, v);
                }
            }
        }

        // Process regular response body
        let body = response.bytes().await?;

        let mut builder = Response::builder().status(status);
        for (name, value) in response_headers.iter() {
            builder = builder.header(name, value);
        }

        Ok(builder.body(Body::from(body)).unwrap())
    }
}
