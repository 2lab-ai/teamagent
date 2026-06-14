//! llmux binary entry: parse the CLI, dispatch.
//!
//! Tracing is initialized inside `cli::dispatch`, not here: the `server`
//! command must pick its own subscriber (TUI bridge on a TTY, plain stderr
//! otherwise — see `crate::logging`), so init has to happen after the
//! command is known.

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = llmux::cli::Cli::parse();
    match llmux::cli::dispatch(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
