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
    case overview, detail, quota, devices, sources, prices, settings
    var id: String { rawValue }

    var title: LocalizedStringKey {
        switch self {
        case .overview: "Overview"
        case .detail: "Details"
        case .quota: "Quota"
        case .devices: "Devices"
        case .sources: "Sources"
        case .prices: "Prices"
        case .settings: "Settings"
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
        case .devices: "macbook.and.iphone"
        case .sources: "externaldrive.connected.to.line.below"
        case .prices: "dollarsign.circle"
        case .settings: "gearshape"
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

    // MARK: LAN devices
    /// This Mac + paired devices, as the ledger registry holds them.
    private(set) var peers: PeerList?
    /// A sync the user asked for is running (the background knock is silent).
    private(set) var isSyncingPeers = false
    private var peerSyncRunning = false
    // MARK: Sharing this Mac
    /// The LAN share (core::peer::share): server, pairing code, viewers.
    private(set) var share: ShareInfo?
    private(set) var isSwitchingShare = false
    private(set) var shareError: String?
    /// When the pairing code on offer expires, for the countdown.
    private(set) var codeExpiry: Date?
    var isSharing: Bool { share?.sharing ?? false }
    /// Last sync failure that was not per-device (per-device codes live on
    /// each device's `lastErrorCode`).
    private(set) var peerError: String?
    private(set) var nearby: [FoundDevice] = []
    private(set) var isDiscovering = false
    /// Device ids the statistics cover; nil = all devices. Saved by core
    /// (`set_filter`), which `gtt_load` applies.
    private(set) var deviceFilter: Set<String>?
    private(set) var updateState: UpdateState = .idle
    /// Banner dismissed for this session (Windows: DismissUpdateBanner).
    var updateBannerDismissed = false

    /// Overview's "Customize Overview…" sheet (toolbar button lives at the
    /// window level so mounted-but-hidden pages add no toolbar items).

    /// Main window on screen and not fully covered (NSWindow occlusion);
    /// background rescans relax while it's hidden.
    var windowVisible = true {
        didSet {
            // Coming back into view: catch up at once instead of waiting.
            if windowVisible && !oldValue && started { requestLoad(scan: true) }
        }
    }

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
    var page: Page = .overview
    /// The page the window chrome (toolbar items, title) shows. Follows
    /// `page` only once a page turn has finished: reconfiguring NSToolbar and
    /// the title re-lays out the window (measured 50–130 ms), which froze the
    /// drum for its first frames; a stall while nothing moves is invisible.
    var chromePage: Page = .overview
    /// The page the title-bar controls show — follows a turn as it starts
    /// (the toolbar items themselves never change, see Chrome.swift).
    var toolbarPage: Page = .overview
    /// Width of the detail column — the title-bar chrome spans it.
    var chromeWidth: CGFloat = 0
    /// Overview card order and visibility (edited from the toolbar panel).
    var overviewLayout = OverviewLayout.load() {
        didSet { if overviewLayout != oldValue { overviewLayout.save() } }
    }
    var range: RangeSpec {
        didSet {
            guard range != oldValue else { return }
            rangeInfo = try? Core.window(.describe(range))
            animateNextData = true
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
        didSet { animateNextData = true; persist(); requestLoad(forceViews: true, resetDetail: true) }
    }
    var modelFilter: Set<String>? {
        didSet { animateNextData = true; persist(); requestLoad(forceViews: true, resetDetail: true) }
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
    private var peerSyncTask: Task<Void, Never>?
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
        // Inert unless set: open on a page (screenshots, UI checks).
        if let p = ProcessInfo.processInfo.environment["GTT_OPEN_PAGE"].flatMap(Page.init(rawValue:)) {
            page = p
            chromePage = p
        }
    }

    /// Apply a calendar / hour-dial / quick-range choice. core builds the
    /// window (inclusive, DST-safe, local-day/hour aligned); we only store it.
    /// Wheel direction for the rolling range captions: down when the new
    /// window starts earlier than the old one.
    private(set) var rangeRollsDown = false
    private var deferRangeLoad = false
    /// The next fresh data was requested by a range / filter change.
    private var animateNextData = false
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

    /// A preset segment picked. The highlight and captions move at once; the
    /// data load waits for clicks to stop (rapid picks collapse into one).
    func selectPreset(_ key: String) {
        guard let preset = RangeSpec.presets.first(where: { $0.key == key }), preset != range else { return }
        let info = try? Core.window(.describe(preset))
        rangeRollsDown = Self.startKey(info) < Self.startKey(rangeInfo)
        deferRangeLoad = true
        range = preset
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
            await loadPeers()
            await nameThisMac()
            if defaults.bool(forKey: "lanSharing") {
                setSharing(true)
            } else {
                await refreshShare()
            }
            armPeerSync()
            // Developer probe: one full sync at launch, timing logged.
            if ProcessInfo.processInfo.environment["GTT_PROBE_SYNC"] != nil {
                let t0 = Date.now
                let r = try? await Core.peerSync(ifDue: false)
                let devices: [DeviceSync] = r?.report?.devices ?? []
                let lines: [String] = devices.map { d in
                    "[sync] \(d.name) ok=\(d.ok) err=\(d.errorCode ?? "-") changed=\(d.daysChanged)"
                }
                FileHandle.standardError.write(Data((lines.joined(separator: "\n") + "\n[sync] done \(Date.now.timeIntervalSince(t0))s\n").utf8))
            }
            // Developer probe: one scan at launch, results logged.
            if ProcessInfo.processInfo.environment["GTT_PROBE_DISCOVER"] != nil {
                try? await Task.sleep(for: .seconds(4))
                discover()
            }
        }
    }

    func manualRefresh() {
        requestLoad(scan: true, forceViews: true, rebuild: true)
        if page == .sources { loadSources() }
        if !(peers?.peers.isEmpty ?? true) { syncPeers() }
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
            // Data a range / filter change asked for arrives animated: the
            // Overview's figures roll to their new values and the charts
            // morph. Periodic refreshes stay instant (no motion while idle).
            let animate = animateNextData && r.vm != nil
            if animate { animateNextData = false }
            withAnimation(animate ? Motion.resolved(Motion.select) : nil) {
                if let vm = r.vm, vm != self.vm { self.vm = vm }
                if let t = r.trend, t != trend { trend = t }
                if let q = r.quotasRanked, q != quotasRanked { quotasRanked = q }
            }
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

    /// Index of each loaded detail row (for prefetch distance), rebuilt
    /// lazily when the rows change.
    private var detailIndex: [String: Int] = [:]
    private var detailIndexCount = -1

    func isNearDetailEnd(_ id: String, within n: Int) -> Bool {
        if detailIndexCount != detailRows.count || detailIndex.count != detailRows.count {
            detailIndex = Dictionary(detailRows.enumerated().map { ($1.id, $0) }, uniquingKeysWith: { a, _ in a })
            detailIndexCount = detailRows.count
        }
        guard let i = detailIndex[id] else { return false }
        return i >= detailRows.count - n
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
                // Relaxed while nobody can see the window or the Mac is in
                // Low Power Mode: half the rescans, and a wide tolerance so
                // macOS can coalesce our wakeups with other work.
                let relaxed = self.map { !$0.windowVisible } ?? false
                    || ProcessInfo.processInfo.isLowPowerModeEnabled
                let interval = relaxed ? secs * 2 : secs
                try? await Task.sleep(
                    for: .seconds(interval), tolerance: .seconds(max(1, interval / (relaxed ? 4 : 10))))
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

    // MARK: LAN devices

    func loadPeers() async {
        guard let list = try? await Core.peerList() else { return }
        if list != peers { peers = list }
        let filter = list.filter.map(Set.init)
        if filter != deviceFilter { deviceFilter = filter }
    }

    /// Pull every enabled paired device now (`ifDue`: the timer's throttle).
    func syncPeers(ifDue: Bool = false) {
        guard !peerSyncRunning else { return }
        peerSyncRunning = true
        if !ifDue { isSyncingPeers = true }
        Task {
            defer {
                peerSyncRunning = false
                isSyncingPeers = false
            }
            do {
                let r = try await Core.peerSync(ifDue: ifDue)
                peerError = nil
                await loadPeers()
                if r.report?.changed == true { requestLoad(forceViews: true) }
            } catch {
                peerError = Self.peerMessage(error)
            }
        }
    }

    /// Background cadence: core's `sync_if_due` decides (every 5 minutes,
    /// or at once after a device paired with this Mac — so its usage shows
    /// up within seconds); we only knock. The knock doesn't wait for a known
    /// peer: the device that just paired isn't in our list until it syncs.
    private func armPeerSync() {
        peerSyncTask?.cancel()
        peerSyncTask = Task { [weak self] in
            while !Task.isCancelled {
                self?.syncPeers(ifDue: true)
                if self?.isSharing == true { await self?.refreshShare() }
                let hidden = self.map { !$0.windowVisible } ?? false
                try? await Task.sleep(for: .seconds(hidden ? 120 : 15), tolerance: .seconds(hidden ? 30 : 3))
            }
        }
    }

    // MARK: Sharing

    /// Other devices list this Mac by name. core falls back to environment
    /// variables a GUI app on macOS doesn't have, so give it the computer
    /// name (System Settings › General › Sharing) once, when none is set.
    private func nameThisMac() async {
        guard let device = peers?.device, device.name == "This device" || device.name.hasPrefix("GTT-"),
              let name = Host.current().localizedName, !name.isEmpty
        else { return }
        if (try? await Core.setDeviceName(name)) != nil { await loadPeers() }
    }

    /// Turn LAN sharing on or off; remembered across launches.
    func setSharing(_ on: Bool) {
        guard !isSwitchingShare else { return }
        isSwitchingShare = true
        shareError = nil
        defaults.set(on, forKey: "lanSharing")
        Task {
            defer { isSwitchingShare = false }
            do {
                applyShare(try await on ? Core.shareStart() : Core.shareStop())
            } catch {
                shareError = Self.peerMessage(error)
                if on { defaults.set(false, forKey: "lanSharing") }
                await refreshShare()
            }
        }
    }

    func refreshShare() async {
        if let s = try? await Core.shareStatus() { applyShare(s) }
    }

    /// Replace the code on offer (the old one stops working).
    func newPairingCode() {
        Task {
            do {
                applyShare(try await Core.shareNewCode())
                shareError = nil
            } catch {
                shareError = Self.peerMessage(error)
            }
        }
    }

    /// The viewer's next request fails; it must pair again.
    func revokeViewer(_ id: String) {
        Task {
            do {
                applyShare(try await Core.revokeViewer(id))
                shareError = nil
            } catch {
                shareError = Self.peerMessage(error)
            }
        }
    }

    private func applyShare(_ s: ShareInfo) {
        if s != share { share = s }
        let expiry = s.status?.codeExpiresIn.map { Date.now.addingTimeInterval(Double($0)) }
        // Keep the countdown steady: only move it when the code changed or
        // the clock drifted by more than a second.
        if let expiry, let old = codeExpiry, abs(expiry.timeIntervalSince(old)) < 1.5 { return }
        codeExpiry = expiry
    }

    func isDeviceSelected(_ id: String) -> Bool { deviceFilter?.contains(id) ?? true }

    func toggleDevice(_ id: String) {
        var set = deviceFilter ?? Set(vm?.devices.map(\.deviceId) ?? [])
        if set.contains(id) { set.remove(id) } else { set.insert(id) }
        setDeviceFilter(set.count == vm?.devices.count ? nil : set)
    }

    func setAllDevices(_ on: Bool) { setDeviceFilter(on ? nil : []) }

    func setDeviceFilter(_ filter: Set<String>?) {
        guard filter != deviceFilter else { return }
        deviceFilter = filter
        animateNextData = true
        Task {
            _ = try? await Core.setDeviceFilter(filter.map { Array($0).sorted() })
            requestLoad(forceViews: true)
        }
    }

    func setDeviceEnabled(_ id: String, _ on: Bool) {
        Task {
            _ = try? await Core.setDeviceEnabled(id, on)
            await loadPeers()
            requestLoad(forceViews: true, rebuild: true)
        }
    }

    /// Forget a device: its pin, token and every synced row.
    func forgetDevice(_ id: String) {
        Task {
            do {
                _ = try await Core.forgetDevice(id)
                peerError = nil
            } catch {
                peerError = Self.peerMessage(error)
            }
            if deviceFilter?.contains(id) == true {
                var f = deviceFilter
                f?.remove(id)
                setDeviceFilter(f?.isEmpty == true ? nil : f)
            }
            await loadPeers()
            requestLoad(forceViews: true, rebuild: true)
        }
    }

    /// Pair, then pull the new device at once. Returns a user-facing error.
    func pair(address: String, code: String) async -> String? {
        do {
            let r = try await Core.pair(
                address: address, code: code,
                name: peers.map(\.device.name).flatMap { $0 == "This device" ? nil : $0 })
            // Mutual pairing started sharing so the other device can read
            // this Mac; keep it on across launches.
            if r.sharingStarted { defaults.set(true, forKey: "lanSharing") }
        } catch {
            return Self.peerMessage(error)
        }
        await refreshShare()
        await loadPeers()
        nearby.removeAll { n in peers?.peers.contains { $0.deviceId == n.deviceId } ?? false }
        syncPeers()
        return nil
    }

    func discover() {
        guard !isDiscovering else { return }
        if ProcessInfo.processInfo.environment["GTT_PROBE_DISCOVER"] != nil {
            Task {
                let t0 = Date.now
                let r = try? await Core.discover()
                let lines = (r?.devices ?? []).map { "[discover] \($0.name) \($0.addresses) paired=\($0.paired)" }
                FileHandle.standardError.write(Data((lines.joined(separator: "\n") + "\n[discover] done \(Date.now.timeIntervalSince(t0))s\n").utf8))
            }
        }
        isDiscovering = true
        Task {
            defer { isDiscovering = false }
            do {
                let r = try await Core.discover()
                let mine = peers?.device.deviceId
                let found = r.devices.filter { $0.deviceId != mine }
                if found != nearby { nearby = found }
                peerError = nil
            } catch {
                peerError = Self.peerMessage(error)
            }
        }
    }

    /// core's fixed codes → words (docs/PEER_API.md).
    static func peerMessage(_ error: Error) -> String {
        guard case CoreError.engine(let code) = error else { return error.localizedDescription }
        return peerMessage(code: code)
    }

    static func peerMessage(code: String) -> String {
        switch code {
        case "invalid_code": String(localized: "The pairing code is six digits.")
        case "pairing_failed": String(localized: "The code didn't match. Show a new code on the other device and try again.")
        case "pairing_code_reused": String(localized: "This code already failed once. Show a new code on the other device.")
        case "pairing_code_expired": String(localized: "The code expired. Show a new code on the other device.")
        case "pairing_unavailable": String(localized: "The other device has no active pairing code.")
        case "pairing_locked": String(localized: "Too many attempts. Wait a few seconds, then try again.")
        case "pairing_busy": String(localized: "The other device is pairing with someone else. Try again shortly.")
        case "pairing_self": String(localized: "That's this Mac.")
        case "pairing_aborted", "pairing_refused": String(localized: "Pairing was interrupted. Try again.")
        case "tls_required": String(localized: "That device isn't sharing over a secure connection.")
        case "connect_failed": String(localized: "Couldn't reach the device. Check the address and that it is sharing.")
        case "tls_failed": String(localized: "Couldn't set up a secure connection to the device.")
        case "peer_fingerprint_mismatch": String(localized: "A different device answered at this address. Its identity doesn't match the paired one.")
        case "auth_invalid", "auth_required": String(localized: "The device no longer accepts this Mac. Pair again.")
        case "not_paired": String(localized: "Not paired.")
        case "plaintext_not_allowed": String(localized: "Only secure connections are allowed on the network.")
        case "sync_timeout": String(localized: "The device took too long to answer.")
        case "unknown_peer": String(localized: "That device isn't paired.")
        case "unknown_viewer": String(localized: "That device no longer has access.")
        case "ambiguous_peer": String(localized: "More than one device matches.")
        case "external_interface_disabled": String(localized: "The external interface is turned off on that device.")
        case "ledger_busy", "ledger_unavailable": String(localized: "The device's ledger is busy. Try again shortly.")
        case "unsupported_protocol_version": String(localized: "That device runs an incompatible version.")
        default: code
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
