//! `llmux import [--from PATH | --json J]` — bring accounts in from
//! teamclaude config, `~/.claude/.credentials.json`, or inline JSON.
//! Works fully offline: profile enrichment is a TODO until `auth` lands.

use crate::config::migrate;
use crate::config::{AccountCredential, Config, Upsert};

use super::{CliError, ImportArgs};

/// Detect the source shape (teamclaude config vs claudeAiOauth envelope vs
/// inline JSON), convert to `AccountConfig`s, and upsert by
/// `account_uuid`/name (FR2).
///
/// TODO(auth): once `auth::profile::fetch_profile` is implemented, enrich
/// oauth accounts whose `account_uuid` is empty (uuid, email-as-name,
/// tier). Until then imports still work offline — names default from file
/// content or to `account-N`, and dedup falls back to name matching.
pub async fn run(args: ImportArgs) -> Result<(), CliError> {
    let imported = collect_accounts(&args)?;
    if imported.is_empty() {
        return Err(CliError::Message("no accounts found to import".into()));
    }

    let mut outcomes: Vec<(String, Upsert)> = Vec::new();
    crate::config::update(|config: &mut Config| {
        for mut account in imported {
            if account.name.is_empty() {
                account.name = default_name(config, &account.credential);
            }
            let name = account.name.clone();
            let outcome = config.upsert_account(account);
            outcomes.push((name, outcome));
        }
    })?;

    for (name, outcome) in &outcomes {
        match outcome {
            Upsert::Added => println!("Added account {name:?}"),
            Upsert::Updated => println!("Updated account {name:?}"),
        }
    }
    println!("Saved to {}", crate::config::config_path()?.display());
    Ok(())
}

fn collect_accounts(args: &ImportArgs) -> Result<Vec<crate::config::AccountConfig>, CliError> {
    if let Some(json) = &args.json {
        return Ok(migrate::import_inline_json(json)?);
    }
    if let Some(path) = &args.from {
        return Ok(migrate::import_file(path)?);
    }

    // No source given: probe both well-known locations.
    let mut accounts = Vec::new();
    let mut probed = Vec::new();

    let teamclaude = migrate::default_teamclaude_path()?;
    if teamclaude.exists() {
        let import = migrate::import_teamclaude_config(&teamclaude)?;
        for reason in &import.skipped {
            eprintln!("warning: {}: skipped {reason}", teamclaude.display());
        }
        println!(
            "Found teamclaude config at {} ({} account(s))",
            teamclaude.display(),
            import.accounts.len()
        );
        accounts.extend(import.accounts);
    }
    probed.push(teamclaude);

    let credentials = migrate::default_claude_credentials_path()?;
    if credentials.exists() {
        let imported = migrate::import_claude_credentials(&credentials)?;
        println!("Found Claude Code credentials at {}", credentials.display());
        accounts.extend(imported);
    }
    probed.push(credentials);

    // Codex CLI credentials (`~/.codex/auth.json`) → a codex account. A
    // malformed file is a warning, never a failed import of the others.
    if let Some(codex) = crate::auth::codex::default_codex_auth_path() {
        if codex.exists() {
            match crate::auth::codex::import_codex_auth(&codex) {
                Ok(account) => {
                    println!(
                        "Found Codex credentials at {} (account {:?})",
                        codex.display(),
                        account.name
                    );
                    accounts.push(account);
                }
                Err(err) => eprintln!("warning: {}: skipped ({err})", codex.display()),
            }
        }
        probed.push(codex);
    }

    if accounts.is_empty() {
        let probed: Vec<String> = probed.iter().map(|p| p.display().to_string()).collect();
        return Err(CliError::Message(format!(
            "nothing to import; probed {} — use --from PATH or --json",
            probed.join(", ")
        )));
    }
    Ok(accounts)
}

/// Default name for an unnamed import: `account-N` / `api-N`, counting
/// existing accounts with the same prefix (teamclaude behavior).
fn default_name(config: &Config, credential: &AccountCredential) -> String {
    let prefix = match credential {
        AccountCredential::Oauth { .. } => "account-",
        AccountCredential::Apikey { .. } => "api-",
        AccountCredential::Codex { .. } => "codex-",
    };
    let mut n = config
        .accounts
        .iter()
        .filter(|a| a.name.starts_with(prefix))
        .count()
        + 1;
    // Counting collides when names were customized; bump until free.
    loop {
        let candidate = format!("{prefix}{n}");
        if !config.accounts.iter().any(|a| a.name == candidate) {
            return candidate;
        }
        n += 1;
    }
}
