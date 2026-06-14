//! `llmux run [-- args]` — ensure the proxy is running (auto-starting a
//! background daemon when needed), then spawn `claude` with the proxy env
//! injected.

use super::daemon::{ensure_server_running, EnsureOutcome};
use super::{proxy_base_url, CliError, RunArgs};

/// Ensure a server is listening (herdr-style auto-start: detached daemon +
/// readiness wait — see `cli::daemon`), then spawn `claude` with
/// `ANTHROPIC_BASE_URL=http://localhost:<port>` and pass-through args, and
/// propagate its exit code.
///
/// Only `ANTHROPIC_BASE_URL` is set — Claude Code keeps its own OAuth
/// token (which the proxy accepts from localhost); not setting
/// `ANTHROPIC_API_KEY` keeps it in subscription mode.
pub async fn run(args: RunArgs) -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;

    match ensure_server_running(&config, args.force).await? {
        EnsureOutcome::Started { pid } => {
            eprintln!(
                "started llmux server (pid {pid}) on port {}",
                config.proxy.port
            );
        }
        EnsureOutcome::Restarted { pid } => {
            eprintln!(
                "restarted llmux server (pid {pid}) on port {} → {}",
                config.proxy.port,
                crate::build_info::version_string()
            );
        }
        EnsureOutcome::AlreadyRunning => {}
    }

    let mut claude_args = args.args.as_slice();
    if claude_args.first().map(String::as_str) == Some("--") {
        claude_args = &claude_args[1..];
    }

    let status = tokio::process::Command::new("claude")
        .args(claude_args)
        .env("ANTHROPIC_BASE_URL", proxy_base_url(config.proxy.port))
        .status()
        .await
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                CliError::Message("claude not found in PATH — install Claude Code first".into())
            } else {
                CliError::Message(format!("failed to start claude: {err}"))
            }
        })?;

    std::process::exit(exit_code(&status));
}

/// Child exit code; signal terminations map to the conventional 128+N.
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}
