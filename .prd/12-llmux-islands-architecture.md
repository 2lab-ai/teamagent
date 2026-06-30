# llmux-islands — Architecture

Two parts: (A) a small **new HTTP surface in the Rust daemon** that exposes GUI-initiated OAuth
login (the only net-new server work — display/add-apikey/remove/switch already exist), and (B) a
**new native macOS app** (`llmux-islands/`) that lifts agent-island's presentation layer and
replaces its data layer with an llmux HTTP client.

Implements [`11-llmux-islands-spec.md`](11-llmux-islands-spec.md).

## A. Daemon HTTP surface (Rust)

### Already exposed (reused as-is)
- `GET /llmux/dashboard` → `DashboardDoc` (`src/dashboard.rs:358`). The read contract for FR1:
  per-account `name`, `type`, `status`, `five_hour`/`seven_day` `{utilization, resets_at,
  resets_in_secs}`, `in_flight`, `token_expires_at_ms`, `last_refresh_ms`, plus totals.
- `POST /llmux/add-account {api_key, name?}` → `AppState::add_apikey_account` (FR2).
- `POST /llmux/remove-account {name, confirm:true}` → `AppState::remove_account` (FR3).
- `POST /llmux/switch {account}` (FR5). All routes registered in `src/proxy/server.rs:718-733`,
  behind `client_auth` (loopback-exempt, `src/proxy/server.rs:744-749`).

### New (FR4) — GUI-initiated OAuth login
New module `src/proxy/login.rs` owning a small in-memory job registry, plus three routes added to
the router and gated by the existing `client_auth` middleware:

```text
POST /llmux/login/start   { provider: "claude" | "codex" }
   -> 200 { state: "<uuid>", auth_url?: "<provider url>" }
   -> 409 if a login is already in flight
GET  /llmux/login/status?state=<uuid>
   -> 200 { phase: "pending" | "done" | "error", account?: "<name>", error?: "<msg>" }
POST /llmux/login/cancel  { state }
   -> 200 { cancelled: bool }
```

State machine:

```text
struct LoginJob { phase: Pending | Done(account_name) | Error(msg), started_at }
AppState.logins: Arc<Mutex<Option<(state, LoginJob, JoinHandle)>>>   // single in-flight
```

- `start`: reject with 409 if a job is `Pending`. Otherwise generate `state`, set `Pending`, and
  `tokio::spawn` a task that runs the **existing** interactive flow on the daemon host (= the user's
  Mac), reusing the shared cores rather than duplicating PKCE:
  - `provider:"claude"` → `crate::cli::login::oauth_login_to_account(&client, &config.upstream)`
    (`src/cli/login.rs:97`) — PKCE browser flow (`auth::oauth::login_interactive`) + profile fetch →
    `AccountConfig` (`claude:<email>`).
  - `provider:"codex"` → `crate::auth::codex::login_codex_interactive(&client, &config.codex.token_url)`
    (`src/auth/codex.rs`) → `AccountConfig` (`codex:<email>`).
  - On success the task calls `AppState::inject_account(account)` (`src/proxy/server.rs:364`) — the
    same path the dashboard's "new login from switcher" already uses (file write via
    `config::update` + live pool hot-swap), then records `Done(name)`.
  - On failure → `Error(msg)`. The browser is opened by llmux itself (existing `login_interactive`
    behavior); `auth_url` is returned for fallback/headless display but the daemon drives the open.
- `status`: returns the current job's phase if `state` matches; `404`/stale otherwise. Terminal jobs
  are retained briefly so the poller can read the result, then cleared on the next `start`.
- Concurrency: single in-flight login (registry holds `Option`) avoids OAuth callback-port
  contention; concurrent `start` → 409 (Risk in spec). `cancel` aborts the `JoinHandle` and clears.
- Tests (in `login.rs` / `server.rs`): provider routing, 409-on-concurrent, status state
  transitions, cancel. The interactive browser step is not unit-tested (manual E2E in spec §Acceptance).

No change to the config schema, the scheduler, or the proxy fast path. The new surface is additive.

## B. macOS app (`llmux-islands/`)

New top-level sibling directory (repo is flat single-crate; `.github/`, `.prd/`, `demo/` are already
root siblings). Native SwiftUI + AppKit Xcode project; bundle id `ai.2lab.LlmuxIslands`; min macOS
15.6; `LSUIElement` notch HUD. v1 drops Sparkle (auto-update) and Mixpanel (analytics).

### Layout

```text
llmux-islands/
  LlmuxIslands.xcodeproj
  LlmuxIslands/
    App/                 # AppDelegate, WindowManager, ScreenObserver, *App  (lifted)
    UI/
      Window/            # NotchWindow + controllers                          (lifted)
      Views/             # AccountsDashboardView (was UsageDashboardView, trimmed)
      Components/        # NotchShape, UsageProviderIcon, UsageDurationText,
                         #   ActionButton, StatusIcons, TerminalColors        (lifted)
    Core/                # NotchGeometry/ViewModel, ScreenSelector, Ext+NSScreen (lifted)
    Models/
      UsageAccountTile.swift   # tile view-model (lifted, kept)
      LlmuxModels.swift        # NEW: Codable mirror of DashboardDoc account/window
    Services/Llmux/
      LlmuxClient.swift        # NEW: URLSession client over the llmux API
    ViewModels/
      AccountsViewModel.swift  # NEW: replaces UsageDashboardViewModel
    Assets.xcassets/           # AppIcon/AccentColor (lifted)
    Resources/Info.plist       # LSUIElement, notch entitlements
```

### Lifted from agent-island (presentation only)
`UI/Window/Notch*`, `Core/Notch*` + `ScreenSelector` + `Ext+NSScreen`, `App/*`, `UI/Components/*`
(notably `NotchShape`, `UsageProviderIcon`, `UsageDurationText`, `ActionButton`, `StatusIcons`,
`TerminalColors`), the `UsageAccountTile` + tile-grid rendering, and `Assets.xcassets`.

### Dropped from agent-island (data layer + irrelevant features)
Entire `Services/Usage/*` (UsageFetcher/Cache/AccountStore/ProfileStore/ProfileSwitcher/
CredentialExporter/identity matchers), the `cauth/` package, `Resources/UsageScripts/` (Node),
profile save/switch tabs, and the Claude-Code monitoring stack (Session/Tmux/Hooks/Chat/State).

### New data layer
- **`LlmuxClient`** (`URLSession`, async): base URL from settings (default `http://127.0.0.1:3456`),
  optional `x-api-key` header. Methods:
  - `dashboard() -> DashboardDTO` (GET `/llmux/dashboard`)
  - `addApiKey(name:, key:)` (POST `/llmux/add-account`)
  - `remove(name:)` (POST `/llmux/remove-account` `{name, confirm:true}`)
  - `startLogin(provider:) -> (state, authURL?)` (POST `/llmux/login/start`)
  - `loginStatus(state:) -> LoginStatusDTO` (GET `/llmux/login/status`)
  - `cancelLogin(state:)` (POST `/llmux/login/cancel`)
  - `switchTo(name:)` (POST `/llmux/switch`) — optional
- **`LlmuxModels`** — `Codable` structs mirroring the `DashboardDoc` account/window shape; mapped to
  the existing `UsageAccountTile` (provider ← `type`, label ← `name`, 5h/7d ← windows, expiry/health
  ← `token_expires_at_ms` + `status`).
- **`AccountsViewModel`** (`@MainActor ObservableObject`) — replaces `UsageDashboardViewModel`. Holds
  `tiles: [UsageAccountTile]`, `connectionState`, in-flight login state. Polls `dashboard()` on a
  timer; exposes `addApiKey/remove/startLogin(+poll loginStatus)/switch`. No credential/profile logic.

### New UI
- `AccountsDashboardView` — trimmed `UsageDashboardView`: header (title + Add + Refresh), the tile
  grid bound to `AccountsViewModel.tiles`, degraded section for expired/auth-failed.
- **Add sheet** — segmented control { API Key | Claude (OAuth) | Codex (OAuth) }. API-key path =
  text fields → `addApiKey`. OAuth path = `startLogin` then a "waiting for browser…" state polling
  `loginStatus`, resolving to success(account)/error/cancel.
- **Remove** — per-tile trash with a confirm dialog → `remove`.
- **Not running** — connection-error state when `dashboard()` fails to reach the daemon.

## Build / run / verify
- Daemon: implement on `feat/llmux-islands`; `just check` (fmt + clippy `-D warnings` + test) green;
  run locally and `curl` the new `/llmux/login/*` endpoints (manual browser step) per spec acceptance.
- App: `xcodebuild -project llmux-islands/LlmuxIslands.xcodeproj -scheme LlmuxIslands build`, then
  launch and verify tiles match `llmux accounts --json` and add/remove/login flows work against the
  live daemon.

## Boundary
Server changes stay on `feat/llmux-islands` as a review-ready PR. No master merge, no CI preview
deploy, no release tag without explicit user approval (repo deploy-gate). App verified by local
build + run.
