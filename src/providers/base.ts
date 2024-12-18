import { ChatCompletionRequest, ProviderConfig } from '../types';

export interface AIProvider {
  initialize(config: ProviderConfig): void;
  validateConfig(): void;
  chatCompletion(request: ChatCompletionRequest): Promise<Response>;
  transformRequest(request: ChatCompletionRequest): Promise<any>;
  transformResponse(response: any, request: ChatCompletionRequest): Promise<any>;
}

export abstract class BaseProvider implements AIProvider {
  protected config: ProviderConfig = {};

  initialize(config: ProviderConfig): void {
    this.config = config;
  }

  abstract validateConfig(): void;
  abstract chatCompletion(request: ChatCompletionRequest): Promise<Response>;
  abstract transformRequest(request: ChatCompletionRequest): Promise<any>;
  abstract transformResponse(response: any, request: ChatCompletionRequest): Promise<any>;

  protected handleError(error: any): Response {
    const errorResponse = {
      error: {
        message: error.message || 'An unknown error occurred',
        type: error.type || 'provider_error',
        code: error.status || 500,
        details: error.details || undefined
      }
    };

    console.error(`[${this.constructor.name}] Error:`, errorResponse);

    return new Response(JSON.stringify(errorResponse), {
      status: error.status || 500,
      headers: {
        'Content-Type': 'application/json'
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
} 