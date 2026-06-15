//! The proxy core (FR1): axum listener, request rewrite + upstream forward,
//! SSE passthrough, optional request logging.

pub mod codex_trace;
pub mod forward;
pub mod logging;
pub mod server;
pub mod sse;

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("failed to bind port {port}: {source}")]
    Bind {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("upstream error: {0}")]
    Upstream(#[from] reqwest::Error),
    #[error("server io error: {0}")]
    Io(#[from] std::io::Error),
}
