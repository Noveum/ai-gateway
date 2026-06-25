# Model Pricing

The gateway computes a per-request USD cost from a built-in model pricing table
([`src/policy/pricing.rs`](../src/policy/pricing.rs)). The same table is used by
the Nova Guard `cost_cap` reservation estimate and by every provider's metrics
extractor, so cost is single-sourced.

> **Accuracy / scope.** Prices are USD **per 1,000,000 tokens**, standard
> synchronous tier, current as of **June 2026**, verified against official
> provider pricing pages. The table intentionally does **not** model
> cached-input, batch, or tiered pricing — see [caveats](#caveats). It exists to
> support budget caps and cost annotation, not to be the system of record for
> billing.

## How lookup works

* **Exact match** on the lowercased model id wins.
* Otherwise a **dated-snapshot family match** applies: the longest table id that
  the requested id extends *at a version boundary* (`-`, `:`, `.`, `@`, `/`)
  wins — so `gpt-4o-2024-11-20` → `gpt-4o`, but `gpt-4` (shorter, no boundary)
  and garbage prefixes like `g` resolve to **nothing** (cost `0`, never a wrong
  family).
* Unknown models cost `0.0`; the caller decides whether unknown-model cost
  should fail open or closed.

## Provider coverage

The gateway proxies these providers (via `x-provider`). All are priced from the
table below **except OpenRouter**, which is a meta-router — it is priced only when
the upstream model id it returns also appears in the table, and otherwise reports
cost `0` (see [caveats](#caveats)).

| Provider | `x-provider` | Endpoint / mode |
|---|---|---|
| OpenAI | `openai` | native |
| Anthropic | `anthropic` | native |
| AWS Bedrock | `bedrock` | SigV4 |
| Groq | `groq` | OpenAI-compatible |
| Together AI | `together` | OpenAI-compatible |
| Fireworks AI | `fireworks` | OpenAI-compatible |
| Mistral | `mistral` | OpenAI-compatible |
| Cohere | `cohere` | OpenAI-compatibility endpoint |
| Google Gemini | `google` / `gemini` | OpenAI-compatibility endpoint (`/v1beta/openai`) |
| DeepSeek | `deepseek` | OpenAI-compatible |
| xAI (Grok) | `xai` / `grok` | OpenAI-compatible |
| OpenRouter | `openrouter` | OpenAI-compatible (routed; priced only if upstream model is in the table) |
| Perplexity | `perplexity` | OpenAI-compatible |

## Pricing table (USD per 1M tokens)

### OpenAI
| model | input | output |
|---|---|---|
| gpt-5 | 1.25 | 10.00 |
| gpt-5-mini | 0.25 | 2.00 |
| gpt-5-nano | 0.05 | 0.40 |
| gpt-4.1 | 2.00 | 8.00 |
| gpt-4.1-mini | 0.40 | 1.60 |
| gpt-4.1-nano | 0.10 | 0.40 |
| gpt-4o | 2.50 | 10.00 |
| gpt-4o-mini | 0.15 | 0.60 |
| o3 | 2.00 | 8.00 |
| o4-mini | 1.10 | 4.40 |
| o1 | 15.00 | 60.00 |
| text-embedding-3-small | 0.02 | — |
| text-embedding-3-large | 0.13 | — |

### Anthropic
| model | input | output |
|---|---|---|
| claude-opus-4-8 | 5.00 | 25.00 |
| claude-opus-4-7 | 5.00 | 25.00 |
| claude-opus-4-6 | 5.00 | 25.00 |
| claude-sonnet-4-6 | 3.00 | 15.00 |
| claude-sonnet-4-5 | 3.00 | 15.00 |
| claude-haiku-4-5 | 1.00 | 5.00 |
| claude-fable-5 | 10.00 | 50.00 |

### Google Gemini
| model | input | output |
|---|---|---|
| gemini-2.5-pro | 1.25 | 10.00 |
| gemini-2.5-flash | 0.30 | 2.50 |
| gemini-2.5-flash-lite | 0.10 | 0.40 |

### Groq
| model | input | output |
|---|---|---|
| llama-3.3-70b-versatile | 0.59 | 0.79 |
| llama-3.1-8b-instant | 0.05 | 0.08 |
| meta-llama/llama-4-scout-17b-16e-instruct | 0.11 | 0.34 |
| openai/gpt-oss-120b | 0.15 | 0.60 |
| openai/gpt-oss-20b | 0.075 | 0.30 |

### Mistral
| model | input | output |
|---|---|---|
| mistral-large-latest | 0.50 | 1.50 |
| mistral-medium-latest | 1.50 | 7.50 |
| mistral-small-latest | 0.15 | 0.60 |
| codestral-latest | 0.30 | 0.90 |
| magistral-medium-latest | 2.00 | 5.00 |

### Cohere
| model | input | output |
|---|---|---|
| command-a-03-2025 | 2.50 | 10.00 |
| command-r-plus-08-2024 | 2.50 | 10.00 |
| command-r-08-2024 | 0.15 | 0.60 |
| command-r7b-12-2024 | 0.0375 | 0.15 |

### Together AI
| model | input | output |
|---|---|---|
| meta-llama/llama-3.3-70b-instruct-turbo | 1.04 | 1.04 |
| meta-llama/llama-4-maverick-17b-128e-instruct-fp8 | 0.27 | 0.85 |
| meta-llama/llama-4-scout-17b-16e-instruct | 0.18 | 0.59 |
| deepseek-ai/deepseek-v3 | 1.25 | 1.25 |

### Fireworks AI
| model | input | output |
|---|---|---|
| accounts/fireworks/models/deepseek-v4-pro | 1.74 | 3.48 |
| accounts/fireworks/models/deepseek-v4-flash | 0.14 | 0.28 |
| accounts/fireworks/models/kimi-k2p6 | 0.95 | 4.00 |
| accounts/fireworks/models/llama-v3p3-70b-instruct | 0.90 | 0.90 |

### AWS Bedrock (US on-demand)
| model | input | output |
|---|---|---|
| anthropic.claude-opus-4-5-20251101-v1:0 | 5.00 | 25.00 |
| anthropic.claude-sonnet-4-5-20250929-v1:0 | 3.00 | 15.00 |
| anthropic.claude-haiku-4-5-20251001-v1:0 | 1.00 | 5.00 |
| amazon.nova-pro-v1:0 | 0.80 | 3.20 |
| amazon.nova-lite-v1:0 | 0.06 | 0.24 |
| amazon.nova-micro-v1:0 | 0.035 | 0.14 |
| amazon.nova-premier-v1:0 | 2.50 | 12.50 |

### DeepSeek (cache-miss input)
| model | input | output |
|---|---|---|
| deepseek-v4-flash | 0.14 | 0.28 |
| deepseek-v4-pro | 0.435 | 0.87 |
| deepseek-chat | 0.27 | 1.10 |
| deepseek-reasoner | 0.55 | 2.19 |

### xAI (Grok)
| model | input | output |
|---|---|---|
| grok-4.3 | 1.25 | 2.50 |
| grok-build-0.1 | 1.00 | 2.00 |

### Perplexity (token cost only; per-request search fees billed separately)
| model | input | output |
|---|---|---|
| sonar | 1.00 | 1.00 |
| sonar-pro | 3.00 | 15.00 |
| sonar-reasoning-pro | 2.00 | 8.00 |

## Caveats

1. **Gemini 2.5 Pro is tiered** — input/output double above a 200K-token prompt.
   The table uses the ≤200K rate; large-context calls are under-priced. Handle
   the threshold separately if exact billing matters.
2. **Cache-hit / cache-miss** — DeepSeek and Fireworks have large cache-hit
   discounts; the table uses cache-miss input. Track cached tokens separately to
   avoid over-billing cached traffic.
3. **Perplexity** adds per-request search fees ($5–$14 / 1,000 requests) on top
   of tokens — not representable as a token rate.
4. **OpenRouter** is a meta-router with per-upstream-model pricing; it is routed
   but not priced in the table (cost annotates as `0` unless the underlying
   model id also appears here).
5. **xAI** retired several slugs (`grok-3`, `grok-4`) that now redirect to
   `grok-4.3`; map old ids explicitly if you depend on them.

## Updating prices

Edit the `MODEL_PRICING` array in [`src/policy/pricing.rs`](../src/policy/pricing.rs)
(`(model_id, input_per_1m, output_per_1m)` tuples) and update this document. The
lookup logic and cost math are covered by unit tests in that module.
