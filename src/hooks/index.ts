import { MiddlewareHooks, RequestTransformer, ResponseTransformer, ErrorHandler } from '../middleware/types';

class HooksManager {
  private hooks: MiddlewareHooks[] = [];

  addHooks(hooks: MiddlewareHooks) {
    this.hooks.push(hooks);
  }

  async transformRequest(request: any, context: any) {
    let transformedRequest = request;
    for (const hook of this.hooks) {
      if (hook.beforeRequest) {
        transformedRequest = await hook.beforeRequest(transformedRequest, context);
      }
    }
    return transformedRequest;
  }

  async transformResponse(response: Response, context: any) {
    let transformedResponse = response;
    for (const hook of this.hooks) {
      if (hook.afterResponse) {
        transformedResponse = await hook.afterResponse(transformedResponse, context);
      }
    }
    return transformedResponse;
  }

  async handleError(error: any, context: any) {
    for (const hook of this.hooks) {
      if (hook.onError) {
        try {
          return await hook.onError(error, context);
        } catch (e) {
          console.error('Error in error handler:', e);
          continue;
        }
      }
    }
    throw error;
  }
}

export const hooksManager = new HooksManager(); 