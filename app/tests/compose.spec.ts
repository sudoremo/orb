import { expect, test } from "@playwright/test";
import { recipientDragTests } from "./recipients";

/** The compose window against compose-harness.html, which stubs the backend. */

test("Send and the draft's state sit in the title bar, and nothing below the editor", async ({ page }) => {
  await page.goto("/compose-harness.html");
  await expect(page.locator("#titlebar #compose-title")).toHaveText("Re: Q4 roadmap");
  await expect(page.locator("#titlebar #send")).toHaveText("Send ⌘↩");
  await expect(page.locator("#titlebar #attach")).toHaveText("Attach");
  await expect(page.locator("#compose footer")).toHaveCount(0);
  // Without Neovim the window says so, in the title bar, and offers the plain editor.
  await expect(page.locator("#compose-status")).toHaveClass(/warn/);
  await expect(page.locator("#plain")).toBeVisible();

  await page.locator("#subject").fill("Re: Q4 roadmap, with comments");
  await page.keyboard.press("Meta+s");
  await expect(page.locator("#compose-status")).toHaveText(/^Saved \d\d:\d\d$/);
  await expect(page.locator("#compose-status")).not.toHaveClass(/warn/);
});

test("Bcc is a row only once it is asked for or holds an address", async ({ page }) => {
  await page.goto("/compose-harness.html");
  await expect(page.locator("#compose-title")).toHaveText("Re: Q4 roadmap");
  await expect(page.locator("#bcc-row")).toBeHidden();
  await page.locator("#show-bcc").click();
  await expect(page.locator("#bcc-row")).toBeVisible();
  await expect(page.locator("#show-bcc")).toBeHidden();
  await expect(page.locator("#bcc .token-input")).toBeFocused();

  await page.goto("/compose-harness.html?bcc=1");
  await expect(page.locator("#bcc-row")).toBeVisible();
  await expect(page.locator("#bcc-row")).toContainText("archive@example.com");
});

test("the Files row shows only what is attached", async ({ page }) => {
  await page.goto("/compose-harness.html");
  await expect(page.locator("#compose-title")).toHaveText("Re: Q4 roadmap");
  await expect(page.locator("#files-row")).toBeHidden();
  await page.goto("/compose-harness.html?files=1");
  await expect(page.locator("#files-row .file span")).toHaveText("roadmap-comments.pdf");
});

test("From names the sender and the account, and chooses among every account's identities", async ({ page }) => {
  await page.goto("/compose-harness.html");
  await expect(page.locator("#from-account")).toHaveText("Personal");
  await expect(page.locator("#identity")).toHaveText("Sam Taylor sam@example.com");
  await expect(page.locator("#identity")).toBeDisabled();
  await expect(page.locator("#identity .chev")).toHaveCount(0);

  await page.goto("/compose-harness.html?identities=2");
  await expect(page.locator("#identity .chev")).toHaveCount(1);
  await page.locator("#identity").click();
  await expect(page.locator(".dp-title")).toHaveText("From");
  await page.locator(".dp-input").fill("work");
  await page.keyboard.press("Enter");
  await expect(page.locator("#identity .addr")).toHaveText("sam@work.example");
  await expect(page.locator("#from-account")).toHaveText("Personal / Work");

  // A single identity of its own is no reason to keep an account's draft
  // from going out of another account.
  await page.goto("/compose-harness.html?accounts=2&saved=1");
  await expect(page.locator("#identity")).toBeEnabled();
  await page.locator("#plain").focus();
  await page.keyboard.press("Meta+i");
  await page.locator(".dp-input").fill("office");
  await page.keyboard.press("Enter");
  await expect(page.locator("#from-account")).toHaveText("Office");
  const calls = () =>
    page.evaluate(() =>
      (window as unknown as { __mock: { calls: [string, Record<string, unknown>][] } }).__mock.calls
        .filter(([cmd]) => cmd === "save_draft" || cmd === "discard_draft")
        .map(([cmd, args]) =>
          cmd === "save_draft"
            ? `save ${(args.input as { account_name: string }).account_name} replacing ${args.replacing}`
            : `discard ${args.account} ${args.uid}`,
        ),
    );
  await expect.poll(calls).toEqual(["save Office replacing null", "discard Personal 3"]);
});

test("a warning is shown under the header, and Send asks before sending past it", async ({ page }) => {
  await page.goto("/compose-harness.html?warn=1&answer=cancel");
  await expect(page.locator("#warnings-row")).toBeVisible();
  await expect(page.locator("#warnings li")).toHaveText("Mentions an attachment (“anbei”) but has none");
  const commands = () =>
    page.evaluate(() =>
      (window as unknown as { __mock: { calls: [string, unknown][] } }).__mock.calls.map(([cmd]) => cmd),
    );
  await page.locator("#send").click();
  await expect.poll(commands).toContain("plugin:dialog|message");
  expect(await commands()).not.toContain("send_mail");

  await page.goto("/compose-harness.html?warn=1");
  await page.locator("#send").click();
  await expect.poll(commands).toContain("send_mail");

  await page.goto("/compose-harness.html");
  await expect(page.locator("#compose-title")).toHaveText("Re: Q4 roadmap");
  await expect(page.locator("#warnings-row")).toBeHidden();
  await page.locator("#send").click();
  await expect.poll(commands).toContain("send_mail");
  expect(await commands()).not.toContain("plugin:dialog|message");
});

test("Save & close keeps the draft, and Discard asks, then deletes it", async ({ page }) => {
  const calls = () =>
    page.evaluate(() =>
      (window as unknown as { __mock: { calls: [string, unknown][] } }).__mock.calls.map(([cmd]) => cmd),
    );
  await page.goto("/compose-harness.html");
  await expect(page.locator("#titlebar #save-close")).toHaveText("Save & close");
  await page.locator("#save-close").click();
  await expect.poll(calls).toContain("plugin:window|close");
  expect(await calls()).toContain("save_draft");

  await page.goto("/compose-harness.html?saved=1&answer=cancel");
  await expect(page.locator("#titlebar #discard")).toHaveText("Discard");
  await page.locator("#discard").click();
  await expect.poll(calls).toContain("plugin:dialog|message");
  expect(await calls()).not.toContain("discard_draft");
  expect(await calls()).not.toContain("plugin:window|close");

  await page.goto("/compose-harness.html?saved=1");
  await page.locator("#discard").click();
  await expect.poll(calls).toContain("plugin:window|close");
  expect(await calls()).toContain("discard_draft");
  expect(await calls()).not.toContain("save_draft");
});

test("a message taken from the Outbox leaves it once saved, and stays there while it cannot be", async ({ page }) => {
  const calls = () =>
    page.evaluate(() =>
      (window as unknown as { __mock: { calls: [string, unknown][] } }).__mock.calls.map(([cmd, args]) =>
        cmd === "discard_outbox" ? `${cmd} ${(args as { id: number }).id}` : cmd,
      ),
    );
  await page.goto("/compose-harness.html?outbox=1");
  await page.locator("#save-close").click();
  await expect.poll(calls).toContain("plugin:window|close");
  const order = await calls();
  expect(order).toContain("discard_outbox 7");
  expect(order.indexOf("save_draft")).toBeLessThan(order.indexOf("discard_outbox 7"));

  await page.goto("/compose-harness.html?outbox=1&savefails=1");
  await page.locator("#save-close").click();
  await expect.poll(calls).toContain("save_draft");
  await page.waitForTimeout(100);
  expect(await calls()).not.toContain("discard_outbox 7");

  await page.goto("/compose-harness.html?outbox=1");
  await page.locator("#send").click();
  await expect.poll(calls).toContain("send_mail");
  await expect.poll(calls).toContain("discard_outbox 7");
});

test("Ctrl-J and Ctrl-K move between the header's lines and the body, and out past its ends", async ({ page }) => {
  await page.goto("/compose-harness.html?identities=2");
  await expect(page.locator("#identity .chev")).toHaveCount(1);
  // Started: until Neovim is given up on, the body is its pane, not the field.
  await expect(page.locator("#compose-status")).toHaveClass(/warn/);
  await page.locator("#plain").focus();

  await page.keyboard.press("Control+k");
  await expect(page.locator("#subject")).toBeFocused();
  await page.keyboard.press("Control+k");
  await expect(page.locator("#cc .token-input")).toBeFocused();
  await page.keyboard.press("Control+k");
  await expect(page.locator("#to .token-input")).toBeFocused();
  await page.keyboard.press("Control+k");
  await expect(page.locator("#identity")).toBeFocused();
  // Past the top, and for a Ctrl+H/J/K/L the window has no use for, the key
  // leaves the window by the escalate command.
  const escalated = () =>
    page.evaluate(() =>
      (window as unknown as { __mock: { calls: [string, { direction: string }][] } }).__mock.calls
        .filter(([cmd]) => cmd === "escalate")
        .map(([, args]) => args.direction),
    );
  await page.keyboard.press("Control+k");
  await expect(page.locator("#identity")).toBeFocused();
  await expect.poll(escalated).toEqual(["up"]);
  await page.keyboard.press("Control+l");
  await expect.poll(escalated).toEqual(["up", "right"]);

  await page.locator("#show-bcc").click();
  await page.keyboard.press("Control+j");
  await expect(page.locator("#subject")).toBeFocused();
  await page.keyboard.press("Control+j");
  await expect(page.locator("#plain")).toBeFocused();
  await expect(page.locator("#plain")).toHaveValue("Hi Sara,\n\nthanks.\n");
  await page.keyboard.press("Control+k");
  await page.keyboard.press("Control+k");
  await expect(page.locator("#bcc .token-input")).toBeFocused();
});

test("Ctrl-T asks for a day to follow the message up on, and Send asks for the task with it", async ({ page }) => {
  await page.goto("/compose-harness.html");
  await expect(page.locator("#compose-title")).toHaveText("Re: Q4 roadmap");
  await expect(page.locator("#follow-up")).toHaveText("Follow up");
  await page.locator("#plain").focus();

  await page.keyboard.press("Control+t");
  await expect(page.locator(".dp-title")).toHaveText("Follow up on");
  await page.locator(".dp-input").fill("thu");
  await expect(page.locator(".dp-list li").first()).toContainText("20 Aug");
  await page.keyboard.press("Enter");
  await expect(page.locator("#follow-up")).toHaveText(/^Follow up \d+ Aug( 2026)?$|^Follow up \w+$/);
  await expect(page.locator("#follow-up")).toHaveClass(/on/);
  await expect(page.locator("#plain")).toBeFocused();

  await page.keyboard.press("Meta+Enter");
  const sent = () =>
    page.evaluate(() =>
      (window as unknown as { __mock: { calls: [string, { input: { follow_up: string | null } }][] } }).__mock.calls
        .filter(([cmd]) => cmd === "send_mail")
        .map(([, args]) => args.input.follow_up),
    );
  await expect.poll(sent).toEqual(["2026-08-20"]);
});

test("a follow-up without tasks configured says what is missing", async ({ page }) => {
  await page.goto("/compose-harness.html?tasks=0");
  await expect(page.locator("#compose-title")).toHaveText("Re: Q4 roadmap");
  await page.locator("#follow-up").click();
  await expect(page.locator("#compose-status")).toContainText("needs a tasks section");
  await expect(page.locator(".dp")).toHaveCount(0);
});

recipientDragTests();
