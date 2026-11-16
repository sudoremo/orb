import { keepingScroll } from "./scrollkeep";
import { isConversationRow } from "./conversation";
import { leaveChat } from "./chat";
import { loadTodayReading } from "./reading";
import { emitTo } from "@tauri-apps/api/event";
import { TaskSummary, TaskDetail, TaskListView, TaskViewName, MessageTaskCount, LinkedMessage, describeTasks, distinctTasks, doneForTodayNote, groupByTag, isDueNow, isPaged } from "./tasklist";
import { localToday } from "./dates";
import { runTask } from "./tasks";
import { PREVIEW_DEBOUNCE, clamp, previewCursor, scrollCursorIntoView, scrollTaskCursorIntoView } from "./actions";
import { Cancelled, call, setStatus } from "./bridge";
import { offerUndo } from "./moving";
import { activeBindings } from "./commands";
import { emptyView } from "./empty";
import { applyListSectionClass, inboxTotal, reloadPages, render, renderList, renderRows, renderTaskSection, rowAt } from "./list";
import { FolderInfo } from "./model";
import { renderReader } from "./reader";
import { syncSearchBox } from "./search";
import { clearSelection } from "./selection";
import { removeSetting, readSetting, writeSetting } from "./splitters";
import { renderSteady } from "./statusline";
import { renderToolbar } from "./toolbar";
import { applyFilter, clearFilter, dropFilter, filterTasks, setFilter } from "./filter";
import { hasMailHalf, inTaskView, renderFolders, selectableFolders, tagFilter, taskEntryIndex, taskSectionActive } from "./sidebar";
import { DAY_WATCH_MS, FETCH_DEBOUNCE, ITEM, WINDOW, el, state, todayHasMail } from "./state";
import { openItemWindow, reloadItemTask } from "./itemwindow";
import { targetMessages } from "./targets";
import { beginTaskEdit, pickDate } from "./taskedit";
import { firstSection, todayKeeps } from "./today";
import { TodayLayout, layoutBeforeMixed, storeTodayLayout, todayLayout, todayMixed } from "./layout";
import { placeTimeline } from "./mixed";
import { setTodayKept } from "./panes";

/** How many rows one page of the Completed view holds; matches the core. */
const COMPLETED_PAGE_SIZE = 50;
/** Redraw only the task section, leaving the mail list beside it untouched. */
export function renderTaskList(): void {
  const view = state.tasks.view;
  if (!view) return;
  applyListSectionClass();
  const section = el.list.querySelector<HTMLElement>(".tk-section");
  // Becoming empty, or ceasing to be, changes the whole window: the reader
  // pane goes or comes back.
  if (!section || emptyView() !== null) {
    renderList();
    return;
  }
  // Replacing this subtree alone matters in Today: rebuilding the whole pane
  // would throw away the mail list's pooled row elements on every `j`.
  keepingScroll(el.list, () => (section.outerHTML = renderTaskSection(view)));
}
/**
 * Move the cursor and the selection marks without rebuilding the list.
 *
 * For everything that changes only which row is highlighted. Completed can run
 * to several hundred rows, and re-parsing that markup on every keystroke is the
 * same mistake the message list has a row pool to avoid — the difference is
 * just that a task list is small enough for two class toggles to be the whole
 * fix.
 */
export function paintTaskCursor(): void {
  applyListSectionClass();
  renderSteady();
  const rows = el.list.querySelectorAll<HTMLElement>("[data-task]");
  rows.forEach((row, index) => {
    row.classList.toggle("cursor", index === state.tasks.cursor);
    row.classList.toggle("selected", state.tasks.selected.has(index));
  });
}
/**
 * Load the open view's rows.
 *
 * The search string goes to the core rather than being applied here: the core
 * already searches title, body and tag names and understands `tag:` (§4.5), and
 * a filter written a second time on this side would be a second answer to what
 * a search means.
 */
export async function reloadTasks(): Promise<void> {
  if (ITEM?.kind === "task") return reloadItemTask();
  const view = state.tasks.view;
  if (!view) return;
  if (view === "search") {
    const search = state.search;
    if (!search?.taskQuery) return;
    const tasks = await call<TaskSummary[]>("search_tasks", { query: search.taskQuery });
    if (state.search !== search) return;
    search.tasks = tasks;
    showSearchKind();
    return;
  }
  const search = state.tasks.search.trim();
  const paged = isPaged(view);
  const pages = paged ? state.tasks.page + 1 : 1;

  // Completed accumulates: page N is a window, and the view is every window
  // asked for so far. Re-reading them all keeps the list correct after a
  // mutation without having to work out which page a task moved between.
  const rows: TaskSummary[] = [];
  let short = true;
  for (let page = 0; page < pages; page++) {
    const batch = await call<TaskSummary[]>(
      "list_tasks",
      {
        view,
        search: search || null,
        page: paged ? page : null,
      },
    );
    rows.push(...batch);
    short = batch.length < COMPLETED_PAGE_SIZE;
  }

  // A task that is still being named stays in the list it was made in, whatever
  // the view's predicate says about it. `create-task` deliberately does not
  // navigate, so in a view a blank task does not belong to — Upcoming, where it
  // has no dates and is therefore a Someday task — the row would otherwise be
  // created and immediately vanish, leaving a title field editing something
  // invisible. It goes wherever it really belongs the moment it is named.
  if (state.tasks.provisional !== null && !rows.some((row) => row.id === state.tasks.provisional)) {
    const pinned = await call<TaskDetail | null>(
      "get_task",
      { id: state.tasks.provisional },
    ).catch(() => null);
    if (pinned) rows.push(pinned.task);
    // Gone from under us — trashed by a sync, say. Nothing to pin and nothing
    // to type into, so the flag goes too rather than being retried every reload.
    else state.tasks.provisional = null;
  }

  const ordered = view === "today" ? todayOrder(rows) : rows;
  replaceTaskRows(groupedByTag(view) ? groupByTag(ordered, state.tasks.tags) : ordered);
  state.tasks.exhausted = !paged || short;
  state.tasks.cursor = clamp(state.tasks.cursor, state.tasks.rows.length - 1);
}
/**
 * Put new rows in the list, the selection still on the tasks it was on.
 *
 * The selection is held as positions, and a reload — after a completion, a
 * drag, a sync — moves tasks about. Carried over by id, a selection outlives
 * what is done to it, and a task that has left the view leaves it too.
 */
function replaceTaskRows(rows: TaskSummary[]): void {
  // A task under two tags is two rows, told apart by the group (§30).
  const key = (row: TaskSummary | undefined) => row && `${row.id}:${row.tag_group ?? ""}`;
  const keyAt = (index: number | null) => (index === null ? undefined : key(state.tasks.rows[index]));
  const selected = new Set([...state.tasks.selected].map(keyAt));
  const anchor = keyAt(state.tasks.anchor);
  state.tasks.rows = rows;
  state.tasks.selected.clear();
  rows.forEach((row, index) => {
    if (selected.has(key(row))) state.tasks.selected.add(index);
  });
  const at = rows.findIndex((row) => key(row) === anchor);
  state.tasks.anchor = at >= 0 ? at : null;
}
/**
 * Point the task half at the search's tasks, or away from them, for the kind
 * of results on screen (§26).
 *
 * The tasks found stand in for a view, so that every task command — complete,
 * open, edit, postpone — works on them as on any list of tasks. Mail alone,
 * or a search that found no task, is the plain mail search it always was.
 */
export function showSearchKind(): void {
  const search = state.search;
  if (!search) return;
  if (search.kind === "mail" || search.tasks.length === 0) {
    if (state.tasks.view === "search") state.tasks.view = null;
    state.tasks.rows = [];
    state.tasks.section = "mail";
    state.total = search.hits.length;
    return;
  }
  const keep = state.tasks.rows[state.tasks.cursor]?.id;
  if (state.tasks.view !== "search") {
    state.tasks.selected.clear();
    state.tasks.anchor = null;
  }
  state.tasks.view = "search";
  replaceTaskRows(filterTasks(search.completedOpen ? search.tasks : search.tasks.filter((t) => !t.is_completed)));
  state.tasks.exhausted = true;
  const at = state.tasks.rows.findIndex((t) => t.id === keep);
  state.tasks.cursor = at >= 0 ? at : clamp(state.tasks.cursor, state.tasks.rows.length - 1);
  // The mail of a search showing only tasks is out of the list's reach.
  state.total = search.kind === "tasks" ? 0 : search.hits.length;
  if (state.total === 0) state.tasks.section = "tasks";
}

/** Unfold the completed tasks a search found, or fold them away (§26). */
export function toggleSearchCompleted(): void {
  const search = state.search;
  if (!search || state.tasks.view !== "search") return;
  const done = search.tasks.filter((t) => t.is_completed).length;
  if (done === 0) {
    setStatus("no completed task among the results");
    return;
  }
  search.completedOpen = !search.completedOpen;
  showSearchKind();
  renderTaskList();
  paintTaskCursor();
}

/** Whether Today lists its completed tasks above the open ones (§24), as the config says. */
export function completedFirst(): boolean {
  return state.settings.completedFirst;
}
const doneInToday = (t: TaskSummary) => t.is_completed || !!t.is_done_for_today;
/**
 * Which band of Today a task stands in: the completed ones first under
 * Completed first (§24), then those due by today under Due first (§31), then
 * the rest.
 */
function todayBand(t: TaskSummary): number {
  if (completedFirst() && doneInToday(t)) return 0;
  return dueFirst("today") && isDueNow(t, state.tasks.today) ? 1 : 2;
}
/** Today's tasks band by band, each band in the global order. */
function todayOrder(rows: TaskSummary[]): TaskSummary[] {
  return [0, 1, 2].flatMap((band) => rows.filter((t) => todayBand(t) === band));
}
/**
 * Whether two rows of the list stand in the global order relative to each
 * other. Every row does, except across the bands Completed first and Due
 * first split Today into: a reorder takes its neighbours from its own band.
 */
export function sameOrderGroup(a: TaskSummary, b: TaskSummary): boolean {
  if (a.tag_group !== b.tag_group) return false;
  return state.tasks.view !== "today" || todayBand(a) === todayBand(b);
}

const DUE_FIRST = ".todayDueFirst";
/** Whether Due first is switched on, whichever view is open (§31). */
export function dueFirstOn(): boolean {
  return readSetting(DUE_FIRST) === "on";
}
/** Whether the view lists the tasks due by today above the rest (§31): Today alone. */
export function dueFirst(view: TaskListView | null): boolean {
  return view === "today" && dueFirstOn();
}
/** How many open tasks of the list are due today or earlier (§31). */
export function dueNowCount(): number {
  return distinctTasks(state.tasks.rows.filter((t) => isDueNow(t, state.tasks.today)));
}
/** List Today's tasks due by today above the rest, or not (§31), the cursor kept on its task. */
export async function setDueFirst(on: boolean): Promise<void> {
  if (on) writeSetting(DUE_FIRST, "on");
  else removeSetting(DUE_FIRST);
  await regroup();
}

const BY_TAG = ".tasksByTag";
/** The views that can be grouped by tag (§30): those of open tasks. */
export function canGroupByTag(view: TaskListView | null): boolean {
  return view === "today" || view === "upcoming" || view === "someday";
}
/** Whether the view's tasks stand under their tags (§30), as switched on this Mac. */
export function groupedByTag(view: TaskListView | null): boolean {
  return canGroupByTag(view) && groupByTagOn();
}
/** Whether grouping by tag is switched on, whichever view is open. */
export function groupByTagOn(): boolean {
  return readSetting(BY_TAG) === "on";
}
/**
 * Group the open-task views by tag, or list them as before (§30), the cursor
 * kept on the task it was on.
 */
export async function setGroupByTag(on: boolean): Promise<void> {
  if (on) writeSetting(BY_TAG, "on");
  else removeSetting(BY_TAG);
  await regroup();
}
/** Read the list again after its grouping changed, the cursor on the task it was on. */
async function regroup(): Promise<void> {
  const keep = state.tasks.rows[state.tasks.cursor]?.id;
  await reloadTasks();
  const at = state.tasks.rows.findIndex((row) => row.id === keep);
  if (at >= 0) state.tasks.cursor = at;
  renderTaskList();
  paintTaskCursor();
  scrollTaskCursorIntoView();
}
/**
 * Hide the tasks completed so far from Today until midnight, on every
 * machine (§10.2). Nothing is written to them: the cut-off travels as a
 * message of its own, and `tasks-changed` brings every window along.
 */
export async function clearCompletedFromToday(): Promise<void> {
  const shown =
    state.tasks.view === "today"
      ? distinctTasks(state.tasks.rows.filter((t) => t.is_completed || t.is_done_for_today))
      : null;
  await call("clear_completed_from_today");
  setStatus(
    shown === null
      ? "completed tasks cleared from Today"
      : `${describeTasks(shown)} cleared from Today · Completed still has them`,
  );
}
/** Add the next page of Completed to what is already on screen (§10.2). */
export async function loadMoreCompleted(): Promise<void> {
  const view = state.tasks.view;
  if (!isPaged(view) || state.tasks.exhausted) return;
  const page = state.tasks.page + 1;
  const batch = await call<TaskSummary[]>(
    "list_tasks",
    { view, search: state.tasks.search.trim() || null, page },
  );
  state.tasks.page = page;
  state.tasks.rows = [...state.tasks.rows, ...batch];
  state.tasks.exhausted = batch.length < COMPLETED_PAGE_SIZE;
  renderTaskList();
}
/** Re-read the Today badge. Cheap, and wrong the moment anything changes. */
export async function refreshTaskBadge(): Promise<void> {
  if (!state.tasks.available) return;
  state.tasks.badge = await call<number>("today_task_count");
  state.tasks.workingOn = (await call<TaskSummary[]>("working_on_tasks").catch(() => [])) ?? [];
  renderSteady();
}
/**
 * The inbox folders, one per account, with the pinned duplicates dropped.
 *
 * Pinned entries are the same folder listed twice, so counting them would make
 * All Inboxes show every message from those accounts twice — which is the kind
 * of duplicate that looks like a sync bug.
 */
export function inboxFolders(): FolderInfo[] {
  const seen = new Set<string>();
  const inboxes: FolderInfo[] = [];
  for (const folder of selectableFolders()) {
    const name = folder.folder_name ?? "";
    const leaf = name.split(/[./]/).pop() ?? name;
    if (leaf.toUpperCase() !== "INBOX") continue;
    const key = `${folder.account_name}\u0000${name}`;
    if (seen.has(key)) continue;
    seen.add(key);
    inboxes.push(folder);
  }
  return inboxes;
}
const TODAY_ACCOUNT = ".todayMailAccount";
/**
 * The account whose inbox Today's mail shows, or null for every account's.
 * Remembered on this machine; an account no longer configured counts as
 * none chosen.
 */
export function todayAccount(): string | null {
  const chosen = readSetting(TODAY_ACCOUNT);
  return chosen && inboxFolders().some((f) => f.account_name === chosen) ? chosen : null;
}
/**
 * Whether Today stands in columns: the mail or the timeline on the left, the
 * rest on the right (§4.2). Columns, and Mixed, whose left column is the
 * timeline. Remembered on this machine.
 */
export function todayColumns(): boolean {
  return todayLayout() !== "stacked";
}
/** Show Today in columns or stacked; with no argument, the one not in force. */
export function setTodayColumns(columns = todayLayout() === "mixed" ? layoutBeforeMixed() !== "columns" : !todayColumns()): Promise<void> {
  return setTodayLayout(columns ? "columns" : "stacked");
}
/** Show Today in a layout, or with no argument go into Mixed or back to the layout it left. */
export async function setTodayLayout(layout: TodayLayout = todayLayout() === "mixed" ? layoutBeforeMixed() : "mixed"): Promise<void> {
  storeTodayLayout(layout);
  if (!hasMailHalf()) return;
  await placeTimeline();
  renderList();
  renderToolbar();
  if (state.tasks.section === "tasks") scrollTaskCursorIntoView();
  else if (todayMixed() || state.tasks.section === "mail") scrollCursorIntoView();
}
const TODAY_THREADS = ".todayThreads";
/** Whether Today shows its mail as conversations. Remembered on this machine. */
export function todayThreads(): boolean {
  return readSetting(TODAY_THREADS) === "on";
}
export function setTodayThreads(on: boolean): void {
  if (on) writeSetting(TODAY_THREADS, "on");
  else removeSetting(TODAY_THREADS);
}
/** The inboxes Today's mail half reads: every account's, or the chosen one's. */
export function todayInboxes(): FolderInfo[] {
  const account = todayAccount();
  return inboxFolders().filter((f) => account === null || f.account_name === account);
}
/** Show one account's inbox in Today, or every account's with null. */
export async function chooseTodayAccount(account: string | null): Promise<void> {
  if (account === null) removeSetting(TODAY_ACCOUNT);
  else writeSetting(TODAY_ACCOUNT, account);
  if (state.tasks.view !== "today" || !todayHasMail()) return;
  state.messageCursor = 0;
  await loadInboxes();
  renderList();
  if (state.tasks.section === "mail") previewCursor();
}
/**
 * Point the mail half of Today at All Inboxes (§4.1).
 *
 * Every account's inbox, read by the core as one list in the active order and
 * paged like a folder — the same rows, the same header, the same cursor, and
 * no bound on how far back it goes. The alternative, a second list widget that
 * happens to show mail, is what §4.1 rules out.
 */
export async function loadInboxes(): Promise<void> {
  reloadPages();
  state.inboxFolders = todayInboxes();
  // Today has a switch of its own; a folder reads its config again when it is
  // next opened.
  state.threads = todayThreads();
  state.threadsFolder = "";
  state.total = await inboxTotal();
  state.messageCursor = Math.min(state.messageCursor, Math.max(0, state.total - 1));
  // Narrowed by a filter, the inboxes are read through it again.
  if (state.filter?.text.trim()) await applyFilter();
  await loadTodayReading();
  await placeTimeline();
}
/**
 * Open a task view, replacing whatever the list was showing.
 *
 * Re-opening the view already showing is a refresh rather than a reset: it is
 * what the day watcher and the `tasks-changed` event both want, and losing the
 * cursor on every push would make the list unusable while a sync runs.
 */
export async function openTaskView(view: TaskViewName): Promise<void> {
  leaveChat();
  const changed = state.tasks.view !== view;
  state.tasks.view = view;
  state.sidebarCursor = taskEntryIndex(view);
  if (changed || state.search) dropFilter();

  if (changed) {
    state.tasks.cursor = 0;
    state.tasks.page = 0;
    state.tasks.section = "tasks";
    state.tasks.search = "";
    state.tasks.selected.clear();
    state.tasks.anchor = null;
    state.tasks.detail = null;
    state.detail = null;
    state.tasks.linked = [];
    state.expandedThreads.clear();
  }

  // The mail machinery is pointed at nothing: a task view other than Today has
  // no messages, and leaving the previous folder's pages loaded would let the
  // message commands act on mail that is no longer on screen.
  state.generation += 1;
  const generation = state.generation;
  state.pages.clear();
  state.fetching.clear();
  clearSelection();
  state.search = null;
  state.inboxFolders = null;
  state.total = 0;

  syncSearchBox();
  await reloadTasks();
  // A folder opened while the tasks were on their way has the list now.
  if (generation !== state.generation) return;
  if (view === "today" && todayHasMail()) await loadInboxes();
  // Today opens on its first row: the mail's while it shows any.
  if (changed && view === "today") state.tasks.section = firstSection();
  render();
  if (state.tasks.section === "mail") previewCursor();
  else previewTaskCursor();
}
/**
 * Filter the open task view to one tag, as the tasks window's sidebar does;
 * the tag already in force lifts the filter. Today opens if no view is.
 */
export async function filterByTag(name: string): Promise<void> {
  if (!state.tasks.view) await openTaskView("today");
  const filter = tagFilter(name);
  state.focus = "list";
  if (state.filter?.text === filter) await clearFilter();
  else await setFilter(filter);
}
/** How long the cursor rests in the task list before the detail follows it. */
let taskPreviewTimer: ReturnType<typeof setTimeout> | undefined;
/**
 * Show whatever task the cursor is on, once it stops moving.
 *
 * Debounced for the same reason the message reader is: holding `j` should not
 * fetch a detail, a log and a link set for every row walked past.
 */
export function previewTaskCursor(): void {
  clearTimeout(taskPreviewTimer);
  if (!taskSectionActive()) return;
  taskPreviewTimer = setTimeout(() => {
    void loadTaskDetail(false).catch(() => {
      // `call` has already reported it; an empty pane is not worth more.
    });
  }, PREVIEW_DEBOUNCE);
}
/** The task under the cursor, if the cursor is in the task list at all. */
export function cursorTask(): TaskSummary | undefined {
  return state.tasks.rows[state.tasks.cursor];
}
/**
 * Load the task under the cursor into the reader.
 *
 * `moveFocus` distinguishes opening it — the keyboard moves to the pane — from
 * following the cursor, which leaves it in the list.
 */
export async function loadTaskDetail(moveFocus: boolean): Promise<void> {
  const task = cursorTask();
  if (!task) {
    state.tasks.detail = null;
    renderReader();
    return;
  }
  const detail = await call<TaskDetail>("get_task", { id: task.id });
  // The cursor may have moved on while the task was being read.
  if (cursorTask()?.id !== task.id) return;
  const wasEditing = state.tasks.detail?.task.id === task.id ? state.tasks.editing : null;
  state.tasks.detail = detail;
  // A reload of the same task — a sync landing mid-sentence, say — must not
  // close the field being typed into and throw the text away. A different task
  // is a different matter: there is nothing to go back to.
  state.tasks.editing = wasEditing;
  if (moveFocus) {
    state.focus = "reader";
    render();
  } else {
    renderReader();
  }
}
/**
 * The tasks a command applies to: the selection when there is one, else the one
 * under the cursor.
 *
 * Ids rather than rows, because every task command takes a list of them and one
 * call does the whole set — which is also what makes a bulk action one undo step
 * (§10.3) rather than a dozen.
 */
export function targetTaskIds(): number[] {
  if (state.tasks.selected.size > 0) {
    const ids = [...state.tasks.selected]
      .sort((a, b) => a - b)
      .map((index) => state.tasks.rows[index]?.id)
      .filter((id): id is number => typeof id === "number");
    // A task under two tags may be selected twice (§30).
    return [...new Set(ids)];
  }
  const task = cursorTask();
  return task ? [task.id] : [];
}
/**
 * Whether every task the next action would touch is already completed.
 *
 * What makes completion a toggle rather than two keys. Read from the rows on
 * screen, which is where the selection lives; an empty target set counts as not
 * complete, so the first press of `x` on nothing selected says "no task
 * selected" rather than claiming to have reopened something.
 */
export function targetTasksAreAllComplete(): boolean {
  const ids = new Set(targetTaskIds());
  if (ids.size === 0) return false;
  const targets = state.tasks.rows.filter((row) => ids.has(row.id));
  return targets.length > 0 && targets.every((row) => row.is_completed);
}
/**
 * Run a task command over the current target set and put the list back in step.
 *
 * Every mutation ends the same way — reload, redraw, say what happened — so
 * saying it once here is what stops one command forgetting to refresh and
 * leaving a completed task looking untouched.
 */
export async function withTasks(
  describe: (n: number) => string,
  fn: (ids: number[]) => Promise<unknown>,
  skipDoneForToday = false,
): Promise<void> {
  let ids = targetTaskIds();
  if (ids.length === 0) {
    setStatus("no task selected");
    return;
  }
  // Completing one again would complete its next date, and reopening it would
  // reopen nothing: it is already open.
  if (skipDoneForToday) {
    const done = state.tasks.rows.filter((row) => row.is_done_for_today && ids.includes(row.id));
    ids = ids.filter((id) => !done.some((row) => row.id === id));
    if (ids.length === 0) {
      setStatus(doneForTodayNote(done[0], state.tasks.today));
      return;
    }
  }
  await fn(ids);
  await refreshTasks();
  // Naming the key rather than the command: undo is only useful if it is
  // reachable in the second after the action, and "press u" is reachable in a
  // way that "the undo binding" is not (§10.3).
  const key = keyFor("undo");
  setStatus(`${describe(ids.length)}${key ? ` · ${key} to undo` : ""}`);
}
/** The keys the task list binds to these commands, for the pane to name. */
export function taskKeys(commands: string[]): Record<string, string | null> {
  const table = state.bindings.task_normal;
  const keyOf = (command: string) =>
    Object.entries(table).find(([, spec]) => spec.trim() === command)?.[0] ?? null;
  return Object.fromEntries(commands.map((c) => [c, keyOf(c)]));
}
/** The key the active table binds to a command, for a status line to name. */
export function keyFor(command: string): string | null {
  const table = activeBindings();
  for (const [key, spec] of Object.entries(table)) {
    if (spec.trim() === command) return key;
  }
  return null;
}
/**
 * Re-read everything a mutation could have moved.
 *
 * A single change can move a task between views, change the badge, advance a
 * recurrence and re-order the list, so there is no useful "what changed" to act
 * on — re-reading the open view is both simpler and correct.
 */
export async function refreshTasks(): Promise<void> {
  if (inTaskView()) {
    await reloadTasks();
    renderTaskList();
    // The detail pane is showing one of these rows; a completion it did not
    // hear about would leave it contradicting the list beside it. Not while a
    // field is open, though: redrawing the pane would replace the text being
    // typed with what is stored, which is the one way this can lose work.
    if (state.tasks.detail && !state.tasks.editing) await loadTaskDetail(false);
  }
  await refreshTaskBadge();
  renderFolders();
  // The message list's task column and the reader's linked block are both
  // derived from tasks, so a task change invalidates them wherever they are.
  state.tasks.counts.clear();
  if (!inTaskView() || state.tasks.view === "today") renderRows();
  if (state.detail) await loadMessageTasks(state.detail.id);
}
/** The tasks linked to a message, for the reader's block (§6.3). */
export async function loadMessageTasks(id: number): Promise<void> {
  if (!state.tasks.available) return;
  const tasks = await call<TaskSummary[]>("tasks_for_message", { message: id });
  if (state.detail?.id !== id) return;
  state.tasks.linked = tasks;
  // Only when the reader is actually showing that message. In Today it can be
  // showing a task instead, and redrawing then would replace the task with the
  // message the cursor last passed.
  if (!taskSectionActive()) renderReader();
}
/** Pending batch count query, so a scroll asks once rather than per frame. */
let countsTimer: ReturnType<typeof setTimeout> | undefined;
/**
 * Fill in the task counts for the rows on screen (§6.3).
 *
 * One query for the whole visible set, never one per row — which is why the
 * command takes a list of message ids and answers only for the ones that have
 * tasks. Ids that come back unmentioned are recorded as zero, so they are not
 * asked about again on the next frame.
 */
export function scheduleTaskCounts(first: number, last: number): void {
  if (!state.tasks.available) return;
  const wanted: number[] = [];
  for (let index = first; index <= last; index++) {
    // A conversation's row counts the tasks of every member.
    const row = rowAt(index);
    const ids = isConversationRow(row) ? row.thread_members! : row ? [row.id] : [];
    for (const id of ids) if (!state.tasks.counts.has(id)) wanted.push(id);
  }
  if (wanted.length === 0) return;

  clearTimeout(countsTimer);
  countsTimer = setTimeout(() => {
    void call<MessageTaskCount[]>("open_task_counts", { messages: wanted })
      .then((counts) => {
        for (const id of wanted) state.tasks.counts.set(id, 0);
        for (const count of counts) state.tasks.counts.set(count.message_id, count.open_tasks);
        renderRows();
      })
      .catch(() => {
        // A column that stays blank is a fair outcome; `call` has said why.
      });
  }, FETCH_DEBOUNCE);
}
/**
 * Put the cursor on a task by id, opening the view that holds it if needed.
 *
 * Returns whether it was found. The views are projections of one list, so a
 * task is in exactly one of them — but which one is a property of its dates and
 * its state, and asking is both cheaper and more honest than working it out
 * here from a copy of the rules.
 */
export async function openTaskById(id: number): Promise<boolean> {
  // A message's window has no task views: the task gets a window of its own.
  if (WINDOW === "message") {
    const detail = await call<TaskDetail>("get_task", { id }).catch(() => null);
    if (!detail) return false;
    await openItemWindow("task", id, detail.task.title);
    return true;
  }
  for (const view of ["today", "upcoming", "someday", "completed", "archive", "trash"] as TaskViewName[]) {
    const rows = await call<TaskSummary[]>(
      "list_tasks",
      { view, search: null, page: null },
    );
    if (!rows.some((row) => row.id === id)) continue;

    await openTaskView(view);
    const index = state.tasks.rows.findIndex((row) => row.id === id);
    state.tasks.cursor = Math.max(0, index);
    // Landing on a task means the tasks are the half being walked, even when
    // the view was already open with the keyboard down in its mail.
    if (view === "today" && !todayKeeps("tasks")) setTodayKept("tasks", true);
    state.tasks.section = "tasks";
    state.focus = "list";
    renderTaskList();
    await loadTaskDetail(false);
    scrollTaskCursorIntoView();
    return true;
  }
  return false;
}
/**
 * Follow a task's link back to its message (§6.2).
 *
 * The local index answers instantly and is tried first, without a task widget
 * around it. Only the miss goes to the server, folder by folder, and that is
 * slow enough to be worth showing — which is what §6.2 means by running steps 2
 * and 3 as visible background work.
 */
export async function openLinkedMessage(task: number, messageId: string): Promise<void> {
  // The tasks window has no mail, and only the mailbox has chat: the mailbox
  // opens it, in front.
  const chat = messageId.startsWith("chat://");
  if (WINDOW === "tasks" || (chat && WINDOW !== "main")) {
    await emitTo("main", "context-menu-command", `open-task-link ${task} ${messageId}`);
    await call("show_mailbox");
    return;
  }
  if (chat) {
    const { openChatLink } = await import("./chat");
    await openChatLink(messageId);
    return;
  }
  let found = await call<LinkedMessage>(
    "linked_message",
    { task, message: messageId },
  );

  if (found.resolution !== "local") {
    found = await runTask(
      { id: "resolve-link", label: "Looking for the message", detail: messageId },
      () =>
        call<LinkedMessage>(
          "resolve_linked_message",
          { task, message: messageId },
        ),
    );
  }

  if (found.local_id === null) {
    // §6.2 step 3. The task is never broken by a missing message, so this is a
    // report rather than a failure.
    setStatus(
      `message no longer available — ${found.subject ?? "(no subject)"}${
        found.from ? ` from ${found.from}` : ""
      }`,
    );
    return;
  }

  // A task's window has no list to land in: the message gets a window of its own.
  if (WINDOW === "task") {
    await openItemWindow("message", found.local_id, found.subject);
    return;
  }
  const { showMessage } = await import("./jump");
  await showMessage(found.local_id, found.account_name, found.folder_name);
}
/**
 * Make a task out of the current message, or one per selected message (§6.1),
 * postponed to today so it is in Today.
 *
 * The mail stays on screen and the status line says what was made, so a key
 * can make the task and file the message in one go. With `edit`, a single
 * message's task is opened for editing instead. With `ask`, the date is asked
 * for first, a postpone date or, switched to in the picker, a due date;
 * dismissing the picker makes nothing, and stops a chain before it files the
 * message.
 */
export async function createTasksFromMessages(
  { edit = false, ask = false }: { edit?: boolean; ask?: boolean } = {},
): Promise<void> {
  // A conversation makes one task, of its newest message.
  const targets = await targetMessages("newest");
  if (!targets || targets.rows.length === 0) {
    setStatus("no message selected");
    return;
  }
  let postpone: string | null = state.tasks.today;
  let due: string | null = null;
  if (ask) {
    const chosen = await pickDate("date", "Postpone until", state.tasks.today, [
      { id: "postpone", title: "Postpone until" },
      { id: "due", title: "Due" },
    ]);
    if (!chosen) throw new Cancelled();
    const date = chosen.kind === "clear" ? null : chosen.date;
    if (chosen.field === "due") {
      postpone = null;
      due = date;
    } else postpone = date;
  }
  const ids = [...new Set(targets.rows.map((row) => (isConversationRow(row) ? row.thread_members!.at(-1)! : row.id)))];
  const created = await call<TaskDetail[]>("create_tasks_from_messages", {
    messages: ids,
    postpone,
    due,
  });
  // Drawn before a chain goes on to file the message, so the task is on screen
  // no later than the mail leaves it.
  await refreshTasks();

  if (edit && created.length === 1 && (await openTaskById(created[0].task.id))) {
    beginTaskEdit("both");
    return;
  }
  const what =
    created.length === 1 ? `task "${created[0].task.title}"` : describeTasks(created.length);
  const where = due
    ? `due ${due}`
    : postpone === state.tasks.today
      ? "in Today"
      : postpone
        ? `for ${postpone}`
        : "in Someday";
  offerUndo(`created ${what} ${where}`);
}
/** The machine's UTC offset in minutes, so a timezone change can be noticed. */
let watchedOffset = new Date().getTimezoneOffset();
/**
 * Re-evaluate every date-dependent view when the day turns over.
 *
 * The native side watches for this too and pushes `task-day-changed`; this is
 * the same watch from inside the window, and both are wanted. A wall-clock
 * comparison rather than a timer to midnight, because a monotonic timer does
 * not run while the machine is asleep — a laptop shut at 23:00 and opened at
 * 08:00 would otherwise still be waiting for a midnight that has been and gone,
 * which is precisely the overnight case §4.3 is about.
 */
export function watchTheDay(): void {
  setInterval(() => {
    const today = localToday();
    const offset = new Date().getTimezoneOffset();
    if (today === state.tasks.today && offset === watchedOffset) return;
    watchedOffset = offset;
    void rolloverDay(today);
  }, DAY_WATCH_MS);
}
/** Redraw everything that was computed against yesterday. */
export async function rolloverDay(today: string): Promise<void> {
  state.tasks.today = today;
  await refreshTaskBadge();
  renderFolders();
  if (inTaskView()) {
    // Which tasks are in Today is a question about the date, so the rows
    // themselves have to be re-read, not merely redrawn.
    await reloadTasks();
    renderTaskList();
  }
  renderReader();
  setStatus(`the day rolled over to ${today}`);
}
