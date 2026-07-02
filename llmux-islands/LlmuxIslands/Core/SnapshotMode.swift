//
//  SnapshotMode.swift
//  LlmuxIslands
//
//  Offscreen snapshot mode for visual verification on hosts without Screen
//  Recording permission. When `LLMUX_ISLANDS_SNAPSHOT_DIR=<abs dir>` is set at
//  launch, the app renders the closed-island pill (with NotchClosedLabelContent
//  inside) to 2x PNGs at 4 fixed animation phases, prints the written file
//  paths to stdout and exits 0 — it creates no window and never touches the
//  llmux daemon. When the variable is absent this is a strict no-op and the
//  app launches normally.
//
//  Session counts come from `LLMUX_ISLANDS_DEMO_INFLIGHT` (DemoMode); relaunch
//  once per counts-state. Output: `label-c{claude}x{codex}-p{0..3}.png` where
//  p0..p3 = phase 0 / 0.25 / 0.5 / 0.75 of the jump cycle (the rainbow hue
//  advances with the same phases).
//
//  Wall-clock mode: setting `LLMUX_ISLANDS_SNAPSHOT_T=<seconds>` additionally
//  renders ONE frame at that absolute time instead of the 4 normalized phases,
//  mapped through the app's real count→period function (`jumpOffset(time:)` /
//  `rainbowHue(time:)` — the phase is computed inside jumpPeriod, never
//  precomputed here), so jump SPEED scaling across counts is exercisable:
//  the same t lands on different cycle phases for different counts. Output:
//  `t{t*100 as %03d}-c{claude}.png` (e.g. t=0.3, claude=3 → `t030-c3.png`).
//

import AppKit
import SwiftUI

@MainActor
enum SnapshotMode {
    /// Jump-cycle phases rendered per counts-state, in filename order p0..p3.
    static let phases: [Double] = [0, 0.25, 0.5, 0.75]

    enum SnapshotError: Error, CustomStringConvertible {
        case renderFailed(String)
        case pngEncodeFailed(String)
        case invalidWallClock(String)

        var description: String {
            switch self {
            case .renderFailed(let file):
                return "ImageRenderer produced no image for \(file)"
            case .pngEncodeFailed(let file):
                return "PNG encoding failed for \(file)"
            case .invalidWallClock(let raw):
                return "LLMUX_ISLANDS_SNAPSHOT_T must be a non-negative number of seconds, got \"\(raw)\""
            }
        }
    }

    /// Called first thing in `applicationDidFinishLaunching`, before any
    /// window or daemon work. Returns immediately (no side effects) when
    /// `LLMUX_ISLANDS_SNAPSHOT_DIR` is unset; otherwise writes the PNGs and
    /// terminates the process (exit 0 on success, 1 on failure).
    static func runIfRequested() {
        guard let dir = ProcessInfo.processInfo.environment["LLMUX_ISLANDS_SNAPSHOT_DIR"],
              !dir.isEmpty
        else { return }

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

    /// Render the requested frames for the current (env-forced) counts and
    /// return the absolute paths written: one wall-clock frame when
    /// `LLMUX_ISLANDS_SNAPSHOT_T` is set, else the 4 fixed phases.
    static func renderAll(into dir: URL) throws -> [String] {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let claude = DemoMode.forcedInFlight?.claude ?? 0
        let codex = DemoMode.forcedInFlight?.codex ?? 0

        if let raw = ProcessInfo.processInfo.environment["LLMUX_ISLANDS_SNAPSHOT_T"], !raw.isEmpty {
            guard let t = TimeInterval(raw), t >= 0, t.isFinite else {
                throw SnapshotError.invalidWallClock(raw)
            }
            let name = String(format: "t%03d-c%d.png", Int((t * 100).rounded()), claude)
            let url = dir.appendingPathComponent(name)
            let view = ClosedIslandSnapshotView(claudeCount: claude, codexCount: codex, clock: .wallClock(t))
            try render(view, to: url)
            return [url.path]
        }

        var written: [String] = []
        for (index, phase) in phases.enumerated() {
            let url = dir.appendingPathComponent("label-c\(claude)x\(codex)-p\(index).png")
            let view = ClosedIslandSnapshotView(claudeCount: claude, codexCount: codex, clock: .phase(phase))
            try render(view, to: url)
            written.append(url.path)
        }
        return written
    }

    private static func render(_ view: ClosedIslandSnapshotView, to url: URL) throws {
        let renderer = ImageRenderer(content: view)
        renderer.scale = 2
        guard let cgImage = renderer.cgImage else {
            throw SnapshotError.renderFailed(url.lastPathComponent)
        }
        try writePNG(cgImage, to: url)
    }

    private static func writePNG(_ image: CGImage, to url: URL) throws {
        let rep = NSBitmapImageRep(cgImage: image)
        guard let data = rep.representation(using: .png, properties: [:]) else {
            throw SnapshotError.pngEncodeFailed(url.lastPathComponent)
        }
        try data.write(to: url)
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
