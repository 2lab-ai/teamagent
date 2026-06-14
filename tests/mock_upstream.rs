//! Configurable Anthropic upstream simulator for the e2e suite: emits
//! unified rate-limit headers, scripted 429s, fragmented SSE bodies, an
//! OAuth token endpoint (counting hits, for refresh-coalescing assertions)
//! and `/api/oauth/usage` (spec §Acceptance). Included by `e2e.rs` via
//! `#[path]`; cargo also compiles it as its own (empty) test crate, hence
//! the allow.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;

/// `(utilization, resets_in_secs)` — rendered as a unified rate-limit
/// header pair (reset = absolute epoch seconds computed at response time).
pub type WindowSpec = (f64, u64);

/// One scripted response for the catch-all route (consumed in order; after
/// the script is exhausted the upstream answers [`MockUpstream::DEFAULT_OK`]
/// with mild utilization headers).
#[derive(Debug, Clone)]
pub enum ScriptedResponse {
    /// 200 JSON with these unified window readings.
    Ok {
        body: String,
        five_hour: Option<WindowSpec>,
        seven_day: Option<WindowSpec>,
    },
    /// 200 SSE, streamed in `chunk_size`-byte fragments with `chunk_delay`
    /// between them (forces event fragmentation across chunks). When
    /// `content_type` is false NO `content-type` header is sent — the real
    /// chatgpt.com codex backend's streaming-200 shape (live capture
    /// 2026-06-12: only date/server/x-codex-*/x-content-type-options).
    /// `extra_headers` lets tests attach e.g. `x-codex-*` quota headers.
    Sse {
        body: String,
        chunk_size: usize,
        chunk_delay: Duration,
        five_hour: Option<WindowSpec>,
        seven_day: Option<WindowSpec>,
        content_type: bool,
        extra_headers: Vec<(String, String)>,
    },
    /// 429 with this `retry-after` (seconds), when `Some`.
    RateLimited { retry_after: Option<u64> },
    /// 401 (expired/invalid token).
    AuthRejected,
}

impl ScriptedResponse {
    /// Plain 200 with default (mild) utilization headers.
    pub fn ok(body: &str) -> Self {
        Self::Ok {
            body: body.to_string(),
            five_hour: Some((0.10, 3_600)),
            seven_day: Some((0.10, 86_400)),
        }
    }

    /// 200 with explicit unified window readings.
    pub fn ok_with(body: &str, five_hour: WindowSpec, seven_day: WindowSpec) -> Self {
        Self::Ok {
            body: body.to_string(),
            five_hour: Some(five_hour),
            seven_day: Some(seven_day),
        }
    }

    /// SSE WITHOUT anthropic rate-limit headers, chunked awkwardly — what a
    /// non-Anthropic upstream (the codex backend) actually serves.
    pub fn sse_plain(body: &str, chunk_size: usize) -> Self {
        Self::Sse {
            body: body.to_string(),
            chunk_size,
            chunk_delay: Duration::from_millis(2),
            five_hour: None,
            seven_day: None,
            content_type: true,
            extra_headers: Vec::new(),
        }
    }

    /// The real codex streaming-200 shape: NO `content-type` header at all,
    /// `x-codex-*` quota headers attached, chunked awkwardly.
    pub fn sse_codex(body: &str, chunk_size: usize, extra_headers: &[(&str, &str)]) -> Self {
        Self::Sse {
            body: body.to_string(),
            chunk_size,
            chunk_delay: Duration::from_millis(2),
            five_hour: None,
            seven_day: None,
            content_type: false,
            extra_headers: extra_headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}

/// One request as the mock saw it — the assertion surface for "which
/// account did the proxy pick" and "was client auth stripped".
#[derive(Debug, Clone)]
pub struct SeenRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub x_api_key: Option<String>,
    /// Codex header assertions (`Chatgpt-Account-Id` / `originator`).
    pub chatgpt_account_id: Option<String>,
    pub originator: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    script: Mutex<VecDeque<ScriptedResponse>>,
    seen: Mutex<Vec<SeenRequest>>,
    /// `access_token` → raw JSON body for `GET /api/oauth/usage`.
    usage: Mutex<HashMap<String, String>>,
    token_hits: AtomicUsize,
    token_delay: Mutex<Duration>,
}

/// In-process Anthropic simulator bound to a random localhost port.
pub struct MockUpstream {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
}

impl MockUpstream {
    /// Body served once the script queue is empty.
    pub const DEFAULT_OK: &'static str = r#"{"id":"msg_default","type":"message"}"#;

    /// Access token handed out by the mock token endpoint.
    pub const REFRESHED_ACCESS_TOKEN: &'static str = "at-refreshed";
    /// Refresh token handed out by the mock token endpoint.
    pub const REFRESHED_REFRESH_TOKEN: &'static str = "rt-refreshed";

    /// Bind on 127.0.0.1:0 and start serving.
    pub async fn spawn() -> Self {
        let shared = Arc::new(Shared::default());
        let app = Router::new()
            .route("/v1/oauth/token", post(token_endpoint))
            .route("/api/oauth/usage", get(usage_endpoint))
            .fallback(catch_all)
            .with_state(Arc::clone(&shared));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock upstream");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, shared }
    }

    /// Base URL to point the proxy's `upstream` at.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Queue the next scripted response (consumed in request order).
    pub fn push(&self, response: ScriptedResponse) {
        self.shared
            .script
            .lock()
            .expect("script lock")
            .push_back(response);
    }

    /// Script `/api/oauth/usage` for the account whose bearer is
    /// `access_token`. Windows are `(utilization, resets_in_secs)` where
    /// `utilization` is a 0..=1 FRACTION for test ergonomics; it is emitted on
    /// the wire as a PERCENTAGE (×100) to match the live endpoint, which the
    /// parser divides back by 100.
    pub fn set_usage(&self, access_token: &str, five_hour: WindowSpec, seven_day: WindowSpec) {
        let body = format!(
            r#"{{"five_hour":{{"utilization":{},"resets_at":{}}},"seven_day":{{"utilization":{},"resets_at":{}}}}}"#,
            five_hour.0 * 100.0,
            epoch_in(five_hour.1),
            seven_day.0 * 100.0,
            epoch_in(seven_day.1),
        );
        self.shared
            .usage
            .lock()
            .expect("usage lock")
            .insert(access_token.to_string(), body);
    }

    /// Delay applied inside the token endpoint (widens the refresh-coalesce
    /// race window for the concurrency test).
    pub fn set_token_delay(&self, delay: Duration) {
        *self.shared.token_delay.lock().expect("delay lock") = delay;
    }

    /// Requests received by the catch-all route so far, in order.
    pub fn seen(&self) -> Vec<SeenRequest> {
        self.shared.seen.lock().expect("seen lock").clone()
    }

    /// `authorization` header values seen so far, in request order — the
    /// assertion surface for "which account served which request".
    pub fn seen_bearers(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter_map(|s| s.authorization)
            .collect()
    }

    /// How many times the token endpoint was hit (refresh coalescing).
    pub fn token_hits(&self) -> usize {
        self.shared.token_hits.load(Ordering::SeqCst)
    }
}

fn epoch_in(secs: u64) -> u64 {
    (SystemTime::now() + Duration::from_secs(secs))
        .duration_since(UNIX_EPOCH)
        .expect("future timestamp")
        .as_secs()
}

fn unified_headers(
    builder: http::response::Builder,
    five_hour: Option<WindowSpec>,
    seven_day: Option<WindowSpec>,
) -> http::response::Builder {
    let mut builder = builder;
    if let Some((utilization, resets_in)) = five_hour {
        builder = builder
            .header(
                "anthropic-ratelimit-unified-5h-utilization",
                utilization.to_string(),
            )
            .header(
                "anthropic-ratelimit-unified-5h-reset",
                epoch_in(resets_in).to_string(),
            );
    }
    if let Some((utilization, resets_in)) = seven_day {
        builder = builder
            .header(
                "anthropic-ratelimit-unified-7d-utilization",
                utilization.to_string(),
            )
            .header(
                "anthropic-ratelimit-unified-7d-reset",
                epoch_in(resets_in).to_string(),
            );
    }
    builder
}

async fn catch_all(
    axum::extract::State(shared): axum::extract::State<Arc<Shared>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let (parts, body) = req.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    shared.seen.lock().expect("seen lock").push(SeenRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        authorization: header("authorization"),
        x_api_key: header("x-api-key"),
        chatgpt_account_id: header("chatgpt-account-id"),
        originator: header("originator"),
        body: body.to_vec(),
    });

    let next = shared
        .script
        .lock()
        .expect("script lock")
        .pop_front()
        .unwrap_or_else(|| ScriptedResponse::ok(MockUpstream::DEFAULT_OK));

    match next {
        ScriptedResponse::Ok {
            body,
            five_hour,
            seven_day,
        } => {
            let builder = http::Response::builder()
                .status(200)
                .header("content-type", "application/json");
            unified_headers(builder, five_hour, seven_day)
                .body(axum::body::Body::from(body))
                .expect("ok response")
        }
        ScriptedResponse::Sse {
            body,
            chunk_size,
            chunk_delay,
            five_hour,
            seven_day,
            content_type,
            extra_headers,
        } => {
            // Stream the body in deliberately awkward fragments so SSE
            // events split across chunks (and across the `\n\n` terminator).
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
            tokio::spawn(async move {
                let bytes = body.into_bytes();
                for chunk in bytes.chunks(chunk_size.max(1)) {
                    if tx.send(Ok(Bytes::copy_from_slice(chunk))).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(chunk_delay).await;
                }
            });
            let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
            let mut builder = http::Response::builder().status(200);
            if content_type {
                builder = builder.header("content-type", "text/event-stream");
            }
            for (name, value) in &extra_headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            unified_headers(builder, five_hour, seven_day)
                .body(axum::body::Body::from_stream(stream))
                .expect("sse response")
        }
        ScriptedResponse::RateLimited { retry_after } => {
            let mut builder = http::Response::builder()
                .status(429)
                .header("content-type", "application/json");
            if let Some(secs) = retry_after {
                builder = builder.header("retry-after", secs.to_string());
            }
            builder
                .body(axum::body::Body::from(
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"overloaded"}}"#,
                ))
                .expect("429 response")
        }
        ScriptedResponse::AuthRejected => http::Response::builder()
            .status(401)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#,
            ))
            .expect("401 response"),
    }
}

async fn token_endpoint(
    axum::extract::State(shared): axum::extract::State<Arc<Shared>>,
) -> axum::response::Response {
    shared.token_hits.fetch_add(1, Ordering::SeqCst);
    let delay = *shared.token_delay.lock().expect("delay lock");
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    http::Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(format!(
            r#"{{"access_token":"{}","refresh_token":"{}","expires_in":3600}}"#,
            MockUpstream::REFRESHED_ACCESS_TOKEN,
            MockUpstream::REFRESHED_REFRESH_TOKEN,
        )))
        .expect("token response")
}

async fn usage_endpoint(
    axum::extract::State(shared): axum::extract::State<Arc<Shared>>,
    headers: http::HeaderMap,
) -> axum::response::Response {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    let body = shared
        .usage
        .lock()
        .expect("usage lock")
        .get(&bearer)
        .cloned();
    match body {
        Some(body) => http::Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .expect("usage response"),
        None => http::Response::builder()
            .status(404)
            .body(axum::body::Body::from(r#"{"error":"no usage scripted"}"#))
            .expect("usage 404"),
    }
}
