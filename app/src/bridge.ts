import { ask } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { describeError } from "./tasks";
import { el, state } from "./state";
import { statusMarkup } from "./statusline";

/**
 * Thrown by a command whose picker was dismissed, so a chain does not go on
 * to act on an answer that was never given. Says nothing of its own.
 */
export class Cancelled extends Error {}
/** A server write as one line: what it did, or why the list lags behind it. */
export function describeWrite(done: string, warnings: (string | null)[]): string {
  const lagging = warnings.filter((w): w is string => w !== null);
  return lagging.length === 0 ? done : `${done}; ${lagging.join("; ")}`;
}
/** Report a server write: what it did, or why the list lags behind it. */
export function reportWrite(done: string, warnings: (string | null)[]): void {
  setStatus(describeWrite(done, warnings));
}
/**
 * Call the core.
 *
 * A failure lands in the status line, also for a call made from a background
 * task: the task records it too, but a task that has already cleared should
 * not swallow the reason.
 */
export async function call<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    setStatus(`${cmd} failed: ${describeError(e)}`);
    throw e;
  }
}
export function setStatus(text: string): void {
  state.status = text;
  const html = statusMarkup(text);
  if (el.status.innerHTML !== html) el.status.innerHTML = html;
}

/**
 * Ask before something with no way back, as a sheet on this window with the
 * action named on its button. `message` is the question on its first line
 * and what is at stake below it.
 */
export function askFirst(message: string, action: string): Promise<boolean> {
  const [title, ...rest] = message.split("\n\n");
  return ask(rest.join("\n\n") || title, {
    title,
    kind: "warning",
    okLabel: action,
    cancelLabel: "Cancel",
  });
}
