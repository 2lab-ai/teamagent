//! axum listener + routing: `/llmux/status`, raw `/v1/oauth/token`
//! relay, and a catch-all that forwards everything else upstream (FR1).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, State};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::Router;
use http::{header, HeaderValue, StatusCode};

use super::logging::RequestLogger;
use super::{forward, ProxyError};
use crate::auth::oauth::RefreshCoalescer;
use crate::config::{AccountConfig, AccountCredential, Config};
use crate::dashboard::{self, DashboardHub};
use crate::logging::LogLine;
use crate::provider::anthropic::AnthropicPassthrough;
use crate::scheduler::select::SelectParams;
use crate::scheduler::usage::UsagePoller;
use crate::scheduler::{AccountId, AccountPool, PoolSnapshot};
use crate::tui::{ActivityEvent, ACTIVITY_CHANNEL_CAP};

/// Periodic scheduler re-evaluation (FR3: selection runs on a tick, never
/// per-request). Public so the TUI can show a next-evaluation countdown.
pub const EVALUATE_TICK: Duration = Duration::from_secs(60);

/// Background token-refresh cadence. Each tick refreshes every healthy
/// oauth account whose remaining token lifetime is under
/// `scheduler.refresh_ahead_secs` — so tokens stay fresh with ZERO client
/// traffic (the request-time 5-minute proactive refresh in `forward` stays
/// as defense in depth). The first tick fires immediately at startup.
const REFRESH_TICK: Duration = Duration::from_secs(600);

/// Per-account relayed-traffic totals, owned by the proxy (the scheduler
/// pool deliberately tracks quota windows only; src/scheduler is untouched).
#[derive(Debug, Default)]
pub struct UsageTotals {
    inner: Mutex<HashMap<String, AccountTotals>>,
}

/// Lifetime counters for one account (since proxy start).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AccountTotals {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl UsageTotals {
    pub fn record(
        &self,
        account: &AccountId,
        requests: u64,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = inner.entry(account.0.clone()).or_default();
        entry.requests = entry.requests.saturating_add(requests);
        entry.input_tokens = entry.input_tokens.saturating_add(input_tokens);
        entry.output_tokens = entry.output_tokens.saturating_add(output_tokens);
    }

    pub fn get(&self, account: &AccountId) -> AccountTotals {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&account.0)
            .copied()
            .unwrap_or_default()
    }
}

/// Shared per-request state. Cloning is cheap (`Arc` inside the pool,
/// `reqwest::Client` is internally reference-counted).
#[derive(Clone)]
pub struct AppState {
    pub pool: AccountPool,
    pub client: reqwest::Client,
    pub config: Config,
    /// `None` when request logging is disabled.
    pub logger: Option<Arc<RequestLogger>>,
    /// The Anthropic passthrough provider (byte-identity fast path), held
    /// concretely (the trait's async `auth` is not dyn-compatible). Provider
    /// choice is per-account-credential at forward time: codex credentials
    /// route through [`Self::codex`], everything else through this.
    pub provider: Arc<AnthropicPassthrough>,
    /// The OpenAI Codex provider (Responses API translation) for
    /// `type: "codex"` accounts. Holds the per-process session id.
    pub codex: Arc<crate::provider::codex::CodexProvider>,
    /// Model→backend-group classifier, built from `config.routing`. When
    /// routing is disabled it is the builtin classifier and is never consulted
    /// for routing (forward passes `group = None`); it is still held so the
    /// status/eval paths can ask whether routing is on.
    pub classifier: Arc<crate::routing::Classifier>,
    /// Coalesces concurrent OAuth refreshes per account.
    pub refresher: Arc<RefreshCoalescer>,
    /// Per-account relayed-traffic totals for `/llmux/status`.
    pub totals: Arc<UsageTotals>,
    /// Where refreshed tokens are persisted (read-merge-write). `None`
    /// disables persistence (tests).
    pub config_path: Option<PathBuf>,
    /// Where finished requests are appended + replayed from on startup
    /// (req-persist A/C: stats survive restart, activity records kept with no
    /// retention). `None` disables activity persistence (unit tests; or no
    /// resolvable state dir). Defaults in [`Self::new`] to the state-dir
    /// `activity.jsonl`; e2e/integration callers override it to a tempdir so a
    /// driven request never touches the user's real log — same pattern as
    /// `config_path`.
    pub activity_log_path: Option<PathBuf>,
    /// Where raw request/response payloads are appended (Feature B) + pruned to
    /// `config.raw_io.retention_days` on startup. DISTINCT from
    /// [`Self::activity_log_path`] (which holds per-request metadata): this holds
    /// the actual payload bytes. `None` disables capture (unit tests; or no
    /// resolvable state dir). Defaults in [`Self::new`] to the state-dir
    /// `raw-io.jsonl`; e2e/integration callers override it to a tempdir so a
    /// driven request never touches the user's real log — same pattern as
    /// `activity_log_path`.
    pub raw_io_path: Option<PathBuf>,
    /// Activity feed emit side. The proxy / poller / refresher `try_send` and
    /// drop on full — best-effort observability, never backpressure (see
    /// `tui::event`). The matching receiver is folded into [`Self::hub`] by
    /// the `dashboard::fold` task `serve` spawns.
    pub events: Option<tokio::sync::mpsc::Sender<ActivityEvent>>,
    /// Server-owned dashboard fold (activity ring, totals, last switch,
    /// poller health, log console). The local TUI renders it directly; the
    /// `GET /llmux/dashboard` endpoint serializes it.
    pub hub: Arc<DashboardHub>,
    /// Activity-event receiver, taken by `serve` to spawn the fold task.
    /// `Mutex<Option<_>>` so `AppState` stays `Clone` (the receiver is a
    /// single-consumer resource — only the first `serve` takes it).
    pending_events: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<ActivityEvent>>>>,
    /// Tracing-bridge receiver (TUI mode only — the `RUST_LOG` channel feed
    /// into the hub's log console). `None` in plain/daemon mode, where the
    /// fold re-traces activity events so `server.log` keeps the history.
    pending_logs: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<LogLine>>>>,
    /// Per-process request id source for activity-event correlation.
    pub request_counter: Arc<AtomicU64>,
    /// Server start, for `/llmux/status` uptime.
    pub started: Instant,
    /// Actually bound port (config port until `serve` binds; the OS-assigned
    /// port afterwards — matters for `proxy.port = 0` test servers).
    pub bound_port: Arc<AtomicU16>,
    /// Graceful-shutdown trigger fired by `POST /llmux/shutdown`.
    pub shutdown: Arc<tokio::sync::Notify>,
}

impl AppState {
    /// Build the shared state. The activity-event channel is created here:
    /// the emit `Sender` lands in [`Self::events`] and the matching `Receiver`
    /// is parked in [`Self::pending_events`] for `serve` to fold into the hub.
    /// `logs_rx` is the optional tracing-bridge feed (TUI mode); its absence
    /// is what tells the fold to re-trace activity events into `server.log`
    /// (daemon parity).
    pub fn new(
        config: Config,
        pool: AccountPool,
        logger: Option<Arc<RequestLogger>>,
        logs_rx: Option<tokio::sync::mpsc::Receiver<LogLine>>,
    ) -> Result<Self, ProxyError> {
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(ACTIVITY_CHANNEL_CAP);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let provider = Arc::new(AnthropicPassthrough::new(config.upstream.clone()));
        let codex = Arc::new(crate::provider::codex::CodexProvider::with_shape(
            config.codex.upstream.clone(),
            crate::provider::codex::CodexShape::from_config(&config.codex),
        ));
        // The classifier is built from config.routing whether or not routing
        // is enabled (it is simply not consulted on the forward path while
        // disabled — forward passes group = None).
        let classifier = Arc::new(crate::routing::Classifier::from_config(
            &config.routing.claude_models,
            &config.routing.codex_models,
            &config.routing.default_group,
        ));
        // A non-default upstream (staging, e2e mock) must also receive the
        // proxy's OWN token refreshes — otherwise refresh traffic would leak
        // to the production endpoint while everything else is redirected.
        let refresher = if config.upstream == crate::config::schema::DEFAULT_UPSTREAM {
            RefreshCoalescer::new()
        } else {
            RefreshCoalescer::with_token_url(format!(
                "{}/v1/oauth/token",
                config.upstream.trim_end_matches('/')
            ))
        };
        Ok(Self {
            pool,
            client,
            logger,
            provider,
            codex,
            classifier,
            refresher: Arc::new(refresher),
            totals: Arc::new(UsageTotals::default()),
            config_path: crate::config::config_path().ok(),
            activity_log_path: crate::cli::daemon::activity_log_path(),
            raw_io_path: crate::cli::daemon::raw_io_path(),
            bound_port: Arc::new(AtomicU16::new(config.proxy.port)),
            config,
            events: Some(events_tx),
            hub: Arc::new(DashboardHub::default()),
            pending_events: Arc::new(Mutex::new(Some(events_rx))),
            pending_logs: Arc::new(Mutex::new(logs_rx)),
            request_counter: Arc::new(AtomicU64::new(0)),
            started: Instant::now(),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        })
    }

    pub fn select_params(&self) -> SelectParams {
        SelectParams::from(&self.config.scheduler)
    }

    /// Next activity-event correlation id (never leaves this process). 1-based
    /// to match [`RequestLogger::next_request_id`]: the first request is id 1,
    /// so the codex trace, the request log, and the dashboard feed all show the
    /// same ascending ids. A bare `fetch_add` would return 0 for the first
    /// request, which then surfaced as `"id":0` on every trace line in a
    /// single-request session.
    pub fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Emit an activity event: `try_send`, dropped on a full channel.
    pub fn emit(&self, event: ActivityEvent) {
        if let Some(events) = &self.events {
            let _ = events.try_send(event);
        }
    }

    /// Add (upsert) an API-key account: read-merge-write the config, then swap
    /// the merged roster into the live pool and re-select so the running daemon
    /// serves it with no restart. The SINGLE in-process implementation behind
    /// both the local TUI `a`-key path and the `POST /llmux/add-account`
    /// endpoint. `name = None` assigns the next `api-N` on the FRESH on-disk
    /// state. The api key is never logged. Returns `(resolved_name, outcome)`.
    pub fn add_apikey_account(
        &self,
        name: Option<&str>,
        api_key: &str,
    ) -> Result<(String, crate::config::Upsert), crate::config::ConfigError> {
        let Some(path) = &self.config_path else {
            return Err(crate::config::ConfigError::NoConfigDir);
        };
        let mut resolved = String::new();
        let mut outcome = crate::config::Upsert::Added;
        let merged = crate::config::update_path(path, |c| {
            resolved = match name {
                Some(n) => n.to_string(),
                None => {
                    let next = c
                        .accounts
                        .iter()
                        .filter(|a| a.name.starts_with("api-"))
                        .count()
                        + 1;
                    format!("api-{next}")
                }
            };
            outcome = c.upsert_account(AccountConfig {
                name: resolved.clone(),
                credential: AccountCredential::Apikey {
                    api_key: api_key.to_string(),
                },
            });
        })?;
        self.apply_roster(&merged);
        // Names are not credentials; the api key never reaches a log line.
        tracing::info!(account = %resolved, action = ?outcome, "account added");
        Ok((resolved, outcome))
    }

    /// Remove an account by name: read-merge-write removal, then reload the
    /// live pool. The SINGLE in-process implementation behind both the local
    /// TUI `r`-key path and the `POST /llmux/remove-account` endpoint. Returns
    /// `Ok(true)` when an account was removed, `Ok(false)` when none matched.
    pub fn remove_account(&self, name: &str) -> Result<bool, crate::config::ConfigError> {
        let Some(path) = &self.config_path else {
            return Err(crate::config::ConfigError::NoConfigDir);
        };
        let mut removed = false;
        let merged = crate::config::update_path(path, |c| {
            removed = c.remove_account(name);
        })?;
        if removed {
            self.apply_roster(&merged);
            tracing::info!(account = %name, "account removed");
        }
        Ok(removed)
    }

    /// Inject (upsert) a fully-formed OAuth/Codex account: read-merge-write
    /// the config, then swap the merged roster into the live pool so the
    /// running daemon serves it with no restart. The SINGLE in-process
    /// implementation behind both the local TUI `n`-key path (login runs in
    /// the client, the resulting credential is injected in-process) and the
    /// `POST /llmux/inject-account` endpoint (attach mode: the client relays
    /// the credential it minted to the daemon). Dedup is by `account_uuid` /
    /// `account_id` then `name` (see [`Config::upsert_account`]), so a
    /// re-login updates the existing entry rather than duplicating it.
    ///
    /// The caller hands over an already-built [`AccountConfig`] carrying an
    /// `Oauth` or `Codex` credential; an `Apikey` credential is rejected (use
    /// [`Self::add_apikey_account`] for those). No token is ever logged.
    /// Returns `(resolved_name, outcome)`.
    pub fn inject_account(
        &self,
        account: AccountConfig,
    ) -> Result<(String, crate::config::Upsert), crate::config::ConfigError> {
        if matches!(account.credential, AccountCredential::Apikey { .. }) {
            return Err(crate::config::ConfigError::Invalid(
                "inject_account accepts only oauth/codex credentials".into(),
            ));
        }
        let Some(path) = &self.config_path else {
            return Err(crate::config::ConfigError::NoConfigDir);
        };
        let name = account.name.clone();
        let kind = account.credential.kind();
        let mut outcome = crate::config::Upsert::Added;
        let merged = crate::config::update_path(path, |c| {
            outcome = c.upsert_account(account.clone());
        })?;
        self.apply_roster(&merged);
        // Names/kinds are not credentials; no token reaches a log line.
        tracing::info!(account = %name, kind, action = ?outcome, "account injected");
        Ok((name, outcome))
    }

    /// Swap a freshly-merged config's roster into the live pool and re-select
    /// every backend group (a removed `current` is cleared by
    /// `reload_accounts`; the re-eval picks a replacement). Shared tail of
    /// [`Self::add_apikey_account`] / [`Self::remove_account`].
    fn apply_roster(&self, merged: &Config) {
        self.pool.reload_accounts(&merged.accounts);
        let params = self.select_params();
        let now = SystemTime::now();
        for group in eval_groups(&self.pool, self.config.routing.enabled) {
            self.pool.evaluate(group, &params, now);
        }
    }
}

/// Run the proxy until shutdown: bind `config.proxy.port`, spawn the usage
/// poller and the re-evaluation tick next to the listener, serve [`router`].
///
/// Binds all interfaces (teamclaude parity — the proxy api key with
/// loopback exemption exists precisely for non-local peers).
pub async fn run(
    config: Config,
    pool: AccountPool,
    log_dir: Option<PathBuf>,
    logs_rx: Option<tokio::sync::mpsc::Receiver<LogLine>>,
) -> Result<(), ProxyError> {
    let logger = match log_dir {
        Some(dir) => {
            let logger = RequestLogger::new(dir.clone())?;
            tracing::info!(dir = %dir.display(), "request logging enabled");
            Some(Arc::new(logger))
        }
        None => None,
    };
    let state = AppState::new(config, pool, logger, logs_rx)?;
    serve(state, None).await
}

/// [`run`] over a pre-built [`AppState`]: prime usage state, run the initial
/// selection, spawn the poller + evaluation tick, bind, and serve.
///
/// `ready` (when given) receives the actual bound address once listening —
/// the seam for `proxy.port = 0` callers (e2e tests) that need the
/// OS-assigned port.
pub async fn serve(
    state: AppState,
    ready: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> Result<(), ProxyError> {
    let params = state.select_params();

    // Arm activity persistence + resume cumulative model/account stats from the
    // persisted log (req-persist A/C). Done once here — the single production
    // serve chokepoint — before the fold task starts appending new finished
    // requests, so a restarted daemon continues the totals instead of resetting
    // them. The path comes from `state` (state dir for a real daemon, a tempdir
    // for e2e), so this never touches the user's real log under test.
    // Best-effort: a missing log leaves an empty hub; `None` disables it.
    state.hub.arm_persistence(state.activity_log_path.clone());

    // Prune the raw-io payload log (Feature B) to its retention window, once at
    // startup, alongside activity persistence. Guarded by config: `enabled =
    // false` skips it, `retention_days = 0` keeps everything (prune is a no-op).
    // Best-effort and total — a missing/corrupt log never fails startup (see
    // `proxy::raw_io::prune`). The path is `None` under unit/e2e callers with no
    // state dir, which is itself a no-op.
    if state.config.raw_io.enabled {
        crate::proxy::raw_io::prune(
            state.raw_io_path.as_deref(),
            state.config.raw_io.retention_days,
            crate::proxy::raw_io::now_ms(),
        );
    }

    // Dashboard fold: the single consumer of the activity-event channel (and,
    // in TUI mode, the tracing-bridge channel) into the hub. Spawned once —
    // the receiver is taken out of `pending_events`/`pending_logs`. Without a
    // bridge feed (plain/daemon mode) the fold also re-traces each activity
    // event so `server.log` keeps the request history the TUI would show.
    let fold_events = state
        .pending_events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let fold_logs = state
        .pending_logs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let fold_task = fold_events.map(|events| {
        let trace_events = fold_logs.is_none();
        tokio::spawn(dashboard::fold(
            state.hub.clone(),
            events,
            fold_logs,
            trace_events,
        ))
    });

    // Background: active usage polling (FR3) + periodic re-evaluation tick.
    // One priming pass runs BEFORE the initial selection so the very first
    // pick already ranks by real window state (soonest 7d reset) instead of
    // falling back to cold-account id order.
    let mut poller = UsagePoller::new(
        state.pool.clone(),
        state.client.clone(),
        state.config.upstream.clone(),
        state.config.scheduler,
    )
    .with_events(state.events.clone());
    poller.prime(SystemTime::now()).await;

    // Initial selection so the first request doesn't pay for it. Evaluate
    // every backend group that has at least one account (with routing
    // disabled this is just the legacy slot — `evaluate(None, ..)`).
    for group in eval_groups(&state.pool, state.config.routing.enabled) {
        state.pool.evaluate(group, &params, SystemTime::now());
    }
    // Announce each group's initial selection (req1 symmetry): claude and codex
    // both surface in the activity log, not just the representative slot.
    for current in state.pool.snapshot().current.values() {
        state.emit(ActivityEvent::AccountSwitched {
            from: None,
            to: current.0.clone(),
            reason: Some("initial selection".into()),
        });
    }

    let poller_task = tokio::spawn(poller.run());

    // Background token refresh (A2): first tick immediately, then every
    // REFRESH_TICK. Lives next to the usage poller, aborted on shutdown.
    let refresh_state = state.clone();
    let refresh_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(REFRESH_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            background_refresh_pass(&refresh_state).await;
        }
    });

    let tick_pool = state.pool.clone();
    let tick_events = state.events.clone();
    let tick_routing_enabled = state.config.routing.enabled;
    let tick_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(EVALUATE_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            for group in eval_groups(&tick_pool, tick_routing_enabled) {
                let slot = group.unwrap_or(crate::routing::BackendGroup::Claude);
                let before = tick_pool.snapshot().current.get(&slot).cloned();
                let decision = tick_pool.evaluate(group, &params, SystemTime::now());
                tracing::debug!(?group, ?decision, "evaluation tick");
                if let crate::scheduler::select::Decision::Switch { to } = decision {
                    if let Some(events) = &tick_events {
                        let _ = events.try_send(ActivityEvent::AccountSwitched {
                            from: before.map(|id| id.0),
                            to: to.0,
                            reason: Some("re-evaluation".into()),
                        });
                    }
                }
            }
        }
    });

    let port = state.config.proxy.port;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|source| ProxyError::Bind { port, source })?;
    let local_addr = listener.local_addr().map_err(ProxyError::Io)?;
    state.bound_port.store(local_addr.port(), Ordering::Relaxed);
    if let Some(ready) = ready {
        let _ = ready.send(local_addr);
    }
    tracing::info!(
        port = local_addr.port(),
        upstream = %state.config.upstream,
        accounts = state.config.accounts.len(),
        "proxy listening (ANTHROPIC_BASE_URL=http://localhost:{})",
        local_addr.port()
    );
    let shutdown = state.shutdown.clone();
    let result = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.notified().await })
    .await;
    poller_task.abort();
    tick_task.abort();
    refresh_task.abort();
    if let Some(fold_task) = fold_task {
        fold_task.abort();
    }
    result.map_err(ProxyError::Io)
}

/// The set of group filters to evaluate on each scheduler tick. With routing
/// DISABLED this is a single `[None]` — the legacy single-slot path, byte-for
/// -byte the old behavior. With routing ENABLED it is one `Some(group)` per
/// distinct backend group that has at least one account in the pool, so each
/// group's sticky slot is kept current independently. Groups with no accounts
/// are skipped (nothing to select).
pub fn eval_groups(
    pool: &AccountPool,
    routing_enabled: bool,
) -> Vec<Option<crate::routing::BackendGroup>> {
    if !routing_enabled {
        return vec![None];
    }
    let mut groups: Vec<crate::routing::BackendGroup> =
        pool.snapshot().accounts.iter().map(|a| a.group).collect();
    groups.sort();
    groups.dedup();
    groups.into_iter().map(Some).collect()
}

/// One background-refresh pass (A2): refresh every HEALTHY oauth-style
/// account (anthropic oauth AND codex) whose access token expires within
/// `scheduler.refresh_ahead_secs`. Reuses the request-time path
/// ([`forward::refresh_credential`]: coalescer for anthropic, direct token
/// grant for codex, pool update, persistence). Auth-failed accounts are
/// skipped — a dead refresh token must not be retried every tick (re-login
/// heals via `update_credential`); transient failures are simply retried on
/// the next tick.
pub async fn background_refresh_pass(state: &AppState) {
    let ahead_ms = state
        .config
        .scheduler
        .refresh_ahead_secs
        .saturating_mul(1000);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    // Refresh AT MOST ONE account per pass — the soonest-to-expire. Refreshing
    // every expiring token in one pass was a burst of token-endpoint calls
    // (worst on startup, when many tokens sit inside the 7h window at once),
    // which can trip the upstream's request-rate limit. One-per-pass spaces
    // them across REFRESH_TICK; tokens carry hours of headroom so the sweep has
    // plenty of time, and request-time forced refresh is the backstop.
    let mut candidate: Option<(AccountId, AccountCredential, u64)> = None;
    for account in state.pool.snapshot().accounts {
        if !account.healthy {
            continue;
        }
        let Some(credential) = state.pool.credential(&account.id) else {
            continue;
        };
        let expires_at_ms = match &credential {
            AccountCredential::Oauth { expires_at_ms, .. }
            | AccountCredential::Codex { expires_at_ms, .. } => *expires_at_ms,
            AccountCredential::Apikey { .. } => continue,
        };
        if expires_at_ms.saturating_sub(now_ms) >= ahead_ms {
            continue;
        }
        if candidate
            .as_ref()
            .is_none_or(|(_, _, soonest)| expires_at_ms < *soonest)
        {
            candidate = Some((account.id.clone(), credential, expires_at_ms));
        }
    }
    let Some((account_id, credential, _)) = candidate else {
        return;
    };
    match forward::refresh_credential(state, &account_id, &credential).await {
        forward::RefreshOutcome::Refreshed(fresh) => {
            if let AccountCredential::Oauth { expires_at_ms, .. }
            | AccountCredential::Codex { expires_at_ms, .. } = fresh
            {
                let hours = expires_at_ms.saturating_sub(now_ms) as f64 / 3_600_000.0;
                tracing::info!(
                    account = %account_id,
                    "background token refresh: expires in {hours:.1}h"
                );
            }
        }
        forward::RefreshOutcome::Permanent => {
            state.pool.record_auth_failure(&account_id);
            state.emit(ActivityEvent::Error {
                context: Some("refresh".into()),
                message: format!("{account_id}: refresh token dead; re-login required"),
            });
        }
        forward::RefreshOutcome::Failed => {} // transient — next tick retries
    }
}

/// Build the router: `GET /llmux/status`, `POST /llmux/shutdown`,
/// `POST /v1/oauth/token` (raw relay), fallback → [`forward_any`]. Every
/// route sits behind the proxy api-key check (loopback peers exempt).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/llmux/status", get(status))
        .route("/llmux/dashboard", get(dashboard_endpoint))
        .route("/llmux/switch", post(switch_endpoint))
        .route("/llmux/codex", post(codex_config_endpoint))
        .route("/llmux/add-account", post(add_account_endpoint))
        .route("/llmux/inject-account", post(inject_account_endpoint))
        .route("/llmux/remove-account", post(remove_account_endpoint))
        .route("/llmux/shutdown", post(shutdown))
        .route("/v1/oauth/token", post(oauth_token_relay))
        .fallback(forward_any)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            client_auth,
        ))
        .with_state(state)
}

/// Pure client-auth decision (FR1): when a proxy api key is configured,
/// non-loopback peers must present it as `x-api-key`; loopback peers are
/// exempt. An unknown peer address (no ConnectInfo) is NOT exempt.
pub fn client_auth_ok(
    required: Option<&str>,
    peer: Option<std::net::IpAddr>,
    presented: Option<&str>,
) -> bool {
    match required {
        None => true,
        Some(key) => presented == Some(key) || peer.is_some_and(|ip| ip.is_loopback()),
    }
}

async fn client_auth(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let presented = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());
    if client_auth_ok(state.config.proxy.api_key.as_deref(), peer, presented) {
        next.run(req).await
    } else {
        let body = serde_json::json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "Invalid proxy API key" },
        });
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response()
    }
}

fn epoch_secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Server-process facts for `/llmux/status` that are not pool state.
#[derive(Debug, Clone, Copy)]
pub struct ServerMeta {
    pub pid: u32,
    pub uptime_secs: u64,
    pub port: u16,
}

/// Serializable `/llmux/status` document — pure function of a pool
/// snapshot + totals + select params + server meta so the shape is
/// unit-testable without a socket. Fields are additive only (the CLI parses
/// this across versions). The `accounts` array is emitted in the
/// scheduler's selection order (B1: current → eligible by rank →
/// ineligible) with a 1-based `order` field and, for ineligible accounts, a
/// `blocked` reason string.
pub fn status_json(
    snapshot: &PoolSnapshot,
    totals: &UsageTotals,
    params: &SelectParams,
    now: SystemTime,
    meta: &ServerMeta,
) -> serde_json::Value {
    let window = |w: &Option<crate::scheduler::window::QuotaWindow>| match w {
        Some(w) => serde_json::json!({
            "utilization": w.effective_utilization(now),
            "resets_at": epoch_secs(w.resets_at),
            "resets_in_secs": w.resets_at
                .duration_since(now)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }),
        None => serde_json::Value::Null,
    };
    let headers_only = crate::scheduler::select::headers_only_mode(snapshot, params, None, now);
    let accounts: Vec<serde_json::Value> =
        crate::scheduler::select::selection_order(snapshot, params, now)
            .into_iter()
            .enumerate()
            .map(|(order, idx)| {
                let account = &snapshot.accounts[idx];
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
                let blocked =
                    crate::scheduler::select::eligibility(account, params, now, headers_only).map(
                        |reason| {
                            crate::scheduler::select::blocking_reason(account, reason, params, now)
                        },
                    );
                let lifetime = totals.get(&account.id);
                serde_json::json!({
                    "name": account.id.0,
                    "type": account.credential_kind,
                    "status": status,
                    "order": order + 1,
                    "blocked": blocked,
                    "five_hour": window(&account.five_hour),
                    "seven_day": window(&account.seven_day),
                    "cooldown_until": account.cooldown_until.filter(|_| cooling).map(epoch_secs),
                    "in_flight": account.in_flight,
                    // Token health (additive): expiry + last refresh, epoch
                    // ms; null for apikey accounts / unknown expiry / never
                    // refreshed.
                    "token_expires_at_ms": account.token_expires_at_ms,
                    "last_refresh_ms": account.last_refresh_ms,
                    "totals": {
                        "requests": lifetime.requests,
                        "input_tokens": lifetime.input_tokens,
                        "output_tokens": lifetime.output_tokens,
                    },
                })
            })
            .collect();
    // Additive: `current` stays a representative scalar (claude slot if
    // present, else codex) for back-compat with older CLI parsers; the new
    // `current_by_group` object exposes the per-group sticky slots.
    let current_by_group: serde_json::Map<String, serde_json::Value> = snapshot
        .current
        .iter()
        .map(|(group, id)| (group.as_str().to_string(), serde_json::json!(id.0)))
        .collect();
    serde_json::json!({
        "version": crate::build_info::version_string(),
        "pid": meta.pid,
        "uptime_secs": meta.uptime_secs,
        "port": meta.port,
        "current": snapshot.representative_current().map(|c| c.0.clone()),
        "current_by_group": current_by_group,
        "accounts": accounts,
    })
}

/// `GET /llmux/status` — JSON scheduler/account state (pool snapshot,
/// current account, cooldowns, build info, pid/uptime/port).
async fn status(State(state): State<AppState>) -> Response {
    let meta = ServerMeta {
        pid: std::process::id(),
        uptime_secs: state.started.elapsed().as_secs(),
        port: state.bound_port.load(Ordering::Relaxed),
    };
    let body = status_json(
        &state.pool.snapshot(),
        &state.totals,
        &state.select_params(),
        SystemTime::now(),
        &meta,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{body:#}"),
    )
        .into_response()
}

/// `GET /llmux/dashboard` — the [`crate::dashboard::DashboardDoc`]: a
/// strict superset of `/llmux/status` (same account fields and ordering)
/// plus scheduler / poller / totals / activity / log state. Behind the same
/// loopback + proxy-api-key gate as every route. The attach-mode client
/// (`llmux dashboard`) polls this; the local TUI builds the same document
/// in-process — one contract, one renderer.
async fn dashboard_endpoint(State(state): State<AppState>) -> Response {
    let doc = dashboard::build_doc(&state, SystemTime::now());
    match serde_json::to_string(&doc) {
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        Err(err) => relay_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("dashboard serialize failed: {err}"),
        ),
    }
}

/// Request body for `POST /llmux/switch`.
#[derive(serde::Deserialize)]
struct SwitchRequest {
    account: String,
}

/// `POST /llmux/switch` `{"account":"<name>"}` — manual account switch,
/// the server-side of the dashboard's `s`-key path. Same gate as every route
/// (loopback exempt, otherwise the proxy api key). Runs the identical
/// `AccountPool::switch_to` the in-process TUI calls, emits the
/// `AccountSwitched` activity event on success, and answers `{"ok":true,
/// "current":"<name>"}`. A refused switch (ineligible / unknown account)
/// is a 409 with the scheduler's own refusal reason.
async fn switch_endpoint(
    State(state): State<AppState>,
    body: axum::extract::Json<SwitchRequest>,
) -> Response {
    let target = AccountId(body.account.clone());
    let now = SystemTime::now();
    let from = state
        .pool
        .snapshot()
        .representative_current()
        .map(|c| c.0.clone());
    match state.pool.switch_to(&target, &state.select_params(), now) {
        Ok(()) => {
            state.emit(ActivityEvent::AccountSwitched {
                from,
                to: target.0.clone(),
                reason: Some("manual".into()),
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::json!({ "ok": true, "current": target.0 }).to_string(),
            )
                .into_response()
        }
        Err(err) => relay_error(StatusCode::CONFLICT, &format!("switch refused: {err}")),
    }
}

/// Partial update for `POST /llmux/codex` (req8.1 — dashboard codex
/// settings). Every field is optional; an omitted field keeps its current
/// value. For `reasoning_effort`, an empty string or `"unset"` clears it
/// (back to the backend default). Applies to the LIVE provider immediately and
/// persists to the config file so it survives a restart.
#[derive(serde::Deserialize)]
struct CodexConfigRequest {
    fast: Option<bool>,
    default_model: Option<String>,
    reasoning_effort: Option<String>,
}

async fn codex_config_endpoint(
    State(state): State<AppState>,
    body: axum::extract::Json<CodexConfigRequest>,
) -> Response {
    let mut shape = state.codex.shape();
    if let Some(fast) = body.fast {
        shape.fast = fast;
    }
    if let Some(model) = body.default_model.as_deref() {
        if !model.trim().is_empty() {
            shape.model = model.trim().to_string();
        }
    }
    if let Some(effort) = body.reasoning_effort.as_deref() {
        let e = effort.trim();
        shape.effort = if e.is_empty() || e.eq_ignore_ascii_case("unset") {
            None
        } else {
            Some(e.to_ascii_lowercase())
        };
    }
    // Apply live (takes effect on the next request) ...
    state.codex.set_shape(shape.clone());
    // ... and persist so it survives a daemon restart (best-effort).
    if let Some(path) = &state.config_path {
        let _ = crate::config::update_path(path, |c| {
            c.codex.default_model = shape.model.clone();
            c.codex.fast = shape.fast;
            c.codex.reasoning_effort = shape.effort.clone();
        });
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "ok": true,
            "fast": shape.fast,
            "default_model": shape.model,
            "reasoning_effort": shape.effort,
        })
        .to_string(),
    )
        .into_response()
}

/// Request body for `POST /llmux/add-account` — an API-key account
/// (issue #3; OAuth/codex login-from-dashboard is issue #4, out of scope).
/// `name` is optional: when omitted the server assigns the next `api-N` name,
/// mirroring `cli::login::login_api`.
#[derive(serde::Deserialize)]
struct AddAccountRequest {
    #[serde(default)]
    name: Option<String>,
    api_key: String,
}

/// `POST /llmux/add-account` `{"api_key":"...","name":"..."?}` — add (upsert)
/// an API-key account from the dashboard, in BOTH local and attach mode. Same
/// loopback / proxy-api-key gate as every route (it sits on the shared
/// `.route(...)` chain behind `client_auth`). The credential is written
/// read-merge-write via [`crate::config::update_path`] (never load/edit/save
/// around the running server) and the live pool is reloaded so the daemon
/// picks it up with no restart. The api key is NEVER logged and the response
/// echoes only a masked form (`crate::proxy::logging::mask_credentials`).
async fn add_account_endpoint(
    State(state): State<AppState>,
    body: axum::extract::Json<AddAccountRequest>,
) -> Response {
    let api_key = body.api_key.trim();
    if api_key.is_empty() {
        return relay_error(StatusCode::BAD_REQUEST, "api_key is required");
    }
    let requested_name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());

    match state.add_apikey_account(requested_name, api_key) {
        Ok((name, outcome)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({
                "ok": true,
                "name": name,
                "type": "apikey",
                "added": matches!(outcome, crate::config::Upsert::Added),
                // Masked echo only — never the raw key (AGENTS.md credential rule).
                "api_key_masked": crate::proxy::logging::mask_credentials(api_key),
            })
            .to_string(),
        )
            .into_response(),
        Err(crate::config::ConfigError::NoConfigDir) => relay_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "config persistence disabled; cannot add account",
        ),
        Err(err) => relay_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write failed: {err}"),
        ),
    }
}

/// Request body for `POST /llmux/inject-account` (issue #4) — a fully-formed
/// OAuth/Codex credential the CLIENT minted by running the browser login
/// locally, relayed to the daemon so the new account joins the pool with no
/// restart. The body deserializes straight into an [`AccountConfig`] (the
/// `type`-tagged credential enum), so `{"name":"claude:me","type":"oauth",
/// "account_uuid":"…","access_token":"…","refresh_token":"…",
/// "expires_at_ms":…}` and the `type:"codex"` shape both parse. An
/// `type:"apikey"` body is rejected by [`AppState::inject_account`] — api-key
/// accounts use `/llmux/add-account`, which never needs a browser.
#[derive(serde::Deserialize)]
struct InjectAccountRequest {
    #[serde(flatten)]
    account: AccountConfig,
}

/// `POST /llmux/inject-account` — inject an OAuth/Codex account from the
/// dashboard, in BOTH local and attach mode. This is the daemon side of the
/// issue #4 architecture: the CLIENT runs the OAuth browser+callback flow
/// (local = the daemon host; attach = the operator's machine) and POSTs the
/// resulting token here, making local and attach ONE code path. Same loopback
/// / proxy-api-key gate as every route (it sits on the shared `.route(...)`
/// chain behind `client_auth`). The credential is written read-merge-write via
/// [`crate::config::update_path`] and the live pool is reloaded so the daemon
/// picks it up with no restart. NO token is ever logged; the response echoes
/// only the account name, kind, and a MASKED access token
/// (`crate::proxy::logging::mask_credentials`).
async fn inject_account_endpoint(
    State(state): State<AppState>,
    body: axum::extract::Json<InjectAccountRequest>,
) -> Response {
    let account = body.0.account;
    if account.name.trim().is_empty() {
        return relay_error(StatusCode::BAD_REQUEST, "account name is required");
    }
    // Capture a masked echo of the access token BEFORE moving the account into
    // the upsert — never the raw token (AGENTS.md credential rule).
    let access_token_masked = match &account.credential {
        AccountCredential::Oauth { access_token, .. }
        | AccountCredential::Codex { access_token, .. } => {
            Some(crate::proxy::logging::mask_credentials(access_token))
        }
        AccountCredential::Apikey { .. } => None,
    };
    let kind = account.credential.kind();

    match state.inject_account(account) {
        Ok((name, outcome)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({
                "ok": true,
                "name": name,
                "type": kind,
                "added": matches!(outcome, crate::config::Upsert::Added),
                "access_token_masked": access_token_masked,
            })
            .to_string(),
        )
            .into_response(),
        Err(crate::config::ConfigError::Invalid(msg)) => relay_error(StatusCode::BAD_REQUEST, &msg),
        Err(crate::config::ConfigError::NoConfigDir) => relay_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "config persistence disabled; cannot inject account",
        ),
        Err(err) => relay_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write failed: {err}"),
        ),
    }
}

/// Request body for `POST /llmux/remove-account`. `confirm` must be `true` —
/// a destructive delete requires explicit confirmation (matches the CLI's
/// `remove --yes` gate); the TUI supplies it via a second-key confirm.
#[derive(serde::Deserialize)]
struct RemoveAccountRequest {
    name: String,
    #[serde(default)]
    confirm: bool,
}

/// `POST /llmux/remove-account` `{"name":"...","confirm":true}` — remove an
/// account from the dashboard in BOTH local and attach mode. Same gate as
/// every route. Read-merge-write removal via [`crate::config::update_path`]
/// (preserves every other account) and a live pool reload so the change takes
/// effect with no restart. Refuses without `confirm: true` (a 400) so a
/// destructive delete is never silent.
async fn remove_account_endpoint(
    State(state): State<AppState>,
    body: axum::extract::Json<RemoveAccountRequest>,
) -> Response {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return relay_error(StatusCode::BAD_REQUEST, "name is required");
    }
    if !body.confirm {
        return relay_error(
            StatusCode::BAD_REQUEST,
            &format!("refusing to remove {name:?} without confirmation; set confirm=true"),
        );
    }

    match state.remove_account(&name) {
        Ok(true) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({ "ok": true, "name": name, "removed": true }).to_string(),
        )
            .into_response(),
        Ok(false) => relay_error(
            StatusCode::NOT_FOUND,
            &format!("account {name:?} not found"),
        ),
        Err(crate::config::ConfigError::NoConfigDir) => relay_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "config persistence disabled; cannot remove account",
        ),
        Err(err) => relay_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write failed: {err}"),
        ),
    }
}

/// `POST /llmux/shutdown` — graceful server exit (same loopback /
/// proxy-api-key rules as every route, via the shared middleware). The 200
/// is delivered before the process exits: hyper's graceful shutdown stops
/// accepting new connections and completes in-flight responses first.
async fn shutdown(State(state): State<AppState>) -> Response {
    tracing::info!("shutdown requested via /llmux/shutdown");
    state.shutdown.notify_one();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"ok":true}"#.to_string(),
    )
        .into_response()
}

/// `POST /v1/oauth/token` relayed RAW to upstream — Claude Code's own token
/// refresh passes through untouched (no auth rewrite, no account lease;
/// intercepting client refreshes would cause token-rotation conflicts).
/// Like the Node reference, only `content-type` / `accept` / `user-agent`
/// travel upstream.
async fn oauth_token_relay(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            return relay_error(StatusCode::BAD_REQUEST, &format!("body read failed: {err}"))
        }
    };
    let path_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let url = format!(
        "{}{}",
        state.config.upstream.trim_end_matches('/'),
        path_query
    );
    let mut builder = state.client.post(url);
    for name in [header::CONTENT_TYPE, header::ACCEPT, header::USER_AGENT] {
        if let Some(value) = parts.headers.get(&name) {
            builder = builder.header(name, value);
        }
    }
    if !body.is_empty() {
        builder = builder.body(body);
    }
    let upstream = match builder.send().await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(error = %err, "oauth token relay failed");
            return relay_error(StatusCode::BAD_GATEWAY, "Upstream unreachable");
        }
    };
    let status = upstream.status();
    let mut headers = upstream.headers().clone();
    for name in ["transfer-encoding", "connection", "content-length"] {
        headers.remove(name);
    }
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "oauth token relay body failed");
            return relay_error(StatusCode::BAD_GATEWAY, "Upstream body read failed");
        }
    };
    let mut response = Response::new(axum::body::Body::from(bytes));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn relay_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": "proxy_error", "message": message },
    });
    let mut response = Response::new(axum::body::Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Catch-all: buffer, lease, rewrite, forward upstream, stream back
/// (see `forward`).
async fn forward_any(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    forward::forward(&state, req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountConfig, AccountCredential};
    use crate::scheduler::headers::{ParsedRateLimitHeaders, WindowReading};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
                api_key: format!("sk-ant-api03-{name}"),
            },
        }
    }

    #[test]
    fn client_auth_no_key_configured_allows_everyone() {
        assert!(client_auth_ok(None, None, None));
        assert!(client_auth_ok(
            None,
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7))),
            None
        ));
    }

    #[test]
    fn client_auth_loopback_is_exempt() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert!(client_auth_ok(Some("lm-secret"), Some(v4), None));
        assert!(client_auth_ok(Some("lm-secret"), Some(v6), None));
    }

    #[test]
    fn client_auth_remote_requires_matching_key() {
        let remote = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        assert!(client_auth_ok(
            Some("lm-secret"),
            Some(remote),
            Some("lm-secret")
        ));
        assert!(!client_auth_ok(Some("lm-secret"), Some(remote), None));
        assert!(!client_auth_ok(
            Some("lm-secret"),
            Some(remote),
            Some("wrong")
        ));
        assert!(
            !client_auth_ok(Some("lm-secret"), None, None),
            "unknown peer is not exempt"
        );
    }

    fn params() -> SelectParams {
        SelectParams {
            five_hour_max: 0.90,
            seven_day_max: 0.99,
            usage_max_age: Duration::from_secs(600),
        }
    }

    #[test]
    fn next_request_id_is_one_based_and_ascending() {
        let config = Config {
            accounts: vec![oauth_account("a")],
            ..Default::default()
        };
        let pool = AccountPool::new(&config.accounts);
        let state = AppState::new(config, pool, None, None).expect("state");
        // The first request must be id 1, not 0: the codex trace, the request
        // log, and the dashboard feed all key off this id, and a 0 surfaced as
        // `"id":0` on every trace line in a single-request session.
        assert_eq!(state.next_request_id(), 1, "first activity id is 1, not 0");
        assert_eq!(state.next_request_id(), 2);
        assert_eq!(state.next_request_id(), 3);
    }

    #[test]
    fn status_json_shape_covers_name_type_status_windows_and_totals() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let pool = AccountPool::new(&[oauth_account("a"), apikey_account("k")]);
        pool.evaluate(None, &params(), now);
        pool.record_headers(
            &AccountId("a".into()),
            &ParsedRateLimitHeaders {
                five_hour: Some(WindowReading {
                    utilization: 0.42,
                    resets_at: now + Duration::from_secs(3600),
                }),
                seven_day: Some(WindowReading {
                    utilization: 0.10,
                    resets_at: now + Duration::from_secs(86_400),
                }),
                ..Default::default()
            },
            now,
        );
        pool.record_429(&AccountId("k".into()), Some(Duration::from_secs(120)), now);
        let totals = UsageTotals::default();
        totals.record(&AccountId("a".into()), 3, 100, 50);

        let meta = ServerMeta {
            pid: 4321,
            uptime_secs: 7980,
            port: 3456,
        };
        let doc = status_json(&pool.snapshot(), &totals, &params(), now, &meta);

        assert_eq!(doc["current"], "a");
        assert!(doc["version"]
            .as_str()
            .expect("version string")
            .starts_with("llmux "));
        assert_eq!(doc["pid"], 4321);
        assert_eq!(doc["uptime_secs"], 7980);
        assert_eq!(doc["port"], 3456);
        let accounts = doc["accounts"].as_array().expect("accounts array");
        assert_eq!(accounts.len(), 2);

        let a = &accounts[0];
        assert_eq!(a["name"], "a");
        assert_eq!(a["type"], "oauth");
        assert_eq!(a["status"], "active");
        assert_eq!(a["order"], 1);
        assert_eq!(a["blocked"], serde_json::Value::Null);
        assert!((a["five_hour"]["utilization"].as_f64().expect("util") - 0.42).abs() < 1e-9);
        assert_eq!(a["five_hour"]["resets_at"], 1_000_000 + 3600);
        assert_eq!(a["five_hour"]["resets_in_secs"], 3600);
        assert_eq!(a["seven_day"]["resets_in_secs"], 86_400);
        assert_eq!(a["totals"]["requests"], 3);
        assert_eq!(a["totals"]["input_tokens"], 100);
        assert_eq!(a["totals"]["output_tokens"], 50);
        assert_eq!(a["in_flight"], 0);

        let k = &accounts[1];
        assert_eq!(k["type"], "apikey");
        assert_eq!(k["status"], "cooldown");
        assert_eq!(k["order"], 2);
        assert_eq!(k["blocked"], "cooldown 2m00s");
        assert_eq!(k["cooldown_until"], 1_000_000 + 120);
        assert_eq!(
            k["five_hour"],
            serde_json::Value::Null,
            "cold window is null"
        );
        assert_eq!(k["totals"]["requests"], 0);
    }

    #[test]
    fn status_json_carries_token_expiry_and_last_refresh() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000); // = 1_000_000_000 ms
        let mut account = oauth_account("a");
        if let AccountCredential::Oauth {
            expires_at_ms,
            last_refresh_ms,
            ..
        } = &mut account.credential
        {
            *expires_at_ms = 1_003_600_000; // 1h from `now`
            *last_refresh_ms = Some(999_820_000); // 3m before `now`
        }
        let pool = AccountPool::new(&[account, apikey_account("k")]);
        pool.evaluate(None, &params(), now);
        let meta = ServerMeta {
            pid: 1,
            uptime_secs: 0,
            port: 0,
        };
        let doc = status_json(
            &pool.snapshot(),
            &UsageTotals::default(),
            &params(),
            now,
            &meta,
        );
        let accounts = doc["accounts"].as_array().expect("accounts");
        let a = accounts.iter().find(|a| a["name"] == "a").expect("a");
        assert_eq!(a["token_expires_at_ms"], 1_003_600_000u64);
        assert_eq!(a["last_refresh_ms"], 999_820_000u64);
        let k = accounts.iter().find(|a| a["name"] == "k").expect("k");
        assert_eq!(
            k["token_expires_at_ms"],
            serde_json::Value::Null,
            "apikey has no token"
        );
        assert_eq!(k["last_refresh_ms"], serde_json::Value::Null);
    }

    #[test]
    fn status_json_marks_auth_failed_accounts() {
        let now = SystemTime::now();
        let pool = AccountPool::new(&[oauth_account("a")]);
        pool.record_auth_failure(&AccountId("a".into()));
        let meta = ServerMeta {
            pid: 1,
            uptime_secs: 0,
            port: 0,
        };
        let doc = status_json(
            &pool.snapshot(),
            &UsageTotals::default(),
            &params(),
            now,
            &meta,
        );
        assert_eq!(doc["accounts"][0]["status"], "auth_failed");
        assert_eq!(doc["accounts"][0]["blocked"], "auth failed");
        assert_eq!(doc["current"], serde_json::Value::Null);
    }

    /// B1: the `accounts` array is emitted in scheduler preference order —
    /// current first, then eligible accounts by rank (soonest 7d reset),
    /// then ineligible accounts with their blocking reason.
    #[test]
    fn status_json_orders_accounts_by_selection_preference() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let pool = AccountPool::new(&[
            oauth_account("parked"),
            oauth_account("later"),
            oauth_account("soon"),
            oauth_account("cur"),
        ]);
        let window = |resets_in: u64| {
            Some(WindowReading {
                utilization: 0.5,
                resets_at: now + Duration::from_secs(resets_in),
            })
        };
        pool.record_headers(
            &AccountId("later".into()),
            &ParsedRateLimitHeaders {
                seven_day: window(48 * 3600),
                ..Default::default()
            },
            now,
        );
        pool.record_headers(
            &AccountId("soon".into()),
            &ParsedRateLimitHeaders {
                seven_day: window(12 * 3600),
                ..Default::default()
            },
            now,
        );
        pool.record_429(
            &AccountId("parked".into()),
            Some(Duration::from_secs(60)),
            now,
        );
        pool.switch_to(&AccountId("cur".into()), &params(), now)
            .expect("test switch");

        let doc = status_json(
            &pool.snapshot(),
            &UsageTotals::default(),
            &params(),
            now,
            &ServerMeta {
                pid: 1,
                uptime_secs: 0,
                port: 0,
            },
        );
        let names: Vec<&str> = doc["accounts"]
            .as_array()
            .expect("accounts array")
            .iter()
            .map(|a| a["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["cur", "soon", "later", "parked"]);
        let orders: Vec<u64> = doc["accounts"]
            .as_array()
            .expect("accounts array")
            .iter()
            .map(|a| a["order"].as_u64().expect("order"))
            .collect();
        assert_eq!(orders, vec![1, 2, 3, 4]);
        assert_eq!(doc["accounts"][3]["blocked"], "cooldown 1m00s");
    }

    #[test]
    fn usage_totals_accumulate_and_default_to_zero() {
        let totals = UsageTotals::default();
        let a = AccountId("a".into());
        assert_eq!(totals.get(&a), AccountTotals::default());
        totals.record(&a, 1, 10, 5);
        totals.record(&a, 1, 2, 3);
        assert_eq!(
            totals.get(&a),
            AccountTotals {
                requests: 2,
                input_tokens: 12,
                output_tokens: 8,
            }
        );
    }

    // --- account add/remove endpoints (issue #3) ---------------------------

    /// Self-cleaning unique temp dir (no tempfile dev-dependency), mirroring
    /// the pattern in `config::tests` / `forward::tests`.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "llmux-server-test-{}-{}",
                std::process::id(),
                ulid::Ulid::new()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Build an `AppState` whose config is persisted at `config_path` (seeded
    /// with `accounts`), so the add/remove handlers exercise the real
    /// read-merge-write path against a throwaway file — never the user config.
    fn endpoint_state(config_path: &std::path::Path, accounts: Vec<AccountConfig>) -> AppState {
        let config = Config {
            accounts,
            ..Default::default()
        };
        crate::config::save_path(config_path, &config).expect("seed config");
        let pool = AccountPool::new(&config.accounts);
        let mut state = AppState::new(config, pool, None, None).expect("state");
        state.config_path = Some(config_path.to_path_buf());
        state
            .pool
            .evaluate(None, &state.select_params(), SystemTime::now());
        state
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn add_account_persists_apikey_masks_response_and_reloads_pool() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(&path, vec![oauth_account("keep")]);

        let response = add_account_endpoint(
            State(state.clone()),
            axum::extract::Json(AddAccountRequest {
                name: Some("api-mine".into()),
                api_key: "sk-ant-api03-SUPERSECRETVALUE".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["name"], "api-mine");
        assert_eq!(body["type"], "apikey");
        assert_eq!(body["added"], true);
        // Response echoes ONLY a masked key — never the raw secret.
        assert_eq!(body["api_key_masked"], "sk-ant-api03-SU...");
        let masked = body["api_key_masked"].as_str().expect("masked");
        assert!(
            !masked.contains("SUPERSECRET"),
            "raw key must not leak: {masked}"
        );

        // Persisted via read-merge-write: the seeded account is preserved and
        // the new apikey account is on disk with the real (unmasked) key.
        let on_disk = crate::config::load_path(&path).expect("reload");
        let names: Vec<&str> = on_disk.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["keep", "api-mine"]);
        match &on_disk.accounts[1].credential {
            AccountCredential::Apikey { api_key } => {
                assert_eq!(api_key, "sk-ant-api03-SUPERSECRETVALUE")
            }
            other => panic!("expected apikey, got {other:?}"),
        }

        // Live pool reflects the add with no restart.
        let live: Vec<String> = state
            .pool
            .snapshot()
            .accounts
            .iter()
            .map(|a| a.id.0.clone())
            .collect();
        assert!(
            live.contains(&"api-mine".to_string()),
            "live pool reloaded: {live:?}"
        );
    }

    #[tokio::test]
    async fn add_account_assigns_default_name_when_omitted() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(&path, vec![apikey_account("api-1")]);

        let response = add_account_endpoint(
            State(state),
            axum::extract::Json(AddAccountRequest {
                name: None,
                api_key: "sk-ant-api03-ANOTHERONE".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        // Next free `api-N`, computed off the fresh on-disk state.
        assert_eq!(body["name"], "api-2");
    }

    #[tokio::test]
    async fn add_account_rejects_empty_key() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(&path, vec![]);

        let response = add_account_endpoint(
            State(state),
            axum::extract::Json(AddAccountRequest {
                name: None,
                api_key: "   ".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // Nothing written.
        let on_disk = crate::config::load_path(&path).expect("reload");
        assert!(on_disk.accounts.is_empty());
    }

    #[tokio::test]
    async fn remove_account_preserves_others_and_reloads_pool() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(
            &path,
            vec![oauth_account("a"), apikey_account("b"), oauth_account("c")],
        );

        let response = remove_account_endpoint(
            State(state.clone()),
            axum::extract::Json(RemoveAccountRequest {
                name: "b".into(),
                confirm: true,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["removed"], true);

        // Read-merge-write removal preserves the other accounts.
        let on_disk = crate::config::load_path(&path).expect("reload");
        let names: Vec<&str> = on_disk.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);

        // Live pool dropped the removed account with no restart.
        let live: Vec<String> = state
            .pool
            .snapshot()
            .accounts
            .iter()
            .map(|a| a.id.0.clone())
            .collect();
        assert_eq!(live, vec!["a".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    async fn remove_account_requires_confirmation() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(&path, vec![oauth_account("a")]);

        let response = remove_account_endpoint(
            State(state),
            axum::extract::Json(RemoveAccountRequest {
                name: "a".into(),
                confirm: false,
            }),
        )
        .await;
        // No confirm → 400, and the account is left untouched (never silent).
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let on_disk = crate::config::load_path(&path).expect("reload");
        assert_eq!(on_disk.accounts.len(), 1);
        assert_eq!(on_disk.accounts[0].name, "a");
    }

    #[tokio::test]
    async fn remove_account_unknown_name_is_404() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(&path, vec![oauth_account("a")]);

        let response = remove_account_endpoint(
            State(state),
            axum::extract::Json(RemoveAccountRequest {
                name: "ghost".into(),
                confirm: true,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // --- issue #4: inject an OAuth/Codex account from the dashboard ---------

    fn codex_account(name: &str, account_id: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            // A realistic single-token Bearer access token (no `:`/whitespace
            // inside) so the mask span covers the whole secret.
            credential: AccountCredential::Codex {
                account_id: account_id.to_string(),
                access_token: "Bearer eyJhbGciLONGSECRETACCESSTOKENPART".to_string(),
                refresh_token: format!("crt-{name}"),
                expires_at_ms: 0,
                last_refresh_ms: None,
            },
        }
    }

    /// An oauth account whose access token looks like a real Anthropic OAuth
    /// token so `mask_credentials` (which keys off `sk-ant-`) actually masks it.
    fn oauth_account_realistic(name: &str, uuid: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            credential: AccountCredential::Oauth {
                account_uuid: uuid.to_string(),
                access_token: "sk-ant-oat01-SUPERSECRETACCESSTOKENVALUE".to_string(),
                refresh_token: "sk-ant-ort01-SECRETREFRESH".to_string(),
                expires_at_ms: 1_700_000_000_000,
                tier: Some("max".into()),
                last_refresh_ms: Some(1_699_990_000_000),
            },
        }
    }

    #[tokio::test]
    async fn inject_oauth_account_persists_masks_and_reloads_pool() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        // Seed an existing account that MUST survive the inject.
        let state = endpoint_state(&path, vec![apikey_account("keep")]);

        let injected = oauth_account_realistic("claude:me@example.com", "uuid-new");
        let response = inject_account_endpoint(
            State(state.clone()),
            axum::extract::Json(InjectAccountRequest {
                account: injected.clone(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["name"], "claude:me@example.com");
        assert_eq!(body["type"], "oauth");
        assert_eq!(body["added"], true);
        // The access token is echoed ONLY masked — never the raw secret.
        let masked = body["access_token_masked"].as_str().expect("masked");
        assert_eq!(masked, "sk-ant-oat01-SU...");
        assert!(
            !masked.contains("SUPERSECRETACCESSTOKENVALUE"),
            "raw token leaked: {masked}"
        );

        // Read-merge-write: the seeded account is preserved and the oauth
        // credential is on disk with its real (unmasked) tokens.
        let on_disk = crate::config::load_path(&path).expect("reload");
        let names: Vec<&str> = on_disk.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["keep", "claude:me@example.com"]);
        match &on_disk.accounts[1].credential {
            AccountCredential::Oauth {
                account_uuid,
                access_token,
                tier,
                ..
            } => {
                assert_eq!(account_uuid, "uuid-new");
                assert_eq!(access_token, "sk-ant-oat01-SUPERSECRETACCESSTOKENVALUE");
                assert_eq!(tier.as_deref(), Some("max"));
            }
            other => panic!("expected oauth, got {other:?}"),
        }

        // Live pool reflects the inject with no restart.
        let live: Vec<String> = state
            .pool
            .snapshot()
            .accounts
            .iter()
            .map(|a| a.id.0.clone())
            .collect();
        assert!(
            live.contains(&"claude:me@example.com".to_string()),
            "live pool reloaded: {live:?}"
        );
    }

    #[tokio::test]
    async fn inject_codex_account_persists_and_masks() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(&path, vec![oauth_account("a")]);

        let response = inject_account_endpoint(
            State(state.clone()),
            axum::extract::Json(InjectAccountRequest {
                account: codex_account("codex:me@example.com", "chatgpt-acct-1"),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["type"], "codex");
        assert_eq!(body["name"], "codex:me@example.com");
        // `Bearer …` is masked to the first 20 chars + `...`.
        let masked = body["access_token_masked"].as_str().expect("masked");
        assert_eq!(masked, "Bearer eyJhbGciLONGS...");
        assert!(
            !masked.contains("SECRETACCESSTOKENPART"),
            "raw token leaked: {masked}"
        );

        let on_disk = crate::config::load_path(&path).expect("reload");
        // Other account preserved; codex added.
        let names: Vec<&str> = on_disk.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "codex:me@example.com"]);
        assert!(matches!(
            on_disk.accounts[1].credential,
            AccountCredential::Codex { .. }
        ));
    }

    #[tokio::test]
    async fn inject_oauth_relogin_updates_by_uuid_not_duplicates() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        // Existing oauth account with the SAME uuid the re-login will carry.
        let state = endpoint_state(&path, vec![oauth_account_realistic("claude:old", "uuid-x")]);

        // Re-login: same uuid, new name (profile email changed) — must UPDATE
        // the existing entry, not add a second one (dedup by account_uuid).
        let mut relogin = oauth_account_realistic("claude:new", "uuid-x");
        if let AccountCredential::Oauth { access_token, .. } = &mut relogin.credential {
            *access_token = "sk-ant-oat01-ROTATEDTOKENVALUE".to_string();
        }
        let response = inject_account_endpoint(
            State(state.clone()),
            axum::extract::Json(InjectAccountRequest { account: relogin }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["added"], false, "re-login updates, never adds");

        let on_disk = crate::config::load_path(&path).expect("reload");
        assert_eq!(on_disk.accounts.len(), 1, "no duplicate from re-login");
        assert_eq!(on_disk.accounts[0].name, "claude:new");
    }

    #[tokio::test]
    async fn inject_rejects_apikey_credential() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(&path, vec![]);

        // An apikey credential is the wrong endpoint (/add-account handles it,
        // no browser needed) — inject must refuse with a 400 and write nothing.
        let response = inject_account_endpoint(
            State(state),
            axum::extract::Json(InjectAccountRequest {
                account: apikey_account("api-1"),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let on_disk = crate::config::load_path(&path).expect("reload");
        assert!(on_disk.accounts.is_empty(), "nothing persisted");
    }

    #[tokio::test]
    async fn inject_rejects_empty_name() {
        let dir = TempDir::new();
        let path = dir.path().join("llmux.json");
        let state = endpoint_state(&path, vec![]);

        let mut acct = oauth_account_realistic("", "uuid-z");
        acct.name = "   ".into();
        let response = inject_account_endpoint(
            State(state),
            axum::extract::Json(InjectAccountRequest { account: acct }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
