//! Telemetry plugins.
//!
//! A "plugin" is a [`crate::telemetry::metrics::MetricsExporter`] that ships
//! per-request metrics somewhere. The console plugin (debug logging) lives here;
//! the Noveum trace exporter lives in [`crate::telemetry::exporters`].

pub mod console;

pub use console::ConsolePlugin;
