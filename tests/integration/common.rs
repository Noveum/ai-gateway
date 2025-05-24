use reqwest::{Client, header::{HeaderMap, HeaderValue}};
use serde_json::{json, Value};
use std::env;
use std::time::Duration;
use tokio::time::sleep;
use dotenv::from_filename;
use uuid::Uuid;
use futures_util::StreamExt;
use reqwest::StatusCode;

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
    
    let es_url = env::var("ELASTICSEARCH_URL")
        .expect("ELASTICSEARCH_URL must be set in .env.test file");
    let es_username = env::var("ELASTICSEARCH_USERNAME")
        .expect("ELASTICSEARCH_USERNAME must be set in .env.test file");
    let es_password = env::var("ELASTICSEARCH_PASSWORD")
        .expect("ELASTICSEARCH_PASSWORD must be set in .env.test file");
    let es_index = env::var("ELASTICSEARCH_INDEX")
        .expect("ELASTICSEARCH_INDEX must be set in .env.test file");
    
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
    // Load environment variables from test config
    init_test_env();
    
    let mut headers = HeaderMap::new();
    
    // Different header setup based on provider
    match provider {
        "bedrock" => {
            // For Bedrock, we need AWS credentials
            let aws_access_key = env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID must be set");
            let aws_secret_key = env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY must be set");
            let aws_region = env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
            
            headers.insert("x-aws-access-key-id", HeaderValue::from_str(&aws_access_key).unwrap());
            headers.insert("x-aws-secret-access-key", HeaderValue::from_str(&aws_secret_key).unwrap());
            headers.insert("x-aws-region", HeaderValue::from_str(&aws_region).unwrap());
        },
        "azure-openai" => {
            // For Azure OpenAI, use api-key header instead of Authorization
            headers.insert("api-key", HeaderValue::from_str(api_key).unwrap());
            
            // Add Azure resource name - try from env var or use default
            let azure_resource_name = env::var("AZURE_OPENAI_RESOURCE_NAME")
                .or_else(|_| env::var("AZURE_RESOURCE_NAME"))
                .unwrap_or_else(|_| "ai-gateway-test".to_string());
            headers.insert("x-azure-resource-name", HeaderValue::from_str(&azure_resource_name).unwrap());
            
            // Add Azure API version if specified
            if let Ok(api_version) = env::var("AZURE_OPENAI_API_VERSION") {
                headers.insert("x-azure-api-version", HeaderValue::from_str(&api_version).unwrap());
            }
        },
        _ => {
            // For other providers, use Bearer token auth
            headers.insert("Authorization", HeaderValue::from_str(&format!("Bearer {}", api_key)).unwrap());
        }
    }
    
    // Common headers for all providers
    headers.insert("Content-Type", HeaderValue::from_str("application/json").unwrap());
    headers.insert("x-provider", HeaderValue::from_str(provider).unwrap());
    headers.insert("x-organisation-id", HeaderValue::from_str("TEST_ORG").unwrap());
    headers.insert("x-project-id", HeaderValue::from_str("TEST_PROJECT").unwrap());
    headers.insert("x-experiment-id", HeaderValue::from_str("TEST_EXPERIMENT").unwrap());
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
        Ok(_) => panic!("{} is set but empty. Please provide a valid value.", env_var_name),
        Err(_) => panic!("{} must be set either in .env.test file or as an environment variable.", env_var_name),
    }
}

/// Run a non-streaming test for a provider
pub async fn run_non_streaming_test(config: &ProviderTestConfig) {
    // Get the API key for the provider
    let api_key = get_api_key(&config.api_key_env_var);
    
    // Get gateway URL from environment or use default
    let gateway_url = env::var("GATEWAY_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    
    println!("Running non-streaming test for provider: {}", config.provider_name);
    
    // Generate a unique request ID for tracking
    let request_id = generate_request_id();
    
    // Setup headers
    let headers = setup_test_headers(&config.provider_name, &api_key, &request_id);
    
    // Print request ID for debugging
    println!("Request ID: {}", request_id);
    
    // Print the request body for debugging
    let request_body = create_test_request_body(config, false);
    println!("Request body: {}", serde_json::to_string_pretty(&request_body).unwrap_or_else(|_| "Failed to serialize".to_string()));
    
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
        let error_body = response.text().await.expect("Failed to read error response body");
        println!("Error response from gateway: {}", error_body);
        panic!("Request failed with status: 403 Forbidden - Make sure your AWS credentials have the correct permissions for AWS Bedrock");
    }
    
    // If we got a 400 Bad Request, print the response body to debug
    if response.status() == StatusCode::BAD_REQUEST {
        let error_body = response.text().await.expect("Failed to read error response body");
        println!("Error response from gateway (400): {}", error_body);
        panic!("Request failed with status: 400 Bad Request - Check API key and request format. Error: {}", error_body);
    }
    
    // Ensure the request was successful
    assert!(response.status().is_success(), 
            "Request failed with status: {} - Make sure the AI Gateway is running with ENABLE_ELASTICSEARCH=true", 
            response.status());
    
    // Extract gateway request ID from headers
    let response_headers = response.headers().clone();
    let gateway_request_id = response_headers.get("x-request-id")
        .expect("x-request-id header not found in response")
        .to_str()
        .expect("Invalid x-request-id header value");
    
    println!("Gateway request ID from headers: {}", gateway_request_id);
    
    // Get the response body
    let response_body = response.json::<Value>().await.expect("Failed to parse response as JSON");
    
    // Print response for debugging
    // println!("Response: {:#?}", response_body);
    
    // Validate response structure - still keep basic validation for immediate feedback
    assert!(response_body.get("choices").is_some(), "Response missing 'choices' field");
    assert!(response_body.get("usage").is_some(), "Response missing 'usage' field");
    
    // Extract token counts
    let usage = response_body.get("usage").unwrap();
    let prompt_tokens = usage.get("prompt_tokens").expect("Missing prompt_tokens").as_u64().unwrap();
    let completion_tokens = usage.get("completion_tokens").expect("Missing completion_tokens").as_u64().unwrap();
    let total_tokens = usage.get("total_tokens").expect("Missing total_tokens").as_u64().unwrap();
    
    // Validate token counts
    assert!(prompt_tokens > 0, "prompt_tokens should be greater than 0");
    assert!(completion_tokens > 0, "completion_tokens should be greater than 0");
    assert_eq!(prompt_tokens + completion_tokens, total_tokens, "Total tokens should equal prompt + completion tokens");
    
    // Wait for data to be indexed in ElasticSearch
    println!("Waiting for data to be indexed in ElasticSearch...");
    sleep(Duration::from_secs(3)).await;
    
    // Load environment variables (to make it clear in the logs)
    dotenv::from_filename(".env.test").ok();
    
    // Search ElasticSearch for the request using gateway request ID
    let es_response = search_elasticsearch(gateway_request_id).await.expect("Failed to search ElasticSearch");

    
    // Basic validation to fail early if something is obviously wrong
    let hits = es_response.get("hits").and_then(|h| h.get("hits")).expect("No hits in ElasticSearch response");
    let hits_array = hits.as_array().expect("Hits is not an array");
    assert!(!hits_array.is_empty(), "No matching documents found in ElasticSearch");
    
    // Print the ES response for debugging
    println!("ElasticSearch response summary:");
    println!("  - Total hits: {}", hits_array.len());
    if let Some(first_hit) = hits_array.first() {
        println!("  - First hit source keys: {:?}", 
            first_hit.get("_source").and_then(|s| s.as_object()).map(|o| o.keys().collect::<Vec<_>>()));
        
        // Print provider request ID from ES
        if let Some(provider_req_id) = first_hit.get("_source")
            .and_then(|s| s.get("attributes"))
            .and_then(|a| a.get("metadata"))
            .and_then(|m| m.get("provider_request_id"))
        {
            println!("  - Provider request ID from ES: {}", provider_req_id);
        }
        
        // Print gateway request ID from ES
        if let Some(gateway_req_id) = first_hit.get("_source")
            .and_then(|s| s.get("attributes"))
            .and_then(|a| a.get("metadata"))
            .and_then(|m| m.get("request_id"))
        {
            println!("  - Gateway request ID from ES: {}", gateway_req_id);
        }
    }
    
    // Use LLM to validate the test results
    let llm_validation_passed = validate_with_llm(
        &config.provider_name,
        &config.model,
        &request_id,
        &headers,
        &response_body,
        &es_response
    ).await;
    
    // Assert that the LLM validation passed
    assert!(llm_validation_passed, "LLM validation failed");
    
    println!("Non-streaming test completed successfully for provider: {}", config.provider_name);
}

/// Run a streaming test for a provider
pub async fn run_streaming_test(config: &ProviderTestConfig) {
    // Get the API key for the provider
    let api_key = get_api_key(&config.api_key_env_var);
    
    // Get gateway URL from environment or use default
    let gateway_url = env::var("GATEWAY_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());
    
    println!("Running streaming test for provider: {}", config.provider_name);
    
    // Generate a unique request ID for tracking
    let request_id = generate_request_id();
    
    // Setup headers
    let headers = setup_test_headers(&config.provider_name, &api_key, &request_id);
    
    // Print request ID for debugging
    println!("Request ID: {}", request_id);
    
    // Create request body with streaming enabled
    let request_body = create_test_request_body(config, true);
    
    // Print the request body for debugging
    println!("Request body: {}", serde_json::to_string_pretty(&request_body).unwrap_or_else(|_| "Failed to serialize".to_string()));
    
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
        let error_body = response.text().await.expect("Failed to read error response body");
        println!("Error response from gateway: {}", error_body);
        panic!("Request failed with status: 403 Forbidden - Make sure your AWS credentials have the correct permissions for AWS Bedrock");
    }
    
    // If we got a 400 Bad Request, print the response body to debug
    if response.status() == StatusCode::BAD_REQUEST {
        let error_body = response.text().await.expect("Failed to read error response body");
        println!("Error response from gateway (400): {}", error_body);
        panic!("Request failed with status: 400 Bad Request - Check API key and request format. Error: {}", error_body);
    }
    
    // Ensure the request was successful
    assert!(response.status().is_success(), 
            "Request failed with status: {} - Make sure the AI Gateway is running with ENABLE_ELASTICSEARCH=true", 
            response.status());
    
    // Extract gateway request ID from headers
    let response_headers = response.headers().clone();
    let gateway_request_id = response_headers.get("x-request-id")
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
                        provider_request_id = json.get("id")
                            .unwrap()
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        
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
    let es_response = search_elasticsearch(gateway_request_id).await.expect("Failed to search ElasticSearch");
    
    // Basic validation to fail early if something is obviously wrong
    let hits = es_response.get("hits").and_then(|h| h.get("hits")).expect("No hits in ElasticSearch response");
    let hits_array = hits.as_array().expect("Hits is not an array");
    assert!(!hits_array.is_empty(), "No matching documents found in ElasticSearch");
    
    // Print the ES response for debugging
    println!("ElasticSearch response summary:");
    println!("  - Total hits: {}", hits_array.len());
    if let Some(first_hit) = hits_array.first() {
        println!("  - First hit source keys: {:?}", 
            first_hit.get("_source").and_then(|s| s.as_object()).map(|o| o.keys().collect::<Vec<_>>()));
        
        // Print provider request ID from ES
        if let Some(provider_req_id) = first_hit.get("_source")
            .and_then(|s| s.get("attributes"))
            .and_then(|a| a.get("metadata"))
            .and_then(|m| m.get("provider_request_id"))
        {
            println!("  - Provider request ID from ES: {}", provider_req_id);
        }
        
        // Print gateway request ID from ES
        if let Some(gateway_req_id) = first_hit.get("_source")
            .and_then(|s| s.get("attributes"))
            .and_then(|a| a.get("metadata"))
            .and_then(|m| m.get("request_id"))
        {
            println!("  - Gateway request ID from ES: {}", gateway_req_id);
        }
    }
    
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
    } else if config.provider_name == "azure-openai" {
        // For Azure OpenAI, check if the final chunk contains usage information
        let mut response = serde_json::json!({
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
        });
        
        // Look for usage information in any of the final chunks
        if let Some(usage) = stream_data.iter().rev().find_map(|chunk| chunk.get("usage")) {
            response["usage"] = usage.clone();
        }
        // If no usage found in chunks, Azure OpenAI streaming typically omits usage like OpenAI
        
        response
    } else {
        // For other providers (Anthropic, Fireworks, GROQ, Together, etc.), 
        // they typically include usage in the final streaming chunk
        let mut response = serde_json::json!({
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
        });
        
        // Look for usage information in any of the chunks (usually in the final one)
        if let Some(usage) = stream_data.iter().rev().find_map(|chunk| chunk.get("usage")) {
            response["usage"] = usage.clone();
            println!("Found usage in streaming chunks for {}: {:?}", config.provider_name, usage);
        } else if config.provider_name == "groq" {
            // GROQ has a special structure where usage is in x_groq.usage
            if let Some(x_groq_usage) = stream_data.iter().rev().find_map(|chunk| 
                chunk.get("x_groq").and_then(|x_groq| x_groq.get("usage"))) {
                response["usage"] = x_groq_usage.clone();
                println!("Found usage in x_groq field for {}: {:?}", config.provider_name, x_groq_usage);
            } else {
                println!("No usage found in streaming chunks for provider: {}", config.provider_name);
                // For debugging - print the last few chunks to see what's available
                println!("Last chunk structure: {:#?}", last_chunk);
            }
        } else {
            println!("No usage found in streaming chunks for provider: {}", config.provider_name);
            // For debugging - print the last few chunks to see what's available
            println!("Last chunk structure: {:#?}", last_chunk);
        }
        
        response
    };
    
    // Use LLM to validate the test results
    let llm_validation_passed = validate_with_llm(
        &config.provider_name,
        &config.model,
        &request_id,
        &headers,
        &reconstructed_response,
        &es_response
    ).await;
    
    // Assert that the LLM validation passed
    assert!(llm_validation_passed, "LLM validation failed");
    
    println!("Streaming test completed successfully for provider: {}", config.provider_name);
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
    let gateway_request_id = request_headers.get("x-request-id")
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
        "As a LLM judge and validator, your task is to verify that metrics are getting accurately logged for this request.\n\n\
        IMPORTANT: The response format does NOT need to be exactly OpenAI-compatible. Different providers (GROQ, Fireworks, etc.) have their own response formats and this is acceptable.\n\n\
        Test Details:\n\
        Provider: {}\n\
        Request ID: {}\n\
        Request Headers: {}\n\
        Gateway Request ID: {}\n\
        Provider Response: {:#?}\n\n\
        ElasticSearch Logs: {:#?}\n\n\
        VALIDATION RULES:\n\
        1. Token counts: If the response contains usage tokens (prompt_tokens, completion_tokens, total_tokens), verify they match the ElasticSearch logs. Token field names may vary by provider (e.g., input_tokens vs prompt_tokens).\n\
        2. For STREAMING responses: Usage tokens may be missing from the final response object - this is normal and acceptable.\n\
        3. Tracking fields: These should be in the ElasticSearch logs, NOT in the response:\n\
           - request_id or gateway request ID\n\
           - organisation_id, org_id, or similar\n\
           - project_id\n\
           - experiment_id\n\
           - user_id\n\
        4. Response format: Different providers have different response structures (GROQ has x_groq, Fireworks has different fields). This is acceptable.\n\n\
        IGNORE THESE:\n\
        - provider_latency (can be zero)\n\
        - Model name mismatches (e.g., request used '{model_name}' but log shows different name)\n\
        - Provider-specific response format differences\n\n\
        Return ONLY a JSON object in this exact format:\n\
        {{\n  \"test_result\": \"pass\",\n  \"failed_fields\": []\n}}\n\
        where test_result is \"pass\" or \"fail\", and failed_fields contains specific field names that failed validation with reasons.",
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
            .map(|fields| fields.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(", "))
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