import OrbKit
import SwiftUI

/// One sheet for Postpone, Due and Remind, titled by what it sets. Typing
/// turns the shortcuts and the calendar into the readings of what was typed
/// (§12.1). The deadline and the reminders of a repeating task are read
/// against its day (§32): "on the day", "2 days after", "day before 18:00".
struct DateSheet: View {
    enum Kind { case postpone, due, remind }

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let task: TaskSummary
    let kind: Kind

    @State private var day = Date.now
    @State private var time = Calendar.current.date(bySettingHour: 9, minute: 0, second: 0, of: .now)!
    @State private var typed = ""
    @State private var readings: [DateSuggestionInfo] = []
    @State private var busy: Set<String> = []
    /// The day what is typed is read against, for a repeating task.
    @State private var seriesDay: String?

    private var placeholder: String {
        switch (kind, seriesDay) {
        case (.remind, .some): "on the day 9am, day before 18:00"
        case (.remind, nil): "tomorrow 9am, fri 14:00"
        case (_, .some): "on the day, 2 days after, +3"
        case (_, nil): "tomorrow, fri, in 3 weeks"
        }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField(placeholder, text: $typed)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .submitLabel(.done)
                        .onSubmit { if let first = readings.first { take(first) } }
                }
                // No calendar for a repeating task: a day picked from one would
                // look like a date for this occurrence alone.
                if typed.trimmingCharacters(in: .whitespaces).isEmpty && seriesDay == nil {
                    picker
                } else {
                    Section {
                        if readings.isEmpty {
                            Text("No matching date").foregroundStyle(.secondary)
                        }
                        ForEach(readings, id: \.self) { reading in
                            Button { take(reading) } label: {
                                LabeledContent(reading.primary) { Text(reading.secondary).foregroundStyle(.tertiary) }
                            }
                            .tint(.primary)
                        }
                    }
                    if seriesDay != nil && kind == .due {
                        Section {
                            Button("No Deadline", role: .destructive) { set(nil) }
                        }
                    }
                }
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", systemImage: "xmark", role: .cancel) { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Set", systemImage: "checkmark", role: .confirm) { setPicked() }
                        .disabled(seriesDay != nil && readings.isEmpty)
                }
            }
            .onAppear {
                let current = kind == .due ? task.dueDate : kind == .postpone ? task.postponeDate : nil
                if let current, let date = CivilDate.date(current) { day = date }
            }
            .task(id: typed) { await read() }
            .task { await loadBusy() }
            .task { await loadSeries() }
        }
        .presentationDetents([.medium, .large])
    }

    /// The day of a repeating task, for its deadline and its reminders. A
    /// postpone date moves one occurrence and is a plain date, as is the
    /// deadline of a task whose day is its due date.
    private func loadSeries() async {
        guard task.isRecurring, kind != .postpone, let service = model.service else { return }
        let id = task.id
        guard let series = try? await service.run({ try $0.getTask(taskId: id).series }) else { return }
        if kind == .due && series.dayIsDue { return }
        seriesDay = series.day
        if let date = CivilDate.date(series.day), kind == .remind || task.dueDate == nil { day = date }
        await read()
    }

    /// The days that already have a task, postponed or due, for the dots.
    private func loadBusy() async {
        guard let service = model.service else { return }
        let tasks = (try? await service.run { orb in
            try orb.listTasks(view: .upcoming, filter: nil)
        }) ?? []
        busy = Set(tasks.filter { !$0.isCompleted }.flatMap { [$0.postponeDate, $0.dueDate].compactMap { $0 } })
    }

    @ViewBuilder
    private var picker: some View {
        if kind != .remind {
            Section {
                ForEach(PostponeShortcut.allCases) { shortcut in
                    Button { set(shortcut.date) } label: {
                        LabeledContent {
                            Text(CivilDate.label(shortcut.date))
                        } label: {
                            Label(shortcut.title, systemImage: shortcut.symbol)
                        }
                    }
                }
            }
        }
        Section {
            MonthCalendar(selection: $day, busy: busy)
            if kind == .remind {
                DatePicker("Time", selection: $time, displayedComponents: .hourAndMinute)
            }
        }
        if kind != .remind {
            Section {
                Button("Clear Date", role: .destructive) { set(nil) }
            }
        }
    }

    private var title: String {
        switch kind {
        case .postpone: "Postpone"
        case .due: seriesDay == nil ? "Due" : "Due, Every Occurrence"
        case .remind: seriesDay == nil ? "Remind" : "Remind, Every Occurrence"
        }
    }

    private func read() async {
        let input = typed.trimmingCharacters(in: .whitespaces)
        // Against the day of a series, nothing typed lists the usual distances.
        guard !input.isEmpty || seriesDay != nil, let service = model.service else {
            readings = []
            return
        }
        let (remind, day) = (kind == .remind, seriesDay)
        readings = (try? await service.run {
            remind ? try $0.suggestDatetimes(input: input, day: day) : try $0.suggestDates(input: input, day: day)
        }) ?? []
    }

    private func take(_ reading: DateSuggestionInfo) {
        if kind == .remind {
            remind(date: reading.date, time: reading.time ?? "09:00")
        } else {
            set(reading.date)
        }
    }

    private func setPicked() {
        if seriesDay != nil {
            if let first = readings.first { take(first) }
        } else if kind == .remind {
            remind(date: CivilDate.string(day), time: time.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits)))
        } else {
            set(CivilDate.string(day))
        }
    }

    private func set(_ date: String?) {
        let (id, kind) = (task.id, kind)
        let undo = date.map { "\(kind == .postpone ? "Postponed to" : "Due") \(CivilDate.label($0))" } ?? "Cleared the date"
        Task {
            await model.perform(undo) { orb in
                if kind == .due {
                    _ = try orb.setDueDate(taskIds: [id], date: date)
                } else {
                    _ = try orb.setPostponeDate(taskIds: [id], date: date)
                }
            }
        }
        dismiss()
    }

    /// A reminder at a local date and `HH:MM`, sent as RFC 3339 with the
    /// offset of that moment.
    private func remind(date: String, time: String) {
        let parts = time.split(separator: ":").compactMap { Int($0) }
        guard let day = CivilDate.date(date), parts.count >= 2,
              let moment = Calendar.current.date(bySettingHour: parts[0], minute: parts[1], second: 0, of: day)
        else { return }
        let rfc3339 = Moment.rfc3339(moment)
        let id = task.id
        Task { await model.perform("Reminder set") { _ = try $0.addReminder(taskId: id, remindAt: rfc3339) } }
        dismiss()
    }
}

/// How a task repeats (§8), typed as a person says it.
struct RepeatSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let task: TaskSummary
    let current: String?
    @State var fromCompletion: Bool

    @State private var typed = ""
    @State private var readings: [RecurrenceSuggestionInfo] = []

    var body: some View {
        NavigationStack {
            Form {
                if let current {
                    Section("Now") { Text(current) }
                }
                Section {
                    TextField("every monday, monthly on the 1st", text: $typed)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .submitLabel(.done)
                        .onSubmit { if let first = readings.first { set(first.rrule) } }
                    if !typed.trimmingCharacters(in: .whitespaces).isEmpty && readings.isEmpty {
                        Text("No matching rule").foregroundStyle(.secondary)
                    }
                    ForEach(readings, id: \.rrule) { reading in
                        Button(reading.label) { set(reading.rrule) }.tint(.primary)
                    }
                }
                Section {
                    Toggle("Repeat from Completion", isOn: $fromCompletion)
                } footer: {
                    Text("The next date counts from the day the task was completed rather than the day it was due.")
                }
                if current != nil {
                    Section {
                        Button("Never Repeat", role: .destructive) { set(nil) }
                    }
                }
            }
            .navigationTitle("Repeat")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", systemImage: "xmark", role: .cancel) { dismiss() }
                }
            }
            .task(id: typed) {
                let input = typed.trimmingCharacters(in: .whitespaces)
                guard !input.isEmpty, let service = model.service else {
                    readings = []
                    return
                }
                readings = (try? await service.run { try $0.suggestRecurrence(input: input) }) ?? []
            }
        }
        .presentationDetents([.medium, .large])
    }

    private func set(_ rrule: String?) {
        let (id, fromCompletion) = (task.id, fromCompletion)
        Task {
            await model.perform(rrule == nil ? "Stopped repeating" : "Repeat set") {
                _ = try $0.setRecurrence(taskId: id, rrule: rrule, fromCompletion: fromCompletion)
            }
        }
        dismiss()
    }
}

/// Multi-select over the tag registry (§7).
struct TagsSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let task: TaskSummary

    @State private var tags: [TaskTagInfo] = []
    @State private var picked: Set<String> = []
    @State private var editor = TagEditor()
    @State private var query = ""

    var body: some View {
        NavigationStack {
            List {
                if canCreate {
                    Button("Create #\(trimmed)", systemImage: "plus") { create() }
                }
                ForEach(shown, id: \.id) { tag in
                    Button {
                        if picked.contains(tag.id) { picked.remove(tag.id) } else { picked.insert(tag.id) }
                    } label: {
                        HStack {
                            Text(tag.name).foregroundStyle(Color(hex: tag.colour) ?? .primary)
                            Spacer()
                            if picked.contains(tag.id) {
                                Image(systemName: "checkmark").foregroundStyle(Color.accentColor)
                            }
                        }
                    }
                    .tint(.primary)
                    .swipeActions { TagSwipeActions(tag: tag, editor: editor) }
                }
            }
            .tagEditing(editor, model: model)
            .searchable(text: $query, placement: .navigationBarDrawer(displayMode: .always), prompt: "Tags")
            .navigationTitle("Tags")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", systemImage: "xmark", role: .cancel) { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done", systemImage: "checkmark", role: .confirm) { save() }
                }
            }
            .task { picked = Set(task.tags.map(\.id)) }
            .task(id: model.revision) {
                tags = (try? await model.service?.run { try $0.listTags() }) ?? []
                picked.formIntersection(tags.map(\.id))
            }
        }
        .presentationDetents([.medium, .large])
    }

    private var trimmed: String {
        query.trimmingCharacters(in: .whitespaces).trimmingCharacters(in: CharacterSet(charactersIn: "#"))
    }

    private var shown: [TaskTagInfo] {
        trimmed.isEmpty ? tags : tags.filter { $0.name.localizedCaseInsensitiveContains(trimmed) }
    }

    private var canCreate: Bool {
        !trimmed.isEmpty && !tags.contains { $0.name.caseInsensitiveCompare(trimmed) == .orderedSame }
    }

    private func create() {
        let name = trimmed
        Task {
            guard let tag = try? await model.service?.run({ try $0.createTag(name: name) }) else { return }
            tags.append(tag)
            picked.insert(tag.id)
            query = ""
        }
    }

    private func save() {
        let (id, ids) = (task.id, tags.map(\.id).filter(picked.contains) + picked.subtracting(tags.map(\.id)))
        Task { await model.perform { _ = try $0.setTags(taskId: id, tagIds: ids) } }
        dismiss()
    }
}

/// The card above the keyboard that makes a task in two seconds. The title
/// is read as it is typed: a date phrase at its end and every `#tag` become
/// chips; tapping a chip drops it and leaves the words in the title.
struct QuickAddSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    /// Where the new task is postponed to unless the title says otherwise:
    /// today, when made from Today.
    let postponeDate: String?

    @State private var title = ""
    @State private var notes = ""
    @State private var reading: DateReading?
    @State private var dropped: Set<String> = []
    @State private var due: Date?
    @State private var remind: Date?
    @State private var steps: [String] = []
    @State private var newStep = ""
    @State private var addingStep = false
    @State private var added = 0
    @FocusState private var focus: Field?

    enum Field { case title, notes, step }

    struct DateReading: Equatable {
        let phrase: String
        let date: String
        let label: String
    }

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 12) {
                TextField("New Task", text: $title)
                    .font(.title3)
                    .focused($focus, equals: .title)
                    .submitLabel(.return)
                    .onSubmit(save)
                TextField("Notes", text: $notes, axis: .vertical)
                    .focused($focus, equals: .notes)
                    .lineLimit(1...5)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("quickadd.notes")
                chips
                ForEach(steps, id: \.self) { step in
                    Label(step, systemImage: "square").foregroundStyle(.secondary)
                }
                if addingStep {
                    TextField("Step", text: $newStep)
                        .focused($focus, equals: .step)
                        .submitLabel(.next)
                        .onSubmit {
                            let step = newStep.trimmingCharacters(in: .whitespaces)
                            if !step.isEmpty { steps.append(step) }
                            newStep = ""
                            focus = .step
                        }
                }
                if let due {
                    DatePicker("Due", selection: Binding(get: { due }, set: { self.due = $0 }), displayedComponents: .date)
                }
                if let remind {
                    DatePicker("Remind", selection: Binding(get: { remind }, set: { self.remind = $0 }))
                }
                HStack(spacing: 10) {
                    Button(due == nil ? "Due" : "No Due", systemImage: "calendar") {
                        due = due == nil ? Calendar.current.date(byAdding: .day, value: 1, to: .now) : nil
                    }
                    Button(remind == nil ? "Remind" : "No Reminder", systemImage: "bell") {
                        remind = remind == nil ? Moment.tomorrowNine : nil
                    }
                    Button("Steps", systemImage: "checklist") {
                        addingStep = true
                        focus = .step
                    }
                }
                .buttonStyle(.bordered)
                .font(.subheadline)
                Spacer(minLength: 0)
            }
            .padding()
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Close", systemImage: "xmark", role: .cancel) { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add", systemImage: "arrow.up", role: .confirm, action: save)
                        .disabled(cleanTitle.isEmpty)
                }
            }
            .navigationTitle(added == 0 ? "" : added == 1 ? "1 Added" : "\(added) Added")
            .navigationBarTitleDisplayMode(.inline)
            .task(id: "\(title)|\(dropped.sorted())") { await read() }
        }
        .presentationDetents([.medium, .large])
        .onAppear { focus = .title }
    }

    @ViewBuilder
    private var chips: some View {
        let tags = tagWords
        if reading != nil || !tags.isEmpty {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    if let reading {
                        chip(reading.label, "calendar") { dropped.insert(reading.phrase) }
                    }
                    ForEach(tags, id: \.self) { tag in
                        chip("#\(tag)", "tag") { dropped.insert("#" + tag) }
                    }
                }
            }
        }
    }

    private func chip(_ text: String, _ symbol: String, drop: @escaping () -> Void) -> some View {
        Button(action: drop) {
            Label(text, systemImage: symbol)
                .font(.subheadline)
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .background(Color.accentColor.opacity(0.15), in: Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityHint("Drops it; the words stay in the title")
    }

    private var words: [String] {
        title.split(whereSeparator: \.isWhitespace).map(String.init)
    }

    /// The `#tags` in the title that have not been dropped.
    private var tagWords: [String] {
        words.filter { $0.count > 1 && $0.hasPrefix("#") && !dropped.contains($0) }.map { String($0.dropFirst()) }
    }

    /// The title without what became chips.
    private var cleanTitle: String {
        var kept = words.filter { !($0.count > 1 && $0.hasPrefix("#") && !dropped.contains($0)) }
        if let reading {
            let phrase = reading.phrase.split(separator: " ").map(String.init)
            if kept.count > phrase.count, Array(kept.suffix(phrase.count)) == phrase {
                kept.removeLast(phrase.count)
            }
        }
        return kept.joined(separator: " ")
    }

    /// The longest phrase at the end of the title, up to four words, that
    /// reads as a date (§12.1). A bare number is not taken for one.
    private func read() async {
        guard let service = model.service else { return }
        let plain = words.filter { !$0.hasPrefix("#") }
        guard plain.count > 1 else {
            reading = nil
            return
        }
        for length in stride(from: min(4, plain.count - 1), through: 1, by: -1) {
            let phrase = plain.suffix(length).joined(separator: " ")
            guard !dropped.contains(phrase), !phrase.allSatisfy(\.isNumber) else { continue }
            if let first = try? await service.run({ try $0.suggestDates(input: phrase, day: nil) }).first {
                reading = DateReading(phrase: phrase, date: first.date, label: CivilDate.label(first.date))
                return
            }
        }
        reading = nil
    }

    /// Save, and open a fresh card for the next one.
    private func save() {
        let title = cleanTitle
        guard !title.isEmpty else { return }
        let postpone = reading?.date ?? postponeDate
        let dueDate = due.map(CivilDate.string)
        let reminder = remind.map(Moment.rfc3339)
        let (tags, steps) = (tagWords, steps)
        let notes = notes.trimmingCharacters(in: .whitespacesAndNewlines)
        Task {
            await model.perform { orb in
                let task = try orb.createTask(title: title, postponeDate: postpone)
                if !notes.isEmpty { _ = try orb.setNotes(taskId: task.id, notes: notes) }
                if let dueDate { _ = try orb.setDueDate(taskIds: [task.id], date: dueDate) }
                if let reminder { _ = try orb.addReminder(taskId: task.id, remindAt: reminder) }
                for step in steps { _ = try orb.addStep(taskId: task.id, title: step) }
                if !tags.isEmpty {
                    let known = try orb.listTags()
                    let ids = try tags.map { name in
                        try known.first { $0.name.caseInsensitiveCompare(name) == .orderedSame }?.id
                            ?? orb.createTag(name: name).id
                    }
                    _ = try orb.setTags(taskId: task.id, tagIds: ids)
                }
            }
        }
        added += 1
        self.title = ""
        self.notes = ""
        reading = nil
        dropped = []
        due = nil
        remind = nil
        self.steps = []
        addingStep = false
        focus = .title
    }
}

/// The tasks a Merge was asked for, in list order.
struct MergeRequest: Identifiable {
    let ids: [Int64]
    var id: [Int64] { ids }
}

/// Merge Tasks (todo-spec §29), as on the Mac: which task stays, its title,
/// notes and dates as proposed by the core and changed here; everything else
/// comes along and the others go to the Trash.
struct MergeSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let ids: [Int64]

    @State private var plan: TaskMergePlan?
    @State private var keep: Int64 = 0
    @State private var title = ""
    /// The title the last plan proposed; one typed over it is kept.
    @State private var proposed = ""
    @State private var notes = ""
    @State private var postpone: String?
    @State private var due: String?

    var body: some View {
        NavigationStack {
            Form {
                if let plan {
                    Section("Keep") {
                        Picker("Keep", selection: $keep) {
                            ForEach(plan.tasks, id: \.id) { Text($0.title).tag($0.id) }
                        }
                        .pickerStyle(.inline)
                        .labelsHidden()
                    }
                    Section("Title") {
                        TextField("Title", text: $title)
                            .accessibilityIdentifier("merge.title")
                    }
                    Section("Notes") {
                        TextField("Notes", text: $notes, axis: .vertical)
                            .lineLimit(3...10)
                    }
                    Section {
                        datePicker("Postpone", plan.tasks.compactMap(\.postponeDate), $postpone)
                        datePicker("Due", plan.tasks.compactMap(\.dueDate), $due)
                    } footer: {
                        Text(along(plan))
                    }
                } else {
                    ProgressView()
                }
            }
            .navigationTitle("Merge \(ids.count) Tasks")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", systemImage: "xmark", role: .cancel) { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Merge", systemImage: "checkmark", role: .confirm, action: merge)
                        .disabled(plan == nil || title.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            .task(id: keep) { await load() }
        }
    }

    private func datePicker(_ label: String, _ dates: [String], _ selection: Binding<String?>) -> some View {
        Picker(label, selection: selection) {
            Text("None").tag(String?.none)
            ForEach(Array(Set(dates)).sorted(), id: \.self) { Text(CivilDate.label($0)).tag(String?.some($0)) }
        }
    }

    private func along(_ plan: TaskMergePlan) -> String {
        let count = { (n: UInt32, one: String) in n == 0 ? nil : n == 1 ? "1 \(one)" : "\(n) \(one)s" }
        let parts = [count(plan.steps, "step"), count(plan.tags, "tag"), count(plan.reminders, "reminder"),
                     count(plan.links, "linked message"), count(plan.attachments, "file")].compactMap { $0 }
        let others = plan.tasks.count - 1
        let trash = others == 1 ? "The other task goes to the Trash." : "The other \(others) tasks go to the Trash."
        return (parts.isEmpty ? "" : "Comes along: \(parts.joined(separator: ", ")). ") + trash
    }

    /// The plan for the task kept: its title unless one was typed, the notes
    /// in its order, and the earliest dates.
    private func load() async {
        guard let service = model.service else { return }
        let keep = keep == 0 ? ids[0] : keep
        let others = ids.filter { $0 != keep }
        guard let plan = try? await service.run({ try $0.planTaskMerge(keep: keep, others: others) }) else { return }
        self.plan = plan
        self.keep = keep
        if let kept = plan.tasks.first {
            if title == proposed { title = kept.title }
            proposed = kept.title
        }
        notes = plan.body ?? ""
        postpone = plan.postponeDate
        due = plan.dueDate
    }

    private func merge() {
        let (keep, title) = (keep, title.trimmingCharacters(in: .whitespaces))
        let others = ids.filter { $0 != keep }
        let notes = notes.trimmingCharacters(in: .whitespacesAndNewlines)
        let (postpone, due) = (postpone, due)
        dismiss()
        Task {
            await model.perform("Merged \(others.count + 1) tasks") {
                _ = try $0.mergeTasks(keep: keep, others: others, title: title, notes: notes.isEmpty ? nil : notes,
                                      postponeDate: postpone, dueDate: due)
            }
        }
    }
}
