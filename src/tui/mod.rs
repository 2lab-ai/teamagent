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
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use tokio::sync::mpsc;

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
}

/// Stable identity of a completed activity entry, used to remember which row is
/// expanded (issue #5: click-to-expand) across re-renders and new rows arriving.
///
/// The request `id` would be the natural key, but it is genuinely NOT available
/// on a *completed* entry: the activity fold ([`activity::ActivityLog::apply`])
/// consumes the started→finished `id` only to clear the in-flight row and never
/// stores it on [`activity::Completed`], and the over-the-wire
/// [`crate::dashboard::CompletedDoc::Request`] carries no `id` either. Both of
/// those live in the data/persistence layer, which this change does not touch.
///
/// So the identity is derived from the fields that DO survive into the view:
/// the completion timestamp (millisecond precision through the doc round-trip),
/// plus method/path/status to disambiguate the rare same-millisecond pair.
/// Because new completed entries are *prepended* (newest first) and an existing
/// entry's fields never change, this key is stable while the list grows — the
/// expanded row stays expanded as activity streams in, which a positional index
/// would not survive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ExpandKey {
    /// Completion timestamp in epoch millis (the view stores `at` as a
    /// `SystemTime` built from the doc's `at_ms`).
    pub at_ms: u128,
    pub method: String,
    pub path: String,
    pub status: u16,
}

impl ExpandKey {
    /// Derive the key for a completed *request* entry, or `None` for a note
    /// (notes are not expandable — there is nothing more to show).
    pub(crate) fn for_completed(entry: &activity::Completed) -> Option<Self> {
        let activity::CompletedBody::Request {
            method,
            path,
            status,
            ..
        } = &entry.body
        else {
            return None;
        };
        let at_ms = entry
            .at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Some(Self {
            at_ms,
            method: method.clone(),
            path: path.clone(),
            status: *status,
        })
    }
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
    /// Cursor row in the Stats overlay's model table.
    pub model_cursor: usize,
    /// The completed activity entry currently expanded in place (issue #5:
    /// click-to-expand), if any. Keyed by [`ExpandKey`] so it survives new rows.
    pub expanded: Option<ExpandKey>,
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
    /// Cursor row in the Stats overlay's model table.
    model_cursor: usize,
    /// Completed activity entry expanded in place by a mouse click (issue #5).
    /// Keyed by [`ExpandKey`] (not a list index) so the expansion survives new
    /// completed rows being prepended as activity streams in.
    expanded: Option<ExpandKey>,
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
            model_cursor: 0,
            expanded: None,
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
            model_cursor: self.model_cursor,
            expanded: self.expanded.clone(),
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
        }
    }

    /// Handle one native mouse event (issue #5). Mouse is purely additive to the
    /// keyboard: a left-button press inside the MAIN activity list toggles the
    /// clicked completed row's expanded detail; the scroll wheel pages the
    /// activity log (same axis as `↑↓`). Mouse input is ignored while an overlay
    /// or a pending `Mode` interaction owns the screen, so it never fights the
    /// account/stats/logs surfaces drawn over MAIN.
    fn on_mouse(
        &mut self,
        mouse: MouseEvent,
        view: Option<&DashboardView>,
        area: ratatui::layout::Rect,
    ) {
        // Only MAIN (no overlay, no pending Mode) reacts to the mouse — the
        // activity list lives on MAIN, and the overlays have their own surfaces.
        if self.overlay != Overlay::None || self.mode != Mode::Normal {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.click_activity(mouse.column, mouse.row, view, area);
            }
            // Wheel maps onto the existing activity scroll (up = into history,
            // down = toward the live tail), matching the `↑↓` keys.
            MouseEventKind::ScrollUp => self.scroll_activity(3, view),
            MouseEventKind::ScrollDown => self.scroll_activity(-3, view),
            _ => {}
        }
    }

    /// Resolve a left-click at `(col, row)` to a completed activity entry and
    /// toggle its expanded state. Clicking the already-expanded row collapses
    /// it; clicking a different row moves the expansion. Clicks that miss the
    /// list (or land on an in-flight / note row) are no-ops.
    fn click_activity(
        &mut self,
        col: u16,
        row: u16,
        view: Option<&DashboardView>,
        area: ratatui::layout::Rect,
    ) {
        let Some(view) = view else { return };
        let activity_area = ui::main_activity_area(area, view);
        let layout = ui::activity_layout(
            activity_area,
            view,
            self.expanded.as_ref(),
            self.activity_scroll,
        );
        if let Some(key) = layout.hit(col, row) {
            self.toggle_expanded(key);
        }
    }

    /// Toggle the expanded entry: collapse if `key` is already expanded, else
    /// expand it (replacing any previously expanded row).
    fn toggle_expanded(&mut self, key: ExpandKey) {
        if self.expanded.as_ref() == Some(&key) {
            self.expanded = None;
        } else {
            self.expanded = Some(key);
        }
    }

    /// Key handling for the Stats overlay (`g`). Arrows/`j`/`k` move the cursor
    /// through model rows; `g`/`Esc` closes back to MAIN; `q` quits.
    fn on_key_stats(&mut self, code: KeyCode, view: Option<&DashboardView>) {
        let len = view.map_or(0, |v| v.model_usage.len());
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('g') | KeyCode::Esc => self.overlay = Overlay::None,
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

/// Enter the TUI terminal: raw mode + alternate screen (via `ratatui::try_init`,
/// which also installs a panic hook that restores those) PLUS native mouse
/// capture (issue #5). The panic hook installed here CHAINS the existing one so
/// mouse capture is also disabled on an unwinding panic — `ratatui::restore`
/// alone does not turn it off, which would leave the user's real terminal
/// emitting mouse escape codes after a crash.
fn init_terminal() -> std::io::Result<ratatui::DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort: undo mouse capture before the (ratatui) hook restores
        // raw mode / the alternate screen and prints the panic.
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        prev(info);
    }));
    Ok(terminal)
}

/// Leave the TUI terminal: disable mouse capture, then `ratatui::restore` (raw
/// mode + alternate screen). Safe to call on every exit path; mirrors
/// [`init_terminal`].
fn restore_terminal() {
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

/// Run the in-process dashboard over live server state until quit.
///
/// Terminal lifecycle: [`init_terminal`] enables raw mode + the alternate
/// screen + mouse capture AND installs a panic hook that restores the terminal
/// before unwinding; [`restore_terminal`] runs on every exit path.
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
    let mut input = tokio::time::interval(INPUT_TICK);
    input.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let mut redraw = tokio::select! {
            _ = render.tick() => {
                app.frame = app.frame.wrapping_add(1);
                true
            }
            _ = input.tick() => drain_input(app, terminal)?,
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
            terminal.draw(|frame| ui::draw(frame, view.as_ref(), &chrome))?;
        }
    }
}

/// Drain every pending terminal event without blocking the runtime
/// (`poll(ZERO)` is a non-blocking readiness check). Returns whether
/// anything happened that warrants a redraw.
///
/// `terminal` is borrowed only to read the current size, so a mouse click can
/// be hit-tested against the same frame rect the renderer lays MAIN out in.
fn drain_input(app: &mut App, terminal: &ratatui::DefaultTerminal) -> std::io::Result<bool> {
    let mut dirty = false;
    // Built once per drain: key/mouse handlers read the same frame the user saw.
    let mut view: Option<Option<DashboardView>> = None;
    while crossterm::event::poll(Duration::ZERO)? {
        match crossterm::event::read()? {
            Event::Key(key) => {
                let view = view.get_or_insert_with(|| app.view(SystemTime::now()));
                app.on_key(key, view.as_ref());
                dirty = true;
            }
            Event::Mouse(mouse) => {
                // The frame rect is the whole terminal; mouse coords are
                // absolute, so MAIN's layout is recomputed against this rect to
                // map the click to an activity row.
                let area = ratatui::layout::Rect::new(
                    0,
                    0,
                    terminal.size()?.width,
                    terminal.size()?.height,
                );
                let view = view.get_or_insert_with(|| app.view(SystemTime::now()));
                app.on_mouse(mouse, view.as_ref(), area);
                dirty = true;
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
    Ok(dirty)
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

    // --- issue #5: native mouse + click-to-expand --------------------------

    fn completed_request(at_secs: u64, account: &str) -> activity::Completed {
        activity::Completed {
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(at_secs),
            body: activity::CompletedBody::Request {
                method: "POST".into(),
                path: "/v1/messages".into(),
                account: Some(account.into()),
                status: 200,
                duration: Duration::from_millis(1_400),
                tokens: None,
                group: Some("codex".into()),
                model: Some("gpt-5.5".into()),
                effort: None,
            },
        }
    }

    fn left_click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn expand_key_distinguishes_completed_requests_and_skips_notes() {
        let a = completed_request(10, "a");
        let b = completed_request(20, "b");
        let key_a = ExpandKey::for_completed(&a).expect("request has a key");
        let key_b = ExpandKey::for_completed(&b).expect("request has a key");
        assert_ne!(key_a, key_b, "different entries → different keys");
        // Same entry → same key (stable identity).
        assert_eq!(key_a, ExpandKey::for_completed(&a).unwrap());
        // Notes are not expandable.
        let note = activity::Completed {
            at: SystemTime::UNIX_EPOCH,
            body: activity::CompletedBody::Note {
                text: "switch".into(),
                error: false,
            },
        };
        assert_eq!(ExpandKey::for_completed(&note), None);
    }

    #[test]
    fn toggle_expanded_is_idempotent_open_close() {
        let mut app = remote_app();
        let key = ExpandKey::for_completed(&completed_request(10, "a")).unwrap();
        assert_eq!(app.expanded, None);
        app.toggle_expanded(key.clone());
        assert_eq!(app.expanded.as_ref(), Some(&key), "first click expands");
        app.toggle_expanded(key.clone());
        assert_eq!(app.expanded, None, "second click on the same row collapses");
    }

    #[test]
    fn clicking_a_different_row_moves_the_expansion() {
        let mut app = remote_app();
        let a = ExpandKey::for_completed(&completed_request(10, "a")).unwrap();
        let b = ExpandKey::for_completed(&completed_request(20, "b")).unwrap();
        app.toggle_expanded(a.clone());
        app.toggle_expanded(b.clone());
        assert_eq!(app.expanded.as_ref(), Some(&b), "expansion moves to b");
    }

    #[test]
    fn click_activity_expands_the_row_under_the_cursor() {
        let mut app = remote_app();
        let mut view = empty_view();
        view.completed = vec![completed_request(30, "a"), completed_request(20, "b")];
        // A frame big enough that MAIN gives the activity panel real rows.
        let area = ratatui::layout::Rect::new(0, 0, 120, 24);
        let activity_area = ui::main_activity_area(area, &view);
        // First content row (one below the top border) → newest entry "a".
        let y = activity_area.y + 1;
        app.click_activity(activity_area.x + 2, y, Some(&view), area);
        assert_eq!(
            app.expanded,
            ExpandKey::for_completed(&view.completed[0]),
            "click on row 0 expands the newest entry"
        );
        // Click it again → collapse.
        app.click_activity(activity_area.x + 2, y, Some(&view), area);
        assert_eq!(app.expanded, None, "re-click collapses");
    }

    #[test]
    fn mouse_is_ignored_while_an_overlay_is_open() {
        let mut app = remote_app();
        let mut view = empty_view();
        view.completed = vec![completed_request(10, "a")];
        app.overlay = Overlay::Accounts;
        let area = ratatui::layout::Rect::new(0, 0, 120, 24);
        let activity_area = ui::main_activity_area(area, &view);
        app.on_mouse(
            left_click(activity_area.x + 2, activity_area.y + 1),
            Some(&view),
            area,
        );
        assert_eq!(app.expanded, None, "overlay swallows the click, no expand");
    }

    #[test]
    fn scroll_wheel_pages_the_activity_log() {
        let mut app = remote_app();
        let mut view = empty_view();
        view.completed = (0..10).map(|i| completed_request(i, "a")).collect();
        let area = ratatui::layout::Rect::new(0, 0, 120, 24);
        let up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(up, Some(&view), area);
        assert!(app.activity_scroll > 0, "wheel up scrolls into history");
        let before = app.activity_scroll;
        let down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(down, Some(&view), area);
        assert!(
            app.activity_scroll < before,
            "wheel down scrolls toward tail"
        );
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
            codex: crate::dashboard::CodexSettingsDoc::default(),
        }
    }
}
