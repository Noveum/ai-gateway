use super::common::{run_non_streaming_test, run_streaming_test, ProviderTestConfig};
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
async fn test_together_non_streaming() {
    let config = ProviderTestConfig::new(
        "together",
        "TOGETHER_API_KEY",
        "meta-llama/Llama-2-7b-chat-hf",
    )
    .with_max_tokens(512);
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_together_streaming() {
    let config = ProviderTestConfig::new(
        "together",
        "TOGETHER_API_KEY",
        "meta-llama/Llama-2-7b-chat-hf",
    )
    .with_max_tokens(512);
    run_streaming_test(&config).await;
}
