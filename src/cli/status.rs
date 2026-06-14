//! `llmux status` — herdr-style client/server/update sections from a
//! running server; `--json` emits the raw status document.
//!
//! Exit codes: 0 = server running, 1 = server not running (or any error).

use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use super::daemon::{self, ServerProbe};
use super::{CliError, StatusArgs};

/// Probe the configured port (shared with `llmux run`'s auto-start —
/// see `cli::daemon`) and render the herdr-style sections. A server that is
/// not running prints the client section + `status: not running` and exits 1.
pub async fn run(args: StatusArgs) -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;
    let port = config.proxy.port;
    let client_version = crate::build_info::version_string();

    match daemon::probe_server(port, config.proxy.api_key.as_deref()).await? {
        ServerProbe::Running { status } => {
            if args.json {
                println!("{status:#}");
            } else {
                print!(
                    "{}",
                    render(&client_version, Some(&status), port, SystemTime::now())
                );
            }
            Ok(())
        }
        ServerProbe::NotRunning => {
            if args.json {
                println!(
                    "{:#}",
                    serde_json::json!({ "server": "not running", "port": port })
                );
            } else {
                print!("{}", render(&client_version, None, port, SystemTime::now()));
            }
            std::process::exit(1);
        }
        ServerProbe::Foreign { detail } => Err(CliError::Message(format!(
            "port {port} answers but is not llmux: {detail}"
        ))),
    }
}

/// Render the human-readable sections (pure over the status document and an
/// explicit `now`, so the layout is unit-testable). `server: None` = not
/// running.
fn render(
    client_version: &str,
    server: Option<&serde_json::Value>,
    port: u16,
    now: SystemTime,
) -> String {
    let mut out = String::new();
    // Writing to a String cannot fail.
    let _ = writeln!(out, "client:");
    let _ = writeln!(out, "  version: {}", display_version(client_version));
    let _ = writeln!(out);
    let _ = writeln!(out, "server:");
    let Some(doc) = server else {
        let _ = writeln!(out, "  status: not running");
        let _ = writeln!(out, "  port: {port}");
        return out;
    };
    let _ = writeln!(out, "  status: running");
    let server_version = doc["version"].as_str().unwrap_or("unknown");
    let _ = writeln!(out, "  version: {}", display_version(server_version));
    let _ = writeln!(
        out,
        "  port: {}",
        doc["port"].as_u64().unwrap_or(u64::from(port))
    );
    if let Some(uptime) = doc["uptime_secs"].as_u64() {
        let _ = writeln!(out, "  uptime: {}", format_uptime(uptime));
    }
    if let Some(pid) = doc["pid"].as_u64() {
        let _ = writeln!(out, "  pid: {pid}");
    }
    let _ = writeln!(out, "  accounts: {}", accounts_summary(doc));
    for line in account_lines(doc, now) {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(
        out,
        "  current: {}",
        doc["current"].as_str().unwrap_or("(none)")
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "update:");
    if server_version == client_version {
        let _ = writeln!(out, "  client/server match: yes");
    } else {
        let _ = writeln!(
            out,
            "  client/server match: no (client {} vs server {} — restart server to apply)",
            display_version(client_version),
            display_version(server_version)
        );
    }
    out
}

/// `version_string` is "llmux X (channel id)" — drop the binary name
/// for display (the section labels already say whose version it is).
fn display_version(version: &str) -> &str {
    version.strip_prefix("llmux ").unwrap_or(version)
}

/// "3 (2 ready, 1 cooldown)" — ready = active/ok; cooldown and auth-failed
/// shown only when non-zero.
fn accounts_summary(doc: &serde_json::Value) -> String {
    let accounts = doc["accounts"].as_array().cloned().unwrap_or_default();
    let mut ready = 0usize;
    let mut cooldown = 0usize;
    let mut auth_failed = 0usize;
    for account in &accounts {
        match account["status"].as_str() {
            Some("active") | Some("ok") => ready += 1,
            Some("cooldown") => cooldown += 1,
            Some("auth_failed") => auth_failed += 1,
            _ => {}
        }
    }
    let mut parts = vec![format!("{ready} ready")];
    if cooldown > 0 {
        parts.push(format!("{cooldown} cooldown"));
    }
    if auth_failed > 0 {
        parts.push(format!("{auth_failed} auth-failed"));
    }
    format!("{} ({})", accounts.len(), parts.join(", "))
}

/// One indented line per account, in the server's selection order (B1: the
/// array already arrives ordered current → eligible by rank → ineligible).
/// Shows the order number, the state — the concrete `blocked` reason when
/// the server sent one, the plain status otherwise — and the token summary
/// ("tok 6h52m ↻3m": expiry countdown + refreshed-ago) when the server
/// reports a token expiry.
fn account_lines(doc: &serde_json::Value, now: SystemTime) -> Vec<String> {
    let accounts = doc["accounts"].as_array().cloned().unwrap_or_default();
    let width = accounts
        .iter()
        .filter_map(|a| a["name"].as_str().map(str::len))
        .max()
        .unwrap_or(0);
    accounts
        .iter()
        .enumerate()
        .map(|(idx, account)| {
            let order = account["order"].as_u64().unwrap_or(idx as u64 + 1);
            let name = account["name"].as_str().unwrap_or("?");
            let state = match account["blocked"].as_str() {
                Some(blocked) => blocked,
                None => match account["status"].as_str() {
                    Some("active") => "active",
                    Some("ok") => "ready",
                    Some(other) => other,
                    None => "?",
                },
            };
            let token = token_summary(account, now)
                .map(|t| format!("  {t}"))
                .unwrap_or_default();
            format!("    {order}. {name:<width$}  {state}{token}")
        })
        .collect()
}

/// "tok 6h52m ↻3m" — expiry countdown plus refreshed-ago marker, mirroring
/// the dashboard's token column. `None` when the server reports no token
/// expiry (apikey accounts, pre-upgrade servers). "tok expired" past expiry;
/// the ↻ marker is omitted when the token was never refreshed.
fn token_summary(account: &serde_json::Value, now: SystemTime) -> Option<String> {
    let expires_ms = account["token_expires_at_ms"].as_u64()?;
    let expires_at = SystemTime::UNIX_EPOCH + Duration::from_millis(expires_ms);
    let expiry = match expires_at.duration_since(now) {
        Ok(left) => crate::scheduler::select::compact_duration(left),
        Err(_) => "expired".to_string(),
    };
    match crate::tui::format::refreshed_marker(account["last_refresh_ms"].as_u64(), now) {
        Some(marker) => Some(format!("tok {expiry} {marker}")),
        None => Some(format!("tok {expiry}")),
    }
}

/// "2h 13m" style uptime.
fn format_uptime(secs: u64) -> String {
    if secs >= 86_400 {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
    } else if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed test clock: 2026-06-13 00:00:00 UTC.
    fn test_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_308_800)
    }

    fn status_doc(version: &str) -> serde_json::Value {
        serde_json::json!({
            "version": version,
            "pid": 4321,
            "uptime_secs": 7980,
            "port": 3456,
            "current": "a@x.com",
            "accounts": [
                { "name": "a@x.com", "status": "active", "order": 1, "blocked": null },
                { "name": "b@x.com", "status": "ok", "order": 2, "blocked": null },
                { "name": "c@x.com", "status": "cooldown", "order": 3, "blocked": "cooldown 3m12s" },
            ],
        })
    }

    #[test]
    fn render_running_server_has_all_sections() {
        let client = "llmux 0.1.0 (dev dev)";
        let out = render(client, Some(&status_doc(client)), 3456, test_now());
        let expected = "\
client:
  version: 0.1.0 (dev dev)

server:
  status: running
  version: 0.1.0 (dev dev)
  port: 3456
  uptime: 2h 13m
  pid: 4321
  accounts: 3 (2 ready, 1 cooldown)
    1. a@x.com  active
    2. b@x.com  ready
    3. c@x.com  cooldown 3m12s
  current: a@x.com

update:
  client/server match: yes
";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_version_mismatch_says_restart() {
        let out = render(
            "llmux 0.2.0 (dev dev)",
            Some(&status_doc("llmux 0.1.0 (dev dev)")),
            3456,
            test_now(),
        );
        assert!(
            out.contains(
                "client/server match: no (client 0.2.0 (dev dev) vs server 0.1.0 (dev dev) \
                 — restart server to apply)"
            ),
            "unexpected output:\n{out}"
        );
    }

    #[test]
    fn render_not_running_prints_client_only() {
        let out = render("llmux 0.1.0 (dev dev)", None, 3456, test_now());
        let expected = "\
client:
  version: 0.1.0 (dev dev)

server:
  status: not running
  port: 3456
";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_tolerates_older_servers_without_new_fields() {
        // A pre-A3 server has no pid/uptime_secs/port — those lines are
        // simply omitted (backwards compat is additive both ways).
        let doc = serde_json::json!({
            "version": "llmux 0.1.0 (dev dev)",
            "current": null,
            "accounts": [{ "name": "a", "status": "auth_failed" }],
        });
        let out = render("llmux 0.1.0 (dev dev)", Some(&doc), 3456, test_now());
        assert!(
            out.contains("  port: 3456"),
            "falls back to config port:\n{out}"
        );
        assert!(!out.contains("uptime:"), "{out}");
        assert!(!out.contains("pid:"), "{out}");
        assert!(
            out.contains("  accounts: 1 (0 ready, 1 auth-failed)"),
            "{out}"
        );
        // Pre-B1 servers send no order/blocked — positional numbering and
        // the raw status word fill in.
        assert!(out.contains("    1. a  auth_failed"), "{out}");
        assert!(out.contains("  current: (none)"), "{out}");
    }

    #[test]
    fn account_lines_show_token_expiry_and_refreshed_ago() {
        let now = test_now();
        let now_ms = 1_781_308_800_000u64;
        let doc = serde_json::json!({
            "accounts": [
                {
                    // 6h52m left, refreshed 3m ago → "tok 6h52m ↻3m".
                    "name": "a@x.com", "status": "active", "order": 1,
                    "token_expires_at_ms": now_ms + (6 * 3600 + 52 * 60) * 1000,
                    "last_refresh_ms": now_ms - 3 * 60 * 1000,
                },
                {
                    // Known expiry, never refreshed → no ↻ marker.
                    "name": "b@x.com", "status": "ok", "order": 2,
                    "token_expires_at_ms": now_ms + 3600 * 1000,
                    "last_refresh_ms": null,
                },
                {
                    // Expired token, refreshed 2h ago.
                    "name": "c@x.com", "status": "auth_failed", "order": 3,
                    "token_expires_at_ms": now_ms - 1000,
                    "last_refresh_ms": now_ms - 2 * 3600 * 1000,
                },
                // apikey: no token fields at all → no tok column.
                { "name": "api-1", "status": "ok", "order": 4 },
            ],
        });
        let lines = account_lines(&doc, now);
        assert_eq!(lines[0], "    1. a@x.com  active  tok 6h52m \u{21bb}3m");
        assert_eq!(lines[1], "    2. b@x.com  ready  tok 1h00m");
        assert_eq!(
            lines[2],
            "    3. c@x.com  auth_failed  tok expired \u{21bb}2h"
        );
        assert_eq!(lines[3], "    4. api-1    ready");
    }

    #[test]
    fn uptime_formats_across_magnitudes() {
        assert_eq!(format_uptime(42), "42s");
        assert_eq!(format_uptime(65), "1m 5s");
        assert_eq!(format_uptime(7980), "2h 13m");
        assert_eq!(format_uptime(90_000), "1d 1h");
    }
}
