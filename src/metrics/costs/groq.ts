import { ProviderModelCosts } from './types';

// Helper function to normalize model names
const normalizeModelName = (model: string): string => {
  return model.toLowerCase().replace(/[-_]/g, '');
};

// Create a map with both original and normalized model names
const createModelCostsWithNormalized = (costs: ProviderModelCosts): ProviderModelCosts => {
  const normalizedCosts: ProviderModelCosts = {};
  Object.entries(costs).forEach(([model, cost]) => {
    normalizedCosts[model] = cost;
    normalizedCosts[normalizeModelName(model)] = cost;
    // Add version without context window
    normalizedCosts[model.replace(/-\d+k$/, '')] = cost;
  });
  return normalizedCosts;
};

// Convert price from dollars per million tokens to dollars per token
const MILLION = 1_000_000;

const BASE_COSTS: ProviderModelCosts = {
  'llama-3.2-1b-preview-8k': {
    inputTokenCost: 0.04 / MILLION,  // $0.04 per million tokens = $0.00000004 per token
    outputTokenCost: 0.04 / MILLION  // $0.04 per million tokens = $0.00000004 per token
  },
  'llama-3.2-3b-preview-8k': {
    inputTokenCost: 0.06 / MILLION,  // $0.06 per million tokens = $0.00000006 per token
    outputTokenCost: 0.06 / MILLION  // $0.06 per million tokens = $0.00000006 per token
  },
  'llama-3.3-70b-versatile-128k': {
    inputTokenCost: 0.59 / MILLION,  // $0.59 per million tokens = $0.00000059 per token
    outputTokenCost: 0.79 / MILLION  // $0.79 per million tokens = $0.00000079 per token
  },
  'llama-3.1-8b-instant-128k': {
    inputTokenCost: 0.05 / MILLION,  // $0.05 per million tokens = $0.00000005 per token
    outputTokenCost: 0.08 / MILLION  // $0.08 per million tokens = $0.00000008 per token
  },
  'llama-3.1-8b-instant': {  // Added without context window
    inputTokenCost: 0.05 / MILLION,  // $0.05 per million tokens = $0.00000005 per token
    outputTokenCost: 0.08 / MILLION  // $0.08 per million tokens = $0.00000008 per token
  },
  'llama-3-70b-8k': {
    inputTokenCost: 0.59 / MILLION,  // $0.59 per million tokens = $0.00000059 per token
    outputTokenCost: 0.79 / MILLION  // $0.79 per million tokens = $0.00000079 per token
  },
  'llama-3-8b-8k': {
    inputTokenCost: 0.05 / MILLION,  // $0.05 per million tokens = $0.00000005 per token
    outputTokenCost: 0.08 / MILLION  // $0.08 per million tokens = $0.00000008 per token
  },
  'mixtral-8x7b-instruct-32k': {
    inputTokenCost: 0.24 / MILLION,  // $0.24 per million tokens = $0.00000024 per token
    outputTokenCost: 0.24 / MILLION  // $0.24 per million tokens = $0.00000024 per token
  },
  'gemma-7b-8k-instruct': {
    inputTokenCost: 0.07 / MILLION,  // $0.07 per million tokens = $0.00000007 per token
    outputTokenCost: 0.07 / MILLION  // $0.07 per million tokens = $0.00000007 per token
  },
  'gemma-2-9b-8k': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens = $0.00000020 per token
    outputTokenCost: 0.20 / MILLION  // $0.20 per million tokens = $0.00000020 per token
  },
  'llama-3-groq-70b-tool-use-preview-8k': {
    inputTokenCost: 0.89 / MILLION,  // $0.89 per million tokens = $0.00000089 per token
    outputTokenCost: 0.89 / MILLION  // $0.89 per million tokens = $0.00000089 per token
  },
  'llama-3-groq-8b-tool-use-preview-8k': {
    inputTokenCost: 0.19 / MILLION,  // $0.19 per million tokens = $0.00000019 per token
    outputTokenCost: 0.19 / MILLION  // $0.19 per million tokens = $0.00000019 per token
  },
  'llama-guard-3-8b-8k': {
    inputTokenCost: 0.20 / MILLION,  // $0.20 per million tokens = $0.00000020 per token
    outputTokenCost: 0.20 / MILLION  // $0.20 per million tokens = $0.00000020 per token
  },
  'llama-3.3-70b-specdec-8k': {
    inputTokenCost: 0.59 / MILLION,  // $0.59 per million tokens = $0.00000059 per token
    outputTokenCost: 0.99 / MILLION  // $0.99 per million tokens = $0.00000099 per token
  }
};

export const GROQ_COSTS = createModelCostsWithNormalized(BASE_COSTS); 