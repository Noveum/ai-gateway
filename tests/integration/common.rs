use dotenv::from_filename;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client,
};
use serde_json::{json, Value};
use std::env;

#[derive(Clone, Copy)]
enum OutputLimitField {
    MaxTokens,
    MaxCompletionTokens,
}

/// Configuration for a provider test
pub struct ProviderTestConfig {
    pub provider_name: String,
    pub api_key_env_var: String,
    pub model: String,
    pub prompt: String,
    pub max_tokens: u32,
    output_limit_field: OutputLimitField,
}

impl ProviderTestConfig {
    pub fn new(provider_name: &str, api_key_env_var: &str, model: &str) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            api_key_env_var: api_key_env_var.to_string(),
            model: model.to_string(),
            prompt: "Write a very short poem about Rust programming language".to_string(),
            max_tokens: 100,
            output_limit_field: OutputLimitField::MaxTokens,
        }
    }

    pub fn with_prompt(mut self, prompt: &str) -> Self {
        self.prompt = prompt.to_string();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self.output_limit_field = OutputLimitField::MaxTokens;
        self
    }

    pub fn with_max_completion_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self.output_limit_field = OutputLimitField::MaxCompletionTokens;
        self
    }

    /// Override the verified default model for a scheduled/manual smoke run.
    /// Empty or whitespace-only repository variables intentionally keep the
    /// checked-in default so a missing optional override cannot break CI.
    pub fn with_model_from_env(mut self, env_var_name: &str) -> Self {
        // Provider fixtures construct their config before `get_api_key` runs;
        // load the ignored test environment here so file-backed overrides and
        // workflow-provided variables have identical precedence.
        init_test_env();
        if let Ok(value) = env::var(env_var_name) {
            let value = value.trim();
            if !value.is_empty() {
                self.model = value.to_string();
            }
        }
        self
    }
}

#[cfg(test)]
mod config_tests {
    use super::ProviderTestConfig;

    #[test]
    fn model_override_uses_only_a_non_empty_environment_value() {
        const KEY: &str = "NOVEUM_TEST_ONLY_PROVIDER_MODEL_OVERRIDE";
        std::env::remove_var(KEY);

        let default = ProviderTestConfig::new("test", "TEST_API_KEY", "default-model")
            .with_model_from_env(KEY);
        assert_eq!(default.model, "default-model");

        std::env::set_var(KEY, "   ");
        let blank = ProviderTestConfig::new("test", "TEST_API_KEY", "default-model")
            .with_model_from_env(KEY);
        assert_eq!(blank.model, "default-model");

        std::env::set_var(KEY, " verified-model ");
        let overridden = ProviderTestConfig::new("test", "TEST_API_KEY", "default-model")
            .with_model_from_env(KEY);
        assert_eq!(overridden.model, "verified-model");

        std::env::remove_var(KEY);
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
        println!(
            "No .env.test found. Using only environment variables already exported by the caller."
        );
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
            if let Ok(session_token) = env::var("AWS_SESSION_TOKEN") {
                if !session_token.trim().is_empty() {
                    headers.insert(
                        "x-aws-session-token",
                        HeaderValue::from_str(session_token.trim()).unwrap(),
                    );
                }
            }
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
    let mut body = json!({
        "model": config.model,
        "messages": [
            {
                "role": "user",
                "content": config.prompt
            }
        ],
        "stream": stream
    });
    let field = match config.output_limit_field {
        OutputLimitField::MaxTokens => "max_tokens",
        OutputLimitField::MaxCompletionTokens => "max_completion_tokens",
    };
    body[field] = json!(config.max_tokens);
    if stream
        && matches!(
            config.provider_name.to_ascii_lowercase().as_str(),
            "openai" | "groq"
        )
    {
        // These APIs support OpenAI's explicit usage option. Together and
        // Fireworks report usage in their final chunks without this
        // undocumented request field; Anthropic/Bedrock usage comes from the
        // gateway's response translators.
        body["stream_options"] = json!({"include_usage": true});
    }
    body
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
/// response with sensible token usage.
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

#[derive(Default)]
struct StreamingSmokeState {
    data_chunks: Vec<Value>,
    saw_done: bool,
    saw_usage: bool,
}

impl StreamingSmokeState {
    fn process_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if line == "data: [DONE]" {
            self.saw_done = true;
            return;
        }
        if let Some(json_str) = line.strip_prefix("data: ") {
            if let Ok(json) = serde_json::from_str::<Value>(json_str) {
                self.saw_usage |= json.get("usage").is_some_and(|usage| {
                    usage.get("prompt_tokens").and_then(Value::as_u64).is_some()
                        && usage
                            .get("completion_tokens")
                            .and_then(Value::as_u64)
                            .is_some()
                });
                self.data_chunks.push(json);
            }
        }
    }

    fn validate_complete(&self) -> Result<(), &'static str> {
        if self.data_chunks.is_empty() {
            return Err("No streaming data chunks received");
        }
        if !self.saw_done {
            return Err("Streaming response ended without the required data: [DONE] marker");
        }
        if !self.saw_usage {
            return Err("Streaming response ended without terminal token usage");
        }
        Ok(())
    }
}

/// Run a streaming test for a provider.
///
/// Validates that the gateway streams OpenAI-compatible JSON chunks and finishes
/// with the protocol's `data: [DONE]` marker. A valid tool-call response may have
/// no text content, so completion is defined by the SSE protocol, not text deltas.
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
    let mut state = StreamingSmokeState::default();

    // Network chunks do not align to SSE event boundaries, so buffer raw bytes
    // and only parse complete lines (a `data:` line split across two chunks would
    // otherwise be dropped, making the test flaky).
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.expect("Failed to read chunk");
        buf.extend_from_slice(&chunk);
        // Drain complete lines (everything up to and including each '\n').
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            state.process_line(&String::from_utf8_lossy(&line_bytes));
        }
    }
    // Handle any final line not terminated by a newline.
    if !buf.is_empty() {
        state.process_line(&String::from_utf8_lossy(&buf));
    }

    state
        .validate_complete()
        .unwrap_or_else(|message| panic!("{message}"));

    println!(
        "Streaming test completed successfully for provider: {}",
        config.provider_name
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_smoke_rejects_a_stream_without_the_done_marker() {
        let mut state = StreamingSmokeState::default();
        state.process_line(r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#);

        let message = state
            .validate_complete()
            .expect_err("a stream without data: [DONE] must fail the smoke test");
        assert!(
            message.contains("data: [DONE]"),
            "failure should name the missing terminator, got: {message}"
        );
    }

    #[test]
    fn streaming_smoke_rejects_a_stream_without_terminal_usage() {
        let mut state = StreamingSmokeState::default();
        state.process_line(r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#);
        state.process_line("data: [DONE]");

        let message = state
            .validate_complete()
            .expect_err("a stream without terminal usage must fail the smoke test");
        assert!(message.contains("usage"), "unexpected failure: {message}");
    }

    #[test]
    fn streaming_smoke_accepts_tool_calls_without_text_content() {
        let mut state = StreamingSmokeState::default();
        state.process_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"lookup","arguments":"{}"}}]}}]}"#,
        );
        state.process_line(
            r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}"#,
        );
        state.process_line("data: [DONE]");

        assert_eq!(state.validate_complete(), Ok(()));
    }

    #[test]
    fn request_body_can_select_max_completion_tokens_without_leaking_max_tokens() {
        let config = ProviderTestConfig::new("openai", "OPENAI_API_KEY", "gpt-4o-mini")
            .with_max_completion_tokens(16);
        let body = create_test_request_body(&config, false);

        assert_eq!(body["max_completion_tokens"], 16);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn streaming_smoke_requests_terminal_usage() {
        let config = ProviderTestConfig::new("openai", "OPENAI_API_KEY", "gpt-4o-mini");
        let body = create_test_request_body(&config, true);

        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn streaming_smoke_does_not_send_undocumented_usage_options() {
        for provider in ["together", "fireworks"] {
            let config = ProviderTestConfig::new(provider, "TEST_API_KEY", "verified-model");
            let body = create_test_request_body(&config, true);
            assert!(body.get("stream_options").is_none(), "{provider}: {body}");
        }
    }
}
