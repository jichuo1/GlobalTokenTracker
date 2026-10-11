// Settings (⌘,) — the WinUI settings page as standard macOS tabbed
// preferences: General, Appearance, Data (incl. the external coordination
// interface), Updates. Interface text size is left to macOS (System Settings
// › Accessibility › Display › Text Size), the platform's own per-app control.
import AppKit
import SwiftUI

struct SettingsView: View {
    var body: some View {
        TabView {
            Tab("General", systemImage: "gearshape") { GeneralSettings() }
            Tab("Appearance", systemImage: "paintpalette") { AppearanceSettings() }
            Tab("Data", systemImage: "cylinder.split.1x2") { DataSettings() }
            Tab("Updates", systemImage: "arrow.down.app") { UpdateSettings() }
        }
        .scenePadding()
        .frame(width: 540)
    }
}

private struct GeneralSettings: View {
    @Environment(AppModel.self) private var model
    @AppStorage(Prefs.showInDockKey) private var showInDock = true
    @AppStorage("menuBarShowsCost") private var menuBarShowsCost = true
    @AppStorage(Prefs.closeActionKey) private var closeAction = ""
    @AppStorage(Prefs.languageKey) private var language = ""
    @ViewState private var launchAtLogin = Prefs.launchAtLogin
    @ViewState private var loginError: String?
    @ViewState private var languageChanged = false

    var body: some View {
        @Bindable var model = model
        Form {
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
                    Text(loginError).font(.caption).foregroundStyle(.red)
                }
                Toggle("Show in Dock", isOn: $showInDock)
                    .onChange(of: showInDock) { _, _ in Prefs.applyDockVisibility() }
                Toggle("Show today's cost in the menu bar", isOn: $menuBarShowsCost)
                Picker("When closing the window", selection: $closeAction) {
                    Text("Ask every time").tag("")
                    Text("Keep running in the menu bar").tag("menubar")
                    Text("Quit").tag("quit")
                }
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

            Section("Refresh") {
                Picker("Rescan sources", selection: $model.refreshSeconds) {
                    ForEach(AppModel.refreshOptions, id: \.self) { s in
                        Text(Prefs.refreshLabel(s)).tag(s)
                    }
                }
            }
        }
        .formStyle(.grouped)
    }
}

private struct AppearanceSettings: View {
    @AppStorage(Prefs.themeKey) private var theme = "system"
    @AppStorage(Prefs.accentKey) private var accent = ""
    @AppStorage(Prefs.chartFontKey) private var chartFont = ""

    private static let fonts: [String] = NSFontManager.shared.availableFontFamilies.sorted()

    var body: some View {
        Form {
            Section {
                Picker("Theme", selection: $theme) {
                    Text("System").tag("system")
                    Text("Light").tag("light")
                    Text("Dark").tag("dark")
                }
                .pickerStyle(.segmented)
                .onChange(of: theme) { _, _ in Prefs.applyTheme() }
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
        .formStyle(.grouped)
    }
}

private struct DataSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Form {
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
        .formStyle(.grouped)
    }
}

private struct UpdateSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Form {
            Section {
                LabeledContent("Current version", value: model.info?.version ?? "—")
                Picker("Update channel", selection: $model.updateChannel) {
                    Text("Stable").tag("stable")
                    Text("Preview").tag("alpha")
                }
                if model.updateChannel == "alpha" {
                    Text("Preview builds contain features that aren't released yet and may be unstable.")
                        .font(.caption).foregroundStyle(.orange)
                }
                Toggle("Check for updates automatically", isOn: $model.updateAuto)
                Text("At launch and every 24 hours.").font(.caption).foregroundStyle(.secondary)
            }
            Section {
                HStack {
                    status
                    Spacer()
                    actions
                }
            }
        }
        .formStyle(.grouped)
    }

    @ViewBuilder private var status: some View {
        switch model.updateState {
        case .idle: Text("Not checked yet").foregroundStyle(.secondary)
        case .checking: HStack { ProgressView().controlSize(.small); Text("Checking…") }
        case .upToDate: Label("You're up to date.", systemImage: "checkmark.circle").foregroundStyle(.secondary)
        case .available(let u):
            VStack(alignment: .leading) {
                Text("New version \(u.version) is available").fontWeight(.medium)
                if let url = URL(string: u.htmlUrl) { Link("Release notes", destination: url).font(.caption) }
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
