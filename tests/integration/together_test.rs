use super::common::{run_non_streaming_test, run_streaming_test, ProviderTestConfig};

#[tokio::test]
async fn test_together_non_streaming() {
    let config = ProviderTestConfig::new(
        "together",
        "TOGETHER_API_KEY",
        "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    )
    .with_max_tokens(512);
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_together_streaming() {
    let config = ProviderTestConfig::new(
        "together",
        "TOGETHER_API_KEY",
        "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    )
    .with_max_tokens(512);
    run_streaming_test(&config).await;
}
