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
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::dashboard::ModelUsageDoc;
use crate::logging::LogLine;
use crate::routing::BackendGroup;
use crate::scheduler::select::IneligibleReason;
use crate::scheduler::window::QuotaWindow;
use crate::scheduler::{select, AccountSnapshot};

use super::activity::{Completed, CompletedBody};
use super::format::{self, GaugeLevel};
use super::view::DashboardView;
use super::{anim, Chrome, Mode};

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
/// A model used within this window counts as "recently active" (req15).
const MODEL_RECENT_WINDOW: Duration = Duration::from_secs(60);

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
pub(crate) fn draw(frame: &mut Frame, view: Option<&DashboardView>, chrome: &Chrome) {
    let Some(view) = view else {
        draw_connecting(frame, chrome);
        return;
    };

    let snapshot = &view.snapshot;
    let now = SystemTime::now();
    let ctx = FrameCtx {
        now,
        tz_offset: format::local_offset_secs(now),
        order: view.display_order(now),
        headers_only: select::headers_only_mode(snapshot, &view.select_params, None, now),
        frame: chrome.frame,
    };
    let table_height = (snapshot.accounts.len().max(1) as u16).saturating_add(2);

    // Detailed model-usage view (req13): keep the header + account table for
    // context, then give the rest of the screen to the full model table +
    // drill-down. The account quota table stays the priority surface above it.
    if chrome.show_models {
        let [header_area, table_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(table_height),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .areas(frame.area());
        draw_header(frame, header_area, view, chrome);
        draw_accounts(frame, table_area, view, &ctx, chrome);
        draw_models_full(frame, body_area, view, &ctx, chrome);
        draw_footer(frame, footer_area, chrome);
        return;
    }

    // Log console sits at the bottom, between activity and the keybar.
    let logs_height = chrome.log_panel.height();
    // Compact model strip (req12): only when model data exists. 0 height (no
    // pane) otherwise, so the idle layout is unchanged.
    let strip_rows = view.model_usage.len().min(MODEL_STRIP_ROWS);
    // +2 for the table's top border (title) and header row.
    let strip_height = if strip_rows > 0 {
        strip_rows as u16 + 2
    } else {
        0
    };
    let [header_area, table_area, middle_area, strip_area, activity_area, logs_area, footer_area] =
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(table_height),
            Constraint::Length(8),
            Constraint::Length(strip_height),
            Constraint::Min(3),
            Constraint::Length(logs_height),
            Constraint::Length(2),
        ])
        .areas(frame.area());

    draw_header(frame, header_area, view, chrome);
    draw_accounts(frame, table_area, view, &ctx, chrome);
    draw_middle(frame, middle_area, view, &ctx, chrome);
    if strip_height > 0 {
        draw_models_strip(frame, strip_area, view, now);
    }
    draw_activity(frame, activity_area, view, chrome, now);
    if logs_height > 0 {
        draw_logs(frame, logs_area, view);
    }
    draw_footer(frame, footer_area, chrome);
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
        Mode::Select { idx } => Some(idx.min(ctx.order.len().saturating_sub(1))),
        Mode::Normal => None,
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
    let (five_gauge, five_reset) = window_cells(
        &account.five_hour,
        params.five_hour_max,
        parked,
        now,
        ctx.tz_offset,
        wide,
    );
    let (seven_gauge, seven_reset) = window_cells(
        &account.seven_day,
        params.seven_day_max,
        parked,
        now,
        ctx.tz_offset,
        wide,
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
fn window_cells(
    window: &Option<QuotaWindow>,
    threshold: f64,
    parked: bool,
    now: SystemTime,
    tz_offset: i64,
    wide: bool,
) -> (Cell<'static>, Cell<'static>) {
    let Some(window) = window else {
        return (
            Cell::from(Span::styled("—", dim())),
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
    let gauge = Cell::from(Line::from(vec![
        Span::styled(
            format::gauge_bar(utilization, GAUGE_BAR_WIDTH),
            Style::new().fg(color),
        ),
        Span::raw(" "),
        Span::styled(label, Style::new().fg(color)),
    ]));
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
/// detail pane beside it when there is room (toggled by `d`).
fn draw_middle(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
) {
    let show_detail = chrome.show_detail && !view.snapshot.accounts.is_empty();
    if show_detail && area.width >= SIDE_BY_SIDE_AT {
        let [summary_area, detail_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(48)]).areas(area);
        draw_summary(frame, summary_area, view, ctx);
        draw_detail(frame, detail_area, view, ctx, chrome);
    } else if show_detail && area.width < SIDE_BY_SIDE_AT {
        // Too narrow for both: `d` flips between summary and detail.
        draw_detail(frame, area, view, ctx, chrome);
    } else {
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
        Mode::Select { idx } => idx.min(ctx.order.len().saturating_sub(1)),
        Mode::Normal => snapshot
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
    lines.push(window_detail_line("5h", &account.five_hour, ctx));
    lines.push(window_detail_line("7d", &account.seven_day, ctx));
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
) -> Line<'static> {
    let label = Span::styled(format!(" {name:<5} "), dim());
    let Some(window) = window else {
        return Line::from(vec![label, Span::styled("no data (cold)", dim())]);
    };
    let now = ctx.now;
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
    Line::from(vec![
        label,
        Span::styled(format::percent(utilization), Style::new().fg(color)),
        Span::raw(format!(" · resets {reset}")),
        Span::styled(format!(" · {source} {age} ago"), dim()),
    ])
}

fn draw_activity(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    chrome: &Chrome,
    now: SystemTime,
) {
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
    let total = view.completed.len();
    let completed_rows = capacity.saturating_sub(lines.len());
    let scroll = chrome.activity_scroll.min(total.saturating_sub(1).max(0));
    for entry in view.completed.iter().skip(scroll).take(completed_rows) {
        lines.push(completed_line(entry));
    }

    // Title carries the scroll position so it's obvious you're in history.
    let shown_last = (scroll + completed_rows).min(total);
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
}

fn completed_line(entry: &Completed) -> Line<'static> {
    let stamp = Span::styled(format!("   {}  ", format::clock_hms_utc(entry.at)), dim());
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
            let account = account.as_deref().unwrap_or("?");
            let status_style = if *status < 400 {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::Red)
            };
            let mut detail = format::elapsed_secs(*duration);
            if let Some(tokens) = tokens {
                detail.push_str(&format!(", {} tok", format::human_count(tokens.total())));
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
            let style = if *error {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            Line::from(vec![stamp, Span::styled(text.clone(), style)])
        }
    }
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
        let mut last = model_age_compact(m.last_used_ms, now);
        if m.in_flight > 0 {
            last = format!("{} in-flight", m.in_flight);
        }
        cells.push(Cell::from(Span::styled(last, dim())));
        Row::new(cells)
    });

    let (header, constraints): (Vec<&'static str>, Vec<Constraint>) = if wide {
        (
            vec!["", "group", "model", "share", "req", "tok", "last"],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(MODEL_BAR_WIDTH as u16 + 1),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(12),
            ],
        )
    } else {
        (
            vec!["", "group", "model", "req", "tok", "last"],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(7),
                Constraint::Length(8),
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
                "", "group", "model", "req", "ok/err", "in", "out", "cache", "last", "if",
            ],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(3),
            ],
        )
    } else {
        (
            vec!["", "group", "model", "req", "ok/err", "in", "out", "if"],
            vec![
                Constraint::Length(2),
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(8),
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
    let keybar = if chrome.show_models {
        // Detailed model view: navigation + back, regardless of attach mode.
        Line::from(vec![
            Span::raw(" "),
            key("g/Esc"),
            Span::raw(" back  "),
            key("↑/k ↓/j"),
            Span::raw(" model  "),
            key("PgUp/PgDn"),
            Span::raw(" page  "),
            key("l"),
            Span::raw(" logs  "),
            key("q"),
            Span::raw(" quit"),
        ])
    } else {
        match chrome.mode {
            Mode::Normal if attached => Line::from(vec![
                Span::raw(" "),
                key("q"),
                Span::raw(" quit  "),
                key("s"),
                Span::raw(" switch  "),
                key("d"),
                Span::raw(" detail  "),
                key("g"),
                Span::raw(" models  "),
                key("l"),
                Span::raw(" logs  "),
                key("f/m/e"),
                Span::raw(" codex  "),
                key("↑↓"),
                Span::raw(" scroll  "),
                Span::styled("a/r/R disabled (attached)", dim()),
            ]),
            Mode::Normal => Line::from(vec![
                Span::raw(" "),
                key("q"),
                Span::raw(" quit  "),
                key("s"),
                Span::raw(" switch  "),
                key("d"),
                Span::raw(" detail  "),
                key("g"),
                Span::raw(" models  "),
                key("a"),
                Span::raw(" add  "),
                key("r"),
                Span::raw(" remove  "),
                key("R"),
                Span::raw(" reload  "),
                key("l"),
                Span::raw(" logs  "),
                key("f/m/e"),
                Span::raw(" codex  "),
                key("↑↓"),
                Span::raw(" scroll"),
            ]),
            Mode::Select { .. } => Line::from(vec![
                Span::raw(" "),
                key("↑/k ↓/j"),
                Span::raw(" move  "),
                key("Enter"),
                Span::raw(" switch  "),
                key("Esc"),
                Span::raw(" cancel"),
            ]),
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
            codex: crate::dashboard::CodexSettingsDoc::default(),
        }
    }

    fn chrome(show_models: bool) -> Chrome {
        Chrome {
            frame: 0,
            mode: Mode::Normal,
            show_detail: false,
            log_panel: super::super::logs::LogPanelSize::Small,
            status_line: None,
            activity_scroll: 0,
            show_models,
            model_cursor: 0,
            attach: None,
        }
    }

    fn render(view: &DashboardView, chrome: &Chrome, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal
            .draw(|f| draw(f, Some(view), chrome))
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
        let text = render(&view, &chrome(false), 160, 30);
        // Discoverability (req30) + the compact strip's top-model label (req12).
        assert!(text.contains("models"), "keybar/strip mentions models");
        assert!(text.contains("gpt-5.5"), "strip shows the top model");
    }

    #[test]
    fn detailed_view_lists_all_model_rows_and_drilldown() {
        let view = view_with(vec![
            model_row("codex", "gpt-5.5", 700, 300),
            model_row("claude", "claude-sonnet-4-5", 100, 50),
        ]);
        let text = render(&view, &chrome(true), 160, 30);
        assert!(text.contains("gpt-5.5"));
        assert!(
            text.contains("claude-sonnet-4-5"),
            "lower rows reachable (req13)"
        );
        assert!(text.contains("model detail"), "drill-down panel present");
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
        let text = render(&view, &chrome(false), 160, 30);
        assert!(
            text.contains("opus-4-8"),
            "in-flight row shows the model name (2a)"
        );
        assert!(
            !text.contains("claude-opus-4-8"),
            "model label is abbreviated, not the raw claude- id (2b)"
        );
    }
}
