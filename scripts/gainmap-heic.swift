// Writes a HEIC with an HDR gain map from a JPEG that has one (Ultra HDR), with ImageIO and
// Core Image: the way current phones store HDR photos.
import CoreImage
import Foundation

let arguments = CommandLine.arguments
guard arguments.count == 3 else {
    FileHandle.standardError.write("usage: gainmap-heic <in.jpg> <out.heic>\n".data(using: .utf8)!)
    exit(2)
}
let source = URL(fileURLWithPath: arguments[1])
let target = URL(fileURLWithPath: arguments[2])
guard let standard = CIImage(contentsOf: source),
      let hdr = CIImage(contentsOf: source, options: [.expandToHDR: true]),
      let space = CGColorSpace(name: CGColorSpace.displayP3)
else {
    FileHandle.standardError.write("cannot read the picture\n".data(using: .utf8)!)
    exit(1)
}
do {
    try CIContext().writeHEIFRepresentation(
        of: standard, to: target, format: .RGBA8, colorSpace: space, options: [.hdrImage: hdr])
} catch {
    FileHandle.standardError.write("cannot write the HEIC: \(error)\n".data(using: .utf8)!)
    exit(1)
}
