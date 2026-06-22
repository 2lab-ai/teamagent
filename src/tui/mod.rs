//! ratatui dashboard (FR6): per-account quota gauges (5h/7d) with reset
//! countdowns, active/cooldown status, activity log, totals.
//!
//! IA (issue #5): a wall-clock MAIN view that is ALWAYS rendered (header ·
//! account quota table · scheduler/totals summary · compact model strip ·
//! in-flight/activity) plus three summoned [`Overlay`]s drawn OVER it —
//! `a`ccounts (account detail + add/remove/login affordances), `g` stats (the
//! detailed per-model table + drill-down), `l`ogs (full-screen tail). `Esc`
//! closes any overlay back to MAIN. MAIN-level keys: `q`uit, `R`eload config,
//! `f`/`m`/`e` codex, `↑↓` scroll. The account interactions (`s`witch, `a`dd,
//! `r`emove, `n`ew browser login) run within the Accounts overlay as [`Mode`]s.
//!
//! Two entry points, ONE renderer:
//! - [`run_local`] — in-process mode (`llmux server` on a TTY): renders
//!   live `AppState` (pool + dashboard hub) directly.
//! - [`run_remote`] — attach mode (`llmux dashboard`, or `llmux
//!   server` when a daemon already owns the port): polls
//!   `GET /llmux/dashboard` every second and renders the fetched
//!   document. Manual switch goes through `POST /llmux/switch`; account
//!   add/remove (issue #3) go through `POST /llmux/add-account` /
//!   `POST /llmux/remove-account`, so they work in attach mode too. Only `R`
//!   (reload from the local config file) stays local-mode-only.
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

/// Bound for the proxy→dashboard activity channel (`try_send` +
/// drop-on-full on the sender side, so a stalled dashboard never
/// backpressures the request path).
///
/// Activity events are tiny (a few enum fields). The previous bound of 256 was
/// small enough that a burst of concurrent codex requests could fill it between
/// dashboard folds, and a *dropped* `RequestFinished` leaks its in-flight row
/// forever (BUG: zombie 25,000s+ rows while the daemon reports `in_flight=0`).
/// 4096 removes drops under realistic codex load; the stale-sweep in
/// [`activity::ActivityLog::prune_stale_in_flight`] is the backstop that
/// guarantees a dropped finish can never leak.
pub const ACTIVITY_CHANNEL_CAP: usize = 4096;

use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::config::AccountConfig;
use crate::dashboard::{CodexSettingsDoc, DashboardDoc};
use crate::scheduler::select;
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

/// Can this client open a browser for an OAuth flow? The login dance (browser
/// plus localhost callback) runs in the CLIENT, so this gates the `n` new-login
/// key; when it returns false the picker is replaced by the `llmux login`
/// fallback rather than starting a flow that would hang on the callback.
///
/// macOS/Windows: `open`/`start` hand the URL to the windowing system, which
/// launches the default browser on the HOST's GUI session. This works even when
/// invoked from an SSH/tmux session (the browser opens on the host's console,
/// where the daemon — and the localhost callback — live). Critically, `SSH_*`
/// env vars routinely LEAK into long-lived tmux sessions, so gating macOS on
/// `SSH_CONNECTION` produced false "headless" negatives for a user sitting at
/// their Mac inside tmux (the bug this fixes). Only Linux's `xdg-open` genuinely
/// needs a reachable display server, so that is the only platform we gate.
fn can_open_browser() -> bool {
    let gui_platform = cfg!(any(target_os = "macos", target_os = "windows"));
    let has_display =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    can_open_browser_decide(gui_platform, has_display)
}

/// Pure decision for [`can_open_browser`], split out so it is testable without
/// mutating process env (which would race other tests). GUI platforms
/// (macOS/Windows) always can; Linux needs a display server. SSH is
/// deliberately NOT an input — it gave false negatives via tmux env leakage.
fn can_open_browser_decide(gui_platform: bool, has_display: bool) -> bool {
    gui_platform || has_display
}

/// Fallback message for a headless client that cannot open a browser. Tells
/// the user to run `llmux login` where the browser is; when attached, that is
/// the daemon host, so name it.
fn headless_login_hint(remote: bool) -> String {
    if remote {
        "new login needs a browser — run `llmux login` on the daemon host, or attach from a \
         machine with a browser"
            .to_string()
    } else {
        "new login needs a browser — run `llmux login` on this host from a desktop session"
            .to_string()
    }
}

/// Render cadence — also the cadence at which a fetched remote document is
/// re-rendered between polls (countdowns keep ticking). 120ms (~8fps) so the
/// status/spinner animations (see `anim`) step smoothly rather than the choppy
/// 4fps the original 250ms FR6 tick gave; still trivial CPU for a glance TUI.
const RENDER_TICK: Duration = Duration::from_millis(120);
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

/// Which browser login flow the "new login" picker (`n`) kicks off. The flow
/// runs in the CLIENT (this process) — `login_interactive` for Anthropic,
/// `login_codex_interactive` for ChatGPT/Codex — then the minted credential is
/// injected into the daemon (in-process locally, `POST /llmux/inject-account`
/// when attached). One code path for local and attach (issue #4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginKind {
    /// Anthropic Claude PKCE OAuth (`login_interactive`).
    Anthropic,
    /// ChatGPT / Codex OAuth (`login_codex_interactive`).
    Codex,
}

impl LoginKind {
    /// Quick-pick rows for `Mode::NewLogin`, in display order.
    pub(crate) const ALL: [LoginKind; 2] = [LoginKind::Anthropic, LoginKind::Codex];

    /// One-line label for the picker + status line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            LoginKind::Anthropic => "Claude (Anthropic OAuth)",
            LoginKind::Codex => "Codex (ChatGPT OAuth)",
        }
    }
}

/// Input mode: normal keybar vs. account-selection (the `s` key) vs. the
/// add-account key entry (`a`) vs. the remove confirmation (`r`) vs. the
/// new-login provider picker (`n`).
///
/// Deliberately `Copy` (no owned buffer inside): the add-account input text
/// lives in [`App::add_input`] so the masked render never has to clone a
/// secret through this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    /// Cursor row for the pending switch.
    Select {
        idx: usize,
    },
    /// Entering an API key for a new account (the `a` key). The typed text is
    /// held in [`App::add_input`] and rendered masked.
    AddKey,
    /// Confirming a destructive account removal (the `r` key). `idx` is the
    /// display row being removed; the name is resolved at confirm time.
    ConfirmRemove {
        idx: usize,
    },
    /// Picking the provider for a new browser login (the `n` key). `idx` is the
    /// cursor row into [`LoginKind::ALL`]. Enter starts the OAuth flow in this
    /// client; the minted credential is injected into the daemon.
    NewLogin {
        idx: usize,
    },
}

/// A summoned surface drawn OVER the always-rendered MAIN view (issue #5). MAIN
/// keeps updating every frame underneath; an overlay only covers its own rect
/// (cleared with [`ratatui::widgets::Clear`] in `ui.rs`). Direct shortcuts —
/// `a`/`g`/`l` open, `Esc` returns to MAIN — with no ordered carousel.
///
/// `Copy` so it threads through `Chrome` without allocation. The in-overlay
/// interactions (Select/AddKey/ConfirmRemove/NewLogin) still live in [`Mode`]
/// and operate WITHIN the Accounts overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Overlay {
    /// MAIN only — no overlay.
    #[default]
    None,
    /// Account detail + the add/remove/login affordances (issues #3/#4).
    Accounts,
    /// The detailed per-model usage table + drill-down (req13; was `show_models`).
    Stats,
    /// Full-screen log tail (was the `l` log-panel size cycle).
    Logs,
    /// Session timeline (issue #34): persisted raw-io grouped by
    /// `metadata.user_id` into confidence-labeled per-session aggregates.
    Sessions,
}

/// UI-local state the renderer needs besides the data view: cursor, panes,
/// spinner frame, status line, attach banner.
pub(crate) struct Chrome {
    pub frame: usize,
    pub mode: Mode,
    /// Which summoned surface (if any) is drawn over MAIN this frame (issue #5).
    pub overlay: Overlay,
    pub status_line: Option<String>,
    /// Activity-log scroll offset: number of newest completed entries skipped
    /// (0 = live tail). Lets the panel page through the full history (req6).
    pub activity_scroll: usize,
    /// The activity entry (if any) currently click-expanded to show its detail
    /// lines (Feature B). Keyed by a STABLE identity (`ActivityKey` = at_ms +
    /// method + path + status) so it survives new rows prepending — never a
    /// list index.
    pub expanded_activity: Option<activity::ActivityKey>,
    /// Cursor row in the Stats overlay's model table.
    pub model_cursor: usize,
    /// Trailing window the Stats heatmap aggregates over (issue #23), cycled
    /// with `w` while the Stats overlay is open.
    pub stats_window: activity::StatsWindow,
    /// Folded session timeline for the Sessions overlay (issue #34), snapshotted
    /// from the persisted raw-io log when the overlay was opened. Empty otherwise.
    pub sessions: Vec<crate::session::Session>,
    /// True while a background `load_sessions` is in flight (issue: `s` froze the
    /// TUI ~10s). The overlay shows a spinner instead of the table/empty hint.
    pub sessions_loading: bool,
    /// Cursor row in the Sessions overlay's session list.
    pub session_cursor: usize,
    /// `Some` in attach mode.
    pub attach: Option<Attach>,
    /// Number of characters typed so far in `Mode::AddKey` — the footer shows
    /// a masked prompt (`••••`) of this width, never the raw key.
    pub add_input_len: usize,
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
    /// API key for a new account, queued by the `a` flow and performed by the
    /// event loop via `POST /llmux/add-account`. Held only until the POST
    /// fires; never logged or rendered raw.
    pending_add: Option<String>,
    /// Account name queued for removal (`r` confirm), performed by the event
    /// loop via `POST /llmux/remove-account`.
    pending_remove: Option<String>,
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
    /// Which summoned overlay is open over MAIN (issue #5): `a`→Accounts,
    /// `g`→Stats, `l`→Logs, `Esc`→None. MAIN renders every frame regardless.
    overlay: Overlay,
    /// Activity-log scroll offset (newest entries skipped; 0 = live tail).
    activity_scroll: usize,
    /// The click-expanded activity entry (Feature B), keyed by stable identity
    /// so it survives new rows prepending. `None` = nothing expanded.
    expanded_activity: Option<activity::ActivityKey>,
    /// The activity panel's hit-test layout from the LAST rendered frame: the
    /// panel rect + the clickable request rows. Recorded by the event loop after
    /// each `draw`, read by the mouse handler to map a click to an entry.
    activity_chrome: ui::ActivityChrome,
    /// Cursor row in the Stats overlay's model table.
    model_cursor: usize,
    /// Trailing window the Stats heatmap aggregates over (issue #23), cycled
    /// with `w` in the Stats overlay.
    stats_window: activity::StatsWindow,
    /// Folded session timeline (issue #34), loaded from the persisted raw-io log
    /// when the Sessions overlay is opened (`s`) and held until it is reopened.
    /// A point-in-time snapshot — re-opening re-reads the file. Empty otherwise.
    sessions: Vec<crate::session::Session>,
    /// True while the background load kicked off by `open_sessions` is running
    /// (read+parse+fold of the multi-MB raw-io log). Cleared when the loaded
    /// timeline arrives over `sessions_tx`. Drives the overlay loading spinner.
    sessions_loading: bool,
    /// Sender handed to the `spawn_blocking` load task by `open_sessions`; the
    /// event loop owns the receiver and applies the result. `None` only in unit
    /// tests that never run `event_loop` (they drive overlay state directly).
    sessions_tx: Option<mpsc::Sender<Vec<crate::session::Session>>>,
    /// Cursor row in the Sessions overlay's session list.
    session_cursor: usize,
    /// API-key buffer for `Mode::AddKey`. Held outside `Mode` so the enum
    /// stays `Copy` and the secret is owned in exactly one place; cleared on
    /// submit/cancel. Never rendered raw — the footer shows a masked width.
    add_input: String,
    /// New browser login queued by the `n` picker, performed by the event loop
    /// (the OAuth flow is async AND needs the raw terminal back — the loop
    /// suspends the TUI, runs the flow, then re-inits). Held on `App` (not
    /// `Remote`) because both local and attach mode use it; the only
    /// difference is where the minted credential is injected. `None` on a
    /// headless client (the picker shows the `llmux login` fallback instead).
    pending_login: Option<LoginKind>,
}

impl App {
    fn new(backend: Backend) -> Self {
        Self {
            backend,
            mode: Mode::Normal,
            frame: 0,
            should_quit: false,
            status: None,
            overlay: Overlay::None,
            activity_scroll: 0,
            expanded_activity: None,
            activity_chrome: ui::ActivityChrome::default(),
            model_cursor: 0,
            stats_window: activity::StatsWindow::default(),
            sessions: Vec::new(),
            sessions_loading: false,
            sessions_tx: None,
            session_cursor: 0,
            add_input: String::new(),
            pending_login: None,
        }
    }

    /// True when this dashboard is attached to a remote daemon (not the
    /// in-process server). Reused to decide where a minted login credential is
    /// injected (in-process vs. `POST /llmux/inject-account`).
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
            overlay: self.overlay,
            activity_scroll: self.activity_scroll,
            expanded_activity: self.expanded_activity.clone(),
            model_cursor: self.model_cursor,
            stats_window: self.stats_window,
            sessions: self.sessions.clone(),
            sessions_loading: self.sessions_loading,
            session_cursor: self.session_cursor,
            add_input_len: self.add_input.chars().count(),
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
        // A pending `Mode` interaction (account switch / key entry / remove
        // confirm / login picker) always takes the key first — these run WITHIN
        // the Accounts overlay (issues #3/#4) and must keep working unchanged.
        match self.mode {
            Mode::Select { idx } => return self.on_key_select(key.code, idx, view),
            Mode::AddKey => return self.on_key_add(key.code),
            Mode::ConfirmRemove { idx } => return self.on_key_confirm_remove(key.code, idx, view),
            Mode::NewLogin { idx } => return self.on_key_new_login(key.code, idx),
            Mode::Normal => {}
        }
        // Otherwise (Mode::Normal): the active overlay, if any, gets the key;
        // MAIN-only keys run when no overlay is open.
        match self.overlay {
            Overlay::None => self.on_key_main(key.code, view),
            Overlay::Accounts => self.on_key_accounts(key.code, view),
            Overlay::Stats => self.on_key_stats(key.code, view),
            Overlay::Logs => self.on_key_logs(key.code),
            Overlay::Sessions => self.on_key_sessions(key.code),
        }
    }

    /// Handle a mouse event (Feature B). Mouse input is ADDITIVE — keyboard nav
    /// is untouched. It is ignored entirely unless MAIN owns the screen (no
    /// overlay, `Mode::Normal`); an overlay or a pending interaction keeps the
    /// activity panel out of reach, so a stray click can't toggle a hidden row.
    /// A left-click inside the activity list toggles the clicked entry's expand
    /// state; the wheel scrolls the activity history. Returns whether the event
    /// changed anything (→ redraw).
    fn on_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        view: Option<&DashboardView>,
    ) -> bool {
        // Only MAIN (no overlay, no pending mode interaction) gets the mouse.
        if self.overlay != Overlay::None || self.mode != Mode::Normal {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match ui::hit_test_activity(&self.activity_chrome, mouse.column, mouse.row) {
                    Some(key) => {
                        self.toggle_expand(key);
                        true
                    }
                    None => false,
                }
            }
            // Wheel up = into history, down = toward the live tail — same
            // direction as the ↑/↓ keys (a nice-to-have bonus).
            MouseEventKind::ScrollUp => {
                self.scroll_activity(1, view);
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll_activity(-1, view);
                true
            }
            _ => false,
        }
    }

    /// Toggle the click-expanded activity entry by its stable key: clicking the
    /// expanded row again collapses it, clicking a different row moves the
    /// expansion there.
    fn toggle_expand(&mut self, key: activity::ActivityKey) {
        if self.expanded_activity.as_ref() == Some(&key) {
            self.expanded_activity = None;
        } else {
            self.expanded_activity = Some(key);
        }
    }

    /// Key handling for the Stats overlay (`g`). Arrows/`j`/`k` move the cursor
    /// through model rows; `g`/`Esc` closes back to MAIN; `q` quits.
    fn on_key_stats(&mut self, code: KeyCode, view: Option<&DashboardView>) {
        let len = view.map_or(0, |v| v.model_usage.len());
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('g') | KeyCode::Esc => self.overlay = Overlay::None,
            // Cycle the heatmap window 24h ↔ 72h (issue #23).
            KeyCode::Char('w') => self.stats_window = self.stats_window.next(),
            KeyCode::Up | KeyCode::Char('k') => self.move_model_cursor(-1, len),
            KeyCode::Down | KeyCode::Char('j') => self.move_model_cursor(1, len),
            KeyCode::PageUp => self.move_model_cursor(-10, len),
            KeyCode::PageDown => self.move_model_cursor(10, len),
            KeyCode::Home => self.model_cursor = 0,
            KeyCode::End => self.model_cursor = len.saturating_sub(1),
            _ => {}
        }
    }

    /// Key handling for the Logs overlay (`l`). `l`/`Esc` closes back to MAIN;
    /// `q` quits. The tail is full-screen, so there is no size cycle anymore.
    fn on_key_logs(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('l') | KeyCode::Esc => self.overlay = Overlay::None,
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

    /// Key handling for the Sessions overlay (`s`, issue #34). Arrows/`j`/`k`
    /// move the cursor through session rows; `s`/`Esc` closes back to MAIN; `q`
    /// quits. The folded sessions are a snapshot taken at open time.
    fn on_key_sessions(&mut self, code: KeyCode) {
        let len = self.sessions.len();
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('s') | KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => self.move_session_cursor(-1, len),
            KeyCode::Down | KeyCode::Char('j') => self.move_session_cursor(1, len),
            KeyCode::PageUp => self.move_session_cursor(-10, len),
            KeyCode::PageDown => self.move_session_cursor(10, len),
            KeyCode::Home => self.session_cursor = 0,
            KeyCode::End => self.session_cursor = len.saturating_sub(1),
            _ => {}
        }
    }

    /// Move the session cursor by `delta` rows, clamped to `[0, len-1]`.
    fn move_session_cursor(&mut self, delta: i64, len: usize) {
        if len == 0 {
            self.session_cursor = 0;
            return;
        }
        let next = (self.session_cursor as i64).saturating_add(delta);
        self.session_cursor = next.clamp(0, (len - 1) as i64) as usize;
    }

    /// Open the Sessions overlay (`s`, issue #34): kick off a background read of
    /// the persisted raw-io log from `$XDG_STATE_HOME/llmux/raw-io.jsonl`, fold it
    /// into a confidence-labeled session timeline off the runtime, and open the
    /// overlay immediately with a loading spinner. The read+parse+fold is blocking
    /// IO/CPU over a multi-MB log, so running it inline inside the async event
    /// loop froze the whole TUI ~10s — it now runs on the blocking pool and the
    /// loaded timeline arrives over `sessions_tx`, mirroring the remote-fetch
    /// pattern. A missing/unreadable file folds to an empty timeline (the overlay
    /// then shows the empty hint). The snapshot is point-in-time — re-opening
    /// re-reads the file.
    fn open_sessions(&mut self) {
        self.overlay = Overlay::Sessions;
        self.session_cursor = 0;
        if self.sessions_loading {
            return; // a load is already in flight
        }
        self.sessions_loading = true;
        if let Some(tx) = self.sessions_tx.clone() {
            // read + parse + fold is blocking IO/CPU → off the runtime onto the
            // blocking pool so the event loop keeps rendering and taking input.
            tokio::task::spawn_blocking(move || {
                let sessions = load_sessions();
                let _ = tx.blocking_send(sessions);
            });
        }
        // No tx (only in unit tests that never run `event_loop`) → stays in the
        // loading state; those tests drive overlay/sessions state directly, not
        // this path.
    }

    /// Key handling for MAIN (no overlay open). `a`/`g`/`l` summon the overlays;
    /// `R` reloads, `f/m/e` drive codex, arrows scroll the activity log, `q`
    /// quits. The account-mutation affordances (add/remove/login/switch) live in
    /// the Accounts overlay, reached with `a`.
    fn on_key_main(&mut self, code: KeyCode, view: Option<&DashboardView>) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('R') => self.reload(),
            // Summon overlays (issue #5).
            KeyCode::Char('a') => self.overlay = Overlay::Accounts,
            KeyCode::Char('g') => self.open_stats(view),
            KeyCode::Char('l') => self.overlay = Overlay::Logs,
            // Session timeline (issue #34): read + fold the persisted raw-io log.
            KeyCode::Char('s') => self.open_sessions(),
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

    /// Key handling for the Accounts overlay (`a`). Houses the issue #3/#4
    /// affordances — switch (`s`), add an API key (`a`), remove (`r`), start a
    /// new browser login (`n`) — each entering its own [`Mode`] which is handled
    /// over this overlay. `Esc` closes back to MAIN; `q` quits.
    fn on_key_accounts(&mut self, code: KeyCode, view: Option<&DashboardView>) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => self.overlay = Overlay::None,
            // Switch the active account (the `s` switcher, now scoped to this
            // overlay). Rows render in selection order; the current account
            // (when one exists) is always row 0 — start the cursor there.
            KeyCode::Char('s') => {
                let accounts = view.map_or(0, |v| v.snapshot.accounts.len());
                if accounts == 0 {
                    self.set_status("no accounts to switch between".into());
                } else {
                    self.mode = Mode::Select { idx: 0 };
                }
            }
            // Add an API-key account (issue #3): works in BOTH local and attach
            // mode (local: in-process config update + pool reload; remote:
            // POST /llmux/add-account).
            KeyCode::Char('a') => {
                self.add_input.clear();
                self.mode = Mode::AddKey;
                self.set_status(
                    "add account: paste an Anthropic API key, Enter to add, Esc to cancel".into(),
                );
            }
            // Remove the selected account (issue #3): a destructive delete, so
            // it opens a confirm step (y/N) — never a silent delete.
            KeyCode::Char('r') => {
                let len = view.map_or(0, |v| v.snapshot.accounts.len());
                if len == 0 {
                    self.set_status("no accounts to remove".into());
                } else {
                    self.mode = Mode::ConfirmRemove { idx: 0 };
                }
            }
            // Start a NEW browser login (issue #4): opens a provider picker
            // (Claude / Codex). The OAuth flow runs in THIS client; the minted
            // credential is injected into the daemon, so it works in both local
            // and attach mode with no restart.
            KeyCode::Char('n') => self.open_new_login(),
            _ => {}
        }
    }

    /// Open the Stats overlay (`g`). No-op (with a hint) until at least one
    /// model row exists, matching the old detailed-view guard (req13).
    fn open_stats(&mut self, view: Option<&DashboardView>) {
        if view.is_some_and(|v| !v.model_usage.is_empty()) {
            self.overlay = Overlay::Stats;
            self.model_cursor = 0;
        } else {
            self.set_status("models: no model usage yet".into());
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
                // Carry the live `client_model` override forward: it is a
                // config-only opt-in the TUI settings panel doesn't manage, so
                // a model/fast/effort change here must not silently clear it.
                let client_model = state.codex.shape().client_model;
                state.codex.set_shape(crate::provider::codex::CodexShape {
                    model: new.model.clone(),
                    client_model,
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
            // `n` from the switcher: start a brand-new login (issue #4's
            // headline path — "start a new login from the account switcher").
            KeyCode::Char('n') => self.open_new_login(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => self.mode = Mode::Normal,
            _ => self.mode = Mode::Select { idx },
        }
    }

    /// Open the new-login provider picker, OR — on a headless client that
    /// cannot open a browser — refuse with the `llmux login` fallback instead
    /// of starting a flow that would hang on the callback. "Headless" is
    /// decided by [`Self::can_open_browser`].
    fn open_new_login(&mut self) {
        if can_open_browser() {
            self.mode = Mode::NewLogin { idx: 0 };
            self.set_status(
                "new login: ↑↓ pick provider, Enter to open the browser, Esc to cancel".into(),
            );
        } else {
            self.mode = Mode::Normal;
            self.set_status(headless_login_hint(self.is_remote()));
        }
    }

    /// Key handling for `Mode::NewLogin` — the provider picker. Up/down move
    /// the cursor; Enter queues the chosen login for the event loop (which
    /// suspends the TUI, runs the browser flow, then re-inits); Esc cancels.
    fn on_key_new_login(&mut self, code: KeyCode, idx: usize) {
        let len = LoginKind::ALL.len();
        let idx = idx.min(len - 1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::NewLogin {
                    idx: idx.saturating_sub(1),
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::NewLogin {
                    idx: (idx + 1).min(len - 1),
                };
            }
            KeyCode::Enter => {
                let kind = LoginKind::ALL[idx];
                self.pending_login = Some(kind);
                self.mode = Mode::Normal;
                self.set_status(format!("opening browser for {}…", kind.label()));
            }
            // Any other key cancels.
            _ => {
                self.mode = Mode::Normal;
                self.set_status("new login cancelled".into());
            }
        }
    }

    fn take_pending_login(&mut self) -> Option<LoginKind> {
        self.pending_login.take()
    }

    /// Key handling for `Mode::AddKey` — typing the new account's API key.
    /// Printable chars append to the buffer; Backspace deletes; Enter submits;
    /// Esc cancels. The buffer is never rendered raw (the footer shows a masked
    /// width via [`Chrome::add_input_len`]).
    fn on_key_add(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.add_input.clear();
                self.mode = Mode::Normal;
                self.set_status("add account cancelled".into());
            }
            KeyCode::Enter => self.submit_add(),
            KeyCode::Backspace => {
                self.add_input.pop();
            }
            // Guard against accidental control chars; only printable input.
            KeyCode::Char(c) if !c.is_control() => {
                self.add_input.push(c);
            }
            _ => {}
        }
    }

    /// Submit the typed API key: add the account in-process (local) or queue a
    /// `POST /llmux/add-account` (remote). The buffer is cleared either way so
    /// the secret does not linger in memory longer than necessary.
    fn submit_add(&mut self) {
        let api_key = self.add_input.trim().to_string();
        self.add_input.clear();
        self.mode = Mode::Normal;
        if api_key.is_empty() {
            self.set_status("add account cancelled: empty key".into());
            return;
        }
        match &mut self.backend {
            Backend::Local(state) => match state.add_apikey_account(None, &api_key) {
                // Status echoes the assigned NAME only — never the key.
                Ok((name, _outcome)) => self.set_status(format!("added account {name}")),
                Err(err) => self.set_status(format!("add account failed: {err}")),
            },
            Backend::Remote(remote) => {
                remote.pending_add = Some(api_key);
                self.set_status("adding account…".into());
            }
        }
    }

    /// Key handling for `Mode::ConfirmRemove` — a destructive delete gate.
    /// `y` confirms the removal; any other key cancels. Arrow/j/k move the
    /// target row so the operator can pick which account to delete.
    fn on_key_confirm_remove(&mut self, code: KeyCode, idx: usize, view: Option<&DashboardView>) {
        let len = view.map_or(0, |v| v.snapshot.accounts.len());
        if len == 0 {
            self.mode = Mode::Normal;
            return;
        }
        let idx = idx.min(len - 1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::ConfirmRemove {
                    idx: idx.saturating_sub(1),
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::ConfirmRemove {
                    idx: (idx + 1).min(len - 1),
                };
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.submit_remove(idx, view);
                self.mode = Mode::Normal;
            }
            // Any other key (Esc/n/q/…) cancels — delete is never silent.
            _ => {
                self.mode = Mode::Normal;
                self.set_status("remove cancelled".into());
            }
        }
    }

    /// Resolve the display row to an account name, then remove it in-process
    /// (local) or queue a `POST /llmux/remove-account` (remote).
    fn submit_remove(&mut self, idx: usize, view: Option<&DashboardView>) {
        let Some(view) = view else { return };
        let now = SystemTime::now();
        // The cursor indexes DISPLAY rows (selection order), not config order.
        let order = view.display_order(now);
        let Some(target) = order.get(idx).and_then(|&i| view.snapshot.accounts.get(i)) else {
            return;
        };
        let name = target.id.0.clone();
        match &mut self.backend {
            Backend::Local(state) => match state.remove_account(&name) {
                Ok(true) => self.set_status(format!("removed account {name}")),
                Ok(false) => self.set_status(format!("account {name} not found")),
                Err(err) => self.set_status(format!("remove failed: {err}")),
            },
            Backend::Remote(remote) => {
                remote.pending_remove = Some(name.clone());
                self.set_status(format!("removing {name}…"));
            }
        }
    }

    fn take_pending_add(&mut self) -> Option<String> {
        match &mut self.backend {
            Backend::Remote(remote) => remote.pending_add.take(),
            Backend::Local(_) => None,
        }
    }

    fn take_pending_remove(&mut self) -> Option<String> {
        match &mut self.backend {
            Backend::Remote(remote) => remote.pending_remove.take(),
            Backend::Local(_) => None,
        }
    }

    /// Perform the queued remote add (`POST /llmux/add-account`). The api key
    /// travels in the JSON body over the (loopback or api-key-gated) control
    /// channel; the response echoes only a masked form, so nothing here logs
    /// or displays the raw key.
    async fn perform_remote_add(&mut self, api_key: String) {
        let Backend::Remote(remote) = &mut self.backend else {
            return;
        };
        let url = format!("{}/llmux/add-account", remote.base_url);
        let mut request = remote
            .client
            .post(&url)
            .json(&serde_json::json!({ "api_key": api_key }));
        if let Some(key) = &remote.api_key {
            request = request.header("x-api-key", key);
        }
        let message = match request.send().await {
            Ok(response) if response.status().is_success() => {
                let name = response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["name"].as_str().map(str::to_string))
                    .unwrap_or_else(|| "account".into());
                format!("added account {name}")
            }
            Ok(response) => format!("add account failed: {}", response.status()),
            Err(err) => format!("add account failed: {err}"),
        };
        self.set_status(message);
    }

    /// Perform the queued remote removal (`POST /llmux/remove-account`).
    async fn perform_remote_remove(&mut self, name: String) {
        let Backend::Remote(remote) = &mut self.backend else {
            return;
        };
        let url = format!("{}/llmux/remove-account", remote.base_url);
        let mut request = remote
            .client
            .post(&url)
            .json(&serde_json::json!({ "name": name, "confirm": true }));
        if let Some(key) = &remote.api_key {
            request = request.header("x-api-key", key);
        }
        let message = match request.send().await {
            Ok(response) if response.status().is_success() => format!("removed account {name}"),
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let detail = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                    .unwrap_or_else(|| status.to_string());
                format!("remove {name} failed: {detail}")
            }
            Err(err) => format!("remove {name} failed: {err}"),
        };
        self.set_status(message);
    }

    /// Run a new browser login in THIS client and inject the resulting account
    /// into the daemon (issue #4). The OAuth flow (`login_interactive` /
    /// `login_codex_interactive`) opens the browser + binds a localhost
    /// callback HERE; the minted credential is then injected — in-process when
    /// local, via `POST /llmux/inject-account` when attached. ONE code path:
    /// the only fork is where the credential lands.
    ///
    /// MUST run with the raw terminal SUSPENDED (the flow prints prompts and
    /// may read a pasted code from stdin) — the event loop handles
    /// suspend/resume around this call. No token is logged or rendered raw; the
    /// status line shows only the resulting account name.
    async fn perform_login(&mut self, kind: LoginKind) {
        let client = reqwest::Client::new();
        // Build the account by running the client-side login. The profile fetch
        // (Anthropic only) hits the public upstream with the user's own token.
        let account = match kind {
            LoginKind::Anthropic => {
                let upstream = match &self.backend {
                    Backend::Local(state) => state.config.upstream.clone(),
                    // The attached client has no copy of the daemon's config;
                    // the profile endpoint is the public Anthropic API.
                    Backend::Remote(_) => crate::config::DEFAULT_UPSTREAM.to_string(),
                };
                match crate::cli::login::oauth_login_to_account(&client, &upstream).await {
                    Ok(account) => account,
                    Err(err) => {
                        self.set_status(format!("login failed: {err}"));
                        return;
                    }
                }
            }
            LoginKind::Codex => {
                let token_url = match &self.backend {
                    Backend::Local(state) => state.config.codex.token_url.clone(),
                    Backend::Remote(_) => crate::config::DEFAULT_CODEX_TOKEN_URL.to_string(),
                };
                match crate::auth::codex::login_codex_interactive(&client, &token_url).await {
                    Ok(account) => account,
                    Err(err) => {
                        self.set_status(format!("codex login failed: {err}"));
                        return;
                    }
                }
            }
        };

        // Inject: in-process locally, or relay to the daemon when attached.
        match &mut self.backend {
            Backend::Local(state) => match state.inject_account(account) {
                Ok((name, _outcome)) => self.set_status(format!("logged in: added {name}")),
                Err(err) => self.set_status(format!("login persist failed: {err}")),
            },
            Backend::Remote(_) => self.perform_remote_inject(account).await,
        }
    }

    /// Relay a freshly-minted OAuth/Codex account to the daemon
    /// (`POST /llmux/inject-account`). The credential travels in the JSON body
    /// over the (loopback or api-key-gated) control channel; the response
    /// echoes only a masked access token, so nothing here logs or displays the
    /// raw token.
    async fn perform_remote_inject(&mut self, account: AccountConfig) {
        let Backend::Remote(remote) = &mut self.backend else {
            return;
        };
        let url = format!("{}/llmux/inject-account", remote.base_url);
        // `AccountConfig` serializes to the `{name, type, …credential}` shape
        // the inject endpoint deserializes (the flattened, type-tagged enum).
        let mut request = remote.client.post(&url).json(&account);
        if let Some(key) = &remote.api_key {
            request = request.header("x-api-key", key);
        }
        let message = match request.send().await {
            Ok(response) if response.status().is_success() => {
                let name = response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["name"].as_str().map(str::to_string))
                    .unwrap_or_else(|| account.name.clone());
                format!("logged in: added {name}")
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let detail = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                    .unwrap_or_else(|| status.to_string());
                format!("login inject failed: {detail}")
            }
            Err(err) => format!("login inject failed: {err}"),
        };
        self.set_status(message);
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

/// Initialize the terminal for the dashboard: `ratatui::try_init` (raw mode +
/// alternate screen + a panic hook that restores them) PLUS native mouse
/// capture (Feature B), which `try_init` does NOT enable. Because the panic
/// hook installed by `try_init` only undoes raw-mode/alt-screen, we chain our
/// own hook BEFORE it that also disables mouse capture, so a panic on any path
/// leaves the terminal fully restored. Every call site uses this helper (and
/// [`restore_terminal`]) so the enable/disable always pair up — including the
/// login suspend/resume path that re-inits the terminal mid-session.
fn init_terminal() -> std::io::Result<ratatui::DefaultTerminal> {
    // Chain a mouse-disable into the panic hook BEFORE `try_init` installs its
    // own restore hook. `try_init`'s hook runs `restore()` then calls the
    // previous hook (this one), so on panic the order is: leave alt-screen +
    // raw mode, then disable mouse capture. Idempotent if it runs twice.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        prev(info);
    }));
    let terminal = ratatui::try_init()?;
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    Ok(terminal)
}

/// Tear down the terminal: disable mouse capture FIRST, then `ratatui::restore`
/// (leave alternate screen + disable raw mode). The inverse of
/// [`init_terminal`]; runs on every normal exit path.
fn restore_terminal() {
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

/// Run the in-process dashboard over live server state until quit.
///
/// Terminal lifecycle via [`init_terminal`] / [`restore_terminal`]: raw mode +
/// alternate screen + mouse capture, all undone on every exit path (and the
/// panic hook).
pub async fn run_local(state: crate::proxy::server::AppState) -> std::io::Result<()> {
    let mut terminal = init_terminal()?;
    let mut app = App::new(Backend::Local(Box::new(state)));
    let result = event_loop(&mut terminal, &mut app, None).await;
    restore_terminal();
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
    let mut terminal = init_terminal()?;
    let mut app = App::new(Backend::Remote(Box::new(Remote {
        client,
        base_url: opts.base_url,
        api_key: opts.api_key,
        pid: opts.pid,
        doc: None,
        connected: false,
        pending_switch: None,
        pending_codex: None,
        pending_add: None,
        pending_remove: None,
    })));
    let result = event_loop(&mut terminal, &mut app, Some(rx)).await;
    restore_terminal();
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
    // Sessions overlay (`s`) loads the persisted raw-io log on the blocking pool
    // and delivers the folded timeline here — mirrors the remote fetch channel so
    // the read+parse+fold never blocks this select (it once froze the TUI ~10s).
    let (sess_tx, mut sess_rx) = mpsc::channel::<Vec<crate::session::Session>>(4);
    app.sessions_tx = Some(sess_tx);
    // Input is event-driven, not polled: `EventStream` parks on the terminal fd
    // (mio) and only wakes the task when a real key/mouse/resize/paste arrives.
    // At idle (no input) this contributes zero wakeups, unlike a fixed-interval
    // poll which fired ~30×/s reading nothing. See issue #14 (idle quiescence).
    let mut events = EventStream::new();

    loop {
        let mut redraw = tokio::select! {
            _ = render.tick() => {
                app.frame = app.frame.wrapping_add(1);
                true
            }
            // Wakes only when the terminal actually has an event. Handle the one
            // the stream delivered, then drain any *already-ready* siblings (a
            // multi-byte paste) without blocking, so a burst is one redraw.
            Some(event) = events.next() => drain_input(app, event?)?,
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
            // The background session load finished — swap in the folded timeline
            // and drop the loading state so the overlay shows the table/hint.
            Some(sessions) = sess_rx.recv() => {
                app.sessions = sessions;
                app.sessions_loading = false;
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
        if let Some(api_key) = app.take_pending_add() {
            app.perform_remote_add(api_key).await;
            redraw = true;
        }
        if let Some(name) = app.take_pending_remove() {
            app.perform_remote_remove(name).await;
            redraw = true;
        }
        // A new browser login needs the RAW terminal back: the OAuth flow
        // prints prompts and may read a pasted code from stdin, which would
        // corrupt the alternate-screen TUI. Suspend (restore the terminal),
        // run the flow, then re-init and force a full redraw. The fetch poller
        // (remote mode) keeps running in the background meanwhile.
        if let Some(kind) = app.take_pending_login() {
            restore_terminal();
            app.perform_login(kind).await;
            *terminal = init_terminal()?;
            let _ = terminal.clear();
            redraw = true;
        }
        if app.should_quit {
            return Ok(());
        }
        if redraw {
            let view = app.view(SystemTime::now());
            let chrome = app.chrome();
            // Capture the activity panel's hit-test layout from this frame so a
            // left-click in the next input drain maps to the right entry.
            let mut hits = None;
            terminal.draw(|frame| ui::draw(frame, view.as_ref(), &chrome, &mut hits))?;
            app.activity_chrome = hits.unwrap_or_default();
        }
    }
}

/// Handle `first` (the event the `EventStream` just woke us with), then drain
/// any *already-ready* terminal events without blocking (`poll(ZERO)` is a
/// non-blocking readiness check), so a multi-byte paste is one redraw rather
/// than one per byte. Returns whether anything warrants a redraw.
fn drain_input(app: &mut App, first: Event) -> std::io::Result<bool> {
    let mut dirty = false;
    // Built once per drain: key handlers read the same frame the user saw.
    let mut view: Option<Option<DashboardView>> = None;
    apply_event(app, first, &mut view, &mut dirty);
    while crossterm::event::poll(Duration::ZERO)? {
        apply_event(app, crossterm::event::read()?, &mut view, &mut dirty);
    }
    Ok(dirty)
}

/// Dispatch one terminal event into the app, lazily building the per-drain view
/// the first time a key/mouse handler needs it and flipping `dirty` when the
/// event warrants a redraw.
fn apply_event(
    app: &mut App,
    event: Event,
    view: &mut Option<Option<DashboardView>>,
    dirty: &mut bool,
) {
    match event {
        Event::Key(key) => {
            let view = view.get_or_insert_with(|| app.view(SystemTime::now()));
            app.on_key(key, view.as_ref());
            *dirty = true;
        }
        Event::Mouse(mouse) => {
            let view = view.get_or_insert_with(|| app.view(SystemTime::now()));
            if app.on_mouse(mouse, view.as_ref()) {
                *dirty = true;
            }
        }
        Event::Resize(_, _) => *dirty = true,
        _ => {}
    }
}

/// Read the persisted raw-io log and fold it into a session timeline (issue #34).
///
/// The path is resolved exactly like the daemon's capture path
/// (`$XDG_STATE_HOME/llmux/raw-io.jsonl`). A missing/unreadable file, or no state
/// dir, yields an empty timeline — best-effort, never panics. Unparseable lines
/// are skipped (the same tolerance `raw_io::prune` applies on rewrite). Only the
/// metadata each record carries is folded; no prompt content is retained.
fn load_sessions() -> Vec<crate::session::Session> {
    let Some(path) = crate::cli::daemon::raw_io_path() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let records: Vec<crate::proxy::raw_io::RawIoRecord> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    crate::session::fold_sessions(&records)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `App` on a remote backend — buildable without a terminal, so the
    /// key-handling state machine (issue #4 new-login flow) is unit-testable.
    fn remote_app() -> App {
        let client = reqwest::Client::new();
        App::new(Backend::Remote(Box::new(Remote {
            client,
            base_url: "http://localhost:3456".into(),
            api_key: None,
            pid: None,
            doc: None,
            connected: false,
            pending_switch: None,
            pending_codex: None,
            pending_add: None,
            pending_remove: None,
        })))
    }

    #[test]
    fn new_login_picker_moves_and_enter_queues_chosen_provider() {
        let mut app = remote_app();
        // Enter the picker (bypassing the env-dependent browser check).
        app.mode = Mode::NewLogin { idx: 0 };

        // Down moves to the Codex row.
        app.on_key_new_login(KeyCode::Down, 0);
        assert_eq!(app.mode, Mode::NewLogin { idx: 1 });

        // Enter queues that provider for the event loop and returns to Normal.
        app.on_key_new_login(KeyCode::Enter, 1);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.take_pending_login(), Some(LoginKind::Codex));
        // Drained exactly once.
        assert_eq!(app.take_pending_login(), None);
    }

    #[test]
    fn new_login_picker_enter_on_first_row_picks_anthropic() {
        let mut app = remote_app();
        app.mode = Mode::NewLogin { idx: 0 };
        app.on_key_new_login(KeyCode::Enter, 0);
        assert_eq!(app.take_pending_login(), Some(LoginKind::Anthropic));
    }

    #[test]
    fn new_login_picker_esc_cancels_without_queueing() {
        let mut app = remote_app();
        app.mode = Mode::NewLogin { idx: 1 };
        app.on_key_new_login(KeyCode::Esc, 1);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.take_pending_login(), None, "cancel queues nothing");
    }

    #[test]
    fn new_login_picker_up_clamps_at_top() {
        let mut app = remote_app();
        app.mode = Mode::NewLogin { idx: 0 };
        app.on_key_new_login(KeyCode::Up, 0);
        assert_eq!(app.mode, Mode::NewLogin { idx: 0 });
    }

    #[test]
    fn headless_fallback_names_llmux_login_per_mode() {
        // Attached: point the operator at the daemon host.
        let remote = headless_login_hint(true);
        assert!(remote.contains("llmux login"), "{remote}");
        assert!(remote.contains("daemon host"), "{remote}");
        // Local: this host.
        let local = headless_login_hint(false);
        assert!(local.contains("llmux login"), "{local}");
        assert!(local.contains("this host"), "{local}");
    }

    #[test]
    fn login_kind_labels_distinguish_providers() {
        assert!(LoginKind::Anthropic.label().contains("Anthropic"));
        assert!(LoginKind::Codex.label().contains("Codex"));
        assert_eq!(LoginKind::ALL.len(), 2);
    }

    // --- issue #5: overlay key routing -------------------------------------

    /// Issue #5 acceptance (state machine): every summoned overlay follows the
    /// same open → `Esc` → closed cycle, and MAIN state held on `App` (the
    /// activity scroll, the click-expanded row — the things that "keep updating
    /// underneath") is preserved across the open and the close, never reset.
    /// `Esc` always lands back on `Overlay::None` (MAIN), from any overlay.
    #[test]
    fn open_overlay_preserves_main_state_then_esc_returns_to_main() {
        let view = stats_view_with_account();
        // Seed MAIN-owned state so we can prove the overlay round-trip leaves it
        // untouched (MAIN keeps its place / expansion underneath the overlay).
        let expanded = activity::ActivityKey {
            at_ms: 7,
            method: "POST".into(),
            path: "/v1/messages".into(),
            status: 200,
        };

        // Each shortcut summons one overlay; `Esc` always closes it. The whole
        // round-trip is driven through the unified `on_key` entry point, so the
        // production overlay-aware routing (open via MAIN, close via the active
        // overlay's `Esc`) is what's under test.
        for open_key in [KeyCode::Char('a'), KeyCode::Char('g'), KeyCode::Char('l')] {
            let mut app = remote_app();
            app.activity_scroll = 3;
            app.expanded_activity = Some(expanded.clone());
            assert_eq!(app.overlay, Overlay::None, "starts on MAIN");

            // Open: the active overlay is set; MAIN-owned state is untouched.
            app.on_key(press(open_key), Some(&view));
            assert_ne!(
                app.overlay,
                Overlay::None,
                "{open_key:?} summons an overlay"
            );
            assert_eq!(
                app.activity_scroll, 3,
                "MAIN scroll preserved under overlay"
            );
            assert_eq!(
                app.expanded_activity.as_ref(),
                Some(&expanded),
                "MAIN expansion preserved under overlay"
            );

            // Esc: back to MAIN, with MAIN state still intact.
            app.on_key(press(KeyCode::Esc), Some(&view));
            assert_eq!(app.overlay, Overlay::None, "Esc returns to MAIN");
            assert_eq!(
                app.activity_scroll, 3,
                "MAIN scroll survives the round-trip"
            );
            assert_eq!(
                app.expanded_activity.as_ref(),
                Some(&expanded),
                "MAIN expansion survives the round-trip"
            );
        }
    }

    /// `a` opens the Accounts overlay; `Esc` returns to MAIN.
    #[test]
    fn a_opens_accounts_overlay_and_esc_returns_to_main() {
        let mut app = remote_app();
        assert_eq!(app.overlay, Overlay::None);
        app.on_key_main(KeyCode::Char('a'), None);
        assert_eq!(app.overlay, Overlay::Accounts);
        app.on_key_accounts(KeyCode::Esc, None);
        assert_eq!(app.overlay, Overlay::None);
    }

    /// `l` opens the Logs overlay; `l`/`Esc` close it.
    #[test]
    fn l_opens_logs_overlay_and_esc_returns_to_main() {
        let mut app = remote_app();
        app.on_key_main(KeyCode::Char('l'), None);
        assert_eq!(app.overlay, Overlay::Logs);
        app.on_key_logs(KeyCode::Esc);
        assert_eq!(app.overlay, Overlay::None);
        // `l` toggles back too.
        app.on_key_main(KeyCode::Char('l'), None);
        assert_eq!(app.overlay, Overlay::Logs);
        app.on_key_logs(KeyCode::Char('l'));
        assert_eq!(app.overlay, Overlay::None);
    }

    /// `s` opens the Sessions overlay (issue #34); arrows move the cursor within
    /// the loaded session list and `s`/`Esc` close back to MAIN. The session list
    /// is injected directly (the real loader reads a file off disk, not under
    /// test here — `fold_sessions` is unit-tested in `crate::session`).
    #[test]
    fn s_opens_sessions_overlay_navigates_and_esc_returns_to_main() {
        use crate::session::{Confidence, Session};
        let session = |uid: &str| Session {
            user_id: Some(uid.into()),
            requests: 1,
            tokens_in: 0,
            tokens_out: 0,
            models: vec![],
            accounts: vec![],
            account_rotations: 0,
            first_ms: 0,
            last_ms: 0,
            confidence: Confidence::High,
        };
        let mut app = remote_app();
        app.sessions = vec![session("u-1"), session("u-2"), session("u-3")];
        app.overlay = Overlay::Sessions;
        assert_eq!(app.session_cursor, 0);

        // Down/up move within bounds.
        app.on_key_sessions(KeyCode::Down);
        assert_eq!(app.session_cursor, 1);
        app.on_key_sessions(KeyCode::Up);
        assert_eq!(app.session_cursor, 0);
        // Up clamps at the top.
        app.on_key_sessions(KeyCode::Up);
        assert_eq!(app.session_cursor, 0);
        // End jumps to the last row; Down clamps there.
        app.on_key_sessions(KeyCode::End);
        assert_eq!(app.session_cursor, 2);
        app.on_key_sessions(KeyCode::Down);
        assert_eq!(app.session_cursor, 2);

        // s/Esc close back to MAIN.
        app.on_key_sessions(KeyCode::Char('s'));
        assert_eq!(app.overlay, Overlay::None);
    }

    /// `open_sessions` must NOT block on the file read: it opens the overlay and
    /// flips into the loading state immediately, leaving `sessions` untouched.
    /// With no `sessions_tx` (the event loop never runs under test) the load is
    /// never kicked off, so the file is never read and nothing populates the
    /// list — proving the key handler returns instantly.
    #[test]
    fn open_sessions_is_non_blocking_and_enters_loading_state() {
        let mut app = remote_app();
        assert!(!app.sessions_loading);
        assert!(app.sessions.is_empty());

        app.open_sessions();

        assert_eq!(app.overlay, Overlay::Sessions);
        assert!(app.sessions_loading, "overlay enters the loading state");
        assert_eq!(app.session_cursor, 0);
        assert!(
            app.sessions.is_empty(),
            "no tx under test → load not kicked off, sessions stay empty"
        );
    }

    /// Reopening while a load is still in flight is a no-op guard, not a second
    /// load: it stays in the loading state and does not clear the cursor twice or
    /// touch `sessions`.
    #[test]
    fn open_sessions_reopen_while_loading_is_a_noop_guard() {
        let mut app = remote_app();
        app.open_sessions();
        assert!(app.sessions_loading);
        // Move the cursor as if a prior list were shown, then reopen.
        app.session_cursor = 5;
        app.open_sessions();
        // Still loading; the early-return guard ran AFTER resetting the cursor.
        assert!(app.sessions_loading);
        assert_eq!(app.overlay, Overlay::Sessions);
    }

    /// `g` opens the Stats overlay only when model usage exists; `g`/`Esc`
    /// close it. The no-data guard keeps MAIN (matching the old `show_models`
    /// behavior).
    #[test]
    fn g_opens_stats_overlay_only_with_model_data() {
        let mut app = remote_app();
        // No view → no model data → stays on MAIN with a hint.
        app.on_key_main(KeyCode::Char('g'), None);
        assert_eq!(app.overlay, Overlay::None);

        let view = stats_view();
        app.on_key_main(KeyCode::Char('g'), Some(&view));
        assert_eq!(app.overlay, Overlay::Stats);
        app.on_key_stats(KeyCode::Esc, Some(&view));
        assert_eq!(app.overlay, Overlay::None);
    }

    /// `w` in the Stats overlay cycles the heatmap window 24h ↔ 72h (issue #23)
    /// without closing the overlay.
    #[test]
    fn w_cycles_the_stats_heatmap_window() {
        let mut app = remote_app();
        let view = stats_view();
        app.on_key_main(KeyCode::Char('g'), Some(&view));
        assert_eq!(app.overlay, Overlay::Stats);
        assert_eq!(app.stats_window, activity::StatsWindow::Day);
        app.on_key_stats(KeyCode::Char('w'), Some(&view));
        assert_eq!(app.stats_window, activity::StatsWindow::ThreeDay);
        assert_eq!(app.overlay, Overlay::Stats, "w stays in the overlay");
        app.on_key_stats(KeyCode::Char('w'), Some(&view));
        assert_eq!(app.stats_window, activity::StatsWindow::Day, "cycles back");
    }

    /// The Accounts overlay houses the #3/#4 affordances: `a`→AddKey,
    /// `r`→ConfirmRemove, `s`→Select, all entering their own `Mode` over the
    /// overlay (which stays open).
    #[test]
    fn accounts_overlay_houses_add_remove_switch_modes() {
        let view = stats_view_with_account();
        let mut app = remote_app();
        app.overlay = Overlay::Accounts;

        app.on_key_accounts(KeyCode::Char('a'), Some(&view));
        assert_eq!(app.mode, Mode::AddKey);
        assert_eq!(
            app.overlay,
            Overlay::Accounts,
            "overlay stays open over Mode"
        );
        app.on_key_add(KeyCode::Esc); // cancel back to Normal
        assert_eq!(app.mode, Mode::Normal);

        app.on_key_accounts(KeyCode::Char('r'), Some(&view));
        assert_eq!(app.mode, Mode::ConfirmRemove { idx: 0 });
        app.on_key_confirm_remove(KeyCode::Esc, 0, Some(&view));
        assert_eq!(app.mode, Mode::Normal);

        app.on_key_accounts(KeyCode::Char('s'), Some(&view));
        assert_eq!(app.mode, Mode::Select { idx: 0 });
    }

    /// A pending `Mode` interaction takes the key before the overlay handler,
    /// so add/remove/login keep working while Accounts is open (issues #3/#4).
    #[test]
    fn pending_mode_takes_keys_over_the_overlay() {
        let mut app = remote_app();
        app.overlay = Overlay::Accounts;
        app.mode = Mode::AddKey;
        // A printable char goes to the key buffer, not the overlay handler.
        app.on_key(press(KeyCode::Char('x')), None);
        assert_eq!(app.mode, Mode::AddKey);
        assert_eq!(app.add_input, "x");
    }

    #[test]
    fn can_open_browser_gates_only_linux_display_not_ssh() {
        // Regression for the `n` new-login false-"headless": a Mac user inside a
        // tmux session that leaked SSH_* was wrongly blocked. GUI platforms must
        // allow regardless of SSH/display; only Linux gates on a display server.
        assert!(
            can_open_browser_decide(true, false),
            "macOS/Windows must allow the browser even with no DISPLAY / under SSH"
        );
        assert!(
            can_open_browser_decide(true, true),
            "GUI platform with a display still allows"
        );
        assert!(
            can_open_browser_decide(false, true),
            "Linux with a display server allows"
        );
        assert!(
            !can_open_browser_decide(false, false),
            "Linux with no display server is genuinely headless"
        );
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // --- Feature B: mouse click-to-expand ----------------------------------

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Seed `app.activity_chrome` with one clickable request row spanning the
    /// given screen rows, returning its key.
    fn seed_one_hit(app: &mut App) -> activity::ActivityKey {
        let key = activity::ActivityKey {
            at_ms: 42,
            method: "POST".into(),
            path: "/v1/messages".into(),
            status: 200,
        };
        app.activity_chrome = ui::ActivityChrome {
            area: ratatui::layout::Rect {
                x: 0,
                y: 5,
                width: 80,
                height: 10,
            },
            hits: vec![ui::ActivityHit {
                key: key.clone(),
                y_start: 6,
                height: 1,
            }],
        };
        key
    }

    #[test]
    fn left_click_toggles_activity_expand() {
        let mut app = remote_app();
        let key = seed_one_hit(&mut app);
        // Click the row → expands.
        let changed = app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 6), None);
        assert!(changed, "a click on a hit row warrants a redraw");
        assert_eq!(app.expanded_activity.as_ref(), Some(&key));
        // Click it again → collapses (re-click toggles).
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 6), None);
        assert_eq!(app.expanded_activity, None);
    }

    #[test]
    fn click_off_a_row_does_nothing() {
        let mut app = remote_app();
        seed_one_hit(&mut app);
        // Row 9 has no hit target; expansion is untouched and no redraw.
        let changed = app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 9), None);
        assert!(!changed);
        assert_eq!(app.expanded_activity, None);
    }

    #[test]
    fn mouse_is_ignored_while_an_overlay_owns_the_screen() {
        let mut app = remote_app();
        seed_one_hit(&mut app);
        app.overlay = Overlay::Stats;
        let changed = app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 6), None);
        assert!(!changed, "overlay swallows the mouse");
        assert_eq!(app.expanded_activity, None, "no hidden row toggled");
    }

    #[test]
    fn wheel_scrolls_the_activity_history() {
        let mut app = remote_app();
        seed_one_hit(&mut app);
        let mut view = empty_view();
        // Give the view a few completed entries so the scroll offset can move.
        view.completed = (0..5)
            .map(|i| activity::Completed {
                at: SystemTime::UNIX_EPOCH + Duration::from_secs(i),
                body: activity::CompletedBody::Note {
                    text: format!("n{i}"),
                    error: false,
                },
            })
            .collect();
        assert_eq!(app.activity_scroll, 0);
        app.on_mouse(mouse(MouseEventKind::ScrollUp, 5, 6), Some(&view));
        assert_eq!(app.activity_scroll, 1, "wheel up scrolls into history");
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 5, 6), Some(&view));
        assert_eq!(app.activity_scroll, 0, "wheel down returns to the tail");
    }

    fn stats_view() -> DashboardView {
        let mut v = empty_view();
        v.model_usage = vec![crate::dashboard::ModelUsageDoc {
            group: "codex".into(),
            model: "gpt-5.5".into(),
            requests: 1,
            ok: 1,
            errors: 0,
            tokens_in: 10,
            tokens_out: 5,
            cache_read: None,
            cache_creation: None,
            last_used_ms: 0,
            in_flight: 0,
            accounts: Vec::new(),
            efforts: Vec::new(),
            endpoints: Vec::new(),
        }];
        v
    }

    fn stats_view_with_account() -> DashboardView {
        use crate::routing::BackendGroup;
        use crate::scheduler::{AccountId, AccountSnapshot};
        let mut v = stats_view();
        v.snapshot.accounts = vec![AccountSnapshot {
            id: AccountId("claude:me@example.com".into()),
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
        }];
        v
    }

    fn empty_view() -> DashboardView {
        use crate::scheduler::PoolSnapshot;
        DashboardView {
            version: "llmux 0.0 (test)".into(),
            pid: 1,
            uptime: Duration::from_secs(1),
            port: 3456,
            upstream: None,
            config_path: None,
            select_params: select::SelectParams {
                five_hour_max: 0.9,
                seven_day_max: 0.99,
                usage_max_age: Duration::from_secs(600),
            },
            refresh_ahead: Duration::from_secs(25_200),
            evaluate_tick: Duration::from_secs(60),
            snapshot: PoolSnapshot {
                accounts: Vec::new(),
                current: std::collections::BTreeMap::new(),
            },
            last_switch: None,
            poll_health: std::collections::HashMap::new(),
            session_totals: std::collections::HashMap::new(),
            global_totals: activity::Totals::default(),
            rpm_5m: 0.0,
            in_flight: Vec::new(),
            completed: Vec::new(),
            logs: Vec::new(),
            model_usage: Vec::new(),
            client_usage: Vec::new(),
            windowed: Vec::new(),
            codex: crate::dashboard::CodexSettingsDoc::default(),
        }
    }
}
