//! The API the app is built on.
//!
//! [`OrbHandle`] wraps the store, the config and the task layer behind
//! blocking methods with serialisable arguments and results, so the Tauri
//! commands in `app/src-tauri` can stay thin shims over it.

use crate::config::{self, BindingsConfig as RustBindingsConfig, Config};
pub use crate::credentials::{PasswordAnswer, PasswordPrompter, PasswordRequest};
use crate::models::SearchResultItem;
use crate::services::{AccountHealth, HealthBook};
use crate::store::{
    DeleteOutcome, FilterActivity, FilterOutcome, HighlightBadge, HighlightCount,
    MessageSort as RustMessageSort, PENDING_ACTION_DELETE, PENDING_ACTION_MOVE,
    PendingActionCount as RustPendingActionCount, PendingActionRecord as RustPendingActionRecord,
    PendingExecution, PendingScope as RustPendingScope, SearchScope as RustSearchScope, Store,
    SyncFailure, SyncReport as RustSyncReport, TransferOutcome, WriteOutcome,
};
use crate::tasks::{
    COMPLETED_PAGE_SIZE, CachedEmailMetadata, ChangeContext, MutationOutcome, NotificationHost,
    ReminderService, ScheduledNotification, Task, TaskAttachment, TaskError, TaskLinkedEmail,
    TaskListItem, TaskLogEntry, TaskQuery, TaskReminder, TaskRepository, TaskSyncReport,
    TaskSyncService, TaskTag, TaskUpdates, TaskView, UndoEntry, UndoStep, attachment_cache_root,
    cached_attachment_path, canonical_message_id, normalise_title, tag_label,
};
use crate::undo::{Place, Step as UndoAction};
use crate::{Error as OrbCoreError, FolderDisplay, MessageListItem, SyncStage as RustSyncStage};
use chrono::Utc;
pub use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::runtime::Runtime;

// ============================================================================
// Error Types
// ============================================================================

/// FFI-compatible error type
#[derive(Debug, Error, serde::Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
pub enum OrbError {
    #[error("Configuration error: {message}")]
    ConfigError { message: String },

    /// There is no config file at `path`: a first launch. The application
    /// answers it with [`config::write_template`] rather than an error.
    #[error("Config file not found at {path}")]
    ConfigMissing { path: String },

    #[error("Database error: {message}")]
    DatabaseError { message: String },

    #[error("IMAP error: {message}")]
    ImapError { message: String },

    #[error("Sync error: {message}")]
    SyncError { message: String },

    #[error("Send error: {message}")]
    SendError { message: String },

    #[error("Not found: {message}")]
    NotFound { message: String },

    /// The request was well-formed but breaks a rule of the domain: a date
    /// that is not a date, an unknown view.
    ///
    /// Separate from `ConfigError` because it is the user's *action* that was
    /// wrong rather than their configuration, and separate from `SyncError`
    /// because it is the one error class the UI is expected to render verbatim
    /// in the status line rather than as a failure.
    #[error("{message}")]
    InvalidInput { message: String },
}

impl From<OrbCoreError> for OrbError {
    fn from(err: OrbCoreError) -> Self {
        match &err {
            OrbCoreError::Config(_) | OrbCoreError::InvalidConfig { .. } => OrbError::ConfigError {
                message: err.to_string(),
            },
            OrbCoreError::Database(_)
            | OrbCoreError::DatabaseError { .. }
            | OrbCoreError::TransactionFailed { .. }
            | OrbCoreError::PoolError(_) => OrbError::DatabaseError {
                message: err.to_string(),
            },
            OrbCoreError::Imap(_)
            | OrbCoreError::Unreachable { .. }
            | OrbCoreError::AuthenticationFailed { .. }
            | OrbCoreError::ImapSyncFailed { .. }
            | OrbCoreError::ImapProtocolError { .. } => OrbError::ImapError {
                message: err.to_string(),
            },
            OrbCoreError::Sync(_)
            | OrbCoreError::SyncAborted { .. }
            | OrbCoreError::UidValidityChanged { .. } => OrbError::SyncError {
                message: err.to_string(),
            },
            OrbCoreError::NotFound(_) => OrbError::NotFound {
                message: err.to_string(),
            },
            // The task rules arrive here: a date that is not one, an
            // unparseable recurrence, an unknown view. Every one of them is something the
            // user did that the status line has to explain, and folding them
            // into `SyncError` would present a rule as a failure.
            OrbCoreError::InvalidInput(_) => OrbError::InvalidInput {
                message: err.to_string(),
            },
            _ => OrbError::SyncError {
                message: err.to_string(),
            },
        }
    }
}

impl From<config::ConfigError> for OrbError {
    fn from(err: config::ConfigError) -> Self {
        match err {
            config::ConfigError::Missing { path } => OrbError::ConfigMissing { path },
            err => OrbError::ConfigError {
                message: err.to_string(),
            },
        }
    }
}

impl From<TaskError> for OrbError {
    fn from(err: TaskError) -> Self {
        OrbError::InvalidInput {
            message: err.to_string(),
        }
    }
}

// ============================================================================
// FFI Data Structures
// ============================================================================

/// Account information for UI display
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Account {
    pub name: String,
    pub email: String,
    /// Where this account keeps drafts, so opening one can be told from
    /// reading ordinary mail.
    pub drafts_folder: String,
    /// Where a delete puts a message, and where a staged delete is final.
    pub trash_folder: String,
    /// Where `archive` files a message.
    pub archive_folder: String,
    /// Where `spam` files a message, and where `not-spam` takes it from.
    pub junk_folder: String,
}

/// Folder information for sidebar display
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FolderInfo {
    pub account_name: String,
    pub display_name: String,
    pub folder_name: Option<String>,
    pub is_account_header: bool,
    pub is_pinned: bool,
    pub show_unread: bool,
    pub show_total: bool,
    pub unread_count: u32,
    pub total_count: u32,
    /// Whether the folder opens as conversations.
    pub threads: bool,
    /// The list's columns in this folder, when it has its own.
    pub columns: Option<Vec<String>>,
    /// What the folder is for, which picks its icon: `inbox`, `drafts`,
    /// `sent`, `archive`, `junk` or `trash`.
    pub role: Option<String>,
    /// How deep in the account's tree the folder sits; 0 at the top.
    pub depth: u32,
    /// The `folder_name` of the folder it sits inside.
    pub parent: Option<String>,
    /// The name the sidebar shows under the parent.
    pub label: String,
    /// The order the folder opens in, as `messages` takes it.
    pub sort: RustMessageSort,
}

impl From<FolderDisplay> for FolderInfo {
    fn from(fd: FolderDisplay) -> Self {
        FolderInfo {
            account_name: fd.account_name,
            display_name: fd.display_name,
            folder_name: fd.folder.map(|f| f.name),
            is_account_header: fd.is_account_header,
            is_pinned: fd.is_pinned,
            show_unread: fd.show_unread,
            show_total: fd.show_total,
            unread_count: fd.unread_count,
            total_count: fd.total_count,
            threads: fd.is_threaded,
            columns: fd.columns,
            role: fd.role,
            depth: fd.depth,
            parent: fd.parent,
            label: fd.label,
            sort: fd.sort,
        }
    }
}

/// The kind of a staged (pending) destructive action on a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PendingActionType {
    /// Message is staged for deletion (recoverable move-to-Trash on execute).
    Delete,
    /// Message is staged to be moved to another folder.
    Move,
}

impl PendingActionType {
    /// Parse the persisted `action_type` text used in the `pending_actions`
    /// table. Unknown values map to `Delete` defensively (the CHECK constraint
    /// makes them unreachable in practice).
    fn from_db(action_type: &str) -> Self {
        match action_type {
            PENDING_ACTION_MOVE => PendingActionType::Move,
            _ => PendingActionType::Delete,
        }
    }

    /// The persisted `action_type` text for this kind.
    fn as_db(self) -> &'static str {
        match self {
            PendingActionType::Delete => PENDING_ACTION_DELETE,
            PendingActionType::Move => PENDING_ACTION_MOVE,
        }
    }
}

/// True when `flags` contains the named IMAP system flag.
///
/// Accepts both the canonical wire form (`\Seen`) and the legacy Rust-debug
/// form (`Seen`) that earlier syncs persisted, so databases written before
/// [`ImapClient::flag_to_wire`] report read and flagged state correctly without
/// needing every row rewritten. System flag names are case-insensitive per
/// RFC 3501, and a custom keyword (stored as `Custom("Old")` by the old path)
/// cannot collide with a system name.
fn has_system_flag(flags: &[String], name: &str) -> bool {
    flags
        .iter()
        .any(|f| f.strip_prefix('\\').unwrap_or(f).eq_ignore_ascii_case(name))
}

/// Message header for list display
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageHeader {
    pub id: i64,
    pub uid: u32,
    /// Where the message lives, so a row in a merged or searched list knows
    /// its folder without asking what the window has open.
    #[serde(default)]
    pub account_name: String,
    #[serde(default)]
    pub folder_name: String,
    pub from_display: Option<String>,
    /// The recipients, for a list with a To column.
    #[serde(default)]
    pub to_display: Option<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
    pub is_read: bool,
    pub is_flagged: bool,
    /// `\Answered`: replied to, from here or from another client.
    #[serde(default)]
    pub is_answered: bool,
    /// `$Forwarded`, the keyword clients set on a message they forwarded.
    #[serde(default)]
    pub is_forwarded: bool,
    pub has_attachments: bool,
    /// Set when this message has a staged (not-yet-executed) delete/move action,
    /// so the list can render a pending marker. `None` when no action is staged.
    pub pending_action: Option<PendingActionType>,
    /// The highlight rules the message matches, in rule order; the interface
    /// draws them with the styles from `highlight_styles`.
    #[serde(default)]
    pub highlights: Vec<String>,
    /// In a threaded list: the thread this row belongs to.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Members of the thread; 1 outside a threaded list.
    #[serde(default = "one")]
    pub thread_count: u32,
    #[serde(default)]
    pub thread_unread: u32,
    #[serde(default)]
    pub thread_expanded: bool,
    /// 0 for a thread's own row, one more per reply under it.
    #[serde(default)]
    pub depth: u32,
    /// The date of the thread's newest message, for its row.
    #[serde(default)]
    pub thread_latest: Option<String>,
    /// Icons the `subject_munges` rules draw before the subject, which is
    /// already the rewritten one.
    #[serde(default)]
    pub subject_icons: Vec<crate::subject_munger::SubjectIcon>,
    /// The opening of the message's text, on one line, once its body is here.
    #[serde(default)]
    pub snippet: Option<String>,
    /// Where a staged move takes the message.
    #[serde(default)]
    pub pending_target: Option<String>,
    /// On the row of a thread shown collapsed, the ids of every member,
    /// oldest first: the row stands for all of them. Empty on every other
    /// row.
    #[serde(default)]
    pub thread_members: Vec<i64>,
    /// On the row of a thread shown collapsed, the sender of its newest
    /// member, whose words the row's snippet then shows.
    #[serde(default)]
    pub thread_newest_from: Option<String>,
}

fn one() -> u32 {
    1
}

impl From<MessageListItem> for MessageHeader {
    fn from(msg: MessageListItem) -> Self {
        MessageHeader::from_item(msg, false)
    }
}

impl MessageHeader {
    /// Build a header from a list item, supplying the attachment flag (computed
    /// separately from the message's cached RFC822 source). The pending-action
    /// marker is left unset; callers that have folder-level staging context set
    /// it afterwards via [`MessageHeader::with_pending_action`].
    fn from_item(msg: MessageListItem, has_attachments: bool) -> Self {
        let is_read = has_system_flag(&msg.flags, "Seen");
        let is_flagged = has_system_flag(&msg.flags, "Flagged");
        let is_answered = has_system_flag(&msg.flags, "Answered");
        let is_forwarded = msg
            .flags
            .iter()
            .any(|f| f.trim_start_matches('$').eq_ignore_ascii_case("Forwarded"));

        MessageHeader {
            id: msg.id,
            uid: msg.uid,
            account_name: String::new(),
            folder_name: String::new(),
            subject_icons: Vec::new(),
            from_display: msg.from,
            to_display: msg.to,
            subject: msg.subject,
            date: msg.date,
            is_read,
            is_flagged,
            is_answered,
            is_forwarded,
            has_attachments,
            pending_action: None,
            highlights: Vec::new(),
            thread_id: None,
            thread_count: 1,
            thread_unread: 0,
            thread_expanded: false,
            depth: 0,
            thread_latest: None,
            snippet: None,
            pending_target: None,
            thread_members: Vec::new(),
            thread_newest_from: None,
        }
    }

    fn with_thread(mut self, row: &crate::store::ThreadRow) -> Self {
        self.thread_id = Some(row.thread_id.clone());
        self.thread_count = row.count;
        self.thread_unread = row.unread;
        self.thread_expanded = row.expanded;
        self.depth = row.depth;
        self.thread_latest = row.latest.clone();
        self.thread_members = row.members.iter().map(|(_, item)| item.id).collect();
        self
    }

    fn with_highlights(mut self, highlights: Option<Vec<String>>) -> Self {
        self.highlights = highlights.unwrap_or_default();
        self
    }

    /// Attach a staged-action marker to this header.
    fn with_pending_action(mut self, pending: Option<PendingActionType>) -> Self {
        self.pending_action = pending;
        self
    }
}

/// Metadata describing a single attachment on a message, for display and
/// (in a later slice) download.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachmentMeta {
    /// Suggested filename for the attachment.
    pub filename: String,
    /// MIME type, e.g. `application/pdf`.
    pub content_type: String,
    /// Decoded size in bytes.
    pub size: u64,
    /// Stable identifier of the MIME part within the message, used to extract
    /// the attachment's bytes later.
    pub part_id: String,
    /// True when the part is displayed inline (e.g. an embedded image).
    pub is_inline: bool,
    /// `Content-ID` value (brackets stripped) for resolving `cid:` references.
    pub content_id: Option<String>,
    /// Why the file is dangerous to open, if it is: "is an Office document
    /// that can run macros".
    pub risk: Option<String>,
}

impl From<crate::attachments::AttachmentMeta> for AttachmentMeta {
    fn from(a: crate::attachments::AttachmentMeta) -> Self {
        AttachmentMeta {
            risk: crate::safety::attachment_risk(&a.filename).map(str::to_string),
            filename: a.filename,
            content_type: a.content_type,
            size: a.size,
            part_id: a.part_id,
            is_inline: a.is_inline,
            content_id: a.content_id,
        }
    }
}

/// The decoded bytes of a single attachment, plus the metadata needed to save
/// or open it (default filename for a save panel, MIME type for the OS to pick a
/// handler application).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachmentData {
    /// Suggested filename (used as the save-panel default).
    pub filename: String,
    /// MIME type, e.g. `application/pdf`.
    pub content_type: String,
    /// The attachment's content with its transfer encoding already decoded.
    pub data: Vec<u8>,
}

impl From<crate::attachments::ExtractedAttachment> for AttachmentData {
    fn from(a: crate::attachments::ExtractedAttachment) -> Self {
        AttachmentData {
            filename: a.filename,
            content_type: a.content_type,
            data: a.data,
        }
    }
}

/// How many sender checks a session keeps before starting over.
const SENDER_CHECKS_KEPT: usize = 2000;

/// Full message detail for viewing
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageDetail {
    pub id: i64,
    pub uid: u32,
    /// RFC822 Message-ID header of this message, used to populate the
    /// In-Reply-To / References headers when replying so the outgoing mail
    /// threads correctly.
    pub message_id: Option<String>,
    pub from_display: Option<String>,
    pub to_display: Option<String>,
    pub cc_display: Option<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub flags: Vec<String>,
    /// True when the message has one or more attachments (computed from its
    /// RFC822 source, which is fetched on demand when the message is opened).
    pub has_attachments: bool,
    /// The icons the `subject_munges` rules give this message's subject.
    /// The subject itself is shown in full here.
    #[serde(default)]
    pub subject_icons: Vec<crate::subject_munger::SubjectIcon>,
    /// True when the sender is on `load_remote_images_from` or the message
    /// matches a `load_remote_images` rule, so the HTML rendering may fetch
    /// its remote images without being asked.
    pub remote_images_allowed: bool,
    /// The highlight rules the message matches, for the reader's header.
    #[serde(default)]
    pub highlights: Vec<String>,
    /// True when a part of the message is still on the server: read from
    /// what is stored, or the fetch failed. [`OrbHandle::get_message`] asks
    /// for the rest.
    #[serde(default)]
    pub incomplete: bool,
}

/// An image embedded in a message and referenced from its HTML by `cid:`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InlineImage {
    /// The `Content-ID` the HTML refers to, without the angle brackets.
    pub content_id: String,
    /// MIME type, e.g. `image/png`.
    pub content_type: String,
    /// The decoded bytes, base64-encoded for a `data:` URL.
    pub data_base64: String,
}

/// Scope restriction for a full-text mailbox search.
///
/// Mirrors [`crate::store::SearchScope`] for the FFI boundary. `All` searches
/// every folder of every account (the default whole-mailbox behaviour); the
/// narrower variants restrict results to one account or one folder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SearchScope {
    All,
    Account {
        account_name: String,
    },
    Folder {
        account_name: String,
        folder_name: String,
    },
    /// Several folders, as `(account, folder)`.
    Folders {
        folders: Vec<(String, String)>,
    },
}

impl From<SearchScope> for RustSearchScope {
    fn from(scope: SearchScope) -> Self {
        match scope {
            SearchScope::All => RustSearchScope::All,
            SearchScope::Account { account_name } => RustSearchScope::Account { account_name },
            SearchScope::Folder {
                account_name,
                folder_name,
            } => RustSearchScope::Folder {
                account_name,
                folder_name,
            },
            SearchScope::Folders { folders } => RustSearchScope::Folders(folders),
        }
    }
}

/// `name`, or `name (2)`, `name (3)`… before the extension, whichever is
/// not in `taken` yet; the result is added to it.
fn unique_name(taken: &mut std::collections::HashSet<String>, name: String) -> String {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name.as_str(), ""),
    };
    let mut candidate = name.clone();
    let mut n = 2;
    while taken.contains(&candidate.to_lowercase()) {
        candidate = format!("{stem} ({n}){extension}");
        n += 1;
    }
    taken.insert(candidate.to_lowercase());
    candidate
}

#[cfg(feature = "desktop")]
fn editor_error(e: crate::editor::EditorError) -> OrbError {
    OrbError::ConfigError {
        message: e.to_string(),
    }
}

/// Sort order for a folder's message list.
///
/// Mirrors [`crate::store::MessageSort`] for the FFI boundary. Callers that pass
/// `None` to [`OrbHandle::get_messages`] get the folder's configured order from
/// `config.yml` instead; these variants let a UI override it per view (e.g. a
/// clickable column header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MessageSortOrder {
    DateNewest,
    DateOldest,
    SubjectAsc,
    SubjectDesc,
    SenderAsc,
    SenderDesc,
}

impl From<MessageSortOrder> for RustMessageSort {
    fn from(order: MessageSortOrder) -> Self {
        match order {
            MessageSortOrder::DateNewest => RustMessageSort::DateNewest,
            MessageSortOrder::DateOldest => RustMessageSort::DateOldest,
            MessageSortOrder::SubjectAsc => RustMessageSort::SubjectAsc,
            MessageSortOrder::SubjectDesc => RustMessageSort::SubjectDesc,
            MessageSortOrder::SenderAsc => RustMessageSort::SenderAsc,
            MessageSortOrder::SenderDesc => RustMessageSort::SenderDesc,
        }
    }
}

/// A single full-text search hit: the message header plus the account and folder
/// it lives in, so the UI can show context and open it in its folder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub message: MessageHeader,
    pub account_name: String,
    pub folder_name: String,
}

impl From<SearchResultItem> for SearchResult {
    fn from(item: SearchResultItem) -> Self {
        let mut message = MessageHeader::from(item.message);
        message.account_name = item.account_name.clone();
        message.folder_name = item.folder_name.clone();
        SearchResult {
            message,
            account_name: item.account_name,
            folder_name: item.folder_name,
        }
    }
}

/// One unread message a sync brought to a watched folder, for a
/// notification.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewMailItem {
    pub id: i64,
    pub account_name: String,
    pub folder_name: String,
    pub from_display: Option<String>,
    pub subject: Option<String>,
}

impl From<crate::db::NewMessage> for NewMailItem {
    fn from(m: crate::db::NewMessage) -> Self {
        NewMailItem {
            id: m.id,
            account_name: m.account,
            folder_name: m.folder,
            from_display: m.from,
            subject: m.subject,
        }
    }
}

/// Scope restriction for staged (pending) actions.
///
/// Mirrors [`crate::store::PendingScope`] for the FFI boundary. `All` covers the
/// whole mailbox; the narrower variants restrict to one account or one folder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PendingScope {
    All,
    Account {
        account_name: String,
    },
    Folder {
        account_name: String,
        folder_name: String,
    },
}

impl From<PendingScope> for RustPendingScope {
    fn from(scope: PendingScope) -> Self {
        match scope {
            PendingScope::All => RustPendingScope::All,
            PendingScope::Account { account_name } => RustPendingScope::Account { account_name },
            PendingScope::Folder {
                account_name,
                folder_name,
            } => RustPendingScope::Folder {
                account_name,
                folder_name,
            },
        }
    }
}

/// A single staged action, for the review UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingActionInfo {
    pub account_name: String,
    pub folder_name: String,
    pub uid: u32,
    pub action_type: PendingActionType,
    /// Destination folder for a move; `None` for a delete.
    pub target_folder: Option<String>,
    /// RFC3339 timestamp of when the action was staged.
    pub created_at: String,
    /// The message's row id, when it is still stored; a review can open it.
    pub message_id: Option<i64>,
    pub subject: Option<String>,
    pub from_display: Option<String>,
}

impl From<RustPendingActionRecord> for PendingActionInfo {
    fn from(r: RustPendingActionRecord) -> Self {
        PendingActionInfo {
            account_name: r.account,
            folder_name: r.folder,
            uid: r.uid,
            action_type: PendingActionType::from_db(&r.action_type),
            target_folder: r.target_folder,
            created_at: r.created_at,
            message_id: None,
            subject: None,
            from_display: None,
        }
    }
}

/// Number of staged actions in a single (account, folder), for badge display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingFolderCount {
    pub account_name: String,
    pub folder_name: String,
    pub count: u32,
}

impl From<RustPendingActionCount> for PendingFolderCount {
    fn from(c: RustPendingActionCount) -> Self {
        PendingFolderCount {
            account_name: c.account,
            folder_name: c.folder,
            count: c.count,
        }
    }
}

/// A step of an in-progress sync, flattened for the FFI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncProgressUpdate {
    pub account: String,
    /// Stage name: `connecting`, `fetching_folders`, `syncing_folder`,
    /// `processing_deletions`, `retrying` or `complete`.
    pub stage: String,
    /// Folder being synced, when the stage names one; for `retrying`, what
    /// failed and when the next attempt is.
    pub folder: Option<String>,
    /// Messages processed so far in this folder, when known.
    pub current: Option<u32>,
    /// Messages to process in this folder, when known.
    pub total: Option<u32>,
    /// Overall progress in 0.0..=1.0.
    pub fraction: f32,
}

impl SyncProgressUpdate {
    fn new(account: String, stage: &RustSyncStage, fraction: f32) -> Self {
        let (name, folder, current, total) = match stage {
            RustSyncStage::Connecting => ("connecting", None, None, None),
            RustSyncStage::FetchingFolders => ("fetching_folders", None, None, None),
            RustSyncStage::SyncingFolder {
                name,
                current,
                total,
            } => (
                "syncing_folder",
                Some(name.clone()),
                current.map(|c| c as u32),
                total.map(|t| t as u32),
            ),
            RustSyncStage::ProcessingDeletions => ("processing_deletions", None, None, None),
            RustSyncStage::Complete => ("complete", None, None, None),
        };
        SyncProgressUpdate {
            account,
            stage: name.to_string(),
            folder,
            current,
            total,
            fraction,
        }
    }
}

/// Receives sync progress as it happens.
///
/// `Store::sync` has always emitted progress on a channel; the FFI simply threw
/// it away, so both UIs could only show a spinner for however long a sync took
/// — which on a cold 198k-message folder is over five minutes with no sign of
/// life.
/// Told about the syncs the core runs on its own: on a push notification,
/// on the periodic timer, and after the machine wakes.
pub trait PushListener: Send + Sync + 'static {
    /// A sync began. `folder` is `None` for a sync of the whole account.
    fn on_started(&self, account: String, folder: Option<String>) {
        let _ = (account, folder);
    }
    /// A sync ended. `folder` is `None` for a sync of the whole account.
    fn on_synced(&self, account: String, folder: Option<String>, result: SyncResult);
    /// A sync failed before it could report.
    fn on_error(&self, account: String, folder: Option<String>, error: String);
}

pub trait SyncProgressListener: Send + Sync {
    fn on_progress(&self, update: SyncProgressUpdate);
}

/// Hears of the task uploads; see [`OrbHandle::start_task_uploads`].
pub trait TaskSyncListener: Send + Sync + 'static {
    fn on_task_sync(&self, event: TaskSyncEvent);
}

/// What a round of the task uploader changed.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskSyncEvent {
    /// Tasks were uploaded, downloaded or removed: the lists are out of date.
    pub tasks_changed: bool,
    /// The tasks folder started or stopped failing: the account's standing
    /// is out of date.
    pub problem_changed: bool,
}

/// The iPhone's setup code, as the window shows it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IphoneSetupCode {
    /// The QR code.
    pub svg: String,
    /// When the code stops working, seconds since the epoch.
    pub expires: i64,
    /// `username on server`, so the window says whose password it shows.
    pub login: String,
    pub folder: String,
}

/// Whether every task change has reached the server (todo-spec §23).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskSyncStatus {
    /// Changes made here that the server does not have yet: tasks, the tag
    /// registry, and purged tasks whose messages are still there.
    pub pending: u32,
    /// Why the last task sync failed, until one succeeds.
    pub problem: Option<String>,
}

/// Hears of the config file being re-read; see [`OrbHandle::start_config_watch`].
pub trait ConfigListener: Send + Sync + 'static {
    fn on_config_reloaded(&self, outcome: ConfigReload);
}

/// One badge on the Dock icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockBadgeCount {
    pub count: u32,
    /// `#rrggbb`, or `None` for the system's red.
    pub color: Option<String>,
}

/// What came of re-reading the config file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigReload {
    /// Why the file could not be read, in which case the previous config
    /// stays in force.
    pub error: Option<String>,
    /// The keys whose change only a restart can follow; see
    /// [`config::keys_needing_restart`].
    pub restart_needed: Vec<String>,
}

/// Forward progress from the store's channel to a listener until the sync ends.
///
/// The channel closes when `sync` drops the last sender, which is what ends
/// this loop.
async fn forward_progress(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::StateUpdate>,
    listener: Box<dyn SyncProgressListener>,
) {
    while let Some(update) = rx.recv().await {
        match update {
            crate::StateUpdate::SyncProgress {
                account,
                stage,
                progress,
            } => listener.on_progress(SyncProgressUpdate::new(account, &stage, progress)),
            // A retry in progress is worth more to the person waiting than a
            // spinner: it says what failed and how long until the next try.
            crate::StateUpdate::RetryAttempt {
                operation,
                attempt,
                max_attempts,
                next_delay_ms,
                error,
            } => listener.on_progress(SyncProgressUpdate {
                account: operation,
                stage: "retrying".to_string(),
                folder: Some(format!(
                    "attempt {attempt} of {max_attempts} failed ({error}); next in {}s",
                    next_delay_ms.div_ceil(1000)
                )),
                current: Some(attempt as u32),
                total: Some(max_attempts as u32),
                fraction: 0.0,
            }),
            crate::StateUpdate::ExternalChange { .. } => {}
        }
    }
}

/// Outcome of a body-backfill pass, plus how much is left.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BodyBackfillResult {
    /// Messages whose shape was recorded by this pass.
    pub shaped: u32,
    /// Messages still without a known shape.
    pub unshaped: u32,
    /// Every message: the denominator the shaping phase counts against.
    ///
    /// Deliberately separate from `total`. The two phases measure different
    /// populations — shaping covers every message, while bodies only concern
    /// those found to have a text part — and sharing one denominator between
    /// them produces nonsense, including negative progress.
    pub messages: u32,
    /// Bodies stored by this pass.
    pub fetched: u32,
    /// Messages the server produced no usable part for.
    pub missing: u32,
    /// Messages still without a body.
    pub pending: u32,
    /// Messages known to have a text part: the body phase's denominator. Grows
    /// as shaping discovers them.
    pub total: u32,
    /// Messages the server described in a way that could not be parsed, and
    /// which are therefore missing from search.
    ///
    /// Reported rather than only logged: these are skipped permanently, and a
    /// silent gap in what is searchable is worse than a visible one.
    pub unreadable: u32,
    /// Messages given a vector by this pass.
    pub embedded: u32,
    /// Messages that still need one, once their text has settled.
    pub embeddings_pending: u32,
    /// HTML renderings stored by this pass, for the newest mail of each folder.
    pub html: u32,
    /// Full sources stored by this pass, for messages with attachments.
    pub sources: u32,
    /// Messages with attachments whose source is still to be fetched.
    pub sources_pending: u32,
    /// Messages whose threading headers this pass fetched.
    pub threaded: u32,
    /// Messages stored before the sync fetched those, still to be asked.
    pub threads_pending: u32,
}

/// Settings the interface needs to honour.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UiSettings {
    /// How long a message must be on screen before it counts as read.
    ///
    /// Zero means never; the explicit `mark-read` command is unaffected.
    pub mark_read_after_ms: u64,
    /// Which rendering a message opens in: `html` or `text`.
    pub default_message_view: String,
    /// The list's columns, in order, where a folder has none of its own.
    pub columns: Vec<String>,
    /// The date column's format, strftime-style, or `auto`.
    pub date_format: String,
    /// The order of a search's mail until another is picked: `relevance`,
    /// or a folder's sort order such as `date_newest`.
    pub search_sort: String,
    /// `compact`, `comfortable` or `spacious`.
    pub density: String,
    /// The From and Date columns' widths from `theme.column_widths`, in
    /// pixels, for a window that has not sized them yet; `None` for auto.
    pub from_width: Option<u32>,
    pub date_width: Option<u32>,
    /// `system`, `dark` or `light`.
    pub theme: String,
    /// The text size in pixels the config asks for.
    pub font_size: f64,
    /// The text size of the compose window's Neovim, or `None` for `font_size`.
    pub editor_font_size: Option<f64>,
    /// Whether HTML mail is drawn dark in a dark window.
    pub dark_html_mail: bool,
    /// `hybrid` or `terminal`.
    pub look: String,
    /// Whether Today lists its completed tasks above the open ones (§24).
    pub completed_first_in_today: bool,
}

/// A view of the sidebar: a saved search, or one of the merged views.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VirtualFolderInfo {
    pub name: String,
    pub icon: Option<String>,
    /// The view as a query in the search language.
    pub query: String,
}

/// Something at the top of the sidebar, in `pinned`'s order. `kind` is
/// `tasks`, `reading`, `chat`, `view` or `folder`; `name` is the task view,
/// the reading view, `waiting` or the view's name; a folder has `account` and
/// `folder` instead.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PinInfo {
    pub kind: String,
    pub name: Option<String>,
    pub account: Option<String>,
    pub folder: Option<String>,
}

/// Messages given a vector per backfill pass.
///
/// Embedding is local and runs at tens of thousands a second, so this is
/// bounded only to keep one pass from holding the write lock for long.
const EMBED_BATCH: usize = 20_000;

/// Sync operation result
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncResult {
    pub folders_synced: u32,
    /// Folders left alone because another sync held their lock; each is
    /// named in `skipped`.
    pub folders_skipped: u32,
    pub messages_added: u32,
    pub messages_updated: u32,
    pub messages_deleted: u32,
    /// New messages a filter rule acted on.
    pub messages_filtered: u32,
    /// The unread mail the sync brought to the watched folders, as it stands
    /// after the filter rules ran: what to announce, oldest first.
    pub new_mail: Vec<NewMailItem>,
    pub errors: Vec<String>,
    /// What a filter rule could not do.
    pub filter_warnings: Vec<String>,
    pub skipped: Vec<String>,
    /// The errors again, each with its account and kind, for the account
    /// status.
    pub failures: Vec<SyncFailure>,
}

impl From<RustSyncReport> for SyncResult {
    fn from(report: RustSyncReport) -> Self {
        SyncResult {
            folders_synced: report.folders_synced as u32,
            folders_skipped: report.folders_skipped as u32,
            skipped: report.skipped,
            messages_added: report.messages_added as u32,
            messages_updated: report.messages_updated as u32,
            messages_deleted: report.messages_deleted as u32,
            messages_filtered: report.messages_filtered as u32,
            new_mail: report.new_mail.into_iter().map(Into::into).collect(),
            errors: report.errors,
            filter_warnings: report.filter_warnings,
            failures: report.failures,
        }
    }
}

/// Input for sending an email from the UI.
///
/// Carries the sender account (by configured account name), the recipient
/// lists, the message content, and optional threading headers for replies
/// (`In-Reply-To` / `References`). `original_message_id` is the local database
/// id of the message being replied to/forwarded, retained so callers can
/// correlate the send with the source message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SendMailInput {
    /// Name of the configured account to send from (matches `Account.name`).
    pub account_name: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// Message-ID of the message being replied to (for the In-Reply-To header).
    pub in_reply_to: Option<String>,
    /// References header value for threading.
    pub references: Option<String>,
    /// Message-ID of the message being forwarded.
    #[serde(default)]
    pub forwards: Option<String>,
    /// Local database id of the message being replied to/forwarded, if any.
    pub original_message_id: Option<i64>,
    /// Filesystem paths of files the user picked to attach. Each is read from
    /// disk at send time; its content type is inferred from the extension.
    pub attachment_paths: Vec<String>,
    /// The identity of the account to write as; the default one when absent.
    #[serde(default)]
    pub identity: Option<String>,
    /// A `YYYY-MM-DD` date to follow the message up on: once it is sent, or
    /// queued, a task linked to it is made, postponed to that date.
    #[serde(default)]
    pub follow_up: Option<String>,
}

/// Result of a send operation.
///
/// `sent` is true once the SMTP send succeeded; `queued` when the mail server
/// could not be reached and the message waits in the outbox instead, to go
/// out with the next sync that finds the server. `saved_to_sent` reflects
/// whether the copy was also appended to the account's Sent folder over IMAP;
/// a failed append is non-fatal (the mail was still delivered) and is
/// surfaced via `warning`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SendMailResult {
    pub sent: bool,
    pub queued: bool,
    pub saved_to_sent: bool,
    pub sent_folder: String,
    pub warning: Option<String>,
    /// The task made to follow the message up, when one was asked for.
    #[serde(default)]
    pub follow_up: Option<TaskDetail>,
}

/// Build a `smtp::OutgoingEmail` from FFI send input.
///
/// Pure and side-effect free so the recipient/threading-header mapping can be
/// unit-tested without an account or a live SMTP connection.
fn outgoing_email_from_input(input: &SendMailInput) -> crate::smtp::OutgoingEmail {
    crate::smtp::OutgoingEmail {
        to: input.to.clone(),
        cc: input.cc.clone(),
        bcc: input.bcc.clone(),
        subject: input.subject.clone(),
        body: input.body.clone(),
        in_reply_to: input.in_reply_to.clone(),
        references: input.references.clone(),
        forwards: input.forwards.clone(),
        // Set from the identity, which needs the account — see
        // `apply_identity`.
        from: None,
        reply_to: None,
        // Attachments are read from disk separately (I/O) — see
        // `read_attachments`, called from `send_mail`.
        attachments: Vec::new(),
        message_id: None,
    }
}

/// A draft, as [`OrbHandle::draft_warnings`] checks it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DraftWarningsInput {
    pub account_name: String,
    /// The identity it is written as; the account's default when absent.
    pub identity: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub attachment_paths: Vec<String>,
    /// The placeholders of the mail templates used in it, checked for still
    /// being written in braces.
    pub placeholders: Vec<String>,
    /// From the draft's template: where the original went.
    pub addressed_to: Vec<String>,
    pub delivered_to: Vec<String>,
}

/// The templates a draft may use, as [`OrbHandle::templates`] lists them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TemplatesForDraft {
    /// Where they are read from.
    pub folder: String,
    pub templates: Vec<TemplateForDraft>,
    pub problems: Vec<crate::templates::TemplateProblem>,
}

/// A template, with what is known for its placeholders.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TemplateForDraft {
    #[serde(flatten)]
    pub template: crate::templates::Template,
    /// Its body as it will be written, placeholders marked.
    pub expanded: crate::templates::Expanded,
    pub suggestions: std::collections::HashMap<String, String>,
}

/// A template ready to go into a draft; see [`OrbHandle::use_template`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TemplateUse {
    pub name: String,
    pub subject: Option<crate::templates::Expanded>,
    pub body: crate::templates::Expanded,
    pub placeholders: Vec<String>,
    pub suggestions: std::collections::HashMap<String, String>,
}

/// One identity, as the compose window lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdentityInfo {
    /// The account it goes out of.
    pub account: String,
    pub name: String,
    pub from: String,
    /// Its signature as a draft carries it, from the `-- ` line on; `None`
    /// when it has none.
    pub signature: Option<String>,
}

/// Write `email` as the identity `input` names.
///
/// An identity that is no longer configured — renamed while a draft was open —
/// stops a send, which would otherwise go out from an address nobody chose,
/// but not a save, which falls back to the default rather than lose the text.
fn apply_identity(
    account: &crate::config::AccountConfig,
    input: &SendMailInput,
    email: &mut crate::smtp::OutgoingEmail,
    sending: bool,
) -> Result<(), OrbError> {
    let Some(name) = input.identity.as_deref() else {
        return Ok(());
    };
    let Some(identity) = account.identity(name) else {
        if sending {
            return Err(OrbError::SendError {
                message: format!(
                    "the identity '{name}' is not configured for account '{}'",
                    account.name
                ),
            });
        }
        crate::logger::warn(&format!(
            "{}: saving a draft as the default identity, '{name}' is not configured",
            account.name
        ));
        return Ok(());
    };
    email.from = Some(identity.from);
    email.reply_to = identity.reply_to;
    Ok(())
}

/// Read each picked file into an [`EmailAttachment`], inferring its MIME type
/// from the filename. A file that cannot be read aborts the send with a
/// `SendError` (so the user learns their attachment is missing rather than
/// silently sending without it).
fn read_attachments(paths: &[String]) -> Result<Vec<crate::smtp::EmailAttachment>, OrbError> {
    paths
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|e| OrbError::SendError {
                message: format!("Failed to read attachment '{}': {}", path, e),
            })?;
            let filename = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment")
                .to_string();
            let content_type = crate::attachments::guess_content_type(&filename);
            Ok(crate::smtp::EmailAttachment {
                filename,
                content_type,
                bytes,
            })
        })
        .collect()
}

/// Keyboard bindings configuration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BindingsConfig {
    pub index_normal: Option<HashMap<String, String>>,
    pub index_detail: Option<HashMap<String, String>>,
    pub compose_normal: Option<HashMap<String, String>>,
    pub compose_edit: Option<HashMap<String, String>>,
    pub command: Option<HashMap<String, String>>,
    pub task_normal: Option<HashMap<String, String>>,
    pub task_detail: Option<HashMap<String, String>>,
    /// Only the keys bound here are intercepted while the body editor is
    /// running; everything else goes to the editor.
    pub task_detail_terminal: Option<HashMap<String, String>>,
    /// The task title/body editor.
    pub task_edit: Option<HashMap<String, String>>,
    /// The date picker (§12). Bare characters are ignored here by design — the
    /// text input owns them, since `s`, `d` and `3` are the parser's vocabulary
    /// — so only bracketed keys have any effect.
    pub date_picker: Option<HashMap<String, String>>,
    /// The chat list and conversation, over `index_normal`.
    pub chat_normal: Option<HashMap<String, String>>,
    /// The chat composer. Only bracketed keys take effect: the text field
    /// owns the rest.
    pub chat_compose: Option<HashMap<String, String>>,
    /// The user's own commands, by name.
    pub commands: Option<HashMap<String, UserCommandInfo>>,
}

/// A command of the user's own: what it runs, and its title in the palette.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserCommandInfo {
    pub run: String,
    pub title: Option<String>,
}

impl From<&RustBindingsConfig> for BindingsConfig {
    fn from(bc: &RustBindingsConfig) -> Self {
        fn non_empty(map: &HashMap<String, String>) -> Option<HashMap<String, String>> {
            if map.is_empty() {
                None
            } else {
                Some(map.clone())
            }
        }

        BindingsConfig {
            index_normal: non_empty(&bc.index_normal),
            index_detail: non_empty(&bc.index_detail),
            compose_normal: non_empty(&bc.compose_normal),
            compose_edit: non_empty(&bc.compose_edit),
            command: non_empty(&bc.command),
            task_normal: non_empty(&bc.task_normal),
            task_detail: non_empty(&bc.task_detail),
            task_detail_terminal: non_empty(&bc.task_detail_terminal),
            task_edit: non_empty(&bc.task_edit),
            date_picker: non_empty(&bc.date_picker),
            chat_normal: non_empty(&bc.chat_normal),
            chat_compose: non_empty(&bc.chat_compose),
            commands: (!bc.commands.is_empty()).then(|| {
                bc.commands
                    .iter()
                    .map(|(name, command)| {
                        let info = UserCommandInfo {
                            run: command.run().to_string(),
                            title: command.title().map(str::to_string),
                        };
                        (name.clone(), info)
                    })
                    .collect()
            }),
        }
    }
}

// ============================================================================
// Tasks
// ============================================================================

/// One of the five task views (§4.1), as the interface names it.
///
/// Mirrors [`crate::tasks::TaskView`] across the boundary for the same reason
/// [`SearchScope`] mirrors its core type: the wire enum is part of the contract
/// with the UI and must not move every time the core one is refactored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum TaskViewName {
    Today,
    Upcoming,
    Someday,
    Completed,
    Archive,
    Trash,
}

impl From<TaskViewName> for TaskView {
    fn from(view: TaskViewName) -> Self {
        match view {
            TaskViewName::Today => TaskView::Today,
            TaskViewName::Upcoming => TaskView::Upcoming,
            TaskViewName::Someday => TaskView::Someday,
            TaskViewName::Completed => TaskView::Completed,
            TaskViewName::Archive => TaskView::Archive,
            TaskViewName::Trash => TaskView::Trash,
        }
    }
}

/// A tag as the UI draws it.
///
/// `is_known` is false when the task carries a tag id the registry cannot
/// resolve. §7 is explicit that such an id renders as a placeholder rather than
/// being dropped: unresolvable is not the same as deleted, and a registry that
/// has not synced yet must not silently strip every tag off every task.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskTagInfo {
    /// The tag's uuid — its identity, and what task messages carry.
    pub id: String,
    pub name: String,
    pub colour: Option<String>,
    pub position: f64,
    pub is_known: bool,
}

impl From<&TaskTag> for TaskTagInfo {
    fn from(tag: &TaskTag) -> Self {
        TaskTagInfo {
            id: tag.tag_uuid.clone(),
            name: tag.name.clone(),
            colour: tag.colour.clone(),
            position: tag.position,
            is_known: true,
        }
    }
}

/// Resolve a task's tag ids against the registry, keeping unknown ones visible.
fn resolve_tags(ids: &[String], registry: &[TaskTag]) -> Vec<TaskTagInfo> {
    ids.iter()
        .map(|id| match registry.iter().find(|tag| &tag.tag_uuid == id) {
            Some(tag) => TaskTagInfo::from(tag),
            None => TaskTagInfo {
                id: id.clone(),
                // `tag_label` yields the id itself for an unknown tag, which is
                // exactly the placeholder §7 asks for.
                name: tag_label(id, registry).to_string(),
                colour: None,
                position: f64::MAX,
                is_known: false,
            },
        })
        .collect()
}

/// A task as a list row.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskSummary {
    pub id: i64,
    /// The task's stable identity (§2), which is what a notification click and
    /// a cross-machine reference carry — never the row id.
    pub uuid: String,
    pub title: String,
    /// `YYYY-MM-DD`, local civil dates both.
    pub postpone_date: Option<String>,
    pub due_date: Option<String>,
    pub completed_at: Option<String>,
    pub deleted_at: Option<String>,
    pub tags: Vec<TaskTagInfo>,
    pub is_recurring: bool,
    pub position: f64,
    pub is_working_on: bool,
    pub is_completed: bool,
    /// Past its due date and still open. Kept separate from `due_date` so the
    /// list does not have to know what today is to draw an overdue task
    /// differently (§4.1).
    pub is_overdue: bool,
    /// Not on the server as it is here: edited since the last upload (§3.2),
    /// or never uploaded at all. The window marks it until it is (§23).
    pub is_dirty: bool,
    pub linked_email_count: u32,
    pub reminder_count: u32,
    pub attachment_count: u32,
    /// The checklist of §17: how many steps, and how many are done.
    pub steps_total: u32,
    pub steps_done: u32,
    /// A repeating task completed today, shown done in Today until midnight
    /// though it is open again for its next date.
    #[serde(default)]
    pub is_done_for_today: bool,
}

impl TaskSummary {
    fn from_item(item: TaskListItem, today: chrono::NaiveDate, registry: &[TaskTag]) -> Self {
        TaskSummary {
            is_overdue: item.is_overdue(today),
            id: item.id,
            uuid: item.task_uuid,
            title: item.subject,
            postpone_date: item.postpone_date,
            due_date: item.due_date,
            completed_at: item.completed_at,
            deleted_at: item.deleted_at,
            tags: resolve_tags(&item.tags, registry),
            is_recurring: item.is_recurring,
            position: item.position,
            is_working_on: item.is_working_on,
            is_completed: item.is_completed,
            is_dirty: item.is_dirty || item.imap_uid.is_none(),
            linked_email_count: item.linked_email_count as u32,
            reminder_count: item.reminder_count as u32,
            attachment_count: item.attachment_count as u32,
            steps_total: item.steps_total,
            steps_done: item.steps_done,
            is_done_for_today: item.is_done_for_today,
        }
    }

    /// The same row, built from a whole task rather than a list projection.
    ///
    /// The three counts come from the collections the detail view already
    /// loaded, so opening a task does not repeat the list's aggregate queries.
    fn from_task(
        task: &Task,
        today: chrono::NaiveDate,
        registry: &[TaskTag],
        links: usize,
        reminders: usize,
        attachments: usize,
    ) -> Self {
        TaskSummary {
            id: task.id.unwrap_or_default(),
            uuid: task.task_uuid.clone(),
            title: task.subject.clone(),
            postpone_date: task.postpone_date.clone(),
            due_date: task.due_date.clone(),
            completed_at: task.completed_at.clone(),
            deleted_at: task.deleted_at.clone(),
            tags: resolve_tags(&task.tags, registry),
            is_recurring: task.recurrence.is_some(),
            position: task.position,
            is_working_on: task.is_working_on,
            is_completed: task.is_completed,
            is_overdue: task.is_overdue(today),
            is_dirty: task.is_dirty || task.imap_uid.is_none(),
            linked_email_count: links as u32,
            reminder_count: reminders as u32,
            attachment_count: attachments as u32,
            steps_total: task.steps.len() as u32,
            steps_done: task.steps.iter().filter(|step| step.done).count() as u32,
            // Only a view knows; this summary stands outside one.
            is_done_for_today: false,
        }
    }
}

/// One reading of what the user typed into the date picker (§12.1).
///
/// The date and time cross the boundary as strings because chrono's `serde`
/// feature is not enabled in this workspace, and because a civil date is not an
/// instant: serialising it as one would invite the frontend to re-interpret it
/// in the WebView's timezone, which is the whole class of bug §4.3 exists to
/// prevent. `YYYY-MM-DD` and `HH:MM` mean exactly what they say, locally.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DateSuggestionInfo {
    /// The resolved local civil date, `YYYY-MM-DD`.
    pub date: String,
    /// The resolved local wall-clock time, `HH:MM`. `None` for a date-only
    /// suggestion, always set for a reminder one.
    pub time: Option<String>,
    /// Left-aligned label, e.g. "Tomorrow" or "September 12, 2028".
    pub primary: String,
    /// Right-aligned label in a lighter colour, e.g. "Aug 7" or "in 5 weeks".
    pub secondary: String,
}

impl From<&crate::tasks::DateSuggestion> for DateSuggestionInfo {
    fn from(suggestion: &crate::tasks::DateSuggestion) -> Self {
        DateSuggestionInfo {
            date: suggestion.date.format("%Y-%m-%d").to_string(),
            time: suggestion.time.map(|time| time.format("%H:%M").to_string()),
            primary: suggestion.primary.clone(),
            secondary: suggestion.secondary.clone(),
        }
    }
}

/// A reminder on a task (§5).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskReminderInfo {
    pub id: i64,
    /// Local date and time, RFC3339.
    pub remind_at: String,
    pub remind_at_unix: i64,
    /// Whether *this machine* has fired it. Local per machine and deliberately
    /// not synced (§5.2).
    pub is_triggered: bool,
    /// The identifier this reminder is scheduled under with the OS, so the
    /// platform layer can correlate a delivered notification with a row.
    pub notification_id: String,
}

impl TaskReminderInfo {
    fn new(reminder: &TaskReminder, task_uuid: &str) -> Self {
        TaskReminderInfo {
            id: reminder.id.unwrap_or_default(),
            remind_at: reminder.remind_at.clone(),
            remind_at_unix: reminder.remind_at_unix,
            is_triggered: reminder.is_triggered,
            notification_id: reminder.notification_id(task_uuid),
        }
    }
}

/// A message linked to a task, as the task remembers it (§6.1).
///
/// Everything here is the *cache*, not the message: it is what the task shows
/// when the message has been moved, deleted or lost with its account. Resolving
/// it to something openable is [`OrbHandle::linked_message`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskLinkInfo {
    pub message_id: String,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub date: Option<String>,
    pub account_name: Option<String>,
    /// Where the message was last seen — the first folder the server search of
    /// §6.2 tries.
    pub folder_name: Option<String>,
    /// Whether the message was answered: `\Answered` on a stored copy, or a
    /// reply to it in a Sent folder.
    pub answered: bool,
    /// When the user last replied to it, where the reply is in a Sent folder.
    pub replied_at: Option<String>,
    /// Set when the link is to a chat message rather than to mail (todo-spec
    /// §28). The cached fields then hold its text (`subject`), author
    /// (`from`), room (`account_name`) and permalink (`folder_name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<ChatLinkInfo>,
}

/// Where a chat message a task links lives.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChatLinkInfo {
    pub rid: String,
    /// The thread it is in; empty for a room's own timeline.
    pub thread: String,
    pub message: String,
}

impl From<&TaskLinkedEmail> for TaskLinkInfo {
    fn from(link: &TaskLinkedEmail) -> Self {
        TaskLinkInfo {
            message_id: link.message_id.clone(),
            subject: link.cached_subject.clone(),
            from: link.cached_from.clone(),
            date: link.cached_date.clone(),
            account_name: link.cached_account.clone(),
            folder_name: link.cached_folder.clone(),
            answered: false,
            replied_at: None,
            chat: crate::chat::ChatLink::parse(&link.message_id).map(|chat| ChatLinkInfo {
                rid: chat.rid,
                thread: chat.thread,
                message: chat.message,
            }),
        }
    }
}

/// A link with whether, and when, the message it names was answered. A
/// lookup that fails leaves the link as "no reply": the detail still opens.
fn with_reply_status(store: &crate::store::Store, mut link: TaskLinkInfo) -> TaskLinkInfo {
    // A chat message is answered by writing in its thread, or in its room
    // outside any thread, after it.
    if let Some(chat) = &link.chat {
        let after = link
            .date
            .as_deref()
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
            .map_or(i64::MAX, |d| d.timestamp_millis());
        let replied = store.reader().ok().and_then(|db| {
            crate::chat::store::my_reply_after(&db.conn, &chat.rid, &chat.thread, after)
                .ok()
                .flatten()
        });
        link.replied_at = replied.map(crate::chat::client::iso);
        link.answered = replied.is_some();
        return link;
    }
    link.replied_at = store.last_reply_to(&link.message_id).unwrap_or_default();
    link.answered = link.replied_at.is_some()
        || store
            .find_messages_by_rfc_id(&link.message_id)
            .unwrap_or_default()
            .iter()
            .any(|item| {
                item.message
                    .flags
                    .iter()
                    .any(|flag| flag.eq_ignore_ascii_case("\\Answered"))
            });
    link
}

/// An attachment on a task (§9). The payload is fetched separately and lazily.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskAttachmentInfo {
    pub id: i64,
    /// The attachment's own identity, which is how its payload message is found
    /// on the server and how its cache file is named.
    pub attachment_uuid: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    /// Whether the payload is already in the local cache, so the UI can tell an
    /// instant open from one that needs the network.
    pub is_cached: bool,
}

impl From<&TaskAttachment> for TaskAttachmentInfo {
    fn from(attachment: &TaskAttachment) -> Self {
        TaskAttachmentInfo {
            id: attachment.id.unwrap_or_default(),
            attachment_uuid: attachment.attachment_uuid.clone(),
            filename: attachment.filename.clone(),
            content_type: attachment.content_type.clone(),
            size: attachment.size,
            is_cached: attachment.is_cached,
        }
    }
}

/// One entry of a task's timeline (§11).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskLogItem {
    /// The entry's own identity, which is what makes the union merge across
    /// devices work and what lets a UI key a list without re-rendering it.
    pub entry_uuid: String,
    pub occurred_at: String,
    pub occurred_at_unix: i64,
    /// Which machine appended it.
    pub device_id: Option<String>,
    /// Machine-readable kind, e.g. `date_changed`.
    pub event_type: String,
    /// The rendered line.
    pub text: String,
}

impl From<&TaskLogEntry> for TaskLogItem {
    fn from(entry: &TaskLogEntry) -> Self {
        TaskLogItem {
            entry_uuid: entry.entry_uuid.clone(),
            occurred_at: entry.occurred_at.clone(),
            occurred_at_unix: entry.occurred_at_unix,
            device_id: entry.device_id.clone(),
            event_type: entry.event_type().to_string(),
            text: entry.rendered_text(),
        }
    }
}

/// What merging tasks into one would make of them, for the dialog that
/// confirms it: its proposals can each be changed there.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskMergePlan {
    /// The task that stays, then the others, as asked.
    pub tasks: Vec<TaskSummary>,
    /// The kept task's notes, then each other task's title and notes, so no
    /// title is lost.
    pub body: Option<String>,
    /// The earliest postpone and due date among them, `YYYY-MM-DD`.
    pub postpone_date: Option<String>,
    pub due_date: Option<String>,
    /// What comes along: every step, tag, reminder still to come, linked
    /// message and attachment of them all.
    pub steps: u32,
    pub tags: u32,
    pub reminders: u32,
    pub links: u32,
    pub attachments: u32,
}

/// A task with everything the detail view shows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskDetail {
    pub task: TaskSummary,
    pub body: Option<String>,
    /// The `RRULE` (§8), canonical form.
    pub recurrence: Option<String>,
    /// The rule in words, in the configured language.
    pub recurrence_label: Option<String>,
    /// Advance from the completion date rather than the scheduled one.
    pub recurrence_from_completion: bool,
    pub created_at: String,
    pub updated_at: String,
    pub revision: i64,
    pub account_name: String,
    pub reminders: Vec<TaskReminderInfo>,
    pub links: Vec<TaskLinkInfo>,
    pub attachments: Vec<TaskAttachmentInfo>,
    pub log: Vec<TaskLogItem>,
    /// The checklist of §17, in order.
    pub steps: Vec<TaskStepInfo>,
}

/// What a task export did (todo-spec §20).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskExportResult {
    /// The snapshot's directory.
    pub path: String,
    /// False when a payload could not be fetched; `missing` says which.
    pub complete: bool,
    pub tasks: u32,
    pub attachments: u32,
    pub missing: Vec<String>,
    /// The snapshot's size on disk.
    pub bytes: u64,
    /// How many older snapshots retention removed.
    pub removed: u32,
}

/// Where the task export stands (todo-spec §20.3), for the status line.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskExportStatus {
    /// Whether `tasks.export` is set at all.
    pub configured: bool,
    pub directory: Option<String>,
    /// When the newest complete snapshot was taken, RFC 3339.
    pub last_complete: Option<String>,
    /// An export is due now.
    pub due: bool,
    /// No complete snapshot younger than twice the interval: the warning
    /// that stays until there is one.
    pub overdue: bool,
    /// What the last export this run said, if it failed or was incomplete.
    pub problem: Option<String>,
}

/// The size of a directory's files, for the diagnostics.
fn dir_bytes(root: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// What importing a snapshot does, or did (todo-spec §21).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TaskImportPlan {
    pub snapshot: String,
    pub exported_at: String,
    /// Not on this machine: created at their own ids.
    pub new: u32,
    /// Newer in the snapshot than here: replace the local copy.
    pub newer: u32,
    /// As new or newer here: only their logs are merged.
    pub older: u32,
    /// Purged here and so left out, unless asked otherwise; by title.
    pub purged: Vec<String>,
    /// Tags the registry gains.
    pub tags: u32,
    /// What cannot be imported, and why.
    pub problems: Vec<String>,
    /// Whether this is what was done rather than what would be.
    pub applied: bool,
}

/// What importing from Things 3 does, or did (todo-spec §22).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ThingsImportPlan {
    pub database: String,
    /// To-dos that become tasks.
    pub tasks: u32,
    /// Of those, completed or cancelled in Things.
    pub completed: u32,
    /// Of those, repeating with a rule Orb expresses.
    pub repeating: u32,
    /// Repeating with a rule Orb cannot express: imported undated, by title.
    pub unmapped: Vec<String>,
    /// Brought over by an earlier import, and left alone.
    pub already: u32,
    /// Left out: in the Things trash, projects and headings as such, open
    /// instances of a repeating to-do (the to-do itself stands for them).
    pub trashed: u32,
    pub projects: u32,
    pub headings: u32,
    pub repeat_instances: u32,
    /// Tags created, by name.
    pub new_tags: Vec<String>,
    /// Brought over by an earlier import and changed in Things since: updated.
    pub updated: u32,
    /// Changed in Things, but edited in Orb since they came over, and so left
    /// alone, by title.
    pub edited_here: Vec<String>,
    /// Of those, how many had a placeholder date of Things' cleared.
    pub repaired: u32,
    /// In the Things trash and unedited here: moved to Orb's trash.
    pub trashed_here: u32,
    pub applied: bool,
}

/// The name the export takes the sync lock under: one export at a time.
const EXPORT_LOCK: &str = "orb:task-export";

/// A step of a task (§17).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskStepInfo {
    pub id: String,
    pub title: String,
    pub done: bool,
}

/// One change to a task's checklist (§17).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StepEdit {
    Add {
        title: String,
    },
    /// Several at once — a selection — as one edit and one undo.
    Done {
        ids: Vec<String>,
        done: bool,
    },
    Rename {
        id: String,
        title: String,
    },
    /// Several at once — a selection — as one edit and one undo.
    Remove {
        ids: Vec<String>,
    },
    /// The steps, in checklist order and side by side, to `to` in the
    /// checklist as it reads once they are taken out.
    Move {
        ids: Vec<String>,
        to: usize,
    },
}

/// Minutes from now until nine tomorrow morning, local time: what "snooze
/// until tomorrow" on a reminder means.
pub fn minutes_until_tomorrow_nine() -> i64 {
    let now = chrono::Local::now();
    let tomorrow = (now + chrono::Duration::days(1)).date_naive();
    let target = tomorrow
        .and_hms_opt(9, 0, 0)
        .and_then(|naive| naive.and_local_timezone(chrono::Local).single())
        .unwrap_or(now + chrono::Duration::hours(24));
    (target - now).num_minutes().max(1)
}

/// A reading of a recurrence typed by a person: the rule and its words.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RecurrenceSuggestionInfo {
    pub rrule: String,
    pub label: String,
}

/// An edit to a task, as the UI sends it.
///
/// Deliberately flat, with an explicit `clear_*` flag beside every nullable
/// field. The core's [`TaskUpdates`] distinguishes "leave alone" from "set to
/// nothing" with `Option<Option<_>>`, which JSON cannot express: `null` and a
/// missing key both deserialise to `None`. Silently turning "clear the due
/// date" into "leave the due date alone" is exactly the class of bug this
/// feature cannot afford, so the distinction is carried by a second field
/// rather than by a subtlety of the encoding.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TaskEdit {
    pub title: Option<String>,
    pub body: Option<String>,
    pub clear_body: bool,
    /// `YYYY-MM-DD`.
    pub postpone_date: Option<String>,
    pub clear_postpone_date: bool,
    /// `YYYY-MM-DD`.
    pub due_date: Option<String>,
    pub clear_due_date: bool,
    /// The complete new set of tag ids.
    pub tags: Option<Vec<String>>,
    /// An `RRULE` (§8).
    pub recurrence: Option<String>,
    pub clear_recurrence: bool,
    pub recurrence_from_completion: Option<bool>,
    pub is_working_on: Option<bool>,
}

impl TaskEdit {
    /// Translate into the core's update type.
    ///
    /// The dates are validated here rather than at the database: a picker that
    /// somehow produced `2026-02-31` must be told so, not have it stored and
    /// discovered a year later when the task fails to appear.
    fn into_updates(self) -> Result<TaskUpdates, OrbError> {
        let mut updates = TaskUpdates::new();
        if let Some(title) = self.title {
            updates = updates.with_subject(title);
        }
        if self.clear_body {
            updates = updates.with_body_text(None);
        } else if let Some(body) = self.body {
            updates = updates.with_body_text(Some(body));
        }
        if self.clear_postpone_date {
            updates = updates.with_postpone_date(None);
        } else if let Some(date) = self.postpone_date.as_deref() {
            updates = updates.with_postpone_date(Some(parse_task_date(date)?));
        }
        if self.clear_due_date {
            updates = updates.with_due_date(None);
        } else if let Some(date) = self.due_date.as_deref() {
            updates = updates.with_due_date(Some(parse_task_date(date)?));
        }
        if let Some(tags) = self.tags {
            updates = updates.with_tags(tags);
        }
        if self.clear_recurrence {
            updates.recurrence = Some(None);
        } else if let Some(rule) = self.recurrence {
            updates.recurrence = Some(Some(rule));
        }
        if let Some(from_completion) = self.recurrence_from_completion {
            updates = updates.with_recurrence_from_completion(from_completion);
        }
        if let Some(working_on) = self.is_working_on {
            updates = updates.with_is_working_on(working_on);
        }
        Ok(updates)
    }
}

/// Parse a `YYYY-MM-DD` civil date, refusing anything else.
fn parse_task_date(date: &str) -> Result<chrono::NaiveDate, OrbError> {
    crate::tasks::parse_civil_date(date).ok_or_else(|| OrbError::InvalidInput {
        message: format!("'{date}' is not a date (expected YYYY-MM-DD)"),
    })
}

/// How a task's linked message was found (§6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkResolution {
    /// Step 1: found in the local index. Instant.
    Local,
    /// Step 2: found by searching the server, and now synced locally.
    Server,
    /// Step 3: not found anywhere. The cached metadata is all there is, and the
    /// UI shows it read-only, marked "message no longer available".
    Cached,
}

/// Where a task's linked message turned out to be.
///
/// Never an error: a message that is gone forever is a normal outcome, and §6.2
/// is explicit that it must not break the task.
/// How many messages' headers a pass of [`OrbHandle::fetch_bodies`] fetches
/// for each body it is allowed.
const HEADER_BUDGET_FACTOR: usize = 10;

/// Where a message stands in a threaded list, from [`OrbHandle::thread_row`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThreadPlace {
    /// The row, counted from 0, with the thread expanded when `expand` says.
    pub row: u32,
    /// The thread's key, as the list names an expanded thread.
    pub thread: String,
    /// Whether the thread has members to show under its row.
    pub expand: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkedMessage {
    /// The RFC `Message-ID` that was resolved.
    pub message_id: String,
    pub resolution: LinkResolution,
    /// The local row id to open, when there is one.
    pub local_id: Option<i64>,
    pub account_name: Option<String>,
    pub folder_name: Option<String>,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub date: Option<String>,
}

impl LinkedMessage {
    /// The cached fallback: everything the link remembers and nothing to open.
    fn cached(message_id: String, link: Option<&TaskLinkedEmail>) -> Self {
        LinkedMessage {
            message_id,
            resolution: LinkResolution::Cached,
            local_id: None,
            account_name: link.and_then(|l| l.cached_account.clone()),
            folder_name: link.and_then(|l| l.cached_folder.clone()),
            subject: link.and_then(|l| l.cached_subject.clone()),
            from: link.and_then(|l| l.cached_from.clone()),
            date: link.and_then(|l| l.cached_date.clone()),
        }
    }

    /// A real, openable message.
    fn found(item: &crate::models::SearchResultItem, resolution: LinkResolution) -> Self {
        LinkedMessage {
            message_id: item.message.message_id.clone().unwrap_or_default(),
            resolution,
            local_id: Some(item.message.id),
            account_name: Some(item.account_name.clone()),
            folder_name: Some(item.folder_name.clone()),
            subject: item.message.subject.clone(),
            from: item.message.from.clone(),
            date: item.message.date.clone(),
        }
    }
}

/// How many open tasks are linked to one message (§6.3).
///
/// A list rather than a map, and one that omits zeroes: the message list draws
/// nothing at all for a message with no tasks, never a "0".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageTaskCount {
    /// Local message row id, as the list knows it.
    pub message_id: i64,
    pub open_tasks: u32,
}

/// What one task sync did.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskSyncResult {
    pub listed: u32,
    pub downloaded: u32,
    pub uploaded: u32,
    pub deleted: u32,
    pub purged: u32,
    pub expunged: u32,
    pub rebound: u32,
    pub attachments_uploaded: u32,
    /// Whether the folder's `UIDVALIDITY` had changed and every cached UID was
    /// therefore discarded (§3.4). Nothing was lost; the UI may want to say so.
    pub uidvalidity_reset: bool,
    /// Messages in the folder this build cannot read — a version-1 wire format,
    /// or something else's mail. Left untouched, and reported rather than only
    /// logged: a silent gap in the task list is the failure this spec exists to
    /// prevent.
    pub unreadable: u32,
    pub errors: Vec<String>,
}

impl From<TaskSyncReport> for TaskSyncResult {
    fn from(report: TaskSyncReport) -> Self {
        TaskSyncResult {
            listed: report.listed as u32,
            downloaded: report.downloaded as u32,
            uploaded: report.uploaded as u32,
            deleted: report.deleted as u32,
            purged: report.purged as u32,
            expunged: report.expunged as u32,
            rebound: report.rebound as u32,
            attachments_uploaded: report.attachments_uploaded as u32,
            uidvalidity_reset: report.uidvalidity_reset,
            unreadable: report.unreadable as u32,
            errors: report.errors,
        }
    }
}

/// What the OS should be told about pending reminder notifications (§5.1).
///
/// The plan is data, so a platform layer that cannot be reached from the core —
/// `UNUserNotificationCenter` lives in Swift — can ask for it, apply it and
/// report back. Cancel before scheduling: a notification withdrawn and not
/// replaced misfires once and is repaired by the next reconcile, whereas
/// scheduling first leaves a moved reminder armed at both times.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskNotificationPlan {
    pub schedule: Vec<ScheduledNotification>,
    pub cancel: Vec<String>,
    /// Already pending and still wanted. Untouched — and the field that proves
    /// a reconcile converged rather than rewriting everything every time.
    pub unchanged: Vec<String>,
    /// More reminders were eligible than the OS has slots for, so the window
    /// has to be re-filled sooner than the next local midnight.
    pub capped: bool,
}

impl From<crate::tasks::NotificationPlan> for TaskNotificationPlan {
    fn from(plan: crate::tasks::NotificationPlan) -> Self {
        TaskNotificationPlan {
            schedule: plan.schedule,
            cancel: plan.cancel,
            unchanged: plan.unchanged,
            capped: plan.capped,
        }
    }
}

/// A reminder that came due while nobody was watching (§5.2).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DueTaskReminder {
    pub task_id: i64,
    pub task_uuid: String,
    pub title: String,
    pub remind_at: String,
    pub notification_id: String,
}

/// The launch sweep's result.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TaskCatchUp {
    /// Less than 24 hours overdue: deliver each of these.
    pub due: Vec<DueTaskReminder>,
    /// Older than that: one summary line, never a year of notifications.
    pub missed_summary: Option<String>,
    pub missed_count: u32,
}

/// Applies a reminder notification plan to the operating system.
///
/// Implemented by the platform layer, because nothing in the core can reach
/// `UNUserNotificationCenter`. Registered once with
/// [`OrbHandle::set_notification_host`], after which every task sync reconciles
/// the OS's pending set without the UI having to remember to (§5.1).
///
/// Deliberately infallible: a host that cannot schedule has a problem the core
/// cannot act on, and turning it into a sync failure would mean a notification
/// permission the user never granted stops their tasks from syncing.
pub trait TaskNotificationHost: Send + Sync + 'static {
    /// The identifiers the OS is currently holding pending.
    fn pending_identifiers(&self) -> Vec<String>;
    /// Add a request. Adding one whose identifier is already pending replaces
    /// it, which is what makes a repeated schedule harmless.
    fn schedule(&self, notification: ScheduledNotification);
    /// Withdraw pending requests.
    fn cancel(&self, identifiers: Vec<String>);
}

/// Adapts the FFI-facing host to the fallible trait the reminder service uses.
struct HostBridge<'a>(&'a dyn TaskNotificationHost);

impl NotificationHost for HostBridge<'_> {
    fn pending_identifiers(&self) -> crate::Result<Vec<String>> {
        Ok(self.0.pending_identifiers())
    }

    fn schedule(&self, notification: &ScheduledNotification) -> crate::Result<()> {
        self.0.schedule(notification.clone());
        Ok(())
    }

    fn cancel(&self, identifiers: &[String]) -> crate::Result<()> {
        self.0.cancel(identifiers.to_vec());
        Ok(())
    }
}

// ============================================================================
// OrbHandle - Main FFI Interface
// ============================================================================

/// Main handle for interacting with the Orb email system
///
/// This object wraps the Store and Config, providing a thread-safe
/// interface for the Swift UI layer.
///
/// The store is shared rather than locked. `Store` holds no mutable state of
/// its own — every field is an `Arc` — and the operations that need
/// serializing already do it themselves: writes take the store's write lock,
/// and a sync takes a per-folder lock in the database that also guards against
/// a second Orb instance. Wrapping it in a mutex on top of that made every
/// read wait for whatever sync happened to be running, which on a cold
/// 198k-message folder meant minutes.
pub struct OrbHandle {
    store: Arc<Store>,
    /// Swapped whole when the file is re-read; read through
    /// [`OrbHandle::config`] at the moment of use, never held.
    config: std::sync::RwLock<Arc<Config>>,
    /// Where the config was read from, for re-reading it.
    config_path: String,
    runtime: Arc<Runtime>,
    /// Cancels the IDLE watches and the periodic sync when the app goes away,
    /// so their connections are closed rather than left for the server to
    /// time out.
    push_shutdown: tokio_util::sync::CancellationToken,
    /// The scheduler, once [`OrbHandle::start_push`] has started it.
    scheduler: std::sync::Mutex<Option<crate::services::SyncScheduler>>,
    /// The last few actions of every kind, taken back one key at a time.
    history: std::sync::Mutex<crate::undo::History>,
    /// The compose windows' embedded Neovims.
    #[cfg(feature = "desktop")]
    editors: crate::editor::Editors,
    /// Where reminder notifications are scheduled, once the platform layer has
    /// registered itself. `None` until then, and on any platform that has no
    /// notification centre at all — in which case reminders still fire through
    /// the launch sweep, just not while Orb is closed.
    notification_host: std::sync::RwLock<Option<Arc<dyn TaskNotificationHost>>>,
    /// Each account's standing with its server, from the last sync of it.
    health: Arc<HealthBook>,
    /// The last scheduled or asked-for export's outcome, for the status line
    /// (todo-spec §20.3). Kept in memory: the directory is the record of the
    /// last complete one.
    last_export: std::sync::Mutex<Option<Result<TaskExportResult, String>>>,
    /// Task changes on their way to the server (todo-spec §23).
    task_uploads: TaskUploads,
    /// The chat service, once [`OrbHandle::start_chat`] has started it.
    chat: std::sync::RwLock<Option<Arc<crate::chat::ChatService>>>,
    /// The sender checks made this session, by message: a message's headers
    /// do not change, and without the source stored each check is a trip to
    /// the server.
    sender_checks: std::sync::Mutex<HashMap<i64, crate::safety::SenderCheck>>,
}

/// What a task sync sets out to do (todo-spec §23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskRound {
    /// List the folder and reconcile it (§3.3).
    Full,
    /// Upload the tasks changed here; reconcile instead if the folder has
    /// changed since the last round.
    Push,
    /// Reconcile if the folder has changed since the last round, and upload
    /// whatever is still waiting.
    Poll,
}

/// What a mailbox's STATUS says that changes whenever anyone adds a message
/// to it or takes one out. Every task edit on any machine is an APPEND, so
/// UIDNEXT alone moves on each; the count catches a purge, which only
/// deletes. MODSEQ is left out on purpose (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FolderMark {
    uid_validity: Option<u32>,
    uid_next: Option<u32>,
    messages: u32,
}

impl From<crate::imap::FolderState> for FolderMark {
    fn from(state: crate::imap::FolderState) -> Self {
        FolderMark {
            uid_validity: state.uid_validity,
            uid_next: state.uid_next,
            messages: state.messages,
        }
    }
}

/// The state behind uploading task changes as they are made.
#[derive(Default)]
struct TaskUploads {
    /// One task sync at a time in this process: an upload beside a full sync
    /// would send the same task twice.
    running: tokio::sync::Mutex<()>,
    /// The tasks folder as this process last left it. A folder that still
    /// matches has not been touched by anybody else since.
    baseline: std::sync::Mutex<Option<FolderMark>>,
    /// Why the last task sync failed, until one succeeds.
    problem: std::sync::Mutex<Option<String>>,
    /// Wakes the uploader, once [`OrbHandle::start_task_uploads`] has started it.
    nudge: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

/// The folders of one account a write landed in, as [`OrbHandle::drain_then_sync`]
/// takes them.
fn destinations_of(
    account: &str,
    folders: Vec<String>,
) -> std::collections::BTreeSet<(String, String)> {
    folders
        .into_iter()
        .map(|folder| (account.to_string(), folder))
        .collect()
}

/// A filename fit for a path: no separators, nothing hidden, never empty.
fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.');
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Write `data` to `path`, creating the directories above it.
/// The type of an image, told from its first bytes.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

fn write_file(path: &std::path::Path, data: &[u8]) -> Result<(), OrbError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| OrbError::ConfigError {
            message: format!("could not create {}: {e}", parent.display()),
        })?;
    }
    std::fs::write(path, data).map_err(|e| OrbError::ConfigError {
        message: format!("could not write {}: {e}", path.display()),
    })
}

/// Resolve the configured trash folder name for an account, falling back to
/// the default ("Trash") when the account is unknown.
fn trash_folder_for(config: &Config, account_name: &str) -> String {
    config
        .accounts
        .iter()
        .find(|a| a.name == account_name)
        .map(|a| a.trash_folder.clone())
        .unwrap_or_else(|| "Trash".to_string())
}

/// Initialize the Orb library with a config path
///
/// # Arguments
/// * `config_path` - Path to the config file (e.g., "~/.config/orb/config.yml")
///
/// # Returns
/// An OrbHandle for interacting with the email system
pub fn init(config_path: String) -> Result<Arc<OrbHandle>, OrbError> {
    let config = config::read_from_path(&config_path)?;
    init_with(config, config_path)
}

/// [`init`] on a configuration already read — a test's, whose accounts take
/// settings a config file cannot give, such as a test server's certificate.
#[doc(hidden)]
pub fn init_with(mut config: Config, config_path: String) -> Result<Arc<OrbHandle>, OrbError> {
    // Create tokio runtime for async operations
    let runtime = Runtime::new().map_err(|e| OrbError::ConfigError {
        message: format!("Failed to create async runtime: {}", e),
    })?;

    // The tasks folder is excluded from the mail world by name, and the name the
    // server uses is not necessarily the name the config gives: a server that
    // namespaces everything under `INBOX.` answers `INBOX.Tasks` to a config
    // that says `Tasks`. The resolved name was written down by the last sync, so
    // it is read back here rather than by asking the server — this runs before
    // the first connection, and a machine that is offline at launch must still
    // keep task messages out of the sidebar, the badges and the search index.
    hide_resolved_tasks_folder(&mut config);

    // Activate file logging before anything else can fail interestingly.
    // `config.log_file` is already tilde-expanded by `postprocess_and_validate`.
    // Without this every `logger::warn` in the sync path is silently dropped,
    // which leaves sync problems undiagnosable from a GUI build.
    crate::logger::init(config.log_file.clone());

    let config = Arc::new(config);

    // Initialize store
    let store = Store::new((*config).clone()).map_err(OrbError::from)?;

    // Reconcile the local database against the current config: prune folders and
    // messages for accounts/folders that were removed, renamed, or disabled in
    // the config. This runs before the store is shared so it holds the write
    // lock uncontended. Failure here is logged but non-fatal.
    if let Err(e) = runtime.block_on(store.reconcile_with_config()) {
        crate::logger::warn(&format!("Config reconcile failed: {}", e));
    }

    // Surface misconfigured special folders now rather than at the moment a
    // delete or send fails against a mailbox that does not exist.
    match store.special_folder_warnings() {
        Ok(warnings) => {
            for warning in warnings {
                crate::logger::warn(&format!("Config: {}", warning));
            }
        }
        Err(e) => crate::logger::warn(&format!("Config check failed: {}", e)),
    }

    let health = Arc::new(HealthBook::new(
        config.accounts.iter().map(|a| a.name.clone()),
    ));
    Ok(Arc::new(OrbHandle {
        store: Arc::new(store),
        config: std::sync::RwLock::new(config),
        config_path,
        runtime: Arc::new(runtime),
        push_shutdown: tokio_util::sync::CancellationToken::new(),
        scheduler: std::sync::Mutex::new(None),
        history: Default::default(),
        #[cfg(feature = "desktop")]
        editors: Default::default(),
        notification_host: std::sync::RwLock::new(None),
        health,
        last_export: std::sync::Mutex::new(None),
        task_uploads: TaskUploads::default(),
        chat: std::sync::RwLock::new(None),
        sender_checks: Default::default(),
    }))
}

/// Modification time and size, which together change on every save.
#[cfg(feature = "desktop")]
fn file_signature(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Take the *resolved* tasks folder out of the mail world too.
///
/// `postprocess_and_validate` already hides the spelling the config uses. This
/// adds the spelling the server turned out to use, which was recorded by the
/// last successful sync — the two differ whenever the server namespaces its
/// folders, and a folder that is hidden under one name and synced under the
/// other is exactly the failure §1 forbids.
///
/// Silent on every failure: the database may not exist yet, the tasks tables may
/// predate their migration, and none of that is a reason to refuse to start.
fn hide_resolved_tasks_folder(config: &mut Config) {
    let Some(tasks) = config.tasks.clone() else {
        return;
    };
    let Ok(db) = crate::db::Database::new(&config.db_path) else {
        return;
    };
    let resolved = TaskRepository::new(&db)
        .get_sync_state(&tasks.account)
        .ok()
        .flatten()
        .map(|state| state.folder_name);
    if let Some(resolved) = resolved.filter(|name| !name.is_empty() && *name != tasks.folder) {
        config.hide_tasks_folder(&tasks.account, &resolved);
    }
}

impl OrbHandle {
    /// The configuration as of now. Taken once per operation, so that a
    /// reload between two reads cannot show half of each.
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Get all configured accounts
    pub fn get_accounts(&self) -> Result<Vec<Account>, OrbError> {
        let store = &self.store;
        let accounts = store.get_accounts();
        let config = self.config();

        Ok(accounts
            .into_iter()
            .map(|a| {
                let configured = config
                    .accounts
                    .iter()
                    .find(|c| c.name == a.name)
                    .map(|c| store.resolve_special_folders(c));
                let configured = configured.as_ref();
                Account {
                    drafts_folder: configured
                        .map(|c| c.drafts_folder.clone())
                        .unwrap_or_else(|| "Drafts".to_string()),
                    trash_folder: configured
                        .map(|c| c.trash_folder.clone())
                        .unwrap_or_else(|| "Trash".to_string()),
                    archive_folder: configured
                        .map(|c| c.archive_folder_name().to_string())
                        .unwrap_or_else(|| config::DEFAULT_ARCHIVE_FOLDER.to_string()),
                    junk_folder: configured
                        .map(|c| c.junk_folder_name().to_string())
                        .unwrap_or_else(|| config::DEFAULT_JUNK_FOLDER.to_string()),
                    name: a.name,
                    email: a.email,
                }
            })
            .collect())
    }

    /// Fetch bodies for messages that have been synced but never opened.
    ///
    /// Returns how many bodies were stored. Bounded per call so the caller can
    /// drive it from a loop and stop between calls; progress is whatever is
    /// already in the database, so it resumes for free.
    pub fn fetch_bodies(
        &self,
        account_name: String,
        folders: Vec<String>,
        budget: u32,
        listener: Option<Box<dyn SyncProgressListener>>,
    ) -> Result<BodyBackfillResult, OrbError> {
        let (progress_tx, forwarder) = match listener {
            Some(listener) => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                (
                    Some(tx),
                    Some(self.runtime.spawn(forward_progress(rx, listener))),
                )
            }
            None => (None, None),
        };

        // Folders are taken in the order given — the one being viewed first —
        // and each is carried through both phases before the next is started.
        //
        // A message needs a recorded shape before its body can be fetched,
        // since the shape is what names the MIME part to ask for. But making
        // every message everywhere shaped before any body is fetched means
        // hours pass before a single body arrives, even for the folder on
        // screen. Per folder, the wait is seconds.
        let mut shaped = 0usize;
        let mut sources = 0usize;
        let mut report = crate::tasks::bodies::BodyFetchReport::default();
        let mut remaining = budget as usize;
        let mut progress_tx = progress_tx;

        let mut threaded = 0usize;
        // A few header lines a message, against a body each: a budget of
        // their own, so a header refetch over the whole mailbox neither takes
        // hours nor holds the bodies up.
        let mut header_budget = budget as usize * HEADER_BUDGET_FACTOR;
        let mut html = 0usize;
        for folder in &folders {
            if remaining == 0 {
                break;
            }

            // The threading headers first, so a folder is conversations
            // before its bodies have arrived.
            if header_budget > 0 {
                let just_threaded = self
                    .runtime
                    .block_on(self.store.fetch_pending_thread_headers(
                        &account_name,
                        std::slice::from_ref(folder),
                        header_budget,
                    ))
                    .map_err(OrbError::from)?;
                threaded += just_threaded;
                header_budget = header_budget.saturating_sub(just_threaded);
            }

            let just_shaped = self
                .runtime
                .block_on(self.store.fetch_pending_shapes(
                    &account_name,
                    std::slice::from_ref(folder),
                    remaining,
                ))
                .map_err(OrbError::from)?;

            if just_shaped > 0 {
                // Still learning this folder's shape; its bodies come next pass.
                shaped += just_shaped;
                remaining = remaining.saturating_sub(just_shaped);
                continue;
            }

            // The HTML of the newest mail ahead of the text of the rest: it is
            // what the reader waits on when a message is opened, and a folder's
            // recent mail is what gets opened.
            let just_html = self
                .runtime
                .block_on(self.store.fetch_recent_html(
                    &account_name,
                    std::slice::from_ref(folder),
                    remaining,
                ))
                .map_err(OrbError::from)?;
            html += just_html;
            remaining = remaining.saturating_sub(just_html);
            if remaining == 0 {
                break;
            }

            let folder_report = self
                .runtime
                .block_on(self.store.fetch_pending_bodies(
                    &account_name,
                    std::slice::from_ref(folder),
                    remaining,
                    progress_tx.clone(),
                ))
                .map_err(OrbError::from)?;
            remaining = remaining.saturating_sub(folder_report.fetched + folder_report.missing);
            report.merge(folder_report);

            // With the bodies in, the sources of the messages with
            // attachments, so those open without the server. Last, because
            // a source is the whole message and a body is a part of it.
            if remaining > 0 {
                let stored = self
                    .runtime
                    .block_on(self.store.fetch_pending_sources(
                        &account_name,
                        std::slice::from_ref(folder),
                        remaining,
                    ))
                    .map_err(OrbError::from)?;
                remaining = remaining.saturating_sub(stored);
                sources += stored;
            }
        }

        // Give whatever now has its final text a vector. Last of the three
        // because it depends on them: a message still waiting for its body is
        // deliberately left alone rather than indexed by its subject and never
        // revisited. Purely local, and fast enough that a whole mailbox takes
        // seconds.
        let embedded = self
            .runtime
            .block_on(self.store.index_pending_embeddings(EMBED_BATCH))
            .unwrap_or_else(|error| {
                crate::logger::warn(&format!("could not index embeddings: {error}"));
                0
            });

        // The forwarder ends when the last sender drops, so this one has to go
        // before waiting on it — otherwise it waits for a message that can
        // never arrive, and the join waits with it.
        progress_tx.take();

        if let Some(handle) = forwarder {
            let _ = self.runtime.block_on(handle);
        }

        let (pending, total) = self
            .store
            .body_backfill_progress()
            .map_err(OrbError::from)?;

        let (unshaped, messages) = self
            .store
            .shape_backfill_progress()
            .map_err(OrbError::from)?;

        let unreadable = self
            .store
            .unreadable_shape_count()
            .map_err(OrbError::from)?;

        Ok(BodyBackfillResult {
            embedded: embedded as u32,
            embeddings_pending: self.store.embedding_progress().unwrap_or((0, 0)).0 as u32,
            shaped: shaped as u32,
            unshaped: unshaped as u32,
            messages: messages as u32,
            fetched: report.fetched as u32,
            missing: report.missing as u32,
            pending: pending as u32,
            total: total as u32,
            unreadable: unreadable as u32,
            html: html as u32,
            sources: sources as u32,
            sources_pending: self.store.source_backfill_pending().unwrap_or(0) as u32,
            threaded: threaded as u32,
            threads_pending: self.store.thread_header_backfill_pending().unwrap_or(0) as u32,
        })
    }

    /// How far the body backfill has got, without doing any work.
    pub fn body_backfill_progress(&self) -> Result<BodyBackfillResult, OrbError> {
        let (pending, total) = self
            .store
            .body_backfill_progress()
            .map_err(OrbError::from)?;
        let (unshaped, messages) = self
            .store
            .shape_backfill_progress()
            .map_err(OrbError::from)?;
        let unreadable = self
            .store
            .unreadable_shape_count()
            .map_err(OrbError::from)?;
        Ok(BodyBackfillResult {
            embedded: 0,
            embeddings_pending: self.store.embedding_progress().unwrap_or((0, 0)).0 as u32,
            shaped: 0,
            unshaped: unshaped as u32,
            messages: messages as u32,
            fetched: 0,
            missing: 0,
            pending: pending as u32,
            total: total as u32,
            unreadable: unreadable as u32,
            html: 0,
            sources: 0,
            sources_pending: self.store.source_backfill_pending().unwrap_or(0) as u32,
            threaded: 0,
            threads_pending: self.store.thread_header_backfill_pending().unwrap_or(0) as u32,
        })
    }

    /// Configuration problems worth showing the user: special folders naming
    /// a mailbox the account does not have, which otherwise only surfaces as
    /// an opaque `UID COPY failed` at the moment a message is deleted or sent;
    /// and filter rules that did not parse or move to a folder that is not
    /// there.
    pub fn config_warnings(&self) -> Result<Vec<String>, OrbError> {
        let mut warnings = self.store.special_folder_warnings()?;
        warnings.extend(self.store.filter_warnings()?);
        warnings.extend(self.config().highlights.warnings.iter().cloned());
        warnings.extend(self.config().load_remote_images.0.warnings.iter().cloned());
        warnings.extend(self.config().warnings.iter().cloned());
        if let Some(export) = self.config().tasks.as_ref().and_then(|t| t.export.as_ref()) {
            let directory =
                std::path::PathBuf::from(shellexpand::tilde(&export.directory).into_owned());
            let probe = directory.join(".orb-export-probe");
            let writable = std::fs::create_dir_all(&directory)
                .and_then(|_| std::fs::write(&probe, b""))
                .and_then(|_| std::fs::remove_file(&probe));
            if let Err(e) = writable {
                warnings.push(format!(
                    "tasks.export.directory {} cannot be written: {e}",
                    directory.display()
                ));
            }
        }
        Ok(warnings)
    }

    /// Each account's standing with its server: whether it is being synced,
    /// whether the last sync succeeded, and if not what kind of failure it
    /// was and what the server said. In the configured order.
    pub fn account_statuses(&self) -> Vec<AccountHealth> {
        let order: Vec<String> = self
            .config()
            .accounts
            .iter()
            .map(|a| a.name.clone())
            .collect();
        self.health.snapshot(&order)
    }

    /// Keep the mailbox in step with the server from now on.
    ///
    /// Starts the IDLE watches (INBOX and every folder the configuration
    /// marks `watch`), the periodic sync of every account at its interval,
    /// and the watch for the machine having slept. `listener` hears of every
    /// sync these run. Idempotent: a second call does nothing.
    pub fn start_push(&self, listener: Box<dyn PushListener>) -> Result<(), OrbError> {
        let mut slot = self.scheduler.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Ok(());
        }
        let listener: Arc<dyn PushListener> = Arc::from(listener);
        let health = self.health.clone();
        let on_event: crate::services::scheduler::SyncEventListener =
            Arc::new(move |event| match event {
                crate::services::SyncEvent::Started { account, folder } => {
                    health.started(&account);
                    listener.on_started(account, folder);
                }
                crate::services::SyncEvent::Synced {
                    account,
                    folder,
                    report,
                } => {
                    health.finished(&account, &report);
                    listener.on_synced(account, folder, SyncResult::from(report));
                }
                crate::services::SyncEvent::Failed {
                    account,
                    folder,
                    kind,
                    error,
                } => {
                    health.failed(&account, folder.as_deref(), kind, &error);
                    listener.on_error(account, folder, error);
                }
            });
        let _guard = self.runtime.enter();
        *slot = Some(crate::services::SyncScheduler::start(
            self.store.clone(),
            self.config(),
            on_event,
            self.push_shutdown.clone(),
        ));
        Ok(())
    }

    /// Ask for a sync of one folder, coalesced with whatever the scheduler is
    /// already doing. Nothing happens before [`OrbHandle::start_push`].
    pub fn request_sync(&self, account_name: String, folder_name: Option<String>) {
        if let Some(scheduler) = self
            .scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            scheduler.request(&account_name, folder_name.as_deref());
        }
    }

    /// The machine has moved to another network, as the platform has seen
    /// it: reconnect and sync, as when Orb notices the move itself.
    pub fn network_changed(&self) {
        crate::services::network::report_change();
    }

    /// Let go of the server before the process does.
    ///
    /// Stops the IDLE watches and the periodic sync, logs out the pooled
    /// connections and releases this instance's folder locks, so that another
    /// instance does not have to wait six minutes to sync what this one was
    /// syncing. Bounded: a server that will not say goodbye is not waited for.
    /// Register who to ask for a password that no command and no Keychain
    /// item provides. Until this is called such a login fails with an error
    /// naming the account.
    pub fn set_password_prompter(&self, prompter: Box<dyn PasswordPrompter>) {
        self.store.credentials().set_prompter(Arc::from(prompter));
    }

    /// Re-read the config file and run on it from now on.
    ///
    /// A file that does not parse changes nothing: the error is returned and
    /// the config in force stays. Otherwise the new one is swapped in whole,
    /// and the keys that were turned into something at launch — the database,
    /// the connections, the sync schedule — are named as needing a restart.
    pub fn reload_config(&self) -> ConfigReload {
        let mut config = match config::read_from_path(&self.config_path) {
            Ok(config) => config,
            Err(error) => {
                crate::logger::warn(&format!("config: not reloaded: {error}"));
                return ConfigReload {
                    error: Some(error.to_string()),
                    restart_needed: Vec::new(),
                };
            }
        };
        hide_resolved_tasks_folder(&mut config);
        let old = self.config();
        let restart_needed = config::keys_needing_restart(&old, &config);
        let config = Arc::new(config);
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config.clone();
        if let (Ok(chat), Some(section)) = (self.chat(), config.chat.clone())
            && !restart_needed.iter().any(|key| key == "chat")
        {
            chat.update_config(section);
        }
        self.store.replace_config(config);
        crate::logger::info(&format!(
            "config: reloaded from {}{}",
            self.config_path,
            if restart_needed.is_empty() {
                String::new()
            } else {
                format!("; a restart is needed for {}", restart_needed.join(", "))
            }
        ));
        ConfigReload {
            error: None,
            restart_needed,
        }
    }

    /// Re-read the config file whenever it changes, and tell `listener` what
    /// came of it. Polled, every couple of seconds: editors replace the file
    /// rather than rewrite it, which a file watch on the path loses track of
    /// and a look at the path does not. Stops with [`OrbHandle::shutdown`].
    #[cfg(feature = "desktop")]
    pub fn start_config_watch(self: &Arc<Self>, listener: Box<dyn ConfigListener>) {
        let handle = Arc::downgrade(self);
        let path = std::path::PathBuf::from(shellexpand::tilde(&self.config_path).as_ref());
        let shutdown = self.push_shutdown.clone();
        let interval = std::time::Duration::from_secs(
            crate::constants::EXTERNAL_CHANGE_CHECK_INTERVAL_SECONDS,
        );
        std::thread::Builder::new()
            .name("config-watch".to_string())
            .spawn(move || {
                let mut last = file_signature(&path);
                while !shutdown.is_cancelled() {
                    std::thread::sleep(interval);
                    let now = file_signature(&path);
                    // A vanished file is an editor mid-save, not a change.
                    if now.is_none() || now == last {
                        continue;
                    }
                    last = now;
                    let Some(handle) = handle.upgrade() else {
                        return;
                    };
                    listener.on_config_reloaded(handle.reload_config());
                }
            })
            .map(|_| ())
            .unwrap_or_else(|error| {
                crate::logger::warn(&format!("config: not watching the file: {error}"))
            });
    }

    pub fn shutdown(&self) {
        self.push_shutdown.cancel();
        self.task_uploads
            .nudge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let store = self.store.clone();
        let outcome = self.runtime.block_on(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), store.shutdown()).await
        });
        match outcome {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => crate::logger::warn(&format!("shutdown: {error}")),
            Err(_) => crate::logger::warn("shutdown: the store did not close in time"),
        }
    }

    /// Save a draft to the account's Drafts folder.
    ///
    /// Returns where it now lives, which a later save passes back as
    /// `replacing` so the folder holds one draft rather than a version of it
    /// per keystroke.
    pub fn save_draft(
        &self,
        input: SendMailInput,
        replacing: Option<u32>,
    ) -> Result<Option<u32>, OrbError> {
        let mut email = outgoing_email_from_input(&input);
        email.attachments = read_attachments(&input.attachment_paths)?;
        if let Some(account) = self
            .config()
            .accounts
            .iter()
            .find(|a| a.name == input.account_name)
        {
            apply_identity(account, &input, &mut email, false)?;
        }

        self.runtime
            .block_on(
                self.store
                    .save_draft(&input.account_name, &email, replacing),
            )
            .map_err(OrbError::from)
    }

    /// Start Neovim on a draft's body for a compose window, with the
    /// identity's signature shown below it, sending what it draws to `sink`;
    /// see [`crate::editor`].
    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "desktop")]
    pub fn editor_open(
        &self,
        id: String,
        owner: String,
        body: String,
        signature: Option<String>,
        cols: u32,
        rows: u32,
        sink: crate::editor::EditorSink,
    ) -> Result<(), OrbError> {
        self.runtime
            .block_on(
                self.editors
                    .open(&id, &owner, &body, signature.as_deref(), cols, rows, sink),
            )
            .map_err(editor_error)
    }

    /// Keys for an editor, in Neovim's notation.
    #[cfg(feature = "desktop")]
    pub fn editor_input(&self, id: String, keys: String) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.editors.input(&id, &keys))
            .map_err(editor_error)
    }

    /// Text pasted into an editor.
    #[cfg(feature = "desktop")]
    pub fn editor_paste(&self, id: String, text: String) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.editors.paste(&id, &text))
            .map_err(editor_error)
    }

    /// A mouse event on an editor's grid.
    #[cfg(feature = "desktop")]
    pub fn editor_mouse(
        &self,
        id: String,
        button: String,
        action: String,
        modifier: String,
        row: u32,
        col: u32,
    ) -> Result<(), OrbError> {
        self.runtime
            .block_on(
                self.editors
                    .mouse(&id, &button, &action, &modifier, row, col),
            )
            .map_err(editor_error)
    }

    /// An editor gained or lost the keyboard.
    #[cfg(feature = "desktop")]
    pub fn editor_focus(&self, id: String, gained: bool) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.editors.focus(&id, gained))
            .map_err(editor_error)
    }

    /// An editor's window changed size.
    #[cfg(feature = "desktop")]
    pub fn editor_resize(&self, id: String, cols: u32, rows: u32) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.editors.resize(&id, cols, rows))
            .map_err(editor_error)
    }

    /// The draft as it stands in the editor.
    #[cfg(feature = "desktop")]
    pub fn editor_buffer(&self, id: String) -> Result<String, OrbError> {
        self.runtime
            .block_on(self.editors.buffer(&id))
            .map_err(editor_error)
    }

    /// Replace the draft in the editor, with the signature shown below it,
    /// and write it.
    #[cfg(feature = "desktop")]
    pub fn editor_set_buffer(
        &self,
        id: String,
        body: String,
        signature: Option<String>,
    ) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.editors.set_buffer(&id, &body, signature.as_deref()))
            .map_err(editor_error)
    }

    /// Put a template's text into an editor's draft at the cursor, keeping
    /// its placeholders to be filled.
    #[cfg(feature = "desktop")]
    pub fn editor_insert_template(
        &self,
        id: String,
        text: crate::templates::Expanded,
    ) -> Result<bool, OrbError> {
        self.runtime
            .block_on(self.editors.insert_template(&id, &text))
            .map_err(editor_error)
    }

    /// Fill a placeholder wherever it stands in an editor's draft.
    #[cfg(feature = "desktop")]
    pub fn editor_fill_placeholder(
        &self,
        id: String,
        name: String,
        value: String,
    ) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.editors.fill_placeholder(&id, &name, &value))
            .map_err(editor_error)
    }

    /// Put an editor's cursor on a placeholder.
    #[cfg(feature = "desktop")]
    pub fn editor_goto_placeholder(&self, id: String, name: String) -> Result<bool, OrbError> {
        self.runtime
            .block_on(self.editors.goto_placeholder(&id, &name))
            .map_err(editor_error)
    }

    /// Open a file in a tab of an editor, beside the draft.
    #[cfg(feature = "desktop")]
    pub fn editor_edit_file(&self, id: String, path: String) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.editors.edit_file(&id, &path))
            .map_err(editor_error)
    }

    /// Stop an editor and remove its file.
    #[cfg(feature = "desktop")]
    pub fn editor_close(&self, id: String) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.editors.close(&id))
            .map_err(editor_error)
    }

    /// Stop the editors of a window that went away.
    #[cfg(feature = "desktop")]
    pub fn editor_close_owned_by(&self, owner: String) {
        self.runtime.block_on(self.editors.close_owned_by(&owner));
    }

    /// Stop every editor, for when the app quits.
    #[cfg(feature = "desktop")]
    pub fn editor_close_all(&self) {
        self.runtime.block_on(self.editors.close_all());
    }

    /// Remove a saved draft, once it has been sent or given up on.
    pub fn discard_draft(&self, account: String, uid: u32) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.store.discard_draft(&account, uid))
            .map_err(OrbError::from)
    }

    /// The Outbox: every queued message, waiting or failed.
    pub fn outbox(&self) -> Result<Vec<crate::store::QueuedSend>, OrbError> {
        self.store.outbox().map_err(OrbError::from)
    }

    /// Who hears of changes to the Outbox.
    pub fn set_outbox_listener(&self, listener: Arc<dyn crate::store::OutboxListener>) {
        self.store.set_outbox_listener(listener);
    }

    /// Put a failed message back in line and try to send it now.
    pub fn retry_outbox(&self, id: i64) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.store.retry_outbox(id))
            .map_err(OrbError::from)
    }

    /// Delete a queued message unsent.
    pub fn discard_outbox(&self, id: i64) -> Result<(), OrbError> {
        self.runtime
            .block_on(self.store.discard_outbox(id))
            .map_err(OrbError::from)
    }

    /// A queued message as a draft to edit, its attachments written out to
    /// files as a reopened draft's are. The message stays queued; the window
    /// deletes it once the draft is saved or sent, so closing the window
    /// loses nothing. Returns the account and the draft.
    pub fn edit_outbox(&self, id: i64) -> Result<(String, crate::draft::DraftTemplate), OrbError> {
        let (account_name, email) = self.store.outbox_message(id).map_err(OrbError::from)?;
        let account = self
            .config()
            .accounts
            .iter()
            .find(|a| a.name == account_name)
            .cloned()
            .ok_or_else(|| OrbError::ConfigError {
                message: format!("Account '{account_name}' not found"),
            })?;

        let identities = account.identities();
        let identity = match email.from.as_deref() {
            None => identities.first(),
            Some(from) => identities
                .iter()
                .find(|i| i.from == from)
                .or_else(|| {
                    identities.iter().find(|i| {
                        crate::config::bare_address(&i.from)
                            .eq_ignore_ascii_case(&crate::config::bare_address(from))
                    })
                })
                .or(identities.first()),
        };

        let directory = std::env::temp_dir().join(format!("orb-outbox-{id}"));
        std::fs::create_dir_all(&directory).map_err(|e| OrbError::ConfigError {
            message: format!("could not prepare the attachments: {e}"),
        })?;
        let mut taken = std::collections::HashSet::new();
        let mut attachments = Vec::new();
        for attachment in &email.attachments {
            let safe: String = attachment
                .filename
                .chars()
                .map(|c| {
                    if matches!(c, '/' | '\\' | ':') {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let name = match safe.trim_start_matches('.') {
                "" => "attachment".to_string(),
                _ => safe,
            };
            let path = directory.join(unique_name(&mut taken, name));
            std::fs::write(&path, &attachment.bytes).map_err(|e| OrbError::ConfigError {
                message: format!("could not write an attachment: {e}"),
            })?;
            attachments.push(path.to_string_lossy().to_string());
        }

        let draft = crate::draft::DraftTemplate {
            to: email.to,
            cc: email.cc,
            bcc: email.bcc,
            subject: email.subject,
            body: email.body,
            in_reply_to: email.in_reply_to,
            references: email.references,
            forwards: email.forwards,
            attachments,
            identity: identity.map(|i| i.name.clone()).unwrap_or_default(),
            ..Default::default()
        };
        Ok((account_name, draft))
    }

    /// Write a message's attachments to disk so they can be sent on.
    ///
    /// Forwarding has to carry them, and the send path takes files by path
    /// rather than by content — so they are unpacked to temporary files and
    /// handed over the same way a file picked by hand would be. Returns the
    /// paths written.
    pub fn detach_attachments(&self, message_id: i64) -> Result<Vec<String>, OrbError> {
        let metas = self.list_attachments(message_id)?;
        let directory = std::env::temp_dir().join(format!("orb-forward-{message_id}"));
        std::fs::create_dir_all(&directory).map_err(|e| OrbError::ConfigError {
            message: format!("could not prepare the attachments: {e}"),
        })?;

        let mut paths = Vec::new();
        let mut taken = std::collections::HashSet::new();
        for meta in metas {
            let data = self.extract_attachment(message_id, meta.part_id.clone())?;
            // The filename comes from the message, so it is not to be trusted
            // as a path: anything that could climb out of the directory is
            // replaced, and an empty name still gets one.
            let safe: String = meta
                .filename
                .chars()
                .map(|c| {
                    if c == '/' || c == '\\' || c == ':' {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let name = match safe.trim_start_matches('.') {
                "" => format!("attachment-{}", meta.part_id),
                _ => safe,
            };
            // Two attachments of one name — pasted images are all
            // `image.png` — must not overwrite each other.
            let name = unique_name(&mut taken, name);
            let path = directory.join(name);
            std::fs::write(&path, &data.data).map_err(|e| OrbError::ConfigError {
                message: format!("could not write an attachment: {e}"),
            })?;
            paths.push(path.to_string_lossy().to_string());
        }
        Ok(paths)
    }

    /// The draft a reply starts from.
    ///
    /// Built here rather than in the window because everything it needs is
    /// here: the quoting settings, the identities and their signatures, and the
    /// message being answered — including its HTML, when that is all it has.
    pub fn reply_draft(
        &self,
        message_id: i64,
        reply_all: bool,
    ) -> Result<crate::draft::DraftTemplate, OrbError> {
        let detail = self.get_message(message_id)?;
        let account = self.account_for_message(message_id);

        // The threading headers are not synced, so this may go to the server.
        // A server that cannot be reached is not a reason to refuse the reply:
        // what is stored still threads under the parent, if not under the
        // whole conversation.
        let mut parent = self
            .runtime
            .block_on(crate::store::fetch_reply_headers(&self.store, message_id))
            .unwrap_or_else(|e| {
                crate::logger::warn(&format!(
                    "message {message_id}: replying with the stored Message-ID alone, \
                     the headers could not be fetched: {e}"
                ));
                crate::draft::ParentHeaders::default()
            });
        if parent.message_id.is_none() {
            parent.message_id = detail.message_id.clone();
        }

        let identity = Self::identity_addressed(&account, &detail, &parent);
        let (addressed_to, delivered_to) = Self::addresses_of_original(&detail, &parent);
        // Every address the user sends as, not only the account's own: a
        // reply-all to mail sent to the support address must not copy it.
        let mine: Vec<String> = self
            .config()
            .accounts
            .iter()
            .flat_map(|a| a.identities())
            .flat_map(|i| std::iter::once(i.from).chain(i.addresses))
            .collect();
        let (to, cc) = crate::draft::reply_recipients(
            detail.from_display.as_deref(),
            parent.reply_to.as_deref(),
            detail.to_display.as_deref(),
            detail.cc_display.as_deref(),
            &mine,
            reply_all,
        );

        Ok(crate::draft::DraftTemplate {
            to,
            cc,
            bcc: Vec::new(),
            subject: crate::draft::reply_subject(detail.subject.as_deref()),
            body: crate::draft::reply_body(
                &self.config().reply,
                &identity,
                detail.from_display.as_deref(),
                detail.date.as_deref(),
                detail.body_text.as_deref(),
                detail.body_html.as_deref(),
            ),
            references: crate::draft::reply_references(&parent),
            in_reply_to: parent.message_id,
            forwards: None,
            attachments: Vec::new(),
            identity: identity.name,
            incomplete: false,
            addressed_to,
            delivered_to,
        })
    }

    /// The draft a forward starts from, carrying the original's attachments.
    pub fn forward_draft(&self, message_id: i64) -> Result<crate::draft::DraftTemplate, OrbError> {
        let detail = self.get_message(message_id)?;
        let account = self.account_for_message(message_id);
        // A forward without the attachments is a different message than the
        // one the user meant to send, so an extraction failure is the
        // forward's failure.
        let attachments = if detail.has_attachments {
            self.detach_attachments(message_id)
                .map_err(|e| OrbError::SendError {
                    message: format!("the attachments could not be unpacked for forwarding: {e}"),
                })?
        } else {
            Vec::new()
        };
        // For `Delivered-To`, which To and Cc can do without, and for the
        // Message-ID the forward names.
        let parent = self
            .runtime
            .block_on(crate::store::fetch_reply_headers(&self.store, message_id))
            .unwrap_or_default();
        let identity = Self::identity_addressed(&account, &detail, &parent);
        let (addressed_to, delivered_to) = Self::addresses_of_original(&detail, &parent);

        Ok(crate::draft::DraftTemplate {
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: crate::draft::forward_subject(detail.subject.as_deref()),
            body: crate::draft::forward_body(
                &identity,
                detail.from_display.as_deref(),
                detail.date.as_deref(),
                detail.subject.as_deref(),
                detail.to_display.as_deref(),
                detail.body_text.as_deref(),
                detail.body_html.as_deref(),
            ),
            in_reply_to: None,
            references: None,
            forwards: parent.message_id,
            attachments,
            identity: identity.name,
            incomplete: false,
            addressed_to,
            delivered_to,
        })
    }

    /// The draft a new message starts from: the signature of the account's
    /// default identity, and room above it.
    pub fn new_draft(&self, account_name: String) -> crate::draft::DraftTemplate {
        let config = self.config();
        let identity = config
            .accounts
            .iter()
            .find(|a| a.name == account_name)
            .or_else(|| config.accounts.first())
            .and_then(|a| a.identities().into_iter().next());

        match identity {
            Some(identity) => crate::draft::DraftTemplate {
                body: crate::draft::new_body(&identity),
                identity: identity.name,
                ..Default::default()
            },
            None => crate::draft::DraftTemplate {
                body: "\n".to_string(),
                ..Default::default()
            },
        }
    }

    /// A saved draft, to be written on.
    ///
    /// Its identity is the one whose address it is from, so that switching
    /// identity finds the signature it was saved with and replaces it. Its
    /// threading headers come back with it: a reply saved and reopened is
    /// still a reply.
    pub fn edit_draft(&self, message_id: i64) -> Result<crate::draft::DraftTemplate, OrbError> {
        let detail = self.get_message(message_id)?;
        let account = self.account_for_message(message_id);
        let saved = self
            .runtime
            .block_on(crate::store::fetch_reply_headers(&self.store, message_id));
        let mut incomplete = saved.is_err();
        let saved = saved.unwrap_or_default();
        // The identity the draft was written as: the one with its From
        // address, and among several with that address the one whose
        // signature the body carries, so that a draft written as `Short` is
        // not reopened as the default with `Short`'s signature left in.
        let identities = account.identities();
        let body = detail.body_text.as_deref().unwrap_or("");
        let same_address: Vec<&crate::config::IdentityConfig> = identities
            .iter()
            .filter(|i| {
                detail.from_display.as_deref().is_some_and(|from| {
                    crate::config::bare_address(&i.from)
                        .eq_ignore_ascii_case(&crate::config::bare_address(from))
                })
            })
            .collect();
        let identity = same_address
            .iter()
            .find(|i| {
                i.get_signature().is_some_and(|signature| {
                    !signature.trim().is_empty() && body.contains(signature.trim())
                })
            })
            .or(same_address.first())
            .copied()
            .or(identities.first());
        let list = |value: &Option<String>| crate::draft::split_addresses(value.as_deref());
        let (addressed_to, delivered_to) =
            self.addresses_of_parent(saved.in_reply_to.as_deref().or(saved.forwards.as_deref()));

        // The files it went to Drafts with, written out again so the window
        // can show them, take them off and send them. A draft whose
        // attachments cannot be read reopens without them rather than not at
        // all; the log says why.
        let attachments = self.detach_attachments(message_id).unwrap_or_else(|e| {
            crate::logger::warn(&format!(
                "the attachments of draft {message_id} could not be reopened: {e}"
            ));
            incomplete = true;
            Vec::new()
        });

        Ok(crate::draft::DraftTemplate {
            to: list(&detail.to_display),
            cc: list(&detail.cc_display),
            bcc: list(&saved.bcc),
            subject: detail.subject.unwrap_or_default(),
            body: detail.body_text.unwrap_or_default(),
            in_reply_to: saved.in_reply_to,
            references: saved.references,
            forwards: saved.forwards,
            attachments,
            identity: identity.map(|i| i.name.clone()).unwrap_or_default(),
            incomplete,
            addressed_to,
            delivered_to,
        })
    }

    /// The addresses of the message a saved reply or forward answers, found
    /// by its Message-ID among the stored ones. Empty when it is not stored.
    fn addresses_of_parent(&self, rfc_message_id: Option<&str>) -> (Vec<String>, Vec<String>) {
        let Some(item) = rfc_message_id
            .and_then(|id| self.store.find_messages_by_rfc_id(id).ok())
            .and_then(|items| items.into_iter().next())
        else {
            return Default::default();
        };
        let Ok(detail) = self.get_message(item.message.id) else {
            return Default::default();
        };
        let parent = self
            .runtime
            .block_on(crate::store::fetch_reply_headers(
                &self.store,
                item.message.id,
            ))
            .unwrap_or_default();
        Self::addresses_of_original(&detail, &parent)
    }

    /// Every address `detail` names, in the order a reply's identity is
    /// chosen by, and its Delivered-To alone.
    fn addresses_of_original(
        detail: &MessageDetail,
        parent: &crate::draft::ParentHeaders,
    ) -> (Vec<String>, Vec<String>) {
        let delivered_to = crate::draft::split_addresses(parent.delivered_to.as_deref());
        let addressed_to = [
            detail.from_display.as_deref(),
            detail.to_display.as_deref(),
            detail.cc_display.as_deref(),
        ]
        .into_iter()
        .flat_map(crate::draft::split_addresses)
        .chain(delivered_to.iter().cloned())
        .collect();
        (addressed_to, delivered_to)
    }

    /// Every identity of every account, each account's default first.
    pub fn senders(&self) -> Vec<IdentityInfo> {
        self.config()
            .accounts
            .iter()
            .flat_map(Self::identity_infos)
            .collect()
    }

    /// What is worth a second look about a draft before it is sent.
    pub fn draft_warnings(&self, input: DraftWarningsInput) -> Vec<crate::draft::DraftWarning> {
        let config = self.config();
        let senders: Vec<crate::draft::Sender> = config
            .accounts
            .iter()
            .flat_map(|a| {
                a.identities()
                    .into_iter()
                    .map(|identity| crate::draft::Sender {
                        account: a.name.clone(),
                        identity,
                    })
            })
            .collect();
        let current = senders
            .iter()
            .filter(|s| s.account == input.account_name)
            .find(|s| {
                input
                    .identity
                    .as_deref()
                    .is_none_or(|name| s.identity.name == name)
            })
            .or_else(|| senders.iter().find(|s| s.account == input.account_name));
        let recipients: Vec<String> = [&input.to, &input.cc, &input.bcc]
            .into_iter()
            .flatten()
            .cloned()
            .collect();

        let mut warnings = match current {
            Some(current) => crate::draft::sender_warnings(
                &senders,
                current,
                &crate::draft::WarningFacts {
                    addressed_to: &input.addressed_to,
                    delivered_to: &input.delivered_to,
                    recipients: &recipients,
                },
            ),
            None => Vec::new(),
        };
        warnings.extend(crate::draft::attachment_warning(
            &input.body,
            !input.attachment_paths.is_empty(),
            &config.compose.attachment_words,
        ));
        warnings.extend(crate::draft::placeholder_warnings(
            &input.subject,
            &input.body,
            &input.placeholders,
        ));
        warnings
    }

    /// The folder mail templates are read from.
    fn templates_folder(&self) -> std::path::PathBuf {
        crate::templates::folder(
            self.config().compose.templates_dir.as_deref(),
            &self.config_path,
        )
    }

    /// The mail templates `account` may use, its own first, with what is
    /// known for their placeholders in a message to `to`; and the files in
    /// the folder that could not be read.
    pub fn templates(&self, account: String, to: Vec<String>) -> TemplatesForDraft {
        let folder = crate::templates::for_account(
            crate::templates::load(&self.templates_folder()),
            &account,
        );
        let templates = folder
            .templates
            .into_iter()
            .map(|template| TemplateForDraft {
                suggestions: crate::templates::suggestions(&template.placeholders, &to),
                expanded: crate::templates::expand(&template.body),
                template,
            })
            .collect();
        TemplatesForDraft {
            folder: self.templates_folder().display().to_string(),
            templates,
            problems: folder.problems,
        }
    }

    /// A template, read again from its file, ready to be put into a draft
    /// to `to`: its subject and body with the placeholders marked.
    pub fn use_template(&self, path: String, to: Vec<String>) -> Result<TemplateUse, OrbError> {
        let template =
            crate::templates::load_one(std::path::Path::new(&path)).map_err(|message| {
                OrbError::InvalidInput {
                    message: format!("{path}: {message}"),
                }
            })?;
        Ok(TemplateUse {
            subject: template.subject.as_deref().map(crate::templates::expand),
            body: crate::templates::expand(&template.body),
            suggestions: crate::templates::suggestions(&template.placeholders, &to),
            placeholders: template.placeholders,
            name: template.name,
        })
    }

    /// What is known for `names` in a message to `to`.
    pub fn template_suggestions(
        &self,
        names: Vec<String>,
        to: Vec<String>,
    ) -> std::collections::HashMap<String, String> {
        crate::templates::suggestions(&names, &to)
    }

    /// The identities of an account, the default one first.
    pub fn identities(&self, account_name: String) -> Vec<IdentityInfo> {
        self.config()
            .accounts
            .iter()
            .find(|a| a.name == account_name)
            .map(Self::identity_infos)
            .unwrap_or_default()
    }

    fn identity_infos(account: &crate::config::AccountConfig) -> Vec<IdentityInfo> {
        account
            .identities()
            .into_iter()
            .map(|i| {
                let block = crate::draft::signature_block(&i);
                let signature = block.trim_matches('\n');
                IdentityInfo {
                    account: account.name.clone(),
                    signature: (!signature.is_empty()).then(|| signature.to_string()),
                    name: i.name,
                    from: i.from,
                }
            })
            .collect()
    }

    /// `body`, written as `from`, rewritten for `to`: the one signature
    /// replaced by the other.
    ///
    /// `to_account` is the account `to` belongs to, when it is not
    /// `account_name`'s.
    pub fn switch_identity(
        &self,
        account_name: String,
        from: String,
        to_account: Option<String>,
        to: String,
        body: String,
    ) -> Result<String, OrbError> {
        let config = self.config();
        let find = |account_name: &str, name: &str| {
            let account = config
                .accounts
                .iter()
                .find(|a| a.name == account_name)
                .ok_or_else(|| OrbError::ConfigError {
                    message: format!("Account '{account_name}' not found"),
                })?;
            account.identity(name).ok_or_else(|| OrbError::ConfigError {
                message: format!("the identity '{name}' is not configured for '{account_name}'"),
            })
        };
        let to_account = to_account.unwrap_or_else(|| account_name.clone());
        Ok(crate::draft::swap_signature(
            &body,
            &find(&account_name, &from)?,
            &find(&to_account, &to)?,
            config.reply.signature,
        ))
    }

    /// The identity a reply to or forward of `detail` is written as.
    fn identity_addressed(
        account: &crate::config::AccountConfig,
        detail: &MessageDetail,
        parent: &crate::draft::ParentHeaders,
    ) -> crate::config::IdentityConfig {
        let identities = account.identities();
        crate::draft::choose_identity(
            &identities,
            &[
                detail.from_display.as_deref(),
                detail.to_display.as_deref(),
                detail.cc_display.as_deref(),
                parent.delivered_to.as_deref(),
            ],
        )
        .cloned()
        .unwrap_or_else(|| identities[0].clone())
    }

    /// The account a message belongs to, for its signature and reply settings.
    fn account_for_message(&self, message_id: i64) -> crate::config::AccountConfig {
        let config = self.config();
        self.store
            .folder_of_message(message_id)
            .ok()
            .flatten()
            .and_then(|(account_name, _)| {
                config
                    .accounts
                    .iter()
                    .find(|a| a.name == account_name)
                    .cloned()
            })
            .or_else(|| config.accounts.first().cloned())
            .unwrap_or_else(|| config.accounts[0].clone())
    }

    /// Settings the interface needs to honour.
    ///
    /// Kept separate from the bindings so that adding one does not mean
    /// reaching into the keymap for an unrelated value.
    pub fn ui_settings(&self) -> UiSettings {
        let config = self.config();
        // A character of the TUI's columns is about eight pixels here.
        let chars = |n: u16| u32::from(n) * 8;
        UiSettings {
            mark_read_after_ms: (config.mark_read_after_seconds.max(0.0) * 1000.0) as u64,
            default_message_view: config.default_message_view.as_str().to_string(),
            columns: config.list_columns(),
            date_format: config.list_date_format().to_string(),
            search_sort: config.search.sort.as_str().to_string(),
            density: config.list.density.as_str().to_string(),
            from_width: match config.theme.column_widths.from {
                config::ColumnWidth::Fixed(n) => Some(chars(n)),
                config::ColumnWidth::Auto => None,
            },
            date_width: Some(chars(config.theme.column_widths.date)),
            theme: config.appearance.theme.as_str().to_string(),
            font_size: config.appearance.font_size,
            editor_font_size: config.appearance.editor_font_size,
            dark_html_mail: config.appearance.dark_html_mail,
            look: config.appearance.look.as_str().to_string(),
            completed_first_in_today: config
                .tasks
                .as_ref()
                .is_some_and(|t| t.completed_first_in_today),
        }
    }

    /// The last lines this process logged, oldest first, for the console.
    pub fn log_recent(&self, lines: u32) -> Vec<String> {
        crate::logger::recent(lines as usize)
    }

    /// Where the log is written, if anywhere.
    pub fn log_path(&self) -> Option<String> {
        crate::logger::path()
    }

    /// The sidebar's views: every virtual folder, each as the query that
    /// answers it.
    pub fn virtual_folders(&self) -> Vec<VirtualFolderInfo> {
        self.config()
            .virtual_folders
            .iter()
            .map(|view| VirtualFolderInfo {
                name: view.name.clone(),
                icon: view.icon.clone(),
                query: view.query(),
            })
            .collect()
    }

    /// The top of the sidebar, in the order `pinned` gives.
    pub fn pins(&self) -> Vec<PinInfo> {
        use crate::config::Pin;
        let named = |kind: &str, name: &str| PinInfo {
            kind: kind.to_string(),
            name: Some(name.to_string()),
            account: None,
            folder: None,
        };
        self.config()
            .pins
            .iter()
            .map(|pin| match pin {
                Pin::Tasks(view) => named("tasks", view),
                Pin::Reading(view) => named("reading", view),
                Pin::Waiting => named("chat", "waiting"),
                Pin::View(name) => named("view", name),
                Pin::Folder(f) => PinInfo {
                    kind: "folder".to_string(),
                    name: None,
                    account: Some(f.account.clone()),
                    folder: Some(f.folder.clone()),
                },
            })
            .collect()
    }

    /// Get folders for a specific account
    pub fn get_folders(&self, account_name: String) -> Result<Vec<FolderInfo>, OrbError> {
        let store = &self.store;
        let folders = store.get_folders(&account_name).map_err(OrbError::from)?;

        Ok(folders.into_iter().map(FolderInfo::from).collect())
    }

    /// Get all folders from all accounts
    pub fn get_all_folders(&self) -> Result<Vec<FolderInfo>, OrbError> {
        let store = &self.store;
        let folders = store.get_all_visible_folders().map_err(OrbError::from)?;

        Ok(folders.into_iter().map(FolderInfo::from).collect())
    }

    /// Get messages from a folder.
    ///
    /// `sort` overrides the ordering for this call; pass `None` to use the
    /// folder's configured order from `config.yml`. Because the ordering is
    /// applied by the query, it covers the whole folder rather than just the
    /// returned page — a UI that sorts by a column header must re-request
    /// rather than re-order what it already has.
    pub fn get_messages(
        &self,
        account_name: String,
        folder_name: String,
        offset: u32,
        limit: u32,
        sort: Option<MessageSortOrder>,
    ) -> Result<Vec<MessageHeader>, OrbError> {
        let store = &self.store;

        let sort = match sort {
            Some(order) => order.into(),
            None => store.get_folder_sort(&account_name, &folder_name),
        };

        let messages = store
            .get_message_list(
                &account_name,
                &folder_name,
                offset as usize,
                limit as usize,
                sort,
            )
            .map_err(OrbError::from)?;
        self.decorate_rows(&account_name, &folder_name, messages, None)
    }

    /// The row a message is on in its folder's list, in `sort` order or the
    /// folder's own, so the list can open the page it is on.
    pub fn message_row(
        &self,
        account_name: String,
        folder_name: String,
        id: i64,
        sort: Option<MessageSortOrder>,
    ) -> Result<Option<u32>, OrbError> {
        let sort = match sort {
            Some(order) => order.into(),
            None => self.store.get_folder_sort(&account_name, &folder_name),
        };
        Ok(self
            .store
            .message_row(&account_name, &folder_name, id, sort)
            .map_err(OrbError::from)?
            .map(|row| row as u32))
    }

    /// [`OrbHandle::message_row`] in several folders read as one list, as
    /// [`OrbHandle::get_inbox_messages`] reads them.
    pub fn inbox_message_row(
        &self,
        folders: Vec<(String, String)>,
        id: i64,
        sort: Option<MessageSortOrder>,
    ) -> Result<Option<u32>, OrbError> {
        let sort = sort
            .map(RustMessageSort::from)
            .unwrap_or(RustMessageSort::DateNewest);
        Ok(self
            .store
            .merged_message_row(&folders, id, sort)
            .map_err(OrbError::from)?
            .map(|row| row as u32))
    }

    /// Where a message stands in the threaded list of `folders` — one folder,
    /// or Today's inboxes — once its thread is expanded too, besides those in
    /// `expanded`.
    pub fn thread_row(
        &self,
        folders: Vec<(String, String)>,
        id: i64,
        expanded: Vec<String>,
    ) -> Result<Option<ThreadPlace>, OrbError> {
        Ok(self
            .store
            .merged_thread_row(&folders, id, &expanded)
            .map_err(OrbError::from)?
            .map(|place| ThreadPlace {
                row: place.row as u32,
                thread: place.thread,
                expand: place.expand,
            }))
    }

    /// A page of several folders read as one list — every account's inbox,
    /// for Today — in `sort` order across all of them, each row decorated as
    /// its own folder would decorate it. Pages like a folder does, so the
    /// list is not bounded by what fits in memory.
    pub fn get_inbox_messages(
        &self,
        folders: Vec<(String, String)>,
        offset: u32,
        limit: u32,
        sort: Option<MessageSortOrder>,
    ) -> Result<Vec<MessageHeader>, OrbError> {
        let sort = sort
            .map(RustMessageSort::from)
            .unwrap_or(RustMessageSort::DateNewest);
        let items = self
            .store
            .get_merged_message_list(&folders, offset as usize, limit as usize, sort)
            .map_err(OrbError::from)?;

        // Decorated folder by folder — pending actions and highlights are
        // per folder — and put back in the order the query gave.
        let mut by_folder: HashMap<(String, String), Vec<MessageListItem>> = HashMap::new();
        for (location, item) in &items {
            by_folder
                .entry(location.clone())
                .or_default()
                .push(item.clone());
        }
        let mut decorated: HashMap<i64, MessageHeader> = HashMap::new();
        for ((account, folder), group) in by_folder {
            for header in self.decorate_rows(&account, &folder, group, None)? {
                decorated.insert(header.id, header);
            }
        }
        Ok(items
            .iter()
            .filter_map(|(_, item)| decorated.remove(&item.id))
            .collect())
    }

    /// A page of the folder as conversations: one row per thread, newest
    /// first, and under each thread named in `expanded` its members.
    pub fn get_threads(
        &self,
        account_name: String,
        folder_name: String,
        offset: u32,
        limit: u32,
        expanded: Vec<String>,
    ) -> Result<Vec<MessageHeader>, OrbError> {
        let (rows, _) = self
            .store
            .thread_list(
                &account_name,
                &folder_name,
                offset as usize,
                limit as usize,
                &expanded,
            )
            .map_err(OrbError::from)?;
        let messages = rows.iter().map(|row| row.message.clone()).collect();
        self.decorate_rows(&account_name, &folder_name, messages, Some(&rows))
    }

    /// A page of several folders as conversations read as one list — every
    /// account's inbox, for Today — the way [`OrbHandle::get_threads`] pages
    /// one folder, each row decorated as its own folder would decorate it.
    pub fn get_inbox_threads(
        &self,
        folders: Vec<(String, String)>,
        offset: u32,
        limit: u32,
        expanded: Vec<String>,
    ) -> Result<Vec<MessageHeader>, OrbError> {
        let (rows, _) = self
            .store
            .merged_thread_list(&folders, offset as usize, limit as usize, &expanded)
            .map_err(OrbError::from)?;

        let mut by_folder: HashMap<(String, String), Vec<crate::store::ThreadRow>> = HashMap::new();
        for (location, row) in &rows {
            by_folder
                .entry(location.clone())
                .or_default()
                .push(row.clone());
        }
        let mut decorated: HashMap<i64, MessageHeader> = HashMap::new();
        for ((account, folder), group) in by_folder {
            let messages = group.iter().map(|row| row.message.clone()).collect();
            for header in self.decorate_rows(&account, &folder, messages, Some(&group))? {
                decorated.insert(header.id, header);
            }
        }
        Ok(rows
            .iter()
            .filter_map(|(_, row)| decorated.remove(&row.message.id))
            .collect())
    }

    /// How many rows [`OrbHandle::get_inbox_threads`] pages through, given
    /// what is expanded.
    pub fn inbox_thread_total(
        &self,
        folders: Vec<(String, String)>,
        expanded: Vec<String>,
    ) -> Result<u32, OrbError> {
        let (_, total) = self
            .store
            .merged_thread_list(&folders, 0, 0, &expanded)
            .map_err(OrbError::from)?;
        Ok(total as u32)
    }

    /// Where items dated `times` (unix seconds) would stand among the rows
    /// Today's inboxes list by date — [`OrbHandle::get_inbox_messages`], or
    /// [`OrbHandle::get_inbox_threads`] when `threads` — so that chat and
    /// reading can be read into the mail as one timeline: for each, how many
    /// rows come before it. Threads are newest first whatever `newest_first`
    /// says.
    pub fn inbox_ranks(
        &self,
        folders: Vec<(String, String)>,
        times: Vec<i64>,
        newest_first: bool,
        threads: bool,
        expanded: Vec<String>,
    ) -> Result<Vec<u32>, OrbError> {
        let ranks = if threads {
            self.store.merged_thread_ranks(&folders, &times, &expanded)
        } else {
            self.store
                .merged_message_ranks(&folders, &times, newest_first)
        }
        .map_err(OrbError::from)?;
        Ok(ranks.into_iter().map(|rank| rank as u32).collect())
    }

    /// How many messages a folder holds now. The folder list carries a count
    /// too, but only as fresh as the list, which a long sync refreshes only
    /// at its end; a folder opened in between would be taken for empty.
    pub fn folder_total(&self, account_name: String, folder_name: String) -> Result<u32, OrbError> {
        let db = self.store.reader().map_err(OrbError::from)?;
        match db
            .get_folder_by_name(&account_name, &folder_name)
            .map_err(OrbError::from)?
            .and_then(|folder| folder.id)
        {
            Some(id) => Ok(db.get_folder_counts(id).map_err(OrbError::from)?.1),
            None => Ok(0),
        }
    }

    /// How many rows the threaded list of a folder has, given what is
    /// expanded.
    pub fn thread_total(
        &self,
        account_name: String,
        folder_name: String,
        expanded: Vec<String>,
    ) -> Result<u32, OrbError> {
        let (_, total) = self
            .store
            .thread_list(&account_name, &folder_name, 0, 0, &expanded)
            .map_err(OrbError::from)?;
        Ok(total as u32)
    }

    /// The whole conversation a message is part of, across every folder of
    /// its account, oldest first.
    pub fn thread_messages(&self, message_id: i64) -> Result<Vec<SearchResult>, OrbError> {
        let results = self
            .store
            .thread_messages(message_id)
            .map_err(OrbError::from)?;
        self.decorate_hits(results)
    }

    /// The members of the threads `threads` within `folders` — the folder,
    /// or Today's inboxes, the threaded list is read from — thread by thread
    /// in the order given, oldest first within each, each decorated as its
    /// folder decorates its rows. What a collapsed thread's row stands for:
    /// the conversation the reader shows, and what an action on the row
    /// acts on.
    pub fn thread_members(
        &self,
        folders: Vec<(String, String)>,
        threads: Vec<String>,
    ) -> Result<Vec<MessageHeader>, OrbError> {
        let members = self
            .store
            .merged_thread_members(&folders, &threads)
            .map_err(OrbError::from)?;
        let mut by_folder: HashMap<(String, String), Vec<MessageListItem>> = HashMap::new();
        for (location, item) in &members {
            by_folder
                .entry(location.clone())
                .or_default()
                .push(item.clone());
        }
        let mut decorated: HashMap<i64, MessageHeader> = HashMap::new();
        for ((account, folder), group) in by_folder {
            for header in self.decorate_rows(&account, &folder, group, None)? {
                decorated.insert(header.id, header);
            }
        }
        Ok(members
            .iter()
            .filter_map(|(_, item)| decorated.remove(&item.id))
            .collect())
    }

    /// The list rows of a folder page, with what the list draws beside the
    /// headers: the attachment clip, the staged action, the highlights and,
    /// for a threaded page, the thread.
    fn decorate_rows(
        &self,
        account_name: &str,
        folder_name: &str,
        messages: Vec<MessageListItem>,
        threads: Option<&[crate::store::ThreadRow]>,
    ) -> Result<Vec<MessageHeader>, OrbError> {
        let store = &self.store;

        // Annotate each row with a real attachment flag, derived from any
        // already-cached RFC822 source. Messages whose body has not been
        // fetched yet report false until opened.
        let ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
        let with_attachments = store
            .messages_with_attachments(&ids)
            .map_err(OrbError::from)?;

        // Annotate each row with any staged (pending) delete/move action, so the
        // list can render a pending marker. Keyed by UID within this folder.
        let mut pending: HashMap<u32, (PendingActionType, Option<String>)> = store
            .list_pending(&RustPendingScope::Folder {
                account_name: account_name.to_string(),
                folder_name: folder_name.to_string(),
            })
            .map_err(OrbError::from)?
            .into_iter()
            .map(|r| {
                (
                    r.uid,
                    (PendingActionType::from_db(&r.action_type), r.target_folder),
                )
            })
            .collect();

        let mut highlights = store
            .highlights_for(account_name, folder_name, &messages)
            .map_err(OrbError::from)?;
        let mut snippets = store.snippets(&ids).map_err(OrbError::from)?;
        let munger = self.subject_munger();

        let mut headers: Vec<MessageHeader> = messages
            .into_iter()
            .enumerate()
            .map(|(position, m)| {
                let has = with_attachments.contains(&m.id);
                let snippet = snippets.remove(&m.id);
                let (pending_action, pending_target) = match pending.remove(&m.uid) {
                    Some((action, target)) => (Some(action), target),
                    None => (None, None),
                };
                let matched = highlights.remove(&m.id);
                let mut header = MessageHeader::from_item(m, has)
                    .with_pending_action(pending_action)
                    .with_highlights(matched);
                header.account_name = account_name.to_string();
                header.folder_name = folder_name.to_string();
                header.snippet = snippet;
                header.pending_target = pending_target;
                munger.apply(&mut header);
                match threads.and_then(|rows| rows.get(position)) {
                    Some(row) => header.with_thread(row),
                    None => header,
                }
            })
            .collect();
        if let Some(rows) = threads {
            self.summarise_conversations(&mut headers, rows)?;
        }
        Ok(headers)
    }

    /// Make the row of each thread shown collapsed stand for the whole
    /// thread: read only when every member is, flagged or with files when
    /// any is, staged when every member is staged alike, marked by every
    /// highlight rule a member matches, and showing who wrote last and what.
    /// `headers` and `rows` go together by position.
    fn summarise_conversations(
        &self,
        headers: &mut [MessageHeader],
        rows: &[crate::store::ThreadRow],
    ) -> Result<(), OrbError> {
        let store = &self.store;
        let collapsed: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !row.members.is_empty())
            .map(|(position, _)| position)
            .collect();
        if collapsed.is_empty() {
            return Ok(());
        }

        // Asked once per folder for the whole page, not once per thread.
        let mut by_folder: HashMap<(String, String), Vec<MessageListItem>> = HashMap::new();
        for &position in &collapsed {
            for (location, item) in &rows[position].members {
                by_folder
                    .entry(location.clone())
                    .or_default()
                    .push(item.clone());
            }
        }
        let mut highlights: HashMap<i64, Vec<String>> = HashMap::new();
        let mut staged: HashMap<i64, (PendingActionType, Option<String>)> = HashMap::new();
        for ((account, folder), items) in &by_folder {
            highlights.extend(
                store
                    .highlights_for(account, folder, items)
                    .map_err(OrbError::from)?,
            );
            let pending: HashMap<u32, (PendingActionType, Option<String>)> = store
                .list_pending(&RustPendingScope::Folder {
                    account_name: account.clone(),
                    folder_name: folder.clone(),
                })
                .map_err(OrbError::from)?
                .into_iter()
                .map(|r| {
                    (
                        r.uid,
                        (PendingActionType::from_db(&r.action_type), r.target_folder),
                    )
                })
                .collect();
            for item in items {
                if let Some(action) = pending.get(&item.uid) {
                    staged.insert(item.id, action.clone());
                }
            }
        }
        let member_ids: Vec<i64> = by_folder.values().flatten().map(|m| m.id).collect();
        let with_attachments = store
            .messages_with_attachments(&member_ids)
            .map_err(OrbError::from)?;
        let newest_of = |members: &[(crate::store::Location, MessageListItem)]| {
            members
                .iter()
                .map(|(_, item)| item)
                .max_by_key(|item| (crate::threads::date_key(item.date.as_deref()), item.id))
                .cloned()
        };
        let newest_ids: Vec<i64> = collapsed
            .iter()
            .filter_map(|&position| newest_of(&rows[position].members).map(|m| m.id))
            .collect();
        let mut snippets = store.snippets(&newest_ids).map_err(OrbError::from)?;
        let rule_order: HashMap<String, usize> = self
            .config()
            .highlights
            .rules
            .iter()
            .enumerate()
            .map(|(index, rule)| (rule.name.clone(), index))
            .collect();

        for position in collapsed {
            let members = &rows[position].members;
            let Some(header) = headers.get_mut(position) else {
                continue;
            };
            let Some(newest) = newest_of(members) else {
                continue;
            };
            let flagged = |name: &str| members.iter().any(|(_, m)| has_system_flag(&m.flags, name));
            header.is_read = members
                .iter()
                .all(|(_, m)| has_system_flag(&m.flags, "Seen"));
            header.is_flagged = flagged("Flagged");
            header.has_attachments = members
                .iter()
                .any(|(_, m)| with_attachments.contains(&m.id));
            // Whether the last word has been answered, not the first.
            header.is_answered = has_system_flag(&newest.flags, "Answered");
            header.is_forwarded = newest
                .flags
                .iter()
                .any(|f| f.trim_start_matches('$').eq_ignore_ascii_case("Forwarded"));
            header.thread_newest_from = newest.from.clone();
            header.snippet = snippets.remove(&newest.id);

            let first = staged.get(&members[0].1.id);
            let alike = first.is_some() && members.iter().all(|(_, m)| staged.get(&m.id) == first);
            let (action, target) = match first {
                Some((action, target)) if alike => (Some(*action), target.clone()),
                _ => (None, None),
            };
            header.pending_action = action;
            header.pending_target = target;

            let mut names: Vec<String> = Vec::new();
            for (_, member) in members {
                for name in highlights.get(&member.id).into_iter().flatten() {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
            }
            names.sort_by_key(|name| rule_order.get(name).copied().unwrap_or(usize::MAX));
            header.highlights = names;
        }
        Ok(())
    }

    /// Full-text search across the whole mailbox.
    ///
    /// Unlike the client-side filtering of an already-loaded page, this queries
    /// the local FTS index over every synced message (subject / from / to /
    /// body) and returns matches from any folder, each tagged with its owning
    /// account and folder so the caller can open it. `scope` narrows the search
    /// to an account or folder; pass [`SearchScope::All`] for the whole mailbox.
    ///
    /// `sort` orders the hits before the result limit applies; `None` keeps
    /// the query's own order, by relevance.
    pub fn search_messages(
        &self,
        query: String,
        scope: SearchScope,
        sort: Option<MessageSortOrder>,
    ) -> Result<Vec<SearchResult>, OrbError> {
        let store = &self.store;
        let results = store
            .search_message_results(&query, &scope.into(), sort.map(RustMessageSort::from))
            .map_err(OrbError::from)?;
        self.decorate_hits(results)
    }

    /// The messages of the list on screen that a filter keeps (§4.5): the
    /// query, in the search language, over these folders only — one for a
    /// folder, every inbox for Today — in `sort` order.
    pub fn filter_messages(
        &self,
        query: String,
        folders: Vec<(String, String)>,
        sort: Option<MessageSortOrder>,
    ) -> Result<Vec<SearchResult>, OrbError> {
        let results = self
            .store
            .search_message_results(
                &query,
                &RustSearchScope::Folders(folders),
                sort.map(RustMessageSort::from),
            )
            .map_err(OrbError::from)?;
        self.decorate_hits(results)
    }

    /// Hits from several folders, each highlighted as its folder would
    /// highlight it, so the same message never looks different in two
    /// places.
    fn decorate_hits(&self, results: Vec<SearchResultItem>) -> Result<Vec<SearchResult>, OrbError> {
        let store = &self.store;
        let mut by_folder: HashMap<(String, String), Vec<MessageListItem>> = HashMap::new();
        for hit in &results {
            by_folder
                .entry((hit.account_name.clone(), hit.folder_name.clone()))
                .or_default()
                .push(hit.message.clone());
        }
        let mut highlights: HashMap<i64, Vec<String>> = HashMap::new();
        for ((account, folder), items) in &by_folder {
            highlights.extend(
                store
                    .highlights_for(account, folder, items)
                    .map_err(OrbError::from)?,
            );
        }

        let ids: Vec<i64> = results.iter().map(|hit| hit.message.id).collect();
        let mut snippets = store.snippets(&ids).map_err(OrbError::from)?;
        let munger = self.subject_munger();
        Ok(results
            .into_iter()
            .map(|item| {
                let matched = highlights.remove(&item.message.id);
                let snippet = snippets.remove(&item.message.id);
                let mut result = SearchResult::from(item);
                result.message = result.message.with_highlights(matched);
                result.message.snippet = snippet;
                munger.apply(&mut result.message);
                result
            })
            .collect())
    }

    /// The highlight rules with their styles, in rule order.
    pub fn highlight_styles(&self) -> Vec<crate::highlights::HighlightStyle> {
        self.config().highlights.styles()
    }

    /// How many messages of a folder each highlight rule matches, for
    /// `:highlights`.
    pub fn highlight_counts(
        &self,
        account: String,
        folder: String,
    ) -> Result<Vec<HighlightCount>, OrbError> {
        Ok(self.store.highlight_counts(&account, &folder)?)
    }

    /// The unread counts of the highlight rules with `sidebar_badge`, per
    /// folder.
    pub fn highlight_badges(&self) -> Result<Vec<HighlightBadge>, OrbError> {
        Ok(self.store.highlight_badges()?)
    }

    /// Return the single newest unread message across the whole mailbox, or
    /// `None` when there is no unread mail.
    ///
    /// Used to build a new-mail notification's representative sender / subject
    /// and its click-to-open target. Because it scans every folder of every
    /// account (not just the currently-loaded page), the sample is correct even
    /// when the mail arrived in a folder the user is not viewing.
    pub fn latest_unread_message(&self) -> Result<Option<MessageHeader>, OrbError> {
        let store = &self.store;
        let message = store.latest_unread_message().map_err(OrbError::from)?;
        Ok(message.map(MessageHeader::from))
    }

    /// The message exactly as the server has it, headers and all.
    ///
    /// Everything the reader shows is a rendering of this — a chosen part,
    /// decoded, with the headers reduced to four lines — so it is the only view
    /// that can answer why a message looks the way it does, or where it really
    /// came from.
    ///
    /// Fetched on demand rather than kept: full sources are the reason a
    /// mailbox is measured in gigabytes, and this is asked for rarely.
    pub fn raw_message(&self, message_id: i64) -> Result<String, OrbError> {
        let store = &self.store;
        let message = self
            .runtime
            .block_on(crate::store::fetch_message_body(
                store,
                message_id,
                crate::store::BodyDetail::Source,
            ))
            .map_err(OrbError::from)?;

        let raw = message.rfc822.ok_or_else(|| OrbError::NotFound {
            message: "the server returned no source for this message".to_string(),
        })?;
        // Lossy: mail is full of headers that claim one encoding and use
        // another, and showing the source with a replacement character in it
        // beats refusing to show it at all.
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }

    /// Get full message detail by ID
    pub fn get_message(&self, message_id: i64) -> Result<MessageDetail, OrbError> {
        self.message_detail(message_id, crate::store::BodyFetch::Complete)
    }

    /// A message as stored, if any of it can be read, without waiting for the
    /// server; only a message with nothing readable stored is fetched. What
    /// the reader asks first, so that a slow connection or none does not hold
    /// up opening it. `incomplete` says when [`Self::get_message`] would bring
    /// more.
    pub fn get_message_stored_first(&self, message_id: i64) -> Result<MessageDetail, OrbError> {
        self.message_detail(message_id, crate::store::BodyFetch::StoredFirst)
    }

    fn message_detail(
        &self,
        message_id: i64,
        fetch: crate::store::BodyFetch,
    ) -> Result<MessageDetail, OrbError> {
        let store = &self.store;

        // Fetch message body (may require IMAP fetch)
        let (message, complete) = self
            .runtime
            .block_on(crate::store::load_message_body(
                store,
                message_id,
                crate::store::BodyDetail::Readable,
                fetch,
            ))
            .map_err(OrbError::from)?;

        // Reading a message fetches its text, not its whole source, which
        // the background pass brings later. Until then the message's
        // structure, recorded when it was shaped, says whether there are
        // attachments; without it the reader would never ask for the list.
        let has_attachments = match message.rfc822.as_deref() {
            Some(source) => crate::attachments::has_attachments(source),
            None => store
                .messages_with_attachments(&[message_id])
                .is_ok_and(|with| with.contains(&message_id)),
        };

        let subject_icons = self.subject_munger().display(message.subject.as_deref()).1;

        let remote_images_allowed = message
            .from
            .as_deref()
            .is_some_and(|from| self.config().allows_remote_images_from(from))
            || store
                .remote_images_rule_for_message(message_id)
                .map_err(OrbError::from)?
                .is_some();
        let highlights = store
            .highlights_for_message(message_id)
            .map_err(OrbError::from)?;

        Ok(MessageDetail {
            highlights,
            id: message.id.unwrap_or(message_id),
            uid: message.uid,
            message_id: message.message_id,
            from_display: message.from,
            to_display: message.to,
            cc_display: message.cc,
            subject: message.subject,
            date: message.date,
            body_text: message.body_text,
            remote_images_allowed,
            body_html: message.body_html,
            flags: message.flags,
            has_attachments,
            subject_icons,
            incomplete: !complete,
        })
    }

    /// What a message's headers say about its sender: whether the receiving
    /// server found it forged, and whether answers go to another domain.
    pub fn sender_check(&self, message_id: i64) -> Result<crate::safety::SenderCheck, OrbError> {
        let known =
            |checks: &HashMap<i64, crate::safety::SenderCheck>| checks.get(&message_id).cloned();
        if let Some(check) = known(&self.sender_checks.lock().unwrap_or_else(|e| e.into_inner())) {
            return Ok(check);
        }
        let check = self
            .runtime
            .block_on(crate::store::fetch_sender_check(&self.store, message_id))
            .map_err(OrbError::from)?;
        let mut checks = self.sender_checks.lock().unwrap_or_else(|e| e.into_inner());
        // Bounded: a long session reads thousands of messages.
        if checks.len() >= SENDER_CHECKS_KEPT {
            checks.clear();
        }
        checks.insert(message_id, check.clone());
        Ok(check)
    }

    /// The web addresses the `message_links` rules find in a message, in
    /// rule order.
    pub fn message_links(
        &self,
        message_id: i64,
    ) -> Result<Vec<crate::message_links::MessageLink>, OrbError> {
        let db = self.store.reader().map_err(OrbError::from)?;
        let message = db.get_message_by_id(message_id).map_err(OrbError::from)?;
        let mut links = message
            .map(|message| message_links_of(&self.config().message_links, &message))
            .unwrap_or_default();
        // A feed item's article comes first.
        if let Some(url) = db
            .extra_header(message_id, crate::feeds::LINK_HEADER)
            .map_err(OrbError::from)?
        {
            links.insert(
                0,
                crate::message_links::MessageLink {
                    name: Some("article".to_string()),
                    url,
                },
            );
        }
        Ok(links)
    }

    /// The web addresses a task points to, the one to open first first: what
    /// the `message_links` rules find in the subject of its first linked
    /// message — what `open-link` on that message opens — then an address on
    /// the first line of its notes, then one in its title, then what the
    /// rules find in its title.
    pub fn task_links(
        &self,
        task_id: i64,
    ) -> Result<Vec<crate::message_links::MessageLink>, OrbError> {
        let detail = self.get_task(task_id)?;
        let first_line = detail
            .body
            .as_deref()
            .and_then(|body| body.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or_default();
        let rules = &self.config().message_links;
        // What the message's own `open-link` opens; what the rules find in
        // the subject remembered with the link where the message is not here.
        let mut links = detail
            .links
            .first()
            .map(|link| {
                let copy = self
                    .store
                    .find_messages_by_rfc_id(&link.message_id)
                    .ok()
                    .and_then(|copies| copies.into_iter().next());
                match copy {
                    Some(copy) => self.message_links(copy.message.id).unwrap_or_default(),
                    None => crate::message_links::links_for(
                        rules,
                        crate::message_links::LinkSource {
                            subject: link.subject.as_deref().unwrap_or_default(),
                            ..Default::default()
                        },
                    ),
                }
            })
            .unwrap_or_default();
        links.extend(
            [first_line, &detail.task.title]
                .into_iter()
                .flat_map(crate::message_links::urls_in)
                .map(|url| crate::message_links::MessageLink { name: None, url }),
        );
        links.extend(crate::message_links::links_for(
            rules,
            crate::message_links::LinkSource {
                subject: &detail.task.title,
                ..Default::default()
            },
        ));
        let mut seen = std::collections::HashSet::new();
        links.retain(|link| seen.insert(link.url.clone()));
        Ok(links)
    }

    /// The configured subject rewriting, compiled for one batch of rows.
    fn subject_munger(&self) -> crate::subject_munger::SubjectMunger {
        crate::subject_munger::SubjectMunger::from_config(&self.config().subject_munges)
    }

    /// The images a message embeds for its own HTML to show.
    ///
    /// Only parts with a `Content-ID` qualify: those are what a `cid:`
    /// reference can name. Carried as base64 so the interface can turn each one
    /// into a `data:` URL, which is the only kind of image the HTML rendering
    /// is allowed to load.
    pub fn inline_images(&self, message_id: i64) -> Result<Vec<InlineImage>, OrbError> {
        use base64::Engine;

        let mut images = Vec::new();
        for meta in self.list_attachments(message_id)? {
            let Some(content_id) = meta.content_id else {
                continue;
            };
            let data = self.extract_attachment(message_id, meta.part_id)?;
            images.push(InlineImage {
                content_id,
                content_type: data.content_type,
                data_base64: base64::engine::general_purpose::STANDARD.encode(&data.data),
            });
        }
        Ok(images)
    }

    /// List the attachments of a message.
    ///
    /// Ensures the message's RFC822 source is present first (fetching it from
    /// IMAP on demand via the same path used to load the body), then parses the
    /// MIME structure to enumerate attachment parts. Returns an empty list for a
    /// message with no attachments.
    pub fn list_attachments(&self, message_id: i64) -> Result<Vec<AttachmentMeta>, OrbError> {
        let store = &self.store;

        // Ensure the full RFC822 source is available (may require IMAP fetch).
        let message = self
            .runtime
            .block_on(crate::store::fetch_message_body(
                store,
                message_id,
                crate::store::BodyDetail::Source,
            ))
            .map_err(OrbError::from)?;

        let attachments = message
            .rfc822
            .as_deref()
            .map(crate::attachments::list_attachments)
            .unwrap_or_default();

        Ok(attachments.into_iter().map(AttachmentMeta::from).collect())
    }

    /// Extract the decoded bytes of a single attachment.
    ///
    /// Ensures the message's RFC822 source is present (fetching it from IMAP on
    /// demand via the same path used to load the body), re-parses the MIME tree,
    /// locates the part identified by `part_id` (as reported by
    /// [`OrbHandle::list_attachments`]), and returns its transfer-decoded bytes
    /// along with a filename and content type. Errors with `NotFound` when the
    /// source is unavailable or the part id does not resolve.
    pub fn extract_attachment(
        &self,
        message_id: i64,
        part_id: String,
    ) -> Result<AttachmentData, OrbError> {
        let store = &self.store;

        // Ensure the full RFC822 source is available (may require IMAP fetch).
        let message = self
            .runtime
            .block_on(crate::store::fetch_message_body(
                store,
                message_id,
                crate::store::BodyDetail::Source,
            ))
            .map_err(OrbError::from)?;

        let rfc822 = message.rfc822.ok_or_else(|| OrbError::NotFound {
            message: format!("Message {} has no source to extract from", message_id),
        })?;

        let extracted =
            crate::attachments::extract_attachment(&rfc822, &part_id).ok_or_else(|| {
                OrbError::NotFound {
                    message: format!("Attachment part '{}' not found", part_id),
                }
            })?;

        Ok(AttachmentData::from(extracted))
    }

    /// The attachment as a file on disk, for the system to open, preview or
    /// take by drag.
    ///
    /// Written once under the attachment cache, beside the task attachments,
    /// and reused after that; the filename is the attachment's own, made safe
    /// for a path. The source is fetched from the server if it is not stored,
    /// which is what the source prefetch of the backfill exists to avoid.
    pub fn attachment_path(&self, message_id: i64, part_id: String) -> Result<String, OrbError> {
        let data = self.extract_attachment(message_id, part_id.clone())?;
        let dir = attachment_cache_root()
            .join("mail")
            .join(message_id.to_string())
            .join(part_id.replace(['/', '\\'], "_"));
        let path = dir.join(safe_filename(&data.filename));
        let fresh = std::fs::metadata(&path)
            .map(|m| m.len() == data.data.len() as u64)
            .unwrap_or(false);
        if !fresh {
            write_file(&path, &data.data)?;
        }
        Ok(path.to_string_lossy().into_owned())
    }

    /// Write the attachment to `path`, which a save dialog chose.
    pub fn save_attachment(
        &self,
        message_id: i64,
        part_id: String,
        path: String,
    ) -> Result<(), OrbError> {
        let data = self.extract_attachment(message_id, part_id)?;
        write_file(std::path::Path::new(&path), &data.data)
    }

    /// Sync a specific folder
    pub fn sync_folder(
        &self,
        account_name: String,
        folder_name: String,
        listener: Option<Box<dyn SyncProgressListener>>,
    ) -> Result<SyncResult, OrbError> {
        self.run_sync(Some(account_name), Some(folder_name), listener)
    }

    /// Sync all accounts and folders
    pub fn sync_all(
        &self,
        listener: Option<Box<dyn SyncProgressListener>>,
    ) -> Result<SyncResult, OrbError> {
        self.run_sync(None, None, listener)
    }

    /// What the last sync of every folder did, phase by phase, and how many
    /// connections this process has opened and why. The text the `sync-stats`
    /// command shows.
    pub fn sync_stats(&self) -> String {
        let mut report = crate::imap::trace::report();
        let export = self.task_export_status();
        if export.configured {
            let last = export.directory.as_deref().and_then(|dir| {
                crate::tasks::export::newest_complete_snapshot(std::path::Path::new(dir))
                    .ok()
                    .flatten()
            });
            report.push_str(&match last {
                Some(snapshot) => format!(
                    "\n\nTask export: last complete {} ({} bytes), {}{}\n",
                    export.last_complete.as_deref().unwrap_or("?"),
                    crate::tasks::export::verify(&snapshot.path)
                        .ok()
                        .map(|_| dir_bytes(&snapshot.path))
                        .unwrap_or(0),
                    snapshot.path.display(),
                    export
                        .problem
                        .map(|p| format!("; last attempt: {p}"))
                        .unwrap_or_default()
                ),
                None => format!(
                    "\n\nTask export: no complete snapshot in {}{}\n",
                    export.directory.as_deref().unwrap_or("?"),
                    export
                        .problem
                        .map(|p| format!("; last attempt: {p}"))
                        .unwrap_or_default()
                ),
            });
        }
        report
    }

    /// Mark messages read or unread: in the stored copy before this returns,
    /// on the server in the background.
    ///
    /// With `undo`, the change is an action the user took, recorded under
    /// that label for undo; without, it is one the app made on their behalf,
    /// such as reading a message by opening it.
    pub fn mark_read(
        &self,
        account_name: String,
        folder_name: String,
        uids: Vec<u32>,
        read: bool,
        undo: Option<String>,
    ) -> Result<(), OrbError> {
        let changed = self
            .runtime
            .block_on(
                self.store
                    .queue_read_status(&account_name, &folder_name, &uids, read),
            )
            .map_err(OrbError::from)?;
        if let Some(label) = undo {
            self.history().record(
                label,
                vec![UndoAction::Read {
                    account: account_name.clone(),
                    folder: folder_name,
                    uids: changed,
                    read: !read,
                }],
            );
        }
        self.drain_outbox_in_background(&account_name);
        Ok(())
    }

    /// Poll the reading list's feeds that are due, and append their new
    /// items to the reading folder; with `force`, every feed now.
    pub fn poll_feeds(&self, force: bool) -> Result<crate::store::FeedPollReport, OrbError> {
        self.runtime
            .block_on(self.store.poll_feeds(force))
            .map_err(OrbError::from)
    }

    /// The reading folder as the server names it, once it exists.
    pub fn reading_folder(&self) -> Option<(String, String)> {
        self.store.reading_folder(false)
    }

    /// Every feed of the reading list, as this machine last heard from it.
    pub fn feed_statuses(&self) -> Result<Vec<crate::store::FeedStatus>, OrbError> {
        self.store.feed_statuses().map_err(OrbError::from)
    }

    /// Flag or unflag messages: in the stored copy before this returns, on
    /// the server in the background. `undo` as for [`OrbHandle::mark_read`].
    pub fn mark_flagged(
        &self,
        account_name: String,
        folder_name: String,
        uids: Vec<u32>,
        flagged: bool,
        undo: Option<String>,
    ) -> Result<(), OrbError> {
        let changed = self
            .runtime
            .block_on(
                self.store
                    .queue_flag_status(&account_name, &folder_name, &uids, flagged),
            )
            .map_err(OrbError::from)?;
        if let Some(label) = undo {
            self.history().record(
                label,
                vec![UndoAction::Flagged {
                    account: account_name.clone(),
                    folder: folder_name,
                    uids: changed,
                    flagged: !flagged,
                }],
            );
        }
        self.drain_outbox_in_background(&account_name);
        Ok(())
    }

    /// Mark the message a sent reply answers `\Answered`, or the one a
    /// forward forwards `$Forwarded`, here at once and on its server in the
    /// background. The mail went out either way, so a failure is logged
    /// rather than reported.
    fn flag_original(&self, in_reply_to: &str, flag: &str) {
        match self
            .runtime
            .block_on(self.store.mark_original(in_reply_to, flag))
        {
            Ok(accounts) => {
                for account in accounts {
                    self.drain_outbox_in_background(&account);
                }
            }
            Err(e) => crate::logger::warn(&format!("could not mark {in_reply_to} {flag}: {e}")),
        }
    }

    /// Carry the account's queued server writes out without the caller
    /// waiting for the server. A write the server cannot be reached for stays
    /// queued for the next sync; one it refuses is dropped with a warning, and
    /// the next sync brings the stored copy back in line with the server.
    fn drain_outbox_in_background(&self, account_name: &str) {
        let Some(account) = self.store.get_account_config(account_name) else {
            return;
        };
        let store = Arc::clone(&self.store);
        self.runtime.spawn(async move {
            match store.drain_outbox(&account).await {
                Ok(_) => {}
                Err(e) if e.is_connection_error() => crate::logger::info(&format!(
                    "{}: flag changes queued, the server is out of reach: {e}",
                    account.name
                )),
                Err(e) => crate::logger::warn(&format!(
                    "{}: queued flag changes failed: {e}",
                    account.name
                )),
            }
        });
    }

    /// Delete messages: a move to the account's Trash folder, or a permanent
    /// delete for messages already there. The interface asks before the
    /// permanent one, using [`OrbHandle::trash_folder`] to tell them apart.
    ///
    /// The list changes now; the server follows through the outbox. A move
    /// to Trash is recorded under `undo`, when given; a permanent delete has
    /// nothing to take back.
    pub fn delete_messages(
        &self,
        account_name: String,
        folder_name: String,
        uids: Vec<u32>,
        undo: Option<String>,
    ) -> Result<DeleteOutcome, OrbError> {
        let outcome = self
            .runtime
            .block_on(
                self.store
                    .delete_messages(&account_name, &folder_name, &uids),
            )
            .map_err(OrbError::from)?;
        let destinations = match &outcome {
            DeleteOutcome::MovedToTrash { trash, moved, .. } => {
                if let Some(label) = undo {
                    self.record_move(label, &account_name, &folder_name, trash, moved);
                }
                vec![trash.clone()]
            }
            DeleteOutcome::Deleted { .. } => Vec::new(),
        };
        self.drain_then_sync(
            vec![account_name.clone()],
            destinations_of(&account_name, destinations),
        );
        Ok(outcome)
    }

    /// Move messages to another folder.
    ///
    /// The list changes now; the server follows through the outbox, and the
    /// target is synced once it has. A move the folder's UIDVALIDITY no longer
    /// allows is dropped there, and the next sync of the folder reconciles it.
    /// Recorded under `undo`, when given.
    pub fn move_messages(
        &self,
        account_name: String,
        source_folder: String,
        target_folder: String,
        uids: Vec<u32>,
        undo: Option<String>,
    ) -> Result<WriteOutcome, OrbError> {
        let outcome = self
            .runtime
            .block_on(self.store.move_messages(
                &account_name,
                &source_folder,
                &target_folder,
                &uids,
            ))
            .map_err(OrbError::from)?;
        if let Some(label) = undo {
            self.record_move(
                label,
                &account_name,
                &source_folder,
                &target_folder,
                &outcome.moved,
            );
        }
        self.drain_then_sync(
            vec![account_name.clone()],
            destinations_of(&account_name, vec![target_folder]),
        );
        Ok(outcome)
    }

    /// Carry out the accounts' queued server writes in the background, then
    /// sync the folders they wrote to, so those show their new mail.
    fn drain_then_sync(
        &self,
        accounts: Vec<String>,
        destinations: std::collections::BTreeSet<(String, String)>,
    ) {
        let accounts: Vec<crate::config::AccountConfig> = accounts
            .iter()
            .filter_map(|name| self.store.get_account_config(name))
            .collect();
        let store = Arc::clone(&self.store);
        let scheduler = self
            .scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.runtime.spawn(async move {
            for account in accounts {
                if let Err(e) = store.drain_outbox(&account).await {
                    crate::logger::warn(&format!(
                        "{}: moves queued, not yet on the server: {e}",
                        account.name
                    ));
                }
            }
            if let Some(scheduler) = scheduler {
                for (account, folder) in destinations {
                    scheduler.request(&account, Some(&folder));
                }
            }
        });
    }

    /// Copy messages to another folder of the same account, on the server.
    /// The target is synced afterwards, so the copies show without waiting.
    /// Recorded under `undo`, when given.
    pub fn copy_messages(
        &self,
        account_name: String,
        source_folder: String,
        target_folder: String,
        uids: Vec<u32>,
        undo: Option<String>,
    ) -> Result<WriteOutcome, OrbError> {
        let copies = self.new_arrivals(
            &account_name,
            &source_folder,
            &account_name,
            &target_folder,
            &uids,
        );
        let outcome = self
            .runtime
            .block_on(self.store.copy_messages(
                &account_name,
                &source_folder,
                &target_folder,
                &uids,
            ))
            .map_err(OrbError::from)?;
        if let Some(label) = undo {
            self.history().record(
                label,
                vec![UndoAction::Uncopy {
                    from: Place::new(&account_name, &source_folder),
                    to: Place::new(&account_name, &target_folder),
                    message_ids: copies,
                }],
            );
        }
        self.request_sync(account_name, Some(target_folder));
        Ok(outcome)
    }

    /// The `Message-ID`s of the UIDs of `from` that `to` does not hold yet:
    /// what a copy or move from one to the other brings, and so what undoing
    /// it may take away again.
    fn new_arrivals(
        &self,
        from_account: &str,
        from_folder: &str,
        to_account: &str,
        to_folder: &str,
        uids: &[u32],
    ) -> Vec<String> {
        let mut ids = self
            .store
            .message_ids_for_uids(from_account, from_folder, uids);
        if let Ok(there) = self.store.stored_message_ids(to_account, to_folder, &ids) {
            ids.retain(|id| !there.contains(id));
        }
        ids
    }

    /// Move or copy messages into a folder of another account: fetched whole
    /// from the one server and appended to the other, with their flags. The
    /// target is synced afterwards. Recorded under `undo`, when given.
    #[allow(clippy::too_many_arguments)]
    pub fn transfer_messages(
        &self,
        from_account: String,
        from_folder: String,
        to_account: String,
        to_folder: String,
        uids: Vec<u32>,
        keep_source: bool,
        undo: Option<String>,
    ) -> Result<TransferOutcome, OrbError> {
        let arrivals =
            self.new_arrivals(&from_account, &from_folder, &to_account, &to_folder, &uids);
        let outcome = self
            .runtime
            .block_on(self.store.transfer_messages(
                &from_account,
                &from_folder,
                &to_account,
                &to_folder,
                &uids,
                keep_source,
            ))
            .map_err(OrbError::from)?;
        if let Some(label) = undo
            && outcome.transferred > 0
        {
            let (from, to) = (
                Place::new(&from_account, &from_folder),
                Place::new(&to_account, &to_folder),
            );
            let step = match keep_source {
                true => UndoAction::Uncopy {
                    from,
                    to,
                    message_ids: arrivals,
                },
                false => UndoAction::MoveAcross {
                    from: to,
                    to: from,
                    message_ids: arrivals,
                },
            };
            self.history().record(label, vec![step]);
        }
        if outcome.transferred > 0 {
            self.request_sync(to_account, Some(to_folder));
        }
        Ok(outcome)
    }

    /// Record a move from `from` to `to` for undo. A move whose messages
    /// carried no `Message-ID` cannot be found again, and is not recorded.
    fn record_move(&self, label: String, account: &str, from: &str, to: &str, moved: &[String]) {
        self.history().record(
            label,
            vec![UndoAction::Move {
                account: account.to_string(),
                from: to.to_string(),
                to: from.to_string(),
                message_ids: moved.to_vec(),
            }],
        );
    }

    /// Bring the messages named by `message_ids` out of `from_folder` into
    /// `to_folder`, which is how a move is undone. Returns how many came;
    /// a message the server no longer has in `from_folder` is left where it
    /// is.
    ///
    /// The outbox is drained first: until it has carried the move out, the
    /// messages are not in `from_folder` to be found.
    ///
    /// Both folders are asked to sync afterwards, so the list shows the
    /// messages back where they were without waiting for the next timer.
    fn move_back(
        &self,
        account_name: &str,
        from_folder: &str,
        to_folder: &str,
        message_ids: &[String],
    ) -> Result<u32, OrbError> {
        if let Some(account) = self.store.get_account_config(account_name) {
            self.runtime
                .block_on(self.store.drain_outbox(&account))
                .map_err(OrbError::from)?;
        }
        let moved = self
            .runtime
            .block_on(self.store.move_by_message_ids(
                account_name,
                from_folder,
                to_folder,
                message_ids,
            ))
            .map_err(OrbError::from)?;
        if moved > 0 {
            self.request_sync(account_name.to_string(), Some(to_folder.to_string()));
            self.request_sync(account_name.to_string(), Some(from_folder.to_string()));
        }
        Ok(moved as u32)
    }

    /// Get the configured trash folder name for an account.
    ///
    /// Used by the UI to move messages to Trash (a recoverable operation)
    /// instead of permanently deleting them. Falls back to the default
    /// ("Trash") when the account is unknown.
    pub fn trash_folder(&self, account_name: String) -> String {
        self.store
            .get_account_config(&account_name)
            .map(|a| a.trash_folder)
            .unwrap_or_else(|| trash_folder_for(&self.config(), &account_name))
    }

    /// Unread mail in every account's INBOX, for a badge on the Dock.
    pub fn inbox_unread_count(&self) -> Result<u32, OrbError> {
        self.store.inbox_unread_count().map_err(OrbError::from)
    }

    /// The counts `dock_badges` asks for, in its order, or `None` when it
    /// asks for none and the Dock shows [`OrbHandle::inbox_unread_count`].
    pub fn dock_badges(&self) -> Result<Option<Vec<DockBadgeCount>>, OrbError> {
        let config = self.config();
        if config.dock_badges.is_empty() {
            return Ok(None);
        }
        let chat = self
            .chat_badge_count()
            .filter(|&n| n > 0)
            .map(|count| DockBadgeCount { count, color: None });
        config
            .dock_badges
            .iter()
            .map(|badge| {
                Ok(DockBadgeCount {
                    count: self
                        .store
                        .unread_count_in(badge.account.as_deref(), &badge.folder)?,
                    color: badge.color.clone(),
                })
            })
            .chain(chat.map(Ok))
            .collect::<Result<Vec<_>, OrbError>>()
            .map(Some)
    }

    /// The threads in Waiting, when `chat.badge` puts them on the Dock badge;
    /// a muted room's are not in it.
    pub fn chat_badge_count(&self) -> Option<u32> {
        let config = self.config();
        if !config.chat.as_ref().is_some_and(|chat| chat.badge) {
            return None;
        }
        let waiting = self.chat().ok()?.waiting().ok()?;
        Some(waiting.iter().filter(|w| !w.muted).count() as u32)
    }

    /// Whether `navigation.escalate_command` is set.
    pub fn escalates(&self) -> bool {
        self.config().navigation.escalate_command.is_some()
    }

    /// Hand a Ctrl+H/J/K/L that Orb has no use for to
    /// `navigation.escalate_command`, with `direction` — `left`, `right`, `up`
    /// or `down` — as its argument. Without one configured, nothing happens.
    ///
    /// Not waited for: moving the focus is the command's business, and the key
    /// that asked for it should not stall on it.
    #[cfg(feature = "desktop")]
    pub fn escalate(&self, direction: &str) -> Result<(), OrbError> {
        if !["left", "right", "up", "down"].contains(&direction) {
            return Err(OrbError::InvalidInput {
                message: format!("no direction {direction:?}"),
            });
        }
        let Some(command) = self.config().navigation.escalate_command.clone() else {
            return Ok(());
        };
        let mut child = std::process::Command::new(&command)
            .arg(direction)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| OrbError::ConfigError {
                message: format!("navigation.escalate_command {command}: {e}"),
            })?;
        // Reaped, or every key that escalated would leave a zombie behind.
        std::thread::spawn(move || child.wait());
        Ok(())
    }

    /// The words a search would highlight: the text and phrase terms of
    /// `query`, without the negated ones and without the structured terms
    /// (`is:unread`, `in:Archive`), which match nothing visible.
    pub fn highlight_terms(&self, query: String) -> Vec<String> {
        use crate::search::query::{Term, parse};
        let Ok(parsed) = parse(&query) else {
            return Vec::new();
        };
        let mut terms: Vec<String> = Vec::new();
        for clause in parsed.all_clauses().filter(|c| !c.negated) {
            let words: Vec<String> = match &clause.term {
                Term::Text { value, .. } | Term::Semantic(value) => {
                    value.split_whitespace().map(str::to_string).collect()
                }
                Term::Phrase { value, .. } => vec![value.clone()],
                _ => Vec::new(),
            };
            for word in words {
                let word = word
                    .trim_matches(|c: char| !c.is_alphanumeric())
                    .to_string();
                if word.chars().count() >= 2 && !terms.iter().any(|t| t.eq_ignore_ascii_case(&word))
                {
                    terms.push(word);
                }
            }
        }
        terms
    }

    /// Stage a destructive action (delete or move) for one or more messages.
    ///
    /// Nothing is sent to the server: the action is persisted in the local
    /// `pending_actions` table (surviving relaunch and shared across windows)
    /// until [`OrbHandle::execute_pending_actions`] flushes it. Staging again for
    /// a message that already has a pending action replaces it (last action
    /// wins). A `Move` requires a non-empty `target_folder`.
    pub fn stage_action(
        &self,
        account_name: String,
        folder_name: String,
        uids: Vec<u32>,
        action_type: PendingActionType,
        target_folder: Option<String>,
        undo: Option<String>,
    ) -> Result<(), OrbError> {
        let before = self.staged_before(&account_name, &folder_name, &uids, &undo)?;
        self.runtime
            .block_on(self.store.stage_action(
                &account_name,
                &folder_name,
                &uids,
                action_type.as_db(),
                target_folder,
            ))
            .map_err(OrbError::from)?;
        self.record_staged(undo, account_name, folder_name, uids, before);
        Ok(())
    }

    /// The staged actions on `uids` before a change that `undo` names, if it
    /// names one: what undoing the change puts back.
    fn staged_before(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        undo: &Option<String>,
    ) -> Result<Vec<RustPendingActionRecord>, OrbError> {
        match undo {
            Some(_) => Ok(self.store.pending_for_uids(account, folder, uids)?),
            None => Ok(Vec::new()),
        }
    }

    /// Record a change to what is staged on `uids`, which had `before`.
    fn record_staged(
        &self,
        undo: Option<String>,
        account: String,
        folder: String,
        uids: Vec<u32>,
        before: Vec<RustPendingActionRecord>,
    ) {
        if let Some(label) = undo {
            self.history().record(
                label,
                vec![UndoAction::Staged {
                    account,
                    folder,
                    uids,
                    actions: before,
                }],
            );
        }
    }

    /// Remove a single staged action (unmark a message).
    pub fn unstage_action(
        &self,
        account_name: String,
        folder_name: String,
        uids: Vec<u32>,
        undo: Option<String>,
    ) -> Result<(), OrbError> {
        let before = self.staged_before(&account_name, &folder_name, &uids, &undo)?;
        self.runtime
            .block_on(
                self.store
                    .unstage_action(&account_name, &folder_name, &uids),
            )
            .map_err(OrbError::from)?;
        self.record_staged(undo, account_name, folder_name, uids, before);
        Ok(())
    }

    /// Every UID in a folder, for acting on all of it at once.
    pub fn folder_uids(
        &self,
        account_name: String,
        folder_name: String,
    ) -> Result<Vec<u32>, OrbError> {
        self.store
            .folder_uids(&account_name, &folder_name)
            .map_err(OrbError::from)
    }

    /// List the staged actions within a scope (whole mailbox / account /
    /// folder), each with the subject and sender of the message it names.
    pub fn list_pending_actions(
        &self,
        scope: PendingScope,
    ) -> Result<Vec<PendingActionInfo>, OrbError> {
        let store = &self.store;
        let records = store
            .list_pending_detailed(&scope.into())
            .map_err(OrbError::from)?;
        Ok(records
            .into_iter()
            .map(|(record, message)| {
                let mut info = PendingActionInfo::from(record);
                if let Some(message) = message {
                    info.message_id = Some(message.id);
                    info.subject = message.subject;
                    info.from_display = message.from;
                }
                info
            })
            .collect())
    }

    /// Count staged actions per (account, folder) within a scope, for badges.
    pub fn pending_action_counts(
        &self,
        scope: PendingScope,
    ) -> Result<Vec<PendingFolderCount>, OrbError> {
        let store = &self.store;
        let counts = store
            .pending_counts(&scope.into())
            .map_err(OrbError::from)?;
        Ok(counts.into_iter().map(PendingFolderCount::from).collect())
    }

    /// Execute (flush) the staged actions within a scope.
    ///
    /// Each staged action is applied through the same safe path as the immediate
    /// UI action: a move goes to its recorded target folder; a delete moves to
    /// the account's Trash (or permanently deletes when already in Trash). Rows
    /// are cleared only after their server operation succeeds. Returns the number
    /// of actions executed, and a warning for each batch the server carried
    /// out that the database could not yet follow.
    ///
    /// The moves are recorded under `undo`, when given; a permanent delete
    /// has nothing to take back. Undoing brings the messages back, but does
    /// not stage them again.
    pub fn execute_pending_actions(
        &self,
        scope: PendingScope,
        undo: Option<String>,
    ) -> Result<PendingExecution, OrbError> {
        let store = &self.store;
        let scope: RustPendingScope = scope.into();
        // Where the staged messages are going: a move's target, a delete's
        // trash. Read before they go, since applying them clears the rows.
        let staged = store.list_pending(&scope).map_err(OrbError::from)?;
        let accounts: Vec<String> = staged
            .iter()
            .map(|action| action.account.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let destinations: std::collections::BTreeSet<(String, String)> = staged
            .into_iter()
            .filter_map(|action| match action.target_folder {
                Some(target) => Some((action.account, target)),
                None => {
                    let trash = store.trash_folder_for(&action.account);
                    (action.folder != trash).then_some((action.account, trash))
                }
            })
            .collect();
        let execution = self
            .runtime
            .block_on(store.execute_pending(&scope))
            .map_err(OrbError::from)?;
        if execution.executed == 0 {
            return Ok(execution);
        }
        if let Some(label) = undo {
            let steps = execution
                .moves
                .iter()
                .map(|(account, from, to, ids)| UndoAction::Move {
                    account: account.clone(),
                    from: to.clone(),
                    to: from.clone(),
                    message_ids: ids.clone(),
                })
                .collect();
            self.history().record(label, steps);
        }
        // The list has changed already; the server follows in the background,
        // and the destinations show their new mail once it has.
        self.drain_then_sync(accounts, destinations);
        Ok(execution)
    }

    /// Create a folder on the server. Returns its name as the server has it.
    /// Recorded for undo, which deletes it again while it is empty.
    pub fn create_folder(&self, account: String, name: String) -> Result<String, OrbError> {
        let wire = self
            .runtime
            .block_on(self.store.create_folder(&account, &name))
            .map_err(OrbError::from)?;
        self.history().record(
            format!("create folder {}", name.trim()),
            vec![UndoAction::Folder {
                account,
                name: wire.clone(),
                exists: false,
            }],
        );
        Ok(wire)
    }

    /// Rename a folder, on the server and here. Returns the new name as the
    /// server has it.
    pub fn rename_folder(
        &self,
        account: String,
        from: String,
        to: String,
    ) -> Result<String, OrbError> {
        let wire = self
            .runtime
            .block_on(self.store.rename_folder(&account, &from, &to))
            .map_err(OrbError::from)?;
        self.history().record(
            format!(
                "rename {} to {}",
                crate::imap::utf7::decode(&from),
                to.trim()
            ),
            vec![UndoAction::FolderName {
                account,
                from: wire.clone(),
                to: from,
            }],
        );
        Ok(wire)
    }

    /// Delete a folder with its messages, on the server and here. Returns
    /// how many messages went with it here.
    pub fn delete_folder(&self, account: String, name: String) -> Result<u32, OrbError> {
        let removed = self
            .runtime
            .block_on(self.store.delete_folder(&account, &name))
            .map_err(OrbError::from)?;
        Ok(removed as u32)
    }

    /// Subscribe to a folder, or unsubscribe from it.
    pub fn set_folder_subscribed(
        &self,
        account: String,
        name: String,
        subscribed: bool,
    ) -> Result<(), OrbError> {
        self.runtime
            .block_on(
                self.store
                    .set_folder_subscribed(&account, &name, subscribed),
            )
            .map_err(OrbError::from)?;
        self.history().record(
            format!(
                "{} {}",
                if subscribed {
                    "subscribe to"
                } else {
                    "unsubscribe from"
                },
                crate::imap::utf7::decode(&name)
            ),
            vec![UndoAction::Subscribed {
                account,
                name,
                subscribed: !subscribed,
            }],
        );
        Ok(())
    }

    /// Run the filter rules on mail already here: the given UIDs of a folder,
    /// or the whole folder when `uids` is `None`.
    /// Recorded under `undo`, when given; the rules do not act again on
    /// what was undone, since their log says they acted on it once.
    pub fn run_filters(
        &self,
        account: String,
        folder: String,
        uids: Option<Vec<u32>>,
        undo: Option<String>,
    ) -> Result<FilterOutcome, OrbError> {
        let mut outcome = self
            .runtime
            .block_on(self.store.run_filters(&account, &folder, uids))
            .map_err(OrbError::from)?;
        self.record_filtering(undo, &mut outcome);
        Ok(outcome)
    }

    /// Run the filter rules on everything synced that some rule watches.
    pub fn run_filters_everywhere(&self, undo: Option<String>) -> Result<FilterOutcome, OrbError> {
        let mut outcome = self
            .runtime
            .block_on(self.store.run_filters_everywhere())
            .map_err(OrbError::from)?;
        self.record_filtering(undo, &mut outcome);
        Ok(outcome)
    }

    fn record_filtering(&self, undo: Option<String>, outcome: &mut FilterOutcome) {
        let steps = std::mem::take(&mut outcome.undo);
        if let Some(label) = undo {
            self.history().record(label, steps);
        }
    }

    /// What the filter rules did in each account's last run that did
    /// anything, for the review panel.
    pub fn filter_activity(&self) -> Result<Vec<FilterActivity>, OrbError> {
        Ok(self.store.filter_activity()?)
    }

    /// Discard the staged actions within a scope without applying them. Returns
    /// the number of actions removed.
    /// Recorded under `undo`, when given.
    pub fn discard_pending_actions(
        &self,
        scope: PendingScope,
        undo: Option<String>,
    ) -> Result<u32, OrbError> {
        let scope: RustPendingScope = scope.into();
        let before = match undo {
            Some(_) => self.store.list_pending(&scope)?,
            None => Vec::new(),
        };
        let discarded = self
            .runtime
            .block_on(self.store.discard_pending(&scope))
            .map_err(OrbError::from)?;
        if let Some(label) = undo {
            let mut folders: std::collections::BTreeMap<(String, String), Vec<_>> =
                Default::default();
            for record in before {
                folders
                    .entry((record.account.clone(), record.folder.clone()))
                    .or_default()
                    .push(record);
            }
            let steps = folders
                .into_iter()
                .map(|((account, folder), actions)| UndoAction::Staged {
                    account,
                    folder,
                    uids: actions.iter().map(|a| a.uid).collect(),
                    actions,
                })
                .collect();
            self.history().record(label, steps);
        }
        Ok(discarded as u32)
    }

    /// Send an email and append a copy to the account's Sent folder.
    ///
    /// Sends `input` via SMTP using the settings of the account named in
    /// `input.account_name`, then appends the RFC822 message to that account's
    /// configured Sent folder over IMAP so it appears in Sent. The SMTP send is
    /// the operation that can fail hard (mapped to `OrbError::SendError`); a
    /// failed Sent-folder append is reported non-fatally via
    /// `SendMailResult.warning` because the mail was still delivered.
    pub fn send_mail(&self, input: SendMailInput) -> Result<SendMailResult, OrbError> {
        // Resolve the sending account by configured name.
        let account = self
            .config()
            .accounts
            .iter()
            .find(|a| a.name == input.account_name)
            .cloned()
            .ok_or_else(|| OrbError::ConfigError {
                message: format!("Account '{}' not found", input.account_name),
            })?;

        // A follow-up that cannot be made is refused before the message goes
        // out, not discovered after it has.
        let follow_up = input
            .follow_up
            .as_deref()
            .map(parse_task_date)
            .transpose()?;
        if follow_up.is_some() {
            self.tasks_config()?;
        }

        let mut email = outgoing_email_from_input(&input);
        apply_identity(&account, &input, &mut email, true)?;
        email.message_id = Some(crate::smtp::new_message_id(
            &account.sending_as(email.from.as_deref()),
        ));
        // Read each picked attachment from disk (I/O) before the async send.
        email.attachments = read_attachments(&input.attachment_paths)?;

        let outcome = self
            .runtime
            .block_on(self.store.send_mail(&account, &email))
            .map_err(|e| OrbError::SendError {
                message: e.to_string(),
            })?;

        // Somebody written to is somebody worth suggesting next time. Done
        // here rather than when a draft is saved: a draft may never go
        // anywhere, and a half-typed address is not a correspondent. A
        // failure to remember must not turn a delivered message into an
        // error, so it is logged and dropped.
        let written_to: Vec<String> = email
            .to
            .iter()
            .chain(&email.cc)
            .chain(&email.bcc)
            .cloned()
            .collect();
        if let Err(e) = self.contacts().record_sent(&written_to) {
            crate::logger::warn(&format!("could not record recipients: {e}"));
        }

        let mut result = match outcome {
            crate::store::SendOutcome::Sent {
                saved_to_sent,
                warning,
            } => {
                if let Some(original) = &email.in_reply_to {
                    self.flag_original(original, "\\Answered");
                }
                if let Some(original) = &email.forwards {
                    self.flag_original(original, "$Forwarded");
                }
                SendMailResult {
                    sent: true,
                    queued: false,
                    saved_to_sent,
                    sent_folder: account.sent_folder.clone(),
                    warning,
                    follow_up: None,
                }
            }
            crate::store::SendOutcome::Queued => SendMailResult {
                sent: false,
                queued: true,
                saved_to_sent: false,
                sent_folder: account.sent_folder.clone(),
                warning: Some(
                    "The mail server cannot be reached; the message is queued and will be sent \
                     when it can."
                        .to_string(),
                ),
                follow_up: None,
            },
        };

        // The message is on its way whatever happens here, so a task that
        // could not be made is a warning, not a failed send.
        if let Some(date) = follow_up {
            match self.create_follow_up(&account, &email, date) {
                Ok(detail) => result.follow_up = Some(detail),
                Err(e) => {
                    let problem = format!("The follow-up task could not be made: {e}");
                    result.warning = Some(match result.warning.take() {
                        Some(warning) => format!("{warning} {problem}"),
                        None => problem,
                    });
                }
            }
        }
        Ok(result)
    }

    /// The task that follows a message just sent up (§25), linked to it in
    /// the Sent folder it was copied to.
    fn create_follow_up(
        &self,
        account: &crate::config::AccountConfig,
        email: &crate::smtp::OutgoingEmail,
        postpone: chrono::NaiveDate,
    ) -> Result<TaskDetail, OrbError> {
        let munger =
            crate::subject_munger::SubjectMunger::from_config(&self.config().subject_munges);
        let title = normalise_title(&munger.munge(Some(&email.subject)));
        let cached = CachedEmailMetadata {
            subject: Some(email.subject.clone()),
            from: Some(email.sender(account).to_string()),
            date: Some(Utc::now().to_rfc3339()),
            account: Some(account.name.clone()),
            folder: Some(account.sent_folder.clone()),
        };
        let tasks_account = self.tasks_config()?.account.clone();
        self.with_tasks(|_, repo, ctx| {
            let mut undo = UndoEntry {
                label: "create 1 follow-up task".to_string(),
                steps: Vec::new(),
            };
            let task_id = Self::create_linked_task(
                repo,
                ctx,
                &tasks_account,
                &title,
                email.message_id.as_deref(),
                cached,
                Some(postpone),
                None,
                &mut undo,
            )?;
            let detail = Self::load_detail(repo, task_id, self.detail_locale(), &self.store)?;
            self.record_tasks(undo);
            Ok(detail)
        })
    }

    /// One new task, linked to a message by its `Message-ID` when it has one,
    /// with what taking it back takes added to `undo`.
    #[allow(clippy::too_many_arguments)]
    fn create_linked_task(
        repo: &TaskRepository<'_>,
        ctx: &ChangeContext<'_>,
        account: &str,
        title: &str,
        rfc_message_id: Option<&str>,
        cached: CachedEmailMetadata,
        postpone: Option<chrono::NaiveDate>,
        due: Option<chrono::NaiveDate>,
        undo: &mut UndoEntry,
    ) -> Result<i64, OrbError> {
        let task = repo.create_task(title, account, ctx)?;
        let task_id = task.id.unwrap_or_default();
        undo.steps.extend(UndoEntry::for_creation(&task, "").steps);
        if let Some(canonical) = rfc_message_id.and_then(canonical_message_id) {
            repo.link_email(task_id, &canonical, &cached, ctx)?;
        }
        if postpone.is_some() {
            repo.set_postpone_date(&[task_id], postpone, ctx)?;
        }
        if due.is_some() {
            repo.set_due_date(&[task_id], due, ctx)?;
        }
        Ok(task_id)
    }

    /// The address book, opened against the same database as everything else.
    fn contacts(&self) -> crate::contacts::ContactService {
        crate::contacts::ContactService::new(&self.config().db_path, &self.config().contacts)
    }

    /// Completions for a partly-typed recipient, best first.
    ///
    /// Answered from the config and from the mail. The system address book is
    /// added by the platform layer, which merges its own results into these —
    /// reaching macOS Contacts is not something the core can do.
    pub fn suggest_recipients(&self, query: String, limit: u32) -> Vec<crate::ContactSuggestion> {
        self.contacts().suggest(&query, limit as usize)
    }

    /// Bring the address book up to date with the mail, if it has fallen behind.
    ///
    /// Returns how many correspondents it now holds, or `None` when there was
    /// nothing to do. Reading every message takes seconds on a large mailbox,
    /// so this belongs on a background thread and is skipped entirely when no
    /// mail has arrived since the last run.
    pub fn refresh_address_book(&self) -> Result<Option<usize>, OrbError> {
        if !self.contacts().auto_extract() {
            return Ok(None);
        }

        let mut db = crate::db::Database::new(&self.config().db_path)?;

        // Which folders hold mail the user wrote, rather than received. Without
        // this the address book cannot tell a correspondent from a sender.
        let mut sent = Vec::new();
        for account in &self.config().accounts {
            for folder in db.get_folders_by_account(&account.name)? {
                if folder.name.eq_ignore_ascii_case(&account.sent_folder) {
                    sent.extend(folder.id);
                }
            }
        }

        let learned = db.learn_correspondents(&sent)?;
        if let Some(total) = learned {
            crate::logger::info(&format!("address book: {total} correspondents"));
        }
        Ok(learned)
    }

    /// Get keyboard bindings configuration
    pub fn get_bindings(&self) -> BindingsConfig {
        BindingsConfig::from(&self.config().bindings)
    }
}

/// Helpers that are not part of the FFI surface.
impl OrbHandle {
    /// Run a sync, forwarding progress to `listener` while it happens.
    ///
    /// The forwarder runs on the shared runtime alongside the sync. Passing the
    /// sender into `sync` by value matters: it is the last sender being dropped
    /// at the end of the sync that closes the channel and ends the forwarder.
    fn run_sync(
        &self,
        account_name: Option<String>,
        folder_name: Option<String>,
        listener: Option<Box<dyn SyncProgressListener>>,
    ) -> Result<SyncResult, OrbError> {
        let (progress_tx, forwarder) = match listener {
            Some(listener) => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                (
                    Some(tx),
                    Some(self.runtime.spawn(forward_progress(rx, listener))),
                )
            }
            None => (None, None),
        };

        // Every configured account when none is named: that is what the
        // sync is about to do, and what the sidebar should say is happening.
        let concerned: Vec<String> = match &account_name {
            Some(name) => vec![name.clone()],
            None => self
                .config()
                .accounts
                .iter()
                .map(|a| a.name.clone())
                .collect(),
        };
        for name in &concerned {
            self.health.started(name);
        }
        let report = self.runtime.block_on(self.store.sync(
            account_name.clone(),
            folder_name.clone(),
            progress_tx,
            false,
        ));
        let report = match report {
            Ok(report) => {
                for name in &concerned {
                    self.health.finished(name, &report);
                }
                report
            }
            Err(error) => {
                for name in &concerned {
                    self.health.failed(
                        name,
                        folder_name.as_deref(),
                        error.kind(),
                        &error.to_string(),
                    );
                }
                return Err(OrbError::from(error));
            }
        };

        // Let the forwarder drain what is already queued before returning.
        if let Some(handle) = forwarder {
            let _ = self.runtime.block_on(handle);
        }

        let mut result = SyncResult::from(report);
        result
            .errors
            .extend(self.sync_tasks_alongside(account_name.as_deref(), folder_name.as_deref()));
        Ok(result)
    }

    /// Sync the tasks folder as part of a mail sync, and re-arm the reminders.
    ///
    /// Tasks ride along with mail rather than on a timer of their own: the whole
    /// point of the folder-based design is that there is one moment when Orb
    /// talks to the server, and a task list that lags the mail by an unrelated
    /// interval is a task list that shows a stale Today after a manual sync.
    ///
    /// Skipped for a single-folder sync, which is the IDLE path: a mail folder
    /// changed, and the tasks folder is not a mail folder. Also skipped when the
    /// sync names an account other than the designated one.
    ///
    /// Returns whatever went wrong, to be reported alongside the mail sync's own
    /// errors. A task sync that fails never fails the mail sync — the two are
    /// independent, and losing the mail because a task message would not parse
    /// is not a trade anyone would make.
    fn sync_tasks_alongside(
        &self,
        account_name: Option<&str>,
        folder_name: Option<&str>,
    ) -> Vec<String> {
        let config = self.config();
        let Some(tasks) = config.tasks.as_ref() else {
            return Vec::new();
        };
        if folder_name.is_some() || account_name.is_some_and(|name| name != tasks.account) {
            return Vec::new();
        }

        let mut errors = Vec::new();
        match self.runtime.block_on(self.run_task_sync()) {
            Ok(report) => errors.extend(report.errors),
            Err(error) => errors.push(format!("Task sync failed: {error}")),
        }
        // §5.1: reconcile the OS's pending notifications after every task sync.
        // A reminder another machine moved is only a reminder this machine has
        // armed at the wrong time until this runs.
        self.refresh_notifications();
        errors
    }

    /// One full task sync: resolve the folder, reconcile it, and record what
    /// happened.
    async fn run_task_sync(&self) -> Result<TaskSyncReport, OrbError> {
        Ok(self
            .run_task_round(TaskRound::Full)
            .await?
            .unwrap_or_default())
    }

    /// One task sync of the kind asked for, or of a fuller one if that is not
    /// enough. `None` when there was nothing to do and the server was not
    /// asked.
    async fn run_task_round(&self, round: TaskRound) -> Result<Option<TaskSyncReport>, OrbError> {
        let _running = self.task_uploads.running.lock().await;
        let tasks = self.tasks_config()?;
        let account = self
            .config()
            .accounts
            .iter()
            .find(|a| a.name == tasks.account)
            .cloned()
            .ok_or_else(|| OrbError::ConfigError {
                message: format!(
                    "tasks.account names '{}', which is not configured",
                    tasks.account
                ),
            })?;

        let round = match (round, self.task_backlog(&account.name)?.count()) {
            (TaskRound::Push, 0) => return Ok(None),
            (TaskRound::Poll, pending) if pending > 0 => TaskRound::Push,
            (round, _) => round,
        };

        let result = async {
            let mut client = self.tasks_client(&account).await?;
            let report = self
                .task_round_on(&mut client, &account.name, &tasks.folder, round)
                .await;
            self.store
                .connection_pool()
                .give_back(&account.name, client, report.as_ref().err())
                .await;
            report.map_err(OrbError::from)
        }
        .await;

        let problem = result.as_ref().err().map(|error| error.to_string());
        self.health.tasks_problem(&account.name, problem.clone());
        *self
            .task_uploads
            .problem
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = problem;
        result.map(Some)
    }

    /// A task round on a connection of the tasks account.
    async fn task_round_on(
        &self,
        client: &mut crate::imap::ImapClient,
        account_name: &str,
        configured: &str,
        round: TaskRound,
    ) -> crate::Result<TaskSyncReport> {
        let mut service =
            TaskSyncService::open(self.store.db_path(), account_name, configured.to_string())?;

        if round != TaskRound::Full
            && let Some(report) = self.task_round_quick(client, &mut service, round).await?
        {
            return Ok(report);
        }

        let resolved = match service.resolve_folder(client).await {
            Ok(resolved) => resolved,
            Err(error) => {
                // §1: failing to create or select the tasks folder is a loud
                // configuration failure, not something to retry silently — a
                // task list that is quietly not syncing is the worst outcome
                // this feature has.
                crate::logger::warn(&format!(
                    "Config: the tasks folder '{configured}' on account '{account_name}' could \
                     not be prepared: {error}"
                ));
                return Err(error);
            }
        };

        // Whatever a past mail sync stored from this folder is not mail, and no
        // amount of excluding it from future syncs removes what is already
        // there. Cheap when there is nothing: one indexed lookup.
        for name in [configured, resolved.as_str()] {
            match self.store.forget_mail_folder(account_name, name).await {
                Ok(0) => {}
                Ok(removed) => crate::logger::info(&format!(
                    "{account_name}/{name}: removed {removed} message(s) that were synced as \
                     mail before this became the tasks folder"
                )),
                Err(error) => crate::logger::warn(&format!(
                    "{account_name}/{name}: could not clear the stale mail rows: {error}"
                )),
            }
        }

        let report = service.sync(client).await?;
        // Without a record the next round lists the folder again, which is
        // the safe way to be wrong.
        let after = client.folder_state(&resolved).await.ok();
        self.set_task_baseline(after.map(FolderMark::from));
        Ok(report)
    }

    /// A push or a poll, when the folder allows one: `None` when only a full
    /// sync will do (todo-spec §23).
    async fn task_round_quick(
        &self,
        client: &mut crate::imap::ImapClient,
        service: &mut TaskSyncService,
        round: TaskRound,
    ) -> crate::Result<Option<TaskSyncReport>> {
        let Some(folder) = service.resolved_folder()? else {
            return Ok(None);
        };
        let before: FolderMark = match client.folder_state(&folder).await {
            Ok(state) => state.into(),
            Err(error) if error.is_connection_error() => return Err(error),
            Err(_) => return Ok(None),
        };
        // Without UIDNEXT an edit elsewhere, an APPEND and a delete, leaves
        // the folder looking as it was; only a listing can tell.
        let baseline = *self
            .task_uploads
            .baseline
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if before.uid_next.is_none() || baseline != Some(before) {
            return Ok(None);
        }
        if round == TaskRound::Poll {
            return Ok(Some(TaskSyncReport::new()));
        }

        let Some(report) = service.push(client).await? else {
            return Ok(None);
        };
        // The folder is still this machine's own if the APPENDs it made are
        // all that moved UIDNEXT. Otherwise the baseline is left behind, and
        // the next poll lists the folder.
        let after: FolderMark = client.folder_state(&folder).await?.into();
        let appended = (report.uploaded + report.attachments_uploaded) as u32;
        if after.uid_validity == before.uid_validity
            && after.uid_next == before.uid_next.map(|next| next + appended)
        {
            self.set_task_baseline(Some(after));
        }
        Ok(Some(report))
    }

    fn set_task_baseline(&self, mark: Option<FolderMark>) {
        *self
            .task_uploads
            .baseline
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = mark;
    }

    fn task_backlog(&self, account_name: &str) -> Result<crate::tasks::UploadBacklog, OrbError> {
        let db = self.task_db()?;
        TaskRepository::new(&db)
            .upload_backlog(account_name)
            .map_err(OrbError::from)
    }

    /// Fetch one attachment's payload into the local cache.
    async fn download_task_attachment(
        &self,
        task_uuid: String,
        attachment: TaskAttachment,
    ) -> Result<std::path::PathBuf, OrbError> {
        let tasks = self.tasks_config()?;
        let account = self
            .config()
            .accounts
            .iter()
            .find(|a| a.name == tasks.account)
            .cloned()
            .ok_or_else(|| OrbError::ConfigError {
                message: format!(
                    "tasks.account names '{}', which is not configured",
                    tasks.account
                ),
            })?;

        let mut client = self.tasks_client(&account).await?;
        let mut service = TaskSyncService::open(
            self.store.db_path(),
            account.name.clone(),
            tasks.folder.clone(),
        )
        .map_err(OrbError::from)?;

        let path = service
            .download_attachment(&mut client, &task_uuid, &attachment)
            .await;
        self.store
            .connection_pool()
            .give_back(&account.name, client, path.as_ref().err())
            .await;
        path.map_err(OrbError::from)
    }

    /// A connection to the designated tasks account, from the shared pool.
    async fn tasks_client(
        &self,
        account: &crate::config::AccountConfig,
    ) -> Result<crate::imap::ImapClient, OrbError> {
        let password = self.store.credentials().imap(account).await?;
        let pool = self.store.connection_pool();
        pool.register_account(
            &account.name,
            crate::imap::AccountConnectionConfig::for_account(account, password),
        )
        .await;
        pool.acquire(&account.name, "tasks")
            .await
            .map_err(OrbError::from)
    }
}

/// What the `message_links` rules find in `message`.
fn message_links_of(
    rules: &[crate::config::MessageLinkRule],
    message: &crate::models::Message,
) -> Vec<crate::message_links::MessageLink> {
    crate::message_links::links_for(
        rules,
        crate::message_links::LinkSource {
            subject: message.subject.as_deref().unwrap_or_default(),
            from: message.from.as_deref(),
            body: message
                .body_text
                .as_deref()
                .or(message.body_html.as_deref()),
        },
    )
}

/// Name a bulk action for the undo stack, in the singular when it is one.
///
/// "Undone: complete 1 tasks" is the kind of thing that makes software feel
/// unfinished, and this is the string the status line shows after every action.
fn label(verb: &str, count: usize) -> String {
    match count {
        1 => format!("{verb} 1 task"),
        n => format!("{verb} {n} tasks"),
    }
}

/// Whether an undo entry would touch a particular task.
///
/// Used to drop entries about a task that has been erased. Undo already skips
/// ids it cannot find rather than failing, so this is about not offering an
/// action that is guaranteed to do nothing, not about safety.
fn names_task(entry: &UndoEntry, task_id: i64) -> bool {
    entry.steps.iter().any(|step| match step {
        UndoStep::Restore(snapshot) => snapshot.task_id == task_id,
        UndoStep::Trash { task_id: id } => *id == task_id,
    })
}

/// What an undo or a redo did, for the status line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct UndoReport {
    /// The action, as it was recorded: `complete 3 tasks`, `archive “Hello”`.
    pub label: String,
    /// What came of it, where there is more to say than the label: how many
    /// messages came back, and why the rest did not. Often empty.
    pub said: String,
}

/// Undo and redo (README "Undo"): one history for every kind of action.
impl OrbHandle {
    /// Take back the last action, of whatever kind, including a bulk one or a
    /// chain run by one key.
    ///
    /// Task changes are restored as a **new** revision rather than rewound:
    /// republishing a revision the server has already seen would discard
    /// whatever another machine did in the meantime, and an undo that can
    /// destroy an unrelated edit is worse than no undo (todo-spec §10.3).
    ///
    /// Returns what was undone, or `None` when there is nothing to take back.
    pub fn undo(&self) -> Result<Option<UndoReport>, OrbError> {
        let Some(entry) = self.history().pop_undo() else {
            return Ok(None);
        };
        let (back, report) = self.carry_out(&entry).inspect_err(|_| {
            // Nothing was done, so the action can still be taken back later.
            self.history().push_undo(entry.clone());
        })?;
        self.history().push_redo(back);
        Ok(Some(report))
    }

    /// Do again what [`OrbHandle::undo`] took back.
    pub fn redo(&self) -> Result<Option<UndoReport>, OrbError> {
        let Some(entry) = self.history().pop_redo() else {
            return Ok(None);
        };
        let (back, report) = self.carry_out(&entry).inspect_err(|_| {
            self.history().push_redo(entry.clone());
        })?;
        self.history().push_undo(back);
        Ok(Some(report))
    }

    /// Where a chain of actions run by one key starts; pass it to
    /// [`OrbHandle::end_undo_group`] when the chain is over.
    pub fn begin_undo_group(&self) -> u64 {
        self.history().mark()
    }

    /// Make everything recorded since `mark` one action, named `label` or by
    /// its parts.
    pub fn end_undo_group(&self, mark: u64, label: Option<String>) {
        self.history().merge_since(mark, label);
    }

    /// Carry out an entry's steps, last first, and return the entry that
    /// reverses them.
    ///
    /// A step that fails after others have been carried out ends the run: the
    /// steps done stay done and the rest are reported. A failure on the first
    /// step leaves everything as it was, which the caller relies on.
    fn carry_out(
        &self,
        entry: &crate::undo::Entry,
    ) -> Result<(crate::undo::Entry, UndoReport), OrbError> {
        let mut inverses = Vec::with_capacity(entry.steps.len());
        let mut said = Vec::new();
        for (done, step) in entry.steps.iter().rev().enumerate() {
            match self.carry_out_step(step) {
                Ok((inverse, what)) => {
                    inverses.push(inverse);
                    if !what.is_empty() {
                        said.push(what);
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(e) => {
                    said.push(format!("stopped: {e}"));
                    break;
                }
            }
        }
        Ok((
            entry.reversed(inverses),
            UndoReport {
                label: entry.label.clone(),
                said: said.join(", "),
            },
        ))
    }

    /// Carry out one step; returns the step that reverses it, and what to say
    /// about it.
    fn carry_out_step(&self, step: &UndoAction) -> Result<(UndoAction, String), OrbError> {
        Ok(match step {
            UndoAction::Tasks(entry) => {
                let back = self.with_tasks(|db, repo, ctx| {
                    let outcome = repo.undo(entry, ctx)?;
                    self.settle(db, &outcome, ctx)?;
                    Ok(outcome.undo)
                })?;
                self.refresh_notifications();
                (UndoAction::Tasks(back), String::new())
            }
            UndoAction::Read {
                account,
                folder,
                uids,
                read,
            } => {
                let changed = self
                    .runtime
                    .block_on(self.store.queue_read_status(account, folder, uids, *read))
                    .map_err(OrbError::from)?;
                self.drain_outbox_in_background(account);
                let said = format!(
                    "{} marked {}",
                    describe_messages(changed.len()),
                    if *read { "read" } else { "unread" }
                );
                let back = UndoAction::Read {
                    account: account.clone(),
                    folder: folder.clone(),
                    uids: changed,
                    read: !read,
                };
                (back, said)
            }
            UndoAction::Flagged {
                account,
                folder,
                uids,
                flagged,
            } => {
                let changed = self
                    .runtime
                    .block_on(
                        self.store
                            .queue_flag_status(account, folder, uids, *flagged),
                    )
                    .map_err(OrbError::from)?;
                self.drain_outbox_in_background(account);
                let said = format!(
                    "{} {}",
                    describe_messages(changed.len()),
                    if *flagged { "flagged" } else { "unflagged" }
                );
                let back = UndoAction::Flagged {
                    account: account.clone(),
                    folder: folder.clone(),
                    uids: changed,
                    flagged: !flagged,
                };
                (back, said)
            }
            UndoAction::Move {
                account,
                from,
                to,
                message_ids,
            } => {
                let came = self.move_back(account, from, to, message_ids)? as usize;
                let said = if came >= message_ids.len() {
                    format!("{} in {to}", describe_messages(came))
                } else {
                    format!(
                        "{came} of {} in {to}; the rest were moved again since",
                        message_ids.len()
                    )
                };
                let back = UndoAction::Move {
                    account: account.clone(),
                    from: to.clone(),
                    to: from.clone(),
                    message_ids: message_ids.clone(),
                };
                (back, said)
            }
            UndoAction::MoveAcross {
                from,
                to,
                message_ids,
            } => {
                let outcome = self
                    .runtime
                    .block_on(self.store.transfer_by_message_ids(
                        (&from.account, &from.folder),
                        (&to.account, &to.folder),
                        message_ids,
                        false,
                    ))
                    .map_err(OrbError::from)?;
                if outcome.transferred > 0 {
                    self.request_sync(to.account.clone(), Some(to.folder.clone()));
                }
                let said = format!(
                    "{} in {} / {}",
                    describe_messages(outcome.transferred),
                    to.account,
                    to.folder
                );
                let back = UndoAction::MoveAcross {
                    from: to.clone(),
                    to: from.clone(),
                    message_ids: message_ids.clone(),
                };
                (back, said)
            }
            UndoAction::Copy {
                from,
                to,
                message_ids,
            } => {
                let copied = if from.account == to.account {
                    self.runtime
                        .block_on(self.store.copy_by_message_ids(
                            &from.account,
                            &from.folder,
                            &to.folder,
                            message_ids,
                        ))
                        .map_err(OrbError::from)?
                } else {
                    self.runtime
                        .block_on(self.store.transfer_by_message_ids(
                            (&from.account, &from.folder),
                            (&to.account, &to.folder),
                            message_ids,
                            true,
                        ))
                        .map_err(OrbError::from)?
                        .transferred
                };
                if copied > 0 {
                    self.request_sync(to.account.clone(), Some(to.folder.clone()));
                }
                let said = format!("{} copied to {}", describe_messages(copied), to.folder);
                let back = UndoAction::Uncopy {
                    from: from.clone(),
                    to: to.clone(),
                    message_ids: message_ids.clone(),
                };
                (back, said)
            }
            UndoAction::Uncopy {
                from,
                to,
                message_ids,
            } => {
                let deleted = self
                    .runtime
                    .block_on(self.store.delete_copies(
                        (&from.account, &from.folder),
                        &to.account,
                        &to.folder,
                        message_ids,
                    ))
                    .map_err(OrbError::from)?;
                let said = match deleted {
                    1 => format!("1 copy deleted from {}", to.folder),
                    n => format!("{n} copies deleted from {}", to.folder),
                };
                let back = UndoAction::Copy {
                    from: from.clone(),
                    to: to.clone(),
                    message_ids: message_ids.clone(),
                };
                (back, said)
            }
            UndoAction::Staged {
                account,
                folder,
                uids,
                actions,
            } => {
                let current = self.store.pending_for_uids(account, folder, uids)?;
                self.runtime
                    .block_on(self.store.restore_pending(account, folder, uids, actions))
                    .map_err(OrbError::from)?;
                let back = UndoAction::Staged {
                    account: account.clone(),
                    folder: folder.clone(),
                    uids: uids.clone(),
                    actions: current,
                };
                (back, String::new())
            }
            UndoAction::Folder {
                account,
                name,
                exists,
            } => {
                match exists {
                    true => {
                        let decoded = crate::imap::utf7::decode(name);
                        self.runtime
                            .block_on(self.store.create_folder(account, &decoded))?;
                    }
                    false => self
                        .runtime
                        .block_on(self.store.delete_empty_folder(account, name))?,
                }
                self.request_sync(account.clone(), None);
                let back = UndoAction::Folder {
                    account: account.clone(),
                    name: name.clone(),
                    exists: !exists,
                };
                (back, String::new())
            }
            UndoAction::FolderName { account, from, to } => {
                let decoded = crate::imap::utf7::decode(to);
                let wire = self
                    .runtime
                    .block_on(self.store.rename_folder(account, from, &decoded))?;
                let back = UndoAction::FolderName {
                    account: account.clone(),
                    from: wire,
                    to: from.clone(),
                };
                (back, String::new())
            }
            UndoAction::Subscribed {
                account,
                name,
                subscribed,
            } => {
                self.runtime.block_on(self.store.set_folder_subscribed(
                    account,
                    name,
                    *subscribed,
                ))?;
                let back = UndoAction::Subscribed {
                    account: account.clone(),
                    name: name.clone(),
                    subscribed: !subscribed,
                };
                (back, String::new())
            }
            UndoAction::Tag { tag_uuid, tag } => {
                let before =
                    self.with_tasks(|_, repo, _| Ok(repo.restore_tag(tag_uuid, tag.as_ref())?))?;
                let back = UndoAction::Tag {
                    tag_uuid: tag_uuid.clone(),
                    tag: before,
                };
                (back, String::new())
            }
            UndoAction::Reminders { task_id, reminders } => {
                let before = self.with_tasks(|_, repo, ctx| {
                    let before = repo.get_reminders(*task_id)?;
                    repo.replace_reminders(*task_id, reminders, ctx)?;
                    Ok(before)
                })?;
                self.refresh_notifications();
                let back = UndoAction::Reminders {
                    task_id: *task_id,
                    reminders: before,
                };
                (back, String::new())
            }
            UndoAction::Attachments {
                task_id,
                attachments,
            } => {
                let before = self.with_tasks(|_, repo, ctx| {
                    Ok(repo.replace_attachments(*task_id, attachments, ctx)?)
                })?;
                let back = UndoAction::Attachments {
                    task_id: *task_id,
                    attachments: before,
                };
                (back, String::new())
            }
            UndoAction::Link {
                task_id,
                message_id,
                metadata,
            } => {
                let before = self.with_tasks(|_, repo, ctx| {
                    let before = repo
                        .get_linked_emails(*task_id)?
                        .into_iter()
                        .find(|link| link.message_id == *message_id)
                        .map(|link| link_metadata(&link));
                    match metadata {
                        Some(metadata) => {
                            repo.link_email(*task_id, message_id, metadata, ctx)?;
                        }
                        None => {
                            repo.unlink_email(*task_id, message_id, ctx)?;
                        }
                    }
                    Ok(before)
                })?;
                let back = UndoAction::Link {
                    task_id: *task_id,
                    message_id: message_id.clone(),
                    metadata: before,
                };
                (back, String::new())
            }
            UndoAction::WorkingOn { task_ids, on } => {
                self.with_tasks(|_, repo, _| Ok(repo.set_working_on(task_ids, *on)?))?;
                let back = UndoAction::WorkingOn {
                    task_ids: task_ids.clone(),
                    on: !on,
                };
                (back, String::new())
            }
            UndoAction::ChatRead { rid, thread, read } => {
                let chat = self.chat()?;
                self.runtime.block_on(chat.set_read(rid, thread, *read))?;
                let back = UndoAction::ChatRead {
                    rid: rid.clone(),
                    thread: thread.clone(),
                    read: !read,
                };
                (back, String::new())
            }
            UndoAction::ChatReaction { rid, id, emoji, on } => {
                let chat = self.chat()?;
                self.runtime.block_on(chat.react(rid, id, emoji, *on))?;
                let back = UndoAction::ChatReaction {
                    rid: rid.clone(),
                    id: id.clone(),
                    emoji: emoji.clone(),
                    on: !on,
                };
                (back, String::new())
            }
            UndoAction::ChatRoom { rid, kind, member } => {
                let chat = self.chat()?;
                match member {
                    true => self.runtime.block_on(chat.rejoin(rid, kind))?,
                    false => {
                        self.runtime.block_on(chat.leave(rid))?;
                    }
                }
                let back = UndoAction::ChatRoom {
                    rid: rid.clone(),
                    kind: kind.clone(),
                    member: !member,
                };
                (back, String::new())
            }
            UndoAction::ChatMark { rid, thread, mark } => {
                let replaced = self.chat()?.restore_mark(rid, thread, mark.clone())?;
                // The other Macs learn of it through the tasks account.
                self.task_changed();
                let back = UndoAction::ChatMark {
                    rid: rid.clone(),
                    thread: thread.clone(),
                    mark: replaced,
                };
                (back, String::new())
            }
        })
    }
}

/// `1 message`, `3 messages`.
fn describe_messages(count: usize) -> String {
    match count {
        1 => "1 message".to_string(),
        n => format!("{n} messages"),
    }
}

/// What a link remembers of the message it found.
fn found_link_metadata(item: &crate::models::SearchResultItem) -> CachedEmailMetadata {
    CachedEmailMetadata {
        subject: item.message.subject.clone(),
        from: item.message.from.clone(),
        date: item.message.date.clone(),
        account: Some(item.account_name.clone()),
        folder: Some(item.folder_name.clone()),
    }
}

/// What a link remembers of its message, as it was given when linking.
fn link_metadata(link: &TaskLinkedEmail) -> CachedEmailMetadata {
    CachedEmailMetadata {
        subject: link.cached_subject.clone(),
        from: link.cached_from.clone(),
        date: link.cached_date.clone(),
        account: link.cached_account.clone(),
        folder: link.cached_folder.clone(),
    }
}

/// Whether an undo step names a task, and so can only be a no-op once the
/// task is gone.
fn step_names_task(step: &UndoAction, task_id: i64) -> bool {
    match step {
        UndoAction::Tasks(entry) => names_task(entry, task_id),
        UndoAction::Reminders { task_id: id, .. }
        | UndoAction::Attachments { task_id: id, .. }
        | UndoAction::Link { task_id: id, .. } => *id == task_id,
        UndoAction::WorkingOn { task_ids, .. } => task_ids.contains(&task_id),
        _ => false,
    }
}

/// Tasks (§§2–11) and their integration with the mail (§6).
///
/// Every method here goes through the same three layers as the mail ones: a
/// serde-serialisable DTO in, a `TaskRepository` against the local cache, and
/// `OrbError` out. Task edits are local-first by design (§3.6) — none of them
/// touches the network, which is what makes every one of them work offline.
/// The app calls [`OrbHandle::task_changed`] after each, and the uploader
/// takes the change to the server (§23).
impl OrbHandle {
    /// The `tasks` section, or a config error naming what is missing.
    ///
    /// Tasks are unusable without it: there is exactly one designated account
    /// (§1), a task belongs to it, and there is nowhere to sync to otherwise.
    /// Failing here rather than silently keeping tasks in a database nothing
    /// will ever upload is the honest answer.
    fn tasks_config(&self) -> Result<crate::config::TasksConfig, OrbError> {
        self.config()
            .tasks
            .clone()
            .ok_or_else(|| OrbError::ConfigError {
                message: "no tasks section in the configuration: tasks need a \
                      designated account and folder"
                    .to_string(),
            })
    }

    /// A database handle for the task tables.
    ///
    /// Its own connection rather than the store's: the store's write lock exists
    /// to serialise the FTS triggers on `messages`, which no task table has, and
    /// making every task keystroke queue behind whatever sync is running would
    /// undo the reason that lock stopped wrapping reads in the first place.
    /// SQLite's WAL mode and busy timeout handle the rest.
    fn task_db(&self) -> Result<crate::db::Database, OrbError> {
        crate::db::Database::new(&self.config().db_path).map_err(OrbError::from)
    }

    /// Open the task cache and hand it to `work` with a stamped change context.
    ///
    /// The device id comes from the sync state rather than from the caller, so
    /// every log entry this machine appends is attributable without every call
    /// site having to remember to attribute it (§11).
    fn with_tasks<T>(
        &self,
        work: impl FnOnce(
            &crate::db::Database,
            &TaskRepository<'_>,
            &ChangeContext<'_>,
        ) -> Result<T, OrbError>,
    ) -> Result<T, OrbError> {
        let account = self.tasks_config()?.account.clone();
        let db = self.task_db()?;
        let repo = TaskRepository::new(&db);
        let device_id = repo.ensure_device_id(&account)?;
        let ctx = repo.context_at(Utc::now(), Some(device_id.as_str()));
        work(&db, &repo, &ctx)
    }

    /// Run a mutation, then do the three things every mutation owes the rest of
    /// the app: drag the reminders of a task that recurrence advanced, push the
    /// pre-state onto the undo stack, and re-reconcile the OS notifications.
    ///
    /// The last one is not optional. Completing a task with a reminder on it has
    /// to withdraw that reminder from the OS, and nothing else in the system is
    /// watching: a notification for a task finished yesterday still fires, and
    /// "reminders I have already dealt with keep interrupting me" is how people
    /// learn to ignore reminders altogether.
    ///
    /// Returns how many tasks actually changed. An update that moved nothing
    /// returns 0 and records no undo — a no-op is not an action to take back.
    fn mutate(
        &self,
        label: &str,
        work: impl FnOnce(&TaskRepository<'_>, &ChangeContext<'_>) -> Result<MutationOutcome, OrbError>,
    ) -> Result<u32, OrbError> {
        let changed = self.with_tasks(|db, repo, ctx| {
            let outcome = work(repo, ctx)?.labelled(label);
            self.settle(db, &outcome, ctx)?;
            let changed = outcome.changed.len() as u32;
            self.record_tasks(outcome.undo);
            Ok(changed)
        })?;
        self.refresh_notifications();
        Ok(changed)
    }

    /// Move the reminders of any task a completion advanced (§8).
    fn settle(
        &self,
        db: &crate::db::Database,
        outcome: &MutationOutcome,
        ctx: &ChangeContext<'_>,
    ) -> Result<(), OrbError> {
        if outcome.reminder_shifts.is_empty() {
            return Ok(());
        }
        ReminderService::new(db)
            .follow_recurrence(&outcome.reminder_shifts, ctx)
            .map_err(OrbError::from)?;
        Ok(())
    }

    /// The undo history, surviving a panic in an earlier holder.
    ///
    /// A poisoned lock here means some other call panicked mid-mutation; that is
    /// worth neither losing the user's undo history over nor propagating as a
    /// second panic.
    fn history(&self) -> std::sync::MutexGuard<'_, crate::undo::History> {
        self.history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record a task mutation as an action of its own.
    fn record_tasks(&self, entry: UndoEntry) {
        let label = entry.label.clone();
        self.history().record(label, vec![UndoAction::Tasks(entry)]);
    }

    /// The registered notification host, if the platform layer supplied one.
    fn host(&self) -> Option<Arc<dyn TaskNotificationHost>> {
        self.notification_host
            .read()
            .ok()
            .and_then(|host| host.clone())
    }

    /// Bring the OS's pending notifications back in line with the database.
    ///
    /// Best effort by construction: a machine that has not granted notification
    /// permission, or a platform with no notification centre, must still be able
    /// to complete a task.
    fn refresh_notifications(&self) {
        let Some(host) = self.host() else {
            return;
        };
        let outcome = self.task_db().and_then(|db| {
            ReminderService::new(&db)
                .apply(&HostBridge(host.as_ref()), Utc::now())
                .map_err(OrbError::from)
        });
        if let Err(error) = outcome {
            crate::logger::warn(&format!("could not reconcile task reminders: {error}"));
        }
    }

    /// Register where reminder notifications are scheduled (§5.1).
    ///
    /// Call once at launch, before the first sync. Registering also reconciles
    /// immediately, so a machine that was closed while a reminder moved has the
    /// right set armed as soon as it starts rather than at the next sync.
    pub fn set_notification_host(&self, host: Box<dyn TaskNotificationHost>) {
        if let Ok(mut slot) = self.notification_host.write() {
            *slot = Some(Arc::from(host));
        }
        self.refresh_notifications();
    }

    // ------------------------------------------------------------------
    // Views (§4.1) and reading
    // ------------------------------------------------------------------

    /// List one view, optionally filtered by the search box.
    ///
    /// `page` is honoured by the Completed view, which is paginated rather than
    /// capped at an arbitrary hundred (§10.2); the other views are a whole
    /// working list and are returned entire.
    ///
    /// Search is a live filter over title, body and tag names of *this* view,
    /// plus a `tag:<name>` term — deliberately not the message full-text index
    /// (§4.5), because a task list is hundreds of rows and a search that leaves
    /// the current view is a different feature.
    /// Today leaves out what was completed before the last `clear-completed`
    /// on any machine (§10.2).
    pub fn list_tasks(
        &self,
        view: TaskViewName,
        search: Option<String>,
        page: Option<u32>,
    ) -> Result<Vec<TaskSummary>, OrbError> {
        let shown = self
            .config()
            .tasks
            .as_ref()
            .is_none_or(|t| t.keep_completed_in_today);
        let cutoff = self.archive_cutoff();
        self.with_tasks(|_, repo, _| {
            let today = repo.today();
            let since = repo.today_cleared_at()?;
            let view: TaskView = view.into();
            let search = search.unwrap_or_default();
            let mut query = TaskQuery::new(view, today)
                .with_search(&search)
                .with_completed(shown, since)
                .with_archived_before(cutoff);
            if view == TaskView::Completed || view == TaskView::Archive {
                let page = page.unwrap_or(0) as usize;
                query = query.page(COMPLETED_PAGE_SIZE, page * COMPLETED_PAGE_SIZE);
            }
            let registry = repo.list_tags()?;
            Ok(repo
                .list(&query)?
                .into_iter()
                .map(|item| TaskSummary::from_item(item, today, &registry))
                .collect())
        })
    }

    /// Hide the tasks completed so far from Today, on every machine (§10.2).
    /// Writes no task: the cut-off is one message of its own.
    pub fn clear_completed_from_today(&self) -> Result<(), OrbError> {
        self.with_tasks(|_, repo, ctx| Ok(repo.clear_completed_from_today(ctx.now)?))
    }

    /// Every task a search finds, in every view but Trash (§4.5), in the
    /// order §26 groups them in. The query is the mail search's; one that
    /// does not parse finds nothing here, since the mail search reports why.
    pub fn search_tasks(&self, query: String) -> Result<Vec<TaskSummary>, OrbError> {
        let Ok(query) = crate::search::query::parse(&query) else {
            return Ok(Vec::new());
        };
        if query.is_empty() {
            return Ok(Vec::new());
        }
        self.with_tasks(|_, repo, _| {
            let today = repo.today();
            let registry = repo.list_tags()?;
            Ok(repo
                .search(&query)?
                .into_iter()
                .map(|item| TaskSummary::from_item(item, today, &registry))
                .collect())
        })
    }

    /// Write a snapshot of every task to `directory` and keep its newest
    /// `keep` complete snapshots (todo-spec §20).
    ///
    /// The tasks are read in one transaction, so a sync that commits midway
    /// cannot mix revisions; payloads not in the cache are fetched afterwards,
    /// outside it. One export at a time on this machine.
    pub fn export_tasks_to(
        &self,
        directory: &std::path::Path,
        keep: usize,
    ) -> Result<TaskExportResult, OrbError> {
        let tasks_config = self.tasks_config()?;
        let (account, folder) = (tasks_config.account.clone(), tasks_config.folder.clone());
        let mut lock = self.task_db()?;
        if !lock.try_acquire_sync_lock(&account, EXPORT_LOCK)? {
            return Err(OrbError::SyncError {
                message: "another export is running".to_string(),
            });
        }
        let outcome = self.export_locked(directory, keep, &account, &folder);
        if let Err(e) = lock.release_sync_lock(&account, EXPORT_LOCK) {
            crate::logger::warn(&format!("export: could not release the lock: {e}"));
        }
        let outcome = outcome?;
        Ok(TaskExportResult {
            path: outcome.path.to_string_lossy().into_owned(),
            complete: outcome.complete,
            tasks: outcome.tasks as u32,
            attachments: outcome.attachments as u32,
            missing: outcome.missing,
            bytes: outcome.bytes,
            removed: outcome.removed.len() as u32,
        })
    }

    fn export_locked(
        &self,
        directory: &std::path::Path,
        keep: usize,
        account: &str,
        folder: &str,
    ) -> Result<crate::tasks::export::ExportOutcome, OrbError> {
        use crate::tasks::export::{ExportSource, SourceTask};
        let source = self.with_tasks(|db, repo, ctx| {
            db.conn
                .execute_batch("BEGIN DEFERRED")
                .map_err(crate::Error::from)?;
            let read = (|| -> Result<ExportSource, OrbError> {
                let mut tasks = Vec::new();
                for row in repo.task_index(account)? {
                    let Some(task) = repo.get_task(row.id)? else {
                        continue;
                    };
                    tasks.push(SourceTask {
                        reminders: repo.get_reminders(row.id)?,
                        links: repo.get_linked_emails(row.id)?,
                        attachments: repo.get_attachments(row.id)?,
                        task,
                    });
                }
                Ok(ExportSource {
                    device_id: ctx.device_id.map(str::to_string),
                    account: account.to_string(),
                    folder: folder.to_string(),
                    tags: repo.list_tags()?,
                    tasks,
                })
            })();
            let _ = db.conn.execute_batch("COMMIT");
            read
        })?;
        let host = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default();
        let outcome = crate::tasks::export::write_snapshot(
            directory,
            &source,
            Utc::now(),
            &host,
            keep,
            |task, attachment| {
                let cached =
                    cached_attachment_path(&attachment.file_path).map_err(|e| e.to_string())?;
                if cached.is_file() {
                    return Ok(cached);
                }
                self.runtime
                    .block_on(
                        self.download_task_attachment(task.task_uuid.clone(), attachment.clone()),
                    )
                    .map_err(|e| e.to_string())
            },
        )?;
        crate::logger::info(&format!(
            "export: {} tasks to {}{}",
            outcome.tasks,
            outcome.path.display(),
            if outcome.complete {
                String::new()
            } else {
                format!(", incomplete: {}", outcome.missing.join("; "))
            }
        ));
        Ok(outcome)
    }

    /// Export to the configured directory now (todo-spec §20.3), and
    /// remember how it went for the status line.
    pub fn export_tasks(&self) -> Result<TaskExportResult, OrbError> {
        let export = self
            .tasks_config()?
            .export
            .clone()
            .ok_or_else(|| OrbError::ConfigError {
                message: "tasks.export is not set in config.yml".to_string(),
            })?;
        let directory =
            std::path::PathBuf::from(shellexpand::tilde(&export.directory).into_owned());
        let result = self.export_tasks_to(&directory, export.keep);
        *self.last_export.lock().unwrap_or_else(|e| e.into_inner()) = Some(match &result {
            Ok(done) => Ok(done.clone()),
            Err(e) => Err(e.to_string()),
        });
        result
    }

    /// Export when one is due — no complete snapshot younger than the
    /// interval — and say how it went; `None` when none was due.
    pub fn export_tasks_if_due(&self) -> Option<Result<TaskExportResult, OrbError>> {
        self.task_export_status().due.then(|| self.export_tasks())
    }

    /// Where the task export stands (todo-spec §20.3).
    pub fn task_export_status(&self) -> TaskExportStatus {
        let config = self.config();
        let Some(export) = config.tasks.as_ref().and_then(|t| t.export.as_ref()) else {
            return TaskExportStatus {
                configured: false,
                directory: None,
                last_complete: None,
                due: false,
                overdue: false,
                problem: None,
            };
        };
        let directory =
            std::path::PathBuf::from(shellexpand::tilde(&export.directory).into_owned());
        let last = crate::tasks::export::last_complete(&directory)
            .ok()
            .flatten();
        let interval = chrono::Duration::seconds((export.interval_hours.max(0.0) * 3600.0) as i64);
        let now = Utc::now();
        let age = last.map(|at| now - at);
        let problem = match &*self.last_export.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(Err(e)) => Some(e.clone()),
            Some(Ok(done)) if !done.complete => {
                Some(format!("incomplete: {}", done.missing.join("; ")))
            }
            _ => None,
        };
        TaskExportStatus {
            configured: true,
            directory: Some(directory.to_string_lossy().into_owned()),
            last_complete: last.map(crate::tasks::format_timestamp),
            due: age.is_none_or(|age| age >= interval),
            overdue: age.is_none_or(|age| age >= interval * 2),
            problem,
        }
    }

    /// Import a snapshot (todo-spec §21): with `apply` false, say what it
    /// would do and write nothing; with it, do it.
    ///
    /// By task id, never by title: a task unknown here is created at its id,
    /// a known one is replaced only by a higher revision, and the log is merged
    /// either way. Whatever is written goes up at a revision above both copies,
    /// so it wins on every machine. A task purged here stays purged unless
    /// `include_purged` says otherwise. A payload that does not match its
    /// SHA-256 stops its task.
    pub fn import_tasks(
        &self,
        path: String,
        apply: bool,
        include_purged: bool,
    ) -> Result<TaskImportPlan, OrbError> {
        let root = std::path::PathBuf::from(shellexpand::tilde(&path).into_owned());
        let snapshot = crate::tasks::export::read_snapshot(&root)?;
        let account = self.tasks_config()?.account.clone();
        let mut plan = TaskImportPlan {
            snapshot: root.to_string_lossy().into_owned(),
            exported_at: snapshot.exported_at.clone(),
            applied: apply,
            ..TaskImportPlan::default()
        };

        let tags: Vec<TaskTag> = snapshot
            .tags
            .iter()
            .map(|t| TaskTag {
                id: None,
                tag_uuid: t.id.clone(),
                name: t.name.clone(),
                colour: t.colour.clone(),
                position: t.position,
            })
            .collect();
        let known_tags = self.with_tasks(|_, repo, _| Ok(repo.list_tags()?))?;
        plan.tags = tags
            .iter()
            .filter(|t| !known_tags.iter().any(|k| k.tag_uuid == t.tag_uuid))
            .count() as u32;
        if apply {
            self.with_tasks(|_, repo, _| Ok(repo.import_tags(&tags)?))?;
        }

        for exported in &snapshot.tasks {
            let local = self.with_tasks(|_, repo, _| {
                Ok((
                    repo.get_task_by_uuid(&exported.id)?.map(|t| t.revision),
                    repo.is_tombstoned(&exported.id)?,
                ))
            })?;
            let (local_revision, tombstoned) = local;
            // Trashed in the snapshot and gone here: the trash was emptied
            // since, which is a purge whether or not its tombstone is still
            // remembered.
            let purged = tombstoned || (local_revision.is_none() && exported.deleted_at.is_some());
            if purged && !include_purged {
                plan.purged.push(exported.title.clone());
                continue;
            }
            let older = local_revision.is_some_and(|r| r >= exported.revision);
            let imported = crate::tasks::export::imported(&root, exported, &account);
            if !imported.damaged.is_empty() {
                plan.problems.push(format!(
                    "'{}': {} not what the export recorded; the task is left out",
                    exported.title,
                    imported.damaged.join(", ")
                ));
                continue;
            }
            match local_revision {
                None => plan.new += 1,
                Some(_) if older => plan.older += 1,
                Some(_) => plan.newer += 1,
            }
            for (attachment, payload) in &imported.attachments {
                if payload.is_none() {
                    plan.problems.push(format!(
                        "'{}': the file {} is not in the snapshot",
                        exported.title, attachment.filename
                    ));
                }
            }
            if !apply {
                continue;
            }
            if older {
                self.with_tasks(|_, repo, _| {
                    Ok(repo.merge_remote_log(&exported.id, &imported.task.log)?)
                })?;
                continue;
            }
            // Payloads into the cache first: a row claiming a cached payload
            // that is not there is a broken attachment.
            let mut attachments = Vec::new();
            for (attachment, payload) in &imported.attachments {
                let Some(payload) = payload else { continue };
                let target = cached_attachment_path(&attachment.file_path)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| OrbError::DatabaseError {
                        message: format!("could not prepare the attachment cache: {e}"),
                    })?;
                }
                std::fs::copy(payload, &target).map_err(|e| OrbError::DatabaseError {
                    message: format!("could not cache {}: {e}", attachment.filename),
                })?;
                attachments.push(attachment.clone());
            }
            self.with_tasks(|_, repo, _| {
                Ok(repo.import_task(
                    &imported.task,
                    &imported.reminders,
                    &imported.links,
                    &attachments,
                )?)
            })?;
        }
        if apply {
            crate::logger::info(&format!(
                "import: {} new, {} newer, {} merged, {} purged left out, from {}",
                plan.new,
                plan.newer,
                plan.older,
                plan.purged.len(),
                plan.snapshot
            ));
        }
        Ok(plan)
    }

    /// Import Things 3 (todo-spec §22), from its database or from the one at
    /// `path`: with `apply` false, say what it would do and write nothing.
    /// A to-do an earlier import brought over is left alone.
    pub fn import_things(
        &self,
        path: Option<String>,
        apply: bool,
    ) -> Result<ThingsImportPlan, OrbError> {
        let database = match path.filter(|p| !p.trim().is_empty()) {
            Some(path) => std::path::PathBuf::from(shellexpand::tilde(&path).into_owned()),
            None => crate::tasks::things::default_database().ok_or_else(|| OrbError::NotFound {
                message: "no Things 3 database here; name its main.sqlite".to_string(),
            })?,
        };
        let account = self.tasks_config()?.account.clone();
        let today = self.with_tasks(|_, repo, _| Ok(repo.today()))?;
        let data = crate::tasks::things::read(&database, &account, today)?;
        let mut plan = ThingsImportPlan {
            database: database.to_string_lossy().into_owned(),
            trashed: data.trashed as u32,
            projects: data.projects as u32,
            headings: data.headings as u32,
            repeat_instances: data.repeat_instances as u32,
            applied: apply,
            ..ThingsImportPlan::default()
        };
        for mut item in data.tasks {
            let uuid = item.task.task_uuid.clone();
            let (local, purged) = self.with_tasks(|_, repo, _| {
                Ok((repo.get_task_by_uuid(&uuid)?, repo.is_tombstoned(&uuid)?))
            })?;
            // What the last import left, and whether it is still as it was.
            let unedited = match &local {
                None if purged => {
                    plan.already += 1;
                    continue;
                }
                None => None,
                Some(local) => {
                    let last = crate::tasks::things::last_import(local, &item.things_id);
                    if last
                        .as_ref()
                        .is_some_and(|(seen, _)| seen.as_deref() == Some(&item.fingerprint))
                    {
                        plan.already += 1;
                        continue;
                    }
                    if last.is_none_or(|(_, at)| local.revision != at) {
                        plan.edited_here.push(local.subject.clone());
                        if apply {
                            plan.repaired += self.clear_placeholder_dates(local)?;
                        }
                        continue;
                    }
                    Some(local)
                }
            };
            match unedited {
                Some(_) => plan.updated += 1,
                None => {
                    plan.tasks += 1;
                    if item.task.is_completed {
                        plan.completed += 1;
                    }
                    if item.task.recurrence.is_some() {
                        plan.repeating += 1;
                    }
                    if item.unmapped_rule.is_some() {
                        plan.unmapped.push(item.task.subject.clone());
                    }
                }
            }
            let mut ids = Vec::new();
            for name in &item.tag_names {
                let tag = self.with_tasks(|_, repo, _| Ok(repo.find_tag_by_name(name)?))?;
                let tag = match tag {
                    Some(tag) => Some(tag),
                    None => {
                        if !plan.new_tags.contains(name) {
                            plan.new_tags.push(name.clone());
                        }
                        if apply {
                            Some(self.with_tasks(|_, repo, _| Ok(repo.create_tag(name, None)?))?)
                        } else {
                            None
                        }
                    }
                };
                if let Some(tag) = tag {
                    ids.push(tag.tag_uuid);
                }
            }
            if !apply {
                continue;
            }
            item.task.tags = ids;
            let noted: Vec<(String, CachedEmailMetadata)> = item
                .links
                .iter()
                .map(|id| (id.clone(), self.local_link_metadata(id)))
                .collect();
            self.with_tasks(|_, repo, ctx| {
                // An update keeps what only Orb has: the task's place in the
                // list, its linked mail and its files.
                let (mut links, attachments) = match unedited {
                    Some(local) => {
                        item.task.position = local.position;
                        let id = local.id.unwrap_or_default();
                        (repo.get_linked_emails(id)?, repo.get_attachments(id)?)
                    }
                    None => (Vec::new(), Vec::new()),
                };
                for (message_id, metadata) in &noted {
                    if !links.iter().any(|link| &link.message_id == message_id) {
                        links.push(TaskLinkedEmail::new(
                            0,
                            message_id.clone(),
                            metadata.clone(),
                        ));
                    }
                }
                // The revision `import_task` leaves it at.
                let revision = unedited.map_or(0, |l| l.revision).max(item.task.revision) + 1;
                let mark = crate::tasks::things::import_mark(&item, revision, ctx);
                item.task.log.push(mark);
                Ok(repo.import_task(&item.task, &item.reminders, &links, &attachments)?)
            })?;
        }
        // A to-do trashed in Things goes to Orb's trash, unless it was
        // edited here since it came over.
        let mut trash = Vec::new();
        for things_id in &data.trashed_ids {
            let uuid = crate::tasks::things::derived_id("things", things_id);
            let Some(local) = self.with_tasks(|_, repo, _| Ok(repo.get_task_by_uuid(&uuid)?))?
            else {
                continue;
            };
            let unedited = crate::tasks::things::last_import(&local, things_id)
                .is_some_and(|(_, at)| local.revision == at);
            if local.deleted_at.is_none() && unedited {
                trash.push(local.id.unwrap_or_default());
            }
        }
        plan.trashed_here = trash.len() as u32;
        if apply && !trash.is_empty() {
            self.mutate(&label("trash", trash.len()), |repo, ctx| {
                Ok(repo.trash_tasks(&trash, ctx)?)
            })?;
        }
        if apply {
            crate::logger::info(&format!(
                "import: {} tasks from Things, {} updated, {} trashed, {} edited here and left \
                 alone ({} already here), {}",
                plan.tasks,
                plan.updated,
                plan.trashed_here,
                plan.edited_here.len(),
                plan.already,
                plan.database
            ));
        }
        Ok(plan)
    }

    /// Clear the dates Things keeps as placeholders — the year 4000 and
    /// later — from a task an earlier import brought over with them, as an
    /// edit of its own. How many tasks it changed: 0 or 1.
    fn clear_placeholder_dates(&self, task: &Task) -> Result<u32, OrbError> {
        let placeholder = |date: &Option<String>| date.as_deref().is_some_and(|d| d >= "4000");
        let (postpone, due) = (
            placeholder(&task.postpone_date),
            placeholder(&task.due_date),
        );
        if !postpone && !due {
            return Ok(0);
        }
        let mut updates = TaskUpdates::new();
        if postpone {
            updates = updates.with_postpone_date(None);
        }
        if due {
            updates = updates.with_due_date(None);
        }
        let id = task.id.unwrap_or_default();
        self.mutate("clear a placeholder date", |repo, ctx| {
            Ok(repo.apply(id, &updates, ctx)?)
        })
    }

    /// The tasks marked as being worked on on this machine (§19).
    pub fn working_on_tasks(&self) -> Result<Vec<TaskSummary>, OrbError> {
        self.with_tasks(|_, repo, _| {
            let today = repo.today();
            let registry = repo.list_tags()?;
            Ok(repo
                .working_on()?
                .into_iter()
                .map(|item| TaskSummary::from_item(item, today, &registry))
                .collect())
        })
    }

    /// Take the working-on mark off every task (§19); how many had it.
    pub fn clear_working_on(&self) -> Result<u32, OrbError> {
        let ids = self.with_tasks(|_, repo, _| Ok(repo.clear_working_on()?))?;
        let count = ids.len() as u32;
        self.history().record(
            format!("stop working on {}", label("", ids.len()).trim_start()),
            vec![UndoAction::WorkingOn {
                task_ids: ids,
                on: true,
            }],
        );
        Ok(count)
    }

    /// Where Completed ends and the Archive begins, by the config (§18).
    fn archive_cutoff(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let days = self
            .config()
            .tasks
            .as_ref()
            .map_or(30, |t| t.archive_after_days);
        (days > 0).then(|| chrono::Utc::now() - chrono::Duration::days(i64::from(days)))
    }

    /// How many tasks a view holds, ignoring pagination.
    pub fn count_tasks(&self, view: TaskViewName, search: Option<String>) -> Result<u32, OrbError> {
        let cutoff = self.archive_cutoff();
        self.with_tasks(|_, repo, _| {
            let search = search.unwrap_or_default();
            let query = TaskQuery::new(view.into(), repo.today())
                .with_search(&search)
                .with_archived_before(cutoff);
            Ok(repo.count(&query)? as u32)
        })
    }

    /// The number on the Today badge: open tasks only.
    ///
    /// Tasks completed today are in the Today *view* — struck through until
    /// midnight (§10.2) — but a badge counting them would only ever go up as the
    /// user worked, which is the opposite of what a badge is for.
    pub fn today_task_count(&self) -> Result<u32, OrbError> {
        self.with_tasks(|_, repo, _| Ok(repo.today_badge_count(repo.today())? as u32))
    }

    /// When the date-dependent views next have to be re-evaluated (§4.3).
    ///
    /// The next *local* midnight, derived from the timezone rather than from
    /// "now plus 24 hours", because the day a DST change falls on is 23 or 25
    /// hours long. An Orb left open overnight must show the right Today list in
    /// the morning without a restart, and this is what the timer waits on.
    pub fn next_task_midnight(&self) -> Result<String, OrbError> {
        self.with_tasks(|_, repo, _| Ok(repo.next_local_midnight(Utc::now()).to_rfc3339()))
    }

    /// Whether this configuration has a designated tasks account at all (§1).
    ///
    /// Asked once at startup so the interface can leave the task views out
    /// entirely rather than offering five of them that each fail. Tasks are an
    /// opt-in feature of a mail client, and a config without them must produce a
    /// working mail client, not a broken everything.
    pub fn tasks_configured(&self) -> bool {
        self.config().tasks.is_some()
    }

    // ------------------------------------------------------------------
    // The fuzzy date parser (§12.1)
    // ------------------------------------------------------------------

    /// Readings of what the user typed into the date picker, best first.
    ///
    /// "Today" and the locale are settled here rather than in the picker for the
    /// same reason the parser takes them as arguments rather than reading a
    /// clock: a suggestion is only correct relative to a particular day, and the
    /// frontend's idea of the day is a `Date` in whatever zone the WebView
    /// decided on. `repo.today()` is the local civil date the *views* are
    /// evaluated against (§4.3), so what the picker offers and what Today shows
    /// can never disagree.
    ///
    /// An empty vector is "no matching date" and is a real answer — §12.1 is
    /// explicit that unparseable input shows nothing rather than a guess.
    ///
    /// Deliberately answerable without a `tasks` section. Parsing a date needs
    /// a day and a language and nothing else; routing it through the task
    /// account meant that a config with no `tasks` in it made every keystroke
    /// in the picker fail, and "could not read the date" for `tomorrow` sends
    /// somebody off inventing spellings for a phrase that parses perfectly well.
    pub fn suggest_dates(&self, input: String) -> Result<Vec<DateSuggestionInfo>, OrbError> {
        let (today, locale) = self.picker_context();
        Ok(crate::tasks::suggest_dates(&input, today, locale)
            .iter()
            .map(DateSuggestionInfo::from)
            .collect())
    }

    /// The language a task's recurrence is described in.
    fn detail_locale(&self) -> crate::tasks::DateLocale {
        self.config()
            .tasks
            .as_ref()
            .map(|tasks| tasks.date_locale())
            .unwrap_or_default()
    }

    /// The readings of a recurrence typed by a person — "every monday",
    /// "monthly on the 1st", "every 2 weeks until June" — as rules with their
    /// description, for `set-recurrence` (§8).
    pub fn suggest_recurrence(
        &self,
        input: String,
    ) -> Result<Vec<RecurrenceSuggestionInfo>, OrbError> {
        let (today, locale) = self.picker_context();
        Ok(crate::tasks::suggest_recurrence(&input, today, locale)
            .into_iter()
            .map(|suggestion| RecurrenceSuggestionInfo {
                rrule: suggestion.rule.to_string(),
                label: suggestion.label,
            })
            .collect())
    }

    /// Snooze the reminder a notification stands for, as its action asks:
    /// marked fired, and re-armed `minutes` from now. `false` when the
    /// notification names no reminder that is still here.
    pub fn snooze_task_notification(
        &self,
        identifier: String,
        minutes: i64,
    ) -> Result<bool, OrbError> {
        let Some((task_uuid, remind_at_unix)) = crate::tasks::parse_notification_id(&identifier)
        else {
            return Ok(false);
        };
        let account = self.tasks_config()?.account.clone();
        let db = self.task_db()?;
        let repo = TaskRepository::new(&db);
        let device_id = repo.ensure_device_id(&account)?;
        let Some(task_id) = repo.get_task_by_uuid(task_uuid)?.and_then(|task| task.id) else {
            return Ok(false);
        };
        let Some(reminder_id) = repo
            .get_reminders(task_id)?
            .into_iter()
            .find(|reminder| reminder.remind_at_unix == remind_at_unix)
            .and_then(|reminder| reminder.id)
        else {
            return Ok(false);
        };
        ReminderService::new(&db)
            .snooze_reminder(reminder_id, minutes, Utc::now(), Some(&device_id))
            .map_err(OrbError::from)?;
        self.refresh_notifications();
        Ok(true)
    }

    /// The day and the language the picker parses against.
    ///
    /// From the task repository when there is one, so that what the picker
    /// offers and what the Today view shows can never disagree about the date
    /// (§4.3). Without a `tasks` section there is no Today view to disagree
    /// with, so the machine's own zone and English are the answer rather than an
    /// error.
    fn picker_context(&self) -> (chrono::NaiveDate, crate::tasks::DateLocale) {
        let locale = self
            .config()
            .tasks
            .as_ref()
            .map(|tasks| tasks.date_locale())
            .unwrap_or_default();
        let today = match self.task_db() {
            Ok(db) => TaskRepository::new(&db).today(),
            Err(_) => crate::tasks::LocalZone::civil_date(&crate::tasks::SystemZone, Utc::now()),
        };
        (today, locale)
    }

    /// [`OrbHandle::suggest_dates`] for a reminder, which is a day *and* a time
    /// (§5.3).
    ///
    /// Anchored on the current local instant rather than on midnight, so a bare
    /// `9am` typed at noon means tomorrow morning and `in 2 hours` means
    /// something at all.
    pub fn suggest_datetimes(&self, input: String) -> Result<Vec<DateSuggestionInfo>, OrbError> {
        let locale = self
            .config()
            .tasks
            .as_ref()
            .map(|tasks| tasks.date_locale())
            .unwrap_or_default();
        let local_now = match self.task_db() {
            Ok(db) => TaskRepository::new(&db).local_now(Utc::now()),
            Err(_) => crate::tasks::LocalZone::local_now(&crate::tasks::SystemZone, Utc::now()),
        };
        Ok(crate::tasks::suggest_datetimes(&input, local_now, locale)
            .iter()
            .map(DateSuggestionInfo::from)
            .collect())
    }

    /// One task with everything the detail view shows.
    pub fn get_task(&self, task_id: i64) -> Result<TaskDetail, OrbError> {
        self.with_tasks(|_, repo, _| {
            Self::load_detail(repo, task_id, self.detail_locale(), &self.store)
        })
    }

    /// A task's timeline (§11), oldest first.
    pub fn task_log(&self, task_id: i64) -> Result<Vec<TaskLogItem>, OrbError> {
        self.with_tasks(|_, repo, _| {
            Ok(repo
                .get_log(task_id)?
                .iter()
                .map(TaskLogItem::from)
                .collect())
        })
    }

    /// Assemble a task detail, shared by every method that returns one.
    fn load_detail(
        repo: &TaskRepository<'_>,
        task_id: i64,
        locale: crate::tasks::DateLocale,
        store: &crate::store::Store,
    ) -> Result<TaskDetail, OrbError> {
        let task = repo.get_task(task_id)?.ok_or_else(|| OrbError::NotFound {
            message: format!("Task {task_id} not found"),
        })?;
        let registry = repo.list_tags()?;
        let reminders = repo.get_reminders(task_id)?;
        let links = repo.get_linked_emails(task_id)?;
        let attachments = repo.get_attachments(task_id)?;

        Ok(TaskDetail {
            task: TaskSummary::from_task(
                &task,
                repo.today(),
                &registry,
                links.len(),
                reminders.len(),
                attachments.len(),
            ),
            body: task.body_text.clone(),
            recurrence_label: task
                .recurrence
                .as_deref()
                .and_then(|rule| rule.parse::<crate::tasks::RecurrenceRule>().ok())
                .map(|rule| crate::tasks::describe_recurrence(&rule, locale)),
            recurrence: task.recurrence.clone(),
            recurrence_from_completion: task.recurrence_from_completion,
            created_at: task.created_at.clone(),
            updated_at: task.updated_at.clone(),
            revision: task.revision,
            account_name: task.account_name.clone(),
            reminders: reminders
                .iter()
                .map(|reminder| TaskReminderInfo::new(reminder, &task.task_uuid))
                .collect(),
            links: links
                .iter()
                .map(|link| with_reply_status(store, TaskLinkInfo::from(link)))
                .collect(),
            attachments: attachments.iter().map(TaskAttachmentInfo::from).collect(),
            log: task.log.iter().map(TaskLogItem::from).collect(),
            steps: task
                .steps
                .iter()
                .map(|step| TaskStepInfo {
                    id: step.id.clone(),
                    title: step.title.clone(),
                    done: step.done,
                })
                .collect(),
        })
    }

    // ------------------------------------------------------------------
    // Mutations
    // ------------------------------------------------------------------

    /// Create a task at the end of the global order.
    ///
    /// `postpone_date` is the date the task should be scheduled for, which the
    /// caller uses to make a task belong to the view it was created in: a task
    /// made in Today with no dates at all would land in Someday, out of sight
    /// of the list it was just asked for. It is applied inside the same
    /// transaction as the creation, so the task is never briefly undated and
    /// one undo takes the whole thing back.
    pub fn create_task(
        &self,
        title: String,
        postpone_date: Option<String>,
    ) -> Result<TaskDetail, OrbError> {
        let title = normalise_title(&title);
        // Parsed before anything is written: a picker that somehow produced
        // `2026-02-31` must be refused, not leave a dateless task behind.
        let postpone = postpone_date.as_deref().map(parse_task_date).transpose()?;
        let detail = self.with_tasks(|_, repo, ctx| {
            let account = self.tasks_config()?.account.clone();
            let task = repo.create_task(&title, &account, ctx)?;
            let id = task.id.unwrap_or_default();
            // Creation is undoable like everything else, and undoing it trashes
            // the task rather than hard-deleting it: the task may already be on
            // the server, and a soft delete syncs while a hard one leaves the
            // message behind to be downloaded back into existence (§10.1).
            self.record_tasks(UndoEntry::for_creation(&task, "create task"));
            if let Some(date) = postpone {
                repo.apply(id, &TaskUpdates::new().with_postpone_date(Some(date)), ctx)?;
            }
            Self::load_detail(repo, id, self.detail_locale(), &self.store)
        })?;
        Ok(detail)
    }

    /// Edit one task. Returns it as it now stands.
    ///
    /// The single door every field change goes through: it checks the dates
    /// are dates, writes one log entry per field that moved, advances a
    /// recurring task instead of completing it, and bumps the revision only if
    /// something really changed.
    pub fn update_task(&self, task_id: i64, edit: TaskEdit) -> Result<TaskDetail, OrbError> {
        let updates = edit.into_updates()?;
        self.mutate("edit task", |repo, ctx| {
            Ok(repo.apply(task_id, &updates, ctx)?)
        })?;
        self.with_tasks(|_, repo, _| {
            Self::load_detail(repo, task_id, self.detail_locale(), &self.store)
        })
    }

    /// What merging `others` into `keep` would make, before anything is done.
    pub fn plan_task_merge(&self, keep: i64, others: Vec<i64>) -> Result<TaskMergePlan, OrbError> {
        let ids: Vec<i64> = std::iter::once(keep).chain(others).collect();
        self.with_tasks(|_, repo, _| {
            let today = repo.today();
            let registry = repo.list_tags()?;
            let mut tasks = Vec::new();
            for id in &ids {
                tasks.push(repo.get_task(*id)?.ok_or_else(|| OrbError::NotFound {
                    message: format!("Task {id} not found"),
                })?);
            }
            let notes = |task: &crate::tasks::Task| {
                task.body_text
                    .as_deref()
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(str::to_string)
            };
            let mut parts: Vec<String> = notes(&tasks[0]).into_iter().collect();
            for task in &tasks[1..] {
                parts.push(match notes(task) {
                    Some(body) => format!("{}\n{body}", task.subject),
                    None => task.subject.clone(),
                });
            }
            let mut tags: Vec<&String> = tasks.iter().flat_map(|t| &t.tags).collect();
            tags.sort();
            tags.dedup();
            let (mut reminders, mut links, mut attachments) = (0, 0, 0);
            for id in &ids {
                reminders += repo
                    .get_reminders(*id)?
                    .iter()
                    .filter(|r| !r.is_triggered)
                    .count() as u32;
                links += repo.get_linked_emails(*id)?.len() as u32;
                attachments += repo.get_attachments(*id)?.len() as u32;
            }
            let earliest = |pick: fn(&crate::tasks::Task) -> Option<&String>| {
                tasks.iter().filter_map(pick).min().cloned()
            };
            Ok(TaskMergePlan {
                body: (!parts.is_empty()).then(|| parts.join("\n\n")),
                postpone_date: earliest(|t| t.postpone_date.as_ref()),
                due_date: earliest(|t| t.due_date.as_ref()),
                steps: tasks.iter().map(|t| t.steps.len() as u32).sum(),
                tags: tags.len() as u32,
                reminders,
                links,
                attachments,
                tasks: tasks
                    .iter()
                    .map(|task| {
                        let id = task.id.unwrap_or_default();
                        Ok(TaskSummary::from_task(
                            task,
                            today,
                            &registry,
                            repo.get_linked_emails(id)?.len(),
                            repo.get_reminders(id)?.len(),
                            repo.get_attachments(id)?.len(),
                        ))
                    })
                    .collect::<Result<_, OrbError>>()?,
            })
        })
    }

    /// Merge `others` into `keep`: `keep` takes the title, notes and dates
    /// given, and every step, tag, reminder still to come, linked message and
    /// attachment of the others; the others go to the Trash. One undo takes
    /// all of it back.
    pub fn merge_tasks(
        &self,
        keep: i64,
        others: Vec<i64>,
        title: String,
        body: Option<String>,
        postpone_date: Option<String>,
        due_date: Option<String>,
    ) -> Result<TaskDetail, OrbError> {
        let title = normalise_title(&title);
        if title.is_empty() {
            return Err(OrbError::InvalidInput {
                message: "a task needs a title".to_string(),
            });
        }
        let others: Vec<i64> = others.into_iter().filter(|id| *id != keep).collect();
        if others.is_empty() {
            return Err(OrbError::InvalidInput {
                message: "merging needs a second task".to_string(),
            });
        }
        let postpone = postpone_date.as_deref().map(parse_task_date).transpose()?;
        let due = due_date.as_deref().map(parse_task_date).transpose()?;

        // Everything read, and every attachment fetched, before anything is
        // written: a download that fails leaves the tasks as they were.
        let (kept, absorbed, reminders, links, files) = self.with_tasks(|_, repo, _| {
            let get = |id: i64| {
                repo.get_task(id)?.ok_or_else(|| OrbError::NotFound {
                    message: format!("Task {id} not found"),
                })
            };
            let kept = get(keep)?;
            let mut absorbed = Vec::new();
            let (mut reminders, mut links, mut files) = (Vec::new(), Vec::new(), Vec::new());
            for id in &others {
                absorbed.push(get(*id)?);
                reminders.extend(
                    repo.get_reminders(*id)?
                        .into_iter()
                        .filter(|r| !r.is_triggered),
                );
                links.extend(repo.get_linked_emails(*id)?);
                files.extend(repo.get_attachments(*id)?.into_iter().map(|a| (*id, a)));
            }
            Ok((kept, absorbed, reminders, links, files))
        })?;
        let mut paths = Vec::new();
        for (owner, attachment) in &files {
            let path = self.task_attachment_path(*owner, attachment.attachment_uuid.clone())?;
            paths.push((path, attachment.filename.clone()));
        }

        let mark = self.begin_undo_group();
        let merged = self.merge_into(
            keep, &kept, &absorbed, title, body, postpone, due, &reminders, &links, &paths,
        );
        let trashed = merged.and_then(|()| self.trash_tasks(others.clone()));
        self.end_undo_group(mark, Some(format!("merge {} tasks", others.len() + 1)));
        trashed?;
        self.get_task(keep)
    }

    /// The writing half of [`OrbHandle::merge_tasks`], each part recorded for
    /// undo as it is done.
    #[allow(clippy::too_many_arguments)]
    fn merge_into(
        &self,
        keep: i64,
        kept: &crate::tasks::Task,
        absorbed: &[crate::tasks::Task],
        title: String,
        body: Option<String>,
        postpone: Option<chrono::NaiveDate>,
        due: Option<chrono::NaiveDate>,
        reminders: &[crate::tasks::TaskReminder],
        links: &[TaskLinkedEmail],
        files: &[(String, String)],
    ) -> Result<(), OrbError> {
        let mut tags = kept.tags.clone();
        let mut steps = kept.steps.clone();
        for task in absorbed {
            for tag in &task.tags {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
            steps.extend(task.steps.iter().cloned());
        }
        let body = body.map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
        let updates = TaskUpdates::new()
            .with_subject(title)
            .with_body_text(body)
            .with_postpone_date(postpone)
            .with_due_date(due)
            .with_tags(tags)
            .with_steps(steps);
        self.mutate("merge", |repo, ctx| Ok(repo.apply(keep, &updates, ctx)?))?;

        let known: Vec<String> = self
            .with_tasks(|_, repo, _| Ok(repo.get_reminders(keep)?))?
            .into_iter()
            .map(|r| r.remind_at_unix.to_string())
            .collect();
        for reminder in reminders {
            if !known.contains(&reminder.remind_at_unix.to_string()) {
                self.add_task_reminder(keep, reminder.remind_at.clone())?;
            }
        }
        for link in links {
            let before = self.with_tasks(|_, repo, ctx| {
                let before = repo
                    .get_linked_emails(keep)?
                    .into_iter()
                    .find(|l| l.message_id == link.message_id)
                    .map(|l| link_metadata(&l));
                if before.is_none() {
                    repo.link_email(keep, &link.message_id, &link_metadata(link), ctx)?;
                }
                Ok(before)
            })?;
            if before.is_none() {
                self.history().record(
                    "link a message",
                    vec![UndoAction::Link {
                        task_id: keep,
                        message_id: link.message_id.clone(),
                        metadata: None,
                    }],
                );
            }
        }
        for (path, filename) in files {
            self.add_task_attachment_named(keep, path, filename)?;
        }
        Ok(())
    }

    /// Change a task's checklist (§17): add a step at its end, tick or
    /// untick one, rename it, or take it away. Each is an edit of the task.
    pub fn edit_task_step(&self, task_id: i64, edit: StepEdit) -> Result<TaskDetail, OrbError> {
        let task = self
            .with_tasks(|_, repo, _| Ok(repo.get_task(task_id)?))?
            .ok_or_else(|| OrbError::NotFound {
                message: format!("Task {task_id} not found"),
            })?;
        let mut steps = task.steps.clone();
        let find = |steps: &[crate::tasks::TaskStep], id: &str| {
            steps
                .iter()
                .position(|step| step.id == id)
                .ok_or_else(|| OrbError::NotFound {
                    message: format!("the task has no step {id}"),
                })
        };
        match edit {
            StepEdit::Add { title } => {
                let title = title.trim().to_string();
                if title.is_empty() {
                    return Err(OrbError::InvalidInput {
                        message: "a step needs a title".to_string(),
                    });
                }
                steps.push(crate::tasks::TaskStep::new(title));
            }
            StepEdit::Done { ids, done } => {
                for id in &ids {
                    let at = find(&steps, id)?;
                    steps[at].done = done;
                }
            }
            StepEdit::Rename { id, title } => {
                let at = find(&steps, &id)?;
                steps[at].title = title.trim().to_string();
            }
            StepEdit::Remove { ids } => {
                for id in &ids {
                    find(&steps, id)?;
                }
                steps.retain(|step| !ids.contains(&step.id));
            }
            StepEdit::Move { ids, to } => {
                for id in &ids {
                    find(&steps, id)?;
                }
                let (moving, mut rest): (Vec<_>, Vec<_>) =
                    steps.into_iter().partition(|step| ids.contains(&step.id));
                let at = to.min(rest.len());
                rest.splice(at..at, moving);
                steps = rest;
            }
        }
        let updates = TaskUpdates::new().with_steps(steps);
        self.mutate("edit step", |repo, ctx| {
            Ok(repo.apply(task_id, &updates, ctx)?)
        })?;
        self.with_tasks(|_, repo, _| {
            Self::load_detail(repo, task_id, self.detail_locale(), &self.store)
        })
    }

    /// Apply one edit to a whole selection, atomically.
    pub fn update_tasks(&self, task_ids: Vec<i64>, edit: TaskEdit) -> Result<u32, OrbError> {
        let updates = edit.into_updates()?;
        self.mutate(&label("edit", task_ids.len()), |repo, ctx| {
            Ok(repo.apply_to(&task_ids, &updates, ctx)?)
        })
    }

    /// Complete a selection (§10.2). A recurring task advances instead (§8).
    pub fn complete_tasks(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.mutate(&label("complete", task_ids.len()), |repo, ctx| {
            Ok(repo.complete_tasks(&task_ids, ctx)?)
        })
    }

    /// Un-complete a selection, from the Completed view or a just-completed task.
    pub fn uncomplete_tasks(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.mutate(&label("un-complete", task_ids.len()), |repo, ctx| {
            Ok(repo.uncomplete_tasks(&task_ids, ctx)?)
        })
    }

    /// Trash a selection: a soft delete that syncs (§10.1).
    pub fn trash_tasks(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.mutate(&label("delete", task_ids.len()), |repo, ctx| {
            Ok(repo.trash_tasks(&task_ids, ctx)?)
        })
    }

    /// Bring a selection back out of the trash.
    pub fn restore_tasks(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.mutate(&label("restore", task_ids.len()), |repo, ctx| {
            Ok(repo.restore_tasks(&task_ids, ctx)?)
        })
    }

    /// Permanently expunge everything in the trash, now rather than in 30 days.
    ///
    /// Not undoable, and deliberately so: the rows are gone, and the messages
    /// they name are expunged from the server by the next sync. Returns how many
    /// tasks were purged.
    pub fn empty_task_trash(&self) -> Result<u32, OrbError> {
        let mut purged_ids = std::collections::HashSet::new();
        let purged = self.with_tasks(|_, repo, _| {
            let purged = repo.empty_trash()?;
            purged_ids.extend(purged.iter().map(|task| task.id));
            // The rows are gone; their cached payloads are not, and nothing else
            // will ever name them. The messages on the server are handled by the
            // tombstones this purge wrote, on the next sync.
            for task in &purged {
                crate::tasks::remove_cached_attachments(&task.task_uuid);
            }
            Ok(purged.len() as u32)
        })?;
        if purged > 0 {
            // The history may hold task steps naming tasks that no longer
            // exist. Undo skips missing ids rather than failing, but a step that
            // can only be a no-op is not worth offering.
            self.history().retain_steps(|step| match step {
                UndoAction::Tasks(entry) => entry.steps.iter().any(|step| {
                    let id = match step {
                        UndoStep::Restore(snapshot) => snapshot.task_id,
                        UndoStep::Trash { task_id } => task_id.to_owned(),
                    };
                    !purged_ids.contains(&id)
                }),
                _ => !purged_ids.iter().any(|&id| step_names_task(step, id)),
            });
        }
        self.refresh_notifications();
        Ok(purged)
    }

    /// Erase one task outright, without going through the trash.
    ///
    /// For a task that was never really made: `create-task` mints a row so the
    /// detail view has something to edit, and abandoning that edit has to leave
    /// nothing behind. Trashing it would be worse than useless — the point is
    /// that it never existed, and a trash full of blank tasks is exactly the
    /// residue this avoids.
    ///
    /// A sync may already have uploaded it, so this leaves a tombstone like any
    /// other purge and the next sync expunges the message.
    pub fn discard_task(&self, task_id: i64) -> Result<bool, OrbError> {
        let discarded = self.with_tasks(|_, repo, _| {
            let purged = repo.discard_task(task_id)?;
            if let Some(task) = &purged {
                crate::tasks::remove_cached_attachments(&task.task_uuid);
            }
            Ok(purged.is_some())
        })?;
        if discarded {
            // An undo entry naming a task that no longer exists can only be a
            // no-op, and offering one is worse than offering nothing.
            self.history()
                .retain_steps(|step| !step_names_task(step, task_id));
            self.refresh_notifications();
        }
        Ok(discarded)
    }

    /// Set the postpone date of a selection (§12).
    ///
    /// `None` clears it. A postpone date later than the task's due date is
    /// rejected — §2 says so, and the caller shows the message in the status
    /// line rather than silently choosing one of the two dates.
    pub fn set_task_postpone_date(
        &self,
        task_ids: Vec<i64>,
        date: Option<String>,
    ) -> Result<u32, OrbError> {
        let date = date.as_deref().map(parse_task_date).transpose()?;
        self.mutate(&label("postpone", task_ids.len()), |repo, ctx| {
            Ok(repo.set_postpone_date(&task_ids, date, ctx)?)
        })
    }

    /// Set the due date of a selection (§12).
    ///
    /// `None` clears it. The postpone date stays as it is, even after it.
    pub fn set_task_due_date(
        &self,
        task_ids: Vec<i64>,
        date: Option<String>,
    ) -> Result<u32, OrbError> {
        let date = date.as_deref().map(parse_task_date).transpose()?;
        self.mutate(&label("set due date on", task_ids.len()), |repo, ctx| {
            Ok(repo.set_due_date(&task_ids, date, ctx)?)
        })
    }

    /// Clear both dates of a selection, sending it back to Someday.
    pub fn clear_task_dates(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.mutate(&label("clear the dates of", task_ids.len()), |repo, ctx| {
            Ok(repo.clear_dates(&task_ids, ctx)?)
        })
    }

    /// Move a task in the single global order (§4.4).
    ///
    /// `before` is the task it lands after and `after` the task it lands before;
    /// `None` for either is the corresponding end of the list. Writes one row,
    /// except on the rare pass where f64 precision between two neighbours has
    /// run out and the whole list is renormalised first.
    pub fn move_task(
        &self,
        task_id: i64,
        before: Option<i64>,
        after: Option<i64>,
    ) -> Result<u32, OrbError> {
        self.mutate("reorder", |repo, ctx| {
            Ok(repo.move_task(task_id, before, after, ctx)?)
        })
    }

    /// Move a selection, in the order given, as one block between two
    /// neighbours (§4.4).
    pub fn move_tasks(
        &self,
        task_ids: Vec<i64>,
        before: Option<i64>,
        after: Option<i64>,
    ) -> Result<u32, OrbError> {
        self.mutate(&label("reorder", task_ids.len()), |repo, ctx| {
            Ok(repo.move_tasks(&task_ids, before, after, ctx)?)
        })
    }

    // ------------------------------------------------------------------
    // Tags (§7)
    // ------------------------------------------------------------------

    /// The tag registry, in its manual order.
    pub fn list_task_tags(&self) -> Result<Vec<TaskTagInfo>, OrbError> {
        self.with_tasks(|_, repo, _| Ok(repo.list_tags()?.iter().map(TaskTagInfo::from).collect()))
    }

    /// Create a tag. Returns it, including the uuid tasks will carry.
    pub fn create_task_tag(
        &self,
        name: String,
        colour: Option<String>,
    ) -> Result<TaskTagInfo, OrbError> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(OrbError::InvalidInput {
                message: "a tag needs a name".to_string(),
            });
        }
        let tag = self.with_tasks(|_, repo, _| Ok(repo.create_tag(&name, colour.as_deref())?))?;
        self.history().record(
            format!("create tag {name}"),
            vec![UndoAction::Tag {
                tag_uuid: tag.tag_uuid.clone(),
                tag: None,
            }],
        );
        Ok(TaskTagInfo::from(&tag))
    }

    /// Rename or recolour a tag.
    ///
    /// Touches no task: tasks carry tag *ids*, which is the whole reason a
    /// rename rewrites one message rather than every task that uses it (§7).
    pub fn update_task_tag(
        &self,
        tag_id: String,
        name: String,
        colour: Option<String>,
    ) -> Result<bool, OrbError> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(OrbError::InvalidInput {
                message: "a tag needs a name".to_string(),
            });
        }
        let (before, updated) = self.with_tasks(|_, repo, _| {
            let before = repo.get_tag(&tag_id)?;
            Ok((before, repo.update_tag(&tag_id, &name, colour.as_deref())?))
        })?;
        if let Some(before) = before.filter(|_| updated) {
            let what = match before.name == name {
                true => format!("recolour tag {name}"),
                false => format!("rename tag {} to {name}", before.name),
            };
            self.history().record(
                what,
                vec![UndoAction::Tag {
                    tag_uuid: tag_id,
                    tag: Some(before),
                }],
            );
        }
        Ok(updated)
    }

    /// Delete a tag, detaching it from the tasks that carried it.
    ///
    /// Never deletes the tasks (§7). The tasks that referenced it are marked
    /// dirty so their headers are rewritten without it. Returns how many tasks
    /// were affected.
    ///
    /// Undoing it puts the tag back on the tasks it was taken off.
    pub fn delete_task_tag(&self, tag_id: String) -> Result<u32, OrbError> {
        let (before, outcome) = self.with_tasks(|_, repo, ctx| {
            Ok((repo.get_tag(&tag_id)?, repo.delete_tag(&tag_id, ctx)?))
        })?;
        if let Some(before) = before {
            self.history().record(
                format!("delete tag {}", before.name),
                vec![
                    UndoAction::Tasks(outcome.undo),
                    UndoAction::Tag {
                        tag_uuid: tag_id,
                        tag: Some(before),
                    },
                ],
            );
        }
        Ok(outcome.changed.len() as u32)
    }

    /// Add a tag to a selection.
    pub fn add_task_tag(&self, task_ids: Vec<i64>, tag_id: String) -> Result<u32, OrbError> {
        self.mutate(&label("tag", task_ids.len()), |repo, ctx| {
            Ok(repo.add_tag_to(&task_ids, &tag_id, ctx)?)
        })
    }

    /// Remove a tag from a selection.
    pub fn remove_task_tag(&self, task_ids: Vec<i64>, tag_id: String) -> Result<u32, OrbError> {
        self.mutate(&label("untag", task_ids.len()), |repo, ctx| {
            Ok(repo.remove_tag_from(&task_ids, &tag_id, ctx)?)
        })
    }

    /// Replace a task's whole tag set.
    pub fn set_task_tags(&self, task_id: i64, tag_ids: Vec<String>) -> Result<u32, OrbError> {
        self.mutate("retag", |repo, ctx| {
            Ok(repo.set_tags(task_id, tag_ids.clone(), ctx)?)
        })
    }

    // ------------------------------------------------------------------
    // Reminders (§5)
    // ------------------------------------------------------------------

    /// A task's reminders.
    pub fn list_task_reminders(&self, task_id: i64) -> Result<Vec<TaskReminderInfo>, OrbError> {
        self.with_tasks(|_, repo, _| {
            let task = repo.get_task(task_id)?.ok_or_else(|| OrbError::NotFound {
                message: format!("Task {task_id} not found"),
            })?;
            Ok(repo
                .get_reminders(task_id)?
                .iter()
                .map(|reminder| TaskReminderInfo::new(reminder, &task.task_uuid))
                .collect())
        })
    }

    /// Add a reminder at a local date and time (RFC3339).
    ///
    /// Scheduled locally regardless of sync state (§3.6): a task created offline
    /// is fully usable, reminders included.
    pub fn add_task_reminder(&self, task_id: i64, remind_at: String) -> Result<i64, OrbError> {
        let (before, id) = self.with_tasks(|_, repo, ctx| {
            let before = repo.get_reminders(task_id)?;
            Ok((before, repo.add_reminder(task_id, &remind_at, ctx)?))
        })?;
        self.history().record(
            "add a reminder",
            vec![UndoAction::Reminders {
                task_id,
                reminders: before,
            }],
        );
        self.refresh_notifications();
        Ok(id)
    }

    /// Remove a reminder.
    pub fn delete_task_reminder(&self, reminder_id: i64) -> Result<(), OrbError> {
        let before = self.with_tasks(|_, repo, ctx| {
            let task_id: Option<i64> = repo.reminder_task(reminder_id)?;
            let before = match task_id {
                Some(task_id) => Some((task_id, repo.get_reminders(task_id)?)),
                None => None,
            };
            repo.delete_reminder(reminder_id, ctx)?;
            Ok(before)
        })?;
        if let Some((task_id, reminders)) = before {
            self.history().record(
                "remove a reminder",
                vec![UndoAction::Reminders { task_id, reminders }],
            );
        }
        self.refresh_notifications();
        Ok(())
    }

    /// Push a reminder back by a number of minutes. Returns its new instant.
    pub fn snooze_task_reminder(&self, reminder_id: i64, minutes: i64) -> Result<i64, OrbError> {
        let account = self.tasks_config()?.account.clone();
        let db = self.task_db()?;
        let device_id = TaskRepository::new(&db).ensure_device_id(&account)?;
        let at = ReminderService::new(&db)
            .snooze_reminder(reminder_id, minutes, Utc::now(), Some(&device_id))
            .map_err(OrbError::from)?;
        self.refresh_notifications();
        Ok(at)
    }

    /// What the OS should be holding pending, given what it says it holds.
    ///
    /// For a platform layer that would rather pull a plan than register a host.
    /// Apply it in the order given: cancel, then schedule.
    pub fn task_notification_plan(
        &self,
        pending: Vec<String>,
    ) -> Result<TaskNotificationPlan, OrbError> {
        let db = self.task_db()?;
        let plan = ReminderService::new(&db)
            .reconcile_at(&pending, Utc::now())
            .map_err(OrbError::from)?;
        Ok(TaskNotificationPlan::from(plan))
    }

    /// Report that the OS delivered a notification.
    ///
    /// The only thing standing between the launch sweep and a duplicate of every
    /// notification the OS already showed while Orb was closed (§5.2), so the
    /// platform layer has to call it — from the delegate while running, and from
    /// the delivered list on launch.
    pub fn record_task_notification_delivered(&self, identifier: String) -> Result<bool, OrbError> {
        let account = self.tasks_config()?.account.clone();
        let db = self.task_db()?;
        let device_id = TaskRepository::new(&db).ensure_device_id(&account)?;
        ReminderService::new(&db)
            .record_delivered(&identifier, Utc::now(), Some(&device_id))
            .map_err(OrbError::from)
    }

    /// The launch sweep (§5.2).
    ///
    /// Reminders less than 24 hours overdue are returned to be delivered; older
    /// ones are marked fired silently and collapsed into one summary and one log
    /// entry per task. A year of missed reminders must never arrive as a year of
    /// notifications at once.
    ///
    /// Everything overdue is marked fired *before* this returns, so a crash and
    /// a relaunch cannot deliver any of it twice.
    pub fn catch_up_task_reminders(&self) -> Result<TaskCatchUp, OrbError> {
        let account = self.tasks_config()?.account.clone();
        let db = self.task_db()?;
        let device_id = TaskRepository::new(&db).ensure_device_id(&account)?;
        let result = ReminderService::new(&db)
            .catch_up_at(Utc::now(), Some(&device_id))
            .map_err(OrbError::from)?;

        Ok(TaskCatchUp {
            due: result
                .due_reminders
                .iter()
                .map(|due| DueTaskReminder {
                    task_id: due.task_id,
                    task_uuid: due.task_uuid.clone(),
                    title: due.task_subject.clone(),
                    remind_at: due.reminder.remind_at.clone(),
                    notification_id: due.notification_id(),
                })
                .collect(),
            missed_count: result.missed.as_ref().map(|m| m.count as u32).unwrap_or(0),
            missed_summary: result.missed.as_ref().map(|m| m.body()),
        })
    }

    // ------------------------------------------------------------------
    // Attachments (§9)
    // ------------------------------------------------------------------

    /// A task's attachments.
    pub fn list_task_attachments(&self, task_id: i64) -> Result<Vec<TaskAttachmentInfo>, OrbError> {
        self.with_tasks(|_, repo, _| {
            Ok(repo
                .get_attachments(task_id)?
                .iter()
                .map(TaskAttachmentInfo::from)
                .collect())
        })
    }

    /// Attach a file to a task.
    ///
    /// The payload is copied into the local cache immediately and uploaded as a
    /// message of its own on the next sync — never inside the task message,
    /// because every task edit rewrites that message and renaming a task must
    /// not re-upload a 10 MB file (§9).
    pub fn add_task_attachment(
        &self,
        task_id: i64,
        path: String,
    ) -> Result<TaskAttachmentInfo, OrbError> {
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment")
            .to_string();
        self.add_task_attachment_named(task_id, &path, &filename)
    }

    /// [`OrbHandle::add_task_attachment`] under a name of its own: a cached
    /// payload's file is named by its id, not as it was attached.
    fn add_task_attachment_named(
        &self,
        task_id: i64,
        path: &str,
        filename: &str,
    ) -> Result<TaskAttachmentInfo, OrbError> {
        let bytes = std::fs::read(path).map_err(|e| OrbError::InvalidInput {
            message: format!("could not read '{path}': {e}"),
        })?;
        let filename = filename.to_string();
        let content_type = crate::attachments::guess_content_type(&filename);

        let info = self.with_tasks(|_, repo, ctx| {
            let task = repo.get_task(task_id)?.ok_or_else(|| OrbError::NotFound {
                message: format!("Task {task_id} not found"),
            })?;
            let mut attachment = TaskAttachment::create(
                task_id,
                &task.task_uuid,
                filename.clone(),
                Some(content_type.clone()),
                Some(bytes.len() as i64),
            );

            // Written to the cache before the row is inserted: a row claiming a
            // cached payload that is not there is a broken attachment, whereas a
            // file with no row is a stray byte-blob the cache sweep removes.
            let target = cached_attachment_path(&attachment.file_path)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| OrbError::DatabaseError {
                    message: format!("could not prepare the attachment cache: {e}"),
                })?;
            }
            std::fs::write(&target, &bytes).map_err(|e| OrbError::DatabaseError {
                message: format!("could not cache the attachment: {e}"),
            })?;
            attachment.is_cached = true;

            let before = repo.get_attachments(task_id)?;
            let id = repo.add_attachment(&attachment, ctx)?;
            attachment.id = Some(id);
            self.history().record(
                format!("attach {filename}"),
                vec![UndoAction::Attachments {
                    task_id,
                    attachments: before,
                }],
            );
            Ok(TaskAttachmentInfo::from(&attachment))
        })?;
        Ok(info)
    }

    /// Remove an attachment from a task. The payload stays where it is, so
    /// undoing this finds it again.
    pub fn delete_task_attachment(&self, attachment_id: i64) -> Result<(), OrbError> {
        self.with_tasks(|_, repo, ctx| {
            let before = repo.attachment_task(attachment_id)?;
            repo.delete_attachment(attachment_id, ctx)?;
            if let Some((task_id, attachments)) = before {
                let name = attachments
                    .iter()
                    .find(|a| a.id == Some(attachment_id))
                    .map_or("an attachment".to_string(), |a| a.filename.clone());
                self.history().record(
                    format!("remove {name}"),
                    vec![UndoAction::Attachments {
                        task_id,
                        attachments,
                    }],
                );
            }
            Ok(())
        })
    }

    /// The bytes of an attachment, downloading them if this machine has never
    /// seen them.
    ///
    /// The cache is disposable and rebuildable (§9): a missing file is fetched
    /// from the attachment's own message in the tasks folder rather than being
    /// an error. That fetch is a network round trip, so this belongs on a
    /// background thread.
    pub fn task_attachment_data(
        &self,
        task_id: i64,
        attachment_uuid: String,
    ) -> Result<AttachmentData, OrbError> {
        let (task_uuid, attachment) = self.with_tasks(|_, repo, _| {
            let task = repo.get_task(task_id)?.ok_or_else(|| OrbError::NotFound {
                message: format!("Task {task_id} not found"),
            })?;
            let attachment = repo
                .get_attachments(task_id)?
                .into_iter()
                .find(|a| a.attachment_uuid == attachment_uuid)
                .ok_or_else(|| OrbError::NotFound {
                    message: format!("Attachment {attachment_uuid} not found"),
                })?;
            Ok((task.task_uuid, attachment))
        })?;

        let cached = cached_attachment_path(&attachment.file_path)?;
        let bytes = match std::fs::read(&cached) {
            Ok(bytes) => bytes,
            Err(_) => {
                let path = self.runtime.block_on(
                    self.download_task_attachment(task_uuid.clone(), attachment.clone()),
                )?;
                std::fs::read(&path).map_err(|e| OrbError::NotFound {
                    message: format!(
                        "attachment '{}' could not be read: {e}",
                        attachment.filename
                    ),
                })?
            }
        };

        Ok(AttachmentData {
            filename: attachment.filename.clone(),
            content_type: attachment
                .content_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            data: bytes,
        })
    }

    /// The path of an attachment on this disk, downloading it if need be.
    ///
    /// What "save" means for a task attachment. The payload already has a home —
    /// the cache under `~/.orb/attachments/tasks/<task>/` — so materialising it
    /// there and handing back the path is both the download and the save, and
    /// the caller can then open it, reveal it, or drag it somewhere permanent.
    /// Returning a path rather than bytes also keeps a 40 MB attachment out of
    /// the IPC channel and out of the webview's heap.
    pub fn task_attachment_path(
        &self,
        task_id: i64,
        attachment_uuid: String,
    ) -> Result<String, OrbError> {
        let (task_uuid, attachment) = self.with_tasks(|_, repo, _| {
            let task = repo.get_task(task_id)?.ok_or_else(|| OrbError::NotFound {
                message: format!("Task {task_id} not found"),
            })?;
            let attachment = repo
                .get_attachments(task_id)?
                .into_iter()
                .find(|a| a.attachment_uuid == attachment_uuid)
                .ok_or_else(|| OrbError::NotFound {
                    message: format!("Attachment {attachment_uuid} not found"),
                })?;
            Ok((task.task_uuid, attachment))
        })?;

        let cached = cached_attachment_path(&attachment.file_path)?;
        if cached.is_file() {
            return Ok(cached.to_string_lossy().into_owned());
        }
        let path = self
            .runtime
            .block_on(self.download_task_attachment(task_uuid, attachment))?;
        Ok(path.to_string_lossy().into_owned())
    }

    // ------------------------------------------------------------------
    // Sync (§3)
    // ------------------------------------------------------------------

    /// Sync the tasks folder now.
    ///
    /// Also runs automatically after a whole-mailbox mail sync; this is the
    /// explicit "sync tasks" the UI can bind to a key.
    pub fn sync_tasks(&self) -> Result<TaskSyncResult, OrbError> {
        let report = self.runtime.block_on(self.run_task_sync())?;
        self.refresh_notifications();
        Ok(TaskSyncResult::from(report))
    }

    /// Whether the window can say the tasks are saved (todo-spec §23).
    pub fn task_sync_status(&self) -> Result<TaskSyncStatus, OrbError> {
        let tasks = self.tasks_config()?;
        Ok(TaskSyncStatus {
            pending: self.task_backlog(&tasks.account)?.count(),
            problem: self
                .task_uploads
                .problem
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        })
    }

    /// A task was changed here: upload it once the changes stop.
    ///
    /// Nothing happens before [`OrbHandle::start_task_uploads`]; the change
    /// waits for the next sync.
    pub fn task_changed(&self) {
        if let Some(nudge) = self
            .task_uploads
            .nudge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = nudge.send(());
        }
    }

    /// The code that sets Orb up on an iPhone (IOS_PLAN.md 2.1), drawn for
    /// the window: the tasks account, its folder and settings, and the
    /// password, obtained as any login obtains it. Good for five minutes.
    #[cfg(feature = "desktop")]
    pub fn iphone_setup_code(&self) -> Result<IphoneSetupCode, OrbError> {
        let payload = self.iphone_setup_payload(Utc::now().timestamp())?;
        Ok(IphoneSetupCode {
            svg: crate::setup_code::qr_svg(&payload.encode()),
            expires: payload.expires,
            login: format!("{} on {}", payload.imap_username, payload.imap_server),
            folder: payload.folder,
        })
    }

    #[cfg(feature = "desktop")]
    fn iphone_setup_payload(&self, now: i64) -> Result<crate::setup_code::SetupPayload, OrbError> {
        use secrecy::ExposeSecret;
        let config = self.config();
        let tasks = self.tasks_config()?;
        let account = config
            .accounts
            .iter()
            .find(|a| a.name == tasks.account)
            .ok_or_else(|| OrbError::ConfigError {
                message: format!("tasks.account names no account: {}", tasks.account),
            })?;
        let password = self
            .runtime
            .block_on(self.store.credentials().imap(account))
            .map_err(OrbError::from)?;
        Ok(crate::setup_code::SetupPayload::new(
            account.imap_server.clone(),
            account.imap_port,
            account.imap_username.clone(),
            password.expose_secret().to_string(),
            tasks.folder.clone(),
            tasks.locale.clone(),
            tasks.keep_completed_in_today,
            tasks.completed_first_in_today,
            tasks.archive_after_days,
            now,
        ))
    }

    /// Stop what [`OrbHandle::start_task_uploads`] started, which may then be
    /// started again — unlike [`OrbHandle::shutdown`], which is for good. A
    /// change still waiting for its upload goes up with the next sync.
    pub fn stop_task_uploads(&self) {
        self.task_uploads
            .nudge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    /// Upload task changes as they are made, and take in other machines'
    /// (todo-spec §23).
    ///
    /// A burst of [`OrbHandle::task_changed`] is one upload, a second after
    /// the last of it and at most five after the first. Every minute the
    /// tasks folder is asked whether anyone else changed it — a listing only
    /// if so — and what could not be uploaded is tried again, which is how a
    /// change made offline goes up on reconnect (§3.6).
    ///
    /// Runs on a thread of its own, as the app's commands do, and holds the
    /// handle only while a round runs. Ends with [`OrbHandle::shutdown`].
    pub fn start_task_uploads(self: &Arc<Self>, listener: Box<dyn TaskSyncListener>) {
        use crate::constants::{
            TASK_POLL_INTERVAL_SECONDS, TASK_UPLOAD_DEBOUNCE_MAX_MS, TASK_UPLOAD_DEBOUNCE_MS,
        };
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::{Duration, Instant};

        let mut slot = self
            .task_uploads
            .nudge
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.is_some() || self.config().tasks.is_none() {
            return;
        }
        let (nudge, nudges) = std::sync::mpsc::channel();
        *slot = Some(nudge);
        drop(slot);

        let handle = Arc::downgrade(self);
        let debounce = Duration::from_millis(TASK_UPLOAD_DEBOUNCE_MS);
        let longest = Duration::from_millis(TASK_UPLOAD_DEBOUNCE_MAX_MS);
        let poll = Duration::from_secs(TASK_POLL_INTERVAL_SECONDS);
        let spawned = std::thread::Builder::new()
            .name("orb-task-uploads".into())
            .spawn(move || {
                // When the burst under way began, and when it is due.
                let mut burst: Option<(Instant, Instant)> = None;
                let mut next_poll = Instant::now() + poll;
                loop {
                    let wake = burst.map_or(next_poll, |(_, due)| due.min(next_poll));
                    match nudges.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                        Ok(()) => {
                            let now = Instant::now();
                            let began = burst.map_or(now, |(began, _)| began);
                            burst = Some((began, (now + debounce).min(began + longest)));
                            continue;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    let now = Instant::now();
                    let round = match burst {
                        Some((_, due)) if due <= now => TaskRound::Push,
                        _ if next_poll <= now => TaskRound::Poll,
                        _ => continue,
                    };
                    burst = None;
                    next_poll = now + poll;

                    let Some(orb) = handle.upgrade() else { return };
                    if orb.push_shutdown.is_cancelled() {
                        return;
                    }
                    orb.task_round_for(round, listener.as_ref());
                }
            });
        if let Err(error) = spawned {
            crate::logger::warn(&format!("task uploads: the thread did not start: {error}"));
        }
    }

    /// Run one round for the uploader, and tell the listener what came of it.
    fn task_round_for(&self, round: TaskRound, listener: &dyn TaskSyncListener) {
        let before = self.task_sync_status().ok();
        let changed = match self.runtime.block_on(self.run_task_round(round)) {
            Ok(None) => return,
            Ok(Some(report)) => {
                for error in &report.errors {
                    crate::logger::warn(&format!("task sync: {error}"));
                }
                if report.downloaded + report.deleted > 0 {
                    self.refresh_notifications();
                }
                report.downloaded + report.uploaded + report.deleted + report.rebound > 0
            }
            Err(error) => {
                crate::logger::warn(&format!("task sync ({round:?}): {error}"));
                false
            }
        };
        let after = self.task_sync_status().ok();
        let problem_changed =
            before.as_ref().map(|s| &s.problem) != after.as_ref().map(|s| &s.problem);
        if changed || before != after {
            listener.on_task_sync(TaskSyncEvent {
                tasks_changed: changed,
                problem_changed,
            });
        }
    }
}

/// Email integration (§6) — the reason tasks live in a mail client.
impl OrbHandle {
    /// Create one task per message in a selection, each linked to its message
    /// (§6.1).
    ///
    /// The title is pre-filled from the subject *after* munging, because the
    /// munged subject is the one the user has been reading in the list — a task
    /// titled with the raw `[EXT] AW: Re: Fwd: …` form would be a title nobody
    /// recognises. It is a starting point, not a decision: the caller opens the
    /// new task for editing straight away.
    ///
    /// The link stores the RFC `Message-ID` plus the cached subject, sender,
    /// date, account and last-known folder. The cache is not a convenience: it
    /// is what keeps the task readable years later, when the message has been
    /// moved, deleted, or lost with its account (§6.2 step 3).
    ///
    /// One query for the whole selection, and a message with no `Message-ID`
    /// still gets its task — an unlinkable task is better than a refusal.
    ///
    /// `postpone`, a `YYYY-MM-DD` date, puts the new tasks in the view for
    /// that day — today's for Today — and `due` gives them a due date, both in
    /// the same change, so one undo takes the whole thing back.
    pub fn create_tasks_from_messages(
        &self,
        message_ids: Vec<i64>,
        postpone: Option<String>,
        due: Option<String>,
    ) -> Result<Vec<TaskDetail>, OrbError> {
        let postpone = postpone.as_deref().map(parse_task_date).transpose()?;
        let due = due.as_deref().map(parse_task_date).transpose()?;
        let sources = self
            .store
            .message_link_sources(&message_ids)
            .map_err(OrbError::from)?;
        if sources.is_empty() {
            return Ok(Vec::new());
        }

        let munger =
            crate::subject_munger::SubjectMunger::from_config(&self.config().subject_munges);
        let account = self.tasks_config()?.account.clone();

        let details = self.with_tasks(|_, repo, ctx| {
            let mut created = Vec::with_capacity(sources.len());
            let mut undo = UndoEntry {
                label: format!("{} from mail", label("create", sources.len())),
                steps: Vec::new(),
            };

            for source in &sources {
                let title = normalise_title(&munger.munge(source.subject.as_deref()));
                let task_id = Self::create_linked_task(
                    repo,
                    ctx,
                    &account,
                    &title,
                    source.rfc_message_id.as_deref(),
                    CachedEmailMetadata {
                        subject: source.subject.clone(),
                        from: source.from.clone(),
                        date: source.date.clone(),
                        account: Some(source.account_name.clone()),
                        folder: Some(source.folder_name.clone()),
                    },
                    postpone,
                    due,
                    &mut undo,
                )?;
                created.push(Self::load_detail(
                    repo,
                    task_id,
                    self.detail_locale(),
                    &self.store,
                )?);
            }

            // One undo entry for the whole selection: a keystroke over twelve
            // messages is one action, not twelve (§10.3).
            self.record_tasks(undo);
            Ok(created)
        })?;

        Ok(details)
    }

    /// Link an existing message to an existing task.
    pub fn link_message_to_task(
        &self,
        task_id: i64,
        message_id: i64,
    ) -> Result<TaskLinkInfo, OrbError> {
        let source = self
            .store
            .message_link_sources(&[message_id])
            .map_err(OrbError::from)?
            .into_iter()
            .next()
            .ok_or_else(|| OrbError::NotFound {
                message: format!("Message {message_id} not found"),
            })?;
        let canonical = source
            .rfc_message_id
            .as_deref()
            .and_then(canonical_message_id)
            .ok_or_else(|| OrbError::InvalidInput {
                message: "this message has no Message-ID, so nothing can link to it".to_string(),
            })?;

        let metadata = CachedEmailMetadata {
            subject: source.subject.clone(),
            from: source.from.clone(),
            date: source.date.clone(),
            account: Some(source.account_name.clone()),
            folder: Some(source.folder_name.clone()),
        };
        let linked_before = self.with_tasks(|_, repo, ctx| {
            let before = repo
                .get_linked_emails(task_id)?
                .into_iter()
                .find(|link| link.message_id == canonical)
                .map(|link| link_metadata(&link));
            repo.link_email(task_id, &canonical, &metadata, ctx)?;
            Ok(before)
        })?;
        self.history().record(
            "link a message",
            vec![UndoAction::Link {
                task_id,
                message_id: canonical.clone(),
                metadata: linked_before,
            }],
        );

        Ok(TaskLinkInfo {
            message_id: canonical,
            subject: metadata.subject,
            from: metadata.from,
            date: metadata.date,
            account_name: metadata.account,
            folder_name: metadata.folder,
            answered: false,
            replied_at: None,
            chat: None,
        })
        .map(|link| with_reply_status(&self.store, link))
    }

    /// Remove a link from a task, leaving both the task and the message alone.
    pub fn unlink_message_from_task(
        &self,
        task_id: i64,
        message_id: String,
    ) -> Result<bool, OrbError> {
        let (before, removed) = self.with_tasks(|_, repo, ctx| {
            let before = repo
                .get_linked_emails(task_id)?
                .into_iter()
                .find(|link| link.message_id == message_id)
                .map(|link| link_metadata(&link));
            Ok((before, repo.unlink_email(task_id, &message_id, ctx)?))
        })?;
        if removed {
            self.history().record(
                "unlink a message",
                vec![UndoAction::Link {
                    task_id,
                    message_id,
                    metadata: before,
                }],
            );
        }
        Ok(removed)
    }

    /// Link every message a task's notes name by `message://` URL — as
    /// Things and Apple Mail write a link to a mail — to that task, as one
    /// undoable step. A link already there is filled in with what this
    /// machine has of its message, where it had nothing. How many tasks
    /// changed.
    pub fn link_mail_in_notes(&self) -> Result<u32, OrbError> {
        let noted: Vec<(i64, Vec<String>)> = self.with_tasks(|db, _, _| {
            let mut stmt = db
                .conn
                .prepare(
                    "SELECT id, body_text FROM tasks
                     WHERE deleted_at IS NULL AND body_text LIKE '%message:%'",
                )
                .map_err(crate::Error::from)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(crate::Error::from)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(crate::Error::from)?;
            Ok(rows
                .into_iter()
                .map(|(id, body)| (id, crate::tasks::message_url_ids(&body)))
                .filter(|(_, ids)| !ids.is_empty())
                .collect())
        })?;

        let mut undo = Vec::new();
        let mut changed = 0;
        for (task_id, ids) in noted {
            let metadata: Vec<CachedEmailMetadata> =
                ids.iter().map(|id| self.local_link_metadata(id)).collect();
            let actions = self.with_tasks(|_, repo, ctx| {
                let existing = repo.get_linked_emails(task_id)?;
                let mut actions = Vec::new();
                for (message_id, metadata) in ids.iter().zip(metadata) {
                    let before = existing
                        .iter()
                        .find(|link| &link.message_id == message_id)
                        .map(link_metadata);
                    // A link stays as it is unless this machine knows more of
                    // its message than it does.
                    if let Some(before) = &before
                        && (before.subject.is_some()
                            || before.from.is_some()
                            || metadata == CachedEmailMetadata::default())
                    {
                        continue;
                    }
                    repo.link_email(task_id, message_id, &metadata, ctx)?;
                    actions.push(UndoAction::Link {
                        task_id,
                        message_id: message_id.clone(),
                        metadata: before,
                    });
                }
                Ok(actions)
            })?;
            if !actions.is_empty() {
                changed += 1;
                undo.extend(actions);
            }
        }
        if !undo.is_empty() {
            self.history()
                .record(label("link mail in", changed as usize), undo);
        }
        Ok(changed)
    }

    /// What this machine has of a message, to remember with a link to it;
    /// nothing where it has not got it.
    fn local_link_metadata(&self, message_id: &str) -> CachedEmailMetadata {
        let copies = self
            .store
            .find_messages_by_rfc_id(message_id)
            .unwrap_or_default();
        crate::store::prefer_located_message(copies, None, None)
            .map(|item| found_link_metadata(&item))
            .unwrap_or_default()
    }

    /// Resolve a task's link from the local index alone — §6.2 step 1.
    ///
    /// Instant and network-free, so a task detail can call it while rendering.
    /// A miss returns the cached metadata marked
    /// [`LinkResolution::Cached`]; the caller then decides whether to spend a
    /// multi-folder server search on it via
    /// [`OrbHandle::resolve_linked_message`].
    pub fn linked_message(
        &self,
        task_id: i64,
        message_id: String,
    ) -> Result<LinkedMessage, OrbError> {
        let link = self.link_of(task_id, &message_id)?;
        let candidates = self
            .store
            .find_messages_by_rfc_id(&message_id)
            .map_err(OrbError::from)?;
        let best = crate::store::prefer_located_message(
            candidates,
            link.as_ref().and_then(|l| l.cached_account.as_deref()),
            link.as_ref().and_then(|l| l.cached_folder.as_deref()),
        );

        // A link made before its message was here — from a `message://` URL —
        // remembers it once it has been found.
        if let (Some(item), Some(link)) = (&best, &link)
            && link.cached_subject.is_none()
            && link.cached_from.is_none()
        {
            let metadata = found_link_metadata(item);
            self.with_tasks(|_, repo, _| {
                repo.update_linked_email_cache(&message_id, &metadata)?;
                Ok(())
            })?;
        }
        Ok(match best {
            Some(item) => LinkedMessage::found(&item, LinkResolution::Local),
            None => LinkedMessage::cached(message_id, link.as_ref()),
        })
    }

    /// Resolve a task's link the slow way — §6.2 steps 2 and 3.
    ///
    /// Tries the local index first (it is free, and the message may have arrived
    /// since), then asks the designated account's server for the `Message-ID`,
    /// starting with the folder the link remembers and continuing through the
    /// rest of the mailbox. A hit is synced into the local cache, the remembered
    /// folder is updated so the next lookup is instant, and the message is
    /// returned ready to open.
    ///
    /// A miss is not an error. The cached subject, sender and date come back
    /// marked [`LinkResolution::Cached`], which is the read-only "message no
    /// longer available" state: a missing message must never break the task.
    ///
    /// Separate from [`OrbHandle::linked_message`] precisely because a
    /// multi-folder search is not instantaneous — §6.2 has the UI run this as a
    /// visible background task rather than during a list render.
    pub fn resolve_linked_message(
        &self,
        task_id: i64,
        message_id: String,
    ) -> Result<LinkedMessage, OrbError> {
        let local = self.linked_message(task_id, message_id.clone())?;
        if local.resolution != LinkResolution::Cached {
            return Ok(local);
        }

        let link = self.link_of(task_id, &message_id)?;
        let account = self.tasks_config()?.account.clone();
        let preferred = link.as_ref().and_then(|l| l.cached_folder.clone());

        let found = self
            .runtime
            .block_on(self.store.find_message_on_server(
                &account,
                &message_id,
                preferred.as_deref(),
            ))
            .map_err(OrbError::from)?;

        let Some(item) = found else {
            return Ok(LinkedMessage::cached(message_id, link.as_ref()));
        };

        // Remember where it turned out to be, so the next lookup is step 1. The
        // subject and sender are refreshed from the live message at the same
        // time: the cache exists to survive the message's disappearance, not to
        // preserve a stale copy of a message that is right there.
        let metadata = found_link_metadata(&item);
        self.with_tasks(|_, repo, _| {
            repo.update_linked_email_cache(&message_id, &metadata)?;
            Ok(())
        })?;

        Ok(LinkedMessage::found(&item, LinkResolution::Server))
    }

    /// One task's link row, for its cached metadata.
    fn link_of(&self, task_id: i64, message_id: &str) -> Result<Option<TaskLinkedEmail>, OrbError> {
        self.with_tasks(|_, repo, _| {
            Ok(repo
                .get_linked_emails(task_id)?
                .into_iter()
                .find(|link| link.message_id == message_id))
        })
    }

    /// How many open tasks each of these messages has (§6.3).
    ///
    /// The message list's task column. Two queries for the whole visible set —
    /// one to turn row ids into `Message-ID`s, one to count the links — never a
    /// query per row. Messages with no tasks are absent from the result rather
    /// than reported as zero, because the column draws nothing for them.
    pub fn open_task_counts(
        &self,
        message_ids: Vec<i64>,
    ) -> Result<Vec<MessageTaskCount>, OrbError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rfc_ids = self
            .store
            .rfc_message_ids(&message_ids)
            .map_err(OrbError::from)?;
        if rfc_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Links are stored under the canonical bracketed spelling, so that is
        // what is asked for; a message stored without brackets still maps onto
        // the same key.
        let mut canonical: HashMap<i64, String> = HashMap::with_capacity(rfc_ids.len());
        for (local_id, rfc_id) in rfc_ids {
            if let Some(key) = canonical_message_id(&rfc_id) {
                canonical.insert(local_id, key);
            }
        }
        let keys: Vec<String> = canonical.values().cloned().collect();

        let counts =
            self.with_tasks(|_, repo, _| Ok(repo.open_task_counts_for_messages(&keys)?))?;

        // One row per message, in the order asked for. A caller that passed the
        // same id twice — two panes showing one thread — gets one answer, not a
        // list the UI has to fold itself.
        let mut seen = std::collections::HashSet::with_capacity(message_ids.len());
        Ok(message_ids
            .iter()
            .filter(|id| seen.insert(**id))
            .filter_map(|id| {
                let key = canonical.get(id)?;
                let count = counts.get(key).copied().unwrap_or(0);
                (count > 0).then_some(MessageTaskCount {
                    message_id: *id,
                    open_tasks: count as u32,
                })
            })
            .collect())
    }

    /// The tasks linked to one message, for the detail view (§6.3).
    ///
    /// Returned as ordinary summaries so they can be acted on in place —
    /// completed, postponed, opened — with the same calls the task list uses.
    pub fn tasks_for_message(&self, message_id: i64) -> Result<Vec<TaskSummary>, OrbError> {
        let Some(rfc_id) = self
            .store
            .rfc_message_ids(&[message_id])
            .map_err(OrbError::from)?
            .remove(&message_id)
            .as_deref()
            .and_then(canonical_message_id)
        else {
            return Ok(Vec::new());
        };

        self.with_tasks(|_, repo, _| {
            let today = repo.today();
            let registry = repo.list_tags()?;
            Ok(repo
                .tasks_for_message(&rfc_id)?
                .into_iter()
                .map(|item| TaskSummary::from_item(item, today, &registry))
                .collect())
        })
    }
}

impl From<crate::chat::ChatError> for OrbError {
    fn from(err: crate::chat::ChatError) -> Self {
        OrbError::SyncError {
            message: err.to_string(),
        }
    }
}

/// A file the composer posts: a path on disk, or the bytes of a pasted one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatFileInput {
    pub name: String,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub data_base64: Option<String>,
}

/// A task's title from a chat message: its first line of text, the empty
/// link a quote starts with left out, cut at a word near 80 characters.
fn chat_task_title(text: &str) -> String {
    const MAX: usize = 80;
    let text = text.trim_start();
    let text = if text.starts_with("[ ](") {
        text.split_once(')').map_or(text, |(_, rest)| rest)
    } else {
        text
    };
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end())
}

/// Chat (PLAN.md, Phase 15): one Rocket.Chat server, synced by
/// [`crate::chat::ChatService`] on the core runtime.
impl OrbHandle {
    /// The configured chat server, if there is one.
    pub fn chat_server(&self) -> Option<String> {
        self.config().chat.as_ref().map(|chat| chat.server.clone())
    }

    /// Start syncing chat; does nothing without a chat section, or a second
    /// time.
    pub fn start_chat(&self, listener: Box<dyn crate::chat::ChatListener>) {
        let Some(config) = self.config().chat.clone() else {
            return;
        };
        let mut slot = self.chat.write().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let _guard = self.runtime.enter();
        *slot = Some(crate::chat::ChatService::start(
            config,
            self.config().db_path.clone(),
            Arc::new(crate::keychain::Keychain),
            Arc::from(listener),
            self.push_shutdown.child_token(),
        ));
    }

    fn chat(&self) -> Result<Arc<crate::chat::ChatService>, OrbError> {
        self.chat
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| OrbError::ConfigError {
                message: "chat is not configured".to_string(),
            })
    }

    pub fn chat_status(&self) -> Result<crate::chat::ChatStatus, OrbError> {
        Ok(self.chat()?.status())
    }

    /// The threads that wait for an answer, newest first.
    pub fn chat_waiting(&self) -> Result<Vec<crate::chat::WaitingThread>, OrbError> {
        Ok(self.chat()?.waiting()?)
    }

    pub fn chat_rooms(&self) -> Result<Vec<crate::chat::ChatRoom>, OrbError> {
        Ok(self.chat()?.rooms()?)
    }

    /// A thread with its messages, fetching what the sync has not reached.
    pub fn chat_conversation(
        &self,
        rid: String,
        thread: String,
    ) -> Result<Option<crate::chat::Conversation>, OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.conversation(&rid, &thread).await })?)
    }

    /// The messages before `before` (milliseconds), as scrolling to the top of
    /// a conversation asks for them.
    pub fn chat_older(
        &self,
        rid: String,
        thread: String,
        before: i64,
    ) -> Result<crate::chat::OlderMessages, OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.older(&rid, &thread, before).await })?)
    }

    /// Every thread of a room, its own timeline first.
    pub fn chat_room_threads(
        &self,
        rid: String,
    ) -> Result<Vec<crate::chat::WaitingThread>, OrbError> {
        Ok(self.chat()?.room_threads(&rid)?)
    }

    /// What the room picker offers for what was typed.
    pub fn chat_find(&self, term: String) -> Result<Vec<crate::chat::store::ChatPick>, OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.find(&term).await })?)
    }

    /// The direct message room with a person, made if there is none.
    pub fn chat_open_direct(&self, username: String) -> Result<String, OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.open_direct(&username).await })?)
    }

    /// Open the conversation with the people in `usernames`, starting it if
    /// there is none yet.
    pub fn chat_open_group(&self, usernames: Vec<String>) -> Result<String, OrbError> {
        if usernames.is_empty() {
            return Err(OrbError::InvalidInput {
                message: "nobody to write to".to_string(),
            });
        }
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.open_group(&usernames).await })?)
    }

    /// Leave a room, or hide a direct message.
    ///
    /// Recorded under `undo`, when given, unless the room is a private group,
    /// which takes an invitation to come back to. Returns whether it was.
    pub fn chat_leave(&self, rid: String, undo: Option<String>) -> Result<bool, OrbError> {
        let chat = self.chat()?;
        let kind = self.runtime.block_on({
            let rid = rid.clone();
            async move { chat.leave(&rid).await }
        })?;
        let undoable = kind != "p";
        if let Some(label) = undo.filter(|_| undoable) {
            self.history().record(
                label,
                vec![UndoAction::ChatRoom {
                    rid,
                    kind,
                    member: true,
                }],
            );
        }
        Ok(undoable)
    }

    /// Create a channel, or a private group; returns its id.
    pub fn chat_create_room(&self, name: String, private: bool) -> Result<String, OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.create_room(&name, private).await })?)
    }

    /// The reactions the config offers on a message, as shortcodes.
    pub fn chat_reactions(&self) -> Vec<String> {
        self.config()
            .chat
            .as_ref()
            .map(|chat| chat.reactions.clone())
            .unwrap_or_default()
    }

    /// Whether the chat server has threads turned on.
    pub fn chat_threads(&self) -> bool {
        self.chat().map_or(true, |chat| chat.threads())
    }

    /// The text the config turns into links in a message.
    pub fn chat_links(&self) -> Vec<crate::config::ChatLinkConfig> {
        self.config()
            .chat
            .as_ref()
            .map(|chat| chat.links.clone())
            .unwrap_or_default()
    }

    /// Post files to a thread, or the room's timeline, `text` as the first
    /// one's message. Each is a file on disk, or (pasted) its bytes.
    pub fn chat_send_files(
        &self,
        rid: String,
        thread: String,
        files: Vec<ChatFileInput>,
        text: String,
    ) -> Result<(), OrbError> {
        use base64::Engine as _;
        let mut uploads = Vec::new();
        for file in files {
            let bytes = match (&file.path, &file.data_base64) {
                (Some(path), _) => std::fs::read(path).map_err(|e| OrbError::InvalidInput {
                    message: format!("{}: {e}", file.name),
                })?,
                (None, Some(data)) => base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| OrbError::InvalidInput {
                        message: format!("{}: {e}", file.name),
                    })?,
                (None, None) => continue,
            };
            uploads.push(crate::chat::ChatUpload {
                mime: file
                    .mime
                    .clone()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| crate::attachments::guess_content_type(&file.name)),
                name: file.name,
                bytes,
            });
        }
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.send_files(&rid, &thread, uploads, &text).await })?)
    }

    /// Replace the text of one of the user's messages.
    pub fn chat_edit(&self, rid: String, id: String, text: String) -> Result<(), OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.edit(&rid, &id, &text).await })?)
    }

    /// Delete one of the user's messages.
    pub fn chat_delete(&self, rid: String, id: String) -> Result<(), OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.delete(&rid, &id).await })?)
    }

    /// Add or take back the user's reaction to a message.
    pub fn chat_react(
        &self,
        rid: String,
        id: String,
        emoji: String,
        on: bool,
        undo: Option<String>,
    ) -> Result<(), OrbError> {
        let chat = self.chat()?;
        self.runtime.block_on({
            let (rid, id, emoji) = (rid.clone(), id.clone(), emoji.clone());
            async move { chat.react(&rid, &id, &emoji, on).await }
        })?;
        if let Some(label) = undo {
            self.history().record(
                label,
                vec![UndoAction::ChatReaction {
                    rid,
                    id,
                    emoji,
                    on: !on,
                }],
            );
        }
        Ok(())
    }

    /// Mark a thread read or unread, in Orb and on the server.
    /// Recorded under `undo`, when given.
    pub fn chat_set_read(
        &self,
        rid: String,
        thread: String,
        read: bool,
        undo: Option<String>,
    ) -> Result<(), OrbError> {
        let chat = self.chat()?;
        self.runtime.block_on({
            let (rid, thread) = (rid.clone(), thread.clone());
            async move { chat.set_read(&rid, &thread, read).await }
        })?;
        if let Some(label) = undo {
            self.history().record(
                label,
                vec![UndoAction::ChatRead {
                    rid,
                    thread,
                    read: !read,
                }],
            );
        }
        Ok(())
    }

    /// A message's file on disk, fetched from the server the first time.
    pub fn chat_file_path(&self, id: String, index: usize) -> Result<String, OrbError> {
        let chat = self.chat()?;
        let dir = attachment_cache_root()
            .join("chat")
            .join(id.replace(['/', '\\'], "_"))
            .join(index.to_string());
        // A file once fetched does not change: the server gives a new one a new id.
        if let Some(found) = std::fs::read_dir(&dir)
            .ok()
            .and_then(|mut entries| entries.next())
            .and_then(Result::ok)
        {
            return Ok(found.path().to_string_lossy().into_owned());
        }
        let (name, bytes) = self
            .runtime
            .block_on(async move { chat.file(&id, index).await })?;
        // Written beside the directory and moved in whole, so that a write cut
        // short is never taken for the file.
        let part = dir.with_extension("part");
        write_file(&part, &bytes)?;
        let path = dir.join(safe_filename(&name));
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::rename(&part, &path))
            .map_err(|e| OrbError::ConfigError {
                message: format!("could not write {}: {e}", path.display()),
            })?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// A message's image preview as a `data:` URL, fetched once and kept
    /// beside the file; `None` for a file that has none.
    pub fn chat_file_preview(&self, id: String, index: usize) -> Result<Option<String>, OrbError> {
        use base64::Engine as _;
        let chat = self.chat()?;
        let cached = attachment_cache_root()
            .join("chat")
            .join(id.replace(['/', '\\'], "_"))
            .join(format!("{index}.preview"));
        let (mime, bytes) = match std::fs::read(&cached) {
            // The type is the first line, the image the rest.
            Ok(stored) => match stored.iter().position(|&b| b == b'\n') {
                Some(at) => (
                    String::from_utf8_lossy(&stored[..at]).into_owned(),
                    stored[at + 1..].to_vec(),
                ),
                None => return Ok(None),
            },
            Err(_) => {
                let Some((mime, bytes)) = self
                    .runtime
                    .block_on(async move { chat.file_preview(&id, index).await })?
                else {
                    return Ok(None);
                };
                // The server may make a thumbnail in another format than the image's.
                let mime = image_type(&bytes)
                    .map(str::to_string)
                    .or(mime.filter(|m| m.starts_with("image/")))
                    .unwrap_or_else(|| "image/png".to_string());
                let mut stored = format!("{mime}\n").into_bytes();
                stored.extend_from_slice(&bytes);
                // Written beside and moved in whole, as the files are.
                let part = cached.with_file_name(format!("{index}.preview.part"));
                write_file(&part, &stored)?;
                std::fs::rename(&part, &cached).map_err(|e| OrbError::ConfigError {
                    message: format!("could not write {}: {e}", cached.display()),
                })?;
                (mime, bytes)
            }
        };
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        Ok(Some(format!("data:{mime};base64,{data}")))
    }

    /// Join a public channel.
    pub fn chat_join(&self, rid: String) -> Result<(), OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.join(&rid).await })?)
    }

    /// The user has seen the thread; nothing changes on the server.
    pub fn chat_mark_seen(&self, rid: String, thread: String) -> Result<(), OrbError> {
        Ok(self.chat()?.mark_seen(&rid, &thread)?)
    }

    /// Take a thread out of Waiting until someone writes again, and mark it
    /// read on the server. Recorded for undo under `undo`; undoing it lets
    /// the thread wait again, and leaves the server's read state alone.
    pub fn chat_done(&self, rid: String, thread: String, undo: String) -> Result<(), OrbError> {
        let chat = self.chat()?;
        let previous = self.runtime.block_on({
            let (rid, thread) = (rid.clone(), thread.clone());
            async move {
                chat.mark(&rid, &thread, crate::chat::store::MARK_DONE)
                    .await
            }
        })?;
        self.history().record(
            undo,
            vec![UndoAction::ChatMark {
                rid,
                thread,
                mark: previous,
            }],
        );
        // The other Macs learn of it through the tasks account.
        self.task_changed();
        Ok(())
    }

    /// Mute a room until `ends` (ms), or until it is unmuted (`None`): its
    /// threads leave Waiting, Today and the notifications, but for one that
    /// names the user or calls the room (chat-spec §13). Recorded for undo
    /// under `undo`; the other Macs hear of it through the tasks account.
    pub fn chat_mute(&self, rid: String, ends: Option<i64>, undo: String) -> Result<(), OrbError> {
        let previous = self.chat()?.mute(&rid, ends)?;
        self.record_mute(rid, previous, undo);
        Ok(())
    }

    /// End a room's mute, recorded for undo under `undo`.
    pub fn chat_unmute(&self, rid: String, undo: String) -> Result<(), OrbError> {
        let previous = self.chat()?.unmute(&rid)?;
        self.record_mute(rid, previous, undo);
        Ok(())
    }

    fn record_mute(&self, rid: String, previous: Option<(String, i64)>, undo: String) {
        self.history().record(
            undo,
            vec![UndoAction::ChatMark {
                rid,
                thread: crate::chat::store::MUTE_THREAD.into(),
                mark: previous,
            }],
        );
        self.task_changed();
    }

    /// Make a task of a chat message — `message`, or the thread's newest — and
    /// take the thread out of Waiting, marked read on the server (todo-spec
    /// §28). `postpone` as for [`OrbHandle::create_tasks_from_messages`].
    /// One action for undo, which trashes the task and lets the thread wait
    /// again.
    pub fn chat_create_task(
        &self,
        rid: String,
        thread: String,
        message: Option<String>,
        postpone: Option<String>,
    ) -> Result<TaskDetail, OrbError> {
        let chat = self.chat()?;
        let postpone = postpone.as_deref().map(parse_task_date).transpose()?;
        let source = chat.task_source(&rid, &thread, message.as_deref())?;
        let title = normalise_title(&chat_task_title(&source.text));
        let account = self.tasks_config()?.account.clone();
        let mut undo = UndoEntry {
            label: "create 1 task from chat".to_string(),
            steps: Vec::new(),
        };
        let detail = self.with_tasks(|_, repo, ctx| {
            let task = repo.create_task(&title, &account, ctx)?;
            let task_id = task.id.unwrap_or_default();
            undo.steps.extend(UndoEntry::for_creation(&task, "").steps);
            repo.link_email(
                task_id,
                &source.link,
                &CachedEmailMetadata {
                    subject: Some(source.text.clone()),
                    from: Some(source.author.clone()),
                    date: Some(source.date.clone()),
                    account: Some(source.room.clone()),
                    folder: Some(source.permalink.clone()),
                },
                ctx,
            )?;
            if postpone.is_some() {
                repo.set_postpone_date(&[task_id], postpone, ctx)?;
            }
            Self::load_detail(repo, task_id, self.detail_locale(), &self.store)
        })?;
        let label = undo.label.clone();
        let mut steps = vec![UndoAction::Tasks(undo)];
        let marked = self.runtime.block_on({
            let (rid, thread) = (rid.clone(), thread.clone());
            async move {
                chat.mark(&rid, &thread, crate::chat::store::MARK_TASK)
                    .await
            }
        });
        // The task stands whether or not the thread could be marked, and
        // undoing it must still take the task back.
        if let Ok(previous) = &marked {
            steps.push(UndoAction::ChatMark {
                rid,
                thread,
                mark: previous.clone(),
            });
        }
        self.history().record(label, steps);
        marked?;
        self.task_changed();
        Ok(detail)
    }

    /// The threads that started waiting after `after` and may notify (PLAN.md
    /// 15.10), and the watermark to pass next time.
    pub fn chat_new_waiting_since(
        &self,
        after: i64,
    ) -> Result<(Vec<crate::chat::WaitingThread>, i64), OrbError> {
        Ok(self.chat()?.new_waiting_since(after)?)
    }

    /// Where notifications count from at launch: what waits already has been seen.
    pub fn chat_waiting_watermark(&self) -> i64 {
        self.chat().map_or(0, |chat| chat.waiting_watermark())
    }

    /// Send a message into a thread, or the room's timeline when `thread` is
    /// empty.
    pub fn chat_send(
        &self,
        rid: String,
        thread: String,
        text: String,
        also_in_room: bool,
    ) -> Result<(), OrbError> {
        let chat = self.chat()?;
        Ok(self
            .runtime
            .block_on(async move { chat.send(&rid, &thread, &text, also_in_room).await })?)
    }

    /// Take the session the sign-in window caught.
    pub fn chat_sign_in(&self, user_id: String, token: String) -> Result<(), OrbError> {
        let chat = self.chat()?;
        Ok(self.runtime.block_on(async move {
            chat.sign_in(user_id, secrecy::SecretString::from(token))
                .await
        })?)
    }

    pub fn chat_sign_out(&self) -> Result<(), OrbError> {
        self.chat()?.sign_out();
        Ok(())
    }

    pub fn chat_sync(&self) -> Result<(), OrbError> {
        self.chat()?.sync_now();
        Ok(())
    }

    /// A link that opens the thread in Rocket.Chat.
    pub fn chat_permalink(&self, rid: String, thread: String) -> Result<String, OrbError> {
        Ok(self.chat()?.permalink(&rid, &thread)?)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn attachments_of_one_name_are_told_apart() {
        let mut taken = std::collections::HashSet::new();
        let names: Vec<String> = ["image.png", "image.png", "Image.PNG", "notes", "notes"]
            .into_iter()
            .map(|n| unique_name(&mut taken, n.to_string()))
            .collect();
        assert_eq!(
            names,
            [
                "image.png",
                "image (2).png",
                "Image (3).PNG",
                "notes",
                "notes (2)"
            ]
        );
    }
    use super::*;
    use crate::models::Folder;

    #[test]
    fn test_orb_error_from_not_found() {
        let err = OrbCoreError::NotFound("test".to_string());
        let ffi_err: OrbError = err.into();
        assert!(matches!(ffi_err, OrbError::NotFound { .. }));
    }

    pub(super) fn account_with_trash(name: &str, trash: &str) -> config::AccountConfig {
        config::AccountConfig {
            name: name.to_string(),
            imap_server: "imap.test.com".to_string(),
            imap_port: 993,
            imap_username: "test@test.com".to_string(),
            imap_password_command: Some("echo test".to_string()),
            from: "test@test.com".to_string(),
            smtp_server: None,
            smtp_port: None,
            smtp_username: None,
            smtp_password_command: None,
            signature_file: None,
            identities: Vec::new(),
            domains: Vec::new(),
            drafts_folder: "Drafts".to_string(),
            sent_folder: "Sent".to_string(),
            trash_folder: trash.to_string(),
            archive_folder: None,
            junk_folder: None,
            default_sort: Default::default(),
            folder_order: Vec::new(),
            folders: std::collections::HashMap::new(),
            accept_invalid_certs: false,
            sync_interval_seconds: None,
            threads: false,
            imap_encryption: Default::default(),
            imap_auth: Default::default(),
            smtp_encryption: None,
            smtp_auth: Default::default(),
        }
    }

    pub(super) fn config_with_accounts(accounts: Vec<config::AccountConfig>) -> Config {
        Config {
            db_path: ":memory:".to_string(),
            accounts,
            bindings: Default::default(),
            theme: Default::default(),
            log_file: None,
            sidebar_mode: Default::default(),
            virtual_folders: Default::default(),
            pinned: None,
            pins: Vec::new(),
            subject_munges: Default::default(),
            message_links: Default::default(),
            contacts: Default::default(),
            load_remote_images_from: Vec::new(),
            mark_read_after_seconds: 2.0,
            embedding_model: None,
            default_message_view: Default::default(),
            reply: Default::default(),
            compose: Default::default(),
            tasks: None,
            chat: None,
            reading: None,
            filters: Default::default(),
            highlights: Default::default(),
            load_remote_images: Default::default(),
            list: Default::default(),
            search: Default::default(),
            appearance: Default::default(),
            dock_badges: Vec::new(),
            navigation: Default::default(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn test_trash_folder_returns_configured_name() {
        let config = config_with_accounts(vec![account_with_trash("work", "Deleted Items")]);
        assert_eq!(trash_folder_for(&config, "work"), "Deleted Items");
    }

    #[test]
    fn test_trash_folder_falls_back_to_default_for_unknown_account() {
        let config = config_with_accounts(vec![account_with_trash("work", "Deleted Items")]);
        assert_eq!(trash_folder_for(&config, "missing"), "Trash");
    }

    pub(super) fn sample_send_input() -> SendMailInput {
        SendMailInput {
            account_name: "work".to_string(),
            to: vec!["alice@example.com".to_string()],
            cc: vec!["carol@example.com".to_string()],
            bcc: vec!["dan@example.com".to_string()],
            subject: "Re: Hello".to_string(),
            body: "Hi there".to_string(),
            in_reply_to: Some("<msg-1@example.com>".to_string()),
            references: Some("<root@example.com> <msg-1@example.com>".to_string()),
            forwards: None,
            original_message_id: Some(42),
            attachment_paths: Vec::new(),
            identity: None,
            follow_up: None,
        }
    }

    #[test]
    fn test_outgoing_email_from_input_maps_recipients_and_content() {
        let input = sample_send_input();
        let email = outgoing_email_from_input(&input);

        assert_eq!(email.to, vec!["alice@example.com".to_string()]);
        assert_eq!(email.cc, vec!["carol@example.com".to_string()]);
        assert_eq!(email.bcc, vec!["dan@example.com".to_string()]);
        assert_eq!(email.subject, "Re: Hello");
        assert_eq!(email.body, "Hi there");
    }

    #[test]
    fn test_outgoing_email_from_input_preserves_reply_threading_headers() {
        let input = sample_send_input();
        let email = outgoing_email_from_input(&input);

        assert_eq!(email.in_reply_to.as_deref(), Some("<msg-1@example.com>"));
        assert_eq!(
            email.references.as_deref(),
            Some("<root@example.com> <msg-1@example.com>")
        );
        // A valid reply email (recipient + subject + body + headers) must pass
        // the SMTP-layer validation.
        assert!(email.validate().is_ok());
    }

    #[test]
    fn test_read_attachments_reads_files_and_infers_content_type() {
        let dir = std::env::temp_dir().join(format!("orb-attach-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("report.pdf");
        std::fs::write(&pdf, b"Hello PDF").unwrap();

        let attachments =
            read_attachments(&[pdf.to_string_lossy().to_string()]).expect("attachments read");

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "report.pdf");
        assert_eq!(attachments[0].content_type, "application/pdf");
        assert_eq!(attachments[0].bytes, b"Hello PDF");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_attachments_missing_file_is_send_error() {
        let result = read_attachments(&["/no/such/file/anywhere.pdf".to_string()]);
        assert!(matches!(result, Err(OrbError::SendError { .. })));
    }

    #[test]
    fn test_read_attachments_empty_is_ok_and_empty() {
        let attachments = read_attachments(&[]).expect("no paths reads cleanly");
        assert!(attachments.is_empty());
    }

    #[test]
    fn test_bindings_config_empty_maps() {
        let bc = RustBindingsConfig::default();
        let ffi_bc = BindingsConfig::from(&bc);

        // Empty maps should become None
        assert!(ffi_bc.index_normal.is_none());
        assert!(ffi_bc.index_detail.is_none());
    }

    #[test]
    fn test_bindings_config_non_empty_maps_are_cloned_through() {
        let mut bc = RustBindingsConfig::default();
        bc.index_normal.insert("j".to_string(), "down".to_string());
        bc.command
            .insert("enter".to_string(), "execute".to_string());

        let ffi_bc = BindingsConfig::from(&bc);

        assert_eq!(
            ffi_bc.index_normal.as_ref().unwrap().get("j"),
            Some(&"down".to_string())
        );
        assert_eq!(
            ffi_bc.command.as_ref().unwrap().get("enter"),
            Some(&"execute".to_string())
        );
        // Untouched maps stay None.
        assert!(ffi_bc.compose_normal.is_none());
        assert!(ffi_bc.task_detail.is_none());
    }

    // ------------------------------------------------------------------
    // MessageHeader conversion
    // ------------------------------------------------------------------

    fn sample_list_item(flags: Vec<String>) -> MessageListItem {
        MessageListItem {
            id: 7,
            folder_id: 3,
            uid: 101,
            message_id: Some("<abc@example.com>".to_string()),
            subject: Some("Hello".to_string()),
            from: Some("Alice <alice@example.com>".to_string()),
            to: Some("bob@example.com".to_string()),
            cc: None,
            date: Some("2026-01-02T03:04:05Z".to_string()),
            size: Some(2048),
            flags,
            modseq: Some(55),
        }
    }

    #[test]
    fn test_message_header_from_list_item_maps_fields_and_defaults_no_attachments() {
        let item = sample_list_item(vec!["\\Seen".to_string()]);
        let header = MessageHeader::from(item);

        assert_eq!(header.id, 7);
        assert_eq!(header.uid, 101);
        assert_eq!(
            header.from_display.as_deref(),
            Some("Alice <alice@example.com>")
        );
        assert_eq!(header.subject.as_deref(), Some("Hello"));
        assert_eq!(header.date.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert!(header.is_read);
        assert!(!header.is_flagged);
        // The From impl always reports no attachments; the real flag is supplied
        // separately via from_item (see get_messages).
        assert!(!header.has_attachments);
    }

    #[test]
    fn test_has_system_flag_accepts_wire_and_legacy_forms() {
        // Canonical wire form, as written by `ImapClient::flag_to_wire`.
        assert!(has_system_flag(&["\\Seen".to_string()], "Seen"));
        assert!(has_system_flag(&["\\Flagged".to_string()], "Flagged"));

        // Legacy Rust-debug form persisted by older syncs. Reading these
        // correctly is what spares existing databases a full rewrite.
        assert!(has_system_flag(&["Seen".to_string()], "Seen"));
        assert!(has_system_flag(&["Flagged".to_string()], "Flagged"));

        // Case-insensitive per RFC 3501.
        assert!(has_system_flag(&["\\seen".to_string()], "Seen"));
    }

    #[test]
    fn test_has_system_flag_rejects_non_matches() {
        assert!(!has_system_flag(&["\\Answered".to_string()], "Seen"));
        assert!(!has_system_flag(&[], "Seen"));

        // Custom keywords must not be mistaken for system flags, in either the
        // legacy debug encoding or as a bare keyword.
        assert!(!has_system_flag(&["Custom(\"Seen\")".to_string()], "Seen"));
        assert!(!has_system_flag(&["Seenish".to_string()], "Seen"));
    }

    #[test]
    fn test_message_header_reads_legacy_debug_flags() {
        // The exact shape found in the live database: flags stored as
        // ["Seen","Custom(\"Old\")"] must still report as read.
        let header = MessageHeader::from(sample_list_item(vec![
            "Seen".to_string(),
            "Custom(\"Old\")".to_string(),
        ]));
        assert!(
            header.is_read,
            "legacy debug-form \\Seen must count as read"
        );
        assert!(!header.is_flagged);
    }

    #[test]
    fn test_message_header_flag_detection() {
        let seen_and_flagged = MessageHeader::from(sample_list_item(vec![
            "\\Seen".to_string(),
            "\\Flagged".to_string(),
        ]));
        assert!(seen_and_flagged.is_read);
        assert!(seen_and_flagged.is_flagged);

        let unread_unflagged = MessageHeader::from(sample_list_item(vec![]));
        assert!(!unread_unflagged.is_read);
        assert!(!unread_unflagged.is_flagged);

        // An unrelated flag toggles neither.
        let other = MessageHeader::from(sample_list_item(vec!["\\Draft".to_string()]));
        assert!(!other.is_read);
        assert!(!other.is_flagged);
        assert!(!other.is_answered);

        let answered = MessageHeader::from(sample_list_item(vec!["\\Answered".to_string()]));
        assert!(answered.is_answered);
        assert!(!answered.is_forwarded);

        let forwarded = MessageHeader::from(sample_list_item(vec!["$Forwarded".to_string()]));
        assert!(forwarded.is_forwarded);
        assert!(!forwarded.is_answered);
    }

    #[test]
    fn test_message_header_from_item_honours_attachment_flag() {
        let with = MessageHeader::from_item(sample_list_item(vec![]), true);
        assert!(with.has_attachments);

        let without = MessageHeader::from_item(sample_list_item(vec![]), false);
        assert!(!without.has_attachments);
    }

    #[test]
    fn test_message_header_preserves_none_optionals() {
        let mut item = sample_list_item(vec![]);
        item.subject = None;
        item.from = None;
        item.date = None;
        let header = MessageHeader::from(item);
        assert!(header.from_display.is_none());
        assert!(header.subject.is_none());
        assert!(header.date.is_none());
    }

    // ------------------------------------------------------------------
    // FolderInfo conversion
    // ------------------------------------------------------------------

    #[test]
    fn test_folder_info_from_real_folder() {
        let folder = Folder::new("work".to_string(), "INBOX".to_string());
        let fd = FolderDisplay::new(folder, "Inbox".to_string(), true, false, 4, 42);

        let info = FolderInfo::from(fd);
        assert_eq!(info.account_name, "work");
        assert_eq!(info.display_name, "Inbox");
        assert_eq!(info.folder_name.as_deref(), Some("INBOX"));
        assert!(!info.is_account_header);
        assert!(!info.is_pinned);
        assert!(info.show_unread);
        assert!(!info.show_total);
        assert_eq!(info.unread_count, 4);
        assert_eq!(info.total_count, 42);
    }

    #[test]
    fn test_folder_info_pinned_folder_sets_pinned_flag() {
        let folder = Folder::new("work".to_string(), "INBOX".to_string());
        let fd = FolderDisplay::pinned(folder, "Inbox".to_string(), true, true, 1, 2);
        let info = FolderInfo::from(fd);
        assert!(info.is_pinned);
        assert_eq!(info.folder_name.as_deref(), Some("INBOX"));
    }

    #[test]
    fn test_folder_info_account_header_has_no_folder_name() {
        // An account header row carries no underlying folder, so folder_name
        // must map to None.
        let fd = FolderDisplay::account_header("work");
        let info = FolderInfo::from(fd);
        assert!(info.is_account_header);
        assert!(info.folder_name.is_none());
        assert_eq!(info.display_name, "work");
    }

    // ------------------------------------------------------------------
    // AttachmentMeta / AttachmentData conversions
    // ------------------------------------------------------------------

    #[test]
    fn test_attachment_meta_conversion_maps_all_fields() {
        let core = crate::attachments::AttachmentMeta {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 1234,
            part_id: "1.0".to_string(),
            is_inline: true,
            content_id: Some("img1".to_string()),
        };
        let ffi = AttachmentMeta::from(core);
        assert_eq!(ffi.filename, "report.pdf");
        assert_eq!(ffi.content_type, "application/pdf");
        assert_eq!(ffi.size, 1234);
        assert_eq!(ffi.part_id, "1.0");
        assert!(ffi.is_inline);
        assert_eq!(ffi.content_id.as_deref(), Some("img1"));
        assert_eq!(ffi.risk, None);
    }

    #[test]
    fn test_attachment_data_conversion_maps_all_fields() {
        let core = crate::attachments::ExtractedAttachment {
            filename: "photo.png".to_string(),
            content_type: "image/png".to_string(),
            data: vec![1, 2, 3, 4],
        };
        let ffi = AttachmentData::from(core);
        assert_eq!(ffi.filename, "photo.png");
        assert_eq!(ffi.content_type, "image/png");
        assert_eq!(ffi.data, vec![1, 2, 3, 4]);
    }

    // ------------------------------------------------------------------
    // SearchResult conversion
    // ------------------------------------------------------------------

    #[test]
    fn test_search_result_conversion_wraps_header_and_context() {
        let item = SearchResultItem {
            message: sample_list_item(vec!["\\Seen".to_string()]),
            account_name: "work".to_string(),
            folder_name: "Archive".to_string(),
        };
        let result = SearchResult::from(item);
        assert_eq!(result.account_name, "work");
        assert_eq!(result.folder_name, "Archive");
        assert_eq!(result.message.id, 7);
        assert!(result.message.is_read);
        // Search results carry no attachment flag; the From<MessageListItem>
        // path always reports false.
        assert!(!result.message.has_attachments);
    }

    // ------------------------------------------------------------------
    // SearchScope conversion (FFI -> core)
    // ------------------------------------------------------------------

    #[test]
    fn test_search_scope_conversion_all() {
        let core: RustSearchScope = SearchScope::All.into();
        assert!(matches!(core, RustSearchScope::All));
    }

    #[test]
    fn test_search_scope_conversion_account_and_folder() {
        let account: RustSearchScope = SearchScope::Account {
            account_name: "work".to_string(),
        }
        .into();
        match account {
            RustSearchScope::Account { account_name } => assert_eq!(account_name, "work"),
            other => panic!("expected Account, got {:?}", other),
        }

        let folder: RustSearchScope = SearchScope::Folder {
            account_name: "work".to_string(),
            folder_name: "INBOX".to_string(),
        }
        .into();
        match folder {
            RustSearchScope::Folder {
                account_name,
                folder_name,
            } => {
                assert_eq!(account_name, "work");
                assert_eq!(folder_name, "INBOX");
            }
            other => panic!("expected Folder, got {:?}", other),
        }
    }

    // ------------------------------------------------------------------
    // MessageSortOrder conversion (FFI -> core)
    // ------------------------------------------------------------------

    #[test]
    fn test_message_sort_order_maps_every_variant() {
        // Each FFI variant must reach its core counterpart; a mismatch here
        // would silently order the list by the wrong column.
        let cases = [
            (MessageSortOrder::DateNewest, RustMessageSort::DateNewest),
            (MessageSortOrder::DateOldest, RustMessageSort::DateOldest),
            (MessageSortOrder::SubjectAsc, RustMessageSort::SubjectAsc),
            (MessageSortOrder::SubjectDesc, RustMessageSort::SubjectDesc),
            (MessageSortOrder::SenderAsc, RustMessageSort::SenderAsc),
            (MessageSortOrder::SenderDesc, RustMessageSort::SenderDesc),
        ];

        for (ffi, expected) in cases {
            let core: RustMessageSort = ffi.into();
            assert_eq!(
                core.as_sql(),
                expected.as_sql(),
                "{ffi:?} must map to {expected:?}"
            );
        }
    }

    #[test]
    fn test_message_sort_order_variants_are_distinct() {
        // Guards against two variants collapsing onto the same ORDER BY.
        let sql: std::collections::HashSet<&str> = [
            MessageSortOrder::DateNewest,
            MessageSortOrder::DateOldest,
            MessageSortOrder::SubjectAsc,
            MessageSortOrder::SubjectDesc,
            MessageSortOrder::SenderAsc,
            MessageSortOrder::SenderDesc,
        ]
        .into_iter()
        .map(|o| RustMessageSort::from(o).as_sql())
        .collect();

        assert_eq!(sql.len(), 6);
    }

    // ------------------------------------------------------------------
    // SyncResult conversion
    // ------------------------------------------------------------------

    #[test]
    fn test_sync_result_from_report_maps_counters_and_errors() {
        let report = RustSyncReport {
            accounts_synced: 2,
            folders_synced: 5,
            messages_added: 10,
            messages_updated: 3,
            messages_deleted: 1,
            errors: vec!["boom".to_string(), "kaboom".to_string()],
            ..Default::default()
        };
        let result = SyncResult::from(report);
        assert_eq!(result.folders_synced, 5);
        assert_eq!(result.messages_added, 10);
        assert_eq!(result.messages_updated, 3);
        assert_eq!(result.messages_deleted, 1);
        assert_eq!(
            result.errors,
            vec!["boom".to_string(), "kaboom".to_string()]
        );
    }

    #[test]
    fn test_sync_result_from_default_report_is_all_zero() {
        let result = SyncResult::from(RustSyncReport::default());
        assert_eq!(result.folders_synced, 0);
        assert_eq!(result.messages_added, 0);
        assert_eq!(result.messages_updated, 0);
        assert_eq!(result.messages_deleted, 0);
        assert!(result.errors.is_empty());
    }

    // ------------------------------------------------------------------
    // OrbCoreError -> OrbError mapping (every FFI variant)
    // ------------------------------------------------------------------

    #[test]
    fn test_error_mapping_config_variants() {
        let invalid = OrbError::from(OrbCoreError::InvalidConfig {
            field: "imap_port".to_string(),
            reason: "out of range".to_string(),
        });
        match invalid {
            OrbError::ConfigError { message } => assert!(message.contains("imap_port")),
            other => panic!("expected ConfigError, got {:?}", other),
        }

        let cfg = OrbError::from(OrbCoreError::Config(config::ConfigError::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        )));
        assert!(matches!(cfg, OrbError::ConfigError { .. }));
    }

    #[test]
    fn test_error_mapping_database_variants() {
        let pool = OrbError::from(OrbCoreError::PoolError("no conns".to_string()));
        match pool {
            OrbError::DatabaseError { message } => assert!(message.contains("no conns")),
            other => panic!("expected DatabaseError, got {:?}", other),
        }

        let db = OrbError::from(OrbCoreError::Database(rusqlite::Error::QueryReturnedNoRows));
        assert!(matches!(db, OrbError::DatabaseError { .. }));

        let txn = OrbError::from(OrbCoreError::TransactionFailed {
            operation: "commit".to_string(),
            source: rusqlite::Error::QueryReturnedNoRows,
        });
        assert!(matches!(txn, OrbError::DatabaseError { .. }));
    }

    #[test]
    fn test_error_mapping_imap_variants() {
        let imap = OrbError::from(OrbCoreError::Imap("bad greeting".to_string()));
        match imap {
            OrbError::ImapError { message } => assert!(message.contains("bad greeting")),
            other => panic!("expected ImapError, got {:?}", other),
        }

        let sync = OrbError::from(OrbCoreError::imap_sync_failed(
            "work", "INBOX", "denied", None,
        ));
        assert!(matches!(sync, OrbError::ImapError { .. }));

        let proto = OrbError::from(OrbCoreError::ImapProtocolError {
            details: "unexpected token".to_string(),
            source: None,
        });
        assert!(matches!(proto, OrbError::ImapError { .. }));
    }

    #[test]
    fn test_error_mapping_sync_variants() {
        let sync = OrbError::from(OrbCoreError::Sync("interrupted".to_string()));
        match sync {
            OrbError::SyncError { message } => assert!(message.contains("interrupted")),
            other => panic!("expected SyncError, got {:?}", other),
        }

        let aborted = OrbError::from(OrbCoreError::SyncAborted {
            reason: "cancelled".to_string(),
        });
        assert!(matches!(aborted, OrbError::SyncError { .. }));

        let uidvalidity = OrbError::from(OrbCoreError::UidValidityChanged {
            folder: "INBOX".to_string(),
            account: "work".to_string(),
            old: 1,
            new: 2,
        });
        assert!(matches!(uidvalidity, OrbError::SyncError { .. }));
    }

    #[test]
    fn test_error_mapping_not_found() {
        let nf = OrbError::from(OrbCoreError::NotFound("message 5".to_string()));
        match nf {
            OrbError::NotFound { message } => assert!(message.contains("message 5")),
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_error_mapping_invalid_input_is_not_a_failure() {
        // A broken rule is something to say in the status line, not a sync
        // error: "'Friday' is not a due date" is an answer, and dressing it up
        // as a failure teaches the user to distrust both.
        let invalid_input = OrbError::from(OrbCoreError::InvalidInput(
            "'Friday' is not a due date of the form YYYY-MM-DD".to_string(),
        ));
        match invalid_input {
            OrbError::InvalidInput { message } => assert!(message.contains("due date")),
            other => panic!("expected InvalidInput, got {:?}", other),
        }
    }

    #[test]
    fn test_error_mapping_fallback_is_sync_error() {
        // Variants not explicitly listed fall through to SyncError.
        let timeout = OrbError::from(OrbCoreError::Timeout {
            operation: "fetch".to_string(),
            seconds: 30,
        });
        assert!(matches!(timeout, OrbError::SyncError { .. }));
    }

    #[test]
    fn test_config_error_into_orb_error_is_config_error() {
        let err: OrbError = config::ConfigError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope",
        ))
        .into();
        match err {
            OrbError::ConfigError { message } => assert!(message.contains("nope")),
            other => panic!("expected ConfigError, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;
    use crate::config::{SubjectMungeRule, TasksConfig};
    use crate::models::Message;
    use crate::tasks::{TaskSyncState, TaskView};
    use std::collections::HashSet;

    // ------------------------------------------------------------------
    // Pure conversions
    // ------------------------------------------------------------------

    #[test]
    fn every_view_name_reaches_its_core_view() {
        // A mismatch here silently lists the wrong tasks under the right title.
        let cases = [
            (TaskViewName::Today, TaskView::Today),
            (TaskViewName::Upcoming, TaskView::Upcoming),
            (TaskViewName::Someday, TaskView::Someday),
            (TaskViewName::Completed, TaskView::Completed),
            (TaskViewName::Archive, TaskView::Archive),
            (TaskViewName::Trash, TaskView::Trash),
        ];
        for (name, expected) in cases {
            assert_eq!(TaskView::from(name).as_str(), expected.as_str());
        }
    }

    #[test]
    fn clearing_a_date_is_not_the_same_as_leaving_it_alone() {
        // The whole reason `TaskEdit` is flat: JSON cannot tell `null` from a
        // missing key, and confusing the two turns "clear the due date" into a
        // silent no-op.
        let untouched = TaskEdit::default().into_updates().expect("valid");
        assert!(
            untouched.due_date.is_none(),
            "an absent field changes nothing"
        );

        let cleared = TaskEdit {
            clear_due_date: true,
            ..Default::default()
        }
        .into_updates()
        .expect("valid");
        assert_eq!(cleared.due_date, Some(None), "clearing must reach the core");

        let set = TaskEdit {
            due_date: Some("2027-03-01".to_string()),
            ..Default::default()
        }
        .into_updates()
        .expect("valid");
        assert_eq!(set.due_date, Some(Some("2027-03-01".to_string())));
    }

    #[test]
    fn clearing_wins_over_a_value_sent_alongside_it() {
        // A UI that sends both is ambiguous; resolving it towards "clear" is the
        // reading that cannot leave a date the user asked to remove.
        let edit = TaskEdit {
            postpone_date: Some("2027-03-01".to_string()),
            clear_postpone_date: true,
            ..Default::default()
        };
        assert_eq!(edit.into_updates().unwrap().postpone_date, Some(None));
    }

    #[test]
    fn a_date_that_is_not_a_date_is_refused_rather_than_stored() {
        let edit = TaskEdit {
            due_date: Some("2026-02-31".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            edit.into_updates(),
            Err(OrbError::InvalidInput { .. })
        ));

        let nonsense = TaskEdit {
            postpone_date: Some("next tuesday".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            nonsense.into_updates(),
            Err(OrbError::InvalidInput { .. })
        ));
    }

    #[test]
    fn an_unknown_tag_id_renders_as_a_placeholder_rather_than_vanishing() {
        let registry = vec![TaskTag::new("work".to_string(), 1024.0)];
        let known = registry[0].tag_uuid.clone();

        let resolved = resolve_tags(
            &[known.clone(), "not-in-the-registry".to_string()],
            &registry,
        );
        assert_eq!(
            resolved.len(),
            2,
            "an unresolvable tag is not a deleted tag"
        );
        assert_eq!(resolved[0].name, "work");
        assert!(resolved[0].is_known);
        assert_eq!(resolved[1].name, "not-in-the-registry");
        assert!(!resolved[1].is_known);
    }

    #[test]
    fn a_task_is_never_titled_nothing() {
        assert_eq!(normalise_title("  Budget  "), "Budget");
        assert_eq!(normalise_title(""), "(no subject)");
        assert_eq!(normalise_title("   "), "(no subject)");
    }

    #[test]
    fn one_task_is_one_task_not_one_tasks() {
        assert_eq!(label("complete", 1), "complete 1 task");
        assert_eq!(label("complete", 3), "complete 3 tasks");
    }

    #[test]
    fn links_are_keyed_by_one_spelling_of_the_message_id() {
        assert_eq!(
            canonical_message_id("a@b").as_deref(),
            Some("<a@b>"),
            "a bare header and a bracketed one must produce the same link key"
        );
        assert_eq!(canonical_message_id("<a@b>").as_deref(), Some("<a@b>"));
        assert!(canonical_message_id("").is_none());
    }

    // ------------------------------------------------------------------
    // A handle with tasks configured
    // ------------------------------------------------------------------

    fn task_config(dir: &tempfile::TempDir) -> Config {
        let mut config =
            super::tests::config_with_accounts(vec![super::tests::account_with_trash(
                "Work", "Trash",
            )]);
        config.db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        config.tasks = Some(TasksConfig {
            account: "Work".to_string(),
            folder: "Tasks".to_string(),
            locale: "en".to_string(),
            keep_completed_in_today: true,
            completed_first_in_today: false,
            archive_after_days: 30,
            export: None,
        });
        // The list shows munged subjects, so a task made from a message has to
        // be titled with the same text the user was reading.
        config.subject_munges = vec![SubjectMungeRule {
            pattern: r"^\[EXT\]\s*".to_string(),
            replacement: String::new(),
            ..Default::default()
        }];
        config
    }

    /// Undo, and what the action was called.
    fn undo(orb: &OrbHandle) -> Result<Option<String>, OrbError> {
        Ok(orb.undo()?.map(|report| report.label))
    }

    /// Redo, and what the action was called.
    fn redo(orb: &OrbHandle) -> Result<Option<String>, OrbError> {
        Ok(orb.redo()?.map(|report| report.label))
    }

    fn handle(config: Config) -> Arc<OrbHandle> {
        let store = Store::new(config.clone()).expect("store");
        let health = Arc::new(HealthBook::new(
            config.accounts.iter().map(|a| a.name.clone()),
        ));
        Arc::new(OrbHandle {
            store: Arc::new(store),
            config: std::sync::RwLock::new(Arc::new(config)),
            config_path: String::new(),
            runtime: Arc::new(Runtime::new().expect("runtime")),
            push_shutdown: tokio_util::sync::CancellationToken::new(),
            scheduler: std::sync::Mutex::new(None),
            history: Default::default(),
            #[cfg(feature = "desktop")]
            editors: Default::default(),
            notification_host: std::sync::RwLock::new(None),
            health,
            last_export: std::sync::Mutex::new(None),
            task_uploads: TaskUploads::default(),
            chat: std::sync::RwLock::new(None),
            sender_checks: Default::default(),
        })
    }

    struct NoListener;

    impl TaskSyncListener for NoListener {
        fn on_task_sync(&self, _event: TaskSyncEvent) {}
    }

    /// The setup code carries the tasks account as configured and the
    /// password its command gives.
    #[test]
    #[cfg(feature = "desktop")]
    fn the_iphone_setup_code_carries_the_tasks_account_and_its_password() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        config.accounts[0].imap_password_command = Some("echo s3cret".to_string());
        let orb = handle(config.clone());

        let payload = orb.iphone_setup_payload(1_000).expect("payload");
        assert_eq!(payload.password, "s3cret");
        assert_eq!(payload.imap_server, config.accounts[0].imap_server);
        assert_eq!(payload.folder, "Tasks");
        assert_eq!(payload.expires, 1_000 + crate::setup_code::LIFETIME_SECONDS);

        let code = orb.iphone_setup_code().expect("code");
        assert!(code.svg.contains("<svg"));
    }

    /// iOS stops the uploader when the app leaves the foreground and starts it
    /// again when it comes back.
    #[test]
    fn the_task_uploader_starts_again_after_it_was_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let running = |orb: &OrbHandle| orb.task_uploads.nudge.lock().unwrap().is_some();

        orb.start_task_uploads(Box::new(NoListener));
        assert!(running(&orb));
        orb.stop_task_uploads();
        assert!(!running(&orb));
        orb.start_task_uploads(Box::new(NoListener));
        assert!(running(&orb));
    }

    /// A message read before its whole source is stored still says it has
    /// attachments, from the structure recorded for it; otherwise the reader
    /// never asks for the list. Seen with a calendar invitation opened while
    /// the background pass was still on its way to the source.
    #[test]
    fn a_message_without_its_source_still_says_it_has_attachments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let db = orb.store.writer().expect("db");
        let folder_id = db.insert_folder("Work", "INBOX").expect("folder");
        let id = db
            .insert_message(&crate::models::Message {
                id: None,
                folder_id,
                uid: 7,
                message_id: Some("<invite@example.com>".into()),
                subject: Some("Meeting".into()),
                from: Some("a@example.com".into()),
                to: Some("b@example.com".into()),
                cc: None,
                date: Some("2026-10-15T12:50:09Z".into()),
                body_text: Some("The following is a new meeting request".into()),
                body_html: None,
                rfc822: None,
                size: Some(1),
                flags: vec![],
                modseq: Some(1),
                shape: None,
                headers: None,
            })
            .expect("message");
        db.conn
            .execute(
                "UPDATE messages SET has_attachments = 1 WHERE id = ?1",
                [id],
            )
            .expect("structure says attachments");

        let detail = orb.get_message(id).expect("detail");
        assert!(detail.has_attachments);
    }

    /// A message whose text is stored opens from it without waiting for the
    /// server, saying that its HTML is still to come; asked for whole, the
    /// unreachable server leaves it as stored, and still says so.
    #[test]
    fn a_message_with_its_text_stored_opens_without_the_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let db = orb.store.writer().expect("db");
        let folder_id = db.insert_folder("Work", "INBOX").expect("folder");
        let id = db
            .insert_message(&crate::models::Message {
                id: None,
                folder_id,
                uid: 8,
                message_id: Some("<text-only@example.com>".into()),
                subject: Some("Both parts".into()),
                from: Some("a@example.com".into()),
                to: Some("b@example.com".into()),
                cc: None,
                date: Some("2026-10-15T12:50:09Z".into()),
                body_text: Some("The text part".into()),
                body_html: None,
                rfc822: None,
                size: Some(1),
                flags: vec![],
                modseq: Some(1),
                shape: None,
                headers: None,
            })
            .expect("message");
        db.conn
            .execute("UPDATE messages SET html_part = '2' WHERE id = ?1", [id])
            .expect("structure says there is HTML");

        let started = std::time::Instant::now();
        let quick = orb.get_message_stored_first(id).expect("detail");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(quick.body_text.as_deref(), Some("The text part"));
        assert!(quick.incomplete);

        let whole = orb.get_message(id).expect("detail");
        assert_eq!(whole.body_text.as_deref(), Some("The text part"));
        assert!(whole.incomplete, "the HTML could not be fetched");
    }

    /// A message's sender is checked once a session: reading it again asks
    /// neither the store's source nor the server.
    #[test]
    fn a_sender_check_is_kept_for_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let db = orb.store.writer().expect("db");
        let folder_id = db.insert_folder("Work", "INBOX").expect("folder");
        let source = b"From: Bank <help@bank.example>\r\nReply-To: x@elsewhere.example\r\n\
                       Subject: Hi\r\n\r\nBody\r\n";
        let id = db
            .insert_message(&crate::models::Message {
                id: None,
                folder_id,
                uid: 9,
                message_id: Some("<sender@example.com>".into()),
                subject: Some("Hi".into()),
                from: Some("Bank <help@bank.example>".into()),
                to: Some("b@example.com".into()),
                cc: None,
                date: Some("2026-10-15T12:50:09Z".into()),
                body_text: Some("Body".into()),
                body_html: None,
                rfc822: Some(source.to_vec()),
                size: Some(1),
                flags: vec![],
                modseq: Some(1),
                shape: None,
                headers: None,
            })
            .expect("message");

        let first = orb.sender_check(id).expect("checked from the source");
        db.conn
            .execute(
                "UPDATE message_bodies SET rfc822 = NULL WHERE message_id = ?1",
                [id],
            )
            .expect("source gone");
        let again = orb.sender_check(id).expect("kept, not fetched");
        assert_eq!(first, again);
    }

    /// A reload takes effect at once for what is read on use, keeps the old
    /// config when the file is broken, and names what needs a restart.
    #[test]
    fn a_reload_swaps_the_config_or_keeps_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yml");
        let write = |body: &str| std::fs::write(&path, body).expect("write config");
        let base = format!(
            "db_path: {}\nmark_read_after_seconds: 2\naccounts:\n  - name: Work\n    \
             imap_server: imap.example.com\n    imap_port: 993\n    imap_username: me\n    \
             from: me@example.com\n",
            dir.path().join("db.sqlite").display()
        );
        write(&base);
        let handle = init(path.to_str().unwrap().to_string()).expect("init");
        assert_eq!(handle.ui_settings().mark_read_after_ms, 2000);
        assert_eq!(handle.ui_settings().look, "hybrid");

        write(
            &format!(
                "{base}appearance:\n  look: terminal\nbindings:\n  index_normal:\n    x: noop\n"
            )
            .replace("mark_read_after_seconds: 2", "mark_read_after_seconds: 5"),
        );
        let outcome = handle.reload_config();
        assert!(outcome.error.is_none(), "{outcome:?}");
        assert!(outcome.restart_needed.is_empty());
        assert_eq!(handle.ui_settings().mark_read_after_ms, 5000);
        assert_eq!(handle.ui_settings().look, "terminal");
        assert_eq!(
            handle
                .get_bindings()
                .index_normal
                .and_then(|table| table.get("x").cloned())
                .as_deref(),
            Some("noop")
        );
        assert_eq!(handle.store.config().mark_read_after_seconds, 5.0);

        write("db_path: [not a path\n");
        let outcome = handle.reload_config();
        assert!(outcome.error.is_some());
        assert_eq!(
            handle.ui_settings().mark_read_after_ms,
            5000,
            "the old config stays"
        );

        write(&base.replace("imap_port: 993", "imap_port: 143"));
        let outcome = handle.reload_config();
        assert_eq!(outcome.restart_needed, ["accounts[Work]"]);
    }

    /// A handle whose database also holds two messages to link tasks to.
    fn handle_with_mail() -> (tempfile::TempDir, Arc<OrbHandle>, Vec<i64>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = task_config(&dir);
        let orb = handle(config.clone());

        let db = crate::db::Database::new(&config.db_path).expect("db");
        let folder = crate::db::Database::new(&config.db_path)
            .expect("db")
            .get_folder_by_name("Work", "INBOX")
            .expect("lookup");
        let folder_id = match folder {
            Some(folder) => folder.id.unwrap(),
            None => crate::db::Database::new(&config.db_path)
                .expect("db")
                .insert_folder("Work", "INBOX")
                .expect("folder"),
        };

        let mut ids = Vec::new();
        for (uid, rfc, subject) in [
            (1u32, "<budget@example.com>", "[EXT] Budget review"),
            (2, "<lunch@example.com>", "Lunch?"),
        ] {
            ids.push(
                db.insert_message(&Message {
                    id: None,
                    folder_id,
                    uid,
                    message_id: Some(rfc.to_string()),
                    subject: Some(subject.to_string()),
                    from: Some("Alice <alice@example.com>".to_string()),
                    to: None,
                    cc: None,
                    date: Some("2026-08-01T09:00:00Z".to_string()),
                    body_text: None,
                    body_html: None,
                    rfc822: None,
                    size: None,
                    flags: Vec::new(),
                    modseq: None,
                    shape: None,
                    headers: None,
                })
                .expect("insert"),
            );
        }

        (dir, orb, ids)
    }

    /// The row of a collapsed thread stands for every member: read only
    /// when all are, flagged when one is, staged when all are staged alike,
    /// marked by every rule a member matches, and showing who wrote last and
    /// what. Expanded, every row is its own message again.
    #[test]
    fn a_collapsed_thread_s_row_sums_up_its_members() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let mut config: Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nhighlights:\n  - name: Bob\n    \
             when: {{ from: '*bob@*' }}\n    style: {{ bold: true }}\n  - name: Starred\n    \
             when: {{ flagged: true }}\n    style: {{ bold: true }}\n"
        ))
        .expect("config");
        config.db_path = db_path;
        let orb = handle(config);
        let message = |uid: u32,
                       id: &str,
                       parent: Option<&str>,
                       from: &str,
                       date: &str,
                       flags: &[&str],
                       body: &str| {
            let mut m = Message::new(0, uid);
            m.message_id = Some(id.to_string());
            m.subject = Some(if parent.is_some() { "Re: Plan" } else { "Plan" }.to_string());
            m.from = Some(from.to_string());
            m.date = Some(date.to_string());
            m.flags = flags.iter().map(|f| f.to_string()).collect();
            m.body_text = Some(body.to_string());
            m.headers = Some(crate::threads::ThreadHeaders {
                in_reply_to: parent.map(str::to_string),
                ..Default::default()
            });
            m
        };
        {
            let mut db = orb.store.writer().expect("writer");
            let folder = db.get_or_create_folder("A", "INBOX").expect("folder");
            let mut messages = vec![
                message(
                    1,
                    "<a@x>",
                    None,
                    "Alice <alice@x>",
                    "2026-01-01T00:00:00Z",
                    &[],
                    "Shall we plan?",
                ),
                message(
                    2,
                    "<b@x>",
                    Some("<a@x>"),
                    "Bob <bob@x>",
                    "2026-01-02T00:00:00Z",
                    &["\\Seen", "\\Flagged"],
                    "Done, closing.",
                ),
                message(
                    3,
                    "<c@x>",
                    None,
                    "Carol <carol@x>",
                    "2026-01-03T00:00:00Z",
                    &["\\Seen"],
                    "Lunch?",
                ),
            ];
            for m in &mut messages {
                m.folder_id = folder;
            }
            db.transaction(|tx| {
                crate::db::Database::bulk_insert_messages_in_tx(tx, &messages)?;
                Ok(())
            })
            .expect("insert");
        }

        let rows = orb
            .get_threads("A".into(), "INBOX".into(), 0, 10, Vec::new())
            .expect("threads");
        assert_eq!(rows.len(), 2);
        let (lunch, plan) = (&rows[0], &rows[1]);
        assert!(lunch.thread_members.is_empty());
        assert_eq!(lunch.thread_newest_from, None);
        assert_eq!(plan.subject.as_deref(), Some("Plan"));
        assert_eq!(plan.thread_members.len(), 2);
        assert!(!plan.is_read, "one member is unread");
        assert!(plan.is_flagged, "one member is flagged");
        assert_eq!(plan.thread_newest_from.as_deref(), Some("Bob <bob@x>"));
        assert_eq!(plan.snippet.as_deref(), Some("Done, closing."));
        assert_eq!(
            plan.highlights,
            vec!["Bob".to_string(), "Starred".to_string()]
        );
        assert_eq!(plan.pending_action, None);

        // Staged alike, the row is staged; one member left out, it is not.
        orb.stage_action(
            "A".into(),
            "INBOX".into(),
            vec![1],
            PendingActionType::Delete,
            None,
            None,
        )
        .expect("stage");
        let rows = orb
            .get_threads("A".into(), "INBOX".into(), 0, 10, Vec::new())
            .expect("threads");
        assert_eq!(rows[1].pending_action, None);
        orb.stage_action(
            "A".into(),
            "INBOX".into(),
            vec![2],
            PendingActionType::Delete,
            None,
            None,
        )
        .expect("stage");
        let rows = orb
            .get_threads("A".into(), "INBOX".into(), 0, 10, Vec::new())
            .expect("threads");
        assert_eq!(rows[1].pending_action, Some(PendingActionType::Delete));

        // Expanded, the thread's own row is its first message again.
        let key = rows[1].thread_id.clone().expect("thread");
        let rows = orb
            .get_threads("A".into(), "INBOX".into(), 0, 10, vec![key.clone()])
            .expect("threads");
        assert_eq!(rows.len(), 3);
        assert!(rows[1].thread_members.is_empty());
        assert!(!rows[1].is_flagged);
        assert_eq!(rows[1].highlights, Vec::<String>::new());

        // The members, as the conversation and an action on the row read them.
        let members = orb
            .thread_members(vec![("A".into(), "INBOX".into())], vec![key])
            .expect("members");
        let uids: Vec<u32> = members.iter().map(|m| m.uid).collect();
        assert_eq!(uids, vec![1, 2]);
        assert_eq!(members[1].folder_name, "INBOX");
        assert_eq!(
            members[1].highlights,
            vec!["Bob".to_string(), "Starred".to_string()]
        );
    }

    /// A review has to show what is staged by subject, not by UID.
    #[test]
    fn staged_actions_are_listed_with_the_messages_they_name() {
        let (_dir, orb, ids) = handle_with_mail();
        orb.stage_action(
            "Work".into(),
            "INBOX".into(),
            vec![1],
            PendingActionType::Move,
            Some("Archive".into()),
            None,
        )
        .expect("stage");
        orb.stage_action(
            "Work".into(),
            "INBOX".into(),
            vec![2],
            PendingActionType::Delete,
            None,
            None,
        )
        .expect("stage");

        let mut listed = orb.list_pending_actions(PendingScope::All).expect("list");
        listed.sort_by_key(|a| a.uid);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].message_id, Some(ids[0]));
        assert_eq!(listed[0].subject.as_deref(), Some("[EXT] Budget review"));
        assert_eq!(
            listed[0].from_display.as_deref(),
            Some("Alice <alice@example.com>")
        );
        assert_eq!(listed[0].target_folder.as_deref(), Some("Archive"));
        assert_eq!(listed[1].subject.as_deref(), Some("Lunch?"));
        assert_eq!(listed[1].action_type, PendingActionType::Delete);

        assert_eq!(
            orb.discard_pending_actions(PendingScope::All, None)
                .expect("discard"),
            2
        );
        assert!(
            orb.list_pending_actions(PendingScope::All)
                .expect("list")
                .is_empty()
        );
    }

    /// Staging, unstaging and discarding are taken back like any action, and
    /// put back exactly what was staged before — a move stays a move.
    #[test]
    fn what_is_staged_is_put_back_by_undo() {
        let (_dir, orb, _) = handle_with_mail();
        let staged = |orb: &OrbHandle| {
            let mut listed = orb.list_pending_actions(PendingScope::All).unwrap();
            listed.sort_by_key(|a| a.uid);
            listed
                .into_iter()
                .map(|a| (a.uid, a.action_type, a.target_folder))
                .collect::<Vec<_>>()
        };
        orb.stage_action(
            "Work".into(),
            "INBOX".into(),
            vec![1],
            PendingActionType::Move,
            Some("Archive".into()),
            Some("stage a move".into()),
        )
        .unwrap();
        orb.stage_action(
            "Work".into(),
            "INBOX".into(),
            vec![1, 2],
            PendingActionType::Delete,
            None,
            Some("stage a delete".into()),
        )
        .unwrap();
        orb.discard_pending_actions(PendingScope::All, Some("discard".into()))
            .unwrap();
        assert!(staged(&orb).is_empty());

        assert_eq!(undo(&orb).unwrap().as_deref(), Some("discard"));
        assert_eq!(
            staged(&orb),
            [
                (1, PendingActionType::Delete, None),
                (2, PendingActionType::Delete, None)
            ]
        );
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("stage a delete"));
        assert_eq!(
            staged(&orb),
            [(1, PendingActionType::Move, Some("Archive".to_string()))]
        );
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("stage a move"));
        assert!(staged(&orb).is_empty());
        assert_eq!(redo(&orb).unwrap().as_deref(), Some("stage a move"));
        assert_eq!(
            staged(&orb),
            [(1, PendingActionType::Move, Some("Archive".to_string()))]
        );
    }

    /// Only the words a reader could see in a message are worth marking.
    #[test]
    fn highlight_terms_keep_the_visible_words_and_drop_the_rest() {
        let (_dir, orb, _) = handle_with_mail();
        let terms = orb.highlight_terms(
            "f:thomas invoice \"second quarter\" -f:noreply is:unread in:Archive ~refund".into(),
        );
        assert_eq!(terms, vec!["thomas", "invoice", "second quarter", "refund"]);
        assert!(orb.highlight_terms("is:unread".into()).is_empty());
    }

    /// Undo takes back the user's mail actions, last first, and only theirs:
    /// a message read by opening it is not an action to take back.
    #[test]
    fn mail_actions_are_undone_last_first_and_redone() {
        let (_dir, orb, _) = handle_with_mail();
        let label = |s: &str| Some(s.to_string());
        orb.mark_read(
            "Work".into(),
            "INBOX".into(),
            vec![1],
            true,
            label("read one"),
        )
        .unwrap();
        orb.mark_read(
            "Work".into(),
            "INBOX".into(),
            vec![1, 2],
            true,
            label("read both"),
        )
        .unwrap();
        orb.mark_flagged("Work".into(), "INBOX".into(), vec![2], true, None)
            .unwrap();
        assert_eq!(orb.inbox_unread_count().unwrap(), 0);

        let report = orb.undo().unwrap().unwrap();
        assert_eq!(report.label, "read both");
        assert_eq!(
            report.said, "1 message marked unread",
            "only what the action changed is put back"
        );
        assert_eq!(orb.inbox_unread_count().unwrap(), 1);
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("read one"));
        assert_eq!(orb.inbox_unread_count().unwrap(), 2);
        assert!(orb.undo().unwrap().is_none(), "the flag was never recorded");

        assert_eq!(redo(&orb).unwrap().as_deref(), Some("read one"));
        assert_eq!(orb.inbox_unread_count().unwrap(), 1);
        orb.mark_read(
            "Work".into(),
            "INBOX".into(),
            vec![2],
            true,
            label("read two"),
        )
        .unwrap();
        assert!(
            orb.redo().unwrap().is_none(),
            "a new action forgets what was undone"
        );
    }

    /// A chain run by one key is one action, however many it records.
    #[test]
    fn a_group_is_undone_by_one_undo() {
        let (_dir, orb, _) = handle_with_mail();
        let mark = orb.begin_undo_group();
        for uid in [1, 2] {
            orb.mark_read(
                "Work".into(),
                "INBOX".into(),
                vec![uid],
                true,
                Some(format!("read {uid}")),
            )
            .unwrap();
        }
        orb.end_undo_group(mark, None);
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("read 1, read 2"));
        assert_eq!(orb.inbox_unread_count().unwrap(), 2);
    }

    /// A message the reply harness can answer without the network: its text is
    /// stored, so nothing has to be fetched to quote it.
    fn stored_message(uid: u32, folder_id: i64, rfc822: Option<&str>) -> Message {
        Message {
            id: None,
            folder_id,
            uid,
            message_id: Some("<third@example.com>".to_string()),
            subject: Some("Budget".to_string()),
            from: Some("Alice <alice@example.com>".to_string()),
            to: Some("test@test.com, Bob <bob@example.com>".to_string()),
            cc: None,
            date: Some("2026-08-01T09:00:00Z".to_string()),
            body_text: Some("Numbers attached.".to_string()),
            body_html: None,
            rfc822: rfc822.map(|s| s.as_bytes().to_vec()),
            size: None,
            flags: Vec::new(),
            modseq: None,
            shape: None,
            headers: None,
        }
    }

    #[test]
    fn a_reply_threads_under_the_whole_conversation_and_honours_reply_to() {
        let (_dir, orb, _) = handle_with_mail();
        let db = crate::db::Database::new(&orb.config().db_path).expect("db");
        let folder_id = db
            .get_folder_by_name("Work", "INBOX")
            .expect("lookup")
            .and_then(|f| f.id)
            .expect("folder");
        let source = "Message-ID: <third@example.com>\r\n\
            In-Reply-To: <second@example.com>\r\n\
            References: <first@example.com> <second@example.com>\r\n\
            Reply-To: Alice at work <alice@work.example>\r\n\
            Subject: Budget\r\n\
            \r\n\
            Numbers attached.\r\n";
        let id = db
            .insert_message(&stored_message(10, folder_id, Some(source)))
            .expect("insert");

        let reply = orb.reply_draft(id, false).expect("reply");
        assert_eq!(reply.in_reply_to.as_deref(), Some("<third@example.com>"));
        assert_eq!(
            reply.references.as_deref(),
            Some("<first@example.com> <second@example.com> <third@example.com>")
        );
        assert_eq!(reply.to, vec!["Alice at work <alice@work.example>"]);
        assert!(reply.cc.is_empty());

        let everyone = orb.reply_draft(id, true).expect("reply all");
        assert_eq!(everyone.to, vec!["Alice at work <alice@work.example>"]);
        assert_eq!(
            everyone.cc,
            vec!["Alice <alice@example.com>", "Bob <bob@example.com>"],
            "the sender and the recipients are kept, the user's own address is not"
        );
    }

    /// The headers live on the server when the source is not stored, and the
    /// server is not always there. The reply still has to open.
    #[test]
    fn a_reply_without_the_headers_still_threads_under_its_parent() {
        let (_dir, orb, _) = handle_with_mail();
        let db = crate::db::Database::new(&orb.config().db_path).expect("db");
        // An account nobody configured, so the fetch fails before it can reach
        // for the network.
        let folder_id = db.insert_folder("Gone", "INBOX").expect("folder");
        let id = db
            .insert_message(&stored_message(11, folder_id, None))
            .expect("insert");

        let reply = orb.reply_draft(id, true).expect("reply");
        assert_eq!(reply.in_reply_to.as_deref(), Some("<third@example.com>"));
        assert_eq!(reply.references.as_deref(), Some("<third@example.com>"));
        assert_eq!(reply.to, vec!["Alice <alice@example.com>"]);
        assert_eq!(reply.cc, vec!["Bob <bob@example.com>"]);
    }

    /// A mailbox whose Work account also sends as support@example.com, with
    /// a message from Alice to support and Bob stored in its INBOX.
    fn handle_with_identities() -> (tempfile::TempDir, Arc<OrbHandle>, i64) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        let work = config
            .accounts
            .iter_mut()
            .find(|a| a.name == "Work")
            .expect("a Work account");
        work.from = "Me <me@example.com>".to_string();
        work.identities = vec![config::IdentityConfig {
            name: "Support".to_string(),
            from: "Example Support <support@example.com>".to_string(),
            signature: Some("The support team".to_string()),
            reply_to: Some("support@example.com".to_string()),
            addresses: vec!["help@example.com".to_string()],
            ..config::IdentityConfig::default()
        }];
        let orb = handle(config.clone());

        let db = crate::db::Database::new(&config.db_path).expect("db");
        let folder_id = match db.get_folder_by_name("Work", "INBOX").expect("lookup") {
            Some(folder) => folder.id.unwrap(),
            None => db.insert_folder("Work", "INBOX").expect("folder"),
        };
        let source = "Message-ID: <ask@example.com>\r\n\
            Delivered-To: help@example.com\r\n\
            Subject: Broken\r\n\
            \r\n\
            It does not work.\r\n";
        let id = db
            .insert_message(&Message {
                to: Some("Bob <bob@example.com>, help@example.com".to_string()),
                cc: Some("Me <me@example.com>".to_string()),
                message_id: Some("<ask@example.com>".to_string()),
                ..stored_message(20, folder_id, Some(source))
            })
            .expect("insert");
        (dir, orb, id)
    }

    #[test]
    fn a_reply_is_written_as_the_identity_the_mail_was_sent_to() {
        let (_dir, orb, id) = handle_with_identities();

        let reply = orb.reply_draft(id, true).expect("reply");
        assert_eq!(reply.identity, "Support");
        assert!(
            reply.body.trim_end().ends_with("-- \nThe support team"),
            "got: {:?}",
            reply.body
        );
        assert_eq!(
            reply.cc,
            vec!["Bob <bob@example.com>"],
            "reply-all copies neither the support address nor the account's own"
        );

        let forward = orb.forward_draft(id).expect("forward");
        assert_eq!(forward.identity, "Support");

        assert_eq!(orb.new_draft("Work".to_string()).identity, "Work");
    }

    #[test]
    fn a_send_goes_out_from_the_identity_and_refuses_an_unknown_one() {
        let (_dir, orb, _) = handle_with_identities();
        let config = orb.config();
        let work = config.accounts.iter().find(|a| a.name == "Work").unwrap();

        let mut input = super::tests::sample_send_input();
        input.identity = Some("Support".to_string());
        let mut email = outgoing_email_from_input(&input);
        apply_identity(work, &input, &mut email, true).expect("known");
        assert_eq!(
            email.from.as_deref(),
            Some("Example Support <support@example.com>")
        );
        assert_eq!(email.reply_to.as_deref(), Some("support@example.com"));

        input.identity = Some("Gone".to_string());
        let mut email = outgoing_email_from_input(&input);
        assert!(apply_identity(work, &input, &mut email, true).is_err());
        apply_identity(work, &input, &mut email, false).expect("a save falls back");
        assert_eq!(email.from, None);
    }

    #[test]
    fn a_reply_is_checked_against_where_the_original_went() {
        let (_dir, orb, id) = handle_with_identities();
        let reply = orb.reply_draft(id, false).expect("reply");
        assert_eq!(reply.delivered_to, vec!["help@example.com"]);
        assert!(reply.addressed_to.iter().any(|a| a == "help@example.com"));

        let input = DraftWarningsInput {
            account_name: "Work".into(),
            identity: Some("Support".into()),
            to: reply.to.clone(),
            body: "Fixed, see the attached log.\n".into(),
            addressed_to: reply.addressed_to.clone(),
            delivered_to: reply.delivered_to.clone(),
            ..Default::default()
        };
        let kinds: Vec<&str> = orb
            .draft_warnings(input.clone())
            .iter()
            .map(|w| w.kind)
            .collect();
        assert_eq!(kinds, ["attachment"]);

        let attached = DraftWarningsInput {
            attachment_paths: vec!["/tmp/log.txt".into()],
            ..input
        };
        assert!(orb.draft_warnings(attached).is_empty());
    }

    #[test]
    fn a_template_is_read_for_a_draft_and_its_open_placeholders_are_warned_about() {
        let (dir, orb, _) = handle_with_identities();
        let path = dir.path().join("workshop.yml");
        std::fs::write(
            &path,
            "name: Workshop\nsubject: Am {Datum}\nbody: |\n  Lieber {Vorname}\n  Am {Datum}\n",
        )
        .unwrap();
        let used = orb
            .use_template(
                path.display().to_string(),
                vec!["Jonas Keller <jonas@example.com>".into()],
            )
            .expect("read");
        assert_eq!(used.placeholders, ["Datum", "Vorname"]);
        assert_eq!(
            used.suggestions.get("Vorname").map(String::as_str),
            Some("Jonas")
        );
        assert_eq!(used.body.lines, ["Lieber {Vorname}", "Am {Datum}"]);
        assert!(orb.use_template("/nonexistent.yml".into(), vec![]).is_err());

        let input = DraftWarningsInput {
            account_name: "Work".into(),
            to: vec!["jonas@example.com".into()],
            subject: "Am Donnerstag".into(),
            body: "Lieber Jonas\nAm {Datum}\n\n> {Vorname}\n".into(),
            placeholders: used.placeholders,
            ..Default::default()
        };
        let open: Vec<(Option<String>, Option<usize>)> = orb
            .draft_warnings(input)
            .into_iter()
            .filter(|w| w.kind == "placeholder")
            .map(|w| (w.placeholder, w.line))
            .collect();
        assert_eq!(open, [(Some("Datum".to_string()), Some(2))]);
    }

    #[test]
    fn a_reopened_draft_keeps_its_identity_and_its_one_signature() {
        let (_dir, orb, _) = handle_with_identities();
        let db = crate::db::Database::new(&orb.config().db_path).expect("db");
        let folder_id = db.insert_folder("Work", "Drafts").expect("folder");
        let source = "From: Example Support <support@example.com>\r\n\
            In-Reply-To: <ask@example.com>\r\n\
            References: <ask@example.com>\r\n\
            Subject: Re: Broken\r\n\
            \r\n\
            Fixed.\r\n";
        let id = db
            .insert_message(&Message {
                from: Some("Example Support <support@example.com>".to_string()),
                body_text: Some("Fixed.\n\n-- \nThe support team\n".to_string()),
                ..stored_message(1, folder_id, Some(source))
            })
            .expect("insert");

        let draft = orb.edit_draft(id).expect("reopen");
        assert_eq!(draft.identity, "Support");
        assert_eq!(
            draft.delivered_to,
            vec!["help@example.com"],
            "the original's addresses come back with it, for the warnings"
        );
        assert_eq!(draft.in_reply_to.as_deref(), Some("<ask@example.com>"));
        assert_eq!(draft.to, vec!["test@test.com", "Bob <bob@example.com>"]);

        let same = orb
            .switch_identity(
                "Work".into(),
                "Support".into(),
                None,
                "Support".into(),
                draft.body.clone(),
            )
            .expect("switch");
        assert_eq!(same.matches("The support team").count(), 1);

        let as_me = orb
            .switch_identity(
                "Work".into(),
                "Support".into(),
                None,
                "Work".into(),
                draft.body,
            )
            .expect("switch");
        assert_eq!(
            as_me, "Fixed.\n",
            "the default identity of this account signs nothing"
        );
    }

    #[test]
    fn tasks_need_a_designated_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        config.tasks = None;
        let orb = handle(config);

        // Not a silent empty list: a task written to a database nothing will
        // ever upload is a task the user will lose (§1).
        assert!(matches!(
            orb.create_task("Buy milk".to_string(), None),
            Err(OrbError::ConfigError { .. })
        ));
        assert!(matches!(
            orb.list_tasks(TaskViewName::Today, None, None),
            Err(OrbError::ConfigError { .. })
        ));
    }

    /// The picker is useless without these two commands, and typed input is
    /// most of §12.1 — the calendar half only ever offers a day you can already
    /// see.
    #[test]
    fn the_date_picker_can_read_what_the_user_types() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));

        let today = crate::tasks::TaskRepository::new(&orb.task_db().expect("db")).today();
        let tomorrow = today.succ_opt().expect("tomorrow");

        let suggestions = orb.suggest_dates("tomorr".to_string()).expect("suggest");
        assert!(!suggestions.is_empty(), "'tomorr' has a reading");
        assert_eq!(suggestions[0].date, tomorrow.format("%Y-%m-%d").to_string());
        assert_eq!(
            suggestions[0].time, None,
            "a date-only suggestion carries no time"
        );

        // Anchored on the machine's local day, which is the day the views are
        // evaluated against — not on the WebView's idea of one.
        let same_day = orb.suggest_dates("today".to_string()).expect("suggest");
        assert_eq!(same_day[0].date, today.format("%Y-%m-%d").to_string());

        // Unparseable input is an empty list, never a guess (§12.1).
        assert!(
            orb.suggest_dates("qwertyuiop".to_string())
                .expect("suggest")
                .is_empty()
        );

        // A reminder is a day *and* a time, defaulting to 09:00 (§5.3).
        let reminder = orb
            .suggest_datetimes("tomorrow 9am".to_string())
            .expect("suggest");
        assert_eq!(reminder[0].date, tomorrow.format("%Y-%m-%d").to_string());
        assert_eq!(reminder[0].time.as_deref(), Some("09:00"));
        assert!(
            orb.suggest_datetimes("tomorrow".to_string())
                .expect("suggest")
                .iter()
                .all(|s| s.time.is_some()),
            "every reminder suggestion names a time"
        );
    }

    /// §12.1 asks for German month and weekday names. The setting is what
    /// selects them; without it `Dienstag` is "no matching date".
    #[test]
    fn the_configured_locale_reaches_the_parser() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        config.tasks.as_mut().expect("tasks").locale = "de-CH".to_string();
        let orb = handle(config);

        let german = orb.suggest_dates("morgen".to_string()).expect("suggest");
        assert!(!german.is_empty(), "'morgen' is tomorrow in German");

        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        assert!(
            orb.suggest_dates("morgen".to_string())
                .expect("suggest")
                .is_empty(),
            "and means nothing in English"
        );
    }

    /// A configuration with no `tasks` section is a mail client, not a fault.
    ///
    /// `tasks_configured` is what the interface asks before it offers a single
    /// task view, and everything else about tasks hangs off the answer. It used
    /// to be absent, so the window opened on Today, `list_tasks` failed, and the
    /// failure came out of the boot sequence: no folder, no focus, no initial
    /// sync, every pane empty. Cheap and infallible, so it can be asked before
    /// anything is drawn.
    #[test]
    fn a_config_without_tasks_reports_tasks_as_unconfigured() {
        let dir = tempfile::tempdir().expect("tempdir");

        let with = handle(task_config(&dir));
        assert!(with.tasks_configured(), "a tasks section means tasks");

        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        config.tasks = None;
        let without = handle(config);
        assert!(
            !without.tasks_configured(),
            "and its absence must be reportable rather than an error"
        );

        // The things the interface reaches for while it is still deciding what to
        // show must not be the things that fail. Mail is unaffected either way.
        assert!(
            without.get_all_folders().is_ok(),
            "the mail must work without a tasks account"
        );
        assert!(without.config_warnings().is_ok());
    }

    /// The picker must answer without a `tasks` section.
    ///
    /// Parsing a date needs a day and a language and nothing else. Routing it
    /// through the task account meant that a config with no `tasks` in it
    /// failed every keystroke in the picker, and the picker — correctly
    /// refusing to report a broken call as "no matching date" — said "could not
    /// read the date" for `tomorrow`, which parses perfectly well.
    #[test]
    fn the_date_picker_answers_without_a_tasks_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        config.tasks = None;
        let orb = handle(config);

        let suggestions = orb
            .suggest_dates("tomorrow".to_string())
            .expect("a date parses without a tasks account");
        assert!(
            suggestions.iter().any(|s| s.primary == "Tomorrow"),
            "'tomorrow' must still resolve, got {:?}",
            suggestions.iter().map(|s| &s.primary).collect::<Vec<_>>()
        );

        let reminders = orb
            .suggest_datetimes("tomorrow 9am".to_string())
            .expect("a reminder parses without a tasks account");
        assert!(!reminders.is_empty(), "reminders parse without one too");

        // And nonsense is still nonsense rather than an error, so the picker can
        // go on telling the two apart.
        assert!(
            orb.suggest_dates("qwertyuiop".to_string())
                .expect("suggest")
                .is_empty()
        );
    }

    #[test]
    fn an_undated_task_lands_in_someday_and_not_in_today() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));

        let created = orb
            .create_task("  Buy milk  ".to_string(), None)
            .expect("create");
        assert_eq!(created.task.title, "Buy milk");
        assert_eq!(created.revision, 1);
        assert!(!created.log.is_empty(), "creation is logged (§11)");

        let someday = orb.list_tasks(TaskViewName::Someday, None, None).unwrap();
        assert_eq!(someday.len(), 1);
        assert_eq!(someday[0].id, created.task.id);

        assert!(
            orb.list_tasks(TaskViewName::Today, None, None)
                .unwrap()
                .is_empty(),
            "undated tasks live in Someday, not Today (§4.1)"
        );
        assert_eq!(orb.today_task_count().unwrap(), 0);
    }

    #[test]
    fn a_task_created_with_a_postpone_date_lands_in_the_view_that_asked_for_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));

        let today = chrono::Local::now().date_naive().to_string();
        let created = orb
            .create_task("Call the plumber".to_string(), Some(today.clone()))
            .expect("create");
        assert_eq!(created.task.postpone_date.as_deref(), Some(today.as_str()));

        // The point of the whole parameter: made in Today, it is in Today, and
        // not off in Someday where the list that asked for it cannot see it.
        let today_rows = orb.list_tasks(TaskViewName::Today, None, None).unwrap();
        assert_eq!(today_rows.len(), 1);
        assert_eq!(today_rows[0].id, created.task.id);
        assert!(
            orb.list_tasks(TaskViewName::Someday, None, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(orb.today_task_count().unwrap(), 1);
    }

    #[test]
    fn a_task_cannot_be_created_with_a_date_that_does_not_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));

        let refused = orb.create_task("Nope".to_string(), Some("2026-02-31".to_string()));
        assert!(matches!(refused, Err(OrbError::InvalidInput { .. })));
        // Refused before anything was written, so there is no dateless orphan.
        assert!(
            orb.list_tasks(TaskViewName::Someday, None, None)
                .unwrap()
                .is_empty(),
            "a refused creation leaves nothing behind"
        );
    }

    #[test]
    fn a_completed_task_can_be_taken_back_with_one_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let task = orb
            .create_task("Write the report".to_string(), None)
            .unwrap();
        let id = task.task.id;

        assert_eq!(orb.complete_tasks(vec![id]).unwrap(), 1);
        assert!(orb.get_task(id).unwrap().task.is_completed);
        assert_eq!(
            orb.list_tasks(TaskViewName::Completed, None, None)
                .unwrap()
                .len(),
            1
        );

        let undone = undo(&orb).unwrap();
        assert_eq!(undone.as_deref(), Some("complete 1 task"));
        assert!(!orb.get_task(id).unwrap().task.is_completed);

        // Undo restores as a new revision rather than rewinding, so the history
        // grows (§10.3) — it never goes backwards.
        assert!(orb.get_task(id).unwrap().revision > task.revision + 1);

        assert_eq!(redo(&orb).unwrap().as_deref(), Some("complete 1 task"));
        assert!(orb.get_task(id).unwrap().task.is_completed);
    }

    /// A task's checklist is added to, ticked, renamed, moved and pruned, a
    /// step or a selection at a time, and undo takes each change back like
    /// any edit (§17).
    #[test]
    fn steps_are_edited_singly_and_as_a_selection_and_undone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb
            .create_task("Review the roadmap".to_string(), None)
            .unwrap()
            .task
            .id;
        let add = |title: &str| {
            orb.edit_task_step(
                id,
                StepEdit::Add {
                    title: title.to_string(),
                },
            )
            .unwrap()
        };
        add("Read it");
        let detail = add("Send comments");
        assert_eq!(
            detail
                .steps
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Read it", "Send comments"]
        );
        let first = detail.steps[0].id.clone();
        let detail = orb
            .edit_task_step(
                id,
                StepEdit::Done {
                    ids: vec![first.clone()],
                    done: true,
                },
            )
            .unwrap();
        assert_eq!((detail.task.steps_done, detail.task.steps_total), (1, 2));
        assert!(
            orb.edit_task_step(
                id,
                StepEdit::Add {
                    title: "  ".to_string()
                }
            )
            .is_err(),
            "a step needs a title"
        );

        undo(&orb).unwrap();
        assert_eq!(orb.get_task(id).unwrap().task.steps_done, 0);

        let detail = add("Book the room");
        let last = detail.steps[2].id.clone();
        let detail = orb
            .edit_task_step(
                id,
                StepEdit::Move {
                    ids: vec![last],
                    to: 0,
                },
            )
            .unwrap();
        assert_eq!(
            detail
                .steps
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Book the room", "Read it", "Send comments"]
        );
        let detail = add("Print it");

        // A selection apart moves as one block, in checklist order, and is
        // ticked as one edit that one undo takes back.
        let apart = vec![detail.steps[3].id.clone(), detail.steps[0].id.clone()];
        let detail = orb
            .edit_task_step(
                id,
                StepEdit::Move {
                    ids: apart.clone(),
                    to: 1,
                },
            )
            .unwrap();
        assert_eq!(
            detail
                .steps
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Read it", "Book the room", "Print it", "Send comments"]
        );
        let detail = orb
            .edit_task_step(
                id,
                StepEdit::Done {
                    ids: apart,
                    done: true,
                },
            )
            .unwrap();
        assert_eq!((detail.task.steps_done, detail.task.steps_total), (2, 4));
        undo(&orb).unwrap();
        let detail = orb.get_task(id).unwrap();
        assert_eq!((detail.task.steps_done, detail.task.steps_total), (0, 4));
        let detail = orb
            .edit_task_step(
                id,
                StepEdit::Remove {
                    ids: vec![first, detail.steps[3].id.clone()],
                },
            )
            .unwrap();
        assert_eq!(detail.steps.len(), 2);
    }

    /// The working-on mark is local state: several tasks can carry it, it
    /// leaves a task that is completed, and it never bumps a revision (§19).
    #[test]
    fn working_on_marks_tasks_without_a_revision_and_leaves_on_completion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let a = orb
            .create_task("Write the report".to_string(), None)
            .unwrap();
        let b = orb
            .create_task("Wait for the numbers".to_string(), None)
            .unwrap();
        let mark = TaskEdit {
            is_working_on: Some(true),
            ..TaskEdit::default()
        };
        orb.update_tasks(vec![a.task.id, b.task.id], mark).unwrap();
        let working: Vec<String> = orb
            .working_on_tasks()
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        // Newest first: a new task goes on top.
        assert_eq!(working, ["Wait for the numbers", "Write the report"]);
        assert_eq!(orb.get_task(a.task.id).unwrap().revision, a.revision);

        orb.complete_tasks(vec![a.task.id]).unwrap();
        assert_eq!(orb.working_on_tasks().unwrap().len(), 1);
        assert!(!orb.get_task(a.task.id).unwrap().task.is_working_on);

        assert_eq!(orb.clear_working_on().unwrap(), 1);
        assert!(orb.working_on_tasks().unwrap().is_empty());
    }

    /// Every task, trashed and completed ones included, goes into a snapshot
    /// that reads back (todo-spec §20).
    #[test]
    fn an_export_holds_every_task_and_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let open = orb.create_task("Open".to_string(), None).unwrap().task.id;
        let done = orb.create_task("Done".to_string(), None).unwrap().task.id;
        let gone = orb.create_task("Gone".to_string(), None).unwrap().task.id;
        orb.complete_tasks(vec![done]).unwrap();
        orb.trash_tasks(vec![gone]).unwrap();
        orb.edit_task_step(
            open,
            StepEdit::Add {
                title: "First".to_string(),
            },
        )
        .unwrap();

        let out = dir.path().join("exports");
        let result = orb.export_tasks_to(&out, 30).unwrap();
        assert!(result.complete);
        assert_eq!(result.tasks, 3);
        let snapshot = crate::tasks::export::verify(std::path::Path::new(&result.path)).unwrap();
        let titles: Vec<&str> = snapshot.tasks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles.len(), 3);
        for title in ["Open", "Done", "Gone"] {
            assert!(titles.contains(&title), "{title}");
        }
        let task = |title: &str| snapshot.tasks.iter().find(|t| t.title == title).unwrap();
        assert!(task("Done").completed_at.is_some());
        assert!(task("Gone").deleted_at.is_some());
        assert_eq!(task("Open").steps[0].title, "First");
        assert_eq!(snapshot.account, "Work");
    }

    /// An export is due until a complete snapshot is younger than the
    /// interval, and the configured one goes where the config says (§20.3).
    #[test]
    fn a_configured_export_is_due_until_one_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        let out = dir.path().join("exports");
        config.tasks.as_mut().unwrap().export = Some(crate::config::TaskExportConfig {
            directory: out.to_string_lossy().into_owned(),
            interval_hours: 24.0,
            keep: 3,
        });
        let orb = handle(config);
        orb.create_task("Something".to_string(), None).unwrap();
        let before = orb.task_export_status();
        assert!(before.configured && before.due && before.overdue);
        assert!(before.last_complete.is_none());

        let result = orb.export_tasks_if_due().expect("due").unwrap();
        assert!(result.path.starts_with(&*out.to_string_lossy()));
        let after = orb.task_export_status();
        assert!(!after.due && !after.overdue);
        assert!(after.last_complete.is_some());
        assert!(after.problem.is_none());
        assert!(orb.export_tasks_if_due().is_none());
    }

    /// An export from one machine imports into an empty one by task id: new
    /// tasks at their ids and above the exported revision, a purged one left
    /// out unless asked, a second import only merging logs, a newer revision
    /// replacing (todo-spec §21).
    #[test]
    fn an_export_imports_into_another_machine_by_task_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = handle(task_config(&dir));
        let open = a.create_task("Open".to_string(), None).unwrap();
        let open_id = open.task.id;
        let gone = a.create_task("Gone".to_string(), None).unwrap().task.id;
        a.trash_tasks(vec![gone]).unwrap();
        let exports = dir.path().join("exports");
        let first = a.export_tasks_to(&exports, 30).unwrap().path;

        let other = tempfile::tempdir().expect("tempdir");
        let b = handle(task_config(&other));
        let dry = b.import_tasks(first.clone(), false, false).unwrap();
        assert!(!dry.applied);
        assert_eq!((dry.new, dry.newer, dry.older), (1, 0, 0));
        assert_eq!(dry.purged, ["Gone"]);
        assert!(b.working_on_tasks().unwrap().is_empty());
        assert!(
            b.list_tasks(TaskViewName::Someday, None, None)
                .unwrap()
                .is_empty(),
            "a dry run writes nothing"
        );

        b.import_tasks(first.clone(), true, false).unwrap();
        let copy = b.list_tasks(TaskViewName::Someday, None, None).unwrap();
        assert_eq!(copy.len(), 1);
        assert_eq!(copy[0].uuid, open.task.uuid);
        assert!(copy[0].is_dirty, "imported to be uploaded");
        let detail = b.get_task(copy[0].id).unwrap();
        assert!(detail.revision > open.revision);
        assert!(!detail.log.is_empty());

        // Again: as new here as in the snapshot, so only the log is merged.
        let again = b.import_tasks(first.clone(), true, false).unwrap();
        assert_eq!((again.new, again.newer, again.older), (0, 0, 1));

        // A newer revision from the first machine replaces the copy.
        let renamed = TaskEdit {
            title: Some("Open, renamed".to_string()),
            ..TaskEdit::default()
        };
        for _ in 0..3 {
            a.update_task(open_id, renamed.clone()).unwrap();
            a.update_task(
                open_id,
                TaskEdit {
                    title: Some("Open".to_string()),
                    ..TaskEdit::default()
                },
            )
            .unwrap();
        }
        a.update_task(open_id, renamed).unwrap();
        let second = a.export_tasks_to(&exports, 30).unwrap().path;
        let newer = b.import_tasks(second.clone(), true, false).unwrap();
        assert_eq!(newer.newer, 1);
        assert_eq!(b.get_task(copy[0].id).unwrap().task.title, "Open, renamed");

        // Asked to, the purged one comes back, in the trash as exported.
        let all = b.import_tasks(second, true, true).unwrap();
        assert!(all.purged.is_empty());
        assert_eq!(
            b.list_tasks(TaskViewName::Trash, None, None).unwrap().len(),
            1
        );
    }

    /// Export, import into an empty store, export again: the same tasks.json
    /// but for when, where and at what revision (§21) — the import raises
    /// every revision on purpose, so that the copy wins everywhere.
    #[test]
    fn a_chat_task_is_titled_by_the_first_line_of_its_message() {
        assert_eq!(
            chat_task_title("[ ](https://chat/x?msg=1) Can you look?\nIt fails."),
            "Can you look?"
        );
        let long = "word ".repeat(30);
        let title = chat_task_title(&long);
        assert!(title.ends_with("word…"), "{title}");
        assert!(title.chars().count() <= 81);
    }

    #[test]
    fn an_export_survives_a_round_trip_through_an_empty_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = handle(task_config(&dir));
        let open = a.create_task("Open".to_string(), None).unwrap().task.id;
        a.edit_task_step(
            open,
            StepEdit::Add {
                title: "First".to_string(),
            },
        )
        .unwrap();
        let done = a.create_task("Done".to_string(), None).unwrap().task.id;
        a.complete_tasks(vec![done]).unwrap();
        // A task made from a chat message keeps its link (todo-spec §28).
        a.with_tasks(|_, repo, ctx| {
            repo.link_email(
                open,
                "chat://chat.example.com/r1/m1",
                &CachedEmailMetadata {
                    subject: Some("Deploy at four?".to_string()),
                    account: Some("#dev".to_string()),
                    ..CachedEmailMetadata::default()
                },
                ctx,
            )?;
            Ok(())
        })
        .unwrap();
        let first = a.export_tasks_to(&dir.path().join("a"), 30).unwrap().path;

        let other = tempfile::tempdir().expect("tempdir");
        let b = handle(task_config(&other));
        b.import_tasks(first.clone(), true, true).unwrap();
        let second = b.export_tasks_to(&other.path().join("b"), 30).unwrap().path;

        let normalised = |path: &str| {
            let mut json: serde_json::Value = serde_json::from_slice(
                &std::fs::read(std::path::Path::new(path).join("tasks.json")).unwrap(),
            )
            .unwrap();
            for key in ["exported_at", "device_id"] {
                json[key] = serde_json::Value::Null;
            }
            for task in json["tasks"].as_array_mut().unwrap() {
                task["revision"] = serde_json::Value::Null;
            }
            json
        };
        assert_eq!(normalised(&first), normalised(&second));
        let links: Vec<_> = normalised(&second)["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| t["links"].as_array().unwrap().clone())
            .collect();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0]["kind"], "chat");
        assert_eq!(links[0]["chat"]["room_id"], "r1");
    }

    /// A task opens the address on the first line of its notes first, then
    /// one in its title, then what a link rule makes of its title.
    #[test]
    fn a_task_points_to_its_notes_its_title_and_its_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        config.message_links = vec![crate::config::MessageLinkRule {
            name: Some("issue".to_string()),
            pattern: r"Issue #(\d+)".to_string(),
            url: "https://tracker.example.com/go/$1".to_string(),
            ..Default::default()
        }];
        let orb = handle(config);
        let id = orb
            .create_task("Issue #7, see https://example.com/title".to_string(), None)
            .unwrap()
            .task
            .id;
        orb.update_task(
            id,
            TaskEdit {
                body: Some("\nhttps://example.com/notes.\nhttps://example.com/later".to_string()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        let urls: Vec<String> = orb
            .task_links(id)
            .unwrap()
            .into_iter()
            .map(|link| link.url)
            .collect();
        assert_eq!(
            urls,
            [
                "https://example.com/notes",
                "https://example.com/title",
                "https://tracker.example.com/go/7"
            ]
        );
    }

    /// A task made from a message opens first what the message would: the
    /// rules read the message's subject as it arrived, not the task's title.
    #[test]
    fn a_task_points_first_to_what_its_linked_message_points_to() {
        let (_dir, orb, message_ids) = handle_with_mail();
        let mut config = (*orb.config()).clone();
        config.message_links = vec![crate::config::MessageLinkRule {
            name: None,
            pattern: r"^\[EXT\] (\w+)".to_string(),
            url: "https://tracker.example.com/$1".to_string(),
            ..Default::default()
        }];
        *orb.config.write().unwrap() = Arc::new(config);
        let task = orb
            .create_tasks_from_messages(vec![message_ids[0]], None, None)
            .unwrap()
            .remove(0)
            .task;
        assert_eq!(task.title, "Budget review");
        orb.update_task(
            task.id,
            TaskEdit {
                body: Some("https://example.com/notes".to_string()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        let urls: Vec<String> = orb
            .task_links(task.id)
            .unwrap()
            .into_iter()
            .map(|link| link.url)
            .collect();
        assert_eq!(
            urls,
            [
                "https://tracker.example.com/Budget",
                "https://example.com/notes"
            ]
        );
    }

    /// Things comes over after a dry run that writes nothing, with its tags,
    /// and a second import adds nothing (todo-spec §22).
    #[test]
    fn things_imports_once_after_a_dry_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let things = dir.path().join("things.sqlite");
        crate::tasks::things::tests::fixture(&things);
        let path = Some(things.to_string_lossy().into_owned());

        let dry = orb.import_things(path.clone(), false).unwrap();
        assert_eq!(
            (dry.tasks, dry.completed, dry.repeating, dry.already),
            (5, 3, 1, 0)
        );
        assert_eq!(
            (
                dry.trashed,
                dry.projects,
                dry.headings,
                dry.repeat_instances
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(dry.new_tags, ["Garden", "Home", "errand"]);
        assert!(
            orb.list_tasks(TaskViewName::Today, None, None)
                .unwrap()
                .is_empty()
        );

        let done = orb.import_things(path.clone(), true).unwrap();
        assert_eq!(done.tasks, 5);
        let everywhere: Vec<TaskSummary> = [
            TaskViewName::Today,
            TaskViewName::Upcoming,
            TaskViewName::Someday,
        ]
        .into_iter()
        .flat_map(|view| orb.list_tasks(view, None, None).unwrap())
        .collect();
        let seeds = everywhere
            .iter()
            .find(|t| t.title == "Buy seeds")
            .expect("imported");
        let names: Vec<&str> = seeds.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert_eq!(seeds.steps_total, 2);
        assert!(seeds.is_dirty, "to be uploaded");

        let again = orb.import_things(path, true).unwrap();
        assert_eq!((again.tasks, again.already), (0, 5));
        assert!(again.new_tags.is_empty());
    }

    /// Importing again brings over what changed in Things, except into a task
    /// edited in Orb since, and trashes what Things trashed (todo-spec §22).
    #[test]
    fn things_imports_again_with_the_changes_made_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let things = dir.path().join("things.sqlite");
        crate::tasks::things::tests::fixture(&things);
        let path = Some(things.to_string_lossy().into_owned());
        orb.import_things(path.clone(), true).unwrap();
        let find = |title: &str| {
            orb.with_tasks(|db, _, _| {
                Ok(db
                    .conn
                    .query_row("SELECT id FROM tasks WHERE subject = ?1", [title], |row| {
                        row.get::<_, i64>(0)
                    })
                    .map_err(crate::Error::from)?)
            })
            .unwrap()
        };
        let mowed = find("Mow the lawn");
        orb.uncomplete_tasks(vec![mowed]).unwrap();

        let conn = rusqlite::Connection::open(&things).unwrap();
        conn.execute_batch(
            "UPDATE TMTask SET title = 'Buy seeds and bulbs' WHERE uuid = 'open';
             UPDATE TMTask SET title = 'Mow the lawn twice' WHERE uuid = 'done';
             UPDATE TMTask SET trashed = 1 WHERE uuid = 'cancelled';",
        )
        .unwrap();
        drop(conn);

        let plan = orb.import_things(path.clone(), false).unwrap();
        assert_eq!(
            (plan.tasks, plan.updated, plan.trashed_here, plan.already),
            (0, 1, 1, 2)
        );
        assert_eq!(plan.edited_here, ["Mow the lawn"]);
        orb.import_things(path.clone(), true).unwrap();

        let seeds = orb.get_task(find("Buy seeds and bulbs")).unwrap();
        assert_eq!(seeds.task.title, "Buy seeds and bulbs");
        let created = seeds
            .log
            .iter()
            .filter(|entry| entry.text == "Created in Things")
            .count();
        assert_eq!(created, 1, "no second copy of the history");
        assert_eq!(orb.get_task(mowed).unwrap().task.title, "Mow the lawn");
        let gardener = orb.get_task(find("Hire a gardener")).unwrap();
        assert!(gardener.task.deleted_at.is_some());

        let again = orb.import_things(path, false).unwrap();
        assert_eq!(
            (again.updated, again.trashed_here, again.already),
            (0, 0, 3)
        );
    }

    /// A mail the notes of a Things to-do link to by `message://` URL comes
    /// over as a linked message (todo-spec §22).
    #[test]
    fn things_notes_link_their_mail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let things = dir.path().join("things.sqlite");
        crate::tasks::things::tests::fixture(&things);
        let conn = rusqlite::Connection::open(&things).unwrap();
        conn.execute(
            "UPDATE TMTask SET notes = 'From message://%3cseeds@shop.example%3e' \
             WHERE uuid = 'open'",
            [],
        )
        .unwrap();
        drop(conn);
        let path = Some(things.to_string_lossy().into_owned());
        orb.import_things(path.clone(), true).unwrap();

        let id = orb
            .with_tasks(|db, _, _| {
                Ok(db
                    .conn
                    .query_row(
                        "SELECT id FROM tasks WHERE subject = 'Buy seeds'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(crate::Error::from)?)
            })
            .unwrap();
        let links: Vec<String> = orb
            .get_task(id)
            .unwrap()
            .links
            .into_iter()
            .map(|link| link.message_id)
            .collect();
        assert_eq!(links, ["<seeds@shop.example>"]);

        let again = orb.import_things(path, false).unwrap();
        assert_eq!(again.already, 5);
    }

    /// `link-mail-in-notes` links what a task's notes name by `message://`
    /// URL, once, and is undone as one step.
    #[test]
    fn mail_named_in_notes_is_linked_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb.create_task("Answer".to_string(), None).unwrap().task.id;
        orb.update_task(
            id,
            TaskEdit {
                body: Some(
                    "message://%3ca@example.com%3e and message://%3cb@example.com%3e".to_string(),
                ),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        orb.create_task("Unrelated".to_string(), None).unwrap();

        assert_eq!(orb.link_mail_in_notes().unwrap(), 1);
        let links: Vec<String> = orb
            .get_task(id)
            .unwrap()
            .links
            .into_iter()
            .map(|link| link.message_id)
            .collect();
        assert_eq!(links.len(), 2);
        assert!(links.contains(&"<a@example.com>".to_string()));
        assert_eq!(orb.link_mail_in_notes().unwrap(), 0, "nothing new");

        orb.undo().unwrap();
        assert!(orb.get_task(id).unwrap().links.is_empty());
    }

    /// A tag deleted from every task comes back on every task, under its
    /// name and colour; a rename and a creation are taken back too.
    #[test]
    fn tag_changes_are_undone_tasks_and_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb.create_task("Shop".to_string(), None).unwrap().task.id;
        let tag = orb
            .create_task_tag("errands".to_string(), Some("#e06060".to_string()))
            .unwrap();
        orb.add_task_tag(vec![id], tag.id.clone()).unwrap();
        orb.update_task_tag(
            tag.id.clone(),
            "chores".to_string(),
            Some("#e06060".to_string()),
        )
        .unwrap();
        orb.delete_task_tag(tag.id.clone()).unwrap();
        assert!(orb.list_task_tags().unwrap().is_empty());
        assert!(orb.get_task(id).unwrap().task.tags.is_empty());

        assert_eq!(undo(&orb).unwrap().as_deref(), Some("delete tag chores"));
        let tags = orb.list_task_tags().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(
            (tags[0].name.as_str(), tags[0].colour.as_deref()),
            ("chores", Some("#e06060"))
        );
        let on_task = orb.get_task(id).unwrap().task.tags;
        assert_eq!(on_task.len(), 1, "the task has it again");
        assert_eq!(on_task[0].id, tag.id);

        assert_eq!(
            undo(&orb).unwrap().as_deref(),
            Some("rename tag errands to chores")
        );
        assert_eq!(orb.list_task_tags().unwrap()[0].name, "errands");
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("tag 1 task"));
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("create tag errands"));
        assert!(orb.list_task_tags().unwrap().is_empty());
        assert_eq!(redo(&orb).unwrap().as_deref(), Some("create tag errands"));
        assert_eq!(orb.list_task_tags().unwrap()[0].id, tag.id);
    }

    /// Reminders, links and the working-on mark are put back as they were.
    #[test]
    fn reminders_links_and_working_on_are_undone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb.create_task("Call".to_string(), None).unwrap().task.id;

        orb.add_task_reminder(id, "2030-01-02T09:00:00+01:00".to_string())
            .unwrap();
        let reminder = orb
            .add_task_reminder(id, "2030-01-03T09:00:00+01:00".to_string())
            .unwrap();
        orb.delete_task_reminder(reminder).unwrap();
        assert_eq!(orb.list_task_reminders(id).unwrap().len(), 1);
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("remove a reminder"));
        assert_eq!(orb.list_task_reminders(id).unwrap().len(), 2);
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("add a reminder"));
        let left = orb.list_task_reminders(id).unwrap();
        assert_eq!(left.len(), 1);
        assert!(left[0].remind_at.starts_with("2030-01-02"));

        orb.with_tasks(|_, repo, ctx| {
            repo.link_email(
                id,
                "<a@example.com>",
                &CachedEmailMetadata {
                    subject: Some("Quote".to_string()),
                    ..CachedEmailMetadata::default()
                },
                ctx,
            )?;
            Ok(())
        })
        .unwrap();
        assert!(
            orb.unlink_message_from_task(id, "<a@example.com>".to_string())
                .unwrap()
        );
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("unlink a message"));
        let links = orb.get_task(id).unwrap().links;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].subject.as_deref(), Some("Quote"));

        orb.update_tasks(
            vec![id],
            TaskEdit {
                is_working_on: Some(true),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        assert_eq!(orb.clear_working_on().unwrap(), 1);
        assert_eq!(
            undo(&orb).unwrap().as_deref(),
            Some("stop working on 1 task")
        );
        assert!(orb.get_task(id).unwrap().task.is_working_on);
    }

    #[test]
    fn undoing_a_creation_trashes_the_task_rather_than_erasing_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb.create_task("Oops".to_string(), None).unwrap().task.id;

        assert_eq!(undo(&orb).unwrap().as_deref(), Some("create task"));
        // A hard delete would leave the already-appended message on the server
        // to be downloaded back into existence; a soft one syncs as a deletion.
        assert_eq!(
            orb.list_tasks(TaskViewName::Trash, None, None)
                .unwrap()
                .len(),
            1
        );
        assert!(orb.get_task(id).unwrap().task.deleted_at.is_some());
    }

    #[test]
    fn nothing_to_undo_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        assert!(undo(&orb).unwrap().is_none());
        assert!(redo(&orb).unwrap().is_none());
    }

    /// A postpone date after the due date is kept, and the due date decides:
    /// due today, the task stays in Today; due later, it is in Upcoming (§2).
    #[test]
    fn a_postpone_date_after_the_due_date_is_kept_and_the_due_date_decides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let today = chrono::Local::now().date_naive();
        let day = |days: i64| (today + chrono::Duration::days(days)).to_string();
        let in_view = |view: TaskViewName, id: i64| {
            orb.list_tasks(view, None, None)
                .unwrap()
                .iter()
                .any(|task| task.id == id)
        };
        let id = orb
            .create_task("Ship it".to_string(), None)
            .unwrap()
            .task
            .id;

        orb.set_task_due_date(vec![id], Some(day(0))).unwrap();
        orb.set_task_postpone_date(vec![id], Some(day(5))).unwrap();
        let task = orb.get_task(id).unwrap().task;
        assert_eq!(task.postpone_date, Some(day(5)));
        assert_eq!(task.due_date, Some(day(0)));
        assert!(in_view(TaskViewName::Today, id), "due today");

        orb.set_task_due_date(vec![id], Some(day(2))).unwrap();
        assert_eq!(orb.get_task(id).unwrap().task.postpone_date, Some(day(5)));
        assert!(!in_view(TaskViewName::Today, id));
        assert!(in_view(TaskViewName::Upcoming, id));
    }

    #[test]
    fn a_bulk_action_is_one_undo_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let ids: Vec<i64> = (0..3)
            .map(|n| orb.create_task(format!("Task {n}"), None).unwrap().task.id)
            .collect();

        assert_eq!(orb.complete_tasks(ids.clone()).unwrap(), 3);
        assert_eq!(undo(&orb).unwrap().as_deref(), Some("complete 3 tasks"));
        for id in ids {
            assert!(!orb.get_task(id).unwrap().task.is_completed);
        }
    }

    #[test]
    fn merging_keeps_one_task_with_everything_and_one_undo_takes_it_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let keep = orb
            .create_task("Plan the trip".into(), Some("2026-12-10".into()))
            .unwrap()
            .task
            .id;
        let other = orb
            .create_task("Book flights".into(), None)
            .unwrap()
            .task
            .id;
        let tag = orb.create_task_tag("travel".into(), None).unwrap();
        orb.add_task_tag(vec![other], tag.id.clone()).unwrap();
        orb.update_task(
            other,
            TaskEdit {
                body: Some("Window seat".into()),
                due_date: Some("2026-12-01".into()),
                ..Default::default()
            },
        )
        .unwrap();
        orb.edit_task_step(
            keep,
            StepEdit::Add {
                title: "Pick dates".into(),
            },
        )
        .unwrap();
        orb.edit_task_step(
            other,
            StepEdit::Add {
                title: "Compare prices".into(),
            },
        )
        .unwrap();
        orb.add_task_reminder(other, "2099-01-01T09:00:00+01:00".into())
            .unwrap();
        let file = dir.path().join("tickets.pdf");
        std::fs::write(&file, b"%PDF").unwrap();
        orb.add_task_attachment(other, file.to_string_lossy().into_owned())
            .unwrap();

        let plan = orb.plan_task_merge(keep, vec![other]).unwrap();
        assert_eq!(plan.body.as_deref(), Some("Book flights\nWindow seat"));
        assert_eq!(plan.due_date.as_deref(), Some("2026-12-01"));
        assert_eq!(plan.postpone_date.as_deref(), Some("2026-12-10"));
        assert_eq!((plan.steps, plan.tags, plan.reminders), (2, 1, 1));

        let merged = orb
            .merge_tasks(
                keep,
                vec![other],
                "Plan the trip to Rome".into(),
                plan.body.clone(),
                plan.postpone_date.clone(),
                plan.due_date.clone(),
            )
            .unwrap();
        assert_eq!(merged.task.title, "Plan the trip to Rome");
        assert_eq!(merged.body.as_deref(), Some("Book flights\nWindow seat"));
        assert_eq!(merged.task.due_date.as_deref(), Some("2026-12-01"));
        let steps: Vec<&str> = merged.steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(steps, ["Pick dates", "Compare prices"]);
        assert_eq!(merged.task.tags.len(), 1);
        assert_eq!(merged.reminders.len(), 1);
        assert_eq!(merged.attachments.len(), 1);
        assert_eq!(merged.attachments[0].filename, "tickets.pdf");
        assert!(orb.get_task(other).unwrap().task.deleted_at.is_some());

        assert_eq!(undo(&orb).unwrap().as_deref(), Some("merge 2 tasks"));
        let kept = orb.get_task(keep).unwrap();
        assert_eq!(kept.task.title, "Plan the trip");
        assert_eq!(kept.steps.len(), 1);
        assert!(kept.reminders.is_empty());
        assert!(kept.attachments.is_empty());
        assert!(kept.task.tags.is_empty());
        assert!(orb.get_task(other).unwrap().task.deleted_at.is_none());
    }

    #[test]
    fn the_filter_keeps_to_its_view_and_the_search_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        orb.create_task("Renew the passport".to_string(), None)
            .unwrap();
        let other = orb
            .create_task("Book the flights".to_string(), None)
            .unwrap();

        let hits = orb
            .list_tasks(TaskViewName::Someday, Some("passport".to_string()), None)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Renew the passport");

        // The filter never leaves the current view (§4.5); the search does.
        orb.complete_tasks(vec![other.task.id]).unwrap();
        assert!(
            orb.list_tasks(TaskViewName::Someday, Some("flights".to_string()), None)
                .unwrap()
                .is_empty()
        );
        let found = orb.search_tasks("flights".to_string()).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].is_completed);
        assert!(
            orb.search_tasks("f:flights".to_string())
                .unwrap()
                .is_empty()
        );
        assert!(
            orb.search_tasks("\"unclosed".to_string())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn tags_can_be_managed_and_deleting_one_keeps_the_tasks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb
            .create_task("Draft the proposal".to_string(), None)
            .unwrap()
            .task
            .id;

        let tag = orb
            .create_task_tag("work".to_string(), Some("#ff8800".to_string()))
            .unwrap();
        assert_eq!(orb.add_task_tag(vec![id], tag.id.clone()).unwrap(), 1);
        assert_eq!(orb.get_task(id).unwrap().task.tags[0].name, "work");

        // Renaming rewrites the registry, not the tasks: they carry ids.
        assert!(
            orb.update_task_tag(tag.id.clone(), "office".to_string(), None)
                .unwrap()
        );
        assert_eq!(orb.get_task(id).unwrap().task.tags[0].name, "office");

        assert_eq!(orb.delete_task_tag(tag.id.clone()).unwrap(), 1);
        let task = orb
            .get_task(id)
            .expect("removing a tag never removes the task");
        assert!(task.task.tags.is_empty());
    }

    #[test]
    fn a_tag_needs_a_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        assert!(matches!(
            orb.create_task_tag("   ".to_string(), None),
            Err(OrbError::InvalidInput { .. })
        ));
    }

    #[test]
    fn a_reminder_survives_being_set_offline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb
            .create_task("Call the dentist".to_string(), None)
            .unwrap()
            .task
            .id;

        // Nothing here reaches the network: §3.6 requires a task created offline
        // to be fully usable, reminders included.
        let reminder = orb
            .add_task_reminder(id, "2027-05-01T09:00:00+02:00".to_string())
            .expect("reminder");

        let reminders = orb.list_task_reminders(id).unwrap();
        assert_eq!(reminders.len(), 1);
        assert!(!reminders[0].is_triggered);
        assert!(
            reminders[0].notification_id.starts_with("orb.task."),
            "the OS identifier has to be derivable from the task, not the row"
        );

        orb.delete_task_reminder(reminder).unwrap();
        assert!(orb.list_task_reminders(id).unwrap().is_empty());
    }

    #[test]
    fn an_attachment_is_cached_locally_the_moment_it_is_added() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb
            .create_task("Send the contract".to_string(), None)
            .unwrap()
            .task
            .id;

        let source = dir.path().join("contract.pdf");
        std::fs::write(&source, b"a contract").unwrap();

        let info = orb
            .add_task_attachment(id, source.to_string_lossy().to_string())
            .expect("attach");
        assert_eq!(info.filename, "contract.pdf");
        assert_eq!(info.content_type.as_deref(), Some("application/pdf"));
        assert_eq!(info.size, Some(10));
        assert!(info.is_cached);

        // Reading it back needs no network, because the payload never left.
        let data = orb
            .task_attachment_data(id, info.attachment_uuid.clone())
            .expect("read back");
        assert_eq!(data.data, b"a contract");

        assert_eq!(orb.list_task_attachments(id).unwrap().len(), 1);
        orb.delete_task_attachment(info.id).unwrap();
        assert!(orb.list_task_attachments(id).unwrap().is_empty());
    }

    #[test]
    fn attaching_a_file_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orb = handle(task_config(&dir));
        let id = orb
            .create_task("Send it".to_string(), None)
            .unwrap()
            .task
            .id;
        assert!(matches!(
            orb.add_task_attachment(id, "/no/such/file.pdf".to_string()),
            Err(OrbError::InvalidInput { .. })
        ));
    }

    // ------------------------------------------------------------------
    // Email integration (§6)
    // ------------------------------------------------------------------

    /// A task made from a message lands in Today, and taking it back is one
    /// undo, not one for the task and another for its date.
    #[test]
    fn a_task_made_from_a_message_can_be_put_in_today_in_one_step() {
        let (_dir, orb, message_ids) = handle_with_mail();
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();

        let created = orb
            .create_tasks_from_messages(vec![message_ids[0]], Some(today.clone()), None)
            .expect("create from message");
        assert_eq!(
            created[0].task.postpone_date.as_deref(),
            Some(today.as_str())
        );
        assert_eq!(created[0].links.len(), 1);
        assert_eq!(
            orb.list_tasks(TaskViewName::Today, None, None)
                .unwrap()
                .len(),
            1
        );

        undo(&orb).unwrap();
        assert!(
            orb.list_tasks(TaskViewName::Today, None, None)
                .unwrap()
                .is_empty()
        );
        assert!(undo(&orb).unwrap().is_none());
    }

    /// A task made from a message can be given a due date instead of a
    /// postpone date, and taking it back is still one undo.
    #[test]
    fn a_task_made_from_a_message_can_be_given_a_due_date_in_one_step() {
        let (_dir, orb, message_ids) = handle_with_mail();

        let created = orb
            .create_tasks_from_messages(vec![message_ids[0]], None, Some("2031-03-14".into()))
            .expect("create from message");
        assert_eq!(created[0].task.due_date.as_deref(), Some("2031-03-14"));
        assert_eq!(created[0].task.postpone_date, None);
        assert_eq!(created[0].links.len(), 1);
        assert_eq!(
            orb.list_tasks(TaskViewName::Upcoming, None, None)
                .unwrap()
                .len(),
            1
        );

        undo(&orb).unwrap();
        assert!(
            orb.list_tasks(TaskViewName::Upcoming, None, None)
                .unwrap()
                .is_empty()
        );
        assert!(undo(&orb).unwrap().is_none());
    }

    /// A follow-up is linked to the message as it was sent, and lands on the
    /// date it was asked for; taking it back is one undo.
    #[test]
    fn a_follow_up_is_linked_to_the_message_it_follows_up() {
        let (_dir, orb, _) = handle_with_mail();
        let account = orb.config().accounts[0].clone();
        let mut email = outgoing_email_from_input(&tests::sample_send_input());
        email.subject = "[EXT] Offer for the roof".to_string();
        email.message_id = Some(crate::smtp::new_message_id(&account));
        let date = chrono::NaiveDate::from_ymd_opt(2031, 3, 14).unwrap();

        let detail = orb
            .create_follow_up(&account, &email, date)
            .expect("follow up");
        assert_eq!(detail.task.title, "Offer for the roof");
        assert_eq!(detail.task.postpone_date.as_deref(), Some("2031-03-14"));
        assert_eq!(detail.links.len(), 1);
        let link = &detail.links[0];
        assert_eq!(
            Some(link.message_id.as_str()),
            email
                .message_id
                .as_deref()
                .and_then(canonical_message_id)
                .as_deref()
        );
        assert_eq!(
            link.folder_name.as_deref(),
            Some(account.sent_folder.as_str())
        );

        let upcoming = || orb.list_tasks(TaskViewName::Upcoming, None, None).unwrap();
        assert_eq!(upcoming().len(), 1);
        undo(&orb).unwrap();
        assert!(upcoming().is_empty());
        assert!(undo(&orb).unwrap().is_none());
    }

    #[test]
    fn a_task_made_from_a_message_is_titled_with_the_munged_subject() {
        let (_dir, orb, message_ids) = handle_with_mail();

        let created = orb
            .create_tasks_from_messages(vec![message_ids[0]], None, None)
            .expect("create from message");

        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].task.title, "Budget review",
            "the title is the subject the user has been reading, not the raw one"
        );

        // The link carries everything needed to render the task when the
        // message is gone (§6.1).
        let link = &created[0].links[0];
        assert_eq!(link.message_id, "<budget@example.com>");
        assert_eq!(link.subject.as_deref(), Some("[EXT] Budget review"));
        assert_eq!(link.from.as_deref(), Some("Alice <alice@example.com>"));
        assert_eq!(link.date.as_deref(), Some("2026-08-01T09:00:00Z"));
        assert_eq!(link.account_name.as_deref(), Some("Work"));
        assert_eq!(link.folder_name.as_deref(), Some("INBOX"));
    }

    #[test]
    fn a_selection_becomes_one_task_each_and_one_undo_step() {
        let (_dir, orb, message_ids) = handle_with_mail();

        let created = orb
            .create_tasks_from_messages(message_ids.clone(), None, None)
            .expect("create from selection");
        assert_eq!(created.len(), 2);
        assert_eq!(
            created
                .iter()
                .map(|t| t.task.id)
                .collect::<HashSet<_>>()
                .len(),
            2,
            "one task each, not one task with two links"
        );

        assert_eq!(
            undo(&orb).unwrap().as_deref(),
            Some("create 2 tasks from mail")
        );
        assert_eq!(
            orb.list_tasks(TaskViewName::Trash, None, None)
                .unwrap()
                .len(),
            2,
            "one keystroke over two messages is one action to take back"
        );
    }

    #[test]
    fn the_message_list_column_counts_open_tasks_and_omits_the_zeroes() {
        let (_dir, orb, message_ids) = handle_with_mail();
        let created = orb
            .create_tasks_from_messages(vec![message_ids[0]], None, None)
            .unwrap();

        let counts = orb.open_task_counts(message_ids.clone()).unwrap();
        assert_eq!(counts.len(), 1, "a message with no tasks renders nothing");
        assert_eq!(counts[0].message_id, message_ids[0]);
        assert_eq!(counts[0].open_tasks, 1);

        // Completing it empties the column: the count is of *open* tasks.
        orb.complete_tasks(vec![created[0].task.id]).unwrap();
        assert!(orb.open_task_counts(message_ids).unwrap().is_empty());
    }

    #[test]
    fn asking_about_no_messages_asks_the_database_nothing() {
        let (_dir, orb, _) = handle_with_mail();
        assert!(orb.open_task_counts(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn a_message_detail_lists_its_tasks_ready_to_be_acted_on() {
        let (_dir, orb, message_ids) = handle_with_mail();
        orb.create_tasks_from_messages(vec![message_ids[0]], None, None)
            .unwrap();

        let tasks = orb.tasks_for_message(message_ids[0]).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Budget review");
        assert_eq!(tasks[0].linked_email_count, 1);

        assert!(orb.tasks_for_message(message_ids[1]).unwrap().is_empty());
    }

    #[test]
    fn a_link_opens_the_local_message_without_touching_the_network() {
        let (_dir, orb, message_ids) = handle_with_mail();
        let created = orb
            .create_tasks_from_messages(vec![message_ids[0]], None, None)
            .unwrap();
        let task_id = created[0].task.id;

        let resolved = orb
            .linked_message(task_id, "<budget@example.com>".to_string())
            .expect("resolve");
        assert_eq!(resolved.resolution, LinkResolution::Local);
        assert_eq!(resolved.local_id, Some(message_ids[0]));
        assert_eq!(resolved.folder_name.as_deref(), Some("INBOX"));
    }

    #[test]
    fn a_message_that_is_gone_leaves_the_task_readable() {
        let (_dir, orb, message_ids) = handle_with_mail();
        let created = orb
            .create_tasks_from_messages(vec![message_ids[0]], None, None)
            .unwrap();
        let task_id = created[0].task.id;

        // The message is deleted everywhere Orb can see — the state a task
        // reaches years later, and the one §6.2 step 3 exists for.
        let db = crate::db::Database::new(&orb.config().db_path).unwrap();
        db.conn
            .execute("DELETE FROM messages WHERE id = ?1", [message_ids[0]])
            .unwrap();

        let resolved = orb
            .linked_message(task_id, "<budget@example.com>".to_string())
            .expect("a missing message is not an error");
        assert_eq!(resolved.resolution, LinkResolution::Cached);
        assert!(resolved.local_id.is_none());
        assert_eq!(
            resolved.subject.as_deref(),
            Some("[EXT] Budget review"),
            "the cache is what keeps the task readable"
        );
        assert_eq!(resolved.from.as_deref(), Some("Alice <alice@example.com>"));

        // And the task itself is untouched.
        assert_eq!(orb.get_task(task_id).unwrap().task.title, "Budget review");
    }

    #[test]
    fn a_message_with_no_message_id_still_gets_its_task() {
        let (_dir, orb, _) = handle_with_mail();
        let db = crate::db::Database::new(&orb.config().db_path).unwrap();
        let folder_id = db
            .get_folder_by_name("Work", "INBOX")
            .unwrap()
            .unwrap()
            .id
            .unwrap();
        let id = db
            .insert_message(&Message {
                id: None,
                folder_id,
                uid: 99,
                message_id: None,
                subject: Some("No headers here".to_string()),
                from: None,
                to: None,
                cc: None,
                date: None,
                body_text: None,
                body_html: None,
                rfc822: None,
                size: None,
                flags: Vec::new(),
                modseq: None,
                shape: None,
                headers: None,
            })
            .unwrap();

        let created = orb
            .create_tasks_from_messages(vec![id], None, None)
            .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].task.title, "No headers here");
        assert!(
            created[0].links.is_empty(),
            "unlinkable, but a task all the same"
        );
    }

    #[test]
    fn a_message_can_be_linked_to_a_task_that_already_exists() {
        let (_dir, orb, message_ids) = handle_with_mail();
        let task_id = orb
            .create_task("Quarterly review".to_string(), None)
            .unwrap()
            .task
            .id;

        let link = orb
            .link_message_to_task(task_id, message_ids[1])
            .expect("link");
        assert_eq!(link.message_id, "<lunch@example.com>");
        assert_eq!(orb.get_task(task_id).unwrap().links.len(), 1);

        assert!(
            orb.unlink_message_from_task(task_id, "<lunch@example.com>".to_string())
                .unwrap()
        );
        assert!(orb.get_task(task_id).unwrap().links.is_empty());
    }

    #[test]
    fn one_message_can_carry_more_than_one_task_and_one_task_more_than_one_message() {
        let (_dir, orb, message_ids) = handle_with_mail();
        let first = orb
            .create_tasks_from_messages(vec![message_ids[0]], None, None)
            .unwrap();
        let second = orb
            .create_tasks_from_messages(vec![message_ids[0]], None, None)
            .unwrap();
        assert_ne!(first[0].task.id, second[0].task.id);
        assert_eq!(
            orb.open_task_counts(vec![message_ids[0]]).unwrap()[0].open_tasks,
            2
        );

        orb.link_message_to_task(first[0].task.id, message_ids[1])
            .unwrap();
        assert_eq!(orb.get_task(first[0].task.id).unwrap().links.len(), 2);
    }

    // ------------------------------------------------------------------
    // Folder exclusion (§1)
    // ------------------------------------------------------------------

    #[test]
    fn the_namespaced_tasks_folder_is_hidden_from_the_mail_world_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);

        // What the last sync found the folder to actually be called.
        {
            let db = crate::db::Database::new(&config.db_path).unwrap();
            db.init_schema().unwrap();
            let repo = TaskRepository::new(&db);
            let mut state = TaskSyncState::new("Work".to_string());
            state.folder_name = "INBOX.Tasks".to_string();
            repo.upsert_sync_state(&state).unwrap();
        }

        hide_resolved_tasks_folder(&mut config);

        let account = &config.accounts[0];
        assert!(
            !account.should_sync_folder("INBOX.Tasks"),
            "a folder hidden under one name and synced under the other is the \
             failure §1 forbids"
        );
        assert!(!account.is_folder_visible("INBOX.Tasks"));
        assert!(
            account.should_sync_folder("INBOX"),
            "the account keeps working as an ordinary mail account"
        );
    }

    #[test]
    fn a_database_without_a_task_sync_state_changes_no_configuration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = task_config(&dir);
        let before = config.accounts[0].folders.len();

        // Nothing has synced yet — the common case on a first launch, and one
        // that must not stop the app from starting.
        hide_resolved_tasks_folder(&mut config);
        assert_eq!(config.accounts[0].folders.len(), before);
    }
}
