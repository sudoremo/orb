import type { UserCommand } from "./bindings";
import { TaskListView, TaskViewName, TaskSummary, TaskDetail, TaskTagInfo } from "./tasklist";
import { localToday } from "./dates";
import type { Scope } from "./searchfield";
import type { MailConversation } from "./conversation";
import type { ChatLink, ChatRoom, ChatStatus, Conversation, WaitingThread } from "./chat";
import { AccountHealth, AccountInfo, AttachmentMeta, FolderInfo, HighlightBadge, HighlightStyleFields, MessageDetail, MessageHeader, NotificationPermission, Pane, QueuedSend, SearchHit, SenderCheck, SortKey, ViewMode, PinInfo, VirtualFolderInfo } from "./model";

/** This window's label, which events sent to this window alone are addressed to. */
export const LABEL: string =
  (window as unknown as { __TAURI_INTERNALS__?: { metadata?: { currentWindow?: { label?: string } } } })
    .__TAURI_INTERNALS__?.metadata?.currentWindow?.label ?? "main";
/**
 * The one message or task a window of its own shows, from its label:
 * `message-<id>` or `task-<id>`. Null in the mailbox and the tasks window.
 */
export const ITEM: { kind: "message" | "task"; id: number } | null = (() => {
  const match = /^(message|task)-(\d+)$/.exec(LABEL);
  return match ? { kind: match[1] as "message" | "task", id: Number(match[2]) } : null;
})();
/**
 * Which window this page is, by the window's label: the mailbox (`main`), the
 * window that shows tasks only (`tasks`) or chat only (`chat`), or one that
 * shows a single message or task (`message`, `task`). The same page serves
 * them all.
 */
export const WINDOW: "main" | "tasks" | "chat" | "message" | "task" =
  ITEM?.kind ?? (LABEL === "tasks" ? "tasks" : LABEL === "chat" ? "chat" : "main");
/** Whether Today has a mail half here: in the mailbox, not in the tasks window. */
export const todayHasMail = (): boolean => WINDOW === "main";

/** The results a search shows (§26): mail and tasks, or one kind of them. */
export type SearchKind = "all" | "mail" | "tasks";

/** Where the task export stands (todo-spec §20.3). */
export interface TaskExportStatus {
  configured: boolean;
  directory: string | null;
  last_complete: string | null;
  due: boolean;
  overdue: boolean;
  problem: string | null;
}

/** Where focus stands: the focus that runs, and the modes one can run in. */
export interface FocusStatus {
  /** Times in ms; `ends_at` is null for a focus the user ends. */
  session: { mode: string; started_at: number; ends_at: number | null } | null;
  modes: { name: string; detail: string }[];
  default_mode: string;
  /** `17:30`, what `workday` stands for as a span. */
  workday_ends: string | null;
}

/** Whether every task change is on the server (todo-spec §23). */
export interface TaskSyncStatus {
  /** Changes made here that the server does not have yet. */
  pending: number;
  /** Why the last task sync failed, until one succeeds. */
  problem: string | null;
}

/** Rows per fetched page. Matches the store's LIMIT/OFFSET granularity. */
export const PAGE_SIZE = 200;
/** Rows rendered beyond the viewport, to absorb fast scrolling. */
export const OVERSCAN = 8;
/**
 * Pages retained either side of the visible range before eviction.
 *
 * Generous on purpose: a page of headers is roughly 50 KB, so holding ±12
 * pages costs a couple of megabytes and keeps ~4800 rows warm. That is what
 * makes ordinary scrolling land in cache instead of on a placeholder.
 */
export const PAGE_RETENTION = 12;
/**
 * Pages fetched either side of the visible range.
 *
 * Prefetching the neighbours is what hides the paging: by the time the user
 * scrolls into the next page it is already there, so no placeholder is seen
 * except on a long jump.
 */
export const PAGE_PREFETCH = 2;
/**
 * Delay before fetching the pages under the viewport, in ms.
 *
 * Rendering is immediate; only fetching waits. Without this a single trackpad
 * fling requests every page it passes over — measured at 44 requests for one
 * flick through a 198k-row folder, each a real LIMIT/OFFSET query contending
 * for the store mutex. Deferring until the scroll settles reduces that to the
 * one or two pages actually landed on.
 */
export const FETCH_DEBOUNCE = 60;
/** How long the task search box rests before the view is re-queried. */
export const TASK_SEARCH_DEBOUNCE = 90;
/** How often the local civil date is re-checked, in ms (§4.3). */
export const DAY_WATCH_MS = 30_000;
export const state = {
  /** Settings from config.yml that the interface has to honour. */
  settings: {
    markReadAfterMs: 0,
    defaultView: "html" as ViewMode,
    columns: ["meta", "tasks", "from", "subject", "date"] as string[],
    dateFormat: "auto",
    /** The order a search's mail starts in; null, by relevance. */
    searchSort: null as { key: SortKey; dir: 1 | -1 } | null,
    completedFirst: false,
    density: "comfortable",
    theme: "system",
    fontSize: 13,
    darkHtmlMail: true,
    look: "hybrid" as "hybrid" | "terminal",
  },
  /** Whether HTML mail is drawn dark just now; starts from the setting. */
  darkMail: true,
  /** The sidebar's views, between the task views and the folders. */
  virtualFolders: [] as VirtualFolderInfo[],
  /** The top of the mailbox window's sidebar, as `pinned` orders it. */
  pins: [] as PinInfo[],
  /**
   * How the open message is being rendered.
   *
   * Starts from the configured default and can be switched per message. Not
   * remembered across restarts: the config says how mail is read, and a second
   * source of truth would quietly override it.
   */
  view: "html" as ViewMode,
  /** The last fetched source, kept only for the message it belongs to. */
  raw: null as { id: number; text: string } | null,
  /**
   * The embedded images of the message being read, as `data:` URLs by
   * Content-ID; `null` while they are being fetched. Kept for one message only.
   */
  inline: null as { id: number; images: Map<string, string> | null } | null,
  /** Messages allowed to load their remote images, by id, for this session. */
  remoteImages: new Set<number>(),
  /**
   * A panel shown in the reader instead of a message: the sync statistics,
   * the staged actions, the account status, the keys. Either plain `text` or
   * `html` built here from escaped parts. Cleared by `back`, by `close` and
   * by opening a message.
   */
  info: null as { title: string; text?: string; html?: string } | null,
  /**
   * While the mailbox window is still deciding where it lands: the list, the
   * reader and the toolbar stay blank until then, so the window appears once
   * in its final layout rather than assembling itself in front of the user.
   */
  booting: WINDOW === "main",
  /** What the headers of the message in the reader say against its sender; `null` while fetching. */
  senderCheck: null as { id: number; check: SenderCheck | null } | null,
  /** The attachments of the message in the reader; `null` while fetching. */
  attachments: null as { id: number; list: AttachmentMeta[] | null } | null,
  /** Staged actions per folder, keyed `account\u0000folder`, for the sidebar. */
  pendingCounts: new Map<string, number>(),
  /** Each account's standing with its server, for the sidebar and its panel. */
  health: [] as AccountHealth[],
  /** The queued and refused messages, oldest first; the Outbox is in the sidebar while there are any. */
  outbox: [] as QueuedSend[],
  /** The highlight rules' styles by name, in rule order. */
  highlightStyles: new Map<string, HighlightStyleFields>(),
  /** The badged highlight rules' unread counts, keyed `account\u0000folder`. */
  highlightBadges: new Map<string, HighlightBadge[]>(),
  /** Icons the highlight rules name that the bundled set does not have. */
  highlightWarnings: [] as string[],
  /** Asked at launch and whenever the window comes back to the front. */
  notificationPermission: "granted" as NotificationPermission,
  /** Whether the open folder is shown as conversations. */
  threads: false,
  /** The threads shown with their members, by thread id. */
  expandedThreads: new Set<string>(),
  /** The folder `threads` was last taken from, as `account\u0000folder`. */
  threadsFolder: "",
  /**
   * The conversation the reader shows for a collapsed thread's row: its
   * members, their bodies as they arrive, which are open and which one the
   * reader's keys act on. Kept for one row only.
   */
  conversation: null as MailConversation | null,
  /** Which highlighted match in the reader `n` last landed on. */
  matchIndex: -1,
  /** Configured accounts, used to exclude yourself from a reply-all. */
  accounts: [] as AccountInfo[],
  folders: [] as FolderInfo[],
  /**
   * The folder being read, as an index into `selectableFolders()`.
   *
   * Separate from the sidebar cursor because the two genuinely differ: walking
   * the sidebar does not change what is in the list until something is opened.
   */
  openFolder: 0,
  /**
   * Cursor over the sidebar as a whole — the task views and the folders in one
   * sequence.
   *
   * One cursor rather than one per section, because the sidebar is one column
   * and `j` walks down it. Which kind of entry it is on is a question for
   * `sidebarEntries`, not for the cursor.
   */
  sidebarCursor: 0,
  /** Today's reading section, when it is on: the items it lists and its own cursor. */
  reading: {
    rows: [] as SearchHit[],
    cursor: 0,
    /** Items selected on Today's timeline, by index. */
    selected: new Set<number>(),
  },
  /**
   * Loaded pages, keyed by page index. Only a window around the viewport is
   * kept — the full folder is never held in memory.
   */
  pages: new Map<number, MessageHeader[]>(),
  /** Pages currently in flight, so a scroll cannot request one twice. */
  fetching: new Set<number>(),
  /** Total rows in the folder, from `FolderInfo.total_count`. */
  total: 0,
  /**
   * Bumped whenever the folder or sort changes, so a page that resolves after
   * the user has moved on is discarded instead of rendered into the wrong list.
   */
  generation: 0,
  /** Cursor as a global row index, not an index into a loaded array. */
  messageCursor: 0,
  /**
   * Multi-selected row indices. Held as indices rather than message ids so a
   * selected row that has been evicted from the page cache is still addressable.
   */
  selected: new Set<number>(),
  /**
   * Where a range selection starts.
   *
   * Shift-clicking and shift-arrowing both mean "from there to here", and
   * without somewhere to count from, "there" would be the last row touched —
   * which makes an extended selection shrink as soon as it changes direction.
   */
  anchor: null as number | null,
  /**
   * The whole folder is selected.
   *
   * Held as a fact rather than as every index, because a folder can hold
   * hundreds of thousands of messages and the interesting question — which
   * UIDs — is one the database answers in a single query. Enumerating them
   * here would mean paging every row into memory to learn numbers already
   * stored beside them.
   */
  selectAll: false,
  /**
   * Message ids with a change applied locally but not yet acknowledged by the
   * server. Their rows render dimmed.
   */
  unconfirmed: new Set<number>(),
  detail: null as MessageDetail | null,
  focus: "folders" as Pane,
  /** The focus mode's standing (`focusmode.ts`), which is not the pane with the keyboard. */
  focusMode: null as FocusStatus | null,
  /**
   * The list fills the window, and the folders and the reader are drawers
   * over it: the folders while they have the keyboard, the reader once it
   * has had it, until it is closed (see `maximize.ts`).
   */
  maximized: false,
  /**
   * The folders are put away, the list and the reader taking their width;
   * they come back as a drawer while they have the keyboard. Kept across
   * restarts (see `maximize.ts`).
   */
  sidebarHidden: false,
  /** The reader's drawer is out. */
  drawer: false,
  /** The reader's drawer covers the whole list: maximized from the reader. */
  drawerWide: false,
  status: "",
  /**
   * Active sort, passed to `get_messages` so the ordering is applied by the
   * query over the whole folder rather than to the loaded page.
   */
  sort: { key: "date" as SortKey, dir: -1 as 1 | -1 },
  /**
   * Active search results, or null when browsing a folder.
   *
   * Held apart from the folder's paging state so leaving a search restores the
   * folder exactly, rather than reloading it.
   */
  search: null as {
    query: string;
    /** The query as run: the typed one, or its `~` fallback by meaning. */
    effective: string;
    hits: SearchHit[];
    /** The words to mark in the list and the reader. */
    terms: string[];
    /** The sidebar view this search is, if it is one rather than typed. */
    view?: number | "must-read";
    /** A conversation shown by `gc` or by following a link, by its subject. */
    conversation?: string;
    /** The order picked for the hits; absent, the query's own, by relevance. */
    sort?: { key: SortKey; dir: 1 | -1 };
    /**
     * The tasks the query found (§26), the completed among them too; the
     * task half shows the open ones and the completed once unfolded.
     */
    tasks: TaskSummary[];
    /** The query the tasks were found by, or null when this search finds none. */
    taskQuery: string | null;
    /** Which results are on screen: both kinds, or one (`⌘1`–`⌘3`). */
    kind: SearchKind;
    /** The Completed group of the tasks is unfolded. */
    completedOpen: boolean;
    /** The task view the search was made from, opened again on leaving it. */
    from: TaskViewName | null;
    /** Where the mail was searched (§4.5): everywhere, an account, folders. */
    scope: Scope;
  } | null,
  /**
   * The filter on the list on screen (§4.5), or null when there is none:
   * the view it belongs to, its text, and the messages it kept — held in
   * memory as search hits are — or null while it keeps every message.
   */
  filter: null as { key: string; text: string; hits: SearchHit[] | null } | null,
  /**
   * The folders Today's mail half pages through, when it is showing (§4.1):
   * every account's inbox, read as one list by the core. Held apart from the
   * sidebar's folder so a search made from Today does not destroy the list it
   * was made from.
   */
  inboxFolders: null as FolderInfo[] | null,
  /** Canonicalised binding tables, loaded from config.yml at boot. */
  bindings: {
    index_normal: {} as Record<string, string>,
    index_detail: {} as Record<string, string>,
    task_normal: {} as Record<string, string>,
    task_detail: {} as Record<string, string>,
    task_edit: {} as Record<string, string>,
    chat_normal: {} as Record<string, string>,
    chat_compose: {} as Record<string, string>,
  },
  /** The user's own commands (`bindings.commands`), by name. */
  userCommands: {} as Record<string, UserCommand>,
  /** Chat (PLAN.md, Phase 15): one Rocket.Chat server, when the config has one. */
  chat: {
    /** Whether the config has a chat section; settled at startup. */
    available: false,
    server: null as string | null,
    status: null as ChatStatus | null,
    /** The open chat view, or null while mail or tasks are shown. */
    view: null as "waiting" | "room" | null,
    /** The room a room view shows. */
    room: null as string | null,
    /** The threads that wait, for Waiting, its badge and Today. */
    waiting: [] as WaitingThread[],
    /** The threads that wait in muted rooms, which only their rooms count (chat-spec §13). */
    muted: [] as WaitingThread[],
    /** The rooms the user is in, newest first, for the sidebar and the picker. */
    rooms: [] as ChatRoom[],
    /** The rows the open chat list shows: Waiting's, or a room's threads. */
    rows: [] as WaitingThread[],
    cursor: 0,
    /** Selected threads, by room and thread, so that a thread leaving Waiting
     * takes no other row's place in the selection. */
    selected: new Set<string>(),
    /** Where a Shift-selection started, as a thread key. */
    anchor: null as string | null,
    /** Whether a page of older messages is being fetched. */
    loadingOlder: false,
    /** The thread in the reader, once fetched. */
    conversation: null as Conversation | null,
    /** Whether the composer has the keyboard. */
    composing: false,
    /** What was typed and not sent, by thread, for this session. */
    drafts: new Map<string, string>(),
    /** Rocket.Chat's "also send to the room" for a thread reply. */
    alsoInRoom: false,
    /** A message of a room's timeline the composer starts a thread on. */
    replyTo: null as { id: string; excerpt: string } | null,
    /** One of the user's messages the composer is editing. */
    editing: null as { id: string; rid: string } | null,
    /** Files the composer will post with its text: a path, or pasted bytes. */
    uploads: [] as { name: string; size: number; path?: string; data?: string; mime?: string }[],
    /** The reactions offered on a message, from the config. */
    reactions: [":+1:", ":heart:", ":joy:", ":tada:", ":eyes:"] as string[],
    /** Text that becomes a link in a message, from the config. */
    links: [] as ChatLink[],
    /** Whether the server has threads turned on; until it has said, it has. */
    threads: true,
    /** The long threads shown with every reply, by thread key (`za`). */
    unfolded: new Set<string>(),
  },
  /** Everything the task half of the interface needs. */
  tasks: {
    /** The open task view, or null while ordinary mail is being read. */
    view: null as TaskListView | null,
    rows: [] as TaskSummary[],
    cursor: 0,
    /** Multi-selected row indices, as the message list holds its own. */
    selected: new Set<number>(),
    anchor: null as number | null,
    /** Steps selected in the detail's checklist (§17), by id, and where a
     * Shift-click range starts. */
    stepSelection: new Set<string>(),
    /** The tasks marked as being worked on on this machine (§19). */
    workingOn: [] as TaskSummary[],
    /** Where the task export stands (§20.3), or null until asked. */
    export: null as TaskExportStatus | null,
    /** Whether the task changes have reached the server, or null until asked. */
    sync: null as TaskSyncStatus | null,
    stepAnchor: null as string | null,
    /** The step being written in place: its id, `"new"` for one being added. */
    stepEditing: null as string | null,
    /** The search box's contents for the open view (§4.5). */
    search: "",
    /**
     * Which half of Today has the keyboard (§4.2).
     *
     * Only meaningful in Today; the other views have one list. Kept beside the
     * view rather than folded into `state.focus`, because both halves are the
     * *list* pane — leaving the pane is still `back`, and `<tab>` moves within
     * it.
     */
    section: "tasks" as "tasks" | "mail" | "reading" | "chat",
    /** The task open in the reader, if one is. */
    detail: null as TaskDetail | null,
    /** Which field of the open task is being edited, if any. */
    editing: null as "title" | "body" | "both" | null,
    /**
     * A task that exists only to be named, and is erased if it never is.
     *
     * `create-task` has to mint a row before there is a title, because the
     * detail view edits a task rather than a blank. This is how the row is
     * remembered as not-yet-real, so that escaping the first title edit removes
     * it instead of leaving "New task" behind.
     */
    provisional: null as number | null,
    /**
     * Whether a designated tasks account is configured (§1).
     *
     * Settled once at startup. Tasks are opt-in, and their absence must leave a
     * working mail client rather than a window whose every pane is empty.
     */
    available: false,
    /** Today's open-task count, for the sidebar badge. */
    badge: 0,
    /** The tag registry, for resolving a typed tag name to its id. */
    tags: [] as TaskTagInfo[],
    /**
     * Open tasks per message id (§6.3), for the list's column.
     *
     * Filled by one batch query over the rows on screen, never one call per
     * row. A message with no tasks is stored as 0 so it is not asked about
     * again; the column draws nothing for it.
     */
    counts: new Map<number, number>(),
    /** The tasks of the message in the reader (§6.3). */
    linked: [] as TaskSummary[],
    /**
     * Which page of the Completed view has been loaded.
     *
     * Only Completed paginates: §10.2 refuses to cap it at an arbitrary
     * hundred, and it is the one view that grows without bound. The others are
     * bounded by what is actually outstanding.
     */
    page: 0,
    /** Whether the last page came back short, so there is no more to ask for. */
    exhausted: true,
    /**
     * The local civil date every view is drawn against.
     *
     * Read once and re-read by the day watcher, so a list and the detail beside
     * it cannot disagree about what "today" is halfway through midnight.
     */
    today: localToday(),
  },
};
export const el = {
  app: document.getElementById("app")!,
  folders: document.getElementById("folders")!,
  list: document.getElementById("list")!,
  reader: document.getElementById("reader")!,
  status: document.getElementById("status")!,
  tasksToggle: document.getElementById("tasks-toggle") as HTMLButtonElement,
  sidebarToggle: document.getElementById("sidebar-toggle") as HTMLButtonElement,
  tasksPanel: document.getElementById("tasks-panel")!,
  vsplit: document.getElementById("vsplit")!,
  hsplit: document.getElementById("hsplit")!,
  searchOverlay: document.getElementById("search-overlay") as HTMLDivElement,
  searchInput: document.getElementById("search-input") as HTMLInputElement,
  searchClear: document.getElementById("search-clear") as HTMLButtonElement,
};
