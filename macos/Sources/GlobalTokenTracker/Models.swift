// Mirrors of core's serialized view models (crates/core/src/viewmodel.rs,
// store/query.rs). Field names match the JSON via snake_case decoding, so
// these stay a straight transcription of the Rust structs — no logic here.
import Foundation

struct RangeSpec: Codable, Hashable, Sendable {
    var key: String
    var startMs: Int64?
    var endMs: Int64?

    static let today = RangeSpec(key: "today")
    static let week = RangeSpec(key: "week")
    static let month = RangeSpec(key: "month")
    static let all = RangeSpec(key: "all")
    static let presets = [today, week, month, all]

    init(key: String, startMs: Int64? = nil, endMs: Int64? = nil) {
        self.key = key
        self.startMs = startMs
        self.endMs = endMs
    }
}

struct Totals: Decodable, Sendable, Equatable {
    var events: UInt64 = 0
    var inputTokens: UInt64 = 0
    var outputTokens: UInt64 = 0
    var reasoningTokens: UInt64 = 0
    var cacheReadTokens: UInt64 = 0
    var cacheWriteTokens: UInt64 = 0
    var unclassifiedTokens: UInt64 = 0
    var credits: Double = 0
    var costUsd: Double = 0
    var activeMs: UInt64 = 0

    /// Same headline as core's `fmt::tokens_total`.
    var tokens: UInt64 {
        inputTokens + outputTokens + cacheReadTokens + cacheWriteTokens + unclassifiedTokens
    }
}

struct AppSummary: Decodable, Sendable, Identifiable, Equatable {
    var app: String
    var events: UInt64
    var inputTokens: UInt64
    var outputTokens: UInt64
    var reasoningTokens: UInt64
    var cacheReadTokens: UInt64
    var cacheWriteTokens: UInt64
    var unclassifiedTokens: UInt64
    var credits: Double
    var costUsd: Double
    var id: String { app }
    var tokens: UInt64 {
        inputTokens + outputTokens + cacheReadTokens + cacheWriteTokens + unclassifiedTokens
    }
}

struct ShareRow: Decodable, Sendable, Identifiable, Equatable {
    var name: String
    var events: UInt64
    var tokens: UInt64
    var costUsd: Double
    var id: String { name }
}

/// Rust `(String, u64)` tuples arrive as two-element arrays.
struct NamedCount: Decodable, Sendable, Equatable {
    var name: String
    var count: UInt64
    init(from decoder: Decoder) throws {
        var c = try decoder.unkeyedContainer()
        name = try c.decode(String.self)
        count = try c.decode(UInt64.self)
    }
}

struct TrendBucket: Decodable, Sendable, Identifiable, Equatable {
    var date: String
    var events: UInt64
    var tokens: UInt64
    var costUsd: Double
    var top: [NamedCount]
    var id: String { date }
}

struct HeatDay: Decodable, Sendable, Identifiable, Equatable {
    var date: String
    var tokens: UInt64
    var costUsd: Double
    var events: UInt64
    var durationMs: UInt64
    var id: String { date }
}

struct QuotaRow: Decodable, Sendable, Identifiable, Equatable {
    var app: String
    var account: String?
    var capturedAt: Int64
    var windowKind: String
    var used: Double?
    var limitValue: Double?
    var usedPercent: Double?
    var resetsAt: Int64?
    var id: String { "\(app)|\(account ?? "")|\(windowKind)" }
}

struct QuotaGroup: Decodable, Sendable, Identifiable, Equatable {
    var app: String
    var display: String
    var worstPct: Double?
    var rows: [QuotaRow]
    var id: String { app }
}

struct OverviewVM: Decodable, Sendable, Equatable {
    var today: Totals
    var span: Totals
    var all: Totals
    var range: RangeSpec
    var byApp: [AppSummary]
    var byModel: [ShareRow]
    var daily: [TrendBucket]
    var apps: [String]
    var models: [String]
    var quotas: [QuotaRow]
    var quotaGroups: [QuotaGroup]
    var unpriced: [NamedCount]
    var heat: [HeatDay]
    var tzOffset: String
}

struct EventRow: Decodable, Sendable, Identifiable, Equatable {
    var app: String
    var model: String?
    var pricingModel: String?
    var project: String?
    var sessionId: String?
    var tsStart: Int64?
    var inputTokens: UInt64
    var outputTokens: UInt64
    var reasoningTokens: UInt64
    var cacheReadTokens: UInt64
    var cacheWriteTokens: UInt64
    var unclassifiedTokens: UInt64
    var credits: Double?
    var costUsd: Double?
    var costSource: String?
    var durationMs: Int64?
    var rawRef: String?

    /// raw_ref is the event's provenance pointer — unique per ledger row.
    var id: String { rawRef ?? "\(app)|\(sessionId ?? "")|\(tsStart ?? 0)|\(model ?? "")" }
    var tokens: UInt64 {
        inputTokens + outputTokens + cacheReadTokens + cacheWriteTokens + unclassifiedTokens
    }
}

struct DetailBundle: Decodable, Sendable, Equatable {
    var rows: [EventRow]
    var total: UInt64
    var pageSize: Int64
}

struct ScanSummary: Decodable, Sendable, Equatable {
    var filesSeen: UInt64?
    var eventsIngested: UInt64?
    var quotas: UInt64?
    var errors: [String]
}

/// The trend card's drawn series (core::trend::drawn): at most 60 buckets
/// plus the 7-bucket moving average aligned to them.
struct TrendSeries: Decodable, Sendable, Equatable {
    var buckets: [TrendBucket]
    var avg: [Double]
}

struct LoadResult: Decodable, Sendable {
    var fresh: Bool
    var scanMs: UInt64
    var priceDue: Bool
    var scan: ScanSummary
    var vm: OverviewVM?
    var detail: DetailBundle?
    var trend: TrendSeries?
    /// vm.quotas ordered most-pressing first (core::viewmodel::rank_quotas).
    var quotasRanked: [QuotaRow]?
}

struct DetailPage: Decodable, Sendable {
    var rows: [EventRow]
}

struct SourceHealth: Decodable, Sendable, Identifiable, Equatable {
    var source: String
    var enabled: Bool
    var lastSyncedAt: Int64?
    var lastError: String?
    var filesSeen: UInt64
    var rowsIngested: UInt64
    var cursors: UInt64
    var id: String { source }
}

struct AdapterInfo: Decodable, Sendable, Equatable {
    var id: String
    var name: String
}

struct SourcesResult: Decodable, Sendable {
    var health: [SourceHealth]
    var adapters: [AdapterInfo]
}

struct PriceQuote: Decodable, Sendable, Equatable {
    var source: String
    var key: String
    var input: Double
    var output: Double
    var cacheRead: Double
    var stance: String
}

struct PriceRow: Decodable, Sendable, Identifiable, Equatable {
    var model: String
    var input: Double
    var output: Double
    var cacheRead: Double
    var cacheWrite: Double
    var source: String
    var agree: UInt32
    var total: UInt32
    var quotes: [PriceQuote]
    /// Sources disagree on this model (core's PriceRow::disputed).
    var disputed: Bool
    var id: String { model }
}

struct PricesResult: Decodable, Sendable, Equatable {
    var rows: [PriceRow]
    /// Unfiltered model count.
    var total: Int
    var syncedAt: Int64?
}

struct ExternalInterfaceState: Decodable, Sendable, Equatable {
    var enabled: Bool
    var invalid: Bool
    var error: String?
    var path: String
}

struct DownloadedUpdate: Decodable, Sendable {
    var version: String
    var path: String
}

struct PriceRefreshResult: Decodable, Sendable {
    var failed: [String]
    var repriced: UInt64
    var rulesRepass: Bool
}

struct QuotaPollResult: Decodable, Sendable {
    var inserted: UInt64
    var errors: [String]
}

struct UpdateInfo: Decodable, Sendable, Equatable {
    var tag: String
    var version: String
    var htmlUrl: String
    var assetName: String
}

struct UpdateCheck: Decodable, Sendable {
    var current: String
    var update: UpdateInfo?
}

struct CoreInfo: Decodable, Sendable {
    var version: String
    var dbPath: String
    var tzOffset: String
}

/// core's read-back of a statistics window (`gtt_range`): the inclusive day
/// span, the hour span of a single-day window, and core's notion of "now"
/// for disabling future days / hours.
struct RangeInfo: Decodable, Sendable, Equatable {
    var range: RangeSpec
    var firstDay: String?
    var lastDay: String?
    var days: Int?
    var hourSpan: [Int]?
    var hourly: Bool
    var today: String
    var nowHour: Int
}
