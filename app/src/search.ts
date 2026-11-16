import { runCommand } from "./commands";
import { render } from "./list";
import { runSearchQuery } from "./moving";
import { applyChosen, fieldKind, fieldQuery, fieldScope, fieldUnchanged, onFieldInput, onFieldKey, openField } from "./searchfield";
import { el, state, todayHasMail } from "./state";

export function focusSearch(): void {
  openField();
  el.searchOverlay.hidden = false;
  el.searchInput.focus();
  el.searchInput.select();
}
/** Put the field away and hand the keyboard back to the list. */
function closeSearch(): void {
  el.searchOverlay.hidden = true;
  el.searchInput.blur();
  state.focus = "list";
  render();
}
/** Show whether a search is active, so leaving it is discoverable. */
export function renderSearchBox(): void {
  el.searchClear.hidden = el.searchInput.value.length === 0 && !state.search;
}
/**
 * Point the search field at what it searches.
 *
 * `/` searches everywhere from every view (§4.5): mail in every folder and
 * every task, and in the tasks window the tasks. Narrowing what is on screen
 * is the filter's job, a gesture of its own.
 */
export function syncSearchBox(): void {
  el.searchInput.placeholder = todayHasMail() ? "Search mail and tasks" : "Search tasks";
  if (!state.search) el.searchInput.value = "";
  renderSearchBox();
}
/**
 * Whether typing should go to the page rather than to the app's keymap.
 *
 * Without this, `j` in the search field would move the message cursor instead
 * of typing a letter.
 */
export function typingInAField(target: EventTarget | null): boolean {
  return target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement;
}
el.searchInput.addEventListener("input", () => {
  renderSearchBox();
  onFieldInput();
});
el.searchInput.addEventListener("keydown", (e) => {
  if (onFieldKey(e)) {
    e.preventDefault();
    return;
  }
  if (e.key === "Enter") {
    e.preventDefault();
    applyChosen();
    const query = fieldQuery();
    if (!query) return;
    void runSearchQuery(query, query, undefined, fieldScope(), fieldKind());
    // Focus follows the results: the point of searching is to read them.
    closeSearch();
    return;
  }

  if (e.key === "Escape") {
    e.preventDefault();
    // Escape puts the field away and keeps the search when the query was
    // changed, so a refinement can be abandoned. Untouched, or emptied, it
    // leaves the search too.
    if (state.search && (!fieldQuery() || fieldUnchanged())) void runCommand("clear-search", 1);
    closeSearch();
  }
});
// A click beside the field puts it away as Escape would, without leaving the
// search; so does focus moving elsewhere in the window. Switching to another
// app leaves it open for the return.
el.searchOverlay.addEventListener("mousedown", (e) => {
  if (e.target === el.searchOverlay) {
    e.preventDefault();
    closeSearch();
  }
});
el.searchInput.addEventListener("blur", (e) => {
  const to = e.relatedTarget;
  if (to instanceof Node && !el.searchOverlay.contains(to)) el.searchOverlay.hidden = true;
});
// The field keeps the keyboard while the button is pressed.
el.searchClear.addEventListener("mousedown", (e) => e.preventDefault());
el.searchClear.addEventListener("click", () => {
  void runCommand("clear-search", 1);
  el.searchInput.focus();
});
