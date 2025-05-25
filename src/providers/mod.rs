use crate::error::AppError;
use async_trait::async_trait;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response},
};
use tracing::{debug, error};

#[async_trait]
pub trait Provider: Send + Sync {
    /// Get the base URL for the provider's API
    fn base_url(&self) -> String;

    /// Get the provider's name for logging and identification
    fn name(&self) -> &str;

    /// Transform the request path if needed
    fn transform_path(&self, path: &str) -> String {
        path.to_string()
    }

    /// Process and validate headers before sending request
    fn process_headers(&self, headers: &HeaderMap) -> Result<HeaderMap, AppError>;

    /// Transform request body if needed
    async fn prepare_request_body(&self, body: Bytes) -> Result<Bytes, AppError> {
        Ok(body)
    }

    /// Process response before returning to client
    async fn process_response(&self, response: Response<Body>) -> Result<Response<Body>, AppError> {
        Ok(response)
    }

    /// Sign the final request if needed
    async fn sign_request(
        &self,
        _method: &str,
        _url: &str,
        _headers: &HeaderMap,
        _body: &[u8],
    ) -> Result<HeaderMap, AppError> {
        // Default implementation returns the headers unchanged
        Ok(HeaderMap::new())
    }

    /// Process any operations needed before the request is sent
    async fn before_request(&self, _headers: &HeaderMap, _body: &Bytes) -> Result<(), AppError> {
        Ok(())
    }

    /// Check if the provider requires AWS signing
    fn requires_signing(&self) -> bool {
        false
    }

    /// Get AWS signing credentials if available
    fn get_signing_credentials(&self, _headers: &HeaderMap) -> Option<(String, String, String)> {
        None
    }

    /// Get the signing host for the provider
    fn get_signing_host(&self) -> String {
        self.base_url()
            .replace("https://", "")
            .replace("http://", "")
    }
}

// Use pub instead of mod to make the modules and their contents public
pub mod anthropic;
pub mod azure_openai;
pub mod bedrock;
pub mod fireworks;
pub mod groq;
pub mod openai;
pub mod together;
pub mod utils;

pub use anthropic::AnthropicProvider;
pub use azure_openai::AzureOpenAIProvider;
pub use bedrock::BedrockProvider;
pub use fireworks::FireworksProvider;
pub use groq::GroqProvider;
pub use openai::OpenAIProvider;
pub use together::TogetherProvider;

/// Factory function to create provider instances
pub fn create_provider(provider_name: &str) -> Result<Box<dyn Provider>, AppError> {
    match provider_name.to_lowercase().as_str() {
        "openai" => Ok(Box::new(OpenAIProvider::new())),
        "anthropic" => Ok(Box::new(AnthropicProvider::new())),
        "azure-openai" => {
            // Create Azure provider with relaxed validation for header-based configuration
            let config = azure_openai::AzureOpenAIProviderConfig::default()
                .allow_empty_resource_name(true)
                .allow_empty_deployment_id(true);
            Ok(Box::new(AzureOpenAIProvider::with_config(config)?))
        },
        "groq" => Ok(Box::new(GroqProvider::new())),
        "fireworks" => Ok(Box::new(FireworksProvider::new())),
        "together" => Ok(Box::new(TogetherProvider::new())),
        "bedrock" => Ok(Box::new(BedrockProvider::new())),
        unknown => {
            error!("Attempted to use unsupported provider: {}", unknown);
            Err(AppError::UnsupportedProvider)
        }
    }
}
