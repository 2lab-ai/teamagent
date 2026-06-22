//! Dashboard state + document: the single source of truth behind both the
//! in-process TUI and the remote attach mode (`llmux dashboard`).
//!
//! - [`DashboardHub`] — server-owned fold of the activity-event stream and
//!   the tracing bridge: activity ring, per-account totals, last switch,
//!   poller health, log console. Lives in `proxy::server::AppState`; one
//!   fold task ([`fold`]) consumes the event/log channels into it.
//! - [`DashboardDoc`] — the serializable superset of `/llmux/status`
//!   served at `GET /llmux/dashboard`. The local TUI builds the SAME
//!   document in-process every frame and the remote client parses it from JSON,
//!   so both render paths share one contract (`tui::view` converts it into
//!   the view-model the draw code consumes).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::logging::LogLine;
use crate::proxy::server::{AppState, UsageTotals, EVALUATE_TICK};
use crate::scheduler::select::{self, SelectParams};
use crate::scheduler::{AccountSnapshot, CooldownSource, PoolSnapshot};
use crate::tui::activity::{
    normalize_model, ActivityLog, ClientUsage, Completed, CompletedBody, InFlight, ModelUsage,
    StatsWindow, Totals, WindowedRow,
};
use crate::tui::logs::LogConsole;
use crate::tui::{ActivityEvent, LastSwitch, PollHealth, TokenCounts};

/// Completed-activity entries served in the document. Matches the hub ring
/// ([`crate::tui::activity::LOG_CAPACITY`]) so the attach client can scroll
/// the FULL retained history (the activity panel is scrollable now), not just
/// a glance window. At a 1 Hz poll this is ~200 small JSON objects — cheap.
pub const ACTIVITY_TAIL: usize = 200;
/// Tracing lines served in the document.
pub const LOG_TAIL: usize = 100;
/// Trailing window for the requests-per-minute figure.
pub const RPM_WINDOW: Duration = Duration::from_secs(5 * 60);

// ---------------------------------------------------------------------------
// Hub: server-owned observability state
// ---------------------------------------------------------------------------

/// Server-side fold of activity events + tracing lines. All mutations are
/// sync and short (std Mutex, never held across an await) — same locking
/// discipline as the scheduler pool.
pub struct DashboardHub {
    inner: Mutex<HubState>,
}

struct HubState {
    log: ActivityLog,
    last_switch: Option<LastSwitch>,
    poll_health: HashMap<String, PollHealth>,
    console: LogConsole,
    /// Where finished requests are appended (req-persist A/C). `None` until the
    /// daemon arms persistence in [`DashboardHub::load_from_state_dir`]; left
    /// `None` in unit tests (which build the hub via `default()` and never call
    /// `serve`), so folding events through the hub never touches the real state
    /// dir during tests.
    persist_path: Option<std::path::PathBuf>,
}

impl Default for DashboardHub {
    fn default() -> Self {
        // Pure construction — NO filesystem IO. Tests build the hub via
        // `default()` and must start from an empty log; the persisted-log
        // replay is an explicit, daemon-only step ([`Self::load_from_state_dir`])
        // run once at serve startup, never on construction.
        Self {
            inner: Mutex::new(HubState {
                log: ActivityLog::new(crate::tui::activity::LOG_CAPACITY),
                last_switch: None,
                poll_health: HashMap::new(),
                console: LogConsole::new(crate::tui::logs::LOG_CONSOLE_CAPACITY),
                persist_path: None,
            }),
        }
    }
}

impl DashboardHub {
    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Arm persistence at `path` and resume from it (req-persist A/C): record
    /// the append target so subsequent `apply_event`s persist finished requests,
    /// then replay the existing log to resume cumulative model/account stats + a
    /// seeded activity ring. Replay folds each persisted request through the SAME
    /// `apply` as the live path, so the restored aggregates are identical (no
    /// double-counting).
    ///
    /// `path` is `state.activity_log_path`: the state-dir `activity.jsonl` for a
    /// real daemon, a tempdir for e2e, `None` to disable. Best-effort: a missing
    /// file leaves the log empty; `None` leaves persistence off. Called once
    /// from `serve` — never from `Default`, so unit tests that build the hub via
    /// `default()` and fold events stay isolated (their `persist_path` is
    /// `None`).
    pub fn arm_persistence(&self, path: Option<std::path::PathBuf>) {
        let mut state = self.lock();
        state.log.load_persisted(path.as_deref());
        state.persist_path = path;
    }

    /// Fold one proxy/scheduler event: last-switch + poller-health pane
    /// state, then the activity log itself.
    pub fn apply_event(&self, event: ActivityEvent, now: SystemTime) {
        let mut state = self.lock();
        match &event {
            ActivityEvent::AccountSwitched { from, to, reason } => {
                state.last_switch = Some(LastSwitch {
                    from: from.clone(),
                    to: to.clone(),
                    reason: reason.clone(),
                    at: now,
                });
            }
            ActivityEvent::UsagePolled {
                account,
                ok,
                consecutive_failures,
                next_in,
            } => {
                let entry = state
                    .poll_health
                    .entry(account.clone())
                    .or_insert(PollHealth {
                        last_ok: None,
                        consecutive_failures: 0,
                        next_at: now,
                    });
                if *ok {
                    entry.last_ok = Some(now);
                }
                entry.consecutive_failures = *consecutive_failures;
                entry.next_at = now + *next_in;
            }
            _ => {}
        }
        // Persist finished requests before folding (req-persist A/C). Borrow,
        // not move: `apply` still consumes the event below. The append target
        // is `None` until the daemon armed persistence in `load_from_state_dir`,
        // so this is a no-op in unit tests. Best-effort — a persistence failure
        // must never break the fold (and a non-finished event is a no-op inside
        // `persist_request`).
        if let Some(path) = state.persist_path.clone() {
            crate::tui::activity::persist_request(Some(&path), &event, now);
        }
        state.log.apply(event, now);
    }

    /// Append a raw tracing line to the log console ring.
    pub fn push_log(&self, line: LogLine) {
        self.lock().console.push(line);
    }

    /// Append an operator note ("config reloaded", …) to the activity log.
    pub fn push_note(&self, text: String, error: bool, now: SystemTime) {
        self.lock().log.push_note(text, error, now);
    }

    /// Point-in-time clone of everything the dashboard document needs.
    pub(crate) fn view(&self, now: SystemTime) -> HubView {
        let mut state = self.lock();
        // Sweep leaked in-flight rows on every read so the dashboard reflects
        // the daemon's real `in_flight` even when a `RequestFinished` event was
        // dropped on a full activity channel (BUG: zombie 25,000s+ rows).
        state.log.prune_stale_in_flight(now);
        HubView {
            last_switch: state.last_switch.clone(),
            poll_health: state.poll_health.clone(),
            in_flight: state.log.in_flight().to_vec(),
            completed: state.log.completed().take(ACTIVITY_TAIL).cloned().collect(),
            account_totals: state.log.totals_map(),
            global_totals: state.log.totals_global(),
            rpm_5m: state.log.requests_per_minute(now, RPM_WINDOW),
            model_usage: state.log.model_usage(),
            client_usage: state.log.client_usage(),
            // Windowed heatmap rows per window (issue #23). Computed under the
            // same lock as the rest of the view so one read is consistent.
            windowed: StatsWindow::ALL
                .iter()
                .map(|&w| (w, state.log.windowed_rows(w, now)))
                .collect(),
            logs: state.console.tail(LOG_TAIL).cloned().collect(),
        }
    }
}

/// Cloned hub state for one document build (no lock held while rendering).
pub(crate) struct HubView {
    pub last_switch: Option<LastSwitch>,
    pub poll_health: HashMap<String, PollHealth>,
    pub in_flight: Vec<InFlight>,
    /// Newest first (activity renders newest-top).
    pub completed: Vec<Completed>,
    pub account_totals: HashMap<String, Totals>,
    pub global_totals: Totals,
    pub rpm_5m: f64,
    /// Aggregated per-(group, model) usage rows, sorted by total tokens desc.
    pub model_usage: Vec<ModelUsage>,
    /// Per-client request attribution rows (issue #32), sorted by requests desc.
    pub client_usage: Vec<ClientUsage>,
    /// Windowed heatmap rows per window (issue #23): one `(window, rows)` pair
    /// per [`StatsWindow`], each already sorted by total tokens desc.
    pub windowed: Vec<(StatsWindow, Vec<WindowedRow>)>,
    /// Oldest→newest (console renders the tail at the bottom).
    pub logs: Vec<LogLine>,
}

/// Consume the activity-event and tracing-line channels into the hub. The
/// single consumer of both streams; spawned next to the listener in
/// `proxy::server::serve` and aborted on shutdown. With `trace_events` each
/// activity event is also rendered as a tracing line (daemon mode: keeps
/// `server.log` carrying the request history the TUI would have shown).
pub async fn fold(
    hub: std::sync::Arc<DashboardHub>,
    mut events: tokio::sync::mpsc::Receiver<ActivityEvent>,
    logs: Option<tokio::sync::mpsc::Receiver<LogLine>>,
    trace_events: bool,
) {
    let mut logs_open = logs.is_some();
    let mut logs = logs;
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Some(event) => {
                        if trace_events {
                            trace_event(&event);
                        }
                        hub.apply_event(event, SystemTime::now());
                    }
                    None => return, // every sender gone — server is down
                }
            }
            line = async {
                match logs.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if logs_open => {
                match line {
                    Some(line) => hub.push_log(line),
                    None => logs_open = false,
                }
            }
        }
    }
}

/// Render one activity event as a tracing log line (daemon mode parity with
/// the old non-TTY event drain).
fn trace_event(event: &ActivityEvent) {
    match event {
        ActivityEvent::RequestStarted { id, method, path } => {
            tracing::debug!(id, %method, %path, "request started");
        }
        ActivityEvent::RequestRouted {
            id,
            account,
            group,
            model,
        } => {
            tracing::debug!(
                id,
                %account,
                group = group.as_deref().unwrap_or("-"),
                model = model.as_deref().unwrap_or("-"),
                "request routed"
            );
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
            user_id,
        } => {
            // API-equivalent USD cost for this request (Feature D). The fold
            // task has no config handle, so the log line uses the built-in
            // default rate table (empty overrides). 0.0 unless group, model,
            // and token usage are all known.
            let cost = match (group, model, tokens) {
                (Some(g), Some(m), Some(t)) => {
                    crate::pricing::cost_usd(g, m, t, &std::collections::HashMap::new())
                }
                _ => 0.0,
            };
            tracing::info!(
                id, %method, %path,
                account = account.as_deref().unwrap_or("-"),
                status,
                duration_ms = duration.as_millis() as u64,
                tokens = tokens.map(TokenCounts::total).unwrap_or(0),
                cost = format!("{cost:.4}"),
                group = group.as_deref().unwrap_or("-"),
                model = model.as_deref().unwrap_or("-"),
                effort = effort.as_deref().unwrap_or("-"),
                client = user_id.as_deref().unwrap_or("unknown"),
                "request finished"
            );
        }
        ActivityEvent::AccountSwitched { from, to, reason } => {
            tracing::info!(
                from = from.as_deref().unwrap_or("(none)"),
                %to,
                reason = reason.as_deref().unwrap_or("-"),
                "account switched"
            );
        }
        ActivityEvent::TokenRefreshed {
            account,
            expires_at_ms,
        } => {
            tracing::info!(%account, expires_at_ms, "token refreshed");
        }
        ActivityEvent::UsagePolled {
            account,
            ok,
            consecutive_failures,
            next_in,
        } => {
            tracing::debug!(
                %account,
                ok,
                consecutive_failures,
                next_in_secs = next_in.as_secs(),
                "usage polled"
            );
        }
        ActivityEvent::Error { context, message } => {
            tracing::warn!(context = context.as_deref().unwrap_or("-"), %message, "proxy error");
        }
    }
}

// ---------------------------------------------------------------------------
// Document: the GET /llmux/dashboard contract
// ---------------------------------------------------------------------------

/// The `/llmux/dashboard` document — a strict superset of
/// `/llmux/status` (same account fields and ordering) plus scheduler /
/// poller / totals / activity / log state. Serialized by the server, parsed
/// by the attach-mode client, and built in-process by the local TUI — one
/// contract, one renderer. Fields are additive only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardDoc {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub port: u16,
    pub current: Option<String>,
    /// Per-group sticky current account (req1): `"claude"`/`"codex"` → account
    /// name, one entry per group that has a selection. The scalar `current`
    /// above stays the representative (claude slot first) for back-compat; this
    /// map drives the per-group `current` lines. Additive: docs written before
    /// this field default to an empty map and the renderer falls back to the
    /// scalar.
    #[serde(default)]
    pub current_by_group: BTreeMap<String, String>,
    pub upstream: String,
    pub config_path: Option<String>,
    pub select_params: SelectParamsDoc,
    pub refresh_ahead_secs: u64,
    pub evaluate_tick_secs: u64,
    /// Selection order (current → eligible by rank → ineligible), same as
    /// `/llmux/status`.
    pub accounts: Vec<AccountDoc>,
    pub scheduler: SchedulerDoc,
    pub poller: Vec<PollerDoc>,
    pub totals: GlobalTotalsDoc,
    /// Per-(group, served model) usage rows (req1-20). Additive: absent in docs
    /// written before this existed → an older client parses it as empty and an
    /// upgraded client attaching to an older daemon renders no model panel.
    #[serde(default)]
    pub model_usage: Vec<ModelUsageDoc>,
    /// Per-client request attribution (issue #32): in-memory request/token
    /// counts keyed by `metadata.user_id` (the `unknown` bucket holds
    /// requests with no id). Pure metering — no key issuance, no auth change.
    /// Additive: absent in docs written before this existed → an older client
    /// parses it as empty and an upgraded client attaching to an older daemon
    /// renders no client panel.
    #[serde(default)]
    pub client_usage: Vec<ClientUsageDoc>,
    /// Windowed (24h/72h) per-account/per-model token heatmap rows (issue #23).
    /// Additive: absent in pre-#23 docs → an older client parses it empty and an
    /// upgraded client attaching to an older daemon shows no heatmap. These are
    /// a BEST-EFFORT sample (the activity event channel is lossy — events are
    /// `try_send` and dropped on a full channel), not an exact ledger.
    #[serde(default)]
    pub windowed: Vec<WindowedStatsDoc>,
    pub activity: ActivityDoc,
    /// Tracing tail, oldest→newest.
    pub logs: Vec<LogLineDoc>,
    /// Live codex request settings (req8.1 — dashboard fast/model/effort).
    /// Additive: absent in docs written before this existed.
    #[serde(default)]
    pub codex: CodexSettingsDoc,
}

/// Live codex provider settings, surfaced so the dashboard can show and toggle
/// them (req8.1). `available` is false when no codex account is configured —
/// the dashboard then hides the controls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexSettingsDoc {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub fast: bool,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SelectParamsDoc {
    pub five_hour_max: f64,
    pub seven_day_max: f64,
    pub usage_max_age_secs: u64,
}

impl From<&SelectParams> for SelectParamsDoc {
    fn from(params: &SelectParams) -> Self {
        Self {
            five_hour_max: params.five_hour_max,
            seven_day_max: params.seven_day_max,
            usage_max_age_secs: params.usage_max_age.as_secs(),
        }
    }
}

impl From<&SelectParamsDoc> for SelectParams {
    fn from(doc: &SelectParamsDoc) -> Self {
        Self {
            five_hour_max: doc.five_hour_max,
            seven_day_max: doc.seven_day_max,
            usage_max_age: Duration::from_secs(doc.usage_max_age_secs),
        }
    }
}

/// One account, status-document-compatible plus the raw scheduler fields the
/// remote view-model needs to re-run the pure eligibility/ranking functions
/// client-side (`healthy`, window `fetched_at_ms`/`source`,
/// `cooldown_source`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountDoc {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub order: u64,
    pub blocked: Option<String>,
    pub healthy: bool,
    pub five_hour: Option<WindowDoc>,
    pub seven_day: Option<WindowDoc>,
    /// Epoch seconds (status parity); only present while cooling.
    pub cooldown_until: Option<u64>,
    pub cooldown_source: Option<String>,
    pub in_flight: u32,
    pub token_expires_at_ms: Option<u64>,
    pub last_refresh_ms: Option<u64>,
    /// Proxy-lifetime relayed totals (status parity).
    pub totals: LifetimeTotalsDoc,
    /// Activity-log totals (ok/err + token split) for the table/detail panes.
    pub session: SessionTotalsDoc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowDoc {
    pub utilization: f64,
    /// Epoch seconds (status parity).
    pub resets_at: u64,
    pub resets_in_secs: u64,
    /// Epoch ms — staleness is judged against this client-side.
    pub fetched_at_ms: u64,
    /// "headers" | "poll".
    pub source: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct LifetimeTotalsDoc {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SessionTotalsDoc {
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerDoc {
    pub last_switch: Option<LastSwitchDoc>,
    /// First eligible non-current account in selection order — what `pick`
    /// would switch to next.
    pub next_in_line: Option<String>,
    pub next_eval_in_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastSwitchDoc {
    pub from: Option<String>,
    pub to: String,
    pub reason: Option<String>,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollerDoc {
    pub account: String,
    pub last_ok_ms: Option<u64>,
    pub consecutive_failures: u32,
    pub next_at_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GlobalTotalsDoc {
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub rpm_5m: f64,
    pub in_flight: u32,
    /// API-equivalent USD cost (Feature D), defined as the **sum of the
    /// per-model-row costs** ([`ModelUsageDoc::cost_usd`]). Summing per row is
    /// the only correct aggregation because the global `tokens_in`/`tokens_out`
    /// mix models with different rates; a row already knows its own model's
    /// price. Additive: absent in docs written before Feature D → `0.0`.
    #[serde(default)]
    pub cost_usd: f64,
}

/// One model-usage row in the document (req1-20). Cache counters are omitted
/// from the JSON when unavailable (`None`), so the client distinguishes
/// "unavailable" from an explicit zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageDoc {
    pub group: String,
    pub model: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    /// Fresh (non-cached) input + output tokens.
    pub tokens_in: u64,
    pub tokens_out: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<u64>,
    /// Epoch ms of the last completed request for this model.
    pub last_used_ms: u64,
    /// In-flight requests currently attributed to this model (req11).
    #[serde(default)]
    pub in_flight: u32,
    /// Which account(s) served it (req19).
    #[serde(default)]
    pub accounts: Vec<ModelAccountDoc>,
    /// Reasoning/effort distribution (req18).
    #[serde(default)]
    pub efforts: Vec<ModelCountDoc>,
    /// Endpoint-class distribution (req20).
    #[serde(default)]
    pub endpoints: Vec<ModelCountDoc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAccountDoc {
    pub name: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// A labelled request count (an effort level or an endpoint class).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCountDoc {
    pub label: String,
    pub requests: u64,
}

/// One per-client attribution row (issue #32): a client identity
/// (`metadata.user_id`, or `unknown`) and its in-memory request/token counts.
/// Counting only — never a credential, never gates a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientUsageDoc {
    pub client: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// One trailing-window slice of the per-account/per-model heatmap (issue #23):
/// a window label ("24h"/"72h") + every `(group, model, account)` cell with
/// activity in that window, sorted by total tokens desc. Additive document
/// type, carried in [`DashboardDoc::windowed`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowedStatsDoc {
    /// Short window label ("24h" / "72h").
    pub window: String,
    /// Trailing duration this window covers, in seconds (so a client need not
    /// hardcode the label→duration map).
    pub window_secs: u64,
    pub cells: Vec<WindowedCellDoc>,
}

/// One `(group, model, account)` heatmap cell over a window. Token fields are
/// the in-window sums; `tokens` is the combined intensity (in+out+cache) the
/// heatmap colours by.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowedCellDoc {
    pub group: String,
    pub model: String,
    pub account: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_creation: u64,
    /// Combined token intensity (in + out + cache_read + cache_creation).
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityDoc {
    /// Started-but-unfinished requests, oldest→newest (render reversed).
    pub in_flight: Vec<InFlightDoc>,
    /// Completed entries, newest first, capped at [`ACTIVITY_TAIL`].
    pub completed: Vec<CompletedDoc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlightDoc {
    pub id: u64,
    pub method: String,
    pub path: String,
    pub account: Option<String>,
    pub started_at_ms: u64,
    /// Backend group / served model, filled at routing time so the in-flight
    /// row can show the model badge while running (issue #2 2a). Additive:
    /// absent in docs written before these fields existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompletedDoc {
    Request {
        at_ms: u64,
        method: String,
        path: String,
        account: Option<String>,
        status: u16,
        duration_ms: u64,
        tokens: Option<TokensDoc>,
        /// API-equivalent USD cost (Feature D) for this single request, from
        /// its `(group, model)` + `tokens` via [`crate::pricing::cost_usd`].
        /// `0.0` when group/model/tokens are not all known, or the model is
        /// unknown/zero-rate. Additive: absent in pre-Feature-D docs → `0.0`.
        #[serde(default)]
        cost_usd: f64,
        /// Backend group / served model / reasoning effort (additive: absent
        /// in docs written before these fields existed).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    Note {
        at_ms: u64,
        text: String,
        error: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TokensDoc {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLineDoc {
    /// "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE".
    pub level: String,
    pub text: String,
}

/// Server-process facts + config-derived display fields for one document.
#[derive(Debug, Clone)]
pub struct DocMeta {
    pub pid: u32,
    pub uptime_secs: u64,
    pub port: u16,
    pub upstream: String,
    pub config_path: Option<String>,
    pub refresh_ahead_secs: u64,
    pub evaluate_tick_secs: u64,
    pub codex: CodexSettingsDoc,
    /// API-equivalent pricing overrides from `[pricing]` in the live config
    /// (Feature D). Empty = use the built-in default rate table. Threaded here
    /// (rather than into the pure `dashboard_doc` signature) because `DocMeta`
    /// already carries the config-derived display fields the builder needs.
    pub pricing_overrides: HashMap<String, crate::pricing::ModelPrice>,
}

pub(crate) fn epoch_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn epoch_secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Derive the status word + blocking reason for one account — shared by
/// `/llmux/status` and `/llmux/dashboard` so the wording never
/// drifts between the two documents.
pub(crate) fn account_status_blocked(
    account: &AccountSnapshot,
    snapshot: &PoolSnapshot,
    params: &SelectParams,
    now: SystemTime,
    headers_only: bool,
) -> (&'static str, Option<String>) {
    let cooling = account.cooldown_until.is_some_and(|until| until > now);
    let status = if !account.healthy {
        "auth_failed"
    } else if cooling {
        "cooldown"
    } else if snapshot.is_current(&account.id) {
        "active"
    } else {
        "ok"
    };
    let blocked = select::eligibility(account, params, now, headers_only)
        .map(|reason| select::blocking_reason(account, reason, params, now));
    (status, blocked)
}

fn window_doc(
    window: &Option<crate::scheduler::window::QuotaWindow>,
    now: SystemTime,
) -> Option<WindowDoc> {
    window.map(|w| WindowDoc {
        utilization: w.utilization,
        resets_at: epoch_secs(w.resets_at),
        resets_in_secs: w
            .resets_at
            .duration_since(now)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        fetched_at_ms: epoch_ms(w.fetched_at),
        source: match w.source {
            crate::scheduler::window::WindowSource::Headers => "headers".into(),
            crate::scheduler::window::WindowSource::UsagePoll => "poll".into(),
        },
    })
}

/// Build the model-usage rows for the document: the finished aggregation from
/// the hub, with in-flight requests overlaid per model (req11). A model seen
/// only in-flight (no completed request yet) still gets a row so a long active
/// request is visible before it finishes.
/// Build the model-usage rows AND the total API-equivalent USD cost across all
/// completed model rows (Feature D). The total is the **sum of each completed
/// row's per-model cost** — the only correct aggregation, since each model
/// carries its own rate. In-flight-only rows have no completed tokens and
/// contribute `0`. The per-model cost is computed here but NOT stored on
/// [`ModelUsageDoc`]: the TUI's `ui.rs` constructs that struct with an
/// exhaustive literal and is frozen for a parallel rewrite, so adding a field
/// to it is impossible from here. Per-model cost is recoverable by the client
/// from the row's tokens + the same pricing table; the global total and the
/// per-request `CompletedDoc::Request::cost_usd` carry the cost end to end.
fn model_usage_docs(
    hub: &HubView,
    now: SystemTime,
    pricing_overrides: &HashMap<String, crate::pricing::ModelPrice>,
) -> (Vec<ModelUsageDoc>, f64) {
    let total_cost: f64 = hub
        .model_usage
        .iter()
        .map(|m| {
            crate::pricing::cost_from_parts(
                &m.group,
                &m.model,
                m.tokens_in,
                m.tokens_out,
                m.cache_read,
                m.cache_creation,
                pricing_overrides,
            )
        })
        .sum();
    let row = |m: &ModelUsage| ModelUsageDoc {
        group: m.group.clone(),
        model: m.model.clone(),
        requests: m.requests,
        ok: m.ok,
        errors: m.errors,
        tokens_in: m.tokens_in,
        tokens_out: m.tokens_out,
        cache_read: m.cache_read,
        cache_creation: m.cache_creation,
        last_used_ms: epoch_ms(m.last_used),
        in_flight: 0,
        accounts: m
            .accounts
            .iter()
            .map(|a| ModelAccountDoc {
                name: a.name.clone(),
                requests: a.requests,
                ok: a.ok,
                errors: a.errors,
                tokens_in: a.tokens_in,
                tokens_out: a.tokens_out,
            })
            .collect(),
        efforts: m
            .efforts
            .iter()
            .map(|c| ModelCountDoc {
                label: c.label.clone(),
                requests: c.requests,
            })
            .collect(),
        endpoints: m
            .endpoints
            .iter()
            .map(|c| ModelCountDoc {
                label: c.label.clone(),
                requests: c.requests,
            })
            .collect(),
    };
    let mut docs: Vec<ModelUsageDoc> = hub.model_usage.iter().map(row).collect();

    // Count in-flight requests per (group, normalized model) — the in-flight
    // entries carry the served identity set at routing time.
    let mut in_flight: BTreeMap<(String, String), u32> = BTreeMap::new();
    for r in &hub.in_flight {
        if let (Some(group), Some(model)) = (&r.group, &r.model) {
            *in_flight
                .entry((group.clone(), normalize_model(model)))
                .or_default() += 1;
        }
    }
    for doc in docs.iter_mut() {
        if let Some(n) = in_flight.remove(&(doc.group.clone(), doc.model.clone())) {
            doc.in_flight = n;
        }
    }
    // Append rows for models that are ONLY in-flight (sorted by the BTreeMap).
    for ((group, model), n) in in_flight {
        docs.push(ModelUsageDoc {
            group,
            model,
            requests: 0,
            ok: 0,
            errors: 0,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: None,
            cache_creation: None,
            last_used_ms: epoch_ms(now),
            in_flight: n,
            accounts: Vec::new(),
            efforts: Vec::new(),
            endpoints: Vec::new(),
        });
    }
    (docs, total_cost)
}

/// Build the windowed heatmap document rows from the hub view (issue #23): one
/// [`WindowedStatsDoc`] per retained window, each carrying every in-window
/// `(group, model, account)` cell. The rows are already sorted by the hub.
fn windowed_docs(hub: &HubView) -> Vec<WindowedStatsDoc> {
    hub.windowed
        .iter()
        .map(|(window, rows)| WindowedStatsDoc {
            window: window.label().to_string(),
            window_secs: window.duration().as_secs(),
            cells: rows
                .iter()
                .map(|r: &WindowedRow| WindowedCellDoc {
                    group: r.group.clone(),
                    model: r.model.clone(),
                    account: r.account.clone(),
                    requests: r.counts.requests,
                    ok: r.counts.ok,
                    errors: r.counts.errors,
                    tokens_in: r.counts.tokens_in,
                    tokens_out: r.counts.tokens_out,
                    cache_read: r.counts.cache_read,
                    cache_creation: r.counts.cache_creation,
                    tokens: r.counts.tokens(),
                })
                .collect(),
        })
        .collect()
}

/// Build the dashboard document — pure over snapshot/hub/totals/params so
/// the shape is unit-testable without a socket.
pub(crate) fn dashboard_doc(
    snapshot: &PoolSnapshot,
    hub: &HubView,
    totals: &UsageTotals,
    params: &SelectParams,
    now: SystemTime,
    meta: &DocMeta,
) -> DashboardDoc {
    let headers_only = select::headers_only_mode(snapshot, params, None, now);
    let order = select::selection_order(snapshot, params, now);
    let accounts: Vec<AccountDoc> = order
        .iter()
        .enumerate()
        .map(|(pos, &idx)| {
            let account = &snapshot.accounts[idx];
            let (status, blocked) =
                account_status_blocked(account, snapshot, params, now, headers_only);
            let cooling = account.cooldown_until.is_some_and(|until| until > now);
            let lifetime = totals.get(&account.id);
            let session = hub
                .account_totals
                .get(&account.id.0)
                .copied()
                .unwrap_or_default();
            AccountDoc {
                name: account.id.0.clone(),
                kind: account.credential_kind.to_string(),
                status: status.to_string(),
                order: pos as u64 + 1,
                blocked,
                healthy: account.healthy,
                five_hour: window_doc(&account.five_hour, now),
                seven_day: window_doc(&account.seven_day, now),
                cooldown_until: account.cooldown_until.filter(|_| cooling).map(epoch_secs),
                cooldown_source: account.cooldown_source.map(|s| match s {
                    CooldownSource::RetryAfter => "retry_after".to_string(),
                    CooldownSource::Heuristic => "heuristic".to_string(),
                }),
                in_flight: account.in_flight,
                token_expires_at_ms: account.token_expires_at_ms,
                last_refresh_ms: account.last_refresh_ms,
                totals: LifetimeTotalsDoc {
                    requests: lifetime.requests,
                    input_tokens: lifetime.input_tokens,
                    output_tokens: lifetime.output_tokens,
                },
                session: SessionTotalsDoc {
                    requests: session.requests,
                    ok: session.ok,
                    errors: session.errors,
                    tokens_in: session.tokens_in,
                    tokens_out: session.tokens_out,
                },
            }
        })
        .collect();

    // First eligible non-current account in selection order.
    let next_in_line = order
        .iter()
        .map(|&i| &snapshot.accounts[i])
        .filter(|a| !snapshot.is_current(&a.id))
        .find(|a| select::eligibility(a, params, now, headers_only).is_none())
        .map(|a| a.id.0.clone());
    let tick = meta.evaluate_tick_secs.max(1);
    let scheduler = SchedulerDoc {
        last_switch: hub.last_switch.as_ref().map(|s| LastSwitchDoc {
            from: s.from.clone(),
            to: s.to.clone(),
            reason: s.reason.clone(),
            at_ms: epoch_ms(s.at),
        }),
        next_in_line,
        next_eval_in_secs: tick - (meta.uptime_secs % tick),
    };

    let mut poller: Vec<PollerDoc> = hub
        .poll_health
        .iter()
        .map(|(account, health)| PollerDoc {
            account: account.clone(),
            last_ok_ms: health.last_ok.map(epoch_ms),
            consecutive_failures: health.consecutive_failures,
            next_at_ms: epoch_ms(health.next_at),
        })
        .collect();
    poller.sort_by(|a, b| a.account.cmp(&b.account));

    let in_flight_total: u32 = snapshot.accounts.iter().map(|a| a.in_flight).sum();
    let activity = ActivityDoc {
        in_flight: hub
            .in_flight
            .iter()
            .map(|r| InFlightDoc {
                id: r.id,
                method: r.method.clone(),
                path: r.path.clone(),
                account: r.account.clone(),
                started_at_ms: epoch_ms(r.started_at),
                group: r.group.clone(),
                model: r.model.clone(),
            })
            .collect(),
        completed: hub
            .completed
            .iter()
            .take(ACTIVITY_TAIL)
            .map(|entry| match &entry.body {
                CompletedBody::Request {
                    method,
                    path,
                    account,
                    status,
                    duration,
                    tokens,
                    group,
                    model,
                    effort,
                } => CompletedDoc::Request {
                    at_ms: epoch_ms(entry.at),
                    method: method.clone(),
                    path: path.clone(),
                    account: account.clone(),
                    status: *status,
                    duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                    tokens: tokens.map(|t| TokensDoc {
                        input: t.input,
                        output: t.output,
                    }),
                    // Per-request API-equivalent cost: 0.0 unless group, model,
                    // and the upstream token usage are ALL known (Feature D).
                    cost_usd: match (group, model, tokens) {
                        (Some(g), Some(m), Some(t)) => {
                            crate::pricing::cost_usd(g, m, t, &meta.pricing_overrides)
                        }
                        _ => 0.0,
                    },
                    group: group.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                },
                CompletedBody::Note { text, error } => CompletedDoc::Note {
                    at_ms: epoch_ms(entry.at),
                    text: text.clone(),
                    error: *error,
                },
            })
            .collect(),
    };

    // Build model rows once; the builder also returns the global cost as the
    // sum of each completed row's per-model cost (Feature D).
    let (model_usage, total_cost_usd) = model_usage_docs(hub, now, &meta.pricing_overrides);

    // Per-client attribution rows (issue #32): already sorted (requests desc)
    // by the activity log; a direct projection to the wire type.
    let client_usage: Vec<ClientUsageDoc> = hub
        .client_usage
        .iter()
        .map(|c| ClientUsageDoc {
            client: c.client.clone(),
            requests: c.requests,
            ok: c.ok,
            errors: c.errors,
            tokens_in: c.tokens_in,
            tokens_out: c.tokens_out,
        })
        .collect();

    DashboardDoc {
        version: crate::build_info::version_string(),
        pid: meta.pid,
        uptime_secs: meta.uptime_secs,
        port: meta.port,
        current: snapshot.representative_current().map(|c| c.0.clone()),
        current_by_group: snapshot
            .current
            .iter()
            .map(|(group, id)| (group.as_str().to_string(), id.0.clone()))
            .collect(),
        upstream: meta.upstream.clone(),
        config_path: meta.config_path.clone(),
        select_params: SelectParamsDoc::from(params),
        refresh_ahead_secs: meta.refresh_ahead_secs,
        evaluate_tick_secs: meta.evaluate_tick_secs,
        accounts,
        scheduler,
        poller,
        totals: GlobalTotalsDoc {
            requests: hub.global_totals.requests,
            ok: hub.global_totals.ok,
            errors: hub.global_totals.errors,
            tokens_in: hub.global_totals.tokens_in,
            tokens_out: hub.global_totals.tokens_out,
            rpm_5m: hub.rpm_5m,
            in_flight: in_flight_total,
            cost_usd: total_cost_usd,
        },
        model_usage,
        client_usage,
        windowed: windowed_docs(hub),
        activity,
        logs: hub
            .logs
            .iter()
            .map(|line| LogLineDoc {
                level: line.level.to_string(),
                text: line.text.clone(),
            })
            .collect(),
        codex: meta.codex.clone(),
    }
}

/// Build the document from live server state — what `GET /llmux/dashboard`
/// serves and what the local TUI renders each frame.
pub(crate) fn build_doc(state: &AppState, now: SystemTime) -> DashboardDoc {
    let snapshot = state.pool.snapshot();
    let params = state.select_params();
    let hub = state.hub.view(now);
    let codex_shape = state.codex.shape();
    let meta = DocMeta {
        pid: std::process::id(),
        uptime_secs: state.started.elapsed().as_secs(),
        port: state.bound_port.load(std::sync::atomic::Ordering::Relaxed),
        upstream: state.config.upstream.clone(),
        config_path: state.config_path.as_ref().map(|p| p.display().to_string()),
        refresh_ahead_secs: state.config.scheduler.refresh_ahead_secs,
        evaluate_tick_secs: EVALUATE_TICK.as_secs(),
        codex: CodexSettingsDoc {
            available: snapshot
                .accounts
                .iter()
                .any(|a| a.group == crate::routing::BackendGroup::Codex),
            fast: codex_shape.fast,
            model: codex_shape.model,
            effort: codex_shape.effort,
        },
        // Pricing overrides from the live config's `[pricing]` section
        // (Feature D); empty → built-in default rate table.
        pricing_overrides: state.config.pricing.clone(),
    };
    dashboard_doc(&snapshot, &hub, &state.totals, &params, now, &meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountConfig, AccountCredential};
    use crate::scheduler::headers::{ParsedRateLimitHeaders, WindowReading};
    use crate::scheduler::{AccountId, AccountPool};

    const NOW_SECS: u64 = 1_000_000;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECS)
    }

    fn params() -> SelectParams {
        SelectParams {
            five_hour_max: 0.90,
            seven_day_max: 0.99,
            usage_max_age: Duration::from_secs(600),
        }
    }

    fn meta() -> DocMeta {
        DocMeta {
            pid: 4321,
            uptime_secs: 130,
            port: 3456,
            upstream: "https://api.anthropic.com".into(),
            config_path: Some("/tmp/llmux.json".into()),
            refresh_ahead_secs: 7 * 3600,
            evaluate_tick_secs: 60,
            codex: CodexSettingsDoc {
                available: true,
                fast: false,
                model: "gpt-5.5".into(),
                effort: None,
            },
            pricing_overrides: HashMap::new(),
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

    /// Hub fed with a realistic event sequence: request lifecycle, a switch,
    /// a usage poll, and a tracing line.
    fn seeded_hub() -> DashboardHub {
        let hub = DashboardHub::default();
        hub.apply_event(
            ActivityEvent::AccountSwitched {
                from: None,
                to: "a".into(),
                reason: Some("initial selection".into()),
            },
            now() - Duration::from_secs(90),
        );
        hub.apply_event(
            ActivityEvent::RequestStarted {
                id: 1,
                method: "POST".into(),
                path: "/v1/messages".into(),
            },
            now() - Duration::from_secs(60),
        );
        hub.apply_event(
            ActivityEvent::RequestFinished {
                id: 1,
                method: "POST".into(),
                path: "/v1/messages".into(),
                account: Some("a".into()),
                status: 200,
                duration: Duration::from_millis(1_400),
                tokens: Some(TokenCounts {
                    input: 700,
                    output: 300,
                    cache_read: Some(120),
                    cache_creation: None,
                }),
                group: Some("codex".into()),
                model: Some("gpt-5.5".into()),
                effort: Some("high".into()),
                user_id: Some("acct_seed".into()),
            },
            now() - Duration::from_secs(58),
        );
        hub.apply_event(
            ActivityEvent::RequestStarted {
                id: 2,
                method: "POST".into(),
                path: "/v1/messages".into(),
            },
            now() - Duration::from_secs(3),
        );
        // In-flight request routed to the same codex model — exercises the
        // per-model in-flight overlay (req11).
        hub.apply_event(
            ActivityEvent::RequestRouted {
                id: 2,
                account: "a".into(),
                group: Some("codex".into()),
                model: Some("gpt-5.5".into()),
            },
            now() - Duration::from_secs(2),
        );
        hub.apply_event(
            ActivityEvent::UsagePolled {
                account: "a".into(),
                ok: true,
                consecutive_failures: 0,
                next_in: Duration::from_secs(300),
            },
            now() - Duration::from_secs(10),
        );
        hub.push_log(LogLine {
            level: tracing::Level::INFO,
            text: "proxy: proxy listening".into(),
        });
        hub
    }

    fn seeded_doc() -> DashboardDoc {
        let pool = AccountPool::new(&[oauth_account("a"), oauth_account("b")]);
        pool.evaluate(None, &params(), now());
        pool.record_headers(
            &AccountId("a".into()),
            &ParsedRateLimitHeaders {
                five_hour: Some(WindowReading {
                    utilization: 0.42,
                    resets_at: now() + Duration::from_secs(3600),
                }),
                seven_day: Some(WindowReading {
                    utilization: 0.10,
                    resets_at: now() + Duration::from_secs(86_400),
                }),
                ..Default::default()
            },
            now(),
        );
        pool.record_429(
            &AccountId("b".into()),
            Some(Duration::from_secs(120)),
            now(),
        );
        let totals = UsageTotals::default();
        totals.record(&AccountId("a".into()), 1, 700, 300);
        let hub = seeded_hub();
        dashboard_doc(
            &pool.snapshot(),
            &hub.view(now()),
            &totals,
            &params(),
            now(),
            &meta(),
        )
    }

    #[test]
    fn doc_is_a_status_superset_with_accounts_in_selection_order() {
        let doc = seeded_doc();
        assert!(doc.version.starts_with("llmux "));
        assert_eq!(doc.pid, 4321);
        assert_eq!(doc.port, 3456);
        assert_eq!(doc.uptime_secs, 130);
        assert_eq!(doc.current.as_deref(), Some("a"));
        assert_eq!(doc.upstream, "https://api.anthropic.com");
        assert_eq!(doc.config_path.as_deref(), Some("/tmp/llmux.json"));

        // Selection order: current first, parked account last.
        let names: Vec<&str> = doc.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(doc.accounts[0].order, 1);
        assert_eq!(doc.accounts[0].status, "active");
        assert!(doc.accounts[0].healthy);
        assert_eq!(doc.accounts[1].status, "cooldown");
        assert_eq!(doc.accounts[1].blocked.as_deref(), Some("cooldown 2m00s"));
        assert_eq!(
            doc.accounts[1].cooldown_source.as_deref(),
            Some("retry_after")
        );

        // Window carries the reconstruction fields.
        let five = doc.accounts[0].five_hour.as_ref().expect("5h window");
        assert!((five.utilization - 0.42).abs() < 1e-9);
        assert_eq!(five.resets_at, NOW_SECS + 3600);
        assert_eq!(five.resets_in_secs, 3600);
        assert_eq!(five.fetched_at_ms, NOW_SECS * 1000);
        assert_eq!(five.source, "headers");

        // Lifetime (proxy) + session (activity) totals both present.
        assert_eq!(doc.accounts[0].totals.requests, 1);
        assert_eq!(doc.accounts[0].totals.input_tokens, 700);
        assert_eq!(doc.accounts[0].session.requests, 1);
        assert_eq!(doc.accounts[0].session.ok, 1);
        assert_eq!(doc.accounts[0].session.tokens_out, 300);
    }

    #[test]
    fn doc_carries_per_group_current_slots() {
        // Routing on: claude and codex each pick a current independently, so
        // the doc must carry BOTH slots — not just the representative scalar.
        let pool = AccountPool::new(&[oauth_account("a"), codex_account("c")]);
        pool.evaluate(Some(crate::routing::BackendGroup::Claude), &params(), now());
        pool.evaluate(Some(crate::routing::BackendGroup::Codex), &params(), now());
        let doc = dashboard_doc(
            &pool.snapshot(),
            &seeded_hub().view(now()),
            &UsageTotals::default(),
            &params(),
            now(),
            &meta(),
        );
        // Representative scalar stays the claude slot (back-compat).
        assert_eq!(doc.current.as_deref(), Some("a"));
        // The new per-group map carries both group currents.
        assert_eq!(
            doc.current_by_group.get("claude").map(String::as_str),
            Some("a")
        );
        assert_eq!(
            doc.current_by_group.get("codex").map(String::as_str),
            Some("c")
        );
    }

    #[test]
    fn doc_carries_scheduler_poller_totals_activity_and_log_tails() {
        let doc = seeded_doc();

        let switch = doc.scheduler.last_switch.as_ref().expect("last switch");
        assert_eq!(switch.to, "a");
        assert_eq!(switch.reason.as_deref(), Some("initial selection"));
        assert_eq!(switch.at_ms, (NOW_SECS - 90) * 1000);
        assert_eq!(
            doc.scheduler.next_in_line, None,
            "b is parked — nothing eligible besides current"
        );
        assert_eq!(doc.scheduler.next_eval_in_secs, 60 - (130 % 60));

        assert_eq!(doc.poller.len(), 1);
        assert_eq!(doc.poller[0].account, "a");
        assert_eq!(doc.poller[0].last_ok_ms, Some((NOW_SECS - 10) * 1000));
        assert_eq!(doc.poller[0].consecutive_failures, 0);
        assert_eq!(doc.poller[0].next_at_ms, (NOW_SECS - 10 + 300) * 1000);

        assert_eq!(doc.totals.requests, 1);
        assert_eq!(doc.totals.ok, 1);
        assert_eq!(doc.totals.errors, 0);
        assert_eq!(doc.totals.tokens_in, 700);
        assert_eq!(doc.totals.tokens_out, 300);

        // Activity: one in-flight (id 2), completed request + switch note.
        assert_eq!(doc.activity.in_flight.len(), 1);
        assert_eq!(doc.activity.in_flight[0].id, 2);
        assert_eq!(doc.activity.in_flight[0].path, "/v1/messages");
        assert!(matches!(
            &doc.activity.completed[0],
            CompletedDoc::Request {
                status: 200,
                duration_ms: 1400,
                ..
            }
        ));
        // group/model/effort (req7) are carried into the doc.
        match &doc.activity.completed[0] {
            CompletedDoc::Request {
                group,
                model,
                effort,
                ..
            } => {
                assert_eq!(group.as_deref(), Some("codex"));
                assert_eq!(model.as_deref(), Some("gpt-5.5"));
                assert_eq!(effort.as_deref(), Some("high"));
            }
            other => panic!("expected request, got {other:?}"),
        }
        assert!(doc
            .activity
            .completed
            .iter()
            .any(|e| matches!(e, CompletedDoc::Note { text, .. } if text.contains("switch"))));

        assert_eq!(doc.logs.len(), 1);
        assert_eq!(doc.logs[0].level, "INFO");
        assert!(doc.logs[0].text.contains("proxy listening"));
    }

    #[test]
    fn doc_carries_model_usage_rows_with_cache_breakdowns_and_in_flight() {
        let doc = seeded_doc();
        // One finished codex/gpt-5.5 request + one in-flight routed to it.
        assert_eq!(doc.model_usage.len(), 1);
        let row = &doc.model_usage[0];
        assert_eq!(row.group, "codex");
        assert_eq!(row.model, "gpt-5.5");
        assert_eq!(row.requests, 1);
        assert_eq!(row.ok, 1);
        assert_eq!(row.errors, 0);
        assert_eq!(row.tokens_in, 700);
        assert_eq!(row.tokens_out, 300);
        // cache_read captured; cache_creation never reported → omitted.
        assert_eq!(row.cache_read, Some(120));
        assert_eq!(row.cache_creation, None);
        assert_eq!(row.last_used_ms, (NOW_SECS - 58) * 1000);
        // The routed-but-unfinished request overlays as in-flight (req11).
        assert_eq!(row.in_flight, 1);
        // Breakdowns.
        assert_eq!(row.accounts.len(), 1);
        assert_eq!(row.accounts[0].name, "a");
        assert_eq!(row.accounts[0].tokens_in, 700);
        assert_eq!(
            row.efforts
                .iter()
                .find(|e| e.label == "high")
                .map(|e| e.requests),
            Some(1)
        );
        assert_eq!(
            row.endpoints
                .iter()
                .find(|e| e.label == "messages")
                .map(|e| e.requests),
            Some(1)
        );
    }

    #[test]
    fn cache_creation_omitted_from_json_when_unavailable() {
        let doc = seeded_doc();
        let value: serde_json::Value = serde_json::to_value(&doc).expect("serialize");
        let row = &value["model_usage"][0];
        assert_eq!(row["cache_read"], 120);
        // None → skipped entirely, so the client renders "unavailable" not 0.
        assert!(row.get("cache_creation").is_none());
    }

    /// Same seed as [`seeded_doc`] but with a caller-supplied [`DocMeta`] so a
    /// test can inject pricing overrides (Feature D).
    fn seeded_doc_with_meta(meta: &DocMeta) -> DashboardDoc {
        let pool = AccountPool::new(&[oauth_account("a"), oauth_account("b")]);
        pool.evaluate(None, &params(), now());
        let totals = UsageTotals::default();
        totals.record(&AccountId("a".into()), 1, 700, 300);
        let hub = seeded_hub();
        dashboard_doc(
            &pool.snapshot(),
            &hub.view(now()),
            &totals,
            &params(),
            now(),
            meta,
        )
    }

    #[test]
    fn doc_carries_api_equivalent_cost_with_default_pricing() {
        // The single completed request is codex/gpt-5.5 with input=700,
        // output=300, cache_read=120. Under the built-in gpt-5.5 rates
        // {5.0, 30.0, 0.5, 0.0} per 1e6: 0.0035 + 0.009 + 0.00006 = 0.01256.
        let tokens = TokenCounts {
            input: 700,
            output: 300,
            cache_read: Some(120),
            cache_creation: None,
        };
        let expected = crate::pricing::cost_usd("codex", "gpt-5.5", &tokens, &HashMap::new());

        let doc = seeded_doc();
        // Global total = sum of per-model row costs (here, the one codex row).
        assert!(
            (doc.totals.cost_usd - expected).abs() < 1e-9,
            "global cost {} != {expected}",
            doc.totals.cost_usd
        );
        assert!(
            (expected - 0.012_56).abs() < 1e-9,
            "sanity: expected gpt-5.5 cost is 0.01256, got {expected}"
        );

        // Per-request activity line carries the same cost.
        match &doc.activity.completed[0] {
            CompletedDoc::Request { cost_usd, .. } => {
                assert!(
                    (*cost_usd - expected).abs() < 1e-9,
                    "per-request cost {cost_usd} != {expected}"
                );
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn doc_global_cost_reflects_config_pricing_override() {
        // Override gpt-5.5 input to 9.99/1e6, everything else 0 → cost is
        // purely 700 * 9.99 / 1e6 = 0.006993 for the one request.
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-5.5".to_string(),
            crate::pricing::ModelPrice {
                input: 9.99,
                output: 0.0,
                cache_read: 0.0,
                cache_creation: 0.0,
            },
        );
        let mut m = meta();
        m.pricing_overrides = overrides.clone();

        let doc = seeded_doc_with_meta(&m);
        let expected = 700.0 * 9.99 / 1_000_000.0;
        assert!(
            (doc.totals.cost_usd - expected).abs() < 1e-9,
            "override global cost {} != {expected}",
            doc.totals.cost_usd
        );
        // And the default (no override) gives a different number, proving the
        // override actually took effect.
        let default_doc = seeded_doc();
        assert!(
            (default_doc.totals.cost_usd - doc.totals.cost_usd).abs() > 1e-6,
            "override must change the cost from the default"
        );
    }

    #[test]
    fn doc_cost_round_trips_through_json() {
        let doc = seeded_doc();
        let value: serde_json::Value = serde_json::to_value(&doc).expect("serialize");
        // Global cost is a plain f64 field on totals.
        assert!(value["totals"]["cost_usd"].is_number());
        // Per-request cost is serialized on the completed request entry.
        let completed = value["activity"]["completed"]
            .as_array()
            .expect("completed array");
        let req = completed
            .iter()
            .find(|e| e["kind"] == "request")
            .expect("a request entry");
        assert!(req["cost_usd"].is_number());

        let parsed: DashboardDoc = serde_json::from_value(value).expect("parse");
        assert!((parsed.totals.cost_usd - doc.totals.cost_usd).abs() < 1e-12);
    }

    #[test]
    fn doc_without_cost_field_parses_to_zero() {
        // A pre-Feature-D document omits cost_usd; the additive serde default
        // keeps it parseable (0.0) so an upgraded client can still attach.
        let doc = seeded_doc();
        let mut value = serde_json::to_value(&doc).expect("serialize");
        value["totals"].as_object_mut().unwrap().remove("cost_usd");
        let parsed: DashboardDoc = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed.totals.cost_usd, 0.0);
    }

    #[test]
    fn doc_carries_windowed_heatmap_cells_per_window() {
        // The seeded hub folds one finished codex/gpt-5.5 request on account
        // "a" (issue #23): both the 24h and 72h windows must carry a cell for
        // (codex, gpt-5.5, a) with the right counts.
        let doc = seeded_doc();
        // One slice per retained window.
        assert_eq!(doc.windowed.len(), 2);
        let labels: Vec<&str> = doc.windowed.iter().map(|w| w.window.as_str()).collect();
        assert_eq!(labels, vec!["24h", "72h"]);

        for slice in &doc.windowed {
            let cell = slice
                .cells
                .iter()
                .find(|c| c.group == "codex" && c.model == "gpt-5.5" && c.account == "a")
                .unwrap_or_else(|| panic!("missing cell in {} window", slice.window));
            assert_eq!(cell.requests, 1);
            assert_eq!(cell.ok, 1);
            assert_eq!(cell.errors, 0);
            assert_eq!(cell.tokens_in, 700);
            assert_eq!(cell.tokens_out, 300);
            assert_eq!(cell.cache_read, 120);
            // tokens() intensity = in + out + cache_read + cache_creation.
            assert_eq!(cell.tokens, 700 + 300 + 120);
        }
    }

    #[test]
    fn doc_without_windowed_field_parses_to_empty() {
        // A pre-#23 daemon's document predates `windowed` — the additive serde
        // default keeps it parseable so an upgraded client can still attach.
        let doc = seeded_doc();
        let mut value = serde_json::to_value(&doc).expect("serialize");
        value.as_object_mut().unwrap().remove("windowed");
        let parsed: DashboardDoc = serde_json::from_value(value).expect("parse");
        assert!(parsed.windowed.is_empty());
    }

    #[test]
    fn doc_without_model_usage_field_parses_to_empty() {
        // An older daemon's document predates `model_usage` — additive default
        // keeps it parseable so an upgraded client can still attach (req23/33).
        let doc = seeded_doc();
        let mut value = serde_json::to_value(&doc).expect("serialize");
        value.as_object_mut().unwrap().remove("model_usage");
        let parsed: DashboardDoc = serde_json::from_value(value).expect("parse");
        assert!(parsed.model_usage.is_empty());
    }

    #[test]
    fn doc_round_trips_through_json() {
        let doc = seeded_doc();
        let json = serde_json::to_string(&doc).expect("serialize");
        let parsed: DashboardDoc = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed.accounts.len(), doc.accounts.len());
        assert_eq!(parsed.accounts[0].name, "a");
        assert_eq!(
            parsed.accounts[0]
                .five_hour
                .as_ref()
                .expect("window")
                .fetched_at_ms,
            doc.accounts[0]
                .five_hour
                .as_ref()
                .expect("window")
                .fetched_at_ms
        );
        assert_eq!(
            parsed.activity.completed.len(),
            doc.activity.completed.len()
        );
        assert_eq!(parsed.model_usage.len(), doc.model_usage.len());
        assert_eq!(parsed.model_usage[0].model, "gpt-5.5");
        assert_eq!(parsed.model_usage[0].in_flight, 1);
        assert_eq!(parsed.logs[0].level, "INFO");
        // The JSON keys stay status-compatible ("type", not "kind").
        let value: serde_json::Value = serde_json::from_str(&json).expect("value");
        assert_eq!(value["accounts"][0]["type"], "oauth");
        assert!(value["accounts"][0]["five_hour"]["resets_in_secs"].is_u64());
    }

    #[test]
    fn activity_tail_caps_at_capacity() {
        let hub = DashboardHub::default();
        let seeded = ACTIVITY_TAIL as u64 + 30;
        for i in 0..seeded {
            hub.apply_event(
                ActivityEvent::RequestFinished {
                    id: i,
                    method: "POST".into(),
                    path: format!("/v1/messages/{i}"),
                    account: Some("a".into()),
                    status: 200,
                    duration: Duration::from_millis(10),
                    tokens: None,
                    group: None,
                    model: None,
                    effort: None,
                    user_id: None,
                },
                now() - Duration::from_secs(seeded - i),
            );
        }
        let view = hub.view(now());
        assert_eq!(view.completed.len(), ACTIVITY_TAIL);
        // Newest first: the last-applied id leads.
        let newest = seeded - 1;
        match &view.completed[0].body {
            CompletedBody::Request { path, .. } => {
                assert_eq!(path, &format!("/v1/messages/{newest}"))
            }
            other => panic!("expected request, got {other:?}"),
        }
    }
}
