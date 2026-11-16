import { expect, Page, test } from "@playwright/test";

/**
 * The reader of a conversation, against the harness's threads: in the
 * Archive shown as conversations, `<t0@example.com>` is messages 0, 1 and 2,
 * of which 0 is unread, and the thread's row is the first.
 */

/** The arguments of every call of one command, in order. */
async function callsTo(page: Page, cmd: string): Promise<Record<string, unknown>[]> {
  return page.evaluate(
    (cmd) =>
      ((window as unknown as { __mock: { calls?: [string, Record<string, unknown>][] } }).__mock.calls ?? [])
        .filter(([name]) => name === cmd)
        .map(([, args]) => args),
    cmd,
  );
}

/** The Archive as conversations, the cursor on the first, the list with the keyboard. */
async function openConversations(page: Page, query = ""): Promise<void> {
  await page.goto(`/harness.html${query}`);
  await page.locator("#list .row.cursor").waitFor();
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await expect(page.locator("#list .rows .row.cursor .subject .text")).toHaveText("Message #0");
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("t");
  await expect(page.locator("#list .rows .row.cursor .subject .text")).toHaveText("Thread #0");
  await expect(cards(page)).toHaveCount(3);
}
const cards = (page: Page) => page.locator("#reader .conv .card");
const card = (page: Page, i: number) => cards(page).nth(i);

test("a conversation's row says who wrote last, and the reader shows every message", async ({ page }) => {
  await openConversations(page, "?markread=0");
  const row = page.locator("#list .rows .row.cursor");
  await expect(row.locator(".from .name")).toHaveText("Sender 2");
  await expect(row.locator(".subject .snip")).toHaveText("The last word of thread 0.");
  await expect(row).toHaveClass(/unread/);

  await expect(page.locator("#reader .conv-sub")).toHaveText("3 messages · 1 unread · Sender 0, Sender 1, Sender 2");
  // The unread message and the newest are open, the read one between them a line.
  await expect(card(page, 0)).toHaveClass(/open/);
  await expect(card(page, 0)).toHaveClass(/unread/);
  await expect(card(page, 1)).not.toHaveClass(/open/);
  await expect(card(page, 1).locator(".card-pv")).toHaveText("The opening line of message 1.");
  await expect(card(page, 2)).toHaveClass(/open/);
  // Each open message has a body of its own, as HTML in its frame.
  await expect(card(page, 0).frameLocator("iframe").locator("body")).toContainText("HTML body of message 0");
  await expect(card(page, 2).frameLocator("iframe").locator("body")).toContainText("HTML body of message 2");
  // Read in one call, not one per message.
  expect(await callsTo(page, "thread_members")).toEqual([{ folders: [["Work", "Archive"]], threads: ["<t0@example.com>"] }]);
  // As plain text throughout, at a click.
  await page.locator("#reader .conv-hdrs [data-view=text]").click();
  await expect(card(page, 2).locator(".card-text pre")).toContainText("Body of message 2.");
});

test("the reader's keys walk the conversation, and open and close its messages", async ({ page }) => {
  await openConversations(page, "?markread=0");
  await page.keyboard.press("Enter");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  // The keys start on the first unread message.
  await expect(card(page, 0)).toHaveClass(/focus/);
  await page.keyboard.press("j");
  await expect(card(page, 1)).toHaveClass(/focus/);
  await page.keyboard.press("Enter");
  await expect(card(page, 1)).toHaveClass(/open/);
  // Message 1 has no HTML: its text.
  await expect(card(page, 1).locator(".card-text pre")).toContainText("Body of message 1.");
  await page.keyboard.press("z");
  await page.keyboard.press("a");
  await expect(card(page, 1)).not.toHaveClass(/open/);
  await page.keyboard.press("G");
  await expect(card(page, 2)).toHaveClass(/focus/);
  await page.keyboard.press("j");
  await expect(page.locator("#status")).toContainText("the newest message");
  await page.keyboard.press("g");
  await page.keyboard.press("g");
  await expect(card(page, 0)).toHaveClass(/focus/);
  // Every message open, then every one closed.
  await page.keyboard.press("z");
  await page.keyboard.press("Shift+R");
  await expect(page.locator("#reader .conv .card.open")).toHaveCount(3);
  await page.keyboard.press("z");
  await page.keyboard.press("Shift+M");
  await expect(page.locator("#reader .conv .card.open")).toHaveCount(0);
  // The list's cursor stayed on the conversation throughout; h goes back to it.
  await expect(page.locator("#list .rows .row.cursor .subject .text")).toHaveText("Thread #0");
  await page.keyboard.press("h");
  await expect(page.locator("#list")).toHaveClass(/focused/);
  await page.keyboard.press("j");
  await expect(page.locator("#list .rows .row.cursor .subject .text")).toHaveText("Message #3");
  await expect(page.locator("#reader h1")).toContainText("Message #3");
  await expect(page.locator("#reader .conv")).toHaveCount(0);
});

test("a click on a message's line opens it and puts the reader's keys on it", async ({ page }) => {
  await openConversations(page, "?markread=0");
  await card(page, 1).locator(".card-head").click();
  await expect(card(page, 1)).toHaveClass(/open/);
  await expect(card(page, 1)).toHaveClass(/focus/);
  await card(page, 1).locator(".card-head").click();
  await expect(card(page, 1)).not.toHaveClass(/open/);
});

test("an action on a conversation's row acts on every message of it", async ({ page }) => {
  // The harness's `u` unmarks; `gu` reads.
  await openConversations(page, "?markread=0&bind=gu:toggle-read");
  await page.keyboard.press("d");
  await expect.poll(async () => (await callsTo(page, "stage_action")).map((a) => a.uids)).toEqual([[0, 1, 2]]);
  await expect(page.locator("#list .rows .row.cursor")).toHaveClass(/staged-delete/);
  await expect(page.locator("#reader .conv .card.staged")).toHaveCount(3);
  await page.keyboard.press("Shift+U");
  await expect.poll(async () => (await callsTo(page, "unstage_action")).map((a) => a.uids)).toEqual([[0, 1, 2]]);
  // One unread among them: marking reads them all, the row and the reader with them.
  await page.keyboard.press("g");
  await page.keyboard.press("u");
  await expect.poll(async () => (await callsTo(page, "mark_read")).map((a) => [a.uids, a.read])).toEqual([[[0, 1, 2], true]]);
  await expect(page.locator("#list .rows .row.cursor")).not.toHaveClass(/unread/);
  await expect(page.locator("#reader .conv .card.unread")).toHaveCount(0);
  // From the reader too: it acts on the conversation it shows.
  await page.keyboard.press("Enter");
  await page.keyboard.press("!");
  await expect.poll(async () => (await callsTo(page, "mark_flagged")).map((a) => a.uids)).toEqual([[0, 1, 2]]);
});

test("a reply answers the newest message from the list, the one the keys are on from the reader", async ({ page }) => {
  await openConversations(page, "?markread=0");
  await page.keyboard.press("r");
  await expect.poll(async () => (await callsTo(page, "reply_draft")).map((a) => a.id)).toEqual([2]);
  await page.keyboard.press("Escape");
  await page.locator("#list").focus();
  await page.keyboard.press("Enter");
  await page.keyboard.press("j");
  await page.keyboard.press("r");
  await expect.poll(async () => (await callsTo(page, "reply_draft")).map((a) => a.id)).toEqual([2, 1]);
});

test("dwelling on a conversation reads every unread message of it", async ({ page }) => {
  await openConversations(page, "?markread=300");
  await expect.poll(async () => (await callsTo(page, "mark_read")).map((a) => [a.uids, a.read])).toEqual([[[0], true]]);
  await expect(page.locator("#list .rows .row.cursor")).not.toHaveClass(/unread/);
  await expect(card(page, 0)).not.toHaveClass(/unread/);
  // Still open: reading a message does not put it away.
  await expect(card(page, 0)).toHaveClass(/open/);
});

test("a selection of conversations is counted as conversations, and acted on whole", async ({ page }) => {
  await openConversations(page, "?markread=0");
  await page.keyboard.press(" ");
  await page.keyboard.press(" ");
  await expect(page.locator("#status")).toHaveText("2 conversations selected");
  await page.keyboard.press("d");
  // The conversation and the message after it.
  await expect.poll(async () => (await callsTo(page, "stage_action")).map((a) => a.uids)).toEqual([[0, 1, 2, 3]]);
});

test("t makes one task of a conversation, from its newest message", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&markread=0");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("t");
  await expect(page.locator("#list .mail-scroll .row.cursor")).toContainText("Thread #0");
  await page.keyboard.press("t");
  await expect.poll(async () => (await callsTo(page, "create_tasks_from_messages")).map((a) => a.messages)).toEqual([[2]]);
});

test("za in the list opens a conversation into its messages, each read alone", async ({ page }) => {
  await openConversations(page, "?markread=0");
  await page.keyboard.press("z");
  await page.keyboard.press("a");
  await expect(page.locator("#reader .conv")).toHaveCount(0);
  await expect(page.locator("#reader h1")).toContainText("Message #0");
  await page.keyboard.press("d");
  await expect.poll(async () => (await callsTo(page, "stage_action")).map((a) => a.uids)).toEqual([[0]]);
});

test("the switch beside Today's choice of inbox shows the mail as messages or as conversations", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&markread=0");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  const choice = (on: boolean) => page.locator(`#list .mail-sec [data-threads-choice="${on ? "on" : "off"}"]`);
  await expect(choice(false)).toHaveAttribute("aria-checked", "true");
  // Before the choice of inbox, in the same heading.
  await expect(page.locator("#list .mail-sec .seg").first()).toHaveClass(/threads-switch/);
  await choice(true).click();
  await expect(page.locator("#list .mail-scroll .row.cursor")).toContainText("Thread #0");
  await expect(choice(true)).toHaveAttribute("aria-checked", "true");
  // The keyboard is the list's again, and zt flips the switch back.
  await page.keyboard.press("z");
  await page.keyboard.press("t");
  await expect(choice(false)).toHaveAttribute("aria-checked", "true");
  await expect(page.locator("#list .mail-scroll .row.cursor")).toContainText("Message #0");
});

test("a folder's toolbar has the switch too, and the order says conversations are newest first", async ({ page }) => {
  await page.goto("/harness.html?markread=0");
  await page.locator("#list .row.cursor").waitFor();
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await expect(page.locator("#list .rows .row.cursor .subject .text")).toHaveText("Message #0");
  await page.locator('#toolbar [data-threads-choice="on"]').click();
  await expect(page.locator("#list .rows .row.cursor .subject .text")).toHaveText("Thread #0");
  await expect(page.locator("#toolbar .sortbtn")).toContainText("Newest first");
  await page.locator('#toolbar [data-threads-choice="off"]').click();
  await expect(page.locator("#list .rows .row.cursor .subject .text")).toHaveText("Message #0");
  // A search has no conversations, and no switch.
  await page.locator("#list").focus();
  await page.keyboard.press("/");
  await page.keyboard.type("roadmap");
  await page.keyboard.press("Enter");
  await expect(page.locator("#toolbar [data-threads-choice]")).toHaveCount(0);
});

test("a click in a message's HTML body puts the reader's keys on that message, and ⌘A selects it", async ({ page }) => {
  await openConversations(page);
  await card(page, 2).frameLocator("iframe").locator("body").click();
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await expect(card(page, 2)).toHaveClass(/focus/);
  await expect(card(page, 0)).not.toHaveClass(/focus/);
  // The frame hands the keyboard back, and ⌘A selects that message alone.
  await expect.poll(() => page.evaluate(() => document.activeElement?.tagName)).not.toBe("IFRAME");
  await page.keyboard.press("Meta+a");
  await expect
    .poll(() => card(page, 2).frameLocator("iframe").locator("body").evaluate((b) => b.ownerDocument.getSelection()?.toString() ?? ""))
    .toContain("HTML body of message 2");
});
