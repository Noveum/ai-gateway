import { MiddlewareHandler } from 'hono';
import { Variables, Bindings } from '../types';

export const loggingMiddleware: MiddlewareHandler<{
  Variables: Variables;
  Bindings: Bindings;
}> = async (c, next) => {
  const requestId = crypto.randomUUID();
  const startTime = Date.now();
  
  // Log request
  const requestLog = {
    id: requestId,
    timestamp: new Date().toISOString(),
    method: c.req.method,
    path: c.req.path,
    provider: c.get('provider'),
    headers: Object.fromEntries(c.req.raw.headers.entries())
  };
  
  console.log('[Request]', JSON.stringify(requestLog));

  try {
    await next();
  } catch (error) {
    // Log error
    console.error('[Error]', {
      id: requestId,
      error: error instanceof Error ? error.message : 'Unknown error',
      stack: error instanceof Error ? error.stack : undefined
    });
    throw error;
  }

  // Log response
  const endTime = Date.now();
  const responseLog = {
    id: requestId,
    timestamp: new Date().toISOString(),
    duration: endTime - startTime,
    status: c.res.status,
    headers: Object.fromEntries(c.res.headers.entries())
  };

  console.log('[Response]', JSON.stringify(responseLog));
}; 