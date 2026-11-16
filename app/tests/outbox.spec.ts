import { expect, Page, test } from "@playwright/test";

type Shell = {
  __emit: (event: string, payload: unknown) => void;
  __askAnswer?: string;
  __outbox: { id: number; failed_at: string | null }[];
  __mock: { answered?: string[]; calls?: [string, unknown][] };
};

/** Wait for the startup sync to have landed and redrawn the window. */
async function settled(page: Page): Promise<void> {
  await page.waitForFunction(() => (window as unknown as Shell).__mock?.answered?.includes("sync_all"));
}

const calls = (page: Page, cmd: string) =>
  page.evaluate(
    (cmd) => ((window as unknown as Shell).__mock.calls ?? []).filter(([c]) => c === cmd).map(([, a]) => a),
    cmd,
  );

test("an empty Outbox is not in the sidebar, and appears when mail is queued", async ({ page }) => {
  await page.goto("/harness.html");
  await settled(page);
  await expect(page.locator("#folders .outbox-row")).toHaveCount(0);
  await expect(page.locator("#status-steady [data-outbox-open]")).toHaveCount(0);

  await page.evaluate(() => {
    const shell = window as unknown as Shell;
    shell.__outbox = [{ id: 9, failed_at: null, account: "Work", sending_as: "Work", from: "me@example.com", to: ["a@example.com"], cc: [], bcc: [], subject: "Hi", body: "", attachments: [], queued_at: "2026-10-28T11:00:00Z", attempts: 0, error: null } as never];
    shell.__emit("outbox-changed", null);
  });
  await expect(page.locator("#folders .outbox-row .sb-label")).toHaveText("Outbox");
  await expect(page.locator("#folders .outbox-row .sb-badge.staged")).toHaveText("1");
  await expect(page.locator("#status-steady .warn[data-outbox-open]")).toHaveText("1 waiting to send");
});

test("a refused message shows in red in the sidebar and the status line", async ({ page }) => {
  await page.goto("/harness.html?outbox=1");
  await settled(page);
  const row = page.locator("#folders .outbox-row");
  await expect(row).toHaveClass(/failed/);
  await expect(row.locator(".sb-badge.outbox-failed")).toHaveText("1");
  await expect(page.locator("#status-steady .fail")).toHaveText("1 not sent");
  await expect(page.locator("#status-steady .warn[data-outbox-open]")).toHaveText("1 waiting to send");
});

test("the Outbox opens from the sidebar with the refused message first and its reason", async ({ page }) => {
  await page.goto("/harness.html?outbox=1");
  await settled(page);
  await page.locator("#folders .outbox-row").click();
  const items = page.locator("#reader .outbox-item");
  await expect(items).toHaveCount(2);
  await expect(items.nth(0)).toHaveClass(/failed/);
  await expect(items.nth(0).locator("h2")).toContainText("Quarterly report");
  await expect(items.nth(0).locator(".outbox-status")).toContainText("535 Incorrect authentication data");
  await expect(items.nth(0)).toContainText("via Work (Support)");
  await expect(items.nth(0)).toContainText("report.pdf");
  await expect(items.nth(1)).toHaveClass(/waiting/);
  await expect(items.nth(1).locator(".outbox-status")).toContainText("Waiting for the mail server");
  await expect(page.locator("#reader .panel-actions")).toContainText("1 not sent · 1 waiting");
});

test("the status line opens the Outbox too", async ({ page }) => {
  await page.goto("/harness.html?outbox=1");
  await settled(page);
  await page.locator("#status-steady .fail").click();
  await expect(page.locator("#reader .outbox-item")).toHaveCount(2);
});

test("Retry puts a refused message back in line", async ({ page }) => {
  await page.goto("/harness.html?outbox=1");
  await settled(page);
  await page.locator("#folders .outbox-row").click();
  await page.locator('#reader [data-outbox="retry"][data-id="7"]').click();
  await expect.poll(() => calls(page, "retry_outbox")).toEqual([{ id: 7 }]);
  await expect(page.locator("#reader .outbox-item.failed")).toHaveCount(0);
  await expect(page.locator("#folders .outbox-row")).not.toHaveClass(/failed/);
  await expect(page.locator("#status-steady .fail")).toHaveCount(0);
});

test("Delete asks first, and takes the message out", async ({ page }) => {
  await page.goto("/harness.html?outbox=1");
  await settled(page);
  await page.locator("#folders .outbox-row").click();
  const remove = page.locator('#reader [data-outbox="delete"][data-id="8"]');

  await remove.click();
  await expect
    .poll(() => page.evaluate(() => (window as unknown as Shell).__mock.answered ?? []))
    .toContain("plugin:dialog|message");
  expect(await calls(page, "discard_outbox")).toEqual([]);

  await page.evaluate(() => ((window as unknown as Shell).__askAnswer = "Delete"));
  await remove.click();
  await expect.poll(() => calls(page, "discard_outbox")).toEqual([{ id: 8 }]);
  await expect(page.locator("#reader .outbox-item")).toHaveCount(1);
});

test("Edit opens the message in a compose window, carrying where it came from", async ({ page }) => {
  await page.goto("/harness.html?outbox=1");
  await settled(page);
  await page.locator("#folders .outbox-row").click();
  await page.locator('#reader [data-outbox="edit"][data-id="7"]').click();
  await expect.poll(() => calls(page, "open_compose")).toHaveLength(1);
  const [opened] = (await calls(page, "open_compose")) as { draft: Record<string, unknown> }[];
  expect(opened!.draft).toMatchObject({ account: "Work", subject: "Quarterly report", outboxId: 7, savedUid: null });
});

test("when the last message leaves, the Outbox leaves the sidebar", async ({ page }) => {
  await page.goto("/harness.html?outbox=1");
  await settled(page);
  await page.evaluate(() => {
    const shell = window as unknown as Shell;
    shell.__outbox = [];
    shell.__emit("outbox-changed", null);
  });
  await expect(page.locator("#folders .outbox-row")).toHaveCount(0);
  await expect(page.locator("#status-steady [data-outbox-open]")).toHaveCount(0);
});
