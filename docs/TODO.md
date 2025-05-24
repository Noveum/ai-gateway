# Product Requirements Document: Azure OpenAI Provider Support

## Executive Summary

This PRD outlines the requirements for adding Azure OpenAI as a supported provider to the Noveum AI Gateway. Azure OpenAI offers enterprise-grade OpenAI models with enhanced security, compliance, and regional deployment options. Adding this provider will allow users to seamlessly route requests to Azure-hosted OpenAI models through our unified gateway interface.

## Background

The Noveum AI Gateway currently supports multiple AI providers (OpenAI, Anthropic, GROQ, Fireworks, Together AI, and AWS Bedrock). Each provider follows a consistent pattern using the Provider trait and includes metrics extraction, authentication handling, and response processing.

Azure OpenAI differs from standard OpenAI in several key areas:
- URL structure includes resource name and deployment ID
- Authentication uses `api-key` header instead of `Authorization: Bearer`
- Requires API version parameter
- Responses include Azure-specific content filtering results
- Model mapping between deployment names and actual models

## Functional Requirements

### FR-1: Provider Implementation
**Priority: High**

The system SHALL implement an Azure OpenAI provider that:
- Follows the existing Provider trait pattern established in the codebase
- Supports all chat completion endpoints
- Handles both streaming and non-streaming responses
- Integrates with the existing telemetry and metrics infrastructure

**Acceptance Criteria:**
- [ ] Create `azure_openai.rs` file in `src/providers/` directory
- [ ] Implement Provider trait with Azure-specific URL handling
- [ ] Register provider in `src/providers/mod.rs`
- [ ] Provider responds to `x-provider: azure-openai` header

### FR-2: Authentication & Headers
**Priority: High**

The system SHALL support Azure OpenAI authentication:
- Accept `api-key` header for authentication
- Support optional `x-azure-resource-name` header for resource specification
- Support optional `x-azure-deployment-id` header for deployment specification
- Support optional `x-azure-api-version` header for API version control

**Acceptance Criteria:**
- [ ] Process `api-key` header and convert to Azure format
- [ ] Extract Azure-specific headers from request
- [ ] Default to configurable resource name/deployment if headers not provided
- [ ] Validate required headers and return appropriate errors

### FR-3: URL Construction
**Priority: High**

The system SHALL construct proper Azure OpenAI URLs:
- Format: `https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}`
- Support configurable default resource name, deployment, and API version
- Allow override via headers

**Acceptance Criteria:**
- [ ] Dynamic URL construction based on headers or defaults
- [ ] Support for multiple Azure regions
- [ ] Proper URL encoding and validation
- [ ] Error handling for malformed URLs

### FR-4: Request Transformation
**Priority: Medium**

The system SHALL transform requests appropriately:
- Preserve OpenAI-compatible request format
- Add required Azure API version parameter
- Handle model name mapping (deployment name vs actual model)

**Acceptance Criteria:**
- [ ] Request body passes through unchanged
- [ ] API version parameter added to URL
- [ ] Proper content-type headers maintained
- [ ] Support for all OpenAI parameters (temperature, max_tokens, etc.)

### FR-5: Response Processing
**Priority: Medium**

The system SHALL process Azure OpenAI responses:
- Handle Azure-specific fields (`content_filter_results`, `prompt_filter_results`)
- Extract standard metrics (tokens, model, etc.)
- Preserve all response fields for client compatibility

**Acceptance Criteria:**
- [ ] Parse usage statistics correctly
- [ ] Extract model information from response
- [ ] Handle content filtering results appropriately
- [ ] Maintain compatibility with existing metrics extraction

### FR-6: Metrics & Telemetry
**Priority: Medium**

The system SHALL integrate with existing telemetry:
- Extract token usage from Azure responses
- Calculate costs based on Azure pricing models
- Track Azure-specific request IDs
- Support all existing tracking headers (project-id, org-id, user-id)

**Acceptance Criteria:**
- [ ] Implement AzureOpenAIMetricsExtractor
- [ ] Extract input_tokens, output_tokens, total_tokens
- [ ] Calculate costs using Azure pricing
- [ ] Track Azure request IDs in telemetry
- [ ] Support streaming metrics extraction

### FR-7: Error Handling
**Priority: Medium**

The system SHALL handle Azure OpenAI specific errors:
- Authentication failures (invalid api-key)
- Resource not found errors
- Deployment not found errors
- Content filtering violations
- Rate limiting responses

**Acceptance Criteria:**
- [ ] Map Azure error codes to appropriate HTTP responses
- [ ] Provide meaningful error messages
- [ ] Log errors with appropriate context
- [ ] Handle content filtering gracefully

### FR-8: Configuration
**Priority: Low**

The system SHALL support configuration:
- Default Azure resource name
- Default deployment ID
- Default API version
- Region selection
- Cost calculation parameters

**Acceptance Criteria:**
- [ ] Environment variable configuration
- [ ] Reasonable defaults for all parameters
- [ ] Runtime configuration validation
- [ ] Documentation for all config options

## Technical Requirements

### TR-1: Code Structure
- Follow existing provider pattern in `src/providers/`
- Implement Provider trait with Azure-specific logic
- Create AzureOpenAIMetricsExtractor for telemetry
- Add comprehensive tests

### TR-2: Dependencies
- Utilize existing reqwest HTTP client
- Leverage existing serde_json for JSON processing
- Use existing telemetry infrastructure
- No new external dependencies required

### TR-3: Performance
- Maintain sub-100ms proxy overhead
- Support concurrent requests
- Efficient URL construction and header processing
- Minimal memory allocation in hot paths

## API Specification

### Request Headers
```
x-provider: azure-openai (required)
api-key: {azure-api-key} (required)
x-azure-resource-name: {resource-name} (optional)
x-azure-deployment-id: {deployment-id} (optional)
x-azure-api-version: {api-version} (optional, defaults to 2024-02-15-preview)
```

### Example Usage
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: azure-openai" \
  -H "api-key: your-azure-api-key" \
  -H "x-azure-resource-name: your-resource" \
  -H "x-azure-deployment-id: gpt-4" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

## Implementation Plan

### Phase 1: Core Provider (Week 1)
- [ ] Create azure_openai.rs with basic Provider implementation
- [ ] Implement URL construction and header processing
- [ ] Add basic authentication handling
- [ ] Register provider in module system

### Phase 2: Metrics & Telemetry (Week 2)
- [ ] Implement AzureOpenAIMetricsExtractor
- [ ] Add cost calculation logic
- [ ] Integrate with existing telemetry middleware
- [ ] Support streaming metrics extraction

### Phase 3: Testing & Documentation (Week 3)
- [ ] Unit tests for provider implementation
- [ ] Integration tests with real Azure OpenAI
- [ ] Update documentation with Azure provider info
- [ ] Add example configurations

### Phase 4: Production Readiness (Week 4)
- [ ] Error handling improvements
- [ ] Performance optimization
- [ ] Security review
- [ ] Deployment testing

## Success Criteria

1. **Functional**: Users can route requests to Azure OpenAI using `x-provider: azure-openai`
2. **Compatible**: Existing OpenAI SDK clients work without modification
3. **Observable**: Full telemetry and metrics collection for Azure requests
4. **Performant**: No measurable performance degradation vs other providers
5. **Reliable**: Handles all Azure OpenAI response formats and error conditions

## Risk Assessment

**High Risk:**
- Azure API changes requiring URL structure modifications
- Authentication complexity with multiple credential types

**Medium Risk:**
- Content filtering affecting response compatibility
- Cost calculation accuracy for pricing changes

**Low Risk:**
- Performance impact from URL construction
- Configuration complexity

## Dependencies

- Azure OpenAI service access for testing
- Documentation of Azure OpenAI API specifications
- Existing provider implementation patterns
- Telemetry infrastructure

## Testing Strategy

1. **Unit Tests**: Test provider implementation in isolation
2. **Integration Tests**: Test against real Azure OpenAI endpoints
3. **Performance Tests**: Verify latency and throughput requirements
4. **Compatibility Tests**: Validate with various OpenAI SDKs
5. **Error Scenario Tests**: Test error handling and edge cases

## Documentation Updates

- Add Azure OpenAI to supported providers list
- Update README with Azure configuration examples
- Create Azure-specific documentation in `docs/providers/azure-openai.md`
- Update deployment guides with Azure considerations

## Conclusion

Adding Azure OpenAI support will enhance the gateway's enterprise appeal while maintaining the existing high-performance, unified API approach. The implementation follows established patterns and leverages existing infrastructure, minimizing risk while maximizing value for enterprise users requiring Azure-hosted AI services.
