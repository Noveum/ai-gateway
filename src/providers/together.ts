import { BaseProvider } from './base';
import { ChatCompletionRequest, ProviderConfig } from '../types';

interface TogetherErrorResponse {
  error?: {
    message?: string;
  };
}

export class TogetherProvider extends BaseProvider {
  private readonly API_BASE = 'https://api.together.xyz/v1';

  validateConfig(): void {
    if (!this.config.apiKey) {
      throw new Error('Together API key is required');
    }
  }

  async chatCompletion(request: ChatCompletionRequest): Promise<Response> {
    try {
      this.validateConfig();
      const transformedRequest = await this.transformRequest(request);
      const metrics = this.initializeMetrics(request);
      const headers = this.createHeaders({
        'Authorization': `Bearer ${this.config.apiKey}`,
      });

      const response = await fetch(`${this.API_BASE}/chat/completions`, {
        method: 'POST',
        headers,
        body: JSON.stringify(transformedRequest),
      });

      if (!response.ok) {
        const error = await response.json() as TogetherErrorResponse;
        throw new Error(error.error?.message || 'Together API request failed');
      }

      return this.wrapResponseWithMetrics(response, request, this.extractMetrics);
    } catch (error) {
      return this.handleError(error);
    }
  }

  async transformRequest(request: ChatCompletionRequest): Promise<any> {
    // Transform the request to match Together API format
    const transformedRequest = {
      model: request.model,
      messages: request.messages,
      stream: request.stream || false,
      max_tokens: request.max_tokens,
      temperature: request.temperature,
      top_p: request.top_p,
      top_k: request.top_k || 50,
      repetition_penalty: request.repetition_penalty || 1,
      stop: request.stop,
    };

    return this.cleanRequestBody(transformedRequest);
  }

  async transformResponse(response: any, request: ChatCompletionRequest): Promise<any> {
    // The response format is already compatible with OpenAI's format
    return response;
  }

  extractMetrics(data: any) {
    if (!data) return null;

    // Handle streaming response
    if (data.object === 'chat.completion.chunk') {
      return {
        tokens: data.usage ? {
          inputTokens: data.usage.prompt_tokens,
          outputTokens: data.usage.completion_tokens,
          totalTokens: data.usage.total_tokens
        } : undefined,
        metadata: {
          finishReason: data.choices?.[0]?.finish_reason,
          model: data.model,
          seed: data.choices?.[0]?.seed
        }
      };
    }

    // Handle regular response
    if (data.usage) {
      return {
        tokens: {
          inputTokens: data.usage.prompt_tokens,
          outputTokens: data.usage.completion_tokens,
          totalTokens: data.usage.total_tokens
        },
        metadata: {
          finishReason: data.choices?.[0]?.finish_reason,
          model: data.model,
          seed: data.choices?.[0]?.seed
        }
      };
    }

    return null;
  }
} 