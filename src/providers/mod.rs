use crate::error::AppError;
use async_trait::async_trait;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response},
};
use tracing::error;

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
        headers: &HeaderMap,
        _body: &[u8],
    ) -> Result<HeaderMap, AppError> {
        Ok(headers.clone())
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
pub mod bedrock;
pub mod fireworks;
pub mod groq;
pub mod openai;
pub mod openai_compatible;
pub mod together;
pub mod utils;

pub use anthropic::AnthropicProvider;
pub use bedrock::BedrockProvider;
pub use fireworks::FireworksProvider;
pub use groq::GroqProvider;
pub use openai::OpenAIProvider;
pub use openai_compatible::{OpenAICompatibleMetricsExtractor, OpenAICompatibleProvider};
pub use together::TogetherProvider;

/// Build a generic OpenAI-compatible provider by `(name, base_url, strip_v1)`.
/// Returns `None` if the name is not a known compatible provider.
///
/// `strip_v1 = true` removes the leading `/v1` from the request path before
/// appending it to `base_url` (for endpoints whose base already carries the
/// version segment, e.g. Gemini's `/v1beta/openai` and Perplexity's `/chat/...`).
fn openai_compatible(name: &str) -> Option<OpenAICompatibleProvider> {
    let (canonical, base, strip_v1): (&'static str, &'static str, bool) = match name {
        "mistral" => ("mistral", "https://api.mistral.ai", false),
        "deepseek" => ("deepseek", "https://api.deepseek.com", false),
        "xai" | "grok" => ("xai", "https://api.x.ai", false),
        "openrouter" => ("openrouter", "https://openrouter.ai/api", false),
        "perplexity" => ("perplexity", "https://api.perplexity.ai", true),
        // Gemini's OpenAI-compatibility endpoint (verified live).
        "google" | "gemini" => (
            "google",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            true,
        ),
        // Cohere's OpenAI-compatibility endpoint.
        "cohere" => ("cohere", "https://api.cohere.ai/compatibility/v1", true),
        _ => return None,
    };
    Some(OpenAICompatibleProvider::new(canonical, base, strip_v1))
}

/// Factory function to create provider instances.
pub fn create_provider(provider_name: &str) -> Result<Box<dyn Provider>, AppError> {
    match provider_name.to_lowercase().as_str() {
        "openai" => Ok(Box::new(OpenAIProvider::new())),
        "anthropic" => Ok(Box::new(AnthropicProvider::new())),
        "groq" => Ok(Box::new(GroqProvider::new())),
        "fireworks" => Ok(Box::new(FireworksProvider::new())),
        "together" => Ok(Box::new(TogetherProvider::new())),
        "bedrock" => Ok(Box::new(BedrockProvider::new())),
        other => {
            if let Some(p) = openai_compatible(other) {
                Ok(Box::new(p))
            } else {
                error!("Attempted to use unsupported provider: {}", other);
                Err(AppError::UnsupportedProvider)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_builds_all_known_providers() {
        for name in [
            "openai",
            "anthropic",
            "groq",
            "fireworks",
            "together",
            "bedrock",
            "mistral",
            "cohere",
            "google",
            "gemini",
            "deepseek",
            "xai",
            "grok",
            "openrouter",
            "perplexity",
        ] {
            assert!(
                create_provider(name).is_ok(),
                "failed to build provider {name}"
            );
        }
    }

    #[test]
    fn factory_is_case_insensitive() {
        assert!(create_provider("OpenAI").is_ok());
        assert!(create_provider("DeepSeek").is_ok());
        assert!(create_provider("GEMINI").is_ok());
    }

    #[test]
    fn factory_rejects_unknown() {
        assert!(matches!(
            create_provider("not-a-provider"),
            Err(AppError::UnsupportedProvider)
        ));
    }

    #[test]
    fn compat_base_urls_and_path_rewrite() {
        // Gemini compat: /v1/chat/completions -> /chat/completions on /v1beta/openai base.
        let g = openai_compatible("gemini").unwrap();
        assert_eq!(
            g.base_url(),
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
        assert_eq!(
            g.transform_path("/v1/chat/completions"),
            "/chat/completions"
        );

        // DeepSeek keeps /v1 (base has no version segment).
        let d = openai_compatible("deepseek").unwrap();
        assert_eq!(
            d.transform_path("/v1/chat/completions"),
            "/v1/chat/completions"
        );

        // Perplexity strips /v1 (uses /chat/completions).
        let p = openai_compatible("perplexity").unwrap();
        assert_eq!(
            p.transform_path("/v1/chat/completions"),
            "/chat/completions"
        );

        assert!(openai_compatible("nope").is_none());
    }
}
