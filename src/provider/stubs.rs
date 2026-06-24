//! Design stubs (spec §Non-goals): `gemini` and `local` exist so the
//! [`Provider`] trait is compile-checked against more than one shape, but
//! every conversion hook returns [`ProviderError::NotImplemented`]. This IS
//! their complete behavior — not a todo. (`openai-codex` graduated from stub
//! to a working provider in [`super::codex`].)

use super::{
    AnthropicRequest, AnthropicResponse, Provider, ProviderError, ProviderRequest,
    ProviderResponse, UnifiedRequest, UnifiedResponse,
};
use crate::config::AccountCredential;

macro_rules! stub_provider {
    ($(#[$doc:meta])* $ty:ident, $name:literal, $endpoint:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Default)]
        pub struct $ty;

        impl Provider for $ty {
            fn name(&self) -> &'static str {
                $name
            }

            fn endpoint(&self) -> &str {
                $endpoint
            }

            async fn auth(
                &self,
                _req: &mut ProviderRequest,
                _account: &AccountCredential,
            ) -> Result<(), ProviderError> {
                Err(ProviderError::NotImplemented { provider: $name })
            }

            fn request_out(
                &self,
                _anthropic_req: AnthropicRequest,
            ) -> Result<UnifiedRequest, ProviderError> {
                Err(ProviderError::NotImplemented { provider: $name })
            }

            fn request_in(
                &self,
                _unified: UnifiedRequest,
            ) -> Result<ProviderRequest, ProviderError> {
                Err(ProviderError::NotImplemented { provider: $name })
            }

            fn response_in(
                &self,
                _provider_resp: ProviderResponse,
            ) -> Result<UnifiedResponse, ProviderError> {
                Err(ProviderError::NotImplemented { provider: $name })
            }

            fn response_out(
                &self,
                _unified: UnifiedResponse,
            ) -> Result<AnthropicResponse, ProviderError> {
                Err(ProviderError::NotImplemented { provider: $name })
            }
        }
    };
}

stub_provider!(
    /// Google Gemini behind an Anthropic-shaped front. Draft.
    Gemini,
    "gemini",
    "https://generativelanguage.googleapis.com"
);

stub_provider!(
    /// Local model server (e.g. an OpenAI-compatible llama.cpp). Draft.
    Local,
    "local",
    "http://localhost:8080"
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountCredential;

    fn anthropic_request() -> AnthropicRequest {
        AnthropicRequest {
            method: http::Method::POST,
            path: "/v1/messages".to_string(),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::new(),
        }
    }

    fn provider_request() -> ProviderRequest {
        ProviderRequest {
            method: http::Method::POST,
            path: "/v1/messages".to_string(),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::new(),
        }
    }

    fn apikey() -> AccountCredential {
        AccountCredential::Apikey {
            api_key: "sk-ant-test".into(),
        }
    }

    fn assert_not_implemented(err: ProviderError, expected_provider: &str) {
        match err {
            ProviderError::NotImplemented { provider } => {
                assert_eq!(provider, expected_provider);
            }
            other => panic!(
                "expected NotImplemented {{ provider: {expected_provider:?} }}, got {other:?}"
            ),
        }
    }

    // PROV-16: Gemini is a design stub — auth and request_out return
    // NotImplemented carrying the correct provider name.
    #[tokio::test]
    async fn gemini_auth_and_request_out_are_not_implemented() {
        let p = Gemini;
        assert_eq!(p.name(), "gemini");
        let mut req = provider_request();
        let auth_err = p
            .auth(&mut req, &apikey())
            .await
            .expect_err("gemini auth is a stub");
        assert_not_implemented(auth_err, "gemini");
        let out_err = p
            .request_out(anthropic_request())
            .expect_err("gemini request_out is a stub");
        assert_not_implemented(out_err, "gemini");
    }

    // PROV-16: Local is a design stub — same contract as Gemini, distinct name.
    #[tokio::test]
    async fn local_auth_and_request_out_are_not_implemented() {
        let p = Local;
        assert_eq!(p.name(), "local");
        let mut req = provider_request();
        let auth_err = p
            .auth(&mut req, &apikey())
            .await
            .expect_err("local auth is a stub");
        assert_not_implemented(auth_err, "local");
        let out_err = p
            .request_out(anthropic_request())
            .expect_err("local request_out is a stub");
        assert_not_implemented(out_err, "local");
    }
}
