# PRD — model usage dashboard (2026-06-14)

Status: **designed → implementing**. The Problem/Solution/User-Stories sections below state the
target behavior; the **Finalized Design** and **Implementation Plan** sections at the end pin the
concrete data model, document fields, keybinding, and layout that the implementation follows.

## Problem Statement

llmux already shows account-level quota and request totals, but the operator cannot answer the
next operational question from the TUI: **which model is consuming quota, how much, and with what
quality/latency/error profile?**

That gap matters more now that llmux is multi-provider and model-routed. A single dashboard session
may include Claude models, Codex models, multiple reasoning-effort levels, cache reads, streaming
and non-streaming requests, account switches, and failed requests. Account totals alone hide the
shape of usage: one noisy model can dominate token spend, one model can fail disproportionately,
and Codex's configured upstream model can differ from the inbound selector used to route the
request.

The user wants the terminal dashboard to expose **detailed model-by-model statistics and usage** in
the same cockpit, without switching to a browser dashboard or reading raw logs.

## Solution

Add a model-usage surface to the existing ratatui dashboard, backed by the same dashboard document
used by both local and attach modes.

The feature should make model usage visible at two levels:

1. **Always-visible compact summary**: a model usage strip/pane that highlights the highest-usage
   models by total tokens and request count, with terminal-friendly bars/sparklines.
2. **Detailed model usage view**: a focusable/scrollable table that exposes all known model rows,
   including request totals, success/error split, token split, cache usage where known, average
   tokens per request, average latency where known, last-seen time, provider group, and effort
   distribution.

The primary row identity is **provider group + served model**. This prevents accidental merging of
same-named models across providers and matches llmux's routing model. The detailed view can drill
into a selected model to show which accounts served it and which effort levels contributed to its
usage.

This is an observability feature only. It must not change scheduling decisions, provider routing,
credential handling, request bodies, or quota-window semantics.

## User Stories

1. As an llmux operator, I want to see requests grouped by served model, so that I know which model
   is using my accounts.
2. As an llmux operator, I want to see provider group per model row, so that Claude and Codex usage
   are not mixed together.
3. As an llmux operator, I want to see total requests per model, so that I can identify the busiest
   models quickly.
4. As an llmux operator, I want to see successful and failed request counts per model, so that I can
   spot model/provider-specific failure patterns.
5. As an llmux operator, I want to see input and output tokens per model, so that I can understand
   the direction of usage rather than only a single total.
6. As an llmux operator, I want to see total tokens per model, so that I can rank models by overall
   usage.
7. As an llmux operator, I want to see average tokens per request per model, so that I can tell
   whether a model is being used for small tasks or large sessions.
8. As an llmux operator, I want cache-read and cache-creation token fields when upstream usage
   exposes them, so that cached usage is not hidden inside vague input-token totals.
9. As an llmux operator, I want unknown cache fields rendered explicitly as unavailable, so that the
   dashboard does not imply precision it does not have.
10. As an llmux operator, I want to see the last time a model was used, so that stale rows do not
    look active.
11. As an llmux operator, I want in-flight model usage reflected separately from completed totals,
    so that a long active request is visible before it finishes.
12. As an llmux operator, I want a compact top-model summary in the normal dashboard, so that I do
    not need to enter another mode for a quick glance.
13. As an llmux operator, I want a detailed model table that can show all model rows, so that lower-
    usage models are still inspectable.
14. As an llmux operator, I want model rows sorted by a useful default such as total tokens, so that
    the highest-impact rows are first.
15. As an llmux operator, I want active or recently used models visually distinguished, so that the
    current workload stands out.
16. As an llmux operator, I want Codex's configured served model shown, so that inbound selector
    aliases do not mislead me about the actual upstream model.
17. As an llmux operator, I want Claude models with client context-window suffixes normalized to the
    served model where appropriate, so that usage is not split by display-only suffixes.
18. As an llmux operator, I want effort/reasoning-level distribution per model, so that I can see
    whether expensive reasoning settings are driving usage.
19. As an llmux operator, I want per-account breakdown for a selected model, so that I know which
    subscription or API key carried that model's load.
20. As an llmux operator, I want per-endpoint breakdown where available, so that token-counting or
    unsupported endpoints are not confused with full message-generation traffic.
21. As an llmux operator, I want model stats in attach mode, so that `llmux dashboard` shows the same
    information as the local server TUI.
22. As an llmux operator, I want reconnecting attach clients to keep rendering the last known model
    stats, so that a transient daemon poll failure does not blank the dashboard.
23. As an llmux operator, I want older daemons or older dashboard documents to degrade gracefully, so
    that an upgraded client can still attach without crashing.
24. As an llmux operator, I want model usage to be derived from completed request events, so that the
    request path is not blocked by dashboard rendering.
25. As an llmux operator, I want the feature to avoid raw prompt/content logging, so that usage
    observability does not become a data-leak surface.
26. As an llmux operator, I want model stats to reset clearly on daemon restart if they are runtime-
    only, so that I do not confuse session totals with persistent analytics.
27. As an llmux operator, I want the dashboard to label model quota windows as account-level only,
    so that I do not infer per-model quota limits that upstream does not expose.
28. As an llmux operator, I want terminal-friendly bars or sparklines for model shares, so that the
    TUI gives the same quick visual scan as a graphical usage dashboard.
29. As an llmux operator, I want the model usage panel to respect narrow terminals, so that the
    existing account table remains usable on laptops.
30. As an llmux operator, I want the keybar to advertise the model-usage view, so that the feature is
    discoverable.
31. As an llmux operator, I want the implementation to preserve the single dashboard contract, so
    that local and remote render paths do not drift.
32. As an llmux maintainer, I want model aggregation tested above the renderer, so that tests verify
    product behavior rather than fragile layout details.
33. As an llmux maintainer, I want new dashboard fields to be additive and defaulted, so that older
    documents remain parseable.
34. As an llmux maintainer, I want no scheduler changes in this feature, so that observability does
    not destabilize quota selection.

## Implementation Decisions

- **Add model usage as dashboard observability, not scheduler state.** The source of truth should be
  completed request activity plus request metadata that already flows through the dashboard. Quota
  windows remain account-level scheduler facts.
- **Use served model identity for attribution.** The model row should represent what was actually
  served by the provider path. For Claude, that is the inbound model after llmux's provider-side
  normalization. For Codex, that is the configured upstream model at request finish time.
- **Key primary rows by provider group and served model.** The table should not merge rows merely
  because two providers use the same text label.
- **Represent effort as a dimension under the model row, not as the primary model key.** The normal
  table remains model-first; the detail pane shows effort/reasoning distribution.
- **Extend token accounting to carry optional cache fields.** The current user-facing token split is
  input/output. Detailed usage requires optional cache-read and cache-creation fields when upstream
  emits them. Missing fields should be rendered as unavailable, not zero, unless the upstream
  explicitly reports zero.
- **Treat cache-inclusive provider semantics carefully.** Provider adapters may need to normalize
  upstream usage into comparable fields. If a provider reports both total input and cached input,
  the dashboard should expose both the fresh-input and cached-input components rather than hiding
  the cached part.
- **Keep runtime aggregation bounded and in-memory for this PRD.** llmux's current product contract
  says there is no analytics database. This feature should add runtime/session observability first;
  durable analytics can be a later PRD.
- **Expose model stats through the dashboard document.** Both local TUI and attach-mode dashboard
  must render from the same document/view-model path. New fields must be additive with defaults.
- **Avoid raw request content.** Aggregation must store only metadata needed for dashboard display:
  group, model, effort, endpoint class, account, status, duration, timestamps, token counters, and
  cache counters.
- **Separate compact and detailed UI surfaces.** The normal dashboard should show a compact top-model
  overview. A detailed focus/table view should make all rows accessible without crowding the account
  quota table.
- **Use terminal-native visual encodings.** Horizontal bars, compact sparklines, and proportion
  columns are preferred over dense chart widgets. The implementation should use the existing TUI
  rendering stack rather than adding a Node/chart dependency.
- **Preserve current account-table priority.** Account quota and selection order remain the primary
  operational control surface. Model usage is additional visibility, not a replacement.
- **Use explicit labels for precision.** If a field is based on the bounded activity ring, process
  lifetime, or current daemon runtime, the UI should label it accordingly.
- **Do not estimate dollar cost in the MVP.** Claude, Codex, subscription, cache, and effort pricing
  do not share one stable cost model in this project. Token usage is the MVP's reliable unit.
- **Do not infer per-model quota windows.** Existing 5h/7d windows are account/provider-account
  evidence. The dashboard may show which accounts served a model, but it must not invent model-level
  reset times or model-level limits.

## Testing Decisions

- **Test external behavior at the dashboard-contract seam first.** The highest-value tests should
  feed representative completed requests into the dashboard state and assert that the serialized
  dashboard document contains the expected model rows and counters.
- **Unit-test aggregation independently of rendering.** Model totals, cache counters, success/error
  split, last-seen timestamps, and effort distributions should be verified without depending on
  terminal buffer formatting.
- **Test backward-compatible document parsing.** A dashboard document with no model-usage field must
  still parse and render with an empty/unavailable model usage state.
- **Test local and attach parity through the shared view model.** A model-usage row produced by the
  document builder should survive conversion into the renderer input without loss.
- **Test streaming and non-streaming usage.** Mock upstreams should cover both SSE usage extraction
  and JSON-body usage extraction, because both paths feed the same model counters.
- **Test Codex usage normalization.** A Codex-style usage payload that includes cached input tokens
  should produce model stats with fresh input, cached input, output, and total display values that
  match the normalization rule.
- **Test Anthropic cache fields opportunistically.** If Anthropic usage fields include cache-read or
  cache-creation tokens, the parser should capture them; if absent, the field should remain
  unavailable rather than silently zero.
- **Test model identity under routing.** Claude rows and Codex rows should be attributed to the
  served model identity, with Codex showing the configured upstream model.
- **Test failure attribution.** Failed requests with a known model should increment the model's error
  count even when they do not include token usage.
- **Test pre-routing failures.** Requests that fail before model/provider attribution should not
  create a bogus model row; they should remain in existing global/unrouted accounting.
- **Test all-model accessibility.** If the number of model rows exceeds visible space, the detailed
  view must make lower rows reachable and indicate that more rows exist.
- **Render tests should be minimal.** Use renderer/buffer assertions only for critical labels and
  discoverability, not pixel-perfect layout snapshots that would churn with harmless UI changes.
- **No implementation is complete until the project check passes.** Before merging a future code
  change, run the repo's normal `just check` gate.

## Out of Scope

- Durable historical analytics across daemon restarts.
- A browser dashboard, Prometheus/OpenMetrics endpoint, SQLite/ClickHouse/TimescaleDB sink, or
  exported reporting pipeline.
- Dollar-cost calculation or billing reconciliation.
- Per-model quota-window inference or scheduler decisions based on model stats.
- Raw prompt, response, or tool-call content logging.
- Changing provider routing rules, Codex request translation, OAuth refresh behavior, or account
  selection policy.
- Alerting/notifications when one model exceeds a threshold.
- Multi-user/team usage accounting.
- Rewriting the existing account table or replacing the current activity/log panes wholesale.

## Further Notes

Research found that the needed high-level metadata already exists on completed request events:
provider group, served model, effort, status, duration, account, and input/output tokens. The main
missing product capabilities are (a) aggregating those fields by model, (b) carrying richer optional
usage fields such as cache-read/cache-creation tokens, and (c) rendering the aggregate in both local
and attach dashboard modes through the existing shared dashboard document.

The most important implementation constraint is to keep the dashboard architecture single-path:
server document → view model → renderer. Any implementation that adds model usage only to the local
TUI but not to attach mode is a product bug.

The feature should be implemented in small layers: first extend usage counters and model aggregation,
then expose them in the dashboard document, then map them into the view model, then render compact
and detailed model-usage panes, then add focused tests at each seam.

## Finalized Design (HOW)

This section pins the concrete contract the implementation follows. It is the design half of the
"complete the spec" step; the user stories above are the requirement half.

### Token normalization (the one comparable unit)

Both providers already report **fresh (non-cached) input** separately from cached input, so the
dashboard's input column means the same thing on both sides:

| Display field | Anthropic (`message_start.message.usage`) | Codex (Responses `usage`) |
|---|---|---|
| `tokens_in` (fresh) | `input_tokens` | `input_tokens − input_tokens_details.cached_tokens` |
| `tokens_out` | `output_tokens` (cumulative, from `message_delta`) | `output_tokens` |
| `cache_read` | `cache_read_input_tokens` (when present) | `input_tokens_details.cached_tokens` (when present) |
| `cache_creation` | `cache_creation_input_tokens` (when present) | *unavailable* (Responses does not report it) |

Cache fields are `Option<u64>`: **present iff the upstream emitted the key** (so an explicit `0` is
`Some(0)` and a missing field is `None` → rendered "—", not "0"). This satisfies req8/req9.
`account` lifetime totals and the existing in/out token split are unchanged — cache counters live
only on the new model rows (and are accumulated `saturating`).

### Aggregation key + identity

- Primary row key = **`(group, served_model)`** where `group ∈ {claude, codex}` (req1/req2). Rows are
  never merged across groups even if the model label matches.
- Served model = what the provider path actually served: Codex → the configured upstream model at
  finish time (`state.codex.model()`); Claude → the inbound model after stripping a trailing
  display-only context suffix `…[1m]` (req16/req17). Normalization = strip from the first `[`.
- A request is folded into a model row **iff both `group` and `model` are known**. Pre-routing
  failures (body-read error, routing-off early failure) keep `group=None`/`model=None` → they stay
  in the existing global/unrouted accounting and never create a bogus row (req-test "pre-routing").
  A *failed* request with a known model still increments that row's `errors` even with no tokens
  (req-test "failure attribution").

### Per-row dimensions

Each model row carries: `requests`, `ok`, `errors`, `tokens_in`, `tokens_out`, `cache_read?`,
`cache_creation?`, `last_used_ms`, `in_flight`, and three drill-down breakdowns:
- `accounts[]` — which account(s) served it (req19): name + req/ok/err + token split.
- `efforts[]` — reasoning/effort distribution (req18): label (`none` when unset) + req count.
- `endpoints[]` — endpoint class (req20): `messages` vs `count_tokens` vs other, + req count.

`in_flight` is computed separately from completed totals (req11): the in-flight request list now
carries the served `(group, model)` (threaded through `RequestRouted` → `InFlight`), and the
document builder counts matching in-flight rows per model and overlays them — including rows that
are *only* in-flight (an active request visible before its first finish).

Default sort: `tokens_in + tokens_out` desc, then `requests` desc, then key (req14).

### Document / view-model / render contract (single path)

- `DashboardDoc` gains `#[serde(default)] model_usage: Vec<ModelUsageDoc>` — additive, so older
  documents parse to an empty list and older daemons degrade gracefully (req23/req33). The nested
  `ModelAccountDoc` / `ModelCountDoc` are likewise plain serde structs.
- `dashboard_doc()` populates it from the hub's aggregation + the in-flight overlay. Both local TUI
  and attach mode go through this one builder and `DashboardView::from_doc`, so there is no render
  fork (req21/req22/req31). `DashboardView` holds `Vec<ModelUsageDoc>` directly (one representation).

### UI surfaces

1. **Compact strip** (always visible, req12/req28): a thin pane between the scheduler middle row and
   the activity log, shown only when ≥1 model row exists. Top models by total tokens, each a
   group-colored name + a proportional mini-bar of token share + `req`/`tok`/last-used age, with an
   active marker (spinner) when `in_flight>0` or used within the recency window (req15). Hidden /
   column-reduced on narrow terminals so the account table stays usable (req29).
2. **Detailed view** (req13): a full-body mode toggled by **`g`** ("models"; advertised in the
   keybar, req30). A scrollable table of *all* rows with a row count / "more" affordance, and a
   drill-down panel for the cursored row showing accounts, efforts, endpoints, and the cache split.
   Arrow/`j`/`k`/PageUp/PageDown move the model cursor; `g`/`Esc` exits; `q` quits.

No dollar cost, no per-model quota window (account-level quota only, req27), no durable storage —
rows are runtime-only and reset on daemon restart (req26). No raw prompt/response content is stored
(req25): only the metadata above.

## Implementation Plan (layers, test-first)

1. **Counters + aggregation.** Extend `sse::StreamUsage` + `tui::TokenCounts` with optional
   `cache_read`/`cache_creation`; capture Anthropic cache fields in `sse::extract_usage` and
   `usage_from_json_body`; expose Codex `cached_input_tokens` (present-only) from the converter.
   Add `ModelStats` aggregation to `tui::activity::ActivityLog` (folded on `RequestFinished`), plus
   `(group, model)` on `RequestRouted`/`InFlight`. Unit tests at this seam.
2. **Document.** Add `ModelUsageDoc` (+ nested) to `dashboard.rs`; populate in `dashboard_doc()`
   with the in-flight overlay; round-trip + backward-compat tests.
3. **View model.** Carry `model_usage` into `DashboardView`; local/attach parity test.
4. **Render.** Compact strip + `g` detailed view + keybar entry + narrow-terminal handling. Minimal
   renderer assertions for the keybar label and the strip's top-model label only.
5. **Verify.** `just check` green; build + hot-deploy; confirm rows appear in the live dashboard and
   in attach mode.
