//! Telemetry plugins.
//!
//! A "plugin" is a [`crate::telemetry::metrics::MetricsExporter`] that ships
//! per-request metrics somewhere. The console plugin (debug logging) lives here;
//! add new exporters as `MetricsExporter` implementations and register them in
//! `main.rs`.

pub mod console;

// Reports ALLOWED usage to the Noveum platform (native-only; the reporter uses
// reqwest + a background tokio task).
#[cfg(not(target_arch = "wasm32"))]
pub mod nova_guard_usage;

pub use console::ConsolePlugin;
#[cfg(not(target_arch = "wasm32"))]
pub use nova_guard_usage::NovaGuardUsagePlugin;
