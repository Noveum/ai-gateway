import { ProviderModelCosts } from './types';

// Convert price from dollars per million tokens to dollars per token
const MILLION = 1_000_000;

// Model types and their pricing tiers
enum ModelType {
  LITE = 'lite',
  TURBO = 'turbo',
  REFERENCE = 'reference'
}

// Pricing structure for Llama models based on size and type ($ per million tokens)
const LLAMA_PRICING: Record<string, Record<string, number>> = {
  '3b': {
    [ModelType.TURBO]: 0.06
  },
  '8b': {
    [ModelType.LITE]: 0.10,
    [ModelType.TURBO]: 0.18,
    [ModelType.REFERENCE]: 0.20
  },
  '11b': {
    [ModelType.TURBO]: 0.18  // Vision model
  },
  '70b': {
    [ModelType.LITE]: 0.54,
    [ModelType.TURBO]: 0.88,
    [ModelType.REFERENCE]: 0.90
  },
  '90b': {
    [ModelType.TURBO]: 1.20  // Vision model
  },
  '405b': {
    [ModelType.TURBO]: 3.50
  }
};

// Pricing for Qwen models ($ per million tokens)
const QWEN_PRICING: Record<string, number> = {
  'qwen2-72b': 0.90,
  'qwen25-7b': 0.30,
  'qwen25-72b': 1.20,
  'qwen25-coder-32b': 0.80,
  'qwen-qwq-32b': 1.20  // Preview model
};

// Helper function to normalize model names for matching
const normalizeModelName = (model: string): string => {
  return model.toLowerCase()
    .replace(/[-_\s]/g, '')  // Remove hyphens, underscores, and spaces
    .replace(/\.(\d)/g, '$1') // Convert .1 to 1, .2 to 2, etc.
    .replace(/v\d+(\.\d+)?/, '')  // Remove version numbers like v1, v0.1
    .replace(/\b(instruct|chat|base|preview|serverless)\b/g, ''); // Remove common suffixes
};

// Helper function to determine model size and type from name
const getModelDetails = (model: string): { size: string; type: ModelType } | null => {
  const modelNorm = normalizeModelName(model);
  
  // Determine size
  let size = '';
  if (modelNorm.includes('405b')) size = '405b';
  else if (modelNorm.includes('90b')) size = '90b';
  else if (modelNorm.includes('70b')) size = '70b';
  else if (modelNorm.includes('11b')) size = '11b';
  else if (modelNorm.includes('8b')) size = '8b';
  else if (modelNorm.includes('3b')) size = '3b';
  
  if (!size) return null;

  // Determine type
  let type = ModelType.REFERENCE;
  if (modelNorm.includes('lite')) type = ModelType.LITE;
  else if (modelNorm.includes('turbo')) type = ModelType.TURBO;

  return { size, type };
};

// Helper function to get cost based on model name
const getModelCost = (model: string): number => {
  const normalizedModel = normalizeModelName(model);
  
  // Check Qwen models first
  for (const [qwenModel, cost] of Object.entries(QWEN_PRICING)) {
    if (normalizedModel.includes(normalizeModelName(qwenModel))) {
      return cost;
    }
  }
  
  // Fall back to Llama pricing
  const details = getModelDetails(model);
  if (!details) return 0.20; // Default to medium tier pricing
  
  const { size, type } = details;
  return LLAMA_PRICING[size]?.[type] || 0.20;
};

const BASE_COSTS: ProviderModelCosts = {
  // Llama 3.1 models
  'meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo': {
    inputTokenCost: 3.50 / MILLION,  // $3.50 per million tokens
    outputTokenCost: 3.50 / MILLION
  },
  'meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo': {
    inputTokenCost: 0.88 / MILLION,  // $0.88 per million tokens
    outputTokenCost: 0.88 / MILLION
  },
  'meta-llama/Meta-Llama-3.1-8B-Instruct-Turbo': {
    inputTokenCost: 0.18 / MILLION,  // $0.18 per million tokens
    outputTokenCost: 0.18 / MILLION
  },
  'meta-llama/Meta-Llama-3.1-8B-Instruct-Lite': {
    inputTokenCost: 0.10 / MILLION,  // $0.10 per million tokens
    outputTokenCost: 0.10 / MILLION
  },

  // Llama 3.2 models
  'meta-llama/Llama-3.2-3B-Instruct-Turbo': {
    inputTokenCost: 0.06 / MILLION,  // $0.06 per million tokens
    outputTokenCost: 0.06 / MILLION
  },
  'meta-llama/Llama-3.2-90B-Vision-Instruct-Turbo': {
    inputTokenCost: 1.20 / MILLION,  // $1.20 per million tokens
    outputTokenCost: 1.20 / MILLION
  },
  'meta-llama/Llama-3.2-11B-Vision-Instruct-Turbo': {
    inputTokenCost: 0.18 / MILLION,  // $0.18 per million tokens
    outputTokenCost: 0.18 / MILLION
  },

  // Mixtral models
  'mistralai/Mixtral-8x7B-Instruct-v0.1': {
    inputTokenCost: 0.50 / MILLION,  // $0.50 per million tokens
    outputTokenCost: 0.50 / MILLION
  },
  'mistralai/Mixtral-8x22B-Instruct-v0.1': {
    inputTokenCost: 1.20 / MILLION,  // $1.20 per million tokens
    outputTokenCost: 1.20 / MILLION
  },

  // Qwen models with updated pricing
  'Qwen/Qwen2-72B-Instruct': {
    inputTokenCost: 0.90 / MILLION,  // $0.90 per million tokens
    outputTokenCost: 0.90 / MILLION
  },
  'Qwen/Qwen2.5-7B-Instruct-Turbo': {
    inputTokenCost: 0.30 / MILLION,  // $0.30 per million tokens
    outputTokenCost: 0.30 / MILLION
  },
  'Qwen/Qwen2.5-72B-Instruct-Turbo': {
    inputTokenCost: 1.20 / MILLION,  // $1.20 per million tokens
    outputTokenCost: 1.20 / MILLION
  },
  'Qwen/Qwen2.5-Coder-32B-Instruct': {
    inputTokenCost: 0.80 / MILLION,  // $0.80 per million tokens
    outputTokenCost: 0.80 / MILLION
  },
  'Qwen/QwQ-32B-Preview': {
    inputTokenCost: 1.20 / MILLION,  // $1.20 per million tokens
    outputTokenCost: 1.20 / MILLION
  },

  // Other models (using standard pricing tiers)
  'google/gemma-2-27b-it': {
    inputTokenCost: 0.50 / MILLION,  // $0.50 per million tokens
    outputTokenCost: 0.50 / MILLION
  },
  'google/gemma-2-9b-it': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens
    outputTokenCost: 0.20 / MILLION
  },
  'microsoft/WizardLM-2-8x22B': {
    inputTokenCost: 1.20 / MILLION,  // $1.20 per million tokens
    outputTokenCost: 1.20 / MILLION
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
      return target[prop as unknown as string];
    }
  });
};

export const TOGETHER_COSTS = createModelCostsWithNormalized(BASE_COSTS); 