//
//  NotchView.swift — the floating "dynamic island" root view.
//
//  Lifted verbatim from agent-island and stripped of the Claude-Code session
//  monitor / Sparkle update machinery: the notch shape, expand/collapse spring
//  animation, hover, and visibility behaviour are unchanged; the expanded panel
//  shows the llmux usage view or the settings menu.
//

import AppKit
import CoreGraphics
import SwiftUI

// Corner radius constants
private let cornerRadiusInsets = (
    opened: (top: CGFloat(19), bottom: CGFloat(24)),
    closed: (top: CGFloat(6), bottom: CGFloat(14))
)

struct NotchView: View {
    @ObservedObject var viewModel: NotchViewModel
    @StateObject private var activityCoordinator = NotchActivityCoordinator.shared
    @ObservedObject private var usageModel = IslandUsageModel.shared
    @State private var isVisible: Bool = false
    @State private var isHovering: Bool = false
    @State private var isBouncing: Bool = false

    @Namespace private var activityNamespace

    // llmux-islands has no Claude-session monitor, so the closed-state "activity"
    // pill never lights up. These stay constant so the layout math is identical.
    private var hasPendingPermission: Bool { false }
    private var hasWaitingForInput: Bool { false }

    // MARK: - Sizing

    private var closedNotchSize: CGSize {
        CGSize(width: viewModel.deviceNotchRect.width, height: viewModel.deviceNotchRect.height)
    }

    private var expansionWidth: CGFloat {
        if activityCoordinator.expandingActivity.show {
            switch activityCoordinator.expandingActivity.type {
            case .claude:
                return 2 * max(0, closedNotchSize.height - 12) + 20
            case .none:
                break
            }
        }
        return 0
    }

    private var notchSize: CGSize {
        switch viewModel.status {
        case .closed, .popping:
            return closedNotchSize
        case .opened:
            return viewModel.openedSize
        }
    }

    private var closedContentWidth: CGFloat {
        closedNotchSize.width + expansionWidth
    }

    // MARK: - Corner Radii

    private var topCornerRadius: CGFloat {
        viewModel.status == .opened ? cornerRadiusInsets.opened.top : cornerRadiusInsets.closed.top
    }

    private var bottomCornerRadius: CGFloat {
        viewModel.status == .opened ? cornerRadiusInsets.opened.bottom : cornerRadiusInsets.closed.bottom
    }

    private var currentNotchShape: NotchShape {
        NotchShape(topCornerRadius: topCornerRadius, bottomCornerRadius: bottomCornerRadius)
    }

    private let openAnimation = Animation.spring(response: 0.42, dampingFraction: 0.8, blendDuration: 0)
    private let closeAnimation = Animation.spring(response: 0.45, dampingFraction: 1.0, blendDuration: 0)

    // MARK: - Body

    var body: some View {
        ZStack(alignment: .top) {
            VStack(spacing: 0) {
                notchLayout
                    .frame(maxWidth: viewModel.status == .opened ? notchSize.width : nil, alignment: .top)
                    .padding(
                        .horizontal,
                        viewModel.status == .opened ? cornerRadiusInsets.opened.top : cornerRadiusInsets.closed.bottom
                    )
                    .padding([.horizontal, .bottom], viewModel.status == .opened ? 12 : 0)
                    .background(.black)
                    .clipShape(currentNotchShape)
                    .overlay(alignment: .top) {
                        Rectangle()
                            .fill(.black)
                            .frame(height: 1)
                            .padding(.horizontal, topCornerRadius)
                    }
                    .shadow(
                        color: (viewModel.status == .opened || isHovering) ? .black.opacity(0.7) : .clear,
                        radius: 6
                    )
                    .frame(
                        maxWidth: viewModel.status == .opened ? notchSize.width : nil,
                        maxHeight: viewModel.status == .opened ? notchSize.height : nil,
                        alignment: .top
                    )
                    .animation(viewModel.status == .opened ? openAnimation : closeAnimation, value: viewModel.status)
                    .animation(openAnimation, value: notchSize)
                    .animation(.smooth, value: activityCoordinator.expandingActivity)
                    .animation(.spring(response: 0.3, dampingFraction: 0.5), value: isBouncing)
                    .contentShape(Rectangle())
                    .onHover { hovering in
                        withAnimation(.spring(response: 0.38, dampingFraction: 0.8)) {
                            isHovering = hovering
                        }
                    }
                    .onTapGesture {
                        if viewModel.status != .opened {
                            viewModel.notchOpen(reason: .click)
                        }
                    }
            }
        }
        .opacity(isVisible ? 1 : 0)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .preferredColorScheme(.dark)
        .onAppear {
            // On non-notched displays keep the island visible so there's an
            // interaction target; on a real notch it stays hidden until hovered.
            if !viewModel.hasPhysicalNotch {
                isVisible = true
            }
        }
        .onChange(of: viewModel.status) { oldStatus, newStatus in
            handleStatusChange(from: oldStatus, to: newStatus)
        }
    }

    private var isProcessing: Bool {
        activityCoordinator.expandingActivity.show && activityCoordinator.expandingActivity.type == .claude
    }

    private var showClosedActivity: Bool {
        isProcessing || hasPendingPermission || hasWaitingForInput
    }

    // MARK: - Notch Layout

    @ViewBuilder
    private var notchLayout: some View {
        VStack(alignment: .leading, spacing: 0) {
            headerRow
                .frame(height: max(24, closedNotchSize.height))

            if viewModel.status == .opened {
                contentView
                    .frame(width: notchSize.width - 24)
                    .transition(
                        .asymmetric(
                            insertion: .scale(scale: 0.8, anchor: .top)
                                .combined(with: .opacity)
                                .animation(.smooth(duration: 0.35)),
                            removal: .opacity.animation(.easeOut(duration: 0.15))
                        )
                    )
            }
        }
    }

    @ViewBuilder
    private var headerRow: some View {
        HStack(spacing: 0) {
            if showClosedActivity {
                HStack(spacing: 4) {
                    ClaudeCrabIcon(size: 14, animateLegs: isProcessing)
                        .matchedGeometryEffect(id: "crab", in: activityNamespace, isSource: showClosedActivity)
                }
                .frame(width: viewModel.status == .opened ? nil : sideWidth)
                .padding(.leading, viewModel.status == .opened ? 8 : 0)
            }

            if viewModel.status == .opened {
                openedHeaderContent
            } else if !showClosedActivity {
                // Closed island: render the info label instead of a black box
                // (todo.md items 1–2). minWidth keeps the pill at least as wide
                // as the notch; wider content grows the pill to fit.
                NotchClosedLabelView(
                    claudeCount: usageModel.claudeInFlight,
                    codexCount: usageModel.codexInFlight
                )
                .frame(minWidth: closedNotchSize.width - 20)
            } else {
                Rectangle()
                    .fill(.black)
                    .frame(width: closedNotchSize.width - cornerRadiusInsets.closed.top + (isBouncing ? 16 : 0))
            }

            if showClosedActivity, isProcessing {
                ProcessingSpinner()
                    .matchedGeometryEffect(id: "spinner", in: activityNamespace, isSource: showClosedActivity)
                    .frame(width: viewModel.status == .opened ? 20 : sideWidth)
            }
        }
        .frame(height: closedNotchSize.height)
    }

    private var sideWidth: CGFloat {
        max(0, closedNotchSize.height - 12) + 10
    }

    @ViewBuilder
    private var openedHeaderContent: some View {
        HStack(spacing: 12) {
            if !showClosedActivity {
                ClaudeCrabIcon(size: 14)
                    .matchedGeometryEffect(id: "crab", in: activityNamespace, isSource: !showClosedActivity)
                    .padding(.leading, 8)
            }

            Spacer()

            Button {
                withAnimation(.spring(response: 0.3, dampingFraction: 0.8)) {
                    viewModel.toggleMenu()
                }
            } label: {
                Image(systemName: viewModel.contentType == .menu ? "xmark" : "line.3.horizontal")
                    .font(.system(size: 11, weight: .medium))
                    .foregroundColor(.white.opacity(0.4))
                    .frame(width: 22, height: 22)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
        }
    }

    @ViewBuilder
    private var contentView: some View {
        Group {
            switch viewModel.contentType {
            case .usage:
                IslandUsageView(model: IslandUsageModel.shared, viewModel: viewModel)
            case .menu:
                NotchMenuView(viewModel: viewModel)
            }
        }
        .frame(width: notchSize.width - 24)
    }

    // MARK: - Event Handlers

    private func handleStatusChange(from oldStatus: NotchStatus, to newStatus: NotchStatus) {
        switch newStatus {
        case .opened, .popping:
            isVisible = true
        case .closed:
            guard viewModel.hasPhysicalNotch else { return }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) {
                if viewModel.status == .closed && !activityCoordinator.expandingActivity.show {
                    isVisible = false
                }
            }
        }
    }
}
