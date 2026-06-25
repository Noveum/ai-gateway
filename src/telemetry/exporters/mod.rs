//! Metrics exporters that ship `RequestMetrics` to external sinks.

pub mod noveum_trace;

pub use noveum_trace::NoveumTraceExporter;
