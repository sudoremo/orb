import { isRunning, runTask } from "./tasks";
import { currentFolder } from "./actions";
import { call, setStatus } from "./bridge";
import { BodyBackfill } from "./model";
import { refreshSearch } from "./moving";
import { state } from "./state";

/** Messages fetched per pass. Bounded so a pass is short and interruptible. */
const BACKFILL_BUDGET = 200;
/** Pause between passes, to leave the servers and the UI alone. */
const BACKFILL_PAUSE_MS = 1500;
/** How often to look again while yielding to a sync. */
const BACKFILL_YIELD_MS = 1000;
/**
 * Whether the user has stopped the backfill in this session.
 *
 * Auto-starting is a default, not a policy: once you have said stop, new mail
 * arriving must not quietly start it up again. Cleared by asking for it again,
 * and not persisted — a restart is a fair place to reconsider.
 */
let backfillStopped = false;

/** Lift or set the stop that `stop-fetch-bodies` leaves behind. */
export function setBackfillStopped(stopped: boolean): void {
  backfillStopped = stopped;
}
/**
 * Start the backfill unless it is already going, or was stopped on purpose.
 *
 * The automatic entry point. Failures are swallowed here because nobody asked
 * for this run: the task records the failure and `call` has already put the
 * reason in the status line.
 */
export function autoBackfill(): void {
  if (backfillStopped || isRunning("backfill")) return;
  void runBackfill().catch(() => {});
}
/**
 * Fill in missing bodies until there are none left, or until stopped.
 *
 * Driven as repeated bounded calls rather than one long one: each pass can be
 * abandoned between batches, and progress is whatever is already stored, so
 * stopping costs nothing. That is also what makes it safe to offer a stop
 * button.
 */
export async function runBackfill(): Promise<void> {
  if (isRunning("backfill")) {
    setStatus("backfill already running");
    return;
  }

  await runTask(
    {
      id: "backfill",
      label: "Fetching bodies",
      detail: "starting…",
      // Safe to stop: each pass commits what it fetched before returning, so
      // stopping between passes loses nothing and resumes from the database.
      cancellable: true,
    },
    async (task) => {
      // A stop can arrive from the command or from the panel's button, so the
      // "stay stopped" rule is recorded here rather than at either control.
      const stopping = () => {
        if (!task.cancelled) return false;
        backfillStopped = true;
        return true;
      };

      for (;;) {
        if (stopping()) return;

        // Background work yields to what the user is waiting for. A sync and a
        // backfill both want the same connections, and the sync is the one
        // holding up the message list.
        while (isRunning("sync")) {
          task.detail("paused while syncing");
          await new Promise((r) => setTimeout(r, BACKFILL_YIELD_MS));
          if (stopping()) return;
        }

        // Every folder of every account, current folder first so what you are
        // looking at becomes searchable before a long archive does.
        const current = currentFolder();
        const byAccount = new Map<string, string[]>();
        for (const f of state.folders) {
          if (f.is_account_header || !f.folder_name) continue;
          const list = byAccount.get(f.account_name) ?? [];
          if (!list.includes(f.folder_name)) list.push(f.folder_name);
          byAccount.set(f.account_name, list);
        }
        if (current?.folder_name) {
          const list = byAccount.get(current.account_name);
          if (list) {
            byAccount.set(current.account_name, [
              current.folder_name,
              ...list.filter((n) => n !== current.folder_name),
            ]);
          }
        }

        let progressed = 0;
        let headers = 0;
        let latest: BodyBackfill | null = null;
        let workedOn = "";
        for (const [account, folders] of byAccount) {
          if (stopping()) return;
          const result = await call<BodyBackfill>(
            "fetch_bodies",
            { account, folders, budget: BACKFILL_BUDGET },
          );
          // Every phase counts. Leaving one out makes a pass that did nothing
          // but that phase look like a stall, and the loop stops for good —
          // which is exactly what adding the subject repair did.
          const moved =
            result.shaped +
            result.fetched +
            result.html +
            result.embedded +
            result.sources +
            result.threaded;
          progressed += moved;
          headers += result.threaded;
          if (moved > 0) workedOn = account;
          latest = result;
        }
        if (!latest) {
          task.detail("nothing to do");
          return;
        }
        // Headers a search asks about (`header:`) may have just arrived.
        if (headers > 0 && state.search) void refreshSearch().catch(() => {});

        // Each phase counts against its own population: shaping covers every
        // message, bodies only those found to have a text part. Sharing one
        // denominator between them is what produced negative progress.
        const shaping = latest.unshaped > 0;
        const of = shaping ? latest.messages : latest.total;
        const remaining = shaping ? latest.unshaped : latest.pending;
        // Clamp, so a denominator that moves under us can never read negative.
        const done = Math.max(0, Math.min(of, of - remaining));
        // Name the step that is running, so a long first phase does not look
        // like the wrong thing happening.
        task.label(
          shaping ? "Fetching bodies · step 1 of 2, reading structure" : "Fetching bodies",
        );
        task.detail(
          `${done.toLocaleString()} / ${of.toLocaleString()}${workedOn ? ` · ${workedOn}` : ""}`,
        );
        task.progress(done, of);

        // No progress and nothing pending means done. No progress with work
        // pending means every remaining message failed, and retrying at once
        // would spin.
        const finished =
          latest.pending === 0 &&
          latest.unshaped === 0 &&
          latest.sources_pending === 0 &&
          latest.threads_pending === 0;
        if (finished || progressed === 0) {
          task.detail(
            finished
              ? `${latest.total.toLocaleString()} messages with bodies`
              : `stalled with ${(
                  latest.unshaped ||
                  latest.pending ||
                  latest.sources_pending ||
                  latest.threads_pending
                ).toLocaleString()} left`,
          );
          return;
        }
        await new Promise((r) => setTimeout(r, BACKFILL_PAUSE_MS));
      }
    },
  );
}
