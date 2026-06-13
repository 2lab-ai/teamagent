//! Config schema v1 for `~/.config/teamagent.json` (see `.prd/02-architecture.md`).
//! These structs are the on-disk contract; they are complete and purely declarative.

use serde::{Deserialize, Serialize};

/// Default proxy listen port (teamclaude-compatible).
pub const DEFAULT_PORT: u16 = 3456;

/// Default upstream base URL.
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

/// Default OpenAI Codex backend base URL (the path `/responses` is appended
/// per request).
pub const DEFAULT_CODEX_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";

/// Default OpenAI OAuth token endpoint used to refresh Codex access tokens.
pub const DEFAULT_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Root of `~/.config/teamagent.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Schema version. Always `1` for now; bump on breaking layout changes.
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// Upstream base URL requests are forwarded to.
    #[serde(default = "default_upstream")]
    pub upstream: String,
    /// OpenAI Codex backend endpoints (used only by `type: "codex"` accounts).
    #[serde(default)]
    pub codex: CodexConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    /// Model→backend-group routing (default: disabled — exactly today's
    /// overflow behavior). See [`RoutingConfig`].
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: default_version(),
            proxy: ProxyConfig::default(),
            upstream: default_upstream(),
            codex: CodexConfig::default(),
            scheduler: SchedulerConfig::default(),
            routing: RoutingConfig::default(),
            accounts: Vec::new(),
        }
    }
}

/// Model→backend-group routing config. When `enabled` is false (the
/// default), routing is OFF and the scheduler behaves exactly as before:
/// no group filter anywhere, codex accounts stay the cross-group overflow
/// pool. When `enabled` is true, an inbound request's `model` selects a
/// backend group (claude vs codex) and the scheduler picks within that
/// group, sticky per group.
///
/// Empty `claude_models` / `codex_models` keep the builtin rules for that
/// group (see [`crate::routing::Classifier`]); a non-empty list replaces the
/// builtins for that group. All fields are additive (`#[serde(default)]`),
/// so a config written before routing existed loads with `enabled = false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Master switch. `false` (default) = today's behavior, no group filter.
    #[serde(default)]
    pub enabled: bool,
    /// Models routed to the claude group (empty → builtin claude rules).
    #[serde(default)]
    pub claude_models: Vec<String>,
    /// Models routed to the codex group (empty → builtin codex rules).
    #[serde(default)]
    pub codex_models: Vec<String>,
    /// Group an unmatched / model-less request routes to. Default `"claude"`.
    #[serde(default = "default_routing_group")]
    pub default_group: String,
    /// What to do when the matched group has no eligible/configured account:
    /// `"error"` (default) returns a clean 404 not_found_error; `"fallback"`
    /// falls back to the other group's normal selection.
    #[serde(default = "default_on_empty_group")]
    pub on_empty_group: String,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            claude_models: Vec::new(),
            codex_models: Vec::new(),
            default_group: default_routing_group(),
            on_empty_group: default_on_empty_group(),
        }
    }
}

/// OpenAI Codex backend endpoints. Defaults target the ChatGPT backend the
/// codex CLI itself uses; overridable for tests/staging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexConfig {
    /// Base URL the Responses request is POSTed to (`{upstream}/responses`).
    #[serde(default = "default_codex_upstream")]
    pub upstream: String,
    /// OAuth token endpoint for Codex refresh-token grants.
    #[serde(default = "default_codex_token_url")]
    pub token_url: String,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            upstream: default_codex_upstream(),
            token_url: default_codex_token_url(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Listen port. Default 3456.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Proxy-level API key (`ta-...`), auto-generated on first run.
    /// Localhost clients are exempt from presenting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            api_key: None,
        }
    }
}

/// Scheduler thresholds and polling cadence (FR3).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Max 5h-window utilization before an account becomes ineligible. Default 0.90.
    #[serde(default = "default_five_hour_max")]
    pub five_hour_max: f64,
    /// Max 7d-window utilization before an account becomes ineligible. Default 0.99.
    #[serde(default = "default_seven_day_max")]
    pub seven_day_max: f64,
    /// `/api/oauth/usage` poll interval per account, seconds. Default 300.
    #[serde(default = "default_usage_poll_secs")]
    pub usage_poll_secs: u64,
    /// Usage data older than this is stale; stale accounts are ineligible
    /// (unless ALL accounts are stale — headers-only fallback). Default 600.
    #[serde(default = "default_usage_max_age_secs")]
    pub usage_max_age_secs: u64,
    /// Background token refresh threshold: oauth tokens whose remaining
    /// lifetime drops below this many seconds are refreshed by the server's
    /// background task, independent of client traffic. Default 7h (access
    /// tokens live ~8h).
    #[serde(default = "default_refresh_ahead_secs")]
    pub refresh_ahead_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            five_hour_max: default_five_hour_max(),
            seven_day_max: default_seven_day_max(),
            usage_poll_secs: default_usage_poll_secs(),
            usage_max_age_secs: default_usage_max_age_secs(),
            refresh_ahead_secs: default_refresh_ahead_secs(),
        }
    }
}

/// One account entry. `name` is the user-facing identifier (unique within the
/// file); the credential variant carries the type-specific fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    #[serde(flatten)]
    pub credential: AccountCredential,
}

/// Credential payload, tagged by `"type": "oauth" | "apikey" | "codex"`
/// in JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AccountCredential {
    /// Claude subscription via PKCE OAuth.
    Oauth {
        /// `accountUuid` from `/api/oauth/profile`; dedup key across imports.
        /// Empty string = unknown (e.g. imported before any profile fetch).
        account_uuid: String,
        access_token: String,
        refresh_token: String,
        /// Access-token expiry, epoch milliseconds. Upstream may deliver
        /// seconds — normalize on ingest (`< 1e12` → ×1000).
        expires_at_ms: u64,
        /// Subscription tier when known (e.g. `max`); display only.
        /// Omitted from the file when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
        /// Epoch ms of the last successful token refresh (initial login
        /// counts as a refresh). `None` on configs written before this
        /// field existed — rendered as "never". Additive: absent in JSON
        /// until the first refresh after upgrade.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_refresh_ms: Option<u64>,
    },
    /// Plain Anthropic API key.
    Apikey { api_key: String },
    /// OpenAI Codex subscription (ChatGPT OAuth, imported from
    /// `~/.codex/auth.json`). Served by the codex provider, not Anthropic.
    Codex {
        /// `tokens.account_id` from `~/.codex/auth.json`; dedup key.
        account_id: String,
        access_token: String,
        refresh_token: String,
        /// Access-token expiry, epoch milliseconds (decoded from the JWT
        /// `exp` claim). `0` = unknown.
        expires_at_ms: u64,
        /// Epoch ms of the last successful token refresh; see the `Oauth`
        /// variant's field of the same name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_refresh_ms: Option<u64>,
    },
}

impl AccountCredential {
    /// Stable kind label for status output and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Oauth { .. } => "oauth",
            Self::Apikey { .. } => "apikey",
            Self::Codex { .. } => "codex",
        }
    }

    /// Epoch ms of the last successful token refresh. `None` for apikey
    /// accounts (nothing to refresh) and for oauth-style accounts that have
    /// not been refreshed since the field was introduced.
    pub fn last_refresh_ms(&self) -> Option<u64> {
        match self {
            Self::Oauth {
                last_refresh_ms, ..
            }
            | Self::Codex {
                last_refresh_ms, ..
            } => *last_refresh_ms,
            Self::Apikey { .. } => None,
        }
    }

    /// The dedup key for oauth-style accounts: a non-empty `account_uuid`
    /// (Anthropic) or `account_id` (Codex). `None` for apikey accounts and
    /// for accounts whose identity is not (yet) known.
    pub fn account_uuid(&self) -> Option<&str> {
        match self {
            Self::Oauth { account_uuid, .. } if !account_uuid.is_empty() => Some(account_uuid),
            Self::Codex { account_id, .. } if !account_id.is_empty() => Some(account_id),
            _ => None,
        }
    }
}

/// Result of [`Config::upsert_account`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Added,
    Updated,
}

impl Config {
    /// Find an account's index by identity: a non-empty oauth
    /// `account_uuid` wins, falling back to `name` (FR2 dedup order,
    /// mirroring teamclaude's `findConfigAccount`).
    pub fn find_account(&self, account: &AccountConfig) -> Option<usize> {
        if let Some(uuid) = account.credential.account_uuid() {
            if let Some(idx) = self
                .accounts
                .iter()
                .position(|a| a.credential.account_uuid() == Some(uuid))
            {
                return Some(idx);
            }
        }
        self.accounts.iter().position(|a| a.name == account.name)
    }

    /// Insert or replace an account, keyed by `account_uuid` then `name`.
    /// On a match the whole entry is replaced in place (a re-login may
    /// rename the account to its profile email).
    pub fn upsert_account(&mut self, account: AccountConfig) -> Upsert {
        match self.find_account(&account) {
            Some(idx) => {
                self.accounts[idx] = account;
                Upsert::Updated
            }
            None => {
                self.accounts.push(account);
                Upsert::Added
            }
        }
    }

    /// Remove an account by exact name. Returns `true` when one was removed.
    pub fn remove_account(&mut self, name: &str) -> bool {
        let before = self.accounts.len();
        self.accounts.retain(|a| a.name != name);
        self.accounts.len() != before
    }

    /// Persist refreshed oauth tokens onto the account identified by
    /// `ident` (matched against `account_uuid`/`account_id` first, then
    /// `name`). `refresh_token: None` preserves the stored refresh token
    /// (the token endpoint may omit a new one). `refreshed_at_ms` records
    /// WHEN the refresh happened (epoch ms) for the dashboard's
    /// "refreshed ago" display. Returns `false` when no oauth-style account
    /// matches. Covers Anthropic `Oauth` and `Codex` credentials alike —
    /// both rotate access/refresh tokens.
    pub fn update_oauth_tokens(
        &mut self,
        ident: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_at_ms: u64,
        refreshed_at_ms: u64,
    ) -> bool {
        let idx = self
            .accounts
            .iter()
            .position(|a| a.credential.account_uuid() == Some(ident))
            .or_else(|| self.accounts.iter().position(|a| a.name == ident));
        let Some(idx) = idx else {
            return false;
        };
        match &mut self.accounts[idx].credential {
            AccountCredential::Oauth {
                access_token: at,
                refresh_token: rt,
                expires_at_ms: exp,
                last_refresh_ms: lr,
                ..
            }
            | AccountCredential::Codex {
                access_token: at,
                refresh_token: rt,
                expires_at_ms: exp,
                last_refresh_ms: lr,
                ..
            } => {
                *at = access_token.to_string();
                if let Some(new_rt) = refresh_token {
                    *rt = new_rt.to_string();
                }
                *exp = expires_at_ms;
                *lr = Some(refreshed_at_ms);
                true
            }
            AccountCredential::Apikey { .. } => false,
        }
    }
}

fn default_version() -> u32 {
    1
}

fn default_codex_upstream() -> String {
    DEFAULT_CODEX_UPSTREAM.to_string()
}

fn default_codex_token_url() -> String {
    DEFAULT_CODEX_TOKEN_URL.to_string()
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_upstream() -> String {
    DEFAULT_UPSTREAM.to_string()
}

fn default_five_hour_max() -> f64 {
    0.90
}

fn default_seven_day_max() -> f64 {
    0.99
}

fn default_usage_poll_secs() -> u64 {
    300
}

fn default_usage_max_age_secs() -> u64 {
    600
}

fn default_refresh_ahead_secs() -> u64 {
    7 * 3600
}

fn default_routing_group() -> String {
    "claude".to_string()
}

fn default_on_empty_group() -> String {
    "error".to_string()
}
