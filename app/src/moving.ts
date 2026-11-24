import { leaveChat, refreshChat } from "./chat";
import { openPicker } from "./picker";
import { runTask } from "./tasks";
import { currentFolder, loadMessages, move, openMessage } from "./actions";
import { call, describeWrite, setStatus } from "./bridge";
import { listScroller, render, renderList, rowAt } from "./list";
import { FolderInfo, PendingExecution, SearchHit, SortKey, TransferOutcome, WriteOutcome, sortOrder } from "./model";
import { showPendingReview } from "./panels";
import { renderReader } from "./reader";
import { syncSearchBox } from "./search";
import { clearSelection, selectionSize } from "./selection";
import { folderEntry, leaveTaskViewForSearch, refreshFolders, refreshPendingCounts, renderFolders, selectableFolders } from "./sidebar";
import { ITEM, SearchKind, el, openPanel, state, todayHasMail } from "./state";
import { refreshItemMessage } from "./itemwindow";
import { describeCount } from "./sync";
import { applyFilter, dropFilter } from "./filter";
import { Scope, scopeArg } from "./searchfield";
import { TaskSummary } from "./tasklist";
import { previewTaskCursor, refreshTasks, showSearchKind } from "./taskview";
import { refreshList, settleWrites, targetMessages, withMessages } from "./targets";
import { isReadingFolder } from "./reading";

/**
 * Say what an action did, and that ⌘Z takes it back.
 *
 * The action itself is recorded in the core, which keeps one history for
 * every window and every kind of action; this is only the status line. Inside
 * a chain the lines are collected, and said together when the chain ends.
 */
export function offerUndo(status: string): void {
  if (group) group.said.push(status);
  else setStatus(`${status} · ⌘Z to undo`);
}
/** The lines said by the chain being run, while one runs. */
let group: { said: string[] } | null = null;
/**
 * Make every action recorded until `end` one, for a chain of commands run by
 * one key: one ⌘Z takes the whole key back, last step first, and one status
 * line says what the key did. `end` takes the whole's label, which otherwise
 * joins its parts'.
 */
export async function groupUndo(): Promise<(label?: string) => Promise<void>> {
  const outer = group;
  const mine = { said: [] as string[] };
  group = mine;
  const mark = await call<number>("begin_undo_group");
  return async (label?: string) => {
    group = outer;
    // A queued write records its action when it runs, which may be after
    // the chain has moved on.
    await settleWrites();
    await call("end_undo_group", { mark, label });
    if (mine.said.length === 0) return;
    if (outer) outer.said.push(...mine.said);
    else setStatus(`${mine.said.join(" · ")} · ⌘Z to undo`);
  };
}
/** What an undo or a redo did. */
interface UndoReport {
  label: string;
  said: string;
}
/** Take back the last action of any kind, or with `redo`, do again what was taken back. */
export async function undoLast(redo = false): Promise<void> {
  await settleWrites();
  const report = await call<UndoReport | null>(redo ? "redo" : "undo");
  if (!report) {
    setStatus(redo ? "nothing to redo" : "nothing to undo");
    return;
  }
  await refreshAfterUndo();
  const said = report.said ? ` — ${report.said}` : "";
  setStatus(`${redo ? "redone" : "undone"}: ${report.label}${said}`);
}
/** Redraw whatever an undo may have changed: mail, tasks and chat alike. */
async function refreshAfterUndo(): Promise<void> {
  await refreshList();
  await refreshFolders();
  if (state.tasks.available) await refreshTasks();
  await refreshChat();
}
/**
 * Say what a move did, and that it can be undone when it can: a move whose
 * messages carry no `Message-ID` cannot be found again to be moved back.
 */
export function sayMoved(said: string, outcome: WriteOutcome): void {
  const status = describeWrite(said, [outcome.warning]);
  if ((outcome.moved ?? []).length > 0) offerUndo(status);
  else setStatus(status);
}
/** The special folders of an account, as configured. */
export function accountFolders(account: string): { trash: string; archive: string; junk: string } {
  const info = state.accounts.find((a) => a.name === account);
  return {
    trash: info?.trash_folder ?? "Trash",
    archive: info?.archive_folder ?? "Archive",
    junk: info?.junk_folder ?? "Junk",
  };
}
/** The folders an account has, by their server-side names. */
function foldersOf(account: string): FolderInfo[] {
  return state.folders.filter(
    (f) => f.account_name === account && !f.is_account_header && !f.is_pinned && f.folder_name,
  );
}
/**
 * Move the targets now, to wherever `target` names for their account, and
 * offer to undo it.
 *
 * `target` is asked per account because a search result spans accounts and
 * each has its own archive. `setting` names the config key to mention when
 * the folder is not there, so the status line says what to change.
 */
export async function moveNow(
  target: (account: string, folder: string) => string | null,
  setting?: string,
): Promise<void> {
  await withMessages(
    async (t, account, folder) => {
      const to = target(account, folder);
      if (!to) {
        setStatus(`${describeCount(t.count)} not in ${accountFolders(account).junk}; nothing to do`);
        return false;
      }
      if (to === folder) {
        setStatus(`already in ${to}`);
        return false;
      }
      if (!foldersOf(account).some((f) => f.folder_name === to)) {
        setStatus(
          `${account} has no folder '${to}'${setting ? `; set ${setting} in config.yml` : ""}`,
        );
        return false;
      }
      const outcome = await call<WriteOutcome>("move_messages", {
        account,
        folder,
        target: to,
        uids: t.uids,
        undo: `move ${describeCount(t.count)} to ${to}`,
      });
      await refreshList();
      sayMoved(`moved ${describeCount(t.count)} to ${to}`, outcome);
      return undefined;
    },
    true,
  );
}
/**
 * Move or copy the targets into a folder picked from every account. Across
 * accounts the messages travel through this machine, whole, so the status
 * says how many arrived; a move deletes from the source only what arrived.
 */
export async function transferAcross(keep: boolean): Promise<void> {
  const targets = await targetMessages();
  if (!targets || targets.groups.length === 0) {
    setStatus("no message selected");
    return;
  }
  const items = state.folders
    .filter((f) => !f.is_account_header && !f.is_pinned && f.folder_name && !isReadingFolder(f))
    .map((f) => ({
      value: `${f.account_name}\u0000${f.folder_name}`,
      label: `${f.account_name} / ${f.display_name}`,
      detail: f.display_name === f.folder_name ? undefined : f.folder_name!,
    }));
  const picked = await openPicker({
    title: keep ? "Copy to any account" : "Move to any account",
    placeholder: "account / folder",
    items,
  });
  if (!picked) return;
  const [toAccount, toFolder] = picked.split("\u0000") as [string, string];
  await transferTo(toAccount, toFolder, keep);
}
/** Move or copy the targets into a folder of any account, as `transferAcross` does once one is picked. */
export async function transferTo(toAccount: string, toFolder: string, keep: boolean): Promise<void> {
  let transferred = 0;
  let missing = 0;
  const warnings: (string | null)[] = [];
  const where = `${toAccount} / ${toFolder}`;
  const endGroup = await groupUndo();
  try {
    await withMessages(
      async (t, account, folder) => {
        if (account === toAccount && folder === toFolder) {
          setStatus(`already in ${where}`);
          return false;
        }
        const outcome = await runTask(
          {
            id: "transfer",
            label: keep ? "Copying across accounts" : "Moving across accounts",
            detail: `${describeCount(t.count)} to ${where}`,
            warnOnQuit: "messages are being transferred between accounts",
          },
          () =>
            call<TransferOutcome>("transfer_messages", {
              fromAccount: account,
              fromFolder: folder,
              toAccount,
              toFolder,
              uids: t.uids,
              keepSource: keep,
              undo: `${keep ? "copy" : "move"} ${describeCount(t.count)} to ${where}`,
            }),
        );
        transferred += outcome.transferred;
        missing += outcome.missing;
        warnings.push(outcome.warning);
        return undefined;
      },
      !keep,
    );
  } finally {
    await endGroup(`${keep ? "copy" : "move"} ${describeCount(transferred)} to ${where}`);
  }
  await refreshList();
  const gone = missing ? `; ${missing} no longer on the server` : "";
  const said = describeWrite(`${keep ? "copied" : "moved"} ${describeCount(transferred)} to ${where}${gone}`, warnings);
  if (transferred > 0) offerUndo(said);
  else setStatus(said);
}
/**
 * Ask for a folder of the account the targets belong to.
 *
 * Targets from several accounts cannot share a destination, since a move
 * never crosses accounts; the picker then refuses rather than guess.
 */
export async function pickFolder(title: string): Promise<string | null> {
  const targets = await targetMessages();
  if (!targets || targets.groups.length === 0) {
    setStatus("no message selected");
    return null;
  }
  const accounts = new Set(targets.groups.map((g) => g.account));
  if (accounts.size > 1) {
    setStatus("the selection spans accounts; a move stays within one");
    return null;
  }
  const account = targets.groups[0]!.account;
  const current = targets.groups[0]!.folder;
  const items = foldersOf(account)
    .filter((f) => f.folder_name !== current && !isReadingFolder(f))
    .map((f) => ({
      value: f.folder_name!,
      label: f.display_name,
      detail: f.display_name === f.folder_name ? undefined : f.folder_name!,
    }));
  if (items.length === 0) {
    setStatus(`${account} has no other folder to move to`);
    return null;
  }
  return openPicker({ title: `${title} · ${account}`, placeholder: "folder", items });
}
/**
 * Apply the staged actions of one folder, of several — Today's inboxes — or,
 * with `null`, of everything, as one task.
 */
export async function applyPending(
  folders: { account: string; folder: string }[] | null,
  label: string,
): Promise<void> {
  // Not cancellable: the actions are applied to the server one after another
  // and stopping midway would leave the staged set half-applied.
  await settleWrites();
  const execution = await runTask(
    {
      id: "expunge",
      label: "Applying staged actions",
      detail: label,
      warnOnQuit: "staged actions are being applied to the server",
    },
    async () => {
      const undo = "apply staged actions";
      if (!folders) return call<PendingExecution>("execute_pending_all", { undo });
      const total: PendingExecution = { executed: 0, warnings: [] };
      const endGroup = await groupUndo();
      try {
        for (const { account, folder } of folders) {
          const one = await call<PendingExecution>("execute_pending", { account, folder, undo });
          total.executed += one.executed;
          total.warnings.push(...one.warnings);
        }
      } finally {
        await endGroup(undo);
      }
      return total;
    },
  );
  await refreshPendingCounts();
  await refreshList();
  renderFolders();
  if (openPanel()?.title === "Staged actions") await showPendingReview();
  const applied = execution.executed;
  const said = describeWrite(
    `executed ${applied} staged action${applied === 1 ? "" : "s"}${folders ? "" : " everywhere"}`,
    execution.warnings,
  );
  if (applied > 0) offerUndo(said);
  else setStatus(said);
}
/** Subscribe to the open folder, or unsubscribe from it, for the clients that list only those. */
export async function setSubscribed(subscribed: boolean): Promise<void> {
  const folder = currentFolder();
  if (!folder?.folder_name) {
    setStatus("open a folder to subscribe to it");
    return;
  }
  await call("set_folder_subscribed", {
    account: folder.account_name,
    name: folder.folder_name,
    subscribed,
  });
  offerUndo(`${subscribed ? "subscribed to" : "unsubscribed from"} ${folder.display_name}`);
}
/** Open the folder at `index` of the sidebar's folders, wrapping around. */
export async function openFolderAt(index: number): Promise<void> {
  const folders = selectableFolders();
  if (folders.length === 0) return;
  const at = ((index % folders.length) + folders.length) % folders.length;
  state.tasks.view = null;
  state.tasks.detail = null;
  leaveChat();
  state.openFolder = at;
  state.sidebarCursor = folderEntry(at);
  syncSearchBox();
  await loadMessages();
  state.focus = "list";
  render();
  setStatus(`${folders[at]!.account_name}/${folders[at]!.display_name}`);
}
/**
 * Walk the search's matches: the marks in the reader when it has them, else
 * the result rows. HTML mail is rendered in a frame this window cannot look
 * into, so there only the headers' marks are walked and the body's are not;
 * past the last mark, the next match is the next message.
 */
export function nextMatch(delta: number): void {
  if (!state.search) {
    setStatus("no search to walk; / searches");
    return;
  }
  const marks = Array.from(el.reader.querySelectorAll<HTMLElement>("pre mark, h1 mark, .hdr mark"));
  if (marks.length > 0) {
    const next = state.matchIndex + delta;
    if (next >= 0 && next < marks.length) {
      marks[state.matchIndex]?.classList.remove("current");
      state.matchIndex = next;
      marks[next]!.classList.add("current");
      marks[next]!.scrollIntoView({ block: "center" });
      setStatus(`match ${next + 1} of ${marks.length}`);
      return;
    }
  }
  // Past the last mark of this message, or none: the next result.
  const before = state.messageCursor;
  move(delta > 0 ? 1 : -1);
  if (state.messageCursor === before) setStatus(delta > 0 ? "last result" : "first result");
}
/**
 * Run a query and show its hits as the list. `label` is what the search row
 * and the status call it — a saved search goes by its name.
 */
export async function runSearchQuery(
  query: string,
  label = query,
  view?: number | "must-read",
  scope: Scope = { kind: "all" },
  kind: SearchKind = "all",
): Promise<void> {
  // Tasks are in no folder, so a search for them alone looks everywhere.
  if (kind === "tasks") scope = { kind: "all" };
  // A typed search looks everywhere (§4.5): the tasks too, and in the tasks
  // window only them. A saved search is a mail view and stays one, and so is
  // a search narrowed to an account or to folders, which tasks are in none of.
  // Both kinds are asked for whatever `kind` shows, so the results' switch
  // shows the other at once.
  const typed = view === undefined && label === query && scope.kind === "all";
  // A new search is a new list; the filter on the old one goes with it.
  dropFilter();
  const mailToo = todayHasMail();
  const findTasks = typed && state.tasks.available;
  const from = state.search
    ? state.search.from
    : state.tasks.view && state.tasks.view !== "search"
      ? state.tasks.view
      : null;
  // A search over a warm index returns in milliseconds and the task is never
  // shown; the same search over a cold 200k-message archive reveals itself.
  // The order the config starts a search in; one by meaning is closest first.
  const sort = query.includes("~") ? undefined : (state.settings.searchSort ?? undefined);
  const [found, tasks] = await runTask({ id: "search", label: "Searching", detail: label }, () =>
    Promise.all([
      mailToo
        ? call<SearchHit[]>("search", { query, scope: scopeArg(scope), sort: sort ? sortOrder(sort) : null })
        : Promise.resolve([] as SearchHit[]),
      findTasks
        ? call<TaskSummary[]>("search_tasks", { query }).catch(() => [] as TaskSummary[])
        : Promise.resolve([] as TaskSummary[]),
    ]),
  );
  let hits = found;
  leaveTaskViewForSearch();

  // Exact search found nothing, so try meaning. A misspelt word matches
  // nothing at all by index — "subscriptiion" is not "subscription" — while
  // the embedding of it still lands next to the right messages, because the
  // tokenizer breaks an unknown word into pieces the correct spelling shares.
  //
  // Only ever a fallback: it cannot dilute a search that did find something,
  // and a query that already asked for meaning is not asked twice. A saved
  // search is a question asked as written, and gets no second guess.
  let byMeaning = false;
  const shown = !mailToo ? "tasks" : findTasks ? kind : "mail";
  if (mailToo && shown !== "tasks" && hits.length === 0 && (shown === "mail" || tasks.length === 0) && !query.includes("~") && label === query) {
    const fuzzy = await runTask(
      { id: "search", label: "Searching by meaning", detail: query },
      () => call<SearchHit[]>("search", { query: `~${query}`, scope: scopeArg(scope) }),
    );
    if (fuzzy.length > 0) {
      hits = fuzzy;
      byMeaning = true;
    }
  }
  // Anything still on its way for the folder or the previous search lands
  // on a list it no longer belongs to; the generation says so.
  state.generation += 1;
  state.pages.clear();
  state.fetching.clear();
  const terms =
    (await call<string[]>("highlight_terms", { query }).catch(() => [] as string[])) ?? [];
  state.search = {
    query: label,
    effective: byMeaning ? `~${query}` : query,
    hits,
    terms,
    view,
    tasks,
    taskQuery: findTasks ? query : null,
    kind: shown,
    completedOpen: false,
    from,
    scope,
    sort: byMeaning ? undefined : sort,
  };
  state.messageCursor = 0;
  state.detail = null;
  clearSelection();
  state.total = hits.length;
  state.focus = "list";
  state.tasks.cursor = 0;
  state.tasks.rows = [];
  showSearchKind();
  // The results open on the first message, or on the first task when no
  // message was found.
  state.tasks.section = hits.length > 0 && state.search.kind !== "tasks" ? "mail" : "tasks";
  listScroller().scrollTop = 0;
  render();
  if (state.tasks.view === "search" && state.tasks.section === "tasks") previewTaskCursor();
  // Say when the results are approximate. Silently showing near-misses as
  // though they were matches is how a search stops being trustworthy.
  const n = (shown === "tasks" ? 0 : hits.length) + (shown === "mail" ? 0 : tasks.length);
  setStatus(
    n
      ? `${n} result${n === 1 ? "" : "s"} for ${label}` +
          (byMeaning ? " — no exact match, closest by meaning" : "")
      : `no results for ${label}`,
  );
}
/**
 * Order the open search by `key` — `relevance` for the query's own order —
 * and run it again, from the top: the result limit applies after the order,
 * so it can be a different set of messages, not only another order of them.
 */
export async function sortSearch(key: SortKey | "relevance", dir?: 1 | -1): Promise<void> {
  const search = state.search;
  if (!search?.effective) {
    setStatus("a conversation is in the order it was written");
    return;
  }
  if (key === "relevance") {
    search.sort = undefined;
  } else if (dir !== undefined) {
    search.sort = { key, dir };
  } else if (search.sort?.key === key) {
    search.sort = { key, dir: search.sort.dir === 1 ? -1 : 1 };
  } else {
    search.sort = { key, dir: key === "date" ? -1 : 1 };
  }
  const hits = await call<SearchHit[]>("search", {
    query: search.effective,
    sort: search.sort ? sortOrder(search.sort) : null,
    scope: scopeArg(search.scope),
  });
  if (state.search !== search) return;
  search.hits = hits;
  state.total = search.kind === "tasks" ? 0 : hits.length;
  state.messageCursor = 0;
  clearSelection();
  listScroller().scrollTop = 0;
  render();
  await openMessage(false);
}
/**
 * Re-run the open search after the mailbox changed under it, keeping the
 * cursor on the message it was on.
 */
export async function refreshSearch(): Promise<void> {
  if (ITEM?.kind === "message") return refreshItemMessage();
  const search = state.search;
  if (!search) return;
  // A conversation is not a query: it is left as it was shown.
  if (!search.effective) return;
  const anchorId = rowAt(state.messageCursor)?.id ?? null;
  const hits = await call<SearchHit[]>("search", {
    query: search.effective,
    sort: search.sort ? sortOrder(search.sort) : null,
    scope: scopeArg(search.scope),
  });
  if (state.search !== search) return;
  search.hits = hits;
  state.total = search.kind === "tasks" ? 0 : hits.length;
  if (state.filter?.text.trim()) return applyFilter();
  const at = anchorId === null ? -1 : hits.findIndex((h) => h.message.id === anchorId);
  if (at >= 0) state.messageCursor = at;
  else {
    state.messageCursor = Math.min(state.messageCursor, Math.max(0, hits.length - 1));
    if (anchorId !== null && selectionSize() > 0) {
      clearSelection();
      setStatus("selection cleared — the results changed underneath it");
    }
  }
  renderList();
  if (hits.length === 0) {
    state.detail = null;
    renderReader();
  } else if (at < 0 && anchorId !== null) {
    await openMessage(false);
  }
}
