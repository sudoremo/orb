import CoreImage
import Vision

/// The setup code in a picture of it: a photo of the Mac's screen or a
/// screenshot, for a phone without a camera at hand (the simulator) or a code
/// shown elsewhere.
public enum SetupCodeImage {
    /// The text of the first Orb setup code in `image`, if there is one.
    ///
    /// Vision first, the better reader of a photo taken at an angle; Core
    /// Image's detector where Vision cannot run, as in the simulator.
    public static func payload(in image: CGImage) -> String? {
        let isCode = { (text: String) in text.hasPrefix("orb-setup:") }
        let request = VNDetectBarcodesRequest()
        request.symbologies = [.qr]
        if (try? VNImageRequestHandler(cgImage: image).perform([request])) != nil,
           let text = request.results?.compactMap(\.payloadStringValue).first(where: isCode) {
            return text
        }
        let detector = CIDetector(ofType: CIDetectorTypeQRCode, context: nil, options: [CIDetectorAccuracy: CIDetectorAccuracyHigh])
        return detector?.features(in: CIImage(cgImage: image))
            .compactMap { ($0 as? CIQRCodeFeature)?.messageString }
            .first(where: isCode)
    }
}
