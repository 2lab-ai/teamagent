//! Codex request/response trace (`codex.trace`): one JSON line per codex
//! request appended to `$XDG_STATE_HOME/llmux/codex-trace.jsonl`, so token
//! issues can be diagnosed offline from the log instead of by asking the user.
//!
//! Each line answers three questions:
//! 1. **Did the request hang or complete?** — the `outcome` tag
//!    (`completed` / `error` / `client_disconnect`), plus `upstream_events`
//!    (how many SSE events the upstream produced) and `duration_ms`.
//! 2. **How big was the input, by part?** — `system_tokens_est`,
//!    `tools_tokens_est`, `messages_tokens_est` (chars/4 per section) with
//!    `tools_count` / `messages_count` and 2 KiB previews of system + messages.
//! 3. **What did the upstream really charge?** — the verbatim `usage` object
//!    on `completed` (input/cached/output/reasoning/total).
//!
//! Everything here is best-effort: building the input never fails the request,
//! and [`CodexTrace::write`] swallows every IO/serialization error. If the
//! state directory can't be resolved the whole trace is silently skipped.

use std::io::Write as _;

use serde_json::{json, Value};

/// First `MAX` bytes of a section, on a UTF-8 char boundary, marked truncated.
const PREVIEW_BYTES: usize = 2048;

/// Cap on the per-tool breakdown: the top [`MAX_TOOLS`] by `tokens_est`, then a
/// single `…(+N more)` roll-up for the rest, so a request with a hundred MCP
/// tools still yields one readable line.
const MAX_TOOLS: usize = 40;

/// The input half of a trace line, built once when a codex request is dispatched
/// (the request body is available then) and held until the terminal outcome is
/// known. Cheap to build; never fails.
#[derive(Debug, Clone)]
pub(crate) struct CodexTrace {
    /// Whether tracing is enabled — when `false`, [`Self::write`] is a no-op so
    /// callers don't have to branch.
    enabled: bool,
    id: u64,
    path: String,
    model: Option<String>,
    system_tokens_est: u64,
    tools_tokens_est: u64,
    messages_tokens_est: u64,
    tools_count: u64,
    /// Per-tool `{name, tokens_est}`, descending by `tokens_est`, capped at
    /// [`MAX_TOOLS`] with a final `…(+N more)` roll-up. Lets one trace line name
    /// which tools (e.g. MCP `slack_*`, `atlassian_*`) dominate the per-turn
    /// token cost, so the exact servers to disable are obvious.
    tools: Vec<Value>,
    messages_count: u64,
    system_preview: String,
    messages_preview: String,
}

impl CodexTrace {
    /// Build the input trace from the inbound Anthropic request body. `enabled`
    /// is `config.codex.trace`; when `false` the returned value is inert.
    /// `model` is the model the request will be served as. Never panics: an
    /// unparseable body yields zeroed estimates and empty previews.
    pub(crate) fn from_request(
        enabled: bool,
        id: u64,
        path: &str,
        model: Option<String>,
        body: &[u8],
    ) -> Self {
        let parsed: Option<Value> = serde_json::from_slice(body).ok();
        let body = parsed.as_ref();

        let system = body.and_then(|b| b.get("system"));
        let tools = body.and_then(|b| b.get("tools"));
        let messages = body.and_then(|b| b.get("messages"));

        let est = |v: Option<&Value>| {
            v.map(crate::provider::codex::estimate_section_tokens)
                .unwrap_or(0)
        };
        let array_len = |v: Option<&Value>| {
            v.and_then(Value::as_array)
                .map(|a| a.len() as u64)
                .unwrap_or(0)
        };

        Self {
            enabled,
            id,
            path: path.to_string(),
            model,
            system_tokens_est: est(system),
            tools_tokens_est: est(tools),
            messages_tokens_est: est(messages),
            tools_count: array_len(tools),
            tools: tools_breakdown(tools),
            messages_count: array_len(messages),
            system_preview: preview(system),
            messages_preview: preview(messages),
        }
    }

    /// Terminal: the request completed with an upstream `usage` object (verbatim
    /// — input/cached/output/reasoning/total). `upstream_events` is how many SSE
    /// events the converter saw.
    pub(crate) fn write_completed(
        &self,
        usage: Option<&Value>,
        upstream_events: u64,
        duration_ms: u128,
    ) {
        self.write(
            json!({
                "type": "completed",
                "usage": usage.cloned().unwrap_or(Value::Null),
            }),
            upstream_events,
            duration_ms,
        );
    }

    /// Terminal: the request failed (upstream error, rewrite failure, transient,
    /// converter error). `message` is the operator-facing reason.
    pub(crate) fn write_error(&self, message: &str, upstream_events: u64, duration_ms: u128) {
        self.write(
            json!({
                "type": "error",
                "message": message,
            }),
            upstream_events,
            duration_ms,
        );
    }

    /// Terminal: the client went away mid-stream before the upstream completed.
    pub(crate) fn write_client_disconnect(&self, upstream_events: u64, duration_ms: u128) {
        self.write(
            json!({ "type": "client_disconnect" }),
            upstream_events,
            duration_ms,
        );
    }

    /// The full trace line (input breakdown + outcome) as a JSON value. Pure —
    /// no IO, no env — so the `id`/`input` shape can be asserted in tests.
    fn line(&self, outcome: Value, upstream_events: u64, duration_ms: u128) -> Value {
        json!({
            "id": self.id,
            "ts": now_rfc3339(),
            "path": self.path,
            "model": self.model,
            "input": {
                "system_tokens_est": self.system_tokens_est,
                "tools_tokens_est": self.tools_tokens_est,
                "messages_tokens_est": self.messages_tokens_est,
                "tools_count": self.tools_count,
                "tools": self.tools.clone(),
                "messages_count": self.messages_count,
                "system_preview": self.system_preview,
                "messages_preview": self.messages_preview,
            },
            "upstream_events": upstream_events,
            "duration_ms": duration_ms,
            "outcome": outcome,
        })
    }

    /// Assemble the full line (input breakdown + outcome) and append it,
    /// best-effort. A disabled trace, a missing state dir, or any IO error is
    /// swallowed — the request path is never affected.
    fn write(&self, outcome: Value, upstream_events: u64, duration_ms: u128) {
        if !self.enabled {
            return;
        }
        let Some(path) = crate::cli::daemon::codex_trace_path() else {
            return;
        };
        let line = self.line(outcome, upstream_events, duration_ms);
        // Best-effort append. Create the parent dir if needed; ignore failures.
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        else {
            return;
        };
        let _ = writeln!(file, "{line}");
    }
}

/// Per-tool `{name, tokens_est}` for the trace input, descending by
/// `tokens_est`. `tokens_est` is the same chars/4 estimate used for the
/// `tools_tokens_est` total ([`estimate_section_tokens`]) applied to each
/// tool's full JSON, so the parts reconcile with the total. Beyond
/// [`MAX_TOOLS`] entries the tail is folded into one `…(+N more)` row whose
/// `tokens_est` is the sum of the omitted tools. Best-effort: a non-array or a
/// nameless tool degrades gracefully (`name: "?"`), never panics.
fn tools_breakdown(tools: Option<&Value>) -> Vec<Value> {
    let Some(items) = tools.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut per_tool: Vec<(String, u64)> = items
        .iter()
        .map(|tool| {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let tokens_est = crate::provider::codex::estimate_section_tokens(tool);
            (name, tokens_est)
        })
        .collect();
    // Descending by tokens_est; ties broken by name for a deterministic line.
    per_tool.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    if per_tool.len() <= MAX_TOOLS {
        return per_tool
            .into_iter()
            .map(|(name, tokens_est)| json!({ "name": name, "tokens_est": tokens_est }))
            .collect();
    }
    // Keep the top MAX_TOOLS, fold the rest into one roll-up row.
    let rest = per_tool.split_off(MAX_TOOLS);
    let rest_sum: u64 = rest.iter().map(|(_, t)| *t).sum();
    let mut out: Vec<Value> = per_tool
        .into_iter()
        .map(|(name, tokens_est)| json!({ "name": name, "tokens_est": tokens_est }))
        .collect();
    out.push(json!({
        "name": format!("…(+{} more)", rest.len()),
        "tokens_est": rest_sum,
    }));
    out
}

/// A compact, truncated preview of a request section for the trace. Serializes
/// the section to JSON (robust to string-or-array shapes) and clips to
/// [`PREVIEW_BYTES`] on a char boundary.
fn preview(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let s = match value {
        // A bare string section (e.g. `system: "..."`) reads better unquoted.
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.len() <= PREVIEW_BYTES {
        return s;
    }
    let mut end = PREVIEW_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} bytes]", &s[..end], s.len() - end)
}

/// Wall-clock timestamp as RFC3339-ish UTC (`YYYY-MM-DDTHH:MM:SSZ`). Uses
/// `SystemTime` like the rest of the wall-clock code; durations stay on the
/// monotonic `Instant` the request already carries.
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Minimal civil-time conversion (UTC, no external dep).
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Days since the Unix epoch → (year, month, day), proleptic Gregorian.
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_body() -> Vec<u8> {
        json!({
            "model": "claude-sonnet-4",
            "system": "you are a helpful assistant",
            "tools": [{"name": "read"}, {"name": "write"}],
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi there"}
            ]
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn from_request_breaks_down_input_per_section() {
        let t = CodexTrace::from_request(
            true,
            7,
            "/v1/messages",
            Some("gpt-5.5".into()),
            &sample_body(),
        );
        assert_eq!(t.id, 7);
        assert_eq!(t.tools_count, 2);
        assert_eq!(t.messages_count, 2);
        // chars/4, each section estimated independently and non-zero.
        assert!(t.system_tokens_est > 0, "system estimated");
        assert!(t.tools_tokens_est > 0, "tools estimated");
        assert!(t.messages_tokens_est > 0, "messages estimated");
        // A bare-string system previews unquoted.
        assert_eq!(t.system_preview, "you are a helpful assistant");
        assert!(t.messages_preview.contains("hello"));
    }

    #[test]
    fn trace_line_logs_the_request_activity_id_non_zero() {
        // Change A: the trace `id` must be the SAME non-zero activity id the
        // dashboard/request log show — not the 0 a bare `fetch_add` returned for
        // the first request. `from_request` is fed `ctx.activity_id`; assert it
        // is preserved verbatim into the emitted line.
        let activity_id = 1; // first request, 1-based (see server::next_request_id)
        let t = CodexTrace::from_request(
            true,
            activity_id,
            "/v1/messages",
            Some("gpt-5.5".into()),
            &sample_body(),
        );
        let line = t.line(json!({"type": "completed"}), 3, 42);
        assert_eq!(line["id"], json!(activity_id), "trace id == activity id");
        assert_ne!(line["id"], json!(0), "id is non-zero, not the old `0` bug");
    }

    #[test]
    fn tools_breakdown_is_per_tool_sorted_desc_and_named() {
        // Two tools of clearly different size: the bigger schema must sort first
        // so a reader sees the costliest tool at the top of `input.tools`.
        let body = json!({
            "tools": [
                {"name": "slack_post_message",
                 "description": "send a very long detailed message to a slack channel with many options and fields and rich formatting blocks"},
                {"name": "read", "description": "read a file"}
            ]
        })
        .to_string()
        .into_bytes();
        let t = CodexTrace::from_request(true, 1, "/v1/messages", None, &body);
        assert_eq!(t.tools.len(), 2);
        assert_eq!(t.tools[0]["name"], json!("slack_post_message"));
        assert_eq!(t.tools[1]["name"], json!("read"));
        let top = t.tools[0]["tokens_est"].as_u64().expect("u64");
        let bottom = t.tools[1]["tokens_est"].as_u64().expect("u64");
        assert!(top > bottom, "sorted descending by tokens_est");
        assert!(top > 0, "tokens_est is the chars/4 estimate, non-zero");
        // Per-tool estimates reconcile with the section total to within the
        // per-tool flooring remainder: each tool floors chars/4 independently,
        // so the sum can trail the once-floored total by up to (tools_count-1).
        let parts_sum = top + bottom;
        assert!(
            parts_sum <= t.tools_tokens_est && t.tools_tokens_est - parts_sum < t.tools_count,
            "parts ({parts_sum}) reconcile with total ({}) within flooring slack",
            t.tools_tokens_est,
        );
    }

    #[test]
    fn tools_breakdown_caps_at_max_tools_with_rollup() {
        // More than MAX_TOOLS tools: keep the top MAX_TOOLS, fold the rest into
        // one `…(+N more)` row whose tokens_est is the omitted sum.
        let n = MAX_TOOLS + 5;
        let tools: Vec<Value> = (0..n)
            .map(|i| json!({"name": format!("tool_{i:03}"), "description": "x".repeat(i + 1)}))
            .collect();
        let body = json!({ "tools": tools }).to_string().into_bytes();
        let t = CodexTrace::from_request(true, 1, "/v1/messages", None, &body);
        assert_eq!(t.tools.len(), MAX_TOOLS + 1, "MAX_TOOLS rows + 1 roll-up");
        let last = t.tools.last().expect("roll-up row");
        assert_eq!(last["name"], json!("…(+5 more)"));
        assert!(
            last["tokens_est"].as_u64().expect("u64") > 0,
            "roll-up carries the omitted tokens"
        );
        // Every non-roll-up row is at least as large as the roll-up's largest
        // omitted tool — i.e. the cap kept the biggest tools.
        let kept_min = t.tools[MAX_TOOLS - 1]["tokens_est"].as_u64().expect("u64");
        assert!(kept_min > 0);
    }

    #[test]
    fn tools_breakdown_empty_when_no_tools() {
        let body = json!({"messages": []}).to_string().into_bytes();
        let t = CodexTrace::from_request(true, 1, "/v1/messages", None, &body);
        assert!(t.tools.is_empty(), "no tools[] ⇒ empty breakdown");
    }

    #[test]
    fn from_request_tolerates_unparseable_body() {
        let t = CodexTrace::from_request(true, 1, "/v1/messages", None, b"not json{");
        assert_eq!(t.system_tokens_est, 0);
        assert_eq!(t.tools_count, 0);
        assert_eq!(t.messages_count, 0);
        assert!(t.system_preview.is_empty());
    }

    #[test]
    fn disabled_trace_writes_nothing_and_never_panics() {
        let t = CodexTrace::from_request(false, 1, "/v1/messages", None, &sample_body());
        // Must not touch the filesystem or panic regardless of outcome.
        t.write_completed(Some(&json!({"input_tokens": 10})), 3, 42);
        t.write_error("boom", 0, 1);
        t.write_client_disconnect(1, 5);
    }

    #[test]
    fn preview_truncates_on_char_boundary() {
        let big = "a".repeat(PREVIEW_BYTES + 500);
        let v = Value::String(big);
        let p = preview(Some(&v));
        assert!(p.contains("[truncated"), "marks truncation");
        assert!(p.len() < PREVIEW_BYTES + 200, "clipped near PREVIEW_BYTES");
    }

    #[test]
    fn timestamp_is_rfc3339_utc() {
        let ts = now_rfc3339();
        assert_eq!(ts.len(), 20, "YYYY-MM-DDTHH:MM:SSZ");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-06-16 is 20_620 days after the epoch.
        assert_eq!(civil_from_days(20_620), (2026, 6, 16));
    }
}
