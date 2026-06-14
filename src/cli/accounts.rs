//! `llmux accounts [-v]` and `llmux remove <name>` — account
//! roster management. Both work purely from the config file (no network,
//! no running server required).

use crate::config::{AccountCredential, Config};

use super::{now_ms, prompt_line, AccountsArgs, CliError, RemoveArgs};

/// List configured accounts: name, type, tier when stored, masked
/// credential; `-v` adds token expiry detail.
pub async fn list(args: AccountsArgs) -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;

    if config.accounts.is_empty() {
        println!("No accounts configured.");
        println!("Add one with: llmux import, llmux login, or llmux login --api");
        return Ok(());
    }

    for (i, account) in config.accounts.iter().enumerate() {
        match &account.credential {
            AccountCredential::Apikey { api_key } => {
                println!("  [{}] {} (apikey)  {}", i + 1, account.name, mask(api_key));
            }
            AccountCredential::Oauth {
                account_uuid,
                expires_at_ms,
                tier,
                ..
            } => {
                let tier_label = tier
                    .as_deref()
                    .map(|t| format!(", {t}"))
                    .unwrap_or_default();
                println!("  [{}] {} (oauth{tier_label})", i + 1, account.name);
                if args.verbose {
                    if !account_uuid.is_empty() {
                        println!("       Uuid:  {account_uuid}");
                    }
                    println!(
                        "       Token: {}",
                        describe_expiry(*expires_at_ms, now_ms())
                    );
                }
            }
            AccountCredential::Codex {
                account_id,
                expires_at_ms,
                ..
            } => {
                println!("  [{}] {} (codex)", i + 1, account.name);
                if args.verbose {
                    if !account_id.is_empty() {
                        println!("       Account: {account_id}");
                    }
                    println!(
                        "       Token: {}",
                        describe_expiry(*expires_at_ms, now_ms())
                    );
                }
            }
        }
    }
    Ok(())
}

/// Remove one account by name via read-merge-write (`config::update`) so a
/// concurrently running server's writes are not clobbered. Asks for
/// confirmation unless `--yes` (non-TTY stdin requires `--yes`).
pub async fn remove(args: RemoveArgs) -> Result<(), CliError> {
    use std::io::IsTerminal as _;

    // Existence pre-check for a friendly error (re-checked inside update).
    let config = crate::config::load()?;
    if !config.accounts.iter().any(|a| a.name == args.name) {
        return Err(CliError::Message(format!(
            "account {:?} not found (see `llmux accounts`)",
            args.name
        )));
    }

    if !args.yes {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::Message(format!(
                "refusing to remove {:?} without confirmation; pass --yes",
                args.name
            )));
        }
        let answer = prompt_line(&format!("Remove account {:?}? [y/N] ", args.name))?;
        if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut removed = false;
    crate::config::update(|c: &mut Config| {
        removed = c.remove_account(&args.name);
    })?;

    if removed {
        println!("Removed account {:?}", args.name);
        Ok(())
    } else {
        // Lost a race with another writer that removed it first.
        Err(CliError::Message(format!(
            "account {:?} was already removed",
            args.name
        )))
    }
}

/// Show a credential prefix only — enough to recognize, useless to leak.
pub(crate) fn mask(secret: &str) -> String {
    let prefix: String = secret.chars().take(15).collect();
    if secret.chars().count() > 15 {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn describe_expiry(expires_at_ms: u64, now_ms: u64) -> String {
    if expires_at_ms == 0 {
        return "expiry unknown".to_string();
    }
    if expires_at_ms <= now_ms {
        return "expired".to_string();
    }
    let mins = (expires_at_ms - now_ms) / 60_000;
    let (hours, mins) = (mins / 60, mins % 60);
    if hours > 0 {
        format!("expires in {hours}h {mins}m")
    } else {
        format!("expires in {mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_keeps_prefix_only() {
        assert_eq!(mask("sk-ant-api03-SECRETSECRET"), "sk-ant-api03-SE...");
        assert_eq!(mask("short"), "short");
    }

    #[test]
    fn expiry_descriptions() {
        let now = 1_000_000_000_000;
        assert_eq!(describe_expiry(0, now), "expiry unknown");
        assert_eq!(describe_expiry(now - 1, now), "expired");
        assert_eq!(describe_expiry(now + 5 * 60_000, now), "expires in 5m");
        assert_eq!(describe_expiry(now + 90 * 60_000, now), "expires in 1h 30m");
    }
}
