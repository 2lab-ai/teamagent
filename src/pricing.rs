//! API-equivalent USD pricing for token usage (Feature D).
//!
//! The dashboard tracks tokens per model and per request; this module turns
//! those token counts into the **API-equivalent USD cost** so the dashboard
//! can always show "$" alongside tokens. The proxy itself bills nothing — these
//! are *reference* prices: what the same traffic would cost on the provider's
//! pay-as-you-go API.
//!
//! ## The four-rate model
//! Anthropic and OpenAI both have cache tiers, but they price them differently:
//! Anthropic bills `cache_read` at 0.1× input and `cache_creation` at 1.25×
//! input; OpenAI/codex bills cached input at a flat discounted rate and has no
//! cache-creation charge. A per-model [`ModelPrice`] with four independent
//! rates (input / output / cache_read / cache_creation) expresses both
//! providers uniformly — codex models simply carry `cache_creation: 0.0`.
//!
//! All rates are **USD per 1,000,000 tokens**. Rates sourced: claude-api skill
//! cached 2026-06-04; OpenAI gpt-5.5 pricing 2026-04-23.

use std::collections::HashMap;

use crate::tui::activity::normalize_model;
use crate::tui::TokenCounts;

/// Per-model price table entry. All four rates are **USD per 1,000,000
/// tokens**. A zero rate means "free / not charged" (e.g. codex has no
/// cache-creation charge → `cache_creation: 0.0`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelPrice {
    /// Fresh (non-cached) input tokens, USD / 1e6.
    pub input: f64,
    /// Output (completion) tokens, USD / 1e6.
    pub output: f64,
    /// Cache-read tokens, USD / 1e6.
    pub cache_read: f64,
    /// Cache-creation (write) tokens, USD / 1e6.
    pub cache_creation: f64,
}

impl ModelPrice {
    const fn new(input: f64, output: f64, cache_read: f64, cache_creation: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_creation,
        }
    }

    /// All-zero rate — the fallback for genuinely unknown (group, model) pairs.
    /// Yields `0.0` cost and never panics.
    pub const fn zero() -> Self {
        Self::new(0.0, 0.0, 0.0, 0.0)
    }
}

/// Opus-tier rates {input 5.0, output 25.0, cache_read 0.5, cache_creation 6.25}.
/// Also the `group == "claude"` unknown-model fallback.
const OPUS_TIER: ModelPrice = ModelPrice::new(5.0, 25.0, 0.5, 6.25);
/// Sonnet-tier rates {3.0, 15.0, 0.3, 3.75}.
const SONNET_TIER: ModelPrice = ModelPrice::new(3.0, 15.0, 0.3, 3.75);
/// Haiku-tier rates {1.0, 5.0, 0.1, 1.25}.
const HAIKU_TIER: ModelPrice = ModelPrice::new(1.0, 5.0, 0.1, 1.25);
/// Fable-5 rates {10.0, 50.0, 1.0, 12.5}.
const FABLE_TIER: ModelPrice = ModelPrice::new(10.0, 50.0, 1.0, 12.5);
/// gpt-5.5 / codex default {input 5.0, output 30.0, cache_read 0.5,
/// cache_creation 0.0}. Codex has no cache-creation charge. Also the
/// `group == "codex"` unknown-model fallback.
const GPT_5_5: ModelPrice = ModelPrice::new(5.0, 30.0, 0.5, 0.0);

/// Look up the built-in default price for a *normalized*, lowercased model
/// slug. Exact matches first, then a sensible prefix fallback (so e.g.
/// `claude-opus-4-8-20260101` still resolves to the opus tier). Returns `None`
/// when nothing matches — callers apply the group fallback.
fn builtin_price(model_norm_lower: &str) -> Option<ModelPrice> {
    // Exact (post-normalization) matches.
    let exact = match model_norm_lower {
        "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" | "claude-opus-4-5" => {
            Some(OPUS_TIER)
        }
        "claude-sonnet-4-6" | "claude-sonnet-4-5" => Some(SONNET_TIER),
        "claude-haiku-4-5" => Some(HAIKU_TIER),
        "claude-fable-5" => Some(FABLE_TIER),
        "gpt-5.5" => Some(GPT_5_5),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    // Prefix fallback for versioned / suffixed slugs.
    if model_norm_lower.starts_with("claude-opus-") {
        Some(OPUS_TIER)
    } else if model_norm_lower.starts_with("claude-sonnet-") {
        Some(SONNET_TIER)
    } else if model_norm_lower.starts_with("claude-haiku-") {
        Some(HAIKU_TIER)
    } else if model_norm_lower.starts_with("claude-fable-") {
        Some(FABLE_TIER)
    } else if model_norm_lower.starts_with("gpt-5.5") {
        Some(GPT_5_5)
    } else {
        None
    }
}

/// Resolve the price for `(group, model)`, honoring config `overrides` first.
///
/// Resolution order:
/// 1. `overrides` keyed by the **normalized** model (case preserved as written
///    in config, but matched case-insensitively against the normalized model).
/// 2. The built-in default table (exact then prefix), case-insensitive.
/// 3. Group fallback: `group == "claude"` → opus-tier rates; `group ==
///    "codex"` → gpt-5.5 rates; any other group → `None` (all-zero cost).
///
/// `overrides` keys are normalized + lowercased on read, so a config can use
/// either the display slug (`claude-opus-4-8[1m]`) or the bare slug.
pub fn price_for(
    group: &str,
    model: &str,
    overrides: &HashMap<String, ModelPrice>,
) -> Option<ModelPrice> {
    let norm = normalize_model(model);
    let norm_lower = norm.to_ascii_lowercase();

    // 1. Config override wins. Match case-insensitively on the normalized slug.
    if !overrides.is_empty() {
        if let Some(p) = overrides.get(&norm) {
            return Some(*p);
        }
        for (k, v) in overrides {
            if normalize_model(k).eq_ignore_ascii_case(&norm) {
                return Some(*v);
            }
        }
    }

    // 2. Built-in default table.
    if let Some(p) = builtin_price(&norm_lower) {
        return Some(p);
    }

    // 3. Group fallback.
    match group.to_ascii_lowercase().as_str() {
        "claude" => Some(OPUS_TIER),
        "codex" => Some(GPT_5_5),
        _ => None,
    }
}

/// API-equivalent USD cost for one [`TokenCounts`] under `(group, model)`'s
/// price. Unknown / zero-rate model → `0.0`. Never panics.
///
/// `cost = input·in/1e6 + output·out/1e6 + cache_read·cr/1e6 +
/// cache_creation·cc/1e6`; absent (`None`) cache fields contribute `0`.
pub fn cost_usd(
    group: &str,
    model: &str,
    tokens: &TokenCounts,
    overrides: &HashMap<String, ModelPrice>,
) -> f64 {
    cost_from_parts(
        group,
        model,
        tokens.input,
        tokens.output,
        tokens.cache_read,
        tokens.cache_creation,
        overrides,
    )
}

/// Same as [`cost_usd`] but from the accumulated row fields the dashboard
/// already holds (`tokens_in`/`tokens_out` plus the `Option` cache counters on
/// [`crate::tui::activity::ModelUsage`]). `None` cache fields contribute `0`.
/// Unknown / zero-rate model → `0.0`. Never panics.
#[allow(clippy::too_many_arguments)]
pub fn cost_from_parts(
    group: &str,
    model: &str,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    overrides: &HashMap<String, ModelPrice>,
) -> f64 {
    let Some(price) = price_for(group, model, overrides) else {
        return 0.0;
    };
    let per_m = |count: u64, rate: f64| (count as f64) * rate / 1_000_000.0;
    per_m(tokens_in, price.input)
        + per_m(tokens_out, price.output)
        + per_m(cache_read.unwrap_or(0), price.cache_read)
        + per_m(cache_creation.unwrap_or(0), price.cache_creation)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn empty() -> HashMap<String, ModelPrice> {
        HashMap::new()
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < EPS, "expected ~{b}, got {a}");
    }

    fn tc(input: u64, output: u64, cr: Option<u64>, cc: Option<u64>) -> TokenCounts {
        TokenCounts {
            input,
            output,
            cache_read: cr,
            cache_creation: cc,
        }
    }

    #[test]
    fn opus_input_one_million_is_five_dollars() {
        let cost = cost_usd(
            "claude",
            "claude-opus-4-8",
            &tc(1_000_000, 0, None, None),
            &empty(),
        );
        approx(cost, 5.00);
    }

    #[test]
    fn opus_cache_read_one_million_is_fifty_cents() {
        let cost = cost_usd(
            "claude",
            "claude-opus-4-8",
            &tc(0, 0, Some(1_000_000), None),
            &empty(),
        );
        approx(cost, 0.50);
    }

    #[test]
    fn opus_cache_creation_one_million_is_six_twentyfive() {
        let cost = cost_usd(
            "claude",
            "claude-opus-4-8",
            &tc(0, 0, None, Some(1_000_000)),
            &empty(),
        );
        approx(cost, 6.25);
    }

    #[test]
    fn gpt_5_5_output_one_million_is_thirty_dollars() {
        let cost = cost_usd("codex", "gpt-5.5", &tc(0, 1_000_000, None, None), &empty());
        approx(cost, 30.00);
    }

    #[test]
    fn gpt_5_5_has_no_cache_creation_charge() {
        // Codex never bills cache creation; even a huge count costs nothing for it.
        let cost = cost_usd(
            "codex",
            "gpt-5.5",
            &tc(0, 0, None, Some(1_000_000)),
            &empty(),
        );
        approx(cost, 0.0);
    }

    #[test]
    fn mixed_tokens_sum_each_component() {
        // opus: 5/25/0.5/6.25 per 1e6.
        // 200k in (1.0) + 100k out (2.5) + 50k cr (0.025) + 40k cc (0.25) = 3.775.
        let cost = cost_usd(
            "claude",
            "claude-opus-4-8",
            &tc(200_000, 100_000, Some(50_000), Some(40_000)),
            &empty(),
        );
        approx(cost, 1.0 + 2.5 + 0.025 + 0.25);
    }

    #[test]
    fn sonnet_and_haiku_and_fable_tiers() {
        approx(
            cost_usd(
                "claude",
                "claude-sonnet-4-5",
                &tc(1_000_000, 0, None, None),
                &empty(),
            ),
            3.0,
        );
        approx(
            cost_usd(
                "claude",
                "claude-haiku-4-5",
                &tc(0, 1_000_000, None, None),
                &empty(),
            ),
            5.0,
        );
        approx(
            cost_usd(
                "claude",
                "claude-fable-5",
                &tc(1_000_000, 0, None, None),
                &empty(),
            ),
            10.0,
        );
    }

    #[test]
    fn normalized_suffix_resolves_to_same_price() {
        // The display-only [1m] suffix must not split the price lookup.
        let bare = cost_usd(
            "claude",
            "claude-opus-4-8",
            &tc(1_000_000, 0, None, None),
            &empty(),
        );
        let suffixed = cost_usd(
            "claude",
            "claude-opus-4-8[1m]",
            &tc(1_000_000, 0, None, None),
            &empty(),
        );
        approx(bare, 5.0);
        approx(suffixed, 5.0);
    }

    #[test]
    fn case_insensitive_model_lookup() {
        let cost = cost_usd(
            "claude",
            "Claude-Opus-4-8",
            &tc(1_000_000, 0, None, None),
            &empty(),
        );
        approx(cost, 5.0);
    }

    #[test]
    fn unknown_model_empty_overrides_unknown_group_is_zero_no_panic() {
        let cost = cost_usd(
            "weirdgroup",
            "totally-made-up-model",
            &tc(9_999_999, 9_999_999, Some(9_999_999), Some(9_999_999)),
            &empty(),
        );
        approx(cost, 0.0);
    }

    #[test]
    fn unknown_claude_model_falls_back_to_opus_tier() {
        let cost = cost_usd(
            "claude",
            "claude-future-9",
            &tc(1_000_000, 0, None, None),
            &empty(),
        );
        approx(cost, 5.0);
    }

    #[test]
    fn unknown_codex_model_falls_back_to_gpt_5_5() {
        let cost = cost_usd(
            "codex",
            "gpt-6-mini",
            &tc(0, 1_000_000, None, None),
            &empty(),
        );
        approx(cost, 30.0);
    }

    #[test]
    fn config_override_beats_default() {
        let mut overrides = HashMap::new();
        overrides.insert("gpt-5.5".to_string(), ModelPrice::new(9.99, 0.0, 0.0, 0.0));
        let cost = cost_usd(
            "codex",
            "gpt-5.5",
            &tc(1_000_000, 0, None, None),
            &overrides,
        );
        approx(cost, 9.99);
    }

    #[test]
    fn config_override_keyed_with_suffix_still_matches() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "claude-opus-4-8[1m]".to_string(),
            ModelPrice::new(1.0, 0.0, 0.0, 0.0),
        );
        // Looked up by the bare slug; override key carried a [1m] suffix.
        let cost = cost_usd(
            "claude",
            "claude-opus-4-8",
            &tc(1_000_000, 0, None, None),
            &overrides,
        );
        approx(cost, 1.0);
    }

    #[test]
    fn cost_from_parts_matches_cost_usd() {
        let tokens = tc(700, 300, Some(120), None);
        let a = cost_usd("codex", "gpt-5.5", &tokens, &empty());
        let b = cost_from_parts("codex", "gpt-5.5", 700, 300, Some(120), None, &empty());
        approx(a, b);
        // gpt-5.5: 700*5/1e6 + 300*30/1e6 + 120*0.5/1e6 = 0.0035 + 0.009 + 0.00006.
        approx(b, 0.0035 + 0.009 + 0.000_06);
    }

    #[test]
    fn price_for_returns_none_for_unknown_group_and_model() {
        assert!(price_for("nope", "nope", &empty()).is_none());
        // But an override makes even an unknown group resolvable.
        let mut overrides = HashMap::new();
        overrides.insert("nope".to_string(), ModelPrice::zero());
        assert!(price_for("nope", "nope", &overrides).is_some());
    }
}
