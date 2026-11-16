/**
 * Selecting on Today's timeline (`mixed.ts`), across the kinds on it.
 *
 * Each kind keeps the selection it has elsewhere — the mail its row indices,
 * the threads their keys, the reading items their indices — so that every
 * command acts on its own kind's selection as it always has. What is added
 * here is the timeline's view of them: a range runs over positions, whatever
 * kind each row is, and an action with rows of several kinds selected runs
 * each kind's own command on that kind's rows.
 */

import { goToPosition, move } from "./actions";
import { setStatus } from "./bridge";
import { todayMixed } from "./layout";
import { renderRows } from "./list";
import { TimelineKind, cursorPosition, timelineAt } from "./mixed";
import { clearSelection, selectionSize } from "./selection";
import { state } from "./state";

/** Where a range selection on the timeline started, as a position. */
let anchor: number | null = null;

const threadKey = (index: number): string | null => {
  const row = state.chat.rows[index];
  return row ? `${row.rid}\u0000${row.thread}` : null;
};

function isSelected(kind: TimelineKind, index: number): boolean {
  if (kind === "mail") return state.selected.has(index);
  if (kind === "reading") return state.reading.selected.has(index);
  const key = threadKey(index);
  return key !== null && state.chat.selected.has(key);
}

function select(kind: TimelineKind, index: number, on: boolean): void {
  const set: Set<number | string> =
    kind === "mail" ? state.selected : kind === "reading" ? state.reading.selected : state.chat.selected;
  const key = kind === "chat" ? threadKey(index) : index;
  if (key === null) return;
  if (on) set.add(key);
  else set.delete(key);
}

/** How many rows of each kind are selected. */
function counts(): { messages: number; threads: number; reading: number } {
  return { messages: selectionSize(), threads: state.chat.selected.size, reading: state.reading.selected.size };
}

/** Whether anything on the timeline is selected. */
export function anySelected(): boolean {
  const n = counts();
  return todayMixed() && n.messages + n.threads + n.reading > 0;
}

/** Forget the selection of every kind. */
export function clearTimelineSelection(): void {
  clearSelection();
  state.chat.selected.clear();
  state.chat.anchor = null;
  state.reading.selected.clear();
  anchor = null;
}

/** Say what is selected, kind by kind when there are several. */
export function reportTimelineSelection(): void {
  const { messages, threads, reading } = counts();
  const parts = [
    messages && `${messages} message${messages === 1 ? "" : "s"}`,
    threads && `${threads} thread${threads === 1 ? "" : "s"}`,
    reading && `${reading} reading item${reading === 1 ? "" : "s"}`,
  ].filter(Boolean);
  const total = messages + threads + reading;
  setStatus(total === 0 ? "selection cleared" : parts.length > 1 ? `${total} selected · ${parts.join(", ")}` : `${parts[0]} selected`);
}

/** Select every row between two positions, and nothing else. */
function selectBetween(from: number, to: number): void {
  clearSelection();
  state.chat.selected.clear();
  state.reading.selected.clear();
  for (let at = Math.min(from, to); at <= Math.max(from, to); at++) {
    const item = timelineAt(at);
    if (item) select(item.kind, item.index, true);
  }
}

/** `Space`: select the row under the cursor, or let it go, and step on. */
export function toggleOnTimeline(count: number): void {
  const at = cursorPosition();
  const item = timelineAt(at);
  if (!item) return;
  select(item.kind, item.index, !isSelected(item.kind, item.index));
  anchor = at;
  move(count);
  renderRows();
  reportTimelineSelection();
}

/** `J`/`K`: move the cursor and drag the selection's far end with it, over every kind. */
export function extendOnTimeline(delta: number): void {
  if (anchor === null) anchor = cursorPosition();
  move(delta);
  selectBetween(anchor, cursorPosition());
  renderRows();
  reportTimelineSelection();
}

/** A click with Shift extends to the row, with ⌘ picks it out; plain clicks are the kinds' own. */
export function clickOnTimeline(position: number, extend: boolean): void {
  const item = timelineAt(position);
  if (!item) return;
  state.focus = "list";
  if (extend) {
    if (anchor === null) anchor = cursorPosition();
    goToPosition(position);
    selectBetween(anchor, position);
  } else {
    select(item.kind, item.index, !isSelected(item.kind, item.index));
    anchor = position;
    goToPosition(position);
  }
  renderRows();
  reportTimelineSelection();
}

/** Set while an action runs across kinds, so its kind's commands run plain. */
let spreading = false;

/**
 * Run an action on everything selected on the timeline, kind by kind: the
 * messages and the reading items — which are messages too — with `mail`, the
 * threads with `chat`. A selection wins over the cursor, whichever kind the
 * cursor is on, and one undo takes it all back. Returns whether it ran, so
 * that the command it was asked from does not run again on the cursor.
 */
export async function actAcrossKinds(mail: string, chat: string, consumes: boolean): Promise<boolean> {
  if (spreading || !anySelected()) return false;
  const { messages, threads, reading } = counts();
  const { runCommand } = await import("./commands");
  const { groupUndo } = await import("./moving");
  const was = state.tasks.section;
  spreading = true;
  // One action, so one ⌘Z takes back every kind's part of it.
  const endGroup = await groupUndo();
  try {
    if (messages + reading > 0) {
      // On a thread the mail commands are refused, as acting on the mail behind it.
      state.tasks.section = reading > 0 && messages === 0 ? "reading" : "mail";
      await runCommand(mail, 1);
      if (consumes) state.reading.selected.clear();
    }
    if (threads > 0) {
      state.tasks.section = "chat";
      await runCommand(chat, 1);
    }
  } finally {
    await endGroup();
    spreading = false;
    if (state.tasks.section !== "tasks") state.tasks.section = was;
    anchor = null;
  }
  renderRows();
  return true;
}
