import { expect, Page, test } from "@playwright/test";

/** Mail templates in the compose window, against compose-harness.html's plain editor. */

type Calls = { calls: [string, Record<string, unknown>][] };
const calls = (page: Page, cmd: string) =>
  page.evaluate(
    (cmd) => (window as unknown as { __mock: Calls }).__mock.calls.filter(([c]) => c === cmd).map(([, a]) => a),
    cmd,
  );

/** Open the harness with the caret on the blank line after "Hi Sara,". */
async function openOnBlankLine(page: Page, query = ""): Promise<void> {
  await page.goto(`/compose-harness.html${query}`);
  await expect(page.locator("#plain")).toBeVisible();
  await page.locator("#plain").evaluate((el: HTMLTextAreaElement) => {
    el.focus();
    el.setSelectionRange(9, 9);
  });
}

test("⌘T lists the account's templates beside a preview, and Enter inserts one at the cursor", async ({ page }) => {
  await openOnBlankLine(page);
  await expect(page.locator("#template")).toHaveText("Template");
  await page.keyboard.press("Meta+t");
  await expect(page.locator(".tpk")).toBeVisible();
  await expect(page.locator(".tpk-h")).toHaveText(["Personal", "Every account", "Could not be read"]);
  await expect(page.locator(".tpk-i.on .tpk-name")).toHaveText("Einladung zum internen Workshop");
  // What Orb knows is filled in the preview already, the rest stays in braces.
  await expect(page.locator(".tpk-body .ph.ok")).toHaveText("Sara");
  await expect(page.locator(".tpk-body .ph:not(.ok)").first()).toHaveText("{Datum}");
  await expect(page.locator(".tpk-kv")).toContainText("stays Re: Q4 roadmap");
  await expect(page.locator(".tpk-i.broken")).toContainText("broken.yml");

  await page.keyboard.type("thank");
  await expect(page.locator(".tpk-i:not(.broken) .tpk-name")).toHaveText(["Thanks for the introduction"]);
  await page.keyboard.press("Backspace");
  await page.locator(".tpk-input").fill("");
  await page.keyboard.press("Enter");
  await expect(page.locator(".tpk")).toHaveCount(0);

  await expect(page.locator("#plain")).toHaveValue(
    "Hi Sara,\nLieber Sara\n\nFür den Workshop vom {Datum} bitte vorbereiten.\n\n[1] {Webseite}\nthanks.\n",
  );
  await expect(page.locator("#template")).toHaveText("Einladung zum internen Workshop");
  // A reply keeps its subject.
  await expect(page.locator("#subject")).toHaveValue("Re: Q4 roadmap");
});

test("the Fill in line fills every place a placeholder stands, live, and hands over to the text", async ({ page }) => {
  await openOnBlankLine(page);
  await page.keyboard.press("Meta+t");
  await expect(page.locator(".tpk")).toBeVisible();
  await page.keyboard.press("Enter");

  const row = page.locator("#fill-row");
  await expect(row).toBeVisible();
  await expect(row.locator(".pf .lb > span:first-of-type")).toHaveText(["Datum", "Vorname", "Webseite"]);
  await expect(row.locator(".pf.auto input")).toHaveValue("Sara");
  await expect(row.locator(".left")).toHaveText("2 of 3 left");
  // The keyboard starts in the first field still open.
  await expect(row.locator('input[data-name="Datum"]')).toBeFocused();

  // Typed in the body before the placeholder: the fill still finds its place.
  await page.locator("#plain").evaluate((el: HTMLTextAreaElement) => {
    el.setRangeText("Hallo! ", 9, 9);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await row.locator('input[data-name="Datum"]').fill("5. November");
  await expect(page.locator("#plain")).toHaveValue(/Workshop vom 5\. November bitte/);
  await row.locator('input[data-name="Datum"]').fill("");
  await expect(page.locator("#plain")).toHaveValue(/Workshop vom \{Datum\} bitte/);
  await row.locator('input[data-name="Datum"]').fill("Donnerstag");

  await page.keyboard.press("Tab");
  await page.keyboard.press("Tab");
  await expect(row.locator('input[data-name="Webseite"]')).toBeFocused();
  // Enter goes to the text, at the placeholder still open.
  await page.keyboard.press("Enter");
  await expect(page.locator("#plain")).toBeFocused();
  const selected = await page
    .locator("#plain")
    .evaluate((el: HTMLTextAreaElement) => el.value.slice(el.selectionStart, el.selectionEnd));
  expect(selected).toBe("{Webseite}");

  await expect(page.locator("#warnings")).toContainText("Not filled in: {Webseite} (line");
  await row.locator('input[data-name="Webseite"]').fill("https://example.com/repo");
  await expect(row.locator(".keys")).toContainText("All filled in");
  await page.keyboard.press("Enter");
  // Everything is filled: the line has done its work.
  await expect(row).toBeHidden();
  await expect(page.locator("#plain")).toHaveValue(/\[1\] https:\/\/example\.com\/repo/);
});

test("in a new message the template's subject fills the empty Subject and follows the fields", async ({ page }) => {
  await page.goto("/compose-harness.html?new=1");
  await expect(page.locator("#plain")).toBeVisible();
  await page.locator("#plain").focus();
  await page.keyboard.press("Meta+t");
  await expect(page.locator(".tpk-kv")).toContainText("Workshop am {Datum}");
  await page.keyboard.press("Enter");
  await expect(page.locator("#subject")).toHaveValue("Workshop am {Datum}");
  // Nobody to greet yet: Vorname waits for To.
  await expect(page.locator("#fill-row .left")).toHaveText("3 of 3 left");
  await page.locator('#fill-row input[data-name="Datum"]').fill("Donnerstag");
  await expect(page.locator("#subject")).toHaveValue("Workshop am Donnerstag");
  await expect(page.locator("#compose-title")).toHaveText("Workshop am Donnerstag");
});

test("Send stops at a placeholder still in braces, Enter goes back to it, and ⌥⌘Enter sends anyway", async ({ page }) => {
  await openOnBlankLine(page);
  await page.keyboard.press("Meta+t");
  await expect(page.locator(".tpk")).toBeVisible();
  await page.keyboard.press("Enter");
  await page.locator('#fill-row input[data-name="Datum"]').fill("Donnerstag");

  await page.locator("#send").click();
  const sheet = page.locator(".sheet");
  await expect(sheet.locator("h3")).toHaveText("1 placeholder is not filled in");
  await expect(sheet).toContainText("Sara Brunner would get it as it stands");
  await expect(sheet.locator(".it .ph")).toHaveText("{Webseite}");
  await expect(sheet.locator(".it .ln")).toHaveText("line 6");
  await page.keyboard.press("Enter");
  await expect(sheet).toHaveCount(0);
  await expect(page.locator("#plain")).toBeFocused();
  expect(await calls(page, "send_mail")).toHaveLength(0);

  await page.locator("#send").click();
  await expect(sheet).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(sheet).toHaveCount(0);
  expect(await calls(page, "send_mail")).toHaveLength(0);

  await page.locator("#send").click();
  await expect(sheet).toBeVisible();
  await page.keyboard.press("Alt+Meta+Enter");
  await expect.poll(async () => (await calls(page, "send_mail")).length).toBe(1);
});

test("an empty templates folder says what a template is", async ({ page }) => {
  await openOnBlankLine(page, "?templates=0");
  await page.locator("#template").click();
  await expect(page.locator(".tpk-empty")).toHaveText("No templates yet.");
  await expect(page.locator(".tpk-about")).toContainText("/tmp/templates");
  await page.keyboard.press("Escape");
  await expect(page.locator(".tpk")).toHaveCount(0);
  await expect(page.locator("#plain")).toBeFocused();
});
