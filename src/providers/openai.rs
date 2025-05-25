use super::Provider;
use super::utils::log_tracking_headers;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, error};

pub struct OpenAIProvider {
    base_url: String,
}

impl OpenAIProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.openai.com".to_string(),
        }
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing OpenAI request headers");
        let mut headers = HeaderMap::new();

        // Log tracking headers for observability
        log_tracking_headers(original_headers);

        // Add content type
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Process authentication
        if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Using provided authorization header");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for OpenAI request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}

// OpenAI-specific metrics extractor
pub struct OpenAIMetricsExtractor;

impl MetricsExtractor for OpenAIMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        debug!("Extracting OpenAI metrics from response: {}", response_body);
        let mut metrics = ProviderMetrics::default();
        
        if let Some(usage) = response_body.get("usage") {
            debug!("Found usage data: {:?}", usage);
            metrics.input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
            metrics.output_tokens = usage.get("completion_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
            metrics.total_tokens = usage.get("total_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
            debug!("Extracted tokens - input: {:?}, output: {:?}, total: {:?}", 
                metrics.input_tokens, metrics.output_tokens, metrics.total_tokens);
        }

        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            debug!("Found model: {}", model);
            metrics.model = model.to_string();
        }

        // Calculate cost if we have tokens
        let cost = if metrics.input_tokens.is_some() && metrics.output_tokens.is_some() {
            Some(calculate_cost(&metrics.model, metrics.input_tokens.unwrap_or(0), metrics.output_tokens.unwrap_or(0)))
        } else if metrics.total_tokens.is_some() {
            // Fallback: estimate 50/50 split for input/output when only total is available
            let total = metrics.total_tokens.unwrap_or(0);
            let estimated_input = total / 2;
            let estimated_output = total - estimated_input;
            Some(calculate_cost(&metrics.model, estimated_input, estimated_output))
        } else {
            None
        };

        metrics.cost = cost;
        
        if let Some(cost_value) = cost {
            debug!("Calculated cost: ${:.6} for model {} with input: {} tokens, output: {} tokens", 
                cost_value, metrics.model, 
                metrics.input_tokens.unwrap_or(0), 
                metrics.output_tokens.unwrap_or(0));
        }

        debug!("Final extracted metrics: {:?}", metrics);
        metrics
    }
    
    // Override with OpenAI-specific streaming metrics extraction
    fn try_extract_provider_specific_streaming_metrics(&self, chunk: &str) -> Option<ProviderMetrics> {
        debug!("Attempting to extract metrics from OpenAI streaming chunk: {}", chunk);
        if let Ok(json) = serde_json::from_str::<Value>(chunk) {
            // If we have usage data, extract full metrics
            if json.get("usage").is_some() {
                debug!("Found usage in OpenAI streaming chunk, extracting metrics");
                return Some(self.extract_metrics(&json));
            }
            
            // For OpenAI streaming, extract what we can even if usage is missing
            // This will handle the common case where OpenAI omits token counts in streaming
            let model = json.get("model").and_then(|m| m.as_str()).unwrap_or("unknown").to_string();
            
            if model.contains("gpt") || json.get("object").and_then(|o| o.as_str()).unwrap_or("") == "chat.completion.chunk" {
                debug!("OpenAI streaming response detected without usage data, creating partial metrics");
                return Some(ProviderMetrics {
                    model,
                    provider_latency: Duration::from_millis(0), // We can't determine this from chunks
                    // Leave token counts and cost as None
                    ..Default::default()
                });
            }
        }
        debug!("No usage data found in OpenAI streaming chunk");
        None
    }
}

// Helper function to calculate cost based on model and tokens with 2024 pricing
fn calculate_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    let (input_cost_per_1k, output_cost_per_1k) = match model {
        // GPT-4o models (latest flagship)
        m if m.contains("gpt-4o") && !m.contains("mini") => (0.005, 0.015), // $5/$15 per 1M tokens
        
        // GPT-4o-mini models (cost-effective)
        m if m.contains("gpt-4o-mini") => (0.0004, 0.0016), // $0.40/$1.60 per 1M tokens
        
        // GPT-4 Turbo models
        m if m.contains("gpt-4-turbo") || m.contains("gpt-4-1106") || m.contains("gpt-4-0125") => {
            (0.01, 0.03) // $10/$30 per 1M tokens
        },
        
        // GPT-4 32k models
        m if m.contains("gpt-4-32k") => (0.06, 0.12), // $60/$120 per 1M tokens
        
        // Standard GPT-4 models
        m if m.contains("gpt-4") => (0.03, 0.06), // $30/$60 per 1M tokens
        
        // GPT-3.5 Turbo models (current pricing)
        m if m.contains("gpt-3.5-turbo") => (0.0005, 0.0015), // $0.50/$1.50 per 1M tokens
        
        // O1 reasoning models (when available via OpenAI API)
        m if m.contains("o1-preview") => (0.015, 0.06), // $15/$60 per 1M tokens
        m if m.contains("o1-mini") => (0.003, 0.012), // $3/$12 per 1M tokens
        
        // Default fallback for unknown models
        _ => (0.0, 0.0),
    };
    
    let input_cost = (input_tokens as f64 / 1000.0) * input_cost_per_1k;
    let output_cost = (output_tokens as f64 / 1000.0) * output_cost_per_1k;
    input_cost + output_cost
}
