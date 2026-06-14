# llmux — Architecture

Rust, edition 2021. Single binary, tokio multi-thread runtime. Module layout follows herdr's
state/runtime separation: scheduler decisions are pure functions over snapshots, runtime state is
folded into dashboard documents, and provider-specific format conversion is isolated from the
Anthropic passthrough fast path.

## Crate layout

```text
src/
  main.rs              # entry, tokio runtime, command dispatch
  cli/                 # clap commands + command impls
    mod.rs             # server/dashboard/run/stop dispatch, daemon attach decision
    daemon.rs          # probe/spawn/wait/stop helpers (herdr-style client/server)
    login.rs import.rs run.rs status.rs accounts.rs api.rs env.rs
  config/              # ~/.config/llmux.json load/save, atomic read-merge-write
    mod.rs schema.rs migrate.rs
  auth/
    oauth.rs           # Claude PKCE flow, token exchange, refresh (coalesced)
    codex.rs           # Codex auth.json import + OpenAI token refresh
    profile.rs         # /api/oauth/profile client (email, uuid, tier)
    credentials.rs     # ~/.claude/.credentials.json import
  scheduler/
    mod.rs             # AccountPool: owns state, applies events, leases
    select.rs          # PURE: eligibility + ranking + selection_order + blocking_reason
    window.rs          # QuotaWindow {utilization, resets_at, fetched_at, source}
    headers.rs         # Anthropic unified + Codex x-codex-* header parsing
    usage.rs           # /api/oauth/usage poller (Claude OAuth only, backoff ladder)
  proxy/
    server.rs          # axum listener, /llmux/* control endpoints, background tasks
    forward.rs         # request rewrite, provider dispatch, retry taxonomy, refresh choke point
    sse.rs             # passthrough + transform relay; SseTransform trait
    logging.rs         # optional request logs, credential masking
  provider/
    mod.rs             # Provider trait + UnifiedRequest/Response types
    anthropic.rs       # passthrough impl (identity hooks, zero-copy fast path)
    codex.rs           # Anthropic Messages <-> OpenAI Responses translation + SSE converter
    stubs.rs           # gemini/local compile-checked drafts
  dashboard.rs         # DashboardHub + DashboardDoc (/llmux/dashboard contract)
  tui/
    mod.rs             # local + remote dashboard loops, attach client
    view.rs            # DashboardView: single render input from live state or DashboardDoc
    ui.rs              # ratatui renderer, no fork between local/attach
    activity.rs logs.rs format.rs event.rs
  build_info.rs        # channel + build id from env (herdr pattern)
tests/
  e2e.rs               # mock upstream + proxy acceptance scenarios
  mock_upstream.rs     # Anthropic/Codex simulators (headers, 429, SSE)
```

## Runtime topology

```text
Claude Code
  │ ANTHROPIC_BASE_URL=http://localhost:3456
  ▼
llmux daemon (axum)
  ├─ AccountPool (scheduler state, windows, leases)
  ├─ Provider dispatch
  │   ├─ AnthropicPassthrough → https://api.anthropic.com
  │   └─ CodexProvider       → https://chatgpt.com/backend-api/codex/responses
  ├─ Background tasks
  │   ├─ usage poller (Claude OAuth accounts)
  │   ├─ token refresh pass (< refresh_ahead_secs remaining)
  │   ├─ scheduler re-evaluation tick
  │   └─ DashboardHub fold (activity/log/poller/switch state)
  └─ Control API
      ├─ GET  /llmux/status
      ├─ GET  /llmux/dashboard
      ├─ POST /llmux/switch
      └─ POST /llmux/shutdown
```

`llmux run` is a client command: it probes `/llmux/status`, spawns a detached daemon with
`server --no-tui` if none is running, waits until ready, then launches `claude` with
`ANTHROPIC_BASE_URL`. `llmux dashboard` is an attach client: it polls `/llmux/dashboard`
and renders the same ratatui layout without binding the proxy port.

## Concurrency model

- `AccountPool` is behind `Arc<RwLock<PoolState>>`; mutations go through event methods
  (`record_headers`, `record_429`, `record_usage`, `switch_to`) that re-validate preconditions
  before applying.
- In-flight requests hold an `AccountLease` (Drop-based guard incrementing/decrementing a
  per-account counter). Switching away never cancels leased requests; the lease pins the
  credential clone for the request lifetime.
- Claude OAuth refresh uses `RefreshCoalescer`: concurrent refresh callers for the same account
  await the same outcome.
- Codex refresh uses the OpenAI refresh-token grant; it is not coalesced in v0.1 because it is
  rare and idempotent enough for the single-user daemon.
- Config writes are read-merge-write and atomic. Refresh updates include `last_refresh_ms` so the
  UI can prove the daemon is actually maintaining credentials.
- DashboardHub is the single fold target for activity/log/poller/switch events. Both local TUI and
  remote attach render from `DashboardView`, so layout logic is not duplicated.

## Scheduler data flow

```text
Anthropic response headers ──┐
Codex x-codex-* headers ─────┼──> headers.rs ─────┐
/api/oauth/usage poll ───────┘                     ├──> PoolState.windows
429 + retry-after ─────────────────> forward.rs ───┘          │
                                                               ▼
                         select.rs::pick(snapshot, now) — pure, deterministic
                         1 gates: health, cooldown, thresholds, staleness
                         2 stickiness + perishability override (SWITCH_MARGIN)
                         3 rank: max score (servable×urgency) → min 5h → min 7d reset → id
                                                               │
                         switch_to(expected_current, target)  # CAS-ish, lease guard
```

Two evidence sources feed the same Claude windows; freshest `fetched_at` wins per window. Headers
are authoritative during traffic; the poller covers idle Claude OAuth accounts. If usage data is
stale, a Claude account is ineligible unless all accounts are stale (headers-only fallback). Codex
has no usage poller, so staleness does not gate it; quota thresholds still gate Codex when
`x-codex-*` header evidence exists.

## Provider dispatch and request flow

1. Buffer incoming request body and create an activity item.
2. Acquire an `AccountLease`. When `routing.enabled` (see `routing.rs`), the request's
   `model` first selects a backend **group** (claude vs codex; the model field — previously
   only carried through `UnifiedRequest` as a future routing key — now drives selection), the
   scheduler is filtered to that group, and the lease is sticky per group. The leased
   credential then determines the provider:
   - `oauth` / `apikey` → `AnthropicPassthrough`.
   - `codex` → `CodexProvider`.

   Routing is **on by default**, so the `model` normally selects the group. With routing disabled
   no group filter is applied: a single legacy current slot is used and codex becomes the
   cross-group overflow pool — the older behavior.
3. Refresh credential if near expiry; on one 401, force refresh and retry once.
4. Build provider request:
   - Anthropic: identity body, inject Bearer or x-api-key.
   - Codex: translate Anthropic Messages to OpenAI Responses JSON, inject Codex OAuth headers.
5. Send upstream, classify response, and retry/switch according to taxonomy.
6. Relay response:
   - Anthropic: byte-identity SSE/body relay; usage observed from emitted Anthropic SSE.
   - Codex: Responses SSE transform relay; converter emits Anthropic SSE and usage accounting sees
     the emitted events.
7. Finish activity, record totals, update DashboardHub.

## Error taxonomy (forward.rs)

| Upstream signal | Action |
|---|---|
| 429 + retry-after | Park that account. If short, wait and retry same account; if long, switch and retry request. |
| 401 on refreshable account | Force one refresh, retry; second 401 marks auth_failed and switches. |
| 5xx / connect reset / timeout | Transient: return 502/close so client retries. |
| Persistent provider error | Mark account error or return provider-shaped error, depending on retryability. |
| Codex non-2xx | Wrap body as Anthropic error event/body; never relay raw Codex JSON to Claude Code. |
| Codex 2xx stream without content-type | Treat as SSE by contract. The live backend omits `content-type`; malformed streams terminate with Anthropic `error`. |

## Config schema (v1)

```jsonc
{
  "version": 1,
  "proxy": { "port": 3456, "api_key": "ta-..." },
  "upstream": "https://api.anthropic.com",
  "codex": {
    "upstream": "https://chatgpt.com/backend-api/codex",
    "token_url": "https://auth.openai.com/oauth/token",
    "default_model": "gpt-5.5",
    "fast": false
  },
  "scheduler": {
    "five_hour_max": 0.90,
    "seven_day_max": 0.99,
    "usage_poll_secs": 300,
    "usage_max_age_secs": 600,
    "refresh_ahead_secs": 25200
  },
  "routing": {            // model→backend-group routing; all keys default-able
    "enabled": true,     // default; false = Codex-as-overflow (no group filter)
    "claude_models": [],  // empty = builtin rules; non-empty replaces them
    "codex_models": [],
    "default_group": "claude",   // unmatched / model-less request lands here
    "on_empty_group": "error"    // "error" = 404 not_found_error; "fallback" = other group
  },
  "accounts": [
    { "name": "a@x.com", "type": "oauth", "account_uuid": "...",
      "access_token": "...", "refresh_token": "...",
      "expires_at_ms": 0, "last_refresh_ms": 0 },
    { "name": "api-1", "type": "apikey", "api_key": "..." },
    { "name": "chatgpt@example.com", "type": "codex", "account_id": "...",
      "access_token": "...", "refresh_token": "...",
      "expires_at_ms": 0, "last_refresh_ms": 0 }
  ]
}
```

`migrate.rs` reads teamclaude's `~/.config/teamclaude.json`; `credentials.rs` reads
`~/.claude/.credentials.json`; `auth/codex.rs` reads Codex CLI `~/.codex/auth.json`.

## Codex translation details

The Codex endpoint is OpenAI Responses-shaped but rejects `role:"system"` input items. The
translator therefore:
- folds top-level Anthropic `system` and any message-level system messages into `instructions`;
- maps legal input roles to `assistant`, `developer`, or `user` (never `system`);
- maps Anthropic text blocks to `input_text`/`output_text`;
- maps `tool_use` to `function_call` and `tool_result` to `function_call_output`;
- drops request-side images/thinking in v0.1 with warnings;
- sends `codex.default_model` (default `gpt-5.5`), optional `service_tier`/`reasoning.effort`,
  `stream: true`, `store: false`, and a stable `prompt_cache_key`.

The response converter is a state machine over Responses SSE events:
- `response.created` → Anthropic `message_start`;
- text deltas → `content_block_start` + `content_block_delta` + `content_block_stop`;
- reasoning summary deltas → thinking blocks;
- function call items/argument deltas → `tool_use` + `input_json_delta`;
- `response.completed` → `message_delta` + `message_stop`;
- `response.failed`/malformed stream → Anthropic `error`.

## Control-plane auth

Control endpoints share the status endpoint's gate: loopback clients are exempt; non-loopback
clients need the generated proxy API key. This preserves local UX while avoiding unauthenticated
remote control if the user binds beyond localhost.

## Key dependencies

tokio, axum (server) + reqwest (upstream/streaming), serde/serde_json, clap, ratatui + crossterm,
tracing + tracing-subscriber, sha2/base64 (PKCE/JWT payload decode), thiserror, ulid, uuid, libc.

## Porting pitfalls now codified

- SSE events fragment across chunks; both passthrough observer and Codex transform buffer correctly.
- Anthropic reset vs Codex reset timestamps differ; parse per source.
- Do not require `content-type: text/event-stream` for Codex 2xx; live backend omits it.
- Never emit `role:"system"` in Codex input.
- Mask credentials in logs; request logging is opt-in and capped.
- TTY detection: bind/probe happens before TUI init so bind errors never corrupt the terminal.
- Config writes must preserve concurrently refreshed tokens.
