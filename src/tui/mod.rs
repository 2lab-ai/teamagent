//! ratatui dashboard (FR6): per-account quota gauges (5h/7d) with reset
//! countdowns, active/cooldown status, activity log, log console, totals.
//! Keys: `q`uit, `R`eload config, `s`witch (select mode), `a`dd / `r`emove
//! (pointers to the CLI), `l` log-panel size, `d` detail toggle.
//!
//! Two entry points, ONE renderer:
//! - [`run_local`] — in-process mode (`llmux server` on a TTY): renders
//!   live `AppState` (pool + dashboard hub) directly.
//! - [`run_remote`] — attach mode (`llmux dashboard`, or `llmux
//!   server` when a daemon already owns the port): polls
//!   `GET /llmux/dashboard` every second and renders the fetched
//!   document. Mostly read-only: manual switch goes through
//!   `POST /llmux/switch`; config mutation keys (`a`/`r`/`R`) are
//!   local-mode-only.
//!
//! Both paths build the same [`view::DashboardView`] (local: from an
//! in-process [`crate::dashboard::DashboardDoc`]; remote: from the fetched
//! JSON) — the draw code in [`ui`] is never forked.

pub(crate) mod activity;
mod anim;
mod event;
// pub(crate): `cli::status` reuses the token/age formatters so the plain
// `llmux status` output and the dashboard agree on the display.
pub(crate) mod format;
pub(crate) mod logs;
mod ui;
mod view;

pub use event::{ActivityEvent, TokenCounts};

/// Recommended bound for the proxy→dashboard activity channel (`try_send` +
/// drop-on-full on the sender side, so a stalled dashboard never
/// backpressures the request path).
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;

use crate::dashboard::{CodexSettingsDoc, DashboardDoc};
use crate::scheduler::select;
use logs::LogPanelSize;
use view::DashboardView;

/// Codex models the dashboard cycles through with `m` (req8.1). Any model can
/// still be set via config / the control endpoint; this is the quick-pick set.
const CODEX_MODELS: &[&str] = &["gpt-5.5", "gpt-5.5-codex", "gpt-5-codex"];
/// Reasoning-effort levels cycled with `e`; "" = unset (backend default).
const CODEX_EFFORTS: &[&str] = &["", "minimal", "low", "medium", "high", "xhigh"];

/// One-line summary of codex settings for the status bar.
fn codex_status_line(c: &CodexSettingsDoc) -> String {
    format!(
        "codex {} · fast {} · effort {}",
        c.model,
        if c.fast { "on" } else { "off" },
        c.effort.as_deref().unwrap_or("default"),
    )
}

/// Render cadence — also the cadence at which a fetched remote document is
/// re-rendered between polls (countdowns keep ticking). 120ms (~8fps) so the
/// status/spinner animations (see `anim`) step smoothly rather than the choppy
/// 4fps the original 250ms FR6 tick gave; still trivial CPU for a glance TUI.
const RENDER_TICK: Duration = Duration::from_millis(120);
/// Input drain cadence — faster than the render tick so keys feel immediate.
const INPUT_TICK: Duration = Duration::from_millis(33);
/// Remote poll cadence for `GET /llmux/dashboard`.
const FETCH_TICK: Duration = Duration::from_secs(1);
/// How long a transient status-line message stays on screen.
const STATUS_TTL: Duration = Duration::from_secs(5);

/// Last committed account switch, persisted for the scheduler pane (the
/// activity ring forgets; the WHY line must not).
#[derive(Debug, Clone)]
pub(crate) struct LastSwitch {
    pub from: Option<String>,
    pub to: String,
    pub reason: Option<String>,
    pub at: SystemTime,
}

/// Poller health for one oauth account, folded from `UsagePolled` events.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PollHealth {
    /// When the last successful poll finished.
    pub last_ok: Option<SystemTime>,
    /// Consecutive failures (0 = healthy).
    pub consecutive_failures: u32,
    /// When the next poll attempt is scheduled.
    pub next_at: SystemTime,
}

/// Input mode: normal keybar vs. account-selection (the `s` key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    /// Cursor row for the pending switch.
    Select {
        idx: usize,
    },
}

/// UI-local state the renderer needs besides the data view: cursor, panes,
/// spinner frame, status line, attach banner.
pub(crate) struct Chrome {
    pub frame: usize,
    pub mode: Mode,
    pub show_detail: bool,
    pub log_panel: LogPanelSize,
    pub status_line: Option<String>,
    /// Activity-log scroll offset: number of newest completed entries skipped
    /// (0 = live tail). Lets the panel page through the full history (req6).
    pub activity_scroll: usize,
    /// Detailed model-usage view active (`g`) — replaces the middle/activity
    /// region with the full model table + drill-down (req13).
    pub show_models: bool,
    /// Cursor row in the detailed model view.
    pub model_cursor: usize,
    /// `Some` in attach mode.
    pub attach: Option<Attach>,
}

/// Attach-mode banner state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Attach {
    /// Daemon pid, from the probe or the last fetched document.
    pub pid: Option<u32>,
    /// False while the poller cannot reach the daemon (reconnect banner).
    pub connected: bool,
}

/// Options for [`run_remote`].
#[derive(Debug, Clone)]
pub struct RemoteOptions {
    /// `http://localhost:<port>` — same base the CLI probes.
    pub base_url: String,
    /// Proxy api key, sent as `x-api-key` (loopback is exempt; harmless).
    pub api_key: Option<String>,
    /// Daemon pid from the probe, for the header marker before the first
    /// document arrives.
    pub pid: Option<u32>,
}

/// Where the dashboard data comes from.
enum Backend {
    /// In-process: live `AppState` (pool + hub) — the document is built
    /// locally each frame. Boxed to keep the two variants size-balanced
    /// (`AppState` is an Arc-heavy 300+ byte struct).
    Local(Box<crate::proxy::server::AppState>),
    /// Attached to a daemon over HTTP. Boxed — it carries a reqwest client +
    /// the last fetched document.
    Remote(Box<Remote>),
}

struct Remote {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Pid from the probe (the fetched document refreshes it).
    pid: Option<u32>,
    /// Last successfully fetched document (kept through reconnects).
    doc: Option<DashboardDoc>,
    connected: bool,
    /// Switch target chosen in select mode, performed by the event loop
    /// (key handling is sync; the POST is not).
    pending_switch: Option<String>,
    /// Codex settings change (fast/model/effort) queued by a key, performed by
    /// the event loop via `POST /llmux/codex` (req8.1).
    pending_codex: Option<crate::dashboard::CodexSettingsDoc>,
}

/// One message from the remote fetch task.
enum FetchMsg {
    Doc(Box<DashboardDoc>),
    Lost,
}

/// Dashboard state, re-rendered each tick from a fresh view-model.
struct App {
    backend: Backend,
    mode: Mode,
    /// Monotonic frame counter driving the spinner.
    frame: usize,
    should_quit: bool,
    status: Option<(String, Instant)>,
    /// Selected-account detail pane visibility (`d` toggles).
    show_detail: bool,
    log_panel: LogPanelSize,
    /// Activity-log scroll offset (newest entries skipped; 0 = live tail).
    activity_scroll: usize,
    /// Detailed model-usage view active (`g`), with its cursor row.
    show_models: bool,
    model_cursor: usize,
}

impl App {
    fn new(backend: Backend) -> Self {
        Self {
            backend,
            mode: Mode::Normal,
            frame: 0,
            should_quit: false,
            status: None,
            show_detail: true,
            log_panel: LogPanelSize::Small,
            activity_scroll: 0,
            show_models: false,
            model_cursor: 0,
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self.backend, Backend::Remote(_))
    }

    /// Build the view-model for one frame. `None` only in remote mode before
    /// the first document arrives.
    fn view(&self, now: SystemTime) -> Option<DashboardView> {
        match &self.backend {
            Backend::Local(state) => Some(DashboardView::from_doc(&crate::dashboard::build_doc(
                state, now,
            ))),
            Backend::Remote(remote) => remote.doc.as_ref().map(DashboardView::from_doc),
        }
    }

    fn chrome(&self) -> Chrome {
        Chrome {
            frame: self.frame,
            mode: self.mode,
            show_detail: self.show_detail,
            log_panel: self.log_panel,
            activity_scroll: self.activity_scroll,
            show_models: self.show_models,
            model_cursor: self.model_cursor,
            status_line: self.status_line().map(str::to_string),
            attach: match &self.backend {
                Backend::Local(_) => None,
                Backend::Remote(remote) => Some(Attach {
                    pid: remote.doc.as_ref().map(|d| d.pid).or(remote.pid),
                    connected: remote.connected,
                }),
            },
        }
    }

    /// Active status-line message, if it hasn't expired.
    fn status_line(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, since)| since.elapsed() < STATUS_TTL)
            .map(|(text, _)| text.as_str())
    }

    fn set_status(&mut self, text: String) {
        self.status = Some((text, Instant::now()));
    }

    fn apply_fetch(&mut self, msg: FetchMsg) {
        if let Backend::Remote(remote) = &mut self.backend {
            match msg {
                FetchMsg::Doc(doc) => {
                    remote.doc = Some(*doc);
                    remote.connected = true;
                }
                FetchMsg::Lost => remote.connected = false,
            }
        }
    }

    fn take_pending_switch(&mut self) -> Option<String> {
        match &mut self.backend {
            Backend::Remote(remote) => remote.pending_switch.take(),
            Backend::Local(_) => None,
        }
    }

    fn on_key(&mut self, key: KeyEvent, view: Option<&DashboardView>) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Ctrl-C quits from any mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.mode {
            Mode::Normal if self.show_models => self.on_key_models(key.code, view),
            Mode::Normal => self.on_key_normal(key.code, view),
            Mode::Select { idx } => self.on_key_select(key.code, idx, view),
        }
    }

    /// Key handling for the detailed model-usage view (`g`). Arrows/`j`/`k`
    /// move the cursor through model rows; `g`/`Esc` exits; `q` quits.
    fn on_key_models(&mut self, code: KeyCode, view: Option<&DashboardView>) {
        let len = view.map_or(0, |v| v.model_usage.len());
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('g') | KeyCode::Esc => self.show_models = false,
            KeyCode::Up | KeyCode::Char('k') => self.move_model_cursor(-1, len),
            KeyCode::Down | KeyCode::Char('j') => self.move_model_cursor(1, len),
            KeyCode::PageUp => self.move_model_cursor(-10, len),
            KeyCode::PageDown => self.move_model_cursor(10, len),
            KeyCode::Home => self.model_cursor = 0,
            KeyCode::End => self.model_cursor = len.saturating_sub(1),
            KeyCode::Char('l') => self.log_panel = self.log_panel.cycle(),
            _ => {}
        }
    }

    /// Move the model cursor by `delta` rows, clamped to `[0, len-1]`.
    fn move_model_cursor(&mut self, delta: i64, len: usize) {
        if len == 0 {
            self.model_cursor = 0;
            return;
        }
        let next = (self.model_cursor as i64).saturating_add(delta);
        self.model_cursor = next.clamp(0, (len - 1) as i64) as usize;
    }

    fn on_key_normal(&mut self, code: KeyCode, view: Option<&DashboardView>) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('R') => self.reload(),
            KeyCode::Char('s') => {
                let accounts = view.map_or(0, |v| v.snapshot.accounts.len());
                if accounts == 0 {
                    self.set_status("no accounts to switch between".into());
                    return;
                }
                // Rows render in selection order, where the current account
                // (when one exists) is always row 0 — start the cursor there.
                self.mode = Mode::Select { idx: 0 };
            }
            // v0.1: add/remove are CLI flows (OAuth browser dance / confirm
            // prompt); the TUI points at them. In attach mode they are
            // local-mode-only (config mutation stays on the server host).
            KeyCode::Char('a') => {
                self.set_status(if self.is_remote() {
                    "add: local mode only — run `llmux login` on the server host".into()
                } else {
                    "add: run `llmux login` (or `llmux login --api`)".into()
                });
            }
            KeyCode::Char('r') => {
                self.set_status(if self.is_remote() {
                    "remove: local mode only — run `llmux remove <name>` on the server host".into()
                } else {
                    "remove: run `llmux remove <name>`".into()
                });
            }
            KeyCode::Char('l') => self.log_panel = self.log_panel.cycle(),
            KeyCode::Char('d') => self.show_detail = !self.show_detail,
            // Detailed model-usage view (req13). No-op (with a hint) until at
            // least one model row exists.
            KeyCode::Char('g') => {
                if view.is_some_and(|v| !v.model_usage.is_empty()) {
                    self.show_models = true;
                    self.model_cursor = 0;
                } else {
                    self.set_status("models: no model usage yet".into());
                }
            }
            // Activity-log scrolling (req6): up = into history, down = toward
            // the live tail. Clamped to the number of completed entries.
            KeyCode::Up | KeyCode::Char('k') => self.scroll_activity(1, view),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_activity(-1, view),
            KeyCode::PageUp => self.scroll_activity(10, view),
            KeyCode::PageDown => self.scroll_activity(-10, view),
            KeyCode::Home => self.scroll_activity(i64::MAX, view),
            KeyCode::End => self.activity_scroll = 0,
            // Codex group settings (req8.1): f = fast on/off, m = cycle model,
            // e = cycle reasoning effort. No-op (with a hint) when there is no
            // codex account.
            KeyCode::Char('f') => self.toggle_codex_fast(view),
            KeyCode::Char('m') => self.cycle_codex_model(view),
            KeyCode::Char('e') => self.cycle_codex_effort(view),
            _ => {}
        }
    }

    /// The live codex settings, or `None` when no codex account exists.
    fn current_codex(&self, view: Option<&DashboardView>) -> Option<CodexSettingsDoc> {
        view.and_then(|v| v.codex.available.then(|| v.codex.clone()))
    }

    fn toggle_codex_fast(&mut self, view: Option<&DashboardView>) {
        match self.current_codex(view) {
            Some(mut c) => {
                c.fast = !c.fast;
                self.set_codex(c);
            }
            None => self.set_status("codex: no codex account (run `llmux login --codex`)".into()),
        }
    }

    fn cycle_codex_model(&mut self, view: Option<&DashboardView>) {
        if let Some(mut c) = self.current_codex(view) {
            let next = CODEX_MODELS
                .iter()
                .position(|m| *m == c.model)
                .map(|i| (i + 1) % CODEX_MODELS.len())
                .unwrap_or(0);
            c.model = CODEX_MODELS[next].to_string();
            self.set_codex(c);
        } else {
            self.set_status("codex: no codex account".into());
        }
    }

    fn cycle_codex_effort(&mut self, view: Option<&DashboardView>) {
        if let Some(mut c) = self.current_codex(view) {
            let cur = c.effort.as_deref().unwrap_or("");
            let next = CODEX_EFFORTS
                .iter()
                .position(|e| *e == cur)
                .map(|i| (i + 1) % CODEX_EFFORTS.len())
                .unwrap_or(0);
            let e = CODEX_EFFORTS[next];
            c.effort = (!e.is_empty()).then(|| e.to_string());
            self.set_codex(c);
        } else {
            self.set_status("codex: no codex account".into());
        }
    }

    /// Apply a codex settings change: locally in-process, or queued for the
    /// event loop to POST in attach mode.
    fn set_codex(&mut self, new: CodexSettingsDoc) {
        match &mut self.backend {
            Backend::Local(state) => {
                state.codex.set_shape(crate::provider::codex::CodexShape {
                    model: new.model.clone(),
                    fast: new.fast,
                    effort: new.effort.clone(),
                });
                if let Some(path) = &state.config_path {
                    let _ = crate::config::update_path(path, |c| {
                        c.codex.default_model = new.model.clone();
                        c.codex.fast = new.fast;
                        c.codex.reasoning_effort = new.effort.clone();
                    });
                }
                self.set_status(codex_status_line(&new));
            }
            Backend::Remote(remote) => {
                remote.pending_codex = Some(new.clone());
                self.set_status(format!("applying {}…", codex_status_line(&new)));
            }
        }
    }

    fn take_pending_codex(&mut self) -> Option<CodexSettingsDoc> {
        match &mut self.backend {
            Backend::Remote(remote) => remote.pending_codex.take(),
            Backend::Local(_) => None,
        }
    }

    /// Perform the queued remote codex change (`POST /llmux/codex`).
    async fn perform_remote_codex(&mut self, new: CodexSettingsDoc) {
        let Backend::Remote(remote) = &mut self.backend else {
            return;
        };
        let url = format!("{}/llmux/codex", remote.base_url);
        let mut request = remote.client.post(&url).json(&serde_json::json!({
            "fast": new.fast,
            "default_model": new.model,
            "reasoning_effort": new.effort.clone().unwrap_or_default(),
        }));
        if let Some(key) = &remote.api_key {
            request = request.header("x-api-key", key);
        }
        let message = match request.send().await {
            Ok(response) if response.status().is_success() => codex_status_line(&new),
            Ok(response) => format!("codex change failed: {}", response.status()),
            Err(err) => format!("codex change failed: {err}"),
        };
        self.set_status(message);
    }

    /// Move the activity scroll offset by `delta` rows (positive = older),
    /// clamped to `[0, completed_len - 1]`. `view` supplies the live length.
    fn scroll_activity(&mut self, delta: i64, view: Option<&DashboardView>) {
        let len = view.map_or(0, |v| v.completed.len());
        let max = len.saturating_sub(1) as i64;
        let next = (self.activity_scroll as i64).saturating_add(delta);
        self.activity_scroll = next.clamp(0, max) as usize;
    }

    fn on_key_select(&mut self, code: KeyCode, idx: usize, view: Option<&DashboardView>) {
        let len = view.map_or(0, |v| v.snapshot.accounts.len());
        if len == 0 {
            self.mode = Mode::Normal;
            return;
        }
        let idx = idx.min(len - 1); // roster may have shrunk under us (R reload)
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::Select {
                    idx: idx.saturating_sub(1),
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::Select {
                    idx: (idx + 1).min(len - 1),
                };
            }
            KeyCode::Enter => {
                self.try_manual_switch(idx, view);
                self.mode = Mode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => self.mode = Mode::Normal,
            _ => self.mode = Mode::Select { idx },
        }
    }

    /// `R` — re-read the config file and swap the roster into the live pool
    /// (`AccountPool::reload_accounts` keeps window/cooldown state for
    /// surviving accounts). Local mode only: an attached client must not
    /// reload the DAEMON's roster from the CLIENT's config file.
    fn reload(&mut self) {
        let now = SystemTime::now();
        match &self.backend {
            Backend::Local(state) => match crate::config::load() {
                Ok(config) => {
                    let n = config.accounts.len();
                    state.pool.reload_accounts(&config.accounts);
                    let msg = format!("config reloaded: {n} account(s)");
                    state.hub.push_note(msg.clone(), false, now);
                    self.set_status(msg);
                }
                Err(err) => self.set_status(format!("reload failed: {err}")),
            },
            Backend::Remote(_) => {
                self.set_status(
                    "reload: local mode only — restart applies on the server host".into(),
                );
            }
        }
    }

    /// Enter in select mode — switch the scheduler to the chosen account.
    ///
    /// The eligibility precheck runs here on the view's snapshot (same pure
    /// gate the scheduler uses), so the operator gets the real refusal
    /// reason immediately; the commit re-validates anyway (local:
    /// `AccountPool::switch_to` under the pool lock; remote: the server's
    /// switch endpoint runs the identical call).
    fn try_manual_switch(&mut self, idx: usize, view: Option<&DashboardView>) {
        let Some(view) = view else { return };
        let now = SystemTime::now();
        // The cursor indexes DISPLAY rows (selection order), not config order.
        let order = view.display_order(now);
        let Some(target) = order.get(idx).and_then(|&i| view.snapshot.accounts.get(i)) else {
            return;
        };
        if view.snapshot.is_current(&target.id) {
            self.set_status(format!("{} is already active", target.id));
            return;
        }
        let headers_only =
            select::headers_only_mode(&view.snapshot, &view.select_params, None, now);
        if let Some(reason) = select::eligibility(target, &view.select_params, now, headers_only) {
            self.set_status(format!("cannot switch to {}: {reason:?}", target.id));
            return;
        }
        let target_id = target.id.clone();
        let from = view.snapshot.representative_current().cloned();
        match &mut self.backend {
            Backend::Local(state) => {
                match state.pool.switch_to(&target_id, &view.select_params, now) {
                    Ok(()) => {
                        state.emit(ActivityEvent::AccountSwitched {
                            from: from.map(|id| id.0),
                            to: target_id.0.clone(),
                            reason: Some("manual".into()),
                        });
                        self.set_status(format!("switched to {target_id} (manual)"));
                    }
                    Err(err) => self.set_status(format!("switch to {target_id} failed: {err}")),
                }
            }
            Backend::Remote(remote) => {
                remote.pending_switch = Some(target_id.0.clone());
                self.set_status(format!("switching to {target_id}…"));
            }
        }
    }

    /// Perform the queued remote switch (`POST /llmux/switch`).
    async fn perform_remote_switch(&mut self, target: String) {
        let Backend::Remote(remote) = &mut self.backend else {
            return;
        };
        let url = format!("{}/llmux/switch", remote.base_url);
        let mut request = remote
            .client
            .post(&url)
            .json(&serde_json::json!({ "account": target }));
        if let Some(key) = &remote.api_key {
            request = request.header("x-api-key", key);
        }
        let message = match request.send().await {
            Ok(response) if response.status().is_success() => {
                format!("switched to {target} (manual)")
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let detail = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                    .unwrap_or_else(|| status.to_string());
                format!("switch to {target} failed: {detail}")
            }
            Err(err) => format!("switch to {target} failed: {err}"),
        };
        self.set_status(message);
    }
}

/// Run the in-process dashboard over live server state until quit.
///
/// Terminal lifecycle: `ratatui::try_init` enables raw mode + the alternate
/// screen AND installs a panic hook that restores the terminal before
/// unwinding; `ratatui::restore` runs on every exit path.
pub async fn run_local(state: crate::proxy::server::AppState) -> std::io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let mut app = App::new(Backend::Local(Box::new(state)));
    let result = event_loop(&mut terminal, &mut app, None).await;
    ratatui::restore();
    result
}

/// Attach to a running daemon: poll `GET /llmux/dashboard` every second
/// and render the fetched document with the same draw code as local mode. A
/// lost connection shows a reconnect banner and keeps retrying — never
/// crashes the client.
pub async fn run_remote(opts: RemoteOptions) -> std::io::Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(std::io::Error::other)?;
    let (tx, rx) = mpsc::channel(4);
    let fetcher = tokio::spawn(fetch_loop(
        client.clone(),
        opts.base_url.clone(),
        opts.api_key.clone(),
        tx,
    ));
    let mut terminal = ratatui::try_init()?;
    let mut app = App::new(Backend::Remote(Box::new(Remote {
        client,
        base_url: opts.base_url,
        api_key: opts.api_key,
        pid: opts.pid,
        doc: None,
        connected: false,
        pending_switch: None,
        pending_codex: None,
    })));
    let result = event_loop(&mut terminal, &mut app, Some(rx)).await;
    ratatui::restore();
    fetcher.abort();
    result
}

/// Poll the dashboard endpoint forever, reporting documents and losses to
/// the event loop. Exits only when the TUI side hangs up.
async fn fetch_loop(
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    tx: mpsc::Sender<FetchMsg>,
) {
    let url = format!("{base_url}/llmux/dashboard");
    let mut interval = tokio::time::interval(FETCH_TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let mut request = client.get(&url);
        if let Some(key) = &api_key {
            request = request.header("x-api-key", key);
        }
        let msg = match request.send().await {
            Ok(response) if response.status().is_success() => {
                match response.json::<DashboardDoc>().await {
                    Ok(doc) => FetchMsg::Doc(Box::new(doc)),
                    Err(_) => FetchMsg::Lost,
                }
            }
            _ => FetchMsg::Lost,
        };
        if tx.send(msg).await.is_err() {
            return; // dashboard quit
        }
    }
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    mut fetch: Option<mpsc::Receiver<FetchMsg>>,
) -> std::io::Result<()> {
    let mut render = tokio::time::interval(RENDER_TICK);
    render.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut input = tokio::time::interval(INPUT_TICK);
    input.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let mut redraw = tokio::select! {
            _ = render.tick() => {
                app.frame = app.frame.wrapping_add(1);
                true
            }
            _ = input.tick() => drain_input(app)?,
            msg = async {
                match fetch.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if fetch.is_some() => {
                match msg {
                    Some(msg) => app.apply_fetch(msg),
                    // Fetch task gone (cannot happen before abort) — show
                    // the reconnect banner instead of spinning.
                    None => {
                        app.apply_fetch(FetchMsg::Lost);
                        fetch = None;
                    }
                }
                true
            }
        };
        if let Some(target) = app.take_pending_switch() {
            app.perform_remote_switch(target).await;
            redraw = true;
        }
        if let Some(codex) = app.take_pending_codex() {
            app.perform_remote_codex(codex).await;
            redraw = true;
        }
        if app.should_quit {
            return Ok(());
        }
        if redraw {
            let view = app.view(SystemTime::now());
            let chrome = app.chrome();
            terminal.draw(|frame| ui::draw(frame, view.as_ref(), &chrome))?;
        }
    }
}

/// Drain every pending terminal event without blocking the runtime
/// (`poll(ZERO)` is a non-blocking readiness check). Returns whether
/// anything happened that warrants a redraw.
fn drain_input(app: &mut App) -> std::io::Result<bool> {
    let mut dirty = false;
    // Built once per drain: key handlers read the same frame the user saw.
    let mut view: Option<Option<DashboardView>> = None;
    while crossterm::event::poll(Duration::ZERO)? {
        match crossterm::event::read()? {
            Event::Key(key) => {
                let view = view.get_or_insert_with(|| app.view(SystemTime::now()));
                app.on_key(key, view.as_ref());
                dirty = true;
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
    Ok(dirty)
}
