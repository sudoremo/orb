/**
 * Maximize: the list fills the window, and the panes beside it become drawers
 * over it rather than taking its width.
 *
 * Laid over the list rather than beside it, so its columns stay as wide as
 * maximizing made them while a message is read. The folders slide in from
 * the left while they have the keyboard. The reader is out over the right
 * from the start, the keyboard staying in the list so `j`/`k` leaf through
 * the messages in it; `q` or `<esc>` puts it away, and giving it the keyboard
 * brings it back. Maximized from the reader, the drawer covers the list.
 *
 * In Today the reader is below the half the keyboard is in instead, taking a
 * row of its own as it does unmaximized: a sheet laid over the bottom of the
 * list would cover its rows, the cursor's often among them, and the list's
 * width is not at stake.
 *
 * The sidebar can be put away on its own too, the list and the reader taking
 * its width; the folders then come out as the same drawer while they have
 * the keyboard.
 */

import { cancelMarkRead, previewCursor, scrollCursorIntoView, scrollTaskCursorIntoView } from "./actions";
import { svgIcon } from "./icons";
import { render } from "./list";
import { readSetting, writeSetting } from "./splitters";
import { el, state, todayHasMail, WINDOW } from "./state";

/** Per window: the tasks window keeps its folders whatever the mailbox does. */
const SIDEBAR_HIDDEN = `.sidebarHidden.${WINDOW}`;

state.sidebarHidden = readSetting(SIDEBAR_HIDDEN) === "1";
el.sidebarToggle.innerHTML = svgIcon("sidebar");
el.sidebarToggle.addEventListener("click", () => toggleSidebar());

/** Whether the reader is put away, so nothing is to be read into it. */
export function readerHidden(): boolean {
  return state.maximized && !state.drawer;
}

/**
 * Bring the layout in line with the focus: the reader's drawer comes out with
 * the keyboard, and a wide one, which leaves nothing of the list to look at,
 * goes when the keyboard leaves it.
 */
export function applyMaximize(): void {
  if (state.maximized && state.focus === "reader") state.drawer = true;
  if (state.drawerWide && state.focus !== "reader") {
    state.drawer = false;
    state.drawerWide = false;
  }
  el.app.classList.toggle("maximized", state.maximized);
  el.app.classList.toggle("sidebar-hidden", state.sidebarHidden);
  el.app.classList.toggle(
    "folders-open",
    (state.maximized || state.sidebarHidden) && state.focus === "folders",
  );
  el.sidebarToggle.title = `${state.sidebarHidden ? "Show" : "Hide"} the sidebar (zs)`;
  el.sidebarToggle.setAttribute("aria-pressed", String(!state.sidebarHidden));
  el.app.classList.toggle("drawer-open", state.maximized && state.drawer);
  el.app.classList.toggle("drawer-wide", state.maximized && state.drawerWide);
  const below = state.maximized && state.tasks.view === "today" && todayHasMail();
  el.app.classList.toggle("drawer-below", below);
  // The reader coming or going below moves the list's bottom edge, which the
  // cursor may be past.
  const shared = below && state.drawer && !state.drawerWide;
  if (shared !== listShared) {
    listShared = shared;
    requestAnimationFrame(() => {
      if (state.tasks.section === "tasks") scrollTaskCursorIntoView();
      else if (state.tasks.section === "reading") el.list.querySelector(".rd-row.cursor")?.scrollIntoView({ block: "nearest" });
      else if (state.tasks.section === "chat") el.list.querySelector(".ch-row.cursor")?.scrollIntoView({ block: "nearest" });
      else scrollCursorIntoView();
    });
  }
}
/** Whether the list shares its height with the reader below it, as last drawn. */
let listShared = false;

/** Maximize the focused pane, or bring every pane back. */
export function toggleMaximize(): void {
  state.maximized = !state.maximized;
  state.drawer = state.maximized;
  state.drawerWide = state.maximized && state.focus === "reader";
  if (state.maximized && state.focus === "folders") state.focus = "list";
  render();
  // The reader passed over the messages the cursor met while it was away.
  previewCursor();
}

/** Put the folders away, or bring them back beside the list. */
export function toggleSidebar(): void {
  state.sidebarHidden = !state.sidebarHidden;
  writeSetting(SIDEBAR_HIDDEN, state.sidebarHidden ? "1" : "0");
  if (state.sidebarHidden && state.focus === "folders") state.focus = "list";
  render();
}

/** Put the reader's drawer away; false when it is not out. */
export function closeDrawer(): boolean {
  if (!state.maximized || !state.drawer) return false;
  state.drawer = false;
  state.drawerWide = false;
  if (state.focus === "reader") state.focus = "list";
  cancelMarkRead();
  render();
  return true;
}
