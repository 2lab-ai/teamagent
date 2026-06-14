# Findings — dashboard overhaul + codex settings + routing (2026-06-13)

Evidence-anchored notes from the dashboard/codex/routing work. Where a claim
rests on a live capture or external source, it is cited.

## 1. Model→group routing is now ON by default

`routing.enabled` defaulted to `false`; it now defaults to **`true`**
(`config/schema.rs`). With it on, an inbound request's `model` selects its
backend group and the scheduler picks within it — claude models → claude
accounts, codex models → codex accounts — independent of which account is
"current". `on_empty_group` stays `"error"`, so a model whose group has no
account returns a clean 404 instead of misrouting.

**Why:** with routing off, `gpt-5.5` was forwarded byte-identically to
`api.anthropic.com`, which rejects it (`model not found`). That was the user's
symptom. Routing on makes `gpt-5.5` reach a codex account. The classifier
(`routing.rs`) lowercases + prefix-matches, so `gpt-5.5`, `gpt-5.5-codex`, and
`gpt-5.5[1m]` all classify to Codex.

## 2. Codex "fast" mode = `service_tier: "priority"` on the wire (VERIFIED)

Source: openai/codex @ `f297b9f07de10c7d8b9ed284b674d06cc5ff7723`.
`codex-rs/protocol/src/config_types.rs` `ServiceTier::Fast.request_value()`
returns `"priority"`. The CLI's config stores the string `"fast"` but the wire
field is `service_tier: "priority"` (flex → `"flex"`; default → field omitted),
gated by the `FastMode` entitlement. Fast does **not** change the model slug or
reasoning effort — only `service_tier`.

llmux now mirrors this: `config.codex.fast = true` ⇒ the Responses body
carries `service_tier: "priority"`. Reasoning effort
(`none|minimal|low|medium|high|xhigh`) and the model slug are independently
configurable. All three are settable live from the dashboard (`f`/`m`/`e`) via
`POST /llmux/codex`, and persisted.

## 3. The 7d-usage bug was a percentage/fraction scale error (VERIFIED)

Live capture (2026-06-13) of `GET /api/oauth/usage` for one account returned
**percentages**: `five_hour.utilization = 16.0` (=16%), `seven_day.utilization
= 1.0` (=1%) — matching the response headers' `0.15` / `0.01` fractions. The
old `window_from_json` used `if raw > 1.0 { raw/100 } else { raw }` per window,
so a 7d value of `1.0` (1%, the common state right after the weekly window
resets) was read as the fraction `1.0` (100%). Fixed by deciding scale **per
response across both windows** (if either raw value > 1.0 the response is
percentages → ÷100 both). Fraction-style responses (max ≤ 1.0) still pass
through, so the pre-existing tests hold.

Note: a separate "7d resets in 17 min" observation was **not** a bug — it was a
genuine near weekly-reset boundary (`old_reset + 7×86400 == next_reset`,
confirmed from the live header `7d-reset`).

## 4. Context window (req9-B): NOT fixable from the proxy

**Finding:** Claude Code derives the context-window size it displays from the
**model-name string, client-side** — it is not read from any `/v1/messages`
response field and llmux serves no `/v1/models`. Sources:
- code.claude.com/docs/en/model-config — features (incl. the 1M window via the
  `[1m]` suffix) are enabled by matching the model ID against known patterns;
  the suffix is stripped before the request is sent.
- github.com/anthropics/claude-code/issues/36725, /issues/59376 — "the max
  context window size is not provided as a field — it is inferred by matching
  the model name." Unknown names fall back to 200000.

gpt-5.5 on the Codex backend has a **400K** context window
(openai.com/index/introducing-gpt-5-5). Because the value lives in Claude
Code's client-side name→window table, **no llmux response or endpoint can
set it.**

**Workaround (works today):** type `/model gpt-5.5[1m]` in Claude Code. The
`[1m]` suffix makes the client display a 1M window; llmux's classifier
still routes it to codex (`gpt-` prefix), and the codex translator rewrites the
model to the configured slug anyway. Caveat: this **over-reports** as 1M (true
codex window is 400K); there is no documented way to display exactly "400K".
The alternative — Claude Code's `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`
`/v1/models` discovery — only populates the model picker (and only for IDs
starting with `claude`/`anthropic`); it carries no window field.
