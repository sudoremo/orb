import Foundation
import Testing
@testable import OrbKit

/// The whole path from Swift into the core and back, without a server: a
/// configuration written, opened, and a task made and listed.
@Test func aTaskMadeInSwiftIsListedInToday() async throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let config = dir.appendingPathComponent("config.yml").path

    try writeTasksConfig(
        configPath: config,
        dataDir: dir.appendingPathComponent("data").path,
        setup: TasksAccountSetup(imapServer: "imap.invalid", imapPort: 993, imapUsername: "me@example.com", folder: "Tasks")
    )
    let service = try await OrbService(configPath: config)

    #expect(service.orb.tasksConfigured())
    let today = Date.ISO8601FormatStyle(timeZone: .current).year().month().day().format(.now)
    let made = try await service.run { try $0.createTask(title: "Water the plants", postponeDate: today) }
    let listed = try await service.run { try $0.listTasks(view: .today, filter: nil) }
    #expect(listed.contains { $0.id == made.id && $0.title == "Water the plants" })
}

/// A repeating task says its deadline against its day (todo-spec §32), and a
/// date typed for it is read against that day.
@Test func aRepeatingTaskSaysItsDeadlineAgainstItsDay() async throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let config = dir.appendingPathComponent("config.yml").path

    try writeTasksConfig(
        configPath: config,
        dataDir: dir.appendingPathComponent("data").path,
        setup: TasksAccountSetup(imapServer: "imap.invalid", imapPort: 993, imapUsername: "me@example.com", folder: "Tasks")
    )
    let service = try await OrbService(configPath: config)
    let made = try await service.run { try $0.createTask(title: "Water the plants", postponeDate: "2030-01-07") }
    _ = try await service.run { try $0.setRecurrence(taskId: made.id, rrule: "FREQ=WEEKLY", fromCompletion: false) }

    let readings = try await service.run { try $0.suggestDates(input: "2 days after", day: "2030-01-07") }
    #expect(readings.first?.date == "2030-01-09")
    _ = try await service.run { try $0.setDueDate(taskIds: [made.id], date: "2030-01-09") }

    let detail = try await service.run { try $0.getTask(taskId: made.id) }
    #expect(detail.series?.day == "2030-01-07")
    #expect(detail.series?.dueLabel == "2 days after")
}

/// An error from the core reads as its message, not as Swift source.
@Test func anErrorFromTheCoreReadsAsItsMessage() {
    let error: Error = OrbError.InvalidInput(message: "No matching date")
    #expect(error.message == "No matching date")
}
