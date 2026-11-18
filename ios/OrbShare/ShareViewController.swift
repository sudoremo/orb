import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// Share to Orb: a new task from whatever was shared — its title from the
/// text or the page, the link in the notes, files attached.
final class ShareViewController: UIViewController {
    override func viewDidLoad() {
        super.viewDidLoad()
        let model = ShareModel(context: extensionContext)
        let host = UIHostingController(rootView: ShareView(model: model))
        addChild(host)
        host.view.frame = view.bounds
        host.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(host.view)
        host.didMove(toParent: self)
        Task { await model.load() }
    }
}

@MainActor @Observable
final class ShareModel {
    var title = ""
    var notes = ""
    var files: [URL] = []
    var problem: String?
    private let context: NSExtensionContext?

    init(context: NSExtensionContext?) {
        self.context = context
    }

    /// The largest file taken: it is read whole into the app's memory when
    /// the task is made, and uploaded as one message.
    static let maxFileBytes = 25 * 1024 * 1024

    /// Take a shared file, unless it is too large to carry.
    private func accept(_ url: URL) {
        let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
        if size > Self.maxFileBytes {
            problem = "\(url.lastPathComponent) is larger than 25 MB and is left out."
        } else {
            files.append(url)
        }
    }

    /// Read what was shared: text and links into the title and notes, files
    /// as attachments.
    func load() async {
        let providers = (context?.inputItems as? [NSExtensionItem] ?? []).flatMap { item -> [NSItemProvider] in
            if let text = item.attributedContentText?.string, !text.isEmpty, title.isEmpty { title = text }
            return item.attachments ?? []
        }
        for provider in providers {
            if provider.hasItemConformingToTypeIdentifier(UTType.url.identifier),
               let url = try? await provider.loadItem(forTypeIdentifier: UTType.url.identifier) as? URL {
                if url.isFileURL {
                    accept(url)
                } else {
                    notes = notes.isEmpty ? url.absoluteString : notes + "\n" + url.absoluteString
                    if title.isEmpty { title = url.host() ?? url.absoluteString }
                }
            } else if provider.hasItemConformingToTypeIdentifier(UTType.plainText.identifier),
                      let text = try? await provider.loadItem(forTypeIdentifier: UTType.plainText.identifier) as? String {
                if title.isEmpty {
                    title = text.split(separator: "\n").first.map(String.init) ?? text
                } else {
                    notes = notes.isEmpty ? text : notes + "\n" + text
                }
            } else if let type = provider.registeredTypeIdentifiers.first,
                      let url = try? await provider.loadFileRepresentation(forTypeIdentifier: type) {
                accept(url)
            }
        }
    }

    func save() {
        let item = SharedItem(
            title: title.trimmingCharacters(in: .whitespacesAndNewlines),
            notes: notes.isEmpty ? nil : notes,
            files: [],
            shared: .now
        )
        do {
            try ShareInbox.put(item, files: files)
            context?.completeRequest(returningItems: nil)
        } catch {
            problem = error.localizedDescription
        }
    }

    func cancel() {
        context?.cancelRequest(withError: CocoaError(.userCancelled))
    }
}

private extension NSItemProvider {
    /// A file representation copied somewhere that outlives the callback.
    func loadFileRepresentation(forTypeIdentifier type: String) async throws -> URL {
        try await withCheckedThrowingContinuation { continuation in
            _ = loadFileRepresentation(forTypeIdentifier: type) { url, error in
                guard let url else {
                    continuation.resume(throwing: error ?? CocoaError(.fileNoSuchFile))
                    return
                }
                let copy = FileManager.default.temporaryDirectory.appendingPathComponent(url.lastPathComponent)
                try? FileManager.default.removeItem(at: copy)
                do {
                    try FileManager.default.copyItem(at: url, to: copy)
                    continuation.resume(returning: copy)
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }
}

struct ShareView: View {
    @Bindable var model: ShareModel

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("New Task", text: $model.title)
                        .font(.title3)
                    if !model.notes.isEmpty {
                        TextField("Notes", text: $model.notes, axis: .vertical)
                            .foregroundStyle(.secondary)
                    }
                }
                if !model.files.isEmpty {
                    Section("Attachments") {
                        ForEach(model.files, id: \.self) { Label($0.lastPathComponent, systemImage: "paperclip") }
                    }
                }
                Section {
                    Text("The task goes to Today the next time Orb runs.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
                if let problem = model.problem {
                    Text(problem).foregroundStyle(.red)
                }
            }
            .navigationTitle("New Task")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", systemImage: "xmark", role: .cancel) { model.cancel() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add", systemImage: "checkmark", role: .confirm) { model.save() }
                        .disabled(model.title.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
        }
    }
}
