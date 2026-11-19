import { expect, Page, test } from "@playwright/test";

async function open(page: Page, at: string, query = ""): Promise<void> {
  await page.clock.setFixedTime(new Date(at));
  await page.goto(`/harness.html?${query}`);
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
}

function callsTo(page: Page, cmd: string): Promise<any[]> {
  return page.evaluate(
    (cmd) => ((window as any).__mock.calls as [string, any][]).filter(([c]) => c === cmd).map(([, a]) => a),
    cmd,
  );
}

const picker = (page: Page) => page.locator(".dp.picker");
const indicator = (page: Page) => page.locator("#status-steady .focus-mode");

test("zf asks how long, offers the named times still ahead, and the status line says until when", async ({ page }) => {
  await open(page, "2026-08-07T09:00:00", "workday=17:30");
  await expect(indicator(page)).toHaveCount(0);
  await page.keyboard.press("z");
  await page.keyboard.press("f");
  await expect(picker(page).locator("li")).toHaveText([
    /For 30 minutes/,
    /For 1 hour/,
    /For 2 hours/,
    /Until noon/,
    /Until the end of the workday.*17:30/,
    /Until I end it/,
    /In batched mode….*mail every 30m/,
  ]);
  await picker(page).locator("li", { hasText: "Until noon" }).click();

  await expect.poll(() => callsTo(page, "focus_start")).toEqual([
    { mode: "deep", ends: new Date("2026-08-07T12:00:00").getTime() },
  ]);
  await expect(indicator(page)).toHaveText("Focus until 12:00");
  await expect(indicator(page)).toHaveAttribute("title", /^Focus: deep — mail held/);
  await expect(page.locator("#status-msg, #status")).toContainText("Focus (deep) until 12:00");
});

test("a named time that has passed is not offered, nor the workday's end without one", async ({ page }) => {
  await open(page, "2026-08-07T14:00:00");
  await page.keyboard.press("z");
  await page.keyboard.press("f");
  await expect(picker(page).locator("li")).toHaveText([
    /For 30 minutes/,
    /For 1 hour/,
    /For 2 hours/,
    /Until I end it/,
    /In batched mode…/,
  ]);
  await picker(page).locator("li", { hasText: "Until I end it" }).click();
  await expect.poll(() => callsTo(page, "focus_start")).toEqual([{ mode: "deep", ends: null }]);
  await expect(indicator(page)).toHaveText("Focus until you end it");
});

test("another mode is one more step in the picker, and a typed span is taken", async ({ page }) => {
  await open(page, "2026-08-07T14:00:00");
  await page.keyboard.press("z");
  await page.keyboard.press("f");
  await picker(page).locator("li", { hasText: "In batched mode…" }).click();
  await expect(picker(page).locator("li").last()).toHaveText(/In deep mode…/);
  await page.keyboard.type("16:30");
  await expect(picker(page).locator("li")).toHaveText([/Until 16:30/]);
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "focus_start")).toEqual([
    { mode: "batched", ends: new Date("2026-08-07T16:30:00").getTime() },
  ]);
  await expect(indicator(page)).toHaveText("Focus until 16:30");
});

test("the command takes a mode and a span, and a click on the status line ends the focus", async ({ page }) => {
  await open(page, "2026-08-07T14:00:00");
  await page.keyboard.type(":focus-mode batched 1h");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "focus_start")).toEqual([
    { mode: "batched", ends: new Date("2026-08-07T15:00:00").getTime() },
  ]);
  await expect(indicator(page)).toHaveText("Focus until 15:00");

  await indicator(page).click();
  // A focus that runs keeps its mode, and ending it comes first.
  await expect(picker(page).locator("li").first()).toHaveText(/End focus.*batched until 15:00/);
  await expect(picker(page).locator("li").last()).toHaveText(/In deep mode…/);
  await picker(page).locator("li", { hasText: "End focus" }).click();
  await expect.poll(() => callsTo(page, "focus_stop")).toHaveLength(1);
  await expect(indicator(page)).toHaveCount(0);
  await expect(page.locator("#status-msg, #status")).toContainText("Focus ended");
});

test("a span that says nothing starts no focus, and off without one says so", async ({ page }) => {
  await open(page, "2026-08-07T14:00:00");
  await page.keyboard.type(":focus-mode whenever");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status-msg, #status")).toContainText("cannot tell how long “whenever” is");
  await page.keyboard.type(":focus-mode workday");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status-msg, #status")).toContainText("focus.workday_ends is not set");
  await page.keyboard.type(":focus-mode off");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status-msg, #status")).toContainText("no focus is running");
  expect(await callsTo(page, "focus_start")).toHaveLength(0);
  expect(await callsTo(page, "focus_stop")).toHaveLength(0);
});

test("the status line follows a focus that another window started or that ended by the clock", async ({ page }) => {
  await open(page, "2026-08-07T14:00:00");
  await page.evaluate(async () => {
    await (window as any).__TAURI_INTERNALS__.invoke("focus_start", { mode: "deep", ends: null });
    (window as any).__emit("focus-changed", null);
  });
  await expect(indicator(page)).toHaveText("Focus until you end it");
  await page.evaluate(async () => {
    await (window as any).__TAURI_INTERNALS__.invoke("focus_stop", {});
    (window as any).__emit("focus-changed", null);
  });
  await expect(indicator(page)).toHaveCount(0);
});

test("a span is read as a mute's, and noon and workday as times of the day", async ({ page }) => {
  await open(page, "2026-08-07T14:00:00");
  const ends = await page.evaluate(async () => {
    const { focusEnds } = await import("/src/focusmode.ts");
    const now = new Date("2026-08-07T14:00:00");
    const at = (spec: string, workday: string | null = "17:30") => {
      const ends = focusEnds(spec, workday, now);
      return typeof ends === "number" ? new Date(ends).toString().slice(0, 21) : ends;
    };
    return [at("90m"), at("workday"), at("noon"), at("forever"), at("workday", null), at("soon")];
  });
  expect(ends).toEqual([
    "Fri Aug 07 2026 15:30",
    "Fri Aug 07 2026 17:30",
    // Past noon, the next one.
    "Sat Aug 08 2026 12:00",
    null,
    undefined,
    undefined,
  ]);
});
