import { BaseProvider } from './base';
import { ChatCompletionRequest } from '../types';

export class AnthropicProvider extends BaseProvider {
  private readonly ANTHROPIC_API_URL = 'https://api.anthropic.com/v1/messages';
  private readonly API_VERSION = '2023-06-01';

  validateConfig(): void {
    if (!this.config.apiKey) {
      throw new Error('Anthropic API key is required');
    }
  }

  async transformRequest(request: ChatCompletionRequest): Promise<any> {
    // Extract system message if present
    const systemMessage = request.messages.find(m => m.role === 'system');
    const userMessages = request.messages.filter(m => m.role !== 'system');

    // Transform to Anthropic's format
    return {
      model: request.model,
      messages: userMessages.map(msg => ({
        role: msg.role === 'assistant' ? 'assistant' : 'user',
        content: msg.content
      })),
      system: systemMessage?.content,
      stream: request.stream,
      max_tokens: request.max_tokens,
      metadata: request.metadata,
      temperature: request.temperature,
      top_p: request.top_p,
      stop: request.stop
    };
  }

  async transformResponse(response: Response, request: ChatCompletionRequest): Promise<Response> {
    if (!request.stream) {
      const anthropicResponse = await response.json();
      const openAIFormat = {
        id: 'chatcmpl-' + crypto.randomUUID(),
        object: 'chat.completion',
        created: Date.now(),
        model: request.model,
        choices: [{
          index: 0,
          message: {
            role: 'assistant',
            content: anthropicResponse.content[0].text
          },
          finish_reason: anthropicResponse.stop_reason || 'stop'
        }],
        usage: {
          prompt_tokens: anthropicResponse.usage?.input_tokens || 0,
          completion_tokens: anthropicResponse.usage?.output_tokens || 0,
          total_tokens: (anthropicResponse.usage?.input_tokens || 0) + (anthropicResponse.usage?.output_tokens || 0)
        }
      };

      return new Response(JSON.stringify(openAIFormat), {
        headers: this.createHeaders()
      });
    }

    // Create a new ReadableStream for the transformed data
    const stream = new ReadableStream({
      async start(controller) {
        const reader = response.body!.getReader();
        const decoder = new TextDecoder();
        let buffer = '';
        let messageId = '';
        let inputTokens = 0;
        let outputTokens = 0;

        try {
          while (true) {
            const { done, value } = await reader.read();
            if (done) break;

            buffer += decoder.decode(value, { stream: true });
            const lines = buffer.split('\n');
            buffer = lines.pop() || '';

            for (const line of lines) {
              if (line.trim() === '') continue;

              const eventMatch = line.match(/^event: (.+)$/);
              const dataMatch = line.match(/^data: (.+)$/);

              if (dataMatch) {
                try {
                  const data = JSON.parse(dataMatch[1]);

                  switch (data.type) {
                    case 'message_start':
                      messageId = data.message.id;
                      if (data.message.usage) {
                        inputTokens = data.message.usage.input_tokens || 0;
                      }
                      break;

                    case 'content_block_delta':
                      if (data.delta.type === 'text_delta') {
                        const chunk = {
                          id: messageId || 'chatcmpl-' + crypto.randomUUID(),
                          object: 'chat.completion.chunk',
                          created: Date.now(),
                          model: request.model,
                          choices: [{
                            index: 0,
                            delta: {
                              content: data.delta.text
                            },
                            finish_reason: null
                          }]
                        };
                        controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify(chunk)}\n\n`));
                      }
                      break;

                    case 'message_delta':
                      if (data.usage?.output_tokens) {
                        outputTokens = data.usage.output_tokens;
                      }
                      if (data.delta.stop_reason) {
                        const finalChunk = {
                          id: messageId || 'chatcmpl-' + crypto.randomUUID(),
                          object: 'chat.completion.chunk',
                          created: Date.now(),
                          model: request.model,
                          choices: [{
                            index: 0,
                            delta: {},
                            finish_reason: data.delta.stop_reason
                          }],
                          usage: {
                            prompt_tokens: inputTokens,
                            completion_tokens: outputTokens,
                            total_tokens: inputTokens + outputTokens
                          }
                        };
                        controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify(finalChunk)}\n\n`));
                        controller.enqueue(new TextEncoder().encode('data: [DONE]\n\n'));
                      }
                      break;
                  }
                } catch (error) {
                  console.error('Error parsing stream data:', error);
                  continue;
                }
              }
            }
          }
        } catch (error) {
          console.error('Stream processing error:', error);
          controller.error(error);
        } finally {
          controller.close();
          reader.releaseLock();
        }
      }
    });

    return new Response(stream, {
      headers: this.createStreamHeaders()
    });
  }

  async chatCompletion(request: ChatCompletionRequest): Promise<Response> {
    try {
      this.validateConfig();
      const requestBody = await this.transformRequest(request);

      const response = await fetch(this.ANTHROPIC_API_URL, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'x-api-key': this.config.apiKey!,
          'anthropic-version': this.API_VERSION
        },
        body: JSON.stringify(requestBody)
      });

      if (!response.ok) {
        const error = await response.json() as { error?: { message?: string } };
        throw {
          message: error.error?.message || 'Anthropic API request failed',
          status: response.status,
          type: 'anthropic_error'
        };
      }

      return this.transformResponse(response, request);
    } catch (error) {
      return this.handleError(error);
    }
  }
} 