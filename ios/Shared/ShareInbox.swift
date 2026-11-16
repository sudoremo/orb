import Foundation

/// What the share extension leaves for the app: a task's title, notes and
/// files, in a folder of its own in the App Group container. The extension
/// never opens the database; the app makes the task the next time it runs.
struct SharedItem: Codable, Sendable {
    var title: String
    var notes: String?
    /// The names of the files in the item's folder, to attach.
    var files: [String]
    var shared: Date
}

enum ShareInbox {
    static var root: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: WidgetSnapshot.appGroup)?
            .appendingPathComponent("share-inbox", isDirectory: true)
    }

    /// Leave an item, with copies of its files, for the app.
    static func put(_ item: SharedItem, files: [URL]) throws {
        guard let root else { throw CocoaError(.fileNoSuchFile) }
        let folder = root.appendingPathComponent(UUID().uuidString, isDirectory: true)
        let staging = root.appendingPathComponent(".staging-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        var item = item
        item.files = []
        for file in files {
            let name = file.lastPathComponent
            try FileManager.default.copyItem(at: file, to: staging.appendingPathComponent(name))
            item.files.append(name)
        }
        try JSONEncoder().encode(item).write(to: staging.appendingPathComponent("item.json"))
        // Moved into place whole, so the app never reads half an item.
        try FileManager.default.moveItem(at: staging, to: folder)
    }

    /// The items waiting, oldest first, each with its folder.
    static func waiting() -> [(item: SharedItem, folder: URL)] {
        guard let root,
              let folders = try? FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
        else { return [] }
        return folders
            .filter { !$0.lastPathComponent.hasPrefix(".") }
            .compactMap { folder in
                guard let data = try? Data(contentsOf: folder.appendingPathComponent("item.json")),
                      let item = try? JSONDecoder().decode(SharedItem.self, from: data)
                else { return nil }
                return (item, folder)
            }
            .sorted { $0.item.shared < $1.item.shared }
    }

    static func remove(_ folder: URL) {
        try? FileManager.default.removeItem(at: folder)
    }
}
