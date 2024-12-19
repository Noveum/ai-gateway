import { BaseProvider } from './base';
import { ChatCompletionRequest } from '../types';

export class OpenAIProvider extends BaseProvider {
  private readonly OPENAI_API_URL = 'https://api.openai.com/v1/chat/completions';

  validateConfig(): void {
    if (!this.config.apiKey) {
      throw new Error('OpenAI API key is required');
    }
  }

  async transformRequest(request: ChatCompletionRequest): Promise<any> {
    const requestBody: Partial<ChatCompletionRequest> = {
      messages: request.messages,
      model: request.model,
      store: request.store,
      reasoning_effort: request.reasoning_effort,
      metadata: request.metadata,
      frequency_penalty: request.frequency_penalty,
      logit_bias: request.logit_bias,
      logprobs: request.logprobs,
      top_logprobs: request.top_logprobs,
      max_tokens: request.max_tokens,
      max_completion_tokens: request.max_completion_tokens,
      n: request.n,
      modalities: request.modalities,
      prediction: request.prediction,
      audio: request.audio,
      presence_penalty: request.presence_penalty,
      response_format: request.response_format,
      seed: request.seed,
      service_tier: request.service_tier,
      stop: request.stop,
      stream: request.stream,
      stream_options: request.stream_options,
      temperature: request.temperature,
      top_p: request.top_p,
      tools: request.tools,
      tool_choice: request.tool_choice,
      parallel_tool_calls: request.parallel_tool_calls,
      user: request.user
    };

    // Handle deprecated fields for backward compatibility
    if (request.functions) {
      console.warn('[OpenAI] Warning: Using deprecated "functions" parameter. Please use "tools" instead.');
      requestBody.functions = request.functions;
    }
    if (request.function_call) {
      console.warn('[OpenAI] Warning: Using deprecated "function_call" parameter. Please use "tool_choice" instead.');
      requestBody.function_call = request.function_call;
    }

    // Remove undefined values to keep the request clean
    const cleanedBody = Object.fromEntries(
      Object.entries(requestBody).filter(([_, value]) => value !== undefined)
    );

    return cleanedBody;
  }

  async transformResponse(response: Response, request: ChatCompletionRequest): Promise<Response> {
    const headers = new Headers();
    response.headers.forEach((value, key) => headers.set(key, value));
    
    // Ensure content type is set correctly for streaming
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

      console.debug('[OpenAI] Request:', {
        url: this.OPENAI_API_URL,
        model: request.model,
        messageCount: request.messages.length,
        stream: request.stream,
        max_tokens: request.max_tokens || request.max_completion_tokens
      });

      const headers = new Headers({
        'Content-Type': 'application/json',
        'Authorization': `Bearer ${this.config.apiKey}`,
        'OpenAI-Beta': request.response_format?.type === 'json_schema' ? 'assistants=v1' : ''
      });

      if (this.config.organization) {
        headers.set('OpenAI-Organization', this.config.organization);
      }

      const response = await fetch(this.OPENAI_API_URL, {
        method: 'POST',
        headers,
        body: JSON.stringify(requestBody)
      });

      console.debug('[OpenAI] Response status:', response.status);

      if (!response.ok) {
        const errorData = await response.json() as { error?: { message?: string } };
        console.error('[OpenAI] Error:', errorData);
        throw {
          message: errorData.error?.message || 'OpenAI API request failed',
          status: response.status,
          type: 'openai_error'
        };
      }

      console.debug('[OpenAI] Success: Forwarding response');

      // Set status in metrics collector for both streaming and non-streaming responses
      if (this.metricsCollector) {
        this.metricsCollector.setStatus(response.status);
      }

      // Clone the response for metrics collection
      const responseClone = response.clone();
      
      // Handle metrics collection in the background
      if (!request.stream) {
        responseClone.json().then(data => {
          if (this.metricsCollector) {
            const metricsData = this.extractMetrics(data);
            if (metricsData?.tokens) {
              this.metricsCollector.setTokenUsage(metricsData.tokens);
            }
            if (metricsData?.metadata) {
              this.metricsCollector.setMetadata(metricsData.metadata);
            }
            this.metricsCollector.setStatus(response.status);
            this.metricsCollector.finish();
          }
        }).catch(error => {
          console.error('[OpenAI] Failed to collect metrics:', error);
        });
      }

      // Return transformed response immediately
      return this.transformResponse(response, request);
    } catch (error) {
      console.error('[OpenAI] Caught error:', error);
      return this.handleError(error);
    }
  }

  extractMetrics(data: any) {
    if (!data) return null;

    return {
      tokens: {
        inputTokens: data.usage?.prompt_tokens,
        outputTokens: data.usage?.completion_tokens,
        totalTokens: data.usage?.total_tokens
      },
      metadata: {
        model: data.model,
        systemFingerprint: data.system_fingerprint,
        finishReason: data.choices?.[0]?.finish_reason
      }
    };
  }
} 