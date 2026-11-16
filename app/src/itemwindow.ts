/**
 * A message or a task in a window of its own, opened by a double-click on its
 * row (see `state.ts` for how the window knows which).
 *
 * The mailbox's page with a list of one row behind the reader: every command
 * acts on the cursor, and with the cursor on the one item, every key of the
 * reader and of the task detail works here as it does in the mailbox. The
 * window closes once its item is gone — moved, deleted, trashed.
 */

import { emitTo } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openMessage, showConversation } from "./actions";
import { call, setStatus } from "./bridge";
import { renderList } from "./list";
import { SearchHit } from "./model";
import { renderReader } from "./reader";
import { ITEM, state } from "./state";
import { TaskDetail } from "./tasklist";
import { loadTaskDetail, reloadTasks } from "./taskview";

/** Open a message or a task in a window of its own, or bring its window forward. */
export async function openItemWindow(kind: "message" | "task", id: number, title: string | null): Promise<void> {
  await call("open_item_window", { kind, id, title: title || (kind === "message" ? "(no subject)" : "(untitled)") });
}

/** Close this window, telling the mailbox first that its mail may have changed. */
export async function closeItemWindow(): Promise<void> {
  if (ITEM?.kind === "message") await noteMailChanged();
  await getCurrentWindow().close();
}

/**
 * Tell the mailbox to read its list again. The core says nothing of what a
 * window does to a message the server has not reported back yet — marked
 * read, staged, filed — so the mailbox would go on showing it as it was.
 */
export function noteMailChanged(): Promise<void> {
  return emitTo("main", "mail-changed").catch(() => {});
}

/** Fill this window with its item, the keyboard in the reader. */
export async function bootItemWindow(): Promise<void> {
  if (!ITEM) return;
  if (ITEM.kind === "task") {
    // A task changed anywhere is announced by the core, as `tasks-changed`.
    await bootTask();
    return;
  }
  await bootMessage(ITEM.id);
  // What the mailbox did to the message meanwhile.
  window.addEventListener("focus", () => void refreshItemMessage().catch(() => {}));
  window.addEventListener("blur", () => void noteMailChanged());
}

async function bootMessage(id: number): Promise<void> {
  const own = await ownHit(id);
  if (!own) {
    setStatus("that message is no longer here");
    return;
  }
  showConversation([own], id, own.message.subject ?? null);
  await openMessage(true);
}

async function bootTask(): Promise<void> {
  // A view the task commands accept, with nothing particular to it; which
  // rows it holds is `reloadItemTask`'s answer.
  state.tasks.view = "someday";
  state.tasks.section = "tasks";
  await reloadTasks();
  if (state.tasks.rows.length === 0) return;
  await loadTaskDetail(true);
}

/** The message as a row, or null once it is no longer anywhere. */
async function ownHit(id: number): Promise<SearchHit | null> {
  const hits = await call<SearchHit[]>("thread_messages", { id });
  return hits.find((h) => h.message.id === id) ?? null;
}

/** Whether the task was in the trash when the window opened, and so may stay there. */
let openedTrashed: boolean | null = null;

/**
 * The task window's one row, for `reloadTasks`: the task as it is now. A
 * task that has gone, or gone to the trash since the window opened, closes
 * the window.
 */
export async function reloadItemTask(): Promise<void> {
  if (ITEM?.kind !== "task") return;
  let detail: TaskDetail;
  try {
    detail = await call<TaskDetail>("get_task", { id: ITEM.id });
  } catch (e) {
    if (e && typeof e === "object" && "NotFound" in e) await closeItemWindow();
    return;
  }
  openedTrashed ??= detail.task.deleted_at !== null;
  if (detail.task.deleted_at !== null && !openedTrashed) {
    await closeItemWindow();
    return;
  }
  state.tasks.rows = [detail.task];
  state.tasks.cursor = 0;
  state.tasks.selected.clear();
  state.tasks.anchor = null;
}

/**
 * The message window's one row, for `refreshSearch`: the message as it is
 * now, or the window closed once it has left its folder.
 */
export async function refreshItemMessage(): Promise<void> {
  if (ITEM?.kind !== "message" || !state.search) return;
  const search = state.search;
  const own = await ownHit(ITEM.id);
  if (state.search !== search) return;
  if (!own) {
    await closeItemWindow();
    return;
  }
  search.hits = [own];
  state.total = 1;
  renderList();
  renderReader();
}
