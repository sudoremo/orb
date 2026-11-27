/**
 * A fuzzy list picker: type a few letters, pick a row.
 *
 * Opened for a folder to move mail to, where typing the folder's name in full
 * was the previous answer and `mark-move Archive` on a key the one before. The
 * list is the folders of one account; the match is `fuzzyMatch`'s, which is
 * what makes `wrk` find `Work` and `ar` find `Archive` ahead of
 * `Mailing Lists/Hardware`.
 *
 * Built on the date picker's markup and styles rather than a dialog of its
 * own: one modal look, one scrim, one set of rules about focus. Like the date
 * picker it owns the keyboard while open and resolves `null` for every path
 * where nothing was chosen, so a caller treats "no answer" as "do nothing".
 *
 * With `modes` it is the command palette: what the field starts with — `>`,
 * `#`, `@` — chooses the list, as in VS Code's Quick Open.
 */

import { escape } from "./format";
import { fuzzyMatchLabel } from "./fuzzy";
import { eventKey } from "./keys";

export interface PickerItem {
  /** What is returned when the row is chosen. */
  value: string;
  /** What the row shows; `value` when absent. */
  label?: string;
  /** Dimmer text at the right of the row. */
  detail?: string;
  /** More text the row is found by, besides what it shows. */
  keywords?: string;
  /** The key that does the same, spelled as the key hints spell it: `⌘K`, `gf`. */
  keys?: string;
  /** Why the row cannot be chosen now; shown, and the row stays in the list greyed out. */
  disabled?: string;
  /** The heading the row is listed under while nothing is typed: `recently used`. */
  section?: string;
  /** How much better than its match alone the row ranks: a command used recently. */
  boost?: number;
}

/** One list of a picker with several, chosen by what the field starts with. */
export interface PickerMode {
  /** What the field starts with for this list: `>`; the empty one is the list for anything else. */
  prefix: string;
  title: string;
  placeholder?: string;
  items: () => PickerItem[];
  /** More items for what was typed, looked up elsewhere, as `PickerOptions.search`. */
  search?: (query: string) => Promise<PickerItem[]>;
  /** How long typing has to pause before `search` is asked, as `PickerOptions.searchDelay`. */
  searchDelay?: number;
}

export interface PickerOptions {
  title: string;
  placeholder?: string;
  items: PickerItem[];
  /**
   * More items for what was typed, looked up elsewhere (a server); asked a
   * moment after typing stops, and listed after the items that match.
   */
  search?: (query: string) => Promise<PickerItem[]>;
  /**
   * How long typing has to pause before `search` is asked, in milliseconds.
   * The default spares a server a request per key; `0` is for a lookup that
   * costs nothing, whose rows then follow every key.
   */
  searchDelay?: number;
  /**
   * Several rows rather than one: `Enter` adds the row to the chosen ones,
   * shown above the field, and `Enter` on an empty field — or `⌘Enter` —
   * is done. `Backspace` on an empty field takes the last one back.
   */
  multiple?: boolean;
  /**
   * Take what was typed as an answer of its own: it is offered as a row,
   * labelled by this, after the rows it matches — first when there are none.
   */
  typed?: (text: string) => string;
  /** The lists, by prefix; `title`, `placeholder`, `items` and `search` are then the empty prefix's defaults. */
  modes?: PickerMode[];
  /** What the field holds when it opens: `>` opens the palette on the commands. */
  initial?: string;
}

/** The value of the row `typed` offers, which no item has. */
const TYPED = "\u0000typed";

let current: Picker | null = null;

/** Whether a picker owns the keyboard, for a caller deciding what a key means. */
export function isPickerOpen(): boolean {
  return current !== null;
}

/** Open the picker and resolve with the chosen value, or `null`. */
export function openPicker(options: PickerOptions): Promise<string | null> {
  return open({ ...options, multiple: false }).then((values) => values?.[0] ?? null);
}

/** Open the picker for several rows; resolves with their values, or `null`. */
export function openMultiPicker(options: PickerOptions): Promise<string[] | null> {
  return open({ ...options, multiple: true });
}

function open(options: PickerOptions): Promise<string[] | null> {
  current?.finish(null);
  return new Promise((resolve) => {
    current = new Picker(options, (values) => {
      current = null;
      resolve(values);
    });
  });
}

/** A row as shown: the item, and the letters of its label the query matched. */
interface Shown {
  item: PickerItem;
  positions: number[];
}

class Picker {
  private readonly scrim: HTMLDivElement;
  private readonly title: HTMLSpanElement;
  private readonly input: HTMLInputElement;
  private readonly list: HTMLUListElement;
  private readonly note: HTMLDivElement;
  private readonly modes: PickerMode[];
  private readonly done: (values: string[] | null) => void;
  private readonly multiple: boolean;
  private readonly typed?: (text: string) => string;
  /** The rows chosen so far, with `multiple`. */
  private readonly chosen: PickerItem[] = [];
  private readonly chips: HTMLDivElement;
  private readonly onKeyDown: (e: KeyboardEvent) => void;
  private readonly restoreFocus: Element | null;
  private shown: Shown[] = [];
  /** The list in use, and its items as they were when it was chosen. */
  private mode: PickerMode;
  private items: PickerItem[] = [];
  /** What the mode's `search` found for the query typed, and that query. */
  private found: { query: string; items: PickerItem[] } = { query: "", items: [] };
  private searchTimer: number | undefined;
  /** `search` has not answered for what is typed now: an empty list is not yet "no match". */
  private searching = false;
  /** Enter came while it had not: the first row is taken once it has. */
  private acceptWhenFound = false;
  private highlighted = 0;
  private closed = false;

  constructor(options: PickerOptions, done: (values: string[] | null) => void) {
    this.multiple = options.multiple ?? false;
    this.typed = options.typed;
    this.modes = [
      {
        prefix: "",
        title: options.title,
        placeholder: options.placeholder,
        items: () => options.items,
        search: options.search,
        searchDelay: options.searchDelay,
      },
      ...(options.modes ?? []).filter((m) => m.prefix !== ""),
    ];
    const plain = options.modes?.find((m) => m.prefix === "");
    if (plain) this.modes[0] = plain;
    this.mode = this.modes[0]!;
    this.done = done;
    this.restoreFocus = document.activeElement;

    this.scrim = document.createElement("div");
    this.scrim.className = "dp-scrim";
    const panel = document.createElement("div");
    panel.className = `dp picker${options.modes ? " palette" : ""}`;
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");

    const header = document.createElement("div");
    header.className = "dp-head";
    this.title = document.createElement("span");
    this.title.className = "dp-title";
    header.append(this.title);

    this.input = document.createElement("input");
    this.input.type = "text";
    this.input.className = "dp-input";
    this.input.spellcheck = false;
    this.input.autocomplete = "off";
    this.input.setAttribute("role", "combobox");
    this.input.setAttribute("aria-expanded", "true");
    this.input.setAttribute("aria-controls", "dp-options");
    this.input.value = options.initial ?? "";

    this.list = document.createElement("ul");
    this.list.className = "dp-list";
    this.list.id = "dp-options";
    this.list.setAttribute("role", "listbox");

    this.note = document.createElement("div");
    this.note.className = "dp-note";
    this.note.hidden = true;

    this.chips = document.createElement("div");
    this.chips.className = "dp-chips";
    panel.append(header, this.chips, this.input, this.list, this.note);
    this.scrim.append(panel);
    document.body.append(this.scrim);

    this.input.addEventListener("input", () => {
      this.askSearch();
      this.filter();
    });
    this.list.addEventListener("click", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-index]");
      if (!row) return;
      this.highlighted = Number(row.dataset.index);
      if (this.multiple) this.add();
      else this.accept();
      this.input.focus();
    });
    this.chips.addEventListener("mousedown", (e) => {
      const chip = (e.target as HTMLElement).closest<HTMLElement>("[data-chip]");
      if (!chip) return;
      e.preventDefault();
      this.chosen.splice(Number(chip.dataset.chip), 1);
      this.filter();
    });
    this.scrim.addEventListener("mousedown", (e) => {
      if (e.target === this.scrim) this.finish(null);
    });

    this.onKeyDown = (e) => this.handleKey(e);
    window.addEventListener("keydown", this.onKeyDown, true);

    this.askSearch();
    this.filter();
    this.input.focus();
  }

  /** The list the field's prefix chooses, and the query after the prefix. */
  private parse(): { mode: PickerMode; query: string } {
    const value = this.input.value;
    const mode = this.modes.find((m) => m.prefix !== "" && value.startsWith(m.prefix)) ?? this.modes[0]!;
    return { mode, query: value.slice(mode.prefix.length).trim() };
  }

  /** Ask the mode's `search` for the query typed, once typing has paused. */
  private askSearch(): void {
    const { mode, query } = this.parse();
    window.clearTimeout(this.searchTimer);
    this.acceptWhenFound = false;
    this.searching = !!mode.search;
    if (!mode.search) return;
    const ask = (): void => {
      // A lookup that failed has said so in the status line; here it found nothing.
      void mode.search!(query).catch(() => [] as PickerItem[]).then((items) => {
        const now = this.parse();
        if (this.closed || now.mode !== mode || now.query !== query) return;
        this.found = { query, items };
        this.searching = false;
        this.filter();
        if (this.acceptWhenFound) {
          this.acceptWhenFound = false;
          this.accept();
        }
      });
    };
    const delay = mode.searchDelay ?? 250;
    if (delay <= 0) ask();
    else this.searchTimer = window.setTimeout(ask, delay);
  }

  private filter(): void {
    const { mode, query } = this.parse();
    if (mode !== this.mode || this.items.length === 0) {
      if (mode !== this.mode) this.found = { query: "", items: [] };
      this.mode = mode;
      this.items = mode.items();
    }
    this.title.textContent = mode.title;
    this.input.placeholder = mode.placeholder ?? "";
    const scored = this.items
      .map((item) => {
        const label = item.label ?? item.value;
        const match = fuzzyMatchLabel(query, label, item.keywords);
        return match && { item, positions: match.positions.filter((at) => at < label.length), score: match.score - (item.boost ?? 0) };
      })
      .filter((s): s is Shown & { score: number } => s !== null)
      .sort((a, b) => a.score - b.score);
    // What the search found matched on the far side, perhaps by a name the
    // label does not show, so it is listed as it came, after the rest.
    const extra = this.found.query === query
      ? this.found.items.filter((item) => !scored.some((s) => s.item.value === item.value))
      : [];
    const taken = new Set(this.chosen.map((item) => item.value));
    this.shown = [...scored, ...extra.map((item) => ({ item, positions: [] }))].filter((s) => !taken.has(s.item.value));
    if (this.typed && query && mode.prefix === "") {
      const exact = this.shown.some((s) => (s.item.label ?? s.item.value).toLowerCase() === query.toLowerCase());
      const row = { item: { value: TYPED, label: this.typed(query) }, positions: [] };
      if (!exact) this.shown = this.shown.length > 0 ? [...this.shown, row] : [row];
    }
    this.highlighted = 0;
    this.note.hidden = true;
    this.render(query === "");
  }

  private render(sections = false): void {
    this.chips.innerHTML = this.chosen
      .map(
        (item, i) =>
          `<span class="dp-chip" data-chip="${i}" title="Take back">${escape(item.label ?? item.value)}<span class="x">×</span></span>`,
      )
      .join("");
    this.chips.hidden = this.chosen.length === 0;
    if (this.shown.length === 0) {
      this.list.innerHTML = `<li class="dp-empty">${this.searching ? "…" : "no match"}</li>`;
      this.input.removeAttribute("aria-activedescendant");
      return;
    }
    let section: string | undefined;
    this.list.innerHTML = this.shown
      .map(({ item, positions }, i) => {
        const heading = sections && item.section && item.section !== section
          ? `<li class="dp-section" role="presentation">${escape(item.section)}</li>`
          : "";
        if (sections) section = item.section;
        const label = item.label ?? item.value;
        const shown = positions.length === 0
          ? escape(label)
          : [...label].map((ch, at) => (positions.includes(at) ? `<b>${escape(ch)}</b>` : escape(ch))).join("");
        const classes = [i === this.highlighted ? "highlighted" : "", item.disabled ? "disabled" : ""].filter(Boolean);
        return (
          heading +
          `<li id="dp-option-${i}" role="option" data-index="${i}" aria-selected="${i === this.highlighted}"` +
          `${item.disabled ? ` aria-disabled="true" title="${escape(item.disabled)}"` : ""}` +
          `${classes.length ? ` class="${classes.join(" ")}"` : ""}>` +
          `<span class="dp-primary">${shown}</span>` +
          (item.detail ? `<span class="dp-secondary">${escape(item.detail)}</span>` : "") +
          (item.keys ? `<kbd>${escape(item.keys)}</kbd>` : "") +
          `</li>`
        );
      })
      .join("");
    this.input.setAttribute("aria-activedescendant", `dp-option-${this.highlighted}`);
    this.list
      .querySelector<HTMLElement>("li.highlighted")
      ?.scrollIntoView({ block: "nearest" });
  }

  private move(delta: number): void {
    if (this.shown.length === 0) return;
    this.highlighted = (this.highlighted + delta + this.shown.length) % this.shown.length;
    this.note.hidden = true;
    this.render(this.parse().query === "");
  }

  /** The value of a row: what it holds, or the text typed for the row `typed` offers. */
  private valueOf(item: PickerItem): string {
    return item.value === TYPED ? this.parse().query : item.value;
  }

  /** Whether the row can be chosen; when not, say why under the list and keep the picker open. */
  private choosable(item: PickerItem | undefined): item is PickerItem {
    if (!item) return false;
    if (!item.disabled) return true;
    this.note.textContent = `${item.label ?? item.value}: ${item.disabled}`;
    this.note.hidden = false;
    return false;
  }

  private accept(): void {
    const chosen = this.shown[this.highlighted]?.item;
    if (!chosen) {
      // Typed and entered in one motion: the answer is taken when it is in.
      if (this.searching) this.acceptWhenFound = true;
      else this.finish(null);
      return;
    }
    if (this.choosable(chosen)) this.finish([this.valueOf(chosen)]);
  }

  /** Add the highlighted row to the chosen ones, and start the field afresh. */
  private add(): void {
    const item = this.shown[this.highlighted]?.item;
    if (item && !this.choosable(item)) return;
    if (item) this.chosen.push({ ...item, value: this.valueOf(item) });
    this.input.value = "";
    this.found = { query: "", items: [] };
    this.filter();
  }

  /** Done with `multiple`: everything chosen, or the row under the cursor alone. */
  private acceptMany(): void {
    if (this.chosen.length === 0) {
      this.accept();
      return;
    }
    this.finish(this.chosen.map((item) => item.value));
  }

  private handleKey(e: KeyboardEvent): void {
    const key = eventKey(e);
    switch (key) {
      case "<esc>":
        e.preventDefault();
        e.stopPropagation();
        this.finish(null);
        return;
      case "<enter>":
        e.preventDefault();
        e.stopPropagation();
        if (!this.multiple) this.accept();
        else if (this.input.value.trim() === "" && this.chosen.length > 0) this.acceptMany();
        else this.add();
        return;
      case "<d-enter>":
        e.preventDefault();
        e.stopPropagation();
        if (this.multiple && this.input.value.trim() !== "") this.add();
        if (this.multiple) this.acceptMany();
        else this.accept();
        return;
      case "<backspace>":
        e.stopPropagation();
        if (this.multiple && this.input.value === "" && this.chosen.length > 0) {
          e.preventDefault();
          this.chosen.pop();
          this.filter();
        }
        return;
      case "<down>":
      case "<c-n>":
      case "<tab>":
        e.preventDefault();
        e.stopPropagation();
        this.move(1);
        return;
      case "<up>":
      case "<c-p>":
      case "<s-tab>":
        e.preventDefault();
        e.stopPropagation();
        this.move(-1);
        return;
      default:
        // Everything else is typing, and belongs to the field — which has
        // focus, so the browser delivers it there once this returns. Stopped
        // from bubbling so the app's own keymap never sees it.
        e.stopPropagation();
    }
  }

  finish(value: string[] | null): void {
    if (this.closed) return;
    this.closed = true;
    window.clearTimeout(this.searchTimer);
    window.removeEventListener("keydown", this.onKeyDown, true);
    this.scrim.remove();
    if (this.restoreFocus instanceof HTMLElement) this.restoreFocus.focus();
    this.done(value);
  }
}
