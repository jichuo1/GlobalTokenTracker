// Developer probe for animation smoothness — inert unless the app is
// launched with GTT_MOTION_PROBE=1. It turns through every page twice and,
// using the window's display link, logs each transition's frame stats to
// stderr: frames, dropped frames (gap > 1.5× the display interval) and the
// worst frame time. Lets "is it smooth?" be answered with numbers.
import AppKit
import QuartzCore

/// Diagnostic switches for bisecting frame drops with the probe — read once
/// at launch, inert unless set: GTT_PROBE_NO_{SEAM,CASCADE,DRUM,CHROME,SIDEBAR}.
enum ProbeFlags {
    private static let env = ProcessInfo.processInfo.environment
    static let noSeam = env["GTT_PROBE_NO_SEAM"] != nil
    static let noCascade = env["GTT_PROBE_NO_CASCADE"] != nil
    static let noDrum = env["GTT_PROBE_NO_DRUM"] != nil
    static let noChrome = env["GTT_PROBE_NO_CHROME"] != nil
    static let noSidebar = env["GTT_PROBE_NO_SIDEBAR"] != nil
    /// Opens one title-bar panel shortly after launch (calendar, tools,
    /// models, customize, refresh) for screenshots.
    static let panel = env["GTT_PROBE_PANEL"]
    /// Title-bar controls switch after the turn instead of with it (A/B).
    static let noToolbarMotion = env["GTT_PROBE_NO_TOOLBAR_MOTION"] != nil
    /// Pages become interactive when the spring is removed, not when it is
    /// logically complete (the old behaviour, for A/B).
    static let lateSettle = env["GTT_PROBE_LATE_SETTLE"] != nil
    /// The resting page at exactly 0° (no 3D transform) — for A/B.
    static let flatRest = env["GTT_PROBE_FLAT_REST"] != nil
}

@MainActor
final class MotionProbe: NSObject {
    static let shared = MotionProbe()
    static var enabled: Bool { ProcessInfo.processInfo.environment["GTT_MOTION_PROBE"] != nil }

    private var link: CADisplayLink?
    private var last: CFTimeInterval = 0
    private var gaps: [CFTimeInterval] = []
    private var interval: CFTimeInterval = 1.0 / 60
    private var recording = false

    func start(model: AppModel) {
        guard Self.enabled else { return }
        Task { @MainActor in
            // Let startup work finish first (initial scan, price download,
            // the Overview rebuild they trigger) so it doesn't land on turns.
            try? await Task.sleep(for: .seconds(4))
            while model.isLoading || model.isRefreshingPrices {
                try? await Task.sleep(for: .milliseconds(250))
            }
            try? await Task.sleep(for: .seconds(2))
            guard let view = NSApp.windows.first(where: { $0.isVisible && $0.contentView != nil })?.contentView else {
                FileHandle.standardError.write(Data("[probe] no window\n".utf8))
                return
            }
            let link = view.displayLink(target: self, selector: #selector(tick(_:)))
            link.add(to: .main, forMode: .common)
            self.link = link
            let scrollOnly = ProcessInfo.processInfo.environment["GTT_PROBE_SCROLL_ALL"] != nil
                || ProcessInfo.processInfo.environment["GTT_PROBE_SCROLL_PAGES"] != nil
            let tour = Page.allCases + Page.allCases.reversed()
            let order = scrollOnly ? [] : tour + tour + [.overview]
            var prev = model.page
            for page in order where page != prev {
                gaps.removeAll()
                last = 0
                recording = true
                model.page = page
                try? await Task.sleep(for: .milliseconds(Int(ProcessInfo.processInfo.environment["GTT_PROBE_WINDOW_MS"] ?? "") ?? 700))
                recording = false
                report("\(prev.rawValue) → \(page.rawValue)")
                prev = page
                let rest = Int(ProcessInfo.processInfo.environment["GTT_PROBE_REST_MS"] ?? "") ?? 500
                try? await Task.sleep(for: .milliseconds(rest))
            }
            // Multi-page jumps (the drum passes the pages in between) and an
            // interrupted turn (re-targeted mid-flight, keeping velocity).
            let jumps: [(String, [(Page, Int)])] = [
                ("jump overview → prices", [(.prices, 0)]),
                ("jump prices → overview", [(.overview, 0)]),
                ("jump overview → sources", [(.sources, 0)]),
                ("jump sources → overview", [(.overview, 0)]),
                ("interrupt → prices ↩ detail", [(.prices, 0), (.detail, 180)]),
            ]
            for (name, steps) in scrollOnly ? [] : jumps {
                gaps.removeAll()
                last = 0
                recording = true
                for (page, after) in steps {
                    if after > 0 { try? await Task.sleep(for: .milliseconds(after)) }
                    model.page = page
                }
                try? await Task.sleep(for: .milliseconds(1300))
                recording = false
                report(name)
                try? await Task.sleep(for: .milliseconds(900))
            }
            // Scrolling: an animated scroll down and back up on the Overview
            // (SwiftUI cards + charts) and the Details table (AppKit).
            let scrollPages = ProcessInfo.processInfo.environment["GTT_PROBE_SCROLL_PAGES"]
                .map { $0.split(separator: ",").compactMap { Page(rawValue: String($0)) } }
                ?? (ProcessInfo.processInfo.environment["GTT_PROBE_SCROLL_ALL"] != nil
                    ? Page.allCases : [Page.overview, .detail])
            for page in scrollPages {
                model.page = page
                try? await Task.sleep(for: .milliseconds(1500))
                guard let scroll = largestVisibleScrollView(in: view) else { continue }
                let wheel = ProcessInfo.processInfo.environment["GTT_PROBE_WHEEL"] != nil
                for (dir, label) in [(1.0, "down"), (-1.0, "up")] {
                    let clip = scroll.contentView
                    let maxY = max(0, (scroll.documentView?.frame.height ?? 0) - clip.bounds.height)
                    let distance = Double(ProcessInfo.processInfo.environment["GTT_PROBE_SCROLL_PX"] ?? "") ?? 2400
                    let target = dir > 0 ? min(maxY, clip.bounds.origin.y + distance) : 0
                    gaps.removeAll()
                    last = 0
                    recording = true
                    if wheel, let window = scroll.window {
                        // Real scroll-wheel events (pixel deltas, one per
                        // frame) through the window, like a fast trackpad
                        // swipe: AppKit's responsive scrolling path.
                        let point = scroll.convert(NSPoint(x: scroll.bounds.midX, y: scroll.bounds.midY), to: nil)
                        let screen = window.convertPoint(toScreen: point)
                        let flipped = CGPoint(x: screen.x, y: (NSScreen.screens.first?.frame.height ?? 0) - screen.y)
                        for _ in 0..<66 {
                            if let cg = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 1,
                                                wheel1: Int32(dir > 0 ? -30 : 30), wheel2: 0, wheel3: 0) {
                                cg.location = flipped
                                if let ev = NSEvent(cgEvent: cg) { window.sendEvent(ev) }
                            }
                            try? await Task.sleep(for: .milliseconds(16))
                        }
                        try? await Task.sleep(for: .milliseconds(100))
                    } else {
                        NSAnimationContext.runAnimationGroup { ctx in
                            ctx.duration = 1.0
                            ctx.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
                            clip.animator().setBoundsOrigin(NSPoint(x: clip.bounds.origin.x, y: target))
                        }
                        try? await Task.sleep(for: .milliseconds(1150))
                    }
                    recording = false
                    scroll.reflectScrolledClipView(clip)
                    report("scroll \(page.rawValue) \(label)")
                    try? await Task.sleep(for: .milliseconds(500))
                }
            }
            link.invalidate()
            FileHandle.standardError.write(Data("[probe] done\n".utf8))
        }
    }

    /// The biggest on-screen, unhidden scroll view — the current page's.
    private func largestVisibleScrollView(in root: NSView) -> NSScrollView? {
        var best: NSScrollView?
        func visit(_ v: NSView) {
            if let s = v as? NSScrollView, !s.isHiddenOrHasHiddenAncestor, s.window != nil,
               s.visibleRect.width > 300, (s.documentView?.frame.height ?? 0) > s.contentView.bounds.height {
                if best == nil || s.visibleRect.width * s.visibleRect.height
                    > best!.visibleRect.width * best!.visibleRect.height { best = s }
            }
            v.subviews.forEach(visit)
        }
        visit(root)
        return best
    }

    @objc private func tick(_ link: CADisplayLink) {
        interval = link.targetTimestamp - link.timestamp
        guard recording else { return }
        if last > 0 { gaps.append(link.timestamp - last) }
        last = link.timestamp
    }

    private func report(_ name: String) {
        let dropped = gaps.filter { $0 > interval * 1.5 }.count
        let worst = (gaps.max() ?? 0) * 1000
        let line = String(format: "[probe] %-22@ frames %3d  dropped %2d  worst %5.1f ms  (display %.1f ms)\n",
                          name as NSString, gaps.count, dropped, worst, interval * 1000)
        FileHandle.standardError.write(Data(line.utf8))
        if ProcessInfo.processInfo.environment["GTT_MOTION_PROBE"] == "verbose" {
            let seq = gaps.map { String(format: "%.0f", $0 * 1000) }.joined(separator: " ")
            FileHandle.standardError.write(Data("[probe]   gaps ms: \(seq)\n".utf8))
        }
    }
}

/// Developer snapshot — inert unless GTT_SNAPSHOT=<png path> is set. Shows
/// GTT_SNAPSHOT_PAGE (default overview) once startup work has settled, then
/// renders the window's own layer tree to a PNG and quits. In-process
/// rendering needs no screen-recording permission, so layout can be checked
/// without a human looking at the screen.
@MainActor
enum Snapshot {
    /// Geometry of every visible table: frames, insets, column widths.
    static func dumpTables(_ v: NSView) {
        if let t = v as? NSTableView, !t.isHiddenOrHasHiddenAncestor, let s = t.enclosingScrollView,
           s.window != nil, s.visibleRect.width > 0 {
            let cols = t.tableColumns.map { "\($0.title)=\(Int($0.width))" }.joined(separator: " ")
            let line = "[tables] scroll \(s.frame.integral) clip \(s.contentView.bounds.integral) insets \(s.contentInsets) "
                + "table \(t.frame.integral) autoresize \(t.columnAutoresizingStyle.rawValue) sum \(Int(t.tableColumns.reduce(0) { $0 + $1.width })) | \(cols)\n"
            FileHandle.standardError.write(Data(line.utf8))
        }
        for sub in v.subviews { dumpTables(sub) }
    }

    static func start(model: AppModel) {
        let env = ProcessInfo.processInfo.environment
        guard let path = env["GTT_SNAPSHOT"] else { return }
        let page = env["GTT_SNAPSHOT_PAGE"].flatMap(Page.init(rawValue:)) ?? .overview
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(3))
            while model.isLoading || model.isRefreshingPrices {
                try? await Task.sleep(for: .milliseconds(250))
            }
            model.page = page
            try? await Task.sleep(for: .seconds(3.5))
            guard let window = NSApp.windows.first(where: { $0.isVisible && $0.contentView != nil }),
                  let view = window.contentView?.superview ?? window.contentView else { return }
            if env["GTT_SNAPSHOT_TABLES"] != nil { dumpTables(view) }
            let bounds = view.bounds
            let scale = window.backingScaleFactor
            let width = Int(bounds.width * scale), height = Int(bounds.height * scale)
            guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                                      space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return }
            ctx.scaleBy(x: scale, y: scale)
            if view.isFlipped {
                ctx.translateBy(x: 0, y: bounds.height)
                ctx.scaleBy(x: 1, y: -1)
            }
            view.layer?.render(in: ctx)
            if let image = ctx.makeImage() {
                let rep = NSBitmapImageRep(cgImage: image)
                try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
            }
            NSApp.terminate(nil)
        }
    }
}

/// Toolbar probe — inert unless GTT_TOOLBAR_PROBE is set. Brings the window
/// to (100, 100) in front, then picks ranges in fast and slow bursts (the
/// segments' own code path) and turns pages, so a recording of the title bar
/// shows how the toolbar behaves under fast input.
@MainActor
enum ToolbarProbe {
    static func start(model: AppModel) {
        guard ProcessInfo.processInfo.environment["GTT_TOOLBAR_PROBE"] != nil else { return }
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(4))
            while model.isLoading { try? await Task.sleep(for: .milliseconds(250)) }
            guard let window = NSApp.windows.first(where: { $0.isVisible && $0.contentView != nil }) else { return }
            NSApp.activate()
            let env = ProcessInfo.processInfo.environment
            let width = env["GTT_TOOLBAR_WIDTH"].flatMap(Double.init) ?? 1180
            window.setFrame(NSRect(x: 100, y: (NSScreen.main?.frame.height ?? 1000) - 100 - 780, width: width, height: 780),
                            display: true)
            window.makeKeyAndOrderFront(nil)
            // Still shot of one page's title bar instead of the run below.
            if let page = env["GTT_TOOLBAR_PAGE"].flatMap(Page.init(rawValue:)) {
                model.page = page
                try? await Task.sleep(for: .seconds(2))
                if let y = env["GTT_TOOLBAR_SCROLL"].flatMap(Double.init), let root = window.contentView {
                    // Every page stays mounted: scroll each page's scroller.
                    @MainActor func visit(_ v: NSView) {
                        if let s = v as? NSScrollView, s.frame.width > 300,
                           (s.documentView?.frame.height ?? 0) > s.contentView.bounds.height {
                            s.contentView.setBoundsOrigin(NSPoint(x: 0, y: y))
                            s.reflectScrolledClipView(s.contentView)
                        }
                        v.subviews.forEach(visit)
                    }
                    visit(root)
                    try? await Task.sleep(for: .seconds(1.5))
                }
                log("shot")
                return
            }
            log("ready")
            try? await Task.sleep(for: .seconds(2))
            log("burst fast")
            for i in [0, 1, 2, 3, 0, 3, 1, 2, 1] {
                model.selectPreset(RangeSpec.presets[i].key)
                try? await Task.sleep(for: .milliseconds(70))
            }
            try? await Task.sleep(for: .milliseconds(1500))
            log("burst slow")
            for i in [2, 0, 3, 1] {
                model.selectPreset(RangeSpec.presets[i].key)
                try? await Task.sleep(for: .milliseconds(220))
            }
            try? await Task.sleep(for: .milliseconds(1500))
            log("pages")
            for p in [Page.detail, .quota, .prices, .overview, .settings, .overview] {
                model.page = p
                try? await Task.sleep(for: .milliseconds(1200))
            }
            log("done")
        }
    }

    private static func log(_ s: String) {
        FileHandle.standardError.write(Data("[toolbar] \(s) \(Date().timeIntervalSince1970)\n".utf8))
    }
}
