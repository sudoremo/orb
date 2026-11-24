import { TaskViewName, TASK_VIEWS } from "./tasklist";
import { openSelection } from "./actions";
import { call, setStatus } from "./bridge";
import { runCommand } from "./commands";
import { colourStyle, escape, icon, configIcon } from "./format";
import { keysOf } from "./keyhints";
import { spellKey } from "./keys";
import { roleIcon, svgIcon } from "./icons";
import { muteSpan, type ChatRoom } from "./chat";
import { AccountHealth, FolderInfo, HighlightBadge, HighlightStyle, PendingFolderCount, PinInfo } from "./model";
import { runSearchQuery } from "./moving";
import { PickerItem } from "./picker";
import { showAccountStatus } from "./panels";
import { syncSearchBox } from "./search";
import { READING_VIEWS, ReadingView, isReadingFolder, readingFolder, readingViewOf } from "./reading";
import { readSetting, writeSetting } from "./splitters";
import { renderSteady } from "./statusline";
import { WINDOW, el, openPanel, state, todayHasMail } from "./state";

/** Selectable folder rows only — account headers are not navigation targets. */
export const selectableFolders = (): FolderInfo[] =>
  state.folders.filter((f) => !f.is_account_header && f.folder_name !== null);
/**
 * One thing the sidebar cursor can be on.
 *
 * The pins come first, then the task views, the reading views and the views
 * that are not pinned, then each account's heading and the folders under it
 * that are not folded away, and chat last, in the order they are drawn.
 * Folders are held as an index rather than as the record, so the existing
 * folder machinery — which speaks in `selectableFolders()` positions — needs no
 * translation layer.
 */
type SidebarEntry =
  | { kind: "task"; view: TaskViewName }
  | { kind: "virtual"; index: number }
  | { kind: "account"; account: string }
  | { kind: "folder"; index: number }
  | { kind: "tag"; name: string }
  | { kind: "reading"; view: ReadingView }
  /** Chat's Waiting (PLAN.md, Phase 15), at the foot of the sidebar. */
  | { kind: "chat"; view: "waiting" }
  /** Chat's Direct and Channels, folded until opened. */
  | { kind: "chat-group"; group: ChatGroup }
  | { kind: "chat-room"; rid: string }
  /** The queued and refused mail, below the pins while there is any. */
  | { kind: "outbox" }
  /** A heading, which folds what is under it. */
  | { kind: "section"; section: SectionName };
type ChatGroup = "direct" | "channels";
type SectionName = "tasks" | "reading" | "views" | "tags" | "chat";
/** The sidebar's headings as they are drawn. */
const SECTION_LABELS: Record<SectionName, string> = {
  tasks: "Tasks",
  reading: "Reading",
  views: "Views",
  tags: "Tags",
  chat: "Chat",
};
/**
 * The task views this configuration has, which is none without `tasks`.
 *
 * Every count and every index in the sidebar goes through here rather than
 * through `TASK_VIEWS` directly. Tasks need a designated account (§1), and
 * without one each of their queries fails — so the views are not offered at
 * all. Showing five entries that report a configuration error when opened is
 * worse than showing a mail client that does not mention tasks, and it used to
 * be worse still: the failure came out of the boot sequence and left every pane
 * empty.
 */
function taskViews(): typeof TASK_VIEWS {
  return state.tasks.available ? TASK_VIEWS : [];
}
/** The tasks window's filter for a tag, as the task search reads it. */
export const tagFilter = (name: string): string => `tag:"${name.replace(/"/g, "")}"`;
/** The tags the tasks window's sidebar lists, in the registry's order. */
function sidebarTags(): string[] {
  return [...state.tasks.tags]
    .filter((tag) => tag.is_known !== false)
    .sort((a, b) => a.position - b.position || a.name.localeCompare(b.name))
    .map((tag) => tag.name);
}
/** The entries of the sidebar, in the order they are drawn: what is folded away is not among them. */
export function sidebarEntries(): SidebarEntry[] {
  const entries: SidebarEntry[] = [];
  const section = (name: SectionName, members: SidebarEntry[]) => {
    if (members.length === 0) return;
    entries.push({ kind: "section", section: name });
    if (!sectionFolded(name)) entries.push(...members);
  };
  // The chat window has chat's group alone, open.
  if (WINDOW === "chat") {
    if (state.chat.available) entries.push(...chatEntries());
    return entries;
  }
  const views = taskViews();
  const task = (v: (typeof TASK_VIEWS)[number]): SidebarEntry => ({ kind: "task", view: v.view });
  // The tasks window has the task views and the tags, and no mail: Today
  // above the rest, as the mailbox has it when nothing else is pinned.
  if (WINDOW === "tasks") {
    entries.push(...views.filter((v) => !v.inSection).map(task));
    section("tasks", views.filter((v) => v.inSection).map(task));
    if (views.length > 0) section("tags", sidebarTags().map((name) => ({ kind: "tag" as const, name })));
    return entries;
  }
  const pins = pinEntries();
  entries.push(...pins);
  if (state.outbox.length > 0) entries.push({ kind: "outbox" });
  section("tasks", unpinned(views.map(task), pins));
  section("reading", unpinned(readingViews().map(({ view }) => ({ kind: "reading" as const, view })), pins));
  section("views", unpinned(state.virtualFolders.map((_, index) => ({ kind: "virtual" as const, index })), pins));
  const pinnedFolders = folderPins(pins);
  let index = 0;
  for (const f of state.folders) {
    if (f.is_account_header) {
      entries.push({ kind: "account", account: f.account_name });
      continue;
    }
    if (f.folder_name === null) continue;
    const at = index++;
    // A pinned folder's own record is drawn among the pins, as is a folder
    // pinned from the window, and the reading folders are under Reading, not
    // among their account's.
    if (f.is_pinned || pinnedFolders.has(at) || isReadingFolder(f)) continue;
    if (!folded(f)) entries.push({ kind: "folder", index: at });
  }
  if (state.chat.available) section("chat", unpinned(chatEntries(), pins));
  return entries;
}
/** The entries the pins leave in a section: a pinned view has one door, at the top. */
function unpinned(entries: SidebarEntry[], pins: SidebarEntry[]): SidebarEntry[] {
  const keys = new Set(pins.map(entryKey));
  return entries.filter((e) => !keys.has(entryKey(e)));
}
/**
 * The pins at the top of the mailbox window's sidebar: `pinned`'s, in its
 * order, then those made with `toggle-pin`, in the order they were made.
 *
 * A pin of something this window has not got — a folder not synced yet, chat
 * signed out of the config, a room left — is left out, so `<c-1>`…`<c-9>` count
 * what is drawn. A folder `pinned` names is its pinned record, which stays in
 * its account's tree as well; anything else leaves its place.
 */
export function pinEntries(): SidebarEntry[] {
  if (WINDOW !== "main") return [];
  const entries: SidebarEntry[] = [];
  for (const pin of state.pins) {
    const entry = pinEntry(pin, true);
    if (entry) entries.push(entry);
  }
  for (const pin of madePins()) {
    if (state.pins.some((p) => samePin(p, pin))) continue;
    const entry = pinEntry(pin, false);
    if (entry) entries.push(entry);
  }
  return entries;
}
/**
 * What a pin names, as the config writes it, and as `toggle-pin` remembers it:
 * by name rather than by place, so it outlives a folder list in another order.
 * A room, which only `toggle-pin` pins, is named by its id.
 */
type PinSpec = Pick<PinInfo, "name" | "account" | "folder"> & { kind: PinInfo["kind"] | "room" };
/** The entry a pin opens, or null when this window has not got it. */
function pinEntry(pin: PinSpec, configured: boolean): SidebarEntry | null {
  if (pin.kind === "tasks" && taskViews().some((v) => v.view === pin.name)) {
    return { kind: "task", view: pin.name as TaskViewName };
  }
  if (pin.kind === "reading" && readingViews().some((v) => v.view === pin.name)) {
    return { kind: "reading", view: pin.name as ReadingView };
  }
  if (pin.kind === "chat" && state.chat.available) return { kind: "chat", view: "waiting" };
  if (pin.kind === "room" && state.chat.available && state.chat.rooms.some((r) => r.rid === pin.name)) {
    return { kind: "chat-room", rid: pin.name! };
  }
  if (pin.kind === "view") {
    const index = state.virtualFolders.findIndex((v) => v.name === pin.name);
    return index >= 0 ? { kind: "virtual", index } : null;
  }
  if (pin.kind === "folder") {
    // `pinned`'s folder is the record the core made for it; one pinned here is the folder itself.
    const index = selectableFolders().findIndex(
      (f) => !!f.is_pinned === configured && f.account_name === pin.account && f.folder_name === pin.folder,
    );
    return index >= 0 ? { kind: "folder", index } : null;
  }
  return null;
}
const samePin = (a: PinSpec, b: PinSpec): boolean =>
  a.kind === b.kind &&
  (a.kind === "chat" || (a.kind === "folder" ? a.account === b.account && a.folder === b.folder : a.name === b.name));
/** What an entry is as a pin, or null for what cannot be one: a heading, an account, the Outbox. */
function pinSpecOf(entry: SidebarEntry): PinSpec | null {
  const named = (kind: PinSpec["kind"], name: string): PinSpec => ({ kind, name, account: null, folder: null });
  switch (entry.kind) {
    case "task":
      return named("tasks", entry.view);
    case "reading":
      return named("reading", entry.view);
    case "chat":
      return named("chat", entry.view);
    case "chat-room":
      return named("room", entry.rid);
    case "virtual": {
      const view = state.virtualFolders[entry.index];
      return view ? named("view", view.name) : null;
    }
    case "folder": {
      const f = selectableFolders()[entry.index];
      return f ? { kind: "folder", name: null, account: f.account_name, folder: f.folder_name } : null;
    }
    default:
      return null;
  }
}
const MADE_PINS = ".sidebar.pins";
let madePinList: PinSpec[] | null = null;
/** The pins made with `toggle-pin`, remembered on this machine rather than in the config. */
function madePins(): PinSpec[] {
  if (!madePinList) {
    try {
      const saved: unknown = JSON.parse(readSetting(MADE_PINS) ?? "[]");
      madePinList = Array.isArray(saved) ? (saved as PinSpec[]).filter((p) => typeof p?.kind === "string") : [];
    } catch {
      madePinList = [];
    }
  }
  return madePinList;
}
/** The folders pinned with `toggle-pin`, as `selectableFolders()` positions: their place in the tree is empty. */
function folderPins(pins: SidebarEntry[]): Set<number> {
  const out = new Set<number>();
  for (const e of pins) if (e.kind === "folder" && !selectableFolders()[e.index]?.is_pinned) out.add(e.index);
  return out;
}
/**
 * `toggle-pin`: pin what the sidebar cursor is on — which is what is open,
 * unless the sidebar has the keyboard — below `pinned`'s pins, or unpin it.
 */
export function togglePin(): void {
  if (WINDOW !== "main") {
    setStatus("the pins are in the mailbox window");
    return;
  }
  if (state.focus !== "folders" && state.search && state.search.view === undefined) {
    setStatus("a search cannot be pinned: virtual_folders in config.yml keeps one");
    return;
  }
  const entry = currentEntry();
  const spec = entry ? pinSpecOf(entry) : null;
  if (!entry || !spec) {
    setStatus("only a folder, a view or a chat room can be pinned");
    return;
  }
  const key = entryKey(entry);
  const before = pinEntries().findIndex((e) => entryKey(e) === key);
  const label = (before >= 0 ? pinLabel(before + 1) : null) ?? entryLabel(entry);
  if (state.pins.some((p) => samePin(p, spec))) {
    setStatus(`${label} is pinned in config.yml`);
    return;
  }
  const pins = madePins();
  const at = pins.findIndex((p) => samePin(p, spec));
  if (at >= 0) pins.splice(at, 1);
  else pins.push(spec);
  writeSetting(MADE_PINS, JSON.stringify(pins));
  // The cursor stays on what it was on, wherever that now is.
  const reveal = destinations().find((d) => entryKey(d.entry) === key)?.reveal;
  state.sidebarCursor = Math.max(0, reveal ? reveal() : entryIndex(entry));
  renderFolders();
  const n = pinEntries().findIndex((e) => entryKey(e) === key) + 1;
  setStatus(at >= 0 ? `unpinned ${label}` : `pinned ${pinLabel(n) ?? label} as pin ${n}`);
}
/** How many of the pins drawn are `pinned`'s, which come first and stay in the config's order. */
function configuredPinCount(): number {
  return state.pins.filter((pin) => pinEntry(pin, true)).length;
}
/**
 * Move the pin made with `toggle-pin` that is drawn as pin `from` to land
 * before or after the one drawn as pin `onto`. Pins are kept by what they
 * name, so the move is made in that list — where a pin of something this
 * window has not got keeps its place among the rest.
 */
function movePin(from: number, onto: number, after: boolean): void {
  const drawn = pinEntries();
  const moving = drawn[from - 1] && pinSpecOf(drawn[from - 1]!);
  const target = drawn[onto - 1] && pinSpecOf(drawn[onto - 1]!);
  if (!moving || !target || samePin(moving, target)) return;
  const pins = madePins();
  const at = pins.findIndex((p) => samePin(p, moving));
  if (at < 0) return;
  const [taken] = pins.splice(at, 1);
  const to = pins.findIndex((p) => samePin(p, target));
  if (to < 0) {
    pins.splice(at, 0, taken!);
    return;
  }
  pins.splice(to + (after ? 1 : 0), 0, taken!);
  writeSetting(MADE_PINS, JSON.stringify(pins));
  const key = entryKey(drawn[from - 1]!);
  const n = pinEntries().findIndex((e) => entryKey(e) === key) + 1;
  state.sidebarCursor = entryIndex(drawn[from - 1]!);
  renderFolders();
  setStatus(`${pinLabel(n) ?? "the pin"} is pin ${n} now`);
}
/**
 * Reorder the pins made with `toggle-pin` by dragging one, with pointer
 * events as the task list does: the window's own file drop takes the
 * webview's drag events. The config's pins neither move nor take a drop.
 */
let pinPress: { pin: number; y: number; dragging: boolean } | null = null;
let swallowPinClick = false;
/** The made pin under the pointer, and whether the drop lands below its middle. */
function pinDropTarget(e: PointerEvent): { row: HTMLElement; pin: number; below: boolean } | null {
  const row = document.elementFromPoint(e.clientX, e.clientY)?.closest<HTMLElement>("#folders [data-pin]");
  const pin = Number(row?.dataset.pin);
  if (!row || !(pin > configuredPinCount())) return null;
  const rect = row.getBoundingClientRect();
  return { row, pin, below: e.clientY > rect.top + rect.height / 2 };
}
function clearPinDrag(): void {
  document.body.classList.remove("reordering");
  for (const marked of el.folders.querySelectorAll(".drop-above, .drop-below, .dragging")) {
    marked.classList.remove("drop-above", "drop-below", "dragging");
  }
}
el.folders.addEventListener("pointerdown", (e) => {
  if (e.button !== 0 || e.metaKey || e.shiftKey) return;
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-pin]");
  const pin = Number(row?.dataset.pin);
  if (!row || (e.target as HTMLElement).closest("[data-fold]") || !(pin > configuredPinCount())) return;
  pinPress = { pin, y: e.clientY, dragging: false };
});
window.addEventListener("pointermove", (e) => {
  if (!pinPress) return;
  if (!pinPress.dragging) {
    if (Math.abs(e.clientY - pinPress.y) < 4) return;
    pinPress.dragging = true;
    el.folders.querySelector(`[data-pin="${pinPress.pin}"]`)?.classList.add("dragging");
    document.body.classList.add("reordering");
  }
  for (const marked of el.folders.querySelectorAll(".drop-above, .drop-below")) {
    marked.classList.remove("drop-above", "drop-below");
  }
  const drop = pinDropTarget(e);
  drop?.row.classList.add(drop.below ? "drop-below" : "drop-above");
});
window.addEventListener("pointerup", (e) => {
  const was = pinPress;
  pinPress = null;
  if (!was?.dragging) return;
  // The click that ends a drag would open the pin.
  swallowPinClick = true;
  setTimeout(() => (swallowPinClick = false), 0);
  const drop = pinDropTarget(e);
  clearPinDrag();
  if (drop && drop.pin !== was.pin) movePin(was.pin, drop.pin, drop.below);
});
window.addEventListener("pointercancel", () => {
  pinPress = null;
  clearPinDrag();
});
el.folders.addEventListener(
  "click",
  (e) => {
    if (!swallowPinClick) return;
    swallowPinClick = false;
    e.stopImmediatePropagation();
  },
  true,
);
/** What an entry is called, for a message about it. */
function entryLabel(entry: SidebarEntry): string {
  return destinations().find((d) => entryKey(d.entry) === entryKey(entry))?.label ?? "it";
}
/** Whether an entry is among the pins, where no heading folds it. */
function pinned(entry: SidebarEntry): boolean {
  const key = entryKey(entry);
  return pinEntries().some((e) => entryKey(e) === key);
}
/** What pin `n` (from 1) is called in the sidebar, or null when there is none. */
export function pinLabel(n: number): string | null {
  const entry = pinEntries()[n - 1];
  if (!entry) return null;
  switch (entry.kind) {
    case "task":
      return TASK_VIEWS.find((v) => v.view === entry.view)?.label ?? entry.view;
    case "reading":
      return READING_VIEWS.find((v) => v.view === entry.view)?.label ?? entry.view;
    case "chat":
      return "Waiting";
    case "virtual":
      return state.virtualFolders[entry.index]?.name ?? null;
    case "folder": {
      const f = selectableFolders()[entry.index];
      return f ? pinnedFolderLabel(f) : null;
    }
    case "chat-room": {
      const room = state.chat.rooms.find((r) => r.rid === entry.rid);
      return room ? roomLabel(room) : null;
    }
    default:
      return null;
  }
}
/** A pinned folder's name: `pinned`'s `display_name`, else `Account: Folder`, as `pinned` names one without it. */
const pinnedFolderLabel = (f: FolderInfo): string =>
  f.is_pinned ? f.display_name : `${f.account_name}: ${f.display_name}`;
/**
 * Open pin `n` (from 1), as Enter on it in the sidebar does, and give the
 * keyboard to what it shows.
 */
export async function openPin(n: number): Promise<void> {
  if (WINDOW !== "main") {
    setStatus("the pins are in the mailbox window");
    return;
  }
  const pins = pinEntries();
  const entry = pins[n - 1];
  if (!Number.isInteger(n) || !entry) {
    setStatus(
      pins.length === 0
        ? "nothing is pinned: pinned in config.yml lists what is"
        : `no pin ${n}: ${pins.length} ${pins.length === 1 ? "is" : "are"} pinned`,
    );
    return;
  }
  await openEntry(entryIndex(entry));
}
/** Open the entry at `index`, as Enter on it in the sidebar does, and give the keyboard to what it shows. */
async function openEntry(index: number): Promise<void> {
  state.sidebarCursor = index;
  state.focus = "folders";
  await openSelection();
  if (state.focus === "folders") state.focus = "list";
  renderFolders();
}
/** Something `go-to` offers: a sidebar entry, its row in the picker, and how to bring it into view. */
interface Destination {
  entry: SidebarEntry;
  label: string;
  detail?: string;
  keywords?: string;
  reveal: () => number;
}
/**
 * Everything the sidebar of this window can open, folded away or not, in the
 * order it is drawn: the pins as they sit in their sections, and a pinned
 * folder once, in its account.
 */
function destinations(): Destination[] {
  const out: Destination[] = [];
  const views = taskViews();
  const task = (v: (typeof TASK_VIEWS)[number]): Destination => ({
    entry: { kind: "task", view: v.view },
    label: v.label,
    detail: "Tasks",
    reveal: () => taskEntryIndex(v.view),
  });
  const chat = (): Destination[] => {
    if (!state.chat.available) return [];
    return [
      { entry: { kind: "chat", view: "waiting" }, label: "Waiting", detail: "Chat", reveal: chatEntryIndex },
      ...state.chat.rooms.map((room) => ({
        entry: { kind: "chat-room" as const, rid: room.rid },
        label: roomLabel(room),
        detail: room.kind === "d" ? "Direct" : "Channel",
        keywords: [room.name, room.fname].filter(Boolean).join(" "),
        reveal: () => chatRoomEntryIndex(room.rid),
      })),
    ];
  };
  if (WINDOW === "chat") return chat();
  if (WINDOW === "tasks") {
    out.push(...views.map(task));
    if (views.length > 0) {
      for (const name of sidebarTags()) {
        const entry: SidebarEntry = { kind: "tag", name };
        out.push({ entry, label: name, detail: "Tag", reveal: () => (unfoldSection("tags"), entryIndex(entry)) });
      }
    }
    return out;
  }
  if (state.outbox.length > 0) {
    const entry: SidebarEntry = { kind: "outbox" };
    out.push({ entry, label: "Outbox", reveal: () => entryIndex(entry) });
  }
  out.push(...views.map(task));
  for (const { view, label } of readingViews()) {
    out.push({ entry: { kind: "reading", view }, label, detail: "Reading", reveal: () => readingEntry(view) });
  }
  state.virtualFolders.forEach((view, index) => {
    const entry: SidebarEntry = { kind: "virtual", index };
    out.push({
      entry,
      label: view.name,
      detail: "View",
      reveal: () => {
        if (!pinned(entry)) unfoldSection("views");
        return entryIndex(entry);
      },
    });
  });
  selectableFolders().forEach((folder, index) => {
    if (folder.is_pinned || isReadingFolder(folder)) return;
    out.push({
      entry: { kind: "folder", index },
      label: `${folder.account_name} / ${folder.display_name}`,
      detail: folder.display_name === folder.folder_name ? undefined : folder.folder_name!,
      reveal: () => folderEntry(index),
    });
  });
  out.push(...chat());
  return out;
}
/** Everything the sidebar of this window can open, as rows of the palette's empty prefix. */
export function placeItems(): PickerItem[] {
  return destinations().map((d) => ({ value: entryKey(d.entry), label: d.label, detail: d.detail, keywords: d.keywords }));
}
/** What the empty prefix of the palette asks for, in this window. */
export function placesPlaceholder(): string {
  return WINDOW === "chat" ? "room or person" : WINDOW === "tasks" ? "view or tag" : "folder, view or room";
}
/**
 * Open a place `placeItems` offered — unfolding whatever it sits in, so the
 * cursor lands on it. False when it is gone.
 */
export async function openPlace(key: string): Promise<boolean> {
  const destination = destinations().find((d) => entryKey(d.entry) === key);
  if (!destination) return false;
  const index = destination.reveal();
  if (index < 0) return false;
  await openEntry(index);
  return true;
}
/**
 * The keys that open each pin, drawn over its icon while they are the next
 * thing to type: `1` beside the first pin while Ctrl is held, or after `g`
 * when the config binds `g1`. Empty otherwise.
 */
let pinKeys = new Map<number, string>();
/** Which pin each binding in `table` opens, and what is left to type for it — or null to skip the binding. */
function pinKeysIn(table: Record<string, string>, rest: (binding: string) => string | null): Map<number, string> {
  const keys = new Map<number, string>();
  for (const [binding, command] of Object.entries(table)) {
    const n = /^open-pinned\s+(\d+)$/.exec(command.trim())?.[1];
    const typed = n ? rest(binding) : null;
    if (typed) keys.set(Number(n), typed);
  }
  return keys;
}
/** A prefix is pending (`sequence`), or none is (`""`): show which key opens which pin. */
export function showPinKeys(sequence: string, table: Record<string, string>): void {
  drawPinKeys(
    sequence
      ? pinKeysIn(table, (binding) =>
          binding.length > sequence.length && binding.startsWith(sequence)
            ? keysOf(binding.slice(sequence.length)).map(spellKey).join("")
            : null,
        )
      : new Map(),
  );
}
/** Ctrl is held on its own (`held`), or no longer is: show the key that opens each pin with it. */
export function showHeldPinKeys(held: boolean, table: Record<string, string>): void {
  drawPinKeys(held ? pinKeysIn(table, (binding) => /^<c-(.+)>$/.exec(binding)?.[1] ?? null) : new Map());
}
function drawPinKeys(keys: Map<number, string>): void {
  if (keys.size === 0 && pinKeys.size === 0) return;
  pinKeys = keys;
  el.folders.classList.toggle("pin-keys", keys.size > 0);
  for (const row of el.folders.querySelectorAll<HTMLElement>("[data-pin]")) {
    const key = row.querySelector(".sb-key");
    const typed = pinKeys.get(Number(row.dataset.pin)) ?? "";
    if (key) key.textContent = typed;
    // An attribute rather than `:empty` in the CSS, which WebKit does not
    // recheck when the text changes.
    if (typed) row.dataset.key = typed;
    else delete row.dataset.key;
  }
}
/** The reading views this configuration has: none without a reading folder, and no Archive before it exists. */
function readingViews(): typeof READING_VIEWS {
  if (WINDOW === "tasks" || !readingFolder()) return [];
  return READING_VIEWS.filter(({ view }) => view !== "archive" || readingFolder(true));
}
/** Where a reading view sits in the sidebar, for putting the cursor on it. */
export function readingEntry(view: ReadingView): number {
  if (!pinned({ kind: "reading", view })) unfoldSection("reading");
  return Math.max(0, entryIndex({ kind: "reading", view }));
}
/** A heading's place in the folding, beside the accounts and folders. */
const sectionKey = (section: SectionName): string => `\u0001${section}`;
function sectionFolded(section: SectionName): boolean {
  return collapsed().has(sectionKey(section));
}
/** Unfold a heading, so an entry under it can take the cursor. */
function unfoldSection(section: SectionName): void {
  if (!sectionFolded(section)) return;
  collapsed().delete(sectionKey(section));
  saveCollapsed();
}
/** Where an entry is in the sidebar just now, or -1. */
function entryIndex(entry: SidebarEntry): number {
  const key = entryKey(entry);
  return sidebarEntries().findIndex((e) => entryKey(e) === key);
}
/** A heading as the sidebar draws it: its fold, its label, the cursor on it. */
function sectionHeading(section: SectionName, cursor: number): string {
  const index = entryIndex({ kind: "section", section });
  const shut = sectionFolded(section);
  const on = index === cursor ? " cursor" : "";
  return `<div class="sb-head sb-section${shut ? " collapsed" : ""}${on}" role="option" aria-selected="${
    on ? "true" : "false"
  }" aria-expanded="${shut ? "false" : "true"}" data-entry="${index}" data-fold="${index}">${svgIcon(
    "chev",
    "sb-chev",
  )}<span class="sb-label lbl">${SECTION_LABELS[section]}</span>${section === "tasks" ? taskSyncMark() : ""}</div>`;
}
/**
 * Whether the task changes are on the server, beside the Tasks heading
 * (todo-spec §23): a warning while they cannot get there, the number still on
 * its way, or "saved" once nothing is — so that nothing pending is something
 * the window says rather than something to infer.
 */
function taskSyncMark(): string {
  const status = state.tasks.sync;
  if (!status) return "";
  if (status.problem) {
    const what = status.pending > 0 ? `${status.pending} change${status.pending === 1 ? "" : "s"} on this machine only` : "not syncing";
    return `<span class="sb-n"><span class="sb-badge tk-unsaved problem" title="${escape(`${what}: ${status.problem}`)}">${svgIcon("alert")}${status.pending > 0 ? status.pending : ""}</span></span>`;
  }
  if (status.pending > 0) {
    return `<span class="sb-n"><span class="sb-badge staged tk-unsaved" title="${status.pending} change${status.pending === 1 ? "" : "s"} not yet on the server">${status.pending}</span></span>`;
  }
  return `<span class="sb-n"><span class="tk-saved" title="Every task change is on the server">saved</span></span>`;
}
/**
 * The sidebar entry of the folder at `index` of `selectableFolders()`, for
 * putting the cursor on it: whatever it is folded inside opens, so the
 * cursor is never on a row that is not drawn.
 */
export function folderEntry(index: number): number {
  if (pinned({ kind: "folder", index })) return entryIndex({ kind: "folder", index });
  const folder = selectableFolders()[index];
  const reading = folder && !folder.is_pinned ? readingViewOf(folder) : null;
  if (reading) return readingEntry(reading);
  if (folder && folded(folder)) {
    collapsed().delete(foldKey(folder.account_name));
    for (const ancestor of ancestors(folder)) collapsed().delete(foldKey(folder.account_name, ancestor));
    saveCollapsed();
  }
  const at = sidebarEntries().findIndex((e) => e.kind === "folder" && e.index === index);
  return Math.max(0, at);
}

// ---- Folding ---------------------------------------------------------------

/** An account, or a folder in it, as the folding remembers it. */
const foldKey = (account: string, folder: string | null = null): string =>
  `${account}\u0000${folder ?? ""}`;
const COLLAPSED = ".sidebar.collapsed";
let collapsedSet: Set<string> | null = null;
/** What is folded away, remembered on this machine rather than in the config. */
function collapsed(): Set<string> {
  if (!collapsedSet) {
    try {
      collapsedSet = new Set(JSON.parse(readSetting(COLLAPSED) ?? "[]") as string[]);
    } catch {
      collapsedSet = new Set();
    }
  }
  return collapsedSet;
}
function saveCollapsed(): void {
  writeSetting(COLLAPSED, JSON.stringify([...collapsed()]));
}
/** The folders of the account sections that have folders inside them. */
function parentKeys(): Set<string> {
  const keys = new Set<string>();
  for (const f of state.folders) {
    if (!f.is_pinned && f.parent) keys.add(foldKey(f.account_name, f.parent));
  }
  return keys;
}
/** The folders a folder sits inside, nearest first. */
function ancestors(folder: FolderInfo): string[] {
  const out: string[] = [];
  if (folder.is_pinned) return out;
  let parent = folder.parent ?? null;
  while (parent && !out.includes(parent)) {
    out.push(parent);
    const name = parent;
    parent =
      state.folders.find(
        (f) => !f.is_pinned && f.account_name === folder.account_name && f.folder_name === name,
      )?.parent ?? null;
  }
  return out;
}
/** Whether a folder is folded away, under its account or one of its parents. */
function folded(folder: FolderInfo): boolean {
  if (folder.is_pinned) return false;
  const set = collapsed();
  if (set.has(foldKey(folder.account_name))) return true;
  return ancestors(folder).some((a) => set.has(foldKey(folder.account_name, a)));
}
const entryKey = (e: SidebarEntry | undefined): string =>
  !e
    ? ""
    : e.kind === "task"
      ? `t:${e.view}`
      : e.kind === "account"
        ? `a:${e.account}`
        : e.kind === "tag"
          ? `tag:${e.name}`
          : e.kind === "chat"
            ? `c:${e.view}`
            : e.kind === "chat-group"
              ? `cg:${e.group}`
              : e.kind === "chat-room"
                ? `cr:${e.rid}`
          : e.kind === "reading"
            ? `r:${e.view}`
          : e.kind === "section"
            ? `s:${e.section}`
            : e.kind === "outbox"
              ? "outbox"
              : `${e.kind}:${e.index}`;
/**
 * Fold or unfold an entry: an account, or a folder with folders inside it.
 * On a folder without any, its parent folds — or its account, at the top —
 * and the cursor moves there, as `za` on a line inside a fold does in Vim.
 *
 * The cursor stays on the entry it was on, wherever that now is; on one that
 * folded away, it moves to what folded.
 */
export function toggleFold(at = state.sidebarCursor, moveCursor = true): boolean {
  const entries = sidebarEntries();
  const entry = entries[at];
  let key: string | null = null;
  let target: SidebarEntry | undefined;
  if (entry?.kind === "chat-group") {
    // Folded until opened, so the key the set holds is the one that is open.
    key = groupKey(entry.group);
    target = entry;
  } else if (entry?.kind === "account") {
    key = foldKey(entry.account);
    target = entry;
  } else if (entry?.kind === "section") {
    key = sectionKey(entry.section);
    target = entry;
  } else if (entry?.kind === "folder") {
    const folder = selectableFolders()[entry.index]!;
    if (folder.is_pinned || pinned(entry)) return false;
    if (parentKeys().has(foldKey(folder.account_name, folder.folder_name))) {
      key = foldKey(folder.account_name, folder.folder_name);
      target = entry;
    } else if (folder.parent) {
      key = foldKey(folder.account_name, folder.parent);
      const parentIndex = selectableFolders().findIndex(
        (f) => !f.is_pinned && f.account_name === folder.account_name && f.folder_name === folder.parent,
      );
      target = { kind: "folder", index: parentIndex };
    } else {
      key = foldKey(folder.account_name);
      target = { kind: "account", account: folder.account_name };
    }
  }
  if (!key || !target) return false;
  const cursorKey = entryKey(entries[state.sidebarCursor]);
  const set = collapsed();
  if (set.has(key)) set.delete(key);
  else set.add(key);
  saveCollapsed();
  const after = sidebarEntries();
  const keep = moveCursor ? -1 : after.findIndex((e) => entryKey(e) === cursorKey);
  state.sidebarCursor = keep >= 0 ? keep : Math.max(0, after.findIndex((e) => entryKey(e) === entryKey(target)));
  renderFolders();
  return true;
}
/** Open a view of the sidebar: run its query, shown under its name. */
export async function openVirtualFolder(index: number): Promise<void> {
  const view = state.virtualFolders[index];
  if (!view) return;
  if (!pinned({ kind: "virtual", index })) unfoldSection("views");
  state.sidebarCursor = entryIndex({ kind: "virtual", index });
  await runSearchQuery(view.query, view.name, index);
}
/**
 * A mail search replaces whatever the list shows, a task view included:
 * the hits are mail, and a task view's own search box filters tasks.
 */
export function leaveTaskViewForSearch(): void {
  if (!inTaskView()) return;
  state.tasks.view = null;
  state.tasks.detail = null;
  syncSearchBox();
}
export function currentEntry(): SidebarEntry | undefined {
  return sidebarEntries()[state.sidebarCursor];
}
/** Where a task view sits in the sidebar, for putting the cursor on it. */
export function taskEntryIndex(view: TaskViewName): number {
  const entry: SidebarEntry = { kind: "task", view };
  if (WINDOW === "tasks" ? taskViews().find((v) => v.view === view)?.inSection : !pinned(entry)) unfoldSection("tasks");
  return entryIndex(entry);
}
/** Where Waiting sits in the sidebar, for putting the cursor on it. */
export function chatEntryIndex(): number {
  const entry: SidebarEntry = { kind: "chat", view: "waiting" };
  if (WINDOW === "chat" || !pinned(entry)) unfoldSection("chat");
  return entryIndex(entry);
}
/** Where a room sits in the sidebar, its group opened to show it unless it is pinned. */
export function chatRoomEntryIndex(rid: string): number {
  const entry: SidebarEntry = { kind: "chat-room", rid };
  if (pinned(entry)) return entryIndex(entry);
  unfoldSection("chat");
  const room = state.chat.rooms.find((r) => r.rid === rid);
  if (room) {
    const key = groupKey(room.kind === "d" ? "direct" : "channels");
    if (!collapsed().has(key)) {
      collapsed().add(key);
      saveCollapsed();
    }
  }
  return entryIndex({ kind: "chat-room", rid });
}
const groupKey = (group: ChatGroup): string => `\u0003open:${group}`;
const groupOpen = (group: ChatGroup): boolean => collapsed().has(groupKey(group));
const inGroup = (group: ChatGroup) => (kind: string): boolean => (group === "direct") === (kind === "d");
/** A group's rooms, less those pinned. */
function groupRooms(group: ChatGroup): ChatRoom[] {
  const pinnedRooms = new Set(pinEntries().flatMap((e) => (e.kind === "chat-room" ? [e.rid] : [])));
  return state.chat.rooms.filter((r) => inGroup(group)(r.kind) && !pinnedRooms.has(r.rid));
}
/** The chat group's entries: Waiting, then Direct and Channels with their rooms when open. */
function chatEntries(): SidebarEntry[] {
  const entries: SidebarEntry[] = [{ kind: "chat", view: "waiting" }];
  for (const group of ["direct", "channels"] as const) {
    const rooms = groupRooms(group);
    if (rooms.length === 0) continue;
    entries.push({ kind: "chat-group", group });
    if (groupOpen(group)) entries.push(...rooms.map((r) => ({ kind: "chat-room" as const, rid: r.rid })));
  }
  return entries;
}
/** Whether a task view is open, as opposed to a mail folder. */
export function inTaskView(): boolean {
  return state.tasks.view !== null;
}
/**
 * Whether the tasks are the half of the view being walked.
 *
 * Today has a mail list in it, and while the cursor is down there the tasks are
 * not what a command means — pressing `x` must not complete a task that is not
 * under the cursor (§4.2). Every other view has only tasks.
 */
export function taskSectionActive(): boolean {
  if (!inTaskView()) return false;
  return !hasMailHalf() || state.tasks.section === "tasks";
}
/**
 * Whether the task view on screen has mail beside its tasks: Today in the
 * mailbox window (§4.1), and a search that found both and shows both (§26).
 */
export function hasMailHalf(): boolean {
  const view = state.tasks.view;
  if (view === "today") return todayHasMail();
  if (view === "search") return !!state.search && state.search.kind === "all" && state.search.hits.length > 0;
  return false;
}
/**
 * …and the keyboard is in it rather than in the sidebar.
 *
 * The distinction matters for the bindings alone: walking the sidebar must not
 * turn `d` into delete-task, but the task cursor is still drawn and the detail
 * pane still follows it while the folder list has focus.
 */
export function inTaskSection(): boolean {
  return state.focus !== "folders" && taskSectionActive();
}
/** Where the sidebar's cursor is drawn: nowhere while a search is showing. */
const drawnCursor = (): number => (state.search ? -1 : state.sidebarCursor);
/**
 * A row of the sidebar. `pin` is its number among the pins, which carries the
 * key that opens it while that key is the next to type.
 */
function sbRow(
  index: number,
  on: boolean,
  glyph: string,
  label: string,
  extra: string,
  { classes = "", pin = 0, depth = 0, attrs = "" }: { classes?: string; pin?: number; depth?: number; attrs?: string } = {},
): string {
  const typed = pin ? (pinKeys.get(pin) ?? "") : "";
  const key = pin ? `<kbd class="sb-key" aria-hidden="true">${escape(typed)}</kbd>` : "";
  const pinned = pin ? ` data-pin="${pin}"${typed ? ` data-key="${escape(typed)}"` : ""}` : "";
  return `<div class="sb-row${classes}${on ? " cursor" : ""}" role="option" aria-selected="${on ? "true" : "false"}" data-entry="${index}"${pinned}${depth ? ` style="--d:${depth}"` : ""}${attrs}>${key}${glyph}${label}${extra}</div>`;
}
/**
 * A task view's row (§4.1). Only Today carries a badge: a count beside
 * Someday would be a running total of everything not being done, which is a
 * number nobody acts on.
 */
function taskRow(view: TaskViewName, pin = 0): string {
  const info = TASK_VIEWS.find((v) => v.view === view)!;
  const index = entryIndex({ kind: "task", view });
  const badge =
    view === "today" && state.tasks.badge > 0
      ? `<span class="sb-n"><span class="sb-badge unread">${state.tasks.badge}</span></span>`
      : "";
  const working =
    view === "today" && state.tasks.workingOn.length
      ? `<span class="sb-working" title="Working on ${escape(state.tasks.workingOn.map((t) => t.title).join(", "))}"></span>`
      : "";
  return sbRow(
    index,
    index === drawnCursor(),
    svgIcon(info.icon, "sb-glyph"),
    `<span class="sb-label lbl">${escape(info.label)}</span>`,
    working + badge,
    { classes: view === "today" ? " today" : "", pin },
  );
}
/**
 * The task views (§4.1). In the tasks window Today stands alone above the
 * other four, which sit under a Tasks heading; in the mailbox Today is a pin
 * unless `pinned` leaves it out, and the views not pinned are under the
 * heading. Either way there is no second Today — one view, one door.
 */
function renderTaskEntries(): string {
  const views = taskViews();
  // Without a tasks account there are no task views, and no heading for them.
  if (views.length === 0) return "";
  const pins = pinEntries();
  const alone = WINDOW === "tasks" ? views.filter((v) => !v.inSection) : [];
  const section =
    WINDOW === "tasks"
      ? views.filter((v) => v.inSection)
      : views.filter((v) => !pins.some((e) => e.kind === "task" && e.view === v.view));
  return (
    alone.map((v) => taskRow(v.view)).join("") +
    (section.length ? sectionHeading("tasks", drawnCursor()) : "") +
    (sectionFolded("tasks") ? "" : section.map((v) => taskRow(v.view)).join(""))
  );
}
/**
 * The pins at the top of the mailbox's sidebar, in the order `pinned` gives,
 * under no heading: what is at the top of the sidebar is the top of the day,
 * and every heading below claims only what is under it.
 */
function renderPins(rows: { folder: (f: FolderInfo, index: number, pin: number) => string }): string {
  return pinEntries()
    .map((entry, i) => {
      const pin = i + 1;
      switch (entry.kind) {
        case "task":
          return taskRow(entry.view, pin);
        case "reading":
          return readingRow(entry.view, pin);
        case "virtual":
          return viewRow(entry.index, pin);
        case "chat":
          return waitingRow(pin);
        case "chat-room": {
          const room = state.chat.rooms.find((r) => r.rid === entry.rid);
          return room ? roomRow(room, pin) : "";
        }
        case "folder":
          return rows.folder(selectableFolders()[entry.index]!, entryIndex(entry), pin);
        default:
          return "";
      }
    })
    .join("");
}
/** The tasks window's sidebar: the task views, then the tags. */
function renderTasksWindowSidebar(): void {
  el.folders.className = `pane${state.focus === "folders" ? " focused" : ""}`;
  if (taskViews().length === 0) {
    el.folders.innerHTML = `<div class="sb-head" role="presentation"><span class="lbl">No tasks account</span></div>`;
    renderSteady();
    return;
  }
  const tags = sidebarTags();
  const rows = sectionFolded("tags") ? "" : tags
    .map((name) => {
      const index = entryIndex({ kind: "tag", name });
      const on = index === state.sidebarCursor || state.tasks.search === tagFilter(name) ? " cursor" : "";
      return `<div class="sb-row${on}" role="option" aria-selected="${on ? "true" : "false"}" data-entry="${index}">${svgIcon(
        "tag",
        "sb-glyph",
      )}<span class="sb-label">#${escape(name)}</span></div>`;
    })
    .join("");
  el.folders.innerHTML =
    renderTaskEntries() + (tags.length ? `${sectionHeading("tags", state.sidebarCursor)}${rows}` : "");
  renderSteady();
}
export function renderFolders(): void {
  if (WINDOW === "tasks") {
    renderTasksWindowSidebar();
    return;
  }
  if (WINDOW === "chat") {
    el.folders.className = `pane${state.focus === "folders" ? " focused" : ""}`;
    el.folders.innerHTML = renderChatEntries();
    renderSteady();
    return;
  }
  const selectable = selectableFolders();
  const entry = currentEntry();
  const active = entry?.kind === "folder" ? selectable[entry.index] : undefined;
  el.folders.className = `pane${state.focus === "folders" ? " focused" : ""}${pinKeys.size ? " pin-keys" : ""}`;

  // A search spans every folder, so while one is showing, no folder is the one
  // being looked at. It gets a row of its own — which is what makes the list
  // above it explicable, and gives the search somewhere to be dismissed from
  // rather than only a keystroke.
  // No heading. A heading claims everything under it until the next one, and
  // what follows here is the pins, which have no heading of their own and are
  // not part of the search — clicking one leaves it. The row is told apart by
  // its own look instead: the glass, the query and the rule below it.
  // A view is its own entry, highlighted where it stands; only a typed
  // search gets the row of its own.
  // A conversation reads as one, by its subject, rather than as the query
  // that found it.
  const conversation = state.search?.conversation;
  const searchRow = state.search && state.search.view === undefined
    ? `<div class="sb-row cursor search-row${conversation === undefined ? "" : " conversation-row"}" role="option" aria-selected="true" title="${escape(
        conversation === undefined ? state.search.query : `Conversation: ${conversation}`,
      )}">
         ${svgIcon(conversation === undefined ? "search" : "mail", "sb-glyph")}
         <span class="sb-label">${escape(conversation ?? state.search.query)}</span>
         <button type="button" data-close-search title="${conversation === undefined ? "Leave search" : "Leave the conversation"}">
           ${icon("fa-xmark", "fa-fw")}
         </button>
       </div>`
    : "";

  const entries = sidebarEntries();
  const entryOf = new Map<string, number>(entries.map((e, i) => [entryKey(e), i]));
  const parents = parentKeys();
  const cursor = drawnCursor();
  const set = collapsed();
  let position = -1;

  const folderRow = (f: FolderInfo, index: number, pin = 0): string => {
    // Nothing in the sidebar is current while a search is.
    const on = f === active && !state.search;
    const staged = state.pendingCounts.get(`${f.account_name}\u0000${f.folder_name}`) ?? 0;
    const highlighted = (state.highlightBadges.get(`${f.account_name}\u0000${f.folder_name}`) ?? [])
      .map((b) => {
        const style = state.highlightStyles.get(b.rule);
        const color = style?.color ?? style?.icon_color ?? style?.label_color ?? "var(--accent)";
        return `<span class="sb-badge hl"${colourStyle("color", color)} title="${escape(b.rule)}: ${b.count} unread">${b.count}</span>`;
      })
      .join("");
    const counts =
      (staged > 0
        ? `<span class="sb-badge staged" title="${staged} staged action${staged === 1 ? "" : "s"}">${staged}</span>`
        : "") +
      highlighted +
      (f.unread_count > 0 ? `<span class="sb-badge unread">${f.unread_count}</span>` : "");
    // Among the pins a folder stands alone: what is inside it stays in its account.
    const parent = !pin && !f.is_pinned && parents.has(foldKey(f.account_name, f.folder_name));
    const open = parent && !set.has(foldKey(f.account_name, f.folder_name));
    const fold = parent
      ? `<span class="sb-fold${open ? " open" : ""}" data-fold="${index}" title="${open ? "Fold" : "Unfold"}">${svgIcon("chev")}</span>`
      : "";
    const label = pin ? pinnedFolderLabel(f) : (f.label ?? f.display_name);
    const expanded = parent ? ` aria-expanded="${open ? "true" : "false"}"` : "";
    return sbRow(
      index,
      on,
      svgIcon(roleIcon(f.role), "sb-glyph"),
      `<span class="sb-label">${escape(label)}</span>`,
      (counts ? `<span class="sb-n">${counts}</span>` : "") + fold,
      { pin, depth: pin ? 0 : (f.depth ?? 0), attrs: `${expanded} title="${escape(f.display_name)}"` },
    );
  };
  const pinnedFolders = folderPins(pinEntries());

  el.folders.innerHTML =
    searchRow +
    renderPins({ folder: folderRow }) +
    outboxRow() +
    renderTaskEntries() +
    renderReadingEntries() +
    renderViewEntries() +
    state.folders
    .map((f) => {
      if (f.is_account_header) {
        const index = entryOf.get(`a:${f.account_name}`) ?? -1;
        const shut = set.has(foldKey(f.account_name));
        const on = index === cursor ? " cursor" : "";
        // A folded account still says how much is unread in it.
        const unread = shut
          ? state.folders
              .filter((g) => !g.is_pinned && !g.is_account_header && g.account_name === f.account_name && !isReadingFolder(g))
              .reduce((sum, g) => sum + g.unread_count, 0)
          : 0;
        return `<div class="sb-head sb-account${shut ? " collapsed" : ""}${on}" role="option" aria-selected="${on ? "true" : "false"}" aria-expanded="${shut ? "false" : "true"}" data-entry="${index}" data-fold="${index}">${svgIcon("chev", "sb-chev")}<span class="sb-label">${escape(f.display_name)}</span>${healthGlyph(f.display_name)}${
          unread > 0 ? `<span class="sb-n"><span class="sb-badge unread">${unread}</span></span>` : ""
        }</div>`;
      }
      if (f.folder_name === null) return "";
      position++;
      // A pinned folder is drawn among the pins: `pinned`'s by a record of its own.
      if (f.is_pinned || pinnedFolders.has(position)) return "";
      const index = entryOf.get(`folder:${position}`);
      return index === undefined ? "" : folderRow(f, index);
    })
    .join("") +
    renderChatEntries();
  renderSteady();
}
/**
 * The Outbox, while it holds anything: in red with the number refused, which
 * wait for the user; otherwise in amber with the number waiting for a server.
 */
function outboxRow(): string {
  if (state.outbox.length === 0) return "";
  const index = entryIndex({ kind: "outbox" });
  const failed = state.outbox.filter((q) => q.failed_at !== null).length;
  const waiting = state.outbox.length - failed;
  const badge = failed > 0
    ? `<span class="sb-badge outbox-failed" title="${failed} message${failed === 1 ? "" : "s"} not sent">${svgIcon("alert")}${failed}</span>`
    : `<span class="sb-badge staged" title="${waiting} message${waiting === 1 ? "" : "s"} waiting for the mail server">${waiting}</span>`;
  return sbRow(
    index,
    index === drawnCursor(),
    svgIcon("outbox", "sb-glyph"),
    `<span class="sb-label">Outbox</span>`,
    `<span class="sb-n">${badge}</span>`,
    { classes: failed > 0 ? " outbox-row failed" : " outbox-row" },
  );
}
const chatBadge = (n: number, title: string, quiet = false) =>
  n > 0 ? `<span class="sb-n"><span class="sb-badge ${quiet ? "quiet" : "unread"}" title="${escape(title)}">${n}</span></span>` : "";
/** Chat's Waiting, with how many threads wait, or a warning while signed out. */
function waitingRow(pin = 0): string {
  const index = entryIndex({ kind: "chat", view: "waiting" });
  const waiting = state.chat.waiting.length;
  return sbRow(
    index,
    index === drawnCursor(),
    svgIcon("chat", "sb-glyph"),
    `<span class="sb-label lbl">Waiting</span>`,
    state.chat.status?.state === "signed_out"
      ? `<span class="sb-n"><span class="sb-badge staged" title="Signed out of chat">!</span></span>`
      : chatBadge(waiting, `${waiting} thread${waiting === 1 ? "" : "s"} waiting`),
    { pin },
  );
}
/** Chat's group at the foot of the sidebar: Waiting unless it is pinned, then Direct and Channels. */
function renderChatEntries(): string {
  if (!state.chat.available) return "";
  const cursorIndex = drawnCursor();
  const rest = unpinned(chatEntries(), pinEntries());
  if (rest.length === 0) return "";
  // The chat window's sidebar is the group alone, never folded.
  const heading = WINDOW === "chat" ? "" : sectionHeading("chat", cursorIndex);
  if (WINDOW !== "chat" && sectionFolded("chat")) return heading;
  let html = heading + (rest[0]?.kind === "chat" ? waitingRow() : "");
  for (const group of ["direct", "channels"] as const) {
    const rooms = groupRooms(group);
    if (rooms.length === 0) continue;
    const open = groupOpen(group);
    const index = entryIndex({ kind: "chat-group", group });
    const on = index === cursorIndex ? " cursor" : "";
    const n = waitingIn(rooms.map((r) => r.rid));
    html += `<div class="sb-row${on}" role="option" aria-selected="${on ? "true" : "false"}" aria-expanded="${open}" data-entry="${index}">${svgIcon(
      group === "direct" ? "user" : "hash",
      "sb-glyph",
    )}<span class="sb-label lbl">${group === "direct" ? "Direct" : "Channels"}</span>${chatBadge(n, `${n} waiting`)}<span class="sb-fold${open ? " open" : ""}" data-fold="${index}" title="${open ? "Fold" : "Unfold"}">${svgIcon("chev")}</span></div>`;
    if (open) html += rooms.map((room) => roomRow(room)).join("");
  }
  return html;
}
const waitingIn = (rids: string[]): number => state.chat.waiting.filter((w) => rids.includes(w.rid)).length;
const roomLabel = (room: ChatRoom): string =>
  room.kind === "d" ? (room.fname ?? room.name ?? room.label) : room.label.replace(/^#/, "");
/** A room's row, in its group or among the pins. */
function roomRow(room: ChatRoom, pin = 0): string {
  const index = entryIndex({ kind: "chat-room", rid: room.rid });
  const showing = state.chat.view === "room" && state.chat.room === room.rid && !state.search;
  // A direct message with one person shows how they stand instead of an icon.
  const status = room.presence ?? "offline";
  const glyph =
    room.kind === "d" && room.label === "Direct"
      ? `<span class="sb-presence ${escape(status)}" title="${escape(status)}"></span>`
      : svgIcon(room.kind === "d" ? "user" : "hash", "sb-glyph");
  // A muted room still counts what waits in it, in grey, as long as none of it names you.
  const loud = waitingIn([room.rid]);
  const quiet = state.chat.muted.filter((w) => w.rid === room.rid).length;
  const muted = room.muted
    ? `<span class="sb-muted" title="${escape(`Muted ${muteSpan(room)}`)}">${svgIcon("mute")}</span>`
    : "";
  return sbRow(
    index,
    showing || index === drawnCursor(),
    glyph,
    `<span class="sb-label">${escape(roomLabel(room))}</span>${muted}`,
    chatBadge(loud + quiet, room.muted ? `${loud + quiet} waiting, muted ${muteSpan(room)}` : "waiting", loud === 0),
    { pin, depth: pin ? 0 : 1, classes: room.muted ? " sb-room-muted" : "" },
  );
}
/** A reading view's row; the reading list says how much is unread in it. */
function readingRow(view: ReadingView, pin = 0): string {
  const { label, icon } = READING_VIEWS.find((v) => v.view === view)!;
  const index = entryIndex({ kind: "reading", view });
  const unread = readingFolder()?.unread_count ?? 0;
  const badge = view === "list" && unread > 0 ? `<span class="sb-n"><span class="sb-badge unread">${unread}</span></span>` : "";
  const showing = state.search ? state.search.view === view : index === drawnCursor();
  return sbRow(index, showing, svgIcon(icon, "sb-glyph"), `<span class="sb-label">${label}</span>`, badge, { pin });
}
/** The reading views not pinned, under a heading of their own, after the task views. */
function renderReadingEntries(): string {
  const pins = pinEntries();
  const views = readingViews().filter(({ view }) => !pins.some((e) => e.kind === "reading" && e.view === view));
  if (views.length === 0) return "";
  const rows = sectionFolded("reading") ? "" : views.map(({ view }) => readingRow(view)).join("");
  return `${sectionHeading("reading", drawnCursor())}${rows}`;
}
/** A view's row, under its own icon: Font Awesome, an emoji, or the search glass. */
function viewRow(i: number, pin = 0): string {
  const view = state.virtualFolders[i]!;
  const index = entryIndex({ kind: "virtual", index: i });
  const showing = state.search ? state.search.view === i : index === drawnCursor();
  const named = view.icon?.trim() ?? "";
  const bare = named.replace(/^fa-/, "");
  const glyph = !named
    ? svgIcon("search", "sb-glyph")
    : /^[a-z0-9-]+$/.test(bare)
      ? configIcon(`fa-${bare}`, null, "fa-fw sb-glyph")
      : `<span class="sb-glyph emoji-glyph">${escape(named)}</span>`;
  return sbRow(index, showing, glyph, `<span class="sb-label">${escape(view.name)}</span>`, "", {
    pin,
    attrs: ` title="${escape(view.query)}"`,
  });
}
/** The views not pinned, under a heading of their own, after the reading views. */
function renderViewEntries(): string {
  const pins = pinEntries();
  const views = state.virtualFolders
    .map((_, i) => i)
    .filter((i) => !pins.some((e) => e.kind === "virtual" && e.index === i));
  if (views.length === 0) return "";
  const rows = sectionFolded("views") ? "" : views.map((i) => viewRow(i)).join("");
  return `${sectionHeading("views", drawnCursor())}${rows}`;
}
/**
 * The mark beside an account's name when its server is not simply fine.
 *
 * Nothing for ok and for not-yet-synced; a turning arrow while a sync runs;
 * and a glyph that says which of the three things a person can fix went wrong
 * — the password, the certificate, the network — with the server's words in
 * the tooltip. `account-status` shows the same as a panel.
 */
export function healthGlyph(account: string): string {
  const health = state.health.find((h) => h.account === account);
  if (!health) return "";
  if (health.state === "ok" || health.state === "unknown") {
    return health.tasks
      ? ` <i class="fa-solid fa-triangle-exclamation health error" title="${escape(`tasks not syncing: ${health.tasks}`)}"></i>`
      : "";
  }
  const glyph = {
    syncing: "fa-rotate spin",
    auth_failed: "fa-key",
    certificate: "fa-shield-halved",
    unreachable: "fa-plug-circle-xmark",
    error: "fa-triangle-exclamation",
  }[health.state];
  const title = health.state === "syncing" ? "syncing" : describeHealth(health);
  return ` <i class="fa-solid ${glyph} health ${health.state}" title="${escape(title)}"></i>`;
}
/** An account's standing as one line: what, where, since when. */
export function describeHealth(health: AccountHealth): string {
  const what = {
    unknown: "not synced yet",
    syncing: "syncing",
    ok: "ok",
    auth_failed: "login refused",
    certificate: "certificate problem",
    unreachable: "unreachable",
    error: "error",
  }[health.state];
  const where = health.folder ? ` in ${health.folder}` : "";
  const since = health.at ? ` since ${health.at.slice(0, 16).replace("T", " ")}` : "";
  const detail = health.message ? `: ${health.message}` : "";
  const tasks = health.tasks ? ` · tasks not syncing: ${health.tasks}` : "";
  return `${what}${where}${since}${detail}${tasks}`;
}
/** Re-read the accounts' standing and redraw the sidebar with it. */
export async function refreshHealth(): Promise<void> {
  state.health = (await call<AccountHealth[]>("account_statuses")) ?? [];
  renderFolders();
  if (openPanel()?.title === "Accounts") showAccountStatus();
}
/**
 * Re-read the sidebar: the folders with their counts, and what is staged in
 * each. One place, so nothing that changes the counts can forget the badges.
 */
export async function refreshFolders(): Promise<void> {
  state.folders = await call<FolderInfo[]>("folders");
  await refreshPendingCounts();
  await refreshHighlightBadges();
  renderFolders();
}
/** Re-read the badged highlight rules' unread counts, for the sidebar. */
export async function refreshHighlightBadges(): Promise<void> {
  const badges =
    (await call<HighlightBadge[]>("highlight_badges").catch(() => [] as HighlightBadge[])) ?? [];
  state.highlightBadges = new Map();
  for (const b of badges) {
    const key = `${b.account}\u0000${b.folder}`;
    state.highlightBadges.set(key, [...(state.highlightBadges.get(key) ?? []), b]);
  }
}
/**
 * Whether the bundled Font Awesome set has an icon of this name: a known one
 * sets its glyph in `--fa`, an unknown one leaves it unset. (The `::before`
 * content reads the same for both.)
 */
export function iconExists(name: string): boolean {
  const probe = document.createElement("i");
  probe.className = `fa-solid ${name}`;
  probe.style.position = "absolute";
  probe.style.visibility = "hidden";
  document.body.appendChild(probe);
  const glyph = getComputedStyle(probe).getPropertyValue("--fa");
  probe.remove();
  return glyph.trim() !== "";
}
/**
 * Read the highlight rules' styles, and check their icons against the
 * bundled set — the core cannot, and an unknown name would draw an empty box.
 */
export async function loadHighlightStyles(): Promise<void> {
  const styles =
    (await call<HighlightStyle[]>("highlight_styles").catch(() => [] as HighlightStyle[])) ?? [];
  state.highlightStyles = new Map(styles.map((s) => [s.name, s.style]));
  state.highlightWarnings = [];
  for (const { name, style } of styles) {
    if (style.icon && !iconExists(style.icon)) {
      state.highlightWarnings.push(`highlight '${name}': no icon called ${style.icon}`);
      style.icon = null;
    }
  }
}
/**
 * The look the rules a message matches add up to: the first rule with a
 * colour or a background sets it, and every rule contributes its icon and its
 * label, in order, up to three of each.
 */
export function highlightLook(names: string[]): {
  color: string | null;
  background: string | null;
  icons: { icon: string; color: string | null }[];
  labels: { label: string; color: string | null }[];
  bold: boolean;
  italic: boolean;
  dim: boolean;
} {
  const look = {
    color: null as string | null,
    background: null as string | null,
    icons: [] as { icon: string; color: string | null }[],
    labels: [] as { label: string; color: string | null }[],
    bold: false,
    italic: false,
    dim: false,
  };
  for (const name of names) {
    const style = state.highlightStyles.get(name);
    if (!style) continue;
    look.color ??= style.color;
    look.background ??= style.background;
    if (style.icon && look.icons.length < 3) {
      look.icons.push({ icon: style.icon, color: style.icon_color ?? style.color });
    }
    if (style.label && look.labels.length < 3) {
      look.labels.push({ label: style.label, color: style.label_color ?? style.color });
    }
    look.bold ||= style.bold;
    look.italic ||= style.italic;
    look.dim ||= style.dim;
  }
  return look;
}
/** The icons and labels of a highlighted message, as markup. */
export function highlightMarks(names: string[]): string {
  const look = highlightLook(names);
  const icons = look.icons
    .map((i) => configIcon(i.icon, i.color))
    .join("");
  const labels = look.labels
    .map((l) => `<span class="chip"${colourStyle("--chip-color", l.color)}>${escape(l.label)}</span>`)
    .join("");
  return icons + labels;
}
/** Re-read how much is staged where, for the sidebar's badges. */
export async function refreshPendingCounts(): Promise<void> {
  const counts =
    (await call<PendingFolderCount[]>("pending_counts").catch(() => [] as PendingFolderCount[])) ??
    [];
  state.pendingCounts = new Map(
    counts.map((c) => [`${c.account_name}\u0000${c.folder_name}`, c.count]),
  );
}
// Right-click on a chat room offers what can be done to the room itself.
el.folders.addEventListener("contextmenu", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-entry]");
  const entry = row ? sidebarEntries()[Number(row.dataset.entry)] : undefined;
  if (!row || entry?.kind !== "chat-room") return;
  e.preventDefault();
  state.sidebarCursor = Number(row.dataset.entry);
  state.focus = "folders";
  renderFolders();
  const room = state.chat.rooms.find((r) => r.rid === entry.rid);
  const pinned = pinEntries().some((p) => p.kind === "chat-room" && p.rid === entry.rid);
  void import("./menus").then(({ menuKeys }) =>
    call("open_chat_room_menu", {
      disabled: room?.muted ? [] : ["chat-unmute-room"],
      labels: {
        ...(room?.muted ? { "chat-unmute-room": `Unmute Room (muted ${muteSpan(room)})` } : {}),
        ...(pinned ? { "toggle-pin": "Unpin from Sidebar" } : {}),
      },
      ...menuKeys({ ...state.bindings.index_normal, ...state.bindings.chat_normal }),
    }),
  );
});
// Clicking a folder does exactly what Enter on it does.
el.folders.addEventListener("click", (e) => {
  if ((e.target as HTMLElement).closest("[data-close-search]")) {
    void runCommand("clear-search", 1);
    return;
  }
  const fold = (e.target as HTMLElement).closest<HTMLElement>("[data-fold]");
  if (fold) {
    toggleFold(Number(fold.dataset.fold), false);
    return;
  }
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-entry]");
  if (!row) return;
  state.sidebarCursor = Number(row.dataset.entry);
  state.focus = "folders";
  void openSelection();
});
