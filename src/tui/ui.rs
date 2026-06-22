//! Frame rendering: header (identity/port/uptime/attach banner), account
//! table in selection order, scheduler/poller/totals pane + selected-account
//! detail, activity log, log console, footer keybar. Pure projection of a
//! [`DashboardView`] (data) + [`Chrome`] (UI-local cursor/panes/status) — no
//! state mutation here, and no knowledge of where the view came from (local
//! `AppState` or a fetched document), so the renderer is never forked.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use ratatui::Frame;

use crate::dashboard::ModelUsageDoc;
use crate::logging::LogLine;
use crate::routing::BackendGroup;
use crate::scheduler::select::IneligibleReason;
use crate::scheduler::window::{classify_window_display, QuotaWindow, WindowDisplayState};
use crate::scheduler::{select, AccountSnapshot};

use super::activity::{ActivityKey, Completed, CompletedBody};
use super::format::{self, GaugeLevel};
use super::view::DashboardView;
use super::{anim, Chrome, Mode, Overlay};

const GAUGE_BAR_WIDTH: usize = 8;
/// Width at/above which the accounts table shows the wide column set
/// (type, absolute reset times, lifetime req/tok).
const WIDE_TABLE_AT: u16 = 150;
/// Width at/above which the middle row fits summary + detail side by side.
const SIDE_BY_SIDE_AT: u16 = 110;
/// Rows shown in the always-visible compact model strip (req12). Width of its
/// token-share mini-bar.
const MODEL_STRIP_ROWS: usize = 3;
const MODEL_BAR_WIDTH: usize = 10;
/// Rows shown in the compact per-client attribution panel in the stats overlay
/// (issue #32) — the top N clients by request count.
const CLIENT_PANEL_ROWS: usize = 6;
/// A model used within this window counts as "recently active" (req15).
const MODEL_RECENT_WINDOW: Duration = Duration::from_secs(60);
/// Max heatmap cells shown at once (issue #23). The rows are sorted by tokens
/// desc, so the busiest (group, model, account) cells stay visible; the panel
/// title reports the total when more exist.
const HEATMAP_MAX_ROWS: usize = 8;
/// Width of the heatmap's token-intensity mini-bar.
const HEATMAP_BAR_WIDTH: usize = 12;

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn level_color(level: GaugeLevel) -> Color {
    match level {
        GaugeLevel::Green => Color::Green,
        GaugeLevel::Yellow => Color::Yellow,
        GaugeLevel::Red => Color::Red,
    }
}

/// Format an API-equivalent USD cost (Feature D) for display: `≥$1` keeps two
/// decimals (`$3.78`), a sub-dollar amount keeps four (`$0.0123`) so small
/// per-request costs are still legible, and exactly zero renders `$0.0000`.
fn format_cost(usd: f64) -> String {
    if usd == 0.0 {
        "$0.0000".to_string()
    } else if usd >= 1.0 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.4}")
    }
}

/// Everything a row/pane needs that is derived once per frame.
struct FrameCtx {
    now: SystemTime,
    /// Local UTC offset for absolute time labels.
    tz_offset: i64,
    /// Indices into `view.snapshot.accounts` in scheduler preference order.
    order: Vec<usize>,
    headers_only: bool,
    /// Monotonic animation frame counter (drives `anim` glyphs).
    frame: usize,
}

/// Top-level draw entry. `view` is `None` only in attach mode before the
/// first document arrives — then we paint a connecting screen + the footer,
/// never a half-rendered table.
pub(crate) fn draw(
    frame: &mut Frame,
    view: Option<&DashboardView>,
    chrome: &Chrome,
    hits: &mut Option<ActivityChrome>,
) {
    // No activity panel hit-targets until MAIN draws one this frame (cleared so
    // a stale layout from a previous frame can never mis-map a click).
    *hits = None;
    let Some(view) = view else {
        draw_connecting(frame, chrome);
        return;
    };

    let now = SystemTime::now();
    let ctx = FrameCtx {
        now,
        tz_offset: format::local_offset_secs(now),
        order: view.display_order(now),
        headers_only: select::headers_only_mode(&view.snapshot, &view.select_params, None, now),
        frame: chrome.frame,
    };

    // MAIN is the wall-clock view: ALWAYS drawn first, every frame, so it keeps
    // updating underneath any overlay (issue #5). Local and attach render from
    // the same `DashboardView`, so this path is never forked.
    draw_main(frame, view, &ctx, chrome, now, hits);

    // A summoned overlay (if any) is then drawn OVER MAIN. Each overlay clears
    // its own rect with `Clear` so MAIN shows through only outside it; because
    // MAIN was already drawn this frame, "MAIN keeps updating underneath" is
    // automatic.
    match chrome.overlay {
        Overlay::None => {}
        Overlay::Accounts => draw_accounts_overlay(frame, view, &ctx, chrome),
        Overlay::Stats => draw_stats_overlay(frame, view, &ctx, chrome),
        Overlay::Logs => draw_logs_overlay(frame, view),
        Overlay::Sessions => draw_sessions_overlay(frame, &ctx, chrome),
    }

    // The footer keybar is part of the chrome and reflects the active overlay /
    // mode; drawn last so it sits above everything.
    let footer_area = Rect {
        x: frame.area().x,
        y: frame.area().bottom().saturating_sub(2),
        width: frame.area().width,
        height: 2,
    };
    frame.render_widget(Clear, footer_area);
    draw_footer(frame, footer_area, chrome);
}

/// MAIN — the always-rendered wall-clock view (issue #5): header banner ·
/// account quota table · scheduler/totals summary · compact per-model strip ·
/// in-flight + activity. No navigation, no overlay surfaces. The selected-
/// account detail pane and the full log console moved to the Accounts and Logs
/// overlays respectively; the model strip stays here.
fn draw_main(
    frame: &mut Frame,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
    now: SystemTime,
    hits: &mut Option<ActivityChrome>,
) {
    let snapshot = &view.snapshot;
    let table_height = (snapshot.accounts.len().max(1) as u16).saturating_add(2);
    // Compact model strip (req12): only when model data exists. 0 height (no
    // pane) otherwise, so the idle layout is unchanged.
    let strip_rows = view.model_usage.len().min(MODEL_STRIP_ROWS);
    // +2 for the table's top border (title) and header row.
    let strip_height = if strip_rows > 0 {
        strip_rows as u16 + 2
    } else {
        0
    };
    let [header_area, table_area, middle_area, strip_area, activity_area, footer_area] =
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(table_height),
            Constraint::Length(8),
            Constraint::Length(strip_height),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .areas(frame.area());

    draw_header(frame, header_area, view, chrome);
    draw_accounts(frame, table_area, view, ctx, chrome);
    draw_middle(frame, middle_area, view, ctx, chrome);
    if strip_height > 0 {
        draw_models_strip(frame, strip_area, view, now);
    }
    *hits = Some(draw_activity(frame, activity_area, view, chrome, now));
    // Footer slot reserved in the layout; the real footer is drawn by `draw`
    // last (over any overlay). Keep MAIN's bottom row clear here.
    let _ = footer_area;
}

/// Accounts overlay (`a`): a near-full-screen surface giving the account quota
/// table the priority slot plus the selected-account detail pane, over which
/// the add/remove/switch/login interactions (issues #3/#4) run. Cleared so MAIN
/// shows through only at the very edges.
fn draw_accounts_overlay(frame: &mut Frame, view: &DashboardView, ctx: &FrameCtx, chrome: &Chrome) {
    let area = overlay_rect(frame.area());
    frame.render_widget(Clear, area);
    let snapshot = &view.snapshot;
    let table_height = (snapshot.accounts.len().max(1) as u16).saturating_add(2);
    let [header_area, table_area, detail_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(table_height),
        Constraint::Min(3),
    ])
    .areas(area);
    let title = Paragraph::new(Line::from(Span::styled(
        " accounts ",
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(title, header_area);
    draw_accounts(frame, table_area, view, ctx, chrome);
    if snapshot.accounts.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "no accounts — press a to add an API key, n to start a browser login",
            Style::new().fg(Color::Yellow),
        )))
        .block(Block::new().borders(Borders::TOP).title(" detail "));
        frame.render_widget(empty, detail_area);
    } else {
        draw_detail(frame, detail_area, view, ctx, chrome);
    }
}

/// Stats overlay (`g`): the detailed per-model usage table + drill-down (req13;
/// was the `show_models` full view). Keeps the account quota table above it for
/// context, matching the old layout.
fn draw_stats_overlay(frame: &mut Frame, view: &DashboardView, ctx: &FrameCtx, chrome: &Chrome) {
    let area = overlay_rect(frame.area());
    frame.render_widget(Clear, area);
    let snapshot = &view.snapshot;
    let table_height = (snapshot.accounts.len().max(1) as u16).saturating_add(2);
    // Reserve a bottom slice for the windowed (24h/72h) per-account/per-model
    // token heatmap (issue #23). The heatmap height tracks the visible cells,
    // capped so the model table/drill-down above always keep room.
    let heatmap_height = heatmap_panel_height(view, chrome.stats_window, area.height);
    let [table_area, body_area, heatmap_area] = Layout::vertical([
        Constraint::Length(table_height),
        Constraint::Min(3),
        Constraint::Length(heatmap_height),
    ])
    .areas(area);
    draw_accounts(frame, table_area, view, ctx, chrome);
    // Reserve a compact per-client attribution panel (issue #32) at the bottom
    // of the stats body when there is client usage to show; otherwise the
    // models view keeps the whole body. The windowed heatmap (issue #23) always
    // renders in its own reserved slice below.
    if view.client_usage.is_empty() {
        draw_models_full(frame, body_area, view, ctx, chrome);
    } else {
        let clients_height = (view.client_usage.len().min(CLIENT_PANEL_ROWS) as u16)
            .saturating_add(2)
            .min(body_area.height.saturating_sub(3).max(2));
        let [models_area, clients_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(clients_height)])
                .areas(body_area);
        draw_models_full(frame, models_area, view, ctx, chrome);
        draw_clients_compact(frame, clients_area, view);
    }
    draw_heatmap(frame, heatmap_area, view, chrome.stats_window);
}

/// Compact per-client request-attribution table (issue #32): top
/// [`CLIENT_PANEL_ROWS`] clients by request count, keyed by `metadata.user_id`
/// (the `unknown` bucket holds requests with no id). In-memory metering only —
/// not a credential, never gates a request.
fn draw_clients_compact(frame: &mut Frame, area: Rect, view: &DashboardView) {
    let total = view.client_usage.len();
    let header = ["client", "req", "ok/err", "in", "out"];
    let rows = view
        .client_usage
        .iter()
        .take(CLIENT_PANEL_ROWS)
        .map(|c| {
            let ok_err = Line::from(vec![
                Span::styled(format::human_count(c.ok), Style::new().fg(Color::Green)),
                Span::raw("/"),
                Span::styled(
                    format::human_count(c.errors),
                    if c.errors > 0 {
                        Style::new().fg(Color::Red)
                    } else {
                        dim()
                    },
                ),
            ]);
            Row::new(vec![
                Cell::from(c.client.clone()),
                Cell::from(format::human_count(c.requests)),
                Cell::from(ok_err),
                Cell::from(format::human_count(c.tokens_in)),
                Cell::from(format::human_count(c.tokens_out)),
            ])
        })
        .collect::<Vec<_>>();
    let constraints = [
        Constraint::Fill(1),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(9),
    ];
    let shown = total.min(CLIENT_PANEL_ROWS);
    let title = format!(" clients — top {shown} of {total} by requests (metadata.user_id) ");
    let table = Table::new(rows, constraints)
        .header(Row::new(header).style(dim().add_modifier(Modifier::BOLD)))
        .block(Block::new().borders(Borders::TOP).title(title));
    frame.render_widget(table, area);
}

/// Rows the windowed heatmap panel needs: a title/header pair + one row per
/// visible cell, capped at [`HEATMAP_MAX_ROWS`], plus the top border. Returns 0
/// when the panel would not fit (tiny terminal) so the model view keeps the
/// space.
fn heatmap_panel_height(
    view: &DashboardView,
    window: super::activity::StatsWindow,
    total: u16,
) -> u16 {
    let cells = heatmap_cells(view, window).len().min(HEATMAP_MAX_ROWS);
    // border(1) + best-effort line(1) + header(1) + cells (≥1 for the "no
    // activity" / first row), then never starve the model view above it.
    let want = 3 + cells.max(1) as u16;
    want.min(total.saturating_sub(8))
}

/// Logs overlay (`l`): a full-screen log tail (was the `l` size-cycle panel).
fn draw_logs_overlay(frame: &mut Frame, view: &DashboardView) {
    let area = overlay_rect(frame.area());
    frame.render_widget(Clear, area);
    draw_logs(frame, area, view);
}

/// Sessions overlay (`s`, issue #34): the persisted raw-io log folded by
/// `metadata.user_id` into a confidence-labeled session timeline, above a
/// per-session detail pane for the cursored row. Renders from the snapshot held
/// on `Chrome` (taken when the overlay opened) — metadata only, no prompt
/// content. On a side-by-side width the detail sits beside the list; otherwise
/// the list takes the whole rect.
fn draw_sessions_overlay(frame: &mut Frame, ctx: &FrameCtx, chrome: &Chrome) {
    let area = overlay_rect(frame.area());
    frame.render_widget(Clear, area);
    if chrome.sessions.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "no sessions yet — enable raw-io capture and send requests through the proxy",
            Style::new().fg(Color::Yellow),
        )))
        .block(Block::new().borders(Borders::TOP).title(" sessions "));
        frame.render_widget(empty, area);
        return;
    }
    if area.width >= SIDE_BY_SIDE_AT {
        let [list_area, detail_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(46)]).areas(area);
        draw_sessions_table(frame, list_area, ctx, chrome);
        draw_session_detail(frame, detail_area, ctx, chrome);
    } else {
        draw_sessions_table(frame, area, ctx, chrome);
    }
}

/// The session timeline table. Columns: confidence label, user_id, request
/// count, tokens in/out, distinct models, distinct accounts + rotation count,
/// and the wall-clock span. The cursored row is highlighted; the title shows the
/// cursor position so it is obvious more rows exist off-screen.
fn draw_sessions_table(frame: &mut Frame, area: Rect, ctx: &FrameCtx, chrome: &Chrome) {
    let total = chrome.sessions.len();
    let cursor = chrome.session_cursor.min(total.saturating_sub(1));
    let capacity = (area.height.saturating_sub(2) as usize).max(1); // border + header
    let start = if cursor >= capacity {
        cursor + 1 - capacity
    } else {
        0
    };
    let end = (start + capacity).min(total);

    let rows = chrome.sessions[start..end]
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let idx = start + i;
            let conf_color = match s.confidence {
                crate::session::Confidence::High => Color::Green,
                crate::session::Confidence::Low => Color::DarkGray,
            };
            let cells = vec![
                Cell::from(Span::styled(
                    s.confidence.label(),
                    Style::new().fg(conf_color),
                )),
                Cell::from(session_id_label(s)),
                Cell::from(format::human_count(s.requests)),
                Cell::from(format::human_count(s.tokens_in)),
                Cell::from(format::human_count(s.tokens_out)),
                Cell::from(format::human_count(s.models.len() as u64)),
                Cell::from(session_accounts_label(s)),
                Cell::from(Span::styled(session_span_label(s), dim())),
            ];
            let row = Row::new(cells);
            if idx == cursor {
                row.style(Style::new().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        });

    let header = vec!["conf", "session", "req", "in", "out", "mdl", "acct", "span"];
    let constraints = vec![
        Constraint::Length(5),
        Constraint::Fill(1),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Length(9),
    ];
    let title = format!(" sessions — {} of {total} ", cursor + 1);
    let table = Table::new(rows, constraints)
        .header(Row::new(header).style(dim().add_modifier(Modifier::BOLD)))
        .block(Block::new().borders(Borders::TOP).title(title));
    frame.render_widget(table, area);
    let _ = ctx;
}

/// Drill-down pane for the cursored session: the grouping confidence, the full
/// user_id, the model list, the account list + rotation count, the token split,
/// and the absolute time span. Metadata only.
fn draw_session_detail(frame: &mut Frame, area: Rect, ctx: &FrameCtx, chrome: &Chrome) {
    let cursor = chrome
        .session_cursor
        .min(chrome.sessions.len().saturating_sub(1));
    let Some(s) = chrome.sessions.get(cursor) else {
        return;
    };
    let models = if s.models.is_empty() {
        "—".to_string()
    } else {
        s.models.join(", ")
    };
    let accounts = if s.accounts.is_empty() {
        "—".to_string()
    } else {
        s.accounts.join(", ")
    };
    let first = format::absolute_label(ms_to_systemtime(s.first_ms), ctx.now, ctx.tz_offset);
    let last = format::absolute_label(ms_to_systemtime(s.last_ms), ctx.now, ctx.tz_offset);
    let lines = vec![
        Line::from(vec![
            Span::styled("confidence  ", dim()),
            Span::styled(
                s.confidence.label(),
                match s.confidence {
                    crate::session::Confidence::High => Style::new().fg(Color::Green),
                    crate::session::Confidence::Low => dim(),
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("user_id     ", dim()),
            Span::raw(s.user_id.clone().unwrap_or_else(|| "(ungrouped)".into())),
        ]),
        Line::from(vec![
            Span::styled("requests    ", dim()),
            Span::raw(format::human_count(s.requests)),
        ]),
        Line::from(vec![
            Span::styled("tokens      ", dim()),
            Span::raw(format!(
                "{} in / {} out",
                format::human_count(s.tokens_in),
                format::human_count(s.tokens_out)
            )),
        ]),
        Line::from(vec![Span::styled("models      ", dim()), Span::raw(models)]),
        Line::from(vec![
            Span::styled("accounts    ", dim()),
            Span::raw(accounts),
        ]),
        Line::from(vec![
            Span::styled("rotations   ", dim()),
            Span::raw(s.account_rotations.to_string()),
        ]),
        Line::from(vec![Span::styled("first       ", dim()), Span::raw(first)]),
        Line::from(vec![Span::styled("last        ", dim()), Span::raw(last)]),
        Line::from(vec![
            Span::styled("span        ", dim()),
            Span::raw(session_span_label(s)),
        ]),
    ];
    let detail = Paragraph::new(lines).block(Block::new().borders(Borders::TOP).title(" session "));
    frame.render_widget(detail, area);
}

/// Display label for a session's grouping key: the user_id, or `(ungrouped)` for
/// the catch-all bucket of records with no `metadata.user_id`.
fn session_id_label(s: &crate::session::Session) -> String {
    s.user_id.clone().unwrap_or_else(|| "(ungrouped)".into())
}

/// `acct ×N` where N is the distinct-account count; rotations are shown in the
/// detail pane. A single account drops the multiplier.
fn session_accounts_label(s: &crate::session::Session) -> String {
    let n = s.accounts.len();
    if n <= 1 {
        n.to_string()
    } else {
        format!("{n} ×{}", s.account_rotations)
    }
}

/// Wall-clock span of a session as a coarse duration ("3m 04s", "2h 11m").
fn session_span_label(s: &crate::session::Session) -> String {
    format::countdown(Duration::from_millis(s.span_ms()))
}

/// Millis-since-epoch → `SystemTime` for the absolute-time labels. Pure; a value
/// that would overflow saturates to the epoch (never panics).
fn ms_to_systemtime(ms: u64) -> SystemTime {
    UNIX_EPOCH
        .checked_add(Duration::from_millis(ms))
        .unwrap_or(UNIX_EPOCH)
}

/// The rect a summoned overlay covers: the whole screen except the bottom two
/// rows reserved for the footer keybar, so MAIN's footer slot is never double
/// drawn and the keybar stays visible under the overlay.
fn overlay_rect(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(2),
    }
}

/// Attach-mode pre-first-document screen: identity + a "connecting…" /
/// reconnect line, plus the footer so `q` is discoverable.
fn draw_connecting(frame: &mut Frame, chrome: &Chrome) {
    let [header_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let mut header = vec![
        Span::styled(
            " llmux ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(crate::build_info::version_with_build(), dim()),
    ];
    header.extend(attach_spans(chrome));
    frame.render_widget(Paragraph::new(Line::from(header)), header_area);

    let connecting = match chrome.attach {
        Some(attach) if attach.connected => "connecting — waiting for the first document…",
        _ => "connecting to daemon — retrying…",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {connecting}"),
            Style::new().fg(Color::Yellow),
        ))),
        body_area,
    );
    draw_footer(frame, footer_area, chrome);
}

/// Attach-mode header markers: `attached → pid N` (or `pid ?`), turning into a
/// red `reconnecting…` while the poller cannot reach the daemon. Empty in
/// local mode.
fn attach_spans(chrome: &Chrome) -> Vec<Span<'static>> {
    let Some(attach) = chrome.attach else {
        return Vec::new();
    };
    let pid = attach
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".into());
    if attach.connected {
        vec![Span::styled(
            format!(" attached → pid {pid} "),
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        )]
    } else {
        vec![Span::styled(
            format!(" reconnecting → pid {pid}… "),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]
    }
}

fn draw_header(frame: &mut Frame, area: Rect, view: &DashboardView, chrome: &Chrome) {
    let mut spans = vec![
        Span::styled(
            " llmux ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(view.display_version().to_string(), dim()),
        Span::raw(format!("  port {} ", view.port)),
        Span::styled(format!(" pid {} ", view.pid), dim()),
        Span::styled(format!(" up {} ", format::countdown(view.uptime)), dim()),
        Span::styled(
            format!(" {} account(s) ", view.snapshot.accounts.len()),
            dim(),
        ),
    ];
    if let Some(upstream) = &view.upstream {
        spans.push(Span::styled(format!(" → {upstream} "), dim()));
    }
    if let Some(path) = &view.config_path {
        spans.push(Span::styled(format!(" cfg {path} "), dim()));
    }
    spans.extend(attach_spans(chrome));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_accounts(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
) {
    let snapshot = &view.snapshot;
    let block = Block::new().borders(Borders::TOP).title(" accounts ");
    if snapshot.accounts.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "no accounts — run `llmux login` or `llmux import`, then press R",
            Style::new().fg(Color::Yellow),
        )))
        .block(block);
        frame.render_widget(empty, area);
        return;
    }
    let wide = area.width >= WIDE_TABLE_AT;

    let selected = match chrome.mode {
        Mode::Select { idx } | Mode::ConfirmRemove { idx } => {
            Some(idx.min(ctx.order.len().saturating_sub(1)))
        }
        // NewLogin is a provider picker, not an account-row cursor.
        Mode::Normal | Mode::AddKey | Mode::NewLogin { .. } => None,
    };
    let rows = ctx.order.iter().enumerate().map(|(pos, &account_idx)| {
        let account = &snapshot.accounts[account_idx];
        let cursor = selected == Some(pos);
        let row = account_row(account, view, ctx, pos, wide, cursor);
        if cursor {
            row.style(Style::new().add_modifier(Modifier::REVERSED))
        } else {
            row
        }
    });

    // "group" (claude/codex — the model group, colored + prominent) leads the
    // data columns; "auth" (oauth/api) is the credential type, separated out
    // (req5). Both appear in narrow mode too — group is load-bearing.
    let (header, constraints): (Vec<&'static str>, Vec<Constraint>) = if wide {
        (
            vec![
                "", "group", "#", "account", "auth", "status", "5h", "reset", "7d", "reset",
                "token", "if", "req", "tok",
            ],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Length(2),
                Constraint::Fill(1),
                Constraint::Length(6),
                Constraint::Length(20),
                Constraint::Length((GAUGE_BAR_WIDTH + 8) as u16),
                Constraint::Length(19),
                Constraint::Length((GAUGE_BAR_WIDTH + 8) as u16),
                Constraint::Length(19),
                // "23h59m ↻59m" / "expired ↻12h" — expiry + refreshed-ago.
                Constraint::Length(13),
                Constraint::Length(3),
                Constraint::Length(6),
                Constraint::Length(7),
            ],
        )
    } else {
        (
            vec![
                "", "group", "#", "account", "status", "5h", "reset", "7d", "reset", "token", "if",
            ],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Length(2),
                Constraint::Fill(1),
                Constraint::Length(20),
                Constraint::Length((GAUGE_BAR_WIDTH + 8) as u16),
                Constraint::Length(7),
                Constraint::Length((GAUGE_BAR_WIDTH + 8) as u16),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(3),
            ],
        )
    };

    let table = Table::new(rows, constraints)
        .header(Row::new(header).style(dim().add_modifier(Modifier::BOLD)))
        .block(block);
    frame.render_widget(table, area);
}

#[allow(clippy::too_many_arguments)]
fn account_row<'a>(
    account: &'a AccountSnapshot,
    view: &'a DashboardView,
    ctx: &FrameCtx,
    pos: usize,
    wide: bool,
    cursor: bool,
) -> Row<'a> {
    let snapshot = &view.snapshot;
    let params = &view.select_params;
    let now = ctx.now;
    let is_current = snapshot.is_current(&account.id);
    let gate = select::eligibility(account, params, now, ctx.headers_only);

    let marker = match (cursor, is_current) {
        (true, _) => Span::styled(">", Style::new().fg(Color::Cyan)),
        (false, true) => Span::styled("►", Style::new().fg(Color::Green)),
        (false, false) => Span::raw(" "),
    };
    let name = if is_current {
        Span::styled(
            account.id.to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(account.id.to_string())
    };
    let parked = matches!(gate, Some(IneligibleReason::CoolingDown));
    // Poller-health overlay (issue #33): a failing usage poll makes every
    // window's value suspect, so it surfaces as a distinct display state rather
    // than collapsing to a plain percent/—. Codex has no usage poller, so it
    // never reads as poll-degraded.
    let consecutive_failures = view
        .poll_health
        .get(&account.id.0)
        .map_or(0, |h| h.consecutive_failures);
    let max_age = params.usage_max_age;
    let (five_gauge, five_reset) = window_cells(
        &account.five_hour,
        params.five_hour_max,
        parked,
        now,
        ctx.tz_offset,
        wide,
        max_age,
        consecutive_failures,
    );
    let (seven_gauge, seven_reset) = window_cells(
        &account.seven_day,
        params.seven_day_max,
        parked,
        now,
        ctx.tz_offset,
        wide,
        max_age,
        consecutive_failures,
    );
    let totals = view.totals_for(&account.id.0);

    let group_label = account.group.as_str();
    let mut cells = vec![
        Cell::from(marker),
        Cell::from(Span::styled(
            group_label.to_uppercase(),
            group_color(Some(group_label)).add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(format!("{}", pos + 1), dim())),
        Cell::from(name),
    ];
    if wide {
        cells.push(Cell::from(Span::styled(
            auth_type(account.credential_kind),
            dim(),
        )));
    }
    cells.push(Cell::from(status_span(
        account, gate, is_current, params, now, ctx.frame,
    )));
    cells.extend([
        five_gauge,
        five_reset,
        seven_gauge,
        seven_reset,
        Cell::from(token_health_line(account, view.refresh_ahead, now, wide)),
        Cell::from(in_flight_span(account.in_flight)),
    ]);
    if wide {
        cells.push(Cell::from(format::human_count(totals.requests)));
        cells.push(Cell::from(format::human_count(totals.tokens())));
    }
    Row::new(cells)
}

/// Status column: active (green) / ready (default) / the concrete blocking
/// reason from the scheduler's own gate ("cooldown 3m12s", "7d 99.4% > 99%",
/// "usage stale 14m", "auth failed") so the TUI never disagrees with the
/// selector about WHY an account is parked.
fn status_span(
    account: &AccountSnapshot,
    gate: Option<IneligibleReason>,
    is_current: bool,
    params: &select::SelectParams,
    now: SystemTime,
    frame: usize,
) -> Span<'static> {
    let Some(reason) = gate else {
        // Eligible. The current account is "active" — a braille working
        // spinner while it has in-flight traffic, otherwise a calm bar
        // heartbeat. Other eligible accounts get a faint "ready" drift.
        return if is_current {
            let glyph = if account.in_flight > 0 {
                anim::braille_spin(frame)
            } else {
                anim::bar_pulse(frame)
            };
            Span::styled(
                format!("{glyph} active"),
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!("{} ready", anim::idle_drift(frame)), dim())
        };
    };
    let text = select::blocking_reason(account, reason, params, now);
    // Each blocked state gets its own animated glyph so the WHY reads at a
    // glance: blinking alert (auth), shade filling up (over quota), a rotating
    // timer (cooldown), a faint drift (stale data).
    let (glyph, style) = match reason {
        IneligibleReason::AuthUnhealthy => (
            anim::blink(frame, '!'),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        IneligibleReason::FiveHourOverThreshold | IneligibleReason::SevenDayOverThreshold => {
            (anim::shade_breathe(frame), Style::new().fg(Color::Red))
        }
        IneligibleReason::CoolingDown => (
            anim::half_block_clock(frame),
            Style::new().fg(Color::Yellow),
        ),
        IneligibleReason::UsageStale => (anim::idle_drift(frame), dim()),
    };
    Span::styled(format!("{glyph} {text}"), style)
}

/// Full token cell: expiry countdown (with due/expired coloring) plus, in
/// wide mode, the dim "refreshed N ago" marker — "6h52m ↻3m". Narrow mode
/// keeps just the expiry countdown.
fn token_health_line(
    account: &AccountSnapshot,
    refresh_ahead: Duration,
    now: SystemTime,
    wide: bool,
) -> Line<'static> {
    let mut spans = vec![token_health_span(account, refresh_ahead, now)];
    if wide && account.token_expires_at_ms.is_some() {
        if let Some(marker) = format::refreshed_marker(account.last_refresh_ms, now) {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(marker, dim()));
        }
    }
    Line::from(spans)
}

/// OAuth token health: expires-in countdown, yellow + `↻` once the
/// background refresher is due, red when already expired. API-key accounts
/// have no token to expire.
fn token_health_span(
    account: &AccountSnapshot,
    refresh_ahead: Duration,
    now: SystemTime,
) -> Span<'static> {
    let Some(expires_ms) = account.token_expires_at_ms else {
        return Span::styled("—", dim());
    };
    let expires_at = UNIX_EPOCH + Duration::from_millis(expires_ms);
    match expires_at.duration_since(now) {
        Ok(left) if left > refresh_ahead => Span::raw(select::compact_duration(left)),
        // Refresh window reached: the background refresher should rotate it
        // on its next tick — flag it instead of silently counting down.
        Ok(left) => Span::styled(
            format!("{}↻", select::compact_duration(left)),
            Style::new().fg(Color::Yellow),
        ),
        Err(_) => Span::styled("expired", Style::new().fg(Color::Red)),
    }
}

fn in_flight_span(in_flight: u32) -> Span<'static> {
    if in_flight == 0 {
        Span::styled("0", dim())
    } else {
        Span::styled(
            in_flight.to_string(),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    }
}

/// One quota window → (gauge cell, reset cell). The gauge label is ALWAYS the
/// percentage (req4: it used to flip to the reset countdown when parked / over
/// threshold, which put a duration where a percent belongs and duplicated the
/// reset column — confusing). Over-threshold is already signaled by the red
/// color and a `!` marker; the countdown lives only in the reset column.
///
/// Issue #33: the gauge cell also carries the [`WindowDisplayState`] so a
/// never-used (`cold`), stale, or poll-degraded window is visibly distinct from
/// an honest `0%` — render-only, derived from already-recorded state.
#[allow(clippy::too_many_arguments)]
fn window_cells(
    window: &Option<QuotaWindow>,
    threshold: f64,
    parked: bool,
    now: SystemTime,
    tz_offset: i64,
    wide: bool,
    max_age: Duration,
    consecutive_failures: u32,
) -> (Cell<'static>, Cell<'static>) {
    let display = classify_window_display(window, now, max_age, consecutive_failures);
    let Some(window) = window else {
        // Cold (or poll-degraded with no window yet): show the state, not a bare
        // — that reads the same as "0% used".
        return (
            Cell::from(Span::styled(
                format!("{} {}", display.glyph(), display.label()),
                dim(),
            )),
            Cell::from(Span::styled("—", dim())),
        );
    };
    let utilization = window.effective_utilization(now);
    let color = level_color(format::gauge_level(utilization));
    // A trailing `!` flags an account that is parked or past its threshold —
    // the signal the old countdown-swap was carrying, without hiding the %.
    let over = parked || utilization > threshold;
    let label = if over {
        format!("{}!", format::percent(utilization))
    } else {
        format::percent(utilization)
    };
    let mut spans = vec![
        Span::styled(
            format::gauge_bar(utilization, GAUGE_BAR_WIDTH),
            Style::new().fg(color),
        ),
        Span::raw(" "),
        Span::styled(label, Style::new().fg(color)),
    ];
    // Stale / poll-degraded windows still carry a real value, but it is no
    // longer trustworthy — flag it with the state glyph so it reads distinctly
    // from a fresh populated window (issue #33). Populated needs no marker.
    if !matches!(display, WindowDisplayState::Populated) {
        spans.push(Span::styled(format!(" {}", display.glyph()), dim()));
    }
    let gauge = Cell::from(Line::from(spans));
    let reset_cell = Cell::from(match reset_label(window, now, tz_offset, wide) {
        Some(label) => Span::raw(label),
        None => Span::styled("—", dim()),
    });
    (gauge, reset_cell)
}

/// Reset column text: compact countdown, plus the absolute local time in
/// wide mode — "1h02m (14:30)", "2d4h (06-15 09:00)".
fn reset_label(
    window: &QuotaWindow,
    now: SystemTime,
    tz_offset: i64,
    wide: bool,
) -> Option<String> {
    let remaining = window.resets_at.duration_since(now).ok()?;
    if remaining.is_zero() {
        return None;
    }
    let countdown = select::compact_duration(remaining);
    if wide {
        Some(format!(
            "{countdown} ({})",
            format::absolute_label(window.resets_at, now, tz_offset)
        ))
    } else {
        Some(countdown)
    }
}

/// Middle row: scheduler/poller/totals summary, with the selected-account
/// detail pane beside it when there is room. The old `d` toggle is gone (issue
/// #5): on MAIN the detail rides alongside the summary whenever the width
/// allows, and the Accounts overlay (`a`) gives detail the full-width slot.
fn draw_middle(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
) {
    let has_accounts = !view.snapshot.accounts.is_empty();
    if has_accounts && area.width >= SIDE_BY_SIDE_AT {
        let [summary_area, detail_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(48)]).areas(area);
        draw_summary(frame, summary_area, view, ctx);
        draw_detail(frame, detail_area, view, ctx, chrome);
    } else {
        // Too narrow for both (or no accounts): MAIN shows the summary; the
        // full detail is one keystroke away in the Accounts overlay.
        draw_summary(frame, area, view, ctx);
    }
}

/// Scheduler / poller / totals summary pane.
fn draw_summary(frame: &mut Frame, area: Rect, view: &DashboardView, ctx: &FrameCtx) {
    let snapshot = &view.snapshot;
    let now = ctx.now;
    let label = |text: &'static str| Span::styled(format!(" {text:<9}"), dim());
    let mut lines: Vec<Line> = Vec::with_capacity(6);

    // Per-group current subscription (req1): claude and codex pick their
    // current account INDEPENDENTLY, so show one line per group present.
    let groups_present: Vec<BackendGroup> = [BackendGroup::Claude, BackendGroup::Codex]
        .into_iter()
        .filter(|g| snapshot.accounts.iter().any(|a| a.group == *g))
        .collect();
    if groups_present.is_empty() {
        lines.push(Line::from(vec![
            label("current"),
            Span::styled("(none)", Style::new().fg(Color::Red)),
        ]));
    }
    for (i, g) in groups_present.iter().enumerate() {
        let mut spans = vec![
            label(if i == 0 { "current" } else { "" }),
            Span::styled(
                format!("{:<7}", g.as_str()),
                group_color(Some(g.as_str())).add_modifier(Modifier::BOLD),
            ),
        ];
        match snapshot.current_for_group(*g) {
            Some(current) => {
                spans.push(Span::styled(
                    current.to_string(),
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                ));
                if let Some(switch) = view.last_switch.as_ref().filter(|s| s.to == current.0) {
                    let ago = now
                        .duration_since(switch.at)
                        .map(select::compact_duration)
                        .unwrap_or_else(|_| "0s".into());
                    let why = switch.reason.as_deref().unwrap_or("switch");
                    let from = switch
                        .from
                        .as_deref()
                        .map(|f| format!("{f} → "))
                        .unwrap_or_default();
                    spans.push(Span::styled(format!("  {from}{why}, {ago} ago"), dim()));
                }
            }
            None => spans.push(Span::styled("(none)", Style::new().fg(Color::Red))),
        }
        lines.push(Line::from(spans));
    }

    // Per-group next-in-line (req1 symmetry with the current block) + the
    // shared eval-tick countdown. One tick re-evaluates every group, so the
    // "eval in ~Xs" is shown once, on the first row.
    let tick = view.evaluate_tick.as_secs().max(1);
    let to_next_eval = tick - (view.uptime.as_secs() % tick);
    if groups_present.is_empty() {
        lines.push(Line::from(vec![label("next"), Span::raw("—")]));
    }
    for (i, g) in groups_present.iter().enumerate() {
        let next = select::next_in_line(snapshot, &view.select_params, now, Some(*g));
        let mut spans = vec![
            label(if i == 0 { "next" } else { "" }),
            Span::styled(
                format!("{:<7}", g.as_str()),
                group_color(Some(g.as_str())).add_modifier(Modifier::BOLD),
            ),
            Span::raw(next.map(|n| n.to_string()).unwrap_or_else(|| "—".into())),
        ];
        if i == 0 {
            spans.push(Span::styled(
                format!(
                    "  eval in ~{}",
                    select::compact_duration(Duration::from_secs(to_next_eval))
                ),
                dim(),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(vec![
        label("poller"),
        Span::raw(poller_summary(view, now)),
    ]));

    let totals = view.global_totals;
    lines.push(Line::from(vec![
        label("totals"),
        Span::raw(format!("{} req · ", format::human_count(totals.requests))),
        Span::styled(
            format!("{} ok", format::human_count(totals.ok)),
            Style::new().fg(Color::Green),
        ),
        Span::raw(" / "),
        Span::styled(
            format!("{} err", format::human_count(totals.errors)),
            if totals.errors > 0 {
                Style::new().fg(Color::Red)
            } else {
                dim()
            },
        ),
        Span::raw(format!(
            " · in {} / out {} tok",
            format::human_count(totals.tokens_in),
            format::human_count(totals.tokens_out)
        )),
    ]));

    let in_flight: u32 = snapshot.accounts.iter().map(|a| a.in_flight).sum();
    lines.push(Line::from(vec![
        label("load"),
        Span::raw(format!(
            "{:.1} req/min (5m) · {in_flight} in flight",
            view.rpm_5m
        )),
    ]));

    // Codex group settings (req8.1): model / fast tier / reasoning effort, with
    // the keys that change them. Only when a codex account exists.
    if view.codex.available {
        let c = &view.codex;
        let fast_style = if c.fast {
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        lines.push(Line::from(vec![
            label("codex"),
            Span::styled(
                c.model.clone(),
                group_color(Some("codex")).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" · fast "),
            Span::styled(if c.fast { "on" } else { "off" }, fast_style),
            Span::raw(" · effort "),
            Span::raw(c.effort.clone().unwrap_or_else(|| "default".into())),
            Span::styled("   [f fast · m model · e effort]", dim()),
        ]));
    }

    let block = Block::new().borders(Borders::TOP).title(" scheduler ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// One-line usage-poller health: oauth count, last-success age spread,
/// backing-off accounts, soonest next poll.
fn poller_summary(view: &DashboardView, now: SystemTime) -> String {
    let oauth: Vec<&AccountSnapshot> = view
        .snapshot
        .accounts
        .iter()
        .filter(|a| a.credential_kind == "oauth")
        .collect();
    if oauth.is_empty() {
        return "no oauth accounts (header-driven only)".into();
    }
    let mut ok_ages: Vec<Duration> = Vec::new();
    let mut next_in: Option<Duration> = None;
    let mut backoff: Vec<String> = Vec::new();
    for account in &oauth {
        let Some(health) = view.poll_health(&account.id.0) else {
            continue;
        };
        if let Some(age) = health.last_ok.and_then(|at| now.duration_since(at).ok()) {
            ok_ages.push(age);
        }
        if let Ok(eta) = health.next_at.duration_since(now) {
            next_in = Some(next_in.map_or(eta, |cur| cur.min(eta)));
        }
        if health.consecutive_failures > 0 {
            backoff.push(format!("{}×{}", account.id, health.consecutive_failures));
        }
    }
    if ok_ages.is_empty() && backoff.is_empty() {
        return format!("{} oauth · warming up", oauth.len());
    }
    let mut out = format!("{} oauth", oauth.len());
    if let (Some(min), Some(max)) = (ok_ages.iter().min(), ok_ages.iter().max()) {
        if min == max {
            out.push_str(&format!(
                " · last ok {} ago",
                select::compact_duration(*min)
            ));
        } else {
            out.push_str(&format!(
                " · last ok {}–{} ago",
                select::compact_duration(*min),
                select::compact_duration(*max)
            ));
        }
    } else {
        out.push_str(" · no successful poll yet");
    }
    if let Some(eta) = next_in {
        out.push_str(&format!(" · next ~{}", select::compact_duration(eta)));
    }
    if !backoff.is_empty() {
        out.push_str(&format!(" · backoff {}", backoff.join(" ")));
    }
    out
}

/// Selected-account detail pane: the cursor row in select mode, otherwise
/// the current account, otherwise the head of the order.
fn draw_detail(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
) {
    let snapshot = &view.snapshot;
    let pos = match chrome.mode {
        Mode::Select { idx } | Mode::ConfirmRemove { idx } => {
            idx.min(ctx.order.len().saturating_sub(1))
        }
        // NewLogin keeps the detail pane on the current account.
        Mode::Normal | Mode::AddKey | Mode::NewLogin { .. } => snapshot
            .representative_current()
            .and_then(|cur| {
                ctx.order
                    .iter()
                    .position(|&i| &snapshot.accounts[i].id == cur)
            })
            .unwrap_or(0),
    };
    let Some(account) = ctx.order.get(pos).map(|&i| &snapshot.accounts[i]) else {
        return;
    };
    let params = &view.select_params;
    let now = ctx.now;
    let gate = select::eligibility(account, params, now, ctx.headers_only);
    let is_current = snapshot.is_current(&account.id);

    let mut lines: Vec<Line> = Vec::with_capacity(7);
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", account.id),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", account.credential_kind), dim()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!(" order #{}", pos + 1), Style::new()),
        Span::raw(" · "),
        status_span(account, gate, is_current, params, now, ctx.frame),
    ]));
    let mut token_line = vec![Span::styled(" token ", dim())];
    token_line.extend(token_detail_spans(
        account,
        view.refresh_ahead,
        now,
        ctx.tz_offset,
    ));
    lines.push(Line::from(token_line));
    let consecutive_failures = view
        .poll_health
        .get(&account.id.0)
        .map_or(0, |h| h.consecutive_failures);
    let max_age = params.usage_max_age;
    lines.push(window_detail_line(
        "5h",
        &account.five_hour,
        ctx,
        max_age,
        consecutive_failures,
    ));
    lines.push(window_detail_line(
        "7d",
        &account.seven_day,
        ctx,
        max_age,
        consecutive_failures,
    ));
    let totals = view.totals_for(&account.id.0);
    lines.push(Line::from(vec![
        Span::styled(" life  ", dim()),
        Span::raw(format!(
            "{} req ({} ok/{} err) · in {}/out {}",
            format::human_count(totals.requests),
            format::human_count(totals.ok),
            format::human_count(totals.errors),
            format::human_count(totals.tokens_in),
            format::human_count(totals.tokens_out),
        )),
    ]));
    let poll = match view.poll_health(&account.id.0) {
        Some(health) => {
            let last = health
                .last_ok
                .and_then(|at| now.duration_since(at).ok())
                .map(|age| format!("ok {} ago", select::compact_duration(age)))
                .unwrap_or_else(|| "no success yet".into());
            let next = health
                .next_at
                .duration_since(now)
                .map(|eta| format!(" · next ~{}", select::compact_duration(eta)))
                .unwrap_or_default();
            let backoff = if health.consecutive_failures > 0 {
                format!(" · backoff ×{}", health.consecutive_failures)
            } else {
                String::new()
            };
            format!("{last}{next}{backoff}")
        }
        None if account.credential_kind == "oauth" => "not polled yet".into(),
        // apikey/codex accounts have no Anthropic usage endpoint to poll.
        None => format!("n/a ({})", account.credential_kind),
    };
    lines.push(Line::from(vec![
        Span::styled(" poll  ", dim()),
        Span::raw(poll),
    ]));

    let block = Block::new().borders(Borders::TOP).title(" detail ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Token line for the detail pane: countdown + absolute local expiry,
/// when the token was last refreshed, and when the background refresher
/// will act — "expires 6h52m (08:21) · refreshed 3m ago (01:26) · refresh
/// due in 52m". Already-due and expired states keep their warning colors.
fn token_detail_spans(
    account: &AccountSnapshot,
    refresh_ahead: Duration,
    now: SystemTime,
    tz_offset: i64,
) -> Vec<Span<'static>> {
    let Some(expires_ms) = account.token_expires_at_ms else {
        return vec![Span::styled("— (apikey)", dim())];
    };
    let expires_at = UNIX_EPOCH + Duration::from_millis(expires_ms);
    let mut spans = match expires_at.duration_since(now) {
        Ok(left) => {
            let absolute = format::absolute_label(expires_at, now, tz_offset);
            let head = format!("expires {} ({absolute})", select::compact_duration(left));
            if left > refresh_ahead {
                vec![Span::raw(head)]
            } else {
                vec![Span::styled(
                    format!("{head} · refresh due"),
                    Style::new().fg(Color::Yellow),
                )]
            }
        }
        Err(_) => vec![Span::styled(
            "expired — refresh overdue".to_string(),
            Style::new().fg(Color::Red),
        )],
    };
    let refreshed = match account.last_refresh_ms {
        Some(ms) => {
            let at = UNIX_EPOCH + Duration::from_millis(ms);
            let ago = now.duration_since(at).unwrap_or_default();
            format!(
                " · refreshed {} ago ({})",
                select::compact_duration(ago),
                format::absolute_label(at, now, tz_offset),
            )
        }
        None => " · refreshed never".to_string(),
    };
    spans.push(Span::styled(refreshed, dim()));
    if let Ok(left) = expires_at.duration_since(now) {
        if left > refresh_ahead {
            spans.push(Span::raw(format!(
                " · refresh due in {}",
                select::compact_duration(left - refresh_ahead)
            )));
        }
    }
    spans
}

/// Detail line for one window: utilization, reset (countdown + absolute),
/// observation source + age.
fn window_detail_line(
    name: &'static str,
    window: &Option<QuotaWindow>,
    ctx: &FrameCtx,
    max_age: Duration,
    consecutive_failures: u32,
) -> Line<'static> {
    let label = Span::styled(format!(" {name:<5} "), dim());
    let now = ctx.now;
    // Issue #33: surface the distinct display state in the detail pane too.
    let display = classify_window_display(window, now, max_age, consecutive_failures);
    let Some(window) = window else {
        return Line::from(vec![
            label,
            Span::styled(format!("no data ({})", display.label()), dim()),
        ]);
    };
    let utilization = window.effective_utilization(now);
    let color = level_color(format::gauge_level(utilization));
    let reset = reset_label(window, now, ctx.tz_offset, true).unwrap_or_else(|| "expired".into());
    let source = match window.source {
        crate::scheduler::window::WindowSource::Headers => "headers",
        crate::scheduler::window::WindowSource::UsagePoll => "poll",
    };
    let age = now
        .duration_since(window.fetched_at)
        .map(select::compact_duration)
        .unwrap_or_else(|_| "0s".into());
    let mut spans = vec![
        label,
        Span::styled(format::percent(utilization), Style::new().fg(color)),
        Span::raw(format!(" · resets {reset}")),
        Span::styled(format!(" · {source} {age} ago"), dim()),
    ];
    if !matches!(display, WindowDisplayState::Populated) {
        spans.push(Span::styled(format!(" · {}", display.label()), dim()));
    }
    Line::from(spans)
}

/// One click-target inside the activity panel: a completed *request* entry, its
/// stable [`ActivityKey`], and the absolute screen rows it occupies this frame
/// (`y_start..y_start+height`). Recorded during [`draw_activity`] so the mouse
/// handler can map a click to the entry without re-deriving the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityHit {
    pub key: ActivityKey,
    pub y_start: u16,
    pub height: u16,
}

/// The activity panel's rendered layout for one frame: the panel rect plus the
/// ordered hit-targets (request rows only — notes/in-flight are not clickable).
/// Threaded back to the runtime so a left-click can be mapped to an entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ActivityChrome {
    pub area: Rect,
    pub hits: Vec<ActivityHit>,
}

/// Pure hit-test (unit-tested): which activity entry, if any, does the click at
/// absolute `(col, row)` land on? `None` when the click is outside the panel,
/// on the title border, or on a non-request line. Used by the mouse handler.
pub(crate) fn hit_test_activity(
    chrome: &ActivityChrome,
    col: u16,
    row: u16,
) -> Option<ActivityKey> {
    let area = chrome.area;
    // Outside the panel rect → not ours.
    if col < area.x || col >= area.right() || row < area.y || row >= area.bottom() {
        return None;
    }
    chrome
        .hits
        .iter()
        .find(|hit| row >= hit.y_start && row < hit.y_start.saturating_add(hit.height))
        .map(|hit| hit.key.clone())
}

fn draw_activity(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    chrome: &Chrome,
    now: SystemTime,
) -> ActivityChrome {
    let in_flight = &view.in_flight;
    let capacity = area.height.saturating_sub(1) as usize; // top border

    let anim_frame = chrome.frame;
    let mut lines: Vec<Line> = Vec::with_capacity(capacity);
    // In-flight rows pinned on top ONLY when viewing the live tail (scroll==0);
    // while scrolled into history they'd steal rows from the page being read.
    if chrome.activity_scroll == 0 {
        for request in in_flight.iter().rev().take(capacity) {
            let elapsed = now.duration_since(request.started_at).unwrap_or_default();
            // Working spinner differs by backend group: Claude gets the braille
            // orbit (magenta), Codex a quarter-block orbit (cyan) — the same
            // colors as the group labels — so you can tell what's running where
            // at a glance. Pre-routing rows (no account yet) are a dim braille.
            let (glyph, color) = match request.account.as_deref().and_then(|a| group_of(view, a)) {
                Some(BackendGroup::Codex) => (anim::block_spin(anim_frame), Color::Cyan),
                Some(BackendGroup::Claude) => (anim::braille_spin(anim_frame), Color::Magenta),
                None => (anim::braille_spin(anim_frame), Color::DarkGray),
            };
            let mut spans = vec![
                Span::styled(format!(" {glyph} "), Style::new().fg(color)),
                Span::styled(format::clock_hms_utc(request.started_at), dim()),
                Span::raw(format!("  {} {}", request.method, request.path)),
            ];
            // [group model] badge while in flight (issue #2, 2a). The data is
            // filled at routing time (req11); effort is not carried in-flight,
            // so the badge mirrors completed rows minus the effort suffix.
            if let Some(meta) =
                activity_meta(request.group.as_deref(), request.model.as_deref(), None)
            {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(meta, group_color(request.group.as_deref())));
            }
            if let Some(account) = &request.account {
                spans.push(Span::raw(format!(" → {account}")));
            }
            spans.push(Span::styled(
                format!(" ({}…)", format::elapsed_secs(elapsed)),
                dim(),
            ));
            lines.push(Line::from(spans));
        }
    }
    // Completed entries, newest first, windowed by the scroll offset (req6:
    // the whole history is reachable, not just the rows that happen to fit).
    // Each request row may expand into several detail lines when clicked; the
    // hit list records the absolute screen rows each entry owns so the click
    // handler maps a (col,row) back to its stable key. Paragraph renders line 0
    // at `area.y + 1` (the title takes the top border row).
    let total = view.completed.len();
    let scroll = chrome.activity_scroll.min(total.saturating_sub(1));
    let body_top = area.y.saturating_add(1);
    let mut hits: Vec<ActivityHit> = Vec::new();
    for entry in view.completed.iter().skip(scroll) {
        if lines.len() >= capacity {
            break;
        }
        let expanded = entry
            .activity_key()
            .is_some_and(|k| chrome.expanded_activity.as_ref() == Some(&k));
        let row_y = body_top.saturating_add(lines.len() as u16);
        lines.push(completed_line(entry, expanded));
        let mut height = 1u16;
        if expanded {
            for detail in completed_detail_lines(entry) {
                if lines.len() >= capacity {
                    break;
                }
                lines.push(detail);
                height = height.saturating_add(1);
            }
        }
        // Only request rows are clickable (notes have no key).
        if let Some(key) = entry.activity_key() {
            hits.push(ActivityHit {
                key,
                y_start: row_y,
                height,
            });
        }
    }

    // Title carries the scroll position so it's obvious you're in history. The
    // shown-range end is approximate when rows expanded, so report the count
    // windowed by the live-tail capacity.
    let shown_last = (scroll + capacity).min(total);
    let title = if scroll > 0 {
        format!(
            " activity — {}–{} of {total} (↑ history) ",
            scroll + 1,
            shown_last
        )
    } else if in_flight.is_empty() {
        format!(" activity — {total} ")
    } else {
        format!(" activity — {} in flight ", in_flight.len())
    };
    let block = Block::new().borders(Borders::TOP).title(title);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    ActivityChrome { area, hits }
}

/// The one-line activity row. For request entries a leading marker shows the
/// expand state (`▸` collapsed / `▾` expanded); notes keep the plain indent.
fn completed_line(entry: &Completed, expanded: bool) -> Line<'static> {
    match &entry.body {
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
        } => {
            let marker = if expanded { '▾' } else { '▸' };
            let stamp = Span::styled(
                format!(" {marker} {}  ", format::clock_hms_utc(entry.at)),
                dim(),
            );
            let account = account.as_deref().unwrap_or("?");
            let status_style = if *status < 400 {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::Red)
            };
            let mut detail = format::elapsed_secs(*duration);
            if let Some(tokens) = tokens {
                detail.push_str(&format!(", {} tok", format::human_count(tokens.total())));
                // Always show the API-equivalent cost ($) inline (item #4). The
                // render path holds no config overrides, so pass an empty map =
                // the built-in default rate table. Cost is shown only when this
                // request's (group, model) is known; an unknown/zero-rate model
                // yields $0.0000. NOTE: the view-model's per-entry tokens carry
                // only input+output (cache detail rides the model rows), so this
                // is the input+output cost — consistent with the tok count above.
                if let (Some(group), Some(model)) = (group, model) {
                    let cost = crate::pricing::cost_usd(
                        group,
                        model,
                        tokens,
                        &std::collections::HashMap::new(),
                    );
                    detail.push_str(&format!(", {}", format_cost(cost)));
                }
            }
            let mut spans = vec![stamp, Span::raw(format!("{method} {path}"))];
            // [group model·effort] badge, when known (req7).
            if let Some(meta) = activity_meta(group.as_deref(), model.as_deref(), effort.as_deref())
            {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(meta, group_color(group.as_deref())));
            }
            spans.push(Span::raw(format!(" → {account} (")));
            spans.push(Span::styled(status.to_string(), status_style));
            spans.push(Span::raw(format!(", {detail})")));
            Line::from(spans)
        }
        CompletedBody::Note { text, error } => {
            let stamp = Span::styled(format!("   {}  ", format::clock_hms_utc(entry.at)), dim());
            let style = if *error {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            Line::from(vec![stamp, Span::styled(text.clone(), style)])
        }
    }
}

/// Indented detail lines for an expanded request row (Feature B): full
/// method+path, account, status, duration, group/model/effort, the token
/// breakdown, and the per-component + total API-equivalent cost via
/// [`crate::pricing`]. Empty for notes (never expandable).
fn completed_detail_lines(entry: &Completed) -> Vec<Line<'static>> {
    let CompletedBody::Request {
        method,
        path,
        account,
        status,
        duration,
        tokens,
        group,
        model,
        effort,
    } = &entry.body
    else {
        return Vec::new();
    };
    let indent = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("       {label:<8}"), dim()),
            Span::raw(value),
        ])
    };
    let mut lines = Vec::new();
    lines.push(indent("request", format!("{method} {path}")));
    lines.push(indent(
        "account",
        account.clone().unwrap_or_else(|| "?".to_string()),
    ));
    let status_color = if *status < 400 {
        Color::Green
    } else {
        Color::Red
    };
    lines.push(Line::from(vec![
        Span::styled("       status  ", dim()),
        Span::styled(status.to_string(), Style::new().fg(status_color)),
        Span::styled(
            format!("  ·  {} elapsed", format::elapsed_secs(*duration)),
            dim(),
        ),
    ]));
    let model_label = match (group.as_deref(), model.as_deref()) {
        (Some(g), Some(m)) => format!("{g} {m}"),
        (Some(g), None) => g.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => "—".to_string(),
    };
    let effort_label = effort
        .as_deref()
        .map(|e| format!(" · effort {e}"))
        .unwrap_or_default();
    lines.push(indent("model", format!("{model_label}{effort_label}")));
    match tokens {
        Some(t) => {
            lines.push(indent(
                "tokens",
                format!(
                    "in {} · out {} · cache_read {} · cache_creation {} · total {}",
                    format::human_count(t.input),
                    format::human_count(t.output),
                    opt_count(t.cache_read),
                    opt_count(t.cache_creation),
                    format::human_count(t.total()),
                ),
            ));
            // Per-component + total API-equivalent cost (item #4). Empty
            // overrides = built-in default rate table. Each component is priced
            // in isolation via `cost_from_parts`, so the four add up to total.
            let empty = std::collections::HashMap::new();
            let (g, m) = (
                group.as_deref().unwrap_or(""),
                model.as_deref().unwrap_or(""),
            );
            let cost_in = crate::pricing::cost_from_parts(g, m, t.input, 0, None, None, &empty);
            let cost_out = crate::pricing::cost_from_parts(g, m, 0, t.output, None, None, &empty);
            let cost_cr = crate::pricing::cost_from_parts(g, m, 0, 0, t.cache_read, None, &empty);
            let cost_cc =
                crate::pricing::cost_from_parts(g, m, 0, 0, None, t.cache_creation, &empty);
            let cost_total = cost_in + cost_out + cost_cr + cost_cc;
            lines.push(Line::from(vec![
                Span::styled("       cost    ", dim()),
                Span::raw(format!(
                    "in {} · out {} · cache_read {} · cache_creation {} · ",
                    format_cost(cost_in),
                    format_cost(cost_out),
                    format_cost(cost_cr),
                    format_cost(cost_cc),
                )),
                Span::styled(format_cost(cost_total), Style::new().fg(Color::Green)),
            ]));
        }
        None => lines.push(indent("tokens", "—".to_string())),
    }
    lines
}

/// The credential's auth TYPE (oauth/api), orthogonal to its model group.
/// Codex accounts authenticate via ChatGPT OAuth, so they are `oauth` too —
/// the only `api` credential is a plain Anthropic API key (req5).
fn auth_type(credential_kind: &str) -> &'static str {
    match credential_kind {
        "apikey" => "api",
        _ => "oauth",
    }
}

/// Backend group of the account named `account` in the current snapshot, for
/// coloring/animating its in-flight rows. `None` if not found (pre-routing).
fn group_of(view: &DashboardView, account: &str) -> Option<BackendGroup> {
    view.snapshot
        .accounts
        .iter()
        .find(|a| a.id.0 == account)
        .map(|a| a.group)
}

/// Color a backend-group label: codex = cyan, claude = magenta, unknown = gray.
/// Shared by the account table (req5) and the activity log (req7) so the group
/// reads the same everywhere.
fn group_color(group: Option<&str>) -> Style {
    match group {
        Some("codex") => Style::new().fg(Color::Cyan),
        Some("claude") => Style::new().fg(Color::Magenta),
        _ => dim(),
    }
}

/// Abbreviate a model id for the activity badge by dropping the redundant
/// `claude-` prefix on Claude models (`claude-opus-4-8` → `opus-4-8`). Codex
/// and unknown models pass through unchanged (issue #2, 2b).
fn abbrev_model<'a>(group: Option<&str>, model: &'a str) -> &'a str {
    if group == Some("claude") {
        model.strip_prefix("claude-").unwrap_or(model)
    } else {
        model
    }
}

/// Compose the `[group model·effort]` badge for an activity line, or `None`
/// when nothing is known. The model id is abbreviated via [`abbrev_model`].
/// Examples: `[codex gpt-5.5·high]`, `[claude opus-4-8·16k]`, `[claude]`.
fn activity_meta(group: Option<&str>, model: Option<&str>, effort: Option<&str>) -> Option<String> {
    if group.is_none() && model.is_none() && effort.is_none() {
        return None;
    }
    let mut label = String::new();
    if let Some(g) = group {
        label.push_str(g);
    }
    if let Some(m) = model {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(abbrev_model(group, m));
    }
    if let Some(e) = effort {
        label.push('·');
        label.push_str(e);
    }
    Some(format!("[{label}]"))
}

/// Bottom log console: the tail of the tracing ring, newest line on the
/// bottom row (auto-follow), level-colored prefix, no wrapping (long lines
/// truncate — the console is a glance surface, not a pager).
fn draw_logs(frame: &mut Frame, area: Rect, view: &DashboardView) {
    let block = Block::new().borders(Borders::TOP).title(" logs ");
    let capacity = area.height.saturating_sub(1) as usize; // top border
                                                           // `view.logs` is oldest→newest; take the newest `capacity` lines.
    let start = view.logs.len().saturating_sub(capacity);
    let lines: Vec<Line> = view.logs[start..].iter().map(log_line).collect();
    // Bottom-align: pad above so the newest line hugs the bottom edge.
    let mut padded: Vec<Line> = Vec::with_capacity(capacity);
    padded.resize_with(capacity.saturating_sub(lines.len()), Line::default);
    padded.extend(lines);
    frame.render_widget(Paragraph::new(padded).block(block), area);
}

fn log_line(line: &LogLine) -> Line<'_> {
    use tracing::Level;

    let (label, style) = if line.level == Level::ERROR {
        ("ERROR", Style::new().fg(Color::Red))
    } else if line.level == Level::WARN {
        (" WARN", Style::new().fg(Color::Yellow))
    } else if line.level == Level::INFO {
        (" INFO", Style::new())
    } else if line.level == Level::DEBUG {
        ("DEBUG", dim())
    } else {
        ("TRACE", dim())
    };
    Line::from(vec![
        Span::styled(format!(" {label} "), style),
        Span::raw(line.text.as_str()),
    ])
}

// ---------------------------------------------------------------------------
// Model usage (req1-20): compact strip + detailed table/drill-down.
// ---------------------------------------------------------------------------

fn model_total(m: &ModelUsageDoc) -> u64 {
    m.tokens_in.saturating_add(m.tokens_out)
}

/// API-equivalent USD cost for one model row (item #4), computed inline at
/// render time from the row's accumulated token parts (input/output + the
/// optional cache split) via [`crate::pricing`]. The render path holds no
/// config overrides, so an empty map = the built-in default rate table. An
/// unknown/zero-rate `(group, model)` yields `0.0`.
fn model_cost(m: &ModelUsageDoc) -> f64 {
    crate::pricing::cost_from_parts(
        &m.group,
        &m.model,
        m.tokens_in,
        m.tokens_out,
        m.cache_read,
        m.cache_creation,
        &std::collections::HashMap::new(),
    )
}

/// "—" when unavailable (the upstream never reported it), else a human count —
/// so the UI never implies a precise zero it does not have (req9).
fn opt_count(v: Option<u64>) -> String {
    match v {
        Some(n) => format::human_count(n),
        None => "—".to_string(),
    }
}

/// Compact "last used" age for the strip ("12s", "3m"); "—" for in-flight-only
/// rows that have no completed request yet.
fn model_age_compact(last_used_ms: u64, now: SystemTime) -> String {
    if last_used_ms == 0 {
        return "—".to_string();
    }
    let at = UNIX_EPOCH + Duration::from_millis(last_used_ms);
    now.duration_since(at)
        .map(select::compact_duration)
        .unwrap_or_else(|_| "now".to_string())
}

fn model_is_recent(last_used_ms: u64, now: SystemTime) -> bool {
    if last_used_ms == 0 {
        return false;
    }
    let at = UNIX_EPOCH + Duration::from_millis(last_used_ms);
    now.duration_since(at)
        .map(|age| age <= MODEL_RECENT_WINDOW)
        .unwrap_or(true)
}

/// Leading marker for a model row: a group-colored working spinner while it has
/// in-flight traffic (req11), a `●` when recently used (req15), else blank.
fn model_active_marker(m: &ModelUsageDoc, now: SystemTime, frame: usize) -> Span<'static> {
    if m.in_flight > 0 {
        let glyph = if m.group == "codex" {
            anim::block_spin(frame)
        } else {
            anim::braille_spin(frame)
        };
        Span::styled(glyph.to_string(), group_color(Some(m.group.as_str())))
    } else if model_is_recent(m.last_used_ms, now) {
        Span::styled("●", Style::new().fg(Color::Green))
    } else {
        Span::raw(" ")
    }
}

/// A `GROUP model` label pair, group-colored, model bold when active.
fn model_name_cells(m: &ModelUsageDoc, active: bool) -> (Cell<'static>, Cell<'static>) {
    let group = Cell::from(Span::styled(
        m.group.to_uppercase(),
        group_color(Some(m.group.as_str())).add_modifier(Modifier::BOLD),
    ));
    let name_style = if active {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    (group, Cell::from(Span::styled(m.model.clone(), name_style)))
}

/// Always-visible compact strip: the top models by total tokens, each with a
/// proportional mini-bar and req/tok/last-used (req12/28). Narrow terminals
/// drop the bar so the column set stays readable (req29).
fn draw_models_strip(frame: &mut Frame, area: Rect, view: &DashboardView, now: SystemTime) {
    let rows_data: Vec<&ModelUsageDoc> = view.model_usage.iter().take(MODEL_STRIP_ROWS).collect();
    let max_total = view
        .model_usage
        .iter()
        .map(model_total)
        .max()
        .unwrap_or(0)
        .max(1);
    let wide = area.width >= SIDE_BY_SIDE_AT;
    let frame_n = 0; // strip markers don't need to animate per draw tick

    let rows = rows_data.into_iter().map(|m| {
        let active = m.in_flight > 0 || model_is_recent(m.last_used_ms, now);
        let (group_cell, name_cell) = model_name_cells(m, active);
        let share = model_total(m) as f64 / max_total as f64;
        let mut cells = vec![
            Cell::from(model_active_marker(m, now, frame_n)),
            group_cell,
            name_cell,
        ];
        if wide {
            cells.push(Cell::from(Span::styled(
                format::gauge_bar(share, MODEL_BAR_WIDTH),
                group_color(Some(m.group.as_str())),
            )));
        }
        cells.push(Cell::from(format::human_count(m.requests)));
        cells.push(Cell::from(format::human_count(model_total(m))));
        // API-equivalent cost ($) cell, right after tok (item #4).
        cells.push(Cell::from(Span::styled(
            format_cost(model_cost(m)),
            Style::new().fg(Color::Green),
        )));
        let mut last = model_age_compact(m.last_used_ms, now);
        if m.in_flight > 0 {
            last = format!("{} in-flight", m.in_flight);
        }
        cells.push(Cell::from(Span::styled(last, dim())));
        Row::new(cells)
    });

    let (header, constraints): (Vec<&'static str>, Vec<Constraint>) = if wide {
        (
            vec!["", "group", "model", "share", "req", "tok", "$", "last"],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(MODEL_BAR_WIDTH as u16 + 1),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(12),
            ],
        )
    } else {
        (
            vec!["", "group", "model", "req", "tok", "$", "last"],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(12),
            ],
        )
    };
    let title = format!(
        " models — top {} by tokens (g: all) ",
        view.model_usage.len()
    );
    let table = Table::new(rows, constraints)
        .header(Row::new(header).style(dim().add_modifier(Modifier::BOLD)))
        .block(Block::new().borders(Borders::TOP).title(title));
    frame.render_widget(table, area);
}

/// Detailed model view body: the full scrollable table beside (or above) the
/// drill-down panel for the cursored model row.
fn draw_models_full(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
) {
    if view.model_usage.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "no model usage yet — send a request through the proxy",
            Style::new().fg(Color::Yellow),
        )))
        .block(Block::new().borders(Borders::TOP).title(" models "));
        frame.render_widget(empty, area);
        return;
    }
    if area.width >= SIDE_BY_SIDE_AT {
        let [table_area, detail_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(46)]).areas(area);
        draw_models_table(frame, table_area, view, ctx, chrome);
        draw_model_detail(frame, detail_area, view, ctx, chrome);
    } else {
        draw_models_table(frame, area, view, ctx, chrome);
    }
}

/// The full model table (all rows reachable via the cursor, req13). Columns
/// drop on narrow widths. The title shows the cursor position and total so it
/// is obvious more rows exist off-screen.
fn draw_models_table(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
) {
    let now = ctx.now;
    let total = view.model_usage.len();
    let cursor = chrome.model_cursor.min(total.saturating_sub(1));
    let capacity = (area.height.saturating_sub(2) as usize).max(1); // border + header
    let start = if cursor >= capacity {
        cursor + 1 - capacity
    } else {
        0
    };
    let end = (start + capacity).min(total);
    let wide = area.width >= WIDE_TABLE_AT;

    let rows = view.model_usage[start..end]
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let idx = start + i;
            let active = m.in_flight > 0 || model_is_recent(m.last_used_ms, now);
            let (group_cell, name_cell) = model_name_cells(m, active);
            let ok_err = Line::from(vec![
                Span::styled(format::human_count(m.ok), Style::new().fg(Color::Green)),
                Span::raw("/"),
                Span::styled(
                    format::human_count(m.errors),
                    if m.errors > 0 {
                        Style::new().fg(Color::Red)
                    } else {
                        dim()
                    },
                ),
            ]);
            let mut cells = vec![
                Cell::from(model_active_marker(m, now, ctx.frame)),
                group_cell,
                name_cell,
                Cell::from(format::human_count(m.requests)),
                Cell::from(ok_err),
                Cell::from(format::human_count(m.tokens_in)),
                Cell::from(format::human_count(m.tokens_out)),
                // API-equivalent cost ($) column, after out (item #4).
                Cell::from(Span::styled(
                    format_cost(model_cost(m)),
                    Style::new().fg(Color::Green),
                )),
            ];
            if wide {
                cells.push(Cell::from(Span::styled(opt_count(m.cache_read), dim())));
            }
            cells.push(Cell::from(Span::styled(
                model_age_compact(m.last_used_ms, now),
                dim(),
            )));
            cells.push(Cell::from(in_flight_span(m.in_flight)));
            let row = Row::new(cells);
            if idx == cursor {
                row.style(Style::new().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        });

    let (header, constraints): (Vec<&'static str>, Vec<Constraint>) = if wide {
        (
            vec![
                "", "group", "model", "req", "ok/err", "in", "out", "$", "cache", "last", "if",
            ],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(3),
            ],
        )
    } else {
        (
            vec![
                "", "group", "model", "req", "ok/err", "in", "out", "$", "if",
            ],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(3),
            ],
        )
    };
    let title = format!(" models — {} of {total} ", cursor + 1);
    let table = Table::new(rows, constraints)
        .header(Row::new(header).style(dim().add_modifier(Modifier::BOLD)))
        .block(Block::new().borders(Borders::TOP).title(title));
    frame.render_widget(table, area);
}

/// Drill-down panel for the cursored model row: token + cache split, account
/// breakdown (req19), effort (req18) and endpoint (req20) distributions.
fn draw_model_detail(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
) {
    let now = ctx.now;
    let cursor = chrome
        .model_cursor
        .min(view.model_usage.len().saturating_sub(1));
    let Some(m) = view.model_usage.get(cursor) else {
        return;
    };
    let counts = |items: &[crate::dashboard::ModelCountDoc]| {
        if items.is_empty() {
            "—".to_string()
        } else {
            items
                .iter()
                .map(|c| format!("{}×{}", c.label, c.requests))
                .collect::<Vec<_>>()
                .join("  ")
        }
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {} ", m.group.to_uppercase()),
            group_color(Some(m.group.as_str())).add_modifier(Modifier::BOLD),
        ),
        Span::styled(m.model.clone(), Style::new().add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" req   ", dim()),
        Span::raw(format!("{} (", format::human_count(m.requests))),
        Span::styled(
            format!("{} ok", format::human_count(m.ok)),
            Style::new().fg(Color::Green),
        ),
        Span::raw("/"),
        Span::styled(
            format!("{} err", format::human_count(m.errors)),
            if m.errors > 0 {
                Style::new().fg(Color::Red)
            } else {
                dim()
            },
        ),
        Span::raw(")"),
        Span::styled(
            if m.in_flight > 0 {
                format!(" · {} in-flight", m.in_flight)
            } else {
                String::new()
            },
            Style::new().fg(Color::Cyan),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" tok   ", dim()),
        Span::raw(format!(
            "in {} · out {}",
            format::human_count(m.tokens_in),
            format::human_count(m.tokens_out)
        )),
    ]));
    // Cache split — explicit "—" when the upstream did not report it (req9),
    // and a reminder that quota windows are account-level only (req27).
    lines.push(Line::from(vec![
        Span::styled(" cache ", dim()),
        Span::raw(format!(
            "read {} · creation {}",
            opt_count(m.cache_read),
            opt_count(m.cache_creation)
        )),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" last  ", dim()),
        Span::raw(model_age_compact(m.last_used_ms, now)),
        Span::styled(" ago", dim()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" effort", dim()),
        Span::raw(format!(" {}", counts(&m.efforts))),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" route ", dim()),
        Span::raw(counts(&m.endpoints)),
    ]));
    lines.push(Line::from(Span::styled(" accounts", dim())));
    if m.accounts.is_empty() {
        lines.push(Line::from(Span::styled("   —", dim())));
    } else {
        for a in &m.accounts {
            lines.push(Line::from(vec![
                Span::raw(format!("   {} ", a.name)),
                Span::styled(
                    format!(
                        "{} req · in {}/out {}",
                        format::human_count(a.requests),
                        format::human_count(a.tokens_in),
                        format::human_count(a.tokens_out),
                    ),
                    dim(),
                ),
            ]));
        }
    }
    // Quota windows are an account/provider fact, never per-model (req27) — make
    // that explicit so the per-account list above isn't read as a model limit.
    lines.push(Line::from(Span::styled(
        " quota is account-level (see accounts table)",
        dim(),
    )));

    let block = Block::new().borders(Borders::TOP).title(" model detail ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The heatmap cells for `window`, or an empty slice when the doc carries no
/// slice for it (older daemon / no activity). Already sorted by tokens desc by
/// the document builder.
fn heatmap_cells(
    view: &DashboardView,
    window: super::activity::StatsWindow,
) -> &[crate::dashboard::WindowedCellDoc] {
    let label = window.label();
    view.windowed
        .iter()
        .find(|w| w.window == label)
        .map(|w| w.cells.as_slice())
        .unwrap_or(&[])
}

/// Windowed per-account/per-model token heatmap (issue #23). One row per
/// `(group, model, account)` cell over the selected trailing window, with a
/// token-intensity bar coloured by the cell's share of the busiest cell. The
/// numbers are a BEST-EFFORT sample — the activity event channel is lossy
/// (events are dropped on a full channel) — so the panel says so explicitly and
/// never presents them as an exact ledger.
fn draw_heatmap(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    window: super::activity::StatsWindow,
) {
    if area.height == 0 {
        return;
    }
    let cells = heatmap_cells(view, window);
    let total = cells.len();
    let title = format!(" token heatmap — {} (best-effort) ", window.label());
    let block = Block::new().borders(Borders::TOP).title(title);

    let mut lines: Vec<Line> = Vec::new();
    // The accuracy contract (mandatory): a visible best-effort qualifier so the
    // windowed numbers are never read as exact accounting.
    lines.push(Line::from(Span::styled(
        " sampled from the activity feed — may undercount (lossy channel); w cycles 24h/72h",
        dim(),
    )));
    if total == 0 {
        lines.push(Line::from(Span::styled(
            " no windowed activity yet",
            Style::new().fg(Color::Yellow),
        )));
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    // Column header.
    lines.push(Line::from(Span::styled(
        format!(
            " {:<7} {:<20} {:<14} {:>6} {:>8}  intensity",
            "group", "model", "account", "req", "tokens"
        ),
        dim().add_modifier(Modifier::BOLD),
    )));

    let max_tokens = cells.iter().map(|c| c.tokens).max().unwrap_or(0).max(1);
    let shown = total.min(HEATMAP_MAX_ROWS);
    for c in &cells[..shown] {
        let share = c.tokens as f64 / max_tokens as f64;
        let bar = format::gauge_bar(share, HEATMAP_BAR_WIDTH);
        let bar_color = level_color(format::gauge_level(share));
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<7}", trunc(&c.group, 7)),
                group_color(Some(c.group.as_str())),
            ),
            Span::raw(format!(" {:<20}", trunc(&c.model, 20))),
            Span::styled(format!(" {:<14}", trunc(&c.account, 14)), dim()),
            Span::raw(format!(" {:>6}", format::human_count(c.requests))),
            Span::raw(format!(" {:>8}", format::human_count(c.tokens))),
            Span::raw("  "),
            Span::styled(bar, Style::new().fg(bar_color)),
        ]));
    }
    if total > shown {
        lines.push(Line::from(Span::styled(
            format!(" …{} more cell(s)", total - shown),
            dim(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Truncate `s` to `width` display columns, appending `…` when clipped. Keeps
/// the heatmap columns aligned without depending on a unicode-width crate (the
/// model/account strings here are ASCII slugs/emails).
fn trunc(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width == 0 {
        String::new()
    } else {
        let keep: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{keep}…")
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, chrome: &Chrome) {
    let status = Line::from(Span::styled(
        format!(" {}", chrome.status_line.as_deref().unwrap_or("")),
        Style::new().fg(Color::Yellow),
    ));
    let key = |k: &'static str| {
        Span::styled(k, Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    };
    // Attach mode disables the config-mutation keys (a/r/R act on the server
    // host's config); the keybar shows what is actually available.
    let attached = chrome.attach.is_some();
    let keybar = match chrome.mode {
        // While a Mode interaction is pending it owns the keybar regardless of
        // which overlay summoned it (the interactions run within Accounts).
        Mode::Normal => match chrome.overlay {
            // MAIN: summon overlays + codex + scroll. `a`/`g`/`l` open the
            // Accounts/Stats/Logs overlays where the detail/model/log surfaces
            // (and the add/remove/login affordances) now live (issue #5).
            Overlay::None => {
                let mut spans = vec![
                    Span::raw(" "),
                    key("q"),
                    Span::raw(" quit  "),
                    key("a"),
                    Span::raw(" accounts  "),
                    key("g"),
                    Span::raw(" stats  "),
                    key("l"),
                    Span::raw(" logs  "),
                    key("s"),
                    Span::raw(" sessions  "),
                ];
                if attached {
                    spans.push(Span::styled("R disabled (attached)  ", dim()));
                } else {
                    spans.push(key("R"));
                    spans.push(Span::raw(" reload  "));
                }
                spans.extend([
                    key("f/m/e"),
                    Span::raw(" codex  "),
                    key("↑↓"),
                    Span::raw(" scroll"),
                ]);
                Line::from(spans)
            }
            // Accounts overlay: the issue #3/#4 affordances. a (add) and r
            // (remove) act on the DAEMON via the control endpoints, so they are
            // live in attach mode too.
            Overlay::Accounts => Line::from(vec![
                Span::raw(" accounts — "),
                key("s"),
                Span::raw(" switch  "),
                key("a"),
                Span::raw(" add  "),
                key("n"),
                Span::raw(" login  "),
                key("r"),
                Span::raw(" remove  "),
                key("Esc"),
                Span::raw(" back  "),
                key("q"),
                Span::raw(" quit"),
            ]),
            // Stats overlay: navigation + window cycle + back, regardless of
            // attach mode. `w` toggles the heatmap window (issue #23).
            Overlay::Stats => Line::from(vec![
                Span::raw(" stats — "),
                key("g/Esc"),
                Span::raw(" back  "),
                key("↑/k ↓/j"),
                Span::raw(" model  "),
                key("PgUp/PgDn"),
                Span::raw(" page  "),
                key("w"),
                Span::raw(" window  "),
                key("q"),
                Span::raw(" quit"),
            ]),
            // Logs overlay: full-screen tail; l/Esc back.
            Overlay::Logs => Line::from(vec![
                Span::raw(" logs — "),
                key("l/Esc"),
                Span::raw(" back  "),
                key("q"),
                Span::raw(" quit"),
            ]),
            // Sessions overlay (issue #34): navigation + back.
            Overlay::Sessions => Line::from(vec![
                Span::raw(" sessions — "),
                key("s/Esc"),
                Span::raw(" back  "),
                key("↑/k ↓/j"),
                Span::raw(" session  "),
                key("PgUp/PgDn"),
                Span::raw(" page  "),
                key("q"),
                Span::raw(" quit"),
            ]),
        },
        Mode::Select { .. } => Line::from(vec![
            Span::raw(" "),
            key("↑/k ↓/j"),
            Span::raw(" move  "),
            key("Enter"),
            Span::raw(" switch  "),
            key("n"),
            Span::raw(" new login  "),
            key("Esc"),
            Span::raw(" cancel"),
        ]),
        // The typed key is shown ONLY as a masked width — never the raw
        // characters (AGENTS.md credential rule).
        Mode::AddKey => Line::from(vec![
            Span::raw(" add account — key: "),
            Span::styled(
                "•".repeat(chrome.add_input_len),
                Style::new().fg(Color::Cyan),
            ),
            Span::raw("  "),
            key("Enter"),
            Span::raw(" add  "),
            key("Esc"),
            Span::raw(" cancel"),
        ]),
        Mode::ConfirmRemove { .. } => Line::from(vec![
            Span::raw(" "),
            key("↑/k ↓/j"),
            Span::raw(" pick  "),
            Span::styled("remove selected? ", Style::new().fg(Color::Red)),
            key("y"),
            Span::raw(" confirm  "),
            key("Esc/n"),
            Span::raw(" cancel"),
        ]),
        Mode::NewLogin { idx } => {
            // Provider picker: the cursor row is shown highlighted; Enter
            // opens the browser for that provider.
            let mut spans = vec![Span::raw(" new login — ")];
            for (i, kind) in super::LoginKind::ALL.iter().enumerate() {
                let label = kind.label();
                if i == idx {
                    spans.push(Span::styled(
                        format!("[{label}]"),
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::styled(format!(" {label} "), dim()));
                }
                spans.push(Span::raw(" "));
            }
            spans.push(Span::raw(" "));
            spans.push(key("↑↓"));
            spans.push(Span::raw(" pick  "));
            spans.push(key("Enter"));
            spans.push(Span::raw(" open  "));
            spans.push(key("Esc"));
            spans.push(Span::raw(" cancel"));
            Line::from(spans)
        }
    };
    frame.render_widget(Paragraph::new(vec![status, keybar]), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::PoolSnapshot;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::{BTreeMap, HashMap};

    fn model_row(group: &str, model: &str, tokens_in: u64, tokens_out: u64) -> ModelUsageDoc {
        ModelUsageDoc {
            group: group.into(),
            model: model.into(),
            requests: 3,
            ok: 3,
            errors: 0,
            tokens_in,
            tokens_out,
            cache_read: Some(40_000),
            cache_creation: None,
            last_used_ms: 0,
            in_flight: 0,
            accounts: Vec::new(),
            efforts: Vec::new(),
            endpoints: Vec::new(),
        }
    }

    fn view_with(model_usage: Vec<ModelUsageDoc>) -> DashboardView {
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
                current: BTreeMap::new(),
            },
            last_switch: None,
            poll_health: HashMap::new(),
            session_totals: HashMap::new(),
            global_totals: super::super::activity::Totals::default(),
            rpm_5m: 0.0,
            in_flight: Vec::new(),
            completed: Vec::new(),
            logs: Vec::new(),
            model_usage,
            client_usage: Vec::new(),
            windowed: Vec::new(),
            codex: crate::dashboard::CodexSettingsDoc::default(),
        }
    }

    /// Chrome with a given overlay active and `Mode::Normal` (issue #5). The
    /// old `chrome(show_models)` builder mapped `true`→Stats; tests now name the
    /// overlay explicitly.
    fn chrome_overlay(overlay: Overlay) -> Chrome {
        Chrome {
            frame: 0,
            mode: Mode::Normal,
            overlay,
            status_line: None,
            activity_scroll: 0,
            expanded_activity: None,
            model_cursor: 0,
            stats_window: super::super::activity::StatsWindow::default(),
            sessions: Vec::new(),
            session_cursor: 0,
            add_input_len: 0,
            attach: None,
        }
    }

    fn render(view: &DashboardView, chrome: &Chrome, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        let mut hits = None;
        terminal
            .draw(|f| draw(f, Some(view), chrome, &mut hits))
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn compact_strip_shows_top_model_and_keybar_advertises_view() {
        let view = view_with(vec![model_row("codex", "gpt-5.5", 700, 300)]);
        // MAIN (overlay=None): the compact strip is part of MAIN and the keybar
        // advertises the stats overlay shortcut (req12/req30, adapted to #5).
        let text = render(&view, &chrome_overlay(Overlay::None), 160, 30);
        assert!(
            text.contains("stats"),
            "keybar advertises the stats overlay"
        );
        assert!(text.contains("gpt-5.5"), "strip shows the top model");
    }

    #[test]
    fn detailed_view_lists_all_model_rows_and_drilldown() {
        let view = view_with(vec![
            model_row("codex", "gpt-5.5", 700, 300),
            model_row("claude", "claude-sonnet-4-5", 100, 50),
        ]);
        // The Stats overlay (was the `show_models` full view) still lists all
        // model rows + the drill-down (req13).
        let text = render(&view, &chrome_overlay(Overlay::Stats), 160, 30);
        assert!(text.contains("gpt-5.5"));
        assert!(
            text.contains("claude-sonnet-4-5"),
            "lower rows reachable (req13)"
        );
        assert!(text.contains("model detail"), "drill-down panel present");
    }

    // --- issue #5: MAIN-always + summoned overlays -------------------------

    /// MAIN (overlay=None) shows in-flight + account quota + the model strip,
    /// with NO navigation/overlay surface drawn.
    #[test]
    fn main_shows_inflight_quota_and_strip_without_overlay() {
        let mut view = view_with(vec![model_row("codex", "gpt-5.5", 700, 300)]);
        view.in_flight = vec![super::super::activity::InFlight {
            id: 7,
            method: "POST".into(),
            path: "/v1/messages".into(),
            account: Some("claude:me@example.com".into()),
            group: Some("claude".into()),
            model: Some("claude-opus-4-8".into()),
            started_at: std::time::SystemTime::UNIX_EPOCH,
        }];
        let text = render(&view, &chrome_overlay(Overlay::None), 160, 30);
        assert!(
            text.contains("opus-4-8"),
            "MAIN shows the in-flight session"
        );
        assert!(text.contains("gpt-5.5"), "MAIN shows the model strip");
        // No overlay chrome on MAIN: the Stats drill-down panel is absent.
        assert!(
            !text.contains("model detail"),
            "MAIN draws no overlay surface"
        );
    }

    /// The Stats overlay still renders MAIN underneath (the model strip stays
    /// visible), proving MAIN is drawn first every frame.
    #[test]
    fn stats_overlay_keeps_main_underneath() {
        let view = view_with(vec![model_row("codex", "gpt-5.5", 700, 300)]);
        let text = render(&view, &chrome_overlay(Overlay::Stats), 160, 30);
        assert!(text.contains("model detail"), "stats overlay drawn on top");
        assert!(
            text.contains("gpt-5.5"),
            "MAIN model data still visible underneath the overlay"
        );
    }

    /// The Stats overlay's windowed heatmap (issue #23) renders the selected
    /// window's cells AND a visible best-effort qualifier (accuracy contract).
    #[test]
    fn stats_overlay_renders_windowed_heatmap_with_best_effort_label() {
        let mut view = view_with(vec![model_row("codex", "gpt-5.5", 700, 300)]);
        view.windowed = vec![
            crate::dashboard::WindowedStatsDoc {
                window: "24h".into(),
                window_secs: 86_400,
                cells: vec![crate::dashboard::WindowedCellDoc {
                    group: "codex".into(),
                    model: "gpt-5.5".into(),
                    account: "z@2lab.ai".into(),
                    requests: 12,
                    ok: 11,
                    errors: 1,
                    tokens_in: 700,
                    tokens_out: 300,
                    cache_read: 120,
                    cache_creation: 0,
                    tokens: 1_120,
                }],
            },
            crate::dashboard::WindowedStatsDoc {
                window: "72h".into(),
                window_secs: 259_200,
                cells: Vec::new(),
            },
        ];
        let text = render(&view, &chrome_overlay(Overlay::Stats), 160, 40);
        assert!(text.contains("heatmap"), "heatmap panel titled");
        assert!(
            text.contains("best-effort"),
            "accuracy contract: best-effort qualifier visible"
        );
        assert!(text.contains("z@2lab.ai"), "per-account axis rendered");
        // The keybar advertises the window-cycle key.
        assert!(text.contains("window"), "footer advertises w window cycle");
    }

    /// The Logs overlay shows the log tail.
    #[test]
    fn logs_overlay_shows_the_log_tail() {
        let mut view = view_with(Vec::new());
        view.logs = vec![crate::logging::LogLine {
            level: tracing::Level::INFO,
            text: "proxy started on :3456".into(),
        }];
        let text = render(&view, &chrome_overlay(Overlay::Logs), 160, 30);
        assert!(text.contains("logs"), "logs overlay titled");
        assert!(
            text.contains("proxy started on :3456"),
            "logs overlay shows the tail"
        );
    }

    /// Sessions overlay (issue #34): renders the folded session list with the
    /// confidence label, the user_id, and the per-session aggregates.
    #[test]
    fn sessions_overlay_shows_session_rows_with_confidence_label() {
        use crate::session::{Confidence, Session};
        let view = view_with(Vec::new());
        let mut chrome = chrome_overlay(Overlay::Sessions);
        chrome.sessions = vec![
            Session {
                user_id: Some("u-active".into()),
                requests: 12,
                tokens_in: 3400,
                tokens_out: 1200,
                models: vec!["claude-sonnet-4".into(), "claude-opus-4".into()],
                accounts: vec!["acct-a".into(), "acct-b".into()],
                account_rotations: 3,
                first_ms: 1_000_000,
                last_ms: 1_600_000,
                confidence: Confidence::High,
            },
            Session {
                user_id: None,
                requests: 1,
                tokens_in: 0,
                tokens_out: 0,
                models: vec![],
                accounts: vec!["acct-c".into()],
                account_rotations: 0,
                first_ms: 2_000_000,
                last_ms: 2_000_000,
                confidence: Confidence::Low,
            },
        ];
        let text = render(&view, &chrome, 160, 30);
        assert!(text.contains("sessions"), "sessions overlay titled");
        assert!(text.contains("u-active"), "shows the user_id grouping key");
        assert!(text.contains("high"), "shows the High confidence label");
        assert!(
            text.contains("low") || text.contains("(ungrouped)"),
            "shows the ungrouped Low bucket"
        );
    }

    /// An empty timeline (no captured raw-io) renders the hint, not a crash.
    #[test]
    fn sessions_overlay_empty_shows_hint() {
        let view = view_with(Vec::new());
        let chrome = chrome_overlay(Overlay::Sessions);
        let text = render(&view, &chrome, 160, 30);
        assert!(text.contains("no sessions yet"), "empty hint shown");
    }

    #[test]
    fn activity_meta_abbreviates_claude_prefix_only(/* issue #2, 2b */) {
        // Claude models drop the redundant `claude-` prefix.
        assert_eq!(
            activity_meta(Some("claude"), Some("claude-opus-4-8"), None).as_deref(),
            Some("[claude opus-4-8]")
        );
        // Codex/gpt models are unchanged.
        assert_eq!(
            activity_meta(Some("codex"), Some("gpt-5.5"), Some("high")).as_deref(),
            Some("[codex gpt-5.5·high]")
        );
        // A claude model without the prefix, and unknown groups, pass through.
        assert_eq!(
            activity_meta(Some("claude"), Some("opus-4-8"), None).as_deref(),
            Some("[claude opus-4-8]")
        );
        assert_eq!(
            activity_meta(None, Some("claude-haiku-4-5"), None).as_deref(),
            Some("[claude-haiku-4-5]"),
            "no group → no claude- stripping"
        );
        // Nothing known → no badge.
        assert_eq!(activity_meta(None, None, None), None);
    }

    #[test]
    fn in_flight_row_shows_abbreviated_model_badge(/* issue #2, 2a */) {
        let mut view = view_with(Vec::new());
        view.in_flight = vec![super::super::activity::InFlight {
            id: 1,
            method: "POST".into(),
            path: "/v1/messages".into(),
            account: Some("claude:me@example.com".into()),
            group: Some("claude".into()),
            model: Some("claude-opus-4-8".into()),
            started_at: std::time::SystemTime::UNIX_EPOCH,
        }];
        let text = render(&view, &chrome_overlay(Overlay::None), 160, 30);
        assert!(
            text.contains("opus-4-8"),
            "in-flight row shows the model name (2a)"
        );
        assert!(
            !text.contains("claude-opus-4-8"),
            "model label is abbreviated, not the raw claude- id (2b)"
        );
    }

    // --- Feature A: cost display -------------------------------------------

    #[test]
    fn format_cost_decimal_scheme() {
        // Exactly zero → fixed 4-decimal sentinel.
        assert_eq!(format_cost(0.0), "$0.0000");
        // Sub-dollar → 4 decimals so small per-request costs stay legible.
        assert_eq!(format_cost(0.0123), "$0.0123");
        assert_eq!(format_cost(0.999_94), "$0.9999");
        // ≥ $1 → 2 decimals.
        assert_eq!(format_cost(1.0), "$1.00");
        assert_eq!(format_cost(3.775), "$3.77"); // round-half-to-even (banker's)
        assert_eq!(format_cost(12.5), "$12.50");
    }

    #[test]
    fn model_cost_matches_pricing_table() {
        // opus: 5/25/0.5/6.25 per 1e6 → 200k in (1.0) + 100k out (2.5)
        //   + 40k cache_read (0.02) = 3.52.
        let mut m = model_row("claude", "claude-opus-4-8", 200_000, 100_000);
        m.cache_read = Some(40_000);
        m.cache_creation = None;
        let cost = model_cost(&m);
        assert!((cost - (1.0 + 2.5 + 0.02)).abs() < 1e-9, "got {cost}");
        assert_eq!(format_cost(cost), "$3.52");
    }

    fn completed_request(
        at_ms: u64,
        group: Option<&str>,
        model: Option<&str>,
        input: u64,
        output: u64,
        status: u16,
    ) -> Completed {
        Completed {
            at: UNIX_EPOCH + Duration::from_millis(at_ms),
            body: CompletedBody::Request {
                method: "POST".into(),
                path: "/v1/messages".into(),
                account: Some("a@x.com".into()),
                status,
                duration: Duration::from_millis(1_400),
                tokens: Some(crate::tui::TokenCounts {
                    input,
                    output,
                    ..Default::default()
                }),
                group: group.map(str::to_string),
                model: model.map(str::to_string),
                effort: None,
            },
        }
    }

    #[test]
    fn activity_row_shows_cost() {
        let mut view = view_with(Vec::new());
        // opus: 1M input = $5.00; rendered inline after the tok count.
        view.completed = vec![completed_request(
            1_000,
            Some("claude"),
            Some("claude-opus-4-8"),
            1_000_000,
            0,
            200,
        )];
        let text = render(&view, &chrome_overlay(Overlay::None), 200, 40);
        assert!(text.contains("$5.00"), "activity row shows the $ cost");
    }

    #[test]
    fn models_strip_and_table_show_cost_column() {
        // No cache tokens so the cost is exactly the input rate (gpt-5.5: $5/1M).
        let mut row = model_row("codex", "gpt-5.5", 1_000_000, 0);
        row.cache_read = None;
        let view = view_with(vec![row]);
        // gpt-5.5 input = $5.00, in the MAIN compact strip.
        let main = render(&view, &chrome_overlay(Overlay::None), 200, 40);
        assert!(main.contains("$5.00"), "compact strip shows the $ cost");
        // And in the full table (Stats overlay).
        let stats = render(&view, &chrome_overlay(Overlay::Stats), 200, 40);
        assert!(
            stats.contains("$5.00"),
            "full models table shows the $ cost"
        );
    }

    // --- Feature B: hit-testing + expand -----------------------------------

    fn key_of(entry: &Completed) -> ActivityKey {
        entry.activity_key().expect("request entry has a key")
    }

    #[test]
    fn hit_test_activity_maps_row_to_entry_and_ignores_outside() {
        let area = Rect {
            x: 0,
            y: 10,
            width: 80,
            height: 10,
        };
        let k1 = ActivityKey {
            at_ms: 1,
            method: "POST".into(),
            path: "/a".into(),
            status: 200,
        };
        let k2 = ActivityKey {
            at_ms: 2,
            method: "POST".into(),
            path: "/b".into(),
            status: 200,
        };
        // Entry 1 occupies rows 11..14 (expanded: 3 rows), entry 2 is row 14.
        let chrome = ActivityChrome {
            area,
            hits: vec![
                ActivityHit {
                    key: k1.clone(),
                    y_start: 11,
                    height: 3,
                },
                ActivityHit {
                    key: k2.clone(),
                    y_start: 14,
                    height: 1,
                },
            ],
        };
        // Clicks within entry 1's row span (any of 11,12,13) map to k1.
        assert_eq!(hit_test_activity(&chrome, 5, 11), Some(k1.clone()));
        assert_eq!(hit_test_activity(&chrome, 5, 13), Some(k1));
        // Row 14 → entry 2.
        assert_eq!(hit_test_activity(&chrome, 5, 14), Some(k2));
        // The title/border row (y=10) and below the last entry map to nothing.
        assert_eq!(hit_test_activity(&chrome, 5, 10), None);
        assert_eq!(hit_test_activity(&chrome, 5, 15), None);
        // Outside the panel horizontally / vertically → None.
        assert_eq!(hit_test_activity(&chrome, 99, 12), None);
        assert_eq!(hit_test_activity(&chrome, 5, 0), None);
    }

    #[test]
    fn click_expand_recorded_layout_round_trips_to_detail() {
        // Render once to capture the hit layout, find the row a click lands on,
        // set that key expanded, and re-render: the detail lines appear.
        let entry = completed_request(
            7_000,
            Some("claude"),
            Some("claude-opus-4-8"),
            200_000,
            100_000,
            200,
        );
        let key = key_of(&entry);
        let mut view = view_with(Vec::new());
        view.completed = vec![entry];

        // Capture layout (collapsed).
        let mut hits = None;
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("terminal");
        let chrome = chrome_overlay(Overlay::None);
        terminal
            .draw(|f| draw(f, Some(&view), &chrome, &mut hits))
            .expect("draw");
        let layout = hits.expect("activity layout recorded");
        assert!(!layout.hits.is_empty(), "the request row is a hit target");
        let hit = &layout.hits[0];
        // A click on the row's first line maps back to the same key.
        assert_eq!(
            hit_test_activity(&layout, layout.area.x + 1, hit.y_start),
            Some(key.clone())
        );

        // Now render expanded and confirm the detail lines show.
        let mut expanded_chrome = chrome_overlay(Overlay::None);
        expanded_chrome.expanded_activity = Some(key);
        let text = render(&view, &expanded_chrome, 200, 40);
        assert!(text.contains("cache_read"), "expanded detail shows tokens");
        assert!(
            text.contains("$"),
            "expanded detail shows per-component cost"
        );
        assert!(text.contains('▾'), "expanded row shows the open marker");
    }

    #[test]
    fn notes_are_not_expandable_hit_targets() {
        let mut view = view_with(Vec::new());
        view.completed = vec![Completed {
            at: UNIX_EPOCH + Duration::from_millis(1),
            body: CompletedBody::Note {
                text: "switch a → b".into(),
                error: false,
            },
        }];
        let mut hits = None;
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("terminal");
        let chrome = chrome_overlay(Overlay::None);
        terminal
            .draw(|f| draw(f, Some(&view), &chrome, &mut hits))
            .expect("draw");
        let layout = hits.expect("layout");
        assert!(
            layout.hits.is_empty(),
            "a note line is not a clickable hit target"
        );
    }
}
