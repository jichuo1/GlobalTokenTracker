// Overview — the same widgets as the WinUI overview (stats, trend, share,
// per-tool, heatmap, quotas, unpriced), in a user-ordered, user-hideable
// layout. Every number comes from core's OverviewVM; the trend series and
// quota ranking are core's too (trend::drawn, rank_quotas).
import Charts
import SwiftUI

// MARK: - Layout (order + hidden, persisted)

enum OverviewWidget: String, CaseIterable, Identifiable, Codable {
    case stats, trend, share, apps, heat, quotas, unpriced
    var id: String { rawValue }

    var title: LocalizedStringKey {
        switch self {
        case .stats: "Summary"
        case .trend: "Trend"
        case .share: "Share"
        case .apps: "By Tool"
        case .heat: "Activity"
        case .quotas: "Subscription Quota"
        case .unpriced: "Unpriced Models"
        }
    }

    var symbol: String {
        switch self {
        case .stats: "square.grid.2x2"
        case .trend: "chart.bar.xaxis"
        case .share: "chart.pie"
        case .apps: "list.bullet"
        case .heat: "square.grid.3x3.fill"
        case .quotas: "gauge.with.dots.needle.50percent"
        case .unpriced: "questionmark.diamond"
        }
    }
}

/// Widget order + hidden set — the WinUI `PageLayout` for the overview page.
/// Ids missing from `order` append at the end, so new widgets show up.
struct OverviewLayout: Codable, Equatable {
    var order: [OverviewWidget] = OverviewWidget.allCases
    var hidden: Set<OverviewWidget> = []

    var all: [OverviewWidget] { order + OverviewWidget.allCases.filter { !order.contains($0) } }
    var visible: [OverviewWidget] { all.filter { !hidden.contains($0) } }

    static let key = "overviewLayout"

    static func load() -> Self {
        guard let data = UserDefaults.standard.data(forKey: key),
              let l = try? JSONDecoder().decode(Self.self, from: data) else { return Self() }
        return l
    }

    func save() {
        if let data = try? JSONEncoder().encode(self) {
            UserDefaults.standard.set(data, forKey: Self.key)
        }
    }
}

struct OverviewView: View {
    @Environment(AppModel.self) private var model
    @ViewState private var layout = OverviewLayout.load()

    var body: some View {
        Group {
            if let vm = model.vm {
                ScrollView {
                    VStack(spacing: 16) {
                        if case .available(let u) = model.updateState, !model.updateBannerDismissed {
                            UpdateBanner(info: u).cascade(0)
                        }
                        // Chain reaction on page turns: each card on its own
                        // spring, released in layout order. The stat tiles
                        // cascade individually inside their grid.
                        ForEach(Array(layout.visible.enumerated()), id: \.element) { i, w in
                            if w == .stats {
                                widget(w, vm: vm)
                            } else {
                                widget(w, vm: vm).cascade(i + 1)
                            }
                        }
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
        .sheet(isPresented: Binding(get: { model.customizingOverview },
                                    set: { model.customizingOverview = $0 })) {
            CustomizeOverviewSheet(layout: $layout)
        }
        .onChange(of: layout) { _, l in l.save() }
    }

    @ViewBuilder
    private func widget(_ w: OverviewWidget, vm: OverviewVM) -> some View {
        switch w {
        case .stats: StatTiles(vm: vm)
        case .trend: TrendCard(vm: vm, series: model.trend)
        case .share: ShareGrid(vm: vm)
        case .apps: ToolsCard(vm: vm)
        case .heat: HeatmapCard(days: vm.heat).equatable()
        case .quotas: QuotaStrip(rows: model.quotasRanked)
        case .unpriced: if !vm.unpriced.isEmpty { UnpricedNote(items: vm.unpriced) }
        }
    }
}

/// Customize sheet: drag to reorder, checkbox to show/hide — the Mac form of
/// the WinUI edit mode (move up/down + hide per card).
private struct CustomizeOverviewSheet: View {
    @Binding var layout: OverviewLayout
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Customize Overview").font(.title3.bold())
            Text("Drag to reorder. Uncheck a card to hide it.")
                .font(.callout).foregroundStyle(.secondary)
            List {
                ForEach(layout.all) { w in
                    Toggle(isOn: Binding(
                        get: { !layout.hidden.contains(w) },
                        set: { on in
                            if on { layout.hidden.remove(w) } else { layout.hidden.insert(w) }
                        }
                    )) {
                        Label(w.title, systemImage: w.symbol)
                    }
                }
                .onMove { from, to in
                    var o = layout.all
                    o.move(fromOffsets: from, toOffset: to)
                    layout.order = o
                }
            }
            .listStyle(.bordered)
            .frame(height: 260)
            HStack {
                Button("Restore Defaults") { layout = OverviewLayout() }
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.glassProminent)
            }
        }
        .padding(20)
        .frame(width: 380)
    }
}

private struct UpdateBanner: View {
    let info: UpdateInfo
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "arrow.down.app.fill").font(.title2).foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 2) {
                Text("New version \(info.version) is available").font(.headline)
                if let url = URL(string: info.htmlUrl) {
                    Link("Release notes", destination: url).font(.callout)
                }
            }
            Spacer()
            Button("Update Now") { model.downloadUpdate() }
                .buttonStyle(.glassProminent)
            Button {
                model.updateBannerDismissed = true
            } label: {
                Image(systemName: "xmark")
            }
            .buttonStyle(.borderless)
            .help("Dismiss")
        }
        .padding(14)
        .glassEffect(.regular.tint(.accentColor.opacity(0.15)), in: .rect(cornerRadius: 14))
    }
}

// MARK: - Stats

private struct StatTiles: View {
    let vm: OverviewVM

    var body: some View {
        let rl = Text(vm.range.title)
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 200), spacing: 16)], spacing: 16) {
            Tile(title: Text("\(rl) Tokens"), symbol: "number",
                 value: Fmt.tokensExact(vm.span.tokens),
                 detail: Text("Events \(Fmt.count(vm.span.events))"))
                .cascade(0)
            Tile(title: Text("\(rl) Est. Cost"), symbol: "dollarsign",
                 value: Fmt.usd(vm.span.costUsd),
                 detail: Text("All time \(Fmt.usd(vm.all.costUsd))"),
                 badge: "Estimated")
                .cascade(1)
            Tile(title: Text("\(rl) Cache Read"), symbol: "memorychip",
                 value: Fmt.tokensExact(vm.span.cacheReadTokens),
                 detail: Text("Input \(Fmt.tokensExact(vm.span.inputTokens))"))
                .cascade(2)
            Tile(title: Text("\(rl) Events"), symbol: "arrow.left.arrow.right",
                 value: Fmt.count(vm.span.events),
                 detail: Text("Active \(Fmt.duration(ms: vm.span.activeMs))"))
                .cascade(3)
        }
    }
}

private struct Tile: View {
    let title: Text
    let symbol: String
    let value: String
    let detail: Text
    var badge: LocalizedStringKey?

    var body: some View {
        Card {
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Label { title } icon: { Image(systemName: symbol) }
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                    Spacer()
                    if let badge {
                        Text(badge)
                            .font(.caption2.weight(.semibold))
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(.tint.opacity(0.15), in: .capsule)
                            .foregroundStyle(.tint)
                    }
                }
                Text(value)
                    .font(.system(.title, design: .rounded, weight: .semibold))
                    .monospacedDigit()
                    .contentTransition(.numericText())
                    .lineLimit(1)
                    .minimumScaleFactor(0.5)
                detail.font(.caption).foregroundStyle(.secondary).monospacedDigit()
            }
        }
    }
}

// MARK: - Trend

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

private struct TrendCard: View {
    let vm: OverviewVM
    let series: TrendSeries?
    @AppStorage("trendMetric") private var metric: Metric = .tokens
    /// Windows: trend_line — trailing 7-bucket moving average overlay.
    @AppStorage("trendLine") private var showAverage = false
    @ViewState private var hovered: String?

    /// Card title per range, as on Windows ("Today · hourly", "All · daily (last 60)").
    private var title: LocalizedStringKey {
        switch vm.range.key {
        case "today": "Today · Hourly"
        case "all": "All Time · Daily (Last 60)"
        case "custom":
            (series?.buckets.first?.date.count == 5) ? "Custom · Hourly" : "Custom · Daily"
        default: "Trend"
        }
    }

    var body: some View {
        let buckets = series?.buckets ?? vm.daily
        let avg = series?.avg ?? []
        // One formatted label per bucket per render (marks, selection and
        // hover all reuse it) instead of re-parsing dates per mark/hover.
        let labels = buckets.map { label($0.date) }
        let indexOf = Dictionary(labels.enumerated().map { ($1, $0) }, uniquingKeysWith: { a, _ in a })
        Card(title: title, trailing: AnyView(
            HStack(spacing: 10) {
                Toggle("7-pt Average", isOn: $showAverage)
                    .toggleStyle(.checkbox)
                    .disabled(metric != .tokens)
                    .help("Moving average over 7 days (7 hours for hourly ranges)")
                Picker("Metric", selection: $metric) {
                    ForEach(Metric.allCases) { Text($0.title).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
            }
        )) {
            if buckets.isEmpty {
                Text("No activity in this range.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 200)
            } else {
                Chart {
                    ForEach(Array(buckets.enumerated()), id: \.element.id) { i, b in
                        BarMark(x: .value("Bucket", labels[i]), y: .value("Value", value(b)))
                            .foregroundStyle(Color.accentColor.gradient)
                            .opacity(hovered == nil || hovered == labels[i] ? 1 : 0.45)
                            .cornerRadius(3)
                    }
                    if showAverage && metric == .tokens && avg.count == buckets.count {
                        ForEach(Array(avg.enumerated()), id: \.offset) { i, a in
                            LineMark(x: .value("Bucket", labels[i]), y: .value("Average", a))
                                .foregroundStyle(.orange)
                                .lineStyle(StrokeStyle(lineWidth: 2))
                                .interpolationMethod(.monotone)
                        }
                    }
                }
                .chartXAxis { AxisMarks(values: .automatic(desiredCount: 8)) { _ in AxisValueLabel() } }
                .chartYAxis {
                    AxisMarks { v in
                        AxisGridLine()
                        AxisValueLabel { Text(metric.format(v.as(Double.self) ?? 0)) }
                    }
                }
                .chartXSelection(value: $hovered)
                .chartFont()
                .frame(height: 220)
                .overlay(alignment: .topLeading) {
                    if let key = hovered, let i = indexOf[key] {
                        TrendTooltip(bucket: buckets[i], title: key,
                                     average: showAverage && i < avg.count ? avg[i] : nil)
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
    let average: Double?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.caption.bold())
            Text("\(Fmt.tokensCompact(bucket.tokens)) tokens · \(Fmt.usd(bucket.costUsd))")
                .font(.caption).monospacedDigit()
            if let average {
                Text("7-pt average \(Fmt.tokensCompact(UInt64(max(0, average))))")
                    .font(.caption).foregroundStyle(.orange)
            }
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

// MARK: - Share (four donuts: tool/model × cost/tokens)

private struct ShareGrid: View {
    let vm: OverviewVM

    var body: some View {
        let byToolCost = vm.byApp.map { (Fmt.app($0.app), $0.costUsd) }
        let byModelCost = vm.byModel.map { ($0.name, $0.costUsd) }
        let byToolTokens = vm.byApp.map { (Fmt.app($0.app), Double($0.tokens)) }
        let byModelTokens = vm.byModel.map { ($0.name, Double($0.tokens)) }
        Card(title: "\(Text(vm.range.title)) · Share") {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 230), spacing: 16)], spacing: 16) {
                Donut(title: "By Tool · Cost", items: byToolCost, format: Fmt.usd)
                Donut(title: "By Model · Cost", items: byModelCost, format: Fmt.usd)
                Donut(title: "By Tool · Tokens", items: byToolTokens) { Fmt.tokensCompact(UInt64(max(0, $0))) }
                Donut(title: "By Model · Tokens", items: byModelTokens) { Fmt.tokensCompact(UInt64(max(0, $0))) }
            }
        }
    }
}

private struct Donut: View {
    let title: LocalizedStringKey
    let slices: [ShareRow]
    let total: Double
    let format: (Double) -> String
    @ViewState private var selectedAngle: Double?

    /// Top 6 by value + an "Other" fold; zero rows dropped (no wedge to draw).
    init(title: LocalizedStringKey, items: [(String, Double)], format: @escaping (Double) -> String) {
        self.title = title
        self.format = format
        let sorted = items.filter { $0.1 > 0 }.sorted { $0.1 > $1.1 }
        var out = sorted.prefix(6).map { ShareRow(name: $0.0, events: 0, tokens: 0, costUsd: $0.1) }
        let rest = sorted.dropFirst(6).reduce(0) { $0 + $1.1 }
        if rest > 0 { out.append(ShareRow(name: String(localized: "Other"), events: 0, tokens: 0, costUsd: rest)) }
        slices = out
        total = out.reduce(0) { $0 + $1.costUsd }
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
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.subheadline).foregroundStyle(.secondary)
            if total <= 0 {
                Text("No data").foregroundStyle(.tertiary).frame(maxWidth: .infinity, minHeight: 150)
            } else {
                Chart(slices) { s in
                    SectorMark(angle: .value("Value", s.costUsd), innerRadius: .ratio(0.62), angularInset: 1.5)
                        .cornerRadius(4)
                        .foregroundStyle(by: .value("Name", s.name))
                        .opacity(selected == nil || selected?.name == s.name ? 1 : 0.4)
                }
                .chartLegend(.hidden)
                .chartAngleSelection(value: $selectedAngle)
                .chartFont()
                .chartBackground { _ in
                    VStack(spacing: 2) {
                        Text(selected?.name ?? String(localized: "Total"))
                            .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                        Text(format(selected?.costUsd ?? total))
                            .font(.headline).monospacedDigit()
                    }
                    .frame(maxWidth: 90)
                }
                .frame(height: 150)
                VStack(alignment: .leading, spacing: 3) {
                    ForEach(slices) { s in
                        HStack {
                            Text(s.name).lineLimit(1)
                            Spacer(minLength: 8)
                            Text(Fmt.percent(s.costUsd / total * 100))
                                .foregroundStyle(.secondary).monospacedDigit()
                        }
                        .font(.caption)
                    }
                }
            }
        }
    }
}

// MARK: - Per-tool

private struct ToolsCard: View {
    let vm: OverviewVM

    var body: some View {
        let top = Array(vm.byApp.prefix(8))
        let maxTokens = max(1, top.map(\.tokens).max() ?? 1)
        Card(title: "\(Text(vm.range.title)) · By Tool") {
            if top.isEmpty {
                Text("No data").foregroundStyle(.secondary).frame(maxWidth: .infinity, minHeight: 80)
            } else {
                VStack(spacing: 10) {
                    ForEach(top) { a in
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Text("\(Fmt.app(a.app))  ·  \(Fmt.count(a.events)) events").fontWeight(.medium)
                                Spacer()
                                Text("\(Fmt.tokensExact(a.tokens)) tokens · \(Fmt.usd(a.costUsd))")
                                    .foregroundStyle(.secondary).monospacedDigit()
                            }
                            .font(.callout)
                            ProgressView(value: Double(a.tokens), total: Double(maxTokens)).tint(.accentColor)
                        }
                    }
                }
            }
        }
    }
}

// MARK: - Heatmap

/// Heatmap metric — tokens | cost | calls | duration (Windows: heat_metric).
enum HeatMetric: String, CaseIterable, Identifiable {
    case tokens, cost, calls, duration
    var id: String { rawValue }
    var title: LocalizedStringKey {
        switch self {
        case .tokens: "Tokens"
        case .cost: "Cost"
        case .calls: "Requests"
        case .duration: "Duration"
        }
    }
    func value(_ d: HeatDay) -> Double {
        switch self {
        case .tokens: Double(d.tokens)
        case .cost: d.costUsd
        case .calls: Double(d.events)
        case .duration: Double(d.durationMs)
        }
    }
}

/// Equatable: the heatmap is a fixed one-year window, so a range change (the
/// common reload) leaves `days` untouched and the card skips re-rendering.
private struct HeatmapCard: View, Equatable {
    let days: [HeatDay]
    @AppStorage("heatMetric") private var metric: HeatMetric = .tokens

    nonisolated static func == (a: Self, b: Self) -> Bool { a.days == b.days }

    var body: some View {
        Card(title: "Activity", trailing: AnyView(
            Picker("Metric", selection: $metric) {
                ForEach(HeatMetric.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.menu)
            .labelsHidden()
            .fixedSize()
        )) {
            HeatmapGrid(days: days, metric: metric)
                .frame(height: 7 * 15 + 18)
            HStack(spacing: 4) {
                Spacer()
                Text("Less").font(.caption2).foregroundStyle(.secondary)
                ForEach([0.0, 0.25, 0.5, 0.75, 1.0], id: \.self) { t in
                    RoundedRectangle(cornerRadius: 2)
                        .fill(t == 0 ? Color.secondary.opacity(0.12) : Color.accentColor.opacity(0.18 + 0.82 * t))
                        .frame(width: 10, height: 10)
                }
                Text("More").font(.caption2).foregroundStyle(.secondary)
            }
        }
    }
}

/// GitHub-style year grid drawn in one Canvas pass — 365 cells as one draw
/// call instead of 365 views/marks. Layout depends only on (days, metric);
/// hover state lives in a child layer, so pointer moves never redo the
/// date math or redraw the grid — only the tooltip re-renders.
private struct HeatmapGrid: View {
    let days: [HeatDay]
    let metric: HeatMetric

    struct Cell {
        var day: HeatDay
        var week: Int
        var row: Int
        var value: Double
    }

    struct Layout {
        var cells: [Cell]
        /// week * 7 + row → index into `cells` (hover lookup is O(1)).
        var slots: [Int: Int]
        var weeks: Int
        var scale: Double
    }

    /// Pure integer calendar math (no DateFormatter): ~365 cells in well
    /// under a millisecond, so this never stalls a page turn.
    private func layout() -> Layout {
        guard let first = days.first.flatMap({ Fmt.epochDay($0.date) }) else {
            return Layout(cells: [], slots: [:], weeks: 0, scale: 1)
        }
        // 1970-01-01 was a Thursday; Calendar.firstWeekday: 1 = Sunday.
        let weekdaySun0 = (first % 7 + 7 + 4) % 7
        let lead = (weekdaySun0 - (Calendar.current.firstWeekday - 1) + 7) % 7
        var cells: [Cell] = []
        cells.reserveCapacity(days.count)
        var slots: [Int: Int] = [:]
        slots.reserveCapacity(days.count)
        var weeks = 0
        for d in days {
            guard let e = Fmt.epochDay(d.date) else { continue }
            let slot = e - first + lead
            slots[slot] = cells.count
            cells.append(Cell(day: d, week: slot / 7, row: slot % 7, value: metric.value(d)))
            weeks = max(weeks, slot / 7 + 1)
        }
        // 95th percentile tops the scale: one outlier day must not wash every
        // other day out to the palest tint.
        let sorted = cells.map(\.value).filter { $0 > 0 }.sorted()
        let p95 = sorted.isEmpty ? 1 : sorted[min(sorted.count - 1, Int(Double(sorted.count) * 0.95))]
        return Layout(cells: cells, slots: slots, weeks: weeks, scale: max(p95, .leastNonzeroMagnitude))
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
            .overlay { HeatHoverLayer(layout: lay, cell: cell, width: geo.size.width) }
        }
    }
}

private struct HeatHoverLayer: View {
    let layout: HeatmapGrid.Layout
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
                    let hit = (0..<7).contains(r) ? layout.slots[w * 7 + r].map { layout.cells[$0].day } : nil
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
                        if d.durationMs > 0 {
                            Text("Duration \(Fmt.duration(ms: d.durationMs))")
                                .font(.caption).foregroundStyle(.secondary)
                        }
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

// MARK: - Quotas + unpriced

private struct QuotaStrip: View {
    let rows: [QuotaRow]
    @Environment(AppModel.self) private var model
    static let maxRows = 6  // Windows: QUOTA_STRIP_ROWS

    var body: some View {
        Card(title: "Subscription Quota", trailing: AnyView(
            Button("Show All") { model.page = .quota }.buttonStyle(.link)
        )) {
            if rows.isEmpty {
                Text("No quota signals yet").foregroundStyle(.secondary)
            } else {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 260), spacing: 16)], spacing: 12) {
                    ForEach(rows.prefix(Self.maxRows)) { q in QuotaMeter(row: q, showApp: true) }
                }
                if rows.count > Self.maxRows {
                    Text("\(rows.count - Self.maxRows) more · see the Quota page")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }
}

/// One quota window — same content as the WinUI quota row: kind, account
/// badge, balance (credits) or used-% pill, bar, used/limit, reset, capture.
struct QuotaMeter: View {
    let row: QuotaRow
    var showApp = false

    private var credits: Bool { row.windowKind == "credits" }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text(showApp ? "\(Fmt.app(row.app)) · \(Fmt.quotaKind(row.windowKind))" : Fmt.quotaKind(row.windowKind))
                    .fontWeight(.semibold).lineLimit(1)
                if let acc = row.account {
                    Pill(text: acc, tint: .secondary)
                }
                Spacer()
                if credits, let u = row.used {
                    Pill(text: String(localized: "Balance \(Fmt.tokensCompact(UInt64(max(0, u))))"), tint: .secondary)
                        .help(String(localized: "Balance \(Fmt.tokensExact(UInt64(max(0, u))))"))
                } else if let pct = row.usedPercent {
                    Pill(text: Fmt.percent(pct), tint: Self.tint(pct))
                }
            }
            .font(.callout)
            if let pct = row.usedPercent {
                ProgressView(value: min(max(pct, 0), 100), total: 100).tint(Self.tint(pct))
            }
            Text(meta).font(.caption).foregroundStyle(.secondary).help(metaExact)
        }
    }

    /// Windows thresholds: > 80 % danger, > 50 % warning.
    static func tint(_ pct: Double) -> Color {
        pct > 80 ? .red : pct > 50 ? .orange : .accentColor
    }

    private func usage(_ f: @escaping (UInt64) -> String) -> String? {
        // `credits` rows carry the remaining balance in `used` — the pill.
        guard !credits else { return nil }
        let n = { (v: Double) in f(UInt64(max(0, v))) }
        switch (row.used, row.limitValue) {
        case let (u?, l?): return String(localized: "Used \(n(u)) / Limit \(n(l))")
        case let (u?, nil): return String(localized: "Usage \(n(u))")
        case let (nil, l?): return String(localized: "Limit \(n(l))")
        default: return nil
        }
    }

    private var tail: [String] {
        var parts: [String] = []
        if let r = row.resetsAt {
            parts.append(String(localized: "Resets \(Fmt.date(ms: r).formatted(.relative(presentation: .named)))"))
        }
        parts.append(String(localized: "Captured \(Fmt.timestamp(row.capturedAt))"))
        return parts
    }

    private var meta: String { ([usage(Fmt.tokensCompact)].compactMap { $0 } + tail).joined(separator: " · ") }
    private var metaExact: String { ([usage(Fmt.tokensExact)].compactMap { $0 } + tail).joined(separator: " · ") }
}

struct Pill: View {
    let text: String
    let tint: Color

    var body: some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .monospacedDigit()
            .lineLimit(1)
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(tint.opacity(0.15), in: .capsule)
            .foregroundStyle(tint)
    }
}

private struct UnpricedNote: View {
    let items: [NamedCount]

    var body: some View {
        Card {
            Label {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Unpriced Models").font(.headline)
                    Text(items.map { "\($0.name)×\(Fmt.count($0.count))" }.joined(separator: ", "))
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
