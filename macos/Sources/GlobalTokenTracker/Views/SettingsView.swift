// Settings (⌘,) — the WinUI settings page as standard macOS tabbed
// preferences: General, Appearance, Data (incl. the external coordination
// interface), Updates. Interface text size is left to macOS (System Settings
// › Accessibility › Display › Text Size), the platform's own per-app control.
import AppKit
import SwiftUI

/// The Settings page (sidebar + ⌘,) — the WinUI shell's settings page: one
/// grouped form with General, Appearance, Data and Updates.
struct SettingsView: View {
    var body: some View {
        Form {
            GeneralSettings()
            AppearanceSettings()
            DataSettings()
            UpdateSettings()
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .background(.windowBackground)
    }
}

struct GeneralSettings: View {
    @Environment(AppModel.self) private var model
    @AppStorage(Prefs.showInDockKey) private var showInDock = true
    @AppStorage("menuBarShowsCost") private var menuBarShowsCost = true
    @AppStorage(Prefs.closeActionKey) private var closeAction = ""
    @AppStorage(Prefs.languageKey) private var language = ""
    @ViewState private var launchAtLogin = Prefs.launchAtLogin
    @ViewState private var loginError: String?
    @ViewState private var languageChanged = false
    @ViewState private var displayCurrency = "USD"
    @ViewState private var fx: FxStatus?
    @ViewState private var fxRefreshing = false
    @ViewState private var fxError: String?

    private struct CurrencyOption: Identifiable {
        let code: String
        let symbol: String
        var id: String { code }
    }

    private static let currencyOptions: [CurrencyOption] = [
        .init(code: "USD", symbol: "$"), .init(code: "CNY", symbol: "¥"),
        .init(code: "EUR", symbol: "€"), .init(code: "JPY", symbol: "¥"),
        .init(code: "GBP", symbol: "£"), .init(code: "KRW", symbol: "₩"),
        .init(code: "HKD", symbol: "HK$"), .init(code: "TWD", symbol: "NT$"),
        .init(code: "CAD", symbol: "CA$"), .init(code: "AUD", symbol: "A$"),
        .init(code: "SGD", symbol: "S$"), .init(code: "INR", symbol: "₹"),
    ]

    private var fxNote: String {
        guard let fx, let src = fx.source else {
            return String(localized: "No rate fetched yet; showing USD.")
        }
        let rate = fx.usdTo.formatted(.number.precision(.fractionLength(0...4)))
        let stale = fx.stale ? " · " + String(localized: "cache stale") : ""
        return "\(src) · 1 USD = \(rate) \(fx.display)\(stale)"
    }

    var body: some View {
        @Bindable var model = model
        Group {
            Section {
                Toggle("Open at login", isOn: $launchAtLogin)
                    .onChange(of: launchAtLogin) { _, on in
                        do {
                            try Prefs.setLaunchAtLogin(on)
                            loginError = nil
                        } catch {
                            loginError = error.localizedDescription
                            launchAtLogin = Prefs.launchAtLogin
                        }
                    }
                if let loginError {
                    Text(loginError).scaledFont(.caption).foregroundStyle(.red)
                }
                Toggle("Show in Dock", isOn: $showInDock)
                    .onChange(of: showInDock) { _, _ in Prefs.applyDockVisibility(refocus: true) }
                Toggle("Show today's cost in the menu bar", isOn: $menuBarShowsCost)
                Picker("When closing the window", selection: $closeAction) {
                    Text("Ask every time").tag("")
                    Text("Keep running in the menu bar").tag("menubar")
                    Text("Quit").tag("quit")
                }
            } header: {
                Text("General")
            } footer: {
                Text("Opened at login, GlobalTokenTracker starts in the menu bar without a window.")
            }

            Section {
                Picker("Language", selection: $language) {
                    Text("System").tag("")
                    Text("中文").tag("zh-Hans")
                    Text("English").tag("en")
                }
                .onChange(of: language) { _, l in
                    Prefs.applyLanguage(l)
                    languageChanged = true
                }
                if languageChanged {
                    HStack {
                        Text("Relaunch to apply the new language.").foregroundStyle(.secondary)
                        Spacer()
                        Button("Relaunch Now") { Prefs.relaunch() }
                    }
                }
            }

            Section {
                Picker("Display currency", selection: $displayCurrency) {
                    ForEach(Self.currencyOptions) { opt in
                        Text("\(opt.code) \(opt.symbol)").tag(opt.code)
                    }
                }
                .onChange(of: displayCurrency) { _, code in
                    Task {
                        if let st = try? await Core.setDisplayCurrency(code) {
                            fx = st
                        }
                    }
                }
                Text(fxNote)
                    .scaledFont(.caption)
                    .foregroundStyle(.secondary)
                if let stamp = fx?.fetchedAtMs {
                    Text("Synced \(Fmt.timestamp(stamp))")
                        .scaledFont(.caption).foregroundStyle(.secondary)
                }
                HStack {
                    Button(fxRefreshing ? String(localized: "Syncing…") : String(localized: "Sync now")) {
                        guard !fxRefreshing else { return }
                        fxRefreshing = true
                        fxError = nil
                        Task {
                            defer { fxRefreshing = false }
                            do {
                                let status = try await Core.refreshFx()
                                fx = status
                                Fmt.setDisplay(code: status.display, usdTo: status.usdTo)
                                model.requestLoad(forceViews: true)
                            } catch { fxError = error.localizedDescription }
                        }
                    }
                    .disabled(fxRefreshing)
                    if fxRefreshing { ProgressView().controlSize(.small) }
                }
                if let fxError {
                    Text("Rate sync failed; cached rates remain available. \(fxError)")
                        .scaledFont(.caption).foregroundStyle(.red)
                }
            } header: {
                Text("Currency")
            } footer: {
                Text("Amounts convert at the cached rate for display; the ledger always stays USD.")
            }
            .task {
                if let st = try? await Core.fxStatus() {
                    displayCurrency = st.display
                    fx = st
                }
            }

            Section("Refresh") {
                Picker("Rescan sources", selection: $model.refreshSeconds) {
                    ForEach(AppModel.refreshOptions, id: \.self) { s in
                        Text(Prefs.refreshLabel(s)).tag(s)
                    }
                }
            }
        }
    }
}

struct AppearanceSettings: View {
    @AppStorage(Prefs.themeKey) private var theme = "system"
    @AppStorage(Prefs.accentKey) private var accent = ""
    @AppStorage(Prefs.chartFontKey) private var chartFont = ""
    @AppStorage(Prefs.textSizeKey) private var textSize = Typography.defaultSize

    private static let fonts: [String] = NSFontManager.shared.availableFontFamilies.sorted()

    var body: some View {
        Group {
            Section("Appearance") {
                Picker("Theme", selection: $theme) {
                    Text("System").tag("system")
                    Text("Light").tag("light")
                    Text("Dark").tag("dark")
                }
                .pickerStyle(.segmented)
                .onChange(of: theme) { _, _ in Prefs.applyTheme() }
                LabeledContent("Text size") {
                    HStack(spacing: 10) {
                        Text("A").font(.system(size: 11))
                        Slider(value: $textSize, in: Typography.range, step: 1)
                            .frame(width: 180)
                        Text("A").font(.system(size: 17))
                        Text("\(Int(textSize)) pt").monospacedDigit().foregroundStyle(.secondary)
                            .frame(width: 44, alignment: .trailing)
                    }
                }
                .help("Base size for all text; headings and captions scale with it")
                Picker("Accent color", selection: $accent) {
                    Text("System").tag("")
                    ForEach(Prefs.accents, id: \.hex) { a in
                        Label {
                            Text(LocalizedStringKey(a.name))
                        } icon: {
                            Image(systemName: "circle.fill").foregroundStyle(Color(hex: a.hex))
                        }
                        .tag(a.hex)
                    }
                }
            }
            Section {
                Picker("Chart font", selection: $chartFont) {
                    Text("System (SF Pro)").tag("")
                    Divider()
                    ForEach(Self.fonts, id: \.self) { f in Text(f).tag(f) }
                }
            } footer: {
                Text("Applies to chart text only; the interface follows the system font.")
            }
        }
    }
}

struct DataSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            Section("Ledger") {
                LabeledContent("Location") {
                    Text(model.info?.dbPath ?? "—")
                        .textSelection(.enabled)
                        .truncationMode(.middle)
                        .lineLimit(1)
                }
                Button("Show in Finder") {
                    if let p = model.info?.dbPath {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: p)])
                    }
                }
                .disabled(model.info == nil)
                LabeledContent("Requests recorded", value: Fmt.count(model.vm?.all.events ?? 0))
                LabeledContent("Tokens recorded", value: Fmt.tokensExact(model.vm?.all.tokens ?? 0))
                LabeledContent("Total cost", value: Fmt.usd(model.vm?.all.costUsd ?? 0))
            }
            Section {
                Toggle("External coordination interface", isOn: Binding(
                    get: { model.externalInterface?.enabled ?? false },
                    set: { model.setExternalInterface($0) }
                ))
                .disabled(model.externalInterface == nil)
                LabeledContent("Interface status") {
                    if let s = model.externalInterface {
                        if s.error != nil {
                            Text("Couldn't save the setting; showing the actual state").foregroundStyle(.red)
                        } else if s.invalid {
                            Text("Settings file is invalid — interface blocked").foregroundStyle(.orange)
                        } else {
                            Text(s.enabled ? "On" : "Off").foregroundStyle(.secondary)
                        }
                    } else {
                        Text("—")
                    }
                }
            } header: {
                Text("External Coordination")
            } footer: {
                Text("Turning this off stops coordination queries and new model routing. Usage tracking is unaffected; requests already sent are allowed to finish.")
            }
            Section {
            } footer: {
                Text("Usage is read locally from each tool's own logs. Nothing leaves this Mac except price, quota and update lookups.")
            }
        }
    }
}

struct UpdateSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Group {
            Section("Updates") {
                LabeledContent("Current version", value: model.info?.version ?? "—")
                Picker("Update channel", selection: $model.updateChannel) {
                    Text("Stable").tag("stable")
                    Text("Preview").tag("alpha")
                }
                if model.updateChannel == "alpha" {
                    Text("Preview builds contain features that aren't released yet and may be unstable.")
                        .scaledFont(.caption).foregroundStyle(.orange)
                }
                Toggle("Check for updates automatically", isOn: $model.updateAuto)
                Text("At launch and every 24 hours.").scaledFont(.caption).foregroundStyle(.secondary)
            }
            Section {
                HStack {
                    status
                    Spacer()
                    actions
                }
            }
        }
    }

    @ViewBuilder private var status: some View {
        switch model.updateState {
        case .idle: Text("Not checked yet").foregroundStyle(.secondary)
        case .checking: HStack { ProgressView().controlSize(.small); Text("Checking…") }
        case .upToDate: Label("You're up to date.", systemImage: "checkmark.circle").foregroundStyle(.secondary)
        case .available(let u):
            VStack(alignment: .leading) {
                Text("New version \(u.version) is available").fontWeight(.medium)
                if let url = URL(string: u.htmlUrl) { Link("Release notes", destination: url).scaledFont(.caption) }
            }
        case .downloading: HStack { ProgressView().controlSize(.small); Text("Downloading and verifying…") }
        case .downloaded(let u):
            Text("Version \(u.version) is open in Finder — drag it to Applications to finish.")
        case .failed(let e): Text("Update failed: \(e)").foregroundStyle(.red).lineLimit(2).help(e)
        }
    }

    @ViewBuilder private var actions: some View {
        switch model.updateState {
        case .available:
            Button("Update Now") { model.downloadUpdate() }.buttonStyle(.glassProminent)
        case .failed:
            Button("Retry") { model.checkForUpdate() }
        case .checking, .downloading:
            EmptyView()
        default:
            Button("Check Now") { model.checkForUpdate() }
        }
    }
}

extension Color {
    /// "#rrggbb" → Color (accent palette shared with the WinUI shell).
    init(hex: String) {
        let v = UInt64(hex.trimmingCharacters(in: CharacterSet(charactersIn: "#")), radix: 16) ?? 0
        self.init(red: Double((v >> 16) & 0xFF) / 255, green: Double((v >> 8) & 0xFF) / 255,
                  blue: Double(v & 0xFF) / 255)
    }
}
