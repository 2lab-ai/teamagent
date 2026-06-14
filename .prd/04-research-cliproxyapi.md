# llmux — Research note: CLIProxyAPI (2026-06-13)

Deep-dive on [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) (Go, MIT,
~37k★, latest v7.1.x). Purpose: it is the **generalized superset** of what llmux does narrowly.
Mining its proven internal factoring for llmux's roadmap milestones — **without** adopting its
generality, which is exactly what llmux's non-goals refuse. Companion to [03-research-notes.md].

## What it is (and how it differs from llmux)

CLIProxyAPI is an **everything-gateway**: it exposes OpenAI(+Responses)/Gemini/Claude/Codex/Grok
-compatible *inbound* endpoints and routes to many *backends* (Gemini CLI, ChatGPT Codex, Claude
Code, Grok, AI Studio, Antigravity, any OpenAI-compatible upstream) via OAuth **and** API keys, with
multi-account round-robin/fill-first load balancing, a translator matrix, an executor registry, a
c-shared plugin host, a management API + web panel, Redis usage queue, and cluster docker-compose.

The axis difference is the whole point:

| | llmux | CLIProxyAPI |
|---|---|---|
| Inbound | **one** harness — Claude Code, Anthropic Messages only | many formats (OpenAI/Gemini/Claude/Codex/Grok) |
| Bet | preserve the *harness* (capital); model is swappable part | be the universal *gateway* for every client+backend |
| Backends | Anthropic passthrough + Codex (+ Gemini stub) | ~7 channels + generic OpenAI-compatible + plugins |
| Tenancy | one human, own accounts (compliance = identity) | multi-tenant, commercial-mode, Redis, cluster |
| Surface | single Rust binary, TUI-first | Go service, web control panel, plugin .so |

So: **llmux is the opinionated path through the map that CLIProxyAPI draws in full.** The value
is its *internal seams*, not its scope.

## The transferable architecture: a 3-seam factoring

CLIProxyAPI proves the `(inbound-format) × (outbound-backend)` matrix factors into three registries,
joined by a routing layer. llmux has all three **embryonically but fused** (`provider/` trait
bundles translate+execute; `routing.rs` separate; `sse.rs` state machine).

1. **Translator registry — pure, directional, keyed by `(FromFormat, ToFormat)`.**
   `sdktr.Register(FOpenAI, FMyProv, requestFn, ResponseTransform{Stream, NonStream})` in `init()`.
   - request: `func(model, raw []byte, stream bool) []byte` (inbound JSON → provider JSON)
   - response: `NonStream func(ctx, model, originalReq, translatedReq, raw, param *any) string`
   - response: `Stream    func(...) []string`  ← **one upstream chunk → 0..N inbound SSE events**,
     `param *any` carries cross-chunk state (block index / ordering).
   - dirs: `internal/translator/{claude,openai,gemini,gemini-cli,codex,antigravity,common}` + `init.go`.

2. **Executor registry — impure transport, `auth.ProviderExecutor`.**
   `Identifier() string`, optional `PrepareRequest(req, auth)` (credential injection),
   `Execute(...) Response`, `ExecuteStream(...) <-chan StreamChunk`, `Refresh(ctx, auth) Auth`.
   Registered via `core.RegisterExecutor(x)`; manager routes auth entries of that provider to it.
   Per-auth transport via `SetRoundTripperProvider` (proxy/mTLS per account).

3. **Model registry** — `GlobalModelRegistry().RegisterClient(authID, provider, models)` feeds
   `/v1/models` and routing.

**The llmux mapping is unusually clean** because inbound is *always* Anthropic Messages: every
backend needs exactly `(anthropic → X)` request + `(X → anthropic)` response. Adding Gemini =
register two transforms + one executor, **touching zero of `forward.rs`**. llmux's existing
`select.rs` purity discipline (pure fn over snapshot) is the cultural fit that makes "translator =
pure fn" land naturally. `provider/codex.rs` currently intermixes both concerns; splitting along
this seam **now**, at 2 providers, is the cheapest it will ever be.

## High-ROx borrows, mapped to roadmap

| Idea (from CLIProxyAPI) | Serves llmux milestone | On-thesis? |
|---|---|---|
| **`oauth-model-alias`** per-channel `name→alias` (+`fork` to expose both in /v1/models) | **[next] model-level routing** — generalizes the hardcoded `gpt-5.5` pin into a config table; inbound model → real upstream id | ✅ core |
| **`openai-compatibility`** generic provider (base-url, api-key-entries, per-model image/thinking, round-robin internal pool) | **Tier-1 expansion** — one reusable `anthropic↔openai-chat` translator pair unlocks *a whole class* of API-key backends (OpenRouter, local llama, vLLM) with no per-vendor code | ✅ highest ROI |
| **Translator/Executor split + (from,to) registry** | **[then] per-subagent cross-provider** — makes 3+ backends tractable; refactor seam for `provider/` | ✅ core |
| **`payload` rules** (declarative `default`/`override`/`filter`/`*-raw`, matched by model-wildcard/protocol/header) | replaces hardcoded Codex pins (`stream:true`, `store:false`, `prompt_cache_key`); per-backend field strip/normalize without code | ✅ low-risk |
| **`watcher`** (auth-dir / config hot reload) | auto-sync imported creds when `~/.codex/auth.json` or `~/.claude/.credentials.json` change out-of-band; complements daemon refresh | ✅ UX |
| **`quota-exceeded` ladder** (`switch-project` → `switch-preview-model` → last-resort pool) | generalizes `on_empty_group:fallback` into an **explicit, opt-in** degradation ladder | ⚠️ partial — must stay non-silent (non-goal: no model laundering) |
| **`session-affinity`** key extraction (`X-Session-ID`, `metadata.user_id`, `X-Amp-Thread-Id`) | only if llmux serves **N concurrent Claude Code windows** through one daemon, each pinned to its own account | 🔵 future-conditional |
| **`fill-first` strategy** | alternative to round-robin; llmux's "soonest 7d-reset first" ranking is already a smarter fill-first variant — confirms the instinct | ✅ already have better |
| **`request-retry` on 403/408/5xx, `max-retry-credentials`, `max-retry-interval`** | hardens `forward.rs` taxonomy; bound credential fan-out per failed request | ✅ minor |
| **Management API gate** (`allow-remote`, hashed `secret-key`, `disable-control-panel`) | richer version of llmux's loopback-exempt `/llmux/*` gate — only if a remote/web panel is ever wanted | 🔵 low priority (TUI-first identity) |

## Studied and deliberately rejected (the discipline line)

- **Multi-inbound-format** (accepting OpenAI/Gemini *clients*). This dissolves llmux's identity —
  the bet is **one** harness. CLIProxyAPI is gateway-for-everything; llmux is harness-preserver.
  Keep inbound singular (Anthropic Messages).
- **c-shared `.so` plugin host** (`pluginhost`/`pluginstore`). Over-general, adds a security/ABI
  surface, contradicts "stay narrow / not a new harness." The Rust analog is the *in-tree* registry
  (borrow the concept, reject the dynamic mechanism).
- **Redis usage queue / `commercial-mode` / cluster compose.** Multi-tenant infra is the opposite of
  "single human, own accounts, no pooling, no resale" — that compliance posture *is* the identity.
- **Web control panel as the primary surface.** TUI-first is identity; a web panel is at most a far
  bonus.

## Net recommendation

Refactor `provider/` along CLIProxyAPI's **three seams now** (translator pure / executor impure /
model+alias routing) while there are only two real backends — the cost never gets lower. Then the
roadmap composes: `oauth-model-alias` delivers [next] model-routing; an `openai-compatible` provider
(one translator pair) is the biggest Tier-1 unlock; `payload` rules + `watcher` are cheap polish; the
registry shape is what makes the [then] per-subagent endgame tractable. Borrow the **factoring**;
keep refusing the **generality** — the generality is precisely what llmux's non-goals exist to
reject.

## Sources

- README / config.example.yaml / `docs/sdk-advanced.md` (executors & translators) /
  `internal/translator` tree, fetched 2026-06-13. SDK interface signatures reconstructed from the
  advanced-doc example; exact param names may differ from upstream definitions.
