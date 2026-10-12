// Main window: NavigationSplitView sidebar + unified toolbar. On macOS 26+
// the sidebar and toolbar render as Liquid Glass automatically; content
// stays on opaque cards (glass is for the navigation layer, not data).
import AppKit
import SwiftUI

struct RootView: View {
    @Environment(AppModel.self) private var model

    // Doesn't read `model.page` itself: only the sidebar and the deck do, so
    // a page change doesn't re-evaluate the split view or the toolbar.
    var body: some View {
        NavigationSplitView {
            Sidebar()
                .navigationSplitViewColumnWidth(min: 180, ideal: 200, max: 260)
                .safeAreaInset(edge: .bottom) { SidebarStatus() }
        } detail: {
            CurrentPageDeck()
                .toolbar { MainToolbar() }
        }
    }
}

private struct Sidebar: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        List(Page.allCases, selection: ProbeFlags.noSidebar ? .constant(nil) : $model.page.optional) { page in
            Label(page.title, systemImage: page.symbol)
                .tag(page)
        }
    }
}

private struct CurrentPageDeck: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        PageDeck(page: model.page)
    }
}

private extension Binding where Value == Page {
    /// List selection is optional; never let a click deselect into "nothing".
    var optional: Binding<Page?> {
        Binding<Page?>(get: { wrappedValue }, set: { if let v = $0 { wrappedValue = v } })
    }
}

/// Detail column: the pages as faces of a drum.
///
/// Pages are built once and kept mounted (a "deck"), so turning to a page
/// never pays its construction — measured at up to ~100 ms (Overview's
/// charts, the grouped Quota form, AppKit tables) on the first frame of the
/// turn, which is what made it stutter. Hidden faces are parked on the side
/// of the drum their sidebar position implies — below the current page if
/// they come after it, above if before — so every turn starts from the
/// right side without a setup frame. Pages not yet visited are built ahead
/// of time, one per idle moment after launch.
///
/// Only the current page is interactive and visible to accessibility; the
/// leaving page stays drawn just until the turn completes.
struct PageDeck: View {
    let page: Page
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ViewState private var current: Page = .overview
    @ViewState private var leaving: Page?
    @ViewState private var settled: Page = .overview
    /// Increments once per turn: triggers each component's cascade and, as
    /// an animated value, drives the frosted seam (fraction = turn progress).
    @ViewState private var turnID = 0
    @ViewState private var seamProgress: Double = 0
    @ViewState private var forward = true
    /// Stable per-face turn state (see PageTurnState).
    @ViewState private var turnStates: [Page: PageTurnState] =
        Dictionary(uniqueKeysWithValues: Page.allCases.map { ($0, PageTurnState()) })
    @ViewState private var built: Set<Page> = []
    @ViewState private var height: CGFloat = 700
    @ViewState private var pendingTurn: Task<Void, Never>?

    var body: some View {
        let geo = DrumGeometry(height: height)
        ZStack {
            ForEach(Page.allCases) { p in
                if built.contains(p) {
                    face(p)
                        .modifier(DrumRoll(angle: reduceMotion || ProbeFlags.noDrum ? Self.restAngle : angle(of: p, span: geo.span),
                                           radius: geo.radius, span: geo.span))
                        // 0.001, not 0: SwiftUI drops fully transparent content
                        // from the render tree, so a page at 0 had its layers
                        // rebuilt and re-rasterised on every turn into view
                        // (up to ~130 ms). Parked faces sit off to the side of
                        // the drum anyway; this keeps their layers alive.
                        .opacity(p == current || p == leaving ? 1 : 0.001)
                        .environment(turnStates[p])
                        // Interaction + accessibility follow `settled`, the page
                        // the last turn finished on: exposing a page's subtree
                        // to accessibility (charts, forms, tables) on the first
                        // frame of a turn stalled it; nothing is clickable or
                        // readable mid-turn anyway.
                        .allowsHitTesting(p == settled)
                        .accessibilityHidden(p != settled)
                        // Fixed stacking order: changing zIndex per turn made
                        // SwiftUI reorder the hosted AppKit views (tables,
                        // forms) and re-lay them out on the turn's first frame.
                }
            }
        }
        .overlay {
            if !reduceMotion && !ProbeFlags.noSeam {
                SeamGlass(progress: seamProgress, forward: forward, height: height, geometry: geo)
            }
        }
        .clipped()
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
        .navigationTitle(model.chromePage.windowTitle)
        .navigationSubtitle(model.chromePage == .overview ? (model.vm.map { "UTC\($0.tzOffset)" } ?? "") : "")
        .onAppear {
            current = page
            settled = page
            model.chromePage = page
            built.insert(page)
        }
        .onChange(of: page) { _, new in
            // Start the turn one run-loop pass later: the sidebar commits its
            // own selection change (an animated Liquid Glass highlight in an
            // AppKit outline view) first, instead of in the turn's first
            // frame, where it caused most first-frame stalls. ~1 frame of
            // latency, imperceptible after a click.
            pendingTurn?.cancel()
            pendingTurn = Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(1))
                guard !Task.isCancelled, page == new else { return }
                turn(from: current, to: new)
            }
        }
        .task { await prebuild() }
    }

    /// Current face at 0°; others parked one face-span away, below the drum
    /// when they come later in the sidebar, above when earlier.
    private func angle(of p: Page, span: Double) -> Double {
        guard p != current, let i = Page.allCases.firstIndex(of: p),
              let c = Page.allCases.firstIndex(of: current) else { return Self.restAngle }
        return i > c ? -span : span
    }

    /// A resting face sits at 0.001°, not 0°: at exactly 0 SwiftUI drops the
    /// 3D transform layer, and re-inserting it when a turn starts re-hosted
    /// the page (AppKit tables, Swift Charts) on the turn's first frame —
    /// the 100–145 ms stalls. Visually identical to 0.
    static let restAngle = 0.001

    private func turn(from old: Page, to new: Page) {
        guard new != current else { return }
        built.insert(new)
        leaving = old
        forward = (Page.allCases.firstIndex(of: new) ?? 0) > (Page.allCases.firstIndex(of: old) ?? 0)
        turnID += 1
        let target = Double(turnID)
        // Only the two faces in this turn get a new turn (their components
        // cascade); a face that was in the previous turn is reset to idle
        // once. Untouched faces' components aren't invalidated at all.
        for p in Page.allCases {
            let role: PageTurn.Role = p == new ? .incoming : p == old ? .outgoing : .idle
            guard role != .idle || turnStates[p]?.turn.role != .idle else { continue }
            turnStates[p]?.turn = PageTurn(id: turnID, forward: forward, role: role)
        }
        let anim = reduceMotion ? Animation.easeInOut(duration: 0.15) : Motion.drum
        // `.removed`: the chrome update (toolbar/title relayout) must wait
        // until the spring has fully settled, not just logically finished,
        // or its stall lands in the visible tail of the motion.
        // While faces move, AppKit would recompute every moving scroll view's
        // tracking areas and cursor rects each frame (seen in profiles);
        // nothing is clickable mid-turn, so pause that until it settles.
        NSApp.keyWindow?.disableCursorRects()
        withAnimation(anim, completionCriteria: .removed) {
            current = new
            seamProgress = target
        } completion: {
            if leaving == old {
                leaving = nil
                NSApp.keyWindow?.enableCursorRects()
                NSApp.keyWindow?.invalidateCursorRects(for: NSApp.keyWindow!.contentView!)
            }
            if current == new {
                settled = new
                if !ProbeFlags.noChrome { model.chromePage = new }
            }
        }
    }

    /// Build the remaining pages after launch, one per idle step, so even a
    /// first visit turns smoothly. Each mounts hidden (opacity 0).
    private func prebuild() async {
        try? await Task.sleep(for: .seconds(2.5))
        for p in Page.allCases where !built.contains(p) {
            guard !Task.isCancelled else { return }
            built.insert(p)
            try? await Task.sleep(for: .milliseconds(350))
        }
    }

    @ViewBuilder private func face(_ p: Page) -> some View {
        switch p {
        case .overview: OverviewView()
        case .detail: DetailView()
        case .quota: QuotaView()
        case .sources: SourcesView()
        case .prices: PricesView()
        }
    }
}

/// Drum sized from the page height: faces of a regular prism ≈ 60° apart,
/// each face's centre at the apothem so neighbours share an edge.
struct DrumGeometry {
    let span: Double
    let radius: CGFloat

    init(height: CGFloat) {
        let h = max(200, height)
        let spanRad = 1 / 0.95
        span = spanRad * 180 / .pi
        radius = h / CGFloat(2 * tan(spanRad / 2))
    }
}

struct MainToolbar: ToolbarContent {
    @Environment(AppModel.self) private var model

    var body: some ToolbarContent {
        @Bindable var model = model
        if model.chromePage == .overview || model.chromePage == .detail {
            ToolbarItem(placement: .navigation) {
                // Always five segments: adding a "Custom" segment only while a
                // custom window is active resized the picker and shoved the
                // date button (and its popover) sideways on the first pick.
                Picker("Range", selection: Binding(
                    get: { model.range.key },
                    set: { key in
                        if key == "custom" {
                            model.pickCustomRange()
                        } else if let preset = RangeSpec.presets.first(where: { $0.key == key }) {
                            model.range = preset
                        }
                    }
                )) {
                    ForEach(RangeSpec.presets, id: \.key) { r in
                        Text(r.title).tag(r.key)
                    }
                    Text("Custom").tag("custom")
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .disabled(model.chromePage == .detail)
                .help("Statistics window")
            }
            ToolbarItem(placement: .navigation) {
                CustomRangeButton()
                    .disabled(model.chromePage == .detail)
            }
            ToolbarItemGroup(placement: .primaryAction) {
                FilterMenu(
                    title: "Tools", symbol: "hammer",
                    items: model.vm?.apps ?? [], label: Fmt.app,
                    isOn: model.isAppSelected, toggle: model.toggleApp,
                    setAll: model.setAllApps, selected: model.appFilter?.count
                )
                FilterMenu(
                    title: "Models", symbol: "cpu",
                    items: model.vm?.models ?? [], label: { $0 },
                    isOn: model.isModelSelected, toggle: model.toggleModel,
                    setAll: model.setAllModels, selected: model.modelFilter?.count
                )
            }
        }
        if model.chromePage == .overview {
            ToolbarItem(placement: .secondaryAction) {
                Button {
                    model.customizingOverview = true
                } label: {
                    Label("Customize Overview…", systemImage: "slider.horizontal.3")
                }
                .help("Reorder or hide overview cards")
            }
        }
        if model.chromePage == .prices {
            ToolbarItem(placement: .primaryAction) {
                PriceSearchField(text: $model.priceQuery)
            }
            ToolbarItem(placement: .primaryAction) {
                Toggle(isOn: $model.priceDisputedOnly) {
                    Label("Disputed Only", systemImage: "exclamationmark.bubble")
                }
                .help("Only models the price sources disagree on")
            }
            ToolbarItem(placement: .primaryAction) {
                Button {
                    model.refreshPrices()
                } label: {
                    Label("Update Prices", systemImage: "arrow.down.circle")
                }
                .disabled(model.isRefreshingPrices)
                .help("Download the latest prices from every source")
            }
        }
        ToolbarSpacer(.fixed)
        ToolbarItem(placement: .primaryAction) {
            // Split button: click scans now; the menu sets the cadence
            // (Windows: the 刷新 pill in the filter strip).
            Menu {
                Picker("Rescan sources", selection: $model.refreshSeconds) {
                    ForEach(AppModel.refreshOptions, id: \.self) { s in
                        Text(Prefs.refreshLabel(s)).tag(s)
                    }
                }
                .pickerStyle(.inline)
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
                    .symbolEffect(.rotate, isActive: model.isScanning)
            } primaryAction: {
                model.manualRefresh()
            }
            .help("Scan all sources now (⌘R); hold for the refresh cadence")
        }
    }
}

/// Toolbar search field for the Prices page. A plain field rather than
/// `.searchable`, which can't be added per page without rebuilding the page
/// it's attached to (and the pages now stay mounted).
struct PriceSearchField: View {
    @Binding var text: String

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
            TextField("Search models, e.g. opus 4.6 or gpt mini", text: $text)
                .textFieldStyle(.plain)
            if !text.isEmpty {
                Button {
                    text = ""
                } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .help("Clear")
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .frame(width: 260)
        .background(.quaternary.opacity(0.5), in: .capsule)
    }
}

/// Checklist menu for tool / model filters. `selected == nil` = everything
/// on; `0` = a deliberately empty view (Windows: Some(vec![])).
struct FilterMenu: View {
    let title: LocalizedStringKey
    let symbol: String
    let items: [String]
    let label: (String) -> String
    let isOn: (String) -> Bool
    let toggle: (String) -> Void
    let setAll: (Bool) -> Void
    let selected: Int?

    var body: some View {
        let active = selected != nil
        Menu {
            Button("Select All") { setAll(true) }.disabled(!active)
            Button("Select None") { setAll(false) }.disabled(selected == 0)
            Divider()
            ForEach(items, id: \.self) { item in
                Toggle(label(item), isOn: Binding(get: { isOn(item) }, set: { _ in toggle(item) }))
            }
        } label: {
            Label {
                // "Tools 12/15" when narrowed — the WinUI "已选 12/15" summary.
                if let n = selected {
                    Text("\(Text(title)) \(n)/\(items.count)")
                } else {
                    Text(title)
                }
            } icon: {
                Image(systemName: active ? "\(symbol).fill" : symbol)
            }
        }
        .menuIndicator(.hidden)
        .help(title)
        .disabled(items.isEmpty)
    }
}

/// Sidebar footer: last scan time / error, Finder-style secondary text.
struct SidebarStatus: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            if let err = model.lastError {
                Label(err, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .lineLimit(2)
            } else if model.isScanning {
                Label("Scanning…", systemImage: "arrow.triangle.2.circlepath")
            } else if let at = model.lastScanAt {
                Label {
                    Text("Updated \(at, format: .relative(presentation: .named))")
                } icon: {
                    Image(systemName: "checkmark.circle")
                }
                if let ms = model.lastScanMs {
                    Text("Scan took \(ms) ms").padding(.leading, 22)
                }
            }
        }
        .font(.caption)
        .foregroundStyle(.secondary)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }
}

// MARK: - Shared building blocks

/// Content card: opaque, hairline-bordered, continuous corners.
struct Card<Content: View>: View {
    var title: LocalizedStringKey?
    var trailing: AnyView?
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let title {
                HStack {
                    Text(title).font(.headline)
                        .contentTransition(.numericText())
                    Spacer()
                    trailing
                }
                .depthLayer(1)
                content.depthLayer(2)
            } else {
                content
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .topLeading)
        .background(.background, in: .rect(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .strokeBorder(.separator.opacity(0.6), lineWidth: 0.5)
        }
    }
}

struct EmptyState: View {
    let title: LocalizedStringKey
    let symbol: String
    var message: LocalizedStringKey?

    var body: some View {
        ContentUnavailableView {
            Label(title, systemImage: symbol)
        } description: {
            if let message { Text(message) }
        }
    }
}
