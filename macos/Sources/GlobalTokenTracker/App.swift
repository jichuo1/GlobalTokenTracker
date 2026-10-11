// Entry point. Three scenes, the macOS-native split of the WinUI shell:
// - the main window (sidebar navigation, unified toolbar),
// - Settings (⌘,) instead of an in-window settings page,
// - a menu bar extra instead of the tray icon; closing the window keeps the
//   app resident there, like the Windows build's hide-to-tray.
import AppKit
import ServiceManagement
import SwiftUI

@main
struct GlobalTokenTrackerApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    private let model = AppModel.shared

    var body: some Scene {
        Window("GlobalTokenTracker", id: "main") {
            RootView()
                .environment(model)
                .frame(minWidth: 820, minHeight: 560)
        }
        .defaultSize(width: 1180, height: 780)
        .windowToolbarStyle(.unified)
        .commands { AppCommands(model: model) }

        Settings {
            SettingsView()
                .environment(model)
        }

        MenuBarExtra {
            MenuBarPanel()
                .environment(model)
        } label: {
            MenuBarLabel(model: model)
        }
        .menuBarExtraStyle(.window)
    }
}

extension AppModel {
    /// One engine-facing model per process: the window, Settings and the menu
    /// bar extra all observe the same state (and the same single load queue).
    static let shared = AppModel()
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        Prefs.applyDockVisibility()
        // Start loading before any scene appears: the menu bar label shows
        // today's numbers even if the window is never opened (login launch).
        MainActor.assumeIsolated { AppModel.shared.start() }
    }

    /// Menu-bar resident: closing the last window must not quit.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// Dock icon click with no window open → bring the main window back.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        !hasVisibleWindows
    }
}

/// App-level preferences that act on AppKit / launchd rather than the engine.
enum Prefs {
    static let showInDockKey = "showInDock"

    static var showInDock: Bool {
        UserDefaults.standard.object(forKey: showInDockKey) as? Bool ?? true
    }

    @MainActor
    static func applyDockVisibility() {
        NSApp.setActivationPolicy(showInDock ? .regular : .accessory)
    }

    /// Login item via SMAppService (macOS 13+): no helper app, user-visible in
    /// System Settings › General › Login Items.
    static var launchAtLogin: Bool {
        SMAppService.mainApp.status == .enabled
    }

    static func setLaunchAtLogin(_ on: Bool) throws {
        if on {
            try SMAppService.mainApp.register()
        } else {
            try SMAppService.mainApp.unregister()
        }
    }
}

struct AppCommands: Commands {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        SidebarCommands()
        CommandGroup(replacing: .newItem) {}
        // Page switching + refresh live in the standard View menu.
        CommandGroup(before: .sidebar) {
            Button("Refresh Now") { model.manualRefresh() }
                .keyboardShortcut("r")
            Divider()
            ForEach(Array(Page.allCases.enumerated()), id: \.element) { i, page in
                Button(page.title) {
                    model.page = page
                    openWindow(id: "main")
                }
                .keyboardShortcut(KeyEquivalent(Character("\(i + 1)")))
            }
            Divider()
        }
    }
}
