import { RequestMetrics } from '../../types';

export interface MetricsExporter {
  export(metrics: RequestMetrics, ctx?: { waitUntil: (promise: Promise<any>) => void }): Promise<void>;
  close(): Promise<void>;
}

export abstract class BaseExporter implements MetricsExporter {
  protected config: Record<string, string>;

  constructor(config: Record<string, string>) {
    this.config = config;
  }

  abstract export(metrics: RequestMetrics, ctx?: { waitUntil: (promise: Promise<any>) => void }): Promise<void>;
  
  abstract close(): Promise<void>;

  protected async exportWithRetry(
    metrics: RequestMetrics,
    exportFn: () => Promise<void>,
    maxRetries: number = 2
  ): Promise<void> {
    let lastError: Error | null = null;
    
    for (let attempt = 0; attempt < maxRetries; attempt++) {
      try {
        await exportFn();
        return;
      } catch (error) {
        lastError = error as Error;
        // Only retry on network errors or 5xx responses
        if (!this.shouldRetry(error)) {
          break;
        }
        await this.delay(Math.pow(2, attempt) * 100); // Exponential backoff
      }
    }
    
    // Log error but don't throw to avoid impacting the main request
    console.error('[MetricsExporter] Failed to export metrics:', lastError);
  }

  private shouldRetry(error: any): boolean {
    return error.name === 'NetworkError' || 
           error.code === 'ECONNREFUSED';
  }

  private delay(ms: number): Promise<void> {
    return new Promise(resolve => setTimeout(resolve, ms));
  }
} 