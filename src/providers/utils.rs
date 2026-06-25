use axum::http::HeaderMap;
use tracing::debug;

/// List of tracking headers that should be preserved and logged
pub const TRACKING_HEADERS: [&str; 5] = [
    "x-project-id",
    "x-organization-id",
    "x-organisation-id", // British spelling
    "x-user-id",
    "x-experiment-id",
];

/// Utility function to log tracking headers for observability
///
/// This function should be called by all providers in their `process_headers`
/// implementation to ensure consistent handling of tracking headers.
///
/// # Arguments
/// * `headers` - The original request headers
pub fn log_tracking_headers(headers: &HeaderMap) {
    for header in &TRACKING_HEADERS {
        if let Some(value) = headers.get(*header).and_then(|h| h.to_str().ok()) {
            debug!("{}: {}", header, value);
        }
    }
}
