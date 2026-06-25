use super::RequestMetrics;
use async_trait::async_trait;
use std::error::Error;

#[async_trait]
pub trait TelemetryPlugin: Send + Sync {
    async fn export(&self, metrics: &RequestMetrics) -> Result<(), Box<dyn Error>>;
    fn name(&self) -> &str;
}

// Example plugin stubs (to be implemented later)
pub mod console;
pub mod elasticsearch;

pub use console::ConsolePlugin;

#[async_trait]
impl TelemetryPlugin for ConsolePlugin {
    async fn export(&self, metrics: &RequestMetrics) -> Result<(), Box<dyn Error>> {
        println!("Request Metrics:\n{:#?}", metrics);
        Ok(())
    }

    fn name(&self) -> &str {
        "console"
    }
}
