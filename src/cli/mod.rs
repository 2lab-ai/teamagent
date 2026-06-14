//! CLI surface (FR5) — mirrors teamclaude's commands. Arg structs here are
//! COMPLETE (they are the user-facing contract); handlers live in their own
//! files and are `todo!()` until the port lands.

pub mod accounts;
pub mod api;
pub mod daemon;
pub mod env;
pub mod import;
pub mod login;
pub mod run;
pub mod status;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Auth(#[from] crate::auth::AuthError),
    #[error(transparent)]
    Proxy(#[from] crate::proxy::ProxyError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Parser)]
#[command(
    name = "llmux",
    version = crate::build_info::version_with_build(),
    about = "Multi-account LLM proxy for Claude Code with quota-maximizing scheduling"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the proxy (TUI dashboard on a TTY, plain logs otherwise).
    Server(ServerArgs),
    /// Spawn `claude` with ANTHROPIC_BASE_URL pointed at the proxy
    /// (auto-starts the server as a background daemon when needed).
    Run(RunArgs),
    /// Stop a running server (POST /llmux/shutdown, wait for the port
    /// to free).
    Stop(StopArgs),
    /// Restart the daemon: cooperatively drain a running server (if any),
    /// then spawn this binary's version. Does not exec `claude`.
    Restart(RestartArgs),
    /// Add an account via browser OAuth (or paste an API key with --api).
    Login(LoginArgs),
    /// Import accounts from teamclaude config, ~/.claude/.credentials.json,
    /// or inline JSON.
    Import(ImportArgs),
    /// Print the env exports for pointing Claude Code at the proxy.
    Env(EnvArgs),
    /// Attach to a running daemon and render its dashboard (read-only except
    /// manual switch). Polls `GET /llmux/dashboard` over HTTP.
    Dashboard(DashboardArgs),
    /// Show scheduler/account state (from a running server when available).
    ///
    /// Exit codes: 0 = server running, 1 = server not running (or error).
    Status(StatusArgs),
    /// List configured accounts.
    Accounts(AccountsArgs),
    /// Remove an account by name.
    Remove(RemoveArgs),
    /// Debug: perform a GET against the upstream API on the current account.
    Api(ApiArgs),
}

#[derive(Debug, Args)]
pub struct ServerArgs {
    /// Override the configured listen port.
    #[arg(long)]
    pub port: Option<u16>,
    /// Force plain log output even on a TTY (no TUI).
    #[arg(long)]
    pub no_tui: bool,
    /// Write one log file per proxied request into DIR (credentials masked).
    #[arg(long, value_name = "DIR")]
    pub log_to: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Restart the daemon even when it already runs this binary's version
    /// (by default a same-version daemon is reused; a different version is
    /// always restarted).
    #[arg(long)]
    pub force: bool,
    /// Arguments passed through to `claude` after `--`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct StopArgs {}

#[derive(Debug, Args)]
pub struct RestartArgs {}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Add a manual API key instead of running the OAuth browser flow.
    #[arg(long)]
    pub api: bool,
    /// Add an OpenAI Codex (ChatGPT subscription) account via the ChatGPT
    /// OAuth browser flow instead of the Claude flow.
    #[arg(long, conflicts_with = "api")]
    pub codex: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Path to a teamclaude config or a ~/.claude/.credentials.json file.
    /// Defaults to probing both well-known locations.
    #[arg(long)]
    pub from: Option<PathBuf>,
    /// Inline JSON credential blob (single account or array).
    #[arg(long, conflicts_with = "from")]
    pub json: Option<String>,
}

#[derive(Debug, Args)]
pub struct EnvArgs {}

#[derive(Debug, Args)]
pub struct DashboardArgs {}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Emit raw JSON instead of the human-readable table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct AccountsArgs {
    /// Include window/cooldown detail per account.
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Account name as shown by `llmux accounts`.
    pub name: String,
    /// Skip the confirmation prompt (required when stdin is not a TTY).
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ApiArgs {
    /// Upstream path to GET (e.g. /api/oauth/usage).
    pub path: String,
}

/// Dispatch a parsed CLI invocation to its handler.
///
/// Tracing init lives here (not in `main`): the server command chooses its
/// own subscriber once it knows whether the TUI owns the terminal; every
/// other command logs plainly to stderr.
pub async fn dispatch(cli: Cli) -> Result<(), CliError> {
    // `server` and `dashboard` are (potentially) TUI commands: they pick
    // their own subscriber once they know whether ratatui owns the terminal.
    // Nothing else may write to the terminal under a live TUI.
    if !matches!(cli.command, Command::Server(_) | Command::Dashboard(_)) {
        crate::logging::init_plain();
    }
    match cli.command {
        Command::Server(args) => server(args).await,
        Command::Run(args) => run::run(args).await,
        Command::Stop(args) => daemon::stop(args).await,
        Command::Restart(_) => daemon::restart().await,
        Command::Login(args) => login::run(args).await,
        Command::Import(args) => import::run(args).await,
        Command::Env(args) => env::run(args).await,
        Command::Dashboard(args) => dashboard(args).await,
        Command::Status(args) => status::run(args).await,
        Command::Accounts(args) => accounts::list(args).await,
        Command::Remove(args) => accounts::remove(args).await,
        Command::Api(args) => api::run(args).await,
    }
}

/// `llmux server` — start the proxy, rendering the in-process TUI on a
/// TTY (unless `--no-tui`).
///
/// herdr semantics (the daemon owns port 3456 and the only local TUI): before
/// touching the terminal we probe the port.
/// - A llmux daemon already runs → print one line and enter the SAME
///   attach mode `llmux dashboard` uses (read-only dashboard over HTTP).
/// - A FOREIGN process answers the port → clean one-line error, NO TUI init.
/// - Nothing is listening → bind FIRST (via `serve`'s readiness signal), and
///   only after the bind succeeds initialize the TUI, so a bind error can
///   never paint over a half-initialized frame again.
async fn server(args: ServerArgs) -> Result<(), CliError> {
    use std::io::IsTerminal as _;

    let mut config = crate::config::load_or_init()?;
    if let Some(port) = args.port {
        config.proxy.port = port;
    }
    let use_tui = !args.no_tui && std::io::stdout().is_terminal() && std::io::stdin().is_terminal();

    // herdr: is someone already on the port? (Cheap HTTP probe — no terminal
    // touched yet, so a foreign-process error stays a clean stderr line.)
    let port = config.proxy.port;
    let api_key = config.proxy.api_key.clone();
    match daemon::probe_server(port, api_key.as_deref()).await? {
        daemon::ServerProbe::Running { status } => {
            let pid = daemon::status_pid(&status);
            let pid_str = pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
            eprintln!("daemon already running (pid {pid_str}) — attaching…");
            // No bind, no plain-log init: hand straight to the attach TUI
            // (or, with --no-tui, a one-liner so scripts don't hang).
            if !use_tui {
                eprintln!(
                    "a llmux daemon already owns port {port}; run `llmux dashboard` to attach"
                );
                return Ok(());
            }
            return attach(port, api_key, pid).await;
        }
        daemon::ServerProbe::Foreign { detail } => {
            return Err(CliError::Message(format!(
                "port {port} is in use by something that is not llmux ({detail})\n\
                 Free the port or change proxy.port in the config."
            )));
        }
        daemon::ServerProbe::NotRunning => {}
    }

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

    // Tracing routing is decided before anything can log. TUI mode: the ONLY
    // output is the channel bridge into the log console pane — nothing may
    // write to the terminal except ratatui. Lines emitted before the first
    // draw just wait in the channel.
    let logs_rx = if use_tui {
        Some(crate::logging::init_tui_bridge())
    } else {
        crate::logging::init_plain();
        None
    };

    let pool = crate::scheduler::AccountPool::new(&config.accounts);
    let logger = match &args.log_to {
        Some(dir) => Some(std::sync::Arc::new(
            crate::proxy::logging::RequestLogger::new(dir.clone())
                .map_err(crate::proxy::ProxyError::from)?,
        )),
        None => None,
    };
    let state = crate::proxy::server::AppState::new(config, pool, logger, logs_rx)
        .map_err(CliError::Proxy)?;

    if !use_tui {
        // No TUI: serve in the foreground; the fold task re-traces activity
        // events into stderr (daemon parity).
        crate::proxy::server::serve(state, None).await?;
        return Ok(());
    }

    // TUI mode: bind BEFORE initializing the terminal. `serve` reports the
    // bound address on `ready`; if it fails to bind it returns the error
    // first, so we never call `ratatui::try_init` over a doomed server.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let tui_state = state.clone();
    let mut serve_task = tokio::spawn(crate::proxy::server::serve(state, Some(ready_tx)));
    tokio::select! {
        bound = ready_rx => {
            // `Err` means `serve` dropped the sender (it returned before
            // binding) — surface its error, not a generic "channel closed".
            if bound.is_err() {
                return match serve_task.await {
                    Ok(result) => result.map_err(CliError::Proxy),
                    Err(join) => Err(CliError::Message(format!("server task panicked: {join}"))),
                };
            }
        }
        result = &mut serve_task => {
            return match result {
                Ok(result) => result.map_err(CliError::Proxy),
                Err(join) => Err(CliError::Message(format!("server task panicked: {join}"))),
            };
        }
    }

    // Bind confirmed — now it is safe to own the terminal. Whichever side
    // finishes first (TUI quit, server error) ends the process.
    tokio::select! {
        result = crate::tui::run_local(tui_state) => result?,
        result = &mut serve_task => match result {
            Ok(result) => result?,
            Err(join) => return Err(CliError::Message(format!("server task panicked: {join}"))),
        },
    }
    Ok(())
}

/// `llmux dashboard` — attach to a running daemon and render its
/// dashboard. Refuses cleanly when no daemon (or a foreign process) is on the
/// port — there is nothing local to fall back to here.
async fn dashboard(_args: DashboardArgs) -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;
    let port = config.proxy.port;
    let api_key = config.proxy.api_key.clone();
    match daemon::probe_server(port, api_key.as_deref()).await? {
        daemon::ServerProbe::Running { status } => {
            attach(port, api_key, daemon::status_pid(&status)).await
        }
        daemon::ServerProbe::NotRunning => Err(CliError::Message(format!(
            "no llmux daemon on port {port} — start one with `llmux server` or `llmux run`"
        ))),
        daemon::ServerProbe::Foreign { detail } => Err(CliError::Message(format!(
            "port {port} is in use by something that is not llmux ({detail})"
        ))),
    }
}

/// Enter attach mode against a confirmed llmux daemon: the remote TUI
/// polls `GET /llmux/dashboard` and renders the identical layout. No
/// tracing subscriber is installed (ratatui owns the terminal; the client has
/// no logs of its own to show).
async fn attach(port: u16, api_key: Option<String>, pid: Option<u32>) -> Result<(), CliError> {
    let opts = crate::tui::RemoteOptions {
        base_url: proxy_base_url(port),
        api_key,
        pid,
    };
    crate::tui::run_remote(opts).await?;
    Ok(())
}

/// `http://localhost:<port>` — the whole Claude Code integration contract.
pub(crate) fn proxy_base_url(port: u16) -> String {
    format!("http://localhost:{port}")
}

/// Print `prompt` on stderr and read one trimmed line from stdin.
pub(crate) fn prompt_line(prompt: &str) -> Result<String, CliError> {
    use std::io::Write as _;

    let mut stderr = std::io::stderr();
    write!(stderr, "{prompt}")?;
    stderr.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Wall clock as epoch milliseconds (saturating at 0 before the epoch).
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
