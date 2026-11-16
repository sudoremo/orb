import { expect, test } from "@playwright/test";

/**
 * A step being written is kept when the task pane is drawn again. Run by
 * `harness.spec.ts` in Chromium and by `webkit.spec.ts` in WebKit, which the
 * app runs on and which blurs a field taken out of the page differently.
 */

type Shell = { __emit: (event: string, payload: unknown) => void; __mock: { answered: string[] } };

export function stepRedrawTests(): void {
  test("a step being written survives the pane being drawn again, as a sync landing does", async ({ page }) => {
    await page.goto("/harness.html?tasks=1");
    await page.locator("#list [data-task]").first().waitFor();
    await page.waitForFunction(() => (window as unknown as Shell).__mock?.answered?.includes("sync_all"));
    await page.locator('#list [data-task="0"]').click();
    await expect(page.locator("#reader .step")).toHaveCount(3);
    const redraw = async () => {
      const before = await page.evaluate(() => (window as unknown as Shell).__mock.answered.filter((c) => c === "get_task").length);
      await page.evaluate(() => (window as unknown as Shell).__emit("tasks-changed", null));
      await page.waitForFunction(
        (before) => (window as unknown as Shell).__mock.answered.filter((c) => c === "get_task").length > before,
        before,
      );
    };
    const field = page.locator("#tk-step-input");

    // A new step: the field stays open, with what was typed and the caret after it.
    await page.locator("#list").focus();
    await page.keyboard.press("+");
    await expect(field).toBeFocused();
    await page.keyboard.type("Half");
    await redraw();
    await expect(field).toBeFocused();
    await expect(field).toHaveValue("Half");
    await page.keyboard.type(" done");
    await page.keyboard.press("Enter");
    await expect(page.locator("#reader .step:not(.adding)").last().locator(".st")).toHaveText("Half done");
    await expect(field).toHaveValue("");
    await page.keyboard.press("Escape");
    await expect(field).toHaveCount(0);

    // A step being renamed, the same.
    await page.locator("#reader .step").first().dblclick();
    await expect(field).toBeFocused();
    await page.keyboard.press("End");
    await page.keyboard.type(" first");
    const typed = await field.inputValue();
    await redraw();
    await expect(field).toBeFocused();
    await expect(field).toHaveValue(typed);
    await page.keyboard.press("Escape");
    await expect(field).toHaveCount(0);
  });
}
