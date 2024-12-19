export interface ModelCosts {
  inputTokenCost: number;
  outputTokenCost: number;
}

export interface ProviderModelCosts {
  [modelName: string]: ModelCosts;
} 