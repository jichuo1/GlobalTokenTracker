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
                .appTint()
                .appTextScale()
                .background(MainWindowHook())
        }
        .defaultSize(width: 1180, height: 780)
        .windowToolbarStyle(.unified)
        .commands { AppCommands(model: model) }

        MenuBarExtra {
            MenuBarPanel()
                .environment(model)
                .appTint()
                .appTextScale()
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
    func applicationWillFinishLaunching(_ notification: Notification) {
        Prefs.noteLoginLaunch()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        Prefs.noteLoginLaunch()
        Prefs.applyDockVisibility()
        Prefs.applyTheme()
        // Start loading before any scene appears: the menu bar label shows
        // today's numbers even if the window is never opened (login launch).
        MainActor.assumeIsolated {
            AppModel.shared.start()
            MotionProbe.shared.start(model: AppModel.shared)
            Snapshot.start(model: AppModel.shared)
            ToolbarProbe.start(model: AppModel.shared)
        }
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        MainActor.assumeIsolated { CloseAction.isTerminating = true }
        return .terminateNow
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
/// Keys mirror the WinUI `UiConfig` fields they stand in for.
enum Prefs {
    static let showInDockKey = "showInDock"
    /// window_theme: system | light | dark
    static let themeKey = "themeMode"
    /// theme accent: "" (system) or "#rrggbb" from `accents`
    static let accentKey = "accent"
    /// D2D chart font family on Windows → chart text font here ("" = system)
    static let chartFontKey = "chartFont"
    /// lang: "" (system) | "zh-Hans" | "en"
    static let languageKey = "appLanguage"
    /// close_action: "" ask | "menubar" (Windows "tray") | "quit"
    static let closeActionKey = "closeAction"
    /// Body text size in points (Windows: theme font size), 11–17.
    static let textSizeKey = "textSize"

    /// The WinUI accent palette (pages.rs ACCENT_OPTIONS).
    static let accents: [(hex: String, name: String)] = [
        ("#4d8ae8", "Blue"), ("#10a874", "Green"), ("#8b6fd8", "Purple"),
        ("#e8853d", "Orange"), ("#d84d5b", "Red"),
    ]

    @MainActor
    static func applyTheme() {
        switch UserDefaults.standard.string(forKey: themeKey) {
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        default: NSApp.appearance = nil
        }
    }

    /// Per-app language override, read by Bundle at the next launch.
    static func applyLanguage(_ code: String) {
        if code.isEmpty {
            UserDefaults.standard.removeObject(forKey: "AppleLanguages")
        } else {
            UserDefaults.standard.set([code], forKey: "AppleLanguages")
        }
    }

    @MainActor
    static func relaunch() {
        let cfg = NSWorkspace.OpenConfiguration()
        cfg.createsNewApplicationInstance = true
        NSWorkspace.shared.openApplication(at: Bundle.main.bundleURL, configuration: cfg) { _, _ in
            DispatchQueue.main.async { NSApp.terminate(nil) }
        }
    }

    static func refreshText(_ secs: Int) -> String {
        switch secs {
        case 0: String(localized: "Only when files change")
        case 60: String(localized: "Every minute")
        case 300: String(localized: "Every 5 minutes")
        default: String(localized: "Every \(secs) seconds")
        }
    }

    static func refreshLabel(_ secs: Int) -> LocalizedStringKey {
        switch secs {
        case 0: "Only when files change"
        case 60: "Every minute"
        case 300: "Every 5 minutes"
        default: "Every \(secs) seconds"
        }
    }

    /// Opened as a login item → start in the menu bar with no window (the
    /// Windows build's `--minimized` autostart). Set once, consumed once.
    nonisolated(unsafe) private(set) static var launchedAtLogin = false

    static func noteLoginLaunch() {
        guard let ev = NSAppleEventManager.shared().currentAppleEvent,
              ev.eventID == kAEOpenApplication,
              ev.paramDescriptor(forKeyword: keyAEPropData)?.enumCodeValue == keyAELaunchedAsLogInItem
        else { return }
        launchedAtLogin = true
    }

    static func consumeLoginLaunch() -> Bool {
        defer { launchedAtLogin = false }
        return launchedAtLogin
    }

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
        // Settings is a page of the main window (as on Windows); ⌘, goes there.
        CommandGroup(replacing: .appSettings) {
            Button("Settings…") {
                model.page = .settings
                openWindow(id: "main")
                NSApp.activate()
            }
            .keyboardShortcut(",")
        }
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
