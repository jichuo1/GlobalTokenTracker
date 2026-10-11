// Renders the DMG window background (660×420 pt) at 1× and 2×.
//   swift macos/dmg/make-background.swift macos/dmg
// Then: tiffutil -cathidpicheck background.png background@2x.png -out background.tiff
// Layout must match settings.py: app icon at (170, 230), Applications at (490, 230).
import AppKit

let size = CGSize(width: 660, height: 420)
let accent = NSColor(srgbRed: 0x4d / 255, green: 0x8a / 255, blue: 0xe8 / 255, alpha: 1)

func render(scale: CGFloat, to url: URL) throws {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: Int(size.width * scale), pixelsHigh: Int(size.height * scale),
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = size  // points → the 2× rep carries 144 dpi
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    defer { NSGraphicsContext.restoreGraphicsState() }

    // Finder draws icon labels in dark text, so the image stays light in both
    // appearances: a quiet top-to-bottom wash with a faint accent glow.
    NSGradient(colors: [
        NSColor(srgbRed: 0.985, green: 0.988, blue: 0.996, alpha: 1),
        NSColor(srgbRed: 0.925, green: 0.940, blue: 0.965, alpha: 1),
    ])!.draw(in: CGRect(origin: .zero, size: size), angle: -90)
    NSGradient(colors: [accent.withAlphaComponent(0.10), accent.withAlphaComponent(0)])!
        .draw(fromCenter: CGPoint(x: 330, y: 190), radius: 0, toCenter: CGPoint(x: 330, y: 190), radius: 300,
              options: [])

    // Title + bilingual hint (y is flipped: AppKit origin is bottom-left).
    func text(_ s: String, _ font: NSFont, _ color: NSColor, y: CGFloat) {
        let p = NSMutableParagraphStyle()
        p.alignment = .center
        NSAttributedString(string: s, attributes: [.font: font, .foregroundColor: color, .paragraphStyle: p])
            .draw(in: CGRect(x: 0, y: size.height - y, width: size.width, height: 30))
    }
    text("GlobalTokenTracker", .systemFont(ofSize: 22, weight: .semibold),
         NSColor(srgbRed: 0.13, green: 0.15, blue: 0.20, alpha: 1), y: 62)
    text("Drag the app into Applications to install  ·  将应用拖入“应用程序”完成安装",
         .systemFont(ofSize: 12.5, weight: .regular),
         NSColor(srgbRed: 0.36, green: 0.40, blue: 0.47, alpha: 1), y: 92)

    // Arrow between the two icon slots (icon centres at y = 230 from top).
    let y = size.height - 222
    let path = NSBezierPath()
    path.move(to: CGPoint(x: 266, y: y))
    path.line(to: CGPoint(x: 382, y: y))
    path.lineWidth = 3.5
    path.lineCapStyle = .round
    path.setLineDash([0.5, 9], count: 2, phase: 0)
    accent.withAlphaComponent(0.85).setStroke()
    path.stroke()
    let head = NSBezierPath()
    head.move(to: CGPoint(x: 384, y: y + 11))
    head.line(to: CGPoint(x: 396, y: y))
    head.line(to: CGPoint(x: 384, y: y - 11))
    head.lineWidth = 3.5
    head.lineCapStyle = .round
    head.lineJoinStyle = .round
    head.stroke()

    // Footer: what the user gets, in one quiet line.
    text("Native for Apple silicon and Intel  ·  macOS 26 or later",
         .systemFont(ofSize: 11, weight: .regular),
         NSColor(srgbRed: 0.50, green: 0.54, blue: 0.60, alpha: 1), y: 388)

    guard let png = rep.representation(using: .png, properties: [:]) else {
        throw NSError(domain: "bg", code: 1)
    }
    try png.write(to: url)
}

let out = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? ".")
try render(scale: 1, to: out.appendingPathComponent("background.png"))
try render(scale: 2, to: out.appendingPathComponent("background@2x.png"))
print("wrote background.png + background@2x.png to \(out.path)")
