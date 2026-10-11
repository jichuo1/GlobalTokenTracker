// Overview: stat tiles, trend (Swift Charts), share donut, activity heatmap,
// quota strip. Everything renders straight from core's OverviewVM.
import Charts
import SwiftUI

enum Metric: String, CaseIterable, Identifiable {
    case tokens, cost, events
    var id: String { rawValue }
    var title: LocalizedStringKey {
        switch self {
        case .tokens: "Tokens"
        case .cost: "Cost"
        case .events: "Requests"
        }
    }
    func format(_ v: Double) -> String {
        switch self {
        case .tokens: Fmt.tokensCompact(UInt64(max(0, v)))
        case .cost: Fmt.usd(v)
        case .events: Fmt.count(UInt64(max(0, v)))
        }
    }
}

struct OverviewView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if let vm = model.vm {
                ScrollView {
                    VStack(spacing: 16) {
                        StatTiles(vm: vm)
                        TrendCard(vm: vm)
                        ViewThatFits(in: .horizontal) {
                            HStack(alignment: .top, spacing: 16) {
                                ShareCard(vm: vm).frame(minWidth: 360)
                                ToolsCard(vm: vm).frame(minWidth: 360)
                            }
                            VStack(spacing: 16) {
                                ShareCard(vm: vm)
                                ToolsCard(vm: vm)
                            }
                        }
                        HeatmapCard(days: vm.heat)
                        if !vm.quotas.isEmpty { QuotaStrip(rows: vm.quotas) }
                        if !vm.unpriced.isEmpty { UnpricedNote(items: vm.unpriced) }
                    }
                    .padding(20)
                }
                .scrollContentBackground(.hidden)
                .background(.windowBackground)
            } else if model.lastError != nil {
                EmptyState(title: "Couldn't Load Data", symbol: "exclamationmark.triangle",
                           message: "Check the sidebar for details, then press ⌘R to retry.")
            } else {
                ProgressView("Loading ledger…").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .navigationTitle("Overview")
        .navigationSubtitle(model.vm.map { "UTC\($0.tzOffset)" } ?? "")
    }
}

// MARK: - Tiles

private struct StatTiles: View {
    let vm: OverviewVM

    var body: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 190), spacing: 16)], spacing: 16) {
            Tile(title: "Tokens", symbol: "number",
                 value: Fmt.tokensCompact(vm.span.tokens),
                 detail: Text("Today \(Fmt.tokensCompact(vm.today.tokens))"),
                 help: Fmt.tokensExact(vm.span.tokens))
            Tile(title: "Cost", symbol: "dollarsign",
                 value: Fmt.usd(vm.span.costUsd),
                 detail: Text("Today \(Fmt.usd(vm.today.costUsd))"))
            Tile(title: "Requests", symbol: "arrow.left.arrow.right",
                 value: Fmt.count(vm.span.events),
                 detail: Text("Today \(Fmt.count(vm.today.events))"))
            Tile(title: "Active Time", symbol: "clock",
                 value: Fmt.duration(ms: vm.span.activeMs),
                 detail: Text("Today \(Fmt.duration(ms: vm.today.activeMs))"))
        }
    }
}

private struct Tile: View {
    let title: LocalizedStringKey
    let symbol: String
    let value: String
    let detail: Text
    var help: String?

    var body: some View {
        Card {
            VStack(alignment: .leading, spacing: 6) {
                Label(title, systemImage: symbol)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                Text(value)
                    .font(.system(.title, design: .rounded, weight: .semibold))
                    .monospacedDigit()
                    .contentTransition(.numericText())
                    .lineLimit(1)
                    .minimumScaleFactor(0.6)
                detail.font(.caption).foregroundStyle(.secondary).monospacedDigit()
            }
        }
        .help(help ?? value)
    }
}

// MARK: - Trend

private struct TrendCard: View {
    let vm: OverviewVM
    @AppStorage("trendMetric") private var metric: Metric = .tokens
    @ViewState private var hovered: String?

    var body: some View {
        Card(title: "Trend", trailing: AnyView(
            Picker("Metric", selection: $metric) {
                ForEach(Metric.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
        )) {
            if vm.daily.isEmpty {
                Text("No activity in this range.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 200)
            } else {
                Chart(vm.daily) { b in
                    BarMark(
                        x: .value("Bucket", label(b.date)),
                        y: .value(metric == .cost ? "Cost" : "Value", value(b))
                    )
                    .foregroundStyle(Color.accentColor.gradient)
                    .opacity(hovered == nil || hovered == label(b.date) ? 1 : 0.45)
                    .cornerRadius(3)
                }
                .chartXAxis {
                    AxisMarks(values: .automatic(desiredCount: 8)) { _ in
                        AxisValueLabel()
                    }
                }
                .chartYAxis {
                    AxisMarks { v in
                        AxisGridLine()
                        AxisValueLabel { Text(metric.format(v.as(Double.self) ?? 0)) }
                    }
                }
                .chartXSelection(value: $hovered)
                .frame(height: 220)
                .overlay(alignment: .topLeading) {
                    if let key = hovered, let b = vm.daily.first(where: { label($0.date) == key }) {
                        TrendTooltip(bucket: b, title: key)
                    }
                }
                .animation(.snappy, value: metric)
            }
        }
    }

    private func value(_ b: TrendBucket) -> Double {
        switch metric {
        case .tokens: Double(b.tokens)
        case .cost: b.costUsd
        case .events: Double(b.events)
        }
    }

    /// "2026-10-12" → localized short date; hourly "14:00" passes through.
    private func label(_ key: String) -> String {
        Fmt.bucketDate(key)?.formatted(.dateTime.month(.abbreviated).day()) ?? key
    }
}

private struct TrendTooltip: View {
    let bucket: TrendBucket
    let title: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.caption.bold())
            Text("\(Fmt.tokensCompact(bucket.tokens)) tokens · \(Fmt.usd(bucket.costUsd))")
                .font(.caption).monospacedDigit()
            ForEach(bucket.top, id: \.name) { t in
                Text("\(t.name)  \(Fmt.tokensCompact(t.count))")
                    .font(.caption2).foregroundStyle(.secondary)
            }
        }
        .padding(8)
        .glassEffect(.regular, in: .rect(cornerRadius: 10))
        .allowsHitTesting(false)
    }
}

// MARK: - Share donut

private struct ShareCard: View {
    let vm: OverviewVM
    @AppStorage("shareByModel") private var byModel = false
    @ViewState private var selectedAngle: Double?

    private var slices: [ShareRow] {
        let rows: [ShareRow] = byModel
            ? vm.byModel
            : vm.byApp.map {
                ShareRow(name: Fmt.app($0.app), events: $0.events, tokens: $0.tokens, costUsd: $0.costUsd)
            }
        let sorted = rows.sorted { $0.costUsd > $1.costUsd }
        guard sorted.count > 7 else { return sorted }
        let rest = sorted.dropFirst(6)
        let other = ShareRow(
            name: String(localized: "Other"),
            events: rest.reduce(0) { $0 + $1.events },
            tokens: rest.reduce(0) { $0 + $1.tokens },
            costUsd: rest.reduce(0) { $0 + $1.costUsd }
        )
        return Array(sorted.prefix(6)) + [other]
    }

    private var selected: ShareRow? {
        guard let a = selectedAngle else { return nil }
        var acc = 0.0
        for s in slices {
            acc += s.costUsd
            if a <= acc { return s }
        }
        return nil
    }

    var body: some View {
        let total = slices.reduce(0) { $0 + $1.costUsd }
        Card(title: "Cost Share", trailing: AnyView(
            Picker("Dimension", selection: $byModel) {
                Text("Tools").tag(false)
                Text("Models").tag(true)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
        )) {
            if total <= 0 {
                Text("No priced usage in this range.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 200)
            } else {
                HStack(alignment: .center, spacing: 20) {
                    Chart(slices) { s in
                        SectorMark(
                            angle: .value("Cost", s.costUsd),
                            innerRadius: .ratio(0.62),
                            angularInset: 1.5
                        )
                        .cornerRadius(4)
                        .foregroundStyle(by: .value("Name", s.name))
                        .opacity(selected == nil || selected?.name == s.name ? 1 : 0.4)
                    }
                    .chartLegend(.hidden)
                    .chartAngleSelection(value: $selectedAngle)
                    .chartBackground { _ in
                        VStack(spacing: 2) {
                            Text(selected?.name ?? String(localized: "Total"))
                                .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                            Text(Fmt.usd(selected?.costUsd ?? total))
                                .font(.title3.weight(.semibold)).monospacedDigit()
                        }
                        .frame(maxWidth: 110)
                    }
                    .frame(width: 190, height: 190)

                    VStack(alignment: .leading, spacing: 6) {
                        ForEach(slices) { s in
                            HStack {
                                Text(s.name).lineLimit(1)
                                Spacer(minLength: 12)
                                Text(Fmt.usd(s.costUsd)).monospacedDigit()
                                Text(Fmt.percent(s.costUsd / total * 100))
                                    .foregroundStyle(.secondary)
                                    .monospacedDigit()
                                    .frame(width: 52, alignment: .trailing)
                            }
                            .font(.callout)
                        }
                    }
                }
            }
        }
    }
}

// MARK: - Per-tool table

private struct ToolsCard: View {
    let vm: OverviewVM

    var body: some View {
        let maxTokens = max(1, vm.byApp.map(\.tokens).max() ?? 1)
        Card(title: "By Tool") {
            if vm.byApp.isEmpty {
                Text("No tools recorded in this range.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 120)
            } else {
                VStack(spacing: 10) {
                    ForEach(vm.byApp.sorted { $0.tokens > $1.tokens }) { a in
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Text(Fmt.app(a.app)).fontWeight(.medium)
                                Spacer()
                                Text(Fmt.tokensCompact(a.tokens)).monospacedDigit()
                                Text(Fmt.usd(a.costUsd))
                                    .foregroundStyle(.secondary)
                                    .monospacedDigit()
                                    .frame(minWidth: 64, alignment: .trailing)
                            }
                            .font(.callout)
                            ProgressView(value: Double(a.tokens), total: Double(maxTokens))
                                .progressViewStyle(.linear)
                                .tint(.accentColor)
                        }
                        .help("\(Fmt.count(a.events)) requests · \(Fmt.tokensExact(a.tokens)) tokens")
                    }
                }
            }
        }
    }
}

// MARK: - Heatmap

private struct HeatmapCard: View {
    let days: [HeatDay]
    @AppStorage("heatMetric") private var metric: Metric = .tokens

    var body: some View {
        Card(title: "Activity", trailing: AnyView(
            Picker("Metric", selection: $metric) {
                ForEach(Metric.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.menu)
            .labelsHidden()
            .fixedSize()
        )) {
            HeatmapGrid(days: days, metric: metric)
                .frame(height: 7 * 15 + 18)
        }
    }
}

/// GitHub-style year grid drawn in one Canvas pass — 365 cells as one draw
/// call instead of 365 views/marks. Layout depends only on (days, metric);
/// hover state lives in a child layer, so pointer moves never redo the
/// date math or redraw the grid — only the tooltip re-renders.
private struct HeatmapGrid: View {
    let days: [HeatDay]
    let metric: Metric

    struct Cell {
        var day: HeatDay
        var week: Int
        var row: Int
        var value: Double
    }

    private func layout() -> (cells: [Cell], weeks: Int, scale: Double) {
        let cal = Calendar.current
        guard let first = days.first.flatMap({ Fmt.bucketDate($0.date) }) else { return ([], 0, 1) }
        let lead = (cal.component(.weekday, from: first) - cal.firstWeekday + 7) % 7
        var cells: [Cell] = []
        cells.reserveCapacity(days.count)
        var weeks = 0
        for d in days {
            guard let date = Fmt.bucketDate(d.date) else { continue }
            let slot = (cal.dateComponents([.day], from: first, to: date).day ?? 0) + lead
            let v: Double = switch metric {
            case .tokens: Double(d.tokens)
            case .cost: d.costUsd
            case .events: Double(d.events)
            }
            cells.append(Cell(day: d, week: slot / 7, row: slot % 7, value: v))
            weeks = max(weeks, slot / 7 + 1)
        }
        // 95th percentile tops the scale: one outlier day must not wash every
        // other day out to the palest tint.
        let sorted = cells.map(\.value).filter { $0 > 0 }.sorted()
        let p95 = sorted.isEmpty ? 1 : sorted[min(sorted.count - 1, Int(Double(sorted.count) * 0.95))]
        return (cells, weeks, max(p95, .leastNonzeroMagnitude))
    }

    var body: some View {
        let lay = layout()
        GeometryReader { geo in
            let cell = min(15, max(6, geo.size.width / Double(max(lay.weeks, 1))))
            let gap = max(2, cell * 0.18)
            Canvas { ctx, _ in
                for c in lay.cells {
                    let rect = CGRect(x: Double(c.week) * cell, y: Double(c.row) * cell,
                                      width: cell - gap, height: cell - gap)
                    let shade: GraphicsContext.Shading = c.value <= 0
                        ? .color(.secondary.opacity(0.12))
                        : .color(.accentColor.opacity(0.18 + 0.82 * min(1, c.value / lay.scale)))
                    ctx.fill(Path(roundedRect: rect, cornerRadius: max(1.5, cell * 0.2)), with: shade)
                }
            }
            .overlay { HeatHoverLayer(cells: lay.cells, cell: cell, width: geo.size.width) }
        }
    }
}

private struct HeatHoverLayer: View {
    let cells: [HeatmapGrid.Cell]
    let cell: Double
    let width: Double
    @ViewState private var hover: HeatDay?
    @ViewState private var point: CGPoint = .zero

    var body: some View {
        Color.clear
            .contentShape(.rect)
            .onContinuousHover { phase in
                switch phase {
                case .active(let p):
                    let w = Int(p.x / cell), r = Int(p.y / cell)
                    let hit = cells.first { $0.week == w && $0.row == r }?.day
                    if hit != hover { hover = hit }
                    point = p
                case .ended:
                    hover = nil
                }
            }
            .overlay(alignment: .topLeading) {
                if let d = hover {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(Fmt.bucketDate(d.date)?.formatted(date: .complete, time: .omitted) ?? d.date)
                            .font(.caption.bold())
                        Text("\(Fmt.tokensCompact(d.tokens)) tokens · \(Fmt.usd(d.costUsd)) · \(Fmt.count(d.events)) requests")
                            .font(.caption).monospacedDigit()
                    }
                    .padding(8)
                    .glassEffect(.regular, in: .rect(cornerRadius: 10))
                    .fixedSize()
                    .offset(x: min(point.x + 12, max(0, width - 280)), y: point.y + 14)
                    .allowsHitTesting(false)
                }
            }
    }
}

// MARK: - Quota + unpriced

private struct QuotaStrip: View {
    let rows: [QuotaRow]
    @Environment(AppModel.self) private var model

    var body: some View {
        Card(title: "Quota", trailing: AnyView(
            Button("Show All") { model.page = .quota }.buttonStyle(.link)
        )) {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 240), spacing: 16)], spacing: 12) {
                ForEach(rows) { q in QuotaMeter(row: q) }
            }
        }
    }
}

struct QuotaMeter: View {
    let row: QuotaRow

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text("\(Fmt.app(row.app)) · \(Fmt.quotaKind(row.windowKind))").lineLimit(1)
                Spacer()
                if let pct = row.usedPercent {
                    Text(Fmt.percent(pct)).monospacedDigit().foregroundStyle(tint(pct))
                }
            }
            .font(.callout)
            if let pct = row.usedPercent {
                ProgressView(value: min(pct, 100), total: 100).tint(tint(pct))
            }
            if let r = row.resetsAt {
                Text("Resets \(Fmt.date(ms: r), format: .relative(presentation: .named))")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func tint(_ pct: Double) -> Color {
        pct >= 90 ? .red : pct >= 70 ? .orange : .accentColor
    }
}

private struct UnpricedNote: View {
    let items: [NamedCount]

    var body: some View {
        Card {
            Label {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Some models have no known price").font(.headline)
                    Text(items.map { "\($0.name) (\(Fmt.count($0.count)))" }.joined(separator: ", "))
                        .font(.callout).foregroundStyle(.secondary)
                    Text("Their usage is counted, but no cost is guessed.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            } icon: {
                Image(systemName: "questionmark.diamond").foregroundStyle(.orange)
            }
        }
    }
}
