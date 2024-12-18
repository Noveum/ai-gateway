import { BaseExporter } from './base';
import { RequestMetrics } from '../../types';

export interface ElasticsearchConfig {
  host: string;
  port: string;
  password: string;
  index: string;
  username?: string;
}

interface ElasticsearchBulkResponse {
  took: number;
  errors: boolean;
  items?: Array<{
    index: {
      _index: string;
      _id: string;
      status: number;
      error?: any;
    };
  }>;
}

export class ElasticsearchExporter extends BaseExporter {
  private readonly url: string;
  private readonly authHeader: string;
  private readonly index: string;

  constructor(config: ElasticsearchConfig) {
    super(config as unknown as Record<string, string>);
    
    console.debug('[ElasticsearchExporter] Initializing with config:', {
      host: config.host,
      port: config.port,
      index: config.index,
      username: config.username
    });
    
    this.index = config.index;
    
    // Construct base URL with protocol
    this.url = config.host.startsWith('http') ? config.host : `https://${config.host}`;
    if (config.port) {
      this.url = `${this.url}:${config.port}`;
    }
    
    // Create auth header
    const username = config.username || 'elastic';
    this.authHeader = `Basic ${btoa(`${username}:${config.password}`)}`;
  }

  async export(metrics: RequestMetrics): Promise<void> {
    console.debug('[ElasticsearchExporter] Starting export for request:', metrics.requestId);
    try {
      const document = this.transformMetrics(metrics);
      console.debug('[ElasticsearchExporter] Transformed metrics:', document);
      
      const bulk = this.createBulkBody(metrics.requestId, document);
      console.debug('[ElasticsearchExporter] Created bulk body, length:', bulk.length);
      
      await this.sendToElasticsearch(bulk);
      console.debug('[ElasticsearchExporter] Successfully exported metrics for request:', metrics.requestId);
    } catch (error) {
      console.error('[ElasticsearchExporter] Export failed:', {
        error,
        requestId: metrics.requestId
      });
      throw error;
    }
  }

  private createBulkBody(id: string, document: Record<string, any>): string {
    const action = {
      index: { 
        _index: this.index,
        _id: id
      }
    };
    return `${JSON.stringify(action)}\n${JSON.stringify(document)}\n`;
  }

  private async sendToElasticsearch(body: string): Promise<void> {
    console.debug('[ElasticsearchExporter] Sending request to:', `${this.url}/_bulk`);
    
    const response = await fetch(`${this.url}/_bulk`, {
      method: 'POST',
      headers: {
        'Authorization': this.authHeader,
        'Content-Type': 'application/json'
      },
      body
    });

    console.debug('[ElasticsearchExporter] Received response:', {
      status: response.status,
      ok: response.ok
    });

    const responseText = await response.text();
    
    if (!response.ok) {
      throw new Error(`Elasticsearch error: ${response.status} - ${responseText}`);
    }

    try {
      const result = JSON.parse(responseText) as ElasticsearchBulkResponse;
      console.debug('[ElasticsearchExporter] Parsed response:', {
        took: result.took,
        hasErrors: result.errors,
        itemCount: result.items?.length
      });
      
      if (result.errors) {
        const errors = result.items?.filter(item => item.index.error);
        throw new Error(`Bulk indexing errors: ${JSON.stringify(errors)}`);
      }
    } catch (e) {
      throw new Error(`Failed to parse Elasticsearch response: ${e.message}`);
    }
  }

  private transformMetrics(metrics: RequestMetrics): Record<string, any> {
    return {
      '@timestamp': metrics.timestamp,
      type: 'metrics',
      request_id: metrics.requestId,
      method: metrics.method,
      path: metrics.path,
      provider: metrics.provider,
      performance: metrics.performance,
      success: metrics.success,
      cached: metrics.cached,
      metadata: metrics.metadata,
      model: metrics.model,
      status: metrics.status,
      tokens: metrics.tokens,
      cost: metrics.cost
    };
  }

  async close(): Promise<void> {} // Nothing to clean up
} 