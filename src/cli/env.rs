//! `llmux env` — print shell exports for pointing Claude Code at the
//! proxy (the whole integration contract: `ANTHROPIC_BASE_URL`).

use super::{proxy_base_url, CliError, EnvArgs};

/// Print `export ANTHROPIC_BASE_URL=http://localhost:<port>` (plus the
/// proxy api key when configured) for eval-style use:
/// `eval "$(llmux env)"`.
///
/// Note `llmux run` deliberately does NOT export `ANTHROPIC_API_KEY` —
/// leaving it unset keeps Claude Code in subscription mode. It is printed
/// here for clients that must authenticate to the proxy from off-host.
pub async fn run(args: EnvArgs) -> Result<(), CliError> {
    let EnvArgs {} = args;
    let config = crate::config::load_or_init()?;
    println!(
        "export ANTHROPIC_BASE_URL={}",
        proxy_base_url(config.proxy.port)
    );
    if let Some(api_key) = &config.proxy.api_key {
        println!("export ANTHROPIC_API_KEY={api_key}");
    }
    Ok(())
}
