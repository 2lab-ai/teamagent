//! `ActivityEvent` — the proxy→TUI activity feed contract.
//!
//! The TUI owns this type (re-exported as `crate::tui::ActivityEvent`); the
//! proxy holds the `tokio::sync::mpsc::Sender<ActivityEvent>` side and emits
//! one event per observable happening. Senders should use `try_send` and drop
//! the event on a full channel — the dashboard is best-effort observability
//! and must never backpressure the request path.

use std::time::Duration;

/// Input/output token counts for one completed request, when the upstream
/// response carried usage. `input`/`output` are the fresh (non-cached) prompt
/// and completion counts; the optional cache counters feed the model-usage
/// rows and are `None` when the upstream did not report them (distinct from an
/// explicit `Some(0)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: Option<u64>,
    pub cache_creation: Option<u64>,
}

impl TokenCounts {
    /// Full context size for the single-number activity display: fresh input +
    /// cache reads + cache writes + output. Including the cached portion is
    /// essential — `input` is the FRESH (non-cached) prompt only, so when codex
    /// caching kicks in the cached tokens move out of `input` into `cache_read`.
    /// Summing only `input + output` therefore makes the displayed number DROP
    /// between turns even though the conversation GREW (e.g. 182,905 → 156,795
    /// when 26,624 tokens got cached). Cached tokens are real context that still
    /// fills the window, so the displayed size must count them.
    pub fn total(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read.unwrap_or(0))
            .saturating_add(self.cache_creation.unwrap_or(0))
    }
}

/// One observable proxy/scheduler happening, rendered into the activity log.
///
/// `id` correlates the started → routed → finished lifecycle of a single
/// request (any per-process unique counter works; it never leaves the TUI).
/// `RequestFinished` repeats `method`/`path`/`account` so a finish whose
/// start was dropped (channel full, TUI attached late) still renders as a
/// complete log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityEvent {
    /// A client request was accepted; shows as in-flight (spinner) until the
    /// matching `RequestFinished` arrives.
    RequestStarted {
        id: u64,
        method: String,
        path: String,
    },
    /// The scheduler leased an account for request `id`. Carries the served
    /// `(group, model)` identity decided at lease time so the dashboard can
    /// attribute in-flight requests to a model row before they finish.
    RequestRouted {
        id: u64,
        account: String,
        /// Backend group of the leased account (`"claude"`/`"codex"`), when
        /// routing attributed one.
        group: Option<String>,
        /// Served model: codex → the configured upstream model; claude → the
        /// inbound model string.
        model: Option<String>,
    },
    /// Request `id` completed (any status, including upstream errors).
    RequestFinished {
        id: u64,
        method: String,
        path: String,
        /// Account that served it; `None` if it failed before routing.
        account: Option<String>,
        /// HTTP status returned to the client.
        status: u16,
        duration: Duration,
        /// Tokens extracted from the response usage, when available — feeds
        /// the per-account in/out token totals.
        tokens: Option<TokenCounts>,
        /// Backend group that served it (`"claude"`/`"codex"`), when known.
        group: Option<String>,
        /// Model slug actually served (codex: the configured model; claude:
        /// the inbound model string), when known.
        model: Option<String>,
        /// Reasoning effort (codex: configured effort; claude: thinking
        /// budget like `"16k"`), when known.
        effort: Option<String>,
    },
    /// The scheduler committed a switch of the current account.
    AccountSwitched {
        /// `None` on the initial selection.
        from: Option<String>,
        to: String,
        /// Human-readable cause ("429 park", "5h threshold", …).
        reason: Option<String>,
    },
    /// An OAuth access token was refreshed for `account`.
    TokenRefreshed {
        account: String,
        /// New access-token expiry, epoch ms — lets the activity line show
        /// how much lifetime the refresh bought.
        expires_at_ms: u64,
    },
    /// The usage poller finished one poll attempt for `account` — feeds the
    /// dashboard's poller-health pane (last success age, backoff, next ETA).
    UsagePolled {
        account: String,
        /// Whether this attempt succeeded.
        ok: bool,
        /// Consecutive failures so far (0 after a success).
        consecutive_failures: u32,
        /// Delay until the next scheduled poll of this account.
        next_in: Duration,
    },
    /// Anything that went wrong and deserves operator eyes.
    Error {
        /// What was being attempted ("usage poll", "refresh", …).
        context: Option<String>,
        message: String,
    },
}

#[cfg(test)]
mod token_total_tests {
    use super::TokenCounts;
    #[test]
    fn total_counts_the_cached_context_so_it_does_not_drop_when_caching_kicks_in() {
        // Turn N: nothing cached yet — fresh 182586 + out 319.
        let a = TokenCounts {
            input: 182_586,
            output: 319,
            cache_read: Some(0),
            cache_creation: None,
        };
        // Turn N+1: conversation GREW, 26624 of the prompt got cached → fresh drops,
        // but cache_read holds it. Real context = 183231 + 188 = 183419.
        let b = TokenCounts {
            input: 183_231 - 26_624,
            output: 188,
            cache_read: Some(26_624),
            cache_creation: None,
        };
        assert_eq!(a.total(), 182_905);
        assert_eq!(
            b.total(),
            183_419,
            "must include cached context, not drop to 156795"
        );
        assert!(
            b.total() > a.total(),
            "displayed size must grow with the conversation, not shrink on a cache hit"
        );
    }
}
