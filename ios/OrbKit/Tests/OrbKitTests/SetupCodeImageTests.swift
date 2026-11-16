import CoreImage
import CoreImage.CIFilterBuiltins
import Testing
@testable import OrbKit

/// A code drawn as a QR image reads back as the text it was made from, and a
/// QR code that is not Orb's reads as nothing.
@Test func aSetupCodeIsReadFromAPictureOfIt() throws {
    let text = "orb-setup:eyJ2ZXJzaW9uIjoxfQ"
    #expect(SetupCodeImage.payload(in: try qr(text)) == text)
    #expect(SetupCodeImage.payload(in: try qr("https://example.com")) == nil)
}

private func qr(_ text: String) throws -> CGImage {
    let filter = CIFilter.qrCodeGenerator()
    filter.message = Data(text.utf8)
    let code = try #require(filter.outputImage).transformed(by: CGAffineTransform(scaleX: 10, y: 10))
    // On white with a margin, as the Mac shows it: the generator draws on
    // nothing, and a reader needs the quiet zone.
    let canvas = code.extent.insetBy(dx: -60, dy: -60)
    let image = code.composited(over: CIImage(color: .white).cropped(to: canvas))
    return try #require(CIContext().createCGImage(image, from: canvas))
}
