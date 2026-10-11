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
struct PageHost: View {
    let page: Page

    var body: some View {
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
                Picker("Range", selection: $model.range) {
                    ForEach(RangeSpec.presets, id: \.self) { r in
                        Text(r.title).tag(r)
                    }
                    if model.range.key == "custom" {
                        Text("Custom").tag(model.range)
                    }
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
                    reset: { model.appFilter = nil }, active: model.appFilter != nil
                )
                FilterMenu(
                    title: "Models", symbol: "cpu",
                    items: model.vm?.models ?? [], label: { $0 },
                    isOn: model.isModelSelected, toggle: model.toggleModel,
                    reset: { model.modelFilter = nil }, active: model.modelFilter != nil
                )
            }
        }
        ToolbarSpacer(.fixed)
        ToolbarItem(placement: .primaryAction) {
            Button {
                model.manualRefresh()
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
                    .symbolEffect(.rotate, isActive: model.isScanning)
            }
            .help("Scan all sources now (⌘R)")
        }
    }
}

/// Checklist menu for tool / model filters. `nil` filter = everything on.
struct FilterMenu: View {
    let title: LocalizedStringKey
    let symbol: String
    let items: [String]
    let label: (String) -> String
    let isOn: (String) -> Bool
    let toggle: (String) -> Void
    let reset: () -> Void
    let active: Bool

    var body: some View {
        Menu {
            Button("Show All", action: reset).disabled(!active)
            Divider()
            ForEach(items, id: \.self) { item in
                Toggle(label(item), isOn: Binding(get: { isOn(item) }, set: { _ in toggle(item) }))
            }
        } label: {
            Label(title, systemImage: active ? "\(symbol).fill" : symbol)
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
