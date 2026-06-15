//! Activity log state: in-flight requests (spinner rows), a bounded ring
//! buffer of completed entries (newest first), and per-account totals.
//! Pure state — rendering lives in `ui`, timestamps are passed in.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, SystemTime};

use super::event::{ActivityEvent, TokenCounts};

/// Completed-entry ring capacity (matches teamclaude's 200-line log).
pub(crate) const LOG_CAPACITY: usize = 200;
/// In-flight rows are bounded too: if the proxy never sends a finish (bug or
/// dropped event), the oldest in-flight entry is retired as an error note
/// instead of leaking forever.
const MAX_IN_FLIGHT: usize = 64;
/// Age after which an in-flight row is presumed finished and swept, even if no
/// `RequestFinished` event ever arrived (the event was dropped on a full
/// activity channel). Real requests finish in well under 90s per the daemon
/// logs, so 300s is a wide safety margin that never retires a live request but
/// still bounds a leaked row's lifetime — instead of growing to 25,000s+.
const STALE_IN_FLIGHT: Duration = Duration::from_secs(300);

/// A request that has started but not finished — rendered with a spinner.
/// `group`/`model` are filled at routing time so the dashboard can attribute
/// in-flight requests to a model row before they complete (req11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InFlight {
    pub id: u64,
    pub method: String,
    pub path: String,
    pub account: Option<String>,
    pub group: Option<String>,
    pub model: Option<String>,
    pub started_at: SystemTime,
}

/// Body of a completed log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletedBody {
    Request {
        method: String,
        path: String,
        account: Option<String>,
        status: u16,
        duration: Duration,
        tokens: Option<TokenCounts>,
        /// Backend group ("claude"/"codex"), model slug, and reasoning effort
        /// served for this request, when known.
        group: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    },
    Note {
        text: String,
        error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completed {
    pub at: SystemTime,
    pub body: CompletedBody,
}

/// Per-account lifetime counters for the table's totals columns and the
/// global totals pane (ok/error split + in/out token split).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub requests: u64,
    /// Requests that finished with status < 400.
    pub ok: u64,
    /// Requests that finished with status >= 400.
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl Totals {
    /// Combined token count for single-number columns.
    pub(crate) fn tokens(&self) -> u64 {
        self.tokens_in.saturating_add(self.tokens_out)
    }

    fn add(&mut self, other: &Totals) {
        self.requests = self.requests.saturating_add(other.requests);
        self.ok = self.ok.saturating_add(other.ok);
        self.errors = self.errors.saturating_add(other.errors);
        self.tokens_in = self.tokens_in.saturating_add(other.tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(other.tokens_out);
    }
}

// ---------------------------------------------------------------------------
// Model-usage aggregation (req1-20): per (group, served_model) row.
// ---------------------------------------------------------------------------

/// In-memory accumulator for one model row. Folded from completed request
/// events; reset on daemon restart (runtime-only, req26). Cache counters are
/// optional — `None` until an upstream reports the field (req8/9).
#[derive(Debug, Default, Clone)]
struct ModelStats {
    requests: u64,
    ok: u64,
    errors: u64,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    last_used: Option<SystemTime>,
    /// Which account(s) served this model (req19).
    accounts: HashMap<String, Totals>,
    /// Reasoning/effort label → request count (req18); `"none"` when unset.
    efforts: HashMap<String, u64>,
    /// Endpoint class → request count (req20): `messages`/`count_tokens`/other.
    endpoints: HashMap<String, u64>,
}

/// A finished aggregated model row (snapshot of [`ModelStats`]). Timestamps are
/// kept as `SystemTime`; the document builder converts to epoch ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelUsage {
    pub group: String,
    pub model: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: Option<u64>,
    pub cache_creation: Option<u64>,
    pub last_used: SystemTime,
    pub accounts: Vec<ModelAccount>,
    pub efforts: Vec<ModelCount>,
    pub endpoints: Vec<ModelCount>,
}

/// Per-account contribution to one model row (req19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelAccount {
    pub name: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// A labelled request count (effort level or endpoint class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCount {
    pub label: String,
    pub requests: u64,
}

/// Strip a trailing display-only context suffix `…[1m]` so usage is not split
/// by client window hints (req17): `claude-sonnet-4-5[1m]` → `claude-sonnet-4-5`.
pub(crate) fn normalize_model(model: &str) -> String {
    match model.split_once('[') {
        Some((base, _)) => base.trim().to_string(),
        None => model.trim().to_string(),
    }
}

/// Classify a request path into an endpoint bucket for the per-model breakdown
/// (req20). `count_tokens` is checked first because its path also contains
/// `/messages`.
fn endpoint_class(path: &str) -> String {
    let p = path.split('?').next().unwrap_or(path);
    if p.contains("count_tokens") {
        "count_tokens".to_string()
    } else if p.contains("/messages") {
        "messages".to_string()
    } else {
        p.rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("other")
            .to_string()
    }
}

fn sorted_counts(map: &HashMap<String, u64>) -> Vec<ModelCount> {
    let mut counts: Vec<ModelCount> = map
        .iter()
        .map(|(label, &requests)| ModelCount {
            label: label.clone(),
            requests,
        })
        .collect();
    counts.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.label.cmp(&b.label)));
    counts
}

#[derive(Debug, Default)]
pub(crate) struct ActivityLog {
    capacity: usize,
    in_flight: Vec<InFlight>,
    /// Front = newest (the log renders newest-top).
    completed: VecDeque<Completed>,
    totals: HashMap<String, Totals>,
    /// Requests that finished before routing (no account) — kept out of the
    /// per-account map but included in the global totals.
    unrouted: Totals,
    /// Per (group, served_model) usage rows (req1-20). Keyed by the normalized
    /// served model within its backend group.
    models: HashMap<(String, String), ModelStats>,
}

impl ActivityLog {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }

    pub(crate) fn in_flight(&self) -> &[InFlight] {
        &self.in_flight
    }

    /// Sweep in-flight rows older than [`STALE_IN_FLIGHT`]: their
    /// `RequestFinished` event was almost certainly dropped on a full activity
    /// channel (the daemon reports the request as completed while the dashboard
    /// would otherwise show it pinned forever). Each swept row leaves a note so
    /// the cause is visible in the log rather than silently vanishing.
    ///
    /// Called on every dashboard read (`view`) and at the top of `apply` so a
    /// leaked row is bounded even with no further activity. Idempotent and
    /// cheap (a single retain over a ≤64-entry vec).
    pub(crate) fn prune_stale_in_flight(&mut self, now: SystemTime) {
        let mut swept: Vec<InFlight> = Vec::new();
        self.in_flight.retain(|entry| {
            let stale = now
                .duration_since(entry.started_at)
                .map(|age| age >= STALE_IN_FLIGHT)
                .unwrap_or(false);
            if stale {
                swept.push(entry.clone());
            }
            !stale
        });
        for entry in swept {
            self.push_note(
                format!(
                    "{} {} presumed finished (activity event dropped)",
                    entry.method, entry.path
                ),
                true,
                now,
            );
        }
    }

    /// Completed entries, newest first.
    pub(crate) fn completed(&self) -> impl Iterator<Item = &Completed> {
        self.completed.iter()
    }

    /// Per-account totals lookup. The dashboard reads the whole map
    /// ([`Self::totals_map`]) for the document; this single-account accessor
    /// is exercised by the unit tests.
    #[cfg(test)]
    pub(crate) fn totals_for(&self, account: &str) -> Totals {
        self.totals.get(account).copied().unwrap_or_default()
    }

    /// Clone of the per-account totals map (the dashboard document carries
    /// every account's session totals, not just the ones on screen).
    pub(crate) fn totals_map(&self) -> HashMap<String, Totals> {
        self.totals.clone()
    }

    /// Lifetime totals across every account, unrouted failures included.
    pub(crate) fn totals_global(&self) -> Totals {
        let mut sum = self.unrouted;
        for totals in self.totals.values() {
            sum.add(totals);
        }
        sum
    }

    /// Fold one attributed completed request into its `(group, model)` row.
    #[allow(clippy::too_many_arguments)]
    fn record_model(
        &mut self,
        group: &str,
        model: &str,
        account: &Option<String>,
        status: u16,
        tokens: Option<TokenCounts>,
        effort: &Option<String>,
        path: &str,
        now: SystemTime,
    ) {
        let entry = self
            .models
            .entry((group.to_string(), normalize_model(model)))
            .or_default();
        entry.requests += 1;
        if status < 400 {
            entry.ok += 1;
        } else {
            entry.errors += 1;
        }
        if let Some(t) = tokens {
            entry.tokens_in = entry.tokens_in.saturating_add(t.input);
            entry.tokens_out = entry.tokens_out.saturating_add(t.output);
            entry.cache_read = crate::proxy::sse::add_opt(entry.cache_read, t.cache_read);
            entry.cache_creation =
                crate::proxy::sse::add_opt(entry.cache_creation, t.cache_creation);
        }
        entry.last_used = Some(now);
        let effort_label = effort.clone().unwrap_or_else(|| "none".to_string());
        *entry.efforts.entry(effort_label).or_default() += 1;
        *entry.endpoints.entry(endpoint_class(path)).or_default() += 1;
        if let Some(name) = account {
            let at = entry.accounts.entry(name.clone()).or_default();
            at.requests += 1;
            if status < 400 {
                at.ok += 1;
            } else {
                at.errors += 1;
            }
            if let Some(t) = tokens {
                at.tokens_in = at.tokens_in.saturating_add(t.input);
                at.tokens_out = at.tokens_out.saturating_add(t.output);
            }
        }
    }

    /// Snapshot of every model row, sorted by total tokens desc, then requests,
    /// then key (req14). The document builder overlays in-flight counts.
    pub(crate) fn model_usage(&self) -> Vec<ModelUsage> {
        let mut rows: Vec<ModelUsage> = self
            .models
            .iter()
            .map(|((group, model), stats)| {
                let mut accounts: Vec<ModelAccount> = stats
                    .accounts
                    .iter()
                    .map(|(name, t)| ModelAccount {
                        name: name.clone(),
                        requests: t.requests,
                        ok: t.ok,
                        errors: t.errors,
                        tokens_in: t.tokens_in,
                        tokens_out: t.tokens_out,
                    })
                    .collect();
                accounts.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
                ModelUsage {
                    group: group.clone(),
                    model: model.clone(),
                    requests: stats.requests,
                    ok: stats.ok,
                    errors: stats.errors,
                    tokens_in: stats.tokens_in,
                    tokens_out: stats.tokens_out,
                    cache_read: stats.cache_read,
                    cache_creation: stats.cache_creation,
                    last_used: stats.last_used.unwrap_or(SystemTime::UNIX_EPOCH),
                    accounts,
                    efforts: sorted_counts(&stats.efforts),
                    endpoints: sorted_counts(&stats.endpoints),
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            (b.tokens_in + b.tokens_out)
                .cmp(&(a.tokens_in + a.tokens_out))
                .then(b.requests.cmp(&a.requests))
                .then(a.group.cmp(&b.group))
                .then(a.model.cmp(&b.model))
        });
        rows
    }

    /// Completed requests per minute over the trailing `window` (notes
    /// excluded). Bounded by the ring capacity: with the default 200-entry
    /// ring this is exact until ~200 requests land inside the window.
    pub(crate) fn requests_per_minute(&self, now: SystemTime, window: Duration) -> f64 {
        let minutes = window.as_secs_f64() / 60.0;
        if minutes <= 0.0 {
            return 0.0;
        }
        let cutoff = now.checked_sub(window);
        let count = self
            .completed
            .iter()
            .filter(|entry| matches!(entry.body, CompletedBody::Request { .. }))
            .filter(|entry| cutoff.is_none_or(|cutoff| entry.at >= cutoff))
            .count();
        count as f64 / minutes
    }

    /// Fold one proxy event into the log. `now` stamps the resulting entry.
    pub(crate) fn apply(&mut self, event: ActivityEvent, now: SystemTime) {
        // Backstop against a dropped `RequestFinished`: any row older than the
        // stale threshold is presumed finished before we fold the next event.
        self.prune_stale_in_flight(now);
        match event {
            ActivityEvent::RequestStarted { id, method, path } => {
                if self.in_flight.len() >= MAX_IN_FLIGHT {
                    let lost = self.in_flight.remove(0);
                    self.push_note(
                        format!(
                            "{} {} never finished (in-flight overflow)",
                            lost.method, lost.path
                        ),
                        true,
                        now,
                    );
                }
                self.in_flight.push(InFlight {
                    id,
                    method,
                    path,
                    account: None,
                    group: None,
                    model: None,
                    started_at: now,
                });
            }
            ActivityEvent::RequestRouted {
                id,
                account,
                group,
                model,
            } => {
                if let Some(entry) = self.in_flight.iter_mut().find(|r| r.id == id) {
                    entry.account = Some(account);
                    entry.group = group;
                    entry.model = model;
                }
            }
            ActivityEvent::RequestFinished {
                id,
                method,
                path,
                account,
                status,
                duration,
                tokens,
                group,
                model,
                effort,
            } => {
                let routed = self
                    .in_flight
                    .iter()
                    .position(|r| r.id == id)
                    .map(|i| self.in_flight.remove(i))
                    .and_then(|r| r.account);
                let account = account.or(routed);
                let bucket = match &account {
                    Some(name) => self.totals.entry(name.clone()).or_default(),
                    None => &mut self.unrouted,
                };
                bucket.requests += 1;
                if status < 400 {
                    bucket.ok += 1;
                } else {
                    bucket.errors += 1;
                }
                if let Some(tokens) = tokens {
                    bucket.tokens_in += tokens.input;
                    bucket.tokens_out += tokens.output;
                }
                // Model-usage aggregation (req1-20): only when the request was
                // attributed to a (group, model). Pre-routing failures keep
                // group/model None and stay in the global/unrouted accounting
                // above — no bogus model row. A failed-but-attributed request
                // still increments the row's error count even with no tokens.
                if let (Some(group), Some(model)) = (&group, &model) {
                    self.record_model(group, model, &account, status, tokens, &effort, &path, now);
                }
                self.push(Completed {
                    at: now,
                    body: CompletedBody::Request {
                        method,
                        path,
                        account,
                        status,
                        duration,
                        tokens,
                        group,
                        model,
                        effort,
                    },
                });
            }
            ActivityEvent::AccountSwitched { from, to, reason } => {
                let from = from.unwrap_or_else(|| "(none)".into());
                let why = reason.map(|r| format!(" ({r})")).unwrap_or_default();
                self.push_note(format!("switch {from} → {to}{why}"), false, now);
            }
            ActivityEvent::TokenRefreshed {
                account,
                expires_at_ms,
            } => {
                let expiry = std::time::UNIX_EPOCH + Duration::from_millis(expires_at_ms);
                let note = match expiry.duration_since(now) {
                    Ok(left) => format!(
                        "token refreshed: {account} (expires {})",
                        crate::scheduler::select::compact_duration(left)
                    ),
                    // Unknown (0) or already-past expiry: no suffix.
                    Err(_) => format!("token refreshed: {account}"),
                };
                self.push_note(note, false, now);
            }
            // Poller health is tracked by `App` (it feeds the poller pane,
            // not the activity list — one line per poll would drown it).
            ActivityEvent::UsagePolled { .. } => {}
            ActivityEvent::Error { context, message } => {
                let ctx = context.map(|c| format!("{c}: ")).unwrap_or_default();
                self.push_note(format!("{ctx}{message}"), true, now);
            }
        }
    }

    /// Append a TUI-internal note (reload result, switch attempt, …).
    pub(crate) fn push_note(&mut self, text: String, error: bool, now: SystemTime) {
        self.push(Completed {
            at: now,
            body: CompletedBody::Note { text, error },
        });
    }

    fn push(&mut self, entry: Completed) {
        self.completed.push_front(entry);
        self.completed.truncate(self.capacity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn note_text(entry: &Completed) -> &str {
        match &entry.body {
            CompletedBody::Note { text, .. } => text,
            other => panic!("expected note, got {other:?}"),
        }
    }

    fn started(id: u64) -> ActivityEvent {
        ActivityEvent::RequestStarted {
            id,
            method: "POST".into(),
            path: "/v1/messages".into(),
        }
    }

    fn finished(id: u64, account: Option<&str>, tokens: Option<(u64, u64)>) -> ActivityEvent {
        finished_status(id, account, tokens, 200)
    }

    fn finished_status(
        id: u64,
        account: Option<&str>,
        tokens: Option<(u64, u64)>,
        status: u16,
    ) -> ActivityEvent {
        ActivityEvent::RequestFinished {
            id,
            method: "POST".into(),
            path: "/v1/messages".into(),
            account: account.map(str::to_string),
            status,
            duration: Duration::from_millis(1_400),
            tokens: tokens.map(|(input, output)| TokenCounts {
                input,
                output,
                ..Default::default()
            }),
            group: None,
            model: None,
            effort: None,
        }
    }

    /// A finished request attributed to a `(group, model)`, with optional
    /// effort and cache counters, for the model-aggregation tests.
    #[allow(clippy::too_many_arguments)]
    fn finished_model(
        id: u64,
        account: Option<&str>,
        group: &str,
        model: &str,
        effort: Option<&str>,
        status: u16,
        tokens: Option<TokenCounts>,
        path: &str,
    ) -> ActivityEvent {
        ActivityEvent::RequestFinished {
            id,
            method: "POST".into(),
            path: path.into(),
            account: account.map(str::to_string),
            status,
            duration: Duration::from_millis(1_400),
            tokens,
            group: Some(group.into()),
            model: Some(model.into()),
            effort: effort.map(str::to_string),
        }
    }

    // ---- ring buffer behavior ----

    #[test]
    fn ring_buffer_evicts_oldest_and_orders_newest_first() {
        let mut log = ActivityLog::new(3);
        for i in 0..4 {
            log.push_note(format!("note-{i}"), false, at(i));
        }
        let texts: Vec<&str> = log.completed().map(note_text).collect();
        assert_eq!(
            texts,
            vec!["note-3", "note-2", "note-1"],
            "newest first, oldest evicted"
        );
    }

    #[test]
    fn capacity_is_respected_under_mixed_events() {
        let mut log = ActivityLog::new(2);
        log.apply(started(1), at(0));
        log.apply(finished(1, Some("a"), None), at(1));
        log.push_note("one".into(), false, at(2));
        log.push_note("two".into(), false, at(3));
        assert_eq!(log.completed().count(), 2);
        assert_eq!(note_text(log.completed().next().expect("entry")), "two");
    }

    // ---- request lifecycle ----

    #[test]
    fn started_request_is_in_flight_until_finished() {
        let mut log = ActivityLog::new(10);
        log.apply(started(7), at(0));
        assert_eq!(log.in_flight().len(), 1);
        assert_eq!(log.in_flight()[0].account, None);

        log.apply(
            ActivityEvent::RequestRouted {
                id: 7,
                account: "a@x.com".into(),
                group: Some("claude".into()),
                model: Some("claude-sonnet-4-5".into()),
            },
            at(1),
        );
        assert_eq!(log.in_flight()[0].account.as_deref(), Some("a@x.com"));
        assert_eq!(log.in_flight()[0].group.as_deref(), Some("claude"));
        assert_eq!(
            log.in_flight()[0].model.as_deref(),
            Some("claude-sonnet-4-5")
        );

        // Finish without an explicit account: the routed account is kept.
        log.apply(finished(7, None, Some((1_000, 200))), at(2));
        assert!(log.in_flight().is_empty(), "finish clears the spinner row");
        let entry = log.completed().next().expect("completed entry").clone();
        match &entry.body {
            CompletedBody::Request {
                account,
                status,
                tokens,
                ..
            } => {
                assert_eq!(account.as_deref(), Some("a@x.com"));
                assert_eq!(*status, 200);
                assert_eq!(
                    *tokens,
                    Some(TokenCounts {
                        input: 1_000,
                        output: 200,
                        ..Default::default()
                    })
                );
            }
            other => panic!("expected request entry, got {other:?}"),
        }
    }

    #[test]
    fn finish_without_matching_start_still_logs() {
        let mut log = ActivityLog::new(10);
        log.apply(finished(99, Some("b"), None), at(0));
        assert_eq!(log.completed().count(), 1);
        assert!(log.in_flight().is_empty());
    }

    #[test]
    fn in_flight_overflow_retires_oldest_as_error_note() {
        let mut log = ActivityLog::new(200);
        for id in 0..(MAX_IN_FLIGHT as u64 + 1) {
            log.apply(started(id), at(id));
        }
        assert_eq!(log.in_flight().len(), MAX_IN_FLIGHT);
        assert!(!log.in_flight().iter().any(|r| r.id == 0), "oldest dropped");
        let entry = log.completed().next().expect("note").clone();
        match &entry.body {
            CompletedBody::Note { error, .. } => assert!(error),
            other => panic!("expected note, got {other:?}"),
        }
    }

    #[test]
    fn prune_stale_in_flight_sweeps_rows_past_threshold_with_a_note() {
        let mut log = ActivityLog::new(200);
        log.apply(started(1), at(0));
        assert_eq!(log.in_flight().len(), 1, "row is in-flight");

        // Still fresh just before the threshold: nothing swept.
        log.prune_stale_in_flight(at(STALE_IN_FLIGHT.as_secs() - 1));
        assert_eq!(log.in_flight().len(), 1, "not yet stale");

        // Advance past the stale threshold (real requests finish in <90s, so a
        // row this old means its RequestFinished was dropped).
        log.prune_stale_in_flight(at(STALE_IN_FLIGHT.as_secs() + 1));
        assert!(
            log.in_flight().is_empty(),
            "stale row swept, no zombie left"
        );
        let entry = log.completed().next().expect("sweep note").clone();
        match &entry.body {
            CompletedBody::Note { text, error } => {
                assert!(error, "sweep note is an error note");
                assert!(
                    text.contains("presumed finished"),
                    "note names the cause, got {text:?}"
                );
            }
            other => panic!("expected note, got {other:?}"),
        }
    }

    #[test]
    fn apply_sweeps_stale_in_flight_before_folding_next_event() {
        let mut log = ActivityLog::new(200);
        log.apply(started(1), at(0));
        // A later, unrelated event arriving past the threshold sweeps the
        // leaked row even though no RequestFinished for id 1 ever came.
        log.apply(started(2), at(STALE_IN_FLIGHT.as_secs() + 5));
        assert!(
            !log.in_flight().iter().any(|r| r.id == 1),
            "leaked row 1 swept on the next apply"
        );
        assert!(
            log.in_flight().iter().any(|r| r.id == 2),
            "fresh row 2 still in-flight"
        );
    }

    // ---- totals ----

    #[test]
    fn totals_accumulate_per_account_with_ok_error_and_token_split() {
        let mut log = ActivityLog::new(10);
        log.apply(started(1), at(0));
        log.apply(finished(1, Some("a"), Some((700, 300))), at(1));
        log.apply(started(2), at(2));
        log.apply(finished(2, Some("a"), None), at(3)); // unknown tokens count 0
        log.apply(finished_status(3, Some("a"), None, 502), at(4));
        log.apply(finished(4, Some("b"), Some((20, 30))), at(5));

        assert_eq!(
            log.totals_for("a"),
            Totals {
                requests: 3,
                ok: 2,
                errors: 1,
                tokens_in: 700,
                tokens_out: 300,
            }
        );
        assert_eq!(log.totals_for("a").tokens(), 1_000);
        assert_eq!(
            log.totals_for("b"),
            Totals {
                requests: 1,
                ok: 1,
                errors: 0,
                tokens_in: 20,
                tokens_out: 30,
            }
        );
        assert_eq!(log.totals_for("ghost"), Totals::default());
    }

    #[test]
    fn unrouted_failure_counts_globally_but_not_per_account() {
        let mut log = ActivityLog::new(10);
        log.apply(started(1), at(0));
        log.apply(finished_status(1, None, None, 429), at(1)); // never routed
        log.apply(finished(2, Some("a"), Some((5, 5))), at(2));
        assert_eq!(log.totals_for("a").requests, 1);
        assert_eq!(
            log.totals_global(),
            Totals {
                requests: 2,
                ok: 1,
                errors: 1,
                tokens_in: 5,
                tokens_out: 5,
            }
        );
    }

    // ---- requests per minute ----

    #[test]
    fn rpm_counts_only_requests_inside_the_window() {
        let mut log = ActivityLog::new(50);
        let now = at(1_000);
        // 3 requests inside the 5m window, 1 outside, plus a note (ignored).
        log.apply(finished(1, Some("a"), None), at(1_000 - 400)); // outside
        log.apply(finished(2, Some("a"), None), at(1_000 - 200));
        log.apply(finished(3, Some("a"), None), at(1_000 - 100));
        log.apply(finished(4, Some("a"), None), at(1_000));
        log.push_note("switch".into(), false, at(1_000 - 50));

        let rpm = log.requests_per_minute(now, Duration::from_secs(300));
        assert!((rpm - 3.0 / 5.0).abs() < 1e-9, "got {rpm}");
    }

    #[test]
    fn rpm_zero_window_and_empty_log_are_zero() {
        let log = ActivityLog::new(10);
        assert_eq!(
            log.requests_per_minute(at(1_000), Duration::from_secs(300)),
            0.0
        );
        let mut log = ActivityLog::new(10);
        log.apply(finished(1, Some("a"), None), at(1_000));
        assert_eq!(log.requests_per_minute(at(1_000), Duration::ZERO), 0.0);
    }

    #[test]
    fn usage_polled_is_not_an_activity_line() {
        let mut log = ActivityLog::new(10);
        log.apply(
            ActivityEvent::UsagePolled {
                account: "a".into(),
                ok: true,
                consecutive_failures: 0,
                next_in: Duration::from_secs(300),
            },
            at(0),
        );
        assert_eq!(log.completed().count(), 0);
    }

    // ---- model usage aggregation ----

    fn tokens(input: u64, output: u64, cache_read: Option<u64>) -> Option<TokenCounts> {
        Some(TokenCounts {
            input,
            output,
            cache_read,
            cache_creation: None,
        })
    }

    #[test]
    fn endpoint_class_buckets_count_tokens_messages_and_other() {
        assert_eq!(endpoint_class("/v1/messages"), "messages");
        assert_eq!(endpoint_class("/v1/messages?beta=true"), "messages");
        assert_eq!(endpoint_class("/v1/messages/count_tokens"), "count_tokens");
        assert_eq!(endpoint_class("/v1/models"), "models");
    }

    #[test]
    fn normalize_model_strips_context_suffix() {
        assert_eq!(
            normalize_model("claude-sonnet-4-5[1m]"),
            "claude-sonnet-4-5"
        );
        assert_eq!(normalize_model("gpt-5.5"), "gpt-5.5");
    }

    #[test]
    fn model_rows_key_by_group_and_served_model() {
        let mut log = ActivityLog::new(50);
        // Same label, different providers → two rows, never merged (req1/2).
        log.apply(
            finished_model(
                1,
                Some("a"),
                "claude",
                "shared",
                None,
                200,
                tokens(10, 5, None),
                "/v1/messages",
            ),
            at(1),
        );
        log.apply(
            finished_model(
                2,
                Some("c"),
                "codex",
                "shared",
                None,
                200,
                tokens(20, 7, None),
                "/v1/messages",
            ),
            at(2),
        );
        let rows = log.model_usage();
        assert_eq!(rows.len(), 2);
        // Sorted by total tokens desc → codex (27) before claude (15).
        assert_eq!(
            (rows[0].group.as_str(), rows[0].model.as_str()),
            ("codex", "shared")
        );
        assert_eq!(
            (rows[1].group.as_str(), rows[1].model.as_str()),
            ("claude", "shared")
        );
    }

    #[test]
    fn model_row_accumulates_split_cache_effort_endpoint_and_accounts() {
        let mut log = ActivityLog::new(50);
        log.apply(
            finished_model(
                1,
                Some("a"),
                "claude",
                "claude-sonnet-4-5[1m]",
                Some("16k"),
                200,
                tokens(100, 40, Some(900)),
                "/v1/messages",
            ),
            at(10),
        );
        log.apply(
            finished_model(
                2,
                Some("b"),
                "claude",
                "claude-sonnet-4-5",
                None,
                200,
                tokens(50, 20, None),
                "/v1/messages/count_tokens",
            ),
            at(20),
        );
        // A failed request with a known model: error count, no tokens (req-test).
        log.apply(
            finished_model(
                3,
                Some("a"),
                "claude",
                "claude-sonnet-4-5",
                Some("16k"),
                529,
                None,
                "/v1/messages",
            ),
            at(30),
        );
        let rows = log.model_usage();
        // Suffix normalization merges into one row (req17).
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.requests, 3);
        assert_eq!(row.ok, 2);
        assert_eq!(row.errors, 1);
        assert_eq!(row.tokens_in, 150);
        assert_eq!(row.tokens_out, 60);
        // cache_read present from req1 only; cache_creation never reported.
        assert_eq!(row.cache_read, Some(900));
        assert_eq!(row.cache_creation, None);
        assert_eq!(row.last_used, at(30));
        // Effort distribution: 16k×2, none×1.
        let effort: HashMap<&str, u64> = row
            .efforts
            .iter()
            .map(|c| (c.label.as_str(), c.requests))
            .collect();
        assert_eq!(effort.get("16k"), Some(&2));
        assert_eq!(effort.get("none"), Some(&1));
        // Endpoint split: messages×2, count_tokens×1.
        let endpoint: HashMap<&str, u64> = row
            .endpoints
            .iter()
            .map(|c| (c.label.as_str(), c.requests))
            .collect();
        assert_eq!(endpoint.get("messages"), Some(&2));
        assert_eq!(endpoint.get("count_tokens"), Some(&1));
        // Per-account: a served 2 (one failed), b served 1.
        let a = row
            .accounts
            .iter()
            .find(|x| x.name == "a")
            .expect("account a");
        assert_eq!((a.requests, a.ok, a.errors, a.tokens_in), (2, 1, 1, 100));
        let b = row
            .accounts
            .iter()
            .find(|x| x.name == "b")
            .expect("account b");
        assert_eq!((b.requests, b.tokens_in), (1, 50));
    }

    #[test]
    fn pre_routing_failure_does_not_create_a_model_row() {
        let mut log = ActivityLog::new(50);
        // No group/model (body-read failure): global accounting only, no row.
        log.apply(finished_status(1, None, None, 400), at(1));
        assert!(log.model_usage().is_empty());
        assert_eq!(log.totals_global().requests, 1);
    }
}
