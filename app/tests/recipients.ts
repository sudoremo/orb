import { expect, Locator, Page, test } from "@playwright/test";

/**
 * Moving recipients between To, Cc and Bcc, run by `compose.spec.ts` in
 * Chromium and by `webkit.spec.ts` in WebKit, which the app runs on.
 */

type Field = "to" | "cc" | "bcc";

const names = (page: Page, field: Field) => page.locator(`#${field} > .token:not(.dragging) > span`);

const pill = (page: Page, field: Field, name: string) => page.locator(`#${field} > .token`, { hasText: name });

/** The recipients of every save the window asked for. */
async function saves(page: Page): Promise<{ to: string[]; cc: string[]; bcc: string[] }[]> {
  return page.evaluate(() =>
    (window as unknown as { __mock: { calls: [string, { input: { to: string[]; cc: string[]; bcc: string[] } }][] } }).__mock.calls
      .filter(([cmd]) => cmd === "save_draft")
      .map(([, args]) => ({ to: args.input.to, cc: args.input.cc, bcc: args.input.bcc })),
  );
}

async function open(page: Page, query = "recipients=1"): Promise<void> {
  await page.goto(`/compose-harness.html?${query}`);
  await expect(names(page, "to")).toHaveCount(3);
}

/** Press on a recipient and carry it to a point, without letting go. */
async function lift(page: Page, from: Locator, to: { x: number; y: number }): Promise<void> {
  const box = (await from.boundingBox())!;
  const x = box.x + box.width / 2;
  const y = box.y + box.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + 8, y + 2, { steps: 2 });
  await page.mouse.move(to.x, to.y, { steps: 6 });
}

async function drag(page: Page, from: Locator, to: { x: number; y: number }): Promise<void> {
  await lift(page, from, to);
  await page.mouse.up();
}

/** A point in a pill: by its left edge, or past its right one. */
async function beside(target: Locator, side: "before" | "after"): Promise<{ x: number; y: number }> {
  const box = (await target.boundingBox())!;
  return { x: side === "before" ? box.x + 4 : box.x + box.width + 16, y: box.y + box.height / 2 };
}

export function recipientDragTests(): void {
  test("a recipient dragged from To to Cc goes there, selected, in one save", async ({ page }) => {
    await open(page);
    const before = (await saves(page)).length;
    await lift(page, pill(page, "to", "Lukas Meier"), await beside(pill(page, "cc", "Tom Berger"), "after"));
    // Aimed: a copy under the pointer, the bar where it would land, its place faint.
    await expect(page.locator("body > .token-ghost")).toHaveCount(1);
    await expect(page.locator("#cc > .token-caret")).toHaveCount(1);
    await expect(page.locator("#to > .token.dragging")).toHaveCount(1);
    await page.mouse.up();

    await expect(names(page, "to")).toHaveText(["Sara Brunner", "Mia Keller"]);
    await expect(names(page, "cc")).toHaveText(["Tom Berger", "Lukas Meier"]);
    await expect(pill(page, "cc", "Lukas Meier")).toHaveClass(/selected/);
    await expect(page.locator("#cc .token-input")).toBeFocused();
    await expect(page.locator(".token-ghost, .token-caret, .token.dragging")).toHaveCount(0);
    await expect.poll(async () => (await saves(page)).slice(before)).toEqual([
      {
        to: ["Sara Brunner <sara@example.com>", "Mia Keller <mia@example.com>"],
        cc: ["Tom Berger <tom@example.com>", "Lukas Meier <lukas@example.com>"],
        bcc: [],
      },
    ]);
  });

  test("a recipient dropped on the front of another goes before it, in its own field too", async ({ page }) => {
    await open(page);
    await drag(page, pill(page, "to", "Mia Keller"), await beside(pill(page, "to", "Sara Brunner"), "before"));
    await expect(names(page, "to")).toHaveText(["Mia Keller", "Sara Brunner", "Lukas Meier"]);
    await drag(page, pill(page, "to", "Lukas Meier"), await beside(pill(page, "cc", "Tom Berger"), "before"));
    await expect(names(page, "cc")).toHaveText(["Lukas Meier", "Tom Berger"]);
    await expect.poll(async () => (await saves(page)).at(-1)?.to).toEqual([
      "Mia Keller <mia@example.com>",
      "Sara Brunner <sara@example.com>",
    ]);
  });

  test("Esc, or letting go outside the fields, puts the recipient back and saves nothing", async ({ page }) => {
    await open(page);
    const before = (await saves(page)).length;
    await lift(page, pill(page, "to", "Lukas Meier"), await beside(pill(page, "cc", "Tom Berger"), "after"));
    await page.keyboard.press("Escape");
    await expect(page.locator(".token-ghost, .token-caret, .token.dragging")).toHaveCount(0);
    await page.mouse.up();

    const body = (await page.locator("#plain").boundingBox())!;
    await drag(page, pill(page, "to", "Lukas Meier"), { x: body.x + 40, y: body.y + 40 });
    await expect(page.locator(".token-ghost, .token-caret, .token.dragging")).toHaveCount(0);
    await expect(names(page, "to")).toHaveText(["Sara Brunner", "Lukas Meier", "Mia Keller"]);
    await expect(names(page, "cc")).toHaveText(["Tom Berger"]);
    await page.waitForTimeout(150);
    expect((await saves(page)).length).toBe(before);
  });

  test("Bcc shows while a recipient is dragged, and stays only when it is dropped there", async ({ page }) => {
    await open(page);
    await expect(page.locator("#bcc-row")).toBeHidden();
    const home = await beside(pill(page, "to", "Mia Keller"), "after");
    await lift(page, pill(page, "to", "Lukas Meier"), home);
    await expect(page.locator("#bcc-row")).toBeVisible();
    await page.mouse.up();
    await expect(page.locator("#bcc-row")).toBeHidden();
    await expect(page.locator("#show-bcc")).toBeVisible();
    await expect(names(page, "to")).toHaveText(["Sara Brunner", "Mia Keller", "Lukas Meier"]);

    await lift(page, pill(page, "to", "Lukas Meier"), home);
    const bcc = (await page.locator("#bcc").boundingBox())!;
    await page.mouse.move(bcc.x + 30, bcc.y + bcc.height / 2, { steps: 4 });
    await page.mouse.up();
    await expect(page.locator("#bcc-row")).toBeVisible();
    await expect(page.locator("#show-bcc")).toBeHidden();
    await expect(names(page, "bcc")).toHaveText(["Lukas Meier"]);
  });

  test("a recipient dragged to a field that has its address already leaves its own, and is not doubled", async ({ page }) => {
    await open(page, "recipients=dup");
    await drag(page, pill(page, "to", "Sara Brunner"), await beside(pill(page, "cc", "Tom Berger"), "before"));
    await expect(names(page, "to")).toHaveText(["Lukas Meier", "Mia Keller"]);
    await expect(names(page, "cc")).toHaveText(["Tom Berger", "sara@example.com"]);
    await expect(pill(page, "cc", "sara@example.com")).toHaveClass(/selected/);
  });

  test("⌥↓ and ⌥↑ take the selected recipient to the field below or above", async ({ page }) => {
    await open(page);
    await pill(page, "to", "Lukas Meier").click();
    await page.keyboard.press("Alt+ArrowDown");
    await expect(names(page, "cc")).toHaveText(["Tom Berger", "Lukas Meier"]);
    await expect(pill(page, "cc", "Lukas Meier")).toHaveClass(/selected/);
    // Into Bcc, which shows for it.
    await page.keyboard.press("Alt+ArrowDown");
    await expect(page.locator("#bcc-row")).toBeVisible();
    await expect(names(page, "bcc")).toHaveText(["Lukas Meier"]);
    await page.keyboard.press("Alt+ArrowDown");
    await expect(names(page, "bcc")).toHaveText(["Lukas Meier"]);
    await page.keyboard.press("Alt+ArrowUp");
    await page.keyboard.press("Alt+ArrowUp");
    await expect(names(page, "to")).toHaveText(["Sara Brunner", "Mia Keller", "Lukas Meier"]);
    await expect(names(page, "cc")).toHaveText(["Tom Berger"]);
    await expect(page.locator("#to .token-input")).toBeFocused();
    // Backspace selects the last one, as before, and ⌥↓ takes it down.
    await page.keyboard.press("Alt+ArrowUp");
    await expect(names(page, "to")).toHaveCount(3);
    await page.locator("#cc .token-input").focus();
    await page.keyboard.press("Backspace");
    await page.keyboard.press("Alt+ArrowUp");
    await expect(names(page, "to")).toHaveText(["Sara Brunner", "Mia Keller", "Lukas Meier", "Tom Berger"]);
    await expect(names(page, "cc")).toHaveCount(0);
    await expect.poll(async () => (await saves(page)).at(-1)?.cc).toEqual([]);
  });

  test("a click on a recipient still selects it, and a double-click still edits it", async ({ page }) => {
    await open(page);
    await pill(page, "to", "Mia Keller").click();
    await expect(pill(page, "to", "Mia Keller")).toHaveClass(/selected/);
    await expect(page.locator(".token-ghost")).toHaveCount(0);
    await pill(page, "to", "Mia Keller").dblclick();
    await expect(page.locator("#to .token-input")).toHaveValue("Mia Keller <mia@example.com>");
    await expect(names(page, "to")).toHaveText(["Sara Brunner", "Lukas Meier"]);
  });
}
