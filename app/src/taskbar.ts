import { runningTasks, allTasks, holdFinishedTasks, cancelTask, onTasksChanged } from "./tasks";
import { escape } from "./format";
import { el } from "./state";

/** A progress bar; indeterminate when the total is unknown. */
function taskBar(fraction: number | null): string {
  if (fraction === null) return `<span class="task-bar indeterminate"><span></span></span>`;
  const percent = Math.round(Math.min(1, Math.max(0, fraction)) * 100);
  return `<span class="task-bar"><span style="width:${percent}%"></span></span>`;
}
let tasksExpanded = false;
function renderTasks(): void {
  const running = runningTasks();
  const all = allTasks();

  // The chip only exists while there is something to report.
  el.tasksToggle.hidden = all.length === 0;
  if (all.length === 0) {
    tasksExpanded = false;
    holdFinishedTasks(false);
    el.tasksPanel.hidden = true;
    return;
  }

  // The chip speaks for one task: the newest running one, or — when nothing is
  // running — the last outcome, which is what a glance after a long job wants.
  const newest = running[running.length - 1];
  const last = all[all.length - 1];
  const summary = newest
    ? `${escape(newest.label)} ${taskBar(newest.fraction)}`
    : `${escape(last.label)} <span class="${last.state}">${last.state}</span>`;
  const count = running.length > 1 ? `<span>+${running.length - 1}</span>` : "";
  el.tasksToggle.innerHTML = `${summary}${count}`;

  el.tasksPanel.hidden = !tasksExpanded;
  if (!tasksExpanded) return;

  el.tasksPanel.innerHTML = all
    .map(
      (task) => `<div class="task-row ${task.state}">
        <span class="name">${escape(task.label)}</span>
        ${
          task.state === "running" && task.cancel
            ? `<button type="button" data-cancel="${escape(task.id)}">stop</button>`
            : `<span>${escape(task.state === "running" ? "" : task.state)}</span>`
        }
        <span class="detail">${escape(task.detail)}</span>
        ${task.state === "running" ? taskBar(task.fraction) : ""}
      </div>`,
    )
    .join("");
}
el.tasksToggle.addEventListener("click", () => {
  tasksExpanded = !tasksExpanded;
  // Finished tasks stop expiring while the list is open, so what is being
  // read cannot disappear part-way through reading it.
  holdFinishedTasks(tasksExpanded);
  renderTasks();
});
el.tasksPanel.addEventListener("click", (e) => {
  const button = (e.target as HTMLElement).closest<HTMLElement>("[data-cancel]");
  if (button) cancelTask(button.dataset.cancel!);
});
onTasksChanged(renderTasks);
