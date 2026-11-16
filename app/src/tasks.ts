/**
 * Background work, and showing it.
 *
 * Two channels, deliberately separate: the status line echoes what you just
 * did and what came of it, and this owns what is still happening. Anything
 * that outlives the keystroke that started it belongs here — otherwise a long
 * operation either scrolls away in the echo line or, worse, keeps overwriting
 * it while you try to do something else.
 *
 * Work is registered through `runTask`, which owns the lifecycle: the task is
 * always finished, with the failure recorded if the body throws. Registering
 * and completing by hand is how a task ends up stuck at "starting…" forever.
 *
 * Two rules let every async operation be wrapped without the widget becoming a
 * flickering mess, so callers never have to guess whether something is
 * "background enough":
 *
 * - A task is not shown until it has been running for a moment, so work that
 *   turns out to be fast is never seen at all.
 * - A task that was never shown and then succeeds disappears silently.
 *
 * Duration decides visibility, not the caller. A search over a warm cache
 * stays invisible; the same search over a cold 200k-message archive shows up
 * on its own.
 *
 * Cancellation is cooperative and honest. A task offers a stop button only if
 * it declared itself cancellable, which it may only do when stopping leaves
 * the data consistent — work that commits in bounded batches can, work that
 * would leave a half-applied change cannot, and says so by not offering it.
 */

export type TaskState = "running" | "done" | "failed" | "cancelled";

export interface Task {
  id: string;
  label: string;
  detail: string;
  /** 0..1, or null when the total is not known. */
  fraction: number | null;
  state: TaskState;
  /** Present only while running, and only for cancellable work. */
  cancel?: () => void;
}

/**
 * Controls handed to a task's body.
 *
 * A task reports through this rather than reaching into the registry, so it
 * cannot update a different task or outlive its own registration.
 */
export interface TaskHandle {
  /** Rename the task, for work that passes through distinct phases. */
  label(text: string): void;
  /** Set the free-form detail line. */
  detail(text: string): void;
  /** Set determinate progress. A null total means indeterminate. */
  progress(done: number, total: number | null): void;
  /**
   * True once the user has asked to stop.
   *
   * Cooperative: the body decides where stopping is safe and returns there.
   * Nothing is interrupted from outside.
   */
  readonly cancelled: boolean;
}

export interface TaskSpec {
  /** Stable id. Starting a task whose id is already running is refused. */
  id: string;
  label: string;
  detail?: string;
  /**
   * Whether a stop button is offered.
   *
   * Only for work that can stop without leaving inconsistent state.
   */
  cancellable?: boolean;
  /**
   * Why quitting now would be bad, if it would be. Shown in the confirmation.
   *
   * This is a different question from `cancellable`, and the two do not line
   * up. Cancellability asks whether stopping mid-flight leaves the data
   * consistent; this asks whether the work is lost or left dangerous if the
   * process dies. A sync is not cancellable and yet perfectly safe to quit
   * during — it commits per batch and resumes — while a backfill is
   * cancellable and equally safe. Warning on either would be noise, and a
   * warning you learn to dismiss is worse than none when the send does matter.
   */
  warnOnQuit?: string;
}

type Listener = () => void;

interface Entry {
  task: Task;
  /** Whether the task has been on screen. Governs whether it lingers. */
  revealed: boolean;
  reveal?: ReturnType<typeof setTimeout>;
  /** `TaskSpec.warnOnQuit`, kept out of `Task` — it is not for display. */
  warnOnQuit?: string;
}

const entries = new Map<string, Entry>();
const cancelling = new Set<string>();
const listeners = new Set<Listener>();

/**
 * Whether finished tasks are being kept on screen.
 *
 * Set while the user has the list open. A finished task otherwise clears on a
 * timer, which means the thing being read can vanish mid-glance — and the chip
 * with it, so a click that lands just as it goes appears to do nothing.
 */
let held = false;

/** How long work must run before it is worth showing. */
export const REVEAL_MS = 400;
/** How long a finished task stays visible before clearing itself. */
const LINGER_MS = 6000;
/** Failures linger longer: they are the ones worth reading. */
const LINGER_FAILED_MS = 20000;

function notify(): void {
  for (const listener of listeners) listener();
}

export function onTasksChanged(listener: Listener): void {
  listeners.add(listener);
}

/** Every task worth showing. Work too short to have been revealed is absent. */
export function allTasks(): Task[] {
  return [...entries.values()].filter((e) => e.revealed).map((e) => e.task);
}

export function runningTasks(): Task[] {
  return allTasks().filter((t) => t.state === "running");
}

/** Whether this id is live, revealed or not. Guards against double-starting. */
export function isRunning(id: string): boolean {
  return entries.get(id)?.task.state === "running";
}

/**
 * Reasons not to quit right now, one per running task that gave one.
 *
 * Deliberately not derived from cancellability — see `TaskSpec.warnOnQuit`.
 * Includes work that has not been revealed: a send that started 200ms ago is
 * invisible but is exactly the thing worth stopping the door on.
 */
export function quitBlockers(): { label: string; reason: string }[] {
  return [...entries.values()]
    .filter((e) => e.task.state === "running" && e.warnOnQuit)
    .map((e) => ({ label: e.task.label, reason: e.warnOnQuit! }));
}

export function cancelTask(id: string): void {
  entries.get(id)?.task.cancel?.();
}

function reveal(id: string): void {
  const entry = entries.get(id);
  if (!entry || entry.revealed) return;
  entry.revealed = true;
  notify();
}

function finish(id: string, state: TaskState, detail?: string): void {
  const entry = entries.get(id);
  if (!entry) return;
  clearTimeout(entry.reveal);
  cancelling.delete(id);

  entry.task = {
    ...entry.task,
    state,
    detail: detail ?? entry.task.detail,
    cancel: undefined,
  };

  // Work that finished before anyone saw it stays unseen — unless it failed,
  // which is precisely the case worth interrupting for.
  if (!entry.revealed && state !== "failed") {
    entries.delete(id);
    notify();
    return;
  }

  entry.revealed = true;
  notify();

  const linger = state === "failed" ? LINGER_FAILED_MS : LINGER_MS;
  const clear = () => {
    // Nothing restarted under the same id in the meantime, and nobody is
    // reading the list.
    if (entries.get(id)?.task.state !== state) return;
    if (held) {
      // Try again once the list is closed rather than dropping the entry.
      setTimeout(clear, linger);
      return;
    }
    entries.delete(id);
    notify();
  };
  setTimeout(clear, linger);
}

/**
 * Keep finished tasks on screen, or stop keeping them.
 *
 * Called with the list's open state. Without it a task can clear while it is
 * being looked at, which reads as the display losing track of its own work.
 */
export function holdFinishedTasks(hold: boolean): void {
  held = hold;
}

/**
 * Run `body` as a tracked background task.
 *
 * Returns whatever the body returns. A throwing body marks the task failed and
 * rethrows, so callers still handle their own errors — the task display is an
 * addition to normal error handling, not a replacement for it.
 *
 * Starting a task whose id is already running throws rather than replacing it:
 * two concurrent runs of the same work would report over each other, and for
 * something like a backfill they would also compete for the same connections.
 */
export async function runTask<T>(
  spec: TaskSpec,
  body: (task: TaskHandle) => Promise<T>,
): Promise<T> {
  if (isRunning(spec.id)) {
    throw new Error(`${spec.label} is already running`);
  }

  cancelling.delete(spec.id);
  const entry: Entry = {
    revealed: false,
    warnOnQuit: spec.warnOnQuit,
    task: {
      id: spec.id,
      label: spec.label,
      detail: spec.detail ?? "",
      fraction: null,
      state: "running",
      cancel: spec.cancellable
        ? () => {
            cancelling.add(spec.id);
            const live = entries.get(spec.id);
            if (live && live.task.state === "running") {
              live.task = { ...live.task, detail: "stopping…" };
              notify();
            }
          }
        : undefined,
    },
  };
  entry.reveal = setTimeout(() => reveal(spec.id), REVEAL_MS);
  entries.set(spec.id, entry);

  const patch = (fields: Partial<Task>) => {
    const live = entries.get(spec.id);
    if (!live || live.task.state !== "running") return;
    live.task = { ...live.task, ...fields };
    if (live.revealed) notify();
  };

  const handle: TaskHandle = {
    label: (text) => patch({ label: text }),
    detail: (text) => patch({ detail: text }),
    progress: (done, total) =>
      patch({
        fraction:
          total === null || total <= 0 ? null : Math.max(0, Math.min(1, done / total)),
      }),
    get cancelled() {
      return cancelling.has(spec.id);
    },
  };

  try {
    const result = await body(handle);
    // "stopping…" is a state, not an outcome; a stopped task should not be
    // left reading as though it were still winding down.
    if (handle.cancelled) finish(spec.id, "cancelled", "stopped");
    else finish(spec.id, "done");
    return result;
  } catch (error) {
    finish(spec.id, "failed", describeError(error));
    throw error;
  }
}

/**
 * A readable one-line reason from whatever was thrown.
 *
 * Errors from the core arrive as the serialised error enum — `{"ImapError":
 * {"message": "..."}}` — which has no `message` of its own, so reading one off
 * the top level yields undefined and `String()` then renders the whole thing as
 * "[object Object]". The message is one level in; this goes and gets it.
 */
export function describeError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  if (error && typeof error === "object") {
    const record = error as Record<string, unknown>;
    // Either `{ message: "..." }` or `{ Variant: { message: "..." } }`.
    if (typeof record.message === "string") return record.message;
    for (const value of Object.values(record)) {
      if (value && typeof value === "object") {
        const nested = (value as Record<string, unknown>).message;
        if (typeof nested === "string") return nested;
      }
    }
    try {
      return JSON.stringify(error);
    } catch {
      // Fall through to the generic rendering below.
    }
  }
  return String(error);
}
