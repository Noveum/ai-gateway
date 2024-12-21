import { RequestMetrics } from '../../types';
import { BaseExporter } from './base';

export interface ElasticsearchConfig {
  host: string;
  port: string;
  password: string;
  index: string;
  username?: string;
}

export class ElasticsearchExporter extends BaseExporter {
  private readonly url: string;
  private readonly authHeader: string;
  private readonly index: string;

  constructor(config: ElasticsearchConfig) {
    super(config as unknown as Record<string, string>);

    this.index = config.index;

    // Construct base URL with protocol
    this.url = config.host.startsWith('http') ? config.host : `https://${config.host}`;
    if (config.port) {
      this.url = `${this.url}:${config.port}`;
    }

    // Create auth header
    const username = config.username || 'elastic';
    this.authHeader = `Basic ${btoa(`${username}:${config.password}`)}`;

    console.info('[ElasticsearchExporter] Initialized with:', {
      url: this.url,
      index: this.index
    });
  }

  async export(metrics: RequestMetrics): Promise<void> {
    // Wait a small amount of time to ensure metrics are fully populated
    await new Promise(resolve => setTimeout(resolve, 100));

    console.info(`[ElasticsearchExporter] Exporting metrics for request ${metrics.requestId}`);

    // Create a deep copy of the metrics to avoid reference issues
    const document = {
      '@timestamp': metrics.timestamp,
      type: 'metrics',
      request_id: metrics.requestId,
      method: metrics.method,
      path: metrics.path,
      provider: metrics.provider,
      performance: { ...metrics.performance },
      success: metrics.success,
      cached: metrics.cached,
      metadata: metrics.metadata ? {
        estimated: Boolean(metrics.metadata.estimated),
        totalChunks: Number(metrics.metadata.totalChunks) || 0,
        streamComplete: Boolean(metrics.metadata.streamComplete)
      } : {
        estimated: false,
        totalChunks: 0,
        streamComplete: false
      },
      model: metrics.model,
      status: metrics.status,
      tokens: metrics.tokens ? {
        input: Number(metrics.tokens.input) || 0,
        output: Number(metrics.tokens.output) || 0,
        total: Number(metrics.tokens.total) || 0
      } : {
        input: 0,
        output: 0,
        total: 0
      },
      cost: metrics.cost ? {
        inputCost: Number(metrics.cost.inputCost) || 0,
        outputCost: Number(metrics.cost.outputCost) || 0,
        totalCost: Number(metrics.cost.totalCost) || 0
      } : {
        inputCost: 0,
        outputCost: 0,
        totalCost: 0
      },
      location: metrics.location ? {
        city: metrics.location.city,
        country: metrics.location.country,
        continent: metrics.location.continent,
        latitude: metrics.location.latitude,
        longitude: metrics.location.longitude,
        timezone: metrics.location.timezone,
        region: metrics.location.region
      } : undefined
    };

    const url = `${this.url}/${this.index}/_doc/${metrics.requestId}?pretty`;
    
    let lastError: Error | null = null;
    const maxRetries = 3;

    for (let attempt = 1; attempt <= maxRetries; attempt++) {
      try {
        await this.sendRequest(url, JSON.stringify(document));
        console.info(`[ElasticsearchExporter] Successfully exported metrics for request ${metrics.requestId}`);
        return;
      } catch (error) {
        lastError = error instanceof Error ? error : new Error(String(error));
        console.error(`[ElasticsearchExporter] Attempt ${attempt}/${maxRetries} failed:`, {
          error: lastError.message,
          requestId: metrics.requestId
        });

        if (attempt < maxRetries) {
          const delay = Math.min(1000 * Math.pow(2, attempt - 1), 5000);
          console.info(`[ElasticsearchExporter] Retrying in ${delay}ms...`);
          await new Promise(resolve => setTimeout(resolve, delay));
        }
      }
    }

    throw lastError || new Error('Failed to export metrics after all retries');
  }

  private async sendRequest(url: string, body: string): Promise<void> {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 5000);

    try {
      console.debug('[ElasticsearchExporter] Sending request...');
      const response = await fetch(url, {
        method: 'POST',
        headers: {
          'Authorization': this.authHeader,
          'Content-Type': 'application/json',
          'Accept': 'application/json'
        },
        body,
        signal: controller.signal
      });

      clearTimeout(timeoutId);

      if (!response) {
        throw new Error('No response received from Elasticsearch');
      }

      const responseText = await response.text();
      console.debug('[ElasticsearchExporter] Response received:', responseText.substring(0, 100) + '...');

      if (!response.ok) {
        throw new Error(`Elasticsearch error: ${response.status}`);
      }

      const result = JSON.parse(responseText);

      if (result.error) {
        throw new Error('Elasticsearch returned an error response');
      }
    } catch (error) {
      if (error instanceof Error && error.name === 'AbortError') {
        throw new Error('Request timed out');
      }
      throw error;
    } finally {
      clearTimeout(timeoutId);
    }
  }

  async close(): Promise<void> { } // Nothing to clean up
} 