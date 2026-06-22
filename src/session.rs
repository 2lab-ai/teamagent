//! Session grouping (issue #34): fold persisted [`RawIoRecord`]s into a
//! confidence-labeled session timeline keyed by the request body's
//! `metadata.user_id`.
//!
//! # Why `metadata.user_id`
//!
//! A 2026-06-18 capture found `metadata.user_id` present in ~98.9% of persisted
//! `raw-io.jsonl` records. It is account-independent — one user_id spans the 2–3
//! upstream accounts llmux rotates through — and is a stable session/
//! conversation-grained key already on disk. That makes it the natural grouping
//! key for an offline session timeline.
//!
//! # Metadata only — never raw prompt content
//!
//! Per `.prd/10-model-usage-dashboard.md:141` ("Avoid raw request content"), the
//! fold extracts ONLY metadata from each record: the `user_id` grouping key, the
//! served model, the account, the timestamp, and the token counters from the
//! response usage object. The verbatim `request_body` / `response_body` strings
//! are parsed for those fields and then dropped — no prompt text is retained,
//! surfaced, or persisted by anything in this module.
//!
//! # Pure
//!
//! [`fold_sessions`] is a pure function over a slice of records: no IO, no clock,
//! no globals. The caller (the TUI Sessions overlay) reads the persisted file and
//! hands the parsed records in; tests feed synthetic records directly. This keeps
//! the aggregation independent of rendering and of a real file on disk.

use std::collections::BTreeMap;

use crate::proxy::raw_io::RawIoRecord;

/// How confidently a group of records is attributed to one session.
///
/// The grouping key is the request body's `metadata.user_id`. A record that
/// carries an explicit `user_id` can be grouped with certainty; a record with no
/// `user_id` (the ~1% the capture found) cannot be attributed to any session, so
/// it lands in a single best-effort `ungrouped` bucket flagged [`Self::Low`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Every record in the group carried an explicit `metadata.user_id` — the
    /// grouping key is fully present, so the session boundary is certain.
    High,
    /// The catch-all bucket of records with no `metadata.user_id`. These cannot
    /// be confidently attributed to a session; they are kept together only so the
    /// timeline accounts for every record.
    Low,
}

impl Confidence {
    /// Short label for the UI (a session row tag).
    pub fn label(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Low => "low",
        }
    }
}

/// Per-session aggregate folded from the records sharing one `metadata.user_id`
/// (or the single `ungrouped` bucket). Metadata only — no prompt content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The grouping key: the request body's `metadata.user_id`, or `None` for the
    /// catch-all bucket of records that had no user_id.
    pub user_id: Option<String>,
    /// Number of proxied requests folded into this session.
    pub requests: u64,
    /// Summed response `usage.input_tokens` across the session's records.
    pub tokens_in: u64,
    /// Summed response `usage.output_tokens` across the session's records.
    pub tokens_out: u64,
    /// Distinct served models seen, sorted, deduplicated.
    pub models: Vec<String>,
    /// Distinct accounts that served records in this session, sorted, deduped.
    /// A session spanning more than one account is the rotation signal.
    pub accounts: Vec<String>,
    /// Number of times the serving account *changed* between consecutive records
    /// (ordered by timestamp). 0 = one account the whole time. This is the
    /// account-rotation count the issue asks for — distinct from `accounts.len()`
    /// because llmux can rotate A→B→A (2 rotations, 2 distinct accounts).
    pub account_rotations: u64,
    /// Earliest record timestamp (ms since epoch) in the session.
    pub first_ms: u64,
    /// Latest record timestamp (ms since epoch) in the session.
    pub last_ms: u64,
    /// Grouping confidence for this session row.
    pub confidence: Confidence,
}

impl Session {
    /// Wall-clock span of the session in milliseconds (`last_ms - first_ms`).
    pub fn span_ms(&self) -> u64 {
        self.last_ms.saturating_sub(self.first_ms)
    }
}

/// Extract the request body's `metadata.user_id`, if present.
///
/// Reuses the body-JSON parse approach `routing::model_from_body` uses for the
/// `model` field: parse the request body as JSON and read a nested string field.
/// A non-JSON body, a missing `metadata`, or a non-string `user_id` yields
/// `None` — the record then lands in the best-effort `ungrouped` bucket.
pub fn user_id_from_request_body(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("metadata")?
        .get("user_id")?
        .as_str()
        .map(str::to_string)
}

/// Extract `(input_tokens, output_tokens)` from a persisted response body.
///
/// The non-streaming Anthropic Messages JSON carries a top-level `usage` object
/// (`src/proxy/sse.rs` reads the same shape for the live path). A missing field
/// counts as 0 so a partial/streamed body still folds without error; a non-JSON
/// body yields `(0, 0)`. Metadata only — the body text itself is never retained.
fn tokens_from_response_body(body: &str) -> (u64, u64) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return (0, 0);
    };
    let Some(usage) = value.get("usage") else {
        return (0, 0);
    };
    let input = usage.get("input_tokens").and_then(|v| v.as_u64());
    let output = usage.get("output_tokens").and_then(|v| v.as_u64());
    (input.unwrap_or(0), output.unwrap_or(0))
}

/// Mutable accumulator while folding; finalized into a [`Session`].
struct Acc {
    user_id: Option<String>,
    requests: u64,
    tokens_in: u64,
    tokens_out: u64,
    models: std::collections::BTreeSet<String>,
    accounts: std::collections::BTreeSet<String>,
    account_rotations: u64,
    first_ms: u64,
    last_ms: u64,
    /// The account of the chronologically last record folded so far, to detect a
    /// change on the next record. `None` until a record with a known account is
    /// seen.
    prev_account: Option<String>,
    /// Whether any record in this group lacked a `user_id` (forces `Low`).
    any_missing_user_id: bool,
}

impl Acc {
    fn new(user_id: Option<String>, ts_ms: u64) -> Self {
        Self {
            user_id,
            requests: 0,
            tokens_in: 0,
            tokens_out: 0,
            models: std::collections::BTreeSet::new(),
            accounts: std::collections::BTreeSet::new(),
            account_rotations: 0,
            first_ms: ts_ms,
            last_ms: ts_ms,
            prev_account: None,
            any_missing_user_id: false,
        }
    }

    fn into_session(self) -> Session {
        // High only when the group is keyed by an explicit user_id AND no record
        // in it was missing one; the ungrouped bucket (and any group that somehow
        // mixed in a missing key) is Low.
        let confidence = if self.user_id.is_some() && !self.any_missing_user_id {
            Confidence::High
        } else {
            Confidence::Low
        };
        Session {
            user_id: self.user_id,
            requests: self.requests,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            models: self.models.into_iter().collect(),
            accounts: self.accounts.into_iter().collect(),
            account_rotations: self.account_rotations,
            first_ms: self.first_ms,
            last_ms: self.last_ms,
            confidence,
        }
    }
}

/// Fold persisted raw-io records into per-`user_id` sessions.
///
/// Records are grouped by `metadata.user_id` (extracted from each
/// `request_body`); records with no user_id are collected into a single
/// `ungrouped` bucket keyed `None` and flagged [`Confidence::Low`]. Within a
/// group the records are processed in timestamp order so `account_rotations`
/// (consecutive serving-account changes) and the `first_ms`/`last_ms` span are
/// correct regardless of input order.
///
/// The returned vector is sorted by `last_ms` descending (most recent session
/// first), with the `ungrouped` bucket — if present — always last so the
/// confident sessions lead the timeline.
///
/// Pure: no IO, no clock, no panics.
pub fn fold_sessions(records: &[RawIoRecord]) -> Vec<Session> {
    // Stable per-key grouping. The BTreeMap key is the user_id (or a sentinel for
    // the ungrouped bucket) so iteration order is deterministic before the final
    // sort.
    let mut groups: BTreeMap<Option<String>, Vec<&RawIoRecord>> = BTreeMap::new();
    for rec in records {
        let key = user_id_from_request_body(&rec.request_body);
        groups.entry(key).or_default().push(rec);
    }

    let mut sessions: Vec<Session> = groups
        .into_iter()
        .map(|(user_id, mut recs)| {
            // Process in timestamp order (then by id) so rotation detection and
            // the span are independent of file/append order.
            recs.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms).then(a.id.cmp(&b.id)));
            let first_ts = recs.first().map(|r| r.ts_ms).unwrap_or(0);
            let mut acc = Acc::new(user_id, first_ts);
            for rec in recs {
                acc.requests = acc.requests.saturating_add(1);
                let (tin, tout) = tokens_from_response_body(&rec.response_body);
                acc.tokens_in = acc.tokens_in.saturating_add(tin);
                acc.tokens_out = acc.tokens_out.saturating_add(tout);
                if let Some(model) = &rec.model {
                    acc.models.insert(model.clone());
                }
                if let Some(account) = &rec.account {
                    acc.accounts.insert(account.clone());
                    // A rotation is a change from the previous record's account.
                    if acc
                        .prev_account
                        .as_ref()
                        .is_some_and(|prev| prev != account)
                    {
                        acc.account_rotations = acc.account_rotations.saturating_add(1);
                    }
                    acc.prev_account = Some(account.clone());
                }
                acc.first_ms = acc.first_ms.min(rec.ts_ms);
                acc.last_ms = acc.last_ms.max(rec.ts_ms);
                if user_id_from_request_body(&rec.request_body).is_none() {
                    acc.any_missing_user_id = true;
                }
            }
            acc.into_session()
        })
        .collect();

    // Most-recent session first; the ungrouped (None) bucket always sinks to the
    // bottom so the confident rows lead.
    sessions.sort_by(|a, b| match (a.user_id.is_none(), b.user_id.is_none()) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => b.last_ms.cmp(&a.last_ms).then(a.user_id.cmp(&b.user_id)),
    });
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::raw_io::{RawIoRecord, RECORD_VERSION, RESPONSE_CAP_BYTES};

    /// Build a synthetic raw-io record. `user_id == None` means the request body
    /// carries no `metadata.user_id` (the ~1% the capture found).
    fn record(
        id: u64,
        ts_ms: u64,
        user_id: Option<&str>,
        model: &str,
        account: &str,
        tokens_in: u64,
        tokens_out: u64,
    ) -> RawIoRecord {
        let request_body = match user_id {
            Some(uid) => {
                format!(r#"{{"model":"{model}","metadata":{{"user_id":"{uid}"}},"messages":[]}}"#)
            }
            None => format!(r#"{{"model":"{model}","messages":[]}}"#),
        };
        let response_body = format!(
            r#"{{"id":"msg_{id}","usage":{{"input_tokens":{tokens_in},"output_tokens":{tokens_out}}}}}"#
        );
        RawIoRecord {
            v: RECORD_VERSION,
            ts_ms,
            id,
            group: Some("claude".into()),
            model: Some(model.into()),
            account: Some(account.into()),
            status: Some(200),
            request_body,
            response_body,
        }
    }

    #[test]
    fn user_id_extracted_from_metadata_in_request_body() {
        let body = r#"{"model":"claude","metadata":{"user_id":"u-abc"}}"#;
        assert_eq!(user_id_from_request_body(body).as_deref(), Some("u-abc"));
    }

    #[test]
    fn missing_or_non_json_user_id_is_none() {
        assert_eq!(user_id_from_request_body(r#"{"model":"claude"}"#), None);
        assert_eq!(
            user_id_from_request_body(r#"{"metadata":{"other":"x"}}"#),
            None
        );
        assert_eq!(user_id_from_request_body("not json at all {"), None);
        // Non-string user_id is rejected (not coerced).
        assert_eq!(
            user_id_from_request_body(r#"{"metadata":{"user_id":42}}"#),
            None
        );
    }

    #[test]
    fn tokens_parsed_from_response_usage_with_missing_treated_as_zero() {
        assert_eq!(
            tokens_from_response_body(r#"{"usage":{"input_tokens":10,"output_tokens":3}}"#),
            (10, 3)
        );
        // Missing output → 0, not an error.
        assert_eq!(
            tokens_from_response_body(r#"{"usage":{"input_tokens":7}}"#),
            (7, 0)
        );
        // No usage / non-JSON → (0, 0).
        assert_eq!(tokens_from_response_body(r#"{"id":"x"}"#), (0, 0));
        assert_eq!(tokens_from_response_body("garbage"), (0, 0));
    }

    /// The acceptance test (issue #34): feed synthetic records — several
    /// user_ids, one session spanning multiple accounts with a real A→B→A
    /// rotation, and ~1% with no user_id — to the fold function and assert every
    /// per-session aggregate (counts, token sums, account-rotation count, models,
    /// span) and the confidence label is correct.
    #[test]
    fn fold_groups_by_user_id_with_correct_aggregates_and_confidence() {
        let records = vec![
            // Session u-1: 3 requests, accounts rotate acct-a → acct-b → acct-a
            // (2 rotations, 2 distinct accounts), two models, span 100..300.
            record(1, 100, Some("u-1"), "claude-sonnet-4", "acct-a", 10, 5),
            record(2, 200, Some("u-1"), "claude-opus-4", "acct-b", 20, 7),
            record(3, 300, Some("u-1"), "claude-sonnet-4", "acct-a", 5, 1),
            // Session u-2: 2 requests, single account (no rotation), span 150..250.
            record(4, 150, Some("u-2"), "claude-sonnet-4", "acct-a", 100, 40),
            record(5, 250, Some("u-2"), "claude-sonnet-4", "acct-a", 50, 20),
            // ~1%: one record with no user_id → the ungrouped Low bucket.
            record(6, 500, None, "claude-sonnet-4", "acct-c", 1, 1),
        ];

        let sessions = fold_sessions(&records);
        assert_eq!(sessions.len(), 3, "u-1, u-2, and the ungrouped bucket");

        // The ungrouped bucket always sinks to the bottom.
        let ungrouped = sessions.last().expect("ungrouped present");
        assert_eq!(ungrouped.user_id, None);
        assert_eq!(ungrouped.confidence, Confidence::Low);
        assert_eq!(ungrouped.requests, 1);
        assert_eq!(ungrouped.account_rotations, 0);
        assert_eq!(ungrouped.accounts, vec!["acct-c".to_string()]);

        let by_id = |uid: &str| {
            sessions
                .iter()
                .find(|s| s.user_id.as_deref() == Some(uid))
                .unwrap_or_else(|| panic!("session {uid} present"))
        };

        let s1 = by_id("u-1");
        assert_eq!(s1.confidence, Confidence::High);
        assert_eq!(s1.requests, 3);
        assert_eq!(s1.tokens_in, 35, "10 + 20 + 5");
        assert_eq!(s1.tokens_out, 13, "5 + 7 + 1");
        assert_eq!(
            s1.models,
            vec!["claude-opus-4".to_string(), "claude-sonnet-4".to_string()],
            "distinct models, sorted"
        );
        assert_eq!(
            s1.accounts,
            vec!["acct-a".to_string(), "acct-b".to_string()],
            "two distinct accounts"
        );
        assert_eq!(
            s1.account_rotations, 2,
            "a→b is one rotation, b→a is a second"
        );
        assert_eq!(s1.first_ms, 100);
        assert_eq!(s1.last_ms, 300);
        assert_eq!(s1.span_ms(), 200);

        let s2 = by_id("u-2");
        assert_eq!(s2.confidence, Confidence::High);
        assert_eq!(s2.requests, 2);
        assert_eq!(s2.tokens_in, 150);
        assert_eq!(s2.tokens_out, 60);
        assert_eq!(s2.models, vec!["claude-sonnet-4".to_string()]);
        assert_eq!(s2.accounts, vec!["acct-a".to_string()]);
        assert_eq!(s2.account_rotations, 0, "one account the whole session");
        assert_eq!(s2.span_ms(), 100);
    }

    #[test]
    fn rotation_count_is_independent_of_input_order() {
        // Same A→B→A session, but the records arrive out of timestamp order. The
        // fold must sort by ts before counting rotations, so the answer is still
        // 2 (not an artifact of append order).
        let records = vec![
            record(3, 300, Some("u-1"), "m", "acct-a", 0, 0),
            record(1, 100, Some("u-1"), "m", "acct-a", 0, 0),
            record(2, 200, Some("u-1"), "m", "acct-b", 0, 0),
        ];
        let sessions = fold_sessions(&records);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].account_rotations, 2);
        assert_eq!(sessions[0].first_ms, 100);
        assert_eq!(sessions[0].last_ms, 300);
    }

    #[test]
    fn empty_input_folds_to_no_sessions() {
        assert!(fold_sessions(&[]).is_empty());
    }

    #[test]
    fn sessions_sorted_most_recent_first_ungrouped_last() {
        let records = vec![
            record(1, 100, Some("u-old"), "m", "acct-a", 0, 0),
            record(2, 900, Some("u-new"), "m", "acct-a", 0, 0),
            record(3, 999, None, "m", "acct-a", 0, 0), // newest, but ungrouped
        ];
        let sessions = fold_sessions(&records);
        assert_eq!(sessions[0].user_id.as_deref(), Some("u-new"));
        assert_eq!(sessions[1].user_id.as_deref(), Some("u-old"));
        assert_eq!(
            sessions[2].user_id, None,
            "ungrouped sinks below confident rows even though it is newest"
        );
    }

    #[test]
    fn truncated_response_body_folds_without_error_tokens_zero() {
        // A streamed/truncated response (the raw-io truncation marker) is not
        // valid JSON → tokens fold as 0, the record still counts.
        let mut rec = record(1, 100, Some("u-1"), "m", "acct-a", 0, 0);
        rec.response_body = "event: message_start…[truncated 990 bytes]".to_string();
        let sessions = fold_sessions(std::slice::from_ref(&rec));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].requests, 1);
        assert_eq!(sessions[0].tokens_in, 0);
        assert_eq!(sessions[0].tokens_out, 0);
        // Cap constant is in scope (silences unused-import on the test module).
        let _ = RESPONSE_CAP_BYTES;
    }
}
