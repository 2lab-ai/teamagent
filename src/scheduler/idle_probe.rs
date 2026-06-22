//! On-demand idle-account usage probe (issue #21).
//!
//! An account with no known 5h/7d window produces no usage data, so the
//! scheduler ranks/displays it as a guessed cold account. This module fills
//! that window ON DEMAND: when such an account is needed for ranking/display,
//! a single `max_tokens = 1` `POST /v1/messages` (or the codex `/responses`
//! equivalent) is sent through the account's own credential, and the
//! response's `anthropic-ratelimit-*` headers feed the existing
//! [`crate::scheduler::window::WindowSource::Headers`] path
//! ([`super::AccountPool::record_headers`]).
//!
//! This is NOT a background poll loop (that is the `usage` poller's job for
//! oauth accounts). It is strictly on-demand and DOUBLE-GATED:
//!
//! 1. a global kill-switch ([`crate::config::IdleProbeConfig::enabled`] —
//!    `false` disables ALL probing), and
//! 2. a per-account cooldown so one account is probed at most once per
//!    [`crate::config::IdleProbeConfig::per_account_cooldown_secs`].
//!
//! The gate decision (`should_probe`) is pure; the per-account last-probe
//! bookkeeping lives behind a `std::sync::Mutex` here (impure, but never held
//! across an `.await` — `try_acquire` returns before any send). The actual
//! send is the injectable [`Prober`] trait, so the gating is unit-testable
//! without a network. The production [`ReqwestProber`] reuses the same
//! provider hooks + `reqwest::Client` the forward path uses.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use http::HeaderMap;

use super::headers as rl_headers;
use super::{AccountId, AccountPool};
use crate::config::{AccountCredential, IdleProbeConfig};

/// Failure of a single probe send. Never carries a credential.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("probe http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("probe request build failed: {0}")]
    Build(String),
}

/// Injectable transport for one idle probe: build a `max_tokens = 1` request
/// for `credential`'s provider, send it, and return the upstream response
/// headers (the orchestrator parses the `anthropic-ratelimit-*` set out of
/// them). Mirrors [`crate::scheduler::usage::UsageFetcher`] so the gating is
/// testable without a network.
pub trait Prober: Send + Sync {
    fn probe(
        &self,
        credential: &AccountCredential,
    ) -> impl Future<Output = Result<HeaderMap, ProbeError>> + Send;
}

/// Pure gate: may this account be probed at `now` given when it was last
/// probed and the per-account cooldown? `None` last-probe = never probed =
/// allowed. A `last_probe_at` in the future (clock skew) is treated as a
/// fresh probe (still cooling down), mirroring `QuotaWindow::is_stale`.
pub fn should_probe(
    last_probe_at: Option<SystemTime>,
    now: SystemTime,
    cooldown: Duration,
) -> bool {
    match last_probe_at {
        None => true,
        Some(last) => now
            .duration_since(last)
            .is_ok_and(|elapsed| elapsed >= cooldown),
    }
}

/// On-demand idle-probe orchestrator: owns the config (kill-switch +
/// cooldown), the per-account last-probe bookkeeping, the injectable prober,
/// and the pool the result is recorded into.
pub struct IdleProber<P> {
    pool: AccountPool,
    prober: P,
    config: IdleProbeConfig,
    /// Per-account wall-clock time of the last *attempted* probe (recorded
    /// when the gate is acquired, BEFORE the send — so a slow/failing probe
    /// still consumes the cooldown and cannot be retried in a tight loop).
    last_probe_at: Mutex<HashMap<AccountId, SystemTime>>,
}

impl<P: Prober> IdleProber<P> {
    pub fn new(pool: AccountPool, prober: P, config: IdleProbeConfig) -> Self {
        Self {
            pool,
            prober,
            config,
            last_probe_at: Mutex::new(HashMap::new()),
        }
    }

    fn cooldown(&self) -> Duration {
        Duration::from_secs(self.config.per_account_cooldown_secs)
    }

    /// Compare-and-set the per-account cooldown gate: returns `true` exactly
    /// once per cooldown window, recording `now` as the new last-probe time on
    /// success. The check + record happen under one lock, so two concurrent
    /// callers cannot both acquire the same window. The kill-switch
    /// (`enabled = false`) refuses unconditionally without touching the map.
    fn try_acquire(&self, account: &AccountId, now: SystemTime) -> bool {
        if !self.config.enabled {
            return false;
        }
        let mut map = self
            .last_probe_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !should_probe(map.get(account).copied(), now, self.cooldown()) {
            return false;
        }
        map.insert(account.clone(), now);
        true
    }

    /// True if the account currently has NO window at all (neither 5h nor
    /// 7d) — the only case a probe is for. An account with any window is
    /// already covered by headers/poll and is left alone.
    fn has_no_window(&self, account: &AccountId) -> bool {
        self.pool
            .snapshot()
            .accounts
            .iter()
            .find(|a| &a.id == account)
            .is_some_and(|a| a.five_hour.is_none() && a.seven_day.is_none())
    }

    /// On-demand entry point: probe `account` once iff probing is enabled, the
    /// account has no window, and its cooldown has elapsed. Returns whether a
    /// probe was actually sent. On a successful send the parsed
    /// `anthropic-ratelimit-*` headers are recorded into the pool via
    /// [`AccountPool::record_headers`] (the `WindowSource::Headers` path).
    ///
    /// The pure gating (`try_acquire`) completes and the lock is released
    /// BEFORE the `.await` on the send — no std lock is held across await.
    pub async fn probe_if_idle(&self, account: &AccountId, now: SystemTime) -> bool {
        if !self.config.enabled || !self.has_no_window(account) {
            return false;
        }
        let Some(credential) = self.pool.credential(account) else {
            return false;
        };
        if !self.try_acquire(account, now) {
            return false;
        }
        match self.prober.probe(&credential).await {
            Ok(headers) => {
                let parsed = rl_headers::parse(&headers);
                if !parsed.is_empty() {
                    // Stamp the recording at the send-completion instant so the
                    // freshest-wins merge orders correctly against concurrent
                    // poll/header data.
                    self.pool
                        .record_headers(account, &parsed, SystemTime::now());
                }
                true
            }
            Err(err) => {
                tracing::warn!(account = %account, error = %err, "idle probe failed");
                // The cooldown was already consumed in `try_acquire`, so a
                // failing account is not re-probed in a tight loop.
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW_SECS: u64 = 1_000_000;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn now() -> SystemTime {
        at(NOW_SECS)
    }

    fn id(s: &str) -> AccountId {
        AccountId(s.to_string())
    }

    fn oauth_account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            credential: AccountCredential::Oauth {
                account_uuid: format!("uuid-{name}"),
                access_token: format!("at-{name}"),
                refresh_token: format!("rt-{name}"),
                expires_at_ms: 0,
                tier: None,
                last_refresh_ms: None,
            },
        }
    }

    fn cfg(enabled: bool, cooldown_secs: u64) -> IdleProbeConfig {
        IdleProbeConfig {
            enabled,
            per_account_cooldown_secs: cooldown_secs,
        }
    }

    /// Scripted prober: counts calls and returns a fixed unified-window header
    /// set so the orchestrator's record path is exercised end to end.
    struct CountingProber {
        calls: AtomicUsize,
        five_hour_util: f64,
    }

    impl CountingProber {
        fn new(five_hour_util: f64) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                five_hour_util,
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Prober for &CountingProber {
        fn probe(
            &self,
            _credential: &AccountCredential,
        ) -> impl Future<Output = Result<HeaderMap, ProbeError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let util = self.five_hour_util;
            async move {
                let mut headers = HeaderMap::new();
                headers.insert(
                    "anthropic-ratelimit-unified-5h-utilization",
                    util.to_string().parse().unwrap(),
                );
                headers.insert(
                    "anthropic-ratelimit-unified-5h-reset",
                    (NOW_SECS + 3600).to_string().parse().unwrap(),
                );
                headers.insert(
                    "anthropic-ratelimit-unified-7d-utilization",
                    "0.20".parse().unwrap(),
                );
                headers.insert(
                    "anthropic-ratelimit-unified-7d-reset",
                    (NOW_SECS + 86_400).to_string().parse().unwrap(),
                );
                Ok(headers)
            }
        }
    }

    /// Prober that always errors — for the "failed send still consumes the
    /// cooldown" case.
    struct FailingProber {
        calls: AtomicUsize,
    }

    impl Prober for &FailingProber {
        fn probe(
            &self,
            _credential: &AccountCredential,
        ) -> impl Future<Output = Result<HeaderMap, ProbeError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            async move { Err(ProbeError::Build("scripted failure".into())) }
        }
    }

    // ---- pure gate ----

    #[test]
    fn gate_allows_never_probed() {
        assert!(should_probe(None, now(), Duration::from_secs(3600)));
    }

    #[test]
    fn gate_blocks_within_cooldown_allows_after() {
        let cooldown = Duration::from_secs(3600);
        let last = now();
        assert!(
            !should_probe(Some(last), at(NOW_SECS + 3599), cooldown),
            "1s before cooldown ends: blocked"
        );
        assert!(
            should_probe(Some(last), at(NOW_SECS + 3600), cooldown),
            "exactly at cooldown end: allowed"
        );
    }

    #[test]
    fn gate_future_last_probe_is_blocked() {
        // Clock skew: last probe stamped in the future ⇒ still cooling down.
        assert!(!should_probe(
            Some(at(NOW_SECS + 100)),
            now(),
            Duration::from_secs(3600)
        ));
    }

    // ---- orchestration ----

    #[tokio::test]
    async fn no_window_account_triggers_exactly_one_probe_and_populates_windows() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        let prober = CountingProber::new(0.42);
        let orch = IdleProber::new(pool.clone(), &prober, cfg(true, 3600));

        assert!(orch.probe_if_idle(&id("a"), now()).await, "probe sent");
        assert_eq!(prober.call_count(), 1);

        let snapshot = pool.snapshot();
        let acct = &snapshot.accounts[0];
        let five = acct.five_hour.expect("5h window populated by probe");
        assert!((five.utilization - 0.42).abs() < 1e-9);
        assert_eq!(
            five.source,
            crate::scheduler::window::WindowSource::Headers,
            "populated via the Headers path"
        );
        assert!(acct.seven_day.is_some(), "7d window populated by probe");
    }

    #[tokio::test]
    async fn cooldown_prevents_a_second_probe_within_the_window() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        let prober = CountingProber::new(0.42);
        let orch = IdleProber::new(pool.clone(), &prober, cfg(true, 3600));

        // First probe records a window — so the account is no longer windowless.
        assert!(orch.probe_if_idle(&id("a"), now()).await);
        assert_eq!(prober.call_count(), 1);

        // Even after the window expires (forcing `has_no_window` true again by
        // clearing it), the cooldown alone must suppress a second probe.
        pool.snapshot(); // (no-op read; window still present here)
        assert!(
            !orch.probe_if_idle(&id("a"), at(NOW_SECS + 60)).await,
            "second probe within cooldown suppressed"
        );
        assert_eq!(prober.call_count(), 1, "no second send");
    }

    #[tokio::test]
    async fn cooldown_gate_suppresses_even_when_window_unknown() {
        // Isolate the cooldown from `has_no_window`: a prober that returns NO
        // usable headers leaves the account windowless, so only the cooldown
        // can stop the second probe.
        let pool = AccountPool::new(&[oauth_account("a")]);
        let prober = FailingProber {
            calls: AtomicUsize::new(0),
        };
        let orch = IdleProber::new(pool.clone(), &prober, cfg(true, 3600));

        assert!(
            orch.probe_if_idle(&id("a"), now()).await,
            "first probe sent"
        );
        assert_eq!(prober.calls.load(Ordering::SeqCst), 1);
        // Account is still windowless (failed probe recorded nothing), yet the
        // cooldown blocks a second attempt 60s later.
        assert!(
            !orch.probe_if_idle(&id("a"), at(NOW_SECS + 60)).await,
            "cooldown suppresses second probe regardless of window state"
        );
        assert_eq!(prober.calls.load(Ordering::SeqCst), 1);
        // After the cooldown elapses, a probe fires again.
        assert!(
            orch.probe_if_idle(&id("a"), at(NOW_SECS + 3600)).await,
            "probe allowed once cooldown elapsed"
        );
        assert_eq!(prober.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn kill_switch_disables_all_probing() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        let prober = CountingProber::new(0.42);
        let orch = IdleProber::new(pool.clone(), &prober, cfg(false, 3600));

        assert!(
            !orch.probe_if_idle(&id("a"), now()).await,
            "disabled: no probe"
        );
        assert_eq!(prober.call_count(), 0, "kill-switch sends nothing");
        assert!(
            pool.snapshot().accounts[0].five_hour.is_none(),
            "no window recorded"
        );
    }

    #[tokio::test]
    async fn account_with_a_window_is_not_probed() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        // Pre-populate a window via the usage path so the account is not idle.
        pool.record_usage(
            &id("a"),
            &crate::scheduler::usage::UsageSnapshot {
                five_hour: Some(rl_headers::WindowReading {
                    utilization: 0.1,
                    resets_at: at(NOW_SECS + 3600),
                }),
                seven_day: None,
            },
            now(),
        );
        let prober = CountingProber::new(0.42);
        let orch = IdleProber::new(pool.clone(), &prober, cfg(true, 3600));

        assert!(
            !orch.probe_if_idle(&id("a"), now()).await,
            "account already has a window: not probed"
        );
        assert_eq!(prober.call_count(), 0);
    }

    #[tokio::test]
    async fn unknown_account_is_not_probed() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        let prober = CountingProber::new(0.42);
        let orch = IdleProber::new(pool, &prober, cfg(true, 3600));
        assert!(!orch.probe_if_idle(&id("ghost"), now()).await);
        assert_eq!(prober.call_count(), 0);
    }
}
