import AppIntents

/// Ticks a task off from a widget. A live-activity intent so that iOS runs it
/// in the app's process, which may open the database, rather than in the
/// widget's, which must not; the tick is then an ordinary edit, logged,
/// synced and undoable in the app.
struct CompleteTaskIntent: LiveActivityIntent {
    static let title: LocalizedStringResource = "Complete Task"
    static let isDiscoverable = false

    @Parameter(title: "Task") var taskId: Int

    init() {}

    init(taskId: Int64) {
        self.taskId = Int(taskId)
    }

    func perform() async throws -> some IntentResult {
        #if ORB_APP
        try await WidgetActions.complete(Int64(taskId))
        #endif
        return .result()
    }
}
