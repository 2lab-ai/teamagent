//! `AnthropicPassthrough` — the v0.1 working provider. Conversion hooks are
//! byte-identity in the common case (the unified types wrap the Anthropic wire
//! shape, and `bytes::Bytes` clones are refcounted); only Claude-Code-local
//! model annotations are normalized before the request leaves the proxy.
//!
//! Where the hourglass would engage for a NON-passthrough provider:
//! `request_out` would parse the Anthropic body into real unified fields
//! (messages, tools, system), `request_in` would serialize them into the
//! provider's native shape, and `response_in`/`response_out` would convert
//! back — `forward.rs` already routes every request through these four hooks
//! plus `endpoint()`/`auth()`, so a future provider slots in without touching
//! the proxy core.

use http::header::AUTHORIZATION;
use http::{HeaderMap, HeaderValue};

use super::{
    AnthropicRequest, AnthropicResponse, Provider, ProviderError, ProviderRequest,
    ProviderResponse, UnifiedRequest, UnifiedResponse,
};
use crate::config::AccountCredential;

fn strip_client_context_suffix(body: bytes::Bytes) -> bytes::Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    let Some(model) = value.get("model").and_then(serde_json::Value::as_str) else {
        return body;
    };
    let Some(base) = model.strip_suffix("[1m]") else {
        return body;
    };
    value["model"] = serde_json::Value::String(base.to_string());
    match serde_json::to_vec(&value) {
        Ok(bytes) => bytes::Bytes::from(bytes),
        Err(_) => body,
    }
}

/// Client auth header stripped/replaced on the way upstream.
pub const X_API_KEY: &str = "x-api-key";

/// Strip client-supplied auth (`x-api-key` / `authorization`) and inject the
/// selected account's credential: `Authorization: Bearer <token>` for oauth,
/// `x-api-key: <key>` for apikey (FR1). A credential that cannot be encoded
/// as a header value is an auth error (never send the client's own auth
/// through by accident).
pub fn inject_credential(
    headers: &mut HeaderMap,
    credential: &AccountCredential,
) -> Result<(), ProviderError> {
    headers.remove(X_API_KEY);
    headers.remove(AUTHORIZATION);
    match credential {
        AccountCredential::Oauth { access_token, .. } => {
            let value = HeaderValue::from_str(&format!("Bearer {access_token}"))
                .map_err(|err| ProviderError::Auth(err.to_string()))?;
            headers.insert(AUTHORIZATION, value);
        }
        AccountCredential::Apikey { api_key } => {
            let value = HeaderValue::from_str(api_key)
                .map_err(|err| ProviderError::Auth(err.to_string()))?;
            headers.insert(X_API_KEY, value);
        }
        // A codex credential must never leak to the Anthropic upstream —
        // the proxy routes codex accounts through the codex provider before
        // this point; reaching here is a routing bug.
        AccountCredential::Codex { .. } => {
            return Err(ProviderError::Auth(
                "codex credential cannot authenticate against the anthropic provider".into(),
            ));
        }
    }
    Ok(())
}

/// Identity transformer for the real Anthropic API.
#[derive(Debug, Clone)]
pub struct AnthropicPassthrough {
    /// Upstream base URL (config `upstream`, default `https://api.anthropic.com`).
    base_url: String,
}

impl AnthropicPassthrough {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Provider for AnthropicPassthrough {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn endpoint(&self) -> &str {
        &self.base_url
    }

    /// Strip client-supplied `x-api-key` / `authorization`, inject the
    /// selected account's credential (Bearer for oauth, x-api-key for
    /// apikey).
    async fn auth(
        &self,
        req: &mut ProviderRequest,
        account: &AccountCredential,
    ) -> Result<(), ProviderError> {
        inject_credential(&mut req.headers, account)
    }

    /// Identity wrap. Extracts `model` and `stream` from the JSON body when
    /// present without touching the body bytes; a non-JSON body simply yields
    /// no flags — passthrough never fails on body shape. `model` is now the
    /// live backend-group routing key (see `routing.rs`); the extraction
    /// itself is shared via `routing::model_from_body`.
    fn request_out(
        &self,
        anthropic_req: AnthropicRequest,
    ) -> Result<UnifiedRequest, ProviderError> {
        // `model` extraction is shared with the proxy's routing path (one
        // source of truth: `routing::model_from_body`); `stream` stays local.
        let model = crate::routing::model_from_body(&anthropic_req.body);
        let stream = serde_json::from_slice::<serde_json::Value>(&anthropic_req.body)
            .ok()
            .and_then(|value| value.get("stream").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);
        Ok(UnifiedRequest {
            model,
            stream,
            wire: anthropic_req,
        })
    }

    /// Normalize the Claude-Code-only context-window suffix, otherwise unwrap
    /// without reserializing (moves the original wire body out).
    fn request_in(&self, unified: UnifiedRequest) -> Result<ProviderRequest, ProviderError> {
        let wire = unified.wire;
        Ok(ProviderRequest {
            method: wire.method,
            path: wire.path,
            headers: wire.headers,
            body: strip_client_context_suffix(wire.body),
        })
    }

    /// Identity wrap.
    fn response_in(
        &self,
        provider_resp: ProviderResponse,
    ) -> Result<UnifiedResponse, ProviderError> {
        Ok(UnifiedResponse {
            wire: AnthropicResponse {
                status: provider_resp.status,
                headers: provider_resp.headers,
                body: provider_resp.body,
            },
        })
    }

    /// Identity unwrap.
    fn response_out(&self, unified: UnifiedResponse) -> Result<AnthropicResponse, ProviderError> {
        Ok(unified.wire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    fn provider() -> AnthropicPassthrough {
        AnthropicPassthrough::new("https://api.anthropic.com")
    }

    fn request(body: &str) -> AnthropicRequest {
        AnthropicRequest {
            method: Method::POST,
            path: "/v1/messages".to_string(),
            headers: HeaderMap::new(),
            body: bytes::Bytes::copy_from_slice(body.as_bytes()),
        }
    }

    #[test]
    fn request_out_extracts_model_and_stream() {
        let unified = provider()
            .request_out(request(
                r#"{"model":"claude-sonnet-4-5","stream":true,"messages":[]}"#,
            ))
            .expect("unified");
        assert_eq!(unified.model.as_deref(), Some("claude-sonnet-4-5"));
        assert!(unified.stream);
    }

    #[test]
    fn request_out_tolerates_non_json_bodies() {
        let unified = provider()
            .request_out(request("not json"))
            .expect("unified");
        assert_eq!(unified.model, None);
        assert!(!unified.stream);
    }

    #[test]
    fn round_trip_is_byte_identical() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let p = provider();
        let unified = p.request_out(request(body)).expect("out");
        let provider_req = p.request_in(unified).expect("in");
        assert_eq!(provider_req.body.as_ref(), body.as_bytes());
        assert_eq!(provider_req.path, "/v1/messages");
        assert_eq!(provider_req.method, Method::POST);
    }

    #[test]
    fn request_in_strips_client_context_suffix_from_claude_model() {
        let body = r#"{"model":"claude-opus-4-8[1m]","messages":[{"role":"user","content":"hi"}]}"#;
        let p = provider();
        let unified = p.request_out(request(body)).expect("out");
        let provider_req = p.request_in(unified).expect("in");
        let upstream: serde_json::Value =
            serde_json::from_slice(&provider_req.body).expect("upstream json");
        assert_eq!(upstream["model"], "claude-opus-4-8");
    }

    #[tokio::test]
    async fn auth_replaces_client_credentials_with_oauth_bearer() {
        let p = provider();
        let unified = p.request_out(request("{}")).expect("out");
        let mut req = p.request_in(unified).expect("in");
        req.headers
            .insert(X_API_KEY, HeaderValue::from_static("client-key"));
        req.headers
            .insert(AUTHORIZATION, HeaderValue::from_static("Bearer client"));
        p.auth(
            &mut req,
            &AccountCredential::Oauth {
                account_uuid: "u".into(),
                access_token: "at-1".into(),
                refresh_token: "rt-1".into(),
                expires_at_ms: 0,
                tier: None,
                last_refresh_ms: None,
            },
        )
        .await
        .expect("auth");
        assert_eq!(req.headers.get(AUTHORIZATION).unwrap(), "Bearer at-1");
        assert!(req.headers.get(X_API_KEY).is_none());
    }

    #[tokio::test]
    async fn auth_injects_api_key_for_apikey_accounts() {
        let p = provider();
        let unified = p.request_out(request("{}")).expect("out");
        let mut req = p.request_in(unified).expect("in");
        req.headers
            .insert(AUTHORIZATION, HeaderValue::from_static("Bearer client"));
        p.auth(
            &mut req,
            &AccountCredential::Apikey {
                api_key: "sk-ant-api03-k".into(),
            },
        )
        .await
        .expect("auth");
        assert_eq!(req.headers.get(X_API_KEY).unwrap(), "sk-ant-api03-k");
        assert!(req.headers.get(AUTHORIZATION).is_none());
    }
}
