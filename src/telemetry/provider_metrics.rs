use axum::http::HeaderMap;
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

/// Metrics collected from an AI provider response
#[derive(Debug, Default, Clone)]
pub struct ProviderMetrics {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub cost: Option<f64>,
    pub model: String,
    pub provider_latency: Duration,
    pub request_id: Option<String>,
    pub project_id: Option<String>,
    pub organization_id: Option<String>,
    pub user_id: Option<String>,
    pub experiment_id: Option<String>,
}

impl ProviderMetrics {
    /// Estimates the number of tokens in a text string
    ///
    /// This is a simple estimation based on the assumption that one token is
    /// approximately 4 characters in English text. This is not perfect but
    /// provides a reasonable fallback when exact token counts are not available.
    ///
    /// # Arguments
    /// * `text` - The text to estimate token count for
    ///
    /// # Returns
    /// Estimated token count as u32
    pub fn estimate_tokens_from_text(text: &str) -> u32 {
        // Simple estimation based on average token length
        // This is a fallback when the provider doesn't give us token counts
        let char_count = text.chars().count();

        // Average English token is about 4 characters
        // Round up to ensure we don't underestimate
        let estimated_tokens = (char_count as f32 / 4.0).ceil() as u32;

        debug!(
            "Estimated {} tokens from {} characters",
            estimated_tokens, char_count
        );
        estimated_tokens
    }

    /// Whether a model id is a real identifier (vs. an extractor placeholder
    /// like `"claude"`, `"llama"`, or `"unknown"` used when a chunk carries no
    /// model field).
    fn is_placeholder_model(model: &str) -> bool {
        matches!(model, "" | "unknown" | "claude" | "llama")
    }

    /// Fold metrics extracted from a later streaming chunk into this running
    /// accumulation. Streaming providers spread metrics across events (e.g.
    /// Anthropic sends the model + input tokens in `message_start` and the
    /// output tokens in `message_delta`), so a later chunk must never wipe
    /// fields an earlier chunk already supplied.
    pub fn merge_streaming(&mut self, newer: ProviderMetrics) {
        if (!Self::is_placeholder_model(&newer.model) || Self::is_placeholder_model(&self.model))
            && !newer.model.is_empty()
        {
            self.model = newer.model;
        }
        self.input_tokens = newer.input_tokens.or(self.input_tokens);
        self.output_tokens = newer.output_tokens.or(self.output_tokens);
        self.total_tokens = newer.total_tokens.or(self.total_tokens);
        self.cost = newer.cost.or(self.cost);
        self.request_id = newer.request_id.or(self.request_id.take());
        self.project_id = newer.project_id.or(self.project_id.take());
        self.organization_id = newer.organization_id.or(self.organization_id.take());
        self.user_id = newer.user_id.or(self.user_id.take());
        self.experiment_id = newer.experiment_id.or(self.experiment_id.take());
    }

    /// Extract tracking headers from the original request headers
    pub fn extract_tracking_headers(headers: &HeaderMap) -> Self {
        let mut metrics = Self::default();

        // Extract project ID
        if let Some(project_id) = headers.get("x-project-id").and_then(|v| v.to_str().ok()) {
            debug!("Found x-project-id header: {}", project_id);
            metrics.project_id = Some(project_id.to_string());
        }

        // Extract organization ID - handle both British and American spellings
        if let Some(org_id) = headers
            .get("x-organization-id")
            .or_else(|| headers.get("x-organisation-id")) // Try both spellings
            .and_then(|v| v.to_str().ok())
        {
            debug!("Found organization ID header: {}", org_id);
            metrics.organization_id = Some(org_id.to_string());
        }

        // Extract user ID
        if let Some(user_id) = headers.get("x-user-id").and_then(|v| v.to_str().ok()) {
            debug!("Found x-user-id header: {}", user_id);
            metrics.user_id = Some(user_id.to_string());
        }

        // Extract experiment ID
        if let Some(experiment_id) = headers.get("x-experiment-id").and_then(|v| v.to_str().ok()) {
            debug!("Found x-experiment-id header: {}", experiment_id);
            metrics.experiment_id = Some(experiment_id.to_string());
        }

        metrics
    }
}

/// The MetricsExtractor trait defines a layered approach to extracting metrics from provider responses
///
/// This architecture provides several key benefits:
/// 1. **Layered Extraction**: Tries provider-specific extraction first, falls back to common patterns
/// 2. **Extensibility**: New providers can be added by implementing just the parts they need to customize
/// 3. **Safety Net**: Common patterns catch metrics when provider-specific logic fails
/// 4. **Maintainability**: Clear separation between provider-specific and common logic
///
/// ## How to add a new provider:
/// 1. Create a new struct for your provider: `struct MyProviderMetricsExtractor;`
/// 2. Implement the required `extract_metrics` method
/// 3. Optionally override `try_extract_provider_specific_streaming_metrics` if your provider
///    needs special handling for streaming responses
/// 4. Update the `get_metrics_extractor` factory function to return your implementation
///
/// The default implementation will handle common patterns automatically.
pub trait MetricsExtractor: Send + Sync {
    /// Extract complete metrics from a full response body
    ///
    /// This is the primary method each provider must implement to extract
    /// metrics from a complete response.
    ///
    /// # Arguments
    /// * `response_body` - The JSON response body from the provider
    ///
    /// # Returns
    /// A ProviderMetrics instance with extracted data
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics;

    /// Extract metrics from streaming chunks
    ///
    /// This method implements a layered approach:
    /// 1. First tries provider-specific extraction
    /// 2. Falls back to common extraction patterns if that returns None
    ///
    /// This layered approach allows new providers to work out-of-the-box
    /// while letting existing providers maintain their specialized logic.
    ///
    /// # Arguments
    /// * `chunk` - A string containing a streaming chunk from the provider
    ///
    /// # Returns
    /// `Option<ProviderMetrics>` with metrics if they could be extracted
    fn extract_streaming_metrics(&self, chunk: &str) -> Option<ProviderMetrics> {
        // First try provider-specific detection based on known patterns
        if let Some(metrics) = self.try_extract_provider_specific_streaming_metrics(chunk) {
            return Some(metrics);
        }

        // Then fall back to common extraction patterns as a safety net
        self.try_extract_common_streaming_metrics(chunk)
    }

    /// Provider-specific implementation for streaming metrics
    ///
    /// Override this method to implement custom extraction logic for a specific provider.
    /// The default implementation returns None, which causes the common extraction to be used.
    ///
    /// # Arguments
    /// * `chunk` - A string containing a streaming chunk from the provider
    ///
    /// # Returns
    /// `Option<ProviderMetrics>` with metrics if they could be extracted
    fn try_extract_provider_specific_streaming_metrics(
        &self,
        _chunk: &str,
    ) -> Option<ProviderMetrics> {
        None // Default is to skip provider-specific extraction
    }

    /// Generic implementation that works for most providers
    ///
    /// This provides a safety net for new providers or when specific extraction fails.
    /// It uses common patterns found across most LLM providers to identify and extract
    /// whatever metrics information is available.
    ///
    /// # Arguments
    /// * `chunk` - A string containing a streaming chunk from the provider
    ///
    /// # Returns
    /// `Option<ProviderMetrics>` with metrics if they could be extracted
    fn try_extract_common_streaming_metrics(&self, chunk: &str) -> Option<ProviderMetrics> {
        debug!("Attempting common streaming metrics extraction for chunk");

        // Try to parse the chunk as JSON
        if let Ok(json) = serde_json::from_str::<Value>(chunk) {
            // If we have usage data, extract full metrics
            if json.get("usage").is_some() {
                debug!("Found usage in streaming chunk, extracting metrics");
                return Some(self.extract_metrics(&json));
            }

            // For any provider's streaming, extract what we can even if usage is missing
            let model = json
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();

            // Check for general indicators that this is a model output
            let is_llm_response = json.get("choices").is_some()
                || json.get("completion").is_some()
                || json.get("delta").is_some()
                || json.get("finish_reason").is_some();

            if is_llm_response {
                debug!(
                    "LLM streaming response detected in common handler, creating partial metrics"
                );
                return Some(ProviderMetrics {
                    model,
                    provider_latency: Duration::from_millis(0),
                    // Leave token counts and cost as None
                    ..Default::default()
                });
            }
        }

        debug!("No metrics data found in common streaming handler");
        None
    }
}

// Factory function for creating metrics extractors
pub fn get_metrics_extractor(provider: &str) -> Box<dyn MetricsExtractor> {
    use crate::providers::anthropic::AnthropicMetricsExtractor;
    use crate::providers::bedrock::BedrockMetricsExtractor;
    use crate::providers::fireworks::FireworksMetricsExtractor;
    use crate::providers::groq::GroqMetricsExtractor;
    use crate::providers::openai::OpenAIMetricsExtractor;
    use crate::providers::openai_compatible::OpenAICompatibleMetricsExtractor;

    // Normalize the provider name (the `x-provider` header is forwarded as-is,
    // but provider construction lowercases it) so a header like `Gemini` or
    // `OpenRouter` dispatches to the right extractor instead of silently falling
    // back to the OpenAI one and skewing telemetry.
    match provider.to_lowercase().as_str() {
        "anthropic" => Box::new(AnthropicMetricsExtractor),
        "bedrock" => Box::new(BedrockMetricsExtractor),
        "groq" => Box::new(GroqMetricsExtractor),
        "fireworks" => Box::new(FireworksMetricsExtractor), // Fireworks-specific extractor
        // All generic OpenAI-compatible providers share one extractor that reads
        // the standard `usage` object and costs via the shared pricing table.
        "together" | "mistral" | "cohere" | "google" | "gemini" | "deepseek" | "xai" | "grok"
        | "openrouter" | "perplexity" => Box::new(OpenAICompatibleMetricsExtractor),
        _ => Box::new(OpenAIMetricsExtractor), // Default to OpenAI format
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_streaming_combines_fields_across_chunks() {
        // Anthropic shape: message_start carries model + input tokens, a later
        // message_delta carries output tokens under the placeholder model
        // "claude". The merge must keep the real model and both token counts.
        let mut acc = ProviderMetrics::default();
        acc.merge_streaming(ProviderMetrics {
            model: "claude-sonnet-5".to_string(),
            input_tokens: Some(13),
            ..Default::default()
        });
        acc.merge_streaming(ProviderMetrics {
            model: "claude".to_string(), // placeholder — must not clobber
            output_tokens: Some(4),
            ..Default::default()
        });
        assert_eq!(acc.model, "claude-sonnet-5");
        assert_eq!(acc.input_tokens, Some(13));
        assert_eq!(acc.output_tokens, Some(4));
    }

    #[test]
    fn merge_streaming_newer_values_win_but_none_does_not_erase() {
        let mut acc = ProviderMetrics {
            model: "gpt-4o".to_string(),
            input_tokens: Some(10),
            cost: Some(0.01),
            ..Default::default()
        };
        acc.merge_streaming(ProviderMetrics {
            model: "unknown".to_string(),
            input_tokens: Some(12), // a later, better value wins
            ..Default::default()
        });
        assert_eq!(acc.model, "gpt-4o");
        assert_eq!(acc.input_tokens, Some(12));
        assert_eq!(acc.cost, Some(0.01), "None must not erase a known cost");
    }
}
