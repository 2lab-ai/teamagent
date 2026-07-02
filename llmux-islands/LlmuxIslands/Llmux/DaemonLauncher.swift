import Foundation

/// Makes the app self-sufficient: right after install, launching LlmuxIslands is
/// enough — if the local llmux daemon isn't already running, we start it in the
/// background so the user never has to run `llmux server` by hand.
///
/// Only a LOCAL (loopback) daemon is managed. If the user has pointed the app at
/// a remote llmux via settings, we never spawn a local server.
enum DaemonLauncher {
    /// Probe the configured daemon; if it's local and unreachable, spawn
    /// `llmux server --no-tui` detached and wait briefly for it to bind so the
    /// first status poll succeeds.
    static func ensureRunning() async {
        guard hostIsLocal() else { return }
        if await isReachable() { return }

        guard let exe = findBinary() else {
            NSLog("llmux-islands: llmux binary not found — cannot auto-start the daemon. Install llmux (brew install llmux).")
            return
        }
        spawnDetached(exe: exe)

        // The daemon binds in ~1s (even with zero accounts); poll up to ~6s so
        // the model's first refresh lands on a live server instead of "offline".
        for _ in 0..<20 {
            try? await Task.sleep(nanoseconds: 300_000_000)
            if await isReachable() { return }
        }
        NSLog("llmux-islands: spawned llmux daemon but it did not answer within ~6s (it may still be starting).")
    }

    private static func hostIsLocal() -> Bool {
        let host = LlmuxSettings.host
        return host == "127.0.0.1" || host == "localhost" || host == "::1"
    }

    private static func isReachable() async -> Bool {
        guard let url = URL(string: LlmuxClient.current().baseURL + "/llmux/status") else { return false }
        var req = URLRequest(url: url)
        req.timeoutInterval = 1.5
        guard let (_, resp) = try? await URLSession.shared.data(for: req),
              let http = resp as? HTTPURLResponse else { return false }
        return (200..<300).contains(http.statusCode)
    }

    /// GUI apps launched from Finder don't inherit the shell PATH (no
    /// /opt/homebrew/bin), so search the common install locations directly.
    private static func findBinary() -> String? {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let candidates = [
            "/opt/homebrew/bin/llmux",   // Homebrew (Apple Silicon)
            "/usr/local/bin/llmux",      // Homebrew (Intel) / manual
            "\(home)/.cargo/bin/llmux",  // cargo install
            "\(home)/.local/bin/llmux",  // manual
        ]
        return candidates.first { FileManager.default.isExecutableFile(atPath: $0) }
    }

    /// Spawn `llmux server --no-tui` fully detached via `/bin/sh -c 'nohup … &'`:
    /// the sh returns immediately and the daemon is reparented to launchd, so it
    /// survives both this helper and the app quitting. stderr is appended to the
    /// daemon's own log (same file the CLI uses).
    private static func spawnDetached(exe: String) {
        let stateDir = "\(FileManager.default.homeDirectoryForCurrentUser.path)/.local/state/llmux"
        try? FileManager.default.createDirectory(atPath: stateDir, withIntermediateDirectories: true)
        let log = "\(stateDir)/server.log"
        let cmd = "nohup \(shq(exe)) server --no-tui >> \(shq(log)) 2>&1 &"

        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/sh")
        proc.arguments = ["-c", cmd]
        do {
            try proc.run()
            NSLog("llmux-islands: starting llmux daemon (\(exe) server --no-tui)")
        } catch {
            NSLog("llmux-islands: failed to start llmux daemon: \(error.localizedDescription)")
        }
    }

    /// Single-quote a path for safe embedding in the /bin/sh command line.
    private static func shq(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }
}
