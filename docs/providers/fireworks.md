# Fireworks AI Provider

## Configuration

### Request Headers
```bash
Authorization: Bearer $FIREWORKS_API_KEY
x-provider: fireworks
```

## Popular Models

- **Llama 3.1 70B Instruct** - High performance general purpose ($0.90/M tokens)
- **Mixtral MoE 8x7B Instruct** - Efficient multi-expert model ($0.50/M tokens)
- **Llama 3.1 8B Instruct** - Fast, cost-effective option ($0.20/M tokens)

See [Fireworks Model Library](https://fireworks.ai/models) for the complete list.

> **Note**: Vision models are currently not supported.

## Usage Examples

### Chat Completion
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $FIREWORKS_API_KEY" \
  -H "x-provider: fireworks" \
  -d '{
    "model": "accounts/fireworks/models/llama-v3p1-70b-instruct",
    "messages": [
      {"role": "user", "content": "What is the capital of France?"}
    ],
    "temperature": 0.7
  }'
```

### Streaming Response
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $FIREWORKS_API_KEY" \
  -H "x-provider: fireworks" \
  -d '{
    "model": "accounts/fireworks/models/llama-v3p1-70b-instruct",
    "messages": [
      {"role": "user", "content": "Write a short poem about coding"}
    ],
    "stream": true
  }'
```

### Multimodal Vision Model
```bash
curl --location 'localhost:3000/v1/chat/completions' \
--header 'Authorization: Bearer $FIREWORKS_API_KEY' \
--header 'Content-Type: application/json' \
--header 'x-provider: fireworks' \
--data '{
    "model": "accounts/fireworks/models/llama-v3p2-11b-vision-instruct",
    "messages": [
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "What'\''s in this image?"
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "https://upload.wikimedia.org/wikipedia/commons/f/f2/LPU-v1-die.jpg"
                    }
                }
            ]
        }
    ],
    "stream": false,
    "max_tokens": 300
}'
```

### TypeScript/JavaScript SDK
```typescript
import OpenAI from 'openai';

const client = new OpenAI({
  apiKey: process.env.FIREWORKS_API_KEY,
  baseURL: 'http://localhost:3000/v1/',
  defaultHeaders: { 'x-provider': 'fireworks' }
});

const response = await client.chat.completions.create({
  model: 'accounts/fireworks/models/llama-v3p1-70b-instruct',
  messages: [
    { role: 'user', content: 'Explain quantum computing in simple terms' }
  ],
  temperature: 0.7
});
```

## Best Practices

1. **Model Selection**
   - Use 70B models for complex tasks requiring deep reasoning
   - Use 8B models for simple tasks and cost optimization
   - Consider Mixtral models for specialized use cases

2. **Performance**
   - Enable streaming for real-time responses
   - Set appropriate `max_tokens` limits
   - Use proper error handling and retries

## Resources

- [Fireworks Documentation](https://docs.fireworks.ai)
- [Model Library](https://fireworks.ai/models)
- [Pricing Information](https://fireworks.ai/pricing)


# Top AI Models

1. **Llama 3.1 405B Instruct**  
   - 131,072 Context  
   - Serverless  

2. **Llama 3.1 70B Instruct**  
   - $0.90/M Tokens  
   - 131,072 Context  
   - Serverless  

3. **Llama 3.1 8B Instruct**  
   - $0.20/M Tokens  
   - 131,072 Context  
   - Serverless  

4. **Llama 3.2 3B Instruct**  
   - $0.20/M Tokens  
   - 131,072 Context  
   - Serverless  

5. **Mixtral MoE 8x22B Instruct**  
   - $0.90/M Tokens  
   - 65,536 Context  
   - Serverless  

6. **Qwen2.5-Coder-32B-Instruct**  
   - $0.90/M Tokens  
   - 32,768 Context  
   - Serverless  

7. **Llama Guard 7B**  
   - $0.20/M Tokens  
   - 4,096 Context  

8. **Zephyr 7B Beta**  
   - $0.20/M Tokens  
   - 4,096 Context  

9. **Qwen2 72B Instruct**  
    - $0.90/M Tokens  
    - 32,768 Context  

10. **Mixtral 8x22B Instruct v0.1**  
    - $0.90/M Tokens  
    - 65,536 Context  

11. **CodeGemma 7B**  
    - $0.20/M Tokens  
    - 8,192 Context  

12. **Mixtral MoE 8x7B Instruct**  
    - $0.50/M Tokens  
    - 32,768 Context  
    - Serverless  

13. **DeepSeek Coder 1.3B Base**  
    - $0.20/M Tokens  
    - 16,384 Context  

14. **Yi-Large**  
    - $3.00/M Tokens  
    - 32,768 Context  
    - Serverless  

15. **Hermes 2 Pro Mistral 7B**  
    - $0.20/M Tokens  

16. **Llama 3 70B Instruct**  
    - $0.90/M Tokens  
    - 8,192 Context  
    - Serverless  

17. **Toppy M 7B**  
    - $0.20/M Tokens  
    - 32,768 Context  

18. **Nous Hermes Llama2 7B**  
    - $0.20/M Tokens  
    - 4,096 Context  

19. **Deepseek Coder 33B Instruct**  
    - $0.90/M Tokens  
    - 16,384 Context  

20. **Chronos Hermes 13B v2**  
    - $0.20/M Tokens  
    - 4,096 Context  

21. **Phind CodeLlama 34B v2**  
    - $0.90/M Tokens  
    - 16,384 Context  

22. **Nous Hermes Llama2 70B**  
    - $0.90/M Tokens  
    - 4,096 Context  

23. **Nous Hermes 2 - Mixtral 8x7B - DPO**  
    - $0.50/M Tokens  
    - 32,768 Context  

24. **Qwen2.5 Math 72B Instruct**  
    - $0.90/M Tokens  
    - 4,096 Context  

25. **Llama 2 70B Chat**  
    - $0.90/M Tokens  
    - 4,096 Context  

26. **Mistral 7B Instruct v0.2**  
    - $0.20/M Tokens  
    - 32,768 Context  

27. **StarCoder 7B**  
    - $0.20/M Tokens  
    - 8,192 Context  
    - Serverless  

28. **Code Llama 7B Python**  
    - $0.20/M Tokens  
    - 32,768 Context  

29. **StarCoder2 15B**  
    - $0.20/M Tokens  
    - 16,384 Context  

30. **Qwen2.5 14B Instruct**  
    - $0.20/M Tokens  
    - 32,768 Context  

31. **Code Llama 34B Instruct**  
    - $0.90/M Tokens  
    - 32,768 Context  

32. **Mixtral Moe 8x22B**  
    - $0.90/M Tokens  
    - 65,536 Context  

33. **DeepSeek V2 Lite Chat**  
    - 163,840 Context  

34. **Phi 3.5 Vision Instruct**  
    - 32,064 Context  
    - Serverless  

35. **Yi 6B**  
    - $0.20/M Tokens  
    - 4,096 Context  

36. **MythoMax L2 13B**  
    - $0.20/M Tokens  
    - 4,096 Context  

37. **StarCoder2 7B**  
    - $0.20/M Tokens  
    - 16,384 Context  

38. **Code Llama 70B Instruct**  
    - $0.90/M Tokens  
    - 4,096 Context  

39. **FireLLaVA-13B**  
    - $0.20/M Tokens  
    - 4,096 Context  

40. **Stable Code 3B**  
    - $0.20/M Tokens  

41. **Deepseek Coder V2 Lite**  
    - 163,840 Context  

42. **Qwen2.5-Coder-7B-Instruct**  
    - $0.20/M Tokens  
    - 32,768 Context  

43. **Qwen2.5 7B**  
    - $0.20/M Tokens  
    - 131,072 Context  

44. **Llama 3.1 Nemotron 70B**  
    - $0.90/M Tokens  
    - 131,072 Context  

45. **Llama 3.2 3B**  
    - $0.20/M Tokens  
    - 131,072 Context  

46. **Capybara 34B**  
    - $0.90/M Tokens  
    - 200,000 Context  

47. **Phind CodeLlama 34B Python v1**  
    - $0.90/M Tokens  
    - 16,384 Context  

48. **Pythia 12B**  
    - $0.20/M Tokens  
    - 2,048 Context  

49. **FireFunction V1**  
    - 32,768 Context  
    - Serverless  
