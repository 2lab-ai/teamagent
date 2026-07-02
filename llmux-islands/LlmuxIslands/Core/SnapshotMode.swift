//
//  SnapshotMode.swift
//  LlmuxIslands
//
//  Offscreen snapshot mode for visual verification on hosts without Screen
//  Recording permission and without disturbing a running production island.
//  When `LLMUX_ISLANDS_SNAPSHOT_DIR=<abs dir>` is set at launch the app
//  creates NO window and never touches the llmux daemon — it renders the
//  requested PNGs (2x), prints the written paths to stdout, and exits 0.
//  When the variable is absent this is a strict no-op and the app launches
//  normally.
//
//  Dispatch rule (one gate, two artifact families):
//  - `LLMUX_ISLANDS_SNAPSHOT_KIND=label|menu|usage` selects explicitly.
//  - When KIND is unset: the **label** family renders if a label-ish env is
//    present (`LLMUX_ISLANDS_DEMO_INFLIGHT` or `LLMUX_ISLANDS_SNAPSHOT_T`);
//    otherwise the **menu + usage** family renders.
//
//  Label family — the closed-island pill (NotchClosedLabelContent) at 4 fixed
//  animation phases. Session counts come from `LLMUX_ISLANDS_DEMO_INFLIGHT`
//  (DemoMode); relaunch once per counts-state. Output:
//  `label-c{claude}x{codex}-p{0..3}.png` where p0..p3 = phase 0 / 0.25 / 0.5 /
//  0.75 of the jump cycle (the rainbow hue advances with the same phases).
//  Wall-clock mode: setting `LLMUX_ISLANDS_SNAPSHOT_T=<seconds>` renders ONE
//  frame at that absolute time instead of the 4 normalized phases, mapped
//  through the app's real count→period function (`jumpOffset(time:)` /
//  `rainbowHue(time:)` — the phase is computed inside jumpPeriod, never
//  precomputed here), so jump SPEED scaling across counts is exercisable:
//  the same t lands on different cycle phases for different counts. Output:
//  `t{t*100 as %03d}-c{claude}.png` (e.g. t=0.3, claude=3 → `t030-c3.png`).
//
//  Menu + usage family — the ☰ menu (`menu.png`) and the Usage panel with
//  fixture accounts carrying demo fake emails (`usage-anon-on.png` /
//  `usage-anon-off.png`, named after the effective email-anonymous state).
//  Force the state per-process WITHOUT touching the shared defaults domain by
//  launching with `-emailAnonymousEnabled YES` / `NO` (volatile argument
//  domain); run the binary twice to get both usage states.
//

import AppKit
import SwiftUI

enum SnapshotMode {
    /// Output directory from the environment; snapshot mode is active iff set.
    static let directory: String? = {
        guard let dir = ProcessInfo.processInfo.environment["LLMUX_ISLANDS_SNAPSHOT_DIR"],
              !dir.isEmpty
        else { return nil }
        return dir
    }()

    /// Read from nonisolated view code (EmailPixelized precomputes its mosaic
    /// synchronously in snapshot mode) — keep this enum nonisolated and put
    /// `@MainActor` on the render functions instead.
    static var isActive: Bool { directory != nil }

    /// PNG scale factor (2x, Retina-like).
    static let scale: CGFloat = 2

    /// Jump-cycle phases rendered per counts-state, in filename order p0..p3.
    static let phases: [Double] = [0, 0.25, 0.5, 0.75]

    /// Artifact families selectable via `LLMUX_ISLANDS_SNAPSHOT_KIND`.
    enum Kind: String {
        case label
        case menu
        case usage
    }

    enum SnapshotError: Error, CustomStringConvertible {
        case renderFailed(String)
        case pngEncodeFailed(String)
        case invalidWallClock(String)
        case invalidKind(String)

        var description: String {
            switch self {
            case .renderFailed(let file):
                return "renderer produced no image for \(file)"
            case .pngEncodeFailed(let file):
                return "PNG encoding failed for \(file)"
            case .invalidWallClock(let raw):
                return "LLMUX_ISLANDS_SNAPSHOT_T must be a non-negative number of seconds, got \"\(raw)\""
            case .invalidKind(let raw):
                return "LLMUX_ISLANDS_SNAPSHOT_KIND must be label|menu|usage, got \"\(raw)\""
            }
        }
    }

    /// Called first thing in `applicationDidFinishLaunching`, before any
    /// window or daemon work. Returns immediately (no side effects) when
    /// `LLMUX_ISLANDS_SNAPSHOT_DIR` is unset; otherwise writes the PNGs and
    /// terminates the process (exit 0 on success, 1 on failure).
    @MainActor
    static func runIfRequested() {
        guard let dir = directory else { return }

        do {
            let paths = try renderAll(into: URL(fileURLWithPath: dir, isDirectory: true))
            for path in paths {
                print(path)
            }
            exit(0)
        } catch {
            FileHandle.standardError.write(Data("snapshot mode failed: \(error)\n".utf8))
            exit(1)
        }
    }

    /// Render the artifacts selected by the dispatch rule (file header) and
    /// return the absolute paths written.
    @MainActor
    static func renderAll(into dir: URL) throws -> [String] {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)

        let kinds = try requestedKinds()
        if kinds.contains(.menu) || kinds.contains(.usage) {
            normalizeEmailAnonymousLaunchArgument()
        }

        var written: [String] = []
        for kind in kinds {
            switch kind {
            case .label: written += try renderLabelFrames(into: dir)
            case .menu: written += try renderMenu(into: dir)
            case .usage: written += try renderUsage(into: dir)
            }
        }
        return written
    }

    /// The dispatch rule: explicit KIND wins; otherwise label-ish envs select
    /// the label family, and the menu + usage family is the default.
    static func requestedKinds(
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) throws -> [Kind] {
        if let raw = environment["LLMUX_ISLANDS_SNAPSHOT_KIND"], !raw.isEmpty {
            guard let kind = Kind(rawValue: raw.lowercased()) else {
                throw SnapshotError.invalidKind(raw)
            }
            return [kind]
        }
        let labelish = ["LLMUX_ISLANDS_DEMO_INFLIGHT", "LLMUX_ISLANDS_SNAPSHOT_T"]
        if labelish.contains(where: { environment[$0]?.isEmpty == false }) {
            return [.label]
        }
        return [.menu, .usage]
    }

    // MARK: - Label family (closed-island pill)

    /// Render the requested label frames for the current (env-forced) counts:
    /// one wall-clock frame when `LLMUX_ISLANDS_SNAPSHOT_T` is set, else the
    /// 4 fixed phases.
    @MainActor
    private static func renderLabelFrames(into dir: URL) throws -> [String] {
        let claude = DemoMode.forcedInFlight?.claude ?? 0
        let codex = DemoMode.forcedInFlight?.codex ?? 0

        if let raw = ProcessInfo.processInfo.environment["LLMUX_ISLANDS_SNAPSHOT_T"], !raw.isEmpty {
            guard let t = TimeInterval(raw), t >= 0, t.isFinite else {
                throw SnapshotError.invalidWallClock(raw)
            }
            let name = String(format: "t%03d-c%d.png", Int((t * 100).rounded()), claude)
            let url = dir.appendingPathComponent(name)
            let view = ClosedIslandSnapshotView(claudeCount: claude, codexCount: codex, clock: .wallClock(t))
            try renderLabel(view, to: url)
            return [url.path]
        }

        var written: [String] = []
        for (index, phase) in phases.enumerated() {
            let url = dir.appendingPathComponent("label-c\(claude)x\(codex)-p\(index).png")
            let view = ClosedIslandSnapshotView(claudeCount: claude, codexCount: codex, clock: .phase(phase))
            try renderLabel(view, to: url)
            written.append(url.path)
        }
        return written
    }

    @MainActor
    private static func renderLabel(_ view: ClosedIslandSnapshotView, to url: URL) throws {
        let renderer = ImageRenderer(content: view)
        renderer.scale = scale
        guard let cgImage = renderer.cgImage else {
            throw SnapshotError.renderFailed(url.lastPathComponent)
        }
        try writePNG(NSBitmapImageRep(cgImage: cgImage), to: url)
    }

    // MARK: - Menu + usage family (opened panels)

    /// The ☰ menu as composed in the app (includes the "Email anonymous" row).
    @MainActor
    private static func renderMenu(into dir: URL) throws -> [String] {
        let viewModel = makeViewModel()
        viewModel.contentType = .menu
        let url = dir.appendingPathComponent("menu.png")
        try writeHosted(view: NotchMenuView(viewModel: viewModel), size: viewModel.openedSize, to: url)
        return [url.path]
    }

    /// The Usage panel (account tile grid), named after the effective
    /// email-anonymous state.
    @MainActor
    private static func renderUsage(into dir: URL) throws -> [String] {
        // Deterministic fixture accounts with demo fake emails — no daemon
        // dependency, and the production daemon is never queried.
        let model = IslandUsageModel.shared
        model.tiles = fixtureTiles()
        model.connection = .online

        let viewModel = makeViewModel()
        viewModel.contentType = .usage
        let anonOn = AppSettings.emailAnonymousEnabled
        let url = dir.appendingPathComponent(anonOn ? "usage-anon-on.png" : "usage-anon-off.png")
        try writeHosted(view: IslandUsageView(model: model, viewModel: viewModel), size: viewModel.openedSize, to: url)
        return [url.path]
    }

    /// Plausible 16" laptop geometry so `openedSize` matches the app.
    @MainActor
    private static func makeViewModel() -> NotchViewModel {
        NotchViewModel(
            deviceNotchRect: CGRect(x: 764, y: 1085, width: 200, height: 32),
            screenRect: CGRect(x: 0, y: 0, width: 1728, height: 1117),
            windowHeight: 800,
            hasPhysicalNotch: true
        )
    }

    /// `-emailAnonymousEnabled YES/NO` lands in the volatile argument domain
    /// as a String; `bool(forKey:)` coerces it (so AppSettings reads it fine)
    /// but SwiftUI's @AppStorage does a strict Bool cast and would silently
    /// fall back to its default. Re-write the coerced value into the same
    /// volatile domain as a typed Bool so the views see it too. Volatile =
    /// per-process; the user's persisted defaults are never touched.
    private static func normalizeEmailAnonymousLaunchArgument() {
        let value = AppSettings.emailAnonymousEnabled
        var argumentDomain = UserDefaults.standard.volatileDomain(forName: UserDefaults.argumentDomain)
        argumentDomain[AppSettings.emailAnonymousEnabledKey] = value
        UserDefaults.standard.setVolatileDomain(argumentDomain, forName: UserDefaults.argumentDomain)
    }

    /// Render `view` at `size` (island content on the island's black backdrop)
    /// into a PNG at `url`.
    ///
    /// Uses an offscreen `NSHostingView` + `cacheDisplay`, not `ImageRenderer`:
    /// `ImageRenderer` cannot rasterize AppKit-backed children, so everything
    /// inside a `ScrollView` (the whole ☰ menu, the usage tile grid) comes out
    /// blank (verified 2026-07-02). The hosting view draws the real hierarchy
    /// straight into a bitmap — no window is created.
    @MainActor
    private static func writeHosted(view: some View, size: CGSize, to url: URL) throws {
        let host = NSHostingView(rootView:
            view
                .frame(width: size.width, height: size.height)
                .background(Color.black)
                .environment(\.colorScheme, .dark)
        )
        host.frame = CGRect(origin: .zero, size: size)
        host.layoutSubtreeIfNeeded()

        guard let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil,
            pixelsWide: Int(size.width * scale),
            pixelsHigh: Int(size.height * scale),
            bitsPerSample: 8,
            samplesPerPixel: 4,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: .calibratedRGB,
            bytesPerRow: 0,
            bitsPerPixel: 0
        ) else {
            throw SnapshotError.renderFailed(url.lastPathComponent)
        }
        rep.size = size
        host.cacheDisplay(in: host.bounds, to: rep)
        try writePNG(rep, to: url)
    }

    /// Single PNG encoder for both families.
    private static func writePNG(_ rep: NSBitmapImageRep, to url: URL) throws {
        guard let data = rep.representation(using: .png, properties: [:]) else {
            throw SnapshotError.pngEncodeFailed(url.lastPathComponent)
        }
        try data.write(to: url)
    }

    /// Four representative accounts using the demo fake emails
    /// (`DemoMode.fakeEmail`), mirroring what `LLMUX_ISLANDS_DEMO=1` shows.
    private static func fixtureTiles() -> [UsageAccountTile] {
        let now = Date()

        func info(name: String, fiveHour: Double, sevenDay: Double) -> CLIUsageInfo {
            CLIUsageInfo(
                name: name,
                available: true,
                error: false,
                fiveHourPercent: fiveHour,
                sevenDayPercent: sevenDay,
                fiveHourReset: now.addingTimeInterval(2 * 3600 + 17 * 60),
                sevenDayReset: now.addingTimeInterval(3 * 24 * 3600 + 5 * 3600),
                model: nil,
                plan: nil,
                buckets: nil
            )
        }

        func tile(index: Int, provider: UsageProvider, tier: String?, fiveHour: Double, sevenDay: Double) -> UsageAccountTile {
            let email = DemoMode.fakeEmail(index: index)
            return UsageAccountTile(
                id: email,
                provider: provider,
                accountId: email,
                label: email,
                email: email,
                tier: tier,
                claudeIsTeam: nil,
                tokenRefresh: TokenRefreshInfo(
                    expiresAt: now.addingTimeInterval(5 * 3600),
                    lifetimeSeconds: 8 * 3600
                ),
                info: info(name: email, fiveHour: fiveHour, sevenDay: sevenDay),
                errorMessage: nil,
                issue: nil
            )
        }

        return [
            tile(index: 0, provider: .claude, tier: "max20", fiveHour: 34, sevenDay: 61),
            tile(index: 1, provider: .claude, tier: "max5", fiveHour: 78, sevenDay: 42),
            tile(index: 2, provider: .codex, tier: nil, fiveHour: 12, sevenDay: 27),
            tile(index: 3, provider: .claude, tier: "pro", fiveHour: 55, sevenDay: 88),
        ]
    }
}

/// The closed island as snapshot mode renders it: `NotchClosedLabelContent`
/// composed with the same chrome the closed `NotchView` applies (min width,
/// row height, 14pt horizontal padding, black fill, NotchShape 6/14 clip).
/// This replicates NotchView's closed-state modifiers instead of instantiating
/// NotchView itself, which requires a live NotchViewModel/window — see the
/// fidelity notes in the PR.
struct ClosedIslandSnapshotView: View {
    /// How the animation instant is specified.
    enum Clock {
        /// Fixed 0..<1 position within the jump cycle (hue uses the same phase).
        case phase(Double)
        /// Absolute wall-clock seconds, mapped through the app's real
        /// count→period function — exercises jumpPeriod's speed scaling/clamp.
        case wallClock(TimeInterval)
    }

    let claudeCount: Int
    let codexCount: Int
    let clock: Clock

    /// Non-notch fallback island size (Ext+NSScreen.notchSize fallback).
    private static let closedNotchSize = CGSize(width: 224, height: 38)

    private var jumpOffset: CGFloat {
        switch clock {
        case .phase(let phase):
            return NotchClosedLabelView.jumpOffset(phase: phase, claudeSessions: claudeCount)
        case .wallClock(let t):
            return NotchClosedLabelView.jumpOffset(time: t, claudeSessions: claudeCount)
        }
    }

    private func hue(seed: Double) -> Double {
        switch clock {
        case .phase(let phase):
            return NotchClosedLabelView.rainbowHue(phase: phase, seed: seed)
        case .wallClock(let t):
            return NotchClosedLabelView.rainbowHue(time: t, seed: seed)
        }
    }

    var body: some View {
        NotchClosedLabelContent(
            claudeCount: claudeCount,
            codexCount: codexCount,
            jumpOffset: jumpOffset,
            claudeHue: hue(seed: NotchClosedLabelView.claudeHueSeed),
            codexHue: hue(seed: NotchClosedLabelView.codexHueSeed)
        )
        .frame(minWidth: Self.closedNotchSize.width - 20)
        .frame(height: max(24, Self.closedNotchSize.height))
        .padding(.horizontal, 14)
        .background(.black)
        .clipShape(NotchShape(topCornerRadius: 6, bottomCornerRadius: 14))
        .environment(\.colorScheme, .dark)
    }
}
