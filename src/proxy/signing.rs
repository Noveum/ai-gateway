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
    session_token: Option<&str>,
    region: &str,
    service: &str,
) -> Result<HeaderMap, AppError> {
    debug!("Signing request with method: {}, url: {}", method, url);

    // Create credentials
    let identity = Credentials::new(
        access_key,
        secret_key,
        session_token.map(str::to_owned),
        None,
        "signing-credentials",
    )
    .into();

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
    .map_err(AppError::AwsSigningError)?;

    // Sign the request
    let (signing_instructions, _signature) =
        aws_sigv4::http_request::sign(signable_request, &signing_params)
            .map_err(AppError::AwsSigningError)?
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

    // Never log signed header values: `authorization` is credential-derived and
    // temporary credentials add the raw `x-amz-security-token` value.
    debug!("AWS request signed with {} headers", final_headers.len());
    Ok(final_headers)
}

#[cfg(test)]
mod tests {
    use super::sign_aws_request;

    #[tokio::test]
    async fn temporary_credentials_sign_and_forward_the_security_token() {
        let headers = sign_aws_request(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/amazon.nova-micro-v1:0/converse",
            br#"{"inputText":"hello"}"#,
            "AKID",
            "secret",
            Some("session-token-123"),
            "us-east-1",
            "bedrock",
        )
        .await
        .unwrap();

        assert_eq!(
            headers
                .get("x-amz-security-token")
                .and_then(|value| value.to_str().ok()),
            Some("session-token-123")
        );
        assert!(headers.contains_key("authorization"));
        for raw_header in [
            "x-aws-access-key-id",
            "x-aws-secret-access-key",
            "x-aws-session-token",
        ] {
            assert!(
                !headers.contains_key(raw_header),
                "raw caller credential header leaked into the signed request: {raw_header}"
            );
        }
    }
}
