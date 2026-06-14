//! Server lifecycle from the CLI: detect a running server on the configured
//! port, auto-start one as a detached background daemon (`llmux run`),
//! and stop it (`llmux stop` → `POST /llmux/shutdown`).
//!
//! Detection is herdr-style: probe `GET /llmux/status` with a short
//! timeout. Connection refused/timeout = not running; a 200 with a
//! llmux-shaped document = running; anything else answering on the port
//! is FOREIGN and we refuse to spawn over it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{proxy_base_url, CliError, StopArgs};
use crate::config::Config;

/// Probe timeout: long enough for a loaded localhost server, short enough
/// that `llmux run` stays snappy when nothing is listening.
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// Max wait for a spawned daemon to answer the status endpoint (and for a
/// stopped server to release the port).
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Drain budget for a version-gated restart: longer than `stop`'s 5s so an
/// in-flight request on the old daemon (it may hold several live accounts,
/// one actively serving) finishes via cooperative shutdown instead of being
/// cut off. If the port still isn't free after this, we error — never SIGKILL.
const RESTART_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval while waiting for readiness / port release.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// What is (or is not) listening on the proxy port.
#[derive(Debug)]
pub enum ServerProbe {
    /// `/llmux/status` answered with a llmux-shaped document.
    Running { status: serde_json::Value },
    /// Connection refused / timed out — nothing is listening.
    NotRunning,
    /// Something answered, but it is not llmux — never spawn over it.
    Foreign { detail: String },
}

/// Outcome of [`ensure_server_running`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// A same-version daemon was already up — reused untouched.
    AlreadyRunning,
    /// Nothing was listening; a fresh daemon was spawned.
    Started { pid: u32 },
    /// A running daemon was a different version (or `--force`): drained and
    /// replaced with a freshly spawned one.
    Restarted { pid: u32 },
}

/// Probe the configured port for a running llmux server.
pub async fn probe_server(port: u16, api_key: Option<&str>) -> Result<ServerProbe, CliError> {
    let client = reqwest::Client::builder()
        .connect_timeout(PROBE_TIMEOUT)
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|err| CliError::Message(format!("http client init failed: {err}")))?;
    let url = format!("{}/llmux/status", proxy_base_url(port));
    let mut request = client.get(&url);
    if let Some(api_key) = api_key {
        // Localhost is exempt, but sending it is harmless and keeps this
        // working if the exemption ever tightens.
        request = request.header("x-api-key", api_key);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) if err.is_connect() || err.is_timeout() => return Ok(ServerProbe::NotRunning),
        // The port answered but not as HTTP we could speak — foreign.
        Err(err) => {
            return Ok(ServerProbe::Foreign {
                detail: err.to_string(),
            })
        }
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Ok(classify_probe(status, &body))
}

/// The daemon's pid from a `/llmux/status` (or `/llmux/dashboard`)
/// document, for the attach-mode header marker before the first dashboard
/// poll lands. `None` if the field is missing (older server).
pub fn status_pid(status: &serde_json::Value) -> Option<u32> {
    status
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
}

/// Classify a status-endpoint response: only a 2xx carrying a
/// llmux-shaped document counts as a running server.
fn classify_probe(status: http::StatusCode, body: &str) -> ServerProbe {
    if !status.is_success() {
        return ServerProbe::Foreign {
            detail: format!("status endpoint returned {status}"),
        };
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(doc) if is_llmux_status(&doc) => ServerProbe::Running { status: doc },
        _ => ServerProbe::Foreign {
            detail: "status response is not a llmux document".into(),
        },
    }
}

/// The minimal shape every llmux server has served since v0.1:
/// `version` ("llmux ...") and an `accounts` array.
fn is_llmux_status(doc: &serde_json::Value) -> bool {
    doc.get("version")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|v| v.starts_with("llmux"))
        && doc.get("accounts").is_some_and(serde_json::Value::is_array)
}

/// Should `run`/`restart` replace an already-running daemon? Compares the
/// running daemon's reported version to THIS binary's version.
///
/// - `force` → always restart.
/// - versions differ → restart (the running daemon predates this install).
/// - versions match → reuse (must NOT churn a healthy same-version server).
/// - version unparseable (`None`) and not forced → reuse: an old/odd status
///   document is not a reason to drain live accounts.
fn should_restart(running_version: Option<&str>, current_version: &str, force: bool) -> bool {
    force || matches!(running_version, Some(v) if v != current_version)
}

/// Make sure a server is listening on `config.proxy.port`: probe, and when
/// nothing is running spawn `llmux server --no-tui` as a detached daemon
/// (stderr → [`server_log_path`]) and wait until the status endpoint answers.
///
/// When a daemon is already running, its version is compared to this binary's
/// (`force` overrides): a mismatch drains the old daemon cooperatively
/// ([`RESTART_DRAIN_TIMEOUT`]) and spawns a fresh one ([`EnsureOutcome::Restarted`]);
/// a match reuses it untouched ([`EnsureOutcome::AlreadyRunning`]). A foreign
/// listener on the port is an error, never spawned over.
pub async fn ensure_server_running(
    config: &Config,
    force: bool,
) -> Result<EnsureOutcome, CliError> {
    let port = config.proxy.port;
    let api_key = config.proxy.api_key.as_deref();
    let mut restarting = false;
    match probe_server(port, api_key).await? {
        ServerProbe::Running { status } => {
            let current = crate::build_info::version_string();
            let running = status.get("version").and_then(serde_json::Value::as_str);
            if should_restart(running, &current, force) {
                // Drain the old daemon cooperatively before we spawn over it.
                shutdown_and_wait(port, api_key, RESTART_DRAIN_TIMEOUT).await?;
                restarting = true;
            } else {
                return Ok(EnsureOutcome::AlreadyRunning);
            }
        }
        ServerProbe::Foreign { detail } => {
            return Err(CliError::Message(format!(
                "port {port} is in use by something that is not llmux ({detail})\n\
                 Free the port or change proxy.port in the config."
            )));
        }
        ServerProbe::NotRunning => {}
    }
    // The daemon would refuse to start without accounts; fail here with the
    // same guidance instead of timing out on readiness.
    if config.accounts.is_empty() {
        return Err(CliError::Message(
            "no accounts configured\n\
             Add one first:\n  \
             llmux import           Import from Claude Code / teamclaude\n  \
             llmux login            OAuth login via browser\n  \
             llmux login --api      Add an API key"
                .into(),
        ));
    }
    let log_path = server_log_path()?;
    let pid = spawn_server_daemon(&log_path)?;
    wait_until_ready(port, api_key, READY_TIMEOUT)
        .await
        .map_err(|err| CliError::Message(format!("{err}\nServer log: {}", log_path.display())))?;
    if restarting {
        Ok(EnsureOutcome::Restarted { pid })
    } else {
        Ok(EnsureOutcome::Started { pid })
    }
}

/// `llmux restart` — explicitly drain-if-running and (re)spawn the daemon,
/// then print status. Unlike `run`, this never execs `claude`: it is just the
/// server-lifecycle half, with `force` so a same-version daemon is replaced too.
pub async fn restart() -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;
    let port = config.proxy.port;
    let version = crate::build_info::version_string();
    match ensure_server_running(&config, true).await? {
        EnsureOutcome::Started { pid } => {
            println!("started llmux server (pid {pid}) on port {port} → {version}");
        }
        EnsureOutcome::Restarted { pid } => {
            println!("restarted llmux server (pid {pid}) on port {port} → {version}");
        }
        // With force=true this is unreachable, but stay total rather than panic.
        EnsureOutcome::AlreadyRunning => {
            println!("llmux server already running on port {port} → {version}");
        }
    }
    Ok(())
}

/// Spawn `current_exe() server --no-tui` fully detached: own process group
/// (survives this CLI and its terminal), stdin/stdout null, stderr appended
/// to the log file (the non-TUI server logs to stderr). Never waited on.
fn spawn_server_daemon(log_path: &Path) -> Result<u32, CliError> {
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["server", "--no-tui"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // New process group: no SIGHUP/SIGINT from the spawning terminal.
        command.process_group(0);
    }
    let child = command.spawn()?;
    Ok(child.id())
}

/// Daemon stderr log: `$XDG_STATE_HOME/llmux/server.log`, defaulting to
/// `~/.local/state/llmux/server.log` (state, not config — same
/// deliberate Unix-everywhere choice as `config::config_path`).
pub fn server_log_path() -> Result<PathBuf, CliError> {
    let dir = state_dir().ok_or_else(|| {
        CliError::Message("could not determine a state directory for the server log".into())
    })?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("server.log"))
}

/// `$XDG_STATE_HOME/llmux` when set and non-empty, else
/// `~/.local/state/llmux`.
fn state_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("llmux"));
        }
    }
    dirs::home_dir().map(|home| home.join(".local/state/llmux"))
}

/// Poll the status endpoint until the server answers as llmux, or fail
/// after `timeout`.
async fn wait_until_ready(
    port: u16,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<(), CliError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let ServerProbe::Running { .. } = probe_server(port, api_key).await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::Message(format!(
                "server did not become ready within {}s on port {port}",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Cooperatively shut down the llmux daemon on `port` and wait up to
/// `timeout` for the port to free: POST `/llmux/shutdown` (hyper graceful
/// shutdown — in-flight requests finish) then poll [`probe_server`] until
/// `NotRunning`. Never SIGKILLs; if the port is still held at the deadline it
/// returns an error. The caller is responsible for having confirmed a
/// llmux (not foreign) daemon is on the port first.
async fn shutdown_and_wait(
    port: u16,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<(), CliError> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|err| CliError::Message(format!("http client init failed: {err}")))?;
    let url = format!("{}/llmux/shutdown", proxy_base_url(port));
    let mut request = client.post(&url);
    if let Some(api_key) = api_key {
        request = request.header("x-api-key", api_key);
    }
    let response = request
        .send()
        .await
        .map_err(|err| CliError::Message(format!("shutdown request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(CliError::Message(format!(
            "server returned {} for {url}",
            response.status()
        )));
    }

    let deadline = Instant::now() + timeout;
    loop {
        if let ServerProbe::NotRunning = probe_server(port, api_key).await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::Message(format!(
                "server acknowledged shutdown but port {port} did not free within {}s",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `llmux stop` — cooperatively shut down the running server and wait for
/// the port to release (5s budget). A missing server is not an error
/// (idempotent stop); a foreign listener is refused.
pub async fn stop(_args: StopArgs) -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;
    let port = config.proxy.port;
    let api_key = config.proxy.api_key.as_deref();

    match probe_server(port, api_key).await? {
        ServerProbe::NotRunning => {
            println!("server not running on port {port}");
            return Ok(());
        }
        ServerProbe::Foreign { detail } => {
            return Err(CliError::Message(format!(
                "port {port} is in use by something that is not llmux ({detail}) — refusing to stop it"
            )));
        }
        ServerProbe::Running { .. } => {}
    }

    shutdown_and_wait(port, api_key, READY_TIMEOUT).await?;
    println!("stopped llmux server on port {port}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use http::StatusCode;

    fn llmux_status_body() -> String {
        serde_json::json!({
            "version": crate::build_info::version_string(),
            "current": null,
            "accounts": [],
        })
        .to_string()
    }

    #[test]
    fn classify_probe_accepts_llmux_shape() {
        let probe = classify_probe(StatusCode::OK, &llmux_status_body());
        assert!(matches!(probe, ServerProbe::Running { .. }), "{probe:?}");
    }

    #[test]
    fn classify_probe_rejects_non_llmux_bodies() {
        for body in [
            "<html>hello</html>",
            "{}",
            r#"{"version":"nginx/1.25","accounts":[]}"#,
            r#"{"version":"llmux 0.1.0 (dev dev)"}"#, // no accounts array
        ] {
            let probe = classify_probe(StatusCode::OK, body);
            assert!(
                matches!(probe, ServerProbe::Foreign { .. }),
                "{body}: {probe:?}"
            );
        }
    }

    #[test]
    fn classify_probe_rejects_non_2xx() {
        let probe = classify_probe(StatusCode::NOT_FOUND, &llmux_status_body());
        assert!(matches!(probe, ServerProbe::Foreign { .. }), "{probe:?}");
    }

    /// The probe-then-attach decision: a running daemon's status document
    /// classifies as `Running` (the trigger for attach mode) and its pid is
    /// extracted for the attach-mode header marker.
    #[test]
    fn running_probe_yields_attach_pid() {
        let body = serde_json::json!({
            "version": crate::build_info::version_string(),
            "pid": 4321u32,
            "accounts": [],
        })
        .to_string();
        let probe = classify_probe(StatusCode::OK, &body);
        let ServerProbe::Running { status } = probe else {
            panic!("expected Running, got {probe:?}");
        };
        assert_eq!(status_pid(&status), Some(4321));
    }

    #[test]
    fn status_pid_is_none_without_the_field() {
        // Older server (status without a pid) → attach still works, header
        // just shows "pid ?".
        let doc = serde_json::json!({ "version": "llmux 0.1.0", "accounts": [] });
        assert_eq!(status_pid(&doc), None);
    }

    /// The version-gated restart decision matrix (`should_restart`).
    #[test]
    fn should_restart_matrix() {
        let cur = "llmux 0.1.0 (dev dev)";
        let same = "llmux 0.1.0 (dev dev)";
        let other = "llmux 0.1.0 (preview preview-20260612-abc1234)";

        // force always wins, regardless of version (or its absence).
        assert!(should_restart(Some(same), cur, true), "force + same");
        assert!(should_restart(Some(other), cur, true), "force + different");
        assert!(should_restart(None, cur, true), "force + unparseable");

        // Without force: only a parseable, differing version restarts.
        assert!(
            !should_restart(Some(same), cur, false),
            "same version must reuse, never churn"
        );
        assert!(
            should_restart(Some(other), cur, false),
            "different version must restart"
        );
        assert!(
            !should_restart(None, cur, false),
            "unparseable version must not churn on its own"
        );
    }

    /// Serve `body` (200) at `/llmux/status` on 127.0.0.1:0.
    async fn spawn_status_mock(body: String) -> u16 {
        let app = Router::new().route("/llmux/status", get(move || async move { body }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    #[tokio::test]
    async fn probe_detects_running_llmux() {
        let port = spawn_status_mock(llmux_status_body()).await;
        let probe = probe_server(port, Some("lm-key")).await.unwrap();
        assert!(matches!(probe, ServerProbe::Running { .. }), "{probe:?}");
    }

    #[tokio::test]
    async fn probe_flags_foreign_listener() {
        let port = spawn_status_mock("welcome to my blog".into()).await;
        let probe = probe_server(port, None).await.unwrap();
        assert!(matches!(probe, ServerProbe::Foreign { .. }), "{probe:?}");
    }

    #[tokio::test]
    async fn probe_reports_not_running_on_refused_connection() {
        // Bind then drop to reserve-and-free a port nobody listens on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let probe = probe_server(port, None).await.unwrap();
        assert!(matches!(probe, ServerProbe::NotRunning), "{probe:?}");
    }

    #[tokio::test]
    async fn wait_until_ready_succeeds_against_live_server_and_times_out_otherwise() {
        let port = spawn_status_mock(llmux_status_body()).await;
        wait_until_ready(port, None, Duration::from_secs(1))
            .await
            .expect("live server is ready");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = wait_until_ready(dead_port, None, Duration::from_millis(150))
            .await
            .expect_err("nothing listening must time out");
        assert!(
            err.to_string().contains("did not become ready"),
            "unexpected error: {err}"
        );
    }
}
