//! `llmux login [--api | --codex]` — add an account.

use crate::auth::{codex, oauth, profile};
use crate::config::{AccountConfig, AccountCredential, Config, Upsert};

use super::{prompt_line, CliError, LoginArgs};

/// OAuth path: PKCE browser flow → profile fetch (accountUuid, email) →
/// upsert into config by `account_uuid` (FR2 dedup).
/// `--api` path: prompt for an API key, store as an apikey account.
/// `--codex` path: ChatGPT OAuth browser flow → upsert a Codex account.
pub async fn run(args: LoginArgs) -> Result<(), CliError> {
    if args.codex {
        login_codex().await
    } else if args.api {
        login_api().await
    } else {
        login_oauth().await
    }
}

async fn login_api() -> Result<(), CliError> {
    let api_key = prompt_line("Anthropic API key: ")?;
    if api_key.is_empty() {
        return Err(CliError::Message("no API key provided".into()));
    }

    let mut name = String::new();
    crate::config::update(|config: &mut Config| {
        let n = config
            .accounts
            .iter()
            .filter(|a| a.name.starts_with("api-"))
            .count()
            + 1;
        name = format!("api-{n}");
        config.upsert_account(AccountConfig {
            name: name.clone(),
            credential: AccountCredential::Apikey {
                api_key: api_key.clone(),
            },
        });
    })?;

    println!("Added API key account {name:?}");
    println!("Saved to {}", crate::config::config_path()?.display());
    Ok(())
}

async fn login_oauth() -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;
    let client = reqwest::Client::new();

    println!("Starting OAuth login...");
    let tokens = oauth::login_interactive(&client).await?;

    // Profile fetch enriches uuid/name/tier; a failure degrades to an
    // unenriched account rather than losing the freshly minted tokens.
    let fetched = profile::fetch_profile(&client, &config.upstream, &tokens.access_token).await;
    let (account_uuid, name, tier) = match fetched {
        Ok(p) => {
            if let Some(tier) = &p.tier {
                println!("Detected Claude {tier} account: {}", p.email);
            }
            (p.account_uuid, p.email, p.tier)
        }
        Err(err) => {
            eprintln!("warning: could not fetch account profile — {err}");
            (String::new(), String::new(), None)
        }
    };

    let mut final_name = String::new();
    let mut outcome = Upsert::Added;
    crate::config::update(|config: &mut Config| {
        // Encode the model group in the name (`claude:<email>`) so the same
        // email can hold a Claude AND a Codex subscription without colliding —
        // mirrors the `codex:<email>` convention the `--codex` flow uses (req5).
        let resolved_name = if name.is_empty() {
            let n = config
                .accounts
                .iter()
                .filter(|a| a.name.starts_with("claude:account-"))
                .count()
                + 1;
            format!("claude:account-{n}")
        } else {
            format!("claude:{name}")
        };
        final_name = resolved_name.clone();
        outcome = config.upsert_account(AccountConfig {
            name: resolved_name,
            credential: AccountCredential::Oauth {
                account_uuid: account_uuid.clone(),
                access_token: tokens.access_token.clone(),
                // A fresh code exchange always carries a refresh token;
                // `None` (refresh-style response) degrades to empty.
                refresh_token: tokens.refresh_token.clone().unwrap_or_default(),
                expires_at_ms: tokens.expires_at_ms,
                tier: tier.clone(),
                // Login mints a brand-new token — that IS a refresh for
                // the dashboard's "refreshed ago" display.
                last_refresh_ms: Some(super::now_ms()),
            },
        });
    })?;

    match outcome {
        Upsert::Added => println!("Added account {final_name:?}"),
        Upsert::Updated => println!("Updated account {final_name:?}"),
    }
    println!("Saved to {}", crate::config::config_path()?.display());
    Ok(())
}

/// `--codex`: run the ChatGPT OAuth browser flow and upsert a Codex account.
/// Falls back to importing `~/.codex/auth.json` (renamed to the
/// `codex:{email}` convention) when the interactive flow cannot run.
async fn login_codex() -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;
    let client = reqwest::Client::new();

    println!("Starting ChatGPT (Codex) OAuth login...");
    let account = match codex::login_codex_interactive(&client, &config.codex.token_url).await {
        Ok(account) => account,
        Err(err) => {
            // Headless / no-browser / port-bind failures degrade to importing
            // the codex CLI's own credential store, still renamed to the
            // `codex:{email}` convention so it never collides with a Claude
            // account of the same email.
            eprintln!("warning: interactive ChatGPT login failed ({err})");
            account_from_codex_import()?.ok_or_else(|| {
                CliError::Message(
                    "interactive ChatGPT login failed and no ~/.codex/auth.json was found to \
                         import — run `codex login` first, or retry with a browser available"
                        .into(),
                )
            })?
        }
    };

    let final_name = account.name.clone();
    let mut outcome = Upsert::Added;
    crate::config::update(|config: &mut Config| {
        outcome = config.upsert_account(account.clone());
    })?;

    match outcome {
        Upsert::Added => println!("Added codex account {final_name:?}"),
        Upsert::Updated => println!("Updated codex account {final_name:?}"),
    }
    println!("Saved to {}", crate::config::config_path()?.display());
    Ok(())
}

/// Import `~/.codex/auth.json` (when present) and rename it to the
/// `codex:{email}` convention. `Ok(None)` when no auth.json exists.
fn account_from_codex_import() -> Result<Option<AccountConfig>, CliError> {
    let Some(path) = codex::default_codex_auth_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let mut account = codex::import_codex_auth(&path)?;
    // `import_codex_auth` names the account after the raw email (or "codex");
    // re-derive the `codex:{email}` name so imports match OAuth logins.
    let account_id = account
        .credential
        .account_uuid()
        .unwrap_or_default()
        .to_string();
    let email = (account.name != "codex").then_some(account.name.as_str());
    account.name = codex::codex_account_name(email, &account_id);
    Ok(Some(account))
}
