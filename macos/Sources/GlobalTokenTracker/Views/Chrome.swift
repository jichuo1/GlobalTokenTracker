// Title-bar chrome. The toolbar holds one item that never changes, sized to
// the detail column, so a page turn doesn't insert or remove NSToolbar items
// (which re-laid out the window and popped the controls in after the turn).
// Every page's controls stay mounted inside it; a turn only flips their
// visibility while ChromeFader crossfades the row in Core Animation, so no
// views are created and no per-frame work runs mid-turn.
//
// Every control that opens something opens a popover panel built like the
// calendar's: 18 pt padding, a secondary caption over a rolling headline,
// glass quick buttons, and the shell's spring vocabulary.
import AppKit
import SwiftUI

extension Page {
    /// Pages whose figures follow the statistics window.
    var usesRange: Bool { self == .overview || self == .detail }
}

/// Shows or hides a mounted chrome control. The change itself is instant;
/// ChromeFader crossfades the whole row around it. Hidden controls take no
/// clicks and are skipped by accessibility.
private struct ChromeFade: ViewModifier {
    let shown: Bool

    func body(content: Content) -> some View {
        content
            .opacity(shown ? 1 : 0)
            .allowsHitTesting(shown)
            .accessibilityHidden(!shown)
    }
}

extension View {
    fileprivate func chromeFade(_ shown: Bool) -> some View {
        modifier(ChromeFade(shown: shown))
    }
}

/// Height shared by every chrome capsule.
private let chromeHeight: CGFloat = 32

/// Glass capsule button for the title bar: hover lift, press dip.
struct ChromeButtonStyle: ButtonStyle {
    var active = false

    func makeBody(configuration: Configuration) -> some View {
        ChromeButtonBody(configuration: configuration, active: active)
    }
}

private struct ChromeButtonBody: View {
    let configuration: ButtonStyleConfiguration
    let active: Bool
    @Environment(\.isEnabled) private var enabled
    @ViewState private var hover = false

    var body: some View {
        configuration.label
            .labelStyle(.titleAndIcon)
            .padding(.horizontal, 12)
            .frame(minWidth: chromeHeight, minHeight: chromeHeight)
            .foregroundStyle(active ? AnyShapeStyle(.tint) : AnyShapeStyle(.primary))
            .glassEffect(active ? .regular.tint(.accentColor.opacity(0.18)).interactive() : .regular.interactive(),
                         in: .capsule)
            .contentShape(.capsule)
            .scaleEffect(configuration.isPressed ? 0.95 : hover && enabled ? 1.03 : 1)
            .opacity(enabled ? 1 : 0.45)
            .animation(Motion.press, value: configuration.isPressed)
            .animation(Motion.hover, value: hover)
            .onHover { hover = $0 }
    }
}

/// The whole title-bar row as one toolbar item, sized to the detail column
/// rather than to its content: a page turn changes what is inside it, never
/// its width, so AppKit never re-lays out the toolbar mid-turn.
struct ChromeBar: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack(spacing: 12) {
            LeadingChrome()
            Spacer(minLength: 0)
            TrailingChrome()
        }
        // Inset from the sidebar edge and the window's rounded corner.
        .padding(.leading, 12)
        .padding(.trailing, 4)
        .frame(width: max(240, model.chromeWidth - 28))
        .background(ChromeHostFinder())
    }
}

/// Crossfades the title-bar row from one page's controls to the next in
/// Core Animation. A SwiftUI animation here ran on the main thread every
/// frame of the turn, competing with the drum and doubling dropped frames;
/// a CATransition snapshots the old and new layer trees once and the
/// window server blends them, so the turn pays for one layout, not one per
/// frame.
@MainActor
enum ChromeFader {
    fileprivate static weak var host: NSView?

    static func crossfade(duration: Double, _ change: () -> Void) {
        var tx = Transaction()
        tx.disablesAnimations = true
        guard let host, let layer = host.layer, duration > 0 else {
            withTransaction(tx, change)
            return
        }
        let fade = CATransition()
        fade.type = .fade
        fade.duration = duration
        fade.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        CATransaction.begin()
        withTransaction(tx, change)
        // Render the new state inside this transaction, so the transition
        // has both trees to blend.
        host.layoutSubtreeIfNeeded()
        host.displayIfNeeded()
        layer.add(fade, forKey: "chromeFade")
        CATransaction.commit()
    }
}

/// Finds the hosting view AppKit gave the toolbar item.
private struct ChromeHostFinder: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let v = NSView()
        DispatchQueue.main.async {
            var host: NSView? = v.superview
            var probe = v.superview
            while let p = probe {
                let name = String(describing: type(of: p))
                if name.contains("ToolbarItemViewer") { break }
                if name.contains("HostingView") { host = p }
                probe = p.superview
            }
            ChromeFader.host = host
        }
        return v
    }

    func updateNSView(_ nsView: NSView, context: Context) {}
}

// MARK: - Leading: range bar + title

struct LeadingChrome: View {
    @Environment(AppModel.self) private var model
    @ViewState private var barWidth: CGFloat = 0

    var body: some View {
        let page = model.toolbarPage
        ZStack(alignment: .leading) {
            RangeBar(dimmed: page == .detail)
                .fixedSize()
                .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { barWidth = $0 }
                .chromeFade(page.usesRange)
            // Every title mounted; the current one shows. The stack sits
            // beside the range bar, or at the leading edge without it.
            ZStack(alignment: .leading) {
                ForEach(Page.allCases) { p in
                    PageTitle(page: p)
                        .fixedSize()
                        .chromeFade(p == page)
                }
            }
            .offset(x: page.usesRange ? barWidth + 12 : 0)
        }
    }
}

/// The range segments and the calendar button. Dimmed (not removed) on
/// Details, which lists every event regardless of the window.
private struct RangeBar: View {
    let dimmed: Bool
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack(spacing: 8) {
            RangeSegments()
            CustomRangeButton()
                .buttonStyle(ChromeButtonStyle())
                .task {
                    guard ProbeFlags.panel == "calendar" else { return }
                    try? await Task.sleep(for: .seconds(5))
                    model.rangePopoverShown = true
                }
        }
        .opacity(dimmed ? 0.45 : 1)
        .saturation(dimmed ? 0 : 1)
        .allowsHitTesting(!dimmed)
        .accessibilityHidden(dimmed)
    }
}

/// Glass segmented control for the statistics window. A single highlight
/// springs between segments: retargeting mid-flight keeps its velocity, so
/// fast clicks bend it toward the latest choice instead of stacking
/// half-finished animations (the AppKit control crossfaded two highlights).
/// Reads only `model.range`, so data reloads never touch it.
private struct RangeSegments: View {
    @Environment(AppModel.self) private var model
    @Namespace private var ns
    @ViewState private var hovered: String?

    private struct Item {
        var key: String
        var title: LocalizedStringKey
    }

    // Always five segments: a "Custom" segment that came and went resized the
    // control and shoved the calendar button sideways.
    private static let items = RangeSpec.presets.map { Item(key: $0.key, title: $0.title) }
        + [Item(key: "custom", title: "Custom")]

    var body: some View {
        let selected = model.range.key
        HStack(spacing: 0) {
            ForEach(Self.items, id: \.key) { item in
                let on = item.key == selected
                Button {
                    pick(item.key)
                } label: {
                    Text(item.title)
                        .foregroundStyle(on ? .primary : .secondary)
                        .padding(.horizontal, 12)
                        .frame(height: chromeHeight - 6)
                        .background {
                            if on {
                                Capsule()
                                    .fill(.primary.opacity(0.2))
                                    .shadow(color: .black.opacity(0.18), radius: 2, y: 1)
                                    .matchedGeometryEffect(id: "pill", in: ns)
                            } else if hovered == item.key {
                                Capsule().fill(.primary.opacity(0.05))
                            }
                        }
                        .contentShape(.capsule)
                }
                .buttonStyle(.plain)
                .onHover { inside in
                    if inside { hovered = item.key } else if hovered == item.key { hovered = nil }
                }
            }
        }
        .padding(3)
        .glassEffect(.regular, in: .capsule)
        .animation(Motion.resolved(Motion.select), value: selected)
        .animation(Motion.hover, value: hovered)
        .help("Statistics window")
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Range")
    }

    private func pick(_ key: String) {
        if key == "custom" {
            model.pickCustomRange()
        } else {
            model.selectPreset(key)
        }
    }
}

/// Page title (and the Overview's UTC offset).
private struct PageTitle: View {
    let page: Page
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(page.windowTitle)
                .font(.headline)
            if page == .overview, let vm = model.vm {
                Text(verbatim: "UTC\(vm.tzOffset)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .lineLimit(1)
        .accessibilityAddTraits(.isHeader)
    }
}

// MARK: - Trailing: page actions + refresh

struct TrailingChrome: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let page = model.toolbarPage
        HStack(spacing: 8) {
            // Each page's actions, all mounted, right-aligned over each other.
            ZStack(alignment: .trailing) {
                HStack(spacing: 8) {
                    CustomizeButton()
                        .chromeFade(page == .overview)
                    FilterButtons(detail: page == .detail)
                        .chromeFade(page.usesRange)
                }
                PriceActions()
                    .chromeFade(page == .prices)
            }
            .fixedSize()
            RefreshControl()
        }
    }
}

private struct FilterButtons: View {
    let detail: Bool
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack(spacing: 8) {
            FilterButton(
                title: "Tools", symbol: "hammer",
                items: model.vm?.apps ?? [], label: Fmt.app,
                isOn: model.isAppSelected, toggle: model.toggleApp,
                setAll: model.setAllApps, selected: model.appFilter?.count
            )
            FilterButton(
                title: "Models", symbol: "cpu",
                items: model.vm?.models ?? [], label: { $0 },
                isOn: model.isModelSelected, toggle: model.toggleModel,
                setAll: model.setAllModels, selected: model.modelFilter?.count
            )
            // The device dimension, once another device is paired.
            if let vm = model.vm, vm.hasPeers {
                FilterButton(
                    title: "Devices", symbol: "macbook.and.iphone",
                    items: vm.devices.map(\.deviceId),
                    label: { id in
                        let d = vm.devices.first { $0.deviceId == id }
                        guard let d, d.local, d.name != "This device" else { return vm.deviceName(id) }
                        return String(localized: "\(d.name) (This Mac)")
                    },
                    isOn: model.isDeviceSelected, toggle: model.toggleDevice,
                    setAll: model.setAllDevices, selected: model.deviceFilter?.count
                )
                .disabled(detail)
            }
        }
    }
}

/// Tools / models / devices filter: a glass button whose count rolls, with
/// a checklist popover.
private struct FilterButton: View {
    let title: LocalizedStringKey
    let symbol: String
    let items: [String]
    let label: (String) -> String
    let isOn: (String) -> Bool
    let toggle: (String) -> Void
    let setAll: (Bool) -> Void
    /// nil = everything on; 0 = a deliberately empty view.
    let selected: Int?
    @ViewState private var shown = false

    var body: some View {
        Button {
            shown.toggle()
        } label: {
            Label {
                // "Tools 12/15" when narrowed — the WinUI "已选 12/15" summary.
                Group {
                    if let n = selected {
                        Text("\(Text(title)) \(n)/\(items.count)")
                    } else {
                        Text(title)
                    }
                }
                .monospacedDigit()
                .contentTransition(Motion.reduced ? .opacity : .numericText())
            } icon: {
                Image(systemName: symbol)
            }
        }
        .buttonStyle(ChromeButtonStyle(active: selected != nil))
        .animation(Motion.resolved(Motion.select), value: selected)
        .help(selected == nil ? Text(title) : Text("\(Text(title)): \(selected ?? 0) of \(items.count) selected"))
        .popover(isPresented: $shown, arrowEdge: .bottom) {
            FilterPanel(title: title, items: items, label: label, isOn: isOn,
                        toggle: toggle, setAll: setAll, selected: selected)
        }
        .task {
            guard ProbeFlags.panel == symbol else { return }
            try? await Task.sleep(for: .seconds(5))
            shown = true
        }
    }
}

private struct CustomizeButton: View {
    @ViewState private var shown = false

    var body: some View {
        Button {
            shown.toggle()
        } label: {
            Label("Customize Overview…", systemImage: "slider.horizontal.3")
                .labelStyle(.iconOnly)
        }
        .buttonStyle(ChromeButtonStyle(active: shown))
        .help("Reorder or hide overview cards")
        .popover(isPresented: $shown, arrowEdge: .bottom) {
            CustomizePanel()
        }
        .task {
            guard ProbeFlags.panel == "customize" else { return }
            try? await Task.sleep(for: .seconds(5))
            shown = true
        }
    }
}

private struct PriceActions: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        HStack(spacing: 8) {
            PriceSearchField(text: $model.priceQuery)
            Button {
                model.priceDisputedOnly.toggle()
            } label: {
                Label("Disputed Only", systemImage: "exclamationmark.bubble")
                    .labelStyle(.iconOnly)
            }
            .buttonStyle(ChromeButtonStyle(active: model.priceDisputedOnly))
            .help("Only models the price sources disagree on")
            Button {
                model.refreshPrices()
            } label: {
                Label("Update Prices", systemImage: "arrow.down.circle")
                    .labelStyle(.iconOnly)
            }
            .buttonStyle(ChromeButtonStyle())
            .disabled(model.isRefreshingPrices)
            .help("Download the latest prices from every source")
        }
    }
}

/// Refresh: the capsule's left half scans now, the right half opens the
/// cadence panel (Windows: the 刷新 pill in the filter strip).
private struct RefreshControl: View {
    @Environment(AppModel.self) private var model
    @ViewState private var shown = false

    var body: some View {
        HStack(spacing: 0) {
            Button {
                model.manualRefresh()
            } label: {
                Image(systemName: "arrow.clockwise")
                    .symbolEffect(.rotate, isActive: model.isScanning)
                    .frame(width: 34, height: chromeHeight)
                    .contentShape(.rect)
            }
            .help("Scan all sources now (⌘R)")
            Divider().frame(height: 16)
            Button {
                shown.toggle()
            } label: {
                Image(systemName: "chevron.down")
                    .font(.caption.weight(.semibold))
                    .rotationEffect(.degrees(shown ? 180 : 0))
                    .frame(width: 28, height: chromeHeight)
                    .contentShape(.rect)
            }
            .help("Refresh cadence")
            .popover(isPresented: $shown, arrowEdge: .bottom) {
                RefreshPanel()
            }
            .task {
                guard ProbeFlags.panel == "refresh" else { return }
                try? await Task.sleep(for: .seconds(5))
                shown = true
            }
        }
        .buttonStyle(.plain)
        .glassEffect(.regular.interactive(), in: .capsule)
        .animation(Motion.resolved(Motion.select), value: shown)
    }
}

// MARK: - Panels

/// Caption over a rolling headline — the calendar panel's "Selected Range"
/// block, shared by every panel.
private struct PanelHeader: View {
    let caption: LocalizedStringKey
    let headline: String
    var detail: String?
    var down = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(caption).scaledFont(.subheadline).foregroundStyle(.secondary)
            Text(headline)
                .scaledFont(.headline)
                .monospacedDigit()
                .rollingText(down: down)
            if let detail {
                Text(detail)
                    .scaledFont(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2, reservesSpace: true)
                    .contentTransition(.opacity)
            }
        }
        .lineLimit(1)
        .animation(Motion.resolved(Motion.select), value: headline)
    }
}

/// One checklist / choice row with a hover wash and a morphing check.
private struct ChoiceRow: View {
    let title: String
    var symbol: String?
    let on: Bool
    var radio = false
    let action: () -> Void
    @ViewState private var hover = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: on ? (radio ? "checkmark.circle.fill" : "checkmark.square.fill")
                                     : (radio ? "circle" : "square"))
                    .foregroundStyle(on ? AnyShapeStyle(.tint) : AnyShapeStyle(.tertiary))
                    .contentTransition(.symbolEffect(.replace))
                if let symbol {
                    Image(systemName: symbol).foregroundStyle(.secondary).frame(width: 18)
                }
                Text(title)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            .scaledFont(.body)
            .padding(.horizontal, 8)
            .frame(height: 28)
            .background(.primary.opacity(hover ? 0.06 : 0), in: .rect(cornerRadius: 7))
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .animation(Motion.resolved(Motion.select), value: on)
        .animation(Motion.hover, value: hover)
        .onHover { hover = $0 }
    }
}

private struct FilterPanel: View {
    let title: LocalizedStringKey
    let items: [String]
    let label: (String) -> String
    let isOn: (String) -> Bool
    let toggle: (String) -> Void
    let setAll: (Bool) -> Void
    let selected: Int?
    @ViewState private var query = ""
    @ViewState private var lastCount: Int?
    @ViewState private var down = false

    var body: some View {
        let shown = query.isEmpty ? items : items.filter { label($0).localizedCaseInsensitiveContains(query) }
        let count = selected ?? items.count
        let headline = selected == nil
            ? String(localized: "All \(items.count)")
            : String(localized: "\(count) of \(items.count)")
        VStack(alignment: .leading, spacing: 14) {
            PanelHeader(caption: title, headline: headline, down: down)
            GlassEffectContainer(spacing: 6) {
                HStack(spacing: 6) {
                    Button("Select All") { setAll(true) }
                        .disabled(selected == nil)
                    Button("Select None") { setAll(false) }
                        .disabled(selected == 0)
                }
                .buttonStyle(.glass)
                .controlSize(.small)
            }
            if items.count > 8 {
                PanelSearchField(text: $query)
            }
            Divider()
            if shown.isEmpty {
                Text("No matches").foregroundStyle(.secondary).frame(maxWidth: .infinity, minHeight: 60)
            } else {
                ScrollView {
                    LazyVStack(spacing: 2) {
                        ForEach(shown, id: \.self) { item in
                            ChoiceRow(title: label(item), on: isOn(item)) { toggle(item) }
                        }
                    }
                }
                .scrollIndicators(.automatic)
                .frame(height: min(320, CGFloat(shown.count) * 30))
            }
        }
        .padding(18)
        .frame(width: 300)
        .onChange(of: count) { old, new in down = new < old }
    }
}

private struct CustomizePanel: View {
    @Environment(AppModel.self) private var model
    @ViewState private var down = false

    var body: some View {
        @Bindable var model = model
        let layout = model.overviewLayout
        let shownCount = layout.visible.count
        VStack(alignment: .leading, spacing: 14) {
            PanelHeader(caption: "Customize Overview",
                        headline: String(localized: "\(shownCount) of \(layout.all.count) cards shown"),
                        detail: String(localized: "Drag to reorder. Uncheck a card to hide it."),
                        down: down)
            List {
                ForEach(layout.all) { w in
                    Toggle(isOn: Binding(
                        get: { !model.overviewLayout.hidden.contains(w) },
                        set: { on in
                            withAnimation(Motion.resolved(Motion.select)) {
                                if on { model.overviewLayout.hidden.remove(w) } else { model.overviewLayout.hidden.insert(w) }
                            }
                        }
                    )) {
                        Label(w.title, systemImage: w.symbol)
                    }
                }
                .onMove { from, to in
                    var o = model.overviewLayout.all
                    o.move(fromOffsets: from, toOffset: to)
                    withAnimation(Motion.resolved(Motion.select)) { model.overviewLayout.order = o }
                }
            }
            .listStyle(.bordered)
            .frame(height: CGFloat(layout.all.count) * 28 + 10)
            GlassEffectContainer(spacing: 6) {
                Button("Restore Defaults") {
                    withAnimation(Motion.resolved(Motion.select)) { model.overviewLayout = OverviewLayout() }
                }
                .buttonStyle(.glass)
                .controlSize(.small)
                .disabled(layout == OverviewLayout())
            }
        }
        .padding(18)
        .frame(width: 320)
        .onChange(of: shownCount) { old, new in down = new < old }
    }
}

private struct RefreshPanel: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Rescan Sources").scaledFont(.subheadline).foregroundStyle(.secondary)
                Group {
                    if let at = model.lastScanAt {
                        Text("Last scan \(at, style: .relative) ago")
                    } else {
                        Text("Not scanned yet")
                    }
                }
                .scaledFont(.headline)
                .monospacedDigit()
                Text(model.isScanning ? "Scanning…" : "New usage is picked up automatically.")
                    .scaledFont(.caption)
                    .foregroundStyle(.secondary)
                    .contentTransition(.opacity)
                    .animation(Motion.hover, value: model.isScanning)
            }
            Divider()
            VStack(spacing: 2) {
                ForEach(AppModel.refreshOptions, id: \.self) { s in
                    ChoiceRow(title: Prefs.refreshText(s), on: model.refreshSeconds == s, radio: true) {
                        model.refreshSeconds = s
                    }
                }
            }
            GlassEffectContainer {
                Button {
                    model.manualRefresh()
                } label: {
                    Label("Scan Now", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.glassProminent)
                .controlSize(.small)
                .disabled(model.isScanning)
                .keyboardShortcut("r", modifiers: .command)
            }
        }
        .padding(18)
        .frame(width: 280)
    }
}

/// Search field used inside panels (and, wider, in the Prices toolbar).
private struct PanelSearchField: View {
    @Binding var text: String

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
            TextField("Filter", text: $text)
                .textFieldStyle(.plain)
            if !text.isEmpty {
                Button {
                    text = ""
                } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .help("Clear")
                .transition(.opacity)
            }
        }
        .padding(.horizontal, 8)
        .frame(height: 26)
        .background(.quaternary.opacity(0.5), in: .capsule)
        .animation(Motion.hover, value: text.isEmpty)
    }
}
