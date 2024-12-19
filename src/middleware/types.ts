import { Context } from 'hono';
import { ChatCompletionRequest, Variables, Bindings } from '../types';

export type MiddlewareContext = Context<{ Variables: Variables; Bindings: Bindings }>;

export type RequestTransformer = (
  request: ChatCompletionRequest,
  context: MiddlewareContext
) => Promise<ChatCompletionRequest> | ChatCompletionRequest;

export type ResponseTransformer = (
  response: Response,
  context: MiddlewareContext
) => Promise<Response> | Response;

export type ErrorHandler = (
  error: any,
  context: MiddlewareContext
) => Promise<Response> | Response;

export interface MiddlewareHooks {
  beforeRequest?: RequestTransformer;
  afterResponse?: ResponseTransformer;
  onError?: ErrorHandler;
} 