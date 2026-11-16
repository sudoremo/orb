import OrbKit
import PhotosUI
import SwiftUI
import VisionKit

/// The camera, looking for the setup code Orb on the Mac shows.
struct CodeScanner: UIViewControllerRepresentable {
    let found: (String) -> Void

    static var isAvailable: Bool {
        DataScannerViewController.isSupported && DataScannerViewController.isAvailable
    }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            isHighlightingEnabled: true
        )
        scanner.delegate = context.coordinator
        try? scanner.startScanning()
        return scanner
    }

    func updateUIViewController(_ controller: DataScannerViewController, context: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(found: found) }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let found: (String) -> Void
        private var done = false

        init(found: @escaping (String) -> Void) { self.found = found }

        func dataScanner(_ scanner: DataScannerViewController, didAdd items: [RecognizedItem], allItems: [RecognizedItem]) {
            for case .barcode(let code) in items {
                guard !done, let text = code.payloadStringValue, text.hasPrefix("orb-setup:") else { continue }
                done = true
                scanner.stopScanning()
                found(text)
            }
        }
    }
}

/// The scanner as a sheet, with its way out.
struct CodeScannerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let found: (String) -> Void

    var body: some View {
        NavigationStack {
            CodeScanner { text in
                dismiss()
                found(text)
            }
            .ignoresSafeArea()
            .navigationTitle("Scan the Code on Your Mac")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", systemImage: "xmark", role: .cancel) { dismiss() }
                }
            }
            .safeAreaInset(edge: .bottom) {
                Text("On the Mac: Tasks ▸ Set Up iPhone…")
                    .font(.footnote)
                    .padding(10)
                    .glassEffect()
                    .padding(.bottom, 24)
            }
        }
    }
}
