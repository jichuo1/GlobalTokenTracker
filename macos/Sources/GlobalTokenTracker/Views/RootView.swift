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
                .toolbar(removing: .title)
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
            // Delivered after the layout pass that measured it: a toolbar
            // item resized during AppKit's own layout is never re-measured
            // (it stayed too wide and moved to the » overflow menu).
            .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { w in
                DispatchQueue.main.async {
                    model.chromeWidth = w
                    ChromeFader.remeasure()
                }
            }
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
    @ViewState private var toolbarTurn: Task<Void, Never>?
    @ViewState private var chromeTask: Task<Void, Never>?
    /// The page VoiceOver sees. Switched with the deferred bookkeeping:
    /// swapping two large pages' accessibility trees cost frames mid-settle.
    @ViewState private var accessible: Page = .overview

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
                        .accessibilityHidden(p != accessible)
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
            accessible = page
            position = Double(Self.index(page))
            model.chromePage = page
            model.toolbarPage = page
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
    static let restAngle = ProbeFlags.flatRest ? 0 : 0.001

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
        // The title-bar controls crossfade a little into the turn, in Core
        // Animation (see ChromeFader): no per-frame work competes with the
        // drum, and the one re-layout it costs lands after the drum's most
        // expensive first frames.
        toolbarTurn?.cancel()
        if !ProbeFlags.noToolbarMotion {
            let lead = reduceMotion ? 0 : min(0.12, spring.travel * 0.25)
            toolbarTurn = Task { @MainActor in
                if lead > 0 { try? await Task.sleep(for: .seconds(lead)) }
                guard !Task.isCancelled, target == new else { return }
                ChromeFader.crossfade(duration: reduceMotion ? 0.15 : 0.3) {
                    model.toolbarPage = new
                }
            }
        }
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
        let started = Date.now
        // Interaction follows the drum's *logical* completion — the moment
        // it has visibly arrived. Waiting for the spring to be removed (its
        // imperceptible tail settling) kept the page unclickable for ~0.9 s
        // after a turn and ~1.7 s after a jump, and AppKit drops clicks
        // made meanwhile.
        withAnimation(anim, completionCriteria: ProbeFlags.lateSettle ? .removed : .logicallyComplete) {
            position = Double(Self.index(new))
        } completion: {
            if MotionProbe.enabled {
                FileHandle.standardError.write(Data("[turn] \(from.rawValue) → \(new.rawValue) interactive after \(Int(Date.now.timeIntervalSince(started) * 1000)) ms\n".utf8))
            }
            // A later click may have re-targeted the drum; only the final
            // destination becomes interactive.
            guard target == new else { return }
            settled = new
            settleChrome(for: new)
        }
    }

    /// Window bookkeeping nobody sees — the window title (it re-lays out
    /// the toolbar, 50–130 ms), scroller styling and cursor rects — once the
    /// spring's tail has died out too, so it never costs a moving frame.
    private func settleChrome(for page: Page) {
        chromeTask?.cancel()
        chromeTask = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(reduceMotion || ProbeFlags.lateSettle ? 0 : 450))
            guard !Task.isCancelled, target == page else { return }
            accessible = page
            if !ProbeFlags.noChrome { model.chromePage = page }
            if ProbeFlags.noToolbarMotion { model.toolbarPage = page }
            ScrollerStyle.apply()
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
        ScrollerStyle.apply()
        for p in Page.allCases where !built.contains(p) {
            guard !Task.isCancelled else { return }
            built.insert(p)
            try? await Task.sleep(for: .milliseconds(350))
            ScrollerStyle.apply()
        }
    }

    @ViewBuilder private func face(_ p: Page) -> some View {
        switch p {
        case .overview: OverviewView()
        case .detail: DetailView()
        case .quota: QuotaView()
        case .devices: DevicesView()
        case .sources: SourcesView()
        case .prices: PricesView()
        case .settings: SettingsView()
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

/// One item that page turns never change (see Chrome.swift): its contents
/// follow `model.toolbarPage`; its width follows the window.
struct MainToolbar: ToolbarContent {
    @Environment(AppModel.self) private var model

    var body: some ToolbarContent {
        // Reading the width here makes SwiftUI update the toolbar item when
        // the window resizes; AppKit only re-measures an item then, and keeps
        // one that grew too wide in the » overflow menu otherwise.
        let width = model.chromeWidth.rounded()
        ToolbarItem(placement: .navigation) {
            ChromeBar(width: width)
        }
        .sharedBackgroundVisibility(.hidden)
    }
}

/// Toolbar search field for the Prices page. A plain field rather than
/// `.searchable`, which can't be added per page without rebuilding the page
/// it's attached to (and the pages now stay mounted).
struct PriceSearchField: View {
    @Binding var text: String
    @Environment(\.chromeDensity) private var density

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
        .padding(.horizontal, 12)
        .frame(width: density <= .labeled ? 260 : density == .compact ? 200 : 150, height: 32)
        .glassEffect(.regular.interactive(), in: .capsule)
    }
}

/// Sidebar footer: last scan time / error, Finder-style secondary text.
/// Scan status under the sidebar. Both lines always exist and only their
/// text changes, so a new scan, the ticking "updated … ago" and the scan
/// time roll like the rest of the figures instead of views swapping.
struct SidebarStatus: View {
    @Environment(AppModel.self) private var model

    private static let relative: RelativeDateTimeFormatter = {
        let f = RelativeDateTimeFormatter()
        f.dateTimeStyle = .named
        f.unitsStyle = .full
        return f
    }()

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { ctx in
            let s = status(now: ctx.date)
            VStack(alignment: .leading, spacing: 2) {
                Label {
                    Text(verbatim: s.line1)
                        .lineLimit(2)
                        .contentTransition(Motion.reduced ? .opacity : .numericText())
                } icon: {
                    Image(systemName: s.symbol)
                        .contentTransition(.symbolEffect(.replace))
                        .symbolEffect(.rotate, isActive: model.isScanning)
                }
                .foregroundStyle(s.warning ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
                Text(verbatim: s.line2.isEmpty ? " " : s.line2)
                    .padding(.leading, 22)
                    .contentTransition(Motion.reduced ? .opacity : .numericText())
                    .opacity(s.line2.isEmpty ? 0 : 1)
            }
            .animation(Motion.resolved(Motion.select), value: s)
        }
        .scaledFont(.caption)
        .foregroundStyle(.secondary)
        .monospacedDigit()
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private struct Status: Equatable {
        var symbol: String
        var line1: String
        var line2: String
        var warning = false
    }

    private func status(now: Date) -> Status {
        // The last scan time stays while a new scan runs: no line pops.
        let took = model.lastScanMs.map { String(localized: "Scan took \($0) ms") } ?? ""
        if let err = model.lastError {
            return Status(symbol: "exclamationmark.triangle.fill", line1: err, line2: took, warning: true)
        }
        if model.isScanning {
            return Status(symbol: "arrow.triangle.2.circlepath", line1: String(localized: "Scanning…"), line2: took)
        }
        guard let at = model.lastScanAt else {
            return Status(symbol: "circle.dashed", line1: String(localized: "Scanning…"), line2: "")
        }
        // Whole seconds, never in the future (clock skew would read "in 1 s").
        let ago = min(0, at.timeIntervalSince(now).rounded(.down))
        let when = Self.relative.localizedString(fromTimeInterval: ago)
        return Status(symbol: "checkmark.circle", line1: String(localized: "Updated \(when)"), line2: took)
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
                    Text(title).scaledFont(.headline)
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
