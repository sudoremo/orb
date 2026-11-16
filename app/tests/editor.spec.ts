import { expect, test } from "@playwright/test";

// The grid model and the key translation are plain modules; the page only
// lends them a browser to run in.
test.beforeEach(async ({ page }) => {
  await page.goto("/harness.html");
});

test("redraw events build the screen Neovim describes", async ({ page }) => {
  const screen = await page.evaluate(async () => {
    const { Grid } = await import("/src/nvimgrid.ts");
    const grid = new Grid();
    const flushed = grid.apply([
      ["grid_resize", [1, 6, 3]],
      ["default_colors_set", [0xffffff, 0x000000, 0xff0000, 0, 0]],
      ["hl_attr_define", [7, { bold: true, foreground: 0x00ff00 }, {}, []]],
      // Cells carry their highlight over, and repeat.
      ["grid_line", [1, 0, 0, [["a", 7], ["b"], [" ", 0, 4]], false], [1, 1, 0, [["c", 0], ["d"]], false]],
      ["grid_line", [1, 2, 0, [["世", 0], [""], ["x"]], false]],
      ["grid_cursor_goto", [1, 2, 2]],
      ["flush"],
    ]);
    const before = grid.text.map((row: string[]) => row.join(""));
    const boldGreen = grid.look(grid.hl[0][1]);
    // Up by one: the second row moves to the top.
    grid.apply([["grid_scroll", [1, 0, 3, 0, 6, 1, 0]]]);
    return {
      flushed,
      before,
      after: grid.text.map((row: string[]) => row.join("")),
      boldGreen,
      cursor: grid.cursor,
    };
  });
  expect(screen.flushed).toBe(true);
  expect(screen.before).toEqual(["ab    ", "cd    ", "世x   "]);
  expect(screen.after.slice(0, 2)).toEqual(["cd    ", "世x   "]);
  expect(screen.boldGreen).toMatchObject({ fg: 0x00ff00, bg: 0x000000, attr: { bold: true } });
  expect(screen.cursor).toEqual({ row: 2, col: 2 });
});

test("keys are spelled the way Neovim reads them", async ({ page }) => {
  const spelled = await page.evaluate(async () => {
    const { keyToInput, textToInput } = await import("/src/nvimkeys.ts");
    const key = (key: string, mods: Partial<Record<"ctrlKey" | "altKey" | "metaKey" | "shiftKey", boolean>> = {}) =>
      keyToInput({ key, ctrlKey: false, altKey: false, metaKey: false, shiftKey: false, ...mods });
    return {
      enter: key("Enter"),
      shiftTab: key("Tab", { shiftKey: true }),
      ctrlW: key("w", { ctrlKey: true }),
      ctrlLt: key("<", { ctrlKey: true }),
      ctrlSpace: key(" ", { ctrlKey: true }),
      plain: key("a"),
      // Alt types characters on a Mac: `@` on a Swiss keyboard is Alt-G.
      altAt: key("@", { altKey: true }),
      dead: key("Dead"),
      command: key("s", { metaKey: true }),
      shift: key("Shift", { shiftKey: true }),
      // The Enter that commits an input method's conversion.
      imeEnter: keyToInput({ key: "Enter", keyCode: 229, ctrlKey: false, altKey: false, metaKey: false, shiftKey: false }),
      text: textToInput("a<b"),
    };
  });
  expect(spelled).toEqual({
    enter: "<CR>",
    shiftTab: "<S-Tab>",
    ctrlW: "<C-w>",
    ctrlLt: "<C-lt>",
    ctrlSpace: "<C-Space>",
    plain: null,
    altAt: null,
    dead: null,
    command: null,
    shift: null,
    imeEnter: null,
    text: "a<lt>b",
  });
});
