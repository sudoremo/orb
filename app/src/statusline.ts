import { chatActive, chatStatusMarkup, inChatView } from "./chat";
import { todayMixed } from "./layout";
import { cursorPosition, timelineKind, timelineTotal } from "./mixed";
import { readingActive } from "./list";
import { call } from "./bridge";
import { escape } from "./format";
import { inTaskSection, renderFolders } from "./sidebar";
import { state, TaskSyncStatus } from "./state";
import { openTaskById } from "./taskview";

/**
 * The status line's two halves: on the left the last thing done and how to
 * take it back, on the right the steady state — what is staged, where the
 * cursor is, how the accounts last synced.
 */

/**
 * A message as markup, with the keys it offers drawn as keys: `⌘Z` where
 * the action can be undone, `U` where it was staged.
 */
export function statusMarkup(text: string): string {
  return escape(text)
    .replace(/⌘Z/g, "<kbd>⌘Z</kbd>")
    .replace(/· U unstage$/, "· <kbd>U</kbd> unstage");
}

/** How the accounts stand with their servers, as the right end says it. */
function syncState(): { text: string; cls: string } | null {
  const health = state.health;
  if (health.length === 0) return null;
  if (health.some((h) => h.state === "syncing")) return { text: "syncing…", cls: "busy" };
  const failing = health.filter((h) => !["ok", "unknown", "syncing"].includes(h.state));
  if (failing.length > 0) {
    return { text: `${failing.map((h) => h.account).join(", ")}: not synced`, cls: "warn" };
  }
  const last = health
    .map((h) => h.last_ok ?? (h.state === "ok" ? h.at : null))
    .filter((at): at is string => !!at)
    .sort()
    .pop();
  if (!last) return null;
  const time = new Date(last);
  if (Number.isNaN(time.getTime())) return null;
  const hhmm = `${String(time.getHours()).padStart(2, "0")}:${String(time.getMinutes()).padStart(2, "0")}`;
  return { text: `synced ${hhmm}`, cls: "ok" };
}

let commandLineOpen = false;

/** The command line opened or closed: the mode says so. */
export function setCommandMode(open: boolean): void {
  commandLineOpen = open;
  renderMode();
}

/**
 * The mode the window is in, for a look that starts the status line with
 * it: the command line, a search, Today, another task view, or mail.
 */
function renderMode(): void {
  const segment = document.getElementById("status-mode");
  if (!segment) return;
  const mode = commandLineOpen
    ? "CMD"
    : state.search
      ? "SEARCH"
      : inChatView()
        ? "CHAT"
        : state.tasks.view === "today"
        ? "TODAY"
        : state.tasks.view
          ? "TASKS"
          : "MAIL";
  if (segment.textContent !== mode) segment.textContent = mode;
}

/**
 * A backup that has failed, or has not run for twice its interval, says so
 * until one succeeds (todo-spec §20.3).
 */
function exportWarning(): string {
  const status = state.tasks.export;
  if (!status?.configured || (!status.overdue && !status.problem)) return "";
  const what = status.problem ? "task export failed" : "task export overdue";
  const why = status.problem ?? (status.last_complete ? `last complete ${status.last_complete}` : "never exported");
  return `<span class="warn" title="${escape(why)} · :export-tasks">${what}</span>`;
}

/**
 * Task changes that cannot reach the server, said for as long as they cannot
 * (todo-spec §23): a task list that is quietly kept on one machine is the
 * failure this exists to prevent.
 */
function taskSyncWarning(): string {
  const status = state.tasks.sync;
  if (!status?.problem) return "";
  const what = status.pending > 0
    ? `${status.pending} task change${status.pending === 1 ? "" : "s"} not on the server`
    : "tasks not syncing";
  return `<span class="warn" title="${escape(status.problem)}">${what}</span>`;
}

/**
 * Mail that has not gone out, said for as long as it has not: a refused
 * message in red, as it waits for the user; one waiting for its server in
 * amber. Either opens the Outbox.
 */
function outboxWarning(): string {
  const failed = state.outbox.filter((q) => q.failed_at !== null).length;
  const waiting = state.outbox.length - failed;
  const parts = [
    failed ? `<span class="fail" data-outbox-open title="Refused by the mail server · click for the Outbox">${failed} not sent</span>` : "",
    waiting ? `<span class="warn" data-outbox-open title="Waiting for the mail server · click for the Outbox">${waiting} waiting to send</span>` : "",
  ];
  return parts.join("");
}

/** Ask again whether the task changes are on the server, and redraw. */
export async function refreshTaskSync(): Promise<void> {
  if (!state.tasks.available) return;
  state.tasks.sync = (await call<TaskSyncStatus | null>("task_sync_status").catch(() => null)) ?? null;
  renderSteady();
  renderFolders();
}

/** Ask again where the task export stands, and redraw. */
export async function refreshExportStatus(): Promise<void> {
  state.tasks.export = (await call<typeof state.tasks.export>("task_export_status").catch(() => null)) ?? null;
  renderSteady();
}

/** Redraw the right end of the status line, and the mode at its start. */
export function renderSteady(): void {
  renderMode();
  const steady = document.getElementById("status-steady");
  if (!steady) return;
  const staged = [...state.pendingCounts.values()].reduce((n, c) => n + c, 0);
  // On Today's timeline the place is on the timeline, and the kind of row says
  // whose keys are in force.
  const timeline = todayMixed() && state.tasks.section !== "tasks";
  const position = timeline
    ? timelineTotal() > 0 ? `${{ mail: "Mail", chat: "Chat", reading: "Reading" }[timelineKind()]} ${cursorPosition() + 1}/${timelineTotal()}` : ""
    : chatActive()
    ? state.chat.rows.length > 0 ? `${state.chat.cursor + 1}/${state.chat.rows.length}` : ""
    : readingActive()
    ? `${state.reading.cursor + 1}/${state.reading.rows.length}`
    : inTaskSection() || (state.tasks.view !== null && state.tasks.section === "tasks")
      ? state.tasks.rows.length > 0 ? `${state.tasks.cursor + 1}/${state.tasks.rows.length}` : ""
      : state.total > 0 ? `${state.messageCursor + 1}/${state.total}` : "";
  const sync = syncState();
  // What is being worked on, wherever the window is (§19).
  const working = state.tasks.workingOn;
  const html =
    (working.length
      ? `<button type="button" class="working" data-working title="Working on — click to open">▶ ${escape(
          working.length === 1 ? working[0]!.title : `${working.length} tasks`,
        )}</button>`
      : "") +
    outboxWarning() +
    exportWarning() +
    taskSyncWarning() +
    (staged > 0 ? `<span class="warn">${staged} staged</span>` : "") +
    (position ? `<span class="pos">${position}</span>` : "") +
    chatStatusMarkup() +
    (sync ? `<span class="sync ${sync.cls}"><span class="dot"></span>${escape(sync.text)}</span>` : "");
  if (steady.innerHTML !== html) steady.innerHTML = html;
}

// The task being worked on opens on a click; of several, the first.
document.getElementById("status-steady")?.addEventListener("click", (e) => {
  if ((e.target as HTMLElement).closest("[data-chat-sign-in]")) {
    void call("chat_sign_in_window").catch(() => {});
    return;
  }
  if ((e.target as HTMLElement).closest("[data-outbox-open]")) {
    void import("./outbox").then((outbox) => outbox.showOutbox());
    return;
  }
  if (!(e.target as HTMLElement).closest("[data-working]")) return;
  const first = state.tasks.workingOn[0];
  if (first) void openTaskById(first.id).catch(() => {});
});
