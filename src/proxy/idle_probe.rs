//! Production [`Prober`] for the on-demand idle-account usage probe (issue
//! #21). Builds a `max_tokens = 1` request for the account's provider —
//! reusing the SAME credential-injection / request-build hooks the forward
//! path uses — sends it with the shared `reqwest::Client`, and hands the
//! response headers back to the scheduler's [`IdleProber`] orchestrator, which
//! parses the `anthropic-ratelimit-*` set into the 5h/7d windows
//! (`WindowSource::Headers`).
//!
//! Codex path (issue #21 §Codex): a Codex account's `count_tokens` is answered
//! locally with no upstream call, so it emits no ratelimit headers — the real
//! `max_tokens = 1` `/responses` ping is therefore used for Codex too (built
//! via [`crate::provider::codex::CodexProvider::build_request`]). That is the
//! only path that actually moves/returns Codex's 5h/7d windows.

use std::sync::Arc;

use http::{HeaderMap, Method};

use crate::config::AccountCredential;
use crate::provider::codex::CodexProvider;
use crate::provider::{anthropic, ProviderRequest};
use crate::scheduler::idle_probe::{ProbeError, Prober};

/// The minimal `POST /v1/messages` probe body for an Anthropic (oauth/apikey)
/// account: a one-character user turn capped at a single output token. The
/// model is the smallest current Claude model — a real request that returns a
/// 200 with the `anthropic-ratelimit-*` headers while spending the least
/// possible quota.
fn anthropic_probe_body(model: &str) -> bytes::Bytes {
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "." }],
    });
    bytes::Bytes::from(body.to_string())
}

/// Default model used for the Anthropic probe. Kept tiny on purpose; any valid
/// model returns the unified rate-limit headers (the headers are
/// account-scoped, not model-scoped).
const ANTHROPIC_PROBE_MODEL: &str = "claude-3-5-haiku-20241022";

/// Production prober backed by `reqwest`, reusing the proxy's provider hooks.
#[derive(Clone)]
pub struct ReqwestProber {
    client: reqwest::Client,
    /// Anthropic upstream base URL (config `upstream`).
    anthropic_base: String,
    /// Codex provider (holds the per-process session id + endpoint + shape),
    /// reused so the Codex probe is byte-shaped exactly like a real request.
    codex: Arc<CodexProvider>,
    /// Codex model slug to request on the probe (config `codex.default_model`).
    codex_model: String,
}

impl ReqwestProber {
    pub fn new(
        client: reqwest::Client,
        anthropic_base: String,
        codex: Arc<CodexProvider>,
        codex_model: String,
    ) -> Self {
        Self {
            client,
            anthropic_base,
            codex,
            codex_model,
        }
    }

    /// Build the upstream `(request, base_url)` for one probe, per credential
    /// kind. Codex → `/responses` via the codex provider; oauth/apikey →
    /// `/v1/messages` with the credential injected exactly as the forward path
    /// does.
    fn build(
        &self,
        credential: &AccountCredential,
    ) -> Result<(ProviderRequest, String), ProbeError> {
        match credential {
            AccountCredential::Codex { .. } => {
                let body = anthropic_probe_body(&self.codex_model);
                let (req, _client_stream) = self
                    .codex
                    .build_request(&body, credential)
                    .map_err(|err| ProbeError::Build(err.to_string()))?;
                Ok((req, self.codex.endpoint().to_string()))
            }
            AccountCredential::Oauth { .. } | AccountCredential::Apikey { .. } => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                headers.insert(
                    "anthropic-version",
                    http::HeaderValue::from_static("2023-06-01"),
                );
                anthropic::inject_credential(&mut headers, credential)
                    .map_err(|err| ProbeError::Build(err.to_string()))?;
                let req = ProviderRequest {
                    method: Method::POST,
                    path: "/v1/messages".to_string(),
                    headers,
                    body: anthropic_probe_body(ANTHROPIC_PROBE_MODEL),
                };
                Ok((req, self.anthropic_base.clone()))
            }
        }
    }
}

impl Prober for ReqwestProber {
    fn probe(
        &self,
        credential: &AccountCredential,
    ) -> impl std::future::Future<Output = Result<HeaderMap, ProbeError>> + Send {
        // Build synchronously (no IO) so the async block borrows nothing.
        let built = self.build(credential);
        let client = self.client.clone();
        async move {
            let (req, base) = built?;
            let url = format!("{}{}", base.trim_end_matches('/'), req.path);
            let response = client
                .request(req.method, url)
                .headers(req.headers)
                .body(req.body)
                .send()
                .await?;
            // The rate-limit headers ride on EVERY response (200, 429, even
            // some errors), so we read them regardless of status — a 429 still
            // tells us the account's current 5h/7d utilization.
            Ok(response.headers().clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_provider() -> Arc<CodexProvider> {
        Arc::new(CodexProvider::new("http://codex.invalid"))
    }

    fn prober() -> ReqwestProber {
        ReqwestProber::new(
            reqwest::Client::new(),
            "http://anthropic.invalid".to_string(),
            codex_provider(),
            "gpt-5.5".to_string(),
        )
    }

    #[test]
    fn anthropic_probe_body_is_minimal() {
        let bytes = anthropic_probe_body(ANTHROPIC_PROBE_MODEL);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["max_tokens"], 1, "single output token");
        assert_eq!(value["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn builds_anthropic_request_with_injected_oauth_bearer() {
        let credential = AccountCredential::Oauth {
            account_uuid: "uuid-a".into(),
            access_token: "at-a".into(),
            refresh_token: "rt-a".into(),
            expires_at_ms: 0,
            tier: None,
            last_refresh_ms: None,
        };
        let (req, base) = prober().build(&credential).unwrap();
        assert_eq!(req.path, "/v1/messages");
        assert_eq!(base, "http://anthropic.invalid");
        assert_eq!(
            req.headers
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer at-a"),
            "oauth credential injected as Bearer"
        );
        assert!(
            req.headers.get(anthropic::X_API_KEY).is_none(),
            "no x-api-key leaked for oauth"
        );
    }

    #[test]
    fn builds_anthropic_request_with_injected_api_key() {
        let credential = AccountCredential::Apikey {
            api_key: "sk-ant-test".into(),
        };
        let (req, _) = prober().build(&credential).unwrap();
        assert_eq!(
            req.headers
                .get(anthropic::X_API_KEY)
                .and_then(|v| v.to_str().ok()),
            Some("sk-ant-test"),
            "apikey credential injected as x-api-key"
        );
    }

    #[test]
    fn builds_codex_request_via_codex_provider() {
        let credential = AccountCredential::Codex {
            account_id: "acct-a".into(),
            access_token: "at-codex".into(),
            refresh_token: "rt-codex".into(),
            expires_at_ms: 0,
            last_refresh_ms: None,
        };
        let (req, base) = prober().build(&credential).unwrap();
        assert_eq!(base, "http://codex.invalid", "codex endpoint");
        assert_eq!(
            req.headers
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer at-codex"),
        );
        assert_eq!(
            req.headers
                .get("chatgpt-account-id")
                .and_then(|v| v.to_str().ok()),
            Some("acct-a"),
        );
    }
}
