//! End-to-end acceptance scenarios (spec §Acceptance): spawn a
//! `MockUpstream` + the real proxy server, drive Claude-Code-shaped requests
//! through a TCP socket, assert scheduler behavior from the outside.
//!
//! Isolation: every test owns its mock, its proxy (port 0), and a tempdir
//! config — nothing touches the real `~/.config` or `~/.claude`. Only the
//! import scenario mutates env vars (`HOME`, `XDG_CONFIG_HOME`,
//! `LLMUX_CONFIG`) and does so under a process-wide lock.

#[path = "mock_upstream.rs"]
mod mock_upstream;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use llmux::config::{self, AccountConfig, AccountCredential, Config};
use llmux::proxy::server::{serve, AppState};
use llmux::scheduler::select::SelectParams;
use llmux::scheduler::{AccountId, AccountPool};
use mock_upstream::{MockUpstream, ScriptedResponse};

/// Serializes the env-mutating test(s); everything else stays env-free.
/// Async-aware because the guard spans an `.await` (the import call).
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Self-cleaning unique temp dir (no tempfile dev-dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "llmux-e2e-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn far_future_ms() -> u64 {
    // Beyond the 7h background-refresh window (scheduler.refresh_ahead_secs)
    // — accounts built with this must never be refreshed behind a test's
    // back by the server's background token-refresh task.
    epoch_ms_now() + 24 * 3_600 * 1_000
}

fn epoch_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

fn oauth_account(name: &str, token: &str) -> AccountConfig {
    oauth_account_expiring(name, token, far_future_ms())
}

fn oauth_account_expiring(name: &str, token: &str, expires_at_ms: u64) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        credential: AccountCredential::Oauth {
            account_uuid: format!("uuid-{name}"),
            access_token: token.to_string(),
            refresh_token: format!("rt-{name}"),
            expires_at_ms,
            tier: None,
            last_refresh_ms: None,
        },
    }
}

fn default_params() -> SelectParams {
    SelectParams::from(&llmux::config::SchedulerConfig::default())
}

/// One running proxy over a tempdir config, listening on an OS-assigned port.
struct Proxy {
    addr: SocketAddr,
    pool: AccountPool,
    config_path: PathBuf,
    _tmp: TempDir,
}

impl Proxy {
    async fn spawn(upstream: &str, accounts: Vec<AccountConfig>) -> Self {
        Self::spawn_config(Config {
            upstream: upstream.to_string(),
            accounts,
            ..Default::default()
        })
        .await
    }

    /// [`Self::spawn`] over a fully custom config (codex tests point
    /// `config.codex` at the mock).
    async fn spawn_config(mut config: Config) -> Self {
        let tmp = TempDir::new();
        let config_path = tmp.path().join("llmux.json");
        config.proxy.port = 0; // OS-assigned; `serve` reports it via `ready`
        config::save_path(&config_path, &config).expect("seed config");

        let pool = AccountPool::new(&config.accounts);
        let mut state = AppState::new(config, pool.clone(), None, None).expect("app state");
        // Persist refreshed tokens into the tempdir config, never the real
        // user config (AppState::new defaulted to the env-resolved path).
        state.config_path = Some(config_path.clone());

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(serve(state, Some(ready_tx)));
        let addr = ready_rx.await.expect("proxy ready");
        Self {
            addr,
            pool,
            config_path,
            _tmp: tmp,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.addr.port(), path)
    }
}

/// Claude-Code-shaped request: JSON POST with a client-side `x-api-key`
/// that the proxy must strip.
async fn post_messages(client: &reqwest::Client, proxy: &Proxy, body: &str) -> reqwest::Response {
    client
        .post(proxy.url("/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "client-supplied-key")
        .header("anthropic-version", "2023-06-01")
        .body(body.to_string())
        .send()
        .await
        .expect("proxy reachable")
}

// ---------------------------------------------------------------------------
// 1. Byte-identical relay + auth rewrite
// ---------------------------------------------------------------------------

/// Acceptance #1: a Claude-Code-shaped request through the proxy returns a
/// byte-identical body, with client auth stripped and the selected
/// account's credential injected.
#[tokio::test]
async fn passthrough_returns_identical_body_with_rewritten_auth() {
    const UPSTREAM_BODY: &str =
        r#"{"id":"msg_1","type":"message","usage":{"input_tokens":7,"output_tokens":3}}"#;
    const CLIENT_BODY: &str =
        r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}]}"#;

    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::ok(UPSTREAM_BODY));
    let proxy = Proxy::spawn(&mock.base_url(), vec![oauth_account("a", "at-a")]).await;

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, CLIENT_BODY).await;
    assert_eq!(response.status(), 200);
    let body = response.bytes().await.expect("body");
    assert_eq!(
        body.as_ref(),
        UPSTREAM_BODY.as_bytes(),
        "relayed body must be byte-identical"
    );

    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/messages");
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer at-a"),
        "account credential injected"
    );
    assert_eq!(seen[0].x_api_key, None, "client x-api-key stripped");
    assert_eq!(
        seen[0].body,
        CLIENT_BODY.as_bytes(),
        "request body forwarded byte-identical"
    );
}

/// Acceptance #1b: Claude Code annotates a 1M-window model client-side as
/// `claude-opus-4-8[1m]`. That literal is not a valid Anthropic model id and
/// 404s upstream (plain `claude-opus-4-8` 200s — its 1M window is the
/// default). The passthrough provider must strip the `[1m]` annotation before
/// the request leaves the proxy. This drives the full forward path (so it also
/// proves the provider's `request_in` hook is wired in, not just the unit).
#[tokio::test]
async fn client_context_window_suffix_is_stripped_before_upstream() {
    const UPSTREAM_BODY: &str =
        r#"{"id":"msg_1","type":"message","usage":{"input_tokens":7,"output_tokens":3}}"#;
    const CLIENT_BODY: &str =
        r#"{"model":"claude-opus-4-8[1m]","messages":[{"role":"user","content":"hi"}]}"#;

    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::ok(UPSTREAM_BODY));
    let proxy = Proxy::spawn(&mock.base_url(), vec![oauth_account("a", "at-a")]).await;

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, CLIENT_BODY).await;
    assert_eq!(response.status(), 200);

    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let upstream: serde_json::Value =
        serde_json::from_slice(&seen[0].body).expect("upstream body is json");
    assert_eq!(
        upstream["model"], "claude-opus-4-8",
        "the [1m] context-window suffix must be stripped before upstream"
    );
    assert_eq!(
        upstream["messages"][0]["content"], "hi",
        "request payload preserved aside from the model normalization"
    );
}

// ---------------------------------------------------------------------------
// 2. SSE passthrough under forced chunk fragmentation
// ---------------------------------------------------------------------------

const SSE_BODY: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// Acceptance #2: an SSE stream passes through intact while the upstream
/// fragments events across tiny chunks (events split mid-line and across
/// the `\n\n` terminator).
#[tokio::test]
async fn sse_stream_passes_through_byte_identical_under_fragmentation() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::Sse {
        body: SSE_BODY.to_string(),
        chunk_size: 7, // deliberately misaligned with event boundaries
        chunk_delay: Duration::from_millis(2),
        five_hour: Some((0.10, 3_600)),
        seven_day: Some((0.10, 86_400)),
        content_type: true,
        extra_headers: Vec::new(),
    });
    let proxy = Proxy::spawn(&mock.base_url(), vec![oauth_account("a", "at-a")]).await;

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, r#"{"stream":true}"#).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let body = response.bytes().await.expect("stream body");
    assert_eq!(
        body.as_ref(),
        SSE_BODY.as_bytes(),
        "SSE passthrough must be byte-identical"
    );
}

// ---------------------------------------------------------------------------
// 3. Threshold crossing: next request switches, in-flight stays pinned
// ---------------------------------------------------------------------------

/// Acceptance #3: account A pushed past 98% 5h utilization → the NEXT
/// request lands on account B, while the request that crossed the threshold
/// (still streaming) completes on A.
#[tokio::test]
async fn threshold_crossing_switches_next_request_but_not_in_flight() {
    let mock = MockUpstream::spawn().await;
    // Request 1 (on A): slow SSE whose headers report A at 99% of the 5h
    // window — over the 0.90 ceiling, so the scheduler must move off A.
    mock.push(ScriptedResponse::Sse {
        body: SSE_BODY.to_string(),
        chunk_size: 16,
        chunk_delay: Duration::from_millis(30),
        five_hour: Some((0.99, 3_600)),
        seven_day: Some((0.50, 86_400)),
        content_type: true,
        extra_headers: Vec::new(),
    });
    // Request 2 (must be on B).
    mock.push(ScriptedResponse::ok_with(
        r#"{"id":"msg_b"}"#,
        (0.10, 3_600),
        (0.10, 86_400),
    ));
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
    )
    .await;
    assert_eq!(
        proxy.pool.snapshot().legacy_current().cloned(),
        Some(AccountId("a".into())),
        "initial selection lands on a (stable id order, cold pool)"
    );

    let client = reqwest::Client::new();
    let streaming = {
        let client = client.clone();
        let url = proxy.url("/v1/messages");
        tokio::spawn(async move {
            let response = client
                .post(url)
                .header("x-api-key", "client-supplied-key")
                .body(r#"{"stream":true}"#)
                .send()
                .await
                .expect("request 1");
            (response.status(), response.bytes().await.expect("body"))
        })
    };

    // Wait until request 1's response headers reached the proxy (the switch
    // happens on header receipt, before the body finishes streaming).
    let mut switched = false;
    for _ in 0..100 {
        if proxy.pool.snapshot().legacy_current().cloned() == Some(AccountId("b".into())) {
            switched = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        switched,
        "scheduler must switch off A while A still streams"
    );
    assert_eq!(
        proxy
            .pool
            .snapshot()
            .accounts
            .iter()
            .find(|a| a.id.0 == "a")
            .expect("a")
            .in_flight,
        1,
        "request 1 is still in flight on A after the switch"
    );

    // Request 2 lands on B.
    let response = post_messages(&client, &proxy, "{}").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.bytes().await.expect("body").as_ref(),
        br#"{"id":"msg_b"}"#
    );

    // Request 1 completes on A, byte-identical.
    let (status, body) = streaming.await.expect("streaming task");
    assert_eq!(status, 200);
    assert_eq!(body.as_ref(), SSE_BODY.as_bytes());

    assert_eq!(
        mock.seen_bearers(),
        vec!["Bearer at-a".to_string(), "Bearer at-b".to_string()],
        "request 1 served by A, request 2 by B"
    );
}

// ---------------------------------------------------------------------------
// 4. Two eligible accounts → sooner 7d reset wins
// ---------------------------------------------------------------------------

/// Acceptance #4: with both accounts under threshold, the scheduler picks
/// the one whose 7d window resets sooner (use-it-or-lose-it). Window state
/// arrives via `/api/oauth/usage` polling before the initial selection.
#[tokio::test]
async fn scheduler_picks_account_with_sooner_seven_day_reset() {
    let mock = MockUpstream::spawn().await;
    // a's 7d window resets in 48h; b's in 12h → b must be picked first.
    mock.set_usage("at-a", (0.50, 3_600), (0.50, 48 * 3_600));
    mock.set_usage("at-b", (0.50, 3_600), (0.50, 12 * 3_600));
    mock.push(ScriptedResponse::ok(r#"{"id":"msg_4"}"#));
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
    )
    .await;

    assert_eq!(
        proxy.pool.snapshot().legacy_current().cloned(),
        Some(AccountId("b".into())),
        "initial selection ranks by soonest 7d reset"
    );

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, "{}").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        mock.seen_bearers(),
        vec!["Bearer at-b".to_string()],
        "request served by the sooner-reset account"
    );
}

// ---------------------------------------------------------------------------
// 5. 429 retry-after 2 → parked ~2s, retried, succeeds
// ---------------------------------------------------------------------------

/// Acceptance #5: upstream answers 429 with `retry-after: 2`; the proxy
/// honors the park (~2s), retries, and the request succeeds. The short park
/// is served out on the SAME account (switching for a 2s park would burn
/// session stickiness for nothing).
#[tokio::test]
async fn short_429_parks_two_seconds_then_retries_and_succeeds() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::RateLimited {
        retry_after: Some(2),
    });
    mock.push(ScriptedResponse::ok(r#"{"id":"msg_5"}"#));
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
    )
    .await;

    let client = reqwest::Client::new();
    let started = std::time::Instant::now();
    let response = post_messages(&client, &proxy, "{}").await;
    let elapsed = started.elapsed();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.bytes().await.expect("body").as_ref(),
        br#"{"id":"msg_5"}"#
    );
    assert!(
        elapsed >= Duration::from_secs(2),
        "request must wait out the retry-after park, took {elapsed:?}"
    );
    assert_eq!(
        mock.seen_bearers(),
        vec!["Bearer at-a".to_string(), "Bearer at-a".to_string()],
        "parked and retried on the same account"
    );
}

// ---------------------------------------------------------------------------
// 6. All exhausted → client 429 + soonest-reset retry-after
// ---------------------------------------------------------------------------

/// Acceptance #6: every account 429s with a long park → the client gets a
/// 429 whose `retry-after` is the soonest reset across the pool.
#[tokio::test]
async fn exhausted_pool_returns_429_with_soonest_reset_retry_after() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::RateLimited {
        retry_after: Some(60),
    });
    mock.push(ScriptedResponse::RateLimited {
        retry_after: Some(90),
    });
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
    )
    .await;

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, "{}").await;
    assert_eq!(response.status(), 429);
    let retry_after: u64 = response
        .headers()
        .get("retry-after")
        .expect("retry-after header")
        .to_str()
        .expect("ascii")
        .parse()
        .expect("seconds");
    assert!(
        (55..=60).contains(&retry_after),
        "retry-after ≈ soonest reset (a's 60s park, not b's 90s), got {retry_after}"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.expect("body")).expect("json");
    assert_eq!(body["error"]["type"], "rate_limit_error");

    assert_eq!(
        mock.seen_bearers(),
        vec!["Bearer at-a".to_string(), "Bearer at-b".to_string()],
        "both accounts were tried before giving up"
    );
}

/// A 429 WITHOUT `retry-after` is a transient, server-side limit (Anthropic
/// "Server is temporarily limiting requests (not your usage limit)"), NOT the
/// account's quota. Each account gets only a SHORT self-healing park, so when
/// every account momentarily 429s the client is told to retry in seconds — not
/// the 60-minute heuristic that used to strand fully-usable accounts.
#[tokio::test]
async fn no_retry_after_429_parks_briefly_not_an_hour() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::RateLimited { retry_after: None });
    mock.push(ScriptedResponse::RateLimited { retry_after: None });
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
    )
    .await;

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, "{}").await;
    assert_eq!(response.status(), 429);
    let retry_after: u64 = response
        .headers()
        .get("retry-after")
        .expect("retry-after header")
        .to_str()
        .expect("ascii")
        .parse()
        .expect("seconds");
    assert!(
        retry_after <= 30,
        "transient 429 → short park, got {retry_after}s (was 3600 before the fix)"
    );
    assert_eq!(
        mock.seen_bearers(),
        vec!["Bearer at-a".to_string(), "Bearer at-b".to_string()],
        "both accounts were tried before giving up"
    );
}

// ---------------------------------------------------------------------------
// 7. Expired token → exactly one (coalesced) refresh, config updated
// ---------------------------------------------------------------------------

/// Acceptance #7: an expired access token triggers a proactive refresh;
/// N concurrent requests coalesce into EXACTLY ONE token-endpoint call, all
/// requests go out with the refreshed token, and the refreshed tokens are
/// persisted back into the config file.
#[tokio::test]
async fn expired_token_refreshes_once_for_concurrent_requests_and_persists() {
    const CONCURRENCY: usize = 5;

    let mock = MockUpstream::spawn().await;
    mock.set_token_delay(Duration::from_millis(200)); // widen the race window
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![oauth_account_expiring("a", "at-stale", 1_000)], // long expired
    )
    .await;

    let client = reqwest::Client::new();
    let url = proxy.url("/v1/messages");
    let handles: Vec<_> = (0..CONCURRENCY)
        .map(|_| {
            let client = client.clone();
            let url = url.clone();
            tokio::spawn(async move {
                client
                    .post(url)
                    .header("content-type", "application/json")
                    .header("x-api-key", "client-supplied-key")
                    .body("{}")
                    .send()
                    .await
                    .expect("proxy reachable")
            })
        })
        .collect();
    for handle in handles {
        let response = handle.await.expect("request task");
        assert_eq!(response.status(), 200);
    }

    assert_eq!(
        mock.token_hits(),
        1,
        "{CONCURRENCY} concurrent refreshes must coalesce into one token call"
    );
    let bearers = mock.seen_bearers();
    assert_eq!(bearers.len(), CONCURRENCY);
    for bearer in &bearers {
        assert_eq!(
            bearer,
            &format!("Bearer {}", MockUpstream::REFRESHED_ACCESS_TOKEN),
            "every request must carry the refreshed token"
        );
    }

    // Refreshed tokens persisted (read-merge-write, async spawn_blocking —
    // poll briefly).
    let mut persisted = None;
    for _ in 0..100 {
        let config = config::load_path(&proxy.config_path).expect("reload config");
        if let AccountCredential::Oauth {
            access_token,
            refresh_token,
            ..
        } = &config.accounts[0].credential
        {
            if access_token == MockUpstream::REFRESHED_ACCESS_TOKEN {
                persisted = Some(refresh_token.clone());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        persisted.as_deref(),
        Some(MockUpstream::REFRESHED_REFRESH_TOKEN),
        "refreshed tokens must land in the config file"
    );
}

// ---------------------------------------------------------------------------
// 8. Imports yield working accounts end-to-end
// ---------------------------------------------------------------------------

/// Acceptance #8: `llmux import` over a teamclaude config AND a
/// `~/.claude/.credentials.json` (tmp HOME) yields accounts that serve real
/// requests through the proxy.
#[tokio::test]
async fn import_teamclaude_and_claude_credentials_yield_working_accounts() {
    let tmp = TempDir::new();
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg-config");
    std::fs::create_dir_all(home.join(".claude")).expect("home dirs");
    std::fs::create_dir_all(&xdg).expect("xdg dir");
    let llmux_config = tmp.path().join("llmux.json");

    let expires_ms = far_future_ms();
    std::fs::write(
        home.join(".claude/.credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"at-imported","refreshToken":"rt-imported","expiresAt":{expires_ms},"scopes":["user:inference"]}}}}"#
        ),
    )
    .expect("write credentials");
    std::fs::write(
        xdg.join("teamclaude.json"),
        format!(
            r#"{{"accounts":[{{"name":"tc-acct","type":"oauth","accountUuid":"uuid-tc","accessToken":"at-tc","refreshToken":"rt-tc","expiresAt":{expires_ms}}}]}}"#
        ),
    )
    .expect("write teamclaude config");

    // `import` resolves its default probe paths and the config target from
    // the environment — set them only under the lock, restore before any
    // other await point can observe them.
    {
        let _guard = ENV_LOCK.lock().await;
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let old_cfg = std::env::var_os(config::CONFIG_ENV);
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        std::env::set_var(config::CONFIG_ENV, &llmux_config);

        let result = llmux::cli::import::run(llmux::cli::ImportArgs {
            from: None,
            json: None,
        })
        .await;

        let restore = |key: &str, value: Option<std::ffi::OsString>| match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        };
        restore("HOME", old_home);
        restore("XDG_CONFIG_HOME", old_xdg);
        restore(config::CONFIG_ENV, old_cfg);
        result.expect("import succeeds");
    }

    let imported = config::load_path(&llmux_config).expect("imported config");
    let mut names: Vec<&str> = imported.accounts.iter().map(|a| a.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["account-1", "tc-acct"],
        "both sources imported (credentials file gets a default name)"
    );

    // The imported accounts work end-to-end through the proxy.
    let mock = MockUpstream::spawn().await;
    let proxy = Proxy::spawn(&mock.base_url(), imported.accounts.clone()).await;

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, "{}").await;
    assert_eq!(response.status(), 200);

    // Manually switch to the teamclaude-imported account and prove it too
    // serves traffic (also exercises AccountPool::switch_to end-to-end).
    proxy
        .pool
        .switch_to(
            &AccountId("tc-acct".into()),
            &default_params(),
            SystemTime::now(),
        )
        .expect("manual switch");
    let response = post_messages(&client, &proxy, "{}").await;
    assert_eq!(response.status(), 200);

    assert_eq!(
        mock.seen_bearers(),
        vec!["Bearer at-imported".to_string(), "Bearer at-tc".to_string()],
        "both imported credentials reached the upstream"
    );
}

// ---------------------------------------------------------------------------
// 9. Background token refresh without any client traffic
// ---------------------------------------------------------------------------

/// A2: an oauth token inside the background-refresh window (< 7h remaining)
/// is refreshed by the server's background task WITHOUT any client request
/// hitting the proxy, and the refreshed tokens are persisted. An account
/// outside the window stays untouched (exactly one token-endpoint hit).
#[tokio::test]
async fn background_refresh_renews_expiring_token_without_traffic() {
    let mock = MockUpstream::spawn().await;
    let started_ms = epoch_ms_now();
    let one_hour_left = started_ms + 3_600 * 1_000; // inside the 7h window
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![
            oauth_account_expiring("a", "at-old", one_hour_left),
            oauth_account("b", "at-b"), // 24h out — outside the window
        ],
    )
    .await;

    // NO requests are sent. The background task's first tick (immediate at
    // startup) must refresh account a and persist the new tokens.
    let mut persisted_refresh_token = None;
    for _ in 0..200 {
        let config = config::load_path(&proxy.config_path).expect("reload config");
        if let AccountCredential::Oauth {
            access_token,
            refresh_token,
            ..
        } = &config.accounts[0].credential
        {
            if access_token == MockUpstream::REFRESHED_ACCESS_TOKEN {
                persisted_refresh_token = Some(refresh_token.clone());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        persisted_refresh_token.as_deref(),
        Some(MockUpstream::REFRESHED_REFRESH_TOKEN),
        "background refresh must persist new tokens with zero client traffic"
    );

    assert_eq!(
        mock.token_hits(),
        1,
        "only the account inside the refresh window may be refreshed"
    );
    let config = config::load_path(&proxy.config_path).expect("reload config");
    match &config.accounts[1].credential {
        AccountCredential::Oauth { access_token, .. } => {
            assert_eq!(access_token, "at-b", "far-future account untouched");
        }
        other => panic!("unexpected credential {other:?}"),
    }
    assert!(
        mock.seen().is_empty(),
        "no proxied client request reached the upstream"
    );

    // The pool serves the refreshed credential too (not just the file).
    match proxy.pool.credential(&AccountId("a".into())) {
        Some(AccountCredential::Oauth { access_token, .. }) => {
            assert_eq!(access_token, MockUpstream::REFRESHED_ACCESS_TOKEN);
        }
        other => panic!("unexpected pool credential {other:?}"),
    }

    // The refresh stamped WHEN it happened — persisted in the config file…
    let stamped = config.accounts[0]
        .credential
        .last_refresh_ms()
        .expect("refresh persists last_refresh_ms");
    assert!(
        (started_ms..=epoch_ms_now()).contains(&stamped),
        "last_refresh_ms {stamped} outside the test window"
    );
    assert_eq!(
        config.accounts[1].credential.last_refresh_ms(),
        None,
        "unrefreshed account stays unstamped"
    );

    // …and visible in /llmux/status alongside the token expiry, so the
    // dashboard can show "refreshed N ago" next to the countdown.
    let doc: serde_json::Value = reqwest::Client::new()
        .get(proxy.url("/llmux/status"))
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");
    let account_a = doc["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .find(|a| a["name"] == "a")
        .expect("account a in status")
        .clone();
    assert_eq!(account_a["last_refresh_ms"], stamped);
    assert!(
        account_a["token_expires_at_ms"].as_u64().expect("expiry") > epoch_ms_now(),
        "refreshed token expiry is in the future"
    );
}

/// req1 symmetry: with routing on and both groups present, the proxy selects a
/// current for EACH group independently, logs an initial selection for each,
/// and the dashboard doc carries the per-group current map (so the TUI renders
/// both `current` lines instead of `codex (none)`).
#[tokio::test]
async fn startup_selects_and_logs_each_group_independently() {
    let mock = MockUpstream::spawn().await;
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![
            oauth_account("claudeacct", "at-a"),
            codex_account("codexacct", "at-c"),
        ],
    )
    .await;

    // Engine state: both groups have an independent initial selection.
    let snap = proxy.pool.snapshot();
    assert_eq!(
        snap.current_for_group(llmux::routing::BackendGroup::Claude)
            .cloned(),
        Some(AccountId("claudeacct".into()))
    );
    assert_eq!(
        snap.current_for_group(llmux::routing::BackendGroup::Codex)
            .cloned(),
        Some(AccountId("codexacct".into()))
    );

    // The startup AccountSwitched events fold into the activity hub on a
    // spawned task, so poll briefly for the codex initial-selection note.
    let client = reqwest::Client::new();
    let mut doc = serde_json::Value::Null;
    let mut found_codex_note = false;
    for _ in 0..40 {
        doc = client
            .get(proxy.url("/llmux/dashboard"))
            .send()
            .await
            .expect("dashboard")
            .json()
            .await
            .expect("dashboard json");
        found_codex_note = doc["activity"]["completed"]
            .as_array()
            .map(|notes| {
                notes.iter().any(|c| {
                    c["text"]
                        .as_str()
                        .is_some_and(|t| t.contains("codexacct") && t.contains("initial selection"))
                })
            })
            .unwrap_or(false);
        if found_codex_note {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        found_codex_note,
        "codex initial-selection note missing from activity log: {:?}",
        doc["activity"]["completed"]
    );

    // The doc carries BOTH per-group currents (drives the TUI current lines).
    assert_eq!(doc["current_by_group"]["claude"], "claudeacct");
    assert_eq!(doc["current_by_group"]["codex"], "codexacct");
}

// ---------------------------------------------------------------------------
// 9b. Codex provider: Anthropic SSE out of a Responses stream
// ---------------------------------------------------------------------------

fn codex_account(name: &str, token: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        credential: AccountCredential::Codex {
            account_id: format!("acct-{name}"),
            access_token: token.to_string(),
            refresh_token: format!("rt-{name}"),
            expires_at_ms: far_future_ms(),
            last_refresh_ms: None,
        },
    }
}

/// Config whose only account is codex, with both codex endpoints pointed at
/// the mock (the Responses fallback route and the token endpoint).
fn codex_config(mock: &MockUpstream, accounts: Vec<AccountConfig>) -> Config {
    let mut config = Config {
        upstream: mock.base_url(),
        accounts,
        ..Default::default()
    };
    config.codex.upstream = mock.base_url();
    config.codex.token_url = format!("{}/v1/oauth/token", mock.base_url());
    // These tests exercise the codex PROVIDER via the legacy cross-group
    // overflow path (a codex-only pool serving arbitrary models). Routing now
    // defaults ON, which would 404 non-codex models against an empty claude
    // group, so disable it here to keep testing the provider in isolation.
    // The dedicated routing tests (`routing_config`) set enabled=true.
    config.routing.enabled = false;
    config
}

/// A scripted Responses-API SSE stream: text, then one function call, then
/// completion with usage.
const CODEX_RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    r#"data: {"type":"response.created","response":{"id":"resp_e2e"}}"#,
    "\n\n",
    "event: response.output_item.added\n",
    r#"data: {"type":"response.output_item.added","item":{"type":"message","role":"assistant"}}"#,
    "\n\n",
    "event: response.output_text.delta\n",
    r#"data: {"type":"response.output_text.delta","delta":"Let me check."}"#,
    "\n\n",
    "event: response.output_item.done\n",
    r#"data: {"type":"response.output_item.done","item":{"type":"message"}}"#,
    "\n\n",
    "event: response.output_item.added\n",
    r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_w1","name":"get_weather","arguments":""}}"#,
    "\n\n",
    "event: response.function_call_arguments.delta\n",
    r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"city\":\"Seoul\"}"}"#,
    "\n\n",
    "event: response.output_item.done\n",
    r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_w1","name":"get_weather","arguments":"{\"city\":\"Seoul\"}"}}"#,
    "\n\n",
    "event: response.completed\n",
    r#"data: {"type":"response.completed","response":{"id":"resp_e2e","usage":{"input_tokens":42,"output_tokens":11}}}"#,
    "\n\n",
);

/// Split an Anthropic SSE body into `(event_type, data_json)` pairs,
/// asserting every event is well-formed.
fn parse_anthropic_sse(body: &str) -> Vec<(String, serde_json::Value)> {
    body.split("\n\n")
        .filter(|chunk| !chunk.trim().is_empty())
        .map(|chunk| {
            let mut event_type = String::new();
            let mut data = String::new();
            for line in chunk.lines() {
                if let Some(t) = line.strip_prefix("event: ") {
                    event_type = t.to_string();
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = d.to_string();
                } else {
                    panic!("malformed SSE line: {line:?}");
                }
            }
            let value: serde_json::Value = serde_json::from_str(&data).expect("data json");
            assert_eq!(value["type"], event_type, "data.type matches event line");
            (event_type, value)
        })
        .collect()
}

/// C1: a streaming Anthropic request served by a codex account comes back as
/// well-formed Anthropic SSE including a full tool_use round, while the
/// upstream saw a Responses-API request (model pinned to gpt-5.5, codex
/// headers, translated body) — even with the upstream stream fragmented
/// across awkward chunk boundaries.
#[tokio::test]
async fn codex_account_serves_anthropic_stream_with_tool_use() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::sse_plain(CODEX_RESPONSES_SSE, 9));
    let proxy =
        Proxy::spawn_config(codex_config(&mock, vec![codex_account("cx", "at-codex")])).await;

    let client = reqwest::Client::new();
    let request_body = r#"{
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "stream": true,
        "system": "Be helpful.",
        "messages": [{"role": "user", "content": "weather in Seoul?"}],
        "tools": [{"name": "get_weather", "description": "Get weather",
                   "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}]
    }"#;
    let response = post_messages(&client, &proxy, request_body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let body = String::from_utf8(response.bytes().await.expect("body").to_vec()).expect("utf8");
    let events = parse_anthropic_sse(&body);
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        types,
        vec![
            "message_start",
            "content_block_start", // text
            "content_block_delta",
            "content_block_stop",
            "content_block_start", // tool_use
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "full body:\n{body}"
    );
    assert_eq!(events[0].1["message"]["model"], "gpt-5.5");
    assert_eq!(events[2].1["delta"]["text"], "Let me check.");
    assert_eq!(events[4].1["content_block"]["type"], "tool_use");
    assert_eq!(events[4].1["content_block"]["id"], "call_w1");
    assert_eq!(events[4].1["content_block"]["name"], "get_weather");
    assert_eq!(events[4].1["index"], 1);
    assert_eq!(events[5].1["delta"]["partial_json"], "{\"city\":\"Seoul\"}");
    assert_eq!(events[7].1["delta"]["stop_reason"], "tool_use");
    assert_eq!(events[7].1["usage"]["input_tokens"], 42);
    assert_eq!(events[7].1["usage"]["output_tokens"], 11);

    // The upstream saw a translated Responses request with codex headers.
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/responses");
    assert_eq!(seen[0].authorization.as_deref(), Some("Bearer at-codex"));
    assert_eq!(seen[0].chatgpt_account_id.as_deref(), Some("acct-cx"));
    assert_eq!(seen[0].originator.as_deref(), Some("codex_cli_rs"));
    assert_eq!(seen[0].x_api_key, None, "client x-api-key never leaks");
    let upstream_body: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("json");
    assert_eq!(upstream_body["model"], "gpt-5.5", "model always rewritten");
    assert_eq!(upstream_body["instructions"], "Be helpful.");
    assert_eq!(upstream_body["stream"], true);
    assert_eq!(upstream_body["store"], false);
    assert_eq!(upstream_body["tools"][0]["type"], "function");
    assert_eq!(upstream_body["tools"][0]["name"], "get_weather");

    // Converter usage feeds the proxy totals (dashboard keeps working).
    let account = AccountId("cx".into());
    let mut totals = llmux::proxy::server::AccountTotals::default();
    for _ in 0..50 {
        totals = proxy_totals(&proxy, &account).await;
        if totals.requests > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(totals.requests, 1);
    assert_eq!(totals.input_tokens, 42);
    assert_eq!(totals.output_tokens, 11);
}

/// Pull one account's totals out of `/llmux/status`.
async fn proxy_totals(proxy: &Proxy, account: &AccountId) -> llmux::proxy::server::AccountTotals {
    let client = reqwest::Client::new();
    let doc: serde_json::Value = client
        .get(proxy.url("/llmux/status"))
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");
    let entry = doc["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .find(|a| a["name"] == account.0.as_str())
        .cloned()
        .unwrap_or_default();
    llmux::proxy::server::AccountTotals {
        requests: entry["totals"]["requests"].as_u64().unwrap_or(0),
        input_tokens: entry["totals"]["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: entry["totals"]["output_tokens"].as_u64().unwrap_or(0),
    }
}

/// C2: a non-streaming client request on a codex account gets ONE aggregated
/// Anthropic Messages JSON document built from the upstream stream.
#[tokio::test]
async fn codex_account_aggregates_non_streaming_requests() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::sse_plain(CODEX_RESPONSES_SSE, 16));
    let proxy =
        Proxy::spawn_config(codex_config(&mock, vec![codex_account("cx", "at-codex")])).await;

    let client = reqwest::Client::new();
    let response = post_messages(
        &client,
        &proxy,
        r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let message: serde_json::Value = response.json().await.expect("json");
    assert_eq!(message["type"], "message");
    assert_eq!(message["model"], "gpt-5.5");
    assert_eq!(message["stop_reason"], "tool_use");
    assert_eq!(message["content"][0]["type"], "text");
    assert_eq!(message["content"][0]["text"], "Let me check.");
    assert_eq!(message["content"][1]["type"], "tool_use");
    assert_eq!(message["content"][1]["input"]["city"], "Seoul");
    assert_eq!(message["usage"]["input_tokens"], 42);
    assert_eq!(message["usage"]["output_tokens"], 11);
}

/// C3: a codex 401 forces one token refresh (form-encoded grant against the
/// codex token endpoint) and the request retries with the fresh token.
#[tokio::test]
async fn codex_401_refreshes_once_and_retries() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::AuthRejected);
    mock.push(ScriptedResponse::sse_plain(CODEX_RESPONSES_SSE, 32));
    let proxy = Proxy::spawn_config(codex_config(
        &mock,
        vec![codex_account("cx", "at-codex-stale")],
    ))
    .await;

    let client = reqwest::Client::new();
    let response = post_messages(
        &client,
        &proxy,
        r#"{"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(response.status(), 200);
    let body = String::from_utf8(response.bytes().await.expect("body").to_vec()).expect("utf8");
    assert!(
        body.contains("event: message_stop"),
        "stream completed:\n{body}"
    );

    assert_eq!(mock.token_hits(), 1, "exactly one refresh");
    let bearers = mock.seen_bearers();
    assert_eq!(
        bearers,
        vec![
            "Bearer at-codex-stale".to_string(),
            format!("Bearer {}", MockUpstream::REFRESHED_ACCESS_TOKEN),
        ],
        "401 → refresh → retry on the same codex account"
    );

    // Refreshed tokens persisted to the config file (read-merge-write).
    let mut persisted = false;
    for _ in 0..100 {
        let config = config::load_path(&proxy.config_path).expect("reload config");
        if let AccountCredential::Codex { access_token, .. } = &config.accounts[0].credential {
            if access_token == MockUpstream::REFRESHED_ACCESS_TOKEN {
                persisted = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(persisted, "refreshed codex tokens must land in the config");
}

/// C4: codex accounts answer `/v1/messages/count_tokens` locally with an
/// estimate (no upstream call) and refuse other endpoints with a clear 501.
#[tokio::test]
async fn codex_count_tokens_is_estimated_locally() {
    let mock = MockUpstream::spawn().await;
    let proxy =
        Proxy::spawn_config(codex_config(&mock, vec![codex_account("cx", "at-codex")])).await;

    let client = reqwest::Client::new();
    let response = client
        .post(proxy.url("/v1/messages/count_tokens"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m","messages":[{"role":"user","content":"abcdefgh"}]}"#)
        .send()
        .await
        .expect("reachable");
    assert_eq!(response.status(), 200);
    let doc: serde_json::Value = response.json().await.expect("json");
    assert!(
        doc["input_tokens"].as_u64().unwrap_or(0) >= 1,
        "naive estimate present: {doc}"
    );
    assert!(
        mock.seen().is_empty(),
        "count_tokens must not reach the codex upstream"
    );

    let response = client
        .get(proxy.url("/v1/models"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response.status(),
        501,
        "non-messages endpoints are a clear 501"
    );
}

/// The 2026-06-12 live chatgpt.com capture, verbatim event sequence: a
/// reasoning item with encrypted_content and an EMPTY summary (no
/// reasoning_summary_text.delta), a message item tagged phase:"final_answer",
/// obfuscation fields on the text deltas, and the in_progress /
/// content_part.* / output_text.done bookkeeping events.
const CODEX_LIVE_SSE: &str = concat!(
    "event: response.created\n",
    r#"data: {"type":"response.created","response":{"id":"resp_live","object":"response","status":"in_progress","model":"gpt-5.5","output":[],"usage":null}}"#,
    "\n\n",
    "event: response.in_progress\n",
    r#"data: {"type":"response.in_progress","response":{"id":"resp_live","status":"in_progress"}}"#,
    "\n\n",
    "event: response.output_item.added\n",
    r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_live","type":"reasoning","encrypted_content":"gAAAAA-opaque","summary":[]}}"#,
    "\n\n",
    "event: response.output_item.done\n",
    r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_live","type":"reasoning","encrypted_content":"gAAAAA-opaque","summary":[]}}"#,
    "\n\n",
    "event: response.output_item.added\n",
    r#"data: {"type":"response.output_item.added","output_index":1,"item":{"id":"msg_live","type":"message","status":"in_progress","content":[],"phase":"final_answer","role":"assistant"}}"#,
    "\n\n",
    "event: response.content_part.added\n",
    r#"data: {"type":"response.content_part.added","content_index":0,"item_id":"msg_live","output_index":1,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#,
    "\n\n",
    "event: response.output_text.delta\n",
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"O","item_id":"msg_live","logprobs":[],"obfuscation":"ydFpcUg7ZI1oyX","output_index":1}"#,
    "\n\n",
    "event: response.output_text.delta\n",
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"K","item_id":"msg_live","logprobs":[],"obfuscation":"x91js","output_index":1}"#,
    "\n\n",
    "event: response.output_text.delta\n",
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":", ","item_id":"msg_live","logprobs":[],"obfuscation":"p2","output_index":1}"#,
    "\n\n",
    "event: response.output_text.delta\n",
    r#"data: {"type":"response.output_text.delta","content_index":0,"delta":"done","item_id":"msg_live","logprobs":[],"obfuscation":"qq8","output_index":1}"#,
    "\n\n",
    "event: response.output_text.done\n",
    r#"data: {"type":"response.output_text.done","content_index":0,"item_id":"msg_live","logprobs":[],"output_index":1,"text":"OK, done"}"#,
    "\n\n",
    "event: response.content_part.done\n",
    r#"data: {"type":"response.content_part.done","content_index":0,"item_id":"msg_live","output_index":1,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":"OK, done"}}"#,
    "\n\n",
    "event: response.output_item.done\n",
    r#"data: {"type":"response.output_item.done","output_index":1,"item":{"id":"msg_live","type":"message","status":"completed","content":[{"type":"output_text","text":"OK, done"}],"phase":"final_answer","role":"assistant"}}"#,
    "\n\n",
    "event: response.completed\n",
    r#"data: {"type":"response.completed","response":{"id":"resp_live","status":"completed","usage":{"input_tokens":8,"input_tokens_details":{"cached_tokens":0},"output_tokens":5,"total_tokens":13}}}"#,
    "\n\n",
);

fn epoch_secs_in(secs: u64) -> u64 {
    (SystemTime::now() + Duration::from_secs(secs))
        .duration_since(UNIX_EPOCH)
        .expect("future timestamp")
        .as_secs()
}

/// Fetch the codex account entry from `/llmux/status`.
async fn status_account(proxy: &Proxy, name: &str) -> serde_json::Value {
    let doc: serde_json::Value = reqwest::Client::new()
        .get(proxy.url("/llmux/status"))
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("status json");
    doc["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .find(|a| a["name"] == name)
        .cloned()
        .expect("account present in status")
}

/// C5 (regression, live smoke 2026-06-12): the real codex backend sends its
/// streaming 200 with NO content-type header — the proxy must still treat
/// the 2xx as SSE (stream:true is always sent upstream) and convert it,
/// never wrap it into a 502. The x-codex-* quota headers on the same
/// response must populate the account's 5h/7d windows in /llmux/status.
#[tokio::test]
async fn codex_200_without_content_type_streams_and_populates_quota_windows() {
    let mock = MockUpstream::spawn().await;
    let primary_reset = epoch_secs_in(275);
    let secondary_reset = epoch_secs_in(465_379);
    let primary_reset = primary_reset.to_string();
    let secondary_reset = secondary_reset.to_string();
    // Header values from the live capture (used-percent 0 / 2, plan pro).
    mock.push(ScriptedResponse::sse_codex(
        CODEX_LIVE_SSE,
        9,
        &[
            ("x-codex-primary-used-percent", "0"),
            ("x-codex-secondary-used-percent", "2"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-primary-reset-after-seconds", "275"),
            ("x-codex-secondary-reset-after-seconds", "465379"),
            ("x-codex-primary-reset-at", primary_reset.as_str()),
            ("x-codex-secondary-reset-at", secondary_reset.as_str()),
            ("x-codex-plan-type", "pro"),
            ("x-codex-active-limit", "premium"),
        ],
    ));
    let proxy =
        Proxy::spawn_config(codex_config(&mock, vec![codex_account("cx", "at-codex")])).await;

    let client = reqwest::Client::new();
    let response = post_messages(
        &client,
        &proxy,
        r#"{"model":"claude-sonnet-4-6","max_tokens":50,"stream":true,"messages":[{"role":"user","content":"Say OK"}]}"#,
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "no content-type must not become 502"
    );
    let body = String::from_utf8(response.bytes().await.expect("body").to_vec()).expect("utf8");
    let events = parse_anthropic_sse(&body);
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        types,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "full body:\n{body}"
    );
    assert_eq!(events[2].1["delta"]["text"], "O");
    assert_eq!(events[5].1["delta"]["text"], "done");
    assert_eq!(events[7].1["delta"]["stop_reason"], "end_turn");

    // The x-codex-* headers were recorded as the account's windows.
    let account = status_account(&proxy, "cx").await;
    assert_eq!(account["type"], "codex");
    let five = &account["five_hour"];
    assert!(five.is_object(), "5h window populated: {account}");
    assert!((five["utilization"].as_f64().expect("5h util") - 0.0).abs() < 1e-9);
    let five_resets_in = five["resets_in_secs"].as_u64().expect("5h reset");
    assert!(
        (260..=275).contains(&five_resets_in),
        "5h resets_in ~275s, got {five_resets_in}"
    );
    let seven = &account["seven_day"];
    assert!((seven["utilization"].as_f64().expect("7d util") - 0.02).abs() < 1e-9);
    let seven_resets_in = seven["resets_in_secs"].as_u64().expect("7d reset");
    assert!(
        (465_300..=465_379).contains(&seven_resets_in),
        "7d resets_in ~465379s, got {seven_resets_in}"
    );
    assert!(
        account["blocked"].is_null(),
        "0%/2% with old header observations must not block (codex is exempt \
         from the staleness gate): {account}"
    );
}

/// C6: x-codex quota headers feed the real eligibility gates — a secondary
/// (7d) window over the 99% ceiling shows up as a concrete blocking reason
/// for the codex account in /llmux/status.
#[tokio::test]
async fn codex_quota_headers_drive_eligibility_gate_and_blocking_reason() {
    let mock = MockUpstream::spawn().await;
    let primary_reset = epoch_secs_in(275).to_string();
    let secondary_reset = epoch_secs_in(465_379).to_string();
    mock.push(ScriptedResponse::sse_codex(
        CODEX_LIVE_SSE,
        16,
        &[
            ("x-codex-primary-used-percent", "37"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", primary_reset.as_str()),
            ("x-codex-secondary-used-percent", "99.5"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", secondary_reset.as_str()),
        ],
    ));
    let proxy =
        Proxy::spawn_config(codex_config(&mock, vec![codex_account("cx", "at-codex")])).await;

    let client = reqwest::Client::new();
    let response = post_messages(
        &client,
        &proxy,
        r#"{"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(response.status(), 200);
    let _ = response.bytes().await.expect("drain stream");

    let account = status_account(&proxy, "cx").await;
    assert!((account["five_hour"]["utilization"].as_f64().expect("5h") - 0.37).abs() < 1e-9);
    assert!((account["seven_day"]["utilization"].as_f64().expect("7d") - 0.995).abs() < 1e-9);
    let blocked = account["blocked"].as_str().expect("blocked reason");
    assert!(
        blocked.contains("7d") && blocked.contains("99"),
        "real gate reason surfaced, got {blocked:?}"
    );
}

/// C7: a codex 2xx whose body is NOT SSE (plain JSON document) must end as
/// a clean Anthropic error event on the stream — not a hang, not garbage.
#[tokio::test]
async fn codex_json_200_body_terminates_with_clean_error_event() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::Ok {
        body: r#"{"detail":"not an event stream"}"#.to_string(),
        five_hour: None,
        seven_day: None,
    });
    let proxy =
        Proxy::spawn_config(codex_config(&mock, vec![codex_account("cx", "at-codex")])).await;

    let client = reqwest::Client::new();
    let body = tokio::time::timeout(Duration::from_secs(10), async {
        let response = post_messages(
            &client,
            &proxy,
            r#"{"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .await;
        assert_eq!(response.status(), 200, "stream already committed as 200");
        String::from_utf8(response.bytes().await.expect("body").to_vec()).expect("utf8")
    })
    .await
    .expect("must not hang");
    let events = parse_anthropic_sse(&body);
    assert_eq!(events.len(), 1, "exactly one terminal event:\n{body}");
    assert_eq!(events[0].0, "error");
    assert_eq!(events[0].1["error"]["type"], "api_error");
}

// ---------------------------------------------------------------------------
// 9c. Dashboard endpoint: the attach-mode document contract
// ---------------------------------------------------------------------------

/// Fetch `GET /llmux/dashboard` as JSON (optionally with the proxy key).
async fn get_dashboard(proxy: &Proxy, api_key: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new().get(proxy.url("/llmux/dashboard"));
    if let Some(key) = api_key {
        request = request.header("x-api-key", key);
    }
    request.send().await.expect("dashboard reachable")
}

/// The dashboard endpoint serves a status superset: accounts in selection
/// order, the meta fields (version/pid/port/uptime/upstream/config_path),
/// the activity tail (a driven request shows up as completed), the scheduler
/// + poller + totals panes, and the log tail field. The document round-trips
/// into the `DashboardView` the attach client renders from.
#[tokio::test]
async fn dashboard_endpoint_serves_the_attach_document() {
    let mock = MockUpstream::spawn().await;
    // b's 7d window resets sooner → b ranks first; a follows. Selection
    // order in the document must mirror that (current first, then by rank).
    mock.set_usage("at-a", (0.50, 3_600), (0.50, 48 * 3_600));
    mock.set_usage("at-b", (0.50, 3_600), (0.50, 12 * 3_600));
    mock.push(ScriptedResponse::ok_with(
        r#"{"id":"msg_d","type":"message","usage":{"input_tokens":11,"output_tokens":4}}"#,
        (0.20, 3_600),
        (0.20, 12 * 3_600),
    ));
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
    )
    .await;
    assert_eq!(
        proxy.pool.snapshot().legacy_current().cloned(),
        Some(AccountId("b".into())),
        "initial selection ranks by soonest 7d reset"
    );

    // Drive one request so the activity ring + totals are non-empty.
    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, "{}").await;
    assert_eq!(response.status(), 200);

    // Poll the dashboard until the completed request lands (the fold task is
    // async — the event is emitted on the request path and folded slightly
    // later).
    let mut doc = serde_json::Value::Null;
    for _ in 0..100 {
        let response = get_dashboard(&proxy, None).await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        doc = response.json().await.expect("dashboard json");
        let completed = doc["activity"]["completed"].as_array();
        if completed.is_some_and(|c| !c.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Meta fields (status superset).
    assert!(doc["version"]
        .as_str()
        .expect("version")
        .starts_with("llmux "));
    assert!(doc["pid"].as_u64().expect("pid") > 0);
    assert!(doc["port"].as_u64().expect("port") > 0);
    assert!(doc["uptime_secs"].is_u64());
    assert_eq!(doc["upstream"], mock.base_url());
    assert!(
        doc["config_path"].as_str().is_some(),
        "config_path present: {doc}"
    );
    assert_eq!(doc["current"], "b");
    assert!(doc["select_params"]["five_hour_max"].is_number());
    assert!(doc["evaluate_tick_secs"].is_u64());

    // Accounts in selection order (current → rank), status-compatible keys.
    let accounts = doc["accounts"].as_array().expect("accounts array");
    let names: Vec<&str> = accounts
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["b", "a"], "selection order: current b, then a");
    assert_eq!(accounts[0]["order"], 1);
    assert_eq!(accounts[0]["status"], "active");
    assert_eq!(accounts[0]["type"], "oauth");
    assert!(accounts[0]["five_hour"]["resets_in_secs"].is_u64());
    assert!(accounts[0]["session"]["requests"].is_u64());

    // Scheduler / poller / totals panes.
    assert!(doc["scheduler"].is_object());
    assert!(doc["poller"].is_array(), "poller array present: {doc}");
    assert_eq!(doc["totals"]["requests"].as_u64().expect("total req"), 1);
    assert_eq!(doc["totals"]["ok"].as_u64().expect("ok"), 1);
    assert_eq!(doc["totals"]["tokens_in"].as_u64().expect("tok in"), 11);
    assert_eq!(doc["totals"]["tokens_out"].as_u64().expect("tok out"), 4);

    // Activity tail: the driven request is present as a completed request.
    let completed = doc["activity"]["completed"].as_array().expect("completed");
    assert!(
        completed
            .iter()
            .any(|e| e["kind"] == "request" && e["status"] == 200),
        "driven request in the activity tail: {doc}"
    );
    assert!(doc["activity"]["in_flight"].is_array());

    // Log tail field present (oldest→newest array; headless serve has no
    // tracing bridge feeding it, so it may be empty — the shape is the
    // contract, content is unit-tested in dashboard.rs).
    assert!(doc["logs"].is_array(), "log tail present: {doc}");

    // The whole document parses back into the typed `DashboardDoc` the attach
    // client deserializes (and then turns into a `DashboardView` — that
    // conversion is unit-tested in `tui::view`). Selection order and the
    // window reconstruction fields survive the round-trip.
    let parsed: llmux::dashboard::DashboardDoc =
        serde_json::from_value(doc.clone()).expect("doc parses as DashboardDoc");
    assert_eq!(parsed.current.as_deref(), Some("b"));
    let parsed_names: Vec<&str> = parsed.accounts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(parsed_names, vec!["b", "a"]);
    assert!(
        parsed.accounts[0].five_hour.is_some(),
        "window reconstruction fields present after round-trip"
    );
}

// ---------------------------------------------------------------------------
// 9d. Switch endpoint: manual account switch over HTTP
// ---------------------------------------------------------------------------

/// `POST /llmux/switch` switches the current account (the server-side of
/// the dashboard's `s`-key), and is gated by the SAME middleware as
/// `/llmux/status`: a bogus key from a loopback peer is still accepted
/// (loopback is exempt), proving the route sits behind `client_auth` rather
/// than bypassing it.
#[tokio::test]
async fn switch_endpoint_switches_current_account() {
    let mock = MockUpstream::spawn().await;
    // Both eligible; a ranks first (sooner 7d reset) so initial current = a.
    mock.set_usage("at-a", (0.30, 3_600), (0.30, 12 * 3_600));
    mock.set_usage("at-b", (0.30, 3_600), (0.30, 48 * 3_600));
    let proxy = Proxy::spawn(
        &mock.base_url(),
        vec![oauth_account("a", "at-a"), oauth_account("b", "at-b")],
    )
    .await;
    assert_eq!(
        proxy.pool.snapshot().legacy_current().cloned(),
        Some(AccountId("a".into()))
    );

    let client = reqwest::Client::new();
    // Loopback peer with a deliberately wrong key: still accepted (exempt),
    // and the switch commits.
    let response = client
        .post(proxy.url("/llmux/switch"))
        .header("x-api-key", "definitely-not-the-key")
        .json(&serde_json::json!({ "account": "b" }))
        .send()
        .await
        .expect("switch reachable");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.expect("switch json");
    assert_eq!(body["ok"], true);
    assert_eq!(body["current"], "b");
    assert_eq!(
        proxy.pool.snapshot().legacy_current().cloned(),
        Some(AccountId("b".into())),
        "the pool's current account moved to b"
    );

    // A switch to an unknown account is refused with a clear error (not 200).
    let response = client
        .post(proxy.url("/llmux/switch"))
        .json(&serde_json::json!({ "account": "ghost" }))
        .send()
        .await
        .expect("switch reachable");
    assert_eq!(response.status(), 409);
    let body: serde_json::Value = response.json().await.expect("error json");
    assert_eq!(body["error"]["type"], "proxy_error");
    assert_eq!(
        proxy.pool.snapshot().legacy_current().cloned(),
        Some(AccountId("b".into())),
        "a refused switch leaves the current account unchanged"
    );
}

// ---------------------------------------------------------------------------
// 10. Graceful shutdown endpoint
// ---------------------------------------------------------------------------

/// A1: `POST /llmux/shutdown` answers 200 and the server exits — the
/// port stops accepting connections (this is exactly what `llmux stop`
/// polls for).
#[tokio::test]
async fn shutdown_endpoint_stops_the_server() {
    let mock = MockUpstream::spawn().await;
    let proxy = Proxy::spawn(&mock.base_url(), vec![oauth_account("a", "at-a")]).await;

    let client = reqwest::Client::new();
    let response = client
        .post(proxy.url("/llmux/shutdown"))
        .send()
        .await
        .expect("shutdown endpoint reachable");
    assert_eq!(response.status(), 200);

    // Fresh client per probe so a pooled keep-alive connection can't mask
    // the closed listener.
    let mut stopped = false;
    for _ in 0..100 {
        let probe = reqwest::Client::new();
        match probe.get(proxy.url("/llmux/status")).send().await {
            Err(err) if err.is_connect() => {
                stopped = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    assert!(stopped, "port must stop accepting after shutdown");
}

/// req8 + req8.1: `POST /llmux/codex` changes the LIVE request shape — the
/// next codex upstream request carries the new model, `service_tier:"priority"`
/// (the wire value for fast mode), and `reasoning.effort`.
#[tokio::test]
async fn codex_settings_endpoint_changes_the_upstream_request() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::sse_plain(CODEX_RESPONSES_SSE, 9));
    let proxy =
        Proxy::spawn_config(codex_config(&mock, vec![codex_account("cx", "at-codex")])).await;
    let client = reqwest::Client::new();

    // Change codex settings via the control endpoint (loopback-exempt).
    let resp = client
        .post(proxy.url("/llmux/codex"))
        .json(&serde_json::json!({
            "fast": true,
            "default_model": "gpt-5.5-codex",
            "reasoning_effort": "high"
        }))
        .send()
        .await
        .expect("codex endpoint reachable");
    assert_eq!(resp.status(), 200);
    let echoed: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(echoed["fast"], true);
    assert_eq!(echoed["default_model"], "gpt-5.5-codex");
    assert_eq!(echoed["reasoning_effort"], "high");

    // The next codex request reflects the new shape on the wire.
    let body = r#"{"model":"gpt-5.5","max_tokens":16,"stream":true,
        "messages":[{"role":"user","content":"hi"}]}"#;
    let response = post_messages(&client, &proxy, body).await;
    assert_eq!(response.status(), 200);
    let _ = response.bytes().await;

    let sent = mock
        .seen()
        .into_iter()
        .find(|r| r.path.contains("responses"))
        .expect("a codex /responses request was sent");
    let upstream: serde_json::Value = serde_json::from_slice(&sent.body).expect("upstream json");
    assert_eq!(upstream["model"], "gpt-5.5-codex", "model is config-driven");
    assert_eq!(
        upstream["service_tier"], "priority",
        "fast mode sends service_tier=priority"
    );
    assert_eq!(upstream["reasoning"]["effort"], "high");
}

// ---------------------------------------------------------------------------
// 11. Model-aware backend-group routing
// ---------------------------------------------------------------------------

/// Mixed claude+codex config with model routing ENABLED, both codex
/// endpoints pointed at the mock. `on_empty` selects the empty-group policy.
fn routing_config(mock: &MockUpstream, accounts: Vec<AccountConfig>, on_empty: &str) -> Config {
    let mut config = Config {
        upstream: mock.base_url(),
        accounts,
        ..Default::default()
    };
    config.codex.upstream = mock.base_url();
    config.codex.token_url = format!("{}/v1/oauth/token", mock.base_url());
    config.routing.enabled = true;
    config.routing.on_empty_group = on_empty.to_string();
    config
}

/// Routing on: a `{"model":"gpt-5.5"}` request is routed to the CODEX group
/// and served by the codex account — the upstream sees a translated
/// Responses-API request with codex headers + the codex bearer, never the
/// claude account.
#[tokio::test]
async fn gpt_5_5_request_leases_codex_account() {
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::sse_codex(CODEX_LIVE_SSE, 64, &[]));
    let proxy = Proxy::spawn_config(routing_config(
        &mock,
        vec![
            oauth_account("claude-acct", "at-claude"),
            codex_account("codex-acct", "at-codex"),
        ],
        "error",
    ))
    .await;

    let client = reqwest::Client::new();
    let response = post_messages(
        &client,
        &proxy,
        r#"{"model":"gpt-5.5","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(response.status(), 200, "gpt-5.5 routed to a codex account");
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "codex stream translated to Anthropic SSE"
    );
    let _ = response.bytes().await.expect("body");

    // The codex account served it: upstream saw the Responses path, the codex
    // bearer, and codex headers — the claude account was never touched.
    let seen = mock.seen();
    assert_eq!(seen.len(), 1, "exactly one upstream request");
    assert_eq!(seen[0].path, "/responses", "served by the codex provider");
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer at-codex"),
        "leased the CODEX account, not the claude account"
    );
    assert_eq!(
        seen[0].chatgpt_account_id.as_deref(),
        Some("acct-codex-acct")
    );
    let upstream_body: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("json");
    assert_eq!(
        upstream_body["model"], "gpt-5.5",
        "codex provider pins gpt-5.5 upstream"
    );

    // The codex slot is current; the claude slot is independent.
    let snapshot = proxy.pool.snapshot();
    assert_eq!(
        snapshot
            .current_for_group(llmux::routing::BackendGroup::Codex)
            .map(|c| c.0.as_str()),
        Some("codex-acct")
    );
}

/// Routing on: an `{"model":"opus"}` request is routed to the CLAUDE group
/// and served by the oauth account via the Anthropic passthrough — the
/// upstream sees the claude account's bearer, body byte-identical.
#[tokio::test]
async fn opus_request_leases_claude_account() {
    const UPSTREAM_BODY: &str = r#"{"id":"msg_opus","type":"message"}"#;
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::ok(UPSTREAM_BODY));
    let proxy = Proxy::spawn_config(routing_config(
        &mock,
        vec![
            oauth_account("claude-acct", "at-claude"),
            codex_account("codex-acct", "at-codex"),
        ],
        "error",
    ))
    .await;

    let client = reqwest::Client::new();
    let response = post_messages(
        &client,
        &proxy,
        r#"{"model":"opus","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(response.status(), 200);
    let body = response.bytes().await.expect("body");
    assert_eq!(body.as_ref(), UPSTREAM_BODY.as_bytes(), "passthrough relay");

    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/messages", "Anthropic passthrough path");
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer at-claude"),
        "leased the CLAUDE account, not the codex account"
    );

    let snapshot = proxy.pool.snapshot();
    assert_eq!(
        snapshot
            .current_for_group(llmux::routing::BackendGroup::Claude)
            .map(|c| c.0.as_str()),
        Some("claude-acct")
    );
}

/// Routing DISABLED is exactly today's behavior: with a claude + codex
/// account, a `gpt-5.5` request still lands on the anthropic (claude) account
/// — codex stays the cross-group overflow pool (cold codex ranks last), so
/// the model string is irrelevant and the request is a plain passthrough.
#[tokio::test]
async fn routing_disabled_preserves_overflow_behavior() {
    const UPSTREAM_BODY: &str = r#"{"id":"msg_legacy","type":"message"}"#;
    let mock = MockUpstream::spawn().await;
    mock.push(ScriptedResponse::ok(UPSTREAM_BODY));
    // codex_config explicitly disables routing (the legacy overflow path).
    let proxy = Proxy::spawn_config(codex_config(
        &mock,
        vec![
            oauth_account("claude-acct", "at-claude"),
            codex_account("codex-acct", "at-codex"),
        ],
    ))
    .await;

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, r#"{"model":"gpt-5.5"}"#).await;
    assert_eq!(response.status(), 200);
    let body = response.bytes().await.expect("body");
    assert_eq!(body.as_ref(), UPSTREAM_BODY.as_bytes());

    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].path, "/v1/messages",
        "no routing → Anthropic passthrough, NOT /responses"
    );
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer at-claude"),
        "overflow behavior: gpt-5.5 lands on the anthropic account, codex is overflow"
    );
}

/// Routing on + `on_empty_group="error"`: a `gpt-5.5` request when only a
/// claude account is configured returns a clean Anthropic 404 not_found_error
/// and NEVER touches the claude account.
#[tokio::test]
async fn empty_codex_group_errors() {
    let mock = MockUpstream::spawn().await;
    let proxy = Proxy::spawn_config(routing_config(
        &mock,
        vec![oauth_account("claude-acct", "at-claude")],
        "error",
    ))
    .await;

    let client = reqwest::Client::new();
    let response = post_messages(&client, &proxy, r#"{"model":"gpt-5.5"}"#).await;
    assert_eq!(response.status(), 404, "empty codex group → 404");
    let value: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.expect("body")).expect("json");
    assert_eq!(value["error"]["type"], "not_found_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .expect("message")
            .contains("codex"),
        "message names the missing group"
    );

    // The claude account was never leased.
    assert!(
        mock.seen().is_empty(),
        "claude account untouched by an empty-codex-group request"
    );
}

// ---------------------------------------------------------------------------
// 12. Brew install (manual)
// ---------------------------------------------------------------------------

/// Acceptance #9: `brew install 2lab-ai/tap/llmux-preview` installs a
/// release-workflow binary that runs. This requires the published tap +
/// GitHub release artifacts — it is the dispatcher's manual verification
/// step, not an in-repo test (kept `#[ignore]`d so the suite documents it).
#[tokio::test]
#[ignore = "manual: requires the published homebrew tap + release artifacts"]
async fn brew_installed_binary_runs() {
    unreachable!("run manually: brew install 2lab-ai/tap/llmux-preview && llmux --version")
}
