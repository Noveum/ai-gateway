import { MiddlewareHandler } from 'hono';
import { Variables, Bindings, Provider } from '../types';

export const authMiddleware: MiddlewareHandler<{
  Variables: Variables;
  Bindings: Bindings;
}> = async (c, next) => {
  const provider = c.req.header('x-provider')?.toLowerCase() as Provider;
  
  if (!provider) {
    return c.json(
      { error: { message: 'Provider not specified', type: 'auth_error', code: 400 } },
      400
    );
  }

  const config: Record<string, string | undefined> = {};

  // Handle provider-specific authentication
  switch (provider) {
    case 'openai':
      const openaiKey = c.req.header('Authorization')?.replace('Bearer ', '') || c.env.OPENAI_API_KEY;
      if (!openaiKey) {
        return c.json(
          { error: { message: 'OpenAI API key not provided', type: 'auth_error', code: 401 } },
          401
        );
      }
      config.apiKey = openaiKey;
      break;

    case 'anthropic':
      const anthropicKey = c.req.header('Authorization')?.replace('Bearer ', '') || c.env.ANTHROPIC_API_KEY;
      if (!anthropicKey) {
        return c.json(
          { error: { message: 'Anthropic API key not provided', type: 'auth_error', code: 401 } },
          401
        );
      }
      config.apiKey = anthropicKey;
      break;

    case 'bedrock':
      const accessKeyId = c.req.header('x-aws-access-key-id') || c.env.AWS_ACCESS_KEY_ID;
      const secretKey = c.req.header('x-aws-secret-access-key') || c.env.AWS_SECRET_ACCESS_KEY;
      const region = c.req.header('x-aws-region') || c.env.AWS_REGION || 'us-east-1';

      if (!accessKeyId || !secretKey) {
        return c.json(
          { error: { message: 'AWS credentials not provided', type: 'auth_error', code: 401 } },
          401
        );
      }

      config.awsAccessKeyId = accessKeyId;
      config.awsSecretAccessKey = secretKey;
      config.awsRegion = region;
      break;

    case 'groq':
    case 'fireworks':
    case 'together':
      const apiKey = c.req.header('Authorization')?.replace('Bearer ', '');
      const envKey = c.env[`${provider.toUpperCase()}_API_KEY`];
      
      if (!apiKey && !envKey) {
        return c.json(
          { error: { message: `${provider} API key not provided`, type: 'auth_error', code: 401 } },
          401
        );
      }
      config.apiKey = apiKey || envKey;
      break;

    default:
      return c.json(
        { error: { message: 'Invalid provider specified', type: 'auth_error', code: 400 } },
        400
      );
  }

  // Set provider and config in context for downstream middleware and handlers
  c.set('provider', provider);
  c.set('config', config);

  await next();
}; 