import OrbKit
import PhotosUI
import QuickLook
import SwiftUI

/// Pushed from any list: opens a task by its row id.
struct TaskRoute: Hashable {
    let id: Int64
}

/// A task with everything it holds, edited in place. Pickers are sheets over
/// it, so the task stays where it is.
struct TaskDetailView: View {
    @Environment(AppModel.self) private var model
    let id: Int64

    @State private var detail: TaskDetail?
    @State private var title = ""
    @State private var notes = ""
    @State private var newStep = ""
    /// The step being renamed in place, and what its title reads so far.
    @State private var editingStep: String?
    @State private var stepTitle = ""
    /// Steps picked out to tick, delete or drag together, while selecting.
    @State private var selectingSteps = false
    @State private var selectedSteps = Set<String>()
    @State private var sheet: Sheet?
    @State private var shownLink: TaskLinkInfo?
    @State private var previewing: URL?
    @State private var opening: Int64?
    @State private var importingFile = false
    @State private var photo: PhotosPickerItem?
    @FocusState private var focus: Field?

    enum Sheet: Identifiable {
        case postpone, due, remind, repeatRule, tags
        var id: Self { self }
    }

    enum Field { case title, notes, newStep, step }

    var body: some View {
        Group {
            if let detail {
                form(detail)
            } else {
                ProgressView()
            }
        }
        .navigationBarTitleDisplayMode(.inline)
        .task(id: model.revision) { await load() }
        .onChange(of: focus) { old, _ in
            if old == .title { saveTitle() }
            if old == .notes { saveNotes() }
        }
        .onDisappear {
            saveTitle()
            saveNotes()
        }
        .sheet(item: $sheet) { sheet in
            if let task = detail?.task {
                switch sheet {
                case .postpone: DateSheet(task: task, kind: .postpone)
                case .due: DateSheet(task: task, kind: .due)
                case .remind: DateSheet(task: task, kind: .remind)
                case .repeatRule: RepeatSheet(task: task, current: detail?.recurrenceLabel, fromCompletion: detail?.recurrenceFromCompletion ?? false)
                case .tags: TagsSheet(task: task)
                }
            }
        }
        .sheet(item: $shownLink) { LinkSheet(link: $0) }
        .quickLookPreview($previewing)
        .fileImporter(isPresented: $importingFile, allowedContentTypes: [.item]) { result in
            guard case .success(let url) = result else { return }
            attachFile(url)
        }
        .onChange(of: photo) { _, item in
            guard let item else { return }
            photo = nil
            attachPhoto(item)
        }
    }

    private func form(_ detail: TaskDetail) -> some View {
        let task = detail.task
        return Form {
            Section {
                HStack(alignment: .top, spacing: 14) {
                    Button {
                        Task {
                            await model.perform(task.isCompleted ? nil : "Completed “\(task.title)”") { orb in
                                if task.isCompleted {
                                    _ = try orb.uncompleteTasks(taskIds: [task.id])
                                } else {
                                    _ = try orb.completeTasks(taskIds: [task.id])
                                }
                            }
                        }
                    } label: {
                        Image(systemName: task.isCompleted ? "checkmark.circle.fill" : "circle")
                            .font(.largeTitle)
                            .foregroundStyle(task.isCompleted ? Color.accentColor : .secondary)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(task.isCompleted ? "Completed" : "Complete")

                    VStack(alignment: .leading, spacing: 6) {
                        TextField("Title", text: $title, axis: .vertical)
                            .font(.title2.bold())
                            .focused($focus, equals: .title)
                            .submitLabel(.done)
                            .onSubmit { focus = nil }
                        marks(task)
                    }
                }
                .listRowBackground(Color.clear)
            }

            Section {
                field("Postpone", "sun.max", task.postponeDate.map(CivilDate.label)) { sheet = .postpone }
                field("Due", "calendar", task.dueDate.map(CivilDate.label), late: deadlineReached(task)) { sheet = .due }
                field("Remind", "bell", reminderSummary(detail.reminders)) { sheet = .remind }
                field("Repeat", "repeat", detail.recurrenceLabel) { sheet = .repeatRule }
                field("Tags", "tag", task.tags.isEmpty ? nil : task.tags.map(\.name).joined(separator: ", ")) { sheet = .tags }
            }

            if !detail.reminders.isEmpty {
                Section("Reminders") {
                    ForEach(detail.reminders, id: \.id) { reminder in
                        Label(reminderLabel(reminder), systemImage: reminder.isTriggered ? "bell.slash" : "bell")
                            .foregroundStyle(reminder.isTriggered ? .secondary : .primary)
                            .swipeActions {
                                Button("Delete", systemImage: "trash", role: .destructive) {
                                    let reminderId = reminder.id
                                    Task { await model.perform("Deleted a reminder") { try $0.deleteReminder(reminderId: reminderId) } }
                                }
                            }
                    }
                }
            }

            steps(detail)

            Section("Notes") {
                TextField("Notes", text: $notes, axis: .vertical)
                    .lineLimit(3...)
                    .focused($focus, equals: .notes)
            }

            if !detail.links.isEmpty {
                Section("Linked") {
                    ForEach(detail.links, id: \.messageId) { link in
                        Button { shownLink = link } label: { LinkRow(link: link).contentShape(Rectangle()) }
                            .buttonStyle(.plain)
                    }
                }
            }

            Section("Attachments") {
                ForEach(detail.attachments, id: \.id) { attachment in
                    Button { open(attachment, of: task.id) } label: {
                        HStack {
                            Label(attachment.filename, systemImage: "paperclip")
                            Spacer()
                            if opening == attachment.id {
                                ProgressView()
                            } else if let size = attachment.size {
                                Text(size, format: .byteCount(style: .file)).foregroundStyle(.secondary)
                            }
                        }
                        .contentShape(Rectangle())
                    }
                    .tint(.primary)
                    .swipeActions {
                        Button("Delete", systemImage: "trash", role: .destructive) {
                            let id = attachment.id
                            Task { await model.perform("Removed \(attachment.filename)") { try $0.deleteAttachment(attachmentId: id) } }
                        }
                    }
                }
                Menu {
                    PhotosPicker(selection: $photo, matching: .any(of: [.images, .videos])) {
                        Label("Photo or Video", systemImage: "photo")
                    }
                    Button("File", systemImage: "folder") { importingFile = true }
                } label: {
                    // The whole row, or only the label's width answers a tap.
                    Label("Add Attachment", systemImage: "plus")
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                }
            }

            if !detail.log.isEmpty {
                Section {
                    DisclosureGroup("History") {
                        ForEach(detail.log.reversed(), id: \.entryUuid) { entry in
                            VStack(alignment: .leading, spacing: 2) {
                                Text(entry.text)
                                Text(Date(timeIntervalSince1970: TimeInterval(entry.occurredAtUnix)).formatted(date: .abbreviated, time: .shortened))
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }
        }
        .toolbar { if selectingSteps { stepSelectionBar(detail) } }
        .toolbar(selectingSteps ? .hidden : .automatic, for: .tabBar)
        .onChange(of: detail.steps.map(\.id)) { _, ids in
            selectedSteps.formIntersection(ids)
            if ids.isEmpty { selectingSteps = false }
        }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Menu("More", systemImage: "ellipsis") {
                    if !detail.steps.isEmpty {
                        Button("Select Steps", systemImage: "checkmark.circle") { selectSteps([]) }
                    }
                    Button(task.isWorkingOn ? "Stop Working On" : "Work On", systemImage: task.isWorkingOn ? "pause" : "play") {
                        let on = !task.isWorkingOn
                        Task { await model.perform { _ = try $0.setWorkingOn(taskId: task.id, on: on) } }
                    }
                    Divider()
                    Button("Delete", systemImage: "trash", role: .destructive) {
                        Task { await model.perform("Deleted “\(task.title)”") { _ = try $0.trashTasks(taskIds: [task.id]) } }
                    }
                }
            }
        }
    }

    @ViewBuilder
    private func marks(_ task: TaskSummary) -> some View {
        if task.isWorkingOn || task.isDirty {
            HStack(spacing: 12) {
                if task.isWorkingOn {
                    Label("Working on", systemImage: "play.fill").foregroundStyle(Color.accentColor)
                }
                if task.isDirty {
                    Label("Not yet on the server", systemImage: "icloud.dashed").foregroundStyle(.orange)
                }
            }
            .font(.footnote)
        }
    }

    /// Whether an open task's deadline is today or has passed, which is drawn in red.
    private func deadlineReached(_ task: TaskSummary) -> Bool {
        guard let due = task.dueDate, !task.isCompleted else { return false }
        return task.isOverdue || due == CivilDate.today
    }

    private func field(_ name: String, _ symbol: String, _ value: String?, late: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            LabeledContent {
                Text(value ?? "None").foregroundStyle(late ? AnyShapeStyle(.red) : value == nil ? AnyShapeStyle(.tertiary) : AnyShapeStyle(.secondary))
            } label: {
                Label(name, systemImage: symbol)
            }
        }
        .tint(.primary)
    }

    @ViewBuilder
    private func steps(_ detail: TaskDetail) -> some View {
        let id = detail.task.id
        let done = detail.steps.filter(\.done).count
        let next = detail.steps.first { !$0.done }?.id
        Section {
            if !detail.steps.isEmpty {
                ProgressView(value: Double(done), total: Double(detail.steps.count))
            }
            ForEach(detail.steps, id: \.id) { step in
                if editingStep == step.id {
                    TextField("Step", text: $stepTitle)
                        .focused($focus, equals: .step)
                        .submitLabel(.done)
                        .onSubmit { renameStep(id, step) }
                        .onAppear { focus = .step }
                        .accessibilityIdentifier("step.rename")
                } else {
                    Button {
                        if selectingSteps {
                            selectedSteps.formSymmetricDifference([step.id])
                        } else {
                            Task { await model.perform { _ = try $0.setStepsDone(taskId: id, stepIds: [step.id], done: !step.done) } }
                        }
                    } label: {
                        HStack {
                            if selectingSteps {
                                Image(systemName: selectedSteps.contains(step.id) ? "checkmark.circle.fill" : "circle")
                                    .foregroundStyle(selectedSteps.contains(step.id) ? Color.accentColor : .secondary)
                                    .accessibilityHidden(true)
                            }
                            Image(systemName: step.done ? "checkmark.square.fill" : "square")
                                .foregroundStyle(step.done ? Color.accentColor : .secondary)
                            Text(step.title).strikethrough(step.done).foregroundStyle(step.done ? .secondary : .primary)
                            Spacer()
                            if step.id == next {
                                Text("Next").font(.caption.bold()).foregroundStyle(Color.accentColor)
                            }
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(selectingSteps && selectedSteps.contains(step.id) ? .isSelected : [])
                    .contextMenu {
                        Button("Select", systemImage: "checkmark.circle") { selectSteps([step.id]) }
                        Button("Rename", systemImage: "pencil") {
                            stepTitle = step.title
                            editingStep = step.id
                        }
                        Button("Delete", systemImage: "trash", role: .destructive) {
                            Task { await model.perform { _ = try $0.removeSteps(taskId: id, stepIds: [step.id]) } }
                        }
                    }
                }
            }
            .onDelete { offsets in
                let ids = offsets.map { detail.steps[$0].id }
                Task { await model.perform { _ = try $0.removeSteps(taskId: id, stepIds: ids) } }
            }
            .onMove { from, to in moveSteps(detail, from: from, to: to) }
            TextField("Add Step", text: $newStep)
                .focused($focus, equals: .newStep)
                .submitLabel(.next)
                .onSubmit {
                    let title = newStep.trimmingCharacters(in: .whitespaces)
                    guard !title.isEmpty else { return }
                    newStep = ""
                    focus = .newStep
                    Task { await model.perform { _ = try $0.addStep(taskId: id, title: title) } }
                }
        } header: {
            Text(detail.steps.isEmpty ? "Steps" : "Steps · \(done) of \(detail.steps.count)")
        }
    }

    private func selectSteps(_ ids: Set<String>) {
        // The keyboard would cover the bar the selection is acted on from.
        focus = nil
        editingStep = nil
        selectedSteps = ids
        selectingSteps = true
    }

    /// What is done to the selected steps: ticked or reopened one way for
    /// all, as a selection of tasks is completed, or deleted; each one edit.
    @ToolbarContentBuilder
    private func stepSelectionBar(_ detail: TaskDetail) -> some ToolbarContent {
        let id = detail.task.id
        let ids = detail.steps.map(\.id).filter(selectedSteps.contains)
        let done = !detail.steps.contains { selectedSteps.contains($0.id) && !$0.done }
        ToolbarItem(placement: .confirmationAction) {
            Button("Done", systemImage: "checkmark", role: .confirm) { selectingSteps = false }
                .accessibilityIdentifier("steps.done")
        }
        ToolbarItemGroup(placement: .bottomBar) {
            Button(done && !ids.isEmpty ? "Reopen" : "Mark as Done", systemImage: done && !ids.isEmpty ? "square" : "checkmark.square") {
                Task { await model.perform { _ = try $0.setStepsDone(taskId: id, stepIds: ids, done: !done) } }
            }
            .accessibilityIdentifier("steps.toggle")
            .disabled(ids.isEmpty)
            Spacer()
            Button("Delete", systemImage: "trash", role: .destructive) {
                selectingSteps = false
                Task { await model.perform("Deleted \(ids.count == 1 ? "a step" : "\(ids.count) steps")") { _ = try $0.removeSteps(taskId: id, stepIds: ids) } }
            }
            .accessibilityIdentifier("steps.delete")
            .disabled(ids.isEmpty)
        }
    }

    /// A dragged step that is selected carries the selection with it, and
    /// steps that were apart land side by side (§17).
    private func moveSteps(_ detail: TaskDetail, from: IndexSet, to: Int) {
        let all = detail.steps.map(\.id)
        var moving = Set(from.map { all[$0] })
        if selectingSteps, !moving.isDisjoint(with: selectedSteps) { moving.formUnion(selectedSteps) }
        let ids = all.filter(moving.contains)
        let target = UInt32(all[..<to].filter { !moving.contains($0) }.count)
        let taskId = detail.task.id
        Task { await model.perform { _ = try $0.moveSteps(taskId: taskId, stepIds: ids, to: target) } }
    }

    /// Keep a renamed step's title; an emptied one is left as it was.
    private func renameStep(_ taskId: Int64, _ step: TaskStepInfo) {
        let title = stepTitle.trimmingCharacters(in: .whitespaces)
        editingStep = nil
        guard !title.isEmpty, title != step.title else { return }
        Task { await model.perform { _ = try $0.renameStep(taskId: taskId, stepId: step.id, title: title) } }
    }

    /// Open an attachment in Quick Look, fetching it first if this iPhone
    /// has never had it.
    private func open(_ attachment: TaskAttachmentInfo, of taskId: Int64) {
        guard let service = model.service else { return }
        opening = attachment.id
        let uuid = attachment.attachmentUuid
        Task {
            defer { opening = nil }
            do {
                let path = try await service.run { try $0.attachmentPath(taskId: taskId, attachmentUuid: uuid) }
                previewing = URL(fileURLWithPath: path)
            } catch {
                model.undoMessage = error.message
            }
        }
    }

    /// A file from Files: read while its security scope is open, since the
    /// core copies it into the cache.
    private func attachFile(_ url: URL) {
        let id = id
        let scoped = url.startAccessingSecurityScopedResource()
        let path = url.path
        Task {
            await model.perform("Attached \(url.lastPathComponent)") { _ = try $0.addAttachment(taskId: id, path: path) }
            if scoped { url.stopAccessingSecurityScopedResource() }
        }
    }

    /// A photo or video from Photos, written to a temporary file under a
    /// name of its own for the core to copy.
    private func attachPhoto(_ item: PhotosPickerItem) {
        let id = id
        Task {
            guard let data = try? await item.loadTransferable(type: Data.self) else { return }
            let type = item.supportedContentTypes.first
            let name = "Photo \(Date.now.formatted(.iso8601.year().month().day().time(includingFractionalSeconds: false)))"
                .replacingOccurrences(of: ":", with: "-")
            let file = FileManager.default.temporaryDirectory
                .appendingPathComponent(name)
                .appendingPathExtension(type?.preferredFilenameExtension ?? "jpg")
            do {
                try data.write(to: file)
            } catch {
                model.undoMessage = error.message
                return
            }
            let path = file.path
            await model.perform("Attached a photo") { _ = try $0.addAttachment(taskId: id, path: path) }
            try? FileManager.default.removeItem(at: file)
        }
    }

    private func load() async {
        guard let service = model.service else { return }
        let id = id
        guard let loaded = try? await service.run({ try $0.getTask(taskId: id) }) else { return }
        detail = loaded
        if focus != .title { title = loaded.task.title }
        if focus != .notes { notes = loaded.body ?? "" }
    }

    private func saveTitle() {
        guard let detail else { return }
        let new = title.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !new.isEmpty, new != detail.task.title else {
            title = detail.task.title
            return
        }
        let id = id
        Task { await model.perform { _ = try $0.setTitle(taskId: id, title: new) } }
    }

    private func saveNotes() {
        guard let detail, notes != (detail.body ?? "") else { return }
        let (id, new) = (id, notes)
        Task { await model.perform { _ = try $0.setNotes(taskId: id, notes: new) } }
    }

    private func reminderSummary(_ reminders: [TaskReminderInfo]) -> String? {
        let pending = reminders.filter { !$0.isTriggered }
        guard let first = pending.min(by: { $0.remindAtUnix < $1.remindAtUnix }) else { return nil }
        return pending.count == 1 ? reminderLabel(first) : "\(reminderLabel(first)) +\(pending.count - 1)"
    }

    private func reminderLabel(_ reminder: TaskReminderInfo) -> String {
        Date(timeIntervalSince1970: TimeInterval(reminder.remindAtUnix)).formatted(date: .abbreviated, time: .shortened)
    }
}

/// A linked mail or chat message, from what the task keeps about it (§6.1).
struct LinkRow: View {
    let link: TaskLinkInfo

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: link.chat == nil ? "envelope" : "bubble.left")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(link.subject ?? "No Subject").lineLimit(1)
                if let from = link.from {
                    Text(from).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            Spacer()
            if link.answered {
                Image(systemName: "arrowshape.turn.up.left").font(.caption).foregroundStyle(.secondary)
                    .accessibilityLabel("Answered")
            }
        }
    }
}

/// No mail on the phone yet: a linked message shows what the task knows.
struct LinkSheet: View {
    @Environment(\.dismiss) private var dismiss
    let link: TaskLinkInfo

    var body: some View {
        NavigationStack {
            Form {
                LabeledContent("Subject", value: link.subject ?? "No Subject")
                if let from = link.from { LabeledContent("From", value: from) }
                if let date = link.date { LabeledContent("Date", value: date) }
                if let folder = link.folderName {
                    LabeledContent("Folder", value: [link.accountName, folder].compactMap { $0 }.joined(separator: " · "))
                }
                LabeledContent("Answered", value: link.answered ? (link.repliedAt ?? "Yes") : "No")
            }
            .navigationTitle(link.chat == nil ? "Linked Mail" : "Linked Message")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done", systemImage: "checkmark", role: .confirm) { dismiss() }
                }
            }
        }
        .presentationDetents([.medium])
    }
}

extension TaskLinkInfo: @retroactive Identifiable {
    public var id: String { messageId }
}
