import AppKit
import Darwin
import SwiftUI

/// Owns the floating-island window and starts the accounts model. Stripped of
/// agent-island's Sparkle / Mixpanel / session-monitor / hook machinery — only
/// the island shell remains, driven by the llmux HTTP API. Settings live in the
/// in-island ☰ menu (no separate window).
@MainActor
class AppDelegate: NSObject, NSApplicationDelegate {
    static var shared: AppDelegate?

    private var windowManager: WindowManager?
    private var screenObserver: ScreenObserver?

    var windowController: NotchWindowController? {
        windowManager?.windowController
    }

    override init() {
        super.init()
        AppDelegate.shared = self
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApplication.shared.setActivationPolicy(.accessory)

        windowManager = WindowManager()
        _ = windowManager?.setupNotchWindow()

        screenObserver = ScreenObserver { [weak self] in
            _ = self?.windowManager?.setupNotchWindow()
        }

        Task { @MainActor in
            // Make the app self-sufficient: start the local llmux daemon in the
            // background if it isn't already running, then begin polling it.
            await DaemonLauncher.ensureRunning()
            IslandUsageModel.shared.start()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        screenObserver = nil
    }

    @MainActor
    func requestTerminateFromMenu() {
        NSApplication.shared.terminate(nil)
        // Some non-activating panel states can swallow the regular terminate
        // flow; keep a short fallback so "Quit" always exits.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
            if NSApplication.shared.isRunning {
                Darwin.exit(0)
            }
        }
    }
}
