import { TaskDetail, TaskEdit, TaskSeriesInfo, TaskTagInfo, describeTasks } from "./tasklist";
import { groupUndo, offerUndo } from "./moving";
import { runTask } from "./tasks";
import { openPicker } from "./picker";
import { eventKey } from "./keys";
import { DatePickerField, DatePickerResult, openDatePicker, DateSuggestion } from "./datepicker";
import { call, setStatus } from "./bridge";
import { runCommand } from "./commands";
import { render } from "./list";
import { renderReader } from "./reader";
import { typingInAField } from "./search";
import { el, state } from "./state";
import { keyFor, loadTaskDetail, refreshTasks, targetTaskIds } from "./taskview";

/** One of the open editor's fields, by name. */
function taskEditorInput(field: "title" | "body"): HTMLInputElement | HTMLTextAreaElement | null {
  const id = field === "title" ? "tk-title-input" : "tk-body-input";
  return document.getElementById(id) as HTMLInputElement | HTMLTextAreaElement | null;
}
/**
 * The field the caret belongs in, which is the title whenever there is one.
 *
 * With both open the title is where writing starts — it is the required field
 * and the one at the top — and Tab reaches the notes from there.
 */
function taskEditorField(): HTMLInputElement | HTMLTextAreaElement | null {
  const editing = state.tasks.editing;
  if (!editing) return null;
  return taskEditorInput(editing === "body" ? "body" : "title");
}
/**
 * Put the caret in the field that has just been drawn.
 *
 * At the end rather than at the start: a task created from a message arrives
 * with its subject already in the field (§6.1), and what you want to do with it
 * is add to it or wipe it, not type in front of it.
 */
export function focusTaskEditor(): void {
  if (!state.tasks.editing) return;
  // Any of the editor's own fields counts as "already there". With the title
  // and the notes both open, checking only the field this would land on would
  // pull the caret out of the notes and back to the top on every repaint.
  const active = document.activeElement;
  if (active === taskEditorInput("title") || active === taskEditorInput("body")) return;
  const field = taskEditorField();
  if (!field) return;
  field.focus();
  field.setSelectionRange(field.value.length, field.value.length);
}
/**
 * Move the caret between the fields the editor has open.
 *
 * Wraps rather than falling out of the last field: there are two of them, and
 * leaving the editor is what accepting and cancelling are for. With one field
 * open there is nowhere to go, and Tab does nothing rather than something
 * surprising.
 */
export function moveTaskEditField(step: 1 | -1): void {
  if (state.tasks.editing !== "both") return;
  const fields = [taskEditorInput("title"), taskEditorInput("body")];
  const here = fields.findIndex((field) => field === document.activeElement);
  if (here < 0) return;
  const next = fields[(here + step + fields.length) % fields.length];
  if (!next) return;
  next.focus();
  next.setSelectionRange(next.value.length, next.value.length);
}
export function beginTaskEdit(field: "title" | "body" | "both"): void {
  if (!state.tasks.detail) {
    setStatus("no task open");
    return;
  }
  state.tasks.editing = field;
  state.focus = "reader";
  render();
}
/** Write the field back, then reload — a title change re-sorts nothing but does re-render. */
export async function acceptTaskEdit(): Promise<void> {
  const editing = state.tasks.editing;
  const detail = state.tasks.detail;
  if (!editing || !detail) return;

  const titleField = editing === "body" ? null : taskEditorInput("title");
  const bodyField = editing === "title" ? null : taskEditorInput("body");
  if (!titleField && !bodyField) return;

  if (titleField && !titleField.value.trim()) {
    // A task that was never named and is accepted empty was never wanted. The
    // field showed a placeholder rather than prefilled text, so an empty one is
    // "I have typed nothing" — the same answer escaping gives, and it gets the
    // same treatment rather than an error about a rule the user has not broken.
    if (state.tasks.provisional === detail.task.id && !bodyField?.value.trim()) {
      await cancelTaskEdit();
      return;
    }
    // §2: a title is required. Refusing here rather than at the database keeps
    // the field open with the text still in it, which is the only place the
    // mistake can be corrected.
    setStatus("a task needs a title");
    return;
  }

  // One edit for however many fields were open, so a title and a note written
  // together are one revision, one log entry apiece and one undo — not two
  // edits that can half-fail and leave the pane disagreeing with the store.
  const edit: TaskEdit = {};
  if (titleField) edit.title = titleField.value.trim();
  if (bodyField) {
    // An emptied body is a decision, and only the explicit flag can say so: an
    // absent `body` means "leave it alone".
    if (bodyField.value.trim()) edit.body = bodyField.value;
    else edit.clear_body = true;
  }

  state.tasks.editing = null;
  // Naming it is what makes it real, so the task stops being provisional the
  // moment the title is accepted — including when the typed title happens to be
  // the placeholder itself.
  if (titleField) state.tasks.provisional = null;
  await call("update_task", { id: detail.task.id, edit });
  await refreshTasks();
  setStatus(`saved · ${keyFor("undo") ?? "undo"} to take it back`);
}
/**
 * Show the tags that already exist, next to a command line waiting for one.
 *
 * A name typed from memory creates a near-duplicate the first time the memory
 * is wrong — `urgent` beside `Urgent` — and the registry is the only place that
 * knows. Not a completion popover: the generic one this application will grow
 * belongs to every field at once, and a bespoke one for tags would be in its
 * way. The list is truncated because the status line is one line, and a
 * registry long enough to overflow it is one to search rather than to read.
 */
/** A tag by its name, ignoring case. */
export function findTag(name: string): TaskTagInfo | undefined {
  return state.tasks.tags.find((tag) => tag.name.toLowerCase() === name.toLowerCase());
}
/** Attach files to a task, one after another, and say how many arrived (§9). */
export async function attachFilesToTask(taskId: number, paths: string[]): Promise<void> {
  let attached = 0;
  const said = () => `attached ${attached} file${attached === 1 ? "" : "s"}`;
  const endGroup = await groupUndo();
  try {
    for (const path of paths) {
      await runTask(
        { id: "task-attachment", label: "Attaching", detail: path.split("/").pop() ?? path },
        () => call("add_task_attachment", { task: taskId, path }),
      );
      attached += 1;
    }
  } finally {
    await endGroup(said().replace("attached", "attach"));
  }
  await refreshTasks();
  if (attached > 0) offerUndo(said());
  else setStatus(said());
}
/** The snooze a reminder offers, as minutes from now. */
export async function pickSnooze(): Promise<number | null> {
  const nine = new Date();
  nine.setDate(nine.getDate() + 1);
  nine.setHours(9, 0, 0, 0);
  const untilNine = Math.max(1, Math.round((nine.getTime() - Date.now()) / 60_000));
  const picked = await openPicker({
    title: "Snooze",
    placeholder: "for how long",
    items: [
      { value: "10", label: "10 minutes" },
      { value: "60", label: "1 hour" },
      { value: "180", label: "3 hours" },
      { value: String(untilNine), label: "Until tomorrow 9:00" },
    ],
  });
  return picked ? Number(picked) : null;
}
export function offerTags(ifEmpty: string): void {
  const names = state.tasks.tags.map((tag) => tag.name);
  if (!names.length) {
    setStatus(`no tags yet — ${ifEmpty}`);
    return;
  }
  const shown = names.slice(0, TAGS_SHOWN).join(", ");
  const rest = names.length - TAGS_SHOWN;
  setStatus(rest > 0 ? `tags: ${shown} … and ${rest} more` : `tags: ${shown}`);
}
/** How many tag names fit in a status line without pushing the prompt off it. */
const TAGS_SHOWN = 12;
export async function cancelTaskEdit(): Promise<void> {
  const provisional = state.tasks.provisional;
  state.tasks.editing = null;
  state.tasks.provisional = null;

  // A task abandoned before it was ever named is erased rather than trashed:
  // there is nothing to restore, and a trash full of blank tasks is its own
  // small mess. Erasing rather than hiding also means the message never reaches
  // another machine, or is expunged from it if a sync got there first.
  // Only if the cursor never left it. Moving to another task and cancelling an
  // edit there must not reach back and erase this one.
  if (provisional !== null && state.tasks.detail?.task.id === provisional) {
    await call("discard_task", { id: provisional });
    state.tasks.detail = null;
    await refreshTasks();
    // Back where `create-task` was pressed: the list has the keyboard, and the
    // pane shows the task now under the cursor rather than the field it had.
    state.focus = "list";
    if (state.tasks.rows.length > 0) await loadTaskDetail(false);
    render();
    el.list.focus({ preventScroll: true });
    setStatus("discarded");
    return;
  }

  renderReader();
  setStatus("edit cancelled");
}
/**
 * Keys typed into a task field.
 *
 * The global keymap never sees these — it steps aside for anything aimed at an
 * input — so `bindings.task_edit` is resolved here instead. `<enter>` in the
 * body is left to the textarea whatever the table says: a task body is prose,
 * and a field that cannot take a newline is not one.
 */
el.reader.addEventListener("keydown", (e) => {
  if (!state.tasks.editing) return;
  if (!typingInAField(e.target)) return;
  const key = eventKey(e);
  if (e.target instanceof HTMLTextAreaElement && key === "<enter>") return;
  const command = state.bindings.task_edit[key];
  if (!command) return;
  e.preventDefault();
  e.stopPropagation();
  void runCommand(command, 1);
});
/**
 * Open the date picker, wired to this window's core bridge.
 *
 * The picker takes its lookup as a seam so it does not have to know about
 * `call`; passing it means a failed parse is timed and reported like every
 * other core call rather than disappearing into the console.
 */
export function pickDate(
  mode: "date" | "reminder",
  title: string,
  initial: string | null,
  fields?: DatePickerField[],
  /**
   * The day of a recurring task's occurrence (§32), when the date is its
   * deadline or a reminder of it: what is typed may then be said against that
   * day, and every reading says how far from it it is.
   */
  day: string | null = null,
): Promise<DatePickerResult | null> {
  return openDatePicker({
    mode,
    title,
    fields,
    initial,
    placeholder: !day
      ? undefined
      : mode === "reminder"
        ? "on the day 9am, day before 18:00, -2 14:00"
        : "on the day, 2 days after, +3",
    suggest: (input, which) =>
      call<DateSuggestion[]>(
        which === "reminder" ? "suggest_datetimes" : "suggest_dates",
        day ? { input, day } : { input },
      ),
    onProblem: setStatus,
  });
}
/**
 * The series of a task (§32), or null for one that does not repeat or has no
 * day yet. The rows say whether a task repeats; the series is on its detail.
 */
export async function seriesOf(id: number): Promise<TaskSeriesInfo | null> {
  const row = state.tasks.rows.find((r) => r.id === id);
  if (row && !row.is_recurring) return null;
  const detail =
    state.tasks.detail?.task.id === id
      ? state.tasks.detail
      : await call<TaskDetail>("get_task", { id }).catch(() => null);
  return detail?.series ?? null;
}
/**
 * The date a task created right now should be scheduled for.
 *
 * Today is the only view that gives one. A task made in Today is a task meant
 * for today, and without the date it has none at all — which puts it in
 * Someday, out of sight of the list that asked for it.
 *
 * Upcoming deliberately gives none. Any date that would put a task there is a
 * guess at *which* future day was meant, and a wrong date is worse than no date:
 * one is corrected by the picker, the other is a promise nobody made.
 */
export function newTaskPostponeDate(): string | null {
  return state.tasks.view === "today" ? state.tasks.today : null;
}
/**
 * Set one of the two dates over the current selection.
 *
 * A cancelled picker changes nothing and a cleared one clears — the distinction
 * the picker's `null` versus `{kind:"clear"}` exists to carry, and the reason
 * unsetting a date is as easy as setting one (§12).
 */
export async function setTaskDate(which: "postpone" | "due"): Promise<void> {
  const ids = targetTaskIds();
  if (ids.length === 0) {
    setStatus("no task selected");
    return;
  }
  const task = state.tasks.rows.find((row) => row.id === ids[0]);
  const initial = which === "postpone" ? task?.postpone_date : task?.due_date;
  // The deadline of one recurring task is said against its day (§32); over
  // several tasks there is no one day to say it against.
  const series = which === "due" && ids.length === 1 ? await seriesOf(ids[0]!) : null;
  const day = series && !series.day_is_due ? series.day : null;
  const chosen = await pickDate(
    "date",
    which === "postpone" ? "Postpone until" : day ? "Due, from its day" : "Due",
    initial ?? day,
    undefined,
    day,
  );
  if (!chosen) return;

  const date = chosen.kind === "clear" ? null : chosen.date;
  const command = which === "postpone" ? "set_task_postpone_date" : "set_task_due_date";
  await call(command, { ids, date });
  await refreshTasks();
  setStatus(
    `${describeTasks(ids.length)} · ${which} ${date ?? "cleared"} · ${
      keyFor("undo") ?? "undo"
    } to undo`,
  );
}
/**
 * Schedule and due on one date, from one picker.
 *
 * "Do this on Friday, and it is due Friday" is the ordinary case, and asking for
 * it twice through two pickers is two chances to set one of them to the wrong
 * day. Sent as a single edit rather than as two calls, so the pair is one undo
 * step rather than a half-applied schedule that needs taking back twice.
 */
export async function setBothTaskDates(): Promise<void> {
  const ids = targetTaskIds();
  if (ids.length === 0) {
    setStatus("no task selected");
    return;
  }
  const task = state.tasks.rows.find((row) => row.id === ids[0]);
  // The due date seeds the picker: it is the date that is a promise, so where
  // the two already disagree it is the one worth showing back.
  const chosen = await pickDate(
    "date",
    "Schedule and due",
    task?.due_date ?? task?.postpone_date ?? null,
  );
  if (!chosen) return;

  const date = chosen.kind === "clear" ? null : chosen.date;
  const edit: TaskEdit = date
    ? { postpone_date: date, due_date: date }
    : { clear_postpone_date: true, clear_due_date: true };
  await call("update_tasks", { ids, edit });
  await refreshTasks();
  setStatus(
    `${describeTasks(ids.length)} · scheduled and due ${date ?? "cleared"} · ${
      keyFor("undo") ?? "undo"
    } to undo`,
  );
}
