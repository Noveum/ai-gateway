# Changelog

All notable changes to MagicAPI AI Gateway will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Azure OpenAI provider support with full enterprise integration
  - Complete Azure-hosted OpenAI models support via `x-provider: azure-openai`
  - Enterprise-grade security and compliance features
  - Built-in Azure content filtering with detailed results in responses
  - Regional deployment options for data residency requirements
  - Azure-specific authentication using `api-key` header instead of `Authorization: Bearer`
  - Automatic deployment-to-model mapping for accurate metrics and cost tracking
  - Support for latest Azure OpenAI models including GPT-4.1, O3, O4 series
  - Automatic parameter transformation for newer models (max_tokens → max_completion_tokens)
  - Azure-specific error handling with detailed error categorization
  - Request ID tracking for Azure OpenAI debugging and telemetry
  - Cost calculation based on Azure OpenAI pricing model
- Comprehensive Azure OpenAI documentation in `docs/providers/azure-openai.md`
  - Configuration examples and usage patterns
  - Model support matrix and pricing information
  - Troubleshooting guide for common Azure-specific issues
  - SDK integration examples with Node.js OpenAI client

### Enhanced
- Provider architecture extended to support Azure-specific URL construction and authentication
- Telemetry system enhanced with Azure-specific metrics collection including content filtering results
- Error handling improved with Azure-specific error types and detailed error messages
- Improved ElasticSearch integration with more reliable data indexing
- Enhanced telemetry data formatting for better analytics
- Performance optimizations for high-volume request handling
- Code cleanup and formatting improvements throughout the codebase
- Documentation updates with clearer examples and instructions

### Fixed
- Connection handling issues with ElasticSearch during high load
- Memory leak in streaming response handling for long-running requests
- Inconsistent error reporting in provider-specific handlers
- Timeout issues with slow-responding upstream providers
- Race condition in concurrent request processing

## [1.0.1] - 2024-12-09
### Enhanced
- Improved ElasticSearch integration with more reliable data indexing
- Enhanced telemetry data formatting for better analytics
- Performance optimizations for high-volume request handling
- Code cleanup and formatting improvements throughout the codebase
- Documentation updates with clearer examples and instructions

### Fixed
- Connection handling issues with ElasticSearch during high load
- Memory leak in streaming response handling for long-running requests
- Inconsistent error reporting in provider-specific handlers
- Timeout issues with slow-responding upstream providers
- Race condition in concurrent request processing

## [1.0.0] - 2024-11-25
### Added
- Comprehensive telemetry system with robust metrics collection
- Elasticsearch integration for advanced log and metrics analysis
- Detailed request and response metrics tracking
- Integration tests for all supported providers
- Debug mode for local development and troubleshooting
- Plugin-based architecture for telemetry exporters (See [Telemetry Plugins Guide](docs/telemetry-plugins.md))
- OpenTelemetry compatible log format for standardized observability

### Enhanced
- Significantly improved logging system with structured logs
- Renamed project from MagicAPI to Noveum AI Gateway
- Optimized metrics middleware for performance monitoring
- Updated documentation to reflect new features and capabilities
- Console plugin for local metrics visualization
- Elasticsearch exporter for advanced analytics and visualization (See [Elasticsearch Integration Guide](docs/elasticsearch-integration.md))
- Complete token usage tracking with cost estimation
- Detailed performance metrics for each request including latency and TTFB
- Provider-specific metrics for better monitoring and analysis
- Thread-based optimizations for improved concurrency handling

### Fixed
- Various performance bottlenecks in request processing
- Inconsistencies in provider metrics reporting
- Resource management for long-running requests

## [0.2.0] - 2024-11-20
### Added
- AWS Bedrock models support via the same gateway, allowing access to streaming capabilities through OpenAI-compatible interfaces.
- AWS request signing functionality in `src/proxy/signing.rs` for secure API requests.
- Comprehensive documentation for AWS Bedrock Provider integration in `docs/providers/bedrock.md`.

### Enhanced
- Updated provider documentation to reflect new features and improvements.
- Significant performance improvements across the codebase, including optimizations in `Cargo.toml`.

### Fixed
- Various minor bug fixes and improvements in request handling and processing.

## [0.1.7] - 2024-11-13
### Added
- Managed deployment offering with testing gateway at gateway.noveum.ai
- Thread-based performance optimizations for improved request handling
- Documentation for testing deployment environment
### Enhanced
- Significant performance improvements in request processing
- Build system optimizations
- CI/CD pipeline improvements
### Fixed
- Git build configuration issues
- Various minor bug fixes

## [0.1.6] - 2024-11-13
### Added
- Support for Fireworks AI provider
  - Complete integration with Fireworks API
  - Streaming and non-streaming support
  - Model-specific optimizations
- Support for Together.ai provider
  - Full API integration
  - Support for all Together.ai models
  - Streaming capabilities
### Enhanced
- Documentation updates for new providers
- Example usage for all supported providers
- Performance optimizations for streaming responses

## [0.1.5] - 2024-11-13
### Added
- Anthropic Claude support with automatic path transformation
- Provider framework restructuring
- Unified provider interface with trait-based implementation
### Enhanced
- Provider-specific path transformations
- Header processing across all providers
- Authentication flow standardization
### Fixed
- Header processing for streaming responses
- Error handling for invalid API keys
- Provider-specific status code handling

## [0.1.4] - 2024-11-07
### Added
- Docker support with multi-stage builds
- Docker Compose configuration
### Enhanced
- Documentation improvements
- Build process optimization

## [0.1.3] - 2024-11-07
### Added
- GROQ provider support
- Native integration with GROQ's ultra-fast LLM API
### Enhanced
- Stream handling improvements
- Error message clarity
- Request timeout handling
### Fixed
- Stream handling edge cases
- Memory management for long-running streams

## [0.1.0] - 2024-11-07
### Added
- Initial release
- Basic provider framework
- OpenAI support
- Streaming capabilities
- Error handling
- Basic documentation

[Unreleased]: https://github.com/noveum/ai-gateway/compare/v1.0.1...HEAD
[1.0.1]: https://github.com/noveum/ai-gateway/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/noveum/ai-gateway/compare/v0.2.0...v1.0.0
[0.2.0]: https://github.com/noveum/ai-gateway/compare/v0.1.7...v0.2.0
[0.1.7]: https://github.com/noveum/ai-gateway/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/noveum/ai-gateway/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/noveum/ai-gateway/compare/v0.1.4...v0.1.5
[0.1.3]: https://github.com/noveum/ai-gateway/compare/v0.1.0...v0.1.3
