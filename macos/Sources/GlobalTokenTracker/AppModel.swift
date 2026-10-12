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
import AppKit
import Foundation
import Observation
import SwiftUI

/// Update flow states — the WinUI shell's `UpdateState`.
enum UpdateState: Equatable {
    case idle
    case checking
    case upToDate
    case available(UpdateInfo)
    case downloading(UpdateInfo)
    /// Verified image opened in Finder; the user drags the app over.
    case downloaded(UpdateInfo)
    case failed(String)
}

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

    /// Window title while the page is showing.
    var windowTitle: LocalizedStringKey {
        self == .prices ? "Prices ($/1M tokens)" : title
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
    private(set) var trend: TrendSeries?
    /// Quotas most-pressing first (core's rank_quotas) — the overview strip.
    private(set) var quotasRanked: [QuotaRow] = []
    private(set) var externalInterface: ExternalInterfaceState?
    private(set) var updateState: UpdateState = .idle
    /// Banner dismissed for this session (Windows: DismissUpdateBanner).
    var updateBannerDismissed = false

    /// Overview's "Customize Overview…" sheet (toolbar button lives at the
    /// window level so mounted-but-hidden pages add no toolbar items).
    var customizingOverview = false

    /// Prices page search — filtered by core (`filter_prices`).
    var priceQuery = "" { didSet { loadPrices() } }
    var priceDisputedOnly = false { didSet { loadPrices() } }

    // MARK: Status
    private(set) var isLoading = false
    private(set) var isScanning = false
    private(set) var isRefreshingPrices = false
    private(set) var lastScanMs: UInt64?
    private(set) var lastScanAt: Date?
    private(set) var lastError: String?
    private(set) var priceError: String?

    // MARK: Selection (persisted)
    var page: Page = .overview {
        didSet {
            if let a = Page.allCases.firstIndex(of: oldValue), let b = Page.allCases.firstIndex(of: page) {
                pageForward = b > a
            }
        }
    }
    /// Direction of the last page change (down the sidebar = forward), for
    /// the page transition.
    private(set) var pageForward = true
    /// The page the window chrome (toolbar items, title) shows. Follows
    /// `page` only once a page turn has finished: reconfiguring NSToolbar and
    /// the title re-lays out the window (measured 50–130 ms), which froze the
    /// drum for its first frames; a stall while nothing moves is invisible.
    var chromePage: Page = .overview
    var range: RangeSpec {
        didSet {
            guard range != oldValue else { return }
            rangeInfo = try? Core.window(.describe(range))
            persist()
            if deferRangeLoad {
                // Calendar / dial picks: let the selection animation settle
                // (~0.3 s spring) before the Overview rebuild lands on the main
                // thread; rapid picks collapse into one load.
                deferRangeLoad = false
                rangeLoadTask?.cancel()
                rangeLoadTask = Task { [weak self] in
                    try? await Task.sleep(for: .milliseconds(240))
                    guard !Task.isCancelled else { return }
                    self?.requestLoad(forceViews: true)
                }
            } else {
                requestLoad(forceViews: true)
            }
        }
    }
    /// core's read-back of `range` (day span, hour span, "now") for the
    /// calendar and hour dial.
    private(set) var rangeInfo: RangeInfo?
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
        didSet { persist(); updateState = .idle }
    }
    /// Check at launch and every 24 h (Windows: update_auto, default on).
    var updateAuto: Bool {
        didSet { persist(); armUpdates() }
    }

    // MARK: Internals
    private let defaults = UserDefaults.standard
    private var started = false
    private var pending: LoadRequest?
    private var refreshTask: Task<Void, Never>?
    private var quotaTask: Task<Void, Never>?
    private var updateTask: Task<Void, Never>?
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
        updateAuto = d.object(forKey: "updateAuto") as? Bool ?? true
        rangeInfo = try? Core.window(.describe(range))
    }

    /// Apply a calendar / hour-dial / quick-range choice. core builds the
    /// window (inclusive, DST-safe, local-day/hour aligned); we only store it.
    /// Wheel direction for the rolling range captions: down when the new
    /// window starts earlier than the old one.
    private(set) var rangeRollsDown = false
    private var deferRangeLoad = false
    private var rangeLoadTask: Task<Void, Never>?

    func applyWindow(_ op: Core.WindowOp) {
        guard let info = try? Core.window(op), info.range != range else { return }
        rangeRollsDown = Self.startKey(info) < Self.startKey(rangeInfo)
        deferRangeLoad = true
        withAnimation(Motion.resolved(Motion.select)) {
            range = info.range
        }
    }

    /// Comparable start of a window: "YYYY-MM-DD" + two-digit first hour.
    private static func startKey(_ info: RangeInfo?) -> String {
        guard let info, let day = info.firstDay else { return "" }
        return day + String(format: "%02d", info.hourSpan?.first ?? 0)
    }

    /// Re-read core's "today" / "now" (the panel may stay open past an hour).
    func refreshRangeInfo() {
        if let info = try? Core.window(.describe(range)), info != rangeInfo { rangeInfo = info }
    }

    /// The calendar popover, owned here so the toolbar's "Custom" segment can
    /// open it as well as the date button.
    var rangePopoverShown = false

    /// "Custom" segment picked — adopt the last custom window, else the last
    /// 7 days (Windows: Msg::PickCustomRange), and show the calendar.
    func pickCustomRange() {
        if range.key != "custom" {
            if let s = defaults.object(forKey: "lastCustomStart") as? Int64,
               let e = defaults.object(forKey: "lastCustomEnd") as? Int64 {
                range = RangeSpec(key: "custom", startMs: s, endMs: e)
            } else if let today = CivilDay(rangeInfo?.today ?? (try? Core.window(.describe(.week)))?.today),
                      let first = CivilDay.gregorian.date(byAdding: .day, value: -6, to: today.date) {
                let c = CivilDay.gregorian.dateComponents([.year, .month, .day], from: first)
                let from = CivilDay(year: c.year ?? today.year, month: c.month ?? today.month, day: c.day ?? today.day)
                applyWindow(.days(first: from.key, last: today.key))
            }
        }
        rangePopoverShown = true
    }

    private func persist() {
        defaults.set(range.key, forKey: "range")
        defaults.set(range.startMs, forKey: "rangeStart")
        defaults.set(range.endMs, forKey: "rangeEnd")
        if range.key == "custom" {
            defaults.set(range.startMs, forKey: "lastCustomStart")
            defaults.set(range.endMs, forKey: "lastCustomEnd")
        }
        defaults.set(appFilter.map { Array($0).sorted() }, forKey: "appFilter")
        defaults.set(modelFilter.map { Array($0).sorted() }, forKey: "modelFilter")
        defaults.set(refreshSeconds, forKey: "refreshSeconds")
        defaults.set(updateChannel, forKey: "updateChannel")
        defaults.set(updateAuto, forKey: "updateAuto")
    }

    // MARK: Lifecycle

    /// First paint straight from the ledger (no scan), then scan — same as
    /// the WinUI startup path, so the window never waits on the filesystem.
    func start() {
        guard !started else { return }
        started = true
        Task {
            info = try? await Core.info()
            externalInterface = try? await Core.externalInterface(set: nil)
            await runLoad(base(scan: false, forceViews: true, forcePrices: true))
            await runLoad(base(scan: true))
            armRefresh()
            armQuota()
            armUpdates()
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
            if let t = r.trend, t != trend { trend = t }
            if let q = r.quotasRanked, q != quotasRanked { quotasRanked = q }
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

    /// Latest-wins: a slower earlier search must not overwrite a newer one.
    private var priceGen = 0

    func loadPrices(reload: Bool = false) {
        priceGen += 1
        let gen = priceGen
        let req = Core.PricesRequest(query: priceQuery, disputedOnly: priceDisputedOnly, reload: reload)
        Task {
            if let p = try? await Core.prices(req), gen == priceGen, p != prices { prices = p }
        }
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
                if page == .prices || prices != nil { loadPrices(reload: true) }
            } catch {
                priceError = error.localizedDescription
            }
        }
    }

    func checkForUpdate() {
        if case .downloading = updateState { return }
        updateState = .checking
        let channel = updateChannel
        Task {
            do {
                let r = try await Core.checkUpdate(channel: channel)
                guard channel == updateChannel else { return }
                updateState = r.update.map { .available($0) } ?? .upToDate
                if r.update != nil { updateBannerDismissed = false }
            } catch {
                updateState = .failed(error.localizedDescription)
            }
        }
    }

    /// "Update Now": core downloads and SHA-256-verifies the disk image; we
    /// only open it, so the user gets Finder's drag-to-Applications window.
    func downloadUpdate() {
        guard case .available(let u) = updateState else { return }
        updateState = .downloading(u)
        let channel = updateChannel
        Task {
            do {
                let d = try await Core.downloadUpdate(channel: channel)
                NSWorkspace.shared.open(URL(fileURLWithPath: d.path))
                updateState = .downloaded(u)
            } catch {
                updateState = .failed(error.localizedDescription)
            }
        }
    }

    private func armUpdates() {
        updateTask?.cancel()
        updateTask = nil
        guard updateAuto, started else { return }
        updateTask = Task { [weak self] in
            while !Task.isCancelled {
                self?.checkForUpdate()
                try? await Task.sleep(for: .seconds(24 * 3600), tolerance: .seconds(600))
            }
        }
    }

    func setExternalInterface(_ on: Bool) {
        Task { externalInterface = try? await Core.externalInterface(set: on) }
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

    /// Bulk scope: `true` = everything (nil filter), `false` = deliberately
    /// empty view — same semantics as the WinUI filter flyout.
    func setAllApps(_ on: Bool) { appFilter = on ? nil : [] }
    func setAllModels(_ on: Bool) { modelFilter = on ? nil : [] }

    func toggleModel(_ m: String) {
        var set = modelFilter ?? Set(vm?.models ?? [])
        if set.contains(m) { set.remove(m) } else { set.insert(m) }
        modelFilter = set.count == vm?.models.count ? nil : set
    }
}
