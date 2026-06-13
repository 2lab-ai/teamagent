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

use crate::logging::LogLine;
use crate::scheduler::select::IneligibleReason;
use crate::scheduler::window::QuotaWindow;
use crate::scheduler::{select, AccountSnapshot};

use super::activity::{Completed, CompletedBody};
use super::format::{self, GaugeLevel};
use super::view::DashboardView;
use super::{Chrome, Mode};

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const GAUGE_BAR_WIDTH: usize = 8;
/// Width at/above which the accounts table shows the wide column set
/// (type, absolute reset times, lifetime req/tok).
const WIDE_TABLE_AT: u16 = 150;
/// Width at/above which the middle row fits summary + detail side by side.
const SIDE_BY_SIDE_AT: u16 = 110;

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
    };
    let table_height = (snapshot.accounts.len().max(1) as u16).saturating_add(2);
    // Log console sits at the bottom, between activity and the keybar.
    let logs_height = chrome.log_panel.height();
    let [header_area, table_area, middle_area, activity_area, logs_area, footer_area] =
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(table_height),
            Constraint::Length(8),
            Constraint::Min(3),
            Constraint::Length(logs_height),
            Constraint::Length(2),
        ])
        .areas(frame.area());

    draw_header(frame, header_area, view, chrome);
    draw_accounts(frame, table_area, view, &ctx, chrome);
    draw_middle(frame, middle_area, view, &ctx, chrome);
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
            " teamagent ",
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
            " teamagent ",
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
            "no accounts — run `teamagent login` or `teamagent import`, then press R",
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

    let (header, constraints): (Vec<&'static str>, Vec<Constraint>) = if wide {
        (
            vec![
                "", "#", "account", "type", "status", "5h", "reset", "7d", "reset", "token", "if",
                "req", "tok",
            ],
            vec![
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Fill(1),
                Constraint::Length(6),
                Constraint::Length(18),
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
                "", "#", "account", "status", "5h", "reset", "7d", "reset", "token", "if",
            ],
            vec![
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Fill(1),
                Constraint::Length(18),
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

    let mut cells = vec![
        Cell::from(marker),
        Cell::from(Span::styled(format!("{}", pos + 1), dim())),
        Cell::from(name),
    ];
    if wide {
        cells.push(Cell::from(Span::styled(account.credential_kind, dim())));
    }
    cells.push(Cell::from(status_span(
        account, gate, is_current, params, now,
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
) -> Span<'static> {
    let Some(reason) = gate else {
        return if is_current {
            Span::styled(
                "active",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("ready")
        };
    };
    let text = select::blocking_reason(account, reason, params, now);
    let style = match reason {
        IneligibleReason::AuthUnhealthy
        | IneligibleReason::FiveHourOverThreshold
        | IneligibleReason::SevenDayOverThreshold => Style::new().fg(Color::Red),
        IneligibleReason::CoolingDown => Style::new().fg(Color::Yellow),
        IneligibleReason::UsageStale => dim(),
    };
    Span::styled(text, style)
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

/// One quota window → (gauge cell, reset cell). The gauge label is the
/// percentage normally, or the reset countdown when the account is parked /
/// the window is over its threshold (FR6). The reset cell shows the
/// countdown, plus the absolute local time in wide mode ("1h02m (14:30)").
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
    let reset = format::countdown_until(Some(window.resets_at), now);
    let color = level_color(format::gauge_level(utilization));
    let label = if (parked || utilization > threshold) && reset.is_some() {
        reset.clone().unwrap_or_default()
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

    // current + WHY (last committed switch).
    let mut current_spans = vec![label("current")];
    match snapshot.representative_current() {
        Some(current) => {
            current_spans.push(Span::styled(
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
                current_spans.push(Span::styled(format!("  {from}{why}, {ago} ago"), dim()));
            }
        }
        None => current_spans.push(Span::styled("(none)", Style::new().fg(Color::Red))),
    }
    lines.push(Line::from(current_spans));

    // next in line + next evaluation tick (approximate — the tick task and
    // the TUI start together; jitter is one render tick).
    let next = next_in_line(view, ctx);
    let tick = view.evaluate_tick.as_secs().max(1);
    let to_next_eval = tick - (view.uptime.as_secs() % tick);
    lines.push(Line::from(vec![
        label("next"),
        Span::raw(next.unwrap_or_else(|| "—".into())),
        Span::styled(
            format!(
                "  eval in ~{}",
                select::compact_duration(Duration::from_secs(to_next_eval))
            ),
            dim(),
        ),
    ]));

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

    let block = Block::new().borders(Borders::TOP).title(" scheduler ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Name of the first eligible non-current account in selection order —
/// exactly the account `pick` would switch to next.
fn next_in_line(view: &DashboardView, ctx: &FrameCtx) -> Option<String> {
    let snapshot = &view.snapshot;
    let params = &view.select_params;
    ctx.order
        .iter()
        .map(|&i| &snapshot.accounts[i])
        .filter(|a| !snapshot.is_current(&a.id))
        .find(|a| select::eligibility(a, params, ctx.now, ctx.headers_only).is_none())
        .map(|a| a.id.to_string())
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
        status_span(account, gate, is_current, params, now),
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
    let title = if in_flight.is_empty() {
        " activity ".to_string()
    } else {
        format!(" activity — {} in flight ", in_flight.len())
    };
    let block = Block::new().borders(Borders::TOP).title(title);
    let capacity = area.height.saturating_sub(1) as usize; // top border

    let spinner = SPINNER[chrome.frame % SPINNER.len()];
    let mut lines: Vec<Line> = Vec::with_capacity(capacity);
    // In-flight rows pinned on top (newest start first), spinner + elapsed.
    for request in in_flight.iter().rev().take(capacity) {
        let elapsed = now.duration_since(request.started_at).unwrap_or_default();
        let mut spans = vec![
            Span::styled(format!(" {spinner} "), Style::new().fg(Color::Cyan)),
            Span::styled(format::clock_hms_utc(request.started_at), dim()),
            Span::raw(format!("  {} {}", request.method, request.path)),
        ];
        if let Some(account) = &request.account {
            spans.push(Span::raw(format!(" → {account}")));
        }
        spans.push(Span::styled(
            format!(" ({}…)", format::elapsed_secs(elapsed)),
            dim(),
        ));
        lines.push(Line::from(spans));
    }
    // Completed entries, newest first.
    for entry in view
        .completed
        .iter()
        .take(capacity.saturating_sub(lines.len()))
    {
        lines.push(completed_line(entry));
    }
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
            Line::from(vec![
                stamp,
                Span::raw(format!("{method} {path} → {account} (")),
                Span::styled(status.to_string(), status_style),
                Span::raw(format!(", {detail})")),
            ])
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
        Mode::Normal if attached => Line::from(vec![
            Span::raw(" "),
            key("q"),
            Span::raw(" quit  "),
            key("s"),
            Span::raw(" switch  "),
            key("d"),
            Span::raw(" detail  "),
            key("l"),
            Span::raw(" logs  "),
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
            key("a"),
            Span::raw(" add  "),
            key("r"),
            Span::raw(" remove  "),
            key("R"),
            Span::raw(" reload  "),
            key("l"),
            Span::raw(" logs"),
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
    };
    frame.render_widget(Paragraph::new(vec![status, keybar]), area);
}
