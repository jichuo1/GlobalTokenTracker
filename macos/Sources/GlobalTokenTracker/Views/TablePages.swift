// Table pages: Details, Quota, Sources, Prices. SwiftUI `Table` is backed by
// NSTableView, so rows are virtualized — only visible rows are built, which
// is what keeps a 60k-event ledger scrolling smoothly. Cell content follows
// the WinUI tables (pages.rs) column for column.
import AppKit
import SwiftUI

// MARK: - Details

struct DetailView: View {
    @Environment(AppModel.self) private var model
    @ViewState private var selection: EventRow.ID?

    var body: some View {
        Table(model.detailRows, selection: $selection) {
            TableColumn("Time") { r in
                Text(RowText.of(r).time).monospacedDigit()
                    .help(RowText.of(r).timeLong)
                    // Infinite scroll: the last visible row asks for the next page.
                    // Deferred a run-loop turn — mutating the rows while
                    // NSTableView is laying out cells is a reentrant delegate
                    // operation (AppKit warns now, will assert later).
                    // Prefetch: ask for the next page while ~60 rows remain, so
                    // it has arrived by the time the scroll reaches the end
                    // (fetching at the last row hitched mid-scroll).
                    .onAppear {
                        guard model.isNearDetailEnd(r.id, within: 60) else { return }
                        Task { @MainActor in model.loadMoreDetail() }
                    }
            }
            // Widths sum to fit a normal window, so the table never needs a
            // horizontal scroller (it covered the last rows and the Cost /
            // Duration columns ended up off-screen).
            .width(min: 84, ideal: 104)
            TableColumn("Tool") { r in Text(Fmt.app(r.app)) }
                .width(min: 56, ideal: 76)
            TableColumn("Model") { r in
                // Windows: the raw model, else the pricing model, else "—".
                Text(r.model ?? r.pricingModel ?? "—")
                    .foregroundStyle(.secondary)
                    .help(r.pricingModel.map { String(localized: "Priced as \($0)") } ?? "")
            }
            .width(min: 100, ideal: 170)
            TableColumn("Project") { r in
                Text(r.project.map { URL(fileURLWithPath: $0).lastPathComponent } ?? "—")
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(r.project ?? "")
            }
            .width(min: 76, ideal: 140)
            TableColumn("Input") { r in num(r.inputTokens) }.width(min: 44, ideal: 64)
            TableColumn("Output") { r in num(r.outputTokens) }.width(min: 52, ideal: 72)
            TableColumn("Cache") { r in
                num(r.cacheReadTokens + r.cacheWriteTokens, secondary: true)
                    .help(String(localized: "Read \(Fmt.tokensExact(r.cacheReadTokens)) · Write \(Fmt.tokensExact(r.cacheWriteTokens))"))
            }
            .width(min: 70, ideal: 92)
            TableColumn("Cost") { r in CostCell(row: r) }
                .width(min: 72, ideal: 104)
            TableColumn("Duration") { r in
                Text(r.durationMs.map { Fmt.duration(ms: UInt64(max(0, $0))) } ?? "—")
                    .monospacedDigit()
                    .frame(maxWidth: .infinity, alignment: .trailing)
                    // Keep clear of the overlay scroller's lane.
                    .padding(.trailing, ScrollerStyle.lane)
            }
            .width(min: 58, ideal: 74)
        }
        // Provenance (raw_ref: source file + offset) — the Windows row tooltip,
        // here as a context menu so it can be copied for reconciliation.
        .contextMenu(forSelectionType: EventRow.ID.self) { ids in
            if let id = ids.first, let r = model.detailRows.first(where: { $0.id == id }) {
                if let ref = r.rawRef {
                    Text(ref)
                    Button("Copy Source Reference") { copy(ref) }
                }
                if let sid = r.sessionId {
                    Button("Copy Session ID") { copy(sid) }
                }
                if let p = r.project {
                    Button("Reveal Project in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: p)])
                    }
                    .disabled(!FileManager.default.fileExists(atPath: p))
                }
            }
        }
        .overlay {
            if model.detailRows.isEmpty && model.vm != nil {
                EmptyState(title: "No Requests", symbol: "tray",
                           message: "Nothing matches the current tool and model filters.")
            }
        }
        .safeAreaInset(edge: .bottom) {
            HStack {
                Text("Showing \(model.detailRows.count) of \(Fmt.count(model.detailTotal))")
                if model.detailLoadingMore { ProgressView().controlSize(.small) }
                Spacer()
                Text("≈ estimated · ↺ provider-reported").foregroundStyle(.tertiary)
            }
            .scaledFont(.caption)
            .foregroundStyle(.secondary)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .background(.bar)
            .cascade(1)
        }
    }

    private func num(_ n: UInt64, secondary: Bool = false) -> some View {
        Text(n == 0 ? "—" : Fmt.tokensExact(n))
            .monospacedDigit()
            .foregroundStyle(n == 0 ? AnyShapeStyle(.tertiary) : secondary ? AnyShapeStyle(.secondary) : AnyShapeStyle(.primary))
            .frame(maxWidth: .infinity, alignment: .trailing)
    }

    private func copy(_ s: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
    }
}

/// Cost cell, same rules as the WinUI `event_row`: USD, else credits ("12cr");
/// "≈" estimated, "↺" provider-reported; unclassified tokens replace the
/// figure; an "unpriced" pill when no source knows the model.
private struct CostCell: View {
    let row: EventRow

    private var text: String {
        if row.unclassifiedTokens > 0 {
            return String(localized: "Unclassified · \(Fmt.tokensExact(row.unclassifiedTokens)) tokens")
        }
        var s = row.costUsd.map(Fmt.usd)
            ?? row.credits.map { "\($0.formatted(.number.precision(.fractionLength(0...1))))cr" }
            ?? "—"
        switch row.costSource {
        case "estimated": s += " ≈"
        case "provider_reported": s += " ↺"
        default: break
        }
        return s
    }

    var body: some View {
        HStack(spacing: 6) {
            Spacer(minLength: 0)
            if row.costSource == "unpriced" {
                Pill(text: "unpriced", tint: .orange)
            }
            Text(text).monospacedDigit().lineLimit(1)
        }
        .help(row.costSource ?? "")
    }
}

// MARK: - Quota

struct QuotaView: View {
    @Environment(AppModel.self) private var model
    /// Folded tool groups (Windows: ToggleQuotaGroup), kept for the session.
    @ViewState private var collapsed: Set<String> = []

    var body: some View {
        Group {
            if let groups = model.vm?.quotaGroups, !groups.isEmpty {
                Form {
                    ForEach(Array(groups.enumerated()), id: \.element.id) { gi, g in
                        Section(isExpanded: Binding(
                            get: { !collapsed.contains(g.app) },
                            set: { open in
                                if open { collapsed.remove(g.app) } else { collapsed.insert(g.app) }
                            }
                        )) {
                            ForEach(Array(g.rows.enumerated()), id: \.element.id) { ri, q in
                                QuotaMeter(row: q).padding(.vertical, 4)
                                    .cascade(gi * 2 + ri)
                            }
                        } header: {
                            HStack {
                                Text("\(g.display) · \(g.rows.count) quotas")
                                Spacer()
                                if let w = g.worstPct {
                                    Pill(text: Fmt.percent(w), tint: QuotaMeter.tint(w))
                                }
                            }
                        }
                    }
                }
                .formStyle(.grouped)
            } else {
                EmptyState(title: "No Quota Data", symbol: "gauge.with.dots.needle.0percent",
                           message: "Quota windows appear here once Codex or Cursor credentials are found on this Mac.")
            }
        }
    }
}

// MARK: - Sources

struct SourcesView: View {
    @Environment(AppModel.self) private var model

    private struct Row: Identifiable {
        var id: String
        var name: String
        var health: SourceHealth?
    }

    private var rows: [Row] {
        guard let s = model.sources else { return [] }
        let byId = Dictionary(s.health.map { ($0.source, $0) }, uniquingKeysWith: { a, _ in a })
        return s.adapters.map { Row(id: $0.id, name: $0.name, health: byId[$0.id]) }
    }

    var body: some View {
        Table(rows) {
            TableColumn("Source") { r in
                Label {
                    Text(r.name)
                } icon: {
                    Image(systemName: icon(r)).foregroundStyle(color(r))
                }
            }
            .width(min: 140, ideal: 180)
            TableColumn("Files") { r in
                Text(r.health.map { Fmt.count($0.filesSeen) } ?? "—").monospacedDigit()
            }
            .width(min: 50, ideal: 70)
            TableColumn("Rows Ingested") { r in
                Text(r.health.map { Fmt.count($0.rowsIngested) } ?? "—").monospacedDigit()
            }
            .width(min: 80, ideal: 110)
            TableColumn("Last Sync") { r in Text(Fmt.relative(r.health?.lastSyncedAt)) }
                .width(min: 90, ideal: 120)
            TableColumn("Status") { r in
                if let err = r.health?.lastError {
                    Pill(text: "⚠ \(err)", tint: .red).help(err)
                } else if r.health == nil {
                    Text("No data yet").foregroundStyle(.secondary)
                } else {
                    Text("OK").foregroundStyle(.secondary)
                }
            }
        }
        .overlay { if model.sources == nil { ProgressView() } }
        // After the page turn settles: new rows landing mid-transition make
        // NSTableView re-lay out under the 3D transform and drop frames.
        .task {
            try? await Task.sleep(for: .milliseconds(Motion.settleMs))
            model.loadSources()
        }
        .background(OnArrival(page: .sources) { model.loadSources() })
    }

    private func icon(_ r: Row) -> String {
        guard let h = r.health else { return "circle.dashed" }
        return h.lastError != nil ? "exclamationmark.triangle.fill" : "checkmark.circle.fill"
    }

    private func color(_ r: Row) -> Color {
        guard let h = r.health else { return .secondary }
        return h.lastError != nil ? .orange : .green
    }
}

// MARK: - Prices

struct PricesView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        let rows = model.prices?.rows ?? []
        Table(rows) {
            TableColumn("Model") { p in Text(p.model).textSelection(.enabled) }
                .width(min: 160, ideal: 240)
            TableColumn("Input") { p in price(p.input) }.width(min: 70, ideal: 90)
            TableColumn("Output") { p in price(p.output) }.width(min: 70, ideal: 90)
            TableColumn("Cache Read") { p in price(p.cacheRead) }.width(min: 80, ideal: 100)
            TableColumn("Cache Write") { p in price(p.cacheWrite) }.width(min: 80, ideal: 100)
            TableColumn("Source") { p in Text(p.source).foregroundStyle(.secondary) }
                .width(min: 80, ideal: 110)
            TableColumn("Agreement") { p in
                Pill(text: "\(p.agree)/\(p.total)", tint: p.disputed ? .orange : .secondary)
                    .help(tooltip(p))
                    .padding(.trailing, ScrollerStyle.lane)
            }
            .width(min: 70, ideal: 90)
        }
        .overlay {
            if model.prices != nil && rows.isEmpty {
                EmptyState(title: "No Matching Models", symbol: "magnifyingglass")
            }
        }
        .safeAreaInset(edge: .bottom) { footer(shown: rows.count).cascade(1) }
        .task {
            try? await Task.sleep(for: .milliseconds(Motion.settleMs))
            model.loadPrices(reload: true)
        }
        .background(OnArrival(page: .prices) { model.loadPrices(reload: true) })
    }

    private func footer(shown: Int) -> some View {
        let total = model.prices?.total ?? 0
        let filtering = !model.priceQuery.trimmingCharacters(in: .whitespaces).isEmpty || model.priceDisputedOnly
        let sync: String = model.prices?.syncedAt.map {
            String(localized: "live sources synced \(Fmt.relative($0))")
        } ?? String(localized: "local seed only, not synced yet")
        return HStack(spacing: 8) {
            if model.isRefreshingPrices {
                ProgressView().controlSize(.small)
                Text("Syncing…")
            } else {
                Text(filtering
                     ? String(localized: "Matching \(shown) / \(total) models · \(sync)")
                     : String(localized: "\(total) models · \(sync)"))
                if let err = model.priceError {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    Text("Last refresh failed: \(err.components(separatedBy: "\n").first ?? err)")
                        .lineLimit(1).help(err)
                }
            }
            Spacer()
        }
        .scaledFont(.caption)
        .foregroundStyle(.secondary)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.bar)
    }

    private func price(_ v: Double) -> some View {
        Text(v > 0 ? Fmt.perMillion(v) : "—")
            .monospacedDigit()
            .frame(maxWidth: .infinity, alignment: .trailing)
    }

    /// The WinUI corroboration tooltip: the adopted price, who it's from and
    /// how many agree, then every source's quote with its stance and alias.
    private func tooltip(_ p: PriceRow) -> String {
        let money = { (i: Double, o: Double) in "\(Fmt.perMillion(i)) / \(Fmt.perMillion(o))" }
        var head = money(p.input, p.output)
        if p.cacheRead > 0 { head += " · " + String(localized: "cache read \(Fmt.perMillion(p.cacheRead))") }
        if p.cacheWrite > 0 { head += " · " + String(localized: "cache write \(Fmt.perMillion(p.cacheWrite))") }
        var lines = [head, String(localized: "Adopted \(p.source) (\(p.agree)/\(p.total) agree)")]
        for q in p.quotes {
            let (mark, detail): (String, String) = switch q.stance {
            case "agrees": ("✓", money(q.input, q.output))
            case "dissents": ("✗", money(q.input, q.output))
            case "no_data": ("–", String(localized: "no quote"))
            default: ("·", String(localized: "\(money(q.input, q.output)) (not counted)"))
            }
            let alias = q.key.caseInsensitiveCompare(p.model) == .orderedSame ? "" : "  [\(q.key)]"
            lines.append("\(mark) \(q.source)  \(detail)\(alias)")
        }
        return lines.joined(separator: "\n")
    }
}

/// Runs `action` each time the deck turns *to* `page`, once the turn has
/// settled. A separate zero-size view so that only it depends on
/// `model.page`: when the page's own body read it, every page change
/// re-evaluated (and re-diffed) its table — 5,000 rows on Prices — at the
/// start of every turn, even turns that didn't involve that page.
private struct OnArrival: View {
    let page: Page
    let action: () -> Void
    @Environment(AppModel.self) private var model

    var body: some View {
        Color.clear
            .frame(width: 0, height: 0)
            .task(id: model.page == page) {
                guard model.page == page else { return }
                try? await Task.sleep(for: .milliseconds(Motion.settleMs))
                if !Task.isCancelled { action() }
            }
    }
}

/// Formatted strings for one detail row, built once and cached: the table
/// re-evaluates visible cells on every update and while scrolling, and date
/// formatting was redone for each of them each time.
final class RowText: NSObject, @unchecked Sendable {
    let time: String
    let timeLong: String

    private init(_ r: EventRow) {
        time = Fmt.shortTimestamp(r.tsStart)
        timeLong = Fmt.timestamp(r.tsStart)
    }

    // NSCache is internally synchronized (thread-safe by documentation).
    nonisolated(unsafe) private static let cache: NSCache<NSString, RowText> = {
        let c = NSCache<NSString, RowText>()
        c.countLimit = 6000
        return c
    }()

    static func of(_ r: EventRow) -> RowText {
        let key = r.id as NSString
        if let hit = cache.object(forKey: key) { return hit }
        let made = RowText(r)
        cache.setObject(made, forKey: key)
        return made
    }
}
