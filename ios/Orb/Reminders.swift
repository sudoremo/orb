import Foundation
import OrbKit
import os
import UserNotifications

private let log = Logger(subsystem: "com.sudoremo.orb", category: "reminders")

/// Task reminders, delivered by iOS so that they fire while Orb is closed
/// (§5.1). The core decides what should be pending; this applies its plan,
/// reports what iOS delivered, and answers the notification's actions.
@MainActor
final class Reminders: NSObject, UNUserNotificationCenterDelegate {
    static let category = "orb.task"
    static let thread = "orb.tasks"
    /// Outside the `orb.task.` namespace the core owns, so a reconcile never
    /// cancels it; fixed, so a relaunch replaces yesterday's summary.
    static let missedSummary = "orb.tasks.missed-summary"

    private enum Action {
        static let complete = "complete"
        static let snoozeHour = "snooze-60"
        static let postponeTomorrow = "postpone-tomorrow"
        static let workOn = "work-on"
    }

    private let center = UNUserNotificationCenter.current()
    private weak var model: AppModel?

    init(model: AppModel) {
        self.model = model
        super.init()
        center.delegate = self
        center.setNotificationCategories([
            UNNotificationCategory(
                identifier: Self.category,
                actions: [
                    UNNotificationAction(identifier: Action.complete, title: "Complete", icon: .init(systemImageName: "checkmark.circle")),
                    UNNotificationAction(identifier: Action.snoozeHour, title: "Remind Me in 1 Hour", icon: .init(systemImageName: "clock")),
                    UNNotificationAction(identifier: Action.postponeTomorrow, title: "Postpone to Tomorrow", icon: .init(systemImageName: "sunrise")),
                    UNNotificationAction(identifier: Action.workOn, title: "Work On", icon: .init(systemImageName: "play")),
                ],
                intentIdentifiers: []
            ),
        ])
    }

    /// Asked once, right after setup: reminders that silently never fire
    /// are the worst failure (§5.1).
    /// Whether iOS will show a reminder at all.
    func allowed() async -> Bool {
        let settings = await center.notificationSettings()
        return [.authorized, .provisional, .ephemeral].contains(settings.authorizationStatus)
    }

    func requestPermission() async {
        let granted = (try? await center.requestAuthorization(options: [.alert, .sound, .badge])) ?? false
        log.notice("notifications \(granted ? "allowed" : "not allowed", privacy: .public)")
    }

    /// At launch: tell the core what iOS showed while Orb was closed, deliver
    /// what is less than a day overdue, and arm the rest.
    func launched(_ service: OrbService) async {
        await recordDelivered(service)
        if let sweep = try? await service.run({ try $0.catchUpReminders() }) {
            for due in sweep.due {
                let taskId = due.taskId
                let steps = (try? await service.run { try $0.getTask(taskId: taskId).steps }) ?? []
                post(identifier: due.notificationId, title: due.title, fireAt: nil, taskId: taskId, body: body(due.remindAt), steps: steps)
            }
            if let summary = sweep.missedSummary {
                post(identifier: Self.missedSummary, title: "Missed Task Reminders", fireAt: nil, taskId: nil, body: summary)
            }
        }
        await reconcile(service)
    }

    /// Bring iOS's pending requests in line with the database: after every
    /// change and sync, and when the day turns over.
    func reconcile(_ service: OrbService) async {
        let pending = await center.pendingNotificationRequests()
            .map(\.identifier)
            .filter { $0.hasPrefix("orb.task.") }
        let plan: TaskNotificationPlan
        do {
            plan = try await service.run { try $0.notificationPlan(pending: pending) }
        } catch {
            log.error("could not plan reminders: \(error.message, privacy: .public)")
            return
        }
        if !plan.schedule.isEmpty || !plan.cancel.isEmpty {
            log.notice("reminders: \(plan.schedule.count) to schedule, \(plan.cancel.count) to cancel")
        }
        center.removePendingNotificationRequests(withIdentifiers: plan.cancel)
        for notification in plan.schedule {
            let taskId = notification.taskId
            let steps = (try? await service.run { try $0.getTask(taskId: taskId).steps }) ?? []
            post(
                identifier: notification.identifier,
                title: notification.title,
                fireAt: notification.fireAt,
                taskId: taskId,
                body: body(notification.fireAt),
                steps: steps
            )
        }
    }

    private func recordDelivered(_ service: OrbService) async {
        let delivered = await center.deliveredNotifications()
            .map(\.request.identifier)
            .filter { $0.hasPrefix("orb.task.") }
        for identifier in delivered {
            _ = try? await service.run { try $0.recordNotificationDelivered(identifier: identifier) }
        }
    }

    /// Scheduled on the local wall clock, so 09:00 stays 09:00 in another
    /// timezone; without `fireAt`, delivered at once.
    /// `steps` travel in the payload, for the expanded notification to
    /// show without opening the database.
    private func post(identifier: String, title: String, fireAt: String?, taskId: Int64?, body: String, steps: [TaskStepInfo] = []) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        content.threadIdentifier = Self.thread
        if let taskId {
            content.categoryIdentifier = Self.category
            content.userInfo = [
                "task_id": taskId,
                "steps": steps.map { ["title": $0.title, "done": $0.done] },
            ]
        }
        var trigger: UNNotificationTrigger?
        if let fireAt, let date = Self.parse(fireAt) {
            let components = Calendar.current.dateComponents([.year, .month, .day, .hour, .minute], from: date)
            trigger = UNCalendarNotificationTrigger(dateMatching: components, repeats: false)
        }
        center.add(UNNotificationRequest(identifier: identifier, content: content, trigger: trigger)) { error in
            if let error {
                log.error("could not schedule \(identifier, privacy: .public): \(error.message, privacy: .public)")
            }
        }
    }

    private func body(_ stamp: String) -> String {
        guard let date = Self.parse(stamp) else { return "Reminder" }
        return "Reminder · \(date.formatted(date: .omitted, time: .shortened))"
    }

    private static func parse(_ stamp: String) -> Date? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.date(from: stamp)
    }

    // MARK: UNUserNotificationCenterDelegate

    // The completion-handler forms, finished on the main actor: UIKit asserts
    // when the async forms complete on another thread.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping @Sendable (UNNotificationPresentationOptions) -> Void
    ) {
        let identifier = notification.request.identifier
        Task { @MainActor in
            self.delivered(identifier)
            completionHandler([.banner, .list, .sound])
        }
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping @Sendable () -> Void
    ) {
        let identifier = response.notification.request.identifier
        let action = response.actionIdentifier
        let taskId = (response.notification.request.content.userInfo["task_id"] as? NSNumber)?.int64Value
        Task { @MainActor in
            await self.respond(identifier: identifier, action: action, taskId: taskId)
            completionHandler()
        }
    }

    private func delivered(_ identifier: String) {
        guard identifier.hasPrefix("orb.task."), let service = model?.service else { return }
        Task { _ = try? await service.run { try $0.recordNotificationDelivered(identifier: identifier) } }
    }

    private func respond(identifier: String, action: String, taskId: Int64?) async {
        log.notice("notification \(identifier, privacy: .public): \(action, privacy: .public), task \(taskId.map(String.init) ?? "none", privacy: .public)")
        guard let model, let service = await model.ready() else { return }
        if identifier.hasPrefix("orb.task.") {
            _ = try? await service.run { try $0.recordNotificationDelivered(identifier: identifier) }
        }
        switch action {
        case Action.complete:
            if let taskId {
                await model.perform { _ = try $0.completeTasks(taskIds: [taskId]) }
            }
        case Action.snoozeHour:
            await model.perform { _ = try $0.snoozeNotification(identifier: identifier, minutes: 60) }
        case Action.postponeTomorrow:
            if let taskId {
                let tomorrow = PostponeShortcut.tomorrow.date
                await model.perform("Postponed to Tomorrow") { _ = try $0.setPostponeDate(taskIds: [taskId], date: tomorrow) }
            }
        case Action.workOn:
            if let taskId {
                await model.perform { _ = try $0.setWorkingOn(taskId: taskId, on: true) }
            }
        case UNNotificationDefaultActionIdentifier:
            if let taskId { model.open(TaskRoute(id: taskId)) }
        default:
            break
        }
    }
}
