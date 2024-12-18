import { Context } from 'hono';
import { Variables, Bindings, ChatCompletionRequest } from '../types';
import { ProviderFactory } from '../providers/factory';
import { hooksManager } from '../hooks';

export const handleChatCompletion = async (c: Context<{ Variables: Variables; Bindings: Bindings }>) => {
  try {
    const provider = c.get('provider');
    const config = c.get('config');
    console.debug('[ChatCompletion] Starting request handling', { provider });
    
    // Get request body
    const originalRequest = await c.req.json<ChatCompletionRequest>();
    console.debug('[ChatCompletion] Original request', { originalRequest });

    // Transform request through hooks
    const request = await hooksManager.transformRequest(originalRequest, c);
    console.debug('[ChatCompletion] Transformed request', { request });

    // Get provider instance
    const providerInstance = ProviderFactory.getProvider(provider, config);
    console.debug('[ChatCompletion] Provider instance created', { providerType: provider });

    // Make the request to the provider
    console.debug('[ChatCompletion] Making request to provider');
    const response = await providerInstance.chatCompletion(request);
    console.debug('[ChatCompletion] Received provider response', { response });

    // Transform response through hooks
    const transformedResponse = await hooksManager.transformResponse(response, c);
    console.debug('[ChatCompletion] Final transformed response', { transformedResponse });

    return transformedResponse;
  } catch (error) {
    console.debug('[ChatCompletion] Error in main try block', { error });
    // Let hooks try to handle the error first
    try {
      return await hooksManager.handleError(error, c);
    } catch (e) {
      console.debug('[ChatCompletion] Error in error handling', { error: e });
      // If no hook handles the error, return a generic error response
      return c.json({
        error: {
          message: error instanceof Error ? error.message : 'An unknown error occurred',
          type: 'request_error',
          code: 500
        }
      }, 500);
    }
  }
}; 