import { canonicalTable } from "./keys";
import { refreshOutbox } from "./outbox";
import { setDatePickerBindings } from "./datepicker";
import { installPasswordPrompt } from "./password";
import { TaskTagInfo } from "./tasklist";
import { openFirstFolder } from "./actions";
import { applyAppearance, applyConfiguredWidths, applyWidths, forgetLook } from "./appearance";
import { followTitleBar } from "./chrome";
import { autoBackfill } from "./backfill";
import { BindingsConfig, DEFAULT_BINDINGS, DEFAULT_CHAT_BINDINGS, DEFAULT_CHAT_COMPOSE_BINDINGS, DEFAULT_TASK_BINDINGS, DEFAULT_TASK_DETAIL_BINDINGS, DEFAULT_TASK_EDIT_BINDINGS } from "./bindings";
import { initChat, openChat } from "./chat";
import { call, setStatus } from "./bridge";
import { COMMANDS } from "./commands";
import { fitTodayRows, render, renderList } from "./list";
import { renderReader } from "./reader";
import { AccountHealth, AccountInfo, FolderInfo, NotificationPermission, PinInfo, SortKey, VirtualFolderInfo } from "./model";
import { folderEntry, inTaskView, loadHighlightStyles, refreshHighlightBadges, refreshPendingCounts, renderFolders, taskEntryIndex } from "./sidebar";
import { initPaneSplits, initTodaySplits } from "./splitters";
import { refreshExportStatus, refreshTaskSync } from "./statusline";
import { ITEM, WINDOW, state } from "./state";
import { bootItemWindow } from "./itemwindow";
import { syncAll } from "./sync";
import { openTaskView, refreshTaskBadge, watchTheDay } from "./taskview";
import "@fortawesome/fontawesome-free/css/all.min.css";
import "./styles.css";
import "./format";
import "./events";
import "./taskedit";
import "./taskpointer";
import "./selection";
import "./moving";
import "./panels";
import "./targets";
import "./search";
import "./composing";
import "./taskbar";
import "./commandline";
import "./stepsui";

/**
 * Load the user's key bindings.
 *
 * The config is merged over the built-in tables, and the summary says how
 * many keys came from where so an empty config is not mistaken for the
 * bindings silently not applying.
 */
async function loadBindings(): Promise<string> {
  const config = await call<BindingsConfig>("bindings");
  const normal = canonicalTable(config.index_normal);
  const detail = canonicalTable(config.index_detail);

  // The task modes get the same treatment: built-ins under the config, so a
  // config written before a command existed lacks a key for it rather than
  // silently disabling it.
  state.bindings.task_normal = { ...DEFAULT_TASK_BINDINGS, ...canonicalTable(config.task_normal) };
  state.bindings.task_detail = {
    ...DEFAULT_TASK_DETAIL_BINDINGS,
    ...canonicalTable(config.task_detail),
  };
  state.bindings.task_edit = {
    ...DEFAULT_TASK_EDIT_BINDINGS,
    ...canonicalTable(config.task_edit),
  };
  state.bindings.chat_normal = { ...DEFAULT_CHAT_BINDINGS, ...canonicalTable(config.chat_normal) };
  state.bindings.chat_compose = { ...DEFAULT_CHAT_COMPOSE_BINDINGS, ...canonicalTable(config.chat_compose) };
  // Published rather than left for the picker to fetch, so there is one answer
  // to what `<c-d>` is bound to instead of two tables read at different times.
  setDatePickerBindings(config.date_picker);

  const fromConfig = Object.keys(normal).length > 0;
  // Merged, not replaced: a config written before a command existed would
  // otherwise silently lack any key for it, and the feature would look broken
  // rather than unbound. The config still wins every key it names, and `noop`
  // is how a default is deliberately turned off.
  state.bindings.index_normal = { ...DEFAULT_BINDINGS, ...normal };
  state.bindings.index_detail = detail;

  // A built-in name wins: a command of one's own cannot change what `archive` does.
  state.userCommands = Object.fromEntries(
    Object.entries(config.commands ?? {}).filter(([name]) => !COMMANDS[name]),
  );
  const unknown = new Set<string>();
  const declared = [
    ...Object.values(state.userCommands).map((c) => c.run),
    ...Object.values(state.bindings.index_normal),
    ...Object.values(state.bindings.index_detail),
    ...Object.values(state.bindings.task_normal),
    ...Object.values(state.bindings.task_detail),
    ...Object.values(state.bindings.task_edit),
    ...Object.values(state.bindings.chat_normal),
    ...Object.values(state.bindings.chat_compose),
  ];
  for (const spec of declared) {
    for (const step of spec.split("&&")) {
      const name = step.trim().split(/\s+/)[0];
      if (name && !COMMANDS[name] && !state.userCommands[name]) unknown.add(name);
    }
  }

  const added = Object.keys(DEFAULT_BINDINGS).filter((k) => !(k in normal)).length;
  const source = fromConfig
    ? `${Object.keys(normal).length} bindings from config.yml${added ? ` + ${added} built-in` : ""}`
    : "built-in bindings (config.yml defines none)";
  return unknown.size
    ? `${source} · unimplemented: ${[...unknown].sort().join(", ")}`
    : source;
}
/**
 * Take from the config what this window keeps: the bindings, the reader
 * settings, the accounts and the folder list. Run at boot and again on every
 * reload of the file. Returns the binding summary for the status line.
 */
/** `search.sort` as the list sorts: null for relevance. */
function searchSortFrom(name: string | undefined): { key: SortKey; dir: 1 | -1 } | null {
  const [field, way] = (name ?? "").split("_");
  const key = ({ date: "date", subject: "subject", sender: "from" } as const)[field as "date" | "subject" | "sender"];
  if (!key) return null;
  return { key, dir: way === "oldest" || way === "asc" ? 1 : -1 };
}
/** The look the config named when it was last read; null before the first read. */
let configuredLook: string | null = null;
export async function applyConfig(): Promise<string> {
  const bindingSummary = await loadBindings();
  const settings = await call<{
    mark_read_after_ms: number;
    default_message_view: string;
    columns?: string[];
    date_format?: string;
    search_sort?: string;
    density?: string;
    from_width?: number | null;
    date_width?: number | null;
    theme?: string;
    font_size?: number;
    dark_html_mail?: boolean;
    look?: string;
    completed_first_in_today?: boolean;
  }>("ui_settings");
  state.settings.markReadAfterMs = settings.mark_read_after_ms;
  state.settings.defaultView =
    settings.default_message_view === "text" ? "text" : "html";
  state.settings.columns = settings.columns ?? state.settings.columns;
  state.settings.dateFormat = settings.date_format ?? state.settings.dateFormat;
  state.settings.searchSort = searchSortFrom(settings.search_sort);
  state.settings.density = settings.density ?? state.settings.density;
  state.settings.theme = settings.theme ?? state.settings.theme;
  state.settings.fontSize = settings.font_size ?? state.settings.fontSize;
  state.settings.darkHtmlMail = settings.dark_html_mail ?? state.settings.darkHtmlMail;
  state.settings.completedFirst = settings.completed_first_in_today ?? false;
  // A look written into the config is meant to be seen: it outranks what the
  // machine chose from the menu before.
  const look = settings.look === "terminal" ? "terminal" : "hybrid";
  if (configuredLook !== null && look !== configuredLook) forgetLook();
  configuredLook = look;
  state.settings.look = look;
  state.darkMail = state.settings.darkHtmlMail;
  applyAppearance();
  followTitleBar();
  applyConfiguredWidths(settings.from_width ?? null, settings.date_width ?? null);
  state.accounts = await call<AccountInfo[]>("accounts");
  state.folders = await call<FolderInfo[]>("folders");
  state.virtualFolders =
    (await call<VirtualFolderInfo[]>("virtual_folders").catch(() => [] as VirtualFolderInfo[])) ??
    [];
  state.pins = (await call<PinInfo[]>("pins").catch(() => [] as PinInfo[])) ?? [];
  await loadHighlightStyles();
  // The sidebar's entries may have shifted — a view added or removed — and
  // the open folder's own `threads` may have changed: the cursor follows the
  // open folder, and the folder is read again as its config now says.
  // Booting with tasks, the cursor is on Today from the first drawing, which
  // is where the boot lands.
  if (state.booting && state.tasks.available) state.sidebarCursor = taskEntryIndex("today");
  else if (!inTaskView()) state.sidebarCursor = folderEntry(state.openFolder);
  state.threadsFolder = "";
  render();
  return bindingSummary;
}
/** Ask again whether notifications may be posted, and redraw if that changed. */
async function refreshNotificationPermission(): Promise<void> {
  const permission = await call<NotificationPermission>("notification_permission").catch(() => null);
  if (!permission || permission === state.notificationPermission) return;
  state.notificationPermission = permission;
  renderReader();
}
/**
 * The tasks window: the mailbox's page with the mail left out. No sync, no
 * backfill, no password prompts — the mailbox does those, and the tasks
 * here follow its changes through `tasks-changed`.
 */
async function bootTasksWindow(): Promise<void> {
  const bindingSummary = await applyConfig();
  await refreshNotificationPermission();
  window.addEventListener("focus", () => void refreshNotificationPermission());
  if (state.tasks.available) {
    state.tasks.tags = await call<TaskTagInfo[]>("list_task_tags").catch(() => []);
    await refreshTaskBadge().catch(() => {});
    await refreshTaskSync();
    await openTaskView("today").catch(() => {});
  }
  state.focus = "list";
  render();
  watchTheDay();
  setStatus(state.tasks.available ? bindingSummary : "config.yml has no tasks account");
}
/**
 * The chat window (PLAN.md 15.9): Waiting and the rooms, for a second
 * screen. The core syncs chat for every window, and its events reach both.
 */
async function bootChatWindow(): Promise<void> {
  const bindingSummary = await applyConfig();
  if (state.tasks.available) {
    state.tasks.tags = await call<TaskTagInfo[]>("list_task_tags").catch(() => []);
  }
  await initChat();
  if (state.chat.available) await openChat();
  state.focus = "list";
  render();
  watchTheDay();
  setStatus(state.chat.available ? bindingSummary : "config.yml has no chat section");
}
/**
 * A window of one message or task: the config and the tasks, and then the
 * item; the mailbox does the syncing, as for the tasks window.
 */
async function bootOneItem(): Promise<void> {
  const bindingSummary = await applyConfig();
  state.view = state.settings.defaultView;
  await refreshNotificationPermission();
  window.addEventListener("focus", () => void refreshNotificationPermission());
  if (state.tasks.available) {
    state.tasks.tags = await call<TaskTagInfo[]>("list_task_tags").catch(() => []);
  }
  await bootItemWindow();
  render();
  watchTheDay();
  if (state.detail || state.tasks.detail) setStatus(bindingSummary);
}
async function boot(): Promise<void> {
  // Anything that opens a folder or a view moves this on; see the landing
  // below.
  const startedAt = state.generation;
  applyWidths();
  initPaneSplits();
  initTodaySplits(fitTodayRows);
  document.documentElement.dataset.window = WINDOW;
  // ⌘+ or the look, changed in another window of the app.
  window.addEventListener("storage", (e) => {
    if (e.key !== "orb.fontSize" && e.key !== "orb.look" && e.key !== null) return;
    applyAppearance();
    renderList();
  });
  // Whether there are tasks at all decides what this window even shows, so it
  // is settled before anything is drawn — `applyConfig` draws the sidebar, and
  // the store calls after it can wait seconds on a sync. Asked rather than
  // inferred from a failure: a task query can fail for reasons other than "not
  // configured", and treating every failure as absence would hide the feature
  // from somebody who has it and hit a bad sync.
  state.tasks.available = await call<boolean>("tasks_configured").catch(() => false);
  if (WINDOW === "tasks") {
    await bootTasksWindow();
    return;
  }
  if (WINDOW === "chat") {
    await bootChatWindow();
    return;
  }
  if (ITEM) {
    await bootOneItem();
    return;
  }
  installPasswordPrompt();
  // Mail left unsent by an earlier session is in the sidebar from the start,
  // above the task views, so it is there before the cursor is put on Today.
  await refreshOutbox();
  const bindingSummary = await applyConfig();
  state.view = state.settings.defaultView;
  const warnings = [...(await call<string[]>("config_warnings")), ...state.highlightWarnings];
  await refreshNotificationPermission();
  // Coming back from System Settings is when the answer changes.
  window.addEventListener("focus", () => void refreshNotificationPermission());
  await refreshPendingCounts();
  state.health = (await call<AccountHealth[]>("account_statuses").catch(() => [])) ?? [];

  if (state.tasks.available) {
    // The tag registry is needed before a tag can be named, and it is one small
    // query — cheaper to hold than to fetch on every keystroke that mentions
    // one.
    state.tasks.tags = await call<TaskTagInfo[]>("list_task_tags").catch(
      () => [],
    );
    await refreshTaskBadge().catch(() => {
      // A missing badge is not a reason to fail the boot.
    });
    await refreshExportStatus();
    await refreshTaskSync();
  }
  render();

  // Today is the landing view (§4.1). The mailboxes are one keystroke away and
  // the last folder is not remembered, deliberately: what is worth doing today
  // is the question this application opens with — unless there are no tasks, in
  // which case the question is the mail and the first folder is the answer.
  //
  // Nothing here may throw. This used to open Today unconditionally, so a config
  // with no `tasks` section failed on `list_tasks` and took the rest of the
  // boot with it: no focus, no second render, no midnight watch, no status line
  // and — worst of the five — no initial sync. The window came up with every
  // pane empty and stayed that way.
  // Unless something was opened already: the sidebar is on screen from the
  // render above, and a folder clicked in the meantime is where the user is.
  const navigated = state.generation !== startedAt;
  if (state.tasks.available && !navigated) {
    await openTaskView("today").catch(() => {
      // Falling back to the mail is better than an empty window.
      state.tasks.view = null;
    });
  }
  if (!navigated && !inTaskView()) await openFirstFolder();

  state.booting = false;
  state.focus = "list";
  render();
  // The badges count every folder's unread mail against the highlight rules,
  // which reads stored sources: a second or more on a cold database, and no
  // reason to keep the landing view waiting.
  await refreshHighlightBadges();
  renderFolders();
  // Chat is a group of its own at the foot of the sidebar; without a chat
  // section it is not there at all. After the landing view, as its server
  // can take seconds to answer, and Today draws its Waiting once it has.
  await initChat();
  render();
  watchTheDay();
  // A misconfigured folder is worth more attention than the binding summary:
  // it only bites later, when a delete or send fails against a mailbox that
  // does not exist.
  setStatus(warnings.length ? `config: ${warnings.join(" · ")}` : bindingSummary);

  // Sync, then fill in bodies — deliberately in that order and deliberately
  // not awaited. The window is already usable at this point, and new mail
  // should arrive without being asked for; fetching bodies first would spend
  // the opening minutes on a mailbox that is about to change anyway.
  //
  // The backfill starts even if the sync fails, because it is a resume: its
  // progress is whatever is already stored, and an offline start is exactly
  // when there is a backlog worth working through.
  void syncAll()
    .catch(() => {
      // `call` has already put the reason in the status line.
    })
    .finally(() => autoBackfill());
}
boot().catch((e: unknown) => {
  // Whatever failed, the window should say so rather than sit empty: the
  // status line is the one thing drawn before anything else is attempted.
  const reason = e instanceof Error ? e.message : typeof e === "string" ? e : JSON.stringify(e);
  setStatus(`startup failed: ${reason}`);
  state.booting = false;
  render();
  console.error("boot failed", e);
});
