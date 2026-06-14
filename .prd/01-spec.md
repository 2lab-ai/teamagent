# llmux — Spec

Multi-account, multi-provider LLM proxy for Claude Code, implemented in Rust with a quota-expiry-aware scheduler, a persistent daemon, a rich terminal dashboard, and an experimental OpenAI Codex provider.

Historical note: proxy/OAuth mechanics began from [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude) (MIT), while the current shipped implementation is Rust.

Status: **shipped** (current `0.2.x`; Homebrew formula `2lab-ai/tap/llmux`).
This document records the currently implemented contract, not a future target.

## Problem

One Claude Max subscription caps out (5h session window, 7d weekly window). Users with multiple
subscriptions burn time manually switching accounts, and naive rotation wastes quota: windows
reset on fixed timestamps, so quota left unspent when a window resets is lost forever.

A second operational problem appeared during dogfooding: Claude Code sessions are long-lived, but
subscription OAuth access tokens expire around 8h. If the proxy is not kept alive and refreshing
idle accounts, users are forced to re-login. Therefore v0.1 is a **daemon-first** local proxy, not
only a foreground process.

## Goals (implemented)

1. **Drop-in proxy for Claude Code** — `ANTHROPIC_BASE_URL=http://localhost:<port>` is the whole
   integration contract. Claude Code works unmodified.
2. **Quota-maximizing scheduling, not plain rotation** — exploit window expiry ("토큰의 유통기한"):
   score each account by *usable burst now × weekly-quota perishability*, so quota that resets
   soon (and would otherwise be lost) is burned first while long-runway accounts are preserved.
3. **Session stickiness with a perishability override** — stay on the current account while
   eligible, switching only on threshold/expiry/429/manual switch, or when another account is
   worth clearly more (ban-risk + prompt-cache-locality mitigation; see Risks). Never per-request.
4. **Daemon-first operation** — `llmux run` auto-starts a detached server when needed; the
   server keeps polling/refreshing tokens even when no dashboard is attached.
5. **Observable control surface** — `llmux status` is herdr-style, and `llmux dashboard`
   attaches to an already-running daemon instead of attempting to bind the port again.
6. **Provider abstraction with a working Codex provider** — Anthropic passthrough remains the fast
   identity path; OpenAI Codex (ChatGPT subscription via `llmux login --codex` or
   `~/.codex/auth.json`) is selected by the request's `model` under default model→group routing,
   with a config-driven upstream model (`codex.default_model`, default `gpt-5.5`). Gemini/local
   remain compile-checked stubs.
7. **brew-installable stable + preview** — `brew install 2lab-ai/tap/llmux` (stable) and
   `brew install 2lab-ai/tap/llmux-preview` (rolling preview).

## Non-goals (v0.1)

- No hosted/multi-user deployment. Localhost, single human, their own accounts only.
- No analytics database or browser dashboard. The dashboard is terminal-native ratatui.
- No request-content routing (claude-code-router task-type routing). Manual switch + scheduler only.
- No production Gemini/local backends. Stub providers only.
- Codex provider v0.1 does not support images; `tool_choice` is ignored; non-`/v1/messages`
  endpoints are limited (`count_tokens` is an estimate, others return clear 501).

## Functional requirements

### FR1 — Proxy core
- HTTP server on configurable port (default **3456**, teamclaude-compatible).
- Forward Anthropic-shaped requests to a selected account/provider, rewriting auth:
  strip client `x-api-key`/`authorization`, inject selected account credential.
- Strip hop-by-hop headers; drop `accept-encoding` (avoid decompression mismatch).
- SSE streaming with backpressure and client-disconnect detection.
  - Anthropic passthrough path is byte-identity and observes usage only.
  - Codex path transforms OpenAI Responses SSE into Anthropic Messages SSE.
- `POST /v1/oauth/token` is relayed raw (Claude Code's own token refresh passes through).
- Control endpoints:
  - `GET /llmux/status` — compact scheduler/account JSON.
  - `GET /llmux/dashboard` — superset document for attach-mode TUI.
  - `POST /llmux/switch` — manual account switch.
  - `POST /llmux/shutdown` — graceful daemon stop.
- Optional per-request file logging with credential masking.

### FR2 — Account model
- Account types: `oauth` (Claude subscription), `apikey` (Anthropic API key), and `codex`
  (OpenAI Codex / ChatGPT subscription token from `~/.codex/auth.json`).
- Sources: PKCE OAuth login (browser flow), API-key login, import from
  `~/.claude/.credentials.json`, import from teamclaude config, import from Codex auth.json,
  inline JSON.
- Dedup by `account_uuid` for Claude OAuth, `account_id` for Codex, or name for API keys.
- Config at `~/.config/llmux.json` (`$LLMUX_CONFIG` override), mode 0600,
  atomic read-merge-write so server and CLI may write concurrently.
- OAuth/Codex refresh:
  - Request-time refresh when near expiry or after one 401.
  - Background daemon refresh when remaining lifetime drops below `scheduler.refresh_ahead_secs`
    (default 7h), so idle subscriptions do not silently expire.
  - `last_refresh_ms` and `expires_at_ms` are persisted and surfaced in status/dashboard.

### FR3 — Scheduler (the differentiator)
Per-account state, two quota windows each (5h session, 7d weekly):
- **Passive tracking**: parse Anthropic `anthropic-ratelimit-unified-*` headers and Codex
  `x-codex-{primary,secondary}-*` headers from upstream responses.
- **Active tracking (Claude OAuth accounts)**: poll `GET /api/oauth/usage` per account on an
  interval (default 5 min, backoff ladder on failure) so idle Claude accounts have fresh state.
- Window state expires by wall clock: when a reset timestamp passes, the window reads as empty.

Selection algorithm (pure over a snapshot; re-evaluated on ineligibility and a 60s tick):
1. Eligibility: healthy auth, not cooling down (429 park), 5h utilization ≤ `five_hour_max`
   (default 0.90), 7d utilization ≤ `seven_day_max` (default 0.99), usage data not stale.
   Codex is exempt from usage-staleness because it has no poller; its 5h/7d gates still apply if
   `x-codex-*` headers have been observed.
2. **Score = `servable_now × urgency`**, ranked descending. `servable_now = min(5h headroom,
   7d headroom)`; `urgency` ramps linearly across the 7d window (capped 4×) so an account whose
   weekly quota resets soon — and is still usable — outranks a long-runway one. Tiebreaks: lower
   5h utilization, then soonest 7d reset, then stable id. (Full derivation:
   `.prd/09-scheduler-perishability.md`.)
3. **Stickiness with override**: stay on the eligible current unless another account's score beats
   it by `SWITCH_MARGIN` (25%), then switch to burn the perishable quota.
4. **Backend grouping**: with `routing.enabled` (default) only same-group accounts compete, each
   group keeps its own sticky pick; with routing off, Codex accounts rank last as a cross-group
   overflow pool.
5. On 429: honor `retry-after`; persistent failure → cooldown + switch.
6. All accounts exhausted → respond 429 with the soonest reset as `retry-after`.
7. Cooldowns self-heal when fresh usage data shows capacity.
8. Never switch accounts mid-stream; in-flight requests pin their account.

### FR4 — Providers

#### Anthropic passthrough
Identity provider. Request/response bodies are not rewritten; only auth/header handling and usage
observation happen.

#### OpenAI Codex provider (working)
- Added via `llmux login --codex` (ChatGPT OAuth) or imported from Codex CLI credentials
  (`~/.codex/auth.json`), using ChatGPT OAuth tokens.
- Upstream: `POST https://chatgpt.com/backend-api/codex/responses`.
- Upstream model is `codex.default_model` (default **`gpt-5.5`**), with optional `fast`
  (`service_tier: "priority"`) and `reasoning_effort` — all settable live from the dashboard.
- Translates Anthropic Messages requests to OpenAI Responses input:
  - Top-level `system` and message-level `role:"system"` are folded into `instructions`; Codex
    rejects `role:"system"` input items.
  - `tool_use` ↔ `function_call`, `tool_result` ↔ `function_call_output`.
  - Responses SSE is transformed back into Anthropic SSE (`message_start`, text/thinking/tool
    blocks, `message_delta`, `message_stop`).
- Parses `x-codex-primary/secondary-*` quota headers into the same 5h/7d windows.
- Refreshes tokens via `auth.openai.com/oauth/token` and persists them.

### FR5 — CLI
`llmux <cmd>`:
- `server` — foreground server; TUI when TTY, plain logs otherwise. If a daemon already runs, it
  attaches instead of attempting to bind the port.
- `dashboard` — attach-mode dashboard client; polls the daemon and renders the same layout.
- `run [--force] [-- args]` — ensure daemon is running, then spawn `claude` with
  `ANTHROPIC_BASE_URL`; `--force` restarts a same-version daemon.
- `stop` — graceful daemon shutdown. `restart` — drain and respawn from this binary.
- `login [--api|--codex]`, `import [--from PATH|--json J]`, `env`, `status`, `accounts [-v]`,
  `remove <name>`, `api <path>`.

### FR6 — TUI / dashboard
- Rich ratatui dashboard: account table in actual selection order; quota bars; reset countdown +
  local reset time; token expiry + last-refresh marker (`7h53m ↻6m`); per-account in-flight and
  totals; scheduler pane; poller health; request/min; activity and log panes.
- Attach mode: `llmux dashboard` displays `attached → pid N`; `q` exits client only;
  `d` toggles detail; `s` + arrows + Enter switches account through `POST /llmux/switch`.
  Config mutation keys (`a`, `r`, `R`) are local/server-TUI only.

## Distribution & release

- `justfile`: `check` (fmt + clippy -D warnings + tests), `build`.
- CI: `ci.yml` (gate), `preview.yml` (prerelease tag `preview-<date>-<sha>`, 4 binaries),
  `release.yml` (stable tag `v*`; Cargo.toml version must match tag).
- Tap: `2lab-ai/homebrew-tap` renders `llmux-preview.rb` and `llmux.rb` from templates.
- `--version` reports `llmux <semver> (<channel> <build-id>)`, e.g. `(stable v0.2.1-…)`.

## Acceptance (verified)

Against mock upstreams and live dogfood:
1. Anthropic passthrough request returns byte-identical body; auth rewritten.
2. SSE stream passes through intact under chunk fragmentation.
3. Threshold crossing and 429s trigger switch without interrupting in-flight requests.
4. Scheduler picks the highest-scoring (perishability-weighted) eligible account, burning
   soon-to-reset quota first and proactively switching off a long-runway current.
5. Expired access token refreshes once/coalesces and persists.
6. Background refresh renews idle tokens before expiry and surfaces `last_refresh_ms`.
7. Imports: teamclaude, `~/.claude/.credentials.json`, Codex `~/.codex/auth.json`.
8. Codex provider serves real Claude Code traffic; `message_start.model` reports `gpt-5.5`.
9. Codex mid-conversation system messages are folded into instructions, avoiding
   `400 System messages are not allowed`.
10. Dashboard endpoint + attach-mode TUI share one view-model and support manual switching.
11. Preview and stable Homebrew formulae install; `brew test 2lab-ai/tap/llmux` passes.

## Risks / tensions

- **Undocumented headers**: Anthropic unified headers and Codex `x-codex-*` headers may change.
  Mitigation: usage endpoint for Claude OAuth, 429/retry-after fallback for all providers.
- **Ban risk**: per-request switching is avoided; the tool is single-user / own accounts only.
- **ToS gray zones**: multi-account Claude proxying and Codex subscription tokens outside the
  official CLI are not endorsed. Documented explicitly; no pooling, no resale.
- **Codex backend instability**: the ChatGPT/Codex backend is not a public API. The provider mimics
  the Codex CLI's originator and keeps a narrow minimal surface.

## Provenance

- Historical proxy/OAuth mechanics and import compatibility: KarpelesLab/teamclaude (MIT).
- Scheduler: 2lab-ai/soma-work `src/oauth/auto-rotate.ts`.
- Session-stickiness rationale: snipeship/ccflare `docs/load-balancing.md`.
- Provider translation references: ollama `anthropic/anthropic.go`, ChatMock, CLIProxyAPI.
- Daemon/attach conventions + brew pipeline: 2lab.ai herdr / herdr-mx + 2lab-ai/homebrew-tap.
