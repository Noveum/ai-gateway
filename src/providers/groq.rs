use super::Provider;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, error};

pub struct GroqProvider {
    base_url: String,
}

impl GroqProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.groq.com/openai".to_string(),
        }
    }
}

#[async_trait]
impl Provider for GroqProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "groq"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Groq request headers");
        let mut headers = HeaderMap::new();

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
            debug!("Using provided authorization header for Groq");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process Groq authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for Groq request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}

// Add Groq-specific metrics extractor
pub struct GroqMetricsExtractor;

impl MetricsExtractor for GroqMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        debug!("Extracting Groq metrics from response: {}", response_body);
        let mut metrics = ProviderMetrics::default();
        
        // Try to get metrics from x_groq field first
        if let Some(x_groq) = response_body.get("x_groq") {
            if let Some(usage) = x_groq.get("usage") {
                debug!("Found Groq usage data in x_groq: {:?}", usage);
                metrics.input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                metrics.output_tokens = usage.get("completion_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                metrics.total_tokens = usage.get("total_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                
                // Also capture provider latency from Groq's timing info
                if let Some(total_time) = usage.get("total_time").and_then(|v| v.as_f64()) {
                    metrics.provider_latency = Duration::from_secs_f64(total_time);
                }
            }
        }
        
        // If no metrics found in x_groq, try root level usage
        if metrics.total_tokens.is_none() {
            if let Some(usage) = response_body.get("usage") {
                debug!("Found Groq usage data at root level: {:?}", usage);
                metrics.input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                metrics.output_tokens = usage.get("completion_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                metrics.total_tokens = usage.get("total_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                
                // Also capture provider latency
                if let Some(total_time) = usage.get("total_time").and_then(|v| v.as_f64()) {
                    metrics.provider_latency = Duration::from_secs_f64(total_time);
                }
            }
        }

        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            debug!("Found Groq model: {}", model);
            metrics.model = model.to_string();
        }

        if let (Some(total_tokens), Some(model)) = (metrics.total_tokens, response_body.get("model")) {
            metrics.cost = Some(calculate_groq_cost(model.as_str().unwrap_or(""), total_tokens));
            debug!("Calculated Groq cost: {:?} for model {} and {} tokens", 
                metrics.cost, metrics.model, total_tokens);
        }

        debug!("Final extracted Groq metrics: {:?}", metrics);
        metrics
    }
    
    // Override with Groq-specific streaming metrics extraction
    fn try_extract_provider_specific_streaming_metrics(&self, chunk: &str) -> Option<ProviderMetrics> {
        debug!("Attempting to extract metrics from Groq streaming chunk");
        
        // First try parsing the chunk directly as JSON
        if let Ok(json) = serde_json::from_str::<Value>(chunk) {
            // Check if this is the final chunk with usage data
            if let Some(x_groq) = json.get("x_groq") {
                debug!("Found x_groq field in direct JSON: {}", json);
                if let Some(usage) = x_groq.get("usage") {
                    // Extract token counts from the usage data
                    let mut metrics = ProviderMetrics::default();
                    
                    metrics.input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                    metrics.output_tokens = usage.get("completion_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                    metrics.total_tokens = usage.get("total_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                    
                    // Capture provider latency from Groq's timing info if available
                    if let Some(total_time) = usage.get("total_time").and_then(|v| v.as_f64()) {
                        metrics.provider_latency = Duration::from_secs_f64(total_time);
                    }
                    
                    // Get the model name
                    if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                        metrics.model = model.to_string();
                    }
                    
                    // Calculate cost if we have total tokens and model
                    if let Some(total_tokens) = metrics.total_tokens {
                        metrics.cost = Some(calculate_groq_cost(&metrics.model, total_tokens));
                        debug!("Calculated Groq cost: {:?} for model {} and {} tokens", 
                            metrics.cost, metrics.model, total_tokens);
                    }
                    
                    debug!("Extracted complete Groq metrics from streaming chunk: {:?}", metrics);
                    return Some(metrics);
                }
            }
            
            // Check for final chunk with finish_reason: "stop"
            let is_final_chunk = json.get("choices")
                .and_then(|c| c.as_array())
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("finish_reason"))
                .and_then(|f| f.as_str())
                .map(|reason| reason == "stop")
                .unwrap_or(false);
                
            // Extract model information for any chunk
            let model = json.get("model").and_then(|v| v.as_str()).unwrap_or("llama").to_string();
            
            // Check if this is a Groq response
            let is_groq_response = 
                model.contains("llama") || 
                model.contains("gemma") || 
                json.get("object").and_then(|o| o.as_str()).map(|obj| obj == "chat.completion.chunk").unwrap_or(false);
                
            if is_groq_response {
                debug!("Groq streaming chunk detected for model: {}, is_final: {}", model, is_final_chunk);
                
                // Create metrics with available information
                return Some(ProviderMetrics {
                    model,
                    provider_latency: Duration::from_millis(0),
                    // Token counts and cost will be None until the final chunk with usage info
                    ..Default::default()
                });
            }
        }
        
        // Handle SSE format: split the chunk into lines and process each line
        for line in chunk.lines() {
            if !line.starts_with("data: ") {
                continue;
            }

            let json_str = line.trim_start_matches("data: ");
            if json_str == "[DONE]" {
                continue;
            }

            if let Ok(json) = serde_json::from_str::<Value>(json_str) {
                // Check if this is the final chunk with x_groq usage data
                if let Some(x_groq) = json.get("x_groq") {
                    debug!("Found x_groq field in SSE data: {}", json_str);
                    if let Some(usage) = x_groq.get("usage") {
                        // This is the final chunk with complete metrics
                        let mut metrics = ProviderMetrics::default();
                        
                        metrics.input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                        metrics.output_tokens = usage.get("completion_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                        metrics.total_tokens = usage.get("total_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);
                        
                        // Capture provider latency if available
                        if let Some(total_time) = usage.get("total_time").and_then(|v| v.as_f64()) {
                            metrics.provider_latency = Duration::from_secs_f64(total_time);
                        }
                        
                        // Get the model name
                        if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                            metrics.model = model.to_string();
                        }
                        
                        // Calculate cost if we have total tokens and model
                        if let Some(total_tokens) = metrics.total_tokens {
                            metrics.cost = Some(calculate_groq_cost(&metrics.model, total_tokens));
                        }
                        
                        debug!("Extracted complete Groq metrics from SSE streaming chunk: {:?}", metrics);
                        return Some(metrics);
                    }
                }
                
                // Extract model information for partial metrics
                let model = json.get("model").and_then(|v| v.as_str()).unwrap_or("llama").to_string();
                
                // Check if this is a Groq response
                let is_groq_response = 
                    model.contains("llama") || 
                    model.contains("gemma") || 
                    json.get("object").and_then(|o| o.as_str()).map(|obj| obj == "chat.completion.chunk").unwrap_or(false);
                    
                if is_groq_response {
                    // Create partial metrics with model information
                    debug!("Groq SSE streaming chunk detected for model: {}", model);
                    return Some(ProviderMetrics {
                        model,
                        provider_latency: Duration::from_millis(0),
                        // Token counts and cost will be filled in the final chunk
                        ..Default::default()
                    });
                }
            }
        }
        
        debug!("No usage data found in Groq streaming chunk");
        None
    }
}

// Helper function for Groq-specific cost calculation
fn calculate_groq_cost(model: &str, total_tokens: u32) -> f64 {
    let tokens = total_tokens as f64;
    
    match model {
        // Llama 3 models
        m if m.contains("llama-3") && m.contains("70b") => tokens * 0.0009,
        m if m.contains("llama-3") && m.contains("8b") => tokens * 0.0001,
        m if m.contains("llama-3.1") && m.contains("70b") => tokens * 0.0009,
        m if m.contains("llama-3.1") && m.contains("8b") => tokens * 0.0001,
        
        // Legacy Llama 2 models
        m if m.contains("llama-2") && m.contains("70b") => tokens * 0.0007,
        m if m.contains("llama-2") && m.contains("13b") => tokens * 0.0002,
        m if m.contains("llama-2") && m.contains("7b") => tokens * 0.0001,
        
        // Mixtral models
        m if m.contains("mixtral-8x7b") => tokens * 0.0002,
        m if m.contains("mixtral-8x22b") => tokens * 0.0006,
        
        // Gemma models
        m if m.contains("gemma") && m.contains("7b") => tokens * 0.0001,
        m if m.contains("gemma") && m.contains("27b") => tokens * 0.0004,
        
        // Generic fallbacks by model family
        m if m.contains("mixtral") => tokens * 0.0002,
        m if m.contains("llama") => tokens * 0.0001,
        m if m.contains("gemma") => tokens * 0.0001,
        
        // Default case - apply minimal cost to avoid zero cost which might mislead
        _ => {
            debug!("Unknown Groq model for cost calculation: {}", model);
            tokens * 0.0001
        },
    }
}
