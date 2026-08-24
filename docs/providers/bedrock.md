# AWS Bedrock provider

The Bedrock adapter accepts OpenAI-shaped **text chat-completions** requests and
converts them to the AWS Bedrock Converse API. Buffered Converse responses are
converted back to OpenAI chat-completions shape on both runtimes. Native Axum
also supports ConverseStream and converts AWS EventStream frames to OpenAI SSE.
The Cloudflare Worker does not implement Bedrock streaming in v2.0.1 and
rejects `stream: true` with an OpenAI-shaped HTTP 400 and
`error.code = "unsupported_feature"` before admission or AWS dispatch.

This is not full OpenAI feature parity. In particular, OpenAI `tools`,
`tool_calls`, multimodal parts, and provider-native Bedrock request extensions
are not currently translated by this adapter. Use text messages only; tool-use
conversion is tracked as follow-up work.

## Authentication

Send these headers to the gateway:

```text
x-provider: bedrock
x-aws-access-key-id: <access key>
x-aws-secret-access-key: <secret key>
x-aws-region: us-east-1
```

`x-aws-region` defaults to `us-east-1`. Both runtimes accept the optional
`x-aws-session-token` header for temporary STS credentials.

For STS credentials, add this header to the request; omit it for a long-lived
access-key pair:

```bash
-H "x-aws-session-token: $AWS_SESSION_TOKEN"
```

These headers contain AWS signing credentials. Do **not** send them through the
public/shared `gate.noveum.ai` service, a browser, or a gateway operated by a
third party. Use a private gateway deployment you control, TLS, sensitive-header
redaction at every proxy, and narrowly scoped credentials. Prefer short-lived
session credentials on either runtime. Instance-role assumption is not
implemented by this adapter; credentials still arrive in request headers.

Grant the credentials only the model resources they need. Buffered requests
need `bedrock:InvokeModel`; add `bedrock:InvokeModelWithResponseStream` only to
credentials used for native streaming:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": [
      "bedrock:InvokeModel"
    ],
    "Resource": "<foundation-model-or-inference-profile-arn>"
  }]
}
```

## Request mapping

The adapter currently maps:

| OpenAI chat field | Bedrock Converse field |
|---|---|
| `messages[].role/content` (text) | `messages[].role/content[].text` |
| `system` messages | top-level `system[].text` |
| `max_tokens`, `max_completion_tokens`, or `max_output_tokens` | `inferenceConfig.maxTokens` |
| `temperature` | `inferenceConfig.temperature` |
| `top_p` | `inferenceConfig.topP` |
| `stop` | `inferenceConfig.stopSequences` |
| `stream: true` | Native only: ConverseStream + OpenAI SSE translation; Worker returns HTTP 400 `unsupported_feature` |

Under an enforcing strict NovaGuard cost cap, output-limit aliases must agree,
the body must be bounded JSON `/v1/chat/completions`, and provider-native fields
such as `inferenceConfig` are rejected before admission. This keeps the amount
reserved by NovaGuard tied to the request that Bedrock receives.

## Example

```bash
curl --fail-with-body http://127.0.0.1:3000/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'x-provider: bedrock' \
  -H "x-aws-access-key-id: $AWS_ACCESS_KEY_ID" \
  -H "x-aws-secret-access-key: $AWS_SECRET_ACCESS_KEY" \
  -H "x-aws-region: ${AWS_REGION:-us-east-1}" \
  -d '{
    "model": "amazon.nova-micro-v1:0",
    "max_tokens": 64,
    "messages": [{"role": "user", "content": "Reply with one sentence."}]
  }'
```

The exact model must be enabled for the AWS account and callable from the
selected source Region. Model availability is account- and Region-specific, so
the gateway does not claim that every model listed by AWS is enabled for every
deployment.

## Claude inference-profile pricing

The catalog stores the published **global** Bedrock Claude rates. For the
catalogued Claude 4.5 models, NovaGuard applies the documented 1.1x token/cache
multiplier to direct and geography-scoped model IDs, while an explicitly
`global.` inference profile remains at the global rate.

Recognized forms include:

```text
anthropic.claude-sonnet-4-5-20250929-v1:0
us.anthropic.claude-sonnet-4-5-20250929-v1:0
global.anthropic.claude-sonnet-4-5-20250929-v1:0
arn:aws:bedrock:us-east-1:123456789012:inference-profile/global.anthropic.claude-sonnet-4-5-20250929-v1:0
```

Commercial AWS system-defined inference-profile ARNs are normalized only when
the backing catalog model can be identified from the ARN. Application
inference-profile ARNs and non-commercial AWS-partition ARNs are not assigned a
commercial rate; NovaGuard keeps them unpriced/assumed, and a `failClosed` cost
cap rejects them rather than guessing.

Under an enforcing strict cost cap, this catalogued commercial Claude set is
the complete Bedrock pricing surface. Amazon Nova, Titan, and other Bedrock
families remain available for ordinary/advisory proxying, but strict requests
receive HTTP 400 `unsupported_strict_input` before `/admit`. AWS publishes
source-Region-specific Nova prices (the public 2026-08-20 price list, for
example, prices Nova Pro in `eu-south-1` at $1.28/M input and $5.21/M output,
versus the catalog's $0.80/$3.20 global row). The gateway cannot make that hold
exact until the validated `x-aws-region` is threaded into both reservation and
settlement.

## Streaming and settlement

On the native gateway, Bedrock's `application/vnd.amazon.eventstream` response
is preserved until the decoder has reassembled complete AWS frames. The adapter
then emits separate OpenAI SSE events and a final `[DONE]`. NovaGuard reads the
native `inputTokens`, `outputTokens`, `cacheReadInputTokens`, and
`cacheWriteInputTokens` counters before the OpenAI response conversion so a
strict reservation settles across every reported token/cache dimension.

On the Worker, use buffered Bedrock requests only. A streaming request returns
HTTP 400 `unsupported_feature` before any strict reservation or provider call;
it is never silently downgraded to buffered mode.

## Known limitations

- OpenAI function/tool definitions and Bedrock `toolUse`/`toolResult` blocks are
  not translated yet.
- Multimodal Converse content is not exposed by the current OpenAI adapter.
- Worker Bedrock streaming is not implemented; native Bedrock streaming is.
- Native and Worker signing both accept temporary session credentials; the
  Worker remains buffered-only while native supports ConverseStream.
- A live gateway Bedrock call still requires safely forwarding AWS credentials
  plus model entitlement. The 2026-08-24 release-hardening audit confirmed
  direct AWS Nova Micro buffered and streaming access, but did not forward
  those AWS credentials through the gateway. Treat gateway-level Bedrock as
  unverified in that audit rather than inferring a pass from direct AWS success.
  Hermetic tests cover signing, buffered conversion, native fragmented AWS
  EventStream reassembly/SSE framing, and usage settlement without an AWS bill.

## References

- [AWS Bedrock pricing](https://aws.amazon.com/bedrock/pricing/)
- [AWS public Bedrock price list](https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonBedrock/current/index.json)
- [Bedrock Converse API](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html)
- [Global cross-Region inference](https://docs.aws.amazon.com/bedrock/latest/userguide/global-cross-region-inference.html)
- [Regional availability and model ID forms](https://docs.aws.amazon.com/bedrock/latest/userguide/models-region-compatibility.html)
- [IAM best practices](https://docs.aws.amazon.com/IAM/latest/UserGuide/best-practices.html)
