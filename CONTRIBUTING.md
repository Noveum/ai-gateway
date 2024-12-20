# Contributing to Noveum AI Gateway

First off, thank you for considering contributing to Noveum AI Gateway! It's people like you that make Noveum AI Gateway such a great tool.

## Code of Conduct

This project and everyone participating in it is governed by our Code of Conduct. By participating, you are expected to uphold this code. Please report unacceptable behavior to [team@noveum.ai](mailto:team@noveum.ai).

## Project Architecture

### Project Structure
```
/src/
├── handlers
│   └── chat.ts
├── hooks
│   └── index.ts
├── index.ts
├── metrics
│   ├── collector.ts
│   └── costs
│       ├── anthropic.ts
│       ├── fireworks.ts
│       ├── groq.ts
│       ├── index.ts
│       ├── openai.ts
│       ├── together.ts
│       └── types.ts
├── middleware
│   ├── auth.ts
│   ├── logging.ts
│   ├── types.ts
│   └── validation.ts
├── providers
│   ├── anthropic.ts
│   ├── base.ts
│   ├── factory.ts
│   ├── fireworks.ts
│   ├── groq.ts
│   ├── openai.ts
│   └── together.ts
├── types
│   └── index.ts
└── utils
```

### Key Components

#### Middleware System
- **Authentication**: Provider-specific API key validation
- **Validation**: Request payload validation
- **Logging**: Comprehensive request/response logging
- **CORS**: Cross-origin resource sharing
- **Error Handling**: Standardized error responses

#### Provider Interface
Each provider implements the `AIProvider` interface:
```typescript
interface AIProvider {
  chat(request: ChatRequest): Promise<ChatResponse>;
  stream(request: ChatRequest): ReadableStream;
  validate(request: ChatRequest): void;
}
```

## Development Guide

### Adding a New Provider

1. Create a new provider class in `src/providers/`:
```typescript
import { AIProvider } from '../types';

export class NewProvider implements AIProvider {
  async chat(request: ChatRequest): Promise<ChatResponse> {
    // Implementation
  }

  stream(request: ChatRequest): ReadableStream {
    // Implementation
  }

  validate(request: ChatRequest): void {
    // Implementation
  }
}
```

2. Register the provider in `src/providers/index.ts`
3. Add provider-specific types in `src/types/`
4. Update the provider factory

### Adding Provider Metrics and Costs

1. Create a cost file in `src/metrics/costs/`:
```typescript
import { ProviderModelCosts } from './types';

// Convert price from dollars per million tokens to dollars per token
const MILLION = 1_000_000;

// Define pricing structure
const BASE_COSTS: ProviderModelCosts = {
  'model-name': {
    inputTokenCost: 0.50 / MILLION,  // $0.50 per million tokens
    outputTokenCost: 0.50 / MILLION
  }
};

// Add helper functions for model name normalization
const normalizeModelName = (model: string): string => {
  return model.toLowerCase()
    .replace(/[-_\s]/g, '')
    .replace(/\.(\d)/g, '$1')
    .replace(/v\d+(\.\d+)?/, '');
};
```

## How Can I Contribute?

### Reporting Bugs

This section guides you through submitting a bug report for Noveum AI Gateway. Following these guidelines helps maintainers and the community understand your report, reproduce the behavior, and find related reports.

**Before Submitting A Bug Report:**

* Check the documentation for a list of common questions and problems.
* Ensure the bug was not already reported by searching on GitHub under [Issues](https://github.com/Noveum/ai-gateway/issues).
* If you're unable to find an open issue addressing the problem, open a new one.

**How Do I Submit A (Good) Bug Report?**

Bugs are tracked as [GitHub issues](https://github.com/Noveum/ai-gateway/issues). Create an issue and provide the following information:

* Use a clear and descriptive title
* Describe the exact steps which reproduce the problem
* Provide specific examples to demonstrate the steps
* Describe the behavior you observed after following the steps
* Explain which behavior you expected to see instead and why
* Include details about your configuration and environment

### Suggesting Enhancements

This section guides you through submitting an enhancement suggestion for Noveum AI Gateway, including completely new features and minor improvements to existing functionality.

**Before Submitting An Enhancement Suggestion:**

* Check if there's already a package which provides that enhancement.
* Determine which repository the enhancement should be suggested in.
* Perform a cursory search to see if the enhancement has already been suggested.

**How Do I Submit A (Good) Enhancement Suggestion?**

Enhancement suggestions are tracked as [GitHub issues](https://github.com/Noveum/ai-gateway/issues). Create an issue and provide the following information:

* Use a clear and descriptive title
* Provide a step-by-step description of the suggested enhancement
* Provide specific examples to demonstrate the steps
* Describe the current behavior and explain which behavior you expected to see instead
* Explain why this enhancement would be useful

### Your First Code Contribution

Unsure where to begin contributing? You can start by looking through these `beginner` and `help-wanted` issues:

* [Beginner issues](https://github.com/Noveum/ai-gateway/labels/good%20first%20issue) - issues which should only require a few lines of code.
* [Help wanted issues](https://github.com/Noveum/ai-gateway/labels/help%20wanted) - issues which should be a bit more involved than `beginner` issues.

### Pull Request Process

1. Fork the repository
2. Create your feature branch:
   ```bash
   git checkout -b feature/amazing-feature
   ```
3. Commit your changes:
   ```bash
   git commit -m 'Add amazing feature'
   ```
4. Push to the branch:
   ```bash
   git push origin feature/amazing-feature
   ```
5. Open a Pull Request

The PR will be merged once you have the sign-off of at least one maintainer.

## Styleguides

### Git Commit Messages

* Use the present tense ("Add feature" not "Added feature")
* Use the imperative mood ("Move cursor to..." not "Moves cursor to...")
* Limit the first line to 72 characters or less
* Reference issues and pull requests liberally after the first line
* Consider starting the commit message with an applicable emoji:
    * 🎨 `:art:` when improving the format/structure of the code
    * 🐎 `:racehorse:` when improving performance
    * 📝 `:memo:` when writing docs
    * 🐛 `:bug:` when fixing a bug
    * 🔥 `:fire:` when removing code or files
    * 💚 `:green_heart:` when fixing the CI build
    * ✅ `:white_check_mark:` when adding tests
    * 🔒 `:lock:` when dealing with security
    * ⬆️ `:arrow_up:` when upgrading dependencies
    * ⬇️ `:arrow_down:` when downgrading dependencies

### TypeScript Styleguide

* Use TypeScript for all new code
* Use type annotations for function parameters and return types
* Use interfaces over type aliases where possible
* Use meaningful variable names
* Add comments for complex logic
* Keep functions small and focused
* Write unit tests for new features

### Documentation Styleguide

* Use [Markdown](https://daringfireball.net/projects/markdown)
* Reference methods and classes in markdown with the custom `{}` notation:
    * Reference classes with `{ClassName}`
    * Reference instance methods with `{ClassName::methodName}`
    * Reference class methods with `{ClassName.methodName}`

## Additional Notes

### Issue and Pull Request Labels

This section lists the labels we use to help us track and manage issues and pull requests.

* `bug` - Issues that are bugs
* `documentation` - Issues for improving or updating our documentation
* `duplicate` - Issues that are duplicates of other issues
* `enhancement` - Issues for new features or improvements
* `good first issue` - Good for newcomers
* `help wanted` - Extra attention is needed
* `invalid` - Issues that aren't valid
* `question` - Further information is requested
* `wontfix` - Issues that won't be worked on 