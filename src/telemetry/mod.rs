pub mod exporters;
pub mod metrics;
pub mod middleware;
pub mod plugins;
pub mod provider_metrics;

pub use self::{metrics::MetricsRegistry, middleware::metrics_middleware, plugins::ConsolePlugin};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::debug;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInfo {
    #[serde(rename = "service.name")]
    pub service_name: String,
    #[serde(rename = "service.version")]
    pub service_version: String,
    #[serde(rename = "deployment.environment")]
    pub deployment_environment: String,
}

impl Default for ResourceInfo {
    fn default() -> Self {
        Self {
            service_name: "noveum_ai_gateway".to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            deployment_environment: std::env::var("DEPLOYMENT_ENVIRONMENT")
                .unwrap_or_else(|_| "development".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogAttributes {
    // Basic identifying fields
    pub id: String,
    pub thread_id: String,
    pub org_id: Option<String>,
    pub user_id: Option<String>,
    pub project_id: Option<String>,
    pub experiment_id: Option<String>,

    // Provider/model details
    pub provider: String,
    pub model: String,

    // Request/Response objects (can be stored as JSON Value)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,

    // Metadata
    pub metadata: LogMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogMetadata {
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub latency: u128,
    pub ttfb: u128, // Time to First Byte in milliseconds
    pub tokens: TokenInfo,
    pub cost: Option<f64>,
    pub status: String,
    pub path: String,
    pub method: String,
    pub request_size: usize,
    pub response_size: usize,
    pub provider_latency: u128,
    pub status_code: u16,
    pub provider_status_code: u16,
    pub error_count: u32,
    pub error_type: Option<String>,
    pub provider_error_count: u32,
    pub provider_error_type: Option<String>,
    pub provider_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub input: Option<u32>,
    pub output: Option<u32>,
    pub total: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RequestMetrics {
    // Request metadata
    pub provider: String,
    pub model: String,
    pub path: String,
    pub method: String,

    // Timing metrics
    pub total_latency: Duration,
    pub provider_latency: Duration,
    pub ttfb: Duration, // Time to First Byte - time taken to receive the first byte of the response

    // Size metrics
    pub request_size: usize,
    pub response_size: usize,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,

    // Status metrics
    pub status_code: u16,
    pub provider_status_code: u16,

    // Error metrics
    pub error_count: u32,
    pub error_type: Option<String>,
    pub provider_error_count: u32,
    pub provider_error_type: Option<String>,

    // Cost metrics
    pub cost: Option<f64>,

    // OpenTelemetry additional fields
    pub id: Option<String>,
    pub thread_id: Option<String>,
    pub org_id: Option<String>,
    pub user_id: Option<String>,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub provider_request_id: Option<String>,
    pub experiment_id: Option<String>,

    // Original request and response
    pub request_body: Option<Value>,
    pub response_body: Option<Value>,

    // Streaming response data
    pub streamed_data: Option<Vec<Value>>,
    pub is_streaming: bool,
}

impl RequestMetrics {
    /// Convert to OpenTelemetry compatible log format
    pub fn to_otel_log(&self) -> serde_json::Value {
        let status = if self.error_count > 0 || self.provider_error_count > 0 {
            "error"
        } else {
            "success"
        };

        let token_info = TokenInfo {
            input: self.input_tokens,
            output: self.output_tokens,
            total: self.total_tokens,
        };

        let metadata = LogMetadata {
            project_id: self.project_id.clone(),
            project_name: self.project_name.clone(),
            latency: self.total_latency.as_millis(),
            ttfb: self.ttfb.as_millis(),
            tokens: token_info,
            cost: self.cost,
            status: status.to_string(),
            path: self.path.clone(),
            method: self.method.clone(),
            request_size: self.request_size,
            response_size: self.response_size,
            provider_latency: self.provider_latency.as_millis(),
            status_code: self.status_code,
            provider_status_code: self.provider_status_code,
            error_count: self.error_count,
            error_type: self.error_type.clone(),
            provider_error_count: self.provider_error_count,
            provider_error_type: self.provider_error_type.clone(),
            provider_request_id: self.provider_request_id.clone(),
        };

        // Prepare the response data based on whether it's streaming or not
        let response_data = if self.is_streaming && self.streamed_data.is_some() {
            // For streaming responses, include both the final response and the streamed chunks
            let mut response_value = self.response_body.clone().unwrap_or(json!({}));

            // Add streamed_data field to the response
            if let Some(streamed_chunks) = &self.streamed_data {
                response_value["streamed_data"] = json!(streamed_chunks);
            }

            Some(self.sanitize_for_elasticsearch(response_value))
        } else {
            // For non-streaming responses, just include the response body
            self.response_body
                .clone()
                .map(|body| self.sanitize_for_elasticsearch(body))
        };

        // Sanitize request body for Elasticsearch
        let request_data = self
            .request_body
            .clone()
            .map(|body| self.sanitize_for_elasticsearch(body));

        let attributes = LogAttributes {
            id: self.id.clone().unwrap_or_else(|| {
                format!(
                    "msg_{}",
                    Uuid::new_v4()
                        .to_string()
                        .split('-')
                        .next()
                        .unwrap_or("unknown")
                )
            }),
            thread_id: self.thread_id.clone().unwrap_or_else(|| {
                format!(
                    "thread_{}",
                    Uuid::new_v4()
                        .to_string()
                        .split('-')
                        .next()
                        .unwrap_or("unknown")
                )
            }),
            org_id: self.org_id.clone(),
            user_id: self.user_id.clone(),
            project_id: self.project_id.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            request: request_data,
            response: response_data,
            metadata,
            experiment_id: self.experiment_id.clone(),
        };

        let resource = ResourceInfo::default();

        json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "resource": resource,
            "name": "ai_gateway_request_log",
            "attributes": attributes
        })
    }

    /// Sanitize complex JSON structures for Elasticsearch
    /// This converts message content objects to strings to prevent mapping errors
    fn sanitize_for_elasticsearch(&self, mut value: Value) -> Value {
        // Handle the case where we have a message array with complex content
        if let Some(messages) = value.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for message in messages {
                if let Some(content) = message.get_mut("content") {
                    // If content is an object, convert it to a string representation
                    if content.is_object() || content.is_array() {
                        let content_str = content.to_string();
                        *content = json!(content_str);
                    }
                }
            }
        }

        // Handle error objects - convert to string to prevent Elasticsearch mapping errors
        if let Some(error) = value.get_mut("error") {
            if error.is_object() || error.is_array() {
                let error_str = error.to_string();
                *error = json!(error_str);
                debug!(
                    "Converting error object to string for Elasticsearch compatibility: {}",
                    error_str
                );
            }
        }

        // Check for error objects in common nested structures (like data.error)
        if let Some(data) = value.get_mut("data") {
            if let Some(error) = data.get_mut("error") {
                if error.is_object() || error.is_array() {
                    let error_str = error.to_string();
                    *error = json!(error_str);
                    debug!(
                        "Converting nested data.error object to string: {}",
                        error_str
                    );
                }
            }
        }

        // Handle nested content in streamed_data field
        if let Some(streamed_data) = value
            .get_mut("streamed_data")
            .and_then(|sd| sd.as_array_mut())
        {
            for chunk in streamed_data {
                if let Some(choices) = chunk.get_mut("choices").and_then(|c| c.as_array_mut()) {
                    for choice in choices {
                        if let Some(delta) = choice.get_mut("delta") {
                            if let Some(content) = delta.get_mut("content") {
                                if content.is_object() || content.is_array() {
                                    let content_str = content.to_string();
                                    *content = json!(content_str);
                                }
                            }
                        }

                        // Convert 'seed' to string to avoid Elasticsearch long integer overflow
                        if let Some(seed) = choice.get_mut("seed") {
                            if seed.is_number() {
                                let seed_str = seed.to_string();
                                *seed = json!(seed_str);
                            }
                        }
                    }
                }

                // Handle error objects in streamed data
                if let Some(error) = chunk.get_mut("error") {
                    if error.is_object() || error.is_array() {
                        let error_str = error.to_string();
                        *error = json!(error_str);
                    }
                }
            }
        }

        // Handle root-level choices array
        if let Some(choices) = value.get_mut("choices").and_then(|c| c.as_array_mut()) {
            for choice in choices {
                // Convert 'seed' to string to avoid Elasticsearch long integer overflow
                if let Some(seed) = choice.get_mut("seed") {
                    if seed.is_number() {
                        let seed_str = seed.to_string();
                        *seed = json!(seed_str);
                    }
                }
            }
        }

        value
    }
}
