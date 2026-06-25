//! Telemetry plugins.
//!
//! A "plugin" is a [`crate::telemetry::metrics::MetricsExporter`] that ships
//! per-request metrics somewhere. The console plugin (debug logging) lives here;
//! add new exporters as `MetricsExporter` implementations and register them in
//! `main.rs`.

pub mod console;

pub use console::ConsolePlugin;
