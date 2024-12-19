import { ChatCompletionRequest, ProviderConfig } from '../types';
import { MetricsCollector } from '../metrics/collector';

export interface AIProvider {
  initialize(config: ProviderConfig): void;
  validateConfig(): void;
  chatCompletion(request: ChatCompletionRequest): Promise<Response>;
  transformRequest(request: ChatCompletionRequest): Promise<any>;
  transformResponse(response: any, request: ChatCompletionRequest): Promise<any>;
  extractMetrics?(response: any): { tokens?: any, performance?: any, metadata?: any } | null;
  getMetricsCollector(): MetricsCollector | undefined;
}

export abstract class BaseProvider implements AIProvider {
  protected config: ProviderConfig = {};
  protected metricsCollector?: MetricsCollector;

  initialize(config: ProviderConfig): void {
    this.config = config;
  }

  abstract validateConfig(): void;
  abstract chatCompletion(request: ChatCompletionRequest): Promise<Response>;
  abstract transformRequest(request: ChatCompletionRequest): Promise<any>;
  abstract transformResponse(response: any, request: ChatCompletionRequest): Promise<any>;

  protected initializeMetrics(request: ChatCompletionRequest): MetricsCollector {
    // Reset metrics collector for each new request
    if (this.metricsCollector) {
      console.debug('[BaseProvider] Resetting previous metrics collector');
      this.metricsCollector = undefined;
    }

    const requestId = crypto.randomUUID();
    this.metricsCollector = new MetricsCollector(
      requestId,
      'POST',
      '/v1/chat/completions',
      this.constructor.name.replace('Provider', '').toLowerCase(),
      {
        inputTokenCost: this.config.inputTokenCost,
        outputTokenCost: this.config.outputTokenCost
      }
    );
    
    console.debug('[BaseProvider] Initialized new metrics collector:', requestId);
    
    if (request.model) {
      this.metricsCollector.setModel(request.model);
    }
    
    return this.metricsCollector;
  }

  protected async wrapResponseWithMetrics(
    response: Response, 
    request: ChatCompletionRequest,
    extractMetricsFn?: (data: any) => any
  ): Promise<Response> {
    const metrics = this.metricsCollector!;
    metrics.setStatus(response.status);

    if (request.stream) {
      const reader = response.body!.getReader();
      let ttfbSet = false;
      let buffer = '';

      // Create a new stream that will handle both metrics and response
      const stream = new ReadableStream({
        async start(controller) {
          try {
            while (true) {
              const {done, value} = await reader.read();
              
              if (done) {
                metrics.setStreamComplete();
                // metrics.finish();
                controller.close();
                break;
              }

              if (!ttfbSet) {
                metrics.setTTFB(Date.now() - metrics.getStartTime());
                ttfbSet = true;
              }

              metrics.incrementChunks();

              // Process metrics from chunk
              const chunk = new TextDecoder().decode(value);
              buffer += chunk;
              const lines = buffer.split('\n');
              buffer = lines.pop() || '';

              for (const line of lines) {
                const trimmedLine = line.trim();
                if (!trimmedLine || trimmedLine === 'data: [DONE]') continue;
                
                if (trimmedLine.startsWith('data: ')) {
                  try {
                    const jsonStr = trimmedLine.slice(6);
                    const data = JSON.parse(jsonStr);
                    if (extractMetricsFn) {
                      const chunkMetrics = extractMetricsFn(data);
                      if (chunkMetrics?.tokens) {
                        metrics.setTokenUsage(chunkMetrics.tokens);
                      }
                    }
                  } catch (e) {
                    console.debug('[Metrics] Failed to parse chunk metrics:', e);
                  }
                }
              }

              // Forward the chunk to the client
              controller.enqueue(value);
            }
          } catch (error) {
            console.error('[Metrics] Error processing stream:', error);
            controller.error(error);
          } finally {
            reader.releaseLock();
          }
        },
        cancel() {
          reader.releaseLock();
        }
      });

      return new Response(stream, {
        headers: response.headers,
        status: response.status
      });
    } else {
      // For non-streaming responses, clone before reading
      const clonedResponse = response.clone();
      
      // Process metrics in the background
      (async () => {
        try {
          const data = await clonedResponse.json();
          if (extractMetricsFn) {
            const responseMetrics = extractMetricsFn(data);
            if (responseMetrics?.tokens) {
              metrics.setTokenUsage(responseMetrics.tokens);
            }
          }
          metrics.setTTFB(Date.now() - metrics.getStartTime());
          // metrics.finish();
        } catch (error) {
          console.error('[Metrics] Error processing metrics:', error);
        }
      })();

      // Return original response
      return response;
    }
  }

  protected handleError(error: any): Response {
    const errorResponse = {
      error: {
        message: error.message || 'An unknown error occurred',
        type: error.type || 'provider_error',
        code: error.status || 500,
        details: error.details || undefined
      }
    };

    if (this.metricsCollector) {
      this.metricsCollector.setStatus(error.status || 500);
      this.metricsCollector.finish();
    }

    console.error(`[${this.constructor.name}] Error:`, errorResponse);

    return new Response(JSON.stringify(errorResponse), {
      status: error.status || 500,
      headers: {
        'Content-Type': 'application/json',
        'x-request-id': this.metricsCollector?.getRequestId() || crypto.randomUUID()
      }
    });
  }

  protected createHeaders(headers: Record<string, string> = {}): Headers {
    const defaultHeaders = {
      'Content-Type': 'application/json',
    };
    return new Headers({ ...defaultHeaders, ...headers });
  }

  protected createStreamHeaders(): Headers {
    return this.createHeaders({
      'Content-Type': 'text/event-stream',
      'Cache-Control': 'no-cache',
      'Connection': 'keep-alive'
    });
  }

  protected cleanRequestBody(body: Record<string, any>): Record<string, any> {
    return Object.fromEntries(
      Object.entries(body).filter(([_, value]) => value !== undefined)
    );
  }

  getMetricsCollector(): MetricsCollector | undefined {
    return this.metricsCollector;
  }
} 