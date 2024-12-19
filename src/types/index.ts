export type Message = {
  role: 'user' | 'assistant' | 'system';
  content: string;
  name?: string;
  function_call?: {
    name: string;
    arguments: string;
  };
};

export type Tool = {
  type: 'function';
  function: {
    name: string;
    description?: string;
    parameters: Record<string, any>;
  };
};

export type ToolChoice = 
  | 'none' 
  | 'auto' 
  | { type: 'function'; function: { name: string } };

export type ResponseFormat = {
  type: 'text' | 'json_object' | 'json_schema';
  json_schema?: Record<string, any>;
};

export type StreamOptions = {
  // Add stream options when OpenAI documents them
};

export type ChatCompletionRequest = {
  messages: Message[];
  model: string;
  store?: boolean;
  reasoning_effort?: 'low' | 'medium' | 'high';
  metadata?: Record<string, any>;
  frequency_penalty?: number;
  logit_bias?: Record<string, number>;
  logprobs?: boolean;
  top_logprobs?: number;
  max_tokens?: number;
  max_completion_tokens?: number;
  n?: number;
  modalities?: ('text' | 'audio')[];
  prediction?: Record<string, any>;
  audio?: {
    // Add audio options when OpenAI documents them
  };
  presence_penalty?: number;
  response_format?: ResponseFormat;
  seed?: number;
  service_tier?: 'auto' | 'default';
  stop?: string | string[];
  stream?: boolean;
  stream_options?: StreamOptions;
  temperature?: number;
  top_p?: number;
  top_k?: number;
  repetition_penalty?: number;
  tools?: Tool[];
  tool_choice?: ToolChoice;
  parallel_tool_calls?: boolean;
  user?: string;
  // Deprecated fields
  function_call?: 'none' | 'auto' | { name: string };
  functions?: Array<{
    name: string;
    description?: string;
    parameters: Record<string, any>;
  }>;
};

export type Provider = 'openai' | 'anthropic' | 'bedrock' | 'groq' | 'fireworks' | 'together';

export type ProviderConfig = {
  apiKey?: string;
  awsAccessKeyId?: string;
  awsSecretAccessKey?: string;
  awsRegion?: string;
  organization?: string;
  inputTokenCost?: number;
  outputTokenCost?: number;
};

export interface Env {
  // Elasticsearch Configuration
  ELASTICSEARCH_HOST: string;
  ELASTICSEARCH_PORT: string;
  ELASTICSEARCH_PASSWORD: string;
  ELASTICSEARCH_INDEX: string;
  ELASTICSEARCH_USERNAME?: string;
  ENVIRONMENT?: string;
}

export interface Bindings extends Env {
  DEFAULT_PROVIDER?: string;
  OPENAI_API_KEY?: string;
  ANTHROPIC_API_KEY?: string;
  AWS_ACCESS_KEY_ID?: string;
  AWS_SECRET_ACCESS_KEY?: string;
  AWS_REGION?: string;
  GROQ_API_KEY?: string;
  FIREWORKS_API_KEY?: string;
  TOGETHER_API_KEY?: string;
}


export type Variables = {
  provider: Provider;
  config: ProviderConfig;
  requestId: string;
};

export type ErrorResponse = {
  error: {
    message: string;
    type: string;
    code: number;
  };
};

// Metrics types
export type TokenUsage = {
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
};

export type ProviderMetricsConfig = {
  inputTokenCost?: number;
  outputTokenCost?: number;
};

export type RequestMetrics = {
  requestId: string;
  timestamp: string;
  method: string;
  path: string;
  provider: string;
  model?: string;
  status?: number;
  success: boolean;
  cached: boolean;
  performance: {
    startTime: number;
    endTime?: number;
    ttfb?: number;
    totalLatency?: number;
  };
  tokens?: {
    input: number;
    output: number;
    total: number;
    details?: Record<string, any>;
  };
  cost?: {
    inputCost?: number;
    outputCost?: number;
    totalCost?: number;
  };
  metadata?: {
    estimated: boolean;
    totalChunks: number;
    streamComplete?: boolean;
    [key: string]: any;
  };
}; 