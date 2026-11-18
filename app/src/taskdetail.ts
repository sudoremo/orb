/**
 * The task detail pane, and the tasks that hang off a message.
 *
 * Both live here because both are "a task, drawn at length" — the detail pane
 * in the reader when a task is open, and the linked-task block under a
 * message's headers (§6.3). Sharing the renderer is what keeps a task looking
 * like a task wherever it is seen, and stops the second one drifting into a
 * lesser copy of the first.
 *
 * Pure, like `tasklist.ts`: records in, HTML out. Every interactive element
 * carries a `data-` attribute naming the command it stands for, so the click
 * handlers in `main.ts` go through the same command registry the keyboard does
 * and a button cannot come to mean something its keystroke does not.
 */

import { colourStyle, escape, icon, shortWhen } from "./format";
import { IconName, svgIcon } from "./icons";
import { linkify } from "./links";
import { NotificationPermission } from "./model";
import {
  TaskAttachmentInfo,
  TaskDetail,
  TaskLinkInfo,
  TaskLogItem,
  TaskReminderInfo,
  TaskStepInfo,
  TaskSummary,
  formatCivilDate,
  formatInstant,
  renderTag,
  taskDateCell,
  deadlineReached,
} from "./tasklist";


/** A byte count as a person reads it. */
function fileSize(bytes: number | null): string {
  if (bytes === null || bytes < 0) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export interface TaskDetailOptions {
  /** Today, so every relative date in the pane agrees with the list's. */
  today: string;
  /** The steps selected in the checklist, by id (§17). */
  selectedSteps?: Set<string>;
  /** The step being written in place, or `"new"` for one being added. */
  stepEditing?: string | null;
  /**
   * Which field is being edited, if any.
   *
   * Editing happens in place rather than in a dialog: the title is the first
   * thing on screen either way, and a task created from a message is meant to
   * be renamed in the same breath (§6.1).
   *
   * `"both"` opens the title and the notes together, which is what `edit-task`
   * with no argument does. A task's name and what it is actually about are
   * written in one sitting, and making the notes a second command meant they
   * were usually not written at all. Tab moves between the two; the pane is
   * still not full of live inputs, because nothing else in it is one.
   */
  editing: "title" | "body" | "both" | null;
  /**
   * Whether this task exists only to be named — created by `create-task` and
   * erased if the naming is abandoned. It is what turns the title field from a
   * prefilled edit into an empty one with a placeholder.
   */
  provisional?: boolean;
  /** Whether the pane has the keyboard, which the header's rule reflects. */
  focused: boolean;
  /** Whether reminders can reach the notification centre. */
  notifications: NotificationPermission;
  /** The key each command is bound to, for the line of what can be added. */
  keys?: Record<string, string | null>;
}

/**
 * What an unnamed task is called, in the list and as the field's placeholder.
 *
 * One constant for both, because they have to agree: the row on screen while
 * the title is being typed is this task, and a row reading something other than
 * the placeholder beside it would look like a second task.
 */
export const PROVISIONAL_TITLE = "New task";

/** A small button running a detail-pane action. */
function fieldButton(action: string, glyph: string, title: string, extra = ""): string {
  return `<button type="button" class="tk-field-button" data-task-action="${action}"${extra}
    title="${escape(title)}">${icon(glyph)}</button>`;
}

/**
 * A property that is set, as a chip: its icon, a word where the value alone
 * does not say what it is, the value. A click changes it; the cross clears it.
 * `data-k` and `.pk` carry the label for a look that draws `Key value` lines.
 */
function chip(
  key: string,
  glyph: IconName,
  value: string,
  options: { word?: string; cls?: string; action?: string; clear?: string; extra?: string; title?: string } = {},
): string {
  const open = options.action ? ` data-task-action="${options.action}"${options.extra ?? ""}` : "";
  const word = options.word ? `<span class="pk">${escape(options.word)}</span>` : "";
  const clear = options.clear
    ? fieldButton(options.clear, "fa-xmark", `Clear ${key.toLowerCase()}`, options.extra ?? "")
    : "";
  return `<span class="p${options.cls ? ` ${options.cls}` : ""}" data-k="${escape(key)}"${
    options.title ? ` title="${escape(options.title)}"` : ""
  }><button type="button" class="p-open"${open}>${svgIcon(glyph)}${word}<span class="pv">${value}</span></button>${clear}</span>`;
}

/**
 * Why reminders will not arrive, where they are set.
 *
 * macOS asks only once; after a refusal only System Settings can grant the
 * permission again, so the notice carries the way there.
 */
function renderBlockedNotice(permission: NotificationPermission): string {
  if (permission === "denied") {
    return `<div class="tk-blocked">${icon("fa-bell-slash")} macOS blocks Orb's notifications, so reminders will not appear.
      <button type="button" data-task-action="notification-settings">Open System Settings</button></div>`;
  }
  if (permission === "unavailable") {
    return `<div class="tk-blocked">${icon("fa-bell-slash")} This build cannot post notifications (it is not an app bundle), so reminders will not appear.</div>`;
  }
  return "";
}

/** A reminder (§5) as a chip; snoozing and removing it are its own buttons. */
function reminderChip(reminder: TaskReminderInfo): string {
  const id = ` data-reminder="${reminder.id}"`;
  // "Fired" is per machine and deliberately not synced (§5.2), so it is said
  // quietly rather than drawn as a state of the task itself.
  const fired = reminder.is_triggered ? ` <span class="dim">fired here</span>` : "";
  return `<span class="p${reminder.is_triggered ? " fired" : ""}" data-k="Remind"><button type="button" class="p-open" data-task-action="snooze-reminder"${id} title="Snooze this reminder">${svgIcon(
    "bell",
  )}<span class="pk">Remind</span><span class="pv">${escape(formatInstant(reminder.remind_at))}${fired}</span></button>${fieldButton(
    "delete-reminder",
    "fa-xmark",
    "Remove this reminder",
    id,
  )}</span>`;
}

/**
 * The checklist (§17): each step with its box, the first open one marked as
 * the next, and a way to take one away on hover. A click on the box ticks it;
 * a click on the step selects it, a double-click renames it, and a drag moves
 * it (`stepsui.ts`). A step being added is an empty field at the end.
 */
function renderSteps(steps: TaskStepInfo[], selectedSteps: Set<string>, editing: string | null): string {
  if (steps.length === 0 && editing !== "new") return "";
  const field = (value: string) =>
    `<input id="tk-step-input" class="st st-input" type="text" value="${escape(value)}" placeholder="New step" autocomplete="off" spellcheck="false">`;
  const done = steps.filter((s) => s.done).length;
  const next = steps.findIndex((s) => !s.done);
  const rows = steps
    .map((step, i) => {
      const id = ` data-step="${escape(step.id)}"`;
      const selected = selectedSteps.has(step.id) ? " selected" : "";
      return `<div class="step${step.done ? " done" : ""}${i === next ? " next" : ""}${selected}" data-step-row="${escape(step.id)}">
          <button type="button" class="tk-box" data-task-action="toggle-step"${id} title="${
            step.done ? "Reopen" : "Done"
          }"><span class="chk${step.done ? " on" : ""}"></span></button>
          ${editing === step.id ? field(step.title) : `<span class="st">${escape(step.title)}</span>`}${fieldButton("remove-step", "fa-xmark", "Remove this step", id)}</div>`;
    })
    .join("")
    + (editing === "new"
      ? `<div class="step adding"><span class="tk-box"><span class="chk"></span></span>${field("")}</div>`
      : "");
  return `<div class="td-h"><span class="lbl">Steps</span><span class="n">${done}/${steps.length}</span><span class="pbar"><i style="width:${Math.round(
    (100 * done) / Math.max(1, steps.length),
  )}%"></i></span></div><div class="steps">${rows}</div>`;
}

/**
 * The linked messages (§6.1), drawn from the task's own cache: a message that
 * has been moved, deleted or lost with its account still reads as a real line
 * here, and opening it is what goes looking (§6.2).
 */
function renderLinks(links: TaskLinkInfo[]): string {
  if (links.length === 0) return "";
  const rows = links
    .map((link) => {
      const id = ` data-message-id="${escape(link.message_id)}"`;
      // An unanswered message is usually why the task exists, so it says so.
      const status = link.answered
        ? `<span class="rs ok">${svgIcon("reply")}<span class="lbl">Replied</span>${
            link.replied_at ? ` ${escape(shortWhen(link.replied_at))}` : ""
          }</span>`
        : `<span class="rs no"><span class="lbl">No reply</span></span>`;
      // A chat message shows its room before its text.
      const what = link.chat
        ? `<span class="lk chat">${svgIcon("chat")}</span><b>${escape(link.from ?? "")}</b>
          <span class="su">${link.account_name ? `<span class="dim">${escape(link.account_name)}</span> ` : ""}${escape(link.subject || "(no text)")}</span>`
        : `<span class="lk mail">${svgIcon("mail")}</span><b>${escape(link.from ?? "")}</b>
          <span class="su">${escape(link.subject || (link.from ? "(no subject)" : link.message_id))}</span>`;
      return `<div class="td-link"><button type="button" class="td-open" data-task-action="open-link"${id}>
          ${what}
          ${status}<span class="r">${link.date ? escape(link.date.slice(0, 10)) : ""}</span></button>${fieldButton(
            "unlink",
            "fa-xmark",
            "Remove this link",
            id,
          )}</div>`;
    })
    .join("");
  return `<div class="td-h"><span class="lbl">Linked</span><span class="n">${links.length}</span></div>${rows}`;
}

/** The task's files (§9). Payloads are fetched on demand, not here. */
function renderFiles(attachments: TaskAttachmentInfo[]): string {
  if (attachments.length === 0) return "";
  const rows = attachments
    .map(
      (attachment) => `<div class="td-link"><button type="button" class="td-open" data-task-action="open-attachment"
          data-attachment="${escape(attachment.attachment_uuid)}">
          <span class="lk file">${svgIcon("clip")}</span><b>${escape(attachment.filename)}</b>
          <span class="r">${escape(fileSize(attachment.size))}${
            // Whether opening it costs a round trip is worth knowing before the
            // click, not after it (§9).
            attachment.is_cached ? "" : " · not downloaded"
          }</span></button>${fieldButton(
            "delete-attachment",
            "fa-xmark",
            "Remove this attachment",
            ` data-attachment-id="${attachment.id}"`,
          )}</div>`,
    )
    .join("");
  return `<div class="td-h"><span class="lbl">Files</span><span class="n">${attachments.length}</span></div>${rows}`;
}

/**
 * The log (§11), newest last, so it reads downwards like a conversation. The
 * device is shown only when there is more than one in the log: on a single
 * machine it is noise, and across machines it is the answer to "where did that
 * come from".
 */
function renderLog(log: TaskLogItem[]): string {
  if (log.length === 0) return "";
  const devices = new Set(log.map((entry) => entry.device_id ?? ""));
  const showDevice = devices.size > 1;
  const rows = log
    .map(
      (entry) => `<li class="tk-event ${escape(entry.event_type)}">
        <time>${escape(formatInstant(entry.occurred_at))}</time>
        <span>${escape(entry.text)}${
          showDevice && entry.device_id ? ` <span class="dim">${escape(entry.device_id)}</span>` : ""
        }</span>
      </li>`,
    )
    .join("");
  return `<div class="td-h"><span class="lbl">History</span></div><ol class="td-log">${rows}</ol>`;
}

/**
 * The whole detail pane: the title with its box, what is set as chips, one
 * line naming the keys for what is not, the notes, the linked mail, the files
 * and the history.
 *
 * The title and body are inputs while they are being edited and plain text
 * otherwise, rather than always-editable fields: a pane full of live inputs
 * swallows every keystroke, and the keyboard is how this application is driven.
 */
export function renderTaskDetail(detail: TaskDetail, options: TaskDetailOptions): string {
  const task = detail.task;
  const date = taskDateCell(task, options.today);

  // A task that has never been named opens with an empty field and the name it
  // would get as a placeholder, rather than with those words already typed in.
  // Prefilled text has to be deleted before it can be replaced, and a field
  // that starts by asking to be cleared reads as a form rather than as a line
  // to write on.
  const editingTitle = options.editing === "title" || options.editing === "both";
  const editingBody = options.editing === "body" || options.editing === "both";

  const naming = editingTitle && options.provisional;
  const title = editingTitle
    ? `<input id="tk-title-input" class="tk-title-input" value="${
        naming ? "" : escape(task.title)
      }" placeholder="${escape(PROVISIONAL_TITLE)}" />`
    : `<h1 class="${task.is_completed ? "completed" : ""}">${escape(task.title || "(untitled)")}</h1>`;
  const box = `<button type="button" class="tk-box" data-task-action="${
    task.is_completed ? "uncomplete" : "complete"
  }" title="${task.is_completed ? "Un-complete" : "Complete"}"><span class="chk${task.is_completed ? " on" : ""}"></span></button>`;

  const body = editingBody
    ? `<textarea id="tk-body-input" class="tk-body-input" placeholder="Notes">${escape(
        detail.body ?? "",
      )}</textarea>`
    : detail.body
      ? `<pre class="tk-body">${linkify(escape(detail.body))}</pre>`
      : "";

  // What is set, as chips.
  const props: string[] = [];
  if (task.is_working_on) {
    props.push(chip("Working", "task", "Working on", { cls: "acc", action: "work-on", title: "Click to stop" }));
  }
  if (task.is_dirty) {
    props.push(
      chip("Saved", "cloud", "Not yet on the server", {
        cls: "warn",
        title: "Saved on this machine; it goes up to the server with the next sync",
      }),
    );
  }
  if (task.deleted_at) props.push(chip("State", "trash", "In the trash", { cls: "warn" }));
  else if (task.is_completed) {
    props.push(chip("State", "done", `Completed ${escape(formatInstant(task.completed_at ?? ""))}`));
  }
  if (task.postpone_date) {
    const today = task.postpone_date <= options.today;
    props.push(
      chip("Postpone", "sun", escape(formatCivilDate(task.postpone_date, options.today)), {
        word: today ? "" : "From",
        cls: today ? "warn" : "",
        action: "set-postpone",
        clear: "clear-postpone",
        title: task.postpone_date,
      }),
    );
  }
  if (task.due_date) {
    props.push(
      chip("Due", "cal", escape(formatCivilDate(task.due_date, options.today)), {
        word: date.kind === "overdue" ? "Overdue since" : "Due",
        cls: deadlineReached(date.kind) ? "late" : "",
        action: "set-due",
        clear: "clear-due",
        title: task.due_date,
      }),
    );
  }
  props.push(...detail.reminders.map(reminderChip));
  if (detail.recurrence) {
    props.push(
      chip("Repeat", "repeat", escape(detail.recurrence_label ?? detail.recurrence), {
        action: "set-recurrence",
        clear: "clear-recurrence",
        title: `${detail.recurrence} · ${
          detail.recurrence_from_completion ? "from completion" : "from the scheduled date"
        }`,
      }),
    );
  }
  for (const tag of task.tags) {
    props.push(
      `<span class="p tag" data-k="Tag"${colourStyle("--tk-tag", tag.colour)}>${svgIcon(
        "tag",
      )}<span class="pv">${renderTag(tag)}</span>${fieldButton(
        "remove-tag",
        "fa-xmark",
        `Remove ${tag.name}`,
        ` data-tag="${escape(tag.id)}"`,
      )}</span>`,
    );
  }

  // What can still be added, by its key: one line instead of a row per field
  // saying "not set".
  const hint = (action: string, command: string, label: string) => {
    const key = options.keys?.[command];
    return `<button type="button" class="hint" data-task-action="${action}">${key ? `<kbd>${escape(key)}</kbd>` : ""}<span class="lbl">${label}</span></button>`;
  };
  const hints = [
    !task.postpone_date ? hint("set-postpone", "postpone-task", "Postpone") : "",
    !task.due_date ? hint("set-due", "set-due-date", "Due") : "",
    hint("add-reminder", "add-reminder", "Reminder"),
    !detail.recurrence ? hint("set-recurrence", "set-recurrence", "Repeat") : "",
    hint("add-tag", "add-tag", "Tag"),
    hint("add-step", "add-step", "Step"),
    hint("attach-file", "attach-file", "File"),
  ].join("");

  return `<div class="td">
    <header class="tk-head">
      <div class="td-title">${box}${title}</div>
      ${props.length ? `<div class="props">${props.join("")}</div>` : ""}
      <div class="td-hints"><span class="lbl">Add:</span>${hints}</div>
      ${renderBlockedNotice(options.notifications)}
    </header>
    <div class="tk-detail-body">
      ${body ? `<div class="td-notes">${body}</div>` : ""}
      ${renderSteps(detail.steps ?? [], options.selectedSteps ?? new Set(), options.stepEditing ?? null)}
      ${renderLinks(detail.links)}
      ${renderFiles(detail.attachments)}
      ${renderLog(detail.log)}
    </div>
  </div>`;
}

/**
 * The tasks linked to the message being read (§6.3).
 *
 * Sits under the metadata block and is actionable where it is: completing a
 * task you were reminded of by the mail in front of you should not require
 * leaving the mail. Postponing opens the same picker the task list uses, and
 * "open" goes to the task rather than doing anything to it.
 */
export function renderMessageTasks(tasks: TaskSummary[], today: string): string {
  return tasks
    .map((task) => {
      const date = taskDateCell(task, today);
      const when = date.text ? ` <span class="dim">${date.kind === "postponed" ? "from" : "due"} ${escape(date.text)}</span>` : "";
      return `<span class="chip acc tk-msg-task${task.is_completed ? " completed" : ""}${deadlineReached(date.kind) ? " late" : ""}">
        <button type="button" class="tk-box" data-message-task="${task.id}"
          data-task-action="${task.is_completed ? "uncomplete" : "complete"}"
          title="${task.is_completed ? "Un-complete" : "Complete"}"><span class="chk${task.is_completed ? " on" : ""}"></span></button>
        <button type="button" class="tk-link tk-title" data-message-task="${task.id}"
          data-task-action="open">${escape(task.title || "(untitled)")}${when}</button>
        <button type="button" class="tk-field-button" data-message-task="${task.id}"
          data-task-action="postpone" title="Postpone…">${icon("fa-clock")}</button>
      </span>`;
    })
    .join("");
}
