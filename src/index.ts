import { Hono } from 'hono';
import { cors } from 'hono/cors';
import { logger } from 'hono/logger';
import { prettyJSON } from 'hono/pretty-json';
import { Variables, Bindings } from './types';
import { authMiddleware } from './middleware/auth';
import { validationMiddleware } from './middleware/validation';
import { loggingMiddleware } from './middleware/logging';
import { handleChatCompletion } from './handlers/chat';
import { hooksManager } from './hooks';

// Create the application
const app = new Hono<{ Variables: Variables; Bindings: Bindings }>();

// Add global middleware
app.use('*', cors());
app.use('*', logger());
app.use('*', prettyJSON());
app.use('*', loggingMiddleware);

// Health check endpoint
app.get('/health', (c) => c.json({ status: 'ok' }));

// API version prefix
const v1 = app.basePath('/v1');

// Chat completion endpoint
v1.post('/chat/completions', authMiddleware, validationMiddleware, handleChatCompletion);

// Add some example hooks
hooksManager.addHooks({
  beforeRequest: async (request, context) => {
    // Example: Add system message if not present
    if (!request.messages.some(m => m.role === 'system')) {
      request.messages.unshift({
        role: 'system',
        content: 'You are a helpful AI assistant.'
      });
    }
    return request;
  },
  afterResponse: async (response, context) => {
    // Example: Add custom header
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

export default app;