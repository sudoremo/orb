/**
 * The date picker (§12).
 *
 * One modal, three jobs: name a date by typing at it, point at a date on a
 * calendar, or take the date away again. It is opened for a postpone date, for
 * a due date and — in reminder mode — for a date *and* a time, because a
 * reminder is a wall-clock promise and not a day (§5.3).
 *
 * The shape of the thing is fixed by the spec and worth restating, because
 * every decision below follows from it: a text input at the top with the
 * keyboard already in it, and a calendar underneath. Typing replaces the
 * calendar with the suggestion list; clearing the input brings the calendar
 * back. So the two halves are never both live, `<enter>` is unambiguous — the
 * highlighted suggestion, or the calendar's day when nothing is typed — and
 * there is never a hidden second cursor to reason about.
 *
 * **No date parsing happens here.** Every suggestion comes from the Rust fuzzy
 * parser (`src/tasks/dates.rs`) through a Tauri command. That parser is the
 * tested one — property tests over every day of two years, both locales — and a
 * second implementation in TypeScript would be a second set of answers to
 * "what does `Sept 2028 12` mean", diverging silently from the day it was
 * written. What this file computes is the calendar *grid* (which month sits
 * where) and the local UTC offset for a reminder instant; neither is parsing.
 *
 * Keys come from `bindings.date_picker` in `config.yml`, read through the
 * existing `bindings` command, exactly like every other mode in Orb. The one
 * rule that mode has and the others do not: **a binding spelled as a bare
 * character is never honoured while the picker is open**, because the input has
 * the keyboard and `s`, `d` and `3` have to reach it — they are the first
 * letters of "sept", "3d" and half the vocabulary in §12.1. Only bracketed keys
 * (`<enter>`, `<c-d>`, `<down>`) can be bindings here, and anything unbound
 * falls through to the field so `<backspace>`, `<c-a>` and ⌘V keep working.
 */

import { addDays, addMonths, civil, isCivilDate, localToday, pad, parts, weekday } from "./dates";
import { invoke } from "@tauri-apps/api/core";
import { canonicalTable, eventKey, spellKey } from "./keys";

// ---------------------------------------------------------------------------
// The contract with the caller
// ---------------------------------------------------------------------------

/** What the picker was asked for, which decides whether a time is collected. */
export type DatePickerMode = "date" | "reminder";

/**
 * What came back.
 *
 * `clear` is a first-class answer, not a null: unsetting a date is a decision
 * the user made and it has to be as easy to express as setting one (§12), so
 * the caller has to tell it apart from a cancel — which is `null`.
 */
export type DatePickerResult =
  | { kind: "clear"; field: string | null }
  | {
      kind: "date";
      /** Which of `fields` the date is for, or null when none were offered. */
      field: string | null;
      /** Local civil date, `YYYY-MM-DD`, as the task commands take it. */
      date: string;
      /** Local wall-clock time, `HH:MM`. Always null outside reminder mode. */
      time: string | null;
      /**
       * The same instant as RFC3339 with the machine's offset, which is what
       * `add_task_reminder` takes. Null outside reminder mode.
       */
      at: string | null;
    };

/** A date the picker can set: its id for the caller, its heading for the user. */
export interface DatePickerField {
  id: string;
  title: string;
}

/** One reading of the input, as the Rust parser produces it. */
export interface DateSuggestion {
  /** `YYYY-MM-DD`. */
  date: string;
  /** `HH:MM` or `HH:MM:SS`; null from the date-only parser. */
  time: string | null;
  /** Left-aligned label — "Tomorrow", "September 12, 2028". */
  primary: string;
  /** Right-aligned label in a lighter colour — "Aug 7", "in 5 weeks". */
  secondary: string;
}

export interface DatePickerOptions {
  /** Whether a time is collected as well as a date. Default `"date"`. */
  mode?: DatePickerMode;
  /** Heading — "Postpone until", "Due", "Remind me". */
  title?: string;
  /**
   * The dates the picker can set, when there is more than one: `switch-field`
   * steps through them, the heading names the one in force, and the answer
   * says which it was. The first is where the picker opens; `title` is then
   * unused.
   */
  fields?: DatePickerField[];
  /**
   * Where the calendar starts and which day is marked as the current value.
   * `YYYY-MM-DD`, or null for "no date set", which starts on today.
   */
  initial?: string | null;
  /**
   * Language for the calendar's own labels (month name, weekday initials).
   *
   * Defaults to English to match the parser's default `DateLocale`: an English
   * suggestion list under a German month header is a picker that cannot decide
   * what language it is in. The integration passes the configured locale to
   * both sides.
   */
  locale?: string;
  /**
   * First column of the calendar grid: 0 Sunday, 1 Monday. Defaults from the
   * locale, because there is no portable way to ask for it — `weekInfo` is not
   * available everywhere — and getting it wrong shifts every date in the grid
   * by a day, which is the one error a calendar must not make.
   */
  weekStart?: 0 | 1;
  /**
   * Bindings for `bindings.date_picker`, already canonical.
   *
   * Passed in when the caller has read the config already; otherwise the
   * picker asks the `bindings` command itself on first open and caches it.
   */
  bindings?: Record<string, string>;
  /**
   * How to fetch suggestions.
   *
   * A seam, not a policy: the app wraps every `invoke` in a timing/status
   * helper this module has no access to, and a harness stubs the IPC entirely.
   * The default goes straight to the Tauri command.
   */
  suggest?: (input: string, mode: DatePickerMode) => Promise<DateSuggestion[]>;
  /** Where to report a failed lookup. Defaults to the console. */
  onProblem?: (reason: string) => void;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/**
 * Tauri commands backing the two parser entry points, named after them.
 *
 * Both take `{ input }` — a single-word argument, because Tauri renames
 * arguments to camelCase and a two-word one can be misspelled from here — and
 * return `DateSuggestion[]` with `date`/`time` already rendered as strings.
 * The locale and "today" are the command's business: they come from the config
 * and the machine clock, and neither belongs in a keystroke.
 */
const COMMANDS: Record<DatePickerMode, string> = {
  date: "suggest_dates",
  reminder: "suggest_datetimes",
};

/**
 * Wait before asking, so a fast typist asks once instead of six times.
 *
 * The same 90ms the recipient field uses; the parser is microseconds and the
 * cost being smoothed is the round trip, which is identical for both.
 */
const SUGGEST_DEBOUNCE = 90;

/** A reminder with no time of its own means this, matching the parser. */
const DEFAULT_REMINDER_TIME = "09:00";

/**
 * Rows in the calendar grid.
 *
 * Fixed at six rather than fitted to the month: a grid that changes height as
 * you page through it moves the day under the pointer, and a March that needs
 * six rows next to a February that needs four would make the whole dialog jump.
 */
const GRID_ROWS = 6;

/**
 * Default keys, used for anything `bindings.date_picker` does not name.
 *
 * Merged rather than replaced, for the reason `main.ts` merges its own table: a
 * config written before this mode existed would otherwise have no key for
 * anything and the picker would look broken rather than unbound. `noop` turns a
 * default off.
 *
 * Every entry is a bracketed key. That is not a coincidence — see the module
 * comment: a bare character belongs to the input.
 */
const DEFAULT_BINDINGS: Record<string, string> = {
  "<enter>": "accept",
  "<esc>": "cancel",
  "<c-d>": "clear-date",
  "<down>": "move-down",
  "<up>": "move-up",
  "<c-n>": "move-down",
  "<c-p>": "move-up",
  "<tab>": "move-down",
  "<s-tab>": "move-up",
  "<left>": "move-left",
  "<right>": "move-right",
  "<pagedown>": "next-month",
  "<pageup>": "prev-month",
  "<c-f>": "next-month",
  "<c-b>": "prev-month",
  "<c-t>": "today",
  "<home>": "today",
  "<c-e>": "switch-field",
};

// ---------------------------------------------------------------------------
// Civil dates
//
// A date here is a `YYYY-MM-DD` string, the way the task commands and the
// database spell one, and arithmetic on it goes through `Date.UTC`. Not because
// these are UTC dates — they are local civil dates — but because UTC is the one
// timeline with no DST in it: adding a day to a local `Date` across the March
// transition lands on the same day at 23:00, and the calendar would show two
// cells for one date and none for the next. Nothing here ever converts between
// a civil date and an instant; the single place that does is `localRfc3339`,
// and it is a reminder's business alone.
// ---------------------------------------------------------------------------

/** `HH:MM` from whatever precision the parser sent. */
function shortTime(time: string | null): string | null {
  if (!time) return null;
  const [h, m] = time.split(":");
  return h === undefined || m === undefined ? null : `${pad(Number(h))}:${pad(Number(m))}`;
}

/**
 * A local civil date and time as an RFC3339 instant.
 *
 * `add_task_reminder` stores what it is given and derives a Unix timestamp from
 * it, so the offset has to be the one in force *at that moment* — not today's.
 * A reminder set in January for a July morning is an hour out otherwise, which
 * is precisely the class of bug §14 exists to forbid. Constructing a local
 * `Date` for the target and asking it for its own offset is the only thing in
 * the browser that knows the rule; the ambiguous hour that a fall-back
 * transition repeats resolves the way the platform resolves it, which for a
 * reminder is a distinction without a difference.
 */
function localRfc3339(date: string, time: string): string {
  const { year, month, day } = parts(date);
  const [hour, minute, second] = time.split(":").map(Number);
  const local = new Date(year, month - 1, day, hour, minute, second || 0);
  const offset = -local.getTimezoneOffset();
  const sign = offset < 0 ? "-" : "+";
  const magnitude = Math.abs(offset);
  const clock = `${pad(hour)}:${pad(minute)}:${pad(second || 0)}`;
  return `${date}T${clock}${sign}${pad(Math.floor(magnitude / 60))}:${pad(magnitude % 60)}`;
}

// ---------------------------------------------------------------------------
// Bindings
// ---------------------------------------------------------------------------

/** Only the table this mode uses; the rest of the config is not our business. */
interface BindingsResponse {
  date_picker?: Record<string, string> | null;
}

let cachedBindings: Record<string, string> | null = null;

/**
 * Publish the configured `date_picker` table.
 *
 * For the caller that has already read the config at boot: telling the picker
 * saves it a round trip and, more to the point, keeps one answer to "what is
 * `<c-d>` bound to" rather than two tables that could be read at different
 * times and disagree.
 */
export function setDatePickerBindings(table: Record<string, string> | null | undefined): void {
  cachedBindings = { ...DEFAULT_BINDINGS, ...canonicalTable(table) };
}

/** The table in force, for the `?` overlay: the defaults until one is published. */
export function datePickerBindings(): Record<string, string> {
  return cachedBindings ?? DEFAULT_BINDINGS;
}

/**
 * The table to use, asking the core once if nobody has published one.
 *
 * A failure here is not fatal and is not reported: the defaults are a complete
 * keymap, and a picker that refuses to open because the binding table was
 * unreachable would be worse than one whose keys are the documented defaults.
 */
async function bindings(): Promise<Record<string, string>> {
  if (cachedBindings) return cachedBindings;
  try {
    const config = await invoke<BindingsResponse>("bindings");
    cachedBindings = { ...DEFAULT_BINDINGS, ...canonicalTable(config?.date_picker) };
  } catch {
    cachedBindings = { ...DEFAULT_BINDINGS };
  }
  return cachedBindings;
}

// ---------------------------------------------------------------------------
// The picker
// ---------------------------------------------------------------------------

let current: DatePicker | null = null;

/** Whether a picker owns the keyboard, for a caller deciding what a key means. */
export function isDatePickerOpen(): boolean {
  return current !== null;
}

/** Dismiss an open picker as though `<esc>` had been pressed. */
export function closeDatePicker(): void {
  current?.finish(null);
}

/**
 * Open the picker and resolve with what the user chose.
 *
 * Resolves `null` on `<esc>`, on a click outside, and on being displaced by a
 * second picker — every path where the user did not name a date. A caller can
 * therefore treat "not a result" as "change nothing" without inspecting why.
 *
 * ```ts
 * const chosen = await openDatePicker({ title: "Postpone until", initial: task.postpone });
 * if (!chosen) return;
 * const date = chosen.kind === "clear" ? null : chosen.date;
 * await invoke("set_task_postpone_date", { ids, date });
 * ```
 */
export async function openDatePicker(
  options: DatePickerOptions = {},
): Promise<DatePickerResult | null> {
  // Two pickers would mean two things listening for `<enter>`, and the older
  // one is by definition no longer what the user is answering.
  current?.finish(null);

  const table = options.bindings
    ? { ...DEFAULT_BINDINGS, ...canonicalTable(options.bindings) }
    : await bindings();

  return new Promise<DatePickerResult | null>((resolve) => {
    current = new DatePicker(options, table, (result) => {
      current = null;
      resolve(result);
    });
  });
}

class DatePicker {
  private readonly mode: DatePickerMode;
  private readonly locale: string;
  private readonly weekStart: 0 | 1;
  private readonly initial: string | null;
  private readonly onProblem: (reason: string) => void;
  private readonly fetch: (input: string, mode: DatePickerMode) => Promise<DateSuggestion[]>;
  private readonly keys: Record<string, string>;
  private readonly done: (result: DatePickerResult | null) => void;
  private readonly fields: DatePickerField[];
  private field = 0;
  private readonly title: HTMLSpanElement;

  private readonly scrim: HTMLDivElement;
  private readonly input: HTMLInputElement;
  private readonly list: HTMLUListElement;
  private readonly calendar: HTMLDivElement;
  private readonly monthLabel: HTMLSpanElement;
  private readonly grid: HTMLDivElement;
  private readonly footer: HTMLDivElement;

  /** Where the calendar's own cursor is; the day `<enter>` takes when empty. */
  private cursor: string;
  private readonly today = localToday();

  private suggestions: DateSuggestion[] = [];
  private highlighted = 0;
  private timer: number | null = null;
  /** The query the visible list answers, so a late reply cannot overwrite a
   *  newer one. */
  private outstanding = "";
  /** Whether a lookup is between its call and its answer. */
  private inFlight = false;
  /** Set once the answer has been handed over, so it is handed over once. */
  private closed = false;
  /** Set when `<enter>` arrives before the answer does — see `accept`. */
  private acceptWhenReady = false;
  /** Whether the last lookup failed, which is not the same as "found nothing". */
  private failed = false;

  private readonly onKeyDown: (e: KeyboardEvent) => void;
  private readonly restoreFocus: Element | null;

  constructor(
    options: DatePickerOptions,
    keys: Record<string, string>,
    done: (result: DatePickerResult | null) => void,
  ) {
    this.mode = options.mode ?? "date";
    this.locale = options.locale ?? "en";
    this.weekStart = options.weekStart ?? (this.locale.startsWith("de") ? 1 : 0);
    this.initial = isCivilDate(options.initial) ? options.initial : null;
    this.onProblem = options.onProblem ?? ((reason) => console.warn(`date picker: ${reason}`));
    this.fetch = options.suggest ?? defaultSuggest;
    this.keys = keys;
    this.done = done;
    this.fields = options.fields ?? [];
    this.cursor = this.initial ?? this.today;
    this.restoreFocus = document.activeElement;

    this.scrim = document.createElement("div");
    this.scrim.className = "dp-scrim";

    const panel = document.createElement("div");
    panel.className = "dp";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");

    const header = document.createElement("div");
    header.className = "dp-head";
    this.title = document.createElement("span");
    this.title.className = "dp-title";
    this.title.textContent =
      this.fields[0]?.title ?? options.title ?? (this.mode === "reminder" ? "Remind me" : "Date");
    header.append(this.title);

    this.input = document.createElement("input");
    this.input.type = "text";
    this.input.className = "dp-input";
    this.input.spellcheck = false;
    this.input.autocomplete = "off";
    // The placeholder is the documentation for a parser with no visible
    // grammar: four examples from §12.1 say more about what may be typed than
    // any label would.
    this.input.placeholder =
      this.mode === "reminder"
        ? "tomorrow 9am, next tuesday 14:00, in 2 hours"
        : "tomorrow, 3d, next tuesday, 12.9.";

    this.list = document.createElement("ul");
    this.list.className = "dp-list";
    this.list.hidden = true;

    this.calendar = document.createElement("div");
    this.calendar.className = "dp-cal";

    const nav = document.createElement("div");
    nav.className = "dp-nav";
    const back = navButton("‹", "Previous month");
    const forward = navButton("›", "Next month");
    this.monthLabel = document.createElement("span");
    this.monthLabel.className = "dp-month";
    nav.append(back, this.monthLabel, forward);
    back.addEventListener("click", () => this.shiftMonth(-1));
    forward.addEventListener("click", () => this.shiftMonth(1));

    const weekdays = document.createElement("div");
    weekdays.className = "dp-weekdays";
    for (const label of weekdayLabels(this.locale, this.weekStart)) {
      const cell = document.createElement("span");
      cell.textContent = label;
      weekdays.append(cell);
    }

    this.grid = document.createElement("div");
    this.grid.className = "dp-grid";
    this.calendar.append(nav, weekdays, this.grid);

    this.footer = document.createElement("div");
    this.footer.className = "dp-foot";

    panel.append(header, this.input, this.list, this.calendar, this.footer);
    this.scrim.append(panel);
    document.body.append(this.scrim);

    this.bind(panel);
    this.renderCalendar();
    this.renderFooter();
    this.input.focus();

    // The listener is on the window in the capture phase, ahead of the app's
    // own keymap. Nothing here bubbles unless it was ignored, so an unbound key
    // still reaches the field — and the app's global handler bows out anyway
    // once it sees an input as the target.
    this.onKeyDown = (e) => this.handleKey(e);
    window.addEventListener("keydown", this.onKeyDown, true);
  }

  /** Close, and hand the caller its answer exactly once. */
  finish(result: DatePickerResult | null): void {
    // A click that accepts a day and an `<esc>` arriving in the same frame are
    // both real; the second must not resolve the promise a second time.
    if (this.closed) return;
    this.closed = true;
    if (this.timer !== null) window.clearTimeout(this.timer);
    window.removeEventListener("keydown", this.onKeyDown, true);
    this.scrim.remove();
    // The picker took the keyboard from somewhere and owes it back — otherwise
    // the next keystroke after cancelling goes to the document and the message
    // list appears to have stopped responding.
    if (this.restoreFocus instanceof HTMLElement) this.restoreFocus.focus();
    this.done(result);
  }

  // -------------------------------------------------------------------------
  // Input
  // -------------------------------------------------------------------------

  private bind(panel: HTMLElement): void {
    this.input.addEventListener("input", () => this.onTyped());

    // Clicking the backdrop is the mouse spelling of `<esc>`.
    this.scrim.addEventListener("mousedown", (e) => {
      if (e.target === this.scrim) this.finish(null);
    });

    // Every click inside keeps the keyboard where it is: the input is the only
    // place a keystroke may land while this is open.
    panel.addEventListener("mousedown", (e) => {
      if (e.target !== this.input) e.preventDefault();
      this.input.focus();
    });

    this.list.addEventListener("mousedown", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-index]");
      if (!row) return;
      this.highlighted = Number(row.dataset.index);
      this.acceptSuggestion();
    });

    this.grid.addEventListener("mousedown", (e) => {
      const cell = (e.target as HTMLElement).closest<HTMLElement>("[data-date]");
      if (!cell) return;
      this.cursor = cell.dataset.date!;
      this.acceptDay();
    });
  }

  /** The field the date is for, as the caller named it. */
  private get fieldId(): string | null {
    return this.fields[this.field]?.id ?? null;
  }

  /** Whether the calendar, rather than the suggestion list, is what is shown. */
  private get onCalendar(): boolean {
    return this.input.value.trim() === "";
  }

  private handleKey(e: KeyboardEvent): void {
    // Alt is the system's, as it is everywhere else in the app.
    if (e.altKey) return;

    const command = this.keys[eventKey(e)];
    const outside = e.target !== this.input;

    if (!command || command === "noop") {
      // Unbound: typing. If the focus has wandered — a click on the backdrop
      // that missed, say — the key would otherwise reach the app underneath,
      // which is not what a modal means.
      if (outside) {
        e.stopPropagation();
        this.input.focus();
      }
      return;
    }

    if (this.run(command)) {
      e.preventDefault();
      e.stopPropagation();
    } else if (outside) {
      e.stopPropagation();
      this.input.focus();
    }
  }

  /**
   * Run a bound command. Returns whether it did anything.
   *
   * A command that means nothing where the cursor is returns false and the key
   * goes to the field instead — which is the whole reason `move-left` is
   * usable: on the calendar it is yesterday, and in the middle of typing
   * "next tuesda|y" it has to be the caret moving one character left.
   */
  private run(command: string): boolean {
    switch (command) {
      case "accept":
        this.accept();
        return true;
      case "cancel":
        this.finish(null);
        return true;
      case "clear-date":
        // Only from an empty input, so that `<c-d>` keeps its native meaning
        // (forward delete) while there is text to delete — and so that clearing
        // a date is never something that happens on the way to typing one.
        if (!this.onCalendar) return false;
        this.finish({ kind: "clear", field: this.fieldId });
        return true;
      case "move-down":
        return this.move(1);
      case "move-up":
        return this.move(-1);
      case "move-left":
        return this.moveDay(-1);
      case "move-right":
        return this.moveDay(1);
      case "next-month":
        return this.shiftMonth(1);
      case "prev-month":
        return this.shiftMonth(-1);
      case "next-year":
        return this.shiftMonth(12);
      case "prev-year":
        return this.shiftMonth(-12);
      case "switch-field": {
        if (this.fields.length < 2) return false;
        this.field = (this.field + 1) % this.fields.length;
        this.title.textContent = this.fields[this.field]!.title;
        this.renderFooter();
        return true;
      }
      case "today":
        if (!this.onCalendar) return false;
        this.cursor = this.today;
        this.renderCalendar();
        return true;
      default:
        // An unknown command name is a typo in the config, and swallowing the
        // key would make it look like a dead key rather than a bad binding.
        this.onProblem(`unknown date_picker command: ${command}`);
        return false;
    }
  }

  /** Down and up: through the suggestions, or a week at a time on the grid. */
  private move(step: number): boolean {
    if (this.onCalendar) {
      this.cursor = addDays(this.cursor, step * 7);
      this.renderCalendar();
      return true;
    }
    if (!this.suggestions.length) return false;
    const count = this.suggestions.length;
    this.highlighted = (this.highlighted + step + count) % count;
    this.renderList();
    return true;
  }

  /** Left and right, which only mean a day while the calendar is showing. */
  private moveDay(step: number): boolean {
    if (!this.onCalendar) return false;
    this.cursor = addDays(this.cursor, step);
    this.renderCalendar();
    return true;
  }

  private shiftMonth(months: number): boolean {
    if (!this.onCalendar) return false;
    this.cursor = addMonths(this.cursor, months);
    this.renderCalendar();
    return true;
  }

  // -------------------------------------------------------------------------
  // Suggestions
  // -------------------------------------------------------------------------

  private onTyped(): void {
    if (this.timer !== null) window.clearTimeout(this.timer);
    const query = this.input.value.trim();

    if (!query) {
      // Back to nothing: the calendar returns, and any answer still in flight
      // is no longer an answer to anything.
      this.outstanding = "";
      this.acceptWhenReady = false;
      this.suggestions = [];
      this.failed = false;
      this.list.hidden = true;
      this.list.replaceChildren();
      this.calendar.hidden = false;
      this.renderFooter();
      return;
    }

    this.calendar.hidden = true;
    this.list.hidden = false;
    this.timer = window.setTimeout(() => void this.lookup(query), SUGGEST_DEBOUNCE);
    this.renderFooter();
  }

  private async lookup(query: string): Promise<void> {
    this.timer = null;
    this.outstanding = query;
    this.inFlight = true;
    let found: DateSuggestion[] = [];
    try {
      found = await this.fetch(query, this.mode);
      this.failed = false;
      this.inFlight = false;
    } catch (e) {
      this.inFlight = false;
      // A lookup that failed is not a date that does not exist, and saying "no
      // matching date" for a broken IPC would send the user off inventing
      // spellings for a phrase that parses perfectly well.
      this.failed = true;
      this.suggestions = [];
      this.acceptWhenReady = false;
      this.onProblem(typeof e === "string" ? e : ((e as Error)?.message ?? String(e)));
      this.renderList();
      return;
    }

    // Two keystrokes in flight at once: the older answer is not an answer to
    // what is on screen now.
    if (this.outstanding !== query || this.input.value.trim() !== query) return;

    this.suggestions = found;
    this.highlighted = 0;
    this.renderList();

    // `<enter>` pressed inside the debounce window. Someone who types
    // "tomorrow" and hits return in one motion means the first suggestion for
    // "tomorrow", and dropping it because the answer had not arrived yet is the
    // picker losing a keystroke it was given.
    if (this.acceptWhenReady) {
      this.acceptWhenReady = false;
      this.acceptSuggestion();
    }
  }

  private accept(): void {
    if (this.onCalendar) {
      this.acceptDay();
      return;
    }
    // Still waiting: remember the intent rather than answering with nothing.
    // The debounce is cut short, because the wait exists to spare the core six
    // lookups per word and not to make return feel slow.
    const query = this.input.value.trim();
    if (this.timer !== null || this.inFlight || this.outstanding !== query) {
      if (this.timer !== null) {
        window.clearTimeout(this.timer);
        void this.lookup(query);
      }
      this.acceptWhenReady = true;
      return;
    }
    this.acceptSuggestion();
  }

  private acceptSuggestion(): void {
    const chosen = this.suggestions[this.highlighted];
    // Nothing parsed. The list already says so; `<enter>` on "no matching date"
    // must not close the picker with a guess, per §12.1.
    if (!chosen) return;
    this.resolve(chosen.date, shortTime(chosen.time));
  }

  private acceptDay(): void {
    this.resolve(this.cursor, this.mode === "reminder" ? DEFAULT_REMINDER_TIME : null);
  }

  private resolve(date: string, time: string | null): void {
    if (this.mode !== "reminder") {
      this.finish({ kind: "date", field: this.fieldId, date, time: null, at: null });
      return;
    }
    const clock = time ?? DEFAULT_REMINDER_TIME;
    this.finish({ kind: "date", field: this.fieldId, date, time: clock, at: localRfc3339(date, `${clock}:00`) });
  }

  // -------------------------------------------------------------------------
  // Rendering
  // -------------------------------------------------------------------------

  private renderList(): void {
    this.renderFooter();

    if (!this.suggestions.length) {
      const empty = document.createElement("li");
      empty.className = "dp-empty";
      empty.textContent = this.failed ? "could not read the date" : "no matching date";
      this.list.replaceChildren(empty);
      return;
    }

    this.list.replaceChildren(
      ...this.suggestions.map((s, i) => {
        const row = document.createElement("li");
        row.dataset.index = String(i);
        if (i === this.highlighted) row.className = "highlighted";

        // Two labels, one row: what the date is called on the left, and the
        // date itself — or how far away it is — right-aligned and quieter, so
        // the column of dates can be scanned without reading the phrases.
        const primary = document.createElement("span");
        primary.className = "dp-primary";
        primary.textContent = s.primary;

        const secondary = document.createElement("span");
        secondary.className = "dp-secondary";
        secondary.textContent = s.secondary;

        row.append(primary, secondary);
        return row;
      }),
    );

    this.list.querySelector(".highlighted")?.scrollIntoView({ block: "nearest" });
  }

  private renderCalendar(): void {
    this.monthLabel.textContent = monthLabel(this.cursor, this.locale);

    // The grid starts on the week containing the first of the month, so the
    // month is shown in the shape a calendar has and not as a ragged list.
    const { year, month } = parts(this.cursor);
    const first = civil(year, month, 1);
    const lead = (weekday(first) - this.weekStart + 7) % 7;
    const start = addDays(first, -lead);

    const cells: HTMLElement[] = [];
    for (let i = 0; i < GRID_ROWS * 7; i += 1) {
      const date = addDays(start, i);
      const cell = document.createElement("button");
      cell.type = "button";
      // Clickable but never focusable: the input keeps the keyboard for as long
      // as the picker is open, and 42 tab stops in a grid is not navigation.
      cell.tabIndex = -1;
      cell.dataset.date = date;
      cell.className = "dp-day";
      if (parts(date).month !== month) cell.classList.add("other");
      if (date === this.today) cell.classList.add("today");
      if (date === this.initial) cell.classList.add("current");
      if (date === this.cursor) cell.classList.add("cursor");
      cell.textContent = String(parts(date).day);
      cells.push(cell);
    }
    this.grid.replaceChildren(...cells);
    this.renderFooter();
  }

  /**
   * The hint line.
   *
   * It says what the keys do, because the picker is modal and there is nowhere
   * else to find that out mid-keystroke, and it names the keys the *config*
   * bound rather than the ones this file would have chosen.
   */
  private renderFooter(): void {
    const hints: string[] = [`${this.keyFor("accept")} select`, `${this.keyFor("cancel")} cancel`];
    if (this.onCalendar) hints.push(`${this.keyFor("clear-date")} no date`);
    const next = this.fields[(this.field + 1) % this.fields.length];
    if (this.fields.length > 1 && next) hints.push(`${this.keyFor("switch-field")} ${next.title.toLowerCase()}`);
    if (this.mode === "reminder" && this.onCalendar) {
      hints.push(`at ${DEFAULT_REMINDER_TIME} unless a time is typed`);
    }
    this.footer.textContent = hints.join("  ·  ");
  }

  /** How the config spells a command, for the hint line. */
  private keyFor(command: string): string {
    const key = Object.keys(this.keys).find((k) => this.keys[k] === command);
    return key ? spellKey(key) : "—";
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** The default lookup: straight at the Rust parser, through Tauri. */
function defaultSuggest(input: string, mode: DatePickerMode): Promise<DateSuggestion[]> {
  return invoke<DateSuggestion[]>(COMMANDS[mode], { input });
}

function navButton(glyph: string, label: string): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.tabIndex = -1;
  button.className = "dp-nav-button";
  button.textContent = glyph;
  button.title = label;
  return button;
}

/**
 * Weekday initials for the header row.
 *
 * Taken from `Intl` rather than a table in this file: the names are the one
 * part of the calendar that is genuinely locale data, and a hand-written list
 * is a list that is wrong for the third language somebody adds. 2024-01-07 is a
 * Sunday, which makes the offset arithmetic below trivially checkable.
 */
function weekdayLabels(locale: string, weekStart: 0 | 1): string[] {
  const format = new Intl.DateTimeFormat(locale, { weekday: "short", timeZone: "UTC" });
  return Array.from({ length: 7 }, (_, i) =>
    format.format(new Date(Date.UTC(2024, 0, 7 + ((i + weekStart) % 7)))),
  );
}

/** "September 2026", as the locale writes it. */
function monthLabel(date: string, locale: string): string {
  const { year, month } = parts(date);
  return new Intl.DateTimeFormat(locale, {
    month: "long",
    year: "numeric",
    timeZone: "UTC",
  }).format(new Date(Date.UTC(year, month - 1, 1)));
}
