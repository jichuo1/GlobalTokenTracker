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
/// The drum has one continuous **position** (0 = Overview … 4 = Prices, the
/// sidebar order); every face's angle is (position − its index) × the face
/// span. A turn is just `position` springing to the new index, so:
/// - a jump of several pages rotates *through* the pages in between, each
///   fading in and out by its own angle as it passes the front;
/// - the spring's duration grows sub-linearly with distance (a 4-page jump
///   spins up, cruises and settles in ~1.5× a single step, not 4×);
/// - clicking another page mid-turn re-targets the same spring, which keeps
///   its current velocity and bends toward the new page — interruptible,
///   no restart.
///
/// Pages are built once and kept mounted, so turning never pays a page's
/// construction; parked faces sit at ~0 opacity (not 0, which would make
/// SwiftUI drop their layers) and the resting face 0.001° off zero (so its
/// 3D transform layer is never re-inserted). Chrome (toolbar, title), hit
/// testing and accessibility switch once the drum has settled.
struct PageDeck: View {
    let page: Page
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// Animated drum position (page index, fractional mid-turn).
    @ViewState private var position: Double = 0
    /// The page the drum is heading to / resting on.
    @ViewState private var target: Page = .overview
    /// The page the last turn finished on: interactive + accessible.
    @ViewState private var settled: Page = .overview
    @ViewState private var turnID = 0
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
                        // Opacity, shading and depth all come from the face's
                        // angle, so passing faces fade in and out continuously.
                        .modifier(DrumRoll(angle: angle(of: p, span: geo.span),
                                           radius: geo.radius, span: geo.span))
                        .environment(turnStates[p])
                        .allowsHitTesting(p == settled)
                        .accessibilityHidden(p != settled)
                }
            }
        }
        .overlay {
            if !reduceMotion && !ProbeFlags.noSeam {
                // The seam between faces k and k+1 sits at fraction
                // position − k; it pulses once per page boundary passed.
                SeamGlass(progress: position, forward: true, height: height, geometry: geo)
            }
        }
        .clipped()
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
        .navigationTitle(model.chromePage.windowTitle)
        .navigationSubtitle(model.chromePage == .overview ? (model.vm.map { "UTC\($0.tzOffset)" } ?? "") : "")
        .onAppear {
            target = page
            settled = page
            position = Double(Self.index(page))
            model.chromePage = page
            built.insert(page)
        }
        .onChange(of: page) { _, new in
            // Start one run-loop pass later: the sidebar commits its own
            // selection change (an animated highlight in an AppKit outline
            // view) first, instead of in the turn's first frame.
            pendingTurn?.cancel()
            pendingTurn = Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(1))
                guard !Task.isCancelled, page == new else { return }
                turn(to: new)
            }
        }
        .task { await prebuild() }
    }

    static func index(_ p: Page) -> Int { Page.allCases.firstIndex(of: p) ?? 0 }

    /// Faces later in the sidebar sit below (negative), earlier ones above;
    /// the resting face 0.001° off zero (see `restAngle`).
    private func angle(of p: Page, span: Double) -> Double {
        if reduceMotion || ProbeFlags.noDrum {
            return p == target ? Self.restAngle : (Self.index(p) > Self.index(target) ? -span : span)
        }
        return (position - Double(Self.index(p))) * span + Self.restAngle
    }

    /// At exactly 0° SwiftUI drops the 3D transform layer; re-inserting it
    /// when a turn starts re-hosted the page (tables, charts) — 100–145 ms
    /// stalls. Visually identical to 0.
    static let restAngle = 0.001

    private func turn(to new: Page) {
        guard new != target else { return }
        let from = target
        let distance = abs(Self.index(new) - Self.index(from))
        // Every face the drum will pass must exist.
        let lo = min(Self.index(new), Self.index(from)), hi = max(Self.index(new), Self.index(from))
        for i in lo...hi { built.insert(Page.allCases[i]) }
        target = new
        turnID += 1
        let forward = Self.index(new) > Self.index(from)
        let spring = Self.spring(distance: distance)
        // The destination's cascade starts as it comes into view, not when a
        // long jump starts: roughly the time to cover all but the last face.
        let arrival = distance > 1 ? spring.travel * Double(distance - 1) / Double(distance) : 0
        for p in Page.allCases {
            let role: PageTurn.Role = p == new ? .incoming : p == from ? .outgoing : .idle
            guard role != .idle || turnStates[p]?.turn.role != .idle else { continue }
            turnStates[p]?.turn = PageTurn(id: turnID, forward: forward, role: role,
                                           delay: role == .incoming ? arrival : 0)
        }
        // AppKit would recompute moving scroll views' tracking areas and
        // cursor rects every frame; nothing is clickable mid-turn.
        NSApp.keyWindow?.disableCursorRects()
        let anim = reduceMotion ? Animation.easeInOut(duration: 0.15) : spring.animation
        withAnimation(anim, completionCriteria: .removed) {
            position = Double(Self.index(new))
        } completion: {
            // A later click may have re-targeted the drum; only the final
            // destination switches chrome, interaction and accessibility.
            guard target == new else { return }
            settled = new
            if !ProbeFlags.noChrome { model.chromePage = new }
            if let w = NSApp.keyWindow, let v = w.contentView {
                w.enableCursorRects()
                w.invalidateCursorRects(for: v)
            }
        }
    }

    /// Spring for a turn of `distance` faces. Response grows with √distance
    /// (sub-linear): far jumps take longer but not proportionally; damping
    /// just under critical gives the reel a faint settle without bouncing.
    /// Re-targeting mid-flight keeps the spring's velocity (SwiftUI merges
    /// springs on the same value), which is what makes turns interruptible.
    static func spring(distance: Int) -> (animation: Animation, travel: Double) {
        let d = Double(max(1, distance))
        let response = 0.5 * (0.55 + 0.45 * d.squareRoot())
        let damping = d > 1 ? 0.88 : 0.94
        // Time to get most of the way there (~75% of the settling time).
        return (.spring(response: response, dampingFraction: damping), response * 0.9)
    }

    /// Build the remaining pages after launch, one per idle step, so even a
    /// first visit turns smoothly. Each mounts parked (≈0 opacity).
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
