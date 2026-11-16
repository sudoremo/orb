/**
 * The `?` overlay: every key of every mode, found by typing.
 *
 * Drawn over the whole window rather than in the reader, so it opens the same
 * way from mail, from a task and from a chat thread, and scrolls on its own.
 * It opens on Here — every key in force where the keyboard was — and has a tab
 * for each binding table, which shows what that table adds to the one beneath
 * it. Every row comes from the tables in force, so a key rebound in config.yml
 * is listed as it now is.
 *
 * Like the picker, it owns the keyboard while open: the field takes the typing,
 * and ⏎ runs the row under the cursor, so a shortcut found here can be tried
 * at once.
 */

import { COMMAND_DOCS, COMMAND_GROUP, COMMAND_GROUPS } from "./commanddocs";
import { datePickerBindings } from "./datepicker";
import { escape, icon } from "./format";
import { keysOf } from "./keyhints";
import { eventKey, spellKey } from "./keys";
import { state } from "./state";

import type { KeyContext } from "./commands";

export interface HelpOptions {
  /** Where the keyboard was when `?` was pressed. */
  context: KeyContext;
  /** Every binding in force there, the tables layered as the keymap layers them. */
  bindings: Record<string, string>;
  /** Run a command string, as a key bound to it would. */
  run: (command: string) => void;
  /** Open the command line with a command typed, for one that needs words. */
  prompt: (prefix: string) => void;
}

interface Row {
  /** Bindings, canonical: `gg`, `<c-d>`; or keys spelled already, for `fixed`. */
  keys: string[];
  /** The command string the keys run; empty for keys no table holds. */
  command: string;
  does: string;
  group: string;
  /** Keys built into the window rather than read from a table. */
  fixed?: boolean;
}

interface Tab {
  id: string;
  title: string;
  /** What the tab lists, in a sentence. */
  about: string;
  rows: Row[];
  /** Whether ⏎ may run a row: not for keys that only mean something in a field. */
  runnable: boolean;
}

const CONTEXT_TITLES: Record<KeyContext, string> = {
  mail: "Mail list",
  reader: "Reader",
  tasks: "Task list",
  "task-detail": "Task detail",
  chat: "Chat",
};

/** The date picker's commands, which are its own and not in `COMMAND_DOCS`. */
const DATE_PICKER_DOCS: Record<string, string> = {
  accept: "Take the date under the cursor",
  cancel: "Close without a date",
  "clear-date": "Clear the date, from the calendar",
  "move-down": "Next suggestion, or a week later",
  "move-up": "Previous suggestion, or a week earlier",
  "move-left": "A day earlier",
  "move-right": "A day later",
  "next-month": "A month later",
  "prev-month": "A month earlier",
  "next-year": "A year later",
  "prev-year": "A year earlier",
  today: "Go to today",
};

const EVERYWHERE = "Everywhere";

/** Keys the window handles itself, outside every binding table. */
const WINDOW_KEYS: Row[] = [
  { keys: ["1–9"], command: "", does: "Before a key: do it that many times (5j)", group: EVERYWHERE, fixed: true },
  { keys: ["⌘1–⌘9"], command: "", does: "Go to a tab of the window by its place", group: EVERYWHERE, fixed: true },
  { keys: ["⌥ö", "⌥ä"], command: "", does: "Go to the previous or the next tab", group: EVERYWHERE, fixed: true },
];

const COMPOSER_KEYS: Row[] = [
  { keys: ["⇧⏎"], command: "", does: "Start a new line", group: "Chat", fixed: true },
  { keys: ["↑"], command: "", does: "In an empty composer: edit your last message", group: "Chat", fixed: true },
];

/** Where each group sorts: the order of `COMMAND_GROUPS`, then the rest. */
const GROUP_ORDER = [...Object.keys(COMMAND_GROUPS), "Date picker", EVERYWHERE];
/** Where each command sorts inside its group: the order of `COMMAND_DOCS`. */
const COMMAND_ORDER = new Map(Object.keys(COMMAND_DOCS).map((name, i) => [name, i]));

const nameOf = (command: string): string => command.trim().split(/\s+/)[0] ?? "";

/** Plain keys first: `j` before `<down>`, `gg` before `<home>`. */
const byPlainness = (a: string, b: string): number =>
  Number(a.startsWith("<")) - Number(b.startsWith("<")) || a.length - b.length;

/** What a command of the user's own is called: its title, else what it runs. */
const ownTitle = (name: string | undefined): string | undefined => {
  const own = state.userCommands[name ?? ""];
  return own && (own.title ?? own.run);
};

/** What a command string does: every step of a chain, each with its arguments. */
function describeCommand(command: string): string {
  return command
    .split("&&")
    .map((step, i) => {
      const [name, ...args] = step.trim().split(/\s+/);
      const doc = COMMAND_DOCS[name ?? ""]?.does ?? ownTitle(name);
      const does = doc && i > 0 ? doc[0]!.toLowerCase() + doc.slice(1) : (doc ?? step.trim());
      return args.length ? `${does} (${args.join(" ")})` : does;
    })
    .join(", then ");
}

/** A table as rows: one per command, with every key bound to it. */
function rowsOf(table: Record<string, string>, docs?: Record<string, string>, group?: string): Row[] {
  const byCommand = new Map<string, string[]>();
  for (const [key, command] of Object.entries(table)) {
    if (nameOf(command) === "noop") continue;
    byCommand.set(command, [...(byCommand.get(command) ?? []), key]);
  }
  const rows: Row[] = [...byCommand.entries()].map(([command, keys]) => ({
    keys: keys.sort(byPlainness),
    command,
    does: docs?.[command] ?? describeCommand(command),
    group: group ?? COMMAND_GROUP[nameOf(command)] ?? "Other",
  }));
  return mergeDirections(rows);
}

/**
 * One row for a command bound once per direction — `focus-pane left` on ⌃h,
 * `focus-pane down` on ⌃j — rather than four rows saying the same thing.
 */
function mergeDirections(rows: Row[]): Row[] {
  const oneArg = (row: Row) => !row.command.includes("&&") && row.command.trim().split(/\s+/).length === 2;
  const byName = new Map<string, Row[]>();
  for (const row of rows.filter(oneArg)) byName.set(nameOf(row.command), [...(byName.get(nameOf(row.command)) ?? []), row]);
  const merged = new Set<Row>();
  const out: Row[] = [];
  for (const row of rows) {
    const siblings = byName.get(nameOf(row.command)) ?? [];
    if (!oneArg(row) || siblings.length < 2 || siblings.some((r) => r.keys.length !== 1)) {
      out.push(row);
      continue;
    }
    if (merged.has(row)) continue;
    siblings.forEach((r) => merged.add(r));
    const name = nameOf(row.command);
    out.push({
      keys: siblings.map((r) => r.keys[0]!),
      command: name,
      does: `${COMMAND_DOCS[name]?.does ?? name}: ${siblings.map((r) => r.command.split(/\s+/)[1]).join(", ")}`,
      group: row.group,
    });
  }
  return out;
}

/** Grouped and in the order the command docs give, which is the order of learning. */
function ordered(rows: Row[]): Row[] {
  const groupAt = (g: string) => {
    const i = GROUP_ORDER.indexOf(g);
    return i === -1 ? GROUP_ORDER.length : i;
  };
  return rows.sort(
    (a, b) =>
      groupAt(a.group) - groupAt(b.group) ||
      (COMMAND_ORDER.get(nameOf(a.command)) ?? 1e6) - (COMMAND_ORDER.get(nameOf(b.command)) ?? 1e6) ||
      a.command.localeCompare(b.command),
  );
}

function buildTabs(context: KeyContext, here: Record<string, string>): Tab[] {
  const b = state.bindings;
  const tabs: Tab[] = [
    {
      id: "here",
      title: `Here · ${CONTEXT_TITLES[context]}`,
      about: `Every key that works in the ${CONTEXT_TITLES[context].toLowerCase()} right now.`,
      rows: ordered([...rowsOf(here), ...WINDOW_KEYS]),
      runnable: true,
    },
    {
      id: "mail",
      title: "Mail",
      about: "The message list, and the keys every other mode builds on (bindings.index_normal).",
      rows: ordered(rowsOf(b.index_normal)),
      runnable: true,
    },
    {
      id: "reader",
      title: "Reader",
      about: "With a message open: what the reader adds to the mail keys (bindings.index_detail).",
      rows: ordered(rowsOf(b.index_detail)),
      runnable: true,
    },
    {
      id: "tasks",
      title: "Tasks",
      about: "In a task view: what it adds to the mail keys, or means instead (bindings.task_normal).",
      rows: ordered(rowsOf(b.task_normal)),
      runnable: true,
    },
    {
      id: "task-detail",
      title: "Task detail",
      about: "With a task open: what it adds to the task keys (bindings.task_detail).",
      rows: ordered(rowsOf(b.task_detail)),
      runnable: true,
    },
    {
      id: "task-edit",
      title: "Editing",
      about: "While a task's title or notes are being typed (bindings.task_edit).",
      rows: ordered(rowsOf(b.task_edit)),
      runnable: false,
    },
  ];
  if (state.chat.available) {
    tabs.push(
      {
        id: "chat",
        title: "Chat",
        about: "In Waiting and in a room: what chat adds to the mail keys (bindings.chat_normal).",
        rows: ordered(rowsOf(b.chat_normal)),
        runnable: true,
      },
      {
        id: "composer",
        title: "Composer",
        about: "While a chat message is being typed (bindings.chat_compose).",
        rows: ordered([...rowsOf(b.chat_compose), ...COMPOSER_KEYS]),
        runnable: false,
      },
    );
  }
  tabs.push({
    id: "dates",
    title: "Dates",
    about: "In the date picker, which opens for a postpone, a due date or a reminder (bindings.date_picker).",
    rows: ordered(rowsOf(datePickerBindings(), DATE_PICKER_DOCS, "Date picker")),
    runnable: false,
  });
  // Every command, bound or not, with the keys it has where the keyboard is:
  // the answer to "is there a command for this", which no key table gives.
  tabs.push({
    id: "commands",
    title: "All commands",
    about: "Every command, with its keys here if it has any. Type : and the name to run one that has none.",
    rows: ordered(
      Object.entries(COMMAND_DOCS)
        .filter(([name]) => name !== "noop")
        .map(([name, doc]) => ({
          keys: Object.keys(here).filter((key) => here[key] === name),
          command: doc.args ? `${name} ${doc.args}` : name,
          does: doc.does,
          group: COMMAND_GROUP[name] ?? "Other",
        })),
    ),
    runnable: true,
  });
  return tabs;
}

/** A binding as keycaps: `gg` as two, `<c-d>` as ⌃d. */
function keycaps(row: Row): string {
  const one = (key: string) =>
    row.fixed
      ? `<kbd>${escape(key)}</kbd>`
      : keysOf(key)
          .map((k) => `<kbd>${escape(spellKey(k))}</kbd>`)
          .join("");
  if (row.keys.length === 0) return `<span class="hk-none">:</span>`;
  return row.keys
    .slice(0, 3)
    .map((key) => `<span class="hk-seq">${one(key)}</span>`)
    .join(`<span class="hk-or">or</span>`);
}

/** Every spelling a row's keys go by, for a query that is a key: `gg`, `⌃d`, `<c-d>`. */
function spellings(row: Row): string[] {
  return row.keys.flatMap((key) => {
    const spelled = keysOf(key).map(spellKey);
    return [key, spelled.join(""), spelled.join(" ")];
  });
}

/**
 * How well `row` answers `query`: `null` for not at all, lower for better.
 * A query that is one of the row's keys is the best answer — "what does `d`
 * do" — and a single character matches nothing else, or every row with a `d`
 * in its description would.
 */
function score(row: Row, query: string): number | null {
  if (spellings(row).includes(query)) return 0;
  if ([...query].length === 1) return null;
  // Every term starts a word, so `arch` finds archive and not search.
  const words = `${row.does} ${row.command} ${row.group}`.toLowerCase().split(/[^\p{L}\p{N}]+/u);
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!terms.every((t) => words.some((w) => w.startsWith(t)))) return null;
  // A command named by what was typed ahead of one that only mentions it.
  return terms.every((t) => nameOf(row.command).startsWith(t) || row.does.toLowerCase().startsWith(t)) ? 1 : 2;
}

let current: Help | null = null;

/** Open the overlay, or close it when it is open: `?` and the menu toggle it. */
export function toggleHelp(options: HelpOptions): void {
  if (current) current.close();
  else current = new Help(options);
}

class Help {
  private readonly scrim: HTMLDivElement;
  private readonly input: HTMLInputElement;
  private readonly tabBar: HTMLDivElement;
  private readonly body: HTMLDivElement;
  private readonly footer: HTMLDivElement;
  private readonly tabs: Tab[];
  private readonly onKeyDown: (e: KeyboardEvent) => void;
  private readonly restoreFocus: Element | null;
  private tab = 0;
  /** The rows on screen, in reading order, and the one under the cursor. */
  private shown: Row[] = [];
  private cursor = 0;

  constructor(private readonly options: HelpOptions) {
    this.tabs = buildTabs(options.context, options.bindings);
    this.restoreFocus = document.activeElement;

    this.scrim = document.createElement("div");
    this.scrim.className = "help-scrim";
    this.scrim.innerHTML = `
      <div class="help" role="dialog" aria-modal="true" aria-label="Keyboard shortcuts">
        <div class="help-head">
          <div class="help-title">${icon("fa-keyboard", "fa-fw")} Keyboard Shortcuts</div>
          <label class="help-search">${icon("fa-magnifying-glass", "fa-fw")}
            <input type="text" spellcheck="false" autocomplete="off"
              placeholder="Type a key, or what you want to do" aria-label="Find a shortcut">
          </label>
        </div>
        <div class="help-tabs" role="tablist"></div>
        <div class="help-body"></div>
        <div class="help-foot"></div>
      </div>`;
    this.input = this.scrim.querySelector("input")!;
    this.tabBar = this.scrim.querySelector(".help-tabs")!;
    this.body = this.scrim.querySelector(".help-body")!;
    this.footer = this.scrim.querySelector(".help-foot")!;
    document.body.append(this.scrim);

    this.input.addEventListener("input", () => {
      this.cursor = 0;
      this.render();
    });
    this.tabBar.addEventListener("mousedown", (e) => {
      const tab = (e.target as HTMLElement).closest<HTMLElement>("[data-tab]");
      if (!tab) return;
      e.preventDefault();
      this.select(Number(tab.dataset.tab));
    });
    this.body.addEventListener("mousedown", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-row]");
      if (!row) return;
      e.preventDefault();
      this.cursor = Number(row.dataset.row);
      this.render(false);
    });
    this.body.addEventListener("dblclick", () => this.runCursor());
    this.scrim.addEventListener("mousedown", (e) => {
      if (e.target === this.scrim) this.close();
    });

    this.onKeyDown = (e) => this.handleKey(e);
    window.addEventListener("keydown", this.onKeyDown, true);

    this.render();
    this.input.focus();
  }

  private query(): string {
    return this.input.value.trim();
  }

  /** The rows of a tab that answer the query, best first within each group. */
  private matches(tab: Tab): Row[] {
    const query = this.query();
    if (!query) return tab.rows;
    return tab.rows
      .map((row) => ({ row, score: score(row, query) }))
      .filter((s): s is { row: Row; score: number } => s.score !== null)
      .sort((a, b) => a.score - b.score)
      .map((s) => s.row);
  }

  private select(tab: number): void {
    this.tab = (tab + this.tabs.length) % this.tabs.length;
    this.cursor = 0;
    this.body.scrollTop = 0;
    this.render();
  }

  private render(scroll = true): void {
    const query = this.query();
    const counts = this.tabs.map((tab) => this.matches(tab).length);
    this.tabBar.innerHTML = this.tabs
      .map(
        (tab, i) =>
          `<button type="button" role="tab" tabindex="-1" data-tab="${i}" aria-selected="${i === this.tab}"` +
          `${query && counts[i] === 0 ? ' class="none"' : ""}>${escape(tab.title)}` +
          `${query ? `<span class="n">${counts[i]}</span>` : ""}</button>`,
      )
      .join("");

    const tab = this.tabs[this.tab]!;
    const rows = this.matches(tab);
    // A key typed is answered best-first, across groups; browsing keeps the
    // groups, which is how the list is learnt.
    const keyQuery = rows.length > 0 && query !== "" && score(rows[0]!, query) === 0;
    this.shown = keyQuery ? rows : rows.slice().sort((a, b) => tab.rows.indexOf(a) - tab.rows.indexOf(b));
    this.cursor = Math.min(this.cursor, Math.max(0, this.shown.length - 1));

    if (this.shown.length === 0) {
      const elsewhere = this.tabs.filter((_, i) => counts[i]! > 0).map((t) => t.title);
      this.body.innerHTML = `<div class="help-empty">No shortcut here matches “${escape(query)}”.${
        elsewhere.length ? ` Try ${escape(elsewhere.join(", "))}: ⇥ moves there.` : ""
      }</div>`;
    } else {
      const groups: string[] = [];
      let open = "";
      this.shown.forEach((row, i) => {
        const group = keyQuery ? "Matches" : row.group;
        if (group !== open) {
          if (open) groups.push(`</section>`);
          groups.push(`<section class="help-group"><h3>${escape(group)}</h3>`);
          open = group;
        }
        groups.push(
          `<div class="hk-row${i === this.cursor ? " on" : ""}" data-row="${i}"` +
            `${row.command ? ` title=":${escape(row.command)}"` : ""}>` +
            `<span class="hk-keys">${keycaps(row)}</span><span class="hk-does">${escape(row.does)}</span></div>`,
        );
      });
      groups.push(`</section>`);
      this.body.innerHTML = `<p class="help-about">${escape(tab.about)}</p><div class="help-cols">${groups.join("")}</div>`;
    }
    this.renderFooter(tab);
    if (scroll) this.body.querySelector(".hk-row.on")?.scrollIntoView({ block: "nearest" });
  }

  private renderFooter(tab: Tab): void {
    const row = this.shown[this.cursor];
    const command = row?.command
      ? `<code>:${escape(row.command)}</code>${tab.runnable ? `<span class="dim">⏎ runs it</span>` : ""}`
      : "";
    this.footer.innerHTML =
      `<span class="help-cmd">${command}</span>` +
      `<span class="help-legend"><kbd>↑</kbd><kbd>↓</kbd> choose <kbd>⇥</kbd> next tab <kbd>esc</kbd> close</span>`;
  }

  private move(delta: number): void {
    if (this.shown.length === 0) return;
    this.cursor = (this.cursor + delta + this.shown.length) % this.shown.length;
    this.render();
  }

  /** Close, then run the row's command where the keyboard was. */
  private runCursor(): void {
    const tab = this.tabs[this.tab]!;
    const row = this.shown[this.cursor];
    if (!row?.command || !tab.runnable) return;
    this.close();
    // A command that wants words is typed rather than run with none.
    if (/[<[]/.test(row.command)) this.options.prompt(`${nameOf(row.command)} `);
    else this.options.run(row.command);
  }

  private handleKey(e: KeyboardEvent): void {
    const key = eventKey(e);
    // Every key is the overlay's while it is open: the field takes what is
    // typed, and nothing reaches the keymap behind it.
    e.stopPropagation();
    const handled = (): void => e.preventDefault();
    switch (key) {
      case "<esc>":
        handled();
        if (this.input.value) {
          this.input.value = "";
          this.cursor = 0;
          this.render();
        } else this.close();
        return;
      case "?":
        if (this.input.value) return;
        handled();
        this.close();
        return;
      case "<enter>":
        handled();
        this.runCursor();
        return;
      case "<down>":
      case "<c-n>":
      case "<c-j>":
        handled();
        this.move(1);
        return;
      case "<up>":
      case "<c-p>":
      case "<c-k>":
        handled();
        this.move(-1);
        return;
      case "<tab>":
        handled();
        this.select(this.tab + 1);
        return;
      case "<s-tab>":
        handled();
        this.select(this.tab - 1);
        return;
      case "<pagedown>":
      case "<c-d>":
        handled();
        this.body.scrollBy({ top: this.body.clientHeight * 0.8 });
        return;
      case "<pageup>":
      case "<c-u>":
        handled();
        this.body.scrollBy({ top: -this.body.clientHeight * 0.8 });
        return;
    }
  }

  close(): void {
    if (current !== this) return;
    current = null;
    window.removeEventListener("keydown", this.onKeyDown, true);
    this.scrim.remove();
    if (this.restoreFocus instanceof HTMLElement) this.restoreFocus.focus({ preventScroll: true });
  }
}
