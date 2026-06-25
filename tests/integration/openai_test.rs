use super::common::{run_non_streaming_test, run_streaming_test, ProviderTestConfig};

#[tokio::test]
async fn test_openai_non_streaming() {
    let config = ProviderTestConfig::new("openai", "OPENAI_API_KEY", "gpt-3.5-turbo");
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_openai_streaming() {
    let config = ProviderTestConfig::new("openai", "OPENAI_API_KEY", "gpt-3.5-turbo");
    run_streaming_test(&config).await;
}
