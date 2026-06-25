//! Policy bundle sources.
//!
//! The active [`PolicyBundle`] is loaded locally from:
//! * a local file (`NOVEUM_GUARD_POLICIES_FILE`) — for dev, CI, self-hosted, and
//!   air-gapped deployments; or
//! * an inline env var (`NOVEUM_GUARD_POLICIES`) — convenient for containers.
//!
//! Returns an empty (pass-through) bundle when neither is set. Policies can also
//! be replaced at runtime via [`crate::policy::PolicyEngine::swap_bundle`].

use super::config::PolicyBundle;

/// Load a bundle from environment configuration.
///
/// Precedence: `NOVEUM_GUARD_POLICIES_FILE` (a path) over `NOVEUM_GUARD_POLICIES`
/// (inline JSON). Returns an empty bundle when neither is set.
pub async fn load_from_env() -> Result<PolicyBundle, String> {
    if let Ok(path) = std::env::var("NOVEUM_GUARD_POLICIES_FILE") {
        if !path.is_empty() {
            return load_from_file(&path).await;
        }
    }
    if let Ok(inline) = std::env::var("NOVEUM_GUARD_POLICIES") {
        if !inline.trim().is_empty() {
            return PolicyBundle::from_json_str(&inline)
                .map_err(|e| format!("NOVEUM_GUARD_POLICIES is not valid JSON: {e}"));
        }
    }
    Ok(PolicyBundle::default())
}

/// Load a bundle from a JSON file path.
pub async fn load_from_file(path: &str) -> Result<PolicyBundle, String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("failed to read policy file '{path}': {e}"))?;
    PolicyBundle::from_json_slice(&bytes)
        .map_err(|e| format!("policy file '{path}' is not a valid nova-guard bundle: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_from_file_parses_bundle() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("nova-guard-test-{}.json", uuid::Uuid::new_v4()));
        let content = r#"{"policies":[{"name":"m","type":"model_allowlist","config":{"allowed":["gpt-4o"]}}]}"#;
        tokio::fs::write(&path, content).await.unwrap();

        let bundle = load_from_file(path.to_str().unwrap()).await.unwrap();
        assert_eq!(bundle.policies.len(), 1);
        tokio::fs::remove_file(&path).await.ok();
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let res = load_from_file("/nonexistent/path/nova-guard.json").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn invalid_json_file_errors() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("nova-guard-bad-{}.json", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, "{not json").await.unwrap();
        let res = load_from_file(path.to_str().unwrap()).await;
        assert!(res.is_err());
        tokio::fs::remove_file(&path).await.ok();
    }
}
