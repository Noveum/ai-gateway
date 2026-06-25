use dotenv::from_filename;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client,
};
use serde_json::{json, Value};
use std::env;

/// Configuration for a provider test
pub struct ProviderTestConfig {
    pub provider_name: String,
    pub api_key_env_var: String,
    pub model: String,
    pub prompt: String,
    pub max_tokens: u32,
}

impl ProviderTestConfig {
    pub fn new(provider_name: &str, api_key_env_var: &str, model: &str) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            api_key_env_var: api_key_env_var.to_string(),
            model: model.to_string(),
            prompt: "Write a very short poem about Rust programming language".to_string(),
            max_tokens: 100,
        }
    }

    pub fn with_prompt(mut self, prompt: &str) -> Self {
        self.prompt = prompt.to_string();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

/// Initialize environment variables from .env.test file for tests
pub fn init_test_env() {
    let mut loaded_test_env = false;

    if from_filename(".env.test").is_ok() {
        loaded_test_env = true;
        println!("Loaded environment from .env.test");
    } else if from_filename("tests/.env.test").is_ok() {
        loaded_test_env = true;
        println!("Loaded environment from tests/.env.test");
    }

    if !loaded_test_env {
        if dotenv::dotenv().is_ok() {
            println!("No .env.test found. Using .env file instead.");
        } else {
            println!("Warning: Neither .env.test nor .env files were found. Make sure you have proper environment variables set.");
        }
    }
}

/// Set up request headers for a provider test
pub fn setup_test_headers(provider: &str, api_key: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();

    match provider {
        "bedrock" => {
            // For Bedrock, we need AWS credentials
            let aws_access_key =
                env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID must be set");
            let aws_secret_key =
                env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY must be set");
            let aws_region = env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());

            headers.insert(
                "x-aws-access-key-id",
                HeaderValue::from_str(&aws_access_key).unwrap(),
            );
            headers.insert(
                "x-aws-secret-access-key",
                HeaderValue::from_str(&aws_secret_key).unwrap(),
            );
            headers.insert("x-aws-region", HeaderValue::from_str(&aws_region).unwrap());
        }
        _ => {
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {}", api_key)).unwrap(),
            );
        }
    }

    headers.insert(
        "Content-Type",
        HeaderValue::from_str("application/json").unwrap(),
    );
    headers.insert("x-provider", HeaderValue::from_str(provider).unwrap());
    headers.insert(
        "x-organisation-id",
        HeaderValue::from_str("TEST_ORG").unwrap(),
    );
    headers.insert(
        "x-project-id",
        HeaderValue::from_str("TEST_PROJECT").unwrap(),
    );
    headers.insert(
        "x-experiment-id",
        HeaderValue::from_str("TEST_EXPERIMENT").unwrap(),
    );
    headers.insert("x-user-id", HeaderValue::from_str("TEST_USER").unwrap());

    headers
}

/// Create a request body for a provider test
pub fn create_test_request_body(config: &ProviderTestConfig, stream: bool) -> Value {
    json!({
        "model": config.model,
        "messages": [
            {
                "role": "user",
                "content": config.prompt
            }
        ],
        "stream": stream,
        "max_tokens": config.max_tokens
    })
}

/// Get API key for the provider
fn get_api_key(env_var_name: &str) -> String {
    init_test_env();

    if env_var_name == "AWS_ACCESS_KEY_ID" {
        if env::var("AWS_SECRET_ACCESS_KEY").is_err() {
            panic!("AWS_SECRET_ACCESS_KEY must be set for Bedrock tests");
        }
        if env::var("AWS_REGION").is_err() {
            panic!("AWS_REGION must be set for Bedrock tests");
        }
    }

    match env::var(env_var_name) {
        Ok(key) if !key.is_empty() => key,
        Ok(_) => panic!(
            "{} is set but empty. Please provide a valid value.",
            env_var_name
        ),
        Err(_) => panic!(
            "{} must be set either in .env.test file or as an environment variable.",
            env_var_name
        ),
    }
}

fn gateway_url() -> String {
    env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:3000".to_string())
}

/// Run a non-streaming test for a provider.
///
/// Validates that the gateway proxies the request and returns an OpenAI-compatible
/// response with sensible token usage. (Telemetry export is validated separately
/// via the Noveum trace exporter's unit tests, not by querying a backing store.)
pub async fn run_non_streaming_test(config: &ProviderTestConfig) {
    let api_key = get_api_key(&config.api_key_env_var);
    let gateway_url = gateway_url();

    println!(
        "Running non-streaming test for provider: {}",
        config.provider_name
    );

    let headers = setup_test_headers(&config.provider_name, &api_key);
    let request_body = create_test_request_body(config, false);

    let client = Client::new();
    let response = client
        .post(format!("{}/v1/chat/completions", gateway_url))
        .headers(headers.clone())
        .json(&request_body)
        .send()
        .await
        .expect("Failed to send request");

    println!("Response status: {}", response.status());

    if response.status() == StatusCode::FORBIDDEN {
        let error_body = response
            .text()
            .await
            .expect("Failed to read error response body");
        panic!("Request failed with status 403 Forbidden: {error_body}");
    }

    assert!(
        response.status().is_success(),
        "Request failed with status: {} - is the gateway running?",
        response.status()
    );

    let response_body = response
        .json::<Value>()
        .await
        .expect("Failed to parse response as JSON");

    // Validate OpenAI-compatible response structure.
    assert!(
        response_body.get("choices").is_some(),
        "Response missing 'choices' field"
    );
    assert!(
        response_body.get("usage").is_some(),
        "Response missing 'usage' field"
    );

    let usage = response_body.get("usage").unwrap();
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .expect("Missing prompt_tokens");
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .expect("Missing completion_tokens");
    let total_tokens = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .expect("Missing total_tokens");

    assert!(prompt_tokens > 0, "prompt_tokens should be greater than 0");
    assert!(
        completion_tokens > 0,
        "completion_tokens should be greater than 0"
    );
    assert_eq!(
        prompt_tokens + completion_tokens,
        total_tokens,
        "Total tokens should equal prompt + completion tokens"
    );

    println!(
        "Non-streaming test completed successfully for provider: {}",
        config.provider_name
    );
}

/// Run a streaming test for a provider.
///
/// Validates that the gateway streams an OpenAI-compatible SSE response and that
/// non-empty content is reconstructable from the chunks.
pub async fn run_streaming_test(config: &ProviderTestConfig) {
    let api_key = get_api_key(&config.api_key_env_var);
    let gateway_url = gateway_url();

    println!(
        "Running streaming test for provider: {}",
        config.provider_name
    );

    let headers = setup_test_headers(&config.provider_name, &api_key);
    let request_body = create_test_request_body(config, true);

    let client = Client::new();
    let response = client
        .post(format!("{}/v1/chat/completions", gateway_url))
        .headers(headers.clone())
        .json(&request_body)
        .send()
        .await
        .expect("Failed to send request");

    println!("Response status: {}", response.status());

    if response.status() == StatusCode::FORBIDDEN {
        let error_body = response
            .text()
            .await
            .expect("Failed to read error response body");
        panic!("Request failed with status 403 Forbidden: {error_body}");
    }

    assert!(
        response.status().is_success(),
        "Request failed with status: {} - is the gateway running?",
        response.status()
    );

    let mut stream = response.bytes_stream();
    let mut stream_data: Vec<Value> = Vec::new();
    let mut content = String::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.expect("Failed to read chunk");
        let chunk_str = std::str::from_utf8(&chunk).expect("Invalid UTF-8");

        for line in chunk_str.lines() {
            if line.trim().is_empty() || line == "data: [DONE]" {
                continue;
            }
            if let Some(json_str) = line.strip_prefix("data: ") {
                if let Ok(json) = serde_json::from_str::<Value>(json_str) {
                    if let Some(delta) = json
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("delta"))
                        .and_then(|d| d.get("content"))
                        .and_then(|c| c.as_str())
                    {
                        content.push_str(delta);
                    }
                    stream_data.push(json);
                }
            }
        }
    }

    assert!(!stream_data.is_empty(), "No streaming data chunks received");
    assert!(
        !content.is_empty(),
        "Reconstructed streaming content should not be empty"
    );

    println!(
        "Streaming test completed successfully for provider: {}",
        config.provider_name
    );
}
