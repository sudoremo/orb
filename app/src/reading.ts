import { keepingScroll } from "./scrollkeep";
import { leaveChat } from "./chat";
import { cancelMarkRead, previewCursor, scrollCursorIntoView } from "./actions";
import { call, setStatus } from "./bridge";
import { escape } from "./format";
import { IconName } from "./icons";
import { applyListSectionClass, messageRowElement, paintTimeline, render, renderRows } from "./list";
import { todayMixed } from "./layout";
import { placeTimeline } from "./mixed";
import { keepsKind, todayNarrowing } from "./kinds";
import { FolderInfo, SearchHit } from "./model";
import { moveNow, openFolderAt, runSearchQuery, transferTo } from "./moving";
import { scopeArg } from "./searchfield";
import { readingEntry, selectableFolders } from "./sidebar";
import { readSetting, removeSetting, writeSetting } from "./splitters";
import { el, state } from "./state";
import { targetMessages } from "./targets";
import { canPutAway, firstSection } from "./today";
import { renderToolbar } from "./toolbar";

/**
 * The reading list's views: the folder of items to read, the ones marked
 * must read in it, and the archive.
 */
export type ReadingView = "list" | "must-read" | "archive";
export const READING_VIEWS: { view: ReadingView; label: string; icon: IconName }[] = [
  { view: "list", label: "To read", icon: "book" },
  { view: "must-read", label: "Must read", icon: "flag" },
  { view: "archive", label: "Archive", icon: "archive" },
];
/** The roles the core gives the two reading folders. */
const READING = "reading";
const READING_ARCHIVE = "reading-archive";

export const isReadingFolder = (folder: FolderInfo): boolean =>
  folder.role === READING || folder.role === READING_ARCHIVE;
/** The reading folder, or its archive; absent without a `reading` section. */
export function readingFolder(archive = false): FolderInfo | undefined {
  const role = archive ? READING_ARCHIVE : READING;
  return state.folders.find((f) => !f.is_pinned && !f.is_account_header && f.role === role);
}
/** The reading view a folder is, if it is one of the two. */
export function readingViewOf(folder: FolderInfo | undefined): ReadingView | null {
  if (folder?.role === READING) return "list";
  if (folder?.role === READING_ARCHIVE) return "archive";
  return null;
}
/** Where `a` files a message: from the reading list into its archive and back, else the account's archive. */
export function archiveTarget(account: string, folder: string): string | null {
  const reading = readingFolder();
  const archive = readingFolder(true);
  if (reading && archive && account === reading.account_name) {
    if (folder === reading.folder_name) return archive.folder_name;
    if (folder === archive.folder_name) return reading.folder_name;
  }
  return null;
}
/** Open a reading view: a folder, or for Must read the flagged items in the reading folder. */
export async function openReading(view: ReadingView): Promise<void> {
  const folder = readingFolder(view === "archive");
  if (!folder?.folder_name) {
    setStatus("no reading list: add a reading section to config.yml");
    return;
  }
  leaveChat();
  if (view === "must-read") {
    state.sidebarCursor = readingEntry(view);
    await runSearchQuery("is:flagged", "Must read", "must-read", {
      kind: "folders",
      folders: [[folder.account_name, folder.folder_name]],
    });
    return;
  }
  await openFolderAt(selectableFolders().indexOf(folder));
}
/**
 * Set the messages aside in the reading folder: moved within its account,
 * and transferred from any other, as a cross-account move is.
 */
export async function readLater(): Promise<void> {
  const reading = readingFolder();
  if (!reading?.folder_name) {
    setStatus("no reading list: add a reading section to config.yml");
    return;
  }
  const targets = await targetMessages();
  if (!targets || targets.groups.length === 0) {
    setStatus("no message selected");
    return;
  }
  if (targets.groups.every((g) => g.account === reading.account_name)) {
    await moveNow(() => reading.folder_name);
    return;
  }
  await transferTo(reading.account_name, reading.folder_name, false);
}

// ---- Reading in Today ------------------------------------------------------

/** Whether Today shows a reading section, remembered on this Mac. */
export const TODAY_READING = ".todayReading";
/** What the section shows: the must-read items, or the new ones. */
export const TODAY_READING_SHOWS = ".todayReadingShows";
/** A screenful: the section points at the full view for the rest. */
const TODAY_READING_LIMIT = 30;
export type ReadingShows = "must-read" | "new";

export function todayReading(): boolean {
  return readSetting(TODAY_READING) === "on" && !!readingFolder();
}
export function todayReadingShows(): ReadingShows {
  return readSetting(TODAY_READING_SHOWS) === "new" ? "new" : "must-read";
}
/** How many items the section holds beyond the ones it lists. */
let readingBeyond = 0;
/** Read the section's items again: the flagged or the unread ones in the reading folder. */
export async function loadTodayReading(): Promise<void> {
  const folder = readingFolder();
  if (state.tasks.view !== "today" || !todayReading() || !folder?.folder_name || !keepsKind("reading")) {
    state.reading.rows = [];
    state.reading.selected.clear();
    readingBeyond = 0;
  } else {
    // Today's filter narrows the items as it narrows the mail.
    const narrowed = todayNarrowing()?.rest ?? "";
    const hits = await call<SearchHit[]>("search", {
      query: `${todayReadingShows() === "new" ? "is:unread" : "is:flagged"} ${narrowed}`.trim(),
      scope: scopeArg({ kind: "folders", folders: [[folder.account_name, folder.folder_name]] }),
    }).catch(() => [] as SearchHit[]);
    // A selection follows its items, which a refresh may have moved.
    const chosen = new Set([...state.reading.selected].map((i) => state.reading.rows[i]?.message.id));
    state.reading.rows = hits.slice(0, TODAY_READING_LIMIT);
    state.reading.selected = new Set(state.reading.rows.flatMap((hit, i) => (chosen.has(hit.message.id) ? [i] : [])));
    readingBeyond = hits.length - state.reading.rows.length;
  }
  state.reading.cursor = Math.max(0, Math.min(state.reading.cursor, state.reading.rows.length - 1));
  // Nothing left to stand on: the keyboard goes back to the mail, or the tasks.
  if (state.tasks.section === "reading" && state.reading.rows.length === 0) {
    state.tasks.section = firstSection();
  }
}
/** Read the section again and draw it in place, if Today is showing it. */
export async function refreshTodayReading(): Promise<void> {
  if (state.tasks.view !== "today") return;
  await loadTodayReading();
  if (todayMixed()) {
    await placeTimeline();
    applyListSectionClass();
    paintTimeline();
    return;
  }
  const section = el.list.querySelector(".rd-section");
  if (!section) return;
  keepingScroll(el.list, () => {
    section.outerHTML = renderReadingSection();
    paintReadingRows();
  });
  applyListSectionClass();
}
/** The section, around a list its rows are put into by `paintReadingRows`. */
export function renderReadingSection(): string {
  const shows = todayReadingShows();
  const option = (value: ReadingShows, label: string) =>
    `<button type="button" role="radio" aria-checked="${shows === value}" data-reading-shows="${value}"><span class="lbl">${label}</span></button>`;
  const n = state.reading.rows.length + readingBeyond;
  const empty = shows === "new" ? "Nothing new to read" : "Nothing marked must read";
  return `<div class="split rd-split" data-rd-split role="separator" aria-orientation="horizontal" title="Drag to resize · double-click to fit the reading list"></div><div class="rd-section">
      <div class="sec rd-section-head"><span class="sec-t">
        <span class="lbl">Reading</span><span class="n">${n}</span>
        <span class="seg" role="radiogroup" aria-label="What Reading shows">${option("must-read", "Must read")}${option("new", "New")}</span>
      </span></div>
      <div class="rd-list" role="listbox" aria-label="Reading">${
        n === 0 ? `<div class="empty">${escape(empty)}</div>` : ""
      }</div>${
        readingBeyond > 0
          ? `<button type="button" class="tk-more" data-reading-more>${readingBeyond} more in ${shows === "new" ? "To read" : "Must read"}</button>`
          : ""
      }
    </div>`;
}
/** Draw the section's rows, the cursor among them. */
export function paintReadingRows(): void {
  if (todayMixed()) {
    renderRows();
    return;
  }
  const list = el.list.querySelector<HTMLElement>(".rd-list");
  if (!list || state.reading.rows.length === 0) return;
  list.replaceChildren(
    ...state.reading.rows.map((hit, i) => {
      const row = messageRowElement(hit.message, i === state.reading.cursor, " rd-row");
      row.dataset.reading = String(i);
      row.id = `reading-row-${i}`;
      return row;
    }),
  );
}
/** Put the cursor on an item of the section, which takes the keyboard. */
export function moveReadingCursor(to: number): void {
  if (state.tasks.section === "mail") cancelMarkRead();
  state.tasks.section = "reading";
  state.reading.cursor = Math.max(0, Math.min(to, state.reading.rows.length - 1));
  applyListSectionClass();
  paintReadingRows();
  if (todayMixed()) scrollCursorIntoView();
  else el.list.querySelector(`#reading-row-${state.reading.cursor}`)?.scrollIntoView({ block: "nearest" });
  previewCursor();
}
/** Show the reading section in Today, or put it away. */
export async function setTodayReading(on = !todayReading()): Promise<void> {
  if (on && !readingFolder()) {
    setStatus("no reading list: add a reading section to config.yml");
    return;
  }
  if (!on && !canPutAway("reading")) {
    setStatus("Reading is all Today shows, so it stays");
    return;
  }
  if (on) writeSetting(TODAY_READING, "on");
  else removeSetting(TODAY_READING);
  if (!on && state.tasks.section === "reading") state.tasks.section = firstSection();
  await loadTodayReading();
  renderToolbar();
  if (state.tasks.view === "today") render();
  setStatus(on ? "Reading shown in Today" : "Reading put away from Today");
}
/** Choose what the section shows: the must-read items, or the new ones. */
export async function setTodayReadingShows(shows: ReadingShows): Promise<void> {
  if (shows === "new") writeSetting(TODAY_READING_SHOWS, "new");
  else removeSetting(TODAY_READING_SHOWS);
  state.reading.cursor = 0;
  await refreshTodayReading();
  setStatus(shows === "new" ? "Reading in Today: new items" : "Reading in Today: must read");
}
