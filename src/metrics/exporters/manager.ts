import { RequestMetrics } from '../../types';
import { MetricsExporter } from './base';

export class MetricsExportManager {
  private static instance: MetricsExportManager;
  private exporters: MetricsExporter[] = [];
  private initialized: boolean = false;

  private constructor() {}

  static getInstance(): MetricsExportManager {
    if (!MetricsExportManager.instance) {
      MetricsExportManager.instance = new MetricsExportManager();
    }
    return MetricsExportManager.instance;
  }

  addExporter(exporter: MetricsExporter): void {
    if (!this.exporters.some(e => e.constructor === exporter.constructor)) {
      this.exporters.push(exporter);
      this.initialized = true;
    }
  }

  async exportMetrics(
    metrics: RequestMetrics, 
    ctx?: { waitUntil: (promise: Promise<any>) => void }
  ): Promise<void> {
    if (!this.initialized || this.exporters.length === 0) return;

    await Promise.all(
      this.exporters.map(exporter => 
        exporter.export(metrics, ctx).catch(error => {
          console.error(`[MetricsExportManager] Export failed:`, error);
        })
      )
    );
  }

  async cleanup(): Promise<void> {
    if (!this.initialized) return;
    
    await Promise.all(
      this.exporters.map(exporter =>
        exporter.close().catch(error => {
          console.error('[MetricsExportManager] Failed to close exporter:', error);
        })
      )
    );
    
    this.exporters = [];
    this.initialized = false;
  }

  isInitialized(): boolean {
    return this.initialized;
  }
} 