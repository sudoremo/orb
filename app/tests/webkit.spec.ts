import { test, expect } from "@playwright/test";
import { chatFocusTests } from "./chatfocus";
import { recipientDragTests } from "./recipients";
import { stepRedrawTests } from "./stepredraw";

// The app runs on WebKit, which differs from Chromium where it matters here:
// it runs no listener inside a frame of HTML mail, whose sandbox allows no
// script, not even one the window attached.
test.use({ browserName: "webkit" });

test("⌘C copies text selected in HTML mail while the pane has the keyboard", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.waitForFunction(() => (window as unknown as { __mock?: { answered?: string[] } }).__mock?.answered?.includes("sync_all"));
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="2"]').click();
  await page.locator("#reader iframe").waitFor();
  // What a drag over the text leaves behind: a selection in the frame, and the
  // keyboard with the pane, which took it back when the button went down.
  await page.frameLocator("#reader iframe").locator("body").evaluate((body) => {
    body.ownerDocument.getSelection()?.selectAllChildren(body);
  });
  await page.locator("#list").focus();
  // The frame's copy is recorded from its own realm, since none of its
  // listeners would run.
  await page.evaluate(() => {
    const frame = document.querySelector<HTMLIFrameElement>("#reader iframe")!;
    const proto = (frame.contentWindow as unknown as { Document: typeof Document }).Document.prototype;
    const copy = proto.execCommand;
    const copies: string[] = [];
    (window as unknown as { copies: string[] }).copies = copies;
    proto.execCommand = function (this: Document, command: string, ...rest: unknown[]) {
      const done = (copy as (...args: unknown[]) => boolean).call(this, command, ...rest);
      if (command === "copy" && done) copies.push(this.getSelection()?.toString() ?? "");
      return done;
    };
  });
  await page.keyboard.press("Meta+c");
  const copies = await page.evaluate(() => (window as unknown as { copies: string[] }).copies);
  expect(copies).toEqual([expect.stringContaining("HTML body of message")]);
  // The keyboard stays with the pane, so the next key is a binding again.
  expect(await page.evaluate(() => document.activeElement?.id)).toBe("list");
});

test("after ⌘A in HTML mail, ⌘C copies the message and the next key is a binding again", async ({ page }) => {
  await page.goto("/harness.html");
  await page.locator("#list .row.cursor").waitFor();
  await page.waitForFunction(() => (window as unknown as { __mock?: { answered?: string[] } }).__mock?.answered?.includes("sync_all"));
  await page.locator('#folders .sb-row[title="Archive"]').click();
  await page.locator('#list .rows .row[data-msg="2"]').click();
  await page.locator("#reader iframe").waitFor();
  await page.locator("#reader header").click();
  await page.keyboard.press("Meta+a");
  const selected = () =>
    page.frameLocator("#reader iframe").locator("body").evaluate((b) => b.ownerDocument.getSelection()?.toString() ?? "");
  await expect.poll(selected).toContain("HTML body of message");
  await page.keyboard.press("Meta+c");
  expect(await page.evaluate(() => document.activeElement?.tagName)).not.toBe("IFRAME");
  await page.keyboard.press("g");
  await page.keyboard.press("f");
  await expect(page.locator(".dp.picker")).toBeVisible();
});

recipientDragTests();

chatFocusTests();

stepRedrawTests();
