import { currentFolder } from "./actions";
import { call } from "./bridge";
import { rowAt } from "./list";
import { MessageHeader } from "./model";
import { accountFolders } from "./moving";
import { spellKey } from "./keys";
import { inTaskView } from "./sidebar";
import { state } from "./state";
import { TaskSummary } from "./tasklist";

/**
 * The context menus, built for what they will act on: an entry that does
 * nothing there is greyed out, and a toggle says which way it goes.
 */
export interface MenuState {
  disabled: string[];
  labels: Record<string, string>;
  /** Why each entry in `disabled` is, for the command palette to say. */
  why: Record<string, string>;
}

/** An empty state, and `off` to grey an entry out with its reason. */
function menuState(): { menu: MenuState; off: (command: string, why: string) => void } {
  const menu: MenuState = { disabled: [], labels: {}, why: {} };
  const off = (command: string, why: string) => {
    menu.disabled.push(command);
    menu.why[command] ??= why;
  };
  return { menu, off };
}

/**
 * The keys a menu shows beside its entries, from the bindings in force: a
 * single key or chord as the entry's key equivalent, the way macOS draws one;
 * a sequence such as `gx`, which a menu cannot draw, after the label.
 */
interface MenuKeys {
  accelerators: Record<string, string>;
  hints: Record<string, string>;
}

const ACCELERATOR_NAMES: Record<string, string> = {
  "<enter>": "Enter",
  "<esc>": "Escape",
  "<space>": "Space",
  "<tab>": "Tab",
  "<up>": "Up",
  "<down>": "Down",
  "<left>": "Left",
  "<right>": "Right",
  "<s-up>": "Shift+Up",
  "<s-down>": "Shift+Down",
};

/** A canonical key as a menu accelerator, or null where a menu cannot draw it. */
export function accelerator(key: string): string | null {
  if (ACCELERATOR_NAMES[key]) return ACCELERATOR_NAMES[key]!;
  if (/^[a-z0-9]$/.test(key)) return key.toUpperCase();
  if (/^[A-Z]$/.test(key)) return `Shift+${key}`;
  const chord = key.match(/^<((?:[cds]-)+)([a-z0-9])>$/);
  if (!chord) return null;
  const names: Record<string, string> = { c: "Ctrl", d: "Cmd", s: "Shift" };
  const modifiers = chord[1]!.split("-").filter(Boolean).map((m) => names[m]!);
  return [...modifiers, chord[2]!.toUpperCase()].join("+");
}

/** For every command bound in `table`, the key to show: a chord before a sequence. */
export function menuKeys(table: Record<string, string>): MenuKeys {
  const accelerators: Record<string, string> = {};
  const hints: Record<string, string> = {};
  for (const [key, spec] of Object.entries(table)) {
    const command = spec.trim();
    if (!command || command.includes(" ") || command === "noop") continue;
    const drawn = accelerator(key);
    if (drawn) {
      accelerators[command] ??= drawn;
    } else {
      hints[command] ??= spellKey(key);
    }
  }
  for (const command of Object.keys(accelerators)) delete hints[command];
  return { accelerators, hints };
}

/** The messages a command will act on: the selection, else the cursor's (loaded rows only). */
export function targetRows(): MessageHeader[] {
  const indices = state.selected.size > 0 ? [...state.selected] : [state.messageCursor];
  return indices.map((i) => rowAt(i)).filter((m): m is MessageHeader => !!m);
}

/** How many actions are staged where `expunge` would apply them. */
function stagedInScope(): number {
  const count = (account: string, folder: string | null) =>
    state.pendingCounts.get(`${account}\u0000${folder}`) ?? 0;
  if (inTaskView()) {
    return (state.inboxFolders ?? []).reduce((n, f) => n + count(f.account_name, f.folder_name), 0);
  }
  const folder = currentFolder();
  return folder ? count(folder.account_name, folder.folder_name) : 0;
}

export function messageMenuState(rows: MessageHeader[] = targetRows()): MenuState {
  const { menu, off } = menuState();
  if (rows.length === 0) return menu;
  const all = (test: (m: MessageHeader) => boolean) => rows.every(test);
  const folders = (m: MessageHeader) => accountFolders(m.account_name);
  const already = (where: string) => `${rows.length === 1 ? "it is" : "they are"} ${where} already`;

  menu.labels["toggle-read"] = rows.some((m) => !m.is_read) ? "Mark as Read" : "Mark as Unread";
  menu.labels["toggle-flag"] = rows.some((m) => !m.is_flagged) ? "Flag" : "Unflag";
  if (all((m) => m.folder_name === folders(m).archive)) off("archive", already("in the archive"));
  if (all((m) => m.folder_name === folders(m).junk)) off("spam", already("in junk"));
  if (all((m) => m.folder_name === folders(m).trash)) menu.labels["delete-immediate"] = "Delete Permanently";
  if (all((m) => m.pending_action === "Delete")) off("mark-delete", "staged for deletion already");
  if (all((m) => !m.pending_action)) off("unmark", "nothing is staged on it");
  if (stagedInScope() === 0 && all((m) => !m.pending_action)) off("expunge", "nothing is staged here");
  // One message at a time: these act on the cursor alone.
  if (rows.length > 1) {
    off("open-message-detail", "one message at a time");
    off("reply", "one message at a time");
  }
  return menu;
}

export function taskMenuState(): MenuState {
  const tasks = state.tasks.selected.size > 0
    ? [...state.tasks.selected].map((i) => state.tasks.rows[i])
    : [state.tasks.rows[state.tasks.cursor]];
  const rows = tasks.filter((t): t is TaskSummary => !!t);
  const { menu, off } = menuState();
  if (rows.length === 0) return menu;
  const all = (test: (t: TaskSummary) => boolean) => rows.every(test);
  if (all((t) => t.is_completed || !!t.is_done_for_today)) off("complete-task", "completed already");
  if (all((t) => !t.is_completed)) off("uncomplete-task", "not completed");
  if (all((t) => !t.postpone_date && !t.due_date)) off("clear-dates", "no dates to clear");
  if (all((t) => !!t.deleted_at)) off("delete-task", "in the trash already");
  if (all((t) => !t.deleted_at)) off("restore-task", "not in the trash");
  if (rows.length > 1) {
    off("open-task-detail", "one task at a time");
    off("edit-task", "one task at a time");
  }
  if (rows.length < 2) off("merge-tasks", "select the tasks to merge");
  else if (rows.some((t) => !!t.deleted_at)) off("merge-tasks", "a task in the trash cannot be merged");
  if (all((t) => t.is_working_on)) menu.labels["work-on"] = "Stop Working On";
  if (all((t) => t.is_completed || !!t.deleted_at)) off("work-on", "completed or in the trash");
  return menu;
}

export function openMessageMenu(): void {
  const { disabled, labels } = messageMenuState();
  void call("open_message_menu", { disabled, labels, ...menuKeys(state.bindings.index_normal) });
}

export function openTaskMenu(): void {
  const table = { ...state.bindings.index_normal, ...state.bindings.task_normal };
  const { disabled, labels } = taskMenuState();
  void call("open_task_menu", { disabled, labels, ...menuKeys(table) });
}
