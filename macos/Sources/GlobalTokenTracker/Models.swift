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
    /// This Mac first, then paired devices, each with its span totals.
    var devices: [DeviceVM] = []
    /// Device ids the aggregates cover; nil = all devices.
    var deviceFilter: [String]?
    /// Freshest quota per app/account/window across devices (never summed).
    var quotasAllDevices: [MergedQuota] = []
    var deviceWarnings: [OverlapWarning] = []

    private enum CodingKeys: String, CodingKey {
        case today, span, all, range, byApp, byModel, daily, apps, models, quotas, quotaGroups
        case unpriced, heat, tzOffset, devices, deviceFilter, quotasAllDevices, deviceWarnings
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        today = try c.decode(Totals.self, forKey: .today)
        span = try c.decode(Totals.self, forKey: .span)
        all = try c.decode(Totals.self, forKey: .all)
        range = try c.decode(RangeSpec.self, forKey: .range)
        byApp = try c.decode([AppSummary].self, forKey: .byApp)
        byModel = try c.decode([ShareRow].self, forKey: .byModel)
        daily = try c.decode([TrendBucket].self, forKey: .daily)
        apps = try c.decode([String].self, forKey: .apps)
        models = try c.decode([String].self, forKey: .models)
        quotas = try c.decode([QuotaRow].self, forKey: .quotas)
        quotaGroups = try c.decode([QuotaGroup].self, forKey: .quotaGroups)
        unpriced = try c.decode([NamedCount].self, forKey: .unpriced)
        heat = try c.decode([HeatDay].self, forKey: .heat)
        tzOffset = try c.decode(String.self, forKey: .tzOffset)
        devices = try c.decodeIfPresent([DeviceVM].self, forKey: .devices) ?? []
        deviceFilter = try c.decodeIfPresent([String].self, forKey: .deviceFilter)
        quotasAllDevices = try c.decodeIfPresent([MergedQuota].self, forKey: .quotasAllDevices) ?? []
        deviceWarnings = try c.decodeIfPresent([OverlapWarning].self, forKey: .deviceWarnings) ?? []
    }

    /// Paired LAN devices exist (the device dimension is worth showing).
    var hasPeers: Bool { devices.count > 1 }

    func deviceName(_ id: String) -> String {
        devices.first { $0.deviceId == id }?.displayName ?? String(id.prefix(8))
    }
}

// MARK: - LAN devices (core::peer, viewmodel::DeviceVm)

struct DeviceVM: Decodable, Sendable, Identifiable, Equatable {
    var deviceId: String
    var name: String
    var local: Bool
    var selected: Bool
    var enabled: Bool
    var lastSeenAt: Int64?
    var lastPullAt: Int64?
    var lastErrorCode: String?
    var span: Totals
    var id: String { deviceId }

    /// core names an unnamed local device "This device"; show "This Mac".
    var displayName: String { local ? LocalDevice.display(name) : name }
}

struct MergedQuota: Decodable, Sendable, Identifiable, Equatable {
    var app: String
    var accountKey: String?
    var windowKind: String
    var capturedAt: Int64
    var used: Double?
    var limitValue: Double?
    var usedPercent: Double?
    var resetsAt: Int64?
    var deviceId: String
    var deviceName: String
    var local: Bool
    var id: String { "\(app)|\(accountKey ?? "")|\(windowKind)" }

    /// As a QuotaRow for the shared meter (the account is a pseudonym).
    var row: QuotaRow {
        QuotaRow(app: app, account: nil, capturedAt: capturedAt, windowKind: windowKind,
                 used: used, limitValue: limitValue, usedPercent: usedPercent, resetsAt: resetsAt)
    }
}

struct OverlapWarning: Decodable, Sendable, Identifiable, Equatable {
    var code: String
    var deviceA: String
    var deviceB: String
    var app: String
    var days: Int
    var matchingHours: Int
    var id: String { "\(deviceA)|\(deviceB)|\(app)" }
}

/// A paired device as the ledger's `peers` table holds it.
struct PeerDevice: Decodable, Sendable, Identifiable, Equatable {
    var deviceId: String
    var name: String
    var fingerprint: String
    var lastAddress: String
    var pairedAt: Int64
    var lastSeenAt: Int64?
    var lastPullAt: Int64?
    var lastErrorCode: String?
    var gttVersion: String?
    var protocolVersion: UInt32?
    var enabled: Bool
    var id: String { deviceId }
}

struct LocalDevice: Decodable, Sendable, Equatable {
    var deviceId: String
    var name: String

    var displayName: String { Self.display(name) }

    static func display(_ name: String) -> String {
        name == "This device" ? String(localized: "This Mac") : name
    }
}

struct PeerList: Decodable, Sendable, Equatable {
    var device: LocalDevice
    var peers: [PeerDevice]
    var warnings: [OverlapWarning]
    var filter: [String]?
}

struct DeviceSync: Decodable, Sendable, Equatable {
    var deviceId: String
    var name: String
    var ok: Bool
    var errorCode: String?
    var daysChanged: Int
    var daysRemoved: Int
}

struct SyncReport: Decodable, Sendable, Equatable {
    var devices: [DeviceSync]
    var changed: Bool
}

struct SyncResult: Decodable, Sendable {
    var ran: Bool
    var report: SyncReport?
}

/// One `_gtt._tcp` answer set (core::peer::discovery::Found).
struct FoundDevice: Decodable, Sendable, Identifiable, Equatable {
    var deviceId: String
    var name: String
    var protocolVersion: UInt32
    var addresses: [String]
    var port: UInt16
    var fingerprintPrefix: String
    var paired: Bool
    var warnings: [String]
    var id: String { "\(deviceId)|\(fingerprintPrefix)" }
}

struct DiscoverResult: Decodable, Sendable {
    var devices: [FoundDevice]
    var invalidAnswers: Int
}

struct PairResult: Decodable, Sendable {
    var device: PeerDevice
    /// The other device can read this Mac too (core::peer::pairing).
    var mutual: Bool
    /// Pairing turned sharing on (mutual pairing needs this Mac to share).
    var sharingStarted: Bool
}

/// This Mac's LAN share (core::peer::share::ShareStatus).
struct ShareStatus: Decodable, Sendable, Equatable {
    var deviceId: String
    var deviceName: String
    var fingerprint: String
    var port: UInt16
    /// What another device types into Add Device.
    var addresses: [String]
    var advertised: Bool
    var code: String?
    var codeExpiresIn: UInt64?
}

/// A device allowed to read this Mac (core::peer::trust::Viewer).
struct ShareViewer: Decodable, Sendable, Identifiable, Equatable {
    var id: String
    var name: String
    var createdAt: Int64
    var lastSeenAt: Int64?
    var revokedAt: Int64?
}

struct ShareInfo: Decodable, Sendable, Equatable {
    var sharing: Bool
    var status: ShareStatus?
    var viewers: [ShareViewer]
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
    /// Event rows are always this Mac's — peers share aggregates only.
    var deviceId: String?
    var deviceName: String?
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

struct SourcesResult: Decodable, Sendable, Equatable {
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

/// Core's price reply: the whole table only when the held version is stale,
/// a search as positions into it.
struct PricesReply: Decodable, Sendable {
    var rows: [PriceRow]?
    var shown: [Int]?
    var total: Int
    var syncedAt: Int64?
    var version: UInt64
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
