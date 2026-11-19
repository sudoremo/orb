/**
 * Focus: a while in which Orb keeps to itself what it would announce. The
 * core holds the session and decides what gets through; this is the command
 * that starts and ends one, its picker, and what the status line says.
 */
import { call, setStatus } from "./bridge";
import { untilText } from "./chat";
import { muteEnds } from "./chatmute";
import { escape } from "./format";
import { state, type FocusStatus } from "./state";

/** What the picker offers besides the named times, which depend on the hour. */
const DURATIONS: [string, string][] = [
  ["30m", "For 30 minutes"],
  ["1h", "For 1 hour"],
  ["2h", "For 2 hours"],
];

/**
 * When a focus asked for by `spec` ends, in ms: `null` for one the user
 * ends, `undefined` for a spec that says nothing. Everything a mute takes,
 * and `noon` and `workday`, the latter to `focus.workday_ends`.
 */
export function focusEnds(spec: string, workdayEnds: string | null, now = new Date()): number | null | undefined {
  const text = spec.trim().toLowerCase();
  if (text === "noon") return muteEnds("12:00", now);
  if (text === "workday") return workdayEnds ? muteEnds(workdayEnds, now) : undefined;
  return muteEnds(text, now);
}

/** How long the running focus lasts: `until 17:00`, `until you end it`. */
export function focusSpan(status: FocusStatus | null, now = new Date()): string {
  const session = status?.session;
  if (!session) return "";
  return session.ends_at == null ? "until you end it" : untilText(session.ends_at, now);
}

/** The running focus, for the right end of the status line. */
export function focusMarkup(): string {
  const status = state.focusMode;
  const session = status?.session;
  if (!status || !session) return "";
  const detail = status.modes.find((m) => m.name === session.mode)?.detail ?? "";
  const title = `Focus: ${session.mode}${detail ? ` — ${detail}` : ""} · click to change or end it`;
  return `<button type="button" class="focus-mode" data-focus-mode title="${escape(title)}">Focus ${escape(focusSpan(status))}</button>`;
}

let endTimer: number | undefined;
/** Ask again where focus stands, redraw, and look again when it ends. */
export async function refreshFocus(): Promise<void> {
  state.focusMode = (await call<FocusStatus | null>("focus_status").catch(() => null)) ?? null;
  (await import("./statusline")).renderSteady();
  window.clearTimeout(endTimer);
  const ends = state.focusMode?.session?.ends_at;
  if (ends == null) return;
  // A timer holds at most about 24.8 days; a longer focus is looked at again then.
  endTimer = window.setTimeout(() => void refreshFocus(), Math.min(Math.max(ends - Date.now(), 0) + 500, 2 ** 31 - 1));
}

/** Whether today's `clock` (`17:30`) is still ahead. */
function ahead(clock: string, now: Date): boolean {
  const [h, m] = clock.split(":").map(Number);
  return now.getHours() * 60 + now.getMinutes() < h! * 60 + m!;
}

/** Ask how long, for `mode`; the answer is a span, `off`, or `mode:<name>` for another mode. */
async function pickSpan(status: FocusStatus, mode: string): Promise<string | null> {
  const { openPicker } = await import("./picker");
  const now = new Date();
  const info = status.modes.find((m) => m.name === mode);
  const workday = status.workday_ends;
  return openPicker({
    title: `Focus · ${mode}${info ? ` — ${info.detail}` : ""}`,
    placeholder: "for 45m, 3h, until 16:30",
    items: [
      ...(status.session ? [{ value: "off", label: "End focus", detail: `${status.session.mode} ${focusSpan(status, now)}` }] : []),
      ...DURATIONS.map(([value, label]) => ({ value, label })),
      ...(ahead("12:00", now) ? [{ value: "noon", label: "Until noon" }] : []),
      ...(workday && ahead(workday, now) ? [{ value: "workday", label: "Until the end of the workday", detail: workday }] : []),
      { value: "forever", label: "Until I end it" },
      ...status.modes
        .filter((m) => m.name !== mode)
        .map((m) => ({ value: `mode:${m.name}`, label: `In ${m.name} mode…`, detail: m.detail })),
    ],
    typed: (text) =>
      focusEnds(text, workday) === undefined ? `“${text}”: say 90m, 4h or 17:00` : `${text.includes(":") ? "Until" : "For"} ${text}`,
  });
}

/**
 * `focus-mode [mode] [span]`: start a focus, or change the one that runs;
 * without a span, ask. `focus-mode off` ends it.
 */
export async function focusMode(args: string[]): Promise<void> {
  const status = await call<FocusStatus>("focus_status");
  if (args[0] === "off") return endFocus(status);
  const named = status.modes.some((m) => m.name === args[0]);
  // A focus that runs keeps its mode when only its end is changed.
  let mode = named ? args[0]! : (status.session?.mode ?? status.default_mode);
  let spec = (named ? args.slice(1) : args).join(" ");
  while (!spec) {
    const picked = await pickSpan(status, mode);
    if (!picked) return;
    if (picked === "off") return endFocus(status);
    if (picked.startsWith("mode:")) mode = picked.slice(5);
    else spec = picked;
  }
  const ends = focusEnds(spec, status.workday_ends);
  if (ends === undefined) {
    setStatus(
      spec.trim().toLowerCase() === "workday"
        ? "focus.workday_ends is not set in config.yml"
        : `cannot tell how long “${spec}” is: say 1h, 90m, 17:00, noon, workday or forever`,
    );
    return;
  }
  await call("focus_start", { mode, ends });
  await refreshFocus();
  setStatus(`Focus (${mode}) ${focusSpan(state.focusMode)}`);
}

async function endFocus(status: FocusStatus): Promise<void> {
  if (!status.session) {
    setStatus("no focus is running");
    return;
  }
  await call("focus_stop");
  await refreshFocus();
  setStatus("Focus ended");
}
