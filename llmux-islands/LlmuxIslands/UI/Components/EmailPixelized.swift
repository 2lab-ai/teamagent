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
    @Environment(\.colorScheme) private var colorScheme
    @State private var mosaic: CGImage?

    init(size: CGSize, cacheKey: String, content: @escaping () -> Content) {
        self.size = size
        self.cacheKey = cacheKey
        self.content = content
        // Offscreen snapshot mode renders through `ImageRenderer`, which never
        // pumps `.task` — precompute the mosaic synchronously there so the
        // snapshot PNGs show the real mosaic instead of a blank gap. The live
        // window path keeps the cached `.task` pipeline below. `@Environment`
        // is not readable in init; snapshot mode always composes on the dark
        // island backdrop (SnapshotMode.write wraps in `.dark`), so pin `.dark`.
        if SnapshotMode.isActive {
            let image = MainActor.assumeIsolated {
                Self.makeMosaic(size: size, displayScale: SnapshotMode.scale, colorScheme: .dark, content: content)
            }
            _mosaic = State(initialValue: image)
        }
    }

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

    /// Re-render whenever the text, the laid-out size, the screen scale, or
    /// the color scheme change.
    private var renderKey: String {
        let scheme = colorScheme == .dark ? "dark" : "light"
        return "\(cacheKey)|\(Int(size.width.rounded()))x\(Int(size.height.rounded()))@\(displayScale)|\(scheme)"
    }

    @MainActor
    private func render() {
        mosaic = Self.makeMosaic(size: size, displayScale: displayScale, colorScheme: colorScheme, content: content)
    }

    @MainActor
    private static func makeMosaic(size: CGSize, displayScale: CGFloat, colorScheme: ColorScheme, content: () -> Content) -> CGImage? {
        guard size.width >= 1, size.height >= 1 else { return nil }
        // `ImageRenderer` renders in a fresh default (light) environment, which
        // flips adaptive colors like `.secondary` — thread the caller's color
        // scheme through so the mosaic matches the surrounding rendering (the
        // token sheet's dark background would otherwise get a near-invisible
        // dark-gray mosaic).
        let renderer = ImageRenderer(content: content().environment(\.colorScheme, colorScheme))
        renderer.proposedSize = ProposedViewSize(size)
        renderer.scale = displayScale
        guard let full = renderer.cgImage else { return nil }
        guard let small = downsample(full, to: size, blockSize: EmailPixelize.blockSize) else { return nil }
        // Blow the tiny image back up to full pixel size with nearest-neighbor
        // HERE (not at display time): view-level `.interpolation(.none)` is not
        // honored on every render path (offscreen NSHostingView snapshots smooth
        // it into a blur), and a pre-upscaled bitmap draws 1:1 with hard block
        // edges everywhere.
        return upscale(small, toPixelWidth: full.width, height: full.height)
    }

    /// Average the full-resolution snapshot down to one pixel per
    /// `blockSize`-point cell.
    private static func downsample(_ image: CGImage, to pointSize: CGSize, blockSize: CGFloat) -> CGImage? {
        let block = max(1, blockSize)
        let width = max(1, Int((pointSize.width / block).rounded(.up)))
        let height = max(1, Int((pointSize.height / block).rounded(.up)))
        return redraw(image, width: width, height: height, quality: .high)
    }

    /// Nearest-neighbor upscale of the mosaic back to the snapshot's pixel size.
    private static func upscale(_ image: CGImage, toPixelWidth width: Int, height: Int) -> CGImage? {
        redraw(image, width: max(1, width), height: max(1, height), quality: .none)
    }

    private static func redraw(_ image: CGImage, width: Int, height: Int, quality: CGInterpolationQuality) -> CGImage? {
        guard let context = CGContext(
            data: nil,
            width: width,
            height: height,
            bitsPerComponent: 8,
            bytesPerRow: 0,
            space: CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ) else { return nil }
        context.interpolationQuality = quality
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        return context.makeImage()
    }
}
