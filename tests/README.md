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
2. Credentials for all six current fixtures (OpenAI, Anthropic, Groq,
   Fireworks, Together, and Bedrock) in an unfiltered/full run, or the selected
   provider's credentials when using a provider-specific filter
3. Provider credentials supplied through the environment or an ignored
   `.env.test` file

## Environment Setup

### Test Environment vs Production Environment

The AI Gateway uses different environment files for different purposes:

- **Production**: Uses the standard `.env` file in the project root
- **Tests**: Uses a separate `.env.test` file to avoid conflicts with production settings

Prefer short-lived/development credentials exported by your shell or secret
manager. If you use `.env.test`, the repository ignores it; never copy a
production credential into source control, logs, issues, or pull requests.

### Setting Up Test Environment

1. Copy the sample environment file and restrict it to your user:
   ```bash
   cp tests/.env.test.example .env.test
   chmod 600 .env.test
   ```

2. Edit the `.env.test` file to include your actual API keys and configuration:
   ```
   # Gateway URL (default: http://localhost:3000)
   GATEWAY_URL=http://localhost:3000

   # Provider API Keys
   OPENAI_API_KEY=your_openai_api_key
   ANTHROPIC_API_KEY=your_anthropic_api_key
   GROQ_API_KEY=your_groq_api_key
   FIREWORKS_API_KEY=your_fireworks_api_key
   TOGETHER_API_KEY=your_together_api_key

   # AWS Bedrock Credentials
   AWS_ACCESS_KEY_ID=your_aws_access_key_id
   AWS_SECRET_ACCESS_KEY=your_aws_secret_access_key
   AWS_SESSION_TOKEN=your_temporary_session_token
   AWS_REGION=us-east-1
   ```

> **Note**: You can place the file at either `.env.test` in the project root or
> `tests/.env.test`. The root `.env.test` takes priority. The integration
> harness never reads the standard production `.env` file.

## Provider-Specific Test Information

### AWS Bedrock

The native Bedrock integration test uses the production-verified
`amazon.nova-micro-v1:0` by default. Unlike the other providers, Bedrock uses
AWS credentials. Prefer a short-lived STS session and include
`AWS_SESSION_TOKEN`. The native server validates buffered and ConverseStream
responses; the Cloudflare Worker is buffered-only in 2.0.1 and rejects
`stream:true` before admission or AWS.

- **AWS_ACCESS_KEY_ID**: Your AWS access key with Bedrock permissions
- **AWS_SECRET_ACCESS_KEY**: Your AWS secret key
- **AWS_SESSION_TOKEN**: Required when using temporary AWS credentials
- **AWS_REGION**: The AWS region where Bedrock is available (e.g., us-east-1)

The test validates that:
1. Request IDs are properly extracted from the AWS Bedrock response headers
2. Streaming and non-streaming modes work correctly
3. Token usage is present and correct in the gateway response

The checked-in provider defaults were verified on 2026-08-24. Override one
without editing source by setting `OPENAI_TEST_MODEL`, `ANTHROPIC_TEST_MODEL`,
`GROQ_TEST_MODEL`, `TOGETHER_TEST_MODEL`, `FIREWORKS_TEST_MODEL`, or
`BEDROCK_TEST_MODEL`. Empty variables retain the verified default.

## Running the Tests

### Start the Gateway

First, start the AI Gateway in a separate terminal:

```bash
cargo run --locked
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
cargo test --locked --test run_integration_tests -- --nocapture --test-threads=1
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

2. Start the gateway:
   ```bash
   cargo run --locked
   ```

3. Run the integration tests:
   ```bash
   # Run all tests
   cargo test --locked --test run_integration_tests -- --nocapture --test-threads=1
   
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

2. **Gateway Status**: Make sure the AI Gateway is running.

3. **Console Output**: Look at the test output for detailed error messages, which often point to specific configuration issues.

4. **Environment File Not Found**: If no `.env.test` is found, the tests use
   only variables already exported by the caller. Create an ignored
   `.env.test` or export the required values; the harness never falls back to
   the repository's production `.env`.

## Extending the Tests

### Adding a New Provider Test

To add tests for a new provider:

1. Create a new test file in the `tests/integration/` directory, e.g., `groq_test.rs`
2. Use the common test module to implement the tests:

```rust
use super::common::{ProviderTestConfig, run_non_streaming_test, run_streaming_test};

#[tokio::test]
async fn test_groq_non_streaming() {
    let config = ProviderTestConfig::new("groq", "GROQ_API_KEY", "openai/gpt-oss-20b");
    run_non_streaming_test(&config).await;
}

#[tokio::test]
async fn test_groq_streaming() {
    let config = ProviderTestConfig::new("groq", "GROQ_API_KEY", "openai/gpt-oss-20b");
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

3. **Test Failures**: The tests validate the gateway's proxied response (status, OpenAI-compatible shape, token usage). If tests fail, review the test output for details on which validation failed.

4. **Environment File Not Found**: If no `.env.test` is found, the tests use
   only variables already exported by the caller. Create an ignored
   `.env.test` or export the required values; the harness never falls back to
   the repository's production `.env`.

### Viewing Test Logs

To see detailed logs from the gateway during test execution, adjust the log level when starting the gateway:

```bash
RUST_LOG=debug cargo run
```

This will provide more information about request processing and metric extraction.

## Adding Custom Test Cases

The common test module provides a flexible way to customize test cases. You can adjust the test parameters using the fluent interface provided by `ProviderTestConfig`:

```rust
let config = ProviderTestConfig::new("openai", "OPENAI_API_KEY", "gpt-4o-mini")
    .with_model_from_env("OPENAI_TEST_MODEL")
    .with_prompt("Explain quantum computing in simple terms")
    .with_max_completion_tokens(200);
```

This allows you to test specific models or use cases with minimal code duplication.
