import SwiftUI

/// A month, Monday first, with a dot under each day that already has a task
/// on it, so a date is picked knowing how full it is.
struct MonthCalendar: View {
    @Binding var selection: Date
    /// `YYYY-MM-DD` of the days with tasks.
    let busy: Set<String>

    @State private var month = Calendar.current.startOfMonth(for: .now)

    private var calendar: Calendar {
        var calendar = Calendar.current
        calendar.firstWeekday = 2
        return calendar
    }

    var body: some View {
        VStack(spacing: 10) {
            HStack {
                Text(month.formatted(.dateTime.month(.wide).year())).font(.headline)
                Spacer()
                Button("Previous Month", systemImage: "chevron.left") { step(-1) }
                    .labelStyle(.iconOnly)
                Button("Next Month", systemImage: "chevron.right") { step(1) }
                    .labelStyle(.iconOnly)
                    .padding(.leading, 16)
            }
            .buttonStyle(.borderless)
            // A plain grid: a lazy one inside a Form row re-measures itself
            // until UIKit gives up.
            Grid(horizontalSpacing: 0, verticalSpacing: 6) {
                GridRow {
                    ForEach(Array(weekdays.enumerated()), id: \.offset) { _, day in
                        Text(day).font(.caption2.bold()).foregroundStyle(.secondary).frame(maxWidth: .infinity)
                    }
                }
                ForEach(Array(weeks.enumerated()), id: \.offset) { _, week in
                    GridRow {
                        ForEach(Array(week.enumerated()), id: \.offset) { _, day in
                            if let day { cell(day) } else { Color.clear.frame(maxWidth: .infinity, maxHeight: 41) }
                        }
                    }
                }
            }
        }
        .onAppear { month = calendar.startOfMonth(for: selection) }
        .onChange(of: selection) { _, day in month = calendar.startOfMonth(for: day) }
    }

    private func cell(_ day: Date) -> some View {
        let key = CivilDate.string(day)
        let selected = calendar.isDate(day, inSameDayAs: selection)
        let today = calendar.isDateInToday(day)
        let hasTasks = busy.contains(key)
        return Button { selection = day } label: {
            VStack(spacing: 2) {
                Text(day.formatted(.dateTime.day()))
                    .font(.body.weight(selected || today ? .semibold : .regular))
                    .foregroundStyle(selected ? .white : today ? .orange : .primary)
                    .frame(width: 34, height: 34)
                    .background(selected ? Color.accentColor : .clear, in: Circle())
                Circle()
                    .fill(hasTasks ? Color.secondary : .clear)
                    .frame(width: 5, height: 5)
            }
            .frame(maxWidth: .infinity)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(day.formatted(date: .complete, time: .omitted) + (hasTasks ? ", has tasks" : ""))
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    private var weekdays: [String] {
        let symbols = calendar.veryShortStandaloneWeekdaySymbols
        return Array(symbols[1...] + symbols[..<1])
    }

    /// The month's days, padded in front to start on a Monday.
    private var cells: [Date?] {
        guard let range = calendar.range(of: .day, in: .month, for: month) else { return [] }
        let lead = (calendar.component(.weekday, from: month) - calendar.firstWeekday + 7) % 7
        let days = range.compactMap { calendar.date(byAdding: .day, value: $0 - 1, to: month) }
        return Array(repeating: nil, count: lead) + days
    }

    /// The cells in rows of seven, the last padded out.
    private var weeks: [[Date?]] {
        let all = cells
        return stride(from: 0, to: all.count, by: 7).map { start in
            let week = Array(all[start..<min(start + 7, all.count)])
            return week + Array(repeating: nil, count: 7 - week.count)
        }
    }

    private func step(_ months: Int) {
        if let moved = calendar.date(byAdding: .month, value: months, to: month) { month = moved }
    }
}

extension Calendar {
    func startOfMonth(for date: Date) -> Date {
        self.date(from: dateComponents([.year, .month], from: date)) ?? date
    }
}
