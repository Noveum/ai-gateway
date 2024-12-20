import { RequestMetrics, TokenUsage, ProviderMetricsConfig } from '../types';
import { MODEL_COSTS } from './costs/index';
import { MetricsExportManager } from './exporters';
import type { MiddlewareHandler } from 'hono';

export class MetricsCollector {
  private metricsPromise: Promise<void>;
  private metricsResolve!: () => void;
  private metricsTimeout: NodeJS.Timeout | null = null;
  private metrics: RequestMetrics;
  private chunkCount: number = 0;
  private streamComplete: boolean = false;
  private config: ProviderMetricsConfig;
  private isFinished: boolean = false;
  private isExported: boolean = false;

  constructor(requestId: string, method: string, path: string, provider: string, config: ProviderMetricsConfig = {}) {
    this.metricsPromise = new Promise((resolve) => {
      this.metricsResolve = resolve;
    });

    // Set a maximum timeout for metrics collection (5 seconds)
    this.metricsTimeout = setTimeout(() => {
      this.markMetricsComplete();
    }, 5000);

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

  getRequestId(): string {
    return this.metrics.requestId;
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
    // Mark metrics as complete when stream is complete
    this.markMetricsComplete();
  }

  setStatus(status: number) {
    this.metrics.status = status;
    this.metrics.success = status >= 200 && status < 300;
  }

  setModel(model: string) {
    this.metrics.model = model;
    // Update token costs based on provider and model if not explicitly set
    if (!this.config.inputTokenCost || !this.config.outputTokenCost) {
      const providerCosts = MODEL_COSTS[this.metrics.provider];
      if (providerCosts) {
        // Try exact match first
        let costs = providerCosts[model];
        if (!costs) {
          // Try normalized version (lowercase, no hyphens)
          const normalizedModel = model.toLowerCase().replace(/[-_]/g, '');
          costs = providerCosts[normalizedModel];
        }
        if (costs) {
          this.config = {
            ...this.config,
            inputTokenCost: costs.inputTokenCost,
            outputTokenCost: costs.outputTokenCost
          };
        }
      }
    }
  }

  setTokenUsage(usage: TokenUsage) {
    if (!this.metrics.tokens) {
      this.metrics.tokens = {
        input: 0,
        output: 0,
        total: 0
      };
    }

    if (usage.inputTokens !== undefined) {
      this.metrics.tokens.input = Number(usage.inputTokens);
    }
    if (usage.outputTokens !== undefined) {
      this.metrics.tokens.output = Number(usage.outputTokens);
    }
    if (usage.totalTokens !== undefined) {
      this.metrics.tokens.total = Number(usage.totalTokens);
    } else if (this.metrics.tokens.input !== undefined && this.metrics.tokens.output !== undefined) {
      this.metrics.tokens.total = this.metrics.tokens.input + this.metrics.tokens.output;
    }
    this.updateCosts();
  }

  private updateCosts() {
    if (!this.metrics.tokens) return;

    if (this.config.inputTokenCost || this.config.outputTokenCost) {
      if (!this.metrics.cost) {
        this.metrics.cost = {
          inputCost: 0,
          outputCost: 0,
          totalCost: 0
        };
      }

      this.metrics.cost.inputCost = this.metrics.tokens.input * (this.config.inputTokenCost || 0);
      this.metrics.cost.outputCost = this.metrics.tokens.output * (this.config.outputTokenCost || 0);
      this.metrics.cost.totalCost = this.metrics.cost.inputCost + this.metrics.cost.outputCost;
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

  markMetricsComplete() {
    if (this.metricsTimeout) {
      clearTimeout(this.metricsTimeout);
      this.metricsTimeout = null;
    }
    if (this.metricsResolve) {
      this.metricsResolve();
    }
  }

  async finish(ctx?: { waitUntil: (promise: Promise<any>) => void }): Promise<RequestMetrics> {
    if (this.isFinished) {
      return this.metrics;
    }

    if (this.isExported) {
      console.debug('[Metrics] Metrics already exported for request:', this.metrics.requestId);
      return this.metrics;
    }

    try {
      // Wait for metrics to be marked as complete with a timeout
      const timeoutPromise = new Promise<void>((_, reject) => {
        setTimeout(() => reject(new Error('Metrics collection timeout')), 5000);
      });

      await Promise.race([this.metricsPromise, timeoutPromise]);
    } catch (error) {
      console.warn('[Metrics] Metrics collection timed out:', error);
    } finally {
      // Ensure we always mark metrics as complete
      this.markMetricsComplete();
    }

    this.isFinished = true;
    
    this.metrics.performance.endTime = Date.now();
    this.metrics.performance.totalLatency = this.metrics.performance.endTime - this.metrics.performance.startTime;
    
    // Ensure all metrics are properly set
    if (!this.metrics.tokens) {
      this.metrics.tokens = {
        input: 0,
        output: 0,
        total: 0
      };
    }
    
    this.updateCosts();
    
    // Make sure metadata is properly set
    this.metrics.metadata = {
      estimated: Boolean(this.metrics.metadata?.estimated),
      totalChunks: this.chunkCount,
      streamComplete: this.streamComplete
    };

    console.debug('[Metrics] Final metrics before export:', this.metrics);
    
    // Export metrics and wait for completion
    if (!this.isExported) {
      try {
        await MetricsExportManager.getInstance().exportMetrics(this.metrics, ctx);
        this.isExported = true;  // Mark as exported after successful export
      } catch (error) {
        console.error('[Metrics] Export failed:', error);
      }
    }
    
    return this.metrics;
  }
} 