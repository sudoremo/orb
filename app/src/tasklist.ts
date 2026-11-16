/**
 * The task list: the wire types, and the markup for a row.
 *
 * Everything here is pure — it takes records and returns HTML strings. State,
 * commands and the core calls live in `main.ts`, which is what keeps this file
 * readable and what lets the same row renderer draw all five views, the Today
 * section and the tasks hanging off a message without any of them growing a
 * renderer of their own.
 *
 * Nothing in this module is related to `tasks.ts`, which is the background-job
 * widget. The two only share a word. Every class name here is prefixed `tk-`,
 * partly for that reason — `.task-row` is already taken by the job widget, and
 * a collision would have styled one with the other's rules.
 */
import { daysBetween, localToday } from "./dates";
import { escape } from "./format";
import { IconName, svgIcon } from "./icons";

export { localToday };


// ---------------------------------------------------------------------------
// Types — mirror the serde shape of the `orb_core::ffi` task records.
// ---------------------------------------------------------------------------

/** The five views of §4.1, spelled as `TaskViewName` serialises them. */
export type TaskViewName = "today" | "upcoming" | "someday" | "completed" | "archive" | "trash";
/**
 * What the task half can show: a view of §4.1, or the tasks a search found
 * (§26), which no view of the core holds.
 */
export type TaskListView = TaskViewName | "search";
/** The views read a page at a time, newest completion first (§10.2, §18). */
export const isPaged = (view: TaskListView | null): boolean => view === "completed" || view === "archive";

export interface TaskTagInfo {
  /** The tag's uuid, which is what a task actually carries. */
  id: string;
  name: string;
  colour: string | null;
  position: number;
  /**
   * False when the registry cannot resolve the id. Such a tag renders as a
   * placeholder rather than vanishing (§7): a registry that has not synced yet
   * must not look like a task whose tags were removed.
   */
  is_known: boolean;
}

export interface TaskStepInfo {
  id: string;
  title: string;
  done: boolean;
}

export interface TaskSummary {
  id: number;
  /** The stable identity (§2). What a notification click carries. */
  uuid: string;
  title: string;
  /** Local civil dates, `YYYY-MM-DD`. */
  postpone_date: string | null;
  due_date: string | null;
  completed_at: string | null;
  deleted_at: string | null;
  tags: TaskTagInfo[];
  is_recurring: boolean;
  position: number;
  is_working_on: boolean;
  is_completed: boolean;
  /**
   * Past its due date and still open. Sent by the core rather than derived
   * here, so a list drawn at 23:59 and the list drawn a minute later agree with
   * the same authority on what "today" was (§4.3).
   */
  is_overdue: boolean;
  /** Edited locally and not yet confirmed by the server (§3.2). */
  is_dirty: boolean;
  linked_email_count: number;
  reminder_count: number;
  attachment_count: number;
  /** The checklist of §17: how many steps, and how many are done. */
  steps_total: number;
  steps_done: number;
  /**
   * A repeating task completed today: open again for its next date, and drawn
   * done in Today until midnight like any task completed today (§10.2).
   */
  is_done_for_today?: boolean;
  /**
   * The tag group the row stands under while the list is grouped by tag
   * (§30): a tag's id, or `""` for the tasks without one. A task with several
   * tags is a row under each.
   */
  tag_group?: string;
}

export interface TaskReminderInfo {
  id: number;
  remind_at: string;
  remind_at_unix: number;
  is_triggered: boolean;
  notification_id: string;
}

export interface TaskLinkInfo {
  message_id: string;
  subject: string | null;
  from: string | null;
  date: string | null;
  account_name: string | null;
  folder_name: string | null;
  /** `\Answered` on a stored copy, or a reply in a Sent folder. */
  answered: boolean;
  /** When the user last replied, where the reply is in a Sent folder. */
  replied_at: string | null;
  /**
   * Set for a chat message (todo-spec §28): `subject` is then its text,
   * `from` its author, `account_name` its room.
   */
  chat?: { rid: string; thread: string; message: string };
}

export interface TaskAttachmentInfo {
  id: number;
  attachment_uuid: string;
  filename: string;
  content_type: string | null;
  size: number | null;
  is_cached: boolean;
}

export interface TaskLogItem {
  entry_uuid: string;
  occurred_at: string;
  occurred_at_unix: number;
  device_id: string | null;
  event_type: string;
  text: string;
}

export interface TaskDetail {
  task: TaskSummary;
  body: string | null;
  recurrence: string | null;
  /** The rule in words, in the configured language. */
  recurrence_label: string | null;
  recurrence_from_completion: boolean;
  created_at: string;
  updated_at: string;
  revision: number;
  account_name: string;
  reminders: TaskReminderInfo[];
  links: TaskLinkInfo[];
  attachments: TaskAttachmentInfo[];
  log: TaskLogItem[];
  /** The checklist of §17, in order. */
  steps: TaskStepInfo[];
}

/**
 * An edit, with a `clear_*` flag beside every nullable field.
 *
 * JSON cannot tell "set this to nothing" from "leave it alone" — `null` and an
 * absent key deserialise the same — so the distinction is carried by a second
 * field. Sending `{}` therefore means "change nothing", never "clear
 * everything".
 */
export interface TaskEdit {
  title?: string;
  body?: string;
  clear_body?: boolean;
  postpone_date?: string;
  clear_postpone_date?: boolean;
  due_date?: string;
  clear_due_date?: boolean;
  tags?: string[];
  recurrence?: string;
  clear_recurrence?: boolean;
  recurrence_from_completion?: boolean;
  is_working_on?: boolean;
}

/** Where a task's linked message turned out to be (§6.2). */
export interface LinkedMessage {
  message_id: string;
  resolution: "local" | "server" | "cached";
  local_id: number | null;
  account_name: string | null;
  folder_name: string | null;
  subject: string | null;
  from: string | null;
  date: string | null;
}

/** How many open tasks one message has. Absent means none (§6.3). */
export interface MessageTaskCount {
  message_id: number;
  open_tasks: number;
}

/** What one task sync did (§3). */
export interface TaskSyncResult {
  listed: number;
  downloaded: number;
  uploaded: number;
  deleted: number;
  purged: number;
  expunged: number;
  rebound: number;
  attachments_uploaded: number;
  uidvalidity_reset: boolean;
  unreadable: number;
  errors: string[];
}

// ---------------------------------------------------------------------------
// The views
// ---------------------------------------------------------------------------

/** A view as the sidebar draws it. */
export interface TaskViewInfo {
  view: TaskViewName;
  label: string;
  icon: IconName;
  /**
   * Whether the entry belongs under the "Tasks" heading.
   *
   * Today does not: it is the landing view and it sits above the mailboxes,
   * and §4.1 is explicit that it appears exactly once — a second "Today" inside
   * the Tasks section would be two doors into one room.
   */
  inSection: boolean;
}

export const TASK_VIEWS: TaskViewInfo[] = [
  { view: "today", label: "Today", icon: "sun", inSection: false },
  { view: "upcoming", label: "Upcoming", icon: "cal", inSection: true },
  { view: "someday", label: "Someday", icon: "cloud", inSection: true },
  { view: "completed", label: "Completed", icon: "done", inSection: true },
  { view: "archive", label: "Archived", icon: "archive", inSection: true },
  { view: "trash", label: "Trash", icon: "trash", inSection: true },
];

/** Whether a string names a view, for a binding argument that could say anything. */
export function isTaskView(name: string): name is TaskViewName {
  return TASK_VIEWS.some((v) => v.view === name);
}

export function taskViewLabel(view: TaskListView): string {
  if (view === "search") return "Search";
  return TASK_VIEWS.find((v) => v.view === view)?.label ?? view;
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/**
 * A civil date as a person reads it, relative to today where that is shorter.
 *
 * "Yesterday" and "Tomorrow" are named; anything within the week either way is
 * given by weekday; the rest is `12 Sep`, with the year only when it is not
 * this one. A date column that says "2026-09-12" for everything is accurate and
 * unreadable, and the whole point of the column is being scannable.
 */
export function formatCivilDate(date: string, today: string): string {
  const delta = daysBetween(today, date);
  if (delta === 0) return "Today";
  if (delta === 1) return "Tomorrow";
  if (delta === -1) return "Yesterday";

  const parsed = new Date(`${date}T00:00:00`);
  if (Number.isNaN(parsed.getTime())) return date;
  if (delta > 1 && delta < 7) {
    return parsed.toLocaleDateString(undefined, { weekday: "long" });
  }
  const day = parsed.getDate();
  const month = MONTHS[parsed.getMonth()] ?? "";
  const sameYear = date.slice(0, 4) === today.slice(0, 4);
  return sameYear ? `${day} ${month}` : `${day} ${month} ${parsed.getFullYear()}`;
}

/** An RFC3339 instant as a short local date and time, for reminders and the log. */
export function formatInstant(value: string): string {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return value;
  return parsed.toLocaleString(undefined, {
    day: "numeric",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** How a due date is drawn: past, today, or still ahead. */
export type DeadlineKind = "overdue" | "due-today" | "due";

/**
 * Whether an open task's due date is past, is today, or is still to come.
 * Past is the core's word (`is_overdue`); a deadline that is today is drawn
 * as urgently, everywhere a due date is shown, since it is the last day to
 * keep the promise.
 */
export function deadlineKind(task: TaskSummary, today: string): DeadlineKind {
  if (task.is_overdue) return "overdue";
  const open = !task.is_completed && !task.is_done_for_today && !task.deleted_at;
  return open && task.due_date === today ? "due-today" : "due";
}

/** Whether a deadline has come: it is today, or it has passed. */
export const deadlineReached = (kind: DeadlineKind | "postponed" | "none"): boolean =>
  kind === "overdue" || kind === "due-today";

/**
 * What a row's date cell says, and how it should be coloured.
 *
 * The due date wins when a task has both: it is the date that is a promise,
 * while the postpone date has already done its work by letting the task appear
 * at all. `kind` is what §4.1 asks for — an overdue task has to be visually
 * distinct from a merely due one, and telling them apart is not something CSS
 * can do from the text.
 */
export function taskDateCell(
  task: TaskSummary,
  today: string,
): { text: string; kind: DeadlineKind | "postponed" | "none" } {
  if (task.due_date) {
    return {
      text: formatCivilDate(task.due_date, today),
      kind: deadlineKind(task, today),
    };
  }
  if (task.postpone_date) {
    return { text: formatCivilDate(task.postpone_date, today), kind: "postponed" };
  }
  return { text: "", kind: "none" };
}

/** One dated part of a row's date column. */
export interface TaskDateChip {
  text: string;
  kind: DeadlineKind | "postponed";
  /** What it is, for the tooltip — the colours alone cannot say it. */
  label: string;
}

/**
 * The dates a row shows, which is not the same question in every view.
 *
 * Today shows the due date alone. Everything in Today is scheduled for today or
 * earlier by definition — that is what put it there — so a postpone column would
 * repeat the name of the view on every row while the one date that varies, and
 * the only one that is a promise, competed with it for the space.
 *
 * The other views show both. There a task's schedule is the interesting fact —
 * *when* does this come back — and it cannot be read off the view the way
 * Today's can, so hiding it behind the due date would leave the two
 * indistinguishable on screen.
 */
export function taskDateChips(
  task: TaskSummary,
  today: string,
  view: TaskListView,
): TaskDateChip[] {
  const chips: TaskDateChip[] = [];
  // Search results stand under their date already (§26).
  if (view !== "today" && view !== "search" && task.postpone_date) {
    chips.push({
      text: formatCivilDate(task.postpone_date, today),
      kind: "postponed",
      label: "scheduled for",
    });
  }
  if (task.due_date) {
    const kind = deadlineKind(task, today);
    chips.push({
      text: formatCivilDate(task.due_date, today),
      kind,
      label: kind === "overdue" ? "overdue since" : "due",
    });
  }
  return chips;
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------


/** What a repeating task done for today says when it is ticked again. */
export function doneForTodayNote(task: TaskSummary, today: string): string {
  const next = task.postpone_date ?? task.due_date;
  return `done for today${next ? ` · back ${formatCivilDate(next, today)}` : ""}`;
}

/** A tag chip. Unknown ids are drawn as their id, marked, and never dropped. */
export function renderTag(tag: TaskTagInfo): string {
  const colour = tag.colour ? ` style="--tk-tag:${escape(tag.colour)}"` : "";
  const unknown = tag.is_known ? "" : " unknown";
  const title = tag.is_known ? "" : ` title="unresolved tag id — the registry has not synced"`;
  return `<span class="tk-tag${unknown}"${colour}${title}>${escape(tag.name)}</span>`;
}

export interface TaskRowOptions {
  /** Row index of the cursor within this list, or -1 when it is elsewhere. */
  cursor: number;
  /** Multi-selected row indices. */
  selected: Set<number>;
  /** Today, so relative dates are all computed against one instant. */
  today: string;
  /** Whether rows can be dragged to reorder (§4.4). */
  draggable: boolean;
  /** Which view is being drawn, which decides what the date column says. */
  view: TaskListView;
  /**
   * A heading over each group of rows: in Upcoming and search results the
   * date they come up on (§26), and by tag the tag they carry (§30).
   */
  grouping?: "date" | "tag";
  /** The tag registry, which names and colours the tag headings. */
  tags?: TaskTagInfo[];
  /**
   * Whether the tasks due by today stand above the rest (§31): under a Due
   * and an Other heading, or, within the groups of a tag, set apart by a line.
   */
  dueFirst?: boolean;
}

/** Whether a task is open and due today or earlier (§31). */
export function isDueNow(task: TaskSummary, today: string): boolean {
  return !task.is_completed && !task.is_done_for_today && !!task.due_date && task.due_date <= today;
}

/**
 * One task as a list row.
 *
 * The completion box is a real button so it can be clicked, and the row carries
 * its index rather than its id: the index is what the cursor, the selection and
 * a drag all speak in, and the id is one lookup away from it.
 */
function renderTaskRow(task: TaskSummary, index: number, options: TaskRowOptions): string {
  const classes = ["tk-row"];
  if (index === options.cursor) classes.push("cursor");
  if (options.selected.has(index)) classes.push("selected");
  const done = task.is_completed || !!task.is_done_for_today;
  if (done) classes.push("completed");
  if (task.is_overdue) classes.push("overdue");
  if (task.is_working_on) classes.push("working");
  if (task.is_dirty) classes.push("dirty");

  const dates = taskDateChips(task, options.today, options.view);
  const marks = [
    task.steps_total > 0 ? stepProgress(task.steps_done, task.steps_total) : "",
    task.is_recurring ? `<span class="m-repeat" title="repeats">${svgIcon("repeat")}</span>` : "",
    task.reminder_count > 0 ? `<span class="m-bell" title="reminders">${svgIcon("bell")}</span>` : "",
    task.attachment_count > 0 ? `<span class="m-clip" title="files">${svgIcon("clip")}</span>` : "",
    task.linked_email_count > 0 ? `<span class="m-mail" title="linked mail">${svgIcon("mail")}</span>` : "",
  ].join("");

  // A completed task keeps its box ticked so the state reads the same in the
  // Completed view as it does in the few hours it lingers in Today (§10.2).
  return `<div class="${classes.join(" ")}" role="option" aria-selected="${
    index === options.cursor || options.selected.has(index) ? "true" : "false"
  }" data-task="${index}"${options.draggable ? " data-reorder" : ""}>
    <button type="button" class="tk-box" data-task-toggle="${index}" tabindex="-1"
      title="${task.is_done_for_today ? "Done for today" : task.is_completed ? "Un-complete" : "Complete"}"><span class="chk${done ? " on" : ""}"></span></button>
    <span class="tk-title">${escape(task.title || "(untitled)")}</span>
    <span class="tk-meta">${task.tags.map(rowTag).join("")}${marks}</span>
    <span class="tk-dates">${dates
      .map(
        (chip) =>
          `<span class="tk-date ${chip.kind}" title="${escape(chip.label)} ${escape(
            chip.text,
          )}">${escape(chip.text)}</span>`,
      )
      .join("")}</span>
  </div>`;
}

/** A task's steps done of all, as a ring and a count (§17). */
function stepProgress(done: number, total: number): string {
  // The arc's length on a ring of radius 6: 2πr ≈ 37.7.
  const arc = ((37.7 * done) / total).toFixed(1);
  return `<span class="m-steps" title="${done} of ${total} steps done"><svg class="ring" viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="6"/><circle class="arc" cx="8" cy="8" r="6" stroke-dasharray="${arc} 40"/></svg><span class="n">${done}/${total}</span></span>`;
}
/** A tag among a row's badges: its name after a `#`, in its colour. */
function rowTag(tag: TaskTagInfo): string {
  const colour = tag.colour ? ` style="--tk-tag:${escape(tag.colour)}"` : "";
  const unknown = tag.is_known ? "" : " unknown";
  const title = tag.is_known ? "" : ` title="unresolved tag id — the registry has not synced"`;
  return `<span class="tk-tagtxt${unknown}"${colour}${title}>#${escape(tag.name)}</span>`;
}

/**
 * A whole list of tasks, or a line saying why there is none.
 *
 * The empty line is per view rather than a generic "nothing here": an empty
 * Today means the day is done, an empty Trash means nothing was deleted, and
 * those are different enough to be worth saying.
 */
export function renderTaskRows(
  tasks: TaskSummary[],
  options: TaskRowOptions,
  empty: string,
): string {
  if (tasks.length === 0) return `<div class="tk-empty">${escape(empty)}</div>`;
  const due = (task: TaskSummary | undefined) => !!options.dueFirst && !!task && isDueNow(task, options.today);
  if (!options.grouping) {
    const count = tasks.filter(due).length;
    let band = "";
    return tasks
      .map((task, index) => {
        let markup = "";
        if (count > 0 && due(task) && band !== "due") {
          markup = groupHeading({ key: "due", label: "Due", icon: "alert" }, `<span class="n">${count}</span>`);
          band = "due";
        } else if (band === "due" && !due(task)) {
          markup = groupHeading({ key: "other", label: "Other", icon: "sun" });
          band = "other";
        }
        return markup + renderTaskRow(task, index, options);
      })
      .join("");
  }
  const byTag = options.grouping === "tag";
  const headOf = (task: TaskSummary) => (byTag ? tagGroup(task, options.tags ?? []) : searchGroup(task, options.today));
  // Under a tag, how many tasks carry it.
  const sizes = new Map<string, number>();
  if (byTag) for (const task of tasks) sizes.set(headOf(task).key, (sizes.get(headOf(task).key) ?? 0) + 1);
  let group = "";
  return tasks
    .map((task, index) => {
      const head = headOf(task);
      const fold = head.key === "done" ? `<span class="fold" title="Fold them (za)">▾</span>` : "";
      const size = byTag ? `<span class="n">${sizes.get(head.key)}</span>` : "";
      // Within a tag, the line under the tasks due by today (§31).
      const after = head.key === group && due(tasks[index - 1]) && !due(task);
      const markup =
        head.key === group ? (after ? `<div class="tk-band-end" aria-hidden="true"></div>` : "") : groupHeading(head, fold + size);
      group = head.key;
      return markup + renderTaskRow(task, index, options);
    })
    .join("");
}

/** A heading of Upcoming or of search results (§26): a date, a month, a year, or a view's name. */
export interface TaskGroup {
  key: string;
  label: string;
  /** The day of the month, drawn big; a month's name takes its place. */
  big?: string;
  icon?: IconName;
  /** A tag's colour, for its heading's icon (§30). */
  colour?: string | null;
}

/** The key of the group of tasks without a tag (§30). */
const UNTAGGED = "";

/**
 * The rows grouped by tag (§30): a group for each tag, in the registry's
 * order, then one for the tasks without a tag. A task with several tags is a
 * row under each, a copy marked with the group it stands in. Within a group
 * the rows keep the order they came in.
 */
export function groupByTag(rows: TaskSummary[], tags: TaskTagInfo[]): TaskSummary[] {
  const order = new Map(tags.map((tag, at) => [tag.id, at]));
  const groups = new Map<string, TaskSummary[]>();
  for (const row of rows) {
    const keys = row.tags.length > 0 ? [...new Set(row.tags.map((tag) => tag.id))] : [UNTAGGED];
    for (const key of keys) {
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key)!.push({ ...row, tag_group: key });
    }
  }
  // A tag the registry has not synced yet comes after the known ones, and the
  // tasks without one last of all.
  const rank = (key: string) => (key === UNTAGGED ? Infinity : order.get(key) ?? tags.length);
  return [...groups.entries()]
    .map(([key, group], seen) => ({ key, group, seen }))
    .sort((a, b) => rank(a.key) - rank(b.key) || a.seen - b.seen)
    .flatMap(({ group }) => group);
}

/** Which heading a row stands under when the list is grouped by tag (§30). */
export function tagGroup(task: TaskSummary, tags: TaskTagInfo[]): TaskGroup {
  const key = task.tag_group ?? UNTAGGED;
  if (key === UNTAGGED) return { key: "tag:", label: "No tag", icon: "tag" };
  const tag = tags.find((t) => t.id === key) ?? task.tags.find((t) => t.id === key);
  return { key: `tag:${key}`, label: tag?.name ?? key, icon: "tag", colour: tag?.colour };
}

/** How many tasks the rows are, a task under several tags counted once. */
export function distinctTasks(rows: TaskSummary[]): number {
  return new Set(rows.map((row) => row.id)).size;
}

const MONTH_NAMES = ["January", "February", "March", "April", "May", "June", "July",
  "August", "September", "October", "November", "December"];
const WEEKDAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/**
 * Which heading a task stands under in Upcoming and in search results (§26):
 * by the date it comes up, the earlier of postpone and due date, as Today
 * and Upcoming read it.
 */
export function searchGroup(task: TaskSummary, today: string): TaskGroup {
  if (task.is_completed) return { key: "done", label: "Completed", icon: "done" };
  const [date] = [task.postpone_date, task.due_date].filter((d): d is string => !!d).sort();
  if (!date) return { key: "someday", label: "Someday", icon: "cloud" };
  if (date <= today) return { key: "today", label: "Today", icon: "sun" };
  const days = daysBetween(today, date);
  const [year, month, day] = date.split("-").map(Number);
  if (days <= 6) {
    const weekday = WEEKDAYS[new Date(Date.UTC(year, month - 1, day)).getUTCDay()];
    return { key: date, big: String(day), label: days === 1 ? "Tomorrow" : weekday };
  }
  const [thisYear, thisMonth] = today.split("-").map(Number);
  if ((year - thisYear) * 12 + month - thisMonth >= 12) return { key: date.slice(0, 4), label: "", big: String(year) };
  const name = MONTH_NAMES[month - 1] + (year === thisYear ? "" : ` ${year}`);
  return { key: date.slice(0, 7), label: "", big: name };
}

/** A group's heading, as the mockup's Search tasks state draws it. */
export function groupHeading(group: TaskGroup, extra = ""): string {
  const big = group.big
    ? `<b class="${/^\d{4}-\d{2}-\d{2}$/.test(group.key) ? "" : "mon"}">${escape(group.big)}</b>`
    : `<b class="dic${group.key === "today" ? " sunny" : ""}${group.key === "tag:" ? " untagged" : ""}"${
        group.colour ? ` style="--tk-tag:${escape(group.colour)}"` : ""
      }>${group.icon ? svgIcon(group.icon) : ""}</b>`;
  const label = group.label ? `<span class="lbl">${escape(group.label)}</span>` : "";
  const toggle = group.key === "done" ? " data-search-completed" : "";
  return `<div class="tk-day" data-group="${escape(group.key)}"${toggle}>${big}<span class="dl">${label}${extra}</span></div>`;
}

/** What an empty view should say. */
export function emptyTaskMessage(view: TaskListView, searching: boolean): string {
  if (searching || view === "search") return "No task matches";
  switch (view) {
    case "today":
      return "Nothing due today";
    case "upcoming":
      return "Nothing scheduled";
    case "someday":
      return "No undated tasks";
    case "completed":
      return "Nothing completed yet";
    case "archive":
      return "Nothing archived yet";
    case "trash":
      return "The trash is empty";
  }
}

/** "1 task" / "12 tasks", for a status line that has to say how many. */
export function describeTasks(n: number): string {
  return `${n} task${n === 1 ? "" : "s"}`;
}

/**
 * The search box's placeholder for a task view.
 *
 * Names the one term the language has, because `tag:` is not guessable and a
 * search box that silently treats `tag:work` as two words is worse than one
 * that never offered it.
 */
export function taskSearchPlaceholder(view: TaskListView): string {
  return `Search ${taskViewLabel(view)}  —  tag:name`;
}
