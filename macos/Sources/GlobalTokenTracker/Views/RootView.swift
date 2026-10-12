// Main window: NavigationSplitView sidebar + unified toolbar. On macOS 26+
// the sidebar and toolbar render as Liquid Glass automatically; content
// stays on opaque cards (glass is for the navigation layer, not data).
import SwiftUI

struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            List(Page.allCases, selection: $model.page.optional) { page in
                Label(page.title, systemImage: page.symbol)
                    .tag(page)
            }
            .navigationSplitViewColumnWidth(min: 180, ideal: 200, max: 260)
            .safeAreaInset(edge: .bottom) { SidebarStatus() }
        } detail: {
            PageHost(page: model.page)
                .toolbar { MainToolbar() }
        }
    }
}

private extension Binding where Value == Page {
    /// List selection is optional; never let a click deselect into "nothing".
    var optional: Binding<Page?> {
        Binding<Page?>(get: { wrappedValue }, set: { if let v = $0 { wrappedValue = v } })
    }
}

/// Detail column. Each page keeps its own scroll state; table pages load
/// their data lazily on first appearance (never during a scan).
///
/// Page changes slide 24 pt in the sidebar's direction while crossfading —
/// a short offset rather than a full-width push, so a heavy page (Overview's
/// charts) composites cheaply for the ~0.35 s both pages are on screen. The
/// animation keys on `page`, so sidebar clicks, ⌘1–5 and in-page links
/// ("Show All") all transition the same way.
struct PageHost: View {
    let page: Page
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        ZStack {
            content
                .id(page)
                .transition(.slideFade(from: model.pageForward ? .bottom : .top,
                                       distance: 24, reduced: reduceMotion))
        }
        .clipped()
        .animation(reduceMotion ? .easeInOut(duration: 0.15) : Motion.page, value: page)
    }

    @ViewBuilder private var content: some View {
        switch page {
        case .overview: OverviewView()
        case .detail: DetailView()
        case .quota: QuotaView()
        case .sources: SourcesView()
        case .prices: PricesView()
        }
    }
}

struct MainToolbar: ToolbarContent {
    @Environment(AppModel.self) private var model

    var body: some ToolbarContent {
        @Bindable var model = model
        if model.page == .overview || model.page == .detail {
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
                .disabled(model.page == .detail)
                .help("Statistics window")
            }
            ToolbarItem(placement: .navigation) {
                CustomRangeButton()
                    .disabled(model.page == .detail)
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
                    Spacer()
                    trailing
                }
            }
            content
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
