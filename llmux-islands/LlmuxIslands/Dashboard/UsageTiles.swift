import SwiftUI
import AppKit

enum UsageProvider: Hashable {
    case claude
    case codex
    case gemini

    var displayName: String {
        switch self {
        case .claude: "Claude"
        case .codex: "Codex"
        case .gemini: "Gemini"
        }
    }
}

enum UsageWindow: CaseIterable {
    case fiveHour
    case twentyFourHour
    case sevenDay

    var label: String {
        switch self {
        case .fiveHour: "5h"
        case .twentyFourHour: "24h"
        case .sevenDay: "7d"
        }
    }
}

private enum UsageAccountIdFormatter {
    static func displayAccountId(provider: UsageProvider, email: String?, claudeIsTeam: Bool?) -> String? {
        guard let emailSlug = emailSlug(email) else { return nil }

        switch provider {
        case .claude:
            if claudeIsTeam == true {
                return "acct_claude_team_\(emailSlug)"
            }
            return "acct_claude_\(emailSlug)"
        case .codex:
            return "acct_codex_\(emailSlug)"
        case .gemini:
            return "acct_gemini_\(emailSlug)"
        }
    }

    private static func emailSlug(_ email: String?) -> String? {
        guard let email = email?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil else { return nil }

        let lowered = email.lowercased()
        var output: [UInt8] = []
        output.reserveCapacity(lowered.utf8.count)

        var lastWasUnderscore = false
        for byte in lowered.utf8 {
            let isDigit = byte >= 48 && byte <= 57
            let isLower = byte >= 97 && byte <= 122
            if isDigit || isLower {
                output.append(byte)
                lastWasUnderscore = false
            } else {
                guard !lastWasUnderscore else { continue }
                output.append(95) // "_"
                lastWasUnderscore = true
            }
        }

        let raw = String(decoding: output, as: UTF8.self)
        let trimmed = raw.trimmingCharacters(in: CharacterSet(charactersIn: "_"))
        return trimmed.nonEmptyOrNil
    }
}

struct UsageAccountTile: Identifiable {
    let id: String
    let provider: UsageProvider
    let accountId: String
    let label: String
    let email: String?
    let tier: String?
    let claudeIsTeam: Bool?
    let tokenRefresh: TokenRefreshInfo?
    let info: CLIUsageInfo?
    let errorMessage: String?
    let issue: UsageIssue?
}

private struct UsageAccountTileRowHeightsPreferenceKey: PreferenceKey {
    static var defaultValue: [Int: CGFloat] = [:]

    static func reduce(value: inout [Int: CGFloat], nextValue: () -> [Int: CGFloat]) {
        for (rowIndex, rowHeight) in nextValue() {
            value[rowIndex] = max(value[rowIndex] ?? 0, rowHeight)
        }
    }
}

struct UsageAccountTileGrid: View {
    let tiles: [UsageAccountTile]
    let columns: [GridItem]
    let now: Date
    var onEditClaudeCodeToken: ((String) -> Void)? = nil
    var onClearClaudeCodeToken: ((String) -> Void)? = nil
    var claudeCodeTokenStatusByAccountId: [String: ClaudeCodeTokenStatus] = [:]
    var onSetClaudeCodeTokenEnabled: ((String, Bool) -> Void)? = nil
    var onRemove: ((String) -> Void)? = nil

    @State private var rowHeights: [Int: CGFloat] = [:]

    private struct IndexedTile: Identifiable {
        let index: Int
        let tile: UsageAccountTile

        var id: String { tile.id }
    }

    var body: some View {
        let indexedTiles = tiles.enumerated().map { IndexedTile(index: $0.offset, tile: $0.element) }
        LazyVGrid(columns: columns, spacing: 10) {
            ForEach(indexedTiles, id: \.id) { indexed in
                let rowIndex = rowIndex(for: indexed.index)
                UsageAccountTileCard(
                    tile: indexed.tile,
                    now: now,
                    forcedHeight: rowHeights[rowIndex],
                    rowIndex: rowIndex,
                    onEditClaudeCodeToken: onEditClaudeCodeToken,
                    onClearClaudeCodeToken: onClearClaudeCodeToken,
                    claudeCodeTokenStatus: indexed.tile.provider == .claude
                        ? claudeCodeTokenStatusByAccountId[indexed.tile.accountId]
                        : nil,
                    onSetClaudeCodeTokenEnabled: onSetClaudeCodeTokenEnabled
                )
                .contextMenu {
                    if let onRemove {
                        Button("Remove \(indexed.tile.label)", role: .destructive) {
                            onRemove(indexed.tile.accountId)
                        }
                    }
                }
            }
        }
        .onPreferenceChange(UsageAccountTileRowHeightsPreferenceKey.self) { newHeights in
            if rowHeights != newHeights {
                rowHeights = newHeights
            }
        }
    }

    private func rowIndex(for tileIndex: Int) -> Int {
        guard !columns.isEmpty else { return 0 }
        return tileIndex / columns.count
    }
}

private struct UsageAccountTileCard: View {
    let tile: UsageAccountTile
    let now: Date
    let forcedHeight: CGFloat?
    let rowIndex: Int?
    let onEditClaudeCodeToken: ((String) -> Void)?
    let onClearClaudeCodeToken: ((String) -> Void)?
    let claudeCodeTokenStatus: ClaudeCodeTokenStatus?
    let onSetClaudeCodeTokenEnabled: ((String, Bool) -> Void)?

    @State private var isHovered = false

    init(
        tile: UsageAccountTile,
        now: Date,
        forcedHeight: CGFloat? = nil,
        rowIndex: Int? = nil,
        onEditClaudeCodeToken: ((String) -> Void)?,
        onClearClaudeCodeToken: ((String) -> Void)?,
        claudeCodeTokenStatus: ClaudeCodeTokenStatus?,
        onSetClaudeCodeTokenEnabled: ((String, Bool) -> Void)?
    ) {
        self.tile = tile
        self.now = now
        self.forcedHeight = forcedHeight
        self.rowIndex = rowIndex
        self.onEditClaudeCodeToken = onEditClaudeCodeToken
        self.onClearClaudeCodeToken = onClearClaudeCodeToken
        self.claudeCodeTokenStatus = claudeCodeTokenStatus
        self.onSetClaudeCodeTokenEnabled = onSetClaudeCodeTokenEnabled
    }

    var body: some View {
        content
            // Keep the measured content at its natural height even when we wrap it with a fixed row height.
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(heightReporter)
            .frame(height: forcedHeight, alignment: .topLeading)
            .background(
                RoundedRectangle(cornerRadius: 10)
                    .fill(isHovered ? Color.white.opacity(0.09) : Color.white.opacity(0.06))
            )
            .onHover { isHovered = $0 }
            .animation(.easeInOut(duration: 0.15), value: isHovered)
    }

    private var content: some View {
        VStack(alignment: .leading, spacing: 8) {
            UsageProviderColumn(
                provider: tile.provider,
                accountId: tile.accountId,
                email: tile.email,
                tier: tile.tier,
                claudeIsTeam: tile.claudeIsTeam,
                tokenRefresh: tile.tokenRefresh,
                info: tile.info,
                now: now,
                onEditClaudeCodeToken: onEditClaudeCodeToken,
                onClearClaudeCodeToken: onClearClaudeCodeToken,
                claudeCodeTokenStatus: claudeCodeTokenStatus,
                onSetClaudeCodeTokenEnabled: onSetClaudeCodeTokenEnabled
            )

            if let issue = tile.issue {
                UsageIssueInlineView(issue: issue)
            } else {
                Text((tile.errorMessage?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil) ?? " ")
                    .font(.system(size: 10))
                    .foregroundColor(TerminalColors.amber.opacity(0.9))
                    .lineLimit(1)
                    .opacity((tile.errorMessage?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil) == nil ? 0 : 1)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    @ViewBuilder
    private var heightReporter: some View {
        if let rowIndex {
            GeometryReader { proxy in
                Color.clear.preference(
                    key: UsageAccountTileRowHeightsPreferenceKey.self,
                    value: [rowIndex: proxy.size.height]
                )
            }
        }
    }
}

private struct UsageDashboardPanel: View {
    let title: String
    let badge: String?
    let snapshot: UsageSnapshot?
    let accountIds: UsageAccountIdSet
    let now: Date
    let showSwitch: Bool
    let isSwitching: Bool
    let onEditClaudeCodeToken: ((String) -> Void)?
    let onClearClaudeCodeToken: ((String) -> Void)?
    let claudeCodeTokenStatus: ClaudeCodeTokenStatus?
    let onSetClaudeCodeTokenEnabled: ((String, Bool) -> Void)?
    let onSwitch: () -> Void
    let onDelete: (() -> Void)?

    @State private var isHovered = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            header

            VStack(alignment: .leading, spacing: 6) {
                Text("Dashboard")
                    .font(.system(size: 11, weight: .semibold, design: .monospaced))
                    .foregroundColor(.white.opacity(0.55))
                Rectangle()
                    .fill(Color.white.opacity(0.08))
                    .frame(height: 1)
            }

            HStack(alignment: .top, spacing: 0) {
                UsageProviderColumn(
                    provider: .claude,
                    accountId: accountIds.claude,
                    email: snapshot?.identities.claudeEmail,
                    tier: snapshot?.identities.claudeTier,
                    claudeIsTeam: snapshot?.identities.claudeIsTeam,
                    tokenRefresh: snapshot?.tokenRefresh.claude,
                    info: snapshot?.output?.claude,
                    now: now,
                    onEditClaudeCodeToken: onEditClaudeCodeToken,
                    onClearClaudeCodeToken: onClearClaudeCodeToken,
                    claudeCodeTokenStatus: claudeCodeTokenStatus,
                    onSetClaudeCodeTokenEnabled: onSetClaudeCodeTokenEnabled
                )
                columnDivider
                UsageProviderColumn(
                    provider: .codex,
                    accountId: accountIds.codex,
                    email: snapshot?.identities.codexEmail,
                    tier: nil,
                    claudeIsTeam: nil,
                    tokenRefresh: snapshot?.tokenRefresh.codex,
                    info: snapshot?.output?.codex,
                    now: now,
                    onEditClaudeCodeToken: nil,
                    onClearClaudeCodeToken: nil,
                    claudeCodeTokenStatus: nil,
                    onSetClaudeCodeTokenEnabled: nil
                )
                columnDivider
                UsageProviderColumn(
                    provider: .gemini,
                    accountId: accountIds.gemini,
                    email: snapshot?.identities.geminiEmail,
                    tier: nil,
                    claudeIsTeam: nil,
                    tokenRefresh: snapshot?.tokenRefresh.gemini,
                    info: snapshot?.output?.gemini,
                    now: now,
                    onEditClaudeCodeToken: nil,
                    onClearClaudeCodeToken: nil,
                    claudeCodeTokenStatus: nil,
                    onSetClaudeCodeTokenEnabled: nil
                )
            }

            if let issue = snapshot?.issue {
                UsageIssueInlineView(issue: issue)
            } else if let message = snapshot?.errorMessage {
                Text(message)
                    .font(.system(size: 10))
                    .foregroundColor(TerminalColors.amber.opacity(0.9))
                    .lineLimit(2)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(
            RoundedRectangle(cornerRadius: 10)
                .fill(isHovered ? Color.white.opacity(0.09) : Color.white.opacity(0.06))
        )
        .onHover { isHovered = $0 }
        .animation(.easeInOut(duration: 0.15), value: isHovered)
    }

    private var header: some View {
        HStack(spacing: 10) {
            Text(title)
                .font(.system(size: 13, weight: .semibold))
                .foregroundColor(.white.opacity(0.9))
                .lineLimit(1)

            if let badge {
                Text(badge)
                    .font(.system(size: 10, weight: .semibold, design: .monospaced))
                    .foregroundColor(.white.opacity(0.3))
                    .padding(.horizontal, 6)
                    .padding(.vertical, 3)
                    .background(
                        RoundedRectangle(cornerRadius: 6)
                            .fill(Color.white.opacity(0.06))
                    )
            }

            Spacer()

            if let snapshot, let fetchedAt = snapshot.fetchedAt {
                Text(timeString(fetchedAt))
                    .font(.system(size: 10, weight: .medium, design: .monospaced))
                    .foregroundColor(.white.opacity(snapshot.isStale ? 0.35 : 0.45))
            }

            if showSwitch {
                Button(action: onSwitch) {
                    HStack(spacing: 6) {
                        if isSwitching {
                            ProgressView()
                                .scaleEffect(0.5)
                                .frame(width: 10, height: 10)
                        } else {
                            Image(systemName: "arrow.triangle.2.circlepath")
                                .font(.system(size: 11, weight: .semibold))
                        }
                        Text("Switch")
                            .font(.system(size: 11, weight: .medium))
                    }
                    .foregroundColor(.white.opacity(0.6))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 6)
                    .background(
                        RoundedRectangle(cornerRadius: 8)
                            .fill(Color.white.opacity(0.05))
                    )
                }
                .buttonStyle(.plain)
                .disabled(isSwitching)

                if let onDelete {
                    Button(action: onDelete) {
                        HStack(spacing: 6) {
                            Image(systemName: "trash")
                                .font(.system(size: 11, weight: .semibold))
                            Text("Delete")
                                .font(.system(size: 11, weight: .medium))
                        }
                        .foregroundColor(TerminalColors.red.opacity(0.85))
                        .padding(.horizontal, 8)
                        .padding(.vertical, 6)
                        .background(
                            RoundedRectangle(cornerRadius: 8)
                                .fill(TerminalColors.red.opacity(0.12))
                        )
                    }
                    .buttonStyle(.plain)
                    .disabled(isSwitching)
                }
            }
        }
    }

    private var columnDivider: some View {
        Rectangle()
            .fill(Color.white.opacity(0.08))
            .frame(width: 1)
            .padding(.horizontal, 10)
    }

    private func timeString(_ date: Date) -> String {
        let seconds = max(0, Int(Date().timeIntervalSince(date)))
        if seconds < 60 { return "\(seconds)s" }
        let minutes = seconds / 60
        if minutes < 60 { return "\(minutes)m" }
        return "\(minutes / 60)h"
    }
}

private struct UsageIssueInlineView: View {
    let issue: UsageIssue

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(issue.message)
                .font(.system(size: 10))
                .foregroundColor(TerminalColors.amber.opacity(0.9))
                .lineLimit(2)
        }
    }
}

private struct UsageProviderColumn: View {
    let provider: UsageProvider
    let accountId: String?
    let email: String?
    let tier: String?
    let claudeIsTeam: Bool?
    let tokenRefresh: TokenRefreshInfo?
    let info: CLIUsageInfo?
    let now: Date
    let onEditClaudeCodeToken: ((String) -> Void)?
    let onClearClaudeCodeToken: ((String) -> Void)?
    let claudeCodeTokenStatus: ClaudeCodeTokenStatus?
    let onSetClaudeCodeTokenEnabled: ((String, Bool) -> Void)?

    @AppStorage(AppSettings.emailAnonymousEnabledKey) private var emailAnonymousEnabled = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            header

            UsageTokenRefreshRow(tokenRefresh: tokenRefresh, now: now)

            usageRows
        }
        .contextMenu {
            if provider == .claude, let accountId = normalizedAccountId {
                if let onEditClaudeCodeToken {
                    Button("Set Claude Code Token…") {
                        onEditClaudeCodeToken(accountId)
                    }
                }

                if let onClearClaudeCodeToken {
                    Button("Clear Claude Code Token") {
                        onClearClaudeCodeToken(accountId)
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                UsageProviderIcon(provider: provider, size: 16)

                Spacer(minLength: 0)

                if let tier = tierBadgeTier {
                    TierBadge(provider: provider, tier: tier)
                }

                if showsClaudeTeamBadge {
                    Text("TEAM")
                        .font(.system(size: 9, weight: .semibold, design: .monospaced))
                        .foregroundColor(Color.white.opacity(0.7))
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .background(
                            Capsule(style: .continuous)
                                .fill(Color.white.opacity(0.08))
                        )
                }

                if let badge = statusBadge {
                    Text(badge.label)
                        .font(.system(size: 9, weight: .semibold, design: .monospaced))
                        .foregroundColor(badge.foreground)
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .background(
                            Capsule(style: .continuous)
                                .fill(badge.background)
                        )
                }
            }

            // The header title is the account email whenever one is known —
            // mosaic it when "Email anonymous" is on (todo item 3).
            EmailPixelized(
                isActive: emailAnonymousEnabled && normalizedEmail != nil,
                cacheKey: headerTitle
            ) {
                Text(headerTitle)
                    .font(.system(size: headerTitleFontSize, weight: .semibold, design: .monospaced))
                    .foregroundColor(headerTitleColor)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .minimumScaleFactor(0.35)
                    .allowsTightening(true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    private var headerTitleFontSize: CGFloat {
        let count = headerTitle.count
        switch count {
        case 0...16: return 14
        case 17...26: return 13
        case 27...38: return 12
        default: return 11
        }
    }

    private var tierBadgeTier: String? {
        guard let tier = resolvedTier else { return nil }

        // Only show Claude tier when we can confidently classify it.
        if provider == .claude, normalizedClaudeTierLabel(from: tier) == nil {
            return nil
        }

        return tier
    }

    private func normalizedClaudeTierLabel(from tier: String) -> String? {
        let raw = tier.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !raw.isEmpty else { return nil }

        let lowered = raw.lowercased()
        let tokens = lowered.split { !($0.isLetter || $0.isNumber) }
        let hasToken: (String) -> Bool = { token in tokens.contains { $0 == token } }
        let normalized = lowered
            .replacingOccurrences(of: " ", with: "")
            .replacingOccurrences(of: "-", with: "")
            .replacingOccurrences(of: "_", with: "")

        if normalized.contains("max20") || (hasToken("max") && (hasToken("20x") || hasToken("20"))) { return "Max20" }
        if normalized.contains("max5") || (hasToken("max") && (hasToken("5x") || hasToken("5"))) { return "Max5" }
        if hasToken("pro") { return "Pro" }
        if hasToken("max") || normalized.contains("max") { return "Max" }

        return nil
    }

    private var showsClaudeTeamBadge: Bool {
        provider == .claude && claudeIsTeam == true
    }

    private var normalizedAccountId: String? {
        accountId?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil
    }

    private var normalizedEmail: String? {
        email?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil
    }

    private var headerTitle: String {
        if let normalizedEmail { return normalizedEmail }
        if let info, !info.available { return "Not installed" }
        if let normalizedAccountId { return normalizedAccountId }
        return "--"
    }

    private var headerTitleColor: Color {
        if normalizedEmail != nil { return Color.white.opacity(0.9) }
        if let info, !info.available { return TerminalColors.dim }
        if normalizedAccountId != nil { return Color.white.opacity(0.22) }
        return Color.white.opacity(0.2)
    }

    private var statusBadge: (label: String, background: Color, foreground: Color)? {
        if let info, !info.available {
            return (label: "MISS", background: Color.white.opacity(0.08), foreground: Color.white.opacity(0.45))
        }

        if isTokenExpired {
            return (label: "EXP", background: TerminalColors.amber.opacity(0.9), foreground: Color.black.opacity(0.85))
        }

        if info?.error == true {
            return (label: "ERR", background: TerminalColors.red.opacity(0.9), foreground: Color.white.opacity(0.9))
        }
        return nil
    }

    private var isTokenExpired: Bool {
        guard let tokenRefresh else { return false }
        return tokenRefresh.expiresAt <= now
    }

    private var resolvedTier: String? {
        switch provider {
        case .claude:
            return tier
        case .codex:
            return normalizeCodexTier(info?.plan)
        case .gemini:
            return inferGeminiTier(model: info?.model, plan: info?.plan)
        }
    }

    @ViewBuilder
    private var usageRows: some View {
        switch provider {
        case .gemini:
            GeminiUsageSummaryRow(info: info, now: now)
        case .claude, .codex:
            ForEach(providerWindows, id: \.label) { window in
                UsageWindowRow(
                    window: window,
                    percentUsed: percentUsed(for: window),
                    resetAt: resetAt(for: window),
                    now: now
                )
            }
        }
    }

    private var providerWindows: [UsageWindow] {
        switch provider {
        case .gemini: return []
        case .claude, .codex: return [.fiveHour, .sevenDay]
        }
    }

    private func percentUsed(for window: UsageWindow) -> Double? {
        guard let info, info.available, !info.error else { return nil }
        switch window {
        case .fiveHour, .twentyFourHour: return info.fiveHourPercent
        case .sevenDay: return info.sevenDayPercent
        }
    }

    private func resetAt(for window: UsageWindow) -> Date? {
        guard let info, info.available, !info.error else { return nil }
        switch window {
        case .fiveHour, .twentyFourHour: return info.fiveHourReset
        case .sevenDay: return info.sevenDayReset
        }
    }

    private func normalizeCodexTier(_ plan: String?) -> String? {
        guard let plan = plan?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil else { return nil }

        let lowered = plan.lowercased()
        let tokens = lowered.split { !($0.isLetter || $0.isNumber) }
        let hasToken: (String) -> Bool = { token in tokens.contains { $0 == token } }

        if hasToken("plus") || lowered.contains("plus") { return "Plus" }
        if hasToken("pro") || lowered.contains("pro") { return "Pro" }
        return plan
    }

    private func inferGeminiTier(model: String?, plan: String?) -> String? {
        let candidates = [plan, model]
            .compactMap { $0?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil }
        guard !candidates.isEmpty else { return nil }

        let lowered = candidates.joined(separator: " ").lowercased()
        if lowered.contains("pro") { return "Pro" }
        if lowered.contains("flash") { return "Flash" }
        if lowered.contains("ultra") { return "Ultra" }
        if lowered.contains("nano") { return "Nano" }
        return nil
    }
}

private struct TierBadge: View {
    let provider: UsageProvider
    let tier: String

    var body: some View {
        Text(label)
            .font(.system(size: 9, weight: .semibold, design: .monospaced))
            .foregroundColor(style.foreground)
            .lineLimit(1)
            .minimumScaleFactor(0.75)
            .fixedSize(horizontal: true, vertical: false)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(
                Capsule(style: .continuous)
                    .fill(style.background)
            )
    }

    private var label: String {
        let lowered = tier.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if lowered.contains("max") && lowered.contains("20") { return "Max20" }
        if lowered.contains("max") && lowered.contains("5") { return "Max5" }
        if lowered.contains("max") { return "Max" }
        if lowered.contains("plus") { return "Plus" }
        if lowered.contains("pro") { return "Pro" }
        if lowered.contains("flash") { return "Flash" }
        if lowered.contains("ultra") { return "Ultra" }
        if lowered.contains("nano") { return "Nano" }
        return tier
    }

    private var style: (background: Color, foreground: Color) {
        let key = label.lowercased()

        switch provider {
        case .claude:
            if key == "pro" { return (Color.white.opacity(0.9), Color.black.opacity(0.85)) }
            if key == "max5" { return (TerminalColors.amber, Color.black.opacity(0.85)) }
            if key == "max20" { return (TerminalColors.red, Color.white.opacity(0.9)) }
            if key == "max" { return (TerminalColors.red.opacity(0.85), Color.white.opacity(0.9)) }
        case .codex:
            if key == "plus" { return (Color.white.opacity(0.9), Color.black.opacity(0.85)) }
            if key == "pro" { return (TerminalColors.red, Color.white.opacity(0.9)) }
        case .gemini:
            return (TerminalColors.blue.opacity(0.85), Color.white.opacity(0.9))
        }

        return (Color.white.opacity(0.08), Color.white.opacity(0.55))
    }
}

private struct UsageTokenRefreshRow: View {
    let tokenRefresh: TokenRefreshInfo?
    let now: Date

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: "key.fill")
                .font(.system(size: 13, weight: .semibold))
                .foregroundColor(iconColor)
                .frame(width: 26, alignment: .leading)

            MiniSegmentBar(
                fraction: remainingFraction,
                fillColor: barFillColor,
                emptyColor: Color.white.opacity(0.08)
            )
            .frame(height: 10)
            .frame(maxWidth: .infinity)

            timeRemainingText
                .lineLimit(1)
                .minimumScaleFactor(0.7)
                .frame(minWidth: 84, alignment: .trailing)
        }
        .opacity(tokenRefresh == nil ? 0.65 : 1)
    }

    private var remainingFraction: Double {
        guard let tokenRefresh else { return 0 }
        let remaining = max(0, tokenRefresh.expiresAt.timeIntervalSince(now))
        let total = max(1, tokenRefresh.lifetimeSeconds)
        return max(0, min(1, remaining / total))
    }

    private var barFillColor: Color {
        tokenRefresh == nil
            ? Color.white.opacity(0.12)
            : TerminalColors.magenta.opacity(0.85)
    }

    private var iconColor: Color {
        tokenRefresh == nil
            ? Color.white.opacity(0.25)
            : TerminalColors.magenta.opacity(0.85)
    }

    private var timeRemainingText: Text {
        let baseColor = Color.white.opacity(0.32)
        let font = Font.system(size: 14, weight: .semibold, design: .monospaced)
        guard let tokenRefresh else { return Text("--").font(font).foregroundColor(baseColor) }
        if tokenRefresh.expiresAt <= now {
            return Text("Expired!")
                .font(font)
                .foregroundColor(TerminalColors.amber.opacity(0.9))
        }
        let seconds = max(0, Int(tokenRefresh.expiresAt.timeIntervalSince(now)))
        return UsageDurationText.make(seconds: seconds, digitColor: baseColor, scale: 1.3)
    }
}

private struct GeminiUsageSummaryRow: View {
    let info: CLIUsageInfo?
    let now: Date

    var body: some View {
        HStack(spacing: 8) {
            Text(modelName)
                .foregroundColor(.white.opacity(0.6))
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: 8)

            Text(bucketCountString)
                .foregroundColor(.white.opacity(0.35))
                .frame(width: 14, alignment: .trailing)

            Text(remainingPercentString)
                .foregroundColor(remainingPercentColor)
                .frame(width: 54, alignment: .trailing)

            resetsInText
                .lineLimit(1)
                .minimumScaleFactor(0.7)
        }
        .font(.system(size: 10, weight: .semibold, design: .monospaced))
    }

    private var modelName: String {
        info?.model?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil ?? "gemini"
    }

    private var bucketCountString: String {
        guard let buckets = info?.buckets else { return "--" }
        return "\(buckets.count)"
    }

    private var remainingPercentString: String {
        guard let used = info?.fiveHourPercent else { return "--" }
        let remaining = max(0, min(100, 100 - used))
        return String(format: "%.1f%%", remaining)
    }

    private var remainingPercentColor: Color {
        guard let used = info?.fiveHourPercent else { return TerminalColors.dim }
        let remaining = max(0, min(100, 100 - used))
        if remaining < 10 { return TerminalColors.red }
        if remaining < 25 { return TerminalColors.amber }
        return TerminalColors.green
    }

    private var resetsInText: Text {
        let baseColor = Color.white.opacity(0.28)
        guard let resetAt = info?.fiveHourReset else {
            return Text("(Resets in --)")
                .foregroundColor(baseColor)
        }

        let seconds = max(0, Int(resetAt.timeIntervalSince(now)))
        return Text("(").foregroundColor(baseColor)
            + Text("Resets in ").foregroundColor(baseColor)
            + UsageDurationText.make(seconds: seconds, digitColor: baseColor)
            + Text(")").foregroundColor(baseColor)
    }
}

private struct UsageWindowRow: View {
    let window: UsageWindow
    let percentUsed: Double?
    let resetAt: Date?
    let now: Date

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Text(window.label)
                .font(.system(size: 14, weight: .semibold, design: .monospaced))
                .foregroundColor(.white.opacity(0.45))
                .frame(width: 26, alignment: .leading)
                .padding(.top, 2)

            // Usage-remaining column (bar over percent), stretches to fill width.
            VStack(spacing: 6) {
                MiniSegmentBar(
                    fraction: usageRemainingFraction,
                    fillColor: usageFillColor,
                    emptyColor: Color.white.opacity(0.08)
                )
                .frame(height: 10)

                Text(remainingPercentString)
                    .font(.system(size: 14, weight: .semibold, design: .monospaced))
                    .foregroundColor(usageTextColor)
                    .lineLimit(1)
                    .minimumScaleFactor(0.7)
            }
            .frame(maxWidth: .infinity)

            // Reset-countdown column (bar over remaining time), stretches to fill width.
            VStack(spacing: 6) {
                MiniSegmentBar(
                    fraction: resetRemainingFraction,
                    fillColor: TerminalColors.blue.opacity(0.85),
                    emptyColor: Color.white.opacity(0.08)
                )
                .frame(height: 10)

                timeRemainingText
                    .lineLimit(1)
                    .minimumScaleFactor(0.7)
            }
            .frame(maxWidth: .infinity)
        }
    }

    private var usageRemainingFraction: Double {
        guard let percentUsed else { return 0 }
        let used = max(0, min(100, percentUsed))
        return max(0, min(1, (100 - used) / 100))
    }

    private var remainingPercentString: String {
        guard let percentUsed else { return "--" }
        let used = max(0, min(100, percentUsed))
        let remaining = max(0, min(100, 100 - used))
        return "\(Int(remaining.rounded()))%"
    }

    private var usageFillColor: Color {
        let fraction = max(0, min(1, usageRemainingFraction))
        let hue = 0.33 * fraction
        return Color(hue: hue, saturation: 0.85, brightness: 0.95)
    }

    private var usageTextColor: Color {
        guard percentUsed != nil else { return TerminalColors.dim }
        return usageFillColor.opacity(0.9)
    }

    private var resetRemainingFraction: Double {
        guard let resetAt, let total = windowDurationSeconds else { return 0 }
        let remaining = max(0, resetAt.timeIntervalSince(now))
        return max(0, min(1, remaining / total))
    }

    private var timeRemainingText: Text {
        let baseColor = Color.white.opacity(0.32)
        guard let resetAt else {
            return Text("--").font(.system(size: 14, weight: .semibold, design: .monospaced)).foregroundColor(baseColor)
        }
        let seconds = max(0, Int(resetAt.timeIntervalSince(now)))
        return UsageDurationText.make(seconds: seconds, digitColor: baseColor, scale: 1.3)
    }

    private var windowDurationSeconds: TimeInterval? {
        switch window {
        case .fiveHour:
            return 5 * 60 * 60
        case .twentyFourHour:
            return 24 * 60 * 60
        case .sevenDay:
            return 7 * 24 * 60 * 60
        }
    }
}

private extension String {
    var nonEmptyOrNil: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}

private struct MiniSegmentBar: View {
    let fraction: Double
    let fillColor: Color
    let emptyColor: Color

    var body: some View {
        GeometryReader { geo in
            let segmentCount = 10
            let spacing: CGFloat = 1
            let totalSpacing = spacing * CGFloat(segmentCount - 1)
            let segmentWidth = max(1, (geo.size.width - totalSpacing) / CGFloat(segmentCount))
            let filledSegments = max(0, min(segmentCount, Int((fraction * Double(segmentCount)).rounded(.toNearestOrAwayFromZero))))

            HStack(spacing: spacing) {
                ForEach(0..<segmentCount, id: \.self) { index in
                    RoundedRectangle(cornerRadius: 2)
                        .fill(index < filledSegments ? fillColor : emptyColor)
                        .frame(width: segmentWidth)
                }
            }
        }
    }
}

private struct ClaudeCodeTokenSheet: View {
    let accountId: String
    let displayAccountId: String
    let email: String?
    @Binding var token: String
    let onCancel: () -> Void
    let onClear: () -> Void
    let onSave: () -> Void

    @AppStorage(AppSettings.emailAnonymousEnabledKey) private var emailAnonymousEnabled = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                UsageProviderIcon(provider: .claude, size: 16)
                Text("Claude Code Token")
                    .font(.system(size: 16, weight: .semibold))
                Spacer()
            }

            VStack(alignment: .leading, spacing: 6) {
                EmailPixelized(
                    isActive: emailAnonymousEnabled && hasEmail,
                    cacheKey: emailLine
                ) {
                    Text(emailLine)
                        .font(.system(size: 12, weight: .semibold, design: .monospaced))
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }

                Text(displayAccountId)
                    .font(.system(size: 11, weight: .semibold, design: .monospaced))
                    .foregroundColor(.secondary.opacity(0.85))
                    .lineLimit(1)
                    .truncationMode(.middle)
            }

            Text("Paste `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`. Stored locally and applied on profile switch. Not used for usage fetching.")
                .font(.system(size: 11))
                .foregroundColor(.secondary)

            SecureField("CLAUDE_CODE_OAUTH_TOKEN", text: $token)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 12, weight: .medium, design: .monospaced))

            HStack(spacing: 10) {
                Button("Cancel") { onCancel() }
                Spacer()
                Button("Clear", role: .destructive) { onClear() }
                Button("Save") { onSave() }
                    .disabled(token.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(16)
        .frame(width: 520)
    }

    private var emailLine: String {
        email?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil ?? "--"
    }

    /// Only mosaic a real email — the "--" placeholder stays readable.
    private var hasEmail: Bool {
        email?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmptyOrNil != nil
    }
}

private struct SaveProfileSheet: View {
    let isSaving: Bool
    @Binding var name: String
    let onCancel: () -> Void
    let onSave: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Save Profile")
                .font(.system(size: 16, weight: .semibold))

            VStack(alignment: .leading, spacing: 8) {
                Text("Profile Name")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundColor(.secondary)

                TextField("e.g. Work", text: $name)
                    .textFieldStyle(.roundedBorder)
            }

            Text("This snapshots your current Claude/Codex/Gemini CLI credentials into `~/.agent-island/accounts/` and links them to the profile.")
                .font(.system(size: 11))
                .foregroundColor(.secondary)

            HStack {
                Button("Cancel") { onCancel() }
                Spacer()
                Button(isSaving ? "Saving…" : "Save") { onSave() }
                    .disabled(isSaving || name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(16)
        .frame(width: 380)
    }
}
