# AI Gateway Integration Tests

This directory contains integration tests for the AI Gateway that validate the functionality of the API endpoints and metrics collection.

## Test Structure

The tests are organized as follows:

- `run_integration_tests.rs` - Main entry point for running the integration tests
- `integration/` - Directory containing the integration test modules
  - `mod.rs` - Module definitions
  - `common.rs` - Common test utilities and shared test logic
  - `openai_test.rs` - Tests for the OpenAI provider
  - `anthropic_test.rs` - Tests for the Anthropic provider
  - Additional provider-specific test files

## Prerequisites

To run these tests, you need:

1. A running instance of the AI Gateway (either locally or in a development environment)
2. API keys for the providers you want to test
3. A configured ElasticSearch instance for metrics collection
4. A `.env.test` file with the required environment variables

## Environment Setup

### Test Environment vs Production Environment

The AI Gateway uses different environment files for different purposes:

- **Production**: Uses the standard `.env` file in the project root
- **Tests**: Uses a separate `.env.test` file to avoid conflicts with production settings

For running tests, creating a `.env.test` file is recommended to keep your test configuration separate from your production settings.

### Setting Up Test Environment

1. Copy the sample environment file and modify it with your actual keys:
   ```bash
   cp tests/.env.test.example .env.test
   ```

2. Edit the `.env.test` file to include your actual API keys and configuration:
   ```
   # Gateway URL (default: http://localhost:3000)
   GATEWAY_URL=http://localhost:3000

   # ElasticSearch Configuration
   ELASTICSEARCH_URL=http://localhost:9200
   ELASTICSEARCH_USERNAME=elastic
   ELASTICSEARCH_PASSWORD=your_password
   ELASTICSEARCH_INDEX=ai-gateway-metrics

   # Provider API Keys
   OPENAI_API_KEY=your_openai_api_key
   ANTHROPIC_API_KEY=your_anthropic_api_key
   GROQ_API_KEY=your_groq_api_key
   FIREWORKS_API_KEY=your_fireworks_api_key
   TOGETHER_API_KEY=your_together_api_key

   # AWS Bedrock Credentials
   AWS_ACCESS_KEY_ID=your_aws_access_key_id
   AWS_SECRET_ACCESS_KEY=your_aws_secret_access_key
   AWS_REGION=us-east-1
   ```

> **Note**: You can place the `.env.test` file either in the project root directory or in the `tests` directory. The tests will check both locations, prioritizing `.env.test` over the standard `.env` file.

## Provider-Specific Test Information

### AWS Bedrock

The AWS Bedrock integration test uses the Claude 3 Sonnet model (`anthropic.claude-3-sonnet-20240229-v1:0`) by default. Unlike other providers that use API keys, Bedrock uses AWS credentials for authentication:

- **AWS_ACCESS_KEY_ID**: Your AWS access key with Bedrock permissions
- **AWS_SECRET_ACCESS_KEY**: Your AWS secret key
- **AWS_REGION**: The AWS region where Bedrock is available (e.g., us-east-1)

The test validates that:
1. Request IDs are properly extracted from the AWS Bedrock response headers
2. Streaming and non-streaming modes work correctly
3. Token usage and metrics are properly captured in ElasticSearch

## Running the Tests

### Start the Gateway

First, start the AI Gateway in a separate terminal with ElasticSearch enabled:

```bash
# Start the gateway with ElasticSearch enabled
ENABLE_ELASTICSEARCH=true cargo run
```

### Using the Test Runner Script

The easiest way to run the tests is to use the provided script, which handles the setup and execution:

```bash
# Make the script executable
chmod +x tests/run_tests.sh

# Run all tests
./tests/run_tests.sh

# Run tests for a specific provider
./tests/run_tests.sh openai
./tests/run_tests.sh anthropic
```

This script will:
1. Check if a `.env.test` file exists and create one from the template if it doesn't
2. Run the specified tests with the proper configuration
3. Display helpful output about the test execution

### Running Tests Manually

If you prefer to run the tests manually:

```bash
# Run all integration tests
cargo test --test run_integration_tests -- --nocapture
```

The `--nocapture` flag ensures that test output (e.g., request/response details) is printed to the console, which is helpful for debugging.

### Running Integration Tests

1. Set up your test environment:
   ```bash
   # Copy the sample test environment file
   cp tests/.env.test.example .env.test
   
   # Edit the file to add your API keys for the providers you want to test
   nano .env.test
   ```

2. Start the gateway with ElasticSearch enabled:
   ```bash
   ENABLE_ELASTICSEARCH=true cargo run
   ```

3. Run the integration tests:
   ```bash
   # Run all tests
   cargo test --test run_integration_tests -- --nocapture
   
   # Run tests for specific providers
   cargo test --test run_integration_tests openai -- --nocapture
   cargo test --test run_integration_tests anthropic -- --nocapture
   cargo test --test run_integration_tests groq -- --nocapture
   cargo test --test run_integration_tests fireworks -- --nocapture
   cargo test --test run_integration_tests together -- --nocapture
   cargo test --test run_integration_tests bedrock -- --nocapture
   ```

## Debugging Test Failures

If you encounter test failures, check the following:

1. **Environment Variables**: Ensure your `.env.test` file exists and contains valid API keys for the providers you're testing. The test output will show which file was loaded.

2. **Gateway Status**: Make sure the AI Gateway is running with `ENABLE_ELASTICSEARCH=true`.

3. **ElasticSearch Setup**: Verify that ElasticSearch is properly configured and accessible.

4. **Console Output**: Look at the test output for detailed error messages, which often point to specific configuration issues.

5. **Environment File Not Found**: The tests will show which environment file was loaded. If you see "Warning: Neither .env.test nor .env files were found", you need to create one of these files with your test configuration.

## Extending the Tests

### Adding a New Provider Test

To add tests for a new provider:

1. Create a new test file in the `tests/integration/` directory, e.g., `groq_test.rs`
2. Use the common test module to implement the tests:

```rust
use super::common::{ProviderTestConfig, run_non_streaming_test, run_streaming_test};

#[tokio::test]
async fn test_groq_non_streaming() {
    let config = ProviderTestConfig::new("groq", "GROQ_API_KEY", "llama2-70b-4096");
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_groq_streaming() {
    let config = ProviderTestConfig::new("groq", "GROQ_API_KEY", "llama2-70b-4096");
    run_streaming_test(&config).await;
}
```

3. Add the new module to `tests/integration/mod.rs`:

```rust
pub mod common;
pub mod openai_test;
pub mod anthropic_test;
pub mod groq_test; // Add the new module here
```

## Troubleshooting

### Common Issues

1. **Gateway Not Running**: Ensure the AI Gateway is running and accessible at the URL specified in `.env.test`.

2. **Authentication Errors**: Make sure your API keys in `.env.test` are valid and have the necessary permissions.

3. **ElasticSearch Connectivity**: Verify that ElasticSearch is properly configured and accessible. Check the gateway logs for any connection errors.

4. **Test Failures**: The tests validate a variety of metrics and response fields. If tests fail, review the test output for details on which validation failed.

5. **Environment File Not Found**: The tests will show which environment file was loaded. If you see "Warning: Neither .env.test nor .env files were found", you need to create one of these files with your test configuration.

### Viewing Test Logs

To see detailed logs from the gateway during test execution, adjust the log level when starting the gateway:

```bash
RUST_LOG=debug ENABLE_ELASTICSEARCH=true cargo run
```

This will provide more information about request processing, metric extraction, and ElasticSearch integration.

## Adding Custom Test Cases

The common test module provides a flexible way to customize test cases. You can adjust the test parameters using the fluent interface provided by `ProviderTestConfig`:

```rust
let config = ProviderTestConfig::new("openai", "OPENAI_API_KEY", "gpt-4")
    .with_prompt("Explain quantum computing in simple terms")
    .with_max_tokens(200);
```

This allows you to test specific models or use cases with minimal code duplication. 