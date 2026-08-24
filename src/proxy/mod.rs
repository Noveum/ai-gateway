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
    original_request: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let provider = create_provider(provider_name)?;
    proxy_request_with_provider(config, provider, original_request).await
}

async fn proxy_request_with_provider(
    config: Arc<AppConfig>,
    provider: Box<dyn Provider>,
    mut original_request: Request<Body>,
) -> Result<Response<Body>, AppError> {
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
        let (access_key, secret_key, region) = provider
            .get_signing_credentials(&headers)
            .ok_or(AppError::MissingApiKey)?;
        let session_token = headers
            .get("x-aws-session-token")
            .and_then(|value| value.to_str().ok());
        signing::sign_aws_request(
            original_request.method().as_str(),
            &url,
            &prepared_body,
            &access_key,
            &secret_key,
            session_token,
            &region,
            "bedrock",
        )
        .await?
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
mod credential_tests {
    use super::proxy_request_with_provider;
    use crate::{
        config::AppConfig,
        error::AppError,
        providers::{BedrockProvider, Provider},
    };
    use async_trait::async_trait;
    use axum::{
        body::Body,
        http::{HeaderMap, HeaderValue, Request, Response},
    };
    use std::{sync::Arc, time::Duration};
    use tokio::{net::TcpListener, time::timeout};

    struct LoopbackSigningProvider {
        base_url: String,
        credential_parser: BedrockProvider,
    }

    #[async_trait]
    impl Provider for LoopbackSigningProvider {
        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        fn name(&self) -> &str {
            "loopback-signing-test"
        }

        fn process_headers(&self, headers: &HeaderMap) -> Result<HeaderMap, AppError> {
            Ok(headers.clone())
        }

        fn requires_signing(&self) -> bool {
            true
        }

        fn get_signing_credentials(&self, headers: &HeaderMap) -> Option<(String, String, String)> {
            self.credential_parser.get_signing_credentials(headers)
        }

        async fn process_response(
            &self,
            response: Response<Body>,
        ) -> Result<Response<Body>, AppError> {
            Ok(response)
        }
    }

    async fn assert_bedrock_credentials_fail_without_dispatch(
        access_key: Option<HeaderValue>,
        secret_key: Option<HeaderValue>,
    ) {
        // Route any erroneous fallback into a controlled loopback listener. A
        // correct credential failure returns before the HTTP client can connect.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dispatch_probe = tokio::spawn(async move {
            timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_ok()
        });

        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("x-aws-region", "us-east-1")
            .body(Body::from(
                r#"{"model":"amazon.nova-micro-v1:0","messages":[{"role":"user","content":"hello"}],"max_tokens":1}"#,
            ))
            .unwrap();
        if let Some(value) = access_key {
            request.headers_mut().insert("x-aws-access-key-id", value);
        }
        if let Some(value) = secret_key {
            request
                .headers_mut()
                .insert("x-aws-secret-access-key", value);
        }

        let result = timeout(
            Duration::from_secs(2),
            proxy_request_with_provider(
                Arc::new(AppConfig {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    worker_threads: 1,
                    max_connections: 1,
                }),
                Box::new(LoopbackSigningProvider {
                    base_url: format!("https://127.0.0.1:{port}"),
                    credential_parser: BedrockProvider::new(),
                }),
                request,
            ),
        )
        .await
        .expect("credential validation must finish locally");
        let dispatched = dispatch_probe.await.unwrap();

        match result {
            Err(AppError::MissingApiKey) => {}
            Err(error) => panic!("expected a credential error, got {error}"),
            Ok(_) => panic!("missing Bedrock credentials were accepted"),
        }
        assert!(!dispatched, "credential failure reached the network client");
    }

    #[tokio::test]
    async fn missing_bedrock_access_or_secret_fails_before_network_dispatch() {
        let valid_access = HeaderValue::from_static("AKIDEXAMPLE");
        let valid_secret = HeaderValue::from_static("secret-example");

        assert_bedrock_credentials_fail_without_dispatch(None, Some(valid_secret.clone())).await;
        assert_bedrock_credentials_fail_without_dispatch(Some(valid_access), None).await;
    }

    #[tokio::test]
    async fn malformed_bedrock_access_or_secret_fails_before_network_dispatch() {
        let valid_access = HeaderValue::from_static("AKIDEXAMPLE");
        let valid_secret = HeaderValue::from_static("secret-example");
        let malformed = [
            HeaderValue::from_static(""),
            HeaderValue::from_static("   "),
            HeaderValue::from_bytes(&[0x80]).unwrap(),
        ];

        for value in malformed.iter().cloned() {
            assert_bedrock_credentials_fail_without_dispatch(
                Some(value),
                Some(valid_secret.clone()),
            )
            .await;
        }
        for value in malformed {
            assert_bedrock_credentials_fail_without_dispatch(
                Some(valid_access.clone()),
                Some(value),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn authority_shaped_bedrock_region_fails_before_network_dispatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dispatch_probe = tokio::spawn(async move {
            timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_ok()
        });
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("x-aws-access-key-id", "AKIDEXAMPLE")
            .header("x-aws-secret-access-key", "secret-example")
            .header(
                "x-aws-region",
                format!("us-east-1.amazonaws.com@127.0.0.1:{port}"),
            )
            .body(Body::from(
                r#"{"model":"amazon.nova-micro-v1:0","messages":[{"role":"user","content":"hello"}],"max_tokens":1}"#,
            ))
            .unwrap();

        let result = timeout(
            Duration::from_secs(2),
            proxy_request_with_provider(
                Arc::new(AppConfig {
                    port: 0,
                    host: "127.0.0.1".to_string(),
                    worker_threads: 1,
                    max_connections: 1,
                }),
                Box::new(BedrockProvider::new()),
                request,
            ),
        )
        .await
        .expect("region validation must finish locally");
        let dispatched = dispatch_probe.await.unwrap();

        match result {
            Err(AppError::RequestError(message)) => {
                assert!(message.contains("x-aws-region"), "{message}");
            }
            Err(error) => panic!("expected a region validation error, got {error}"),
            Ok(_) => panic!("authority-shaped Bedrock region was accepted"),
        }
        assert!(!dispatched, "invalid region reached the network client");
    }
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
