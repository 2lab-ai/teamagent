//
//  Settings.swift
//  ClaudeIsland
//
//  App settings manager using UserDefaults
//

import Foundation

/// Available notification sounds
enum NotificationSound: String, CaseIterable {
    case none = "None"
    case pop = "Pop"
    case ping = "Ping"
    case tink = "Tink"
    case glass = "Glass"
    case blow = "Blow"
    case bottle = "Bottle"
    case frog = "Frog"
    case funk = "Funk"
    case hero = "Hero"
    case morse = "Morse"
    case purr = "Purr"
    case sosumi = "Sosumi"
    case submarine = "Submarine"
    case basso = "Basso"

    /// The system sound name to use with NSSound, or nil for no sound
    var soundName: String? {
        self == .none ? nil : rawValue
    }
}

enum AppSettings {
    private static let defaults = UserDefaults.standard

    // MARK: - Keys

    private enum Keys {
        static let notificationSound = "notificationSound"
        static let usageResetAlertsEnabled = "usageResetAlertsEnabled"
        static let emailAnonymousEnabled = "emailAnonymousEnabled"
    }

    // MARK: - Notification Sound

    /// The sound to play when Claude finishes and is ready for input
    static var notificationSound: NotificationSound {
        get {
            guard let rawValue = defaults.string(forKey: Keys.notificationSound),
                  let sound = NotificationSound(rawValue: rawValue) else {
                return .pop // Default to Pop
            }
            return sound
        }
        set {
            defaults.set(newValue.rawValue, forKey: Keys.notificationSound)
        }
    }

    // MARK: - Usage Reset Alerts

    /// Whether to show usage reset timer alerts (opens the island when a reset is near).
    static var usageResetAlertsEnabled: Bool {
        get {
            if defaults.object(forKey: Keys.usageResetAlertsEnabled) == nil {
                return true
            }
            return defaults.bool(forKey: Keys.usageResetAlertsEnabled)
        }
        set {
            defaults.set(newValue, forKey: Keys.usageResetAlertsEnabled)
        }
    }

    // MARK: - Email Anonymous

    /// UserDefaults key for the email-anonymous setting, exposed so SwiftUI
    /// views can observe it live via `@AppStorage`.
    static let emailAnonymousEnabledKey = Keys.emailAnonymousEnabled

    /// Whether emails in the Usage area are mosaic-pixelized to illegibility.
    /// Defaults to off — emails render as-is.
    static var emailAnonymousEnabled: Bool {
        get {
            defaults.bool(forKey: Keys.emailAnonymousEnabled)
        }
        set {
            defaults.set(newValue, forKey: Keys.emailAnonymousEnabled)
        }
    }
}
