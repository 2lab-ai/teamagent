//! Model-aware backend-group routing (PURE).
//!
//! An inbound Anthropic Messages request names a `model`; this module maps
//! that name to a [`BackendGroup`] — the pool of accounts that can serve it.
//! Two groups exist: [`BackendGroup::Claude`] (oauth + apikey accounts,
//! served by the Anthropic provider) and [`BackendGroup::Codex`] (chatgpt
//! oauth accounts, served by the codex provider). The scheduler then picks
//! the best eligible account *within* that group, sticky per group.
//!
//! Everything here is a deterministic function of its inputs — no IO, no
//! clock, no shared state — so it is unit-test heavy by design. The
//! classifier is built once from config (or the builtin defaults) and shared
//! read-only behind an `Arc`.

/// Which backend pool an account belongs to / a model routes to.
///
/// `Ord` is derived so the group can key a `BTreeMap` (per-group stickiness)
/// with a stable, total order: `Claude < Codex`. That order also makes
/// `Claude` the representative group when a scalar must be chosen (status
/// output picks the claude slot first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BackendGroup {
    Claude,
    Codex,
}

impl BackendGroup {
    /// Group an account belongs to, derived from its credential `kind`
    /// (`"oauth" | "apikey" | "codex"` — see [`crate::config::AccountCredential::kind`]).
    /// Codex credentials are the Codex group; everything else is Claude.
    pub fn from_kind(kind: &str) -> Self {
        match kind {
            "codex" => Self::Codex,
            _ => Self::Claude,
        }
    }

    /// Lowercase label for logs / status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl std::fmt::Display for BackendGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One model-matching rule. A model string (already lowercased) matches when
/// it satisfies the `kind`; the first matching rule (config order, then
/// builtins) decides the group.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// Matches when the model starts with this (lowercase) prefix.
    Prefix(String),
    /// Matches when the model contains this (lowercase) substring.
    Substring(String),
    /// Matches the model exactly (lowercase).
    Exact(String),
}

impl Rule {
    fn matches(&self, model_lower: &str) -> bool {
        match self {
            Rule::Prefix(p) => model_lower.starts_with(p.as_str()),
            Rule::Substring(s) => model_lower.contains(s.as_str()),
            Rule::Exact(e) => model_lower == e.as_str(),
        }
    }

    /// Parse a config rule token. `"codex"`-style bare words are substrings;
    /// `*`/wildcards are not supported — a token is treated as a PREFIX rule
    /// unless it is wrapped: `~substr` → substring, `=exact` → exact. This
    /// keeps the common config case (`"claude-"`, `"gpt-"`) a simple prefix
    /// while still allowing the two builtin special cases to be expressed.
    fn parse(token: &str) -> Self {
        let token = token.trim().to_ascii_lowercase();
        if let Some(rest) = token.strip_prefix('~') {
            Rule::Substring(rest.to_string())
        } else if let Some(rest) = token.strip_prefix('=') {
            Rule::Exact(rest.to_string())
        } else {
            Rule::Prefix(token)
        }
    }
}

/// Builtin codex rules: `gpt-` / `o1`-`o4` prefixes, `codex` substring, and
/// the exact `gpt-5.5` (covered by `gpt-` already, but kept explicit so the
/// intent survives a future prefix change).
fn builtin_codex_rules() -> Vec<Rule> {
    vec![
        Rule::Prefix("gpt-".to_string()),
        Rule::Prefix("o1".to_string()),
        Rule::Prefix("o3".to_string()),
        Rule::Prefix("o4".to_string()),
        Rule::Substring("codex".to_string()),
        Rule::Exact("gpt-5.5".to_string()),
    ]
}

/// Builtin claude rules: the Anthropic model families plus the fable alias.
fn builtin_claude_rules() -> Vec<Rule> {
    vec![
        Rule::Prefix("claude".to_string()),
        Rule::Prefix("opus".to_string()),
        Rule::Prefix("sonnet".to_string()),
        Rule::Prefix("haiku".to_string()),
        Rule::Prefix("fable".to_string()),
    ]
}

/// Compiled model→group classifier. First-match-wins over the codex rules
/// then the claude rules; an unmatched (or absent) model falls back to
/// `default_group`. Built from config overrides when present, else builtins.
#[derive(Debug, Clone)]
pub struct Classifier {
    codex_rules: Vec<Rule>,
    claude_rules: Vec<Rule>,
    default_group: BackendGroup,
}

impl Default for Classifier {
    /// Builtin defaults: codex = `gpt-`/`o1`/`o3`/`o4` prefixes + `codex`
    /// substring + exact `gpt-5.5`; claude = the Anthropic families; fallback
    /// = Claude.
    fn default() -> Self {
        Self {
            codex_rules: builtin_codex_rules(),
            claude_rules: builtin_claude_rules(),
            default_group: BackendGroup::Claude,
        }
    }
}

impl Classifier {
    /// Build from config-supplied model lists. An EMPTY list for a group
    /// keeps that group's builtin rules (so partial config doesn't silently
    /// drop a whole family); a non-empty list REPLACES the builtins for that
    /// group (config override beats builtin). `default_group` is parsed from
    /// the config string (`"codex"` → Codex, anything else → Claude).
    pub fn from_config(
        claude_models: &[String],
        codex_models: &[String],
        default_group: &str,
    ) -> Self {
        let claude_rules = if claude_models.is_empty() {
            builtin_claude_rules()
        } else {
            claude_models.iter().map(|m| Rule::parse(m)).collect()
        };
        let codex_rules = if codex_models.is_empty() {
            builtin_codex_rules()
        } else {
            codex_models.iter().map(|m| Rule::parse(m)).collect()
        };
        Self {
            codex_rules,
            claude_rules,
            default_group: match default_group.trim().to_ascii_lowercase().as_str() {
                "codex" => BackendGroup::Codex,
                _ => BackendGroup::Claude,
            },
        }
    }

    /// Classify a model name (case-insensitive) to a group. `None` (no model
    /// in the body) routes to the configured default group. Codex rules are
    /// checked first, then claude rules; an unrecognized model falls back to
    /// `default_group`.
    pub fn classify(&self, model: Option<&str>) -> BackendGroup {
        let Some(model) = model else {
            return self.default_group;
        };
        let lower = model.to_ascii_lowercase();
        if self.codex_rules.iter().any(|r| r.matches(&lower)) {
            return BackendGroup::Codex;
        }
        if self.claude_rules.iter().any(|r| r.matches(&lower)) {
            return BackendGroup::Claude;
        }
        self.default_group
    }
}

/// Extract the `model` field from an Anthropic Messages JSON body, if any.
/// A non-JSON body, or one with no string `model`, yields `None` — the
/// classifier then routes by the default group. This is the single source of
/// the model-extraction logic the Anthropic provider's `request_out` reuses.
pub fn model_from_body(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("model")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Classifier {
        Classifier::default()
    }

    // ---- from_kind ----

    #[test]
    fn from_kind_maps_codex_credential_to_codex_group() {
        assert_eq!(BackendGroup::from_kind("codex"), BackendGroup::Codex);
    }

    #[test]
    fn from_kind_maps_oauth_and_apikey_to_claude_group() {
        assert_eq!(BackendGroup::from_kind("oauth"), BackendGroup::Claude);
        assert_eq!(BackendGroup::from_kind("apikey"), BackendGroup::Claude);
        assert_eq!(
            BackendGroup::from_kind("anything-else"),
            BackendGroup::Claude
        );
    }

    // ---- builtin codex rules ----

    #[test]
    fn gpt_prefix_routes_to_codex() {
        assert_eq!(builtin().classify(Some("gpt-4o")), BackendGroup::Codex);
        assert_eq!(builtin().classify(Some("gpt-5")), BackendGroup::Codex);
    }

    #[test]
    fn exact_gpt_5_5_routes_to_codex() {
        assert_eq!(builtin().classify(Some("gpt-5.5")), BackendGroup::Codex);
    }

    #[test]
    fn o_series_prefixes_route_to_codex() {
        assert_eq!(builtin().classify(Some("o1")), BackendGroup::Codex);
        assert_eq!(builtin().classify(Some("o1-mini")), BackendGroup::Codex);
        assert_eq!(builtin().classify(Some("o3")), BackendGroup::Codex);
        assert_eq!(builtin().classify(Some("o3-pro")), BackendGroup::Codex);
        assert_eq!(builtin().classify(Some("o4-mini")), BackendGroup::Codex);
    }

    #[test]
    fn codex_substring_routes_to_codex() {
        assert_eq!(builtin().classify(Some("codex")), BackendGroup::Codex);
        assert_eq!(
            builtin().classify(Some("some-codex-model")),
            BackendGroup::Codex
        );
    }

    // ---- builtin claude rules ----

    #[test]
    fn claude_families_route_to_claude() {
        assert_eq!(
            builtin().classify(Some("claude-sonnet-4-5")),
            BackendGroup::Claude
        );
        assert_eq!(builtin().classify(Some("opus")), BackendGroup::Claude);
        assert_eq!(builtin().classify(Some("opus-4-1")), BackendGroup::Claude);
        assert_eq!(builtin().classify(Some("sonnet")), BackendGroup::Claude);
        assert_eq!(builtin().classify(Some("haiku")), BackendGroup::Claude);
        assert_eq!(builtin().classify(Some("fable-5")), BackendGroup::Claude);
    }

    // ---- case-insensitivity ----

    #[test]
    fn classification_is_case_insensitive() {
        assert_eq!(builtin().classify(Some("GPT-5.5")), BackendGroup::Codex);
        assert_eq!(builtin().classify(Some("Gpt-4O")), BackendGroup::Codex);
        assert_eq!(builtin().classify(Some("OPUS")), BackendGroup::Claude);
        assert_eq!(
            builtin().classify(Some("Claude-Sonnet-4-5")),
            BackendGroup::Claude
        );
        assert_eq!(builtin().classify(Some("CODEX")), BackendGroup::Codex);
    }

    // ---- None / unknown → default fallback ----

    #[test]
    fn none_model_routes_to_default_group() {
        assert_eq!(builtin().classify(None), BackendGroup::Claude);
    }

    #[test]
    fn unknown_model_falls_back_to_claude() {
        assert_eq!(builtin().classify(Some("llama-3")), BackendGroup::Claude);
        assert_eq!(
            builtin().classify(Some("mistral-large")),
            BackendGroup::Claude
        );
        assert_eq!(builtin().classify(Some("")), BackendGroup::Claude);
    }

    // ---- first-match-wins (codex checked before claude) ----

    #[test]
    fn codex_rule_wins_when_both_could_match() {
        // A contrived name containing both a codex substring and a claude
        // prefix: codex rules are evaluated first, so it routes to codex.
        assert_eq!(
            builtin().classify(Some("claude-codex-hybrid")),
            BackendGroup::Codex,
            "codex substring is matched before the claude prefix"
        );
    }

    // ---- config override beats builtin ----

    #[test]
    fn config_codex_list_replaces_builtin() {
        // Config says ONLY "wizard-" is codex; gpt-5.5 is no longer codex.
        let c = Classifier::from_config(&[], &["wizard-".to_string()], "claude");
        assert_eq!(c.classify(Some("wizard-7b")), BackendGroup::Codex);
        assert_eq!(
            c.classify(Some("gpt-5.5")),
            BackendGroup::Claude,
            "builtin gpt- rule dropped when config provides its own codex list"
        );
    }

    #[test]
    fn config_claude_list_replaces_builtin() {
        let c = Classifier::from_config(&["acme-".to_string()], &[], "claude");
        assert_eq!(c.classify(Some("acme-1")), BackendGroup::Claude);
        // opus is no longer a claude model under the override; with no codex
        // match it falls back to the default group (claude).
        assert_eq!(c.classify(Some("opus")), BackendGroup::Claude);
        // gpt-5.5 still matches the builtin codex list (codex list empty →
        // builtins kept).
        assert_eq!(c.classify(Some("gpt-5.5")), BackendGroup::Codex);
    }

    #[test]
    fn config_can_move_a_model_across_groups() {
        // Make "opus" a CODEX model via config — config override wins over
        // the builtin claude prefix.
        let c = Classifier::from_config(&[], &["=opus".to_string()], "claude");
        assert_eq!(c.classify(Some("opus")), BackendGroup::Codex);
    }

    #[test]
    fn config_substring_and_exact_tokens_parse() {
        let c = Classifier::from_config(
            &[],
            &["~special".to_string(), "=exact-model".to_string()],
            "claude",
        );
        assert_eq!(c.classify(Some("my-special-build")), BackendGroup::Codex);
        assert_eq!(c.classify(Some("exact-model")), BackendGroup::Codex);
        assert_eq!(
            c.classify(Some("exact-model-2")),
            BackendGroup::Claude,
            "exact rule does not match a longer string"
        );
    }

    #[test]
    fn config_default_group_codex_changes_fallback() {
        let c = Classifier::from_config(&[], &[], "codex");
        assert_eq!(
            c.classify(None),
            BackendGroup::Codex,
            "absent model routes to the configured default"
        );
        assert_eq!(
            c.classify(Some("llama-3")),
            BackendGroup::Codex,
            "unknown model routes to the configured default"
        );
        // Explicit matches still win over the default.
        assert_eq!(c.classify(Some("opus")), BackendGroup::Claude);
    }

    // ---- model_from_body ----

    #[test]
    fn model_from_body_extracts_string_model() {
        assert_eq!(
            model_from_body(br#"{"model":"gpt-5.5","messages":[]}"#).as_deref(),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn model_from_body_tolerates_missing_or_non_string_model() {
        assert_eq!(model_from_body(br#"{"messages":[]}"#), None);
        assert_eq!(model_from_body(br#"{"model":123}"#), None);
    }

    #[test]
    fn model_from_body_tolerates_non_json() {
        assert_eq!(model_from_body(b"not json at all"), None);
        assert_eq!(model_from_body(b""), None);
    }
}
