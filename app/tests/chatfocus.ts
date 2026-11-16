import { expect, Page, test } from "@playwright/test";

/**
 * A room opened, or a thread double-clicked, is one to write in: its composer
 * takes the keyboard. A thread clicked once leaves it with the list. Run by `chat.spec.ts` in Chromium and by `webkit.spec.ts` in
 * WebKit, which the app runs on.
 */

const composer = (page: Page) => page.locator("#reader .cc-field");

async function boot(page: Page, query: string): Promise<void> {
  await page.goto(`/harness.html?${query}`);
  await page.waitForFunction(() => (window as unknown as { __mock?: { answered?: string[] } }).__mock?.answered?.includes("sync_all"));
}

/** Typed now, the text has to land in the composer, and nowhere else. */
async function typesInComposer(page: Page): Promise<void> {
  await expect(composer(page)).toBeFocused();
  await page.keyboard.type("jk");
  await expect(composer(page)).toHaveValue("jk");
}

export function chatFocusTests(): void {
  test("a room or a person opened in the sidebar has the composer ready", async ({ page }) => {
    await boot(page, "chat=1");
    const channels = page.locator("#folders .sb-row", { hasText: "Channels" });
    await channels.locator("[data-fold]").click();
    await page.locator("#folders .sb-row", { hasText: "orb-dev" }).click();
    await expect(page.locator("#toolbar .tb-t")).toHaveText("#orb-dev");
    await typesInComposer(page);

    const direct = page.locator("#folders .sb-row", { hasText: "Direct" });
    await direct.locator("[data-fold]").click();
    await page.locator("#folders .sb-row", { hasText: "lukas" }).click();
    await expect(page.locator("#toolbar .tb-t")).toHaveText("lukas");
    await typesInComposer(page);
  });

  test("a thread clicked in Waiting keeps the keys with the list; a is Done", async ({ page }) => {
    await boot(page, "chat=1");
    await page.locator("#folders .sb-row", { hasText: "Waiting" }).click();
    await page.locator("#list .chat-row").first().waitFor();
    await page.locator("#list .chat-row", { hasText: "Customer cannot sign in" }).click();
    await expect(page.locator("#reader .cv-t")).toHaveText("Customer cannot sign in after the update");
    await expect(composer(page)).not.toBeFocused();
    await page.keyboard.press("a");
    await expect(page.locator("#list .chat-row", { hasText: "Customer cannot sign in" })).toHaveCount(0);
  });

  test("a thread double-clicked in Waiting, or answered with i, has the composer ready", async ({ page }) => {
    await boot(page, "chat=1");
    await page.locator("#folders .sb-row", { hasText: "Waiting" }).click();
    await page.locator("#list .chat-row").first().waitFor();
    await page.locator("#list .chat-row", { hasText: "Customer cannot sign in" }).dblclick();
    await expect(page.locator("#reader .cv-t")).toHaveText("Customer cannot sign in after the update");
    await typesInComposer(page);

    await page.keyboard.press("Escape");
    await expect(composer(page)).not.toBeFocused();
    await page.keyboard.press("i");
    await expect(composer(page)).toBeFocused();
  });

  test("a thread clicked in Today's Chat keeps the keys with the list; double-clicked, it has the composer ready", async ({ page }) => {
    await boot(page, "tasks=1&chat=1");
    await page.locator("#list").focus();
    await page.keyboard.press("z");
    await page.keyboard.press("w");
    const release = page.locator("#list .ch-section .ch-row", { hasText: "Release 0.9" });
    await release.click();
    await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
    await expect(composer(page)).not.toBeFocused();
    await release.dblclick();
    await typesInComposer(page);
  });

  test("a thread double-clicked on Today's timeline has the composer ready", async ({ page }) => {
    await boot(page, "tasks=1&chat=1&reading=1&spread=1");
    await page.locator("#list").focus();
    for (const key of ["z", "w", "z", "r", "z", "x"]) await page.keyboard.press(key);
    const release = page.locator("#list.today-mixed .rows .row.chat-row", { hasText: "Release 0.9" });
    await release.click();
    await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
    await expect(composer(page)).not.toBeFocused();
    await release.dblclick();
    await typesInComposer(page);
    await page.evaluate(() => localStorage.clear());
  });
}
