# Metrics Exporters

The Noveum AI Gateway supports exporting metrics to various destinations for monitoring, analysis, and debugging purposes. This document details the configuration and usage of available metrics exporters.

## Elasticsearch Exporter

The Elasticsearch exporter allows you to send request metrics to an Elasticsearch cluster for storage, analysis, and visualization.

### Quick Start

1. Configure environment variables:
```env
ELASTICSEARCH_HOST=your-elasticsearch-host
ELASTICSEARCH_PORT=9200  # Optional, defaults to 9200
ELASTICSEARCH_USERNAME=elastic  # Optional, defaults to 'elastic'
ELASTICSEARCH_PASSWORD=your-password
ELASTICSEARCH_INDEX=metrics  # Optional, defaults to 'metrics'
```

2. The exporter will be automatically initialized on the first request if valid credentials are provided.

### Configuration Options

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| ELASTICSEARCH_HOST | Yes | - | Elasticsearch host URL (with protocol) |
| ELASTICSEARCH_PORT | No | 9200 | Elasticsearch port |
| ELASTICSEARCH_USERNAME | No | elastic | Username for authentication |
| ELASTICSEARCH_PASSWORD | Yes | - | Password for authentication |
| ELASTICSEARCH_INDEX | No | metrics | Index name for storing metrics |

### Security Considerations

1. **SSL/TLS**: The exporter automatically uses HTTPS if the host URL starts with 'https://'
2. **Authentication**: Basic authentication is used with provided credentials
3. **Network Security**: Ensure your network allows connections to Elasticsearch port

### Exported Data Structure

The exporter sends the following document structure to Elasticsearch:

```json
{
  "@timestamp": "2024-03-18T12:00:00Z",
  "type": "metrics",
  "request_id": "unique-request-id",
  "method": "POST",
  "path": "/v1/chat/completions",
  "provider": "provider-name",
  "performance": {
    "startTime": 1734524178773,
    "ttfb": 555,
    "endTime": 1734524179734,
    "totalLatency": 961
  },
  "success": true,
  "cached": false,
  "metadata": {
    "estimated": false,
    "totalChunks": 26,
    "streamComplete": true
  },
  "model": "model-name",
  "status": 200,
  "tokens": {
    "input": 34,
    "output": 75,
    "total": 109
  },
  "cost": {
    "inputCost": 0.000034,
    "outputCost": 0.000075,
    "totalCost": 0.000109
  }
}
```

### Reliability Features

1. **Automatic Retries**:
   - Maximum 3 retry attempts
   - Exponential backoff between retries
   - Retries only on network errors or 5xx responses

2. **Timeout Handling**:
   - 5-second timeout per request
   - Automatic cleanup of timed-out requests

3. **Error Handling**:
   - Non-blocking error handling
   - Detailed error logging
   - Failed exports don't impact the main request flow

### Troubleshooting

Common issues and solutions:

1. **Connection Errors**:
   - Verify ELASTICSEARCH_HOST includes protocol (http:// or https://)
   - Check network connectivity
   - Verify firewall rules

2. **Authentication Failures**:
   - Verify ELASTICSEARCH_USERNAME and ELASTICSEARCH_PASSWORD
   - Ensure user has write permissions to the index
   - Check if basic authentication is enabled in Elasticsearch

3. **Export Failures**:
   - Check logs for detailed error messages
   - Verify index exists or user has permission to create indices
   - Monitor Elasticsearch disk space and health

### Best Practices

1. **Index Management**:
   - Use ILM policies to manage index lifecycle
   - Set up index templates before sending data
   - Monitor index size and performance

2. **Security**:
   - Use dedicated user for metrics export
   - Implement minimal required permissions
   - Use HTTPS for secure data transmission

3. **Performance**:
   - Monitor export latency
   - Set up alerts for failed exports
   - Regularly clean up old indices 