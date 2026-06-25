use super::common::{run_non_streaming_test, run_streaming_test, ProviderTestConfig};

#[tokio::test]
async fn test_fireworks_non_streaming() {
    let config = ProviderTestConfig::new(
        "fireworks",
        "FIREWORKS_API_KEY",
        "accounts/fireworks/models/llama-v3p2-11b-vision-instruct",
    )
    .with_max_tokens(300)
    .with_prompt("What is the history of AI?"); // Using a text-only prompt for non-streaming test
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_fireworks_streaming() {
    let config = ProviderTestConfig::new(
        "fireworks",
        "FIREWORKS_API_KEY",
        "accounts/fireworks/models/llama-v3p2-11b-vision-instruct",
    )
    .with_max_tokens(300)
    .with_prompt("What is the history of AI?"); // Using a text-only prompt for streaming test
    run_streaming_test(&config).await;
}
