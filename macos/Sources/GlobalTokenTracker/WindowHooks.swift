// AppKit hooks SwiftUI doesn't expose: the close-button policy of the main
// window (Windows close_action: ask / keep in menu bar / quit) and hiding it
// on a login launch. Also the app-wide accent tint and chart font.
import AppKit
import SwiftUI

/// Attaches to the main window once it exists.
struct MainWindowHook: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView { HookView() }
    func updateNSView(_ nsView: NSView, context: Context) {}

    private final class HookView: NSView {
        private var observer: NSObjectProtocol?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            guard let window, observer == nil else { return }
            if Prefs.consumeLoginLaunch() {
                DispatchQueue.main.async { window.close() }
            }
            observer = NotificationCenter.default.addObserver(
                forName: NSWindow.willCloseNotification, object: window, queue: .main
            ) { _ in
                MainActor.assumeIsolated { CloseAction.windowClosed() }
            }
        }

        override func viewWillMove(toWindow newWindow: NSWindow?) {
            super.viewWillMove(toWindow: newWindow)
            if newWindow == nil, let observer {
                NotificationCenter.default.removeObserver(observer)
                self.observer = nil
            }
        }
    }
}

@MainActor
enum CloseAction {
    static func windowClosed() {
        // A quit already in progress closes windows too — don't prompt then.
        guard !isTerminating else { return }
        switch UserDefaults.standard.string(forKey: Prefs.closeActionKey) ?? "" {
        case "quit":
            NSApp.terminate(nil)
        case "menubar":
            break
        default:
            ask()
        }
    }

    static var isTerminating = false

    /// The WinUI close prompt: quit or keep running, with "remember my choice".
    private static func ask() {
        let alert = NSAlert()
        alert.messageText = String(localized: "Keep GlobalTokenTracker running?")
        alert.informativeText = String(localized: "It can keep tracking from the menu bar, or quit completely.")
        alert.addButton(withTitle: String(localized: "Keep in Menu Bar"))
        alert.addButton(withTitle: String(localized: "Quit"))
        alert.showsSuppressionButton = true
        alert.suppressionButton?.title = String(localized: "Remember my choice")
        NSApp.activate()
        let quit = alert.runModal() == .alertSecondButtonReturn
        if alert.suppressionButton?.state == .on {
            UserDefaults.standard.set(quit ? "quit" : "menubar", forKey: Prefs.closeActionKey)
        }
        if quit { NSApp.terminate(nil) }
    }
}

extension View {
    /// The user's accent override ("" = system accent).
    func appTint() -> some View { modifier(AppTint()) }

    /// Chart text in the user's chart font (Windows: theme font_family).
    func chartFont() -> some View { modifier(ChartFont()) }
}

private struct AppTint: ViewModifier {
    @AppStorage(Prefs.accentKey) private var accent = ""

    func body(content: Content) -> some View {
        if accent.isEmpty {
            content
        } else {
            content.tint(Color(hex: accent)).accentColor(Color(hex: accent))
        }
    }
}

private struct ChartFont: ViewModifier {
    @AppStorage(Prefs.chartFontKey) private var family = ""

    func body(content: Content) -> some View {
        if family.isEmpty {
            content
        } else {
            content.font(.custom(family, size: NSFont.smallSystemFontSize))
        }
    }
}
