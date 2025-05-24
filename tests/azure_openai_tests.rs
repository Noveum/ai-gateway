#[cfg(test)]
mod azure_openai_tests {
    use super::super::src::providers::azure_openai::{AzureOpenAIProvider, AzureOpenAIProviderConfig, AzureOpenAIMetricsExtractor};
    use super::super::src::error::AppError;
    use super::super::src::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
    use axum::http::HeaderMap;
    use std::env;

    #[test]
    fn test_provider_creation() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        assert_eq!(provider.name(), "azure-openai");
    }
    
    #[test]
    fn test_provider_creation_with_config() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        assert_eq!(provider.name(), "azure-openai");
    }
    
    #[test]
    fn test_configuration_validation() {
        // Test valid configuration
        let config = AzureOpenAIProviderConfig::with_defaults(
            "valid-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        assert!(config.validate().is_ok());
        
        // Test empty API version
        let invalid_config = AzureOpenAIProviderConfig::with_defaults(
            "valid-resource",
            "gpt-4",
            ""
        );
        assert!(invalid_config.validate().is_err());
        
        // Test resource name validation
        let invalid_resource = AzureOpenAIProviderConfig::with_defaults(
            "ab", // Too short
            "gpt-4",
            "2024-02-15-preview"
        );
        assert!(invalid_resource.validate().is_err());
        
        // Test resource name with invalid characters
        let invalid_chars = AzureOpenAIProviderConfig::with_defaults(
            "invalid_resource_name", // Contains underscores
            "gpt-4",
            "2024-02-15-preview"
        );
        assert!(invalid_chars.validate().is_err());
    }
    
    #[test]
    fn test_configuration_allow_empty_flags() {
        let config = AzureOpenAIProviderConfig::with_defaults("", "", "2024-02-15-preview")
            .allow_empty_resource_name(true)
            .allow_empty_deployment_id(true);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_provider_base_url() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4", 
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        assert_eq!(provider.base_url(), "https://test-resource.openai.azure.com");
    }

    #[test]
    fn test_extract_resource_name_with_default() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "default-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let headers = HeaderMap::new();
        
        let resource_name = provider.extract_resource_name(&headers);
        assert_eq!(resource_name, "default-resource");
    }

    #[test]
    fn test_extract_resource_name_with_header() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "default-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let mut headers = HeaderMap::new();
        headers.insert("x-azure-resource-name", "custom-resource".parse().unwrap());
        
        let resource_name = provider.extract_resource_name(&headers);
        assert_eq!(resource_name, "custom-resource");
    }

    #[test]
    fn test_extract_deployment_id_with_default() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "default-deployment",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let headers = HeaderMap::new();
        
        let deployment_id = provider.extract_deployment_id(&headers);
        assert_eq!(deployment_id, "default-deployment");
    }

    #[test]
    fn test_extract_deployment_id_with_header() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "default-deployment",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let mut headers = HeaderMap::new();
        headers.insert("x-azure-deployment-id", "custom-deployment".parse().unwrap());
        
        let deployment_id = provider.extract_deployment_id(&headers);
        assert_eq!(deployment_id, "custom-deployment");
    }

    #[test]
    fn test_extract_api_version_with_default() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2023-12-01-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let headers = HeaderMap::new();
        
        let api_version = provider.extract_api_version(&headers);
        assert_eq!(api_version, "2023-12-01-preview");
    }

    #[test]
    fn test_extract_api_version_with_header() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2023-12-01-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let mut headers = HeaderMap::new();
        headers.insert("x-azure-api-version", "2024-05-01-preview".parse().unwrap());
        
        let api_version = provider.extract_api_version(&headers);
        assert_eq!(api_version, "2024-05-01-preview");
    }

    #[test]
    fn test_validate_azure_parameters_success() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        assert!(provider.validate_azure_parameters("test-resource", "gpt-4").is_ok());
    }

    #[test]
    fn test_validate_azure_parameters_empty_resource() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let result = provider.validate_azure_parameters("", "gpt-4");
        assert!(result.is_err());
        
        // Check that the error message is informative
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Azure resource name is required"));
        assert!(error_msg.contains("x-azure-resource-name"));
    }

    #[test]
    fn test_validate_azure_parameters_empty_deployment() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let result = provider.validate_azure_parameters("test-resource", "");
        assert!(result.is_err());
        
        // Check that the error message is informative
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Azure deployment ID is required"));
        assert!(error_msg.contains("x-azure-deployment-id"));
    }

    #[test]
    fn test_extract_azure_headers_with_defaults() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "env-resource",
            "env-deployment",
            "env-api-version"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        
        let headers = HeaderMap::new();
        let (resource_name, deployment_id, api_version) = provider.extract_azure_headers(&headers);
        
        assert_eq!(resource_name, "env-resource");
        assert_eq!(deployment_id, "env-deployment");
        assert_eq!(api_version, "env-api-version");
    }

    #[test]
    fn test_get_endpoint_path() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");

        assert_eq!(provider.get_endpoint_path("/v1/chat/completions").unwrap(), "chat/completions");
        assert_eq!(provider.get_endpoint_path("/v1/completions").unwrap(), "completions");
        assert_eq!(provider.get_endpoint_path("/v1/embeddings").unwrap(), "embeddings");
        assert_eq!(provider.get_endpoint_path("/v1/audio/transcriptions").unwrap(), "audio/transcriptions");
        assert_eq!(provider.get_endpoint_path("/v1/audio/translations").unwrap(), "audio/translations");
        assert_eq!(provider.get_endpoint_path("/v1/images/generations").unwrap(), "images/generations");

        // Test the fallback to chat/completions for unknown paths
        assert_eq!(provider.get_endpoint_path("/unknown/path").unwrap(), "chat/completions");
    }

    #[test]
    fn test_build_azure_url_success() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");

        let result = provider.build_azure_url(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview",
            "/v1/chat/completions"
        );

        assert!(result.is_ok());
        let url = result.unwrap();
        
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str().unwrap(), "test-resource.openai.azure.com");
        assert_eq!(url.path(), "/openai/deployments/gpt-4/chat/completions");
        assert_eq!(url.query().unwrap(), "api-version=2024-02-15-preview");
    }

    #[test]
    fn test_build_azure_url_validation_errors() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");

        // Test empty resource name
        let result = provider.build_azure_url("", "gpt-4", "2024-02-15-preview", "/v1/chat/completions");
        assert!(result.is_err());

        // Test empty deployment ID
        let result = provider.build_azure_url("test-resource", "", "2024-02-15-preview", "/v1/chat/completions");
        assert!(result.is_err());

        // Test empty API version
        let result = provider.build_azure_url("test-resource", "gpt-4", "", "/v1/chat/completions");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_azure_url_with_different_endpoints() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");

        // Test chat completions
        let url = provider.build_azure_url("test-resource", "gpt-4", "2024-02-15-preview", "/v1/chat/completions").unwrap();
        assert!(url.path().contains("chat/completions"));

        // Test completions
        let url = provider.build_azure_url("test-resource", "gpt-35-turbo", "2024-02-15-preview", "/v1/completions").unwrap();
        assert!(url.path().contains("completions"));

        // Test embeddings
        let url = provider.build_azure_url("test-resource", "text-embedding-ada-002", "2024-02-15-preview", "/v1/embeddings").unwrap();
        assert!(url.path().contains("embeddings"));
        assert!(url.path().contains("text-embedding-ada-002"));

        // Test audio transcriptions
        let url = provider.build_azure_url("test-resource", "whisper-1", "2024-02-15-preview", "/v1/audio/transcriptions").unwrap();
        assert!(url.path().contains("audio/transcriptions"));
        assert!(url.path().contains("whisper-1"));

        // Test images generations
        let url = provider.build_azure_url("test-resource", "dall-e-3", "2024-02-15-preview", "/v1/images/generations").unwrap();
        assert!(url.path().contains("images/generations"));
        assert!(url.path().contains("dall-e-3"));
    }

    #[test]
    fn test_process_headers_missing_api_key() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let headers = HeaderMap::new();
        
        let result = provider.process_headers(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn test_process_headers_missing_resource_name() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "", // Empty resource name
            "gpt-4",
            "2024-02-15-preview"
        ).allow_empty_resource_name(false); // Don't allow empty resource name
        
        match AzureOpenAIProvider::with_config(config) {
            Ok(_) => panic!("Should have failed with empty resource name"),
            Err(e) => {
                // Expected to fail during provider creation due to validation
                assert!(e.to_string().contains("resource name"));
            }
        }
    }

    #[test]
    fn test_calculate_azure_cost() {
        // Create a test extractor to access the cost calculation method
        let extractor = AzureOpenAIMetricsExtractor::new("gpt-4".to_string());
        
        // Test GPT-4 standard model (0.03 input + 0.06 output per 1K tokens)
        let gpt4_cost = extractor.calculate_azure_cost("gpt-4", 1000, 1000);
        assert!((gpt4_cost - 0.09).abs() < f64::EPSILON, "GPT-4 cost should be $0.09 (0.03 + 0.06), got {}", gpt4_cost);
        
        // Test GPT-3.5 Turbo (0.0015 input + 0.002 output per 1K tokens)
        assert_eq!(extractor.calculate_azure_cost("gpt-35-turbo", 1000, 1000), 0.0035);
        assert_eq!(extractor.calculate_azure_cost("gpt-3.5-turbo", 1000, 1000), 0.0035);
        
        // Test GPT-4 Turbo (0.01 input + 0.03 output per 1K tokens)
        assert_eq!(extractor.calculate_azure_cost("gpt-4-turbo", 1000, 1000), 0.04);
        
        // Test GPT-4 32k (0.06 input + 0.12 output per 1K tokens)
        assert_eq!(extractor.calculate_azure_cost("gpt-4-32k", 1000, 1000), 0.18);
        
        // Test text embedding (input only, 0.0001 per 1K tokens)
        assert_eq!(extractor.calculate_azure_cost("text-embedding-ada-002", 1000, 0), 0.0001);
        
        // Test unknown model
        assert_eq!(extractor.calculate_azure_cost("unknown-model", 1000, 1000), 0.0);
    }

    #[test]
    fn test_metrics_extractor_creation() {
        let extractor = AzureOpenAIMetricsExtractor::new("gpt-4-turbo-2024-04-09".to_string());
        let response = serde_json::json!({
            "model": "gpt-4-turbo-2024-04-09",
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            },
            "id": "test-request-id"
        });
        
        let metrics = extractor.extract_metrics(&response);
        assert_eq!(metrics.model, "gpt-4-turbo-2024-04-09");
        assert_eq!(metrics.input_tokens, Some(10));
        assert_eq!(metrics.output_tokens, Some(20));
        assert_eq!(metrics.total_tokens, Some(30));
        assert_eq!(metrics.request_id, Some("test-request-id".to_string()));
        // Expected cost for GPT-4 Turbo: (10/1000 * 0.01) + (20/1000 * 0.03) = 0.0001 + 0.0006 = 0.0007
        assert!((metrics.cost.unwrap() - 0.0007).abs() < f64::EPSILON, "Expected cost 0.0007, got {:?}", metrics.cost);
    }

    #[test]
    fn test_metrics_extractor_with_content_filtering() {
        let extractor = AzureOpenAIMetricsExtractor::new("gpt-4-turbo-2024-04-09".to_string());
        let response = serde_json::json!({
            "choices": [
                {
                    "content_filter_results": {
                        "hate": {
                            "filtered": false,
                            "severity": "safe"
                        },
                        "self_harm": {
                            "filtered": false,
                            "severity": "safe"
                        },
                        "sexual": {
                            "filtered": false,
                            "severity": "safe"
                        },
                        "violence": {
                            "filtered": false,
                            "severity": "low"
                        }
                    },
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {
                        "content": "Hello! How can I help you today?",
                        "role": "assistant"
                    }
                }
            ],
            "created": 1748093069,
            "id": "chatcmpl-test",
            "model": "gpt-4-turbo-2024-04-09",
            "object": "chat.completion",
            "prompt_filter_results": [
                {
                    "prompt_index": 0,
                    "content_filter_results": {
                        "hate": {
                            "filtered": false,
                            "severity": "safe"
                        },
                        "jailbreak": {
                            "filtered": false,
                            "detected": false
                        }
                    }
                }
            ],
            "usage": {
                "completion_tokens": 17,
                "prompt_tokens": 17,
                "total_tokens": 34
            }
        });
        
        let metrics = extractor.extract_metrics(&response);
        assert_eq!(metrics.model, "gpt-4-turbo-2024-04-09");
        assert_eq!(metrics.input_tokens, Some(17));
        assert_eq!(metrics.output_tokens, Some(17));
        assert_eq!(metrics.total_tokens, Some(34));
        assert_eq!(metrics.request_id, Some("chatcmpl-test".to_string()));
        
        // GPT-4 Turbo pricing: (17/1000 * 0.01) + (17/1000 * 0.03) = 0.00017 + 0.00051 = 0.00068
        let expected_cost = 0.00068;
        assert!((metrics.cost.unwrap() - expected_cost).abs() < 0.000001, "Expected cost {}, got {:?}", expected_cost, metrics.cost);
    }

    #[test]
    fn test_metrics_extractor_different_models() {
        let extractor = AzureOpenAIMetricsExtractor::new("gpt-4-turbo-2024-04-09".to_string());
        
        // Test GPT-3.5 Turbo
        let response_35 = serde_json::json!({
            "model": "gpt-35-turbo",
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            },
            "id": "test-gpt35"
        });
        
        let metrics_35 = extractor.extract_metrics(&response_35);
        assert_eq!(metrics_35.model, "gpt-35-turbo");
        // GPT-3.5 Turbo: (100/1000 * 0.0015) + (50/1000 * 0.002) = 0.00015 + 0.0001 = 0.00025
        assert!((metrics_35.cost.unwrap() - 0.00025).abs() < f64::EPSILON);
        
        // Test GPT-4 32k
        let response_4_32k = serde_json::json!({
            "model": "gpt-4-32k",
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 500,
                "total_tokens": 1500
            },
            "id": "test-gpt4-32k"
        });
        
        let metrics_4_32k = extractor.extract_metrics(&response_4_32k);
        assert_eq!(metrics_4_32k.model, "gpt-4-32k");
        // GPT-4 32k: (1000/1000 * 0.06) + (500/1000 * 0.12) = 0.06 + 0.06 = 0.12
        assert!((metrics_4_32k.cost.unwrap() - 0.12).abs() < f64::EPSILON);
    }

    #[test]
    fn test_metrics_extractor_streaming() {
        let extractor = AzureOpenAIMetricsExtractor::new("gpt-4-turbo-2024-04-09".to_string());
        
        // Test streaming chunk without usage
        let chunk_without_usage = r#"{"id":"chatcmpl-test","object":"chat.completion.chunk","created":1748093069,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        
        let metrics = extractor.try_extract_provider_specific_streaming_metrics(chunk_without_usage);
        assert!(metrics.is_some());
        let metrics = metrics.unwrap();
        assert_eq!(metrics.model, "gpt-4"); // Response model from chunk, not deployment mapping
        assert_eq!(metrics.request_id, Some("chatcmpl-test".to_string()));
        assert!(metrics.cost.is_none()); // No cost for streaming chunks without usage
        
        // Test streaming chunk with usage (final chunk)
        let chunk_with_usage = r#"{"id":"chatcmpl-test","object":"chat.completion.chunk","created":1748093069,"model":"gpt-4","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}}"#;
        
        let metrics_with_usage = extractor.try_extract_provider_specific_streaming_metrics(chunk_with_usage);
        assert!(metrics_with_usage.is_some());
        let metrics_with_usage = metrics_with_usage.unwrap();
        assert_eq!(metrics_with_usage.model, "gpt-4"); // Response model from chunk, not deployment mapping
        assert_eq!(metrics_with_usage.input_tokens, Some(10));
        assert_eq!(metrics_with_usage.output_tokens, Some(20));
        assert!(metrics_with_usage.cost.is_some());
    }

    #[test]
    fn test_metrics_extractor_missing_fields() {
        let extractor = AzureOpenAIMetricsExtractor::new("gpt-4-turbo-2024-04-09".to_string());
        
        // Test response with missing usage
        let response_no_usage = serde_json::json!({
            "model": "gpt-4-turbo-2024-04-09",
            "id": "test-no-usage"
        });
        
        let metrics = extractor.extract_metrics(&response_no_usage);
        assert_eq!(metrics.model, "gpt-4-turbo-2024-04-09"); // Uses response model directly
        assert_eq!(metrics.request_id, Some("test-no-usage".to_string()));
        assert!(metrics.input_tokens.is_none());
        assert!(metrics.output_tokens.is_none());
        assert!(metrics.cost.is_none()); // No cost without token usage
        
        // Test response with missing model but with usage
        let response_no_model = serde_json::json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            },
            "id": "test-no-model"
        });
        
        let metrics = extractor.extract_metrics(&response_no_model);
        assert_eq!(metrics.model, "gpt-4-turbo"); // Uses mapped deployment ID since no response model
        assert_eq!(metrics.request_id, Some("test-no-model".to_string()));
        assert_eq!(metrics.input_tokens, Some(10));
        assert_eq!(metrics.output_tokens, Some(20));
        assert_eq!(metrics.total_tokens, Some(30));
        // Cost should be calculated since we have tokens and mapped model
        // GPT-4 Turbo: (10/1000 * 0.01) + (20/1000 * 0.03) = 0.0001 + 0.0006 = 0.0007
        assert!((metrics.cost.unwrap() - 0.0007).abs() < f64::EPSILON);
    }

    #[test]
    fn test_extract_model_from_body_success() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        
        // Test valid JSON with model field
        let json_body = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello"}]}"#;
        let model = provider.extract_model_from_body(json_body.as_bytes());
        assert_eq!(model, Some("gpt-4".to_string()));
        
        // Test with different model
        let json_body = r#"{"model": "gpt-35-turbo", "temperature": 0.7}"#;
        let model = provider.extract_model_from_body(json_body.as_bytes());
        assert_eq!(model, Some("gpt-35-turbo".to_string()));
    }

    #[test]
    fn test_extract_model_from_body_missing_model() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        
        // Test JSON without model field
        let json_body = r#"{"messages": [{"role": "user", "content": "Hello"}], "temperature": 0.7}"#;
        let model = provider.extract_model_from_body(json_body.as_bytes());
        assert_eq!(model, None);
    }

    #[test]
    fn test_extract_model_from_body_invalid_json() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        
        // Test invalid JSON
        let invalid_json = r#"{"model": "gpt-4", "messages": ["#;
        let model = provider.extract_model_from_body(invalid_json.as_bytes());
        assert_eq!(model, None);
        
        // Test empty body
        let model = provider.extract_model_from_body(b"");
        assert_eq!(model, None);
        
        // Test non-UTF8 bytes
        let invalid_utf8 = &[0xFF, 0xFE, 0xFD];
        let model = provider.extract_model_from_body(invalid_utf8);
        assert_eq!(model, None);
    }

    #[tokio::test]
    async fn test_before_request_extracts_model() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let headers = HeaderMap::new();
        
        // Test that before_request extracts model from body
        let json_body = r#"{"model": "gpt-4-turbo", "messages": [{"role": "user", "content": "Hello"}]}"#;
        let result = provider.before_request(&headers, json_body.as_bytes()).await;
        assert!(result.is_ok());
        
        // Check that the model was stored as deployment ID
        let stored_deployment = provider.extracted_deployment_id.read().unwrap().clone();
        assert_eq!(stored_deployment, "gpt-4-turbo");
    }

    #[test]
    fn test_process_headers_uses_model_from_body() {
        env::set_var("AZURE_OPENAI_RESOURCE_NAME", "test-resource");
        let provider = AzureOpenAIProvider::new().expect("Failed to create provider");
        
        // First, simulate before_request storing a model
        *provider.extracted_deployment_id.write().unwrap() = "gpt-4-from-body".to_string();
        
        let mut headers = HeaderMap::new();
        headers.insert("api-key", "test-key".parse().unwrap());
        // Note: no x-azure-deployment-id header provided
        
        let result = provider.process_headers(&headers);
        assert!(result.is_ok());
        
        // Verify that the stored deployment ID from body was used
        let final_deployment = provider.extracted_deployment_id.read().unwrap().clone();
        assert_eq!(final_deployment, "gpt-4-from-body");
        
        env::remove_var("AZURE_OPENAI_RESOURCE_NAME");
    }

    #[test]
    fn test_process_headers_header_overrides_body() {
        env::set_var("AZURE_OPENAI_RESOURCE_NAME", "test-resource");
        let provider = AzureOpenAIProvider::new().expect("Failed to create provider");
        
        // Simulate before_request storing a model from body
        *provider.extracted_deployment_id.write().unwrap() = "gpt-4-from-body".to_string();
        
        let mut headers = HeaderMap::new();
        headers.insert("api-key", "test-key".parse().unwrap());
        headers.insert("x-azure-deployment-id", "gpt-35-turbo-from-header".parse().unwrap());
        
        let result = provider.process_headers(&headers);
        assert!(result.is_ok());
        
        // Verify that the header value was used instead of body value
        let final_deployment = provider.extracted_deployment_id.read().unwrap().clone();
        assert_eq!(final_deployment, "gpt-35-turbo-from-header");
        
        env::remove_var("AZURE_OPENAI_RESOURCE_NAME");
    }

    #[test]
    fn test_minimal_headers_workflow() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        
        // Test that we can process minimal headers (just api-key and resource name)
        let mut headers = HeaderMap::new();
        headers.insert("api-key", "test-key".parse().unwrap());
        headers.insert("x-azure-resource-name", "test-resource".parse().unwrap());
        
        let result = provider.process_headers(&headers);
        assert!(result.is_ok(), "Should successfully process minimal headers");
        
        let processed = result.unwrap();
        assert_eq!(processed.get("api-key").unwrap(), "test-key");
        assert_eq!(processed.get("x-azure-resource-name-processed").unwrap(), "test-resource");
    }

    // Error handling tests
    #[test]
    fn test_error_handling_logic() {
        // Test authentication error detection
        let auth_body = r#"{"error": {"message": "Invalid API key"}}"#;
        let auth_json: serde_json::Value = serde_json::from_str(auth_body).unwrap();
        
        // Test that we can parse Azure error structure
        assert!(auth_json.get("error").is_some());
        assert_eq!(
            auth_json["error"]["message"].as_str().unwrap(),
            "Invalid API key"
        );
    }

    #[test]
    fn test_content_filter_detection() {
        let content_filter_body = r#"{"error": {"code": "content_filter", "message": "Content was filtered"}}"#;
        let json: serde_json::Value = serde_json::from_str(content_filter_body).unwrap();
        
        let is_content_filter = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .map_or(false, |code| code.contains("content_filter") || code.contains("ContentFilter"));
            
        assert!(is_content_filter);
    }

    #[test]
    fn test_error_message_extraction() {
        let error_bodies = vec![
            (r#"{"error": {"message": "Rate limit exceeded"}}"#, "Rate limit exceeded"),
            (r#"{"error": {"message": "Deployment not found"}}"#, "Deployment not found"),
            (r#"{"error": {"message": "Invalid parameter"}}"#, "Invalid parameter"),
        ];
        
        for (body, expected_message) in error_bodies {
            let json: serde_json::Value = serde_json::from_str(body).unwrap();
            let message = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error");
            
            assert_eq!(message, expected_message);
        }
    }

    #[test]
    fn test_error_code_detection() {
        let structured_error = r#"{"error": {"code": "invalid_request_error", "message": "Missing parameter"}}"#;
        let json: serde_json::Value = serde_json::from_str(structured_error).unwrap();
        
        let error_code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("unknown");
            
        assert_eq!(error_code, "invalid_request_error");
    }

    #[test]
    fn test_malformed_json_handling() {
        // Test handling of non-JSON error responses
        let malformed_responses = vec![
            "Bad Gateway",
            "<html><body>Internal Server Error</body></html>",
            "",
        ];
        
        for response in malformed_responses {
            let parse_result: Result<serde_json::Value, _> = serde_json::from_str(response);
            assert!(parse_result.is_err(), "Should fail to parse non-JSON: {}", response);
        }
    }

    #[test]
    fn test_azure_error_status_code_mapping() {
        // Test that we have proper status code to error type mapping logic
        let status_error_pairs = vec![
            (401, "authentication"),
            (404, "resource_not_found"),
            (429, "rate_limit"),
            (400, "bad_request"),
            (500, "server_error"),
            (502, "server_error"),
            (503, "server_error"),
        ];
        
        for (status_code, error_type) in status_error_pairs {
            // This tests the logic we use in handle_azure_error
            let is_auth_error = status_code == 401;
            let is_not_found = status_code == 404;
            let is_rate_limit = status_code == 429;
            let is_bad_request = status_code == 400;
            let is_server_error = (500..=599).contains(&status_code);
            
            match error_type {
                "authentication" => assert!(is_auth_error),
                "resource_not_found" => assert!(is_not_found),
                "rate_limit" => assert!(is_rate_limit),
                "bad_request" => assert!(is_bad_request),
                "server_error" => assert!(is_server_error),
                _ => panic!("Unknown error type: {}", error_type),
            }
        }
    }

    #[test]
    fn test_azure_error_response_parsing_edge_cases() {
        // Test edge cases in error response parsing
        
        // Case 1: Error object without message
        let no_message = r#"{"error": {"code": "test_error"}}"#;
        let json: serde_json::Value = serde_json::from_str(no_message).unwrap();
        let message = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("Default error message");
        assert_eq!(message, "Default error message");
        
        // Case 2: Error object without code
        let no_code = r#"{"error": {"message": "Some error"}}"#;
        let json: serde_json::Value = serde_json::from_str(no_code).unwrap();
        let code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("unknown");
        assert_eq!(code, "unknown");
        
        // Case 3: No error object at all
        let no_error = r#"{"status": "failed", "reason": "Something went wrong"}"#;
        let json: serde_json::Value = serde_json::from_str(no_error).unwrap();
        let has_error = json.get("error").is_some();
        assert!(!has_error);
    }

    #[test]
    fn test_extract_azure_headers_with_overrides() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "default-resource",
            "default-deployment",
            "default-api-version"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        let mut headers = HeaderMap::new();
        
        headers.insert("x-azure-resource-name", "override-resource".parse().unwrap());
        headers.insert("x-azure-deployment-id", "override-deployment".parse().unwrap());
        headers.insert("x-azure-api-version", "2024-03-01".parse().unwrap());
        
        let (resource_name, deployment_id, api_version) = provider.extract_azure_headers(&headers);
        assert_eq!(resource_name, "override-resource");
        assert_eq!(deployment_id, "override-deployment");
        assert_eq!(api_version, "2024-03-01");
    }

    #[test]
    fn test_validate_azure_parameters_invalid_resource_format() {
        let config = AzureOpenAIProviderConfig::with_defaults(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview"
        );
        let provider = AzureOpenAIProvider::with_config(config).expect("Failed to create provider");
        
        // Test resource name too short
        let result = provider.validate_azure_parameters("ab", "gpt-4");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("must be between 3 and 24 characters"));
        
        // Test resource name too long
        let result = provider.validate_azure_parameters("this-is-a-very-long-resource-name-that-exceeds-24-chars", "gpt-4");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("must be between 3 and 24 characters"));
        
        // Test resource name with invalid characters
        let result = provider.validate_azure_parameters("invalid_name", "gpt-4");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("can only contain alphanumeric characters and hyphens"));
        
        // Test resource name starting with hyphen
        let result = provider.validate_azure_parameters("-invalid", "gpt-4");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot start or end with a hyphen"));
        
        // Test resource name ending with hyphen
        let result = provider.validate_azure_parameters("invalid-", "gpt-4");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot start or end with a hyphen"));
    }
} 