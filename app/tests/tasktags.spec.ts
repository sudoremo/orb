import { expect, Page, test } from "@playwright/test";

async function open(page: Page, query: string): Promise<void> {
  await page.goto(`/harness.html?${query}`);
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list [data-task]").first().waitFor();
  await page.locator("#list").focus();
}

/** Today opens on its mail; step into its tasks, as `<tab>` does. */
async function intoTasks(page: Page): Promise<void> {
  await page.locator("#list").focus();
  // A click in the task section has stepped in already.
  if (!(await page.locator("#list.tasks-active").count())) await page.keyboard.press("Tab");
  await page.locator("#list.tasks-active").waitFor();
}

function callsOf(page: Page, name: string): Promise<Record<string, unknown>[]> {
  return page.evaluate(
    (name) =>
      (window as any).__mock.calls.filter(([cmd]: [string]) => cmd === name).map(([, args]: [string, unknown]) => args),
    name,
  );
}

const row = (page: Page, title: string) => page.locator("#list .tk-row", { hasText: title }).first();
const headings = (page: Page) => page.locator("#list .tk-section .tk-day .lbl");
const titles = (page: Page) => page.locator("#list .tk-section .tk-row .tk-title");

test("Today in columns shows the tags of its tasks", async ({ page }) => {
  await open(page, "tasks=1");
  await page.keyboard.press("z");
  await page.keyboard.press("l");
  await page.locator("#list.today-columns").waitFor();
  await expect(row(page, "Buy groceries").locator(".tk-tagtxt")).toBeVisible();
  await expect(row(page, "Buy groceries").locator(".tk-tagtxt")).toHaveText("#work");
});

test("a tag's colour reaches its chip only when it is a colour", async ({ page }) => {
  // The registry is written by another machine; a value that is not a colour
  // must not become a second declaration in the chip's style.
  await open(page, "tasks=1&tagcolour=%23ff8800");
  const chip = row(page, "Buy groceries").locator(".tk-tag, .tk-tagtxt").first();
  await expect(chip).toHaveAttribute("style", "--tk-tag:#ff8800");

  await open(page, `tasks=1&tagcolour=${encodeURIComponent("red;background:url(https://tracker.example/p)")}`);
  const spoofed = row(page, "Buy groceries").locator(".tk-tag, .tk-tagtxt").first();
  await expect(spoofed).toBeVisible();
  expect(await spoofed.getAttribute("style")).toBeNull();
});

test("a deadline that is today is red, as one that has passed is; only the passed one is marked", async ({ page }) => {
  // The harness's tasks are dated against this day.
  await page.clock.setFixedTime(new Date("2026-08-07T10:00:00"));
  await open(page, "tasks=1");
  const today = row(page, "Buy groceries").locator(".tk-date");
  const past = row(page, "Renew the domain").locator(".tk-date");
  await expect(today).toHaveClass(/due-today/);
  await expect(past).toHaveClass(/overdue/);
  // Read again until both are drawn: a redraw can leave one detached a moment.
  const colour = (l: typeof today) => l.evaluate((e) => getComputedStyle(e).color);
  await expect.poll(async () => {
    const [a, b] = [await colour(today), await colour(past)];
    return a !== "" && a === b;
  }).toBe(true);
  expect(await today.evaluate((e) => getComputedStyle(e, "::before").content)).toBe("none");

  // The detail pane says so too.
  await intoTasks(page);
  await expect(page.locator(".td .p.late")).toContainText("Due");
});

test("the switch in the Tasks heading groups the tasks under their tags, a task under each of its own", async ({ page }) => {
  await open(page, "tasks=1&moretags=1");
  const toggle = page.locator("#list .tk-section-head [data-group-by-tag]");
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  await expect(headings(page)).toHaveCount(0);

  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  // In the registry's order; a tag no task here carries has no heading, and
  // the tasks without a tag come last.
  await expect(headings(page)).toHaveText(["work", "home", "GSI", "No tag"]);
  await expect(titles(page)).toHaveText(["Buy groceries", "Call the plumber", "Call the plumber", "Renew the domain"]);
  // Counted once, though listed twice.
  await expect(page.locator("#list .tk-section-head .n").first()).toHaveText("3");

  // Kept across a restart.
  await page.reload();
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await expect(headings(page)).toHaveText(["work", "home", "GSI", "No tag"]);

  // `zg` takes the groups away again.
  await intoTasks(page);
  await page.keyboard.press("z");
  await page.keyboard.press("g");
  await expect(headings(page)).toHaveCount(0);
  await expect(titles(page)).toHaveText(["Buy groceries", "Call the plumber", "Renew the domain"]);
});

test("a task selected under two of its tags is acted on once", async ({ page }) => {
  await open(page, "tasks=1&moretags=1");
  await page.locator("#list [data-group-by-tag]").click();
  await expect(headings(page)).toHaveCount(4);
  await intoTasks(page);
  await page.keyboard.press("j");
  await page.keyboard.press(" ");
  await page.keyboard.press(" ");
  await expect(page.locator("#list .tk-row.selected")).toHaveCount(2);
  await page.keyboard.press("x");
  await expect.poll(() => callsOf(page, "complete_tasks")).toEqual([{ ids: [2] }]);
});

test("a task moved by the keyboard stays in its tag's group", async ({ page }) => {
  await open(page, "tasks=1&moretags=1");
  await page.locator("#list [data-group-by-tag]").click();
  await expect(headings(page)).toHaveCount(4);
  await intoTasks(page);
  // Buy groceries is alone under work: there is nowhere for it to go.
  await page.keyboard.press("Meta+ArrowDown");
  await page.waitForTimeout(100);
  expect(await callsOf(page, "move_task")).toEqual([]);
});
