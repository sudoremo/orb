import XCTest

/// The app driven as a person would, on a configuration of its own whose
/// server does not exist: everything here works offline (§3.6).
@MainActor
final class TasksUITests: XCTestCase {
    private var app: XCUIApplication!

    override func setUp() async throws {
        continueAfterFailure = false
        app = XCUIApplication()
        app.launchEnvironment = [
            "ORB_RESET": "1",
            "ORB_SETUP": "imap.invalid:993:ui-test@example.invalid:Tasks",
            "ORB_SETUP_PASSWORD": "unused",
        ]
        app.launch()
        // The notification prompt that follows setup, in whatever language
        // the simulator speaks.
        let alert = XCUIApplication(bundleIdentifier: "com.apple.springboard").alerts.firstMatch
        if alert.waitForExistence(timeout: 5) {
            alert.buttons.element(boundBy: 1).tap()
        }
        XCTAssertTrue(app.navigationBars["Today"].waitForExistence(timeout: 20))
    }

    private func add(_ title: String, shownAs shown: String? = nil) {
        app.navigationBars["Today"].buttons["New Task"].tap()
        let field = app.textFields["New Task"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.typeText(title)
        app.buttons["Add"].tap()
        app.buttons["Close"].tap()
        if let shown = shown ?? Optional(title), !shown.isEmpty {
            XCTAssertTrue(app.staticTexts[shown].waitForExistence(timeout: 5))
        }
    }

    /// A tab: in the tab bar on the iPhone, at the top on the iPad, where
    /// it is not exposed as a tab bar.
    private func tab(_ name: String) -> XCUIElement {
        let inTabBar = app.tabBars.buttons[name]
        return inTabBar.exists ? inTabBar : app.buttons[name].firstMatch
    }

    private func row(_ title: String) -> XCUIElement {
        app.cells.containing(.staticText, identifier: title).firstMatch
    }

    private func text(containing fragment: String) -> XCUIElement {
        app.staticTexts.matching(NSPredicate(format: "label CONTAINS[c] %@", fragment)).firstMatch
    }

    func testATaskAddedFromTodayIsPostponedToToday() {
        add("Water the plants")
        app.staticTexts["Water the plants"].tap()
        XCTAssertTrue(app.buttons["Postpone, Today"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["Not yet on the server"].exists)
    }

    func testStepsAreAddedAndCountedInTheDetail() {
        add("Pack for the trip")
        app.staticTexts["Pack for the trip"].tap()
        let step = app.textFields["Add Step"]
        XCTAssertTrue(step.waitForExistence(timeout: 5))
        step.tap()
        step.typeText("Passport\n")
        XCTAssertTrue(text(containing: "0 of 1").waitForExistence(timeout: 5))
        // The middle of the row, where nothing is drawn, as a thumb would.
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Passport")).firstMatch.tap()
        XCTAssertTrue(text(containing: "1 of 1").waitForExistence(timeout: 5))
    }

    /// Select Steps from the ⋯ menu, pick two, tick both at once, then
    /// delete them together.
    func testSelectedStepsAreTickedAndDeletedTogether() {
        add("Pack for the trip")
        app.staticTexts["Pack for the trip"].tap()
        let step = app.textFields["Add Step"]
        XCTAssertTrue(step.waitForExistence(timeout: 5))
        step.tap()
        step.typeText("Passport\n")
        XCTAssertTrue(text(containing: "0 of 1").waitForExistence(timeout: 5))
        step.typeText("Charger\n")
        XCTAssertTrue(text(containing: "0 of 2").waitForExistence(timeout: 5))
        app.navigationBars.buttons["More"].tap()
        app.buttons["Select Steps"].tap()
        XCTAssertTrue(app.buttons["steps.toggle"].waitForExistence(timeout: 5))
        for title in ["Passport", "Charger"] {
            app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", title)).firstMatch.tap()
        }
        app.buttons["steps.toggle"].tap()
        XCTAssertTrue(text(containing: "2 of 2").waitForExistence(timeout: 5))
        app.buttons["steps.delete"].tap()
        XCTAssertTrue(text(containing: "of 2").waitForNonExistence(timeout: 5))
        XCTAssertFalse(app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Passport")).firstMatch.exists)
    }

    /// A synthesized swipe reveals the actions rather than running the first,
    /// so the test taps it.
    func testSwipingLeftPostponesToTomorrowAndUndoBringsItBack() {
        add("Call the bank")
        row("Call the bank").swipeLeft()
        app.buttons["Tomorrow"].tap()
        XCTAssertTrue(app.staticTexts["Call the bank"].waitForNonExistence(timeout: 5))
        app.buttons["Undo"].tap()
        XCTAssertTrue(app.staticTexts["Call the bank"].waitForExistence(timeout: 5))
    }

    func testCompletingFromTheBoxKeepsTheTaskInTodayAsCompleted() {
        add("Reply to Anna")
        app.buttons["Complete"].firstMatch.tap()
        XCTAssertTrue(app.buttons["Completed"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["Reply to Anna"].exists)
    }

    /// The whole way: typed into the Remind sheet, scheduled with iOS, and
    /// shown as a banner at its minute.
    func testAReminderFiresAsANotificationAtItsMinute() {
        add("Take the cake out")
        app.staticTexts["Take the cake out"].tap()
        let step = app.textFields["Add Step"]
        XCTAssertTrue(step.waitForExistence(timeout: 5))
        step.tap()
        step.typeText("Let it cool\n")
        app.buttons["Remind, None"].tap()

        // At least 90 seconds ahead, so typing and saving never outrun it.
        let moment = Date.now.addingTimeInterval(90)
        let clock = moment.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits))
        let field = app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap()
        field.typeText("today \(clock)")
        let reading = app.buttons.matching(NSPredicate(format: "label CONTAINS %@", clock)).firstMatch
        XCTAssertTrue(reading.waitForExistence(timeout: 5), "a reading of today \(clock)")
        reading.tap()
        XCTAssertTrue(app.staticTexts["Reminders"].waitForExistence(timeout: 5))

        let banner = XCUIApplication(bundleIdentifier: "com.apple.springboard")
            .descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS %@", "Take the cake out")).firstMatch
        XCTAssertTrue(banner.waitForExistence(timeout: 180), "the reminder's banner")

        // Its actions, under a long press: Complete completes the task. The
        // iPad's banner does not offer them to a synthesized press.
        if UIDevice.current.userInterfaceIdiom == .pad { return }
        banner.press(forDuration: 1.5)
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let nextStep = springboard.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS %@", "Let it cool")).firstMatch
        XCTAssertTrue(nextStep.waitForExistence(timeout: 5), "the step in the expanded notification")
        let complete = XCUIApplication(bundleIdentifier: "com.apple.springboard").buttons["Complete"]
        XCTAssertTrue(complete.waitForExistence(timeout: 5), "the Complete action")
        complete.tap()
        XCTAssertTrue(app.buttons["Completed"].waitForExistence(timeout: 10))
    }

    /// The app icon carries Today's open count, as the tab does.
    func testTheAppIconBadgeCountsTodaysOpenTasks() throws {
        try XCTSkipIf(UIDevice.current.userInterfaceIdiom == .pad, "the iPad's Home Screen does not expose the badge")
        add("Buy stamps")
        add("Book the train")
        XCUIDevice.shared.press(.home)
        let icon = XCUIApplication(bundleIdentifier: "com.apple.springboard").icons["Orb"]
        XCTAssertTrue(icon.waitForExistence(timeout: 10))
        let badged = NSPredicate(format: "value CONTAINS %@", "2")
        expectation(for: badged, evaluatedWith: icon)
        waitForExpectations(timeout: 10)
    }

    /// Settings apply at once: without Keep Completed in Today, a task ticked
    /// off leaves the list rather than staying struck through.
    func testWithoutKeepCompletedACompletedTaskLeavesToday() {
        tab("Tasks").tap()
        app.buttons["Settings"].tap()
        let keep = app.switches["Keep Completed in Today"]
        XCTAssertTrue(keep.waitForExistence(timeout: 5))
        keep.switches.firstMatch.tap()
        XCTAssertTrue(app.switches["Keep Completed in Today"].waitForValue("0", timeout: 5))

        tab("Today").tap()
        add("Post the letter")
        app.buttons["Complete"].firstMatch.tap()
        XCTAssertTrue(app.staticTexts["Post the letter"].waitForNonExistence(timeout: 5))
    }
}

extension TasksUITests {
    /// A date phrase and a #tag in the title become chips and leave it: the
    /// task is postponed to tomorrow, so it is not in Today, and carries
    /// the tag, which now has a list of its own.
    func testQuickAddReadsADateAndATagOutOfTheTitle() {
        app.navigationBars["Today"].buttons["New Task"].tap()
        let field = app.textFields["New Task"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.typeText("Call mum tomorrow #family")
        XCTAssertTrue(app.buttons["Tomorrow"].waitForExistence(timeout: 5), "the date chip")
        XCTAssertTrue(app.buttons["#family"].exists, "the tag chip")
        app.buttons["Add"].tap()
        app.buttons["Close"].tap()
        XCTAssertFalse(app.staticTexts["Call mum"].waitForExistence(timeout: 2))

        tab("Tasks").tap()
        let tag = app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "family")).firstMatch
        XCTAssertTrue(tag.waitForExistence(timeout: 5))
        tag.tap()
        XCTAssertTrue(app.staticTexts["Call mum"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["Tomorrow"].exists, "grouped under tomorrow (§26)")
    }

    /// A dropped chip gives its words back to the title.
    func testADroppedDateChipLeavesTheWordsInTheTitle() {
        app.navigationBars["Today"].buttons["New Task"].tap()
        let field = app.textFields["New Task"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.typeText("Read tomorrow")
        let chip = app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "Tomorrow")).firstMatch
        XCTAssertTrue(chip.waitForExistence(timeout: 5))
        chip.tap()
        app.buttons["Add"].tap()
        app.buttons["Close"].tap()
        XCTAssertTrue(app.staticTexts["Read tomorrow"].waitForExistence(timeout: 5))
    }

    /// Pulling down offers a filter that narrows this list only (§4.5).
    func testTheFilterNarrowsTheList() {
        add("Water the ferns")
        add("Pay the rent")
        app.swipeDown()
        let filter = app.searchFields["Filter"]
        XCTAssertTrue(filter.waitForExistence(timeout: 5))
        filter.tap()
        filter.typeText("rent")
        XCTAssertTrue(app.staticTexts["Water the ferns"].waitForNonExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["Pay the rent"].exists)
    }

    /// Select from the ⋯ menu, pick two, and complete both. (The context
    /// menu's Select is left out: under XCTest a context menu holds every
    /// step for a minute waiting for animations to settle.)
    func testSelectedTasksAreCompletedTogether() {
        add("Sort the mail")
        add("File the receipts")
        app.navigationBars["Today"].buttons["More"].tap()
        app.buttons["Select"].tap()
        XCTAssertTrue(app.buttons["selection.complete"].waitForExistence(timeout: 5))
        row("Sort the mail").tap()
        row("File the receipts").tap()
        app.buttons["selection.complete"].tap()
        let completed = app.buttons.matching(NSPredicate(format: "label == %@", "Completed"))
        expectation(for: NSPredicate(format: "count == 2"), evaluatedWith: completed)
        waitForExpectations(timeout: 5)
    }
}

extension TasksUITests {
    /// The app icon's quick action opens quick add.
    func testTheIconQuickActionOpensQuickAdd() {
        XCUIDevice.shared.press(.home)
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let icon = springboard.icons["Orb"].firstMatch
        XCTAssertTrue(icon.waitForExistence(timeout: 10))
        icon.press(forDuration: 1.5)
        let action = springboard.buttons["New Task"]
        XCTAssertTrue(action.waitForExistence(timeout: 5))
        action.tap()
        XCTAssertTrue(app.textFields["New Task"].waitForExistence(timeout: 10))
    }
}


extension TasksUITests {
    /// Remind on the quick-add card gives the new task a reminder at 09:00
    /// tomorrow.
    func testQuickAddSetsAReminder() {
        app.navigationBars["Today"].buttons["New Task"].tap()
        let field = app.textFields["New Task"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.typeText("Feed the cat")
        app.buttons["Remind"].tap()
        app.buttons["Add"].tap()
        app.buttons["Close"].tap()
        app.staticTexts["Feed the cat"].tap()
        let remind = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@ AND label CONTAINS %@", "Remind", "9:00")).firstMatch
        XCTAssertTrue(remind.waitForExistence(timeout: 5), "the reminder at 9:00")
    }

    /// A tag is renamed from the tag sheet by swiping its row.
    func testATagIsRenamedFromTheTagSheet() {
        add("Buy bread")
        app.staticTexts["Buy bread"].tap()
        app.buttons["Tags, None"].tap()
        let search = app.searchFields["Tags"]
        XCTAssertTrue(search.waitForExistence(timeout: 5))
        search.tap()
        search.typeText("errand")
        app.buttons["Create #errand"].tap()
        let row = app.buttons["errand"]
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.swipeLeft()
        app.buttons["Rename"].tap()
        let name = app.alerts["Rename Tag"].textFields.firstMatch
        XCTAssertTrue(name.waitForExistence(timeout: 5))
        name.typeText("s")
        app.alerts["Rename Tag"].buttons["Rename"].tap()
        XCTAssertTrue(app.buttons["errands"].waitForExistence(timeout: 5))
    }
}

extension TasksUITests {
    /// The date sheet's calendar marks the days that already have tasks,
    /// and a day picked there is the one set.
    func testTheDateSheetMarksBusyDaysAndSetsThePickedOne() {
        add("Call the plumber")
        row("Call the plumber").swipeLeft()
        app.buttons["Tomorrow"].tap()
        add("Book the dentist")
        app.staticTexts["Book the dentist"].tap()
        app.buttons["Postpone, Today"].tap()

        let tomorrow = Calendar.current.date(byAdding: .day, value: 1, to: .now)!
        let label = tomorrow.formatted(date: .complete, time: .omitted)
        let day = app.buttons["\(label), has tasks"]
        XCTAssertTrue(day.waitForExistence(timeout: 5), "tomorrow marked as busy")
        day.tap()
        app.buttons["Set"].tap()
        XCTAssertTrue(app.buttons["Postpone, Tomorrow"].waitForExistence(timeout: 5))
    }
}

extension TasksUITests {
    /// On the iPad a task opens beside the list, which stays where it is.
    func testOnTheIPadATaskOpensBesideTheList() throws {
        try XCTSkipIf(UIDevice.current.userInterfaceIdiom != .pad, "the iPhone pushes the task over the list")
        add("Plan the offsite")
        add("Order the chairs")
        row("Plan the offsite").tap()
        XCTAssertTrue(app.buttons["Postpone, Today"].waitForExistence(timeout: 5), "the task beside the list")
        XCTAssertTrue(row("Order the chairs").isHittable, "the list still there")
        XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Plan the offsite")).firstMatch.isSelected, "its row marked")
        row("Order the chairs").tap()
        let title = app.textFields.matching(NSPredicate(format: "value == %@", "Order the chairs")).firstMatch
        XCTAssertTrue(title.waitForExistence(timeout: 5), "the other task opened in its place")
    }
}

extension TasksUITests {
    /// With Completed First, Today lists its completed tasks above the open
    /// ones, as the Mac does (§24).
    func testCompletedFirstPutsTodaysCompletedTasksOnTop() {
        add("Open task")
        add("Done task")
        app.cells.containing(.staticText, identifier: "Done task").firstMatch.buttons["Complete"].tap()
        tab("Tasks").tap()
        app.buttons["Settings"].tap()
        let first = app.switches["Completed First"]
        XCTAssertTrue(first.waitForExistence(timeout: 5))
        first.switches.firstMatch.tap()
        XCTAssertTrue(first.waitForValue("1", timeout: 5))
        tab("Today").tap()
        let done = app.staticTexts["Done task"]
        let open = app.staticTexts["Open task"]
        XCTAssertTrue(done.waitForExistence(timeout: 5))
        let settled = expectation(for: NSPredicate { _, _ in done.frame.minY < open.frame.minY }, evaluatedWith: nil)
        wait(for: [settled], timeout: 5)
    }
}

extension XCUIElement {
    func waitForValue(_ value: String, timeout: TimeInterval) -> Bool {
        let predicate = NSPredicate(format: "value == %@", value)
        let expectation = XCTNSPredicateExpectation(predicate: predicate, object: self)
        return XCTWaiter().wait(for: [expectation], timeout: timeout) == .completed
    }
}

extension TasksUITests {
    /// Grouped by tag, a task stands under each of its tags and the
    /// untagged under No tag (§30); the choice outlasts the view.
    func testGroupingByTagListsATaskUnderEachOfItsTags() {
        add("Pay the rent #home #money", shownAs: "Pay the rent")
        add("Water the ferns")
        app.navigationBars["Today"].buttons["More"].tap()
        app.buttons["Group by Tag"].firstMatch.tap()
        XCTAssertTrue(app.staticTexts["No tag"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["home"].exists)
        XCTAssertTrue(app.staticTexts["money"].exists)
        let rent = app.staticTexts.matching(identifier: "Pay the rent")
        expectation(for: NSPredicate(format: "count == 2"), evaluatedWith: rent)
        waitForExpectations(timeout: 5)

        tab("Tasks").tap()
        tab("Today").tap()
        XCTAssertTrue(app.staticTexts["No tag"].waitForExistence(timeout: 5))
    }
}

extension TasksUITests {
    /// Today carries the number of tasks due by today, and Due First lists
    /// them above the rest under Due (§31); the choice outlasts the view.
    func testDueFirstCountsAndListsTheDueTasksOnTop() {
        add("Water the ferns")
        add("Pay the rent")
        // Due today, from the task's detail.
        app.staticTexts["Water the ferns"].tap()
        let due = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Due")).firstMatch
        XCTAssertTrue(due.waitForExistence(timeout: 5))
        due.tap()
        app.buttons["Set"].tap()
        XCTAssertTrue(app.buttons["Due, Today"].waitForExistence(timeout: 5))
        // Back to the list on the iPhone; on the iPad it stays beside it.
        if !app.navigationBars["Today"].exists { app.navigationBars.buttons.element(boundBy: 0).tap() }

        let toggle = app.descendants(matching: .any)["dueFirst"].firstMatch
        XCTAssertTrue(toggle.waitForExistence(timeout: 5))
        let counted = expectation(for: NSPredicate(format: "value CONTAINS %@", "1 due"), evaluatedWith: toggle)
        wait(for: [counted], timeout: 5)
        let ferns = app.staticTexts["Water the ferns"]
        let rent = app.staticTexts["Pay the rent"]
        XCTAssertTrue(rent.frame.minY < ferns.frame.minY, "the newer task on top while off")

        toggle.tap()
        XCTAssertTrue(app.staticTexts["Due"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["Other"].exists)
        let first = expectation(for: NSPredicate { _, _ in ferns.frame.minY < rent.frame.minY }, evaluatedWith: nil)
        wait(for: [first], timeout: 5)

        tab("Tasks").tap()
        tab("Today").tap()
        XCTAssertTrue(app.staticTexts["Due"].waitForExistence(timeout: 5))
    }
}
