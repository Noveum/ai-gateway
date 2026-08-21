use super::common::{run_non_streaming_test, run_streaming_test, ProviderTestConfig};

#[tokio::test]
async fn test_groq_non_streaming() {
    let config =
        ProviderTestConfig::new("groq", "GROQ_API_KEY", "openai/gpt-oss-20b").with_max_tokens(300);
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_groq_streaming() {
    let config =
        ProviderTestConfig::new("groq", "GROQ_API_KEY", "openai/gpt-oss-20b").with_max_tokens(300);
    run_streaming_test(&config).await;
}
