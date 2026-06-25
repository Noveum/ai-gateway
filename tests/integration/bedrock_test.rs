use super::common::{run_non_streaming_test, run_streaming_test, ProviderTestConfig};

#[tokio::test]
async fn test_bedrock_non_streaming() {
    // Use a model that's likely to be available to all AWS accounts with Bedrock access
    let config = ProviderTestConfig::new(
        "bedrock",
        "AWS_ACCESS_KEY_ID",
        "amazon.titan-text-express-v1",
    )
    .with_max_tokens(300);
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_bedrock_streaming() {
    // Use a model that's likely to be available to all AWS accounts with Bedrock access
    let config = ProviderTestConfig::new(
        "bedrock",
        "AWS_ACCESS_KEY_ID",
        "amazon.titan-text-express-v1",
    )
    .with_max_tokens(300);
    run_streaming_test(&config).await;
}
