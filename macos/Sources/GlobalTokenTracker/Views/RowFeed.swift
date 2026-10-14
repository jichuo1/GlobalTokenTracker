// Row sources for NativeTable.
//
// `ArrayFeed` serves a list that is already in memory (Prices). `DetailFeed`
// serves the request ledger without ever holding all of it: the table is
// told the full row count up front (a stable scroller, no "load more"
// stalls at the bottom) and rows are fetched around the viewport on demand.
import AppKit

/// What a NativeTable reads rows from. Subclasses push changes back through
/// `refresh` / `reloadAll`; nothing here goes through SwiftUI.
@MainActor
class RowFeed<Row: Identifiable> {
    fileprivate(set) weak var table: NSTableView?

    var count: Int { 0 }
    /// nil: not loaded yet (the table shows a placeholder).
    func row(_ i: Int) -> Row? { nil }
    /// The rows on screen moved; `velocity` in rows per second (+ = down).
    func visible(_ rows: Range<Int>, velocity: Double) {}

    /// Index of a row by id, where known (keeps the selection).
    func index(of id: Row.ID) -> Int? { nil }

    func attach(_ table: NSTableView) { self.table = table }

    /// Redraw rows `range` where they are on screen; nothing else.
    final func refresh(_ range: Range<Int>) {
        guard let table, !range.isEmpty else { return }
        let vis = table.rows(in: table.visibleRect)
        let lo = max(range.lowerBound, vis.location)
        let hi = min(range.upperBound, vis.location + vis.length)
        guard lo < hi else { return }
        table.reloadData(forRowIndexes: IndexSet(integersIn: lo..<hi),
                         columnIndexes: IndexSet(integersIn: 0..<table.numberOfColumns))
    }

    /// The row count or order changed. `insertedAbove` rows were added at
    /// the top: when scrolled down, the view moves with its content
    /// instead of jumping.
    final func reloadAll(insertedAbove shift: Int = 0) {
        guard let table else { return }
        let clip = table.enclosingScrollView?.contentView
        let y = clip?.bounds.origin.y ?? 0
        table.reloadData()
        if let clip, shift > 0, y > 0 {
            let step = table.rowHeight + table.intercellSpacing.height
            clip.scroll(to: NSPoint(x: clip.bounds.origin.x, y: y + CGFloat(shift) * step))
            table.enclosingScrollView?.reflectScrolledClipView(clip)
        }
    }
}

/// A list held in memory. Updates touch only what changed: the same rows
/// redraw what is on screen, a longer list with the same head appends.
final class ArrayFeed<Row: Identifiable>: RowFeed<Row> {
    private var rows: [Row] = []
    private var ids: [Row.ID] = []

    override var count: Int { rows.count }
    override func row(_ i: Int) -> Row? { i < rows.count ? rows[i] : nil }
    override func index(of id: Row.ID) -> Int? { ids.firstIndex(of: id) }

    func update(_ new: [Row], force: Bool = false) {
        let newIDs = new.map(\.id)
        let old = ids
        rows = new
        ids = newIDs
        guard let table else { return }
        if force {
            table.reloadData()
        } else if newIDs == old {
            refresh(0..<rows.count)
        } else if newIDs.count > old.count, Array(newIDs.prefix(old.count)) == old {
            table.insertRows(at: IndexSet(integersIn: old.count..<newIDs.count), withAnimation: [])
        } else {
            table.reloadData()
        }
    }
}

/// The request ledger for the Details table, loaded around the viewport.
///
/// - Rows live in a sparse index → row store; `loaded` / `inflight` say
///   which indices are present or on their way.
/// - What is fetched: the visible rows plus a look-ahead in the scroll
///   direction that grows with speed (a flick reaches further), and a little
///   behind. Missing runs are fetched as chunks of at most one page, nearest
///   to the viewport first, three at a time; rows flown past are never asked
///   for.
/// - Memory stays bounded: rows far from the viewport are dropped once the
///   store passes `maxRows`.
/// - A refresh that found new requests (the ledger is newest-first) shifts
///   the store down by their number instead of throwing it away, and checks
///   the shifted rows against the fresh first page; any mismatch (filters,
///   deletions) falls back to a clean reload.
final class DetailFeed: RowFeed<EventRow> {
    static let pageSize = 200
    private let maxRows = 4000
    private let maxInflight = 3

    private var total = 0
    private var rows: [Int: EventRow] = [:]
    private var loaded = IndexSet()
    private var inflight = IndexSet()
    /// Bumped whenever indices stop meaning what they did: late fetches
    /// for the old numbering are dropped.
    private var generation = 0
    private var apps: [String]?
    private var models: [String]?
    private var window = 0..<0
    private var velocity = 0.0

    override var count: Int { total }
    override func row(_ i: Int) -> EventRow? { rows[i] }

    /// Index of a loaded row by id (selection survives refreshes).
    override func index(of id: EventRow.ID) -> Int? {
        rows.first { $0.value.id == id }?.key
    }

    /// New data from a load: the total, the first page as it is now, and
    /// the filters it was read with.
    /// `restart`: the range or filters changed — start over at the top.
    func reset(total newTotal: Int, first: [EventRow], apps newApps: [String]?, models newModels: [String]?,
               restart: Bool = false) {
        let sameQuery = !restart && newApps == apps && newModels == models
        let added = newTotal - total
        apps = newApps
        models = newModels
        if sameQuery, added >= 0, total > 0, shiftKeeps(added, first: first) {
            if added == 0 {
                // Nothing new: only values (costs re-priced) may differ.
                place(first, at: 0)
                refresh(0..<first.count)
                return
            }
            generation += 1
            inflight = IndexSet()
            var shifted: [Int: EventRow] = [:]
            shifted.reserveCapacity(rows.count + first.count)
            for (i, r) in rows where i + added < newTotal { shifted[i + added] = r }
            rows = shifted
            loaded.shift(startingAt: 0, by: added)
            loaded.remove(integersIn: newTotal..<Int.max)
            total = newTotal
            place(first, at: 0)
            reloadAll(insertedAbove: added)
        } else {
            generation += 1
            rows = [:]
            loaded = IndexSet()
            inflight = IndexSet()
            total = newTotal
            place(first, at: 0)
            if let table {
                table.reloadData()
                if !sameQuery { table.scrollRowToVisible(0) }
            }
        }
        schedule()
    }

    override func visible(_ range: Range<Int>, velocity v: Double) {
        window = range
        velocity = v
        schedule()
    }

    /// After shifting by `added`, would the rows we hold line up with the
    /// fresh first page? (Only checkable where the two overlap.)
    private func shiftKeeps(_ added: Int, first: [EventRow]) -> Bool {
        for (i, r) in first.enumerated() where i >= added {
            if let held = rows[i - added], held.id != r.id { return false }
        }
        return true
    }

    private func place(_ page: [EventRow], at start: Int) {
        for (k, r) in page.enumerated() { rows[start + k] = r }
        if !page.isEmpty { loaded.insert(integersIn: start..<(start + page.count)) }
    }

    /// Rows that should be present now, nearest to the viewport first.
    private func wanted() -> [Range<Int>] {
        guard total > 0 else { return [] }
        let size = Self.pageSize
        let lo = max(0, window.lowerBound), hi = min(total, max(window.upperBound, lo + 1))
        // Look-ahead grows with speed: one page, plus one per 1,500 rows/s.
        let ahead = size * (1 + min(3, Int(abs(velocity) / 1500)))
        let behind = size / 2
        let down = velocity >= 0
        let start = max(0, lo - (down ? behind : ahead))
        let end = min(total, hi + (down ? ahead : behind))
        var missing = IndexSet(integersIn: start..<end)
        missing.subtract(loaded)
        missing.subtract(inflight)
        // Chunks of at most a page, ordered by distance from the viewport.
        var chunks: [Range<Int>] = []
        for run in missing.rangeView {
            var a = run.lowerBound
            while a < run.upperBound {
                let b = min(run.upperBound, a + size)
                chunks.append(a..<b)
                a = b
            }
        }
        let centre = (lo + hi) / 2
        return chunks.sorted { abs(($0.lowerBound + $0.upperBound) / 2 - centre) < abs(($1.lowerBound + $1.upperBound) / 2 - centre) }
    }

    private var running = 0

    private func schedule() {
        for chunk in wanted() where running < maxInflight {
            fetch(chunk)
        }
    }

    private func fetch(_ chunk: Range<Int>) {
        running += 1
        inflight.insert(integersIn: chunk)
        let gen = generation
        let req = PageRequest(offset: Int64(chunk.lowerBound), limit: Int64(chunk.count), apps: apps, models: models)
        Task { [weak self] in
            let page = try? await Core.detailPage(req)
            guard let self else { return }
            self.running -= 1
            guard gen == self.generation else { self.schedule(); return }
            self.inflight.remove(integersIn: chunk)
            if let got = page?.rows, !got.isEmpty {
                self.place(got, at: chunk.lowerBound)
                self.evict()
                self.refresh(chunk.lowerBound..<(chunk.lowerBound + got.count))
            }
            self.schedule()
        }
    }

    /// Drop the rows farthest from the viewport once over budget.
    private func evict() {
        guard rows.count > maxRows else { return }
        let centre = (window.lowerBound + window.upperBound) / 2
        let keep = maxRows * 3 / 4
        let far = rows.keys.sorted { abs($0 - centre) > abs($1 - centre) }.prefix(rows.count - keep)
        for i in far {
            rows[i] = nil
            loaded.remove(i)
        }
    }
}
