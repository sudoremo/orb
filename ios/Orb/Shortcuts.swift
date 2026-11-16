import AppIntents
import CoreSpotlight
import OrbKit
import UIKit

/// Capture a task without opening Orb: from Shortcuts, Siri or the Action
/// button. It lands in Today, as one made there does.
struct AddTaskIntent: AppIntent {
    static let title: LocalizedStringResource = "Add Task"
    static let description = IntentDescription("Adds a task to Today in Orb.")

    @Parameter(title: "Title", requestValueDialog: "What is the task?")
    var title: String

    static var parameterSummary: some ParameterSummary {
        Summary("Add \(\.$title) to Today")
    }

    @MainActor
    func perform() async throws -> some IntentResult & ProvidesDialog {
        let service = try await OrbRuntime.shared.open()
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        let today = CivilDate.today
        _ = try await service.run { try $0.createTask(title: title, postponeDate: today) }
        await WidgetSnapshotWriter.update(service)
        NotificationCenter.default.post(name: .orbTasksChanged, object: nil)
        return .result(dialog: "Added “\(title)” to Today.")
    }
}

/// Open Orb on Today.
struct ShowTodayIntent: AppIntent {
    static let title: LocalizedStringResource = "Show Today"
    static let description = IntentDescription("Opens Orb on Today.")
    static let supportedModes: IntentModes = .foreground

    @MainActor
    func perform() async throws -> some IntentResult {
        NotificationCenter.default.post(name: .orbShowToday, object: nil)
        return .result()
    }
}

struct OrbShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: AddTaskIntent(),
            phrases: ["Add a task in \(.applicationName)", "New \(.applicationName) task"],
            shortTitle: "Add Task",
            systemImageName: "plus.circle"
        )
        AppShortcut(
            intent: ShowTodayIntent(),
            phrases: ["Show Today in \(.applicationName)", "What's on today in \(.applicationName)"],
            shortTitle: "Today",
            systemImageName: "sun.max"
        )
    }
}

extension Notification.Name {
    static let orbShowToday = Notification.Name("OrbShowToday")
}

/// Open tasks in Spotlight, each opening its task.
@MainActor
enum SpotlightIndex {
    private static let domain = "tasks"
    /// What the index holds now, by item identifier, so an unchanged list
    /// is not indexed again.
    private static var indexed: [String: String] = [:]

    /// Replace the index when what it would hold has changed.
    @MainActor
    static func update(_ service: OrbService) async {
        guard let tasks = try? await service.run({ orb in
            try [TaskViewName.upcoming, .someday]
                .flatMap { try orb.listTasks(view: $0, filter: nil) }
                .filter { !$0.isCompleted }
        }) else { return }
        let wanted = Dictionary(tasks.map { ("task-\($0.id)", $0.title) }, uniquingKeysWith: { first, _ in first })
        guard wanted != indexed else { return }
        let items = tasks.map { task in
            let attributes = CSSearchableItemAttributeSet(contentType: .content)
            attributes.title = task.title
            attributes.contentDescription = [task.dueDate.map { "Due \(CivilDate.label($0))" }, task.tags.isEmpty ? nil : task.tags.map { "#\($0.name)" }.joined(separator: " ")]
                .compactMap { $0 }.joined(separator: " · ")
            return CSSearchableItem(uniqueIdentifier: "task-\(task.id)", domainIdentifier: domain, attributeSet: attributes)
        }
        let index = CSSearchableIndex.default()
        try? await index.deleteSearchableItems(withDomainIdentifiers: [domain])
        try? await index.indexSearchableItems(items)
        indexed = wanted
    }

    /// The task a Spotlight result stands for.
    nonisolated static func route(_ activity: NSUserActivity) -> TaskRoute? {
        guard let identifier = activity.userInfo?[CSSearchableItemActivityIdentifier] as? String,
              identifier.hasPrefix("task-"), let id = Int64(identifier.dropFirst(5))
        else { return nil }
        return TaskRoute(id: id)
    }
}

/// The Home Screen quick action, which UIKit hands to a scene delegate.
final class QuickActionDelegate: NSObject, UIWindowSceneDelegate {
    static let newTask = "com.sudoremo.orb.new-task"

    func windowScene(
        _ windowScene: UIWindowScene,
        performActionFor shortcutItem: UIApplicationShortcutItem,
        completionHandler: @escaping (Bool) -> Void
    ) {
        if shortcutItem.type == Self.newTask { UIApplication.shared.open(OrbLink.quickAdd) }
        completionHandler(true)
    }

    func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
        if options.shortcutItem?.type == Self.newTask {
            DispatchQueue.main.async { UIApplication.shared.open(OrbLink.quickAdd) }
        }
    }
}

final class OrbAppDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        configurationForConnecting session: UISceneSession,
        options: UIScene.ConnectionOptions
    ) -> UISceneConfiguration {
        let configuration = UISceneConfiguration(name: nil, sessionRole: session.role)
        configuration.delegateClass = QuickActionDelegate.self
        return configuration
    }
}
