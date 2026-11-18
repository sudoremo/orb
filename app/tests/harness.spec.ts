import { expect, Locator, Page, test } from "@playwright/test";
import { stepRedrawTests } from "./stepredraw";

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

/** A locator's box once it has one: a redraw can leave it detached for a moment. */
async function box(locator: Locator): Promise<{ x: number; y: number; width: number; height: number }> {
  let found: Awaited<ReturnType<Locator["boundingBox"]>> = null;
  await expect.poll(async () => (found = await locator.boundingBox()) !== null).toBe(true);
  return found!;
}

/**
 * What a row looks like, in the terms the stylesheet decides: its classes,
 * its text, and the computed colours of the row and of each cell. Computed
 * styles rather than pixels, so the snapshot is the same on every machine
 * whatever its fonts, and still changes when a highlight style, a colour
 * variable or a column's markup does.
 */
async function describeRows(page: Page, selector: string, count: number): Promise<unknown[]> {
  // Looked up and read in one evaluation: the list re-renders when the
  // startup sync ends, and rows found in one round trip may be detached by
  // the next.
  return page.evaluate(
    ([selector, count]) =>
      [...document.querySelectorAll(selector as string)].slice(0, count as number).map((row) => {
        const style = (node: Element) => {
          const s = getComputedStyle(node);
          return {
            color: s.color,
            background: s.backgroundColor,
            weight: s.fontWeight,
            decoration: s.textDecorationLine,
          };
        };
        return {
          classes: row.className,
          selected: row.getAttribute("aria-selected"),
          row: style(row),
          cells: [...row.children].map((cell) => ({
            classes: cell.className,
            text: (cell.textContent ?? "").replace(/\s+/g, " ").trim(),
            ...style(cell),
            icons: [...cell.querySelectorAll("i, svg")]
              .filter((i) => !i.closest("[hidden]"))
              .map((i) => i.getAttribute("class") ?? ""),
          })),
        };
      }),
    [selector, count] as const,
  );
}

/**
 * Wait for the startup sync to have landed: it redraws the list, and an
 * element measured across two round trips before then may be replaced.
 */
async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
}

/** Today opens on its mail; step into its tasks, as `<tab>` does. */
async function intoTasks(page: Page): Promise<void> {
  await page.locator("#list [data-task]").first().waitFor();
  await page.locator("#list").focus();
  await page.keyboard.press("Tab");
  await page.locator("#list.tasks-active").waitFor();
}

// The style snapshots, once per look: Hybrid's under the names they always
// had, Terminal's beside them.
for (const look of ["hybrid", "terminal"]) {
  const suffix = look === "hybrid" ? "" : `-${look}`;

  test(`the message list draws every highlight style as before (${look})`, async ({ page }) => {
    await page.goto(`/harness.html?look=${look}`);
    await page.locator("#list .row.cursor").waitFor();
    // The highlight rules match rows of the large Archive folder.
    await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
    await expect(page.locator("#list .rows .row").nth(11)).toBeVisible();
    await expect(page.locator("#list .rows .row.loading")).toHaveCount(0);
    // Sorted by position, since the pool reuses row elements in any order.
    const rows = await page.$$eval("#list .rows .row", (rows) =>
      rows
        .map((row) => ({ top: parseFloat((row as HTMLElement).style.top || "0"), row }))
        .sort((a, b) => a.top - b.top)
        .map(({ row }, index) => row.setAttribute("data-order", String(index))),
    );
    expect(rows.length).toBeGreaterThan(10);
    const described = [];
    for (let i = 0; i < 12; i++) {
      described.push(...(await describeRows(page, `#list .rows .row[data-order="${i}"]`, 1)));
    }
    expect(JSON.stringify(described, null, 2)).toMatchSnapshot(`message-list${suffix}.json`);
  });

  test(`Today's tasks keep their look (${look})`, async ({ page }) => {
    await page.goto(`/harness.html?tasks=1&look=${look}`);
    await page.locator("#list [data-task]").first().waitFor();
    await settled(page);
    await intoTasks(page);
    const tasks = await describeRows(page, "#list [data-task]", 10);
    expect(JSON.stringify(tasks, null, 2)).toMatchSnapshot(`today-tasks${suffix}.json`);
  });
}

test("the cursor keeps a highlight's colour, tinted until it reads on the cursor fill", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  const urgent = page.locator("#list .rows .row", {
    has: page.locator(".subject .text", { hasText: /^Message #4$/ }),
  });
  await urgent.click();
  await expect(urgent).toHaveClass(/cursor/);
  await expect(page.locator("#list")).toHaveClass(/focused/);
  const [subject, name, fill, plain] = await urgent.evaluate((row) => {
    const probe = document.createElement("i");
    row.parentElement!.append(probe);
    probe.style.color = "var(--accent-fg)";
    const plain = getComputedStyle(probe).color;
    probe.remove();
    return [
      getComputedStyle(row.querySelector(".subject .text")!).color,
      getComputedStyle(row.querySelector(".from .name")!).color,
      getComputedStyle(row).backgroundColor,
      plain,
    ];
  });
  const rgb = (value: string) => value.match(/[\d.]+/g)!.slice(0, 3).map(Number);
  const luminance = (c: number[]) => {
    const [r, g, b] = c.map((v) => {
      const s = v / 255;
      return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  };
  const [light, dark] = [luminance(rgb(subject)), luminance(rgb(fill))].sort((a, b) => b - a);
  expect(subject).not.toBe(plain);
  expect(name).toBe(subject);
  // Red rather than grey: the tint keeps the rule's hue.
  const [r, g, b] = rgb(subject);
  expect(r - Math.max(g, b)).toBeGreaterThan(40);
  expect((light + 0.05) / (dark + 0.05)).toBeGreaterThanOrEqual(3);
});

test("Today in columns shortens a row's chips rather than cutting through its badges", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("l");
  await page.locator("#list.today-columns").waitFor();
  const metas = await page.$$eval("#list .mail-scroll .rows .row .meta", (metas) =>
    metas.map((meta) => {
      // Measured edge by edge: what overflows to the left of a right-aligned
      // box does not count towards its scroll width.
      const area = meta.getBoundingClientRect();
      const badges = [...meta.querySelectorAll(".marks > *, .labels .chip, .tasks, .clip, .flagged")]
        .map((badge) => badge.getBoundingClientRect())
        .filter((badge) => badge.width > 0);
      return {
        chips: meta.querySelectorAll(".labels .chip").length,
        fits: badges.every((badge) => badge.left >= area.left - 0.5 && badge.right <= area.right + 0.5),
      };
    }),
  );
  expect(metas.some((m) => m.chips > 1)).toBe(true);
  expect(metas.filter((m) => !m.fits)).toEqual([]);
});

test("a message row is one line: sender and address, subject and snippet, badges, date", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  const row = (n: number) =>
    page.locator("#list .rows .row", { has: page.locator(".subject .text", { hasText: new RegExp(`^Message #${n}$`) }) });
  await expect(row(0)).toBeVisible();
  // No header row: the order is the toolbar's to show.
  await expect(page.locator("#list .thead")).toHaveCount(0);
  await expect(row(0).locator(".from .name")).toHaveText("Sender 0");
  await expect(row(0).locator(".from .addr")).toHaveText("s0@example.com");
  await expect(row(0).locator(".subject .snip")).toHaveText("The opening line of message 0, as the list shows it.");
  await expect(row(2).locator(".subject .snip")).toHaveText("");
  // A bare address is shown once, as the sender.
  await expect(row(7).locator(".from .name")).toBeHidden();
  await expect(row(7).locator(".from .addr")).toHaveText("s7@example.com");
  // Every cell sits on one line of the row's height.
  const heights = await row(0).evaluate((r) =>
    [...r.children].map((c) => (c as HTMLElement).offsetHeight <= r.clientHeight),
  );
  expect(heights.every(Boolean)).toBe(true);
  // A folder with more below fades out at the bottom; the end of it does not.
  await expect(page.locator("#list")).toHaveClass(/\bmore\b/);
  await page.keyboard.press("G");
  await expect(page.locator("#list")).not.toHaveClass(/\bmore\b/);
});

test("a sender whose name is another address is marked, in the list and in the reader", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  const danger = await page.evaluate(() =>
    getComputedStyle(document.documentElement).getPropertyValue("--danger").trim(),
  );
  expect(danger).not.toBe("");
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await expect(page.locator("#list .rows .row.loading")).toHaveCount(0);
  const row = page.locator('#list .rows .row[data-msg="9"]');
  await expect(row).toHaveClass(/\bspoofed\b/);
  await expect(row.locator(".spw")).toBeVisible();
  await expect(row.locator(".name")).toHaveText("service@paypal.com");
  await expect(row.locator(".addr")).toHaveText("security@paypa1-support.ru");
  await expect(row.locator(".addr")).toHaveCSS("color", "rgb(241, 121, 114)");
  // Its neighbours are not.
  await expect(page.locator('#list .rows .row[data-msg="8"] .spw')).toBeHidden();
  await row.click();
  await expect(page.locator("#reader h1")).toHaveText("Message #9");
  const from = page.locator('#reader .hdr[data-k="From"]');
  await expect(from.locator(".spw")).toBeVisible();
  await expect(from.locator(".addr.spoof")).toHaveText("security@paypa1-support.ru");
});

test("moving the cursor with j or k drops the selection, of messages and of tasks", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await expect(page.locator("#list .rows .row.loading")).toHaveCount(0);
  await settled(page);
  // The folder takes the keyboard once it is open; until then the cursor
  // keys move nothing, and Space would select without stepping on.
  await expect(page.locator("#list")).toHaveClass(/\bfocused\b/);
  await page.locator("#list").focus();
  const selected = page.locator("#list .rows .row.selected");
  await page.keyboard.press("Space");
  await page.keyboard.press("J");
  await expect(selected).toHaveCount(3);
  await page.keyboard.press("j");
  await expect(selected).toHaveCount(0);
  await expect(page.locator("#status")).toHaveText("selection cleared");
  await page.keyboard.press("J");
  await expect(selected).toHaveCount(2);
  await page.keyboard.press("k");
  await expect(selected).toHaveCount(0);
  await page.keyboard.press("J");
  await page.keyboard.press("G");
  await expect(selected).toHaveCount(0);

  await page.locator("#folders .sb-row", { hasText: "Today" }).first().click();
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await intoTasks(page);
  await page.keyboard.press("Space");
  await expect(page.locator("#list .tk-row.selected")).toHaveCount(1);
  await page.keyboard.press("j");
  await expect(page.locator("#list .tk-row.selected")).toHaveCount(0);
});

test("a task's linked mail far down its folder opens there, not as a conversation", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("open-task-link 1 <asked@example.com>");
  await page.keyboard.press("Enter");
  await expect(cursorSubject(page)).toHaveText("Message #5000");
  await expect(page.locator("#reader h1")).toContainText("Message #5000");
  await expect(page.locator("#folders .search-row")).toHaveCount(0);
  await expect(page.locator('#folders .sb-row[title="Archive"]')).toHaveClass(/\bon\b|\bcursor\b/);
  await expect(page.locator("#status")).toContainText("in Archive · Work");
  await expect(page.locator("#status")).toContainText("goes back");

  // Ctrl-O goes back to Today, where the jump was made from.
  await page.keyboard.press("Control+o");
  await expect(page.locator("#folders .sb-row.cursor")).toHaveClass(/today/);
  await page.locator("#list [data-task]").first().waitFor();
});

/** Have a task's linked mail resolve to a message of the stub's. */
async function linkTo(page: Page, id: number, account: string, folder: string): Promise<void> {
  await page.evaluate(
    ([id, account, folder]) => {
      (window as unknown as { __mock: { linked?: unknown } }).__mock.linked = {
        local_id: id, account_name: account, folder_name: folder,
        subject: `Message #${id}`, from: `Sender ${id}`, date: "2026-01-01T00:00:00Z",
      };
    },
    [id, account, folder] as const,
  );
}

async function openTaskLink(page: Page): Promise<void> {
  await page.keyboard.press(":");
  await page.keyboard.type("open-task-link 1 <asked@example.com>");
  await page.keyboard.press("Enter");
}

test("a task's linked mail that Today lists is shown in Today, lit, and Ctrl-O goes back to the task", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await intoTasks(page);
  await linkTo(page, 4, "Work", "INBOX");
  await openTaskLink(page);

  await expect(cursorSubject(page)).toHaveText("Message #4");
  await expect(page.locator("#folders .sb-row.cursor")).toHaveClass(/today/);
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await expect(page.locator("#list .rows .row.cursor")).toHaveClass(/landed/);
  await expect(page.locator("#list .rows .row.cursor")).not.toHaveClass(/landed/, { timeout: 3000 });
  await expect(page.locator("#reader h1")).toContainText("Message #4");

  await page.keyboard.press("Control+o");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("Control+o");
  await expect(page.locator("#status")).toHaveText("no jump to go back from");
});

test("a linked message a filter in Today hides opens in its folder, and Ctrl-O brings back Today and the filter", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("F");
  await page.keyboard.type("renew");
  await expect(page.locator("#list .mail-scroll .empty")).toHaveText("No mail matches");
  await page.locator("#list .tk-section .tk-row").first().click();
  await linkTo(page, 1, "Work", "INBOX");
  await openTaskLink(page);

  await expect(cursorSubject(page)).toHaveText("Message #1");
  await expect(page.locator("#folders .sb-row.cursor")).not.toHaveClass(/today/);
  await expect(page.locator("#status")).toContainText("in INBOX · Work");

  await page.keyboard.press("Control+o");
  await expect(page.locator("#folders .sb-row.cursor")).toHaveClass(/today/);
  await expect(page.locator("#list .fbar input")).toHaveValue("renew");
  await expect(page.locator("#list .tk-section .tk-row .tk-title")).toHaveText(["Renew the domain"]);
});

test("a linked message inside a conversation of Today's threaded mail opens the conversation where it stands", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("t");
  await expect(page.locator("#list .mail-scroll .row.cursor")).toContainText("Thread #0");
  await intoTasks(page);
  // Past the first page of conversations, inside one of them.
  await linkTo(page, 501, "Work", "INBOX");
  await openTaskLink(page);

  await expect(cursorSubject(page)).toHaveText("Re: Thread #500");
  await expect(page.locator("#folders .sb-row.cursor")).toHaveClass(/today/);
  await expect(page.locator("#folders .search-row")).toHaveCount(0);
});

test("a conversation is shown by its subject, not as a query", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator("#list").focus();
  await page.keyboard.press("g");
  await page.keyboard.press("c");
  const row = page.locator("#folders .search-row");
  await expect(row).toHaveClass(/conversation-row/);
  await expect(row.locator(".sb-label")).toHaveText("Message #0");
});

test("a replied-to or forwarded message carries its mark, unless it is unread", async ({ page }) => {
  for (const look of ["hybrid", "terminal"]) {
    await page.goto(`/harness.html?look=${look}`);
    await page.locator("#list .row.cursor").waitFor();
    await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
    await expect(page.locator("#list .rows .row.loading")).toHaveCount(0);
    const mark = (n: number) => page.locator(`#list .rows .row[data-msg="${n}"] .status .mark`).first();
    await expect(mark(1)).toHaveAttribute("data-g", "r");
    await expect(mark(1)).toBeVisible();
    await expect(mark(3)).toHaveAttribute("data-g", "N");
    await expect(mark(4)).toHaveAttribute("data-g", "f");
    await expect(mark(4)).toBeVisible();
    await expect(mark(5)).toHaveAttribute("data-g", "");
  }
});

test("a task's linked mail says whether it was answered, and when", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator('#list [data-task="0"]').click();
  const links = page.locator("#reader .td-link");
  await expect(links).toHaveCount(2);
  await expect(links.nth(0).locator(".rs.ok")).toHaveText(/^Replied (6 Aug|6 Aug 2026)$/);
  await expect(links.nth(1).locator(".rs.no")).toHaveText("No reply");
});

test("every command says what it does", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  const missing = await page.evaluate(async () => {
    const { COMMANDS } = await import("/src/commands.ts");
    const { COMMAND_DOCS } = await import("/src/commanddocs.ts");
    return {
      undocumented: Object.keys(COMMANDS).filter((name) => !COMMAND_DOCS[name]),
      unknown: Object.keys(COMMAND_DOCS).filter((name) => !COMMANDS[name]),
    };
  });
  expect(missing).toEqual({ undocumented: [], unknown: [] });
});

test("? lays every key over the window, finds one by typing and runs it", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("?");
  const help = page.locator(".help");
  await expect(help).toBeVisible();
  await expect(help.locator(".help-tabs [aria-selected=true]")).toHaveText("Here · Mail list");
  const row = (does: string) => help.locator(".hk-row", { has: page.getByText(does, { exact: true }) });
  await expect(row("Move down a row").locator("kbd")).toHaveText(["j", "↓"]);

  // Words find what a key does, from the start of a word: archive, not search.
  await page.keyboard.type("arch");
  await expect(row("File in the account's archive folder; from the reading list into its archive, and back")).toBeVisible();
  await expect(row("Open the search field")).toHaveCount(0);

  // A key says what it does, and Enter runs it where the keyboard was.
  await page.keyboard.press("Escape");
  await expect(help).toBeVisible();
  await page.keyboard.type("j");
  await expect(help.locator(".hk-row.on")).toContainText("Move down a row");
  await page.keyboard.press("Enter");
  await expect(help).toHaveCount(0);
  await expect(page.locator("#reader")).toContainText("Message #1");

  await page.keyboard.press("?");
  await page.keyboard.press("?");
  await expect(help).toHaveCount(0);
});

test("? opens on the keys of a task view, which the reader could not show", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("open-task-view upcoming");
  await page.keyboard.press("Enter");
  await expect(page.locator("#folders .sb-row.cursor")).toHaveText("Upcoming");
  await page.keyboard.press("?");
  const help = page.locator(".help");
  await expect(help.locator(".help-tabs [aria-selected=true]")).toHaveText("Here · Task list");
  await page.keyboard.type("d");
  await expect(help.locator(".hk-row")).toHaveText([/Move the task to the trash/]);
  // Every row fits the overlay's own scrolling body, not the window.
  await page.keyboard.press("Escape");
  const body = help.locator(".help-body");
  expect(await body.evaluate((b) => b.scrollHeight > b.clientHeight && getComputedStyle(b).overflowY === "auto")).toBe(true);
});

test("the command line completes commands, with what they do and their keys", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("arch");
  const rows = page.locator("#completions .ci");
  await expect(rows.first()).toHaveAttribute("data-command", "archive");
  await expect(rows.first().locator(".cd")).toHaveText("File in the account's archive folder; from the reading list into its archive, and back");
  expect((await rows.first().locator(".cn b").allTextContents()).join("")).toBe("arch");
  // Tab fills in the first, again the next, Shift-Tab goes back.
  await page.keyboard.press("Escape");
  await page.keyboard.press(":");
  await page.keyboard.type("sort");
  await page.keyboard.press("Tab");
  await expect(page.locator("#status")).toHaveText(":sort-by");
  await page.keyboard.press("Tab");
  await expect(page.locator("#status")).toHaveText(":sort-menu");
  await page.keyboard.press("Shift+Tab");
  await expect(page.locator("#status")).toHaveText(":sort-by");
  await expect(page.locator('#completions .ci.on')).toHaveAttribute("data-command", "sort-by");
  // After the name, the line shows what it takes.
  await page.keyboard.type(" ");
  await expect(rows).toHaveCount(1);
  await expect(rows.first().locator(".arg")).toHaveText("date|from|subject|relevance [asc|desc]");
  await page.keyboard.press("Escape");
  await expect(page.locator("#completions")).toHaveCount(0);
});

test("the command line matches loosely and walks its completions with the arrows and ⌃J/⌃K", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  const rows = page.locator("#completions .ci");
  const status = page.locator("#status");
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("clsl");
  await expect(rows.first()).toHaveAttribute("data-command", "clear-selection");
  expect((await rows.first().locator(".cn b").allTextContents()).join("")).toBe("clsl");

  await page.keyboard.press("ArrowDown");
  await expect(status).toHaveText(":clear-selection");
  await page.keyboard.press("Control+j");
  const second = await rows.nth(1).getAttribute("data-command");
  await expect(status).toHaveText(`:${second}`);
  await page.keyboard.press("Control+k");
  await expect(status).toHaveText(":clear-selection");
  await page.keyboard.press("ArrowUp");
  await expect(page.locator("#completions .ci.on")).toHaveAttribute("data-command", await rows.last().getAttribute("data-command") ?? "");

  // On an empty line the arrows are the history.
  await page.keyboard.press("Escape");
  await page.keyboard.press(":");
  await page.keyboard.type("sort-by date asc");
  await page.keyboard.press("Enter");
  // Keys typed ahead wait for the command before them; this one waits here.
  await expect(status).toHaveText("sort: date asc");
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await expect(rows).toHaveCount(0);
  await page.keyboard.press("ArrowUp");
  await expect(status).toHaveText(":sort-by date asc");
  await page.keyboard.press("Escape");
});

test("sort-by takes a direction, and keeps it when asked again", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  const run = async (command: string) => {
    await page.locator("#list").focus();
    await page.keyboard.press(":");
    await page.keyboard.type(command);
    await page.keyboard.press("Enter");
  };
  await run("sort-by subject desc");
  await expect(page.locator("#status")).toHaveText("sort: subject desc");
  await run("sort-by subject desc");
  await expect(page.locator("#status")).toHaveText("sort: subject desc");
  await run("sort-by subject asc");
  await expect(page.locator("#status")).toHaveText("sort: subject asc");
  // Without one, the field it has turns round, as before.
  await run("sort-by subject");
  await expect(page.locator("#status")).toHaveText("sort: subject desc");
  await run("sort-by subject sideways");
  await expect(page.locator("#status")).toHaveText("sort-by: say asc or desc, not 'sideways'");
});

test("the tasks window shows tasks only: views and tags, no mail, no sync", async ({ page }) => {
  await page.goto("/harness.html?window=tasks&tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await expect(page.locator("html")).toHaveAttribute("data-window", "tasks");
  await expect(page.locator("#folders .sb-row .sb-label")).toHaveText([
    "Today", "Upcoming", "Someday", "Completed", "Archived", "Trash", "#work", "#home",
  ]);
  await expect(page.locator("#list .mail-scroll")).toHaveCount(0);
  await expect(page.locator("#list .tk-section.full")).toHaveCount(1);
  const calls = await page.evaluate(() =>
    (window as unknown as { __mock: { calls: [string, unknown][] } }).__mock.calls.map(([cmd]) => cmd),
  );
  expect(calls).not.toContain("sync_all");
  expect(calls).not.toContain("messages");
});

test("the tasks window puts the detail beside the list when wide, below when narrow", async ({ page }) => {
  const placement = () =>
    page.evaluate(() => {
      const list = document.getElementById("list")!.getBoundingClientRect();
      const reader = document.getElementById("reader")!.getBoundingClientRect();
      return reader.left >= list.right - 1 ? "beside" : reader.top >= list.bottom - 1 ? "below" : "overlapping";
    });
  await page.goto("/harness.html?window=tasks&tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  expect(await placement()).toBe("beside");
  await page.setViewportSize({ width: 800, height: 700 });
  await expect.poll(placement).toBe("below");
});

test("the line between list and detail in the tasks window is dragged sideways, and kept", async ({ page }) => {
  await page.goto("/harness.html?window=tasks&tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  const listWidth = () => page.evaluate(() => Math.round(document.getElementById("list")!.getBoundingClientRect().width));
  const line = (await box(page.locator("#hsplit")));
  const start = await listWidth();
  await page.mouse.move(line.x + line.width / 2, line.y + 200);
  await page.mouse.down();
  await page.mouse.move(line.x + 150, line.y + 200, { steps: 6 });
  await page.mouse.up();
  await expect.poll(listWidth).toBeGreaterThan(start + 100);
  const dragged = await listWidth();
  await page.reload();
  await page.locator("#list [data-task]").first().waitFor();
  expect(await listWidth()).toBe(dragged);
  await page.locator("#hsplit").dblclick();
  await expect.poll(listWidth).toBe(start);
  await page.evaluate(() => localStorage.clear());
});

test("a tag in the tasks window filters the view, and again lifts the filter", async ({ page }) => {
  await page.goto("/harness.html?window=tasks&tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  const titles = page.locator("#list [data-task] .tk-title");
  await expect(titles).toHaveCount(3);
  await page.locator("#folders .sb-row", { hasText: "#work" }).click();
  await expect(titles).toHaveText(["Buy groceries"]);
  await expect(page.locator("#folders .sb-row.cursor")).toHaveText("#work");
  await page.locator("#folders .sb-row", { hasText: "#work" }).click();
  await expect(titles).toHaveCount(3);
});

test("a linked message opened in the tasks window opens in the mailbox", async ({ page }) => {
  await page.goto("/harness.html?window=tasks&tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await page.locator('#list [data-task="0"]').click();
  await page.locator("#reader .td-link .td-open").first().click();
  await expect
    .poll(() =>
      page.evaluate(() =>
        (window as unknown as { __mock: { calls: [string, Record<string, unknown>][] } }).__mock.calls
          .filter(([cmd]) => cmd === "plugin:event|emit_to" || cmd === "show_mailbox")
          .map(([cmd, args]) => (cmd === "show_mailbox" ? cmd : `${(args.target as { label?: string })?.label ?? args.target} ${args.event} ${args.payload}`)),
      ),
    )
    .toEqual(["main context-menu-command open-task-link 1 <asked@example.com>", "show_mailbox"]);
});

test("the context menus show the keys bound to their entries", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const menu = (cmd: string) =>
    page.evaluate(
      (cmd) =>
        (window as unknown as { __calls: { cmd: string; args: Record<string, Record<string, string>> }[] }).__calls
          .filter((c) => c.cmd === cmd)
          .at(-1)?.args,
      cmd,
    );
  await page.locator('#list [data-task="0"]').click({ button: "right" });
  await expect.poll(async () => (await menu("open_task_menu"))?.accelerators?.["work-on"]).toBe("W");
  const tasks = (await menu("open_task_menu"))!;
  expect(tasks.accelerators!["complete-task"]).toBe("X");
  expect(tasks.hints!["open-link"]).toBe("gx");

  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="1"]').click({ button: "right" });
  await expect.poll(async () => (await menu("open_message_menu"))?.accelerators?.["archive"]).toBe("A");
});

test("a right-click keeps the list where it is, for tasks and for mail", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&manytasks=40");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const tasks = page.locator("#list .tk-list");
  await tasks.evaluate((list) => (list.scrollTop = 300));
  const row = page.locator('#list [data-task="13"]');
  await row.click({ button: "right" });
  await expect(row).toHaveClass(/\bcursor\b/);
  expect(await tasks.evaluate((list) => list.scrollTop)).toBe(300);
  // A redraw of the tasks, as completing one from the menu makes, keeps it too.
  await page.evaluate(async () => (await import("/src/taskview.ts")).renderTaskList());
  expect(await tasks.evaluate((list) => list.scrollTop)).toBe(300);

  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await expect(page.locator("#list .rows .row.loading")).toHaveCount(0);
  // Scrolled once the folder has landed: its first page brings the cursor
  // into view, which would undo an earlier scroll.
  const mail = page.locator('#list .rows .row[data-msg="130"]');
  await expect
    .poll(async () => {
      await page.locator("#list").evaluate((list) => (list.scrollTop = 3000));
      return mail.count();
    })
    .toBe(1);
  await mail.click({ button: "right" });
  await expect(mail).toHaveClass(/\bcursor\b/);
  expect(await page.locator("#list").evaluate((list) => list.scrollTop)).toBe(3000);
});

test("Today's tasks and mail stay where they are scrolled when the whole list is drawn again", async ({ page }) => {
  // Short enough for Today's six messages to scroll.
  await page.setViewportSize({ width: 1200, height: 520 });
  await page.goto("/harness.html?tasks=1&manytasks=40&reading=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const tasks = page.locator("#list .tk-list");
  const mail = page.locator("#list .mail-scroll");
  const where = async () => [await tasks.evaluate((e) => e.scrollTop), await mail.evaluate((e) => e.scrollTop)];
  await page.locator('#list [data-task="13"]').click();
  await tasks.evaluate((e) => (e.scrollTop = 300));
  await mail.evaluate((e) => (e.scrollTop = e.scrollHeight));
  const scrolled = await where();
  expect(scrolled[1]).toBeGreaterThan(0);

  // Opening the task and coming back, a sync, and the reading put away and
  // back each draw the whole list again.
  for (const [name, step] of [
    ["enter", () => page.keyboard.press("Enter")],
    ["h", () => page.keyboard.press("h")],
    ["ctrl-h", () => page.keyboard.press("Control+h")],
    ["renderList", () => page.evaluate(async () => (await import("/src/list.ts")).renderList())],
    ["reading", () =>
      page.evaluate(async () => {
        const { runCommand } = await import("/src/commands.ts");
        await runCommand("toggle-today-reading", 1);
        await runCommand("toggle-today-reading", 1);
      })],
  ] as const) {
    await step();
    await expect.poll(where, { message: name }).toEqual(scrolled);
  }
});

test("a task drawn again keeps where its detail is scrolled", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&longnotes=1");
  await intoTasks(page);
  await page.locator('#list [data-task="0"]').click();
  await page.keyboard.press("Enter");
  const body = page.locator("#reader .tk-detail-body");
  await expect(body).toContainText("Line 80 of the notes");
  // Until it takes: the pane may still be settling into its size.
  await expect.poll(() => body.evaluate((e) => ((e.scrollTop = 400), e.scrollTop))).toBe(400);
  await page.evaluate(async () => (await import("/src/reader.ts")).renderReader());
  expect(await body.evaluate((e) => e.scrollTop)).toBe(400);
});

test("Upcoming lists today's tasks too, under a heading per day, then per month", async ({ page }) => {
  await page.clock.setFixedTime(new Date("2026-08-07T10:00:00"));
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("open-task-view upcoming");
  await page.keyboard.press("Enter");
  await expect(page.locator("#list .tk-day")).toHaveText(["Today", "13Thursday", "August"]);
  await expect(page.locator("#list [data-task] .tk-title")).toHaveText([
    "Renew the domain", "Buy groceries", "Call the plumber", "Book the flights", "Renew the passport",
  ]);
});

test("clear-completed hides today's completed tasks from Today, kept on reload", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const titles = page.locator("#list [data-task] .tk-title");
  await page.locator('#list [data-task="0"] [data-task-toggle]').click();
  await expect(page.locator("#list .tk-row.completed .tk-title")).toHaveText("Buy groceries");
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("clear-completed");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status")).toHaveText("1 task cleared from Today · Completed still has them");
  await expect(page.locator("#list .tk-row.completed")).toHaveCount(0);
  await expect(titles).not.toContainText(["Buy groceries"]);

  await page.reload();
  await page.locator("#list [data-task]").first().waitFor();
  await expect(page.locator("#list .tk-row.completed")).toHaveCount(0);
  await page.evaluate(() => localStorage.clear());
});

test("tasks.completed_first_in_today lists Today's completed tasks above the open ones", async ({ page }) => {
  for (const first of [false, true]) {
    await page.goto(`/harness.html?tasks=1${first ? "&completedfirst=1" : ""}`);
    await page.locator("#list [data-task]").first().waitFor();
    await settled(page);
    const rows = page.locator("#list [data-task]");
    const title = (await rows.last().locator(".tk-title").textContent())!;
    await rows.last().locator("[data-task-toggle]").click();
    await expect(page.locator("#list .tk-row.completed .tk-title")).toHaveText(title);
    // In its place in the order unless the config asks otherwise.
    await expect((first ? rows.first() : rows.last()).locator(".tk-title")).toHaveText(title);
    await expect(page.locator("#toolbar [data-completed-first]")).toHaveCount(0);
  }
});

test("with Completed first, an open task moves up past its open neighbour, and not into the completed ones", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&completedfirst=1");
  await intoTasks(page);
  await settled(page);
  const titles = page.locator("#list .tk-row .tk-title");
  await page.locator("#list [data-task]").last().locator("[data-task-toggle]").click();
  await expect(page.locator("#list .tk-row").first()).toHaveClass(/completed/);
  const second = (await titles.nth(1).textContent())!;
  const third = (await titles.nth(2).textContent())!;

  await page.locator("#list").focus();
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  await page.keyboard.press("Meta+ArrowUp");
  await expect(titles.nth(1)).toHaveText(third);
  await expect(titles.nth(2)).toHaveText(second);
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText(third);

  // The top of the open tasks is as far up as an open task goes.
  await page.keyboard.press("Meta+ArrowUp");
  await expect(titles.nth(1)).toHaveText(third);
  await expect(page.locator("#list .tk-row").first()).toHaveClass(/completed/);
  await page.evaluate(() => localStorage.clear());
});

test("a repeating task completed today stays in Today, ticked, until cleared", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&repeat=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const row = page.locator("#list .tk-row", { hasText: "Water the plants" });
  await expect(row).toHaveClass(/\bcompleted\b/);
  await expect(row.locator(".chk")).toHaveClass(/\bon\b/);
  // Ticking it again would complete tomorrow's; it says so instead.
  const calls = () => page.evaluate(() => (window as unknown as { __calls: { cmd: string }[] }).__calls.map((c) => c.cmd));
  await row.locator("[data-task-toggle]").click();
  await expect(page.locator("#status")).toHaveText(/^done for today · back /);
  expect(await calls()).not.toContain("complete_tasks");
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("clear-completed");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status")).toHaveText("1 task cleared from Today · Completed still has them");
  await expect(row).toHaveCount(0);
  await page.evaluate(() => localStorage.clear());
});

test("Shift with Command and a letter is a key of its own", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  const spelled = await page.evaluate(async () => {
    const { canonicalKey, eventKey, spellKey } = await import("/src/keys.ts");
    const key = (init: KeyboardEventInit) => eventKey(new KeyboardEvent("keydown", init));
    return {
      plain: key({ key: "y", metaKey: true }),
      shifted: key({ key: "Y", metaKey: true, shiftKey: true }),
      plus: key({ key: "+", metaKey: true, shiftKey: true }),
      config: canonicalKey("<S-D-y>"),
      shown: spellKey("<d-s-y>"),
      enter: key({ key: "Enter", metaKey: true }),
      shiftedEnter: key({ key: "Enter", metaKey: true, shiftKey: true }),
      configEnter: canonicalKey("<S-D-Enter>"),
      up: key({ key: "ArrowUp", metaKey: true }),
      shownUp: spellKey("<d-up>"),
    };
  });
  expect(spelled).toEqual({
    plain: "<d-y>",
    shifted: "<d-s-y>",
    plus: "<d-+>",
    config: "<d-s-y>",
    shown: "⇧⌘Y",
    enter: "<d-enter>",
    shiftedEnter: "<d-s-enter>",
    configEnter: "<d-s-enter>",
    up: "<d-up>",
    shownUp: "⌘↑",
  });
});

test("a task's steps are a checklist: ticked by a click, added with +, counted on the row", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const row = page.locator('#list [data-task="0"]');
  await expect(row.locator(".m-steps .n")).toHaveText("1/3");
  await row.click();
  const steps = page.locator("#reader .step");
  await expect(steps).toHaveCount(3);
  await expect(page.locator("#reader .step.next .st")).toHaveText("Check the fridge");
  await expect(page.locator("#reader .td-h", { hasText: "Steps" }).locator(".n")).toHaveText("1/3");
  await expect(page.locator("#reader .hint", { hasText: "Step" }).locator("kbd")).toHaveText("+");

  await steps.nth(1).locator(".tk-box").click();
  await expect(page.locator("#reader .step.next .st")).toHaveText("Go to the shop");
  await expect(row.locator(".m-steps .n")).toHaveText("2/3");

  // + opens a step to write in place; Enter keeps it and opens the next.
  await page.locator("#list").focus();
  await page.keyboard.press("+");
  const field = page.locator("#tk-step-input");
  await expect(field).toBeFocused();
  await page.keyboard.type("Pay");
  await page.keyboard.press("Enter");
  await expect(page.locator("#reader .step:not(.adding)")).toHaveCount(4);
  await expect(page.locator("#reader .step:not(.adding)").last().locator(".st")).toHaveText("Pay");
  await expect(field).toBeFocused();
  await expect(field).toHaveValue("");
  await page.keyboard.press("Escape");
  await expect(field).toHaveCount(0);
  await expect(steps).toHaveCount(4);

  await steps.last().hover();
  await steps.last().locator('[data-task-action="remove-step"]').click();
  await expect(steps).toHaveCount(3);
});

test("a task completed long ago is in Archived, not in Completed", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const open = async (view: string) => {
    await page.locator("#list").focus();
    await page.keyboard.press(":");
    await page.keyboard.type(`open-task-view ${view}`);
    await page.keyboard.press("Enter");
  };
  await open("archive");
  await expect(page.locator("#folders .sb-row.cursor")).toHaveText("Archived");
  await expect(page.locator("#list [data-task] .tk-title")).toHaveText(["File the taxes"]);
  await open("completed");
  await expect(page.locator("#list [data-task] .tk-title")).not.toContainText(["File the taxes"]);
});

test("a link in HTML mail navigates the window, for the shell to open, and an in-page one stays", async ({ page }) => {
  // The shell's navigation guard opens the browser and cancels; here the
  // navigation is recorded and cancelled the same way.
  const navigated: string[] = [];
  await page.route("https://example.com/**", (route) => {
    if (route.request().isNavigationRequest() && route.request().frame() === page.mainFrame()) {
      navigated.push(route.request().url());
    }
    void route.abort();
  });
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="2"]').click();
  const body = page.frameLocator("#reader iframe");
  await body.locator("#top").click();
  expect(navigated).toEqual([]);
  expect(page.frames().map((f) => f.url())).toEqual([page.url(), "about:srcdoc#top"]);
  // `target="_blank"` in the mail, which the sandbox would block as a popup.
  await body.locator("#news").click();
  await expect.poll(() => navigated).toEqual(["https://example.com/news"]);
});

for (const scheme of ["dark", "light"] as const) {
  test(`a multi-selection is filled, without an edge, and its text still reads at 4.5:1 (${scheme})`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: scheme });
    await page.goto("/harness.html");
    await page.locator("#list .row.cursor").waitFor();
    await settled(page);
    await page.locator('#folders .sb-row[title="Archive"]').click();
    // The spoofed sender of message 9 is drawn in the danger colour.
    await page.locator('#list .rows .row[data-msg="9"]').click();
    await page.keyboard.press("J");
    await page.keyboard.press("J");
    await expect(page.locator("#list .rows .row.selected")).toHaveCount(3);
    const measured = await page.evaluate(() => {
      // `rgb()` in 0–255, or `color(srgb …)` in 0–1 for a `color-mix`.
      const rgba = (css: string) => {
        const scale = css.startsWith("color(") ? 255 : 1;
        const [r, g, b, a = 1] = css.match(/[\d.]+/g)!.map(Number);
        return { r: r * scale, g: g * scale, b: b * scale, a };
      };
      const over = (top: string, under: string) => {
        const t = rgba(top);
        const u = rgba(under);
        const mix = (x: number, y: number) => x * t.a + y * (1 - t.a);
        return { r: mix(t.r, u.r), g: mix(t.g, u.g), b: mix(t.b, u.b), a: 1 };
      };
      const luminance = ({ r, g, b }: { r: number; g: number; b: number }) => {
        const lin = (c: number) => ((c /= 255) <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
        return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
      };
      let surface: Element | null = document.querySelector("#list .rows");
      while (surface && rgba(getComputedStyle(surface).backgroundColor).a === 0) surface = surface.parentElement;
      const list = getComputedStyle(surface ?? document.body).backgroundColor;
      const rows = [...document.querySelectorAll<HTMLElement>("#list .rows .row.selected:not(.cursor)")];
      return rows.map((row) => {
        const fill = over(getComputedStyle(row).backgroundColor, list);
        const contrasts = [...row.querySelectorAll("*")]
          .filter((node) => (node as HTMLElement).offsetParent !== null)
          .filter((node) => (node.textContent ?? "").trim() !== "" && node.children.length === 0)
          .map((node) => {
            const text = over(getComputedStyle(node).color, `rgb(${fill.r}, ${fill.g}, ${fill.b})`);
            const [hi, lo] = [luminance(text), luminance(fill)].sort((x, y) => y - x);
            return (hi + 0.05) / (lo + 0.05);
          });
        return { edge: getComputedStyle(row).boxShadow, lowest: Math.min(...contrasts) };
      });
    });
    expect(measured).toHaveLength(2);
    for (const row of measured) {
      expect(row.edge).toBe("none");
      expect(row.lowest).toBeGreaterThanOrEqual(4.5);
    }
  });
}

test("⌘A in the reader selects the message text, not every message", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="5"]').click();
  await page.locator("#reader pre").click();
  await page.keyboard.press("Meta+a");
  const text = await page.locator("#reader pre").evaluate((pre) => pre.textContent);
  expect(await page.evaluate(() => window.getSelection()?.toString())).toBe(text);
  await expect(page.locator("#list .rows .row.selected")).toHaveCount(0);
  // A sync that leaves the message as it was leaves the selection too.
  await page.evaluate(() => {
    const w = window as unknown as { __emit: (event: string, payload: unknown) => void };
    w.__emit("folder-changed", ["Work", "Archive"]);
  });
  await page.waitForTimeout(500);
  expect(await page.evaluate(() => window.getSelection()?.toString())).toBe(text);
});

test("⌘A in HTML mail selects the message in its frame and leaves the keyboard with the pane", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="2"]').click();
  await page.locator("#reader iframe").waitFor();
  await page.locator("#reader header").click();
  await page.keyboard.press("Meta+a");
  const frame = page.frameLocator("#reader iframe").locator("body");
  await expect
    .poll(() =>
      frame.evaluate((body) => ({
        focused: body.ownerDocument.hasFocus(),
        selected: body.ownerDocument.getSelection()?.toString() === body.innerText,
      })),
    )
    .toEqual({ focused: false, selected: true });
  await expect(page.locator("#list .rows .row.selected")).toHaveCount(0);
});

test("a click into HTML mail hands the keyboard back and clears a selection elsewhere", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="2"]').click();
  await page.locator("#reader iframe").waitFor();
  await page.locator("#reader header h1").evaluate((h1) => window.getSelection()?.selectAllChildren(h1));
  await page.frameLocator("#reader iframe").locator("body").click();
  await expect.poll(() => page.evaluate(() => document.activeElement?.id)).toBe("list");
  expect(await page.evaluate(() => window.getSelection()?.toString())).toBe("");
  // The next key is a binding again.
  await page.keyboard.press("g");
  await page.keyboard.press("f");
  await expect(page.locator(".dp.picker")).toBeVisible();
});

test("a tracking pixel is taken out of HTML mail, and every link shows where it goes", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="2"]').click();
  const body = page.frameLocator("#reader iframe");
  await expect(body.locator("#news")).toHaveAttribute("title", "https://example.com/news");
  await expect(body.locator("#pixel")).toHaveCount(0);
  // The pixel was the only remote image, so there is nothing to ask about.
  await expect(page.locator("#reader .notice")).toHaveCount(0);
});

test("a link that shows one address and opens another asks before it opens", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="2"]').click();
  const body = page.frameLocator("#reader iframe");
  await expect(body.locator("#fake")).toHaveAttribute("title", "https://evil.example/login");
  // Clicked, it navigates the window to this address, as every link does (see
  // above), and the shell's guard hands it back rather than opening it.
  const checkedAt = (await body.locator("#fake").getAttribute("href"))!;
  const checked = new URL(checkedAt);
  expect(checked.host).toBe("link-check.orb.invalid");
  expect(checked.searchParams.get("url")).toBe("https://evil.example/login");
  expect(checked.searchParams.get("text")).toBe("https://bank.example/login");
  await expect(body.locator("#news")).toHaveAttribute("href", "https://example.com/news");

  // The shell hands it back; Cancel opens nothing, Open Link opens it.
  type Shell = { __emit: (event: string, payload: unknown) => void; __askAnswer?: string };
  const opened = () =>
    page.evaluate(() => (window as unknown as { __mock: { opened?: string[] } }).__mock.opened ?? []);
  const handBack = (url: string) =>
    page.evaluate((url) => (window as unknown as Shell).__emit("link-check", url), url);
  await handBack(checkedAt);
  await expect.poll(() => callsTo(page, "plugin:dialog|message")).toHaveLength(1);
  const [asked] = await callsTo(page, "plugin:dialog|message");
  expect(JSON.stringify(asked)).toContain("opens https://evil.example/login");
  expect(await opened()).toEqual([]);
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { __mock: { answered?: string[] } }).__mock.answered ?? []))
    .toContain("plugin:dialog|message");
  await page.evaluate(() => ((window as unknown as Shell).__askAnswer = "Open Link"));
  await handBack(checkedAt);
  await expect.poll(opened).toEqual(["https://evil.example/login"]);
});

test("a link is suspect when its text names another site or its address is built to mislead", async ({ page }) => {
  await page.goto("/harness.html");
  const verdicts = await page.evaluate(async () => {
    const { linkSuspicion } = await import("/src/safety.ts");
    const cases: [string, string][] = [
      ["https://www.bank.example/login", "bank.example"],
      ["https://bank.example/", "https://login.bank.example/account"],
      ["https://nodejs.org/", "Node.js"],
      ["https://example.com/news", "Read the news"],
      ["mailto:a@example.com", "b@example.org"],
      ["#top", "example.org"],
      ["https://evil.example/", "www.bank.example"],
      ["//evil.example/", "bank.com"],
      ["https://bank.com@evil.example/", "Sign in"],
      ["http://192.0.2.7/login", "Sign in"],
      ["https://xn--pypal-4ve.com/", "Sign in"],
    ];
    return cases.map(([href, text]) => linkSuspicion(href, text) !== null);
  });
  expect(verdicts).toEqual([false, false, false, false, false, false, true, true, true, true, true]);
});

test("a message the server found forged says so under its header", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await expect(page.locator("#reader .notice.warn")).toHaveCount(0);
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await page.locator('#list .rows .row[data-msg="9"]').click();
  const warnings = page.locator("#reader .notice.warn");
  await expect(warnings).toHaveCount(2);
  await expect(warnings.first()).toContainText("could not be verified (dmarc=fail)");
  await expect(warnings.last()).toContainText("Replies go to help@paypa1-support.ru");
});

test("an attachment that is dangerous to open is marked, and the shell's answer is shown", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await page.locator('#list .rows .row[data-msg="6"]').click();
  const chips = page.locator("#reader .chip.att");
  await expect(chips).toHaveCount(2);
  await expect(chips.first()).toHaveClass(/\brisky\b/);
  await expect(chips.last()).not.toHaveClass(/\brisky\b/);
  const opened = () =>
    page.evaluate(() => (window as unknown as { __mock: { openedAttachments?: string[] } }).__mock.openedAttachments ?? []);

  await chips.last().locator(".att-name").click();
  await expect.poll(opened).toEqual(["2"]);

  // The question is the shell's, asked in a native dialog, so the page asks
  // nothing of its own; it only reports the answer.
  await page.evaluate(() => ((window as unknown as { __mock: { declineAttachment?: boolean } }).__mock.declineAttachment = true));
  await chips.first().locator(".att-name").click();
  await expect(page.locator("#status")).toContainText("invoice.html not opened");
  expect(await opened()).toEqual(["2"]);
  expect(await callsTo(page, "plugin:dialog|message")).toHaveLength(0);

  await page.evaluate(() => ((window as unknown as { __mock: { declineAttachment?: boolean } }).__mock.declineAttachment = false));
  await chips.first().locator(".att-name").click();
  await expect.poll(opened).toEqual(["2", "1"]);
});

test("open-link on a task opens the address in its notes, and the notes link it", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await intoTasks(page);
  await page.keyboard.press("j");
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText("Call the plumber");
  const opened = () =>
    page.evaluate(() => (window as unknown as { __mock: { opened?: string[] } }).__mock.opened ?? []);
  await page.keyboard.press("g");
  await page.keyboard.press("x");
  await expect.poll(opened).toEqual(["https://plumber.example.com/book"]);
  await expect(page.locator("#status")).toHaveText("opened https://plumber.example.com/book");
  const link = page.locator("#reader .tk-body a[data-link]").first();
  await expect(link).toHaveText("https://plumber.example.com/book");
  await link.click();
  await expect.poll(opened).toHaveLength(2);
});

test("a message:// link in a task's notes opens the mail in Orb", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await intoTasks(page);
  await page.keyboard.press("j");
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText("Call the plumber");
  const link = page.locator("#reader .tk-body a[data-link^='message:']");
  await expect(link).toHaveText("message://%3cquote@plumber.example.com%3e");
  await link.click();
  const looked = () =>
    page.evaluate(() =>
      ((window as unknown as { __mock: { calls?: [string, { message?: string }][] } }).__mock.calls ?? [])
        .filter(([cmd]) => cmd === "linked_message")
        .map(([, args]) => args.message),
    );
  await expect.poll(looked).toEqual(["<quote@plumber.example.com>"]);
  await expect(cursorSubject(page)).toHaveText("Message #5000");
  const opened = await page.evaluate(
    () => (window as unknown as { __mock: { opened?: string[] } }).__mock.opened ?? [],
  );
  expect(opened).toEqual([]);
});

test("tasks selected apart are dragged together and land as one block", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&manytasks=2");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const titles = page.locator("#list [data-task] .tk-title");
  await expect(titles).toHaveText(["Buy groceries", "Call the plumber", "Renew the domain", "Errand 0", "Errand 1"]);
  await page.locator('#list [data-task="0"]').click();
  await page.locator('#list [data-task="2"]').click({ modifiers: ["Meta"] });
  await expect(page.locator("#list .tk-row.selected")).toHaveCount(2);
  const grab = (await box(page.locator('#list [data-task="2"]')));
  const drop = (await box(page.locator('#list [data-task="3"]')));
  await page.mouse.move(grab.x + 200, grab.y + grab.height / 2);
  await page.mouse.down();
  await page.mouse.move(drop.x + 200, drop.y + drop.height - 3, { steps: 8 });
  await expect(page.locator("#list .dragging")).toHaveCount(2);
  await page.mouse.up();
  await expect(titles).toHaveText(["Call the plumber", "Errand 0", "Buy groceries", "Renew the domain", "Errand 1"]);
  await expect(page.locator("#status")).toHaveText("moved 2 tasks");
});

test("steps are selected with ⌘ and Shift, removed together, and moved by dragging", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator('#list [data-task="0"]').click();
  const steps = page.locator("#reader .step");
  const titles = () => steps.locator(".st").allTextContents();
  await expect(steps).toHaveCount(3);

  // Drag the last step above the first. Both measured in one evaluation:
  // the detail is redrawn as the startup settles, and a row measured across
  // two round trips may be gone by the second.
  const [first, last] = await page.evaluate(() =>
    [...document.querySelectorAll("#reader .step")]
      .filter((_, i) => i === 0 || i === 2)
      .map((row) => {
        const r = row.getBoundingClientRect();
        return { x: r.x, y: r.y, height: r.height };
      }),
  );
  await page.mouse.move(last!.x + 200, last!.y + last!.height / 2);
  await page.mouse.down();
  await page.mouse.move(first!.x + 200, first!.y + 4, { steps: 6 });
  await page.mouse.up();
  await expect.poll(titles).toEqual(["Go to the shop", "Write the list", "Check the fridge"]);

  // Select the first, then the last with Shift: all three.
  await steps.nth(0).locator(".st").click();
  await steps.nth(2).locator(".st").click({ modifiers: ["Shift"] });
  await expect(page.locator("#reader .step.selected")).toHaveCount(3);
  // ⌘ takes the middle one out again.
  await steps.nth(1).locator(".st").click({ modifiers: ["Meta"] });
  await expect(page.locator("#reader .step.selected")).toHaveCount(2);

  await page.locator("#list").focus();
  await page.keyboard.press("Backspace");
  await expect.poll(titles).toEqual(["Write the list"]);
});

test("steps selected apart are dragged as one block and ticked together with x", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator('#list [data-task="0"]').click();
  const steps = page.locator("#reader .step");
  const titles = () => steps.locator(".st").allTextContents();
  await expect(steps).toHaveCount(3);

  await steps.nth(0).locator(".st").click();
  await steps.nth(2).locator(".st").click({ modifiers: ["Meta"] });
  await expect(page.locator("#reader .step.selected")).toHaveCount(2);

  // Grab the last, drop it below the middle one: both land there, together.
  const [middle, last] = await page.evaluate(() =>
    [...document.querySelectorAll("#reader .step")]
      .filter((_, i) => i > 0)
      .map((row) => {
        const r = row.getBoundingClientRect();
        return { x: r.x, y: r.y, height: r.height };
      }),
  );
  await page.mouse.move(last!.x + 200, last!.y + last!.height / 2);
  await page.mouse.down();
  await page.mouse.move(middle!.x + 200, middle!.y + middle!.height - 3, { steps: 6 });
  await expect(page.locator("#reader .step.dragging")).toHaveCount(2);
  await page.mouse.up();
  await expect.poll(titles).toEqual(["Check the fridge", "Write the list", "Go to the shop"]);
  await expect(page.locator("#reader .step.selected .st")).toHaveText(["Write the list", "Go to the shop"]);

  // x ticks the selected steps, one way for all, and leaves the task open.
  await page.locator("#list").focus();
  await page.keyboard.press("x");
  await expect(page.locator("#reader .step.done .st")).toHaveText(["Write the list", "Go to the shop"]);
  await expect(page.locator("#status")).toContainText("ticked 2 steps");
  await page.keyboard.press("x");
  await expect(page.locator("#reader .step.done")).toHaveCount(0);
  await expect(page.locator('#list [data-task="0"]')).not.toHaveClass(/done/);
});

test("a task selection outlives what is done to it, and grows from the keyboard", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&manytasks=2");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const selected = page.locator("#list .tk-row.selected .tk-title");

  // Shift-↓ and J extend from the row clicked; K takes one back.
  await page.locator('#list [data-task="0"]').click();
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("J");
  await expect(selected).toHaveText(["Buy groceries", "Call the plumber", "Renew the domain"]);
  await page.keyboard.press("K");
  await expect(selected).toHaveText(["Buy groceries", "Call the plumber"]);

  // Completing them leaves them selected, so the next key acts on them too.
  await page.keyboard.press("x");
  await expect(page.locator("#status")).toContainText("completed 2 tasks");
  await expect(selected).toHaveText(["Buy groceries", "Call the plumber"]);

  // ⌘A takes every task.
  await page.keyboard.press("Meta+a");
  await expect(page.locator("#list .tk-row.selected")).toHaveCount(await page.locator("#list .tk-row").count());

  // A right-click outside the selection lets it go and acts on that row.
  await page.keyboard.press("Escape");
  await page.locator('#list [data-task="0"]').click();
  await page.locator('#list [data-task="1"]').click({ modifiers: ["Meta"] });
  await page.locator('#list [data-task="3"]').click({ button: "right" });
  await expect(page.locator("#list .tk-row.selected")).toHaveCount(0);
});

test("a step is renamed in place by a double-click, and d with steps selected removes only them", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator('#list [data-task="0"]').click();
  const steps = page.locator("#reader .step");
  await expect(steps).toHaveCount(3);

  await steps.nth(1).locator(".st").dblclick();
  const field = page.locator("#tk-step-input");
  await expect(field).toBeFocused();
  await expect(field).toHaveValue("Check the fridge");
  await field.fill("Check the freezer");
  await page.keyboard.press("Enter");
  await expect(field).toHaveCount(0);
  await expect(steps.nth(1).locator(".st")).toHaveText("Check the freezer");

  // The step menu is offered for what was right-clicked.
  await steps.nth(2).locator(".st").click({ button: "right" });
  await expect(steps.nth(2)).toHaveClass(/selected/);
  const menu = await page.evaluate(() =>
    (window as unknown as { __calls: { cmd: string; args: unknown }[] }).__calls
      .filter((c) => c.cmd === "open_step_menu").at(-1)?.args);
  expect(menu).toEqual({ disabled: [], labels: { "toggle-step": "Mark as Done", "remove-step": "Delete Step" } });

  const tasks = page.locator("#list [data-task]");
  const before = await tasks.count();
  await page.locator("#list").focus();
  await page.keyboard.press("d");
  await expect(steps).toHaveCount(2);
  await expect(tasks).toHaveCount(before);
  await expect(steps.locator(".st")).toHaveText(["Write the list", "Check the freezer"]);
});

test("tasks selected are merged into the one kept, as the dialog settles it", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const tasks = page.locator("#list [data-task]");
  const before = await tasks.count();
  await intoTasks(page);
  await page.keyboard.press("Space");
  await page.keyboard.press("Space");
  await page.keyboard.press(":");
  await page.keyboard.type("merge-tasks");
  await page.keyboard.press("Enter");

  const dialog = page.locator(".task-merge");
  await expect(dialog.locator(".tm-option")).toHaveText(["Buy groceries", "Call the plumber"]);
  await expect(dialog.locator("#tm-title")).toBeFocused();
  await expect(dialog.locator("#tm-title")).toHaveValue("Buy groceries");
  await expect(dialog.locator("#tm-notes")).toHaveValue(/Call the plumber/);
  await expect(dialog.locator(".tm-along")).toContainText("The other task goes to the Trash");

  // Keeping the other one takes its title, until the title is typed in.
  await dialog.locator(".tm-option", { hasText: "Call the plumber" }).click();
  await expect(dialog.locator("#tm-title")).toHaveValue("Call the plumber");
  await expect(dialog.locator(".tm-option.on")).toHaveText("Call the plumber");
  await dialog.locator("#tm-title").fill("Groceries and the plumber");
  await page.keyboard.press("Meta+Enter");

  await expect(dialog).toHaveCount(0);
  await expect(tasks).toHaveCount(before - 1);
  await expect(page.locator("#status")).toContainText("merged 2 tasks into “Groceries and the plumber”");
  const merge = await page.evaluate(() =>
    (window as unknown as { __calls: { cmd: string; args: Record<string, unknown> }[] }).__calls
      .filter((c) => c.cmd === "merge_tasks").at(-1)?.args);
  expect(merge).toMatchObject({ keep: 2, others: [1], title: "Groceries and the plumber" });
});

test("w marks a task as being worked on, seen in the status line and the sidebar, until it is done", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const working = page.locator("#status-steady .working");
  await page.locator('#list [data-task="1"]').click();
  await page.locator("#list").focus();
  await page.keyboard.press("w");
  await expect(working).toHaveText("▶ Call the plumber");
  await expect(page.locator('#list [data-task="1"]')).toHaveClass(/\bworking\b/);
  await expect(page.locator("#folders .sb-row.today .sb-working")).toHaveCount(1);
  await expect(page.locator("#reader .p.acc", { hasText: "Working on" })).toBeVisible();

  // A second task: the status line counts them.
  await page.keyboard.press("k");
  await page.keyboard.press("w");
  await expect(working).toHaveText("▶ 2 tasks");

  // Done is not in progress.
  await page.locator('#list [data-task="0"] [data-task-toggle]').click();
  await expect(working).toHaveText("▶ Call the plumber");

  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("work-on clear");
  await page.keyboard.press("Enter");
  await expect(working).toHaveCount(0);
  await expect(page.locator("#folders .sb-working")).toHaveCount(0);
});

test("an overdue task export says so until one is written", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&export=overdue");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const warning = page.locator("#status-steady .warn", { hasText: "task export" });
  await expect(warning).toHaveText("task export overdue");
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("export-tasks");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status")).toHaveText(/^exported \d+ tasks to \/tmp\/orb-exports\//);
  await expect(warning).toHaveCount(0);
});

test("import-tasks says what it will do and imports only on a yes", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const run = async () => {
    await page.locator("#list").focus();
    await page.keyboard.press(":");
    await page.keyboard.type("import-tasks /tmp/snapshot");
    await page.keyboard.press("Enter");
  };
  const calls = () =>
    page.evaluate(() =>
      (window as unknown as { __mock: { calls: [string, Record<string, unknown>][] } }).__mock.calls.filter(
        ([cmd]) => cmd === "import_tasks" || cmd === "plugin:dialog|message",
      ),
    );
  await run();
  await expect(page.locator("#status")).toHaveText("nothing imported");
  const asked = (await calls()).find(([cmd]) => cmd === "plugin:dialog|message")!;
  expect(JSON.stringify(asked[1])).toContain("2 new, 1 newer than here, 3 as new or newer here");
  expect(JSON.stringify(asked[1])).toContain("Old errand");
  expect((await calls()).filter(([cmd, args]) => cmd === "import_tasks" && args.apply)).toHaveLength(0);

  await page.evaluate(() => ((window as unknown as { __askAnswer: string }).__askAnswer = "Import"));
  await run();
  await expect(page.locator("#status")).toHaveText("imported 3 tasks, merged the history of 3");
});

test("import-things says what it will bring over and imports only on a yes", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const run = async () => {
    await page.locator("#list").focus();
    await page.keyboard.press(":");
    await page.keyboard.type("import-things");
    await page.keyboard.press("Enter");
  };
  await run();
  await expect(page.locator("#status")).toHaveText("nothing imported");
  const asked = await page.evaluate(() =>
    (window as unknown as { __mock: { calls: [string, Record<string, unknown>][] } }).__mock.calls.find(
      ([cmd]) => cmd === "plugin:dialog|message",
    ),
  );
  const text = JSON.stringify(asked![1]);
  expect(text).toContain("12 to-dos become tasks: 7 done, 2 repeating.");
  expect(text).toContain("Pay rent");
  expect(text).toContain("4 tasks from an earlier import take the changes made in Things.");
  expect(text).toContain("1 task trashed in Things goes to the trash here.");
  expect(text).toContain("Edited here since, and so left as they are: Fix the gate.");
  expect(text).toContain("Things itself is not changed.");

  await page.evaluate(() => ((window as unknown as { __askAnswer: string }).__askAnswer = "Import"));
  await run();
  await expect(page.locator("#status")).toHaveText("imported 12 tasks, updated 4, trashed 1 from Things");
});

test("the toolbar names the folder and sorts it from a menu, by mouse or keyboard", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Archive");
  await expect(page.locator("#toolbar .tb-s")).toHaveText("Work");
  const button = page.locator("#toolbar .sortbtn");
  await expect(button).toHaveText(/Date ↓/);
  await expect(button).toHaveAttribute("aria-haspopup", "menu");

  await button.click();
  const menu = page.locator("#toolbar [role=menu]");
  await expect(menu).toBeVisible();
  await expect(menu.locator('[role=menuitemradio][aria-checked="true"] .lbl')).toHaveText(["Date", "Newest first"]);
  await menu.locator("[data-by=from]").click();
  await expect(menu).toHaveCount(0);
  await expect(button).toHaveText(/Sender ↑/);
  const lastSort = () =>
    page.evaluate(() =>
      (window as any).__mock.calls.filter((c: any) => c[0] === "messages").pop()?.[1].sort,
    );
  await expect.poll(lastSort).toBe("SenderAsc");

  // From the keyboard: the arrows walk the menu, Enter picks, Escape leaves.
  await page.keyboard.press(":");
  for (const key of "sort-menu") await page.keyboard.press(key);
  await page.keyboard.press("Enter");
  await expect(menu).toBeVisible();
  await expect(page.locator("#toolbar .mi:focus .lbl")).toHaveText("Sender");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await expect(page.locator("#toolbar .mi:focus .lbl")).toHaveText("Z to A");
  await page.keyboard.press("Enter");
  await expect.poll(lastSort).toBe("SenderDesc");
  await expect(button).toHaveText(/Sender ↓/);
});

test("a folder opens in the order its config gives it", async ({ page }) => {
  await page.goto("/harness.html?archivesort=DateOldest");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await expect(page.locator("#toolbar .sortbtn")).toHaveText(/Date ↑/);
  const sorts = await page.evaluate(() =>
    (window as any).__mock.calls
      .filter((c: any) => c[0] === "messages" && c[1].folder === "Archive")
      .map((c: any) => c[1].sort),
  );
  expect(new Set(sorts)).toEqual(new Set(["DateOldest"]));
});

test("the search box opens the search and holds the query while it is open", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#toolbar .search").click();
  await expect(page.locator("#search-overlay")).toBeVisible();
  await page.keyboard.type("message #1");
  await page.keyboard.press("Enter");
  await expect(page.locator("#toolbar .search.on .q")).toHaveText("message #1");
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Search");
  await page.locator("#toolbar [data-search-clear]").click();
  await expect(page.locator("#toolbar .search.on")).toHaveCount(0);
});

test("the status line says what was staged and how to take it back, and where the cursor is", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await expect(page.locator("#status-steady .pos")).toHaveText("1/2");
  await page.keyboard.press("d");
  await expect(page.locator("#status")).toContainText("for deletion");
  await expect(page.locator("#status kbd")).toHaveText("U");
  await expect(page.locator("#status")).toHaveAttribute("aria-live", "polite");
});

test("the message list is a listbox with its cursor announced", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await expect(page.locator("#list .rows")).toHaveAttribute("role", "listbox");
  await expect(page.locator("#list .row.cursor")).toHaveAttribute("aria-selected", "true");
  await expect(page.locator("#status")).toHaveAttribute("aria-live", "polite");
  // The pane keeps the keyboard and names the cursor row as the active one.
  const active = await page.locator("#list").getAttribute("aria-activedescendant");
  await expect(page.locator(`#${active}`)).toHaveClass(/cursor/);
});

test("HTML mail in a dark window is drawn dark with its pictures put back", async ({ page }) => {
  await page.goto("/harness.html?theme=dark&view=html");
  await page.locator("#reader iframe").waitFor();
  await expect(page.locator("#reader iframe")).toHaveClass(/dark-mail/);
  const srcdoc = await page.locator("#reader iframe").getAttribute("srcdoc");
  expect(srcdoc).toContain("invert(1) hue-rotate(180deg)");
});

test("j moves the cursor and the reader follows", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.keyboard.press("j");
  await expect(page.locator("#reader")).toContainText("Message #1");
});

test("rows are as tall as the density and the text size ask, and grow with ⌘+", async ({ page }) => {
  // In one evaluation: the list re-renders when the startup sync ends, and
  // a row looked up in one round trip may be gone by the next.
  const rowHeight = () =>
    page.evaluate(() => document.querySelector<HTMLElement>("#list .rows .row")?.offsetHeight ?? 0);
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  expect(await rowHeight()).toBe(24);
  await page.locator("#list").focus();
  await page.keyboard.press("Meta+=");
  await expect.poll(rowHeight).toBe(26);
  await expect(page.locator("#list .rows")).toHaveCSS("height", "52px");

  // ⌘+ is remembered on the machine, over the configured size; forget it.
  await page.evaluate(() => localStorage.clear());
  await page.goto("/harness.html?density=compact&font=17");
  await page.locator("#list .row.cursor").waitFor();
  expect(await rowHeight()).toBe(26);
  // The address stays beside the name, in compact as everywhere.
  await expect(page.locator("#list .row.cursor .from .addr")).toBeVisible();
});

test("the Terminal look leads with flags and the date, and draws keys and labels as text", async ({ page }) => {
  await page.goto("/harness.html?look=terminal");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  const row = await page.evaluate(() => {
    const row = document.querySelector<HTMLElement>("#list .row.cursor")!;
    const left = (selector: string) => row.querySelector(selector)!.getBoundingClientRect().left;
    const glyphs = [...row.querySelectorAll(".status .mark")].map(
      (mark) => getComputedStyle(mark, "::before").content,
    );
    return { glyphs, order: [left(".status"), left(".date"), left(".from"), left(".subject")] };
  });
  // Unread, flagged and with files, in the flag column.
  expect(row.glyphs).toEqual(['"N"', '"!"', '"+"']);
  expect([...row.order].sort((a, b) => a - b)).toEqual(row.order);
  // The header's label is lowercased by the look; its text stays as written.
  const from = page.locator('#reader .hdr[data-k="From"] .k');
  await expect(from).toHaveText("From");
  await expect(from).toHaveCSS("text-transform", "lowercase");
  await expect(page.locator("#status-mode")).toHaveText("MAIL");
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await expect(page.locator("#status-mode")).toHaveText("CMD");
  await page.keyboard.press("Escape");
  await expect(page.locator("#status-mode")).toHaveText("MAIL");
});

test("the Hybrid look has no mode segment", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await expect(page.locator("#status-mode")).toBeHidden();
});

test("an empty folder's name keeps its case when the look lowercases labels", async ({ page }) => {
  await page.goto("/harness.html?look=terminal&tasks=1&empty=1");
  await page.locator("#list .empty-view").waitFor();
  await settled(page);
  await page.locator("#folders .sb-row", { hasText: "Work" }).first().click();
  const title = page.locator("#list .empty-view h3");
  await expect(title).toHaveText("Work is empty");
  // innerText is the text as drawn, after the look's text-transform.
  expect(await title.evaluate((h) => (h as HTMLElement).innerText)).toBe("Work is empty");
});

test("toggle-look switches every window's look on this machine, until the config names one", async ({ page }) => {
  const look = () => page.evaluate(() => document.documentElement.dataset.look);
  const rowHeight = () =>
    page.evaluate(() => document.querySelector<HTMLElement>("#list .rows .row")?.offsetHeight ?? 0);
  const run = async (command: string) => {
    await page.locator("#list").focus();
    await page.keyboard.press(":");
    await page.keyboard.type(command);
    await page.keyboard.press("Enter");
  };
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await run("toggle-look");
  await expect.poll(look).toBe("terminal");
  expect(await rowHeight()).toBe(24);
  await expect(page.locator("#status")).toHaveText("Terminal look");

  // Remembered on the machine.
  await page.reload();
  await page.locator("#list .row.cursor").waitFor();
  expect(await look()).toBe("terminal");

  // Back to the config's look: nothing of it is kept.
  await run("look hybrid");
  await expect.poll(look).toBe("hybrid");
  expect(await page.evaluate(() => localStorage.getItem("orb.look"))).toBeNull();

  // A look the config comes to name outranks what the machine chose.
  await run("toggle-look");
  await expect.poll(look).toBe("terminal");
  await page.evaluate(() => {
    const w = window as unknown as { __configLook: string; __emit: (event: string, payload: unknown) => void };
    w.__configLook = "hybrid";
    w.__emit("config-reloaded", { error: null, restart_needed: [] });
  });
  await expect(page.locator("#status")).toHaveText("config.yml reloaded");
  expect(await look()).toBe("terminal");
  await page.evaluate(() => {
    const w = window as unknown as { __configLook: string; __emit: (event: string, payload: unknown) => void };
    w.__configLook = "terminal";
    w.__emit("config-reloaded", { error: null, restart_needed: [] });
  });
  await expect.poll(() => page.evaluate(() => localStorage.getItem("orb.look"))).toBeNull();
  expect(await look()).toBe("terminal");
});

test("the Terminal look has rows of its own height, and the look is the root's to say", async ({ page }) => {
  const rowHeight = () =>
    page.evaluate(() => document.querySelector<HTMLElement>("#list .rows .row")?.offsetHeight ?? 0);
  await page.goto("/harness.html?look=terminal");
  await page.locator("#list .row.cursor").waitFor();
  await expect(page.locator("html")).toHaveAttribute("data-look", "terminal");
  expect(await rowHeight()).toBe(24);
  await page.goto("/harness.html?look=terminal&density=spacious");
  await page.locator("#list .row.cursor").waitFor();
  expect(await rowHeight()).toBe(28);
});

test("the reader's headers keep each label apart from its value", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#reader h1").waitFor();
  const from = page.locator('#reader .hdr[data-k="From"]');
  await expect(from.locator(".k.lbl")).toHaveText("From");
  // The name and the address, apart.
  await expect(from.locator(".v b")).toHaveText("Sender");
  await expect(from.locator(".v .addr")).toHaveText("s@example.com");
  // The date as a person reads it, the recipients by name, the user as "me".
  await expect(page.locator('#reader .hdr[data-k="Date"] .v')).toHaveText(/^(Thu 1 Jan|1 Jan 2026), \d\d:\d\d$/);
  await expect(page.locator('#reader .hdr[data-k="To"] .v')).toHaveText("me");
});

test("a long subject wraps short of the rendering switch", async ({ page }) => {
  await page.goto("/harness.html");
  // Message 0 has an HTML part, so the switch is drawn.
  await page.locator("#reader .views").waitFor();
  await settled(page);
  const overlap = await page.evaluate(() => {
    const subject = document.querySelector<HTMLElement>("#reader .h-subj")!;
    subject.textContent = "A subject long enough to reach the right edge of the reader ".repeat(3);
    const s = subject.getBoundingClientRect();
    const v = document.querySelector("#reader .views")!.getBoundingClientRect();
    return s.right > v.left && s.top < v.bottom;
  });
  expect(overlap).toBe(false);
});

test("the history's times stay clear of its entries", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await intoTasks(page);
  await page.locator("#reader .td-log li").first().waitFor();
  await settled(page);
  const clashes = await page.evaluate(() =>
    [...document.querySelectorAll("#reader .td-log li")].filter((li) => {
      // How WebKit, which the app runs in, writes a time on a 12-hour clock.
      const node = li.querySelector("time")!;
      node.textContent = "Oct 15 at 09:06 PM";
      // The text's extent, not the cell's: text overflows its cell unclipped.
      const range = document.createRange();
      range.selectNodeContents(node);
      const time = range.getBoundingClientRect();
      const text = li.querySelector("time + *")!.getBoundingClientRect();
      return time.right > text.left;
    }).length,
  );
  expect(clashes).toBe(0);
});

test("the reader and the task detail start where the list's text starts", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await intoTasks(page);
  await expect(page.locator("#reader .td-title h1")).toHaveText("Buy groceries");
  const edges = await page.evaluate(() => {
    const left = (sel: string) => Math.round(document.querySelector(sel)!.getBoundingClientRect().left);
    const centre = (sel: string) => {
      const r = document.querySelector(sel)!.getBoundingClientRect();
      return Math.round(r.left + r.width / 2);
    };
    return {
      listText: left("#list [data-task] .tk-title"),
      props: left("#reader .props .p"),
      listBox: centre("#list [data-task] .tk-box .chk"),
      detailBox: centre("#reader .td-title .chk"),
    };
  });
  // The pane sits right of the list's left edge by the same offset as list and
  // reader stack vertically here, so compare against the list's own column.
  const listLeft = await page.evaluate(() => Math.round(document.getElementById("list")!.getBoundingClientRect().left));
  const readerLeft = await page.evaluate(() => Math.round(document.getElementById("reader")!.getBoundingClientRect().left));
  expect(edges.props - readerLeft).toBe(edges.listText - listLeft);
  expect(edges.detailBox - readerLeft).toBe(edges.listBox - listLeft);
  // Set properties are chips; what is not set is one line of keys.
  await expect(page.locator('#reader .props .p[data-k="Due"]')).toBeVisible();
  await expect(page.locator("#reader .td-hints .hint", { hasText: "Due" })).toHaveCount(0);
  await expect(page.locator("#reader .td-hints .hint", { hasText: "Repeat" })).toBeVisible();
  await expect(page.locator("#reader")).not.toContainText("not set");
});

test("Today shows the tasks and every inbox as one list", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .task-row, #list [data-task]").first().waitFor();
  // The mail half pages through both inboxes: Work's two and Private's four.
  await expect(page.locator(".mail-scroll .rows")).toHaveCSS("height", /px$/);
  const height = await page.$eval(".mail-scroll .rows", (el) => parseFloat((el as HTMLElement).style.height));
  const rowHeight = await page.$eval("#list", (el) =>
    parseFloat(getComputedStyle(el).getPropertyValue("--row-height")),
  );
  expect(Math.round(height / rowHeight)).toBe(6);
});

test("Today shows its mail above its tasks, opens on the first message, and walks on into the tasks", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  const top = async (sel: string) => (await box(page.locator(sel).first())).y;
  expect(await top("#list .mail-sec")).toBeLessThan(await top("#list .tk-section"));
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await expect(page.locator('#list .rows .row.cursor[data-msg="0"]')).toHaveCount(1);
  // Past the last of the six messages is the first task.
  await page.locator("#list").focus();
  await page.keyboard.press("G");
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText("Renew the domain");
  await page.keyboard.press("g");
  await page.keyboard.press("g");
  await expect(page.locator('#list .rows .row.cursor[data-msg="0"]')).toHaveCount(1);
  for (let i = 0; i < 6; i++) await page.keyboard.press("j");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText("Buy groceries");
});

test("Today in columns puts the mail left and the tasks right, compact, and keeps the cursor in its column", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("l");
  await expect(page.locator("#list")).toHaveClass(/today-columns/);
  expect(await todaySwitch(page, '[data-today-layout="columns"]')).toBe("true");
  await expect(page.locator("#list [data-tk-split]")).toHaveCount(0);
  const mail = (await box(page.locator("#list .mail-scroll")));
  const tasks = (await box(page.locator("#list .tk-section")));
  expect(mail.x + mail.width).toBeLessThanOrEqual(tasks.x + 1);
  expect(Math.abs(mail.y - tasks.y)).toBeLessThan(1);
  // The rows of both columns line up under their labels.
  const firstRow = async (sel: string) => (await box(page.locator(sel).first())).y;
  expect(await firstRow("#list .mail-scroll .rows .row")).toBe(await firstRow("#list [data-task]"));
  // One line each, without the snippet or the address beside a name.
  const row = page.locator("#list .mail-scroll .rows .row:not(.bare-from)").first();
  await expect(row.locator(".subject .snip")).toBeHidden();
  await expect(row.locator(".from .addr")).toBeHidden();

  // j stops at the end of the mail; Tab crosses, and G stops at the last task.
  await page.keyboard.press("G");
  await page.keyboard.press("j");
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await expect(page.locator('#list .rows .row.cursor[data-msg="5"]')).toHaveCount(1);
  await page.keyboard.press("Tab");
  await page.keyboard.press("G");
  await page.keyboard.press("j");
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText("Renew the domain");
  await page.keyboard.press("g");
  await page.keyboard.press("g");
  await page.keyboard.press("k");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText("Buy groceries");

  // Remembered on this machine, and the toolbar switches back.
  await page.reload();
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await expect(page.locator("#list")).toHaveClass(/today-columns/);
  await chooseInToday(page, '[data-today-layout="stacked"]');
  await expect(page.locator("#list")).not.toHaveClass(/today-columns/);
  await expect(page.locator("#list [data-tk-split]")).toHaveCount(1);
});

test("Today's Tasks heading counts the tasks due by today in red, and zd lists them first under Due", async ({ page }) => {
  await page.clock.setFixedTime(new Date("2026-08-07T10:00:00"));
  await page.goto("/harness.html?tasks=1");
  await intoTasks(page);
  const titles = () => page.locator("#list .tk-row .tk-title").allTextContents();
  const count = page.locator("#list .due-switch .due-n");
  // Off: the global order, and the count all the same.
  await expect(count).toHaveText("2");
  expect(await count.evaluate((n) => getComputedStyle(n).color)).toBe(
    await page.locator("#list .tk-date.overdue").first().evaluate((n) => getComputedStyle(n).color),
  );
  expect(await titles()).toEqual(["Buy groceries", "Call the plumber", "Renew the domain"]);
  await expect(page.locator("#list .tk-day")).toHaveCount(0);

  await page.keyboard.press("z");
  await page.keyboard.press("d");
  await expect(page.locator("#list .due-switch button")).toHaveAttribute("aria-checked", "true");
  await expect.poll(titles).toEqual(["Buy groceries", "Renew the domain", "Call the plumber"]);
  await expect(page.locator("#list .tk-day .lbl")).toHaveText(["Due", "Other"]);
  await expect(page.locator('#list .tk-day[data-group="due"] .n')).toHaveText("2");

  // A preference of this Mac: still on after a reload.
  await page.reload();
  await intoTasks(page);
  await expect.poll(titles).toEqual(["Buy groceries", "Renew the domain", "Call the plumber"]);

  // Completing a due task takes it out of the band and the count.
  await page.locator('#list .tk-row:has-text("Renew the domain") [data-task-toggle]').click();
  await expect(count).toHaveText("1");
  await expect(page.locator('#list .tk-day[data-group="due"] .n')).toHaveText("1");

  // A click switches it off again.
  await page.locator("#list .due-switch button").click();
  await expect(page.locator("#list .due-switch button")).toHaveAttribute("aria-checked", "false");
  await expect(page.locator("#list .tk-day")).toHaveCount(0);
});

test("a task moved by key in Due first stays in its band, placed among its neighbours there", async ({ page }) => {
  await page.clock.setFixedTime(new Date("2026-08-07T10:00:00"));
  await page.addInitScript(() => localStorage.setItem("orb.todayDueFirst", "on"));
  await page.goto("/harness.html?tasks=1");
  await intoTasks(page);
  const titles = () => page.locator("#list .tk-row .tk-title").allTextContents();
  await expect.poll(titles).toEqual(["Buy groceries", "Renew the domain", "Call the plumber"]);

  // The first of Other cannot climb into Due: nothing is asked of the core.
  for (let i = 0; i < 2; i++) await page.keyboard.press("j");
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText("Call the plumber");
  await page.keyboard.press("Meta+ArrowUp");
  await page.waitForTimeout(100);
  expect(await callsTo(page, "move_task")).toHaveLength(0);

  // Within Due, it moves between the tasks of Due.
  await page.keyboard.press("k");
  await expect(page.locator("#list .tk-row.cursor .tk-title")).toHaveText("Renew the domain");
  await page.keyboard.press("Meta+ArrowUp");
  await expect.poll(() => callsTo(page, "move_task")).toEqual([{ id: 6, before: null, after: 1 }]);
});

test("Today's tasks and mail share the rows' grid, each under a label with its count", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await expect(page.locator("#list .tk-section .sec")).toContainText(/Tasks\s*3/);
  await expect(page.locator("#list .mail-sec")).toContainText(/Mail\s*6/);
  // One left edge for the text, one height, one date column.
  const edges = await page.evaluate(() => {
    const task = document.querySelector<HTMLElement>("#list [data-task]")!;
    const mail = document.querySelector<HTMLElement>("#list .mail-scroll .rows .row")!;
    const left = (el: Element) => Math.round(el.getBoundingClientRect().left);
    const right = (el: Element) => Math.round(el.getBoundingClientRect().right);
    return {
      text: [left(task.querySelector(".tk-title")!), left(mail.querySelector(".from")!)],
      box: [left(task.querySelector(".tk-box")!), left(mail.querySelector(".status")!)],
      date: [right(task.querySelector(".tk-dates")!), right(mail.querySelector(".date")!)],
      height: [task.offsetHeight, mail.offsetHeight],
    };
  });
  expect(edges.text[0]).toBe(edges.text[1]);
  expect(edges.box[0]).toBe(edges.box[1]);
  expect(edges.date[0]).toBe(edges.date[1]);
  expect(edges.height[0]).toBe(edges.height[1]);
  // An overdue date carries a mark as well as its colour.
  await expect(page.locator("#list [data-task] .tk-date.overdue")).toHaveCount(1);
  await expect
    .poll(() =>
      page.evaluate(() => {
        const date = document.querySelector("#list [data-task] .tk-date.overdue");
        return date ? getComputedStyle(date, "::before").content : "";
      }),
    )
    .toBe('"! "');
});

test("x in Today applies what is staged in every inbox it shows", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  // Today opens on the mail: stage a move there.
  await page.keyboard.press("A");
  await expect
    .poll(() => page.evaluate(() => (window as any).__mock.calls.some((c: any) => c[0] === "stage_action")))
    .toBe(true);
  await page.keyboard.press("x");
  await expect
    .poll(() =>
      page.evaluate(() =>
        (window as any).__mock.calls
          .filter((c: any) => c[0] === "execute_pending")
          .map((c: any) => `${c[1].account}/${c[1].folder}`),
      ),
    )
    .toEqual(["Work/INBOX", "Private/INBOX"]);
});

test(":filter in Today runs the rules on every inbox it shows, not the folder last opened", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await page.keyboard.press(":");
  await page.keyboard.type("filter");
  await page.keyboard.press("Enter");
  await expect
    .poll(() =>
      page.evaluate(() =>
        (window as any).__mock.calls
          .filter((c: any) => c[0] === "run_filters")
          .map((c: any) => `${c[1].account}/${c[1].folder}`),
      ),
    )
    .toEqual(["Work/INBOX", "Private/INBOX"]);
  await expect(page.locator("#status")).toHaveText("no filter rule matched");
});

test("a task is reordered by dragging it, and a short press stays a click", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").nth(2).waitFor();
  await settled(page);
  const titles = () => page.locator("#list [data-task] .tk-title").allTextContents();
  expect(await titles()).toEqual(["Buy groceries", "Call the plumber", "Renew the domain"]);
  const first = await page.locator("#list [data-task]").nth(0).boundingBox();
  const second = await page.locator("#list [data-task]").nth(1).boundingBox();
  await page.mouse.move(first!.x + 200, first!.y + first!.height / 2);
  await page.mouse.down();
  await page.mouse.move(first!.x + 200, second!.y + second!.height * 0.8, { steps: 6 });
  await expect(page.locator("#list [data-task]").nth(1)).toHaveClass(/drop-below/);
  await page.mouse.up();
  await expect.poll(titles).toEqual(["Call the plumber", "Buy groceries", "Renew the domain"]);
  // The dragged task stays under the cursor.
  await expect(page.locator("#list [data-task].cursor .tk-title")).toHaveText("Buy groceries");
  const moves = await page.evaluate(() =>
    (window as any).__mock.calls.filter((c: any) => c[0] === "move_task").length,
  );
  expect(moves).toBe(1);
  await page.locator("#list [data-task]").nth(2).click();
  await expect(page.locator("#list [data-task].cursor .tk-title")).toHaveText("Renew the domain");
});

test("Today's mail can be narrowed to one account's inbox from its label, remembered", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  const choices = page.locator("#list .mail-sec [data-mail-account]");
  await expect(choices).toHaveText(["All", "Work", "Private"]);
  await expect(page.locator('#list .mail-sec [data-mail-account][aria-checked="true"]')).toHaveText("All");
  await page.locator('#list .mail-sec [data-mail-account="Private"]').click();
  await expect(page.locator('#list .mail-sec [data-mail-account][aria-checked="true"]')).toHaveText("Private");
  await expect(page.locator("#list .mail-sec .n")).toHaveText("4");
  const lastFolders = () =>
    page.evaluate(() =>
      (window as any).__mock.calls.filter((c: any) => c[0] === "inbox_messages").pop()?.[1].folders,
    );
  await expect.poll(lastFolders).toEqual([["Private", "INBOX"]]);
  await page.reload();
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await expect(page.locator('#list .mail-sec [data-mail-account][aria-checked="true"]')).toHaveText("Private");
  await page.locator('#list .mail-sec [data-mail-account=""]').click();
  await expect(page.locator("#list .mail-sec .n")).toHaveText("6");
});

test("Today's Mail label is centred on its switches", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-sec .seg:not(.threads-switch)").waitFor();
  await settled(page);
  const offsets = await page.evaluate(() => {
    const middle = (sel: string) => {
      const r = document.querySelector(sel)!.getBoundingClientRect();
      return r.top + r.height / 2;
    };
    // The account switch, and the one between messages and conversations.
    return [".seg:not(.threads-switch)", ".seg.threads-switch"].flatMap((switcher) => {
      const seg = middle(`#list .mail-sec ${switcher}`);
      return [middle("#list .mail-sec .lbl") - seg, middle("#list .mail-sec .n") - seg];
    });
  });
  for (const offset of offsets) expect(Math.abs(offset)).toBeLessThanOrEqual(1);
});

test("the context menus grey out what does not apply and say which way a toggle goes", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const lastMenu = (cmd: string) =>
    page.evaluate((cmd) => (window as any).__mock.calls.filter((c: any) => c[0] === cmd).pop()?.[1], cmd);

  // An open task: Complete applies, Un-complete and Restore do not.
  await page.locator("#list [data-task]").first().click({ button: "right" });
  await expect.poll(() => lastMenu("open_task_menu")).toBeTruthy();
  const task = await lastMenu("open_task_menu");
  expect(task.disabled).toContain("uncomplete-task");
  expect(task.disabled).toContain("restore-task");
  expect(task.disabled).not.toContain("complete-task");

  // An unread message, nothing staged: read it, but nothing to unmark or apply.
  await page.locator("#list .mail-scroll .rows .row").first().click({ button: "right" });
  await expect.poll(() => lastMenu("open_message_menu")).toBeTruthy();
  const mail = await lastMenu("open_message_menu");
  expect(mail.labels["toggle-read"]).toBe("Mark as Read");
  expect(mail.disabled).toEqual(expect.arrayContaining(["unmark", "expunge"]));
  expect(mail.disabled).not.toContain("mark-delete");
});

test("without tasks the sidebar has no empty Tasks heading", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await expect(page.locator("#folders .sb-head", { hasText: /^Tasks$/ })).toHaveCount(0);
});

test("the pins open the sidebar in their order, under no heading, each view in one place", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1&pins=today,Work/INBOX,Private/INBOX,waiting,view:Invoices");
  await settled(page);
  const labels = page.locator("#folders .sb-row .sb-label");
  await expect(labels.nth(0)).toHaveText("Today");
  await expect(labels.nth(1)).toHaveText("Work");
  await expect(labels.nth(2)).toHaveText("Private");
  await expect(labels.nth(3)).toHaveText("Waiting");
  await expect(labels.nth(4)).toHaveText("Invoices");
  // Nothing above them, and no heading of their own.
  await expect(page.locator("#folders > *").first()).toHaveAttribute("data-pin", "1");
  await expect(page.locator("#folders .sb-section", { hasText: "Pinned" })).toHaveCount(0);
  // A pinned view has left its section; a pinned folder is still in its account.
  await expect(page.locator("#folders .sb-row", { hasText: /^Today/ })).toHaveCount(1);
  await expect(page.locator("#folders .sb-row", { hasText: /^Waiting/ })).toHaveCount(1);
  await expect(page.locator("#folders .sb-row", { hasText: "Invoices" })).toHaveCount(1);
  await expect(page.locator("#folders .sb-row", { hasText: "Unread everywhere" })).toHaveCount(1);
  await expect(page.locator("#folders .sb-row", { hasText: /^INBOX/ })).toHaveCount(2);
});

test("without Today among the pins, it leads the Tasks section", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&pins=Work/INBOX");
  await settled(page);
  const labels = page.locator("#folders .sb-row .sb-label, #folders .sb-section .sb-label");
  await expect(labels.nth(0)).toHaveText("Work");
  await expect(labels.nth(1)).toHaveText("Tasks");
  await expect(labels.nth(2)).toHaveText("Today");
});

test("Ctrl-1 to Ctrl-9 open the pins, whose keys show over them while Ctrl is held", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1&pins=today,Work/INBOX,Private/INBOX,waiting");
  await settled(page);
  await page.locator("#list").focus();
  const cursor = page.locator("#folders .sb-row.cursor .sb-label");

  await page.keyboard.down("Control");
  await expect(page.locator("#folders")).toHaveClass(/\bpin-keys\b/);
  await expect(page.locator("#folders [data-pin] .sb-key")).toHaveText(["1", "2", "3", "4"]);
  for (const key of await page.locator("#folders [data-pin] .sb-key").all()) await expect(key).toHaveCSS("opacity", "1");
  await page.keyboard.press("2");
  await expect(page.locator("#folders")).not.toHaveClass(/\bpin-keys\b/);
  await page.keyboard.up("Control");
  await expect(cursor).toHaveText("Work");
  await expect(page.locator("#list")).toHaveClass(/\bfocused\b/);

  await page.keyboard.down("Control");
  await page.keyboard.up("Control");
  await expect(page.locator("#folders")).not.toHaveClass(/\bpin-keys\b/);

  await page.keyboard.press("Control+4");
  await expect(cursor).toHaveText("Waiting");
  await page.locator("#list .chat-row").first().waitFor();

  // A field types nothing with them, so they reach the pins from there too.
  await page.keyboard.press("r");
  await expect(page.locator("#reader .cc-field")).toBeFocused();
  await page.keyboard.press("Control+2");
  await expect(cursor).toHaveText("Work");
  await expect(page.locator("#reader .cc-field")).toHaveCount(0);
  await page.keyboard.press("Control+4");
  await expect(cursor).toHaveText("Waiting");

  await page.keyboard.press("Control+1");
  await expect(cursor).toHaveText("Today");
  await page.locator("#list [data-task]").first().waitFor();

  await page.keyboard.press("Control+7");
  await expect(page.locator("#status")).toContainText("no pin 7: 4 are pinned");
});

test("gp and ⇧⌘P pin what is open below the config's pins, remembered, and take it back", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1&pins=today,Work/INBOX");
  await settled(page);
  await page.locator("#list").focus();
  const cursor = page.locator("#folders .sb-row.cursor .sb-label");
  const pins = page.locator("#folders [data-pin] .sb-label");
  const status = page.locator("#status");

  // A room, from its composer, which keeps the keyboard: it leaves its group
  // for the pins, the third of them.
  await page.keyboard.press("Meta+k");
  await page.keyboard.type("lukas");
  await page.keyboard.press("Enter");
  await expect(cursor).toHaveText("lukas");
  const field = page.locator("#reader .cc-field");
  await expect(field).toBeFocused();
  await page.keyboard.press("Meta+Shift+p");
  await expect(field).toBeFocused();
  await expect(pins).toHaveText(["Today", "Work", "lukas"]);
  await expect(status).toContainText("pinned lukas as pin 3");
  await expect(page.locator("#folders .sb-row", { hasText: /^lukas/ })).toHaveCount(1);
  await expect(cursor).toHaveText("lukas");
  await page.keyboard.press("Control+1");
  await expect(cursor).toHaveText("Today");
  await page.keyboard.press("Control+3");
  await expect(cursor).toHaveText("lukas");
  await expect(field).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(field).not.toBeFocused();

  // A folder, by gp: it leaves its account, and is named with it.
  await page.keyboard.press("Meta+k");
  await page.keyboard.type("work 2025");
  await page.keyboard.press("Enter");
  await expect(cursor).toContainText("2025");
  await page.keyboard.press("g");
  await page.keyboard.press("p");
  await expect(pins).toHaveText(["Today", "Work", "lukas", "Work: Archive/2025"]);
  await expect(page.locator("#folders .sb-row", { hasText: "2025" })).toHaveCount(1);
  await expect(page.locator("#folders [data-pin='4'] .sb-fold")).toHaveCount(0);

  // A config pin stays where the config put it.
  await page.keyboard.press("Control+2");
  await page.keyboard.press("Meta+Shift+p");
  await expect(status).toContainText("Work is pinned in config.yml");
  await expect(pins).toHaveCount(4);

  // Remembered across a reload.
  await page.reload();
  await settled(page);
  await expect(pins).toHaveText(["Today", "Work", "lukas", "Work: Archive/2025"]);

  await page.locator("#list").focus();
  await page.keyboard.press("Control+3");
  await expect(cursor).toHaveText("lukas");
  await expect(field).toBeFocused();
  await page.keyboard.press("Meta+Shift+p");
  await expect(status).toContainText("unpinned lukas");
  await expect(pins).toHaveText(["Today", "Work", "Work: Archive/2025"]);
  // Back in its group, which opens to show it, with the cursor on it.
  await expect(cursor).toHaveText("lukas");
  await expect(page.locator("#folders .sb-row", { hasText: /^lukas/ })).toHaveCount(1);

  // A heading cannot be pinned: Tasks is the first entry below the pins.
  await page.keyboard.press("Escape");
  await expect(field).not.toBeFocused();
  await page.keyboard.press("Control+1");
  await page.locator("#list [data-task]").first().waitFor();
  for (const key of ["h", "g", "g", "j", "j", "j"]) await page.keyboard.press(key);
  await expect(page.locator("#folders .sb-section.cursor")).toHaveText(/Tasks/);
  await page.keyboard.press("Meta+Shift+p");
  await expect(status).toContainText("only a folder, a view or a chat room can be pinned");
});

test("the pins made with gp are put in another order by dragging them; the config's stay put", async ({ page }) => {
  await page.addInitScript(() => {
    if (localStorage.getItem("orb.sidebar.pins")) return;
    localStorage.setItem("orb.sidebar.pins", JSON.stringify([
      { kind: "room", name: "r-lukas", account: null, folder: null },
      { kind: "folder", name: null, account: "Work", folder: "Archive/2025" },
      { kind: "tasks", name: "upcoming", account: null, folder: null },
    ]));
  });
  await page.goto("/harness.html?tasks=1&chat=1&pins=today,Work/INBOX");
  await settled(page);
  const pins = page.locator("#folders [data-pin] .sb-label");
  await expect(pins).toHaveText(["Today", "Work", "lukas", "Work: Archive/2025", "Upcoming"]);

  const drag = async (from: number, onto: number, below: boolean) => {
    const a = (await page.locator(`#folders [data-pin="${from}"]`).boundingBox())!;
    const b = (await page.locator(`#folders [data-pin="${onto}"]`).boundingBox())!;
    await page.mouse.move(a.x + 20, a.y + a.height / 2);
    await page.mouse.down();
    await page.mouse.move(a.x + 20, a.y + a.height / 2 + 6);
    await page.mouse.move(b.x + 20, b.y + (below ? b.height - 3 : 3), { steps: 4 });
    await page.mouse.up();
  };

  await drag(5, 3, false);
  await expect(pins).toHaveText(["Today", "Work", "Upcoming", "lukas", "Work: Archive/2025"]);
  await expect(page.locator("#status")).toContainText("Upcoming is pin 3 now");
  await drag(3, 5, true);
  await expect(pins).toHaveText(["Today", "Work", "lukas", "Work: Archive/2025", "Upcoming"]);

  // A config pin is not dragged, and is no place to drop one.
  await drag(1, 4, true);
  await drag(4, 1, false);
  await expect(pins).toHaveText(["Today", "Work", "lukas", "Work: Archive/2025", "Upcoming"]);
  // Dropping does not open what was dragged: Today is still on screen.
  await expect(page.locator("#toolbar")).toContainText("Today");

  await page.reload();
  await settled(page);
  await expect(pins).toHaveText(["Today", "Work", "lukas", "Work: Archive/2025", "Upcoming"]);
});

test("gf and ⌘K open anything in the sidebar by a few letters of its name, unfolding what it sits in", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1");
  await settled(page);
  await page.locator("#list").focus();
  const cursor = page.locator("#folders .sb-row.cursor .sb-label");
  const picker = page.locator(".dp.picker");

  // Fold the Tasks heading away: the picker still finds what is under it.
  await page.locator("#folders .sb-section", { hasText: "Tasks" }).locator(".sb-chev").click();
  await expect(page.locator("#folders .sb-row", { hasText: /^Upcoming/ })).toHaveCount(0);
  await page.locator("#list").focus();

  await page.keyboard.press("g");
  await page.keyboard.press("f");
  await expect(picker).toBeVisible();
  await expect(picker).toContainText("Work / Archive/2025");
  await expect(picker).toContainText("Waiting");
  await page.keyboard.type("upc");
  await page.keyboard.press("Enter");
  await expect(picker).toHaveCount(0);
  await expect(cursor).toHaveText("Upcoming");
  await expect(page.locator("#list")).toHaveClass(/\bfocused\b/);

  await page.keyboard.press("Meta+k");
  await page.keyboard.type("work 2025");
  await page.keyboard.press("Enter");
  await expect(cursor).toContainText("2025");
  await expect.poll(async () => (await callsOf(page, "folder_total")).some((c) => c.folder === "Archive/2025")).toBe(true);

  await page.keyboard.press("Meta+k");
  await page.keyboard.type("waiting");
  await page.keyboard.press("Enter");
  await expect(cursor).toHaveText("Waiting");

  await page.keyboard.press("Meta+k");
  await page.keyboard.press("Escape");
  await expect(picker).toHaveCount(0);
  await expect(cursor).toHaveText("Waiting");
});

test("a digit after a pending prefix carries the sequence on, and is a count otherwise", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&pins=today,Work/INBOX&bind=g2:open-pinned%202");
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("g");
  await expect(page.locator("#folders [data-pin] .sb-key")).toHaveText(["", "2"]);
  await expect(page.locator(".keyhints")).toContainText("Open Work");
  await expect(page.locator(".keyhints")).not.toContainText("open-pinned");
  await page.keyboard.press("2");
  await expect(page.locator("#folders .sb-row.cursor .sb-label")).toHaveText("Work");
});

test("every heading in the sidebar folds what is under it, remembered", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&pins=today");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const tasks = page.locator("#folders .sb-section", { hasText: "Tasks" });
  const upcoming = page.locator("#folders .sb-row", { hasText: "Upcoming" });
  const views = page.locator("#folders .sb-section", { hasText: "Views" });
  await expect(upcoming).toBeVisible();
  await tasks.click();
  await expect(upcoming).toHaveCount(0);
  await expect(tasks).toHaveAttribute("aria-expanded", "false");
  await views.click();
  await expect(page.locator("#folders .sb-row", { hasText: "Invoices" })).toHaveCount(0);
  // Today stands above the headings and stays.
  await expect(page.locator("#folders .sb-row.today")).toBeVisible();

  await page.reload();
  await page.locator("#list [data-task]").first().waitFor();
  await expect(upcoming).toHaveCount(0);
  await expect(page.locator("#folders .sb-section.collapsed")).toHaveCount(2);

  // By the keyboard: the cursor stops on a heading, and za opens it.
  await page.keyboard.press("h");
  await page.keyboard.press("g");
  await page.keyboard.press("g");
  await page.keyboard.press("j");
  await expect(page.locator("#folders .sb-section.cursor")).toHaveText(/Tasks/);
  await page.keyboard.press("z");
  await page.keyboard.press("a");
  await expect(upcoming).toBeVisible();

  // Opening a view under a folded heading opens the heading too.
  await tasks.click();
  await expect(upcoming).toHaveCount(0);
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("open-task-view upcoming");
  await page.keyboard.press("Enter");
  await expect(page.locator("#folders .sb-row.cursor", { hasText: "Upcoming" })).toBeVisible();
  await page.evaluate(() => localStorage.clear());
});

test("folders nest under their parent, and folders and accounts fold, remembered", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  const child = page.locator("#folders .sb-row", { hasText: /^2025$/ });
  const archive = page.locator("#folders .sb-row", { hasText: "Archive" }).first();
  await expect(child).toBeVisible();
  // One step in from its parent, and the parent carries the disclosure arrow.
  // Polled: the sidebar is redrawn when the startup sync ends, and a row
  // read across two round trips may be a detached one.
  const indent = () =>
    page.evaluate(() => {
      const rows = [...document.querySelectorAll<HTMLElement>("#folders .sb-row")];
      const pad = (text: string) =>
        parseFloat(getComputedStyle(rows.find((r) => r.textContent?.trim().startsWith(text))!).paddingLeft);
      return pad("2025") - pad("Archive");
    });
  await expect.poll(indent).toBe(16);
  await expect(archive.locator(".sb-glyph .duo")).toHaveCount(1);

  await archive.locator(".sb-fold").click();
  await expect(child).toHaveCount(0);
  await expect(archive).toHaveAttribute("aria-expanded", "false");

  // A folded account still counts what is unread in it.
  const work = page.locator("#folders .sb-account", { hasText: "Work" });
  await work.click();
  await expect(archive).toHaveCount(0);
  await expect(work.locator(".sb-badge.unread")).toHaveText("14");

  await page.reload();
  await page.locator("#list .row.cursor").waitFor();
  await expect(page.locator("#folders .sb-account.collapsed", { hasText: "Work" })).toBeVisible();
  await expect(page.locator("#folders .sb-row", { hasText: "Archive" })).toHaveCount(0);

  // By the keyboard: za on the heading opens the account again.
  await page.keyboard.press("h");
  await page.keyboard.press("G");
  await page.keyboard.press("k");
  await page.keyboard.press("k");
  await expect(page.locator("#folders .sb-account.cursor")).toHaveText(/Work/);
  await page.keyboard.press("z");
  await page.keyboard.press("a");
  await expect(page.locator("#folders .sb-row", { hasText: "Archive" })).toBeVisible();
  await expect(child).toHaveCount(0);
  await page.evaluate(() => localStorage.clear());
});

test("a view opens in place, highlighted, without a search row", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  const view = page.locator("#folders .sb-row", { hasText: "Unread everywhere" });
  await view.click();
  await expect(view).toHaveClass(/cursor/);
  await expect(page.locator("#folders .search-row")).toHaveCount(0);
});

test("a subject rewriting rule's icons are drawn in the list and in the reader", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  const row = page
    .locator("#list .rows .row")
    .filter({ has: page.locator(".subject .text", { hasText: /^Message #2$/ }) });
  await expect(row.locator(".subject-icons i")).toHaveCount(2);
  await expect(row.locator(".subject-icons i.fa-linux")).toHaveClass(/\bfa-brands\b/);
  await expect(row.locator(".subject-icons i.fa-gears")).toHaveCSS("color", "rgb(221, 51, 51)");
  await row.click();
  await expect(page.locator("#reader h1 i.fa-gears")).toHaveCount(1);
  await expect(page.locator("#reader h1")).toContainText("Message #2");
});

test("gx opens the address a message link rule finds in the subject", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.keyboard.press("g");
  await page.keyboard.press("x");
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { __mock: { opened?: string[] } }).__mock.opened))
    .toEqual(["https://tracker.example.com/go/0"]);
});

test("HTML mail that names no font is set in a sans-serif", async ({ page }) => {
  await page.goto("/harness.html?view=html");
  await page.locator("#reader iframe").waitFor();
  const font = await page
    .frameLocator("#reader iframe")
    .locator("body")
    .evaluate((body) => getComputedStyle(body).fontFamily);
  expect(font).toContain("sans-serif");
});

test("a message with no body opens as empty text, not as a source that never comes", async ({ page }) => {
  await page.goto("/harness.html?view=html");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await page
    .locator("#list .rows .row")
    .filter({ has: page.locator(".subject .text", { hasText: /^Message #3$/ }) })
    .click();
  await expect(page.locator("#reader")).toContainText("This message has no text");
  await expect(page.locator("#reader")).not.toContainText("Fetching the source");
});

/** The calls made to `cmd`, by their arguments, in the order they were made. */
async function callsTo(page: Page, cmd: string): Promise<Record<string, unknown>[]> {
  return page.evaluate(
    (cmd) =>
      ((window as unknown as { __mock: { calls?: [string, Record<string, unknown>][] } }).__mock.calls ?? [])
        .filter(([name]) => name === cmd)
        .map(([, args]) => args),
    cmd,
  );
}
async function openArchive(page: Page, query = ""): Promise<void> {
  await page.goto(`/harness.html${query}`);
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  // The inbox starts with a "Message #0" too; the Archive is the long list.
  await expect(page.locator("#list .rows .row").nth(11)).toBeVisible();
  await expect(page.locator("#list .rows .row.cursor .subject .text")).toHaveText("Message #0");
  await expect(page.locator("#reader h1")).toContainText("Message #0");
  await page.locator("#list").focus();
}
const cursorSubject = (page: Page) => page.locator("#list .rows .row.cursor .subject .text");

test("a message whose text is stored is read from it at once, and its HTML follows when the server answers", async ({ page }) => {
  await openArchive(page, "?textfirst=1");
  await page.evaluate(() => ((window as any).__mock.delay.message_complete = 2500));
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  await expect(cursorSubject(page)).toHaveText("Message #2");
  // The text that is stored, before the server has answered.
  await expect(page.locator("#reader pre")).toContainText("Body of message 2", { timeout: 2000 });
  await expect(page.locator("#reader iframe")).toHaveCount(0);
  // Then the HTML, in its place.
  await expect(page.locator("#reader iframe")).toHaveCount(1, { timeout: 8000 });

  // An answer for a message the cursor has left is not drawn.
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  await expect(cursorSubject(page)).toHaveText("Message #4");
  await expect(page.locator("#reader pre")).toContainText("Body of message 4", { timeout: 2000 });
  await page.keyboard.press("j");
  await expect(page.locator("#reader h1")).toContainText("Message #5");
  await page.waitForTimeout(3000);
  await expect(page.locator("#reader h1")).toContainText("Message #5");
  await expect(page.locator("#reader iframe")).toHaveCount(0);
});

test("Enter on a message whose body is slow shows it at once, and keys typed after it are not held up", async ({ page }) => {
  await openArchive(page);
  // A server that takes its time, as a patchy connection does.
  await page.evaluate(() => ((window as any).__mock.delay.message = 4000));
  await page.keyboard.press("j");
  await expect(cursorSubject(page)).toHaveText("Message #1");
  await page.keyboard.press("Enter");
  await expect(page.locator("#reader")).toHaveClass(/focused/, { timeout: 2000 });
  await expect(page.locator("#reader h1")).toContainText("Message #1", { timeout: 2000 });
  await expect(page.locator("#reader .fetching")).toBeVisible();
  // While it loads, nothing acts on the message read before it.
  await page.keyboard.press("h");
  await page.keyboard.press("j");
  await expect(cursorSubject(page)).toHaveText("Message #2", { timeout: 2000 });
  // The body lands for the message it was asked for, or not at all.
  await page.evaluate(() => ((window as any).__mock.delay.message = 8));
  await expect(page.locator("#reader h1")).toContainText("Message #2");
  await expect(page.locator("#reader .fetching")).toHaveCount(0);
  await expect(page.locator("#reader .h-views, #reader iframe, #reader pre").first()).toBeVisible();
});

test("a plain-text body sets its signature apart, up to the quote below it", async ({ page }) => {
  await openArchive(page);
  for (let i = 0; i < 5; i++) await page.keyboard.press("j");
  await expect(page.locator("#reader h1")).toContainText("Message #5");
  const signature = page.locator("#reader pre .sig");
  await expect(signature).toHaveText("-- \nSender\nhttps://example.com/sender\n");
  await expect(signature.locator("a")).toHaveCount(1);
  await expect(page.locator("#reader pre")).toContainText("On Monday, Me wrote:\n> earlier");
  const colour = (selector: string) =>
    page.locator(selector).evaluate((e) => getComputedStyle(e).color);
  expect(await colour("#reader pre .sig")).not.toBe(await colour("#reader pre"));
});

test("a key pressed while the one before it waits on the core runs after it, on what it left", async ({ page }) => {
  await openArchive(page);
  // A server slow to mark read: `A` is `stage-read && mark-move Archive && next-message`.
  await page.evaluate(() => {
    (window as unknown as { __mock: { delay: Record<string, number> } }).__mock.delay.mark_read = 400;
  });
  await page.keyboard.press("Shift+A");
  await page.keyboard.press("j");
  await expect(cursorSubject(page)).toHaveText("Message #2");
  await expect.poll(async () => (await callsTo(page, "stage_action")).map((a) => a.uids)).toEqual([[0]]);
});

test("advance moves up after a stage when the message below is staged and the one above is not", async ({ page }) => {
  await openArchive(page);
  // `Z` is `mark-delete && advance`.
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  await page.keyboard.press("Shift+Z");
  await expect(cursorSubject(page)).toHaveText("Message #3");
  await page.keyboard.press("k");
  await page.keyboard.press("k");
  await page.keyboard.press("Shift+Z");
  await expect(cursorSubject(page)).toHaveText("Message #0");
  await expect.poll(async () => (await callsTo(page, "stage_action")).map((a) => a.uids)).toEqual([[2], [1]]);
});

test("the same action typed twice acts on two messages, one after the other", async ({ page }) => {
  await openArchive(page);
  await page.evaluate(() => {
    (window as unknown as { __mock: { delay: Record<string, number> } }).__mock.delay.mark_read = 300;
  });
  await page.keyboard.press("Shift+A");
  await page.keyboard.press("Shift+A");
  await expect(cursorSubject(page)).toHaveText("Message #2");
  await expect.poll(async () => (await callsTo(page, "stage_action")).map((a) => a.uids)).toEqual([[0], [1]]);
});

test("the reader shows the message under the cursor or that it is loading it, never another", async ({ page }) => {
  await openArchive(page);
  const delay = (ms: number) =>
    page.evaluate((ms) => {
      (window as unknown as { __mock: { delay: Record<string, number> } }).__mock.delay.message = ms;
    }, ms);
  await delay(700);
  await page.keyboard.press("j");
  // At once: the new message's headers and a loading line, not the old body.
  await expect(page.locator("#reader h1")).toContainText("Message #1", { timeout: 100 });
  await expect(page.locator("#reader .fetching")).toBeVisible();
  await expect(page.locator("#reader")).not.toContainText("Body of message 0");
  // The body for #1 is still on its way when the cursor moves on, and #2's
  // arrives first: #1's, arriving late, must not replace it.
  await page.waitForTimeout(200);
  await delay(20);
  await page.keyboard.press("j");
  await expect(page.locator("#reader iframe")).toHaveCount(1);
  await page.waitForTimeout(800);
  await expect(page.locator("#reader h1")).toContainText("Message #2");
  await expect(page.locator("#reader iframe")).toHaveCount(1);
  await expect(page.locator("#reader")).not.toContainText("Body of message 1");
  await expect(cursorSubject(page)).toHaveText("Message #2");
});

test("mail arriving above the cursor while a key is pressed neither moves the action nor the reader", async ({ page }) => {
  await openArchive(page);
  for (let i = 0; i < 5; i++) await page.keyboard.press("j");
  await expect(cursorSubject(page)).toHaveText("Message #5");
  await expect(page.locator("#reader h1")).toContainText("Message #5");
  // Three new messages arrive and the list is refreshed slowly; `d` is
  // pressed while the new rows are on their way.
  await page.evaluate(() => {
    const w = window as unknown as {
      __mock: { delay: Record<string, number>; arrived?: number };
      __emit: (event: string, payload: unknown) => void;
    };
    // The rows come quickly and the count slowly, so the key falls between
    // the rows changing and the refresh ending.
    w.__mock.delay.folder_total = 400;
    w.__mock.arrived = 3;
    w.__emit("folder-changed", ["Work", "Archive"]);
  });
  await page.waitForTimeout(150);
  await page.keyboard.press("d");
  await page.waitForTimeout(700);
  const staged = await callsTo(page, "stage_action");
  expect(staged.map((a) => a.uids)).toEqual([[5]]);
  await expect(cursorSubject(page)).toHaveText("Message #5");
  await expect(page.locator("#reader h1")).toContainText("Message #5");
});

test("a task made from a message goes to Today without leaving the mail, and can be filed in the same key", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  // The window lands in Today first.
  await expect(page.locator("#list")).toHaveClass(/task-view/);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  // Out of Today, and the Archive's own rows in.
  await expect(page.locator("#list")).not.toHaveClass(/task-view/);
  await expect(page.locator("#list .rows .row .subject .text", { hasText: /^Message #5$/ })).toBeVisible();
  await expect(cursorSubject(page)).toHaveText("Message #0");
  await page.locator("#list").focus();
  await expect(page.locator("#list")).toHaveClass(/focused/);
  // `y` is `create-task-from-message && stage-read && mark-move Archive && next-message`.
  await page.keyboard.press("y");
  await page.keyboard.press("j");
  await expect(cursorSubject(page)).toHaveText("Message #2");
  expect((await callsTo(page, "create_tasks_from_messages")).map((a) => a.messages)).toEqual([[0]]);
  const created = await callsTo(page, "create_tasks_from_messages");
  expect(created[0]!.postpone).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  await expect.poll(async () => (await callsTo(page, "stage_action")).map((a) => a.uids)).toEqual([[0]]);
  // One key, one line: every step says what it did, and one undo covers all.
  await expect(page.locator("#status")).toHaveText(
    'created task "Message #0" in Today · marked 1 message read · ⌘Z to undo',
  );
  await expect(page.locator("#reader h1")).toContainText("Message #2");

  await page.keyboard.press("Meta+z");
  await expect(page.locator("#status")).toContainText("undone:");
  expect((await callsTo(page, "trash_tasks")).map((a) => a.ids)).toHaveLength(1);
  const unread = (await callsTo(page, "mark_read")).filter((a) => a.read === false);
  expect(unread.map((a) => a.uids)).toEqual([[0]]);
});

test("a task made from a message in Today is in Today's tasks before the chain files the message", async ({ page }) => {
  await page.clock.setFixedTime(new Date("2026-08-07T10:00:00"));
  await page.goto("/harness.html?tasks=1");
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await page.locator("#list [data-task]").first().waitFor();
  await page.locator("#list").focus();
  await expect(cursorSubject(page)).toHaveText("Message #0");
  // The stub announces no `tasks-changed`: the command draws what it made itself,
  // rather than leaving the list to an event that may come later.
  await page.keyboard.press("y");
  await expect(page.locator("#status")).toContainText('created task "Message #0" in Today');
  await expect(page.locator("#list [data-task]", { hasText: "Message #0" })).toHaveCount(1);
  // Drawn before the next step ran, not after it.
  const order = await page.evaluate(() =>
    ((window as any).__mock.calls as [string][]).map(([name]) => name).filter((name) => ["create_tasks_from_messages", "list_tasks", "stage_action"].includes(name)),
  );
  const created = order.indexOf("create_tasks_from_messages");
  expect(order.indexOf("list_tasks", created)).toBeLessThan(order.indexOf("stage_action"));
});

test("undo in a task list takes back the last mail action too, and redo does it again", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  // The startup sync says what it did in the status line; it has to be over.
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await expect(page.locator("#list")).toHaveClass(/task-view/);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await expect(cursorSubject(page)).toHaveText("Message #0");
  await page.locator("#list").focus();
  await page.keyboard.press("!");
  await expect(page.locator("#status")).toContainText("⌘Z to undo");
  await page.locator("#folders .sb-row", { hasText: "Upcoming" }).click();
  await expect(page.locator("#list")).toHaveClass(/task-view/);
  await page.locator("#list").focus();
  await page.keyboard.press("u");
  // Message #0 came flagged, so `!` unflagged it and undo flags it again.
  await expect(page.locator("#status")).toHaveText("undone: unflag 1 message");
  const flags = (await callsTo(page, "mark_flagged")).map((a) => a.flagged);
  expect(flags).toEqual([false, true]);
  await page.keyboard.press("Meta+Shift+z");
  await expect(page.locator("#status")).toHaveText("redone: unflag 1 message");
});

test("Ctrl-T asks for the date first, and dismissing the picker stops the chain before it files the message", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await expect(page.locator("#list")).toHaveClass(/task-view/);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await expect(page.locator("#list")).not.toHaveClass(/task-view/);
  await expect(page.locator("#list .rows .row .subject .text", { hasText: /^Message #5$/ })).toBeVisible();
  await expect(cursorSubject(page)).toHaveText("Message #0");
  await page.locator("#list").focus();
  await expect(page.locator("#list")).toHaveClass(/focused/);

  // `Y` is `y` with `postpone`; escaping the picker makes nothing and files nothing.
  await page.keyboard.press("Shift+Y");
  await expect(page.locator(".dp-title")).toHaveText("Postpone until");
  await page.keyboard.press("Escape");
  await expect(page.locator(".dp")).toHaveCount(0);
  await page.waitForTimeout(100);
  expect(await callsTo(page, "create_tasks_from_messages")).toHaveLength(0);
  expect(await callsTo(page, "stage_action")).toHaveLength(0);
  await expect(cursorSubject(page)).toHaveText("Message #0");

  // The built-in `<c-t>` makes the task on the day picked, and leaves the mail where it is.
  await page.keyboard.press("Control+t");
  await page.locator(".dp-input").fill("thu");
  await expect(page.locator(".dp-list li").first()).toContainText("20 Aug");
  await page.keyboard.press("Enter");
  await expect.poll(async () => (await callsTo(page, "create_tasks_from_messages")).map((a) => a.postpone)).toEqual([
    "2026-08-20",
  ]);
  await expect(page.locator("#status")).toContainText('created task "Message #0" for 2026-08-20');
  expect(await callsTo(page, "stage_action")).toHaveLength(0);
});

test("Ctrl-T's picker switches to a due date with Ctrl-E, and the task is made due that day", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await expect(cursorSubject(page)).toHaveText("Message #0");
  await page.locator("#list").focus();

  await page.keyboard.press("Control+t");
  await expect(page.locator(".dp-title")).toHaveText("Postpone until");
  await expect(page.locator(".dp-foot")).toContainText("due");
  await page.keyboard.press("Control+e");
  await expect(page.locator(".dp-title")).toHaveText("Due");
  await expect(page.locator(".dp-foot")).toContainText("postpone until");
  // Switched back and forth, it lands where it started; once more, on Due.
  await page.keyboard.press("Control+e");
  await expect(page.locator(".dp-title")).toHaveText("Postpone until");
  await page.keyboard.press("Control+e");
  await page.locator(".dp-input").fill("thu");
  await expect(page.locator(".dp-list li").first()).toContainText("20 Aug");
  await page.keyboard.press("Enter");
  await expect
    .poll(async () => (await callsTo(page, "create_tasks_from_messages")).map((a) => [a.postpone, a.due]))
    .toEqual([[null, "2026-08-20"]]);
  await expect(page.locator("#status")).toContainText('created task "Message #0" due 2026-08-20');
});

test("a task says when macOS blocks its reminders, and the notice leaves once they are allowed", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await intoTasks(page);
  await page.locator("#reader .tk-head").waitFor();
  await expect(page.locator("#reader .tk-blocked")).toHaveCount(0);
  const permit = (permission: string) =>
    page.evaluate((permission) => {
      (window as unknown as { __mock: { notifications: string } }).__mock.notifications = permission;
      window.dispatchEvent(new Event("focus"));
    }, permission);

  await permit("denied");
  await expect(page.locator("#reader .tk-blocked")).toContainText("macOS blocks Orb's notifications");
  await page.locator("#reader .tk-blocked button").click();
  await expect.poll(async () => (await callsTo(page, "open_notification_settings")).length).toBe(1);

  await permit("granted");
  await expect(page.locator("#reader .tk-blocked")).toHaveCount(0);
});

test("the line between Today's tasks and mail can be dragged, is kept, and resets on a double-click", async ({ page }) => {
  await page.setViewportSize({ width: 1100, height: 800 });
  await page.goto("/harness.html?tasks=1");
  await expect(page.locator("#list")).toHaveClass(/task-view/);
  await settled(page);
  const split = page.locator("#list [data-tk-split]");
  const section = page.locator("#list .tk-section");
  const list = (await box(page.locator("#list")));
  const fitted = (await box(section)).height;

  const from = (await box(split));
  await page.mouse.move(from.x + 50, from.y + 2);
  await page.mouse.down();
  // The tasks are below the line: dragging it up gives them more.
  await page.mouse.move(from.x + 50, list.y + list.height * 0.3, { steps: 5 });
  await page.mouse.up();
  const dragged = (await box(section)).height;
  expect(dragged).toBeGreaterThan(list.height * 0.6);

  // Kept across a reload, as a share of the pane.
  await page.reload();
  await expect(page.locator("#list")).toHaveClass(/task-view/);
  await settled(page);
  expect(Math.abs((await box(section)).height - dragged)).toBeLessThan(3);

  await split.dblclick();
  expect(Math.abs((await box(section)).height - fitted)).toBeLessThan(3);
});

test("the line between Today's columns can be dragged, is kept, and resets on a double-click", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await chooseInToday(page, '[data-today-layout="columns"]');
  const split = page.locator("#list [data-tk-vsplit]");
  const mail = page.locator("#list .mail-scroll");
  const list = (await box(page.locator("#list")));
  const usual = (await box(mail)).width;
  expect(usual / list.width).toBeGreaterThan(0.55);

  const from = (await box(split));
  await page.mouse.move(from.x, from.y + 100);
  await page.mouse.down();
  await page.mouse.move(list.x + list.width * 0.4, from.y + 100, { steps: 5 });
  await page.mouse.up();
  const dragged = (await box(mail)).width;
  expect(Math.abs(dragged - list.width * 0.4)).toBeLessThan(6);

  // Kept across a reload, as a share of the pane.
  await page.reload();
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  expect(Math.abs((await box(mail)).width - dragged)).toBeLessThan(3);

  await split.dblclick();
  expect(Math.abs((await box(mail)).width - usual)).toBeLessThan(3);
});

test("the search field floats over the window only while it is used", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  const overlay = page.locator("#search-overlay");
  await expect(overlay).toBeHidden();

  await page.keyboard.press("/");
  await expect(overlay).toBeVisible();
  await expect(page.locator("#search-input")).toBeFocused();

  await page.keyboard.press("Escape");
  await expect(overlay).toBeHidden();

  await page.keyboard.press("/");
  await page.mouse.click(10, 790);
  await expect(overlay).toBeHidden();
  // Back on the list, the keymap has the keyboard again.
  await page.keyboard.press("/");
  await expect(overlay).toBeVisible();
});

test("search results are by relevance until another order is picked", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type("message #1");
  await page.keyboard.press("Enter");
  const sort = page.locator("#toolbar .sortbtn");
  const first = page.locator("#list .rows .row").first();
  await expect(sort).toHaveText("Relevance");
  await expect(first).toContainText("Sender 1 ");

  await sort.click();
  await page.locator('#toolbar .menu [data-by="from"]').click();
  await expect(sort).toHaveText("Sender ↑");
  await sort.click();
  await page.locator('#toolbar .menu [data-dir="-1"]').click();
  await expect(sort).toHaveText("Sender ↓");
  await expect(first).toContainText("Sender 19");
  const sent = await page.evaluate(() =>
    (window as unknown as { __mock: { calls: [string, { sort?: string }][] } }).__mock.calls
      .filter(([cmd]) => cmd === "search")
      .map(([, args]) => args.sort ?? null),
  );
  expect(sent.slice(-2)).toEqual(["SenderAsc", "SenderDesc"]);
  await expect(page.locator("#toolbar .search.on")).toBeVisible();

  await page.locator("#list").focus();
  await page.keyboard.type(":sort-by relevance");
  await page.keyboard.press("Enter");
  await expect(sort).toHaveText("Relevance");
  await expect(first).toContainText("Sender 1 ");
});

test("search.sort starts every search in its order, but one by meaning", async ({ page }) => {
  await page.goto("/harness.html?searchsort=date_newest");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  const sorts = () =>
    page.evaluate(() =>
      (window as unknown as { __mock: { calls: [string, { query: string; sort?: string | null }][] } }).__mock.calls
        .filter(([cmd]) => cmd === "search")
        .map(([, args]) => [args.query, args.sort ?? null]),
    );
  const search = async (query: string) => {
    await page.keyboard.press("/");
    await page.locator("#search-input").fill(query);
    await page.keyboard.press("Enter");
    await expect.poll(async () => (await sorts()).at(-1)?.[0]).toBe(query);
    await expect(page.locator("#toolbar .search.on")).toBeVisible();
  };

  await search("message #1");
  await expect(page.locator("#toolbar .sortbtn")).toHaveText("Date ↓");
  expect((await sorts()).at(-1)).toEqual(["message #1", "DateNewest"]);

  await search("~message");
  await expect(page.locator("#toolbar .sortbtn")).toHaveText("Relevance");
  expect((await sorts()).at(-1)).toEqual(["~message", null]);
});

test("Enter on a sender typed in part searches for it as typed, not for the first person offered", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type("f:sar");
  const first = page.locator("#search-suggest .sp-i").first();
  await expect(first).toContainText("From contains “sar”");
  await expect(page.locator("#search-suggest .sp-i")).toHaveCount(3);
  await page.keyboard.press("Enter");
  const queries = await page.evaluate(() =>
    (window as unknown as { __mock: { calls: [string, { query: string }][] } }).__mock.calls
      .filter(([cmd]) => cmd === "search")
      .map(([, args]) => args.query),
  );
  expect(queries.at(-1)).toBe("f:sar");
});

test("the auto date shows the year of mail from another year, and the time where it fits", async ({ page }) => {
  // The harness's mail is from the first days of 2026.
  await page.clock.setFixedTime(new Date("2027-03-01T10:00:00"));
  await page.goto("/harness.html?datefmt=auto");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  const date = page.locator("#list .rows .row .date").first();
  // In whatever zone the test runs: west of Greenwich the mail is from 2025.
  await expect(date).toHaveText(/^\d+ \w{3} 202[56] \d\d:\d\d$/);

  // Narrowed, the time goes before the year does.
  await page.locator("#list").evaluate((list) => list.style.setProperty("--col-date", "70px"));
  await page.keyboard.press("j");
  await expect(date).toHaveText(/^(\d+ \w{3}|\w{3}) 202[56]$/);
});

test("Esc in the search field leaves the search unless the query was changed", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type("message #1");
  await page.keyboard.press("Enter");
  const searching = page.locator("#toolbar .search.on");
  await expect(searching).toBeVisible();

  // Changed, Escape abandons the change and keeps the results.
  await page.keyboard.press("/");
  await expect(page.locator("#search-input")).toHaveValue("message #1");
  await page.keyboard.type("2");
  await page.keyboard.press("Escape");
  await expect(page.locator("#search-overlay")).toBeHidden();
  await expect(searching).toBeVisible();

  // Untouched, it leaves the search.
  await page.keyboard.press("/");
  await page.keyboard.press("Escape");
  await expect(searching).toBeHidden();
});

test("Esc on the result list leaves the search once nothing is selected", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type("message #1");
  await page.keyboard.press("Enter");
  const searching = page.locator("#toolbar .search.on");
  await expect(searching).toBeVisible();
  await expect(page.locator("#search-input")).not.toBeFocused();

  await page.keyboard.press(" ");
  await page.keyboard.press("Escape");
  await expect(searching).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(searching).toBeHidden();
});

test("a search from Today finds tasks in every view, grouped by when they come up", async ({ page }) => {
  // The day the fixtures' dates are written against.
  await page.clock.setFixedTime(new Date("2026-08-07T10:00:00"));
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type("renew");
  await page.keyboard.press("Enter");

  // No message says "renew": the tasks have the list to themselves.
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Search");
  await expect(page.locator("#list .mail-scroll")).toHaveCount(0);
  const heads = page.locator("#list .tk-day");
  await expect(heads).toHaveText(["Today", "August"]);
  await expect(page.locator("#list .tk-row .tk-title")).toHaveText(["Renew the domain", "Renew the passport"]);
  await expect(page.locator("#list.tasks-active")).toHaveCount(1);

  // Esc goes back to where the search was made.
  await page.locator("#list").focus();
  await page.keyboard.press("Escape");
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Today");
  await expect(page.locator("#list .mail-scroll")).toHaveCount(1);
});

test("a search's completed tasks are folded until za opens them", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type("the");
  await page.keyboard.press("Enter");
  const folded = page.locator("#list .tk-day.folded");
  await expect(folded).toContainText("Completed");
  await expect(page.locator("#list .tk-row", { hasText: "File the taxes" })).toHaveCount(0);

  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("a");
  await expect(folded).toHaveCount(0);
  await expect(page.locator("#list .tk-row", { hasText: "File the taxes" })).toHaveCount(1);
  await folded.or(page.locator('#list .tk-day[data-group="done"]')).click();
  await expect(page.locator("#list .tk-row", { hasText: "File the taxes" })).toHaveCount(0);
});

test("a search that finds mail and tasks shows both, and gs shows one kind at a time", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  // A task made from a message shares its subject.
  await page.keyboard.press("t");
  await expect(page.locator("#status")).toContainText("created task");
  const [made] = await callsTo(page, "create_tasks_from_messages");
  const title = `Message #${(made!.messages as number[])[0]}`;

  await page.locator("#list").focus();
  await page.keyboard.press("/");
  await page.keyboard.type(title);
  await page.keyboard.press("Enter");
  const kinds = page.locator("#toolbar .seg.kinds button");
  await expect(kinds).toHaveText([/All\s+\d+/, /Mail\s+\d+/, /Tasks\s+1/]);
  await expect(page.locator("#list .mail-scroll .rows .row").first()).toBeVisible();
  await expect(page.locator("#list .tk-section .tk-row")).toHaveCount(1);
  await expect(page.locator("#list.mail-active")).toHaveCount(1);

  await page.locator("#list").focus();
  await page.keyboard.press("g");
  await page.keyboard.press("s");
  await expect(kinds.nth(1)).toHaveAttribute("aria-checked", "true");
  await expect(page.locator("#list .tk-section")).toHaveCount(0);
  await page.keyboard.press("g");
  await page.keyboard.press("s");
  await expect(kinds.nth(2)).toHaveAttribute("aria-checked", "true");
  await expect(page.locator("#list .mail-scroll")).toHaveCount(0);
  await expect(page.locator("#list .tk-row")).toHaveCount(1);
  await kinds.nth(0).click();
  await expect(page.locator("#list .mail-scroll")).toHaveCount(1);
});

test("the search field's switch picks mail or tasks before searching, and is remembered", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("t");
  await expect(page.locator("#status")).toContainText("created task");
  const [made] = await callsTo(page, "create_tasks_from_messages");
  const title = `Message #${(made!.messages as number[])[0]}`;

  await page.locator("#list").focus();
  await page.keyboard.press("/");
  const kind = page.locator("#search-kind button");
  await expect(kind).toHaveText(["Mail", "Tasks", "All"]);
  await expect(kind.nth(2)).toHaveAttribute("aria-checked", "true");
  await page.keyboard.press("Control+t");
  await page.keyboard.press("Control+t");
  await expect(kind.nth(1)).toHaveAttribute("aria-checked", "true");
  // Tasks are in no folder.
  await expect(page.locator("#search-scope")).toBeHidden();
  await page.keyboard.type(title);
  await page.keyboard.press("Enter");
  await expect(page.locator("#toolbar .seg.kinds button").nth(2)).toHaveAttribute("aria-checked", "true");
  await expect(page.locator("#list .mail-scroll")).toHaveCount(0);
  await expect(page.locator("#list .tk-row")).toHaveCount(1);

  // Next time, and after a restart, the field opens on Tasks.
  await page.reload();
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await page.locator("#list").focus();
  await page.keyboard.press("/");
  await expect(kind.nth(1)).toHaveAttribute("aria-checked", "true");
  await kind.nth(0).click();
  await expect(kind.nth(0)).toHaveAttribute("aria-checked", "true");
  await expect(page.locator("#search-scope")).toBeVisible();
  await expect(page.locator("#search-input")).toBeFocused();
});

test("F filters the folder on screen, and Esc takes the filter off", async ({ page }) => {
  await openArchive(page);
  await settled(page);
  await page.keyboard.press("F");
  const input = page.locator("#list .fbar input");
  await expect(input).toBeFocused();
  await page.keyboard.type("#150");
  await expect(page.locator("#list .rows .row .subject .text")).toHaveText(["Message #150"]);
  await expect(page.locator("#list .fbar .cnt")).toHaveText("1 match");
  const [asked] = (await callsTo(page, "filter_messages")).slice(-1);
  expect(asked!.folders).toEqual([["Work", "Archive"]]);
  // The filter stays on the folder, not in a search.
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Archive");

  // Enter gives the list the keyboard and keeps the filter.
  await page.keyboard.press("Enter");
  await expect(page.locator("#list")).toBeFocused();
  await expect(input).toHaveValue("#150");
  await page.keyboard.press("Escape");
  await expect(page.locator("#list .fbar")).toHaveCount(0);
  await expect(page.locator("#list .rows .row").nth(11)).toBeVisible();
});

test("a filter in Today narrows its mail and its tasks, and is gone in another view", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("F");
  await page.keyboard.type("renew");
  await expect(page.locator("#list .tk-section .tk-row .tk-title")).toHaveText(["Renew the domain"]);
  await expect(page.locator("#list .mail-scroll .empty")).toHaveText("No mail matches");
  const [asked] = (await callsTo(page, "list_tasks")).slice(-1);
  expect(asked!.search).toBe("renew");

  await page.locator("#folders .sb-row", { hasText: "Upcoming" }).click();
  await expect(page.locator("#list .fbar")).toHaveCount(0);
  await expect(page.locator("#list .tk-row .tk-title")).toContainText(["Renew the passport"]);
});

test("a filter narrows search results by what their rows show, and the search stays", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type("message");
  await page.keyboard.press("Enter");
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Search");
  await page.locator("#list").focus();
  await page.keyboard.press("F");
  await page.keyboard.type("#3");
  await expect(page.locator("#list .rows .row")).toHaveCount(11);
  expect(await callsTo(page, "filter_messages")).toEqual([]);
  await page.keyboard.press("Escape");
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Search");
  await expect(page.locator("#list .fbar")).toHaveCount(0);
});

test("the filter bar spans Today's columns and leaves them side by side", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await chooseInToday(page, '[data-today-layout="columns"]');
  await page.locator("#list").focus();
  await page.keyboard.press("F");
  const bar = (await box(page.locator("#list .fbar")));
  const mail = (await box(page.locator("#list .mail-scroll")));
  const tasks = (await box(page.locator("#list .tk-section")));
  expect(mail.y).toBeGreaterThanOrEqual(bar.y + bar.height);
  expect(Math.abs(mail.y - tasks.y)).toBeLessThan(2);
  expect(tasks.x).toBeGreaterThan(mail.x + mail.width - 2);
  expect(bar.width).toBeGreaterThan(mail.width + tasks.width - 40);
});

test("a quoted value stays one term in the search field", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type('s:"devel pass');
  // Inside the quotes the space is part of the value, not the end of a word.
  await expect(page.locator("#search-suggest .sp-i")).toHaveCount(0);
  await page.keyboard.type('wort" ');
  await expect(page.locator("#search-chips .tokn")).toHaveText(["sdevel passwort"]);
  await page.keyboard.press("Enter");
  // The first search; nothing matches it here, so one by meaning follows.
  await expect.poll(async () => (await callsTo(page, "search")).at(0)?.query).toBe('s:"devel passwort"');

  // Opened again over the results, the field gives the term back whole.
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Search");
  await page.locator("#list").focus();
  await page.keyboard.press("/");
  await expect(page.locator("#search-chips .tokn")).toHaveText(["sdevel passwort"]);
  await expect(page.locator("#search-input")).toHaveValue("");
});

test("a filter that keeps nothing says so and offers to clear it", async ({ page }) => {
  await openArchive(page);
  await settled(page);
  await page.keyboard.press("F");
  await page.keyboard.type("nothing like this");
  await expect(page.locator("#list .empty, #list .empty-view").first()).toContainText("Nothing matches");
  await page.keyboard.press("Escape");
  await expect(page.locator("#list .rows .row").first()).toBeVisible();
});

test("the search field turns operators into chips and offers what a word could mean", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.keyboard.press("/");
  await page.keyboard.type("is:unread ");
  const chips = page.locator("#search-chips .tokn");
  await expect(chips).toHaveText(["isunread"]);
  await expect(page.locator("#search-input")).toHaveValue("");

  await page.keyboard.type("sara");
  const items = page.locator("#search-suggest .sp-i");
  await expect(items.first()).toContainText("anywhere");
  await expect(items.filter({ hasText: "From Sara Brunner" })).toHaveCount(1);
  await expect(items.filter({ hasText: "Subject contains" })).toHaveCount(1);
  await expect(page.locator("#search-ops")).toContainText("f: from");

  // Down to the sender, Tab makes it a chip, Enter searches.
  const sender = await items.allTextContents();
  const at = sender.findIndex((t) => t.includes("From Sara Brunner"));
  for (let i = 0; i < at; i++) await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Tab");
  await expect(chips).toHaveText(["isunread", "fsara.brunner@example.com"]);
  await expect(page.locator("#search-input")).toHaveValue("");

  // Backspace at the start turns the last chip back into text.
  await page.keyboard.press("Backspace");
  await expect(chips).toHaveText(["isunread"]);
  await expect(page.locator("#search-input")).toHaveValue("f:sara.brunner@example.com");
  await page.keyboard.press("End");
  await page.keyboard.press("Enter");
  // Asked as written; finding nothing, it is asked again by meaning.
  await expect
    .poll(async () => (await callsTo(page, "search")).map((c) => c.query))
    .toContain("is:unread f:sara.brunner@example.com");
});

test("a search narrowed to this folder or to folders picked finds mail only", async ({ page }) => {
  await openArchive(page);
  await settled(page);
  await page.keyboard.press("/");
  const pills = page.locator("#search-scope .pill");
  await expect(pills.first()).toHaveText("Everywhere");
  await expect(pills.nth(1)).toHaveText("This folder · Archive");
  await page.keyboard.press("Control+f");
  await expect(pills.nth(1)).toHaveClass(/on/);
  await page.keyboard.type("message");
  await page.keyboard.press("Enter");
  await expect.poll(async () => (await callsTo(page, "search")).at(-1)?.scope).toEqual({
    Folders: { folders: [["Work", "Archive"]] },
  });
  expect(await callsTo(page, "search_tasks")).toEqual([]);
  await expect(page.locator("#toolbar .tb-s")).toContainText("in Archive");

  // Folders… ticks folders into chips.
  await page.locator("#list").focus();
  await page.keyboard.press("/");
  await pills.filter({ hasText: "Folders…" }).click();
  const rows = page.locator("#search-suggest [data-pick]");
  await rows.filter({ hasText: "INBOX" }).first().click();
  await expect(page.locator("#search-chips .tokn.scope")).toHaveCount(2);
  await page.locator("#search-input").focus();
  await page.keyboard.press("Enter");
  await expect.poll(async () => ((await callsTo(page, "search")).at(-1)?.scope as { Folders: { folders: unknown[] } }).Folders.folders.length).toBe(2);
});

test("inboxes emptied by an expunge leave no placeholder rows in Today", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await expect(page.locator(".mail-scroll .rows .row .subject .text").first()).toBeVisible();
  await page.evaluate(() => {
    (window as unknown as { __mock: Record<string, unknown> }).__mock.inboxEmpty = true;
  });
  await page.keyboard.press(":");
  await page.keyboard.type("expunge-all");
  await page.keyboard.press("Enter");
  await expect.poll(() => callsTo(page, "execute_pending_all")).toHaveLength(1);
  await expect(page.locator(".mail-scroll .rows .row")).toHaveCount(0);
});

test("archiving with a moves the cursor without waiting for the backend", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await expect(cursorSubject(page)).toHaveText("Message #0");
  await page.locator("#list").focus();
  await page.evaluate(() => {
    const mock = (window as unknown as { __mock: { delay: Record<string, number> } }).__mock;
    for (const cmd of ["mark_read", "stage_action", "pending_counts"]) mock.delay[cmd] = 1500;
  });
  // `A` is `stage-read && mark-move Archive && next-message`.
  await page.keyboard.press("A");
  await expect(cursorSubject(page)).toHaveText("Message #1", { timeout: 300 });
  // Applying waits for the staged write rather than racing past it.
  await page.keyboard.press("x");
  await expect.poll(() => callsTo(page, "execute_pending"), { timeout: 5000 }).toHaveLength(1);
  const order = await page.evaluate(() =>
    (window as unknown as { __mock: { calls: [string][] } }).__mock.calls
      .map(([cmd]) => cmd)
      .filter((cmd) => cmd === "stage_action" || cmd === "execute_pending"),
  );
  expect(order).toEqual(["stage_action", "execute_pending"]);
});

test("an empty Today stands alone and offers the keys that lead on", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&empty=1");
  await page.locator("#list .empty-view").waitFor();
  await settled(page);
  await expect(page.locator("#list .empty-view h3")).toHaveText("Nothing left for today");
  await expect(page.locator("#list .empty-view .leads button")).toHaveText(["oNew task", "CCompose"]);
  await expect(page.locator("#reader")).toBeHidden();
  // A new task fills the list, and the reader comes back to edit it in.
  await page.locator("#list .empty-view [data-empty-command=create-task]").click();
  await expect(page.locator("#list [data-task]")).toHaveCount(1);
  await expect(page.locator("#reader")).toBeVisible();
});

test("Esc while naming a new task discards it and hands the keyboard back to the list", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  const rows = page.locator("#list [data-task]");
  const before = await rows.count();
  await page.keyboard.press("o");
  await expect(page.locator("#tk-title-input")).toBeFocused();
  await expect(rows).toHaveCount(before + 1);
  await page.keyboard.press("Escape");
  await expect(page.locator("#status")).toHaveText("discarded");
  await expect(rows).toHaveCount(before);
  await expect(page.locator("#tk-title-input")).toHaveCount(0);
  await expect(page.locator("#list")).toBeFocused();
  // The keys work again at once.
  await page.keyboard.press("j");
  await expect(page.locator("#list .tk-row.cursor")).toHaveCount(1);
});

test("a new task opens its title and its notes together", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("o");
  await expect(page.locator("#tk-title-input")).toBeFocused();
  await expect(page.locator("#tk-body-input")).toBeVisible();
  await page.keyboard.type("Pack the bags");
  await page.keyboard.press("Tab");
  await expect(page.locator("#tk-body-input")).toBeFocused();
  await page.keyboard.type("Passports first.");
  await page.keyboard.press("Meta+s");
  await expect(page.locator("#tk-body-input")).toHaveCount(0);
  const edit = await page.evaluate(() =>
    (window as unknown as { __calls: { cmd: string; args: { edit?: unknown } }[] }).__calls
      .filter((c) => c.cmd === "update_task").at(-1)?.args.edit);
  expect(edit).toEqual({ title: "Pack the bags", body: "Passports first." });
});

test("New Task from a mail folder opens Today and edits the new task there", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .row.cursor").waitFor();
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await expect(page.locator("#list .rows .row.loading")).toHaveCount(0);
  await page.locator("#list").focus();
  await page.keyboard.type(":create-task");
  await page.keyboard.press("Enter");
  await expect(page.locator("#tk-title-input")).toBeFocused();
  await expect(page.locator("#status")).not.toContainText("went missing");
});

test("the Tasks heading says whether every task change is on the server", async ({ page }) => {
  type Sync = { __taskSync: unknown; __emit: (event: string, payload: unknown) => void };
  await page.addInitScript(() => {
    (window as unknown as Sync).__taskSync = { pending: 2, problem: null };
  });
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  const heading = page.locator("#folders .sb-section", { hasText: "Tasks" });
  await expect(heading.locator(".tk-unsaved")).toHaveText("2");

  const report = (status: unknown) =>
    page.evaluate((status) => {
      const w = window as unknown as Sync;
      w.__taskSync = status;
      w.__emit("task-sync", null);
    }, status);
  await report({ pending: 0, problem: null });
  await expect(heading.locator(".tk-saved")).toHaveText("saved");
  await expect(page.locator("#status-steady .warn", { hasText: "server" })).toHaveCount(0);

  await report({ pending: 1, problem: "the tasks folder 'Tasks' is taken" });
  await expect(heading.locator(".tk-unsaved.problem")).toHaveAttribute(
    "title",
    "1 change on this machine only: the tasks folder 'Tasks' is taken",
  );
  await expect(page.locator("#status-steady .warn", { hasText: "server" })).toHaveText(
    "1 task change not on the server",
  );
});

test("a new task says in its detail that it is not on the server yet", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await expect(page.locator('#reader .p[data-k="Saved"]')).toHaveCount(0);
  await page.locator("#list").focus();
  await page.keyboard.type(":create-task");
  await page.keyboard.press("Enter");
  await expect(page.locator("#tk-title-input")).toBeFocused();
  await expect(page.locator('#reader .p[data-k="Saved"]')).toHaveText("Not yet on the server");
});

test("an empty folder stands alone until a folder with mail is opened", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator("#folders .sb-row", { hasText: "2025" }).click();
  await expect(page.locator("#list .empty-view h3")).toHaveText("2025 is empty");
  await expect(page.locator("#list .empty-view kbd")).toHaveText(["C", "⌃s"]);
  await expect(page.locator("#reader")).toBeHidden();
  await page.locator("#folders .sb-row", { hasText: "Archive" }).click();
  await expect(page.locator("#list .row.cursor")).toBeVisible();
  await expect(page.locator("#reader")).toBeVisible();
});

/** The directions handed to the escalate command so far. */
async function escalated(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    (window as unknown as { __calls: { cmd: string; args: { direction: string } }[] }).__calls
      .filter((c) => c.cmd === "escalate")
      .map((c) => c.args.direction),
  );
}

test("Ctrl+H/J/K/L move between the panes, and out of Orb past the last one", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator("#list").focus();

  await page.keyboard.press("Control+h");
  await expect(page.locator("#folders")).toHaveClass(/focused/);
  await page.keyboard.press("Control+h");
  await expect.poll(() => escalated(page)).toEqual(["left"]);
  // The list and the reader both lie to the right of the folders; the list
  // first, since the keyboard last left it.
  await page.keyboard.press("Control+l");
  await expect(page.locator("#list")).toHaveClass(/focused/);
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await page.keyboard.press("Control+j");
  await expect.poll(() => escalated(page)).toEqual(["left", "down"]);
  // Back to the folders from the reader, and on to the reader again: the pane
  // left last wins the tie.
  await page.keyboard.press("Control+h");
  await expect(page.locator("#folders")).toHaveClass(/focused/);
  await page.keyboard.press("Control+l");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/focused/);
});

test("Ctrl-J and Ctrl-K walk Today's mail, tasks and reader, stacked or in columns", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await expect(page.locator("#list")).toHaveClass(/mail-active/);

  await page.keyboard.press("Control+j");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await expect(page.locator("#list")).toHaveClass(/focused/);
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await expect(page.locator("#reader")).toHaveClass(/task-detail/);
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await page.keyboard.press("Control+k");
  await expect.poll(() => escalated(page)).toEqual(["up"]);

  // Side by side, the tasks are to the right of the mail and the reader below
  // both.
  await page.keyboard.press("z");
  await page.keyboard.press("l");
  await page.locator("#list.today-columns").waitFor();
  await page.keyboard.press("Control+l");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("Control+h");
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  // The reader is below the mail column too.
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await expect(page.locator("#list")).toHaveClass(/focused/);

  // Maximized in columns, Ctrl-L crosses to the tasks and Ctrl-J opens the reader.
  await page.keyboard.press("z");
  await page.keyboard.press("m");
  await page.keyboard.press("Control+l");
  await expect(page.locator("#list .tk-section")).toBeVisible();
  await expect(page.locator("#list .mail-scroll")).toBeHidden();
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await expect(page.locator("#reader")).toBeVisible();
});

test("Today in columns keeps the mail inside the pane when the chat and reading were dragged tall, and Ctrl-J reaches the reader from it", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/harness.html?tasks=1&chat=1&reading=1");
  await page.evaluate(() => {
    localStorage.setItem("orb.todayChatHeight", "60%");
    localStorage.setItem("orb.todayReadingHeight", "60%");
  });
  await page.reload();
  await page.waitForFunction(() => (window as any).__mock?.answered?.includes("sync_all"));
  await chooseInToday(page, '[data-today-layout="columns"]');
  await chooseInToday(page, '[data-today-show="chat"]');
  await chooseInToday(page, '[data-today-show="reading"]');
  await page.locator("#list > .rd-section").waitFor();

  const list = (await page.locator("#list").boundingBox())!;
  const mail = (await page.locator("#list > .mail-scroll").boundingBox())!;
  expect(mail.y + mail.height).toBeLessThanOrEqual(list.y + list.height + 1);

  await page.locator("#list .mail-scroll .row").first().click();
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
});

test("Ctrl-H/J/K/L in Today go by where its sections are drawn, Waiting among them", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await chooseInToday(page, '[data-today-show="chat"]');
  await page.locator("#list .ch-section .ch-row").first().waitFor();
  await page.locator("#list").focus();

  // Stacked: the mail, the tasks, Waiting, then the reader below them all.
  await page.keyboard.press("Control+j");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("Control+j");
  await expect(page.locator("#list")).toHaveClass(/chat-active/);
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/chat-active/);

  // In columns, Waiting is below the tasks to the right of the mail.
  await chooseInToday(page, '[data-today-layout="columns"]');
  await page.locator("#list.today-columns").waitFor();
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("Control+j");
  await expect(page.locator("#list")).toHaveClass(/chat-active/);
  await page.keyboard.press("Control+h");
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  // Back to the right, the one of the two left last.
  await page.keyboard.press("Control+l");
  await expect(page.locator("#list")).toHaveClass(/chat-active/);
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
});

test("Today's Waiting takes the keyboard while nothing waits, by Ctrl-J and by a click", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&chat=quiet");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await chooseInToday(page, '[data-today-show="chat"]');
  await expect(page.locator("#list .ch-section .empty")).toBeVisible();
  await intoTasks(page);

  await page.keyboard.press("Control+j");
  await expect(page.locator("#list")).toHaveClass(/chat-active/);
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.locator("#list .ch-section .empty").click();
  await expect(page.locator("#list")).toHaveClass(/chat-active/);
});

test("zm maximizes the list, and the reader comes out over it as a drawer", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator("#list").focus();
  const app = page.locator("#app");

  await page.keyboard.press("z");
  await page.keyboard.press("m");
  await expect(app).toHaveClass(/maximized/);
  await expect(page.locator("#folders")).toBeHidden();
  // Out from the start, the keyboard still in the list.
  await expect(page.locator("#reader")).toBeVisible();
  await expect(page.locator("#list")).toHaveClass(/focused/);
  await page.keyboard.press("q");
  await expect(page.locator("#reader")).toBeHidden();
  const listWidth = (await box(page.locator("#list"))).width;

  // Out with the keyboard, and the list keeps its width under it.
  await page.keyboard.press("Control+l");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await expect(page.locator("#reader")).toBeVisible();
  expect((await box(page.locator("#list"))).width).toBe(listWidth);
  // Back to the list, the drawer still out; then put away.
  await page.keyboard.press("Control+h");
  await expect(page.locator("#list")).toHaveClass(/focused/);
  await expect(page.locator("#reader")).toBeVisible();
  await page.keyboard.press("q");
  await expect(page.locator("#reader")).toBeHidden();

  // The folders come out from the left while they have the keyboard.
  await page.keyboard.press("Control+h");
  await expect(page.locator("#folders")).toHaveClass(/focused/);
  await expect(page.locator("#folders")).toBeVisible();
  await page.keyboard.press("Control+l");
  await expect(page.locator("#folders")).toBeHidden();

  await page.keyboard.press("z");
  await page.keyboard.press("m");
  await expect(app).not.toHaveClass(/maximized/);
  await expect(page.locator("#reader")).toBeVisible();
});

test("the sidebar button and zs put the folders away, and Ctrl-H brings them out as a drawer", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator("#list").focus();
  const app = page.locator("#app");
  const listWidth = (await box(page.locator("#list"))).width;

  await page.locator("#sidebar-toggle").click();
  await expect(app).toHaveClass(/sidebar-hidden/);
  await expect(page.locator("#folders")).toBeHidden();
  await expect(page.locator("#reader")).toBeVisible();
  expect((await box(page.locator("#list"))).width).toBeGreaterThan(listWidth);

  // Out from the left while they have the keyboard, away again after.
  await page.locator("#list").focus();
  await page.keyboard.press("Control+h");
  await expect(page.locator("#folders")).toHaveClass(/focused/);
  await expect(page.locator("#folders")).toBeVisible();
  await page.keyboard.press("Control+l");
  await expect(page.locator("#list")).toHaveClass(/focused/);
  await expect(page.locator("#folders")).toBeHidden();

  // Kept across a reload.
  await page.reload();
  await page.locator("#list .row.cursor").waitFor();
  await expect(app).toHaveClass(/sidebar-hidden/);

  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("s");
  await expect(app).not.toHaveClass(/sidebar-hidden/);
  await expect(page.locator("#folders")).toBeVisible();
});

test("maximized, Today shows the half the keyboard is in, with the reader below it", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await expect(page.locator("#list")).toHaveClass(/mail-active/);

  await page.keyboard.press("z");
  await page.keyboard.press("m");
  await expect(page.locator("#list .mail-scroll")).toBeVisible();
  await expect(page.locator("#list .tk-section")).toBeHidden();
  await expect(page.locator("#toolbar")).toBeVisible();
  await expect(page.locator("#sidebar-top")).toBeHidden();

  // Below the list, as wide as it, and not over it.
  await expect(page.locator("#reader")).toBeVisible();
  const list = (await box(page.locator("#list")));
  const reader = (await box(page.locator("#reader")));
  expect(reader.y).toBeGreaterThanOrEqual(list.y + list.height);
  expect(reader.width).toBe(list.width);

  // Stacked, Tab crosses to the tasks and Ctrl-J goes down to the reader.
  await page.keyboard.press("Tab");
  await expect(page.locator("#list .tk-section")).toBeVisible();
  await expect(page.locator("#list .mail-scroll")).toBeHidden();
  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await page.keyboard.press("Control+k");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("Tab");
  await expect(page.locator("#list .mail-scroll .row.cursor")).toBeVisible();

  // Put away, the list has the height back.
  await page.keyboard.press("q");
  await expect(page.locator("#reader")).toBeHidden();
  expect((await box(page.locator("#list"))).height).toBeGreaterThan(list.height);
});

test("only the half or pane with the keyboard draws its cursor in full", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  const accentEdge = (selector: string) =>
    page.locator(selector).evaluate((e) => getComputedStyle(e).borderLeftColor);
  const mailEdge = await accentEdge("#list .mail-scroll");
  expect(mailEdge).not.toBe(await accentEdge("#list .tk-section"));

  const fill = (selector: string) =>
    page.locator(selector).evaluate((e) => getComputedStyle(e).backgroundColor);
  const live = await fill("#list .mail-scroll .row.cursor");

  await page.keyboard.press("Control+j");
  expect(await accentEdge("#list .tk-section")).toBe(mailEdge);
  // The live cursor is filled with the accent; the other half's keeps a fill
  // of its own, so the place is not lost.
  expect(await fill("#list .tk-row.cursor")).toBe(live);
  const quiet = await fill("#list .mail-scroll .row.cursor");
  expect(quiet).not.toBe(live);
  expect(quiet).not.toBe("rgba(0, 0, 0, 0)");

  await page.keyboard.press("Control+j");
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  // The list's cursor goes quiet while the reader has the keyboard.
  expect(await fill("#list .tk-row.cursor")).toBe(quiet);
});

test("a click anywhere in a pane, or a half of Today, gives it the keyboard", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press("Control+j");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);

  // The empty space under the mail rows.
  const mail = (await box(page.locator("#list .mail-scroll")));
  await page.mouse.click(mail.x + mail.width / 2, mail.y + mail.height - 5);
  await expect(page.locator("#list")).toHaveClass(/mail-active/);
  await expect(page.locator("#list")).toHaveClass(/focused/);

  // The reader's headers; the message is not drawn again for it.
  // The mail clicked into above is still on its way to the reader.
  await page.waitForTimeout(400);
  const header = page.locator("#reader h1").first();
  await header.evaluate((e) => ((e as HTMLElement & { mark?: boolean }).mark = true));
  await header.click();
  await expect(page.locator("#reader")).toHaveClass(/focused/);
  await expect(page.locator("#list")).not.toHaveClass(/focused/);
  expect(await header.evaluate((e) => (e as HTMLElement & { mark?: boolean }).mark)).toBe(true);

  const folders = (await box(page.locator("#folders")));
  await page.mouse.click(folders.x + folders.width / 2, folders.y + folders.height - 5);
  await expect(page.locator("#folders")).toHaveClass(/focused/);
});

test("a message clicked with the keyboard in the folders draws the list as having it", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await settled(page);
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.keyboard.press("Control+h");
  await expect(page.locator("#folders")).toHaveClass(/focused/);
  await page.locator("#list .rows .row").nth(2).click();
  await expect(page.locator("#list")).toHaveClass(/focused/);
  await expect(page.locator("#folders")).not.toHaveClass(/focused/);
  await expect(page.locator("#list .row.cursor")).toHaveCSS("background-color", "rgb(64, 96, 192)");
});

test("zt shows Today's mail as conversations, and za opens one", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  await page.locator("#list").focus();

  await page.keyboard.press("z");
  await page.keyboard.press("t");
  await expect(page.locator("#list .mail-scroll .row.cursor")).toContainText("Thread #0");
  await expect(page.locator("#list .mail-scroll .row.cursor")).toContainText("3 ▸");
  await page.keyboard.press("z");
  await page.keyboard.press("a");
  await expect(page.locator("#list .mail-scroll .row.cursor")).toContainText("3 ▾");
  await expect(page.locator("#list .mail-scroll .rows .row").nth(1)).toContainText("Re: Thread #0");
});

test("⌘↓ and ⌘↑ move a task down and up the order", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await intoTasks(page);
  const first = await page.locator("#list .tk-row .tk-title").first().textContent();
  await page.keyboard.press("Meta+ArrowDown");
  await expect(page.locator("#list .tk-row .tk-title").nth(1)).toHaveText(first!);
  await page.keyboard.press("Meta+ArrowUp");
  await expect(page.locator("#list .tk-row .tk-title").first()).toHaveText(first!);
});

/** The arguments of every call of one command, in order. */
function callsOf(page: Page, name: string): Promise<Record<string, unknown>[]> {
  return page.evaluate(
    (name) =>
      (window as unknown as { __mock: { calls: [string, Record<string, unknown>][] } }).__mock.calls
        .filter(([cmd]) => cmd === name)
        .map(([, args]) => args),
    name,
  );
}

test("a double-click opens a message or a task in a window of its own", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await page.locator('#list [data-msg="0"]').dblclick();
  await expect.poll(() => callsOf(page, "open_item_window")).toEqual([
    { kind: "message", id: 0, title: "Message #0" },
  ]);
  await page.locator('#list [data-task="0"]').dblclick();
  await expect.poll(async () => (await callsOf(page, "open_item_window"))[1]).toEqual({
    kind: "task",
    id: 1,
    title: "Buy groceries",
  });
});

test("a message's window shows the message alone, and q closes it", async ({ page }) => {
  await page.goto("/harness.html?window=message-0");
  await expect(page.locator("#reader header")).toContainText("Message #0");
  await expect(page.locator("html")).toHaveAttribute("data-window", "message");
  for (const pane of ["#folders", "#list", "#hsplit"]) await expect(page.locator(pane)).toBeHidden();
  await expect(page.locator("#toolbar .tb-t")).toHaveText("Archive");
  expect(await callsOf(page, "sync_all")).toEqual([]);
  // `h` would go back to a list there is none of.
  await page.keyboard.press("h");
  await expect(page.locator("#status")).toHaveText("q or ⌘W closes the window");
  await page.keyboard.press("q");
  await expect.poll(async () => (await callsOf(page, "plugin:window|close")).length).toBe(1);
});

test("a task's window shows the task alone, and its keys act on it", async ({ page }) => {
  await page.goto("/harness.html?window=task-2&tasks=1");
  await expect(page.locator("#reader h1")).toHaveText("Call the plumber");
  await expect(page.locator("#list")).toBeHidden();
  await page.keyboard.press("x");
  await expect.poll(() => callsOf(page, "complete_tasks")).toEqual([{ ids: [2] }]);
  await page.keyboard.press("q");
  await expect.poll(async () => (await callsOf(page, "plugin:window|close")).length).toBe(1);
});

test("closing hides the mailbox, and closes every other window", async ({ page }) => {
  const keepsOpen = () =>
    page.evaluate(() => "tauri://close-requested" in ((window as unknown as { __listeners?: object }).__listeners ?? {}));
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  expect(await keepsOpen()).toBe(true);
  await page.goto("/harness.html?window=tasks&tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  expect(await keepsOpen()).toBe(false);
});

test("the reading list sits under Reading, archives into its own archive, and takes mail from any account", async ({ page }) => {
  await page.goto("/harness.html?reading=1");
  await settled(page);
  const calls = () =>
    page.evaluate(() =>
      (window as unknown as { __calls: { cmd: string; args: Record<string, unknown> }[] }).__calls
        .filter((c) => c.cmd === "move_messages" || c.cmd === "transfer_messages" || c.cmd === "search"),
    );
  const sidebar = page.locator("#folders");
  // Under its own heading, with the new items counted, and not among Private's folders.
  await expect(sidebar.locator(".sb-section", { hasText: "Reading" })).toHaveCount(1);
  await expect(sidebar.locator(".sb-row", { hasText: "To read" }).locator(".sb-badge.unread")).toHaveText("3");
  await expect(sidebar.locator('.sb-row[title="Reading"]')).toHaveCount(0);

  // `a` in the reading list files into the reading archive, not the account's.
  await sidebar.locator(".sb-row", { hasText: "To read" }).click();
  await expect(sidebar.locator(".sb-row.cursor", { hasText: "To read" })).toHaveCount(1);
  await page.locator("#list .rows .row").first().waitFor();
  await page.locator("#list").focus();
  await page.keyboard.press("a");
  await expect.poll(async () => (await calls()).at(-1)).toMatchObject({
    cmd: "move_messages",
    args: { account: "Private", folder: "INBOX.Reading", target: "INBOX.Reading Archive" },
  });

  // Must read is the flagged items of the reading folder.
  await sidebar.locator(".sb-row", { hasText: "Must read" }).click();
  await expect.poll(async () => (await calls()).at(-1)).toMatchObject({
    cmd: "search",
    args: { query: "is:flagged", scope: { Folders: { folders: [["Private", "INBOX.Reading"]] } } },
  });
  await expect(sidebar.locator(".sb-row.cursor", { hasText: "Must read" })).toHaveCount(1);
  await expect(sidebar.locator(".search-row")).toHaveCount(0);

  // `b` sets mail aside: within the account a move, from another a transfer.
  await page.locator('#folders .sb-row[title="Private"]').first().click();
  await page.locator("#list .rows .row").first().waitFor();
  await page.locator("#list").focus();
  await page.keyboard.press("b");
  await expect.poll(async () => (await calls()).at(-1)).toMatchObject({
    cmd: "move_messages",
    args: { account: "Private", folder: "INBOX", target: "INBOX.Reading" },
  });
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator("#list .rows .row").first().waitFor();
  await page.locator("#list").focus();
  await page.keyboard.press("b");
  await expect.poll(async () => (await calls()).at(-1)).toMatchObject({
    cmd: "transfer_messages",
    args: { fromAccount: "Work", fromFolder: "Archive", toAccount: "Private", toFolder: "INBOX.Reading" },
  });
});

test("Today shows the reading list below its tasks when asked, and walks into it", async ({ page }) => {
  await page.goto("/harness.html?tasks=1&reading=1");
  await page.locator("#list .mail-scroll .rows .row").first().waitFor();
  await settled(page);
  // Off until asked for.
  await expect(page.locator("#list .rd-section")).toHaveCount(0);
  await page.locator("#list").focus();
  await page.keyboard.press("z");
  await page.keyboard.press("r");
  expect(await todaySwitch(page, '[data-today-show="reading"]')).toBe("true");
  const rows = page.locator("#list .rd-section .rd-row");
  // Must read first: the flagged items.
  await expect(rows).toHaveCount(2);
  const tasks = (await box(page.locator("#list .tk-section")));
  const reading = (await box(page.locator("#list .rd-section")));
  expect(reading.y).toBeGreaterThanOrEqual(tasks.y + tasks.height - 1);

  // Tab goes mail, tasks, reading; the reader follows the reading cursor.
  await page.keyboard.press("Tab");
  await page.keyboard.press("Tab");
  await expect(page.locator("#list")).toHaveClass(/reading-active/);
  await expect(rows.first()).toHaveClass(/cursor/);
  await expect.poll(async () => (await callsTo(page, "message")).at(-1)?.id).toBe(50_000);
  await page.keyboard.press("j");
  await expect(rows.nth(1)).toHaveClass(/cursor/);
  await expect.poll(async () => (await callsTo(page, "message")).at(-1)?.id).toBe(50_001);
  // k past the first item goes back into the tasks.
  await page.keyboard.press("k");
  await page.keyboard.press("k");
  await expect(page.locator("#list")).toHaveClass(/tasks-active/);
  await page.keyboard.press("j");
  await expect(page.locator("#list")).toHaveClass(/reading-active/);

  // A key acts on the item under the reading cursor: `a` files it in the reading archive.
  await page.keyboard.press("a");
  await expect.poll(async () => (await callsTo(page, "move_messages")).at(-1)).toMatchObject({
    account: "Private",
    folder: "INBOX.Reading",
    target: "INBOX.Reading Archive",
    uids: [500],
  });

  // The switch in the heading shows the new items instead, and is remembered.
  await page.locator('#list [data-reading-shows="new"]').click();
  await expect(rows).toHaveCount(2);
  await expect(rows.first()).toContainText("Hacker Newsletter");
  await expect(rows.nth(1)).toContainText("How Linear");
  await page.reload();
  await page.locator("#list .rd-section .rd-row").first().waitFor();
  await expect(page.locator('#list [data-reading-shows="new"]')).toHaveAttribute("aria-checked", "true");

  // In columns it sits under the tasks, right of the mail.
  await chooseInToday(page, '[data-today-layout="columns"]');
  const mail = (await box(page.locator("#list .mail-scroll")));
  const right = (await box(page.locator("#list .rd-section")));
  const above = (await box(page.locator("#list .tk-section")));
  expect(right.x).toBeGreaterThanOrEqual(mail.x + mail.width - 1);
  expect(right.y).toBeGreaterThanOrEqual(above.y + above.height - 1);
  // The mail runs the pane's full height beside both, also once a filter
  // came and went: the pane's classes, not `:has()`, shape the columns.
  await page.locator("#list").focus();
  await page.keyboard.press("Shift+F");
  await expect(page.locator("#list")).toHaveClass(/with-filter/);
  await page.keyboard.press("Escape");
  await expect(page.locator("#list")).not.toHaveClass(/with-filter/);
  const pane = await box(page.locator("#list"));
  await expect.poll(async () => (await box(page.locator("#list .mail-scroll"))).height).toBeGreaterThan(pane.height - 4);

  await chooseInToday(page, '[data-today-show="reading"]');
  await expect(page.locator("#list .rd-section")).toHaveCount(0);
  await page.evaluate(() => localStorage.clear());
});

test("maximized from Today's reading section, the section takes the whole list, in either layout", async ({ page }) => {
  for (const columns of [false, true]) {
    await page.goto("/harness.html?tasks=1&reading=1");
    await page.locator("#list .mail-scroll .rows .row").first().waitFor();
    await settled(page);
    if (columns) await chooseInToday(page, '[data-today-layout="columns"]');
    await page.locator("#list").focus();
    await page.keyboard.press("z");
    await page.keyboard.press("r");
    await page.keyboard.press("Tab");
    await page.keyboard.press("Tab");
    await page.keyboard.press("z");
    await page.keyboard.press("m");
    await expect(page.locator("#app")).toHaveClass(/maximized/);
    await expect(page.locator("#list .mail-scroll")).toBeHidden();
    await expect(page.locator("#list .tk-section")).toBeHidden();
    const list = (await box(page.locator("#list")));
    const reading = (await box(page.locator("#list .rd-section")));
    expect(Math.abs(reading.x - list.x)).toBeLessThan(4);
    expect(reading.width).toBeGreaterThan(list.width - 4);
    // Tab brings the mail back in its place.
    await page.keyboard.press("Tab");
    await expect(page.locator("#list .rd-section")).toBeHidden();
    await expect(page.locator("#list .mail-scroll")).toBeVisible();
    await page.evaluate(() => localStorage.clear());
  }
});

test("the feeds are asked from the command line, and say how they stand", async ({ page }) => {
  await page.goto("/harness.html?reading=1");
  await settled(page);
  await page.locator("#list").focus();
  await page.keyboard.press(":");
  await page.keyboard.type("poll-feeds");
  await page.keyboard.press("Enter");
  await expect(page.locator("#status")).toContainText("3 new items from 2 feeds · 1 failed");
  await page.keyboard.press(":");
  await page.keyboard.type("feed-status");
  await page.keyboard.press("Enter");
  const panel = page.locator(".account-health");
  await expect(panel).toHaveCount(2);
  await expect(panel.nth(0)).toContainText("This Week in Rust");
  await expect(panel.nth(1)).toContainText("404 Not Found");
});

test("the line above Today's reading section can be dragged, is kept, and resets on a double-click, in either layout", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  for (const columns of [false, true]) {
    await page.goto("/harness.html?tasks=1&reading=1");
    await page.locator("#list .mail-scroll .rows .row").first().waitFor();
    await settled(page);
    if (columns) await chooseInToday(page, '[data-today-layout="columns"]');
    await page.locator("#list").focus();
    await page.keyboard.press("z");
    await page.keyboard.press("r");
    const section = page.locator("#list .rd-section");
    await section.locator(".rd-row").first().waitFor();
    const split = page.locator("#list [data-rd-split]");
    const list = (await box(page.locator("#list")));
    const fitted = (await box(section)).height;

    const from = (await box(split));
    await page.mouse.move(from.x + 50, from.y);
    await page.mouse.down();
    // The reading section is below the line: dragging it up gives it more.
    await page.mouse.move(from.x + 50, list.y + list.height * 0.4, { steps: 5 });
    await page.mouse.up();
    const dragged = (await box(section)).height;
    expect(dragged).toBeGreaterThan(list.height * 0.5);

    await page.reload();
    await section.locator(".rd-row").first().waitFor();
    expect(Math.abs((await box(section)).height - dragged)).toBeLessThan(3);

    await split.dblclick();
    expect(Math.abs((await box(section)).height - fitted)).toBeLessThan(3);
    await page.evaluate(() => localStorage.clear());
  }
});

test("the lines beside the folders and above the reader can be dragged, and are kept", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  const box = (selector: string) => page.locator(selector).boundingBox().then((b) => b!);
  const folders = (await box("#folders")).width;
  const vsplit = await box("#vsplit");
  await page.mouse.move(vsplit.x + vsplit.width / 2, vsplit.y + 300);
  await page.mouse.down();
  await page.mouse.move(vsplit.x + 80, vsplit.y + 300, { steps: 5 });
  await page.mouse.up();
  await expect.poll(async () => (await box("#folders")).width).toBeGreaterThan(folders + 60);

  const list = (await box("#list")).height;
  const hsplit = await box("#hsplit");
  await page.mouse.move(hsplit.x + 300, hsplit.y + hsplit.height / 2);
  await page.mouse.down();
  await page.mouse.move(hsplit.x + 300, hsplit.y + 100, { steps: 5 });
  await page.mouse.up();
  await expect.poll(async () => (await box("#list")).height).toBeGreaterThan(list + 60);

  const dragged = { folders: (await box("#folders")).width, list: (await box("#list")).height };
  await page.reload();
  await page.locator("#list .row.cursor").waitFor();
  expect(Math.abs((await box("#folders")).width - dragged.folders)).toBeLessThan(2);
  expect(Math.abs((await box("#list")).height - dragged.list)).toBeLessThan(2);
  await page.evaluate(() => localStorage.clear());
});

test("a slow boot keeps the cursor on Today and says it is loading, and a slow chat server does not hold Today back", async ({ page }) => {
  await page.addInitScript(() => {
    (window as unknown as { __mock: unknown }).__mock = { delay: { config_warnings: 1500, chat_status: 5000 }, fail: [] };
  });
  await page.goto("/harness.html?tasks=1&chat=1");
  await page.locator('#folders .sb-row[title="Archive"]').waitFor();
  await expect(page.locator("#folders .sb-row.cursor")).toHaveClass(/today/, { timeout: 500 });
  await expect(page.locator("#list .empty-view.loading")).toContainText("Loading");

  // Today is drawn while the chat server has yet to answer.
  await page.locator("#list .mail-scroll .row").first().waitFor({ timeout: 3000 });
  await expect(page.locator("#folders .sb-row.cursor")).toHaveClass(/today/);
  await expect(page.locator("#list .empty-view.loading")).toHaveCount(0);
});

test("Today does not wait for the sidebar's highlight badges, which follow", async ({ page }) => {
  await page.addInitScript(() => {
    (window as unknown as { __mock: unknown }).__mock = { delay: { highlight_badges: 4000 }, fail: [] };
  });
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list .mail-scroll .row").first().waitFor({ timeout: 2000 });
  await expect(page.locator("#list .empty-view.loading")).toHaveCount(0);
  await expect(page.locator('#folders .sb-badge.hl[title="Boss: 3 unread"]')).toHaveCount(0);
  await expect(page.locator('#folders .sb-badge.hl[title="Boss: 3 unread"]')).toHaveCount(1, { timeout: 6000 });
});

test("the task views are in the sidebar from its first drawing, however slow the boot after it", async ({ page }) => {
  await page.addInitScript(() => {
    (window as unknown as { __mock: unknown }).__mock = { delay: { config_warnings: 3000 }, fail: [] };
  });
  await page.goto("/harness.html?tasks=1");
  await page.locator('#folders .sb-row[title="Archive"]').waitFor();
  await expect(page.locator("#folders .sb-row.today")).toHaveCount(1, { timeout: 500 });
  await expect(page.locator("#folders .sb-section", { hasText: "Tasks" })).toHaveCount(1, { timeout: 500 });
});

test("Set Up iPhone shows the code with whose password it holds, replaces an expired one only when asked, and closes on Escape or when the window leaves the front", async ({ page }) => {
  await page.goto("/harness.html?tasks=1");
  await page.locator("#list [data-task]").first().waitFor();
  await settled(page);
  await page.evaluate(() => {
    (window as unknown as { __setupLifetime: number }).__setupLifetime = 2;
  });
  const summon = async () => {
    await page.locator("#list").focus();
    await page.keyboard.press(":");
    await page.keyboard.type("set-up-iphone");
    await page.keyboard.press("Enter");
  };
  await summon();

  const dialog = page.locator(".iphone-setup");
  await expect(dialog.locator(".is-code svg")).toHaveAttribute("data-code", "1");
  await expect(dialog.locator(".is-about")).toContainText("me@example.com on imap.example.com");
  await expect(dialog.locator(".is-warning")).toContainText("holds the password");
  await expect(dialog.locator(".is-countdown")).toContainText("This code works for 0:0");

  // Expired: gone from the screen, and not replaced until asked for.
  await expect(dialog.locator(".is-countdown")).toContainText("expired", { timeout: 5000 });
  await expect(dialog.locator(".is-code svg")).toHaveCount(0);
  await page.keyboard.press("Enter");
  await expect(dialog.locator(".is-code svg")).toHaveAttribute("data-code", "2");

  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);

  // A code nobody is looking at is a password on a screen: the window
  // leaving the front closes the dialog.
  await summon();
  await expect(dialog.locator(".is-code svg")).toHaveAttribute("data-code", "3");
  await page.evaluate(() => window.dispatchEvent(new Event("blur")));
  await expect(dialog).toHaveCount(0);

  // The shell's dialog answered no: nothing is shown.
  await page.evaluate(() => ((window as unknown as { __setupDeclined: boolean }).__setupDeclined = true));
  await summon();
  await expect(dialog).toHaveCount(0);
});

stepRedrawTests();
