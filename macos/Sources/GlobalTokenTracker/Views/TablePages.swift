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
        NativeTable(feed: model.detailFeed, columns: Self.columns, selection: $selection, menu: menu)
        .overlay {
            if model.detailEmpty && model.vm != nil {
                EmptyState(title: "No Requests", symbol: "tray",
                           message: "Nothing matches the current tool and model filters.")
            }
        }
        .safeAreaInset(edge: .bottom) {
            HStack {
                Text("\(Fmt.count(model.detailTotal)) requests")
                if model.vm?.hasPeers == true {
                    // Peers share hourly totals, not events.
                    Text("This Mac's events only").foregroundStyle(.tertiary)
                }
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

    private static let columns: [NativeColumn<EventRow>] = [
        NativeColumn(id: "time", title: "Time", min: 84, ideal: 104, mono: true) { r in
            NativeCell(text: RowText.of(r).time, tooltip: RowText.of(r).timeLong)
        },
        NativeColumn(id: "tool", title: "Tool", min: 56, ideal: 76) { r in NativeCell(text: Fmt.app(r.app)) },
        // Windows: the raw model, else the pricing model, else "—".
        NativeColumn(id: "model", title: "Model", min: 100, ideal: 170) { r in
            NativeCell(text: r.model ?? r.pricingModel ?? "—", tone: .secondary,
                       tooltip: r.pricingModel.map { String(localized: "Priced as \($0)") })
        },
        NativeColumn(id: "project", title: "Project", min: 76, ideal: 140) { r in
            NativeCell(text: r.project.map { URL(fileURLWithPath: $0).lastPathComponent } ?? "—",
                       tooltip: r.project, truncation: .byTruncatingMiddle)
        },
        NativeColumn(id: "input", title: "Input", min: 44, ideal: 64, trailing: true, mono: true) { r in num(r.inputTokens) },
        NativeColumn(id: "output", title: "Output", min: 52, ideal: 72, trailing: true, mono: true) { r in num(r.outputTokens) },
        NativeColumn(id: "cache", title: "Cache", min: 70, ideal: 92, trailing: true, mono: true) { r in
            var c = num(r.cacheReadTokens + r.cacheWriteTokens, secondary: true)
            c.tooltip = String(localized: "Read \(Fmt.tokensExact(r.cacheReadTokens)) · Write \(Fmt.tokensExact(r.cacheWriteTokens))")
            return c
        },
        NativeColumn(id: "cost", title: "Cost", min: 72, ideal: 104, trailing: true, mono: true) { r in costCell(r) },
        NativeColumn(id: "duration", title: "Duration", min: 58, ideal: 74, trailing: true, mono: true) { r in
            NativeCell(text: r.durationMs.map { Fmt.duration(ms: UInt64(max(0, $0))) } ?? "—")
        },
        NativeColumn(id: "speed", title: "Speed", min: 64, ideal: 84, trailing: true, mono: true) { r in speedCell(r) },
    ]

    private static func num(_ n: UInt64, secondary: Bool = false) -> NativeCell {
        NativeCell(text: n == 0 ? "—" : Fmt.tokensExact(n),
                   tone: n == 0 ? .tertiary : secondary ? .secondary : .primary)
    }

    /// Cost, same rules as the WinUI `event_row`: USD, else credits ("12cr");
    /// on unpriced rows credits lead (the stored $0 carries no information);
    /// "≈" estimated, "↺" provider-reported; unclassified tokens replace the
    /// figure; an "unpriced" badge when no source knows the model.
    private static func costCell(_ row: EventRow) -> NativeCell {
        var s: String
        if row.unclassifiedTokens > 0 {
            s = String(localized: "Unclassified · \(Fmt.tokensExact(row.unclassifiedTokens)) tokens")
        } else {
            let creditsText = row.credits.filter { $0 > 0 }.map {
                "\($0.formatted(.number.precision(.fractionLength(0...1))))cr"
            }
            if row.costSource == "unpriced" {
                s = creditsText ?? row.costUsd.map(Fmt.usd) ?? "—"
            } else {
                s = row.costUsd.map(Fmt.usd) ?? creditsText ?? "—"
            }
            switch row.costSource {
            case "estimated": s += " ≈"
            case "provider_reported": s += " ↺"
            default: break
            }
        }
        return NativeCell(text: s, tooltip: row.costSource,
                          badge: row.costSource == "unpriced" ? ("unpriced", .systemOrange) : nil)
    }

    /// Output tokens/s over the call's effective timing ("≈" when the span
    /// is a timestamp estimate, no usable timing shows "—"); the tooltip
    /// carries the total/in/out/prefill/gen breakdown — same parts as the
    /// WinUI speed tooltip.
    private static func speedCell(_ r: EventRow) -> NativeCell {
        guard let (tps, est) = r.outputTps else { return NativeCell(text: "—", tone: .tertiary) }
        var parts: [String] = []
        if let (v, _) = r.totalTps { parts.append(String(localized: "Total \(Fmt.speed(v))")) }
        if let (v, _) = r.inputTps { parts.append(String(localized: "In \(Fmt.speed(v))")) }
        if let (v, _) = r.outputTps { parts.append(String(localized: "Out \(Fmt.speed(v))")) }
        if let v = r.prefillTps { parts.append(String(localized: "Prefill \(Fmt.speed(v))")) }
        if let (v, _) = r.decodeTps { parts.append(String(localized: "Gen \(Fmt.speed(v))")) }
        if let (ms, _) = r.effectiveMs { parts.append(String(localized: "Timed \(Fmt.duration(ms: ms))")) }
        if est { parts.append(String(localized: "estimated")) }
        return NativeCell(text: "\(est ? "≈" : "")\(Fmt.speed(tps))",
                          tooltip: parts.joined(separator: " · "))
    }

    /// Provenance (raw_ref: source file + offset) — the Windows row tooltip,
    /// here as a context menu so it can be copied for reconciliation.
    private func menu(_ r: EventRow) -> [NativeMenuItem] {
        var items: [NativeMenuItem] = []
        if let ref = r.rawRef {
            items.append(NativeMenuItem(title: ref))
            items.append(NativeMenuItem(title: String(localized: "Copy Source Reference")) { copy(ref) })
        }
        if let sid = r.sessionId {
            items.append(NativeMenuItem(title: String(localized: "Copy Session ID")) { copy(sid) })
        }
        if let p = r.project {
            items.append(NativeMenuItem(title: String(localized: "Reveal Project in Finder"),
                                        enabled: FileManager.default.fileExists(atPath: p)) {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: p)])
            })
        }
        return items
    }

    private func copy(_ s: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
    }
}

// MARK: - Quota

struct QuotaView: View {
    @Environment(AppModel.self) private var model

    /// Quotas whose freshest report came from another device.
    private var fromOtherDevices: [MergedQuota] {
        (model.vm?.quotasAllDevices ?? []).filter { !$0.local }
    }
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
                    if !fromOtherDevices.isEmpty {
                        Section {
                            ForEach(fromOtherDevices) { q in
                                VStack(alignment: .leading, spacing: 2) {
                                    QuotaMeter(row: q.row, showApp: true)
                                    Text("Reported by \(q.deviceName)").scaledFont(.caption2).foregroundStyle(.secondary)
                                }
                                .padding(.vertical, 4)
                            }
                        } header: {
                            Text("From Other Devices")
                        } footer: {
                            Text("The freshest report per account wins; quotas are never added up across devices.")
                                .scaledFont(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
                .formStyle(.grouped)
            } else if !fromOtherDevices.isEmpty {
                Form {
                    Section("From Other Devices") {
                        ForEach(fromOtherDevices) { q in
                            VStack(alignment: .leading, spacing: 2) {
                                QuotaMeter(row: q.row, showApp: true)
                                Text("Reported by \(q.deviceName)").scaledFont(.caption2).foregroundStyle(.secondary)
                            }
                            .padding(.vertical, 4)
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
        NativeTable(rows: rows, columns: columns, menu: { p in
            [NativeMenuItem(title: String(localized: "Copy Model Name")) {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(p.model, forType: .string)
            }]
        })
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

    private var columns: [NativeColumn<PriceRow>] {
        let price = { (v: Double) in NativeCell(text: v > 0 ? Fmt.perMillion(v) : "—") }
        return [
            NativeColumn(id: "model", title: "Model", min: 160, ideal: 240) { p in
                NativeCell(text: p.model)
            },
            NativeColumn(id: "input", title: "Input", min: 70, ideal: 90, trailing: true, mono: true) { price($0.input) },
            NativeColumn(id: "output", title: "Output", min: 70, ideal: 90, trailing: true, mono: true) { price($0.output) },
            NativeColumn(id: "cacheRead", title: "Cache Read", min: 80, ideal: 100, trailing: true, mono: true) { price($0.cacheRead) },
            NativeColumn(id: "cacheWrite", title: "Cache Write", min: 80, ideal: 100, trailing: true, mono: true) { price($0.cacheWrite) },
            NativeColumn(id: "source", title: "Source", min: 80, ideal: 110) { p in NativeCell(text: p.source, tone: .secondary) },
            NativeColumn(id: "agreement", title: "Agreement", min: 70, ideal: 90) { p in
                NativeCell(text: "", tooltip: Self.tooltip(p),
                           badge: ("\(p.agree)/\(p.total)", p.disputed ? .systemOrange : .secondaryLabelColor))
            },
        ]
    }

    /// The WinUI corroboration tooltip: the adopted price, who it's from and
    /// how many agree, then every source's quote with its stance and alias.
    private static func tooltip(_ p: PriceRow) -> String {
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
