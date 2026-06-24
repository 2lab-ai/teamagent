//! AccountPool: owns per-account scheduler state and applies events.
//!
//! Concurrency model (see `.prd/02-architecture.md`): `PoolState` lives behind
//! `Arc<std::sync::RwLock<_>>` — every mutation is sync and IO-free, so a std
//! lock is correct (short critical sections, no `.await` while held) and lets
//! `AccountLease::drop` release synchronously. Decisions are NOT made here:
//! `select::pick` is a pure function over a `PoolSnapshot`.

pub mod headers;
pub mod idle_probe;
pub mod select;
pub mod usage;
pub mod window;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use crate::config::{AccountConfig, AccountCredential};
use crate::routing::BackendGroup;
use headers::{ParsedRateLimitHeaders, WindowReading};
use usage::UsageSnapshot;
use window::{QuotaWindow, WindowSource};

/// The group slot used for the legacy (routing-disabled) selection path:
/// when callers pass `group = None`, a SINGLE shared current slot is used so
/// behavior is byte-for-byte unchanged. `Claude` is that slot. Routing-on
/// callers always pass `Some(group)`, so the legacy slot never coexists with
/// real per-group slots in one process.
const LEGACY_GROUP: BackendGroup = BackendGroup::Claude;

/// Heuristic cooldown applied to a 429 WITHOUT `retry-after`. Such a 429 is
/// almost always a transient, server-side limit (Anthropic "Server is
/// temporarily limiting requests (not your usage limit)") rather than the
/// account's own quota — a real quota 429 carries `retry-after`/reset headers,
/// and the 5h/7d usage windows are the authoritative quota gate anyway. So this
/// is SHORT (recover fast, let the client retry) and self-heals early on fresh
/// data showing capacity. A 60-minute park here would strand a fully-usable
/// account (≈2% utilized) for an hour on a momentary blip.
///
/// 8s, not 30s: a retry-after-less 429 is a per-minute-window blip, so 30s
/// over-parks. Paired with heuristic-degraded selection
/// (`select::heuristic_degraded_mode`), which serves the soonest-freed account
/// when an in-flight burst parks the whole group this way — so even this short
/// park no longer hard-locks the pool.
pub const DEFAULT_HEURISTIC_COOLDOWN: Duration = Duration::from_secs(8);

/// Stable account identifier — the config `name`. Newtype so ids don't get
/// mixed up with credentials or display strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(pub String);

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Auth-level health, distinct from quota state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountHealth {
    Healthy,
    /// Refresh failed or upstream said 401 twice — needs re-login.
    AuthFailed,
    /// Persistent non-auth error; message kept for status output.
    Errored(String),
}

/// Why an account is cooling down — fresh usage data may self-heal a
/// `Heuristic` guess but must not override an explicit `RetryAfter` park.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownSource {
    /// Upstream 429 carried `retry-after`; park exactly that long.
    RetryAfter,
    /// Heuristic cooldown (no retry-after); clearable by fresh capacity.
    Heuristic,
}

/// Full per-account scheduler state.
#[derive(Debug, Clone)]
pub struct AccountState {
    pub id: AccountId,
    pub credential: AccountCredential,
    pub health: AccountHealth,
    pub five_hour: Option<QuotaWindow>,
    pub seven_day: Option<QuotaWindow>,
    pub cooldown_until: Option<SystemTime>,
    pub cooldown_source: Option<CooldownSource>,
    /// When the active cooldown was set. Self-healing requires evidence
    /// STRICTLY NEWER than this — otherwise the same response that carried
    /// the 429 could immediately clear its own cooldown via its headers.
    pub cooldown_set_at: Option<SystemTime>,
    /// Live leases (in-flight requests pinned to this account).
    pub in_flight: u32,
}

impl AccountState {
    fn fresh(config: &AccountConfig) -> Self {
        Self {
            id: AccountId(config.name.clone()),
            credential: config.credential.clone(),
            health: AccountHealth::Healthy,
            five_hour: None,
            seven_day: None,
            cooldown_until: None,
            cooldown_source: None,
            cooldown_set_at: None,
            in_flight: 0,
        }
    }

    /// Merge one window observation, freshest `fetched_at` wins.
    fn merge_window(
        slot: &mut Option<QuotaWindow>,
        reading: WindowReading,
        fetched_at: SystemTime,
        source: WindowSource,
    ) -> bool {
        let keep_existing = slot.is_some_and(|old| old.fetched_at > fetched_at);
        if keep_existing {
            return false;
        }
        *slot = Some(QuotaWindow {
            utilization: reading.utilization,
            resets_at: reading.resets_at,
            fetched_at,
            source,
        });
        true
    }

    /// Cooldown self-healing: fresh data (strictly newer than the cooldown)
    /// showing capacity (< 100% on every present window) clears a `Heuristic`
    /// cooldown. `RetryAfter` parks are explicit upstream instructions and
    /// are never cleared early.
    fn maybe_self_heal(&mut self, now: SystemTime) {
        if self.cooldown_source != Some(CooldownSource::Heuristic) {
            return;
        }
        let newer_than_cooldown = self.cooldown_set_at.is_none_or(|set_at| now > set_at);
        if !newer_than_cooldown {
            return;
        }
        let windows: Vec<&QuotaWindow> = [&self.five_hour, &self.seven_day]
            .into_iter()
            .flatten()
            .collect();
        let shows_capacity =
            !windows.is_empty() && windows.iter().all(|w| w.effective_utilization(now) < 1.0);
        if shows_capacity {
            self.cooldown_until = None;
            self.cooldown_source = None;
            self.cooldown_set_at = None;
        }
    }
}

/// The pool's mutable state. All mutations re-validate preconditions before
/// applying (CAS pattern) — see `commit_switch`.
#[derive(Debug, Clone, Default)]
pub struct PoolState {
    pub accounts: Vec<AccountState>,
    /// Currently selected account PER backend group (session stickiness,
    /// independent per group). A group is absent until its first selection
    /// and removed when its accounts are all exhausted. With routing
    /// disabled only the [`LEGACY_GROUP`] slot is ever populated, so the map
    /// degenerates to the single-current-slot behavior of before.
    pub current: BTreeMap<BackendGroup, AccountId>,
}

/// Switch failed; nothing was mutated.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SwitchError {
    /// CAS failure: `current` is no longer what the caller observed.
    #[error("current account changed (now {actual:?})")]
    CurrentChanged { actual: Option<AccountId> },
    #[error("unknown account {0}")]
    UnknownAccount(AccountId),
    /// Target failed re-validation at commit time.
    #[error("target {account} ineligible: {reason:?}")]
    TargetIneligible {
        account: AccountId,
        reason: select::IneligibleReason,
    },
}

impl PoolState {
    /// Build initial state from config; all accounts healthy with no windows
    /// (cold account = immediately eligible).
    pub fn from_accounts(accounts: &[AccountConfig]) -> Self {
        Self {
            accounts: accounts.iter().map(AccountState::fresh).collect(),
            current: BTreeMap::new(),
        }
    }

    fn account_mut(&mut self, account: &AccountId) -> Option<&mut AccountState> {
        self.accounts.iter_mut().find(|a| &a.id == account)
    }

    /// Record rate-limit headers from an upstream response. Freshest
    /// `fetched_at` wins per window; header data also self-heals a
    /// `Heuristic` cooldown when it shows capacity. When no unified windows
    /// are present, the most-constrained standard (API-key) bucket is
    /// recorded into the 5h slot so API-key accounts get proactive
    /// scheduling too (the token bucket's short reset horizon means the
    /// reading expires quickly and degrades back to cold).
    pub fn record_headers(
        &mut self,
        account: &AccountId,
        parsed: &ParsedRateLimitHeaders,
        now: SystemTime,
    ) {
        let Some(acct) = self.account_mut(account) else {
            return;
        };
        let mut recorded = false;
        if let Some(reading) = parsed.five_hour {
            recorded |= AccountState::merge_window(
                &mut acct.five_hour,
                reading,
                now,
                WindowSource::Headers,
            );
        }
        if let Some(reading) = parsed.seven_day {
            recorded |= AccountState::merge_window(
                &mut acct.seven_day,
                reading,
                now,
                WindowSource::Headers,
            );
        }
        if parsed.five_hour.is_none() && parsed.seven_day.is_none() {
            if let Some(reading) = parsed.standard.and_then(|s| s.as_window_reading()) {
                recorded |= AccountState::merge_window(
                    &mut acct.five_hour,
                    reading,
                    now,
                    WindowSource::Headers,
                );
            }
        }
        if recorded {
            acct.maybe_self_heal(now);
        }
    }

    /// Record a `/api/oauth/usage` poll result. Same freshness merge as
    /// headers; fresh data showing capacity clears `Heuristic` cooldowns.
    pub fn record_usage(&mut self, account: &AccountId, usage: &UsageSnapshot, now: SystemTime) {
        let Some(acct) = self.account_mut(account) else {
            return;
        };
        let mut recorded = false;
        if let Some(reading) = usage.five_hour {
            recorded |= AccountState::merge_window(
                &mut acct.five_hour,
                reading,
                now,
                WindowSource::UsagePoll,
            );
        }
        if let Some(reading) = usage.seven_day {
            recorded |= AccountState::merge_window(
                &mut acct.seven_day,
                reading,
                now,
                WindowSource::UsagePoll,
            );
        }
        if recorded {
            acct.maybe_self_heal(now);
        }
    }

    /// Record an upstream 429. With `retry_after` the account parks exactly
    /// that long (`CooldownSource::RetryAfter`); without it a default
    /// heuristic cooldown applies.
    pub fn record_429(
        &mut self,
        account: &AccountId,
        retry_after: Option<Duration>,
        now: SystemTime,
    ) {
        let Some(acct) = self.account_mut(account) else {
            return;
        };
        let (duration, source) = match retry_after {
            Some(d) => (d, CooldownSource::RetryAfter),
            None => (DEFAULT_HEURISTIC_COOLDOWN, CooldownSource::Heuristic),
        };
        acct.cooldown_until = now.checked_add(duration);
        acct.cooldown_source = Some(source);
        acct.cooldown_set_at = Some(now);
    }

    /// Record an auth failure (second 401 after a forced refresh, or a
    /// failed refresh). Marks the account `AuthFailed` until re-login.
    pub fn record_auth_failure(&mut self, account: &AccountId) {
        if let Some(acct) = self.account_mut(account) {
            acct.health = AccountHealth::AuthFailed;
        }
    }

    /// Replace an account's credential after a successful OAuth refresh or a
    /// config reload; restores `Healthy` if it was `AuthFailed`.
    pub fn update_credential(&mut self, account: &AccountId, credential: AccountCredential) {
        if let Some(acct) = self.account_mut(account) {
            acct.credential = credential;
            if acct.health == AccountHealth::AuthFailed {
                acct.health = AccountHealth::Healthy;
            }
        }
    }

    /// Commit an account switch with compare-and-swap semantics: aborts with
    /// `CurrentChanged` if `current` differs from `expected_current` (another
    /// task already switched) and with `TargetIneligible` if the target
    /// stopped being eligible between selection and commit. Never cancels
    /// in-flight leases — they keep their pinned credential until Drop.
    pub fn commit_switch(
        &mut self,
        group: Option<BackendGroup>,
        expected_current: Option<&AccountId>,
        target: &AccountId,
        params: &select::SelectParams,
        now: SystemTime,
    ) -> Result<(), SwitchError> {
        let slot = group.unwrap_or(LEGACY_GROUP);
        if self.current.get(&slot) != expected_current {
            return Err(SwitchError::CurrentChanged {
                actual: self.current.get(&slot).cloned(),
            });
        }
        let snapshot = self.snapshot();
        let target_snapshot = snapshot
            .accounts
            .iter()
            .find(|a| &a.id == target)
            .ok_or_else(|| SwitchError::UnknownAccount(target.clone()))?;
        let headers_only = select::headers_only_mode(&snapshot, params, group, now);
        let heuristic_degraded = select::heuristic_degraded_mode(&snapshot, params, group, now);
        // The selector may legitimately pick a Heuristic-parked account in
        // heuristic-degraded mode (transient-429 lockout recovery); the commit
        // re-validation must use the SAME gate so it does not reject what
        // `pick` just chose.
        if let Some(reason) = select::gate(
            target_snapshot,
            params,
            now,
            headers_only,
            heuristic_degraded,
        ) {
            return Err(SwitchError::TargetIneligible {
                account: target.clone(),
                reason,
            });
        }
        self.current.insert(slot, target.clone());
        Ok(())
    }

    /// Immutable snapshot for the pure selector and for `/llmux/status`.
    pub fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot {
            accounts: self
                .accounts
                .iter()
                .map(|a| AccountSnapshot {
                    id: a.id.clone(),
                    healthy: a.health == AccountHealth::Healthy,
                    credential_kind: a.credential.kind(),
                    group: BackendGroup::from_kind(a.credential.kind()),
                    five_hour: a.five_hour,
                    seven_day: a.seven_day,
                    cooldown_until: a.cooldown_until,
                    cooldown_source: a.cooldown_source,
                    in_flight: a.in_flight,
                    token_expires_at_ms: match &a.credential {
                        AccountCredential::Oauth { expires_at_ms, .. }
                        | AccountCredential::Codex { expires_at_ms, .. }
                            if *expires_at_ms > 0 =>
                        {
                            Some(*expires_at_ms)
                        }
                        _ => None,
                    },
                    last_refresh_ms: a.credential.last_refresh_ms(),
                })
                .collect(),
            current: self.current.clone(),
        }
    }
}

impl PoolSnapshot {
    /// The current account for one backend group, if any.
    pub fn current_for_group(&self, group: BackendGroup) -> Option<&AccountId> {
        self.current.get(&group)
    }

    /// The current account for the legacy / group-less path (routing
    /// disabled): the [`LEGACY_GROUP`] slot.
    pub fn legacy_current(&self) -> Option<&AccountId> {
        self.current.get(&LEGACY_GROUP)
    }

    /// A single representative current account for scalar status output and
    /// for the many display readers that show "the" active account: the
    /// claude-group slot if present, else the codex-group slot. With routing
    /// disabled this is exactly the legacy current.
    pub fn representative_current(&self) -> Option<&AccountId> {
        self.current
            .get(&BackendGroup::Claude)
            .or_else(|| self.current.get(&BackendGroup::Codex))
    }

    /// Whether `id` is the current account in ANY group — the predicate the
    /// display layer uses to mark the active row(s).
    pub fn is_current(&self, id: &AccountId) -> bool {
        self.current.values().any(|c| c == id)
    }
}

/// Read-only projection of one account for selection / status.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSnapshot {
    pub id: AccountId,
    pub healthy: bool,
    pub credential_kind: &'static str,
    /// Backend group this account belongs to, derived from `credential_kind`
    /// (codex credential → Codex, oauth/apikey → Claude). The selector's
    /// group filter and per-group stickiness key off this.
    pub group: BackendGroup,
    pub five_hour: Option<QuotaWindow>,
    pub seven_day: Option<QuotaWindow>,
    pub cooldown_until: Option<SystemTime>,
    pub cooldown_source: Option<CooldownSource>,
    pub in_flight: u32,
    /// OAuth access-token expiry (epoch ms) for the dashboard's token-health
    /// column; `None` for API-key accounts and for oauth accounts whose
    /// expiry is unknown (`expires_at_ms == 0`).
    pub token_expires_at_ms: Option<u64>,
    /// When the access token was last successfully refreshed (epoch ms);
    /// `None` for API-key accounts and never-refreshed oauth accounts —
    /// rendered as "never" in the dashboard.
    pub last_refresh_ms: Option<u64>,
}

/// Read-only projection of the whole pool. `select::pick` takes this plus an
/// explicit `now` — it must never read the clock or any shared state itself.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PoolSnapshot {
    pub accounts: Vec<AccountSnapshot>,
    /// Current account per backend group (see [`PoolState::current`]).
    pub current: BTreeMap<BackendGroup, AccountId>,
}

/// Shared handle around `PoolState`. Cheap to clone; every method takes the
/// lock briefly and never does IO under it.
#[derive(Clone)]
pub struct AccountPool {
    inner: Arc<RwLock<PoolState>>,
}

impl std::fmt::Debug for AccountPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountPool").finish_non_exhaustive()
    }
}

/// No account is currently usable; carries the soonest reset so the proxy
/// can answer 429 + `retry-after`.
#[derive(Debug, thiserror::Error)]
#[error("no account available (retry after {retry_after:?})")]
pub struct NoAccountAvailable {
    pub retry_after: Option<Duration>,
}

impl AccountPool {
    pub fn new(accounts: &[AccountConfig]) -> Self {
        Self {
            inner: Arc::new(RwLock::new(PoolState::from_accounts(accounts))),
        }
    }

    pub fn snapshot(&self) -> PoolSnapshot {
        self.read().snapshot()
    }

    /// Credential of one account, cloned (the usage poller needs the live
    /// access token; the snapshot intentionally carries only the kind).
    pub fn credential(&self, account: &AccountId) -> Option<AccountCredential> {
        self.read()
            .accounts
            .iter()
            .find(|a| &a.id == account)
            .map(|a| a.credential.clone())
    }

    /// Lease the CURRENT account for one request within `group` (or the
    /// legacy single slot when `group` is `None`): clones its credential and
    /// increments `in_flight`. The lease pins the account for the request's
    /// lifetime — switching away never affects live leases. Errors when no
    /// account is selected for the group or the current one is hard-down
    /// (auth failure / active cooldown); threshold drift is left to the
    /// evaluation tick, per session stickiness.
    ///
    /// `params` lets the usability check honor heuristic-degraded mode: when
    /// the WHOLE group is parked by retry-after-less (Heuristic) 429s the
    /// selector picks the soonest-freed account, and this lease must accept it
    /// rather than re-reject it on the raw `cooldown_until`. RetryAfter parks
    /// and auth failure still refuse the lease.
    pub fn lease_for(
        &self,
        group: Option<BackendGroup>,
        params: &select::SelectParams,
    ) -> Result<AccountLease, NoAccountAvailable> {
        let slot = group.unwrap_or(LEGACY_GROUP);
        let now = SystemTime::now();
        let mut state = self.write();
        let no_account = |state: &PoolState| NoAccountAvailable {
            retry_after: select::soonest_reset(&state.snapshot(), now),
        };
        let Some(current) = state.current.get(&slot).cloned() else {
            return Err(no_account(&state));
        };
        let snapshot = state.snapshot();
        let headers_only = select::headers_only_mode(&snapshot, params, group, now);
        let heuristic_degraded = select::heuristic_degraded_mode(&snapshot, params, group, now);
        let unusable = match snapshot.accounts.iter().find(|a| a.id == current) {
            // Reuse the pure gate so the lease agrees with what the selector
            // chose. Auth failure and a RetryAfter cooldown always refuse; a
            // Heuristic cooldown refuses UNLESS the group is in degraded mode.
            // 5h/7d/staleness gates are evaluation-tick concerns, not a reason
            // to refuse a request already routed to the sticky current — so
            // ignore those reasons here, matching the prior behavior (which
            // refused only on health + active cooldown).
            Some(acct) => matches!(
                select::gate(acct, params, now, headers_only, heuristic_degraded),
                Some(select::IneligibleReason::AuthUnhealthy)
                    | Some(select::IneligibleReason::CoolingDown)
            ),
            None => true,
        };
        if unusable {
            return Err(no_account(&state));
        }
        // Re-borrow mutably now that the immutable checks are done.
        let Some(acct) = state.account_mut(&current) else {
            return Err(NoAccountAvailable { retry_after: None });
        };
        acct.in_flight = acct.in_flight.saturating_add(1);
        let lease = AccountLease {
            pool: Arc::clone(&self.inner),
            id: acct.id.clone(),
            credential: acct.credential.clone(),
        };
        Ok(lease)
    }

    /// Run the pure selector over a fresh snapshot and commit the resulting
    /// decision (CAS). Returns the decision actually applied. This is the
    /// ONLY entry point that changes `current` — called from the periodic
    /// re-evaluation tick and from the 429/ineligibility paths, never
    /// per-request. Snapshot, decision, and commit happen under ONE write
    /// lock, so the CAS cannot race (the CAS in `commit_switch` still guards
    /// direct external callers).
    pub fn evaluate(
        &self,
        group: Option<BackendGroup>,
        params: &select::SelectParams,
        now: SystemTime,
    ) -> select::Decision {
        let slot = group.unwrap_or(LEGACY_GROUP);
        let mut state = self.write();
        let snapshot = state.snapshot();
        let decision = select::pick(&snapshot, params, group, now);
        match &decision {
            select::Decision::Stay => decision,
            select::Decision::Switch { to } => {
                let expected = snapshot.current.get(&slot);
                match state.commit_switch(group, expected, to, params, now) {
                    Ok(()) => decision,
                    // Unreachable while the write lock is held (pick and
                    // commit see the same state) — degrade honestly anyway.
                    Err(err) => {
                        tracing::error!(?err, "commit_switch failed under evaluate lock");
                        select::Decision::Exhausted {
                            retry_after: select::soonest_reset(&snapshot, now),
                        }
                    }
                }
            }
            select::Decision::Exhausted { .. } => {
                // Nothing usable in this group: clear its slot so lease_for
                // refuses until a later evaluation finds capacity again.
                state.current.remove(&slot);
                decision
            }
        }
    }

    /// Manual switch to an explicit target (TUI `s`): validates eligibility
    /// via the same pure gate the selector uses and commits like
    /// [`PoolState::commit_switch`] — snapshot, gate, and commit happen under
    /// ONE write lock, so the CAS against `current` cannot race. Lease-guard
    /// semantics are preserved: in-flight requests keep their pinned
    /// account/credential until Drop; only NEW leases land on the target.
    pub fn switch_to(
        &self,
        target: &AccountId,
        params: &select::SelectParams,
        now: SystemTime,
    ) -> Result<(), SwitchError> {
        let mut state = self.write();
        // A manual switch lands the target into ITS OWN group's slot (derived
        // from the target's credential kind) — so switching a codex account
        // never displaces the claude slot and vice versa. An unknown target
        // is reported by commit_switch.
        let group = state
            .accounts
            .iter()
            .find(|a| &a.id == target)
            .map(|a| BackendGroup::from_kind(a.credential.kind()));
        let Some(group) = group else {
            return Err(SwitchError::UnknownAccount(target.clone()));
        };
        let expected = state.current.get(&group).cloned();
        state.commit_switch(Some(group), expected.as_ref(), target, params, now)
    }

    pub fn record_headers(
        &self,
        account: &AccountId,
        parsed: &ParsedRateLimitHeaders,
        now: SystemTime,
    ) {
        self.write().record_headers(account, parsed, now);
    }

    pub fn record_usage(&self, account: &AccountId, usage: &UsageSnapshot, now: SystemTime) {
        self.write().record_usage(account, usage, now);
    }

    pub fn record_429(&self, account: &AccountId, retry_after: Option<Duration>, now: SystemTime) {
        self.write().record_429(account, retry_after, now);
    }

    pub fn record_auth_failure(&self, account: &AccountId) {
        self.write().record_auth_failure(account);
    }

    pub fn update_credential(&self, account: &AccountId, credential: AccountCredential) {
        self.write().update_credential(account, credential);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, PoolState> {
        self.inner.read().expect("pool lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, PoolState> {
        self.inner.write().expect("pool lock poisoned")
    }

    /// Replace the account roster after a config reload (TUI `R`, `import`
    /// while running). Existing window/cooldown state is kept for accounts
    /// that survive (credentials refresh from config); leases on removed
    /// accounts drain naturally. A removed `current` clears the selection.
    pub fn reload_accounts(&self, accounts: &[AccountConfig]) {
        let mut state = self.write();
        let next: Vec<AccountState> = accounts
            .iter()
            .map(|config| {
                let id = AccountId(config.name.clone());
                match state.accounts.iter().find(|a| a.id == id) {
                    Some(existing) => {
                        let mut kept = existing.clone();
                        kept.credential = config.credential.clone();
                        kept
                    }
                    None => AccountState::fresh(config),
                }
            })
            .collect();
        // Drop any group slot whose current account no longer exists; other
        // slots keep their sticky selection.
        state
            .current
            .retain(|_, current| next.iter().any(|a| &a.id == current));
        state.accounts = next;
    }
}

/// Drop-based guard pinning one account for one in-flight request.
/// Holds a CLONE of the credential taken at lease time — a concurrent
/// credential refresh or switch does not change what this request sends.
pub struct AccountLease {
    pool: Arc<RwLock<PoolState>>,
    id: AccountId,
    credential: AccountCredential,
}

/// Manual impl: never print the pinned credential (it holds live secrets).
impl std::fmt::Debug for AccountLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountLease")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl AccountLease {
    pub fn account_id(&self) -> &AccountId {
        &self.id
    }

    pub fn credential(&self) -> &AccountCredential {
        &self.credential
    }
}

impl Drop for AccountLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.pool.write() {
            if let Some(acct) = state.accounts.iter_mut().find(|a| a.id == self.id) {
                acct.in_flight = acct.in_flight.saturating_sub(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use select::{Decision, IneligibleReason, SelectParams};

    const NOW_SECS: u64 = 1_000_000;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn now() -> SystemTime {
        at(NOW_SECS)
    }

    fn params() -> SelectParams {
        SelectParams {
            five_hour_max: 0.90,
            seven_day_max: 0.99,
            usage_max_age: Duration::from_secs(600),
        }
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

    fn apikey_account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            credential: AccountCredential::Apikey {
                api_key: format!("sk-ant-{name}"),
            },
        }
    }

    fn id(s: &str) -> AccountId {
        AccountId(s.to_string())
    }

    fn reading(utilization: f64, resets_at_secs: u64) -> WindowReading {
        WindowReading {
            utilization,
            resets_at: at(resets_at_secs),
        }
    }

    fn usage(five: Option<WindowReading>, seven: Option<WindowReading>) -> UsageSnapshot {
        UsageSnapshot {
            five_hour: five,
            seven_day: seven,
        }
    }

    #[test]
    fn from_accounts_starts_cold_and_healthy() {
        let state = PoolState::from_accounts(&[oauth_account("a"), apikey_account("b")]);
        assert_eq!(state.accounts.len(), 2);
        assert!(state.current.is_empty());
        for acct in &state.accounts {
            assert_eq!(acct.health, AccountHealth::Healthy);
            assert!(acct.five_hour.is_none());
            assert!(acct.seven_day.is_none());
            assert!(acct.cooldown_until.is_none());
            assert_eq!(acct.in_flight, 0);
        }
    }

    #[test]
    fn record_headers_merges_unified_windows() {
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        let parsed = ParsedRateLimitHeaders {
            five_hour: Some(reading(0.42, NOW_SECS + 3600)),
            seven_day: Some(reading(0.87, NOW_SECS + 86_400)),
            ..Default::default()
        };
        state.record_headers(&id("a"), &parsed, now());
        let acct = &state.accounts[0];
        assert_eq!(acct.five_hour.unwrap().utilization, 0.42);
        assert_eq!(acct.five_hour.unwrap().source, WindowSource::Headers);
        assert_eq!(acct.seven_day.unwrap().utilization, 0.87);
        assert_eq!(acct.five_hour.unwrap().fetched_at, now());
    }

    #[test]
    fn stale_observation_does_not_overwrite_fresher_one() {
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        let fresh = ParsedRateLimitHeaders {
            five_hour: Some(reading(0.50, NOW_SECS + 3600)),
            ..Default::default()
        };
        state.record_headers(&id("a"), &fresh, at(NOW_SECS + 100));
        // An older (out-of-order) observation must not win.
        let older = ParsedRateLimitHeaders {
            five_hour: Some(reading(0.10, NOW_SECS + 3600)),
            ..Default::default()
        };
        state.record_headers(&id("a"), &older, now());
        assert_eq!(state.accounts[0].five_hour.unwrap().utilization, 0.50);
    }

    #[test]
    fn standard_headers_feed_five_hour_slot_when_unified_absent() {
        let mut state = PoolState::from_accounts(&[apikey_account("k")]);
        let parsed = ParsedRateLimitHeaders {
            standard: Some(headers::StandardRateLimit {
                requests_limit: Some(100),
                requests_remaining: Some(20),
                requests_reset: Some(at(NOW_SECS + 60)),
                tokens_limit: None,
                tokens_remaining: None,
                tokens_reset: None,
            }),
            ..Default::default()
        };
        state.record_headers(&id("k"), &parsed, now());
        let window = state.accounts[0].five_hour.unwrap();
        assert!((window.utilization - 0.80).abs() < 1e-9);
        assert_eq!(window.resets_at, at(NOW_SECS + 60));
    }

    #[test]
    fn record_429_with_retry_after_parks_exactly() {
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        state.record_429(&id("a"), Some(Duration::from_secs(2)), now());
        let acct = &state.accounts[0];
        assert_eq!(acct.cooldown_until, Some(at(NOW_SECS + 2)));
        assert_eq!(acct.cooldown_source, Some(CooldownSource::RetryAfter));
    }

    #[test]
    fn record_429_without_retry_after_uses_heuristic_default() {
        // The transient (retry-after-less) park is SHORT: 8s, not the old 30s —
        // a retry-after-less 429 is a per-minute-window blip, and degraded-mode
        // selection serves the soonest-freed account meanwhile.
        assert_eq!(DEFAULT_HEURISTIC_COOLDOWN, Duration::from_secs(8));
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        state.record_429(&id("a"), None, now());
        let acct = &state.accounts[0];
        assert_eq!(
            acct.cooldown_until,
            Some(at(NOW_SECS + DEFAULT_HEURISTIC_COOLDOWN.as_secs()))
        );
        assert_eq!(acct.cooldown_source, Some(CooldownSource::Heuristic));
    }

    #[test]
    fn fresh_usage_with_capacity_clears_heuristic_cooldown() {
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        state.record_429(&id("a"), None, now());
        // Later poll shows both windows under 100%.
        state.record_usage(
            &id("a"),
            &usage(
                Some(reading(0.30, NOW_SECS + 3600)),
                Some(reading(0.50, NOW_SECS + 86_400)),
            ),
            at(NOW_SECS + 300),
        );
        let acct = &state.accounts[0];
        assert!(
            acct.cooldown_until.is_none(),
            "heuristic cooldown self-heals"
        );
        assert!(acct.cooldown_source.is_none());
    }

    #[test]
    fn same_instant_data_cannot_heal_its_own_cooldown() {
        // The 429 response itself carries headers; those must not clear the
        // cooldown the same response just set.
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        state.record_429(&id("a"), None, now());
        state.record_usage(
            &id("a"),
            &usage(Some(reading(0.30, NOW_SECS + 3600)), None),
            now(),
        );
        assert!(state.accounts[0].cooldown_until.is_some());
    }

    #[test]
    fn usage_at_full_capacity_does_not_heal() {
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        state.record_429(&id("a"), None, now());
        state.record_usage(
            &id("a"),
            &usage(Some(reading(1.0, NOW_SECS + 3600)), None),
            at(NOW_SECS + 300),
        );
        assert!(state.accounts[0].cooldown_until.is_some());
    }

    #[test]
    fn retry_after_park_is_never_healed_by_data() {
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        state.record_429(&id("a"), Some(Duration::from_secs(600)), now());
        state.record_usage(
            &id("a"),
            &usage(Some(reading(0.0, NOW_SECS + 3600)), None),
            at(NOW_SECS + 300),
        );
        assert!(
            state.accounts[0].cooldown_until.is_some(),
            "explicit retry-after park must run its full course"
        );
    }

    /// Construct a `QuotaWindow` directly for `maybe_self_heal` unit tests.
    fn window(utilization: f64, resets_at_secs: u64, fetched_at_secs: u64) -> QuotaWindow {
        QuotaWindow {
            utilization,
            resets_at: at(resets_at_secs),
            fetched_at: at(fetched_at_secs),
            source: WindowSource::UsagePoll,
        }
    }

    /// Park an account on a `Heuristic` cooldown set at `set_at`, returning it
    /// ready for direct `maybe_self_heal` calls.
    fn heuristic_parked(set_at: SystemTime) -> AccountState {
        let mut acct = AccountState::fresh(&oauth_account("a"));
        acct.cooldown_until = set_at.checked_add(DEFAULT_HEURISTIC_COOLDOWN);
        acct.cooldown_source = Some(CooldownSource::Heuristic);
        acct.cooldown_set_at = Some(set_at);
        acct
    }

    // SCHED-15(a): newer data with every present window under 100% clears a
    // Heuristic cooldown.
    #[test]
    fn maybe_self_heal_clears_heuristic_when_all_windows_have_capacity() {
        let mut acct = heuristic_parked(now());
        acct.five_hour = Some(window(0.30, NOW_SECS + 3600, NOW_SECS + 1));
        acct.seven_day = Some(window(0.99, NOW_SECS + 86_400, NOW_SECS + 1));
        acct.maybe_self_heal(at(NOW_SECS + 1));
        assert!(acct.cooldown_until.is_none());
        assert!(acct.cooldown_source.is_none());
        assert!(acct.cooldown_set_at.is_none());
    }

    // SCHED-15(b): a RetryAfter park is an explicit upstream instruction and is
    // never cleared by fresh capacity.
    #[test]
    fn maybe_self_heal_never_clears_retry_after_cooldown() {
        let mut acct = AccountState::fresh(&oauth_account("a"));
        acct.cooldown_until = Some(at(NOW_SECS + 600));
        acct.cooldown_source = Some(CooldownSource::RetryAfter);
        acct.cooldown_set_at = Some(now());
        acct.five_hour = Some(window(0.0, NOW_SECS + 3600, NOW_SECS + 1));
        acct.maybe_self_heal(at(NOW_SECS + 1));
        assert_eq!(acct.cooldown_until, Some(at(NOW_SECS + 600)));
        assert_eq!(acct.cooldown_source, Some(CooldownSource::RetryAfter));
    }

    // SCHED-15(c): strict-newer guard — data with `fetched_at == cooldown_set_at`
    // (here, `now == set_at`) must not heal; the 429's own response can't clear
    // the cooldown it just set.
    #[test]
    fn maybe_self_heal_requires_strictly_newer_than_cooldown_set_at() {
        let mut acct = heuristic_parked(now());
        acct.five_hour = Some(window(0.10, NOW_SECS + 3600, NOW_SECS));
        // now == cooldown_set_at -> not strictly newer.
        acct.maybe_self_heal(now());
        assert!(acct.cooldown_until.is_some());
        assert_eq!(acct.cooldown_source, Some(CooldownSource::Heuristic));
    }

    #[test]
    fn auth_failure_marks_and_credential_update_heals() {
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        state.record_auth_failure(&id("a"));
        assert_eq!(state.accounts[0].health, AccountHealth::AuthFailed);
        state.update_credential(
            &id("a"),
            AccountCredential::Oauth {
                account_uuid: "uuid-a".into(),
                access_token: "new-at".into(),
                refresh_token: "new-rt".into(),
                expires_at_ms: 9_999,
                tier: None,
                last_refresh_ms: None,
            },
        );
        assert_eq!(state.accounts[0].health, AccountHealth::Healthy);
        match &state.accounts[0].credential {
            AccountCredential::Oauth { access_token, .. } => assert_eq!(access_token, "new-at"),
            other => panic!("unexpected credential {other:?}"),
        }
    }

    /// Legacy (routing-disabled) current — these tests drive the `None`
    /// group, so the single legacy slot holds the selection.
    fn legacy(state: &PoolState) -> Option<AccountId> {
        state.current.get(&LEGACY_GROUP).cloned()
    }

    fn snap_legacy(pool: &AccountPool) -> Option<AccountId> {
        pool.snapshot().legacy_current().cloned()
    }

    #[test]
    fn commit_switch_cas_aborts_on_changed_current() {
        let mut state = PoolState::from_accounts(&[oauth_account("a"), oauth_account("b")]);
        state.current.insert(LEGACY_GROUP, id("a"));
        let err = state
            .commit_switch(None, None, &id("b"), &params(), now())
            .unwrap_err();
        assert_eq!(
            err,
            SwitchError::CurrentChanged {
                actual: Some(id("a")),
            }
        );
        assert_eq!(legacy(&state), Some(id("a")), "nothing mutated on abort");
    }

    #[test]
    fn commit_switch_rejects_unknown_target() {
        let mut state = PoolState::from_accounts(&[oauth_account("a")]);
        let err = state
            .commit_switch(None, None, &id("ghost"), &params(), now())
            .unwrap_err();
        assert_eq!(err, SwitchError::UnknownAccount(id("ghost")));
    }

    #[test]
    fn commit_switch_rejects_target_that_became_ineligible() {
        let mut state = PoolState::from_accounts(&[oauth_account("a"), oauth_account("b")]);
        // Target b 429s between selection and commit.
        state.record_429(&id("b"), Some(Duration::from_secs(60)), now());
        let err = state
            .commit_switch(None, None, &id("b"), &params(), now())
            .unwrap_err();
        assert_eq!(
            err,
            SwitchError::TargetIneligible {
                account: id("b"),
                reason: IneligibleReason::CoolingDown,
            }
        );
        assert!(legacy(&state).is_none());
    }

    #[test]
    fn commit_switch_applies_on_clean_cas() {
        let mut state = PoolState::from_accounts(&[oauth_account("a"), oauth_account("b")]);
        state.current.insert(LEGACY_GROUP, id("a"));
        let current = legacy(&state);
        state
            .commit_switch(None, current.as_ref(), &id("b"), &params(), now())
            .unwrap();
        assert_eq!(legacy(&state), Some(id("b")));
    }

    #[test]
    fn evaluate_initial_selection_commits() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        let decision = pool.evaluate(None, &params(), now());
        assert_eq!(decision, Decision::Switch { to: id("a") });
        assert_eq!(snap_legacy(&pool), Some(id("a")));
    }

    #[test]
    fn evaluate_stays_then_switches_on_429() {
        let pool = AccountPool::new(&[oauth_account("a"), oauth_account("b")]);
        // a wins the initial id tiebreak.
        assert_eq!(
            pool.evaluate(None, &params(), now()),
            Decision::Switch { to: id("a") }
        );
        assert_eq!(pool.evaluate(None, &params(), now()), Decision::Stay);
        pool.record_429(&id("a"), Some(Duration::from_secs(120)), now());
        assert_eq!(
            pool.evaluate(None, &params(), now()),
            Decision::Switch { to: id("b") }
        );
        assert_eq!(snap_legacy(&pool), Some(id("b")));
    }

    #[test]
    fn evaluate_exhausted_clears_current_and_reports_reset() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        assert_eq!(
            pool.evaluate(None, &params(), now()),
            Decision::Switch { to: id("a") }
        );
        pool.record_429(&id("a"), Some(Duration::from_secs(2)), now());
        assert_eq!(
            pool.evaluate(None, &params(), now()),
            Decision::Exhausted {
                retry_after: Some(Duration::from_secs(2)),
            }
        );
        assert!(snap_legacy(&pool).is_none());
        assert!(pool.lease_for(None, &params()).is_err());
        // After the park expires the account is selectable again.
        assert_eq!(
            pool.evaluate(None, &params(), at(NOW_SECS + 3)),
            Decision::Switch { to: id("a") }
        );
    }

    #[test]
    fn switch_to_eligible_target_commits_and_refuses_ineligible() {
        let pool = AccountPool::new(&[oauth_account("a"), oauth_account("b")]);
        pool.evaluate(None, &params(), now());
        assert_eq!(snap_legacy(&pool), Some(id("a")));

        // Eligible target: manual switch commits.
        pool.switch_to(&id("b"), &params(), now()).unwrap();
        assert_eq!(snap_legacy(&pool), Some(id("b")));

        // Ineligible target (parked by a 429): refused, current unchanged.
        pool.record_429(&id("a"), Some(Duration::from_secs(60)), now());
        let err = pool.switch_to(&id("a"), &params(), now()).unwrap_err();
        assert_eq!(
            err,
            SwitchError::TargetIneligible {
                account: id("a"),
                reason: IneligibleReason::CoolingDown,
            }
        );
        assert_eq!(snap_legacy(&pool), Some(id("b")));

        // Unknown target: refused.
        let err = pool.switch_to(&id("ghost"), &params(), now()).unwrap_err();
        assert_eq!(err, SwitchError::UnknownAccount(id("ghost")));
    }

    #[test]
    fn switch_to_does_not_cancel_in_flight_leases() {
        let pool = AccountPool::new(&[oauth_account("a"), oauth_account("b")]);
        pool.evaluate(None, &params(), now());
        let lease = pool.lease_for(None, &params()).unwrap();
        assert_eq!(lease.account_id(), &id("a"));

        pool.switch_to(&id("b"), &params(), now()).unwrap();
        assert_eq!(snap_legacy(&pool), Some(id("b")));
        assert_eq!(
            pool.snapshot().accounts[0].in_flight,
            1,
            "manual switch leaves the live lease pinned to a"
        );
        drop(lease);
        assert_eq!(pool.snapshot().accounts[0].in_flight, 0);
    }

    #[test]
    fn lease_pins_credential_across_switch_and_refresh() {
        let pool = AccountPool::new(&[oauth_account("a"), oauth_account("b")]);
        pool.evaluate(None, &params(), now());
        let lease = pool.lease_for(None, &params()).unwrap();
        assert_eq!(lease.account_id(), &id("a"));
        assert_eq!(pool.snapshot().accounts[0].in_flight, 1);

        // Concurrent refresh + switch must not affect the live lease.
        pool.update_credential(
            &id("a"),
            AccountCredential::Oauth {
                account_uuid: "uuid-a".into(),
                access_token: "rotated".into(),
                refresh_token: "rotated".into(),
                expires_at_ms: 1,
                tier: None,
                last_refresh_ms: None,
            },
        );
        pool.record_429(&id("a"), Some(Duration::from_secs(60)), now());
        pool.evaluate(None, &params(), now());
        assert_eq!(snap_legacy(&pool), Some(id("b")));
        match lease.credential() {
            AccountCredential::Oauth { access_token, .. } => {
                assert_eq!(access_token, "at-a", "lease keeps the credential clone");
            }
            other => panic!("unexpected credential {other:?}"),
        }
        assert_eq!(
            pool.snapshot().accounts[0].in_flight,
            1,
            "switching away does not yank the lease"
        );
        drop(lease);
        assert_eq!(pool.snapshot().accounts[0].in_flight, 0);
    }

    #[test]
    fn lease_for_without_selection_reports_soonest_reset() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        pool.record_429(&id("a"), Some(Duration::from_secs(3600)), SystemTime::now());
        let err = pool.lease_for(None, &params()).unwrap_err();
        assert!(err.retry_after.is_some());
    }

    #[test]
    fn lease_for_refuses_cooling_current() {
        let pool = AccountPool::new(&[oauth_account("a")]);
        pool.evaluate(None, &params(), now());
        // Park far in the future relative to the real clock used by lease_for.
        pool.record_429(&id("a"), Some(Duration::from_secs(3600)), SystemTime::now());
        assert!(pool.lease_for(None, &params()).is_err());
    }

    #[test]
    fn reload_preserves_state_and_clears_removed_current() {
        let pool = AccountPool::new(&[oauth_account("a"), oauth_account("b")]);
        pool.evaluate(None, &params(), now());
        assert_eq!(snap_legacy(&pool), Some(id("a")));
        pool.record_429(&id("b"), Some(Duration::from_secs(60)), now());

        // Drop a, keep b (cooldown must survive), add c.
        pool.reload_accounts(&[oauth_account("b"), oauth_account("c")]);
        let snapshot = pool.snapshot();
        assert!(
            snapshot.legacy_current().is_none(),
            "removed current is cleared"
        );
        let b = snapshot.accounts.iter().find(|a| a.id == id("b")).unwrap();
        assert!(b.cooldown_until.is_some(), "surviving account keeps state");
        assert!(snapshot.accounts.iter().any(|a| a.id == id("c")));
        assert!(!snapshot.accounts.iter().any(|a| a.id == id("a")));
    }

    // ---- per-group sticky (routing enabled) ----

    fn codex_account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            credential: AccountCredential::Codex {
                account_id: format!("acct-{name}"),
                access_token: format!("at-{name}"),
                refresh_token: format!("rt-{name}"),
                expires_at_ms: 0,
                last_refresh_ms: None,
            },
        }
    }

    #[test]
    fn evaluate_keeps_independent_current_per_group() {
        let pool = AccountPool::new(&[oauth_account("a"), codex_account("cx")]);
        // Selecting the claude group lands on the oauth account; selecting the
        // codex group lands on the codex account — independent slots.
        assert_eq!(
            pool.evaluate(Some(BackendGroup::Claude), &params(), now()),
            Decision::Switch { to: id("a") }
        );
        assert_eq!(
            pool.evaluate(Some(BackendGroup::Codex), &params(), now()),
            Decision::Switch { to: id("cx") }
        );
        let snapshot = pool.snapshot();
        assert_eq!(
            snapshot.current_for_group(BackendGroup::Claude),
            Some(&id("a"))
        );
        assert_eq!(
            snapshot.current_for_group(BackendGroup::Codex),
            Some(&id("cx"))
        );
    }

    #[test]
    fn group_filtered_evaluate_only_selects_in_group() {
        let pool = AccountPool::new(&[oauth_account("a"), codex_account("cx")]);
        // The codex group only ever selects the codex account, never the
        // claude one, even though it would win on id order.
        assert_eq!(
            pool.evaluate(Some(BackendGroup::Codex), &params(), now()),
            Decision::Switch { to: id("cx") }
        );
        // Lease for the codex group returns the codex account.
        let lease = pool
            .lease_for(Some(BackendGroup::Codex), &params())
            .unwrap();
        assert_eq!(lease.account_id(), &id("cx"));
    }

    #[test]
    fn empty_group_evaluates_to_exhausted() {
        // Only a claude account exists; the codex group has nothing.
        let pool = AccountPool::new(&[oauth_account("a")]);
        assert_eq!(
            pool.evaluate(Some(BackendGroup::Codex), &params(), now()),
            Decision::Exhausted { retry_after: None }
        );
        assert!(pool
            .lease_for(Some(BackendGroup::Codex), &params())
            .is_err());
        // The claude group is unaffected.
        assert_eq!(
            pool.evaluate(Some(BackendGroup::Claude), &params(), now()),
            Decision::Switch { to: id("a") }
        );
    }

    #[test]
    fn switch_to_sets_targets_own_group_slot() {
        let pool = AccountPool::new(&[oauth_account("a"), codex_account("cx")]);
        pool.evaluate(Some(BackendGroup::Claude), &params(), now());
        // Manually switching to the codex account sets the CODEX slot, leaving
        // the claude slot intact.
        pool.switch_to(&id("cx"), &params(), now()).unwrap();
        let snapshot = pool.snapshot();
        assert_eq!(
            snapshot.current_for_group(BackendGroup::Claude),
            Some(&id("a"))
        );
        assert_eq!(
            snapshot.current_for_group(BackendGroup::Codex),
            Some(&id("cx"))
        );
    }
}
