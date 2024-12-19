import { MiddlewareHandler } from 'hono';
import { Variables, Bindings, ChatCompletionRequest } from '../types';

export const validationMiddleware: MiddlewareHandler<{
  Variables: Variables;
  Bindings: Bindings;
}> = async (c, next) => {
  if (c.req.method !== 'POST') {
    return c.json(
      { error: { message: 'Method not allowed', type: 'validation_error', code: 405 } },
      405
    );
  }

  let body: ChatCompletionRequest;
  try {
    body = await c.req.json();
  } catch (error) {
    return c.json(
      { error: { message: 'Invalid JSON body', type: 'validation_error', code: 400 } },
      400
    );
  }

  // Validate required fields
  if (!body.model) {
    return c.json(
      { error: { message: 'Model is required', type: 'validation_error', code: 400 } },
      400
    );
  }

  if (!Array.isArray(body.messages) || body.messages.length === 0) {
    return c.json(
      { error: { message: 'Messages array is required and cannot be empty', type: 'validation_error', code: 400 } },
      400
    );
  }

  // Validate message format
  const invalidMessage = body.messages.find(
    msg => !msg.role || !msg.content || !['user', 'assistant', 'system'].includes(msg.role)
  );

  if (invalidMessage) {
    return c.json(
      { error: { message: 'Invalid message format', type: 'validation_error', code: 400 } },
      400
    );
  }

  // Validate numeric parameters
  if (body.temperature !== undefined && (typeof body.temperature !== 'number' || body.temperature < 0 || body.temperature > 2)) {
    return c.json(
      { error: { message: 'temperature must be between 0 and 2', type: 'validation_error', code: 400 } },
      400
    );
  }

  if (body.top_p !== undefined && (typeof body.top_p !== 'number' || body.top_p < 0 || body.top_p > 1)) {
    return c.json(
      { error: { message: 'top_p must be between 0 and 1', type: 'validation_error', code: 400 } },
      400
    );
  }

  if (body.frequency_penalty !== undefined && (typeof body.frequency_penalty !== 'number' || body.frequency_penalty < -2 || body.frequency_penalty > 2)) {
    return c.json(
      { error: { message: 'frequency_penalty must be between -2 and 2', type: 'validation_error', code: 400 } },
      400
    );
  }

  if (body.presence_penalty !== undefined && (typeof body.presence_penalty !== 'number' || body.presence_penalty < -2 || body.presence_penalty > 2)) {
    return c.json(
      { error: { message: 'presence_penalty must be between -2 and 2', type: 'validation_error', code: 400 } },
      400
    );
  }

  // Validate token limits
  if (body.max_tokens !== undefined && (typeof body.max_tokens !== 'number' || body.max_tokens <= 0)) {
    return c.json(
      { error: { message: 'max_tokens must be a positive number', type: 'validation_error', code: 400 } },
      400
    );
  }

  if (body.max_completion_tokens !== undefined && (typeof body.max_completion_tokens !== 'number' || body.max_completion_tokens <= 0)) {
    return c.json(
      { error: { message: 'max_completion_tokens must be a positive number', type: 'validation_error', code: 400 } },
      400
    );
  }

  // Validate tools/functions
  if (body.tools && (!Array.isArray(body.tools) || body.tools.length > 128)) {
    return c.json(
      { error: { message: 'tools must be an array with max 128 items', type: 'validation_error', code: 400 } },
      400
    );
  }

  // Validate response format
  if (body.response_format && !['text', 'json_object', 'json_schema'].includes(body.response_format.type)) {
    return c.json(
      { error: { message: 'Invalid response_format type', type: 'validation_error', code: 400 } },
      400
    );
  }

  // Validate modalities
  if (body.modalities && (!Array.isArray(body.modalities) || !body.modalities.every(m => ['text', 'audio'].includes(m)))) {
    return c.json(
      { error: { message: 'Invalid modalities', type: 'validation_error', code: 400 } },
      400
    );
  }

  // Validate reasoning effort
  if (body.reasoning_effort && !['low', 'medium', 'high'].includes(body.reasoning_effort)) {
    return c.json(
      { error: { message: 'Invalid reasoning_effort value', type: 'validation_error', code: 400 } },
      400
    );
  }

  await next();
}; 