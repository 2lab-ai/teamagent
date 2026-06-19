//! SSE passthrough: stream upstream `text/event-stream` bodies to the client
//! with backpressure and client-disconnect detection, while extracting usage
//! from `message_start` / `message_delta` events for stats (FR1).
//!
//! Porting pitfall: events fragment across chunks — buffer to event boundary
//! (`\n\n`) before parsing; never assume one chunk == one event.
//!
//! Byte-identity contract: the bytes sent to the client are the exact chunks
//! received from upstream — the [`EventBuffer`] only *observes* a copy for
//! usage stats and never rewrites the stream, so parse failures cannot
//! corrupt the relay.

use bytes::Bytes;
use tokio_stream::StreamExt as _;

/// Token usage extracted from a message stream. `input_tokens` is the FRESH
/// (non-cached) prompt count on both providers; the cached components are kept
/// separately and optionally — `None` means the upstream did not report that
/// field (rendered "—"), distinct from `Some(0)` (an explicit zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

impl StreamUsage {
    /// Accumulate another observation (saturating). For the optional cache
    /// counters the result is present iff at least one side reported a value.
    pub fn add(&mut self, other: StreamUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_input_tokens =
            add_opt(self.cache_read_input_tokens, other.cache_read_input_tokens);
        self.cache_creation_input_tokens = add_opt(
            self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
    }
}

/// Saturating add of two optional counters where `None` means "unavailable":
/// the sum is present iff at least one operand reported a value. Shared by the
/// stream-usage accumulator and the model-usage aggregation.
pub fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.saturating_add(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

/// Reassembles SSE events from arbitrarily fragmented chunks. Push bytes in,
/// get complete events out; partial trailing data stays buffered.
#[derive(Debug, Default)]
pub struct EventBuffer {
    buf: Vec<u8>,
}

impl EventBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk and drain every COMPLETE event (terminated by a blank
    /// line, i.e. `\n\n`) accumulated so far, in order. Whitespace-only
    /// events (stray blank lines) are skipped.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<u8> = self.buf.drain(..pos + 2).collect();
            let text = String::from_utf8_lossy(&event[..pos]);
            if !text.trim().is_empty() {
                events.push(text.into_owned());
            }
        }
        events
    }

    /// Drain whatever is left after the stream ends — an unterminated final
    /// event still gets parsed for usage (mirrors the teamclaude tail parse).
    pub fn take_remainder(&mut self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.buf);
        let out = if text.trim().is_empty() {
            None
        } else {
            Some(text.into_owned())
        };
        self.buf.clear();
        out
    }
}

/// Extract usage from one complete SSE event, if it is a `message_start`
/// (input tokens) or `message_delta` (output tokens) event. Malformed
/// events yield `None` — usage stats are best-effort, never fatal.
pub fn extract_usage(event: &str) -> Option<StreamUsage> {
    let data = event.lines().find_map(|line| {
        line.strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
    })?;
    let value: serde_json::Value = serde_json::from_str(data.trim()).ok()?;
    match value.get("type")?.as_str()? {
        "message_start" => {
            let usage = value.get("message")?.get("usage")?;
            let input = usage.get("input_tokens")?.as_u64()?;
            Some(StreamUsage {
                input_tokens: input,
                output_tokens: 0,
                // Anthropic prompt-caching counters, present only when the
                // request used caching — captured opportunistically (req8/9).
                cache_read_input_tokens: usage
                    .get("cache_read_input_tokens")
                    .and_then(serde_json::Value::as_u64),
                cache_creation_input_tokens: usage
                    .get("cache_creation_input_tokens")
                    .and_then(serde_json::Value::as_u64),
            })
        }
        "message_delta" => {
            let output = value.get("usage")?.get("output_tokens")?.as_u64()?;
            Some(StreamUsage {
                input_tokens: 0,
                output_tokens: output,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            })
        }
        _ => None,
    }
}

/// Stateful per-request SSE transformer: upstream events in, downstream SSE
/// bytes out. The codex provider's Responses→Anthropic converter implements
/// this; the Anthropic passthrough never goes through it (byte-identity path
/// untouched).
pub trait SseTransform: Send {
    /// One COMPLETE upstream event (terminated `\n\n` already stripped) →
    /// zero or more downstream SSE bytes.
    fn on_event(&mut self, event: &str) -> Vec<u8>;

    /// Upstream ended (cleanly or not) — flush any termination events.
    fn on_end(&mut self) -> Vec<u8>;

    /// Usage accumulated from the EMITTED Anthropic events, for the
    /// dashboard totals.
    fn usage(&self) -> StreamUsage;
}

/// Relay an upstream SSE response through a [`SseTransform`]: upstream
/// chunks are reassembled into complete events, each event is fed to the
/// transform, and the transform's OUTPUT bytes are what the client receives
/// (this is the codex path; the byte-identity path is
/// [`passthrough_body`]). Backpressure/disconnect semantics are identical to
/// the passthrough pump.
///
/// `finish` receives the transform's usage, TWO independent observe-only
/// buffers of the EMITTED Anthropic-SSE bytes, the upstream error if one
/// aborted the stream, the finished transform, and whether the client
/// disconnected:
/// - `captured` — the first `capture_limit` emitted bytes (short debug log
///   excerpt).
/// - `raw_captured` — the first `raw_capture_limit` emitted bytes (raw-io
///   full-payload tee).
///
/// Both are filled from the same emitted output, capped independently. Each
/// emitted chunk is `tx.send`'d to the client FIRST; the copies are a side
/// effect. Callers move the account lease into `finish` (never switch
/// mid-stream).
pub fn transform_body<T, F>(
    upstream: reqwest::Response,
    mut transform: T,
    capture_limit: usize,
    raw_capture_limit: usize,
    finish: F,
) -> axum::body::Body
where
    T: SseTransform + 'static,
    // `finish` also receives the finished transform (for converter-level detail
    // like the codex trace's raw usage / event count) and whether the relay
    // ended because the CLIENT disconnected (vs. upstream completing).
    F: FnOnce(StreamUsage, Vec<u8>, RawCapture, Option<String>, &T, bool) + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut events = EventBuffer::new();
        let mut captured: Vec<u8> = Vec::new();
        let mut raw_captured = RawCapture::new(raw_capture_limit);
        let mut error: Option<String> = None;
        let mut stream = Box::pin(upstream.bytes_stream());
        let mut client_gone = false;
        'pump: while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    for event in events.push(&chunk) {
                        let out = transform.on_event(&event);
                        if out.is_empty() {
                            continue;
                        }
                        // Send FIRST, then observe both buffers.
                        if tx.send(Ok(Bytes::from(out.clone()))).await.is_err() {
                            client_gone = true;
                            break 'pump;
                        }
                        capture(&mut captured, &out, capture_limit);
                        raw_captured.push(&out);
                    }
                }
                Err(err) => {
                    error = Some(err.to_string());
                    break;
                }
            }
        }
        if let Some(rest) = events.take_remainder() {
            let out = transform.on_event(&rest);
            if !out.is_empty() && !client_gone {
                if tx.send(Ok(Bytes::from(out.clone()))).await.is_err() {
                    client_gone = true;
                } else {
                    capture(&mut captured, &out, capture_limit);
                    raw_captured.push(&out);
                }
            }
        }
        let tail = transform.on_end();
        if !tail.is_empty() && !client_gone && tx.send(Ok(Bytes::from(tail.clone()))).await.is_ok()
        {
            capture(&mut captured, &tail, capture_limit);
            raw_captured.push(&tail);
        }
        if let Some(err) = &error {
            if !client_gone {
                let _ = tx.send(Err(std::io::Error::other(err.clone()))).await;
            }
        }
        finish(
            transform.usage(),
            captured,
            raw_captured,
            error,
            &transform,
            client_gone,
        );
    });
    axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
}

fn capture(captured: &mut Vec<u8>, chunk: &[u8], limit: usize) {
    if captured.len() < limit {
        let room = limit - captured.len();
        captured.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
}

/// Observe-only accumulator for the raw-io full-payload tee: keeps at most
/// `limit` bytes (memory bound) while counting the TOTAL bytes seen, so a body
/// that overflows the cap can still be truncation-marked with the exact dropped
/// count — unlike a bare `Vec` cap, which loses the overflow size. Filled AFTER
/// each chunk is forwarded to the client; it never feeds back into the stream.
#[derive(Debug, Default)]
pub struct RawCapture {
    bytes: Vec<u8>,
    total: usize,
    limit: usize,
}

impl RawCapture {
    /// A tee bounded to `limit` retained bytes.
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            total: 0,
            limit,
        }
    }

    /// Observe a chunk already forwarded to the client: count all of it, retain
    /// up to the cap.
    pub fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len());
        capture(&mut self.bytes, chunk, self.limit);
    }

    /// Total bytes seen (including those dropped past the cap).
    pub fn total(&self) -> usize {
        self.total
    }

    /// The retained (bounded) bytes; the kept prefix when the body overflowed.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Relay an upstream SSE response as an axum body, observing usage on the
/// side. The pump task ends when upstream finishes OR the client disconnects
/// (the channel receiver is dropped, `send` fails, and we stop polling
/// upstream — dropping the `reqwest::Response` closes the upstream stream).
///
/// `finish` runs exactly once when the relay ends, with the accumulated usage,
/// TWO independent observe-only buffers, and the upstream error if one aborted
/// the stream:
/// - `captured` — the first `capture_limit` relayed bytes (the short debug
///   request-log excerpt, typically 8 KiB).
/// - `raw_captured` — the first `raw_capture_limit` relayed bytes (the raw-io
///   full-payload tee, typically `max_body_bytes` = 8 MiB).
///
/// Both are filled from the SAME forwarded chunks but capped independently, so
/// the debug log stays a short excerpt while raw-io retains the full (bounded)
/// body. Each chunk is `tx.send`'d to the client FIRST; the copies are a side
/// effect that never blocks, slows, or mutates the relayed bytes.
///
/// Callers move the account lease into this closure so the account stays pinned
/// for the stream's lifetime — errors after this point propagate to the client
/// as a broken body, never as an account switch (never switch mid-stream).
pub fn passthrough_body<F>(
    upstream: reqwest::Response,
    capture_limit: usize,
    raw_capture_limit: usize,
    finish: F,
) -> axum::body::Body
where
    F: FnOnce(StreamUsage, Vec<u8>, RawCapture, Option<String>) + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut events = EventBuffer::new();
        let mut usage = StreamUsage::default();
        let mut captured: Vec<u8> = Vec::new();
        let mut raw_captured = RawCapture::new(raw_capture_limit);
        let mut error: Option<String> = None;
        let mut stream = Box::pin(upstream.bytes_stream());
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    for event in events.push(&chunk) {
                        if let Some(observed) = extract_usage(&event) {
                            usage.add(observed);
                        }
                    }
                    // Backpressure: bounded channel; client disconnect drops
                    // the receiver and we stop polling upstream. Send FIRST,
                    // then observe — the copies never delay the client.
                    if tx.send(Ok(chunk.clone())).await.is_err() {
                        break;
                    }
                    capture(&mut captured, &chunk, capture_limit);
                    raw_captured.push(&chunk);
                }
                Err(err) => {
                    error = Some(err.to_string());
                    let _ = tx.send(Err(std::io::Error::other(err))).await;
                    break;
                }
            }
        }
        if let Some(rest) = events.take_remainder() {
            if let Some(observed) = extract_usage(&rest) {
                usage.add(observed);
            }
        }
        finish(usage, captured, raw_captured, error);
    });
    axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}";
    const MESSAGE_DELTA: &str =
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}";

    #[test]
    fn whole_event_in_one_chunk() {
        let mut buf = EventBuffer::new();
        let events = buf.push(format!("{MESSAGE_START}\n\n").as_bytes());
        assert_eq!(events, vec![MESSAGE_START.to_string()]);
    }

    #[test]
    fn event_split_mid_line_across_chunks() {
        let whole = format!("{MESSAGE_START}\n\n");
        // Split in the middle of the JSON payload (mid-line).
        let (a, b) = whole.split_at(whole.len() / 2);
        let mut buf = EventBuffer::new();
        assert!(
            buf.push(a.as_bytes()).is_empty(),
            "incomplete event stays buffered"
        );
        assert_eq!(buf.push(b.as_bytes()), vec![MESSAGE_START.to_string()]);
    }

    #[test]
    fn event_split_mid_terminator() {
        // The "\n\n" terminator itself fragments across chunks.
        let mut buf = EventBuffer::new();
        assert!(buf.push(format!("{MESSAGE_DELTA}\n").as_bytes()).is_empty());
        assert_eq!(buf.push(b"\n"), vec![MESSAGE_DELTA.to_string()]);
    }

    #[test]
    fn multiple_events_in_one_chunk_plus_partial_tail() {
        let chunk = format!("{MESSAGE_START}\n\n{MESSAGE_DELTA}\n\nevent: partial\ndata: {{");
        let mut buf = EventBuffer::new();
        let events = buf.push(chunk.as_bytes());
        assert_eq!(
            events,
            vec![MESSAGE_START.to_string(), MESSAGE_DELTA.to_string()]
        );
        assert_eq!(
            buf.take_remainder(),
            Some("event: partial\ndata: {".to_string())
        );
    }

    #[test]
    fn one_byte_at_a_time_still_yields_the_event() {
        let whole = format!("{MESSAGE_DELTA}\n\n");
        let mut buf = EventBuffer::new();
        let mut events = Vec::new();
        for byte in whole.as_bytes() {
            events.extend(buf.push(&[*byte]));
        }
        assert_eq!(events, vec![MESSAGE_DELTA.to_string()]);
    }

    #[test]
    fn blank_only_events_are_skipped() {
        let mut buf = EventBuffer::new();
        assert!(buf.push(b"\n\n\n\n").is_empty());
        assert_eq!(buf.take_remainder(), None);
    }

    #[test]
    fn extract_usage_message_start() {
        assert_eq!(
            extract_usage(MESSAGE_START),
            Some(StreamUsage {
                input_tokens: 25,
                output_tokens: 0,
                // No cache keys in the payload → unavailable, not zero.
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            })
        );
    }

    #[test]
    fn extract_usage_message_start_captures_cache_fields() {
        let event = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2679,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":40000,\"output_tokens\":3}}}";
        assert_eq!(
            extract_usage(event),
            Some(StreamUsage {
                input_tokens: 2679,
                output_tokens: 0,
                // Present in the payload → captured (explicit 0 stays Some(0)).
                cache_read_input_tokens: Some(40000),
                cache_creation_input_tokens: Some(0),
            })
        );
    }

    #[test]
    fn extract_usage_message_delta() {
        assert_eq!(
            extract_usage(MESSAGE_DELTA),
            Some(StreamUsage {
                input_tokens: 0,
                output_tokens: 42,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            })
        );
    }

    #[test]
    fn extract_usage_ignores_other_events_and_malformed_json() {
        assert_eq!(
            extract_usage("event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}"),
            None
        );
        assert_eq!(extract_usage("data: {not json"), None);
        assert_eq!(extract_usage("event: ping"), None);
        assert_eq!(
            extract_usage("data: {\"type\":\"message_start\",\"message\":{}}"),
            None,
            "missing usage payload is tolerated"
        );
    }

    #[test]
    fn usage_accumulates() {
        let mut total = StreamUsage::default();
        total.add(StreamUsage {
            input_tokens: 10,
            output_tokens: 0,
            cache_read_input_tokens: Some(5),
            cache_creation_input_tokens: None,
        });
        total.add(StreamUsage {
            input_tokens: 0,
            output_tokens: 7,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        });
        assert_eq!(
            total,
            StreamUsage {
                input_tokens: 10,
                output_tokens: 7,
                // cache_read carried from the first observation; cache_creation
                // never reported → stays unavailable.
                cache_read_input_tokens: Some(5),
                cache_creation_input_tokens: None,
            }
        );
    }

    #[test]
    fn add_opt_is_present_iff_either_side_is() {
        assert_eq!(add_opt(None, None), None);
        assert_eq!(add_opt(Some(3), None), Some(3));
        assert_eq!(add_opt(None, Some(4)), Some(4));
        assert_eq!(add_opt(Some(3), Some(4)), Some(7));
    }
}
