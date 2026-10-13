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
        private var occlusion: NSObjectProtocol?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            guard let window, observer == nil else { return }
            if Prefs.consumeLoginLaunch() {
                DispatchQueue.main.async { window.close() }
            }
            observer = NotificationCenter.default.addObserver(
                forName: NSWindow.willCloseNotification, object: window, queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    AppModel.shared.windowVisible = false
                    CloseAction.windowClosed()
                }
            }
            occlusion = NotificationCenter.default.addObserver(
                forName: NSWindow.didChangeOcclusionStateNotification, object: window, queue: .main
            ) { [weak window] _ in
                MainActor.assumeIsolated {
                    AppModel.shared.windowVisible = window?.occlusionState.contains(.visible) ?? false
                }
            }
        }

        override func viewWillMove(toWindow newWindow: NSWindow?) {
            super.viewWillMove(toWindow: newWindow)
            if newWindow == nil {
                if let observer { NotificationCenter.default.removeObserver(observer) }
                if let occlusion { NotificationCenter.default.removeObserver(occlusion) }
                observer = nil
                occlusion = nil
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
    @Environment(\.textScale) private var scale

    /// Axis/label text follows both the chart font and the text size.
    func body(content: Content) -> some View {
        let size = (NSFont.smallSystemFontSize * scale).rounded()
        if family.isEmpty {
            content.font(.system(size: size))
        } else {
            content.font(.custom(family, size: size))
        }
    }
}

/// Scrollers that blend in: every scroll view in the window uses thin
/// overlay scrollers that appear while scrolling and fade out, instead of
/// the legacy always-on tracks ("Show scroll bars: Always", or a mouse
/// attached) whose opaque lanes sat on top of table cells. Tables size their
/// columns to fit, so horizontal scrollers are switched off where the
/// content is designed to fit. Re-applied when AppKit resets the style
/// (system preference changes) and whenever pages are built or settle.
@MainActor
enum ScrollerStyle {
    /// Room the overlay knob needs at a table's trailing edge.
    static let lane: CGFloat = 10

    private static var observing = false

    static func apply() {
        if !observing {
            observing = true
            NotificationCenter.default.addObserver(
                forName: NSScroller.preferredScrollerStyleDidChangeNotification, object: nil, queue: .main
            ) { _ in
                MainActor.assumeIsolated { apply() }
            }
            // Text size changed in Settings: re-measure the tables' rows.
            NotificationCenter.default.addObserver(
                forName: UserDefaults.didChangeNotification, object: nil, queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    let size = UserDefaults.standard.double(forKey: Prefs.textSizeKey)
                    guard size != lastTextSize else { return }
                    lastTextSize = size
                    apply()
                }
            }
        }
        for window in NSApp.windows {
            guard let root = window.contentView else { continue }
            visit(root)
        }
    }

    /// Shrink columns proportionally (never below their minimum) until the
    /// table fits its clip view. SwiftUI applies the columns' ideal widths
    /// but AppKit adds intercell spacing and edge padding on top, so a
    /// 9-column table overflowed by ~84 pt. Once it fits, the table's own
    /// uniform autoresizing keeps it fitting as the window resizes.
    private static func fit(_ table: NSTableView, into width: CGFloat) {
        guard width > 0 else { return }
        let cols = table.tableColumns.filter { !$0.isHidden }
        // Measured, not assumed: intercell spacing plus the table style's
        // own edge padding (≈18.5 pt per column here).
        let columns = cols.reduce(0) { $0 + $1.width }
        let spacing = max(table.intercellSpacing.width * CGFloat(cols.count), table.frame.width - columns)
        var excess = columns + spacing - width
        guard excess > 0.5 else { return }
        for _ in 0..<4 where excess > 0.5 {
            let room = cols.reduce(0) { $0 + max(0, $1.width - $1.minWidth) }
            guard room > 0 else { break }
            for c in cols {
                let give = max(0, c.width - c.minWidth) / room * excess
                c.width = max(c.minWidth, c.width - give)
            }
            excess = cols.reduce(0) { $0 + $1.width } + spacing - width
        }
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
    }

    /// Text size each table's fixed row height was measured at.
    private static var measuredAt: [ObjectIdentifier: Double] = [:]
    private static var retryArmed = false
    private static var retries = 0
    private static var lastTextSize: Double?

    /// SwiftUI's Table runs its NSTableView with automatic row heights:
    /// every row scrolling into view had AppKit run Auto Layout over each
    /// cell's hosting view to measure it — half the main-thread time of a
    /// table scroll (profiled), and the frames it dropped. Every row here is
    /// one line, so the height never varies: measure one row once, at the
    /// current text size, and use it as a fixed height. A text size change
    /// switches automatic heights back on for one measurement.
    private static func fixRowHeight(_ table: NSTableView) {
        let id = ObjectIdentifier(table)
        let size = UserDefaults.standard.double(forKey: Prefs.textSizeKey)
        if let at = measuredAt[id], at != size {
            table.usesAutomaticRowHeights = true
            measuredAt[id] = nil
            retryLater()
            return
        }
        guard table.usesAutomaticRowHeights else { return }
        guard table.numberOfRows > 0 else {
            retryLater() // rows arrive after the page settles
            return
        }
        let height = table.rect(ofRow: 0).height - table.intercellSpacing.height
        guard height > 4 else { return }
        retries = 0
        table.usesAutomaticRowHeights = false
        table.rowHeight = height.rounded()
        if MotionProbe.enabled {
            FileHandle.standardError.write(Data("[rows] fixed \(table.rowHeight) pt for \(table.numberOfRows) rows\n".utf8))
        }
        measuredAt[id] = size
    }

    private static func retryLater() {
        // A table may simply be empty (no sources yet): give up after a
        // while; the next page turn calls apply() again anyway.
        guard !retryArmed, retries < 10 else { return }
        retries += 1
        retryArmed = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
            retryArmed = false
            apply()
        }
    }

    private static func visit(_ view: NSView) {
        if let scroll = view as? NSScrollView {
            if scroll.scrollerStyle != .overlay { scroll.scrollerStyle = .overlay }
            scroll.autohidesScrollers = true
            scroll.scrollerKnobStyle = .default
            // Tables are laid out to fit their width; never show a
            // horizontal scroller (or the corner square it brings).
            if let table = scroll.documentView as? NSTableView {
                scroll.hasHorizontalScroller = false
                fit(table, into: scroll.contentView.bounds.width)
                fixRowHeight(table)
            }
        }
        for sub in view.subviews { visit(sub) }
    }
}
