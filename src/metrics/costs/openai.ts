import { ProviderModelCosts } from './types';

// Convert price from dollars per million tokens to dollars per token
const MILLION = 1_000_000;

export const OPENAI_COSTS: ProviderModelCosts = {
  'gpt-4o': {
    inputTokenCost: 2.50 / MILLION,  // $2.50 per million tokens = $0.00000250 per token
    outputTokenCost: 10.00 / MILLION  // $10.00 per million tokens = $0.00001000 per token
  },
  'gpt-4o-mini': {
    inputTokenCost: 0.15 / MILLION,  // $0.15 per million tokens = $0.00000015 per token
    outputTokenCost: 0.60 / MILLION  // $0.60 per million tokens = $0.00000060 per token
  },
  'gpt-4o-mini-audio-preview': {
    inputTokenCost: 0.15 / MILLION,  // $0.15 per million tokens = $0.00000015 per token
    outputTokenCost: 0.60 / MILLION  // $0.60 per million tokens = $0.00000060 per token
  },
  'gpt-4o-mini-realtime-preview': {
    inputTokenCost: 0.60 / MILLION,  // $0.60 per million tokens = $0.00000060 per token
    outputTokenCost: 2.40 / MILLION  // $2.40 per million tokens = $0.00000240 per token
  },
  'gpt-4o-audio-preview': {
    inputTokenCost: 2.50 / MILLION,  // $2.50 per million tokens = $0.00000250 per token
    outputTokenCost: 10.00 / MILLION  // $10.00 per million tokens = $0.00001000 per token
  },
  'gpt-4o-realtime-preview': {
    inputTokenCost: 5.00 / MILLION,  // $5.00 per million tokens = $0.00000500 per token
    outputTokenCost: 20.00 / MILLION  // $20.00 per million tokens = $0.00002000 per token
  },
  'o1': {
    inputTokenCost: 15.00 / MILLION,  // $15.00 per million tokens = $0.00001500 per token
    outputTokenCost: 60.00 / MILLION  // $60.00 per million tokens = $0.00006000 per token
  },
  'o1-preview': {
    inputTokenCost: 15.00 / MILLION,  // $15.00 per million tokens = $0.00001500 per token
    outputTokenCost: 60.00 / MILLION  // $60.00 per million tokens = $0.00006000 per token
  },
  'o1-mini': {
    inputTokenCost: 3.00 / MILLION,  // $3.00 per million tokens = $0.00000300 per token
    outputTokenCost: 12.00 / MILLION  // $12.00 per million tokens = $0.00001200 per token
  },
  'gpt-4-turbo': {
    inputTokenCost: 10.00 / MILLION,  // $10.00 per million tokens = $0.00001000 per token
    outputTokenCost: 30.00 / MILLION  // $30.00 per million tokens = $0.00003000 per token
  },
  'gpt-4': {
    inputTokenCost: 30.00 / MILLION,  // $30.00 per million tokens = $0.00003000 per token
    outputTokenCost: 60.00 / MILLION  // $60.00 per million tokens = $0.00006000 per token
  },
  'gpt-3.5-turbo': {
    inputTokenCost: 0.50 / MILLION,  // $0.50 per million tokens = $0.00000050 per token
    outputTokenCost: 1.50 / MILLION  // $1.50 per million tokens = $0.00000150 per token
  }
}; 