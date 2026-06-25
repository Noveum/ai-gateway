use dotenv::from_filename;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client,
};
use serde_json::{json, Value};
use std::env;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

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
    // For tests, we prioritize .env.test files
    let mut loaded_test_env = false;

    // Try to load from .env.test in project root first
    if from_filename(".env.test").is_ok() {
        loaded_test_env = true;
        println!("Loaded environment from .env.test");
    } else if from_filename("tests/.env.test").is_ok() {
        // Try tests/.env.test as fallback
        loaded_test_env = true;
        println!("Loaded environment from tests/.env.test");
    }

    if !loaded_test_env {
        // Only if no test environment was loaded, try the standard .env file
        if dotenv::dotenv().is_ok() {
            println!("No .env.test found. Using .env file instead.");
        } else {
            println!("Warning: Neither .env.test nor .env files were found. Make sure you have proper environment variables set.");
        }
    }
}

/// Helper function to generate a unique request ID for tracking
pub fn generate_request_id() -> String {
    format!("test-{}", Uuid::new_v4().to_string())
}

/// Search ElasticSearch for a document with the given gateway request ID
pub async fn search_elasticsearch(gateway_request_id: &str) -> Result<Value, reqwest::Error> {
    // Ensure environment variables are loaded
    init_test_env();

    let es_url =
        env::var("ELASTICSEARCH_URL").expect("ELASTICSEARCH_URL must be set in .env.test file");
    let es_username = env::var("ELASTICSEARCH_USERNAME")
        .expect("ELASTICSEARCH_USERNAME must be set in .env.test file");
    let es_password = env::var("ELASTICSEARCH_PASSWORD")
        .expect("ELASTICSEARCH_PASSWORD must be set in .env.test file");
    let es_index =
        env::var("ELASTICSEARCH_INDEX").expect("ELASTICSEARCH_INDEX must be set in .env.test file");

    let client = Client::new();
    let search_url = format!("{}/{}/_search", es_url, es_index);

    let query = json!({
        "query": {
            "term": {
                "attributes.metadata.provider_request_id.keyword": gateway_request_id
            }
        }
    });

    println!("Searching ElasticSearch with query: {}", query);

    let response = client
        .post(&search_url)
        .basic_auth(es_username, Some(es_password))
        .json(&query)
        .send()
        .await?;

    response.json::<Value>().await
}

/// Set up request headers for a provider test
pub fn setup_test_headers(provider: &str, api_key: &str, request_id: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();

    // Different header setup based on provider
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
            // For other providers, use Bearer token auth
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {}", api_key)).unwrap(),
            );
        }
    }

    // Common headers for all providers
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
    // Ensure environment variables are loaded
    init_test_env();

    // For Bedrock, we need to ensure all required AWS credentials are available
    if env_var_name == "AWS_ACCESS_KEY_ID" {
        // Make sure the other AWS credentials are set as well
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

/// Run a non-streaming test for a provider
pub async fn run_non_streaming_test(config: &ProviderTestConfig) {
    // Get the API key for the provider
    let api_key = get_api_key(&config.api_key_env_var);

    // Get gateway URL from environment or use default
    let gateway_url =
        env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:3000".to_string());

    println!(
        "Running non-streaming test for provider: {}",
        config.provider_name
    );

    // Generate a unique request ID for tracking
    let request_id = generate_request_id();

    // Setup headers
    let headers = setup_test_headers(&config.provider_name, &api_key, &request_id);

    // Print request ID for debugging
    println!("Request ID: {}", request_id);

    // Create request body
    let request_body = create_test_request_body(config, false);

    // Send request to the gateway
    let client = Client::new();
    let response = client
        .post(&format!("{}/v1/chat/completions", gateway_url))
        .headers(headers.clone())
        .json(&request_body)
        .send()
        .await
        .expect("Failed to send request");

    // Print the status code
    println!("Response status: {}", response.status());

    // If we got a 403 Forbidden error, print the response body to debug
    if response.status() == StatusCode::FORBIDDEN {
        let error_body = response
            .text()
            .await
            .expect("Failed to read error response body");
        println!("Error response from gateway: {}", error_body);
        panic!("Request failed with status: 403 Forbidden - Make sure your AWS credentials have the correct permissions for AWS Bedrock");
    }

    // Ensure the request was successful
    assert!(response.status().is_success(),
            "Request failed with status: {} - Make sure the AI Gateway is running with ENABLE_ELASTICSEARCH=true",
            response.status());

    // Extract gateway request ID from headers
    let response_headers = response.headers().clone();
    let gateway_request_id = response_headers
        .get("x-request-id")
        .expect("x-request-id header not found in response")
        .to_str()
        .expect("Invalid x-request-id header value");

    println!("Gateway request ID from headers: {}", gateway_request_id);

    // Get the response body
    let response_body = response
        .json::<Value>()
        .await
        .expect("Failed to parse response as JSON");

    // Print response for debugging
    // println!("Response: {:#?}", response_body);

    // Validate response structure - still keep basic validation for immediate feedback
    assert!(
        response_body.get("choices").is_some(),
        "Response missing 'choices' field"
    );
    assert!(
        response_body.get("usage").is_some(),
        "Response missing 'usage' field"
    );

    // Extract token counts
    let usage = response_body.get("usage").unwrap();
    let prompt_tokens = usage
        .get("prompt_tokens")
        .expect("Missing prompt_tokens")
        .as_u64()
        .unwrap();
    let completion_tokens = usage
        .get("completion_tokens")
        .expect("Missing completion_tokens")
        .as_u64()
        .unwrap();
    let total_tokens = usage
        .get("total_tokens")
        .expect("Missing total_tokens")
        .as_u64()
        .unwrap();

    // Validate token counts
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

    // Wait for data to be indexed in ElasticSearch
    println!("Waiting for data to be indexed in ElasticSearch...");
    sleep(Duration::from_secs(3)).await;

    // Load environment variables (to make it clear in the logs)
    dotenv::from_filename(".env.test").ok();

    // Search ElasticSearch for the request using gateway request ID
    let es_response = search_elasticsearch(gateway_request_id)
        .await
        .expect("Failed to search ElasticSearch");

    // Basic validation to fail early if something is obviously wrong
    let hits = es_response
        .get("hits")
        .and_then(|h| h.get("hits"))
        .expect("No hits in ElasticSearch response");
    let hits_array = hits.as_array().expect("Hits is not an array");
    assert!(
        !hits_array.is_empty(),
        "No matching documents found in ElasticSearch"
    );

    // Use LLM to validate the test results
    let llm_validation_passed = validate_with_llm(
        &config.provider_name,
        &config.model,
        &request_id,
        &headers,
        &response_body,
        &es_response,
    )
    .await;

    // Assert that the LLM validation passed
    assert!(llm_validation_passed, "LLM validation failed");

    println!(
        "Non-streaming test completed successfully for provider: {}",
        config.provider_name
    );
}

/// Run a streaming test for a provider
pub async fn run_streaming_test(config: &ProviderTestConfig) {
    // Get the API key for the provider
    let api_key = get_api_key(&config.api_key_env_var);

    // Get gateway URL from environment or use default
    let gateway_url =
        env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:3000".to_string());

    println!(
        "Running streaming test for provider: {}",
        config.provider_name
    );

    // Generate a unique request ID for tracking
    let request_id = generate_request_id();

    // Setup headers
    let headers = setup_test_headers(&config.provider_name, &api_key, &request_id);

    // Print request ID for debugging
    println!("Request ID: {}", request_id);

    // Create request body with streaming enabled
    let request_body = create_test_request_body(config, true);

    // Send request to the gateway
    let client = Client::new();
    let response = client
        .post(&format!("{}/v1/chat/completions", gateway_url))
        .headers(headers.clone())
        .json(&request_body)
        .send()
        .await
        .expect("Failed to send request");

    // Print the status code
    println!("Response status: {}", response.status());

    // If we got a 403 Forbidden error, print the response body to debug
    if response.status() == StatusCode::FORBIDDEN {
        let error_body = response
            .text()
            .await
            .expect("Failed to read error response body");
        println!("Error response from gateway: {}", error_body);
        panic!("Request failed with status: 403 Forbidden - Make sure your AWS credentials have the correct permissions for AWS Bedrock");
    }

    // Ensure the request was successful
    assert!(response.status().is_success(),
            "Request failed with status: {} - Make sure the AI Gateway is running with ENABLE_ELASTICSEARCH=true",
            response.status());

    // Extract gateway request ID from headers
    let response_headers = response.headers().clone();
    let gateway_request_id = response_headers
        .get("x-request-id")
        .expect("x-request-id header not found in response")
        .to_str()
        .expect("Invalid x-request-id header value");

    println!("Gateway request ID from headers: {}", gateway_request_id);

    // Get a reference to the response body stream
    let mut stream = response.bytes_stream();

    // Consume the streaming response
    let mut stream_data = Vec::new();
    let mut provider_request_id = String::new();

    // Process stream chunks
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.expect("Failed to read chunk");
        let chunk_str = std::str::from_utf8(&chunk).expect("Invalid UTF-8");

        // Process each line in the chunk
        for line in chunk_str.lines() {
            // Skip empty lines or data: [DONE]
            if line.trim().is_empty() || line == "data: [DONE]" {
                continue;
            }

            // Process chunk (remove "data: " prefix and parse JSON)
            if let Some(json_str) = line.strip_prefix("data: ") {
                if let Ok(json) = serde_json::from_str::<Value>(json_str) {
                    stream_data.push(json.clone());

                    // Extract provider request ID from chunk if available and not already set
                    if provider_request_id.is_empty() && json.get("id").is_some() {
                        provider_request_id =
                            json.get("id").unwrap().as_str().unwrap_or("").to_string();

                        println!("Provider request ID from stream: {}", provider_request_id);
                    }
                }
            }
        }
    }

    // Validate we received some streaming chunks
    assert!(!stream_data.is_empty(), "No streaming data chunks received");

    sleep(Duration::from_secs(3)).await;

    // Load environment variables (to make it clear in the logs)
    dotenv::from_filename(".env.test").ok();

    // Search ElasticSearch for the request using gateway request ID
    let es_response = search_elasticsearch(gateway_request_id)
        .await
        .expect("Failed to search ElasticSearch");

    // Basic validation to fail early if something is obviously wrong
    let hits = es_response
        .get("hits")
        .and_then(|h| h.get("hits"))
        .expect("No hits in ElasticSearch response");
    let hits_array = hits.as_array().expect("Hits is not an array");
    assert!(
        !hits_array.is_empty(),
        "No matching documents found in ElasticSearch"
    );

    // Reconstruct the complete response from the streaming chunks for the LLM validation
    let last_chunk = stream_data.last().unwrap();

    // Create the reconstructed response based on provider
    let reconstructed_response = if config.provider_name == "openai" {
        // For OpenAI, we need to handle the missing usage field in streaming responses
        serde_json::json!({
            "id": last_chunk.get("id").unwrap_or(&serde_json::Value::Null),
            "object": "chat.completion",
            "model": last_chunk.get("model").unwrap_or(&serde_json::Value::Null),
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": stream_data.iter()
                        .filter_map(|chunk| chunk.get("choices").and_then(|choices|
                            choices.get(0).and_then(|choice|
                                choice.get("delta").and_then(|delta|
                                    delta.get("content").and_then(|content|
                                        content.as_str())))))
                        .collect::<Vec<_>>()
                        .join("")
                },
                "finish_reason": last_chunk.get("choices")
                    .and_then(|choices| choices.get(0))
                    .and_then(|choice| choice.get("finish_reason"))
                    .unwrap_or(&serde_json::Value::Null)
            }]
            // Intentionally omit usage field for OpenAI streaming
        })
    } else {
        // For other providers, include a usage field if available
        let usage = serde_json::json!({
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        });

        serde_json::json!({
            "id": last_chunk.get("id").unwrap_or(&serde_json::Value::Null),
            "object": "chat.completion",
            "model": last_chunk.get("model").unwrap_or(&serde_json::Value::Null),
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": stream_data.iter()
                        .filter_map(|chunk| chunk.get("choices").and_then(|choices|
                            choices.get(0).and_then(|choice|
                                choice.get("delta").and_then(|delta|
                                    delta.get("content").and_then(|content|
                                        content.as_str())))))
                        .collect::<Vec<_>>()
                        .join("")
                },
                "finish_reason": last_chunk.get("choices")
                    .and_then(|choices| choices.get(0))
                    .and_then(|choice| choice.get("finish_reason"))
                    .unwrap_or(&serde_json::Value::Null)
            }],
            "usage": usage
        })
    };

    // Use LLM to validate the test results
    let llm_validation_passed = validate_with_llm(
        &config.provider_name,
        &config.model,
        &request_id,
        &headers,
        &reconstructed_response,
        &es_response,
    )
    .await;

    // Assert that the LLM validation passed
    assert!(llm_validation_passed, "LLM validation failed");

    println!(
        "Streaming test completed successfully for provider: {}",
        config.provider_name
    );
}

/// Validate test results using an OpenAI LLM
pub async fn validate_with_llm(
    provider_name: &str,
    model_name: &str,
    request_id: &str,
    request_headers: &HeaderMap,
    response_body: &Value,
    es_response: &Value,
) -> bool {
    // Get OpenAI API key from environment
    let openai_api_key = get_api_key("OPENAI_API_KEY");

    // Format the prompt with all the necessary data
    let gateway_request_id = request_headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    // Create a cleaned version of headers for the prompt
    let headers_json = serde_json::json!({
        "authorization": mask_api_key(request_headers.get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")),
        "content-type": request_headers.get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "x-provider": request_headers.get("x-provider")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "x-organisation-id": request_headers.get("x-organisation-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "x-project-id": request_headers.get("x-project-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "x-experiment-id": request_headers.get("x-experiment-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "x-user-id": request_headers.get("x-user-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    });

    // Build the prompt for the OpenAI model
    let prompt = format!(
        "As a LLM judge and validator your task is to make sure that all metrics are getting accurately logged for the given request ->\n\n\n\
        This is our request and other logs ->\n\
        Also make sure that the response from provider is OpenAI compatible always. Give error if the response format is not OpenAI compatible.
        Running non-streaming test for provider: {}\n\
        Request ID: {}\n\
        Request Headers: {}\n\
        Gateway request ID from headers: {}\n\
        Response: {:#?}\n\
        Waiting for data to be indexed in ElasticSearch...\n\
        Loaded environment from .env.test\n\n\
        This is what we stored in ElasticSearch -> \n{:#?}\n\n\n\
        Fields we really care about ->\n\
        tokens computation (match input_token, output_token, total_token). They can be named differently, so for validation check in the response from the ES log.\n\
        IMPORTANT NOTE: For streaming responses, the tokens may not be present in the response object. In this case, only validate tokens in metrics if they exist in the response object. If tokens are not in the response, they can be ignored.\n\
        There should be a valid request and response.\n\
        request-id\n\
        organisation+id or org_id\n\
        project_id\n\
        experiment_id\n\
        user_id\n\n\
        Ignore these fields ->\n\
        - provider_latency can be zero.\n\
        - provider_status_code\n\
        - provider_latency\n\
        - Model Mismatch like these are okay -> Request used '{model_name}', but logged request shows a different model name\n\n\n\
        and other metrics. So just tell me in JSON response did the test pass or fail?\n\
        IMPORTANT: A mismatch in token counts is NOT an error. be smart and match prompt_tokens with input, completion with output and total with total tokens
        and other metrics. So just tell me in JSON response did the test pass or fail?
        Which field failed, de descriptive in error message with failed field and reason?
        IMPORTANT: Your response must be a valid JSON object in EXACTLY this format:\n\
        {{\n  \"test_result\": \"pass\",\n  \"failed_fields\": []\n}}\n\
        where test_result is either \"pass\" or \"fail\", and failed_fields is an array of field names that failed validation.\n\
        Do not include any explanation, only return the JSON object.",
        provider_name,
        request_id,
        headers_json,
        gateway_request_id,
        response_body,
        es_response
    );
    // Create OpenAI request
    let openai_request = serde_json::json!({
        "model": "gpt-4o",
        "messages": [
            {
                "role": "user",
                "content": prompt
            }
        ],
        "response_format": {
            "type": "json_object"
        },
        "temperature": 0
    });

    // Send request to OpenAI
    let client = Client::new();
    let openai_response = client
        .post("https://gateway.noveum.ai/v1/chat/completions")
        // .post("https://api.openai.com/v1/chat/completions")
        .header("provider", "openai")
        .header("x-project-id", "noveum-integration-test")
        .header("x-organisation-id", "noveumtest")
        .header("x-experiment-id", "eval_job_1")
        .header("x-user-id", "shashank")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", openai_api_key))
        .json(&openai_request)
        .send()
        .await
        .expect("Failed to send request to OpenAI");

    // Parse OpenAI response
    let openai_result = openai_response
        .json::<Value>()
        .await
        .expect("Failed to parse OpenAI response");

    // Extract and parse the validation result
    let validation_result = openai_result
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .and_then(|content_str| serde_json::from_str::<Value>(content_str).ok())
        .expect("Failed to parse validation result from OpenAI");

    // Print the full validation result for debugging
    println!("LLM validation result: {:#?}", validation_result);

    // Check if the test passed according to the LLM
    let test_passed = validation_result
        .get("test_result")
        .and_then(|result| result.as_str())
        .map(|result| result == "pass")
        .unwrap_or(false);

    if !test_passed {
        let failed_fields = validation_result
            .get("failed_fields")
            .and_then(|fields| fields.as_array())
            .map(|fields| {
                fields
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "unknown fields".to_string());

        println!("❌ LLM validation failed. Failed fields: {}", failed_fields);
    } else {
        println!("✅ LLM validation passed!");
    }

    test_passed
}

// Helper function to mask API keys
fn mask_api_key(api_key: &str) -> String {
    if api_key.is_empty() {
        return String::from("");
    }

    // If it contains "Bearer ", keep that prefix
    if let Some(stripped) = api_key.strip_prefix("Bearer ") {
        if stripped.len() <= 8 {
            return format!("Bearer {}", stripped);
        }

        let visible_start = &stripped[..4];
        let visible_end = &stripped[stripped.len() - 4..];
        return format!("Bearer {}...{}", visible_start, visible_end);
    }

    // Otherwise just mask the key directly
    if api_key.len() <= 8 {
        return api_key.to_string();
    }

    let visible_start = &api_key[..4];
    let visible_end = &api_key[api_key.len() - 4..];
    format!("{}...{}", visible_start, visible_end)
}
