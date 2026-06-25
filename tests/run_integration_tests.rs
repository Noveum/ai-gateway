//! Main entry point for integration tests.
//! These tests need a running instance of the AI Gateway.
//!
//! # Environment Setup
//!
//! The tests expect the following environment variables to be set:
//!
//! ```
//! # Gateway URL (default: http://localhost:3000)
//! GATEWAY_URL=http://localhost:3000
//!
//! # Provider API Keys
//! OPENAI_API_KEY=your_openai_api_key
//! ANTHROPIC_API_KEY=your_anthropic_api_key
//! GROQ_API_KEY=your_groq_api_key
//! FIREWORKS_API_KEY=your_fireworks_api_key
//! TOGETHER_API_KEY=your_together_api_key
//!
//! # AWS Bedrock Credentials (if testing Bedrock)
//! AWS_ACCESS_KEY_ID=your_aws_access_key_id
//! AWS_SECRET_ACCESS_KEY=your_aws_secret_access_key
//! AWS_REGION=us-east-1
//! ```
//!
//! # Running the tests
//!
//! ```bash
//! # Start the gateway in one terminal
//! cargo run
//!
//! # In another terminal, run the tests
//! cargo test --test run_integration_tests -- --nocapture
//!
//! # Run tests for specific providers
//! cargo test --test run_integration_tests openai -- --nocapture
//! cargo test --test run_integration_tests anthropic -- --nocapture
//! cargo test --test run_integration_tests groq -- --nocapture
//! cargo test --test run_integration_tests fireworks -- --nocapture
//! cargo test --test run_integration_tests together -- --nocapture
//! cargo test --test run_integration_tests bedrock -- --nocapture
//! ```

mod integration;
