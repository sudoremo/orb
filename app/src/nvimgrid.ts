/**
 * Neovim's screen as the `ext_linegrid` UI protocol describes it: one grid of
 * cells, each a character and a highlight id, with the highlight table and the
 * cursor beside it. `apply` folds a batch of `redraw` events into it; drawing
 * is the renderer's business. See `:help ui-linegrid`.
 */

/** A highlight as `hl_attr_define` gives it, colours as 24-bit integers. */
export interface HlAttr {
  foreground?: number;
  background?: number;
  special?: number;
  reverse?: boolean;
  italic?: boolean;
  bold?: boolean;
  strikethrough?: boolean;
  underline?: boolean;
  undercurl?: boolean;
  underdouble?: boolean;
  underdotted?: boolean;
  underdashed?: boolean;
}

/** One entry of `mode_info_set`: how the cursor looks in a mode. */
export interface ModeInfo {
  name?: string;
  cursor_shape?: "block" | "horizontal" | "vertical";
  cell_percentage?: number;
  attr_id?: number;
}

export class Grid {
  cols = 0;
  rows = 0;
  /** The character in each cell; "" for the right half of a wide one. */
  text: string[][] = [];
  hl: number[][] = [];
  attrs = new Map<number, HlAttr>();
  fg = 0xd0d0d0;
  bg = 0x1e1e1e;
  sp = 0xff5f5f;
  cursor = { row: 0, col: 0 };
  modes: ModeInfo[] = [];
  mode = 0;
  busy = false;
  mouse = false;
  /** Rows changed since the renderer last drew; `all` for everything. */
  dirty = new Set<number>();
  all = true;

  resize(cols: number, rows: number): void {
    const text: string[][] = [];
    const hl: number[][] = [];
    for (let r = 0; r < rows; r++) {
      text.push(Array.from({ length: cols }, (_, c) => this.text[r]?.[c] ?? " "));
      hl.push(Array.from({ length: cols }, (_, c) => this.hl[r]?.[c] ?? 0));
    }
    this.cols = cols;
    this.rows = rows;
    this.text = text;
    this.hl = hl;
    this.all = true;
  }

  clear(): void {
    for (let r = 0; r < this.rows; r++) {
      this.text[r].fill(" ");
      this.hl[r].fill(0);
    }
    this.all = true;
  }

  /** The highlight of a cell, with the defaults filled in and `reverse` applied. */
  look(id: number): { fg: number; bg: number; sp: number; attr: HlAttr } {
    const attr = this.attrs.get(id) ?? {};
    let fg = attr.foreground ?? this.fg;
    let bg = attr.background ?? this.bg;
    if (attr.reverse) [fg, bg] = [bg, fg];
    return { fg, bg, sp: attr.special ?? this.sp, attr };
  }

  /** Fold one batch of `redraw` events in. Returns whether it ended in a flush. */
  apply(batch: unknown[]): boolean {
    let flushed = false;
    for (const event of batch) {
      if (!Array.isArray(event) || typeof event[0] !== "string") continue;
      const [name, ...calls] = event as [string, ...unknown[][]];
      // An event with no argument tuple at all still happens once.
      for (const args of calls.length ? calls : [[]]) {
        if (this.event(name, args ?? [])) flushed = true;
      }
    }
    return flushed;
  }

  private event(name: string, a: unknown[]): boolean {
    switch (name) {
      case "grid_resize":
        this.resize(a[1] as number, a[2] as number);
        break;
      case "grid_clear":
        this.clear();
        break;
      case "grid_line":
        this.line(a[1] as number, a[2] as number, a[3] as unknown[][]);
        break;
      case "grid_scroll":
        this.scroll(a[1] as number, a[2] as number, a[3] as number, a[4] as number, a[5] as number);
        break;
      case "grid_cursor_goto":
        this.dirty.add(this.cursor.row);
        this.cursor = { row: a[1] as number, col: a[2] as number };
        this.dirty.add(this.cursor.row);
        break;
      case "default_colors_set": {
        const [fg, bg, sp] = a as number[];
        if (fg >= 0) this.fg = fg;
        if (bg >= 0) this.bg = bg;
        if (sp >= 0) this.sp = sp;
        this.all = true;
        break;
      }
      case "hl_attr_define":
        this.attrs.set(a[0] as number, (a[1] ?? {}) as HlAttr);
        break;
      case "mode_info_set":
        this.modes = (a[1] ?? []) as ModeInfo[];
        this.dirty.add(this.cursor.row);
        break;
      case "mode_change":
        this.mode = a[1] as number;
        this.dirty.add(this.cursor.row);
        break;
      case "busy_start":
        this.busy = true;
        this.dirty.add(this.cursor.row);
        break;
      case "busy_stop":
        this.busy = false;
        this.dirty.add(this.cursor.row);
        break;
      case "mouse_on":
        this.mouse = true;
        break;
      case "mouse_off":
        this.mouse = false;
        break;
      case "flush":
        return true;
    }
    return false;
  }

  /** `grid_line`: cells are `[text, hl_id?, repeat?]`, the id carrying over. */
  private line(row: number, col: number, cells: unknown[][]): void {
    const text = this.text[row];
    const hl = this.hl[row];
    if (!text) return;
    let id = 0;
    let c = col;
    for (const cell of cells) {
      const ch = cell[0] as string;
      if (cell.length > 1) id = cell[1] as number;
      const repeat = cell.length > 2 ? (cell[2] as number) : 1;
      for (let i = 0; i < repeat && c < this.cols; i++, c++) {
        text[c] = ch;
        hl[c] = id;
      }
    }
    this.dirty.add(row);
  }

  /**
   * `grid_scroll`: the region `top..bot` × `left..right` moves up by `rows`
   * (down when negative). The rows it uncovers are redrawn by the
   * `grid_line` events that follow.
   */
  private scroll(top: number, bot: number, left: number, right: number, rows: number): void {
    const copy = (to: number, from: number) => {
      for (let c = left; c < right; c++) {
        this.text[to][c] = this.text[from][c];
        this.hl[to][c] = this.hl[from][c];
      }
      this.dirty.add(to);
    };
    if (rows > 0) {
      for (let r = top; r < bot - rows; r++) copy(r, r + rows);
    } else {
      for (let r = bot - 1; r >= top - rows; r--) copy(r, r + rows);
    }
  }
}
