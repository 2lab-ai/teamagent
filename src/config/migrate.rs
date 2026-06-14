//! Import accounts from teamclaude's `~/.config/teamclaude.json` (same
//! fields, camelCase), Claude Code's `~/.claude/.credentials.json`
//! (`claudeAiOauth` envelope), and inline JSON blobs.
//!
//! Kept self-contained on purpose: `auth::credentials` covers the same
//! envelope for the *auth* layer, but its implementation is still
//! mid-flight and importing must work without it (and without network).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{xdg_config_dir, AccountConfig, AccountCredential, ConfigError};

/// Everything worth carrying over from a teamclaude config.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamclaudeImport {
    pub accounts: Vec<AccountConfig>,
    /// `proxy.port` when present.
    pub proxy_port: Option<u16>,
    /// `switchThreshold` when present — maps onto `scheduler.five_hour_max`
    /// (teamclaude's single threshold is its 5h-window switch point).
    pub switch_threshold: Option<f64>,
    /// Human-readable reasons for entries that could not be converted.
    pub skipped: Vec<String>,
}

/// Default location of the teamclaude config this fork supersedes:
/// `$XDG_CONFIG_HOME/teamclaude.json`, else `~/.config/teamclaude.json`.
pub fn default_teamclaude_path() -> Result<PathBuf, ConfigError> {
    xdg_config_dir()
        .map(|dir| dir.join("teamclaude.json"))
        .ok_or(ConfigError::NoConfigDir)
}

/// Default location of Claude Code's own credential store:
/// `~/.claude/.credentials.json`.
pub fn default_claude_credentials_path() -> Result<PathBuf, ConfigError> {
    dirs::home_dir()
        .map(|home| home.join(".claude").join(".credentials.json"))
        .ok_or(ConfigError::NoConfigDir)
}

/// Parse a teamclaude config file and return its accounts converted to the
/// llmux schema (camelCase → snake_case, `expiresAt` normalized to ms).
/// Dedup against existing accounts is the caller's job (by `account_uuid`).
pub fn import_teamclaude(path: &Path) -> Result<Vec<AccountConfig>, ConfigError> {
    Ok(import_teamclaude_config(path)?.accounts)
}

/// [`import_teamclaude`] plus the settings worth migrating
/// (`proxy.port`, `switchThreshold`).
pub fn import_teamclaude_config(path: &Path) -> Result<TeamclaudeImport, ConfigError> {
    let value = read_json(path)?;
    let Some(obj) = value.as_object() else {
        return Err(ConfigError::Invalid(format!(
            "{}: expected a JSON object",
            path.display()
        )));
    };

    let proxy_port = obj
        .get("proxy")
        .and_then(|p| p.get("port"))
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok());
    let switch_threshold = obj.get("switchThreshold").and_then(Value::as_f64);

    let mut accounts = Vec::new();
    let mut skipped = Vec::new();
    let entries = obj.get("accounts").and_then(Value::as_array);
    for (i, entry) in entries.into_iter().flatten().enumerate() {
        match account_from_value(entry) {
            Ok(account) => accounts.push(account),
            Err(reason) => skipped.push(format!("accounts[{i}]: {reason}")),
        }
    }

    Ok(TeamclaudeImport {
        accounts,
        proxy_port,
        switch_threshold,
        skipped,
    })
}

/// Parse Claude Code's `~/.claude/.credentials.json` (`claudeAiOauth`
/// envelope) into a single unnamed oauth account. The caller assigns a
/// default name and (once `auth::profile` lands) enriches `account_uuid`.
pub fn import_claude_credentials(path: &Path) -> Result<Vec<AccountConfig>, ConfigError> {
    let value = read_json(path)?;
    if value.get("claudeAiOauth").is_none() {
        return Err(ConfigError::Invalid(format!(
            "{}: missing \"claudeAiOauth\" envelope",
            path.display()
        )));
    }
    account_from_value(&value)
        .map(|a| vec![a])
        .map_err(ConfigError::Invalid)
}

/// Read `path` and autodetect its shape:
/// - object with `claudeAiOauth` → Claude Code credentials envelope,
/// - object with `accounts` → teamclaude config (accounts only),
/// - array → list of account objects,
/// - any other object → single account object (either field naming).
pub fn import_file(path: &Path) -> Result<Vec<AccountConfig>, ConfigError> {
    let value = read_json(path)?;
    if value.get("accounts").is_some() {
        return import_teamclaude(path);
    }
    // Codex CLI `~/.codex/auth.json` shape: a `tokens` object carrying
    // `account_id` — handled by the auth layer's parser.
    if value
        .get("tokens")
        .is_some_and(|tokens| tokens.get("account_id").is_some())
    {
        return crate::auth::codex::import_codex_auth(path)
            .map(|account| vec![account])
            .map_err(|err| ConfigError::Invalid(err.to_string()));
    }
    accounts_from_value(&value).map_err(ConfigError::Invalid)
}

/// Parse an inline JSON credential blob (`llmux import --json`).
/// Accepts a single account object or an array of them, in either the
/// llmux or teamclaude field naming, or a `claudeAiOauth` envelope.
pub fn import_inline_json(json: &str) -> Result<Vec<AccountConfig>, ConfigError> {
    let value: Value = serde_json::from_str(json)?;
    accounts_from_value(&value).map_err(ConfigError::Invalid)
}

fn read_json(path: &Path) -> Result<Value, ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(serde_json::from_str(&raw)?)
}

fn accounts_from_value(value: &Value) -> Result<Vec<AccountConfig>, String> {
    match value {
        Value::Array(items) => items.iter().map(account_from_value).collect(),
        Value::Object(_) => account_from_value(value).map(|a| vec![a]),
        _ => Err("expected a JSON object or array of account objects".into()),
    }
}

/// Lenient single-account conversion. Accepts:
/// - `{"claudeAiOauth": {...}}` envelopes (tokens nested),
/// - teamclaude camelCase (`accessToken`, `refreshToken`, `expiresAt`,
///   `accountUuid`, `apiKey`) and llmux snake_case fields,
/// - `{"importFrom": "~/.claude/.credentials.json"}` indirection
///   (teamclaude config entries that defer to a credentials file),
/// - explicit `"type"` or inference from which credential fields exist.
fn account_from_value(value: &Value) -> Result<AccountConfig, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "account entry is not a JSON object".to_string())?;

    let name = get_str(obj, &["name"]).unwrap_or_default();

    // claudeAiOauth envelope: tokens live one level down, name (if any) up top.
    if let Some(envelope) = obj.get("claudeAiOauth") {
        let mut account = account_from_value(envelope)?;
        if account.name.is_empty() {
            account.name = name;
        }
        if let AccountCredential::Apikey { .. } = account.credential {
            return Err("\"claudeAiOauth\" envelope did not contain oauth tokens".into());
        }
        return Ok(account);
    }

    // teamclaude `importFrom` indirection: resolve the referenced
    // credentials file now (local file read, no network).
    if let Some(from) = get_str(obj, &["importFrom", "import_from"]) {
        if get_str(obj, &["accessToken", "access_token"]).is_none() {
            let path = expand_tilde(&from);
            let mut accounts =
                import_claude_credentials(&path).map_err(|e| format!("importFrom {from}: {e}"))?;
            // import_claude_credentials returns exactly one account.
            let mut account = accounts
                .pop()
                .ok_or_else(|| format!("importFrom {from}: empty credentials file"))?;
            if !name.is_empty() {
                account.name = name;
            }
            return Ok(account);
        }
    }

    let declared_type = get_str(obj, &["type"]);
    let access_token = get_str(obj, &["accessToken", "access_token"]);
    let api_key = get_str(obj, &["apiKey", "api_key"]);

    let is_oauth = match declared_type.as_deref() {
        Some("oauth") => true,
        Some("apikey") => false,
        Some(other) => return Err(format!("unknown account type {other:?}")),
        None => {
            // Infer: oauth wins when tokens are present.
            if access_token.is_some() {
                true
            } else if api_key.is_some() {
                false
            } else {
                return Err("cannot infer account type: neither access token nor api key".into());
            }
        }
    };

    if is_oauth {
        let access_token =
            access_token.ok_or_else(|| "oauth account is missing its access token".to_string())?;
        let refresh_token = get_str(obj, &["refreshToken", "refresh_token"]).unwrap_or_default();
        let account_uuid = get_str(obj, &["accountUuid", "account_uuid"]).unwrap_or_default();
        let expires_at_ms = obj
            .get("expiresAt")
            .or_else(|| obj.get("expires_at"))
            .or_else(|| obj.get("expires_at_ms"))
            .and_then(Value::as_u64)
            .map(normalize_expires_at_ms)
            .unwrap_or(0);
        Ok(AccountConfig {
            name,
            credential: AccountCredential::Oauth {
                account_uuid,
                access_token,
                refresh_token,
                expires_at_ms,
                tier: None,
                // Imported tokens are of unknown age — "never (refreshed)"
                // until the proxy's first refresh stamps it.
                last_refresh_ms: None,
            },
        })
    } else {
        let api_key = api_key.ok_or_else(|| "apikey account is missing its api key".to_string())?;
        Ok(AccountConfig {
            name,
            credential: AccountCredential::Apikey { api_key },
        })
    }
}

/// `expiresAt` may arrive in seconds or milliseconds: values `< 1e12` are
/// seconds and get multiplied by 1000 (`.prd/02-architecture.md` §OAuth).
fn normalize_expires_at_ms(raw: u64) -> u64 {
    if raw == 0 {
        0
    } else if raw < 1_000_000_000_000 {
        raw.saturating_mul(1000)
    } else {
        raw
    }
}

fn get_str(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| obj.get(*k))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::super::tests::TempDir;
    use super::*;

    const TEAMCLAUDE_FIXTURE: &str = r#"{
      "proxy": { "port": 4567, "apiKey": "tc-secret" },
      "upstream": "https://api.anthropic.com",
      "switchThreshold": 0.98,
      "accounts": [
        {
          "name": "primary-max",
          "type": "oauth",
          "accountUuid": "uuid-primary",
          "accessToken": "sk-ant-oat01-AAA",
          "refreshToken": "sk-ant-ort01-AAA",
          "expiresAt": 1750000000000
        },
        {
          "name": "secondary-max",
          "type": "oauth",
          "accessToken": "sk-ant-oat01-BBB",
          "refreshToken": "sk-ant-ort01-BBB",
          "expiresAt": 1750000000
        },
        { "name": "api-fallback", "type": "apikey", "apiKey": "sk-ant-api03-CCC" },
        { "name": "broken", "type": "oauth" }
      ]
    }"#;

    #[test]
    fn teamclaude_fixture_imports_accounts_and_settings() {
        let dir = TempDir::new();
        let path = dir.path().join("teamclaude.json");
        std::fs::write(&path, TEAMCLAUDE_FIXTURE).expect("write fixture");

        let import = import_teamclaude_config(&path).expect("import");
        assert_eq!(import.proxy_port, Some(4567));
        assert_eq!(import.switch_threshold, Some(0.98));
        assert_eq!(import.accounts.len(), 3);
        assert_eq!(import.skipped.len(), 1, "tokenless entry reported");
        assert!(
            import.skipped[0].contains("accounts[3]"),
            "{:?}",
            import.skipped
        );

        let primary = &import.accounts[0];
        assert_eq!(primary.name, "primary-max");
        match &primary.credential {
            AccountCredential::Oauth {
                account_uuid,
                access_token,
                refresh_token,
                expires_at_ms,
                tier,
                last_refresh_ms,
            } => {
                assert_eq!(account_uuid, "uuid-primary");
                assert_eq!(access_token, "sk-ant-oat01-AAA");
                assert_eq!(refresh_token, "sk-ant-ort01-AAA");
                assert_eq!(*expires_at_ms, 1_750_000_000_000);
                assert_eq!(*tier, None);
                assert_eq!(*last_refresh_ms, None, "import never stamps a refresh");
            }
            other => panic!("unexpected credential {other:?}"),
        }
        assert_eq!(
            import.accounts[2].credential,
            AccountCredential::Apikey {
                api_key: "sk-ant-api03-CCC".into()
            }
        );
    }

    #[test]
    fn expires_at_seconds_normalized_to_ms() {
        let dir = TempDir::new();
        let path = dir.path().join("teamclaude.json");
        std::fs::write(&path, TEAMCLAUDE_FIXTURE).expect("write fixture");

        let import = import_teamclaude_config(&path).expect("import");
        match &import.accounts[1].credential {
            AccountCredential::Oauth { expires_at_ms, .. } => {
                assert_eq!(*expires_at_ms, 1_750_000_000_000, "seconds -> ms");
            }
            other => panic!("unexpected credential {other:?}"),
        }
    }

    #[test]
    fn credentials_envelope_imports_single_oauth() {
        let dir = TempDir::new();
        let path = dir.path().join(".credentials.json");
        std::fs::write(
            &path,
            r#"{ "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-XYZ",
                "refreshToken": "sk-ant-ort01-XYZ",
                "expiresAt": 1750000000,
                "scopes": ["user:inference"]
            } }"#,
        )
        .expect("write");

        let accounts = import_claude_credentials(&path).expect("import");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "", "name left for the caller to default");
        match &accounts[0].credential {
            AccountCredential::Oauth {
                access_token,
                expires_at_ms,
                account_uuid,
                ..
            } => {
                assert_eq!(access_token, "sk-ant-oat01-XYZ");
                assert_eq!(*expires_at_ms, 1_750_000_000_000);
                assert_eq!(account_uuid, "", "uuid pending profile enrichment");
            }
            other => panic!("unexpected credential {other:?}"),
        }

        // import_file autodetects the same shape.
        let detected = import_file(&path).expect("detect");
        assert_eq!(detected, accounts);
    }

    #[test]
    fn inline_json_single_object() {
        let accounts = import_inline_json(
            r#"{ "accessToken": "sk-ant-oat01-A", "refreshToken": "sk-ant-ort01-A", "expiresAt": 1750000000000 }"#,
        )
        .expect("inline");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].credential.kind(), "oauth");
    }

    #[test]
    fn inline_json_array_mixed_naming() {
        let accounts = import_inline_json(
            r#"[
              { "name": "snake", "type": "oauth", "account_uuid": "u1",
                "access_token": "at", "refresh_token": "rt", "expires_at_ms": 1750000000000 },
              { "name": "camel", "type": "apikey", "apiKey": "sk-ant-api03-K" },
              { "claudeAiOauth": { "accessToken": "at2", "refreshToken": "rt2", "expiresAt": 1750000000 } }
            ]"#,
        )
        .expect("inline");
        assert_eq!(accounts.len(), 3);
        assert_eq!(accounts[0].credential.account_uuid(), Some("u1"));
        assert_eq!(accounts[1].credential.kind(), "apikey");
        assert_eq!(accounts[2].credential.kind(), "oauth");
    }

    #[test]
    fn inline_json_rejects_garbage() {
        assert!(import_inline_json("not json").is_err());
        assert!(import_inline_json("42").is_err());
        assert!(import_inline_json(r#"{ "name": "x" }"#).is_err());
        assert!(import_inline_json(r#"{ "type": "carrier-pigeon" }"#).is_err());
    }

    #[test]
    fn import_from_entry_resolves_credentials_file() {
        let dir = TempDir::new();
        let creds = dir.path().join("creds.json");
        std::fs::write(
            &creds,
            r#"{ "claudeAiOauth": { "accessToken": "at", "refreshToken": "rt", "expiresAt": 1750000000 } }"#,
        )
        .expect("write creds");

        let config_path = dir.path().join("teamclaude.json");
        std::fs::write(
            &config_path,
            format!(
                r#"{{ "accounts": [ {{ "name": "indirect", "type": "oauth", "importFrom": {} }} ] }}"#,
                serde_json::to_string(&creds.display().to_string()).expect("path json")
            ),
        )
        .expect("write config");

        let import = import_teamclaude_config(&config_path).expect("import");
        assert_eq!(import.skipped, Vec::<String>::new());
        assert_eq!(import.accounts.len(), 1);
        assert_eq!(import.accounts[0].name, "indirect");
        assert_eq!(import.accounts[0].credential.kind(), "oauth");
    }
}
