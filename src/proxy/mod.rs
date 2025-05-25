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

    let response = client
        .request(method, url)
        .headers(reqwest_headers)
        .body(body)
        .send()
        .await?;

    // Check for error status codes and handle provider-specific errors
    if !response.status().is_success() {
        debug!("Received error status: {} from provider: {}", response.status(), provider.name());
        
        // Handle Azure OpenAI-specific errors
        if provider.name() == "azure-openai" {
            // Import the AzureOpenAIProvider to access error handling
            use crate::providers::azure_openai::{AzureOpenAIProvider, AzureOpenAIProviderConfig};
            
            // We need to downcast the provider to access Azure-specific methods
            // Since we can't downcast trait objects easily, we'll create a temporary instance
            // This is a temporary solution - in a more sophisticated design, we'd add error handling to the trait
            let config = AzureOpenAIProviderConfig::default();
            match AzureOpenAIProvider::with_config(config) {
                Ok(azure_provider) => {
                    let error = azure_provider.handle_azure_error(response).await;
                    return Err(error);
                }
                Err(e) => {
                    error!("Failed to create temporary Azure provider for error handling: {}", e);
                    // Fall through to generic error handling
                }
            }
        }
        
        // For other providers, use a generic error handling approach
        let status_code = response.status().as_u16();
        let error_body = response.text().await.unwrap_or_default();
        
        return Err(match status_code {
            401 => AppError::MissingApiKey,
            429 => AppError::RequestError("Rate limit exceeded".to_string()),
            400..=499 => AppError::RequestError(format!(
                "Client error ({}): {}", 
                status_code, 
                if error_body.is_empty() { "No details provided" } else { &error_body }
            )),
            500..=599 => AppError::HttpError(format!(
                "Server error ({}): {}", 
                status_code, 
                if error_body.is_empty() { "No details provided" } else { &error_body }
            )),
            _ => AppError::HttpError(format!(
                "HTTP error ({}): {}", 
                status_code, 
                if error_body.is_empty() { "No details provided" } else { &error_body }
            )),
        });
    }

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
