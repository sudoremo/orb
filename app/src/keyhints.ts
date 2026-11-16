/**
 * What can follow a key that starts a longer binding: after `g`, a moment
 * later, a panel lists `g` top, `r` open a chat room, and the rest of the
 * table in force. Typing on without a pause never shows it.
 */
import { COMMAND_DOCS } from "./commanddocs";
import { escape } from "./format";
import { spellKey } from "./keys";
import { state } from "./state";

/** How long a prefix has to stand before the panel says what can follow. */
const HINT_DELAY_MS = 400;

const panel = document.createElement("div");
panel.className = "keyhints";
panel.setAttribute("role", "tooltip");
let timer: number | undefined;

/** A binding's keys one by one: `g`, `<c-d>`. */
export const keysOf = (sequence: string): string[] => sequence.match(/<[^>]+>|./g) ?? [];

/** What a command string does, in a few words. */
function describe(command: string): string {
  const steps = command.split("&&").map((s) => s.trim());
  const first = steps[0] ?? "";
  const [name, ...args] = first.split(/\s+/);
  const own = state.userCommands[name ?? ""];
  const does = COMMAND_DOCS[name ?? ""]?.does ?? own?.title ?? own?.run ?? first;
  const shown = args.length ? `${does} (${args.join(" ")})` : does;
  return steps.length > 1 ? `${shown}, then ${steps.length - 1} more` : shown;
}

/**
 * A prefix is pending (`sequence`), or none is (`""`). `describeAs` may name
 * what a command does better than its documentation can — a pin by its name —
 * or answer null to leave a binding out that has nothing to do.
 */
export function showKeyHints(
  sequence: string,
  table: Record<string, string>,
  describeAs: (command: string) => string | null | undefined = () => undefined,
): void {
  window.clearTimeout(timer);
  if (!sequence) {
    panel.remove();
    return;
  }
  timer = window.setTimeout(() => {
    const rows = Object.entries(table)
      .filter(([keys, command]) => keys.startsWith(sequence) && keys !== sequence && command !== "noop")
      .filter(([, command]) => describeAs(command) !== null)
      .map(([keys, command]) => ({
        rest: keysOf(keys.slice(sequence.length)).map(spellKey).join(" "),
        does: describeAs(command) ?? describe(command),
      }))
      .sort((a, b) => a.rest.localeCompare(b.rest));
    if (rows.length === 0) return;
    panel.innerHTML = `<div class="kh-head"><kbd>${escape(keysOf(sequence).map(spellKey).join(" "))}</kbd><span class="lbl">then</span></div>${rows
      .map((r) => `<div class="kh-row"><kbd>${escape(r.rest)}</kbd><span>${escape(r.does)}</span></div>`)
      .join("")}`;
    document.body.append(panel);
  }, HINT_DELAY_MS);
}
