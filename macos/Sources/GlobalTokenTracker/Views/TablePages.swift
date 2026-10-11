// Table pages: Details, Quota, Sources, Prices. SwiftUI `Table` is backed by
// NSTableView, so rows are virtualized — only visible rows are built, which
// is what keeps a 60k-event ledger scrolling smoothly.
import SwiftUI

// MARK: - Details

struct DetailView: View {
    @Environment(AppModel.self) private var model
    @ViewState private var selection: EventRow.ID?

    var body: some View {
        Table(model.detailRows, selection: $selection) {
            TableColumn("Time") { r in
                Text(Fmt.timestamp(r.tsStart)).monospacedDigit()
                    // Infinite scroll: the last visible row asks for the next page.
                    // Deferred a run-loop turn — mutating the rows while
                    // NSTableView is laying out cells is a reentrant delegate
                    // operation (AppKit warns now, will assert later).
                    .onAppear {
                        guard r.id == model.detailRows.last?.id else { return }
                        Task { @MainActor in model.loadMoreDetail() }
                    }
            }
            .width(min: 120, ideal: 150)
            TableColumn("Tool") { r in Text(Fmt.app(r.app)) }
                .width(min: 70, ideal: 100)
            TableColumn("Model") { r in
                Text(r.model ?? "—").help(r.pricingModel.map { "Priced as \($0)" } ?? "")
            }
            .width(min: 120, ideal: 200)
            TableColumn("Project") { r in
                Text(r.project.map { URL(fileURLWithPath: $0).lastPathComponent } ?? "—")
                    .help(r.project ?? "")
            }
            .width(min: 80, ideal: 140)
            TableColumn("Input") { r in num(r.inputTokens) }.width(min: 60, ideal: 80)
            TableColumn("Output") { r in num(r.outputTokens) }.width(min: 60, ideal: 80)
            TableColumn("Cache Read") { r in num(r.cacheReadTokens) }.width(min: 60, ideal: 90)
            TableColumn("Cache Write") { r in num(r.cacheWriteTokens) }.width(min: 60, ideal: 90)
            TableColumn("Cost") { r in
                Text(r.costUsd.map(Fmt.usd) ?? "—")
                    .monospacedDigit()
                    .frame(maxWidth: .infinity, alignment: .trailing)
                    .help(r.costSource ?? "")
            }
            .width(min: 60, ideal: 80)
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
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .background(.bar)
        }
        .navigationTitle("Details")
    }

    private func num(_ n: UInt64) -> some View {
        Text(n == 0 ? "—" : Fmt.tokensExact(n))
            .monospacedDigit()
            .foregroundStyle(n == 0 ? .tertiary : .primary)
            .frame(maxWidth: .infinity, alignment: .trailing)
    }
}

// MARK: - Quota

struct QuotaView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if let groups = model.vm?.quotaGroups, !groups.isEmpty {
                Form {
                    ForEach(groups) { g in
                        Section {
                            ForEach(g.rows) { q in
                                QuotaMeter(row: q)
                                    .padding(.vertical, 4)
                                    .help("Captured \(Fmt.timestamp(q.capturedAt))")
                            }
                        } header: {
                            HStack {
                                Text(g.display)
                                Spacer()
                                if let w = g.worstPct {
                                    Text(Fmt.percent(w)).monospacedDigit().foregroundStyle(.secondary)
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
        .navigationTitle("Quota")
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
                    Text(err).foregroundStyle(.red).lineLimit(1).help(err)
                } else if r.health == nil {
                    Text("Not found on this Mac").foregroundStyle(.secondary)
                } else {
                    Text("OK").foregroundStyle(.secondary)
                }
            }
        }
        .overlay { if model.sources == nil { ProgressView() } }
        .task { model.loadSources() }
        .navigationTitle("Sources")
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
    @ViewState private var query = ""

    private func filtered() -> [PriceRow] {
        let all = model.prices?.rows ?? []
        let q = query.lowercased().filter { $0.isLetter || $0.isNumber }
        guard !q.isEmpty else { return all }
        // Same matching idea as core's `hay`: alphanumerics only, any spelling.
        return all.filter { r in
            ([r.model] + r.quotes.map(\.key)).contains {
                $0.lowercased().filter { $0.isLetter || $0.isNumber }.contains(q)
            }
        }
    }

    var body: some View {
        // Filter once per render; the table and the footer share the result.
        let rows = filtered()
        Table(rows) {
            TableColumn("Model") { p in Text(p.model).textSelection(.enabled) }
                .width(min: 160, ideal: 240)
            TableColumn("Input / M") { p in price(p.input) }.width(min: 70, ideal: 90)
            TableColumn("Output / M") { p in price(p.output) }.width(min: 70, ideal: 90)
            TableColumn("Cache Read / M") { p in price(p.cacheRead) }.width(min: 80, ideal: 110)
            TableColumn("Cache Write / M") { p in price(p.cacheWrite) }.width(min: 80, ideal: 110)
            TableColumn("Source") { p in Text(p.source).foregroundStyle(.secondary) }
                .width(min: 80, ideal: 110)
            TableColumn("Agreement") { p in
                Label("\(p.agree)/\(p.total)",
                      systemImage: p.agree == p.total ? "checkmark.seal" : "exclamationmark.bubble")
                    .foregroundStyle(p.agree == p.total ? Color.secondary : Color.orange)
                    .help(tooltip(p))
            }
            .width(min: 70, ideal: 90)
        }
        .searchable(text: $query, placement: .toolbar, prompt: "Filter models")
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    model.refreshPrices()
                } label: {
                    Label("Update Prices", systemImage: "arrow.down.circle")
                }
                .disabled(model.isRefreshingPrices)
                .help("Download the latest prices from every source")
            }
        }
        .safeAreaInset(edge: .bottom) {
            HStack(spacing: 8) {
                if model.isRefreshingPrices {
                    ProgressView().controlSize(.small)
                    Text("Updating prices…")
                } else if let err = model.priceError {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    Text(err.components(separatedBy: "\n").first ?? err).lineLimit(1).help(err)
                } else {
                    Text("\(rows.count) models · live sources synced \(Fmt.relative(model.prices?.syncedAt))")
                }
                Spacer()
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .background(.bar)
        }
        .task { model.loadPrices() }
        .navigationTitle("Prices")
    }

    private func price(_ v: Double) -> some View {
        Text(v > 0 ? Fmt.perMillion(v) : "—")
            .monospacedDigit()
            .frame(maxWidth: .infinity, alignment: .trailing)
    }

    private func tooltip(_ p: PriceRow) -> String {
        p.quotes.map { q in
            let mark = switch q.stance {
            case "agrees": "✓"
            case "dissents": "✗"
            default: "·"
            }
            return "\(mark) \(q.source): in \(Fmt.perMillion(q.input)) · out \(Fmt.perMillion(q.output))"
        }
        .joined(separator: "\n")
    }
}
