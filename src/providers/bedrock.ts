import { BaseProvider } from './base';
import { ChatCompletionRequest } from '../types';

export class BedrockProvider extends BaseProvider {
  private readonly BEDROCK_API_URL = 'https://bedrock-runtime.{region}.amazonaws.com/model/anthropic.claude-3-sonnet-20240229-v1:0/invoke';

  validateConfig(): void {
    if (!this.config.awsAccessKeyId || !this.config.awsSecretAccessKey || !this.config.awsRegion) {
      throw new Error('AWS credentials (access key, secret key, and region) are required');
    }
  }

  async transformRequest(request: ChatCompletionRequest): Promise<any> {
    return {
      modelId: request.model,
      input: {
        messages: request.messages
      },
      stream: request.stream,
      max_tokens: request.max_tokens
    };
  }

  async transformResponse(response: Response, request: ChatCompletionRequest): Promise<Response> {
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

      const apiUrl = this.BEDROCK_API_URL.replace('{region}', this.config.awsRegion!);
      const requestBody = await this.transformRequest(request);
      
      // AWS Signature v4 signing would go here
      // For brevity, we're skipping the actual signing process
      // In a real implementation, you'd need to properly sign the request

      const response = await fetch(apiUrl, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'X-Amz-Content-Sha256': 'UNSIGNED-PAYLOAD',
          'X-Amz-Date': new Date().toISOString().replace(/[:-]|\.\d{3}/g, '')
        },
        body: JSON.stringify(requestBody)
      });

      if (!response.ok) {
        const error = await response.json();
        throw {
          message: error.message || 'AWS Bedrock request failed',
          status: response.status,
          type: 'bedrock_error'
        };
      }

      return this.transformResponse(response, request);
    } catch (error) {
      return this.handleError(error);
    }
  }
} 