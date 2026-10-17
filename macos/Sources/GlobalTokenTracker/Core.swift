// Thin Swift face of the Rust engine (crates/ffi). Every call is blocking
// and must run off the main actor; callers use `Task.detached` / `async`
// wrappers below. Strings returned by Rust are copied then freed at once.
import CGTT
import Foundation

enum CoreError: LocalizedError {
    case engine(String)
    case nullResult

    var errorDescription: String? {
        switch self {
        case .engine(let msg): msg
        case .nullResult: "engine returned no data"
        }
    }
}

struct LoadRequest: Encodable, Sendable {
    var range: RangeSpec
    var apps: [String]?
    var models: [String]?
    var scan = false
    var forceViews = false
    var rebuild = false
    var forcePrices = false
}

struct PageRequest: Encodable, Sendable {
    var offset: Int64
    var limit: Int64?
    var apps: [String]?
    var models: [String]?
}

enum Core {
    private struct Envelope<T: Decodable>: Decodable {
        var ok: Bool
        var data: T?
        var error: String?
    }

    private static func decoder() -> JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }

    private static func encode<R: Encodable>(_ req: R) -> String {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        return (try? String(decoding: e.encode(req), as: UTF8.self)) ?? "{}"
    }

    /// Run one FFI call, take ownership of the returned C string, decode.
    private static func call<T: Decodable>(_ f: () -> UnsafeMutablePointer<CChar>?) throws -> T {
        guard let p = f() else { throw CoreError.nullResult }
        defer { gtt_free(p) }
        // No copy into a Swift String: decode straight from the C buffer.
        let data = Data(bytesNoCopy: p, count: strlen(p), deallocator: .none)
        let env = try decoder().decode(Envelope<T>.self, from: data)
        guard env.ok, let value = env.data else {
            throw CoreError.engine(env.error ?? "unknown engine error")
        }
        if let r = value as? LoadResult, let fx = r.fx {
            Fmt.setDisplay(code: fx.display, usdTo: fx.usdTo)
        }
        return value
    }

    nonisolated static func load(_ req: LoadRequest) async throws -> LoadResult {
        let json = encode(req)
        return try await detached { try call { json.withCString { gtt_load($0) } } }
    }

    nonisolated static func detailPage(_ req: PageRequest) async throws -> DetailPage {
        let json = encode(req)
        return try await detached { try call { json.withCString { gtt_detail_page($0) } } }
    }

    nonisolated static func sources() async throws -> SourcesResult {
        try await detached { try call { gtt_sources() } }
    }

    struct PricesRequest: Encodable, Sendable {
        var query = ""
        var disputedOnly = false
        var reload = false
        /// The table version already held (rows are left out when current).
        var known: UInt64?
    }

    nonisolated static func prices(_ req: PricesRequest) async throws -> PricesReply {
        let json = encode(req)
        return try await detached { try call { json.withCString { gtt_prices($0) } } }
    }

    /// nil reads the switch; a value writes it, then reports the state on disk.
    nonisolated static func externalInterface(set: Bool?) async throws -> ExternalInterfaceState {
        let arg: Int32 = set.map { $0 ? 1 : 0 } ?? -1
        return try await detached(priority: .utility) { try call { gtt_external_interface(arg) } }
    }

    nonisolated static func fxStatus() async throws -> FxStatus {
        try await detached(priority: .utility) { try call { gtt_fx_status() } }
    }

    nonisolated static func setDisplayCurrency(_ code: String) async throws -> FxStatus {
        try await detached(priority: .utility) {
            try call { code.withCString { gtt_set_display_currency($0) } }
        }
    }

    nonisolated static func downloadUpdate(channel: String) async throws -> DownloadedUpdate {
        try await detached(priority: .utility) {
            try call { channel.withCString { gtt_download_update($0) } }
        }
    }

    nonisolated static func refreshPrices() async throws -> PriceRefreshResult {
        try await detached(priority: .utility) { try call { gtt_refresh_prices() } }
    }

    nonisolated static func pollQuotas() async throws -> QuotaPollResult {
        try await detached(priority: .utility) { try call { gtt_poll_quotas() } }
    }

    nonisolated static func watchRoots() async throws -> [String] {
        try await detached(priority: .utility) { try call { gtt_watch_roots() } }
    }

    nonisolated static func checkUpdate(channel: String) async throws -> UpdateCheck {
        try await detached(priority: .utility) {
            try call { channel.withCString { gtt_check_update($0) } }
        }
    }

    /// Custom-window ops (`days` / `hours` / `quick` / `describe`). Pure time
    /// math in core — no ledger I/O — so this one is synchronous.
    enum WindowOp {
        case days(first: String, last: String)
        case hours(day: String, h1: Int, h2: Int)
        case quick(String)
        case describe(RangeSpec)
    }

    static func window(_ op: WindowOp) throws -> RangeInfo {
        let req: [String: Any] = switch op {
        case let .days(first, last): ["op": "days", "first": first, "last": last]
        case let .hours(day, h1, h2): ["op": "hours", "day": day, "h1": h1, "h2": h2]
        case let .quick(key): ["op": "quick", "key": key]
        case let .describe(r): ["op": "describe", "range": rangeObject(r)]
        }
        let json = String(decoding: try JSONSerialization.data(withJSONObject: req), as: UTF8.self)
        return try call { json.withCString { gtt_range($0) } }
    }

    /// JSONSerialization can't encode Optionals: include only present bounds.
    private static func rangeObject(_ r: RangeSpec) -> [String: Any] {
        var o: [String: Any] = ["key": r.key]
        if let s = r.startMs { o["start_ms"] = s }
        if let e = r.endMs { o["end_ms"] = e }
        return o
    }

    // MARK: LAN devices (gtt_peer)

    /// One `gtt_peer` op. Network ops (sync / pair / discover) take seconds:
    /// they run detached at utility priority like every other engine call.
    /// Failures carry core's fixed code (`CoreError.engine("pairing_failed")`).
    private static func peer<T: Decodable & Sendable>(_ op: [String: Any]) async throws -> T {
        let json = String(decoding: try JSONSerialization.data(withJSONObject: op), as: UTF8.self)
        return try await detached(priority: .utility) {
            try call { json.withCString { gtt_peer($0) } }
        }
    }

    nonisolated static func peerList() async throws -> PeerList {
        try await peer(["op": "list"])
    }

    /// `ifDue`: the timer hook — runs only when the last sync is old enough.
    nonisolated static func peerSync(ifDue: Bool) async throws -> SyncResult {
        try await peer(["op": "sync", "if_due": ifDue])
    }

    struct FilterResult: Decodable, Sendable { var filter: [String]? }

    /// nil = all devices.
    nonisolated static func setDeviceFilter(_ ids: [String]?) async throws -> FilterResult {
        try await peer(["op": "set_filter", "devices": ids.map { $0 as Any } ?? NSNull()])
    }

    struct Ack: Decodable, Sendable {}

    nonisolated static func forgetDevice(_ id: String) async throws -> Ack {
        try await peer(["op": "forget", "device": id])
    }

    nonisolated static func setDeviceEnabled(_ id: String, _ on: Bool) async throws -> Ack {
        try await peer(["op": "set_enabled", "device": id, "enabled": on])
    }

    nonisolated static func pair(address: String, code: String, name: String?) async throws -> PairResult {
        var op: [String: Any] = ["op": "pair", "addr": address, "code": code]
        if let name { op["name"] = name }
        return try await peer(op)
    }

    struct NameResult: Decodable, Sendable { var name: String }

    /// How other devices see this Mac.
    nonisolated static func setDeviceName(_ name: String) async throws -> NameResult {
        try await peer(["op": "set_name", "name": name])
    }

    // Sharing this Mac (core::peer::share): server + Bonjour + pairing code.

    nonisolated static func shareStart() async throws -> ShareInfo {
        try await peer(["op": "share_start"])
    }

    nonisolated static func shareStop() async throws -> ShareInfo {
        try await peer(["op": "share_stop"])
    }

    /// Synchronous stop for app termination (no time for a task hop).
    nonisolated static func shareStopNow() {
        let _: ShareInfo? = try? call { #"{"op":"share_stop"}"#.withCString { gtt_peer($0) } }
    }

    nonisolated static func shareStatus() async throws -> ShareInfo {
        try await peer(["op": "share_status"])
    }

    nonisolated static func shareNewCode() async throws -> ShareInfo {
        try await peer(["op": "share_new_code"])
    }

    nonisolated static func revokeViewer(_ id: String) async throws -> ShareInfo {
        try await peer(["op": "revoke", "viewer": id])
    }

    nonisolated static func discover(seconds: Int = 3) async throws -> DiscoverResult {
        try await peer(["op": "discover", "timeout_secs": seconds])
    }

    nonisolated static func info() async throws -> CoreInfo {
        try await detached { try call { gtt_info() } }
    }

    private static func detached<T: Sendable>(
        priority: TaskPriority = .userInitiated,
        _ body: @escaping @Sendable () throws -> T
    ) async throws -> T {
        try await Task.detached(priority: priority, operation: body).value
    }
}
