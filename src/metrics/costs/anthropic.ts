import { ProviderModelCosts } from './types';

// Helper function to normalize model names by removing version numbers and standardizing format
const normalizeModelName = (model: string): string[] => {
  // Remove version numbers and dates (e.g., -20241022)
  const baseModel = model.toLowerCase().replace(/-\d{8}$/, '');
  
  // Generate variations of the model name
  const variations = [
    baseModel,
    baseModel.replace(/[-_]/g, ''),  // Remove all hyphens and underscores
    baseModel.replace('claude-3-', 'claude-3-5-'),  // Handle 3 vs 3.5 variations
    baseModel.replace('claude-3-5-', 'claude-3-')   // Handle 3.5 vs 3 variations
  ];
  
  return [...new Set(variations)];  // Remove duplicates
};

// Create a map with both original and normalized model names
const createModelCostsWithNormalized = (costs: ProviderModelCosts): ProviderModelCosts => {
  const normalizedCosts: ProviderModelCosts = {};
  Object.entries(costs).forEach(([model, cost]) => {
    // Add the original model name
    normalizedCosts[model] = cost;
    
    // Add all normalized variations
    normalizeModelName(model).forEach(variation => {
      normalizedCosts[variation] = cost;
    });
  });
  return normalizedCosts;
};

// Convert price from dollars per million tokens to dollars per token
const MILLION = 1_000_000;

const BASE_COSTS: ProviderModelCosts = {
  'claude-3-opus': {
    inputTokenCost: 15 / MILLION,    // $15 per million tokens = $0.000015 per token
    outputTokenCost: 75 / MILLION    // $75 per million tokens = $0.000075 per token
  },
  'claude-3-sonnet': {
    inputTokenCost: 3 / MILLION,     // $3 per million tokens = $0.000003 per token
    outputTokenCost: 15 / MILLION    // $15 per million tokens = $0.000015 per token
  },
  'claude-3-haiku': {
    inputTokenCost: 0.80 / MILLION,  // $0.80 per million tokens = $0.0000008 per token
    outputTokenCost: 4 / MILLION     // $4 per million tokens = $0.000004 per token
  }
};

export const ANTHROPIC_COSTS = createModelCostsWithNormalized(BASE_COSTS); 