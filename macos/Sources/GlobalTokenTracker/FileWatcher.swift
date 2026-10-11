// FSEvents watcher over the adapters' source roots. The kernel coalesces
// bursts (`latency`), so an agent appending to its JSONL every few ms costs
// one callback per window, not one per write — the Mac counterpart of the
// WinUI shell's debounced notify watcher.
import CoreServices
import Foundation

final class FileWatcher: @unchecked Sendable {
    private var stream: FSEventStreamRef?
    private let queue = DispatchQueue(label: "gtt.fsevents", qos: .utility)
    private let onChange: @Sendable () -> Void

    init(onChange: @escaping @Sendable () -> Void) {
        self.onChange = onChange
    }

    deinit { stop() }

    /// (Re)start on `paths`; an empty list just stops watching.
    func start(paths: [String], latency: TimeInterval = 2.0) {
        stop()
        guard !paths.isEmpty else { return }
        var ctx = FSEventStreamContext(
            version: 0,
            info: Unmanaged.passUnretained(self).toOpaque(),
            retain: nil, release: nil, copyDescription: nil
        )
        let callback: FSEventStreamCallback = { _, info, _, _, _, _ in
            guard let info else { return }
            Unmanaged<FileWatcher>.fromOpaque(info).takeUnretainedValue().onChange()
        }
        // Directory-level events only: the scan finds what changed itself, so
        // per-file events would just multiply callbacks for nothing.
        let flags = UInt32(kFSEventStreamCreateFlagIgnoreSelf)
        guard let s = FSEventStreamCreate(
            nil, callback, &ctx, paths as CFArray,
            FSEventStreamEventId(kFSEventStreamEventIdSinceNow), latency, flags
        ) else { return }
        FSEventStreamSetDispatchQueue(s, queue)
        FSEventStreamStart(s)
        stream = s
    }

    func stop() {
        guard let s = stream else { return }
        FSEventStreamStop(s)
        FSEventStreamInvalidate(s)
        FSEventStreamRelease(s)
        stream = nil
    }
}
