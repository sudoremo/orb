/**
 * Today's layout: stacked, in columns, or mixed — the mail, the chat and the
 * reading as one timeline beside the tasks (`mixed.ts`). On its own, with
 * nothing but the settings and the state under it, so that the chat can ask
 * which is in force without loading the views.
 */

import { readSetting, removeSetting, writeSetting } from "./splitters";
import { state, todayHasMail } from "./state";

export type TodayLayout = "stacked" | "columns" | "mixed";

const TODAY_LAYOUT = ".todayLayout";
/** The layout Mixed was entered from, which leaving it returns to. */
const TODAY_LAYOUT_BEFORE = ".todayLayoutBefore";

/** Today's layout, remembered on this machine. */
export function todayLayout(): TodayLayout {
  const saved = readSetting(TODAY_LAYOUT);
  return saved === "columns" || saved === "mixed" ? saved : "stacked";
}

export function storeTodayLayout(layout: TodayLayout): void {
  const was = todayLayout();
  if (layout === "mixed" && was !== "mixed") writeSetting(TODAY_LAYOUT_BEFORE, was);
  if (layout === "stacked") removeSetting(TODAY_LAYOUT);
  else writeSetting(TODAY_LAYOUT, layout);
}

/** The layout leaving Mixed returns to: the one it was entered from. */
export function layoutBeforeMixed(): TodayLayout {
  return readSetting(TODAY_LAYOUT_BEFORE) === "columns" ? "columns" : "stacked";
}

/** Whether Today is on screen as one timeline beside the tasks. */
export function todayMixed(): boolean {
  return state.tasks.view === "today" && todayHasMail() && todayLayout() === "mixed" && !state.search;
}
