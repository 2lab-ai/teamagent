//! CLI black-box integration tests: spawn the BUILT binary
//! (`env!("CARGO_BIN_EXE_llmux")`) and assert its user-facing contract —
//! stdout/stderr strings and process exit codes.
//!
//! This is deliberately distinct from `tests/e2e.rs`, which drives the proxy
//! *in-process* (`llmux::proxy::server::serve`) and never spawns the binary.
//! Here every assertion is against the real CLI surface a user/script sees.
//!
//! Hermeticity (enforced on EVERY test):
//!   * `$LLMUX_CONFIG` points at a per-test tempdir file, so the binary never
//!     reads or writes the real `~/.config/llmux.json`.
//!   * Every seeded config uses a non-default, unique high port, and no test
//!     starts a real daemon or makes a network call (the one exception, the
//!     `run` test, talks only to an in-test loopback status mock it controls).
//!
//! Each test names the spec row IDs it covers in a comment.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path to the binary cargo built for this test target.
const BIN: &str = env!("CARGO_BIN_EXE_llmux");

/// A self-cleaning temp directory (no `tempfile` dev-dep in this crate, so a
/// tiny hand-rolled one keeps the test tree dependency-free). The directory is
/// created under the system temp dir with a process+counter-unique name and
/// removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "llmux-cli-test-{}-{}-{n}",
            std::process::id(),
            // Nanos give intra-process uniqueness even across counter resets.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create tempdir");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A hermetic test harness: a tempdir holding the config file `$LLMUX_CONFIG`
/// will point at. Seed the config (or let a `login`/`import` create it), then
/// run the binary through [`Harness::cmd`].
struct Harness {
    dir: TempDir,
    config_path: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new();
        let config_path = dir.path().join("llmux.json");
        Harness { dir, config_path }
    }

    /// Write `json` verbatim as the config file `$LLMUX_CONFIG` resolves to.
    fn seed_config(&self, json: &str) {
        std::fs::write(&self.config_path, json).expect("seed config");
    }

    /// Read the config file back as JSON (to assert a command's effect).
    fn read_config(&self) -> serde_json::Value {
        let raw = std::fs::read_to_string(&self.config_path).expect("read config");
        serde_json::from_str(&raw).expect("parse config")
    }

    /// Build a `Command` for the built binary with the hermetic env applied:
    /// `$LLMUX_CONFIG` → this harness's config file, `XDG_*` redirected into
    /// the tempdir so even commands that resolve a state dir never touch the
    /// real home, and demo mode explicitly off.
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env("LLMUX_CONFIG", &self.config_path)
            .env("XDG_CONFIG_HOME", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path())
            .env_remove("LLMUX_DEMO_MODE");
        cmd
    }
}

/// A captured process result, with helpers for asserting on it.
struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    fn run(mut cmd: Command) -> Self {
        let out = cmd.output().expect("spawn llmux binary");
        Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Run with `input` fed to the child's stdin (closed after the write).
    fn run_with_stdin(mut cmd: Command, input: &str) -> Self {
        use std::process::Stdio;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn llmux binary");
        child
            .stdin
            .take()
            .expect("child stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
        let out = child.wait_with_output().expect("wait for llmux binary");
        Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// A minimal valid v1 config with the given port and account array, ready to
/// hand to [`Harness::seed_config`]. `accounts` is a raw JSON array literal.
fn config_json(port: u16, accounts: &str) -> String {
    format!(r#"{{"version":1,"proxy":{{"port":{port}}},"accounts":{accounts}}}"#)
}

// ---------------------------------------------------------------------------
// DIST-11 — `--version` shape.
// ---------------------------------------------------------------------------

#[test]
fn version_matches_dist11_shape() {
    // DIST-11: `llmux --version` → `llmux <semver> (<channel> <build>)`.
    let h = Harness::new();
    let out = Output::run({
        let mut c = h.cmd();
        c.arg("--version");
        c
    });
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let line = out.stdout.trim_end();
    // ^llmux \d+\.\d+\.\d+ \(.+ .+\)$ — checked by structure (no regex dep).
    let rest = line
        .strip_prefix("llmux ")
        .unwrap_or_else(|| panic!("missing 'llmux ' prefix: {line:?}"));
    let (semver, build) = rest
        .split_once(' ')
        .unwrap_or_else(|| panic!("no space after semver: {line:?}"));
    let parts: Vec<&str> = semver.split('.').collect();
    assert_eq!(parts.len(), 3, "semver must be X.Y.Z: {semver:?}");
    assert!(
        parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "semver components must be digits: {semver:?}"
    );
    // `(.+ .+)` — a parenthesized "channel build" with both parts non-empty.
    let inner = build
        .strip_prefix('(')
        .and_then(|b| b.strip_suffix(')'))
        .unwrap_or_else(|| panic!("build must be parenthesized: {build:?}"));
    let (channel, id) = inner
        .split_once(' ')
        .unwrap_or_else(|| panic!("build needs 'channel id': {inner:?}"));
    assert!(
        !channel.is_empty() && !id.is_empty(),
        "build parts: {inner:?}"
    );
}

// ---------------------------------------------------------------------------
// CLI-12 — `env` prints the integration exports.
// ---------------------------------------------------------------------------

#[test]
fn env_prints_base_url_export() {
    // CLI-12: `llmux env` → `export ANTHROPIC_BASE_URL=http://localhost:<port>`.
    let h = Harness::new();
    h.seed_config(&config_json(39101, "[]"));
    let out = Output::run({
        let mut c = h.cmd();
        c.arg("env");
        c
    });
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("export ANTHROPIC_BASE_URL=http://localhost:39101"),
        "stdout: {}",
        out.stdout
    );
    // No proxy.api_key configured → no ANTHROPIC_API_KEY export.
    assert!(
        !out.stdout.contains("ANTHROPIC_API_KEY"),
        "stdout leaked api key export: {}",
        out.stdout
    );
}

#[test]
fn env_prints_api_key_export_when_proxy_key_set() {
    // CLI-12: with proxy.api_key set, `env` also emits the ANTHROPIC_API_KEY
    // export (for off-host clients that must authenticate to the proxy).
    let h = Harness::new();
    h.seed_config(
        r#"{"version":1,"proxy":{"port":39102,"api_key":"lm-secret-key"},"accounts":[]}"#,
    );
    let out = Output::run({
        let mut c = h.cmd();
        c.arg("env");
        c
    });
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("export ANTHROPIC_BASE_URL=http://localhost:39102"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("export ANTHROPIC_API_KEY=lm-secret-key"),
        "stdout: {}",
        out.stdout
    );
}

// ---------------------------------------------------------------------------
// CLI-14 / CLI-15 — `status` with no server.
// ---------------------------------------------------------------------------

#[test]
fn status_no_server_reports_not_running_and_exits_1() {
    // CLI-14: human-readable `status` with nothing on the port → the
    // "not running" section and exit code 1.
    let h = Harness::new();
    h.seed_config(&config_json(39103, "[]"));
    let out = Output::run({
        let mut c = h.cmd();
        c.arg("status");
        c
    });
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("status: not running"), "{}", out.stdout);
    assert!(out.stdout.contains("port: 39103"), "{}", out.stdout);
}

#[test]
fn status_json_no_server_emits_document_and_exits_1() {
    // CLI-15: `status --json` with no server → a JSON document
    // {"server":"not running","port":<port>} and exit code 1.
    let h = Harness::new();
    h.seed_config(&config_json(39104, "[]"));
    let out = Output::run({
        let mut c = h.cmd();
        c.args(["status", "--json"]);
        c
    });
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    let doc: serde_json::Value = serde_json::from_str(out.stdout.trim())
        .unwrap_or_else(|e| panic!("not JSON: {e}\n{}", out.stdout));
    assert_eq!(doc["server"], "not running", "{doc}");
    assert_eq!(doc["port"], 39104, "{doc}");
}

// ---------------------------------------------------------------------------
// CLI-16 / CLI-17 — `accounts` listing.
// ---------------------------------------------------------------------------

#[test]
fn accounts_empty_prints_guidance_and_exits_0() {
    // CLI-16: `accounts` with no configured accounts → the "No accounts
    // configured." guidance and exit 0.
    let h = Harness::new();
    h.seed_config(&config_json(39105, "[]"));
    let out = Output::run({
        let mut c = h.cmd();
        c.arg("accounts");
        c
    });
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("No accounts configured."),
        "{}",
        out.stdout
    );
}

#[test]
fn accounts_apikey_is_masked() {
    // CLI-16: a seeded apikey account lists as a masked line (prefix + "...",
    // never the full secret).
    let h = Harness::new();
    h.seed_config(&config_json(
        39106,
        r#"[{"name":"api-1","type":"apikey","api_key":"sk-ant-api03-SECRETSECRETSECRET"}]"#,
    ));
    let out = Output::run({
        let mut c = h.cmd();
        c.arg("accounts");
        c
    });
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("api-1"), "{}", out.stdout);
    assert!(out.stdout.contains("(apikey)"), "{}", out.stdout);
    // Masked: first 15 chars + "..." present, full secret absent.
    assert!(out.stdout.contains("sk-ant-api03-SE..."), "{}", out.stdout);
    assert!(
        !out.stdout.contains("SECRETSECRETSECRET"),
        "full secret leaked: {}",
        out.stdout
    );
}

#[test]
fn accounts_verbose_shows_oauth_token_detail() {
    // CLI-17: `accounts -v` on an oauth account adds the verbose Uuid/Token
    // (expiry) detail lines that the non-verbose listing omits.
    let h = Harness::new();
    // Expiry ~2h in the future so the detail reads "expires in 1h ..".
    let expires_at_ms = (now_secs() + 2 * 3600) * 1000;
    h.seed_config(&config_json(
        39107,
        &format!(
            r#"[{{"name":"claude:me@x.com","type":"oauth","account_uuid":"uuid-abc",
                  "access_token":"sk-ant-oat01-AAA","refresh_token":"sk-ant-ort01-AAA",
                  "expires_at_ms":{expires_at_ms},"tier":"max"}}]"#
        ),
    ));

    // Non-verbose: no Uuid/Token detail.
    let plain = Output::run({
        let mut c = h.cmd();
        c.arg("accounts");
        c
    });
    assert_eq!(plain.code, Some(0), "stderr: {}", plain.stderr);
    assert!(plain.stdout.contains("(oauth, max)"), "{}", plain.stdout);
    assert!(!plain.stdout.contains("Token:"), "{}", plain.stdout);

    // Verbose: Uuid + Token expiry detail appear.
    let verbose = Output::run({
        let mut c = h.cmd();
        c.args(["accounts", "-v"]);
        c
    });
    assert_eq!(verbose.code, Some(0), "stderr: {}", verbose.stderr);
    assert!(verbose.stdout.contains("Uuid:"), "{}", verbose.stdout);
    assert!(verbose.stdout.contains("uuid-abc"), "{}", verbose.stdout);
    assert!(verbose.stdout.contains("Token:"), "{}", verbose.stdout);
    assert!(verbose.stdout.contains("expires in"), "{}", verbose.stdout);
}

// ---------------------------------------------------------------------------
// CLI-07 — `login --api`.
// ---------------------------------------------------------------------------

#[test]
fn login_api_writes_apikey_account_from_stdin() {
    // CLI-07: `login --api` reads the key from stdin and writes an apikey
    // account; assert by reading the config back.
    let h = Harness::new();
    h.seed_config(&config_json(39108, "[]"));
    let out = Output::run_with_stdin(
        {
            let mut c = h.cmd();
            c.args(["login", "--api"]);
            c
        },
        "sk-ant-api03-FROM-STDIN-1234567890\n",
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains(r#"Added API key account "api-1""#),
        "{}",
        out.stdout
    );

    let cfg = h.read_config();
    let accounts = cfg["accounts"].as_array().expect("accounts array");
    assert_eq!(accounts.len(), 1, "{cfg}");
    assert_eq!(accounts[0]["name"], "api-1", "{cfg}");
    assert_eq!(accounts[0]["type"], "apikey", "{cfg}");
    assert_eq!(
        accounts[0]["api_key"], "sk-ant-api03-FROM-STDIN-1234567890",
        "{cfg}"
    );
}

#[test]
fn login_api_empty_stdin_errors() {
    // CLI-07: empty stdin → "no API key provided" error, exit 1, config
    // unchanged (still no accounts).
    let h = Harness::new();
    h.seed_config(&config_json(39109, "[]"));
    let out = Output::run_with_stdin(
        {
            let mut c = h.cmd();
            c.args(["login", "--api"]);
            c
        },
        "\n",
    );
    assert_eq!(
        out.code,
        Some(1),
        "stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stderr.contains("no API key provided"),
        "stderr: {}",
        out.stderr
    );
    let cfg = h.read_config();
    assert!(cfg["accounts"].as_array().unwrap().is_empty(), "{cfg}");
}

// ---------------------------------------------------------------------------
// CLI-08 (partial) — `login` clap conflict.
// ---------------------------------------------------------------------------

#[test]
fn login_api_and_codex_conflict_exits_2() {
    // CLI-08 (partial): `--api` and `--codex` are mutually exclusive; clap
    // rejects the combination with usage error exit code 2.
    let h = Harness::new();
    h.seed_config(&config_json(39110, "[]"));
    let out = Output::run({
        let mut c = h.cmd();
        c.args(["login", "--api", "--codex"]);
        c
    });
    assert_eq!(
        out.code,
        Some(2),
        "stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stderr.contains("cannot be used with"),
        "stderr: {}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// CLI-10 / CLI-11 — `import`.
// ---------------------------------------------------------------------------

#[test]
fn import_from_file_imports_account() {
    // CLI-10: `import --from <fixture>` reads a teamclaude-shaped credentials
    // file and imports its account (assert via config read-back).
    let h = Harness::new();
    h.seed_config(&config_json(39111, "[]"));
    let fixture = h.dir.path().join("teamclaude.json");
    std::fs::write(
        &fixture,
        r#"{"accounts":[{"name":"primary","type":"oauth","accountUuid":"uuid-p",
            "accessToken":"sk-ant-oat01-X","refreshToken":"sk-ant-ort01-X",
            "expiresAt":1750000000000}]}"#,
    )
    .expect("write fixture");

    let out = Output::run({
        let mut c = h.cmd();
        c.args(["import", "--from"]).arg(&fixture);
        c
    });
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains(r#"Added account "primary""#),
        "{}",
        out.stdout
    );

    let cfg = h.read_config();
    let accounts = cfg["accounts"].as_array().expect("accounts");
    assert_eq!(accounts.len(), 1, "{cfg}");
    assert_eq!(accounts[0]["name"], "primary", "{cfg}");
    assert_eq!(accounts[0]["type"], "oauth", "{cfg}");
    assert_eq!(accounts[0]["account_uuid"], "uuid-p", "{cfg}");
}

#[test]
fn import_json_blob_imports_account() {
    // CLI-11: `import --json '<blob>'` imports a single inline account.
    let h = Harness::new();
    h.seed_config(&config_json(39112, "[]"));
    let out = Output::run({
        let mut c = h.cmd();
        c.args([
            "import",
            "--json",
            r#"{"name":"inline-api","type":"apikey","apiKey":"sk-ant-api03-INLINE"}"#,
        ]);
        c
    });
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains(r#"Added account "inline-api""#),
        "{}",
        out.stdout
    );

    let cfg = h.read_config();
    let accounts = cfg["accounts"].as_array().expect("accounts");
    assert_eq!(accounts.len(), 1, "{cfg}");
    assert_eq!(accounts[0]["name"], "inline-api", "{cfg}");
    assert_eq!(accounts[0]["type"], "apikey", "{cfg}");
    assert_eq!(accounts[0]["api_key"], "sk-ant-api03-INLINE", "{cfg}");
}

#[test]
fn import_from_and_json_conflict_exits_2() {
    // CLI-10/CLI-11: `--from` and `--json` are mutually exclusive; clap
    // rejects the combination with usage error exit code 2.
    let h = Harness::new();
    h.seed_config(&config_json(39113, "[]"));
    let out = Output::run({
        let mut c = h.cmd();
        c.args(["import", "--from", "/nonexistent/x", "--json", "{}"]);
        c
    });
    assert_eq!(
        out.code,
        Some(2),
        "stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stderr.contains("cannot be used with"),
        "stderr: {}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// CLI-18 — `remove`.
// ---------------------------------------------------------------------------

#[test]
fn remove_without_yes_non_tty_errors() {
    // CLI-18: `remove <name>` with stdin not a TTY and no --yes refuses
    // (exit 1) and leaves the account in place.
    let h = Harness::new();
    h.seed_config(&config_json(
        39114,
        r#"[{"name":"api-1","type":"apikey","api_key":"sk-ant-api03-X"}]"#,
    ));
    // Closed stdin (/dev/null-equivalent) → not a TTY.
    let out = Output::run_with_stdin(
        {
            let mut c = h.cmd();
            c.args(["remove", "api-1"]);
            c
        },
        "",
    );
    assert_eq!(
        out.code,
        Some(1),
        "stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stderr.contains("without confirmation; pass --yes"),
        "stderr: {}",
        out.stderr
    );
    let cfg = h.read_config();
    assert_eq!(cfg["accounts"].as_array().unwrap().len(), 1, "{cfg}");
}

#[test]
fn remove_with_yes_removes_account() {
    // CLI-18: `remove <name> --yes` on a seeded account removes it (assert via
    // config read-back: the array is now empty).
    let h = Harness::new();
    h.seed_config(&config_json(
        39115,
        r#"[{"name":"api-1","type":"apikey","api_key":"sk-ant-api03-X"}]"#,
    ));
    let out = Output::run({
        let mut c = h.cmd();
        c.args(["remove", "api-1", "--yes"]);
        c
    });
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains(r#"Removed account "api-1""#),
        "{}",
        out.stdout
    );
    let cfg = h.read_config();
    assert!(cfg["accounts"].as_array().unwrap().is_empty(), "{cfg}");
}

#[test]
fn remove_nonexistent_errors() {
    // CLI-18: `remove <missing> --yes` → a clean not-found error, exit 1.
    let h = Harness::new();
    h.seed_config(&config_json(
        39116,
        r#"[{"name":"api-1","type":"apikey","api_key":"sk-ant-api03-X"}]"#,
    ));
    let out = Output::run({
        let mut c = h.cmd();
        c.args(["remove", "ghost", "--yes"]);
        c
    });
    assert_eq!(
        out.code,
        Some(1),
        "stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(out.stderr.contains("not found"), "stderr: {}", out.stderr);
    // The real account survives the failed removal.
    let cfg = h.read_config();
    assert_eq!(cfg["accounts"].as_array().unwrap().len(), 1, "{cfg}");
}

// ---------------------------------------------------------------------------
// CLI-13 (refusal path) — `dashboard` with no daemon.
// ---------------------------------------------------------------------------

#[test]
fn dashboard_no_daemon_refuses_and_exits_1() {
    // CLI-13 (refusal path): `dashboard` with nothing on the port refuses
    // cleanly with a "no llmux daemon" error and exit 1 — no TUI, no hang.
    let h = Harness::new();
    h.seed_config(&config_json(39117, "[]"));
    let out = Output::run({
        let mut c = h.cmd();
        c.arg("dashboard");
        c
    });
    assert_eq!(
        out.code,
        Some(1),
        "stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stderr.contains("no llmux daemon on port 39117"),
        "stderr: {}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// CLI-02 — `run` injects ANTHROPIC_BASE_URL and propagates the child exit code.
// ---------------------------------------------------------------------------

/// CLI-02: `llmux run` must (a) inject `ANTHROPIC_BASE_URL=http://localhost:<port>`
/// into the spawned `claude` and (b) propagate `claude`'s exit code.
///
/// Hermetic wiring: we stand up our own loopback `/llmux/status` mock that
/// answers as a same-version llmux daemon, so `run` takes the
/// `AlreadyRunning` branch and never spawns a real daemon; and we put a stub
/// `claude` (which echoes `$ANTHROPIC_BASE_URL` and exits a chosen code) first
/// on the child's PATH. No network beyond loopback, no real daemon.
#[tokio::test]
async fn run_injects_base_url_and_propagates_exit_code() {
    use axum::routing::get;
    use axum::Router;

    // Mirror the binary's own version string verbatim so `should_restart`
    // sees a same-version daemon and reuses it (the restart path is never
    // taken — no real daemon is spawned).
    let version_out = std::process::Command::new(BIN)
        .arg("--version")
        .output()
        .expect("llmux --version");
    let version = String::from_utf8(version_out.stdout)
        .expect("utf8 version")
        .trim()
        .to_string();
    assert!(version.starts_with("llmux "), "version: {version}");

    // Bind a loopback status mock that classifies as a running llmux daemon.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind status mock");
    let port = listener.local_addr().expect("addr").port();
    let body = serde_json::json!({
        "version": version,
        "pid": 4321,
        "accounts": [],
    })
    .to_string();
    let app = Router::new().route("/llmux/status", get(move || async move { body }));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Hermetic config: an account (so `run` doesn't refuse on empty) and the
    // mock's port.
    let h = Harness::new();
    h.seed_config(&config_json(
        port,
        r#"[{"name":"api-1","type":"apikey","api_key":"sk-ant-api03-X"}]"#,
    ));

    // Stub `claude`: echo the injected base URL, exit a recognizable code (7).
    let bindir = h.dir.path().join("bin");
    std::fs::create_dir_all(&bindir).expect("bindir");
    let stub = bindir.join("claude");
    std::fs::write(
        &stub,
        "#!/bin/sh\necho \"BASE_URL=$ANTHROPIC_BASE_URL\"\nexit 7\n",
    )
    .expect("write stub claude");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
    }

    let path_env = format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let config_path = h.config_path.clone();
    let state_dir = h.dir.path().to_path_buf();

    // The child blocks (it waits on the stub), so run it off the async runtime
    // thread; the mock keeps serving on this runtime meanwhile.
    let out = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(BIN);
        cmd.arg("run")
            .env("LLMUX_CONFIG", &config_path)
            .env("XDG_CONFIG_HOME", &state_dir)
            .env("XDG_STATE_HOME", &state_dir)
            .env_remove("LLMUX_DEMO_MODE")
            .env("PATH", path_env);
        let o = cmd.output().expect("spawn llmux run");
        (
            o.status.code(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    })
    .await
    .expect("join run");

    server.abort();

    let (code, stdout, stderr) = out;
    // (a) base-url injection: the stub echoed exactly the proxy URL.
    assert!(
        stdout.contains(&format!("BASE_URL=http://localhost:{port}")),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    // (b) exit-code propagation: the stub's 7 reaches the caller.
    assert_eq!(code, Some(7), "stdout: {stdout}\nstderr: {stderr}");
}

/// Wall-clock seconds since the epoch (for building future token expiries).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
