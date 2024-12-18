# AWS Bedrock Provider Integration

## Overview
The AWS Bedrock provider enables seamless access to AWS's AI models through an OpenAI-compatible interface. This means you can use your existing OpenAI SDK code and simply point it to our gateway - no code changes required!

## Configuration

### Headers
```bash
x-aws-access-key-id: your_access_key
x-aws-secret-access-key: your_secret_key
x-aws-region: us-east-1  # Optional
x-provider: bedrock
```

## IAM Setup

1. Create a new IAM Policy:
```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": [
                "bedrock:InvokeModel",
                "bedrock:InvokeModelWithResponseStream"
            ],
            "Resource": "*"
        }
    ]
}
```

2. Create a new IAM Role:
   - Go to IAM Console
   - Click "Roles" → "Create role"
   - Select "AWS service" and "Bedrock"
   - Attach the policy created above
   - Name the role (e.g., "bedrock-gateway-role")

3. Create an IAM User:
   - Go to IAM Console
   - Click "Users" → "Add user"
   - Enable programmatic access
   - Attach the policy created above
   - Save the access key and secret key

## Model Activation

Before using any model, you need to enable it in your AWS Console:

1. Go to AWS Bedrock Console
2. Navigate to "Model access"
3. Click "Manage model access"
4. Select the models you want to use
5. Click "Request model access"

## Examples

### Using OpenAI SDK
```typescript
import OpenAI from 'openai';

const openai = new OpenAI({
  baseURL: 'http://localhost:3000/v1',
  defaultHeaders: {
    'x-provider': 'bedrock',
    'x-aws-access-key-id': 'YOUR_ACCESS_KEY',
    'x-aws-secret-access-key': 'YOUR_SECRET_KEY',
    "x-aws-region": process.env.AWS_REGION,
  }
});

// Use exactly like OpenAI!
const response = await openai.chat.completions.create({
  model: 'anthropic.claude-3-sonnet-20240229-v1:0',
  messages: [{ role: 'user', content: 'Hello!' }]
});
```

### Using Curl
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: bedrock" \
  -H "x-aws-access-key-id: YOUR_ACCESS_KEY" \
  -H "x-aws-secret-access-key: YOUR_SECRET_KEY" \
  -d '{
    "model": "anthropic.claude-3-sonnet-20240229-v1:0",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

## Supported Models

### AI21 Labs Models
| Model Name | Model ID | Regions | Capabilities | Streaming |
|------------|----------|---------|--------------|-----------|
| Jamba 1.5 Large | ai21.jamba-1-5-large-v1:0 | us-east-1 | Text, Chat | Yes |
| Jamba 1.5 Mini | ai21.jamba-1-5-mini-v1:0 | us-east-1 | Text, Chat | Yes |
| Jamba-Instruct | ai21.jamba-instruct-v1:0 | us-east-1 | Text, Chat | Yes |

### Amazon Titan Models
| Model Name | Model ID | Regions | Capabilities | Streaming |
|------------|----------|---------|--------------|-----------|
| Titan Text G1 - Express | amazon.titan-text-express-v1 | Multiple | Text, Chat | Yes |
| Titan Text G1 - Lite | amazon.titan-text-lite-v1 | Multiple | Text | Yes |
| Titan Text G1 - Premier | amazon.titan-text-premier-v1:0 | us-east-1 | Text | Yes |
| Titan Text Large | amazon.titan-tg1-large | us-east-1, us-west-2 | Text | Yes |

### Anthropic Models
| Model Name | Model ID | Regions | Capabilities | Streaming |
|------------|----------|---------|--------------|-----------|
| Claude 2.1 | anthropic.claude-v2:1 | Multiple | Text, Chat | Yes |
| Claude 2 | anthropic.claude-v2 | Multiple | Text, Chat | Yes |
| Claude 3.5 Haiku | anthropic.claude-3-5-haiku-20241022-v1:0 | Multiple | Text, Chat | Yes |

### Cohere Models
| Model Name | Model ID | Regions | Capabilities | Streaming |
|------------|----------|---------|--------------|-----------|
| Command Light | cohere.command-light-text-v14 | us-east-1, us-west-2 | Text | Yes |
| Command R+ | cohere.command-r-plus-v1:0 | us-east-1, us-west-2 | Text, Chat | Yes |
| Command R | cohere.command-r-v1:0 | us-east-1, us-west-2 | Text, Chat | Yes |
| Command | cohere.command-text-v14 | us-east-1, us-west-2 | Text | Yes |

### Meta Models
| Model Name | Model ID | Regions | Capabilities | Streaming |
|------------|----------|---------|--------------|-----------|
| Llama 3 8B Instruct | meta.llama3-8b-instruct-v1:0 | Multiple | Text, Chat | Yes |
| Llama 3 70B Instruct | meta.llama3-70b-instruct-v1:0 | Multiple | Text, Chat | Yes |
| Llama 3.1 8B Instruct | meta.llama3-1-8b-instruct-v1:0 | Multiple | Text, Chat | Yes |
| Llama 3.2 1B Instruct | meta.llama3-2-1b-instruct-v1:0 | Multiple | Text, Chat | Yes |

### Mistral AI Models
| Model Name | Model ID | Regions | Capabilities | Streaming |
|------------|----------|---------|--------------|-----------|
| Mistral 7B Instruct | mistral.mistral-7b-instruct-v0:2 | Multiple | Text | Yes |
| Mixtral 8x7B Instruct | mistral.mixtral-8x7b-instruct-v0:1 | Multiple | Text | Yes |

> Note: All models need to be enabled in your AWS Bedrock Console before use. See the [Model Activation](#model-activation) section for setup instructions.

## Error Handling

| Error Code | Description | Solution |
|------------|-------------|----------|
| 401 | Invalid AWS credentials | Check AWS access key and secret |
| 403 | Insufficient permissions | Verify IAM permissions |
| 404 | Model not found | Enable model in AWS Console |
| 429 | Rate limit exceeded | Check AWS quotas |

## Best Practices

1. **Security**
   - Use IAM roles with minimum required permissions
   - Rotate access keys regularly
   - Never commit credentials to code

2. **Performance**
   - Choose the closest AWS region
   - Implement proper error handling and retries
   - Monitor usage and costs

3. **Model Selection**
   - Enable models before use
   - Verify model availability in your region
   - Consider model-specific pricing

## Additional Resources

- [AWS Bedrock Documentation](https://docs.aws.amazon.com/bedrock)
- [IAM Best Practices](https://docs.aws.amazon.com/IAM/latest/UserGuide/best-practices.html)
- [Bedrock Pricing](https://aws.amazon.com/bedrock/pricing/) 