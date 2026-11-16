import { currentFolder, scrollCursorIntoView } from "./actions";
import { setStatus } from "./bridge";
import { renderList } from "./list";
import { Column, ViewMode } from "./model";
import { inTaskView } from "./sidebar";
import { readSetting, writeSetting } from "./splitters";
import { el, state } from "./state";

export const VIEW_LABELS: Record<ViewMode, string> = {
  html: "HTML",
  text: "Plain",
  raw: "Source",
};
export const COLUMNS: Column[] = [
  { key: null, label: "", cls: "meta" },
  // Open tasks linked to the message (§6.3). Deliberately narrow and deliberately
  // blank when there are none: a column of zeroes would be a column of noise,
  // and the number only means anything where it is not one.
  { key: null, label: "", cls: "tasks" },
  { key: "from", label: "From", cls: "from" },
  // The recipients, off by default and on for a Sent folder.
  { key: null, label: "To", cls: "to" },
  { key: "subject", label: "Subject", cls: "subject" },
  { key: "date", label: "Date", cls: "date" },
];
/** The columns the open list shows: the folder's own, else the config's. */
export function visibleColumns(): Column[] {
  const folder = inTaskView() || state.search ? undefined : currentFolder();
  const names = folder?.columns ?? state.settings.columns;
  return COLUMNS.filter((c) => names.includes(c.cls));
}
/**
 * Say on the list pane which columns are hidden, and lay the rest out.
 *
 * The rows are one grid whose order is the design's — status, From, To,
 * subject, badges, date — whatever order `list.columns` names them in; the
 * columns only say which are shown. `meta` is the status column and the
 * badges, `tasks` the badge of the open tasks. `--cols-date-first` is the
 * same columns with the date second, for a look that leads with it, and the
 * `-compact` pair the same again with a narrower From and no To, for Today in
 * columns.
 */
export function applyColumnClasses(): void {
  const shown = new Set(visibleColumns().map((c) => c.cls));
  for (const c of COLUMNS) el.list.classList.toggle(`hide-${c.cls}`, !shown.has(c.cls));
  const layout = (compact: boolean, kind = false) => {
    const tracks = [
      shown.has("meta") ? "var(--status-w)" : "",
      shown.has("from") ? (compact ? "var(--col-from-compact)" : "var(--col-from)") : "",
      shown.has("to") && !compact ? "var(--col-to)" : "",
      "minmax(0, 1fr)",
      "auto",
      shown.has("date") ? "var(--col-date)" : "",
    ].filter(Boolean);
    const dateFirst = [...tracks];
    const date = shown.has("date") ? dateFirst.pop()! : null;
    if (date) dateFirst.splice(shown.has("meta") ? 1 : 0, 0, date);
    // Today's timeline names each row's kind after its marks, or after the
    // date where the date comes first.
    if (kind) {
      tracks.splice(shown.has("meta") ? 1 : 0, 0, "var(--col-kind)");
      dateFirst.splice((shown.has("meta") ? 1 : 0) + (date ? 1 : 0), 0, "var(--col-kind)");
    }
    return [tracks.join(" "), dateFirst.join(" ")];
  };
  const [cols, colsDateFirst] = layout(false);
  const [compact, compactDateFirst] = layout(true);
  el.list.style.setProperty("--cols", cols);
  el.list.style.setProperty("--cols-date-first", colsDateFirst);
  el.list.style.setProperty("--cols-compact", compact);
  el.list.style.setProperty("--cols-date-first-compact", compactDateFirst);
  const [mixed, mixedDateFirst] = layout(true, true);
  el.list.style.setProperty("--cols-mixed", mixed);
  el.list.style.setProperty("--cols-date-first-mixed", mixedDateFirst);
}
/** Steps of ⌘+ / ⌘-, in pixels, within what still reads. */
const FONT_SIZE_MIN = 9;
const FONT_SIZE_MAX = 24;
/** The text size in force: the machine's choice if it made one, else the config's. */
function fontSize(): number {
  const saved = Number(readSetting(".fontSize"));
  return saved >= FONT_SIZE_MIN && saved <= FONT_SIZE_MAX ? saved : state.settings.fontSize;
}
export type Look = "hybrid" | "terminal";
/** The look in force: the machine's choice if it made one, else the config's. */
export function currentLook(): Look {
  const saved = readSetting(".look");
  return saved === "hybrid" || saved === "terminal" ? saved : state.settings.look;
}
/** Forget the machine's choice of look, so the config's is in force. */
export function forgetLook(): void {
  try {
    localStorage.removeItem("orb.look");
  } catch {
    // Nothing to forget.
  }
}
/**
 * Draw every window in `look` on this machine. Choosing the config's look
 * forgets the choice rather than storing a copy of the config.
 */
export function chooseLook(look: Look): void {
  if (look === state.settings.look) forgetLook();
  else writeSetting(".look", look);
  applyAppearance();
  // The rows are as tall as the look has them, so the list is laid out anew.
  renderList();
  scrollCursorIntoView();
  setStatus(`${look === "terminal" ? "Terminal" : "Hybrid"} look`);
}
/**
 * Give the window the appearance, the look and the text size the settings
 * ask for. The look is the root's `data-look`, and only the CSS reads it.
 */
export function applyAppearance(): void {
  const root = document.documentElement;
  root.dataset.look = currentLook();
  if (state.settings.theme === "dark" || state.settings.theme === "light") {
    root.dataset.theme = state.settings.theme;
  } else {
    delete root.dataset.theme;
  }
  root.style.setProperty("--font-size", `${fontSize()}px`);
}
/** Whether the window is dark just now, by the setting or the system. */
export function windowIsDark(): boolean {
  if (state.settings.theme === "dark") return true;
  if (state.settings.theme === "light") return false;
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}
/** Change the text size by `delta` pixels, or back to the config's with 0. */
export function zoom(delta: number): void {
  const next = delta === 0
    ? state.settings.fontSize
    : Math.min(FONT_SIZE_MAX, Math.max(FONT_SIZE_MIN, fontSize() + delta));
  if (delta === 0) {
    try {
      localStorage.removeItem("orb.fontSize");
    } catch {
      // Nothing to forget.
    }
  } else {
    writeSetting(".fontSize", String(next));
  }
  applyAppearance();
  // The rows are as tall as the text needs, so the list is laid out anew.
  renderList();
  scrollCursorIntoView();
  setStatus(`text size ${next}px`);
}
/**
 * The row height each look gives each density at the default text size of
 * 13 px; the CSS follows the JS. A larger text size makes every row taller in
 * step.
 */
const ROW_HEIGHTS: Record<string, Record<string, number>> = {
  hybrid: { compact: 20, comfortable: 24, spacious: 30 },
  terminal: { compact: 20, comfortable: 24, spacious: 28 },
};
/** Give the list the density the config asks for, at the text size in force. */
export function applyDensity(): void {
  const heights = ROW_HEIGHTS[currentLook()] ?? ROW_HEIGHTS.hybrid!;
  const base = heights[state.settings.density] ?? heights.comfortable!;
  ROW_HEIGHT = Math.round((base * fontSize()) / 13);
  el.list.style.setProperty("--row-height", `${ROW_HEIGHT}px`);
}
/**
 * The fixed columns' widths, in pixels; the subject takes the rest. From and
 * Date follow `theme.column_widths`, set when the settings arrive.
 */
export type SizedColumn = "from" | "to" | "date";
export const widths: Record<SizedColumn, number> = { from: 260, to: 160, date: 112 };
export function applyConfiguredWidths(from: number | null, date: number | null): void {
  if (from) widths.from = Math.max(90, from);
  if (date) widths.date = Math.max(40, date);
  applyWidths();
}
export function applyWidths(): void {
  for (const [col, px] of Object.entries(widths)) {
    el.list.style.setProperty(`--col-${col}`, `calc(${px} * var(--u))`);
  }
}
/**
 * Virtual list geometry.
 *
 * Row height must be fixed: it is what lets a row's position be computed from
 * its index without measuring the DOM, which is the whole basis of scrolling
 * 200k rows smoothly.
 */
export let ROW_HEIGHT = 30;
