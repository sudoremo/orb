import { currentFolder, followCursor, loadMessages, scrollCursorIntoView } from "./actions";
import { call, setStatus } from "./bridge";
import { cursorLocation, cursorRow, ensureCursorRow, ensureRow, fetchPage, growTotal, memoryRows, noteTotalFromPage, pageSource, render, renderList, renderRows, rowAt, rowLocation, sourceTotal, visibleRange } from "./list";
import { refreshSearch } from "./moving";
import { readingActive } from "./list";
import { refreshTodayReading } from "./reading";
import { loadInboxes } from "./taskview";
import { MessageHeader } from "./model";
import { conversationMessage, isConversationRow, membersOf, shownConversation } from "./conversation";
import { todayMixed } from "./layout";
import { clearSelection, selectionSize } from "./selection";
import { PAGE_SIZE, el, state } from "./state";

/** Act on the cursor message, or report why nothing happened. */
/**
 * The messages an action applies to: the selection when there is one, else the
 * message under the cursor.
 *
 * Selected rows may have been scrolled out of the cache, so each is fetched
 * back if needed rather than silently dropped from the action.
 */
export async function targetMessages(conversations: "members" | "newest" = "members"): Promise<Targets | null> {
  const folder = currentFolder();

  // Today's timeline: the messages and the reading items selected, together.
  if (todayMixed() && state.reading.selected.size > 0) {
    const picked: { index: number; row: MessageHeader }[] = [];
    for (const index of [...state.selected].sort((a, b) => a - b)) {
      const msg = await ensureRow(index);
      if (msg) picked.push({ index, row: msg });
    }
    const groups = (await expand(picked, conversations)).groups;
    const reading = [...state.reading.selected].sort((a, b) => a - b).flatMap((i) => state.reading.rows[i]?.message ?? []);
    for (const msg of reading) {
      const existing = groups.find((g) => g.account === msg.account_name && g.folder === msg.folder_name);
      if (existing) {
        existing.uids.push(msg.uid);
        existing.rows.push(msg);
      } else {
        groups.push({ account: msg.account_name, folder: msg.folder_name, uids: [msg.uid], rows: [msg] });
      }
    }
    const uids = groups.flatMap((g) => g.uids);
    const rows = groups.flatMap((g) => g.rows);
    return { uids, rows, count: uids.length, groups };
  }

  // Today's reading section has a cursor and no selection.
  if (readingActive()) {
    const msg = cursorRow();
    if (!msg) return null;
    const group = { account: msg.account_name, folder: msg.folder_name, uids: [msg.uid], rows: [msg] };
    return { uids: [msg.uid], rows: [msg], count: 1, groups: [group] };
  }

  // The whole folder: ask the database for the UIDs rather than reading the
  // folder to find them. Paging a quarter of a million headers into memory to
  // collect a number stored beside each one is the difference between one
  // query and thirteen hundred round trips.
  // Today's inboxes: every UID of each, one group per inbox.
  if (state.selectAll && state.inboxFolders && !memoryRows()) {
    const rows = loadedRows();
    const groups: TargetGroup[] = [];
    for (const inbox of state.inboxFolders) {
      if (!inbox.folder_name) continue;
      const uids = await call<number[]>("folder_uids", {
        account: inbox.account_name,
        folder: inbox.folder_name,
      });
      if (!uids.length) continue;
      groups.push({
        account: inbox.account_name,
        folder: inbox.folder_name,
        uids,
        rows: rows.filter(
          (r) => r.account_name === inbox.account_name && r.folder_name === inbox.folder_name,
        ),
      });
    }
    const uids = groups.flatMap((g) => g.uids);
    return { uids, rows, count: uids.length, groups };
  }

  if (state.selectAll && folder?.folder_name && !memoryRows()) {
    const uids = await call<number[]>(
      "folder_uids",
      { account: folder.account_name, folder: folder.folder_name },
    );
    // Only the rows already on screen can be updated ahead of the server; the
    // rest are not being displayed, so there is nothing to correct.
    const rows = loadedRows();
    return {
      uids,
      rows,
      count: uids.length,
      groups: [
        { account: folder.account_name, folder: folder.folder_name, uids, rows },
      ],
    };
  }

  const picked: { index: number; row: MessageHeader }[] = [];
  if (selectionSize() === 0) {
    const msg = await ensureRow(state.messageCursor);
    if (!msg) return null;
    picked.push({ index: state.messageCursor, row: msg });
  } else {
    for (const index of [...state.selected].sort((a, b) => a - b)) {
      const msg = await ensureRow(index);
      if (msg) picked.push({ index, row: msg });
    }
  }
  return expand(picked, conversations);
}
/**
 * The targets of picked rows, where a collapsed thread's row stands for its
 * conversation: for its members — every one, or the newest, for an action
 * that makes one thing of the conversation, as a task. The row itself goes
 * with its first member, so that what is done shows on it at once; only the
 * members' UIDs go to the server.
 */
async function expand(
  picked: { index: number; row: MessageHeader }[],
  conversations: "members" | "newest",
): Promise<Targets> {
  const threaded = picked.filter((p) => isConversationRow(p.row));
  if (threaded.length === 0) {
    return {
      uids: picked.map((p) => p.row.uid),
      rows: picked.map((p) => p.row),
      count: picked.length,
      groups: group(picked),
    };
  }
  const members = await membersOf(threaded.map((p) => p.row));
  const groups = new Map<string, TargetGroup>();
  const add = (account: string, folder: string, row: MessageHeader, uid: number | null) => {
    const key = `${account}\u0000${folder}`;
    const existing = groups.get(key) ?? { account, folder, uids: [], rows: [] };
    if (uid !== null) existing.uids.push(uid);
    existing.rows.push(row);
    groups.set(key, existing);
  };
  for (const { index, row } of picked) {
    const list = isConversationRow(row) ? (members.get(row.thread_id!) ?? []) : [];
    if (list.length === 0) {
      const where = rowLocation(index);
      if (where) add(where.account, where.folder, row, row.uid);
      continue;
    }
    const chosen = conversations === "newest" ? list.slice(-1) : list;
    add(chosen[0]!.account_name, chosen[0]!.folder_name, row, null);
    for (const m of chosen) add(m.account_name, m.folder_name, m, m.uid);
  }
  const all = [...groups.values()];
  const uids = all.flatMap((g) => g.uids);
  return { uids, rows: all.flatMap((g) => g.rows), count: uids.length, groups: all };
}
/**
 * Split rows by the folder they live in.
 *
 * An ordinary folder yields exactly one group and this changes nothing. A
 * search result or Today's All Inboxes list yields several, and without this
 * they were all sent to whichever folder happened to be open — which is the one
 * way a delete can act on the wrong mailbox.
 */
function group(rows: { index: number; row: MessageHeader }[]): TargetGroup[] {
  const groups = new Map<string, TargetGroup>();
  for (const { index, row } of rows) {
    const where = rowLocation(index);
    if (!where) continue;
    const key = `${where.account}\u0000${where.folder}`;
    const existing = groups.get(key);
    if (existing) {
      existing.uids.push(row.uid);
      existing.rows.push(row);
    } else {
      groups.set(key, { ...where, uids: [row.uid], rows: [row] });
    }
  }
  return [...groups.values()];
}
/** Every row currently held in the page cache, in no particular order. */
function loadedRows(): MessageHeader[] {
  const rows: MessageHeader[] = [];
  for (const page of state.pages.values()) {
    for (const row of page) if (row) rows.push(row);
  }
  return rows;
}
/**
 * What an action acts on.
 *
 * `uids` is what goes to the server; `rows` is only what happens to be loaded,
 * so that the rows on screen can be updated before the server answers. For a
 * whole-folder selection the two differ by orders of magnitude, which is the
 * point: acting on a folder should not require having read it.
 */
interface Targets {
  uids: number[];
  rows: MessageHeader[];
  count: number;
  /**
   * The same targets, split by the folder they live in.
   *
   * One entry for an ordinary folder; several for a search result or Today's
   * merged inbox list, where each row carries its own address.
   */
  groups: TargetGroup[];
}
/** One folder's share of an action. */
interface TargetGroup {
  account: string;
  folder: string;
  uids: number[];
  rows: MessageHeader[];
}
/**
 * Run an action over the current target set.
 *
 * `fn` receives every target at once, because every command underneath takes a
 * list of UIDs and one call does the whole set. Handing them over one at a time
 * turned a fifty-message delete into fifty IPC calls and fifty IMAP round
 * trips, run in series — which is the difference between a batch action and
 * fifty single actions with a progress bar.
 *
 * `consumesSelection` is for actions that carry the selection out rather than
 * annotate it: a message moved or deleted is no longer in the list to be
 * selected, so holding on to it means the next command applies to rows that
 * are gone. Marking is not such an action — flagging twelve messages and then
 * marking the same twelve read is one thought, and clearing in between makes
 * it two. An action that returns `false` declined to run — a confirmation
 * answered no — and leaves the selection as it was.
 */
export async function withMessages(
  fn: (targets: Targets, account: string, folder: string) => Promise<void | false>,
  consumesSelection = false,
): Promise<void> {
  const targets = await targetMessages();
  if (!targets || targets.count === 0 || targets.groups.length === 0) {
    setStatus("no message selected");
    return;
  }
  // One call per folder, and for the overwhelmingly common case — a folder
  // being read — that is one call, exactly as before. The loop only does work
  // where the rows genuinely came from different places.
  let acted = false;
  for (const g of targets.groups) {
    const result = await fn(
      { uids: g.uids, rows: g.rows, count: g.uids.length, groups: [g] },
      g.account,
      g.folder,
    );
    if (result !== false) acted = true;
  }
  if (consumesSelection && acted && selectionSize() > 0) {
    clearSelection();
    renderRows();
  }
}
/**
 * Run an action on the message under the cursor alone.
 *
 * For the commands that only make sense one at a time — replying, forwarding,
 * reopening a draft. A selection is not silently ignored: acting on the cursor
 * while twelve rows are highlighted looks like a bug, so it says which one it
 * used.
 */
export async function withCursorMessage(
  fn: (msg: MessageHeader, account: string, folder: string) => Promise<void>,
): Promise<void> {
  const row = await ensureCursorRow();
  // A conversation's row means one of its messages here: the one the
  // reader's keys are on, or the newest.
  const one = isConversationRow(row) && !readingActive() ? await conversationTarget(row) : null;
  const msg = one ?? row;
  const where = one ? { account: one.account_name, folder: one.folder_name } : cursorLocation();
  if (!msg || !where) {
    setStatus("no message selected");
    return;
  }
  if (selectionSize() > 1 && !readingActive()) {
    setStatus(`acting on the message under the cursor, not the ${selectionSize()} selected`);
  }
  await fn(msg, where.account, where.folder);
}
/**
 * The message of a conversation a command for one message acts on: from the
 * reader when it shows the conversation, else its newest message.
 */
async function conversationTarget(row: MessageHeader): Promise<MessageHeader | null> {
  if (shownConversation()?.rowId === row.id) return conversationMessage() ?? null;
  const members = (await membersOf([row])).get(row.thread_id!) ?? [];
  return members[members.length - 1] ?? null;
}
/**
 * Re-read the pages under the viewport, replacing them only once the new data
 * arrives.
 *
 * Clearing the cache first would turn every visible row into a placeholder for
 * the length of a round trip — the black flash after each action. Fetching over
 * the top keeps the old rows on screen until there is something better to show.
 *
 * Deliberately does not re-read `total_count`: refreshing the sidebar means a
 * COUNT per folder across every account, far too heavy to run after every
 * keystroke. Staged actions do not change the row count anyway, and the one-row
 * drift from an immediate delete corrects on the next sync or folder switch.
 */
export async function refreshList(): Promise<void> {
  await settleWrites();
  // What was done to an item may take it out of Today's reading section.
  if (state.inboxFolders) await refreshTodayReading();
  // Search hits are not pages: the question is asked again instead.
  if (state.search) return refreshSearch();
  const source = pageSource();
  if (!source) return;
  const body = el.list.querySelector<HTMLElement>(".rows");
  if (!body) {
    // An empty folder renders no rows to refresh over, so there is nothing to
    // read pages into. Without this it could never fill up again: every later
    // push would find no rows and give up, and the folder would stay empty
    // until it was opened by hand.
    if (state.inboxFolders) {
      await loadInboxes();
      renderList();
      return;
    }
    return loadMessages(true);
  }

  const { first, last } = visibleRange(body);
  const generation = state.generation;
  // The visible pages, and the cursor's, which is not always among them.
  const wanted = new Set<number>();
  for (let page = Math.floor(first / PAGE_SIZE); page <= Math.floor(last / PAGE_SIZE); page++) {
    wanted.add(page);
  }
  wanted.add(Math.floor(state.messageCursor / PAGE_SIZE));

  // Everything is read before anything is replaced. A key pressed while the
  // pages are on their way acts on the rows on screen, with the cursor on the
  // message it is drawn on; replacing a page at a time would leave the cursor
  // pointing into new rows at its old index until the last one arrived.
  const fetched = new Map<number, MessageHeader[]>();
  let endSeen = false;
  for (const page of [...wanted].sort((a, b) => a - b)) {
    const rows = await fetchPage(source, page);
    if (generation !== state.generation) return;
    fetched.set(page, rows);
    if (rows.length < PAGE_SIZE) {
      endSeen = true; // folder ends here
      break;
    }
  }
  // A short page shrinks the count; mail that arrived by push has to grow
  // it, or the newest rows sit past the end of the list where no scroll
  // reaches them until the folder is opened again. The sidebar's count is
  // only consulted when no page reached the end: it may not have caught up
  // with an expunge yet, and growing back to it leaves rows that never load.
  const total = endSeen ? null : await sourceTotal();
  if (generation !== state.generation) return;

  // From here on nothing waits, so no key runs between the rows changing and
  // the cursor following its message. Which message that is is read now,
  // not before the fetch: the cursor may have moved while it ran.
  const anchorId = rowAt(state.messageCursor)?.id ?? null;
  const followerId = rowAt(state.messageCursor + 1)?.id ?? null;
  // Pages not read again would hold the rows where they were before the
  // change; dropped, they are read again when scrolled to.
  state.pages.clear();
  for (const [page, rows] of fetched) {
    state.pages.set(page, rows);
    noteTotalFromPage(page, rows.length);
  }
  if (total !== null) growTotal(total);
  settleCursor(anchorId, followerId);
  renderRows();
  followCursor();
}
/** Where a message currently sits, if it is in a page that is loaded. */
export function loadedIndexOf(id: number): number | null {
  for (const [page, rows] of state.pages) {
    const offset = rows.findIndex((row) => row.id === id);
    if (offset !== -1) return page * PAGE_SIZE + offset;
  }
  return null;
}
/**
 * Keep the cursor on the message it was on, across a refresh that moved things.
 *
 * The cursor is a row index, and the rows underneath it are not fixed: mail
 * arriving pushes everything down, mail filed elsewhere pulls everything up. An
 * index left alone therefore ends up pointing at a different message than the
 * one being read, silently — which matters most for the commands that act on
 * "the message under the cursor".
 *
 * Three outcomes, in order of preference. The message is still here, at a new
 * index: follow it. It is gone but the one that came after it is still here:
 * land on that, which is where a reader would expect to be after the mail they
 * were on was filed away. Neither is here — a run of messages went at once — so
 * keep the index, whose occupant is at least nearby, and show it.
 *
 * Keeping the index alone is not enough. It is right when only the selected
 * message goes, and wrong by exactly as many as were removed above it when
 * several go: reading the third message and having the first three filed
 * elsewhere would leave the cursor on the sixth.
 */
function settleCursor(anchorId: number | null, followerId: number | null): void {
  state.messageCursor = Math.min(state.messageCursor, Math.max(0, state.total - 1));

  if (state.total === 0) {
    // Nothing left to be on. Clearing the reader matters as much as clearing
    // the cursor: mail that is no longer in the folder must not go on being
    // displayed as though it were.
    state.detail = null;
    clearSelection();
    render();
    return;
  }
  if (anchorId === null) return;

  const now = loadedIndexOf(anchorId) ?? loadedIndexOf(followerId ?? -1);
  if (now === state.messageCursor) return;

  // Selected row indices refer to positions that have just moved, and the
  // commands that use them delete and move mail. Dropping the selection is the
  // only safe answer; acting on the wrong messages is not recoverable.
  if (selectionSize() > 0) {
    clearSelection();
    setStatus("selection cleared — the folder changed underneath it");
  }

  // Neither survived in a loaded page: whatever now occupies the index is the
  // closest thing to where the reader was. The reader follows either way.
  if (now !== null) {
    state.messageCursor = now;
    scrollCursorIntoView();
  }
}
/**
 * Apply a change locally, then send it, reverting if the server refuses.
 *
 * Waiting for an IMAP round trip before showing anything makes every action
 * feel unresponsive. The change is shown immediately and the affected rows are
 * marked unconfirmed — rendered dimmed — until the server agrees. A failure
 * restores exactly the previous state rather than leaving the row showing
 * something that never happened.
 */
/**
 * The local writes sent but not yet confirmed, as one chain: they reach the
 * core in the order they were made, which `stage_action` then `unstage_action`
 * on the same rows depends on, and nothing waits for them but what reads
 * their result back.
 */
let writes: Promise<unknown> = Promise.resolve();
/** Wait until every queued local write has landed or been rolled back. */
export function settleWrites(): Promise<unknown> {
  return writes;
}
/**
 * Change the rows on screen and queue the write behind them, without waiting
 * for it: what a key does to the list shows at once, and the next key is not
 * held up by the core. A write that fails is rolled back on screen, and `call`
 * has already said why. Only for writes the core makes locally — a staged
 * action, a flag — never for one that waits on the server.
 */
export function queuedWrite(
  targets: MessageHeader[],
  mutate: (m: MessageHeader) => void,
  send: () => Promise<unknown>,
): void {
  const before = writes;
  writes = optimisticWrite(targets, mutate, () => before.then(send)).catch(() => {});
}
export async function optimisticWrite(
  targets: MessageHeader[],
  mutate: (m: MessageHeader) => void,
  send: () => Promise<unknown>,
): Promise<boolean> {
  const snapshots = targets.map((m) => ({ row: m, before: { ...m } }));
  for (const { row } of snapshots) {
    mutate(row);
    state.unconfirmed.add(row.id);
  }
  renderRows();

  try {
    await send();
    return true;
  } catch {
    // `call` has already put the failure in the status line; undo the guess.
    for (const { row, before } of snapshots) Object.assign(row, before);
    return false;
  } finally {
    for (const { row } of snapshots) state.unconfirmed.delete(row.id);
    renderRows();
  }
}
