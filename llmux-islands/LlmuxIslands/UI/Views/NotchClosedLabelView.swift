//
//  NotchClosedLabelView.swift
//  LlmuxIslands
//
//  Closed-island label (todo.md items 1–2). Replaces the plain black box with
//
//      Llmux Islands [mascot] [claude]{n} [codex]{m}
//
//  - `[mascot]` is the existing pixel-art ClaudeCrabIcon (the app's top-left
//    element), looping a small vertical jump whose speed scales with the Claude
//    session count: 1 = normal, faster from 2, very fast at 10, clamped past 10.
//  - A provider group `[icon]{count}` is hidden entirely while its count is 0;
//    at ≥1 it cycles through rainbow hues in a continuous loop.
//
//  Layout/colors live in `NotchClosedLabelContent`, a pure function of counts
//  and animation phases — the live view drives it from a TimelineView clock;
//  offscreen snapshot mode (SnapshotMode.swift) renders it at fixed phases.
//

import Foundation
import SwiftUI

struct NotchClosedLabelView: View {
    /// Σ in-flight sessions over Claude accounts — drives `{n}` and the jump.
    let claudeCount: Int
    /// Σ in-flight sessions over Codex accounts — drives `{m}`.
    let codexCount: Int

    /// Full rainbow revolution takes this long.
    private static let rainbowLoopSeconds: Double = 3.0
    /// Jump cycle duration at 1 Claude session (normal speed).
    private static let slowestJumpPeriod: Double = 1.2
    /// Jump cycle duration at ≥10 Claude sessions (very fast, clamped).
    private static let fastestJumpPeriod: Double = 0.25
    /// Leading fraction of the jump cycle spent airborne (rest = on the ground).
    private static let airborneFraction: Double = 0.6
    /// How high the mascot hops, in points ("살짝").
    private static let jumpHeight: CGFloat = 3

    /// Hue offsets so the two providers don't share the exact same color.
    static let claudeHueSeed: Double = 0
    static let codexHueSeed: Double = 0.35

    private var isAnimating: Bool { claudeCount > 0 || codexCount > 0 }

    var body: some View {
        TimelineView(.animation(minimumInterval: 1.0 / 30.0, paused: !isAnimating)) { timeline in
            let time = timeline.date.timeIntervalSinceReferenceDate
            NotchClosedLabelContent(
                claudeCount: claudeCount,
                codexCount: codexCount,
                jumpOffset: Self.jumpOffset(time: time, claudeSessions: claudeCount),
                claudeHue: Self.rainbowHue(time: time, seed: Self.claudeHueSeed),
                codexHue: Self.rainbowHue(time: time, seed: Self.codexHueSeed)
            )
        }
    }

    // MARK: - Rainbow

    /// Continuous 0..<1 hue loop from wall-clock time.
    static func rainbowHue(time: TimeInterval, seed: Double) -> Double {
        rainbowHue(phase: time / rainbowLoopSeconds, seed: seed)
    }

    /// Hue for a fixed 0..<1 phase (snapshot mode renders these directly).
    static func rainbowHue(phase: Double, seed: Double) -> Double {
        let hue = (phase + seed).truncatingRemainder(dividingBy: 1)
        return hue < 0 ? hue + 1 : hue
    }

    // MARK: - Jump

    /// Vertical offset for the mascot's hop at `time`. 0 (grounded) when no
    /// Claude sessions are running.
    static func jumpOffset(time: TimeInterval, claudeSessions: Int) -> CGFloat {
        guard let period = jumpPeriod(claudeSessions: claudeSessions) else { return 0 }
        let phase = time.truncatingRemainder(dividingBy: period) / period
        return jumpOffset(phase: phase, claudeSessions: claudeSessions)
    }

    /// Vertical offset at a fixed 0..<1 position within the jump cycle
    /// (snapshot mode renders these directly).
    static func jumpOffset(phase: Double, claudeSessions: Int) -> CGFloat {
        guard claudeSessions >= 1 else { return 0 }
        let normalized = phase - floor(phase)
        guard normalized < airborneFraction else { return 0 }
        return -jumpHeight * CGFloat(sin(.pi * normalized / airborneFraction))
    }

    /// Jump cycle duration for a Claude session count: nil (idle) at 0, normal
    /// speed at 1, linearly faster up to 10, clamped for anything past 10.
    static func jumpPeriod(claudeSessions: Int) -> TimeInterval? {
        guard claudeSessions >= 1 else { return nil }
        let clamped = Double(min(claudeSessions, 10))
        return slowestJumpPeriod - (slowestJumpPeriod - fastestJumpPeriod) * (clamped - 1) / 9.0
    }
}

/// The label row itself — a pure function of counts + animation phases, shared
/// by the live TimelineView wrapper above and offscreen snapshot rendering.
struct NotchClosedLabelContent: View {
    let claudeCount: Int
    let codexCount: Int
    /// Mascot vertical offset in points (≤ 0 while airborne).
    let jumpOffset: CGFloat
    /// 0..<1 rainbow hue for the claude `[icon]{n}` group.
    let claudeHue: Double
    /// 0..<1 rainbow hue for the codex `[icon]{m}` group.
    let codexHue: Double

    var body: some View {
        HStack(spacing: 8) {
            // Prefix text. If space ever gets tight, shrink/truncate this
            // (never the counts) — see minimumScaleFactor + tail truncation.
            Text("Llmux Islands")
                .font(.system(size: 11, weight: .semibold, design: .rounded))
                .foregroundColor(.white.opacity(0.85))
                .lineLimit(1)
                .truncationMode(.tail)
                .minimumScaleFactor(0.6)

            ClaudeCrabIcon(size: 14, animateLegs: claudeCount > 0)
                .offset(y: jumpOffset)

            if claudeCount > 0 {
                providerGroup(.claude, count: claudeCount, hue: claudeHue)
            }
            if codexCount > 0 {
                providerGroup(.codex, count: codexCount, hue: codexHue)
            }
        }
    }

    @ViewBuilder
    private func providerGroup(_ provider: UsageProvider, count: Int, hue: Double) -> some View {
        HStack(spacing: 3) {
            UsageProviderIcon(provider: provider, size: 12)
                .hueRotation(.degrees(hue * 360))
            Text("\(count)")
                .font(.system(size: 11, weight: .bold, design: .rounded))
                .monospacedDigit()
                .foregroundStyle(Color(hue: hue, saturation: 0.85, brightness: 1.0))
        }
    }
}
