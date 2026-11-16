/**
 * Neovim, in the window.
 *
 * `nvim --embed`, spoken to over msgpack-RPC by the core, with the grid drawn
 * here on a canvas: the user's own Neovim — their configuration, plugins and
 * colour scheme — in the app's font, with the app's cursor and the system's
 * keyboard. Text arrives through a hidden text field, so dead keys and input
 * methods compose as they do everywhere else on the Mac.
 *
 * The draft is also a file Neovim edits, so a Neovim that dies takes nothing
 * written with it.
 */

import { Channel, invoke } from "@tauri-apps/api/core";
import { Grid } from "./nvimgrid";
import { keyToInput, mouseModifiers, textToInput } from "./nvimkeys";
import type { Expanded } from "./placeholders";

export interface Editor {
  /** The draft as it stands in the editor, signature included. */
  body(): Promise<string>;
  /** The same; kept apart for the callers that mean "unwritten too". */
  buffer(): Promise<string>;
  /** Replace the body in the editor, as one change, and write it. */
  setBuffer(body: string, signature: string | null): Promise<void>;
  /**
   * Put a template's text in at the cursor, as one change, keeping its
   * placeholders to be filled; false when the draft's buffer is gone.
   */
  insertTemplate(text: Expanded): Promise<boolean>;
  /** Write `value` wherever the placeholder stands, or the placeholder again for "". */
  fillPlaceholder(name: string, value: string): Promise<void>;
  /** Put the cursor where the placeholder first stands; false when it stands nowhere. */
  gotoPlaceholder(name: string): Promise<boolean>;
  /** Open a file in a tab of its own, beside the draft. */
  editFile(path: string): Promise<void>;
  /** Give the editor the keyboard. */
  focus(): void;
  /** Stop Neovim and clean up the file it was editing. */
  close(): Promise<void>;
}

type EditorEvent =
  | { type: "redraw"; events: unknown[] }
  | { type: "written" }
  | { type: "exited" };


const css = (rgb: number): string => `#${rgb.toString(16).padStart(6, "0")}`;

/** Neovim's text size: `appearance.editor_font_size` if set, else the window's. */
function editorFontSize(): number {
  const root = getComputedStyle(document.documentElement);
  return parseFloat(root.getPropertyValue("--editor-font-size")) || parseFloat(root.getPropertyValue("--font-size"));
}

/**
 * Start Neovim on `body`, drawing into `host`.
 *
 * `id` names the session, so several compose windows each have their own.
 * `signature` is the identity's, from its `-- ` line on: where the body
 * carries it unchanged, Neovim shows it below the text and cannot edit it.
 */
export async function startEditor(
  host: HTMLElement,
  id: string,
  body: string,
  signature: string | null,
  onExit: (body: string) => void,
  takeFocus = false,
  onWrite?: () => void,
): Promise<Editor> {
  host.replaceChildren();
  host.classList.add("nvim");
  const canvas = document.createElement("canvas");
  const input = document.createElement("textarea");
  input.className = "nvim-input";
  input.setAttribute("autocapitalize", "off");
  input.setAttribute("autocomplete", "off");
  input.setAttribute("autocorrect", "off");
  input.spellcheck = false;
  input.setAttribute("aria-label", "Message body, in Neovim");
  host.append(canvas, input);
  const ctx = canvas.getContext("2d")!;

  // Cell geometry from the app's font at the size the window uses, measured
  // again when the size or the display's pixel density changes.
  let size = 13;
  let dpr = 1;
  let cellW = 8;
  let cellH = 18;
  let baseline = 14;
  // A canvas font cannot say `var(--mono)`, so the stack is read from it.
  let family = "monospace";
  const measure = () => {
    const root = getComputedStyle(document.documentElement);
    size = editorFontSize() || 13;
    family = root.getPropertyValue("--mono").trim() || "monospace";
    dpr = window.devicePixelRatio || 1;
    ctx.font = `${size}px ${family}`;
    cellW = Math.ceil(ctx.measureText("M").width * dpr) / dpr;
    cellH = Math.ceil(size * 1.35);
    baseline = Math.round(size * 1.05);
  };
  measure();
  // Every listener on the window or the document goes when the editor does.
  const life = new AbortController();
  const { signal } = life;

  const grid = new Grid();
  let focused = false;
  let closed = false;

  const fit = (): { cols: number; rows: number } => {
    const style = getComputedStyle(host);
    const width = host.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
    const height = host.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom);
    return {
      cols: Math.max(1, Math.floor(width / cellW)),
      rows: Math.max(1, Math.floor(height / cellH)),
    };
  };

  const sizeCanvas = () => {
    canvas.width = Math.round(grid.cols * cellW * dpr);
    canvas.height = Math.round(grid.rows * cellH * dpr);
    canvas.style.width = `${grid.cols * cellW}px`;
    canvas.style.height = `${grid.rows * cellH}px`;
  };

  // ---------------------------------------------------------------- drawing

  const drawRow = (r: number) => {
    const y = r * cellH;
    ctx.fillStyle = css(grid.bg);
    ctx.fillRect(0, y, grid.cols * cellW, cellH);
    const text = grid.text[r];
    const hl = grid.hl[r];
    // Backgrounds first, in runs, so a wide glyph is not cut by its
    // neighbour's background.
    for (let c = 0; c < grid.cols; ) {
      const id = hl[c];
      let end = c + 1;
      while (end < grid.cols && hl[end] === id) end++;
      const { bg } = grid.look(id);
      if (bg !== grid.bg) {
        ctx.fillStyle = css(bg);
        ctx.fillRect(c * cellW, y, (end - c) * cellW, cellH);
      }
      c = end;
    }
    for (let c = 0; c < grid.cols; c++) {
      const ch = text[c];
      const { fg, sp, attr } = grid.look(hl[c]);
      const x = c * cellW;
      if (ch && ch !== " ") {
        ctx.font = `${attr.italic ? "italic " : ""}${attr.bold ? "bold " : ""}${size}px ${family}`;
        ctx.fillStyle = css(fg);
        ctx.fillText(ch, x, y + baseline);
      }
      const width = text[c + 1] === "" ? 2 * cellW : cellW;
      if (attr.underline || attr.underdouble || attr.underdotted || attr.underdashed) {
        ctx.fillStyle = css(attr.special !== undefined ? sp : fg);
        ctx.fillRect(x, y + cellH - 2, width, 1);
      }
      if (attr.undercurl) {
        ctx.strokeStyle = css(sp);
        ctx.lineWidth = 1;
        ctx.beginPath();
        for (let i = 0; i <= width; i += 2) {
          ctx.lineTo(x + i, y + cellH - 2 + (i % 4 === 0 ? 0 : -1.5));
        }
        ctx.stroke();
      }
      if (attr.strikethrough) {
        ctx.fillStyle = css(fg);
        ctx.fillRect(x, y + Math.round(cellH / 2), width, 1);
      }
    }
  };

  const drawCursor = () => {
    const { row, col } = grid.cursor;
    if (grid.busy || row >= grid.rows || col >= grid.cols) return;
    const mode = grid.modes[grid.mode] ?? {};
    const cell = grid.look(grid.hl[row][col]);
    const own = mode.attr_id ? grid.look(mode.attr_id) : null;
    const color = css(own && own.attr.background !== undefined ? own.bg : cell.fg);
    const x = col * cellW;
    const y = row * cellH;
    const wide = grid.text[row][col + 1] === "" ? 2 : 1;
    if (!focused) {
      ctx.strokeStyle = color;
      ctx.lineWidth = 1;
      ctx.strokeRect(x + 0.5, y + 0.5, wide * cellW - 1, cellH - 1);
      return;
    }
    const pct = (mode.cell_percentage ?? 100) / 100;
    ctx.fillStyle = color;
    if (mode.cursor_shape === "vertical") {
      ctx.fillRect(x, y, Math.max(1, cellW * pct), cellH);
    } else if (mode.cursor_shape === "horizontal") {
      const h = Math.max(1, cellH * pct);
      ctx.fillRect(x, y + cellH - h, wide * cellW, h);
    } else {
      ctx.fillRect(x, y, wide * cellW, cellH);
      const ch = grid.text[row][col];
      if (ch && ch !== " ") {
        ctx.font = `${size}px ${family}`;
        ctx.fillStyle = css(own && own.attr.foreground !== undefined ? own.fg : cell.bg);
        ctx.fillText(ch, x, y + baseline);
      }
    }
  };

  let drawnCursorRow = 0;
  const draw = () => {
    if (grid.cols === 0) return;
    const expected = Math.round(grid.cols * cellW * dpr);
    if (canvas.width !== expected || canvas.height !== Math.round(grid.rows * cellH * dpr)) {
      sizeCanvas();
      grid.all = true;
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.textBaseline = "alphabetic";
    grid.dirty.add(drawnCursorRow);
    const rows = grid.all ? [...Array(grid.rows).keys()] : [...grid.dirty];
    for (const r of rows) if (r >= 0 && r < grid.rows) drawRow(r);
    grid.dirty.clear();
    grid.all = false;
    drawCursor();
    drawnCursorRow = grid.cursor.row;
    host.style.background = css(grid.bg);
    // The text field follows the cursor, so an input method's candidate
    // window opens where the text goes.
    input.style.left = `${canvas.offsetLeft + grid.cursor.col * cellW}px`;
    input.style.top = `${canvas.offsetTop + grid.cursor.row * cellH}px`;
  };

  // ---------------------------------------------------------------- the session

  const events = new Channel<EditorEvent>();
  events.onmessage = (event) => {
    if (closed) return;
    if (event.type === "redraw") {
      if (grid.apply(event.events)) draw();
    } else if (event.type === "written") {
      onWrite?.();
    } else if (event.type === "exited") {
      // `:x` and `:q` are how someone leaves an editor, and leaving should
      // hand the draft back rather than leave the last frame on screen.
      void invoke<string>("editor_buffer", { id })
        .catch(() => "")
        .then((text) => onExit(text));
    }
  };

  const send = (keys: string) => {
    if (keys) void invoke("editor_input", { id, keys }).catch(() => {});
  };

  const start = fit();
  await invoke("editor_open", { id, body, signature, cols: start.cols, rows: start.rows, events });

  // ---------------------------------------------------------------- input

  let composing = false;
  // The key that commits a conversion arrives just after the composition
  // ends, as an ordinary Enter; it belongs to the input method, not to Neovim.
  let committedAt = -1;
  input.addEventListener("keydown", (e) => {
    const keys = keyToInput(e);
    if (keys === null) return;
    e.preventDefault();
    if (keys === "<CR>" && performance.now() - committedAt < 50) return;
    send(keys);
  });
  input.addEventListener("compositionstart", () => {
    composing = true;
  });
  input.addEventListener("compositionend", (e) => {
    composing = false;
    committedAt = performance.now();
    send(textToInput(e.data));
    input.value = "";
  });
  input.addEventListener("input", (e) => {
    if (composing || (e as InputEvent).isComposing) return;
    send(textToInput(input.value));
    input.value = "";
  });
  input.addEventListener("paste", (e) => {
    e.preventDefault();
    const text = e.clipboardData?.getData("text/plain") ?? "";
    if (text) void invoke("editor_paste", { id, text }).catch(() => {});
  });
  const setFocus = (gained: boolean) => {
    focused = gained;
    grid.dirty.add(grid.cursor.row);
    draw();
    void invoke("editor_focus", { id, gained }).catch(() => {});
  };
  input.addEventListener("focus", () => setFocus(true));
  input.addEventListener("blur", () => setFocus(false));

  const cellAt = (e: MouseEvent) => {
    const box = canvas.getBoundingClientRect();
    return {
      row: Math.max(0, Math.min(grid.rows - 1, Math.floor((e.clientY - box.top) / cellH))),
      col: Math.max(0, Math.min(grid.cols - 1, Math.floor((e.clientX - box.left) / cellW))),
    };
  };
  const mouse = (button: string, action: string, e: MouseEvent) => {
    if (!grid.mouse) return;
    const { row, col } = cellAt(e);
    void invoke("editor_mouse", {
      id,
      button,
      action,
      modifier: mouseModifiers(e),
      row,
      col,
    }).catch(() => {});
  };
  const BUTTONS = ["left", "middle", "right"];
  let pressed: string | null = null;
  canvas.addEventListener("mousedown", (e) => {
    e.preventDefault();
    input.focus();
    pressed = BUTTONS[e.button] ?? null;
    if (pressed) mouse(pressed, "press", e);
  });
  canvas.addEventListener("mousemove", (e) => {
    if (pressed) mouse(pressed, "drag", e);
  });
  window.addEventListener(
    "mouseup",
    (e) => {
      if (pressed) mouse(pressed, "release", e);
      pressed = null;
    },
    { signal },
  );
  let wheel = 0;
  canvas.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      wheel += e.deltaY;
      while (Math.abs(wheel) >= cellH) {
        mouse("wheel", wheel > 0 ? "down" : "up", e);
        wheel -= Math.sign(wheel) * cellH;
      }
    },
    { passive: false },
  );
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());

  // The grid follows the window.
  let last = start;
  const refit = () => {
    const next = fit();
    if (next.cols === last.cols && next.rows === last.rows) return;
    last = next;
    void invoke("editor_resize", { id, cols: next.cols, rows: next.rows }).catch(() => {});
  };
  const observer = new ResizeObserver(refit);
  observer.observe(host);

  // A new text size (⌘+ in the mailbox window) or a display of another
  // density: new cells, the whole grid redrawn, and as many of them as fit.
  const remeasure = () => {
    measure();
    sizeCanvas();
    grid.all = true;
    draw();
    refit();
  };
  const sizeWatch = new MutationObserver(() => {
    const now = editorFontSize();
    if (now && now !== size) remeasure();
  });
  sizeWatch.observe(document.documentElement, { attributes: true, attributeFilter: ["style"] });
  const watchDensity = () => {
    window
      .matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`)
      .addEventListener(
        "change",
        () => {
          if (closed) return;
          remeasure();
          watchDensity();
        },
        { once: true, signal },
      );
  };
  watchDensity();

  // Only if asked. Which field has the keyboard when a draft opens is the
  // window's decision, not the editor's.
  if (takeFocus) input.focus();

  const read = () => invoke<string>("editor_buffer", { id });
  return {
    body: read,
    buffer: read,
    setBuffer: (text, signature) => invoke("editor_set_buffer", { id, body: text, signature }),
    insertTemplate: (text) => invoke<boolean>("editor_insert_template", { id, text }),
    fillPlaceholder: (name, value) => invoke("editor_fill_placeholder", { id, name, value }),
    gotoPlaceholder: (name) => invoke<boolean>("editor_goto_placeholder", { id, name }),
    editFile: (path) => invoke("editor_edit_file", { id, path }),
    focus: () => input.focus(),
    close: async () => {
      closed = true;
      observer.disconnect();
      sizeWatch.disconnect();
      life.abort();
      await invoke("editor_close", { id }).catch(() => {});
      host.replaceChildren();
      host.classList.remove("nvim");
      host.style.background = "";
    },
  };
}
