import Foundation
import OrbKit
import WidgetKit

/// The core, opened once per process. A widget's intent can launch the app
/// in the background with no window, so the core cannot belong to a screen.
@MainActor
final class OrbRuntime {
    static let shared = OrbRuntime()

    private(set) var service: OrbService?

    func open() async throws -> OrbService {
        if let service { return service }
        let opened = try await OrbService(configPath: AppModel.configPath)
        service = opened
        return opened
    }

    func adopt(_ service: OrbService) {
        self.service = service
    }

    func forget() {
        service = nil
    }
}

extension Notification.Name {
    /// Tasks changed outside the screens: a widget ticked one off.
    static let orbTasksChanged = Notification.Name("OrbTasksChanged")
}

/// What a widget's intent does, in the app's process.
enum WidgetActions {
    @MainActor
    static func complete(_ id: Int64) async throws {
        let service = try await OrbRuntime.shared.open()
        _ = try await service.run { try $0.completeTasks(taskIds: [id]) }
        await WidgetSnapshotWriter.update(service)
        NotificationCenter.default.post(name: .orbTasksChanged, object: nil)
    }
}

/// Turns what the share extension left into tasks in Today.
enum ShareInboxDrain {
    @MainActor
    static func run(_ service: OrbService) async -> Int {
        var made = 0
        for (item, folder) in ShareInbox.waiting() {
            let today = CivilDate.today
            let files = item.files.map { folder.appendingPathComponent($0).path }
            do {
                _ = try await service.run { orb in
                    let task = try orb.createTask(title: item.title.isEmpty ? "Shared" : item.title, postponeDate: today)
                    if let notes = item.notes { _ = try orb.setNotes(taskId: task.id, notes: notes) }
                    for file in files { _ = try orb.addAttachment(taskId: task.id, path: file) }
                }
                ShareInbox.remove(folder)
                made += 1
            } catch {
                // Left where it is, for the next run.
            }
        }
        return made
    }
}

/// Builds the widgets' snapshot from the core and hands it over.
enum WidgetSnapshotWriter {
    static func update(_ service: OrbService) async {
        guard let snapshot = try? await service.run(build) else { return }
        guard snapshot != WidgetSnapshot.read().with(written: snapshot.written) else { return }
        try? snapshot.write()
        WidgetCenter.shared.reloadAllTimelines()
    }

    @Sendable
    static func build(_ orb: Orb) throws -> WidgetSnapshot {
        let today = CivilDate.today
        let tasks = try orb.listTasks(view: .today, filter: nil)
        let open = tasks.filter { !$0.isCompleted && !$0.isDoneForToday }
        let done = tasks.filter { $0.isCompleted || $0.isDoneForToday }
        let row = { (task: TaskSummary) in
            WidgetSnapshot.Row(
                id: task.id,
                title: task.title,
                done: task.isCompleted || task.isDoneForToday,
                due: task.dueDate.map { task.isOverdue ? "! \(CivilDate.label($0))" : CivilDate.label($0) },
                overdue: task.isOverdue,
                dueToday: task.dueDate == today
            )
        }

        var workingOn: WidgetSnapshot.WorkingOn?
        if let task = try orb.workingOnTasks().first {
            let detail = try orb.getTask(taskId: task.id)
            workingOn = .init(
                id: task.id,
                title: task.title,
                nextStep: detail.steps.first { !$0.done }?.title,
                stepsDone: Int(task.stepsDone),
                stepsTotal: Int(task.stepsTotal)
            )
        }

        // The next reminder still to fire today, among Today's tasks.
        let now = Int64(Date.now.timeIntervalSince1970)
        let midnight = Int64(Calendar.current.startOfDay(for: .now.addingTimeInterval(86_400)).timeIntervalSince1970)
        var next: (TaskSummary, Int64)?
        for task in open where task.reminderCount > 0 {
            let reminders = try orb.getTask(taskId: task.id).reminders
            for reminder in reminders where !reminder.isTriggered && reminder.remindAtUnix > now && reminder.remindAtUnix < midnight {
                if next == nil || reminder.remindAtUnix < next!.1 { next = (task, reminder.remindAtUnix) }
            }
        }

        return WidgetSnapshot(
            written: .now,
            today: (open + done).prefix(9).map(row),
            openCount: open.count,
            doneCount: done.count,
            workingOn: workingOn,
            nextTimed: next.map { row($0.0) },
            nextTimedAt: next.map { Date(timeIntervalSince1970: TimeInterval($0.1)) }
        )
    }
}

extension WidgetSnapshot {
    func with(written: Date) -> WidgetSnapshot {
        var copy = self
        copy.written = written
        return copy
    }
}

#if DEBUG
import SwiftUI

/// The widgets at their sizes, from the snapshot as written (`orb://debug/widgets`).
struct WidgetGallery: View {
    @State private var snapshot = WidgetSnapshot.read()

    var body: some View {
        ScrollView {
            VStack(spacing: 20) {
                HStack(spacing: 16) {
                    tile(WorkingOnWidgetView(snapshot: snapshot), width: 158, height: 158)
                    VStack(spacing: 12) {
                        tile(TodayAccessoryView(snapshot: snapshot), width: 160, height: 72)
                        tile(DoneRingView(snapshot: snapshot), width: 72, height: 72)
                    }
                }
                tile(TodayWidgetView(snapshot: snapshot, rows: 3), width: 338, height: 158)
                tile(TodayWidgetView(snapshot: snapshot, rows: 9), width: 338, height: 354)
            }
            .padding()
        }
        .background(Color(.systemGroupedBackground))
    }

    private func tile(_ content: some View, width: CGFloat, height: CGFloat) -> some View {
        content
            .padding(14)
            .frame(width: width, height: height, alignment: .topLeading)
            .background(.background, in: RoundedRectangle(cornerRadius: 22))
    }
}
#endif
