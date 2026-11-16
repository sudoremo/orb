/**
 * Which of Today's sections are on screen, in the order they stand there.
 *
 * Mail and tasks are shown unless put away, chat and reading once switched on.
 * A section put away keeps its rows and its cursor; it is only neither drawn
 * nor walked. Today always shows one at least.
 */

import { todayChat } from "./chat";
import { todayMixed } from "./layout";
import { timelineKind, timelineTotal } from "./mixed";
import { todayReading } from "./reading";
import { hasMailHalf } from "./sidebar";
import { readSetting, removeSetting, writeSetting } from "./splitters";
import { state, todayHasMail } from "./state";

export type TodaySection = "mail" | "tasks" | "chat" | "reading";

/** Top to bottom, and in columns the mail on the left of the rest. */
const ORDER: TodaySection[] = ["mail", "tasks", "chat", "reading"];

const PUT_AWAY = { mail: ".todayHidesMail", tasks: ".todayHidesTasks" } as const;

/** Whether Today keeps its mail or its tasks on screen, remembered on this Mac. */
export function todayKeeps(section: "mail" | "tasks"): boolean {
  return readSetting(PUT_AWAY[section]) !== "on";
}

/** Put Today's mail or tasks away, or bring them back. */
export function keepInToday(section: "mail" | "tasks", on: boolean): void {
  if (on) removeSetting(PUT_AWAY[section]);
  else writeSetting(PUT_AWAY[section], "on");
}

/** Whether Today's switches show a section, whichever view is open. */
function switchedOn(section: TodaySection): boolean {
  if (section === "mail" || section === "tasks") return todayKeeps(section);
  return section === "chat" ? todayChat() : todayReading();
}

/**
 * Whether a section is on screen. A search showing mail and tasks shows
 * both whatever Today's switches say, and a view without a mail half has
 * its tasks alone.
 */
export function shows(section: TodaySection): boolean {
  if (!hasMailHalf()) return section === "tasks";
  if (state.tasks.view !== "today") return section === "mail" || section === "tasks";
  return switchedOn(section);
}

export function shownSections(): TodaySection[] {
  // Mixed has two: the timeline, standing for the kind its cursor is on, and the tasks.
  if (todayMixed()) {
    const timeline = shows("mail") || shows("chat") || shows("reading");
    return [...(timeline ? [timelineKind()] : []), ...(shows("tasks") ? ["tasks" as const] : [])];
  }
  return ORDER.filter(shows);
}

/** Whether putting a section away leaves Today something to show. */
export function canPutAway(section: TodaySection): boolean {
  return !todayHasMail() || ORDER.some((s) => s !== section && switchedOn(s));
}

function hasRows(section: TodaySection): boolean {
  if (todayMixed() && section !== "tasks") return timelineTotal() > 0;
  if (section === "mail") return state.total > 0;
  if (section === "chat") return state.chat.rows.length > 0;
  if (section === "reading") return state.reading.rows.length > 0;
  return true;
}

/** Where the keyboard starts, and goes when its section is put away: the first shown with rows. */
export function firstSection(): TodaySection {
  const shown = shownSections();
  return shown.find(hasRows) ?? shown[0] ?? "tasks";
}

/** The section `Tab` goes to from `from`: the next shown with rows, round to the first. */
export function nextSection(from: TodaySection): TodaySection {
  const walked = shownSections().filter((s) => s === from || hasRows(s));
  return walked[(walked.indexOf(from) + 1) % walked.length] ?? from;
}
