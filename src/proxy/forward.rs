//! Request rewrite + upstream call + the error taxonomy
//! (`.prd/02-architecture.md` §Error taxonomy): 429 park/retry, 401
//! refresh-once, transient vs persistent failures.
//!
//! Deviations from the Node reference (`teamclaude src/server.js`), forced by
//! axum/reqwest reality — documented per the task contract:
//!
//! - **Transient errors** (connect refused/reset/timeout, upstream 5xx per
//!   the architecture table): Node destroys the client socket so the client
//!   retries. An axum handler cannot destroy the TCP socket; the closest
//!   faithful behavior is `502` + `Connection: close` — hyper closes the
//!   connection after the response and Claude Code's retry logic fires on
//!   the 5xx. (Node relays upstream 5xx bodies; the architecture table
//!   classifies 5xx as transient — the table wins here.)
//! - **429 handling** is split, unlike Node's wait-always: `retry-after ≤ 5s`
//!   waits and retries the SAME account (bounded); longer parks the account
//!   via `record_429` and retries on the next eligible account (Node has no
//!   scheduler to switch to; we do).
//! - `forward` owns lease acquisition (the retry loop needs to re-lease
//!   after a switch), so it takes the whole request instead of a pre-made
//!   lease.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::response::Response;
use bytes::Bytes;
use http::{header, HeaderMap, HeaderValue, Method, StatusCode};

use super::logging::BODY_LOG_LIMIT;
use super::server::AppState;
use super::sse::{self, SseTransform as _};
use crate::config::AccountCredential;
use crate::provider::{anthropic, AnthropicRequest, Provider as _, ProviderRequest};
use crate::routing::BackendGroup;
use crate::scheduler::select::{self, Decision};
use crate::scheduler::{headers as rl_headers, AccountId};
use crate::tui::{ActivityEvent, TokenCounts};

/// Hop-by-hop headers stripped from the client request before forwarding
/// (FR1; the set teamclaude strips).
pub const HOP_BY_HOP_HEADERS: [&str; 9] = [
    "host",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];

/// 429s with `retry-after` at or under this wait on the SAME account.
const SAME_ACCOUNT_WAIT_MAX: Duration = Duration::from_secs(5);

/// Bound on same-account 429 waits per request.
const MAX_SAME_ACCOUNT_WAITS: u32 = 2;

/// OAuth tokens expiring within this window are refreshed before use.
const REFRESH_AHEAD_MS: u64 = 5 * 60 * 1000;

/// `retry-after` surfaced to the client when the pool is exhausted and no
/// reset is known (Node default).
const DEFAULT_CLIENT_RETRY_AFTER_SECS: u64 = 60;

/// Classification of an upstream response/failure, driving the retry
/// decision table in the architecture doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamSignal {
    /// Success (or any status that should be relayed as-is).
    Relay,
    /// 429: park the account for `retry_after` (exact when present). Retry
    /// the same request on the next eligible account if parked > 5s, else
    /// wait it out on the same account.
    RateLimited { retry_after: Option<Duration> },
    /// 401 on an oauth account: force one refresh and retry; a second 401
    /// marks the account `AuthFailed` and switches.
    AuthRejected,
    /// 5xx / connect reset / timeout: transient — close the client
    /// connection (502 + `Connection: close`) and let the client retry.
    Transient,
    /// Anything else persistent: mark the account errored, switch, retry
    /// (bounded).
    Persistent,
}

/// Classify an upstream response status (+ headers, for `retry-after`).
pub fn classify(status: StatusCode, headers: &HeaderMap) -> UpstreamSignal {
    if status == StatusCode::TOO_MANY_REQUESTS {
        UpstreamSignal::RateLimited {
            retry_after: parse_retry_after(headers),
        }
    } else if status == StatusCode::UNAUTHORIZED {
        UpstreamSignal::AuthRejected
    } else if status.is_server_error() {
        UpstreamSignal::Transient
    } else {
        UpstreamSignal::Relay
    }
}

/// Classify a reqwest send failure: connect refused / reset / timeout are
/// transient (close the client connection, let it retry); everything else
/// is persistent (mark account, switch, bounded retry).
pub fn classify_send_error(err: &reqwest::Error) -> UpstreamSignal {
    if err.is_connect() || err.is_timeout() {
        return UpstreamSignal::Transient;
    }
    // Connection resets surface as io errors buried in the source chain.
    let mut source = std::error::Error::source(err);
    while let Some(inner) = source {
        if let Some(io) = inner.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
            ) {
                return UpstreamSignal::Transient;
            }
        }
        source = std::error::Error::source(inner);
    }
    UpstreamSignal::Persistent
}

/// Strip hop-by-hop headers, `accept-encoding` (avoid decompression
/// mismatch) and `content-length` (recomputed by reqwest from the buffered
/// body) from an outgoing request.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
    headers.remove(header::ACCEPT_ENCODING);
    headers.remove(header::CONTENT_LENGTH);
}

/// Rewrite client headers for upstream: strip client `x-api-key` /
/// `authorization`, strip hop-by-hop headers, drop `accept-encoding`
/// (avoid decompression mismatch), inject the leased credential.
///
/// This is the proxy-generic strip composed with the provider-specific
/// credential injection — the production path runs the same two steps via
/// the `Provider` trait (`strip_hop_by_hop` + `Provider::auth`).
pub fn rewrite_headers(headers: &mut HeaderMap, credential: &AccountCredential) {
    strip_hop_by_hop(headers);
    if let Err(err) = anthropic::inject_credential(headers, credential) {
        tracing::warn!(error = %err, "credential injection failed; request goes out unauthenticated");
    }
}

/// Parse a `retry-after` header (delta-seconds form). The HTTP-date form is
/// not parsed — Anthropic sends seconds; an unparseable value falls back to
/// the heuristic cooldown via `None`.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    let secs: u64 = value.parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Per-request context threaded through the retry loop: the buffered
/// original request (headers/body are reused on every retry) plus the
/// request-log accumulator and the activity-event correlation handle.
struct ForwardContext {
    method: Method,
    path_query: String,
    headers: HeaderMap,
    body: Bytes,
    request_id: u64,
    log_enabled: bool,
    sections: Vec<String>,
    /// Correlates RequestStarted → Routed → Finished in the activity feed.
    activity_id: u64,
    started: std::time::Instant,
    /// The model named in the request body (for the routing log line).
    model: Option<String>,
    /// The backend group this request routes to, OR `None` when routing is
    /// disabled (the legacy single-slot / cross-group-overflow path). When
    /// `Some`, the scheduler is filtered to that group and the leased
    /// credential must belong to it.
    group: Option<BackendGroup>,
    /// Whether the request was actually served by the codex provider, set once
    /// the account is leased and the provider path is chosen (`None` before
    /// then, e.g. a pre-routing failure). Drives the activity log's
    /// group/model/effort columns even when `group` is `None` (routing off).
    served_codex: Option<bool>,
}

impl ForwardContext {
    fn log(&mut self, section: String) {
        if self.log_enabled {
            self.sections.push(section);
        }
    }

    fn flush_log(&mut self, state: &AppState) {
        if let Some(logger) = &state.logger {
            if self.log_enabled {
                logger.write(self.request_id, std::mem::take(&mut self.sections));
            }
        }
    }

    /// The (group, model, effort) triple shown in the activity log. Codex: the
    /// configured model + effort; Claude: the inbound model + the thinking
    /// budget. All `None` before the provider path is chosen (early failures).
    fn finished_meta(&self, state: &AppState) -> (Option<String>, Option<String>, Option<String>) {
        match self.served_codex {
            Some(true) => (
                Some("codex".to_string()),
                Some(state.codex.model()),
                state.codex.effort(),
            ),
            Some(false) => (
                Some("claude".to_string()),
                self.model.clone(),
                effort_from_thinking(&self.body),
            ),
            None => (
                self.group.map(|g| g.as_str().to_string()),
                self.model.clone(),
                effort_from_thinking(&self.body),
            ),
        }
    }

    /// Emit the terminal activity event for this request.
    fn emit_finished(
        &self,
        state: &AppState,
        account: Option<&AccountId>,
        status: StatusCode,
        tokens: Option<TokenCounts>,
    ) {
        let (group, model, effort) = self.finished_meta(state);
        state.emit(ActivityEvent::RequestFinished {
            id: self.activity_id,
            method: self.method.to_string(),
            path: self.path_query.clone(),
            account: account.map(|a| a.0.clone()),
            status: status.as_u16(),
            duration: self.started.elapsed(),
            tokens,
            group,
            model,
            effort,
        });
    }
}

/// Map the inbound Anthropic `thinking` block to a compact effort label for
/// the activity log: `{budget/1000}k` when extended thinking is enabled, else
/// `None`. (For codex the effort comes from config, not the body.)
fn effort_from_thinking(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let thinking = v.get("thinking")?;
    if thinking.get("type").and_then(|t| t.as_str()) != Some("enabled") {
        return None;
    }
    match thinking.get("budget_tokens").and_then(|b| b.as_u64()) {
        Some(b) => Some(format!("{}k", (b / 1000).max(1))),
        None => Some("on".to_string()),
    }
}

fn format_headers(headers: &HeaderMap) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("  {name}: {}", value.to_str().unwrap_or("<binary>")))
        .collect::<Vec<_>>()
        .join("\n")
}

fn body_excerpt(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().min(BODY_LOG_LIMIT)]).into_owned()
}

/// Read an upstream ERROR response body and condense it to a one-line detail
/// for the activity log. Consumes the response — only call it on paths that
/// would otherwise discard the body (429, 5xx). This is what lets the operator
/// tell a real per-account `rate_limit_error` apart from Anthropic's own
/// transient 429/5xx (`overloaded_error`, …), which the bare status hides.
async fn upstream_error_detail(response: reqwest::Response) -> String {
    match response.bytes().await {
        Ok(body) => condense_error_body(&body),
        Err(err) => format!("<error body unreadable: {err}>"),
    }
}

/// Condense an error body to `type: message` (the Anthropic/codex error shape
/// `{"error":{"type","message"}}`) or a trimmed raw excerpt. Pure + testable.
fn condense_error_body(body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(err) = v.get("error") {
            let ty = err.get("type").and_then(|t| t.as_str()).unwrap_or("error");
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
            return if msg.is_empty() {
                ty.to_string()
            } else {
                format!("{ty}: {msg}")
            };
        }
    }
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "<empty body>".to_string()
    } else {
        trimmed.chars().take(300).collect()
    }
}

/// Anthropic-style JSON error response.
fn error_response(status: StatusCode, error_type: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": error_type, "message": message },
    });
    let mut response = Response::new(axum::body::Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Pool exhausted: 429 + `retry-after` = soonest reset (FR3.5).
fn exhausted_response(retry_after: Option<Duration>, accounts: usize) -> Response {
    let secs = retry_after
        .map(|d| d.as_secs().max(1))
        .unwrap_or(DEFAULT_CLIENT_RETRY_AFTER_SECS);
    let mut response = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_error",
        &format!("All {accounts} accounts are rate-limited right now; retry in {secs}s."),
    );
    if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// Routing dead-end: the model's backend group has no configured account and
/// `on_empty_group="error"` — a clean Anthropic-shaped 404 not_found_error.
fn not_found_response(message: &str) -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found_error", message)
}

/// Transient upstream failure: 502 + `Connection: close` (see module docs —
/// the axum-feasible equivalent of Node's socket destroy).
fn transient_response(detail: &str) -> Response {
    let mut response = error_response(
        StatusCode::BAD_GATEWAY,
        "proxy_error",
        &format!("transient upstream error: {detail}"),
    );
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// Forward one client request upstream: buffer the body (needed for retry),
/// then run the lease → refresh → rewrite → send → taxonomy loop until the
/// request is relayed or the pool is exhausted. Once a response starts
/// streaming back, the account is pinned and errors propagate to the client
/// (never switch mid-stream).
pub async fn forward(state: &AppState, req: axum::extract::Request) -> Response {
    let started = std::time::Instant::now();
    let activity_id = state.next_request_id();
    let (parts, body) = req.into_parts();
    let path_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    state.emit(ActivityEvent::RequestStarted {
        id: activity_id,
        method: parts.method.to_string(),
        path: path_query.clone(),
    });
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            state.emit(ActivityEvent::RequestFinished {
                id: activity_id,
                method: parts.method.to_string(),
                path: path_query,
                account: None,
                status: StatusCode::BAD_REQUEST.as_u16(),
                duration: started.elapsed(),
                tokens: None,
                group: None,
                model: None,
                effort: None,
            });
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("failed to read request body: {err}"),
            );
        }
    };
    let log_enabled = state.logger.is_some();
    // Parse the model once (body is buffered) and classify to a backend group.
    // Routing disabled ⇒ group = None (legacy single-slot path); the
    // classifier is not consulted on the forward path in that case.
    let model = crate::routing::model_from_body(&body);
    let group = if state.config.routing.enabled {
        Some(state.classifier.classify(model.as_deref()))
    } else {
        None
    };
    let mut ctx = ForwardContext {
        method: parts.method,
        path_query,
        headers: parts.headers,
        body,
        request_id: state
            .logger
            .as_ref()
            .map(|l| l.next_request_id())
            .unwrap_or_default(),
        log_enabled,
        sections: Vec::new(),
        activity_id,
        started,
        model,
        group,
        served_codex: None,
    };
    if log_enabled && !ctx.body.is_empty() {
        ctx.log(format!(
            "=== REQUEST BODY ({} bytes) ===\n{}",
            ctx.body.len(),
            body_excerpt(&ctx.body)
        ));
    }
    run_taxonomy_loop(state, &mut ctx).await
}

async fn run_taxonomy_loop(state: &AppState, ctx: &mut ForwardContext) -> Response {
    let params = state.select_params();
    let snapshot = state.pool.snapshot();
    let accounts = snapshot.accounts.len();
    let max_switches = accounts.max(1);
    let mut switches = 0usize;
    let mut same_account_waits = 0u32;
    // Accounts already granted their one forced post-401 refresh.
    let mut force_refreshed: HashSet<AccountId> = HashSet::new();

    // Resolve the effective routing group, applying `on_empty_group` when the
    // model's group has no configured account. `None` = legacy path.
    let group = match resolve_group(state, ctx, &snapshot) {
        Ok(group) => group,
        Err(response) => return *response,
    };

    loop {
        // 1. Lease the current account for the group (evaluate on demand when
        // none).
        let lease = match acquire_lease(state, group, &params) {
            Ok(lease) => lease,
            Err(retry_after) => {
                ctx.log("=== ERROR ===\nall accounts exhausted".to_string());
                ctx.flush_log(state);
                state.emit(ActivityEvent::Error {
                    context: Some("scheduler".into()),
                    message: format!("all {accounts} accounts exhausted"),
                });
                ctx.emit_finished(state, None, StatusCode::TOO_MANY_REQUESTS, None);
                return exhausted_response(retry_after, accounts);
            }
        };
        let account = lease.account_id().clone();
        let mut credential = lease.credential().clone();
        // Routing invariant: when a group filter is active the leased
        // credential MUST belong to that group — a mismatch is a routing bug
        // (the scheduler handed back an out-of-group account), never served.
        if let Some(group) = group {
            let leased_group = BackendGroup::from_kind(credential.kind());
            if leased_group != group {
                tracing::error!(
                    account = %account, ?group, ?leased_group,
                    "routing bug: leased credential does not match the request group"
                );
                ctx.log(format!(
                    "=== ERROR ===\nrouting bug: {account} is {leased_group} but request routed to {group}"
                ));
                ctx.flush_log(state);
                drop(lease);
                ctx.emit_finished(
                    state,
                    Some(&account),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                );
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "proxy_error",
                    "internal routing error: account/group mismatch",
                );
            }
        }
        // One-line routing trace: model → group → account.
        tracing::info!(
            model = ctx.model.as_deref().unwrap_or("<none>"),
            group = group.map(|g| g.as_str()).unwrap_or("legacy"),
            account = %account,
            "routing: model={} -> group={} -> account={}",
            ctx.model.as_deref().unwrap_or("<none>"),
            group.map(|g| g.as_str()).unwrap_or("legacy"),
            account,
        );
        // Served (group, model) for in-flight model attribution (req11): codex
        // → the configured upstream model; claude → the inbound model. Mirrors
        // `finished_meta` so the in-flight row matches its eventual finish.
        let (served_group, served_model) = match BackendGroup::from_kind(credential.kind()) {
            BackendGroup::Codex => (Some("codex".to_string()), Some(state.codex.model())),
            _ => (Some("claude".to_string()), ctx.model.clone()),
        };
        state.emit(ActivityEvent::RequestRouted {
            id: ctx.activity_id,
            account: account.0.clone(),
            group: served_group,
            model: served_model,
        });

        // 2. Proactive refresh: oauth-style tokens (anthropic oauth AND
        // codex chatgpt tokens) expiring within 5 minutes.
        if let Some(expires_at_ms) = refreshable_expiry(&credential) {
            if expiring_soon(expires_at_ms) {
                match refresh_credential(state, &account, &credential).await {
                    RefreshOutcome::Refreshed(fresh) => credential = fresh,
                    RefreshOutcome::Permanent => {
                        state.pool.record_auth_failure(&account);
                        state.emit(ActivityEvent::Error {
                            context: Some("refresh".into()),
                            message: format!("{account}: refresh token dead; re-login required"),
                        });
                        drop(lease);
                        switches += 1;
                        if switches > max_switches {
                            ctx.flush_log(state);
                            ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                            return error_response(
                                StatusCode::BAD_GATEWAY,
                                "proxy_error",
                                "account retries exhausted (token refresh failed)",
                            );
                        }
                        continue;
                    }
                    // Transient refresh failure: try the old token; a 401
                    // lands in the forced-refresh path below.
                    RefreshOutcome::Failed => {}
                }
            }
        }

        // 3. Codex accounts serve the Messages API only: count_tokens is
        // answered locally with a naive estimate (no upstream equivalent);
        // any other endpoint is a clear 501.
        //
        // With routing ON the codex path is driven by the request's GROUP
        // (`group == Codex`) — which, by the invariant asserted above, always
        // matches the leased credential's kind. With routing OFF (`group` is
        // `None`) it falls back to the legacy credential check (codex stays
        // the cross-group overflow pool).
        let is_codex = match group {
            Some(g) => g == BackendGroup::Codex,
            None => matches!(credential, AccountCredential::Codex { .. }),
        };
        // Record the served provider so the activity log can show the right
        // group/model/effort even on the legacy (routing-off) path.
        ctx.served_codex = Some(is_codex);
        if is_codex {
            let path = ctx.path_query.split('?').next().unwrap_or("").to_string();
            if path == "/v1/messages/count_tokens" {
                drop(lease);
                return codex_count_tokens_response(state, ctx, &account);
            }
            if path != "/v1/messages" {
                drop(lease);
                ctx.log(format!("=== ERROR ===\ncodex account cannot serve {path}"));
                ctx.flush_log(state);
                ctx.emit_finished(state, Some(&account), StatusCode::NOT_IMPLEMENTED, None);
                return error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "not_supported_error",
                    &format!("codex accounts only serve /v1/messages (requested {path})"),
                );
            }
        }

        // 4. Rewrite + send via the provider hooks (codex: translate the
        // Anthropic body into a Responses API request).
        let rewrite_error = |state: &AppState, ctx: &mut ForwardContext, err: String| {
            ctx.log(format!("=== ERROR ===\nprovider rewrite failed: {err}"));
            ctx.flush_log(state);
            error_response(
                StatusCode::BAD_GATEWAY,
                "proxy_error",
                &format!("request rewrite failed: {err}"),
            )
        };
        // `Some(client_stream)` marks the codex transform path; `None` is the
        // untouched byte-identity passthrough.
        let mut codex_stream: Option<bool> = None;
        let (upstream_req, endpoint) = if is_codex {
            match state.codex.build_request(&ctx.body, &credential) {
                Ok((req, client_stream)) => {
                    codex_stream = Some(client_stream);
                    (req, state.codex.endpoint().to_string())
                }
                Err(err) => {
                    ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                    return rewrite_error(state, ctx, err.to_string());
                }
            }
        } else {
            match build_upstream_request(state, ctx, &credential).await {
                Ok(req) => (req, state.provider.endpoint().to_string()),
                Err(err) => {
                    ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                    return rewrite_error(state, ctx, err.to_string());
                }
            }
        };
        if ctx.log_enabled {
            ctx.log(format!(
                "=== REQUEST (account: {account}, switches: {switches}) ===\n{} {}{}\n{}",
                upstream_req.method,
                endpoint.trim_end_matches('/'),
                upstream_req.path,
                format_headers(&upstream_req.headers)
            ));
        }
        let send_result = send_upstream(state, &endpoint, &upstream_req).await;

        let response = match send_result {
            Ok(response) => response,
            Err(err) => match classify_send_error(&err) {
                UpstreamSignal::Transient => {
                    tracing::warn!(account = %account, error = %err, "transient upstream error");
                    ctx.log(format!("=== ERROR ===\ntransient: {err}"));
                    ctx.flush_log(state);
                    state.emit(ActivityEvent::Error {
                        context: Some("upstream".into()),
                        message: format!("transient error on {account}: {err}"),
                    });
                    ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                    return transient_response(&err.to_string());
                }
                _ => {
                    // Persistent: mark the account (the pool's only
                    // health-degrading event is record_auth_failure — it
                    // doubles as the generic "errored, needs attention"
                    // marker; a credential update heals it), switch.
                    tracing::warn!(account = %account, error = %err, "persistent upstream error; switching");
                    ctx.log(format!("=== ERROR ===\npersistent: {err}"));
                    state.pool.record_auth_failure(&account);
                    drop(lease);
                    switches += 1;
                    if switches > max_switches {
                        ctx.flush_log(state);
                        ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                        return error_response(
                            StatusCode::BAD_GATEWAY,
                            "proxy_error",
                            &format!("upstream error after {switches} account attempts: {err}"),
                        );
                    }
                    continue;
                }
            },
        };

        // 4. Feed rate-limit evidence to the scheduler. If this evidence
        // just pushed the CURRENT account over a threshold, re-evaluate NOW
        // (FR3: selection runs when the current account becomes ineligible,
        // not on the next 60s tick) — so the next request lands on the new
        // pick while this in-flight one finishes on its pinned lease.
        let parsed = rl_headers::parse(response.headers());
        if !parsed.is_empty() {
            let now = SystemTime::now();
            state.pool.record_headers(&account, &parsed, now);
            reevaluate_if_current_ineligible(state, group, &params, &account, now);
        }

        // 5. Taxonomy.
        match classify(response.status(), response.headers()) {
            UpstreamSignal::Relay => {
                return match codex_stream {
                    Some(client_stream) => {
                        relay_codex(state, ctx, lease, account, response, client_stream).await
                    }
                    None => relay(state, ctx, lease, account, response).await,
                };
            }
            UpstreamSignal::RateLimited { retry_after } => {
                let headers_log = format_headers(response.headers());
                let detail = upstream_error_detail(response).await;
                ctx.log(format!(
                    "=== RESPONSE 429 (retry-after: {retry_after:?}) ===\n{headers_log}\n{detail}"
                ));
                let retry_note = match retry_after {
                    Some(d) => format!(" · retry-after {}s", d.as_secs()),
                    None => String::new(),
                };
                state.emit(ActivityEvent::Error {
                    context: Some("upstream".into()),
                    message: format!("429 from {account}: {detail}{retry_note}"),
                });
                match retry_after {
                    Some(wait)
                        if wait <= SAME_ACCOUNT_WAIT_MAX
                            && same_account_waits < MAX_SAME_ACCOUNT_WAITS =>
                    {
                        // Short park: wait it out on the same account.
                        same_account_waits += 1;
                        drop(lease);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    Some(wait) => {
                        // Real rate limit with explicit timing: park exactly
                        // that long, switch. Exhaustion here is a genuine "no
                        // quota" 429 → tell the client when to come back.
                        state
                            .pool
                            .record_429(&account, Some(wait), SystemTime::now());
                        drop(lease);
                        switches += 1;
                        if switches > max_switches {
                            let retry =
                                select::soonest_reset(&state.pool.snapshot(), SystemTime::now());
                            ctx.flush_log(state);
                            ctx.emit_finished(
                                state,
                                Some(&account),
                                StatusCode::TOO_MANY_REQUESTS,
                                None,
                            );
                            return exhausted_response(retry, accounts);
                        }
                        continue;
                    }
                    None => {
                        // No retry-after = transient, server-side limit (not the
                        // account's quota). Brief self-healing park, switch. If
                        // EVERY account is momentarily limited, return a
                        // transient 502 so the client retries promptly — never a
                        // long "quota exhausted" park on a server-side blip.
                        state.pool.record_429(&account, None, SystemTime::now());
                        drop(lease);
                        switches += 1;
                        if switches > max_switches {
                            ctx.flush_log(state);
                            ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                            return transient_response(
                                "upstream is temporarily rate-limiting (not a usage limit)",
                            );
                        }
                        continue;
                    }
                }
            }
            UpstreamSignal::AuthRejected => {
                ctx.log("=== RESPONSE 401 ===".to_string());
                drop(response);
                let oauth = matches!(
                    credential,
                    AccountCredential::Oauth { .. } | AccountCredential::Codex { .. }
                );
                if oauth && !force_refreshed.contains(&account) {
                    force_refreshed.insert(account.clone());
                    if let RefreshOutcome::Refreshed(_) =
                        refresh_credential(state, &account, &credential).await
                    {
                        // Retry the SAME account with the refreshed token
                        // (it is now the pool credential; re-leased next
                        // iteration).
                        drop(lease);
                        continue;
                    }
                }
                // Second 401, refresh failure, or apikey account: auth is
                // dead — mark and switch.
                state.pool.record_auth_failure(&account);
                drop(lease);
                switches += 1;
                if switches > max_switches {
                    ctx.flush_log(state);
                    ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "proxy_error",
                        "all accounts rejected authentication",
                    );
                }
                continue;
            }
            UpstreamSignal::Transient => {
                let status = response.status();
                let headers_log = format_headers(response.headers());
                let detail = upstream_error_detail(response).await;
                tracing::warn!(account = %account, %status, "upstream 5xx; closing client connection");
                ctx.log(format!(
                    "=== RESPONSE {status} (transient) ===\n{headers_log}\n{detail}"
                ));
                state.emit(ActivityEvent::Error {
                    context: Some("upstream".into()),
                    message: format!("{} from {account}: {detail}", status.as_u16()),
                });
                ctx.flush_log(state);
                ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                return transient_response(&format!("upstream returned {status}"));
            }
            UpstreamSignal::Persistent => {
                state.pool.record_auth_failure(&account);
                drop(lease);
                switches += 1;
                if switches > max_switches {
                    ctx.flush_log(state);
                    ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "proxy_error",
                        "persistent upstream errors on every account",
                    );
                }
                continue;
            }
        }
    }
}

/// Re-run selection immediately when fresh evidence shows the CURRENT
/// account is no longer eligible (threshold crossed, window data updated).
/// In-flight leases stay pinned; only the pool's `current` moves. Emits
/// `AccountSwitched` when a switch commits.
fn reevaluate_if_current_ineligible(
    state: &AppState,
    group: Option<BackendGroup>,
    params: &select::SelectParams,
    account: &AccountId,
    now: SystemTime,
) {
    let slot = group.unwrap_or(BackendGroup::Claude);
    let snapshot = state.pool.snapshot();
    if snapshot.current.get(&slot) != Some(account) {
        return;
    }
    let Some(target) = snapshot.accounts.iter().find(|a| &a.id == account) else {
        return;
    };
    let headers_only = select::headers_only_mode(&snapshot, params, group, now);
    let Some(reason) = select::eligibility(target, params, now, headers_only) else {
        return; // still eligible — session stickiness holds
    };
    let before = snapshot.current.get(&slot).cloned();
    if let Decision::Switch { to } = state.pool.evaluate(group, params, now) {
        tracing::info!(from = %account, to = %to, ?reason, "current account became ineligible; switched");
        state.emit(ActivityEvent::AccountSwitched {
            from: before.map(|id| id.0),
            to: to.0,
            reason: Some(format!("{reason:?}")),
        });
    }
}

/// Lease the current account for `group`; when that fails, run one selection
/// pass and try once more. `Err` carries the soonest-reset hint for the
/// client 429.
fn acquire_lease(
    state: &AppState,
    group: Option<BackendGroup>,
    params: &crate::scheduler::select::SelectParams,
) -> Result<crate::scheduler::AccountLease, Option<Duration>> {
    if let Ok(lease) = state.pool.lease_for(group) {
        return Ok(lease);
    }
    match state.pool.evaluate(group, params, SystemTime::now()) {
        Decision::Exhausted { retry_after } => Err(retry_after),
        Decision::Stay | Decision::Switch { .. } => {
            state.pool.lease_for(group).map_err(|err| err.retry_after)
        }
    }
}

/// Resolve the effective routing group for a request, applying the
/// `on_empty_group` policy. Returns:
/// - `Ok(None)` — routing disabled (legacy single-slot path).
/// - `Ok(Some(group))` — routing on; the model's group has ≥1 configured
///   account (or `on_empty_group="fallback"` redirected to a group that does).
/// - `Err(response)` — `on_empty_group="error"` and the matched group has no
///   configured account: a clean Anthropic-shaped 404 not_found_error. The
///   other group's accounts are left untouched.
fn resolve_group(
    state: &AppState,
    ctx: &ForwardContext,
    snapshot: &crate::scheduler::PoolSnapshot,
) -> Result<Option<BackendGroup>, Box<Response>> {
    let Some(group) = ctx.group else {
        return Ok(None); // routing disabled
    };
    let has_account = |g: BackendGroup| snapshot.accounts.iter().any(|a| a.group == g);
    if has_account(group) {
        return Ok(Some(group));
    }
    // Matched group is empty — apply the policy.
    let model = ctx.model.as_deref().unwrap_or("<none>");
    if state
        .config
        .routing
        .on_empty_group
        .eq_ignore_ascii_case("fallback")
    {
        let other = match group {
            BackendGroup::Claude => BackendGroup::Codex,
            BackendGroup::Codex => BackendGroup::Claude,
        };
        if has_account(other) {
            tracing::info!(
                model, from = %group, to = %other,
                "routing: matched group empty; on_empty_group=fallback → other group"
            );
            return Ok(Some(other));
        }
        // Neither group has an account — fall through to the 404.
    }
    let message = format!("no {group} account configured for model {model}");
    tracing::warn!(model, %group, "routing: {message}");
    Err(Box::new(not_found_response(&message)))
}

/// Expiry of a refreshable (oauth-style) credential: anthropic `Oauth` and
/// `Codex` both rotate access tokens; `Apikey` never expires.
fn refreshable_expiry(credential: &AccountCredential) -> Option<u64> {
    match credential {
        AccountCredential::Oauth { expires_at_ms, .. }
        | AccountCredential::Codex { expires_at_ms, .. } => Some(*expires_at_ms),
        AccountCredential::Apikey { .. } => None,
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn expiring_soon(expires_at_ms: u64) -> bool {
    expires_at_ms <= now_epoch_ms().saturating_add(REFRESH_AHEAD_MS)
}

pub(crate) enum RefreshOutcome {
    /// New tokens are live in the pool (and persisted); use this credential.
    Refreshed(AccountCredential),
    /// Refresh token is dead (401/invalid_grant) — re-login required.
    Permanent,
    /// Transient refresh failure — old token may still work.
    Failed,
}

/// Refresh an oauth credential through the [`RefreshCoalescer`] (concurrent
/// callers share one in-flight refresh), update the pool, and persist the
/// new tokens via read-merge-write `config::update_path` (off the runtime's
/// worker threads — file IO via `spawn_blocking`). `pub(crate)` because the
/// server's background refresh task reuses this exact path, so request-time
/// and background refreshes coalesce.
pub(crate) async fn refresh_credential(
    state: &AppState,
    account: &AccountId,
    credential: &AccountCredential,
) -> RefreshOutcome {
    // (refresh_token, identity for persistence, refresh future) per kind.
    // Anthropic refreshes coalesce via the RefreshCoalescer; codex refreshes
    // go direct to the OpenAI token endpoint (form-encoded grant) — the
    // coalescer stays anthropic-specific by design (v1; a concurrent codex
    // double-refresh is harmless, OpenAI refresh tokens are reusable).
    let outcome = match credential {
        AccountCredential::Oauth { refresh_token, .. } => {
            state
                .refresher
                .refresh(&state.client, &account.0, refresh_token)
                .await
        }
        AccountCredential::Codex { refresh_token, .. } => {
            crate::auth::codex::refresh_codex_at(
                &state.client,
                &state.config.codex.token_url,
                refresh_token,
            )
            .await
        }
        AccountCredential::Apikey { .. } => return RefreshOutcome::Failed,
    };
    match outcome {
        Ok(tokens) => {
            // One refresh timestamp shared by the pool credential and the
            // persisted config so both views agree on "refreshed N ago".
            let refreshed_at_ms = now_epoch_ms();
            let (fresh, ident) = match credential {
                AccountCredential::Oauth {
                    account_uuid,
                    refresh_token,
                    tier,
                    ..
                } => (
                    AccountCredential::Oauth {
                        account_uuid: account_uuid.clone(),
                        access_token: tokens.access_token.clone(),
                        refresh_token: tokens
                            .refresh_token
                            .clone()
                            .unwrap_or_else(|| refresh_token.clone()),
                        expires_at_ms: tokens.expires_at_ms,
                        tier: tier.clone(),
                        last_refresh_ms: Some(refreshed_at_ms),
                    },
                    non_empty_or(account_uuid, &account.0),
                ),
                AccountCredential::Codex {
                    account_id,
                    refresh_token,
                    ..
                } => (
                    AccountCredential::Codex {
                        account_id: account_id.clone(),
                        access_token: tokens.access_token.clone(),
                        refresh_token: tokens
                            .refresh_token
                            .clone()
                            .unwrap_or_else(|| refresh_token.clone()),
                        expires_at_ms: tokens.expires_at_ms,
                        last_refresh_ms: Some(refreshed_at_ms),
                    },
                    non_empty_or(account_id, &account.0),
                ),
                AccountCredential::Apikey { .. } => unreachable!("filtered above"),
            };
            state.pool.update_credential(account, fresh.clone());
            state.emit(ActivityEvent::TokenRefreshed {
                account: account.0.clone(),
                expires_at_ms: tokens.expires_at_ms,
            });
            persist_tokens(state, ident, &tokens, refreshed_at_ms).await;
            RefreshOutcome::Refreshed(fresh)
        }
        Err(crate::auth::AuthError::RefreshPermanent { status, body }) => {
            tracing::warn!(account = %account, %status, %body, "refresh token dead; re-login required");
            RefreshOutcome::Permanent
        }
        Err(err) => {
            tracing::warn!(account = %account, error = %err, "token refresh failed (transient)");
            RefreshOutcome::Failed
        }
    }
}

fn non_empty_or(preferred: &str, fallback: &str) -> String {
    if preferred.is_empty() {
        fallback.to_string()
    } else {
        preferred.to_string()
    }
}

/// Persist refreshed tokens with read-merge-write semantics. Persistence
/// failure is logged, never fatal: the pool already has the live tokens.
async fn persist_tokens(
    state: &AppState,
    ident: String,
    tokens: &crate::auth::oauth::OAuthTokens,
    refreshed_at_ms: u64,
) {
    let Some(path) = state.config_path.clone() else {
        return;
    };
    let access = tokens.access_token.clone();
    let refresh = tokens.refresh_token.clone();
    let expires = tokens.expires_at_ms;
    let result = tokio::task::spawn_blocking(move || {
        crate::config::update_path(&path, |config| {
            config.update_oauth_tokens(
                &ident,
                &access,
                refresh.as_deref(),
                expires,
                refreshed_at_ms,
            );
        })
    })
    .await;
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => tracing::warn!(error = %err, "failed to persist refreshed tokens"),
        Err(err) => tracing::warn!(error = %err, "token persistence task failed"),
    }
}

/// Run the provider hooks: Anthropic wire → unified → provider wire, strip
/// hop-by-hop, inject the credential. For [`AnthropicPassthrough`] every
/// conversion is identity over refcounted `Bytes` (zero-copy fast path).
async fn build_upstream_request(
    state: &AppState,
    ctx: &ForwardContext,
    credential: &AccountCredential,
) -> Result<ProviderRequest, crate::provider::ProviderError> {
    let wire = AnthropicRequest {
        method: ctx.method.clone(),
        path: ctx.path_query.clone(),
        headers: ctx.headers.clone(),
        body: ctx.body.clone(),
    };
    let unified = state.provider.request_out(wire)?;
    let mut upstream_req = state.provider.request_in(unified)?;
    strip_hop_by_hop(&mut upstream_req.headers);
    state.provider.auth(&mut upstream_req, credential).await?;
    Ok(upstream_req)
}

async fn send_upstream(
    state: &AppState,
    endpoint: &str,
    req: &ProviderRequest,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("{}{}", endpoint.trim_end_matches('/'), req.path);
    let mut builder = state
        .client
        .request(req.method.clone(), url)
        .headers(req.headers.clone());
    if req.method != Method::GET && req.method != Method::HEAD && !req.body.is_empty() {
        builder = builder.body(req.body.clone());
    }
    builder.send().await
}

/// Headers stripped from the upstream response before relaying. We never
/// decompress (accept-encoding was dropped on the way up), so
/// `content-encoding` passes through untouched; `content-length` is
/// recomputed by hyper from the (byte-identical) relayed body.
fn sanitize_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = headers.clone();
    for name in [
        "transfer-encoding",
        "connection",
        "keep-alive",
        "trailer",
        "upgrade",
        "proxy-authenticate",
        "content-length",
    ] {
        out.remove(name);
    }
    out
}

/// Terminal relay of an upstream response. SSE bodies stream through
/// byte-identically with usage extraction on the side; everything else is
/// buffered (Node parity — enables body logging + usage extraction from
/// non-streaming JSON). The lease rides along until the body is fully
/// delivered.
async fn relay(
    state: &AppState,
    ctx: &mut ForwardContext,
    lease: crate::scheduler::AccountLease,
    account: AccountId,
    response: reqwest::Response,
) -> Response {
    let status = response.status();
    let headers = sanitize_response_headers(response.headers());
    ctx.log(format!(
        "=== RESPONSE {status} ===\n{}",
        format_headers(response.headers())
    ));
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    let body = if is_sse {
        let totals = state.totals.clone();
        let logger = state.logger.clone();
        let request_id = ctx.request_id;
        let mut sections = std::mem::take(&mut ctx.sections);
        let log_enabled = ctx.log_enabled;
        let events = state.events.clone();
        let activity_id = ctx.activity_id;
        let method = ctx.method.to_string();
        let path = ctx.path_query.clone();
        let started = ctx.started;
        let (group, model, effort) = ctx.finished_meta(state);
        sse::passthrough_body(response, BODY_LOG_LIMIT, move |usage, captured, error| {
            totals.record(&account, 1, usage.input_tokens, usage.output_tokens);
            if log_enabled {
                sections.push(format!(
                    "=== RESPONSE BODY (streamed, first {} bytes) ===\n{}",
                    captured.len(),
                    String::from_utf8_lossy(&captured)
                ));
                if let Some(error) = error {
                    sections.push(format!("=== ERROR ===\nstream aborted: {error}"));
                }
                if let Some(logger) = logger {
                    logger.write(request_id, sections);
                }
            }
            if let Some(events) = events {
                let _ = events.try_send(ActivityEvent::RequestFinished {
                    id: activity_id,
                    method,
                    path,
                    account: Some(account.0.clone()),
                    status: status.as_u16(),
                    duration: started.elapsed(),
                    tokens: Some(token_counts(usage)),
                    group,
                    model,
                    effort,
                });
            }
            // The lease (and its in-flight pin) lives exactly as long as
            // the stream: dropped here, when the relay finishes.
            drop(lease);
        })
    } else {
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                // Body died before we sent anything to the client —
                // transient per the taxonomy (client retries).
                ctx.log(format!("=== ERROR ===\nbody read failed: {err}"));
                ctx.flush_log(state);
                ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                return transient_response(&format!("upstream body read failed: {err}"));
            }
        };
        let usage = usage_from_json_body(&bytes);
        state
            .totals
            .record(&account, 1, usage.input_tokens, usage.output_tokens);
        ctx.log(format!(
            "=== RESPONSE BODY ({} bytes) ===\n{}",
            bytes.len(),
            body_excerpt(&bytes)
        ));
        ctx.flush_log(state);
        ctx.emit_finished(state, Some(&account), status, Some(token_counts(usage)));
        drop(lease);
        axum::body::Body::from(bytes)
    };

    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// `/v1/messages/count_tokens` on a codex account: no upstream equivalent —
/// answer locally with a naive chars/4 estimate (good enough for Claude
/// Code's context-window bookkeeping, and strictly better than an error).
///
/// Deliberately NOT codex-traced: it makes no upstream call, so there is no
/// "hung vs completed" question and no real upstream usage to record — the
/// trace exists to diagnose the `/v1/messages` relay path. Tracing it would
/// only add instant, usage-less noise to the file.
fn codex_count_tokens_response(
    state: &AppState,
    ctx: &mut ForwardContext,
    account: &AccountId,
) -> Response {
    let estimate = serde_json::from_slice::<serde_json::Value>(&ctx.body)
        .map(|v| crate::provider::codex::estimate_input_tokens(&v))
        .unwrap_or(1);
    ctx.log(format!(
        "=== RESPONSE (codex count_tokens estimate: {estimate}) ==="
    ));
    ctx.flush_log(state);
    ctx.emit_finished(state, Some(account), StatusCode::OK, None);
    let body = serde_json::json!({ "input_tokens": estimate });
    let mut response = Response::new(axum::body::Body::from(body.to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Terminal relay for a codex upstream response. Every request goes upstream
/// with `stream: true`, so a 2xx from `/responses` IS a Responses SSE stream
/// by contract — the real chatgpt.com backend sends streaming 200s with NO
/// `content-type` header at all (live capture 2026-06-12), so sniffing the
/// header would misclassify good streams. 2xx therefore always enters the
/// transform path: converted to Anthropic SSE on the fly (streaming clients)
/// or aggregated into one Messages JSON document (non-streaming clients); a
/// 2xx body that is not actually SSE terminates with a clean Anthropic
/// `error` event from the converter. Non-2xx bodies are wrapped into
/// Anthropic error shapes — codex bytes are NEVER relayed verbatim (the
/// client speaks the Anthropic wire format only).
async fn relay_codex(
    state: &AppState,
    ctx: &mut ForwardContext,
    lease: crate::scheduler::AccountLease,
    account: AccountId,
    response: reqwest::Response,
    client_stream: bool,
) -> Response {
    let status = response.status();
    // Codex trace (best-effort): input breakdown captured now from the inbound
    // body, terminal outcome written at each return below. `model` is what the
    // request is served as (codex's configured model).
    let trace = crate::proxy::codex_trace::CodexTrace::from_request(
        state.config.codex.trace,
        ctx.activity_id,
        &ctx.path_query,
        Some(state.codex.model()),
        &ctx.body,
    );
    ctx.log(format!(
        "=== RESPONSE {status} (codex) ===\n{}",
        format_headers(response.headers())
    ));
    if !status.is_success() {
        // classify() already diverted 401/429/5xx; what lands here is a 4xx
        // error body — wrapped into an Anthropic-shaped error.
        let bytes = response.bytes().await.unwrap_or_default();
        ctx.log(format!(
            "=== RESPONSE BODY ({} bytes) ===\n{}",
            bytes.len(),
            body_excerpt(&bytes)
        ));
        ctx.flush_log(state);
        let (out_status, error_type) = if status == StatusCode::BAD_REQUEST {
            (status, "invalid_request_error")
        } else {
            (status, "api_error")
        };
        trace.write_error(
            &format!("codex upstream {out_status}: {}", body_excerpt(&bytes)),
            0,
            ctx.started.elapsed().as_millis(),
        );
        ctx.emit_finished(state, Some(&account), out_status, None);
        drop(lease);
        return error_response(
            out_status,
            error_type,
            &format!("codex upstream: {}", body_excerpt(&bytes)),
        );
    }

    if client_stream {
        // Streaming transform relay: upstream Responses events in, Anthropic
        // SSE out. Usage accounting runs on the EMITTED events (converter
        // totals), so the dashboard keeps working.
        let converter = state.codex.converter();
        let totals = state.totals.clone();
        let logger = state.logger.clone();
        let request_id = ctx.request_id;
        let mut sections = std::mem::take(&mut ctx.sections);
        let log_enabled = ctx.log_enabled;
        let events = state.events.clone();
        let activity_id = ctx.activity_id;
        let method = ctx.method.to_string();
        let path = ctx.path_query.clone();
        let started = ctx.started;
        let (group, model, effort) = ctx.finished_meta(state);
        let body = sse::transform_body(
            response,
            converter,
            BODY_LOG_LIMIT,
            move |usage, captured, error, converter, client_gone| {
                totals.record(&account, 1, usage.input_tokens, usage.output_tokens);
                // Codex trace: terminal outcome of the streamed request. A
                // client disconnect mid-stream, an upstream stream error, or a
                // clean completion are distinct outcomes for diagnosis.
                let duration_ms = started.elapsed().as_millis();
                let upstream_events = converter.events_seen();
                if client_gone {
                    trace.write_client_disconnect(upstream_events, duration_ms);
                } else if let Some(error) = &error {
                    trace.write_error(
                        &format!("stream aborted: {error}"),
                        upstream_events,
                        duration_ms,
                    );
                } else {
                    trace.write_completed(converter.raw_usage(), upstream_events, duration_ms);
                }
                if log_enabled {
                    sections.push(format!(
                        "=== RESPONSE BODY (codex→anthropic, first {} bytes) ===\n{}",
                        captured.len(),
                        String::from_utf8_lossy(&captured)
                    ));
                    if let Some(error) = error {
                        sections.push(format!("=== ERROR ===\nstream aborted: {error}"));
                    }
                    if let Some(logger) = logger {
                        logger.write(request_id, sections);
                    }
                }
                if let Some(events) = events {
                    let _ = events.try_send(ActivityEvent::RequestFinished {
                        id: activity_id,
                        method,
                        path,
                        account: Some(account.0.clone()),
                        status: StatusCode::OK.as_u16(),
                        duration: started.elapsed(),
                        tokens: Some(token_counts(usage)),
                        group,
                        model,
                        effort,
                    });
                }
                // Lease pinned for the stream's whole lifetime, as always.
                drop(lease);
            },
        );
        let mut out = Response::new(body);
        *out.status_mut() = StatusCode::OK;
        out.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        out.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        return out;
    }

    // Non-streaming client: consume the whole upstream stream through the
    // converter, then answer with the aggregated Messages JSON.
    use tokio_stream::StreamExt as _;
    let mut converter = state.codex.converter();
    let mut events = sse::EventBuffer::new();
    let mut stream = Box::pin(response.bytes_stream());
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                for event in events.push(&chunk) {
                    let _ = converter.on_event(&event);
                }
            }
            Err(err) => {
                // Nothing was sent to the client yet — transient.
                ctx.log(format!("=== ERROR ===\ncodex stream read failed: {err}"));
                ctx.flush_log(state);
                trace.write_error(
                    &format!("codex upstream stream failed: {err}"),
                    converter.events_seen(),
                    ctx.started.elapsed().as_millis(),
                );
                ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
                drop(lease);
                return transient_response(&format!("codex upstream stream failed: {err}"));
            }
        }
    }
    if let Some(rest) = events.take_remainder() {
        let _ = converter.on_event(&rest);
    }
    let _ = converter.on_end();
    let usage = converter.usage();
    state
        .totals
        .record(&account, 1, usage.input_tokens, usage.output_tokens);
    // Capture converter-level trace detail BEFORE into_message_json consumes it.
    let trace_raw_usage = converter.raw_usage().cloned();
    let trace_events_seen = converter.events_seen();
    let trace_duration_ms = ctx.started.elapsed().as_millis();
    let error_message = converter.error_message().map(str::to_string);
    let result = match converter.into_message_json() {
        Some(message) => {
            ctx.log(format!(
                "=== RESPONSE BODY (codex aggregate) ===\n{message}"
            ));
            trace.write_completed(
                trace_raw_usage.as_ref(),
                trace_events_seen,
                trace_duration_ms,
            );
            ctx.emit_finished(
                state,
                Some(&account),
                StatusCode::OK,
                Some(token_counts(usage)),
            );
            let mut out = Response::new(axum::body::Body::from(message.to_string()));
            out.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            out
        }
        None => {
            let message =
                error_message.unwrap_or_else(|| "codex upstream produced no response".into());
            ctx.log(format!("=== ERROR ===\n{message}"));
            trace.write_error(&message, trace_events_seen, trace_duration_ms);
            ctx.emit_finished(state, Some(&account), StatusCode::BAD_GATEWAY, None);
            error_response(StatusCode::BAD_GATEWAY, "api_error", &message)
        }
    };
    ctx.flush_log(state);
    drop(lease);
    result
}

/// Usage from a non-streaming JSON response body (`{"usage": {...}}`),
/// best-effort like the Node `extractUsageFromBody`.
fn usage_from_json_body(body: &[u8]) -> sse::StreamUsage {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return sse::StreamUsage::default();
    };
    let Some(usage) = value.get("usage") else {
        return sse::StreamUsage::default();
    };
    sse::StreamUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        // Cache counters present only when the upstream reported them (req8/9).
        cache_read_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(serde_json::Value::as_u64),
        cache_creation_input_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(serde_json::Value::as_u64),
    }
}

/// Map observed stream usage into the activity-event token counts, carrying the
/// optional cache counters through to the model-usage rows.
fn token_counts(usage: sse::StreamUsage) -> TokenCounts {
    TokenCounts {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_creation: usage.cache_creation_input_tokens,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::routing::post;
    use axum::Router;

    use super::*;
    use crate::config::{AccountConfig, Config};
    use crate::proxy::server::AppState;
    use crate::scheduler::AccountPool;

    // ---- pure unit tests ----

    fn oauth_credential(token: &str) -> AccountCredential {
        AccountCredential::Oauth {
            account_uuid: "uuid".into(),
            access_token: token.into(),
            refresh_token: "rt".into(),
            expires_at_ms: far_future_ms(),
            tier: None,
            last_refresh_ms: None,
        }
    }

    fn far_future_ms() -> u64 {
        (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64)
            + 3_600_000
    }

    #[test]
    fn rewrite_strips_hop_by_hop_client_auth_and_encoding_then_injects_bearer() {
        let mut headers = HeaderMap::new();
        for name in HOP_BY_HOP_HEADERS {
            headers.insert(
                http::header::HeaderName::from_static(name),
                HeaderValue::from_static("x"),
            );
        }
        headers.insert("accept-encoding", HeaderValue::from_static("gzip"));
        headers.insert("content-length", HeaderValue::from_static("12"));
        headers.insert("x-api-key", HeaderValue::from_static("client-key"));
        headers.insert("authorization", HeaderValue::from_static("Bearer client"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        rewrite_headers(&mut headers, &oauth_credential("at-x"));

        for name in HOP_BY_HOP_HEADERS {
            assert!(headers.get(name).is_none(), "{name} must be stripped");
        }
        assert!(headers.get("accept-encoding").is_none());
        assert!(headers.get("content-length").is_none());
        assert!(headers.get("x-api-key").is_none());
        assert_eq!(
            headers.get("authorization").expect("auth"),
            "Bearer at-x",
            "client authorization replaced by the account credential"
        );
        assert_eq!(
            headers.get("anthropic-version").expect("kept"),
            "2023-06-01"
        );
        assert_eq!(
            headers.get("content-type").expect("kept"),
            "application/json"
        );
    }

    #[test]
    fn rewrite_injects_x_api_key_for_apikey_accounts() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer client"));
        rewrite_headers(
            &mut headers,
            &AccountCredential::Apikey {
                api_key: "sk-ant-api03-k".into(),
            },
        );
        assert_eq!(headers.get("x-api-key").expect("key"), "sk-ant-api03-k");
        assert!(headers.get("authorization").is_none());
    }

    #[test]
    fn classify_follows_the_taxonomy_table() {
        let empty = HeaderMap::new();
        let mut with_retry = HeaderMap::new();
        with_retry.insert("retry-after", HeaderValue::from_static("2"));

        assert_eq!(classify(StatusCode::OK, &empty), UpstreamSignal::Relay);
        assert_eq!(
            classify(StatusCode::NOT_FOUND, &empty),
            UpstreamSignal::Relay,
            "4xx other than 401/429 relays as-is"
        );
        assert_eq!(
            classify(StatusCode::TOO_MANY_REQUESTS, &with_retry),
            UpstreamSignal::RateLimited {
                retry_after: Some(Duration::from_secs(2)),
            }
        );
        assert_eq!(
            classify(StatusCode::TOO_MANY_REQUESTS, &empty),
            UpstreamSignal::RateLimited { retry_after: None }
        );
        assert_eq!(
            classify(StatusCode::UNAUTHORIZED, &empty),
            UpstreamSignal::AuthRejected
        );
        assert_eq!(
            classify(StatusCode::INTERNAL_SERVER_ERROR, &empty),
            UpstreamSignal::Transient
        );
        assert_eq!(
            classify(StatusCode::SERVICE_UNAVAILABLE, &empty),
            UpstreamSignal::Transient
        );
    }

    #[test]
    fn parse_retry_after_accepts_seconds_and_rejects_garbage() {
        let mut headers = HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None, "absent header");

        headers.insert("retry-after", HeaderValue::from_static("2"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(2)));

        headers.insert("retry-after", HeaderValue::from_static("  30 "));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));

        headers.insert("retry-after", HeaderValue::from_static("0"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::ZERO));

        headers.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(
            parse_retry_after(&headers),
            None,
            "HTTP-date form unsupported"
        );

        headers.insert("retry-after", HeaderValue::from_static("-3"));
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn condense_error_body_surfaces_the_real_upstream_reason() {
        // Anthropic/codex error shape → "type: message".
        assert_eq!(
            condense_error_body(
                br#"{"type":"error","error":{"type":"rate_limit_error","message":"overloaded"}}"#
            ),
            "rate_limit_error: overloaded"
        );
        // type-only (no message) → just the type (e.g. their own transient 429).
        assert_eq!(
            condense_error_body(br#"{"error":{"type":"overloaded_error"}}"#),
            "overloaded_error"
        );
        // Non-JSON / empty bodies degrade to a trimmed excerpt, never panic.
        assert_eq!(
            condense_error_body(b"upstream proxy: 429 Too Many Requests"),
            "upstream proxy: 429 Too Many Requests"
        );
        assert_eq!(condense_error_body(b"   \n  "), "<empty body>");
    }

    // ---- mock upstream + integration tests ----

    #[derive(Debug, Clone)]
    enum Scripted {
        /// 200 JSON with unified rate-limit headers.
        Ok {
            body: &'static str,
        },
        /// 200 `text/event-stream` with this exact body.
        OkSse {
            body: &'static str,
        },
        Rate {
            retry_after: Option<u64>,
        },
        /// 401 unless the bearer token matches one of `accept`.
        RequireBearer {
            accept: &'static [&'static str],
            body: &'static str,
        },
    }

    #[derive(Debug, Clone)]
    struct Seen {
        authorization: Option<String>,
        x_api_key: Option<String>,
        path: String,
    }

    #[derive(Clone, Default)]
    struct MockShared {
        script: Arc<Mutex<VecDeque<Scripted>>>,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    async fn mock_handler(
        axum::extract::State(shared): axum::extract::State<MockShared>,
        req: axum::extract::Request,
    ) -> axum::response::Response {
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let key = req
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        shared.seen.lock().expect("seen lock").push(Seen {
            authorization: auth.clone(),
            x_api_key: key,
            path: req.uri().path().to_string(),
        });
        let next = shared
            .script
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or(Scripted::Ok { body: "{}" });
        let reset_5h = SystemTime::now() + Duration::from_secs(3600);
        let reset_7d = SystemTime::now() + Duration::from_secs(86_400);
        let epoch = |t: SystemTime| {
            t.duration_since(UNIX_EPOCH)
                .expect("future")
                .as_secs()
                .to_string()
        };
        match next {
            Scripted::Ok { body } => http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("anthropic-ratelimit-unified-5h-utilization", "0.42")
                .header("anthropic-ratelimit-unified-5h-reset", epoch(reset_5h))
                .header("anthropic-ratelimit-unified-7d-utilization", "0.10")
                .header("anthropic-ratelimit-unified-7d-reset", epoch(reset_7d))
                .body(axum::body::Body::from(body))
                .expect("response"),
            Scripted::OkSse { body } => http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(body))
                .expect("response"),
            Scripted::Rate { retry_after } => {
                let mut builder = http::Response::builder()
                    .status(429)
                    .header("content-type", "application/json");
                if let Some(secs) = retry_after {
                    builder = builder.header("retry-after", secs.to_string());
                }
                builder
                    .body(axum::body::Body::from(
                        r#"{"type":"error","error":{"type":"rate_limit_error"}}"#,
                    ))
                    .expect("response")
            }
            Scripted::RequireBearer { accept, body } => {
                let authorized = auth
                    .as_deref()
                    .is_some_and(|a| accept.iter().any(|t| a == format!("Bearer {t}")));
                if authorized {
                    http::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body))
                        .expect("response")
                } else {
                    http::Response::builder()
                        .status(401)
                        .body(axum::body::Body::from(
                            r#"{"type":"error","error":{"type":"authentication_error"}}"#,
                        ))
                        .expect("response")
                }
            }
        }
    }

    async fn token_handler() -> axum::response::Response {
        http::Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":3600}"#,
            ))
            .expect("response")
    }

    /// In-process mock upstream on 127.0.0.1:0; also serves the token
    /// endpoint at `/mock/token`.
    async fn spawn_mock(shared: MockShared) -> String {
        let app = Router::new()
            .route("/mock/token", post(token_handler))
            .fallback(mock_handler)
            .with_state(shared);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn oauth_account(name: &str, token: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            credential: AccountCredential::Oauth {
                account_uuid: format!("uuid-{name}"),
                access_token: token.to_string(),
                refresh_token: format!("rt-{name}"),
                expires_at_ms: far_future_ms(),
                tier: None,
                last_refresh_ms: None,
            },
        }
    }

    fn test_state(upstream: &str, accounts: Vec<AccountConfig>) -> AppState {
        let config = Config {
            upstream: upstream.to_string(),
            accounts,
            ..Default::default()
        };
        let pool = AccountPool::new(&config.accounts);
        let mut state = AppState::new(config, pool, None, None).expect("state");
        state.config_path = None; // never touch the real user config in tests
        state
            .pool
            .evaluate(None, &state.select_params(), SystemTime::now());
        state
    }

    fn client_request(body: &str) -> axum::extract::Request {
        http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", "client-supplied-key")
            .header("accept-encoding", "gzip")
            .body(axum::body::Body::from(body.to_string()))
            .expect("request")
    }

    async fn response_body(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec()
    }

    #[tokio::test]
    async fn happy_path_relays_body_and_rewrites_auth() {
        let shared = MockShared::default();
        shared.script.lock().expect("lock").push_back(Scripted::Ok {
            body: r#"{"id":"msg_1","usage":{"input_tokens":7,"output_tokens":3}}"#,
        });
        let upstream = spawn_mock(shared.clone()).await;
        let state = test_state(&upstream, vec![oauth_account("a", "at-a")]);

        let response = forward(&state, client_request(r#"{"model":"m"}"#)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_body(response).await;
        assert_eq!(
            body, br#"{"id":"msg_1","usage":{"input_tokens":7,"output_tokens":3}}"#,
            "body must be byte-identical"
        );

        let seen = shared.seen.lock().expect("lock").clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer at-a"));
        assert_eq!(seen[0].x_api_key, None, "client x-api-key stripped");
        assert_eq!(seen[0].path, "/v1/messages");

        // Rate-limit headers were recorded into the pool.
        let snapshot = state.pool.snapshot();
        let account = &snapshot.accounts[0];
        let five = account.five_hour.expect("5h window recorded");
        assert!((five.utilization - 0.42).abs() < 1e-9);

        // Usage extracted from the JSON body into the proxy totals.
        let totals = state.totals.get(&AccountId("a".into()));
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.input_tokens, 7);
        assert_eq!(totals.output_tokens, 3);
    }

    #[tokio::test]
    async fn long_429_parks_account_and_switches_to_next() {
        let shared = MockShared::default();
        {
            let mut script = shared.script.lock().expect("lock");
            script.push_back(Scripted::Rate {
                retry_after: Some(60),
            });
            script.push_back(Scripted::Ok {
                body: r#"{"ok":1}"#,
            });
        }
        let upstream = spawn_mock(shared.clone()).await;
        let state = test_state(
            &upstream,
            vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
        );

        let response = forward(&state, client_request("{}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await, br#"{"ok":1}"#);

        let seen = shared.seen.lock().expect("lock").clone();
        let auths: Vec<_> = seen
            .iter()
            .filter_map(|s| s.authorization.clone())
            .collect();
        assert_eq!(
            auths,
            vec!["Bearer at-a".to_string(), "Bearer at-b".to_string()],
            "429'd account a, retried on b"
        );

        let snapshot = state.pool.snapshot();
        let a = snapshot
            .accounts
            .iter()
            .find(|acct| acct.id.0 == "a")
            .expect("a");
        assert!(a.cooldown_until.is_some(), "a parked by record_429");
        assert_eq!(snapshot.legacy_current(), Some(&AccountId("b".into())));
    }

    #[tokio::test]
    async fn short_429_waits_and_retries_same_account() {
        let shared = MockShared::default();
        {
            let mut script = shared.script.lock().expect("lock");
            script.push_back(Scripted::Rate {
                retry_after: Some(0),
            });
            script.push_back(Scripted::Ok {
                body: r#"{"ok":1}"#,
            });
        }
        let upstream = spawn_mock(shared.clone()).await;
        let state = test_state(
            &upstream,
            vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
        );

        let response = forward(&state, client_request("{}")).await;
        assert_eq!(response.status(), StatusCode::OK);

        let seen = shared.seen.lock().expect("lock").clone();
        let auths: Vec<_> = seen
            .iter()
            .filter_map(|s| s.authorization.clone())
            .collect();
        assert_eq!(
            auths,
            vec!["Bearer at-a".to_string(), "Bearer at-a".to_string()],
            "retry-after ≤ 5s retries the SAME account"
        );
        let snapshot = state.pool.snapshot();
        assert!(
            snapshot.accounts[0].cooldown_until.is_none(),
            "short wait does not park"
        );
        assert_eq!(snapshot.legacy_current(), Some(&AccountId("a".into())));
    }

    #[tokio::test]
    async fn first_401_forces_refresh_and_retries_same_account() {
        let shared = MockShared::default();
        // Every request requires the REFRESHED token; the stale one 401s.
        {
            let mut script = shared.script.lock().expect("lock");
            for _ in 0..3 {
                script.push_back(Scripted::RequireBearer {
                    accept: &["at-new"],
                    body: r#"{"ok":1}"#,
                });
            }
        }
        let upstream = spawn_mock(shared.clone()).await;
        let mut state = test_state(&upstream, vec![oauth_account("a", "at-stale")]);
        state.refresher = Arc::new(crate::auth::oauth::RefreshCoalescer::with_token_url(
            format!("{upstream}/mock/token"),
        ));
        // Seed a config file so persistence is exercised end-to-end.
        let dir = std::env::temp_dir().join(format!(
            "llmux-fwd-test-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let config_path = dir.join("llmux.json");
        crate::config::save_path(&config_path, &state.config).expect("seed config");
        state.config_path = Some(config_path.clone());

        let response = forward(&state, client_request("{}")).await;
        assert_eq!(response.status(), StatusCode::OK);

        let seen = shared.seen.lock().expect("lock").clone();
        let auths: Vec<_> = seen
            .iter()
            .filter_map(|s| s.authorization.clone())
            .collect();
        assert_eq!(
            auths,
            vec!["Bearer at-stale".to_string(), "Bearer at-new".to_string()],
            "401 forced one refresh, then retried the same account"
        );

        // Refreshed tokens persisted via read-merge-write.
        let persisted = crate::config::load_path(&config_path).expect("reload");
        match &persisted.accounts[0].credential {
            AccountCredential::Oauth {
                access_token,
                refresh_token,
                ..
            } => {
                assert_eq!(access_token, "at-new");
                assert_eq!(refresh_token, "rt-new");
            }
            other => panic!("unexpected credential {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn second_401_marks_auth_failed_and_switches() {
        let shared = MockShared::default();
        // Only account b's token is ever accepted: a 401s before AND after
        // its forced refresh.
        {
            let mut script = shared.script.lock().expect("lock");
            for _ in 0..4 {
                script.push_back(Scripted::RequireBearer {
                    accept: &["at-b"],
                    body: r#"{"ok":1}"#,
                });
            }
        }
        let upstream = spawn_mock(shared.clone()).await;
        let mut state = test_state(
            &upstream,
            vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
        );
        state.refresher = Arc::new(crate::auth::oauth::RefreshCoalescer::with_token_url(
            format!("{upstream}/mock/token"),
        ));

        let response = forward(&state, client_request("{}")).await;
        assert_eq!(response.status(), StatusCode::OK);

        let seen = shared.seen.lock().expect("lock").clone();
        let auths: Vec<_> = seen
            .iter()
            .filter_map(|s| s.authorization.clone())
            .collect();
        assert_eq!(
            auths,
            vec![
                "Bearer at-a".to_string(),   // first 401
                "Bearer at-new".to_string(), // refreshed, second 401
                "Bearer at-b".to_string(),   // switched
            ]
        );

        let snapshot = state.pool.snapshot();
        let a = snapshot
            .accounts
            .iter()
            .find(|acct| acct.id.0 == "a")
            .expect("a");
        assert!(!a.healthy, "a marked AuthFailed after the second 401");
        assert_eq!(snapshot.legacy_current(), Some(&AccountId("b".into())));
    }

    #[tokio::test]
    async fn exhausted_pool_returns_429_with_soonest_reset() {
        let upstream = "http://127.0.0.1:9"; // never reached
        let state = test_state(upstream, vec![oauth_account("a", "at-a")]);
        state.pool.record_429(
            &AccountId("a".into()),
            Some(Duration::from_secs(1800)),
            SystemTime::now(),
        );

        let response = forward(&state, client_request("{}")).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 = response
            .headers()
            .get("retry-after")
            .expect("retry-after header")
            .to_str()
            .expect("ascii")
            .parse()
            .expect("seconds");
        assert!(
            (1790..=1800).contains(&retry_after),
            "retry-after ≈ soonest reset, got {retry_after}"
        );
        let body = response_body(response).await;
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["error"]["type"], "rate_limit_error");
    }

    #[tokio::test]
    async fn sse_stream_is_byte_identical_and_usage_recorded() {
        // Includes a malformed event — bytes must still pass through 1:1.
        const SSE_BODY: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\ndata: {malformed json\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n";
        let shared = MockShared::default();
        shared
            .script
            .lock()
            .expect("lock")
            .push_back(Scripted::OkSse { body: SSE_BODY });
        let upstream = spawn_mock(shared.clone()).await;
        let state = test_state(&upstream, vec![oauth_account("a", "at-a")]);

        let response = forward(&state, client_request(r#"{"stream":true}"#)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .expect("content-type"),
            "text/event-stream"
        );
        let body = response_body(response).await;
        assert_eq!(body, SSE_BODY.as_bytes(), "SSE passthrough byte-identical");

        // The finish hook runs after the last chunk; poll briefly.
        let account = AccountId("a".into());
        let mut totals = state.totals.get(&account);
        for _ in 0..50 {
            if totals.requests > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            totals = state.totals.get(&account);
        }
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.input_tokens, 25);
        assert_eq!(totals.output_tokens, 42);
    }

    #[tokio::test]
    async fn unreachable_upstream_is_transient_and_closes_connection() {
        // Port 9 (discard) on localhost: connection refused → transient.
        let state = test_state("http://127.0.0.1:9", vec![oauth_account("a", "at-a")]);
        let response = forward(&state, client_request("{}")).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response.headers().get("connection").expect("connection"),
            "close",
            "transient errors close the client connection"
        );
        let snapshot = state.pool.snapshot();
        assert!(
            snapshot.accounts[0].healthy,
            "transient errors do not mark the account"
        );
    }
}
