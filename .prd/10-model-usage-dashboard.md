# PRD — model usage dashboard (2026-06-14)

Status: **proposed / spec only**. This document describes the target product behavior for a
future implementation; it does not record shipped behavior yet.

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
