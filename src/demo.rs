//! Demo mode (`LLMUX_DEMO_MODE`): replace real account identities (which are
//! emails) with deterministic, stable fake ones so a screen recording or
//! screenshot never leaks the operator's real accounts.
//!
//! The substitution happens once, at config load (`config::load*`), on the
//! account `name` — which is the display id used everywhere (table, detail,
//! current/next, activity, logs). Aliasing it at the source keeps every surface
//! consistent with zero per-render-site risk of a miss, while credentials
//! (looked up by token/uuid, never by name) keep working. Config writes are
//! suppressed in demo mode so the aliases never reach disk.
//!
//! "Stable" = the same real name always maps to the same fake one (FNV-1a hash
//! into a fixed pool), so the recording is internally coherent across frames.

/// Whether `LLMUX_DEMO_MODE` is set to an on-ish value (set + not empty / `0` /
/// `false`).
pub fn enabled() -> bool {
    match std::env::var_os("LLMUX_DEMO_MODE") {
        Some(v) => {
            let v = v.to_string_lossy();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        None => false,
    }
}

/// A fixed pool of obviously-fake-but-realistic emails. Replacing a real email
/// with one of these keeps the dashboard legible without exposing anything.
const POOL: [&str; 16] = [
    "ada.lovelace@example.com",
    "alan.turing@example.com",
    "grace.hopper@example.com",
    "katherine.j@example.com",
    "linus.t@example.com",
    "margaret.h@example.com",
    "dennis.r@example.com",
    "barbara.l@example.com",
    "john.mccarthy@example.com",
    "edsger.d@example.com",
    "claude.s@example.com",
    "donald.k@example.com",
    "rosalind.f@example.com",
    "tim.bl@example.com",
    "vint.cerf@example.com",
    "radia.p@example.com",
];

/// Deterministic display alias for `name` when demo mode is on; otherwise the
/// name unchanged. See [`alias_always`] for the mapping itself.
pub fn alias(name: &str) -> String {
    if enabled() {
        alias_always(name)
    } else {
        name.to_string()
    }
}

/// The pure mapping (no env read): a `provider:` tag (e.g. `codex:`) is kept and
/// only the email after it is replaced; a value with no `@` is returned as-is.
fn alias_always(name: &str) -> String {
    let (prefix, email) = match name.split_once(':') {
        Some((tag, rest)) if rest.contains('@') => (format!("{tag}:"), rest),
        _ => (String::new(), name),
    };
    if !email.contains('@') {
        return name.to_string();
    }
    let idx = (fnv1a(email) % POOL.len() as u64) as usize;
    format!("{prefix}{}", POOL[idx])
}

/// FNV-1a over the bytes — a small, dependency-free, deterministic hash. (Not
/// `DefaultHasher`: its output is not guaranteed stable across runs/versions,
/// and stability is the whole point here.)
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_is_stable_for_the_same_input() {
        assert_eq!(
            alias_always("info@insightquest.io"),
            alias_always("info@insightquest.io"),
            "same real name must always map to the same fake one"
        );
    }

    #[test]
    fn alias_lands_in_the_fake_pool() {
        let a = alias_always("someone@real-domain.com");
        assert!(POOL.contains(&a.as_str()), "got {a}");
        assert!(!a.contains("real-domain"), "real domain must not survive");
    }

    #[test]
    fn codex_provider_tag_is_preserved() {
        let a = alias_always("codex:chatgpt-user@gmail.com");
        assert!(a.starts_with("codex:"), "got {a}");
        let email = a.strip_prefix("codex:").unwrap();
        assert!(POOL.contains(&email), "got {a}");
    }

    #[test]
    fn non_email_names_are_left_alone() {
        assert_eq!(alias_always("my-api-key-account"), "my-api-key-account");
    }

    #[test]
    fn distinct_typical_accounts_get_distinct_aliases() {
        // The four demo accounts must read as four different people.
        let names = [
            "ai2@insightquest.io",
            "notify@insightquest.io",
            "codex:ai@insightquest.io",
            "codex:icedac@gmail.com",
        ];
        let aliased: Vec<String> = names.iter().map(|n| alias_always(n)).collect();
        let unique: std::collections::HashSet<&String> = aliased.iter().collect();
        assert_eq!(unique.len(), names.len(), "aliases collided: {aliased:?}");
    }

    #[test]
    fn enabled_parses_common_values() {
        // Pure check of the truthiness rule without mutating process env in a
        // way that races other tests: exercise the same predicate inline.
        let truthy = |v: &str| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false");
        assert!(truthy("1"));
        assert!(truthy("true"));
        assert!(!truthy("0"));
        assert!(!truthy("false"));
        assert!(!truthy(""));
    }
}
