import { RequestMetrics, TokenUsage, ProviderMetricsConfig } from '../types';

export class MetricsCollector {
  private metrics: RequestMetrics;
  private chunkCount: number = 0;
  private streamComplete: boolean = false;
  private config: ProviderMetricsConfig;

  constructor(requestId: string, method: string, path: string, provider: string, config: ProviderMetricsConfig = {}) {
    this.metrics = {
      requestId,
      timestamp: new Date().toISOString(),
      method,
      path,
      provider,
      performance: {
        startTime: Date.now()
      },
      success: false,
      cached: false,
      metadata: {
        estimated: false,
        totalChunks: 0,
        streamComplete: false
      }
    };
    this.config = config;
  }

  getStartTime(): number {
    return this.metrics.performance.startTime;
  }

  setTTFB(ttfb: number) {
    console.debug('[Metrics] Setting TTFB:', ttfb);
    this.metrics.performance.ttfb = ttfb;
  }

  incrementChunks() {
    this.chunkCount++;
    // console.debug('[Metrics] Incrementing chunks:', this.chunkCount);
    this.metrics.metadata!.totalChunks = this.chunkCount;
  }

  setStreamComplete() {
    this.streamComplete = true;
    this.metrics.metadata!.streamComplete = true;
  }

  setStatus(status: number) {
    this.metrics.status = status;
    this.metrics.success = status >= 200 && status < 300;
  }

  setModel(model: string) {
    this.metrics.model = model;
  }

  setTokenUsage(usage: TokenUsage) {
    // console.debug('[Metrics] Setting token usage:', usage);
    
    if (usage.inputTokens || usage.outputTokens || usage.totalTokens) {
      if (!this.metrics.tokens) {
        this.metrics.tokens = {
          input: 0,
          output: 0,
          total: 0
        };
      }

      if (usage.inputTokens) {
        this.metrics.tokens.input = usage.inputTokens;
      }
      if (usage.outputTokens) {
        this.metrics.tokens.output = usage.outputTokens;
      }
      if (usage.totalTokens) {
        this.metrics.tokens.total = usage.totalTokens;
      } else if (this.metrics.tokens.input !== undefined && this.metrics.tokens.output !== undefined) {
        this.metrics.tokens.total = this.metrics.tokens.input + this.metrics.tokens.output;
      }
    }

    this.updateCosts();
  }

  private updateCosts() {
    if (!this.metrics.tokens) return;

    if (this.config.inputTokenCost || this.config.outputTokenCost) {
      this.metrics.cost = {
        inputCost: this.metrics.tokens.input ? this.metrics.tokens.input * (this.config.inputTokenCost || 0) : undefined,
        outputCost: this.metrics.tokens.output ? this.metrics.tokens.output * (this.config.outputTokenCost || 0) : undefined,
        totalCost: undefined
      };

      if (this.metrics.cost.inputCost !== undefined && this.metrics.cost.outputCost !== undefined) {
        this.metrics.cost.totalCost = this.metrics.cost.inputCost + this.metrics.cost.outputCost;
      }
    }
  }

  setMetadata(metadata: Record<string, any>) {
    this.metrics.metadata = {
      estimated: this.metrics.metadata?.estimated ?? false,
      totalChunks: this.metrics.metadata?.totalChunks ?? 0,
      streamComplete: this.metrics.metadata?.streamComplete,
      ...metadata
    };
  }

  setCached(cached: boolean) {
    this.metrics.cached = cached;
  }

  finish() {
    console.debug('[Metrics] Finishing collection');
    this.metrics.performance.endTime = Date.now();
    this.metrics.performance.totalLatency = this.metrics.performance.endTime - this.metrics.performance.startTime;
    
    this.updateCosts();
    
    console.log('[Metrics] Final Report:', JSON.stringify({
        ...this.metrics,
        _debug: {
          hasCostConfig: !!(this.config.inputTokenCost || this.config.outputTokenCost),
          hasTokens: !!this.metrics.tokens,
          isStreamComplete: this.streamComplete
        }
      }, null, 2)
    );
    // setTimeout(() => {
    //   , null, 2));
    // }, 0);

    return this.metrics;
  }
} 