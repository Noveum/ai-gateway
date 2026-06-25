use super::RequestMetrics;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

#[async_trait]
pub trait MetricsExporter: Send + Sync {
    async fn export_metrics(
        &self,
        metrics: RequestMetrics,
    ) -> Result<(), Box<dyn std::error::Error>>;
    fn name(&self) -> &str;
}

pub struct MetricsRegistry {
    exporters: Arc<RwLock<Vec<Box<dyn MetricsExporter>>>>,
    debug_mode: bool,
}

impl MetricsRegistry {
    pub fn new(debug_mode: bool) -> Self {
        Self {
            exporters: Arc::new(RwLock::new(Vec::new())),
            debug_mode,
        }
    }

    pub async fn register_exporter(&self, exporter: Box<dyn MetricsExporter>) {
        let mut exporters = self.exporters.write().await;
        info!("Registering metrics exporter: {}", exporter.name());
        exporters.push(exporter);
    }

    pub async fn record_metrics(&self, metrics: RequestMetrics) {
        if self.debug_mode {
            debug!("Request Metrics: {:#?}", metrics);
        }

        // First, get all exporter names to process
        let exporter_names = {
            let exporters = self.exporters.read().await;
            exporters
                .iter()
                .map(|e| e.name().to_string())
                .collect::<Vec<_>>()
        };

        // Process each exporter by name, getting a fresh lock for each one
        for name in exporter_names {
            let metrics_clone = metrics.clone();
            let self_clone = self.exporters.clone();

            // Process each exporter in its own task to avoid holding locks
            tokio::spawn(async move {
                // Get a fresh lock on the exporters
                let exporters = self_clone.read().await;

                // Find the exporter with this name, if it still exists
                if let Some(exporter) = exporters.iter().find(|e| e.name() == name) {
                    if let Err(e) = exporter.export_metrics(metrics_clone).await {
                        error!("Failed to export metrics to {}: {}", name, e);
                    }
                }
            });
        }
    }
}
