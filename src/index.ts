import { Hono } from 'hono';
import { cors } from 'hono/cors';
import { logger } from 'hono/logger';
import { prettyJSON } from 'hono/pretty-json';
import { handleChatCompletion } from './handlers/chat';
import { hooksManager } from './hooks';
import { ElasticsearchExporter, MetricsExportManager } from './metrics/exporters';
import { authMiddleware } from './middleware/auth';
import { loggingMiddleware } from './middleware/logging';
import { validationMiddleware } from './middleware/validation';
import { Bindings, Env, Variables } from './types';

// Create the application
const app = new Hono<{ Variables: Variables; Bindings: Bindings }>();

// Initialize metrics exporters once at startup
const initializeMetricsExporters = (env: Env) => {
  const manager = MetricsExportManager.getInstance();

  if (manager.isInitialized()) {
    return;
  }

  if (!env.ELASTICSEARCH_HOST) {
    return;
  }

  try {
    manager.addExporter(new ElasticsearchExporter({
      host: env.ELASTICSEARCH_HOST,
      port: env.ELASTICSEARCH_PORT || '9200',
      username: env.ELASTICSEARCH_USERNAME || 'elastic',
      password: env.ELASTICSEARCH_PASSWORD,
      index: env.ELASTICSEARCH_INDEX || 'metrics'
    }));
  } catch (error) {
    console.error('[Metrics] Failed to initialize Elasticsearch exporter:', error);
  }
};

// Add global middleware
app.use('*', cors());
app.use('*', logger());
app.use('*', prettyJSON());
app.use('*', loggingMiddleware);

// Initialize metrics exporters middleware
app.use('*', async (c, next) => {
  // Only initialize on first request
  if (!MetricsExportManager.getInstance().isInitialized()) {
    initializeMetricsExporters(c.env);
  }
  return next();
});

// Health check endpoint
app.get('/health', (c) => c.json({ status: 'ok' }));

// API version prefix
const v1 = app.basePath('/v1');

// Chat completion endpoint
v1.post('/chat/completions', authMiddleware, validationMiddleware, handleChatCompletion);


// Add hooks
hooksManager.addHooks({
  beforeRequest: async (request, context) => {
    if (!request.messages.some(m => m.role === 'system')) {
      request.messages.unshift({
        role: 'system',
        content: 'You are a helpful AI assistant.'
      });
    }
    return request;
  },
  afterResponse: async (response, context) => {
    const headers = new Headers(response.headers);
    headers.set('x-gateway-version', '1.0.0');
    return new Response(response.body, {
      status: response.status,
      headers
    });
  },
  onError: async (error, context) => {
    console.error('Error in request:', error);
    return context.json({
      error: {
        message: error instanceof Error ? error.message : 'An unknown error occurred',
        type: 'gateway_error',
        code: 500
      }
    }, 500);
  }
});

// 404 handler
app.notFound((c) => {
  return c.json({
    error: {
      message: 'Not Found',
      type: 'not_found',
      code: 404
    }
  }, 404);
});

// Error handler
app.onError((err, c) => {
  console.error('Unhandled error:', err);
  return c.json({
    error: {
      message: 'Internal Server Error',
      type: 'internal_error',
      code: 500
    }
  }, 500);
});

// Export the app instance for direct use
export { app };

// Export default object for Cloudflare Workers
export default {
  fetch: app.fetch,
  async scheduled(event: ScheduledEvent, env: Env, ctx: ExecutionContext) {
    ctx.waitUntil(MetricsExportManager.getInstance().cleanup());
  }
};