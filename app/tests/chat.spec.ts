import { expect, Page, test } from "@playwright/test";
import { chatFocusTests } from "./chatfocus";

/**
 * Chat (PLAN.md, Phase 15) against the harness's stubbed server: Waiting in
 * the list, the conversation in the reader, the composer under it.
 */

async function openWaiting(page: Page, query = "chat=1"): Promise<void> {
  await page.goto(`/harness.html?${query}`);
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#folders .sb-row", { hasText: "Waiting" }).click();
  await page.locator("#list .chat-row").first().waitFor();
}

/** Flip a switch in Today's View menu, and close the menu again. */
async function chooseInToday(page: Page, item: string): Promise<void> {
  await page.locator("#toolbar [data-today-menu]").click();
  await page.locator(`#toolbar .menu ${item}`).click();
  await page.keyboard.press("Escape");
}

/** Whether a switch in Today's View menu is on, read with the menu open. */
async function todaySwitch(page: Page, item: string): Promise<string | null> {
  await page.locator("#toolbar [data-today-menu]").click();
  const on = await page.locator(`#toolbar .menu ${item}`).getAttribute("aria-checked");
  await page.keyboard.press("Escape");
  return on;
}

async function callsTo(page: Page, cmd: string): Promise<any[]> {
  return page.evaluate(
    (cmd) => ((window as any).__mock.calls as [string, any][]).filter(([c]) => c === cmd).map(([, a]) => a),
    cmd,
  );
}

for (const look of ["hybrid", "terminal"]) {
  test(`Waiting's rows keep their look (${look})`, async ({ page }) => {
    await openWaiting(page, `chat=1&look=${look}`);
    const rows = await page.evaluate(() =>
      [...document.querySelectorAll("#list .chat-row")].map((row) => ({
        classes: row.className,
        cells: [...row.children].map((cell) => {
          const s = getComputedStyle(cell);
          return {
            classes: cell.className,
            text: (cell.textContent ?? "").replace(/\s+/g, " ").trim(),
            color: s.color,
            weight: s.fontWeight,
          };
        }),
      })),
    );
    expect(JSON.stringify(rows, null, 2)).toMatchSnapshot(`waiting${look === "hybrid" ? "" : `-${look}`}.json`);
  });
}

test("without a chat section there is no Chat in the sidebar", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await expect(page.locator("#folders .sb-section", { hasText: "Chat" })).toHaveCount(0);
});

test("Waiting lists one row per thread, and the reader follows the cursor", async ({ page }) => {
  await openWaiting(page);
  await expect(page.locator("#list .chat-row")).toHaveCount(3);
  await expect(page.locator("#status-mode")).toHaveText("CHAT");
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Waiting");
  await expect(page.locator("#folders .sb-row", { hasText: "Waiting" }).locator(".sb-badge")).toHaveText("3");
  // A direct message's own timeline is named after the person.
  await expect(page.locator("#reader .cv-t")).toHaveText("lukas");

  await page.keyboard.press("j");
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
  await expect(page.locator("#list .chat-row.cursor .room")).toHaveText("#orb-dev");
  await expect(page.locator("#list .chat-row.cursor .status .mark")).toHaveAttribute("data-g", "@");
  // What came since my last message is marked new.
  await expect(page.locator("#reader .cv-new + .cm")).toContainText("the notarisation step fails");
  await expect(page.locator('#reader [data-chat-message="m3"] .cm-file')).toContainText("ci-notarize.log");
  // Reading marks nothing done and sends nothing.
  expect(await callsTo(page, "chat_done")).toHaveLength(0);
  expect(await callsTo(page, "chat_send")).toHaveLength(0);
});

test("a line above the first message of each day names the day", async ({ page }) => {
  await page.clock.setFixedTime(new Date("2026-08-07T14:00:00"));
  await openWaiting(page);
  await page.keyboard.press("j");
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
  await expect(page.locator("#reader .cv-day")).toHaveText(["Yesterday", "Today"]);
  await expect(page.locator('#reader .cv-day + [data-chat-message="t-rel"]')).toHaveCount(1);
  await expect(page.locator('#reader .cv-day + [data-chat-message="m2"]')).toHaveCount(1);
});

test("the threads beside the cursor are read ahead, and moving onto one shows it at once", async ({ page }) => {
  await openWaiting(page);
  await expect(page.locator("#reader .cv-t")).toHaveText("lukas");
  await expect.poll(async () => (await callsTo(page, "chat_conversation")).map((a) => a.thread)).toContain("t-rel");
  await page.waitForTimeout(100);

  // The fresh copy takes its time; the reader does not wait for it.
  await page.evaluate(() => ((window as any).__mock.delay.chat_conversation = 3000));
  await page.keyboard.press("j");
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?", { timeout: 500 });
  await expect(page.locator("#reader .cv-loading")).toHaveCount(0);
  await expect(page.locator('#reader [data-chat-message="m3"] .cm-file')).toContainText("ci-notarize.log");

  // Back up again, the thread shown before comes from what was read.
  await page.keyboard.press("k");
  await expect(page.locator("#reader .cv-t")).toHaveText("lukas", { timeout: 500 });
  await expect(page.locator("#reader .cv-loading")).toHaveCount(0);

  // Typed ahead, the cursor does not wait for each thread to arrive.
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  await expect(page.locator("#list .chat-row.cursor .room")).toHaveText("#support", { timeout: 500 });
  await expect(page.locator("#reader .cv-t")).toHaveText("Customer cannot sign in after the update");
});

test("r writes in the thread, Enter sends, and the thread leaves Waiting", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("j");
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
  await page.keyboard.press("r");
  await expect(page.locator("#reader .cc-field")).toBeFocused();
  await page.keyboard.type("Renewing the certificate now.");
  // Shift-Enter starts a new line rather than sending.
  await page.keyboard.press("Shift+Enter");
  await page.keyboard.type("Back in ten.");
  expect(await callsTo(page, "chat_send")).toHaveLength(0);
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_send")).toHaveLength(1);
  const [sent] = await callsTo(page, "chat_send");
  expect(sent).toMatchObject({
    rid: "r-dev",
    thread: "t-rel",
    text: "Renewing the certificate now.\nBack in ten.",
    alsoInRoom: false,
  });
  await expect(page.locator("#list .chat-row")).toHaveCount(2);
  await expect(page.locator("#reader .cc-field")).not.toBeFocused();
});

test("in a room the composer keeps the keyboard after sending, for the next message", async ({ page }) => {
  await openWaiting(page);
  await page.locator("#folders .sb-row", { hasText: "Channels" }).locator("[data-fold]").click();
  await page.locator("#folders .sb-row", { hasText: "orb-dev" }).click();
  const field = page.locator("#reader .cc-field");
  await expect(field).toBeFocused();
  await page.keyboard.type("First");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_send")).toHaveLength(1);
  await expect(page.locator("#status")).toContainText("Sent");
  await expect(field).toBeFocused();
  await expect(field).toHaveValue("");
  await page.keyboard.type("Second");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_send")).toHaveLength(2);
  expect((await callsTo(page, "chat_send")).map((s) => s.text)).toEqual(["First", "Second"]);
  await expect(field).toBeFocused();
});

test("Esc leaves the composer and keeps what was typed", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("r");
  await page.keyboard.type("Half a thought");
  await page.keyboard.press("Escape");
  await expect(page.locator("#reader .cc-field")).not.toBeFocused();
  // The list has the keyboard again: j moves, and coming back finds the draft.
  await page.keyboard.press("j");
  await expect(page.locator("#reader .cc-field")).toHaveValue("");
  await page.keyboard.press("k");
  await expect(page.locator("#reader .cc-field")).toHaveValue("Half a thought");
});

test("a takes the thread out of Waiting, and ⌘Z puts it back", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("a");
  await expect(page.locator("#list .chat-row")).toHaveCount(2);
  expect(await callsTo(page, "chat_done")).toEqual([{ rid: "r-lukas", thread: "", undo: "done: lukas" }]);
  await expect(page.locator("#status-msg, #status")).toContainText("undo");
  await page.keyboard.press("Meta+z");
  await expect(page.locator("#list .chat-row")).toHaveCount(3);
  expect(await callsTo(page, "chat_restore_mark")).toHaveLength(1);
});

test("signed out, Waiting offers the sign-in and the status line says so", async ({ page }) => {
  await page.goto("/harness.html?chat=signedout");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await expect(page.locator("#folders .sb-row", { hasText: "Waiting" }).locator(".sb-badge")).toHaveText("!");
  await page.locator("#folders .sb-row", { hasText: "Waiting" }).click();
  await expect(page.locator("#list .empty-view")).toContainText("Sign in to chat.example.com");
  await expect(page.locator("#status-steady")).toContainText("chat signed out");
  await page.locator("#list [data-empty-command='chat-sign-in']").click();
  await expect.poll(() => callsTo(page, "chat_sign_in_window")).toHaveLength(1);
});

test("a long thread folds its earlier replies, za unfolds them, and it opens at its newest message", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("G");
  await expect(page.locator("#reader .cv-t")).toHaveText("Customer cannot sign in after the update");
  const fold = page.locator("#reader [data-chat-unfold]");
  await expect(fold).toHaveText("37 earlier replies");
  // The first message, and the newest four.
  await expect(page.locator("#reader .cm")).toHaveCount(5);
  await page.keyboard.press("z");
  await page.keyboard.press("a");
  await expect(fold).toHaveCount(0);
  await expect(page.locator("#reader .cm")).toHaveCount(42);
  const messages = page.locator("#reader [data-chat-messages]");
  await expect(messages).toContainText("does that ring a bell");
  await expect
    .poll(() => messages.evaluate((m) => m.scrollHeight - m.scrollTop - m.clientHeight))
    .toBeLessThan(2);
  expect(await messages.evaluate((m) => m.scrollHeight > m.clientHeight)).toBe(true);
});

test("text that chat.links names becomes a link, but not inside a link or code", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("j");
  const body = page.locator("#reader .cm-x", { hasText: "the notarisation step fails" });
  await expect(body.locator("a.lnk")).toHaveCount(3);
  await expect(body.locator("a.lnk").nth(0)).toHaveText("#4711");
  await expect(body.locator("a.lnk").nth(0)).toHaveAttribute("href", "https://tracker.example.com/issues/4711");
  await expect(body.locator("a.lnk").nth(1)).toHaveText("https://ci.example.com/runs/#4712");
  // An optional group that matches is part of the link; the full stop after it is not.
  await expect(body.locator("a.lnk").nth(2)).toHaveText("#4714.2");
  await expect(body.locator("a.lnk").nth(2)).toHaveAttribute("href", "https://tracker.example.com/issues/4714.2");
  await expect(body.locator("code")).toHaveText("#4713");
});

test("scrolling back brings in earlier messages, and keeps the place", async ({ page }) => {
  await openWaiting(page);
  const older = page.locator("#reader [data-chat-older]");
  await expect(older).toHaveText("Earlier messages");
  await older.click();
  await expect.poll(() => callsTo(page, "chat_older")).toHaveLength(1);
  await expect(page.locator("#reader .cm").first()).toContainText("the build from last night is green");
  await expect(older).toHaveCount(0);
});

test("threads are selected as messages are, and a marks every selected one done", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("Space");
  await expect(page.locator("#list .chat-row.selected")).toHaveCount(1);
  await expect(page.locator("#list .chat-row.cursor")).toHaveAttribute("data-chat", "1");
  // As in mail: the row Space touched is where a Shift-extension counts from.
  await page.keyboard.press("Shift+ArrowDown");
  await expect(page.locator("#list .chat-row.selected")).toHaveCount(3);
  await page.keyboard.press("Escape");
  await expect(page.locator("#list .chat-row.selected")).toHaveCount(0);

  await page.locator("#list .chat-row").nth(0).click();
  await page.locator("#list .chat-row").nth(2).click({ modifiers: ["Meta"] });
  await expect(page.locator("#list .chat-row.selected")).toHaveCount(1);
  await page.locator("#list .chat-row").nth(0).click({ modifiers: ["Shift"] });
  await expect(page.locator("#list .chat-row.selected")).toHaveCount(3);
  await page.keyboard.press("a");
  await expect(page.locator("#list .chat-row")).toHaveCount(0);
  expect(await callsTo(page, "chat_done")).toHaveLength(3);
  await page.keyboard.press("Meta+z");
  await expect(page.locator("#list .chat-row")).toHaveCount(3);
});

test("right-click opens the thread's menu, for the selection when the row is in it", async ({ page }) => {
  await openWaiting(page);
  await page.locator("#list .chat-row").nth(1).click({ button: "right" });
  await expect.poll(() => callsTo(page, "open_chat_menu")).toHaveLength(1);
  let [menu] = await callsTo(page, "open_chat_menu");
  expect(menu.disabled).toEqual(["chat-unmute-room"]);
  expect(menu.accelerators["chat-done"]).toBe("A");
  await expect(page.locator("#list .chat-row.cursor")).toHaveAttribute("data-chat", "1");

  await page.locator("#list .chat-row").nth(0).click({ modifiers: ["Meta"] });
  await page.locator("#list .chat-row").nth(2).click({ modifiers: ["Meta"] });
  await page.locator("#list .chat-row").nth(2).click({ button: "right" });
  await expect.poll(() => callsTo(page, "open_chat_menu")).toHaveLength(2);
  menu = (await callsTo(page, "open_chat_menu"))[1];
  expect(menu.disabled).toEqual(["chat-reply", "chat-open-in-browser", "chat-unmute-room"]);
  expect(menu.labels["chat-done"]).toBe("Done (2 threads)");
});

test("zw shows Chat in Today when nothing waits too, and says so", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=quiet");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("w");
  await expect(page.locator("#list .ch-section .empty")).toHaveText("Nothing waits");
  await expect(page.locator("#list .ch-section-head")).toHaveText(/^Chat\s*0\s*waiting$/);
  await page.keyboard.press("z");
  await page.keyboard.press("w");
  await expect(page.locator("#list .ch-section")).toHaveCount(0);
});

test("zw shows Chat in Today, and Tab and the chat keys reach it", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await expect(page.locator("#list .ch-section")).toHaveCount(0);
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("w");
  await expect(page.locator("#list .ch-section .ch-row")).toHaveCount(3);
  // Headed by what it is, as Mail and Tasks are.
  await expect(page.locator("#list .ch-section-head")).toHaveText(/^Chat\s*3\s*waiting$/);
  expect(await todaySwitch(page, '[data-today-show="chat"]')).toBe("true");

  // Mail, tasks, then chat.
  await page.keyboard.press("Tab");
  await page.keyboard.press("Tab");
  await expect(page.locator("#list")).toHaveClass(/chat-active/);
  await expect(page.locator("#reader .cv-t")).toHaveText("lukas");
  await page.keyboard.press("j");
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");

  // Keys that act on mail are refused rather than reaching the hidden mail.
  await page.keyboard.press("!");
  expect(await callsTo(page, "set_flag")).toHaveLength(0);
  await expect(page.locator("#status")).toContainText("acts on mail");

  await page.keyboard.press("a");
  await expect(page.locator("#list .ch-section .ch-row")).toHaveCount(2);
  expect(await callsTo(page, "chat_done")).toMatchObject([{ rid: "r-dev", thread: "t-rel" }]);

  // Up from the first thread is back in the tasks.
  await page.keyboard.press("g");
  await page.keyboard.press("g");
  await page.keyboard.press("k");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
});

test("Chat in Today is a pane of its own, lit alone and resized by its own line, in either layout", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  const box = async (selector: string) => (await page.locator(selector).boundingBox())!;
  for (const columns of [false, true]) {
    await page.goto("/harness.html?tasks=1&chat=1&reading=1");
    await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
    if (columns) await chooseInToday(page, '[data-today-layout="columns"]');
    await page.locator("#list").focus();
    await page.keyboard.press("z");
    await page.keyboard.press("w");
    await page.keyboard.press("z");
    await page.keyboard.press("r");
    await page.locator("#list > .rd-section .rd-row").first().waitFor();

    // Between the tasks and the reading, not inside the tasks.
    await expect(page.locator("#list > .ch-section")).toHaveCount(1);
    await expect(page.locator("#list .tk-section .ch-section")).toHaveCount(0);
    const tasks = await box("#list > .tk-section");
    const chat = await box("#list > .ch-section");
    const reading = await box("#list > .rd-section");
    expect(chat.y).toBeGreaterThanOrEqual(tasks.y + tasks.height - 1);
    expect(reading.y).toBeGreaterThanOrEqual(chat.y + chat.height - 1);
    if (columns) expect(chat.x).toBeGreaterThan((await box("#list > .mail-scroll")).x + 100);

    // Clicked, the chat takes the accent edge and the tasks do not.
    await page.locator("#list .ch-section .ch-row").nth(1).click();
    await expect(page.locator("#list")).toHaveClass(/chat-active/);
    const edge = (selector: string) =>
      page.locator(selector).evaluate((e) => getComputedStyle(e).borderLeftColor);
    // Polled: the edge's colour eases in.
    await expect.poll(async () => (await edge("#list > .ch-section")) !== (await edge("#list > .tk-section"))).toBe(true);

    // The line above it is dragged, kept, and reset by a double-click.
    const list = await box("#list");
    const fitted = chat.height;
    const split = await box("#list > [data-ch-split]");
    await page.mouse.move(split.x + 50, split.y);
    await page.mouse.down();
    await page.mouse.move(split.x + 50, list.y + list.height * 0.3, { steps: 5 });
    await page.mouse.up();
    const dragged = (await box("#list > .ch-section")).height;
    expect(dragged).toBeGreaterThan(fitted + 40);
    await expect(page.locator("#list > .rd-section")).toBeVisible();

    await page.reload();
    await page.locator("#list > .ch-section .ch-row").first().waitFor();
    // Polled, and a box not there yet is not an answer: Today draws itself again as it loads.
    const height = async () => (await page.locator("#list > .ch-section").boundingBox())?.height ?? Number.NaN;
    await expect.poll(async () => Math.abs((await height()) - dragged)).toBeLessThan(3);

    await page.locator("#list > [data-ch-split]").dblclick();
    expect(Math.abs((await box("#list > .ch-section")).height - fitted)).toBeLessThan(3);

    // Maximized from it, the chat is all the list shows.
    await page.locator("#list .ch-section .ch-row").first().click();
    await expect(page.locator("#list .ch-section .ch-row.cursor")).toHaveAttribute("data-chat", "0");
    await expect(page.locator("#list")).toHaveClass(/chat-active/);
    await page.keyboard.press("z");
    await page.keyboard.press("m");
    await expect(page.locator("#app")).toHaveClass(/maximized/);
    await expect(page.locator("#list > .tk-section")).toBeHidden();
    await expect(page.locator("#list > .rd-section")).toBeHidden();
    await expect(page.locator("#list > .ch-section")).toBeVisible();
    const width = async () => (await page.locator("#list > .ch-section").boundingBox())?.width ?? Number.NaN;
    await expect.poll(async () => (await width()) - (await box("#list")).width).toBeGreaterThan(-4);
    await page.evaluate(() => localStorage.clear());
  }
});

test("Today's View menu puts each section away and back, and the keys pass a section put away", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  const box = async (selector: string) => (await page.locator(selector).boundingBox())!;
  await page.goto("/harness.html?tasks=1&chat=1&reading=1");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
  for (const key of ["z", "w", "z", "r"]) await page.keyboard.press(key);
  await page.locator("#list > .rd-section .rd-row").first().waitFor();

  // One menu holds the four switches and the layout.
  await page.locator("#toolbar [data-today-menu]").click();
  const menu = page.locator("#toolbar .menu");
  await expect(menu.locator("[data-today-show] .lbl")).toHaveText(["Mail", "Tasks", "Chat", "Reading"]);
  await expect(menu.locator("[data-today-layout]")).toHaveCount(3);

  // It stays open while switches are flipped.
  await menu.locator('[data-today-show="mail"]').click();
  await menu.locator('[data-today-show="reading"]').click();
  await expect(menu.locator('[data-today-show="mail"]')).toHaveAttribute("aria-checked", "false");
  await page.keyboard.press("Escape");
  await expect(menu).toHaveCount(0);
  await expect(page.locator("#list > .mail-scroll")).toBeHidden();
  await expect(page.locator("#list > .rd-section")).toHaveCount(0);
  await expect(page.locator("#list > [data-tk-split]")).toBeHidden();
  // The tasks take the room the mail had.
  const list = await box("#list");
  expect(Math.abs((await box("#list > .tk-section")).y - list.y)).toBeLessThan(4);
  expect((await box("#list > .tk-section")).height).toBeGreaterThan(list.height * 0.4);

  // The keyboard starts in the tasks, and j runs from the last task into the chat.
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("G");
  await page.keyboard.press("j");
  await expect(page.locator("#list")).toHaveClass(/chat-active/);
  // Tab goes round the sections shown: chat, then tasks again.
  await page.keyboard.press("Tab");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);

  // In columns with the mail away, the rest have the whole width.
  await page.keyboard.press("z");
  await page.keyboard.press("l");
  await expect(page.locator("#list")).toHaveClass(/today-columns/);
  expect((await box("#list > .tk-section")).width).toBeGreaterThan(list.width - 4);
  expect((await box("#list > .ch-section")).width).toBeGreaterThan(list.width - 4);

  // The last section shown cannot be put away.
  await chooseInToday(page, '[data-today-show="chat"]');
  await page.locator("#toolbar [data-today-menu]").click();
  await expect(menu.locator('[data-today-show="tasks"]')).toHaveAttribute("aria-disabled", "true");
  await page.keyboard.press("Escape");
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("toggle-today-tasks off");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status")).toContainText("Tasks is all Today shows");
  await expect(page.locator("#list > .tk-section")).toBeVisible();

  // Put away, the tasks come back for a new task.
  await chooseInToday(page, '[data-today-show="mail"]');
  await chooseInToday(page, '[data-today-show="tasks"]');
  await expect(page.locator("#list > .tk-section")).toBeHidden();
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await page.locator("#list").focus();
  await page.keyboard.press("o");
  await expect(page.locator("#list > .tk-section")).toBeVisible();
  await page.evaluate(() => localStorage.clear());
});

test("a click on the thread Today's cursor already stands on shows it at once", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("w");
  await page.locator("#list .ch-section .ch-row").first().click();
  await expect(page.locator("#reader .cm-x").first()).toContainText("Can you have a quick look at the PR?", { timeout: 1000 });
});

test("j walks Today's sections in their order on screen: mail, tasks, chat, reading", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1&reading=1");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
  for (const key of ["z", "w", "z", "r"]) await page.keyboard.press(key);
  await page.locator("#list > .rd-section .rd-row").first().waitFor();
  await page.locator("#list .tk-row").last().click();
  await page.locator("#list").focus();
  await page.keyboard.press("j");
  await expect(page.locator("#list")).toHaveClass(/chat-active/);
  await page.keyboard.press("G");
  await page.keyboard.press("j");
  await expect(page.locator("#list")).toHaveClass(/reading-active/);
  await page.keyboard.press("Tab");
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await page.evaluate(() => localStorage.clear());
});

test("in Waiting, a key that acts on mail does nothing to the mail behind it", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("d");
  await expect(page.locator("#status")).toContainText("acts on mail");
  expect(await callsTo(page, "stage_action")).toHaveLength(0);
});

test("Direct and Channels open in the sidebar, and a room lists every thread, its timeline first", async ({ page }) => {
  await openWaiting(page);
  const channels = page.locator("#folders .sb-row", { hasText: "Channels" });
  await expect(channels.locator(".sb-badge")).toHaveText("2");
  await channels.locator("[data-fold]").click();
  await page.locator("#folders .sb-row", { hasText: "orb-dev" }).click();
  await expect(page.locator("#toolbar .tb-t")).toHaveText("#orb-dev");
  await expect(page.locator("#reader .cc-field")).toBeFocused();
  await expect(page.locator("#app")).not.toHaveClass(/reader-alone/);
  const rows = page.locator("#list .chat-row");
  await expect(rows).toHaveCount(3);
  // The room's own timeline leads, even before anything is written in it.
  await expect(rows.nth(0)).toContainText("Nothing written in the room yet");
  await expect(rows.nth(2)).toContainText("Flaky store_integration test on CI");
  await expect(rows.nth(2).locator(".status .mark")).toHaveAttribute("data-g", "r");

  // c writes to the room, outside any thread.
  await page.locator("#list").focus();
  await page.keyboard.press("j");
  await page.keyboard.press("c");
  await expect(page.locator("#reader .cc-field")).toBeFocused();
  await page.keyboard.type("Deploy at four.");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_send")).toHaveLength(1);
  expect((await callsTo(page, "chat_send"))[0]).toMatchObject({ rid: "r-dev", thread: "", text: "Deploy at four." });
});

test("gr finds a room or a person by name: a person gets a new direct message, a public room is joined", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("g");
  await page.keyboard.press("r");
  const input = page.locator(".picker .dp-input");
  await expect(input).toBeFocused();
  await input.fill("luc");
  await expect(page.locator(".picker li", { hasText: "Lucia Ferri" })).toBeVisible();
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_open_direct")).toEqual([{ username: "lucia" }]);
  await expect(page.locator("#toolbar .tb-t")).toHaveText("lucia");
  // Nothing but the conversation yet: the reader stands alone, its composer
  // has the keyboard and is named after the person.
  await expect(page.locator("#reader .cc-field")).toBeFocused();
  await expect(page.locator("#reader .cc-field")).toHaveAttribute("placeholder", "Write to lucia");
  await expect(page.locator("#app")).toHaveClass(/reader-alone/);
  await expect(page.locator("#list")).toBeHidden();
  await page.keyboard.press("Escape");
  await expect(page.locator("#reader")).toHaveClass(/focused/);

  await page.keyboard.press("g");
  await page.keyboard.press("r");
  await page.locator(".picker .dp-input").fill("rand");
  await expect(page.locator(".picker li", { hasText: "#random" })).toBeVisible();
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_join")).toEqual([{ rid: "r-random" }]);
  await expect(page.locator("#toolbar .tb-t")).toHaveText("#random");
});

test("Direct shows how each person stands", async ({ page }) => {
  await openWaiting(page);
  await page.locator("#folders .sb-row", { hasText: "Direct" }).locator("[data-fold]").click();
  await expect(page.locator("#folders .sb-row", { hasText: "lukas" }).locator(".sb-presence")).toHaveClass(/away/);
});

test("R starts a thread on a message of the room's timeline", async ({ page }) => {
  await openWaiting(page);
  await expect(page.locator("#reader .cv-t")).toHaveText("lukas");
  await page.keyboard.press("R");
  await expect(page.locator("#reader .cc-field")).toBeFocused();
  await expect(page.locator("#reader .cc-to")).toContainText("Can you have a quick look at the PR?");
  await page.keyboard.type("Looking now.");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_send")).toHaveLength(1);
  expect((await callsTo(page, "chat_send"))[0]).toMatchObject({ rid: "r-lukas", thread: "m1", text: "Looking now." });

  // The button on a message does the same, and the chip can be taken away.
  await page.locator("#reader .cm").first().hover();
  await page.locator("#reader [data-chat-thread]").first().click();
  await expect(page.locator("#reader .cc-to")).toBeVisible();
  await page.locator("#reader [data-chat-thread-off]").click();
  await expect(page.locator("#reader .cc-to")).toHaveCount(0);
});

test("a server without threads offers none, and R says so", async ({ page }) => {
  await openWaiting(page, "chat=1&threads=off");
  await expect(page.locator("#reader .cv-t")).toHaveText("lukas");
  await page.locator("#reader .cm").first().hover();
  await expect(page.locator("#reader .cm-tools").first()).toBeVisible();
  await expect(page.locator("#reader [data-chat-thread]")).toHaveCount(0);
  await page.locator("#list").focus();
  await page.keyboard.press("R");
  await expect(page.locator("#status")).toContainText("threads are turned off");
  await expect(page.locator("#reader .cc-to")).toHaveCount(0);
});

test("the same person's next message stands apart, its time in the gutter, and nothing drawn over it", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  const cont = page.locator("#reader .cm.cont").last();
  await expect(cont.locator(".cm-h")).toHaveCount(0);
  await expect(cont.locator(".cm-gt")).toBeHidden();
  await cont.hover();
  await expect(cont.locator(".cm-gt")).toBeVisible();
  const gap = await cont.evaluate((m) => {
    const previous = m.previousElementSibling!.querySelector(".cm-x")!.getBoundingClientRect();
    return m.querySelector(".cm-x")!.getBoundingClientRect().top - previous.bottom;
  });
  expect(gap).toBeGreaterThanOrEqual(8);
  // The tools stand beside the short line, not across the pane at its far edge.
  const [text, tools] = await cont.evaluate((m) => [
    m.querySelector(".cm-x")!.getBoundingClientRect().right,
    m.querySelector(".cm-tools")!.getBoundingClientRect().left,
  ]);
  expect(tools - text).toBeLessThan(200);
});

test("a room is left after asking, and a new one is created by name", async ({ page }) => {
  await openWaiting(page);
  await page.locator("#folders .sb-row", { hasText: "Channels" }).locator("[data-fold]").click();
  await page.locator("#folders .sb-row", { hasText: "orb-dev" }).click();
  await expect(page.locator("#toolbar .tb-t")).toHaveText("#orb-dev");
  await page.evaluate(() => ((window as any).__askAnswer = "Leave"));
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("chat-leave-room");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_leave")).toEqual([{ rid: "r-dev", undo: "leave #orb-dev" }]);
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Waiting");
  await expect(page.locator("#folders .sb-row", { hasText: "orb-dev" })).toHaveCount(0);
  await expect(page.locator("#status")).toContainText("⌘Z to undo");
  // ⌘Z joins it again.
  await page.keyboard.press("Meta+z");
  await expect(page.locator("#status")).toContainText("undone: leave #orb-dev");
  await expect(page.locator("#folders .sb-row", { hasText: "orb-dev" })).toHaveCount(1);

  await page.keyboard.press(":");
  await page.keyboard.type("chat-create-room release-0.9");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_create_room")).toEqual([{ name: "release-0.9", private: false }]);
  await expect(page.locator("#toolbar .tb-t")).toHaveText("#release-0.9");
});

test("the completion opens over the word it completes, one line a person", async ({ page }) => {
  await openWaiting(page);
  // A group conversation goes by the names of all its people: nobody to name with @.
  await page.evaluate(() =>
    (window as any).__TAURI_INTERNALS__.invoke("chat_harness_rejoin", {
      room: { rid: "r-trio", kind: "d", name: "lukas,lucia,nora", fname: "Lukas Meier, Lucia Ferri, Nora Keller", label: "Direct", unread: 0, mentions: 0 },
    }),
  );
  await page.keyboard.press("r");
  const field = page.locator("#reader .cc-field");
  await expect(field).toBeFocused();
  const pop = page.locator("#reader .mpop");
  const left = async () => (await pop.boundingBox())!.x - (await field.boundingBox())!.x;

  await page.keyboard.type("@lu");
  await expect(pop.locator(".ci").first()).toContainText("lukas");
  const atStart = await left();
  expect(atStart).toBeLessThan(20);
  await expect(pop.locator(".ci", { hasText: "lucia" })).toHaveCount(1);
  await expect(pop.locator(".ci", { hasText: "," })).toHaveCount(0);
  const heights = await pop.locator(".ci > *").evaluateAll((cells) => cells.map((c) => c.scrollHeight - c.clientHeight));
  expect(Math.max(...heights)).toBeLessThanOrEqual(0);

  await field.fill("");
  await page.keyboard.type("Thanks for this, @lu");
  await expect(pop.locator(".ci").first()).toContainText("lukas");
  expect(await left()).toBeGreaterThan(atStart + 80);

  // Far to the right, it stays inside the composer.
  await field.fill("");
  await page.keyboard.type(`${"word ".repeat(40)}@lu`);
  await expect(pop.locator(".ci").first()).toContainText("lukas");
  const box = (await pop.boundingBox())!;
  const composer = (await page.locator("#reader .cc").boundingBox())!;
  expect(box.x).toBeGreaterThanOrEqual(composer.x - 1);
  expect(box.x + box.width).toBeLessThanOrEqual(composer.x + composer.width + 1);
});

test("@, : and # complete people, emoji and rooms in the composer", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("r");
  const field = page.locator("#reader .cc-field");
  await expect(field).toBeFocused();
  await page.keyboard.type("Thanks @lu");
  await expect(page.locator("#reader .mpop .ci").first()).toContainText("lukas");
  await page.keyboard.press("Tab");
  await expect(field).toHaveValue("Thanks @lukas ");
  await page.keyboard.type(":ta");
  await expect(page.locator("#reader .mpop .ci").first()).toContainText("🎉");
  // Enter takes the completion rather than sending.
  await page.keyboard.press("Enter");
  await expect(field).toHaveValue("Thanks @lukas :tada: ");
  await page.keyboard.type("see #orb");
  await page.keyboard.press("Enter");
  await expect(field).toHaveValue("Thanks @lukas :tada: see #orb-dev ");
  expect(await callsTo(page, "chat_send")).toHaveLength(0);
});

test("up in an empty composer edits my last message; my messages can be deleted", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("j");
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
  await page.keyboard.press("r");
  const field = page.locator("#reader .cc-field");
  await expect(field).toBeFocused();
  await page.keyboard.press("ArrowUp");
  await expect(field).toHaveValue("README is done, the migration notice is in review.");
  await expect(page.locator("#reader .cc-to")).toContainText("Editing your message");
  await page.keyboard.press("End");
  await page.keyboard.type(" Merged.");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_edit")).toEqual([
    { rid: "r-dev", id: "m2", text: "README is done, the migration notice is in review. Merged." },
  ]);

  await page.evaluate(() => ((window as any).__askAnswer = "Delete"));
  await page.locator('#reader [data-chat-message="m2"]').hover();
  await page.locator('#reader [data-chat-delete="m2"]').click();
  await expect.poll(() => callsTo(page, "chat_delete")).toEqual([{ rid: "r-dev", id: "m2" }]);
});

test("the configured reactions are offered on a message, and a reaction toggles", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("j");
  const first = page.locator('#reader [data-chat-message="t-rel"]');
  // Under the pointer, at once: no button to open them first.
  await expect(first.locator(".cm-tools")).toBeHidden();
  await first.hover();
  const bar = first.locator(".cm-tools .q");
  await expect(bar).toHaveText(["👍", "🙏", "🎉", "👌"]);
  await bar.nth(1).click();
  await expect.poll(() => callsTo(page, "chat_react")).toEqual([{ rid: "r-dev", id: "t-rel", emoji: ":pray:", on: true, undo: "react :pray:" }]);
  // An existing reaction shows its emoji, and joins it on a click.
  const eyes = page.locator('#reader [data-chat-message="m4"] .rx');
  await expect(eyes).toHaveText("👀 1");
  await eyes.click();
  await expect.poll(() => callsTo(page, "chat_react")).toHaveLength(2);
  expect((await callsTo(page, "chat_react"))[1]).toMatchObject({ rid: "r-dev", id: "m4", emoji: ":eyes:", on: true });
});

test("a pasted file goes with the message, and a draft survives a restart", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("r");
  await page.keyboard.type("The screenshot");
  await page.locator("#reader .cc-field").evaluate((field) => {
    const data = new DataTransfer();
    data.items.add(new File(["png bytes"], "shot.png", { type: "image/png" }));
    field.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }));
  });
  await expect(page.locator("#reader .cc-a .cm-file")).toContainText("shot.png");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_send_files")).toHaveLength(1);
  const [sent] = await callsTo(page, "chat_send_files");
  expect(sent).toMatchObject({ rid: "r-lukas", thread: "", text: "The screenshot" });
  expect(sent.files[0]).toMatchObject({ name: "shot.png", mime: "image/png", path: null });
  expect(atob(sent.files[0].data_base64)).toBe("png bytes");
  await expect(page.locator("#status")).toContainText("Sent");

  // Answered, the thread has left Waiting, and the cursor is on the next.
  await expect(page.locator("#reader .cv-t")).toContainText("Release 0.9");
  await expect(page.locator("#reader .cc-field")).not.toBeFocused();
  await page.keyboard.press("r");
  await expect(page.locator("#reader .cc-field")).toBeFocused();
  await page.keyboard.type("Half a thought");
  await expect(page.locator("#reader .cc-field")).toHaveValue("Half a thought");
  await page.keyboard.press("Escape");
  await page.reload();
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#folders .sb-row", { hasText: "Waiting" }).click();
  await expect(page.locator("#reader .cc-field")).toHaveAttribute("placeholder", "Write to lukas");
  await page.keyboard.press("j");
  await expect(page.locator("#reader .cc-field")).toHaveValue("Half a thought");
});

test("a quote shows whom it quotes and leads to the message; a file opens with a click", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("j");
  const quote = page.locator('#reader [data-chat-message="m4"] .cm-quote');
  await expect(quote.locator("b")).toHaveText("Sam Taylor");
  await quote.click();
  await expect(page.locator('#reader [data-chat-message="m2"]')).toHaveClass(/flash/);
  await page.locator('#reader [data-chat-file="m3"]').click();
  await expect.poll(() => callsTo(page, "chat_open_file")).toEqual([{ id: "m3", index: 0 }]);
  await expect(page.locator("#status")).toContainText("Opened ci-notarize.log");
});

test("an image shows as a preview in its own shape, opens with a click, and is a file when it has no preview", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("j");
  const message = page.locator('#reader [data-chat-message="m4"]');
  const image = message.locator(".cm-image");
  // The preview the server could not give leaves the file, as a file.
  await expect(message.locator(".cm-file")).toHaveText(/cert\.png/);
  await expect(image).toHaveCount(1);
  // 572 × 750 into a box of 320 × 240: the height decides, the shape is kept.
  expect(await image.evaluate((b) => [b.offsetWidth, b.offsetHeight])).toEqual([183, 240]);
  await expect(image.locator("img")).toHaveAttribute("src", /^data:image\//);
  await expect(image.locator("img")).toHaveAttribute("alt", "codesign.png");
  expect((await callsTo(page, "chat_file_preview")).map((a) => a.index).sort()).toEqual([0, 1]);
  await image.click();
  await expect.poll(() => callsTo(page, "chat_open_file")).toEqual([{ id: "m4", index: 0 }]);
  // Fetched once: a redraw draws it at once, without asking again.
  await page.keyboard.press("k");
  await page.keyboard.press("j");
  await expect(image.locator("img")).toHaveAttribute("src", /^data:image\//);
  expect(await callsTo(page, "chat_file_preview")).toHaveLength(2);
});

test("m marks a thread read on the server, and unread again", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("m");
  await expect.poll(() => callsTo(page, "chat_set_read")).toEqual([{ rid: "r-lukas", thread: "", read: true, undo: "mark lukas read" }]);
  await expect(page.locator("#status")).toContainText("Marked read");
  await page.keyboard.press("m");
  await expect.poll(() => callsTo(page, "chat_set_read")).toHaveLength(2);
  expect((await callsTo(page, "chat_set_read"))[1]).toMatchObject({ rid: "r-lukas", thread: "", read: false });
  await expect(page.locator("#status")).toContainText("Marked unread");
  // ⌘Z marks it read again.
  await page.keyboard.press("Meta+z");
  await expect(page.locator("#status")).toContainText("undone: mark lukas unread");
  expect((await callsTo(page, "chat_set_read"))[2]).toEqual({ rid: "r-lukas", thread: "", read: true });
});

test("F narrows Waiting to the threads that match, and Esc brings the rest back", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("Shift+F");
  await page.keyboard.type("safari");
  await expect(page.locator("#list .chat-row")).toHaveCount(1);
  await expect(page.locator("#list .fbar .cnt")).toHaveText("1 match");
  await expect(page.locator("#reader .cv-t")).toHaveText("Customer cannot sign in after the update");
  await page.keyboard.type("x");
  await expect(page.locator("#list")).toContainText("Nothing matches");
  await page.keyboard.press("Escape");
  await expect(page.locator("#list .chat-row")).toHaveCount(3);
});

test("t makes a task of the thread's newest message and takes the thread out of Waiting; undo puts both back", async ({ page }) => {
  await openWaiting(page, "chat=1&tasks=1");
  await page.keyboard.press("j");
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
  await page.keyboard.press("t");
  await expect.poll(() => callsTo(page, "chat_create_task")).toHaveLength(1);
  expect((await callsTo(page, "chat_create_task"))[0]).toMatchObject({ rid: "r-dev", thread: "t-rel", message: null });
  await expect(page.locator("#status")).toContainText("created task");
  await expect(page.locator("#list .chat-row")).toHaveCount(2);
  await page.keyboard.press("Meta+z");
  await expect.poll(() => callsTo(page, "chat_restore_mark")).toHaveLength(1);
  expect(await callsTo(page, "trash_tasks")).toHaveLength(1);
});

test("a message's task shows on it, opens from it, and the task links back to the message", async ({ page }) => {
  await openWaiting(page, "chat=1&tasks=1");
  await page.locator("#folders .sb-row", { hasText: "Channels" }).locator("[data-fold]").click();
  await page.locator("#folders .sb-row", { hasText: "orb-dev" }).click();
  await expect(page.locator("#toolbar .tb-t")).toHaveText("#orb-dev");
  await page.locator("#list .chat-row", { hasText: "Release 0.9" }).click();
  const message = page.locator('#reader [data-chat-message="m3"]');
  await message.hover();
  await message.locator("[data-chat-task]").click();
  await expect.poll(() => callsTo(page, "chat_create_task")).toHaveLength(1);
  expect((await callsTo(page, "chat_create_task"))[0]).toMatchObject({ rid: "r-dev", thread: "t-rel", message: "m3" });
  const chip = message.locator(".cm-task");
  await expect(chip).toContainText("the notarisation step fails");
  await chip.click();
  const link = page.locator('#reader [data-task-action="open-link"]');
  await expect(link).toContainText("#orb-dev");
  await link.click();
  await expect(page.locator("#toolbar .tb-t")).toHaveText("#orb-dev");
  await expect(page.locator('#reader [data-chat-message="m3"]')).toHaveClass(/flash/);
});

test("the chat window shows chat alone, two lines a thread, and leaves mail to the mailbox", async ({ page }) => {
  await page.goto("/harness.html?chat=1&tasks=1&window=chat");
  await page.locator("#list .chat-row").first().waitFor();
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Waiting");
  // The sidebar is chat's group, open: Waiting, Direct and Channels.
  await expect(page.locator("#folders .sb-row").first()).toContainText("Waiting");
  await expect(page.locator("#folders .sb-row", { hasText: "Inbox" })).toHaveCount(0);
  const row = page.locator("#list .chat-row").first();
  const [from, subject] = await Promise.all([row.locator(".from").boundingBox(), row.locator(".subject").boundingBox()]);
  expect(subject!.y).toBeGreaterThan(from!.y + from!.height - 1);
  await page.screenshot({ path: "/tmp/orb-chat-window.png" });
  await page.keyboard.press("/");
  await expect(page.locator("#status")).toContainText("this window shows chat");
});

test("a click on a chat notification opens the thread it announced", async ({ page }) => {
  await openWaiting(page);
  await page.evaluate(() => (window as any).__emit("chat-notification-opened", ["r-sup", "t-login"]));
  await expect(page.locator("#reader .cv-t")).toHaveText("Customer cannot sign in after the update");
  await expect(page.locator("#list .chat-row.cursor")).toHaveAttribute("data-chat", "2");
});

test("a pause after a prefix key shows what can follow it, and the next key puts it away", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("g");
  const hints = page.locator(".keyhints");
  await expect(hints.locator(".kh-row", { hasText: "Open a chat room" })).toContainText("r");
  // In chat the chat table is in force: gx opens the thread in Rocket.Chat.
  await expect(hints.locator(".kh-row", { hasText: "Rocket.Chat" })).toContainText("x");
  await page.keyboard.press("Escape");
  await expect(hints).toHaveCount(0);
  // Typed on without a pause, it never shows.
  await page.keyboard.press("g");
  await page.keyboard.press("g");
  await page.waitForTimeout(600);
  await expect(hints).toHaveCount(0);
});

test("gn starts a chat with the people picked, one after another", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("g");
  await page.keyboard.press("n");
  const picker = page.locator(".dp.picker");
  await expect(picker.locator("li").first()).toContainText("@lukas");
  // Enter adds the person under the cursor; the field starts afresh.
  await page.keyboard.press("Enter");
  await expect(picker.locator(".dp-chip")).toHaveText(["lukas×"]);
  await page.keyboard.type("luc");
  await expect(picker.locator("li", { hasText: "Lucia Ferri" })).toBeVisible();
  await page.keyboard.press("Enter");
  await expect(picker.locator(".dp-chip")).toHaveCount(2);
  // Backspace on the empty field takes the last one back, and it can be added again.
  await page.keyboard.press("Backspace");
  await expect(picker.locator(".dp-chip")).toHaveCount(1);
  await page.keyboard.type("luc");
  await expect(picker.locator("li", { hasText: "Lucia Ferri" })).toBeVisible();
  await page.keyboard.press("Meta+Enter");
  await expect.poll(() => callsTo(page, "chat_open_group")).toEqual([{ usernames: ["lukas", "lucia"] }]);
  await expect(picker).toHaveCount(0);
  await expect(page.locator("#reader .cc-field")).toBeFocused();
});

/** The kinds of Today's timeline rows, top to bottom. */
async function timelineKinds(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    [...document.querySelectorAll<HTMLElement>("#list.today-mixed .rows .row")]
      .sort((a, b) => parseFloat(a.style.top) - parseFloat(b.style.top))
      .map((row) => row.querySelector<HTMLElement>(".kind")!.dataset.k ?? ""),
  );
}

/** Today with chat and reading on, mail dated an hour apart, in Mixed. */
async function openMixed(page: Page): Promise<void> {
  await page.goto("/harness.html?tasks=1&chat=1&reading=1&spread=1");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
  for (const key of ["z", "w", "z", "r", "z", "x"]) await page.keyboard.press(key);
  await page.locator("#list.today-mixed .rows .row.chat-row").first().waitFor();
}

test("Mixed puts the mail, the threads and the reading on one timeline beside the tasks", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await openMixed(page);
  // Each at its moment: the mail an hour apart from 12:30 in both inboxes,
  // the threads at 10:41, 10:20 and 09:05, the reading at 09:15 and 06:15 (UTC).
  const order = ["mail", "mail", "mail", "mail", "chat", "mail", "chat", "mail", "reading", "chat", "reading"];
  await expect.poll(() => timelineKinds(page)).toEqual(order);
  await expect(page.locator("#list > .mail-scroll .mail-sec .sec-t > .lbl")).toHaveText("Timeline");
  await expect(page.locator("#list > .mail-scroll .mail-sec .n")).toHaveText("11");
  // No sections of their own: the tasks stand on the right.
  await expect(page.locator("#list > .ch-section")).toHaveCount(0);
  await expect(page.locator("#list > .rd-section")).toHaveCount(0);
  const timeline = (await page.locator("#list > .mail-scroll").boundingBox())!;
  const tasks = (await page.locator("#list > .tk-section").boundingBox())!;
  expect(tasks.x).toBeGreaterThan(timeline.x + timeline.width - 4);

  // Turned over, oldest first.
  await page.locator("#toolbar [data-sort-menu]").click();
  await page.locator('#toolbar .menu [data-dir="1"]').click();
  await expect(page.locator("#toolbar [data-sort-menu] .lbl")).toHaveText("Oldest first");
  await expect.poll(() => timelineKinds(page)).toEqual([...order].reverse());

  // Remembered on this machine, and zx goes back to the layout it left.
  await page.reload();
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await expect(page.locator("#list")).toHaveClass(/today-mixed/);
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("x");
  await expect(page.locator("#list")).not.toHaveClass(/today-mixed/);
  await expect(page.locator("#list > .ch-section")).toHaveCount(1);
  await page.evaluate(() => localStorage.clear());
});

test("on the timeline the keys and the reader are those of the row under the cursor", async ({ page }) => {
  await openMixed(page);
  const position = page.locator("#status-steady .pos");
  await expect(position).toHaveText("Mail 1/11");
  for (let i = 0; i < 4; i++) await page.keyboard.press("j");
  await expect(position).toHaveText("Chat 5/11");
  await expect(page.locator("#list .rows .row.cursor")).toHaveClass(/chat-row/);
  await expect(page.locator("#reader")).toContainText("Waiting since");

  // a is done for a thread, and the thread leaves the timeline.
  await page.keyboard.press("a");
  await expect.poll(() => callsTo(page, "chat_done")).toEqual([expect.objectContaining({ rid: "r-lukas" })]);
  await expect.poll(async () => (await timelineKinds(page)).filter((k) => k === "chat").length).toBe(2);

  // On to a reading item, whose keys are the mail's.
  await page.keyboard.press("G");
  await expect(position).toHaveText("Reading 10/10");
  await expect(page.locator("#list .rows .row.cursor")).toHaveClass(/reading-row/);

  // Tab goes to the tasks beside the timeline and back to where it was.
  await page.keyboard.press("Tab");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("j");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("Tab");
  await expect(position).toHaveText("Reading 10/10");
  await page.keyboard.press("k");
  await expect(position).toHaveText("Chat 9/10");
  await page.evaluate(() => localStorage.clear());
});

test("the reader follows the thread clicked, after the one it showed left Waiting elsewhere", async ({ page }) => {
  await openMixed(page);
  for (let i = 0; i < 4; i++) await page.keyboard.press("j");
  await expect(page.locator("#reader")).toContainText("Can you have a quick look at the PR?");

  // The thread is done on another Mac while the tasks have the keyboard.
  await page.keyboard.press("Tab");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.evaluate(async () => {
    await (window as any).__TAURI_INTERNALS__.invoke("chat_done", { rid: "r-lukas", thread: "" });
    (window as any).__emit("chat-changed", null);
  });
  await expect.poll(async () => (await timelineKinds(page)).filter((k) => k === "chat").length).toBe(2);

  // The thread that took its place shows when clicked, not the one that left.
  await page.locator("#list .rows .row.chat-row", { hasText: "Release 0.9" }).click();
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
  await expect(page.locator("#reader")).not.toContainText("Can you have a quick look at the PR?");
  await page.evaluate(() => localStorage.clear());
});

test("a panel opened over a thread is shown, and never keeps the reader from the row the cursor is on", async ({ page }) => {
  await openMixed(page);
  const title = page.locator("#reader > header h1");
  for (let i = 0; i < 4; i++) await page.keyboard.press("j");
  await expect(page.locator("#reader")).toContainText("Can you have a quick look at the PR?");

  // The panel takes the reader from the thread, and q gives it back.
  await page.keyboard.press("p");
  await expect(title).toHaveText("Staged actions");
  await expect(page.locator("#reader")).not.toContainText("Can you have a quick look at the PR?");
  await page.keyboard.press("q");
  await expect(title).toHaveCount(0);
  await expect(page.locator("#reader")).toContainText("Can you have a quick look at the PR?");

  // Open again while every thread leaves Waiting elsewhere: the cursor is
  // put on the first row, a mail, and a click on another mail shows that mail.
  await page.keyboard.press("p");
  await expect(title).toHaveText("Staged actions");
  await page.evaluate(() => {
    for (const key of ["r-lukas\u0000", "r-dev\u0000t-rel", "r-sup\u0000t-login"]) {
      // @ts-expect-error: the harness's own fixture
      CHAT_DONE.set(key, true);
    }
    (window as any).__emit("chat-changed", null);
  });
  await expect.poll(async () => (await timelineKinds(page)).filter((k) => k === "chat").length).toBe(0);
  await expect(title).toHaveText("Staged actions");
  const mail = page.locator('#list .rows .row[data-msg="2"]');
  await mail.click();
  await expect(mail).toHaveClass(/cursor/);
  await expect(page.locator("#reader")).not.toContainText("Staged actions");
  await expect(page.locator("#reader .cv-t")).toHaveCount(0);
  await expect(page.locator("#reader .h-subj")).toHaveText(await mail.locator(".subject").innerText());

  // Moving on from a thread under a panel shows the thread the cursor went to.
  await page.evaluate(() => {
    // @ts-expect-error: the harness's own fixture
    CHAT_DONE.clear();
    (window as any).__emit("chat-changed", null);
  });
  await page.locator("#list .rows .row.chat-row", { hasText: "Release 0.9" }).click();
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
  await page.keyboard.press("p");
  await expect(title).toHaveText("Staged actions");
  await page.keyboard.press("Tab");
  await page.keyboard.press("Tab");
  await expect(page.locator("#reader .cv-t")).toHaveText("Release 0.9 — what is still missing?");
  await page.evaluate(() => localStorage.clear());
});

test("the thread clicked shows what was written in it while the cursor was on mail", async ({ page }) => {
  await openMixed(page);
  for (let i = 0; i < 4; i++) await page.keyboard.press("j");
  await expect(page.locator("#reader")).toContainText("Can you have a quick look at the PR?");

  // The cursor goes to a mail, and the thread it left gets another message.
  await page.locator("#list .rows .row:not(.chat-row):not(.reading-row)").first().click();
  await expect(page.locator("#reader .cv-t")).toHaveCount(0);
  await page.evaluate(async () => {
    await (window as any).__TAURI_INTERNALS__.invoke("chat_receive", { rid: "r-lukas", thread: "", text: "Never mind, it is green again." });
    (window as any).__emit("chat-changed", null);
  });
  const row = page.locator("#list .rows .row.chat-row", { hasText: "Never mind, it is green again." });
  await expect(row).toBeVisible();

  // The reader shows the message the row shows, not the thread as it was read.
  await row.click();
  await expect(page.locator("#reader .cv-msgs")).toContainText("Never mind, it is green again.");
  await page.evaluate(() => localStorage.clear());
});

test("the filter narrows every kind in Today, and is:chat keeps the threads alone", async ({ page }) => {
  await openMixed(page);
  const filter = async (text: string) => {
    await page.locator("#list").focus();
    await page.keyboard.press("F");
    await page.locator("#list .fbar input").fill(text);
    await page.keyboard.press("Enter");
  };

  await filter("is:chat");
  await expect.poll(() => timelineKinds(page)).toEqual(["chat", "chat", "chat"]);
  await expect(page.locator("#list .tk-row")).toHaveCount(0);
  await expect(page.locator("#list .fbar .cnt")).toHaveText("3 matches");
  await expect(page.locator("#status-steady .pos")).toHaveText("Chat 1/3");
  // The search language is not asked about the kinds.
  expect((await callsTo(page, "filter_messages")).map((a) => a.query)).not.toContain("is:chat");

  // A thread by what its row shows, or by its author.
  await filter("is:chat release");
  await expect.poll(() => timelineKinds(page)).toEqual(["chat"]);
  await filter("f:lukas");
  await expect.poll(() => timelineKinds(page)).toEqual(["chat"]);
  await expect(page.locator("#list .rows .row.chat-row .name")).toHaveText("Lukas Meier");

  // A reading item by its subject; the mail has nothing that matches.
  await filter("rust");
  await expect.poll(() => timelineKinds(page)).toEqual(["reading"]);
  await expect(page.locator("#status-steady .pos")).toHaveText("Reading 1/1");

  // is:mail keeps the inboxes whole and puts the rest away.
  await filter("is:mail");
  await expect.poll(() => timelineKinds(page)).toEqual(["mail", "mail", "mail", "mail", "mail", "mail"]);

  // Esc takes the filter off, and everything is back.
  await page.locator("#list").focus();
  await page.keyboard.press("F");
  await page.keyboard.press("Escape");
  await expect.poll(async () => (await timelineKinds(page)).length).toBe(11);
  await expect(page.locator("#list .tk-row")).toHaveCount(3);
  await page.evaluate(() => localStorage.clear());
});

test("stacked, the filter narrows Today's chat and reading sections too", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1&reading=1");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
  for (const key of ["z", "w", "z", "r"]) await page.keyboard.press(key);
  await page.locator("#list > .rd-section .rd-row").first().waitFor();
  await page.keyboard.press("F");
  await page.locator("#list .fbar input").fill("is:task is:chat");
  await page.keyboard.press("Enter");
  await expect(page.locator("#list > .ch-section .ch-row")).toHaveCount(3);
  await expect(page.locator("#list > .rd-section .rd-row")).toHaveCount(0);
  await expect(page.locator("#list .mail-scroll .rows .row")).toHaveCount(0);
  await expect(page.locator("#list .tk-row")).toHaveCount(3);
  await page.evaluate(() => localStorage.clear());
});

test("on the timeline a selection runs over every kind, and a acts on each kind's rows", async ({ page }) => {
  await openMixed(page);
  const status = page.locator("#status");
  // From the third row: Work's second message, Private's second, Lukas's thread.
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  await page.keyboard.press("Shift+J");
  await page.keyboard.press("Shift+J");
  await expect(status).toContainText("3 selected · 2 messages, 1 thread");
  await expect(page.locator("#list .rows .row.selected")).toHaveCount(3);
  await expect(page.locator("#list .rows .row.cursor")).toHaveClass(/chat-row/);

  // With the cursor on the thread, a still archives the messages too.
  await page.keyboard.press("a");
  await expect.poll(() => callsTo(page, "chat_done")).toEqual([expect.objectContaining({ rid: "r-lukas" })]);
  await expect
    .poll(async () => (await callsTo(page, "move_messages")).map((m) => [m.account, m.folder, m.target, m.uids]))
    .toEqual([["Work", "INBOX", "Archive", [1]], ["Private", "INBOX", "Archive", [1]]]);
  await expect(page.locator("#list .rows .row.selected")).toHaveCount(0);
  // The cursor goes on to the row after the last one taken away.
  await expect(page.locator("#status-steady .pos")).toHaveText("Mail 3/8");
  await expect(page.locator("#list .rows .row.cursor")).toContainText("Message #2");
  // One action, one undo, whichever kinds it took: the group closed last is
  // the one opened first, around every kind's part.
  await expect
    .poll(async () => {
      const marks = (await callsTo(page, "end_undo_group")).map((g) => g.mark as number);
      return marks.length > 1 && marks.at(-1) === Math.min(...marks);
    })
    .toBe(true);
  await page.evaluate(() => localStorage.clear());
});

test("Space picks rows of several kinds, Esc lets them all go, and a reading item goes to its archive", async ({ page }) => {
  await openMixed(page);
  const status = page.locator("#status");
  // The first reading item, at nine, and the thread after it.
  for (let i = 0; i < 8; i++) await page.keyboard.press("j");
  await expect(page.locator("#status-steady .pos")).toHaveText("Reading 9/11");
  await page.keyboard.press(" ");
  await page.keyboard.press(" ");
  await expect(status).toContainText("2 selected · 1 thread, 1 reading item");
  // Marking read goes to each kind its own way, and keeps the selection.
  await page.keyboard.press(":");
  await page.keyboard.type("toggle-read");
  await page.keyboard.press("Enter");
  await expect.poll(async () => (await callsTo(page, "mark_read")).map((m) => [m.folder, m.uids])).toEqual([["INBOX.Reading", [500]]]);
  await expect.poll(async () => (await callsTo(page, "chat_set_read")).map((m) => m.rid)).toEqual(["r-sup"]);
  await expect(page.locator("#list .rows .row.selected")).toHaveCount(2);
  await page.keyboard.press("Escape");
  await expect(status).toContainText("selection cleared");
  await expect(page.locator("#list .rows .row.selected")).toHaveCount(0);

  // ⌘-click picks them out again; a archives the item and is done for the thread.
  const rows = page.locator("#list .rows .row");
  await rows.filter({ hasText: "Hacker Newsletter" }).click({ modifiers: ["Meta"] });
  await rows.filter({ hasText: "Daniel Rossi" }).click({ modifiers: ["Meta"] });
  await expect(page.locator("#list .rows .row.selected")).toHaveCount(2);
  await page.locator("#list").focus();
  await page.keyboard.press("a");
  await expect
    .poll(async () => (await callsTo(page, "move_messages")).map((m) => [m.folder, m.target]))
    .toEqual([["INBOX.Reading", "INBOX.Reading Archive"]]);
  await expect.poll(() => callsTo(page, "chat_done")).toEqual([expect.objectContaining({ rid: "r-sup" })]);
  await page.evaluate(() => localStorage.clear());
});

/** Choose `command` in the context menu the shell would have shown. */
async function chooseInMenu(page: Page, command: string): Promise<void> {
  await page.evaluate((command) => {
    const w = window as any;
    w[w.__listeners["context-menu-command"]]({ event: "context-menu-command", id: 0, payload: command });
  }, command);
}

test("zq mutes the thread's room: it leaves Waiting, its room still counts it in grey, and ⌘Z unmutes", async ({ page }) => {
  await openWaiting(page);
  await page.keyboard.press("G");
  await expect(page.locator("#list .chat-row.cursor .room")).toHaveText("#support");
  await page.keyboard.press("z");
  await page.keyboard.press("q");
  const picker = page.locator(".dp.picker");
  await expect(picker.locator("li")).toHaveText([/For 1 hour/, /For 4 hours/, /Until tomorrow/, /Until I unmute/]);
  const before = Date.now();
  await picker.locator("li", { hasText: "For 1 hour" }).click();

  await expect(page.locator("#list .chat-row")).toHaveCount(2);
  const [call] = await callsTo(page, "chat_mute");
  expect(call).toMatchObject({ rid: "r-sup", undo: "mute #support" });
  expect(call.ends - before).toBeGreaterThanOrEqual(3_600_000 - 1000);
  expect(call.ends - before).toBeLessThan(3_600_000 + 60_000);
  await expect(page.locator("#status-msg, #status")).toContainText("Muted #support until");
  await expect(page.locator("#folders .sb-row", { hasText: "Waiting" }).locator(".sb-badge")).toHaveText("2");
  const channels = page.locator("#folders .sb-row", { hasText: "Channels" });
  await expect(channels.locator(".sb-badge")).toHaveText("1");
  await channels.locator("[data-fold]").click();
  const support = page.locator("#folders .sb-row", { hasText: "support" });
  await expect(support.locator(".sb-muted")).toHaveAttribute("title", /^Muted until \d\d:\d\d$/);
  await expect(support.locator(".sb-badge.quiet")).toHaveText("1");

  // In the room itself the thread still waits, dimmed, and the header says the room is muted.
  await support.click();
  await expect(page.locator("#list .chat-row.muted")).toHaveCount(1);
  await expect(page.locator("#reader .cv-muted")).toContainText("Muted until");

  await page.keyboard.press("Escape");
  await page.keyboard.press("Meta+z");
  await expect.poll(() => callsTo(page, "chat_unmute")).toHaveLength(1);
  await expect(support.locator(".sb-muted")).toHaveCount(0);
  await expect(support.locator(".sb-badge.unread")).toHaveText("1");
});

test("a muted room's thread that names me still waits, and the room's menu mutes and unmutes", async ({ page }) => {
  await openWaiting(page);
  const channels = page.locator("#folders .sb-row", { hasText: "Channels" });
  await channels.locator("[data-fold]").click();
  const dev = page.locator("#folders .sb-row", { hasText: "orb-dev" });
  await dev.click({ button: "right" });
  await expect.poll(() => callsTo(page, "open_chat_room_menu")).toHaveLength(1);
  expect((await callsTo(page, "open_chat_room_menu"))[0].disabled).toEqual(["chat-unmute-room"]);
  await chooseInMenu(page, "chat-mute-room forever");

  await expect.poll(() => callsTo(page, "chat_mute")).toEqual([{ rid: "r-dev", ends: null, undo: "mute #orb-dev" }]);
  await expect(dev.locator(".sb-muted")).toHaveAttribute("title", "Muted until you unmute it");
  // Sara's message names me: the thread keeps its place in Waiting, and the room's count its colour.
  await expect(page.locator("#list .chat-row")).toHaveCount(3);
  await expect(dev.locator(".sb-badge.unread")).toHaveText("1");

  await dev.click({ button: "right" });
  await expect.poll(() => callsTo(page, "open_chat_room_menu")).toHaveLength(2);
  const menu = (await callsTo(page, "open_chat_room_menu"))[1];
  expect(menu.disabled).toEqual([]);
  expect(menu.labels["chat-unmute-room"]).toBe("Unmute Room (muted until you unmute it)");
  await chooseInMenu(page, "chat-unmute-room");
  await expect.poll(() => callsTo(page, "chat_unmute")).toEqual([{ rid: "r-dev", undo: "unmute #orb-dev" }]);
  await expect(dev.locator(".sb-muted")).toHaveCount(0);

  // The room's menu leaves the room clicked, not the thread under the list's cursor.
  await page.evaluate(() => ((window as any).__askAnswer = "Leave"));
  await page.locator("#folders .sb-row", { hasText: "support" }).click({ button: "right" });
  await expect.poll(() => callsTo(page, "open_chat_room_menu")).toHaveLength(3);
  await chooseInMenu(page, "chat-leave-room");
  await expect.poll(() => callsTo(page, "chat_leave")).toMatchObject([{ rid: "r-sup" }]);
});

test("a mute takes 90m or a time of day, Waiting says how many rooms are muted, and zQ unmutes one", async ({ page }) => {
  await openWaiting(page, "chat=1");
  await page.keyboard.press("G");
  await page.keyboard.press("z");
  await page.keyboard.press("q");
  const input = page.locator(".picker .dp-input");
  await input.fill("soon");
  await expect(page.locator(".picker li").first()).toContainText("say 90m");
  await input.fill("17:00");
  await expect(page.locator(".picker li").first()).toHaveText(/Until 17:00/);
  await input.fill("90m");
  await expect(page.locator(".picker li").first()).toHaveText(/For 90m/);
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "chat_mute")).toHaveLength(1);
  const [call] = await callsTo(page, "chat_mute");
  expect(Math.round((call.ends - Date.now()) / 60_000)).toBe(90);

  // Done with the rest: Waiting is empty, but for the muted room, and says so.
  await page.keyboard.press("g");
  await page.keyboard.press("g");
  await page.keyboard.press("a");
  await page.keyboard.press("a");
  await expect(page.locator("#list .chat-row")).toHaveCount(0);
  await expect(page.locator("#list")).toContainText("but those of 1 muted room");

  await page.keyboard.press("z");
  await page.keyboard.press("Q");
  const picker = page.locator(".dp.picker");
  await expect(picker.locator("li")).toHaveCount(1);
  await expect(picker.locator("li")).toContainText("#support");
  await picker.locator("li").click();
  await expect.poll(() => callsTo(page, "chat_unmute")).toHaveLength(1);
  await expect(page.locator("#list .chat-row")).toHaveCount(1);
});

test("a code block shows its code without the language named on its fence", async ({ page }) => {
  await page.goto("/harness.html");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  const html = await page.evaluate(async () => {
    const { formatBody } = await import("/src/chat.ts" as string);
    return [
      formatBody("Run\n```bash\nbin/ruby -S rake check\n```"),
      formatBody("```\nplain\n```"),
      formatBody("```one line```"),
    ];
  });
  expect(html[0]).toContain('<pre class="cm-code">bin/ruby -S rake check</pre>');
  expect(html[1]).toContain('<pre class="cm-code">plain</pre>');
  expect(html[2]).toContain('<pre class="cm-code">one line</pre>');
});

// After a on the timeline the cursor takes the row that came next, whatever
// its kind, as it does in a list of one kind; on the last row, the one before.
for (const { from, gone, lands } of [
  { from: "Mail 4/11", gone: "mail", lands: "Chat 4/10" },
  { from: "Chat 5/11", gone: "chat", lands: "Mail 5/10" },
  { from: "Mail 6/11", gone: "mail", lands: "Chat 6/10" },
  { from: "Mail 8/11", gone: "mail", lands: "Reading 8/10" },
  { from: "Reading 9/11", gone: "reading", lands: "Chat 9/10" },
  { from: "Chat 10/11", gone: "chat", lands: "Reading 10/10" },
  { from: "Reading 11/11", gone: "reading", lands: "Chat 10/10" },
]) {
  test(`a on the timeline's ${from} leaves the cursor on ${lands}`, async ({ page }) => {
    await openMixed(page);
    const position = page.locator("#status-steady .pos");
    const steps = Number(/ (\d+)\//.exec(from)![1]) - 1;
    for (let i = 0; i < steps; i++) await page.keyboard.press("j");
    await expect(position).toHaveText(from);
    await page.keyboard.press("a");
    await expect.poll(async () => (await timelineKinds(page)).length).toBe(10);
    expect((await timelineKinds(page)).filter((k) => k === gone).length).toBe(
      { mail: 5, chat: 2, reading: 1 }[gone],
    );
    await expect(position).toHaveText(lands);
    const kind = lands.split(" ")[0]!.toLowerCase();
    await expect(page.locator("#list .rows .row.cursor")).toHaveClass(new RegExp(`${kind === "mail" ? "" : `${kind}-row`}`));
    await expect(page.locator("#list .rows .row.cursor")).toHaveCount(1);
    await page.evaluate(() => localStorage.clear());
  });
}

chatFocusTests();

test("a thread that starts waiting on a timeline drawn empty is drawn, and the last one leaving says so", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1&empty=1");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
  for (const key of ["z", "w", "z", "x"]) await page.keyboard.press(key);
  const rows = page.locator("#list .rows .row.chat-row");
  await expect(rows).toHaveCount(3);
  const waitOnly = (keys: string[]) =>
    page.evaluate((keys) => {
      for (const key of ["r-lukas\u0000", "r-dev\u0000t-rel", "r-sup\u0000t-login"]) {
        // @ts-expect-error: the harness's own fixture
        if (keys.includes(key)) CHAT_DONE.delete(key);
        // @ts-expect-error: the harness's own fixture
        else CHAT_DONE.set(key, true);
      }
      (window as any).__emit("chat-changed", null);
    }, keys);

  // The last thread leaves: the timeline says nothing waits.
  await waitOnly([]);
  await expect(rows).toHaveCount(0);
  await expect(page.locator("#list .mail-scroll .empty")).toHaveText("Nothing waits");
  // Drawn afresh while empty, as on launch, it has no rows to draw into.
  await page.locator("#folders .sb-row", { hasText: "Today" }).first().click();
  await expect(page.locator("#list .mail-scroll .empty")).toHaveText("Nothing waits");

  // Somebody writes again: the thread is drawn without opening Today again.
  await waitOnly(["r-lukas\u0000"]);
  await expect(rows).toHaveCount(1);
  await expect(page.locator("#list .mail-scroll .empty")).toHaveCount(0);
  await expect(page.locator("#list .mail-sec .n")).toHaveText("1");
  await page.evaluate(() => localStorage.clear());
});
