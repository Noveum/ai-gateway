import { ProviderModelCosts } from './types';
import { GROQ_COSTS } from './groq';
import { OPENAI_COSTS } from './openai';
import { ANTHROPIC_COSTS } from './anthropic';
import { FIREWORKS_COSTS } from './fireworks';
import { TOGETHER_COSTS } from './together';

export const MODEL_COSTS: Record<string, ProviderModelCosts> = {
  groq: GROQ_COSTS,
  openai: OPENAI_COSTS,
  anthropic: ANTHROPIC_COSTS,
  fireworks: FIREWORKS_COSTS,
  together: TOGETHER_COSTS
}; 