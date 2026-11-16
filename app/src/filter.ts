/**
 * The filter (§4.5): `F` narrows the list on screen and nothing else.
 *
 * Search looks everywhere and replaces the list with what it found; the filter
 * keeps the view — the folder, Today, a task view, the search results — and
 * only takes rows out of it, live as it is typed. It belongs to the view it
 * was opened in and goes when another view is shown.
 *
 * What narrows what:
 *
 * - a folder and Today's inboxes ask the core, which runs the query over those
 *   folders alone, so the search language works here too (`f:`, `is:unread`);
 * - a task view hands the text to the core's task filter (title, notes, tags,
 *   `tag:`);
 * - rows already held in memory — search results, a saved search, chat's
 *   threads — are narrowed here, by what the rows show.
 *
 * In Today it narrows every kind on screen, and `is:mail`, `is:task`,
 * `is:chat` and `is:reading` keep one kind or several (`kinds.ts`).
 */
import { call } from "./bridge";
import { chatViewKey, filterChat, followChatCursor, inChatView, loadWaiting, todayChat } from "./chat";
import { currentFolder, loadMessages, previewCursor } from "./actions";
import { keepsKind, splitKinds } from "./kinds";
import { todayMixed } from "./layout";
import { placeTimeline, timelineAt } from "./mixed";
import { svgIcon } from "./icons";
import { inboxTotal, render, reloadPages } from "./list";
import { SearchHit, sortOrder } from "./model";
import { hasMailHalf, inTaskView } from "./sidebar";
import { TASK_SEARCH_DEBOUNCE, el, state, todayHasMail } from "./state";
import { TaskSummary } from "./tasklist";
import { loadTodayReading, todayReading } from "./reading";
import { loadInboxes, previewTaskCursor, reloadTasks, showSearchKind } from "./taskview";
import { shows } from "./today";

/** What the list on screen is, so a filter knows when it has been left. */
export function viewKey(): string {
  const search = state.search;
  if (search) return `search\u0000${search.query}\u0000${search.view ?? ""}\u0000${search.conversation ?? ""}`;
  if (state.tasks.view) return `tasks\u0000${state.tasks.view}`;
  if (inChatView()) return chatViewKey();
  const folder = currentFolder();
  return `folder\u0000${folder?.account_name ?? ""}\u0000${folder?.folder_name ?? ""}`;
}

/** Forget a filter that belongs to a view no longer on screen. */
export function syncFilterToView(): void {
  if (state.filter && state.filter.key !== viewKey()) dropFilter();
}

/** Take the filter away, without re-reading anything: the caller does. */
export function dropFilter(): void {
  state.filter = null;
  state.tasks.search = "";
  input.value = "";
}

/** Open the filter bar on the list, or put the keyboard back in it. */
export function openFilter(): void {
  if (!state.filter) {
    state.filter = { key: viewKey(), text: "", hits: null };
    input.value = "";
    render();
  }
  input.focus();
  input.select();
}

/** Filter by `text`, as typing it into the bar would. */
export async function setFilter(text: string): Promise<void> {
  if (!state.filter) state.filter = { key: viewKey(), text: "", hits: null };
  state.filter.text = text;
  input.value = text;
  await applyFilter();
}

/** Clear the filter and show the whole list again. */
export async function clearFilter(): Promise<void> {
  if (!state.filter) return;
  const narrowed = !!state.filter.text.trim();
  dropFilter();
  if (narrowed) await restore();
  render();
  if (state.focus === "list") el.list.focus();
}

const words = (text: string) => text.toLowerCase().split(/\s+/).filter(Boolean);

/** A message held in memory, against what its row shows. */
function rowMatches(hit: SearchHit, wanted: string[]): boolean {
  const m = hit.message;
  const shown = `${m.from_display} ${m.subject ?? ""} ${m.snippet ?? ""} ${hit.folder_name}`.toLowerCase();
  return wanted.every((w) => shown.includes(w));
}

/** The tasks of search results the filter keeps: title, tags and `tag:`. */
export function filterTasks(tasks: TaskSummary[]): TaskSummary[] {
  const wanted = words(state.filter?.text ?? "");
  if (wanted.length === 0) return tasks;
  return tasks.filter((t) =>
    wanted.every((w) =>
      w.startsWith("tag:")
        ? t.tags.some((g) => g.name.toLowerCase().startsWith(w.slice(4)))
        : t.title.toLowerCase().includes(w) || t.tags.some((g) => g.name.toLowerCase().includes(w)),
    ),
  );
}

/** The folders the list on screen pages through, for the core to filter. */
function pagedFolders(): [string, string][] | null {
  if (state.inboxFolders) {
    return state.inboxFolders.flatMap((f): [string, string][] => (f.folder_name ? [[f.account_name, f.folder_name]] : []));
  }
  if (inTaskView()) return null;
  const folder = currentFolder();
  return folder?.folder_name ? [[folder.account_name, folder.folder_name]] : null;
}

/** Whether the list on screen has mail in it at all. */
function showsMail(): boolean {
  if (!todayHasMail()) return false;
  if (state.tasks.view === "search") return hasMailHalf() || state.search?.kind === "mail";
  if (state.tasks.view === "today") return true;
  return !inTaskView();
}

/** The list as it is without the filter. */
async function restore(): Promise<void> {
  if (inChatView()) return filterChat();
  if (inTaskView() && state.tasks.view !== "search") {
    state.tasks.page = 0;
    await reloadTasks();
  }
  if (state.search) {
    showSearchKind();
    state.messageCursor = 0;
    return;
  }
  reloadPages();
  if (state.tasks.view === "today" && todayHasMail()) {
    if (todayChat()) await loadWaiting();
    await loadInboxes();
  } else if (!inTaskView()) await loadMessages(true);
}

/** Narrow the list to what the filter's text keeps. */
export async function applyFilter(): Promise<void> {
  const filter = state.filter;
  if (!filter) return;
  if (inChatView()) return filterChat();
  const text = filter.text.trim();
  if (!text) {
    filter.hits = null;
    await restore();
    if (state.filter === filter) await finish();
    return;
  }

  // In Today the kinds named are taken off the text, and the rest narrows
  // each kind kept.
  const today = state.tasks.view === "today" && todayHasMail();
  const { rest } = today ? splitKinds(text) : { rest: text };

  // The tasks
  if (state.tasks.view === "search") showSearchKind();
  else if (inTaskView()) {
    state.tasks.search = rest;
    state.tasks.page = 0;
    state.tasks.cursor = 0;
    await reloadTasks();
    if (today && !keepsKind("task")) state.tasks.rows = [];
  }

  // The mail
  if (showsMail()) {
    let hits: SearchHit[] | null = [];
    if (state.search) hits = state.search.hits.filter((h) => rowMatches(h, words(text)));
    else if (today && !keepsKind("mail")) hits = [];
    // Kept whole: the inboxes page as they do unfiltered.
    else if (today && !rest) hits = null;
    else {
      const folders = pagedFolders();
      if (folders) {
        const sort = todayMixed() ? sortOrder({ key: "date", dir: state.sort.dir }) : sortOrder();
        hits = await call<SearchHit[]>("filter_messages", { query: rest, folders, sort }).catch(() => [] as SearchHit[]);
      }
    }
    const total = hits ? hits.length : await inboxTotal();
    if (state.filter !== filter || filter.text.trim() !== text) return;
    filter.hits = hits;
    state.generation += 1;
    state.pages.clear();
    state.fetching.clear();
    state.total = state.tasks.view === "search" && state.search?.kind === "tasks" ? 0 : total;
    state.messageCursor = 0;
  }

  // The threads and the reading
  if (today) {
    if (todayChat()) await loadWaiting();
    if (todayReading()) await loadTodayReading();
    state.chat.cursor = 0;
    state.reading.cursor = 0;
    if (state.filter !== filter || filter.text.trim() !== text) return;
  }
  await finish();
}

/** Draw the narrowed list and show what the cursor is on. */
async function finish(): Promise<void> {
  // The timeline starts on its first row, whatever kind that is.
  if (todayMixed()) {
    await placeTimeline();
    const first = timelineAt(0);
    if (first && state.tasks.section !== "tasks") {
      state.tasks.section = first.kind;
      if (first.kind === "mail") state.messageCursor = first.index;
      else if (first.kind === "chat") state.chat.cursor = first.index;
      else state.reading.cursor = first.index;
    } else if (!first && state.tasks.rows.length > 0 && shows("tasks")) state.tasks.section = "tasks";
    render();
    if (state.tasks.section === "tasks") previewTaskCursor();
    else if (state.tasks.section === "chat") await followChatCursor();
    else previewCursor();
    return;
  }
  // A plain folder is all mail; a task view says whether it shows any.
  const mail = state.total > 0 && (!inTaskView() || shows("mail"));
  if (state.tasks.view && (!mail || state.tasks.section === "tasks") && state.tasks.rows.length > 0 && shows("tasks")) {
    state.tasks.section = "tasks";
  } else if (mail) state.tasks.section = "mail";
  render();
  if (inTaskView() && state.tasks.section === "tasks") previewTaskCursor();
  else if (mail) previewCursor();
}

// ---------------------------------------------------------------------------
// The bar
// ---------------------------------------------------------------------------

const bar = document.createElement("div");
bar.className = "fbar";
bar.innerHTML = `${svgIcon("filter")}<span class="fk lbl">Filter</span><input type="text" spellcheck="false" autocomplete="off" aria-label="Filter this list"><span class="cnt"></span><button type="button" class="fx" data-filter-clear title="Clear the filter"><kbd>esc</kbd></button>`;
const input = bar.querySelector("input")!;
const count = bar.querySelector<HTMLElement>(".cnt")!;

/**
 * Put the bar at the top of the list after the list was drawn again.
 *
 * One element for the life of the window, so that a redraw while the text is
 * being typed keeps the keyboard, the caret and every key in the field.
 */
export function attachFilterBar(hadFocus: boolean): void {
  // A class rather than `:has(> .fbar)` in the stylesheet: WebKit does not
  // always re-evaluate `:has()` once the bar is gone, and Today's columns
  // then keep a row for it.
  el.list.classList.toggle("with-filter", !!state.filter);
  if (!state.filter) {
    bar.remove();
    return;
  }
  el.list.prepend(bar);
  const mail = showsMail() && !inChatView() ? state.total : 0;
  const tasks = inTaskView() ? state.tasks.rows.length : 0;
  const today = state.tasks.view === "today" && todayHasMail();
  const others = today ? (shows("chat") ? state.chat.rows.length : 0) + (shows("reading") ? state.reading.rows.length : 0) : 0;
  const n = inChatView() ? state.chat.rows.length : mail + tasks + others;
  count.textContent = state.filter.text.trim() ? `${n} match${n === 1 ? "" : "es"}` : "";
  input.placeholder = inChatView()
    ? "Words: a person, a room, the text"
    : state.tasks.view === "today" && todayHasMail()
      ? "Words, or f:, is:unread, is:chat …"
    : inTaskView() && !showsMail()
      ? "Words, or tag:name"
      : "Words, or f:, s:, is:unread …";
  if (hadFocus) input.focus({ preventScroll: true });
}

/** Whether the bar has the keyboard, to be given back after a redraw. */
export function filterHasFocus(): boolean {
  return document.activeElement === input;
}

let timer: ReturnType<typeof setTimeout> | undefined;
input.addEventListener("input", () => {
  if (!state.filter) return;
  state.filter.text = input.value;
  clearTimeout(timer);
  timer = setTimeout(() => void applyFilter(), TASK_SEARCH_DEBOUNCE);
});
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    clearTimeout(timer);
    // The list keeps the filter and gets the keyboard.
    void applyFilter().then(() => {
      state.focus = "list";
      el.list.focus();
      render();
    });
  } else if (e.key === "Escape") {
    e.preventDefault();
    e.stopPropagation();
    clearTimeout(timer);
    state.focus = "list";
    void clearFilter();
  }
});
bar.addEventListener("mousedown", (e) => {
  if ((e.target as HTMLElement).closest("[data-filter-clear]")) {
    e.preventDefault();
    void clearFilter();
  }
});
