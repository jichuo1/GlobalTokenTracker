// Custom statistics window: month calendar (click a start day, then an end
// day), one-tap quick ranges, and a 24-hour dial that narrows a single day to
// whole-hour slots. Same interaction model as the WinUI shell's custom-range
// chrome; every window is built by core (`gtt_range`) — this view only turns
// clicks into civil days / hours and renders core's read-back.
import AppKit
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

    var body: some View {
        @Bindable var model = model
        Button {
            model.rangePopoverShown.toggle()
        } label: {
            Label {
                // Fixed width: the label used to resize with every pick
                // ("Oct 3 – 9" vs "Sep 28 – Oct 12"), which moved the button
                // and re-anchored the popover, so the panel slid sideways.
                Text(Self.summary(model.range, model.rangeInfo))
                    .monospacedDigit()
                    .rollingText(down: model.rangeRollsDown)
                    .frame(width: Self.captionWidth, alignment: .center)
            } icon: {
                Image(systemName: "calendar")
            }
            .labelStyle(.titleAndIcon)
        }
        .help("Pick a custom date or hour range")
        .popover(isPresented: $model.rangePopoverShown, arrowEdge: .bottom) {
            CustomRangePanel()
                .environment(model)
        }
    }

    /// Toolbar caption: the custom window, compact ("Oct 3 – 9", "Oct 12, 09:00–19:00").
    static func summary(_ range: RangeSpec, _ info: RangeInfo?) -> String {
        guard range.key == "custom", let info,
              let a = CivilDay(info.firstDay), let b = CivilDay(info.lastDay)
        else { return String(localized: "Custom") }
        if let h = info.hourSpan, a == b, h != [0, 23] {
            return hourly(a, h[0], h[1])
        }
        return daily(a, b)
    }

    private static func hourly(_ d: CivilDay, _ h1: Int, _ h2: Int) -> String {
        "\(d.date.formatted(.dateTime.month(.abbreviated).day())), \(hh(h1))–\(hh(h2 + 1))"
    }

    private static func daily(_ a: CivilDay, _ b: CivilDay) -> String {
        // Across New Year the interval style adds both years ("Dec 28, 2026 –
        // Jan 30, 2027"), twice the usual width; the toolbar keeps it compact
        // (the panel headline shows the years).
        if a.year != b.year {
            let md = Date.FormatStyle.dateTime.month(.abbreviated).day()
            return "\(a.date.formatted(md)) – \(b.date.formatted(md))"
        }
        return (a.date..<b.date.addingTimeInterval(1)).formatted(.interval.month(.abbreviated).day())
    }

    /// Width of the longest caption, measured once with the toolbar's font
    /// and tabular digits (was 26 hidden Text views re-laid out per pick).
    private static let captionWidth: CGFloat = {
        let font = NSFont.monospacedDigitSystemFont(ofSize: NSFont.systemFontSize, weight: .regular)
        let w = widest.map { ($0 as NSString).size(withAttributes: [.font: font]).width }.max() ?? 80
        return ceil(w) + 2
    }()

    /// Longest captions this locale can produce — every month's abbreviation,
    /// two-digit days, cross-month spans and the hour form.
    private static let widest: [String] = {
        var out = [String(localized: "Custom")]
        for m in 1...12 {
            let d = CivilDay(year: 2026, month: m, day: 28)
            let next = CivilDay(year: 2026, month: m % 12 + 1, day: 28)
            out.append(hourly(d, 22, 23))
            out.append(daily(d, m == 12 ? CivilDay(year: 2026, month: 12, day: 30) : next))
        }
        out.append(daily(CivilDay(year: 2026, month: 12, day: 28), CivilDay(year: 2027, month: 1, day: 30)))
        return out
    }()

    static func hh(_ h: Int) -> String { String(format: "%02d:00", h) }
}

struct CustomRangePanel: View {
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// First click of a two-click day pick (nil = next click starts a range).
    @ViewState private var anchorDay: CivilDay?
    @ViewState private var anchorHour: Int?
    /// Month on screen (day == 1); nil until first shown.
    @ViewState private var shownMonth: CivilDay?
    /// Direction of the last month change — drives the page slide and the
    /// month title's roll.
    @ViewState private var monthForward = true

    var body: some View {
        let info = model.rangeInfo
        let today = CivilDay(info?.today) ?? CivilDay(year: 2000, month: 1, day: 1)
        let sel = selection(info)
        HStack(alignment: .top, spacing: 20) {
            MonthGrid(
                month: shownMonth ?? Self.monthOf(sel?.1 ?? today),
                forward: monthForward,
                today: today,
                selection: sel,
                anchor: anchorDay,
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
        .onAppear {
            model.refreshRangeInfo()
            shownMonth = Self.monthOf(selection(model.rangeInfo)?.1 ?? today)
        }
    }

    // MARK: Blocks

    /// Headline + count as single Text views whose strings change, so the
    /// characters roll like a wheel instead of the views swapping.
    private func summaryBlock(_ info: RangeInfo?, sel: (CivilDay, CivilDay)?) -> some View {
        let down = model.rangeRollsDown
        var headline = String(localized: "Custom")
        var detail = " "
        if let (a, b) = sel {
            if let h = info?.hourSpan, a == b, h != [0, 23] {
                headline = "\(a.date.formatted(.dateTime.year().month().day())) \(CustomRangeButton.hh(h[0])) → \(CustomRangeButton.hh(h[1] + 1))"
                detail = String(localized: "\(h[1] - h[0] + 1) hours")
            } else {
                headline = (a.date..<b.date.addingTimeInterval(1)).formatted(.interval.year().month().day())
                detail = String(localized: "\(info?.days ?? 1) days")
            }
        }
        return VStack(alignment: .leading, spacing: 4) {
            Text("Selected Range").scaledFont(.subheadline).foregroundStyle(.secondary)
            Group {
                if sel == nil {
                    Text(model.range.title)
                } else {
                    Text(headline).rollingText(down: down)
                }
            }
            .scaledFont(.headline)
            Text(detail).foregroundStyle(.secondary).rollingText(down: down)
            Text(anchorDay == nil ? "Click a start day, then an end day." : "Now click the other end.")
                .scaledFont(.caption)
                .foregroundStyle(anchorDay == nil ? Color.secondary : Color.accentColor)
                .lineLimit(2, reservesSpace: true)
                .contentTransition(.opacity)
                .animation(Motion.hover, value: anchorDay)
        }
        .monospacedDigit()
        .lineLimit(1)
    }

    private var quickBlock: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Quick Ranges").scaledFont(.subheadline).foregroundStyle(.secondary)
            GlassEffectContainer(spacing: 6) {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 82), spacing: 6)], alignment: .leading, spacing: 6) {
                    ForEach(Self.quick, id: \.key) { q in
                        Button(q.title) {
                            anchorDay = nil
                            anchorHour = nil
                            model.applyWindow(.quick(q.key))
                            showMonth(of: selection(model.rangeInfo)?.1)
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
        let label = span.map {
            partial ? "\(CustomRangeButton.hh($0[0])) – \(CustomRangeButton.hh($0[1] + 1))" : String(localized: "All Day")
        } ?? "—"
        return VStack(alignment: .leading, spacing: 10) {
            Text("By Hour").scaledFont(.subheadline).foregroundStyle(.secondary)
            HStack(alignment: .center, spacing: 16) {
                HourDial(span: span, anchor: anchorHour, liveUntil: liveUntil, enabled: single != nil) { h in
                    pickHour(h, day: single)
                }
                .frame(width: 132, height: 132)
                VStack(alignment: .leading, spacing: 6) {
                    Text(label)
                        .scaledFont(.headline)
                        .monospacedDigit()
                        .rollingText(down: model.rangeRollsDown)
                    Text(single == nil
                         ? "Select a single day to narrow it to hours."
                         : anchorHour == nil ? "Click a start hour, then an end hour." : "Now click the other end.")
                        .scaledFont(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(3, reservesSpace: true)
                        .contentTransition(.opacity)
                        .animation(Motion.hover, value: anchorHour)
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
        guard let base = shownMonth else { return }
        var m = base.month + delta, y = base.year
        while m < 1 { m += 12; y -= 1 }
        while m > 12 { m -= 12; y += 1 }
        showMonth(of: CivilDay(year: y, month: m, day: 1))
    }

    /// Page the grid to `day`'s month, sliding in the direction of travel.
    private func showMonth(of day: CivilDay?) {
        guard let day else { return }
        let target = Self.monthOf(day)
        guard target != shownMonth else { return }
        monthForward = target > (shownMonth ?? target)
        withAnimation(reduceMotion ? .easeInOut(duration: 0.15) : Motion.page) {
            shownMonth = target
        }
    }

    private static func monthOf(_ d: CivilDay) -> CivilDay {
        CivilDay(year: d.year, month: d.month, day: 1)
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

/// Fixed cell geometry, so the selection layer can be laid out by arithmetic
/// (and animate as plain frames) instead of measuring cells.
private enum CalendarMetrics {
    static let cellW: CGFloat = 34
    static let cellH: CGFloat = 30
    static let gap: CGFloat = 2
    static let rows = 6
    static var width: CGFloat { 7 * cellW + 6 * gap }
    static var height: CGFloat { CGFloat(rows) * cellH + CGFloat(rows - 1) * gap }

    static func origin(_ index: Int) -> CGPoint {
        CGPoint(x: CGFloat(index % 7) * (cellW + gap), y: CGFloat(index / 7) * (cellH + gap))
    }
}

private struct MonthGrid: View {
    let month: CivilDay  // day == 1
    let forward: Bool
    let today: CivilDay
    let selection: (CivilDay, CivilDay)?
    let anchor: CivilDay?
    let pick: (CivilDay) -> Void
    let step: (Int) -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var cal: Calendar { CivilDay.gregorian }

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
                    .scaledFont(.headline)
                    .monospacedDigit()
                    .rollingText(down: !forward)
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
            HStack(spacing: CalendarMetrics.gap) {
                ForEach(Array(weekdaySymbols.enumerated()), id: \.offset) { _, s in
                    Text(s).scaledFont(.caption).foregroundStyle(.secondary)
                        .frame(width: CalendarMetrics.cellW)
                }
            }
            // Only the day grid pages; header and weekday row stay put.
            ZStack {
                MonthPage(month: month, today: today, selection: selection, anchor: anchor, pick: pick)
                    .id(month)
                    .transition(.slideFade(from: forward ? .trailing : .leading,
                                           distance: CalendarMetrics.width * 0.35,
                                           reduced: reduceMotion))
            }
            .frame(width: CalendarMetrics.width, height: CalendarMetrics.height, alignment: .topLeading)
            .clipped()
        }
        .frame(width: CalendarMetrics.width)
    }
}

/// One month: the selection layer (row bands + two endpoint markers) under
/// 42 fixed day cells. The layer is a handful of views with stable identity,
/// so a new pick animates their frames — bands stretch along the rows and the
/// markers glide to their new days — while the cells only cross-fade colour.
private struct MonthPage: View {
    let month: CivilDay
    let today: CivilDay
    let selection: (CivilDay, CivilDay)?
    let anchor: CivilDay?
    let pick: (CivilDay) -> Void

    private var cal: Calendar { CivilDay.gregorian }

    private var lead: Int {
        (cal.component(.weekday, from: month.date) - cal.firstWeekday + 7) % 7
    }

    private var count: Int {
        cal.range(of: .day, in: .month, for: month.date)?.count ?? 30
    }

    private func index(of d: CivilDay) -> Int { lead + d.day - 1 }

    /// Selection clipped to this month, as cell indices (nil if disjoint).
    private var span: (lo: Int, hi: Int, startIn: Bool, endIn: Bool)? {
        guard let (a, b) = selection else { return nil }
        let first = month, last = CivilDay(year: month.year, month: month.month, day: count)
        guard b >= first, a <= last else { return nil }
        let lo = a < first ? lead : index(of: a)
        let hi = b > last ? lead + count - 1 : index(of: b)
        return (lo, hi, a >= first, b <= last)
    }

    var body: some View {
        let s = span
        ZStack(alignment: .topLeading) {
            // Row bands (6, stable ids). An uncovered row collapses to a
            // zero-width band at the edge nearest the selection, so bands
            // grow and shrink in reading order.
            ForEach(0..<CalendarMetrics.rows, id: \.self) { row in
                band(row: row, span: s)
            }
            // Endpoint markers glide between days; hidden ones park at the
            // nearest month edge so they slide in from the right side.
            marker(index: s?.lo ?? anchorIndex ?? 0, visible: s?.startIn ?? false)
            marker(index: s?.hi ?? anchorIndex ?? 0, visible: s?.endIn ?? false)
            // Day cells on top.
            ForEach(0..<count, id: \.self) { i in
                let day = CivilDay(year: month.year, month: month.month, day: i + 1)
                let idx = index(of: day)
                let endpoint = s.map { (idx == $0.lo && $0.startIn) || (idx == $0.hi && $0.endIn) } ?? false
                let inside = s.map { idx >= $0.lo && idx <= $0.hi } ?? false
                DayCell(day: day, today: today, endpoint: endpoint, inside: inside, pick: pick)
                    .frame(width: CalendarMetrics.cellW, height: CalendarMetrics.cellH)
                    .offset(x: CalendarMetrics.origin(idx).x, y: CalendarMetrics.origin(idx).y)
            }
        }
        .frame(width: CalendarMetrics.width, height: CalendarMetrics.height, alignment: .topLeading)
    }

    private var anchorIndex: Int? {
        guard let a = anchor, a.year == month.year, a.month == month.month else { return nil }
        return index(of: a)
    }

    private func band(row: Int, span s: (lo: Int, hi: Int, startIn: Bool, endIn: Bool)?) -> some View {
        let rowLo = row * 7, rowHi = row * 7 + 6
        var x: CGFloat = 0, width: CGFloat = 0, visible = false
        if let s {
            let lo = max(s.lo, rowLo), hi = min(s.hi, rowHi)
            if lo <= hi {
                x = CalendarMetrics.origin(lo).x
                width = CalendarMetrics.origin(hi).x - x + CalendarMetrics.cellW
                visible = true
            } else {
                // Before the selection → park at the row's end; after → start.
                x = rowHi < s.lo ? CalendarMetrics.origin(rowHi).x + CalendarMetrics.cellW : 0
            }
        }
        return RoundedRectangle(cornerRadius: 8, style: .continuous)
            .fill(Color.accentColor.opacity(0.2))
            .frame(width: width, height: CalendarMetrics.cellH)
            .offset(x: x, y: CGFloat(row) * (CalendarMetrics.cellH + CalendarMetrics.gap))
            .opacity(visible ? 1 : 0)
            .allowsHitTesting(false)
    }

    private func marker(index: Int, visible: Bool) -> some View {
        let o = CalendarMetrics.origin(index)
        return RoundedRectangle(cornerRadius: 8, style: .continuous)
            .fill(Color.accentColor)
            .frame(width: CalendarMetrics.cellW, height: CalendarMetrics.cellH)
            .shadow(color: Color.accentColor.opacity(0.35), radius: 4, y: 1)
            .scaleEffect(visible ? 1 : 0.6)
            .opacity(visible ? 1 : 0)
            .offset(x: o.x, y: o.y)
            .allowsHitTesting(false)
    }
}

/// A day: plain digits over the selection layer, with hover and press
/// feedback. Colour changes ride the selection spring.
private struct DayCell: View {
    let day: CivilDay
    let today: CivilDay
    let endpoint: Bool
    let inside: Bool
    let pick: (CivilDay) -> Void
    @ViewState private var hovering = false

    var body: some View {
        let future = day > today
        Button {
            pick(day)
        } label: {
            Text("\(day.day)")
                .monospacedDigit()
                .fontWeight(endpoint ? .semibold : .regular)
                .foregroundStyle(endpoint ? Color.white : future ? Color.secondary.opacity(0.5) : Color.primary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background {
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .fill(Color.primary.opacity(hovering && !endpoint && !future ? 0.07 : 0))
                }
                .overlay {
                    if day == today && !endpoint {
                        RoundedRectangle(cornerRadius: 8, style: .continuous)
                            .strokeBorder(Color.accentColor.opacity(0.7), lineWidth: 1)
                    }
                }
                .contentShape(.rect)
        }
        .buttonStyle(PressScale())
        .disabled(future)
        .onHover { h in
            withAnimation(Motion.hover) { hovering = h }
        }
        .accessibilityLabel(Text(day.date, format: .dateTime.year().month(.wide).day()))
        .accessibilityAddTraits(endpoint || inside ? .isSelected : [])
    }
}

/// Springy press: shrinks slightly while held, bounces back on release.
private struct PressScale: ButtonStyle {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed && !reduceMotion ? 0.88 : 1)
            .animation(Motion.press, value: configuration.isPressed)
    }
}

// MARK: - Hour dial

/// 24-hour clock face (00 at the top, clockwise). The static face (track,
/// ticks, labels) is drawn once and skipped on re-render; the selected arc
/// is an animatable shape, so a new pick grows/shrinks it around the dial,
/// the pending first click rides a rotating marker, and hovering previews
/// the slot under the pointer.
struct HourDial: View {
    let span: [Int]?
    let anchor: Int?
    let liveUntil: Int
    let enabled: Bool
    let pick: (Int) -> Void

    var body: some View {
        GeometryReader { geo in
            let size = min(geo.size.width, geo.size.height)
            let ring = size / 2 - 10
            ZStack {
                DialFace(ring: ring, liveUntil: liveUntil, enabled: enabled)
                    .equatable()
                if enabled, let s = span, s.count == 2 {
                    HourArc(start: Double(s[0]), end: Double(s[1] + 1), ring: ring)
                        .stroke(Color.accentColor, style: StrokeStyle(lineWidth: 12, lineCap: .round))
                        .transition(.opacity)
                }
                if let h = anchor {
                    // Rotating the marker (not moving it) keeps it on the ring
                    // while it animates to a new hour.
                    Circle()
                        .fill(.white)
                        .overlay(Circle().strokeBorder(Color.accentColor, lineWidth: 2))
                        .frame(width: 11, height: 11)
                        .offset(y: -ring)
                        .rotationEffect(.degrees((Double(h) + 0.5) * 15))
                        .transition(.scale.combined(with: .opacity))
                }
                DialHover(ring: ring, liveUntil: liveUntil, enabled: enabled, pick: pick)
            }
            .frame(width: size, height: size)
            .opacity(enabled ? 1 : 0.55)
            .animation(Motion.hover, value: enabled)
        }
        .accessibilityElement()
        .accessibilityLabel("Hour range")
        .accessibilityValue(span.map { "\($0[0]):00 to \($0[1] + 1):00" } ?? "")
    }
}

/// Arc over hour slots [start, end) — angles animate as one pair.
private struct HourArc: Shape {
    var start: Double
    var end: Double
    let ring: CGFloat

    var animatableData: AnimatablePair<Double, Double> {
        get { AnimatablePair(start, end) }
        set { start = newValue.first; end = newValue.second }
    }

    func path(in rect: CGRect) -> Path {
        var p = Path()
        p.addArc(center: CGPoint(x: rect.midX, y: rect.midY), radius: ring,
                 startAngle: .degrees(start * 15 - 90), endAngle: .degrees(end * 15 - 90),
                 clockwise: false)
        return p
    }
}

/// Track, ticks and hour labels — redrawn only when its inputs change.
private struct DialFace: View, Equatable {
    let ring: CGFloat
    let liveUntil: Int
    let enabled: Bool

    var body: some View {
        Canvas(rendersAsynchronously: true) { ctx, size in
            let c = CGPoint(x: size.width / 2, y: size.height / 2)
            ctx.stroke(Path(ellipseIn: CGRect(x: c.x - ring, y: c.y - ring, width: ring * 2, height: ring * 2)),
                       with: .color(.secondary.opacity(enabled ? 0.18 : 0.08)), lineWidth: 12)
            for h in 0..<24 {
                let a = Angle.degrees((Double(h) + 0.5) * 15 - 90).radians
                let r1 = ring - 12, r2 = ring - (h % 3 == 0 ? 20 : 16)
                var t = Path()
                t.move(to: CGPoint(x: c.x + cos(a) * r1, y: c.y + sin(a) * r1))
                t.addLine(to: CGPoint(x: c.x + cos(a) * r2, y: c.y + sin(a) * r2))
                let live = enabled && h <= liveUntil
                ctx.stroke(t, with: .color(live ? .primary.opacity(0.55) : .secondary.opacity(0.25)), lineWidth: 1)
            }
            for h in [0, 6, 12, 18] {
                let a = Angle.degrees(Double(h) * 15 - 90).radians
                let p = CGPoint(x: c.x + cos(a) * (ring - 30), y: c.y + sin(a) * (ring - 30))
                ctx.draw(Text(String(format: "%02d", h)).font(.caption2.monospacedDigit()).foregroundStyle(.secondary),
                         at: p)
            }
        }
    }
}

/// Pointer layer: hit-testing by angle, a ghost segment that follows the
/// hovered slot, and its time in the dial's centre. Owns its hover state so
/// pointer moves re-render only this layer.
private struct DialHover: View {
    let ring: CGFloat
    let liveUntil: Int
    let enabled: Bool
    let pick: (Int) -> Void
    @ViewState private var hovered: Int?

    var body: some View {
        GeometryReader { geo in
            let c = CGPoint(x: geo.size.width / 2, y: geo.size.height / 2)
            ZStack {
                if let h = hovered {
                    HourArc(start: Double(h) + 0.15, end: Double(h) + 0.85, ring: ring)
                        .stroke(Color.accentColor.opacity(0.35), style: StrokeStyle(lineWidth: 12, lineCap: .round))
                    Text(CustomRangeButton.hh(h))
                        .scaledFont(.caption, weight: .semibold).monospacedDigit()
                        .foregroundStyle(Color.accentColor)
                        .contentTransition(.numericText(value: Double(h)))
                }
            }
            .frame(width: geo.size.width, height: geo.size.height)
            .contentShape(.circle)
            .onContinuousHover { phase in
                switch phase {
                case .active(let p):
                    let h = slot(at: p, center: c)
                    let next = enabled && h <= liveUntil ? h : nil
                    if next != hovered { withAnimation(Motion.hover) { hovered = next } }
                case .ended:
                    withAnimation(Motion.hover) { hovered = nil }
                }
            }
            .onTapGesture(coordinateSpace: .local) { p in
                guard enabled else { return }
                let h = slot(at: p, center: c)
                if h <= liveUntil { pick(h) }
            }
        }
    }

    /// Angle from 12 o'clock, clockwise → slot 0..23.
    private func slot(at p: CGPoint, center c: CGPoint) -> Int {
        var deg = atan2(p.y - c.y, p.x - c.x) * 180 / .pi + 90
        if deg < 0 { deg += 360 }
        return min(23, Int(deg / 15))
    }
}
