//! Anthropic → OpenAI **streaming** translation.
//!
//! Anthropic's `/v1/messages` streaming endpoint speaks its own SSE dialect
//! (`message_start`, `content_block_delta`, `message_delta`, `message_stop`, …).
//! OpenAI clients expect `chat.completion.chunk` frames terminated by
//! `data: [DONE]`. This module is the state machine that turns the former into
//! the latter, plus the [`transform_body`] adapter that drives it over an Axum
//! body.
//!
//! Three properties drive the design:
//!
//! 1. **Byte-exact reassembly.** HTTP/TCP chunk boundaries have nothing to do
//!    with SSE frame boundaries: a flush can land in the middle of the JSON, or
//!    in the middle of a multi-byte UTF-8 character. Bytes are buffered and only
//!    *complete* frames (terminated by a blank line, `\n\n` or `\r\n\r\n`) are
//!    decoded. This mirrors the reassembler already proven in
//!    `telemetry::middleware` (which is private to that module, so the approach
//!    is duplicated here rather than the file being touched).
//!
//! 2. **Errors are visible, never a clean EOF.** A truncated stream that still
//!    ends politely is indistinguishable from a complete answer, so a caller
//!    silently gets half a response. An upstream `error` event, an unparseable
//!    frame, or a stream that stops before `message_stop` all emit an in-band
//!    `event: error` frame and deliberately withhold `data: [DONE]`, then abort
//!    the transport.
//!
//! 3. **Usage survives the translation.** Anthropic splits token accounting
//!    across two events — `input_tokens` on `message_start`, `output_tokens` on
//!    `message_delta`. Both are accumulated and emitted together on the final
//!    chunk as an OpenAI `usage` object, which is what the telemetry layer reads
//!    to price the request.

use axum::body::{Body, Bytes};
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use std::pin::Pin;
use tracing::{debug, warn};

/// Upper bound on a single unterminated SSE frame. A frame larger than this is
/// treated as a broken stream rather than being buffered without limit.
const MAX_FRAME_BYTES: usize = 5 * 1024 * 1024;

/// Model name used when the upstream never announced one (no `message_start`).
const UNKNOWN_MODEL: &str = "claude";

/// What a transformer step produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TransformOutput {
    /// SSE text to forward to the client. May be empty (e.g. a `ping`).
    pub sse: String,
    /// When set, the stream is terminally broken. `sse` still carries the
    /// client-visible error frame and must be flushed first; afterwards the
    /// transport should be aborted rather than closed cleanly.
    pub fatal: Option<String>,
}

impl TransformOutput {
    fn text(sse: String) -> Self {
        Self { sse, fatal: None }
    }
}

/// Incremental SSE frame reassembler.
///
/// Buffers at the **byte** level and hands back only whole frames. A transport
/// chunk can split a multi-byte UTF-8 character, so decoding per chunk would
/// corrupt or drop it; frame terminators are ASCII, so a complete frame is
/// always complete UTF-8.
#[derive(Default)]
struct SseFrameBuffer {
    buf: Vec<u8>,
}

impl SseFrameBuffer {
    /// Append transport bytes; return every frame they complete.
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some((end, sep_len)) = Self::find_frame_end(&self.buf) {
            let frame = self.buf.drain(..end + sep_len).collect::<Vec<u8>>();
            out.push(String::from_utf8_lossy(&frame[..end]).into_owned());
        }
        // A frame that never terminates must not grow without bound; emit what
        // we have so the buffer stays capped. The oversized payload will fail to
        // parse downstream and surface as an error, which is the honest outcome.
        if self.buf.len() > MAX_FRAME_BYTES {
            let frame = std::mem::take(&mut self.buf);
            out.push(String::from_utf8_lossy(&frame).into_owned());
        }
        out
    }

    /// The unterminated remainder at end of stream, if any.
    fn flush(&mut self) -> Option<String> {
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

/// Map an Anthropic `stop_reason` onto an OpenAI `finish_reason`.
fn map_stop_reason(reason: Option<&str>) -> &str {
    match reason {
        Some("end_turn") | Some("stop_sequence") | Some("pause_turn") => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        Some(other) => other,
        None => "stop",
    }
}

/// Streaming state machine: Anthropic message events in, OpenAI chunks out.
///
/// Drive it with [`push`](Self::push) for each transport chunk and exactly one
/// [`finish`](Self::finish) at end of stream.
pub struct AnthropicStreamTransformer {
    frames: SseFrameBuffer,
    /// `created` timestamp stamped on every chunk. Fixed at construction so all
    /// chunks of one response agree (and so tests are deterministic).
    created: i64,
    id: String,
    model: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    finish_reason: Option<String>,
    /// The `finish_reason` + `usage` chunk has been emitted.
    final_emitted: bool,
    saw_message_stop: bool,
    /// Terminal: either `[DONE]` was written or the stream failed. Further bytes
    /// are ignored.
    done: bool,
}

impl AnthropicStreamTransformer {
    /// Build a transformer stamping chunks with `created` (Unix seconds).
    pub fn new(created: i64) -> Self {
        Self {
            frames: SseFrameBuffer::default(),
            created,
            id: String::new(),
            model: UNKNOWN_MODEL.to_string(),
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            finish_reason: None,
            final_emitted: false,
            saw_message_stop: false,
            done: false,
        }
    }

    /// Feed one transport chunk. Returns the SSE text to forward.
    pub fn push(&mut self, bytes: &[u8]) -> TransformOutput {
        if self.done {
            return TransformOutput::default();
        }
        let mut out = String::new();
        for frame in self.frames.push(bytes) {
            match self.handle_frame(&frame) {
                Ok(text) => out.push_str(&text),
                Err(err) => return self.fail(out, err),
            }
            if self.done {
                // `message_stop` wrote `[DONE]`; anything after it is trailing
                // noise from the upstream and must not be forwarded.
                break;
            }
        }
        TransformOutput::text(out)
    }

    /// End of upstream body: flush any unterminated frame and verify the stream
    /// actually completed. A stream that stops before `message_stop` is an
    /// incomplete response, not a successful one.
    pub fn finish(&mut self) -> TransformOutput {
        if self.done {
            return TransformOutput::default();
        }
        let mut out = String::new();
        if let Some(tail) = self.frames.flush() {
            match self.handle_frame(&tail) {
                Ok(text) => out.push_str(&text),
                Err(err) => return self.fail(out, err),
            }
        }
        if self.done {
            return TransformOutput::text(out);
        }
        if !self.saw_message_stop {
            return self.fail(
                out,
                StreamFailure::gateway(
                    "anthropic stream ended without message_stop: response is incomplete",
                ),
            );
        }
        TransformOutput::text(out)
    }

    /// Terminate the stream with a client-visible error frame. `[DONE]` is
    /// deliberately NOT written: its absence is the signal SDKs use to tell a
    /// truncated stream from a complete one.
    fn fail(&mut self, mut out: String, err: StreamFailure) -> TransformOutput {
        self.done = true;
        warn!(
            error_type = %err.kind,
            message = %err.message,
            "Anthropic stream failed; surfacing an error frame to the client"
        );
        out.push_str(&format!(
            "event: error\ndata: {}\n\n",
            json!({
                "error": {
                    "type": err.kind,
                    "message": err.message,
                    "param": Value::Null,
                    "code": Value::Null,
                }
            })
        ));
        TransformOutput {
            sse: out,
            fatal: Some(err.message),
        }
    }

    /// Translate one reassembled SSE frame.
    fn handle_frame(&mut self, frame: &str) -> Result<String, StreamFailure> {
        // Per the SSE spec a frame may carry several `data:` lines, joined with
        // newlines; `event:` names the type and `:` lines are comments.
        let mut data = String::new();
        for line in frame.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(String::new());
        }

        let event: Value = serde_json::from_str(data).map_err(|e| {
            StreamFailure::gateway(format!(
                "malformed event in anthropic stream ({e}); response is incomplete"
            ))
        })?;

        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        match event_type.as_str() {
            "message_start" => Ok(self.on_message_start(&event)),
            "content_block_start" => Ok(self.on_content_block_start(&event)),
            "content_block_delta" => Ok(self.on_content_block_delta(&event)),
            "content_block_stop" => Ok(String::new()),
            "message_delta" => Ok(self.on_message_delta(&event)),
            "message_stop" => Ok(self.on_message_stop()),
            // Keep-alives carry no payload.
            "ping" => Ok(String::new()),
            "error" => Err(StreamFailure::upstream(&event)),
            other => {
                debug!("Ignoring unhandled Anthropic stream event: {}", other);
                Ok(String::new())
            }
        }
    }

    /// `message_start` carries the message id, the resolved model, and the
    /// **input** half of the token accounting.
    fn on_message_start(&mut self, event: &Value) -> String {
        let message = event.get("message");
        if let Some(id) = message
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.id = id.to_string();
        }
        if let Some(model) = message
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.model = model.to_string();
        }
        if let Some(usage) = message.and_then(|m| m.get("usage")) {
            self.absorb_usage(usage);
        }
        // OpenAI's first chunk announces the assistant role.
        self.chunk(
            json!([{ "index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": Value::Null }]),
            None,
        )
    }

    /// A text block may open with content already in it; anything else (tool
    /// use, thinking) has no OpenAI `delta.content` equivalent.
    fn on_content_block_start(&mut self, event: &Value) -> String {
        let text = event
            .get("content_block")
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if text.is_empty() {
            String::new()
        } else {
            self.content_delta(text)
        }
    }

    fn on_content_block_delta(&mut self, event: &Value) -> String {
        let delta = event.get("delta");
        let kind = delta
            .and_then(|d| d.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind != "text_delta" {
            debug!("Ignoring non-text Anthropic content delta: {}", kind);
            return String::new();
        }
        let text = delta
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if text.is_empty() {
            String::new()
        } else {
            self.content_delta(text)
        }
    }

    /// `message_delta` carries the stop reason and the **output** half of the
    /// token accounting — the point at which usage becomes complete.
    fn on_message_delta(&mut self, event: &Value) -> String {
        if let Some(reason) = event
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(Value::as_str)
        {
            self.finish_reason = Some(map_stop_reason(Some(reason)).to_string());
        }
        if let Some(usage) = event.get("usage") {
            self.absorb_usage(usage);
        }
        self.final_chunk()
    }

    fn on_message_stop(&mut self) -> String {
        self.saw_message_stop = true;
        let mut out = String::new();
        // Defensive: a stream that never sent `message_delta` still needs a
        // terminating chunk so the client sees a finish_reason and usage.
        if !self.final_emitted {
            out.push_str(&self.final_chunk());
        }
        out.push_str("data: [DONE]\n\n");
        self.done = true;
        out
    }

    /// Fold an Anthropic `usage` object into the running totals. Both events
    /// that carry usage are partial, so fields are merged, never reset.
    fn absorb_usage(&mut self, usage: &Value) {
        let field = |name: &str| usage.get(name).and_then(Value::as_u64);
        if let Some(input) = field("input_tokens") {
            self.input_tokens = Some(input);
        }
        if let Some(output) = field("output_tokens") {
            self.output_tokens = Some(output);
        }
        // Cache hits are billed separately upstream; surface them for
        // observability without folding them into the billable prompt count
        // (which matches how the non-streaming path reports `input_tokens`).
        let cached = field("cache_read_input_tokens");
        if cached.is_some() {
            self.cached_tokens = cached;
        }
    }

    fn content_delta(&self, text: &str) -> String {
        self.chunk(
            json!([{ "index": 0, "delta": {"content": text}, "finish_reason": Value::Null }]),
            None,
        )
    }

    /// The terminating chunk: finish reason plus the merged usage totals.
    fn final_chunk(&mut self) -> String {
        self.final_emitted = true;
        let finish = self
            .finish_reason
            .clone()
            .unwrap_or_else(|| map_stop_reason(None).to_string());
        let input = self.input_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        let mut usage = json!({
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input + output,
        });
        if let Some(cached) = self.cached_tokens {
            usage["prompt_tokens_details"] = json!({ "cached_tokens": cached });
        }
        self.chunk(
            json!([{ "index": 0, "delta": {}, "finish_reason": finish }]),
            Some(usage),
        )
    }

    /// Render one OpenAI `chat.completion.chunk` as an SSE frame.
    fn chunk(&self, choices: Value, usage: Option<Value>) -> String {
        let id = if self.id.is_empty() {
            "chatcmpl-anthropic"
        } else {
            self.id.as_str()
        };
        let mut chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": choices,
            "system_fingerprint": format!("anthropic-{}", self.model),
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        format!("data: {chunk}\n\n")
    }
}

/// A terminal stream failure, rendered into a client-visible error frame.
struct StreamFailure {
    kind: String,
    message: String,
}

impl StreamFailure {
    fn gateway(message: impl Into<String>) -> Self {
        Self {
            kind: "gateway_error".to_string(),
            message: message.into(),
        }
    }

    /// Build from an Anthropic `error` event, preserving its type and message.
    fn upstream(event: &Value) -> Self {
        let error = event.get("error");
        let kind = error
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("upstream_error")
            .to_string();
        let message = error
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("anthropic returned an error mid-stream")
            .to_string();
        Self { kind, message }
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>;

/// Driver phases for [`transform_body`].
enum Phase {
    /// Reading the upstream body.
    Body(ByteStream, Box<AnthropicStreamTransformer>),
    /// Upstream ended; flush the transformer.
    Tail(Box<AnthropicStreamTransformer>),
    /// Abort the transport after the error frame has been flushed.
    Abort(String),
    Done,
}

/// Wrap an Anthropic event-stream body in the OpenAI-shaped translation.
///
/// On failure the client-visible `event: error` frame is emitted first and the
/// stream then yields a transport error, so the body ends abnormally instead of
/// looking like a completed response. (An outer layer that swallows the
/// transport error still cannot make it look complete: `data: [DONE]` is never
/// written once the stream has failed.)
pub fn transform_body(body: Body, created: i64) -> Body {
    let stream = futures_util::stream::unfold(
        Phase::Body(
            Box::pin(body.into_data_stream()),
            Box::new(AnthropicStreamTransformer::new(created)),
        ),
        |phase| async move {
            match phase {
                Phase::Body(mut upstream, mut transformer) => loop {
                    match upstream.next().await {
                        Some(Ok(bytes)) => {
                            let out = transformer.push(&bytes);
                            if let Some(err) = out.fatal {
                                return Some((Ok(Bytes::from(out.sse)), Phase::Abort(err)));
                            }
                            if out.sse.is_empty() {
                                continue;
                            }
                            return Some((
                                Ok(Bytes::from(out.sse)),
                                Phase::Body(upstream, transformer),
                            ));
                        }
                        Some(Err(e)) => {
                            // The upstream connection broke mid-response: same
                            // rule as a bad event — make it visible.
                            let out = transformer
                                .fail(String::new(), StreamFailure::gateway(e.to_string()));
                            return Some((
                                Ok(Bytes::from(out.sse)),
                                Phase::Abort(out.fatal.unwrap_or_default()),
                            ));
                        }
                        // Upstream finished: hand off to the flush phase. The
                        // empty data frame is a no-op for the client.
                        None => return Some((Ok(Bytes::new()), Phase::Tail(transformer))),
                    }
                },
                Phase::Tail(mut transformer) => {
                    let out = transformer.finish();
                    match out.fatal {
                        Some(err) => Some((Ok(Bytes::from(out.sse)), Phase::Abort(err))),
                        None if out.sse.is_empty() => None,
                        None => Some((Ok(Bytes::from(out.sse)), Phase::Done)),
                    }
                }
                Phase::Abort(err) => Some((Err(std::io::Error::other(err)), Phase::Done)),
                Phase::Done => None,
            }
        },
    );
    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATED: i64 = 1_700_000_000;

    /// Feed `raw` through a transformer in slices of `step` bytes and return the
    /// full forwarded SSE text plus the terminal error, if any.
    fn run(raw: &[u8], step: usize) -> (String, Option<String>) {
        let mut transformer = AnthropicStreamTransformer::new(CREATED);
        let mut sse = String::new();
        for piece in raw.chunks(step.max(1)) {
            let out = transformer.push(piece);
            sse.push_str(&out.sse);
            if let Some(err) = out.fatal {
                return (sse, Some(err));
            }
        }
        let out = transformer.finish();
        sse.push_str(&out.sse);
        (sse, out.fatal)
    }

    /// Parse the `data:` payloads out of forwarded SSE text (skipping `[DONE]`).
    fn chunks(sse: &str) -> Vec<Value> {
        sse.split("\n\n")
            .filter_map(|frame| {
                frame
                    .lines()
                    .find_map(|l| l.strip_prefix("data: "))
                    .filter(|d| *d != "[DONE]")
            })
            .map(|d| serde_json::from_str(d).expect("forwarded chunk must be valid JSON"))
            .collect()
    }

    /// A realistic Anthropic stream: multi-byte UTF-8 in the deltas, usage split
    /// across `message_start` and `message_delta`.
    fn happy_stream() -> String {
        concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01ABC\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5-20250929\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Héllo\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" wörld 🌍\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )
        .to_string()
    }

    #[test]
    fn happy_path_produces_openai_chunks_and_final_usage() {
        let raw = happy_stream();
        let (sse, fatal) = run(raw.as_bytes(), raw.len());
        assert!(fatal.is_none(), "clean stream must not fail: {fatal:?}");
        assert!(sse.ends_with("data: [DONE]\n\n"), "stream must terminate");

        let out = chunks(&sse);
        assert_eq!(out.len(), 4, "role chunk + 2 content deltas + final chunk");

        // First chunk announces the role and the resolved model.
        assert_eq!(out[0]["object"], "chat.completion.chunk");
        assert_eq!(out[0]["id"], "msg_01ABC");
        assert_eq!(out[0]["model"], "claude-sonnet-4-5-20250929");
        assert_eq!(out[0]["created"], CREATED);
        assert_eq!(out[0]["choices"][0]["delta"]["role"], "assistant");

        // `ping`, `content_block_start` (empty) and `content_block_stop` emit
        // nothing; the text deltas land in `choices[0].delta.content`.
        assert_eq!(out[1]["choices"][0]["delta"]["content"], "Héllo");
        assert_eq!(out[2]["choices"][0]["delta"]["content"], " wörld 🌍");
        assert!(out[1]["choices"][0]["finish_reason"].is_null());

        // Final chunk: mapped finish reason + usage merged from BOTH events.
        let last = &out[3];
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
        assert_eq!(last["usage"]["prompt_tokens"], 11, "from message_start");
        assert_eq!(last["usage"]["completion_tokens"], 4, "from message_delta");
        assert_eq!(last["usage"]["total_tokens"], 15);
    }

    #[test]
    fn one_byte_at_a_time_is_byte_identical() {
        // The key regression: SSE frames split at ANY offset — including through
        // the middle of multi-byte UTF-8 characters and JSON literals — must
        // reassemble to exactly the same output as the unsplit stream.
        let raw = happy_stream();
        let bytes = raw.as_bytes();
        let (want, want_fatal) = run(bytes, bytes.len());
        assert!(want_fatal.is_none());

        for step in 1..=bytes.len() {
            let (got, fatal) = run(bytes, step);
            assert!(fatal.is_none(), "clean stream failed at step {step}");
            assert_eq!(got, want, "output differs when split every {step} bytes");
        }
    }

    #[test]
    fn every_single_cut_point_reassembles_identically() {
        // Same guarantee expressed the other way: one cut, swept across every
        // byte offset of the stream.
        let raw = happy_stream();
        let bytes = raw.as_bytes();
        let (want, _) = run(bytes, bytes.len());
        for cut in 1..bytes.len() {
            let mut transformer = AnthropicStreamTransformer::new(CREATED);
            let mut got = String::new();
            for piece in [&bytes[..cut], &bytes[cut..]] {
                got.push_str(&transformer.push(piece).sse);
            }
            got.push_str(&transformer.finish().sse);
            assert_eq!(got, want, "output differs when cut at byte {cut}");
        }
    }

    #[test]
    fn multibyte_utf8_split_across_chunk_boundaries_is_preserved() {
        // Split *inside* the 4-byte emoji and the 2-byte umlaut specifically.
        let raw = happy_stream();
        let bytes = raw.as_bytes();
        let emoji_at = raw.find('🌍').expect("fixture contains the emoji");
        for offset in 1..4 {
            let (sse, fatal) = {
                let mut transformer = AnthropicStreamTransformer::new(CREATED);
                let mut sse = String::new();
                for piece in [&bytes[..emoji_at + offset], &bytes[emoji_at + offset..]] {
                    sse.push_str(&transformer.push(piece).sse);
                }
                let out = transformer.finish();
                sse.push_str(&out.sse);
                (sse, out.fatal)
            };
            assert!(fatal.is_none(), "split inside emoji broke the stream");
            let out = chunks(&sse);
            assert_eq!(
                out[2]["choices"][0]["delta"]["content"], " wörld 🌍",
                "multi-byte content corrupted when split at emoji byte {offset}"
            );
        }
    }

    #[test]
    fn crlf_framing_is_supported() {
        let raw = happy_stream().replace('\n', "\r\n");
        let (sse, fatal) = run(raw.as_bytes(), 3);
        assert!(fatal.is_none(), "CRLF stream must parse: {fatal:?}");
        let out = chunks(&sse);
        assert_eq!(out.len(), 4);
        assert_eq!(out[3]["usage"]["total_tokens"], 15);
    }

    #[test]
    fn mid_stream_error_event_surfaces_and_withholds_done() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":9}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let (sse, fatal) = run(raw.as_bytes(), 7);

        assert_eq!(
            fatal.as_deref(),
            Some("Overloaded"),
            "error event must terminate the stream"
        );
        assert!(
            !sse.contains("[DONE]"),
            "a failed stream must never look complete"
        );
        assert!(
            sse.contains("event: error"),
            "the error must be visible in-band, not a silent truncation"
        );
        // The partial content the client already received is preserved.
        assert_eq!(chunks(&sse)[1]["choices"][0]["delta"]["content"], "partial");
        // ...and the error payload is machine-readable.
        let err: Value = serde_json::from_str(
            sse.rsplit("event: error\ndata: ")
                .next()
                .unwrap()
                .trim_end(),
        )
        .unwrap();
        assert_eq!(err["error"]["type"], "overloaded_error");
        assert_eq!(err["error"]["message"], "Overloaded");
    }

    #[test]
    fn stream_ending_without_message_stop_is_an_error() {
        // Truncated after `message_delta`: usage arrived, but the stream never
        // completed. This MUST NOT be reported as a clean finish.
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":9}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        );
        let (sse, fatal) = run(raw.as_bytes(), 11);
        assert!(
            fatal.is_some_and(|e| e.contains("message_stop")),
            "truncated stream must be reported as incomplete"
        );
        assert!(!sse.contains("[DONE]"));
        assert!(sse.contains("event: error"));
    }

    #[test]
    fn stream_cut_mid_frame_is_an_error() {
        // Upstream died in the middle of a JSON payload.
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":9}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_de",
        );
        let (sse, fatal) = run(raw.as_bytes(), 5);
        assert!(
            fatal.is_some_and(|e| e.contains("malformed")),
            "a truncated frame must surface as an error"
        );
        assert!(!sse.contains("[DONE]"));
    }

    #[test]
    fn message_stop_without_message_delta_still_terminates_with_usage() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-4-1-20250805\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let (sse, fatal) = run(raw.as_bytes(), 1);
        assert!(fatal.is_none());
        let out = chunks(&sse);
        let last = out.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
        assert_eq!(last["usage"]["prompt_tokens"], 3);
        assert_eq!(last["usage"]["completion_tokens"], 2);
        assert!(sse.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn cache_read_tokens_are_surfaced_without_inflating_prompt_tokens() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":5,\"cache_read_input_tokens\":100}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let (sse, fatal) = run(raw.as_bytes(), 64);
        assert!(fatal.is_none());
        let out = chunks(&sse);
        let last = out.last().unwrap();
        assert_eq!(last["usage"]["prompt_tokens"], 5);
        assert_eq!(last["usage"]["prompt_tokens_details"]["cached_tokens"], 100);
        assert_eq!(
            last["choices"][0]["finish_reason"], "length",
            "max_tokens maps to length"
        );
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason(Some("end_turn")), "stop");
        assert_eq!(map_stop_reason(Some("stop_sequence")), "stop");
        assert_eq!(map_stop_reason(Some("max_tokens")), "length");
        assert_eq!(map_stop_reason(Some("tool_use")), "tool_calls");
        assert_eq!(map_stop_reason(Some("refusal")), "content_filter");
        assert_eq!(map_stop_reason(Some("weird")), "weird");
        assert_eq!(map_stop_reason(None), "stop");
    }

    #[test]
    fn non_text_deltas_are_ignored_not_forwarded() {
        // Tool-use JSON deltas have no OpenAI `delta.content` equivalent; they
        // must not be forwarded as text.
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let (sse, fatal) = run(raw.as_bytes(), 13);
        assert!(fatal.is_none());
        let out = chunks(&sse);
        assert_eq!(out.len(), 2, "role chunk + final chunk only");
        assert_eq!(out[1]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn transform_body_end_to_end() {
        use http_body_util::BodyExt;

        let raw = happy_stream();
        let body = transform_body(Body::from(raw.clone()), CREATED);
        let bytes = body.collect().await.expect("clean stream").to_bytes();
        let sse = String::from_utf8(bytes.to_vec()).unwrap();
        let (want, _) = run(raw.as_bytes(), raw.len());
        assert_eq!(sse, want, "body adapter must match the transformer output");
    }

    #[tokio::test]
    async fn transform_body_aborts_on_error_event_after_flushing_it() {
        use http_body_util::BodyExt;

        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":9}}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        let mut body = transform_body(Body::from(raw), CREATED);
        let mut seen = String::new();
        let mut aborted = false;
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(f) => {
                    if let Some(d) = f.data_ref() {
                        seen.push_str(&String::from_utf8_lossy(d));
                    }
                }
                Err(_) => {
                    aborted = true;
                    break;
                }
            }
        }
        assert!(
            seen.contains("event: error"),
            "error frame must reach the client before the abort"
        );
        assert!(!seen.contains("[DONE]"));
        assert!(aborted, "the body must end abnormally, not cleanly");
    }
}
