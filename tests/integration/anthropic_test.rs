use super::common::{run_non_streaming_test, run_streaming_test, ProviderTestConfig};

#[tokio::test]
async fn test_anthropic_non_streaming() {
    let config = ProviderTestConfig::new("anthropic", "ANTHROPIC_API_KEY", "claude-sonnet-5")
        .with_max_tokens(300);
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_anthropic_streaming() {
    let config = ProviderTestConfig::new("anthropic", "ANTHROPIC_API_KEY", "claude-sonnet-5")
        .with_max_tokens(300);
    run_streaming_test(&config).await;
}
