import { Provider, ProviderConfig } from '../types';
import { AIProvider } from './base';
import { OpenAIProvider } from './openai';
import { AnthropicProvider } from './anthropic';
import { BedrockProvider } from './bedrock';
import { GroqProvider } from './groq';
import { FireworksProvider } from './fireworks';
import { TogetherProvider } from './together';

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
        return new FireworksProvider();
      case 'together':
        return new TogetherProvider();
      default:
        throw new Error(`Provider ${provider} not supported`);
    }
  }
} 