//! This module provides functionality for intercepting and mapping keys in HTTP requests.
//!
//! It includes a key fetcher that retrieves provider application keys, caches them, and
//! replaces the Authorization header in incoming requests with the fetched keys.
//!
//! # Constants
//!
//! * `CACHE_TTL` - The time-to-live duration for cached keys, set to 1 hour.
//!
//! # Static Variables
//!
//! * `APP_KEY_CACHE` - A lazy-initialized cache for storing provider application keys.
//!
//! # Structs
//!
//! * `ProviderAppKey` - Represents a provider application key with an optional expiration time.
//!
//! # Functions
//!
//! * `key_fetcher` - An asynchronous function that intercepts HTTP requests, fetches provider
//!   application keys, caches them, and replaces the Authorization header in the request.
//!
//! # Example
//!
//! ## HTTP Request
//!
//! ```http
//! GET /fetch_key/{some_app_key} HTTP/1.1
//! Host: 127.0.0.1:3000
//! ```
//!
//! ## HTTP Response
//!
//! ```http
//! HTTP/1.1 200 OK
//! Content-Type: application/json
//!
//! {
//!     "key": "key_for_some_app_key",
//!     "expires": 1633024800
//! }
//! ```

use std::{
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::extract::{Request, State};
use http::StatusCode;
use serde::Deserialize;
use timedmap::TimedMap;

use crate::config::AppConfig;

static APP_KEY_CACHE: LazyLock<TimedMap<String, String>> = LazyLock::new(TimedMap::new);
// Cache TTL is 1 hour
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// Represents a provider application key with an optional expiration time.
///
/// # Fields
///
/// * `key` - A string representing the key.
/// * `expires` - An optional expiration time in seconds since the Unix epoch,
///   e.g., the `exp` field in a JWT (JSON Web Token).
#[derive(Deserialize)]
pub struct ProviderAppKey {
    pub key: String,
    #[serde(default)]
    pub expires: Option<u64>,
}

pub(crate) async fn key_fetcher<B>(
    State(state): State<Arc<AppConfig>>,
    mut request: Request<B>,
) -> Result<Request<B>, StatusCode> {
    let fetcher = state.key_fetcher.as_deref().unwrap_or_default();
    if fetcher.is_empty() {
        return Ok(request);
    }
    let mut app_key = String::new();
    if let Some(auth_header) = request
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
    {
        if let Some(stripped) = auth_header.strip_prefix("Bearer ") {
            app_key.push_str(stripped);
        }
    }
    // If app key is empty, return request as is
    if app_key.is_empty() {
        return Ok(request);
    }

    // Fetch app key from cache
    let key = if let Some(key) = APP_KEY_CACHE.get(&app_key) {
        key
    } else {
        // Fetch app key via fetcher url with app_key appending.
        let token = reqwest::get(&format!("{}{}", fetcher, app_key))
            .await
            .map_err(|e| {
                if e.is_connect() {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            })?
            .json::<ProviderAppKey>()
            .await
            .map_err(|e| e.status().unwrap_or(StatusCode::BAD_GATEWAY))?;
        // expires or use default TTL
        let ttl = if let Some(exp) = token.expires {
            let expires = UNIX_EPOCH + Duration::from_secs(exp);
            expires
                .duration_since(SystemTime::now())
                .map(|e| e.min(CACHE_TTL))
                .map_err(|_| StatusCode::UNAUTHORIZED)?
        } else {
            CACHE_TTL
        };
        APP_KEY_CACHE.insert(app_key, token.key.clone(), ttl);
        token.key
    };
    // Replace Authorization header with app key
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", key)
            .parse()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );

    Ok(request)
}
