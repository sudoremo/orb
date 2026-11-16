import { setStatus } from "./bridge";
import { state } from "./state";
import { describeCount } from "./sync";

/** Whether a row is selected, whether one by one or as part of the whole folder. */
export function isSelected(index: number): boolean {
  return state.selectAll || state.selected.has(index);
}
/** How many messages are selected, without enumerating a whole folder. */
export function selectionSize(): number {
  return state.selectAll ? state.total : state.selected.size;
}
/** Forget the selection, in whichever form it is held. */
export function clearSelection(): void {
  state.selected.clear();
  state.selectAll = false;
  state.anchor = null;
}
/**
 * Select every row between the anchor and `to`, inclusive.
 *
 * Replaces the range rather than adding to it, so dragging a shift-selection
 * back and forth grows and shrinks it instead of leaving a trail of rows
 * selected on the way out.
 */
export function selectRange(to: number): void {
  const from = state.anchor ?? state.messageCursor;
  state.anchor = from;
  state.selected.clear();
  state.selectAll = false;
  for (let i = Math.min(from, to); i <= Math.max(from, to); i++) {
    state.selected.add(i);
  }
  state.messageCursor = to;
}
/** Say how many are selected, since the row shading alone is easy to miss. */
export function reportSelection(): void {
  const n = selectionSize();
  // A threaded list's rows are conversations, each standing for its messages.
  const threaded = state.threads && !state.search && !state.filter?.hits;
  const what = threaded ? `${n} conversation${n === 1 ? "" : "s"}` : describeCount(n);
  setStatus(n === 0 ? "selection cleared" : `${what} selected`);
}
