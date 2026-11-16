import Foundation

/// The core's dates are local civil dates, `YYYY-MM-DD`, never instants.
enum CivilDate {
    private static let format: DateFormatter = {
        let formatter = DateFormatter()
        formatter.calendar = Calendar(identifier: .gregorian)
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyy-MM-dd"
        return formatter
    }()

    static func string(_ date: Date) -> String { format.string(from: date) }
    static func date(_ string: String) -> Date? { format.date(from: string) }

    static var today: String { string(.now) }

    /// "Today", "Tomorrow", or "24 Oct" — with the weekday within a week.
    static func label(_ string: String) -> String {
        guard let date = date(string) else { return string }
        let calendar = Calendar.current
        if calendar.isDateInToday(date) { return "Today" }
        if calendar.isDateInTomorrow(date) { return "Tomorrow" }
        if let days = calendar.dateComponents([.day], from: calendar.startOfDay(for: .now), to: date).day, (2..<7).contains(days) {
            return date.formatted(.dateTime.weekday(.abbreviated))
        }
        return date.formatted(.dateTime.day().month(.abbreviated))
    }
}

/// A local date and time as the core's reminders take it: RFC 3339 with the
/// offset of that moment, `2026-10-30T09:00:00+01:00`.
enum Moment {
    static func rfc3339(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        formatter.timeZone = .current
        return formatter.string(from: date)
    }

    /// Tomorrow at 09:00, where a reminder starts unless told otherwise (§5.3).
    static var tomorrowNine: Date {
        let tomorrow = Calendar.current.date(byAdding: .day, value: 1, to: .now)!
        return Calendar.current.date(bySettingHour: 9, minute: 0, second: 0, of: tomorrow)!
    }
}

/// The four postpone shortcuts, the same in the context menu and the date
/// sheet as on the Mac.
enum PostponeShortcut: CaseIterable, Identifiable {
    case today, tomorrow, weekend, nextWeek

    var id: Self { self }

    var title: String {
        switch self {
        case .today: "Today"
        case .tomorrow: "Tomorrow"
        case .weekend: "Weekend"
        case .nextWeek: "Next Week"
        }
    }

    var symbol: String {
        switch self {
        case .today: "sun.max"
        case .tomorrow: "sunrise"
        case .weekend: "sofa"
        case .nextWeek: "calendar.badge.clock"
        }
    }

    var date: String {
        let calendar = Calendar.current
        let today = calendar.startOfDay(for: .now)
        let day: Date = switch self {
        case .today: today
        case .tomorrow: calendar.date(byAdding: .day, value: 1, to: today)!
        case .weekend: next(weekday: 7, after: today)
        case .nextWeek: next(weekday: 2, after: today)
        }
        return CivilDate.string(day)
    }

    private func next(weekday: Int, after day: Date) -> Date {
        Calendar.current.nextDate(after: day, matching: DateComponents(weekday: weekday), matchingPolicy: .nextTime)!
    }
}
