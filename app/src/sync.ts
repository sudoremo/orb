import { TaskHandle, runTask } from "./tasks";
import { listen } from "@tauri-apps/api/event";
import { currentFolder, loadMessages } from "./actions";
import { autoBackfill } from "./backfill";
import { call, setStatus } from "./bridge";
import { renderList } from "./list";
import { applyConfig } from "./main";
import { SyncProgress, SyncResult } from "./model";
import { refreshSearch } from "./moving";
import { refreshFolders, refreshHealth, refreshHighlightBadges, renderFolders } from "./sidebar";
import { WINDOW, state } from "./state";
import { refreshList } from "./targets";
import { loadInboxes, refreshTasks } from "./taskview";
import { showMessage } from "./jump";

/**
 * The task a sync is currently reporting into, if one is running.
 *
 * The core's progress channel is a single global event stream with no notion of
 * which call it belongs to, so the sync that started it parks its handle here
 * and the listener writes through it. Only one sync runs at a time, which is
 * what makes a single slot sufficient.
 */
let syncReporter: TaskHandle | null = null;
/**
 * Show sync progress as it happens.
 *
 * A cold folder can take minutes, and until the core's progress channel was
 * plumbed through the FFI the only feedback was a spinner for the duration.
 */
/** The folder each account is syncing, as the last progress event named it. */
const syncingFolder = new Map<string, string>();

/**
 * Show the open folder's new mail as soon as its own sync is done, rather
 * than when the whole sync is: on a launch that also loads a large archive,
 * that is minutes during which an inbox already synced looks empty. An
 * account that moves on to another folder, or completes, has finished the
 * one before.
 */
function noteFolderProgress(p: SyncProgress): void {
  const before = syncingFolder.get(p.account);
  if (p.stage === "syncing_folder" && p.folder) syncingFolder.set(p.account, p.folder);
  else if (p.stage === "complete") syncingFolder.delete(p.account);
  const finished = before !== undefined && before !== syncingFolder.get(p.account);
  if (!finished || state.search) return;
  const open = currentFolder();
  if (open?.account_name === p.account && open.folder_name === before) {
    void refreshList().catch(() => {});
  }
}

/** A mail event, which only the mailbox answers: the tasks window has no mail. */
const onMailEvent: typeof listen = WINDOW === "main" ? listen : async () => () => {};

void onMailEvent<SyncProgress>("sync-progress", (event) => {
  const p = event.payload;
  noteFolderProgress(p);
  if (!syncReporter) return;
  const where = p.folder ? `${p.account}/${p.folder}` : p.account;
  const counted =
    p.current !== null && p.total !== null
      ? ` ${p.current.toLocaleString()}/${p.total.toLocaleString()}`
      : "";
  syncReporter.detail(`${where} · ${p.stage.replace(/_/g, " ")}${counted}`);
  syncReporter.progress(p.fraction, 1);
});
/**
 * The server said a folder changed, and the core has already synced it — or
 * a message's window acted on its message.
 *
 * Only the display is left to do. The folder counts always move; the message
 * list only when the change was to the folder being looked at, and then over
 * the top of what is there — a refresh must not blank the list under someone
 * reading it.
 */
function followMailChange(): void {
  // A message's window has one message to read again, and no sidebar.
  if (WINDOW === "message") {
    void refreshSearch().catch(() => {});
    return;
  }
  void refreshFolders()
    .then(() => {
      // Refreshed whichever folder changed. Matching the pushed folder against
      // the open one looks tidier and is wrong often enough to matter: a
      // pinned entry and a virtual folder do not name themselves the way the
      // server does, so the message that vanished elsewhere stayed on screen.
      // Re-reading the visible pages costs one query and cannot flicker,
      // because the rows are replaced only once the new ones have arrived.
      if (state.search) return refreshSearch();
      return refreshList();
    })
    .catch(() => {
      // `call` has already reported it; the next sync will catch up.
    });
}
void (WINDOW === "message" ? listen : onMailEvent)<[string, string]>("folder-changed", followMailChange);
// A message's window acted on its message (see `itemwindow.ts`).
void onMailEvent("mail-changed", followMailChange);
// A sync began or ended somewhere, so an account's standing may have moved.
let healthTimer: ReturnType<typeof setTimeout> | undefined;
void onMailEvent("account-status", () => {
  clearTimeout(healthTimer);
  healthTimer = setTimeout(() => {
    void refreshHealth().catch(() => {
      // `call` has already reported it.
    });
  }, 80);
});
// Mail was queued, sent, refused, put back in line or deleted.
void onMailEvent("outbox-changed", () => {
  void import("./outbox").then((outbox) => outbox.refreshOutbox());
});
// A notification about a refused message was clicked.
void onMailEvent("outbox-notification-opened", () => {
  void import("./outbox").then(async (outbox) => {
    await outbox.refreshOutbox();
    outbox.showOutbox();
  });
});
// A new-mail notification was clicked: show the message it announced.
void onMailEvent<number>("mail-notification-opened", (event) => {
  void showMessage(event.payload, null, null).catch(() => {
    // `call` has already reported it; the message may have been moved since.
  });
});
// A push that could not be established or has stopped. Worth saying once:
// mail still arrives, just not the moment it is sent.
void onMailEvent<string>("push-error", (event) => {
  setStatus(`push: ${event.payload}`);
});
// The config file was re-read. Everything this window took from it at boot is
// taken again; what the core could not follow is named for a restart.
void listen<{ error: string | null; restart_needed: string[] }>(
  "config-reloaded",
  async (event) => {
    const { error, restart_needed } = event.payload;
    if (error) {
      setStatus(`config.yml not reloaded: ${error} · the previous configuration stays in force`);
      return;
    }
    await applyConfig().catch(() => {
      // `call` has already put the reason in the status line.
    });
    // The rows carry the names of the rules they matched when they were read,
    // and the rules may just have changed: forget every page, so the ones
    // scrolled back to are read again too, and re-read the visible ones now.
    state.generation += 1;
    state.pages.clear();
    state.fetching.clear();
    await refreshList().catch(() => {});
    // Today's order follows `tasks.completed_first_in_today`.
    if (state.tasks.view === "today") await refreshTasks().catch(() => {});
    await refreshHighlightBadges();
    renderFolders();
    const problems = state.highlightWarnings.length
      ? ` · ${state.highlightWarnings.join(" · ")}`
      : "";
    setStatus(
      (restart_needed.length
        ? `config.yml reloaded · restart Orb for: ${restart_needed.join(", ")}`
        : "config.yml reloaded") + problems,
    );
  },
);
/** "1 message" or "12 messages", for a status line that has to say which. */
export function describeCount(n: number): string {
  return `${n} message${n === 1 ? "" : "s"}`;
}
/** Format the counters of a completed sync for the status line. */
function syncSummary(label: string, r: SyncResult): string {
  const errors = r.errors.length
    ? ` · ${r.errors.length} error${r.errors.length === 1 ? "" : "s"} (see account-status)`
    : "";
  // A folder another sync was holding was not synced, and saying so is the
  // whole difference between "nothing new" and "did not look".
  const skipped = r.folders_skipped ? ` · ${r.folders_skipped} skipped (busy)` : "";
  const scope = r.folders_synced > 1 ? ` (${r.folders_synced} folders)` : "";
  const filtered = r.messages_filtered ? ` · ${r.messages_filtered} filtered` : "";
  const problems = r.filter_warnings?.length
    ? ` · ${r.filter_warnings.length} filter problem${r.filter_warnings.length === 1 ? "" : "s"} (see review-pending)`
    : "";
  return `${label}${scope}: +${r.messages_added} new · ${r.messages_updated} updated · ${r.messages_deleted} removed${filtered}${skipped}${errors}${problems}`;
}
/**
 * Run a sync as a tracked task, with the core's progress routed into it.
 *
 * Both syncs share this so neither can forget to release the reporter slot —
 * a stale handle would send the next sync's progress into a finished task.
 */
export async function runSync(scope: string, fetch: () => Promise<SyncResult>): Promise<void> {
  const result = await runTask(
    { id: "sync", label: `Syncing ${scope}`, detail: "connecting…" },
    async (task) => {
      syncReporter = task;
      try {
        return await fetch();
      } finally {
        syncReporter = null;
      }
    },
  );

  // Unread/total counts move with the sync, so refresh the sidebar too, and
  // keep the cursor: a refresh should not throw away your place in the list.
  await refreshFolders();
  if (state.tasks.view === "today") {
    // Today's mail half is All Inboxes, which is exactly what a sync changes.
    // `loadMessages` deliberately refuses to act inside a task view, so the
    // merged list is rebuilt here instead.
    await loadInboxes();
    renderList();
  } else if (state.search) {
    // A search is a question about the mailbox, and the mailbox just
    // changed; the answer is refreshed, not thrown away.
    await refreshSearch();
  } else {
    await loadMessages(true);
  }
  setStatus(syncSummary(scope, result));

  // New headers arrive without bodies, so the mail that just landed is exactly
  // what is missing from search. The queue is newest-first, which puts it at
  // the front. Nothing new means nothing to add, so an idle backfill stays idle.
  if (result.messages_added > 0) autoBackfill();
}
/**
 * Sync only the selected folder.
 *
 * This is the interactive sync. `sync_all` walks every folder of every account,
 * so a single cold folder (Work/Archive is ~198k messages) keeps it running for
 * minutes. One folder is bounded and is what a refresh keystroke should mean.
 */
export async function syncCurrentFolder(): Promise<void> {
  const folder = currentFolder();
  if (!folder?.folder_name) return;
  await runSync(folder.display_name, () =>
    call<SyncResult>(
      "sync_folder",
      { account: folder.account_name, folder: folder.folder_name },
    ),
  );
}
/**
 * Sync every account and folder.
 *
 * Reads are no longer serialized behind a sync, so this stays usable while it
 * runs, and the core reports progress as it goes.
 */
export async function syncAll(): Promise<void> {
  await runSync("all accounts", () => call<SyncResult>("sync_all"));
}
