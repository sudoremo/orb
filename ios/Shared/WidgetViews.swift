import AppIntents
import SwiftUI
import WidgetKit

/// Today, interactive: each box ticks its task off without opening Orb,
/// and + opens quick add.
struct TodayWidgetView: View {
    let snapshot: WidgetSnapshot
    let rows: Int

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Label("Today", systemImage: "sun.max").font(.headline).foregroundStyle(.orange)
                Text(snapshot.openCount, format: .number).font(.headline).foregroundStyle(.secondary)
                Spacer()
                Link(destination: OrbLink.quickAdd) {
                    Image(systemName: "plus.circle.fill").font(.title2)
                }
                .accessibilityLabel("New Task")
            }
            if snapshot.today.isEmpty {
                Spacer()
                Text("Nothing for Today").font(.subheadline).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity)
                Spacer()
            } else {
                ForEach(snapshot.today.prefix(rows)) { row in
                    HStack(spacing: 8) {
                        Button(intent: CompleteTaskIntent(taskId: row.id)) {
                            Image(systemName: row.done ? "checkmark.circle.fill" : "circle")
                                .foregroundStyle(row.done ? Color.accentColor : .secondary)
                        }
                        .buttonStyle(.plain)
                        .disabled(row.done)
                        Link(destination: OrbLink.task(row.id)) {
                            HStack {
                                Text(row.title).lineLimit(1).strikethrough(row.done)
                                    .foregroundStyle(row.done ? .secondary : .primary)
                                Spacer(minLength: 4)
                                DueText(row: row)
                            }
                        }
                        .tint(.primary)
                    }
                    .font(.subheadline)
                }
                Spacer(minLength: 0)
            }
        }
    }
}

/// What is being worked on, with the next step and the progress; with
/// nothing marked, the next timed task instead.
struct WorkingOnWidgetView: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        if let task = snapshot.workingOn {
            VStack(alignment: .leading, spacing: 6) {
                Label("Working On", systemImage: "play.fill").font(.caption.bold()).foregroundStyle(Color.accentColor)
                Text(task.title).font(.headline).lineLimit(3)
                Spacer(minLength: 0)
                if let next = task.nextStep {
                    Text("Next: \(next)").font(.caption).foregroundStyle(.secondary).lineLimit(2)
                }
                if task.stepsTotal > 0 {
                    ProgressView(value: Double(task.stepsDone), total: Double(task.stepsTotal))
                }
            }
            .widgetURL(OrbLink.task(task.id))
        } else if let next = snapshot.nextTimed {
            VStack(alignment: .leading, spacing: 6) {
                Label("Next", systemImage: "bell").font(.caption.bold()).foregroundStyle(.secondary)
                Text(next.title).font(.headline).lineLimit(3)
                Spacer(minLength: 0)
                if let at = snapshot.nextTimedAt {
                    Text(at, style: .time).font(.caption).foregroundStyle(.secondary)
                }
            }
            .widgetURL(OrbLink.task(next.id))
        } else {
            VStack(spacing: 6) {
                Image(systemName: "sun.max").font(.title).foregroundStyle(.orange)
                Text(snapshot.openCount == 0 ? "All done" : "\(snapshot.openCount) for Today")
                    .font(.headline)
            }
        }
    }
}

/// The Lock Screen line: Today's count, the next timed task and the overdue.
struct TodayAccessoryView: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Label("\(snapshot.openCount) for Today", systemImage: "sun.max").font(.headline)
            // Redacted while the Lock Screen hides what is private, as the
            // notifications are: a task's title is the person's business.
            if let next = snapshot.nextTimed, let at = snapshot.nextTimedAt {
                Text("\(at.formatted(date: .omitted, time: .shortened)) \(next.title)").lineLimit(1).privacySensitive()
            }
            if let overdue = snapshot.today.first(where: { $0.overdue && !$0.done }) {
                Text("! \(overdue.title)").lineLimit(1).privacySensitive()
            }
        }
    }
}

/// The Lock Screen ring of what is done today.
struct DoneRingView: View {
    let snapshot: WidgetSnapshot

    var body: some View {
        let total = snapshot.openCount + snapshot.doneCount
        Gauge(value: Double(snapshot.doneCount), in: 0...Double(max(total, 1))) {
            Image(systemName: "checkmark")
        } currentValueLabel: {
            Text("\(snapshot.doneCount)")
        }
        .gaugeStyle(.accessoryCircularCapacity)
    }
}

/// The due date as the app's rows show it: red with "!" overdue, red today.
struct DueText: View {
    let row: WidgetSnapshot.Row

    var body: some View {
        if let due = row.due, !row.done {
            Text(due)
                .font(.caption)
                .foregroundStyle(row.overdue || row.dueToday ? .red : .secondary)
        }
    }
}
