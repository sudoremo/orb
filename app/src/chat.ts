/**
 * Chat (PLAN.md, Phase 15): Waiting, the threads that wait for an answer; the
 * conversation in the reader; and the composer under it.
 *
 * A thread is named by its room and the id of its first message, the room's
 * own timeline by its room and an empty thread id. Reading here marks nothing
 * read in Rocket.Chat; answering, done (`a`) and a task do.
 */
import { keepingScroll } from "./scrollkeep";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { askFirst, call, setStatus } from "./bridge";
import { pickFiles } from "./files";
import { escape, formatDate, dayHeading, listDate, readerDate } from "./format";
import { IconName, svgIcon } from "./icons";
import { eventKey } from "./keys";
import { drawEmoji, emojiMatching, withEmoji } from "./emoji";
import { readSetting, removeSetting, writeSetting } from "./splitters";
import { el, state } from "./state";
import { todayMixed } from "./layout";
import { keepsKind, todayNarrowing } from "./kinds";

// Types only: `commands.ts` spreads `CHAT_COMMANDS` as it loads, so a value
// import back from it would close a cycle that runs before this module has.
import type { Command } from "./commands";

export interface ChatRoom {
  rid: string;
  kind: string;
  name: string | null;
  fname: string | null;
  label: string;
  unread: number;
  mentions: number;
  /** The other person's status, for a direct message with one person. */
  presence?: string | null;
  /** Whether the room is muted now, and until when (ms); `null` for a mute the user ends. */
  muted?: boolean;
  muted_until?: number | null;
}
export interface WaitingThread {
  rid: string;
  thread: string;
  room: ChatRoom;
  title: string | null;
  replies: number;
  author: string;
  preview: string;
  newest_at: number;
  waiting_since: number;
  mention: boolean;
  /** Whether the room is muted and nothing since names the user: only the room shows it. */
  muted?: boolean;
  unread: boolean;
  files: boolean;
  task: boolean;
  waits: boolean;
  mine_last: boolean;
}
/** Something the room picker offers. */
interface ChatPick {
  kind: "room" | "join" | "person";
  rid: string | null;
  username: string | null;
  label: string;
  detail: string;
}
export interface ChatFile {
  name: string;
  mime?: string;
  size?: number;
  path?: string;
  /** The server path of an image's preview, and the image's size in pixels. */
  preview?: string;
  width?: number;
  height?: number;
}
export interface ChatReaction {
  emoji: string;
  count: number;
  mine: boolean;
}
/** A task made from a message (todo-spec §28). */
export interface ChatTaskChip {
  id: number;
  title: string;
  done: boolean;
}
export interface ChatQuote {
  author?: string;
  text: string;
  link?: string;
}
export interface ChatMessage {
  id: string;
  rid: string;
  tmid: string | null;
  author_id: string;
  author_username: string | null;
  author_name: string | null;
  mine: boolean;
  body: string;
  ts: number;
  edited: boolean;
  system: string | null;
  replies: number;
  mentions_me: boolean;
  files: ChatFile[];
  reactions: ChatReaction[];
  quotes: ChatQuote[];
  tasks: ChatTaskChip[];
}
export interface Conversation {
  rid: string;
  thread: string;
  room: ChatRoom;
  title: string | null;
  messages: ChatMessage[];
  has_older: boolean;
  new_after: number | null;
  waiting: WaitingThread | null;
  mark: string | null;
}
export type ChatStatus =
  | { state: "connecting" | "signed_out" | "live" }
  | { state: "polling" | "offline"; reason: string };
interface OlderMessages {
  messages: ChatMessage[];
  has_older: boolean;
}

/** Whether the list shows chat: a chat view is open, and no task view or search over it. */
export function inChatView(): boolean {
  return state.chat.view !== null && state.tasks.view === null && !state.search;
}
/**
 * Whether the open room has nothing but its own conversation: the list of its
 * one row would say nothing, so the reader takes its place until a thread
 * starts. A filter keeps the list, to be changed or cleared there.
 */
export function roomAlone(): boolean {
  const rows = state.chat.rows;
  return inChatView() && state.chat.view === "room" && state.filter?.key !== chatViewKey() && rows.length === 1 && rows[0]!.thread === "";
}
/** What a room is called: a direct message by the people in it, where its label says only "Direct". */
export function roomName(room: ChatRoom): string {
  return room.kind === "d" ? (room.fname ?? room.name ?? room.label) : room.label;
}
/** How long a room stays muted, for a tooltip or the status line: `until 17:00`, `until Fri 09:00`. */
export function muteSpan(room: ChatRoom, now = new Date()): string {
  if (!room.muted) return "";
  if (room.muted_until == null) return "until you unmute it";
  return untilText(room.muted_until, now);
}
/** A moment ahead (ms) as a span says it: `until 17:00`, `until Fri 09:00`, `until tomorrow`. */
export function untilText(until: number, now = new Date()): string {
  const ends = new Date(until);
  const iso = ends.toISOString();
  const sameDay = ends.toDateString() === now.toDateString();
  const midnight = ends.getHours() === 0 && ends.getMinutes() === 0;
  if (midnight && ends.getTime() - now.getTime() <= 86_400_000) return "until tomorrow";
  return `until ${formatDate(iso, sameDay ? "%H:%M" : "%a %H:%M")}`;
}

/** Whether Today shows Waiting below its tasks, remembered on this Mac. */
export const TODAY_CHAT = ".todayChat";
/** A screenful: the section points at Waiting for the rest. */
const TODAY_CHAT_LIMIT = 30;
export function todayChat(): boolean {
  return state.chat.available && readSetting(TODAY_CHAT) === "on";
}
/** Whether Today's Waiting section has the cursor. */
export function chatSectionActive(): boolean {
  return state.tasks.view === "today" && state.tasks.section === "chat" && todayChat() && !state.search;
}
/** Whether chat has the cursor and the reader: Waiting, or its section in Today. */
export function chatActive(): boolean {
  return inChatView() || chatSectionActive();
}
/** …and the keyboard is in it rather than in the sidebar. */
export function inChatSection(): boolean {
  return state.focus !== "folders" && chatActive();
}
const keyOf = (rid: string, thread: string): string => `${rid}\u0000${thread}`;
const rowKey = (row: WaitingThread): string => keyOf(row.rid, row.thread);
/** Whether a thread is among those selected. */
export const chatSelected = (row: WaitingThread): boolean => state.chat.selected.has(rowKey(row));
/** What a command acts on: the selected threads, or the one under the cursor. */
export function chatTargets(): WaitingThread[] {
  if (state.chat.selected.size > 0) return state.chat.rows.filter((r) => state.chat.selected.has(rowKey(r)));
  const row = cursorThread();
  return row ? [row] : [];
}
/** What the filter (`F`) belongs to in chat: Waiting, or one room. */
export function chatViewKey(): string {
  return `chat\u0000${state.chat.view ?? ""}\u0000${state.chat.room ?? ""}`;
}

/**
 * The rows the filter keeps, by what they show: the filter of a chat view,
 * or Today's, which may keep no threads at all (`kinds.ts`). Of the search
 * language a thread knows `is:unread`, `is:read` and `f:`; any other `x:y`
 * looks for `y` in what the row shows.
 */
function narrowed(rows: WaitingThread[]): WaitingThread[] {
  const filter = state.filter;
  const today = state.tasks.view === "today" ? todayNarrowing() : null;
  if (today && !keepsKind("chat")) return [];
  const text = today ? today.rest : filter && filter.key === chatViewKey() ? filter.text : "";
  const words = text.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return rows;
  return rows.filter((r) => {
    const shown = `${r.author} ${r.room.label} ${r.title ?? ""} ${r.preview}`.toLowerCase();
    return words.every((w) => {
      const [field, value] = w.includes(":") ? [w.slice(0, w.indexOf(":")), w.slice(w.indexOf(":") + 1)] : ["", w];
      if (field === "is" && (value === "unread" || value === "new")) return r.unread;
      if (field === "is" && (value === "read" || value === "seen")) return !r.unread;
      if (field === "f" || field === "from") return r.author.toLowerCase().includes(value);
      return shown.includes(value);
    });
  });
}

/** Narrow the list to the filter's text, or widen it again. */
export async function filterChat(): Promise<void> {
  await loadWaiting();
  const { renderList } = await views();
  renderList();
  await followChatCursor();
}

export function cursorThread(): WaitingThread | undefined {
  return state.chat.rows[state.chat.cursor];
}
const iso = (ms: number): string => new Date(ms).toISOString();

// ----- Loading -----

/** Learn whether chat is configured, and how it stands. */
export async function initChat(): Promise<void> {
  const server = await call<string | null>("chat_server").catch(() => null);
  state.chat.available = !!server;
  state.chat.server = server;
  if (!server) return;
  state.chat.status = await call<ChatStatus>("chat_status").catch(() => null);
  const [reactions] = await Promise.all([call<string[]>("chat_reactions").catch(() => null), loadLinks(), loadThreads()]);
  if (reactions?.length) state.chat.reactions = reactions;
  try {
    const saved = JSON.parse(readSetting(DRAFTS) ?? "{}") as Record<string, string>;
    state.chat.drafts = new Map(Object.entries(saved));
  } catch {
    // A draft store that cannot be read starts empty.
  }
  await loadWaiting();
}

/** A pattern of `chat.links`, compiled; `url` takes `$0` to `$9`. */
export interface ChatLink {
  pattern: RegExp;
  url: string;
}

/** Take `chat.links` from the config, saying which patterns do not compile. */
async function loadLinks(): Promise<void> {
  const links = await call<{ pattern: string; url: string }[]>("chat_links").catch(() => []);
  const broken: string[] = [];
  state.chat.links = links.flatMap(({ pattern, url }) => {
    try {
      return [{ pattern: new RegExp(pattern, "g"), url }];
    } catch {
      broken.push(pattern);
      return [];
    }
  });
  if (broken.length) setStatus(`chat.links: not a regular expression: ${broken.join(", ")}`);
}

void listen("config-reloaded", () => {
  if (state.chat.available) void loadLinks();
});

/** Whether the server has threads, which it says once Orb has connected. */
async function loadThreads(): Promise<void> {
  state.chat.threads = await call<boolean>("chat_threads").catch(() => state.chat.threads);
}

/** Where drafts are kept, so that a restart does not lose them. */
const DRAFTS = ".chatDrafts";

/**
 * A room's rows, its own timeline first even before anything is written in
 * it: the row `c` writes to.
 */
function withRoomTimeline(rows: WaitingThread[], rid: string): WaitingThread[] {
  if (rows.some((r) => r.thread === "")) return rows;
  const room = state.chat.rooms.find((r) => r.rid === rid);
  if (!room) return rows;
  const timeline: WaitingThread = {
    rid, thread: "", room, title: null, replies: 0, author: "", preview: "Nothing written in the room yet",
    newest_at: 0, waiting_since: 0, mention: false, unread: false, files: false, task: false, waits: false, mine_last: false,
  };
  return [timeline, ...rows];
}

/**
 * Re-read Waiting and the rooms, and the rows the chat list shows — Waiting's,
 * or the open room's threads — keeping the cursor on the thread it was on.
 */
export async function loadWaiting(): Promise<void> {
  const [waiting, rooms] = await Promise.all([
    call<WaitingThread[]>("chat_waiting").catch(() => null),
    call<ChatRoom[]>("chat_rooms").catch(() => null),
  ]);
  if (waiting) {
    state.chat.waiting = waiting.filter((w) => !w.muted);
    state.chat.muted = waiting.filter((w) => w.muted);
  }
  if (rooms) {
    state.chat.rooms = rooms;
    void import("./chatmute").then((m) => m.followMutes());
  }
  let rows = state.chat.waiting;
  const rid = state.chat.view === "room" ? state.chat.room : null;
  if (rid) {
    const threads = await call<WaitingThread[]>("chat_room_threads", { rid }).catch(() => null);
    if (!threads) return;
    rows = withRoomTimeline(threads, rid);
  } else if (!waiting) {
    return;
  }
  rows = narrowed(rows);
  const current = cursorThread();
  state.chat.rows = rows;
  if (current) {
    const at = rows.findIndex((r) => r.rid === current.rid && r.thread === current.thread);
    if (at >= 0) state.chat.cursor = at;
  }
  state.chat.cursor = Math.max(0, Math.min(state.chat.cursor, rows.length - 1));
  // A thread that left Waiting leaves the selection with it.
  const keys = new Set(rows.map(rowKey));
  for (const key of state.chat.selected) if (!keys.has(key)) state.chat.selected.delete(key);
}

async function views() {
  const [list, reader, sidebar] = await Promise.all([import("./list"), import("./reader"), import("./sidebar")]);
  return { ...list, ...reader, ...sidebar };
}

/** Open Waiting in the list, its first thread in the reader. */
export async function openChat(): Promise<void> {
  if (!state.chat.available) {
    setStatus("chat is not configured: add a chat section to config.yml");
    return;
  }
  const { chatEntryIndex, render } = await views();
  const { syncSearchBox } = await import("./search");
  state.tasks.view = null;
  state.tasks.detail = null;
  state.search = null;
  state.info = null;
  state.chat.view = "waiting";
  state.chat.room = null;
  state.sidebarCursor = chatEntryIndex();
  syncSearchBox();
  (await import("./filter")).syncFilterToView();
  await loadWaiting();
  state.focus = "list";
  render();
  await followChatCursor();
}

/** Open a room: its threads in the list, its own timeline first. */
export async function openRoom(rid: string): Promise<void> {
  const { chatRoomEntryIndex, render } = await views();
  const { syncSearchBox } = await import("./search");
  saveDraft();
  state.tasks.view = null;
  state.tasks.detail = null;
  state.search = null;
  state.info = null;
  state.chat.view = "room";
  state.chat.room = rid;
  state.chat.cursor = 0;
  state.chat.selected.clear();
  state.chat.anchor = null;
  state.chat.conversation = null;
  state.chat.composing = false;
  (await import("./filter")).syncFilterToView();
  await loadWaiting();
  state.sidebarCursor = chatRoomEntryIndex(rid);
  syncSearchBox();
  state.focus = "list";
  render();
  await followChatCursor(true);
}

/** Leave chat for a folder or a task view. */
export function leaveChat(): void {
  saveDraft();
  state.chat.view = null;
  state.chat.room = null;
  state.chat.rows = state.chat.waiting;
  state.chat.conversation = null;
  state.chat.composing = false;
}

/** Find a room or a person by name, and open it: join a public room, start a direct message. */
async function pickRoom(): Promise<void> {
  const { openPicker } = await import("./picker");
  const item = (p: ChatPick) => ({
    value: [p.kind, p.rid ?? "", p.username ?? ""].join("\u0000"),
    label: p.label,
    detail: p.detail,
  });
  const find = async (term: string) => (await call<ChatPick[]>("chat_find", { term }).catch(() => [])).map(item);
  const picked = await openPicker({
    title: "Open chat room",
    placeholder: "room or person",
    items: await find(""),
    search: find,
  });
  if (!picked) return;
  const [kind, rid, username] = picked.split("\u0000");
  if (kind === "person") {
    setStatus(`Opening a message to ${username}…`);
    const direct = await call<string>("chat_open_direct", { username }).catch(() => null);
    if (direct) await openRoom(direct);
  } else if (kind === "join") {
    setStatus("Joining…");
    const joined = await call("chat_join", { rid }).then(() => true, () => false);
    if (joined) await openRoom(rid!);
  } else if (rid) {
    await openRoom(rid);
  }
}

/**
 * Start a conversation with the people picked (`chat-new-message`): one is a
 * direct message, several are one conversation with all of them. Either is
 * opened as it is when it exists already.
 */
async function newMessage(): Promise<void> {
  if (!state.chat.available) {
    setStatus("chat is not configured: add a chat section to config.yml");
    return;
  }
  const { openMultiPicker } = await import("./picker");
  // People only: the ones there are direct messages with, and anyone the
  // server finds by name.
  const people = async (term: string) =>
    (await call<ChatPick[]>("chat_find", { term }).catch(() => [] as ChatPick[]))
      .filter((p) => p.username && (p.kind === "person" || p.detail === "direct"))
      .map((p) => ({ value: p.username!, label: p.label, detail: `@${p.username}` }));
  const picked = await openMultiPicker({
    title: "New chat message",
    placeholder: "names; Enter adds one, Enter again starts",
    items: await people(""),
    search: people,
  });
  if (!picked?.length) return;
  setStatus(picked.length === 1 ? `Opening a message to ${picked[0]}…` : `Opening a message to ${picked.length} people…`);
  const rid = await call<string>("chat_open_group", { usernames: picked }).catch(() => null);
  if (!rid) return;
  await openRoom(rid);
  // The conversation's own timeline, with the keyboard in its composer.
  const at = state.chat.rows.findIndex((r) => r.thread === "");
  if (at >= 0) {
    await moveCursor(at, true, false);
    await followChatCursor(true);
  }
}

/**
 * Start a thread on a message of the room's timeline: the composer takes the
 * keyboard and says which message it answers. Without an id, the newest
 * message someone else wrote.
 */
async function replyInThread(id?: string): Promise<void> {
  if (!state.chat.threads) {
    setStatus("threads are turned off on the chat server");
    return;
  }
  const c = state.chat.conversation;
  if (!c || c.thread !== "") {
    setStatus("a thread starts on a message of a room's own timeline");
    return;
  }
  const message = id
    ? c.messages.find((m) => m.id === id)
    : [...c.messages].reverse().find((m) => !m.system && !m.mine) ?? [...c.messages].reverse().find((m) => !m.system);
  if (!message) return;
  const excerpt = message.body.split(/\s+/).join(" ");
  state.chat.replyTo = { id: message.id, excerpt: excerpt.length > 60 ? `${excerpt.slice(0, 60)}…` : excerpt };
  await startReply();
}

/** Leave the room under the sidebar's cursor, the open room, or the cursor thread's; a direct message is hidden. */
async function leaveRoom(): Promise<void> {
  const entry = state.focus === "folders" ? (await views()).currentEntry() : undefined;
  const rid = entry?.kind === "chat-room" ? entry.rid : state.chat.view === "room" ? state.chat.room : cursorThread()?.rid;
  const room = state.chat.rooms.find((r) => r.rid === rid);
  if (!rid || !room) return;
  const direct = room.kind === "d";
  const name = roomName(room);
  const question = direct
    ? `Hide the direct messages with ${name}?\n\nThey come back when either of you writes again.`
    : `Leave ${name}?\n\nIts threads leave Orb with it; you can join again from Open Chat Room.`;
  if (!(await askFirst(question, direct ? "Hide" : "Leave"))) return;
  const undo = `${direct ? "hide" : "leave"} ${name}`;
  const left = await call<boolean>("chat_leave", { rid, undo }).catch(() => null);
  if (left === null) return;
  const { offerUndo } = await import("./moving");
  const said = direct ? `Hid ${name}` : `Left ${name}`;
  if (left) offerUndo(said);
  else setStatus(said);
  if (state.chat.view === "room" && state.chat.room === rid) await openChat();
  else await refreshChat();
}

/** Create a room by name, `private` for a private group; asks for the name without one. */
async function createRoom(args: string[]): Promise<void> {
  const isPrivate = args.includes("private");
  const name = args.filter((a) => a !== "private").join("-").trim();
  if (!name) {
    const { promptCommand } = await import("./commandline");
    promptCommand("chat-create-room ");
    setStatus("name the room; add private for a private group");
    return;
  }
  setStatus(`Creating ${isPrivate ? "private group" : "#"}${name}…`);
  const rid = await call<string>("chat_create_room", { name, private: isPrivate }).catch(() => null);
  if (rid) await openRoom(rid);
}

/** Write to the room itself, outside any thread: the open room's, or the cursor thread's. */
async function writeToRoom(): Promise<void> {
  const row = cursorThread();
  if (!row) return;
  if (state.chat.view !== "room" || state.chat.room !== row.rid) await openRoom(row.rid);
  const at = state.chat.rows.findIndex((r) => r.thread === "");
  if (at < 0) return;
  await moveCursor(at, true, false);
  await followChatCursor(true);
}

/** The chat tables changed: re-read Waiting and the thread on screen. */
export async function refreshChat(): Promise<void> {
  if (!state.chat.available) return;
  await Promise.all([loadWaiting(), loadThreads()]);
  const { renderFolders, renderList } = await views();
  const { renderSteady } = await import("./statusline");
  renderFolders();
  renderSteady();
  if (state.tasks.view === "today" && todayChat()) {
    if (todayMixed()) await (await import("./mixed")).placeTimeline();
    paintChatSection();
  }
  if (!chatActive()) {
    // Nobody is looking at the thread the reader last held, and it may be
    // what just changed: coming back to it reads it again.
    state.chat.conversation = null;
    return;
  }
  if (inChatView()) renderList();
  await followChatCursor();
}

let seenTimer: number | undefined;
/** The thread the reader last drew, and how far from its end it was scrolled. */
let showing = "";
let scrollMemo: { key: string; fromBottom: number } | null = null;

/**
 * Whether the reader holds another thread than the cursor's. The cursor is an
 * index, so a refresh that drops a thread from Waiting while chat is not
 * active moves it onto the next one without the reader following.
 */
function staleConversation(): boolean {
  const c = state.chat.conversation;
  const row = cursorThread();
  return !!c && !!row && keyOf(c.rid, c.thread) !== rowKey(row);
}

/**
 * Threads read lately, the least recently shown first: the reader shows one
 * at once on coming back to it, while a fresh copy is fetched.
 */
const recentConversations = new Map<string, Conversation>();
const RECENT_KEPT = 40;

function keepConversation(c: Conversation): void {
  const key = keyOf(c.rid, c.thread);
  recentConversations.delete(key);
  recentConversations.set(key, c);
  for (const oldest of recentConversations.keys()) {
    if (recentConversations.size <= RECENT_KEPT) break;
    recentConversations.delete(oldest);
  }
}

/** Read the threads above and below the cursor ahead, for moving onto them. */
function prefetchNeighbours(): void {
  for (const at of [state.chat.cursor + 1, state.chat.cursor - 1]) {
    const row = state.chat.rows[at];
    if (at < 0 || at > lastRow() || !row || recentConversations.has(rowKey(row))) continue;
    // Not through `call`: a read ahead that fails is no news to the status line.
    void invoke<Conversation | null>("chat_conversation", { rid: row.rid, thread: row.thread })
      .then((c) => {
        if (c && !recentConversations.has(rowKey(row))) keepConversation(c);
      })
      .catch(() => undefined);
  }
}

/** A reader slower than this to show a thread says so in the log. */
const SLOW_SHOW_MS = 300;

/**
 * Show the cursor's thread in the reader, fetching it. With `answer` the
 * composer takes the keyboard as soon as the thread is the one shown, before
 * it arrives, so the first key typed is already in it; started any earlier, it
 * would be let go again here as the thread changes.
 */
export async function followChatCursor(answer = false): Promise<void> {
  const began = performance.now();
  const { renderReader } = await views();
  const row = cursorThread();
  if (!row) {
    state.chat.conversation = null;
    renderReader();
    return;
  }
  const key = keyOf(row.rid, row.thread);
  const shown = state.chat.conversation;
  if (!shown || keyOf(shown.rid, shown.thread) !== key) {
    if (keptUnread !== key) keptUnread = "";
    saveDraft();
    state.chat.composing = false;
    state.chat.replyTo = null;
    state.chat.editing = null;
    state.chat.uploads = [];
    closeCompletion();
    state.chat.conversation = recentConversations.get(key) ?? null;
    renderReader();
  }
  if (answer) await startReply();
  const asked = performance.now();
  const conversation = await call<Conversation | null>("chat_conversation", {
    rid: row.rid,
    thread: row.thread,
  }).catch(() => null);
  const answered = performance.now();
  const now = cursorThread();
  if (!now || keyOf(now.rid, now.thread) !== key || !chatActive()) return;
  // A refresh of the thread on screen keeps what scrolling back brought in.
  const previous = state.chat.conversation;
  if (conversation && previous && keyOf(previous.rid, previous.thread) === key) {
    const oldest = conversation.messages[0]?.ts ?? Number.POSITIVE_INFINITY;
    const earlier = previous.messages.filter((m) => m.ts < oldest);
    if (earlier.length > 0) {
      conversation.messages = [...earlier, ...conversation.messages];
      conversation.has_older = previous.has_older;
    }
  }
  state.chat.conversation = conversation;
  if (conversation) keepConversation(conversation);
  renderReader();
  const drawn = performance.now();
  if (drawn - began > SLOW_SHOW_MS) {
    const ms = (from: number, to: number) => Math.round(to - from);
    void invoke("log_frontend", {
      message:
        `chat: showing ${key.replace("\u0000", " ")} took ${ms(began, drawn)} ms: ` +
        `${ms(began, asked)} before asking, ${ms(asked, answered)} for the answer ` +
        `(${conversation?.messages.length ?? 0} messages), ${ms(answered, drawn)} drawing`,
    }).catch(() => undefined);
  }
  prefetchNeighbours();
  window.clearTimeout(seenTimer);
  if (row.unread && keptUnread !== key && state.settings.markReadAfterMs > 0) {
    seenTimer = window.setTimeout(() => {
      const still = cursorThread();
      if (still && keyOf(still.rid, still.thread) === key) {
        void call("chat_mark_seen", { rid: row.rid, thread: row.thread }).catch(() => undefined);
      }
    }, state.settings.markReadAfterMs);
  }
}

/** Draw the rows again where they are: Waiting's list, or Today's section. */
async function redrawRows(): Promise<void> {
  if (todayMixed()) {
    const { renderRows } = await import("./list");
    const { scrollCursorIntoView } = await import("./actions");
    renderRows();
    scrollCursorIntoView();
    return;
  }
  if (inChatView()) {
    const { renderList } = await views();
    renderList();
  } else {
    paintChatSection();
  }
  el.list.querySelector(".chat-row.cursor")?.scrollIntoView({ block: "nearest" });
}

/** The last row a cursor can stand on: Today's section lists a screenful. */
function lastRow(): number {
  const rows = state.chat.rows.length;
  return (chatSectionActive() && !todayMixed() ? Math.min(rows, TODAY_CHAT_LIMIT) : rows) - 1;
}

/**
 * Move the cursor and draw the rows again. The reader follows without
 * holding up the keys typed after this one, which the command queue would
 * otherwise keep waiting for the thread to arrive; `follow` off leaves it
 * to a caller that waits for the thread itself.
 */
async function moveCursor(to: number, redraw = false, follow = true): Promise<void> {
  const last = lastRow();
  const next = Math.max(0, Math.min(to, last));
  // The reader may hold nothing yet: Today's section keeps its cursor while
  // the conversation is let go on leaving chat.
  const behind = state.chat.conversation === null || staleConversation();
  if (last < 0 || (next === state.chat.cursor && !redraw && !behind)) return;
  if (next === state.chat.cursor && !behind) {
    await redrawRows();
    return;
  }
  state.chat.cursor = next;
  // The keys are chat's in the reader too, so the cursor can leave a thread
  // from under a panel: the reader then shows the thread it went to.
  state.info = null;
  await redrawRows();
  if (follow) void followChatCursor();
}

// ----- The list -----

/** The empty list, or `null` while something waits. */
export function chatEmptyView(): { icon: "chat" | "filter"; title: string; facts: string; leads: [string, string][] } | null {
  if (state.chat.status?.state === "signed_out") {
    const host = (state.chat.server ?? "").replace(/^https?:\/\//, "");
    return {
      icon: "chat",
      title: `Sign in to ${host}`,
      facts: "Orb opens the server’s sign-in page in a window of its own",
      leads: [["chat-sign-in", "Sign in"]],
    };
  }
  if (state.chat.rows.length > 0) return null;
  const filtered = state.filter?.key === chatViewKey() ? state.filter.text.trim() : "";
  if (filtered) {
    return {
      icon: "filter",
      title: "Nothing matches",
      facts: `No thread here matches “${filtered}”`,
      leads: [["filter-mode", "Change the filter"], ["clear-filter", "Clear it"]],
    };
  }
  const muted = state.chat.rooms.filter((r) => r.muted).length;
  if (state.chat.view === "room") {
    return { icon: "chat", title: "Nothing waits", facts: "Every thread has an answer", leads: [["chat-sync", "Check for messages"]] };
  }
  return {
    icon: "chat",
    title: "Nothing waits",
    facts: muted ? `Every thread has an answer, but those of ${muted} muted room${muted === 1 ? "" : "s"}` : "Every thread has an answer",
    leads: [["chat-sync", "Check for messages"], ...(muted ? ([["chat-muted-rooms", "See the muted rooms"]] as [string, string][]) : [])],
  };
}

function chatRow(row: WaitingThread, index: number, inToday = false): string {
  const cursor = index === state.chat.cursor;
  const selected = state.chat.selected.has(rowKey(row));
  // In a room's own list the room goes without saying, but for its timeline.
  const roomLabel = state.chat.view === "room" && !inToday ? (row.thread === "" ? "In the room" : "") : row.room.label;
  // A room lists threads that do not wait too: the arrow marks those whose
  // newest message is yours.
  const mark = row.waits ? (row.mention ? "@" : row.unread ? "N" : "") : row.mine_last ? "r" : "";
  const cls = ["row", "chat-row", inToday && "ch-row", row.unread && "unread", row.waits && row.muted && "muted", cursor && "cursor", selected && "selected"].filter(Boolean).join(" ");
  const text = row.title
    ? `<span class="text">${escape(row.title)}</span>${row.replies ? `<span class="thread">${row.replies}</span>` : ""}<span class="snip">${escape(row.preview)}</span>`
    : `<span class="text">${escape(row.preview)}</span>`;
  const meta =
    (row.task ? `<span class="tasks" title="A task made from this thread is open">${svgIcon("task")}</span>` : "") +
    (row.files ? `<span class="clip" title="The newest message carries files">${svgIcon("clip")}</span>` : "");
  return `<div class="${cls}" role="option" aria-selected="${cursor || selected}" data-chat="${index}">
    <span class="status"><i class="mark" data-g="${mark}"></i></span>
    <span class="from"><span class="name">${escape(row.author)}</span></span>
    <span class="subject">${roomLabel ? `<span class="room">${escape(roomLabel)}</span>` : ""}${text}</span>
    <span class="meta">${meta}</span>
    <span class="date">${row.newest_at ? escape(listDate(iso(row.newest_at), state.settings.dateFormat)) : ""}</span>
  </div>`;
}

/** Waiting as the list shows it. */
export function chatListMarkup(): string {
  return `<div class="chat-list" role="listbox" aria-label="Waiting">${state.chat.rows.map((r, i) => chatRow(r, i)).join("")}</div>`;
}

/**
 * Chat in Today, a section of its own under the tasks, with the line above it
 * to drag: the threads that wait. Shown whenever it is switched on, and saying
 * so when nothing waits, so that the switch always shows what it did.
 */
export function chatTodaySection(): string {
  const section = chatSectionMarkup();
  return section
    ? `<div class="split ch-split" data-ch-split role="separator" aria-orientation="horizontal" title="Drag to resize · double-click to fit the chat"></div>${section}`
    : "";
}

function chatSectionMarkup(): string {
  if (state.tasks.view !== "today" || !todayChat()) return "";
  const shown = state.chat.rows.slice(0, TODAY_CHAT_LIMIT);
  const beyond = state.chat.rows.length - shown.length;
  const rows = shown.length > 0 ? shown.map((r, i) => chatRow(r, i, true)).join("") : `<div class="empty">Nothing waits</div>`;
  return `<div class="ch-section">
      <div class="sec ch-section-head"><span class="sec-t"><span class="lbl">Chat</span><span class="n">${state.chat.rows.length}</span><span class="n">waiting</span></span></div>
      <div class="chat-list ch-list" role="listbox" aria-label="Chat: waiting">${rows}</div>${
        beyond > 0 ? `<button type="button" class="tk-more" data-chat-more>${beyond} more waiting in Chat</button>` : ""
      }
    </div>`;
}

/** Draw Today's section again in place, or take it away. */
export function paintChatSection(): void {
  // On the timeline the threads are rows among the mail.
  if (todayMixed()) {
    void import("./list").then(({ applyListSectionClass, paintTimeline }) => {
      applyListSectionClass();
      paintTimeline();
    });
    return;
  }
  const section = el.list.querySelector(":scope > .ch-section");
  const markup = chatSectionMarkup();
  if (section && markup) keepingScroll(el.list, () => (section.outerHTML = markup));
  else if (section) {
    el.list.querySelector(":scope > .ch-split")?.remove();
    section.remove();
  } else if (markup) el.list.querySelector(":scope > .tk-section:not(.full)")?.insertAdjacentHTML("afterend", chatTodaySection());
  // The list moves the keyboard on from there to the first section shown.
  if (state.tasks.section === "chat" && !markup) state.tasks.section = state.total > 0 ? "mail" : "tasks";
  void import("./list").then(({ applyListSectionClass }) => applyListSectionClass());
}

/** Put the cursor on a thread of Today's section, which takes the keyboard. */
export async function moveChatSectionCursor(to: number): Promise<void> {
  // Empty, the section still takes the keyboard, as the mail does: it is a
  // pane on screen like any other.
  if (!todayChat()) return;
  // The cursor is where it went before anything is awaited: the next key may
  // already be on its way, and has to move on from here.
  const fromMail = state.tasks.section === "mail";
  state.tasks.section = "chat";
  state.chat.cursor = Math.max(0, Math.min(to, lastRow()));
  // As in `moveCursor`: a panel does not stay over a thread the cursor came to.
  state.info = null;
  if (fromMail) {
    const { cancelMarkRead } = await import("./actions");
    cancelMarkRead();
  }
  paintChatSection();
  if (todayMixed()) {
    const { scrollCursorIntoView } = await import("./actions");
    scrollCursorIntoView();
  } else {
    el.list.querySelector(".ch-row.cursor")?.scrollIntoView({ block: "nearest" });
  }
  await followChatCursor();
}

/** Show chat in Today, or put it away. */
export async function setTodayChat(on = !todayChat()): Promise<void> {
  const { canPutAway, firstSection } = await import("./today");
  if (!on && !canPutAway("chat")) {
    setStatus("Chat is all Today shows, so it stays");
    return;
  }
  if (on) writeSetting(TODAY_CHAT, "on");
  else removeSetting(TODAY_CHAT);
  if (!on && state.tasks.section === "chat") state.tasks.section = firstSection();
  const { render } = await views();
  render();
  setStatus(on ? "Waiting shown in Today" : "Waiting put away from Today");
}

/** Select every thread from the anchor to `to`, as Shift-click and Shift-arrow do. */
function selectRange(to: number): void {
  const anchorKey = state.chat.anchor ?? (cursorThread() ? rowKey(cursorThread()!) : null);
  const from = Math.max(0, state.chat.rows.findIndex((r) => rowKey(r) === anchorKey));
  state.chat.anchor = anchorKey;
  state.chat.selected.clear();
  for (let i = Math.min(from, to); i <= Math.max(from, to); i++) {
    state.chat.selected.add(rowKey(state.chat.rows[i]!));
  }
}

function toggleSelected(row: WaitingThread): void {
  const key = rowKey(row);
  if (state.chat.selected.has(key)) state.chat.selected.delete(key);
  else state.chat.selected.add(key);
  state.chat.anchor = key;
}

function sayHowManySelected(): void {
  const n = state.chat.selected.size;
  setStatus(n === 0 ? "selection cleared" : `${n} thread${n === 1 ? "" : "s"} selected`);
}

el.list.addEventListener("click", (e) => {
  if ((e.target as HTMLElement).closest("[data-chat-more]")) {
    void openChat();
    return;
  }
  const element = (e.target as HTMLElement).closest<HTMLElement>("[data-chat]");
  if (!element) return;
  // On the timeline Shift and ⌘ are the list's, across every kind.
  if (todayMixed() && (e.shiftKey || e.metaKey)) return;
  const index = Number(element.dataset.chat);
  const row = state.chat.rows[index];
  if (!row) return;
  state.focus = "list";
  if (element.closest(".ch-section") || todayMixed()) state.tasks.section = "chat";
  if (e.metaKey) toggleSelected(row);
  else if (e.shiftKey) selectRange(index);
  else {
    state.chat.selected.clear();
    state.chat.anchor = null;
  }
  // A click keeps the keyboard with the list, for a, t or u; a double-click is
  // one to answer, and gives it to the composer. Counted on the click rather
  // than a `dblclick`, which never comes: the first click redrew the row.
  const answer = e.detail === 2 && !e.metaKey && !e.shiftKey;
  void moveCursor(index, true, !answer).then(async () => {
    if (answer) return followChatCursor(true);
    const { render } = await views();
    render();
  });
});

// Right-click acts on what was clicked: the selection when the row is in it,
// otherwise that row alone.
el.list.addEventListener("contextmenu", (e) => {
  const element = (e.target as HTMLElement).closest<HTMLElement>("[data-chat]");
  if (!element) return;
  e.preventDefault();
  const index = Number(element.dataset.chat);
  const row = state.chat.rows[index];
  if (!row) return;
  state.focus = "list";
  if (element.closest(".ch-section") || todayMixed()) state.tasks.section = "chat";
  if (!state.chat.selected.has(rowKey(row))) {
    state.chat.selected.clear();
    state.chat.anchor = null;
  }
  void moveCursor(index, true).then(openChatMenu);
});

async function openChatMenu(): Promise<void> {
  const { menuKeys } = await import("./menus");
  const many = chatTargets().length > 1;
  const rids = state.chat.view === "room" && state.chat.room ? [state.chat.room] : chatTargets().map((t) => t.rid);
  const muted = state.chat.rooms.filter((r) => r.muted && rids.includes(r.rid));
  await call("open_chat_menu", {
    disabled: [...(many ? ["chat-reply", "chat-open-in-browser"] : []), ...(muted.length ? [] : ["chat-unmute-room"])],
    labels: {
      ...(many ? { "chat-done": `Done (${chatTargets().length} threads)` } : {}),
      ...(muted.length === 1 ? { "chat-unmute-room": `Unmute Room (muted ${muteSpan(muted[0]!)})` } : {}),
    },
    ...menuKeys({ ...state.bindings.index_normal, ...state.bindings.chat_normal }),
  });
}

// ----- The conversation -----

/** A colour of its own for each person, from their name. */
function avatar(name: string): string {
  let hash = 0;
  for (const c of name) hash = (hash * 31 + c.charCodeAt(0)) | 0;
  const initials = name
    .split(/[\s._-]+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => w[0]!.toUpperCase())
    .join("");
  return `<span class="av" style="--avc: hsl(${Math.abs(hash) % 360} 45% 48%)" aria-hidden="true">${escape(initials || "?")}</span>`;
}

/** Rocket.Chat's inline markup in a stretch of text. */
function formatInline(text: string): string {
  return withEmoji(escape(text))
    .replace(/`([^`\n]+)`/g, "<code>$1</code>")
    .replace(/(https?:\/\/[^\s<]+[^\s<.,;:!?)])/g, '<a class="lnk" href="$1">$1</a>')
    .replace(/(^|\s)\*([^*\n]+)\*(?=\s|$|[.,;:!?])/g, "$1<b>$2</b>")
    .replace(/(^|\s)_([^_\n]+)_(?=\s|$|[.,;:!?])/g, "$1<i>$2</i>")
    .replace(/(^|\s)~([^~\n]+)~(?=\s|$|[.,;:!?])/g, "$1<s>$2</s>")
    .replace(/(^|[\s(])@([\w.-]+)/g, '$1<span class="men">@$2</span>')
    .replace(/\n/g, "<br>")
    .split(/(<a\b[^>]*>[\s\S]*?<\/a>|<code>[\s\S]*?<\/code>|<[^>]+>)/)
    .map((part, i) => (i % 2 === 1 ? part : state.chat.links.reduce(linkPattern, part)))
    .join("");
}

/**
 * The matches of one of `chat.links` in a stretch of escaped text, as links.
 * A match lies wholly in one stretch, so a link already made — by an earlier
 * pattern too — is never linked again.
 */
function linkPattern(escaped: string, link: ChatLink): string {
  if (escaped.includes("<a ")) {
    return escaped
      .split(/(<a\b[^>]*>[\s\S]*?<\/a>)/)
      .map((part, i) => (i % 2 === 1 ? part : linkPattern(part, link)))
      .join("");
  }
  const text = escaped.replace(/&(lt|gt|quot|amp);/g, (_, e: string) => ({ lt: "<", gt: ">", quot: '"', amp: "&" })[e]!);
  let out = "";
  let last = 0;
  for (const m of text.matchAll(link.pattern)) {
    if (!m[0]) continue;
    const href = link.url.replace(/\$(\d)/g, (_, n: string) => encodeURIComponent(m[Number(n)] ?? ""));
    out += `${escape(text.slice(last, m.index))}<a class="lnk" href="${escape(href)}">${escape(m[0])}</a>`;
    last = m.index + m[0].length;
  }
  return out + escape(text.slice(last));
}

/** Rocket.Chat's message markup: code blocks, `>` quotes and inline markup. */
export function formatBody(body: string): string {
  // A quote of another message starts the text with an empty link to it,
  // which the quote under the message stands for.
  const blocks = body.replace(/^(\[ \]\([^)\s]*\)\s*)+/, "").split(/```/);
  return blocks
    .map((part, i) => {
      // A word on the line of the opening fence names the language: ```ruby.
      if (i % 2 === 1) return `<pre class="cm-code">${escape(part.replace(/^[\w+#.-]*\n|\n$/g, ""))}</pre>`;
      // Lines that start with `>` are a quote, run together.
      const groups: { quote: boolean; lines: string[] }[] = [];
      for (const line of part.split("\n")) {
        const quote = line.startsWith(">");
        const text = quote ? line.replace(/^> ?/, "") : line;
        const last = groups[groups.length - 1];
        if (last && last.quote === quote) last.lines.push(text);
        else groups.push({ quote, lines: [text] });
      }
      return groups
        .map((g) =>
          g.quote
            ? `<blockquote class="cm-q">${formatInline(g.lines.join("\n"))}</blockquote>`
            : formatInline(g.lines.join("\n")),
        )
        .join("");
    })
    .join("");
}

function size(bytes: number | undefined): string {
  if (bytes === undefined) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function fileMarkup(id: string, index: number, f: ChatFile): string {
  return `<button type="button" class="cm-file" data-chat-file="${escape(id)}" data-chat-file-index="${index}" title="Open ${escape(f.name)}">${svgIcon("clip")}${escape(f.name)} <span class="dim">${escape(size(f.size))}</span></button>`;
}
/** The box an image preview is drawn in, at most this wide and high. */
const PREVIEW_BOX = { width: 320, height: 240 };
/**
 * Image previews by message and file, once fetched: a `data:` URL, or null for
 * one that could not be had. Kept for the session, so that a redraw draws the
 * image at once rather than blinking.
 */
const previews = new Map<string, string | null>();
// Not the NUL the other keys use: this one goes into an attribute, where HTML
// turns NUL into U+FFFD.
const previewKey = (id: string, index: number): string => `${id}/${index}`;
/**
 * An image, drawn small in the conversation and opened with a click. The box
 * takes the image's shape before it loads, so that the view does not move
 * when it arrives.
 */
function imageMarkup(id: string, index: number, f: ChatFile): string {
  const url = previews.get(previewKey(id, index));
  if (url === null) return fileMarkup(id, index, f);
  const w = f.width ?? PREVIEW_BOX.width;
  const h = f.height ?? PREVIEW_BOX.height;
  const scale = Math.min(1, PREVIEW_BOX.width / w, PREVIEW_BOX.height / h);
  const box = `width: ${Math.max(24, Math.round(w * scale))}px; height: ${Math.max(24, Math.round(h * scale))}px`;
  return `<button type="button" class="cm-image" data-chat-file="${escape(id)}" data-chat-file-index="${index}" title="Open ${escape(f.name)}" style="${box}"><img alt="${escape(f.name)}" data-chat-preview="${escape(previewKey(id, index))}"${url ? ` src="${escape(url)}"` : ""}></button>`;
}
/** Fetch the previews the conversation shows and does not have yet. */
function loadPreviews(): void {
  for (const img of el.reader.querySelectorAll<HTMLImageElement>("img[data-chat-preview]:not([src])")) {
    const button = img.closest<HTMLElement>("[data-chat-file]");
    const key = img.dataset.chatPreview!;
    if (!button || previews.has(key) || requested.has(key)) continue;
    requested.add(key);
    const id = button.dataset.chatFile!;
    const index = Number(button.dataset.chatFileIndex);
    void call<string | null>("chat_file_preview", { id, index })
      .catch(() => null)
      .then((url) => {
        requested.delete(key);
        previews.set(key, url);
        const file = state.chat.conversation?.messages.find((m) => m.id === id)?.files[index];
        for (const shown of el.reader.querySelectorAll<HTMLImageElement>("img[data-chat-preview]")) {
          if (shown.dataset.chatPreview !== key) continue;
          // One that cannot be had is the file it is, as a file without a preview is.
          if (url) shown.src = url;
          else if (file) shown.closest("[data-chat-file]")?.replaceWith(markupNode(fileMarkup(id, index, file)));
        }
      });
  }
}
const requested = new Set<string>();
function markupNode(html: string): Node {
  const template = document.createElement("template");
  template.innerHTML = html;
  return template.content;
}

function messageMarkup(m: ChatMessage, previous: ChatMessage | undefined, inThread: boolean): string {
  const who = m.author_name || m.author_username || "someone";
  const time = escape(formatDate(iso(m.ts), "%H:%M"));
  if (m.system) {
    return `<div class="cm system"><div class="cm-b"><span class="cm-sys">${escape(who)} · ${escape(m.body || m.system)}</span> <time>${time}</time></div></div>`;
  }
  // Within five minutes, the same person's next message joins the one before.
  const cont =
    previous && !previous.system && previous.author_id === m.author_id && m.ts - previous.ts < 5 * 60_000;
  const files = m.files.map((f, i) => (f.preview ? imageMarkup(m.id, i, f) : fileMarkup(m.id, i, f))).join("");
  // The messages it quotes, each with its author; a click goes to it when it is on screen.
  const quotes = m.quotes
    .map((q) => {
      const target = /[?&]msg=([^&#]+)/.exec(q.link ?? "")?.[1];
      return `<div class="cm-quote"${target ? ` data-chat-goto="${escape(target)}"` : ""}>${q.author ? `<b>${escape(q.author)}</b>` : ""}<div>${formatBody(q.text)}</div></div>`;
    })
    .join("");
  // A reaction is a button: yours taken back, another's joined.
  const reactions = m.reactions
    .map(
      (r) =>
        `<button type="button" class="rx${r.mine ? " mine" : ""}" data-chat-react="${escape(r.emoji)}" data-chat-react-on="${!r.mine}" data-chat-react-to="${escape(m.id)}" title="${escape(r.emoji)}">${escape(drawEmoji(r.emoji))} ${r.count}</button>`,
    )
    .join("");
  // In the room's timeline a thread's first message says how many replies it has; in the thread, the replies are there.
  const replies = !inThread && m.replies > 0 && m.tmid === null ? `<span class="cm-replies">${m.replies} ${m.replies === 1 ? "reply" : "replies"}</span>` : "";
  const tasks = m.tasks
    .map(
      (t) =>
        `<button type="button" class="cm-task${t.done ? " done" : ""}" data-chat-open-task="${t.id}" title="Open the task">${svgIcon("task")}<span>${escape(t.title)}</span></button>`,
    )
    .join("");
  const cls = ["cm", cont && "cont", m.mine && "me", m.mentions_me && "mentions-me"].filter(Boolean).join(" ");
  // On the message under the pointer: the configured reactions, a thread on
  // it (in a room's timeline, where the server has threads), a task, and for
  // the user's own, editing and deleting.
  const tool = (data: string, icon: IconName, title: string) =>
    `<button type="button" ${data}="${escape(m.id)}" title="${escape(title)}">${svgIcon(icon)}</button>`;
  const quick = state.chat.reactions
    .map((code) => {
      const mine = m.reactions.some((r) => r.emoji === code && r.mine);
      return `<button type="button" class="q${mine ? " mine" : ""}" data-chat-react="${escape(code)}" data-chat-react-on="${!mine}" data-chat-react-to="${escape(m.id)}" title="${escape(code)}">${escape(drawEmoji(code))}</button>`;
    })
    .join("");
  const tools = `<span class="cm-tools">${quick ? `${quick}<i class="sep"></i>` : ""}${
    !inThread && state.chat.threads ? tool("data-chat-thread", "reply", "Reply in a thread on this message") : ""
  }${tool("data-chat-task", "task", "Make a task of this message")}${
    m.mine ? tool("data-chat-edit", "draft", "Edit") + tool("data-chat-delete", "trash", "Delete") : ""
  }</span>`;
  // A message that joins the one before shows its time in the gutter, on hover.
  const gutter = cont ? `<time class="cm-gt" title="${escape(readerDate(iso(m.ts)))}">${time}</time>` : avatar(who);
  return `<div class="${cls}" data-chat-message="${escape(m.id)}">${gutter}
    <div class="cm-b">${cont ? "" : `<div class="cm-h"><b>${escape(who)}</b><time title="${escape(readerDate(iso(m.ts)))}">${time}</time>${m.edited ? '<span class="cm-edited">edited</span>' : ""}</div>`}
    ${quotes}<div class="cm-x">${formatBody(m.body)}${cont && m.edited ? ' <span class="cm-edited">edited</span>' : ""}</div>${files ? `<div class="cm-files">${files}</div>` : ""}${tasks ? `<div class="cm-tasks">${tasks}</div>` : ""}${reactions || replies ? `<div class="cm-foot">${reactions}${replies}</div>` : ""}${tools}</div></div>`;
}

/** Above the composer's text: what it answers or edits, and the files it posts. */
function composerChips(thread: string): string {
  const off = (data: string, title: string) =>
    `<button type="button" class="cc-to-off" ${data} title="${escape(title)}">${svgIcon("close")}</button>`;
  const chips: string[] = [];
  if (state.chat.editing) {
    chips.push(`<span class="cc-to">${svgIcon("draft")}<span class="lbl">Editing your message</span></span>${off("data-chat-edit-off", "Stop editing")}`);
  } else if (state.chat.replyTo && !thread) {
    chips.push(`<span class="cc-to">${svgIcon("reply")}<span class="lbl">In a thread on</span> ${escape(state.chat.replyTo.excerpt)}</span>${off("data-chat-thread-off", "Write to the room instead")}`);
  }
  state.chat.uploads.forEach((file, i) => {
    chips.push(`<span class="cm-file">${svgIcon("clip")}${escape(file.name)} <span class="dim">${escape(size(file.size))}</span><button type="button" class="cc-to-off" data-chat-upload-off="${i}" title="Leave it out">${svgIcon("close")}</button></span>`);
  });
  return chips.length ? `<div class="cc-a">${chips.join("")}</div>` : "";
}

/** A thread's replies: its first message counts them, when it is among those loaded. */
function replyCount(c: Conversation, row: WaitingThread | undefined): number {
  const first = c.messages.find((m) => m.id === c.thread);
  return first?.replies ?? row?.replies ?? Math.max(0, c.messages.length - 1);
}

/** A thread with more messages than this folds its earlier replies away… */
const FOLD_LONGER_THAN = 8;
/** …but for the newest few. */
const FOLD_KEEP = 4;

/** The reader in a chat view: the thread's header, its messages and the composer. */
export function chatReaderMarkup(): string {
  // Called before the reader is redrawn: how far the conversation on screen
  // is from its end, so that the redraw keeps the place.
  const current = el.reader.querySelector<HTMLElement>("[data-chat-messages]");
  scrollMemo = current
    ? { key: showing, fromBottom: current.scrollHeight - current.scrollTop - current.clientHeight }
    : null;
  const row = cursorThread();
  if (staleConversation()) void followChatCursor();
  const c = staleConversation() ? (recentConversations.get(rowKey(row!)) ?? null) : state.chat.conversation;
  if (!row && !c) return `<div class="empty">Nothing waits</div>`;
  const room = c?.room ?? row!.room;
  const thread = c?.thread ?? row!.thread;
  // A direct message's own timeline is named after the person, a room's after the room.
  const title = c?.title ?? row?.title ?? (thread ? "Thread" : roomName(room));
  const waiting = c ? c.waiting : row;
  const people = new Set((c?.messages ?? []).filter((m) => !m.system).map((m) => (m.mine ? "you" : (m.author_name || m.author_username) ?? "")));
  const meta = [
    `<span>${escape(room.label)}</span>`,
    thread && c ? `<span>${replyCount(c, row)} ${replyCount(c, row) === 1 ? "reply" : "replies"}${people.size ? ` · ${escape([...people].join(", "))}` : ""}</span>` : "",
    waiting ? `<span class="wt">Waiting since ${escape(formatDate(iso(waiting.waiting_since), "%H:%M"))}</span>` : "",
  ].join("");

  let messages = `<div class="cv-loading">Loading…</div>`;
  if (c) {
    const newAt = c.messages.findIndex((m) => !m.mine && !m.system && c.new_after !== null && m.ts > c.new_after);
    const older = c.has_older
      ? `<button type="button" class="cv-older" data-chat-older><span class="lbl">${state.chat.loadingOlder ? "Loading earlier messages…" : "Earlier messages"}</span></button>`
      : "";
    const all = c.messages;
    // A long thread shows its first message and the newest replies, from the
    // one before what is new; the replies between fold away until asked for.
    let hidden: [number, number] | null = null;
    if (c.thread !== "" && !state.chat.unfolded.has(keyOf(c.rid, c.thread)) && all.length > FOLD_LONGER_THAN) {
      const start = all[0]?.id === c.thread ? 1 : 0;
      const end = Math.min(all.length - FOLD_KEEP, newAt > 0 ? newAt - 1 : all.length);
      if (end - start >= 2) hidden = [start, end];
    }
    const fold = hidden
      ? `<button type="button" class="cv-fold" data-chat-unfold><span class="lbl">${hidden[0] === 0 && c.has_older ? "Earlier replies" : `${hidden[1] - hidden[0]} earlier replies`}</span></button>`
      : "";
    messages =
      (hidden?.[0] === 0 ? "" : older) +
      all
        .map((m, i) => {
          if (hidden && i >= hidden[0] && i < hidden[1]) return i === hidden[0] ? fold : "";
          const previous = hidden && i === hidden[1] ? undefined : all[i - 1];
          const day = dayHeading(iso(m.ts));
          const newDay = !previous || dayHeading(iso(previous.ts)) !== day;
          return (
            (newDay ? `<div class="cv-day"><span class="lbl">${escape(day)}</span></div>` : "") +
            (i === newAt && i > 0 ? `<div class="cv-new"><span class="lbl">New</span></div>` : "") +
            messageMarkup(m, newDay ? undefined : previous, c.thread !== "")
          );
        })
        .join("");
  }
  const also = thread
    ? `<label class="tsh"><input type="checkbox" data-chat-also${state.chat.alsoInRoom ? " checked" : ""}> <span class="lbl">Also send to ${escape(roomName(room))}</span></label>`
    : `<span class="tsh"></span>`;
  const shown = state.chat.rooms.find((r) => r.rid === room.rid) ?? room;
  const muted = shown.muted
    ? `<button type="button" class="cv-chip cv-muted" data-chat-unmute title="Mentions of you and of the room still come through. Click to unmute">${svgIcon("mute")}<span class="lbl">Muted ${escape(shown.muted_until == null ? "" : muteSpan(shown))}</span><span class="lbl un">Unmute</span></button>`
    : "";
  return `<div class="cv-h"><div class="cv-t">${escape(title)}</div><div class="cv-m">${meta}
      ${muted}<button type="button" class="cv-chip" data-chat-browser title="Open in Rocket.Chat (gx)">${svgIcon("arrow")}<span class="lbl">Open in Rocket.Chat</span></button></div></div>
    <div class="cv-msgs" data-chat-messages>${messages}</div>
    <div class="cc${state.chat.composing ? " on" : ""}">${composerChips(thread)}<div class="cc-x" data-composer-slot></div>
      <div class="cc-f">${also}<button type="button" class="cc-send${state.chat.composing ? " pri" : ""}" data-chat-send><span class="lbl">Send</span> <kbd>↩</kbd></button></div></div>`;
}

// ----- The composer -----

/**
 * One text field for the window's life, put back into each fresh render of
 * the reader, so that a refresh while typing keeps the text, the caret and
 * the keyboard.
 */
const field = document.createElement("textarea");
field.className = "cc-field";
field.rows = 1;
field.dataset.chatComposer = "";
let fieldKey = "";

function conversationKey(): string {
  const c = state.chat.conversation;
  if (c) return keyOf(c.rid, c.thread);
  const row = cursorThread();
  return row ? keyOf(row.rid, row.thread) : "";
}

function saveDraft(): void {
  // An edit is not a draft: its text is the message's, until sent.
  if (!fieldKey || state.chat.editing) return;
  const before = state.chat.drafts.get(fieldKey) ?? "";
  if (field.value.trim()) state.chat.drafts.set(fieldKey, field.value);
  else state.chat.drafts.delete(fieldKey);
  if ((state.chat.drafts.get(fieldKey) ?? "") !== before) {
    writeSetting(DRAFTS, JSON.stringify(Object.fromEntries(state.chat.drafts)));
  }
}

function fit(): void {
  field.style.height = "auto";
  field.style.height = `${Math.min(field.scrollHeight, 200)}px`;
}

/** Put the composer into the reader just drawn, and scroll to the newest message. */
export function attachChatReader(): void {
  const slot = el.reader.querySelector<HTMLElement>("[data-composer-slot]");
  if (!slot) return;
  const key = conversationKey();
  if (key !== fieldKey) {
    saveDraft();
    field.value = state.chat.drafts.get(key) ?? "";
    fieldKey = key;
  }
  const row = cursorThread();
  const thread = state.chat.conversation?.thread ?? row?.thread ?? "";
  const shownRoom = state.chat.conversation?.room ?? row?.room;
  const room = shownRoom ? roomName(shownRoom) : "";
  field.placeholder = thread ? "Reply in the thread" : state.chat.replyTo ? "Start the thread" : `Write to ${room}`;
  slot.appendChild(field);
  slot.closest(".cc")?.appendChild(pop);
  fit();
  // A thread opens at its end. Redrawn, it keeps its distance from the end,
  // so older messages coming in above leave the view where it was; close to
  // the end, it follows new messages down.
  const messages = el.reader.querySelector<HTMLElement>("[data-chat-messages]");
  if (messages) {
    const fromBottom = scrollMemo && scrollMemo.key === key && scrollMemo.fromBottom > 40 ? scrollMemo.fromBottom : 0;
    messages.scrollTop = messages.scrollHeight - messages.clientHeight - fromBottom;
  }
  scrollMemo = null;
  showing = key;
  loadPreviews();
  if (state.chat.composing && document.activeElement !== field) field.focus({ preventScroll: true });
}

field.addEventListener("input", () => {
  saveDraft();
  fit();
  void updateCompletion();
});

// ----- Completion: @people, :emoji: and #rooms -----

interface Completion {
  /** Where the word being completed starts, its `@`, `:` or `#` included. */
  start: number;
  items: { label: string; detail: string; insert: string }[];
  index: number;
}
let completion: Completion | null = null;
const pop = document.createElement("div");
pop.className = "mpop";
pop.hidden = true;

function closeCompletion(): void {
  completion = null;
  pop.hidden = true;
}

/** People the composer can name: those in the conversation and in direct messages. */
function knownPeople(term: string): { label: string; detail: string; insert: string }[] {
  const people = new Map<string, string>();
  for (const m of state.chat.conversation?.messages ?? []) {
    if (m.author_username && !m.mine) people.set(m.author_username, m.author_name ?? m.author_username);
  }
  for (const r of state.chat.rooms) {
    if (r.kind === "d" && r.label === "Direct" && r.name) people.set(r.name, r.fname ?? r.name);
  }
  return [...people]
    .filter(([username, name]) => username.toLowerCase().includes(term) || name.toLowerCase().includes(term))
    .slice(0, 8)
    .map(([username, name]) => ({ label: name, detail: `@${username}`, insert: `@${username} ` }));
}

/** Offer what the word before the caret could be, as it is typed. */
async function updateCompletion(): Promise<void> {
  const before = field.value.slice(0, field.selectionStart ?? field.value.length);
  const match = /(^|[\s(])([@#:])([\w.+-]*)$/.exec(before);
  if (!match || (match[2] === ":" && match[3]!.length < 2)) {
    closeCompletion();
    return;
  }
  const [, , sigil, word] = match as unknown as [string, string, string, string];
  const term = word.toLowerCase();
  const start = before.length - word.length - 1;
  let items: Completion["items"] =
    sigil === ":"
      ? emojiMatching(term).map((e) => ({ label: `${e.glyph}  :${e.code}:`, detail: "", insert: `:${e.code}: ` }))
      : sigil === "#"
        ? state.chat.rooms
            .filter((r) => r.kind !== "d" && (r.name ?? "").toLowerCase().includes(term))
            .slice(0, 8)
            .map((r) => ({ label: `#${r.name}`, detail: "", insert: `#${r.name} ` }))
        : knownPeople(term);
  completion = items.length ? { start, items, index: 0 } : null;
  drawCompletion();
  // People the server knows beyond those, once two letters say who.
  if (sigil === "@" && term.length >= 2) {
    const found = await call<ChatPick[]>("chat_find", { term }).catch(() => [] as ChatPick[]);
    if (field.value.slice(0, field.selectionStart ?? 0) !== before) return;
    const more = found
      .filter((p) => p.username && !items.some((i) => i.insert === `@${p.username} `))
      .map((p) => ({ label: p.label, detail: `@${p.username}`, insert: `@${p.username} ` }));
    items = [...items, ...more].slice(0, 8);
    completion = items.length ? { start, items, index: completion?.index ?? 0 } : null;
    drawCompletion();
  }
}

function drawCompletion(): void {
  if (!completion) {
    pop.hidden = true;
    return;
  }
  pop.hidden = false;
  pop.innerHTML = completion.items
    .map(
      (item, i) =>
        `<div class="ci${i === completion!.index ? " on" : ""}" data-complete="${i}"><span>${escape(item.label)}</span><span class="d">${escape(item.detail)}</span></div>`,
    )
    .join("");
}

function acceptCompletion(index = completion?.index ?? 0): void {
  if (!completion) return;
  const item = completion.items[index];
  if (!item) return;
  const caret = field.selectionStart ?? field.value.length;
  field.value = field.value.slice(0, completion.start) + item.insert + field.value.slice(caret);
  const at = completion.start + item.insert.length;
  field.setSelectionRange(at, at);
  closeCompletion();
  saveDraft();
  fit();
}

pop.addEventListener("mousedown", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-complete]");
  if (!row) return;
  e.preventDefault();
  acceptCompletion(Number(row.dataset.complete));
});

// Pasted files join the message, read here since the clipboard gives no path.
field.addEventListener("paste", (e) => {
  const files = [...(e.clipboardData?.files ?? [])];
  if (files.length === 0) return;
  e.preventDefault();
  for (const file of files) {
    const reader = new FileReader();
    reader.onload = () => {
      const data = String(reader.result).replace(/^data:[^,]*,/, "");
      state.chat.uploads.push({ name: file.name || "pasted.png", size: file.size, data, mime: file.type });
      void views().then(({ renderReader }) => renderReader());
    };
    reader.readAsDataURL(file);
  }
});

/** Add files from disk to the message: from the open dialog, or dropped. */
export async function attachToMessage(paths: string[]): Promise<void> {
  if (!chatActive() || paths.length === 0) return;
  for (const path of paths) {
    state.chat.uploads.push({ name: path.split("/").pop() ?? path, size: 0, path });
  }
  await startReply();
}
field.addEventListener("focus", () => {
  if (state.chat.composing) return;
  state.chat.composing = true;
  state.focus = "reader";
  el.reader.querySelector(".cc")?.classList.add("on");
});
field.addEventListener("keydown", (e) => {
  if (e.isComposing) return;
  // An open completion takes the arrows, Tab, Enter and Esc first.
  if (completion) {
    const key = eventKey(e);
    const step = key === "<down>" || key === "<c-n>" ? 1 : key === "<up>" || key === "<c-p>" ? -1 : 0;
    if (step) {
      completion.index = (completion.index + step + completion.items.length) % completion.items.length;
      drawCompletion();
    } else if (key === "<tab>" || key === "<enter>") {
      acceptCompletion();
    } else if (key === "<esc>") {
      closeCompletion();
    } else {
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    return;
  }
  // Up in an empty composer edits the user's last message, as Rocket.Chat does.
  if (e.key === "ArrowUp" && field.value === "" && !state.chat.editing) {
    const last = [...(state.chat.conversation?.messages ?? [])].reverse().find((m) => m.mine && !m.system);
    if (last) {
      e.preventDefault();
      startEdit(last);
      return;
    }
  }
  // Shift-Enter starts a new line, which `eventKey` does not tell from Enter.
  if (e.key === "Enter" && e.shiftKey) return;
  const command = state.bindings.chat_compose[eventKey(e)];
  if (!command) return;
  e.preventDefault();
  e.stopPropagation();
  // The keyboard leaves the field at once: a key pressed before the command
  // has run must reach the list, not the text.
  if (command === "chat-leave-composer") {
    saveDraft();
    state.chat.composing = false;
    state.focus = "list";
    el.list.focus({ preventScroll: true });
  }
  void import("./commands").then(({ dispatchCommand }) => dispatchCommand(command, 1));
});

/** Bring in the page of messages before the first one shown. */
async function loadOlder(): Promise<void> {
  const c = state.chat.conversation;
  if (!c || !c.has_older || state.chat.loadingOlder) return;
  const key = keyOf(c.rid, c.thread);
  const { renderReader } = await views();
  state.chat.loadingOlder = true;
  renderReader();
  const page = await call<OlderMessages>("chat_older", {
    rid: c.rid,
    thread: c.thread,
    before: c.messages[0]?.ts ?? Date.now(),
  }).catch(() => null);
  state.chat.loadingOlder = false;
  const now = state.chat.conversation;
  if (!now || keyOf(now.rid, now.thread) !== key) return;
  if (page) {
    const have = new Set(now.messages.map((m) => m.id));
    now.messages = [...page.messages.filter((m) => !have.has(m.id)), ...now.messages];
    now.has_older = page.has_older;
  }
  renderReader();
}

// Scrolled near the top, the conversation reaches further back.
el.reader.addEventListener(
  "scroll",
  (e) => {
    const target = e.target as HTMLElement;
    if (target.matches?.("[data-chat-messages]") && target.scrollTop < 120) void loadOlder();
  },
  { capture: true, passive: true },
);

el.reader.addEventListener("click", (e) => {
  const target = e.target as HTMLElement;
  const onMessage = target.closest<HTMLElement>("[data-chat-thread]");
  const reaction = target.closest<HTMLElement>("[data-chat-react]");
  const edit = target.closest<HTMLElement>("[data-chat-edit]");
  const remove = target.closest<HTMLElement>("[data-chat-delete]");
  const unattach = target.closest<HTMLElement>("[data-chat-upload-off]");
  const file = target.closest<HTMLElement>("[data-chat-file]");
  const makeTask = target.closest<HTMLElement>("[data-chat-task]");
  const openTask = target.closest<HTMLElement>("[data-chat-open-task]");
  const goto = target.closest<HTMLElement>("[data-chat-goto]");
  if (makeTask) {
    void makeTasks(false, makeTask.dataset.chatTask!);
  } else if (openTask) {
    void import("./taskview").then(async ({ openTaskById }) => {
      if (!(await openTaskById(Number(openTask.dataset.chatOpenTask)))) setStatus("the task is in no view");
    });
  } else if (file) {
    void openFile(file.dataset.chatFile!, Number(file.dataset.chatFileIndex));
  } else if (target.closest("[data-chat-unfold]")) {
    void import("./commands").then(({ dispatchCommand }) => dispatchCommand("chat-toggle-fold", 1));
  } else if (goto && !target.closest("a")) {
    showMessage(goto.dataset.chatGoto!);
  } else if (reaction) {
    void react(reaction.dataset.chatReactTo!, reaction.dataset.chatReact!, reaction.dataset.chatReactOn === "true");
  } else if (edit) {
    const message = state.chat.conversation?.messages.find((m) => m.id === edit.dataset.chatEdit);
    if (message) startEdit(message);
  } else if (remove) {
    void deleteMessage(remove.dataset.chatDelete!);
  } else if (unattach) {
    state.chat.uploads.splice(Number(unattach.dataset.chatUploadOff), 1);
    void views().then(({ renderReader }) => renderReader());
  } else if (target.closest("[data-chat-edit-off]")) {
    stopEdit();
  } else if (onMessage) void replyInThread(onMessage.dataset.chatThread!);
  else if (target.closest("[data-chat-thread-off]")) {
    state.chat.replyTo = null;
    void views().then(({ renderReader }) => renderReader());
  } else if (target.closest("[data-chat-older]")) void loadOlder();
  else if (target.closest("[data-chat-send]")) void sendDraft();
  else if (target.closest("[data-chat-browser]")) void openInBrowser();
  else if (target.closest("[data-chat-unmute]")) void import("./chatmute").then((m) => m.unmuteRooms());
});
/**
 * Put a message's tools beside its widest line, or over the end of its first
 * line when they would not fit there.
 */
function placeTools(message: HTMLElement): void {
  const tools = message.querySelector<HTMLElement>(":scope > .cm-b > .cm-tools");
  if (!tools) return;
  message.classList.remove("tools-over");
  const fits = tools.getBoundingClientRect().right <= message.getBoundingClientRect().right;
  message.classList.toggle("tools-over", !fits);
}

el.reader.addEventListener("mouseover", (e) => {
  const message = (e.target as HTMLElement).closest<HTMLElement>(".cm[data-chat-message]");
  if (message && !message.contains(e.relatedTarget as Node | null)) placeTools(message);
});

el.reader.addEventListener("change", (e) => {
  const box = (e.target as HTMLElement).closest<HTMLInputElement>("[data-chat-also]");
  if (box) state.chat.alsoInRoom = box.checked;
});

async function startReply(): Promise<void> {
  if (!chatActive() || !cursorThread()) return;
  state.chat.composing = true;
  state.focus = "reader";
  // At once, before the redraw: the next key typed belongs in the text.
  if (field.isConnected) field.focus({ preventScroll: true });
  const { render } = await views();
  render();
}

async function leaveComposer(): Promise<void> {
  saveDraft();
  state.chat.composing = false;
  field.blur();
  // Alone, the conversation keeps the keyboard: there is no list to go back to.
  state.focus = roomAlone() ? "reader" : "list";
  (roomAlone() ? el.reader : el.list).focus({ preventScroll: true });
  const { render } = await views();
  render();
}

/** Put one of the user's messages into the composer to change it. */
function startEdit(message: ChatMessage): void {
  saveDraft();
  state.chat.editing = { id: message.id, rid: message.rid };
  field.value = message.body;
  void startReply().then(() => {
    field.setSelectionRange(field.value.length, field.value.length);
    fit();
  });
}

function stopEdit(): void {
  state.chat.editing = null;
  field.value = state.chat.drafts.get(fieldKey) ?? "";
  void views().then(({ renderReader }) => renderReader());
}

async function sendDraft(): Promise<void> {
  const row = cursorThread();
  const c = state.chat.conversation;
  const rid = c?.rid ?? row?.rid;
  const timeline = c?.thread ?? row?.thread ?? "";
  // A reply to a message of the timeline starts a thread on it.
  const thread = timeline === "" && state.chat.replyTo ? state.chat.replyTo.id : timeline;
  const text = field.value.trim();
  const uploads = state.chat.uploads;
  if (!rid || (!text && uploads.length === 0)) return;
  closeCompletion();
  const editing = state.chat.editing;
  try {
    if (editing) {
      await call("chat_edit", { rid: editing.rid, id: editing.id, text });
    } else if (uploads.length > 0) {
      setStatus(`Sending ${uploads.length} file${uploads.length === 1 ? "" : "s"}…`);
      await call("chat_send_files", {
        rid,
        thread,
        text,
        files: uploads.map((f) => ({ name: f.name, mime: f.mime ?? null, path: f.path ?? null, data_base64: f.data ?? null })),
      });
    } else {
      await call("chat_send", { rid, thread, text, alsoInRoom: thread !== "" && state.chat.alsoInRoom });
    }
  } catch {
    // The text and the files stay in the composer, and the status line says why.
    return;
  }
  field.value = "";
  state.chat.editing = null;
  state.chat.uploads = [];
  if (!editing) {
    state.chat.drafts.delete(keyOf(rid, timeline));
    writeSetting(DRAFTS, JSON.stringify(Object.fromEntries(state.chat.drafts)));
  }
  state.chat.alsoInRoom = false;
  state.chat.replyTo = null;
  await refreshChat();
  // In a room the conversation stays, ready for the next message; in Waiting
  // and Today the answer takes the thread away, and the keyboard goes back.
  const now = cursorThread();
  if (chatActive() && now && keyOf(now.rid, now.thread) === keyOf(rid, timeline)) await startReply();
  else await leaveComposer();
  setStatus(editing ? "Edited" : "Sent");
}

/** Open a message's file with its default application, fetching it the first time. */
async function openFile(id: string, index: number): Promise<void> {
  const name = state.chat.conversation?.messages.find((m) => m.id === id)?.files[index]?.name ?? "the file";
  setStatus(`Opening ${name}…`);
  const opened = await call<{ path: string; revealed: boolean; declined: boolean }>("chat_open_file", { id, index }).catch(() => null);
  if (!opened) return;
  if (opened.declined) setStatus(`${name} not opened`);
  else setStatus(opened.revealed ? `Revealed ${name} in Finder` : `Opened ${name}`);
}

/** Scroll to a message of the conversation on screen, and point it out. */
function showMessage(id: string, missing = "the quoted message is not in this conversation"): void {
  const message = el.reader.querySelector<HTMLElement>(`[data-chat-message="${CSS.escape(id)}"]`);
  if (!message) {
    setStatus(missing);
    return;
  }
  message.scrollIntoView({ block: "center" });
  message.classList.remove("flash");
  void message.offsetWidth;
  message.classList.add("flash");
}

/**
 * Make a task of a message (todo-spec §28): `message` of the conversation on
 * screen, or the newest message of each thread `a` would act on. Each thread
 * leaves Waiting; undo trashes the tasks and lets the threads wait again.
 */
async function makeTasks(ask: boolean, message?: string): Promise<void> {
  const c = state.chat.conversation;
  const sources =
    message && c
      ? [{ rid: c.rid, thread: c.thread, message }]
      : chatTargets().map((r) => ({ rid: r.rid, thread: r.thread, message: null as string | null }));
  if (sources.length === 0) return;
  let postpone: string | null = state.tasks.today;
  if (ask) {
    const { pickDate } = await import("./taskedit");
    const chosen = await pickDate("date", "Postpone until", state.tasks.today);
    if (!chosen) return;
    postpone = chosen.kind === "clear" ? null : chosen.date;
  }
  const { describeTasks } = await import("./tasklist");
  const { refreshTaskBadge } = await import("./taskview");
  const { groupUndo, offerUndo } = await import("./moving");
  const made: string[] = [];
  const endGroup = await groupUndo();
  try {
    for (const source of sources) {
      const detail = await call<{ task: { title: string } }>("chat_create_task", { ...source, postpone }).catch(
        () => null,
      );
      if (!detail) break;
      made.push(detail.task.title);
    }
    if (made.length === 0) return;
    const what = made.length === 1 ? `task "${made[0]!}"` : describeTasks(made.length);
    const where = postpone === state.tasks.today ? "in Today" : postpone ? `for ${postpone}` : "in Someday";
    offerUndo(`created ${what} ${where}`);
  } finally {
    await endGroup(`create ${describeTasks(made.length)} from chat`);
  }
  state.chat.selected.clear();
  state.chat.anchor = null;
  state.tasks.counts.clear();
  await refreshTaskBadge();
  await refreshChat();
}

/** Open a thread a notification announced: in Waiting, or in its room once it no longer waits. */
export async function openChatThread(rid: string, thread: string): Promise<void> {
  if (!state.chat.available) return;
  await openChat();
  let at = state.chat.rows.findIndex((r) => r.rid === rid && r.thread === thread);
  if (at < 0) {
    await openRoom(rid);
    at = state.chat.rows.findIndex((r) => r.thread === thread);
  }
  if (at < 0) return;
  await moveCursor(at, true, false);
  await followChatCursor(true);
}

/**
 * Open the chat message a task links (`chat://host/rid/message?thread=…`):
 * its room, its thread, and the message pointed out.
 */
export async function openChatLink(id: string): Promise<void> {
  const parts = /^chat:\/\/[^/]+\/([^/]+)\/([^/?]+)(?:\?thread=(.+))?$/.exec(id);
  if (!parts) return;
  const [, rid, message, thread = ""] = parts as unknown as [string, string, string, string?];
  if (!state.chat.available) {
    setStatus("chat is not configured: add a chat section to config.yml");
    return;
  }
  await openRoom(rid);
  const at = state.chat.rows.findIndex((r) => r.thread === thread);
  if (at < 0) {
    setStatus("the message's thread is not in its room any more");
    return;
  }
  state.chat.unfolded.add(keyOf(rid, thread));
  await moveCursor(at, true, false);
  await followChatCursor();
  showMessage(message, "the message is further back than the conversation shows; scroll up for it");
}

/** Add or take back a reaction, and show it at once. */
async function react(id: string, emoji: string, on: boolean): Promise<void> {
  const c = state.chat.conversation;
  if (!c) return;
  const undo = `${on ? "react" : "take back"} ${emoji}`;
  const done = await call("chat_react", { rid: c.rid, id, emoji, on, undo }).then(
    () => true,
    () => false,
  );
  await refreshChat();
  if (done) {
    const { offerUndo } = await import("./moving");
    offerUndo(on ? `reacted ${emoji}` : `took back ${emoji}`);
  }
}

/** Delete one of the user's messages, after asking. */
async function deleteMessage(id: string): Promise<void> {
  const c = state.chat.conversation;
  if (!c) return;
  if (!(await askFirst("Delete this message?\n\nIt goes for everyone in the room.", "Delete"))) return;
  const deleted = await call("chat_delete", { rid: c.rid, id }).then(() => true, () => false);
  if (deleted) {
    setStatus("Deleted");
    await refreshChat();
  }
}

// ----- What the user does -----

/** Done for the selected threads, or the cursor's: out of Waiting until someone writes again. */
async function markDone(): Promise<void> {
  const rows = chatTargets();
  if (rows.length === 0) return;
  const { groupUndo, offerUndo } = await import("./moving");
  const name = (row: WaitingThread) => row.title ?? roomName(row.room);
  let done = 0;
  const endGroup = await groupUndo();
  let label = "";
  try {
    for (const row of rows) {
      const ok = await call("chat_done", { rid: row.rid, thread: row.thread, undo: `done: ${name(row)}` }).then(
        () => true,
        () => false,
      );
      if (!ok) break;
      done++;
    }
    label = `done: ${done === 1 ? name(rows[0]!) : `${done} threads`}`;
    if (done > 0) offerUndo(label);
  } finally {
    await endGroup(label || undefined);
  }
  if (done === 0) return;
  state.chat.selected.clear();
  state.chat.anchor = null;
  await refreshChat();
}

/** The thread made unread with `m`, which the reader does not mark seen again until left. */
let keptUnread = "";

/** Mark the threads read, in Orb and on the server — or unread, when all of them are read. */
async function toggleRead(): Promise<void> {
  const rows = chatTargets();
  if (rows.length === 0) return;
  const read = rows.some((r) => r.unread);
  const { groupUndo, offerUndo } = await import("./moving");
  const said = `${rows.length === 1 ? "Marked" : `Marked ${rows.length} threads`} ${read ? "read" : "unread"}`;
  const endGroup = await groupUndo();
  try {
    for (const row of rows) {
      const undo = `mark ${row.title ?? roomName(row.room)} ${read ? "read" : "unread"}`;
      const ok = await call("chat_set_read", { rid: row.rid, thread: row.thread, read, undo }).then(
        () => true,
        () => false,
      );
      if (!ok) return;
    }
  } finally {
    await endGroup(rows.length === 1 ? undefined : `mark ${rows.length} threads ${read ? "read" : "unread"}`);
  }
  const cursor = cursorThread();
  keptUnread = !read && cursor && rows.includes(cursor) ? keyOf(cursor.rid, cursor.thread) : "";
  window.clearTimeout(seenTimer);
  offerUndo(said);
  await refreshChat();
}

async function openInBrowser(): Promise<void> {
  const c = state.chat.conversation;
  const row = cursorThread();
  const rid = c?.rid ?? row?.rid;
  if (!rid) return;
  await call("chat_open_in_browser", { rid, thread: c?.thread ?? row?.thread ?? "" }).catch(() => undefined);
}

function scrollMessages(by: number): void {
  const messages = el.reader.querySelector<HTMLElement>("[data-chat-messages]");
  messages?.scrollBy({ top: by * (messages.clientHeight / 2) });
}

/** The chat commands; `runSteps` refuses them while chat is not configured. */
export const CHAT_COMMANDS: Record<string, Command> = {
  "open-chat": () => openChat(),
  // In Today, down from the last thread listed is on into the reading section.
  "chat-next": async (_a, n) => {
    if (todayMixed()) {
      const { navigate } = await import("./actions");
      navigate(Math.max(1, n));
      return;
    }
    if (chatSectionActive() && state.chat.cursor >= Math.min(state.chat.rows.length, TODAY_CHAT_LIMIT) - 1) {
      const { move } = await import("./actions");
      move(1);
      return;
    }
    await moveCursor(state.chat.cursor + Math.max(1, n));
  },
  // In Today, up from the first thread is back into the tasks.
  "chat-prev": async (_a, n) => {
    if (todayMixed()) {
      const { navigate } = await import("./actions");
      navigate(-Math.max(1, n));
      return;
    }
    if (chatSectionActive() && state.chat.cursor === 0) {
      const { move } = await import("./actions");
      move(-1);
      return;
    }
    await moveCursor(state.chat.cursor - Math.max(1, n));
  },
  "toggle-today-chat": async (args) => {
    const choice = args[0];
    if (choice !== undefined && choice !== "on" && choice !== "off") {
      setStatus("toggle-today-chat: on or off");
      return;
    }
    await setTodayChat(choice === undefined ? undefined : choice === "on");
  },
  "chat-first": async () => {
    if (todayMixed()) return (await import("./actions")).jump("top");
    await moveCursor(0);
  },
  "chat-last": async () => {
    if (todayMixed()) return (await import("./actions")).jump("bottom");
    await moveCursor(state.chat.rows.length - 1);
  },
  "chat-open": async () => {
    if (!chatActive() || !cursorThread()) return;
    state.focus = "reader";
    const { render } = await views();
    render();
  },
  "chat-reply": () => startReply(),
  "chat-open-room": () => pickRoom(),
  "chat-new-message": () => newMessage(),
  "chat-reply-thread": () => replyInThread(),
  /** Attach files from disk to the message being written. */
  "chat-attach": async () => {
    if (!chatActive()) return;
    const paths = await pickFiles({ kind: "files", title: "Attach Files" });
    await attachToMessage(paths);
  },
  /** React to the newest message somebody else wrote, from the configured reactions. */
  "chat-react": async () => {
    const c = state.chat.conversation;
    const message = [...(c?.messages ?? [])].reverse().find((m) => !m.mine && !m.system);
    if (!message) return;
    const { openPicker } = await import("./picker");
    const picked = await openPicker({
      title: "React",
      items: state.chat.reactions.map((code) => ({ value: code, label: `${drawEmoji(code)}  ${code}` })),
    });
    if (picked) await react(message.id, picked, !message.reactions.some((r) => r.emoji === picked && r.mine));
  },
  "chat-leave-room": () => leaveRoom(),
  "chat-mute-room": async (args) => (await import("./chatmute")).muteRooms(args),
  "chat-unmute-room": async () => (await import("./chatmute")).unmuteRooms(),
  "chat-muted-rooms": async () => (await import("./chatmute")).pickMutedRoom(),
  "chat-create-room": (args) => createRoom(args),
  "chat-write-room": () => writeToRoom(),
  "chat-send": () => sendDraft(),
  "chat-leave-composer": () => leaveComposer(),
  "chat-done": async () => {
    if (todayMixed() && (await (await import("./mixedselect")).actAcrossKinds("archive", "chat-done", true))) return;
    await markDone();
  },
  "chat-toggle-read": async () => {
    if (todayMixed() && (await (await import("./mixedselect")).actAcrossKinds("toggle-read", "chat-toggle-read", false))) return;
    await toggleRead();
  },
  /** Make a task of the thread's newest message; `postpone` asks for the date. */
  "chat-create-task": async (args) => {
    const postpone = args.includes("postpone") ? " postpone" : "";
    const across = todayMixed() && (await import("./mixedselect"));
    if (across && (await across.actAcrossKinds(`create-task-from-message${postpone}`, `chat-create-task${postpone}`, true))) return;
    await makeTasks(args.includes("postpone"));
  },
  /** Show every reply of a long thread, or fold the earlier ones away again. */
  "chat-toggle-fold": async () => {
    const c = state.chat.conversation;
    if (!c || c.thread === "") return;
    const key = keyOf(c.rid, c.thread);
    if (!state.chat.unfolded.delete(key)) state.chat.unfolded.add(key);
    const { renderReader } = await views();
    renderReader();
  },
  "chat-select": async (_a, n) => {
    if (todayMixed()) return (await import("./mixedselect")).toggleOnTimeline(Math.max(1, n));
    const row = cursorThread();
    if (!row) return;
    toggleSelected(row);
    sayHowManySelected();
    await moveCursor(state.chat.cursor + Math.max(1, n), true);
  },
  "chat-extend-down": async (_a, n) => {
    if (todayMixed()) return (await import("./mixedselect")).extendOnTimeline(Math.max(1, n));
    if (!cursorThread()) return;
    const to = Math.min(state.chat.cursor + Math.max(1, n), state.chat.rows.length - 1);
    selectRange(to);
    sayHowManySelected();
    await moveCursor(to, true);
  },
  "chat-extend-up": async (_a, n) => {
    if (todayMixed()) return (await import("./mixedselect")).extendOnTimeline(-Math.max(1, n));
    if (!cursorThread()) return;
    const to = Math.max(state.chat.cursor - Math.max(1, n), 0);
    selectRange(to);
    sayHowManySelected();
    await moveCursor(to, true);
  },
  "chat-select-all": async () => {
    for (const row of state.chat.rows) state.chat.selected.add(rowKey(row));
    sayHowManySelected();
    await moveCursor(state.chat.cursor, true);
  },
  "chat-clear-selection": async () => {
    if (todayMixed()) {
      const across = await import("./mixedselect");
      if (!across.anySelected()) return;
      across.clearTimelineSelection();
      await redrawRows();
      across.reportTimelineSelection();
      return;
    }
    if (state.chat.selected.size === 0) return;
    state.chat.selected.clear();
    state.chat.anchor = null;
    sayHowManySelected();
    await moveCursor(state.chat.cursor, true);
  },
  "chat-menu": () => openChatMenu(),
  "chat-also-in-room": async () => {
    state.chat.alsoInRoom = !state.chat.alsoInRoom;
    const { renderReader } = await views();
    renderReader();
  },
  "chat-open-in-browser": () => openInBrowser(),
  "chat-scroll-down": (_a, n) => scrollMessages(Math.max(1, n)),
  "chat-scroll-up": (_a, n) => scrollMessages(-Math.max(1, n)),
  "chat-sync": async () => {
    await call("chat_sync");
    setStatus("Checking for messages");
  },
  "chat-sign-in": async () => {
    await call("chat_sign_in_window");
  },
  "chat-sign-out": async () => {
    const host = (state.chat.server ?? "").replace(/^https?:\/\//, "");
    if (!(await askFirst(`Sign out of ${host}?\n\nOrb forgets the session; reading chat needs a new sign-in.`, "Sign Out"))) return;
    await call("chat_sign_out");
  },
};

/** The status line's word on chat, when it has one to say. */
export function chatStatusMarkup(): string {
  if (!state.chat.available) return "";
  const status = state.chat.status;
  if (!status) return "";
  switch (status.state) {
    case "live":
      return chatActive() ? `<span class="seg2"><span class="dot"></span>chat live</span>` : "";
    case "connecting":
      return chatActive() ? `<span>chat connecting</span>` : "";
    case "polling":
      return chatActive() ? `<span title="${escape(status.reason)}">chat polling</span>` : "";
    case "signed_out":
      return `<span class="warn" data-chat-sign-in title="Sign in to chat (:chat-sign-in)">chat signed out</span>`;
    case "offline":
      return `<span class="warn" title="${escape(status.reason)}">chat offline</span>`;
  }
}
