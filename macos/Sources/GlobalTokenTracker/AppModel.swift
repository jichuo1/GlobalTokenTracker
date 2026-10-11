// App state + refresh orchestration — the Mac counterpart of the WinUI
// shell's `Shell` message loop. Same cadence rules (crates/ui/src/config.rs):
// refresh 0 = file-watcher only, otherwise a timer (watcher off, so bursts of
// writes never defeat the chosen cadence); quotas every 30 min; prices when
// core says they're due.
//
// Performance contract:
// - every engine call is off the main actor (Core is nonisolated);
// - at most one load in flight; requests arriving meanwhile coalesce into a
//   single follow-up (`pending`), so a watcher storm costs one extra pass;
// - unchanged scans return `fresh:false` and touch no observable state, so
//   SwiftUI re-renders nothing;
// - published values are assigned only when they differ (Equatable VMs).
import Foundation
import Observation
import SwiftUI

enum Page: String, CaseIterable, Identifiable, Hashable {
    case overview, detail, quota, sources, prices
    var id: String { rawValue }

    var title: LocalizedStringKey {
        switch self {
        case .overview: "Overview"
        case .detail: "Details"
        case .quota: "Quota"
        case .sources: "Sources"
        case .prices: "Prices"
        }
    }

    var symbol: String {
        switch self {
        case .overview: "chart.bar.xaxis"
        case .detail: "list.bullet.rectangle"
        case .quota: "gauge.with.dots.needle.67percent"
        case .sources: "externaldrive.connected.to.line.below"
        case .prices: "dollarsign.circle"
        }
    }
}

@MainActor
@Observable
final class AppModel {
    static let quotaPollSeconds: UInt64 = 30 * 60
    static let refreshOptions: [Int] = [0, 10, 30, 60, 300]

    // MARK: Data
    private(set) var vm: OverviewVM?
    private(set) var detailRows: [EventRow] = []
    private(set) var detailTotal: UInt64 = 0
    private(set) var detailLoadingMore = false
    private(set) var sources: SourcesResult?
    private(set) var prices: PricesResult?
    private(set) var info: CoreInfo?
    private(set) var update: UpdateCheck?

    // MARK: Status
    private(set) var isLoading = false
    private(set) var isScanning = false
    private(set) var isRefreshingPrices = false
    private(set) var lastScanMs: UInt64?
    private(set) var lastScanAt: Date?
    private(set) var lastError: String?
    private(set) var priceError: String?

    // MARK: Selection (persisted)
    var page: Page = .overview
    var range: RangeSpec {
        didSet { persist(); requestLoad(forceViews: true) }
    }
    var appFilter: Set<String>? {
        didSet { persist(); requestLoad(forceViews: true, resetDetail: true) }
    }
    var modelFilter: Set<String>? {
        didSet { persist(); requestLoad(forceViews: true, resetDetail: true) }
    }
    var refreshSeconds: Int {
        didSet { persist(); armRefresh() }
    }
    var updateChannel: String {
        didSet { persist() }
    }

    // MARK: Internals
    private let defaults = UserDefaults.standard
    private var started = false
    private var pending: LoadRequest?
    private var refreshTask: Task<Void, Never>?
    private var quotaTask: Task<Void, Never>?
    private var watcher: FileWatcher?
    private var pricesDueHandled = false
    /// Filters changed: the next fresh detail page replaces scrolled pages.
    private var detailNeedsReset = true

    init() {
        let d = UserDefaults.standard
        let key = d.string(forKey: "range") ?? "week"
        range = RangeSpec(
            key: key,
            startMs: d.object(forKey: "rangeStart") as? Int64,
            endMs: d.object(forKey: "rangeEnd") as? Int64
        )
        appFilter = (d.array(forKey: "appFilter") as? [String]).map(Set.init)
        modelFilter = (d.array(forKey: "modelFilter") as? [String]).map(Set.init)
        refreshSeconds = d.object(forKey: "refreshSeconds") as? Int ?? 30
        updateChannel = d.string(forKey: "updateChannel") ?? "stable"
    }

    private func persist() {
        defaults.set(range.key, forKey: "range")
        defaults.set(range.startMs, forKey: "rangeStart")
        defaults.set(range.endMs, forKey: "rangeEnd")
        defaults.set(appFilter.map { Array($0).sorted() }, forKey: "appFilter")
        defaults.set(modelFilter.map { Array($0).sorted() }, forKey: "modelFilter")
        defaults.set(refreshSeconds, forKey: "refreshSeconds")
        defaults.set(updateChannel, forKey: "updateChannel")
    }

    // MARK: Lifecycle

    /// First paint straight from the ledger (no scan), then scan — same as
    /// the WinUI startup path, so the window never waits on the filesystem.
    func start() {
        guard !started else { return }
        started = true
        Task {
            info = try? await Core.info()
            await runLoad(base(scan: false, forceViews: true, forcePrices: true))
            await runLoad(base(scan: true))
            armRefresh()
            armQuota()
        }
    }

    func manualRefresh() {
        requestLoad(scan: true, forceViews: true, rebuild: true)
        if page == .sources { loadSources() }
    }

    // MARK: Loading

    private func base(
        scan: Bool, forceViews: Bool = false, rebuild: Bool = false, forcePrices: Bool = false
    ) -> LoadRequest {
        LoadRequest(
            range: range,
            apps: appFilter.map { Array($0).sorted() },
            models: modelFilter.map { Array($0).sorted() },
            scan: scan, forceViews: forceViews, rebuild: rebuild, forcePrices: forcePrices
        )
    }

    /// Queue a load; while one is running, requests merge into one follow-up.
    func requestLoad(
        scan: Bool = false, forceViews: Bool = false, rebuild: Bool = false,
        resetDetail: Bool = false
    ) {
        guard started else { return }
        if resetDetail { detailNeedsReset = true }
        var req = base(scan: scan, forceViews: forceViews, rebuild: rebuild)
        if isLoading {
            if let p = pending {
                req.scan = req.scan || p.scan
                req.forceViews = req.forceViews || p.forceViews
                req.rebuild = req.rebuild || p.rebuild
            }
            pending = req
            return
        }
        Task { await runLoad(req) }
    }

    private func runLoad(_ req: LoadRequest) async {
        isLoading = true
        isScanning = req.scan
        defer {
            isLoading = false
            isScanning = false
        }
        do {
            let r = try await Core.load(req)
            if req.scan {
                lastScanMs = r.scanMs
                lastScanAt = .now
            }
            if let vm = r.vm, vm != self.vm { self.vm = vm }
            if let d = r.detail {
                mergeDetail(d)
            }
            if lastError != nil { lastError = nil }
            if r.priceDue && !pricesDueHandled {
                pricesDueHandled = true
                refreshPrices()
            }
        } catch {
            lastError = error.localizedDescription
        }
        if let next = pending {
            pending = nil
            await runLoad(next)
        }
    }

    /// Keep pages the user already scrolled through: a refresh only prepends
    /// rows that are new since the last load (the table is newest-first).
    private func mergeDetail(_ d: DetailBundle) {
        detailTotal = d.total
        if detailNeedsReset || detailRows.count <= d.rows.count {
            detailNeedsReset = false
            if d.rows != detailRows { detailRows = d.rows }
            return
        }
        let known = Set(detailRows.lazy.map(\.id))
        let fresh = d.rows.prefix { !known.contains($0.id) }
        if !fresh.isEmpty { detailRows.insert(contentsOf: fresh, at: 0) }
    }

    /// Infinite-scroll page for the details table.
    func loadMoreDetail() {
        guard !detailLoadingMore, UInt64(detailRows.count) < detailTotal else { return }
        detailLoadingMore = true
        let req = PageRequest(
            offset: Int64(detailRows.count), limit: nil,
            apps: appFilter.map { Array($0).sorted() },
            models: modelFilter.map { Array($0).sorted() }
        )
        Task {
            defer { detailLoadingMore = false }
            if let page = try? await Core.detailPage(req) {
                detailRows.append(contentsOf: page.rows)
            }
        }
    }

    func loadSources() {
        Task { if let s = try? await Core.sources() { sources = s } }
    }

    func loadPrices() {
        Task { if let p = try? await Core.prices() { prices = p } }
    }

    func refreshPrices() {
        guard !isRefreshingPrices else { return }
        isRefreshingPrices = true
        priceError = nil
        Task {
            defer { isRefreshingPrices = false }
            do {
                let r = try await Core.refreshPrices()
                if !r.failed.isEmpty { priceError = r.failed.joined(separator: "\n") }
                if r.rulesRepass || r.repriced > 0 {
                    requestLoad(forceViews: true, rebuild: r.rulesRepass)
                }
                if page == .prices || prices != nil { loadPrices() }
            } catch {
                priceError = error.localizedDescription
            }
        }
    }

    func checkForUpdate() {
        Task { update = try? await Core.checkUpdate(channel: updateChannel) }
    }

    // MARK: Background cadence

    private func armRefresh() {
        refreshTask?.cancel()
        refreshTask = nil
        if refreshSeconds == 0 {
            startWatcher()
            return
        }
        watcher?.stop()
        let secs = UInt64(refreshSeconds)
        refreshTask = Task { [weak self] in
            while !Task.isCancelled {
                // 10% tolerance lets the OS coalesce our wakeups with others'.
                try? await Task.sleep(
                    for: .seconds(secs), tolerance: .seconds(max(1, secs / 10)))
                guard !Task.isCancelled else { return }
                self?.requestLoad(scan: true)
            }
        }
    }

    private func startWatcher() {
        if watcher == nil {
            watcher = FileWatcher { [weak self] in
                Task { @MainActor in self?.requestLoad(scan: true) }
            }
        }
        Task {
            let roots = (try? await Core.watchRoots()) ?? []
            watcher?.start(paths: roots)
        }
    }

    private func armQuota() {
        guard ProcessInfo.processInfo.environment["GTT_NO_QUOTA"] == nil else { return }
        quotaTask?.cancel()
        quotaTask = Task { [weak self] in
            while !Task.isCancelled {
                if let r = try? await Core.pollQuotas(), r.inserted > 0 {
                    self?.requestLoad(forceViews: true)
                }
                try? await Task.sleep(
                    for: .seconds(Self.quotaPollSeconds), tolerance: .seconds(120))
            }
        }
    }

    // MARK: Derived

    /// Effective tool selection for checklists (nil filter = everything).
    func isAppSelected(_ app: String) -> Bool { appFilter?.contains(app) ?? true }
    func isModelSelected(_ m: String) -> Bool { modelFilter?.contains(m) ?? true }

    func toggleApp(_ app: String) {
        var set = appFilter ?? Set(vm?.apps ?? [])
        if set.contains(app) { set.remove(app) } else { set.insert(app) }
        appFilter = set.count == vm?.apps.count ? nil : set
    }

    func toggleModel(_ m: String) {
        var set = modelFilter ?? Set(vm?.models ?? [])
        if set.contains(m) { set.remove(m) } else { set.insert(m) }
        modelFilter = set.count == vm?.models.count ? nil : set
    }
}
