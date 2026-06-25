use super::common::{
    init_test_env, run_non_streaming_test, run_streaming_test, ProviderTestConfig,
};
use dotenv::dotenv;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client,
};
use serde_json::{json, Value};
use std::env;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

// Helper function to generate a unique request ID for tracking
fn generate_request_id() -> String {
    format!("test-{}", Uuid::new_v4().to_string())
}

async fn search_elasticsearch(request_id: &str) -> Result<Value, reqwest::Error> {
    let es_url = env::var("ELASTICSEARCH_URL").expect("ELASTICSEARCH_URL must be set");
    let es_username =
        env::var("ELASTICSEARCH_USERNAME").expect("ELASTICSEARCH_USERNAME must be set");
    let es_password =
        env::var("ELASTICSEARCH_PASSWORD").expect("ELASTICSEARCH_PASSWORD must be set");
    let es_index = env::var("ELASTICSEARCH_INDEX").expect("ELASTICSEARCH_INDEX must be set");

    let client = Client::new();
    let search_url = format!("{}/{}/_search", es_url, es_index);

    let query = json!({
        "query": {
            "match": {
                "attributes.metadata.request_id.keyword": request_id
            }
        }
    });

    let response = client
        .post(&search_url)
        .basic_auth(es_username, Some(es_password))
        .json(&query)
        .send()
        .await?;

    response.json::<Value>().await
}

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
