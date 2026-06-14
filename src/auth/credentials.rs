//! Import of Claude Code's own credential store:
//! `~/.claude/.credentials.json` (`claudeAiOauth` envelope).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::oauth::normalize_expires_at_ms;
use super::AuthError;
use crate::config::AccountCredential;

/// Raw token envelope read from `~/.claude/.credentials.json`. Turning this
/// into an `AccountConfig` requires a profile fetch (for `account_uuid` and
/// the display name) — that composition lives in `cli::import`.
#[derive(Debug, Clone)]
pub struct ClaudeCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Epoch milliseconds, normalized on read (source may carry seconds).
    /// `0` when the file carries no expiry — treated as already expired, so
    /// the first use refreshes.
    pub expires_at_ms: u64,
    /// e.g. `max` / `pro`, when the file reports it.
    pub subscription_type: Option<String>,
}

impl ClaudeCredentials {
    /// Map into the crate's credential type. The `account_uuid` comes from a
    /// [`super::profile::fetch_profile`] call — the file does not carry it.
    /// The file's `subscriptionType` becomes the display `tier`.
    pub fn into_account_credential(self, account_uuid: String) -> AccountCredential {
        AccountCredential::Oauth {
            account_uuid,
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at_ms: self.expires_at_ms,
            tier: self.subscription_type,
            // The credentials file carries no refresh timestamp — shows
            // "never (refreshed)" until the proxy's first refresh.
            last_refresh_ms: None,
        }
    }
}

/// Default location: `~/.claude/.credentials.json`.
pub fn default_credentials_path() -> Result<PathBuf, AuthError> {
    dirs::home_dir()
        .map(|home| home.join(".claude").join(".credentials.json"))
        .ok_or_else(|| AuthError::Aborted("could not determine home directory".to_string()))
}

/// Read and parse the `claudeAiOauth` envelope from `path`.
pub fn read(path: &Path) -> Result<ClaudeCredentials, AuthError> {
    let raw = std::fs::read_to_string(path)?;
    parse(&raw)
}

/// Parse credentials from a JSON string: either the full file shape
/// (`{"claudeAiOauth": {...}}`) or the bare inner object (inline `--json`
/// input). teamclaude falls back to the bare shape the same way.
pub fn parse(json: &str) -> Result<ClaudeCredentials, AuthError> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let inner = value.get("claudeAiOauth").cloned().unwrap_or(value);
    let raw: RawCredentials = serde_json::from_value(inner)?;
    Ok(ClaudeCredentials {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        expires_at_ms: raw
            .expires_at
            .filter(|&at| at > 0)
            .map(normalize_expires_at_ms)
            .unwrap_or(0),
        subscription_type: raw.subscription_type,
    })
}

/// On-disk field names (camelCase, Claude Code's serialization).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCredentials {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    subscription_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_ai_oauth_envelope() {
        let json = r#"{
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-x",
                "refreshToken": "sk-ant-ort01-y",
                "expiresAt": 1750000000000,
                "subscriptionType": "max",
                "rateLimitTier": "default_claude_max_20x"
            }
        }"#;
        let creds = parse(json).unwrap();
        assert_eq!(creds.access_token, "sk-ant-oat01-x");
        assert_eq!(creds.refresh_token, "sk-ant-ort01-y");
        assert_eq!(creds.expires_at_ms, 1_750_000_000_000);
        assert_eq!(creds.subscription_type.as_deref(), Some("max"));
    }

    #[test]
    fn parse_bare_envelope_and_normalize_seconds() {
        // Inline JSON without the wrapper; expiry in SECONDS.
        let json = r#"{"accessToken": "at", "refreshToken": "rt", "expiresAt": 1750000000}"#;
        let creds = parse(json).unwrap();
        assert_eq!(creds.expires_at_ms, 1_750_000_000_000);
        assert_eq!(creds.subscription_type, None);
    }

    #[test]
    fn parse_missing_expiry_means_expired() {
        let json = r#"{"accessToken": "at", "refreshToken": "rt"}"#;
        assert_eq!(parse(json).unwrap().expires_at_ms, 0);
    }

    #[test]
    fn parse_missing_required_field_errors() {
        let json = r#"{"claudeAiOauth": {"accessToken": "at"}}"#;
        assert!(matches!(parse(json).unwrap_err(), AuthError::Parse(_)));
    }

    #[test]
    fn read_from_file() {
        let dir = std::env::temp_dir().join(format!("llmux-cred-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt","expiresAt":1}}"#,
        )
        .unwrap();
        let creds = read(&path).unwrap();
        assert_eq!(creds.access_token, "at");
        assert_eq!(creds.expires_at_ms, 1000); // 1s → 1000ms
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn maps_into_account_credential() {
        let creds = ClaudeCredentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at_ms: 42,
            subscription_type: Some("max".into()),
        };
        let credential = creds.into_account_credential("uuid-1".into());
        assert_eq!(
            credential,
            AccountCredential::Oauth {
                account_uuid: "uuid-1".into(),
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at_ms: 42,
                tier: Some("max".into()),
                last_refresh_ms: None,
            }
        );
    }
}
