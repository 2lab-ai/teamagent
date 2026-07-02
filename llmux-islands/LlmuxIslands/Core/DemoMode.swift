import Foundation

/// Demo/recording mode for screen-recorded walkthroughs.
///
/// Active when the app is launched with `--demo` or `LLMUX_ISLANDS_DEMO=1`.
/// In this mode the island **opens itself and stays open** (so a recorder can
/// capture the usage panel without a human hovering the notch), and account
/// emails are replaced with **stable fake addresses** so a public demo GIF never
/// leaks real account names. This mirrors the CLI/daemon `LLMUX_DEMO_MODE=1`
/// behaviour that masks emails in the dashboard/status/logs.
enum DemoMode {
    static let isActive: Bool =
        CommandLine.arguments.contains("--demo")
        || ProcessInfo.processInfo.environment["LLMUX_ISLANDS_DEMO"] == "1"

    /// Stable fake email for the account at `index` (0-based) in the tile list.
    /// Deterministic per position so labels don't flicker between status polls.
    static func fakeEmail(index: Int) -> String {
        "demo-\(index + 1)@example.com"
    }

    /// Forced per-provider in-flight session counts for screenshot verification
    /// of the closed-island label (rainbow / jump states at fixed counts).
    ///
    /// Set `LLMUX_ISLANDS_DEMO_INFLIGHT="claude=3,codex=2"` to force the counts
    /// regardless of daemon state. Either key may be omitted (that provider then
    /// tracks the real daemon value). Works standalone or together with
    /// `LLMUX_ISLANDS_DEMO=1` — note that demo mode holds the island *open*, so
    /// to screenshot the closed-state label launch with only this variable set.
    struct ForcedInFlight: Equatable {
        var claude: Int?
        var codex: Int?
    }

    static let forcedInFlight: ForcedInFlight? =
        parseForcedInFlight(ProcessInfo.processInfo.environment["LLMUX_ISLANDS_DEMO_INFLIGHT"])

    /// Parse `"claude=3,codex=2"` (order-free, whitespace-tolerant, unknown keys
    /// ignored, negative values rejected). Returns nil when no valid key is found.
    static func parseForcedInFlight(_ raw: String?) -> ForcedInFlight? {
        guard let raw, !raw.isEmpty else { return nil }
        var forced = ForcedInFlight()
        for part in raw.split(separator: ",") {
            let pair = part.split(separator: "=", maxSplits: 1)
            guard pair.count == 2,
                  let value = Int(pair[1].trimmingCharacters(in: .whitespaces)),
                  value >= 0
            else { continue }
            switch pair[0].trimmingCharacters(in: .whitespaces).lowercased() {
            case "claude": forced.claude = value
            case "codex": forced.codex = value
            default: break
            }
        }
        guard forced.claude != nil || forced.codex != nil else { return nil }
        return forced
    }
}
