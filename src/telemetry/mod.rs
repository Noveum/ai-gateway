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

            Some(self.sanitize_json_for_export(response_value))
        } else {
            // For non-streaming responses, just include the response body
            self.response_body
                .clone()
                .map(|body| self.sanitize_json_for_export(body))
        };

        // Sanitize request body for export.
        let request_data = self
            .request_body
            .clone()
            .map(|body| self.sanitize_json_for_export(body));

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

    /// Serialize this request as a single Noveum-platform **trace** object,
    /// matching the `noveum-trace` Python SDK wire format so gateway traffic
    /// lands in the same project view as SDK traffic.
    ///
    /// The shape mirrors the SDK's `Trace.to_dict()` plus the transport layer's
    /// injected fields (`project`, `environment`, `sdk`): a trace with one span
    /// representing the LLM call, carrying `llm.*` attributes (model, usage, cost,
    /// finish/latency) so the Noveum UI parses it identically to SDK spans.
    /// `project` is the project id the trace is attributed to (the ingest API
    /// requires it). Wrap the result in `{ "traces": [...], "timestamp": ... }`.
    pub fn to_noveum_trace(&self, project: &str, environment: &str) -> Value {
        let now = chrono::Utc::now();
        let dur_ms = self.total_latency.as_millis() as u64;
        let start = now - chrono::Duration::milliseconds(dur_ms as i64);
        let start_s = start.to_rfc3339();
        let end_s = now.to_rfc3339();

        // A trace is an error if the gateway recorded an error, or if either the
        // gateway or the upstream provider returned a 4xx/5xx status. This keeps
        // the trace/span status honest for proxied provider failures (e.g. a
        // provider 404/429) that don't otherwise bump the error counters.
        let http_failed = self.status_code >= 400 || self.provider_status_code >= 400;
        let is_error = self.error_count > 0 || self.provider_error_count > 0 || http_failed;
        let status = if is_error { "error" } else { "ok" };
        let counted_errors = self.error_count + self.provider_error_count;
        let error_count = if is_error {
            counted_errors.max(1)
        } else {
            counted_errors
        };

        let trace_id = self
            .id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let span_id = Uuid::new_v4().to_string();

        // Span attributes follow the noveum-trace SDK `llm.*` convention so the
        // Noveum UI parses model/usage/cost identically to SDK-emitted spans.
        let mut attrs = serde_json::Map::new();
        attrs.insert("llm.provider".into(), json!(self.provider));
        attrs.insert("llm.model".into(), json!(self.model));
        attrs.insert("llm.operation".into(), json!("chat"));
        attrs.insert("llm.streaming".into(), json!(self.is_streaming));
        if let Some(t) = self.input_tokens {
            attrs.insert("llm.input_tokens".into(), json!(t));
            attrs.insert("llm.usage.input_tokens".into(), json!(t));
            attrs.insert("llm.usage.prompt_tokens".into(), json!(t));
        }
        if let Some(t) = self.output_tokens {
            attrs.insert("llm.output_tokens".into(), json!(t));
            attrs.insert("llm.usage.output_tokens".into(), json!(t));
            attrs.insert("llm.usage.completion_tokens".into(), json!(t));
        }
        if let Some(t) = self.total_tokens {
            attrs.insert("llm.total_tokens".into(), json!(t));
            attrs.insert("llm.usage.total_tokens".into(), json!(t));
        }
        if let Some(c) = self.cost {
            attrs.insert("llm.cost.total".into(), json!(c));
            attrs.insert("llm.cost.currency".into(), json!("USD"));
        }
        attrs.insert(
            "llm.latency_ms".into(),
            json!(self.provider_latency.as_millis()),
        );
        if self.ttfb.as_millis() > 0 {
            attrs.insert(
                "llm.time_to_first_token_ms".into(),
                json!(self.ttfb.as_millis()),
            );
        }
        if let Some(rid) = &self.provider_request_id {
            attrs.insert("llm.request_id".into(), json!(rid));
        }
        // Transport / HTTP attributes.
        attrs.insert("http.method".into(), json!(self.method));
        attrs.insert("http.route".into(), json!(self.path));
        attrs.insert("http.status_code".into(), json!(self.status_code));
        attrs.insert("http.request.size".into(), json!(self.request_size));
        attrs.insert("http.response.size".into(), json!(self.response_size));
        attrs.insert(
            "gateway.provider_status_code".into(),
            json!(self.provider_status_code),
        );
        if let Some(et) = &self.error_type {
            attrs.insert("error.type".into(), json!(et));
        }
        // Request / response payloads (sanitized to avoid deeply-nested content).
        if let Some(req) = &self.request_body {
            attrs.insert(
                "llm.request".into(),
                self.sanitize_json_for_export(req.clone()),
            );
        }
        if self.is_streaming && self.streamed_data.is_some() {
            let mut resp = self.response_body.clone().unwrap_or_else(|| json!({}));
            if let Some(chunks) = &self.streamed_data {
                resp["streamed_data"] = json!(chunks);
            }
            attrs.insert("llm.response".into(), self.sanitize_json_for_export(resp));
        } else if let Some(resp) = &self.response_body {
            attrs.insert(
                "llm.response".into(),
                self.sanitize_json_for_export(resp.clone()),
            );
        }

        let span = json!({
            "span_id": span_id,
            "trace_id": trace_id,
            "parent_span_id": null,
            "name": format!("{} {}", self.provider, self.model),
            "start_time": start_s,
            "end_time": end_s,
            "duration_ms": dur_ms,
            "status": status,
            "status_message": self.error_type,
            "attributes": Value::Object(attrs),
            "events": [],
            "links": [],
        });

        json!({
            "trace_id": trace_id,
            "name": format!("ai_gateway {} {}", self.provider, self.model),
            "start_time": start_s,
            "end_time": end_s,
            "duration_ms": dur_ms,
            "status": status,
            "status_message": self.error_type,
            "span_count": 1,
            "error_count": error_count,
            "project": project,
            "environment": environment,
            "sdk": { "name": "noveum-ai-gateway", "version": env!("CARGO_PKG_VERSION") },
            "attributes": {
                "llm.provider": self.provider,
                "llm.model": self.model,
                "service.name": "noveum-ai-gateway",
            },
            "metadata": {
                "user_id": self.user_id,
                "session_id": null,
                "request_id": self.provider_request_id,
                "tags": {},
                "custom_attributes": {
                    "org_id": self.org_id,
                    "experiment_id": self.experiment_id,
                    "thread_id": self.thread_id,
                },
            },
            "spans": [span],
        })
    }

    /// Sanitize complex JSON structures for export.
    /// Flattens nested message `content` objects to strings and normalizes error
    /// objects / large seed numbers so downstream stores (trace ingest, console)
    /// receive predictable shapes.
    fn sanitize_json_for_export(&self, mut value: Value) -> Value {
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

        // Handle error objects - convert to string to prevent type-mapping errors in downstream stores
        if let Some(error) = value.get_mut("error") {
            if error.is_object() || error.is_array() {
                let error_str = error.to_string();
                *error = json!(error_str);
                debug!(
                    "Converting error object to string for downstream-store compatibility: {}",
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

                        // Convert 'seed' to string to avoid integer overflow in downstream stores
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
                // Convert 'seed' to string to avoid integer overflow in downstream stores
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
