import CoreSpotlight
import SwiftUI

@main
struct OrbApp: App {
    @UIApplicationDelegateAdaptor(OrbAppDelegate.self) private var delegate
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
                .task { await model.start() }
                .onOpenURL { model.handle($0) }
                .onContinueUserActivity(CSSearchableItemActionType) { activity in
                    if let route = SpotlightIndex.route(activity) { model.open(route) }
                }
        }
        .commands { OrbCommands(model: model) }
        .backgroundTask(.appRefresh(AppModel.refreshTask)) {
            await model.backgroundRefresh()
        }
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .active: Task { await model.becameActive() }
            case .background: model.enteredBackground()
            default: break
            }
        }
    }
}

struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        switch model.state {
        case .starting:
            ProgressView()
        case .setup:
            SetupView()
        case .ready:
            MainTabs()
        case .failed(let message):
            ContentUnavailableView {
                Label("Orb Could Not Start", systemImage: "exclamationmark.triangle")
            } description: {
                Text(message)
            } actions: {
                Button("Set Up Again") { model.reset() }
            }
        }
    }
}

enum MainTab: Hashable {
    case today, tasks, search
}

struct MainTabs: View {
    @Environment(AppModel.self) private var model
    @Environment(\.horizontalSizeClass) private var sizeClass

    /// The iPad's width: the list and the task side by side.
    private var wide: Bool { sizeClass == .regular }

    var body: some View {
        @Bindable var model = model
        TabView(selection: $model.tab) {
            Tab("Today", systemImage: "sun.max", value: .today) {
                if wide {
                    // A notification or Spotlight opens a task through the path.
                    SplitWithTask(path: $model.todayPath) { TaskListView(view: .today) }
                } else {
                    NavigationStack(path: $model.todayPath) { TaskListView(view: .today).taskDestinations() }
                }
            }
            .badge(Int(model.todayOpenCount))
            Tab("Tasks", systemImage: "checklist", value: .tasks) {
                NavigationStack { TasksTab().taskDestinations() }
            }
            Tab(value: .search, role: .search) {
                if wide {
                    SplitWithTask { SearchTab() }
                } else {
                    NavigationStack { SearchTab().taskDestinations() }
                }
            }
        }
        // A sidebar on the iPad, the tab bar on the iPhone.
        .tabViewStyle(.sidebarAdaptable)
        .tabViewBottomAccessory(isEnabled: model.workingOn != nil) {
            WorkingOnBar()
        }
        .overlay(alignment: .bottom) { UndoPill() }
        #if DEBUG
        .sheet(isPresented: $model.showWidgetGallery) { WidgetGallery() }
        #endif
        .onReceive(NotificationCenter.default.publisher(for: .deviceDidShake)) { _ in
            Task { await model.undo() }
        }
    }
}

extension View {
    /// Where a tapped task row leads, in any tab.
    func taskDestinations() -> some View {
        navigationDestination(for: TaskRoute.self) { TaskDetailView(id: $0.id) }
    }
}

extension UIWindow {
    /// Shaking undoes the last change (§10.3).
    open override func motionEnded(_ motion: UIEvent.EventSubtype, with event: UIEvent?) {
        if motion == .motionShake { NotificationCenter.default.post(name: .deviceDidShake, object: nil) }
        super.motionEnded(motion, with: event)
    }
}

extension Notification.Name {
    static let deviceDidShake = Notification.Name("OrbDeviceDidShake")
}

/// What a hardware keyboard reaches, shown in the iPad's shortcut overlay
/// (hold ⌘).
struct OrbCommands: Commands {
    let model: AppModel

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("New Task") { model.handle(OrbLink.quickAdd) }
                .keyboardShortcut("n")
        }
        CommandGroup(replacing: .undoRedo) {
            Button("Undo") { Task { await model.undo() } }
                .keyboardShortcut("z")
        }
        CommandMenu("Go") {
            Button("Today") { model.tab = .today }
                .keyboardShortcut("1")
            Button("Tasks") { model.tab = .tasks }
                .keyboardShortcut("2")
            Button("Search") { model.tab = .search }
                .keyboardShortcut("f")
            Divider()
            Button("Sync Now") { Task { await model.sync() } }
                .keyboardShortcut("r")
        }
    }
}

/// A list beside the task opened from it, for the iPad: a row opens its
/// task in the right-hand column rather than over the list.
struct SplitWithTask<List: View>: View {
    var path: Binding<[TaskRoute]>?
    @ViewBuilder let list: () -> List
    @State private var opened: TaskRoute?

    var body: some View {
        NavigationSplitView {
            list()
                .taskDestinations()
                .environment(\.openTask, OpenTask(opened: opened) { opened = $0 })
                .navigationSplitViewColumnWidth(min: 320, ideal: 380)
        } detail: {
            NavigationStack {
                if let opened {
                    TaskDetailView(id: opened.id).id(opened)
                } else {
                    ContentUnavailableView("No Task Selected", systemImage: "checklist")
                }
            }
        }
        .onChange(of: path?.wrappedValue, initial: true) { _, routes in
            guard let route = routes?.last else { return }
            opened = route
            path?.wrappedValue = []
        }
    }
}

/// Where a row's task opens, when not over the list.
struct OpenTask {
    /// The task open beside the list, whose row is marked.
    let opened: TaskRoute?
    let action: (TaskRoute) -> Void

    init(opened: TaskRoute?, _ action: @escaping (TaskRoute) -> Void) {
        self.opened = opened
        self.action = action
    }
}

extension EnvironmentValues {
    @Entry var openTask: OpenTask?
}

/// A row that opens its task: pushed over the list on the iPhone, beside it
/// on the iPad.
struct TaskLink<Label: View>: View {
    @Environment(\.openTask) private var openTask
    let id: Int64
    @ViewBuilder let label: () -> Label

    var body: some View {
        if let openTask {
            let isOpen = openTask.opened?.id == id
            Button { openTask.action(TaskRoute(id: id)) } label: {
                label().contentShape(Rectangle())
            }
            .tint(.primary)
            .listRowBackground(isOpen ? Color.accentColor.opacity(0.15) : nil)
            .accessibilityAddTraits(isOpen ? .isSelected : [])
        } else {
            NavigationLink(value: TaskRoute(id: id), label: label)
        }
    }
}
