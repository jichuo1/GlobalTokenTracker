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
    /// Hover feedback.
    static let hover = Animation.easeOut(duration: 0.12)
    /// Press feedback (springs back).
    static let press = Animation.spring(response: 0.22, dampingFraction: 0.6)

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
