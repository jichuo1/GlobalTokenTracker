
// Dedicated speed rankings. Ratios use core's summed numerators and timing;
// missing timing stays absent, and timestamp estimates remain visible.
import SwiftUI

private enum SpeedMetric: String, CaseIterable, Identifiable {
    case output, total, input, prefill, decode
    var id: String { rawValue }
    var title: LocalizedStringKey {
        switch self {case .output: "Output"; case .total: "Total throughput"; case .input: "Input"; case .prefill: "Prefill"; case .decode: "Decode"}
    }
    func value(_ row: ShareRow) -> Double? {
        switch self {case .output: row.outputTps; case .total: row.totalTps; case .input: row.inputTps; case .prefill: row.prefillTps; case .decode: row.decodeTps}
    }
    func estimated(_ row: ShareRow) -> Bool { self != .prefill && row.speedEstMs > 0 }
}

struct SpeedView: View {
    @Environment(AppModel.self) private var model
    @ViewState private var byTool = false
    @ViewState private var metric = SpeedMetric.output
    @ViewState private var page = 0

    private func rows(_ vm: OverviewVM) -> [ShareRow] { byTool ? vm.byAppSpeed : vm.byModel }
    private func ranked(_ vm: OverviewVM) -> [ShareRow] {
        rows(vm).filter { metric.value($0) != nil }.sorted {
            let a = metric.value($0) ?? 0, b = metric.value($1) ?? 0
            return a == b ? $0.name < $1.name : a > b
        }
    }
    private func name(_ row: ShareRow) -> String { byTool ? Fmt.app(row.name) : row.name }

    var body: some View {
        Group {
            if let vm = model.vm {
                let ranking = ranked(vm)
                let currentPage = min(page, max(0, ranking.count - 1) / 25)
                let start = currentPage * 25
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 16) {
                        Text("Speed rankings").scaledFont(.title2).fontWeight(.semibold)
                        summary(vm, ranking: ranking)
                        Card(title: "Ranking") {
                            Picker("Group", selection: $byTool) {
                                Text("Models").tag(false)
                                Text("Tools").tag(true)
                            }.pickerStyle(.segmented)
                            Picker("Rank by", selection: $metric) {
                                ForEach(SpeedMetric.allCases) { Text($0.title).tag($0) }
                            }.pickerStyle(.segmented)
                            Text("Rates are weighted by valid timing. Requests show the full filtered request count.")
                                .scaledFont(.caption).foregroundStyle(.secondary)
                            Text("25 per page. ≈ includes timestamp estimates; unreported metrics show —.")
                                .scaledFont(.caption).foregroundStyle(.secondary)
                        }
                        if ranking.isEmpty {
                            EmptyState(title: "No timing data for this ranking", symbol: "speedometer",
                                       message: "Try another metric or widen the current filters.")
                                .frame(maxWidth: .infinity, minHeight: 180)
                        } else {
                            HStack {
                                Text("\(start + 1)–\(min(start + 25, ranking.count)) of \(ranking.count)")
                                    .scaledFont(.caption).foregroundStyle(.secondary).monospacedDigit()
                                Spacer()
                                Button("Previous") { page = currentPage - 1 }.disabled(currentPage == 0)
                                Button("Next") { page = currentPage + 1 }.disabled(start + 25 >= ranking.count)
                            }
                            let firstRate = ranking.first.flatMap { metric.value($0) } ?? 0
                            let peak = firstRate > 0 ? firstRate : 1
                            ForEach(Array(ranking.enumerated().dropFirst(start).prefix(25)), id: \.element.id) { index, row in
                                rankRow(row, index: index, peak: peak)
                            }
                        }
                    }.padding(20)
                }
                .scrollContentBackground(.hidden).background(.windowBackground)
            } else {
                ProgressView("Loading ledger…").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .onChange(of: byTool) { _, _ in page = 0 }
        .onChange(of: metric) { _, _ in page = 0 }
    }

    @ViewBuilder
    private func summary(_ vm: OverviewVM, ranking: [ShareRow]) -> some View {
        let all = rows(vm)
        let ms = all.reduce(UInt64(0)) { $0 + $1.speedMs }
        let output = all.reduce(UInt64(0)) { $0 + $1.speedOut }
        let est = all.reduce(UInt64(0)) { $0 + $1.speedEstMs }
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 230), spacing: 16)], spacing: 16) {
            summaryTile("Weighted output", value: ms > 0 ? Fmt.speed(Double(output) / Double(ms) * 1000) : "—",
                        note: String(localized: "Weighted by valid timing"))
            summaryTile("Current leader", value: ranking.first.flatMap { metric.value($0) }.map(Fmt.speed) ?? "—",
                        note: ranking.first.map(name) ?? "—")
            summaryTile("Estimated timing", value: ms > 0 ? Fmt.percent(Double(est) / Double(ms) * 100) : "—",
                        note: String(localized: "\(Fmt.count(UInt64(ranking.count))) ranked groups"))
        }
    }

    private func summaryTile(_ title: LocalizedStringKey, value: String, note: String) -> some View {
        Card(title: title) {
            Text(value).scaledFont(.title).fontWeight(.semibold).monospacedDigit().foregroundStyle(Color.accentColor)
            Text(note).scaledFont(.caption).foregroundStyle(.secondary).lineLimit(2)
        }
    }

    private func rankRow(_ row: ShareRow, index: Int, peak: Double) -> some View {
        let value = metric.value(row) ?? 0
        return Card(title: "") {
            HStack(spacing: 14) {
                Text(String(format: "%02d", index + 1)).scaledFont(.title3).fontWeight(.semibold).monospacedDigit()
                    .foregroundStyle(index < 3 ? Color.accentColor : Color.secondary)
                    .frame(width: 32, alignment: .leading)
                Text(name(row)).scaledFont(.headline).lineLimit(1).truncationMode(.middle)
                Spacer(minLength: 12)
                Text("\(metric.estimated(row) ? "≈" : "")\(Fmt.speed(value))")
                    .scaledFont(.title3).fontWeight(.semibold).monospacedDigit().foregroundStyle(Color.accentColor)
            }
            ProgressView(value: value, total: peak).tint(.accentColor)
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 135), alignment: .leading)], alignment: .leading, spacing: 12) {
                ForEach(SpeedMetric.allCases.filter { $0 != metric }) { part in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(part.title).scaledFont(.caption).foregroundStyle(.secondary)
                        Text(part.value(row).map { "\(part.estimated(row) ? "≈" : "")\(Fmt.speed($0))" } ?? "—")
                            .scaledFont(.callout).fontWeight(.medium).monospacedDigit()
                    }
                }
            }
            let ratio = row.speedMs > 0 ? Double(row.speedEstMs) / Double(row.speedMs) * 100 : 0
            Text("\(Fmt.count(row.events)) requests · Timed \(Fmt.duration(ms: row.speedMs)) · Estimated \(Fmt.percent(ratio))")
                .scaledFont(.caption).foregroundStyle(.secondary).monospacedDigit()
        }.help(name(row))
    }
}
