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
use crate::scheduler::window::QuotaWindow;
use crate::scheduler::{select, AccountSnapshot};

use super::activity::{Completed, CompletedBody};
use super::format::{self, GaugeLevel};
use super::view::DashboardView;
use super::{anim, Chrome, ExpandKey, Mode, Overlay};

const GAUGE_BAR_WIDTH: usize = 8;
/// Width at/above which the accounts table shows the wide column set
/// (type, absolute reset times, lifetime req/tok).
const WIDE_TABLE_AT: u16 = 150;

/// Width at/above which detail panes fit side by side.
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

/// Format an API-equivalent USD cost for display next to tokens. Sub-dollar
/// amounts get four decimals (`$0.0042`) so small per-request costs stay
/// legible; `$1+` uses two (`$3.78`). Zero/unknown renders `$0.0000` rather
/// than panicking. All TUI cost sites compute via [`crate::pricing`] with an
/// empty overrides map (built-in default rate table, same as the server.log
/// path) — see the call sites.
fn format_cost(usd: f64) -> String {
    if usd >= 1.0 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.4}")
    }
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
    draw_main(frame, view, &ctx, chrome, now);

    // A summoned overlay (if any) is then drawn OVER MAIN. Each overlay clears
    // its own rect with `Clear` so MAIN shows through only outside it; because
    // MAIN was already drawn this frame, "MAIN keeps updating underneath" is
    // automatic.
    match chrome.overlay {
        Overlay::None => {}
        Overlay::Accounts => draw_accounts_overlay(frame, view, &ctx, chrome),
        Overlay::Stats => draw_stats_overlay(frame, view, &ctx, chrome),
        Overlay::Logs => draw_logs_overlay(frame, view),
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

/// MAIN — L0 command center. The full account/model/log inventory still exists
/// behind Accounts/Stats/Logs overlays, but the first screen now shows only the
/// operator-critical rollups, ranked exceptions, L1 domain map, and evidence
/// preview so it fits as a glanceable dashboard.
fn draw_main(
    frame: &mut Frame,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
    now: SystemTime,
) {
    let compact = frame.area().height < 30;
    let strip_rows = view.model_usage.len().min(MODEL_STRIP_ROWS);
    let strip_height = if !compact && strip_rows > 0 {
        strip_rows as u16 + 2
    } else {
        0
    };
    let [header_area, overview_area, domains_area, strip_area, activity_area, footer_area] =
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(if compact { 10 } else { 13 }),
            Constraint::Length(if compact { 0 } else { 7 }),
            Constraint::Length(strip_height),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .areas(frame.area());

    draw_header(frame, header_area, view, chrome);
    draw_overview(frame, overview_area, view, ctx, chrome, now);
    if domains_area.height > 0 {
        draw_domain_map(frame, domains_area, view, ctx);
    }
    if strip_height > 0 {
        draw_models_strip(frame, strip_area, view, now);
    }
    draw_activity(frame, activity_area, view, chrome, now);
    // Footer slot reserved in the layout; the real footer is drawn by `draw`
    // last (over any overlay). Keep MAIN's bottom row clear here.
    let _ = footer_area;
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum OverviewSeverity {
    Ok = 0,
    Stale = 1,
    Warn = 2,
    Crit = 3,
}

impl OverviewSeverity {
    fn badge(self) -> &'static str {
        match self {
            OverviewSeverity::Ok => "OK",
            OverviewSeverity::Stale => "STALE",
            OverviewSeverity::Warn => "WARN",
            OverviewSeverity::Crit => "CRIT",
        }
    }

    fn color(self) -> Color {
        match self {
            OverviewSeverity::Ok => Color::Green,
            OverviewSeverity::Stale => Color::Blue,
            OverviewSeverity::Warn => Color::Yellow,
            OverviewSeverity::Crit => Color::Red,
        }
    }
}

struct OverviewRow {
    severity: OverviewSeverity,
    domain: &'static str,
    entity: String,
    impact: String,
    age: String,
    reason: String,
    next: &'static str,
}

fn compact_age(now: SystemTime, at: SystemTime) -> String {
    now.duration_since(at)
        .map(select::compact_duration)
        .unwrap_or_else(|_| "0s".to_string())
}

fn completed_is_error(entry: &Completed) -> bool {
    match &entry.body {
        CompletedBody::Request { status, .. } => *status >= 400,
        CompletedBody::Note { error, .. } => *error,
    }
}

fn completed_label(entry: &Completed) -> String {
    match &entry.body {
        CompletedBody::Request {
            method,
            path,
            status,
            ..
        } => format!("{method} {path} → {status}"),
        CompletedBody::Note { text, .. } => text.chars().take(48).collect(),
    }
}

fn completed_impact(entry: &Completed) -> String {
    match &entry.body {
        CompletedBody::Request {
            account,
            group,
            model,
            ..
        } => match (account, group, model) {
            (Some(account), Some(group), Some(model)) => format!("{account} · {group}/{model}"),
            (Some(account), _, _) => account.clone(),
            _ => "unrouted".to_string(),
        },
        CompletedBody::Note { .. } => "dashboard note".to_string(),
    }
}

fn overview_rows(view: &DashboardView, ctx: &FrameCtx, now: SystemTime) -> Vec<OverviewRow> {
    let mut rows = Vec::new();
    let accounts = &view.snapshot.accounts;
    let healthy = accounts.iter().filter(|a| a.healthy).count();

    if !accounts.is_empty() && healthy == 0 {
        rows.push(OverviewRow {
            severity: OverviewSeverity::Crit,
            domain: "Accounts",
            entity: "all accounts".to_string(),
            impact: format!("0/{} healthy", accounts.len()),
            age: "now".to_string(),
            reason: "no usable upstream".to_string(),
            next: "press a",
        });
    }

    for account in accounts {
        if !account.healthy {
            rows.push(OverviewRow {
                severity: OverviewSeverity::Warn,
                domain: "Accounts",
                entity: account.id.0.clone(),
                impact: account.credential_kind.to_string(),
                age: "now".to_string(),
                reason: "unhealthy".to_string(),
                next: "press a",
            });
        }
        if let Some(until) = account.cooldown_until {
            if until > now {
                rows.push(OverviewRow {
                    severity: OverviewSeverity::Warn,
                    domain: "Accounts",
                    entity: account.id.0.clone(),
                    impact: "cooldown".to_string(),
                    age: until
                        .duration_since(now)
                        .map(select::compact_duration)
                        .unwrap_or_else(|_| "now".to_string()),
                    reason: "rate limited".to_string(),
                    next: "press a",
                });
            }
        }
    }

    for (name, poll) in &view.poll_health {
        if poll.consecutive_failures > 0 {
            rows.push(OverviewRow {
                severity: OverviewSeverity::Warn,
                domain: "Accounts",
                entity: name.clone(),
                impact: format!("{} poll failures", poll.consecutive_failures),
                age: poll
                    .last_ok
                    .map(|at| compact_age(now, at))
                    .unwrap_or_else(|| "never ok".to_string()),
                reason: "usage poll failing".to_string(),
                next: "press l",
            });
        }
    }

    if ctx.headers_only {
        rows.push(OverviewRow {
            severity: OverviewSeverity::Stale,
            domain: "Routing",
            entity: "scheduler".to_string(),
            impact: "headers-only".to_string(),
            age: "now".to_string(),
            reason: "limited usage evidence".to_string(),
            next: "press g",
        });
    }

    for request in view.in_flight.iter().rev().take(4) {
        rows.push(OverviewRow {
            severity: OverviewSeverity::Ok,
            domain: "Traffic",
            entity: format!("{} {}", request.method, request.path),
            impact: request
                .account
                .clone()
                .unwrap_or_else(|| "routing".to_string()),
            age: compact_age(now, request.started_at),
            reason: "in flight".to_string(),
            next: "watch",
        });
    }

    for entry in view
        .completed
        .iter()
        .filter(|entry| completed_is_error(entry))
        .take(5)
    {
        rows.push(OverviewRow {
            severity: OverviewSeverity::Warn,
            domain: "Incidents",
            entity: completed_label(entry),
            impact: completed_impact(entry),
            age: compact_age(now, entry.at),
            reason: "recent error".to_string(),
            next: "press l",
        });
    }

    if rows.is_empty() {
        rows.push(OverviewRow {
            severity: OverviewSeverity::Ok,
            domain: "Overview",
            entity: "no active exceptions".to_string(),
            impact: format!(
                "{} accounts · {} model rows",
                accounts.len(),
                view.model_usage.len()
            ),
            age: "live".to_string(),
            reason: "healthy glance".to_string(),
            next: "drill down",
        });
    }

    rows.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.domain.cmp(b.domain))
            .then_with(|| a.entity.cmp(&b.entity))
    });
    rows
}

fn draw_overview(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    ctx: &FrameCtx,
    chrome: &Chrome,
    now: SystemTime,
) {
    let rows = overview_rows(view, ctx, now);
    let crit = rows
        .iter()
        .filter(|row| row.severity == OverviewSeverity::Crit)
        .count();
    let warn = rows
        .iter()
        .filter(|row| row.severity == OverviewSeverity::Warn)
        .count();
    let stale = rows
        .iter()
        .filter(|row| row.severity == OverviewSeverity::Stale)
        .count();
    let ok = rows
        .iter()
        .filter(|row| row.severity == OverviewSeverity::Ok)
        .count();
    let healthy = view.snapshot.accounts.iter().filter(|a| a.healthy).count();
    let [summary_area, body_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);

    let summary = Line::from(vec![
        Span::styled(
            " L0 ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" command center  "),
        Span::styled(
            format!("CRIT {crit} "),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("WARN {warn} "),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("STALE {stale} "), Style::new().fg(Color::Blue)),
        Span::styled(format!("OK {ok}"), Style::new().fg(Color::Green)),
        Span::raw(format!(
            "  │ accounts {}/{}",
            healthy,
            view.snapshot.accounts.len()
        )),
        Span::raw(format!("  in-flight {}", view.in_flight.len())),
        Span::raw(format!("  rpm {:.1}", view.rpm_5m)),
        Span::raw(format!(
            "  err {}",
            format::human_count(view.global_totals.errors)
        )),
        Span::raw(format!("  eval {}s", view.evaluate_tick.as_secs().max(1))),
        Span::raw(format!("  models {}", view.model_usage.len())),
        // Global API-equivalent $ across all model rows (built-in default rates).
        Span::raw(format!(
            "  cost {}",
            format_cost(view.model_usage.iter().map(model_cost).sum())
        )),
    ]);
    let subtitle = Line::from(vec![
        Span::styled(" hierarchy ", dim()),
        Span::raw("Overview > Domain > Entity > Evidence"),
        Span::styled("   a accounts · g models · l logs · ↑↓ history", dim()),
    ]);
    frame.render_widget(
        Paragraph::new(vec![summary, subtitle]).block(Block::new().borders(Borders::BOTTOM)),
        summary_area,
    );

    if body_area.width >= 140 {
        let [list_area, preview_area] =
            Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)])
                .areas(body_area);
        draw_exception_table(frame, list_area, &rows, chrome);
        draw_selected_preview(frame, preview_area, &rows, view);
    } else {
        let [list_area, preview_area] = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(if body_area.height > 8 { 4 } else { 0 }),
        ])
        .areas(body_area);
        draw_exception_table(frame, list_area, &rows, chrome);
        if preview_area.height > 0 {
            draw_selected_preview(frame, preview_area, &rows, view);
        }
    }
}

fn draw_exception_table(frame: &mut Frame, area: Rect, rows: &[OverviewRow], chrome: &Chrome) {
    let visible = area.height.saturating_sub(3) as usize;
    let selected = chrome.activity_scroll.min(rows.len().saturating_sub(1));
    let table_rows = rows
        .iter()
        .take(visible.max(1))
        .enumerate()
        .map(|(idx, item)| {
            let style = if idx == selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            Row::new([
                Cell::from(Span::styled(
                    item.severity.badge(),
                    Style::new()
                        .fg(item.severity.color())
                        .add_modifier(Modifier::BOLD),
                )),
                Cell::from(item.domain.to_string()),
                Cell::from(item.entity.clone()),
                Cell::from(item.impact.clone()),
                Cell::from(item.age.clone()),
                Cell::from(item.reason.clone()),
                Cell::from(item.next.to_string()),
            ])
            .style(style)
        });
    let hidden = rows.len().saturating_sub(visible);
    let title = if hidden > 0 {
        format!(" ranked exceptions (+{} hidden) ", hidden)
    } else {
        " ranked exceptions ".to_string()
    };
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(6),
            Constraint::Length(11),
            Constraint::Percentage(25),
            Constraint::Percentage(18),
            Constraint::Length(8),
            Constraint::Percentage(22),
            Constraint::Length(10),
        ],
    )
    .header(Row::new(["sev", "domain", "entity", "impact", "age", "why", "next"]).style(dim()))
    .block(Block::new().borders(Borders::ALL).title(title));
    frame.render_widget(table, area);
}

fn draw_selected_preview(
    frame: &mut Frame,
    area: Rect,
    rows: &[OverviewRow],
    view: &DashboardView,
) {
    let mut lines = Vec::new();
    if let Some(item) = rows.first() {
        lines.push(Line::from(vec![
            Span::styled(
                item.severity.badge(),
                Style::new()
                    .fg(item.severity.color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {} › {}", item.domain, item.entity)),
        ]));
        lines.push(Line::from(format!(
            "why: {} · impact: {}",
            item.reason, item.impact
        )));
        lines.push(Line::from(format!(
            "age: {} · next: {}",
            item.age, item.next
        )));
    }
    lines.push(Line::from(format!(
        "evidence: {} logs · {} completed · config {}",
        view.logs.len(),
        view.completed.len(),
        view.config_path.as_deref().unwrap_or("unknown")
    )));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .title(" selected evidence preview "),
        ),
        area,
    );
}

fn draw_domain_map(frame: &mut Frame, area: Rect, view: &DashboardView, ctx: &FrameCtx) {
    let [tiles_area, pool_area] =
        Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)]).areas(area);
    let domains = [
        ("Traffic", "requests · streams · queue", "activity"),
        ("Accounts", "quota · health · cooldown", "a"),
        ("Models", "usage · tokens · cache", "g"),
        ("Routing", "current · fallback · why", "summary"),
        ("Config", "port · upstream · reload", "R"),
        ("Incidents", "errors · poll failures", "l"),
        ("Evidence", "logs · raw rows · detail", "Enter"),
    ];
    let tile_rows = domains.chunks(2).map(|chunk| {
        let mut cells: Vec<Cell> = chunk
            .iter()
            .map(|(label, scope, key)| {
                Cell::from(Line::from(vec![
                    Span::styled(
                        *label,
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("  {scope}")),
                    Span::styled(format!("  [{key}]"), dim()),
                ]))
            })
            .collect();
        while cells.len() < 2 {
            cells.push(Cell::from(""));
        }
        Row::new(cells)
    });
    let tiles = Table::new(
        tile_rows,
        [Constraint::Percentage(50), Constraint::Percentage(50)],
    )
    .block(Block::new().borders(Borders::ALL).title(" L1 domains "));
    frame.render_widget(tiles, tiles_area);

    let accounts = view.snapshot.accounts.len().max(1);
    let healthy = view.snapshot.accounts.iter().filter(|a| a.healthy).count();
    let filled = ((healthy * 10) / accounts).min(10);
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled));
    let mut lines = vec![
        Line::from(format!("provider pool  {healthy}/{accounts} healthy")),
        Line::from(Span::styled(
            bar,
            Style::new().fg(if healthy == accounts {
                Color::Green
            } else {
                Color::Yellow
            }),
        )),
        Line::from(format!(
            "mode: {}",
            if ctx.headers_only {
                "headers-only"
            } else {
                "usage-aware"
            }
        )),
        Line::from(format!(
            "port: {} · upstream: {}",
            view.port,
            view.upstream.as_deref().unwrap_or("direct")
        )),
    ];
    if let Some(last) = &view.last_switch {
        lines.push(Line::from(format!(
            "last switch: {} → {}",
            last.from.as_deref().unwrap_or("none"),
            last.to
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::new().borders(Borders::ALL).title(" L2 preview ")),
        pool_area,
    );
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
    let [table_area, body_area] =
        Layout::vertical([Constraint::Length(table_height), Constraint::Min(3)]).areas(area);
    draw_accounts(frame, table_area, view, ctx, chrome);
    draw_models_full(frame, body_area, view, ctx, chrome);
}

/// Logs overlay (`l`): a full-screen log tail (was the `l` size-cycle panel).
fn draw_logs_overlay(frame: &mut Frame, view: &DashboardView) {
    let area = overlay_rect(frame.area());
    frame.render_widget(Clear, area);
    draw_logs(frame, area, view);
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

/// Recompute MAIN's activity-list `Rect` for a given frame area + view, so a
/// mouse handler can hit-test clicks against the exact rect [`draw_main`] laid
/// the activity panel into. MUST mirror the vertical [`Layout`] in `draw_main`
/// (header · overview · domain map · model strip · activity · footer); kept here
/// next to the activity code so the two move together. Returns a zero-height
/// rect if the terminal is too short for the activity slot.
pub(crate) fn main_activity_area(area: Rect, view: &DashboardView) -> Rect {
    let compact = area.height < 30;
    let strip_rows = view.model_usage.len().min(MODEL_STRIP_ROWS);
    let strip_height = if !compact && strip_rows > 0 {
        strip_rows as u16 + 2
    } else {
        0
    };
    let [_header, _overview, _domains, _strip, activity_area, _footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if compact { 10 } else { 13 }),
        Constraint::Length(if compact { 0 } else { 7 }),
        Constraint::Length(strip_height),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(area);
    activity_area
}

/// What a screen row in the activity list maps back to (for click hit-testing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActivityHit {
    /// An in-flight (spinner) row — not expandable.
    InFlight,
    /// A completed note row — not expandable (nothing more to show).
    Note,
    /// A completed request row, identified by its stable [`ExpandKey`].
    Request(ExpandKey),
}

/// One rendered activity row's vertical extent + what it maps to. `y_start` is
/// absolute (screen) coordinates; `height` is 1 for a collapsed row and `1 + N`
/// detail lines for the expanded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityRow {
    pub y_start: u16,
    pub height: u16,
    pub hit: ActivityHit,
}

/// The full ordered set of rows drawn into the activity list this frame, plus
/// the rendered lines. Produced once by [`activity_layout`] and consumed by BOTH
/// the renderer (`draw_activity` reads `lines`) and the click handler
/// (`hit` maps a `(col,row)` back to an entry). Doing the layout in one pure
/// place is what keeps "what was drawn" and "what was clicked" in lock-step.
pub(crate) struct ActivityLayout {
    pub area: Rect,
    pub rows: Vec<ActivityRow>,
    pub lines: Vec<Line<'static>>,
    /// Total completed entries (for the title), and the clamped scroll used.
    pub total: usize,
    pub scroll: usize,
}

impl ActivityLayout {
    /// Map a click at absolute `(col, row)` to the expandable entry it lands on,
    /// or `None` for a miss / a non-request row. Only clicks INSIDE the activity
    /// rect count; in-flight and note rows return `None` (nothing to expand).
    pub(crate) fn hit(&self, col: u16, row: u16) -> Option<ExpandKey> {
        if col < self.area.left()
            || col >= self.area.right()
            || row < self.area.top()
            || row >= self.area.bottom()
        {
            return None;
        }
        for r in &self.rows {
            if row >= r.y_start && row < r.y_start + r.height {
                return match &r.hit {
                    ActivityHit::Request(key) => Some(key.clone()),
                    _ => None,
                };
            }
        }
        None
    }
}

/// Pure layout for the activity list: build the exact lines `draw_activity`
/// renders AND the row→entry map the click handler hit-tests against, from the
/// same inputs, so the two can never drift. `expanded` is the currently expanded
/// entry's key (its row gets the ▾ marker + indented detail lines); everything
/// else renders as a single ▸-less collapsed line. `now` is needed for the
/// in-flight elapsed/spinner; pass a fixed time in tests for determinism.
///
/// The math mirrors the original `draw_activity`: in-flight rows pinned on top
/// only at the live tail (scroll==0), then completed entries newest-first
/// windowed by the scroll offset, all clamped to the panel's row `capacity`
/// (height minus the top border).
pub(crate) fn activity_layout(
    area: Rect,
    view: &DashboardView,
    expanded: Option<&ExpandKey>,
    scroll: usize,
) -> ActivityLayout {
    activity_layout_at(area, view, expanded, scroll, 0, SystemTime::UNIX_EPOCH)
}

/// [`activity_layout`] with explicit animation frame + `now` (for the live
/// renderer and for deterministic tests).
fn activity_layout_at(
    area: Rect,
    view: &DashboardView,
    expanded: Option<&ExpandKey>,
    scroll: usize,
    anim_frame: usize,
    now: SystemTime,
) -> ActivityLayout {
    let in_flight = &view.in_flight;
    let capacity = area.height.saturating_sub(1) as usize; // top border
                                                           // Content starts one row below the top border the Block draws.
    let y0 = area.y.saturating_add(1);

    let mut rows: Vec<ActivityRow> = Vec::new();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(capacity);
    let mut used: usize = 0; // content rows consumed so far

    // In-flight rows pinned on top ONLY when viewing the live tail (scroll==0);
    // while scrolled into history they'd steal rows from the page being read.
    if scroll == 0 {
        for request in in_flight.iter().rev() {
            if used >= capacity {
                break;
            }
            lines.push(in_flight_line(view, request, anim_frame, now));
            rows.push(ActivityRow {
                y_start: y0.saturating_add(used as u16),
                height: 1,
                hit: ActivityHit::InFlight,
            });
            used += 1;
        }
    }

    // Completed entries, newest first, windowed by the scroll offset (req6:
    // the whole history is reachable, not just the rows that happen to fit).
    let total = view.completed.len();
    let scroll = scroll.min(total.saturating_sub(1));
    for entry in view.completed.iter().skip(scroll) {
        if used >= capacity {
            break;
        }
        let key = ExpandKey::for_completed(entry);
        let is_expanded = matches!((&key, expanded), (Some(k), Some(e)) if k == e);
        // Header line for this entry. The expand marker only applies to
        // requests (notes are not expandable).
        let header = completed_line(entry, key.is_some(), is_expanded);
        lines.push(header);
        let hit = match key.clone() {
            Some(k) => ActivityHit::Request(k),
            None => ActivityHit::Note,
        };
        let mut height: u16 = 1;
        used += 1;

        // Expanded detail lines, indented, as long as they fit in the panel.
        if is_expanded {
            for detail in expanded_detail_lines(entry) {
                if used >= capacity {
                    break;
                }
                lines.push(detail);
                height += 1;
                used += 1;
            }
        }
        rows.push(ActivityRow {
            y_start: y0.saturating_add((used as u16).saturating_sub(height)),
            height,
            hit,
        });
    }

    ActivityLayout {
        area,
        rows,
        lines,
        total,
        scroll,
    }
}

/// Render one in-flight (spinner) row — extracted so [`activity_layout_at`] and
/// any future caller share one definition.
fn in_flight_line(
    view: &DashboardView,
    request: &super::activity::InFlight,
    anim_frame: usize,
    now: SystemTime,
) -> Line<'static> {
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
    if let Some(meta) = activity_meta(request.group.as_deref(), request.model.as_deref(), None) {
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
    Line::from(spans)
}

fn draw_activity(
    frame: &mut Frame,
    area: Rect,
    view: &DashboardView,
    chrome: &Chrome,
    now: SystemTime,
) {
    let in_flight = &view.in_flight;
    let layout = activity_layout_at(
        area,
        view,
        chrome.expanded.as_ref(),
        chrome.activity_scroll,
        chrome.frame,
        now,
    );
    let total = layout.total;
    let scroll = layout.scroll;
    // Rows actually shown of the completed history (for the title range). The
    // layout already clamped everything to the panel capacity.
    let completed_shown = layout
        .rows
        .iter()
        .filter(|r| !matches!(r.hit, ActivityHit::InFlight))
        .count();
    let shown_last = (scroll + completed_shown).min(total);

    // Title carries the scroll position so it's obvious you're in history.
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
    frame.render_widget(Paragraph::new(layout.lines).block(block), area);
}

/// Indented detail lines for an expanded completed *request* (issue #5): full
/// method+path, account, status, duration, group/model/effort, the token
/// breakdown (input / output / cache_read / cache_creation / total), and the
/// per-component + total API-equivalent cost via [`crate::pricing`]. Notes
/// produce no detail lines. Cost uses the built-in default rate table (empty
/// overrides) — same as the inline `completed_line` cost and the server.log path.
///
/// NOTE: the view-model drops the cache split for completed entries (only
/// in/out survive the doc round-trip), so `cache_read`/`cache_creation` read as
/// `—` here and the cost is the input+output cost. That is a data-layer fact,
/// not a render bug — the model-usage rows (Stats overlay) carry the cache split.
fn expanded_detail_lines(entry: &Completed) -> Vec<Line<'static>> {
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

    // Indent under the timestamp column so detail reads as a child of its row.
    let indent = "        ";
    let label_style = dim();
    let mut out: Vec<Line<'static>> = Vec::new();

    let mut detail_line = |label: &str, value: String| {
        out.push(Line::from(vec![
            Span::styled(format!("{indent}{label:<9}"), label_style),
            Span::raw(value),
        ]));
    };

    detail_line("request", format!("{method} {path}"));
    detail_line("account", account.clone().unwrap_or_else(|| "?".into()));
    detail_line(
        "status",
        format!("{status} · {}", format::elapsed_secs(*duration)),
    );

    // group / model / effort, omitting the parts that are unknown.
    let mut routed = String::new();
    if let Some(g) = group {
        routed.push_str(g);
    }
    if let Some(m) = model {
        if !routed.is_empty() {
            routed.push(' ');
        }
        routed.push_str(m);
    }
    if let Some(e) = effort {
        routed.push_str(&format!(" · {e}"));
    }
    if routed.is_empty() {
        routed.push_str("(unrouted)");
    }
    detail_line("model", routed);

    // Token breakdown. Completed entries carry only input/output (the doc drops
    // the cache split), so cache fields render as "—".
    let fmt_tok = format::human_count;
    let fmt_opt = |o: Option<u64>| o.map(format::human_count).unwrap_or_else(|| "—".into());
    if let Some(t) = tokens {
        detail_line(
            "tokens",
            format!(
                "in {} · out {} · cache_r {} · cache_w {} · total {}",
                fmt_tok(t.input),
                fmt_tok(t.output),
                fmt_opt(t.cache_read),
                fmt_opt(t.cache_creation),
                fmt_tok(t.total()),
            ),
        );

        // Per-component + total API-equivalent cost, when group+model known.
        if let (Some(g), Some(m)) = (group, model) {
            let overrides = std::collections::HashMap::new();
            let in_cost = crate::pricing::cost_from_parts(g, m, t.input, 0, None, None, &overrides);
            let out_cost =
                crate::pricing::cost_from_parts(g, m, 0, t.output, None, None, &overrides);
            let cr_cost =
                crate::pricing::cost_from_parts(g, m, 0, 0, t.cache_read, None, &overrides);
            let cw_cost =
                crate::pricing::cost_from_parts(g, m, 0, 0, None, t.cache_creation, &overrides);
            let total_cost = crate::pricing::cost_usd(g, m, t, &overrides);
            detail_line(
                "cost",
                format!(
                    "in {} · out {} · cache_r {} · cache_w {} · total {}",
                    format_cost(in_cost),
                    format_cost(out_cost),
                    format_cost(cr_cost),
                    format_cost(cw_cost),
                    format_cost(total_cost),
                ),
            );
        } else {
            detail_line("cost", "n/a (model unknown)".into());
        }
    } else {
        detail_line("tokens", "—".into());
    }

    out
}

/// Render one completed entry's single header line. `expandable` adds a clickable
/// ▸/▾ marker (issue #5) — ▾ when `expanded`, ▸ when collapsed; notes pass
/// `expandable=false` and get a blank marker slot so columns stay aligned.
fn completed_line(entry: &Completed, expandable: bool, expanded: bool) -> Line<'static> {
    // Marker column: ▾ expanded, ▸ collapsed-but-expandable, space otherwise.
    // Kept a fixed 1-char glyph + space so the timestamp column never shifts.
    let marker = if !expandable {
        Span::raw("  ")
    } else if expanded {
        Span::styled("▾ ", Style::new().fg(Color::Cyan))
    } else {
        Span::styled("▸ ", dim())
    };
    let stamp = Span::styled(format!(" {}  ", format::clock_hms_utc(entry.at)), dim());
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
                // API-equivalent $ next to tokens, when the backend group+model
                // are known. TUI render holds no config overrides → empty map =
                // built-in default rate table (same as the server.log path).
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
            let mut spans = vec![marker, stamp, Span::raw(format!("{method} {path}"))];
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
            Line::from(vec![marker, stamp, Span::styled(text.clone(), style)])
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

/// API-equivalent USD cost for one model row's accumulated tokens. Computed via
/// [`crate::pricing`] with an empty overrides map — the TUI render path holds no
/// config overrides, so this uses the built-in default rate table (same as the
/// server.log path).
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
        cells.push(Cell::from(Span::styled(format_cost(model_cost(m)), dim())));
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
                Cell::from(Span::styled(format_cost(model_cost(m)), dim())),
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
            // Stats overlay: navigation + back, regardless of attach mode.
            Overlay::Stats => Line::from(vec![
                Span::raw(" stats — "),
                key("g/Esc"),
                Span::raw(" back  "),
                key("↑/k ↓/j"),
                Span::raw(" model  "),
                key("PgUp/PgDn"),
                Span::raw(" page  "),
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
            model_cursor: 0,
            expanded: None,
            add_input_len: 0,
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

    #[test]
    fn format_cost_scheme() {
        // Sub-dollar: four decimals so small per-request costs stay legible.
        assert_eq!(format_cost(0.0), "$0.0000");
        assert_eq!(format_cost(0.004_25), "$0.0043"); // rounds to 4dp
        assert_eq!(format_cost(0.5), "$0.5000");
        // >= $1: two decimals.
        assert_eq!(format_cost(1.0), "$1.00");
        assert_eq!(format_cost(3.775), "$3.77"); // banker-agnostic: 2dp round
        assert_eq!(format_cost(42.5), "$42.50");
    }

    #[test]
    fn model_cost_uses_default_rates() {
        // gpt-5.5: 1e6 out @ $30/1e6 = $30.00, cache_read 40k @ $0.5/1e6 = $0.02.
        let m = model_row("codex", "gpt-5.5", 0, 1_000_000);
        let cost = model_cost(&m);
        assert!((cost - 30.02).abs() < 1e-6, "expected ~30.02, got {cost}");
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

    // --- issue #5: native mouse + click-to-expand activity rows -------------

    use super::super::activity::{Completed, CompletedBody};

    /// A completed request entry stamped at `at_secs` (so its `ExpandKey.at_ms`
    /// is deterministic). `account` distinguishes rows for the assertions.
    fn completed_request(at_secs: u64, account: &str, status: u16) -> Completed {
        Completed {
            at: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(at_secs),
            body: CompletedBody::Request {
                method: "POST".into(),
                path: "/v1/messages".into(),
                account: Some(account.into()),
                status,
                duration: Duration::from_millis(1_400),
                tokens: Some(super::super::TokenCounts {
                    input: 1_000,
                    output: 200,
                    ..Default::default()
                }),
                group: Some("codex".into()),
                model: Some("gpt-5.5".into()),
                effort: Some("high".into()),
            },
        }
    }

    fn completed_note(at_secs: u64, text: &str) -> Completed {
        Completed {
            at: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(at_secs),
            body: CompletedBody::Note {
                text: text.into(),
                error: false,
            },
        }
    }

    /// The activity panel rect for a typical MAIN frame (no in-flight, no model
    /// strip). 8-tall so the activity Min(3) slot lands at a known y.
    fn activity_area() -> Rect {
        let view = view_with(Vec::new());
        main_activity_area(Rect::new(0, 0, 120, 24), &view)
    }

    #[test]
    fn hit_test_maps_screen_row_to_the_clicked_completed_entry() {
        // Three completed requests, newest first. The panel content begins one
        // row below the top border, one row per (collapsed) entry.
        let mut view = view_with(Vec::new());
        view.completed = vec![
            completed_request(30, "a", 200),
            completed_request(20, "b", 200),
            completed_request(10, "c", 500),
        ];
        let area = activity_area();
        let layout = activity_layout(area, &view, None, 0);

        // First content row → the newest entry ("a"); the third → "c".
        let y0 = area.y + 1;
        let hit0 = layout.hit(area.x + 2, y0).expect("row 0 hits an entry");
        assert_eq!(hit0, ExpandKey::for_completed(&view.completed[0]).unwrap());
        let hit2 = layout.hit(area.x + 2, y0 + 2).expect("row 2 hits an entry");
        assert_eq!(hit2, ExpandKey::for_completed(&view.completed[2]).unwrap());
        // Distinct entries → distinct keys.
        assert_ne!(hit0, hit2);
    }

    #[test]
    fn hit_test_misses_outside_the_rect_and_on_notes() {
        let mut view = view_with(Vec::new());
        view.completed = vec![
            completed_note(20, "switch a → b"),
            completed_request(10, "a", 200),
        ];
        let area = activity_area();
        let layout = activity_layout(area, &view, None, 0);
        let y0 = area.y + 1;

        // The note row (row 0) is not expandable → miss.
        assert_eq!(layout.hit(area.x + 2, y0), None, "notes are not expandable");
        // Above the rect and below the last row → miss.
        assert_eq!(
            layout.hit(area.x + 2, area.y),
            None,
            "border/above is a miss"
        );
        assert_eq!(
            layout.hit(area.x + 2, area.bottom() + 5),
            None,
            "below the rect is a miss"
        );
        // At/beyond the right edge → miss (the rect is half-open on the right).
        assert_eq!(
            layout.hit(area.right(), y0 + 1),
            None,
            "right edge is outside the rect"
        );
    }

    #[test]
    fn expanding_an_entry_inserts_detail_rows_and_shifts_later_rows_down() {
        let mut view = view_with(Vec::new());
        view.completed = vec![
            completed_request(30, "a", 200),
            completed_request(20, "b", 200),
        ];
        let area = activity_area();

        // Collapsed: each entry is one row tall, back to back.
        let collapsed = activity_layout(area, &view, None, 0);
        assert_eq!(collapsed.rows[0].height, 1);
        assert_eq!(collapsed.rows[1].height, 1);
        assert_eq!(collapsed.rows[1].y_start, collapsed.rows[0].y_start + 1);

        // Expand the first entry: its row grows by the detail lines, and the
        // second entry's row starts *below* the expanded block — the click→entry
        // map stays correct as rows move (stable identity, not index).
        let key = ExpandKey::for_completed(&view.completed[0]).unwrap();
        let expanded = activity_layout(area, &view, Some(&key), 0);
        assert!(
            expanded.rows[0].height > 1,
            "expanded row carries detail lines"
        );
        assert_eq!(
            expanded.rows[1].y_start,
            expanded.rows[0].y_start + expanded.rows[0].height,
            "the next entry shifts down by the expanded block height"
        );
        // Clicking the second entry's NEW position still resolves to entry b.
        let hit = expanded
            .hit(area.x + 2, expanded.rows[1].y_start)
            .expect("second entry hittable at its shifted row");
        assert_eq!(hit, ExpandKey::for_completed(&view.completed[1]).unwrap());
    }

    #[test]
    fn expanded_row_renders_detail_with_tokens_and_cost() {
        let mut view = view_with(Vec::new());
        view.completed = vec![completed_request(10, "acct-x", 200)];
        let key = ExpandKey::for_completed(&view.completed[0]).unwrap();
        let mut chrome = chrome_overlay(Overlay::None);
        chrome.expanded = Some(key);
        // Tall enough that the activity panel has room for the detail lines.
        let text = render(&view, &chrome, 120, 36);
        assert!(text.contains("acct-x"), "detail names the account");
        assert!(text.contains("tokens"), "detail shows the token breakdown");
        assert!(text.contains("cost"), "detail shows the cost breakdown");
        // gpt-5.5 output 200 tok @ $30/1e6 ⇒ a $ figure is present.
        assert!(text.contains('$'), "cost is rendered as USD");
        assert!(text.contains('▾'), "expanded row carries the open marker");
    }

    #[test]
    fn collapsed_request_rows_show_the_expand_marker() {
        let mut view = view_with(Vec::new());
        view.completed = vec![completed_request(10, "a", 200)];
        let text = render(&view, &chrome_overlay(Overlay::None), 120, 24);
        assert!(text.contains('▸'), "collapsed request row shows ▸");
    }
}
