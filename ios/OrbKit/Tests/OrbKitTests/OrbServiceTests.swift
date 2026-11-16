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

/// An error from the core reads as its message, not as Swift source.
@Test func anErrorFromTheCoreReadsAsItsMessage() {
    let error: Error = OrbError.InvalidInput(message: "No matching date")
    #expect(error.message == "No matching date")
}
