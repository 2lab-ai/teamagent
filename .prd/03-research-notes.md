# llmux — Research notes (2026-06-12 → 2026-06-13)

Distilled inputs behind the shipped Rust v0.1.0 implementation. Original research streams: teamclaude
compatibility audit, soma-work rotation audit, herdr convention audit, and a 20-source verified web survey
(102-agent deep-research run; 25/25 claims survived 3-vote adversarial verification). Dogfooding on
2026-06-13 added live backend findings for Codex, token refresh, dashboard attach mode, and brew
release mechanics.

Historical note: llmux started from teamclaude proxy/OAuth mechanics, but this document describes the current Rust implementation and its import compatibility surface.

## Landscape

Two project families, not combined in one tool before llmux:

| Family | Projects | What they prove |
|---|---|---|
| Multi-account schedulers | teamclaude (Node, npm), ccflare (TS/Bun, git-only), claude-relay-service (unassessed), CC-Router | Header-driven dual-window scheduling works; field believes per-request rotation is ban-risky |
| Provider gateways | claude-code-router, LiteLLM `/v1/messages`, y-router (archived), ollama Anthropic compatibility, ChatMock, CLIProxyAPI | `ANTHROPIC_BASE_URL` is enough for Claude Code; transformer/hourglass adapters can make non-Anthropic backends usable |

No surveyed competitor shipped both a quota-aware subscription scheduler and a brew-installable
single-user proxy with a terminal dashboard.

## Quota signals (verified)

- **Claude Max subscription**: undocumented `anthropic-ratelimit-unified-5h-*` / `-7d-*`
  response headers (`utilization` 0–1, `reset` epoch-seconds, `status`
  allowed|allowed_warning|rejected). Fixed windows, not token buckets. Corroborated by
  claude-code GitHub issues.
- **Claude API keys**: documented `anthropic-ratelimit-{requests,tokens,...}-{limit,remaining,reset}`
  (reset = RFC3339). Token bucket with continuous replenishment.
- **429**: names the exceeded limit, carries `retry-after` seconds; earlier retries will fail.
- **Active Claude source**: `GET /api/oauth/usage` (Bearer) returns five_hour/seven_day
  `{utilization, resets_at}`. Polling covers idle accounts that produce no headers.
- **Codex / ChatGPT subscription** (live 2026-06-13): `chatgpt.com/backend-api/codex/responses`
  returns `x-codex-primary-*` and `x-codex-secondary-*` headers:
  - primary window minutes = `300` → 5h slot;
  - secondary window minutes = `10080` → 7d slot;
  - used-percent is 0–100;
  - reset-at is epoch seconds;
  - plan metadata includes `x-codex-plan-type` and `x-codex-active-limit`.
  The live backend omitted `content-type` on 200 streaming responses, so stream handling must rely
  on the request contract (`stream:true`), not the header.

## soma-work scheduler (the algorithm we ported)

`src/oauth/auto-rotate.ts` (+ token-manager.ts, 2.1k lines):
- Eligibility: healthy, no cooldown, fresh usage (≤2× poll interval), 5h ≤ 90%, 7d ≤ 99%.
- Rank: **min 7d resets_at** (exhaust before reset wastes it) → min 5h utilization → stable id.
- Commit: CAS on expected-current-account; abort if in-flight leases exist; re-validate target.
- 429: cooldown (default 60 min or parsed reset), `rateLimitSource` recorded; fresh usage showing
  capacity clears stale cooldowns.
- Refresh: proactive 7h-before-expiry; failure taxonomy 401→refresh_failed, 403→revoked,
  429/5xx/network→stay healthy and retry next tick.

v0.1 adapts this to Rust: request-time refresh + background daemon refresh. Dogfood proved the
importance of the daemon: idle tokens that would otherwise expire were refreshed back to ~8h and
surfaced in the dashboard (`7h53m ↻6m`).

## teamclaude mechanics (what the port kept)

- Proxy rewrite rules, hop-by-hop strips, accept-encoding drop, `/v1/oauth/token` raw relay.
- 429 retry-same-account with retry-after wait; transient-vs-persistent error split
  (transient → destroy connection, let client retry; persistent → mark error + switch).
- PKCE OAuth constants, profile-based dedup by accountUuid.
- Config read-merge-write for server/CLI concurrency; 0600 perms; random proxy api_key with
  localhost exemption.
- CLI surface (`server`, `run`, `login`, `import`, `env`, `status`, `accounts`, `remove`, `api`).

## ccflare lesson

Sticky sessions: pick an account and stay on it for the 5h window; rotate only on
threshold/expiry/429. Their docs assert per-request switching can trigger anti-abuse bans; unverified
by Anthropic, but it is the field's only operational signal. llmux = stickiness + window-aware
ranking + manual override.

## herdr lesson (2026-06-13 dogfood)

Foreground `server` is not enough once `run` auto-starts a daemon: the daemon owns the port, so a
second `llmux server` must not bind and crash. Herdr's pattern is the right one:
- probe first;
- if server exists, attach as a client;
- if foreign process owns the port, print a clean one-line error;
- initialize TUI only after bind/probe decisions are settled.

Implemented in v0.1 as `llmux dashboard` + `GET /llmux/dashboard` + shared
`DashboardView`. This prevents the old failure mode where a bind error painted over a partially
initialized TUI.

## Provider translation lessons

### claude-code-router / transformer hourglass

Transformer interfaces confirm the abstraction: provider auth and format conversion belong in the
same layer. For v0.1, the trait remains conservative; the Codex path adds an explicit streaming
transform seam while Anthropic passthrough stays byte-identical.

### ollama Anthropic compatibility

Ollama's `anthropic/anthropic.go` is the useful reference for content-block semantics and SSE event
state machines: text, images, tool_use, tool_result, thinking, and block-index ordering. User
feedback on 2026-06-13 confirmed that naming a reference implementation ("ollama 참고") means the
mapping must be ported, not hand-waved as a minimal transport adapter.

### ChatMock / CLIProxyAPI / Codex backend

Two OSS implementations plus local CLI inspection showed that Codex subscription tokens can call:

```text
POST https://chatgpt.com/backend-api/codex/responses
Authorization: Bearer <access_token>
Chatgpt-Account-Id: <account_id>
OpenAI-Beta: responses=experimental
originator: codex_cli_rs
Accept: text/event-stream
```

`~/.codex/auth.json` contains `tokens.access_token`, `refresh_token`, `account_id`, and can be
refreshed via `https://auth.openai.com/oauth/token` with client id
`app_EMoamEEZ73f0CkXaXp7hrann`.

### Live Codex findings (2026-06-13)

1. The backend accepts arbitrary `instructions`, including a realistic ~27KB Claude Code system
   prompt.
2. The backend rejects any Responses input item with `role:"system"`:
   `400 {"detail":"System messages are not allowed"}`.
3. Claude Code interactive sessions can emit mid-conversation `messages[]` entries with
   `role:"system"` (e.g. `<system-reminder>` / operator channel), not only top-level `system`.
4. Therefore the translator must fold **both** top-level system and message-level system into
   `instructions`, preserving order, and must never emit a Codex input item with `role:"system"`.
5. `role:"developer"` is accepted, but llmux only emits it when the client sent developer;
   unknown roles degrade to user.
6. Tool round-trips (`tool_use` → `function_call`; `tool_result` → `function_call_output`) work.
7. A live request through the stable build returned Anthropic SSE `message_start.model: gpt-5.5`,
   proving the end-to-end path.

## Release / distribution notes

- Preview releases are generated on main pushes as `preview-<date>-<sha>` and rendered into
  `Formula/llmux-preview.rb`.
- Stable releases are tag-driven. The release workflow rejects a tag if Cargo.toml's version does
  not match. Because Cargo.toml is `0.1.0`, the first stable llmux tag is
  `v0.1.0`; the old `v1.0.1` tag belongs to the historical teamclaude tree.
- `v0.1.0` released from `f99573f`, with 4 binaries and SHA256SUMS. `homebrew-tap` generated
  `Formula/llmux.rb`; local stable install and `brew test` passed.

## Risk register

1. Unified Anthropic headers are undocumented → may break silently. Fallback chain: usage endpoint
   → 429/retry-after.
2. Codex backend is not a public API. The provider mimics Codex CLI headers and is personal-use
   only; backend shape can change without notice.
3. Per-request rotation ban risk is unverified but acted on by ccflare; llmux avoids it.
4. ToS gray zones: multi-account Claude proxying and Codex subscription token use outside the
   official CLI. Single user's own accounts only, no pooling, no resale.
5. Active usage polling itself can 429. The dashboard must show poller health and not hide stale
   evidence.

## Open questions carried forward

- Header stability across Claude plan tiers and Codex plan tiers.
- Whether Codex's `x-codex-*` semantics differ by model or plan.
- Whether Codex reasoning encrypted content should be round-tripped for longer sessions beyond v0.1.
- Whether `llmux dashboard` should support add/remove/reload against the daemon (v0.1 attach
  intentionally disables config mutation keys).
