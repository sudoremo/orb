import OrbKit
import SwiftUI

/// The Mac sidebar's Tasks section, reached the way iOS reaches lists.
struct TasksTab: View {
    @Environment(AppModel.self) private var model

    @State private var tags: [TagCount] = []
    @State private var editor = TagEditor()

    private let views: [TaskViewName] = [.upcoming, .someday, .completed, .archive, .trash]

    var body: some View {
        List {
            Section {
                ForEach(views, id: \.self) { view in
                    NavigationLink(value: view) {
                        Label(view.title, systemImage: view.symbol)
                    }
                }
            }
            if !tags.isEmpty {
                Section("Tags") {
                    ForEach(tags, id: \.tag.id) { entry in
                        NavigationLink(value: TagRoute(name: entry.tag.name)) {
                            LabeledContent {
                                Text(entry.open, format: .number)
                            } label: {
                                Label {
                                    Text(entry.tag.name).foregroundStyle(Color(hex: entry.tag.colour) ?? .primary)
                                } icon: {
                                    Image(systemName: "tag")
                                }
                            }
                        }
                        .swipeActions { TagSwipeActions(tag: entry.tag, editor: editor) }
                    }
                }
            }
            Section {
                NavigationLink {
                    SettingsView()
                } label: {
                    Label("Settings", systemImage: "gear")
                }
            } footer: {
                if model.notificationsAllowed == false {
                    Text("Notifications are off: reminders will not fire.").foregroundStyle(.red)
                }
            }
        }
        .navigationTitle("Tasks")
        .navigationDestination(for: TaskViewName.self) { TaskListView(view: $0) }
        .navigationDestination(for: TagRoute.self) { SearchResults(source: .tag($0.name), title: "#\($0.name)") }
        .task(id: model.revision) { await loadTags() }
        .tagEditing(editor, model: model)
    }

    private func loadTags() async {
        guard let service = model.service else { return }
        tags = (try? await service.run { orb in
            try orb.listTags().filter(\.isKnown).map { tag in
                TagCount(tag: tag, open: try SearchResults.Source.tag(tag.name).find(orb).filter { !$0.isCompleted }.count)
            }
        }) ?? []
    }
}

struct TagCount: Sendable {
    let tag: TaskTagInfo
    let open: Int
}

struct TagRoute: Hashable {
    let name: String
}

/// The colours a tag can be given here.
enum TagColour: String, CaseIterable, Identifiable {
    case red, orange, yellow, green, teal, blue, purple, grey

    var id: Self { self }
    var name: String { rawValue.capitalized }

    var hex: String {
        switch self {
        case .red: "#e06060"
        case .orange: "#e0904a"
        case .yellow: "#d4b440"
        case .green: "#5aa860"
        case .teal: "#45a5a0"
        case .blue: "#5a8ee0"
        case .purple: "#9a6ad0"
        case .grey: "#8a8a8e"
        }
    }
}

/// Search over every task (§4.5).
struct SearchTab: View {
    @State private var query = ""

    var body: some View {
        SearchResults(source: .search(query), title: "Search")
            .searchable(text: $query, prompt: "Tasks")
    }
}

/// What a search finds, grouped as §26 groups it: Today, the next six days,
/// the months after, Someday, and Completed folded at the end.
struct SearchResults: View {
    /// What the results are of.
    enum Source: Hashable, Sendable {
        /// A search over every task but the trashed (§4.5).
        case search(String)
        /// The tasks with a tag, read through the views' filter, which takes
        /// a tag name of several words; the search does not.
        case tag(String)

        var isEmpty: Bool {
            switch self {
            case .search(let query): query.trimmingCharacters(in: .whitespaces).isEmpty
            case .tag(let name): name.isEmpty
            }
        }

        var text: String {
            switch self {
            case .search(let query): query
            case .tag(let name): "#\(name)"
            }
        }

        /// The tasks found, open ones first in the order §26 groups them in.
        func find(_ orb: Orb) throws -> [TaskSummary] {
            switch self {
            case .search(let query):
                return try orb.searchTasks(query: query.trimmingCharacters(in: .whitespaces))
            case .tag(let name):
                let filter = "tag:\"\(name)\""
                let open = try [TaskViewName.upcoming, .someday]
                    .flatMap { try orb.listTasks(view: $0, filter: filter) }
                    .filter { !$0.isCompleted }
                return open + (try orb.listTasks(view: .completed, filter: filter))
            }
        }
    }

    @Environment(AppModel.self) private var model
    let source: Source
    let title: String

    @State private var results: [TaskSummary] = []
    @State private var searched = ""
    @State private var showCompleted = false

    var body: some View {
        List {
            ForEach(SearchGroup.grouping(results.filter { !$0.isCompleted })) { group in
                Section {
                    ForEach(group.tasks) { task in
                        TaskLink(id: task.id) { TaskRow(task: task) }
                    }
                } header: {
                    group.header
                }
            }
            let completed = results.filter(\.isCompleted)
            if !completed.isEmpty {
                Section {
                    DisclosureGroup("Completed · \(completed.count)", isExpanded: $showCompleted) {
                        ForEach(completed) { task in
                            TaskLink(id: task.id) { TaskRow(task: task) }
                        }
                    }
                }
            }
        }
        .listStyle(.plain)
        .navigationTitle(title)
        .overlay {
            if !searched.isEmpty && results.isEmpty { ContentUnavailableView.search(text: searched) }
        }
        .task(id: "\(source)|\(model.revision)") {
            let source = source
            guard !source.isEmpty, let service = model.service else {
                results = []
                searched = ""
                return
            }
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled else { return }
            results = (try? await service.run { try source.find($0) }) ?? []
            searched = source.text
        }
    }
}

/// One heading of Upcoming and of search results (§26).
struct SearchGroup: Identifiable {
    enum Kind: Hashable {
        case today
        case day(String)
        case month(Int, Int)
        case year(Int)
        case someday
    }

    let kind: Kind
    var tasks: [TaskSummary]
    var id: Kind { kind }

    @ViewBuilder
    var header: some View {
        switch kind {
        case .today:
            Text("Today")
        case .day(let date):
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(CivilDate.date(date).map { $0.formatted(.dateTime.day()) } ?? date)
                    .font(.title2.bold())
                Text(CivilDate.label(date))
            }
        case .month(let year, let month):
            let date = Calendar.current.date(from: DateComponents(year: year, month: month, day: 1)) ?? .now
            let thisYear = Calendar.current.component(.year, from: .now) == year
            Text(thisYear ? date.formatted(.dateTime.month(.wide)) : date.formatted(.dateTime.month(.wide).year()))
        case .year(let year):
            Text(String(year))
        case .someday:
            Text("Someday")
        }
    }

    /// The core returns the tasks in §26's order; this only draws the lines
    /// between the groups: the six days after today one by one, then the
    /// months of the coming year, then whole years.
    static func grouping(_ tasks: [TaskSummary]) -> [SearchGroup] {
        let calendar = Calendar.current
        let today = calendar.startOfDay(for: .now)
        var groups: [SearchGroup] = []
        for task in tasks {
            let comes = [task.postponeDate, task.dueDate].compactMap { $0 }.min()
            let kind: Kind
            if let comes, let date = CivilDate.date(comes) {
                let days = calendar.dateComponents([.day], from: today, to: date).day ?? 0
                if days <= 0 {
                    kind = .today
                } else if days <= 6 {
                    kind = .day(comes)
                } else {
                    let now = calendar.dateComponents([.year, .month], from: today)
                    let parts = calendar.dateComponents([.year, .month], from: date)
                    let (year, month) = (parts.year ?? 0, parts.month ?? 0)
                    let ahead = (year - (now.year ?? 0)) * 12 + month - (now.month ?? 0)
                    kind = ahead >= 12 ? .year(year) : .month(year, month)
                }
            } else {
                kind = .someday
            }
            if let index = groups.firstIndex(where: { $0.kind == kind }) {
                groups[index].tasks.append(task)
            } else {
                groups.append(SearchGroup(kind: kind, tasks: [task]))
            }
        }
        return groups
    }
}

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openURL) private var openURL
    @State private var changingAccount = false

    var body: some View {
        Form {
            if let settings = model.todaySettings {
                Section {
                    Toggle("Keep Completed in Today", isOn: binding(settings, \.keepCompletedInToday))
                    Toggle("Completed First", isOn: binding(settings, \.completedFirstInToday))
                        .disabled(!settings.keepCompletedInToday)
                    Stepper(value: binding(settings, \.archiveAfterDays), in: 1...365) {
                        LabeledContent("Archive After", value: settings.archiveAfterDays == 1 ? "1 day" : "\(settings.archiveAfterDays) days")
                    }
                } header: {
                    Text("Today")
                } footer: {
                    Text("Kept on this iPhone, as each Mac keeps its own.")
                }
            }
            Section("Reminders") {
                if model.notificationsAllowed == false {
                    Button {
                        if let url = URL(string: UIApplication.openNotificationSettingsURLString) { openURL(url) }
                    } label: {
                        Label("Notifications are off: reminders will not fire", systemImage: "bell.slash")
                            .foregroundStyle(.red)
                    }
                } else {
                    Label("Notifications are on", systemImage: "bell")
                }
            }
            Section("Sync") {
                LabeledContent("Changes to upload", value: "\(model.syncStatus?.pending ?? 0)")
                if let problem = model.syncStatus?.problem {
                    Text(problem).foregroundStyle(.red)
                }
                Button("Sync Now") { Task { await model.sync() } }
                    .disabled(model.syncing)
            }
            Section {
                if let account = model.accountDescription {
                    LabeledContent("Account", value: account)
                }
                Button("Change Account", role: .destructive) { changingAccount = true }
            } footer: {
                Text("The tasks stay on the server; this iPhone forgets them, the account and its password.")
            }
        }
        .navigationTitle("Settings")
        .confirmationDialog(
            "Change the account?",
            isPresented: $changingAccount,
            titleVisibility: .visible
        ) {
            Button("Change Account", role: .destructive) { Task { await model.reset() } }
        } message: {
            let pending = model.syncStatus?.pending ?? 0
            if pending > 0 {
                Text(pending == 1
                    ? "1 change has not reached the server yet and will be lost."
                    : "\(pending) changes have not reached the server yet and will be lost.")
            } else {
                Text("Every change is on the server.")
            }
        }
    }

    private func binding<Value>(_ settings: TodaySettings, _ key: WritableKeyPath<TodaySettings, Value>) -> Binding<Value> {
        Binding {
            settings[keyPath: key]
        } set: { value in
            var changed = settings
            changed[keyPath: key] = value
            Task { await model.setTodaySettings(changed) }
        }
    }
}

/// The working-on mark (§19) above the tab bar, on every tab.
struct WorkingOnBar: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if let task = model.workingOn {
            HStack(spacing: 10) {
                Button {
                    let id = task.id
                    Task {
                        await model.perform("Completed “\(task.title)”") { orb in
                            _ = try orb.completeTasks(taskIds: [id])
                            _ = try orb.setWorkingOn(taskId: id, on: false)
                        }
                    }
                } label: {
                    Image(systemName: "circle")
                }
                .accessibilityLabel("Complete")
                Image(systemName: "play.fill").font(.caption).foregroundStyle(Color.accentColor)
                Text(task.title).lineLimit(1)
                Spacer()
            }
            .padding(.horizontal)
        }
    }
}

/// What the last change did, with the way back.
struct UndoPill: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if let message = model.undoMessage {
            HStack(spacing: 12) {
                Text(message).lineLimit(1)
                Button("Undo") { Task { await model.undo() } }
                    .bold()
            }
            .font(.subheadline)
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
            .glassEffect()
            .padding(.bottom, 120)
            .transition(.move(edge: .bottom).combined(with: .opacity))
            .task(id: message) {
                try? await Task.sleep(for: .seconds(4))
                model.undoMessage = nil
            }
        }
    }
}
