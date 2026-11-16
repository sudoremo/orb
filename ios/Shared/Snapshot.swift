import Foundation

/// What the widgets show, written by the app after every change and read
/// by the widgets. A widget never opens the database: iOS terminates an app
/// that holds a SQLite lock in a shared container while it is suspended.
struct WidgetSnapshot: Codable, Equatable, Sendable {
    struct Row: Codable, Equatable, Sendable, Identifiable {
        let id: Int64
        let title: String
        let done: Bool
        /// "! 24 Oct", "Today" or "Fri"; `nil` without a due date.
        let due: String?
        let overdue: Bool
        let dueToday: Bool
    }

    struct WorkingOn: Codable, Equatable, Sendable {
        let id: Int64
        let title: String
        let nextStep: String?
        let stepsDone: Int
        let stepsTotal: Int
    }

    var written: Date
    /// Today's tasks, the open ones first, as many as a large widget holds.
    var today: [Row]
    var openCount: Int
    var doneCount: Int
    var workingOn: WorkingOn?
    /// The next reminder still to fire today, with its time.
    var nextTimed: Row?
    var nextTimedAt: Date?

    static let empty = WidgetSnapshot(written: .distantPast, today: [], openCount: 0, doneCount: 0)

    /// From the bundle's Info.plist, where the build writes it from the
    /// bundle prefix (Orb.xcconfig).
    static let appGroup = Bundle.main.object(forInfoDictionaryKey: "OrbAppGroup") as? String ?? "group.com.sudoremo.orb"

    private static var url: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup)?
            .appendingPathComponent("widget-snapshot.json")
    }

    static func read() -> WidgetSnapshot {
        guard let url, let data = try? Data(contentsOf: url),
              let snapshot = try? JSONDecoder().decode(WidgetSnapshot.self, from: data)
        else { return .empty }
        return snapshot
    }

    func write() throws {
        guard let url = Self.url else { throw CocoaError(.fileNoSuchFile) }
        try JSONEncoder().encode(self).write(to: url, options: .atomic)
    }
}

/// The links a widget opens the app with.
enum OrbLink {
    static let quickAdd = URL(string: "orb://quick-add")!
    static func task(_ id: Int64) -> URL { URL(string: "orb://task/\(id)")! }
}
