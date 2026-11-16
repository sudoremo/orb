import { ChatStatus, attachToMessage, chatActive, inChatView, openChatThread, refreshChat } from "./chat";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen } from "@tauri-apps/api/event";
import { localToday } from "./dates";
import { setStatus } from "./bridge";
import { dispatchCommand } from "./commands";
import { focusClicked } from "./panes";
import { taskSectionActive } from "./sidebar";
import { LABEL, WINDOW, el, state } from "./state";
import { refreshExportStatus, refreshTaskSync, renderSteady } from "./statusline";
import { render } from "./list";
import { attachFilesToTask } from "./taskedit";
import { openTaskById, refreshTasks, rolloverDay } from "./taskview";
import { openCheckedLink } from "./safety";
import { focusFrameCard } from "./conversation";
import { typingInAField } from "./search";

// HTML mail runs in a frame whose sandbox allows no script, and WebKit then
// runs no listener in it at all, the window's included: a key pressed while
// the frame has the keyboard reaches nothing. The frame therefore never keeps
// it. A click there gives the keyboard straight back to the pane, and what is
// selected in the frame stays selected for ⌘C, which copies it from here.
window.addEventListener(
  "keydown",
  (e) => {
    if (!e.metaKey || e.altKey || e.ctrlKey || e.shiftKey || e.key !== "c") return;
    if (typingInAField(e.target) || window.getSelection()?.toString()) return;
    for (const frame of el.reader.querySelectorAll("iframe")) {
      const doc = frame.contentDocument;
      const text = doc?.getSelection()?.toString();
      if (!doc || !text) continue;
      e.preventDefault();
      // The frame's own copy keeps the formatting of what was selected.
      if (!doc.execCommand("copy")) copyText(text);
      return;
    }
  },
  true,
);
/** Copy plain text from the window, leaving the keyboard where it was. */
function copyText(text: string): void {
  const had = document.activeElement;
  const field = document.createElement("textarea");
  field.value = text;
  field.style.cssText = "position:fixed;opacity:0;pointer-events:none";
  document.body.append(field);
  field.select();
  document.execCommand("copy");
  field.remove();
  if (had instanceof HTMLElement) had.focus({ preventScroll: true });
}
// A click into a frame is seen here, as the window losing the keyboard to it.
window.addEventListener("blur", () => {
  setTimeout(() => {
    const frame = document.activeElement;
    if (!(frame instanceof HTMLIFrameElement)) return;
    focusClicked(frame);
    focusFrameCard(frame);
    // A selection elsewhere would be the one ⌘C copies.
    window.getSelection()?.removeAllRanges();
    for (const other of el.reader.querySelectorAll("iframe")) {
      if (other !== frame) other.contentDocument?.getSelection()?.removeAllRanges();
    }
    frame.blur();
    el.list.focus({ preventScroll: true });
  }, 0);
});
el.list.tabIndex = -1;
el.folders.tabIndex = -1;
el.reader.tabIndex = -1;
// Files dropped on the pane while a task is open become its attachments (§9).
// The drop comes from the window, not the DOM: Tauri intercepts file drags and
// reports them with their paths, which the browser's own drop event never has.
void getCurrentWebview().onDragDropEvent((event) => {
  if (event.payload.type !== "drop") return;
  // In chat, anywhere in the window: the files join the message being written.
  if (chatActive()) {
    void attachToMessage(event.payload.paths);
    return;
  }
  const task = state.tasks.detail?.task;
  if (!task || !taskSectionActive()) return;
  const { x, y } = event.payload.position;
  const pane = el.reader.getBoundingClientRect();
  const scale = window.devicePixelRatio || 1;
  const inside =
    x / scale >= pane.left && x / scale <= pane.right && y / scale >= pane.top && y / scale <= pane.bottom;
  if (!inside || event.payload.paths.length === 0) return;
  void attachFilesToTask(task.id, event.payload.paths).catch(() => {
    // `call` has already reported it.
  });
});
// Menu selections arrive as command strings and go through the same registry
// as the keybindings, so a menu entry cannot diverge from its shortcut.
void listen<string>(
  "context-menu-command",
  (event) => {
    void dispatchCommand(event.payload, 1);
  },
  // Sent to one window, and run by that one alone.
  { target: LABEL },
);
// A link in a message that is not what it says, caught by the shell on its
// way out to the browser, to be asked about first.
void listen<string>(
  "link-check",
  (event) => {
    void openCheckedLink(event.payload).catch(() => {
      // `call` has already said why in the status line.
    });
  },
  { target: LABEL },
);
/**
 * Something changed a task — here, on another machine, or in a sync.
 *
 * Payload-free on purpose, and answered by re-reading rather than by patching:
 * one mutation can move a task between views, change the badge, advance a
 * recurrence and re-order the list, so a "what changed" payload would have to
 * be a second implementation of the views to be acted on.
 */
let taskRefreshQueued = false;
void listen("tasks-changed", () => {
  // Coalesced: a command mutates, refreshes on its own so the status line and
  // the list agree, and *then* the event for that same mutation arrives — and a
  // sync of forty tasks emits it forty times. One redraw per burst is all any
  // of them wanted.
  if (taskRefreshQueued) return;
  taskRefreshQueued = true;
  setTimeout(() => {
    taskRefreshQueued = false;
    void refreshTasks().catch(() => {
      // `call` has already reported it; the next change will catch up.
    });
    void refreshTaskSync();
  }, 60);
});
// The uploader took task changes to the server, or failed to (todo-spec §23).
void listen("task-sync", () => void refreshTaskSync());
/**
 * The chat tables changed (PLAN.md, Phase 15): re-read Waiting and the thread
 * on screen, once per burst, as for tasks.
 */
let chatRefreshQueued = false;
void listen("chat-changed", () => {
  if (chatRefreshQueued) return;
  chatRefreshQueued = true;
  setTimeout(() => {
    chatRefreshQueued = false;
    void refreshChat().catch(() => {});
  }, 80);
});
// Signed in or out, the stream up or down.
/** A chat notification was clicked: the thread it announced, in this window. */
void listen<[string, string]>("chat-notification-opened", (event) => {
  const [rid, thread] = event.payload;
  void openChatThread(rid, thread);
});
void listen<ChatStatus>("chat-status", (event) => {
  const before = state.chat.status?.state;
  state.chat.status = event.payload;
  renderSteady();
  if (before !== event.payload.state) void refreshChat().then(() => {
    if (inChatView()) render();
  }).catch(() => {});
});
/**
 * Local midnight, a wake from sleep, or a timezone change (§4.3).
 *
 * Pushed by the native watcher. The window watches the wall clock as well, and
 * both are wanted: the native side is the one that keeps working while this
 * window is idle, and the redraw is idempotent either way.
 */
void listen("task-day-changed", () => {
  void rolloverDay(localToday()).catch(() => {
    // `call` has already reported it.
  });
});
/** A reminder notification was clicked: land on the task it was about (§5). */
void listen<{ identifier: string; task_uuid: string; task_id: number | null }>(
  "task-notification-opened",
  (event) => {
    // Sent to every window; the mailbox lands on it, since the tasks window
    // is not always open.
    if (WINDOW !== "main") return;
    const id = event.payload.task_id;
    if (id === null) {
      // The uuid is the identity that travels; a row id that could not be
      // resolved means the task is not in any view on this machine yet.
      setStatus(`reminder for a task this machine cannot find (${event.payload.task_uuid})`);
      return;
    }
    void openTaskById(id).catch(() => {
      // `call` has already reported it.
    });
  },
);
// Reminders that came due while nobody was watching (§5.2). Said once, and
// deliberately not as a notification each — that is the whole point of the
// summary.
void listen<number>("task-reminders-missed", (event) => {
  setStatus(`${event.payload} reminder${event.payload === 1 ? "" : "s"} were missed`);
});

// An export the timer ran (§20.3): a failure is said, and the status line's
// warning follows either way.
void listen<{ result: { complete: boolean; missing: string[] } | null; error: string | null } | null>(
  "task-export",
  (event) => {
    const payload = event.payload;
    if (payload?.error) setStatus(`task export failed: ${payload.error}`);
    else if (payload?.result && !payload.result.complete) {
      setStatus(`task export incomplete: ${payload.result.missing.join("; ")}`);
    }
    void refreshExportStatus();
  },
);
