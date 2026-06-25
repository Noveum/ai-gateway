use super::TelemetryPlugin;
use crate::telemetry::metrics::MetricsExporter;
use crate::telemetry::RequestMetrics;
use async_trait::async_trait;
use elasticsearch::{
    auth::Credentials,
    http::transport::{SingleNodeConnectionPool, TransportBuilder},
    Elasticsearch, IndexParts,
};
use opentelemetry::trace::TraceError;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::timeout;
use tokio_retry::{
    strategy::{jitter, ExponentialBackoff},
    Retry,
};
use tracing::{debug, error, info, warn};

pub struct ElasticsearchPlugin {
    client: Elasticsearch,
    index: String,
    requests_processed: AtomicUsize,
    docs_exported: AtomicUsize,
}

impl ElasticsearchPlugin {
    pub fn new(
        url: String,
        username: Option<String>,
        password: Option<String>,
        index: String,
    ) -> Result<Self, Box<dyn Error>> {
        let transport = match (username, password) {
            (Some(u), Some(p)) => {
                let credentials = Credentials::Basic(u, p);
                let conn_pool = SingleNodeConnectionPool::new(url.parse()?);
                let transport_builder = TransportBuilder::new(conn_pool).auth(credentials);
                transport_builder.build()?
            }
            _ => {
                let conn_pool = SingleNodeConnectionPool::new(url.parse()?);
                TransportBuilder::new(conn_pool).build()?
            }
        };

        info!(
            "Initialized Elasticsearch telemetry plugin for index: {}",
            index
        );

        Ok(Self {
            client: Elasticsearch::new(transport),
            index,
            requests_processed: AtomicUsize::new(0),
            docs_exported: AtomicUsize::new(0),
        })
    }

    // Enhanced send_metrics method with retries and better error handling
    async fn send_metrics(
        &self,
        document: serde_json::Value,
        request_id: &str,
    ) -> Result<(), TraceError> {
        // Configure retry strategy with exponential backoff
        let retry_strategy = ExponentialBackoff::from_millis(100)
            .map(jitter) // Add jitter to prevent thundering herd
            .take(3); // Max 3 retries

        // Log sending to Elasticsearch at debug level instead of info to reduce noise
        debug!(
            "Sending telemetry data to Elasticsearch, request_id: {}",
            request_id
        );

        let index = self.index.clone();
        let client = self.client.clone();
        let _request_id = request_id.to_string(); // Clone for use in async block

        let result = Retry::start(retry_strategy, || async {
            match timeout(
                Duration::from_secs(10),
                client
                    .index(IndexParts::Index(&index))
                    .body(document.clone())
                    .send(),
            )
            .await
            {
                Ok(response_result) => {
                    match response_result {
                        Ok(response) => {
                            if response.status_code().is_success() {
                                debug!("Successfully sent metrics to Elasticsearch");
                                Ok(())
                            } else {
                                let status = response.status_code();
                                let error_text = response
                                    .text()
                                    .await
                                    .unwrap_or_else(|_| "Unable to get response text".to_string());
                                warn!(
                                    "Elasticsearch returned non-success status: {}, response: {}",
                                    status, error_text
                                );

                                // Return error but retry for 5xx errors (server errors)
                                if status.as_u16() >= 500 && status.as_u16() < 600 {
                                    Err(TraceError::from(format!(
                                        "Server error from Elasticsearch: {}",
                                        status
                                    )))
                                } else {
                                    // Don't retry for 4xx errors (client errors)
                                    Err(TraceError::from(format!(
                                        "Client error from Elasticsearch: {}",
                                        status
                                    )))
                                }
                            }
                        }
                        Err(e) => {
                            let error_type = std::any::type_name_of_val(&e);
                            let is_connection_error = e.to_string().contains("connection")
                                || e.to_string().contains("network")
                                || e.to_string().contains("connect");

                            let is_timeout = e.to_string().contains("timeout")
                                || e.to_string().contains("timed out");

                            let error_category = if is_connection_error {
                                "CONNECTION"
                            } else if is_timeout {
                                "TIMEOUT"
                            } else {
                                "REQUEST"
                            };

                            warn!(
                                error_type = error_type,
                                error_category = error_category,
                                error_message = %e,
                                "Failed to send metrics to Elasticsearch: [{}] {} - {}",
                                error_category, error_type, e
                            );

                            Err(TraceError::from(format!(
                                "Failed to send metrics [{}]: {} - {}",
                                error_category, error_type, e
                            )))
                        }
                    }
                }
                Err(_) => {
                    warn!("Elasticsearch request timed out after 10 seconds");
                    Err(TraceError::from("Request to Elasticsearch timed out"))
                }
            }
        })
        .await;

        match result {
            Ok(_) => {
                let count = self.docs_exported.fetch_add(1, Ordering::Relaxed) + 1;

                // Only log count periodically, not every single successful export
                if count.is_multiple_of(100) {
                    info!(
                        "Elasticsearch telemetry: {} documents exported successfully",
                        count
                    );
                }
                Ok(())
            }
            Err(e) => {
                let error_str = e.to_string();
                let retry_exhausted = error_str.contains("retry") || error_str.contains("attempt");

                // Extract the original error if possible
                let original_error = if let Some(start) = error_str.find(':') {
                    error_str[start + 1..].trim()
                } else {
                    &error_str
                };

                // Determine if this is a connection, authentication, or other issue
                let error_category = if original_error.contains("connection")
                    || original_error.contains("network")
                    || original_error.contains("connect")
                {
                    "CONNECTION"
                } else if original_error.contains("unauthorized")
                    || original_error.contains("forbidden")
                    || original_error.contains("authentication")
                {
                    "AUTHENTICATION"
                } else if original_error.contains("timeout") || original_error.contains("timed out")
                {
                    "TIMEOUT"
                } else if original_error.contains("index") {
                    "INDEX"
                } else {
                    "UNKNOWN"
                };

                error!(
                    error_category = error_category,
                    retries_exhausted = retry_exhausted,
                    original_error = original_error,
                    "Failed to send metrics to Elasticsearch after retries: [{}] {}",
                    error_category,
                    original_error
                );

                Err(TraceError::from(format!(
                    "Failed to send metrics after retries [{}]: {}",
                    error_category, original_error
                )))
            }
        }
    }
}

#[async_trait]
impl TelemetryPlugin for ElasticsearchPlugin {
    async fn export(&self, metrics: &RequestMetrics) -> Result<(), Box<dyn Error>> {
        // Skip exporting health check requests to reduce noise
        if metrics.path == "/health" {
            debug!("Skipping telemetry export for health check request");
            return Ok(());
        }

        // Increment request counter and log periodically
        let req_count = self.requests_processed.fetch_add(1, Ordering::Relaxed) + 1;
        if req_count.is_multiple_of(500) {
            info!(
                "Elasticsearch telemetry plugin has processed {} requests",
                req_count
            );
        }

        // Convert metrics to OpenTelemetry format
        let document = metrics.to_otel_log();

        // Extract some key metrics for logging context
        let provider = document["attributes"]["provider"]
            .as_str()
            .unwrap_or("unknown");
        let model = document["attributes"]["model"]
            .as_str()
            .unwrap_or("unknown");
        let request_id = document["attributes"]["id"].as_str().unwrap_or("unknown");

        // Extract provider request ID if available
        let provider_request_id = document["attributes"]["metadata"]["provider_request_id"]
            .as_str()
            .unwrap_or(request_id);

        // We'll log at the beginning of the export but not both at beginning and end
        debug!(
            provider = provider,
            model = model,
            request_id = request_id,
            provider_request_id = provider_request_id,
            "Exporting metrics to Elasticsearch for request"
        );

        if let Err(e) = self
            .send_metrics(document.clone(), provider_request_id)
            .await
        {
            let error_message = e.to_string();
            let error_category = if error_message.contains("CONNECTION") {
                "CONNECTION"
            } else if error_message.contains("AUTHENTICATION") {
                "AUTHENTICATION"
            } else if error_message.contains("TIMEOUT") {
                "TIMEOUT"
            } else if error_message.contains("INDEX") {
                "INDEX"
            } else {
                "UNKNOWN"
            };

            error!(
                provider = provider,
                model = model,
                request_id = request_id,
                error_category = error_category,
                error_message = %e,
                "Failed to export metrics to Elasticsearch: [{}] {}",
                error_category, e
            );

            // Log a clear error message for operational visibility
            error!(
                "Elasticsearch telemetry export failed: {} - Check Elasticsearch connection",
                error_category
            );

            return Err(Box::new(e));
        }

        debug!(
            provider = provider,
            model = model,
            request_id = provider_request_id,
            "Successfully exported metrics to Elasticsearch"
        );

        Ok(())
    }

    fn name(&self) -> &str {
        "elasticsearch"
    }
}

#[async_trait]
impl MetricsExporter for ElasticsearchPlugin {
    async fn export_metrics(&self, metrics: RequestMetrics) -> Result<(), Box<dyn Error>> {
        // Skip exporting health check requests to reduce noise
        if metrics.path == "/health" {
            debug!("Skipping metrics export for health check request");
            return Ok(());
        }

        self.export(&metrics).await
    }

    fn name(&self) -> &str {
        "elasticsearch"
    }
}
