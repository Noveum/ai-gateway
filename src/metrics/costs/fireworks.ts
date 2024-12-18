import { ProviderModelCosts } from './types';

// Convert price from dollars per million tokens to dollars per token
const MILLION = 1_000_000;

// Pricing tiers per million tokens
const PRICING_TIERS = {
  SMALL: 0.10,      // 0B - 4B models
  MEDIUM: 0.20,     // 4B - 16B models
  LARGE: 0.90,      // 16.1B+ models
  MOE_SMALL: 0.50,  // MoE 0B - 56B (e.g. Mixtral 8x7B)
  MOE_LARGE: 1.20,  // MoE 56.1B - 176B (e.g. DBRX, Mixtral 8x22B)
  SPECIAL: 3.00     // Special models like Yi Large and Meta Llama 3.1 405B
};

// Helper function to normalize model names for matching
const normalizeModelName = (model: string): string => {
  return model.toLowerCase()
    .replace(/[-_\s]/g, '')  // Remove hyphens, underscores, and spaces
    .replace(/\.(\d)/g, '$1') // Convert .1 to 1, .2 to 2, etc.
    .replace(/v\d+(\.\d+)?/, '')  // Remove version numbers like v1, v0.1
    .replace(/\b(instruct|chat|base|preview|serverless)\b/g, ''); // Remove common suffixes
};

// Helper function to get the cost based on model name
const getModelCost = (model: string): number => {
  const modelNorm = normalizeModelName(model);
  
  // Special models
  if (modelNorm.includes('yilarge')) {
    return PRICING_TIERS.SPECIAL;
  }
  if (modelNorm.includes('llama31405b')) {
    return PRICING_TIERS.SPECIAL;
  }

  // MoE models
  if (modelNorm.includes('mixtral8x22b') || modelNorm.includes('dbrx')) {
    return PRICING_TIERS.MOE_LARGE;
  }
  if (modelNorm.includes('mixtral8x7b')) {
    return PRICING_TIERS.MOE_SMALL;
  }

  // Size-based models
  if (modelNorm.includes('72b') || modelNorm.includes('70b')) {
    return PRICING_TIERS.LARGE;
  }
  if (modelNorm.includes('34b') || modelNorm.includes('33b') || modelNorm.includes('32b')) {
    return PRICING_TIERS.LARGE;
  }
  if (modelNorm.includes('14b') || modelNorm.includes('13b') || modelNorm.includes('12b')) {
    return PRICING_TIERS.MEDIUM;
  }
  if (modelNorm.includes('7b') || modelNorm.includes('6b') || modelNorm.includes('3b')) {
    return PRICING_TIERS.MEDIUM;
  }
  if (modelNorm.includes('1b')) {
    return PRICING_TIERS.SMALL;
  }

  // Default to small for other models
  return PRICING_TIERS.SMALL;
};

const BASE_COSTS: ProviderModelCosts = {
  // Llama 3.1 models
  'llama-3.1-405b': {
    inputTokenCost: 3.00 / MILLION,  // $3.00 per million tokens
    outputTokenCost: 3.00 / MILLION
  },
  'llama-3.1-70b': {
    inputTokenCost: 0.90 / MILLION,  // $0.90 per million tokens
    outputTokenCost: 0.90 / MILLION
  },
  'llama-3.1-8b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },

  // Llama 3.2 models
  'llama-3.2-3b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },

  // Mixtral models
  'mixtral-8x22b': {
    inputTokenCost: 0.90 / MILLION,  // $0.90 per million tokens
    outputTokenCost: 0.90 / MILLION
  },
  'mixtral-8x7b': {
    inputTokenCost: 0.50 / MILLION,  // $0.50 per million tokens
    outputTokenCost: 0.50 / MILLION
  },

  // Qwen models
  'qwen2.5-coder-32b': {
    inputTokenCost: 0.90 / MILLION,  // $0.90 per million tokens
    outputTokenCost: 0.90 / MILLION
  },
  'qwen2-72b': {
    inputTokenCost: 0.90 / MILLION,  // $0.90 per million tokens
    outputTokenCost: 0.90 / MILLION
  },
  'qwen2.5-14b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },
  'qwen2.5-7b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },

  // Yi models
  'yi-large': {
    inputTokenCost: 3.00 / MILLION,  // $3.00 per million tokens
    outputTokenCost: 3.00 / MILLION
  },
  'yi-6b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },

  // Code models
  'code-llama-34b': {
    inputTokenCost: 0.90 / MILLION,  // $0.90 per million tokens
    outputTokenCost: 0.90 / MILLION
  },
  'code-llama-7b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },
  'starcoder-7b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },
  'starcoder2-15b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },
  'starcoder2-7b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },

  // Other models
  'llama-guard-7b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },
  'zephyr-7b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },
  'mistral-7b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },
  'pythia-12b': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  }
};

// Create a map with both original and normalized model names
const createModelCostsWithNormalized = (costs: ProviderModelCosts): ProviderModelCosts => {
  const normalizedCosts: ProviderModelCosts = { ...costs };
  
  // Add normalized versions of each model name
  Object.entries(costs).forEach(([model, cost]) => {
    normalizedCosts[normalizeModelName(model)] = cost;
  });
  
  // For any model not explicitly defined, we'll determine its cost based on its name
  return new Proxy(normalizedCosts, {
    get: (target, prop) => {
      if (typeof prop === 'string') {
        // Try exact match first
        if (prop in target) {
          return target[prop];
        }
        
        // Try normalized version
        const normalizedProp = normalizeModelName(prop);
        if (normalizedProp in target) {
          return target[normalizedProp];
        }
        
        // Fallback to size-based cost calculation
        const cost = getModelCost(prop);
        return {
          inputTokenCost: cost / MILLION,
          outputTokenCost: cost / MILLION
        };
      }
      return target[String(prop)];
    }
  });
};

export const FIREWORKS_COSTS = createModelCostsWithNormalized(BASE_COSTS); 