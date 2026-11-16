import { call, setStatus } from "./bridge";
import { render } from "./list";
import { renderReader } from "./reader";
import { el, state } from "./state";
import { TaskDetail } from "./tasklist";
import { refreshTasks } from "./taskview";

/**
 * The checklist's pointer (§17): a click selects a step, ⌘-click adds or
 * takes one away, Shift-click selects the range from the last one clicked;
 * a drag moves a step, or the selection it is in as one block. Pointer
 * events rather than HTML drag and drop, for the reason the task list
 * gives: the window's file drop takes the webview's drag events in the app.
 */

/** The task the selection belongs to; another task's detail starts afresh. */
let selectionTask: number | null = null;

/** The selection as it stands, emptied when the detail is another task's. */
function selection(detail: TaskDetail): Set<string> {
  if (selectionTask !== detail.task.id) {
    selectionTask = detail.task.id;
    state.tasks.stepSelection.clear();
    state.tasks.stepAnchor = null;
  }
  for (const id of state.tasks.stepSelection) {
    if (!detail.steps.some((s) => s.id === id)) state.tasks.stepSelection.delete(id);
  }
  return state.tasks.stepSelection;
}

/** Change the checklist and show the result. */
export async function editSteps(detail: TaskDetail, edit: Record<string, unknown>): Promise<void> {
  state.tasks.detail = await call<TaskDetail>("edit_task_step", { id: detail.task.id, edit });
  selection(state.tasks.detail);
  await refreshTasks();
}

/**
 * The steps a removal means: the selection when the step named is in it, or
 * there is no step named; else that step alone.
 */
export function stepsToRemove(detail: TaskDetail, named: string | null): string[] {
  const selected = selection(detail);
  if (named === null) return [...selected];
  return selected.has(named) ? [...selected] : [named];
}

function select(detail: TaskDetail, id: string, e: MouseEvent): void {
  const selected = selection(detail);
  if (e.metaKey) {
    if (selected.has(id)) selected.delete(id);
    else selected.add(id);
    state.tasks.stepAnchor = id;
  } else if (e.shiftKey && state.tasks.stepAnchor) {
    const ids = detail.steps.map((s) => s.id);
    const [a, b] = [ids.indexOf(state.tasks.stepAnchor), ids.indexOf(id)].sort((x, y) => x - y);
    selected.clear();
    for (const step of ids.slice(Math.max(0, a!), b! + 1)) selected.add(step);
  } else {
    selected.clear();
    selected.add(id);
    state.tasks.stepAnchor = id;
  }
  renderReader();
}

let press: { id: string; y: number; dragging: boolean; group: string[] } | null = null;
/** Swallow the click that ends a drag, which would otherwise select. */
let swallowClick = false;
const DRAG_THRESHOLD = 4;

/** The step under the pointer and whether the drop lands below its middle. */
function dropTarget(e: PointerEvent): { row: HTMLElement; below: boolean } | null {
  const row = document.elementFromPoint(e.clientX, e.clientY)?.closest<HTMLElement>("#reader [data-step-row]");
  if (!row) return null;
  const rect = row.getBoundingClientRect();
  return { row, below: e.clientY > rect.top + rect.height / 2 };
}

function clearMarks(): void {
  document.body.classList.remove("reordering");
  for (const marked of el.reader.querySelectorAll(".drop-above, .drop-below, .dragging")) {
    marked.classList.remove("drop-above", "drop-below", "dragging");
  }
}

el.reader.addEventListener("pointerdown", (e) => {
  if (e.button !== 0 || e.metaKey || e.shiftKey) return;
  const target = e.target as HTMLElement;
  if (target.closest("button, input")) return;
  const row = target.closest<HTMLElement>("[data-step-row]");
  const detail = state.tasks.detail;
  if (!row || !detail) return;
  const id = row.dataset.stepRow!;
  // A selected step carries the whole selection with it.
  const selected = selectedSteps(detail);
  press = { id, y: e.clientY, dragging: false, group: selected.includes(id) ? selected : [id] };
});

window.addEventListener("pointermove", (e) => {
  if (!press) return;
  if (!press.dragging) {
    if (Math.abs(e.clientY - press.y) < DRAG_THRESHOLD) return;
    press.dragging = true;
    // A step outside the selection goes alone, and the selection lets go.
    const detail = state.tasks.detail;
    if (detail && press.group.length === 1 && !selection(detail).has(press.id)) {
      state.tasks.stepSelection.clear();
      state.tasks.stepAnchor = press.id;
      renderReader();
    }
    for (const id of press.group) {
      el.reader.querySelector(`[data-step-row="${CSS.escape(id)}"]`)?.classList.add("dragging");
    }
    document.body.classList.add("reordering");
  }
  for (const marked of el.reader.querySelectorAll(".drop-above, .drop-below")) {
    marked.classList.remove("drop-above", "drop-below");
  }
  const drop = dropTarget(e);
  drop?.row.classList.add(drop.below ? "drop-below" : "drop-above");
});

window.addEventListener("pointerup", (e) => {
  const was = press;
  press = null;
  if (!was?.dragging) return;
  // The click that ends a drag follows at once, if at all: a drop that
  // redraws the checklist leaves none, and the next real click must count.
  swallowClick = true;
  setTimeout(() => (swallowClick = false), 0);
  const drop = dropTarget(e);
  clearMarks();
  const detail = state.tasks.detail;
  if (!drop || !detail) return;
  const ids = detail.steps.map((s) => s.id);
  const moving = new Set(was.group);
  const at = ids.indexOf(drop.row.dataset.stepRow!) + (drop.below ? 1 : 0);
  // Counted in the checklist with the moving steps already taken out of it.
  const to = ids.slice(0, at).filter((id) => !moving.has(id)).length;
  const after = ids.filter((id) => !moving.has(id));
  after.splice(to, 0, ...ids.filter((id) => moving.has(id)));
  if (after.every((id, index) => id === ids[index])) return;
  void editSteps(detail, { op: "move", ids: was.group, to }).catch(() => {
    // `call` has already said why.
  });
});

window.addEventListener("pointercancel", () => {
  press = null;
  clearMarks();
});

el.reader.addEventListener(
  "click",
  (e) => {
    if (swallowClick) {
      swallowClick = false;
      e.stopImmediatePropagation();
      return;
    }
    const target = e.target as HTMLElement;
    if (target.closest("button, input")) return;
    const row = target.closest<HTMLElement>("[data-step-row]");
    const detail = state.tasks.detail;
    if (!row || !detail) return;
    // A double-click writes the step in place. Counted on the click rather
    // than a `dblclick`, which never comes: the first click redrew the row.
    if (e.detail === 2 && !e.metaKey && !e.shiftKey) {
      beginStepEdit(row.dataset.stepRow!);
      return;
    }
    select(detail, row.dataset.stepRow!, e);
    setStatus(
      state.tasks.stepSelection.size > 1
        ? `${state.tasks.stepSelection.size} steps selected · x ticks them, ⌫ removes them`
        : state.status,
    );
  },
  true,
);

/**
 * Write a step in place: `"new"` opens an empty one at the end of the
 * checklist, a step's id opens its title. Enter keeps it, and after a new
 * one opens the next, so a list is typed in one go; Escape, or Enter on an
 * empty new one, leaves.
 */
export function beginStepEdit(target: string): void {
  if (!state.tasks.detail) {
    setStatus("no task open");
    return;
  }
  state.tasks.editing = null;
  state.tasks.stepEditing = target;
  fresh = true;
  state.focus = "reader";
  render();
}

function stepField(): HTMLInputElement | null {
  return el.reader.querySelector<HTMLInputElement>("#tk-step-input");
}

/** Set when the step field opens anew, so a redraw does not carry the old field's text into it. */
let fresh = false;
/** Set while the pane is drawn again, which blurs the field without anyone leaving it. */
let redrawing = false;

/**
 * Draw the pane with `draw`, keeping an open step field as it was: its text,
 * its caret and the keyboard. A sync landing or any other redraw would
 * otherwise replace it with what is stored, and its blur would close it.
 */
export function redrawKeepingStepField(draw: () => void): void {
  const old = stepField();
  const kept =
    old && state.tasks.stepEditing && !fresh
      ? { value: old.value, start: old.selectionStart, end: old.selectionEnd, focused: document.activeElement === old }
      : null;
  fresh = false;
  redrawing = true;
  try {
    draw();
  } finally {
    redrawing = false;
  }
  const field = stepField();
  // The step being renamed went away, deleted on another machine say.
  if (state.tasks.stepEditing && !field && state.tasks.detail) state.tasks.stepEditing = null;
  if (!kept || !field) return;
  field.value = kept.value;
  if (kept.focused) field.focus();
  field.setSelectionRange(kept.start ?? kept.value.length, kept.end ?? kept.value.length);
}

/** Put the caret in the step field that has just been drawn. */
export function focusStepEditor(): void {
  const field = stepField();
  if (!state.tasks.stepEditing || !field || document.activeElement === field) return;
  field.focus();
  field.setSelectionRange(field.value.length, field.value.length);
}

/** Set while a step is being written back, so the field's own blur does not write it twice. */
let committing = false;

async function commitStepEdit(again: boolean): Promise<void> {
  const detail = state.tasks.detail;
  const target = state.tasks.stepEditing;
  const field = stepField();
  if (!detail || !target || !field || committing) return;
  const title = field.value.trim();
  committing = true;
  try {
    fresh = true;
    if (target === "new") {
      state.tasks.stepEditing = again && title ? "new" : null;
      if (title) await editSteps(detail, { op: "add", title });
    } else {
      state.tasks.stepEditing = null;
      const step = detail.steps.find((s) => s.id === target);
      if (step && title && title !== step.title) await editSteps(detail, { op: "rename", id: target, title });
    }
  } finally {
    // Drawn before the flag drops: replacing the field blurs it.
    renderReader();
    committing = false;
  }
}

function cancelStepEdit(): void {
  state.tasks.stepEditing = null;
  committing = true;
  renderReader();
  committing = false;
}

el.reader.addEventListener("keydown", (e) => {
  if ((e.target as HTMLElement).id !== "tk-step-input") return;
  if (e.key === "Enter" && !e.isComposing) {
    e.preventDefault();
    e.stopPropagation();
    void commitStepEdit(true);
  } else if (e.key === "Escape") {
    e.preventDefault();
    e.stopPropagation();
    cancelStepEdit();
  }
});

// Clicking elsewhere keeps what was typed, as leaving any field does.
el.reader.addEventListener("focusout", (e) => {
  if ((e.target as HTMLElement).id !== "tk-step-input" || committing || redrawing) return;
  void commitStepEdit(false);
});

/** Select the step right-clicked, unless it is already among the selected, and show what can be done to it. */
el.reader.addEventListener("contextmenu", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-step-row]");
  const detail = state.tasks.detail;
  if (!row || !detail) return;
  e.preventDefault();
  const id = row.dataset.stepRow!;
  const selected = selection(detail);
  if (!selected.has(id)) {
    selected.clear();
    selected.add(id);
    state.tasks.stepAnchor = id;
    renderReader();
  }
  const steps = detail.steps.filter((s) => selected.has(s.id));
  void call("open_step_menu", {
    disabled: steps.length > 1 ? ["edit-step"] : [],
    labels: {
      "toggle-step": steps.every((s) => s.done) ? "Reopen" : "Mark as Done",
      "remove-step": steps.length > 1 ? `Delete ${steps.length} Steps` : "Delete Step",
    },
  });
});

/** The steps selected in the open task's checklist, in their order. */
export function selectedSteps(detail: TaskDetail): string[] {
  const selected = selection(detail);
  return detail.steps.filter((s) => selected.has(s.id)).map((s) => s.id);
}
