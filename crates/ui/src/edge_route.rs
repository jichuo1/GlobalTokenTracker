//! Card-avoiding edge routes for the workflow canvas.
//!
//! A route is an orthogonal polyline found on a sparse grid built from the
//! cards' (padded) edges, so it never crosses a card — not just its own two
//! cards — and prefers few turns. Drawing rounds every corner into a short
//! curve, which keeps the line smooth while it still runs in clean horizontal
//! and vertical lanes that the eye can follow. All coordinates are canvas
//! world units (the card size below), before pan and zoom.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

pub const CARD_W: f64 = 220.0;
pub const CARD_H: f64 = 112.0;
/// (clearance kept around every card, straight run out of and into a port)
/// tried in order: the stubs make lines leave and enter cards head-on with
/// the arrow on a straight piece; tighter pairs fit cards set closer than the
/// usual clearance, where the wide stubs would start inside a neighbour.
const CLEARANCES: [(f64, f64); 3] = [(14.0, 22.0), (6.0, 10.0), (1.0, 2.0)];
/// Cost of one turn, in units of path length: a route takes a detour of up to
/// this much rather than add a bend.
const BEND: f64 = 60.0;
/// Corner radius when drawing.
pub const RADIUS: f64 = 14.0;
/// Cards farther than this from the box spanned by the two ports cannot
/// shape the route; leaving them out keeps the grid small while dragging.
const REACH: f64 = 2.0 * CARD_W;

/// Which side of a card a port sits on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    fn out(self) -> f64 {
        match self {
            Side::Left => -1.0,
            Side::Right => 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Port {
    pub at: (f64, f64),
    pub side: Side,
}

#[derive(Clone, Copy)]
struct Rect {
    l: f64,
    t: f64,
    r: f64,
    b: f64,
}

impl Rect {
    fn card(at: (f64, f64), pad: f64) -> Self {
        Self {
            l: at.0 - pad,
            t: at.1 - pad,
            r: at.0 + CARD_W + pad,
            b: at.1 + CARD_H + pad,
        }
    }
    fn inside(&self, p: (f64, f64)) -> bool {
        self.l < p.0 && p.0 < self.r && self.t < p.1 && p.1 < self.b
    }
    /// Whether an axis-aligned segment passes through the open interior.
    fn crossed_by(&self, a: (f64, f64), b: (f64, f64)) -> bool {
        if a.1 == b.1 {
            self.t < a.1 && a.1 < self.b && a.0.min(b.0).max(self.l) < a.0.max(b.0).min(self.r)
        } else {
            self.l < a.0 && a.0 < self.r && a.1.min(b.1).max(self.t) < a.1.max(b.1).min(self.b)
        }
    }
}

/// Sorted, de-duplicated coordinates plus the midpoint of every gap, so a
/// route can run down the middle of a channel instead of hugging a card.
fn axis(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_by(f64::total_cmp);
    values.dedup_by(|a, b| (*a - *b).abs() < 0.5);
    let mids: Vec<f64> = values.windows(2).map(|w| w[0].midpoint(w[1])).collect();
    values.extend(mids);
    values.sort_by(f64::total_cmp);
    values
}

#[derive(PartialEq)]
struct Visit {
    cost: f64,
    at: usize,
}
impl Eq for Visit {}
impl Ord for Visit {
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.total_cmp(&self.cost)
    }
}
impl PartialOrd for Visit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Route from `from` to `to` around `cards` (top-left corners; may include
/// the two endpoint cards). Returns the polyline including both ports. When
/// no clear route exists (cards overlapping a port) the result is the plain
/// four-point stub path, which the caller may still draw.
pub fn route(from: Port, to: Port, cards: &[(f64, f64)]) -> Vec<(f64, f64)> {
    for (pad, stub) in CLEARANCES {
        if let Some(path) = route_with(from, to, cards, pad, stub) {
            return path;
        }
    }
    let (_, stub) = CLEARANCES[0];
    simplify(vec![
        from.at,
        (from.at.0 + from.side.out() * stub, from.at.1),
        (to.at.0 + to.side.out() * stub, to.at.1),
        to.at,
    ])
}

fn route_with(
    from: Port,
    to: Port,
    cards: &[(f64, f64)],
    pad: f64,
    stub: f64,
) -> Option<Vec<(f64, f64)>> {
    let start = (from.at.0 + from.side.out() * stub, from.at.1);
    let end = (to.at.0 + to.side.out() * stub, to.at.1);
    let (lo_x, hi_x) = (start.0.min(end.0) - REACH, start.0.max(end.0) + REACH);
    let (lo_y, hi_y) = (start.1.min(end.1) - REACH, start.1.max(end.1) + REACH);
    let obstacles: Vec<Rect> = cards
        .iter()
        .map(|c| Rect::card(*c, pad))
        .filter(|r| r.r > lo_x && r.l < hi_x && r.b > lo_y && r.t < hi_y)
        .collect();
    if obstacles.iter().any(|r| r.inside(start) || r.inside(end)) {
        return None;
    }
    let mut xs = vec![start.0, end.0];
    let mut ys = vec![start.1, end.1];
    for r in &obstacles {
        xs.extend([r.l, r.r]);
        ys.extend([r.t, r.b]);
    }
    let (xs, ys) = (axis(xs), axis(ys));
    let (nx, ny) = (xs.len(), ys.len());
    let point = |i: usize| (xs[i % nx], ys[i / nx]);
    let free: Vec<bool> = (0..nx * ny)
        .map(|i| !obstacles.iter().any(|r| r.inside(point(i))))
        .collect();
    let index = |p: (f64, f64)| {
        let x = xs.iter().position(|v| (*v - p.0).abs() < 0.5)?;
        let y = ys.iter().position(|v| (*v - p.1).abs() < 0.5)?;
        Some(y * nx + x)
    };
    let (source, goal) = (index(start)?, index(end)?);
    // State = grid point × heading (0 horizontal, 1 vertical); both stubs are
    // horizontal, so the search starts and must finish heading horizontally.
    let states = nx * ny * 2;
    let mut best = vec![f64::INFINITY; states];
    let mut prev = vec![usize::MAX; states];
    let mut heap = BinaryHeap::new();
    best[source * 2] = 0.0;
    heap.push(Visit {
        cost: 0.0,
        at: source * 2,
    });
    let clear = |a: (f64, f64), b: (f64, f64)| !obstacles.iter().any(|r| r.crossed_by(a, b));
    while let Some(Visit { cost, at }) = heap.pop() {
        if cost > best[at] {
            continue;
        }
        let (cell, heading) = (at / 2, at % 2);
        if cell == goal && heading == 0 {
            break;
        }
        let (x, y) = (cell % nx, cell / nx);
        let mut step = |next: usize, dir: usize| {
            if !free[next] {
                return;
            }
            let (a, b) = (point(cell), point(next));
            if !clear(a, b) {
                return;
            }
            let length = (a.0 - b.0).abs() + (a.1 - b.1).abs();
            let turn = if dir == heading { 0.0 } else { BEND };
            let state = next * 2 + dir;
            let total = cost + length + turn;
            if total < best[state] {
                best[state] = total;
                prev[state] = at;
                heap.push(Visit {
                    cost: total,
                    at: state,
                });
            }
        };
        if x > 0 {
            step(cell - 1, 0);
        }
        if x + 1 < nx {
            step(cell + 1, 0);
        }
        if y > 0 {
            step(cell - nx, 1);
        }
        if y + 1 < ny {
            step(cell + nx, 1);
        }
    }
    // Arriving vertically costs one more turn into the final stub.
    let arrive = if best[goal * 2] <= best[goal * 2 + 1] + BEND {
        goal * 2
    } else {
        goal * 2 + 1
    };
    if !best[arrive].is_finite() {
        return None;
    }
    let mut path = vec![to.at];
    let mut at = arrive;
    loop {
        path.push(point(at / 2));
        if at == source * 2 || prev[at] == usize::MAX {
            break;
        }
        at = prev[at];
    }
    path.push(from.at);
    path.reverse();
    Some(simplify(path))
}

/// Drops repeated and collinear points.
fn simplify(points: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(points.len());
    for p in points {
        if out
            .last()
            .is_some_and(|q| (q.0 - p.0).abs() < 0.01 && (q.1 - p.1).abs() < 0.01)
        {
            continue;
        }
        if out.len() >= 2 {
            let (a, b) = (out[out.len() - 2], out[out.len() - 1]);
            let cross = (b.0 - a.0) * (p.1 - b.1) - (b.1 - a.1) * (p.0 - b.0);
            if cross.abs() < 0.01 {
                out.pop();
            }
        }
        out.push(p);
    }
    out
}

/// One drawing step after the start point.
#[derive(Clone, Copy, Debug)]
pub enum Seg {
    Line((f64, f64)),
    /// Cubic Bézier: two controls, then the end point.
    Curve((f64, f64), (f64, f64), (f64, f64)),
}

/// Rounds every corner of `points` with a quarter-curve of up to `RADIUS`
/// (less where a leg is short, so short legs never overshoot).
pub fn rounded(points: &[(f64, f64)]) -> Vec<Seg> {
    let mut segs = Vec::with_capacity(points.len() * 2);
    for i in 1..points.len() {
        let p = points[i];
        if i + 1 == points.len() {
            segs.push(Seg::Line(p));
            break;
        }
        let (a, n) = (points[i - 1], points[i + 1]);
        let (len_in, len_out) = (distance(a, p), distance(p, n));
        let r = RADIUS.min(len_in / 2.0).min(len_out / 2.0);
        if r < 0.5 {
            segs.push(Seg::Line(p));
            continue;
        }
        let enter = lerp(p, a, r / len_in);
        let leave = lerp(p, n, r / len_out);
        // A quadratic through the corner, written as the equivalent cubic.
        let c1 = lerp(enter, p, 2.0 / 3.0);
        let c2 = lerp(leave, p, 2.0 / 3.0);
        segs.push(Seg::Line(enter));
        segs.push(Seg::Curve(c1, c2, leave));
    }
    segs
}

/// The rounded path flattened into short pieces, for placing flow dots at
/// even arc-length spacing.
pub fn flatten(start: (f64, f64), segs: &[Seg]) -> Vec<(f64, f64)> {
    let mut out = vec![start];
    let mut at = start;
    for seg in segs {
        match *seg {
            Seg::Line(p) => {
                out.push(p);
                at = p;
            }
            Seg::Curve(c1, c2, p) => {
                for k in 1..=6 {
                    let t = f64::from(k) / 6.0;
                    let u = 1.0 - t;
                    out.push((
                        u * u * u * at.0
                            + 3.0 * u * u * t * c1.0
                            + 3.0 * u * t * t * c2.0
                            + t * t * t * p.0,
                        u * u * u * at.1
                            + 3.0 * u * u * t * c1.1
                            + 3.0 * u * t * t * c2.1
                            + t * t * t * p.1,
                    ));
                }
                at = p;
            }
        }
    }
    out
}

/// Points every `spacing` along `line`, shifted forward by `phase`; with a
/// phase that grows over time the points travel from the start to the end.
pub fn dots(line: &[(f64, f64)], spacing: f64, phase: f64) -> Vec<(f64, f64)> {
    let total: f64 = line.windows(2).map(|w| distance(w[0], w[1])).sum();
    if total <= 1.0 || spacing <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut next = phase.rem_euclid(spacing);
    let mut walked = 0.0;
    for w in line.windows(2) {
        let len = distance(w[0], w[1]);
        while next <= walked + len && len > 0.0 {
            out.push(lerp(w[0], w[1], (next - walked) / len));
            next += spacing;
        }
        walked += len;
    }
    out
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn lerp(a: (f64, f64), b: (f64, f64), t: f64) -> (f64, f64) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// Windows' "Animation effects" setting (Accessibility › Visual effects).
/// When it is off the flow dots stand still; arrows still show direction.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn system_animations_enabled() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };
    let mut on: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL into `on`.
    let ok =
        unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut on).cast(), 0) };
    ok == 0 || on != 0
}

#[cfg(not(windows))]
pub fn system_animations_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crosses_any(path: &[(f64, f64)], cards: &[(f64, f64)]) -> bool {
        // The exact card rectangles (no padding): a route may run along the
        // padding, never through a card.
        path.windows(2).any(|w| {
            cards.iter().any(|c| {
                let r = Rect {
                    l: c.0,
                    t: c.1,
                    r: c.0 + CARD_W,
                    b: c.1 + CARD_H,
                };
                r.crossed_by(w[0], w[1])
            })
        })
    }

    fn task(from: (f64, f64), to: (f64, f64)) -> (Port, Port) {
        (
            Port {
                at: (from.0 + CARD_W, from.1 + 44.0),
                side: Side::Right,
            },
            Port {
                at: (to.0, to.1 + 44.0),
                side: Side::Left,
            },
        )
    }

    fn report(from: (f64, f64), to: (f64, f64)) -> (Port, Port) {
        (
            Port {
                at: (from.0, from.1 + 80.0),
                side: Side::Left,
            },
            Port {
                at: (to.0 + CARD_W, to.1 + 80.0),
                side: Side::Right,
            },
        )
    }

    #[test]
    fn routes_start_and_end_head_on_and_never_cross_any_card() {
        // (cards, source index, target index)
        type Layout<'a> = (&'a [(f64, f64)], usize, usize);
        let layouts: &[Layout] = &[
            // target right of source
            (&[(0.0, 0.0), (400.0, 0.0)], 0, 1),
            // target left of source: must loop around both cards
            (&[(400.0, 0.0), (0.0, 0.0)], 0, 1),
            // a third card squarely between the two
            (&[(0.0, 0.0), (300.0, 0.0), (600.0, 0.0)], 0, 2),
            // vertically stacked
            (&[(0.0, 0.0), (0.0, 300.0)], 0, 1),
            // the screenshot's fan-out: master, two operators, reviewer
            (
                &[(0.0, 150.0), (420.0, 0.0), (420.0, 300.0), (860.0, 150.0)],
                3,
                1,
            ),
            (
                &[(0.0, 150.0), (420.0, 0.0), (420.0, 300.0), (860.0, 150.0)],
                2,
                0,
            ),
        ];
        for (cards, a, b) in layouts {
            for (from, to) in [task(cards[*a], cards[*b]), report(cards[*a], cards[*b])] {
                let path = route(from, to, cards);
                assert!(path.len() >= 2);
                assert_eq!(path[0], from.at);
                assert_eq!(*path.last().unwrap(), to.at);
                // leaves and enters horizontally, away from / into the card
                assert_eq!(path[0].1, path[1].1);
                assert!((path[1].0 - path[0].0) * from.side.out() > 0.0);
                let n = path.len();
                assert_eq!(path[n - 1].1, path[n - 2].1);
                assert!((path[n - 2].0 - path[n - 1].0) * to.side.out() > 0.0);
                for w in path.windows(2) {
                    assert!(w[0].0 == w[1].0 || w[0].1 == w[1].1, "{path:?}");
                }
                assert!(!crosses_any(&path, cards), "{cards:?} {a}->{b}: {path:?}");
            }
        }
    }

    #[test]
    fn cards_closer_than_the_clearance_still_route_clear() {
        // 4 DIP apart, report from the right card back into the left one.
        let cards = [(244.0, 20.0), (20.0, 20.0)];
        let (from, to) = report(cards[0], cards[1]);
        let path = route(from, to, &cards);
        assert_eq!(path[0], from.at);
        assert_eq!(*path.last().unwrap(), to.at);
        assert!(!crosses_any(&path, &cards), "{path:?}");
    }

    #[test]
    fn a_straight_shot_stays_straight() {
        let cards = [(0.0, 0.0), (400.0, 0.0)];
        let (from, to) = task(cards[0], cards[1]);
        assert_eq!(route(from, to, &cards), vec![from.at, to.at]);
    }

    #[test]
    fn corners_are_rounded_and_dots_travel_forward() {
        let path = [(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (200.0, 100.0)];
        let segs = rounded(&path);
        assert_eq!(
            segs.iter().filter(|s| matches!(s, Seg::Curve(..))).count(),
            2
        );
        let line = flatten(path[0], &segs);
        assert_eq!(*line.last().unwrap(), (200.0, 100.0));
        let early = dots(&line, 50.0, 0.0);
        let later = dots(&line, 50.0, 10.0);
        assert!(!early.is_empty() && early.len() >= later.len() - 1);
        assert!(later[0].0 > early[0].0, "phase moves dots toward the end");
    }
}
