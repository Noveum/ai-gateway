//! Integration tests for the AI Gateway.
//!
//! These tests make real requests to the AI providers via the gateway and verify
//! the proxied responses (status, OpenAI-compatible shape, token usage).
//!
//! # Running the tests
//!
//! To run the tests, you need to:
//!
//! 1. Start the AI Gateway locally: `cargo run`
//! 2. Create a `.env.test` file with your provider API keys
//! 3. Run the tests with: `cargo test --test run_integration_tests -- --nocapture`

pub mod anthropic_test;
pub mod bedrock_test;
pub mod common;
pub mod fireworks_test;
pub mod groq_test;
pub mod openai_test;
pub mod together_test;

// Add more provider test modules here as they are implemented
