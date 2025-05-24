use super::Provider;
use super::utils::log_tracking_headers;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use reqwest::Url;
use serde_json::Value;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tracing::{debug, error};

/// Azure OpenAI provider implementation
/// 
/// Handles Azure-hosted OpenAI models with enterprise-grade security and compliance.
/// Key differences from standard OpenAI:
/// - Uses api-key header instead of Authorization bearer token
/// - URL structure includes resource name and deployment ID
/// - Requires API version parameter
/// - Responses include Azure-specific content filtering results
pub struct AzureOpenAIProvider {
    /// Default Azure resource name (e.g., "my-openai-resource")
    default_resource_name: String,
    /// Default deployment ID (e.g., "gpt-4", "gpt-35-turbo")
    default_deployment_id: String,
    /// Default API version (e.g., "2024-02-15-preview")
    default_api_version: String,
    /// Constructed Azure URL (stored after processing headers)
    constructed_url: Arc<RwLock<Option<String>>>,
    /// Current path being processed
    current_path: Arc<RwLock<String>>,
    /// Extracted Azure resource name from headers/defaults
    extracted_resource_name: Arc<RwLock<String>>,
    /// Extracted Azure deployment ID from headers/defaults
    extracted_deployment_id: Arc<RwLock<String>>,
    /// Extracted Azure API version from headers/defaults
    extracted_api_version: Arc<RwLock<String>>,
}

impl AzureOpenAIProvider {
    /// Create a new Azure OpenAI provider instance
    ///
    /// Reads configuration from environment variables with sensible defaults:
    /// - AZURE_OPENAI_RESOURCE_NAME: The Azure resource name
    /// - AZURE_OPENAI_DEPLOYMENT_ID: The deployment identifier
    /// - AZURE_OPENAI_API_VERSION: The API version (defaults to "2024-02-15-preview")
    pub fn new() -> Self {
        Self {
            default_resource_name: std::env::var("AZURE_OPENAI_RESOURCE_NAME")
                .unwrap_or_else(|_| "".to_string()),
            default_deployment_id: std::env::var("AZURE_OPENAI_DEPLOYMENT_ID")
                .unwrap_or_else(|_| "gpt-4".to_string()),
            default_api_version: std::env::var("AZURE_OPENAI_API_VERSION")
                .unwrap_or_else(|_| "2024-02-15-preview".to_string()),
            constructed_url: Arc::new(RwLock::new(None)),
            current_path: Arc::new(RwLock::new("".to_string())),
            extracted_resource_name: Arc::new(RwLock::new("".to_string())),
            extracted_deployment_id: Arc::new(RwLock::new("".to_string())),
            extracted_api_version: Arc::new(RwLock::new("".to_string())),
        }
    }

    /// Extract Azure resource name from headers with fallback to default
    ///
    /// Looks for 'x-azure-resource-name' header first, then falls back to the configured default
    fn extract_resource_name(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-azure-resource-name")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.default_resource_name.clone())
    }

    /// Extract Azure deployment ID from headers with fallback to default
    ///
    /// Looks for 'x-azure-deployment-id' header first, then falls back to the configured default
    fn extract_deployment_id(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-azure-deployment-id")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.default_deployment_id.clone())
    }

    /// Extract Azure API version from headers with fallback to default
    ///
    /// Looks for 'x-azure-api-version' header first, then falls back to the configured default
    fn extract_api_version(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-azure-api-version")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.default_api_version.clone())
    }

    /// Validate Azure-specific parameters
    ///
    /// Ensures that required Azure parameters are not empty and meet basic requirements
    fn validate_azure_parameters(&self, resource_name: &str, deployment_id: &str) -> Result<(), AppError> {
        if resource_name.is_empty() {
            error!("Azure resource name is required but not provided");
            return Err(AppError::RequestError(
                "Azure resource name is required. Set AZURE_OPENAI_RESOURCE_NAME environment variable or provide x-azure-resource-name header.".to_string()
            ));
        }

        if deployment_id.is_empty() {
            error!("Azure deployment ID is required but not provided");
            return Err(AppError::RequestError(
                "Azure deployment ID is required. Set AZURE_OPENAI_DEPLOYMENT_ID environment variable or provide x-azure-deployment-id header.".to_string()
            ));
        }

        // Additional validation could be added here (e.g., format validation)
        debug!("Azure parameters validated successfully: resource={}, deployment={}", resource_name, deployment_id);
        Ok(())
    }

    /// Extract Azure-specific headers from the request
    ///
    /// Returns a tuple containing (resource_name, deployment_id, api_version)
    /// Uses the individual extraction methods for better modularity
    fn extract_azure_headers(&self, headers: &HeaderMap) -> (String, String, String) {
        let resource_name = self.extract_resource_name(headers);
        let deployment_id = self.extract_deployment_id(headers);
        let api_version = self.extract_api_version(headers);

        debug!(
            "Using Azure configuration: resource={}, deployment={}, api_version={}",
            resource_name, deployment_id, api_version
        );

        (resource_name, deployment_id, api_version)
    }

    /// Map OpenAI-style paths to Azure OpenAI endpoints
    ///
    /// Converts standard OpenAI API paths to Azure OpenAI deployment-specific endpoints
    fn get_endpoint_path(&self, path: &str) -> Result<&'static str, AppError> {
        if path.contains("chat/completions") || path.ends_with("chat/completions") {
            Ok("chat/completions")
        } else if path.contains("completions") || path.ends_with("completions") {
            Ok("completions")
        } else if path.contains("embeddings") || path.ends_with("embeddings") {
            Ok("embeddings")
        } else if path.contains("audio/transcriptions") || path.ends_with("audio/transcriptions") {
            Ok("audio/transcriptions")
        } else if path.contains("audio/translations") || path.ends_with("audio/translations") {
            Ok("audio/translations")
        } else if path.contains("images/generations") || path.ends_with("images/generations") {
            Ok("images/generations")
        } else {
            // Default to chat/completions for Azure OpenAI (most common use case)
            debug!("Unknown path '{}', defaulting to chat/completions", path);
            Ok("chat/completions")
        }
    }

    /// Construct Azure OpenAI API URL using reqwest::Url for proper validation
    ///
    /// Format: https://{resource}.openai.azure.com/openai/deployments/{deployment}/{endpoint}?api-version={version}
    fn build_azure_url(&self, resource_name: &str, deployment_id: &str, api_version: &str, path: &str) -> Result<Url, AppError> {
        // Validate input parameters
        if resource_name.is_empty() {
            return Err(AppError::RequestError("Resource name cannot be empty".to_string()));
        }
        if deployment_id.is_empty() {
            return Err(AppError::RequestError("Deployment ID cannot be empty".to_string()));
        }
        if api_version.is_empty() {
            return Err(AppError::RequestError("API version cannot be empty".to_string()));
        }

        // Get the appropriate endpoint for the given path
        let endpoint = self.get_endpoint_path(path)?;
        
        // Construct base URL
        let base_url = format!("https://{}.openai.azure.com", resource_name);
        let mut url = Url::parse(&base_url)
            .map_err(|e| {
                error!("Failed to parse Azure base URL '{}': {}", base_url, e);
                AppError::RequestError(format!("Invalid Azure resource name: {}", resource_name))
            })?;

        // Set the path
        let full_path = format!("/openai/deployments/{}/{}", deployment_id, endpoint);
        url.set_path(&full_path);

        // Add API version query parameter
        url.query_pairs_mut()
            .append_pair("api-version", api_version);

        debug!("Constructed Azure OpenAI URL: {}", url);
        Ok(url)
    }

    /// Extract model name from request body JSON
    /// 
    /// Azure OpenAI uses the model name from the request body as the deployment name
    /// This allows for a cleaner API where users only specify the model they want
    fn extract_model_from_body(&self, body_bytes: &[u8]) -> Option<String> {
        // Parse the JSON body to extract the model field
        if let Ok(body_str) = std::str::from_utf8(body_bytes) {
            if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(body_str) {
                if let Some(model) = json_value.get("model").and_then(|m| m.as_str()) {
                    debug!("Extracted model from request body: {}", model);
                    return Some(model.to_string());
                }
            }
        }
        debug!("Could not extract model from request body");
        None
    }

    /// Store the extracted model as the deployment ID for URL construction
    /// This is called during before_request to capture the model from the body
    async fn store_model_as_deployment(&self, body_bytes: &[u8]) {
        if let Some(model) = self.extract_model_from_body(body_bytes) {
            // Store the model as the deployment ID
            *self.extracted_deployment_id.write().unwrap() = model.clone();
            debug!("Using model '{}' as Azure deployment ID", model);
        }
    }

    async fn before_request(&self, _headers: &HeaderMap, body_bytes: &[u8]) -> Result<(), AppError> {
        // Extract model from request body and use it as deployment ID
        // This allows for cleaner API where users only specify the model they want
        self.store_model_as_deployment(body_bytes).await;
        Ok(())
    }
}

#[async_trait]
impl Provider for AzureOpenAIProvider {
    fn base_url(&self) -> String {
        // Return the constructed Azure URL if available
        if let Some(url) = self.constructed_url.read().unwrap().as_ref() {
            debug!("Using constructed Azure URL: {}", url);
            return url.clone();
        }
        
        // Fallback to a basic URL (this should only happen if headers haven't been processed yet)
        let fallback = format!("https://{}.openai.azure.com", self.default_resource_name);
        debug!("Using fallback Azure URL: {}", fallback);
        fallback
    }

    fn name(&self) -> &str {
        "azure-openai"
    }

    fn transform_path(&self, path: &str) -> String {
        // Store the current path for later use
        *self.current_path.write().unwrap() = path.to_string();
        
        // Try to construct the Azure URL now that we have the path
        // We should have Azure configuration from process_headers by now
        if let Ok(constructed_url_guard) = self.constructed_url.read() {
            if constructed_url_guard.is_none() {
                // URL hasn't been constructed yet, try to build it now
                drop(constructed_url_guard); // Release read lock
                
                // Get the Azure configuration that was stored during process_headers
                let resource_name = self.extracted_resource_name.read().unwrap().clone();
                let deployment_id = self.extracted_deployment_id.read().unwrap().clone();
                let api_version = self.extracted_api_version.read().unwrap().clone();
                
                debug!("Using stored Azure configuration in transform_path: resource={}, deployment={}, path={}", 
                    resource_name, deployment_id, path);
                
                if resource_name.is_empty() {
                    error!("No Azure resource name available in transform_path");
                    return format!("ERROR_NO_AZURE_RESOURCE_NAME_{}", path);
                }
                
                if let Ok(url) = self.build_azure_url(&resource_name, &deployment_id, &api_version, path) {
                    let url_string = url.to_string();
                    debug!("Successfully constructed Azure URL in transform_path: {}", url_string);
                    *self.constructed_url.write().unwrap() = Some(url_string);
                } else {
                    error!("Failed to construct Azure URL in transform_path");
                    return format!("ERROR_INVALID_AZURE_CONFIG_{}", path);
                }
            }
        }
        
        // For Azure, we need to return an empty string since the full URL 
        // is already constructed and stored in base_url()
        debug!("Azure URL construction handled, returning empty path");
        "".to_string()
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Azure OpenAI request headers");
        let mut headers = HeaderMap::new();

        // Log tracking headers for observability
        log_tracking_headers(original_headers);

        // Add content type
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Process Azure OpenAI authentication - use api-key instead of Authorization
        if let Some(api_key) = original_headers
            .get("api-key")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Using provided api-key header for Azure OpenAI");
            headers.insert(
                "api-key",
                http::header::HeaderValue::from_str(api_key).map_err(|_| {
                    error!("Failed to process api-key header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No api-key header found for Azure OpenAI request");
            return Err(AppError::MissingApiKey);
        }

        // Extract and validate Azure-specific configuration
        let resource_name = self.extract_resource_name(original_headers);
        let api_version = self.extract_api_version(original_headers);
        
        // For deployment ID, priority is: header > body > default
        let deployment_id = {
            // First check for header
            if let Some(header_deployment) = original_headers
                .get("x-azure-deployment-id")
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string())
            {
                debug!("Using deployment ID from header (overrides body): {}", header_deployment);
                header_deployment
            } else {
                // Then check if we have a model from the request body
                let stored_deployment = self.extracted_deployment_id.read().unwrap().clone();
                
                if !stored_deployment.is_empty() {
                    debug!("Using model from request body as deployment ID: {}", stored_deployment);
                    stored_deployment
                } else {
                    // Fall back to default
                    let default_deployment = self.default_deployment_id.clone();
                    debug!("Using default deployment ID: {}", default_deployment);
                    default_deployment
                }
            }
        };

        // Validate the extracted parameters
        self.validate_azure_parameters(&resource_name, &deployment_id)?;

        // Store the extracted Azure configuration for later use in transform_path
        *self.extracted_resource_name.write().unwrap() = resource_name.clone();
        *self.extracted_deployment_id.write().unwrap() = deployment_id.clone();
        *self.extracted_api_version.write().unwrap() = api_version.clone();
        
        debug!("Stored Azure configuration: resource={}, deployment={}, api_version={}", 
            resource_name, deployment_id, api_version);

        // Construct the Azure URL using the current path
        let current_path = self.current_path.read().unwrap().clone();
        if !current_path.is_empty() {
            debug!("Constructing Azure URL for path: {}", current_path);
            match self.build_azure_url(&resource_name, &deployment_id, &api_version, &current_path) {
                Ok(url) => {
                    let url_string = url.to_string();
                    debug!("Successfully constructed Azure URL: {}", url_string);
                    *self.constructed_url.write().unwrap() = Some(url_string);
                },
                Err(e) => {
                    error!("Failed to construct Azure URL: {}", e);
                    return Err(e);
                }
            }
        } else {
            debug!("No current path available, URL construction will be deferred");
        }

        // Store Azure configuration in custom headers for later use
        // This is a workaround since we need these values for URL construction
        headers.insert(
            "x-azure-resource-name-processed",
            http::header::HeaderValue::from_str(&resource_name).map_err(|_| AppError::InvalidHeader)?,
        );
        headers.insert(
            "x-azure-deployment-id-processed",
            http::header::HeaderValue::from_str(&deployment_id).map_err(|_| AppError::InvalidHeader)?,
        );
        headers.insert(
            "x-azure-api-version-processed",
            http::header::HeaderValue::from_str(&api_version).map_err(|_| AppError::InvalidHeader)?,
        );

        Ok(headers)
    }
}

/// Azure OpenAI-specific metrics extractor
///
/// Handles Azure-specific response format including content filtering results
/// and maintains compatibility with standard OpenAI metrics structure.
pub struct AzureOpenAIMetricsExtractor;

impl MetricsExtractor for AzureOpenAIMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        debug!("Extracting Azure OpenAI metrics from response: {}", response_body);
        let mut metrics = ProviderMetrics::default();

        // Extract usage information (same structure as OpenAI)
        if let Some(usage) = response_body.get("usage") {
            debug!("Found usage data: {:?}", usage);
            metrics.input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
            metrics.output_tokens = usage.get("completion_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
            metrics.total_tokens = usage.get("total_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
            debug!("Extracted tokens - input: {:?}, output: {:?}, total: {:?}", 
                metrics.input_tokens, metrics.output_tokens, metrics.total_tokens);
        }

        // Extract model information
        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            debug!("Found model: {}", model);
            metrics.model = model.to_string();
        }

        // Extract Azure-specific request ID if available
        if let Some(request_id) = response_body.get("id").and_then(|v| v.as_str()) {
            debug!("Found Azure request ID: {}", request_id);
            metrics.request_id = Some(request_id.to_string());
        }

        // Calculate cost based on Azure OpenAI pricing with separate input/output token rates
        if let (Some(input_tokens), Some(output_tokens)) = (metrics.input_tokens, metrics.output_tokens) {
            let model_name = &metrics.model;
            // Only calculate cost if we have a valid model name
            if !model_name.is_empty() {
                metrics.cost = Some(calculate_azure_cost(model_name, input_tokens, output_tokens));
                debug!("Calculated cost: {:?} for model {} with {} input tokens and {} output tokens", 
                    metrics.cost, model_name, input_tokens, output_tokens);
            } else {
                debug!("Skipping cost calculation due to missing model name");
            }
        }

        // TODO: Extract content filtering metrics when available
        // This will be implemented in the next enhancement step
        self.extract_content_filtering_metrics(response_body, &mut metrics);

        debug!("Final extracted Azure OpenAI metrics: {:?}", metrics);
        metrics
    }

    fn try_extract_provider_specific_streaming_metrics(&self, chunk: &str) -> Option<ProviderMetrics> {
        debug!("Attempting to extract metrics from Azure OpenAI streaming chunk: {}", chunk);
        
        if let Ok(json) = serde_json::from_str::<Value>(chunk) {
            // If we have usage data, extract full metrics
            if json.get("usage").is_some() {
                debug!("Found usage in Azure OpenAI streaming chunk, extracting metrics");
                return Some(self.extract_metrics(&json));
            }
            
            // For Azure OpenAI streaming, extract what we can even if usage is missing
            let model = json.get("model").and_then(|m| m.as_str()).unwrap_or("unknown").to_string();
            
            // Check for Azure OpenAI specific indicators
            if model.contains("gpt") || 
               json.get("object").and_then(|o| o.as_str()).unwrap_or("") == "chat.completion.chunk" ||
               json.get("choices").is_some() {
                debug!("Azure OpenAI streaming response detected without usage data, creating partial metrics");
                
                // Extract request ID if available
                let request_id = json.get("id").and_then(|id| id.as_str()).map(|s| s.to_string());
                
                return Some(ProviderMetrics {
                    model,
                    request_id,
                    provider_latency: Duration::from_millis(0),
                    // Leave token counts and cost as None for streaming chunks
                    ..Default::default()
                });
            }
        }
        
        debug!("No usage data found in Azure OpenAI streaming chunk");
        None
    }
}

impl AzureOpenAIMetricsExtractor {
    /// Extract content filtering metrics from Azure OpenAI responses
    ///
    /// Azure OpenAI includes content filtering information that can be valuable
    /// for compliance and monitoring purposes. This method extracts:
    /// - content_filter_results from the response choices
    /// - prompt_filter_results when available
    fn extract_content_filtering_metrics(&self, response_body: &Value, metrics: &mut ProviderMetrics) {
        // Extract content filtering results from choices
        if let Some(choices) = response_body.get("choices").and_then(|c| c.as_array()) {
            for (index, choice) in choices.iter().enumerate() {
                if let Some(content_filter_results) = choice.get("content_filter_results") {
                    debug!("Found content filter results for choice {}: {:?}", index, content_filter_results);
                    // TODO: Store content filter results in metrics
                    // For now, we'll just log them since ProviderMetrics doesn't have dedicated fields yet
                }
            }
        }

        // Extract prompt filtering results
        if let Some(prompt_filter_results) = response_body.get("prompt_filter_results") {
            debug!("Found prompt filter results: {:?}", prompt_filter_results);
            // TODO: Store prompt filter results in metrics
            // For now, we'll just log them since ProviderMetrics doesn't have dedicated fields yet
        }
    }
}

/// Calculate cost for Azure OpenAI models with current pricing tiers
///
/// Azure OpenAI pricing is based on separate input and output token costs
/// This function implements the most current Azure-specific pricing tiers
fn calculate_azure_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    // Azure OpenAI pricing (per 1K tokens) - Updated with current rates
    let (input_rate, output_rate) = match model.to_lowercase().as_str() {
        // GPT-4 Turbo models (128k context)
        m if m.contains("gpt-4-turbo") || m.contains("gpt-4-1106") || m.contains("gpt-4-0125") => {
            (0.01, 0.03) // $0.01 input, $0.03 output per 1K tokens
        },
        // GPT-4 Vision models
        m if m.contains("gpt-4-vision") || m.contains("gpt-4v") => {
            (0.01, 0.03) // Same as GPT-4 Turbo
        },
        // GPT-4 32k models
        m if m.contains("gpt-4-32k") || m.contains("gpt-4-0613-32k") => {
            (0.06, 0.12) // $0.06 input, $0.12 output per 1K tokens
        },
        // Standard GPT-4 models (8k context)
        m if m.contains("gpt-4") && !m.contains("turbo") => {
            (0.03, 0.06) // $0.03 input, $0.06 output per 1K tokens
        },
        // GPT-3.5 Turbo 16k models
        m if m.contains("gpt-35-turbo-16k") || m.contains("gpt-3.5-turbo-16k") => {
            (0.003, 0.004) // $0.003 input, $0.004 output per 1K tokens
        },
        // GPT-3.5 Turbo Instruct
        m if m.contains("gpt-35-turbo-instruct") || m.contains("gpt-3.5-turbo-instruct") => {
            (0.0015, 0.002) // $0.0015 input, $0.002 output per 1K tokens
        },
        // Standard GPT-3.5 Turbo models
        m if m.contains("gpt-35-turbo") || m.contains("gpt-3.5-turbo") => {
            (0.0015, 0.002) // $0.0015 input, $0.002 output per 1K tokens
        },
        // Text embedding models (input only, no output tokens)
        m if m.contains("text-embedding") => {
            (0.0001, 0.0) // $0.0001 per 1K tokens, no output cost
        },
        // DALL-E models (special pricing structure - simplified here)
        m if m.contains("dall-e") => {
            (0.0, 0.0) // DALL-E has per-image pricing, not token-based
        },
        // Whisper models (special pricing structure - simplified here)
        m if m.contains("whisper") => {
            (0.006, 0.0) // $0.006 per minute, approximated here as token cost
        },
        // Default for unknown models
        _ => {
            debug!("Unknown Azure model '{}' for cost calculation, using default rates", model);
            (0.0, 0.0)
        }
    };
    
    // Calculate total cost: (input_tokens / 1000 * input_rate) + (output_tokens / 1000 * output_rate)
    let input_cost = (input_tokens as f64 / 1000.0) * input_rate;
    let output_cost = (output_tokens as f64 / 1000.0) * output_rate;
    let total_cost = input_cost + output_cost;
    
    debug!(
        "Azure cost calculation for {}: {} input tokens (${:.6}) + {} output tokens (${:.6}) = ${:.6}",
        model, input_tokens, input_cost, output_tokens, output_cost, total_cost
    );
    
    total_cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use std::env;

    #[test]
    fn test_provider_creation() {
        let provider = AzureOpenAIProvider::new();
        assert_eq!(provider.name(), "azure-openai");
    }

    #[test]
    fn test_provider_base_url() {
        // Set a test resource name
        env::set_var("AZURE_OPENAI_RESOURCE_NAME", "test-resource");
        let provider = AzureOpenAIProvider::new();
        assert_eq!(provider.base_url(), "https://test-resource.openai.azure.com");
        env::remove_var("AZURE_OPENAI_RESOURCE_NAME");
    }

    #[test]
    fn test_extract_resource_name_with_default() {
        env::set_var("AZURE_OPENAI_RESOURCE_NAME", "default-resource");
        let provider = AzureOpenAIProvider::new();
        let headers = HeaderMap::new();
        
        let resource_name = provider.extract_resource_name(&headers);
        assert_eq!(resource_name, "default-resource");
        
        env::remove_var("AZURE_OPENAI_RESOURCE_NAME");
    }

    #[test]
    fn test_extract_resource_name_with_header() {
        let provider = AzureOpenAIProvider::new();
        let mut headers = HeaderMap::new();
        headers.insert("x-azure-resource-name", "custom-resource".parse().unwrap());
        
        let resource_name = provider.extract_resource_name(&headers);
        assert_eq!(resource_name, "custom-resource");
    }

    #[test]
    fn test_extract_deployment_id_with_default() {
        env::set_var("AZURE_OPENAI_DEPLOYMENT_ID", "default-deployment");
        let provider = AzureOpenAIProvider::new();
        let headers = HeaderMap::new();
        
        let deployment_id = provider.extract_deployment_id(&headers);
        assert_eq!(deployment_id, "default-deployment");
        
        env::remove_var("AZURE_OPENAI_DEPLOYMENT_ID");
    }

    #[test]
    fn test_extract_deployment_id_with_header() {
        let provider = AzureOpenAIProvider::new();
        let mut headers = HeaderMap::new();
        headers.insert("x-azure-deployment-id", "gpt-35-turbo".parse().unwrap());
        
        let deployment_id = provider.extract_deployment_id(&headers);
        assert_eq!(deployment_id, "gpt-35-turbo");
    }

    #[test]
    fn test_extract_api_version_with_default() {
        env::set_var("AZURE_OPENAI_API_VERSION", "2024-02-15-preview");
        let provider = AzureOpenAIProvider::new();
        let headers = HeaderMap::new();
        
        let api_version = provider.extract_api_version(&headers);
        assert_eq!(api_version, "2024-02-15-preview");
        
        env::remove_var("AZURE_OPENAI_API_VERSION");
    }

    #[test]
    fn test_extract_api_version_with_header() {
        let provider = AzureOpenAIProvider::new();
        let mut headers = HeaderMap::new();
        headers.insert("x-azure-api-version", "2023-12-01-preview".parse().unwrap());
        
        let api_version = provider.extract_api_version(&headers);
        assert_eq!(api_version, "2023-12-01-preview");
    }

    #[test]
    fn test_validate_azure_parameters_success() {
        let provider = AzureOpenAIProvider::new();
        let result = provider.validate_azure_parameters("test-resource", "gpt-4");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_azure_parameters_empty_resource() {
        let provider = AzureOpenAIProvider::new();
        let result = provider.validate_azure_parameters("", "gpt-4");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::RequestError(_)));
    }

    #[test]
    fn test_validate_azure_parameters_empty_deployment() {
        let provider = AzureOpenAIProvider::new();
        let result = provider.validate_azure_parameters("test-resource", "");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::RequestError(_)));
    }

    #[test]
    fn test_extract_azure_headers_with_defaults() {
        env::set_var("AZURE_OPENAI_RESOURCE_NAME", "default-resource");
        env::set_var("AZURE_OPENAI_DEPLOYMENT_ID", "default-deployment");
        env::set_var("AZURE_OPENAI_API_VERSION", "2024-02-15-preview");
        
        let provider = AzureOpenAIProvider::new();
        let headers = HeaderMap::new();
        
        let (resource, deployment, version) = provider.extract_azure_headers(&headers);
        assert_eq!(resource, "default-resource");
        assert_eq!(deployment, "default-deployment");
        assert_eq!(version, "2024-02-15-preview");
        
        // Clean up
        env::remove_var("AZURE_OPENAI_RESOURCE_NAME");
        env::remove_var("AZURE_OPENAI_DEPLOYMENT_ID");
        env::remove_var("AZURE_OPENAI_API_VERSION");
    }

    #[test]
    fn test_extract_azure_headers_with_overrides() {
        let provider = AzureOpenAIProvider::new();
        let mut headers = HeaderMap::new();
        
        headers.insert("x-azure-resource-name", "override-resource".parse().unwrap());
        headers.insert("x-azure-deployment-id", "override-deployment".parse().unwrap());
        headers.insert("x-azure-api-version", "2024-03-01".parse().unwrap());
        
        let (resource, deployment, version) = provider.extract_azure_headers(&headers);
        assert_eq!(resource, "override-resource");
        assert_eq!(deployment, "override-deployment");
        assert_eq!(version, "2024-03-01");
    }

    #[test]
    fn test_get_endpoint_path() {
        let provider = AzureOpenAIProvider::new();
        
        // Test chat completions
        assert_eq!(provider.get_endpoint_path("/v1/chat/completions").unwrap(), "chat/completions");
        assert_eq!(provider.get_endpoint_path("chat/completions").unwrap(), "chat/completions");
        
        // Test completions
        assert_eq!(provider.get_endpoint_path("/v1/completions").unwrap(), "completions");
        assert_eq!(provider.get_endpoint_path("completions").unwrap(), "completions");
        
        // Test embeddings
        assert_eq!(provider.get_endpoint_path("/v1/embeddings").unwrap(), "embeddings");
        assert_eq!(provider.get_endpoint_path("embeddings").unwrap(), "embeddings");
        
        // Test audio endpoints
        assert_eq!(provider.get_endpoint_path("/v1/audio/transcriptions").unwrap(), "audio/transcriptions");
        assert_eq!(provider.get_endpoint_path("audio/transcriptions").unwrap(), "audio/transcriptions");
        assert_eq!(provider.get_endpoint_path("/v1/audio/translations").unwrap(), "audio/translations");
        assert_eq!(provider.get_endpoint_path("audio/translations").unwrap(), "audio/translations");
        
        // Test image generation
        assert_eq!(provider.get_endpoint_path("/v1/images/generations").unwrap(), "images/generations");
        assert_eq!(provider.get_endpoint_path("images/generations").unwrap(), "images/generations");
        
        // Test unknown path (should default to chat/completions)
        assert_eq!(provider.get_endpoint_path("/unknown/path").unwrap(), "chat/completions");
        assert_eq!(provider.get_endpoint_path("").unwrap(), "chat/completions");
    }

    #[test]
    fn test_build_azure_url_success() {
        let provider = AzureOpenAIProvider::new();
        
        let url = provider.build_azure_url(
            "test-resource",
            "gpt-4",
            "2024-02-15-preview",
            "/v1/chat/completions"
        ).unwrap();
        
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str().unwrap(), "test-resource.openai.azure.com");
        assert_eq!(url.path(), "/openai/deployments/gpt-4/chat/completions");
        assert_eq!(url.query(), Some("api-version=2024-02-15-preview"));
        
        // Test with different endpoint
        let url2 = provider.build_azure_url(
            "test-resource",
            "gpt-35-turbo", 
            "2024-02-15-preview",
            "/v1/completions"
        ).unwrap();
        
        assert_eq!(url2.scheme(), "https");
        assert_eq!(url2.host_str().unwrap(), "test-resource.openai.azure.com");
        assert_eq!(url2.path(), "/openai/deployments/gpt-35-turbo/completions");
        assert_eq!(url2.query(), Some("api-version=2024-02-15-preview"));
    }

    #[test]
    fn test_build_azure_url_validation_errors() {
        let provider = AzureOpenAIProvider::new();
        
        // Test empty resource name
        let result = provider.build_azure_url("", "gpt-4", "2024-02-15-preview", "/v1/chat/completions");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::RequestError(_)));
        
        // Test empty deployment ID
        let result = provider.build_azure_url("test-resource", "", "2024-02-15-preview", "/v1/chat/completions");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::RequestError(_)));
        
        // Test empty API version
        let result = provider.build_azure_url("test-resource", "gpt-4", "", "/v1/chat/completions");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::RequestError(_)));
    }

    #[test]
    fn test_build_azure_url_with_different_endpoints() {
        let provider = AzureOpenAIProvider::new();
        
        // Test embeddings endpoint
        let url = provider.build_azure_url(
            "test-resource",
            "text-embedding-ada-002",
            "2024-02-15-preview",
            "/v1/embeddings"
        ).unwrap();
        assert_eq!(url.path(), "/openai/deployments/text-embedding-ada-002/embeddings");
        
        // Test audio transcription endpoint
        let url = provider.build_azure_url(
            "test-resource", 
            "whisper-1",
            "2024-02-15-preview",
            "/v1/audio/transcriptions"
        ).unwrap();
        assert_eq!(url.path(), "/openai/deployments/whisper-1/audio/transcriptions");
        
        // Test images generation endpoint
        let url = provider.build_azure_url(
            "test-resource",
            "dall-e-3", 
            "2024-02-15-preview",
            "/v1/images/generations"
        ).unwrap();
        assert_eq!(url.path(), "/openai/deployments/dall-e-3/images/generations");
    }

    #[test]
    fn test_process_headers_missing_api_key() {
        let provider = AzureOpenAIProvider::new();
        let headers = HeaderMap::new();
        
        let result = provider.process_headers(&headers);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::MissingApiKey));
    }

    #[test]
    fn test_process_headers_missing_resource_name() {
        // Ensure no environment variables are set that could provide defaults
        env::remove_var("AZURE_OPENAI_RESOURCE_NAME");
        env::remove_var("AZURE_OPENAI_DEPLOYMENT_ID");
        
        let provider = AzureOpenAIProvider::new();
        let mut headers = HeaderMap::new();
        headers.insert("api-key", "test-key".parse().unwrap());
        
        let result = provider.process_headers(&headers);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::RequestError(_)));
    }

    #[test]
    fn test_calculate_azure_cost() {
        // Test GPT-4 standard model (0.03 input + 0.06 output per 1K tokens)
        let gpt4_cost = calculate_azure_cost("gpt-4", 1000, 1000);
        assert!((gpt4_cost - 0.09).abs() < f64::EPSILON, "GPT-4 cost should be $0.09 (0.03 + 0.06), got {}", gpt4_cost);
        
        // Test GPT-3.5 Turbo (0.0015 input + 0.002 output per 1K tokens)
        assert_eq!(calculate_azure_cost("gpt-35-turbo", 1000, 1000), 0.0035);
        assert_eq!(calculate_azure_cost("gpt-3.5-turbo", 1000, 1000), 0.0035);
        
        // Test GPT-4 Turbo (0.01 input + 0.03 output per 1K tokens)
        assert_eq!(calculate_azure_cost("gpt-4-turbo", 1000, 1000), 0.04);
        
        // Test GPT-4 32k (0.06 input + 0.12 output per 1K tokens)
        assert_eq!(calculate_azure_cost("gpt-4-32k", 1000, 1000), 0.18);
        
        // Test text embedding (input only, 0.0001 per 1K tokens)
        assert_eq!(calculate_azure_cost("text-embedding-ada-002", 1000, 0), 0.0001);
        
        // Test unknown model
        assert_eq!(calculate_azure_cost("unknown-model", 1000, 1000), 0.0);
    }

    #[test]
    fn test_metrics_extractor_creation() {
        let extractor = AzureOpenAIMetricsExtractor;
        let response = serde_json::json!({
            "model": "gpt-4",
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            },
            "id": "test-request-id"
        });
        
        let metrics = extractor.extract_metrics(&response);
        assert_eq!(metrics.model, "gpt-4");
        assert_eq!(metrics.input_tokens, Some(10));
        assert_eq!(metrics.output_tokens, Some(20));
        assert_eq!(metrics.total_tokens, Some(30));
        assert_eq!(metrics.request_id, Some("test-request-id".to_string()));
        // Expected cost: (10/1000 * 0.03) + (20/1000 * 0.06) = 0.0003 + 0.0012 = 0.0015
        assert!((metrics.cost.unwrap() - 0.0015).abs() < f64::EPSILON, "Expected cost 0.0015, got {:?}", metrics.cost);
    }

    #[test]
    fn test_metrics_extractor_with_content_filtering() {
        let extractor = AzureOpenAIMetricsExtractor;
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
        let extractor = AzureOpenAIMetricsExtractor;
        
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
        let extractor = AzureOpenAIMetricsExtractor;
        
        // Test streaming chunk without usage
        let chunk_without_usage = r#"{"id":"chatcmpl-test","object":"chat.completion.chunk","created":1748093069,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        
        let metrics = extractor.try_extract_provider_specific_streaming_metrics(chunk_without_usage);
        assert!(metrics.is_some());
        let metrics = metrics.unwrap();
        assert_eq!(metrics.model, "gpt-4");
        assert_eq!(metrics.request_id, Some("chatcmpl-test".to_string()));
        assert!(metrics.cost.is_none()); // No cost for streaming chunks without usage
        
        // Test streaming chunk with usage (final chunk)
        let chunk_with_usage = r#"{"id":"chatcmpl-test","object":"chat.completion.chunk","created":1748093069,"model":"gpt-4","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}}"#;
        
        let metrics_with_usage = extractor.try_extract_provider_specific_streaming_metrics(chunk_with_usage);
        assert!(metrics_with_usage.is_some());
        let metrics_with_usage = metrics_with_usage.unwrap();
        assert_eq!(metrics_with_usage.model, "gpt-4");
        assert_eq!(metrics_with_usage.input_tokens, Some(10));
        assert_eq!(metrics_with_usage.output_tokens, Some(20));
        assert!(metrics_with_usage.cost.is_some());
    }

    #[test]
    fn test_metrics_extractor_missing_fields() {
        let extractor = AzureOpenAIMetricsExtractor;
        
        // Test response with missing usage
        let response_no_usage = serde_json::json!({
            "model": "gpt-4",
            "id": "test-no-usage"
        });
        
        let metrics = extractor.extract_metrics(&response_no_usage);
        assert_eq!(metrics.model, "gpt-4");
        assert_eq!(metrics.request_id, Some("test-no-usage".to_string()));
        assert!(metrics.input_tokens.is_none());
        assert!(metrics.output_tokens.is_none());
        assert!(metrics.cost.is_none());
        
        // Test response with missing model
        let response_no_model = serde_json::json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            },
            "id": "test-no-model"
        });
        
        let metrics = extractor.extract_metrics(&response_no_model);
        assert_eq!(metrics.model, ""); // Default empty string
        assert_eq!(metrics.input_tokens, Some(10));
        assert_eq!(metrics.output_tokens, Some(20));
        assert!(metrics.cost.is_none()); // No cost without model
    }

    #[test]
    fn test_extract_model_from_body_success() {
        let provider = AzureOpenAIProvider::new();
        
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
        let provider = AzureOpenAIProvider::new();
        
        // Test JSON without model field
        let json_body = r#"{"messages": [{"role": "user", "content": "Hello"}], "temperature": 0.7}"#;
        let model = provider.extract_model_from_body(json_body.as_bytes());
        assert_eq!(model, None);
    }

    #[test]
    fn test_extract_model_from_body_invalid_json() {
        let provider = AzureOpenAIProvider::new();
        
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
        let provider = AzureOpenAIProvider::new();
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
        let provider = AzureOpenAIProvider::new();
        
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
        let provider = AzureOpenAIProvider::new();
        
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
        env::set_var("AZURE_OPENAI_RESOURCE_NAME", "test-resource");
        let provider = AzureOpenAIProvider::new();
        
        // Step 1: Simulate the new workflow - only api-key and x-azure-resource-name needed
        let mut headers = HeaderMap::new();
        headers.insert("api-key", "test-key".parse().unwrap());
        // x-azure-resource-name comes from environment
        // x-azure-deployment-id will come from model in request body
        // x-azure-api-version will use default
        
        // Step 2: Simulate before_request extracting model from body
        let json_body = r#"{"model": "gpt-4", "messages": [{"role": "user", "content": "Hello"}]}"#;
        let model = provider.extract_model_from_body(json_body.as_bytes());
        assert_eq!(model, Some("gpt-4".to_string()));
        
        // Store the model (this happens in before_request)
        *provider.extracted_deployment_id.write().unwrap() = "gpt-4".to_string();
        
        // Step 3: process_headers should work with minimal headers
        let result = provider.process_headers(&headers);
        assert!(result.is_ok());
        
        // Verify configuration
        let resource_name = provider.extracted_resource_name.read().unwrap().clone();
        let deployment_id = provider.extracted_deployment_id.read().unwrap().clone();
        let api_version = provider.extracted_api_version.read().unwrap().clone();
        
        assert_eq!(resource_name, "test-resource");
        assert_eq!(deployment_id, "gpt-4");
        assert_eq!(api_version, "2024-02-15-preview"); // default
        
        env::remove_var("AZURE_OPENAI_RESOURCE_NAME");
    }
} 