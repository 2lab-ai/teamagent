//! Build channel + build id baked in at compile time from env (herdr pattern).

/// Release channel this binary was built for (`dev`, `preview`, `stable`).
/// Set by CI via `LLMUX_BUILD_CHANNEL`; defaults to `dev` for local builds.
pub const BUILD_CHANNEL: &str = match option_env!("LLMUX_BUILD_CHANNEL") {
    Some(channel) => channel,
    None => "dev",
};

/// Unique build identifier (e.g. `preview-20260612-abc1234`).
/// Set by CI via `LLMUX_BUILD_ID`; defaults to `dev` for local builds.
pub const BUILD_ID: &str = match option_env!("LLMUX_BUILD_ID") {
    Some(id) => id,
    None => "dev",
};

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version + channel/build id, without the binary name — what clap appends
/// after the command name for `--version`.
pub fn version_with_build() -> String {
    format!("{VERSION} ({BUILD_CHANNEL} {BUILD_ID})")
}

/// Human-readable version line for `--version` and `/llmux/status`.
pub fn version_string() -> String {
    format!("llmux {}", version_with_build())
}

#[cfg(test)]
mod tests {
    use super::*;

    // DIST-10: `version_with_build` is exactly "<version> (<channel> <id>)".
    #[test]
    fn version_with_build_formats_version_channel_and_id() {
        assert_eq!(
            version_with_build(),
            format!("{VERSION} ({BUILD_CHANNEL} {BUILD_ID})")
        );
    }

    // DIST-11: `version_string` is the build line prefixed with "llmux ".
    #[test]
    fn version_string_is_llmux_prefixed_build_line() {
        let s = version_string();
        assert!(s.starts_with("llmux "), "got {s:?}");
        assert!(s.ends_with(&version_with_build()), "got {s:?}");
    }
}
