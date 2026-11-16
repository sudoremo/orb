/**
 * Merge Tasks: several tasks become the one chosen to stay. The dialog
 * proposes what the core's plan says — the kept task's title, every task's
 * notes and the earliest dates — and each proposal can be changed before
 * Merge; steps, tags, reminders, linked messages and files all come along,
 * and the others go to the Trash. Built on the picker's frame.
 */

import { call } from "./bridge";
import { formatDate } from "./format";
import { TaskDetail, TaskSummary } from "./tasklist";

interface TaskMergePlan {
  tasks: TaskSummary[];
  body: string | null;
  postpone_date: string | null;
  due_date: string | null;
  steps: number;
  tags: number;
  reminders: number;
  links: number;
  attachments: number;
}

let open = false;

/** Whether the dialog owns the keyboard. */
export function isTaskMergeOpen(): boolean {
  return open;
}

/** Ask how to merge `ids`, the first proposed as the one to keep; resolves to the merged task, or null if cancelled. */
export async function mergeTasksDialog(ids: number[]): Promise<TaskDetail | null> {
  if (open || ids.length < 2) return null;
  const plan = await call<TaskMergePlan>("plan_task_merge", { keep: ids[0], others: ids.slice(1) });
  open = true;
  return new Promise((resolve) => {
    new MergeDialog(ids, plan, (merged) => {
      open = false;
      resolve(merged);
    });
  });
}

/** `Thu 10 Dec`, with the year when it is not this one. */
function dateLabel(date: string): string {
  const shown = formatDate(`${date}T12:00:00`, "%a %-d %b");
  return date.slice(0, 4) === String(new Date().getFullYear()) ? shown : `${shown} ${date.slice(0, 4)}`;
}

function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}

class MergeDialog {
  private readonly scrim = document.createElement("div");
  private readonly keepList = document.createElement("div");
  private readonly title = document.createElement("input");
  private readonly notes = document.createElement("textarea");
  private readonly postpone = document.createElement("select");
  private readonly due = document.createElement("select");
  private readonly along = document.createElement("div");
  private readonly problem = document.createElement("div");
  private readonly onKeyDown: (e: KeyboardEvent) => void;
  private readonly restoreFocus = document.activeElement;
  private keep: number;
  /** Whether the title was typed in, which a change of the kept task then leaves alone. */
  private titleTouched = false;
  private closed = false;

  constructor(
    private readonly ids: number[],
    private plan: TaskMergePlan,
    private readonly done: (merged: TaskDetail | null) => void,
  ) {
    this.keep = ids[0]!;
    this.scrim.className = "dp-scrim";
    const panel = document.createElement("div");
    panel.className = "dp task-merge";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");
    panel.setAttribute("aria-label", "Merge Tasks");

    const head = document.createElement("div");
    head.className = "dp-head";
    const heading = document.createElement("span");
    heading.className = "dp-title";
    heading.textContent = `Merge ${ids.length} tasks`;
    head.append(heading);

    const body = document.createElement("div");
    body.className = "tm-body";
    this.keepList.className = "tm-keep";
    this.keepList.setAttribute("role", "radiogroup");
    this.keepList.setAttribute("aria-label", "Keep");
    this.title.className = "tm-title";
    this.title.id = "tm-title";
    this.title.addEventListener("input", () => (this.titleTouched = true));
    this.notes.className = "tm-notes";
    this.notes.id = "tm-notes";
    this.notes.placeholder = "Notes";
    this.postpone.id = "tm-postpone";
    this.due.id = "tm-due";
    const dates = document.createElement("div");
    dates.className = "tm-dates";
    dates.append(this.labelled("Postpone", this.postpone), this.labelled("Due", this.due));
    this.along.className = "tm-along";
    this.problem.className = "tm-problem";
    body.append(
      this.labelled("Keep", this.keepList),
      this.labelled("Title", this.title),
      this.labelled("Notes", this.notes),
      dates,
      this.along,
      this.problem,
    );

    const foot = document.createElement("div");
    foot.className = "dp-foot tm-foot";
    const keys = document.createElement("span");
    keys.textContent = "⌘↩ merge · esc cancel";
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "tm-button";
    cancel.textContent = "Cancel";
    cancel.addEventListener("click", () => this.close(null));
    const merge = document.createElement("button");
    merge.type = "button";
    merge.className = "tm-button primary";
    merge.dataset.merge = "";
    merge.textContent = "Merge";
    merge.addEventListener("click", () => void this.merge());
    foot.append(keys, cancel, merge);

    panel.append(head, body, foot);
    this.scrim.append(panel);
    document.body.append(this.scrim);
    this.scrim.addEventListener("mousedown", (e) => {
      if (e.target === this.scrim) this.close(null);
    });

    this.onKeyDown = (e) => {
      // The dialog has the keyboard: nothing reaches the list behind it.
      e.stopPropagation();
      if (e.key === "Escape") {
        e.preventDefault();
        this.close(null);
      } else if (e.key === "Enter" && (e.metaKey || e.target === this.title)) {
        e.preventDefault();
        void this.merge();
      }
    };
    window.addEventListener("keydown", this.onKeyDown, true);

    this.fill();
    this.title.focus();
    this.title.select();
  }

  private labelled(text: string, control: HTMLElement): HTMLElement {
    const row = document.createElement("label");
    row.className = "tm-field";
    const name = document.createElement("span");
    name.className = "tm-label";
    name.textContent = text;
    row.append(name, control);
    return row;
  }

  /** Draw the plan: the kept task, its proposals, and what comes along. */
  private fill(): void {
    const tasks = this.plan.tasks;
    this.keepList.replaceChildren(
      ...tasks.map((task) => {
        const option = document.createElement("button");
        option.type = "button";
        option.className = `tm-option${task.id === this.keep ? " on" : ""}`;
        option.setAttribute("role", "radio");
        option.setAttribute("aria-checked", String(task.id === this.keep));
        option.dataset.keep = String(task.id);
        option.textContent = task.title;
        option.addEventListener("click", () => void this.choose(task.id));
        return option;
      }),
    );
    const kept = tasks.find((t) => t.id === this.keep) ?? tasks[0]!;
    if (!this.titleTouched) this.title.value = kept.title;
    this.notes.value = this.plan.body ?? "";
    this.options(this.postpone, tasks.map((t) => t.postpone_date), this.plan.postpone_date);
    this.options(this.due, tasks.map((t) => t.due_date), this.plan.due_date);
    const parts = [
      this.plan.steps && plural(this.plan.steps, "step"),
      this.plan.tags && plural(this.plan.tags, "tag"),
      this.plan.reminders && plural(this.plan.reminders, "reminder"),
      this.plan.links && plural(this.plan.links, "linked message"),
      this.plan.attachments && plural(this.plan.attachments, "file"),
    ].filter(Boolean);
    const others = tasks.length - 1;
    this.along.textContent =
      (parts.length ? `Comes along: ${parts.join(", ")}. ` : "")
      + `The other ${others === 1 ? "task goes" : `${others} tasks go`} to the Trash; ⌘Z takes the merge back.`;
  }

  /** A date's choices: every date among the tasks, and none. */
  private options(select: HTMLSelectElement, dates: (string | null)[], chosen: string | null): void {
    const distinct = [...new Set(dates.filter((d): d is string => !!d))].sort();
    select.replaceChildren(
      ...[null, ...distinct].map((date) => {
        const option = document.createElement("option");
        option.value = date ?? "";
        option.textContent = date ? dateLabel(date) : "None";
        option.selected = date === chosen;
        return option;
      }),
    );
  }

  /** Keep another of the tasks: its title, and the notes in its order. */
  private async choose(id: number): Promise<void> {
    if (id === this.keep) return;
    this.keep = id;
    this.plan = await call<TaskMergePlan>("plan_task_merge", {
      keep: id,
      others: this.ids.filter((other) => other !== id),
    });
    if (!this.closed) this.fill();
  }

  private async merge(): Promise<void> {
    const title = this.title.value.trim();
    if (!title) {
      this.problem.textContent = "The merged task needs a title.";
      this.title.focus();
      return;
    }
    try {
      const merged = await call<TaskDetail>("merge_tasks", {
        keep: this.keep,
        others: this.ids.filter((id) => id !== this.keep),
        title,
        body: this.notes.value.trim() || null,
        postpone: this.postpone.value || null,
        due: this.due.value || null,
      });
      this.close(merged);
    } catch (error) {
      this.problem.textContent = `Not merged: ${String(error)}`;
    }
  }

  private close(merged: TaskDetail | null): void {
    if (this.closed) return;
    this.closed = true;
    window.removeEventListener("keydown", this.onKeyDown, true);
    this.scrim.remove();
    if (this.restoreFocus instanceof HTMLElement) this.restoreFocus.focus();
    this.done(merged);
  }
}
