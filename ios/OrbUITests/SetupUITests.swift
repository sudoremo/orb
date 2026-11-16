import XCTest

/// The first launch, through the setup screen rather than the launch
/// environment.
@MainActor
final class SetupUITests: XCTestCase {
    func testAnUnreachableServerIsSaidPlainlyAndNothingIsSaved() {
        let app = XCUIApplication()
        app.launchEnvironment = ["ORB_RESET": "1"]
        app.launch()
        XCTAssertTrue(app.navigationBars["Set Up Orb"].waitForExistence(timeout: 20))

        app.textFields["imap.example.com"].tap()
        app.textFields["imap.example.com"].typeText("imap.invalid")
        app.textFields["Username"].tap()
        app.textFields["Username"].typeText("me@example.invalid")
        app.secureTextFields["Password"].tap()
        app.secureTextFields["Password"].typeText("secret")
        app.buttons["Continue"].tap()

        let alert = app.alerts["Could Not Set Up Orb"]
        XCTAssertTrue(alert.waitForExistence(timeout: 30))
        XCTAssertTrue(alert.staticTexts["imap.invalid could not be reached on port 993."].exists)
        alert.buttons["OK"].tap()
        XCTAssertTrue(app.navigationBars["Set Up Orb"].exists, "still setting up")
    }

    /// The setup screen offers the code from the Mac first; the simulator,
    /// without a camera, offers a photo of it.
    func testTheSetupScreenOffersTheCodeFromTheMac() {
        let app = XCUIApplication()
        app.launchEnvironment = ["ORB_RESET": "1"]
        app.launch()
        XCTAssertTrue(app.navigationBars["Set Up Orb"].waitForExistence(timeout: 20))
        XCTAssertTrue(app.buttons["Choose a Photo of the Code"].exists)
        XCTAssertTrue(app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "Set Up iPhone")).firstMatch.exists)
    }
}
