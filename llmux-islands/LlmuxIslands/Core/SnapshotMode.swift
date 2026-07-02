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

import AppKit
import SwiftUI

@MainActor
enum SnapshotMode {
    /// Jump-cycle phases rendered per counts-state, in filename order p0..p3.
    static let phases: [Double] = [0, 0.25, 0.5, 0.75]

    enum SnapshotError: Error, CustomStringConvertible {
        case renderFailed(phase: Double)
        case pngEncodeFailed(String)

        var description: String {
            switch self {
            case .renderFailed(let phase):
                return "ImageRenderer produced no image at phase \(phase)"
            case .pngEncodeFailed(let file):
                return "PNG encoding failed for \(file)"
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

    /// Render every phase for the current (env-forced) counts. Returns the
    /// absolute paths written, in phase order.
    static func renderAll(into dir: URL) throws -> [String] {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let claude = DemoMode.forcedInFlight?.claude ?? 0
        let codex = DemoMode.forcedInFlight?.codex ?? 0

        var written: [String] = []
        for (index, phase) in phases.enumerated() {
            let view = ClosedIslandSnapshotView(claudeCount: claude, codexCount: codex, phase: phase)
            let renderer = ImageRenderer(content: view)
            renderer.scale = 2
            guard let cgImage = renderer.cgImage else {
                throw SnapshotError.renderFailed(phase: phase)
            }
            let url = dir.appendingPathComponent("label-c\(claude)x\(codex)-p\(index).png")
            try writePNG(cgImage, to: url)
            written.append(url.path)
        }
        return written
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
    let claudeCount: Int
    let codexCount: Int
    /// 0..<1 position within the jump cycle; the rainbow hue uses the same phase.
    let phase: Double

    /// Non-notch fallback island size (Ext+NSScreen.notchSize fallback).
    private static let closedNotchSize = CGSize(width: 224, height: 38)

    var body: some View {
        NotchClosedLabelContent(
            claudeCount: claudeCount,
            codexCount: codexCount,
            jumpOffset: NotchClosedLabelView.jumpOffset(phase: phase, claudeSessions: claudeCount),
            claudeHue: NotchClosedLabelView.rainbowHue(phase: phase, seed: NotchClosedLabelView.claudeHueSeed),
            codexHue: NotchClosedLabelView.rainbowHue(phase: phase, seed: NotchClosedLabelView.codexHueSeed)
        )
        .frame(minWidth: Self.closedNotchSize.width - 20)
        .frame(height: max(24, Self.closedNotchSize.height))
        .padding(.horizontal, 14)
        .background(.black)
        .clipShape(NotchShape(topCornerRadius: 6, bottomCornerRadius: 14))
        .environment(\.colorScheme, .dark)
    }
}
