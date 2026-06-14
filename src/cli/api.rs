//! `llmux api <path>` — debug GET against the upstream API using the
//! current account's credential (e.g. `/api/oauth/usage`).

use crate::config::AccountCredential;

use super::{ApiArgs, CliError};

/// Perform the GET, print status + headers to stderr and the pretty JSON
/// body to stdout. Credentials are never echoed.
pub async fn run(args: ApiArgs) -> Result<(), CliError> {
    let config = crate::config::load_or_init()?;

    // Prefer an oauth account (most debug endpoints are oauth-only),
    // falling back to the first configured account.
    let account = config
        .accounts
        .iter()
        .find(|a| matches!(a.credential, AccountCredential::Oauth { .. }))
        .or_else(|| config.accounts.first())
        .ok_or_else(|| CliError::Message("no accounts configured (see `llmux login`)".into()))?;

    let url = if args.path.starts_with("http://") || args.path.starts_with("https://") {
        args.path.clone()
    } else {
        format!("{}{}", config.upstream.trim_end_matches('/'), args.path)
    };

    let client = reqwest::Client::new();
    let request = match &account.credential {
        AccountCredential::Oauth { access_token, .. } => client
            .get(&url)
            .header("authorization", format!("Bearer {access_token}")),
        AccountCredential::Apikey { api_key } => client.get(&url).header("x-api-key", api_key),
        // Debug GETs with a codex credential carry the chatgpt headers; the
        // caller is expected to pass a full codex backend URL.
        AccountCredential::Codex {
            access_token,
            account_id,
            ..
        } => client
            .get(&url)
            .header("authorization", format!("Bearer {access_token}"))
            .header("chatgpt-account-id", account_id),
    };

    let response = request
        .send()
        .await
        .map_err(|err| CliError::Message(format!("request to {url} failed: {err}")))?;

    eprintln!("{} (account: {})", response.status(), account.name);
    for (name, value) in response.headers() {
        eprintln!("  {name}: {}", value.to_str().unwrap_or("<binary>"));
    }
    eprintln!();

    let body = response
        .text()
        .await
        .map_err(|err| CliError::Message(format!("failed to read body: {err}")))?;
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value).unwrap_or(body)),
        Err(_) => println!("{body}"),
    }
    Ok(())
}
