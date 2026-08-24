use super::common::{run_non_streaming_test, run_streaming_test, ProviderTestConfig};

#[tokio::test]
async fn test_bedrock_non_streaming() {
    // Verified against the release smoke-test AWS account on 2026-08-24.
    let config = ProviderTestConfig::new("bedrock", "AWS_ACCESS_KEY_ID", "amazon.nova-micro-v1:0")
        .with_model_from_env("BEDROCK_TEST_MODEL")
        .with_max_tokens(300);
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_bedrock_streaming() {
    // Native uses ConverseStream; the Cloudflare Worker intentionally rejects
    // Bedrock streaming until its AWS event-stream transport is implemented.
    let config = ProviderTestConfig::new("bedrock", "AWS_ACCESS_KEY_ID", "amazon.nova-micro-v1:0")
        .with_model_from_env("BEDROCK_TEST_MODEL")
        .with_max_tokens(300);
    run_streaming_test(&config).await;
}
