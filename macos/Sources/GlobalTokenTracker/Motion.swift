// One motion vocabulary for the whole shell, so every animated change feels
// related: a quick, barely-overshooting spring for selections, a slightly
// longer one for page and month changes. With Reduce Motion on, movement is
// replaced by short crossfades (no slides, no rolling digits).
import AppKit
import SwiftUI

enum Motion {
    /// Calendar / dial / caption changes — settles in ~0.3 s.
    static let select = Animation.spring(response: 0.32, dampingFraction: 0.86)
    /// Month paging and page navigation — a touch softer.
    static let page = Animation.spring(response: 0.38, dampingFraction: 0.9)
    /// Drum page turn — long enough to read as a rotation, no bounce.
    static let drum = Animation.spring(response: 0.5, dampingFraction: 0.94)
    /// Keyframe durations are never 0: a zero-length keyframe (the first
    /// component's hold, index 0 × step) interpolates by dividing by zero,
    /// and the NaN pose left the first stat tile's text displaced and stuck.
    static func span(_ seconds: Double) -> Double {
        seconds.isFinite ? max(0.001, seconds) : 0.001
    }

    /// Delay between consecutive components in a page-turn cascade.
    static let cascadeStep = 0.035
    /// Hover feedback.
    static let hover = Animation.easeOut(duration: 0.12)
    /// Press feedback (springs back).
    static let press = Animation.spring(response: 0.22, dampingFraction: 0.6)

    /// When a page turn has visually settled — data that would re-lay out a
    /// large view waits this long after the page appears.
    static let settleMs = 480

    static var reduced: Bool { NSWorkspace.shared.accessibilityDisplayShouldReduceMotion }

    /// The animation to use for `base`, honouring Reduce Motion.
    static func resolved(_ base: Animation) -> Animation {
        reduced ? .easeInOut(duration: 0.15) : base
    }
}

extension View {
    /// Wheel-like rolling text: changed glyphs roll up when the new value is
    /// later, down when earlier. Plain crossfade under Reduce Motion.
    func rollingText(down: Bool) -> some View { modifier(RollingText(down: down)) }
}

private struct RollingText: ViewModifier {
    let down: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        content.contentTransition(reduceMotion ? .opacity : .numericText(countsDown: down))
    }
}

/// Short directional slide + fade between pages or months.
struct SlideFade: ViewModifier {
    let offset: CGSize
    let opacity: Double

    func body(content: Content) -> some View {
        content.offset(offset).opacity(opacity)
    }
}

extension AnyTransition {
    /// Content enters from `edge` (a short offset, not a full-width move —
    /// cheap for heavy pages) and leaves toward the opposite edge.
    static func slideFade(from edge: Edge, distance: CGFloat, reduced: Bool) -> AnyTransition {
        if reduced { return .opacity }
        func shift(_ e: Edge) -> CGSize {
            switch e {
            case .top: CGSize(width: 0, height: -distance)
            case .bottom: CGSize(width: 0, height: distance)
            case .leading: CGSize(width: -distance, height: 0)
            case .trailing: CGSize(width: distance, height: 0)
            }
        }
        let opposite: Edge = switch edge {
        case .top: .bottom
        case .bottom: .top
        case .leading: .trailing
        case .trailing: .leading
        }
        return .asymmetric(
            insertion: .modifier(active: SlideFade(offset: shift(edge), opacity: 0),
                                 identity: SlideFade(offset: .zero, opacity: 1)),
            removal: .modifier(active: SlideFade(offset: shift(opposite), opacity: 0),
                               identity: SlideFade(offset: .zero, opacity: 1))
        )
    }
}

// MARK: - Drum (cylinder) page roll

/// A page on a drum whose axis is horizontal and behind the screen, turned
/// by `angle` degrees: positive rolls it up and away over the top, negative
/// down and under. Projection built by hand — SwiftUI's `rotation3DEffect`
/// perspective alone magnifies the near edge — so the page travels R·sin a,
/// recedes R·(1−cos a) (shrinking toward a virtual camera), tilts by a about
/// its own centre, and darkens/fades as it turns away. `Animatable`, so every
/// frame lies on the circle instead of a straight line between end states.
struct DrumRoll: ViewModifier, Animatable {
    var angle: Double
    let radius: CGFloat
    /// Angle between adjacent pages (= page height / radius); full shading
    /// and fade are reached there.
    let span: Double

    nonisolated var animatableData: Double {
        get { angle }
        set { angle = newValue }
    }

    func body(content: Content) -> some View {
        // Faces more than one span away are fully faded; keep their
        // transform bounded instead of spinning them round the back.
        let clamped = min(max(angle, -span * 1.05), span * 1.05)
        let a = clamped * .pi / 180
        let camera = radius * 2.4
        let scale = camera / (camera + radius * (1 - cos(a)))
        let t = min(1, abs(angle) / span)
        content
            .overlay(Color.black.opacity(0.22 * t).allowsHitTesting(false))
            .rotation3DEffect(.degrees(clamped), axis: (x: 1, y: 0, z: 0), anchor: .center, perspective: 0.2)
            .scaleEffect(scale)
            .offset(y: -radius * sin(a) * scale)
            // Reaches ~0 exactly at the parked angle: the previous page used
            // to stay 40% visible at the window edge until the turn fully
            // settled — an unfinished seam. 0.001 (not 0) keeps its layers.
            .opacity(max(0.001, 1 - t * t))
    }
}

// MARK: - Page-turn cascade

/// What a page turn means for one face: the page coming in, the one going
/// out, or one that isn't moving. `id` changes once per turn and triggers
/// every component's own cascade animation.
struct PageTurn: Equatable {
    enum Role: Equatable { case incoming, outgoing, idle }
    var id = 0
    var forward = true
    var role = Role.idle
    /// Extra wait before an incoming page's cascade: on a multi-page jump
    /// the destination only comes into view near the end of the travel.
    var delay: Double = 0
}

/// One per page face, injected once and never replaced. The deck mutates
/// `turn`; only the cascade modifiers that read it are invalidated. (Passing
/// the role as a plain environment *value* changed the environment on every
/// turn, and AppKit-backed views — the 5,000-row prices Table — re-synced
/// on any environment change: a 110 ms first frame.)
@MainActor
@Observable
final class PageTurnState {
    var turn = PageTurn()
}

extension View {
    /// Chain-reaction motion for one component of a page: it rides its own
    /// spring, released `index` steps after the first, so content ripples
    /// into place behind the drum instead of moving as one rigid sheet.
    /// Incoming components arrive from the direction of travel (later ones
    /// from further out); outgoing ones peel away the other way. Only this
    /// component's offset/opacity animate — its contents don't re-render.
    func cascade(_ index: Int) -> some View { modifier(Cascade(index: index)) }
}

private struct Cascade: ViewModifier {
    let index: Int
    @Environment(PageTurnState.self) private var state: PageTurnState?
    private var turn: PageTurn { state?.turn ?? PageTurn() }
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    struct Pose {
        var y: CGFloat = 0
        var opacity: Double = 1
    }

    func body(content: Content) -> some View {
        if reduceMotion || ProbeFlags.noCascade {
            content
        } else {
            content.environment(\.cascadeIndex, index).keyframeAnimator(initialValue: Pose(), trigger: turn.id) { view, pose in
                // transformEffect, not offset: a render-time translation, so
                // hosted AppKit views (tables) aren't re-framed every frame —
                // that re-laid them out mid-turn (and tripped NSTableView's
                // reentrancy warning).
                view.transformEffect(CGAffineTransform(translationX: 0, y: pose.y.isFinite ? pose.y : 0))
                    .opacity(pose.opacity.isFinite ? pose.opacity : 1)
            } keyframes: { _ in
                let k = plan()
                KeyframeTrack(\.y) {
                    LinearKeyframe(k.y0, duration: 0.001)
                    LinearKeyframe(k.y0, duration: Motion.span(k.hold))
                    SpringKeyframe(k.y1, duration: Motion.span(k.duration), spring: k.spring)
                }
                KeyframeTrack(\.opacity) {
                    LinearKeyframe(k.o0, duration: 0.001)
                    LinearKeyframe(k.o0, duration: Motion.span(k.hold))
                    CubicKeyframe(k.o1, duration: Motion.span(k.fade))
                }
            }
        }
    }

    /// Keyframe parameters for this component's role in the turn.
    private struct Plan {
        var y0: CGFloat = 0, y1: CGFloat = 0
        var o0: Double = 1, o1: Double = 1
        var hold: Double = 0
        var duration: Double = 0.001
        var fade: Double = 0.001
        var spring = Spring(response: 0.46, dampingRatio: 0.82)
    }

    private func plan() -> Plan {
        let step = Double(min(index, 10))
        let delay = step * Motion.cascadeStep
        let dir: CGFloat = turn.forward ? 1 : -1
        switch turn.role {
        case .incoming:
            // From the direction of travel, later components from further
            // out; hold through the delay, then spring home.
            return Plan(y0: dir * (26 + CGFloat(step) * 9), y1: 0, o0: 0.25, o1: 1,
                        hold: turn.delay + delay, duration: 0.62, fade: 0.34,
                        spring: Spring(response: 0.46, dampingRatio: 0.82))
        case .outgoing:
            // Lead the drum away: earlier components leave first.
            return Plan(y0: 0, y1: -dir * (18 + CGFloat(step) * 7), o0: 1, o1: 0.35,
                        hold: delay * 0.6, duration: 0.45, fade: 0.4,
                        spring: Spring(response: 0.4, dampingRatio: 0.9))
        case .idle:
            return Plan()
        }
    }
}

// MARK: - Frosted seam

/// Frosted-glass band riding the seam between two turning faces: a live
/// material blur, faded in at both ends by a vertical gradient mask, with a
/// hairline highlight along the edge. Sits above the drum (blur needs a real
/// backdrop; it can't live inside the 3D-transformed faces) and follows the
/// shared edge computed from the same prism geometry. `progress` runs 0 → 1
/// over a turn; strength peaks mid-turn and is zero at rest, so the band
/// costs nothing between turns (it isn't drawn).
struct SeamGlass: View, Animatable {
    var progress: Double
    let forward: Bool
    let height: CGFloat
    let geometry: DrumGeometry

    nonisolated var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    var body: some View {
        let p = progress - progress.rounded(.down)  // turn-local 0…1
        let strength = sin(.pi * min(max(p, 0), 1))
        let band = max(56, height * 0.16)
        // Always mounted: the material is an AppKit visual-effect view, and
        // creating it at the start of each turn / destroying it at the end
        // cost a ~40–50 ms frame each time. At rest it's parked above the
        // top edge (clipped away) at near-zero opacity instead.
        let resting = strength < 0.02
        Rectangle()
            .fill(.ultraThinMaterial)
            .overlay(
                LinearGradient(colors: [.white.opacity(0), .white.opacity(0.16), .white.opacity(0)],
                               startPoint: .top, endPoint: .bottom)
            )
            .overlay(Rectangle().fill(.white.opacity(0.35)).frame(height: 0.5))
            .mask(
                LinearGradient(stops: [
                    .init(color: .clear, location: 0),
                    .init(color: .black, location: 0.38),
                    .init(color: .black, location: 0.62),
                    .init(color: .clear, location: 1),
                ], startPoint: .top, endPoint: .bottom)
            )
            .frame(height: band)
            .offset(y: resting ? -band * 2 : seamY(p) - band / 2)
            .opacity(resting ? 0.001 : strength)
            .frame(maxHeight: .infinity, alignment: .top)
            .allowsHitTesting(false)
            .accessibilityHidden(true)
    }

    /// Screen y (from the top) of the edge shared by the two faces at turn
    /// progress `p`: the leaving face's bottom edge (forward) or top edge.
    private func seamY(_ p: Double) -> CGFloat {
        let span = geometry.span * .pi / 180
        let a = (forward ? 1 : -1) * p * span          // leaving face angle
        let apo = Double(geometry.radius)
        let half = Double(height) / 2
        let s = forward ? 1.0 : -1.0                    // bottom (+) or top (−) edge
        let y = -apo * sin(a) + s * half * cos(a)       // from centre, down = +
        let z = apo * (1 - cos(a)) + s * half * sin(a)  // depth, away = +
        let camera = apo * 2.4
        return CGFloat(half + y * camera / (camera + max(z, -camera * 0.5)))
    }
}

// MARK: - Depth layers (text parallax inside a component)

private struct CascadeIndexKey: EnvironmentKey {
    static let defaultValue = 0
}

extension EnvironmentValues {
    /// The enclosing component's cascade position (set by `.cascade(_:)`), so
    /// its depth layers release in step with it.
    var cascadeIndex: Int {
        get { self[CascadeIndexKey.self] }
        set { self[CascadeIndexKey.self] = newValue }
    }
}

extension View {
    /// A layer inside a cascading component, `depth` levels "behind" its
    /// surface: on a page turn it trails the component by a little more
    /// distance and time per level, so a card's label, figure and caption
    /// slide past each other instead of moving as one flat sheet. Same
    /// render-only transform as the cascade; idle pages aren't touched.
    func depthLayer(_ depth: Int) -> some View { modifier(DepthLayer(depth: depth)) }
}

private struct DepthLayer: ViewModifier {
    let depth: Int
    @Environment(PageTurnState.self) private var state: PageTurnState?
    @Environment(\.cascadeIndex) private var index
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    struct Pose {
        var y: CGFloat = 0
        var opacity: Double = 1
    }

    func body(content: Content) -> some View {
        if reduceMotion || ProbeFlags.noCascade {
            content
        } else {
            let turn = state?.turn ?? PageTurn()
            content.keyframeAnimator(initialValue: Pose(), trigger: turn.id) { view, pose in
                view.transformEffect(CGAffineTransform(translationX: 0, y: pose.y.isFinite ? pose.y : 0))
                    .opacity(pose.opacity.isFinite ? pose.opacity : 1)
            } keyframes: { _ in
                let d = Double(depth)
                let step = Double(min(index, 10))
                let dir: CGFloat = turn.forward ? 1 : -1
                let incoming = turn.role == .incoming
                let outgoing = turn.role == .outgoing
                let y0: CGFloat = incoming ? dir * CGFloat(depth) * 11 : 0
                let y1: CGFloat = outgoing ? -dir * CGFloat(depth) * 9 : 0
                let hold = incoming ? turn.delay + step * Motion.cascadeStep + d * 0.024
                    : outgoing ? step * Motion.cascadeStep * 0.6 : 0
                let moving = incoming || outgoing
                KeyframeTrack(\.y) {
                    LinearKeyframe(y0, duration: 0.001)
                    LinearKeyframe(y0, duration: Motion.span(hold))
                    SpringKeyframe(y1, duration: moving ? 0.6 + d * 0.06 : 0.001,
                                   spring: Spring(response: 0.44 + d * 0.07, dampingRatio: 0.8))
                }
                KeyframeTrack(\.opacity) {
                    LinearKeyframe(incoming ? 0 : 1, duration: 0.001)
                    LinearKeyframe(incoming ? 0 : 1, duration: Motion.span(hold))
                    CubicKeyframe(outgoing ? 0.25 : 1, duration: moving ? 0.28 + d * 0.05 : 0.001)
                }
            }
        }
    }
}
