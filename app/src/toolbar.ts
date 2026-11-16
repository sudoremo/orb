import { inChatView, todayChat } from "./chat";
import { readingFolder, todayReading } from "./reading";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { currentFolder } from "./actions";
import { runCommand } from "./commands";
import { escape } from "./format";
import { svgIcon } from "./icons";
import { SortKey } from "./model";
import { hasMailHalf, inTaskView } from "./sidebar";
import { ITEM, SearchKind, el, state, todayHasMail } from "./state";
import { taskViewLabel } from "./tasklist";
import { scopeLabel } from "./searchfield";
import { setTodayLayout } from "./taskview";
import { todayLayout, todayMixed } from "./layout";
import { newestFirst } from "./mixed";
import { setTodayKept } from "./panes";
import { canPutAway, todayKeeps } from "./today";

/**
 * The toolbar: what the list shows and where it is from, the order in force
 * and a menu to change it, and the search box. Drawn with the rest of the
 * window, from the state alone.
 */

/** A field's name, and its two directions in the words that fit it. */
const SORTS: Record<SortKey, { name: string; desc: string; asc: string }> = {
  date: { name: "Date", desc: "Newest first", asc: "Oldest first" },
  from: { name: "Sender", desc: "Z to A", asc: "A to Z" },
  subject: { name: "Subject", desc: "Z to A", asc: "A to Z" },
};

/** The toolbar menu open, if any: the sort order, or what Today shows. */
let menuOpen: "sort" | "today" | null = null;

/** What the list is, in a word or two, and where it is from. */
function heading(): { title: string; sub: string } {
  const view = state.tasks.view;
  if (view === "search" || (state.search?.taskQuery && !view)) {
    const n = (state.search?.hits.length ?? 0) + (state.search?.tasks.length ?? 0);
    return { title: "Search", sub: `${n} result${n === 1 ? "" : "s"}` };
  }
  if (view) {
    const sub = view === "today"
      ? new Date(`${state.tasks.today}T00:00:00`).toLocaleDateString("en-GB", {
          weekday: "long",
          day: "numeric",
          month: "long",
        })
      : "";
    return { title: taskViewLabel(view), sub };
  }
  if (state.search) {
    const n = state.search.hits.length;
    const view = state.search.view;
    const named = view === undefined ? "Search" : view === "must-read" ? "Must read" : state.virtualFolders[view]?.name ?? "Search";
    // Where it looked, when that was not everywhere.
    const where = state.search.scope.kind === "all" || view === "must-read" ? "" : ` · in ${scopeLabel(state.search.scope)}`;
    return { title: named, sub: `${n} result${n === 1 ? "" : "s"}${where}` };
  }
  const folder = currentFolder();
  if (!folder) return { title: "", sub: "" };
  return {
    title: folder.is_pinned ? folder.display_name : folder.label ?? folder.display_name,
    sub: folder.is_pinned ? "" : folder.account_name,
  };
}

/** Whether the list is a folder or search results, which an order applies to. */
function sortable(): boolean {
  // Search results order their mail; their tasks stand under their dates.
  if (state.tasks.view === "search") return state.search?.kind === "all" && state.search.hits.length > 0;
  if (inTaskView()) return false;
  // A conversation reads in the order it was written.
  if (state.search) return !!state.search.effective;
  return !!currentFolder()?.folder_name;
}

function sortControl(): string {
  if (!sortable()) return "";
  if (state.search) return searchSortControl(state.search.sort);
  const field = SORTS[state.sort.key];
  // Newest first for dates, A to Z for text: the order each is read in.
  const directions: (1 | -1)[] = state.sort.key === "date" ? [-1, 1] : [1, -1];
  // Conversations have one order, which the switch beside this says they are in.
  const label = state.threads
    ? "Newest first"
    : `${field.name} ${state.sort.dir === -1 ? "↓" : "↑"}`;
  const check = (on: boolean) => `<span class="ck" aria-hidden="true">${on ? "✓" : ""}</span>`;
  const item = (role: string, on: boolean, attrs: string, text: string, key = "") =>
    `<button type="button" class="mi" role="${role}" aria-checked="${on}" tabindex="-1"${
      state.threads && role === "menuitemradio" ? ' aria-disabled="true"' : ""
    } ${attrs}>${check(on)}<span class="lbl">${escape(text)}</span>${key ? `<kbd>${key}</kbd>` : ""}</button>`;
  const open = menuOpen === "sort";
  const menu = !open
    ? ""
    : `<div class="menu" role="menu" aria-label="Sort">
        <div class="mh lbl" role="presentation">Sort by</div>
        ${(Object.keys(SORTS) as SortKey[])
          .map((key) => item("menuitemradio", state.sort.key === key, `data-by="${key}"`, SORTS[key].name))
          .join("")}
        <hr role="separator">
        ${directions
          .map((dir) => item("menuitemradio", state.sort.dir === dir, `data-dir="${dir}"`, dir === -1 ? field.desc : field.asc))
          .join("")}
        <hr role="separator">
        ${item("menuitemcheckbox", state.threads, "data-threads", "Show as conversations", "zt")}
      </div>`;
  return `<span class="sort${open ? " open" : ""}"><button type="button" class="sortbtn" data-sort-menu aria-haspopup="menu" aria-expanded="${open}">${svgIcon(
    "sort",
  )}<span class="lbl">${escape(label)}</span>${svgIcon("chev", "chev")}</button>${menu}</span>`;
}

/**
 * The switch between a list of messages and one of conversations, as `zt`
 * switches: in a folder's toolbar, and in Today beside the choice of inbox.
 */
export function threadsSwitch(): string {
  // An icon each; the terminal look, which draws no icons, says a word.
  const option = (threads: boolean, icon: "mail" | "thread", label: string, word: string) =>
    `<button type="button" role="radio" aria-checked="${state.threads === threads}" data-threads-choice="${threads ? "on" : "off"}" title="${label} (zt)" aria-label="${label}">${svgIcon(icon)}<span class="tw">${word}</span></button>`;
  return `<span class="seg threads-switch" role="radiogroup" aria-label="Show as">${option(false, "mail", "Messages", "messages")}${option(true, "thread", "Conversations", "threads")}</span>`;
}
/** Whether the list on screen is a folder, which can be shown as conversations. */
function folderList(): boolean {
  return !state.search && !inTaskView() && !!currentFolder()?.folder_name;
}

/** The control for search results: relevance, or a field and a direction. */
function searchSortControl(sort: { key: SortKey; dir: 1 | -1 } | undefined): string {
  const label = sort ? `${SORTS[sort.key].name} ${sort.dir === -1 ? "↓" : "↑"}` : "Relevance";
  const check = (on: boolean) => `<span class="ck" aria-hidden="true">${on ? "✓" : ""}</span>`;
  const item = (on: boolean, attrs: string, text: string) =>
    `<button type="button" class="mi" role="menuitemradio" aria-checked="${on}" tabindex="-1" ${attrs}>${check(
      on,
    )}<span class="lbl">${escape(text)}</span></button>`;
  const directions: (1 | -1)[] = sort?.key === "date" ? [-1, 1] : [1, -1];
  const open = menuOpen === "sort";
  const menu = !open
    ? ""
    : `<div class="menu" role="menu" aria-label="Sort">
        <div class="mh lbl" role="presentation">Sort by</div>
        ${item(!sort, 'data-by="relevance"', "Relevance")}
        ${(Object.keys(SORTS) as SortKey[])
          .map((key) => item(sort?.key === key, `data-by="${key}"`, SORTS[key].name))
          .join("")}
        ${
          sort
            ? `<hr role="separator">${directions
                .map((dir) =>
                  item(sort.dir === dir, `data-dir="${dir}"`, dir === -1 ? SORTS[sort.key].desc : SORTS[sort.key].asc),
                )
                .join("")}`
            : ""
        }
      </div>`;
  return `<span class="sort${open ? " open" : ""}"><button type="button" class="sortbtn" data-sort-menu aria-haspopup="menu" aria-expanded="${open}">${svgIcon(
    "sort",
  )}<span class="lbl">${escape(label)}</span>${svgIcon("chev", "chev")}</button>${menu}</span>`;
}

/** Over search results of both kinds: all of them, the mail, the tasks (§26). */
function kindControl(): string {
  const search = state.search;
  if (!search?.taskQuery || !todayHasMail()) return "";
  const kinds: [SearchKind, string, number][] = [
    ["all", "All", search.hits.length + search.tasks.length],
    ["mail", "Mail", search.hits.length],
    ["tasks", "Tasks", search.tasks.length],
  ];
  return `<span class="seg kinds" role="radiogroup" aria-label="Results" title="Switch the results (gs)">${kinds
    .map(
      ([kind, label, n]) =>
        `<button type="button" role="radio" aria-checked="${search.kind === kind}" data-search-kind="${kind}"><span class="lbl">${label}</span> <span class="c">${n}</span></button>`,
    )
    .join("")}</span>`;
}

/**
 * In Today, the menu of what it shows and how: the mail, the tasks, the chat
 * and the reading list, each on or off, and the layout. It stays open while
 * its switches are flipped, so several are flipped at once. The last section
 * shown cannot be put away.
 */
function todayControl(): string {
  if (state.tasks.view !== "today" || !hasMailHalf()) return "";
  const open = menuOpen === "today";
  const layout = todayLayout();
  const check = (on: boolean) => `<span class="ck" aria-hidden="true">${on ? "✓" : ""}</span>`;
  const item = (role: string, on: boolean, attrs: string, text: string, key = "", locked = false) =>
    `<button type="button" class="mi" role="${role}" aria-checked="${on}" tabindex="-1"${
      locked ? ' aria-disabled="true" title="Today shows one section at least"' : ""
    } ${attrs}>${check(on)}<span class="lbl">${escape(text)}</span>${key ? `<kbd>${key}</kbd>` : ""}</button>`;
  const sections: [string, boolean, string, string][] = [
    ["mail", todayKeeps("mail"), "Mail", ""],
    ["tasks", todayKeeps("tasks"), "Tasks", ""],
    ...(state.chat.available ? [["chat", todayChat(), "Chat", "zw"] as [string, boolean, string, string]] : []),
    ...(readingFolder() ? [["reading", todayReading(), "Reading", "zr"] as [string, boolean, string, string]] : []),
  ];
  const menu = !open
    ? ""
    : `<div class="menu" role="menu" aria-label="Today">
        <div class="mh lbl" role="presentation">Show</div>
        ${sections
          .map(([key, on, text, binding]) =>
            item("menuitemcheckbox", on, `data-today-show="${key}"`, text, binding, on && !canPutAway(key as "mail")),
          )
          .join("")}
        <hr role="separator">
        <div class="mh lbl" role="presentation">Layout</div>
        ${item("menuitemradio", layout === "stacked", 'data-today-layout="stacked"', "Stacked", layout === "columns" ? "zl" : "")}
        ${item("menuitemradio", layout === "columns", 'data-today-layout="columns"', "Columns", layout === "stacked" ? "zl" : "")}
        ${item("menuitemradio", layout === "mixed", 'data-today-layout="mixed"', "Mixed", layout === "mixed" ? "" : "zx")}
      </div>`;
  return `<span class="sort${open ? " open" : ""}"><button type="button" class="sortbtn" data-today-menu aria-haspopup="menu" aria-expanded="${open}" title="What Today shows, and how">${svgIcon(
    layout === "mixed" ? "mixed" : layout === "columns" ? "cols" : "rows",
  )}<span class="lbl">View</span>${svgIcon("chev", "chev")}</button>${menu}</span>`;
}

/** Mixed's one order: the timeline newest first, or oldest first. */
function timelineOrder(): string {
  if (!todayMixed()) return "";
  const open = menuOpen === "sort";
  const newest = newestFirst();
  const check = (on: boolean) => `<span class="ck" aria-hidden="true">${on ? "✓" : ""}</span>`;
  const item = (dir: -1 | 1, text: string) =>
    `<button type="button" class="mi" role="menuitemradio" aria-checked="${newest === (dir === -1)}" tabindex="-1"${
      state.threads ? ' aria-disabled="true" title="Conversations are newest first"' : ""
    } data-dir="${dir}">${check(newest === (dir === -1))}<span class="lbl">${text}</span></button>`;
  const menu = !open
    ? ""
    : `<div class="menu" role="menu" aria-label="Order">
        <div class="mh lbl" role="presentation">Order</div>
        ${item(-1, "Newest first")}${item(1, "Oldest first")}
      </div>`;
  return `<span class="sort${open ? " open" : ""}"><button type="button" class="sortbtn" data-sort-menu aria-haspopup="menu" aria-expanded="${open}">${svgIcon(
    "sort",
  )}<span class="lbl">${newest ? "Newest first" : "Oldest first"}</span>${svgIcon("chev", "chev")}</button>${menu}</span>`;
}

/**
 * The search box: a door to the search field, which floats over the window,
 * and while results or a filter are showing, the query and a way out.
 */
function searchBox(): string {
  const query = state.search?.view === undefined ? state.search?.query ?? "" : "";
  if (query) {
    return `<span class="search on" data-search-open title="Search">${svgIcon(
      "search",
    )}<span class="q">${escape(query)}</span><button type="button" class="search-x" data-search-clear title="Leave the search"><kbd>esc</kbd></button></span>`;
  }
  return `<span class="search" data-search-open role="button" title="Search">${svgIcon(
    "search",
  )}<span class="q lbl">Search</span><kbd>/</kbd></span>`;
}

/** Beside the search: the filter, which narrows only what is on screen. */
function filterButton(): string {
  const on = state.filter ? " on" : "";
  return `<button type="button" class="filterbtn${on}" data-filter-open title="Filter this list (F)">${svgIcon(
    "filter",
  )}<span class="lbl">Filter</span><kbd>F</kbd></button>`;
}

export function renderToolbar(): void {
  const toolbar = document.getElementById("toolbar");
  if (!toolbar) return;
  if (state.booting) {
    toolbar.innerHTML = "";
    return;
  }
  if (ITEM) {
    renderItemToolbar(toolbar);
    return;
  }
  if (inChatView()) {
    // Waiting has one order, newest first, and no filter yet: only the search.
    const n = state.chat.rows.filter((r) => r.newest_at > 0).length;
    const room = state.chat.view === "room" ? state.chat.rooms.find((r) => r.rid === state.chat.room) : undefined;
    const name = room ? (room.kind === "d" ? (room.fname ?? room.name ?? room.label) : room.label) : null;
    const title = name ? `<span class="tb-t">${escape(name)}</span>` : `<span class="tb-t lbl">Waiting</span>`;
    const html = `${title}<span class="tb-s"><span class="lbl">Chat</span> · ${n} thread${n === 1 ? "" : "s"}</span><span class="tb-sp"></span>${searchBox()}`;
    if (toolbar.innerHTML !== html) toolbar.innerHTML = html;
    return;
  }
  const { title, sub } = heading();
  const html = `<span class="tb-t">${escape(title)}</span>${sub ? `<span class="tb-s">${escape(sub)}</span>` : ""}<span class="tb-sp"></span>${kindControl()}${folderList() ? threadsSwitch() : ""}${sortControl()}${todayControl()}${timelineOrder()}${filterButton()}${searchBox()}`;
  if (toolbar.innerHTML !== html) toolbar.innerHTML = html;
  if (menuOpen) {
    const items = menuItems();
    const again = refocus && items.find((i) => i.matches(refocus!));
    (again || items.find((i) => i.getAttribute("aria-checked") === "true") || items[0])?.focus();
    refocus = null;
  }
}

/**
 * A window of one item has no list to sort, filter or search: its toolbar
 * says where the item lives, and the window is named after the item, for
 * the Window menu.
 */
let itemTitle = "";
function renderItemToolbar(toolbar: HTMLElement): void {
  const hit = state.search?.hits[0];
  const task = state.tasks.detail;
  const { title, sub } =
    ITEM?.kind === "message"
      ? { title: folderName(hit?.account_name, hit?.folder_name), sub: hit?.account_name ?? "" }
      : { title: "Task", sub: task?.account_name ?? "" };
  const html = `<span class="tb-t">${escape(title)}</span>${sub ? `<span class="tb-s">${escape(sub)}</span>` : ""}`;
  if (toolbar.innerHTML !== html) toolbar.innerHTML = html;
  const named = ITEM?.kind === "message" ? state.detail?.subject : task?.task.title;
  if (named && named !== itemTitle) {
    itemTitle = named;
    void getCurrentWindow().setTitle(named);
  }
}

/** A folder by the name the sidebar gives it, which for an inbox is not `INBOX`. */
function folderName(account: string | undefined, folder: string | undefined): string {
  const known = state.folders.find((f) => !f.is_pinned && f.account_name === account && f.folder_name === folder);
  return known?.label ?? known?.display_name ?? folder ?? "";
}

function menuItems(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>("#toolbar .menu .mi")].filter(
    (i) => i.getAttribute("aria-disabled") !== "true",
  );
}

/** The item to give the keyboard back to when the open menu is drawn again. */
let refocus: string | null = null;

/** Open the sort menu, or close it; from the keyboard, the arrows walk it. */
export function toggleSortMenu(open = menuOpen !== "sort"): void {
  if (open && !sortable() && !todayMixed()) return;
  menuOpen = open ? "sort" : null;
  renderToolbar();
  if (!open) el.list.focus();
}

/** Open Today's menu, or close it. */
export function toggleTodayMenu(open = menuOpen !== "today"): void {
  if (open && (state.tasks.view !== "today" || !hasMailHalf())) return;
  menuOpen = open ? "today" : null;
  renderToolbar();
  if (!open) el.list.focus();
}

function closeMenu(): void {
  if (menuOpen === "today") toggleTodayMenu(false);
  else if (menuOpen) toggleSortMenu(false);
}

/** Flip a switch of Today's menu, which stays open on the item flipped. */
async function chooseInToday(item: HTMLElement): Promise<void> {
  const show = item.dataset.todayShow;
  const layout = item.dataset.todayLayout;
  if (show === "mail" || show === "tasks") setTodayKept(show, item.getAttribute("aria-checked") !== "true");
  else if (show === "chat") await runCommand("toggle-today-chat", 1);
  else if (show === "reading") await runCommand("toggle-today-reading", 1);
  else if (layout === "stacked" || layout === "columns" || layout === "mixed") await setTodayLayout(layout);
  refocus = show ? `[data-today-show="${show}"]` : `[data-today-layout="${layout}"]`;
  renderToolbar();
}

async function choose(item: HTMLElement): Promise<void> {
  if (item.getAttribute("aria-disabled") === "true") return;
  if (menuOpen === "today") return chooseInToday(item);
  menuOpen = null;
  renderToolbar();
  el.list.focus();
  // Search results keep an order of their own; a folder's is `state.sort`.
  const current = state.search ? state.search.sort : state.sort;
  if (item.dataset.by) {
    const key = item.dataset.by;
    if (key !== (current?.key ?? "relevance")) await runCommand(`sort-by ${key}`, 1);
  } else if (item.dataset.dir && current) {
    // The timeline is by date whatever the mail was sorted by before.
    const key = todayMixed() ? "date" : current.key;
    if (Number(item.dataset.dir) !== current.dir || key !== current.key) {
      await runCommand(`sort-by ${key} ${Number(item.dataset.dir) === 1 ? "asc" : "desc"}`, 1);
    }
  } else if ("threads" in item.dataset) {
    await runCommand("toggle-threads", 1);
  }
  renderToolbar();
}

// The switch between messages and conversations, wherever it is drawn: the
// toolbar, or Today's Mail heading. The keyboard goes back to the list.
document.addEventListener("click", (e) => {
  const choice = (e.target as HTMLElement).closest<HTMLElement>("[data-threads-choice]");
  if (!choice) return;
  el.list.focus({ preventScroll: true });
  if ((choice.dataset.threadsChoice === "on") !== state.threads) void runCommand("toggle-threads", 1);
});

// The menu keeps the keyboard while it is open: the arrows, Enter, Space and
// Escape are its own, and nothing reaches the list's bindings.
window.addEventListener(
  "keydown",
  (e) => {
    if (!menuOpen) return;
    const items = menuItems();
    const at = items.indexOf(document.activeElement as HTMLElement);
    const step = (delta: number) => items[(at + delta + items.length) % items.length]?.focus();
    switch (e.key) {
      case "ArrowDown":
      case "j":
        step(1);
        break;
      case "ArrowUp":
      case "k":
        step(-1);
        break;
      case "Enter":
      case " ":
        if (at >= 0) void choose(items[at]!);
        break;
      case "Escape":
        closeMenu();
        break;
      case "Tab":
        break;
      default:
        return;
    }
    e.preventDefault();
    e.stopImmediatePropagation();
  },
  true,
);

document.addEventListener("click", (e) => {
  const target = e.target as HTMLElement;
  const toolbar = document.getElementById("toolbar");
  if (!toolbar?.contains(target)) {
    if (menuOpen) closeMenu();
    return;
  }
  const item = target.closest<HTMLElement>(".menu .mi");
  if (item) {
    void choose(item);
    return;
  }
  if (target.closest("[data-sort-menu]")) {
    if (menuOpen === "today") toggleTodayMenu(false);
    toggleSortMenu();
    return;
  }
  if (target.closest("[data-today-menu]")) {
    if (menuOpen === "sort") toggleSortMenu(false);
    toggleTodayMenu();
    return;
  }
  if (menuOpen) closeMenu();
  const kind = target.closest<HTMLElement>("[data-search-kind]");
  if (kind) {
    void runCommand(`search-kind ${kind.dataset.searchKind}`, 1);
    return;
  }
  if (target.closest("[data-filter-open]")) {
    void runCommand(state.filter ? "clear-filter" : "filter-mode", 1);
    return;
  }
  if (target.closest("[data-search-clear]")) {
    void runCommand("clear-search", 1);
    return;
  }
  if (target.closest("[data-search-open]")) void runCommand("search-mode", 1);
});
