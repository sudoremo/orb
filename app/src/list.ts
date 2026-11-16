import { restoreScrolls, scrollsIn, type Scrolls } from "./scrollkeep";
import { WaitingThread, chatListMarkup, chatSectionActive, chatSelected, chatTodaySection, inChatView, roomAlone } from "./chat";
import { todayMixed } from "./layout";
import { isConversationRow } from "./conversation";
import { TimelineKind, placeTimeline, placedBetween, timelineAt, timelineKind, timelineTotal } from "./mixed";
import { anySelected, clearTimelineSelection, clickOnTimeline } from "./mixedselect";
import { TaskListView, distinctTasks, groupHeading, isPaged, renderTaskRows, emptyTaskMessage, taskViewLabel } from "./tasklist";
import { cancelMarkRead, currentFolder, folderTotal, followCursor, loadMessages, openMessage, openSelection, previewCursor, threadArgs } from "./actions";
import { viewingDrafts } from "./composing";
import { openItemWindow } from "./itemwindow";
import { ROW_HEIGHT, applyColumnClasses, applyDensity } from "./appearance";
import { call, setStatus } from "./bridge";
import { SPOOF_TITLE, configIcon, dateRoom, escape, icon, listDate, parseFrom, spoofedName, subjectIconsHtml } from "./format";
import { emptyView, listStandsAlone } from "./empty";
import { attachFilterBar, filterHasFocus } from "./filter";
import { svgIcon } from "./icons";
import { openMessageMenu } from "./menus";
import { applyMaximize } from "./maximize";
import { refreshMore } from "./more";
import { renderSteady } from "./statusline";
import { firstSection, shows, TodaySection } from "./today";
import { renderToolbar, threadsSwitch } from "./toolbar";
import { MessageHeader, SearchHit, SortKey, sortOrder } from "./model";
import { openReading, paintReadingRows, renderReadingSection, setTodayReadingShows, todayReading, todayReadingShows } from "./reading";
import { renderReader } from "./reader";
import { clearSelection, isSelected, reportSelection, selectRange } from "./selection";
import { hasMailHalf, highlightLook, iconExists, inTaskView, renderFolders, selectableFolders, taskSectionActive } from "./sidebar";
import { FETCH_DEBOUNCE, OVERSCAN, PAGE_PREFETCH, PAGE_RETENTION, PAGE_SIZE, ITEM, el, state } from "./state";
import { canGroupByTag, chooseTodayAccount, dueFirst, dueFirstOn, dueNowCount, groupedByTag, setDueFirst, inboxFolders, keyFor, setGroupByTag, toggleSearchCompleted, scheduleTaskCounts, todayAccount, todayColumns, todayInboxes } from "./taskview";

/**
 * Change the sort and re-query.
 *
 * The order is applied by SQL over the entire folder, so it cannot be honoured
 * by shuffling the rows already on screen — the page has to be re-fetched. The
 * cursor re-anchors to the same message when it survives into the new page.
 */
export async function sortBy(key: SortKey, dir?: 1 | -1): Promise<void> {
  // A threaded list is ordered by each thread's newest message and nothing
  // else; the header has no say until the list is messages again.
  if (state.threads && (!inTaskView() || state.inboxFolders) && !state.search) {
    setStatus("threads are ordered by their newest message · zt for a sortable list");
    return;
  }
  // A direction named is taken as it is. Otherwise re-picking the active
  // column flips it, and a new column starts descending for dates (newest
  // first) and ascending for text.
  if (dir !== undefined) {
    if (state.sort.key === key && state.sort.dir === dir) return;
    state.sort = { key, dir };
  } else if (state.sort.key === key) {
    state.sort.dir = state.sort.dir === 1 ? -1 : 1;
  } else {
    state.sort = { key, dir: key === "date" ? -1 : 1 };
  }
  // Today's mail half pages through the inboxes rather than one folder, and
  // `loadMessages` steps aside in a task view — so the pages are dropped and
  // read again in the new order, cursor where it was.
  if (inTaskView()) {
    reloadPages();
    // Turned over, the timeline places its threads and reading again.
    if (todayMixed()) {
      await placeTimeline();
      paintTimeline();
    } else renderRows();
    previewCursor();
    return;
  }
  // The cursor's row index means nothing under a new ordering, and finding the
  // message's new position would need an index-of query the store does not
  // expose, so a re-sort returns to the top.
  await loadMessages();
}
/** Drop every loaded page, so the rows are read again as they are now. */
export function reloadPages(): void {
  state.generation += 1;
  state.pages.clear();
  state.fetching.clear();
}
/**
 * The rows held in memory rather than paged: search hits, assembled once and
 * not scrolled through a query. Everything else — a folder, or Today's
 * inboxes — is paged.
 */
export function memoryRows(): SearchHit[] | null {
  return state.filter?.hits ?? state.search?.hits ?? null;
}
/** The message at a global row index, if its page is loaded. */
export function rowAt(index: number): MessageHeader | undefined {
  const rows = memoryRows();
  if (rows) return rows[index]?.message;
  return state.pages.get(Math.floor(index / PAGE_SIZE))?.[index % PAGE_SIZE];
}
/**
 * Where a row lives. Every row carries its own folder, so a merged list and a
 * folder answer the same way, and nothing has to ask what the sidebar has
 * open.
 */
export function rowLocation(index: number): { account: string; folder: string } | undefined {
  const m = rowAt(index);
  return m ? { account: m.account_name, folder: m.folder_name } : undefined;
}
/** Whether the keys act on Today's reading section rather than on its mail or tasks. */
export function readingActive(): boolean {
  return state.tasks.view === "today" && state.tasks.section === "reading" && todayReading();
}
/** The message under the cursor that is live: the reading section's, or the list's. */
export function cursorRow(): MessageHeader | undefined {
  return readingActive() ? state.reading.rows[state.reading.cursor]?.message : rowAt(state.messageCursor);
}
/** Where the message under the live cursor lives. */
export function cursorLocation(): { account: string; folder: string } | undefined {
  const m = cursorRow();
  return m ? { account: m.account_name, folder: m.folder_name } : undefined;
}
/** The message under the live cursor, its page read in if need be. */
export async function ensureCursorRow(): Promise<MessageHeader | undefined> {
  return readingActive() ? cursorRow() : ensureRow(state.messageCursor);
}
/**
 * What the paged list reads from: the open folder, or Today's inboxes as
 * one list. `null` when there is nothing to page — a search, or a task view
 * without mail.
 */
export function pageSource(): { command: string; args: Record<string, unknown> } | null {
  if (state.search || state.filter?.hits) return null;
  if (state.inboxFolders) {
    return {
      command: "inbox_messages",
      args: {
        folders: state.inboxFolders.map((f) => [f.account_name, f.folder_name]),
        // The timeline is a timeline: by date, either way up.
        sort: todayMixed() ? sortOrder({ key: "date", dir: state.sort.dir }) : sortOrder(),
        ...threadArgs(),
      },
    };
  }
  const folder = currentFolder();
  if (!folder?.folder_name) return null;
  return {
    command: "messages",
    args: {
      account: folder.account_name,
      folder: folder.folder_name,
      sort: sortOrder(),
      ...threadArgs(),
    },
  };
}
/** One page of the source, quietly: paging is the list's own machinery. */
export function fetchPage(
  source: { command: string; args: Record<string, unknown> },
  page: number,
): Promise<MessageHeader[]> {
  return call<MessageHeader[]>(source.command, {
    ...source.args,
    offset: page * PAGE_SIZE,
    limit: PAGE_SIZE,
  });
}
/** How many rows the source has now, by what the sidebar knows. */
export async function sourceTotal(): Promise<number | null> {
  if (state.inboxFolders) return inboxTotal();
  const folder = currentFolder();
  if (!folder?.folder_name) return null;
  return folderTotal(folder);
}
/** How many rows Today's mail has as it is shown: messages, or threads. */
export async function inboxTotal(): Promise<number> {
  const inboxes = todayInboxes();
  if (!state.threads) return inboxes.reduce((n, f) => n + f.total_count, 0);
  return call<number>("inbox_thread_total", {
    folders: inboxes.map((f) => [f.account_name, f.folder_name]),
    expanded: [...state.expandedThreads],
  });
}
/** Take the row count up to `total` when more mail has arrived than the list knows. */
/**
 * The switch beside Today's "Mail" label between every account's inbox and
 * one account's, when there is more than one account to choose between.
 */
function accountSwitch(): string {
  const accounts = [...new Set(inboxFolders().map((f) => f.account_name))];
  if (accounts.length < 2) return "";
  const chosen = todayAccount();
  const option = (value: string | null, label: string) =>
    `<button type="button" role="radio" aria-checked="${chosen === value}" data-mail-account="${escape(value ?? "")}">${label}</button>`;
  return `<span class="seg" role="radiogroup" aria-label="Accounts">${option(null, `<span class="lbl">All</span>`)}${accounts
    .map((a) => option(a, escape(a)))
    .join("")}</span>`;
}
/** Today's "Mail" or "Timeline" label counts what the list holds. */
function paintMailCount(): void {
  const n = el.list.querySelector<HTMLElement>(".mail-sec .n");
  if (n && n.textContent !== String(rowCount())) n.textContent = String(rowCount());
}
/**
 * Place the timeline's threads and reading items again after it is drawn,
 * since whatever drew it may have changed them, and draw it again if that
 * changed how many rows it has. Every switch, filter and refresh that draws
 * Today comes through here.
 */
async function replaceTimeline(): Promise<void> {
  await placeTimeline();
  paintTimeline();
}
/**
 * Draw Today's timeline as it now stands, after its mail, threads or reading
 * changed. Drawn with no rows it holds a note and no rows to draw into, so a
 * change across empty draws the whole list again; anything else draws the
 * rows, the count and the height in place.
 */
export function paintTimeline(): void {
  if (!todayMixed() || state.tasks.view !== "today") return;
  const scroll = el.list.querySelector(":scope > .mail-scroll");
  if (!scroll) return;
  const drawnEmpty = !scroll.querySelector(":scope > .rows");
  if (drawnEmpty !== (rowCount() === 0)) {
    renderList();
    return;
  }
  paintMailCount();
  sizeRows();
  renderRows();
}
export function growTotal(total: number): void {
  if (total <= state.total) return;
  state.total = total;
  paintMailCount();
  sizeRows();
  // New mail comes before the threads and items placed after it.
  if (todayMixed()) void placeTimeline().then(paintTimeline);
}
/** How many rows the virtual list holds: the mail, or Today's timeline. */
function rowCount(): number {
  return todayMixed() ? timelineTotal() : state.total;
}
/** Give the spacer the height of every row. */
function sizeRows(): void {
  const body = el.list.querySelector<HTMLElement>(".rows");
  const height = `${rowCount() * ROW_HEIGHT}px`;
  if (body && body.style.height !== height) body.style.height = height;
}
/**
 * Correct the row count from a page that came back short.
 *
 * `total` starts from the folder's cached count, which goes stale the moment a
 * message is moved or deleted — leaving rows addressed past the end of the
 * folder that can never load, and so stay placeholders forever. A page with
 * fewer rows than requested is the end of the folder, which makes the count
 * self-correcting at no extra cost. Re-reading the real count would mean a
 * COUNT over every folder of every account.
 *
 * A full page proves nothing, since there may be more beyond it.
 */
export function noteTotalFromPage(page: number, received: number): void {
  if (received >= PAGE_SIZE) return;
  const total = page * PAGE_SIZE + received;
  if (total === state.total) return;

  state.total = total;
  paintMailCount();
  // Drop anything now past the end, so a later scroll cannot resurrect it.
  for (const key of [...state.pages.keys()]) {
    if (key * PAGE_SIZE >= total && key !== page) state.pages.delete(key);
  }
  state.messageCursor = Math.min(state.messageCursor, Math.max(0, total - 1));
  sizeRows();
  if (todayMixed()) void placeTimeline().then(paintTimeline);
}
/**
 * The message at a row index, fetching its page first if necessary.
 *
 * Acting on the cursor cannot rely on `rowAt`: fetching is debounced, so a row
 * can be on screen as a placeholder while its page is still pending. Reading it
 * synchronously would make Enter — or any command operating on the cursor — a
 * silent no-op precisely when the user is moving fastest.
 */
export async function ensureRow(index: number): Promise<MessageHeader | undefined> {
  const loaded = rowAt(index);
  if (loaded || memoryRows()) return loaded;

  const source = pageSource();
  if (!source) return undefined;
  const page = Math.floor(index / PAGE_SIZE);
  const generation = state.generation;

  // Marked in flight like a scrolled-to page, so the scroll handler does not
  // ask for the same page a second time while this one is on its way.
  state.fetching.add(page);
  let rows: MessageHeader[];
  try {
    rows = await fetchPage(source, page);
  } finally {
    if (generation === state.generation) state.fetching.delete(page);
  }
  // The folder or sort changed while this was in flight.
  if (generation !== state.generation) return undefined;

  state.pages.set(page, rows);
  noteTotalFromPage(page, rows.length);
  renderRows();
  return rowAt(index);
}
/**
 * The element the message rows scroll inside.
 *
 * Normally the list pane itself. In Today it is the mail half of it, because
 * §4.2 wants the task section sized to its content and the mail list to take
 * the remainder and scroll — which means two scroll regions, and the virtual
 * list has to be told which one is its own. Everything else about the list is
 * unchanged, which is the point: Today shows the same list, not another one.
 */
export function listScroller(): HTMLElement {
  return el.list.querySelector<HTMLElement>(".mail-scroll") ?? el.list;
}
/**
 * Row range overlapping the viewport, padded by the overscan.
 *
 * `body.offsetTop` is the sticky header's height, measured rather than assumed
 * so the range stays correct if the header's padding changes.
 */
export function visibleRange(body: HTMLElement): { first: number; last: number } {
  const scroller = listScroller();
  const scrollTop = Math.max(0, scroller.scrollTop - body.offsetTop);
  const rows = Math.ceil(scroller.clientHeight / ROW_HEIGHT);
  const first = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const last = Math.min(rowCount() - 1, first + rows + OVERSCAN * 2);
  return { first, last };
}
/**
 * Request any unloaded pages covering `first..last`, and drop pages far away.
 *
 * Fetches are fire-and-forget: the row is drawn as a placeholder now and
 * repainted when its page lands. A stale generation means the folder or sort
 * changed while the request was in flight, so the result is discarded.
 */
function ensurePages(from: number, to: number): void {
  if (memoryRows()) return;
  const source = pageSource();
  if (!source) return;
  const { first, last } = mailSpan(from, to);

  const visibleFirst = Math.floor(first / PAGE_SIZE);
  const visibleLast = Math.floor(last / PAGE_SIZE);
  const lastPageIndex = Math.floor(Math.max(0, state.total - 1) / PAGE_SIZE);
  // Widen to the prefetch window so scrolling onward lands in cache.
  const firstPage = Math.max(0, visibleFirst - PAGE_PREFETCH);
  const lastPage = Math.min(lastPageIndex, visibleLast + PAGE_PREFETCH);

  for (let page = firstPage; page <= lastPage; page++) {
    if (state.pages.has(page) || state.fetching.has(page)) continue;
    state.fetching.add(page);
    const generation = state.generation;

    void fetchPage(source, page)
      .then((rows) => {
        if (generation !== state.generation) return;
        state.pages.set(page, rows);
        noteTotalFromPage(page, rows.length);
        renderRows();
      })
      .finally(() => {
        // Only this generation's marker: after a folder switch the same page
        // number belongs to a newer request, which must not be unmarked by
        // this one landing late.
        if (generation === state.generation) state.fetching.delete(page);
      });
  }

  // Evict pages outside the retention window to bound memory.
  for (const page of state.pages.keys()) {
    if (page < firstPage - PAGE_RETENTION || page > lastPage + PAGE_RETENTION) {
      state.pages.delete(page);
    }
  }
}
/**
 * A recycled row element.
 *
 * Rows are built once and then mutated in place. Rebuilding the window's markup
 * on every scroll event — parsing an HTML string and recreating ~30 subtrees,
 * with the style recalc and layout that follows — is what made scrolling feel
 * stepped. Updating `style.top` and a few `textContent`s costs a fraction of
 * that, and keeps the DOM stable so the browser can reuse its layout work.
 */
interface RowNode {
  el: HTMLElement;
  /** Unread, or a staged delete or move: one mark in the status column. */
  mark: HTMLElement;
  /** On Today's timeline, what the row is: mail, a thread, a reading item. */
  kind: HTMLElement;
  /** Flagged, and has files: the marks a look with a flag column shows. */
  flagMark: HTMLElement;
  clipMark: HTMLElement;
  name: HTMLElement;
  addr: HTMLElement;
  to: HTMLElement;
  /** A reply's place under its thread. */
  tree: HTMLElement;
  /** The `subject_munges` icons before the subject. */
  subjectIcons: HTMLElement;
  subject: HTMLElement;
  /** The member count and caret after a thread's subject; empty elsewhere. */
  thread: HTMLElement;
  snippet: HTMLElement;
  /** The folder of a search hit. */
  folder: HTMLElement;
  marks: HTMLElement;
  labels: HTMLElement;
  /** Where a staged move goes. */
  target: HTMLElement;
  tasks: HTMLElement;
  clip: HTMLElement;
  flag: HTMLElement;
  date: HTMLElement;
}
/** Row elements in DOM order, reused as the window moves. */
const rowPool: RowNode[] = [];
function createRowNode(): RowNode {
  const el = document.createElement("div");
  el.className = "row";
  el.setAttribute("role", "option");
  el.innerHTML =
    `<span class="status"><i class="mark"></i><i class="mark"></i><i class="mark"></i></span>` +
    `<span class="kind"></span>` +
    `<span class="from"><span class="name"></span>` +
    `<span class="spw" title="${SPOOF_TITLE}" aria-label="${SPOOF_TITLE}">${svgIcon("alert")}</span>` +
    `<span class="addr"></span></span>` +
    `<span class="to"></span>` +
    `<span class="subject"><span class="tree">└</span><span class="subject-icons"></span>` +
    `<span class="text"></span><span class="thread"></span><span class="snip"></span></span>` +
    `<span class="meta"><span class="folder"></span><span class="marks"></span>` +
    `<span class="labels"></span><span class="target"></span>` +
    `<span class="tasks" title="open tasks">${svgIcon("task")}<span class="n"></span></span>` +
    `<span class="clip">${svgIcon("clip")}</span><span class="flagged">${svgIcon("flag")}</span></span>` +
    `<span class="date"></span>`;
  const q = (selector: string) => el.querySelector<HTMLElement>(selector)!;
  return {
    el,
    mark: q(".status .mark:nth-child(1)"),
    kind: q(".kind"),
    flagMark: q(".status .mark:nth-child(2)"),
    clipMark: q(".status .mark:nth-child(3)"),
    name: q(".from .name"),
    addr: q(".from .addr"),
    to: q(".to"),
    tree: q(".subject .tree"),
    subjectIcons: q(".subject-icons"),
    subject: q(".subject .text"),
    thread: q(".subject .thread"),
    snippet: q(".subject .snip"),
    folder: q(".meta .folder"),
    marks: q(".meta .marks"),
    labels: q(".meta .labels"),
    target: q(".meta .target"),
    tasks: q(".meta .tasks"),
    clip: q(".meta .clip"),
    flag: q(".meta .flagged"),
    date: q(".date"),
  };
}
/** Write a cell's text only when it changed: the pool repaints on every scroll. */
function setText(cell: HTMLElement, text: string): void {
  if (cell.textContent !== text) cell.textContent = text;
}
/** Show or hide a cell, writing only when that changed. */
function show(cell: HTMLElement, visible: boolean): void {
  if (cell.hidden === visible) cell.hidden = !visible;
}
/** Give a row the look of the highlight rules its message matches. */
function applyHighlight(node: RowNode, names: string[]): void {
  // The appearance is part of the key: the cursor's tint is worked out
  // against the cursor fill of the look and the light or dark in force.
  const key = `${names.join("\u0000")}\u0001${document.documentElement.dataset.look}${darkScheme.matches}`;
  if (node.el.dataset.hl === key) return;
  node.el.dataset.hl = key;
  const look = highlightLook(names);
  const set = (property: string, value: string | null) =>
    value ? node.el.style.setProperty(property, value) : node.el.style.removeProperty(property);
  set("--hl-color", look.color);
  set("--hl-cursor-color", look.color ? cursorTint(look.color) : null);
  set("--hl-bg", look.background);
  node.marks.innerHTML = look.icons
    .map((i) => configIcon(i.icon, i.color))
    .join("");
  node.labels.innerHTML = look.labels
    .map((l) => `<span class="chip"${l.color ? ` style="--chip-color:${escape(l.color)}"` : ""}>${escape(l.label)}</span>`)
    .join("");
}
const darkScheme = window.matchMedia("(prefers-color-scheme: dark)");
const cursorTints = new Map<string, string | null>();
/**
 * A rule's colour as the cursor row draws it: mixed toward the cursor's text
 * colour just far enough to read on the cursor fill, so red stays a red tint
 * rather than turning white or vanishing into the blue. 3:1, WCAG's bar for
 * large text: at 4.5:1 every colour is all but white on the blue. Null when
 * a colour cannot be resolved, and the cursor's text colour applies.
 */
function cursorTint(color: string): string | null {
  const key = `${color}\u0000${document.documentElement.dataset.look}${darkScheme.matches}`;
  if (cursorTints.has(key)) return cursorTints.get(key)!;
  // Resolved in the list but outside any row: on the cursor row itself the
  // named colours are all the cursor's text colour.
  const probe = document.createElement("i");
  probe.hidden = true;
  el.list.append(probe);
  const resolve = (value: string) => {
    probe.style.color = "";
    probe.style.color = value;
    return parseRgb(getComputedStyle(probe).color);
  };
  const rule = resolve(color);
  const fill = resolve("var(--cursor-fill)");
  const text = resolve("var(--accent-fg)");
  probe.remove();
  let tint: string | null = null;
  if (rule && fill && text) {
    for (let step = 0; step <= 20; step++) {
      const t = step / 20;
      const mixed = rule.map((c, i) => Math.round(c + (text[i] - c) * t));
      if (contrastRatio(mixed, fill) >= 3 || step === 20) {
        tint = `rgb(${mixed.join(", ")})`;
        break;
      }
    }
  }
  cursorTints.set(key, tint);
  return tint;
}
/** The red, green and blue of a computed colour, 0–255; null for a form not read here. */
function parseRgb(value: string): number[] | null {
  const rgb = value.match(/^rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)/);
  if (rgb) return rgb.slice(1, 4).map(Number);
  const srgb = value.match(/^color\(srgb\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)/);
  if (srgb) return srgb.slice(1, 4).map((c) => Math.round(Number(c) * 255));
  return null;
}
/** WCAG contrast ratio of two opaque colours. */
function contrastRatio(a: number[], b: number[]): number {
  const luminance = (c: number[]) => {
    const [r, g, bl] = c.map((v) => {
      const s = v / 255;
      return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [light, dark] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (light + 0.05) / (dark + 0.05);
}
/** A folder's name as the sidebar shows it, for a search hit's badge. */
function folderLabel(account: string, folder: string): string {
  const known = selectableFolders().find(
    (f) => !f.is_pinned && f.account_name === account && f.folder_name === folder,
  );
  return known?.display_name ?? folder;
}
/** The row classes a highlight adds, for the class string the row is drawn with. */
function highlightClasses(names: string[]): string {
  if (names.length === 0) return "";
  const look = highlightLook(names);
  let cls = " hl";
  if (look.color) cls += " hl-color";
  if (look.bold) cls += " hl-bold";
  if (look.italic) cls += " hl-italic";
  if (look.dim) cls += " hl-dim";
  return cls;
}
/**
 * The mail rows between two positions of the virtual list. On Today's
 * timeline the threads and reading items placed among them are not mail.
 */
function mailSpan(first: number, last: number): { first: number; last: number } {
  if (!todayMixed()) return { first, last };
  return { first: first - placedBetween(0, first - 1).length, last: last - placedBetween(0, last).length };
}
const KIND_ICON = { mail: "mail", chat: "chat", reading: "book" } as const;
const KIND_TITLE = { mail: "Mail", chat: "Chat", reading: "Reading" } as const;
/** Put a row element at a position, standing for an item of `kind`. */
function placeRow(node: RowNode, kind: TimelineKind, index: number, position: number): void {
  const top = `${position * ROW_HEIGHT}px`;
  if (node.el.style.top !== top) node.el.style.top = top;
  const data = node.el.dataset;
  for (const [key, of] of [["msg", "mail"], ["chat", "chat"], ["reading", "reading"]] as const) {
    if (kind === of) data[key] = String(index);
    else delete data[key];
  }
  node.el.id = `${kind === "mail" ? "message" : kind}-row-${index}`;
  const shown = todayMixed() ? kind : "";
  if (node.kind.dataset.k !== shown) {
    node.kind.dataset.k = shown;
    node.kind.title = shown ? KIND_TITLE[shown] : "";
    node.kind.innerHTML = shown ? svgIcon(KIND_ICON[shown]) : "";
  }
}
/** Whether the timeline's cursor is on the item of `kind` at `index`. */
function timelineCursor(kind: TimelineKind, index: number): boolean {
  const at = { mail: state.messageCursor, chat: state.chat.cursor, reading: state.reading.cursor }[kind];
  return index === at && (!todayMixed() || timelineKind() === kind);
}
/** Draw a thread that waits into a row element, on Today's timeline. */
function fillChatRow(node: RowNode, row: WaitingThread, index: number): void {
  const cursor = timelineCursor("chat", index);
  const selected = chatSelected(row);
  let cls = "row chat-row";
  if (cursor) cls += " cursor";
  if (selected) cls += " selected";
  if (row.unread) cls += " unread";
  if (node.el.className !== cls) node.el.className = cls;
  const aria = String(cursor || selected);
  if (node.el.getAttribute("aria-selected") !== aria) node.el.setAttribute("aria-selected", aria);
  applyHighlight(node, []);
  applyThread(node, undefined);
  // The reply count stands where a thread's member count does, so the next
  // message drawn here works its thread mark out afresh.
  delete node.el.dataset.thread;
  setText(node.thread, row.title && row.replies ? String(row.replies) : "");

  const mark = row.waits ? (row.mention ? "@" : row.unread ? "N" : "") : row.mine_last ? "r" : "";
  if (node.mark.dataset.g !== mark) node.mark.dataset.g = mark;
  node.flagMark.dataset.g = "";
  node.clipMark.dataset.g = row.files ? "+" : "";
  setMarkedText(node.name, row.author);
  setText(node.addr, "");
  setText(node.to, "");
  const room = `<span class="room">${escape(row.room.label)}</span>`;
  if (node.subjectIcons.innerHTML !== room) node.subjectIcons.innerHTML = room;
  setMarkedText(node.subject, row.title ?? row.preview);
  setMarkedText(node.snippet, row.title ? row.preview : "");
  setText(node.folder, "");
  setText(node.target, "");
  show(node.tasks, row.task);
  setText(node.tasks.lastElementChild as HTMLElement, "");
  show(node.clip, row.files);
  show(node.flag, false);
  setText(node.date, row.newest_at ? listDate(new Date(row.newest_at).toISOString(), state.settings.dateFormat, dateColumn) : "");
}
/** The date column's room, as the last drawing of the list found it. */
let dateColumn: ReturnType<typeof dateRoom> = null;
/** Point an existing row element at a different message. */
function updateRowNode(node: RowNode, index: number, m: MessageHeader | undefined, position = index): void {
  placeRow(node, "mail", index, position);

  const cursor = timelineCursor("mail", index);
  const selected = String(cursor || isSelected(index));
  if (node.el.getAttribute("aria-selected") !== selected) {
    node.el.setAttribute("aria-selected", selected);
  }
  if (!m) {
    node.el.className =
      "row loading" +
      (cursor ? " cursor" : "") +
      (isSelected(index) ? " selected" : "");
    for (const cell of [node.name, node.addr, node.to, node.subject, node.snippet, node.folder, node.target, node.date]) {
      setText(cell, "");
    }
    node.subjectIcons.innerHTML = "";
    for (const mark of [node.mark, node.flagMark, node.clipMark]) mark.dataset.g = "";
    for (const cell of [node.tasks, node.clip, node.flag]) show(cell, false);
    applyHighlight(node, []);
    applyThread(node, undefined);
    return;
  }
  fillRow(node, m, cursor, isSelected(index), "");
}
/** The message a jump just landed on, lit for a moment so the eye finds it. */
let landed: number | null = null;
let landedTimer: ReturnType<typeof setTimeout> | undefined;
const LANDED_MS = 1500;
export function flashRow(id: number): void {
  landed = id;
  clearTimeout(landedTimer);
  renderRows();
  landedTimer = setTimeout(() => {
    landed = null;
    renderRows();
  }, LANDED_MS);
}
/** Draw a message into a row element: its classes, its marks and its cells. */
function fillRow(node: RowNode, m: MessageHeader, cursor: boolean, selected: boolean, extraClass: string): void {
  const highlights = m.highlights ?? [];
  // The address is what tells a sender from an impostor, so it is never cut
  // short: the name gives way. A bare address is shown once, as the address.
  const { name, email } = parseFrom(isConversationRow(m) ? (m.thread_newest_from ?? m.from_display) : m.from_display);
  const bare = !email;
  // A collapsed thread's row stands for every member: unread when any is,
  // which the core has already said in `is_read`, and with the sender and
  // the words of the newest.
  const collapsedThread = (m.thread_count ?? 1) > 1 && !m.thread_expanded;
  const conversation = isConversationRow(m);
  const unread = !m.is_read;
  let cls = `row${extraClass}`;
  if (cursor) cls += " cursor";
  if (m.id === landed) cls += " landed";
  if (selected) cls += " selected";
  if (unread) cls += " unread";
  if (m.pending_action === "Delete") cls += " staged-delete";
  if (m.pending_action === "Move") cls += " staged-move";
  if (state.unconfirmed.has(m.id)) cls += " unconfirmed";
  if (bare) cls += " bare-from";
  if (spoofedName(name, email)) cls += " spoofed";
  cls += highlightClasses(highlights);
  if (node.el.className !== cls) node.el.className = cls;
  applyHighlight(node, highlights);
  applyThread(node, m);

  // One mark: a staged delete outranks unread, which outranks answered and
  // forwarded, and a staged move says where it goes among the badges instead.
  const mark =
    m.pending_action === "Delete"
      ? "D"
      : m.pending_action === "Move"
        ? "M"
        : unread
          ? "N"
          : m.is_answered
            ? "r"
            : m.is_forwarded
              ? "f"
              : "";
  if (node.mark.dataset.g !== mark) node.mark.dataset.g = mark;
  const flag = m.is_flagged ? "!" : "";
  if (node.flagMark.dataset.g !== flag) node.flagMark.dataset.g = flag;
  const clip = m.has_attachments ? "+" : "";
  if (node.clipMark.dataset.g !== clip) node.clipMark.dataset.g = clip;

  setMarkedText(node.name, bare ? "" : name);
  setText(node.addr, bare ? name || "(unknown)" : email);
  const to = (m.to_display ?? "")
    .split(",")
    .map((entry) => parseFrom(entry.trim()).name || parseFrom(entry.trim()).email)
    .filter(Boolean)
    .join(", ");
  setText(node.to, to);
  // Empty, not just absent: the subject repair records a genuinely
  // subjectless message as an empty string.
  setMarkedText(node.subject, m.subject || "(no subject)");
  const icons = subjectIconsHtml(m.subject_icons, iconExists);
  if (node.subjectIcons.innerHTML !== icons) node.subjectIcons.innerHTML = icons;
  setMarkedText(node.snippet, m.snippet ?? "");

  setText(node.folder, state.search ? folderLabel(m.account_name, m.folder_name) : "");
  setText(node.target, m.pending_action === "Move" && m.pending_target ? `→ ${m.pending_target}` : "");
  // Blank rather than "0" when a message has no tasks (§6.3). The count comes
  // from a batch query over the whole visible set; an id the batch has not
  // answered for yet is also blank, so the badge fills in rather than
  // flickering through a wrong number.
  const openTasks = (conversation ? m.thread_members! : [m.id]).reduce((n, id) => n + (state.tasks.counts.get(id) ?? 0), 0);
  show(node.tasks, openTasks > 0);
  setText(node.tasks.lastElementChild as HTMLElement, openTasks > 1 ? String(openTasks) : "");
  show(node.clip, m.has_attachments);
  show(node.flag, m.is_flagged);

  const shownDate = (collapsedThread ? m.thread_latest : null) ?? m.date;
  setText(node.date, shownDate ? listDate(shownDate, state.settings.dateFormat, dateColumn) : "");
}
/** A message row outside the virtual list, drawn as the list draws its rows. */
export function messageRowElement(m: MessageHeader, cursor: boolean, extraClass: string): HTMLElement {
  const node = createRowNode();
  fillRow(node, m, cursor, false, extraClass);
  node.el.setAttribute("aria-selected", String(cursor));
  return node.el;
}
/**
 * Draw a row's place in its thread: a reply indents one step per level
 * under a tree mark, and a thread's own row carries the member count and
 * a caret after the subject, so the subjects keep one edge.
 */
function applyThread(node: RowNode, m: MessageHeader | undefined): void {
  const depth = m?.depth ?? 0;
  const count = m?.thread_count ?? 1;
  const key = `${depth}\u0000${count > 1 ? (m?.thread_expanded ? "open" : "closed") : ""}\u0000${count}`;
  if (node.el.dataset.thread === key) return;
  node.el.dataset.thread = key;
  if (depth > 0) node.el.style.setProperty("--depth", String(depth - 1));
  else node.el.style.removeProperty("--depth");
  show(node.tree, depth > 0);
  setText(node.thread, count > 1 && depth === 0 ? `${count} ${m?.thread_expanded ? "▾" : "▸"}` : "");
}
/**
 * Put `text` in a cell, with the search's words marked when there is one.
 *
 * Written only when it changed, like the plain cells: the pool is repainted
 * on every scroll and every cursor move, and rebuilding innerHTML each time
 * would undo what reusing the rows is for.
 */
function setMarkedText(cell: HTMLElement, text: string): void {
  const terms = state.search?.terms ?? [];
  const key = terms.length ? `${text}\u0000${terms.join("\u0000")}` : text;
  if (cell.dataset.shown === key) return;
  cell.dataset.shown = key;
  if (terms.length) cell.innerHTML = markTerms(escape(text), terms);
  else cell.textContent = text;
}
/**
 * Wrap every occurrence of the search's words in `<mark>`, in HTML that is
 * already escaped. Only the text between tags is touched, and entities are
 * stepped over, so an anchor's `href` and an `&amp;` stay what they are.
 */
export function markTerms(html: string, terms: string[]): string {
  if (terms.length === 0) return html;
  const pattern = new RegExp(
    `(${terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`,
    "gi",
  );
  return html
    .split(/(<[^>]*>|&[a-zA-Z#0-9]+;)/)
    .map((piece, i) => (i % 2 === 1 ? piece : piece.replace(pattern, "<mark>$1</mark>")))
    .join("");
}
/** Pending debounced fetch, if any. */
let fetchTimer: number | undefined;
/**
 * Fetch the pages under the viewport once scrolling settles.
 *
 * The range is recomputed when the timer fires, not when it was scheduled, so
 * pages merely scrolled past are never requested.
 */
function scheduleFetch(): void {
  const body = el.list.querySelector<HTMLElement>(".rows");
  if (!body) return;

  // Nothing cached at all means the folder was just opened, and there is
  // nothing to scroll past — waiting only delays the first paint. The debounce
  // exists to stop a fling requesting every page it crosses, which cannot
  // happen before anything has been loaded.
  if (state.pages.size === 0) {
    if (fetchTimer !== undefined) {
      clearTimeout(fetchTimer);
      fetchTimer = undefined;
    }
    const { first, last } = visibleRange(body);
    ensurePages(first, last);
    return;
  }

  if (fetchTimer !== undefined) clearTimeout(fetchTimer);
  fetchTimer = window.setTimeout(() => {
    fetchTimer = undefined;
    const rows = el.list.querySelector<HTMLElement>(".rows");
    if (!rows) return;
    const { first, last } = visibleRange(rows);
    ensurePages(first, last);
  }, FETCH_DEBOUNCE);
}
/** Repaint the windowed rows in place, leaving the header and spacer alone. */
export function renderRows(): void {
  const body = el.list.querySelector<HTMLElement>(".rows");
  if (!body) return;
  const { first, last } = visibleRange(body);
  const needed = Math.max(0, last - first + 1);

  // Grow the pool to the window size; it only ever grows to the tallest
  // viewport seen, so this settles after the first frame.
  while (rowPool.length < needed) {
    const node = createRowNode();
    rowPool.push(node);
    body.appendChild(node.el);
  }
  while (rowPool.length > needed) {
    rowPool.pop()!.el.remove();
  }

  if (state.settings.dateFormat === "auto") dateColumn = dateRoom(rowPool[0]?.date) ?? dateColumn;
  const mixed = todayMixed();
  for (let k = 0; k < needed; k++) {
    const node = rowPool[k]!;
    const position = first + k;
    const item = mixed ? timelineAt(position) : undefined;
    if (item?.kind === "chat") {
      placeRow(node, "chat", item.index, position);
      fillChatRow(node, state.chat.rows[item.index]!, item.index);
    } else if (item?.kind === "reading") {
      placeRow(node, "reading", item.index, position);
      const cursor = timelineCursor("reading", item.index);
      const selected = state.reading.selected.has(item.index);
      fillRow(node, state.reading.rows[item.index]!.message, cursor, selected, " reading-row");
      node.el.setAttribute("aria-selected", String(cursor || selected));
    } else {
      const index = item?.index ?? position;
      updateRowNode(node, index, rowAt(index), position);
    }
  }
  // The pane keeps the keyboard; the row under the cursor is what a screen
  // reader announces as active.
  const kind = mixed ? timelineKind() : "mail";
  const active = `${kind === "mail" ? "message" : kind}-row-${{ mail: state.messageCursor, chat: state.chat.cursor, reading: state.reading.cursor }[kind]}`;
  if (el.list.getAttribute("aria-activedescendant") !== active) {
    el.list.setAttribute("aria-activedescendant", active);
  }
  scheduleFetch();
  const mail = mailSpan(first, last);
  scheduleTaskCounts(mail.first, mail.last);
  renderSteady();
  refreshMore();
  // Rows changing under the cursor change what it is on; the reader follows.
  followCursor();
}
/** Coalesce scroll-driven repaints to one per animation frame. */
let framePending = false;
function requestRender(): void {
  if (framePending) return;
  framePending = true;
  requestAnimationFrame(() => {
    framePending = false;
    renderRows();
  });
}
/**
 * The mail half of the list: the virtual list's spacer.
 *
 * Factored out because Today wants exactly this, in a scroll region of its own,
 * under the tasks — the same list rather than a second one (§4.1).
 */
function mailListMarkup(): string {
  const label = todayMixed() ? "Timeline" : "Messages";
  return `<div class="rows" role="listbox" aria-label="${label}" style="height:${rowCount() * ROW_HEIGHT}px"></div>`;
}
/**
 * The task half of the list pane.
 *
 * Sized to its content, which is what `flex: none` here and `flex: 1` on the
 * mail half amount to: §4.2 will not have the tasks squeezed to a sliver by a
 * long mail list. It gets its own scrollbar past a point, because "sized to its
 * content" cannot mean an eight-hundred-row section either.
 */
export function renderTaskSection(view: TaskListView): string {
  const searching = state.tasks.search.trim().length > 0;
  const found = view === "search" ? state.search : null;
  const rows = renderTaskRows(
    state.tasks.rows,
    {
      // Both halves keep a cursor — losing your place in one because you
      // stepped into the other would make `<tab>` a way of forgetting where you
      // were. Which of them is live is said by the pane's class instead.
      cursor: state.tasks.cursor,
      selected: state.tasks.selected,
      today: state.tasks.today,
      // Order is a preference over the whole list, so it is set where the whole
      // list is — not in Completed or Trash, where the rows are ordered by when
      // they left it (§4.4, §10.2).
      draggable: !isPaged(view) && view !== "trash" && view !== "search",
      view,
      grouping: groupedByTag(view) ? "tag" : view === "search" || view === "upcoming" ? "date" : undefined,
      dueFirst: dueFirst(view),
      tags: state.tasks.tags,
    },
    emptyTaskMessage(view, searching),
  );

  // Search results end on their completed tasks, folded until asked for (§26).
  const done = found ? found.tasks.filter((t) => t.is_completed).length : 0;
  const folded =
    found && done > 0 && !found.completedOpen
      ? groupHeading(
          { key: "done", label: "Completed", icon: "done" },
          `<span class="n">${done}</span><span class="fold" title="Show them (za)">▸</span>`,
        ).replace('class="tk-day"', 'class="tk-day folded"')
      : "";
  const count = found ? found.tasks.length : distinctTasks(state.tasks.rows);
  const more =
    isPaged(view) && !state.tasks.exhausted
      ? `<button type="button" class="tk-more" data-task-more>Load more</button>`
      : view === "trash" && count > 0
        ? `<button type="button" class="tk-more" data-task-empty-trash>Empty trash</button>`
        : "";

  const halves = hasMailHalf();
  return `<div class="tk-section${halves ? "" : " full"}">
      <div class="sec tk-section-head"><span class="sec-t">
        <span class="lbl">${escape(halves || view === "search" ? "Tasks" : taskViewLabel(view))}</span>
        <span class="n">${count}${state.tasks.exhausted ? "" : "+"}</span>
        ${searching ? `<span class="n">filtered</span>` : ""}
        ${view === "today" ? dueSwitch(dueFirst(view), dueNowCount()) : ""}
        ${canGroupByTag(view) ? tagSwitch(groupedByTag(view)) : ""}
      </span></div>
      <div class="tk-list" role="listbox" aria-label="Tasks">${rows}${folded}${more}</div>
    </div>`;
}
/**
 * The switch in Today's Tasks heading that lists the tasks due by today first
 * (§31), as `zd` does, carrying how many there are, in red, whether it is on
 * or not.
 */
function dueSwitch(on: boolean, count: number): string {
  const key = keyFor("toggle-due-first");
  const title = `${on ? "Stop listing the due tasks first" : "List the due tasks first"}${key ? ` (${key})` : ""}`;
  const due = `${count} due today or earlier`;
  const n = count > 0 ? `<span class="due-n" title="${due}">${count}</span>` : "";
  return `<span class="seg due-switch"><button type="button" role="switch" aria-checked="${on}" data-due-first title="${title}" aria-label="Due first, ${due}">${svgIcon("alert")}<span class="tw">due</span>${n}</button></span>`;
}
/** The switch at the end of the Tasks heading that groups the tasks by tag (§30), as `zg` does. */
function tagSwitch(on: boolean): string {
  const key = keyFor("toggle-group-by-tag");
  const title = `${on ? "Stop grouping by tag" : "Group by tag"}${key ? ` (${key})` : ""}`;
  return `<span class="seg tag-switch"><button type="button" role="switch" aria-checked="${on}" data-group-by-tag title="${title}" aria-label="Group by tag">${svgIcon("tag")}<span class="tw">by tag</span></button></span>`;
}
/**
 * Say which half of a task view is live.
 *
 * Both halves keep their cursor, so something has to distinguish the one a
 * keystroke will act on from the one merely holding a place. A class rather
 * than a re-render, because it changes on every `<tab>` and every crossing of
 * the boundary.
 */
export function applyListSectionClass(): void {
  // The keyboard never stays in a section that is not drawn.
  if (state.tasks.view === "today" && hasMailHalf() && !shows(state.tasks.section)) {
    state.tasks.section = firstSection();
  }
  const active = inTaskView() && taskSectionActive();
  // On the timeline the mail, the threads and the reading are one list, live
  // as the mail half is; the section the cursor's item belongs to is not drawn.
  const mixed = todayMixed();
  el.list.classList.toggle("today-mixed", mixed);
  const reading = readingActive() && !mixed;
  const chat = chatSectionActive() && !mixed;
  el.list.classList.toggle("with-mail", !!el.list.querySelector(":scope > .mail-scroll"));
  if (hasMailHalf()) applyTodaySections();
  el.list.classList.toggle("tasks-active", active);
  el.list.classList.toggle("reading-active", reading);
  el.list.classList.toggle("chat-active", chat);
  el.list.classList.toggle("mail-active", inTaskView() && !active && !reading && !chat);
}
/**
 * Draw the sections `today.ts` shows and hide the rest, with the lines that
 * would stand above them. The first section shown takes the room the others
 * leave; in columns that is the first one beside the mail, whose grid is
 * built here from what is shown rather than from a rule per combination.
 * Classes on the pane rather than on the sections, which are drawn again on
 * their own.
 */
function applyTodaySections(): void {
  const columns = todayColumns();
  const mixed = todayMixed();
  // Mixed reads the chat and the reading into the mail's column.
  const side = ((mixed ? ["tasks"] : ["tasks", "chat", "reading"]) as TodaySection[]).filter(shows);
  const mail = mixed ? shows("mail") || shows("chat") || shows("reading") : shows("mail");
  const lineAbove = (section: TodaySection) =>
    shows(section) && (side.indexOf(section) > 0 || (!columns && mail));
  const shown: [string, boolean][] = [
    ["mail", mail],
    ["tasks", shows("tasks")],
    ["chat", shows("chat") && !mixed],
    ["reading", shows("reading") && !mixed],
    ["tk-split", mail && shows("tasks")],
    ["tk-vsplit", mail && side.length > 0],
    ["ch-split", lineAbove("chat") && !mixed],
    ["rd-split", lineAbove("reading") && !mixed],
  ];
  for (const [part, on] of shown) el.list.classList.toggle(`hides-${part}`, !on);
  const filler = columns || !mail ? side[0] : undefined;
  for (const section of ["tasks", "chat", "reading"]) el.list.classList.toggle(`fills-${section}`, section === filler);

  fitTodayRows();
}
/** The most of Today's height the sections below the first one beside the mail may take, in percent. */
const MAX_TODAY_SIDE_SHARE = 80;
/**
 * Build the grid of Today in columns from the sections shown and the shares
 * they were dragged to. Also called while a line between them is dragged.
 */
export function fitTodayRows(): void {
  const style = el.list.style;
  if (!todayColumns() || !hasMailHalf()) {
    style.removeProperty("--today-rows");
    style.removeProperty("--today-cols");
    style.removeProperty("--today-side");
    return;
  }
  const mixed = todayMixed();
  const side = ((mixed ? ["tasks"] : ["tasks", "chat", "reading"]) as TodaySection[]).filter(shows);
  const mail = mixed ? shows("mail") || shows("chat") || shows("reading") : shows("mail");
  // Each share was dragged on its own, some in the stacked layout, so
  // together they can ask for more than the pane. The grid would then grow
  // past it, and the mail beside them with it, its last rows out of sight.
  const kept = (section: TodaySection) => parseFloat(style.getPropertyValue(`--today-${section}`)) || 0;
  const asked = side.slice(1).reduce((sum, section) => sum + kept(section), 0);
  const scale = asked > MAX_TODAY_SIDE_SHARE ? MAX_TODAY_SIDE_SHARE / asked : 1;
  const track = (section: TodaySection) =>
    kept(section) > 0 ? `${(kept(section) * scale).toFixed(2)}%` : "fit-content(33%)";
  const rows = side.flatMap((section, i) => (i === 0 ? ["minmax(0, 1fr)"] : ["1px", track(section)]));
  style.setProperty("--today-rows", rows.join(" ") || "minmax(0, 1fr)");
  if (mail && side.length > 0) style.removeProperty("--today-cols");
  else style.setProperty("--today-cols", "minmax(0, 1fr)");
  style.setProperty("--today-side", mail ? "3" : "1");
}
/** Show the reader pane, unless the list stands alone in its empty view. */
export function applyReaderPresence(): void {
  el.app.classList.toggle("list-alone", listStandsAlone());
  el.app.classList.toggle("reader-alone", roomAlone());
  applyMaximize();
}
export function renderList(): void {
  const typing = filterHasFocus();
  drawList();
  // Again for what was just drawn: the classes describe the pane's children.
  applyListSectionClass();
  attachFilterBar(typing);
}
/** What the pane last showed, so that a redraw of it keeps its scroll positions and a new view does not. */
let drawnFor: string | null = null;
function drawList(): void {
  const shows = `${state.tasks.view ?? ""}\u0000${state.search?.query ?? ""}\u0000${state.search?.conversation ?? ""}`;
  const kept = shows === drawnFor ? scrollsIn(el.list) : new Map<string, number>();
  drawnFor = shows;
  drawPane(kept);
  // Again once every region is filled: the reading's rows are put in after
  // the markup, and an empty region cannot be scrolled.
  restoreScrolls(el.list, kept);
}
function drawPane(kept: Scrolls): void {
  if (state.booting) {
    el.list.className = "pane";
    el.list.innerHTML = `<div class="empty-view loading">${icon("fa-rotate", "spin")}<p>Loading…</p></div>`;
    return;
  }
  const view = state.tasks.view;
  const empty = emptyView();
  // The reader pane goes with an empty list, so the keyboard cannot stay in it.
  if (empty && !state.info && state.focus === "reader") state.focus = "list";
  // …and a reader standing alone takes it from the list it hides.
  if (roomAlone() && state.focus === "list") state.focus = "reader";
  el.list.className =
    `pane${state.focus === "list" ? " focused" : ""}` + (view ? " task-view" : "");
  applyListSectionClass();
  applyColumnClasses();
  applyDensity();

  // Rebuilding the pane's markup discards the pooled row elements, so the pool
  // has to be dropped with them wherever that happens.
  rowPool.length = 0;

  applyReaderPresence();
  if (empty) {
    el.list.innerHTML = empty;
    return;
  }

  if (inChatView()) {
    el.list.innerHTML = chatListMarkup();
    return;
  }

  if (view && !hasMailHalf()) {
    el.list.innerHTML = renderTaskSection(view);
    return;
  }

  if (view) {
    // Two scroll regions, mail first: stacked, or side by side. The mail half
    // carries the virtual list; the task half is ordinary flow content,
    // because a Today list is tens of rows and virtualising it would buy
    // nothing and cost a geometry to keep right.
    const columns = todayColumns();
    const mixed = todayMixed();
    el.list.classList.toggle("today-columns", columns);
    const rows = rowCount();
    const empty = state.filter?.text.trim()
      ? mixed ? "Nothing matches" : "No mail matches"
      : view === "search" ? "No mail" : mixed ? "Nothing waits" : "No inbox mail";
    el.list.innerHTML =
      `<div class="mail-scroll"><div class="sec mail-sec"><span class="sec-t"><span class="lbl">${mixed ? "Timeline" : "Mail"}</span><span class="n">${rows}</span>${view === "today" ? threadsSwitch() + accountSwitch() : ""}</span></div>${
        rows === 0 ? `<div class="empty">${empty}</div>` : mailListMarkup()
      }</div>` +
      (columns
        ? `<div class="split tk-vsplit" data-tk-vsplit role="separator" aria-orientation="vertical" title="Drag to resize · double-click for the usual widths"></div>`
        : `<div class="split tk-split" data-tk-split role="separator" aria-orientation="horizontal" title="Drag to resize · double-click to fit the tasks"></div>`) +
      renderTaskSection(view) +
      (mixed ? "" : chatTodaySection() + (view === "today" && todayReading() ? renderReadingSection() : ""));
    applyListSectionClass();
    // Before the rows are drawn, which go by where the mail half is scrolled.
    restoreScrolls(el.list, kept);
    if (rows > 0) renderRows();
    if (mixed) void replaceTimeline();
    else paintReadingRows();
    return;
  }

  // The spacer carries the full scroll height; `.rows` positions each rendered
  // row absolutely within it.
  el.list.innerHTML = mailListMarkup();
  renderRows();
}
// Capture rather than bubble: scroll events do not bubble, and in Today the
// element that scrolls is the mail half rather than the pane. One listener
// covers both because a capturing listener on the pane still sees an event
// aimed at anything inside it.
el.list.addEventListener(
  "scroll",
  () => {
    if (rowCount() > 0) requestRender();
  },
  { passive: true, capture: true },
);
el.list.addEventListener("click", (e) => {
  const target = e.target as HTMLElement;

  const account = target.closest<HTMLElement>("[data-mail-account]");
  if (account) {
    void chooseTodayAccount(account.dataset.mailAccount || null);
    return;
  }
  if (target.closest("[data-due-first]")) {
    void setDueFirst(!dueFirstOn());
    return;
  }
  if (target.closest("[data-group-by-tag]")) {
    void setGroupByTag(!groupedByTag(state.tasks.view));
    return;
  }
  if (target.closest("[data-search-completed]")) {
    toggleSearchCompleted();
    return;
  }

  const shows = target.closest<HTMLElement>("[data-reading-shows]");
  if (shows) {
    void setTodayReadingShows(shows.dataset.readingShows === "new" ? "new" : "must-read");
    return;
  }
  if (target.closest("[data-reading-more]")) {
    void openReading(todayReadingShows() === "new" ? "list" : "must-read").then(() => {
      state.focus = "list";
      render();
    });
    return;
  }
  // On the timeline Shift and ⌘ select across every kind.
  const placed = todayMixed() && (e.shiftKey || e.metaKey) ? target.closest<HTMLElement>(".rows .row") : null;
  if (placed) {
    clickOnTimeline(Math.round(parseFloat(placed.style.top) / ROW_HEIGHT), e.shiftKey);
    return;
  }
  // A plain click is a fresh start there, for every kind.
  if (todayMixed() && target.closest(".rows .row") && anySelected()) clearTimelineSelection();
  const item = target.closest<HTMLElement>("[data-reading]");
  if (item) {
    state.focus = "list";
    if (state.tasks.section === "mail") cancelMarkRead();
    state.tasks.section = "reading";
    state.reading.cursor = Number(item.dataset.reading);
    applyListSectionClass();
    paintReadingRows();
    if (todayMixed()) renderReader();
    previewCursor();
    return;
  }

  // Clicking a message shows it below but leaves the keyboard in the list, so
  // j/k keeps walking the messages.
  const row = target.closest<HTMLElement>("[data-msg]");
  if (row) {
    const index = Number(row.dataset.msg);
    const wasSection = state.tasks.section;
    state.focus = "list";
    // Today has both lists in it, and the reader shows whichever half is being
    // walked. Clicking a message is that half being chosen — without this the
    // click moved the mail cursor while the pane went on showing a task, so the
    // message appeared to be unselectable and the row's own bindings acted on
    // something else entirely (§4.2).
    state.tasks.section = "mail";
    if (wasSection !== state.tasks.section) {
      // Which half is live is drawn from a class on the pane, and the task rows
      // take their quiet mark from it. Without this the task under the cursor
      // kept the live cursor colour while the message that was just clicked got
      // the dim one — the two halves both looking selected, and the brighter of
      // them the one that was not.
      applyListSectionClass();
      // The pane below has to change with it. The two multi-select branches
      // return before opening anything, and would otherwise leave the task
      // detail sitting under a mail selection.
      renderReader();
    }

    // Shift extends from the anchor, Command picks rows out one at a time —
    // the two gestures every list on the platform shares.
    if (e.shiftKey) {
      selectRange(index);
      renderRows();
      reportSelection();
      return;
    }
    if (e.metaKey) {
      // Picking one row out of "everything" means the selection is no longer
      // the folder, so it has to be written out before it can be changed.
      if (state.selectAll) {
        state.selectAll = false;
        for (let i = 0; i < state.total; i++) state.selected.add(i);
      }
      if (state.selected.has(index)) state.selected.delete(index);
      else state.selected.add(index);
      state.anchor = index;
      state.messageCursor = index;
      renderRows();
      reportSelection();
      return;
    }

    // A plain click is a fresh start: it moves the cursor and abandons whatever
    // was selected, which is what clicking a row means everywhere else.
    clearSelection();
    state.anchor = index;
    state.messageCursor = index;
    // Whoever moves the cursor repaints it, as `move` and `jump` do. Opening
    // the message only repaints the reader — deliberately, so that following
    // the cursor does not rebuild the list on every keystroke — so relying on
    // it here left the highlight behind on the previously clicked row.
    renderRows();
    void openMessage(false);
  }
});
// A double-click opens the message in a window of its own — a draft, as
// Enter does, in a compose window. The click before it has put the cursor
// on the row.
el.list.addEventListener("dblclick", (e) => {
  if (e.shiftKey || e.metaKey) return;
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-msg]");
  const msg = row ? rowAt(Number(row.dataset.msg)) : undefined;
  if (!msg) return;
  if (viewingDrafts()) {
    void openSelection();
    return;
  }
  // A conversation opens at its newest message, as a reply from its row answers.
  const id = isConversationRow(msg) ? msg.thread_members!.at(-1)! : msg.id;
  void openItemWindow("message", id, msg.subject).catch(() => {});
});
/**
 * Right-click opens the native menu for the row under the pointer.
 *
 * The row is selected first, so the menu acts on what was clicked rather than
 * wherever the cursor happened to be — the behaviour every mail client has.
 */
el.list.addEventListener("contextmenu", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-msg]");
  if (!row) return;
  e.preventDefault();
  state.messageCursor = Number(row.dataset.msg);
  state.focus = "list";
  renderRows();
  openMessageMenu();
});
export function render(): void {
  // A window of one item has no other pane to be in.
  if (ITEM) state.focus = "reader";
  renderFolders();
  renderList();
  renderReader();
  renderToolbar();
  setStatus(state.status);
  renderSteady();
  refreshMore();
}
