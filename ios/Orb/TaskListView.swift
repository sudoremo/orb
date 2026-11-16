import OrbKit
import SwiftUI

/// One of the task views of the core: Today, Upcoming, Someday, Completed,
/// Archived or Trash.
struct TaskListView: View {
    @Environment(AppModel.self) private var model
    let view: TaskViewName

    @State private var tasks: [TaskSummary] = []
    @State private var tags: [TaskTagInfo] = []
    /// Today, Upcoming and Someday under their tags (§30), on this device.
    @AppStorage("tasksByTag") private var byTag = false
    /// Today's tasks due by today above the rest (§31), on this device.
    @AppStorage("todayDueFirst") private var dueFirstOn = false
    @State private var loaded = false
    @State private var adding = false
    @State private var postponing: TaskSummary?
    @State private var due: TaskSummary?
    @State private var merging: MergeRequest?
    @State private var filter = ""
    @State private var editMode = EditMode.inactive
    @State private var selection = Set<Int64>()

    private var selecting: Bool { editMode.isEditing }

    var body: some View {
        // Selection only while selecting: otherwise it takes a row's tap from
        // its link, which on the iPad opens the task beside the list.
        List(selection: selecting ? $selection : nil) {
            if view == .today, !model.online, let pending = model.syncStatus?.pending, pending > 0 {
                Label(
                    pending == 1 ? "Offline. 1 change waits to upload." : "Offline. \(pending) changes wait to upload.",
                    systemImage: "icloud.dashed"
                )
                .font(.footnote)
                .foregroundStyle(.orange)
            } else if let problem = model.syncStatus?.problem, view == .today, model.online {
                Label(problem, systemImage: "exclamationmark.icloud")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
            if grouped {
                // A task with several tags is a row under each (§30).
                ForEach(TagGroup.grouping(tasks, tags: tags)) { group in
                    Section {
                        ForEach(Array(group.tasks.enumerated()), id: \.element.id) { at, task in
                            // The line under the tag's tasks due by today (§31).
                            let endsDue = todayBand(task) == 1 && group.tasks.indices.contains(at + 1)
                                && todayBand(group.tasks[at + 1]) != 1
                            row(task).listRowSeparatorTint(endsDue ? .red : nil)
                        }
                        .onMove(perform: view.isOrdered ? { move(from: $0, to: $1, in: group.tasks) } : nil)
                    } header: {
                        group.header
                    }
                }
            } else if view == .upcoming {
                // Under a heading per day, month and year, in date order;
                // reordering is for the views drawn in the global order.
                ForEach(SearchGroup.grouping(tasks)) { group in
                    Section {
                        ForEach(group.tasks) { row($0) }
                    } header: {
                        group.header
                    }
                }
            } else if dueBands {
                // A band each, so a drag stays within its band (§31).
                ForEach([0, 1, 2], id: \.self) { band in
                    let rows = tasks.filter { todayBand($0) == band }
                    if !rows.isEmpty {
                        Section {
                            ForEach(rows) { row($0) }
                                .onMove { move(from: $0, to: $1, in: rows) }
                        } header: {
                            bandHeader(band, count: rows.count)
                        }
                    }
                }
            } else {
                ForEach(tasks) { row($0) }
                    .onMove(perform: mover)
            }
        }
        .listStyle(.plain)
        .environment(\.editMode, $editMode)
        .searchable(text: $filter, placement: .navigationBarDrawer(displayMode: .automatic), prompt: "Filter")
        .overlay { if loaded && tasks.isEmpty { empty } }
        .navigationTitle(view.title)
        .navigationSubtitle(subtitle)
        .toolbar { toolbarItems }
        .toolbar(selecting ? .hidden : .automatic, for: .tabBar)
        .refreshable { await model.sync() }
        .task(id: "\(model.revision)|\(filter)|\(dueFirstOn)") { await load() }
        .onChange(of: editMode) { _, mode in if !mode.isEditing { selection = [] } }
        .onChange(of: model.quickAddRequested, initial: true) { _, requested in
            guard requested, view == .today else { return }
            model.quickAddRequested = false
            adding = true
        }
        .sheet(isPresented: $adding) {
            QuickAddSheet(postponeDate: view == .today ? CivilDate.today : nil)
        }
        .sheet(item: $postponing) { task in
            DateSheet(task: task, kind: .postpone)
        }
        .sheet(item: $due) { task in
            DateSheet(task: task, kind: .due)
        }
        .sheet(item: $merging) { request in
            MergeSheet(ids: request.ids)
        }
    }

    private func row(_ task: TaskSummary) -> some View {
        TaskLink(id: task.id) { TaskRow(task: task) }
            .swipeActions(edge: .leading) { leadingActions(task) }
            .swipeActions(edge: .trailing, allowsFullSwipe: view != .trash) { trailingActions(task) }
            .contextMenu { menu(task) }
    }

    private var mover: ((IndexSet, Int) -> Void)? {
        view.isOrdered ? { move(from: $0, to: $1, in: tasks) } : nil
    }

    private var grouped: Bool { byTag && view.groupsByTag }

    /// Today under Due and Other (§31): only while anything is due.
    private var dueBands: Bool { dueFirst && tasks.contains { todayBand($0) == 1 } }

    private var dueFirst: Bool { view == .today && dueFirstOn }

    /// Which band of Today a task stands in: the completed ones under
    /// Completed First (§24), then the due ones under Due First (§31), then
    /// the rest. Every other view is one band.
    private func todayBand(_ task: TaskSummary) -> Int {
        guard view == .today else { return 2 }
        if completedFirst && task.doneInToday { return 0 }
        return dueFirst && task.isDueNow ? 1 : 2
    }

    @ViewBuilder
    private func bandHeader(_ band: Int, count: Int) -> some View {
        if band == 1 {
            HStack {
                Label("Due", systemImage: "exclamationmark.triangle").foregroundStyle(.red)
                Spacer()
                Text("\(count)").monospacedDigit()
            }
        } else if band == 2 {
            Label("Other", systemImage: "sun.max").foregroundStyle(.secondary)
        }
    }

    /// How many of Today's tasks are open and due by today (§31).
    private var dueCount: Int { Set(tasks.filter(\.isDueNow).map(\.id)).count }

    @ToolbarContentBuilder
    private var toolbarItems: some ToolbarContent {
        if selecting {
            ToolbarItem(placement: .confirmationAction) {
                Button("Done", systemImage: "checkmark", role: .confirm) { editMode = .inactive }
            }
            ToolbarItemGroup(placement: .bottomBar) {
                selectionActions
            }
        } else {
            if view.takesNewTasks {
                ToolbarItem(placement: .primaryAction) {
                    Button("New Task", systemImage: "plus") { adding = true }
                }
            }
            ToolbarItem(placement: .secondaryAction) {
                Button("Select", systemImage: "checkmark.circle") { editMode = .active }
            }
            if view == .today {
                // The number due by today, in red, whether the switch is on or not (§31).
                ToolbarItem(placement: .topBarLeading) {
                    Toggle(isOn: $dueFirstOn) {
                        HStack(spacing: 3) {
                            Image(systemName: "exclamationmark.triangle")
                            if dueCount > 0 { Text("\(dueCount)").monospacedDigit().foregroundStyle(.red) }
                        }
                    }
                    .toggleStyle(.button)
                    .accessibilityIdentifier("dueFirst")
                    .accessibilityLabel("Due First")
                    .accessibilityValue(dueCount == 1 ? "1 due" : "\(dueCount) due")
                }
            }
            if view.groupsByTag {
                ToolbarItem(placement: .secondaryAction) {
                    Toggle("Group by Tag", systemImage: "tag", isOn: $byTag)
                }
            }
            if view == .today, tasks.contains(where: \.doneInToday) {
                ToolbarItem(placement: .secondaryAction) {
                    // On every device: the cut-off syncs like a task (§10.2).
                    Button("Hide Completed", systemImage: "eye.slash") {
                        Task { await model.perform { try $0.clearCompletedFromToday() } }
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var selectionActions: some View {
        let ids = Array(selection)
        // A toggle, as x is on the Mac: a selection that is all completed reopens.
        let reopening = !ids.isEmpty && tasks.filter { selection.contains($0.id) }.allSatisfy(\.isCompleted)
        Button(reopening ? "Reopen" : "Complete", systemImage: reopening ? "circle" : "checkmark.circle") {
            if reopening {
                bulk("Reopened \(ids.count)") { _ = try $0.uncompleteTasks(taskIds: ids) }
            } else {
                bulk("Completed \(ids.count)") { _ = try $0.completeTasks(taskIds: ids) }
            }
        }
        .accessibilityIdentifier("selection.complete")
        .disabled(ids.isEmpty)
        Spacer()
        Menu("Postpone", systemImage: "sunrise") {
            ForEach(PostponeShortcut.allCases) { shortcut in
                Button(shortcut.title, systemImage: shortcut.symbol) {
                    let date = shortcut.date
                    bulk("Postponed \(ids.count) to \(CivilDate.label(date))") { _ = try $0.setPostponeDate(taskIds: ids, date: date) }
                }
            }
        }
        .disabled(ids.isEmpty)
        Spacer()
        Button("Merge", systemImage: "arrow.triangle.merge") {
            // In list order, so the first one shown is the one proposed to stay.
            let ordered = tasks.map(\.id).filter(ids.contains)
            editMode = .inactive
            merging = MergeRequest(ids: ordered)
        }
        .accessibilityIdentifier("selection.merge")
        .disabled(ids.count < 2)
        Spacer()
        Button("Delete", systemImage: "trash", role: .destructive) {
            bulk("Deleted \(ids.count)") { _ = try $0.trashTasks(taskIds: ids) }
        }
        .accessibilityIdentifier("selection.delete")
        .disabled(ids.isEmpty)
    }

    private var subtitle: String {
        let pending = model.syncStatus?.pending ?? 0
        let waiting = pending == 1 ? "1 change to upload" : "\(pending) changes to upload"
        if !model.online { return pending > 0 ? "Offline · \(waiting)" : "Offline" }
        return pending > 0 ? waiting : ""
    }

    private var empty: some View {
        Group {
            if filter.isEmpty {
                ContentUnavailableView(view.emptyTitle, systemImage: view.symbol)
            } else {
                ContentUnavailableView.search(text: filter)
            }
        }
    }

    private func load() async {
        guard let service = model.service else { return }
        let (view, filter) = (view, filter)
        let listed = (try? await service.run { try $0.listTasks(view: view, filter: filter) }) ?? []
        tags = (try? await service.run { try $0.listTags() }) ?? []
        tasks = [0, 1, 2].flatMap { band in listed.filter { todayBand($0) == band } }
        loaded = true
    }

    /// Today with its completed tasks above the open ones (§24), as on the
    /// Mac: each group in the global order.
    private var completedFirst: Bool {
        view == .today && model.todaySettings?.completedFirstInToday == true
    }

    /// The moved rows land between their new neighbours in the one global
    /// order (§4.4); the neighbours are read with the moved rows taken out,
    /// and from the moved rows' own band of Today only (§24, §31). A
    /// selected row carries the selection with it, as one block. Grouped by
    /// tag, `rows` are the tag's alone, and a row moves within them (§30).
    private func move(from dragged: IndexSet, to destination: Int, in rows: [TaskSummary]) {
        var source = dragged
        if selecting, dragged.contains(where: { selection.contains(rows[$0].id) }) {
            source.formUnion(IndexSet(rows.indices.filter { selection.contains(rows[$0].id) }))
        }
        var reordered = rows
        reordered.move(fromOffsets: source, toOffset: destination)
        let moved = source.map { rows[$0].id }
        guard let first = reordered.firstIndex(where: { $0.id == moved.first }) else { return }
        let band = todayBand(reordered[first])
        let sameGroup = { (task: TaskSummary) in todayBand(task) == band }
        let previous = reordered[..<first].last(where: sameGroup)?.id
        let next = reordered[(first + moved.count)...].first(where: sameGroup)?.id
        if !grouped && !dueBands { tasks = reordered }
        Task { await model.perform { _ = try $0.moveTasks(taskIds: moved, previous: previous, next: next) } }
    }

    private func bulk(_ undo: String, _ change: @escaping @Sendable (Orb) throws -> Void) {
        editMode = .inactive
        Task { await model.perform(undo, change) }
    }

    @ViewBuilder
    private func leadingActions(_ task: TaskSummary) -> some View {
        if view == .trash {
            Button("Restore", systemImage: "arrow.uturn.backward") {
                let id = task.id
                Task { await model.perform("Restored “\(task.title)”") { _ = try $0.restoreTasks(taskIds: [id]) } }
            }
            .tint(.green)
        } else if !task.isCompleted {
            Button(task.isWorkingOn ? "Stop" : "Work On", systemImage: task.isWorkingOn ? "pause" : "play") {
                let (id, on) = (task.id, !task.isWorkingOn)
                Task { await model.perform { _ = try $0.setWorkingOn(taskId: id, on: on) } }
            }
            .tint(.accentColor)
        }
    }

    @ViewBuilder
    private func trailingActions(_ task: TaskSummary) -> some View {
        if view != .trash {
            Button("Tomorrow", systemImage: "sunrise") {
                // A full swipe postpones to tomorrow, the most common case.
                postpone(task, to: PostponeShortcut.tomorrow.date)
            }
            .tint(.orange)
            Button("Delete", systemImage: "trash", role: .destructive) {
                let id = task.id
                Task { await model.perform("Deleted “\(task.title)”") { _ = try $0.trashTasks(taskIds: [id]) } }
            }
            Button("Postpone", systemImage: "calendar") { postponing = task }
                .tint(.indigo)
        }
    }

    @ViewBuilder
    private func menu(_ task: TaskSummary) -> some View {
        ControlGroup {
            ForEach(PostponeShortcut.allCases) { shortcut in
                Button(shortcut.title, systemImage: shortcut.symbol) { postpone(task, to: shortcut.date) }
            }
        }
        .controlGroupStyle(.palette)
        Button("Postpone…", systemImage: "calendar") { postponing = task }
        Button("Select", systemImage: "checkmark.circle") {
            editMode = .active
            selection = [task.id]
        }
        Button("Due…", systemImage: "calendar.badge.exclamationmark") { due = task }
        Button(task.isWorkingOn ? "Stop Working On" : "Work On", systemImage: task.isWorkingOn ? "pause" : "play") {
            let (id, on) = (task.id, !task.isWorkingOn)
            Task { await model.perform { _ = try $0.setWorkingOn(taskId: id, on: on) } }
        }
        Divider()
        Button("Delete", systemImage: "trash", role: .destructive) {
            let id = task.id
            Task { await model.perform("Deleted “\(task.title)”") { _ = try $0.trashTasks(taskIds: [id]) } }
        }
    }

    private func postpone(_ task: TaskSummary, to date: String) {
        let id = task.id
        Task {
            await model.perform("Postponed to \(CivilDate.label(date))") {
                _ = try $0.setPostponeDate(taskIds: [id], date: date)
            }
        }
    }
}

extension TaskSummary: @retroactive Identifiable {
    /// Done as far as Today is concerned: completed, or a repeating task
    /// completed today and open again for its next date.
    var doneInToday: Bool { isCompleted || isDoneForToday }

    /// Open and due today or earlier (§31).
    var isDueNow: Bool {
        guard !doneInToday, let due = dueDate else { return false }
        return due <= CivilDate.today
    }
}

extension TaskViewName {
    var title: String {
        switch self {
        case .today: "Today"
        case .upcoming: "Upcoming"
        case .someday: "Someday"
        case .completed: "Completed"
        case .archive: "Archived"
        case .trash: "Trash"
        }
    }

    var symbol: String {
        switch self {
        case .today: "sun.max"
        case .upcoming: "calendar"
        case .someday: "sofa"
        case .completed: "checkmark.circle"
        case .archive: "archivebox"
        case .trash: "trash"
        }
    }

    var emptyTitle: String {
        switch self {
        case .today: "Nothing for Today"
        case .trash: "Trash Is Empty"
        default: "No Tasks"
        }
    }

    var takesNewTasks: Bool { [.today, .upcoming, .someday].contains(self) }

    /// The views drawn in the global order, where dragging a row means
    /// something (§4.4). Upcoming is drawn by date.
    var isOrdered: Bool { [.today, .someday].contains(self) }

    /// The views of open tasks, which can be listed under their tags (§30).
    var groupsByTag: Bool { [.today, .upcoming, .someday].contains(self) }
}

/// The tasks under one tag, or under none (§30).
struct TagGroup: Identifiable {
    /// The tag's id; nil for the tasks without one.
    let tag: TaskTagInfo?
    var tasks: [TaskSummary]
    var id: String { tag?.id ?? "" }

    var header: some View {
        HStack {
            if let tag {
                Label(tag.name, systemImage: "tag")
                    .foregroundStyle(Color(hex: tag.colour) ?? .secondary)
            } else {
                Label("No tag", systemImage: "tag")
                    .foregroundStyle(.tertiary)
            }
            Spacer()
            Text("\(tasks.count)").monospacedDigit()
        }
    }

    /// A group per tag in the registry's order, a tag the registry has not
    /// synced after the known ones, and the tasks without one last. Within
    /// a group the rows keep the order the view gave them.
    static func grouping(_ tasks: [TaskSummary], tags: [TaskTagInfo]) -> [TagGroup] {
        var groups: [TagGroup] = []
        var untagged: [TaskSummary] = []
        for task in tasks {
            if task.tags.isEmpty { untagged.append(task) }
            var seen = Set<String>()
            for tag in task.tags where seen.insert(tag.id).inserted {
                if let at = groups.firstIndex(where: { $0.tag?.id == tag.id }) {
                    groups[at].tasks.append(task)
                } else {
                    groups.append(TagGroup(tag: tags.first { $0.id == tag.id } ?? tag, tasks: [task]))
                }
            }
        }
        let order = Dictionary(tags.enumerated().map { ($1.id, $0) }, uniquingKeysWith: { first, _ in first })
        let sorted = groups.enumerated()
            .sorted { (order[$0.element.id] ?? tags.count, $0.offset) < (order[$1.element.id] ?? tags.count, $1.offset) }
            .map(\.element)
        return untagged.isEmpty ? sorted : sorted + [TagGroup(tag: nil, tasks: untagged)]
    }
}
