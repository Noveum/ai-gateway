use super::Provider;
use super::utils::log_tracking_headers;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{HeaderMap, Response},
};
use reqwest::Url;
use serde_json::Value;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tracing::{debug, error, warn};
use bytes::Bytes;

/// Model mapping utilities for Azure OpenAI
mod model_mapping {
    use tracing::debug;

    /// Map Azure deployment ID to actual model name
    pub fn map_deployment_to_model(deployment_id: &str, response_model: Option<&str>) -> String {
        debug!("Mapping Azure deployment '{}' to model name, response_model={:?}", deployment_id, response_model);
        
        // If the response includes a model name, use that (most accurate)
        if let Some(model) = response_model {
            debug!("Using model name from response: {}", model);
            return model.to_string();
        }
        
        // Otherwise, try to infer from deployment ID patterns
        let deployment_lower = deployment_id.to_lowercase();
        debug!("Deployment ID: {}", deployment_lower);
        
        let mapped_model = match deployment_lower.as_str() {
            // O3 models (latest reasoning models from 2025)
            d if d.contains("o3-mini") => "o3-mini",
            d if d.contains("o3") => "o3",
            
            // O4 models (newer reasoning models)
            d if d.contains("o4-mini") => "o4-mini",
            d if d.contains("o4") => "o4",
            
            // O1 models (reasoning models)
            d if d.contains("o1-preview") => "o1-preview",
            d if d.contains("o1-mini") => "o1-mini",
            d if d.contains("o1") => "o1-preview",
            
            // GPT-4.1 series (2025 models)
            d if d.contains("gpt-4.1-nano") || d.contains("gpt-41-nano") => "gpt-4.1-nano",
            d if d.contains("gpt-4.1-mini") || d.contains("gpt-41-mini") => "gpt-4.1-mini",
            d if d.contains("gpt-4.1") || d.contains("gpt-41") => "gpt-4.1",
            
            // GPT-4.5 models
            d if d.contains("gpt-4.5") || d.contains("gpt-45") => "gpt-4.5-preview",
            
            // GPT-4o models
            d if d.contains("gpt-4o-realtime") => "gpt-4o-realtime-preview",
            d if d.contains("gpt-4o-audio") => "gpt-4o-audio-preview",
            d if d.contains("gpt-4o-mini") => "gpt-4o-mini",
            d if d.contains("gpt-4o") => "gpt-4o",
            
            // GPT-4 Turbo models
            d if d.contains("gpt-4-turbo") || d.contains("gpt4-turbo") || d.contains("gpt-4-1106") || d.contains("gpt-4-0125") => "gpt-4-turbo",
            
            // GPT-4 Vision models
            d if d.contains("gpt-4-vision") || d.contains("gpt4-vision") || d.contains("gpt-4v") => "gpt-4-vision-preview",
            
            // GPT-4 32k models
            d if d.contains("gpt-4-32k") || d.contains("gpt4-32k") => "gpt-4-32k",
            
            // Standard GPT-4 models
            d if d.contains("gpt-4") || d.contains("gpt4") => "gpt-4",
            
            // GPT-3.5 Turbo variants
            d if d.contains("gpt-35-turbo-16k") || d.contains("gpt-3.5-turbo-16k") || d.contains("gpt35-turbo-16k") => "gpt-3.5-turbo-16k",
            d if d.contains("gpt-35-turbo-instruct") || d.contains("gpt-3.5-turbo-instruct") => "gpt-3.5-turbo-instruct",
            d if d.contains("gpt-35-turbo") || d.contains("gpt-3.5-turbo") || d.contains("gpt35-turbo") => "gpt-3.5-turbo",
            
            // Embedding models
            d if d.contains("text-embedding-ada-002") || d.contains("ada-002") => "text-embedding-ada-002",
            d if d.contains("text-embedding-3-small") || d.contains("embedding-3-small") => "text-embedding-3-small",
            d if d.contains("text-embedding-3-large") || d.contains("embedding-3-large") => "text-embedding-3-large",
            d if d.contains("text-embedding") || d.contains("embedding") => "text-embedding-ada-002",
            
            // DALL-E models
            d if d.contains("dall-e-3") || d.contains("dalle-3") => "dall-e-3",
            d if d.contains("dall-e-2") || d.contains("dalle-2") => "dall-e-2",
            d if d.contains("dall-e") || d.contains("dalle") => "dall-e-3",
            
            // Whisper models
            d if d.contains("whisper-1") || d.contains("whisper") => "whisper-1",
            
            // Fallback: use deployment ID itself
            _ => {
                debug!("No model mapping found for deployment '{}', using deployment ID as model name", deployment_id);
                deployment_id
            }
        };
        
        let result = mapped_model.to_string();
        debug!("Mapped deployment '{}' to model '{}'", deployment_id, result);
        result
    }

    /// Check if model requires parameter transformation (max_tokens -> max_completion_tokens)
    pub fn model_requires_transformation(model: &str) -> bool {
        matches!(model, 
            "o1" | "o1-preview" | "o1-mini" |
            "o3" | "o3-mini" |
            "o4" | "o4-mini" |
            "gpt-4.1" | "gpt-4.1-nano" | "gpt-4.1-mini" |
            "gpt-4.5-preview"
        ) || model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4") || model.contains("o4-mini")
    }
}

/// Pricing utilities for Azure OpenAI
mod pricing {
    use tracing::debug;

    /// Get pricing information for a model (input_cost_per_1k, output_cost_per_1k)
    pub fn get_model_pricing(model: &str) -> (f64, f64) {
        match model {
            // O3 models (latest reasoning models from 2025) - Estimated pricing
            "o3" => (0.02, 0.08),
            "o3-mini" => (0.008, 0.032),
            
            // O4 models - Estimated pricing
            "o4" => (0.015, 0.06),
            "o4-mini" | "o4-mini-2025-04-16" => (0.004, 0.016),
            d if d.starts_with("o4-mini") => (0.004, 0.016),
            
            // O1 models - Higher cost due to reasoning capability
            "o1-preview" => (0.015, 0.06),
            "o1-mini" => (0.003, 0.012),
            
            // GPT-4.1 series - Estimated pricing
            "gpt-4.1" => (0.012, 0.036),
            "gpt-4.1-mini" => (0.0008, 0.0024),
            "gpt-4.1-nano" => (0.0002, 0.0006),
            
            // GPT-4.5 models
            "gpt-4.5-preview" => (0.015, 0.045),
            
            // GPT-4o models
            "gpt-4o" | "gpt-4o-2024-11-20" | "gpt-4o-2024-08-06" | "gpt-4o-2024-05-13" => (0.005, 0.015),
            "gpt-4o-mini" | "gpt-4o-mini-2024-07-18" => (0.0004, 0.0016),
            "gpt-4o-realtime-preview" => (0.01, 0.03),
            "gpt-4o-audio-preview" => (0.008, 0.024),
            
            // GPT-4 Turbo models
            "gpt-4-turbo" | "gpt-4-turbo-2024-04-09" | "gpt-4-turbo-preview" | 
            "gpt-4-0125-preview" | "gpt-4-1106-preview" => (0.01, 0.03),
            
            // GPT-4 Vision models
            "gpt-4-vision-preview" | "gpt-4v" => (0.01, 0.03),
            
            // GPT-4 32K models
            "gpt-4-32k" | "gpt-4-32k-0314" | "gpt-4-32k-0613" => (0.06, 0.12),
            
            // Standard GPT-4 models
            "gpt-4" | "gpt-4-0314" | "gpt-4-0613" => (0.03, 0.06),
            
            // GPT-3.5 Turbo variants
            "gpt-35-turbo-16k" | "gpt-3.5-turbo-16k" | 
            "gpt-35-turbo-16k-0613" | "gpt-3.5-turbo-16k-0613" => (0.003, 0.004),
            "gpt-35-turbo-instruct" | "gpt-3.5-turbo-instruct" => (0.0015, 0.002),
            "gpt-35-turbo" | "gpt-3.5-turbo" | "gpt-35-turbo-0301" | 
            "gpt-3.5-turbo-0613" | "gpt-35-turbo-1106" | "gpt-3.5-turbo-1106" => (0.0015, 0.002),
            
            // Embedding models (input only)
            "text-embedding-ada-002" => (0.0001, 0.0),
            "text-embedding-3-small" => (0.00002, 0.0),
            "text-embedding-3-large" => (0.00013, 0.0),
            
            // DALL-E models (approximate per generation)
            "dall-e-3" => (0.04, 0.0),
            "dall-e-2" => (0.02, 0.0),
            
            // Whisper models (per minute, approximated)
            "whisper-1" => (0.006, 0.0),
            
            // Unknown models
            _ => {
                debug!("Unknown model '{}' for Azure cost calculation, returning $0", model);
                (0.0, 0.0)
            }
        }
    }

    /// Calculate cost based on model and token usage
    pub fn calculate_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
        let (input_cost_per_1k, output_cost_per_1k) = get_model_pricing(model);
        
        let input_cost = (input_tokens as f64 / 1000.0) * input_cost_per_1k;
        let output_cost = (output_tokens as f64 / 1000.0) * output_cost_per_1k;
        let total_cost = input_cost + output_cost;
        
        debug!(
            "Azure cost calculation for model '{}': {} input tokens (${:.6}), {} output tokens (${:.6}), total: ${:.6}",
            model, input_tokens, input_cost, output_tokens, output_cost, total_cost
        );
        
        total_cost
    }
}

/// Azure resource validation utilities
mod validation {
    use crate::error::AppError;

    /// Validate Azure resource name format
    pub fn validate_resource_name(resource_name: &str) -> Result<(), AppError> {
        if resource_name.is_empty() {
            return Err(AppError::RequestError(
                "Azure resource name is required. Provide 'x-azure-resource-name' header or set AZURE_OPENAI_RESOURCE_NAME environment variable.".to_string()
            ));
        }

        if resource_name.len() < 3 || resource_name.len() > 24 {
            return Err(AppError::RequestError(
                format!("Azure resource name '{}' must be between 3 and 24 characters", resource_name)
            ));
        }
        
        if !resource_name.chars().all(|c| c.is_alphanumeric() || c == '-') {
            return Err(AppError::RequestError(
                format!("Azure resource name '{}' can only contain alphanumeric characters and hyphens", resource_name)
            ));
        }
        
        if resource_name.starts_with('-') || resource_name.ends_with('-') {
            return Err(AppError::RequestError(
                format!("Azure resource name '{}' cannot start or end with a hyphen", resource_name)
            ));
        }

        Ok(())
    }

    /// Validate Azure deployment ID
    pub fn validate_deployment_id(deployment_id: &str) -> Result<(), AppError> {
        if deployment_id.is_empty() {
            return Err(AppError::RequestError(
                "Azure deployment ID is required. The 'model' field must be provided in the request body.".to_string()
            ));
        }
        Ok(())
    }
}

/// Configuration for Azure OpenAI provider
/// 
/// Centralizes all Azure OpenAI-specific configuration with validation
/// and environment variable support.
#[derive(Debug, Clone)]
pub struct AzureOpenAIProviderConfig {
    /// Default Azure resource name (e.g., "my-openai-resource")
    pub default_resource_name: String,
    /// Default deployment ID (e.g., "gpt-4", "gpt-35-turbo")
    pub default_deployment_id: String,
    /// Default API version (e.g., "2024-02-15-preview")
    pub default_api_version: String,
    /// Allow empty resource name (primarily for testing)
    pub allow_empty_resource_name: bool,
    /// Allow empty deployment ID (primarily for testing)
    pub allow_empty_deployment_id: bool,
}

impl Default for AzureOpenAIProviderConfig {
    fn default() -> Self {
        Self {
            default_resource_name: std::env::var("AZURE_OPENAI_RESOURCE_NAME")
                .unwrap_or_default(),
            default_deployment_id: "".to_string(), // Always empty - will be extracted from request body
            default_api_version: std::env::var("AZURE_OPENAI_API_VERSION")
                .unwrap_or_else(|_| "2025-01-01-preview".to_string()), // Updated to support latest models
            allow_empty_resource_name: false,
            allow_empty_deployment_id: true, // Always allow empty since we extract from request body
        }
    }
}

impl AzureOpenAIProviderConfig {
    /// Create a new configuration instance with defaults from environment variables
    pub fn new() -> Self {
        Self::default()
    }
    
    /// Create configuration with explicit values
    pub fn with_defaults(
        resource_name: impl Into<String>,
        deployment_id: impl Into<String>,
        api_version: impl Into<String>,
    ) -> Self {
        Self {
            default_resource_name: resource_name.into(),
            default_deployment_id: deployment_id.into(), 
            default_api_version: api_version.into(),
            allow_empty_resource_name: false,
            allow_empty_deployment_id: false,
        }
    }
    
    /// Allow empty resource name (primarily for testing)
    pub fn allow_empty_resource_name(mut self, allow: bool) -> Self {
        self.allow_empty_resource_name = allow;
        self
    }
    
    /// Allow empty deployment ID (primarily for testing)
    pub fn allow_empty_deployment_id(mut self, allow: bool) -> Self {
        self.allow_empty_deployment_id = allow;
        self
    }
    
    /// Validate the configuration
    /// 
    /// Ensures that all required configuration parameters are present and valid
    pub fn validate(&self) -> Result<(), AppError> {
        // Validate API version is not empty
        if self.default_api_version.is_empty() {
            return Err(AppError::RequestError(
                "Azure API version cannot be empty".to_string()
            ));
        }
        
        // Validate API version format (basic check)
        if !self.default_api_version.contains("-") {
            warn!("Azure API version '{}' may not be in expected format (YYYY-MM-DD-preview)", self.default_api_version);
        }
        
        // Check for required resource name (unless explicitly allowed to be empty)
        if !self.allow_empty_resource_name && self.default_resource_name.is_empty() {
            return Err(AppError::RequestError(
                "Azure resource name is required. Set AZURE_OPENAI_RESOURCE_NAME environment variable or provide via configuration".to_string()
            ));
        }
        
        // Deployment ID validation is now optional since we extract it from request body
        // Only validate if a deployment ID is provided and we're not allowing empty
        if !self.allow_empty_deployment_id && self.default_deployment_id.is_empty() {
            return Err(AppError::RequestError(
                "Azure deployment ID is required. Set AZURE_OPENAI_DEPLOYMENT_ID environment variable or provide via configuration".to_string()
            ));
        }
        
        // Validate resource name format (basic Azure naming rules)
        if !self.default_resource_name.is_empty() {
            if self.default_resource_name.len() < 3 || self.default_resource_name.len() > 24 {
                return Err(AppError::RequestError(
                    "Azure resource name must be between 3 and 24 characters".to_string()
                ));
            }
            
            // Azure resource names should be alphanumeric with hyphens
            if !self.default_resource_name.chars().all(|c| c.is_alphanumeric() || c == '-') {
                return Err(AppError::RequestError(
                    "Azure resource name can only contain alphanumeric characters and hyphens".to_string()
                ));
            }
            
            // Should not start or end with hyphen
            if self.default_resource_name.starts_with('-') || self.default_resource_name.ends_with('-') {
                return Err(AppError::RequestError(
                    "Azure resource name cannot start or end with a hyphen".to_string()
                ));
            }
        }
        
        debug!("Azure OpenAI configuration validated successfully");
        Ok(())
    }
}

/// Azure OpenAI provider implementation
/// 
/// Handles Azure-hosted OpenAI models with enterprise-grade security and compliance.
/// Key differences from standard OpenAI:
/// - Uses api-key header instead of Authorization bearer token
/// - URL structure includes resource name and deployment ID
/// - Requires API version parameter
/// - Responses include Azure-specific content filtering results
pub struct AzureOpenAIProvider {
    /// Configuration for the Azure OpenAI provider
    config: AzureOpenAIProviderConfig,
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
    /// Create a new Azure OpenAI provider instance with default configuration
    ///
    /// Reads configuration from environment variables with sensible defaults:
    /// - AZURE_OPENAI_RESOURCE_NAME: The Azure resource name
    /// - AZURE_OPENAI_DEPLOYMENT_ID: The deployment identifier
    /// - AZURE_OPENAI_API_VERSION: The API version (defaults to "2024-02-15-preview")
    pub fn new() -> Result<Self, AppError> {
        let config = AzureOpenAIProviderConfig::default();
        Self::with_config(config)
    }
    
    /// Create a new Azure OpenAI provider instance with custom configuration
    /// 
    /// Allows for more flexible configuration without relying solely on environment variables
    pub fn with_config(config: AzureOpenAIProviderConfig) -> Result<Self, AppError> {
        // Only validate basic configuration at creation time
        // Deployment ID validation will happen during request processing when we extract from body
        if config.default_api_version.is_empty() {
            return Err(AppError::RequestError(
                "Azure API version cannot be empty".to_string()
            ));
        }
        
        debug!(
            "Creating Azure OpenAI provider with config: resource={}, deployment={}, api_version={}",
            config.default_resource_name,
            config.default_deployment_id,
            config.default_api_version
        );
        
        Ok(Self {
            config,
            constructed_url: Arc::new(RwLock::new(None)),
            current_path: Arc::new(RwLock::new("".to_string())),
            extracted_resource_name: Arc::new(RwLock::new("".to_string())),
            extracted_deployment_id: Arc::new(RwLock::new("".to_string())),
            extracted_api_version: Arc::new(RwLock::new("".to_string())),
        })
    }

    /// Extract Azure resource name from headers with fallback to default
    ///
    /// Looks for 'x-azure-resource-name' header first, then falls back to the configured default
    pub fn extract_resource_name(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-azure-resource-name")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.config.default_resource_name.clone())
    }

    /// Extract Azure deployment ID from headers with fallback to default
    ///
    /// Looks for 'x-azure-deployment-id' header first, then falls back to the configured default
    pub fn extract_deployment_id(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-azure-deployment-id")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.config.default_deployment_id.clone())
    }

    /// Extract Azure API version from headers with fallback to default
    ///
    /// Looks for 'x-azure-api-version' header first, then falls back to the configured default
    pub fn extract_api_version(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-azure-api-version")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.config.default_api_version.clone())
    }

    /// Validate Azure-specific parameters
    ///
    /// Ensures that required Azure parameters are not empty and meet basic requirements.
    /// This validation happens when headers are processed, not during provider creation.
    /// By this point, deployment_id should have been extracted from the request body.
    pub fn validate_azure_parameters(&self, resource_name: &str, deployment_id: &str) -> Result<(), AppError> {
        validation::validate_resource_name(resource_name)?;
        validation::validate_deployment_id(deployment_id)?;
        debug!("Azure parameters validated successfully: resource={}, deployment={}", resource_name, deployment_id);
        Ok(())
    }

    /// Extract Azure-specific headers from the request
    ///
    /// Returns a tuple containing (resource_name, deployment_id, api_version)
    /// Uses the individual extraction methods for better modularity
    pub fn extract_azure_headers(&self, headers: &HeaderMap) -> (String, String, String) {
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
    pub fn get_endpoint_path(&self, path: &str) -> Result<&'static str, AppError> {
        let endpoint = match path {
            p if p.contains("chat/completions") || p.ends_with("chat/completions") => "chat/completions",
            p if p.contains("completions") || p.ends_with("completions") => "completions",
            p if p.contains("embeddings") || p.ends_with("embeddings") => "embeddings",
            p if p.contains("audio/transcriptions") || p.ends_with("audio/transcriptions") => "audio/transcriptions",
            p if p.contains("audio/translations") || p.ends_with("audio/translations") => "audio/translations",
            p if p.contains("images/generations") || p.ends_with("images/generations") => "images/generations",
            _ => {
                debug!("Unknown path '{}', defaulting to chat/completions", path);
                "chat/completions"
            }
        };
        Ok(endpoint)
    }

    /// Construct Azure OpenAI API URL using reqwest::Url for proper validation
    ///
    /// Format: https://{resource}.openai.azure.com/openai/deployments/{deployment}/{endpoint}?api-version={version}
    pub fn build_azure_url(&self, resource_name: &str, deployment_id: &str, api_version: &str, path: &str) -> Result<Url, AppError> {
        // Validate input parameters
        if resource_name.is_empty() || deployment_id.is_empty() || api_version.is_empty() {
            return Err(AppError::RequestError("All Azure parameters (resource, deployment, API version) are required".to_string()));
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
        url.query_pairs_mut().append_pair("api-version", api_version);

        debug!("Constructed Azure OpenAI URL: {}", url);
        Ok(url)
    }

    /// Extract model name from request body JSON
    /// 
    /// Azure OpenAI uses the model name from the request body as the deployment name
    /// This allows for a cleaner API where users only specify the model they want
    pub fn extract_model_from_body(&self, body_bytes: &Bytes) -> Option<String> {
        debug!("Attempting to extract model from request body, body size: {} bytes", body_bytes.len());
        
        let body_str = std::str::from_utf8(body_bytes).ok()?;
        let json_value: serde_json::Value = serde_json::from_str(body_str).ok()?;
        
        if let Some(model) = json_value.get("model").and_then(|m| m.as_str()) {
            debug!("Extracted model from request body: {}", model);
            Some(model.to_string())
        } else {
            debug!("No 'model' field found in JSON body");
            None
        }
    }

    /// Transform request body for Azure OpenAI compatibility
    /// 
    /// Newer models (o1, o3, o4, etc.) require max_completion_tokens instead of max_tokens
    fn transform_request_body(&self, body_bytes: &Bytes, model: &str) -> Result<Bytes, AppError> {
        if !model_mapping::model_requires_transformation(model) {
            debug!("Model '{}' does not require parameter transformation", model);
            return Ok(body_bytes.clone());
        }
        
        debug!("Transforming request body for model '{}' - replacing max_tokens with max_completion_tokens", model);
        
        // Parse and transform the JSON
        let body_str = std::str::from_utf8(body_bytes)
            .map_err(|e| AppError::RequestError(format!("Invalid UTF-8 in request body: {}", e)))?;
            
        let mut json_value: serde_json::Value = serde_json::from_str(body_str)
            .map_err(|e| AppError::RequestError(format!("Invalid JSON in request body: {}", e)))?;
        
        // Transform max_tokens to max_completion_tokens if present
        if let Some(max_tokens) = json_value.get("max_tokens") {
            debug!("Found max_tokens parameter, converting to max_completion_tokens for model '{}'", model);
            let max_tokens_value = max_tokens.clone();
            
            if let Some(obj) = json_value.as_object_mut() {
                obj.remove("max_tokens");
                obj.insert("max_completion_tokens".to_string(), max_tokens_value);
                debug!("Successfully transformed max_tokens to max_completion_tokens");
            }
        }
        
        // Convert back to bytes
        let transformed_body = serde_json::to_string(&json_value)
            .map_err(|e| AppError::RequestError(format!("Failed to serialize transformed JSON: {}", e)))?;
            
        debug!("Transformed request body: {}", transformed_body);
        Ok(Bytes::from(transformed_body))
    }

    /// Handle Azure OpenAI-specific error responses
    pub async fn handle_azure_error(&self, response: reqwest::Response) -> AppError {
        let status = response.status();
        let status_code = status.as_u16();
        
        debug!("Handling Azure OpenAI error response with status: {}", status_code);
        
        // Try to parse the error response body
        let error_body = match response.text().await {
            Ok(body) => {
                debug!("Azure error response body: {}", body);
                body
            },
            Err(e) => {
                error!("Failed to read Azure error response body: {}", e);
                return AppError::AzureProviderError(format!("HTTP {} - Could not read error response", status_code));
            }
        };
        
        // Try to parse as JSON to extract Azure error details
        let error_json: Result<serde_json::Value, _> = serde_json::from_str(&error_body);
        
        match (status_code, error_json) {
            (401, _) => {
                debug!("Azure authentication error detected");
                AppError::AzureAuthenticationError(
                    "Invalid Azure OpenAI API key or insufficient permissions".to_string()
                )
            },
            (404, Ok(json)) => {
                debug!("Azure resource not found error detected");
                let message = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Azure resource or deployment not found");
                AppError::AzureResourceNotFoundError(message.to_string())
            },
            (429, Ok(json)) => {
                debug!("Azure rate limit error detected");
                let message = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Azure OpenAI rate limit exceeded");
                AppError::AzureRateLimitError(message.to_string())
            },
            (400, Ok(json)) if json
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .map_or(false, |code| code.contains("content_filter") || code.contains("ContentFilter")) => {
                debug!("Azure content filtering error detected");
                let message = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Content was filtered by Azure OpenAI");
                AppError::AzureContentFilterError(message.to_string())
            },
            (400, Ok(json)) => {
                debug!("Azure bad request error detected");
                let message = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Invalid request to Azure OpenAI");
                AppError::AzureProviderError(format!("Bad request: {}", message))
            },
            (500..=599, Ok(json)) => {
                debug!("Azure server error detected: {}", status_code);
                let message = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Azure OpenAI service error");
                AppError::AzureProviderError(format!("Server error ({}): {}", status_code, message))
            },
            (_, Ok(json)) if json.get("error").is_some() => {
                debug!("Azure structured error detected: {}", status_code);
                let error_code = json
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown");
                let message = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown Azure OpenAI error");
                AppError::AzureProviderError(format!("Error {}: {} ({})", error_code, message, status_code))
            },
            _ => {
                debug!("Azure unstructured error detected: {}", status_code);
                AppError::AzureProviderError(format!(
                    "HTTP {} - {}", 
                    status_code, 
                    if error_body.is_empty() { "No error details provided" } else { &error_body }
                ))
            }
        }
    }

    /// Get the currently extracted deployment ID (for testing)
    #[cfg(test)]
    pub fn get_extracted_deployment_id(&self) -> String {
        self.extracted_deployment_id.read().unwrap().clone()
    }

    /// Set the extracted deployment ID (for testing)
    #[cfg(test)]
    pub fn set_extracted_deployment_id(&self, deployment_id: String) {
        *self.extracted_deployment_id.write().unwrap() = deployment_id;
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
        let fallback = format!("https://{}.openai.azure.com", self.config.default_resource_name);
        debug!("Using fallback Azure URL: {}", fallback);
        fallback
    }

    fn name(&self) -> &str {
        "azure-openai"
    }

    fn transform_path(&self, path: &str) -> String {
        // Store the current path for later use
        *self.current_path.write().unwrap() = path.to_string();
        
        // Get the Azure configuration that was stored during process_headers
        let resource_name = self.extracted_resource_name.read().unwrap().clone();
        let deployment_id = self.extracted_deployment_id.read().unwrap().clone();
        let api_version = self.extracted_api_version.read().unwrap().clone();
        
        debug!("Using stored Azure configuration in transform_path: resource={}, deployment={}, path={}", 
            resource_name, deployment_id, path);
        
        // Validate that we have all required parameters
        if resource_name.is_empty() {
            error!("No Azure resource name available in transform_path");
            return format!("ERROR_NO_AZURE_RESOURCE_NAME_{}", path);
        }
        
        if deployment_id.is_empty() {
            error!("No Azure deployment ID available in transform_path - model should have been extracted from request body");
            return format!("ERROR_NO_DEPLOYMENT_ID_{}", path);
        }
        
        // Validate the deployment ID now that we have it
        if let Err(e) = self.validate_azure_parameters(&resource_name, &deployment_id) {
            error!("Azure parameter validation failed: {}", e);
            return format!("ERROR_VALIDATION_FAILED_{}", path);
        }
        
        // Construct the Azure URL now that we have all parameters
        match self.build_azure_url(&resource_name, &deployment_id, &api_version, path) {
            Ok(url) => {
                let url_string = url.to_string();
                debug!("Successfully constructed Azure URL in transform_path: {}", url_string);
                *self.constructed_url.write().unwrap() = Some(url_string);
                
                // For Azure, we need to return an empty string since the full URL 
                // is already constructed and stored in base_url()
                debug!("Azure URL construction completed, returning empty path");
                "".to_string()
            },
            Err(e) => {
                error!("Failed to construct Azure URL in transform_path: {}", e);
                format!("ERROR_INVALID_AZURE_CONFIG_{}", path)
            }
        }
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Azure OpenAI request headers");
        let mut headers = HeaderMap::new();

        // Log tracking headers for observability
        log_tracking_headers(original_headers);

        // Debug: Log all Azure-related headers for troubleshooting
        debug!("Azure headers received:");
        for (name, value) in original_headers.iter() {
            if name.as_str().starts_with("x-azure") || name.as_str() == "api-key" {
                debug!("  {}: {:?}", name, value);
            }
        }

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

        // Extract Azure-specific configuration (excluding deployment ID for now)
        let resource_name = self.extract_resource_name(original_headers);
        let api_version = self.extract_api_version(original_headers);
        
        debug!("Extracted from headers/env: resource_name='{}', api_version='{}'", resource_name, api_version);
        
        // For deployment ID, we prioritize the model from request body
        // Check if we already have a deployment ID from the request body
        let deployment_id = {
            // First check for header override
            if let Some(header_deployment) = original_headers
                .get("x-azure-deployment-id")
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string())
            {
                debug!("Using deployment ID from header (overrides body): {}", header_deployment);
                header_deployment
            } else {
                // Check if we have a model from the request body (stored by before_request or earlier)
                let stored_deployment = self.extracted_deployment_id.read().unwrap().clone();
                
                if !stored_deployment.is_empty() {
                    debug!("Using deployment ID from request body: {}", stored_deployment);
                    stored_deployment
                } else {
                    // If no deployment ID available yet, we'll defer validation
                    // The deployment ID should be extracted from the request body later
                    debug!("No deployment ID available yet, will be extracted from request body");
                    "".to_string()
                }
            }
        };

        debug!("Current Azure configuration: resource='{}', deployment='{}', api_version='{}'", 
            resource_name, deployment_id, api_version);

        // Only validate resource name at this point - deployment ID validation will happen later
        if resource_name.is_empty() {
            error!("Azure resource name is required but not provided in headers or environment");
            return Err(AppError::RequestError(
                "Azure resource name is required. Provide 'x-azure-resource-name' header or set AZURE_OPENAI_RESOURCE_NAME environment variable.".to_string()
            ));
        }

        // Store the extracted Azure configuration for later use
        *self.extracted_resource_name.write().unwrap() = resource_name.clone();
        *self.extracted_api_version.write().unwrap() = api_version.clone();
        
        // Only store deployment ID if we have one
        if !deployment_id.is_empty() {
            *self.extracted_deployment_id.write().unwrap() = deployment_id.clone();
        }
        
        debug!("Stored Azure configuration: resource={}, deployment={}, api_version={}", 
            resource_name, deployment_id, api_version);

        // Store Azure configuration in custom headers for later use
        headers.insert(
            "x-azure-resource-name-processed",
            http::header::HeaderValue::from_str(&resource_name).map_err(|_| AppError::InvalidHeader)?,
        );
        if !deployment_id.is_empty() {
            headers.insert(
                "x-azure-deployment-id-processed",
                http::header::HeaderValue::from_str(&deployment_id).map_err(|_| AppError::InvalidHeader)?,
            );
        }
        headers.insert(
            "x-azure-api-version-processed",
            http::header::HeaderValue::from_str(&api_version).map_err(|_| AppError::InvalidHeader)?,
        );

        Ok(headers)
    }

    async fn before_request(&self, _headers: &HeaderMap, body_bytes: &Bytes) -> Result<(), AppError> {
        debug!("Azure OpenAI before_request called with {} bytes", body_bytes.len());
        
        // Extract model from request body and use it as deployment ID
        // This allows for cleaner API where users only specify the model they want
        if let Some(model) = self.extract_model_from_body(body_bytes) {
            // Use the model name directly as the deployment name
            *self.extracted_deployment_id.write().unwrap() = model.clone();
            debug!("SUCCESS: Using model '{}' directly as deployment name for Azure OpenAI", model);
        } else {
            debug!("FAILURE: Could not extract model from request body for deployment ID");
        }
        
        Ok(())
    }

    async fn prepare_request_body(&self, body: Bytes) -> Result<Bytes, AppError> {
        debug!("Azure OpenAI prepare_request_body called with {} bytes", body.len());
        
        // Extract the model name from the body to determine if transformation is needed
        if let Some(model) = self.extract_model_from_body(&body) {
            debug!("Preparing request body for model: {}", model);
            self.transform_request_body(&body, &model)
        } else {
            debug!("Could not extract model from request body, using body as-is");
            Ok(body)
        }
    }

    async fn process_response(&self, response: Response<Body>) -> Result<Response<Body>, AppError> {
        debug!("Azure OpenAI process_response called");
        
        // Extract Azure request ID from response headers for telemetry
        if let Some(request_id) = response.headers().get("x-request-id")
            .and_then(|v| v.to_str().ok()) {
            debug!("Azure OpenAI request ID: {}", request_id);
            // Note: Request ID is available for logging/debugging
            // In a full implementation, this could be added to telemetry context
        } else {
            debug!("No Azure request ID found in response headers");
        }
        
        // Log other relevant Azure headers if present
        if let Some(content_type) = response.headers().get("content-type")
            .and_then(|v| v.to_str().ok()) {
            debug!("Azure OpenAI response content-type: {}", content_type);
        }
        
        // Check for rate limiting headers
        if let Some(remaining) = response.headers().get("x-ratelimit-remaining-requests")
            .and_then(|v| v.to_str().ok()) {
            debug!("Azure OpenAI remaining requests: {}", remaining);
        }
        
        Ok(response)
    }
}

/// Azure OpenAI-specific metrics extractor
///
/// Handles Azure-specific response format including content filtering results
/// and maintains compatibility with standard OpenAI metrics structure.
pub struct AzureOpenAIMetricsExtractor {
    /// Azure deployment ID for model mapping
    deployment_id: String,
}

impl AzureOpenAIMetricsExtractor {
    /// Create a new Azure OpenAI metrics extractor with deployment ID
    ///
    /// The deployment ID is used for accurate model name mapping when the response
    /// doesn't include a model field or when the model name needs to be normalized.
    pub fn new(deployment_id: String) -> Self {
        Self {
            deployment_id,
        }
    }

    /// Calculate Azure OpenAI cost based on model and token usage
    ///
    /// Azure OpenAI uses separate pricing for input and output tokens.
    /// Pricing is per 1K tokens and varies by model type.
    pub fn calculate_azure_cost(&self, model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
        pricing::calculate_cost(model, input_tokens, output_tokens)
    }

    /// Extract Azure-specific content filtering metrics
    ///
    /// Azure responses include content filtering results that should be captured for compliance
    fn extract_content_filtering_metrics(&self, response_body: &Value, _metrics: &mut ProviderMetrics) {
        debug!("Extracting Azure content filtering metrics");
        
        // Extract content filtering from choices (for completion responses)
        if let Some(choices) = response_body.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if let Some(content_filter) = choice.get("content_filter_results") {
                    debug!("Found content filtering results in choice: {:?}", content_filter);
                    // Content filtering data can be logged or added to custom metrics
                }
            }
        }
        
        // Extract prompt filtering results (for input validation)
        if let Some(prompt_filters) = response_body.get("prompt_filter_results").and_then(|p| p.as_array()) {
            for filter in prompt_filters {
                if let Some(content_filter) = filter.get("content_filter_results") {
                    debug!("Found prompt filtering results: {:?}", content_filter);
                    // Prompt filtering data can be logged or added to custom metrics
                }
            }
        }
    }
}

#[async_trait]
impl MetricsExtractor for AzureOpenAIMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        debug!("Extracting Azure OpenAI metrics from response: {:?}", response_body);
        
        // Get the actual model name using mapping
        let response_model = response_body
            .get("model")
            .and_then(|m| m.as_str());
        let mapped_model = model_mapping::map_deployment_to_model(&self.deployment_id, response_model);
        
        debug!("Using mapped model '{}' for metrics (deployment: '{}')", mapped_model, self.deployment_id);
        
        // Extract basic metrics
        let mut metrics = ProviderMetrics {
            model: mapped_model.clone(),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cost: None,
            request_id: response_body
                .get("id")
                .and_then(|id| id.as_str())
                .map(|s| s.to_string()),
            provider_latency: Duration::from_millis(0),
            project_id: None,
            organization_id: None,
            user_id: None,
            experiment_id: None,
        };
        
        // Extract token usage
        if let Some(usage) = response_body.get("usage") {
            metrics.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|t| t.as_u64())
                .map(|t| t as u32);
            
            metrics.output_tokens = usage
                .get("completion_tokens")
                .and_then(|t| t.as_u64())
                .map(|t| t as u32);
            
            metrics.total_tokens = usage
                .get("total_tokens")
                .and_then(|t| t.as_u64())
                .map(|t| t as u32);
            
            // Calculate cost if we have token counts
            if let (Some(input_tokens), Some(output_tokens)) = (metrics.input_tokens, metrics.output_tokens) {
                let cost = self.calculate_azure_cost(&mapped_model, input_tokens, output_tokens);
                if cost > 0.0 {
                    metrics.cost = Some(cost);
                    debug!("Calculated Azure cost: ${:.6} for model '{}'", cost, mapped_model);
                }
            } else {
                debug!("Cannot calculate cost: missing token counts");
            }
        } else {
            debug!("No usage information found in response");
        }
        
        // Extract Azure-specific content filtering metrics
        self.extract_content_filtering_metrics(response_body, &mut metrics);
        
        metrics
    }
    
    fn try_extract_provider_specific_streaming_metrics(&self, chunk: &str) -> Option<ProviderMetrics> {
        debug!("Extracting Azure OpenAI streaming metrics from chunk");
        
        // Parse the streaming chunk as JSON
        let chunk_json: Value = serde_json::from_str(chunk).ok()?;
        
        // Get the actual model name using mapping
        let response_model = chunk_json
            .get("model")
            .and_then(|m| m.as_str());
        let mapped_model = model_mapping::map_deployment_to_model(&self.deployment_id, response_model);
        
        debug!("Using mapped model '{}' for streaming metrics (deployment: '{}')", mapped_model, self.deployment_id);
        
        // Create basic metrics structure
        let mut metrics = ProviderMetrics {
            model: mapped_model.clone(),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cost: None,
            request_id: chunk_json
                .get("id")
                .and_then(|id| id.as_str())
                .map(|s| s.to_string()),
            provider_latency: Duration::from_millis(0),
            project_id: None,
            organization_id: None,
            user_id: None,
            experiment_id: None,
        };
        
        // Extract usage if present (usually only in the final chunk)
        if let Some(usage) = chunk_json.get("usage") {
            metrics.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|t| t.as_u64())
                .map(|t| t as u32);
            
            metrics.output_tokens = usage
                .get("completion_tokens")
                .and_then(|t| t.as_u64())
                .map(|t| t as u32);
            
            metrics.total_tokens = usage
                .get("total_tokens")
                .and_then(|t| t.as_u64())
                .map(|t| t as u32);
            
            // Calculate cost if we have token counts
            if let (Some(input_tokens), Some(output_tokens)) = (metrics.input_tokens, metrics.output_tokens) {
                let cost = self.calculate_azure_cost(&mapped_model, input_tokens, output_tokens);
                if cost > 0.0 {
                    metrics.cost = Some(cost);
                    debug!("Calculated Azure streaming cost: ${:.6} for model '{}'", cost, mapped_model);
                }
            }
        }
        
        // Extract Azure-specific content filtering metrics from streaming chunk
        self.extract_content_filtering_metrics(&chunk_json, &mut metrics);
        
        Some(metrics)
    }
} 