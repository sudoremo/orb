import Foundation
import BackgroundTasks
import Network
import OrbKit
import os
import SwiftUI
import UserNotifications

private let log = Logger(subsystem: "com.sudoremo.orb", category: "tasks")

/// The app's state: the core, once open, and what every tab shares.
@MainActor @Observable
final class AppModel {
    enum State {
        case starting
        case setup
        case ready
        case failed(String)
    }

    private(set) var state = State.starting
    private(set) var service: OrbService?
    /// Bumped whenever the tasks may have changed, so that every list reloads.
    private(set) var revision = 0
    private(set) var todayOpenCount: UInt32 = 0
    private(set) var workingOn: TaskSummary?
    private(set) var syncStatus: TaskSyncStatus?
    private(set) var syncing = false
    private(set) var online = true
    /// Whether reminders can fire; `nil` until asked.
    private(set) var notificationsAllowed: Bool?
    private(set) var todaySettings: TodaySettings?
    /// `username @ server`, for Settings.
    private(set) var accountDescription: String?
    var undoMessage: String?
    /// The tab shown, and the task opened in Today from a notification.
    var tab = MainTab.today
    var todayPath: [TaskRoute] = []

    private var pathMonitor: NWPathMonitor?
    private var uploads: Task<Void, Never>?
    private var reminders: Reminders?
    private var dayWatch: NSObjectProtocol?

    /// Quick add asked for from outside the app (the widget's +).
    var quickAddRequested = false
    #if DEBUG
    /// The widgets drawn in the app, to look at them without a Home Screen.
    var showWidgetGallery = false
    #endif
    private var widgetWatch: NSObjectProtocol?
    private var todayWatch: NSObjectProtocol?

    init() {
        reminders = Reminders(model: self)
        widgetWatch = NotificationCenter.default.addObserver(forName: .orbTasksChanged, object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in await self?.refresh() }
        }
        todayWatch = NotificationCenter.default.addObserver(forName: .orbShowToday, object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in
                self?.tab = .today
                self?.todayPath = []
            }
        }
    }

    /// `orb://quick-add` and `orb://task/<id>`, from the widgets.
    func handle(_ url: URL) {
        guard url.scheme == "orb" else { return }
        switch url.host() {
        case "quick-add":
            tab = .today
            todayPath = []
            quickAddRequested = true
        case "task":
            if let id = Int64(url.lastPathComponent) { open(TaskRoute(id: id)) }
        #if DEBUG
        case "debug":
            showWidgetGallery = true
        #endif
        default:
            break
        }
    }

    /// As listed in the Info.plist, from the bundle identifier.
    static let refreshTask = "\(Bundle.main.bundleIdentifier ?? "com.sudoremo.orb").refresh"
    static let directory = URL.applicationSupportDirectory.appendingPathComponent("Orb", isDirectory: true)
    static let configPath = directory.appendingPathComponent("config.yml").path

    func start() async {
        #if DEBUG
        if await setUpFromEnvironment() { return }
        #endif
        guard FileManager.default.fileExists(atPath: Self.configPath) else {
            state = .setup
            return
        }
        await open()
    }

    /// Write the configuration the setup screen asked for, file the
    /// password, and open the core on it.
    func setUp(_ setup: TasksAccountSetup, password: String, today: TodaySettings? = nil) async throws {
        try FileManager.default.createDirectory(at: Self.directory, withIntermediateDirectories: true)
        try writeTasksConfig(configPath: Self.configPath, dataDir: Self.directory.path, setup: setup, today: today)
        OrbRuntime.shared.forget()
        let service = try await OrbRuntime.shared.open()
        if let account = service.orb.tasksAccount() {
            try await service.run { try $0.storePassword(account: account, password: password) }
        }
        await open(service)
        await reminders?.requestPermission()
    }

    /// Forget the configuration and the tasks cached with it, and start over.
    func reset() {
        enteredBackground()
        try? FileManager.default.removeItem(at: Self.directory)
        OrbRuntime.shared.forget()
        service = nil
        state = .setup
    }

    private func open(_ opened: OrbService? = nil) async {
        do {
            let service = if let opened { opened } else { try await OrbRuntime.shared.open() }
            self.service = service
            state = .ready
            watchNetwork(service)
            watchUploads(service)
            watchDay()
            await reminders?.launched(service)
            await refresh()
            await sync()
        } catch {
            state = .failed(error.message)
        }
    }

    func becameActive() async {
        guard service != nil else { return }
        // Coming back from the Settings app may have changed the permission.
        await refresh()
        await sync()
    }

    /// Upload what is still waiting within the grace iOS gives an app that
    /// leaves the foreground, then stop the uploader until it comes back.
    func enteredBackground() {
        scheduleRefresh()
        guard let service else { return }
        service.orb.stopTaskUploads()
        uploads?.cancel()
        uploads = nil
        guard (syncStatus?.pending ?? 0) > 0 else { return }
        let grace = UIApplication.shared.beginBackgroundTask(withName: "Upload task changes")
        Task {
            _ = try? await service.run { try $0.syncTasks() }
            await refresh()
            UIApplication.shared.endBackgroundTask(grace)
        }
    }

    /// A refresh iOS granted: take in other machines' changes, so reminders,
    /// the badge and the widgets are current when the phone is next looked at
    /// (IOS_PLAN.md 5.1).
    func backgroundRefresh() async {
        scheduleRefresh()
        if service == nil, FileManager.default.fileExists(atPath: Self.configPath) {
            service = try? await OrbRuntime.shared.open()
            if service != nil { state = .ready }
        }
        guard let service else { return }
        do {
            _ = try await service.run { try $0.syncTasks() }
        } catch {
            log.error("background refresh: \(String(describing: error), privacy: .public)")
        }
        await refresh()
    }

    private func scheduleRefresh() {
        let request = BGAppRefreshTaskRequest(identifier: Self.refreshTask)
        request.earliestBeginDate = .now.addingTimeInterval(15 * 60)
        do {
            try BGTaskScheduler.shared.submit(request)
        } catch {
            log.notice("background refresh not scheduled: \(String(describing: error), privacy: .public)")
        }
    }

    /// Ask the server for other machines' changes and send ours.
    func sync() async {
        guard let service, !syncing else { return }
        syncing = true
        defer { syncing = false }
        _ = try? await service.run { try $0.syncTasks() }
        if uploads == nil { watchUploads(service) }
        await refresh()
    }

    func setTodaySettings(_ settings: TodaySettings) async {
        await perform { try $0.setTodaySettings(settings: settings) }
    }

    /// Re-read what every tab shows.
    func refresh() async {
        guard let service else { return }
        let shared = await ShareInboxDrain.run(service)
        if shared > 0 { undoMessage = shared == 1 ? "Added a shared task" : "Added \(shared) shared tasks" }
        todayOpenCount = (try? await service.run { orb in
            try orb.listTasks(view: .today, filter: nil).filter { !$0.isCompleted && !$0.isDoneForToday }.count
        }).map(UInt32.init) ?? 0
        try? await UNUserNotificationCenter.current().setBadgeCount(Int(todayOpenCount))
        workingOn = try? await service.run { try $0.workingOnTasks().first }
        syncStatus = try? await service.run { try $0.taskSyncStatus() }
        notificationsAllowed = await reminders?.allowed()
        todaySettings = service.orb.todaySettings()
        accountDescription = service.orb.accountDescription()
        revision += 1
        await reminders?.reconcile(service)
        await WidgetSnapshotWriter.update(service)
        await SpotlightIndex.update(service)
    }

    /// The core once it is open, waiting for a launch still under way — a
    /// notification's action can arrive before it is.
    func ready() async -> OrbService? {
        for _ in 0..<100 {
            if let service { return service }
            if case .ready = state { return service }
            if case .setup = state { return nil }
            if case .failed = state { return nil }
            try? await Task.sleep(for: .milliseconds(100))
        }
        return service
    }

    /// Show a task in Today, on top of whatever was open there.
    func open(_ route: TaskRoute) {
        tab = .today
        todayPath = [route]
    }

    /// Run a change to tasks, then reload. `undo` names it for the undo pill.
    func perform(_ undo: String? = nil, _ change: @escaping @Sendable (Orb) throws -> Void) async {
        guard let service else { return }
        do {
            try await service.run(change)
            if let undo { undoMessage = undo }
        } catch {
            log.error("a change failed: \(String(describing: error), privacy: .public)")
            undoMessage = error.message
        }
        await refresh()
    }

    func undo() async {
        guard let service else { return }
        let report = try? await service.run { try $0.undo() }
        undoMessage = report.map { "Undone: \($0.label)" }
        await refresh()
    }

    /// The day turning over changes Today and brings the next reminders
    /// into iOS's pending window (§4.3, §5.1).
    private func watchDay() {
        guard dayWatch == nil else { return }
        dayWatch = NotificationCenter.default.addObserver(
            forName: UIApplication.significantTimeChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            Task { @MainActor in await self?.refresh() }
        }
    }

    private func watchNetwork(_ service: OrbService) {
        guard pathMonitor == nil else { return }
        let monitor = NWPathMonitor()
        let seen = SeenPath()
        monitor.pathUpdateHandler = { [weak self] path in
            // The first update is the network Orb started on, not a move.
            if seen.replace(with: path), path.status == .satisfied { service.orb.networkChanged() }
            let online = path.status == .satisfied
            Task { @MainActor in
                guard let self, self.online != online else { return }
                self.online = online
                if online { await self.sync() }
            }
        }
        monitor.start(queue: DispatchQueue(label: "orb.network"))
        pathMonitor = monitor
    }

    private func watchUploads(_ service: OrbService) {
        let events = service.taskSyncEvents()
        uploads = Task { [weak self] in
            for await event in events where event.tasksChanged || event.problemChanged {
                await self?.refresh()
            }
        }
    }
}

#if DEBUG
extension AppModel {
    /// Set up from the launch environment rather than the setup screen, for
    /// running against a test account in the simulator and for the UI tests:
    /// `ORB_SETUP=server:port:username:folder`, the password in
    /// `ORB_SETUP_PASSWORD_FILE` or `ORB_SETUP_PASSWORD`, and `ORB_RESET=1`
    /// to start from nothing.
    fileprivate func setUpFromEnvironment() async -> Bool {
        let environment = ProcessInfo.processInfo.environment
        if environment["ORB_RESET"] == "1" {
            try? FileManager.default.removeItem(at: Self.directory)
            // The device's own preferences too, such as Group by Tag: a UI
            // test that switches one on would otherwise leave it on for the next.
            if let domain = Bundle.main.bundleIdentifier {
                UserDefaults.standard.removePersistentDomain(forName: domain)
            }
        }
        // A setup code as the scanner would read it, for a check without a camera.
        if let text = environment["ORB_SETUP_CODE"] {
            do {
                let code = try readSetupCode(text: text)
                try await setUp(code.setup, password: code.password, today: code.today)
            } catch {
                state = .failed(error.message)
            }
            return true
        }
        guard let fields = environment["ORB_SETUP"]?.split(separator: ":", maxSplits: 3).map(String.init),
              fields.count == 4, let port = UInt16(fields[1]),
              let password = environment["ORB_SETUP_PASSWORD"]
                ?? environment["ORB_SETUP_PASSWORD_FILE"].flatMap({ try? String(contentsOfFile: $0, encoding: .utf8) })
        else { return false }
        let setup = TasksAccountSetup(imapServer: fields[0], imapPort: port, imapUsername: fields[2], folder: fields[3])
        do {
            try await setUp(setup, password: password.trimmingCharacters(in: .whitespacesAndNewlines))
        } catch {
            state = .failed(error.message)
        }
        return true
    }
}
#endif

/// The path NWPathMonitor last reported, touched only on its serial queue.
private final class SeenPath: @unchecked Sendable {
    private var path: NWPath?

    /// Keep `new`, and say whether it differs from a path seen before it.
    func replace(with new: NWPath) -> Bool {
        defer { path = new }
        return path.map { $0 != new } ?? false
    }
}
