import { BaseProvider } from './base';
import { ChatCompletionRequest } from '../types';

export class GroqProvider extends BaseProvider {
  private readonly GROQ_API_URL = 'https://api.groq.com/openai/v1/chat/completions';

  validateConfig(): void {
    if (!this.config.apiKey) {
      throw new Error('Groq API key is required');
    }
  }

  async transformRequest(request: ChatCompletionRequest): Promise<any> {
    // GROQ uses OpenAI-compatible format, so we can reuse most fields
    const requestBody = {
      messages: request.messages,
      model: request.model,
      max_tokens: request.max_tokens,
      temperature: request.temperature,
      top_p: request.top_p,
      stream: request.stream,
      stop: request.stop,
      n: request.n,
      presence_penalty: request.presence_penalty,
      frequency_penalty: request.frequency_penalty,
      user: request.user
    };

    return this.cleanRequestBody(requestBody);
  }

  async transformResponse(response: Response, request: ChatCompletionRequest): Promise<Response> {
    // GROQ returns OpenAI-compatible responses, so we can forward them directly
    const headers = new Headers();
    response.headers.forEach((value, key) => headers.set(key, value));
    
    if (request.stream) {
      headers.set('Content-Type', 'text/event-stream');
      headers.set('Cache-Control', 'no-cache');
      headers.set('Connection', 'keep-alive');
    }

    return new Response(response.body, {
      status: response.status,
      headers
    });
  }

  async chatCompletion(request: ChatCompletionRequest): Promise<Response> {
    try {
      this.validateConfig();
      const requestBody = await this.transformRequest(request);
      const metrics = this.initializeMetrics(request);

      console.debug('[Groq] Request:', {
        url: this.GROQ_API_URL,
        model: request.model,
        messageCount: request.messages.length,
        stream: request.stream,
        max_tokens: request.max_tokens
      });

      const response = await fetch(this.GROQ_API_URL, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'Authorization': `Bearer ${this.config.apiKey}`
        },
        body: JSON.stringify(requestBody)
      });

      if (!response.ok) {
        const errorData = await response.json() as { error?: { message?: string } };
        throw {
          message: errorData.error?.message || 'Groq API request failed',
          status: response.status,
          type: 'groq_error'
        };
      }

      return this.wrapResponseWithMetrics(response, request, this.extractMetrics);
    } catch (error) {
      return this.handleError(error);
    }
  }

  extractMetrics(data: any) {
    if (!data) return null;

    const groqMetrics = data.x_groq?.usage || {};
    const usage = data.usage || {};

    return {
      tokens: {
        inputTokens: usage.prompt_tokens || groqMetrics.prompt_tokens,
        outputTokens: usage.completion_tokens || groqMetrics.completion_tokens,
        totalTokens: usage.total_tokens || groqMetrics.total_tokens
      },
      performance: {
        queueTime: groqMetrics.queue_time,
        promptTime: groqMetrics.prompt_time,
        completionTime: groqMetrics.completion_time,
        totalTime: groqMetrics.total_time
      },
      metadata: {
        systemFingerprint: data.system_fingerprint,
        finishReason: data.choices?.[0]?.finish_reason,
        requestId: data.x_groq?.id
      }
    };
  }
}
