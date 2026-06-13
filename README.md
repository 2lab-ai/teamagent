# teamagent

**Models change every month. Your harness shouldn't.**

teamagent lets you build your agent workflow once on a single canonical harness — [Claude Code](https://www.anthropic.com/claude-code) — and swap the *model* behind it freely. Wherever the next frontier model ships, you don't re-port your setup.

![teamagent demo](screenshots/teamagent-demo.gif)

## The problem

Frontier models keep coming out of the big labs, and "current best" moves often. But each vendor's CLI agent **harness** — the operating layer around the model: file edits, shell execution, tool calls, context management, permissions — evolves independently and is mutually incompatible. That creates four layers of pain:

1. **Can't port.** A workflow built in Claude Code (subagents, slash commands, MCP servers, `CLAUDE.md` conventions, hooks) does not transfer to Codex CLI or Gemini CLI as-is.
2. **Can't sync.** Even after a painful port, each harness keeps improving separately — there's no way to keep two environments in the same state. The gap only widens.
3. **Model lock-in.** Moving to a better model means moving your *entire harness*. Your tooling investment holds your model choice hostage.
4. **Subscription lock-in.** Flat-rate subscriptions are bound to each vendor's first-party client — you can't drive a Claude subscription from a third-party tool.

**Root cause:** the valuable asset (your workflow) is bound to the harness, and the harness is bound to the model and vendor. teamagent breaks that chain by standardizing on **one** harness and making the model a swappable part behind it.

## The thesis

What you actually want to preserve isn't a specific *model* — it's the *harness environment* you built. The model is a consumable; the harness is capital.

So teamagent adopts Claude Code as **the one canonical harness** and turns the model into a part you swap behind it. Model-switch cost drops from "rebuild your harness" to "one setting." You keep your subagents, your slash commands, your `CLAUDE.md` — and point them at whichever model is best this month.

## What ships today

A local proxy that sits behind Claude Code (`ANTHROPIC_BASE_URL=http://localhost:3456` is the whole integration contract) and routes requests to a backend you control:

- **One Rust binary, `teamagent`** — `server`, `run`, `dashboard`, `stop`, `login`, `import`, `env`, `status`, `accounts`, `remove`, `api`.
- **Claude Code stays unmodified.** `teamagent run` starts (or reuses) the local daemon and launches `claude` pointed at the proxy.
- **Multiple accounts, one cockpit.** Manage several Claude subscription/API-key accounts plus optional OpenAI Codex accounts, and switch between them without leaving Claude Code.
- **Quota-window-aware scheduling.** Each Claude account tracks its 5-hour and 7-day windows from upstream headers + OAuth usage polling; eligible accounts are ranked by soonest 7-day reset (use it before it resets), then lower 5-hour utilization. Sticky per session, never per request.
- **Codex (gpt-5.5) as an overflow/alternate backend.** A ChatGPT/Codex credential is translated from the Anthropic Messages API into the Codex Responses backend and streamed back as Anthropic SSE — so Claude Code talks to GPT without knowing it.
- **Daemon-first + attach-mode dashboard.** A detached daemon keeps polling and refreshing tokens; `teamagent dashboard` renders the live ratatui view from it.

This is **Tier 1 + a first step into Tier 2** (see below): the multi-account and Codex pieces are the proving ground for the real goal — pick a *model*, and let teamagent route it to the right backend.

## Two tiers: where we bet, what's convenience

teamagent draws a hard line between what it stakes its identity on and what is a best-effort convenience that depends on vendor policy.

| | **Tier 1 — durable (the identity)** | **Tier 2 — convenience (bonus)** |
|---|---|---|
| What | Claude Code as the single harness. Claude via subscription (through Claude Code), other models via **API key**. | Routing non-Anthropic models through *their* flat-rate subscription, where the vendor currently allows it. |
| Compliance | Fully compliant, stable. | Vendor-policy-dependent, gray, mutable. |
| Value | Solves painpoints 1–4. ~90% of the value. | Flat-rate savings. Can break without notice. |

We put the product's identity in Tier 1. Tier 2 is offered opt-in, with an explicit "works now, no guarantee" warning — so the product's lifespan isn't hostage to the next vendor policy change.

## Roadmap

```
[now]   Subscription-level switching
        One backend per session. Toggle "Claude mode" <-> "Codex mode".
          |
          v
[next]  Model-level routing
        You pick a MODEL; teamagent maps model -> subscription/key transparently.
        You stop having to think about which subscription a model lives on.
          |
          v
[then]  Per-subagent cross-provider
        In one Claude Code session:
          main agent  = a Claude model   (Anthropic subscription, native to Claude Code)
          subagents   = gpt-5.5          (OpenAI backend, via the router)
        Wire Claude Code's subagent `model` field to a backend mapping.
        "GPT subagents inside the Claude Code harness, naturally" — the endgame.
```

Claude Code already supports in-session model switching and per-task routing, and subagents already carry a `model` field (`.claude/agents/*.md`). The endgame composes those existing mechanisms — the router just maps the model string to a different backend. (Model names move fast; `fable-5` / `gpt-5.5` are illustrative of the *shape*, replaced by whatever is current.)

## Non-goals

- **Not a new harness.** teamagent attaches above/below Claude Code; it does not replace it. (Competing on harness features is a losing game — Claude Code is overwhelmingly harness code, and that's the moat.)
- **Not model laundering.** Route to a weaker model and you get that model's quality. teamagent unifies the UX; it cannot raise intelligence.
- **Not a policy-circumvention product.** Vendor-policy gray zones live in Tier 2, opt-in and clearly marked — never the identity.

## Install

```bash
brew install 2lab-ai/tap/teamagent
```

Rolling preview channel:

```bash
brew install 2lab-ai/tap/teamagent-preview
```

Or build from source:

```bash
git clone https://github.com/2lab-ai/teamagent && cd teamagent
just build    # cargo build --release --locked
```

## Quick start

```bash
# Add accounts — browser OAuth, one login per account
teamagent login
teamagent login

# Or import existing credentials from supported local stores
teamagent import

# Start the proxy with the foreground TUI when attached to a TTY
teamagent server

# In another terminal, run Claude Code through the proxy
teamagent run
```

`teamagent run` spawns `claude` with only `ANTHROPIC_BASE_URL` set and passes arguments through after `--`. If nothing is listening on the configured port, `run` auto-starts a detached daemon (stderr at `~/.local/state/teamagent/server.log`, respecting `$XDG_STATE_HOME`) and waits until it is ready. A port occupied by a foreign process is an error, never spawned over.

A convenient alias, so launching Claude Code through teamagent is one word:

```bash
alias claudex='teamagent run'
claudex
```

Manual shell wiring also works:

```bash
eval "$(teamagent env)"
claude
```

## Commands

| Command | Description |
|---|---|
| `server [--port N] [--no-tui] [--log-to DIR]` | Start the proxy. `--log-to` writes one file per request with credentials masked. If a teamagent daemon already owns the port, attach to it instead. |
| `dashboard` | Attach to a running daemon and render its dashboard over HTTP. Read-only except manual account switch. |
| `run [-- args]` | Ensure the daemon is running, then spawn `claude` pointed at the proxy. |
| `stop` | Stop a running server gracefully via `POST /teamagent/shutdown`. |
| `login [--api]` | Add a Claude account via browser OAuth, or paste an Anthropic API key with `--api`. |
| `import [--from PATH \| --json JSON]` | Import credentials from a teamclaude config, `~/.claude/.credentials.json`, Codex auth JSON, or inline JSON. |
| `env` | Print shell exports for pointing Claude Code at the proxy. |
| `status [--json]` | Show client/server/update sections; exits 1 when no server is running. |
| `accounts [-v]` | List configured accounts; `-v` adds quota/cooldown detail. |
| `remove <name> [--yes]` | Remove an account by name. |
| `api <path>` | Debug: GET an upstream path with the current account's credentials. |

In the TUI: `s` switches account, `a` adds, `r` removes, `R` reloads config, `d` toggles detail, `l` cycles the log panel, `q` quits, and `j`/`k` or arrows navigate. In attach mode (`teamagent dashboard`, or `server` attaching to a daemon), config-mutation keys `a`/`r`/`R` are disabled because they would act on the server host's config; `s` still works through `POST /teamagent/switch`.

## Daemon and dashboard

Only one process can own port 3456 — normally the background daemon created by `teamagent run`. To inspect it:

- `teamagent dashboard` polls `GET /teamagent/dashboard` once a second and renders the same view model the in-process TUI uses. Dropped connections show a reconnecting banner and keep retrying.
- `teamagent server`, when a teamagent daemon already owns the port, prints `daemon already running (pid N) — attaching…` and enters attach mode instead of failing with `Address already in use`.
- A foreign process on the port remains a clean error and is never overwritten.

Both attach paths are read-only except manual switching through the gated loopback control endpoint.

## Configuration

Config lives at `~/.config/teamagent.json` (respects `$XDG_CONFIG_HOME`; override with `$TEAMAGENT_CONFIG`). File mode is 0600. Writes are atomic read-merge-write so the server and CLI can update concurrently.

```json
{
  "version": 1,
  "proxy": { "port": 3456, "api_key": "ta-..." },
  "upstream": "https://api.anthropic.com",
  "scheduler": {
    "five_hour_max": 0.90,
    "seven_day_max": 0.99,
    "usage_poll_secs": 300,
    "usage_max_age_secs": 600,
    "refresh_ahead_secs": 25200
  },
  "routing": {
    "enabled": false,
    "claude_models": [],
    "codex_models": [],
    "default_group": "claude",
    "on_empty_group": "error"
  },
  "accounts": [
    {
      "name": "user@example.com",
      "type": "oauth",
      "account_uuid": "...",
      "access_token": "<oauth-access-token>",
      "refresh_token": "<oauth-refresh-token>",
      "expires_at_ms": 1774384968427
    }
  ]
}
```

Scheduler knobs:

| Key | Default | Meaning |
|---|---:|---|
| `five_hour_max` | `0.90` | Max 5-hour utilization before an account is ineligible. |
| `seven_day_max` | `0.99` | Max 7-day utilization before an account is ineligible. |
| `usage_poll_secs` | `300` | Per-account OAuth usage poll interval. |
| `usage_max_age_secs` | `600` | Usage older than this is stale; stale accounts are skipped unless all are stale. |
| `refresh_ahead_secs` | `25200` | Background refresh threshold; default 7 hours before token expiry. |

Accounts are `oauth` (Claude subscription), `apikey` (Anthropic API key), or `codex` (ChatGPT/Codex subscription token). Claude accounts dedupe by `account_uuid`; Codex accounts dedupe by `account_id`; API keys dedupe by name. A `ta-...` proxy API key is generated on first run; localhost clients are exempt.

## Scheduling model

Each account tracks two quota windows: 5-hour session and 7-day weekly. Anthropic accounts get passive data from upstream response headers plus active OAuth usage polling; Codex accounts ingest `x-codex-*` headers when present.

Selection happens when the current account becomes ineligible and on periodic checks, not per request:

1. Filter to accounts with healthy auth, no active 429 park, utilization under both thresholds, and fresh usage data where required.
2. Rank eligible Claude/API accounts by soonest 7-day reset, then lower 5-hour utilization, then stable id.
3. Rank Codex accounts last so they behave as overflow/manual backends rather than consuming quota before healthy Claude accounts.
4. Stick to the selected account until it crosses a threshold, its window resets, it 429s, or the user manually switches.
5. Honor `retry-after` on 429. If every account is exhausted, return 429 with the soonest reset as `retry-after`.

## Model routing

By default (`routing.enabled = false`) the scheduler treats Codex accounts as a cross-group overflow pool: a request lands on the best Claude/API account and only spills to Codex when every Claude account is exhausted. The inbound `model` is ignored for account selection.

Turn routing **on** (`routing.enabled = true`) to select the backend **group** by the request's `model` instead:

- **claude group** — `oauth` + `apikey` accounts; models `claude-*`, `opus`, `sonnet`, `haiku`, `fable-5`.
- **codex group** — `codex` accounts; models `gpt-*`, `gpt-5.5`, `codex`, `o1`/`o3`/`o4`.

Within the matched group the existing scheduler picks the best eligible account, sticky **per group** (the Claude pick and the Codex pick advance independently). An unrecognized or absent model falls back to `default_group`.

```json
"routing": {
  "enabled": true,
  "claude_models": [],
  "codex_models": [],
  "default_group": "claude",
  "on_empty_group": "error"
}
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Off = today's overflow behavior; on = model→group routing. |
| `claude_models` | `[]` | Models routed to the claude group. Empty keeps the builtin rules; a non-empty list replaces them. |
| `codex_models` | `[]` | Models routed to the codex group (same semantics). |
| `default_group` | `"claude"` | Group for an unmatched / model-less request. |
| `on_empty_group` | `"error"` | When the matched group has no configured account: `"error"` returns a 404 `not_found_error`; `"fallback"` falls back to the other group. |

Override tokens in `claude_models` / `codex_models` are matched in order, first-match-wins, case-insensitively. A bare token is a **prefix** (`"gpt-"`); prefix it with `~` for a **substring** (`"~codex"`) or `=` for an **exact** match (`"=gpt-5.5"`).

### Selecting the codex model from Claude Code

The inbound `model` string **is** the selector — point Claude Code's model at a codex-group model and the proxy routes the request to a Codex account:

```bash
# Per-session: route this Claude Code session's requests to the codex group
ANTHROPIC_MODEL=gpt-5.5 claude
```

or set the model in Claude Code's own model setting (e.g. `/model gpt-5.5`). The Codex provider pins the upstream model to `gpt-5.5` regardless of the exact codex-group model named, so any `gpt-*` / `codex` / `o1`–`o4` string that classifies to the codex group reaches the same upstream — the model string's only job here is to choose the group. `/teamagent/status` reports the per-group current accounts under `current_by_group` (and keeps a representative scalar `current` for back-compat).

## Codex (gpt-5.5) backend

Experimental. A ChatGPT/Codex subscription credential from `~/.codex/auth.json` can be imported as a `type: "codex"` account:

```bash
teamagent import --from ~/.codex/auth.json
```

The Codex provider translates Claude Code Messages requests into the Codex Responses backend, pins the model to `gpt-5.5`, and converts the stream back into Anthropic Messages SSE. Text, thinking summaries, and tool calls are supported; images are dropped with a warning for now. Codex accounts rank last and are best selected manually with `s` in the TUI; `/v1/messages/count_tokens` is answered locally, other non-`/v1/messages` endpoints return a clear 501.

## Compliance & caveats

teamagent is for **one human using their own accounts** — no credential pooling, no resale.

- **Tier 1 is the safe path.** Claude via subscription through Claude Code, everything else via API key. This is fully compliant and stable.
- **Tier 2 is gray.** Driving a vendor's flat-rate subscription from outside its official client depends on that vendor's current policy and can break or trigger account action without notice. The Codex backend uses ChatGPT subscription tokens outside the official client; OpenAI does not endorse it. Anthropic restricts using Claude subscription tokens outside Claude Code / Claude.ai. Use Tier 2 opt-in, at your own risk, with your own accounts only — and keep an API-key fallback configured.
- Anthropic's unified quota headers are undocumented and may change; the OAuth usage endpoint and 429 + `retry-after` are the fallback evidence chain.
- Not affiliated with Anthropic or OpenAI.

The product intent — what teamagent is, what it bets on, and what it refuses — is fixed in [`.prd/`](.prd/) as the source of truth.

## Development

```bash
just check    # cargo fmt --check + cargo clippy --all-targets -- -D warnings + cargo test
just build    # cargo build --release --locked
```

Contributor conventions are in [`AGENTS.md`](AGENTS.md).

## License

MIT.
