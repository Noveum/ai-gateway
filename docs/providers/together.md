# Together AI Provider Integration

## Overview
The Together AI provider enables access to multiple open-source models through Together's inference infrastructure.

## Base Configuration
Refer to the provider implementation in `src/providers/together.rs`.

## Configuration

### Request Headers
```bash
Authorization: Bearer tok_...
x-provider: together
Content-Type: application/json
```

## Supported Models

### Open Source Models
- Llama 2 (7B, 13B, 70B)
- Mixtral-8x7B
- CodeLlama
- Stable LM
- Yi Series
- Qwen Series

For a complete list of models, refer to the [Together AI Dedicated Models](https://docs.together.ai/docs/dedicated-models).

## API Endpoints

### Chat Completions
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: together" \
  -H "Authorization: Bearer $TOGETHER_API_KEY" \
  -d '{
    "model": "togethercomputer/llama-2-70b-chat",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### Streaming
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: together" \
  -H "Authorization: Bearer $TOGETHER_API_KEY" \
  -d '{
    "model": "togethercomputer/llama-2-70b-chat",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": true
  }'
```

## SDK Integration

### Node.js (OpenAI SDK Compatible)
```typescript
import OpenAI from 'openai';

const together = new OpenAI({
  apiKey: process.env.TOGETHER_API_KEY,
  baseURL: "http://localhost:3000/v1/",
  defaultHeaders: { "x-provider": "together" }
});

const response = await together.chat.completions.create({
  model: "meta-llama/Meta-Llama-3-8B-Instruct-Turbo",
  messages: [{ role: "user", content: "What are some fun things to do in New York?" }]
});

console.log(response.choices[0].message.content);
```

## Error Handling

| Error Code | Description | Solution |
|------------|-------------|----------|
| 401 | Invalid API key | Check your Together API key |
| 429 | Rate limit exceeded | Implement backoff strategy |
| 500 | Server error | Check server logs |

## Best Practices

1. **Model Selection**
   - Choose appropriate model size
   - Consider inference speed requirements
   - Balance cost vs performance

2. **Rate Limiting**
   - Implement exponential backoff
   - Monitor usage quotas
   - Use streaming for long responses

3. **Error Handling**
   - Implement retry logic
   - Handle timeouts gracefully
   - Log errors appropriately

## Additional Resources

- [Together AI Documentation](https://docs.together.ai)
- [Available Models](https://docs.together.ai/reference/models)
- [Pricing Information](https://www.together.ai/pricing) 

### Serverless Models

#### Meta
- **Llama 3.1 8B Instruct Turbo**  
  API Model: `meta-llama/Meta-Llama-3.1-8B-Instruct-Turbo`  
  Context Length: 131072  
  Quantization: FP8  

- **Llama 3.1 70B Instruct Turbo**  
  API Model: `meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo`  
  Context Length: 131072  
  Quantization: FP8  

- **Llama 3.1 405B Instruct Turbo**  
  API Model: `meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo`  
  Context Length: 130815  
  Quantization: FP8  

- **Llama 3 8B Instruct Turbo**  
  API Model: `meta-llama/Meta-Llama-3-8B-Instruct-Turbo`  
  Context Length: 8192  
  Quantization: FP8  

- **Llama 3 70B Instruct Turbo**  
  API Model: `meta-llama/Meta-Llama-3-70B-Instruct-Turbo`  
  Context Length: 8192  
  Quantization: FP8  

- **Llama 3.2 3B Instruct Turbo**  
  API Model: `meta-llama/Llama-3.2-3B-Instruct-Turbo`  
  Context Length: 131072  
  Quantization: FP16  

- **Llama 3 8B Instruct Lite**  
  API Model: `meta-llama/Meta-Llama-3-8B-Instruct-Lite`  
  Context Length: 8192  
  Quantization: INT4  

- **Llama 3 70B Instruct Lite**  
  API Model: `meta-llama/Meta-Llama-3-70B-Instruct-Lite`  
  Context Length: 8192  
  Quantization: INT4  

- **Llama 3 8B Instruct Reference**  
  API Model: `meta-llama/Llama-3-8b-chat-hf`  
  Context Length: 8192  
  Quantization: FP16  

- **Llama 3 70B Instruct Reference**  
  API Model: `meta-llama/Llama-3-70b-chat-hf`  
  Context Length: 8192  
  Quantization: FP16  

#### Nvidia
- **Llama 3.1 Nemotron 70B**  
  API Model: `nvidia/Llama-3.1-Nemotron-70B-Instruct-HF`  
  Context Length: 32768  
  Quantization: FP16  

#### Qwen
- **Qwen 2.5 Coder 32B Instruct**  
  API Model: `Qwen/Qwen2.5-Coder-32B-Instruct`  
  Context Length: 32769  
  Quantization: FP16  

- **Qwen 2.5 7B Instruct Turbo**  
  API Model: `Qwen/Qwen2.5-7B-Instruct-Turbo`  
  Context Length: 32768  
  Quantization: FP8  

- **Qwen 2.5 72B Instruct Turbo**  
  API Model: `Qwen/Qwen2.5-72B-Instruct-Turbo`  
  Context Length: 32768  
  Quantization: FP8  

- **Qwen 2 Instruct (72B)**  
  API Model: `Qwen/Qwen2-72B-Instruct`  
  Context Length: 32768  
  Quantization: FP16  

#### Microsoft
- **WizardLM-2 8x22B**  
  API Model: `microsoft/WizardLM-2-8x22B`  
  Context Length: 65536  
  Quantization: FP16  

#### Google
- **Gemma 2 27B**  
  API Model: `google/gemma-2-27b-it`  
  Context Length: 8192  
  Quantization: FP16  

- **Gemma 2 9B**  
  API Model: `google/gemma-2-9b-it`  
  Context Length: 8192  
  Quantization: FP16  

- **Gemma Instruct (2B)**  
  API Model: `google/gemma-2b-it`  
  Context Length: 8192  
  Quantization: FP16  

#### Databricks
- **DBRX Instruct**  
  API Model: `databricks/dbrx-instruct`  
  Context Length: 32768  
  Quantization: FP16  

#### DeepSeek
- **DeepSeek LLM Chat (67B)**  
  API Model: `deepseek-ai/deepseek-llm-67b-chat`  
  Context Length: 4096  
  Quantization: FP16  

#### Gryphe
- **MythoMax-L2 (13B)**  
  API Model: `Gryphe/MythoMax-L2-13b`  
  Context Length: 4096  
  Quantization: FP16  

#### Mistralai
- **Mistral (7B) Instruct**  
  API Model: `mistralai/Mistral-7B-Instruct-v0.1`  
  Context Length: 8192  
  Quantization: FP16  

- **Mistral (7B) Instruct v0.2**  
  API Model: `mistralai/Mistral-7B-Instruct-v0.2`  
  Context Length: 32768  
  Quantization: FP16  

- **Mistral (7B) Instruct v0.3**  
  API Model: `mistralai/Mistral-7B-Instruct-v0.3`  
  Context Length: 32768  
  Quantization: FP16  

- **Mixtral-8x7B Instruct (46.7B)**  
  API Model: `mistralai/Mixtral-8x7B-Instruct-v0.1`  
  Context Length: 32768  
  Quantization: FP16  

- **Mixtral-8x22B Instruct (141B)**  
  API Model: `mistralai/Mixtral-8x22B-Instruct-v0.1`  
  Context Length: 65536  
  Quantization: FP16  

#### NousResearch
- **Nous Hermes 2 - Mixtral 8x7B-DPO (46.7B)**  
  API Model: `NousResearch/Nous-Hermes-2-Mixtral-8x7B-DPO`  
  Context Length: 32768  
  Quantization: FP16  

#### Together
- **StripedHyena Nous (7B)**  
  API Model: `togethercomputer/StripedHyena-Nous-7B`  
  Context Length: 32768  
  Quantization: FP16  

#### Upstage
- **Upstage SOLAR Instruct v1 (11B)**  
  API Model: `upstage/SOLAR-10.7B-Instruct-v1.0`  
  Context Length: 4096  
  Quantization: FP16  

### Dedicated Models

#### 01.AI
- **01-ai Yi Chat (34B)**  
  API Model: `zero-one-ai/Yi-34B-Chat`  
  Context Length: 4096  
  Quantization: FP16  

#### AllenAI
- **OLMo Instruct (7B)**  
  API Model: `allenai/OLMo-7B-Instruct`  
  Context Length: 2048  
  Quantization: FP16  

#### Austism
- **Chronos Hermes (13B)**  
  API Model: `Austism/chronos-hermes-13b`  
  Context Length: 2048  
  Quantization: FP16  

#### Carson
- **carson ml318br**  
  API Model: `carson/ml318br`  
  Context Length: 8192  
  Quantization: FP16  

#### Cognitive Computations
- **Dolphin 2.5 Mixtral 8x7B**  
  API Model: `cognitivecomputations/dolphin-2.5-mixtral-8x7b`  
  Context Length: 32768  
  Quantization: FP16  

#### Databricks
- **DBRX Instruct**  
  API Model: `databricks/dbrx-instruct`  
  Context Length: 32768  
  Quantization: FP16  

#### DeepSeek
- **DeepSeek LLM Chat (67B)**  
  API Model: `deepseek-ai/deepseek-llm-67b-chat`  
  Context Length: 4096  
  Quantization: FP16  
- **Deepseek Coder Instruct (33B)**  
  API Model: `deepseek-ai/deepseek-coder-33b-instruct`  
  Context Length: 16384  
  Quantization: FP16  

#### Garage-bAInd
- **Platypus2 Instruct (70B)**  
  API Model: `garage-bAInd/Platypus2-70B-instruct`  
  Context Length: 4096  
  Quantization: FP16  

#### Google
- **Gemma-2 Instruct (9B)**  
  API Model: `google/gemma-2-9b-it`  
  Context Length: 8192  
  Quantization: FP16  
- **Gemma-2 Instruct (27B)**  
  API Model: `google/gemma-2-27b-it`  
  Context Length: 8192  
  Quantization: FP16  
- **Gemma Instruct (2B)**  
  API Model: `google/gemma-2b-it`  
  Context Length: 8192  
  Quantization: FP16  
- **Gemma Instruct (7B)**  
  API Model: `google/gemma-7b-it`  
  Context Length: 8192  
  Quantization: FP16  

#### GradientAI
- **Llama-3 70B Instruct Gradient 1048K**  
  API Model: `gradientai/Llama-3-70B-Instruct-Gradient-1048k`  
  Context Length: 1048576  
  Quantization: FP16  

#### Gryphe
- **MythoMax-L2 (13B)**  
  API Model: `Gryphe/MythoMax-L2-13b`  
  Context Length: 4096  
  Quantization: FP16  
- **Gryphe MythoMax L2 Lite (13B)**  
  API Model: `Gryphe/MythoMax-L2-13b-Lite`  
  Context Length: 4096  
  Quantization: FP16  

#### Haotian Liu
- **LLaVa-Next (Mistral-7B)**  
  API Model: `llava-hf/llava-v1.6-mistral-7b-hf`  
  Context Length: 4096  
  Quantization: FP16  

#### Hugging Face
- **Zephyr-7B-ß**  
  API Model: `HuggingFaceH4/zephyr-7b-beta`  
  Context Length: 32768  
  Quantization: FP16  

#### LM Sys
- **Koala (7B)**  
  API Model: `togethercomputer/Koala-7B`  
  Context Length: 2048  
  Quantization: FP16  
- **Vicuna v1.3 (7B)**  
  API Model: `lmsys/vicuna-7b-v1.3`  
  Context Length: 2048  
  Quantization: FP16  
- **Vicuna v1.5 16K (13B)**  
  API Model: `lmsys/vicuna-13b-v1.5-16k`  
  Context Length: 16384  
  Quantization: FP16  
- **Vicuna v1.5 (13B)**  
  API Model: `lmsys/vicuna-13b-v1.5`  
  Context Length: 4096  
  Quantization: FP16  

#### Meta
- **Code Llama Instruct (34B)**  
  API Model: `codellama/CodeLlama-34b-Instruct-hf`  
  Context Length: 16384  
  Quantization: FP16  
- **Meta Llama 3.2 90B Vision Instruct Turbo**  
  API Model: `meta-llama/Llama-3.2-90B-Vision-Instruct-Turbo`  
  Context Length: 131072  
  Quantization: FP16  
- **Meta Llama 3.2 11B Vision Instruct Turbo**  
  API Model: `meta-llama/Llama-3.2-11B-Vision-Instruct-Turbo`  
  Context Length: 131072  
  Quantization: FP16  

#### Microsoft
- **WizardLM-2 (8x22B)**  
  API Model: `microsoft/WizardLM-2-8x22B`  
  Context Length: 65536  
  Quantization: FP16  

#### Mistralai
- **Mistral (7B) Instruct v0.3**  
  API Model: `mistralai/Mistral-7B-Instruct-v0.3`  
  Context Length: 32768  
  Quantization: FP16  

#### NousResearch
- **Nous Hermes 2 - Mixtral 8x7B-DPO**  
  API Model: `NousResearch/Nous-Hermes-2-Mixtral-8x7B-DPO`  
  Context Length: 32768  
  Quantization: FP16  

#### Qwen
- **Qwen 2 Instruct (72B)**  
  API Model: `Qwen/Qwen2-72B-Instruct`  
  Context Length: 32768  
  Quantization: FP16  

#### Upstage
- **Upstage SOLAR Instruct v1 (11B)**  
  API Model: `upstage/SOLAR-10.7B-Instruct-v1.0`  
  Context Length: 4096  
  Quantization: FP16  

#### WizardLM
- **WizardLM v1.2 (13B)**  
  API Model: `WizardLM/WizardLM-13B-V1.2`  
  Context Length: 4096  
  Quantization: FP16  

