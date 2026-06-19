//! `QuotaWindow` — one rate-limit window (5h session or 7d weekly) with
//! wall-clock expiry: once `resets_at` passes, the window reads as empty.

use std::time::{Duration, SystemTime};

/// Where a window observation came from. Headers are authoritative during
/// traffic; the usage poller covers idle accounts. Freshest `fetched_at`
/// wins per window regardless of source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    /// Parsed from `anthropic-ratelimit-*` response headers.
    Headers,
    /// Fetched from `GET /api/oauth/usage`.
    UsagePoll,
}

/// A point-in-time observation of one quota window.
///
/// All time fields are `SystemTime` (wall clock), NOT `Instant`: reset
/// timestamps arrive as epoch seconds / RFC3339 from upstream and must
/// survive comparison against externally supplied "now" values in pure
/// scheduler code and in tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaWindow {
    /// Utilization 0.0..=1.0 as reported by upstream.
    pub utilization: f64,
    /// When this window resets (epoch-based wall clock).
    pub resets_at: SystemTime,
    /// When this observation was made; staleness is judged against this.
    pub fetched_at: SystemTime,
    pub source: WindowSource,
}

impl QuotaWindow {
    /// True once `resets_at` has passed — the window no longer constrains.
    pub fn is_expired(&self, now: SystemTime) -> bool {
        self.resets_at <= now
    }

    /// Utilization with wall-clock expiry applied: 0.0 if expired,
    /// `self.utilization` otherwise.
    pub fn effective_utilization(&self, now: SystemTime) -> f64 {
        if self.is_expired(now) {
            0.0
        } else {
            self.utilization
        }
    }

    /// True if this observation is older than `max_age`. Stale windows must
    /// not drive scheduling (don't schedule on fiction). An observation
    /// stamped in the future (clock skew) is treated as fresh.
    pub fn is_stale(&self, now: SystemTime, max_age: Duration) -> bool {
        now.duration_since(self.fetched_at)
            .is_ok_and(|age| age > max_age)
    }
}

/// How one usage window should be *displayed*, distinct from the silent
/// `—`/`0%` collapse the dashboard used to show for every non-populated case
/// (issue #33). This is a pure, render-only classification: it spends no
/// tokens, makes no upstream call, and never feeds back into scheduling — it
/// only makes already-recorded state legible.
///
/// Precedence, strongest "trust this less" signal first:
/// 1. [`Self::PollDegraded`] — the usage poller has `consecutive_failures > 0`,
///    so whatever the window says may already be diverging from reality.
/// 2. [`Self::Cold`] — no live window has ever been seen (window `None`, or an
///    expired observation that carries no constraint). This is the never-used
///    account the issue calls out: previously indistinguishable from "0% used".
/// 3. [`Self::Stale`] — a live window exists but its observation is older than
///    `max_age`; the value is real but no longer trustworthy for scheduling.
/// 4. [`Self::Populated`] — a fresh, live window value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowDisplayState {
    /// A fresh, live window value exists.
    Populated,
    /// No live window has ever been seen (cold / unknown account).
    Cold,
    /// A live window exists but is older than `max_age`.
    Stale,
    /// The usage poller is failing (`consecutive_failures > 0`); the window
    /// value, if any, may be diverging from upstream.
    PollDegraded,
}

impl WindowDisplayState {
    /// A short, stable label for rendering and serialization.
    pub fn label(self) -> &'static str {
        match self {
            WindowDisplayState::Populated => "populated",
            WindowDisplayState::Cold => "cold",
            WindowDisplayState::Stale => "stale",
            WindowDisplayState::PollDegraded => "poll-degraded",
        }
    }

    /// A single-glyph marker for the dense table cells.
    pub fn glyph(self) -> char {
        match self {
            WindowDisplayState::Populated => '●',
            WindowDisplayState::Cold => '○',
            WindowDisplayState::Stale => '◑',
            WindowDisplayState::PollDegraded => '!',
        }
    }
}

/// Classify one window observation for display (issue #33), render-only.
///
/// Mirrors the scheduler's own notion of "cold vs stale" ([`is_stale`] /
/// `select::usage_is_stale`): an *expired* observation carries no constraint
/// and degrades to [`WindowDisplayState::Cold`] rather than reading as stale,
/// so a long-idle account that has reset shows as cold, not falsely stale.
///
/// Pure over its inputs — `now`, `max_age`, and the poller's
/// `consecutive_failures` are all passed in, so this stays unit-testable and
/// free of clocks/IO.
pub fn classify_window_display(
    window: &Option<QuotaWindow>,
    now: SystemTime,
    max_age: Duration,
    consecutive_failures: u32,
) -> WindowDisplayState {
    if consecutive_failures > 0 {
        return WindowDisplayState::PollDegraded;
    }
    match window {
        // An expired observation no longer constrains; treat as never-seen.
        Some(w) if !w.is_expired(now) => {
            if w.is_stale(now, max_age) {
                WindowDisplayState::Stale
            } else {
                WindowDisplayState::Populated
            }
        }
        _ => WindowDisplayState::Cold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn window(utilization: f64, resets_at: u64, fetched_at: u64) -> QuotaWindow {
        QuotaWindow {
            utilization,
            resets_at: at(resets_at),
            fetched_at: at(fetched_at),
            source: WindowSource::Headers,
        }
    }

    #[test]
    fn not_expired_before_reset() {
        assert!(!window(0.5, 1000, 900).is_expired(at(999)));
    }

    #[test]
    fn expired_exactly_at_reset() {
        assert!(window(0.5, 1000, 900).is_expired(at(1000)));
    }

    #[test]
    fn expired_after_reset() {
        assert!(window(0.5, 1000, 900).is_expired(at(1001)));
    }

    #[test]
    fn effective_utilization_passes_through_when_live() {
        let w = window(0.73, 1000, 900);
        assert_eq!(w.effective_utilization(at(950)), 0.73);
    }

    #[test]
    fn effective_utilization_zero_when_expired() {
        let w = window(0.99, 1000, 900);
        assert_eq!(w.effective_utilization(at(1000)), 0.0);
    }

    #[test]
    fn stale_only_past_max_age() {
        let w = window(0.5, 10_000, 1000);
        let max_age = Duration::from_secs(600);
        assert!(!w.is_stale(at(1600), max_age), "age == max_age is fresh");
        assert!(w.is_stale(at(1601), max_age), "age > max_age is stale");
    }

    #[test]
    fn future_fetched_at_is_not_stale() {
        let w = window(0.5, 10_000, 5000);
        assert!(!w.is_stale(at(1000), Duration::from_secs(1)));
    }

    // ---- WindowDisplayState (issue #33: distinct render states) ----

    const MAX_AGE: Duration = Duration::from_secs(600);

    #[test]
    fn display_state_cold_when_window_never_seen() {
        // The never-used account: window None. Previously collapsed to —/0%.
        assert_eq!(
            classify_window_display(&None, at(1000), MAX_AGE, 0),
            WindowDisplayState::Cold,
        );
    }

    #[test]
    fn display_state_populated_when_fresh_and_live() {
        let w = Some(window(0.42, 10_000, 1000));
        assert_eq!(
            classify_window_display(&w, at(1200), MAX_AGE, 0),
            WindowDisplayState::Populated,
        );
    }

    #[test]
    fn display_state_stale_when_live_but_past_max_age() {
        // Live (resets far in the future) but fetched > max_age ago.
        let w = Some(window(0.42, 100_000, 1000));
        assert_eq!(
            classify_window_display(&w, at(1000 + 601), MAX_AGE, 0),
            WindowDisplayState::Stale,
        );
    }

    #[test]
    fn display_state_expired_window_reads_cold_not_stale() {
        // Old observation whose window has already reset carries no constraint:
        // it degrades to cold (mirrors select::usage_is_stale), not stale.
        let w = Some(window(0.42, 2000, 1000));
        assert_eq!(
            classify_window_display(&w, at(5000), MAX_AGE, 0),
            WindowDisplayState::Cold,
        );
    }

    #[test]
    fn display_state_poll_degraded_overrides_window_value() {
        // Even a fresh, populated window reads as poll-degraded when the poller
        // is failing — the strongest "trust this less" signal.
        let w = Some(window(0.42, 10_000, 1000));
        assert_eq!(
            classify_window_display(&w, at(1200), MAX_AGE, 1),
            WindowDisplayState::PollDegraded,
        );
        // And it applies to a cold window too.
        assert_eq!(
            classify_window_display(&None, at(1200), MAX_AGE, 3),
            WindowDisplayState::PollDegraded,
        );
    }

    #[test]
    fn display_states_are_distinct_across_the_four_cases() {
        // The acceptance: empty / stale / poll-degraded / populated must NOT
        // all collapse into one variant.
        let fresh = Some(window(0.42, 10_000, 1000));
        let stale = Some(window(0.42, 100_000, 1000));
        let states = [
            classify_window_display(&fresh, at(1200), MAX_AGE, 0),
            classify_window_display(&None, at(1200), MAX_AGE, 0),
            classify_window_display(&stale, at(1000 + 601), MAX_AGE, 0),
            classify_window_display(&fresh, at(1200), MAX_AGE, 2),
        ];
        assert_eq!(
            states,
            [
                WindowDisplayState::Populated,
                WindowDisplayState::Cold,
                WindowDisplayState::Stale,
                WindowDisplayState::PollDegraded,
            ],
        );
        // Labels/glyphs are distinct too, so the render is legible.
        let labels: std::collections::HashSet<_> = states.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), 4, "all four labels must be distinct");
        let glyphs: std::collections::HashSet<_> = states.iter().map(|s| s.glyph()).collect();
        assert_eq!(glyphs.len(), 4, "all four glyphs must be distinct");
    }
}
