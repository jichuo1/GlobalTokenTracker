// Custom statistics window: month calendar (click a start day, then an end
// day), one-tap quick ranges, and a 24-hour dial that narrows a single day to
// whole-hour slots. Same interaction model as the WinUI shell's custom-range
// chrome; every window is built by core (`gtt_range`) — this view only turns
// clicks into civil days / hours and renders core's read-back.
import SwiftUI

/// A local calendar day with no time or zone attached ("YYYY-MM-DD" to core).
struct CivilDay: Hashable, Comparable {
    var year: Int
    var month: Int
    var day: Int

    init(year: Int, month: Int, day: Int) {
        self.year = year
        self.month = month
        self.day = day
    }

    init?(_ s: String?) {
        guard let parts = s?.split(separator: "-").compactMap({ Int($0) }), parts.count == 3 else {
            return nil
        }
        self.init(year: parts[0], month: parts[1], day: parts[2])
    }

    var key: String { String(format: "%04d-%02d-%02d", year, month, day) }

    static func < (a: Self, b: Self) -> Bool {
        (a.year, a.month, a.day) < (b.year, b.month, b.day)
    }

    /// Gregorian components → Date at local noon (display only; never sent).
    var date: Date {
        Self.gregorian.date(from: DateComponents(year: year, month: month, day: day, hour: 12)) ?? .now
    }

    static let gregorian: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = .current
        c.firstWeekday = 2  // Monday-first, matching core's QuickRange::ThisWeek
        return c
    }()
}

struct CustomRangeButton: View {
    @Environment(AppModel.self) private var model
    @ViewState private var open = false

    var body: some View {
        Button {
            open.toggle()
        } label: {
            Label {
                Text(summary)
            } icon: {
                Image(systemName: "calendar")
            }
            .labelStyle(.titleAndIcon)
        }
        .help("Pick a custom date or hour range")
        .popover(isPresented: $open, arrowEdge: .bottom) {
            CustomRangePanel()
                .environment(model)
        }
    }

    /// Toolbar caption: the custom window, compact ("Oct 3 – 9", "Oct 12, 09:00–19:00").
    private var summary: String {
        guard model.range.key == "custom", let info = model.rangeInfo,
              let a = CivilDay(info.firstDay), let b = CivilDay(info.lastDay)
        else { return String(localized: "Custom") }
        if let h = info.hourSpan, a == b, h != [0, 23] {
            let day = a.date.formatted(.dateTime.month(.abbreviated).day())
            return "\(day), \(Self.hh(h[0]))–\(Self.hh(h[1] + 1))"
        }
        return (a.date..<b.date.addingTimeInterval(1))
            .formatted(.interval.month(.abbreviated).day())
    }

    static func hh(_ h: Int) -> String { String(format: "%02d:00", h) }
}

struct CustomRangePanel: View {
    @Environment(AppModel.self) private var model
    /// First click of a two-click day pick (nil = next click starts a range).
    @ViewState private var anchorDay: CivilDay?
    @ViewState private var anchorHour: Int?
    @ViewState private var shownMonth: CivilDay?
    @ViewState private var info: RangeInfo?

    var body: some View {
        let info = info ?? model.rangeInfo
        let today = CivilDay(info?.today) ?? CivilDay(year: 2000, month: 1, day: 1)
        let sel = selection(info)
        HStack(alignment: .top, spacing: 20) {
            MonthGrid(
                month: shownMonth ?? CivilDay(year: (sel?.1 ?? today).year, month: (sel?.1 ?? today).month, day: 1),
                today: today,
                selection: sel,
                pick: pickDay,
                step: stepMonth
            )
            Divider()
            VStack(alignment: .leading, spacing: 16) {
                summaryBlock(info, sel: sel)
                quickBlock
                Divider()
                hourBlock(info, sel: sel, today: today)
            }
            .frame(width: 270)
        }
        .padding(18)
        .onAppear { refresh() }
        .onChange(of: model.range) { _, _ in refresh() }
    }

    // MARK: Blocks

    private func summaryBlock(_ info: RangeInfo?, sel: (CivilDay, CivilDay)?) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Selected Range").font(.subheadline).foregroundStyle(.secondary)
            if let (a, b) = sel {
                if let h = info?.hourSpan, a == b, h != [0, 23] {
                    Text("\(a.date, format: .dateTime.year().month().day()) \(CustomRangeButton.hh(h[0])) → \(CustomRangeButton.hh(h[1] + 1))")
                        .font(.headline)
                    Text("\(h[1] - h[0] + 1) hours").foregroundStyle(.secondary)
                } else {
                    Text((a.date..<b.date.addingTimeInterval(1)).formatted(.interval.year().month().day()))
                        .font(.headline)
                    Text("\(info?.days ?? 1) days").foregroundStyle(.secondary)
                }
            } else {
                Text(model.range.title).font(.headline)
            }
            Text(anchorDay == nil ? "Click a start day, then an end day." : "Now click the other end.")
                .font(.caption)
                .foregroundStyle(anchorDay == nil ? Color.secondary : Color.accentColor)
        }
    }

    private var quickBlock: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Quick Ranges").font(.subheadline).foregroundStyle(.secondary)
            GlassEffectContainer(spacing: 6) {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 82), spacing: 6)], alignment: .leading, spacing: 6) {
                    ForEach(Self.quick, id: \.key) { q in
                        Button(q.title) {
                            anchorDay = nil
                            anchorHour = nil
                            model.applyWindow(.quick(q.key))
                            jumpToSelection()
                        }
                        .buttonStyle(.glass)
                        .controlSize(.small)
                    }
                }
            }
        }
    }

    private func hourBlock(_ info: RangeInfo?, sel: (CivilDay, CivilDay)?, today: CivilDay) -> some View {
        let single = sel.flatMap { $0.0 == $0.1 ? $0.0 : nil }
        let span = single == nil ? nil : info?.hourSpan
        let partial = span.map { $0 != [0, 23] } ?? false
        // Hours after the current one are in the future when the day is today.
        let liveUntil = single == today ? (info?.nowHour ?? 23) : 23
        return VStack(alignment: .leading, spacing: 10) {
            Text("By Hour").font(.subheadline).foregroundStyle(.secondary)
            HStack(alignment: .center, spacing: 16) {
                HourDial(span: span, anchor: anchorHour, liveUntil: liveUntil, enabled: single != nil) { h in
                    pickHour(h, day: single)
                }
                .frame(width: 132, height: 132)
                VStack(alignment: .leading, spacing: 6) {
                    Text(span.map { partial ? "\(CustomRangeButton.hh($0[0])) – \(CustomRangeButton.hh($0[1] + 1))" : String(localized: "All Day") } ?? "—")
                        .font(.headline)
                        .monospacedDigit()
                    Text(single == nil
                         ? "Select a single day to narrow it to hours."
                         : anchorHour == nil ? "Click a start hour, then an end hour." : "Now click the other end.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Button("All Day") {
                        anchorHour = nil
                        if let d = single { model.applyWindow(.days(first: d.key, last: d.key)) }
                    }
                    .disabled(!partial)
                    .controlSize(.small)
                }
            }
        }
    }

    // MARK: Interaction

    private func pickDay(_ day: CivilDay) {
        anchorHour = nil
        if let first = anchorDay {
            anchorDay = nil
            model.applyWindow(.days(first: first.key, last: day.key))
        } else {
            anchorDay = day
            model.applyWindow(.days(first: day.key, last: day.key))
        }
    }

    private func pickHour(_ h: Int, day: CivilDay?) {
        guard let day else { return }
        if let first = anchorHour {
            anchorHour = nil
            model.applyWindow(.hours(day: day.key, h1: first, h2: h))
        } else {
            anchorHour = h
            model.applyWindow(.hours(day: day.key, h1: h, h2: h))
        }
    }

    private func stepMonth(_ delta: Int) {
        let base = shownMonth ?? selection(info ?? model.rangeInfo).map { CivilDay(year: $0.1.year, month: $0.1.month, day: 1) }
            ?? CivilDay(info?.today ?? model.rangeInfo?.today) ?? CivilDay(year: 2000, month: 1, day: 1)
        var m = base.month + delta, y = base.year
        while m < 1 { m += 12; y -= 1 }
        while m > 12 { m -= 12; y += 1 }
        shownMonth = CivilDay(year: y, month: m, day: 1)
    }

    private func jumpToSelection() {
        shownMonth = nil
    }

    private func refresh() {
        info = try? Core.window(.describe(model.range))
    }

    /// Day span of the current window: custom → core's span; presets → none
    /// highlighted (the calendar shows custom windows only).
    private func selection(_ info: RangeInfo?) -> (CivilDay, CivilDay)? {
        guard let a = CivilDay(info?.firstDay), let b = CivilDay(info?.lastDay) else { return nil }
        return (a, b)
    }

    private struct Quick {
        var key: String
        var title: LocalizedStringKey
    }

    private static let quick = [
        Quick(key: "yesterday", title: "Yesterday"),
        Quick(key: "this_week", title: "This Week"),
        Quick(key: "this_month", title: "This Month"),
        Quick(key: "last_month", title: "Last Month"),
        Quick(key: "last_90_days", title: "Last 90 Days"),
    ]
}

// MARK: - Month grid

private struct MonthGrid: View {
    let month: CivilDay  // day == 1
    let today: CivilDay
    let selection: (CivilDay, CivilDay)?
    let pick: (CivilDay) -> Void
    let step: (Int) -> Void

    private static let cell: CGFloat = 34

    private var cal: Calendar { CivilDay.gregorian }

    /// Leading blanks + the month's days, Monday-first.
    private var cells: [CivilDay?] {
        let first = month.date
        let lead = (cal.component(.weekday, from: first) - cal.firstWeekday + 7) % 7
        let count = cal.range(of: .day, in: .month, for: first)?.count ?? 30
        return Array(repeating: nil, count: lead)
            + (1...count).map { CivilDay(year: month.year, month: month.month, day: $0) }
    }

    private var weekdaySymbols: [String] {
        let s = cal.veryShortStandaloneWeekdaySymbols
        let k = cal.firstWeekday - 1
        return Array(s[k...] + s[..<k])
    }

    var body: some View {
        let atCurrent = (month.year, month.month) >= (today.year, today.month)
        VStack(spacing: 8) {
            HStack {
                Text(month.date, format: .dateTime.year().month(.wide))
                    .font(.headline)
                Spacer()
                ControlGroup {
                    Button { step(-1) } label: { Image(systemName: "chevron.left") }
                        .help("Previous month")
                    Button { step(1) } label: { Image(systemName: "chevron.right") }
                        .help("Next month")
                        .disabled(atCurrent)
                }
                .controlSize(.small)
                .fixedSize()
            }
            Grid(horizontalSpacing: 2, verticalSpacing: 2) {
                GridRow {
                    ForEach(Array(weekdaySymbols.enumerated()), id: \.offset) { _, s in
                        Text(s).font(.caption).foregroundStyle(.secondary)
                            .frame(width: Self.cell)
                    }
                }
                let rows = stride(from: 0, to: cells.count, by: 7).map { Array(cells[$0..<min($0 + 7, cells.count)]) }
                ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                    GridRow {
                        ForEach(0..<7, id: \.self) { i in
                            if i < row.count, let d = row[i] {
                                DayCell(day: d, today: today, selection: selection, pick: pick)
                                    .frame(width: Self.cell, height: Self.cell - 4)
                            } else {
                                Color.clear.frame(width: Self.cell, height: Self.cell - 4)
                            }
                        }
                    }
                }
            }
        }
        .frame(width: 7 * Self.cell + 12)
    }
}

private struct DayCell: View {
    let day: CivilDay
    let today: CivilDay
    let selection: (CivilDay, CivilDay)?
    let pick: (CivilDay) -> Void

    var body: some View {
        let future = day > today
        let endpoint = selection.map { day == $0.0 || day == $0.1 } ?? false
        let inside = selection.map { day > $0.0 && day < $0.1 } ?? false
        Button {
            pick(day)
        } label: {
            Text("\(day.day)")
                .monospacedDigit()
                .fontWeight(endpoint ? .semibold : .regular)
                .foregroundStyle(endpoint ? Color.white : future ? Color.secondary.opacity(0.5) : Color.primary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background {
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(endpoint ? Color.accentColor : inside ? Color.accentColor.opacity(0.22) : .clear)
                }
                .overlay {
                    if day == today && !endpoint {
                        RoundedRectangle(cornerRadius: 7, style: .continuous)
                            .strokeBorder(Color.accentColor.opacity(0.7), lineWidth: 1)
                    }
                }
                .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .disabled(future)
        .accessibilityLabel(Text(day.date, format: .dateTime.year().month(.wide).day()))
        .accessibilityAddTraits(endpoint || inside ? .isSelected : [])
    }
}

// MARK: - Hour dial

/// 24-hour clock face (00 at the top, clockwise). The selected inclusive
/// slot span draws as an arc; click a slot to set an end. One Canvas pass
/// for the face, hit-testing by angle — no 24 separate views.
struct HourDial: View {
    let span: [Int]?
    let anchor: Int?
    let liveUntil: Int
    let enabled: Bool
    let pick: (Int) -> Void

    var body: some View {
        GeometryReader { geo in
            let size = min(geo.size.width, geo.size.height)
            let c = CGPoint(x: size / 2, y: size / 2)
            let ring = size / 2 - 10
            Canvas { ctx, _ in
                // Track
                ctx.stroke(Path(ellipseIn: CGRect(x: c.x - ring, y: c.y - ring, width: ring * 2, height: ring * 2)),
                           with: .color(.secondary.opacity(enabled ? 0.18 : 0.08)), lineWidth: 12)
                // Selected arc (inclusive slots a...b)
                if enabled, let s = span, s.count == 2 {
                    var arc = Path()
                    arc.addArc(center: c, radius: ring,
                               startAngle: angle(Double(s[0])), endAngle: angle(Double(s[1] + 1)),
                               clockwise: false)
                    ctx.stroke(arc, with: .color(.accentColor), style: StrokeStyle(lineWidth: 12, lineCap: .round))
                }
                // Hour ticks; future hours dimmed
                for h in 0..<24 {
                    let a = angle(Double(h) + 0.5).radians
                    let r1 = ring - 12, r2 = ring - (h % 3 == 0 ? 20 : 16)
                    var t = Path()
                    t.move(to: CGPoint(x: c.x + cos(a) * r1, y: c.y + sin(a) * r1))
                    t.addLine(to: CGPoint(x: c.x + cos(a) * r2, y: c.y + sin(a) * r2))
                    let live = enabled && h <= liveUntil
                    ctx.stroke(t, with: .color(live ? .primary.opacity(0.55) : .secondary.opacity(0.25)), lineWidth: 1)
                }
                // Pending first click
                if let h = anchor {
                    let a = angle(Double(h) + 0.5).radians
                    let p = CGPoint(x: c.x + cos(a) * ring, y: c.y + sin(a) * ring)
                    ctx.fill(Path(ellipseIn: CGRect(x: p.x - 5, y: p.y - 5, width: 10, height: 10)),
                             with: .color(.white))
                    ctx.stroke(Path(ellipseIn: CGRect(x: p.x - 5, y: p.y - 5, width: 10, height: 10)),
                               with: .color(.accentColor), lineWidth: 2)
                }
            }
            .overlay {
                ForEach([0, 6, 12, 18], id: \.self) { h in
                    let a = angle(Double(h)).radians
                    Text(String(format: "%02d", h))
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.secondary)
                        .position(x: c.x + cos(a) * (ring - 30), y: c.y + sin(a) * (ring - 30))
                }
            }
            .contentShape(.circle)
            .onTapGesture(coordinateSpace: .local) { p in
                guard enabled else { return }
                // Angle from 12 o'clock, clockwise → slot 0..23.
                var deg = atan2(p.y - c.y, p.x - c.x) * 180 / .pi + 90
                if deg < 0 { deg += 360 }
                let h = min(23, Int(deg / 15))
                if h <= liveUntil { pick(h) }
            }
            .opacity(enabled ? 1 : 0.55)
        }
        .accessibilityElement()
        .accessibilityLabel("Hour range")
        .accessibilityValue(span.map { "\($0[0]):00 to \($0[1] + 1):00" } ?? "")
    }

    /// Slot position → angle; 0 at 12 o'clock, 15° per hour, clockwise.
    private func angle(_ slot: Double) -> Angle {
        .degrees(slot * 15 - 90)
    }
}
