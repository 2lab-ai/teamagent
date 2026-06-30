# llmux-islands — Spec

A native macOS menu/notch app that shows per-account llmux usage at a glance and lets the user
add/remove subscriptions, driven entirely by llmux's HTTP control API. Companion to the `llmux`
daemon; lives in this repo at top-level `llmux-islands/`.

Status: **planned** (design confirmed 2026-06-30). This document records the target contract.

Sibling docs: [`12-llmux-islands-architecture.md`](12-llmux-islands-architecture.md) (how),
[`10-model-usage-dashboard.md`](10-model-usage-dashboard.md) (the in-TUI usage view this mirrors).

## Problem

llmux already aggregates several Codex (ChatGPT) and Claude (Anthropic) subscriptions and tracks
per-account 5h/7d quota windows. But today that state is only reachable from a terminal:
`llmux accounts`, `llmux status`, the ratatui dashboard. Adding/removing an account is also
terminal-only (`llmux login [--api|--codex]`, `llmux import`, `llmux remove`). A user who lives in
the GUI has no always-visible "how much quota is left across my accounts, and is everything
healthy" surface, and no one-click way to add or drop a subscription.

A rough native app already exists at `~/2lab.ai/agent-island` (SwiftUI+AppKit notch "island" HUD,
shipped at v1.2.7) that renders a per-account usage grid — but it acquires data by extracting local
Claude/Codex credentials and running its own `cauth`/Node usage scripts, duplicating logic llmux
already owns and bypassing llmux entirely. The fix is to keep that app's presentation layer and
**replace its whole data-acquisition layer with calls to the llmux HTTP API**, so llmux remains the
single source of truth for accounts and usage.

## Goals

1. **Glanceable usage** — a menu-bar/notch HUD showing every llmux account as a tile: provider
   (Claude / Codex / API-key), label, 5h + 7d window utilization with reset countdown, in-flight
   count, and token/auth health. Auto-refreshes on an interval.
2. **Add a subscription from the GUI** — API-key accounts and OAuth subscriptions (Claude Max,
   ChatGPT/Codex) added without touching a terminal.
3. **Remove a subscription from the GUI** — one-click delete with confirmation.
4. **llmux is the only data path** — the app never reads `~/.config/llmux.json`, never touches
   provider credentials, never runs `cauth`/usage scripts. Everything is an llmux HTTP call. The
   app is a thin client over an API llmux exposes.
5. **Same repo, same conventions** — ships from this repo as a sibling subproject; PRD lives in
   `.prd/`; commits follow the repo's conventional-commit rule.

## Non-goals (v1)

- No hosted/multi-user. Talks to a local llmux daemon over loopback (configurable host/port for a
  remote daemon, but single-user, the user's own accounts).
- No account *switching* automation surface beyond a manual switch action (optional). The scheduler
  still owns selection; the app does not re-implement scheduling.
- No Claude-Code agent/session/tmux monitoring (agent-island has this; it is out of scope here).
- No analytics/telemetry, no auto-update channel in v1 (Sparkle/Mixpanel dropped; can return later).
- No cost/$ accounting view in v1 (llmux exposes window utilization, not per-account dollar spend).
- The app does not initiate provider OAuth itself or hold provider secrets — llmux does (see FR4).

## Functional requirements

### FR1 — Account + usage display (read)
- The app lists every account llmux knows, sourced from **`GET /llmux/dashboard`** (the existing
  `DashboardDoc`, `src/dashboard.rs`). Per account it renders: `name`, provider (`type`:
  oauth/codex/apikey), `status`, `five_hour`/`seven_day` `{utilization, resets_at|resets_in_secs}`,
  `in_flight`, `token_expires_at_ms`, `last_refresh_ms`.
- Auto-refresh on a fixed interval (default 10s while open) and on a manual Refresh action.
- Accounts whose token is expired / auth-failed are visually separated (degraded section), matching
  agent-island's expired-tile treatment.

### FR2 — Add API-key account
- A form collecting an Anthropic API key (and optional name) calls **`POST /llmux/add-account`**
  `{api_key, name?}` (existing endpoint). On success the account appears on next refresh.

### FR3 — Remove account
- A per-tile delete with a confirm dialog calls **`POST /llmux/remove-account`**
  `{name, confirm:true}` (existing endpoint). Removal hot-applies in the running daemon.

### FR4 — Add OAuth subscription (Claude / Codex)
- The app starts a browser OAuth flow **through llmux**, not itself: **`POST /llmux/login/start`**
  `{provider:"claude"|"codex"}` (new endpoint). llmux runs the same PKCE browser flow the CLI uses,
  mints + injects the account (hot-reload), and the app reflects it on refresh.
- The app shows progress by polling **`GET /llmux/login/status?state=…`** until `done` (with the new
  account name) or `error`. A **`POST /llmux/login/cancel`** abandons an in-progress login.
- The app never sees provider tokens; only `{state, phase, account?, error?}`.

### FR5 — Manual switch (optional, behind a control)
- An optional "use this account" action calls the existing **`POST /llmux/switch`** `{account}`.

### FR6 — Connection + errors
- Default endpoint `http://127.0.0.1:3456`; host/port/api_key overridable in app settings (for a
  remote daemon — loopback is unauthenticated, remote needs the `x-api-key`).
- If the daemon is not running / unreachable, the app shows a clear "llmux not running" state with
  a hint, not a crash or a blank grid.

## Acceptance (end-to-end)

1. With a running daemon holding ≥1 account, launching the app shows real tiles whose 5h/7d numbers
   match `llmux accounts --json`.
2. Add an API key in the app → it appears in `llmux accounts`.
3. Remove an account in the app → it disappears from `llmux accounts`.
4. Add a Claude (or Codex) subscription in the app → browser opens → after authorizing, a new
   `claude:<email>` (or `codex:<email>`) account appears.
5. Killing the daemon → app shows the not-running state; restarting → tiles return.

## Risks

- **OAuth callback contention** — the CLI login binds a fixed localhost callback port; the daemon
  doing the same must serialize logins (one in-flight) to avoid a port clash. (See architecture.)
- **Loopback trust** — llmux exempts loopback from `x-api-key`. The new login endpoints are
  mutating and trigger a browser; they inherit that exemption, which is acceptable for a
  single-user local daemon but must NOT be exposed to non-loopback without the api key.
- **Sandbox vs. browser-open** — llmux (not the app) opens the browser, so the macOS app needs no
  process-spawn entitlement for OAuth; a direct-distribution build is still simplest for v1.
