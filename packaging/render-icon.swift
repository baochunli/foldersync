import CoreGraphics
import Foundation
import ImageIO

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("Error: \(message)\n".utf8))
    exit(2)
}

let input = URL(fileURLWithPath: CommandLine.arguments[1])
let output = URL(fileURLWithPath: CommandLine.arguments[2])
let png = "public.png" as CFString
guard let source = CGImageSourceCreateWithURL(input as CFURL, nil),
      CGImageSourceGetType(source) == png,
      let image = CGImageSourceCreateImageAtIndex(source, 0, nil),
      image.width >= 512, image.height >= 512 else {
    fail("The icon input must be a readable PNG at least 512x512 pixels.")
}

let canvas = 1024
let tile = CGRect(x: 96, y: 96, width: 832, height: 832)
guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
      let context = CGContext(data: nil, width: canvas, height: canvas,
                              bitsPerComponent: 8, bytesPerRow: 0, space: colorSpace,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else {
    fail("Could not create the app icon canvas.")
}

context.clear(CGRect(x: 0, y: 0, width: canvas, height: canvas))
context.setFillColor(red: 0.976, green: 0.980, blue: 0.988, alpha: 1)
context.addPath(CGPath(roundedRect: tile, cornerWidth: 83, cornerHeight: 83, transform: nil))
context.fillPath()

let scale = 614.0 / CGFloat(max(image.width, image.height))
let width = CGFloat(image.width) * scale
let height = CGFloat(image.height) * scale
context.interpolationQuality = .high
context.draw(image, in: CGRect(x: tile.midX - width / 2, y: tile.midY - height / 2,
                              width: width, height: height))

guard let master = context.makeImage(),
      let destination = CGImageDestinationCreateWithURL(output as CFURL, png, 1, nil) else {
    fail("Could not create the app icon PNG.")
}
CGImageDestinationAddImage(destination, master, nil)
guard CGImageDestinationFinalize(destination) else {
    fail("Could not write the app icon PNG.")
}
