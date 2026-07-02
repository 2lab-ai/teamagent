//
//  EmailPixelized.swift
//  LlmuxIslands
//
//  Mosaic post-processing for the "Email anonymous" setting (todo item 3).
//
//  The wrapped content keeps its exact layout in both states. When inactive it
//  renders unchanged. When active, the original glyphs are hidden and replaced
//  by a snapshot of the same content taken with `ImageRenderer`, averaged down
//  to one pixel per mosaic cell, and drawn back at full size with
//  `.interpolation(.none)` — a true render-then-pixelate mosaic, not a string
//  replacement, so the email is illegible but its footprint is unchanged.
//

import SwiftUI

enum EmailPixelize {
    /// Mosaic cell edge in points. ~4x4 is the starting value from todo item 3;
    /// raise it for a coarser (more anonymous) mosaic, lower it toward 1 to
    /// approach the original rendering.
    static let blockSize: CGFloat = 4
}

/// Wraps an email-bearing view. `isActive == false` renders `content` as-is;
/// `isActive == true` keeps the identical layout but shows only the mosaic.
struct EmailPixelized<Content: View>: View {
    let isActive: Bool
    /// Snapshot invalidation key: pass the rendered string so a changed email
    /// re-renders the mosaic.
    let cacheKey: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        if isActive {
            content()
                .opacity(0)
                .overlay(
                    GeometryReader { proxy in
                        PixelizedSnapshot(size: proxy.size, cacheKey: cacheKey, content: content)
                    }
                )
                .accessibilityHidden(true)
        } else {
            content()
        }
    }
}

/// Renders `content` at `size`, downsamples it to `EmailPixelize.blockSize`
/// point cells, and displays the tiny bitmap scaled back up with no
/// interpolation (each source pixel becomes one visible mosaic block).
private struct PixelizedSnapshot<Content: View>: View {
    let size: CGSize
    let cacheKey: String
    let content: () -> Content

    @Environment(\.displayScale) private var displayScale
    @State private var mosaic: CGImage?

    var body: some View {
        ZStack {
            if let mosaic {
                Image(decorative: mosaic, scale: 1)
                    .resizable()
                    .interpolation(.none)
                    .frame(width: size.width, height: size.height)
            }
        }
        .task(id: renderKey) { render() }
    }

    /// Re-render whenever the text, the laid-out size, or the screen scale change.
    private var renderKey: String {
        "\(cacheKey)|\(Int(size.width.rounded()))x\(Int(size.height.rounded()))@\(displayScale)"
    }

    @MainActor
    private func render() {
        guard size.width >= 1, size.height >= 1 else {
            mosaic = nil
            return
        }
        let renderer = ImageRenderer(content: content())
        renderer.proposedSize = ProposedViewSize(size)
        renderer.scale = displayScale
        guard let full = renderer.cgImage else {
            mosaic = nil
            return
        }
        mosaic = Self.downsample(full, to: size, blockSize: EmailPixelize.blockSize)
    }

    /// Average the full-resolution snapshot down to one pixel per
    /// `blockSize`-point cell.
    private static func downsample(_ image: CGImage, to pointSize: CGSize, blockSize: CGFloat) -> CGImage? {
        let block = max(1, blockSize)
        let width = max(1, Int((pointSize.width / block).rounded(.up)))
        let height = max(1, Int((pointSize.height / block).rounded(.up)))
        guard let context = CGContext(
            data: nil,
            width: width,
            height: height,
            bitsPerComponent: 8,
            bytesPerRow: 0,
            space: CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ) else { return nil }
        context.interpolationQuality = .high
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        return context.makeImage()
    }
}
