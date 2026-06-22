//! Config schema v1 for `~/.config/llmux.json` (see `.prd/02-architecture.md`).
//! These structs are the on-disk contract; they are complete and purely declarative.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::pricing::ModelPrice;

/// Default proxy listen port (teamclaude-compatible).
pub const DEFAULT_PORT: u16 = 3456;

/// Default ingress request-body admission cap: 64 MiB. A client request body
/// larger than this is rejected with 413 before it is buffered for forwarding,
/// bounding the heap one oversized request can pin (see
/// [`ProxyConfig::max_request_bytes`]).
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// Default upstream base URL.
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

/// Default OpenAI Codex backend base URL (the path `/responses` is appended
/// per request).
pub const DEFAULT_CODEX_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";

/// Default OpenAI OAuth token endpoint used to refresh Codex access tokens.
pub const DEFAULT_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Root of `~/.config/llmux.json`.
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
    /// API-equivalent pricing overrides (Feature D). Keyed by the *normalized*
    /// model slug (display suffixes like `[1m]` are stripped on lookup; match
    /// is case-insensitive). An entry here wins over the built-in default rate
    /// table in [`crate::pricing`]; absent/empty (the default) means "use the
    /// built-in rates". All rates are USD per 1,000,000 tokens. Additive: a
    /// config written before this field loads with an empty map.
    #[serde(default)]
    pub pricing: HashMap<String, ModelPrice>,
    /// Raw input/output payload capture (Feature B). When `enabled` (the
    /// default), the proxy appends one JSON line per request — the raw request
    /// and response bodies — to `$XDG_STATE_HOME/llmux/raw-io.jsonl`, pruned to
    /// `retention_days`. Best-effort: capture never affects the request path.
    /// See [`RawIoConfig`]. Additive: a config written before this field loads
    /// with the defaults (capture on, 90-day retention).
    #[serde(default)]
    pub raw_io: RawIoConfig,
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
            pricing: HashMap::new(),
            raw_io: RawIoConfig::default(),
            accounts: Vec::new(),
        }
    }
}

/// Raw input/output payload capture config (Feature B). The proxy keeps a
/// verbatim record of each proxied request's request body and the response
/// body delivered to the client, so traffic can be replayed/audited offline.
///
/// This is DISTINCT from activity persistence (`activity.jsonl`, per-request
/// metadata): this store holds the actual payload bytes. Capture is strictly
/// best-effort — it never blocks, mutates, or slows the bytes forwarded to the
/// client, and every IO/serialization error is swallowed (see
/// [`crate::proxy::raw_io`]). All fields are additive (`#[serde(default)]`), so
/// a config written before this section existed loads with capture ON and a
/// 90-day window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawIoConfig {
    /// Master switch. When `true` (the default), each request appends one
    /// [`crate::proxy::raw_io::RawIoRecord`] to the raw-io log. When `false`,
    /// nothing is captured or written.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Days of history to keep. On startup, records older than
    /// `now - retention_days * 86_400_000 ms` are pruned. `0` = keep forever
    /// (no pruning). Default `90`.
    #[serde(default = "default_raw_io_retention_days")]
    pub retention_days: u64,
    /// Per-body capture cap, in bytes, applied identically to the request body
    /// and the response body on BOTH the streaming and non-streaming paths. A
    /// body over this is clipped on a UTF-8 char boundary with a
    /// `…[truncated N bytes]` marker. This is the raw-io retention cap and is
    /// DELIBERATELY decoupled from the debug request-log's 8 KiB
    /// [`crate::proxy::logging::BODY_LOG_LIMIT`]: the debug log stays a short
    /// 8 KiB excerpt while raw-io retains the full (bounded) payload — most LLM
    /// responses stream tens to hundreds of KB, so an 8 KiB raw-io cap would
    /// lose almost the entire response. Default
    /// [`crate::proxy::raw_io::RESPONSE_CAP_BYTES`] (8 MiB), generous for a real
    /// request/response yet bounding the memory a pathological body can pin.
    #[serde(default = "default_raw_io_max_body_bytes")]
    pub max_body_bytes: usize,
}

impl Default for RawIoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: default_raw_io_retention_days(),
            max_body_bytes: default_raw_io_max_body_bytes(),
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
    /// Master switch. When `true` (now the default), an inbound request's
    /// `model` selects its backend group and the scheduler picks within it —
    /// claude models → claude accounts, codex models → codex accounts,
    /// independent of which account is "current". This is what makes
    /// `gpt-5.5` reach a codex account instead of being forwarded verbatim to
    /// Anthropic (which 404s "model not found"). When `false`, no group filter
    /// is applied and codex stays a cross-group overflow pool (the original
    /// behavior). Toggleable from the dashboard.
    #[serde(default = "default_true")]
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
            enabled: true,
            claude_models: Vec::new(),
            codex_models: Vec::new(),
            default_group: default_routing_group(),
            on_empty_group: default_on_empty_group(),
        }
    }
}

/// OpenAI Codex backend endpoints + request defaults. Endpoint defaults target
/// the ChatGPT backend the codex CLI itself uses; overridable for tests/staging.
/// The request-shaping fields (`default_model`, `fast`, `reasoning_effort`)
/// mirror what the codex CLI sets on its Responses requests and are settable
/// from the dashboard's codex group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexConfig {
    /// Base URL the Responses request is POSTed to (`{upstream}/responses`).
    #[serde(default = "default_codex_upstream")]
    pub upstream: String,
    /// OAuth token endpoint for Codex refresh-token grants.
    #[serde(default = "default_codex_token_url")]
    pub token_url: String,
    /// Model slug the codex provider requests upstream. Was a hardcoded
    /// `gpt-5.5` const; now config-driven so the dashboard can change it.
    /// Additive: configs written before this field load with the default.
    #[serde(default = "default_codex_model")]
    pub default_model: String,
    /// When set, llmux reports THIS model name to the client (Claude Code) in
    /// the response instead of the real codex model. Claude Code picks its
    /// context-window denominator by a hardcoded model-name lookup
    /// (unknown→200k, known 1M models→1,000,000) and offers no per-model
    /// window override, so set this to a 1M-window model name (e.g.
    /// `claude-opus-4-8`) to stop Claude Code cutting codex sessions off at
    /// ~200k. Pair with the `CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000` env var on
    /// the Claude Code side to make auto-compaction fire at codex's real limit.
    /// None (default) = report the real codex model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_model: Option<String>,
    /// "Fast" service tier. When `true`, the Responses request carries
    /// `service_tier: "priority"` — the exact wire value the codex CLI sends
    /// for its fast mode (config stores "fast", wire sends "priority"). When
    /// `false`, no `service_tier` field is sent. Default `false`.
    #[serde(default)]
    pub fast: bool,
    /// Reasoning effort for the Responses request: one of
    /// `none|minimal|low|medium|high|xhigh` (the codex CLI's `ReasoningEffort`
    /// wire values). `None` → omit `reasoning.effort` and let the backend use
    /// the model's default. Display + request only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Append a JSON-line trace of every codex request/response to
    /// `$XDG_STATE_HOME/llmux/codex-trace.jsonl` (input size breakdown +
    /// terminal outcome + verbatim upstream usage). Best-effort: write errors
    /// never affect the request. Default `true` while we diagnose token issues.
    #[serde(default = "default_true")]
    pub trace: bool,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            upstream: default_codex_upstream(),
            token_url: default_codex_token_url(),
            default_model: default_codex_model(),
            client_model: None,
            fast: false,
            reasoning_effort: None,
            trace: true,
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
    /// Forward-path idle (inactivity) timeout, seconds: how long the proxy
    /// waits for the NEXT byte from upstream after the connection is
    /// established before aborting the stream. This is an inactivity ceiling,
    /// NOT a total-request deadline — legitimate LLM streams run for minutes
    /// with long inter-token gaps, so the clock resets on every chunk. A
    /// silent upstream that connects and then stalls would otherwise hang the
    /// session and pin the account; this bounds the silence. Default 120.
    /// Applied two ways (defense in depth): `reqwest`'s `read_timeout` on the
    /// serving client and a per-chunk `tokio::time::timeout` around the SSE
    /// pump (see [`crate::proxy::sse::passthrough_body`]).
    #[serde(default = "default_forward_idle_timeout_secs")]
    pub forward_idle_timeout_secs: u64,
    /// Hard cap, in bytes, on a client request body buffered on the ingress
    /// forward path before it is relayed upstream. The body must be fully
    /// buffered (it can be replayed across account retries), so an unbounded
    /// read lets one oversized request pin arbitrary heap and OOM the daemon.
    /// A request whose body exceeds this returns 413 Payload Too Large.
    /// Default [`crate::config::DEFAULT_MAX_REQUEST_BYTES`] (64 MiB).
    ///
    /// This is the ingress admission limit and is DELIBERATELY distinct from
    /// [`RawIoConfig::max_body_bytes`] (the observability-tee retention cap):
    /// raw-io clips what is *retained* for inspection; this rejects what is
    /// *accepted* for forwarding. Additive (`#[serde(default)]`) so configs
    /// written before this field load with the 64 MiB default.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            api_key: None,
            forward_idle_timeout_secs: default_forward_idle_timeout_secs(),
            max_request_bytes: default_max_request_bytes(),
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

fn default_true() -> bool {
    true
}

fn default_codex_upstream() -> String {
    DEFAULT_CODEX_UPSTREAM.to_string()
}

fn default_codex_token_url() -> String {
    DEFAULT_CODEX_TOKEN_URL.to_string()
}

/// Default codex model slug (the value `CODEX_MODEL` used to hardcode).
fn default_codex_model() -> String {
    "gpt-5.5".to_string()
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// Default forward-path idle timeout: 120 seconds of upstream silence
/// (post-connect) before the stream is aborted. The connect phase is covered
/// separately by the client's 10s `connect_timeout`.
fn default_forward_idle_timeout_secs() -> u64 {
    120
/// Default ingress request-body admission cap (64 MiB). Kept in sync with
/// [`DEFAULT_MAX_REQUEST_BYTES`] so a config that omits the field caps exactly
/// where the const-defined backstop does.
fn default_max_request_bytes() -> usize {
    DEFAULT_MAX_REQUEST_BYTES
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

/// Default raw-io retention window: 90 days (per Feature B).
fn default_raw_io_retention_days() -> u64 {
    90
}

/// Default raw-io per-body capture cap: 8 MiB
/// ([`crate::proxy::raw_io::RESPONSE_CAP_BYTES`]). Kept in sync with that
/// constant so a config that omits the field caps exactly where the code's
/// backstop does.
fn default_raw_io_max_body_bytes() -> usize {
    crate::proxy::raw_io::RESPONSE_CAP_BYTES
}

fn default_routing_group() -> String {
    "claude".to_string()
}

fn default_on_empty_group() -> String {
    "error".to_string()
}
