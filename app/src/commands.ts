import { showKeyHints } from "./keyhints";
import { runTask, isRunning, cancelTask, quitBlockers } from "./tasks";
import { isTaskView, TaskDetail, TaskStepInfo, describeTasks, TaskTagInfo, TaskSyncResult, TaskSummary } from "./tasklist";
import { PROVISIONAL_TITLE } from "./taskdetail";
import { openPicker } from "./picker";
import { openPalette } from "./palette";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Vim } from "./vim";
import { CHAT_COMMANDS, chatActive, inChatSection } from "./chat";
import { advance, back, cancelMarkRead, currentFolder, followCursor, readerHoldsCursor, showConversation, extendSelection, folderTotal, loadMessages, move, navigate, navigateTo, openMessage, openSelection, previewCursor, scrollCursorIntoView, scrollTaskCursorIntoView, toggleRead } from "./actions";
import { COLUMNS, chooseLook, currentLook, zoom } from "./appearance";
import { runBackfill, setBackfillStopped } from "./backfill";
import { Cancelled, askFirst, call, describeWrite, reportWrite, setStatus } from "./bridge";
import { pickFiles } from "./files";
import { openCommandLine, promptCommand } from "./commandline";
import { COMMAND_DOCS } from "./commanddocs";
import { beginStepEdit, selectedSteps, stepsToRemove } from "./stepsui";
import { mergeTasksDialog } from "./taskmerge";
import { refreshExportStatus } from "./statusline";
import { DraftTemplate, editDraft, fromTemplate, openCompose } from "./composing";
import { escape } from "./format";
import { ensureCursorRow, ensureRow, inboxTotal, render, renderList, renderRows, rowAt, sortBy } from "./list";
import { closeDrawer, toggleMaximize, toggleSidebar } from "./maximize";
import { focusPane, setTodayKept, showTodaySection } from "./panes";
import { fieldKind, setFieldKind } from "./searchfield";
import { todayLayout, todayMixed } from "./layout";
import { holdTimelineCursor } from "./mixed";
import { actAcrossKinds, anySelected, clearTimelineSelection, extendOnTimeline, reportTimelineSelection, toggleOnTimeline } from "./mixedselect";
import { AccountHealth, BodyBackfill, DeleteOutcome, FilterOutcome, FolderInfo, HighlightCount, MessageHeader, RecurrenceSuggestion, SearchHit, SortKey, SyncResult, WriteOutcome } from "./model";
import { READING_VIEWS, ReadingView, archiveTarget, openReading, readLater, setTodayReading, setTodayReadingShows } from "./reading";
import { accountFolders, applyPending, moveNow, nextMatch, offerUndo, openFolderAt, pickFolder, runSearchQuery, setSubscribed, sortSearch, transferAcross, undoLast, groupUndo, sayMoved } from "./moving";
import { refreshOutbox, retryOutbox, showOutbox } from "./outbox";
import { showAccountStatus, showFeedStatus, showPanel, showPendingReview } from "./panels";
import { toggleHelp } from "./help";
import { actOnAttachment, availableViews, renderReader, selectReaderText, setView } from "./reader";
import { openAllInConversation, scrollConversation, shownConversation, toggleInConversation } from "./conversation";
import { focusSearch, renderSearchBox } from "./search";
import { clearFilter, openFilter } from "./filter";
import { clearSelection, reportSelection, selectionSize } from "./selection";
import { hasMailHalf, highlightLook, openPin, pinLabel, showPinKeys, highlightMarks, inTaskSection, inTaskView, refreshFolders, refreshPendingCounts, renderFolders, selectableFolders, taskSectionActive, toggleFold, togglePin } from "./sidebar";
import { ITEM, SearchKind, WINDOW, el, openPanel, state, todayHasMail } from "./state";
import { closeItemWindow } from "./itemwindow";
import { describeCount, runSync, syncAll, syncCurrentFolder } from "./sync";
import { toggleSortMenu, toggleTodayMenu } from "./toolbar";
import { nextSection, todayKeeps } from "./today";
import { queuedWrite, refreshList, settleWrites, withCursorMessage, withMessages } from "./targets";
import { acceptTaskEdit, attachFilesToTask, beginTaskEdit, cancelTaskEdit, findTag, moveTaskEditField, newTaskPostponeDate, offerTags, pickDate, setBothTaskDates, setTaskDate } from "./taskedit";
import { moveTaskBy, selectTaskRange } from "./taskpointer";
import { showIphoneSetup } from "./iphonesetup";
import { dueFirstOn, setDueFirst, groupByTagOn, setGroupByTag, chooseTodayAccount, clearCompletedFromToday, previewTaskCursor, showSearchKind, toggleSearchCompleted, createTasksFromMessages, cursorTask, loadInboxes, loadTaskDetail, openLinkedMessage, openTaskById, openTaskView, paintTaskCursor, refreshTasks, renderTaskList, setTodayColumns, setTodayLayout, setTodayThreads, targetTaskIds, targetTasksAreAllComplete, withTasks } from "./taskview";

/** A bound action. `args` are the whitespace-separated words after the name. */
export type Command = (args: string[], count: number) => void | Promise<void>;
/**
 * The command vocabulary of `config.yml`.
 *
 * Names match the action strings in the binding tables. Anything absent is
 * reported by name in the status line rather than failing silently, so an
 * unimplemented binding is obvious the first time it is pressed.
 */
/** The mail commands, and everything else that needs no tasks account. */
/** `toggle-today-mail` and `toggle-today-tasks`: `on`, `off`, or the other one. */
function toggleTodayKept(section: "mail" | "tasks", choice: string | undefined): void {
  if (choice !== undefined && choice !== "on" && choice !== "off") {
    setStatus(`toggle-today-${section}: on or off`);
    return;
  }
  setTodayKept(section, choice === undefined ? undefined : choice === "on");
}

const MAIL_COMMANDS: Record<string, Command> = {
  noop: () => {},

  // Navigation. `next-item`/`prev-item` are the task-list spellings of the
  // same motion.
  "next-message": (_a, n) => navigate(n),
  "prev-message": (_a, n) => navigate(-n),
  advance: (_a, n) => advance(n),
  "next-item": (_a, n) => navigate(n),
  "prev-item": (_a, n) => navigate(-n),
  "scroll-down": (_a, n) => navigate(n),
  "scroll-up": (_a, n) => navigate(-n),
  // In a conversation the reader has the keyboard of, a page is a page of it.
  "next-page": (_a, n) => (state.focus === "reader" && scrollConversation(n) ? undefined : navigate(10 * n)),
  "prev-page": (_a, n) => (state.focus === "reader" && scrollConversation(-n) ? undefined : navigate(-10 * n)),
  top: () => navigateTo("top"),
  bottom: () => navigateTo("bottom"),
  "jump-back": async () => (await import("./jump")).jumpBack(),

  // In a conversation the reader has the keyboard of, the message the keys
  // are on opens, or closes.
  "open-message-detail": () => (state.focus === "reader" && toggleInConversation() ? undefined : openSelection()),
  /** Open every message of the conversation in the reader. */
  "open-all-messages": () => {
    if (!openAllInConversation(true)) setStatus("not a conversation; zt shows the folder as conversations");
  },
  /** Close every message of the conversation in the reader to a line. */
  "close-all-messages": () => {
    if (!openAllInConversation(false)) setStatus("not a conversation; zt shows the folder as conversations");
  },
  "toggle-detail": () => {
    if (state.focus === "reader" && closeDrawer()) return;
    state.focus = state.focus === "reader" ? "list" : "reader";
    // Maximized, the reader is not kept up with the cursor while it is away.
    if (state.focus === "reader" && !openPanel() && !taskSectionActive() && !readerHoldsCursor()) {
      return openMessage(false);
    }
    render();
  },
  back: () => {
    // A window of one item has no list to go back to.
    if (ITEM && !openPanel()) {
      setStatus("q or ⌘W closes the window");
      return;
    }
    back();
  },

  // `sync` covers every account, because a refresh that silently touched only
  // the folder under the cursor meant pressing it once per folder to see new
  // mail. It was scoped that way while a sync blocked every read; now that it
  // does not, and progress is reported, the whole-mailbox sync is the sane
  // default. `sync-folder` keeps the narrow behaviour.
  sync: () => syncAll(),
  "sync-all": () => syncAll(),
  "sync-folder": () => syncCurrentFolder(),
  /** What the last sync of every folder did, and the connections opened. */
  "sync-stats": async () => {
    const text = await call<string>("sync_stats");
    state.info = { title: "Sync statistics", text };
    state.focus = "reader";
    render();
  },

  // Flag writes go straight to the server, unlike the staged actions below.
  "toggle-read": async () => {
    if (await actAcrossKinds("toggle-read", "chat-toggle-read", false)) return;
    await toggleRead();
  },
  "stage-read": () =>
    withMessages(async (t, account, folder) => {
      // Marking an already-read message read is a no-op on the server, so the
      // whole set goes; only the rows on screen need correcting.
      const unread = t.rows.filter((m) => !m.is_read);
      if (t.uids.length === 0) return;
      const undo = `mark ${describeCount(unread.length)} read`;
      queuedWrite(
        unread,
        (m) => { m.is_read = true; },
        () => call("mark_read", { account, folder, uids: t.uids, read: true, undo }),
      );
      if (unread.length > 0) offerUndo(`marked ${describeCount(unread.length)} read`);
    }),
  /**
   * Open the web address a `message_links` rule finds in the subject or the
   * body of the message under the cursor: the first rule's, or the one named.
   */
  "open-link": async (args) => {
    const wanted = args.join(" ");
    const open = async (links: { name: string | null; url: string }[], what: string) => {
      const link = wanted ? links.find((l) => l.name === wanted) : links[0];
      if (!link) {
        setStatus(wanted ? `no ${wanted} link in this ${what}` : `no link in this ${what}`);
        return;
      }
      await call("open_external", { url: link.url });
      setStatus(`opened ${link.url}`);
    };
    // A task: an address in its notes or title, or what a link rule makes
    // of its title.
    const task = taskSectionActive() ? cursorTask() : undefined;
    if (task) {
      await open(await call("task_links", { id: task.id }), "task");
      return;
    }
    await withCursorMessage(async (msg) => open(await call("message_links", { id: msg.id }), "message"));
  },
  "toggle-flag": () =>
    withMessages(async (t, account, folder) => {
      // One state for the whole set, decided by whether any of them still
      // needs flagging. Toggling each row against itself would leave a mixed
      // selection exactly as mixed as it started, only inverted.
      const flagged = t.rows.some((m) => !m.is_flagged);
      const undo = `${flagged ? "flag" : "unflag"} ${describeCount(t.count)}`;
      queuedWrite(
        t.rows,
        (m) => { m.is_flagged = flagged; },
        () => call("mark_flagged", { account, folder, uids: t.uids, flagged, undo }),
      );
      offerUndo(`${flagged ? "flagged" : "unflagged"} ${describeCount(t.count)}`);
    }),

  // Staged actions: recorded locally, applied to the server by `expunge`.
  "mark-delete": async () => {
    await withMessages(async (t, account, folder) => {
      const said = `marked ${describeTargets(t.rows, t.count)} for deletion`;
      setStatus(`${said} · U unstage`);
      queuedWrite(
        t.rows,
        (m) => { m.pending_action = "Delete"; },
        () =>
          call("stage_action", {
            account,
            folder,
            uids: t.uids,
            action: "Delete",
            target: null,
            undo: `mark ${describeTargets(t.rows, t.count)} for deletion`,
          }),
      );
    });
    void settleWrites().then(refreshPendingCounts).then(renderFolders, () => {});
  },
  "mark-move": async (args) => {
    const target = args.join(" ") || (await pickFolder("Stage a move to"));
    if (!target) return;
    await withMessages(async (t, account, folder) => {
      setStatus(`marked ${describeTargets(t.rows, t.count)} for ${target} · U unstage`);
      queuedWrite(
        t.rows,
        (m) => {
          m.pending_action = "Move";
          m.pending_target = target;
        },
        () =>
          call("stage_action", {
            account,
            folder,
            uids: t.uids,
            action: "Move",
            target,
            undo: `mark ${describeTargets(t.rows, t.count)} for ${target}`,
          }),
      );
    });
    void settleWrites().then(refreshPendingCounts).then(renderFolders, () => {});
  },
  unmark: async () => {
    await withMessages(async (t, account, folder) => {
      queuedWrite(
        t.rows,
        (m) => {
          m.pending_action = null;
          m.pending_target = null;
        },
        () => call("unstage_action", { account, folder, uids: t.uids, undo: `unmark ${describeTargets(t.rows, t.count)}` }),
      );
      offerUndo(`unmarked ${describeTargets(t.rows, t.count)}`);
    });
    void settleWrites().then(refreshPendingCounts).then(renderFolders, () => {});
  },
  expunge: async () => {
    // Today's mail is every inbox, not the folder the sidebar last opened.
    if (inTaskView()) {
      const inboxes = (state.inboxFolders ?? []).filter((f) => f.folder_name);
      if (inboxes.length === 0) {
        setStatus("nothing staged here; open a folder to apply its staged actions");
        return;
      }
      await applyPending(
        inboxes.map((f) => ({ account: f.account_name, folder: f.folder_name! })),
        "the inboxes",
      );
      return;
    }
    const folder = currentFolder();
    if (!folder?.folder_name) return;
    await applyPending([{ account: folder.account_name, folder: folder.folder_name }], folder.display_name);
  },
  /** Apply every staged action in every folder of every account. */
  "expunge-all": () => applyPending(null, "all folders"),
  /**
   * Forget what is staged in the current folder — or everywhere, with `all` —
   * without touching the server.
   */
  "discard-pending": async (args) => {
    const everywhere = args[0] === "all";
    // In Today, the inboxes it shows; elsewhere the open folder.
    const folders = everywhere
      ? [{}]
      : inTaskView()
        ? (state.inboxFolders ?? [])
            .filter((f) => f.folder_name)
            .map((f) => ({ account: f.account_name, folder: f.folder_name }))
        : [currentFolder()]
            .filter((f) => f?.folder_name)
            .map((f) => ({ account: f!.account_name, folder: f!.folder_name }));
    if (folders.length === 0) return;
    await settleWrites();
    let dropped = 0;
    const said = () => `discarded ${dropped} staged action${dropped === 1 ? "" : "s"}${everywhere ? " everywhere" : ""}`;
    const endGroup = await groupUndo();
    try {
      for (const where of folders) {
        dropped += await call<number>("discard_pending", { ...where, undo: "discard staged actions" });
      }
    } finally {
      await endGroup(said().replace("discarded", "discard"));
    }
    await refreshPendingCounts();
    await refreshList();
    renderFolders();
    if (openPanel()?.title === "Staged actions") await showPendingReview();
    if (dropped > 0) offerUndo(said());
    else setStatus(said());
  },
  /** What is staged, everywhere, as a panel with the subjects. */
  "review-pending": () => showPendingReview(),
  /** The mail that is queued or was refused, as a panel. */
  outbox: async () => {
    await refreshOutbox();
    showOutbox();
  },
  /** Put everything in the Outbox back in line, and send it now. */
  "retry-outbox": () => retryOutbox(),
  // Folder management, on the open folder: `:create-folder Name` makes a
  // sibling at the top level, the rest act on the folder itself.
  "create-folder": async (args) => {
    const name = args.join(" ").trim();
    const folder = currentFolder();
    if (!folder) {
      setStatus("no account selected");
      return;
    }
    if (!name) {
      setStatus("create-folder: give a name, e.g. create-folder Receipts");
      return;
    }
    const created = await call<string>("create_folder", { account: folder.account_name, name });
    await runSync(created, () =>
      call<SyncResult>("sync_folder", { account: folder.account_name, folder: created }),
    );
    offerUndo(`created ${created}`);
  },
  "rename-folder": async (args) => {
    const to = args.join(" ").trim();
    const folder = currentFolder();
    if (!folder?.folder_name) {
      setStatus("open a folder to rename it");
      return;
    }
    if (!to) {
      setStatus("rename-folder: give the new name");
      return;
    }
    const renamed = await call<string>("rename_folder", {
      account: folder.account_name,
      from: folder.folder_name,
      to,
    });
    await refreshFolders();
    const at = selectableFolders().findIndex(
      (f) => f.account_name === folder.account_name && f.folder_name === renamed,
    );
    if (at >= 0) await openFolderAt(at);
    offerUndo(`renamed ${folder.display_name} to ${renamed}`);
  },
  "delete-folder": async () => {
    const folder = currentFolder();
    if (!folder?.folder_name) {
      setStatus("open a folder to delete it");
      return;
    }
    const count = folder.total_count;
    if (
      !(await askFirst(
        `Delete the folder ${folder.display_name} on the server?\n\n` +
          `Its ${count} message${count === 1 ? "" : "s"} go with it. There is no way back.`,
        "Delete Folder",
      ))
    ) {
      setStatus("not deleted");
      return;
    }
    await call<number>("delete_folder", { account: folder.account_name, name: folder.folder_name });
    await refreshFolders();
    await openFolderAt(Math.max(0, state.openFolder - 1));
    setStatus(`deleted ${folder.display_name}`);
  },
  "subscribe-folder": () => setSubscribed(true),
  "unsubscribe-folder": () => setSubscribed(false),
  /** Show the open folder as conversations, or as messages again. */
  "toggle-threads": async () => {
    if (state.inboxFolders && !state.search) {
      state.threads = !state.threads;
      setTodayThreads(state.threads);
      state.expandedThreads.clear();
      state.messageCursor = 0;
      await loadInboxes();
      renderList();
      if (state.tasks.section === "mail") previewCursor();
      setStatus(state.threads ? "threads: on · za opens one, gc shows the whole conversation" : "threads: off");
      return;
    }
    const folder = currentFolder();
    if (!folder?.folder_name || state.search || inTaskView()) {
      setStatus("open a folder to show it as threads");
      return;
    }
    state.threads = !state.threads;
    state.expandedThreads.clear();
    await loadMessages();
    setStatus(
      state.threads ? "threads: on · za opens one, gc shows the whole conversation" : "threads: off",
    );
  },
  /** Open or close the thread under the cursor. */
  "toggle-thread": async () => {
    // In the sidebar the same key folds an account or a folder.
    if (state.focus === "folders") {
      if (!toggleFold()) setStatus("nothing to fold here");
      return;
    }
    // Among the tasks a search found, it unfolds the completed ones (§26).
    if (state.tasks.view === "search" && taskSectionActive()) {
      toggleSearchCompleted();
      return;
    }
    // In the reader, the message of the conversation the keys are on.
    if (state.focus === "reader" && toggleInConversation()) return;
    if (!state.threads || (state.inboxFolders && state.tasks.section !== "mail")) {
      setStatus("not a threaded list; zt switches");
      return;
    }
    const m = rowAt(state.messageCursor);
    const id = m?.thread_id;
    if (!m || !id || (m.thread_count ?? 1) < 2) {
      setStatus("the message is a thread of its own");
      return;
    }
    if (state.expandedThreads.has(id)) state.expandedThreads.delete(id);
    else state.expandedThreads.add(id);
    const folder = currentFolder();
    if (!state.inboxFolders && !folder?.folder_name) return;
    // The rows shift under the cursor; keep it on the thread's own row, which
    // is the nearest row above with this thread's id at depth 0 — a member's
    // depth counts replies, not rows.
    let anchor = state.messageCursor;
    while (anchor > 0) {
      const above = rowAt(anchor);
      if (!above || above.thread_id !== id || (above.depth ?? 0) === 0) break;
      anchor -= 1;
    }
    const generation = ++state.generation;
    state.pages.clear();
    state.fetching.clear();
    const total = state.inboxFolders ? await inboxTotal() : await folderTotal(folder!);
    if (generation !== state.generation) return;
    state.total = total;
    state.messageCursor = Math.min(anchor, Math.max(0, state.total - 1));
    renderList();
    await ensureRow(state.messageCursor);
    renderRows();
    scrollCursorIntoView();
  },
  /** The whole conversation of the cursor message, from every folder of its account. */
  "open-thread": () =>
    withCursorMessage(async (msg) => {
      const hits = await call<SearchHit[]>("thread_messages", { id: msg.id });
      state.focus = "list";
      showConversation(hits, msg.id, msg.subject ?? null);
      await openMessage(false);
    }),
  /** The highlight rules, and how many messages of this folder each matches. */
  highlights: async () => {
    const folder = currentFolder();
    if (!folder?.folder_name) {
      setStatus("open a folder to count its highlights");
      return;
    }
    const counts = await runTask(
      { id: "highlights", label: "Counting highlights", detail: folder.display_name },
      () =>
        call<HighlightCount[]>("highlight_counts", {
          account: folder.account_name,
          folder: folder.folder_name,
        }),
    );
    const rows = counts
      .map((c) => {
        const look = highlightLook([c.rule]);
        const swatch = look.color || look.background
          ? `<span class="swatch" style="${look.color ? `color:${escape(look.color)};` : ""}${look.background ? `background:${escape(look.background)}` : ""}">Aa</span>`
          : "";
        return `<tr><td class="what">${swatch}${highlightMarks([c.rule])}</td><td>${escape(c.rule)}</td><td class="dim">${c.count}</td></tr>`;
      })
      .join("");
    showPanel(
      "Highlights",
      counts.length
        ? `<section><h2>${escape(folder.account_name)} / ${escape(folder.display_name)}</h2><table>${rows}</table></section>`
        : `<div class="empty">No highlight rule applies to ${escape(folder.account_name)}.</div>`,
    );
  },
  /**
   * Run the filter rules on the selection, else on the whole folder — in
   * Today, on the inboxes it shows; with `--all`, on every folder a rule
   * watches. Mail already filtered is filtered again: this is how a rule
   * written today is run on yesterday's mail.
   */
  filter: async (args) => {
    const outcome: FilterOutcome = { filtered: 0, warnings: [] };
    const add = (o: FilterOutcome) => {
      outcome.filtered += o.filtered;
      outcome.warnings.push(...o.warnings);
    };
    const undo = "run the filters";
    // Today's mail is every inbox, not the folder the sidebar last opened.
    const folders = (inTaskView() ? (state.inboxFolders ?? []) : [currentFolder()]).filter(
      (f): f is FolderInfo => !!f?.folder_name,
    );
    if (args[0] !== "--all" && selectionSize() === 0 && folders.length === 0) {
      setStatus("no folder to filter; :filter --all runs the rules everywhere");
      return;
    }
    const endGroup = await groupUndo();
    try {
      if (args[0] === "--all") {
        add(
          await runTask(
            { id: "filter", label: "Running the filters", detail: "every folder a rule watches" },
            () => call<FilterOutcome>("run_filters_all", { undo }),
          ),
        );
      } else if (selectionSize() > 0) {
        await withMessages(async (t, account, folder) => {
          add(await call<FilterOutcome>("run_filters", { account, folder, uids: t.uids, undo }));
        }, true);
      } else {
        for (const folder of folders) {
          add(
            await runTask(
              { id: "filter", label: "Running the filters", detail: folder.display_name },
              () =>
                call<FilterOutcome>("run_filters", {
                  account: folder.account_name,
                  folder: folder.folder_name,
                  uids: null,
                  undo,
                }),
            ),
          );
        }
      }
    } finally {
      await endGroup(`filter ${describeCount(outcome.filtered)}`);
    }
    await refreshFolders();
    await refreshList();
    if (openPanel()?.title === "Staged actions") await showPendingReview();
    const problems = outcome.warnings.length
      ? `; ${outcome.warnings.length} problem${outcome.warnings.length === 1 ? "" : "s"} (see review-pending)`
      : "";
    if (outcome.filtered === 0 && !problems) setStatus("no filter rule matched");
    else if (outcome.filtered > 0) offerUndo(`filtered ${describeCount(outcome.filtered)}${problems}`);
    else setStatus(`filtered ${describeCount(outcome.filtered)}${problems}`);
  },
  /** The log as it is written and the recent jobs, in a tab of their own. */
  console: () => call("open_console"),
  /** Start a focus, change the one that runs, or end it with `off`. */
  "focus-mode": async (args) => (await import("./focusmode")).focusMode(args),
  /** Show the message on screen as HTML, plain text or its source. */
  view: (args) => {
    const mode = args[0];
    if (mode !== "html" && mode !== "text" && mode !== "raw") {
      setStatus("view: html, text or raw");
      return;
    }
    // A conversation is drawn one way throughout, each message as it can.
    if (shownConversation()) {
      if (mode === "raw") {
        setStatus("a conversation has no one source · za in the list opens it into messages");
        return;
      }
      setView(mode);
      return;
    }
    if (!state.detail) return;
    if (mode !== "raw" && !availableViews(state.detail).includes(mode)) {
      setStatus(`this message has no ${mode} part`);
      return;
    }
    setView(mode);
  },
  /** Let the focused pane fill the window, the others drawers over it; or undo that. */
  "toggle-maximize": () => toggleMaximize(),
  /** Put the folders away, the list and the reader taking their width; or bring them back. */
  "toggle-sidebar": () => toggleSidebar(),
  /** Text size, for the machine: ⌘+ and ⌘- step it, ⌘0 comes back to the config's. */
  "zoom-in": () => zoom(1),
  "zoom-out": () => zoom(-1),
  "zoom-reset": () => zoom(0),
  /**
   * Mark the task under the cursor, or the selection, as being worked on, or
   * take the mark off if they all carry it; `work-on clear` takes it off every
   * task (§19).
   */
  "work-on": async (args) => {
    if (args[0] === "clear") {
      const n = await call<number>("clear_working_on");
      await refreshTasks();
      if (n) offerUndo(`no longer working on ${describeTasks(n)}`);
      else setStatus("nothing was being worked on");
      return;
    }
    const ids = targetTaskIds();
    if (ids.length === 0) {
      setStatus("no task selected");
      return;
    }
    const all = ids.every((id) => state.tasks.workingOn.some((t) => t.id === id));
    await call("update_tasks", { ids, edit: { is_working_on: !all } });
    await refreshTasks();
    const title = ids.length === 1 ? state.tasks.rows.find((t) => t.id === ids[0])?.title : null;
    setStatus(`${all ? "no longer working on" : "working on"} ${title ? `'${title}'` : describeTasks(ids.length)}`);
  },
  /** Show the code that sets Orb up on an iPhone, password included. */
  "set-up-iphone": async () => {
    await showIphoneSetup();
  },
  /** Export every task to the configured directory now (§20.3). */
  "export-tasks": async () => {
    setStatus("exporting the tasks…");
    const done = await call<{ path: string; complete: boolean; tasks: number; missing: string[] }>("export_tasks");
    await refreshExportStatus();
    setStatus(
      done.complete
        ? `exported ${describeTasks(done.tasks)} to ${done.path}`
        : `exported ${describeTasks(done.tasks)} to ${done.path}, incomplete: ${done.missing.join("; ")}`,
    );
  },
  /**
   * Import a snapshot (§21): what it would do first, and only on a yes.
   * Without a path, a folder is picked; `--include-purged` brings back what
   * was purged here.
   */
  "import-tasks": async (args) => {
    const includePurged = args.includes("--include-purged");
    let path: string | null = args.filter((a) => a !== "--include-purged").join(" ").trim() || null;
    if (!path) {
      const [picked] = await pickFiles({
        kind: "folder",
        title: "Import a task snapshot",
        defaultPath: state.tasks.export?.directory ?? undefined,
      });
      path = picked ?? null;
    }
    if (!path) return;
    const plan = await call<TaskImportPlan>("import_tasks", { path, apply: false, includePurged });
    const lines = [
      `${plan.new} new, ${plan.newer} newer than here, ${plan.older} as new or newer here (their history is merged).`,
      plan.tags ? `${plan.tags} tags are added.` : "",
      plan.purged.length
        ? `${plan.purged.length} purged here and left out: ${plan.purged.slice(0, 5).join(", ")}${plan.purged.length > 5 ? " …" : ""}. :import-tasks --include-purged brings them back.`
        : "",
      plan.problems.length ? `Left out: ${plan.problems.join("; ")}` : "",
      "What is imported is uploaded, and wins on every machine.",
    ].filter(Boolean);
    const go = await askFirst(`Import tasks exported ${plan.exported_at}?\n\n${lines.join("\n")}`, "Import");
    if (!go) {
      setStatus("nothing imported");
      return;
    }
    const done = await call<TaskImportPlan>("import_tasks", { path, apply: true, includePurged });
    await refreshTasks();
    setStatus(`imported ${describeTasks(done.new + done.newer)}, merged the history of ${done.older}`);
  },
  /**
   * Import Things 3 (§22), from its own database or the one named: what it
   * would do first, and only on a yes. Importing again brings over what
   * changed there, except into tasks edited here since.
   */
  "import-things": async (args) => {
    const path = args.join(" ").trim() || null;
    const plan = await call<ThingsImportPlan>("import_things", { path, apply: false });
    if (plan.tasks === 0 && plan.updated === 0 && plan.trashed_here === 0) {
      setStatus(
        plan.already || plan.edited_here.length
          ? `nothing new in Things: ${plan.already + plan.edited_here.length} tasks are here already`
          : "no tasks in Things",
      );
      return;
    }
    const left = [
      plan.trashed ? `${plan.trashed} in the Things trash` : "",
      plan.projects || plan.headings ? `${plan.projects} projects and ${plan.headings} headings, which become tags` : "",
      plan.repeat_instances ? `${plan.repeat_instances} open repeats, which their repeating to-do stands for` : "",
    ].filter(Boolean);
    const lines = [
      plan.tasks ? `${plan.tasks} to-dos become tasks: ${plan.completed} done, ${plan.repeating} repeating.` : "",
      plan.updated
        ? `${describeTasks(plan.updated)} from an earlier import ${plan.updated === 1 ? "takes" : "take"} the changes made in Things.`
        : "",
      plan.trashed_here
        ? `${describeTasks(plan.trashed_here)} trashed in Things ${plan.trashed_here === 1 ? "goes" : "go"} to the trash here.`
        : "",
      plan.already ? `${plan.already} are unchanged.` : "",
      plan.edited_here.length
        ? `Edited here since, and so left as they are: ${plan.edited_here.join(", ")}.`
        : "",
      plan.new_tags.length ? `Tags added: ${plan.new_tags.join(", ")}.` : "",
      plan.unmapped.length ? `Repeating in a way Orb cannot say, so imported without dates: ${plan.unmapped.join(", ")}.` : "",
      left.length ? `Left out: ${left.join("; ")}.` : "",
      "Things itself is not changed.",
    ].filter(Boolean);
    if (!(await askFirst(`Import from Things 3?\n\n${lines.join("\n")}`, "Import"))) {
      setStatus("nothing imported");
      return;
    }
    const done = await call<ThingsImportPlan>("import_things", { path, apply: true });
    await refreshTasks();
    const parts = [
      done.tasks ? `imported ${describeTasks(done.tasks)}` : "",
      done.updated ? `updated ${done.updated}` : "",
      done.trashed_here ? `trashed ${done.trashed_here}` : "",
    ].filter(Boolean);
    setStatus(`${parts.join(", ")} from Things`);
  },
  /** Link the mail every task's notes name by `message://` URL (§6). */
  "link-mail-in-notes": async () => {
    const changed = await call<number>("link_mail_in_notes");
    if (changed === 0) {
      setStatus("no unlinked message:// links in any task's notes");
      return;
    }
    await refreshTasks();
    offerUndo(`linked mail in ${describeTasks(changed)}`);
  },
  /** Today without the tasks completed so far, on this machine (§10.2). */
  "clear-completed": () => clearCompletedFromToday(),
  /** The window that shows tasks only, opened or brought forward. */
  "open-chat-window": async () => {
    await call("open_chat_window");
  },
  "open-tasks-window": async () => {
    await call("open_tasks_window");
  },
  /** The other look, or the one named, for every window on this machine. */
  "toggle-look": () => chooseLook(currentLook() === "terminal" ? "hybrid" : "terminal"),
  look: (args) => {
    const look = args[0];
    if (look !== "hybrid" && look !== "terminal") {
      setStatus("look: say hybrid or terminal");
      return;
    }
    chooseLook(look);
  },
  /** Draw the HTML mail on screen dark, or as it came, for this session. */
  "toggle-dark-mail": () => {
    state.darkMail = !state.darkMail;
    renderReader();
    setStatus(state.darkMail ? "HTML mail drawn dark" : "HTML mail drawn as it came");
  },
  /** Each account's standing with its server, as a panel. */
  "account-status": async () => {
    state.health = (await call<AccountHealth[]>("account_statuses")) ?? [];
    showAccountStatus();
  },
  /** Every key of every mode, from the tables in force, over the window. */
  help: () =>
    toggleHelp({
      context: activeContext(),
      bindings: activeBindings(),
      run: (command) => void dispatchCommand(command, 1),
      prompt: promptCommand,
    }),
  /** Close the reader, or the panel in it, or a window of one item; never the app. */
  close: () => {
    if (openPanel()) {
      state.info = null;
      state.focus = "list";
      render();
      // The rows may have moved under the cursor while the panel had the reader.
      followCursor();
      return;
    }
    if (ITEM) return closeItemWindow();
    if (state.focus === "reader") {
      back();
      return;
    }
    if (closeDrawer()) return;
    setStatus("nothing to close · ⌘Q quits");
  },
  "next-folder": (_a, n) => openFolderAt(state.openFolder + n),
  "open-pinned": (args) => openPin(Number(args[0] ?? 1)),
  "go-to": (_a, n) => openPalette("", n),
  "command-palette": (args, n) => openPalette(args[0] ?? ">", n),
  "toggle-pin": () => togglePin(),
  "prev-folder": (_a, n) => openFolderAt(state.openFolder - n),
  "next-match": (_a, n) => nextMatch(n),
  "prev-match": (_a, n) => nextMatch(-n),
  /** Open attachment `n` (1-based, default the first) of the message on screen. */
  "open-attachment": (args) => actOnAttachment(Number(args[0] ?? 1) - 1, "open"),
  "save-attachment": (args) => actOnAttachment(Number(args[0] ?? 1) - 1, "save"),
  "quick-look": (args) => actOnAttachment(Number(args[0] ?? 1) - 1, "look"),

  // Immediate equivalents, bypassing staging.
  /**
   * Move to Trash now; in Trash itself, delete for good — but only after
   * asking, since that is the one step there is no way back from.
   */
  "delete-immediate": () =>
    withMessages(
      async (t, account, folder) => {
        const trash = await call<string>("trash_folder", { account });
        if (folder === trash) {
          const these = t.count === 1 ? "It is" : "They are";
          if (
            !(await askFirst(
              `Delete ${describeCount(t.count)} for good?\n\n${these} already in ${trash}.`,
              "Delete",
            ))
          ) {
            setStatus("not deleted");
            return false;
          }
        }
        const outcome = await call<DeleteOutcome>(
          "delete_messages",
          { account, folder, uids: t.uids, undo: `delete ${describeCount(t.count)}` },
        );
        await refreshList();
        if (outcome.kind === "Deleted") {
          reportWrite(`deleted ${describeCount(t.count)} for good`, [outcome.warning]);
        } else {
          sayMoved(`moved ${describeCount(t.count)} to ${outcome.trash}`, outcome);
        }
        return undefined;
      },
      // The rows are gone; a selection pointing at them is not a selection.
      true,
    ),
  "move-immediate": async (args) => {
    const target = args.join(" ") || (await pickFolder("Move to"));
    if (!target) return;
    await moveNow(() => target);
  },
  /** Copy to another folder of the account, on the server; the originals stay. */
  "copy-immediate": async (args) => {
    const target = args.join(" ") || (await pickFolder("Copy to"));
    if (!target) return;
    await withMessages(async (t, account, folder) => {
      if (target === folder) {
        setStatus(`already in ${target}`);
        return false;
      }
      const outcome = await call<WriteOutcome>("copy_messages", {
        account,
        folder,
        target,
        uids: t.uids,
        undo: `copy ${describeCount(t.count)} to ${target}`,
      });
      offerUndo(describeWrite(`copied ${describeCount(t.count)} to ${target}`, [outcome.warning]));
      return undefined;
    });
  },
  /** Move into a folder of another account: fetched from the one server, appended to the other. */
  "move-across": () => transferAcross(false),
  "copy-across": () => transferAcross(true),
  /** File the message in the account's archive folder. */
  archive: async () => {
    if (await actAcrossKinds("archive", "chat-done", true)) return;
    await moveNow(
      (account, folder) => archiveTarget(account, folder) ?? accountFolders(account).archive,
      "archive_folder",
    );
  },
  /** Set the message aside in the reading folder, from whichever account it is in. */
  "read-later": () => readLater(),
  /** Ask every feed of the reading list for new items now. */
  "poll-feeds": async () => {
    setStatus("asking the feeds…");
    const report = await call<{ polled: number; appended: number; problems: string[] }>("poll_feeds");
    const problems = report.problems.length ? ` · ${report.problems.length} failed (feed-status says which)` : "";
    setStatus(`${report.appended} new item${report.appended === 1 ? "" : "s"} from ${report.polled} feed${report.polled === 1 ? "" : "s"}${problems}`);
  },
  /** Each feed: when it was last asked, and what went wrong. */
  "feed-status": () => showFeedStatus(),
  /** Open the reading list, its must-read items, or its archive. */
  "open-reading": async (args) => {
    const view = args[0] ?? "list";
    if (!READING_VIEWS.some((v) => v.view === view)) {
      setStatus(`open-reading: unknown view '${view}'`);
      return;
    }
    await openReading(view as ReadingView);
    state.focus = "list";
    render();
  },
  /** Put the message in the account's junk folder. */
  spam: () => moveNow((account) => accountFolders(account).junk, "junk_folder"),
  /** Bring a message out of the junk folder back to the inbox. */
  "not-spam": () =>
    moveNow(
      (account, folder) =>
        folder === accountFolders(account).junk ? "INBOX" : null,
      "junk_folder",
    ),

  /** Today's mail above its tasks, or beside them: `stacked`, `columns`, or the other one. */
  "toggle-today-layout": async (args) => {
    const choice = args[0];
    if (choice !== undefined && choice !== "stacked" && choice !== "columns" && choice !== "mixed") {
      setStatus("toggle-today-layout: stacked, columns or mixed");
      return;
    }
    if (choice === undefined) await setTodayColumns();
    else await setTodayLayout(choice);
    sayTodayLayout();
  },
  /** Show Today as one timeline beside the tasks, or go back to the layout it left. */
  "toggle-today-mixed": async () => {
    await setTodayLayout();
    sayTodayLayout();
  },
  /** Show Today's mail, or put it away: `on`, `off`, or the other one. */
  "toggle-today-mail": (args) => toggleTodayKept("mail", args[0]),
  /** Show Today's tasks, or put them away: `on`, `off`, or the other one. */
  "toggle-today-tasks": (args) => toggleTodayKept("tasks", args[0]),
  /** The toolbar's menu of what Today shows, and how. */
  "today-menu": () => toggleTodayMenu(),
  /** Show Today's reading section, or put it away: `on`, `off`, or the other one. */
  "toggle-today-reading": async (args) => {
    const choice = args[0];
    if (choice !== undefined && choice !== "on" && choice !== "off") {
      setStatus("toggle-today-reading: on or off");
      return;
    }
    await setTodayReading(choice === undefined ? undefined : choice === "on");
  },
  /** What Today's reading section shows: `must-read` or `new`. */
  "today-reading": async (args) => {
    const choice = args[0];
    if (choice !== "must-read" && choice !== "new") {
      setStatus("today-reading: must-read or new");
      return;
    }
    await setTodayReadingShows(choice);
  },
  /** Today's mail from one account's inbox, or from every account's with `all`. */
  "today-mail": async (args) => {
    const name = args.join(" ").trim();
    if (!name || name === "all") {
      await chooseTodayAccount(null);
      setStatus("Today shows every inbox");
      return;
    }
    if (!state.accounts.some((a) => a.name === name)) {
      setStatus(`today-mail: no account called ${name}`);
      return;
    }
    await chooseTodayAccount(name);
    setStatus(`Today shows ${name}'s inbox`);
  },
  /** The toolbar's sort menu, walked with the arrows. */
  "sort-menu": () => toggleSortMenu(),
  /** Not in config.yml, but bindable: `sort-by date|from|subject`. */
  "sort-by": async (args) => {
    // Search results have one more order than a folder: the query's own.
    if (state.search && args[0] === "relevance") {
      await sortSearch("relevance");
      setStatus("sort: relevance");
      return;
    }
    const key = args[0] as SortKey;
    if (!COLUMNS.some((c) => c.key === key)) {
      setStatus(`sort-by: unknown column '${args[0] ?? ""}'`);
      return;
    }
    const direction = args[1];
    if (direction !== undefined && direction !== "asc" && direction !== "desc") {
      setStatus(`sort-by: say asc or desc, not '${direction}'`);
      return;
    }
    const dir = direction === undefined ? undefined : direction === "asc" ? 1 : -1;
    if (state.search) {
      await sortSearch(key, dir);
      const sort = state.search?.sort;
      if (sort) setStatus(`sort: ${sort.key} ${sort.dir === 1 ? "asc" : "desc"}`);
      return;
    }
    await sortBy(key, dir);
    setStatus(`sort: ${key} ${state.sort.dir === 1 ? "asc" : "desc"}`);
  },

  compose: async () => {
    const folder = currentFolder();
    if (!folder) {
      setStatus("no account selected");
      return;
    }
    const draft = await call<DraftTemplate>("new_draft", { account: folder.account_name });
    await openCompose(folder.account_name, fromTemplate(draft));
  },

  /**
   * Reply to the cursor message.
   *
   * `-aq` in the config's `reply -aq` means reply-all and quote in mutt; only
   * quoting is honoured here, and reply-all is not yet distinguished from a
   * plain reply.
   */
  /**
   * Reply to the message under the cursor.
   *
   * The draft itself is built by the core, which is where the quoting
   * settings, the account's signature and the message's HTML all are. `-a` is
   * mutt's reply-all, which the config's `reply -aq` passes.
   */
  reply: (args) =>
    withCursorMessage(async (msg, account) => {
      const all = args.some((a) => a.startsWith("-") && a.includes("a"));
      const draft = await call<DraftTemplate>("reply_draft", { id: msg.id, all });
      await openCompose(account, fromTemplate(draft));
    }),

  /** Reply to everyone, without needing to remember mutt's flag. */
  "reply-all": () => runCommand("reply -a", 1),

  /** Forward the message under the cursor, attachments and all. */
  forward: () =>
    withCursorMessage(async (msg, account) => {
      const draft = await call<DraftTemplate>("forward_draft", { id: msg.id });
      await openCompose(account, fromTemplate(draft));
    }),

  /** Continue writing the draft under the cursor. */
  "edit-draft": () =>
    withCursorMessage(async (msg, account) => {
      await editDraft(msg, account);
    }),

  /** Let the message on screen fetch its remote images, this once. */
  "load-remote-images": () => {
    const d = state.detail;
    if (!d) {
      setStatus("no message open");
      return;
    }
    state.remoteImages.add(d.id);
    if (availableViews(d).includes("html")) state.view = "html";
    renderReader();
  },

  /** Open the command line, as `:` does in vim. */
  "command-mode": () => openCommandLine(),

  /** Toggle the cursor row's selection and advance, as `<space>` does in mutt. */
  "select-message": (_a, n) => {
    if (todayMixed()) return toggleOnTimeline(n);
    if (state.selected.has(state.messageCursor)) {
      state.selected.delete(state.messageCursor);
    } else {
      state.selected.add(state.messageCursor);
    }
    // Where a later shift-extension counts from: the row last touched by hand.
    state.anchor = state.messageCursor;
    move(n);
    reportSelection();
  },

  /**
   * Extend the selection by a row, the way shift-arrow does everywhere.
   *
   * The cursor moves with it, so the pair reads as one gesture: the anchor
   * stays put and the far end follows the cursor.
   */
  "extend-selection-down": (_a, n) => extendAnySelection(n),
  "extend-selection-up": (_a, n) => extendAnySelection(-n),

  /** Select every message in the folder or every task in the list; in the reader, what it shows. */
  "select-all": () => {
    if (state.focus === "reader") {
      if (!selectReaderText()) setStatus("nothing to select");
      return;
    }
    if (inTaskSection()) {
      selectAllTasks();
      return;
    }
    if (state.focus !== "list") state.focus = "list";
    state.selected.clear();
    state.selectAll = true;
    state.anchor = 0;
    renderRows();
    reportSelection();
  },

  /**
   * Start filling in missing message bodies.
   *
   * Rarely needed by hand — it runs on its own — but asking for it explicitly
   * also lifts a stop from earlier in the session.
   */
  "fetch-bodies": () => {
    setBackfillStopped(false);
    return runBackfill();
  },

  /** Stop the backfill after the batch in flight, and keep it stopped. */
  "stop-fetch-bodies": () => {
    setBackfillStopped(true);
    if (!isRunning("backfill")) {
      setStatus("backfill is not running");
      return;
    }
    cancelTask("backfill");
    setStatus("stopping backfill…");
  },

  /** Report how much of the mailbox has bodies, without fetching any. */
  "backfill-status": async () => {
    const p = await call<BodyBackfill>("body_backfill_progress");
    const shaped = Math.max(0, p.messages - p.unshaped);
    const bodies = Math.max(0, p.total - p.pending);
    // Unreadable messages are skipped for good, so they belong in any honest
    // account of how much of the mailbox is actually searchable.
    const skipped = p.unreadable ? ` · ${p.unreadable.toLocaleString()} unreadable` : "";
    const vectors = p.embeddings_pending
      ? ` · ${p.embeddings_pending.toLocaleString()} to index`
      : "";
    setStatus(
      `structure ${shaped.toLocaleString()}/${p.messages.toLocaleString()} · ` +
        `bodies ${bodies.toLocaleString()}/${p.total.toLocaleString()}${vectors}${skipped}`,
    );
  },

  /**
   * Search the whole mailbox.
   *
   * Takes the rest of the command line, so `:search f:thomas is:unread` passes
   * the query through untouched — the language is the core's, not the shell's.
   */
  search: async (args) => {
    const query = args.join(" ");
    if (!query) {
      setStatus("search: give a query, e.g. f:thomas is:unread");
      return;
    }
    await runSearchQuery(query, query, undefined, undefined, fieldKind());
  },

  /** Put the cursor in the search field. */
  "search-mode": () => focusSearch(),

  /**
   * Show the search's mail and tasks, or one kind of them (§26), without
   * searching again; with no argument, the next kind.
   */
  "search-kind": (args) => {
    const search = state.search;
    if (!search?.taskQuery) {
      setStatus("no search of mail and tasks on screen; / searches");
      return;
    }
    const kinds: SearchKind[] = todayHasMail() ? ["all", "mail", "tasks"] : ["tasks"];
    const asked = args[0] as SearchKind | undefined;
    if (asked !== undefined && !kinds.includes(asked)) {
      setStatus(`search-kind: ${kinds.join(", ")}`);
      return;
    }
    search.kind = asked ?? kinds[(kinds.indexOf(search.kind) + 1) % kinds.length]!;
    setFieldKind(search.kind);
    showSearchKind();
    render();
    if (taskSectionActive()) previewTaskCursor();
    else void openMessage(false);
  },

  /** Leave search: the results, and in a task view without them the filter. */
  "clear-search": () => {
    el.searchInput.value = "";
    renderSearchBox();
    const search = state.search;
    if (search && (search.from !== null || state.tasks.view === "search" || !inTaskView())) {
      state.search = null;
      if (state.tasks.view === "search") {
        state.tasks.view = null;
        state.tasks.rows = [];
      }
      // Back where the search was made from: a task view, or the folder.
      if (search.from) void openTaskView(search.from);
      else void loadMessages();
      setStatus("search cleared");
      return;
    }
    if (state.filter) return runCommand("clear-filter", 1);
    if (!state.search) return;
    state.search = null;
    void loadMessages();
    setStatus("search cleared");
  },

  /** Narrow the list on screen as you type (§4.5); `F`. */
  "filter-mode": () => openFilter(),

  /** Take the filter off the list, showing all of it again. */
  "clear-filter": async () => {
    if (!state.filter) return;
    await clearFilter();
    setStatus("filter cleared");
  },

  /**
   * Move between the two halves of what is on screen.
   *
   * In Today that is the tasks and the mail (§4.2); everywhere else it is the
   * list and the reader, which is the same gesture applied to the same
   * question — "the other one".
   */
  "focus-next-section": () => {
    if (hasMailHalf() && state.focus === "list") {
      // The sections shown, in their order on screen, skipping any that lists nothing.
      showTodaySection(nextSection(state.tasks.section));
      return;
    }
    return runCommand("toggle-detail", 1);
  },

  /**
   * Give the keyboard to the pane to the left, below, above or to the right,
   * by where the panes sit on screen; past the last one, leave Orb by the
   * escalate command.
   */
  "focus-pane": (args) => {
    const direction = args[0];
    if (direction !== "left" && direction !== "right" && direction !== "up" && direction !== "down") {
      setStatus("focus-pane: left, right, up or down");
      return;
    }
    return focusPane(direction);
  },

  /**
   * Take back the last action, whatever it acted on and wherever it was
   * taken: mail, tasks (§10.3) or chat.
   */
  undo: () => undoLast(),
  redo: () => undoLast(true),

  /** Scroll the task detail, since there is no cursor to move in it. */
  "scroll-detail-down": (_a, n) => el.reader.scrollBy({ top: 60 * n }),
  "scroll-detail-up": (_a, n) => el.reader.scrollBy({ top: -60 * n }),

  /**
   * Drop the selection without acting on it; with none, put the drawer away,
   * and with neither, leave the search.
   */
  "clear-selection": () => {
    if (anySelected()) {
      clearTimelineSelection();
      renderRows();
      reportTimelineSelection();
      return;
    }
    const had = selectionSize() > 0;
    if (!had && closeDrawer()) return;
    if (!had && state.filter) {
      void runCommand("clear-filter", 1);
      return;
    }
    if (!had && state.search) {
      void runCommand("clear-search", 1);
      return;
    }
    clearSelection();
    renderRows();
    if (had) reportSelection();
  },
  quit: async () => {
    if (!(await confirmQuit())) return;
    await settleWrites();
    void call("quit_app");
  },
};

/**
 * The commands that need a designated tasks account (§1), kept as a table of
 * their own so that the set is the table itself rather than a list kept
 * beside it: a command added here is a task command, and nothing has to say
 * so twice. `undo` and `redo` are not: they take back actions of every kind.
 */
export const TASK_COMMANDS: Record<string, Command> = {
  // -------------------------------------------------------------------------
  // Tasks (§13). Every one of these is a binding, in `task_normal`,
  // `task_detail` or — for the two that act on mail — `index_normal`.
  // -------------------------------------------------------------------------

  /** Open one of the five views by name: `open-task-view upcoming`. */
  "open-task-view": async (args) => {
    const name = args[0] ?? "today";
    if (!isTaskView(name)) {
      setStatus(`open-task-view: unknown view '${name}'`);
      return;
    }
    await openTaskView(name);
    state.focus = "list";
    render();
  },
  today: () => runCommand("open-task-view today", 1),

  "open-task-detail": () => loadTaskDetail(true),

  "create-task": async () => {
    // From mail there is no task list to type the title into; Today is where
    // a new task goes by default.
    if (!inTaskView()) await openTaskView("today");
    const created = await call<TaskDetail>("create_task", {
      title: PROVISIONAL_TITLE,
      postpone: newTaskPostponeDate(),
    });
    // The row exists only so there is something to type into. Escaping out of
    // that first edit has to leave nothing behind — otherwise every abandoned
    // keystroke deposits a task called "New task", and a list you have to tidy
    // up after using is one you stop trusting. Set before the reload, which
    // reads it to keep the row on screen in a view it does not yet belong to.
    state.tasks.provisional = created.task.id;
    await refreshTasks();

    // The view stays put. Creating a task is not a request to be taken
    // somewhere else, and the old behaviour — following the task to whichever
    // view its dates put it in — meant `o` in Upcoming silently swapped the
    // list out from under the cursor before a single letter had been typed.
    const index = state.tasks.rows.findIndex((row) => row.id === created.task.id);
    if (index < 0) {
      // `reloadTasks` pins the provisional row, so this cannot happen unless the
      // task disappeared between the two calls.
      state.tasks.provisional = null;
      await call("discard_task", { id: created.task.id });
      setStatus("created, but it went missing again — that should not happen");
      return;
    }
    state.tasks.cursor = index;
    if (state.tasks.section === "mail") cancelMarkRead();
    // A task being named is a task on screen.
    if (state.tasks.view === "today" && !todayKeeps("tasks")) setTodayKept("tasks", true);
    state.tasks.section = "tasks";
    state.tasks.selected.clear();
    state.tasks.anchor = index;
    renderTaskList();
    scrollTaskCursorIntoView();
    // Straight into the title, with the notes open below it: "New task" is a
    // placeholder, and a task you have to go and rename is one you will not.
    await loadTaskDetail(false);
    beginTaskEdit("both");
  },

  /**
   * Toggle completion over the target set.
   *
   * One state for the whole set rather than each task flipped against itself:
   * if anything in it is still open, the intention is to finish it, and a mixed
   * selection inverted task by task would come back exactly as mixed as it
   * started. The same rule the mail actions decide read and flagged by.
   */
  /** Where the open task stands with the server, which the pane leaves out. */
  "task-info": () => {
    const detail = state.tasks.detail;
    if (!detail) {
      setStatus("open a task to see where it stands");
      return;
    }
    const sync = detail.task.is_dirty ? "not yet uploaded" : `uploaded to ${detail.account_name}`;
    setStatus(
      `${sync} · revision ${detail.revision} · created ${detail.created_at.slice(0, 16).replace("T", " ")} · changed ${detail.updated_at.slice(0, 16).replace("T", " ")}`,
    );
  },
  // The steps selected in the open task's checklist, as `delete-task`
  // removes them: the task itself stays as it is.
  "complete-task": () => {
    if (stepsSelectedHere()) return runCommand("toggle-step", 1);
    // Decided once, before anything moves: `withTasks` describes the action
    // *after* it has run and reloaded, so asking a second time would read the
    // state this command just produced and report the opposite of what it did.
    const reopening = targetTasksAreAllComplete();
    return withTasks(
      (n) => `${reopening ? "reopened" : "completed"} ${describeTasks(n)}`,
      (ids) =>
        call(reopening ? "uncomplete_tasks" : "complete_tasks", { ids }),
      true,
    );
  },
  "uncomplete-task": () =>
    withTasks(
      (n) => `reopened ${describeTasks(n)}`,
      (ids) => call("uncomplete_tasks", { ids }),
      true,
    ),
  // The steps selected in the open task's checklist, when there are any:
  // the task itself stays.
  "delete-task": () =>
    stepsSelectedHere()
      ? runCommand("remove-step", 1)
      : withTasks(
      (n) => `trashed ${describeTasks(n)}`,
      (ids) => call("trash_tasks", { ids }),
    ),
  /** Merge the selected tasks into one, after asking how (§29). */
  "merge-tasks": async () => {
    const ids = targetTaskIds();
    if (ids.length < 2) {
      setStatus("select the tasks to merge first");
      return;
    }
    const merged = await mergeTasksDialog(ids);
    if (!merged) return;
    state.tasks.selected.clear();
    state.tasks.anchor = null;
    await refreshTasks();
    const index = state.tasks.rows.findIndex((row) => row.id === merged.task.id);
    if (index >= 0) state.tasks.cursor = index;
    renderTaskList();
    offerUndo(`merged ${describeTasks(ids.length)} into “${merged.task.title}”`);
  },
  "restore-task": () =>
    withTasks(
      (n) => `restored ${describeTasks(n)}`,
      (ids) => call("restore_tasks", { ids }),
    ),
  "clear-dates": () =>
    withTasks(
      (n) => `cleared the dates of ${describeTasks(n)}`,
      (ids) => call("clear_task_dates", { ids }),
    ),

  "postpone-task": () => setTaskDate("postpone"),
  "set-due-date": () => setTaskDate("due"),
  "set-both-dates": () => setBothTaskDates(),

  /**
   * Make the task repeat (§8): `set-recurrence every monday`, or with no
   * words a list of the usual rules. The input is the date picker's kind of
   * language — "every 2 weeks until June", "monthly on the 1st".
   */
  "set-recurrence": async (args) => {
    const ids = targetTaskIds();
    if (ids.length === 0) {
      setStatus("no task selected");
      return;
    }
    const text = args.join(" ").trim();
    let chosen: RecurrenceSuggestion | undefined;
    if (text) {
      const readings = await call<RecurrenceSuggestion[]>("suggest_recurrence", { input: text });
      if (readings.length === 0) {
        setStatus(`"${text}" makes no rule; try "every monday", "every 2 weeks", "monthly on the 1st"`);
        return;
      }
      chosen = readings[0];
    } else {
      const usual = [
        "every day", "every weekday", "every week", "every 2 weeks", "every month",
        "monthly on the 1st", "monthly on the last day", "every 3 months", "every year",
      ];
      const readings = (
        await Promise.all(
          usual.map((input) => call<RecurrenceSuggestion[]>("suggest_recurrence", { input })),
        )
      ).flat();
      const picked = await openPicker({
        title: "Repeat",
        placeholder: "every monday, monthly on the 15th, every 2 weeks until June…",
        items: readings.map((r) => ({ value: r.rrule, label: r.label, detail: r.rrule })),
      });
      if (!picked) return;
      chosen = readings.find((r) => r.rrule === picked);
    }
    if (!chosen) return;
    const rule = chosen;
    await withTasks(
      (n) => `${describeTasks(n)} · repeats ${rule.label.toLowerCase()}`,
      (ids) => call("update_tasks", { ids, edit: { recurrence: rule.rrule } }),
    );
  },
  /** Stop the task repeating; what it is now stays as it is. */
  "clear-recurrence": () =>
    withTasks(
      (n) => `${describeTasks(n)} · no longer repeats`,
      (ids) => call("update_tasks", { ids, edit: { clear_recurrence: true } }),
    ),
  /** Advance a repeating task from the day it was completed rather than its date (§8). */
  "recurrence-from-completion": (args) => {
    const on = args[0] !== "off";
    return withTasks(
      (n) => `${describeTasks(n)} · advances from ${on ? "completion" : "the scheduled date"}`,
      (ids) => call("update_tasks", { ids, edit: { recurrence_from_completion: on } }),
    );
  },
  /** Attach files picked from a dialog to the task (§9). */
  /** Add a step at the end of the task's checklist (§17); without a title, write it in place. */
  "add-step": async (args) => {
    const title = args.join(" ").trim();
    if (!title) {
      if (!state.tasks.detail) await loadTaskDetail(false);
      beginStepEdit("new");
      return;
    }
    if (await editStep(() => ({ op: "add", title }), () => true)) setStatus(`step added: ${title}`);
  },
  /** Tick step n, or the selected steps, or the next open one; untick what is done. */
  "toggle-step": async (args) => {
    const selected = args[0] === undefined && state.tasks.detail ? selectedSteps(state.tasks.detail) : [];
    if (selected.length > 1) {
      // One way for all of them, as completing a selection of tasks does,
      // and one edit, which one undo takes back.
      const detail = state.tasks.detail!;
      const done = detail.steps.some((s) => selected.includes(s.id) && !s.done);
      if (await editStep(() => ({ op: "done", ids: selected, done }), () => true)) {
        setStatus(`${done ? "ticked" : "unticked"} ${selected.length} steps`);
      }
      return;
    }
    await editStep(
      (detail) => {
        const step = stepAt(detail, args[0])
          ?? detail.steps.find((s) => s.id === selected[0])
          ?? detail.steps.find((s) => !s.done);
        return step ? { op: "done", ids: [step.id], done: !step.done } : null;
      },
      () => setStatus("no open step"),
    );
  },
  /** Write step n, or the selected step, in place. */
  "edit-step": async (args) => {
    if (!state.tasks.detail) await loadTaskDetail(false);
    const detail = state.tasks.detail;
    const step = detail && (stepAt(detail, args[0]) ?? detail.steps.find((s) => s.id === selectedSteps(detail)[0]));
    if (!step) {
      setStatus("edit-step <n>, or select a step first");
      return;
    }
    beginStepEdit(step.id);
  },
  /** Rename step n. */
  "rename-step": async (args) => {
    await editStep((detail) => {
      const step = stepAt(detail, args[0]);
      const title = args.slice(1).join(" ").trim();
      return step && title ? { op: "rename", id: step.id, title } : null;
    }, () => setStatus("rename-step <n> <title>"));
  },
  /** Take step n out of the checklist, or the selected steps. */
  "remove-step": async (args) => {
    await editStep((detail) => {
      const ids = args[0] === undefined ? stepsToRemove(detail, null) : [stepAt(detail, args[0])?.id].filter((id): id is string => id !== undefined);
      return ids.length ? { op: "remove", ids } : null;
    }, () => setStatus("remove-step <n>, or select steps first"));
  },
  "attach-file": async () => {
    const task = state.tasks.detail?.task ?? cursorTask();
    if (!task) {
      setStatus("no task selected");
      return;
    }
    const paths = await pickFiles({ kind: "files", title: "Attach files" });
    if (paths.length === 0) return;
    await attachFilesToTask(task.id, paths);
  },
  /** List Today, Upcoming and Someday under their tags, or not: `on`, `off`, or the other one (§30). */
  "toggle-group-by-tag": async (args) => {
    const choice = args[0];
    if (choice !== undefined && choice !== "on" && choice !== "off") {
      setStatus("toggle-group-by-tag: on or off");
      return;
    }
    const on = choice === undefined ? !groupByTagOn() : choice === "on";
    await setGroupByTag(on);
    setStatus(on ? "tasks grouped by tag" : "tasks no longer grouped by tag");
  },
  /** List Today's tasks due by today above the rest, or not: `on`, `off`, or the other one (§31). */
  "toggle-due-first": async (args) => {
    const choice = args[0];
    if (choice !== undefined && choice !== "on" && choice !== "off") {
      setStatus("toggle-due-first: on or off");
      return;
    }
    const on = choice === undefined ? !dueFirstOn() : choice === "on";
    await setDueFirst(on);
    setStatus(on ? "the tasks due by today come first" : "the due tasks no longer come first");
  },
  /** Give a tag another name; every task carrying it follows (§7). */
  "rename-tag": async (args) => {
    const [from, ...rest] = args;
    const to = rest.join(" ").trim();
    const tag = from ? findTag(from) : undefined;
    if (!tag || !to) {
      setStatus("rename-tag <old> <new>");
      return;
    }
    await call("update_task_tag", { id: tag.id, name: to, colour: tag.colour });
    state.tasks.tags = await call<TaskTagInfo[]>("list_task_tags");
    await refreshTasks();
    offerUndo(`renamed tag ${tag.name} to ${to}`);
  },
  /** Give a tag a colour, as a CSS colour, or `none` for the default. */
  "recolour-tag": async (args) => {
    const [name, colour] = args;
    const tag = name ? findTag(name) : undefined;
    if (!tag || !colour) {
      setStatus("recolour-tag <name> <colour>, e.g. recolour-tag urgent #e06060");
      return;
    }
    await call("update_task_tag", {
      id: tag.id,
      name: tag.name,
      colour: colour === "none" ? null : colour,
    });
    state.tasks.tags = await call<TaskTagInfo[]>("list_task_tags");
    await refreshTasks();
    offerUndo(`${tag.name} is now ${colour}`);
  },
  /** Delete a tag from every task; the tasks stay (§7). Asks first. */
  "delete-tag": async (args) => {
    const name = args.join(" ").trim();
    const tag = name ? findTag(name) : undefined;
    if (!tag) {
      setStatus(name ? `no tag named '${name}'` : "delete-tag <name>");
      return;
    }
    if (!(await askFirst(`Delete the tag ${tag.name} from every task?\n\nThe tasks stay.`, "Delete Tag"))) {
      return;
    }
    const affected = await call<number>("delete_task_tag", { id: tag.id });
    state.tasks.tags = await call<TaskTagInfo[]>("list_task_tags");
    await refreshTasks();
    offerUndo(`deleted tag ${tag.name} from ${describeTasks(affected)}`);
  },

  /** Empty the trash for good (§10.1). Not undoable, so it asks first. */
  "empty-task-trash": async () => {
    if (!(await askFirst("Empty the task trash?\n\nEvery task in it is deleted for good.", "Empty Trash"))) {
      return;
    }
    const purged = await call<number>("empty_task_trash");
    await refreshTasks();
    setStatus(`expunged ${describeTasks(purged)}`);
  },

  /** Open System Settings at Orb's notifications, to grant a refused permission. */
  "notification-settings": () => call("open_notification_settings"),

  /** Add a reminder to the task under the cursor (§5.3). */
  "add-reminder": async () => {
    const task = state.tasks.detail?.task ?? cursorTask();
    if (!task) {
      setStatus("no task selected");
      return;
    }
    const chosen = await pickDate("reminder", "Remind me", task.due_date ?? task.postpone_date);
    if (!chosen || chosen.kind === "clear" || !chosen.at) return;
    await call("add_task_reminder", { task: task.id, at: chosen.at });
    await refreshTasks();
    offerUndo(`reminder set for ${chosen.date}${chosen.time ? ` ${chosen.time}` : ""}`);
  },

  /**
   * Open the task for writing.
   *
   * With no argument that is the title *and* the notes: a task's name and what
   * it is actually about get written in one sitting, and behind two separate
   * commands the notes were mostly never written at all. `edit-task title` and
   * `edit-task body` still open one field each, for a binding that wants to go
   * straight to one of them.
   */
  "edit-task": async (args) => {
    if (!state.tasks.detail) await loadTaskDetail(false);
    const which = args[0];
    beginTaskEdit(which === "body" || which === "title" ? which : "both");
  },
  "accept-edit": () => acceptTaskEdit(),
  "cancel-edit": () => cancelTaskEdit(),
  "next-edit-field": () => moveTaskEditField(1),
  "prev-edit-field": () => moveTaskEditField(-1),

  /**
   * Add a tag by name, creating it if the registry has never seen it.
   *
   * With no argument the command line opens pre-filled, which is where a name
   * gets typed in this application — a modal of its own would be a second way
   * to type one word. What the command line cannot do on its own is say which
   * tags exist, so [`offerTags`] puts them where they can be read while typing.
   */
  "add-tag": async (args) => {
    let name = args.join(" ").trim();
    if (!name) {
      // The existing tags to pick from, fuzzily, so that `urgent` and
      // `Urgent` cannot both come to exist; a name none of them has is
      // offered as a new tag, even while it finds some of them.
      const picked = await openPicker({
        title: "Add tag",
        placeholder: "tag",
        items: state.tasks.tags.map((tag) => ({ value: tag.name, label: tag.name })),
        typed: (text) => `New tag “${text}”`,
      });
      if (!picked) return;
      name = picked.trim();
      if (!name) return;
    }
    const tag = state.tasks.tags.find((t) => t.name.toLowerCase() === name.toLowerCase());
    const id = tag ? tag.id : (await call<TaskTagInfo>("create_task_tag", { name, colour: null })).id;
    await withTasks(
      (n) => `tagged ${describeTasks(n)} ${name}`,
      (ids) => call("add_task_tag", { ids, tag: id }),
    );
    state.tasks.tags = await call<TaskTagInfo[]>("list_task_tags");
  },

  "remove-tag": async (args) => {
    const name = args.join(" ").trim();
    if (!name) {
      promptCommand("remove-tag ");
      offerTags("none of these are on it");
      return;
    }
    const tag = state.tasks.tags.find((t) => t.name.toLowerCase() === name.toLowerCase());
    if (!tag) {
      setStatus(`no tag named '${name}'`);
      return;
    }
    await withTasks(
      (n) => `untagged ${describeTasks(n)}`,
      (ids) => call("remove_task_tag", { ids, tag: tag.id }),
    );
  },

  /** Link the message in the reader to the task under the cursor (§6.1). */
  "link-message": async () => {
    const task = state.tasks.detail?.task ?? cursorTask();
    if (!task) {
      setStatus("no task selected");
      return;
    }
    if (!state.detail) {
      setStatus("open a message first — link-message links the one being read");
      return;
    }
    await call("link_message_to_task", { task: task.id, message: state.detail.id });
    await refreshTasks();
    offerUndo(`linked "${state.detail.subject ?? "(no subject)"}"`);
  },

  /** Follow the task's link back to the message (§6.2). */
  /** The mailbox's side of a link followed in the tasks window. */
  "open-task-link": async (args) => {
    const [task, messageId] = args;
    if (!task || !messageId) {
      setStatus("open-task-link <task> <message-id>");
      return;
    }
    await openLinkedMessage(Number(task), messageId);
  },
  "open-linked-message": async () => {
    const detail = state.tasks.detail;
    const task = detail?.task ?? cursorTask();
    if (!task) {
      setStatus("no task selected");
      return;
    }
    const links = detail?.task.id === task.id ? detail.links : [];
    if (links.length === 0) {
      setStatus(
        task.linked_email_count > 0
          ? "open the task first — its links are listed there"
          : "this task has no linked message",
      );
      return;
    }
    await openLinkedMessage(task.id, links[0].message_id);
  },

  "move-task-up": (_a, n) => moveTaskBy(-n),
  "move-task-down": (_a, n) => moveTaskBy(n),

  /** Toggle the task under the cursor and advance, as `<space>` does in mail. */
  "select-task": (_a, n) => {
    if (state.tasks.selected.has(state.tasks.cursor)) {
      state.tasks.selected.delete(state.tasks.cursor);
    } else {
      state.tasks.selected.add(state.tasks.cursor);
    }
    state.tasks.anchor = state.tasks.cursor;
    move(n);
    setStatus(
      state.tasks.selected.size === 0
        ? "selection cleared"
        : `${describeTasks(state.tasks.selected.size)} selected`,
    );
  },

  /**
   * Drop the task selection, and the search with it.
   *
   * `<esc>` means "back out of what I set up", and in a task view a filter is
   * as much a thing to back out of as a selection (§4.5).
   */
  "clear-task-selection": () => {
    const had = state.tasks.selected.size > 0;
    if (!had && closeDrawer()) return;
    if (!had && state.filter) {
      void runCommand("clear-filter", 1);
      return;
    }
    if (!had && state.tasks.view === "search") {
      void runCommand("clear-search", 1);
      return;
    }
    state.tasks.selected.clear();
    state.tasks.anchor = null;
    if (state.tasks.stepSelection.size > 0) {
      state.tasks.stepSelection.clear();
      renderReader();
    }
    paintTaskCursor();
    if (had) setStatus("selection cleared");
  },

  /** Sync the tasks folder on its own (§3). */
  "sync-tasks": async () => {
    const result = await runTask({ id: "sync-tasks", label: "Syncing tasks" }, () =>
      call<TaskSyncResult>("sync_tasks"),
    );
    await refreshTasks();
    const unreadable = result.unreadable ? ` · ${result.unreadable} unreadable` : "";
    const reset = result.uidvalidity_reset ? " · uidvalidity reset" : "";
    setStatus(
      `tasks: ${result.downloaded} in · ${result.uploaded} out · ` +
        `${result.deleted} removed${unreadable}${reset}`,
    );
  },

  /** One task per selected message, each linked to it (§6.1). */
  "create-task-from-message": async (args) => {
    const postpone = args.includes("postpone") ? " postpone" : "";
    if (!args.includes("edit") && (await actAcrossKinds(`create-task-from-message${postpone}`, `chat-create-task${postpone}`, true))) return;
    await createTasksFromMessages({ edit: args.includes("edit"), ask: args.includes("postpone") });
  },

  /** Show what already refers to the message being read (§6.3). */
  "open-tasks-for-message": async () => {
    // A conversation: what any of its messages is linked to.
    const conversation = shownConversation();
    if (conversation) {
      const linked = conversation.linked;
      if (linked.length === 0) {
        setStatus("no task refers to this conversation");
        return;
      }
      if (!(await openTaskById(linked[0].id))) {
        setStatus(`${describeTasks(linked.length)} refer to this conversation, none in a view`);
      }
      return;
    }
    // The message being read if there is one, else the one under the cursor —
    // so the binding means the same thing in the list and in the detail view.
    const id = state.detail?.id ?? (await ensureCursorRow())?.id;
    if (id === undefined) {
      setStatus("no message selected");
      return;
    }
    const tasks = await call<TaskSummary[]>("tasks_for_message", { message: id });
    if (tasks.length === 0) {
      setStatus("no task refers to this message");
      return;
    }
    if (!(await openTaskById(tasks[0].id))) {
      setStatus(`${describeTasks(tasks.length)} refer to this message, none in a view`);
    }
  },
};

export const COMMANDS: Record<string, Command> = { ...MAIL_COMMANDS, ...TASK_COMMANDS, ...CHAT_COMMANDS };

function sayTodayLayout(): void {
  const layout = todayLayout();
  setStatus(
    layout === "mixed"
      ? "Today mixed · mail, chat and reading on one timeline, tasks right"
      : layout === "columns"
        ? "Today in columns · mail left, tasks right"
        : "Today stacked · mail above tasks",
  );
}

interface TaskImportPlan {
  snapshot: string;
  exported_at: string;
  new: number;
  newer: number;
  older: number;
  purged: string[];
  tags: number;
  problems: string[];
  applied: boolean;
}

interface ThingsImportPlan {
  database: string;
  tasks: number;
  completed: number;
  repeating: number;
  unmapped: string[];
  already: number;
  trashed: number;
  projects: number;
  headings: number;
  repeat_instances: number;
  new_tags: string[];
  updated: number;
  edited_here: string[];
  repaired: number;
  trashed_here: number;
  applied: boolean;
}

type StepEdit =
  | { op: "add"; title: string }
  | { op: "done"; ids: string[]; done: boolean }
  | { op: "rename"; id: string; title: string }
  | { op: "remove"; ids: string[] };
/** Extend the selection of whatever list the cursor is in: tasks, the timeline or the messages. */
function extendAnySelection(delta: number): void {
  if (inTaskSection()) extendTaskSelection(delta);
  else if (todayMixed()) extendOnTimeline(delta);
  else extendSelection(delta);
}
/** The task list's shift-arrow: the anchor stays, the far end follows the cursor. */
function extendTaskSelection(delta: number): void {
  if (state.tasks.rows.length === 0) return;
  state.tasks.anchor ??= state.tasks.cursor;
  state.tasks.cursor = Math.min(Math.max(state.tasks.cursor + delta, 0), state.tasks.rows.length - 1);
  selectTaskRange(state.tasks.cursor);
  paintTaskCursor();
  scrollTaskCursorIntoView();
  setStatus(`${describeTasks(state.tasks.selected.size)} selected`);
}
function selectAllTasks(): void {
  state.tasks.selected.clear();
  state.tasks.rows.forEach((_, index) => state.tasks.selected.add(index));
  state.tasks.anchor = 0;
  paintTaskCursor();
  setStatus(`${describeTasks(state.tasks.selected.size)} selected`);
}
/** Whether steps are selected in the checklist of the task the cursor is on. */
function stepsSelectedHere(): boolean {
  const detail = state.tasks.detail;
  if (!detail || selectedSteps(detail).length === 0) return false;
  return state.tasks.selected.size === 0 && cursorTask()?.id === detail.task.id;
}
/** Step `n` of a task's checklist, counted from 1. */
function stepAt(detail: TaskDetail, n: string | undefined): TaskStepInfo | undefined {
  const at = Number(n);
  return Number.isInteger(at) && at >= 1 ? detail.steps[at - 1] : undefined;
}
/**
 * Change the checklist of the task in the detail pane, or under the cursor
 * (§17). `edit` names the change from the task as it is; `refused` speaks
 * when it names none. Returns whether a change was made.
 */
async function editStep(
  edit: (detail: TaskDetail) => StepEdit | null,
  refused: () => unknown,
): Promise<boolean> {
  if (!state.tasks.detail) await loadTaskDetail(false);
  const detail = state.tasks.detail;
  if (!detail) {
    setStatus("no task selected");
    return false;
  }
  const change = edit(detail);
  if (!change) {
    refused();
    return false;
  }
  state.tasks.detail = await call<TaskDetail>("edit_task_step", { id: detail.task.id, edit: change });
  await refreshTasks();
  return true;
}
// Closing the mailbox window hides it, and the Dock brings it back as it
// was; the shell does the hiding. Without this the frontend's own close
// handling would destroy the window instead. Quitting asks, through `quit`.
// The tasks window runs this page too, and simply closes.
if (WINDOW === "main") {
  void getCurrentWindow().onCloseRequested((event) => {
    event.preventDefault();
  });
}
/**
 * Ask before quitting on top of work that would be lost.
 *
 * Returns whether to go ahead. Silent when nothing is at stake, which is the
 * common case — a sync or a backfill in flight is not a reason to be stopped
 * at the door.
 */
async function confirmQuit(): Promise<boolean> {
  const blockers = quitBlockers();
  if (blockers.length === 0) return true;
  const reasons = blockers.map((b) => `· ${b.reason}`).join("\n");
  return askFirst(`Quit anyway?\n\n${reasons}`, "Quit");
}
/**
 * Run a command string, which may be a `&&` chain such as
 * `stage-read && mark-move Archive && next-message`.
 *
 * Steps run in order and stop at the first unknown or failing one, so a chain
 * never half-applies an action and then advances the cursor as if it had
 * succeeded.
 */
/** The messages an action names: the subject of one, the count of several. */
function describeTargets(rows: (MessageHeader | undefined)[], count: number): string {
  const subject = count === 1 ? rows[0]?.subject : null;
  return subject ? `“${subject}”` : describeCount(count);
}
/** How deep the user's own commands may name one another before one is taken to name itself. */
const MAX_NESTING = 8;
/**
 * The steps of a spec with every command of the user's own replaced by what
 * it runs, so that a key bound to one is a chain like any other: one undo,
 * stopping at the first step that fails. Null, said in the status line, for
 * one that runs itself.
 */
export function expandSpec(spec: string, depth = 0): string[] | null {
  const steps: string[] = [];
  for (const step of spec.split("&&").map((s) => s.trim()).filter(Boolean)) {
    const name = step.split(/\s+/)[0]!;
    const own = COMMANDS[name] ? undefined : state.userCommands[name];
    if (!own) {
      steps.push(step);
      continue;
    }
    if (depth >= MAX_NESTING) {
      setStatus(`${name}: runs itself, through bindings.commands`);
      return null;
    }
    const inner = expandSpec(own.run, depth + 1);
    if (!inner) return null;
    steps.push(...inner);
  }
  return steps;
}
export async function runCommand(spec: string, count: number): Promise<void> {
  const steps = expandSpec(spec);
  if (!steps) return;
  if (steps.length < 2) return runSteps(steps, count);
  // A chain is one action: what its steps offer to undo becomes one undo,
  // announced on one line.
  const endGroup = await groupUndo();
  try {
    await runSteps(steps, count);
  } finally {
    await endGroup();
  }
}
async function runSteps(steps: string[], count: number): Promise<void> {
  for (const step of steps) {
    const [name, ...args] = step.split(/\s+/);
    const command = COMMANDS[name];
    if (!command) {
      setStatus(`not implemented: ${name}`);
      return;
    }
    // A task command with no tasks configured says so once, here, rather than
    // failing in the core and reporting a configuration error per keystroke. The
    // keys stay bound on purpose: `t` on a message is a reasonable thing to try,
    // and being told what is missing is more use than being told nothing.
    if (!state.tasks.available && name in TASK_COMMANDS) {
      setStatus("tasks need a tasks section in config.yml: an account and a folder");
      return;
    }
    if (!state.chat.available && name in CHAT_COMMANDS) {
      setStatus("chat needs a chat section in config.yml: the server");
      return;
    }
    // The chat window has no mail, no tasks and no reading to go to.
    if (WINDOW === "chat" && COMMAND_DOCS[name]?.chatWindow === false) {
      setStatus(`${name} is for the mailbox; this window shows chat`);
      return;
    }
    // While chat has the cursor, a key that acts on mail would act on the
    // mail list hidden behind it.
    if (chatActive() && COMMAND_DOCS[name]?.on === "mail") {
      setStatus(`${name} acts on mail; in chat, r replies and a marks a thread done`);
      return;
    }
    // Only the outermost command: an action across kinds runs others inside it.
    const restore = holding ? null : holdTimelineCursor();
    holding ||= restore !== null;
    try {
      await command(args, count);
      await restore?.();
    } catch (e) {
      // A failed core call has already put its reason in the status line and
      // rethrows the raw IPC error. An `Error` is ours — a refused duplicate
      // task, say — and would otherwise vanish without a word.
      if (e instanceof Error && !(e instanceof Cancelled)) setStatus(e.message);
      return;
    } finally {
      if (restore) holding = false;
    }
  }
}
/** Set while a command holds the timeline's cursor, for the commands it runs. */
let holding = false;
/**
 * Commands that run for a long while without changing what the list holds or
 * where the cursor is: started in their turn, but not waited for.
 */
const BACKGROUND = new Set([
  "sync",
  "sync-all",
  "sync-folder",
  "sync-tasks",
  "fetch-bodies",
  "backfill-status",
  "sync-stats",
  "account-status",
  "console",
  "open-attachment",
  "save-attachment",
  "quick-look",
  "compose",
  "reply",
  "reply-all",
  "forward",
  "edit-draft",
  "open-link",
]);
/**
 * How long one command may hold up the ones typed after it. Only a command
 * that never settles reaches this; it keeps running, and what follows is no
 * longer held up by it.
 */
const QUEUE_PATIENCE_MS = 15_000;
let queue: Promise<void> = Promise.resolve();
/**
 * Run a command the user asked for — a key, the command line, a menu entry —
 * after every one asked for before it.
 *
 * Without this, a command still waiting on the core when the next key arrives
 * runs alongside it: `a` bound to `stage-read && mark-move Archive &&
 * next-message`, followed quickly by `j`, staged the message *below* the one
 * `a` was pressed on, because the cursor had moved by the time the second step
 * looked at it. In order, every command sees the list exactly as the ones
 * before it left it, which is what typing ahead means.
 *
 * A command a command runs itself calls `runCommand`, never this: it is
 * already inside its turn, and waiting for the queue would wait for itself.
 */
export function dispatchCommand(spec: string, count: number): Promise<void> {
  const turn = queue.then(() => {
    const steps = (expandSpec(spec) ?? []).map((s) => s.split(/\s+/)[0] ?? "");
    const running = runCommand(spec, count);
    if (steps.every((name) => BACKGROUND.has(name))) return;
    let patience: ReturnType<typeof setTimeout> | undefined;
    return Promise.race([
      running,
      new Promise<void>((resolve) => {
        patience = setTimeout(resolve, QUEUE_PATIENCE_MS);
      }),
    ]).finally(() => clearTimeout(patience));
  });
  queue = turn.catch(() => {});
  return turn;
}
/** Where the keyboard is: which of the layered binding tables are in force. */
export type KeyContext = "mail" | "reader" | "tasks" | "task-detail" | "chat";
export function activeContext(): KeyContext {
  if (inTaskSection()) return state.focus === "reader" ? "task-detail" : "tasks";
  if (inChatSection()) return "chat";
  return state.focus === "reader" ? "reader" : "mail";
}
/**
 * Bindings for the focused pane.
 *
 * `index_detail` is documented in config.yml as inheriting from
 * `index_normal`, so it layers on top rather than replacing it.
 */
export function activeBindings(): Record<string, string> {
  const b = state.bindings;
  // The task modes layer on top of `index_normal` rather than replacing it, so
  // navigation, search, sync and quit keep working in a task view without every
  // one of them having to be restated in `task_normal`. A task binding wins
  // every key it names, which is how `d` means delete-task there and
  // mark-delete everywhere else. Chat layers the same way, in the list and in
  // the conversation alike: the reader follows the thread under the cursor.
  switch (activeContext()) {
    case "task-detail":
      return { ...b.index_normal, ...b.task_normal, ...b.task_detail };
    case "tasks":
      return { ...b.index_normal, ...b.task_normal };
    case "chat":
      return { ...b.index_normal, ...b.chat_normal };
    case "reader":
      return { ...b.index_normal, ...b.index_detail };
    case "mail":
      return b.index_normal;
  }
}
/** What `open-pinned n` opens, for the key hints: the pin's name, or nothing when there is no such pin. */
function describePin(command: string): string | null | undefined {
  const n = /^open-pinned\s+(\d+)$/.exec(command.trim())?.[1];
  if (!n) return undefined;
  const label = pinLabel(Number(n));
  return label === null ? null : `Open ${label}`;
}
export const vim = new Vim(activeBindings, dispatchCommand, (sequence, table) => {
  showKeyHints(sequence, table, describePin);
  showPinKeys(sequence, table);
});
