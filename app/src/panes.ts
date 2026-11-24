/**
 * Ctrl+H/J/K/L between the window's panes, by where they sit on screen.
 *
 * The panes are the folders, the list and the reader — and in Today the mail
 * and the tasks, each a pane of its own (§4.2). The geometry picks the next
 * one rather than a fixed order, so the same keys work whether Today is
 * stacked or in columns and whether the reader is shown at all. Past the last
 * pane that way, the key leaves Orb by the escalate command.
 */

import { chatActive, moveChatSectionCursor } from "./chat";
import { cancelMarkRead, cursorMessageId, followCursor, openMessage, previewCursor, scrollCursorIntoView, scrollTaskCursorIntoView } from "./actions";
import { call, setStatus } from "./bridge";
import { Direction } from "./keys";
import { applyListSectionClass, render, renderRows } from "./list";
import { applyMaximize } from "./maximize";
import { hasMailHalf, renderFolders } from "./sidebar";
import { moveReadingCursor } from "./reading";
import { el, openPanel, state } from "./state";
import { canPutAway, firstSection, keepInToday, shownSections, todayKeeps, TodaySection } from "./today";
import { renderToolbar } from "./toolbar";
import { loadTaskDetail, previewTaskCursor, renderTaskList, todayColumns } from "./taskview";
import { todayMixed } from "./layout";
import { timelineKind } from "./mixed";

type Region = "folders" | "list" | TodaySection | "reader";
const isTodaySection = (region: Region): region is TodaySection =>
  region === "mail" || region === "tasks" || region === "reading" || region === "chat";

/** The panes on screen now, in the order a tie between them falls back to. */
function regions(): { name: Region; box: DOMRect }[] {
  const today = hasMailHalf();
  const candidates: [Region, Element | null][] = [
    ["folders", el.folders],
    ...(today
      ? ([
          [todayMixed() ? timelineKind() : "mail", el.list.querySelector(".mail-scroll")],
          ["tasks", el.list.querySelector(".tk-section")],
          ["reading", el.list.querySelector(".rd-section")],
          ["chat", el.list.querySelector(".ch-section")],
        ] as [Region, Element | null][])
      : ([["list", el.list]] as [Region, Element | null][])),
    ["reader", el.reader],
  ];
  const list = el.list.getBoundingClientRect();
  return candidates.flatMap(([name, element]) => {
    if (!element) return [];
    const drawn = element.getBoundingClientRect();
    // A section reaching past the list is measured by the part on screen,
    // or the pane below the list would not be below it.
    const box = el.list.contains(element) && element !== el.list ? visiblePart(drawn, list) : drawn;
    return box.width > 0 && box.height > 0 ? [{ name, box }] : [];
  });
}

function visiblePart(box: DOMRect, within: DOMRect): DOMRect {
  const left = Math.max(box.left, within.left);
  const top = Math.max(box.top, within.top);
  const right = Math.min(box.right, within.right);
  const bottom = Math.min(box.bottom, within.bottom);
  return new DOMRect(left, top, Math.max(0, right - left), Math.max(0, bottom - top));
}

function current(): Region {
  if (state.focus !== "list") return state.focus;
  if (hasMailHalf()) return state.tasks.section;
  return "list";
}

/** The panes left most recently, latest first: what a tie is settled by. */
const recent: Region[] = [];

function remember(region: Region): void {
  const at = recent.indexOf(region);
  if (at >= 0) recent.splice(at, 1);
  recent.unshift(region);
}

/**
 * The pane that way: the nearest one that lies wholly past this one's edge
 * and shares some of its span across. One merely diagonal from here is not
 * "that way".
 */
function neighbour(from: DOMRect, direction: Direction, among: { name: Region; box: DOMRect }[]): Region | null {
  // Panes meet at a 1px splitter; a little slack keeps rounding from
  // excluding the one next door.
  const slack = 2;
  const scored = among.flatMap(({ name, box }) => {
    const across =
      direction === "left" || direction === "right"
        ? Math.min(from.bottom, box.bottom) - Math.max(from.top, box.top)
        : Math.min(from.right, box.right) - Math.max(from.left, box.left);
    if (across <= slack) return [];
    const distance = {
      left: from.left - box.right,
      right: box.left - from.right,
      up: from.top - box.bottom,
      down: box.top - from.bottom,
    }[direction];
    return distance >= -slack ? [{ name, distance }] : [];
  });
  if (scored.length === 0) return null;
  const nearest = Math.min(...scored.map((s) => s.distance));
  const tied = scored.filter((s) => s.distance - nearest <= slack).map((s) => s.name);
  return recent.find((name) => tied.includes(name)) ?? tied[0]!;
}

/** Switch Today's keyboard to its mail, its tasks or its reading, the reader following. */
export function showTodaySection(section: TodaySection): void {
  if (section === "chat") {
    void moveChatSectionCursor(state.chat.cursor).then(renderRows);
    return;
  }
  if (section === "reading") {
    moveReadingCursor(state.reading.cursor);
    renderTaskList();
    renderRows();
    return;
  }
  state.tasks.section = section;
  // Maximized, the half coming back was hidden and may have lost its scroll.
  if (section === "mail") {
    cancelMarkRead();
    renderTaskList();
    renderRows();
    scrollCursorIntoView();
    previewCursor();
  } else {
    renderTaskList();
    scrollTaskCursorIntoView();
    previewTaskCursor();
  }
}

/** Show Today's mail or tasks, or put them away; with no argument, the other. */
export function setTodayKept(section: "mail" | "tasks", on = !todayKeeps(section)): void {
  const name = section === "mail" ? "Mail" : "Tasks";
  if (!on && !canPutAway(section)) {
    setStatus(`${name} is all Today shows, so it stays`);
    return;
  }
  const had = state.tasks.section === section;
  keepInToday(section, on);
  render();
  if (!on && had && state.tasks.view === "today") showTodaySection(firstSection());
  renderToolbar();
  setStatus(on ? `${name} shown in Today` : `${name} put away from Today`);
}

async function enter(region: Region): Promise<void> {
  if (region === "reader") {
    state.focus = "reader";
    if (openPanel() || chatActive()) return render();
    if (state.tasks.view && (!hasMailHalf() || state.tasks.section === "tasks")) {
      return loadTaskDetail(true);
    }
    // Maximized, the reader is not kept up with the cursor while it is away.
    if (!state.detail || state.detail.id !== cursorMessageId()) return openMessage(false);
    return render();
  }
  if (region === "folders" || region === "list") {
    const hadPanel = openPanel() !== null;
    state.focus = region;
    render();
    // The rows may have moved under the cursor while the panel had the reader.
    if (hadPanel) followCursor();
    return;
  }
  state.focus = "list";
  render();
  if (state.tasks.section !== region) showTodaySection(region);
}

/**
 * Give the keyboard to the pane that way, or leave Orb past the last one.
 *
 * Measured from what is on screen, whatever the view and its layout; only
 * the drawers of a maximized pane or a hidden sidebar, which lie over the
 * list rather than beside it, are stepped to by where they come out.
 */
export async function focusPane(direction: Direction): Promise<void> {
  const from = current();
  const among = regions();
  let next: Region | null;
  if (state.maximized) next = maximizedStep(from, direction);
  else if (state.sidebarHidden && from === "folders") {
    next = direction === "right" ? lastListRegion() : null;
  } else {
    const here = among.find((r) => r.name === from);
    next = here ? neighbour(here.box, direction, among.filter((r) => r !== here)) : null;
  }
  // The folders put away still lie to the left, as a drawer.
  if (!next && state.sidebarHidden && direction === "left" && from !== "folders") next = "folders";
  if (!next) {
    await call("escalate", { direction }).catch(() => {});
    return;
  }
  remember(from);
  await enter(next);
}

/**
 * The same keys while maximized (`maximize.ts`), where the folders and the
 * reader are drawers over the list rather than panes beside it: to the left
 * of the list are the folders, to its right and below it the reader, and
 * from either the list is back the other way. In Today one section is shown;
 * in columns the mail and the rest stand side by side, so left and right
 * cross between them, and the others are reached by `<tab>` alone.
 */
function maximizedStep(from: Region, direction: Direction): Region | null {
  if (from === "folders") return direction === "right" ? lastListRegion() : null;
  if (from === "reader") return direction === "left" || direction === "up" ? lastListRegion() : null;
  if (isTodaySection(from) && todayColumns()) {
    const shown = shownSections();
    const left = todayMixed() ? timelineKind() : "mail";
    const side: Region[] = shown.filter((s) => s !== left);
    if (from === left && direction === "right" && side.length > 0) {
      return recent.find((r) => side.includes(r)) ?? side[0]!;
    }
    if (from !== left && direction === "left" && shown.includes(left)) return left;
  }
  if (direction === "left") return "folders";
  if (direction === "down" || (direction === "right" && !isTodaySection(from))) return "reader";
  return null;
}

/** The part of the list left most recently: Today's mail or tasks, or the list. */
function lastListRegion(): Region {
  if (!(hasMailHalf())) return "list";
  return recent.find(isTodaySection) ?? state.tasks.section;
}

/** The pane, or the half of Today, an element is in; null between them. */
function regionOf(target: Element): Region | null {
  if (el.folders.contains(target)) return "folders";
  if (el.reader.contains(target)) return "reader";
  if (!el.list.contains(target)) return null;
  if (!(hasMailHalf())) return "list";
  if (target.closest(".mail-scroll")) return todayMixed() ? timelineKind() : "mail";
  if (target.closest(".ch-section")) return "chat";
  if (target.closest(".tk-section")) return "tasks";
  if (target.closest(".rd-section")) return "reading";
  return null;
}

/**
 * Give the keyboard to the pane a click landed in, wherever in it.
 *
 * Only the focus is drawn again: a full render would rebuild the reader, and
 * with it the text a drag across the message just selected. A pane's own
 * handler may have moved the keyboard already — a message row takes it for
 * the list — so the focus is drawn whenever the drawing is behind it, not
 * only when it moves here.
 */
export function focusClicked(target: Element): void {
  const region = regionOf(target);
  const from = current();
  if (region && region !== from) {
    remember(from);
    if (isTodaySection(region)) {
      state.focus = "list";
      if (state.tasks.section !== region) showTodaySection(region);
    } else {
      state.focus = region;
    }
  } else if (focusDrawn()) {
    return;
  }
  renderFolders();
  el.list.classList.toggle("focused", state.focus === "list");
  el.reader.classList.toggle("focused", state.focus === "reader");
  applyListSectionClass();
  applyMaximize();
}

/** True when the pane drawn as having the keyboard is the one that has it. */
function focusDrawn(): boolean {
  return (["folders", "list", "reader"] as const).every(
    (pane) => el[pane].classList.contains("focused") === (state.focus === pane),
  );
}

// After the panes' own handlers, so a row or a folder clicked has had its say
// first and this only catches the rest of the pane.
document.addEventListener("click", (e) => {
  if (e.target instanceof Element) focusClicked(e.target);
});
