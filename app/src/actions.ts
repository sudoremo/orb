import { invoke } from "@tauri-apps/api/core";
import { isPaged } from "./tasklist";
import { ROW_HEIGHT } from "./appearance";
import { call, setStatus } from "./bridge";
import { editDraft, viewingDrafts } from "./composing";
import { applyListSectionClass, cursorLocation, cursorRow, ensureCursorRow, listScroller, memoryRows, readingActive, render, renderRows, rowAt } from "./list";
import { FolderInfo, MessageDetail, MessageHeader, SearchHit, sortFrom } from "./model";
import { offerUndo } from "./moving";
import { readerHidden } from "./maximize";
import { readerDrawn, renderReader } from "./reader";
import { syncSearchBox } from "./search";
import { clearSelection, reportSelection, selectRange } from "./selection";
import { applyFilter, syncFilterToView } from "./filter";
import { moveReadingCursor, openReading, paintReadingRows } from "./reading";
import { showOutbox } from "./outbox";
import { currentEntry, folderEntry, hasMailHalf, toggleFold, inTaskSection, inTaskView, openVirtualFolder, renderFolders, selectableFolders, sidebarEntries, taskSectionActive } from "./sidebar";
import { el, state } from "./state";
import { shows, TodaySection } from "./today";
import { todayMixed } from "./layout";
import { cursorPosition, timelineAt, timelineTotal } from "./mixed";
import { chatActive, inChatView, leaveChat, moveChatSectionCursor, openChat, openRoom, paintChatSection } from "./chat";
import { describeCount } from "./sync";
import { conversationFor, edgeOfConversation, fillConversation, isConversationRow, loadConversationTasks, moveInConversation, paintShown, readConversation, rowKey, shownConversation } from "./conversation";
import { optimisticWrite, withMessages } from "./targets";
import { filterByTag, loadMessageTasks, loadMoreCompleted, loadTaskDetail, openTaskView, paintTaskCursor, previewTaskCursor, renderTaskList, todayColumns } from "./taskview";

/**
 * The folder whose messages are on screen.
 *
 * Deliberately not the sidebar cursor: the cursor can be walked over the task
 * views and back without opening any of them, and every action that names an
 * account and a folder means the one being read, not the one being pointed at.
 */
export function currentFolder(): FolderInfo | undefined {
  return selectableFolders()[state.openFolder];
}
/** Scroll the cursor row just far enough to be fully visible. */
export function scrollCursorIntoView(): void {
  const body = el.list.querySelector<HTMLElement>(".rows");
  if (!body) return;
  const scroller = listScroller();
  const top = body.offsetTop + (todayMixed() ? cursorPosition() : state.messageCursor) * ROW_HEIGHT;
  const bottom = top + ROW_HEIGHT;
  const viewTop = scroller.scrollTop;
  const viewBottom = viewTop + scroller.clientHeight;
  // Keep the header's height clear at the top, since it floats over the rows.
  if (top < viewTop + body.offsetTop) scroller.scrollTop = top - body.offsetTop;
  else if (bottom > viewBottom) scroller.scrollTop = bottom - scroller.clientHeight;
}
/** Bring the task under the cursor into view within its own section. */
export function scrollTaskCursorIntoView(): void {
  const row = el.list.querySelector<HTMLElement>(`[data-task="${state.tasks.cursor}"]`);
  row?.scrollIntoView({ block: "nearest" });
}
/**
 * Move the cursor and drag the selection's far end with it.
 *
 * Anchored at the row the selection started from, so reversing direction
 * shrinks the range rather than selecting backwards over rows already left.
 */
export function extendSelection(delta: number): void {
  if (state.focus !== "list") return;
  if (state.anchor === null) state.anchor = state.messageCursor;
  const to = Math.min(Math.max(state.messageCursor + delta, 0), Math.max(state.total - 1, 0));
  selectRange(to);
  scrollCursorIntoView();
  renderRows();
  reportSelection();
}
export const clamp = (value: number, max: number): number =>
  Math.min(Math.max(value, 0), Math.max(max, 0));
/**
 * Move through a task view as though its two lists were one (§4.2).
 *
 * The combined index is the whole trick: the mail occupies `0..m`, the tasks
 * `m..m+n`, the chat and the reading what follows, a section put away
 * nothing, and moving is arithmetic on that single number. Anything else — a
 * per-section cursor with a hand-written crossing rule — gets the boundary
 * wrong in one direction or the other, which is exactly where it is noticed.
 * Side by side, the two are separate lists, and the index stays within the
 * half it is in.
 *
 * The views other than Today have no mail half, which needs no special case:
 * `m` is zero and the mail branch is unreachable.
 */
function moveContinuous(delta: number): void {
  if (todayMixed() && state.tasks.section !== "tasks") {
    moveOnTimeline(delta);
    return;
  }
  const tasks = state.tasks.rows.length;
  const mail = state.total;
  // In the order they stand on screen, a section put away holding no rows.
  const size: Record<TodaySection, number> = {
    mail: shows("mail") ? mail : 0,
    tasks: shows("tasks") ? tasks : 0,
    // Waiting, the screenful Today's section lists.
    chat: shows("chat") ? Math.min(state.chat.rows.length, 30) : 0,
    reading: shows("reading") ? state.reading.rows.length : 0,
  };
  const order: TodaySection[] = ["mail", "tasks", "chat", "reading"];
  const start = (section: TodaySection) => order.slice(0, order.indexOf(section)).reduce((n, s) => n + size[s], 0);
  const section = state.tasks.section;
  const inMail = section === "mail";
  const inReading = section === "reading";
  const inChat = section === "chat";
  const cursor = { mail: state.messageCursor, tasks: state.tasks.cursor, chat: state.chat.cursor, reading: state.reading.cursor }[section];
  const combined = start(section) + cursor;
  const total = start("reading") + size.reading;
  let [lo, hi] = [0, total - 1];
  if (todayMixed()) {
    // The tasks are a column of their own beside the timeline.
    [lo, hi] = [start("tasks"), start("tasks") + size.tasks - 1];
    if (hi < lo) return;
  } else if (hasMailHalf() && todayColumns()) {
    [lo, hi] = inMail ? [0, size.mail - 1] : [size.mail, total - 1];
    if (hi < lo) return;
  }
  const next = Math.min(Math.max(combined + delta, lo), Math.max(hi, lo));
  const target = order.find((s) => size[s] > 0 && next < start(s) + size[s]) ?? section;
  const at = next - start(target);

  if (target === "chat") {
    void moveChatSectionCursor(at);
    paintTaskCursor();
    paintReadingRows();
    renderRows();
    return;
  }
  if (inChat) paintChatSection();
  if (target === "reading") {
    moveReadingCursor(at);
    paintTaskCursor();
    renderRows();
    return;
  }
  if (target === "tasks") {
    state.tasks.section = "tasks";
    state.tasks.cursor = at;
    if (inMail || inReading) cancelMarkRead();
    if (inReading || inChat) {
      applyListSectionClass();
      paintReadingRows();
    }
    paintTaskCursor();
    scrollTaskCursorIntoView();
    // Reaching the end of Completed asks for the next page rather than
    // stopping: §10.2 refuses to cap the view, and a "load more" you have to
    // aim at is a cap with extra steps.
    if (
      isPaged(state.tasks.view) &&
      !state.tasks.exhausted &&
      state.tasks.cursor >= tasks - 1
    ) {
      void loadMoreCompleted();
    }
    previewTaskCursor();
    return;
  }

  if (inReading) cancelMarkRead();
  state.tasks.section = "mail";
  state.tasks.cursor = Math.max(0, Math.min(state.tasks.cursor, tasks - 1));
  state.messageCursor = at;
  paintReadingRows();
  // The task half still has to stop reading as the live one.
  paintTaskCursor();
  scrollCursorIntoView();
  renderRows();
  previewCursor();
}
/**
 * Move along Today's timeline (`mixed.ts`): the item at the new position
 * takes the keyboard, its section and its cursor with it, so the keys and
 * the reader are those of the kind of row the cursor is on.
 */
function moveOnTimeline(delta: number): void {
  const total = timelineTotal();
  if (total === 0) return;
  const item = timelineAt(clamp(cursorPosition() + delta, total - 1));
  if (!item) return;
  const from = state.tasks.section;
  if (from === "mail" && item.kind !== "mail") cancelMarkRead();
  if (item.kind === "chat") {
    void moveChatSectionCursor(item.index);
    return;
  }
  if (item.kind === "reading") {
    moveReadingCursor(item.index);
    return;
  }
  state.tasks.section = "mail";
  state.messageCursor = item.index;
  if (from !== "mail") applyListSectionClass();
  scrollCursorIntoView();
  renderRows();
  previewCursor();
}
/** Put the timeline's cursor on the row at a position. */
export function goToPosition(position: number): void {
  moveOnTimeline(position - cursorPosition());
}
export function move(delta: number): void {
  if (state.focus === "folders") {
    state.sidebarCursor = clamp(state.sidebarCursor + delta, sidebarEntries().length - 1);
    renderFolders();
    return;
  }
  if (state.focus !== "list") return;
  if (inTaskView()) {
    moveContinuous(delta);
    return;
  }
  state.messageCursor = clamp(state.messageCursor + delta, state.total - 1);
  scrollCursorIntoView();
  renderRows();
  previewCursor();
}
/**
 * Move on from a message just staged: down, unless the message below is
 * staged already — or there is none — and the one above is not.
 *
 * Working up the list then keeps going up, rather than landing on a message
 * already dealt with and having to be turned round with `k` every time.
 */
export function advance(count: number): void {
  if (state.focus !== "list" || (inTaskView() && state.tasks.section !== "mail")) {
    move(count);
    return;
  }
  const cursor = state.messageCursor;
  const below = cursor + 1 < state.total ? rowAt(cursor + 1) : null;
  const above = cursor > 0 ? rowAt(cursor - 1) : null;
  const doneBelow = below === null || !!below?.pending_action;
  const openAbove = !!above && !above.pending_action;
  move(doneBelow && openAbove ? -count : count);
}
/**
 * Move the cursor by hand: the selection goes first, as in any list where an
 * unmodified arrow key moves the cursor. `move` itself keeps it, for the
 * commands that select and step on.
 */
export function navigate(delta: number): void {
  // With the reader's keyboard on a conversation, the keys walk its messages.
  if (state.focus === "reader" && moveInConversation(delta)) return;
  dropSelections();
  move(delta);
}
/** `jump` by hand, dropping the selection the way `navigate` does. */
export function navigateTo(to: "top" | "bottom"): void {
  if (state.focus === "reader" && edgeOfConversation(to)) return;
  dropSelections();
  jump(to);
}
function dropSelections(): void {
  if (state.focus !== "list") return;
  // On the timeline every kind's selection goes, as the mail's does.
  if (todayMixed() && (state.chat.selected.size > 0 || state.reading.selected.size > 0)) {
    state.chat.selected.clear();
    state.chat.anchor = null;
    state.reading.selected.clear();
    clearSelection();
    renderRows();
    setStatus("selection cleared");
  }
  if (state.selectAll || state.selected.size > 0) {
    clearSelection();
    renderRows();
    reportSelection();
  }
  if (state.tasks.selected.size > 0) {
    state.tasks.selected.clear();
    state.tasks.anchor = null;
    renderTaskList();
    setStatus("selection cleared");
  }
}
export function jump(to: "top" | "bottom"): void {
  if (state.focus === "folders") {
    state.sidebarCursor = to === "top" ? 0 : sidebarEntries().length - 1;
    renderFolders();
    return;
  }
  if (state.focus !== "list") return;
  if (inTaskView()) {
    // The top of Today is the first message and the bottom is the last task:
    // one list, so its ends are the ends of the whole thing. In columns, the
    // ends of the column the cursor is in.
    const span = state.tasks.rows.length + state.total + (todayMixed() ? timelineTotal() : 0);
    moveContinuous(to === "top" ? -span : span);
    return;
  }
  state.messageCursor = to === "top" ? 0 : Math.max(0, state.total - 1);
  scrollCursorIntoView();
  renderRows();
  previewCursor();
}
/**
 * Point the list at the current folder and load the first window of rows.
 *
 * Nothing is prefetched beyond the viewport: the row count comes from
 * `FolderInfo.total_count`, and pages arrive as they are scrolled towards.
 *
 * `keepCursor` holds the cursor's row index across a reload — the right
 * behaviour after deleting or moving a message, where the next one slides up
 * into place. Without it the cursor returns to the top, which is what a folder
 * or sort change should do.
 */
/** The `messages` arguments that say whether a page is threads, and which are open. */
export function threadArgs(): { threads: boolean; expanded: string[] } {
  return { threads: state.threads, expanded: [...state.expandedThreads] };
}
/** How many rows the open folder has as it is shown: messages, or threads. */
export async function folderTotal(folder: FolderInfo): Promise<number> {
  if (!folder.folder_name) return folder.total_count;
  if (!state.threads) {
    // Asked rather than read off the folder list, which a long sync
    // refreshes only when it ends.
    const total = await call<number>("folder_total", {
      account: folder.account_name,
      folder: folder.folder_name,
    }).catch(() => null);
    return typeof total === "number" ? total : folder.total_count;
  }
  return call<number>("thread_total", {
    account: folder.account_name,
    folder: folder.folder_name,
    expanded: [...state.expandedThreads],
  });
}
export async function loadMessages(keepCursor = false): Promise<void> {
  // A task view has no folder underneath it, and the refreshes that call this
  // after a sync or an action must not tip the user out of the view they are
  // in. Opening a folder clears the view first, so this only ever short-cuts a
  // refresh.
  if (inTaskView() || inChatView()) return;
  const folder = currentFolder();
  if (!folder?.folder_name) return;

  // Browsing a folder means no longer showing search results.
  state.search = null;
  // A filter stays with its folder, and a refresh of the folder is its own.
  syncFilterToView();
  if (state.filter?.text.trim()) return applyFilter();
  state.inboxFolders = null;
  // Invalidate any pages still in flight for the previous folder or sort.
  state.generation += 1;
  state.pages.clear();
  state.fetching.clear();
  // Row indices refer to a different set of messages now — and so does
  // "everything", which is why the mode goes as well as the set.
  clearSelection();
  // A folder opens the way its config says; `zt` and the sort change it for
  // as long as it is open.
  const key = `${folder.account_name}\u0000${folder.folder_name}`;
  if (state.threadsFolder !== key) {
    state.threadsFolder = key;
    state.threads = folder.threads;
    state.expandedThreads.clear();
    // …and in the order its config says (`sort`, else `default_sort`).
    state.sort = sortFrom(folder.sort);
  }
  const generation = state.generation;
  const total = await folderTotal(folder);
  // The folder changed again while the count was on its way.
  if (generation !== state.generation) return;
  state.total = total;
  if (!keepCursor) {
    state.messageCursor = 0;
    state.detail = null;
    listScroller().scrollTop = 0;
  }
  state.messageCursor = Math.min(state.messageCursor, Math.max(0, state.total - 1));
  render();

  // The cursor row is drawn as the selected one, so the reader has to be
  // showing it. Without this, arriving in a folder — including at launch —
  // highlights a message and leaves an empty pane next to it, which reads as
  // the selection not having taken.
  followCursor();
}
/**
 * Load the cursor message into the reader. `moveFocus` distinguishes opening it
 * with Enter (keyboard moves to the reader) from clicking it (keyboard stays in
 * the list).
 *
 * Resolves once the reader shows the message, not once its body is in: the
 * body may have to come from the server, and a command waiting for it would
 * hold up every key typed after it (see `runCommand`). The headers the row has
 * are drawn at once, and the body follows when it arrives.
 */
export async function openMessage(moveFocus: boolean): Promise<void> {
  const generation = state.generation;
  const index = state.messageCursor;
  const msg = await ensureCursorRow();
  if (!msg) return;
  if (isConversationRow(msg) && !readingActive()) return openConversation(msg, moveFocus, generation);
  if (state.search && !readingActive()) {
    const hit = memoryRows()?.[index];
    if (hit) setStatus(`${hit.account_name}/${hit.folder_name}`);
  }
  if (state.detail?.id !== msg.id) {
    // Until the body is in, no command may take the message read before for
    // this one; the reader shows this one's headers and says it is loading.
    state.detail = null;
    loadingFor = msg.id;
  }
  if (moveFocus) {
    state.focus = "reader";
    render();
  }
  void showBody(msg.id, generation, moveFocus).catch(() => {
    // `call` has said why. The reader says it too, rather than go on loading,
    // and never falls back to another message.
    if (loadingFor === msg.id) loadingFor = null;
    if (cursorMessageId() === msg.id) {
      el.reader.querySelector(".fetching")?.replaceChildren("This message could not be loaded.");
    }
  });
}
/** Fetch a message's body and draw it, if it is still the one to show. */
async function showBody(id: number, generation: number, moveFocus: boolean): Promise<void> {
  const detail = await call<MessageDetail>("message", { id });
  // The folder changed, or the cursor is on another message now, while the
  // body was in flight — so this is no longer the message to show. Asked by
  // identity: mail arriving above shifts every row, and the same index then
  // holds a different message.
  if (generation !== state.generation || cursorMessageId() !== id) return;
  // A preview does not take the reader from a panel that opened meanwhile.
  if (!moveFocus && state.info) {
    loadingFor = null;
    return;
  }
  // Looking at one message's source says nothing about wanting to see the
  // next one's, so the reader comes back to however mail is normally read.
  if (state.view === "raw") state.view = state.settings.defaultView;
  state.raw = null;
  state.info = null;

  state.detail = detail;
  loadingFor = null;
  // Cleared rather than left: the previous message's tasks would otherwise be
  // drawn under this one's headers until the query comes back, which reads as
  // this message having them.
  state.tasks.linked = [];
  void loadMessageTasks(detail.id).catch(() => {
    // `call` has already reported it; a message without its task block is
    // still a message.
  });
  // The message is on screen now, so the clock towards "read" starts here
  // rather than when the cursor arrived — a slow fetch should not eat the
  // time the user is given to move on.
  scheduleMarkRead(id);
  // Only the reader changed. A full render rebuilds the list's DOM and
  // discards the row pool, which once per cursor move would flicker and undo
  // what the virtual list is for.
  renderReader();
  if (detail.incomplete) void completeBody(id, generation);
}
/**
 * Bring a message read from what is stored up to the whole of it, once the
 * server answers, if it is still the one shown. Quietly: offline, every
 * message read would otherwise say so in the status line, and the reader
 * already has what could be read.
 */
async function completeBody(id: number, generation: number): Promise<void> {
  const whole = await invoke<MessageDetail>("message_complete", { id }).catch(() => null);
  if (!whole || whole.incomplete) return;
  if (generation !== state.generation || state.detail?.id !== id) return;
  state.detail = whole;
  renderReader();
}
/**
 * Show the conversation a collapsed thread's row stands for, as
 * `openMessage` shows a message: read its members, then draw it and fetch
 * the bodies of the messages it opens.
 */
async function openConversation(row: MessageHeader, moveFocus: boolean, generation: number): Promise<void> {
  const conv = conversationFor(row)?.signature === rowKey(row) ? conversationFor(row)! : await readConversation(row);
  if (generation !== state.generation || cursorMessageId() !== row.id) return;
  if (!moveFocus && state.info) {
    loadingFor = null;
    return;
  }
  // A conversation has no one source to show.
  if (state.view === "raw") state.view = state.settings.defaultView;
  state.raw = null;
  state.info = null;
  const fresh = state.conversation !== conv;
  state.conversation = conv;
  state.detail = conv.details.get(conv.focus) ?? null;
  state.tasks.linked = [];
  if (fresh) void loadConversationTasks(conv).catch(() => {});
  void fillConversation(conv);
  scheduleMarkRead(row.id);
  if (moveFocus) {
    state.focus = "reader";
    render();
  } else {
    renderReader();
  }
}
/**
 * Read the members of the conversation on screen again, when the row says
 * they changed — a message arrived, or was read elsewhere — and redraw what
 * changed, keeping which messages are open and where the reader is.
 */
async function refreshConversation(row: MessageHeader): Promise<void> {
  const generation = state.generation;
  const signature = rowKey(row);
  if (refreshing === signature) return;
  refreshing = signature;
  try {
    const conv = await readConversation(row);
    if (generation !== state.generation || cursorMessageId() !== row.id) return;
    state.conversation = conv;
    state.detail = conv.details.get(conv.focus) ?? null;
    void fillConversation(conv);
    void loadConversationTasks(conv).catch(() => {});
    renderReader();
  } catch {
    // `call` has said why; the conversation as it was stays on screen.
  } finally {
    if (refreshing === signature) refreshing = null;
  }
}
/** The row a conversation is being read again for, so a repaint does not ask twice. */
let refreshing: string | null = null;
/** Whether the reader holds what the cursor is on: its message, or its conversation. */
export function readerHoldsCursor(): boolean {
  const row = cursorRow();
  if (isConversationRow(row) && !readingActive()) return conversationFor(row) !== null;
  const id = cursorMessageId();
  return id !== null && id !== undefined && state.detail?.id === id;
}
/** How long the cursor must rest before the reader follows it. */
export const PREVIEW_DEBOUNCE = 90;
let previewTimer: ReturnType<typeof setTimeout> | undefined;
let markReadTimer: ReturnType<typeof setTimeout> | undefined;
/**
 * Start counting towards marking the message under the cursor as read.
 *
 * Displaying a message is not reading it — the reader follows the cursor, so
 * everything walked past is displayed on the way. Dwelling on one is the
 * signal, which is why this is a timer and not something `openMessage` does.
 *
 * Any cursor movement cancels it, so a message passed over is never marked.
 */
function scheduleMarkRead(id: number): void {
  clearTimeout(markReadTimer);
  if (state.settings.markReadAfterMs <= 0) return;

  markReadTimer = setTimeout(() => {
    const where = cursorLocation();
    const msg = cursorRow();
    if (msg?.id === id && isConversationRow(msg) && !readingActive()) {
      markConversationRead(msg);
      return;
    }
    // Still the message under the cursor, still on screen, still unread.
    if (!where || !msg || msg.id !== id || msg.is_read) return;
    if (state.detail?.id !== id) return;

    void optimisticWrite(
      [msg],
      (m) => {
        m.is_read = true;
      },
      () =>
        call(
          "mark_read",
          { account: where.account, folder: where.folder, uids: [msg.uid], read: true },
        ),
    ).catch(() => {
      // `optimisticWrite` puts the row back; `call` has reported the reason.
    });
  }, state.settings.markReadAfterMs);
}
/**
 * A conversation dwelt on is read: every message of it, in whichever folder
 * each lives, and its row with them.
 */
function markConversationRead(row: MessageHeader): void {
  const conv = shownConversation();
  if (!conv || conv.rowId !== row.id) return;
  const unread = conv.members.filter((m) => !m.is_read);
  if (unread.length === 0) return;
  const groups = new Map<string, { account: string; folder: string; uids: number[] }>();
  for (const m of unread) {
    const key = `${m.account_name}\u0000${m.folder_name}`;
    const group = groups.get(key) ?? { account: m.account_name, folder: m.folder_name, uids: [] };
    group.uids.push(m.uid);
    groups.set(key, group);
  }
  void optimisticWrite(
    [row, ...unread],
    (m) => {
      m.is_read = true;
    },
    () =>
      Promise.all(
        [...groups.values()].map((g) => call("mark_read", { ...g, read: true })),
      ),
  )
    .then(() => paintShown(conv))
    .catch(() => {
      // `optimisticWrite` puts the rows back; `call` has reported the reason.
    });
}
/** Stop any pending mark-read. Called whenever the cursor leaves a message. */
export function cancelMarkRead(): void {
  clearTimeout(markReadTimer);
}
/**
 * The message under the cursor: its id, `null` when the list is empty, and
 * `undefined` when its row is not loaded yet.
 */
export function cursorMessageId(): number | null | undefined {
  if (readingActive()) return cursorRow()?.id ?? null;
  if (state.total === 0) return null;
  return rowAt(state.messageCursor)?.id;
}
/**
 * Bring the reader in line with the cursor, which every change of the cursor
 * or of the rows under it has to end in.
 *
 * The reader shows the message under the cursor or that it is loading it —
 * never another one. So when the cursor is on a different message from the
 * one drawn, the reader switches at once to the new one's headers and a
 * loading line, and the body follows once the cursor comes to rest.
 */
export function followCursor(moved = false): void {
  // A task or a panel has the reader; the message comes back when it goes.
  if (state.info || taskSectionActive() || chatActive()) return;
  // A message nobody can see is neither fetched nor counted towards read.
  if (readerHidden()) {
    cancelMarkRead();
    return;
  }
  const id = cursorMessageId();
  const drawn = readerDrawn();

  // The cursor's row is not loaded yet. Moved there, the reader says it is
  // loading at once, and the load reads the row in. Otherwise the cursor is
  // where it was and the reader is already right; repainting the row when it
  // comes back in brings it here again.
  if (id === undefined) {
    if (moved && (drawn?.id !== undefined || loadingFor !== undefined)) {
      cancelMarkRead();
      renderReader();
      scheduleLoad();
    }
    return;
  }

  // A conversation the reader holds: draw it, or bring what is drawn up to
  // date, and read its members again when the row says they changed.
  const row = id === null ? undefined : cursorRow();
  if (id !== null && isConversationRow(row) && !readingActive()) {
    const conv = conversationFor(row);
    if (conv) {
      clearTimeout(previewTimer);
      loadingFor = null;
      if (drawn?.id !== id || !drawn.body || !drawn.conversation) {
        renderReader();
        scheduleMarkRead(id);
      } else {
        paintShown(conv);
      }
      if (conv.signature !== rowKey(row)) void refreshConversation(row);
      return;
    }
  } else if (id !== null && state.detail?.id === id) {
    // The body in memory is the cursor's: draw it, if something else is drawn.
    clearTimeout(previewTimer);
    loadingFor = null;
    if (drawn?.id !== id || !drawn.body || drawn.conversation) {
      renderReader();
      scheduleMarkRead(id);
    }
    return;
  }

  // Already showing this message's headers, with its body on its way.
  if (drawn?.id === id && loadingFor === id) return;
  // The cursor has moved, so whatever was being counted towards read no
  // longer is.
  cancelMarkRead();
  renderReader();
  if (id === null) {
    clearTimeout(previewTimer);
    loadingFor = null;
    return;
  }
  scheduleLoad();
}
/** The message a body is being fetched for, so a repaint does not ask again. */
let loadingFor: number | null | undefined = null;
/**
 * Fetch the cursor message's body once the cursor comes to rest. Debounced:
 * holding `j` walks the list a row at a time, and fetching a body for every
 * row passed over would spend the whole time loading messages nobody looked
 * at.
 */
function scheduleLoad(): void {
  clearTimeout(previewTimer);
  loadingFor = cursorMessageId();
  previewTimer = setTimeout(() => {
    void openMessage(false).catch(() => {
      // Reading the row in failed; `call` has said why.
      loadingFor = null;
    });
  }, PREVIEW_DEBOUNCE);
}
/** Show whatever the cursor is on. Called whenever the cursor moves. */
export function previewCursor(): void {
  followCursor(true);
}
/**
 * Show a message's conversation as the list, with the cursor on the message:
 * the list that has it in it wherever the message lives.
 */
export function showConversation(hits: SearchHit[], id: number, subject: string | null): void {
  state.generation += 1;
  state.pages.clear();
  state.fetching.clear();
  state.inboxFolders = null;
  state.search = {
    query: `thread: ${subject || "(no subject)"}`,
    effective: "",
    hits,
    terms: [],
    conversation: subject || "(no subject)",
    tasks: [],
    taskQuery: null,
    kind: "mail",
    completedOpen: false,
    from: null,
    scope: { kind: "all" },
  };
  state.messageCursor = Math.max(0, hits.findIndex((h) => h.message.id === id));
  state.detail = null;
  clearSelection();
  state.total = hits.length;
  listScroller().scrollTop = 0;
  render();
  setStatus(`${hits.length} message${hits.length === 1 ? "" : "s"} in the conversation · q leaves it`);
}
/**
 * Open the first folder there is, as the landing view when there are no tasks.
 *
 * The first entry rather than a remembered one, for the same reason Today is not
 * remembered either: the sidebar's own order is the answer to "where does this
 * open", and the pinned folders are at the top of it precisely because they are
 * where somebody wants to start.
 */
export async function openFirstFolder(): Promise<void> {
  if (!selectableFolders().length) return;
  state.tasks.view = null;
  state.tasks.detail = null;
  leaveChat();
  state.sidebarCursor = folderEntry(0);
  state.openFolder = 0;
  syncSearchBox();
  await loadMessages();
}
export async function openSelection(): Promise<void> {
  if (state.focus === "folders") {
    const entry = currentEntry();
    if (entry?.kind === "task") {
      await openTaskView(entry.view);
      state.focus = "list";
      render();
      return;
    }
    if (entry?.kind === "virtual") {
      await openVirtualFolder(entry.index);
      return;
    }
    if (entry?.kind === "tag") {
      await filterByTag(entry.name);
      return;
    }
    if (entry?.kind === "chat") {
      await openChat();
      return;
    }
    if (entry?.kind === "outbox") {
      showOutbox();
      return;
    }
    if (entry?.kind === "chat-room") {
      await openRoom(entry.rid);
      return;
    }
    if (entry?.kind === "chat-group") {
      toggleFold();
      return;
    }
    if (entry?.kind === "reading") {
      await openReading(entry.view);
      state.focus = "list";
      render();
      return;
    }
    if (entry?.kind === "account" || entry?.kind === "section") {
      toggleFold();
      return;
    }
    if (entry?.kind === "folder") {
      // Opening a folder is what leaves a task view; nothing else does, which
      // is why every refresh path can assume the view stays put.
      state.tasks.view = null;
      state.tasks.detail = null;
      leaveChat();
      state.openFolder = entry.index;
      syncSearchBox();
      await loadMessages();
      state.focus = "list";
      render();
    }
    return;
  }
  if (state.focus === "list") {
    // In a task view the list is tasks, and opening one means reading it.
    if (inTaskSection()) {
      await loadTaskDetail(true);
      return;
    }
    // In the Drafts folder, opening means carrying on writing.
    if (viewingDrafts()) {
      const msg = await ensureCursorRow();
      const where = cursorLocation();
      if (msg && where) await editDraft(msg, where.account);
      return;
    }
    await openMessage(true);
  }
}
export function back(): void {
  state.info = null;
  state.focus = state.focus === "reader" ? "list" : "folders";
  render();
}
export async function toggleRead(): Promise<void> {
  await withMessages(async (t, account, folder) => {
    // One state for the whole set: if any of them is still unread, the
    // intention is to read them, not to invert each row against itself. Judged
    // from the rows on screen, which for a whole-folder selection is a sample —
    // the alternative is reading the folder to decide, which is what this is
    // avoiding.
    const read = t.rows.some((m) => !m.is_read);
    const undo = `mark ${describeCount(t.count)} ${read ? "read" : "unread"}`;
    const done = await optimisticWrite(
      t.rows,
      (m) => { m.is_read = read; },
      () => call("mark_read", { account, folder, uids: t.uids, read, undo }),
    );
    if (done) offerUndo(`marked ${describeCount(t.count)} ${read ? "read" : "unread"}`);
  });
}
