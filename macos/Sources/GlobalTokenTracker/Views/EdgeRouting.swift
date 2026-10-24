// Card-avoiding edge routes for the workflow canvas — the same algorithm as
// the Windows build (crates/ui/src/edge_route.rs), so both platforms draw a
// workflow the same way. A route is an orthogonal polyline found on a sparse
// grid built from the cards' padded edges: it never crosses any card and
// prefers few turns. Drawing rounds every corner, which keeps lines smooth
// while they still run in clean lanes the eye can follow. All coordinates are
// canvas world units, before pan and zoom.
import CoreGraphics
import SwiftUI

enum EdgeRouting {
    static let cardWidth: CGFloat = 220
    static let cardHeight: CGFloat = 112
    /// (clearance around every card, straight run out of and into a port),
    /// tried in order; the tighter pairs fit cards set closer than usual.
    private static let clearances: [(pad: CGFloat, stub: CGFloat)] = [(14, 22), (6, 10), (1, 2)]
    /// Cost of one turn, in units of path length.
    private static let bend: CGFloat = 60
    /// Corner radius when drawing.
    static let radius: CGFloat = 14
    /// Cards farther than this from the ports cannot shape the route.
    private static let reach: CGFloat = 440

    /// A port on a card side; `out` is +1 for the right side, -1 for the left.
    struct Port {
        var at: CGPoint
        var out: CGFloat
    }

    private struct Box {
        var l: CGFloat, t: CGFloat, r: CGFloat, b: CGFloat
        init(card: CGPoint, pad: CGFloat) {
            l = card.x - pad; t = card.y - pad
            r = card.x + EdgeRouting.cardWidth + pad; b = card.y + EdgeRouting.cardHeight + pad
        }
        func inside(_ p: CGPoint) -> Bool { l < p.x && p.x < r && t < p.y && p.y < b }
        /// Whether an axis-aligned segment passes through the open interior.
        func crossed(_ a: CGPoint, _ c: CGPoint) -> Bool {
            if a.y == c.y { return t < a.y && a.y < b && max(min(a.x, c.x), l) < min(max(a.x, c.x), r) }
            return l < a.x && a.x < r && max(min(a.y, c.y), t) < min(max(a.y, c.y), b)
        }
    }

    private struct Heap {
        var items: [(cost: CGFloat, at: Int)] = []
        mutating func push(_ cost: CGFloat, _ at: Int) {
            items.append((cost, at))
            var i = items.count - 1
            while i > 0 {
                let p = (i - 1) / 2
                if items[p].cost <= items[i].cost { break }
                items.swapAt(p, i); i = p
            }
        }
        mutating func pop() -> (cost: CGFloat, at: Int)? {
            guard !items.isEmpty else { return nil }
            items.swapAt(0, items.count - 1)
            let top = items.removeLast()
            var i = 0
            while true {
                let l = 2 * i + 1, r = l + 1
                var m = i
                if l < items.count && items[l].cost < items[m].cost { m = l }
                if r < items.count && items[r].cost < items[m].cost { m = r }
                if m == i { break }
                items.swapAt(i, m); i = m
            }
            return top
        }
    }

    /// Route from `from` to `to` around `cards` (top-left corners, may include
    /// the two endpoint cards); the polyline includes both ports.
    static func route(from: Port, to: Port, cards: [CGPoint]) -> [CGPoint] {
        for c in clearances {
            if let path = route(from: from, to: to, cards: cards, pad: c.pad, stub: c.stub) { return path }
        }
        let stub = clearances[0].stub
        return simplify([from.at, CGPoint(x: from.at.x + from.out * stub, y: from.at.y),
                         CGPoint(x: to.at.x + to.out * stub, y: to.at.y), to.at])
    }

    /// Sorted unique coordinates plus every gap's midpoint, so routes can run
    /// down the middle of a channel instead of hugging a card.
    private static func axis(_ values: [CGFloat]) -> [CGFloat] {
        var unique: [CGFloat] = []
        for x in values.sorted() where unique.last.map({ abs($0 - x) >= 0.5 }) ?? true { unique.append(x) }
        let mids = zip(unique, unique.dropFirst()).map { ($0 + $1) / 2 }
        return (unique + mids).sorted()
    }

    private static func route(from: Port, to: Port, cards: [CGPoint], pad: CGFloat, stub: CGFloat) -> [CGPoint]? {
        let start = CGPoint(x: from.at.x + from.out * stub, y: from.at.y)
        let end = CGPoint(x: to.at.x + to.out * stub, y: to.at.y)
        let loX = min(start.x, end.x) - reach, hiX = max(start.x, end.x) + reach
        let loY = min(start.y, end.y) - reach, hiY = max(start.y, end.y) + reach
        let boxes = cards.map { Box(card: $0, pad: pad) }.filter { $0.r > loX && $0.l < hiX && $0.b > loY && $0.t < hiY }
        if boxes.contains(where: { $0.inside(start) || $0.inside(end) }) { return nil }
        var xs: [CGFloat] = [start.x, end.x], ys: [CGFloat] = [start.y, end.y]
        for box in boxes { xs += [box.l, box.r]; ys += [box.t, box.b] }
        xs = axis(xs); ys = axis(ys)
        let nx = xs.count, ny = ys.count
        func point(_ i: Int) -> CGPoint { CGPoint(x: xs[i % nx], y: ys[i / nx]) }
        let free = (0..<(nx * ny)).map { i in !boxes.contains { $0.inside(point(i)) } }
        func index(_ p: CGPoint) -> Int? {
            guard let x = xs.firstIndex(where: { abs($0 - p.x) < 0.5 }),
                  let y = ys.firstIndex(where: { abs($0 - p.y) < 0.5 }) else { return nil }
            return y * nx + x
        }
        guard let source = index(start), let goal = index(end) else { return nil }
        // State = grid point × heading (0 horizontal, 1 vertical); both stubs
        // are horizontal, so the search starts and should finish horizontally.
        var best = [CGFloat](repeating: .infinity, count: nx * ny * 2)
        var prev = [Int](repeating: -1, count: nx * ny * 2)
        var heap = Heap()
        best[source * 2] = 0
        heap.push(0, source * 2)
        while let top = heap.pop() {
            let cost = top.cost, at = top.at
            if cost > best[at] { continue }
            let cell = at / 2, heading = at % 2
            if cell == goal && heading == 0 { break }
            let x = cell % nx, y = cell / nx
            var moves: [(Int, Int)] = []
            if x > 0 { moves.append((cell - 1, 0)) }
            if x + 1 < nx { moves.append((cell + 1, 0)) }
            if y > 0 { moves.append((cell - nx, 1)) }
            if y + 1 < ny { moves.append((cell + nx, 1)) }
            for (next, dir) in moves where free[next] {
                let a = point(cell), b = point(next)
                if boxes.contains(where: { $0.crossed(a, b) }) { continue }
                let total = cost + abs(a.x - b.x) + abs(a.y - b.y) + (dir == heading ? 0 : bend)
                let state = next * 2 + dir
                if total < best[state] { best[state] = total; prev[state] = at; heap.push(total, state) }
            }
        }
        let arrive = best[goal * 2] <= best[goal * 2 + 1] + bend ? goal * 2 : goal * 2 + 1
        guard best[arrive].isFinite else { return nil }
        var path = [to.at]
        var at = arrive
        while true {
            path.append(point(at / 2))
            if at == source * 2 || prev[at] < 0 { break }
            at = prev[at]
        }
        path.append(from.at)
        return simplify(path.reversed())
    }

    /// Drops repeated and collinear points.
    private static func simplify(_ points: [CGPoint]) -> [CGPoint] {
        var out: [CGPoint] = []
        for p in points {
            if let q = out.last, abs(q.x - p.x) < 0.01, abs(q.y - p.y) < 0.01 { continue }
            if out.count >= 2 {
                let a = out[out.count - 2], b = out[out.count - 1]
                if abs((b.x - a.x) * (p.y - b.y) - (b.y - a.y) * (p.x - b.x)) < 0.01 { out.removeLast() }
            }
            out.append(p)
        }
        return out
    }

    /// Corner geometry shared by drawing and dot placement: where the line
    /// leaves the straight leg, the corner (curve control) and where it rejoins.
    private static func corners(_ points: [CGPoint]) -> [(enter: CGPoint, corner: CGPoint, leave: CGPoint)?] {
        guard points.count > 2 else { return [] }
        return (1..<(points.count - 1)).map { i in
            let a = points[i - 1], p = points[i], n = points[i + 1]
            let lenIn = hypot(p.x - a.x, p.y - a.y), lenOut = hypot(n.x - p.x, n.y - p.y)
            let r = min(radius, lenIn / 2, lenOut / 2)
            guard r >= 0.5 else { return nil }
            return (CGPoint(x: p.x + (a.x - p.x) * r / lenIn, y: p.y + (a.y - p.y) * r / lenIn), p,
                    CGPoint(x: p.x + (n.x - p.x) * r / lenOut, y: p.y + (n.y - p.y) * r / lenOut))
        }
    }

    /// The route with every corner rounded.
    static func roundedPath(_ points: [CGPoint]) -> Path {
        var path = Path()
        guard let first = points.first else { return path }
        path.move(to: first)
        for (i, c) in corners(points).enumerated() {
            if let c { path.addLine(to: c.enter); path.addQuadCurve(to: c.leave, control: c.corner) }
            else { path.addLine(to: points[i + 1]) }
        }
        if let last = points.last, points.count > 1 { path.addLine(to: last) }
        return path
    }

    /// The rounded route as short straight pieces, for even dot spacing.
    static func flatten(_ points: [CGPoint]) -> [CGPoint] {
        guard let first = points.first else { return [] }
        var out = [first]
        for (i, c) in corners(points).enumerated() {
            guard let c else { out.append(points[i + 1]); continue }
            out.append(c.enter)
            for k in 1...6 {
                let t = CGFloat(k) / 6, u = 1 - t
                out.append(CGPoint(x: u * u * c.enter.x + 2 * u * t * c.corner.x + t * t * c.leave.x,
                                   y: u * u * c.enter.y + 2 * u * t * c.corner.y + t * t * c.leave.y))
            }
        }
        if let last = points.last, points.count > 1 { out.append(last) }
        return out
    }

    /// Points every `spacing` along `line`, shifted forward by `phase`; a
    /// growing phase moves them from the start toward the end.
    static func dots(_ line: [CGPoint], spacing: CGFloat, phase: CGFloat) -> [CGPoint] {
        guard line.count > 1, spacing > 0 else { return [] }
        var out: [CGPoint] = []
        var next = phase.truncatingRemainder(dividingBy: spacing)
        if next < 0 { next += spacing }
        var walked: CGFloat = 0
        for (a, b) in zip(line, line.dropFirst()) {
            let len = hypot(b.x - a.x, b.y - a.y)
            while len > 0 && next <= walked + len {
                let t = (next - walked) / len
                out.append(CGPoint(x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t))
                next += spacing
            }
            walked += len
        }
        return out
    }
}
