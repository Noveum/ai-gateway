import { Provider, ProviderConfig } from '../types';
import { AIProvider } from './base';
import { OpenAIProvider } from './openai';
import { AnthropicProvider } from './anthropic';
import { BedrockProvider } from './bedrock';
import { GroqProvider } from './groq';

export class ProviderFactory {
  private static providers: Map<Provider, AIProvider> = new Map();

  static getProvider(provider: Provider, config: ProviderConfig): AIProvider {
    let instance = this.providers.get(provider);

    if (!instance) {
      instance = this.createProvider(provider);
      this.providers.set(provider, instance);
    }

    instance.initialize(config);
    return instance;
  }

  private static createProvider(provider: Provider): AIProvider {
    switch (provider) {
      case 'openai':
        return new OpenAIProvider();
      case 'anthropic':
        return new AnthropicProvider();
      case 'bedrock':
        return new BedrockProvider();
      case 'groq':
        return new GroqProvider();
      case 'fireworks':
        // TODO: Implement Fireworks provider
        throw new Error('Fireworks provider not implemented yet');
      case 'together':
        // TODO: Implement Together provider
        throw new Error('Together provider not implemented yet');
      default:
        throw new Error(`Provider ${provider} not supported`);
    }
  }
} 