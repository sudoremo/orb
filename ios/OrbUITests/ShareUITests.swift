import XCTest

/// Share to Orb from another app: the item waits in the App Group, and the
/// app makes it a task in Today.
@MainActor
final class ShareUITests: XCTestCase {
    func testAPageSharedFromSafariBecomesATaskInToday() {
        let app = XCUIApplication()
        app.launchEnvironment = [
            "ORB_RESET": "1",
            "ORB_SETUP": "imap.invalid:993:ui-test@example.invalid:Tasks",
            "ORB_SETUP_PASSWORD": "unused",
        ]
        app.launch()
        let alert = XCUIApplication(bundleIdentifier: "com.apple.springboard").alerts.firstMatch
        if alert.waitForExistence(timeout: 5) { alert.buttons.element(boundBy: 1).tap() }
        XCTAssertTrue(app.navigationBars["Today"].waitForExistence(timeout: 20))

        // Safari loads the page, so this test needs the network.
        let safari = XCUIApplication(bundleIdentifier: "com.apple.mobilesafari")
        safari.open(URL(string: "https://example.com/orb-share-test")!)
        // In the toolbar on the iPad, behind More in the iPhone's compact
        // Safari.
        // Loaded before the toolbar is touched: a fresh simulator's Safari is slow.
        XCTAssertTrue(safari.staticTexts["Learn more"].waitForExistence(timeout: 30), "the page")
        let direct = safari.buttons["Share"].firstMatch
        let more = safari.buttons["More"]
        XCTAssertTrue(direct.waitForExistence(timeout: 20) || more.exists, "Safari's toolbar")
        if !direct.exists { more.tap() }
        let share = safari.buttons["Share"].firstMatch
        XCTAssertTrue(share.waitForExistence(timeout: 10), "Safari's Share")
        share.tap()
        let orb = safari.descendants(matching: .any).matching(NSPredicate(format: "label == %@", "Orb")).firstMatch
        XCTAssertTrue(orb.waitForExistence(timeout: 10), "Orb in the share sheet")
        orb.tap()
        let add = safari.buttons["Add"]
        XCTAssertTrue(add.waitForExistence(timeout: 10), "the share card")
        add.tap()

        app.activate()
        // Titled by the page, with its link in the notes.
        let task = app.staticTexts["Example Domain"]
        XCTAssertTrue(task.waitForExistence(timeout: 15), "the shared task in Today")
        task.tap()
        let notes = app.textFields.matching(NSPredicate(format: "value CONTAINS %@", "example.com/orb-share-test")).firstMatch
        XCTAssertTrue(notes.waitForExistence(timeout: 10), "the link in the notes")
    }
}
