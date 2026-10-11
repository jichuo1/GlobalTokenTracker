// Display formatting. Same precision rules as core's `viewmodel::fmt`, but
// expressed through Foundation's locale-aware styles so the Mac shell reads
// natively in each language (6.4K in English, 6.4万 in Chinese).
import Foundation
import SwiftUI

enum Fmt {
    /// core's `fmt::usd`: 0 decimals ≥ $100, 2 ≥ $1, else up to 4; trailing zeros trimmed.
    static func usd(_ v: Double) -> String {
        let maxDigits = v >= 100 ? 0 : (v >= 1 ? 2 : 4)
        return v.formatted(
            .currency(code: "USD").precision(.fractionLength(0...maxDigits))
        )
    }

    static func tokensCompact(_ n: UInt64) -> String {
        Int64(clamping: n).formatted(.number.notation(.compactName).precision(.fractionLength(0...1)))
    }

    static func tokensExact(_ n: UInt64) -> String {
        Int64(clamping: n).formatted(.number)
    }

    static func count(_ n: UInt64) -> String {
        Int64(clamping: n).formatted(.number)
    }

    static func percent(_ v: Double) -> String {
        (v / 100).formatted(.percent.precision(.fractionLength(0...1)))
    }

    static func date(ms: Int64) -> Date {
        Date(timeIntervalSince1970: TimeInterval(ms) / 1000)
    }

    static func timestamp(_ ms: Int64?) -> String {
        guard let ms else { return "—" }
        return date(ms: ms).formatted(date: .abbreviated, time: .shortened)
    }

    static func relative(_ ms: Int64?) -> String {
        guard let ms else { return String(localized: "Never") }
        return date(ms: ms).formatted(.relative(presentation: .named))
    }

    /// Active / session duration, compact ("1h 12m", "45s").
    static func duration(ms: UInt64) -> String {
        Duration.milliseconds(Int64(clamping: ms)).formatted(
            .units(allowed: [.hours, .minutes, .seconds], width: .narrow, maximumUnitCount: 2)
        )
    }

    /// Per-million price, trimmed ("$3", "$0.075").
    static func perMillion(_ v: Double) -> String {
        v.formatted(.currency(code: "USD").precision(.fractionLength(0...4)))
    }

    private static let dayParser: DateFormatter = {
        let f = DateFormatter()
        f.calendar = Calendar(identifier: .gregorian)
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd"
        return f
    }()

    /// Bucket keys from core: "YYYY-MM-DD" (daily) or "HH:00" (hourly).
    static func bucketDate(_ key: String) -> Date? {
        dayParser.date(from: key)
    }

    /// Quota window ids (spec §6.9) → localized label; unknown ids pass through.
    static func quotaKind(_ kind: String) -> String {
        switch kind {
        case "5h_block": String(localized: "5-hour window")
        case "weekly": String(localized: "Weekly limit")
        case "monthly": String(localized: "Monthly limit")
        case "credits": String(localized: "Credits left")
        case "billing_period": String(localized: "Billing period")
        case "auto_pool": String(localized: "Auto pool")
        case "api_pool": String(localized: "API pool")
        case "session_ctx": String(localized: "Session context")
        default: kind
        }
    }

    /// Tool id → display name (core's `viewmodel::app_display`).
    static func app(_ id: String) -> String {
        switch id {
        case "claude": "Claude"
        case "codex": "Codex"
        case "cursor": "Cursor"
        case "workbuddy": "WorkBuddy"
        case "codebuddy_ide": "CodeBuddy"
        case "codebuddy_cli": "CodeBuddy CLI"
        case "qoder": "Qoder"
        case "opencode": "OpenCode"
        case "zcode": "ZCode"
        case "grok": "Grok"
        case "devin": "Devin"
        case "minimax_code": "MiniMax Code"
        case "kimi_code": "Kimi Code"
        case "cline": "Cline"
        case "commandcode": "Command Code"
        case "gemini_antigravity": "Antigravity"
        case "dsh": "DeepSeek Harness"
        default: id
        }
    }
}

extension RangeSpec {
    var title: LocalizedStringKey {
        switch key {
        case "today": "Today"
        case "week": "7 Days"
        case "month": "30 Days"
        case "all": "All Time"
        default: "Custom"
        }
    }

    var isHourly: Bool { key == "today" }
}
