import { ProviderModelCosts } from './types';
import { GROQ_COSTS } from './groq';
import { OPENAI_COSTS } from './openai';
import { ANTHROPIC_COSTS } from './anthropic';

export const MODEL_COSTS: Record<string, ProviderModelCosts> = {
  groq: GROQ_COSTS,
  openai: OPENAI_COSTS,
  anthropic: ANTHROPIC_COSTS
}; 