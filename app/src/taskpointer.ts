import { offerUndo } from "./moving";
import { runTask } from "./tasks";
import { clamp, scrollTaskCursorIntoView } from "./actions";
import { call, setStatus } from "./bridge";
import { runCommand } from "./commands";
import { openTaskMenu } from "./menus";
import { el, state } from "./state";
import { beginStepEdit, editSteps, stepsToRemove } from "./stepsui";
import { pickDate, pickSnooze, setTaskDate } from "./taskedit";
import { openItemWindow } from "./itemwindow";
import { loadMoreCompleted, openLinkedMessage, openTaskById, paintTaskCursor, previewTaskCursor, refreshTasks, renderTaskList, sameOrderGroup } from "./taskview";
import { describeTasks, doneForTodayNote, type TaskSummary } from "./tasklist";

/**
 * Move one task to a position in the list.
 *
 * `to` is where it lands in the list as the user sees it *after* the move, so
 * the neighbours are read from the list with the dragged row already taken out
 * — otherwise dragging downwards is off by one, which is the classic way a
 * reorder ends up one place short of where it was dropped.
 *
 * One call, one row written: `position` is a float and the core puts the task
 * between its new neighbours rather than renumbering the list.
 */
async function moveTaskTo(from: number, to: number): Promise<void> {
  const task = state.tasks.rows[from];
  if (!task) return;
  const rest = state.tasks.rows.filter((_, index) => index !== from);
  const { previous, next } = neighbours(task, rest, clamp(to, rest.length));
  await call(
    "move_task",
    { id: task.id, before: previous?.id ?? null, after: next?.id ?? null },
  );
  await refreshTasks();
}
/**
 * The rows a task landing at `landing` in `rest` goes between: the nearest on
 * either side in its own order group, which are the ones the global order
 * puts around it.
 */
function neighbours(task: TaskSummary, rest: TaskSummary[], landing: number): { previous: TaskSummary | null; next: TaskSummary | null } {
  const ordered = (row: TaskSummary) => sameOrderGroup(task, row);
  return {
    previous: rest.slice(0, landing).reverse().find(ordered) ?? null,
    next: rest.slice(landing).find(ordered) ?? null,
  };
}
/**
 * Move several rows, in list order, as one block to `to`, an index in the
 * list as it stands. Rows that were apart land together.
 */
async function moveTasksTo(indexes: number[], to: number): Promise<number> {
  const ids = [...new Set(indexes.map((index) => state.tasks.rows[index]!.id))];
  // By id: a task under two tags moves out of both rows (§30).
  const stays = (row: TaskSummary) => !ids.includes(row.id);
  const rest = state.tasks.rows.filter(stays);
  const landing = state.tasks.rows.slice(0, to).filter(stays).length;
  const { previous, next } = neighbours(state.tasks.rows[indexes[0]!]!, rest, landing);
  await call("move_tasks", {
    ids,
    before: previous?.id ?? null,
    after: next?.id ?? null,
  });
  await refreshTasks();
  setStatus(`moved ${describeTasks(ids.length)}`);
  return landing;
}
/** `move-task-up` / `move-task-down`: the keyboard's version of a drag. */
export async function moveTaskBy(delta: number): Promise<void> {
  const from = state.tasks.cursor;
  if (!state.tasks.rows[from]) {
    setStatus("no task selected");
    return;
  }
  // Within the task's own group: a step past its edge has no place in the
  // global order to go to.
  const task = state.tasks.rows[from]!;
  let to = clamp(from + delta, state.tasks.rows.length - 1);
  while (to !== from && !sameOrderGroup(task, state.tasks.rows[to]!)) to -= Math.sign(delta);
  if (to === from) return;
  await moveTaskTo(from, to);
  state.tasks.cursor = to;
  renderTaskList();
  scrollTaskCursorIntoView();
}
/**
 * Reorder by dragging a row (§4.4), with pointer events rather than HTML
 * drag and drop: the window's own file drop takes the webview's drag events
 * in the app, so `dragover` and `drop` never arrive there. A press becomes a
 * drag once the pointer has moved a few pixels; a shorter one stays a click.
 */
let press: { from: number; y: number; row: HTMLElement; dragging: boolean; group: number[] } | null =
  null;
/** Swallow the click that ends a drag, which would otherwise open the task. */
let swallowClick = false;
const DRAG_THRESHOLD = 4;

/** The row under the pointer and whether the drop lands below its middle. */
function dropTarget(e: PointerEvent): { row: HTMLElement; below: boolean } | null {
  const under = document.elementFromPoint(e.clientX, e.clientY);
  const row = under?.closest<HTMLElement>("#list [data-task][data-reorder]");
  if (!row) return null;
  const rect = row.getBoundingClientRect();
  return { row, below: e.clientY > rect.top + rect.height / 2 };
}
el.list.addEventListener("pointerdown", (e) => {
  if (e.button !== 0 || e.metaKey || e.shiftKey) return;
  const target = e.target as HTMLElement;
  if (target.closest("[data-task-toggle]")) return;
  const row = target.closest<HTMLElement>("[data-task][data-reorder]");
  if (!row) return;
  const from = Number(row.dataset.task);
  // A selected row carries the whole selection with it.
  const group =
    state.tasks.selected.has(from) && state.tasks.selected.size > 1
      ? [...state.tasks.selected].sort((a, b) => a - b)
      : [from];
  press = { from, y: e.clientY, row, dragging: false, group };
});
window.addEventListener("pointermove", (e) => {
  if (!press) return;
  if (!press.dragging) {
    if (Math.abs(e.clientY - press.y) < DRAG_THRESHOLD) return;
    press.dragging = true;
    // Dragging a row outside the selection takes that row alone, and the
    // selection lets go, as it does when the row is clicked.
    if (press.group.length === 1 && !state.tasks.selected.has(press.from)) {
      state.tasks.selected.clear();
      state.tasks.anchor = press.from;
      paintTaskCursor();
    }
    for (const index of press.group) {
      el.list.querySelector(`[data-task="${index}"]`)?.classList.add("dragging");
    }
    document.body.classList.add("reordering");
  }
  for (const marked of el.list.querySelectorAll(".drop-above, .drop-below")) {
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
  // redraws the list leaves none, and the next real click must count.
  swallowClick = true;
  setTimeout(() => (swallowClick = false), 0);
  const drop = dropTarget(e);
  clearDropMarks();
  if (!drop) return;
  const from = was.from;
  const to = Number(drop.row.dataset.task) + (drop.below ? 1 : 0);
  if (was.group.length > 1) {
    // A block that is already together and lands within or beside itself
    // stays where it is; one that was apart closes up there.
    const first = was.group[0]!;
    const last = was.group[was.group.length - 1]!;
    if (last - first === was.group.length - 1 && to >= first && to <= last + 1) return;
    void moveTasksTo(was.group, to).then((landing) => {
      state.tasks.cursor = landing;
      renderTaskList();
    });
    return;
  }
  // Landing either side of itself is where it already is.
  if (to === from || to === from + 1) return;
  const landing = to > from ? to - 1 : to;
  void moveTaskTo(from, landing).then(() => {
    state.tasks.cursor = landing;
    renderTaskList();
  });
});
window.addEventListener("pointercancel", () => {
  press = null;
  clearDropMarks();
});
el.list.addEventListener(
  "click",
  (e) => {
    if (!swallowClick) return;
    swallowClick = false;
    e.stopImmediatePropagation();
  },
  true,
);
function clearDropMarks(): void {
  document.body.classList.remove("reordering");
  for (const marked of el.list.querySelectorAll(".drop-above, .drop-below, .dragging")) {
    marked.classList.remove("drop-above", "drop-below", "dragging");
  }
}
el.list.addEventListener("contextmenu", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-task]");
  if (!row) return;
  e.preventDefault();
  state.tasks.cursor = Number(row.dataset.task);
  state.tasks.section = "tasks";
  state.focus = "list";
  // The menu acts on the selection the row is in, or on the row alone: never
  // on a selection somewhere else in the list.
  if (!state.tasks.selected.has(state.tasks.cursor)) {
    state.tasks.selected.clear();
    state.tasks.anchor = state.tasks.cursor;
  }
  paintTaskCursor();
  openTaskMenu();
});
el.list.addEventListener("click", (e) => {
  const target = e.target as HTMLElement;

  if (target.closest("[data-task-more]")) {
    void loadMoreCompleted();
    return;
  }
  if (target.closest("[data-task-empty-trash]")) {
    void runCommand("empty-task-trash", 1);
    return;
  }

  const box = target.closest<HTMLElement>("[data-task-toggle]");
  if (box) {
    const index = Number(box.dataset.taskToggle);
    const task = state.tasks.rows[index];
    if (!task) return;
    if (task.is_done_for_today) {
      setStatus(doneForTodayNote(task, state.tasks.today));
      return;
    }
    // The box acts on the row it is on, not on the selection: it is a click on
    // one checkbox, and treating it as a bulk action would complete twelve
    // tasks because twelve happened to be highlighted.
    void call(task.is_completed ? "uncomplete_tasks" : "complete_tasks", { ids: [task.id] })
      .then(refreshTasks)
      .catch(() => {
        // `call` has already reported it.
      });
    return;
  }

  const row = target.closest<HTMLElement>("[data-task]");
  if (!row) return;
  // The row clicked before a first ⌘-click is part of the selection it
  // starts, as in any list.
  if (e.metaKey && state.tasks.selected.size === 0 && state.tasks.anchor !== null) {
    state.tasks.selected.add(state.tasks.anchor);
  }
  state.tasks.cursor = Number(row.dataset.task);
  state.tasks.section = "tasks";
  state.focus = "list";
  if (e.metaKey) {
    if (state.tasks.selected.has(state.tasks.cursor)) {
      state.tasks.selected.delete(state.tasks.cursor);
    } else {
      state.tasks.selected.add(state.tasks.cursor);
    }
  } else if (e.shiftKey && state.tasks.anchor !== null) {
    selectTaskRange(state.tasks.cursor);
  } else {
    state.tasks.selected.clear();
    state.tasks.anchor = state.tasks.cursor;
  }
  paintTaskCursor();
  previewTaskCursor();
});
// A double-click opens the task in a window of its own.
el.list.addEventListener("dblclick", (e) => {
  if (e.shiftKey || e.metaKey) return;
  const target = e.target as HTMLElement;
  if (target.closest("[data-task-toggle]")) return;
  const row = target.closest<HTMLElement>("[data-task]");
  const task = row ? state.tasks.rows[Number(row.dataset.task)] : undefined;
  if (task) void openItemWindow("task", task.id, task.title).catch(() => {});
});
/** Replace the task selection with the range from the anchor to `to`. */
export function selectTaskRange(to: number): void {
  const from = state.tasks.anchor ?? to;
  state.tasks.anchor = from;
  state.tasks.selected.clear();
  for (let i = Math.min(from, to); i <= Math.max(from, to); i++) state.tasks.selected.add(i);
}
/**
 * The detail pane's own buttons, and the linked tasks under a message.
 *
 * Delegated, because both blocks are rebuilt by `innerHTML` on every render,
 * and both name the action they stand for rather than carrying a closure — the
 * same discipline the context menu follows.
 */
el.reader.addEventListener("click", (e) => {
  const target = e.target as HTMLElement;

  const onMessage = target.closest<HTMLElement>("[data-message-task]");
  if (onMessage) {
    const id = Number(onMessage.dataset.messageTask);
    const action = onMessage.dataset.taskAction ?? "open";
    void actOnMessageTask(id, action).catch(() => {
      // `call` has already reported it.
    });
    return;
  }

  const button = target.closest<HTMLElement>("[data-task-action]");
  if (!button) return;
  void actOnTaskDetail(button).catch(() => {
    // `call` has already reported it.
  });
});
/** Complete, postpone or open one of the tasks listed under a message (§6.3). */
async function actOnMessageTask(id: number, action: string): Promise<void> {
  if (action === "complete" || action === "uncomplete") {
    await call(action === "complete" ? "complete_tasks" : "uncomplete_tasks", { ids: [id] });
    await refreshTasks();
    return;
  }
  if (action === "postpone") {
    const chosen = await pickDate("date", "Postpone until", null);
    if (!chosen) return;
    await call("set_task_postpone_date", {
      ids: [id],
      date: chosen.kind === "clear" ? null : chosen.date,
    });
    await refreshTasks();
    return;
  }
  if (!(await openTaskById(id))) setStatus("that task is no longer in any view");
}
/** A button inside the task detail pane. */
async function actOnTaskDetail(button: HTMLElement): Promise<void> {
  const detail = state.tasks.detail;
  if (!detail) return;
  const action = button.dataset.taskAction;

  if (action === "complete" || action === "uncomplete") {
    await call(action === "complete" ? "complete_tasks" : "uncomplete_tasks", { ids: [detail.task.id] });
    await refreshTasks();
    return;
  }
  if (action === "delete-reminder") {
    await call("delete_task_reminder", { id: Number(button.dataset.reminder) });
    await refreshTasks();
    offerUndo("removed the reminder");
    return;
  }
  if (action === "snooze-reminder") {
    const minutes = await pickSnooze();
    if (minutes === null) return;
    await call("snooze_task_reminder", { id: Number(button.dataset.reminder), minutes });
    await refreshTasks();
    setStatus(`snoozed for ${minutes} minutes`);
    return;
  }
  // The buttons beside the fields run the commands the keys run, on the task
  // the pane shows (§5.3 pointer coverage).
  if (action === "set-postpone" || action === "set-due") {
    await setTaskDate(action === "set-postpone" ? "postpone" : "due");
    return;
  }
  if (action === "clear-postpone" || action === "clear-due") {
    const command = action === "clear-postpone" ? "set_task_postpone_date" : "set_task_due_date";
    await call(command, { ids: [detail.task.id], date: null });
    await refreshTasks();
    return;
  }
  if (action === "add-tag") {
    await runCommand("add-tag", 1);
    return;
  }
  if (action === "remove-tag") {
    await call("remove_task_tag", { ids: [detail.task.id], tag: button.dataset.tag ?? "" });
    await refreshTasks();
    return;
  }
  if (action === "add-reminder") {
    await runCommand("add-reminder", 1);
    return;
  }
  if (action === "notification-settings") {
    await runCommand("notification-settings", 1);
    return;
  }
  if (action === "set-recurrence") {
    await runCommand("set-recurrence", 1);
    return;
  }
  if (action === "clear-recurrence") {
    await runCommand("clear-recurrence", 1);
    return;
  }
  if (action === "attach-file") {
    await runCommand("attach-file", 1);
    return;
  }
  if (action === "delete-attachment") {
    await call("delete_task_attachment", { id: Number(button.dataset.attachmentId) });
    await refreshTasks();
    offerUndo("removed the attachment");
    return;
  }
  if (action === "open-attachment") {
    const uuid = button.dataset.attachment ?? "";
    // A cache miss is a download, which is exactly the kind of wait the task
    // widget exists for (§9).
    const opened = await runTask({ id: "task-attachment", label: "Opening attachment" }, () =>
      call<{ path: string; revealed: boolean; declined: boolean }>(
        "open_task_attachment",
        { task: detail.task.id, attachment: uuid },
      ),
    );
    if (opened.declined) {
      setStatus("not opened");
      return;
    }
    // The payload's home is the cache, so downloading it and saving it are the
    // same act and there is nothing to choose a location for. Something the
    // system would run rather than display is revealed in Finder instead, and
    // says so — opening it is then a decision rather than a side effect of
    // clicking a row.
    const name = opened.path.split("/").pop() ?? opened.path;
    setStatus(opened.revealed ? `${name} revealed in Finder` : `opened ${name}`);
    return;
  }
  if (action === "toggle-step" || action === "remove-step") {
    const id = button.dataset.step ?? "";
    const step = detail.steps.find((s) => s.id === id);
    if (!step) return;
    await editSteps(
      detail,
      action === "remove-step"
        ? { op: "remove", ids: stepsToRemove(detail, id) }
        : { op: "done", ids: [id], done: !step.done },
    );
    return;
  }
  if (action === "work-on") {
    await runCommand("work-on", 1);
    return;
  }
  if (action === "add-step") {
    beginStepEdit("new");
    return;
  }
  if (action === "unlink") {
    await call("unlink_message_from_task", {
      task: detail.task.id,
      message: button.dataset.messageId ?? "",
    });
    await refreshTasks();
    offerUndo("unlinked the message");
    return;
  }
  if (action === "open-link") {
    await openLinkedMessage(detail.task.id, button.dataset.messageId ?? "");
  }
}
