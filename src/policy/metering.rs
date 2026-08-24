//! Shared metering primitives: SSE frame reassembly + response-usage parsing.
//!
//! This module is the one place the gateway turns *provider bytes* into
//! *authoritative token counts*. It is deliberately pure and synchronous — no
//! `reqwest`, no `tokio`, no I/O — so it compiles unchanged for the native
//! server **and** for the `wasm32-unknown-unknown` Cloudflare Worker, and so it
//! is testable without a runtime.
//!
//! Two things live here, both previously duplicated:
//!
//! 1. [`SseFrameBuffer`](crate::policy::metering::SseFrameBuffer) — incremental
//!    SSE frame reassembly. HTTP/TCP chunk
//!    boundaries have nothing to do with SSE frame boundaries: a provider (or
//!    any hop) may flush `data: {...}` split in the middle of the JSON, or even
//!    in the middle of a multi-byte UTF-8 sequence. Parsing each transport chunk
//!    on its own silently drops those events. Two proven copies of this existed
//!    (`telemetry::middleware`, private, and `providers::anthropic_stream`,
//!    copied *because* the first was private); this is now the only one.
//!
//! 2. [`extract_actual_usage`](crate::policy::metering::extract_actual_usage) /
//!    [`StreamUsageScanner`](crate::policy::metering::StreamUsageScanner) —
//!    reading the provider's own token counts out of a buffered JSON body or out
//!    of a live SSE stream.
//!
//! ## Why the stream scanner matters
//!
//! A strict-mode request reserves `input_tokens + max_output_tokens` up front.
//! Streams used to settle via `abandon`, which by design RETAINS that estimate.
//! But `max_tokens` is routinely 4096 while a real streamed reply is often tens
//! of tokens, so every streaming request could be billed ~100x its true cost and
//! consume the customer's cost cap accordingly. "Errs high" is only acceptable
//! when usage is genuinely unrecoverable — and for SSE it is *recoverable*: the
//! final chunk carries authoritative usage.
//! [`StreamUsageScanner`](crate::policy::metering::StreamUsageScanner) recovers
//! it without buffering the response, so the reservation can be reconciled down
//! to actual via `complete`.

use serde_json::Value;

/// Upper bound on a single unterminated SSE frame (and on the reassembler's
/// buffer). A frame larger than this is emitted as-is rather than buffered
/// without limit: it will fail to parse downstream and surface as an error,
/// which is the honest outcome. Matches the caps the two former copies used.
pub const MAX_FRAME_BYTES: usize = 5 * 1024 * 1024;

/// Incremental SSE frame reassembler.
///
/// Holds raw bytes that have not yet completed a frame and hands back only whole
/// frames (terminated by a blank line — `\n\n` or `\r\n\r\n`). Buffering at the
/// *byte* level is deliberate: a transport chunk can split a multi-byte UTF-8
/// character, so decoding per chunk would corrupt or discard it. Frame
/// terminators are ASCII, so a complete frame is always complete UTF-8.
///
/// Feeding a stream through this in slices of *any* size — down to one byte at a
/// time — yields exactly the frames the unsplit stream would. That guarantee is
/// covered by tests here and, end to end, in both former call sites.
pub struct SseFrameBuffer {
    buf: Vec<u8>,
    /// Emit-and-reset threshold for a frame that never terminates.
    cap: usize,
}

impl Default for SseFrameBuffer {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            cap: MAX_FRAME_BYTES,
        }
    }
}

impl SseFrameBuffer {
    /// A reassembler with the default [`MAX_FRAME_BYTES`] cap.
    pub fn new() -> Self {
        Self::default()
    }

    /// A reassembler with an explicit unterminated-frame cap (tests).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap,
        }
    }

    /// Append transport bytes; return every frame they complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some((end, sep_len)) = Self::find_frame_end(&self.buf) {
            let frame = self.buf.drain(..end + sep_len).collect::<Vec<u8>>();
            out.push(String::from_utf8_lossy(&frame[..end]).into_owned());
        }
        // A frame that never terminates must not grow without bound; emit what
        // we have so the buffer stays capped.
        if self.buf.len() > self.cap {
            let frame = std::mem::take(&mut self.buf);
            out.push(String::from_utf8_lossy(&frame).into_owned());
        }
        out
    }

    /// The unterminated remainder at end of stream, if any.
    pub fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let frame = std::mem::take(&mut self.buf);
        let text = String::from_utf8_lossy(&frame).into_owned();
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Offset and length of the first frame terminator, if the buffer holds one.
    fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
        let find = |pat: &[u8]| buf.windows(pat.len()).position(|w| w == pat);
        match (find(b"\r\n\r\n"), find(b"\n\n")) {
            (Some(a), Some(b)) if a <= b => Some((a, 4)),
            (_, Some(b)) => Some((b, 2)),
            (Some(a), None) => Some((a, 4)),
            (None, None) => None,
        }
    }
}

/// Authoritative token counts recovered from a provider response.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ActualUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Full provider-aware price when detailed usage was available. `None`
    /// retains the legacy two-token fallback for call sites without a provider.
    pub cost_usd: Option<f64>,
}

/// The `(input, output)` token counts a `usage` object carries, each `None` when
/// the provider did not report it.
///
/// Accepts the OpenAI spelling (`prompt_tokens` / `completion_tokens`) and the
/// Anthropic one (`input_tokens` / `output_tokens`). Keeping the `Option`s
/// distinct (rather than defaulting to zero) is what lets a *stream* tell
/// "reported 0 output tokens" from "never reported output tokens" — the
/// difference between reconciling a reservation and having to retain it.
pub fn usage_fields(usage: &Value) -> (Option<u32>, Option<u32>) {
    let num = |keys: [&str; 2]| -> Option<u32> {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(|v| v.as_u64()))
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
    };
    (
        num(["prompt_tokens", "input_tokens"]),
        num(["completion_tokens", "output_tokens"]),
    )
}

/// Pull the provider's own token counts out of a (non-streaming) response body.
///
/// `None` means "not authoritative": the caller must NOT invent numbers, it
/// abandons the reservation instead so the conservative estimate stands.
pub fn extract_actual_usage(body: &Value) -> Option<ActualUsage> {
    let usage = body.get("usage")?;
    let (input, output) = usage_fields(usage);
    // Both halves are required. Completing a reservation with an invented zero
    // for either missing half releases headroom for spend the provider may
    // still have billed; retaining the conservative hold is the safe outcome.
    let (Some(input), Some(output)) = (input, output) else {
        return None;
    };
    Some(ActualUsage {
        input_tokens: input,
        output_tokens: output,
        cost_usd: None,
    })
}

/// Recover authoritative counts and price every usage dimension the provider
/// reported (cache reads/writes, TTL tiers, server tools, and token output).
pub fn extract_actual_usage_priced(
    model: &str,
    provider: &str,
    body: &Value,
) -> Option<ActualUsage> {
    let mut actual = extract_actual_usage(body)?;
    if let Some(declared) = crate::policy::pricing::parse_usage(model, provider, body) {
        actual.input_tokens = declared.total_input_tokens();
        actual.output_tokens = declared.output_tokens;
        actual.cost_usd = Some(crate::policy::pricing::price_usage(model, &declared).total_usd);
    }
    Some(actual)
}

/// Recovers a stream's authoritative usage while its bytes flow past, unchanged,
/// to the client.
///
/// Feed every transport chunk to [`push`](Self::push) (the same bytes are still
/// forwarded verbatim — nothing here rewrites or holds them back), call
/// [`finish`](Self::finish) once at end of stream, then read [`usage`](Self::usage).
///
/// Understands both dialects the gateway sees:
///
/// * **OpenAI** — the terminal `chat.completion.chunk` carries a top-level
///   `usage` object (the gateway forces `stream_options.include_usage` while
///   platform metering is active, so it is always present). This is also the
///   shape an Anthropic response has *after* the gateway's own
///   `anthropic_stream` translation.
/// * **Anthropic (raw)** — `message_start` carries `message.usage.input_tokens`
///   and `message_delta` carries `usage.output_tokens`; both are folded in.
///
/// Also tolerates providers that stream bare JSON objects instead of `data:`
/// lines.
#[derive(Default)]
pub struct StreamUsageScanner {
    frames: SseFrameBuffer,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    /// Anthropic splits usage across events; OpenAI supplies it at the end.
    /// Merge the raw fields so provider-aware pricing can see every dimension.
    usage_fields: serde_json::Map<String, Value>,
    finished: bool,
}

impl StreamUsageScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one transport chunk. Pure inspection: the caller forwards the
    /// identical bytes onward.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.finished {
            return;
        }
        for frame in self.frames.push(bytes) {
            self.ingest_frame(&frame);
        }
    }

    /// End of stream: fold in any frame the provider left unterminated.
    /// Idempotent.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        if let Some(tail) = self.frames.flush() {
            self.ingest_frame(&tail);
        }
        self.finished = true;
    }

    /// The usage this stream reported, or `None` when it is not authoritative.
    ///
    /// **Both token halves are required.** An input-only report (an Anthropic
    /// `message_start` from a stream that then died, say) tells us nothing about
    /// what was generated; an output-only report does not reveal the billed
    /// prompt. Completing either with an invented zero silently releases part
    /// of the hold. Those streams must keep the conservative estimate via
    /// `abandon`.
    pub fn usage(&self) -> Option<ActualUsage> {
        Some(ActualUsage {
            input_tokens: self.input_tokens?,
            output_tokens: self.output_tokens?,
            cost_usd: None,
        })
    }

    /// The authoritative stream usage, priced with the same detailed catalog
    /// path as a buffered provider response.
    pub fn usage_priced(&self, model: &str, provider: &str) -> Option<ActualUsage> {
        let mut actual = self.usage()?;
        if !self.usage_fields.is_empty() {
            let body = serde_json::json!({
                "usage": Value::Object(self.usage_fields.clone()),
            });
            if let Some(declared) = crate::policy::pricing::parse_usage(model, provider, &body) {
                actual.input_tokens = declared.total_input_tokens();
                actual.output_tokens = declared.output_tokens;
                actual.cost_usd =
                    Some(crate::policy::pricing::price_usage(model, &declared).total_usd);
            }
        }
        Some(actual)
    }

    /// Parse one reassembled frame and fold whatever usage it carries into the
    /// running totals. Later reports win: Anthropic's `message_start` announces
    /// a placeholder `output_tokens` that `message_delta` then corrects.
    fn ingest_frame(&mut self, frame: &str) {
        // Per the SSE spec a frame may carry several `data:` lines, joined with
        // newlines; `event:` names the type and `:` lines are comments. Some
        // providers stream bare JSON objects with no `data:` prefix at all.
        let payload = match frame_data(frame) {
            Some(d) => d,
            None => frame.to_string(),
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        // Top-level `usage`: OpenAI's terminal chunk, Anthropic's `message_delta`.
        // Nested `message.usage`: Anthropic's `message_start`.
        for usage in [event.get("usage"), event.pointer("/message/usage")]
            .into_iter()
            .flatten()
        {
            if let Some(fields) = usage.as_object() {
                for (name, value) in fields {
                    self.usage_fields.insert(name.clone(), value.clone());
                }
            }
            let (input, output) = usage_fields(usage);
            if input.is_some() {
                self.input_tokens = input;
            }
            if output.is_some() {
                self.output_tokens = output;
            }
        }
    }
}

/// The `data:` payload of one reassembled SSE frame, per the SSE spec: every
/// `data:` line, one optional leading space stripped, joined with newlines.
/// `None` when the frame has no `data:` line at all (a comment, an `event:`-only
/// frame, or a bare JSON payload).
pub fn frame_data(frame: &str) -> Option<String> {
    let mut data = String::new();
    let mut saw = false;
    for line in frame.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            if saw {
                data.push('\n');
            }
            saw = true;
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    saw.then_some(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A real-shaped OpenAI stream: content deltas (one carrying multi-byte
    /// UTF-8), the final usage frame, then `[DONE]`.
    fn openai_stream() -> String {
        concat!(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5.6-luna\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Héllo\"}}]}\n\n",
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5.6-luna\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" wörld 🌍\"}}]}\n\n",
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5.6-luna\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string()
    }

    /// The raw Anthropic dialect: usage split across `message_start` (input) and
    /// `message_delta` (output).
    fn anthropic_stream() -> String {
        concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Héllo 🌍\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )
        .to_string()
    }

    #[test]
    fn streamed_anthropic_usage_preserves_cache_and_server_tool_cost() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cost\",\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":20,\"cache_creation\":{\"ephemeral_5m_input_tokens\":12,\"ephemeral_1h_input_tokens\":8},\"server_tool_use\":{\"web_search_requests\":2}}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut scanner = StreamUsageScanner::new();
        for bytes in raw.as_bytes().chunks(3) {
            scanner.push(bytes);
        }
        scanner.finish();

        let usage = scanner
            .usage_priced("claude-sonnet-5", "anthropic")
            .expect("terminal output usage is authoritative");
        let expected_body = serde_json::json!({
            "usage": {
                "input_tokens": 5,
                "output_tokens": 7,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 20,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 12,
                    "ephemeral_1h_input_tokens": 8
                },
                "server_tool_use": {"web_search_requests": 2}
            }
        });
        let declared =
            crate::policy::pricing::parse_usage("claude-sonnet-5", "anthropic", &expected_body)
                .unwrap();
        let expected = crate::policy::pricing::price_usage("claude-sonnet-5", &declared).total_usd;
        assert_eq!(usage.input_tokens, declared.total_input_tokens());
        assert_eq!(usage.output_tokens, 7);
        assert!((usage.cost_usd.unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn streamed_xai_usage_prices_exact_ticks_or_hidden_reasoning_tokens() {
        for (ticks, expected_output, expected_cost) in
            [(Some(37_756_000_u64), 103, 0.0037756), (None, 103, 0.0)]
        {
            let mut usage = json!({
                "prompt_tokens": 32,
                "completion_tokens": 9,
                "total_tokens": 135,
                "completion_tokens_details": {"reasoning_tokens": 94}
            });
            if let Some(ticks) = ticks {
                usage["cost_in_usd_ticks"] = json!(ticks);
            }
            let raw = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({
                    "choices": [],
                    "usage": usage
                })
            );
            let mut scanner = StreamUsageScanner::new();
            for bytes in raw.as_bytes().chunks(2) {
                scanner.push(bytes);
            }
            scanner.finish();
            let actual = scanner
                .usage_priced("grok-4.6", "xai")
                .expect("both token halves were reported");
            assert_eq!(actual.input_tokens, 32);
            assert_eq!(actual.output_tokens, expected_output);
            if ticks.is_some() {
                assert_eq!(actual.cost_usd, Some(expected_cost));
            } else {
                assert!(actual.cost_usd.is_some_and(|cost| cost > expected_cost));
            }
        }
    }

    /// Drive the scanner over `raw` in slices of `step` bytes.
    fn scan(raw: &[u8], step: usize) -> Option<ActualUsage> {
        let mut s = StreamUsageScanner::new();
        for piece in raw.chunks(step.max(1)) {
            s.push(piece);
        }
        s.finish();
        s.usage()
    }

    #[test]
    fn frames_reassemble_identically_at_every_split_size() {
        for raw in [openai_stream(), anthropic_stream()] {
            let bytes = raw.as_bytes();
            let mut whole = SseFrameBuffer::new();
            let want = whole.push(bytes);
            assert!(!want.is_empty());
            for step in 1..=bytes.len() {
                let mut b = SseFrameBuffer::new();
                let mut got = Vec::new();
                for piece in bytes.chunks(step) {
                    got.extend(b.push(piece));
                }
                if let Some(tail) = b.flush() {
                    got.push(tail);
                }
                assert_eq!(got, want, "frames differ when split every {step} bytes");
            }
        }
    }

    #[test]
    fn frames_reassemble_identically_at_every_single_cut_point() {
        let raw = openai_stream();
        let bytes = raw.as_bytes();
        let want = SseFrameBuffer::new().push(bytes);
        for cut in 1..bytes.len() {
            let mut b = SseFrameBuffer::new();
            let mut got = Vec::new();
            for piece in [&bytes[..cut], &bytes[cut..]] {
                got.extend(b.push(piece));
            }
            if let Some(tail) = b.flush() {
                got.push(tail);
            }
            assert_eq!(got, want, "frames differ when cut at byte {cut}");
        }
    }

    #[test]
    fn crlf_framing_and_an_unterminated_tail() {
        let raw = "data: {\"a\":1}\r\n\r\ndata: {\"b\":2}";
        let mut b = SseFrameBuffer::new();
        let mut got = Vec::new();
        for piece in raw.as_bytes().chunks(3) {
            got.extend(b.push(piece));
        }
        got.extend(b.flush());
        assert_eq!(got, vec!["data: {\"a\":1}", "data: {\"b\":2}"]);
        // A buffer holding only whitespace is not a frame.
        let mut b = SseFrameBuffer::new();
        b.push(b"   \n");
        assert_eq!(b.flush(), None);
    }

    #[test]
    fn an_unterminated_frame_is_emitted_rather_than_buffered_forever() {
        let mut b = SseFrameBuffer::with_cap(16);
        assert!(b.push(b"data: ").is_empty());
        let out = b.push(&[b'x'; 32]);
        assert_eq!(out.len(), 1, "the cap must force an emit");
        assert!(out[0].starts_with("data: xxx"));
        assert_eq!(b.flush(), None, "and the buffer is empty afterwards");
    }

    #[test]
    fn an_openai_stream_yields_its_real_usage_at_every_split() {
        let raw = openai_stream();
        let bytes = raw.as_bytes();
        let want = ActualUsage {
            input_tokens: 11,
            output_tokens: 4,
            cost_usd: None,
        };
        for step in 1..=bytes.len() {
            assert_eq!(scan(bytes, step), Some(want), "lost usage at step {step}");
        }
    }

    #[test]
    fn an_anthropic_stream_merges_usage_across_its_two_events() {
        let raw = anthropic_stream();
        let bytes = raw.as_bytes();
        // `message_start` says output_tokens: 1 — the `message_delta` correction
        // must win, or every Anthropic stream settles at one output token.
        let want = ActualUsage {
            input_tokens: 11,
            output_tokens: 4,
            cost_usd: None,
        };
        for step in 1..=bytes.len() {
            assert_eq!(scan(bytes, step), Some(want), "lost usage at step {step}");
        }
    }

    #[test]
    fn a_stream_that_never_reports_output_is_not_authoritative() {
        // Content deltas only (an OpenAI stream without `include_usage`).
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(scan(raw.as_bytes(), 4), None);

        // Anthropic truncated after `message_start`: input is known, output is
        // NOT. Completing at output=0 would release the entire hold.
        let raw = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n";
        assert_eq!(scan(raw.as_bytes(), 5), None);

        // Nothing at all.
        assert_eq!(scan(b"", 1), None);
        assert_eq!(scan(b": keep-alive\n\n", 1), None);
    }

    #[test]
    fn an_explicit_zero_is_authoritative_but_a_missing_half_is_not() {
        // The provider explicitly said "0 completion tokens" — that is a real
        // measurement and must reconcile the reservation.
        let raw =
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":0}}\n\n";
        assert_eq!(
            scan(raw.as_bytes(), 3),
            Some(ActualUsage {
                input_tokens: 7,
                output_tokens: 0,
                cost_usd: None,
            })
        );
        // Output reported without input cannot reveal the billed prompt.
        let raw = "data: {\"usage\":{\"completion_tokens\":12}}\n\n";
        assert_eq!(scan(raw.as_bytes(), 3), None);
    }

    #[test]
    fn a_usage_frame_missing_its_terminator_is_still_recovered() {
        // Provider closed the connection right after writing the usage chunk,
        // with no trailing blank line.
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}";
        assert_eq!(
            scan(raw.as_bytes(), 7),
            Some(ActualUsage {
                input_tokens: 3,
                output_tokens: 2,
                cost_usd: None,
            })
        );
    }

    #[test]
    fn bare_json_streams_and_multiline_data_are_understood() {
        // No `data:` prefix at all.
        let raw = "{\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\n\n";
        assert_eq!(
            scan(raw.as_bytes(), 2),
            Some(ActualUsage {
                input_tokens: 5,
                output_tokens: 6,
                cost_usd: None,
            })
        );
        // Multiple `data:` lines in one frame are joined with newlines.
        let raw = "data: {\"usage\":\ndata: {\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n";
        assert_eq!(
            scan(raw.as_bytes(), 3),
            Some(ActualUsage {
                input_tokens: 1,
                output_tokens: 2,
                cost_usd: None,
            })
        );
    }

    #[test]
    fn finish_is_idempotent_and_push_after_finish_is_inert() {
        let mut s = StreamUsageScanner::new();
        s.push(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n");
        s.finish();
        s.finish();
        let before = s.usage();
        s.push(b"data: {\"usage\":{\"prompt_tokens\":99,\"completion_tokens\":99}}\n\n");
        assert_eq!(s.usage(), before, "a settled scanner must not drift");
    }

    #[test]
    fn buffered_body_usage_reads_either_provider_spelling() {
        assert_eq!(
            extract_actual_usage(&json!({
                "usage": {"prompt_tokens": 123, "completion_tokens": 45, "total_tokens": 168}
            })),
            Some(ActualUsage {
                input_tokens: 123,
                output_tokens: 45,
                cost_usd: None,
            })
        );
        assert_eq!(
            extract_actual_usage(&json!({"usage": {"input_tokens": 7, "output_tokens": 9}})),
            Some(ActualUsage {
                input_tokens: 7,
                output_tokens: 9,
                cost_usd: None,
            })
        );
        // Nothing authoritative → None, so the caller retains its estimate.
        for body in [
            json!({"choices": [{"message": {"content": "hi"}}]}),
            json!({"usage": {}}),
            json!({"usage": {"total_tokens": 10}}),
            json!({"usage": {"prompt_tokens": 7}}),
            json!({"usage": {"completion_tokens": 5}}),
            json!({"usage": null}),
            json!({}),
        ] {
            assert_eq!(extract_actual_usage(&body), None, "body: {body}");
        }
    }

    #[test]
    fn stream_usage_requires_both_authoritative_token_halves() {
        for usage in [json!({"prompt_tokens": 7}), json!({"completion_tokens": 5})] {
            let mut scanner = StreamUsageScanner::new();
            scanner.push(format!("data: {}\n\n", json!({"usage": usage})).as_bytes());
            scanner.finish();
            assert_eq!(scanner.usage(), None, "usage: {usage}");
        }
    }

    #[test]
    fn oversized_counts_saturate_rather_than_wrapping() {
        let (i, o) = usage_fields(&json!({
            "prompt_tokens": u64::MAX, "completion_tokens": 5u64
        }));
        assert_eq!(i, Some(u32::MAX));
        assert_eq!(o, Some(5));
    }

    #[test]
    fn frame_data_follows_the_sse_spec() {
        assert_eq!(frame_data("data: hi"), Some("hi".to_string()));
        // Exactly one leading space is stripped.
        assert_eq!(frame_data("data:  hi"), Some(" hi".to_string()));
        assert_eq!(frame_data("data:hi"), Some("hi".to_string()));
        assert_eq!(
            frame_data("event: x\ndata: a\ndata: b"),
            Some("a\nb".to_string())
        );
        assert_eq!(frame_data("event: ping"), None);
        assert_eq!(frame_data(": comment"), None);
    }
}
