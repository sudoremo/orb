import OrbKit
import SwiftUI

/// A task in a list: the box, the title, the Mac's marks at touch size, and
/// the due date on the right.
struct TaskRow: View {
    @Environment(AppModel.self) private var model
    let task: TaskSummary

    private var done: Bool { task.isCompleted || task.isDoneForToday }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Button {
                let id = task.id
                Task {
                    if done {
                        await model.perform { _ = try $0.uncompleteTasks(taskIds: [id]) }
                    } else {
                        await model.perform("Completed “\(task.title)”") { _ = try $0.completeTasks(taskIds: [id]) }
                    }
                }
            } label: {
                Image(systemName: done ? "checkmark.circle.fill" : "circle")
                    .font(.title2)
                    .foregroundStyle(done ? Color.accentColor : .secondary)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(done ? "Completed" : "Complete")

            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    if task.isWorkingOn {
                        Image(systemName: "play.fill").font(.caption).foregroundStyle(Color.accentColor)
                            .accessibilityLabel("Working on")
                    }
                    Text(task.title)
                        .strikethrough(done)
                        .foregroundStyle(done ? .secondary : .primary)
                }
                if hasDetails { details }
            }

            Spacer(minLength: 8)

            if task.isDirty {
                Image(systemName: "icloud.dashed")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .accessibilityLabel("Not yet on the server")
            }
            if let due = task.dueDate, !done { dueLabel(due) }
        }
        .contentShape(Rectangle())
    }

    private var hasDetails: Bool {
        task.stepsTotal > 0 || task.reminderCount > 0 || task.isRecurring
            || task.linkedEmailCount > 0 || !task.tags.isEmpty
    }

    private var details: some View {
        HStack(spacing: 8) {
            if task.stepsTotal > 0 {
                Label("\(task.stepsDone)/\(task.stepsTotal)", systemImage: "checklist")
            }
            if task.reminderCount > 0 { Image(systemName: "bell") }
            if task.isRecurring { Image(systemName: "repeat") }
            if task.linkedEmailCount > 0 { Image(systemName: "envelope") }
            ForEach(task.tags, id: \.id) { tag in
                Text(tag.name).foregroundStyle(Color(hex: tag.colour) ?? .secondary)
            }
        }
        .font(.caption)
        .foregroundStyle(.secondary)
        .labelStyle(.titleAndIcon)
    }

    @ViewBuilder
    private func dueLabel(_ due: String) -> some View {
        if task.isOverdue {
            Text("! \(CivilDate.label(due))").font(.subheadline).foregroundStyle(.red)
        } else if due == CivilDate.today {
            Text("Today").font(.subheadline).foregroundStyle(.red)
        } else {
            Text(CivilDate.label(due)).font(.subheadline).foregroundStyle(.secondary)
        }
    }
}

extension Color {
    /// A `#rrggbb` colour, as the tag registry keeps them.
    init?(hex: String?) {
        guard let hex, hex.hasPrefix("#"), hex.count == 7, let value = UInt32(hex.dropFirst(), radix: 16) else {
            return nil
        }
        self.init(
            red: Double((value >> 16) & 0xFF) / 255,
            green: Double((value >> 8) & 0xFF) / 255,
            blue: Double(value & 0xFF) / 255
        )
    }
}
