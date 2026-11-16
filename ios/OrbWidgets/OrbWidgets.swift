import SwiftUI
import WidgetKit

@main
struct OrbWidgets: WidgetBundle {
    var body: some Widget {
        TodayWidget()
        WorkingOnWidget()
        TodayAccessoryWidget()
        DoneRingWidget()
    }
}

struct SnapshotEntry: TimelineEntry {
    let date: Date
    let snapshot: WidgetSnapshot
}

/// Reads the app's snapshot; the app reloads the timelines whenever it
/// writes one, and the day turning over is the one change it cannot see.
struct SnapshotProvider: TimelineProvider {
    func placeholder(in context: Context) -> SnapshotEntry {
        SnapshotEntry(date: .now, snapshot: .empty)
    }

    func getSnapshot(in context: Context, completion: @escaping (SnapshotEntry) -> Void) {
        completion(SnapshotEntry(date: .now, snapshot: .read()))
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<SnapshotEntry>) -> Void) {
        let midnight = Calendar.current.startOfDay(for: .now.addingTimeInterval(86_400))
        completion(Timeline(entries: [SnapshotEntry(date: .now, snapshot: .read())], policy: .after(midnight)))
    }
}

struct TodayWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: "today", provider: SnapshotProvider()) { entry in
            TodayRowsView(snapshot: entry.snapshot)
                .containerBackground(.fill.tertiary, for: .widget)
        }
        .configurationDisplayName("Today")
        .description("Today's tasks, ticked off right here.")
        .supportedFamilies([.systemMedium, .systemLarge])
    }
}

private struct TodayRowsView: View {
    @Environment(\.widgetFamily) private var family
    let snapshot: WidgetSnapshot

    var body: some View {
        TodayWidgetView(snapshot: snapshot, rows: family == .systemLarge ? 9 : 3)
    }
}

struct WorkingOnWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: "working-on", provider: SnapshotProvider()) { entry in
            WorkingOnWidgetView(snapshot: entry.snapshot)
                .containerBackground(.fill.tertiary, for: .widget)
        }
        .configurationDisplayName("Working On")
        .description("The task you are working on and its next step.")
        .supportedFamilies([.systemSmall])
    }
}

struct TodayAccessoryWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: "today-accessory", provider: SnapshotProvider()) { entry in
            TodayAccessoryView(snapshot: entry.snapshot)
                .containerBackground(.clear, for: .widget)
        }
        .configurationDisplayName("Today")
        .description("Today's count, the next timed task and the overdue one.")
        .supportedFamilies([.accessoryRectangular])
    }
}

struct DoneRingWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: "done-ring", provider: SnapshotProvider()) { entry in
            DoneRingView(snapshot: entry.snapshot)
                .containerBackground(.clear, for: .widget)
        }
        .configurationDisplayName("Done Today")
        .description("A ring of what is done today.")
        .supportedFamilies([.accessoryCircular])
    }
}
