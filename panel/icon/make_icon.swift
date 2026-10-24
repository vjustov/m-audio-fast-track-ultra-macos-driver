// Draws the app icon: three mixer faders at different levels on a deep blue field.
// Full-bleed square artwork; macOS applies the rounded mask (HIG app-icons.md, Icon shape).
// Usage: swift make_icon.swift out.png
import AppKit

let size = 1024.0
let ctx = CGContext(data: nil, width: Int(size), height: Int(size), bitsPerComponent: 8, bytesPerRow: 0,
                    space: CGColorSpace(name: CGColorSpace.displayP3)!,
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!

func color(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: CGColorSpace(name: CGColorSpace.displayP3)!,
            components: [CGFloat(hex >> 16 & 0xff) / 255, CGFloat(hex >> 8 & 0xff) / 255, CGFloat(hex & 0xff) / 255, a])!
}

// Background: vertical gradient, lighter at the top so system lighting reads well.
let bg = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.displayP3), colors: [color(0x2F5D8C), color(0x142A45)] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(bg, start: CGPoint(x: 0, y: size), end: CGPoint(x: 0, y: 0), options: [])

// Three fader tracks with caps. Caps rise left to right like a level climbing.
let xs = [332.0, 512.0, 692.0]
let caps = [400.0, 600.0, 500.0] // cap centre heights (y up)
let trackTop = 790.0, trackBottom = 234.0
for (i, x) in xs.enumerated() {
    // Track: a thick rounded slot, thick enough to survive small sizes.
    let track = CGPath(roundedRect: CGRect(x: x - 22, y: trackBottom, width: 44, height: trackTop - trackBottom),
                       cornerWidth: 22, cornerHeight: 22, transform: nil)
    ctx.addPath(track)
    ctx.setFillColor(color(0x0A1626, 0.75))
    ctx.fillPath()
    // Filled part below the cap, the "level".
    let fill = CGPath(roundedRect: CGRect(x: x - 22, y: trackBottom, width: 44, height: caps[i] - trackBottom),
                      cornerWidth: 22, cornerHeight: 22, transform: nil)
    ctx.addPath(fill)
    ctx.setFillColor(color(0x7FD3C9, 0.55))
    ctx.fillPath()
    // Cap.
    let cap = CGPath(roundedRect: CGRect(x: x - 70, y: caps[i] - 46, width: 140, height: 92),
                     cornerWidth: 28, cornerHeight: 28, transform: nil)
    ctx.addPath(cap)
    ctx.setFillColor(color(0xF4B860))
    ctx.fillPath()
    // Cap line.
    ctx.setFillColor(color(0x8A5A1C))
    ctx.fill(CGRect(x: x - 50, y: caps[i] - 7, width: 100, height: 14))
}

let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
