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

    nonisolated static func prices() async throws -> PricesResult {
        try await detached { try call { gtt_prices() } }
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
