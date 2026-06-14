//! `DashboardView` — the one struct the draw code renders from, built from a
//! [`DashboardDoc`] regardless of where that document came from: the local
//! TUI builds it in-process from live `AppState` and the attach-mode client
//! parses it from `GET /llmux/dashboard` JSON. One contract, one
//! renderer — the rendering is never forked.

use std::collections::HashMap;
use std::str::FromStr as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::dashboard::{CompletedDoc, DashboardDoc, WindowDoc};
use crate::logging::LogLine;
use crate::scheduler::select::{self, SelectParams};
use crate::scheduler::window::{QuotaWindow, WindowSource};
use crate::scheduler::{AccountId, AccountSnapshot, CooldownSource, PoolSnapshot};

use super::activity::{Completed, CompletedBody, InFlight, Totals};
use super::{LastSwitch, PollHealth, TokenCounts};

/// Everything one frame renders. Owned (no borrow into app state) so a
/// remote document and live local state produce the identical input.
pub(crate) struct DashboardView {
    pub version: String,
    pub pid: u32,
    pub uptime: Duration,
    pub port: u16,
    pub upstream: Option<String>,
    pub config_path: Option<String>,
    pub select_params: SelectParams,
    pub refresh_ahead: Duration,
    pub evaluate_tick: Duration,
    pub snapshot: PoolSnapshot,
    pub last_switch: Option<LastSwitch>,
    pub poll_health: HashMap<String, PollHealth>,
    /// Per-account activity totals (table req/tok columns, detail pane).
    pub session_totals: HashMap<String, Totals>,
    pub global_totals: Totals,
    pub rpm_5m: f64,
    /// Oldest→newest (rendered reversed: newest start on top).
    pub in_flight: Vec<InFlight>,
    /// Newest first.
    pub completed: Vec<Completed>,
    /// Oldest→newest tail.
    pub logs: Vec<LogLine>,
    /// Per-(group, model) usage rows (req1-20), already sorted by total tokens.
    /// One representation — the serializable doc row — used by both the
    /// document and the renderer, so local and attach render identically.
    pub model_usage: Vec<crate::dashboard::ModelUsageDoc>,
    /// Live codex settings (req8.1): shown + toggled from the dashboard.
    pub codex: crate::dashboard::CodexSettingsDoc,
}

fn ms_time(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

fn secs_time(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Map a serialized credential kind back to the static str the scheduler's
/// pure functions compare against. Unknown kinds (newer server) degrade to a
/// label that matches no special case.
fn kind_static(kind: &str) -> &'static str {
    match kind {
        "oauth" => "oauth",
        "apikey" => "apikey",
        "codex" => "codex",
        _ => "unknown",
    }
}

fn window_from_doc(doc: &Option<WindowDoc>) -> Option<QuotaWindow> {
    doc.as_ref().map(|w| QuotaWindow {
        utilization: w.utilization,
        resets_at: secs_time(w.resets_at),
        fetched_at: ms_time(w.fetched_at_ms),
        source: match w.source.as_str() {
            "poll" => WindowSource::UsagePoll,
            _ => WindowSource::Headers,
        },
    })
}

impl DashboardView {
    pub(crate) fn from_doc(doc: &DashboardDoc) -> Self {
        let accounts: Vec<AccountSnapshot> = doc
            .accounts
            .iter()
            .map(|a| AccountSnapshot {
                id: AccountId(a.name.clone()),
                healthy: a.healthy,
                credential_kind: kind_static(&a.kind),
                group: crate::routing::BackendGroup::from_kind(kind_static(&a.kind)),
                five_hour: window_from_doc(&a.five_hour),
                seven_day: window_from_doc(&a.seven_day),
                cooldown_until: a.cooldown_until.map(secs_time),
                cooldown_source: a.cooldown_source.as_deref().map(|s| match s {
                    "retry_after" => CooldownSource::RetryAfter,
                    _ => CooldownSource::Heuristic,
                }),
                in_flight: a.in_flight,
                token_expires_at_ms: a.token_expires_at_ms,
                last_refresh_ms: a.last_refresh_ms,
            })
            .collect();
        // Rebuild the per-group current map. A current daemon sends the full
        // per-group map (`current_by_group`), so each group's sticky slot
        // renders independently (req1). Fall back to the representative scalar
        // (`current`) — placed into its own group's slot — for docs from an
        // older daemon that predates the map.
        let mut current = std::collections::BTreeMap::new();
        if !doc.current_by_group.is_empty() {
            for (label, name) in &doc.current_by_group {
                current.insert(
                    crate::routing::BackendGroup::from_label(label),
                    AccountId(name.clone()),
                );
            }
        } else if let Some(name) = &doc.current {
            let id = AccountId(name.clone());
            let group = accounts
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.group)
                .unwrap_or(crate::routing::BackendGroup::Claude);
            current.insert(group, id);
        }
        let snapshot = PoolSnapshot { accounts, current };
        let session_totals: HashMap<String, Totals> = doc
            .accounts
            .iter()
            .map(|a| {
                (
                    a.name.clone(),
                    Totals {
                        requests: a.session.requests,
                        ok: a.session.ok,
                        errors: a.session.errors,
                        tokens_in: a.session.tokens_in,
                        tokens_out: a.session.tokens_out,
                    },
                )
            })
            .collect();
        let poll_health: HashMap<String, PollHealth> = doc
            .poller
            .iter()
            .map(|p| {
                (
                    p.account.clone(),
                    PollHealth {
                        last_ok: p.last_ok_ms.map(ms_time),
                        consecutive_failures: p.consecutive_failures,
                        next_at: ms_time(p.next_at_ms),
                    },
                )
            })
            .collect();
        let in_flight = doc
            .activity
            .in_flight
            .iter()
            .map(|r| InFlight {
                id: r.id,
                method: r.method.clone(),
                path: r.path.clone(),
                account: r.account.clone(),
                // Per-model in-flight counts are precomputed server-side into
                // the model-usage rows, so the view's in-flight entries (used
                // only for the activity spinner) don't carry group/model.
                group: None,
                model: None,
                started_at: ms_time(r.started_at_ms),
            })
            .collect();
        let completed = doc
            .activity
            .completed
            .iter()
            .map(|entry| match entry {
                CompletedDoc::Request {
                    at_ms,
                    method,
                    path,
                    account,
                    status,
                    duration_ms,
                    tokens,
                    group,
                    model,
                    effort,
                } => Completed {
                    at: ms_time(*at_ms),
                    body: CompletedBody::Request {
                        method: method.clone(),
                        path: path.clone(),
                        account: account.clone(),
                        status: *status,
                        duration: Duration::from_millis(*duration_ms),
                        // The activity line shows only the in/out total; cache
                        // detail rides the model-usage rows, not these entries.
                        tokens: tokens.map(|t| TokenCounts {
                            input: t.input,
                            output: t.output,
                            ..Default::default()
                        }),
                        group: group.clone(),
                        model: model.clone(),
                        effort: effort.clone(),
                    },
                },
                CompletedDoc::Note { at_ms, text, error } => Completed {
                    at: ms_time(*at_ms),
                    body: CompletedBody::Note {
                        text: text.clone(),
                        error: *error,
                    },
                },
            })
            .collect();
        let logs = doc
            .logs
            .iter()
            .map(|line| LogLine {
                level: tracing::Level::from_str(&line.level).unwrap_or(tracing::Level::INFO),
                text: line.text.clone(),
            })
            .collect();

        Self {
            version: doc.version.clone(),
            pid: doc.pid,
            uptime: Duration::from_secs(doc.uptime_secs),
            port: doc.port,
            upstream: Some(doc.upstream.clone()).filter(|u: &String| !u.is_empty()),
            config_path: doc.config_path.clone(),
            select_params: SelectParams::from(&doc.select_params),
            refresh_ahead: Duration::from_secs(doc.refresh_ahead_secs),
            evaluate_tick: Duration::from_secs(doc.evaluate_tick_secs.max(1)),
            snapshot,
            last_switch: doc.scheduler.last_switch.as_ref().map(|s| LastSwitch {
                from: s.from.clone(),
                to: s.to.clone(),
                reason: s.reason.clone(),
                at: ms_time(s.at_ms),
            }),
            poll_health,
            session_totals,
            global_totals: Totals {
                requests: doc.totals.requests,
                ok: doc.totals.ok,
                errors: doc.totals.errors,
                tokens_in: doc.totals.tokens_in,
                tokens_out: doc.totals.tokens_out,
            },
            rpm_5m: doc.totals.rpm_5m,
            in_flight,
            completed,
            logs,
            model_usage: doc.model_usage.clone(),
            codex: doc.codex.clone(),
        }
    }

    /// Display order of the accounts table: indices into `snapshot.accounts`
    /// in the scheduler's preference order — same pure function the
    /// scheduler itself ranks with.
    pub(crate) fn display_order(&self, now: SystemTime) -> Vec<usize> {
        select::selection_order(&self.snapshot, &self.select_params, now)
    }

    pub(crate) fn totals_for(&self, account: &str) -> Totals {
        self.session_totals
            .get(account)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn poll_health(&self, account: &str) -> Option<PollHealth> {
        self.poll_health.get(account).copied()
    }

    /// "0.1.0 (channel id)" — the version string with the binary name
    /// stripped (the header already says whose version it is).
    pub(crate) fn display_version(&self) -> &str {
        self.version.strip_prefix("llmux ").unwrap_or(&self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_json() -> serde_json::Value {
        serde_json::json!({
            "version": "llmux 0.1.0 (dev dev)",
            "pid": 61282,
            "uptime_secs": 7980,
            "port": 3456,
            "current": "a",
            "upstream": "https://api.anthropic.com",
            "config_path": "/home/u/.config/llmux/llmux.json",
            "select_params": { "five_hour_max": 0.90, "seven_day_max": 0.99, "usage_max_age_secs": 600 },
            "refresh_ahead_secs": 25200,
            "evaluate_tick_secs": 60,
            "accounts": [
                {
                    "name": "a", "type": "oauth", "status": "active", "order": 1,
                    "blocked": null, "healthy": true,
                    "five_hour": { "utilization": 0.42, "resets_at": 1_003_600u64,
                                   "resets_in_secs": 3600, "fetched_at_ms": 1_000_000_000u64,
                                   "source": "headers" },
                    "seven_day": null,
                    "cooldown_until": null, "cooldown_source": null,
                    "in_flight": 1,
                    "token_expires_at_ms": 1_003_600_000u64, "last_refresh_ms": 999_820_000u64,
                    "totals": { "requests": 3, "input_tokens": 100, "output_tokens": 50 },
                    "session": { "requests": 3, "ok": 2, "errors": 1, "tokens_in": 100, "tokens_out": 50 },
                },
                {
                    "name": "b", "type": "apikey", "status": "cooldown", "order": 2,
                    "blocked": "cooldown 2m00s", "healthy": true,
                    "five_hour": null, "seven_day": null,
                    "cooldown_until": 1_000_120u64, "cooldown_source": "retry_after",
                    "in_flight": 0,
                    "token_expires_at_ms": null, "last_refresh_ms": null,
                    "totals": { "requests": 0, "input_tokens": 0, "output_tokens": 0 },
                    "session": { "requests": 0, "ok": 0, "errors": 0, "tokens_in": 0, "tokens_out": 0 },
                },
            ],
            "scheduler": {
                "last_switch": { "from": null, "to": "a", "reason": "initial selection",
                                 "at_ms": 999_910_000u64 },
                "next_in_line": null,
                "next_eval_in_secs": 42,
            },
            "poller": [
                { "account": "a", "last_ok_ms": 999_990_000u64, "consecutive_failures": 0,
                  "next_at_ms": 1_000_290_000u64 },
            ],
            "totals": { "requests": 3, "ok": 2, "errors": 1, "tokens_in": 100,
                        "tokens_out": 50, "rpm_5m": 0.6, "in_flight": 1 },
            "model_usage": [
                { "group": "claude", "model": "claude-sonnet-4-5", "requests": 3,
                  "ok": 2, "errors": 1, "tokens_in": 100, "tokens_out": 50,
                  "cache_read": 4000, "last_used_ms": 999_940_000u64, "in_flight": 1,
                  "accounts": [ { "name": "a", "requests": 3, "ok": 2, "errors": 1,
                                  "tokens_in": 100, "tokens_out": 50 } ],
                  "efforts": [ { "label": "16k", "requests": 1 },
                               { "label": "none", "requests": 2 } ],
                  "endpoints": [ { "label": "messages", "requests": 3 } ] },
            ],
            "activity": {
                "in_flight": [
                    { "id": 7, "method": "POST", "path": "/v1/messages", "account": "a",
                      "started_at_ms": 999_997_000u64 },
                ],
                "completed": [
                    { "kind": "request", "at_ms": 999_940_000u64, "method": "POST",
                      "path": "/v1/messages", "account": "a", "status": 200,
                      "duration_ms": 1400, "tokens": { "input": 70, "output": 30 } },
                    { "kind": "note", "at_ms": 999_910_000u64,
                      "text": "switch (none) → a (initial selection)", "error": false },
                ],
            },
            "logs": [
                { "level": "INFO", "text": "proxy: proxy listening" },
                { "level": "ERROR", "text": "refresh: token dead" },
            ],
        })
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000)
    }

    #[test]
    fn from_doc_rebuilds_per_group_current_from_map() {
        use crate::routing::BackendGroup;
        let mut json = doc_json();
        // A codex account joins the roster, and the doc carries a per-group
        // current map with BOTH slots (what a current daemon emits).
        json["accounts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "name": "c", "type": "codex", "status": "active", "order": 3,
                "blocked": null, "healthy": true,
                "five_hour": null, "seven_day": null,
                "cooldown_until": null, "cooldown_source": null,
                "in_flight": 0,
                "token_expires_at_ms": null, "last_refresh_ms": null,
                "totals": { "requests": 0, "input_tokens": 0, "output_tokens": 0 },
                "session": { "requests": 0, "ok": 0, "errors": 0, "tokens_in": 0, "tokens_out": 0 },
            }));
        json["current_by_group"] = serde_json::json!({ "claude": "a", "codex": "c" });

        let doc: DashboardDoc = serde_json::from_value(json).expect("parse doc");
        let view = DashboardView::from_doc(&doc);

        assert_eq!(
            view.snapshot.current_for_group(BackendGroup::Claude),
            Some(&AccountId("a".into()))
        );
        assert_eq!(
            view.snapshot.current_for_group(BackendGroup::Codex),
            Some(&AccountId("c".into()))
        );
        assert!(view.snapshot.is_current(&AccountId("c".into())));
    }

    #[test]
    fn from_doc_falls_back_to_scalar_current_when_map_absent() {
        use crate::routing::BackendGroup;
        // Legacy daemon: no current_by_group, only the scalar `current`.
        let doc: DashboardDoc = serde_json::from_value(doc_json()).expect("parse doc");
        assert!(doc.current_by_group.is_empty());
        let view = DashboardView::from_doc(&doc);
        // "a" is oauth (claude) → lands in the claude slot; codex stays empty.
        assert_eq!(
            view.snapshot.current_for_group(BackendGroup::Claude),
            Some(&AccountId("a".into()))
        );
        assert_eq!(view.snapshot.current_for_group(BackendGroup::Codex), None);
    }

    #[test]
    fn view_model_builds_from_fetched_json() {
        let doc: DashboardDoc = serde_json::from_value(doc_json()).expect("parse doc");
        let view = DashboardView::from_doc(&doc);

        assert_eq!(view.pid, 61282);
        assert_eq!(view.port, 3456);
        assert_eq!(view.uptime, Duration::from_secs(7980));
        assert_eq!(view.display_version(), "0.1.0 (dev dev)");
        assert_eq!(
            view.snapshot.representative_current(),
            Some(&AccountId("a".into()))
        );

        let a = &view.snapshot.accounts[0];
        assert_eq!(a.credential_kind, "oauth");
        assert!(a.healthy);
        let five = a.five_hour.expect("window");
        assert!((five.utilization - 0.42).abs() < 1e-9);
        assert_eq!(five.resets_at, UNIX_EPOCH + Duration::from_secs(1_003_600));
        assert_eq!(five.fetched_at, now());
        assert_eq!(five.source, WindowSource::Headers);
        assert_eq!(a.token_expires_at_ms, Some(1_003_600_000));
        assert_eq!(a.in_flight, 1);

        let b = &view.snapshot.accounts[1];
        assert_eq!(b.credential_kind, "apikey");
        assert_eq!(
            b.cooldown_until,
            Some(UNIX_EPOCH + Duration::from_secs(1_000_120))
        );
        assert_eq!(b.cooldown_source, Some(CooldownSource::RetryAfter));

        // The pure scheduler functions run on the rebuilt snapshot: the
        // parked account gates exactly like it does server-side.
        assert_eq!(
            select::eligibility(b, &view.select_params, now(), false),
            Some(crate::scheduler::select::IneligibleReason::CoolingDown)
        );
        assert_eq!(view.display_order(now()), vec![0, 1]);

        assert_eq!(view.totals_for("a").ok, 2);
        assert_eq!(view.global_totals.errors, 1);
        assert!((view.rpm_5m - 0.6).abs() < 1e-9);

        let poll = view.poll_health("a").expect("poll health");
        assert_eq!(poll.consecutive_failures, 0);
        assert_eq!(
            poll.last_ok,
            Some(UNIX_EPOCH + Duration::from_millis(999_990_000))
        );

        assert_eq!(view.in_flight.len(), 1);
        assert_eq!(view.in_flight[0].account.as_deref(), Some("a"));
        assert_eq!(view.completed.len(), 2);
        match &view.completed[0].body {
            CompletedBody::Request {
                status,
                duration,
                tokens,
                ..
            } => {
                assert_eq!(*status, 200);
                assert_eq!(*duration, Duration::from_millis(1400));
                assert_eq!(
                    *tokens,
                    Some(TokenCounts {
                        input: 70,
                        output: 30,
                        ..Default::default()
                    })
                );
            }
            other => panic!("expected request, got {other:?}"),
        }
        assert_eq!(view.logs.len(), 2);
        assert_eq!(view.logs[1].level, tracing::Level::ERROR);

        let switch = view.last_switch.expect("last switch");
        assert_eq!(switch.to, "a");
        assert_eq!(switch.from, None);
    }

    #[test]
    fn model_usage_survives_doc_to_view_without_loss() {
        // Local and attach both go through from_doc, so a row produced by the
        // document builder must reach the renderer input intact (req21/31).
        let doc: DashboardDoc = serde_json::from_value(doc_json()).expect("parse doc");
        let view = DashboardView::from_doc(&doc);
        assert_eq!(view.model_usage.len(), 1);
        let row = &view.model_usage[0];
        assert_eq!(row.group, "claude");
        assert_eq!(row.model, "claude-sonnet-4-5");
        assert_eq!(row.tokens_in, 100);
        assert_eq!(row.tokens_out, 50);
        assert_eq!(row.cache_read, Some(4000));
        assert_eq!(row.cache_creation, None);
        assert_eq!(row.in_flight, 1);
        assert_eq!(row.accounts.len(), 1);
        assert_eq!(row.efforts.len(), 2);
        assert_eq!(row.endpoints[0].label, "messages");
    }

    #[test]
    fn model_usage_defaults_to_empty_for_older_documents() {
        let mut value = doc_json();
        value.as_object_mut().unwrap().remove("model_usage");
        let doc: DashboardDoc = serde_json::from_value(value).expect("parse doc");
        let view = DashboardView::from_doc(&doc);
        assert!(view.model_usage.is_empty());
    }

    #[test]
    fn unknown_credential_kind_degrades_without_special_casing() {
        let mut doc: DashboardDoc = serde_json::from_value(doc_json()).expect("parse doc");
        doc.accounts[0].kind = "gemini".into();
        let view = DashboardView::from_doc(&doc);
        assert_eq!(view.snapshot.accounts[0].credential_kind, "unknown");
    }
}
