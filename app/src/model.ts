import { state } from "./state";

/** A view of the sidebar: a saved search, or a merged view, as a query. */
export interface VirtualFolderInfo {
  name: string;
  icon: string | null;
  query: string;
}
/**
 * Something at the top of the sidebar, in `pinned`'s order: a task view, a
 * reading view, chat's Waiting, a view by its name, or a folder.
 */
export interface PinInfo {
  kind: "tasks" | "reading" | "chat" | "view" | "folder";
  name: string | null;
  account: string | null;
  folder: string | null;
}
export interface FolderInfo {
  account_name: string;
  display_name: string;
  folder_name: string | null;
  is_account_header: boolean;
  /**
   * Repeated at the top of the sidebar as a shortcut. The entry also appears in
   * its account's own section, so anything that walks the folders looking for
   * one of them — All Inboxes, say — has to skip the duplicate.
   */
  is_pinned: boolean;
  unread_count: number;
  total_count: number;
  /** Whether the folder opens as conversations. */
  threads: boolean;
  /** The list's columns in this folder, when it has its own. */
  columns: string[] | null;
  /** What the folder is for: `inbox`, `drafts`, `sent`, `archive`, `junk`, `trash`. */
  role?: string | null;
  /** How deep in its account's tree the folder sits; 0 at the top. */
  depth?: number;
  /** The `folder_name` of the folder it sits inside. */
  parent?: string | null;
  /** The name shown under the parent: `2025` for `Archive/2025`. */
  label?: string;
  /** The order the folder opens in, as `sortOrder` spells it. */
  sort?: string;
}
export interface MessageHeader {
  id: number;
  uid: number;
  /** Where the message lives; every row knows, merged list or not. */
  account_name: string;
  folder_name: string;
  from_display: string | null;
  /** The recipients, for a list with a To column. */
  to_display?: string | null;
  subject: string | null;
  date: string | null;
  is_read: boolean;
  is_flagged: boolean;
  /** `\Answered`: replied to, from here or from another client. */
  is_answered?: boolean;
  /** `$Forwarded`: forwarded, from here or from another client. */
  is_forwarded?: boolean;
  has_attachments: boolean;
  pending_action: string | null;
  /** The highlight rules the message matches, in rule order. */
  highlights: string[];
  /** In a threaded list: the thread this row belongs to. */
  thread_id?: string | null;
  /** Members of the thread; 1 outside a threaded list. */
  thread_count?: number;
  thread_unread?: number;
  thread_expanded?: boolean;
  /** 0 for a thread's own row, one more per reply under it. */
  depth?: number;
  /** The date of the thread's newest message, shown on its row. */
  thread_latest?: string | null;
  /** Icons the `subject_munges` rules draw before the (rewritten) subject. */
  subject_icons?: SubjectIcon[];
  /** The opening of the message's text, on one line, once its body is here. */
  snippet?: string | null;
  /** Where a staged move takes the message. */
  pending_target?: string | null;
  /**
   * On the row of a thread shown collapsed, the ids of every member, oldest
   * first: the row stands for the whole conversation, and its marks, its
   * highlights and its snippet are the conversation's.
   */
  thread_members?: number[];
  /** On such a row, who wrote the newest member, whose words the snippet shows. */
  thread_newest_from?: string | null;
}
/** How a highlight rule draws the messages it matches; see `highlights`. */
export interface HighlightStyleFields {
  color: string | null;
  background: string | null;
  icon: string | null;
  icon_color: string | null;
  label: string | null;
  label_color: string | null;
  bold: boolean;
  italic: boolean;
  dim: boolean;
  sidebar_badge: boolean;
}
export interface HighlightStyle {
  name: string;
  style: HighlightStyleFields;
}
/** A badged highlight rule's unread count in one folder. */
export interface HighlightBadge {
  account: string;
  folder: string;
  rule: string;
  count: number;
}
/** How many messages of a folder a highlight rule matches. */
export interface HighlightCount {
  rule: string;
  count: number;
}
/** An icon a `subject_munges` rule draws before a subject. */
export interface SubjectIcon {
  /** A Font Awesome class, `fa-gears`. */
  icon: string;
  color: string | null;
}

export interface MessageDetail {
  id: number;
  /**
   * A part is still on the server: answered from what is stored, so that a
   * slow connection does not hold up reading it. `message_complete` brings
   * the rest.
   */
  incomplete?: boolean;
  /** Icons the `subject_munges` rules give the subject, shown in full here. */
  subject_icons?: SubjectIcon[];
  /** RFC822 Message-ID, needed to thread a reply correctly. */
  message_id: string | null;
  from_display: string | null;
  to_display: string | null;
  cc_display: string | null;
  subject: string | null;
  date: string | null;
  body_text: string | null;
  body_html: string | null;
  /** Whether the message carries attachments, which a forward has to mention. */
  has_attachments: boolean;
  /** The sender is on `load_remote_images_from`: remote images load unasked. */
  remote_images_allowed: boolean;
  /** The highlight rules the message matches, for the header. */
  highlights: string[];
}
/** What a message's headers say against its sender. */
export interface SenderCheck {
  /** The verdict that failed, `dmarc=fail`, when the receiving server found the sender forged. */
  failed: string | null;
  /** The `Reply-To` address, when it is outside the sender's domain. */
  reply_elsewhere: string | null;
}
/** An image embedded in a message, which its HTML refers to by `cid:`. */
export interface InlineImage {
  content_id: string;
  content_type: string;
  data_base64: string;
}
/**
 * What a delete or move the server carried out left behind: a warning when
 * the list does not yet show it, which the next sync puts right, and the
 * Message-IDs of what was relocated, which is how the move is undone.
 */
export interface WriteOutcome {
  warning: string | null;
  moved?: string[];
}
/** What a move or copy into another account did. */
export interface TransferOutcome {
  transferred: number;
  missing: number;
  warning: string | null;
}
/** One attachment of a message, as the core lists it. */
export interface AttachmentMeta {
  filename: string;
  content_type: string;
  size: number;
  part_id: string;
  is_inline: boolean;
  content_id: string | null;
  /** Why the file is dangerous to open, if it is: "is a program". */
  risk: string | null;
}
/** A staged action with the message it names, for the review panel. */
export interface PendingActionInfo {
  account_name: string;
  folder_name: string;
  uid: number;
  action_type: "Delete" | "Move";
  target_folder: string | null;
  created_at: string;
  message_id: number | null;
  subject: string | null;
  from_display: string | null;
}
/** A message in the Outbox: queued while its server was out of reach, or refused by it. */
export interface QueuedSend {
  id: number;
  account: string;
  /** The login it goes out over: the account, or `Account (Identity)`. */
  sending_as: string;
  from: string;
  to: string[];
  cc: string[];
  bcc: string[];
  subject: string;
  body: string;
  attachments: string[];
  queued_at: string;
  attempts: number;
  /** Why the last attempt did not send it. */
  error: string | null;
  /** Set once the server has refused it; it then waits for the user, not the server. */
  failed_at: string | null;
}
/** How many actions are staged in one folder. */
export interface PendingFolderCount {
  account_name: string;
  folder_name: string;
  count: number;
}
/** An account's standing with its server, from the last sync of it. */
export interface AccountHealth {
  account: string;
  state: "unknown" | "syncing" | "ok" | "auth_failed" | "certificate" | "unreachable" | "error";
  message: string | null;
  folder: string | null;
  at: string | null;
  last_ok: string | null;
  /** Why the tasks folder cannot sync, on the tasks account, while it cannot. */
  tasks?: string | null;
}
/** The accounts as the core describes them, special folders included. */
export interface AccountInfo {
  name: string;
  email: string;
  drafts_folder: string;
  trash_folder: string;
  archive_folder: string;
  junk_folder: string;
}
/** What `delete_messages` did: the recoverable move, or the permanent delete. */
export type DeleteOutcome =
  | ({ kind: "MovedToTrash"; trash: string } & WriteOutcome)
  | ({ kind: "Deleted" } & WriteOutcome);
/** What `execute_pending` did. */
export interface PendingExecution {
  executed: number;
  warnings: string[];
}
/** A step of an in-progress sync, emitted by the core while it runs. */
export interface SyncProgress {
  account: string;
  stage: string;
  folder: string | null;
  current: number | null;
  total: number | null;
  fraction: number;
}
/** One search hit: the message plus where it lives. */
export interface SearchHit {
  message: MessageHeader;
  account_name: string;
  folder_name: string;
}
/** Outcome of one body-backfill pass. */
export interface BodyBackfill {
  /** HTML renderings stored by this pass, for the newest mail of each folder. */
  html: number;
  /** Full sources stored by this pass, for messages with attachments. */
  sources: number;
  /** Messages with attachments whose source is still to be fetched. */
  sources_pending: number;
  /** Messages whose MIME shape was recorded by this pass. */
  shaped: number;
  /** Messages still without a known shape. */
  unshaped: number;
  /** Every message — the shaping phase's denominator. */
  messages: number;
  fetched: number;
  missing: number;
  pending: number;
  total: number;
  /** Messages whose structure the server sent unparseably; skipped for good. */
  unreadable: number;
  /** Messages given a search vector by this pass. */
  embedded: number;
  /** Messages still to be indexed for search by meaning. */
  embeddings_pending: number;
  /** Messages whose threading headers this pass fetched. */
  threaded: number;
  /** Messages stored before the sync fetched those, still to be asked. */
  threads_pending: number;
}
export interface SyncResult {
  folders_synced: number;
  /** Folders another sync held the lock on; each is named in `skipped`. */
  folders_skipped: number;
  messages_added: number;
  messages_updated: number;
  messages_deleted: number;
  /** New messages a filter rule acted on. */
  messages_filtered: number;
  errors: string[];
  /** What a filter rule could not do. */
  filter_warnings: string[];
  skipped: string[];
}
/** What a run of the filter rules did. */
export interface FilterOutcome {
  filtered: number;
  warnings: string[];
}
/** A reading of a recurrence typed by a person: the rule and its words. */
export interface RecurrenceSuggestion {
  rrule: string;
  label: string;
}
/** One message a filter rule acted on, for the review panel. */
export interface FilterActivity {
  account: string;
  folder: string;
  rule: string;
  actions: string;
  subject: string | null;
  from: string | null;
  at: string;
}
/** How a message can be rendered. */
export type ViewMode = "html" | "text" | "raw";
export type Pane = "folders" | "list" | "reader";
/**
 * Sortable columns. Status has no entry: the core orders by the SQL columns
 * `date_unix`, `subject` and `from_addr`, and read/flagged state lives in a
 * JSON `flags` blob that it cannot sort on.
 */
export type SortKey = "from" | "subject" | "date";
export interface Column {
  /** `null` for a display-only column. */
  key: SortKey | null;
  label: string;
  cls: string;
}
/** The `MessageSortOrder` variant for the active column and direction. */
/** The sort a `MessageSortOrder` name stands for; newest first for anything else. */
export function sortFrom(order: string | undefined): { key: SortKey; dir: 1 | -1 } {
  switch (order) {
    case "DateOldest": return { key: "date", dir: 1 };
    case "SubjectAsc": return { key: "subject", dir: 1 };
    case "SubjectDesc": return { key: "subject", dir: -1 };
    case "SenderAsc": return { key: "from", dir: 1 };
    case "SenderDesc": return { key: "from", dir: -1 };
    default: return { key: "date", dir: -1 };
  }
}
export function sortOrder(sort: { key: SortKey; dir: 1 | -1 } = state.sort): string {
  const ascending = sort.dir === 1;
  switch (sort.key) {
    case "from":
      return ascending ? "SenderAsc" : "SenderDesc";
    case "subject":
      return ascending ? "SubjectAsc" : "SubjectDesc";
    case "date":
      return ascending ? "DateOldest" : "DateNewest";
  }
}

/** Whether Orb may post notifications; mirrors `notifications::Permission`. */
export type NotificationPermission = "granted" | "denied" | "undetermined" | "unavailable";
