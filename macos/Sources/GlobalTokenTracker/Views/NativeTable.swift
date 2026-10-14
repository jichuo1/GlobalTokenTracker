// A plain AppKit table for the long lists (Details, Prices).
//
// SwiftUI's Table hosts every cell in its own NSHostingView. Profiled while
// wheel-scrolling Details, ~80% of the main thread went to rows scrolling in
// and out: each cell's hosting view leaving the window (KVO teardown,
// responder and view-graph updates) and a new SwiftUI graph per cell coming
// in — 30 of 35 frames dropped, the worst 450 ms — plus a SwiftUI hit test
// of the window on every wheel event. Here cells are NSTextFields that
// NSTableView recycles itself: a row scrolling in is a few string and colour
// assignments.
import AppKit
import SwiftUI

/// What one cell shows.
struct NativeCell {
    enum Tone { case primary, secondary, tertiary }
    var text: String
    var tone: Tone = .primary
    var tooltip: String?
    /// A small capsule before the text ("unpriced", "2/2").
    var badge: (text: String, color: NSColor)?
    /// An SF Symbol before the text.
    var symbol: (name: String, color: NSColor)?
    var truncation: NSLineBreakMode = .byTruncatingTail
}

struct NativeColumn<Row> {
    var id: String
    /// Localization key of the header.
    var title: String
    var min: CGFloat
    var ideal: CGFloat
    /// Right-aligned (numbers).
    var trailing = false
    /// Tabular digits.
    var mono = false
    var cell: (Row) -> NativeCell
}

/// One context-menu entry.
struct NativeMenuItem {
    var title: String
    var enabled = true
    /// nil: an informational line (shown, not clickable).
    var action: (() -> Void)?
}

struct NativeTable<Row: Identifiable>: NSViewRepresentable where Row.ID: Hashable {
    /// An in-memory list, or…
    var rows: [Row]?
    /// …a feed that loads rows itself (Details).
    var feed: RowFeed<Row>?
    var columns: [NativeColumn<Row>]
    var selection: Binding<Row.ID?>?
    var menu: ((Row) -> [NativeMenuItem])?
    @Environment(\.textScale) private var scale

    init(rows: [Row], columns: [NativeColumn<Row>], selection: Binding<Row.ID?>? = nil,
         menu: ((Row) -> [NativeMenuItem])? = nil) {
        self.rows = rows
        self.columns = columns
        self.selection = selection
        self.menu = menu
    }

    init(feed: RowFeed<Row>, columns: [NativeColumn<Row>], selection: Binding<Row.ID?>? = nil,
         menu: ((Row) -> [NativeMenuItem])? = nil) {
        self.feed = feed
        self.columns = columns
        self.selection = selection
        self.menu = menu
    }

    func makeCoordinator() -> Coordinator { Coordinator(feed: feed ?? ArrayFeed<Row>()) }

    func makeNSView(context: Context) -> NSScrollView {
        let table = NSTableView()
        table.style = .inset
        table.usesAlternatingRowBackgroundColors = true
        table.usesAutomaticRowHeights = false
        table.allowsColumnReordering = true
        table.allowsColumnResizing = true
        table.allowsMultipleSelection = false
        table.allowsEmptySelection = true
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.intercellSpacing = NSSize(width: 17, height: 0)
        for (i, c) in columns.enumerated() {
            let col = NSTableColumn(identifier: .init(c.id))
            col.title = String(localized: String.LocalizationValue(c.title))
            col.minWidth = c.min
            col.width = c.ideal
            col.headerCell.alignment = c.trailing ? .right : .left
            col.resizingMask = [.autoresizingMask, .userResizingMask]
            if i == columns.count - 1 { col.resizingMask = .autoresizingMask }
            table.addTableColumn(col)
        }
        let coordinator = context.coordinator
        table.dataSource = coordinator
        table.delegate = coordinator
        let menu = NSMenu()
        menu.delegate = coordinator
        table.menu = menu
        coordinator.table = table
        coordinator.feed.attach(table)

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = false
        scroll.autohidesScrollers = true
        scroll.scrollerStyle = .overlay
        scroll.drawsBackground = false
        // SwiftUI already places the view below the title bar.
        scroll.automaticallyAdjustsContentInsets = false
        // Where the viewport is and how fast it moves steers the feed's loading.
        scroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(coordinator, selector: #selector(Coordinator.scrolled),
                                               name: NSView.boundsDidChangeNotification, object: scroll.contentView)
        coordinator.apply(self, force: true)
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.apply(self, force: false)
    }

    @MainActor
    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSMenuDelegate {
        weak var table: NSTableView?
        let feed: RowFeed<Row>
        private var parent: NativeTable?
        private var scale: CGFloat = 0
        private var font = NSFont.systemFont(ofSize: 13)
        private var monoFont = NSFont.monospacedDigitSystemFont(ofSize: 13, weight: .regular)
        private var badgeFont = NSFont.systemFont(ofSize: 10, weight: .medium)
        private var syncingSelection = false
        /// Viewport tracking for the feed: last first-visible row, when, and
        /// a smoothed speed in rows per second.
        private var lastTop = 0
        private var lastTime = CACurrentMediaTime()
        private var velocity = 0.0

        init(feed: RowFeed<Row>) { self.feed = feed }

        func apply(_ parent: NativeTable, force: Bool) {
            self.parent = parent
            guard let table else { return }
            let scaleChanged = parent.scale != scale
            if scaleChanged {
                scale = parent.scale
                let size = (Typography.defaultSize * scale).rounded()
                font = .systemFont(ofSize: size)
                monoFont = .monospacedDigitSystemFont(ofSize: size, weight: .regular)
                badgeFont = .systemFont(ofSize: (size * 0.78).rounded(), weight: .medium)
                table.rowHeight = (NSLayoutManager().defaultLineHeight(for: font) + 6).rounded()
            }
            if let rows = parent.rows, let array = feed as? ArrayFeed<Row> {
                array.update(rows, force: force || scaleChanged)
            } else if force || scaleChanged {
                table.reloadData()
            }
            syncSelection()
            if force { DispatchQueue.main.async { [weak self] in self?.scrolled() } }
        }

        private func syncSelection() {
            guard let table, let binding = parent?.selection else { return }
            let want = binding.wrappedValue.flatMap { feed.index(of: $0) }
            let have = table.selectedRow >= 0 ? table.selectedRow : nil
            guard want != have else { return }
            syncingSelection = true
            if let want { table.selectRowIndexes([want], byExtendingSelection: false) } else { table.deselectAll(nil) }
            syncingSelection = false
        }

        @objc func scrolled() {
            guard let table else { return }
            let vis = table.rows(in: table.visibleRect)
            let now = CACurrentMediaTime()
            let dt = now - lastTime
            if dt > 0.001 {
                let instant = Double(vis.location - lastTop) / dt
                velocity = dt > 0.25 ? instant : velocity * 0.6 + instant * 0.4
            }
            lastTop = vis.location
            lastTime = now
            feed.visible(vis.location..<(vis.location + max(vis.length, 1)), velocity: velocity)
        }

        // MARK: Data

        func numberOfRows(in tableView: NSTableView) -> Int { feed.count }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            guard let tableColumn, let parent,
                  let spec = parent.columns.first(where: { $0.id == tableColumn.identifier.rawValue })
            else { return nil }
            let isLast = tableColumn === tableView.tableColumns.last
            let view = (tableView.makeView(withIdentifier: tableColumn.identifier, owner: nil) as? NativeCellView)
                ?? NativeCellView(identifier: tableColumn.identifier, trailing: spec.trailing, lane: isLast)
            // Not loaded yet: a quiet placeholder until the feed refreshes the row.
            let cell = feed.row(row).map(spec.cell)
                ?? NativeCell(text: tableColumn === tableView.tableColumns.first ? "…" : "", tone: .tertiary)
            view.configure(cell, font: spec.mono ? monoFont : font, badgeFont: badgeFont)
            return view
        }

        func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
            tableView.makeView(withIdentifier: flatRowID, owner: nil) as? NSTableRowView
                ?? { let v = FlatRowView(); v.identifier = flatRowID; return v }()
        }


        func tableViewSelectionDidChange(_ notification: Notification) {
            guard !syncingSelection, let table, let binding = parent?.selection else { return }
            let row = table.selectedRow
            binding.wrappedValue = row >= 0 ? feed.row(row)?.id : nil
        }

        // MARK: Menu

        func menuNeedsUpdate(_ menu: NSMenu) {
            menu.removeAllItems()
            guard let table, let build = parent?.menu else { return }
            let row = table.clickedRow
            guard row >= 0, let r = feed.row(row) else { return }
            for item in build(r) {
                let m = ClosureMenuItem(title: item.title, action: item.action)
                m.isEnabled = item.enabled && item.action != nil
                menu.addItem(m)
            }
        }
    }
}

/// A menu item that runs a closure.
private final class ClosureMenuItem: NSMenuItem {
    private let run: (() -> Void)?

    init(title: String, action: (() -> Void)?) {
        run = action
        super.init(title: title, action: action == nil ? nil : #selector(fire), keyEquivalent: "")
        target = self
    }

    required init(coder: NSCoder) { fatalError("unused") }

    @objc private func fire() { run?() }
}

private let flatRowID = NSUserInterfaceItemIdentifier("row")

/// One table row as a single layer: its cells draw into it (no layer per
/// cell, label, badge and icon — a thousand-odd small layers per table that
/// the window server transformed one by one through every page turn), and
/// it repaints only when told to, never because a turn moved or scaled it.
private final class FlatRowView: NSTableRowView {
    override init(frame: NSRect) {
        super.init(frame: frame)
        canDrawSubviewsIntoLayer = true
        layerContentsRedrawPolicy = .onSetNeedsDisplay
    }

    required init?(coder: NSCoder) { fatalError("unused") }
}

/// Icon, badge and text drawn directly — no subviews, so the row can take
/// the whole cell into its own layer.
private final class NativeCellView: NSTableCellView {
    private var content = NativeCell(text: "")
    private var font = NSFont.systemFont(ofSize: 13)
    private var badgeFont = NSFont.systemFont(ofSize: 10, weight: .medium)
    private let trailing: Bool
    private let lane: CGFloat

    init(identifier: NSUserInterfaceItemIdentifier, trailing: Bool, lane: Bool) {
        self.trailing = trailing
        // The last column keeps clear of the overlay scroller's lane.
        self.lane = lane ? ScrollerStyle.lane : 0
        super.init(frame: .zero)
        self.identifier = identifier
        layerContentsRedrawPolicy = .onSetNeedsDisplay
    }

    required init?(coder: NSCoder) { fatalError("unused") }

    override var isFlipped: Bool { true }

    func configure(_ c: NativeCell, font: NSFont, badgeFont: NSFont) {
        content = c
        self.font = font
        self.badgeFont = badgeFont
        toolTip = c.tooltip
        needsDisplay = true
    }

    /// Selected rows (emphasized) draw their text in the selection's colour.
    override var backgroundStyle: NSView.BackgroundStyle {
        didSet { if backgroundStyle != oldValue { needsDisplay = true } }
    }

    override func draw(_ dirtyRect: NSRect) {
        let selected = backgroundStyle == .emphasized
        let tone: NSColor = switch content.tone {
        case .primary: .labelColor
        case .secondary: .secondaryLabelColor
        case .tertiary: .tertiaryLabelColor
        }
        let color = selected ? NSColor.alternateSelectedControlTextColor : tone
        let para = NSMutableParagraphStyle()
        para.lineBreakMode = content.truncation
        let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color, .paragraphStyle: para]
        let text = NSAttributedString(string: content.text, attributes: attrs)

        // Pieces left to right: icon, badge, text.
        let gap: CGFloat = 6
        let iconSize = content.symbol == nil ? 0 : font.pointSize + 2
        var badgeText: NSAttributedString?
        var badgeSize = NSSize.zero
        if let b = content.badge {
            let bt = NSAttributedString(string: b.text, attributes: [
                .font: badgeFont, .foregroundColor: selected ? NSColor.alternateSelectedControlTextColor : b.color,
            ])
            badgeText = bt
            let ts = bt.size()
            badgeSize = NSSize(width: ceil(ts.width) + 12, height: ceil(ts.height) + 2)
        }
        let fixed = (iconSize > 0 ? iconSize + gap : 0) + (badgeText != nil ? badgeSize.width + (content.text.isEmpty ? 0 : gap) : 0)
        let room = max(0, bounds.width - lane)
        let textWidth = min(ceil(text.size().width), max(0, room - fixed))
        let total = fixed + textWidth
        var x = trailing ? max(0, room - total) : 0
        let lineHeight = ceil(font.ascender - font.descender + font.leading)
        let midY = bounds.midY

        if let s = content.symbol,
           let img = NSImage(systemSymbolName: s.name, accessibilityDescription: nil)?
               .withSymbolConfiguration(.init(pointSize: font.pointSize, weight: .regular)
                   .applying(.init(paletteColors: [selected ? .alternateSelectedControlTextColor : s.color]))) {
            let r = NSRect(x: x, y: midY - iconSize / 2, width: iconSize, height: iconSize)
            img.draw(in: r.insetBy(dx: (iconSize - img.size.width) / 2, dy: (iconSize - img.size.height) / 2))
            x += iconSize + gap
        }
        if let bt = badgeText, let b = content.badge {
            let r = NSRect(x: x, y: midY - badgeSize.height / 2, width: badgeSize.width, height: badgeSize.height)
            (selected ? NSColor.white.withAlphaComponent(0.2) : b.color.withAlphaComponent(0.15)).setFill()
            NSBezierPath(roundedRect: r, xRadius: r.height / 2, yRadius: r.height / 2).fill()
            let ts = bt.size()
            bt.draw(at: NSPoint(x: r.midX - ts.width / 2, y: r.midY - ts.height / 2))
            x += badgeSize.width + gap
        }
        if !content.text.isEmpty, textWidth > 0 {
            text.draw(with: NSRect(x: x, y: midY - lineHeight / 2, width: textWidth, height: lineHeight),
                      options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine])
        }
    }
}
