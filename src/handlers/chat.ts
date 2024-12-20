import { Context } from 'hono';
import { Variables, Bindings, ChatCompletionRequest } from '../types';
import { ProviderFactory } from '../providers/factory';
import { hooksManager } from '../hooks';
import { getRuntimeKey } from 'hono/adapter';

const processMetrics = async (providerInstance: any, c: Context) => {
  const metrics = providerInstance.getMetricsCollector();
  if (!metrics) return;

  const processMetricsTask = metrics.finish(c.env).catch((error: Error) => {
    console.error('[Metrics] Failed to process metrics:', error);
  });

  // Handle background tasks differently based on runtime
  const runtime = getRuntimeKey();
  if (runtime === 'node') {
    // For Node.js, just run the promise in the background
    processMetricsTask.catch((error: Error) => {
      console.error('[Background Task] Error:', error);
    });
  } else if (c.executionCtx) {
    // For Cloudflare Workers, use waitUntil
    c.executionCtx.waitUntil(processMetricsTask);
  }
};

export const handleChatCompletion = async (c: Context<{ Variables: Variables; Bindings: Bindings }>) => {
  const startTime = Date.now();
  let success = false;
  let providerInstance = null;
  
  try {
    const provider = c.get('provider');
    const config = c.get('config');
    console.debug('[ChatCompletion] Starting request handling');
    
    // Get request body
    const originalRequest = await c.req.json<ChatCompletionRequest>();
    console.debug('[ChatCompletion] Received request');

    // Transform request through hooks
    const request = await hooksManager.transformRequest(originalRequest, c);
    console.debug('[ChatCompletion] Request transformed');

    // Get provider instance
    providerInstance = ProviderFactory.getProvider(provider, config);
    console.debug('[ChatCompletion] Initialized provider');

    // Make the request to the provider
    console.debug('[ChatCompletion] Processing request');
    const response = await providerInstance.chatCompletion(request);
    console.debug('[ChatCompletion] Request processed');

    // Transform response through hooks
    const transformedResponse = await hooksManager.transformResponse(response, c);
    console.debug('[ChatCompletion] Response transformed');

    success = true;
    return transformedResponse;
  } catch (error: unknown) {
    success = false;
    console.debug('[ChatCompletion] Error occurred');
    // Let hooks try to handle the error first
    try {
      return await hooksManager.handleError(error, c);
    } catch (e) {
      console.debug('[ChatCompletion] Error handling failed');
      // If no hook handles the error, return a generic error response
      return c.json({
        error: {
          message: error instanceof Error ? error.message : 'An unknown error occurred',
          type: 'request_error',
          code: 500
        }
      }, 500);
    }
  } finally {
    if (providerInstance) {
      await processMetrics(providerInstance, c);
    }
  }
}; 