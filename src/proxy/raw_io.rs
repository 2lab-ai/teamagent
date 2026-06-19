//! Raw input/output payload capture (`[raw_io]`, Feature B): one JSON line per
//! proxied request appended to `$XDG_STATE_HOME/llmux/raw-io.jsonl`, holding the
//! verbatim request body and the response body delivered to the client, so the
//! actual traffic can be replayed/audited offline.
//!
//! This is DISTINCT from activity persistence (`activity.jsonl`), which keeps
//! per-request *metadata* (status, tokens, model). This store keeps the payload
//! *bytes*.
//!
//! # Best-effort, never on the hot path
//!
//! Capture mirrors the discipline in [`crate::proxy::codex_trace`]: building a
//! record never fails the request, and [`append`] swallows every IO/
//! serialization error. The proxy must never block, backpressure, or mutate the
//! bytes forwarded to the client to capture them — so the response body is only
//! ever an *observed copy*, filled on the relay's pump task AFTER each chunk has
//! been forwarded to the client (`tx.send` first, the copy is a side effect),
//! written when the client stream has finished. A disabled config or an
//! unresolvable state dir makes the whole thing a no-op.
//!
//! # Decoupled from the 8 KiB debug body-log cap
//!
//! The streaming relays keep TWO independent observe-only buffers: a short one
//! capped at the debug request-log's 8 KiB
//! [`crate::proxy::logging::BODY_LOG_LIMIT`] (for the `=== RESPONSE BODY ===`
//! log excerpt) and a separate one for raw-io capped at the configurable
//! [`crate::config::RawIoConfig::max_body_bytes`] (default
//! [`RESPONSE_CAP_BYTES`], 8 MiB). They are filled side by side from the same
//! forwarded chunks. Reusing the 8 KiB debug cap for raw-io would truncate
//! every streamed response to 8 KiB — but real LLM responses stream tens to
//! hundreds of KB, so raw-io needs its own, much larger cap to retain the full
//! payload the feature exists to keep.
//!
//! # Memory cost (the intended tradeoff)
//!
//! With its own buffer, each in-flight STREAMED request can pin up to
//! `max_body_bytes` of response bytes (plus the request body, itself bounded by
//! the same cap) until the stream finishes and the record is flushed. That
//! memory is bounded by the proxy's concurrency cap × `max_body_bytes`. This is
//! the deliberate price of full-payload retention; tune `max_body_bytes` down if
//! the ceiling is too high for the host.
//!
//! # What is captured on each path
//!
//! - **Non-streaming** (`relay` JSON path): request body + the full response
//!   body (it is already materialized to relay it).
//! - **Codex** (`relay_codex`, streaming and non-streaming): request body + the
//!   bytes EMITTED to the client (the converter's Anthropic-SSE output for
//!   streaming clients, the aggregated Messages JSON for non-streaming),
//!   bounded by [`RESPONSE_CAP_BYTES`] / the relay's own capture limit.
//! - **Claude streaming passthrough** (`relay` SSE path): request body + a
//!   BOUNDED tee of the SSE bytes streamed to the client. The tee is a dedicated
//!   raw-io buffer (`passthrough_body`'s `raw_capture_limit`), an in-memory `Vec`
//!   capped at `max_body_bytes`, filled on the pump task AFTER each chunk is
//!   forwarded — it never blocks, slows, or alters the chunk sent to the client
//!   (the chunk is `tx.send`'d first; the copy is a side effect). The record is
//!   flushed in the relay's `finish` closure, after the client stream completes
//!   or on disconnect/error with whatever was captured so far.
//!
//! # Bounds
//!
//! Each captured body is clipped to the configurable `max_body_bytes`
//! ([`crate::config::RawIoConfig::max_body_bytes`], default
//! [`RESPONSE_CAP_BYTES`]) on a UTF-8 char boundary with a
//! `…[truncated N bytes]` marker, so a pathological huge body can't blow memory.
//! The streaming relays accumulate their dedicated raw-io tee up to this same
//! cap (then stop growing), and the non-streaming full-body path clips to it as
//! the final backstop — one cap, every path, request and response alike.

use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Schema version of a [`RawIoRecord`] line. Bump on a breaking layout change;
/// [`prune`] tolerates (skips) lines it cannot parse, so old/new lines coexist.
pub const RECORD_VERSION: u8 = 1;

/// Default cap on each captured body before it is stored, used when no
/// per-config [`crate::config::RawIoConfig::max_body_bytes`] is supplied. A body
/// over the effective cap is clipped on a char boundary with a
/// `…[truncated N bytes]` marker. 8 MiB is generous for a real request/response
/// yet bounds the memory a pathological body can pin.
///
/// This is DELIBERATELY larger than, and independent of, the debug request
/// log's 8 KiB [`crate::proxy::logging::BODY_LOG_LIMIT`]: the debug log keeps a
/// short excerpt for eyeballing while raw-io retains the full (bounded) body for
/// replay/audit. Most LLM responses stream tens to hundreds of KB, so reusing
/// the 8 KiB debug cap here would discard almost the entire response.
pub const RESPONSE_CAP_BYTES: usize = 8 * 1024 * 1024;

/// Milliseconds in a day, for the retention window arithmetic.
const MS_PER_DAY: u64 = 86_400_000;

/// One raw-io line: the verbatim request/response payloads for a single proxied
/// request plus the correlation/attribution fields the forward path already
/// knows. Field-named JSON so adding a field stays backward-readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawIoRecord {
    /// Schema version ([`RECORD_VERSION`]).
    pub v: u8,
    /// Capture timestamp, millis since the Unix epoch (the retention key).
    pub ts_ms: u64,
    /// The request's activity id (correlates with `activity.jsonl` /
    /// `codex-trace.jsonl` / the dashboard feed).
    pub id: u64,
    /// Backend group served ("claude"/"codex"), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Model served, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Account name that served the request, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// HTTP status delivered to the client, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Verbatim request body (bounded + truncation-marked at capture time).
    pub request_body: String,
    /// Response body delivered to the client (bounded + truncation-marked).
    pub response_body: String,
}

impl RawIoRecord {
    /// Build a record from raw bytes, clipping each body to `max_body_bytes`
    /// (the configurable raw-io cap; see
    /// [`crate::config::RawIoConfig::max_body_bytes`], default
    /// [`RESPONSE_CAP_BYTES`]). The SAME cap applies to request and response.
    /// `now_ms` is the capture timestamp; the bodies are stored as lossy UTF-8
    /// (binary payloads degrade gracefully, never panic).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        now_ms: u64,
        group: Option<String>,
        model: Option<String>,
        account: Option<String>,
        status: Option<u16>,
        request_body: &[u8],
        response_body: &[u8],
        max_body_bytes: usize,
    ) -> Self {
        Self {
            v: RECORD_VERSION,
            ts_ms: now_ms,
            id,
            group,
            model,
            account,
            status,
            request_body: bounded_body(request_body, max_body_bytes),
            response_body: bounded_body(response_body, max_body_bytes),
        }
    }
}

/// Clip a body to `max_body_bytes` on a UTF-8 char boundary, appending a
/// `…[truncated N bytes]` marker when it overflows. A body within the cap is
/// returned whole (lossy UTF-8). Pure; never panics.
fn bounded_body(body: &[u8], max_body_bytes: usize) -> String {
    let s = String::from_utf8_lossy(body);
    if s.len() <= max_body_bytes {
        return s.into_owned();
    }
    let mut end = max_body_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    format!("{}…[truncated {} bytes]", &s[..end], dropped)
}

/// Render a STREAMED body that was already bounded at capture time: `kept` is
/// the retained prefix (capped at the relay's raw-io limit) and `total` is the
/// full number of bytes that streamed past the tee. When `total > kept.len()`
/// the body overflowed the cap, so we append the same `…[truncated N bytes]`
/// marker with the exact dropped count — which `bounded_body` alone cannot
/// compute, because the relay only handed us the bounded prefix, not the whole
/// body. When nothing was dropped the prefix is returned whole (lossy UTF-8).
/// Pure; never panics.
fn bounded_body_streamed(kept: &[u8], total: usize) -> String {
    let s = String::from_utf8_lossy(kept).into_owned();
    let dropped = total.saturating_sub(kept.len());
    if dropped == 0 {
        return s;
    }
    format!("{s}…[truncated {dropped} bytes]")
}

/// Wall-clock now, millis since the Unix epoch. Mirrors the idiom in
/// `tui::activity` / `forward`.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Append one record as a JSON line to `path`, best-effort. A `None` path (no
/// state dir / disabled), a serialization failure, or any IO error is swallowed
/// — the request path is never affected, nothing here panics. The parent dir is
/// created if missing; the file is opened `create(true).append(true)`.
pub fn append(path: Option<&std::path::Path>, record: &RawIoRecord) {
    let Some(path) = path else {
        return;
    };
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

/// Build a record at `now_ms()` and [`append`] it, best-effort. The single
/// entry point the forward path calls at a request's terminal outcome: a
/// `None` path (capture disabled / no state dir) is a silent no-op, so callers
/// need not branch. The bodies are clipped to `max_body_bytes` (the
/// configurable raw-io cap; default [`RESPONSE_CAP_BYTES`]).
#[allow(clippy::too_many_arguments)]
pub fn capture(
    path: Option<&std::path::Path>,
    id: u64,
    group: Option<String>,
    model: Option<String>,
    account: Option<String>,
    status: Option<u16>,
    request_body: &[u8],
    response_body: &[u8],
    max_body_bytes: usize,
) {
    if path.is_none() {
        return; // disabled / no state dir — skip building the record at all
    }
    let record = RawIoRecord::new(
        id,
        now_ms(),
        group,
        model,
        account,
        status,
        request_body,
        response_body,
        max_body_bytes,
    );
    append(path, &record);
}

impl RawIoRecord {
    /// Build a record for a STREAMED response: the request body is clipped to
    /// `max_body_bytes` as usual, but the response is the relay's raw-io tee —
    /// already bounded at capture time to `response_kept` with `response_total`
    /// total bytes seen — so its truncation marker is computed from the dropped
    /// count the relay observed (see [`bounded_body_streamed`]). This is what
    /// lets a streamed body that overflows the cap carry an accurate
    /// `…[truncated N bytes]` marker even though only the bounded prefix reaches
    /// this point.
    #[allow(clippy::too_many_arguments)]
    pub fn new_streamed(
        id: u64,
        now_ms: u64,
        group: Option<String>,
        model: Option<String>,
        account: Option<String>,
        status: Option<u16>,
        request_body: &[u8],
        response_kept: &[u8],
        response_total: usize,
        max_body_bytes: usize,
    ) -> Self {
        Self {
            v: RECORD_VERSION,
            ts_ms: now_ms,
            id,
            group,
            model,
            account,
            status,
            request_body: bounded_body(request_body, max_body_bytes),
            response_body: bounded_body_streamed(response_kept, response_total),
        }
    }
}

/// Streaming sibling of [`capture`]: build a record at `now_ms()` from the
/// relay's raw-io tee (`response_kept` = retained prefix, `response_total` =
/// full streamed length) and [`append`] it, best-effort. A `None` path is a
/// silent no-op. The request body is clipped to `max_body_bytes`; the response
/// is marker-truncated from the relay's observed dropped count.
#[allow(clippy::too_many_arguments)]
pub fn capture_streamed(
    path: Option<&std::path::Path>,
    id: u64,
    group: Option<String>,
    model: Option<String>,
    account: Option<String>,
    status: Option<u16>,
    request_body: &[u8],
    response_kept: &[u8],
    response_total: usize,
    max_body_bytes: usize,
) {
    if path.is_none() {
        return; // disabled / no state dir — skip building the record at all
    }
    let record = RawIoRecord::new_streamed(
        id,
        now_ms(),
        group,
        model,
        account,
        status,
        request_body,
        response_kept,
        response_total,
        max_body_bytes,
    );
    append(path, &record);
}

/// Prune the raw-io log to a retention window, best-effort.
///
/// When `retention_days > 0`, the file is rewritten keeping only records whose
/// `ts_ms >= now_ms - retention_days * 86_400_000`. `retention_days == 0` keeps
/// everything (no-op). Corrupt lines (not JSON, or not the current
/// [`RECORD_VERSION`]) are tolerated by being DROPPED — a rewrite is a natural
/// point to shed unreadable history; the kept set is exactly the in-window,
/// parseable records.
///
/// Strictly best-effort: a `None` path, a missing/unreadable file, or any IO
/// error on write leaves the file as-is (the temp file is discarded). Never
/// panics. The rewrite goes through a sibling temp file + atomic rename so a
/// crash mid-prune can't truncate the log.
pub fn prune(path: Option<&std::path::Path>, retention_days: u64, now_ms: u64) {
    if retention_days == 0 {
        return; // keep forever
    }
    let Some(path) = path else {
        return;
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        // Missing/unreadable file = nothing to prune.
        return;
    };
    let cutoff = now_ms.saturating_sub(retention_days.saturating_mul(MS_PER_DAY));

    // Keep only in-window, parseable, current-version records. A line we cannot
    // parse is dropped (best-effort shedding of corruption on rewrite).
    let mut kept = String::with_capacity(contents.len());
    let mut changed = false;
    for line in contents.lines() {
        if line.trim().is_empty() {
            changed = true;
            continue;
        }
        match serde_json::from_str::<RawIoRecord>(line) {
            Ok(record) if record.v == RECORD_VERSION && record.ts_ms >= cutoff => {
                kept.push_str(line);
                kept.push('\n');
            }
            // Out of window, wrong version, or unparseable → drop it.
            _ => changed = true,
        }
    }
    // Nothing to do: every line was kept verbatim (no reorder, no drop).
    if !changed {
        return;
    }

    // Atomic rewrite: write a sibling temp, then rename over the original. On
    // any IO error, leave the original untouched.
    let tmp = path.with_extension("jsonl.prune.tmp");
    if std::fs::write(&tmp, kept.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: u64, ts_ms: u64) -> RawIoRecord {
        RawIoRecord::new(
            id,
            ts_ms,
            Some("claude".into()),
            Some("claude-sonnet-4".into()),
            Some("acct-a".into()),
            Some(200),
            br#"{"model":"m","messages":[]}"#,
            br#"{"id":"msg_1"}"#,
            RESPONSE_CAP_BYTES,
        )
    }

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "llmux-rawio-test-{}-{}-{tag}.jsonl",
            std::process::id(),
            ulid::Ulid::new()
        ))
    }

    #[test]
    fn record_round_trips_through_json_with_fields_intact() {
        let path = tmp_path("roundtrip");
        let record = rec(7, 1_700_000_000_000);
        append(Some(&path), &record);

        let contents = std::fs::read_to_string(&path).expect("file written");
        let parsed: RawIoRecord =
            serde_json::from_str(contents.trim()).expect("one parseable line");
        assert_eq!(parsed, record, "all fields survive the round-trip");
        assert_eq!(parsed.v, RECORD_VERSION);
        assert_eq!(parsed.id, 7);
        assert_eq!(parsed.status, Some(200));
        assert_eq!(parsed.request_body, r#"{"model":"m","messages":[]}"#);
        assert_eq!(parsed.response_body, r#"{"id":"msg_1"}"#);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn body_over_the_cap_is_truncated_with_a_marker() {
        let big = "a".repeat(RESPONSE_CAP_BYTES + 500);
        let record = RawIoRecord::new(
            1,
            0,
            None,
            None,
            None,
            None,
            big.as_bytes(),
            b"resp",
            RESPONSE_CAP_BYTES,
        );
        assert!(
            record.request_body.contains("…[truncated 500 bytes]"),
            "marks the exact dropped byte count"
        );
        assert!(
            record.request_body.len() <= RESPONSE_CAP_BYTES + 64,
            "clipped near the cap (+ short marker), got {}",
            record.request_body.len()
        );
        // A body at/under the cap is stored whole.
        assert_eq!(record.response_body, "resp", "small body kept whole");
    }

    #[test]
    fn body_at_exactly_the_cap_is_kept_whole() {
        let exact = "b".repeat(RESPONSE_CAP_BYTES);
        let record = RawIoRecord::new(
            1,
            0,
            None,
            None,
            None,
            None,
            exact.as_bytes(),
            b"",
            RESPONSE_CAP_BYTES,
        );
        assert_eq!(record.request_body.len(), RESPONSE_CAP_BYTES);
        assert!(!record.request_body.contains("truncated"));
    }

    #[test]
    fn truncation_respects_utf8_char_boundaries() {
        // A multi-byte char straddling the cap boundary must not be split.
        let prefix = "x".repeat(RESPONSE_CAP_BYTES - 1);
        let body = format!("{prefix}€€€"); // '€' is 3 bytes
        let record = RawIoRecord::new(
            1,
            0,
            None,
            None,
            None,
            None,
            body.as_bytes(),
            b"",
            RESPONSE_CAP_BYTES,
        );
        // The stored prefix (before the marker) must be valid UTF-8 by
        // construction (String), and must not include a partial '€'.
        assert!(record.request_body.contains("…[truncated"));
        let kept = record
            .request_body
            .split("…[truncated")
            .next()
            .expect("prefix");
        assert!(kept.is_char_boundary(kept.len()));
        let _ = String::from(kept); // valid UTF-8, no panic
    }

    #[test]
    fn max_body_bytes_override_is_respected() {
        // A body comfortably UNDER the 8 MiB default but OVER a small override
        // must be truncated at the override, proving the cap is configurable and
        // applies to BOTH request and response bodies.
        let body = "z".repeat(100);
        let record = RawIoRecord::new(
            1,
            0,
            None,
            None,
            None,
            None,
            body.as_bytes(),
            body.as_bytes(),
            32,
        );
        assert!(
            record.request_body.contains("…[truncated 68 bytes]"),
            "request body clipped at the override cap (32), got: {}",
            record.request_body
        );
        assert!(
            record.response_body.contains("…[truncated 68 bytes]"),
            "same override cap applies to the response body, got: {}",
            record.response_body
        );
        // And a body under the override is kept whole.
        let small = RawIoRecord::new(1, 0, None, None, None, None, b"hi", b"hi", 32);
        assert_eq!(small.request_body, "hi");
        assert_eq!(small.response_body, "hi");
    }

    #[test]
    fn streamed_body_under_cap_is_kept_whole_no_marker() {
        // total == kept.len() ⇒ nothing dropped ⇒ no marker, body verbatim.
        let kept = b"event: message_start\n\n";
        let record = RawIoRecord::new_streamed(
            1,
            0,
            None,
            None,
            None,
            Some(200),
            b"{}",
            kept,
            kept.len(),
            RESPONSE_CAP_BYTES,
        );
        assert_eq!(record.response_body, String::from_utf8_lossy(kept));
        assert!(!record.response_body.contains("truncated"));
    }

    #[test]
    fn streamed_body_over_cap_marks_dropped_count_from_total() {
        // The relay retained only 10 bytes of a 1000-byte stream → the marker
        // must report the 990 bytes it dropped, which the bounded prefix alone
        // cannot reveal. This is the streamed truncation path.
        let kept = b"0123456789"; // 10 bytes retained
        let total = 1000usize; // 1000 bytes actually streamed
        let record = RawIoRecord::new_streamed(
            1,
            0,
            None,
            None,
            None,
            Some(200),
            b"{}",
            kept,
            total,
            10, // cap (matches what the relay retained)
        );
        assert_eq!(
            record.response_body, "0123456789…[truncated 990 bytes]",
            "kept prefix + accurate dropped count from the relay total"
        );
    }

    #[test]
    fn prune_keeps_recent_drops_old() {
        let path = tmp_path("prune");
        let now = 100 * MS_PER_DAY; // day 100
                                    // Old: day 1; recent: day 99. Retention 90 days → cutoff = day 10.
        append(Some(&path), &rec(1, MS_PER_DAY));
        append(Some(&path), &rec(2, 99 * MS_PER_DAY));

        prune(Some(&path), 90, now);

        let contents = std::fs::read_to_string(&path).expect("file kept");
        let ids: Vec<u64> = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<RawIoRecord>(l).ok())
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![2], "only the in-window record survives");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_with_zero_retention_keeps_all() {
        let path = tmp_path("prune-zero");
        append(Some(&path), &rec(1, 1)); // ancient
        append(Some(&path), &rec(2, 2));
        let before = std::fs::read_to_string(&path).expect("file");

        prune(Some(&path), 0, u64::MAX);

        let after = std::fs::read_to_string(&path).expect("file");
        assert_eq!(before, after, "retention_days == 0 is a no-op");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_tolerates_corrupt_lines_and_keeps_valid_recent_ones() {
        let path = tmp_path("prune-corrupt");
        let now = 100 * MS_PER_DAY;
        // A recent valid record, a corrupt line, and an old valid record.
        append(Some(&path), &rec(2, 99 * MS_PER_DAY));
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            writeln!(f, "not json at all {{").expect("write");
        }
        append(Some(&path), &rec(1, MS_PER_DAY));

        prune(Some(&path), 90, now);

        let contents = std::fs::read_to_string(&path).expect("file");
        let ids: Vec<u64> = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<RawIoRecord>(l).ok())
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![2], "corrupt + old dropped, recent kept");
        assert!(
            !contents.contains("not json"),
            "corrupt line shed on rewrite"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_missing_file_is_a_noop() {
        let path = tmp_path("prune-missing");
        // Never created.
        prune(Some(&path), 90, now_ms());
        assert!(!path.exists(), "prune does not create the file");
    }

    #[test]
    fn append_with_none_path_writes_nothing_and_never_panics() {
        // No path = disabled / no state dir → silent no-op.
        append(None, &rec(1, 1));
    }

    #[test]
    fn prune_with_none_path_is_a_noop() {
        prune(None, 90, now_ms());
    }
}
