/**
 * Today's third layout, Mixed: the tasks in their column on the right, and on
 * the left one timeline of the mail, the chat and the reading, each item at
 * its moment.
 *
 * The timeline is not a list of its own. The mail stays the paged list it is
 * in the other layouts, addressed by its own index; the threads that wait and
 * the reading items, a few dozen at most, are read into it where their time
 * falls, by asking the core how many rows of the mail come before each. So a
 * position on the timeline is a mail index plus the items placed above it,
 * and the cursor on the timeline is the cursor of the section whose item it
 * stands on — which keeps every key, the reader and undo working as they do
 * in the sections.
 */

import { goToPosition } from "./actions";
import { call } from "./bridge";
import { SearchHit } from "./model";
import { todayMixed } from "./layout";
import { rowAt } from "./list";
import { state } from "./state";
import { loadedIndexOf } from "./targets";
import { shows, TodaySection } from "./today";

export type TimelineKind = "mail" | "chat" | "reading";

/** Whether the timeline reads newest first; a threaded list knows no other way. */
export function newestFirst(): boolean {
  return state.threads || state.sort.dir === -1;
}

/** A thread or a reading item, and the place it takes on the timeline. */
interface Placed {
  kind: "chat" | "reading";
  /** Its index in `state.chat.rows` or `state.reading.rows`. */
  index: number;
  /** Unix seconds. */
  time: number;
  /** How many rows of the mail come before it. */
  rank: number;
}

/** The items read into the mail, in timeline order. */
let placed: Placed[] = [];
let generation = 0;

/** Where the timeline's cursor stands while the tasks have the keyboard. */
let resting: TimelineKind = "mail";

export function isTimeline(section: TodaySection): section is TimelineKind {
  return section !== "tasks";
}

/** The kind of the item the timeline's cursor is on, whether or not it has the keyboard. */
export function timelineKind(): TimelineKind {
  const section = state.tasks.section;
  if (isTimeline(section)) resting = section;
  return resting;
}

function mailShown(): number {
  return shows("mail") ? state.total : 0;
}

const seconds = (date: string | null | undefined): number => {
  const ms = date ? Date.parse(date) : NaN;
  return Number.isNaN(ms) ? 0 : Math.floor(ms / 1000);
};

/** When a mail row stands on the timeline: a collapsed thread at its newest message. */
function mailTime(hit: SearchHit): number {
  const m = hit.message;
  const collapsed = (m.thread_count ?? 1) > 1 && !m.thread_expanded;
  return seconds((collapsed ? m.thread_latest : null) ?? m.date);
}

/**
 * Place the threads and the reading items among the mail again. Called
 * whenever any of the three changes; the rows on screen are drawn from what
 * the last call left.
 */
export async function placeTimeline(): Promise<void> {
  const mine = ++generation;
  if (!todayMixed()) {
    placed = [];
    return;
  }
  const items: Omit<Placed, "rank">[] = [
    ...(shows("chat") ? state.chat.rows.map((r, index) => ({ kind: "chat" as const, index, time: Math.floor(r.newest_at / 1000) })) : []),
    ...(shows("reading")
      ? state.reading.rows.map((hit, index) => ({ kind: "reading" as const, index, time: seconds(hit.message.date) }))
      : []),
  ];
  const newest = newestFirst();
  let ranks: number[];
  // The filter's hits are in memory, so they are counted here.
  const hits = state.filter?.hits ?? null;
  if (mailShown() === 0 || items.length === 0) {
    ranks = items.map(() => 0);
  } else if (hits) {
    const times = hits.map(mailTime);
    ranks = items.map((item) => times.filter((t) => (newest ? t > item.time : t > 0 && t < item.time)).length);
  } else {
    const fetched = await call<number[]>("inbox_ranks", {
      folders: (state.inboxFolders ?? []).map((f) => [f.account_name, f.folder_name]),
      times: items.map((item) => item.time),
      newestFirst: newest,
      threads: state.threads,
      expanded: [...state.expandedThreads],
    }).catch(() => null);
    if (mine !== generation) return;
    ranks = fetched ?? items.map(() => 0);
  }
  const total = mailShown();
  const order = newest ? -1 : 1;
  placed = items
    .map((item, i) => ({ ...item, rank: Math.min(ranks[i] ?? 0, total) }))
    .sort((a, b) => a.rank - b.rank || order * (a.time - b.time));
  settleCursor();
}

const cursorOf = (kind: TimelineKind): number =>
  ({ mail: state.messageCursor, chat: state.chat.cursor, reading: state.reading.cursor })[kind];

/**
 * Keep the timeline's cursor on an item it holds: one whose thread got its
 * answer, or whose kind was switched off, leaves it on the first row instead.
 */
function settleCursor(): void {
  const kind = timelineKind();
  if (positionOf(kind, cursorOf(kind)) >= 0) return;
  const first = timelineAt(0);
  if (!first) return;
  resting = first.kind;
  if (first.kind === "mail") state.messageCursor = first.index;
  else if (first.kind === "chat") state.chat.cursor = first.index;
  else state.reading.cursor = first.index;
  if (isTimeline(state.tasks.section)) state.tasks.section = first.kind;
}

/** How many rows the timeline has. */
export function timelineTotal(): number {
  return mailShown() + placed.length;
}

/** What stands at a position on the timeline. */
export function timelineAt(position: number): { kind: TimelineKind; index: number } | undefined {
  if (position < 0 || position >= timelineTotal()) return undefined;
  let before = 0;
  for (const item of placed) {
    const at = item.rank + before;
    if (position === at) return { kind: item.kind, index: item.index };
    if (position < at) break;
    before += 1;
  }
  return { kind: "mail", index: position - before };
}

/** The position of the mail row at `index`: the items placed above it come first. */
export function mailPosition(index: number): number {
  let before = 0;
  for (const item of placed) {
    if (item.rank > index) break;
    before += 1;
  }
  return index + before;
}

/** The position of an item of `kind`, or -1 when it is not on the timeline. */
export function positionOf(kind: TimelineKind, index: number): number {
  if (kind === "mail") return index < mailShown() ? mailPosition(index) : -1;
  const at = placed.findIndex((item) => item.kind === kind && item.index === index);
  return at < 0 ? -1 : placed[at]!.rank + at;
}

/** The position of the timeline's cursor. */
export function cursorPosition(): number {
  const kind = timelineKind();
  const at = positionOf(kind, cursorOf(kind));
  return at >= 0 ? at : 0;
}

/**
 * The mail indices drawn between two positions, and the items placed among
 * them, for the virtual list to draw.
 */
export function placedBetween(first: number, last: number): { position: number; kind: "chat" | "reading"; index: number }[] {
  return placed
    .map((item, i) => ({ position: item.rank + i, kind: item.kind, index: item.index }))
    .filter((item) => item.position >= first && item.position <= last);
}

/** A row of the timeline by what it shows, which outlasts its position. */
type Identity = { kind: TimelineKind; key: number | string };

function identityAt(position: number): Identity | null {
  const item = timelineAt(position);
  if (!item) return null;
  if (item.kind === "mail") {
    const id = rowAt(item.index)?.id;
    return id === undefined ? null : { kind: "mail", key: id };
  }
  if (item.kind === "reading") {
    const id = state.reading.rows[item.index]?.message.id;
    return id === undefined ? null : { kind: "reading", key: id };
  }
  const row = state.chat.rows[item.index];
  return row ? { kind: "chat", key: `${row.rid}\u0000${row.thread}` } : null;
}

function positionOfIdentity(identity: Identity): number {
  if (identity.kind === "mail") {
    const index = loadedIndexOf(identity.key as number);
    return index === null ? -1 : positionOf("mail", index);
  }
  if (identity.kind === "reading") {
    return positionOf("reading", state.reading.rows.findIndex((hit) => hit.message.id === identity.key));
  }
  return positionOf("chat", state.chat.rows.findIndex((row) => `${row.rid}\u0000${row.thread}` === identity.key));
}

/** How far around the cursor a removal is looked past: a selection taken away at once. */
const AROUND = 50;

/**
 * Remember the rows around the timeline's cursor before a command, and
 * return what puts the cursor back after it: on the row that took the place
 * of one the command took away, whatever its kind — or, at the end, on the one
 * above. Each kind left to itself would land on the next row of its own kind,
 * past the rows of the others between.
 */
export function holdTimelineCursor(): (() => Promise<void>) | null {
  if (!todayMixed() || state.focus !== "list" || !isTimeline(state.tasks.section)) return null;
  const at = cursorPosition();
  const current = identityAt(at);
  if (!current) return null;
  const after: Identity[] = [];
  const before: Identity[] = [];
  for (let i = 1; i <= AROUND; i++) {
    const next = identityAt(at + i);
    const previous = identityAt(at - i);
    if (next) after.push(next);
    if (previous) before.push(previous);
  }
  return async () => {
    if (!todayMixed() || state.focus !== "list" || !isTimeline(state.tasks.section)) return;
    if (positionOfIdentity(current) >= 0) return;
    await placeTimeline();
    if (positionOfIdentity(current) >= 0) return;
    for (const identity of [...after, ...before]) {
      const position = positionOfIdentity(identity);
      if (position < 0) continue;
      if (position !== cursorPosition() || timelineKind() !== identity.kind) goToPosition(position);
      return;
    }
  };
}
