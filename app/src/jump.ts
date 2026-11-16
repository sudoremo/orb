/**
 * Landing on a message from elsewhere — a notification, a task's linked mail —
 * and going back to where the jump was made from.
 *
 * The reader only ever shows the message under the cursor, so landing on a
 * message means finding a list it is in. The list on screen comes first: a
 * message Today lists is shown in Today, not in its folder. Then its folder,
 * then its conversation, which every message has.
 */

import { loadMessages, openMessage, scrollCursorIntoView, showConversation } from "./actions";
import { call, setStatus } from "./bridge";
import { inChatView, leaveChat } from "./chat";
import { setFilter } from "./filter";
import { spellKey } from "./keys";
import { ensureRow, flashRow, memoryRows, pageSource, render, renderRows, rowAt } from "./list";
import { SearchHit } from "./model";
import { showTodaySection } from "./panes";
import { syncSearchBox } from "./search";
import { clearSelection } from "./selection";
import { folderEntry, inTaskView, selectableFolders } from "./sidebar";
import { state, todayHasMail } from "./state";
import { loadedIndexOf } from "./targets";
import { TaskListView } from "./tasklist";
import { keyFor, loadInboxes, openTaskView, previewTaskCursor } from "./taskview";
import { shows } from "./today";

/** Where a jump was made from, for `jump-back` to return to. */
interface Origin {
  view: TaskListView | null;
  folder: number;
  search: typeof state.search;
  inboxFolders: typeof state.inboxFolders;
  filter: string | null;
  section: typeof state.tasks.section;
  taskCursor: number;
  messageId: number | null;
  focus: typeof state.focus;
}

/** The places jumped from, latest last; as many as vim's jump list keeps back. */
const origins: Origin[] = [];
const MAX_ORIGINS = 100;

/** Where the list is now, or null in a view `jump-back` could not return to. */
function here(): Origin | null {
  if (inChatView() || state.info) return null;
  const message = rowAt(state.messageCursor);
  return {
    view: state.tasks.view,
    folder: state.openFolder,
    search: state.search,
    inboxFolders: state.inboxFolders,
    filter: state.filter?.text.trim() || null,
    section: state.tasks.section,
    taskCursor: state.tasks.cursor,
    messageId: message?.id ?? null,
    focus: state.focus,
  };
}

/** Whether the list on screen has mail a cursor can be put on. */
function listShowsMail(): boolean {
  if (inChatView()) return false;
  if (state.search) return state.search.kind !== "tasks";
  if (state.tasks.view === "today") return todayHasMail() && shows("mail");
  return !inTaskView();
}

/** Where a message is in the rows the list holds now, or null. */
function listedIndexOf(id: number): number | null {
  const rows = memoryRows();
  if (!rows) return loadedIndexOf(id);
  const at = rows.findIndex((h) => h.message.id === id);
  return at >= 0 ? at : null;
}

/**
 * Find a message in the paged list on screen — the open folder, or Today's
 * inboxes — reading in the page it is on and opening its thread in a
 * threaded list. Not through a filter: what it hides stays hidden.
 */
async function findInPagedList(id: number, account: string | null, folder: string | null): Promise<number | null> {
  const source = pageSource();
  if (!source || !account || !folder) return null;
  const folders = state.inboxFolders
    ? state.inboxFolders.flatMap((f): [string, string][] => (f.folder_name ? [[f.account_name, f.folder_name]] : []))
    : [[source.args.account as string, source.args.folder as string] as [string, string]];
  if (!folders.some(([a, f]) => a === account && f === folder)) return null;

  if (state.threads) {
    const place = await call<{ row: number; thread: string; expand: boolean } | null>("thread_row", {
      folders,
      id,
      expanded: [...state.expandedThreads],
    });
    if (!place) return null;
    if (place.expand && !state.expandedThreads.has(place.thread)) {
      state.expandedThreads.add(place.thread);
      if (state.inboxFolders) await loadInboxes();
      else await loadMessages(true);
    }
    await ensureRow(place.row);
    return loadedIndexOf(id);
  }
  const row = state.inboxFolders
    ? await call<number | null>("inbox_message_row", { folders, id, sort: source.args.sort })
    : await call<number | null>("message_row", { account, folder, id, sort: source.args.sort });
  if (row === null) return null;
  await ensureRow(row);
  return loadedIndexOf(id);
}

/** Put the list's cursor on a row of mail, Today's mail section taking the keyboard. */
function placeCursor(at: number): void {
  clearSelection();
  state.messageCursor = at;
  if (state.tasks.view === "today") showTodaySection("mail");
  scrollCursorIntoView();
  renderRows();
}

/** Open a folder for a jump to land in, leaving whatever view was open. */
async function openFolder(index: number): Promise<void> {
  state.tasks.view = null;
  state.tasks.detail = null;
  leaveChat();
  state.openFolder = index;
  state.sidebarCursor = folderEntry(index);
  syncSearchBox();
  await loadMessages();
}

/** The key that goes back, for the status line to name. */
function backHint(): string {
  const key = keyFor("jump-back");
  return key ? ` · ${spellKey(key)} goes back` : "";
}

/** Land on a message, with the cursor on it and the reader showing it. */
export async function showMessage(
  id: number,
  accountName: string | null,
  folderName: string | null,
): Promise<void> {
  const origin = here();
  const remember = () => {
    if (!origin) return;
    origins.push(origin);
    if (origins.length > MAX_ORIGINS) origins.shift();
  };
  const land = async (at: number, where: string | null) => {
    remember();
    placeCursor(at);
    flashRow(id);
    await openMessage(true);
    // Today draws its sections again for the reader, and the mail's scroll
    // goes with them.
    scrollCursorIntoView();
    renderRows();
    if (where) setStatus(`in ${where}${backHint()}`);
  };

  // On screen already, or in the list on screen once paged to.
  if (listShowsMail()) {
    const at = listedIndexOf(id);
    if (at !== null) return land(at, null);
  }
  const hits = await call<SearchHit[]>("thread_messages", { id });
  const own = hits.find((h) => h.message.id === id);
  const account = accountName ?? own?.account_name ?? null;
  const folder = folderName ?? own?.folder_name ?? null;
  if (listShowsMail() && !state.filter?.text.trim()) {
    const at = await findInPagedList(id, account, folder);
    if (at !== null) return land(at, null);
  }

  // Its folder.
  const index = selectableFolders().findIndex((f) => f.account_name === account && f.folder_name === folder);
  if (index >= 0) {
    await openFolder(index);
    const at = listedIndexOf(id) ?? (await findInPagedList(id, account, folder));
    if (at !== null) {
      // Named as under its account, not as a pin is, which takes the account's name.
      const same = selectableFolders().filter((f) => f.account_name === account && f.folder_name === folder);
      const name = same.find((f) => !f.is_pinned) ?? same[0];
      return land(at, name ? `${name.display_name} · ${name.account_name}` : null);
    }
  }

  // Its conversation.
  if (!own) {
    setStatus("that message is no longer here");
    return;
  }
  remember();
  state.tasks.view = null;
  state.tasks.detail = null;
  showConversation(hits, id, own.message.subject ?? null);
  flashRow(id);
  await openMessage(true);
  setStatus(`in its conversation · q leaves it${backHint()}`);
}

/** Go back to where the last jump to a message was made from. */
export async function jumpBack(): Promise<void> {
  const origin = origins.pop();
  if (!origin) {
    setStatus("no jump to go back from");
    return;
  }

  // The view
  if (origin.search) {
    state.tasks.view = origin.view;
    state.openFolder = origin.folder;
    state.inboxFolders = origin.inboxFolders;
    state.search = origin.search;
    state.filter = null;
    state.generation += 1;
    state.pages.clear();
    state.fetching.clear();
    clearSelection();
    state.total = origin.search.hits.length;
    state.messageCursor = 0;
    syncSearchBox();
    render();
  } else if (origin.view && origin.view !== "search") {
    await openTaskView(origin.view);
  } else if (selectableFolders()[origin.folder]) {
    await openFolder(origin.folder);
  }
  if (origin.filter) await setFilter(origin.filter);

  // The cursor
  const back = async () => {
    if (origin.messageId === null) return null;
    const at = listedIndexOf(origin.messageId);
    if (at !== null || state.filter?.text.trim()) return at;
    const location = memoryRows() ? null : await locate(origin.messageId);
    return location ? findInPagedList(origin.messageId, location.account, location.folder) : null;
  };
  if (origin.view === "today" && origin.section !== "mail" && origin.section !== "tasks") {
    showTodaySection(origin.section);
  } else if (origin.view && origin.section === "tasks") {
    state.tasks.section = "tasks";
    state.tasks.cursor = Math.min(origin.taskCursor, Math.max(0, state.tasks.rows.length - 1));
    render();
    previewTaskCursor();
  } else {
    const at = await back();
    if (at !== null) {
      placeCursor(at);
      flashRow(origin.messageId!);
    }
  }
  const onMail = !state.tasks.view || state.search !== null || state.tasks.section === "mail";
  state.focus = origin.focus === "folders" ? "folders" : "list";
  if (origin.focus === "reader" && onMail) await openMessage(true);
  else render();
  if (onMail) {
    scrollCursorIntoView();
    renderRows();
  }
  setStatus(origins.length ? `back · ${origins.length} more to go back to` : "back where the jump was made from");
}

/** Which folder a message is in now. */
async function locate(id: number): Promise<{ account: string; folder: string } | null> {
  const hits = await call<SearchHit[]>("thread_messages", { id }).catch(() => [] as SearchHit[]);
  const own = hits.find((h) => h.message.id === id);
  return own ? { account: own.account_name, folder: own.folder_name } : null;
}
