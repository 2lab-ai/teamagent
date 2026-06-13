//! PURE selection: eligibility gates + soonest-7d-reset ranking.
//!
//! Everything here is a deterministic function of `(&PoolSnapshot, &SelectParams,
//! now)`. No IO, no clock reads, no locks — unit-test heavy by design. The
//! impure half (snapshotting, CAS commit) lives in `scheduler::AccountPool`.

use std::cmp::Ordering;
use std::time::{Duration, SystemTime};

use super::{AccountId, AccountSnapshot, PoolSnapshot};
use crate::config::SchedulerConfig;
use crate::routing::BackendGroup;

/// Selection thresholds, derived from `SchedulerConfig`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectParams {
    /// 5h utilization ceiling (default 0.90).
    pub five_hour_max: f64,
    /// 7d utilization ceiling (default 0.99).
    pub seven_day_max: f64,
    /// Usage data older than this makes an account ineligible — unless ALL
    /// accounts are stale, in which case selection falls back to
    /// headers-only mode (429-driven, the always-true path).
    pub usage_max_age: Duration,
}

impl From<&SchedulerConfig> for SelectParams {
    fn from(cfg: &SchedulerConfig) -> Self {
        Self {
            five_hour_max: cfg.five_hour_max,
            seven_day_max: cfg.seven_day_max,
            usage_max_age: Duration::from_secs(cfg.usage_max_age_secs),
        }
    }
}

/// What the scheduler should do, as decided by `pick`.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Current account is still eligible — session stickiness, don't move.
    Stay,
    /// Switch to `to` (CAS-committed against the snapshot's `current`).
    Switch { to: AccountId },
    /// Every account is ineligible. `retry_after` is the time until the
    /// soonest window reset, for the client-facing 429.
    Exhausted { retry_after: Option<Duration> },
}

/// Why an account was skipped — surfaced in `/teamagent/status` and the TUI
/// so operators can see WHY the scheduler did what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IneligibleReason {
    AuthUnhealthy,
    CoolingDown,
    FiveHourOverThreshold,
    SevenDayOverThreshold,
    UsageStale,
}

/// Single-account eligibility gate (FR3 step 1). Returns the first failing
/// gate, or `None` if eligible. `headers_only` is the all-stale fallback
/// mode in which staleness stops gating.
///
/// Boundary semantics: a window AT the threshold is still eligible
/// (`utilization <= max`); only strictly-over crosses the gate. A missing
/// window is a cold account — utilization 0, immediately eligible.
pub fn eligibility(
    account: &AccountSnapshot,
    params: &SelectParams,
    now: SystemTime,
    headers_only: bool,
) -> Option<IneligibleReason> {
    if !account.healthy {
        return Some(IneligibleReason::AuthUnhealthy);
    }
    if account.cooldown_until.is_some_and(|until| until > now) {
        return Some(IneligibleReason::CoolingDown);
    }
    let five = account
        .five_hour
        .map_or(0.0, |w| w.effective_utilization(now));
    if five > params.five_hour_max {
        return Some(IneligibleReason::FiveHourOverThreshold);
    }
    let seven = account
        .seven_day
        .map_or(0.0, |w| w.effective_utilization(now));
    if seven > params.seven_day_max {
        return Some(IneligibleReason::SevenDayOverThreshold);
    }
    // Codex accounts are exempt from the staleness gate: there is no usage
    // poller for codex (usage.rs polls oauth only) — `x-codex-*` response
    // headers are their ONLY window source and are legitimately old between
    // requests. Gating on age would park an idle codex account forever.
    let staleness_applies = account.credential_kind != "codex";
    if !headers_only && staleness_applies && usage_is_stale(account, now, params.usage_max_age) {
        return Some(IneligibleReason::UsageStale);
    }
    None
}

/// True when the account's freshest LIVE (non-expired) window observation is
/// older than `max_age`. An account with no live observations at all is cold,
/// not stale — expired windows carry no constraint and old-but-expired data
/// degrades to cold rather than blocking the account.
fn usage_is_stale(account: &AccountSnapshot, now: SystemTime, max_age: Duration) -> bool {
    match freshest_live_fetch(account, now) {
        Some(fetched_at) => now
            .duration_since(fetched_at)
            .is_ok_and(|age| age > max_age),
        None => false,
    }
}

/// Freshest `fetched_at` among the account's live (non-expired) windows —
/// the timestamp staleness is judged against, also surfaced as "usage stale
/// Xm" in blocking reasons.
pub fn freshest_live_fetch(account: &AccountSnapshot, now: SystemTime) -> Option<SystemTime> {
    [account.five_hour, account.seven_day]
        .into_iter()
        .flatten()
        .filter(|w| !w.is_expired(now))
        .map(|w| w.fetched_at)
        .max()
}

/// Headers-only degraded mode (FR3 staleness fallback): active when NO
/// account passes the full gate but at least one fails ONLY on staleness.
/// Then the staleness gate is dropped and scheduling falls back to
/// 429-driven behavior — better to try a stale account (upstream will 429 if
/// it's actually exhausted) than to refuse service because the usage
/// endpoint is down.
///
/// `group` scopes the decision to one backend group when `Some` (routing on)
/// — staleness in the codex group must not flip the claude group into
/// headers-only mode and vice versa. `None` (routing off) considers every
/// account, exactly as before.
pub fn headers_only_mode(
    snapshot: &PoolSnapshot,
    params: &SelectParams,
    group: Option<BackendGroup>,
    now: SystemTime,
) -> bool {
    let mut any_eligible = false;
    let mut any_stale_only = false;
    for account in &snapshot.accounts {
        if !in_group(account, group) {
            continue;
        }
        match eligibility(account, params, now, false) {
            None => any_eligible = true,
            Some(IneligibleReason::UsageStale) => any_stale_only = true,
            Some(_) => {}
        }
    }
    !any_eligible && any_stale_only
}

/// Whether an account is in the selection scope: every account when `group`
/// is `None` (routing off), only same-group accounts when `Some`.
fn in_group(account: &AccountSnapshot, group: Option<BackendGroup>) -> bool {
    group.is_none_or(|g| account.group == g)
}

/// THE selection algorithm (FR3): gate, then rank eligible accounts by
/// soonest 7d `resets_at` (use-it-or-lose-it), tiebreak lower 5h effective
/// utilization, then stable id order.
///
/// Stickiness contract: if the group's `current` is still eligible this
/// returns `Stay` even when another account would rank higher. A missing 5h
/// window means a cold account (utilization 0, immediately eligible).
///
/// `group` scopes selection to one backend group when `Some` (routing on):
/// only same-group accounts are eligible, stickiness keys off that group's
/// current slot, and the codex `tier`-last rank rule becomes a no-op (every
/// candidate is already in-group). `None` (routing off) keeps the legacy
/// behavior: all accounts considered, the single legacy current slot, codex
/// kept as the cross-group overflow tier.
pub fn pick(
    snapshot: &PoolSnapshot,
    params: &SelectParams,
    group: Option<BackendGroup>,
    now: SystemTime,
) -> Decision {
    let headers_only = headers_only_mode(snapshot, params, group, now);
    let eligible: Vec<&AccountSnapshot> = snapshot
        .accounts
        .iter()
        .filter(|a| in_group(a, group))
        .filter(|a| eligibility(a, params, now, headers_only).is_none())
        .collect();

    if let Some(current) = group_current(snapshot, group) {
        if eligible.iter().any(|a| &a.id == current) {
            return Decision::Stay;
        }
    }

    match eligible.into_iter().min_by(|a, b| rank(a, b, group, now)) {
        Some(best) => Decision::Switch {
            to: best.id.clone(),
        },
        None => Decision::Exhausted {
            retry_after: soonest_reset(snapshot, now),
        },
    }
}

/// The current account for the active selection scope: the group's slot when
/// `Some`, the legacy slot when `None`.
fn group_current(snapshot: &PoolSnapshot, group: Option<BackendGroup>) -> Option<&AccountId> {
    match group {
        Some(g) => snapshot.current_for_group(g),
        None => snapshot.legacy_current(),
    }
}

/// Ranking comparator: provider tier first (codex accounts are the overflow
/// pool — they have no Anthropic quota windows and must never be auto-picked
/// over a healthy anthropic account; manual TUI switch still works), then
/// min 7d `resets_at` (a window about to reset must be exhausted before its
/// leftover quota evaporates), then lower 5h effective utilization, then
/// stable id. Accounts with no live 7d window rank AFTER accounts with a
/// known reset — unknown expiry can't be use-it-or-lose-it prioritized.
///
/// Under a group filter (`group.is_some()`, routing on) the codex `tier`-last
/// rule is a NO-OP: every candidate is already in the same group, so there is
/// no cross-group overflow to demote. It is kept for the `None` (legacy) path
/// where codex is the cross-group overflow tier.
fn rank(
    a: &AccountSnapshot,
    b: &AccountSnapshot,
    group: Option<BackendGroup>,
    now: SystemTime,
) -> Ordering {
    if group.is_none() {
        let tier = |x: &AccountSnapshot| u8::from(x.credential_kind == "codex");
        let by_tier = tier(a).cmp(&tier(b));
        if by_tier != Ordering::Equal {
            return by_tier;
        }
    }
    let reset_a = live_reset(&a.seven_day, now);
    let reset_b = live_reset(&b.seven_day, now);
    let by_reset = match (reset_a, reset_b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    by_reset
        .then_with(|| {
            let five_a = a.five_hour.map_or(0.0, |w| w.effective_utilization(now));
            let five_b = b.five_hour.map_or(0.0, |w| w.effective_utilization(now));
            five_a.total_cmp(&five_b)
        })
        .then_with(|| a.id.cmp(&b.id))
}

fn live_reset(window: &Option<super::window::QuotaWindow>, now: SystemTime) -> Option<SystemTime> {
    window.filter(|w| !w.is_expired(now)).map(|w| w.resets_at)
}

/// THE display order (B1): indices into `snapshot.accounts`, sorted by the
/// scheduler's actual preference —
///
/// 1. the currently selected account first (even if it just became
///    ineligible: it is still serving leases until the next evaluation),
/// 2. then every eligible account in [`rank`] order (the literal order the
///    scheduler would pick them next),
/// 3. then ineligible accounts, grouped last, in stable config order.
///
/// Pure over the same inputs as [`pick`] and reusing the same `eligibility`
/// gate + `rank` comparator, so the TUI / status output can never disagree
/// with the selector.
pub fn selection_order(
    snapshot: &PoolSnapshot,
    params: &SelectParams,
    now: SystemTime,
) -> Vec<usize> {
    // The display lists ALL accounts (across groups), so it ranks with the
    // legacy comparator (codex overflow last) and treats an account as
    // "current" if it is the current pick for ANY group.
    let group = None;
    let headers_only = headers_only_mode(snapshot, params, group, now);
    let mut current: Vec<usize> = Vec::new();
    let mut eligible: Vec<usize> = Vec::new();
    let mut ineligible: Vec<usize> = Vec::new();
    for (idx, account) in snapshot.accounts.iter().enumerate() {
        if snapshot.is_current(&account.id) {
            current.push(idx);
        } else if eligibility(account, params, now, headers_only).is_none() {
            eligible.push(idx);
        } else {
            ineligible.push(idx);
        }
    }
    eligible.sort_by(|&a, &b| rank(&snapshot.accounts[a], &snapshot.accounts[b], group, now));
    current
        .into_iter()
        .chain(eligible)
        .chain(ineligible)
        .collect()
}

/// Human-readable blocking reason for an ineligible account, with the
/// concrete numbers an operator acts on: "cooldown 3m12s",
/// "7d 99.4% > 99%", "usage stale 14m03s", "auth failed". Shared by the TUI
/// status column and `/teamagent/status` so the wording never drifts.
pub fn blocking_reason(
    account: &AccountSnapshot,
    reason: IneligibleReason,
    params: &SelectParams,
    now: SystemTime,
) -> String {
    match reason {
        IneligibleReason::AuthUnhealthy => "auth failed".to_string(),
        IneligibleReason::CoolingDown => {
            match account
                .cooldown_until
                .and_then(|until| until.duration_since(now).ok())
            {
                Some(left) => format!("cooldown {}", compact_duration(left)),
                None => "cooldown".to_string(),
            }
        }
        IneligibleReason::FiveHourOverThreshold => {
            let util = account
                .five_hour
                .map_or(0.0, |w| w.effective_utilization(now));
            format!(
                "5h {:.1}% > {:.0}%",
                util * 100.0,
                params.five_hour_max * 100.0
            )
        }
        IneligibleReason::SevenDayOverThreshold => {
            let util = account
                .seven_day
                .map_or(0.0, |w| w.effective_utilization(now));
            format!(
                "7d {:.1}% > {:.0}%",
                util * 100.0,
                params.seven_day_max * 100.0
            )
        }
        IneligibleReason::UsageStale => {
            match freshest_live_fetch(account, now).and_then(|at| now.duration_since(at).ok()) {
                Some(age) => format!("usage stale {}", compact_duration(age)),
                None => "usage stale".to_string(),
            }
        }
    }
}

/// Compact no-space duration: "2d4h", "6h52m", "3m12s", "42s". Used in
/// blocking reasons and the dashboard's countdown cells.
pub fn compact_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (days, hours, mins, secs) = (
        total / 86_400,
        (total % 86_400) / 3_600,
        (total % 3_600) / 60,
        total % 60,
    );
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{secs:02}s")
    } else {
        format!("{secs}s")
    }
}

/// Time until the soonest moment any account could become usable: the
/// minimum over all future window `resets_at` AND future `cooldown_until`
/// across auth-healthy accounts — the `retry-after` answer when exhausted.
/// (A 2s 429-park is sooner than any window reset and must win.)
pub fn soonest_reset(snapshot: &PoolSnapshot, now: SystemTime) -> Option<Duration> {
    snapshot
        .accounts
        .iter()
        .filter(|a| a.healthy)
        .flat_map(|a| {
            a.five_hour
                .map(|w| w.resets_at)
                .into_iter()
                .chain(a.seven_day.map(|w| w.resets_at))
                .chain(a.cooldown_until)
        })
        .filter_map(|t| t.duration_since(now).ok())
        .filter(|d| *d > Duration::ZERO)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::window::{QuotaWindow, WindowSource};
    use crate::scheduler::CooldownSource;

    const HOUR: u64 = 3600;
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

    /// Fresh window observation (fetched "now", resets in the future).
    fn window(utilization: f64, resets_in_secs: u64) -> QuotaWindow {
        window_fetched(utilization, resets_in_secs, NOW_SECS)
    }

    fn window_fetched(utilization: f64, resets_in_secs: u64, fetched_secs: u64) -> QuotaWindow {
        QuotaWindow {
            utilization,
            resets_at: at(NOW_SECS + resets_in_secs),
            fetched_at: at(fetched_secs),
            source: WindowSource::UsagePoll,
        }
    }

    fn account(id: &str) -> AccountSnapshot {
        AccountSnapshot {
            id: AccountId(id.to_string()),
            healthy: true,
            credential_kind: "oauth",
            group: BackendGroup::Claude,
            five_hour: None,
            seven_day: None,
            cooldown_until: None,
            cooldown_source: None,
            in_flight: 0,
            token_expires_at_ms: None,
            last_refresh_ms: None,
        }
    }

    /// Build a snapshot whose legacy current slot is `current` (the
    /// routing-disabled path these tests exercise).
    fn pool(accounts: Vec<AccountSnapshot>, current: Option<&str>) -> PoolSnapshot {
        let mut map = std::collections::BTreeMap::new();
        if let Some(c) = current {
            map.insert(BackendGroup::Claude, AccountId(c.to_string()));
        }
        PoolSnapshot {
            accounts,
            current: map,
        }
    }

    fn id(s: &str) -> AccountId {
        AccountId(s.to_string())
    }

    // ---- eligibility gates ----

    #[test]
    fn cold_account_is_eligible() {
        assert_eq!(eligibility(&account("a"), &params(), now(), false), None);
    }

    #[test]
    fn five_hour_threshold_boundary() {
        let mut a = account("a");
        a.five_hour = Some(window(0.90, HOUR));
        assert_eq!(
            eligibility(&a, &params(), now(), false),
            None,
            "exactly at threshold is eligible"
        );
        a.five_hour = Some(window(0.9000001, HOUR));
        assert_eq!(
            eligibility(&a, &params(), now(), false),
            Some(IneligibleReason::FiveHourOverThreshold),
            "just over threshold is ineligible"
        );
    }

    #[test]
    fn seven_day_threshold_boundary() {
        let mut a = account("a");
        a.seven_day = Some(window(0.99, 24 * HOUR));
        assert_eq!(eligibility(&a, &params(), now(), false), None);
        a.seven_day = Some(window(0.991, 24 * HOUR));
        assert_eq!(
            eligibility(&a, &params(), now(), false),
            Some(IneligibleReason::SevenDayOverThreshold)
        );
    }

    #[test]
    fn expired_window_reads_as_cold() {
        let mut a = account("a");
        // 100% utilized, but the reset timestamp already passed.
        a.five_hour = Some(QuotaWindow {
            utilization: 1.0,
            resets_at: at(NOW_SECS - 1),
            fetched_at: at(NOW_SECS - 10),
            source: WindowSource::Headers,
        });
        assert_eq!(eligibility(&a, &params(), now(), false), None);
    }

    #[test]
    fn auth_unhealthy_gates_first() {
        let mut a = account("a");
        a.healthy = false;
        a.five_hour = Some(window(1.0, HOUR));
        a.cooldown_until = Some(at(NOW_SECS + 60));
        assert_eq!(
            eligibility(&a, &params(), now(), false),
            Some(IneligibleReason::AuthUnhealthy)
        );
    }

    #[test]
    fn cooldown_gates_until_it_expires() {
        let mut a = account("a");
        a.cooldown_until = Some(at(NOW_SECS + 1));
        a.cooldown_source = Some(CooldownSource::RetryAfter);
        assert_eq!(
            eligibility(&a, &params(), now(), false),
            Some(IneligibleReason::CoolingDown)
        );
        // At/after cooldown_until the park is over.
        assert_eq!(eligibility(&a, &params(), at(NOW_SECS + 1), false), None);
    }

    #[test]
    fn stale_usage_gates_unless_headers_only() {
        let mut a = account("a");
        // Fetched 601s ago (max age 600), still-live window.
        a.five_hour = Some(window_fetched(0.10, HOUR, NOW_SECS - 601));
        assert_eq!(
            eligibility(&a, &params(), now(), false),
            Some(IneligibleReason::UsageStale)
        );
        assert_eq!(
            eligibility(&a, &params(), now(), true),
            None,
            "headers-only mode drops the staleness gate"
        );
    }

    #[test]
    fn codex_accounts_are_exempt_from_the_staleness_gate() {
        // Same stale-but-live window shape as
        // `stale_usage_gates_unless_headers_only` — but on a codex account
        // it must NOT gate: codex has no usage poller, headers are its only
        // (and legitimately old) window source.
        let mut codex = account("cx");
        codex.credential_kind = "codex";
        codex.five_hour = Some(window_fetched(0.10, HOUR, NOW_SECS - 601));
        assert_eq!(eligibility(&codex, &params(), now(), false), None);
        // The quota gates themselves still apply to codex.
        codex.seven_day = Some(window_fetched(0.995, 24 * HOUR, NOW_SECS - 601));
        assert_eq!(
            eligibility(&codex, &params(), now(), false),
            Some(IneligibleReason::SevenDayOverThreshold)
        );
    }

    #[test]
    fn stale_but_expired_data_degrades_to_cold_not_stale() {
        let mut a = account("a");
        // Old observation whose window already reset: carries no constraint.
        a.five_hour = Some(QuotaWindow {
            utilization: 1.0,
            resets_at: at(NOW_SECS - HOUR),
            fetched_at: at(NOW_SECS - 2 * HOUR),
            source: WindowSource::Headers,
        });
        assert_eq!(eligibility(&a, &params(), now(), false), None);
    }

    // ---- ranking ----

    #[test]
    fn ranks_by_soonest_seven_day_reset() {
        let mut a = account("a");
        a.seven_day = Some(window(0.5, 48 * HOUR));
        let mut b = account("b");
        b.seven_day = Some(window(0.5, 12 * HOUR)); // resets sooner: use it or lose it
        let decision = pick(&pool(vec![a, b], None), &params(), None, now());
        assert_eq!(decision, Decision::Switch { to: id("b") });
    }

    #[test]
    fn tiebreaks_by_lower_five_hour_utilization() {
        let mut a = account("a");
        a.seven_day = Some(window(0.5, 24 * HOUR));
        a.five_hour = Some(window(0.60, HOUR));
        let mut b = account("b");
        b.seven_day = Some(window(0.5, 24 * HOUR));
        b.five_hour = Some(window(0.20, HOUR));
        let decision = pick(&pool(vec![a, b], None), &params(), None, now());
        assert_eq!(decision, Decision::Switch { to: id("b") });
    }

    #[test]
    fn final_tiebreak_is_stable_id_order() {
        let decision = pick(
            &pool(vec![account("bravo"), account("alpha")], None),
            &params(),
            None,
            now(),
        );
        assert_eq!(decision, Decision::Switch { to: id("alpha") });
    }

    #[test]
    fn known_seven_day_reset_ranks_before_cold_unknown() {
        let cold = account("aaa"); // would win an id tiebreak
        let mut known = account("zzz");
        known.seven_day = Some(window(0.5, 24 * HOUR));
        let decision = pick(&pool(vec![cold, known], None), &params(), None, now());
        assert_eq!(
            decision,
            Decision::Switch { to: id("zzz") },
            "account with a known expiring window is burned first"
        );
    }

    fn codex_account(id: &str) -> AccountSnapshot {
        let mut a = account(id);
        a.credential_kind = "codex";
        a.group = BackendGroup::Codex;
        a
    }

    #[test]
    fn codex_ranks_after_cold_anthropic_accounts() {
        // "aaa" would beat "zzz" on the id tiebreak — the codex tier must
        // override that: codex is the overflow pool, never the default pick.
        let codex = codex_account("aaa");
        let anthropic = account("zzz");
        let decision = pick(&pool(vec![codex, anthropic], None), &params(), None, now());
        assert_eq!(decision, Decision::Switch { to: id("zzz") });
    }

    #[test]
    fn codex_ranks_after_anthropic_with_known_resets() {
        let codex = codex_account("a-codex");
        let mut anthropic = account("b-known");
        anthropic.seven_day = Some(window(0.5, 48 * HOUR));
        let decision = pick(&pool(vec![codex, anthropic], None), &params(), None, now());
        assert_eq!(decision, Decision::Switch { to: id("b-known") });
    }

    #[test]
    fn codex_is_picked_when_every_anthropic_account_is_ineligible() {
        let codex = codex_account("codex");
        let mut over = account("anthropic");
        over.five_hour = Some(window(0.95, HOUR));
        let decision = pick(&pool(vec![codex, over], None), &params(), None, now());
        assert_eq!(
            decision,
            Decision::Switch { to: id("codex") },
            "overflow pool serves when anthropic quota is gone"
        );
    }

    #[test]
    fn codex_never_gates_on_staleness_or_windows() {
        let codex = codex_account("codex");
        assert_eq!(
            eligibility(&codex, &params(), now(), false),
            None,
            "no windows, no usage polling — always eligible while healthy"
        );
    }

    #[test]
    fn selection_order_puts_codex_after_eligible_anthropic() {
        let codex = codex_account("a-codex");
        let anthropic = account("b-anthropic");
        let snapshot = pool(vec![codex, anthropic], None);
        assert_eq!(ordered_ids(&snapshot), vec!["b-anthropic", "a-codex"]);
    }

    // ---- group filter (routing enabled) ----

    /// Build a snapshot whose per-group current slots are set explicitly.
    fn pool_with_groups(
        accounts: Vec<AccountSnapshot>,
        slots: &[(BackendGroup, &str)],
    ) -> PoolSnapshot {
        let mut current = std::collections::BTreeMap::new();
        for (g, c) in slots {
            current.insert(*g, AccountId(c.to_string()));
        }
        PoolSnapshot { accounts, current }
    }

    #[test]
    fn group_filter_picks_only_in_group_account() {
        // "aaa" (codex) would win id order, but the claude group must pick the
        // claude account; the codex group must pick the codex account.
        let codex = codex_account("aaa");
        let claude = account("zzz");
        let snapshot = pool(vec![codex, claude], None);
        assert_eq!(
            pick(&snapshot, &params(), Some(BackendGroup::Claude), now()),
            Decision::Switch { to: id("zzz") },
            "claude group ignores the codex account"
        );
        assert_eq!(
            pick(&snapshot, &params(), Some(BackendGroup::Codex), now()),
            Decision::Switch { to: id("aaa") },
            "codex group ignores the claude account"
        );
    }

    #[test]
    fn group_filter_codex_tier_rule_is_a_no_op_under_filter() {
        // Two codex accounts: under the codex group filter the tier rule is a
        // no-op, so ranking falls through to id order ("a" before "z").
        let a = codex_account("a");
        let z = codex_account("z");
        let snapshot = pool(vec![z, a], None);
        assert_eq!(
            pick(&snapshot, &params(), Some(BackendGroup::Codex), now()),
            Decision::Switch { to: id("a") }
        );
    }

    #[test]
    fn group_stickiness_is_independent_per_group() {
        // Claude slot points at the claude account, codex slot at the codex
        // account: each group stays on its own sticky pick even though a
        // higher-ranked account exists in the other group.
        let claude = account("claude-cur");
        let claude_alt = account("claude-alt");
        let codex = codex_account("codex-cur");
        let snapshot = pool_with_groups(
            vec![claude, claude_alt, codex],
            &[
                (BackendGroup::Claude, "claude-cur"),
                (BackendGroup::Codex, "codex-cur"),
            ],
        );
        assert_eq!(
            pick(&snapshot, &params(), Some(BackendGroup::Claude), now()),
            Decision::Stay,
            "claude group stays on its sticky current"
        );
        assert_eq!(
            pick(&snapshot, &params(), Some(BackendGroup::Codex), now()),
            Decision::Stay,
            "codex group stays on its sticky current, independent of claude"
        );
    }

    #[test]
    fn empty_group_is_exhausted() {
        // Only a claude account: the codex group has nothing to select.
        let snapshot = pool(vec![account("a")], None);
        assert_eq!(
            pick(&snapshot, &params(), Some(BackendGroup::Codex), now()),
            Decision::Exhausted { retry_after: None },
            "empty group exhausts without picking the other group"
        );
        // The claude group still selects fine.
        assert_eq!(
            pick(&snapshot, &params(), Some(BackendGroup::Claude), now()),
            Decision::Switch { to: id("a") }
        );
    }

    // ---- stickiness ----

    #[test]
    fn stays_on_eligible_current_even_when_outranked() {
        let mut current = account("a");
        current.seven_day = Some(window(0.5, 48 * HOUR));
        current.five_hour = Some(window(0.80, HOUR));
        let mut better = account("b");
        better.seven_day = Some(window(0.5, HOUR));
        better.five_hour = Some(window(0.10, HOUR));
        let decision = pick(
            &pool(vec![current, better], Some("a")),
            &params(),
            None,
            now(),
        );
        assert_eq!(decision, Decision::Stay);
    }

    #[test]
    fn switches_when_current_crosses_threshold() {
        let mut current = account("a");
        current.five_hour = Some(window(0.95, HOUR)); // over 0.90
        let fallback = account("b");
        let decision = pick(
            &pool(vec![current, fallback], Some("a")),
            &params(),
            None,
            now(),
        );
        assert_eq!(decision, Decision::Switch { to: id("b") });
    }

    #[test]
    fn switches_when_current_is_cooling_down() {
        let mut current = account("a");
        current.cooldown_until = Some(at(NOW_SECS + 120));
        let fallback = account("b");
        let decision = pick(
            &pool(vec![current, fallback], Some("a")),
            &params(),
            None,
            now(),
        );
        assert_eq!(decision, Decision::Switch { to: id("b") });
    }

    #[test]
    fn switches_when_current_auth_fails() {
        let mut current = account("a");
        current.healthy = false;
        let fallback = account("b");
        let decision = pick(
            &pool(vec![current, fallback], Some("a")),
            &params(),
            None,
            now(),
        );
        assert_eq!(decision, Decision::Switch { to: id("b") });
    }

    #[test]
    fn initial_selection_with_no_current_switches() {
        let decision = pick(&pool(vec![account("a")], None), &params(), None, now());
        assert_eq!(decision, Decision::Switch { to: id("a") });
    }

    // ---- all-stale degraded mode ----

    #[test]
    fn all_stale_enables_headers_only_mode() {
        let mut a = account("a");
        a.five_hour = Some(window_fetched(0.10, HOUR, NOW_SECS - 5000));
        let mut b = account("b");
        b.five_hour = Some(window_fetched(0.50, HOUR, NOW_SECS - 5000));
        let snapshot = pool(vec![a, b], None);
        assert!(headers_only_mode(&snapshot, &params(), None, now()));
        // Staleness gate dropped: scheduling proceeds on the stale data.
        assert_eq!(
            pick(&snapshot, &params(), None, now()),
            Decision::Switch { to: id("a") }
        );
    }

    #[test]
    fn stale_account_loses_to_fresh_cold_account() {
        let mut stale = account("a");
        stale.five_hour = Some(window_fetched(0.10, HOUR, NOW_SECS - 5000));
        let cold = account("b");
        let snapshot = pool(vec![stale, cold], None);
        assert!(!headers_only_mode(&snapshot, &params(), None, now()));
        assert_eq!(
            pick(&snapshot, &params(), None, now()),
            Decision::Switch { to: id("b") }
        );
    }

    #[test]
    fn staleness_gate_drops_when_it_is_the_only_blocker() {
        // A is genuinely over threshold, B is merely stale: serving B beats
        // refusing service (upstream 429 is the corrective backstop).
        let mut over = account("a");
        over.five_hour = Some(window(0.95, HOUR));
        let mut stale = account("b");
        stale.five_hour = Some(window_fetched(0.10, HOUR, NOW_SECS - 5000));
        let snapshot = pool(vec![over, stale], None);
        assert!(headers_only_mode(&snapshot, &params(), None, now()));
        assert_eq!(
            pick(&snapshot, &params(), None, now()),
            Decision::Switch { to: id("b") }
        );
    }

    #[test]
    fn sticky_current_survives_in_headers_only_mode() {
        let mut a = account("a");
        a.five_hour = Some(window_fetched(0.10, HOUR, NOW_SECS - 5000));
        let mut b = account("b");
        b.five_hour = Some(window_fetched(0.05, HOUR, NOW_SECS - 5000));
        let decision = pick(&pool(vec![a, b], Some("a")), &params(), None, now());
        assert_eq!(decision, Decision::Stay);
    }

    // ---- exhaustion + soonest reset ----

    #[test]
    fn all_exhausted_reports_soonest_reset() {
        let mut a = account("a");
        a.five_hour = Some(window(0.95, 3 * HOUR));
        a.seven_day = Some(window(0.5, 48 * HOUR));
        let mut b = account("b");
        b.five_hour = Some(window(0.99, 2 * HOUR)); // soonest future reset
        b.seven_day = Some(window(0.5, 24 * HOUR));
        let decision = pick(&pool(vec![a, b], None), &params(), None, now());
        assert_eq!(
            decision,
            Decision::Exhausted {
                retry_after: Some(Duration::from_secs(2 * HOUR)),
            }
        );
    }

    #[test]
    fn short_cooldown_park_wins_soonest_reset() {
        let mut a = account("a");
        a.five_hour = Some(window(0.95, 3 * HOUR));
        a.cooldown_until = Some(at(NOW_SECS + 2)); // 429 retry-after: 2
        let snapshot = pool(vec![a], None);
        assert_eq!(
            pick(&snapshot, &params(), None, now()),
            Decision::Exhausted {
                retry_after: Some(Duration::from_secs(2)),
            }
        );
    }

    #[test]
    fn unhealthy_accounts_do_not_contribute_resets() {
        let mut dead = account("a");
        dead.healthy = false;
        dead.five_hour = Some(window(1.0, 60)); // would be the soonest
        let mut parked = account("b");
        parked.cooldown_until = Some(at(NOW_SECS + 300));
        let snapshot = pool(vec![dead, parked], None);
        assert_eq!(
            soonest_reset(&snapshot, now()),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn empty_pool_is_exhausted_with_no_retry_hint() {
        assert_eq!(
            pick(&pool(vec![], None), &params(), None, now()),
            Decision::Exhausted { retry_after: None }
        );
    }

    // ---- selection order (B1) ----

    /// Pull the display-order ids out for assertion readability.
    fn ordered_ids(snapshot: &PoolSnapshot) -> Vec<String> {
        selection_order(snapshot, &params(), now())
            .into_iter()
            .map(|i| snapshot.accounts[i].id.0.clone())
            .collect()
    }

    #[test]
    fn order_is_current_then_rank_then_ineligible() {
        let mut current = account("cur");
        current.seven_day = Some(window(0.5, 72 * HOUR)); // worst rank of the eligibles
        let mut soon = account("soon");
        soon.seven_day = Some(window(0.5, 12 * HOUR)); // best rank
        let mut later = account("later");
        later.seven_day = Some(window(0.5, 48 * HOUR));
        let mut parked = account("parked");
        parked.cooldown_until = Some(at(NOW_SECS + 60));
        let mut dead = account("dead");
        dead.healthy = false;
        let snapshot = pool(vec![parked, later, soon, dead, current], Some("cur"));
        assert_eq!(
            ordered_ids(&snapshot),
            vec!["cur", "soon", "later", "parked", "dead"],
            "current first, eligibles by rank, ineligibles last in stable order"
        );
    }

    #[test]
    fn order_keeps_ineligible_current_first() {
        // The current account just crossed a threshold: the scheduler will
        // move off it on the next evaluation, but it IS still current.
        let mut current = account("cur");
        current.five_hour = Some(window(0.95, HOUR));
        let other = account("other");
        let snapshot = pool(vec![other, current], Some("cur"));
        assert_eq!(ordered_ids(&snapshot), vec!["cur", "other"]);
    }

    #[test]
    fn order_matches_pick_for_the_next_account() {
        let mut a = account("a");
        a.seven_day = Some(window(0.5, 48 * HOUR));
        let mut b = account("b");
        b.seven_day = Some(window(0.5, 12 * HOUR));
        let snapshot = pool(vec![a, b], None);
        let order = selection_order(&snapshot, &params(), now());
        let first = &snapshot.accounts[order[0]].id;
        assert_eq!(
            pick(&snapshot, &params(), None, now()),
            Decision::Switch { to: first.clone() },
            "head of the order is exactly what pick would choose"
        );
    }

    #[test]
    fn order_covers_every_account_exactly_once() {
        let snapshot = pool(vec![account("a"), account("b"), account("c")], Some("b"));
        let mut order = selection_order(&snapshot, &params(), now());
        order.sort_unstable();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn order_uses_headers_only_mode_like_pick() {
        // All accounts stale → staleness gate drops; both stay "eligible"
        // and are ranked instead of dumped into the ineligible tail.
        let mut a = account("a");
        a.five_hour = Some(window_fetched(0.50, HOUR, NOW_SECS - 5000));
        a.seven_day = Some(window_fetched(0.5, 48 * HOUR, NOW_SECS - 5000));
        let mut b = account("b");
        b.five_hour = Some(window_fetched(0.10, HOUR, NOW_SECS - 5000));
        b.seven_day = Some(window_fetched(0.5, 12 * HOUR, NOW_SECS - 5000));
        let snapshot = pool(vec![a, b], None);
        assert_eq!(ordered_ids(&snapshot), vec!["b", "a"], "ranked, not parked");
    }

    // ---- blocking reasons ----

    #[test]
    fn blocking_reason_strings_carry_the_numbers() {
        let mut parked = account("a");
        parked.cooldown_until = Some(at(NOW_SECS + 192));
        assert_eq!(
            blocking_reason(&parked, IneligibleReason::CoolingDown, &params(), now()),
            "cooldown 3m12s"
        );

        let mut over5 = account("b");
        over5.five_hour = Some(window(0.95, HOUR));
        assert_eq!(
            blocking_reason(
                &over5,
                IneligibleReason::FiveHourOverThreshold,
                &params(),
                now()
            ),
            "5h 95.0% > 90%"
        );

        let mut over7 = account("c");
        over7.seven_day = Some(window(0.994, 24 * HOUR));
        assert_eq!(
            blocking_reason(
                &over7,
                IneligibleReason::SevenDayOverThreshold,
                &params(),
                now()
            ),
            "7d 99.4% > 99%"
        );

        let mut stale = account("d");
        stale.five_hour = Some(window_fetched(0.10, HOUR, NOW_SECS - 14 * 60));
        assert_eq!(
            blocking_reason(&stale, IneligibleReason::UsageStale, &params(), now()),
            "usage stale 14m00s"
        );

        let mut dead = account("e");
        dead.healthy = false;
        assert_eq!(
            blocking_reason(&dead, IneligibleReason::AuthUnhealthy, &params(), now()),
            "auth failed"
        );
    }

    #[test]
    fn blocking_reason_degrades_without_timestamps() {
        let parked = account("a"); // CoolingDown claimed but no cooldown_until
        assert_eq!(
            blocking_reason(&parked, IneligibleReason::CoolingDown, &params(), now()),
            "cooldown"
        );
        let stale = account("b"); // no live observation to age
        assert_eq!(
            blocking_reason(&stale, IneligibleReason::UsageStale, &params(), now()),
            "usage stale"
        );
    }

    #[test]
    fn compact_duration_bands() {
        assert_eq!(compact_duration(Duration::from_secs(42)), "42s");
        assert_eq!(compact_duration(Duration::from_secs(192)), "3m12s");
        assert_eq!(
            compact_duration(Duration::from_secs(6 * 3600 + 52 * 60)),
            "6h52m"
        );
        assert_eq!(
            compact_duration(Duration::from_secs(2 * 86_400 + 4 * 3600)),
            "2d4h"
        );
        assert_eq!(compact_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn past_resets_are_ignored_by_soonest_reset() {
        let mut a = account("a");
        a.five_hour = Some(QuotaWindow {
            utilization: 1.0,
            resets_at: at(NOW_SECS - 100),
            fetched_at: at(NOW_SECS - 200),
            source: WindowSource::Headers,
        });
        a.seven_day = Some(window(0.5, 500));
        assert_eq!(
            soonest_reset(&pool(vec![a], None), now()),
            Some(Duration::from_secs(500))
        );
    }
}
