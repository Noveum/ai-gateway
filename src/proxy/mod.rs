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
        .map_err(|e| AppError::AxumError(e))?;

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
    _provider: &Box<dyn Provider>,
    config: Arc<AppConfig>,
) -> Result<Response<Body>, AppError> {
    let client = client::client_for_url(&url);

    let reqwest_headers = headers
        .iter()
        .filter_map(|(name, value)| {
            name.as_str()
                .parse::<reqwest::header::HeaderName>()
                .ok()
                .zip(reqwest::header::HeaderValue::from_bytes(value.as_bytes()).ok())
        })
        .collect::<reqwest::header::HeaderMap>();

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
    _config: Arc<AppConfig>,
) -> Result<Response<Body>, AppError> {
    let status = StatusCode::from_u16(response.status().as_u16())?;
    let mut response_builder = Response::builder().status(status);

    let response_content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let is_aws_event_stream = response_content_type.contains("application/vnd.amazon.eventstream");
    let is_sse = response_content_type.contains("text/event-stream");
    let is_streaming = is_aws_event_stream || is_sse;

    // Copy provider headers. Streaming framing headers are gateway-owned: if
    // the upstream values are copied and then added again below, `http` keeps
    // both values and clients receive e.g.
    // `content-type: text/event-stream; charset=utf-8, text/event-stream`.
    for (name, value) in response.headers() {
        if is_sse
            && matches!(
                name.as_str(),
                "content-type"
                    | "content-length"
                    | "cache-control"
                    | "connection"
                    | "transfer-encoding"
                    | "x-accel-buffering"
            )
        {
            continue;
        }
        if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
            response_builder = response_builder.header(name.clone(), v);
        }
    }

    // Fast path for non-streaming responses
    if !is_streaming {
        let body = response.bytes().await?;
        return Ok(response_builder.body(Body::from(body)).unwrap());
    }

    // Optimized streaming response handling
    debug!("Processing streaming response");

    let stream = response.bytes_stream().map(|result| match result {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            error!("Stream error: {}", e);
            Err(std::io::Error::other(e))
        }
    });

    // OpenAI-compatible streams are already SSE and receive one canonical set
    // of framing headers. AWS EventStream must keep its native content type:
    // `BedrockProvider::process_response` runs after this function and uses that
    // header to select its binary frame decoder before producing SSE itself.
    if is_sse {
        response_builder = response_builder
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .header("transfer-encoding", "chunked")
            .header("x-accel-buffering", "no");
    }

    Ok(response_builder.body(Body::from_stream(stream)).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn streaming_response_replaces_framing_headers_instead_of_duplicating_them() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/events"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream; charset=utf-8")
                    .insert_header("cache-control", "private")
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream; charset=utf-8"),
            )
            .mount(&upstream)
            .await;

        let upstream_response = reqwest::get(format!("{}/events", upstream.uri()))
            .await
            .expect("mock stream response");
        let response = process_response(
            upstream_response,
            Arc::new(AppConfig {
                port: 3000,
                host: "127.0.0.1".into(),
                worker_threads: 1,
                max_connections: 1,
            }),
        )
        .await
        .expect("proxy stream response");

        assert_eq!(
            response.headers().get_all("content-type").iter().count(),
            1,
            "a second content-type value breaks strict SSE clients"
        );
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(
            response.headers().get_all("cache-control").iter().count(),
            1
        );
        assert_eq!(response.headers()["cache-control"], "no-cache");
    }

    #[tokio::test]
    async fn aws_event_stream_keeps_its_native_content_type_for_the_bedrock_decoder() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/events"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(Vec::<u8>::new(), "application/vnd.amazon.eventstream"),
            )
            .mount(&upstream)
            .await;

        let upstream_response = reqwest::get(format!("{}/events", upstream.uri()))
            .await
            .expect("mock AWS event stream response");
        let response = process_response(
            upstream_response,
            Arc::new(AppConfig {
                port: 3000,
                host: "127.0.0.1".into(),
                worker_threads: 1,
                max_connections: 1,
            }),
        )
        .await
        .expect("proxy AWS event stream response");

        assert_eq!(
            response.headers()["content-type"],
            "application/vnd.amazon.eventstream",
            "BedrockProvider selects its binary frame decoder from this header"
        );
    }
}
