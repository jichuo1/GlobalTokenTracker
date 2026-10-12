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
            let tour = Page.allCases + Page.allCases.reversed()
            let order = tour + tour + [.overview]
            var prev = model.page
            for page in order where page != prev {
                gaps.removeAll()
                last = 0
                recording = true
                model.page = page
                try? await Task.sleep(for: .milliseconds(700))
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
            for (name, steps) in jumps {
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
            link.invalidate()
            FileHandle.standardError.write(Data("[probe] done\n".utf8))
        }
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
