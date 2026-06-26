//! AWS Signature Version 4 signing — shared, but used only by the Cloudflare
//! Worker (wasm32).
//!
//! The native server signs Bedrock requests with `aws-sigv4` (which pulls
//! `ring`/`aws-lc` and does not build for `wasm32-unknown-unknown`). On the edge
//! we implement the SigV4 algorithm directly with pure-Rust `sha2` + `hmac`
//! (both build for wasm), which is simpler and more reliable than the async Web
//! Crypto `SubtleCrypto` API. The module is compiled on both targets so the
//! algorithm is unit-tested in native CI.
//!
//! Unlike the native path, this supports **temporary credentials**: an
//! `x-amz-security-token` is signed + sent when a session token is present.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// The headers a caller must attach to the outbound request after signing.
pub struct SignedHeaders {
    pub authorization: String,
    pub amz_date: String,
    pub security_token: Option<String>,
}

/// Compute the SigV4 `Authorization` header for a request.
///
/// `canonical_uri` must already be path-encoded (e.g. model ARNs with `/`
/// escaped to `%2F`). `query` is the canonical query string (empty for Bedrock
/// Converse). `amz_date` is `YYYYMMDDTHHMMSSZ`, `datestamp` is `YYYYMMDD`. The
/// signed headers are exactly `content-type;host;x-amz-date[;x-amz-security-token]`.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    method: &str,
    host: &str,
    canonical_uri: &str,
    query: &str,
    body: &[u8],
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    region: &str,
    service: &str,
    amz_date: &str,
    datestamp: &str,
) -> SignedHeaders {
    let payload_hash = sha256_hex(body);

    // Canonical headers MUST be sorted by lowercase name:
    // content-type < host < x-amz-date < x-amz-security-token.
    let mut canonical_headers =
        format!("content-type:application/json\nhost:{host}\nx-amz-date:{amz_date}\n");
    let mut signed_headers = String::from("content-type;host;x-amz-date");
    if let Some(token) = session_token {
        canonical_headers.push_str(&format!("x-amz-security-token:{token}\n"));
        signed_headers.push_str(";x-amz-security-token");
    }

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );

    let scope = format!("{datestamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    // Derive the signing key (HMAC chain) and sign.
    let k_date = hmac(format!("AWS4{secret_key}").as_bytes(), datestamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );

    SignedHeaders {
        authorization,
        amz_date: amz_date.to_string(),
        security_token: session_token.map(String::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_2() {
        // RFC 4231 §4.3 Test Case 2 — authoritative HMAC-SHA256 vector. Pins our
        // HMAC primitive (the SigV4 signing-key chain) to the reference output.
        let mac = hmac(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sha256_hex_of_empty_is_known() {
        // The universal SHA-256 of the empty string (used as the SigV4 payload
        // hash for empty bodies).
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sign_includes_security_token_when_present() {
        let out = sign(
            "POST",
            "bedrock-runtime.us-east-1.amazonaws.com",
            "/model/amazon.titan/converse",
            "",
            b"{}",
            "AKID",
            "secret",
            Some("session-token-123"),
            "us-east-1",
            "bedrock",
            "20240101T000000Z",
            "20240101",
        );
        assert!(out.authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKID/20240101/us-east-1/bedrock/aws4_request"
        ));
        assert!(out
            .authorization
            .contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token"));
        assert_eq!(out.security_token.as_deref(), Some("session-token-123"));
    }

    #[test]
    fn sign_is_deterministic_and_body_sensitive() {
        let args = |body: &'static [u8]| {
            sign(
                "POST",
                "bedrock-runtime.us-east-1.amazonaws.com",
                "/model/m/converse",
                "",
                body,
                "AKID",
                "secret",
                None,
                "us-east-1",
                "bedrock",
                "20240101T000000Z",
                "20240101",
            )
            .authorization
        };
        assert_eq!(args(b"{}"), args(b"{}"), "same inputs → same signature");
        assert_ne!(
            args(b"{}"),
            args(b"{\"a\":1}"),
            "body change → signature change"
        );
        assert!(!args(b"{}").contains("x-amz-security-token"));
    }
}
