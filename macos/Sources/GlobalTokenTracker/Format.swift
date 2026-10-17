// Display formatting. Same precision rules as core's `viewmodel::fmt`, but
// expressed through Foundation's locale-aware styles so the Mac shell reads
// natively in each language (6.4K in English, 6.4万 in Chinese).
import Foundation
import SwiftUI

enum Fmt {
    /// Display currency converted from the ledger USD at core's cached rate
    /// (`fx`), applied on every load. The ledger itself never leaves USD.
    private static let fxLock = NSLock()
    private static var fxCode = "USD"
    private static var fxUsdTo = 1.0

    static func setDisplay(code: String, usdTo: Double) {
        fxLock.lock()
        fxCode = code
        fxUsdTo = usdTo > 0 ? usdTo : 1.0
        fxLock.unlock()
    }

    private static func display() -> (code: String, rate: Double) {
        fxLock.lock()
        defer { fxLock.unlock() }
        return (fxCode, fxUsdTo)
    }

    /// core's `fmt::usd` under the configured display currency: 0 decimals
    /// ≥ 100, 2 ≥ 1, else up to 4; trailing zeros trimmed.
    static func usd(_ v: Double) -> String {
        let (code, rate) = display()
        let plain = code == "USD" || rate == 1.0
        let value = plain ? v : v * rate
        let maxDigits = value >= 100 ? 0 : (value >= 1 ? 2 : 4)
        return value.formatted(
            .currency(code: plain ? "USD" : code).precision(.fractionLength(0...maxDigits))
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
        return date(ms: ms).formatted(longStamp)
    }

    /// Compact table timestamp — "10/12 14:00" (locale-ordered, 24 h); the
    /// full form goes in the tooltip.
    private static let shortStamp = Date.FormatStyle.dateTime
        .month(.twoDigits).day(.twoDigits).hour(.twoDigits(amPM: .omitted)).minute(.twoDigits)
    private static let longStamp = Date.FormatStyle(date: .abbreviated, time: .shortened)

    static func shortTimestamp(_ ms: Int64?) -> String {
        guard let ms else { return "—" }
        return date(ms: ms).formatted(shortStamp)
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

    /// Per-million price, trimmed ("$3", "$0.075") — display currency too.
    static func perMillion(_ v: Double) -> String {
        let (code, rate) = display()
        let plain = code == "USD" || rate == 1.0
        let value = plain ? v : v * rate
        return value.formatted(
            .currency(code: plain ? "USD" : code).precision(.fractionLength(0...4))
        )
    }

    private static let dayParser: DateFormatter = {
        let f = DateFormatter()
        f.calendar = Calendar(identifier: .gregorian)
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd"
        return f
    }()

    /// "YYYY-MM-DD" → days since 1970-01-01 (proleptic Gregorian), pure
    /// integer math — no DateFormatter on hot paths (heatmap layout/hover).
    static func epochDay(_ key: String) -> Int? {
        let u = Array(key.utf8)
        guard u.count == 10, u[4] == 45, u[7] == 45 else { return nil }  // '-'
        func num(_ r: Range<Int>) -> Int? {
            var v = 0
            for i in r { let d = Int(u[i]) - 48; guard (0...9).contains(d) else { return nil }; v = v * 10 + d }
            return v
        }
        guard let y0 = num(0..<4), let m = num(5..<7), let d = num(8..<10) else { return nil }
        let y = m <= 2 ? y0 - 1 : y0
        let era = (y >= 0 ? y : y - 399) / 400
        let yoe = y - era * 400
        let doy = (153 * (m > 2 ? m - 3 : m + 9) + 2) / 5 + d - 1
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy
        return era * 146_097 + doe - 719_468
    }

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
        case "freebuff": "Freebuff"
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

    /// `title` as a plain string (for views that take `String`).
    var titleText: String {
        switch key {
        case "today": String(localized: "Today")
        case "week": String(localized: "7 Days")
        case "month": String(localized: "30 Days")
        case "all": String(localized: "All Time")
        default: String(localized: "Custom")
        }
    }

    var isHourly: Bool { key == "today" }
}
