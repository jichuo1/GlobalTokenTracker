// App-wide text size — the WinUI "界面字号" setting. macOS ignores
// `dynamicTypeSize` (verified: every DynamicTypeSize renders identically),
// so the scale is our own: one base size (default 13 pt, the macOS body
// size) from which every text style is derived, like the Windows shell's
// body size with title/h2/label derived from it.
import SwiftUI

enum Typography {
    static let defaultSize: Double = 13
    static let range: ClosedRange<Double> = 11...17

    /// macOS point sizes and weights of the semantic text styles at 13 pt.
    static func base(_ style: Font.TextStyle) -> (size: CGFloat, weight: Font.Weight) {
        switch style {
        case .largeTitle: (26, .regular)
        case .title: (22, .regular)
        case .title2: (17, .regular)
        case .title3: (15, .regular)
        case .headline: (13, .bold)
        case .subheadline: (11, .regular)
        case .callout: (12, .regular)
        case .footnote: (10, .regular)
        case .caption: (10, .regular)
        case .caption2: (10, .regular)
        default: (13, .regular)  // body
        }
    }

    static func font(_ style: Font.TextStyle, scale: CGFloat, weight: Font.Weight? = nil,
                     design: Font.Design = .default) -> Font {
        let b = base(style)
        return .system(size: (b.size * scale).rounded(), weight: weight ?? b.weight, design: design)
    }
}

private struct TextScaleKey: EnvironmentKey {
    static let defaultValue: CGFloat = 1
}

extension EnvironmentValues {
    /// User text size / 13 pt.
    var textScale: CGFloat {
        get { self[TextScaleKey.self] }
        set { self[TextScaleKey.self] = newValue }
    }
}

extension View {
    /// A semantic text style at the user's text size.
    func scaledFont(_ style: Font.TextStyle, weight: Font.Weight? = nil,
                    design: Font.Design = .default) -> some View {
        modifier(ScaledFont(style: style, weight: weight, design: design))
    }

    /// Root of a window or panel: publishes the scale and sets the default
    /// (body) font, so unstyled text — table cells, form labels — follows.
    func appTextScale() -> some View { modifier(AppTextScale()) }
}

private struct ScaledFont: ViewModifier {
    let style: Font.TextStyle
    let weight: Font.Weight?
    let design: Font.Design
    @Environment(\.textScale) private var scale

    func body(content: Content) -> some View {
        content.font(Typography.font(style, scale: scale, weight: weight, design: design))
    }
}

private struct AppTextScale: ViewModifier {
    @AppStorage(Prefs.textSizeKey) private var size = Typography.defaultSize

    func body(content: Content) -> some View {
        let scale = CGFloat(min(max(size, Typography.range.lowerBound), Typography.range.upperBound)
            / Typography.defaultSize)
        content
            .environment(\.textScale, scale)
            .font(Typography.font(.body, scale: scale))
    }
}
