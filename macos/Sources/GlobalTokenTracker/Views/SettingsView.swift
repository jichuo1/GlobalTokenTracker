// Settings (⌘,) — standard macOS tabbed preferences with grouped forms.
import AppKit
import SwiftUI

struct SettingsView: View {
    var body: some View {
        TabView {
            Tab("General", systemImage: "gearshape") { GeneralSettings() }
            Tab("Data", systemImage: "cylinder.split.1x2") { DataSettings() }
            Tab("Updates", systemImage: "arrow.down.app") { UpdateSettings() }
        }
        .scenePadding()
        .frame(width: 520)
    }
}

private struct GeneralSettings: View {
    @Environment(AppModel.self) private var model
    @AppStorage(Prefs.showInDockKey) private var showInDock = true
    @AppStorage("menuBarShowsCost") private var menuBarShowsCost = true
    @ViewState private var launchAtLogin = Prefs.launchAtLogin
    @ViewState private var loginError: String?

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
            } footer: {
                Text("GlobalTokenTracker keeps running in the menu bar after you close its window.")
            }

            Section("Refresh") {
                Picker("Rescan sources", selection: $model.refreshSeconds) {
                    ForEach(AppModel.refreshOptions, id: \.self) { s in
                        Text(refreshLabel(s)).tag(s)
                    }
                }
            }
        }
        .formStyle(.grouped)
    }

    private func refreshLabel(_ secs: Int) -> LocalizedStringKey {
        switch secs {
        case 0: "Only when files change"
        case 60: "Every minute"
        case 300: "Every 5 minutes"
        default: "Every \(secs) seconds"
        }
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
            }
            Section {
                LabeledContent("Requests recorded", value: Fmt.count(model.vm?.all.events ?? 0))
                LabeledContent("Tokens recorded", value: Fmt.tokensExact(model.vm?.all.tokens ?? 0))
                LabeledContent("Total cost", value: Fmt.usd(model.vm?.all.costUsd ?? 0))
            } header: {
                Text("All Time")
            } footer: {
                Text("Usage is read locally from each tool's own logs. Nothing leaves this Mac except price and quota lookups.")
            }
        }
        .formStyle(.grouped)
    }
}

private struct UpdateSettings: View {
    @Environment(AppModel.self) private var model
    @ViewState private var checking = false

    var body: some View {
        @Bindable var model = model
        Form {
            Section {
                LabeledContent("Current version", value: model.info?.version ?? "—")
                Picker("Channel", selection: $model.updateChannel) {
                    Text("Stable").tag("stable")
                    Text("Alpha").tag("alpha")
                }
                HStack {
                    Button("Check for Updates") {
                        checking = true
                        model.checkForUpdate()
                    }
                    if checking && model.update == nil { ProgressView().controlSize(.small) }
                }
                if let u = model.update {
                    if let new = u.update, let url = URL(string: new.htmlUrl) {
                        LabeledContent("Available") {
                            Link("Version \(new.version)", destination: url)
                        }
                    } else {
                        Text("You're up to date.").foregroundStyle(.secondary)
                    }
                }
            }
        }
        .formStyle(.grouped)
        .onChange(of: model.update?.current) { _, _ in checking = false }
    }
}
