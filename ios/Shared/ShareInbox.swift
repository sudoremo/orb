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
    /// How often the app has tried and failed to make the task.
    var attempts: Int?
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
            // Two files of one name — two photos called IMG_0001 — must not
            // make the share fail on the second copy.
            var name = file.lastPathComponent
            var next = 2
            while item.files.contains(name) {
                name = "\(next)-\(file.lastPathComponent)"
                next += 1
            }
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

    /// How often the app tries an item before giving it up.
    static let maxAttempts = 3

    /// Note that the app failed to make `item`'s task. The third failure
    /// removes it: a file the core cannot take would otherwise be read whole
    /// into memory on every launch for as long as the phone lasts.
    static func failed(_ item: SharedItem, in folder: URL) {
        var item = item
        item.attempts = (item.attempts ?? 0) + 1
        if item.attempts ?? 0 >= maxAttempts {
            remove(folder)
            return
        }
        try? JSONEncoder().encode(item).write(to: folder.appendingPathComponent("item.json"), options: .atomic)
    }
}
