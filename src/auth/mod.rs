//! Account authentication: PKCE OAuth login, token refresh (coalesced),
//! profile lookup, and `~/.claude/.credentials.json` import.

pub mod codex;
pub mod credentials;
pub mod oauth;
pub mod profile;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("oauth http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("token endpoint returned {status}: {body}")]
    TokenEndpoint {
        status: http::StatusCode,
        body: String,
    },
    #[error("auth response parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("oauth state mismatch (possible CSRF, aborting)")]
    StateMismatch,
    #[error("login flow aborted: {0}")]
    Aborted(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Refresh failed in a way a retry cannot fix (401 / `invalid_grant`):
    /// the refresh token is dead and the account needs a fresh login.
    #[error("token refresh permanently failed ({status}): {body}")]
    RefreshPermanent {
        status: http::StatusCode,
        body: String,
    },
    /// Transport-level failure (connect/reset/timeout) after retries.
    #[error("network error: {0}")]
    Network(String),
    #[error("profile endpoint returned {status}: {body}")]
    ProfileEndpoint {
        status: http::StatusCode,
        body: String,
    },
    /// Profile response was 200 but lacked a field llmux cannot work
    /// without (`account.uuid` is the dedup key; `account.email` the name).
    #[error("profile response missing {0}")]
    ProfileIncomplete(&'static str),
    /// `~/.codex/auth.json` (or the codex token response) lacked a field
    /// the codex provider cannot work without.
    #[error("codex auth data invalid: {0}")]
    CodexAuth(&'static str),
}
