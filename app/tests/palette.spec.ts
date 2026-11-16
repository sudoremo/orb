import { expect, Page, test } from "@playwright/test";

async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
}

function callsOf(page: Page, name: string): Promise<Record<string, unknown>[]> {
  return page.evaluate(
    (name) =>
      (window as unknown as { __mock: { calls: [string, Record<string, unknown>][] } }).__mock.calls
        .filter(([cmd]) => cmd === name)
        .map(([, args]) => args),
    name,
  );
}

async function open(page: Page, query = "tasks=1&chat=1"): Promise<void> {
  await page.goto(`/harness.html?${query}`);
  await settled(page);
  await page.locator("#list").focus();
}

const palette = (page: Page) => page.locator(".dp.picker.palette");
const row = (page: Page, text: string | RegExp) => palette(page).locator("li[data-index]", { hasText: text });

test("every command has a title or is left out on purpose, and the titles are told apart", async ({ page }) => {
  await open(page);
  const problems = await page.evaluate(async () => {
    const { COMMANDS } = await import("/src/commands.ts");
    const { COMMAND_DOCS, COMMAND_VARIANTS } = await import("/src/commanddocs.ts");
    const titles = [
      ...Object.values(COMMAND_DOCS).map((d) => d.title),
      ...Object.values(COMMAND_VARIANTS).map((v) => v.title),
    ].filter((t): t is string => !!t);
    return {
      duplicates: titles.filter((t, i) => titles.indexOf(t) !== i),
      variantsOfNothing: Object.keys(COMMAND_VARIANTS).filter((spec) => !COMMANDS[spec.split(" ")[0]!]),
      variantsWithoutArgs: Object.keys(COMMAND_VARIANTS).filter((spec) => !COMMAND_DOCS[spec.split(" ")[0]!]?.args),
    };
  });
  expect(problems).toEqual({ duplicates: [], variantsOfNothing: [], variantsWithoutArgs: [] });
});

test("⇧⌘K finds a command by its title, shows its key, runs it and lists it as recently used", async ({ page }) => {
  await open(page);
  await page.keyboard.press("Meta+Shift+K");
  await expect(palette(page).locator(".dp-title")).toHaveText("Commands");
  await expect(palette(page).locator(".dp-input")).toHaveValue(">");
  await page.keyboard.type("stage del");
  const first = palette(page).locator("li[data-index]").first();
  await expect(first.locator(".dp-primary")).toHaveText("Stage for Deletion");
  await expect(first.locator(".dp-primary b").first()).toHaveText("S");
  await expect(first.locator("kbd")).toHaveText("d");
  await expect(first.locator(".dp-secondary")).toHaveText("Acting on mail");
  await page.keyboard.press("Enter");
  await expect(palette(page)).toHaveCount(0);
  await expect(page.locator("#list .row.cursor")).toHaveClass(/pending-delete|staged/);

  await page.keyboard.press("Meta+Shift+K");
  await expect(palette(page).locator(".dp-section").first()).toHaveText("recently used");
  await expect(palette(page).locator("li[data-index]").first()).toContainText("Stage for Deletion");
  // A toggle is named for the way it goes now, and found by its other name.
  await page.keyboard.type("unstage");
  await expect(palette(page).locator("li[data-index]").first()).toContainText("Unstage");
  await expect(palette(page).locator("li[data-index]").first()).not.toHaveClass(/disabled/);
  await page.keyboard.press("Escape");
});

test("a command that cannot apply now stays listed, greyed out, and says why instead of running", async ({ page }) => {
  await open(page);
  await page.keyboard.press("Meta+Shift+K");
  await page.keyboard.type("unstage");
  const unstage = row(page, "Unstage");
  await expect(unstage).toHaveClass(/disabled/);
  await page.keyboard.press("Enter");
  await expect(palette(page)).toBeVisible();
  await expect(palette(page).locator(".dp-note")).toHaveText("Unstage: nothing is staged on it");
  await page.keyboard.press("Escape");
});

test("the prefix chooses the list: places, commands, tags", async ({ page }) => {
  await open(page);
  await page.keyboard.press("Meta+k");
  const title = palette(page).locator(".dp-title");
  await expect(title).toHaveText("Go to");
  await expect(palette(page)).toContainText("Work / Archive/2025");
  await page.keyboard.type(">");
  await expect(title).toHaveText("Commands");
  await expect(palette(page)).toContainText("New Message");
  await page.keyboard.press("Backspace");
  await expect(title).toHaveText("Go to");
  await page.keyboard.type("#");
  await expect(title).toHaveText("Tags");
  await expect(palette(page).locator("li[data-index] .dp-primary")).toHaveText(["work", "home"]);
  await page.keyboard.type("hom");
  await page.keyboard.press("Enter");
  await expect.poll(async () => (await callsOf(page, "search_tasks")).map((c) => c.query)).toContain("tag:home");
});

test("a command that needs a word asks for it, one step after another", async ({ page }) => {
  await open(page);
  await page.keyboard.press("Meta+Shift+K");
  await page.keyboard.type("new folder");
  await page.keyboard.press("Enter");
  await expect(page.locator(".dp.picker .dp-title")).toHaveText("New folder");
  await page.keyboard.type("Receipts");
  await expect(page.locator(".dp.picker li[data-index]")).toHaveText("“Receipts”");
  await page.keyboard.press("Enter");
  await expect.poll(async () => (await callsOf(page, "create_folder")).map((c) => c.name)).toEqual(["Receipts"]);
});

test("@ finds people and searches their mail; / finds a message and opens it", async ({ page }) => {
  await open(page);
  await page.keyboard.press("Meta+k");
  await page.keyboard.type("@sara");
  await expect(row(page, "Sara Brunner")).toBeVisible();
  await page.keyboard.press("Enter");
  await expect
    .poll(async () => (await callsOf(page, "search")).map((c) => c.query))
    .toContain("f:sara.brunner@example.com OR t:sara.brunner@example.com");

  await page.keyboard.press("Meta+k");
  await expect(palette(page)).toBeVisible();
  await page.keyboard.type("/message #7");
  await expect(row(page, "Message #7")).toBeVisible();
  await page.keyboard.press("Enter");
  await expect(page.locator("#reader")).toContainText("Message #7");
});

test("a command of one's own runs its chain from a key, the command line and the palette", async ({ page }) => {
  await open(page, "tasks=1&chat=1&own=1&bind=F:file-and-next");
  const staged = async () => (await callsOf(page, "stage_action")).length;

  await page.keyboard.press("F");
  await expect.poll(staged).toBeGreaterThan(0);
  await expect(page.locator("#list .row.cursor")).toHaveAttribute("data-msg", "1");

  await page.keyboard.press("Meta+Shift+K");
  await page.keyboard.type("file and move");
  const first = palette(page).locator("li[data-index]").first();
  await expect(first.locator(".dp-primary")).toHaveText("File and Move On");
  await expect(first.locator(".dp-secondary")).toHaveText("Your commands");
  await expect(first.locator("kbd")).toHaveText("F");
  const before = await staged();
  await page.keyboard.press("Enter");
  await expect.poll(staged).toBeGreaterThan(before);
  await expect(page.locator("#list .row.cursor")).toHaveAttribute("data-msg", "2");

  // One of one's own may name another; one that names itself is refused.
  await page.keyboard.press(":");
  await expect(page.locator("#status")).toHaveText(":");
  await page.keyboard.type("file-tw");
  await expect(page.locator("#completions")).toContainText("file-twice");
  await page.keyboard.press("Tab");
  await page.keyboard.press("Enter");
  await expect(page.locator("#list .row.cursor")).toHaveAttribute("data-msg", "4");
  await page.keyboard.press(":");
  await expect(page.locator("#status")).toHaveText(":");
  await page.keyboard.type("loop");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status")).toContainText("loop: runs itself");
});

test("a direct message with one person is found by their name, first", async ({ page }) => {
  await open(page);
  await page.keyboard.press("Meta+k");
  await page.keyboard.type("lukas");
  const first = palette(page).locator("li[data-index]").first();
  await expect(first.locator(".dp-primary")).toHaveText("lukas");
  await expect(first.locator(".dp-secondary")).toHaveText("Direct");
  await expect(palette(page).locator("li[data-index] .dp-primary", { hasText: /^Direct$/ })).toHaveCount(0);
  await page.keyboard.press("Enter");
  await expect(page.locator("#folders .sb-row.cursor .sb-label")).toHaveText("lukas");
});

test("the tag whose name is what was typed comes before the one whose initials it is", async ({ page }) => {
  await open(page, "tasks=1&moretags=1");
  await page.keyboard.press(":");
  await page.keyboard.type("add-tag");
  await page.keyboard.press("Enter");
  const picker = page.locator(".dp.picker");
  await expect(picker.locator(".dp-title")).toHaveText("Add tag");
  await page.keyboard.type("GSI");
  await expect(picker.locator("li[data-index] .dp-primary")).toHaveText(["GSI", "Garden Seedling Inventory"]);
  await expect(picker.locator("li.highlighted .dp-primary")).toHaveText("GSI");
});

test("a name that finds existing tags can still become a new tag", async ({ page }) => {
  await open(page, "tasks=1&moretags=1");
  await page.keyboard.press(":");
  await page.keyboard.type("add-tag");
  await page.keyboard.press("Enter");
  const picker = page.locator(".dp.picker");
  await page.keyboard.type("gs");
  const rows = picker.locator("li[data-index] .dp-primary");
  await expect(rows).toHaveText(["GSI", "Garden Seedling Inventory", "New tag “gs”"]);
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status")).toContainText(/tagged .* gs\b/);
});
