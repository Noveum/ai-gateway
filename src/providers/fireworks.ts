import { BaseProvider } from './base';
import { ChatCompletionRequest, ProviderConfig } from '../types';

interface FireworksErrorResponse {
  error?: {
    message?: string;
  };
}

export class FireworksProvider extends BaseProvider {
  private readonly API_BASE = 'https://api.fireworks.ai/inference/v1';

  validateConfig(): void {
    if (!this.config.apiKey) {
      throw new Error('Fireworks API key is required');
    }
  }

  async chatCompletion(request: ChatCompletionRequest): Promise<Response> {
    try {
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
        const error = await response.json() as FireworksErrorResponse;
        throw new Error(error.error?.message || 'Fireworks API request failed');
      }

      return this.wrapResponseWithMetrics(response, request, this.extractMetrics);
    } catch (error) {
      return this.handleError(error);
    }
  }

  async transformRequest(request: ChatCompletionRequest): Promise<any> {
    // Transform the request to match Fireworks API format
    const transformedRequest = {
      model: request.model,
      messages: request.messages,
      stream: request.stream || false,
      max_tokens: request.max_tokens,
      temperature: request.temperature || 0.7,
      top_p: request.top_p || 1,
      top_k: 40, // Fireworks specific parameter
      presence_penalty: request.presence_penalty || 0,
      frequency_penalty: request.frequency_penalty || 0,
    };

    return this.cleanRequestBody(transformedRequest);
  }

  async transformResponse(response: any, request: ChatCompletionRequest): Promise<any> {
    // The response format is already compatible with OpenAI's format
    return response;
  }

  extractMetrics(data: any) {
    if (!data) return null;

    // Handle both streaming and non-streaming responses
    const usage = data.usage || {};

    return {
      tokens: {
        inputTokens: usage.prompt_tokens,
        outputTokens: usage.completion_tokens,
        totalTokens: usage.total_tokens
      },
      metadata: {
        model: data.model,
        finishReason: data.choices?.[0]?.finish_reason,
        requestId: data.id
      }
    };
  }
} 