use crate::error::AppError;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings};
use aws_sigv4::sign::v4;
use axum::http::HeaderMap;
use std::time::SystemTime;
use tracing::debug;

pub async fn sign_aws_request(
    method: &str,
    url: &str,
    body: &[u8],
    access_key: &str,
    secret_key: &str,
    region: &str,
    service: &str,
) -> Result<HeaderMap, AppError> {
    debug!("Signing request with method: {}, url: {}", method, url);

    // Create credentials
    let identity =
        Credentials::new(access_key, secret_key, None, None, "signing-credentials").into();

    // Create signing parameters
    let signing_settings = SigningSettings::default();
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()
        .map_err(|e| AppError::AwsParamsError(e.to_string()))?
        .into();

    // Create signable request with minimal required headers
    let signable_request = SignableRequest::new(
        method,
        url,
        vec![("Content-Type", "application/json")].into_iter(),
        SignableBody::Bytes(body),
    )
    .map_err(|e| AppError::AwsSigningError(e))?;

    // Sign the request
    let (signing_instructions, _signature) =
        aws_sigv4::http_request::sign(signable_request, &signing_params)
            .map_err(|e| AppError::AwsSigningError(e))?
            .into_parts();

    // Create a temporary request to apply signing instructions
    let mut temp_request = http::Request::builder()
        .method(method)
        .uri(url)
        .header("Content-Type", "application/json")
        .body(())
        .unwrap();

    // Apply signing instructions
    signing_instructions.apply_to_request_http1x(&mut temp_request);

    // Convert signed headers to HeaderMap
    let mut final_headers = HeaderMap::new();
    for (key, value) in temp_request.headers() {
        final_headers.insert(key.clone(), value.clone());
    }

    debug!("Final signed headers: {:?}", final_headers);
    Ok(final_headers)
}
