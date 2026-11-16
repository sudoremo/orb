use crate::commands::{StateUpdate, SyncStage};
use crate::config::{AccountConfig, Config, PinnedFolder, SortOrder};
use crate::constants::{
    INITIAL_FETCH_PAGE, MAX_RETRY_ATTEMPTS, MAX_SYNC_RETRY_ATTEMPTS,
    SYNC_LOCK_RENEW_INTERVAL_SECONDS, SYNC_RETRY_INITIAL_DELAY_MS, SYNC_RETRY_MAX_DELAY_MS,
};
use crate::credentials::PasswordCache;
use crate::db::{Database, LockAttempt, OutboxItem};
use crate::error::FailureKind;
use crate::imap::{self, AccountConnectionConfig, ConnectionPool, ImapClient, SyncTrace};
use crate::models::{Folder, FolderDisplay, MessageListItem, SearchResultItem};
use crate::{Error, Result};
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod feeds;
mod filtering;
mod highlighting;
mod reading;
mod threading;
mod transfer;

pub use feeds::{FeedPollReport, FeedStatus};
pub use filtering::{FilterActivity, FilterOutcome};
pub use highlighting::{HighlightBadge, HighlightCount};
pub use threading::{Location, ThreadPlace, ThreadRow};
pub use transfer::TransferOutcome;

/// Everything a task's link to a message has to remember about it (§6.1).
///
/// The cached fields are not a convenience: they are what keeps a task readable
/// after the message it came from has been deleted, archived on another machine
/// or lost with an account. `folder_name` is the last place the message was
/// seen, which is where the server search of §6.2 starts looking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLinkSource {
    /// Local row id, so the caller can correlate this with the selection it
    /// asked about.
    pub local_id: i64,
    /// The RFC `Message-ID`, which is the link's identity. `None` for a message
    /// whose header was absent — such a message cannot be linked at all.
    pub rfc_message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub date: Option<String>,
    pub account_name: String,
    pub folder_name: String,
}

/// The two spellings of an RFC `Message-ID`: with and without angle brackets.
///
/// The header is `<id@host>` and that is how sync stores it, but a link can also
/// arrive from somewhere that stripped the brackets — a URL, a header copied by
/// hand, another client's export. Matching both costs one extra bound parameter
/// and removes an entire class of "the message is right there and Orb cannot
/// find it".
///
/// Returns the two forms in a fixed order (bracketed, bare); either may be empty
/// when the input has nothing left after trimming, and an empty variant must
/// never be used as a search term — it would match every message with no
/// `Message-ID` at all.
pub fn message_id_variants(rfc_message_id: &str) -> [String; 2] {
    let trimmed = rfc_message_id.trim();
    let bare = trimmed
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(trimmed)
        .trim();
    if bare.is_empty() {
        return [String::new(), String::new()];
    }
    [format!("<{bare}>"), bare.to_string()]
}

/// The folders to search for a message, in the order that finds it soonest.
///
/// The link's cached folder comes first because it is where the message was last
/// seen and is right the overwhelming majority of the time; `INBOX` comes next
/// because a message that moved usually moved out of or into it. The rest follow
/// in the server's own listing order, which at least groups a hierarchy together.
///
/// `skip` takes the tasks folder out: its messages are tasks, and handing one to
/// the mail reader would be showing the user a `X-Orb-Task-Id` header as if
/// somebody had sent it.
///
/// Pure, and separate from the search itself, because "which folder do we try
/// first" is the only interesting decision in a multi-folder search and it is not
/// worth a live server to test.
pub fn search_folder_order(
    preferred: Option<&str>,
    listed: &[String],
    skip: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut order: Vec<String> = Vec::with_capacity(listed.len() + 2);
    let push = |candidate: &str, order: &mut Vec<String>| {
        if candidate.is_empty() || skip(candidate) {
            return;
        }
        if order.iter().any(|f| f.eq_ignore_ascii_case(candidate)) {
            return;
        }
        order.push(candidate.to_string());
    };

    if let Some(preferred) = preferred {
        push(preferred, &mut order);
    }
    push("INBOX", &mut order);
    for folder in listed {
        push(folder, &mut order);
    }
    order
}

/// Which of several local copies of one message to open.
///
/// Preference order: the exact account and folder the link remembers, then the
/// same account in any folder, then anything at all. The account matters more
/// than the folder because the same message really does exist under two accounts
/// — a mailing list delivered twice, a forward to oneself — and opening the copy
/// in the wrong account lands the reader in a mailbox the user was not looking at.
pub fn prefer_located_message(
    candidates: Vec<SearchResultItem>,
    account: Option<&str>,
    folder: Option<&str>,
) -> Option<SearchResultItem> {
    let score = |item: &SearchResultItem| -> u8 {
        let account_matches = account.is_some_and(|a| a == item.account_name);
        let folder_matches = folder.is_some_and(|f| f == item.folder_name);
        match (account_matches, folder_matches) {
            (true, true) => 3,
            (true, false) => 2,
            (false, true) => 1,
            (false, false) => 0,
        }
    };
    candidates.into_iter().max_by_key(|item| score(item))
}

/// Every locally-stored message carrying one of the given `Message-ID` spellings.
fn find_messages_by_rfc_id(
    conn: &rusqlite::Connection,
    variants: &[String; 2],
) -> Result<Vec<SearchResultItem>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.folder_id, m.uid, m.message_id, m.subject, m.from_addr, m.to_addr,
                m.cc_addr, m.date, m.size, m.flags, m.modseq, f.account_name, f.name
         FROM messages m
         JOIN folders f ON f.id = m.folder_id
         WHERE m.message_id IN (?1, ?2)
         ORDER BY m.date_unix DESC NULLS LAST, m.id DESC",
    )?;
    let rows = stmt.query_map(params![variants[0], variants[1]], |row| {
        Ok(SearchResultItem {
            message: MessageListItem {
                id: row.get(0)?,
                folder_id: row.get(1)?,
                uid: row.get(2)?,
                message_id: row.get(3)?,
                subject: row.get(4)?,
                from: row.get(5)?,
                to: row.get(6)?,
                cc: row.get(7)?,
                date: row.get(8)?,
                size: row.get(9)?,
                flags: parse_stored_flags(row.get::<_, Option<String>>(10)?),
                modseq: row.get(11)?,
            },
            account_name: row.get(12)?,
            folder_name: row.get(13)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// The RFC `Message-ID` of each of the given local rows, in one query.
fn rfc_message_ids(
    conn: &rusqlite::Connection,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, String>> {
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, message_id FROM messages
         WHERE id IN ({placeholders}) AND message_id IS NOT NULL AND message_id <> ''"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The link-worthy metadata of each of the given local rows, in one query.
fn message_link_sources(
    conn: &rusqlite::Connection,
    ids: &[i64],
) -> Result<Vec<MessageLinkSource>> {
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.message_id, m.subject, m.from_addr, m.date, f.account_name, f.name
         FROM messages m
         JOIN folders f ON f.id = m.folder_id
         WHERE m.id IN ({placeholders})"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
        Ok(MessageLinkSource {
            local_id: row.get(0)?,
            rfc_message_id: row
                .get::<_, Option<String>>(1)?
                .filter(|id| !id.trim().is_empty()),
            subject: row.get(2)?,
            from: row.get(3)?,
            date: row.get(4)?,
            account_name: row.get(5)?,
            folder_name: row.get(6)?,
        })
    })?;
    let mut sources: Vec<MessageLinkSource> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    // The caller passed an order (the selection's order); the `IN` clause did
    // not preserve it, so restore it rather than handing back SQLite's.
    sources.sort_by_key(|source| {
        ids.iter()
            .position(|id| *id == source.local_id)
            .unwrap_or(usize::MAX)
    });
    Ok(sources)
}

/// Remove a folder and its messages from the local mail cache.
fn forget_folder(
    conn: &rusqlite::Connection,
    account_name: &str,
    folder_name: &str,
) -> Result<usize> {
    let folder_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM folders WHERE account_name = ?1 AND name = ?2",
            params![account_name, folder_name],
            |row| row.get(0),
        )
        .ok();
    let Some(folder_id) = folder_id else {
        return Ok(0);
    };
    let removed = conn.execute(
        "DELETE FROM messages WHERE folder_id = ?1",
        params![folder_id],
    )?;
    conn.execute("DELETE FROM folders WHERE id = ?1", params![folder_id])?;
    Ok(removed)
}

/// Read the `flags` column, which is JSON but was comma-separated in databases
/// written by older builds.
fn parse_stored_flags(stored: Option<String>) -> Vec<String> {
    let stored = stored.unwrap_or_default();
    serde_json::from_str(&stored).unwrap_or_else(|_| {
        stored
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    })
}

/// Store manages email synchronization from IMAP servers to the local database
///
/// Architecture for database access:
/// - Reads: Create connection on demand (concurrent, fast)
/// - Writes: Serialized via write_lock Mutex to prevent FTS index corruption
///
/// The write_lock ensures only one write operation happens at a time across
/// the entire process, preventing the FTS trigger corruption we saw with
/// concurrent writes from multiple connections.
/// Messages per FETCH when backfilling bodies.
///
/// Large enough that the round trip is amortised, small enough that a slow
/// server cannot tie up a connection for long or produce a response that has to
/// be buffered whole.
const BODY_BATCH_SIZE: usize = 50;

/// How many of a folder's newest messages have their HTML fetched ahead of
/// being opened.
///
/// Mail a few hundred messages down is rarely opened, and the HTML of a whole
/// archive runs to gigabytes.
const RECENT_HTML_WINDOW: usize = 300;

/// What a BODYSTRUCTURE fetch found, and what it could not read.
///
/// Servers do send structures that violate RFC 3501 — a part with a `NIL`
/// media type, say — and the parser rejects the whole response, not the one
/// bad message. Since a batch covers hundreds of messages and the shaping pass
/// retries whatever it has not stored, one such message would otherwise fail
/// every pass over its folder forever, and take the others down with it.
#[derive(Debug, Clone, Default)]
struct ShapeFetch {
    shapes: std::collections::HashMap<u32, crate::imap::structure::MessageShape>,
    /// UIDs the server described but the parser could not read.
    unreadable: Vec<u32>,
}

/// Messages per BODYSTRUCTURE fetch.
///
/// Larger than the body batch because the response carries no content — only a
/// description of each message's parts.
const SHAPE_BATCH_SIZE: usize = 300;

/// Messages fetched per sync to repair a folder that has fallen behind.
///
/// Bounded so that discovering a large gap does not turn one sync into a
/// multi-minute fetch. Each pass recomputes what is still missing, so the
/// repair converges over successive syncs instead of stalling one of them.
const MISSING_MESSAGE_REPAIR_BUDGET: usize = 2000;

/// At most this many messages that have just arrived in a folder are fetched
/// whole by the sync that found them, newest first.
const ARRIVED_PREFETCH_MESSAGES: usize = 20;

/// And at most this many bytes of them together, so that a large attachment
/// is left to the background pass rather than holding up the list.
const ARRIVED_PREFETCH_BYTES: usize = 4 * 1024 * 1024;

/// What comparing the local and server UID lists found.
///
/// Both directions matter. `deleted` keeps the local copy from holding mail
/// that is gone; `missing` is what lets a folder that has drifted catch back
/// up, which nothing did before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reconciliation {
    /// Stored locally, absent from the server.
    deleted: Vec<u32>,
    /// Present on the server, absent locally.
    missing: Vec<u32>,
}

/// What a folder is for: the inbox; else the role the server flags it with
/// (RFC 6154); else the role the account's config gives it.
fn folder_role(
    account: &AccountConfig,
    listed: &[imap::ListedFolder],
    name: &str,
) -> Option<String> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some("inbox".to_string());
    }
    let flagged = listed
        .iter()
        .find(|f| f.name == name)
        .and_then(|f| f.special_use.as_deref())
        .map(|role| role.trim_start_matches('\\').to_ascii_lowercase());
    if flagged.is_some() {
        return flagged;
    }
    [
        (Some(account.drafts_folder.as_str()), "drafts"),
        (Some(account.sent_folder.as_str()), "sent"),
        (Some(account.trash_folder.as_str()), "trash"),
        (account.archive_folder.as_deref(), "archive"),
        (account.junk_folder.as_deref(), "junk"),
    ]
    .into_iter()
    .find(|(configured, _)| *configured == Some(name))
    .map(|(_, role)| role.to_string())
}

pub struct Store {
    /// Swapped whole when the file is re-read; see [`Store::replace_config`].
    config: std::sync::RwLock<Arc<Config>>,
    db_path: String,
    /// Mutex to serialize write operations (prevents FTS corruption)
    write_lock: Arc<tokio::sync::Mutex<()>>,
    /// One warm, authenticated connection per account, shared by everything
    /// that talks to the server.
    connection_pool: Arc<imap::ConnectionPool>,
    /// The accounts' passwords, each command run once per session.
    credentials: Arc<PasswordCache>,
    /// Each account's folders as the server last listed them, read once per
    /// listing rather than once per write: `get_account_config` resolves the
    /// special folders against it on every call.
    server_folders: std::sync::Mutex<HashMap<String, Arc<Vec<imap::ListedFolder>>>>,
    /// One drain of each account's outbox at a time. Two at once would both
    /// read the same queued items, and a queued send would go out twice.
    outbox_drains: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    outbox_listener: std::sync::RwLock<Option<Arc<dyn OutboxListener>>>,
}

/// What became of one folder in a sync.
#[derive(Debug)]
enum FolderOutcome {
    Synced(FolderSyncReport),
    /// Another sync holds the folder's lock; nothing was done.
    Skipped {
        holder: String,
    },
}

/// Renews a folder's sync lock on a timer for as long as it is alive.
///
/// The lock goes stale after `SYNC_LOCK_TIMEOUT_SECONDS`, and a cold sync of
/// a large folder outlasts that. Renewing between phases is not enough: the
/// longest phase is the fetch, and a renewal that waits for it hands the
/// folder to another instance in the middle of it.
struct LockRenewer {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl LockRenewer {
    fn start(
        db_path: String,
        write_lock: Arc<tokio::sync::Mutex<()>>,
        account: String,
        folder: String,
    ) -> Self {
        let stop = CancellationToken::new();
        let token = stop.clone();
        let task = tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(SYNC_LOCK_RENEW_INTERVAL_SECONDS);
            loop {
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = tokio::time::sleep(interval) => {}
                }
                let renewed = {
                    let _guard = write_lock.lock().await;
                    Database::new(&db_path).and_then(|mut db| db.renew_sync_lock(&account, &folder))
                };
                match renewed {
                    Ok(true) => {}
                    Ok(false) => crate::logger::warn(&format!(
                        "sync[{account}/{folder}]: the folder lock is no longer ours"
                    )),
                    Err(e) => crate::logger::warn(&format!(
                        "sync[{account}/{folder}]: could not renew the folder lock: {e}"
                    )),
                }
            }
        });
        Self { stop, task }
    }
}

impl Drop for LockRenewer {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

/// Outbox kinds.
const OUTBOX_FLAGS: &str = "flags";
const OUTBOX_SEND: &str = "send";
const OUTBOX_MOVE: &str = "move";

/// The SMTP login a queued message goes out over, by the name
/// [`AccountConfig::sending_as`] gives it.
fn queued_login(account: &AccountConfig, item: &OutboxItem) -> String {
    let from = serde_json::from_str::<crate::smtp::OutgoingEmail>(&item.payload)
        .ok()
        .and_then(|email| email.from);
    account.sending_as(from.as_deref()).name.clone()
}

/// The subject of a queued message, for a notice about it.
fn queued_subject(item: &OutboxItem) -> String {
    serde_json::from_str::<crate::smtp::OutgoingEmail>(&item.payload)
        .map(|email| email.subject)
        .unwrap_or_default()
}

/// A queued flag change, as the outbox stores it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FlagWrite {
    folder: String,
    uids: Vec<u32>,
    flag: String,
    add: bool,
}

/// A queued move — or, without a target, a permanent delete — as the outbox
/// stores it. The messages have already left the stored copy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MoveWrite {
    folder: String,
    uids: Vec<u32>,
    target: Option<String>,
    /// The UIDVALIDITY the UIDs were issued under.
    uidvalidity: Option<u32>,
}

/// What one attempt at a queued send came to, short of a refusal.
enum SendAttempt {
    Sent,
    /// The mail server could not be reached; the message stays queued.
    Unreachable(String),
    /// It will not go out as it is — the server refused it, its password
    /// could not be had, or it cannot be read. It stays in the outbox,
    /// failed, for the user to retry, edit or delete.
    Refused(String),
}

/// A message in the Outbox: queued because the mail server could not be
/// reached, or failed because it refused it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueuedSend {
    pub id: i64,
    pub account: String,
    /// The login it goes out over: the account's name, or `Account (Identity)`.
    pub sending_as: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// The attachments' file names.
    pub attachments: Vec<String>,
    /// RFC 3339.
    pub queued_at: String,
    pub attempts: i64,
    /// Why the last attempt did not send it.
    pub error: Option<String>,
    /// RFC 3339; set when it failed for good rather than still waiting.
    pub failed_at: Option<String>,
}

/// A queued message the server has just refused.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedSend {
    pub id: i64,
    pub account: String,
    pub subject: String,
    pub reason: String,
}

/// Hears of changes to the Outbox; see [`Store::set_outbox_listener`].
pub trait OutboxListener: Send + Sync + 'static {
    /// A message was queued, sent, refused, put back in line or deleted.
    /// `failed` names the ones refused by this change.
    fn on_outbox_changed(&self, failed: Vec<FailedSend>);
}

/// What sending a message came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Sent {
        saved_to_sent: bool,
        warning: Option<String>,
    },
    /// The mail server could not be reached; the message waits in the
    /// outbox and goes out when it can.
    Queued,
}

/// Lightweight account information for UI rendering
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountInfo {
    pub name: String,
    pub email: String,
}

/// Sort order for message lists
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum MessageSort {
    /// Newest messages first (default)
    DateNewest,
    /// Oldest messages first
    DateOldest,
    /// Subject A-Z
    SubjectAsc,
    /// Subject Z-A
    SubjectDesc,
    /// Sender A-Z
    SenderAsc,
    /// Sender Z-A
    SenderDesc,
}

impl MessageSort {
    /// Convert to SQL ORDER BY clause
    pub(crate) fn as_sql(&self) -> &'static str {
        match self {
            // Use date_unix (integer timestamp) for reliable sorting
            // Each ends in `id` so that ties — common by subject or sender,
            // and across several folders — keep one order from page to page.
            MessageSort::DateNewest => "date_unix DESC NULLS LAST, id DESC",
            MessageSort::DateOldest => "date_unix ASC NULLS LAST, id ASC",
            MessageSort::SubjectAsc => "subject ASC, id DESC",
            MessageSort::SubjectDesc => "subject DESC, id DESC",
            MessageSort::SenderAsc => "from_addr ASC, id DESC",
            MessageSort::SenderDesc => "from_addr DESC, id DESC",
        }
    }

    /// [`Self::as_sql`] with every column taken from the table `alias`, for a
    /// query that joins another table with an `id` of its own.
    pub(crate) fn as_sql_on(&self, alias: &str) -> String {
        self.as_sql()
            .split(", ")
            .map(|term| format!("{alias}.{term}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Scope restriction for a full-text mailbox search.
///
/// Search is whole-mailbox by default ([`SearchScope::All`]); the narrower
/// variants let the caller restrict results to a single account or a single
/// folder within an account.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SearchScope {
    /// Every folder of every configured account.
    All,
    /// All folders belonging to a single account.
    Account { account_name: String },
    /// A single folder within a single account.
    Folder {
        account_name: String,
        folder_name: String,
    },
    /// Several folders, as `(account, folder)`: the list a filter narrows,
    /// such as Today's inboxes. None is no folder at all.
    Folders(Vec<(String, String)>),
}

/// Maximum number of full-text search results returned in one query. Keeps the
/// result set (and the FFI marshalling to Swift) bounded for very common terms.
pub const SEARCH_RESULT_LIMIT: usize = 500;

/// Scope restriction for staged (pending) destructive actions.
///
/// Mirrors [`SearchScope`] but for the `pending_actions` table: whole-mailbox
/// ([`PendingScope::All`]), a single account, or a single folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingScope {
    /// Every folder of every configured account.
    All,
    /// All folders belonging to a single account.
    Account { account_name: String },
    /// A single folder within a single account.
    Folder {
        account_name: String,
        folder_name: String,
    },
}

/// The action type stored for a staged (pending) action.
pub const PENDING_ACTION_DELETE: &str = "delete";
/// The action type stored for a staged (pending) action.
pub const PENDING_ACTION_MOVE: &str = "move";

/// The concrete, safe server operation a staged action resolves to. Separated
/// from [`Store::execute_pending`] so the trash-vs-permanent-vs-move decision is
/// pure and unit-testable without any IMAP/DB IO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingExecutionOp {
    /// Move the messages to `target`.
    Move { target: String },
    /// Permanently delete: the messages already live in the account's Trash.
    PermanentDelete,
    /// Recoverable delete: move the messages to the account's Trash folder.
    MoveToTrash { trash: String },
}

/// Resolve the safe server operation a staged action performs. A `delete` in the
/// account's Trash is permanent; a `delete` elsewhere is a recoverable move to
/// Trash; a `move` requires a non-empty target folder.
pub fn resolve_pending_execution_op(
    action_type: &str,
    folder: &str,
    trash_folder: &str,
    target: Option<&str>,
) -> Result<PendingExecutionOp> {
    match action_type {
        PENDING_ACTION_MOVE => {
            let target = target
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    Error::InvalidInput("Staged move is missing its target folder".to_string())
                })?;
            Ok(PendingExecutionOp::Move {
                target: target.to_string(),
            })
        }
        PENDING_ACTION_DELETE => {
            if folder == trash_folder {
                Ok(PendingExecutionOp::PermanentDelete)
            } else {
                Ok(PendingExecutionOp::MoveToTrash {
                    trash: trash_folder.to_string(),
                })
            }
        }
        other => Err(Error::InvalidInput(format!(
            "Unknown pending action type '{}'",
            other
        ))),
    }
}

/// What [`Store::delete_messages`] did with the messages.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum DeleteOutcome {
    /// Moved to the named Trash folder, from where they can be recovered.
    MovedToTrash {
        trash: String,
        /// See [`WriteOutcome::warning`].
        warning: Option<String>,
        /// See [`WriteOutcome::moved`].
        moved: Vec<String>,
    },
    /// Expunged for good: they were already in Trash.
    Deleted {
        /// See [`WriteOutcome::warning`].
        warning: Option<String>,
    },
}

/// What a delete or move the server carried out left behind.
///
/// Once the server has done the work the write is done, whatever happens
/// next: a database that then cannot be brought in step is a warning, not a
/// failure, because the next sync reconciles it. It is a warning the user has
/// to see, though: until that sync the messages are still in the list, and
/// deleting them again does nothing visible.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriteOutcome {
    /// Why the list does not yet show what the server has, when it does not.
    pub warning: Option<String>,
    /// The RFC `Message-ID` of every message the write relocated, so the move
    /// can be undone by [`Store::move_by_message_ids`] once the messages have
    /// new UIDs in another folder. Empty for a write that relocated nothing,
    /// and shorter than the UID list when a message carried no `Message-ID`.
    #[serde(default)]
    pub moved: Vec<String>,
}

/// What [`Store::execute_pending`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingExecution {
    /// How many staged actions were applied here and queued for the server.
    pub executed: usize,
    /// One [`WriteOutcome::warning`] per batch that left the database behind.
    pub warnings: Vec<String>,
    /// Each move carried out, as `(account, from, to, Message-IDs)`, for
    /// undo. A permanent delete has none.
    #[serde(skip)]
    pub moves: Vec<(String, String, String, Vec<String>)>,
}

/// A single persisted staged action row, as read back from the
/// `pending_actions` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingActionRecord {
    pub account: String,
    pub folder: String,
    pub uid: u32,
    /// One of [`PENDING_ACTION_DELETE`] or [`PENDING_ACTION_MOVE`].
    pub action_type: String,
    /// Destination folder for a `move`; `None` for a `delete`.
    pub target_folder: Option<String>,
    pub created_at: String,
}

/// The message a staged action names, as far as a review needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMessage {
    pub id: i64,
    pub subject: Option<String>,
    pub from: Option<String>,
}

/// Number of staged actions in a single (account, folder), for badge display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingActionCount {
    pub account: String,
    pub folder: String,
    pub count: u32,
}

impl From<SortOrder> for MessageSort {
    fn from(order: SortOrder) -> Self {
        match order {
            SortOrder::DateNewest => MessageSort::DateNewest,
            SortOrder::DateOldest => MessageSort::DateOldest,
            SortOrder::SubjectAsc => MessageSort::SubjectAsc,
            SortOrder::SubjectDesc => MessageSort::SubjectDesc,
            SortOrder::SenderAsc => MessageSort::SenderAsc,
            SortOrder::SenderDesc => MessageSort::SenderDesc,
        }
    }
}

/// Report of sync operation results
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SyncReport {
    pub accounts_synced: usize,
    pub folders_synced: usize,
    /// Folders that were not synced because another sync held their lock.
    pub folders_skipped: usize,
    pub messages_added: usize,
    pub messages_updated: usize,
    pub messages_deleted: usize,
    /// New messages a filter rule acted on.
    pub messages_filtered: usize,
    /// The unread mail this sync brought to the watched folders, oldest
    /// first, as it stands after the filter rules ran on it: what a new-mail
    /// notification is made from.
    pub new_mail: Vec<crate::db::NewMessage>,
    pub errors: Vec<String>,
    /// What a filter rule could not do. Not a failure of the sync: the mail
    /// arrived, and the account is fine.
    pub filter_warnings: Vec<String>,
    /// One line per skipped folder, naming the lock's holder.
    pub skipped: Vec<String>,
    /// The same failures as `errors`, typed, for an account status that has
    /// to say whether the password, the certificate or the network is wrong.
    pub failures: Vec<SyncFailure>,
}

/// One thing a sync could not do, with the kind of failure it was.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncFailure {
    pub account: String,
    /// The folder concerned, or `None` when the whole account was.
    pub folder: Option<String>,
    pub kind: FailureKind,
    pub message: String,
}

impl SyncReport {
    fn new() -> Self {
        Self::default()
    }

    fn merge(&mut self, other: SyncReport) {
        self.folders_synced += other.folders_synced;
        self.folders_skipped += other.folders_skipped;
        self.messages_added += other.messages_added;
        self.messages_updated += other.messages_updated;
        self.messages_deleted += other.messages_deleted;
        self.messages_filtered += other.messages_filtered;
        self.new_mail.extend(other.new_mail);
        self.errors.extend(other.errors);
        self.filter_warnings.extend(other.filter_warnings);
        self.skipped.extend(other.skipped);
        self.failures.extend(other.failures);
    }

    /// Record a failure, both as the line the status bar shows and typed.
    fn fail(&mut self, account: &str, folder: Option<&str>, kind: FailureKind, message: String) {
        self.failures.push(SyncFailure {
            account: account.to_string(),
            folder: folder.map(str::to_string),
            kind,
            message: message.clone(),
        });
        self.errors.push(message);
    }

    /// The failure most worth showing for `account`, if any.
    ///
    /// The most severe kind wins (see [`FailureKind::severity`]), and among
    /// equals the first, which is the one the others followed from.
    pub fn worst_failure(&self, account: &str) -> Option<&SyncFailure> {
        self.failures
            .iter()
            .filter(|f| f.account == account)
            .min_by_key(|f| std::cmp::Reverse(f.kind.severity()))
    }
}

/// Report for a single folder sync operation
#[derive(Debug, Default)]
struct FolderSyncReport {
    messages_added: usize,
    messages_updated: usize,
    messages_deleted: usize,
    /// The rows of the mail that arrived: what the sync added to a folder it
    /// had synced before. A first sync brings a folder's past, not news.
    added_ids: Vec<i64>,
}

impl FolderSyncReport {
    fn add(&mut self, other: &FolderSyncReport) {
        self.messages_added += other.messages_added;
        self.messages_updated += other.messages_updated;
        self.messages_deleted += other.messages_deleted;
        self.added_ids.extend(&other.added_ids);
    }
}

/// Pure classification of whether a folder-sync pass should run deletion
/// detection, and — when skipped — why (used only for logging).
///
/// This mirrors the branching that used to live inline in `sync_folder_impl`.
/// Extracting it keeps `sync_folder_impl` a thin orchestrator and makes the
/// decision unit-testable in isolation. `Run` corresponds one-to-one with the
/// original run condition; every skip variant maps to the exact `skip_reason`
/// string the old inline `if/else` chain produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeletionDetectionPlan {
    /// Run deletion detection (VANISHED or stored-vs-server UID comparison).
    Run,
    /// First sync: no baseline modseq yet, nothing to compare against.
    SkipFirstSync,
    /// UIDVALIDITY changed: a full clear + resync happens instead.
    SkipUidValidityChanged,
    /// Modseq unchanged on a QRESYNC server: mailbox provably identical.
    SkipModseqUnchanged,
    /// Caller explicitly requested skipping (e.g. MOVE destination folder).
    SkipRequested,
}

impl DeletionDetectionPlan {
    /// Whether deletion detection should actually run for this pass.
    fn should_run(self) -> bool {
        matches!(self, DeletionDetectionPlan::Run)
    }

    /// Log label identical to the strings the old inline chain emitted.
    fn skip_reason(self) -> &'static str {
        match self {
            DeletionDetectionPlan::Run => "none",
            DeletionDetectionPlan::SkipFirstSync => "first_sync",
            DeletionDetectionPlan::SkipUidValidityChanged => "uidvalidity_changed",
            DeletionDetectionPlan::SkipModseqUnchanged => "modseq_unchanged",
            DeletionDetectionPlan::SkipRequested => "skip_requested",
        }
    }
}

/// Whether a process on this host is still running, for breaking the sync
/// locks of ones that are not. Answers yes when it cannot tell, since a lock
/// wrongly broken is worse than one that ages out on its own.
fn process_is_alive(pid: u32) -> bool {
    // iOS runs one instance of the app, and its sandbox may answer EPERM for
    // a recycled pid that now belongs to another app: a lock not held by this
    // process was left by an earlier launch.
    #[cfg(target_os = "ios")]
    {
        let _ = pid;
        false
    }
    #[cfg(all(unix, not(target_os = "ios")))]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return true;
        };
        // Signal 0 delivers nothing and reports whether it could have. EPERM
        // means the process exists and belongs to somebody else.
        let answer = unsafe { libc::kill(pid, 0) };
        answer == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

impl Store {
    /// Create a new Store instance
    ///
    /// Initializes the database schema and sync_meta table if they don't exist.
    pub fn new(config: Config) -> Result<Self> {
        let db_path = config.db_path.clone();
        imap::set_extra_headers(config.extra_header_names());

        // Initialize database with schema
        let mut db = Database::new(&db_path)?;
        db.init_schema()?;
        db.refetch_headers_for_new_names(&config.extra_header_names())?;

        // A process killed mid-sync leaves its folder locks behind, and every
        // instance then skips those folders until the locks age out.
        match db.break_dead_locks(process_is_alive) {
            Ok(0) => {}
            Ok(broken) => crate::logger::info(&format!(
                "broke {broken} sync lock(s) left by processes that are gone"
            )),
            Err(e) => crate::logger::warn(&format!("could not check for dead sync locks: {e}")),
        }

        Ok(Store {
            config: std::sync::RwLock::new(Arc::new(config)),
            db_path,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            connection_pool: Arc::new(ConnectionPool::new()),
            credentials: PasswordCache::new(),
            server_folders: std::sync::Mutex::new(HashMap::new()),
            outbox_drains: std::sync::Mutex::new(HashMap::new()),
            outbox_listener: std::sync::RwLock::new(None),
        })
    }

    /// An account's folders as the server last listed them, from the cache
    /// or, the first time, from the database.
    fn listed_folders(&self, account: &str) -> Arc<Vec<imap::ListedFolder>> {
        if let Some(listed) = self
            .server_folders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
        {
            return listed.clone();
        }
        let listed = Arc::new(
            self.reader()
                .and_then(|db| db.server_folders(account))
                .unwrap_or_default(),
        );
        self.server_folders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), listed.clone());
        listed
    }

    /// Forget what was cached of an account's folder list, because it changed.
    fn forget_listed_folders(&self, account: &str) {
        self.server_folders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
    }

    /// Get a reference to the connection pool
    pub fn connection_pool(&self) -> Arc<ConnectionPool> {
        self.connection_pool.clone()
    }

    /// The accounts' passwords, shared with whatever else connects.
    pub fn credentials(&self) -> Arc<PasswordCache> {
        self.credentials.clone()
    }

    /// Reconcile the local database against the current configuration.
    ///
    /// Removes folders (and their messages) that belong to accounts no longer
    /// present in the config. This prevents stale/duplicate folder sets from
    /// lingering after an account is removed or renamed in the config (the
    /// user-visible "duplicate folder display" bug).
    ///
    /// The decision keys off the *config*, never runtime/selection state, so a
    /// folder that is merely not currently selected is never removed. Folders
    /// with `sync = false` are also preserved: that flag means "show but don't
    /// auto-sync", not "removed", so deleting them (and their cached messages)
    /// would be data loss. Because account orphans are deleted outright,
    /// re-enabling an account later starts from a fresh row (uidvalidity/modseq
    /// reset to 0), so it re-syncs cleanly.
    ///
    /// Intended to run at startup / config reload. Serialized with other writers
    /// via the write lock, and performed in a single database transaction.
    pub async fn reconcile_with_config(&self) -> Result<crate::db::ReconcileReport> {
        // Serialize with other writers (e.g. concurrent sync) to avoid racing
        // folder creation while we prune.
        let lock = self.write_lock.clone();
        let _guard = lock.lock().await;

        // Valid accounts = exactly those in the config, keyed by name.
        let valid_accounts: HashSet<String> = self
            .config()
            .accounts
            .iter()
            .map(|a| a.name.clone())
            .collect();

        // NOTE: Folders with `sync = false` are intentionally NOT pruned here.
        // `sync = false` is a persistent "show but don't auto-sync" preference
        // (orthogonal to `visible`), not a removal — deleting such a folder would
        // permanently destroy its cached messages, and sync never re-creates it.
        // Only folders whose account is gone from the config are pruned.
        let mut db = self.writer()?;
        let report = db.prune_orphaned_folders(&valid_accounts)?;

        if report.folders_removed > 0 {
            crate::logger::info(&format!(
                "Config reconcile: pruned {} orphaned folder(s) and {} message(s)",
                report.folders_removed, report.messages_removed
            ));
        }

        Ok(report)
    }

    /// Create a read-only database connection
    ///
    /// Connections are created on demand. Multiple readers can operate
    /// concurrently since SQLite WAL mode allows this.
    pub fn reader(&self) -> Result<Database> {
        Database::new(&self.db_path)
    }

    /// Create a read-write database connection
    ///
    /// Note: For write operations in production code, you should also acquire
    /// the write_lock() to prevent FTS index corruption from concurrent writes.
    /// This method is primarily for tests and internal use.
    pub fn writer(&self) -> Result<Database> {
        Database::new(&self.db_path)
    }

    /// Get the write lock for serializing write operations
    ///
    /// Use this to prevent FTS index corruption from concurrent writes.
    /// Hold the lock while performing write operations.
    pub fn write_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.write_lock.clone()
    }

    /// The configuration as of now. A caller that needs a consistent view
    /// across several reads takes one clone and reads from that.
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Run on `config` from now on. What was already built from the old one —
    /// the connections, the sync schedule — is not rebuilt; see
    /// [`crate::config::keys_needing_restart`].
    pub fn replace_config(&self, config: Arc<Config>) {
        imap::set_extra_headers(config.extra_header_names());
        if let Err(e) = self
            .writer()
            .and_then(|mut db| db.refetch_headers_for_new_names(&config.extra_header_names()))
        {
            crate::logger::warn(&format!(
                "headers named by the new config not queued for refetch: {e}"
            ));
        }
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    /// Get the database path
    pub fn db_path(&self) -> &str {
        &self.db_path
    }

    /// Get account config by name, with its special folders resolved against
    /// what the server announces; see [`Store::resolve_special_folders`].
    pub fn get_account_config(&self, account_name: &str) -> Option<AccountConfig> {
        self.config()
            .accounts
            .iter()
            .find(|a| a.name == account_name)
            .map(|account| self.resolve_special_folders(account))
    }

    /// The account with its drafts, sent, trash, archive and junk folders as
    /// they are on the server.
    ///
    /// A name the config gives stands when the server has that folder. When
    /// it does not — a default like `Trash` on a server whose trash is
    /// `[Gmail]/Trash` — the folder the server marks for that role (RFC
    /// 6154 `\Trash` and the like) takes its place, so the special folders
    /// need no config on a server that announces them. Before the first
    /// LIST nothing is known, and the names stay as configured.
    pub fn resolve_special_folders(&self, account: &AccountConfig) -> AccountConfig {
        let listed = self.listed_folders(&account.name);
        if listed.is_empty() {
            return account.clone();
        }
        let exists = |name: &str| listed.iter().any(|f| f.name == name);
        let role = |role: &str| {
            listed
                .iter()
                .find(|f| f.special_use.as_deref() == Some(role))
                .map(|f| f.name.clone())
        };
        let mut resolved = account.clone();
        let settle = |name: &mut String, role_name: &str| {
            if !exists(name)
                && let Some(found) = role(role_name)
            {
                *name = found;
            }
        };
        settle(&mut resolved.drafts_folder, "\\Drafts");
        settle(&mut resolved.sent_folder, "\\Sent");
        settle(&mut resolved.trash_folder, "\\Trash");
        for (slot, role_name) in [
            (&mut resolved.archive_folder, "\\Archive"),
            (&mut resolved.junk_folder, "\\Junk"),
        ] {
            match slot {
                Some(name) if exists(name) => {}
                _ => {
                    if let Some(found) = role(role_name) {
                        *slot = Some(found);
                    }
                }
            }
        }
        resolved
    }

    /// Get visible folders for an account, sorted per config, with display info
    ///
    /// Returns FolderDisplay structs containing the folder, display name,
    /// badge settings, and message counts. Pinned folders for this account
    /// appear at the top.
    pub fn get_visible_folders(&self, account_name: &str) -> Result<Vec<FolderDisplay>> {
        let account_config = self
            .get_account_config(account_name)
            .ok_or_else(|| Error::NotFound(format!("Account '{}' not found", account_name)))?;

        let db = self.reader()?;

        // Find pinned folders for this account
        let config = self.config();
        let pinned_for_account: Vec<&PinnedFolder> = config
            .pinned_folders()
            .filter(|p| p.account == account_name)
            .collect();

        let mut result = Vec::new();

        // Add pinned folders first
        for pinned in &pinned_for_account {
            if let Some(folder_display) = self.build_pinned_folder_display(&db, pinned)? {
                result.push(folder_display);
            }
        }

        result.extend(self.account_folder_displays(&db, &account_config)?);

        Ok(result)
    }

    /// Get all visible folders from all accounts with account headers
    ///
    /// This method creates a flat list of FolderDisplay items for multi-account
    /// sidebar display. Pinned folders appear first at the top, then account
    /// headers with their folders (including the pinned folders again).
    ///
    /// # Returns
    ///
    /// A flat Vec containing:
    /// - Pinned folders (at the very top)
    /// - AccountHeader("work")
    /// - FolderDisplay(work/INBOX) -- including pinned folders
    /// - FolderDisplay(work/Sent)
    /// - AccountHeader("personal")
    /// - FolderDisplay(personal/INBOX) -- including pinned folders
    /// - etc.
    pub fn get_all_visible_folders(&self) -> Result<Vec<FolderDisplay>> {
        let db = self.reader()?;
        let mut all_folders = Vec::new();

        // Build pinned folders first
        for pinned in self.config().pinned_folders() {
            if let Some(folder_display) = self.build_pinned_folder_display(&db, pinned)? {
                all_folders.push(folder_display);
            }
        }

        // Build regular account sections (including pinned folders - they appear in both places)
        for account_config in &self.config().accounts {
            // Add account header
            all_folders.push(FolderDisplay::account_header(&account_config.name));

            let resolved = self.resolve_special_folders(account_config);
            all_folders.extend(self.account_folder_displays(&db, &resolved)?);
        }

        Ok(all_folders)
    }

    /// An account's visible folders in the sidebar's order: sorted per the
    /// config, each directly after the folder it sits inside.
    fn account_folder_displays(
        &self,
        db: &Database,
        account_config: &AccountConfig,
    ) -> Result<Vec<FolderDisplay>> {
        let folders = db.get_folders_by_account(&account_config.name)?;
        let mut folder_names: Vec<String> = folders
            .iter()
            .filter(|f| account_config.is_folder_visible(&f.name))
            .map(|f| f.name.clone())
            .collect();
        account_config.sort_folder_names(&mut folder_names);

        let listed = self.listed_folders(&account_config.name);
        let delimiter = listed.iter().find_map(|f| f.delimiter.as_deref());
        let mut result = Vec::new();
        for nested in crate::models::nest_folders(&folder_names, delimiter) {
            let Some(folder) = folders.iter().find(|f| f.name == nested.name) else {
                continue;
            };
            let config = account_config.get_folder_config(&folder.name);
            let (unread, total) = if let Some(id) = folder.id {
                db.get_folder_counts(id)?
            } else {
                (0, 0)
            };
            let mut display = FolderDisplay::new(
                folder.clone(),
                config
                    .display_name
                    .clone()
                    .unwrap_or_else(|| imap::utf7::decode(&folder.name)),
                config.show_unread,
                config.show_total,
                unread,
                total,
            );
            display.is_threaded = account_config.threads_for(&folder.name);
            display.columns = config
                .columns
                .as_deref()
                .map(|names| crate::config::ordered_columns(Some(names)));
            display.role = self
                .config()
                .reading_role(&account_config.name, &folder.name)
                .map(str::to_string)
                .or_else(|| folder_role(account_config, &listed, &folder.name));
            display.sort = account_config.get_folder_sort(&folder.name).into();
            display.depth = nested.depth;
            display.parent = nested.parent;
            display.label = config
                .display_name
                .clone()
                .unwrap_or_else(|| imap::utf7::decode(&nested.leaf));
            result.push(display);
        }
        Ok(result)
    }

    /// Build a FolderDisplay for a pinned folder configuration
    ///
    /// Returns None if the account or folder doesn't exist (silently skipped).
    fn build_pinned_folder_display(
        &self,
        db: &Database,
        pinned: &PinnedFolder,
    ) -> Result<Option<FolderDisplay>> {
        // Get account config
        let account_config = match self.get_account_config(&pinned.account) {
            Some(config) => config,
            None => {
                crate::logger::warn(&format!(
                    "Pinned folder: account '{}' not found, skipping",
                    pinned.account
                ));
                return Ok(None);
            }
        };

        // Get folder from database
        let folder = match db.get_folder_by_name(&pinned.account, &pinned.folder)? {
            Some(f) => f,
            None => {
                crate::logger::warn(&format!(
                    "Pinned folder: folder '{}' not found in account '{}', skipping",
                    pinned.folder, pinned.account
                ));
                return Ok(None);
            }
        };

        // Get folder config for badge settings
        let folder_config = account_config.get_folder_config(&folder.name);

        // Get message counts
        let (unread, total) = if let Some(id) = folder.id {
            db.get_folder_counts(id)?
        } else {
            (0, 0)
        };

        // Determine display name: custom > "Account: Folder" default
        let display_name = pinned
            .display_name
            .clone()
            .unwrap_or_else(|| format!("{}: {}", pinned.account, pinned.folder));

        let role = self
            .config()
            .reading_role(&pinned.account, &folder.name)
            .map(str::to_string)
            .or_else(|| {
                folder_role(
                    &account_config,
                    &self.listed_folders(&pinned.account),
                    &folder.name,
                )
            });
        let mut display = FolderDisplay::pinned(
            folder,
            display_name,
            folder_config.show_unread,
            folder_config.show_total,
            unread,
            total,
        );
        display.role = role;
        display.sort = account_config.get_folder_sort(&pinned.folder).into();
        Ok(Some(display))
    }

    /// Get the sort order for a specific folder
    pub fn get_folder_sort(&self, account_name: &str, folder_name: &str) -> MessageSort {
        self.get_account_config(account_name)
            .map(|config| config.get_folder_sort(folder_name).into())
            .unwrap_or(MessageSort::DateNewest)
    }

    /// Synchronize email accounts and folders
    ///
    /// # Arguments
    ///
    /// * `account_name` - Optional account to sync (None = all accounts)
    /// * `folder_name` - Optional folder to sync (None = all folders)
    /// * `progress_tx` - Optional channel for sending progress updates
    ///
    /// # Behavior
    ///
    /// - If both None: sync all accounts and all folders
    /// - If account_name Some, folder_name None: sync all folders for that account
    /// - If both Some: sync that specific folder only
    pub async fn sync(
        &self,
        account_name: Option<String>,
        folder_name: Option<String>,
        progress_tx: Option<mpsc::UnboundedSender<StateUpdate>>,
        skip_deletion_detection: bool,
    ) -> Result<SyncReport> {
        let mut report = SyncReport::new();

        // Determine which account indices to sync
        let account_indices: Vec<usize> = match &account_name {
            Some(name) => self
                .config()
                .accounts
                .iter()
                .enumerate()
                .filter(|(_, a)| &a.name == name)
                .map(|(i, _)| i)
                .collect(),
            None => (0..self.config().accounts.len()).collect(),
        };

        if account_indices.is_empty() {
            return Err(Error::NotFound(format!(
                "No matching accounts found{}",
                account_name
                    .as_ref()
                    .map(|n| format!(" for '{}'", n))
                    .unwrap_or_default()
            )));
        }

        // Accounts sync concurrently. They are separate servers with separate
        // connections and separate folders; the only thing they share is the
        // database write lock, and a folder's write takes well under a
        // millisecond against round trips of tens. Sequentially the total was
        // the sum of the accounts; now it is the slowest of them.
        let account_syncs = account_indices.into_iter().map(|idx| {
            let account = self.config().accounts[idx].clone();
            let folder_name = folder_name.clone();
            let progress_tx = progress_tx.clone();
            async move {
                let result = self
                    .sync_account(&account, folder_name, progress_tx, skip_deletion_detection)
                    .await;
                (account.name.clone(), result)
            }
        });

        for (name, result) in futures::future::join_all(account_syncs).await {
            match result {
                Ok(account_report) => {
                    report.merge(account_report);
                    report.accounts_synced += 1;
                }
                Err(e) => {
                    report.fail(&name, None, e.kind(), format!("Account '{}': {}", name, e));
                }
            }
        }

        Ok(report)
    }

    /// Sync all folders for a single account
    async fn sync_account(
        &self,
        account: &AccountConfig,
        folder_name: Option<String>,
        progress_tx: Option<mpsc::UnboundedSender<StateUpdate>>,
        skip_deletion_detection: bool,
    ) -> Result<SyncReport> {
        let mut report = SyncReport::new();
        // Shared by everything the filters do in this sync, which is how the
        // review panel tells this sync's work from the last one's.
        let run_at = chrono::Utc::now().to_rfc3339();

        let password = self.credentials.imap(account).await?;
        let pool = self.connection_pool.clone();
        pool.register_account(
            &account.name,
            AccountConnectionConfig::for_account(account, password.clone()),
        )
        .await;

        let mut account_trace = SyncTrace::begin(&account.name, "", 0);
        let connect = account_trace.start("connect", 0);
        let acquire_started = std::time::Instant::now();
        let mut imap_client = match pool.acquire(&account.name, "sync").await {
            Ok(client) => client,
            Err(error) => {
                self.forget_password_if_refused(account, &error).await;
                account_trace.end(connect, 0, None, format!("failed: {error}"));
                account_trace.finish(Err(error.to_string()));
                return Err(error);
            }
        };
        account_trace.connection(imap_client.id());
        // The unrecovered connection error, if any, that leaves the client
        // broken. `None` while the client is healthy, including after a
        // reconnect swapped in a fresh one.
        let mut fatal_connection_error: Option<Error> = None;

        {
            // A connection younger than the acquire was opened by it.
            let opened_now = imap_client.age() <= acquire_started.elapsed();
            account_trace.end(
                connect,
                if opened_now {
                    imap_client.bytes_on_wire()
                } else {
                    0
                },
                None,
                if opened_now {
                    "opened".to_string()
                } else {
                    format!("reused, age {:?}", imap_client.age())
                },
            );
        }

        // Writes made here while the server was out of reach go first, so
        // that what follows compares against a server that has them.
        let outbox = account_trace.start("outbox", imap_client.bytes_on_wire());
        match self.drain_outbox_with(&mut imap_client, account).await {
            Ok(done) => account_trace.end(
                outbox,
                imap_client.bytes_on_wire(),
                Some(done),
                if done == 0 { "nothing queued" } else { "" },
            ),
            Err(error) => {
                account_trace.end(
                    outbox,
                    imap_client.bytes_on_wire(),
                    None,
                    format!("failed: {error}"),
                );
                report.fail(
                    &account.name,
                    None,
                    error.kind(),
                    format!("Outbox: {error}"),
                );
                if error.is_connection_error() {
                    match Self::reconnect_imap(account, &password, "sync, after the outbox failed")
                        .await
                    {
                        Ok(client) => {
                            imap_client = client;
                            account_trace.connection(imap_client.id());
                        }
                        Err(reconnect_error) => {
                            report.fail(
                                &account.name,
                                None,
                                reconnect_error.kind(),
                                format!("Reconnection failed: {reconnect_error}"),
                            );
                            account_trace.finish(Err(error.to_string()));
                            return Ok(report);
                        }
                    }
                }
            }
        }

        if let Some(ref tx) = progress_tx {
            tx.send(StateUpdate::SyncProgress {
                account: account.name.clone(),
                stage: SyncStage::FetchingFolders,
                progress: 0.2,
            })
            .ok();
        }

        // Determine which folders to sync
        let folders_to_sync: Vec<String> = match folder_name {
            Some(name) => {
                account_trace.finish(Ok(()));
                if account.should_sync_folder(&name) {
                    vec![name]
                } else {
                    vec![]
                }
            }
            None => {
                let list = account_trace.start("list", imap_client.bytes_on_wire());
                let listed = imap_client.list_folders_detailed().await;
                account_trace.end(
                    list,
                    imap_client.bytes_on_wire(),
                    listed.as_ref().ok().map(Vec::len),
                    match &listed {
                        Ok(_) => String::new(),
                        Err(e) => format!("failed: {e}"),
                    },
                );
                account_trace.finish(Ok(()));
                // What the server has is what Orb has: the roles it announces
                // are recorded, and a folder it no longer lists is forgotten
                // here rather than left as a ghost.
                if let Ok(folders) = &listed
                    && !folders.is_empty()
                    && let Err(error) = self.record_listing(&account.name, folders).await
                {
                    crate::logger::warn(&format!(
                        "{}: could not record the folder list: {error}",
                        account.name
                    ));
                }
                let listed_ok = listed.is_ok();
                let delimiter = listed
                    .as_ref()
                    .ok()
                    .and_then(|folders| folders.iter().find_map(|f| f.delimiter.clone()));
                let mut all_folders = match listed.map(|folders| {
                    folders
                        .into_iter()
                        .map(|folder| folder.name)
                        .collect::<Vec<String>>()
                }) {
                    Ok(folders) if !folders.is_empty() => folders,
                    Ok(_) => {
                        report.fail(
                            &account.name,
                            None,
                            FailureKind::Server,
                            "No folders found on server".to_string(),
                        );
                        pool.release(&account.name, imap_client).await;
                        return Ok(report);
                    }
                    Err(e) => {
                        // Fall back to the folders the database knows.
                        report.fail(
                            &account.name,
                            None,
                            e.kind(),
                            format!("Failed to list folders from server: {}", e),
                        );
                        let db_reader = self.reader()?;
                        let known_folders = db_reader.get_folders_by_account(&account.name)?;
                        if known_folders.is_empty() {
                            report.fail(
                                &account.name,
                                None,
                                FailureKind::Other,
                                "No folders in database to fall back to".to_string(),
                            );
                            pool.give_back(&account.name, imap_client, Some(&e)).await;
                            return Ok(report);
                        }
                        // If the list failure means the connection is broken,
                        // remember it; the per-folder retry below reconnects
                        // and clears the flag on the first folder that syncs.
                        if Self::is_broken_connection_error(&e) {
                            fatal_connection_error = Some(e);
                        }
                        known_folders.into_iter().map(|f| f.name).collect()
                    }
                };
                // Only against what the server listed: the database's
                // fallback list says nothing about what the server lacks.
                let created = if listed_ok {
                    self.ensure_reading_folders(
                        &mut imap_client,
                        &account.name,
                        delimiter.as_deref(),
                        &all_folders,
                    )
                    .await
                } else {
                    Vec::new()
                };
                if !created.is_empty()
                    && let Ok(folders) = imap_client.list_folders_detailed().await
                    && let Err(error) = self.record_listing(&account.name, &folders).await
                {
                    crate::logger::warn(&format!(
                        "{}: could not record the folder list: {error}",
                        account.name
                    ));
                }
                all_folders.extend(created);
                // INBOX first: it is what the window shows at launch, and a
                // large archive ahead of it in the listing kept it stale.
                let mut folders: Vec<String> = all_folders
                    .into_iter()
                    .filter(|name| account.should_sync_folder(name))
                    .collect();
                folders.sort_by_key(|name| !name.eq_ignore_ascii_case("INBOX"));
                folders
            }
        };

        // The write lock (prevents FTS corruption) is NOT held for the whole
        // sync. It is taken for the duration of each individual DB write
        // inside the sync loop (see `Self::locked_write`) and released during
        // network I/O, so user actions stay responsive while a sync runs. The
        // per-folder optimistic `sync_version` check makes the interleaving
        // safe: a folder modified during the fetch is retried from a fresh
        // baseline.
        let write_lock = self.write_lock.clone();
        let mut db = Database::new(&self.db_path)?;

        let total_folders = folders_to_sync.len();
        for (idx, folder_name) in folders_to_sync.iter().enumerate() {
            if let Some(ref tx) = progress_tx {
                let progress = 0.2 + (0.7 * (idx as f32 / total_folders as f32));
                tx.send(StateUpdate::SyncProgress {
                    account: account.name.clone(),
                    stage: SyncStage::SyncingFolder {
                        name: folder_name.clone(),
                        current: None,
                        total: None,
                    },
                    progress,
                })
                .ok();
            }

            let mut trace = SyncTrace::begin(&account.name, folder_name, imap_client.id());

            // Ask the folder whether anything happened before opening it. One
            // round trip against the two that SELECT and FETCH cost, and for a
            // folder that has not changed — most of them, most of the time —
            // it replaces them entirely.
            let status = trace.start("status", imap_client.bytes_on_wire());
            let state = imap_client.folder_state(folder_name).await;
            trace.end(
                status,
                imap_client.bytes_on_wire(),
                None,
                match &state {
                    Ok(state) => format!(
                        "messages={} uidnext={} highestmodseq={}",
                        state.messages,
                        state.uid_next.unwrap_or(0),
                        state.highest_modseq.unwrap_or(0)
                    ),
                    Err(e) => format!("failed: {e}"),
                },
            );
            match state {
                // The connection is gone. Replace it now rather than let the
                // SELECT below spend another timeout finding that out.
                Err(e) if e.is_connection_error() => {
                    match Self::reconnect_imap(
                        account,
                        &password,
                        &format!("sync, after STATUS of {folder_name} failed"),
                    )
                    .await
                    {
                        Ok(client) => {
                            imap_client = client;
                            trace.connection(imap_client.id());
                            fatal_connection_error = None;
                        }
                        Err(reconnect_err) => {
                            trace.finish(Err(e.to_string()));
                            report.fail(
                                &account.name,
                                Some(folder_name),
                                reconnect_err.kind(),
                                format!(
                                    "Folder '{folder_name}': {e}; reconnection failed: \
                                     {reconnect_err} - skipping remaining folders"
                                ),
                            );
                            fatal_connection_error = Some(e);
                            break;
                        }
                    }
                }
                // The server's own answer — a NO for a folder it does not
                // have — is final, and the folder must not be created locally
                // on the strength of a name in the configuration.
                Err(e) => {
                    trace.finish(Err(e.to_string()));
                    report.fail(
                        &account.name,
                        Some(folder_name),
                        e.kind(),
                        format!("Folder '{folder_name}': {e}"),
                    );
                    continue;
                }
                Ok(state) => {
                    let stored = self
                        .reader()
                        .and_then(|db| db.get_folder_by_name(&account.name, folder_name));
                    if let Ok(Some(stored)) = stored
                        && let Some(folder_id) = stored.id
                        && let Ok(count) =
                            self.reader().and_then(|db| db.get_message_count(folder_id))
                        && Self::folder_is_unchanged(&stored, count, &state)
                    {
                        // "Unchanged" is decided from UIDVALIDITY, the highest
                        // UID, the modseq and the message count — and a flag
                        // changes none of them on a server whose modseq does
                        // not move. So the flag check runs from here too, on
                        // its own schedule, before the folder is waved through.
                        trace.note(
                            "unchanged",
                            "uidvalidity, uidnext, count and modseq all match; nothing to fetch",
                        );
                        if db
                            .flags_check_is_due(folder_id, Self::FLAG_CHECK_INTERVAL)
                            .unwrap_or(false)
                            && imap_client.select_mailbox(folder_name).await.is_ok()
                        {
                            match Self::reconcile_flags(
                                &mut db,
                                &write_lock,
                                &mut imap_client,
                                &account.name,
                                folder_name,
                                folder_id,
                                &mut trace,
                            )
                            .await
                            {
                                Ok(corrected) => report.messages_updated += corrected,
                                Err(e) => crate::logger::warn(&format!(
                                    "sync[{}/{folder_name}]: flag check failed: {e}",
                                    account.name
                                )),
                            }
                        }
                        report.folders_synced += 1;
                        trace.finish(Ok(()));
                        if let Err(error) = self
                            .filter_after_sync(
                                &mut imap_client,
                                account,
                                &password,
                                folder_name,
                                &run_at,
                                &mut report,
                            )
                            .await
                        {
                            fatal_connection_error = Some(error);
                            break;
                        }
                        continue;
                    }
                }
            }

            if let Err(error) = self
                .begin_filter_watermark(&account.name, folder_name)
                .await
            {
                crate::logger::warn(&format!(
                    "filters: {}/{folder_name}: no watermark: {error}",
                    account.name
                ));
            }
            let folder_result = Self::sync_folder_with_retry(
                &mut db,
                &self.db_path,
                &mut imap_client,
                account,
                folder_name,
                &password,
                progress_tx.clone(),
                skip_deletion_detection,
                &write_lock,
                &mut trace,
            )
            .await;

            match folder_result {
                Ok((outcome, new_client)) => {
                    trace.finish(Ok(()));
                    if let Some(client) = new_client {
                        imap_client = client;
                    }
                    // A folder synced on whatever client we now hold, so it is
                    // proven healthy again.
                    fatal_connection_error = None;
                    match outcome {
                        FolderOutcome::Synced(folder_report) => {
                            report.folders_synced += 1;
                            report.messages_added += folder_report.messages_added;
                            report.messages_updated += folder_report.messages_updated;
                            report.messages_deleted += folder_report.messages_deleted;
                            if let Err(error) = self
                                .filter_after_sync(
                                    &mut imap_client,
                                    account,
                                    &password,
                                    folder_name,
                                    &run_at,
                                    &mut report,
                                )
                                .await
                            {
                                fatal_connection_error = Some(error);
                                break;
                            }
                            match self.new_mail_among(
                                account,
                                folder_name,
                                &folder_report.added_ids,
                            ) {
                                Ok(mail) => report.new_mail.extend(mail),
                                Err(error) => crate::logger::warn(&format!(
                                    "sync[{}/{folder_name}]: the new mail could not be read: {error}",
                                    account.name
                                )),
                            }
                        }
                        FolderOutcome::Skipped { holder } => {
                            report.folders_skipped += 1;
                            report
                                .skipped
                                .push(format!("{folder_name}: locked by {holder}"));
                        }
                    }
                }
                Err(e) => {
                    trace.finish(Err(e.to_string()));
                    report.fail(
                        &account.name,
                        Some(folder_name),
                        e.kind(),
                        format!("Folder '{}': {}", folder_name, e),
                    );
                    // `sync_folder_with_retry` reports an exhausted connection
                    // failure as `RetryExhausted` or as the raw connection
                    // error; either way the client we still hold is dead.
                    if Self::is_broken_connection_error(&e) {
                        match Self::reconnect_imap(
                            account,
                            &password,
                            &format!("sync, after an error in {folder_name}"),
                        )
                        .await
                        {
                            Ok(new_client) => {
                                imap_client = new_client;
                                fatal_connection_error = None;
                                crate::logger::info(&format!(
                                    "Reconnected after error in folder '{}'",
                                    folder_name
                                ));
                            }
                            Err(reconnect_err) => {
                                report.fail(
                                    &account.name,
                                    Some(folder_name),
                                    reconnect_err.kind(),
                                    format!(
                                        "Reconnection failed: {} - skipping remaining folders",
                                        reconnect_err
                                    ),
                                );
                                // The original error says why the client is
                                // broken; `reconnect_err` describes a client
                                // that was never pooled.
                                fatal_connection_error = Some(e);
                                break;
                            }
                        }
                    }
                }
            }
        }

        if Self::should_release_to_pool(fatal_connection_error.as_ref()) {
            pool.release(&account.name, imap_client).await;
        } else {
            drop(imap_client);
        }

        Ok(report)
    }

    /// Decide whether an IMAP client should be returned to the connection pool
    /// after a sync operation finishes.
    ///
    /// `fatal_connection_error` is the unrecovered connection/IO error (if any)
    /// that the client hit and that we could not reconnect around. A healthy
    /// client (no such error) is returned for reuse; a client left in a broken
    /// or unknown state is dropped so it is never handed to the next operation.
    fn should_release_to_pool(fatal_connection_error: Option<&Error>) -> bool {
        match fatal_connection_error {
            None => true,
            Some(err) => !Self::is_broken_connection_error(err),
        }
    }

    /// Returns true if `err` means the IMAP client that produced it is left in a
    /// broken or unknown state and must therefore be dropped rather than handed
    /// back to the pool.
    ///
    /// This is broader than [`Error::is_connection_error`] because the per-folder
    /// retry loop wraps an exhausted connection failure in
    /// [`Error::RetryExhausted`] (whose own `is_connection_error()` is `false`).
    /// After retries and reconnects are exhausted the client we still hold is the
    /// dead one, so `RetryExhausted` must also count as broken — otherwise the
    /// broken connection is silently re-pooled and handed to the next operation.
    fn is_broken_connection_error(err: &Error) -> bool {
        err.is_connection_error() || matches!(err, Error::RetryExhausted { .. })
    }

    /// A fresh connection for a sync whose connection failed it.
    async fn reconnect_imap(
        account: &AccountConfig,
        password: &secrecy::SecretString,
        reason: &str,
    ) -> Result<ImapClient> {
        ImapClient::open(
            &imap::ConnectionSettings::for_account(account, password.clone()),
            reason,
            "direct",
        )
        .await
    }

    /// A refused login means the cached password is wrong, or was: forget it,
    /// so that the next attempt runs the command again rather than failing
    /// forever on a password that was changed elsewhere.
    async fn forget_password_if_refused(&self, account: &AccountConfig, error: &Error) {
        if matches!(error, Error::AuthenticationFailed { .. }) {
            self.credentials.forget_imap(&account.name).await;
        }
    }

    /// Run a single database write while holding the global write lock for the
    /// minimum possible duration.
    ///
    /// The write lock only exists to serialize concurrent DB *writes* (to avoid
    /// FTS index corruption). By acquiring it here around one short, synchronous
    /// DB operation and releasing it as soon as the closure returns, the lock is
    /// free during the surrounding network I/O, so unrelated user write actions
    /// are not blocked for the whole duration of a sync.
    async fn locked_write<T>(
        write_lock: &Arc<tokio::sync::Mutex<()>>,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let _guard = write_lock.lock().await;
        f()
    }

    /// Fetch the mail that has just arrived whole, on the connection that
    /// still has its folder selected, so that it opens without waiting on the
    /// server — it is the mail most likely to be opened next. What does not
    /// fit [`ARRIVED_PREFETCH_MESSAGES`] and [`ARRIVED_PREFETCH_BYTES`] is
    /// left to the background pass.
    async fn prefetch_arrived(
        db: &mut Database,
        write_lock: &Arc<tokio::sync::Mutex<()>>,
        imap_client: &mut ImapClient,
        folder_id: i64,
        arrived: &[(u32, Option<u32>)],
        trace: &mut SyncTrace,
    ) -> Result<()> {
        let mut newest_first = arrived.to_vec();
        newest_first.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
        let mut room = ARRIVED_PREFETCH_BYTES;
        let uids: Vec<u32> = newest_first
            .into_iter()
            .take(ARRIVED_PREFETCH_MESSAGES)
            .filter_map(|(uid, size)| {
                let size = size.map_or(usize::MAX, |size| size as usize);
                (size <= room).then(|| {
                    room -= size;
                    uid
                })
            })
            .collect();
        if uids.is_empty() {
            return Ok(());
        }

        let step = trace.start("prefetch", imap_client.bytes_on_wire());
        let sources = match imap_client.fetch_bodies(&uids).await {
            Ok(sources) => sources,
            Err(e) => {
                trace.end(
                    step,
                    imap_client.bytes_on_wire(),
                    None,
                    format!("failed: {e}"),
                );
                return Err(e);
            }
        };
        let received = sources.len();
        let stored = Self::locked_write(write_lock, move || {
            let ids = db.ids_for_uids(folder_id, &uids)?;
            let mut stored = 0;
            for (uid, source) in &sources {
                // Held back for a queued move, or gone meanwhile.
                let Some(&id) = ids.get(uid) else { continue };
                db.update_message_body(
                    id,
                    crate::imap::extract_body_text(source).as_deref(),
                    crate::imap::extract_body_html(source).as_deref(),
                    Some(source),
                )?;
                stored += 1;
            }
            Ok(stored)
        })
        .await;
        trace.end(
            step,
            imap_client.bytes_on_wire(),
            Some(received),
            match &stored {
                Ok(stored) => format!("{stored} of {} arrived", arrived.len()),
                Err(e) => format!("not stored: {e}"),
            },
        );
        stored.map(|_| ())
    }

    /// Sync a single folder, retrying what is worth retrying.
    ///
    /// Only a connection error gets a new connection; database contention
    /// leaves the one we hold perfectly good, and reconnecting for it cost a
    /// TLS handshake and a login per retry. A concurrency conflict that
    /// outlasted its own retries inside [`Self::sync_folder_impl`] is not
    /// retried again here: the folder is being written to constantly, and the
    /// next sync will find a quieter moment.
    ///
    /// Returns the outcome and the client to continue with, when a reconnect
    /// replaced the one passed in.
    #[allow(clippy::too_many_arguments)]
    async fn sync_folder_with_retry(
        db: &mut Database,
        db_path: &str,
        imap_client: &mut ImapClient,
        account: &AccountConfig,
        folder_name: &str,
        password: &secrecy::SecretString,
        progress_tx: Option<mpsc::UnboundedSender<StateUpdate>>,
        skip_deletion_detection: bool,
        write_lock: &Arc<tokio::sync::Mutex<()>>,
        trace: &mut SyncTrace,
    ) -> Result<(FolderOutcome, Option<ImapClient>)> {
        let mut delay_ms = SYNC_RETRY_INITIAL_DELAY_MS;
        let mut new_client: Option<ImapClient> = None;

        for attempt in 1..=MAX_SYNC_RETRY_ATTEMPTS {
            let client = new_client.as_mut().unwrap_or(imap_client);

            let error = match Self::sync_folder(
                db,
                db_path,
                client,
                &account.name,
                folder_name,
                progress_tx.clone(),
                skip_deletion_detection,
                write_lock,
                trace,
            )
            .await
            {
                Ok(outcome) => return Ok((outcome, new_client)),
                Err(e) => e,
            };

            if !error.is_retryable() || matches!(error, Error::ConcurrencyConflict { .. }) {
                return Err(error);
            }
            let error_msg = error.to_string();
            if attempt >= MAX_SYNC_RETRY_ATTEMPTS {
                return Err(Error::RetryExhausted {
                    operation: format!("sync folder '{}'", folder_name),
                    attempts: MAX_SYNC_RETRY_ATTEMPTS,
                    last_error: error_msg,
                });
            }

            crate::logger::warn(&format!(
                "Folder '{}' sync attempt {}/{} failed (retryable): {}. Retrying in {}ms...",
                folder_name, attempt, MAX_SYNC_RETRY_ATTEMPTS, error_msg, delay_ms
            ));
            trace.note(
                "retry",
                format!(
                    "attempt {attempt}/{MAX_SYNC_RETRY_ATTEMPTS} failed: {error_msg}; \
                     waiting {delay_ms}ms{}",
                    if error.is_connection_error() {
                        ", then reconnecting"
                    } else {
                        ""
                    }
                ),
            );
            if let Some(ref tx) = progress_tx {
                tx.send(StateUpdate::RetryAttempt {
                    operation: format!("sync folder '{}'", folder_name),
                    attempt,
                    max_attempts: MAX_SYNC_RETRY_ATTEMPTS,
                    next_delay_ms: delay_ms,
                    error: error_msg,
                })
                .ok();
            }

            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            let base_delay = (delay_ms * 2).min(SYNC_RETRY_MAX_DELAY_MS);
            let jitter = rand::Rng::gen_range(&mut rand::thread_rng(), 0..=base_delay / 4);
            delay_ms = (base_delay + jitter).min(SYNC_RETRY_MAX_DELAY_MS);

            if error.is_connection_error() {
                match Self::reconnect_imap(
                    account,
                    password,
                    &format!("sync, retry {} of {folder_name}", attempt + 1),
                )
                .await
                {
                    Ok(client) => {
                        trace.connection(client.id());
                        new_client = Some(client);
                        crate::logger::info(&format!(
                            "Reconnected for retry of folder '{}'",
                            folder_name
                        ));
                    }
                    Err(reconnect_err) => {
                        crate::logger::warn(&format!(
                            "Reconnection failed during retry: {}",
                            reconnect_err
                        ));
                    }
                }
            }
        }

        Err(Error::RetryExhausted {
            operation: format!("sync folder '{}'", folder_name),
            attempts: MAX_SYNC_RETRY_ATTEMPTS,
            last_error: "no attempt was made".to_string(),
        })
    }

    /// Sync a single folder under its lock.
    ///
    /// The lock keeps two syncs, in this process or in another, from working
    /// on the same folder at once. A folder whose lock is held is skipped and
    /// reported as such, with the holder; it is not synced, and nothing here
    /// pretends otherwise. The lock is renewed on a timer for as long as the
    /// sync runs, so a fetch longer than the stale cutoff does not hand the
    /// folder to another instance mid-write.
    #[allow(clippy::too_many_arguments)]
    async fn sync_folder(
        db: &mut Database,
        db_path: &str,
        imap_client: &mut ImapClient,
        account_name: &str,
        folder_name: &str,
        progress_tx: Option<mpsc::UnboundedSender<StateUpdate>>,
        skip_deletion_detection: bool,
        write_lock: &Arc<tokio::sync::Mutex<()>>,
        trace: &mut SyncTrace,
    ) -> Result<FolderOutcome> {
        let attempt = Self::locked_write(write_lock, || {
            db.acquire_sync_lock(account_name, folder_name)
        })
        .await?;
        if let LockAttempt::Held { holder } = attempt {
            trace.skipped(format!("locked by {holder}"));
            return Ok(FolderOutcome::Skipped { holder });
        }

        let renewer = LockRenewer::start(
            db_path.to_string(),
            write_lock.clone(),
            account_name.to_string(),
            folder_name.to_string(),
        );
        let sync_result = Self::sync_folder_impl(
            db,
            imap_client,
            account_name,
            folder_name,
            progress_tx,
            skip_deletion_detection,
            write_lock,
            trace,
        )
        .await;
        drop(renewer);

        let _ = Self::locked_write(write_lock, || {
            db.release_sync_lock(account_name, folder_name)
        })
        .await;

        sync_result.map(FolderOutcome::Synced)
    }

    /// Internal implementation of sync_folder (called after acquiring lock)
    ///
    /// Uses the two-transaction pattern for multi-instance safety: read the
    /// baseline state, do the IMAP work outside any transaction, then write
    /// the changes atomically with the baseline's `sync_version` validated.
    /// A first sync is paged, and every page is its own such write.
    #[allow(clippy::too_many_arguments)]
    async fn sync_folder_impl(
        db: &mut Database,
        imap_client: &mut ImapClient,
        account_name: &str,
        folder_name: &str,
        progress_tx: Option<mpsc::UnboundedSender<StateUpdate>>,
        skip_deletion_detection: bool,
        write_lock: &Arc<tokio::sync::Mutex<()>>,
        trace: &mut SyncTrace,
    ) -> Result<FolderSyncReport> {
        // SELECT before anything is written: a folder the server does not have
        // must not come into being locally because a configuration named it.
        let select = trace.start("select", imap_client.bytes_on_wire());
        let selected = imap_client.select_mailbox(folder_name).await;
        trace.end(
            select,
            imap_client.bytes_on_wire(),
            None,
            match (&selected, imap_client.current_mailbox()) {
                (Ok(()), Some(info)) => format!(
                    "exists={} uidnext={} uidvalidity={} highestmodseq={}",
                    info.exists,
                    info.uid_next.unwrap_or(0),
                    info.uid_validity.unwrap_or(0),
                    info.highest_modseq.unwrap_or(0)
                ),
                (Ok(()), None) => "no mailbox state".to_string(),
                (Err(e), _) => format!("failed: {e}"),
            },
        );
        selected?;
        let mailbox = imap_client
            .current_mailbox()
            .cloned()
            .ok_or_else(|| Error::Sync(format!("{folder_name}: SELECT answered without state")))?;
        let Some(server_uidvalidity) = mailbox.uid_validity else {
            return Err(Error::Sync(format!(
                "{folder_name}: the server did not report UIDVALIDITY"
            )));
        };
        let snapshot_modseq = mailbox.highest_modseq.unwrap_or(0);
        let exists = mailbox.exists;

        let folder_id = Self::locked_write(write_lock, || {
            db.get_or_create_folder(account_name, folder_name)
        })
        .await?;

        let progress = |current: usize, total: usize| {
            if let Some(tx) = &progress_tx {
                tx.send(StateUpdate::SyncProgress {
                    account: account_name.to_string(),
                    stage: SyncStage::SyncingFolder {
                        name: folder_name.to_string(),
                        current: Some(current),
                        total: Some(total),
                    },
                    progress: if total > 0 {
                        current as f32 / total as f32
                    } else {
                        0.0
                    },
                })
                .ok();
            }
        };

        // Retry loop for optimistic concurrency conflicts
        'attempts: for attempt in 0..MAX_RETRY_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            // Transaction 1: the baseline, read atomically with the stored
            // UIDs so deletion detection compares against the same moment.
            let baseline = trace.start("baseline", 0);
            let (folder, mut baseline_version, mut stored_uids) =
                Self::locked_write(write_lock, || {
                    db.transaction(|tx| {
                        let folder = tx.query_row(
                            "SELECT id, account_name, name, uidvalidity, highest_uid, \
                             highest_modseq, last_sync_time, sync_version
                             FROM folders WHERE id = ?1",
                            params![folder_id],
                            |row| {
                                Ok(Folder {
                                    id: Some(row.get(0)?),
                                    account_name: row.get(1)?,
                                    name: row.get(2)?,
                                    uidvalidity: row.get(3)?,
                                    highest_uid: row.get(4)?,
                                    highest_modseq: row.get(5)?,
                                    last_sync_time: row.get(6)?,
                                    sync_version: row.get(7)?,
                                })
                            },
                        )?;
                        let sync_version = folder.sync_version;
                        let uids = Database::get_folder_uids_in_tx(tx, folder_id)?;
                        Ok((folder, sync_version, uids))
                    })
                })
                .await?;
            trace.end(
                baseline,
                0,
                Some(stored_uids.len()),
                format!(
                    "sync_version={baseline_version} modseq={} uidvalidity={}",
                    folder.highest_modseq.unwrap_or(0),
                    folder.uidvalidity.unwrap_or(0)
                ),
            );

            // A changed UIDVALIDITY voids every stored UID. The folder is
            // cleared in a transaction of its own, so that the paged refetch
            // below starts from an empty folder whose version it knows.
            let uidvalidity_changed =
                Self::uidvalidity_mismatch(folder.uidvalidity, Some(server_uidvalidity));
            if uidvalidity_changed {
                trace.note(
                    "uidvalidity",
                    format!(
                        "changed {} -> {server_uidvalidity}; every stored UID is void, clearing \
                         the folder and refetching all",
                        folder.uidvalidity.unwrap_or(0)
                    ),
                );
                let cleared = Self::locked_write(write_lock, || {
                    db.transaction(|tx| {
                        Self::check_version(tx, folder_id, baseline_version, folder_name)?;
                        tx.execute(
                            "DELETE FROM messages WHERE folder_id = ?1",
                            params![folder_id],
                        )?;
                        Database::maintain_pending_actions_after_sync_in_tx(
                            tx,
                            account_name,
                            folder_name,
                            folder_id,
                            true,
                        )?;
                        tx.execute(
                            "UPDATE folders SET uidvalidity = NULL, highest_uid = NULL, \
                             highest_modseq = NULL, last_filtered_uid = NULL, \
                             sync_version = sync_version + 1 \
                             WHERE id = ?1",
                            params![folder_id],
                        )?;
                        Ok(baseline_version + 1)
                    })
                })
                .await;
                match cleared {
                    Ok(version) => baseline_version = version,
                    Err(Error::ConcurrencyConflict { .. }) if attempt < MAX_RETRY_ATTEMPTS - 1 => {
                        continue 'attempts;
                    }
                    Err(e) => return Err(e),
                }
                stored_uids.clear();
            }

            // Without CONDSTORE there is no "what changed since": the folder
            // is listed and compared, the way a first sync is, every time.
            let modseq = if uidvalidity_changed || !imap_client.supports_condstore() {
                None
            } else {
                folder.highest_modseq
            };
            // A baseline is a completed sync, whatever the server can be
            // asked since then.
            let has_baseline = !uidvalidity_changed && folder.highest_modseq.is_some();
            // The highest UID actually held, which is what the UIDNEXT
            // cross-check compares against. Deliberately not
            // `folder.highest_uid`: that is derived from the server's UIDNEXT,
            // so comparing it with UIDNEXT would compare the server against
            // itself and never report a gap.
            let stored_max_uid = stored_uids.iter().copied().max();
            let stored_set: HashSet<u32> = stored_uids.iter().copied().collect();

            // Messages still to be written by the final transaction. A first
            // sync commits its pages as it goes and leaves nothing here.
            let mut messages: Vec<crate::models::Message> = Vec::new();
            let mut committed = FolderSyncReport::default();
            // The server's UIDs, when the fetch listed them: what the deletion
            // check compares against, rather than listing a second time.
            let mut listed_server_uids: Option<Vec<u32>> = None;

            if exists == 0 {
                trace.note("fetch", "mailbox empty, nothing fetched");
            } else if let Some(modseq) = modseq {
                // Incremental: what changed since the last snapshot.
                let fetch = trace.start("fetch", imap_client.bytes_on_wire());
                let total = exists as usize;
                let mut on_progress = |received: usize| progress(received, total);
                let fetched = imap_client
                    .fetch_changed_since(folder_id, modseq, Some(&mut on_progress))
                    .await;
                trace.end(
                    fetch,
                    imap_client.bytes_on_wire(),
                    fetched.as_ref().ok().map(Vec::len),
                    match &fetched {
                        Ok(_) => format!("changedsince={modseq} of {exists}"),
                        Err(e) => format!("failed: {e}"),
                    },
                );
                messages = fetched?;
                progress(messages.len(), total);

                // Safety net against a server whose MODSEQ does not advance:
                // UIDNEXT says which UIDs exist, and it came with the SELECT.
                // Only when CHANGEDSINCE brought nothing from the gap — when
                // it did, the gap is already in hand and asking again would
                // fetch the same mail twice.
                if let Some(uid_next) = mailbox.uid_next
                    && let Some(first_missing) = imap::uid_gap(stored_max_uid, uid_next)
                {
                    let newest_fetched = messages.iter().map(|m| m.uid).max();
                    if newest_fetched.is_none_or(|uid| uid < first_missing) {
                        let fetched: HashSet<u32> = messages.iter().map(|m| m.uid).collect();
                        let gap = trace.start("uidnext-gap", imap_client.bytes_on_wire());
                        let recovered =
                            imap_client.fetch_from_uid(folder_id, first_missing).await?;
                        trace.end(
                            gap,
                            imap_client.bytes_on_wire(),
                            Some(recovered.len()),
                            format!("from uid {first_missing}, uidnext {uid_next}"),
                        );
                        let before = messages.len();
                        let highest_seen = stored_max_uid.unwrap_or(0);
                        messages.extend(recovered.into_iter().filter(|message| {
                            // `X:*` always includes the last message, even when
                            // X is above it, so the newest stored message comes
                            // back whenever the mail above it has been moved
                            // away.
                            message.uid > highest_seen && !fetched.contains(&message.uid)
                        }));
                        if messages.len() > before {
                            crate::logger::warn(&format!(
                                "{folder_name}: MODSEQ reported no change, but UIDNEXT {uid_next} \
                                 implied unseen mail from UID {first_missing}; recovered {} \
                                 message(s). The server's CONDSTORE counter cannot be relied on \
                                 for this mailbox.",
                                messages.len() - before
                            ));
                        }
                    } else {
                        trace.note("uidnext-gap", "covered by changedsince");
                    }
                }
            } else {
                // First sync: list what is there, then fetch it in pages,
                // committing each page before fetching the next. A folder of
                // two hundred thousand messages neither holds every envelope
                // in memory nor holds one write for minutes, and shows its
                // first page while the rest is still arriving. Stored UIDs are
                // skipped, so a sync interrupted here resumes where it stopped.
                let listing = trace.start("uid-listing", imap_client.bytes_on_wire());
                let server_uids = imap_client.fetch_all_uids().await;
                trace.end(
                    listing,
                    imap_client.bytes_on_wire(),
                    server_uids.as_ref().ok().map(Vec::len),
                    match &server_uids {
                        Ok(_) => format!("of {exists}, {} already stored", stored_uids.len()),
                        Err(e) => format!("failed: {e}"),
                    },
                );
                let server_uids = server_uids?;
                if has_baseline {
                    listed_server_uids = Some(server_uids.clone());
                }
                if server_uids.is_empty() {
                    // Not a legitimate outcome, and recording it would leave an
                    // empty folder that reports success.
                    return Err(Error::Sync(format!(
                        "{folder_name}: the server reports {exists} messages but listed none; \
                         refusing to record that as an empty folder"
                    )));
                }
                let to_fetch: Vec<u32> = server_uids
                    .iter()
                    .copied()
                    .filter(|uid| !stored_set.contains(uid))
                    .collect();
                let total = to_fetch.len();
                let mut fetched_so_far = 0usize;
                let fetch = trace.start("fetch", imap_client.bytes_on_wire());
                for page in to_fetch.chunks(INITIAL_FETCH_PAGE) {
                    let mut on_progress =
                        |received: usize| progress(fetched_so_far + received, total);
                    let page_messages = imap_client
                        .fetch_uids_with_progress(folder_id, page, Some(&mut on_progress))
                        .await;
                    let page_messages = match page_messages {
                        Ok(page_messages) => page_messages,
                        Err(e) => {
                            trace.end(
                                fetch,
                                imap_client.bytes_on_wire(),
                                Some(fetched_so_far),
                                format!("failed after {fetched_so_far} of {total}: {e}"),
                            );
                            return Err(e);
                        }
                    };
                    fetched_so_far += page.len();
                    match Self::commit_page(
                        db,
                        write_lock,
                        folder_id,
                        baseline_version,
                        server_uidvalidity,
                        &page_messages,
                    )
                    .await
                    {
                        Ok((version, page_report)) => {
                            baseline_version = version;
                            committed.add(&page_report);
                        }
                        Err(Error::ConcurrencyConflict { .. })
                            if attempt < MAX_RETRY_ATTEMPTS - 1 =>
                        {
                            trace.end(
                                fetch,
                                imap_client.bytes_on_wire(),
                                Some(fetched_so_far),
                                format!(
                                    "sync_version moved after {fetched_so_far} of {total}; attempt \
                                     {} of {} starts over",
                                    attempt + 2,
                                    MAX_RETRY_ATTEMPTS
                                ),
                            );
                            continue 'attempts;
                        }
                        Err(e) => {
                            trace.end(
                                fetch,
                                imap_client.bytes_on_wire(),
                                Some(fetched_so_far),
                                format!("write failed after {fetched_so_far} of {total}: {e}"),
                            );
                            return Err(e);
                        }
                    }
                    progress(fetched_so_far, total);
                }
                trace.end(
                    fetch,
                    imap_client.bytes_on_wire(),
                    Some(fetched_so_far),
                    format!(
                        "full, {total} of {} listed, {} page(s) committed, added={} updated={}",
                        server_uids.len(),
                        to_fetch.len().div_ceil(INITIAL_FETCH_PAGE.max(1)),
                        committed.messages_added,
                        committed.messages_updated
                    ),
                );
            }

            // Detect deletions (IMAP operation - outside transaction), against
            // the stored UIDs of Transaction 1. Skipped on a first sync (there
            // is nothing to compare), after a UIDVALIDITY change (the folder
            // was just refetched whole), when the modseq says nothing moved,
            // and when the caller knows there is nothing to find.
            let newly_fetched = committed.messages_added
                + messages
                    .iter()
                    .filter(|m| !stored_set.contains(&m.uid))
                    .count();
            let modseq_unchanged = Self::can_skip_deletion_detection_on_unchanged_modseq(
                imap_client.supports_qresync(),
                modseq,
                snapshot_modseq,
                messages.is_empty(),
                stored_uids.len(),
                Some(exists),
            );
            let deletion_plan = Self::plan_deletion_detection(
                has_baseline,
                uidvalidity_changed,
                modseq_unchanged,
                skip_deletion_detection,
            );

            let reconciliation = if deletion_plan.should_run() {
                if let Some(ref tx) = progress_tx {
                    tx.send(StateUpdate::SyncProgress {
                        account: account_name.to_string(),
                        stage: SyncStage::ProcessingDeletions,
                        progress: 0.9,
                    })
                    .ok();
                }
                Self::detect_deletions_with_stored_uids(
                    imap_client,
                    folder_name,
                    folder.uidvalidity,
                    modseq,
                    &stored_uids,
                    newly_fetched,
                    exists,
                    listed_server_uids.take(),
                    trace,
                )
                .await?
            } else {
                trace.note(
                    "deletions",
                    format!("not checked: {}", deletion_plan.skip_reason()),
                );
                Reconciliation::default()
            };
            let uids_to_delete = reconciliation.deleted;

            // Fetch mail the server has and we do not. This is what actually
            // repairs a folder that has fallen behind; the UIDNEXT check keeps
            // it from falling behind again, but only this closes an existing
            // gap. Bounded per pass so one enormous repair is spread over
            // several syncs rather than blocking one of them for minutes.
            if !reconciliation.missing.is_empty() {
                let total_missing = reconciliation.missing.len();
                let batch =
                    &reconciliation.missing[..total_missing.min(MISSING_MESSAGE_REPAIR_BUDGET)];
                crate::logger::warn(&format!(
                    "sync[{account_name}/{folder_name}]: {total_missing} message(s) present on the \
                     server are missing locally; fetching {} of them now",
                    batch.len()
                ));
                let repair = trace.start("repair", imap_client.bytes_on_wire());
                let recovered = imap_client.fetch_uids(folder_id, batch).await?;
                trace.end(
                    repair,
                    imap_client.bytes_on_wire(),
                    Some(recovered.len()),
                    format!(
                        "{total_missing} missing locally, {} per pass",
                        MISSING_MESSAGE_REPAIR_BUDGET
                    ),
                );
                messages.extend(recovered);
            }

            // Mail that arrived since the last sync; a first sync brings a
            // folder's whole past, which the background pass is for.
            let arrived: Vec<(u32, Option<u32>)> = if has_baseline {
                messages
                    .iter()
                    .filter(|m| !stored_set.contains(&m.uid))
                    .map(|m| (m.uid, m.size))
                    .collect()
            } else {
                Vec::new()
            };

            // Transaction 2: apply the remaining changes and record the
            // folder's new state, with the version validated. The validation
            // is what makes releasing the write lock during the network phase
            // safe: a user write to this folder during the fetch moved the
            // version, and the attempt starts over from a fresh baseline.
            let write = trace.start("write", 0);
            let highest_uid = Self::compute_highest_uid(mailbox.uid_next);
            let written = Self::locked_write(write_lock, || {
                db.transaction(|tx| {
                    Self::check_version(tx, folder_id, baseline_version, folder_name)?;
                    let held_back = Self::queued_move_uids(tx, account_name, folder_name)?;
                    let (inserted, updated) = if held_back.is_empty() {
                        Database::bulk_insert_messages_in_tx(tx, &messages)?
                    } else {
                        let kept: Vec<crate::models::Message> = messages
                            .iter()
                            .filter(|message| !held_back.contains(&message.uid))
                            .cloned()
                            .collect();
                        Database::bulk_insert_messages_in_tx(tx, &kept)?
                    };
                    let deleted =
                        Database::delete_messages_by_uids_in_tx(tx, folder_id, &uids_to_delete)?;
                    // Staged actions whose message has gone from the server
                    // can no longer apply.
                    Database::maintain_pending_actions_after_sync_in_tx(
                        tx,
                        account_name,
                        folder_name,
                        folder_id,
                        false,
                    )?;
                    tx.execute(
                        "UPDATE folders SET
                            uidvalidity = ?1,
                            highest_uid = ?2,
                            highest_modseq = ?3,
                            last_sync_time = ?4,
                            sync_version = sync_version + 1
                         WHERE id = ?5",
                        params![
                            server_uidvalidity,
                            highest_uid,
                            snapshot_modseq,
                            chrono::Utc::now().to_rfc3339(),
                            folder_id
                        ],
                    )?;
                    Ok(FolderSyncReport {
                        messages_added: committed.messages_added + inserted.len(),
                        messages_updated: committed.messages_updated + updated,
                        messages_deleted: deleted,
                        added_ids: if has_baseline { inserted } else { Vec::new() },
                    })
                })
            })
            .await;

            match written {
                Ok(mut report) => {
                    trace.end(
                        write,
                        0,
                        Some(messages.len()),
                        format!(
                            "added={} updated={} deleted={}",
                            report.messages_added, report.messages_updated, report.messages_deleted
                        ),
                    );

                    // Check the flags against the server on a schedule,
                    // whatever the modseq did: a modseq is exactly the thing
                    // under suspicion, and deciding whether to verify by
                    // consulting it is circular. The interval and the window
                    // bound the cost.
                    match Self::reconcile_flags(
                        db,
                        write_lock,
                        imap_client,
                        account_name,
                        folder_name,
                        folder_id,
                        trace,
                    )
                    .await
                    {
                        Ok(corrected) => report.messages_updated += corrected,
                        // A folder whose messages synced is worth keeping even
                        // if its flags could not be verified.
                        Err(e) => crate::logger::warn(&format!(
                            "sync[{account_name}/{folder_name}]: flag check failed: {e}"
                        )),
                    }

                    if let Err(e) = Self::prefetch_arrived(
                        db,
                        write_lock,
                        imap_client,
                        folder_id,
                        &arrived,
                        trace,
                    )
                    .await
                    {
                        crate::logger::warn(&format!(
                            "sync[{account_name}/{folder_name}]: could not fetch the new mail \
                             ahead of opening it: {e}"
                        ));
                    }

                    return Ok(report);
                }
                Err(Error::ConcurrencyConflict { .. }) if attempt < MAX_RETRY_ATTEMPTS - 1 => {
                    trace.end(
                        write,
                        0,
                        None,
                        format!(
                            "sync_version moved during the fetch; attempt {} of {} starts over",
                            attempt + 2,
                            MAX_RETRY_ATTEMPTS
                        ),
                    );
                    continue 'attempts;
                }
                Err(e) => {
                    trace.end(write, 0, None, format!("failed: {e}"));
                    return Err(e);
                }
            }
        }

        Err(Error::ConcurrencyConflict {
            operation: "sync_folder".to_string(),
            reason: format!(
                "Failed after {} retry attempts - another instance is actively syncing",
                MAX_RETRY_ATTEMPTS
            ),
        })
    }

    /// Fail with a conflict unless the folder's version is still `expected`.
    fn check_version(
        tx: &rusqlite::Transaction,
        folder_id: i64,
        expected: i64,
        folder_name: &str,
    ) -> Result<()> {
        let current: i64 = tx.query_row(
            "SELECT sync_version FROM folders WHERE id = ?1",
            params![folder_id],
            |row| row.get(0),
        )?;
        if current != expected {
            return Err(Error::ConcurrencyConflict {
                operation: format!("sync_folder({})", folder_name),
                reason: format!(
                    "Folder modified by another writer during sync (version {} -> {})",
                    expected, current
                ),
            });
        }
        Ok(())
    }

    /// Commit one page of a first sync.
    ///
    /// UIDVALIDITY is recorded with the page, so that a sync interrupted here
    /// and resumed later can tell whether the stored UIDs still mean anything.
    /// The modseq waits for the final transaction: a folder without one is a
    /// folder whose first sync has not finished, and is treated as such.
    ///
    /// Returns the folder's version after the commit, which the next page
    /// expects to find.
    async fn commit_page(
        db: &mut Database,
        write_lock: &Arc<tokio::sync::Mutex<()>>,
        folder_id: i64,
        expected_version: i64,
        uidvalidity: u32,
        messages: &[crate::models::Message],
    ) -> Result<(i64, FolderSyncReport)> {
        Self::locked_write(write_lock, || {
            db.transaction(|tx| {
                Self::check_version(tx, folder_id, expected_version, "page")?;
                let (inserted, updated) = Database::bulk_insert_messages_in_tx(tx, messages)?;
                tx.execute(
                    "UPDATE folders SET uidvalidity = ?1, sync_version = sync_version + 1 \
                     WHERE id = ?2",
                    params![uidvalidity, folder_id],
                )?;
                Ok((
                    expected_version + 1,
                    FolderSyncReport {
                        messages_added: inserted.len(),
                        messages_updated: updated,
                        messages_deleted: 0,
                        added_ids: Vec::new(),
                    },
                ))
            })
        })
        .await
    }

    /// How many of the newest messages to check flags for.
    ///
    /// Bounded because a folder can hold hundreds of thousands of messages and
    /// this runs during a sync. Flags change on mail people are reading, which
    /// is the recent end; older mail drifts too, but a full pass belongs to a
    /// command someone asks for, not to every sync.
    const FLAG_CHECK_WINDOW: usize = 500;

    /// How often a folder's flags are worth re-checking.
    const FLAG_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);

    /// Compare stored flags against the server's and correct the differences.
    ///
    /// Flags normally arrive through CONDSTORE: ask what changed since a
    /// modseq, and the server says. That rests on the server advancing its
    /// HIGHESTMODSEQ whenever a flag changes — and one server here advertises
    /// CONDSTORE while leaving the value where it is, so a message read in
    /// another client stays unread here indefinitely with nothing reporting a
    /// problem.
    ///
    /// So when the server says nothing has changed, that claim is checked
    /// rather than believed. When the modseq did move, CONDSTORE has already
    /// reported the changes and there is nothing to verify.
    ///
    /// Returns how many messages were corrected.
    async fn reconcile_flags(
        db: &mut Database,
        write_lock: &Arc<tokio::sync::Mutex<()>>,
        imap_client: &mut ImapClient,
        account_name: &str,
        folder_name: &str,
        folder_id: i64,
        trace: &mut SyncTrace,
    ) -> Result<usize> {
        if !db.flags_check_is_due(folder_id, Self::FLAG_CHECK_INTERVAL)? {
            trace.note("flags", "not due");
            return Ok(0);
        }

        let stored = db.folder_flags(folder_id)?;
        if stored.is_empty() {
            trace.note("flags", "nothing stored to compare");
            return Ok(0);
        }

        let mut uids: Vec<u32> = stored.keys().copied().collect();
        uids.sort_unstable();
        let window: Vec<u32> = uids
            .iter()
            .rev()
            .take(Self::FLAG_CHECK_WINDOW)
            .copied()
            .collect();

        let flags = trace.start("flags", imap_client.bytes_on_wire());
        let server = match imap_client.fetch_flags(&window).await {
            Ok(server) => server,
            Err(e) => {
                trace.end(
                    flags,
                    imap_client.bytes_on_wire(),
                    Some(window.len()),
                    format!("failed: {e}"),
                );
                return Err(e);
            }
        };

        let corrections: Vec<(u32, Vec<String>)> = server
            .into_iter()
            .filter(|(uid, theirs)| stored.get(uid).is_some_and(|ours| ours != theirs))
            .collect();

        let corrected = Self::locked_write(write_lock, || {
            db.note_flags_checked(folder_id)?;
            db.replace_flags(folder_id, &corrections)
        })
        .await?;
        trace.end(
            flags,
            imap_client.bytes_on_wire(),
            Some(window.len()),
            format!("newest {} checked, corrected={corrected}", window.len()),
        );

        if corrected > 0 {
            crate::logger::info(&format!(
                "sync[{account_name}/{folder_name}]: {corrected} flag(s) corrected from the \
                 server, which had reported no change (newest {} checked)",
                window.len()
            ));
        }
        Ok(corrected)
    }

    /// Detect deleted messages using the stored UIDs of Transaction 1.
    ///
    /// On a QRESYNC server, VANISHED since the stored modseq; failing that,
    /// a comparison of every UID — rate-limited by the message count, since
    /// on a large folder the full listing is the dominant cost of a sync that
    /// changed nothing.
    #[allow(clippy::too_many_arguments)]
    async fn detect_deletions_with_stored_uids(
        imap_client: &mut ImapClient,
        folder_name: &str,
        uidvalidity: Option<u32>,
        modseq: Option<u64>,
        stored_uids: &[u32],
        fetched_count: usize,
        exists: u32,
        listed: Option<Vec<u32>>,
        trace: &mut SyncTrace,
    ) -> Result<Reconciliation> {
        if imap_client.supports_qresync()
            && let (Some(uidvalidity), Some(modseq)) = (uidvalidity, modseq)
        {
            let vanished = trace.start("vanished", imap_client.bytes_on_wire());
            match imap_client
                .fetch_vanished_uids(uidvalidity, modseq, stored_uids)
                .await
            {
                Ok(relevant_deletions) => {
                    // Trust VANISHED only when the row count it implies agrees
                    // with the server's own message count. See
                    // `vanished_result_is_complete` for why it can silently
                    // under-report.
                    if Self::vanished_result_is_complete(
                        stored_uids.len(),
                        relevant_deletions.len(),
                        fetched_count,
                        Some(exists),
                    ) {
                        trace.end(
                            vanished,
                            imap_client.bytes_on_wire(),
                            Some(relevant_deletions.len()),
                            format!("since modseq {modseq}, of {} stored", stored_uids.len()),
                        );
                        return Ok(Reconciliation {
                            deleted: relevant_deletions,
                            missing: Vec::new(),
                        });
                    }

                    trace.end(
                        vanished,
                        imap_client.bytes_on_wire(),
                        Some(relevant_deletions.len()),
                        "does not reconcile with EXISTS; listing every UID instead",
                    );
                    crate::logger::warn(&format!(
                        "deletion_detect[{}]: VANISHED reported {} deletions, leaving {} local \
                         rows against server EXISTS={}; falling back to full UID comparison",
                        folder_name,
                        relevant_deletions.len(),
                        stored_uids.len().saturating_sub(relevant_deletions.len()) + fetched_count,
                        exists
                    ));
                }
                Err(e) => {
                    trace.end(
                        vanished,
                        imap_client.bytes_on_wire(),
                        None,
                        format!("failed: {e}; listing every UID instead"),
                    );
                    crate::logger::warn(&format!(
                        "QRESYNC VANISHED failed, falling back to UID comparison: {}",
                        e
                    ));
                }
            }
        } else if stored_uids.len() + fetched_count == exists as usize {
            // Without QRESYNC the only way to find a deletion is to list the
            // whole folder, and on a large one that listing is most of what a
            // sync costs. The message count is the cheaper witness: if what is
            // stored plus what just arrived is what the server holds, nothing
            // left and nothing is missing, and the listing can wait for a
            // sync where the count moved.
            trace.note(
                "uid-listing",
                format!(
                    "skipped: {} stored + {fetched_count} fetched = EXISTS {exists}",
                    stored_uids.len()
                ),
            );
            return Ok(Reconciliation::default());
        }

        // Fallback: compare every stored UID with every UID on the server —
        // the list the fetch already took, when it took one.
        let listing = trace.start("uid-listing", imap_client.bytes_on_wire());
        let server_uids = match listed {
            Some(uids) => uids,
            None => match imap_client.fetch_all_uids().await {
                Ok(uids) => uids,
                Err(e) => {
                    trace.end(
                        listing,
                        imap_client.bytes_on_wire(),
                        None,
                        format!("failed: {e}"),
                    );
                    return Err(e);
                }
            },
        };
        let deleted_uids =
            Self::compute_uid_deletions(stored_uids, &server_uids, Some(exists), folder_name);
        // The same comparison answers the other direction for free, and that
        // direction is the one that repairs a mailbox which has drifted.
        let missing_uids = Self::compute_uid_additions(stored_uids, &server_uids);
        trace.end(
            listing,
            imap_client.bytes_on_wire(),
            Some(server_uids.len()),
            format!(
                "every UID, against {} stored: deleted={} missing={}",
                stored_uids.len(),
                deleted_uids.len(),
                missing_uids.len()
            ),
        );

        Ok(Reconciliation {
            deleted: deleted_uids,
            missing: missing_uids,
        })
    }

    /// UIDs the server holds that are absent locally, oldest first.
    ///
    /// The counterpart to [`Self::compute_uid_deletions`], and the half that was
    /// missing. Comparing UID lists was only ever used to find mail that had
    /// gone away, so a local copy that had *fallen behind* — for any reason,
    /// including a MODSEQ that stopped advancing — had no way back: every later
    /// sync compared the same two lists and ignored the difference. A mailbox
    /// 3,722 messages short stayed 3,722 messages short.
    fn compute_uid_additions(stored_uids: &[u32], server_uids: &[u32]) -> Vec<u32> {
        let stored: std::collections::HashSet<u32> = stored_uids.iter().copied().collect();
        let mut missing: Vec<u32> = server_uids
            .iter()
            .copied()
            .filter(|uid| !stored.contains(uid))
            .collect();
        missing.sort_unstable();
        missing
    }

    /// Whether a folder can be skipped entirely this pass.
    ///
    /// Most folders have not changed since the last sync, and opening each one
    /// to be told so is where the time in a full sync goes: SELECT and FETCH
    /// are two round trips against STATUS's one, seventeen times over.
    ///
    /// The condition is deliberately every signal at once, because each covers
    /// what another misses:
    ///
    /// - UIDVALIDITY, or the stored UIDs mean nothing and everything must be
    ///   resynced.
    /// - UIDNEXT, which moves when mail arrives even on a server whose MODSEQ
    ///   does not.
    /// - the message count, which moves when mail is deleted — the only signal
    ///   that does, on such a server.
    /// - HIGHESTMODSEQ, which is the only one that moves when a flag changes
    ///   without anything arriving or leaving.
    ///
    /// Anything unknown means not skipping. A folder never synced has no stored
    /// state to compare and is never skipped.
    fn folder_is_unchanged(
        stored: &Folder,
        stored_message_count: usize,
        state: &crate::imap::FolderState,
    ) -> bool {
        let (Some(uid_validity), Some(uid_next)) = (state.uid_validity, state.uid_next) else {
            return false;
        };
        // Never synced: there is no baseline to compare against.
        if stored.highest_modseq.is_none() || stored.uidvalidity.is_none() {
            return false;
        }

        // A server without CONDSTORE reports no modseq; the stored one is
        // then the zero the sync recorded, and the count and UIDNEXT decide.
        stored.uidvalidity == Some(uid_validity)
            && stored.highest_uid == Some(uid_next.saturating_sub(1))
            && stored.highest_modseq == Some(state.highest_modseq.unwrap_or(0))
            && stored_message_count == state.messages as usize
    }

    /// Decide whether an unchanged HIGHESTMODSEQ may be trusted to imply "no
    /// deletions", allowing deletion detection to be skipped for this sync pass.
    ///
    /// This shortcut is ONLY sound on QRESYNC servers. On a plain CONDSTORE
    /// server (e.g. Gmail) an EXPUNGE is not required to advance HIGHESTMODSEQ,
    /// so an unchanged modseq does NOT guarantee the mailbox is unchanged.
    /// Skipping deletion detection there would let expunged messages linger as
    /// ghosts; instead the caller must fall back to a UID-list comparison.
    ///
    /// It is also only sound if the server's MODSEQ actually advances, which
    /// some servers advertising QRESYNC do not do. `server_message_count` is
    /// the independent check: EXISTS comes back with every SELECT, and a count
    /// that disagrees with what is stored means the mailbox moved regardless of
    /// what the counter claims. Without this, deletions on such a server are
    /// never noticed and the local copy drifts from the mailbox forever.
    fn can_skip_deletion_detection_on_unchanged_modseq(
        supports_qresync: bool,
        baseline_modseq: Option<u64>,
        current_modseq: u64,
        changed_messages_empty: bool,
        stored_message_count: usize,
        server_message_count: Option<u32>,
    ) -> bool {
        let counts_agree = match server_message_count {
            Some(exists) => exists as usize == stored_message_count,
            // No EXISTS to compare against: fall back to trusting the modseq,
            // which is the behaviour this had before the cross-check.
            None => true,
        };

        supports_qresync
            && baseline_modseq.is_some()
            && current_modseq == baseline_modseq.unwrap_or(0)
            && changed_messages_empty
            && counts_agree
    }

    /// Compute deleted UIDs by comparing the stored (cached) UID set against the
    /// server's current UID set.
    ///
    /// UIDs present in `stored_uids` but absent from `server_uids` are treated as
    /// deletions — EXCEPT in one guarded, inconsistent case:
    ///
    /// If the server returned an *empty* UID list while we still hold cached UIDs
    /// AND the mailbox reports `EXISTS > 0` (i.e. the server just told us the
    /// folder is non-empty), the empty UID list is almost certainly a spurious or
    /// failed response rather than a genuine "everything was expunged" event.
    /// Acting on it would wipe the entire folder cache. In that case we skip
    /// deletion detection for this pass and log a warning, returning no deletions.
    fn compute_uid_deletions(
        stored_uids: &[u32],
        server_uids: &[u32],
        mailbox_exists: Option<u32>,
        folder_name: &str,
    ) -> Vec<u32> {
        if server_uids.is_empty()
            && !stored_uids.is_empty()
            && mailbox_exists.is_some_and(|exists| exists > 0)
        {
            crate::logger::warn(&format!(
                "deletion_detect[{}]: server returned an empty UID list but mailbox EXISTS={} \
                 with {} cached UIDs; skipping deletion detection this pass to avoid wiping cache",
                folder_name,
                mailbox_exists.unwrap_or(0),
                stored_uids.len()
            ));
            return Vec::new();
        }

        let server_uid_set: HashSet<u32> = server_uids.iter().copied().collect();
        stored_uids
            .iter()
            .filter(|uid| !server_uid_set.contains(uid))
            .copied()
            .collect()
    }

    /// Classify whether the current folder-sync pass should run deletion
    /// detection.
    ///
    /// Pure decision function extracted from `sync_folder_impl`. The priority of
    /// the skip reasons matches the original inline `if/else` chain exactly:
    /// first-sync > uidvalidity-changed > modseq-unchanged > skip-requested, and
    /// `Run` is returned only when none of those apply.
    fn plan_deletion_detection(
        has_baseline_modseq: bool,
        uidvalidity_changed: bool,
        modseq_unchanged: bool,
        skip_requested: bool,
    ) -> DeletionDetectionPlan {
        if !has_baseline_modseq {
            DeletionDetectionPlan::SkipFirstSync
        } else if uidvalidity_changed {
            DeletionDetectionPlan::SkipUidValidityChanged
        } else if modseq_unchanged {
            DeletionDetectionPlan::SkipModseqUnchanged
        } else if skip_requested {
            DeletionDetectionPlan::SkipRequested
        } else {
            DeletionDetectionPlan::Run
        }
    }

    /// Decide whether a QRESYNC VANISHED result can be trusted as complete.
    ///
    /// VANISHED reports only messages expunged *after* the baseline modseq. If a
    /// deletion is ever missed while the stored modseq advances past it, the
    /// server will never mention those UIDs again and the local rows are
    /// orphaned permanently — the folder silently reports "in sync" while
    /// listing messages that no longer exist.
    ///
    /// Cross-check the row count this sync will leave behind against the
    /// server's own EXISTS. Any disagreement — in either direction — means the
    /// fast path cannot account for the mailbox, and the full UID comparison
    /// must run instead.
    ///
    /// Holding *more* rows than the server has proves rows were orphaned.
    /// Holding *fewer* used to be waved through as "messages not fetched yet",
    /// and that is what made a lagging folder permanent: nothing else looked at
    /// the shortfall, so the one signal that mail was missing was discarded on
    /// every sync. It is now equally a reason to reconcile properly.
    ///
    /// `fetched_count` is the number of messages this sync fetched that were
    /// not already stored, without which a perfectly healthy sync that picked
    /// up new mail would look short by exactly that many.
    ///
    /// An unknown EXISTS is treated as trustworthy, preserving the previous
    /// behaviour rather than forcing an extra round trip on every sync.
    fn vanished_result_is_complete(
        stored_count: usize,
        deletion_count: usize,
        fetched_count: usize,
        mailbox_exists: Option<u32>,
    ) -> bool {
        match mailbox_exists {
            Some(exists) => {
                let after = stored_count.saturating_sub(deletion_count) + fetched_count;
                after == exists as usize
            }
            None => true,
        }
    }

    /// Record what LIST said about an account, and forget the folders it no
    /// longer lists, with their messages.
    async fn record_listing(
        &self,
        account_name: &str,
        folders: &[imap::ListedFolder],
    ) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut db = self.writer()?;
        db.replace_server_folders(account_name, folders)?;
        self.forget_listed_folders(account_name);
        let listed: HashSet<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        for stored in db.get_folders_by_account(account_name)? {
            if !listed.contains(stored.name.as_str()) {
                let removed = forget_folder(&db.conn, account_name, &stored.name)?;
                crate::logger::info(&format!(
                    "{account_name}/{}: no longer on the server; forgotten here with {removed} \
                     message(s)",
                    stored.name
                ));
            }
        }
        Ok(())
    }

    /// Create a folder on the server. `name` is as the user wrote it; it
    /// travels in the server's encoding.
    pub async fn create_folder(&self, account: &str, name: &str) -> Result<String> {
        let account_config = self.account_config(account)?;
        let wire = imap::utf7::encode(name.trim());
        self.with_connection(&account_config, "create folder", async |client| {
            client.create_folder(&wire).await
        })
        .await?;
        Ok(wire)
    }

    /// Rename a folder on the server, and follow it here.
    pub async fn rename_folder(&self, account: &str, from: &str, to: &str) -> Result<String> {
        let account_config = self.account_config(account)?;
        let wire = imap::utf7::encode(to.trim());
        self.with_connection(&account_config, "rename folder", async |client| {
            client.rename_folder(from, &wire).await
        })
        .await?;
        let _guard = self.write_lock.lock().await;
        self.writer()?.rename_folder(account, from, &wire)?;
        self.forget_listed_folders(account);
        Ok(wire)
    }

    /// Delete a folder the server holds no messages in, and forget it here:
    /// how creating one is undone. A folder that has mail by now is refused,
    /// since undoing the creation must not delete what was filed into it.
    pub async fn delete_empty_folder(&self, account: &str, name: &str) -> Result<()> {
        let account_config = self.account_config(account)?;
        let total = self
            .with_connection(&account_config, "folder status", async |client| {
                Ok(client.get_folder_status(name).await?.total)
            })
            .await?;
        if total > 0 {
            return Err(Error::InvalidInput(format!(
                "{} holds {total} message(s) now, and stays",
                imap::utf7::decode(name)
            )));
        }
        self.delete_folder(account, name).await.map(|_| ())
    }

    /// Delete a folder on the server, with its messages, and forget it here.
    pub async fn delete_folder(&self, account: &str, name: &str) -> Result<usize> {
        let account_config = self.account_config(account)?;
        self.with_connection(&account_config, "delete folder", async |client| {
            client.delete_folder(name).await
        })
        .await?;
        let _guard = self.write_lock.lock().await;
        let db = self.writer()?;
        db.conn.execute(
            "DELETE FROM server_folders WHERE account_name = ?1 AND name = ?2",
            params![account, name],
        )?;
        self.forget_listed_folders(account);
        forget_folder(&db.conn, account, name)
    }

    /// Subscribe to a folder, or unsubscribe from it.
    pub async fn set_folder_subscribed(
        &self,
        account: &str,
        name: &str,
        subscribed: bool,
    ) -> Result<()> {
        let account_config = self.account_config(account)?;
        self.with_connection(&account_config, "subscribe", async |client| {
            client.set_subscribed(name, subscribed).await
        })
        .await
    }

    /// Decide whether a stored UIDVALIDITY genuinely conflicts with the server's
    /// current UIDVALIDITY.
    ///
    /// A mismatch is only asserted when BOTH values are known and differ. If
    /// either side is unknown (`None` — e.g. no stored value yet, or the server
    /// did not report UIDVALIDITY) the decision is "no mismatch", preserving the
    /// original let-chain guard behavior.
    fn uidvalidity_mismatch(
        stored_uidvalidity: Option<u32>,
        current_uidvalidity: Option<u32>,
    ) -> bool {
        matches!(
            (stored_uidvalidity, current_uidvalidity),
            (Some(stored), Some(current)) if stored != current
        )
    }

    /// Derive the folder's `highest_uid` from the mailbox's UIDNEXT.
    ///
    /// UIDNEXT is the UID that will be assigned to the *next* message, so the
    /// highest UID currently present is `uid_next - 1`. Uses saturating
    /// subtraction so an absent/zero UIDNEXT yields 0 rather than underflowing.
    fn compute_highest_uid(uid_next: Option<u32>) -> u32 {
        uid_next.unwrap_or(0).saturating_sub(1)
    }

    /// Get all configured accounts
    ///
    /// Returns lightweight account information from the configuration.
    /// Does not contact any servers.
    pub fn get_accounts(&self) -> Vec<AccountInfo> {
        self.config()
            .accounts
            .iter()
            .map(|acc| AccountInfo {
                name: acc.name.clone(),
                email: acc.from.clone(),
            })
            .collect()
    }

    /// Fetch structures for `uids`, isolating any the parser cannot read.
    ///
    /// A structure that will not parse fails the whole response, so the message
    /// responsible has to be found by halving the batch. The subtlety, and the
    /// reason this lives here rather than on the client: a failed parse leaves
    /// the response half-consumed, so the connection is finished. Retrying on it
    /// returns nothing at all — silently — and every later batch on that
    /// connection returns nothing too. That is what left 222,391 messages
    /// looking bodyless.
    ///
    /// So each attempt after a parse failure gets a fresh connection. That costs
    /// a reconnect per split, about a dozen for one bad message in three
    /// hundred, and only when there is a bad message at all.
    async fn fetch_shapes_isolating(
        &self,
        client: &mut ImapClient,
        account: &crate::config::AccountConfig,
        folder: &str,
        uids: &[u32],
    ) -> Result<ShapeFetch> {
        let mut result = ShapeFetch::default();
        let mut queue: Vec<Vec<u32>> = vec![uids.to_vec()];

        while let Some(batch) = queue.pop() {
            match client.fetch_shapes(&batch).await {
                Ok(shapes) => result.shapes.extend(shapes),
                Err(Error::ImapProtocolError { details, .. }) => {
                    // This connection can no longer be trusted to answer.
                    *client = self
                        .reconnect(
                            account,
                            folder,
                            "shape backfill, after an unparseable answer",
                        )
                        .await?;

                    match batch.as_slice() {
                        [uid] => {
                            crate::logger::warn(&format!(
                                "{}/{folder}: UID {uid} has an unreadable structure ({details})",
                                account.name
                            ));
                            result.unreadable.push(*uid);
                        }
                        _ => {
                            let middle = batch.len() / 2;
                            queue.push(batch[middle..].to_vec());
                            queue.push(batch[..middle].to_vec());
                        }
                    }
                }
                Err(error) if error.is_connection_error() => {
                    *client = self
                        .reconnect(
                            account,
                            folder,
                            "shape backfill, after a dropped connection",
                        )
                        .await?;
                    queue.push(batch);
                }
                Err(error) => return Err(error),
            }
        }

        Ok(result)
    }

    /// A fresh connection with `folder` selected, after the previous one died.
    ///
    /// The broken connection is deliberately not returned to the pool — handing
    /// it on would fail the next caller in exactly the same way.
    async fn reconnect(
        &self,
        account: &crate::config::AccountConfig,
        folder: &str,
        reason: &str,
    ) -> Result<ImapClient> {
        let password = self.credentials.imap(account).await?;
        self.connection_pool
            .register_account(
                &account.name,
                AccountConnectionConfig::for_account(account, password),
            )
            .await;
        let mut client = self.connection_pool.acquire(&account.name, reason).await?;
        client.select_mailbox(folder).await?;
        Ok(client)
    }

    /// Record what messages are made of, for ones synced before Orb asked.
    ///
    /// Sync learns a message's shape from BODYSTRUCTURE, but only re-fetches
    /// messages whose modseq changed — so mail that predates that request never
    /// acquires one, and the body worker cannot see it. This fills the gap.
    /// It carries no body content, so it is cheap enough to run over an entire
    /// mailbox and is the pass that has to finish first.
    pub async fn fetch_pending_shapes(
        &self,
        account_name: &str,
        folders: &[String],
        budget: usize,
    ) -> Result<usize> {
        let account = self
            .get_account_config(account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {account_name} not found")))?;

        let mut shaped = 0usize;
        let mut remaining = budget;

        for folder_name in folders {
            if remaining == 0 {
                break;
            }

            let pending = {
                let db = self.reader()?;
                let Some(folder) = db.get_folder_by_name(account_name, folder_name)? else {
                    continue;
                };
                let folder_id = folder.id.expect("folder must have id");
                db.messages_without_shape(folder_id, remaining)?
            };
            if pending.is_empty() {
                continue;
            }

            let password = self.credentials.imap(&account).await?;
            self.connection_pool
                .register_account(
                    &account.name,
                    AccountConnectionConfig::for_account(&account, password),
                )
                .await;
            let mut client = self
                .connection_pool
                .acquire(&account.name, "shape backfill")
                .await?;
            client.select_mailbox(folder_name).await?;

            for chunk in pending.chunks(SHAPE_BATCH_SIZE) {
                let uids: Vec<u32> = chunk.iter().map(|(_, uid)| *uid).collect();
                let fetched = self
                    .fetch_shapes_isolating(&mut client, &account, folder_name, &uids)
                    .await?;
                let unreadable: std::collections::HashSet<u32> =
                    fetched.unreadable.iter().copied().collect();
                if !unreadable.is_empty() {
                    crate::logger::warn(&format!(
                        "{}/{folder_name}: {} message(s) have a BODYSTRUCTURE that cannot be \
                         parsed; recording them as unreadable so the backfill can move on",
                        account.name,
                        unreadable.len()
                    ));
                }

                // Every UID gone from the mailbox is a legitimate empty answer,
                // but recording "no parts" for a whole batch is how a fetch
                // problem becomes a permanently bodyless message. Leave them
                // for a later pass; the next sync removes any that really are
                // gone, so this cannot loop forever.
                if fetched.shapes.is_empty() && unreadable.is_empty() {
                    crate::logger::warn(&format!(
                        "{}/{folder_name}: no structures came back for {} message(s); leaving \
                         them unshaped rather than recording them as bodyless",
                        account.name,
                        chunk.len()
                    ));
                    continue;
                }

                let _guard = self.write_lock.lock().await;
                let db = self.reader()?;
                for (message_id, uid) in chunk {
                    if unreadable.contains(uid) {
                        db.mark_shape_unreadable(*message_id)?;
                    } else {
                        // A message the server no longer describes still gets a
                        // shape recorded, or it would be re-selected on every
                        // pass forever.
                        let shape = fetched.shapes.get(uid).cloned().unwrap_or_default();
                        db.set_message_shape(*message_id, &shape)?;
                    }
                    shaped += 1;
                }
            }

            self.connection_pool.release(&account.name, client).await;
            remaining = remaining.saturating_sub(pending.len());
        }

        Ok(shaped)
    }

    /// Give messages a vector, so they can be found by meaning.
    ///
    /// Local work: the model is a static embedder, so this is a table lookup
    /// and a mean per message, with no network and no server to be polite to.
    /// It runs at tens of thousands of messages a second, which is why it can
    /// afford to be exact later — see `search::embed`.
    ///
    /// Does nothing at all when no model is configured, which is how semantic
    /// search stays optional without exact search noticing.
    pub async fn index_pending_embeddings(&self, budget: usize) -> Result<usize> {
        use crate::search::embed;

        let Some(embedder) = embed::shared(self.config().embedding_model.as_deref()) else {
            return Ok(0);
        };

        let pending = {
            let db = self.reader()?;
            if !db.has_vector_index() {
                return Ok(0);
            }
            db.messages_without_embedding(budget)?
        };
        if pending.is_empty() {
            return Ok(0);
        }

        let texts: Vec<String> = pending
            .iter()
            .map(|(_, subject, body)| embed::embedding_text(subject.as_deref(), body.as_deref()))
            .collect();
        let vectors = embedder.encode(&texts);

        let _guard = self.write_lock.lock().await;
        let db = self.reader()?;
        let mut indexed = 0usize;
        for ((message_id, _, _), vector) in pending.iter().zip(&vectors) {
            db.set_message_embedding(*message_id, &embed::pack(vector))?;
            indexed += 1;
        }
        Ok(indexed)
    }

    /// How many messages still need a vector, and how many have one.
    pub fn embedding_progress(&self) -> Result<(i64, i64)> {
        let db = self.reader()?;
        if !db.has_vector_index() {
            return Ok((0, 0));
        }
        db.embedding_progress()
    }

    /// Put a draft in the account's Drafts folder, replacing an earlier save.
    ///
    /// A draft has to live on the server rather than in the window that is
    /// writing it. Otherwise closing the window, quitting, or crashing loses
    /// the work, and the draft cannot be picked up on another machine — which
    /// is the whole reason mail clients keep a Drafts folder rather than a
    /// scratch file.
    ///
    /// IMAP cannot update a message, so a save is an append followed by the
    /// removal of what it supersedes. Appending first means a failure leaves
    /// the previous version in place rather than nothing at all.
    ///
    /// Returns the UID of the saved draft, which is what a later save must be
    /// told in order to replace it.
    pub async fn save_draft(
        &self,
        account_name: &str,
        email: &crate::smtp::OutgoingEmail,
        replacing: Option<u32>,
    ) -> Result<Option<u32>> {
        let account = self
            .get_account_config(account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {account_name} not found")))?;
        let folder = account.drafts_folder.clone();

        let bytes = crate::smtp::draft_bytes(&account, email)
            .map_err(|e| Error::InvalidInput(format!("could not build the draft: {e}")))?;

        let password = self.credentials.imap(&account).await?;
        self.connection_pool
            .register_account(
                &account.name,
                AccountConnectionConfig::for_account(&account, password),
            )
            .await;
        let mut client = self
            .connection_pool
            .acquire(&account.name, "draft save")
            .await?;

        // Some servers want the mailbox selected before APPEND, and it is
        // needed anyway to remove the version being replaced.
        client.select_mailbox(&folder).await?;
        client
            .append_with_flags(&folder, &bytes, Some(r"(\Draft \Seen)"))
            .await?;

        if let Some(old) = replacing {
            // Best effort: a draft that fails to be cleaned up is a duplicate,
            // which is untidy; failing the save over it would lose the edit.
            if let Err(error) = client.delete_messages(&[old]).await {
                crate::logger::warn(&format!(
                    "{account_name}/{folder}: saved the draft but could not remove the previous \
                     version (UID {old}): {error}"
                ));
            }
        }

        // The new UID, so the next save can replace this one. Found by asking
        // the mailbox rather than by trusting APPENDUID, which not every server
        // returns.
        let uid = client
            .folder_state(&folder)
            .await
            .ok()
            .and_then(|state| state.uid_next.and_then(|next| next.checked_sub(1)));

        self.connection_pool.release(&account.name, client).await;
        Ok(uid)
    }

    /// Remove a saved draft.
    pub async fn discard_draft(&self, account_name: &str, uid: u32) -> Result<()> {
        let account = self
            .get_account_config(account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {account_name} not found")))?;
        let folder = account.drafts_folder.clone();

        let password = self.credentials.imap(&account).await?;
        self.connection_pool
            .register_account(
                &account.name,
                AccountConnectionConfig::for_account(&account, password),
            )
            .await;
        let mut client = self
            .connection_pool
            .acquire(&account.name, "draft discard")
            .await?;
        client.select_mailbox(&folder).await?;
        let result = client.delete_messages(&[uid]).await;
        self.connection_pool.release(&account.name, client).await;
        result
    }

    /// Which account and folder a message is in.
    ///
    /// A reply is written from the account that received it, so its signature
    /// and its reply settings are the ones that apply.
    pub fn folder_of_message(&self, message_id: i64) -> Result<Option<(String, String)>> {
        let db = self.reader()?;
        db.folder_of_message(message_id)
    }

    /// How many messages still lack a shape, and how many exist.
    pub fn shape_backfill_progress(&self) -> Result<(i64, i64)> {
        let db = self.reader()?;
        db.pending_shape_count()
    }

    /// How many messages the server described in a way that could not be
    /// parsed, and which are therefore permanently absent from search.
    pub fn unreadable_shape_count(&self) -> Result<i64> {
        let db = self.reader()?;
        db.unreadable_shape_count()
    }

    /// Fetch bodies for messages that have been synced but never opened.
    ///
    /// Header sync deliberately stores no body content, so a freshly synced
    /// mailbox has almost none: 126 of 289,214 messages here. That makes
    /// full-text search an index over subject lines. This fills it in
    /// afterwards, fetching only the text part of each message rather than its
    /// full source.
    ///
    /// Bounded by `budget` messages per call so it can be driven repeatedly
    /// from a background loop and stopped between calls. Progress is whatever
    /// is already stored, so an interrupted run resumes for free.
    ///
    /// Folders are visited in the order given, which is how the caller keeps
    /// the folder the user is looking at ahead of a long archive.
    pub async fn fetch_pending_bodies(
        &self,
        account_name: &str,
        folders: &[String],
        budget: usize,
        progress_tx: Option<mpsc::UnboundedSender<StateUpdate>>,
    ) -> Result<crate::tasks::bodies::BodyFetchReport> {
        use crate::tasks::bodies::{BodyFetchReport, fetch_bodies_for_folder};

        let account = self
            .get_account_config(account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {account_name} not found")))?;

        let mut total = BodyFetchReport::default();
        let mut remaining = budget;

        for folder_name in folders {
            if remaining == 0 {
                break;
            }

            // Read the work list under the read lock only.
            let (folder_id, pending) = {
                let db = self.reader()?;
                let Some(folder) = db.get_folder_by_name(account_name, folder_name)? else {
                    continue;
                };
                let folder_id = folder.id.expect("folder must have id");
                (folder_id, db.pending_bodies(folder_id, remaining)?)
            };
            let _ = folder_id;
            if pending.is_empty() {
                continue;
            }

            let password = self.credentials.imap(&account).await?;
            self.connection_pool
                .register_account(
                    &account.name,
                    AccountConnectionConfig::for_account(&account, password.clone()),
                )
                .await;
            let mut client = self
                .connection_pool
                .acquire(&account.name, "body backfill")
                .await?;

            // Writes go through the write lock, like every other body write.
            let guard = self.write_lock.lock().await;
            let db = self.reader()?;
            let report = match fetch_bodies_for_folder(
                &mut client,
                &db,
                folder_name,
                &pending,
                BODY_BATCH_SIZE,
            )
            .await
            {
                Ok(report) => report,
                // Same reasoning as the other passes: a dropped connection
                // must not cost the whole pass. Re-running the folder is safe
                // because it only fetches what is still missing.
                Err(error) if error.is_connection_error() => {
                    crate::logger::warn(&format!(
                        "{}/{folder_name}: connection lost while fetching bodies ({error}); \
                         reconnecting",
                        account.name
                    ));
                    client = self
                        .reconnect(
                            &account,
                            folder_name,
                            "body backfill, after a dropped connection",
                        )
                        .await?;
                    fetch_bodies_for_folder(
                        &mut client,
                        &db,
                        folder_name,
                        &pending,
                        BODY_BATCH_SIZE,
                    )
                    .await?
                }
                Err(error) => return Err(error),
            };
            drop(guard);

            self.connection_pool.release(&account.name, client).await;

            remaining = remaining.saturating_sub(pending.len());
            total.merge(report);

            if let Some(tx) = &progress_tx {
                let _ = tx.send(StateUpdate::SyncProgress {
                    account: account_name.to_string(),
                    stage: SyncStage::SyncingFolder {
                        name: folder_name.clone(),
                        current: Some(total.fetched),
                        total: None,
                    },
                    progress: 0.0,
                });
            }
        }

        Ok(total)
    }

    /// How many messages still need a body, and how many could have one.
    pub fn body_backfill_progress(&self) -> Result<(i64, i64)> {
        let db = self.reader()?;
        db.pending_body_count()
    }

    /// Report configured special folders that do not exist on the server.
    ///
    /// A `drafts_folder`, `sent_folder` or `trash_folder` naming a mailbox the
    /// account does not have fails late and opaquely: the copy is only attempted
    /// when the user finally deletes or sends something, and the server answers
    /// `UID COPY failed`, naming neither the folder nor the setting. Checking the
    /// names against the server's folder list turns that into something
    /// reportable at startup.
    ///
    /// The list the server last gave, not the folders synced from it: a trash
    /// folder set `sync: false` is still there to move into. Before the first
    /// listing has been recorded the synced folders stand in, and an account
    /// with neither is skipped — on a first run every name would look missing.
    pub fn special_folder_warnings(&self) -> Result<Vec<String>> {
        let db = self.reader()?;
        let mut warnings = Vec::new();

        for account in &self.config().accounts {
            let listed = self.listed_folders(&account.name);
            let known: HashSet<String> = if listed.is_empty() {
                db.get_folders_by_account(&account.name)?
                    .into_iter()
                    .map(|f| f.name)
                    .collect()
            } else {
                listed.iter().map(|f| f.name.clone()).collect()
            };
            if known.is_empty() {
                continue;
            }
            // As resolved: a default the server has no folder for but a role
            // for is not missing.
            let account = &self.resolve_special_folders(account);

            let mut settings = vec![
                ("drafts_folder", &account.drafts_folder),
                ("sent_folder", &account.sent_folder),
                ("trash_folder", &account.trash_folder),
            ];
            // Only when named: the defaults are guesses the server need not
            // share, and `archive` says so itself when its folder is missing.
            if let Some(archive) = &account.archive_folder {
                settings.push(("archive_folder", archive));
            }
            if let Some(junk) = &account.junk_folder {
                settings.push(("junk_folder", junk));
            }
            for (setting, value) in settings {
                if !known.contains(value.as_str()) {
                    warnings.push(format!(
                        "{}: {} = '{}' does not exist on the server",
                        account.name, setting, value
                    ));
                }
            }
        }

        Ok(warnings)
    }

    /// Get all folders for an account
    ///
    /// Queries the database for all folders that have been synced for the
    /// specified account. Does not contact the IMAP server.
    ///
    /// # Arguments
    ///
    /// * `account_name` - Account identifier
    ///
    /// # Returns
    ///
    /// Returns a vector of FolderDisplay structs containing folder metadata,
    /// display configuration, and message counts.
    pub fn get_folders(&self, account_name: &str) -> Result<Vec<FolderDisplay>> {
        self.get_visible_folders(account_name)
    }

    /// Get folders for all accounts
    ///
    /// Returns folders from all configured accounts, useful for global folder views.
    pub fn get_all_folders(&self) -> Result<Vec<FolderDisplay>> {
        let mut all_folders = Vec::new();
        for account in &self.config().accounts {
            let folders = self.get_visible_folders(&account.name)?;
            all_folders.extend(folders);
        }
        Ok(all_folders)
    }

    /// Get paginated message list for UI rendering
    ///
    /// Queries the database only (does not contact IMAP server) to retrieve
    /// lightweight message metadata without body content for efficient list
    /// rendering.
    ///
    /// # Arguments
    ///
    /// * `account_name` - Account identifier
    /// * `folder_name` - Folder name (e.g., "INBOX")
    /// * `offset` - Number of messages to skip (for pagination)
    /// * `count` - Maximum number of messages to return
    /// * `sort` - Sort order for the message list
    ///
    /// # Returns
    ///
    /// Returns a vector of MessageListItem structs containing message metadata
    /// (from, to, subject, date, flags, etc.) but excluding body content.
    ///
    /// # Errors
    ///
    /// Returns Error::NotFound if the folder doesn't exist in the database.
    /// The row a message is on in its folder's list in `sort` order.
    pub fn message_row(
        &self,
        account_name: &str,
        folder_name: &str,
        id: i64,
        sort: MessageSort,
    ) -> Result<Option<usize>> {
        let db = self.reader()?;
        let Some(folder_id) = db
            .get_folder_by_name(account_name, folder_name)?
            .and_then(|folder| folder.id)
        else {
            return Ok(None);
        };
        db.message_row(folder_id, id, sort)
    }

    /// [`Store::message_row`] in several folders read as one list, as
    /// [`Store::get_merged_message_list`] reads them.
    pub fn merged_message_row(
        &self,
        folders: &[(String, String)],
        id: i64,
        sort: MessageSort,
    ) -> Result<Option<usize>> {
        let db = self.reader()?;
        let ids: Vec<i64> = threading::merged_locations(&db, folders)?
            .into_keys()
            .collect();
        db.message_row_in(&ids, id, sort)
    }

    pub fn get_message_list(
        &self,
        account_name: &str,
        folder_name: &str,
        offset: usize,
        count: usize,
        sort: MessageSort,
    ) -> Result<Vec<MessageListItem>> {
        let db = self.reader()?;

        // Get folder to verify it exists and get its ID
        let folder = db
            .get_folder_by_name(account_name, folder_name)?
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "Folder '{}' not found for account '{}'",
                    folder_name, account_name
                ))
            })?;

        let folder_id = folder.id.expect("folder must have id");

        // Query message list from database
        let messages = db.get_message_list(folder_id, offset, count, sort)?;

        Ok(messages)
    }

    /// A page of several folders as one list in `sort` order, each item with
    /// the account and folder it came from. A folder that does not exist is
    /// skipped rather than failing the page: the inboxes of an account that
    /// has not synced yet are simply empty.
    pub fn get_merged_message_list(
        &self,
        folders: &[(String, String)],
        offset: usize,
        count: usize,
        sort: MessageSort,
    ) -> Result<Vec<((String, String), MessageListItem)>> {
        let db = self.reader()?;
        let mut ids: Vec<i64> = Vec::new();
        let mut locations: HashMap<i64, (String, String)> = HashMap::new();
        for (account_name, folder_name) in folders {
            if let Some(id) = db
                .get_folder_by_name(account_name, folder_name)?
                .and_then(|folder| folder.id)
            {
                ids.push(id);
                locations.insert(id, (account_name.clone(), folder_name.clone()));
            }
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(db
            .get_message_list_in(&ids, offset, count, sort)?
            .into_iter()
            .filter_map(|item| {
                locations
                    .get(&item.folder_id)
                    .cloned()
                    .map(|location| (location, item))
            })
            .collect())
    }

    /// Where items dated `times` (unix seconds) would stand among the rows
    /// of [`Store::get_merged_message_list`] ordered by date: for each, how
    /// many messages come before it. An item as new as a message stands
    /// above it; a message without a date comes last either way, as in the
    /// list.
    pub fn merged_message_ranks(
        &self,
        folders: &[(String, String)],
        times: &[i64],
        newest_first: bool,
    ) -> Result<Vec<usize>> {
        let db = self.reader()?;
        let locations = threading::merged_locations(&db, folders)?;
        if locations.is_empty() {
            return Ok(vec![0; times.len()]);
        }
        let in_folders = threading::in_folders_clause(&locations);
        let before = if newest_first { ">" } else { "<" };
        let mut stmt = db.conn.prepare(&format!(
            "SELECT COUNT(*) FROM messages WHERE {in_folders} AND date_unix {before} ?1"
        ))?;
        times
            .iter()
            .map(|&time| Ok(stmt.query_row(params![time], |row| row.get::<_, i64>(0))? as usize))
            .collect()
    }

    /// Return the subset of the given message ids that have attachments,
    /// evaluated from each message's cached RFC822 source.
    ///
    /// Messages whose full source has not yet been fetched are treated as
    /// having no attachments (they are simply absent from the result). Used to
    /// annotate list rows without forcing a network fetch per message.
    pub fn messages_with_attachments(&self, ids: &[i64]) -> Result<std::collections::HashSet<i64>> {
        let db = self.reader()?;
        db.messages_with_attachments(ids)
    }

    /// The list's snippet of each message whose text is here.
    pub fn snippets(&self, ids: &[i64]) -> Result<HashMap<i64, String>> {
        self.reader()?.snippets(ids)
    }

    /// Full-text search across the whole mailbox.
    ///
    /// Returns the matching messages as the standard [`MessageListItem`] list
    /// type (newest first). Results can span any folder of any account — this is
    /// a server-independent search over the locally-synced messages, so it finds
    /// mail that is not on the currently-loaded page.
    ///
    /// Use [`Store::search_message_results`] instead when the caller also needs
    /// each hit's owning account/folder (e.g. to open it from a results list).
    pub fn search_messages(
        &self,
        query: &str,
        scope: &SearchScope,
    ) -> Result<Vec<MessageListItem>> {
        Ok(self
            .search_message_results(query, scope, None)?
            .into_iter()
            .map(|r| r.message)
            .collect())
    }

    /// Full-text search across the whole mailbox, returning each hit tagged with
    /// its owning account and folder.
    ///
    /// This is the richer variant of [`Store::search_messages`]: the extra
    /// context lets the UI present where each result lives and open it in its
    /// folder regardless of pagination. `sort` orders the hits before the
    /// [`SEARCH_RESULT_LIMIT`] cuts them; `None` keeps the query's own order,
    /// by relevance.
    pub fn search_message_results(
        &self,
        query: &str,
        scope: &SearchScope,
        sort: Option<MessageSort>,
    ) -> Result<Vec<SearchResultItem>> {
        let db = self.reader()?;
        db.search_messages_sorted(
            query,
            scope,
            SEARCH_RESULT_LIMIT,
            self.config().embedding_model.as_deref(),
            sort,
        )
    }

    /// The single newest unread message across the whole mailbox, or `None`
    /// when there is no unread mail.
    ///
    /// Unlike a folder-scoped list, this spans every folder of every account,
    /// so it yields the correct representative message for a new-mail
    /// notification even when the mail arrived in a folder the user is not
    /// currently viewing.
    pub fn latest_unread_message(&self) -> Result<Option<MessageListItem>> {
        let db = self.reader()?;
        db.latest_unread_message()
    }

    // ------------------------------------------------------------------
    // Message-ID lookups (task links, spec §6.2 step 1)
    // ------------------------------------------------------------------

    /// Every locally-stored copy of the message carrying this RFC `Message-ID`.
    ///
    /// This is step 1 of resolving a task's link back to its message (§6.2), and
    /// the reason `messages(message_id)` is indexed: without it this is a scan
    /// of the whole mailbox, and it runs every time a task detail is opened.
    ///
    /// A list rather than a single row, because one `Message-ID` legitimately
    /// appears in several folders at once — the copy in `INBOX` and the copy the
    /// rule filed in `Archive`, the same thread on two accounts, a message and
    /// its `Sent` counterpart. Which of them to open is a question about the
    /// link's cached folder, so it is answered by [`prefer_located_message`]
    /// rather than by the query.
    pub fn find_messages_by_rfc_id(&self, rfc_message_id: &str) -> Result<Vec<SearchResultItem>> {
        let variants = message_id_variants(rfc_message_id);
        if variants.iter().all(|v| v.is_empty()) {
            return Ok(Vec::new());
        }
        let db = self.reader()?;
        find_messages_by_rfc_id(&db.conn, &variants)
    }

    /// Set `flag` on every stored copy of a message a sent one refers to —
    /// `\Answered` on what a reply answers, `$Forwarded` on what a forward
    /// forwards — locally at once and on its server through the outbox. Returns the
    /// accounts a write was queued for, for the caller to drain.
    pub async fn mark_original(&self, in_reply_to: &str, flag: &str) -> Result<Vec<String>> {
        let mut accounts: Vec<String> = Vec::new();
        for item in self.find_messages_by_rfc_id(in_reply_to)? {
            let answered = item
                .message
                .flags
                .iter()
                .any(|set| set.eq_ignore_ascii_case(flag));
            if answered || self.get_account_config(&item.account_name).is_none() {
                continue;
            }
            self.queue_flag(
                &item.account_name,
                &item.folder_name,
                &[item.message.uid],
                flag,
                true,
            )
            .await?;
            if !accounts.contains(&item.account_name) {
                accounts.push(item.account_name);
            }
        }
        Ok(accounts)
    }

    /// When the user last replied to a message: the date of the newest
    /// message in a Sent folder whose `In-Reply-To` names it.
    pub fn last_reply_to(&self, rfc_message_id: &str) -> Result<Option<String>> {
        let variants = message_id_variants(rfc_message_id);
        if variants.iter().all(|v| v.is_empty()) {
            return Ok(None);
        }
        let replies: Vec<(String, String, Option<String>)> = {
            let db = self.reader()?;
            let mut stmt = db.conn.prepare(
                "SELECT f.account_name, f.name, m.date
                 FROM messages m
                 JOIN folders f ON f.id = m.folder_id
                 WHERE m.in_reply_to IN (?1, ?2)
                 ORDER BY m.date_unix DESC NULLS LAST, m.id DESC",
            )?;
            stmt.query_map(params![variants[0], variants[1]], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?
        };
        // A reply from somebody else lands in an inbox; only the Sent folder
        // holds the user's own.
        Ok(replies.into_iter().find_map(|(account, folder, date)| {
            let config = self.get_account_config(&account)?;
            let role = folder_role(&config, &self.listed_folders(&account), &folder);
            (role.as_deref() == Some("sent")).then_some(date).flatten()
        }))
    }

    /// The RFC `Message-ID` of each of the given local messages.
    ///
    /// One query for the whole set. Everything about tasks-on-messages (§6.3) is
    /// keyed by `Message-ID` while everything about a message list is keyed by
    /// row id, so this is the join between the two — and it has to be a single
    /// query, because the caller is a list column rendered for every visible row.
    ///
    /// Messages whose `Message-ID` header was absent or empty are simply missing
    /// from the map: they cannot be linked to a task, so there is nothing to
    /// count for them.
    pub fn rfc_message_ids(&self, ids: &[i64]) -> Result<std::collections::HashMap<i64, String>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let db = self.reader()?;
        rfc_message_ids(&db.conn, ids)
    }

    /// Everything needed to link a message to a task, for a whole selection at
    /// once (§6.1).
    ///
    /// One query, because creating a task per message over a 200-message
    /// selection must not be 200 round trips through the message loader. The
    /// cached subject/from/date and the account and folder travel with the link
    /// so the task stays readable years later, when the message itself may be
    /// gone (§6.2 step 3).
    pub fn message_link_sources(&self, ids: &[i64]) -> Result<Vec<MessageLinkSource>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.reader()?;
        message_link_sources(&db.conn, ids)
    }

    // ------------------------------------------------------------------
    // Server-side Message-ID search (spec §6.2 step 2)
    // ------------------------------------------------------------------

    /// Hunt for a message on the server by its RFC `Message-ID`, and store what
    /// is found.
    ///
    /// Reached only when the local index has no copy: the message was moved by
    /// another client, filed by a server-side rule, or lives in a folder this
    /// machine has never synced. The cached last-known folder is tried first
    /// because it is right most of the time and costs one round trip; the rest
    /// of the mailbox follows, which is why this is slow enough that §6.2 makes
    /// it a visible background task rather than part of rendering a task.
    ///
    /// On a hit the message is fetched and written to the local cache, so the
    /// caller gets back a real local row it can open — and the next lookup is
    /// answered by step 1 without any of this.
    ///
    /// The tasks folder is never searched: its messages are tasks, not mail, and
    /// a task message must never be handed to the mail reader.
    pub async fn find_message_on_server(
        &self,
        account_name: &str,
        rfc_message_id: &str,
        preferred_folder: Option<&str>,
    ) -> Result<Option<SearchResultItem>> {
        let account = self
            .get_account_config(account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {account_name} not found")))?;

        let password = self.credentials.imap(&account).await?;
        self.connection_pool
            .register_account(
                &account.name,
                AccountConnectionConfig::for_account(&account, password),
            )
            .await;
        let mut client = self
            .connection_pool
            .acquire(&account.name, "find message on server")
            .await?;

        let listed = client.list_folders().await.unwrap_or_default();
        let order = search_folder_order(preferred_folder, &listed, |folder| {
            self.config().is_tasks_folder(&account.name, folder)
        });

        let mut found = None;
        for folder in &order {
            match self
                .search_one_folder(&mut client, &account, folder, rfc_message_id)
                .await
            {
                Ok(Some(item)) => {
                    found = Some(item);
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    // A folder that cannot be selected or searched is one folder
                    // of many — a permission error on a shared mailbox is not a
                    // reason to stop looking in the other forty.
                    crate::logger::warn(&format!(
                        "{}/{folder}: could not search for {rfc_message_id}: {error}",
                        account.name
                    ));
                }
            }
        }

        self.connection_pool.release(&account.name, client).await;
        Ok(found)
    }

    /// Search one folder, and cache the message if it is there.
    async fn search_one_folder(
        &self,
        client: &mut ImapClient,
        account: &AccountConfig,
        folder: &str,
        rfc_message_id: &str,
    ) -> Result<Option<SearchResultItem>> {
        client.select_mailbox(folder).await?;

        // Both spellings are asked for: the header is stored with its angle
        // brackets, servers differ on whether they match the bracketed form, and
        // a search that silently matches nothing is indistinguishable here from
        // a message that is genuinely elsewhere.
        let mut uids = Vec::new();
        for variant in message_id_variants(rfc_message_id) {
            if variant.is_empty() {
                continue;
            }
            uids = client.uid_search_message_id(&variant).await?;
            if !uids.is_empty() {
                break;
            }
        }
        let Some(uid) = uids.first().copied() else {
            return Ok(None);
        };

        let folder_id = {
            let lock = self.write_lock.clone();
            let _guard = lock.lock().await;
            let mut db = self.writer()?;
            db.get_or_create_folder(&account.name, folder)?
        };

        let messages = client.fetch_uids(folder_id, &[uid]).await?;
        if messages.is_empty() {
            return Ok(None);
        }

        {
            let lock = self.write_lock.clone();
            let _guard = lock.lock().await;
            let mut db = self.writer()?;
            db.bulk_insert_messages(&messages)?;
        }

        // Read the row back rather than constructing it: the caller needs the
        // local row id, which only exists once the message has been stored.
        let stored = self
            .find_messages_by_rfc_id(rfc_message_id)?
            .into_iter()
            .find(|item| item.folder_name == folder && item.account_name == account.name);
        Ok(stored)
    }

    /// Drop the local mail rows for a folder that is no longer mail.
    ///
    /// The tasks folder becomes the tasks folder the moment `tasks` is added
    /// to the config — but whatever was synced from it before then is still
    /// sitting in `messages`, showing up in search, in the unread badge and in
    /// All Inboxes as mail nobody sent. Excluding the folder from future syncs
    /// does not remove what past syncs already stored, so this does.
    ///
    /// Messages are deleted explicitly rather than left to the foreign key's
    /// `ON DELETE CASCADE`: SQLite does not fire triggers for cascaded deletes
    /// unless recursive triggers are enabled, and the FTS index is maintained by
    /// exactly such a trigger. A cascade would leave the folder's text in the
    /// search index forever.
    pub async fn forget_mail_folder(&self, account_name: &str, folder_name: &str) -> Result<usize> {
        let lock = self.write_lock.clone();
        let _guard = lock.lock().await;
        let db = self.writer()?;
        forget_folder(&db.conn, account_name, folder_name)
    }

    /// Delete messages by UID from local database after IMAP confirms deletion
    ///
    /// This method is called after IMAP operations (delete/move) succeed to immediately
    /// update the local database. This eliminates the need for "optimistic" UI state -
    /// the database is always the source of truth.
    ///
    /// CRITICAL: This method increments sync_version to prevent concurrent syncs from
    /// reinserting the deleted messages from their cached IMAP data. Without this,
    /// a concurrent sync could reinsert deleted messages.
    ///
    /// # Arguments
    ///
    /// * `account` - Account name
    /// * `folder` - Folder name
    /// * `uids` - UIDs of messages to delete
    ///
    /// # Returns
    ///
    /// The number of messages actually deleted from the database.
    pub async fn delete_messages_by_uids(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
    ) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }

        // Acquire write lock to prevent concurrent writes
        let _write_guard = self.write_lock.lock().await;

        let mut db = Database::new(&self.db_path)?;

        // Get folder ID
        let folder_obj = db.get_folder_by_name(account, folder)?.ok_or_else(|| {
            Error::NotFound(format!(
                "Folder '{}' not found for account '{}'",
                folder, account
            ))
        })?;
        let folder_id = folder_obj.id.expect("folder must have id");

        // Transaction: delete messages AND increment sync_version
        let tx = db.conn.transaction()?;

        let deleted_count = Database::delete_messages_by_uids_in_tx(&tx, folder_id, uids)?;

        // CRITICAL: Increment sync_version to invalidate concurrent syncs
        // This ensures any concurrent sync will detect the modification and retry
        // with a fresh baseline instead of reinserting the deleted messages
        tx.execute(
            "UPDATE folders SET sync_version = sync_version + 1 WHERE id = ?1",
            params![folder_id],
        )?;

        tx.commit()?;

        crate::logger::info(&format!(
            "{account}/{folder}: removed {deleted_count} of {} message(s) locally",
            uids.len()
        ));

        Ok(deleted_count)
    }

    /// Gracefully shutdown the store, releasing all locks
    ///
    /// This method should be called when the application is shutting down to ensure
    /// all sync locks held by this instance are released, allowing other instances
    /// to acquire them immediately rather than waiting for the stale lock timeout.
    ///
    /// Returns the number of locks that were released.
    pub async fn shutdown(&self) -> Result<usize> {
        // Close all pooled IMAP connections
        self.connection_pool.close_all().await;

        // Acquire write lock to ensure no concurrent writes
        let _write_guard = self.write_lock.lock().await;
        let mut db = Database::new(&self.db_path)?;
        let count = db.release_all_locks()?;

        if count > 0 {
            crate::logger::info(&format!("shutdown: released {} sync lock(s)", count));
        }

        Ok(count)
    }

    /// Mark messages read or unread: locally at once, on the server as soon as
    /// it can be reached.
    pub async fn set_read_status(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        read: bool,
    ) -> Result<()> {
        self.set_flag(account, folder, uids, "\\Seen", read).await
    }

    /// Mark messages read or unread locally and queue the server write,
    /// without waiting for the server: the caller drains the outbox, see
    /// [`Self::drain_outbox`]. Returns the UIDs whose state changed.
    pub async fn queue_read_status(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        read: bool,
    ) -> Result<Vec<u32>> {
        self.queue_flag(account, folder, uids, "\\Seen", read).await
    }

    /// Flag or unflag messages locally and queue the server write, without
    /// waiting for the server. Returns the UIDs whose state changed.
    pub async fn queue_flag_status(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        flagged: bool,
    ) -> Result<Vec<u32>> {
        self.queue_flag(account, folder, uids, "\\Flagged", flagged)
            .await
    }

    /// Flag or unflag messages: locally at once, on the server as soon as it
    /// can be reached.
    pub async fn set_flag_status(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        flagged: bool,
    ) -> Result<()> {
        self.set_flag(account, folder, uids, "\\Flagged", flagged)
            .await
    }

    /// Change one flag, locally first.
    ///
    /// The stored copy changes before the server hears of it, so the list
    /// shows the change at once and keeps it offline; the server write is
    /// queued in the outbox and carried out now if the server answers, and
    /// otherwise by the next sync of the account. A server that is out of
    /// reach is therefore not an error here — the change has been made and
    /// will arrive.
    async fn set_flag(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        flag: &str,
        add: bool,
    ) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let account_config = self.account_config(account)?.clone();
        self.queue_flag(account, folder, uids, flag, add).await?;

        match self.drain_outbox(&account_config).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_connection_error() => {
                crate::logger::info(&format!(
                    "{account}/{folder}: {} on {} message(s) queued, the server is out of reach: {e}",
                    flag,
                    uids.len()
                ));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Change one flag in the stored copy and queue the server write in the
    /// outbox, which keeps the writes in the order they were made. Returns
    /// the UIDs whose stored state changed.
    async fn queue_flag(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        flag: &str,
        add: bool,
    ) -> Result<Vec<u32>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        // An unknown account is an error here, not at the drain.
        self.account_config(account)?;
        let mut changed = Vec::new();
        {
            let _guard = self.write_lock.lock().await;
            let mut db = self.writer()?;
            if let Some(stored) = db.get_folder_by_name(account, folder)?
                && let Some(folder_id) = stored.id
            {
                changed = db.set_flag_locally(folder_id, uids, flag, add)?;
            }
            let write = FlagWrite {
                folder: folder.to_string(),
                uids: uids.to_vec(),
                flag: flag.to_string(),
                add,
            };
            db.enqueue_outbox(account, OUTBOX_FLAGS, &serde_json::to_string(&write)?)?;
        }
        Ok(changed)
    }

    /// The lock that keeps one drain of `account`'s outbox at a time.
    fn outbox_drain_lock(&self, account: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut drains = self.outbox_drains.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(drains.entry(account.to_string()).or_default())
    }

    /// Carry out the account's queued server writes, in order, on a pooled
    /// connection. Returns how many were carried out.
    ///
    /// Stops at the first write the server cannot be reached for; the rest
    /// wait for the next call. A flag write or move the server refuses, or
    /// one whose payload cannot be read, is dropped with a warning — a retry
    /// would get the same answer, and one bad item must not hold up
    /// everything queued behind it. A send that fails for good is never
    /// dropped: it stays in the Outbox, marked failed, for the user.
    pub async fn drain_outbox(&self, account: &AccountConfig) -> Result<usize> {
        if self.reader()?.outbox_len(Some(&account.name))? == 0 {
            return Ok(0);
        }
        self.with_connection(account, "outbox", async |client| {
            self.drain_outbox_with(client, account).await
        })
        .await
    }

    /// [`Self::drain_outbox`] on a connection the caller already holds.
    ///
    /// Flag writes and sends are two independent queues that happen to share
    /// a table: a mail server that is down keeps the sends waiting and says
    /// nothing about the IMAP connection, which carries on with the flags.
    async fn drain_outbox_with(
        &self,
        client: &mut ImapClient,
        account: &AccountConfig,
    ) -> Result<usize> {
        // Waiting here while holding a connection cannot deadlock: a drain
        // that needs one while this holds it is given a new one by the pool.
        let lock = self.outbox_drain_lock(&account.name);
        let _drain = lock.lock().await;
        let queued = self.reader()?.outbox_queued(&account.name)?;
        let mut done = 0;
        let mut sends_touched = false;
        let mut failed = Vec::new();
        // By login: an identity's own server going down keeps the account's
        // other mail moving, and the other way round.
        let mut smtp_down: HashMap<String, String> = HashMap::new();
        for item in queued {
            let login = (item.kind == OUTBOX_SEND).then(|| queued_login(account, &item));
            let outcome = match item.kind.as_str() {
                OUTBOX_FLAGS => self.apply_queued_flags(client, &item).await,
                OUTBOX_MOVE => self.apply_queued_move(client, &item).await,
                OUTBOX_SEND if login.as_ref().is_some_and(|l| smtp_down.contains_key(l)) => {
                    continue;
                }
                OUTBOX_SEND => {
                    sends_touched = true;
                    match self.send_queued(client, account, &item).await {
                        SendAttempt::Sent => Ok(()),
                        SendAttempt::Unreachable(details) => {
                            let _guard = self.write_lock.lock().await;
                            self.writer()?.outbox_failed(item.id, &details)?;
                            smtp_down.insert(login.unwrap_or_default(), details);
                            continue;
                        }
                        SendAttempt::Refused(reason) => {
                            crate::logger::warn(&format!(
                                "outbox[{}]: send #{} failed, kept in the Outbox: {reason}",
                                account.name, item.id
                            ));
                            {
                                let _guard = self.write_lock.lock().await;
                                self.writer()?.outbox_mark_failed(item.id, &reason)?;
                            }
                            failed.push(FailedSend {
                                id: item.id,
                                account: account.name.clone(),
                                subject: queued_subject(&item),
                                reason,
                            });
                            continue;
                        }
                    }
                }
                other => Err(Error::Sync(format!("unknown outbox kind '{other}'"))),
            };
            match outcome {
                Ok(()) => {
                    done += 1;
                    let _guard = self.write_lock.lock().await;
                    self.writer()?.outbox_done(item.id)?;
                }
                Err(e) if e.is_connection_error() => {
                    {
                        let _guard = self.write_lock.lock().await;
                        self.writer()?.outbox_failed(item.id, &e.to_string())?;
                    }
                    if sends_touched {
                        self.outbox_changed(failed);
                    }
                    return Err(e);
                }
                Err(e) => {
                    crate::logger::warn(&format!(
                        "outbox[{}]: {} #{} dropped after {} attempt(s): {e}",
                        account.name,
                        item.kind,
                        item.id,
                        item.attempts + 1
                    ));
                    let _guard = self.write_lock.lock().await;
                    self.writer()?.outbox_done(item.id)?;
                }
            }
        }
        for (login, details) in smtp_down {
            crate::logger::warn(&format!(
                "outbox[{login}]: the mail server is still out of reach ({details}); queued \
                 messages wait"
            ));
        }
        if sends_touched {
            self.outbox_changed(failed);
        }
        Ok(done)
    }

    async fn apply_queued_flags(&self, client: &mut ImapClient, item: &OutboxItem) -> Result<()> {
        let write: FlagWrite = serde_json::from_str(&item.payload)?;
        self.select_for_uids(client, &item.account, &write.folder)
            .await?;
        client
            .store_flags(&write.uids, &[write.flag.as_str()], write.add)
            .await
    }

    /// Carry out a queued move or permanent delete. The UIDs are checked
    /// against the UIDVALIDITY they were queued under, not the one stored now:
    /// a folder reset in between has made them name other mail.
    ///
    /// A move the server refuses puts the messages back in the stored copy,
    /// since they are still where they were and no sync would bring back mail
    /// that has not changed.
    async fn apply_queued_move(&self, client: &mut ImapClient, item: &OutboxItem) -> Result<()> {
        let write: MoveWrite = serde_json::from_str(&item.payload)?;
        client.select_mailbox(&write.folder).await?;
        let current = client
            .current_mailbox()
            .and_then(|mailbox| mailbox.uid_validity);
        if Self::uidvalidity_mismatch(write.uidvalidity, current) {
            return Err(Error::uidvalidity_changed(
                &item.account,
                &write.folder,
                write.uidvalidity.unwrap_or(0),
                current.unwrap_or(0),
            ));
        }
        let outcome = match &write.target {
            Some(target) => client.move_messages(&write.uids, target).await.map(|_| ()),
            None => client.delete_messages(&write.uids).await,
        };
        if let Err(error) = &outcome
            && !error.is_connection_error()
            && let Err(restore) = self
                .restore_queued_move(client, &item.account, &write)
                .await
        {
            crate::logger::warn(&format!(
                "{}/{}: a refused move left {} message(s) unlisted until the folder is synced \
                 in full ({restore})",
                item.account,
                write.folder,
                write.uids.len()
            ));
        }
        outcome
    }

    /// Store again the messages of a queued move the server refused, from the
    /// folder the client has selected.
    async fn restore_queued_move(
        &self,
        client: &mut ImapClient,
        account: &str,
        write: &MoveWrite,
    ) -> Result<()> {
        let Some(folder_id) = self
            .reader()?
            .get_folder_by_name(account, &write.folder)?
            .and_then(|folder| folder.id)
        else {
            return Ok(());
        };
        let messages = client.fetch_uids(folder_id, &write.uids).await?;
        let _guard = self.write_lock.lock().await;
        self.writer()?.transaction(|tx| {
            Database::bulk_insert_messages_in_tx(tx, &messages)?;
            tx.execute(
                "UPDATE folders SET sync_version = sync_version + 1 WHERE id = ?1",
                params![folder_id],
            )?;
            Ok(())
        })
    }

    /// Take messages out of the stored copy and queue their move — or, with no
    /// target, their permanent delete — on the server, in one transaction. The
    /// list changes now; the server follows when the outbox drains, and until
    /// then a sync leaves the queued UIDs out ([`Self::queued_move_uids`]).
    async fn queue_move(
        &self,
        account: &str,
        folder: &str,
        target: Option<&str>,
        uids: &[u32],
    ) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let _guard = self.write_lock.lock().await;
        let mut db = self.writer()?;
        let stored = db.get_folder_by_name(account, folder)?.ok_or_else(|| {
            Error::NotFound(format!(
                "Folder '{folder}' not found for account '{account}'"
            ))
        })?;
        let folder_id = stored.id.expect("folder must have id");
        let payload = serde_json::to_string(&MoveWrite {
            folder: folder.to_string(),
            uids: uids.to_vec(),
            target: target.map(str::to_string),
            uidvalidity: stored.uidvalidity,
        })?;
        db.transaction(|tx| {
            Database::delete_messages_by_uids_in_tx(tx, folder_id, uids)?;
            // A sync fetching this folder right now starts again, and its
            // second attempt finds the queued UIDs to leave out.
            tx.execute(
                "UPDATE folders SET sync_version = sync_version + 1 WHERE id = ?1",
                params![folder_id],
            )?;
            tx.execute(
                "INSERT INTO outbox (account, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    account,
                    OUTBOX_MOVE,
                    payload,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
            Ok(())
        })
    }

    /// The UIDs of `folder` whose move is queued and not yet carried out. The
    /// server still lists them there, and a sync must not store them again.
    fn queued_move_uids(
        tx: &rusqlite::Transaction,
        account: &str,
        folder: &str,
    ) -> Result<HashSet<u32>> {
        let mut statement =
            tx.prepare("SELECT payload FROM outbox WHERE account = ?1 AND kind = ?2")?;
        let payloads = statement
            .query_map(params![account, OUTBOX_MOVE], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(payloads
            .iter()
            .filter_map(|payload| serde_json::from_str::<MoveWrite>(payload).ok())
            .filter(|write| write.folder == folder)
            .flat_map(|write| write.uids)
            .collect())
    }

    /// Send a queued message. SMTP first; the copy to the Sent folder goes
    /// over the connection in hand. A mail server that cannot be reached
    /// leaves it waiting; anything that will not change by waiting — a
    /// refusal, a password that cannot be had, a payload that cannot be read
    /// — fails it, and the user decides what becomes of it.
    async fn send_queued(
        &self,
        client: &mut ImapClient,
        account: &AccountConfig,
        item: &OutboxItem,
    ) -> SendAttempt {
        let email: crate::smtp::OutgoingEmail = match serde_json::from_str(&item.payload) {
            Ok(email) => email,
            Err(e) => {
                return SendAttempt::Refused(format!("the queued message cannot be read: {e}"));
            }
        };
        let smtp = account.sending_as(email.from.as_deref());
        let password = match self.credentials.smtp(&smtp).await {
            Ok(password) => password,
            Err(e) => {
                return SendAttempt::Refused(format!("no SMTP password for '{}': {e}", smtp.name));
            }
        };
        match crate::smtp::send_email(&smtp, &email, &password).await {
            Ok(rfc822) => {
                if let Err(e) = client
                    .append_message(&account.sent_folder, &rfc822, true)
                    .await
                {
                    crate::logger::warn(&format!(
                        "{}: queued message sent but could not be saved to '{}': {e}",
                        account.name, account.sent_folder
                    ));
                }
                crate::logger::info(&format!(
                    "{}: queued message to {} sent",
                    account.name,
                    email.to.join(", ")
                ));
                // Set locally now; the server hears of it at the next drain.
                let marks = [
                    (&email.in_reply_to, "\\Answered"),
                    (&email.forwards, "$Forwarded"),
                ];
                for (original, flag) in marks {
                    if let Some(original) = original
                        && let Err(e) = self.mark_original(original, flag).await
                    {
                        crate::logger::warn(&format!(
                            "{}: could not mark {original} {flag}: {e}",
                            account.name
                        ));
                    }
                }
                SendAttempt::Sent
            }
            Err(crate::smtp::SmtpError::Unreachable(details)) => SendAttempt::Unreachable(details),
            Err(refused) => {
                if matches!(refused, crate::smtp::SmtpError::AuthRefused(_)) {
                    self.credentials.forget_smtp(&smtp).await;
                }
                SendAttempt::Refused(refused.to_string())
            }
        }
    }

    /// Send a message now, or queue it when the mail server is out of reach.
    ///
    /// A refusal — a bad address, a rejected message — is an error, now, to
    /// whoever pressed send; only a server that cannot be reached at all turns
    /// into a queued message, because waiting is the right answer to that and
    /// to nothing else.
    pub async fn send_mail(
        &self,
        account: &AccountConfig,
        email: &crate::smtp::OutgoingEmail,
    ) -> Result<SendOutcome> {
        let smtp = account.sending_as(email.from.as_deref());
        let password = self.credentials.smtp(&smtp).await?;
        let rfc822 = match crate::smtp::send_email(&smtp, email, &password).await {
            Ok(rfc822) => rfc822,
            Err(crate::smtp::SmtpError::Unreachable(details)) => {
                let _guard = self.write_lock.lock().await;
                self.writer()?.enqueue_outbox(
                    &account.name,
                    OUTBOX_SEND,
                    &serde_json::to_string(email)?,
                )?;
                crate::logger::warn(&format!(
                    "{}: the mail server is out of reach ({details}); message to {} queued",
                    account.name,
                    email.to.join(", ")
                ));
                self.outbox_changed(Vec::new());
                return Ok(SendOutcome::Queued);
            }
            Err(e) => {
                if matches!(e, crate::smtp::SmtpError::AuthRefused(_)) {
                    self.credentials.forget_smtp(&smtp).await;
                }
                return Err(Error::Sync(e.to_string()));
            }
        };

        // A copy to the Sent folder. Non-fatal: the message was delivered.
        let saved = self
            .with_connection(account, "sent copy", async |client| {
                client
                    .append_message(&account.sent_folder, &rfc822, true)
                    .await
            })
            .await;
        Ok(SendOutcome::Sent {
            saved_to_sent: saved.is_ok(),
            warning: saved.err().map(|e| {
                format!(
                    "Message sent but could not be saved to '{}': {e}",
                    account.sent_folder
                )
            }),
        })
    }

    /// How many server writes are still queued, over every account.
    pub fn outbox_len(&self) -> Result<usize> {
        self.reader()?.outbox_len(None)
    }

    /// Register who hears of changes to the Outbox.
    pub fn set_outbox_listener(&self, listener: Arc<dyn OutboxListener>) {
        *self
            .outbox_listener
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(listener);
    }

    fn outbox_changed(&self, failed: Vec<FailedSend>) {
        let listener = self
            .outbox_listener
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(listener) = listener {
            listener.on_outbox_changed(failed);
        }
    }

    /// The Outbox: every queued message of every account, waiting or
    /// failed, oldest first.
    pub fn outbox(&self) -> Result<Vec<QueuedSend>> {
        let config = self.config();
        let items = self.reader()?.outbox_sends()?;
        Ok(items
            .into_iter()
            .map(|item| {
                let account = config.accounts.iter().find(|a| a.name == item.account);
                let email = serde_json::from_str::<crate::smtp::OutgoingEmail>(&item.payload);
                let (sending_as, from) = match (account, &email) {
                    (Some(account), Ok(email)) => (
                        account.sending_as(email.from.as_deref()).name.clone(),
                        email.sender(account).to_string(),
                    ),
                    (_, Ok(email)) => {
                        (item.account.clone(), email.from.clone().unwrap_or_default())
                    }
                    _ => (item.account.clone(), String::new()),
                };
                let email = email.ok();
                QueuedSend {
                    id: item.id,
                    account: item.account,
                    sending_as,
                    from,
                    to: email.as_ref().map(|e| e.to.clone()).unwrap_or_default(),
                    cc: email.as_ref().map(|e| e.cc.clone()).unwrap_or_default(),
                    bcc: email.as_ref().map(|e| e.bcc.clone()).unwrap_or_default(),
                    subject: email
                        .as_ref()
                        .map(|e| e.subject.clone())
                        .unwrap_or_default(),
                    body: email.as_ref().map(|e| e.body.clone()).unwrap_or_default(),
                    attachments: email
                        .as_ref()
                        .map(|e| e.attachments.iter().map(|a| a.filename.clone()).collect())
                        .unwrap_or_default(),
                    queued_at: item.created_at,
                    attempts: item.attempts,
                    error: item.last_error,
                    failed_at: item.failed_at,
                }
            })
            .collect())
    }

    /// Put a failed message back in line and try its account's outbox now.
    /// The drain's own trouble is not this call's: the message is back in
    /// line either way, and the Outbox shows how the attempt went.
    pub async fn retry_outbox(&self, id: i64) -> Result<()> {
        let item = self
            .reader()?
            .outbox_send(id)?
            .ok_or_else(|| Error::Sync(format!("no queued message #{id}")))?;
        {
            let _guard = self.write_lock.lock().await;
            self.writer()?.outbox_retry(id)?;
        }
        self.outbox_changed(Vec::new());
        let account = self.account_config(&item.account)?.clone();
        if let Err(e) = self.drain_outbox(&account).await {
            crate::logger::warn(&format!(
                "outbox[{}]: retrying send #{id}: the outbox could not be drained: {e}",
                account.name
            ));
        }
        Ok(())
    }

    /// Delete a queued message unsent.
    pub async fn discard_outbox(&self, id: i64) -> Result<()> {
        let discarded = {
            let _guard = self.write_lock.lock().await;
            self.writer()?.outbox_discard_send(id)?
        };
        if discarded {
            crate::logger::info(&format!("outbox: send #{id} deleted unsent"));
            self.outbox_changed(Vec::new());
        }
        Ok(())
    }

    /// A queued message as it was written, for editing: its account and the
    /// message.
    pub fn outbox_message(&self, id: i64) -> Result<(String, crate::smtp::OutgoingEmail)> {
        let item = self
            .reader()?
            .outbox_send(id)?
            .ok_or_else(|| Error::Sync(format!("no queued message #{id}")))?;
        let email = serde_json::from_str(&item.payload)?;
        Ok((item.account, email))
    }

    /// Delete messages the way the user means it: a move to the account's
    /// Trash folder, or, for messages that already live in Trash, a permanent
    /// delete. The same decision [`Store::execute_pending`] makes for a staged
    /// delete, so neither path can hard-delete outside Trash. Queued like a
    /// staged one: the server follows when the outbox drains.
    pub async fn delete_messages(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
    ) -> Result<DeleteOutcome> {
        let trash = self.trash_folder_for(account);
        match resolve_pending_execution_op(PENDING_ACTION_DELETE, folder, &trash, None)? {
            PendingExecutionOp::MoveToTrash { trash } => {
                let outcome = self.move_messages(account, folder, &trash, uids).await?;
                Ok(DeleteOutcome::MovedToTrash {
                    trash,
                    warning: outcome.warning,
                    moved: outcome.moved,
                })
            }
            PendingExecutionOp::PermanentDelete => {
                self.queue_move(account, folder, None, uids).await?;
                Ok(DeleteOutcome::Deleted { warning: None })
            }
            PendingExecutionOp::Move { .. } => unreachable!("a delete never resolves to a move"),
        }
    }

    /// Move messages to another folder of the account: out of the stored copy
    /// now, on the server when the outbox drains ([`Self::queue_move`]).
    ///
    /// The Message-IDs it reports are how the move is found again in its
    /// destination should it be undone, once the outbox has carried it out.
    pub async fn move_messages(
        &self,
        account: &str,
        source_folder: &str,
        destination_folder: &str,
        uids: &[u32],
    ) -> Result<WriteOutcome> {
        let moved = self.message_ids_for_uids(account, source_folder, uids);
        self.queue_move(account, source_folder, Some(destination_folder), uids)
            .await?;
        Ok(WriteOutcome {
            warning: None,
            moved,
        })
    }

    /// Delete messages permanently (STORE \\Deleted + EXPUNGE), wherever they
    /// are, waiting for the server. Callers wanting the recoverable delete use
    /// [`Store::delete_messages`].
    pub async fn delete_immediate(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
    ) -> Result<WriteOutcome> {
        let account_config = self.account_config(account)?.clone();
        self.with_connection(&account_config, "delete", async |client| {
            self.select_for_uids(client, account, folder).await?;
            client.delete_messages(uids).await
        })
        .await?;

        let warning = self
            .delete_messages_by_uids(account, folder, uids)
            .await
            .err()
            .map(|e| {
                format!(
                    "{account}/{folder}: deleted on the server but still listed here ({e}); \
                     the next sync puts that right"
                )
            });
        if let Some(warning) = &warning {
            crate::logger::warn(warning);
        }
        Ok(WriteOutcome {
            warning,
            moved: Vec::new(),
        })
    }

    /// Move messages to another folder, waiting for the server; the user's
    /// moves go through the outbox instead ([`Store::move_messages`]).
    ///
    /// `UID MOVE` where the server has it, COPY + STORE \\Deleted + a scoped
    /// expunge otherwise.
    pub async fn move_immediate(
        &self,
        account: &str,
        source_folder: &str,
        destination_folder: &str,
        uids: &[u32],
    ) -> Result<WriteOutcome> {
        let account_config = self.account_config(account)?.clone();
        // Read before the rows go: the Message-IDs are how the move is found
        // again in its destination should it have to be undone.
        let moved = self.message_ids_for_uids(account, source_folder, uids);
        self.with_connection(&account_config, "move", async |client| {
            self.select_for_uids(client, account, source_folder).await?;
            client.move_messages(uids, destination_folder).await?;
            Ok(())
        })
        .await?;

        let warning = self
            .delete_messages_by_uids(account, source_folder, uids)
            .await
            .err()
            .map(|e| {
                format!(
                    "{account}/{source_folder}: moved to {destination_folder} on the server but \
                     still listed here ({e}); the next sync puts that right"
                )
            });
        if let Some(warning) = &warning {
            crate::logger::warn(warning);
        }
        Ok(WriteOutcome { warning, moved })
    }

    /// The RFC `Message-ID`s of the given UIDs, or nothing when they cannot be
    /// read — a move that cannot be undone is still a move.
    pub fn message_ids_for_uids(&self, account: &str, folder: &str, uids: &[u32]) -> Vec<String> {
        let ids = self.reader().and_then(|db| {
            let Some(folder) = db.get_folder_by_name(account, folder)? else {
                return Ok(Vec::new());
            };
            db.message_ids_for_uids(folder.id.expect("folder must have id"), uids)
        });
        ids.unwrap_or_else(|error| {
            crate::logger::warn(&format!(
                "{account}/{folder}: could not read the Message-IDs of a move ({error}); \
                 it will not be undoable"
            ));
            Vec::new()
        })
    }

    /// Move messages named by their RFC `Message-ID` out of `from_folder` into
    /// `to_folder`. How a move is undone: the messages now have UIDs this
    /// database may not know, so the server is asked for them by the one
    /// identity that survived the move. Returns how many were moved.
    ///
    /// Messages the server cannot find are skipped, not an error: a message
    /// another client has filed again since is not this undo's to move.
    pub async fn move_by_message_ids(
        &self,
        account: &str,
        from_folder: &str,
        to_folder: &str,
        message_ids: &[String],
    ) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let account_config = self.account_config(account)?.clone();
        let uids = self
            .with_connection(&account_config, "undo move", async |client| {
                client.select_mailbox(from_folder).await?;
                let uids = Self::uids_of_message_ids(client, message_ids).await?;
                if !uids.is_empty() {
                    client.move_messages(&uids, to_folder).await?;
                }
                Ok(uids)
            })
            .await?;

        // The rows of the moved copies, if a sync has stored them meanwhile.
        // Not finding the folder locally is the common case right after a
        // move and not worth a warning.
        if !uids.is_empty()
            && let Err(error) = self
                .delete_messages_by_uids(account, from_folder, &uids)
                .await
            && !matches!(error, Error::NotFound(_))
        {
            crate::logger::warn(&format!(
                "{account}/{from_folder}: moved back to {to_folder} on the server but still \
                 listed here ({error}); the next sync puts that right"
            ));
        }
        Ok(uids.len())
    }

    /// The UIDs the selected folder has for these `Message-ID`s.
    async fn uids_of_message_ids(
        client: &mut ImapClient,
        message_ids: &[String],
    ) -> Result<Vec<u32>> {
        let mut uids = Vec::new();
        for id in message_ids {
            uids.extend(client.uid_search_message_id(id).await?);
        }
        uids.sort_unstable();
        uids.dedup();
        Ok(uids)
    }

    /// The `Message-ID`s of `message_ids` that `folder` has on the server.
    async fn message_ids_present(
        &self,
        account: &str,
        folder: &str,
        message_ids: &[String],
    ) -> Result<Vec<String>> {
        let account_config = self.account_config(account)?.clone();
        self.with_connection(&account_config, "find by Message-ID", async |client| {
            client.select_mailbox(folder).await?;
            let mut present = Vec::new();
            for id in message_ids {
                if !client.uid_search_message_id(id).await?.is_empty() {
                    present.push(id.clone());
                }
            }
            Ok(present)
        })
        .await
    }

    /// Copy the messages named by `Message-ID` from one folder of the account
    /// into another, on the server: how an undone copy is made again.
    /// Returns how many were copied.
    pub async fn copy_by_message_ids(
        &self,
        account: &str,
        from_folder: &str,
        to_folder: &str,
        message_ids: &[String],
    ) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let account_config = self.account_config(account)?.clone();
        self.with_connection(&account_config, "copy", async |client| {
            client.select_mailbox(from_folder).await?;
            let uids = Self::uids_of_message_ids(client, message_ids).await?;
            client.copy_messages(&uids, to_folder).await?;
            Ok(uids.len())
        })
        .await
    }

    /// Delete for good the copies in `folder` of messages named by
    /// `Message-ID`: how a copy is undone. Only a message whose original is
    /// still in `original` is a copy — one whose original has gone since is
    /// the only one left, and is kept. Returns how many were deleted.
    pub async fn delete_copies(
        &self,
        original: (&str, &str),
        account: &str,
        folder: &str,
        message_ids: &[String],
    ) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let originals = self
            .message_ids_present(original.0, original.1, message_ids)
            .await?;
        if originals.is_empty() {
            return Ok(0);
        }
        let account_config = self.account_config(account)?.clone();
        let uids = self
            .with_connection(&account_config, "undo copy", async |client| {
                client.select_mailbox(folder).await?;
                let uids = Self::uids_of_message_ids(client, &originals).await?;
                client.delete_messages(&uids).await?;
                Ok(uids)
            })
            .await?;
        if !uids.is_empty()
            && let Err(error) = self.delete_messages_by_uids(account, folder, &uids).await
            && !matches!(error, Error::NotFound(_))
        {
            crate::logger::warn(&format!(
                "{account}/{folder}: deleted on the server but still listed here ({error}); \
                 the next sync puts that right"
            ));
        }
        Ok(uids.len())
    }

    /// Move or copy the messages named by `Message-ID` into a folder of
    /// another account, as [`Store::transfer_messages`] does: how a move
    /// across accounts is undone, and an undone one made again.
    pub async fn transfer_by_message_ids(
        &self,
        from: (&str, &str),
        to: (&str, &str),
        message_ids: &[String],
        keep_source: bool,
    ) -> Result<TransferOutcome> {
        if message_ids.is_empty() {
            return Ok(TransferOutcome::default());
        }
        let account_config = self.account_config(from.0)?.clone();
        let uids = self
            .with_connection(&account_config, "find by Message-ID", async |client| {
                client.select_mailbox(from.1).await?;
                Self::uids_of_message_ids(client, message_ids).await
            })
            .await?;
        self.transfer_messages(from.0, from.1, to.0, to.1, &uids, keep_source)
            .await
    }

    /// Which of `message_ids` the stored copy of `folder` holds already. A
    /// copy or a move into a folder that has the message before is not the
    /// action's to take back there.
    pub fn stored_message_ids(
        &self,
        account: &str,
        folder: &str,
        message_ids: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        let db = self.reader()?;
        let Some(folder_id) = db.get_folder_by_name(account, folder)?.and_then(|f| f.id) else {
            return Ok(Default::default());
        };
        let mut stored = std::collections::HashSet::new();
        for chunk in message_ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut stmt = db.conn.prepare(&format!(
                "SELECT message_id FROM messages WHERE folder_id = ? AND message_id IN ({placeholders})"
            ))?;
            let parameters = std::iter::once(rusqlite::types::Value::from(folder_id)).chain(
                chunk
                    .iter()
                    .map(|id| rusqlite::types::Value::from(id.clone())),
            );
            for id in stmt.query_map(rusqlite::params_from_iter(parameters), |row| {
                row.get::<_, String>(0)
            })? {
                stored.insert(id?);
            }
        }
        Ok(stored)
    }

    /// Select `folder` on `client` for a command that names UIDs, and refuse
    /// if the UIDs the database knows are no longer the server's.
    ///
    /// A UID means something only under the UIDVALIDITY it was issued with
    /// (RFC 3501 §2.3.1.1). A folder recreated, restored from a backup or
    /// moved to another server numbers its mail afresh, and a delete or move
    /// sent with the old numbers lands on whatever carries them now. The sync
    /// clears the folder when it sees the change; a write that gets there
    /// first stops here instead.
    async fn select_for_uids(
        &self,
        client: &mut ImapClient,
        account: &str,
        folder: &str,
    ) -> Result<()> {
        let stored = self
            .reader()?
            .get_folder_by_name(account, folder)?
            .and_then(|folder| folder.uidvalidity);
        client.select_mailbox(folder).await?;
        let current = client
            .current_mailbox()
            .and_then(|mailbox| mailbox.uid_validity);
        if Self::uidvalidity_mismatch(stored, current) {
            return Err(Error::uidvalidity_changed(
                account,
                folder,
                stored.unwrap_or(0),
                current.unwrap_or(0),
            ));
        }
        Ok(())
    }

    /// The configuration of a named account.
    fn account_config(&self, account: &str) -> Result<AccountConfig> {
        self.get_account_config(account)
            .ok_or_else(|| Error::NotFound(format!("Account '{}' not found", account)))
    }

    /// Run `work` on one of the account's pooled connections.
    ///
    /// Every operation that talks to the server goes through here, so that an
    /// account has one authenticated connection kept warm rather than a TLS
    /// handshake, a LOGIN and a password command per click. The connection
    /// goes back to the pool afterwards unless `work` broke it, and a refused
    /// login forgets the cached password.
    pub(crate) async fn with_connection<T>(
        &self,
        account: &AccountConfig,
        reason: &str,
        work: impl AsyncFnOnce(&mut ImapClient) -> Result<T>,
    ) -> Result<T> {
        self.connected(account, reason, false, work).await
    }

    /// [`Self::with_connection`] for what the user is waiting on: see
    /// [`imap::ConnectionPool::acquire_interactive`].
    pub(crate) async fn with_interactive_connection<T>(
        &self,
        account: &AccountConfig,
        reason: &str,
        work: impl AsyncFnOnce(&mut ImapClient) -> Result<T>,
    ) -> Result<T> {
        self.connected(account, reason, true, work).await
    }

    async fn connected<T>(
        &self,
        account: &AccountConfig,
        reason: &str,
        interactive: bool,
        work: impl AsyncFnOnce(&mut ImapClient) -> Result<T>,
    ) -> Result<T> {
        let password = self.credentials.imap(account).await?;
        self.connection_pool
            .register_account(
                &account.name,
                AccountConnectionConfig::for_account(account, password),
            )
            .await;
        let acquired = if interactive {
            self.connection_pool
                .acquire_interactive(&account.name, reason)
                .await
        } else {
            self.connection_pool.acquire(&account.name, reason).await
        };
        let mut client = match acquired {
            Ok(client) => client,
            Err(error) => {
                self.forget_password_if_refused(account, &error).await;
                return Err(error);
            }
        };
        let result = work(&mut client).await;
        self.connection_pool
            .give_back(&account.name, client, result.as_ref().err())
            .await;
        result
    }

    // ========================================================================
    // Staged (pending) actions
    //
    // The user marks delete/move actions, reviews them, then executes (flush)
    // or discards. Unlike delete_immediate/move_immediate these do NOT touch the
    // IMAP server when staged: the actions are persisted in the `pending_actions`
    // table (surviving relaunch and shared across windows) and only reach the
    // server when `execute_pending` applies them via the same safe paths.
    // ========================================================================

    /// Resolve the configured Trash folder name for an account, falling back to
    /// the default ("Trash") when the account is unknown.
    pub(crate) fn trash_folder_for(&self, account: &str) -> String {
        self.config()
            .accounts
            .iter()
            .find(|a| a.name == account)
            .map(|a| a.trash_folder.clone())
            .unwrap_or_else(|| "Trash".to_string())
    }

    /// Stage a destructive action (`delete` or `move`) for one or more messages.
    ///
    /// Persists one row per UID in the `pending_actions` table. Staging again for
    /// a message that already has a pending action replaces it (last action
    /// wins). A `move` requires a non-empty `target_folder`; a `delete` ignores
    /// it.
    pub async fn stage_action(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        action_type: &str,
        target_folder: Option<String>,
    ) -> Result<()> {
        if action_type != PENDING_ACTION_DELETE && action_type != PENDING_ACTION_MOVE {
            return Err(Error::InvalidInput(format!(
                "Unknown pending action type '{}'",
                action_type
            )));
        }
        let target = if action_type == PENDING_ACTION_MOVE {
            match target_folder.as_deref() {
                Some(t) if !t.is_empty() => Some(t.to_string()),
                _ => {
                    return Err(Error::InvalidInput(
                        "A staged move requires a non-empty target folder".to_string(),
                    ));
                }
            }
        } else {
            None
        };

        if uids.is_empty() {
            return Ok(());
        }

        let _write_guard = self.write_lock.lock().await;
        let db = Database::new(&self.db_path)?;
        let created_at = chrono::Utc::now().to_rfc3339();
        for uid in uids {
            db.stage_pending_action(
                account,
                folder,
                *uid,
                action_type,
                target.as_deref(),
                &created_at,
            )?;
        }
        Ok(())
    }

    /// Remove a single staged action (unmark a message).
    pub async fn unstage_action(&self, account: &str, folder: &str, uids: &[u32]) -> Result<()> {
        let _write_guard = self.write_lock.lock().await;
        let db = Database::new(&self.db_path)?;
        db.unstage_pending_actions(account, folder, uids)?;
        Ok(())
    }

    /// The staged actions on these UIDs of a folder.
    pub fn pending_for_uids(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
    ) -> Result<Vec<PendingActionRecord>> {
        let wanted: std::collections::HashSet<u32> = uids.iter().copied().collect();
        let mut records = self.list_pending(&PendingScope::Folder {
            account_name: account.to_string(),
            folder_name: folder.to_string(),
        })?;
        records.retain(|record| wanted.contains(&record.uid));
        Ok(records)
    }

    /// Make the staged actions on these UIDs of a folder exactly `actions`:
    /// how staging, unstaging and discarding are undone.
    pub async fn restore_pending(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        actions: &[PendingActionRecord],
    ) -> Result<()> {
        let _write_guard = self.write_lock.lock().await;
        let db = Database::new(&self.db_path)?;
        db.unstage_pending_actions(account, folder, uids)?;
        for action in actions {
            db.stage_pending_action(
                account,
                folder,
                action.uid,
                &action.action_type,
                action.target_folder.as_deref(),
                &action.created_at,
            )?;
        }
        Ok(())
    }

    /// Every UID in a folder, in one query.
    ///
    /// For acting on a whole folder without reading it. The list view is paged
    /// — deliberately, since a folder can hold hundreds of thousands of
    /// messages — but "select all, then archive" needs only the UIDs, and
    /// fetching them as pages of message headers means a round trip per two
    /// hundred rows to assemble a number the database can produce in one.
    pub fn folder_uids(&self, account: &str, folder: &str) -> Result<Vec<u32>> {
        let db = Database::new(&self.db_path)?;
        let Some(folder) = db.get_folder_by_name(account, folder)? else {
            return Ok(Vec::new());
        };
        let Some(folder_id) = folder.id else {
            return Ok(Vec::new());
        };
        db.get_folder_uids(folder_id)
    }

    /// List staged actions within a scope (whole mailbox / account / folder).
    pub fn list_pending(&self, scope: &PendingScope) -> Result<Vec<PendingActionRecord>> {
        let db = Database::new(&self.db_path)?;
        db.list_pending_actions(scope)
    }

    /// The staged actions within a scope, each with the message it names when
    /// that message is still stored. For a review that has to show a subject
    /// rather than a UID.
    pub fn list_pending_detailed(
        &self,
        scope: &PendingScope,
    ) -> Result<Vec<(PendingActionRecord, Option<PendingMessage>)>> {
        let db = Database::new(&self.db_path)?;
        let records = db.list_pending_actions(scope)?;
        let mut folder_ids: std::collections::HashMap<(String, String), Option<i64>> =
            std::collections::HashMap::new();
        let mut detailed = Vec::with_capacity(records.len());
        for record in records {
            let key = (record.account.clone(), record.folder.clone());
            let folder_id = match folder_ids.get(&key) {
                Some(id) => *id,
                None => {
                    let id = db
                        .get_folder_by_name(&record.account, &record.folder)?
                        .and_then(|f| f.id);
                    folder_ids.insert(key, id);
                    id
                }
            };
            let message = match folder_id {
                Some(folder_id) => {
                    db.message_headline(folder_id, record.uid)?
                        .map(|h| PendingMessage {
                            id: h.id,
                            subject: h.subject,
                            from: h.from,
                        })
                }
                None => None,
            };
            detailed.push((record, message));
        }
        Ok(detailed)
    }

    /// The unread mail among the messages `ids` name, which a sync just
    /// brought to `folder`, if the folder is watched; oldest first.
    ///
    /// Watched means INBOX and the folders marked `watch`: the ones whose mail
    /// is wanted the moment it arrives, which is also the mail worth a
    /// notification. Asked after the filter rules ran, so a message a rule
    /// filed away is gone and one it marked read is not unread. The tasks
    /// folder is never synced as mail, so it cannot appear here.
    pub(crate) fn new_mail_among(
        &self,
        account: &AccountConfig,
        folder: &str,
        ids: &[i64],
    ) -> Result<Vec<crate::db::NewMessage>> {
        if ids.is_empty()
            || !crate::services::scheduler::watched_folders(account)
                .iter()
                .any(|watched| watched.eq_ignore_ascii_case(folder))
        {
            return Ok(Vec::new());
        }
        self.reader()?.unread_messages_among(ids)
    }

    /// Unread mail in every account's INBOX, for the Dock badge.
    pub fn inbox_unread_count(&self) -> Result<u32> {
        self.reader()?.inbox_unread_count()
    }

    /// The unread mail in `folder` of `account`, or of every account.
    pub fn unread_count_in(&self, account: Option<&str>, folder: &str) -> Result<u32> {
        self.reader()?.unread_count_in(account, folder)
    }

    /// Fetch the full source of messages that have attachments and a body but
    /// no source yet, so their attachments can be opened offline.
    ///
    /// Runs after the body backfill has done a folder, with whatever budget is
    /// left. Returns how many sources were stored.
    pub async fn fetch_pending_sources(
        &self,
        account_name: &str,
        folders: &[String],
        budget: usize,
    ) -> Result<usize> {
        let account = self
            .get_account_config(account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {account_name} not found")))?;

        let mut fetched = 0usize;
        let mut remaining = budget;
        for folder_name in folders {
            if remaining == 0 {
                break;
            }
            let pending = {
                let db = self.reader()?;
                let Some(folder) = db.get_folder_by_name(account_name, folder_name)? else {
                    continue;
                };
                db.messages_needing_source(folder.id.expect("folder must have id"), remaining)?
            };
            if pending.is_empty() {
                continue;
            }

            let uids: Vec<u32> = pending.iter().map(|(_, uid)| *uid).collect();
            let sources = self
                .with_connection(&account, "source backfill", async |client| {
                    client.select_mailbox(folder_name).await?;
                    client.fetch_bodies(&uids).await
                })
                .await?;
            let by_uid: std::collections::HashMap<u32, Vec<u8>> = sources.into_iter().collect();

            let _guard = self.write_lock.lock().await;
            let db = self.reader()?;
            for (message_id, uid) in &pending {
                // A UID the server no longer has is left alone; the next sync
                // removes the row, so this cannot loop forever.
                if let Some(source) = by_uid.get(uid) {
                    db.set_message_source(*message_id, source)?;
                    fetched += 1;
                }
            }
            remaining = remaining.saturating_sub(pending.len());
        }
        Ok(fetched)
    }

    /// Fetch the HTML part of the newest messages in each folder, so that
    /// opening recent mail does not wait on the server.
    ///
    /// The body backfill stores only the text, which is what search needs; the
    /// reader wants the HTML wherever there is one. Fetching it for every
    /// message would cost gigabytes, so only the newest
    /// `RECENT_HTML_WINDOW` per folder get it. Returns how many were stored.
    pub async fn fetch_recent_html(
        &self,
        account_name: &str,
        folders: &[String],
        budget: usize,
    ) -> Result<usize> {
        let account = self
            .get_account_config(account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {account_name} not found")))?;

        let mut stored = 0usize;
        let mut remaining = budget;
        for folder_name in folders {
            if remaining == 0 {
                break;
            }
            let pending = {
                let db = self.reader()?;
                let Some(folder) = db.get_folder_by_name(account_name, folder_name)? else {
                    continue;
                };
                db.pending_recent_html(
                    folder.id.expect("folder must have id"),
                    RECENT_HTML_WINDOW,
                    remaining,
                )?
            };
            if pending.is_empty() {
                continue;
            }

            let fetched = self
                .with_connection(&account, "html prefetch", async |client| {
                    client.select_mailbox(folder_name).await?;
                    let mut fetched = std::collections::HashMap::new();
                    for (part, uids) in crate::tasks::bodies::group_by_part(&pending) {
                        for chunk in crate::tasks::bodies::batches(&uids, BODY_BATCH_SIZE) {
                            fetched.extend(client.fetch_parts(&chunk, part).await?);
                        }
                    }
                    Ok(fetched)
                })
                .await?;

            let _guard = self.write_lock.lock().await;
            let db = self.reader()?;
            for message in &pending {
                // A part the server did not produce is left for the reader to
                // fetch whole when the message is opened.
                if let Some(html) = fetched.get(&message.uid) {
                    db.set_message_body_html(message.message_id, html)?;
                    stored += 1;
                }
            }
            remaining = remaining.saturating_sub(pending.len());
        }
        Ok(stored)
    }

    /// How many messages still want their source for their attachments.
    pub fn source_backfill_pending(&self) -> Result<usize> {
        self.reader()?.source_backfill_pending()
    }

    /// Count staged actions per (account, folder) within a scope.
    pub fn pending_counts(&self, scope: &PendingScope) -> Result<Vec<PendingActionCount>> {
        let db = Database::new(&self.db_path)?;
        db.pending_action_counts(scope)
    }

    /// Discard staged actions within a scope without applying them.
    ///
    /// Returns the number of staged actions removed.
    pub async fn discard_pending(&self, scope: &PendingScope) -> Result<usize> {
        let _write_guard = self.write_lock.lock().await;
        let db = Database::new(&self.db_path)?;
        db.clear_pending_actions(scope)
    }

    /// Execute (flush) staged actions within a scope.
    ///
    /// The staged messages leave the stored copy at once and their server
    /// writes are queued in the outbox ([`Self::queue_move`]); the caller
    /// drains it ([`Self::drain_outbox`]). Each write resolves the way the
    /// immediate action does:
    /// - `move`   -> a move to the recorded target folder.
    /// - `delete` -> a move to the account's Trash folder, matching the
    ///   recoverable delete used everywhere else; when the message already
    ///   lives in Trash the delete is permanent, matching the established
    ///   in-Trash behavior. Delete never silently hard-deletes outside Trash.
    ///
    /// Returns the number of staged actions executed.
    pub async fn execute_pending(&self, scope: &PendingScope) -> Result<PendingExecution> {
        let rows = {
            let db = Database::new(&self.db_path)?;
            db.list_pending_actions(scope)?
        };
        if rows.is_empty() {
            return Ok(PendingExecution::default());
        }

        // Group by (account, folder, action_type, target) so each distinct
        // operation is one queued write and one IMAP round-trip.
        let mut groups: std::collections::BTreeMap<
            (String, String, String, Option<String>),
            Vec<u32>,
        > = std::collections::BTreeMap::new();
        for row in &rows {
            groups
                .entry((
                    row.account.clone(),
                    row.folder.clone(),
                    row.action_type.clone(),
                    row.target_folder.clone(),
                ))
                .or_default()
                .push(row.uid);
        }

        let mut execution = PendingExecution::default();
        for ((account, folder, action_type, target), uids) in groups {
            let trash = self.trash_folder_for(&account);
            let destination = match resolve_pending_execution_op(
                &action_type,
                &folder,
                &trash,
                target.as_deref(),
            )? {
                PendingExecutionOp::Move { target } => Some(target),
                PendingExecutionOp::MoveToTrash { trash } => Some(trash),
                // Already in Trash: staged delete is permanent, matching
                // confirmPermanentDelete() in the UI.
                PendingExecutionOp::PermanentDelete => None,
            };
            if let Some(to) = &destination {
                let ids = self.message_ids_for_uids(&account, &folder, &uids);
                execution
                    .moves
                    .push((account.clone(), folder.clone(), to.clone(), ids));
            }
            self.queue_move(&account, &folder, destination.as_deref(), &uids)
                .await?;

            // Clear only the rows that still match the snapshot queued, so a
            // concurrent re-stage isn't clobbered.
            let _write_guard = self.write_lock.lock().await;
            let db = Database::new(&self.db_path)?;
            db.delete_pending_actions_matching(
                &account,
                &folder,
                &uids,
                &action_type,
                target.as_deref(),
            )?;
            drop(_write_guard);
            execution.executed += uids.len();
        }

        Ok(execution)
    }
}

/// How much of a message a caller needs fetched.
///
/// The distinction exists because a message can be partly stored: the
/// background pass keeps the text part and nothing else, which is enough to
/// read and not enough to show as HTML or as source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyDetail {
    /// Something to read — text or HTML, whichever the message has.
    Readable,
    /// The full RFC822 source, headers and all.
    Source,
}

impl BodyDetail {
    /// Is there enough stored to answer this at all?
    ///
    /// Weaker than the test for whether a fetch can be skipped: that one asks
    /// for everything the message is known to have, this one asks whether
    /// there is anything worth putting on screen. The gap between them is a
    /// message whose text is stored and whose HTML never arrived — worth
    /// reading, and unreachable while the only two answers were "complete" and
    /// "error".
    fn is_readable(&self, message: &crate::models::Message) -> bool {
        match self {
            BodyDetail::Readable => message.body_text.is_some() || message.body_html.is_some(),
            BodyDetail::Source => message.rfc822.is_some(),
        }
    }
}

/// Load a message, going to the server for whatever is not cached yet.
///
/// A message that cannot be completed is still returned if what is already
/// stored can be read. The server is not the only place a message lives once
/// it has been synced, and a message deleted or moved elsewhere — in another
/// client, by a rule, by anyone — would otherwise become unreadable here the
/// moment it went, taking a body already sitting in the database with it. The
/// reader showed its "select a message" placeholder instead, which says the
/// wrong thing twice: nothing is wrong, and nothing is selected.
///
/// Only a message with nothing readable stored fails outright, because then
/// there is genuinely nothing to show.
pub async fn fetch_message_body(
    store: &Store,
    message_id: i64,
    want: BodyDetail,
) -> Result<crate::models::Message> {
    load_message_body(store, message_id, want, BodyFetch::Complete)
        .await
        .map(|(message, _)| message)
}

/// How far loading a message goes for what is not stored yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFetch {
    /// To the server for whatever is missing.
    Complete,
    /// To the server only when nothing readable is stored. The reader asks
    /// this way first, so that a message whose text is here opens at once
    /// on a slow connection or none, and asks for the rest afterwards.
    StoredFirst,
}

/// Load a message as [`fetch_message_body`] does, saying whether it is
/// complete: `false` when a part it has is still on the server, because
/// `fetch` said not to go there or because going there failed.
pub async fn load_message_body(
    store: &Store,
    message_id: i64,
    want: BodyDetail,
    fetch: BodyFetch,
) -> Result<(crate::models::Message, bool)> {
    match complete_message(store, message_id, want, fetch).await {
        Ok(found) => Ok(found),
        Err(e) => {
            let cached = store
                .reader()
                .and_then(|db| db.get_message_by_id(message_id))
                .ok()
                .flatten()
                .filter(|m| want.is_readable(m));
            match cached {
                Some(message) => {
                    crate::logger::warn(&format!(
                        "message {message_id}: showing what is stored, \
                         the rest could not be fetched: {e}"
                    ));
                    Ok((message, false))
                }
                None => Err(e),
            }
        }
    }
}

/// Fetch whatever the message is missing.
///
/// 1. Read message/folder info from database
/// 2. Fetch body from IMAP on a pooled connection (no locks held)
/// 3. Write body to database (acquires write_lock to prevent FTS corruption)
///
/// Returns immediately when everything asked for is already stored, and with
/// [`BodyFetch::StoredFirst`] also when what is stored can be read; the flag
/// says whether the message is complete.
async fn complete_message(
    store: &Store,
    message_id: i64,
    want: BodyDetail,
    fetch: BodyFetch,
) -> Result<(crate::models::Message, bool)> {
    // Phase 1: Read necessary data from database
    let (message, folder, account_config) = {
        let db = store.reader()?;

        let message = db
            .get_message_by_id(message_id)?
            .ok_or_else(|| Error::NotFound(format!("Message {} not found", message_id)))?;

        // Already have what was asked for?
        //
        // "Has a body" is not the same as "has everything". The background
        // pass stores only the text part, so a message with both renderings
        // arrives here with its text filled in and its HTML missing — and the
        // full source missing too. Returning early on the text alone meant the
        // HTML could never be offered and the source could never be shown.
        let has_html = message.body_html.is_some();
        let expects_html = db.message_has_html_part(message_id)?;
        let satisfied = match want {
            BodyDetail::Source => message.rfc822.is_some(),
            BodyDetail::Readable => {
                // Having the HTML settles it. Having only the text settles
                // it only when there is no HTML part to be missing.
                has_html || (message.body_text.is_some() && !expects_html)
            }
        };
        if satisfied {
            return Ok((message, true));
        }

        // The source prefetched for the attachments holds the HTML too, so
        // the reader need not ask the server for what is already here.
        if matches!(want, BodyDetail::Readable)
            && let Some(source) = message.rfc822.as_deref()
            && let Some(html) = crate::imap::extract_body_html(source)
        {
            drop(db);
            let _write_guard = store.write_lock.lock().await;
            store.reader()?.set_message_body_html(message_id, &html)?;
            return Ok((
                crate::models::Message {
                    body_html: Some(html),
                    ..message
                },
                true,
            ));
        }

        if fetch == BodyFetch::StoredFirst && want.is_readable(&message) {
            return Ok((message, false));
        }

        let folder = db
            .get_folder_by_id(message.folder_id)?
            .ok_or_else(|| Error::NotFound(format!("Folder {} not found", message.folder_id)))?;

        let account = store
            .get_account_config(&folder.account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {} not found", folder.account_name)))?;

        (message, folder, account)
    };

    // Phase 2: Network I/O (no locks held)
    let (body_text, body_html, rfc822) = store
        .with_interactive_connection(&account_config, "open message", async |client| {
            store
                .select_for_uids(client, &folder.account_name, &folder.name)
                .await?;
            client.fetch_message_body(message.uid).await
        })
        .await?;

    // Phase 3: Write results to database (acquire write lock to prevent FTS corruption)
    {
        let _write_guard = store.write_lock.lock().await;
        let db = store.reader()?;
        db.update_message_body(
            message_id,
            body_text.as_deref(),
            body_html.as_deref(),
            rfc822.as_deref(),
        )?;
    }

    // Return updated message (construct in memory to avoid extra DB read)
    Ok((
        crate::models::Message {
            body_text,
            body_html,
            rfc822,
            ..message
        },
        true,
    ))
}

/// The header fields a reply is built from, as asked of the server.
const REPLY_HEADER_FIELDS: &[&str] = &[
    "MESSAGE-ID",
    "BCC",
    "IN-REPLY-TO",
    "REFERENCES",
    "REPLY-TO",
    "DELIVERED-TO",
];

/// The headers of a message that a reply to it needs.
///
/// Nothing is stored: a reply is rare enough that fetching twice costs less
/// than a column.
pub async fn fetch_reply_headers(
    store: &Store,
    message_id: i64,
) -> Result<crate::draft::ParentHeaders> {
    fetch_header_fields(
        store,
        message_id,
        REPLY_HEADER_FIELDS,
        "reply headers",
        crate::draft::ParentHeaders::parse,
    )
    .await
}

/// What a message's headers say about its sender, for the reader.
///
/// Asked for each message read; the fields are a few hundred bytes, and
/// storing them would mean a column for every message ever synced.
pub async fn fetch_sender_check(
    store: &Store,
    message_id: i64,
) -> Result<crate::safety::SenderCheck> {
    fetch_header_fields(
        store,
        message_id,
        crate::safety::SENDER_HEADER_FIELDS,
        "sender headers",
        crate::safety::check_sender,
    )
    .await
}

/// Some header fields of a message, read by `parse`.
///
/// Read off the stored source when there is one, and asked of the server
/// otherwise — as the named fields alone, a few hundred bytes, where the
/// whole source of a message with attachments could be megabytes.
async fn fetch_header_fields<T>(
    store: &Store,
    message_id: i64,
    fields: &[&str],
    purpose: &str,
    parse: fn(&[u8]) -> T,
) -> Result<T> {
    let (message, folder, account_config) = {
        let db = store.reader()?;
        let message = db
            .get_message_by_id(message_id)?
            .ok_or_else(|| Error::NotFound(format!("Message {} not found", message_id)))?;
        if let Some(source) = &message.rfc822 {
            return Ok(parse(source));
        }

        let folder = db
            .get_folder_by_id(message.folder_id)?
            .ok_or_else(|| Error::NotFound(format!("Folder {} not found", message.folder_id)))?;
        let account = store
            .get_account_config(&folder.account_name)
            .ok_or_else(|| Error::NotFound(format!("Account {} not found", folder.account_name)))?;
        (message, folder, account)
    };

    let listed = store
        .with_interactive_connection(&account_config, purpose, async |client| {
            store
                .select_for_uids(client, &folder.account_name, &folder.name)
                .await?;
            client
                .fetch_header_listing(&message.uid.to_string(), fields)
                .await
        })
        .await?;
    let listing = listed
        .into_iter()
        .find(|listing| listing.uid == message.uid)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "Message UID {} not found on server (may have been deleted)",
                message.uid
            ))
        })?;
    Ok(parse(&listing.headers))
}

#[cfg(test)]
mod tests {
    /// A config with no accounts in it, so anything needing the network fails
    /// before it can reach for it.
    fn config_without_accounts() -> Config {
        serde_saphyr::from_str("db_path: ignored\naccounts: []\n")
            .expect("a config with no accounts is still a config")
    }

    use super::*;
    use rusqlite::params;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// A message whose text is stored, whose HTML never arrived, and whose copy
    /// on the server has since gone — archived in another client, moved by a
    /// rule, deleted by anyone. It used to become unreadable at that moment,
    /// with the reader showing "select a message" over a body sitting in the
    /// database.
    #[tokio::test]
    async fn a_message_still_reads_when_the_server_no_longer_has_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();

        let message_id = {
            let mut db = crate::db::Database::new(&db_path).expect("db");
            db.init_schema().expect("schema");
            let folder_id = db.get_or_create_folder("Gone", "INBOX").expect("folder");
            db.insert_message(&crate::models::Message {
                id: None,
                folder_id,
                uid: 1,
                message_id: None,
                subject: Some("Approved".into()),
                from: None,
                to: None,
                cc: None,
                date: None,
                // The text is here; the HTML never was.
                body_text: Some("The absence request was approved.".into()),
                body_html: None,
                rfc822: None,
                size: None,
                flags: Vec::new(),
                modseq: None,
                shape: None,
                headers: None,
            })
            .expect("insert")
        };

        // No account called "Gone" is configured, so completing the message
        // cannot even begin — the same shape of failure as a message the server
        // no longer has, and it needs no network to reproduce.
        let mut config = config_without_accounts();
        config.db_path = db_path;
        let store = Store::new(config).expect("a store");
        let message = fetch_message_body(&store, message_id, BodyDetail::Readable)
            .await
            .expect("what is stored must still be readable");
        assert_eq!(
            message.body_text.as_deref(),
            Some("The absence request was approved.")
        );

        // The full source is a different question: nothing of it is stored, so
        // there is genuinely nothing to show and the failure must surface.
        let source = fetch_message_body(&store, message_id, BodyDetail::Source).await;
        assert!(source.is_err(), "a source nobody has cannot be invented");
    }

    fn create_test_db() -> Database {
        let db = Database::new(":memory:").expect("failed to create test database");
        db.init_schema().expect("failed to run migrations");
        db
    }

    /// Selecting a whole folder and deleting it hands the local delete more
    /// UIDs than SQLite binds in one statement (32,766), and the statement
    /// failed after the server had already carried the delete out.
    #[tokio::test]
    async fn a_local_delete_of_more_uids_than_sqlite_binds_still_goes_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let folder_id = {
            let mut db = Database::new(&db_path).expect("db");
            db.init_schema().expect("schema");
            let folder_id = db.get_or_create_folder("Big", "INBOX").expect("folder");
            for uid in [1, 20_000, 40_000] {
                db.insert_message(&crate::models::Message {
                    id: None,
                    folder_id,
                    uid,
                    message_id: None,
                    subject: Some(format!("Message {uid}")),
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
                .expect("insert");
            }
            folder_id
        };

        let mut config = config_without_accounts();
        config.db_path = db_path.clone();
        let store = Store::new(config).expect("a store");

        let every_uid: Vec<u32> = (1..=40_000).collect();
        let deleted = store
            .delete_messages_by_uids("Big", "INBOX", &every_uid)
            .await
            .expect("a delete of a whole folder must not trip over the bind limit");
        assert_eq!(deleted, 3);

        let db = Database::new(&db_path).expect("db");
        assert_eq!(db.get_message_count(folder_id).expect("count"), 0);
    }

    #[test]
    fn test_errored_connection_is_not_returned_to_pool() {
        // Regression: a client that hit an unrecovered connection/IO error must
        // NOT be re-pooled, otherwise the broken connection is handed to the
        // next operation, which then fails too.
        let io_err = Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        ));
        assert!(
            !Store::should_release_to_pool(Some(&io_err)),
            "a connection/IO error must not be returned to the pool"
        );

        // The actual value the sync loop feeds this helper in production: after a
        // server drop, per-folder retries + reconnects are exhausted and the
        // failure is surfaced as `RetryExhausted`, while the still-held client is
        // the dead one. `RetryExhausted::is_connection_error()` is false, so the
        // old helper (keyed only on `is_connection_error`) wrongly re-pooled the
        // broken connection — this asserts the fix drops it instead.
        let retry_exhausted = Error::RetryExhausted {
            operation: "sync folder 'INBOX'".to_string(),
            attempts: 3,
            last_error: "connection reset by peer".to_string(),
        };
        assert!(
            !Store::should_release_to_pool(Some(&retry_exhausted)),
            "an exhausted-retry (dead) connection must not be returned to the pool"
        );
        assert!(
            Store::is_broken_connection_error(&retry_exhausted),
            "RetryExhausted must be classified as a broken connection"
        );

        // A healthy client (no fatal connection error) is reused.
        assert!(
            Store::should_release_to_pool(None),
            "a healthy connection must be returned to the pool"
        );

        // A non-connection error (e.g. bad data) leaves the socket healthy, so
        // it may still be reused rather than needlessly dropped.
        let data_err = Error::NotFound("message 42".to_string());
        assert!(
            Store::should_release_to_pool(Some(&data_err)),
            "a non-connection error must still allow pool reuse"
        );
        assert!(
            !Store::is_broken_connection_error(&data_err),
            "a non-connection error must not be classified as a broken connection"
        );
    }

    #[test]
    fn test_empty_uid_response_with_nonempty_cache_and_exists_deletes_nothing() {
        // Regression: an empty server UID list must NOT be treated as "everything
        // was deleted" when the mailbox reports EXISTS > 0 and we hold cached UIDs.
        let stored_uids = vec![1u32, 2, 3, 4, 5];
        let server_uids: Vec<u32> = Vec::new();

        let deleted = Store::compute_uid_deletions(&stored_uids, &server_uids, Some(5), "INBOX");

        assert!(
            deleted.is_empty(),
            "expected no deletions when server UID list is empty but EXISTS > 0, got {:?}",
            deleted
        );
    }

    #[test]
    fn test_empty_uid_response_with_empty_mailbox_deletes_all() {
        // When the mailbox genuinely reports EXISTS == 0, an empty server UID
        // list is consistent and all cached UIDs should be reported as deleted.
        let stored_uids = vec![1u32, 2, 3];
        let server_uids: Vec<u32> = Vec::new();

        let deleted = Store::compute_uid_deletions(&stored_uids, &server_uids, Some(0), "INBOX");

        assert_eq!(deleted, vec![1, 2, 3]);
    }

    #[test]
    fn test_uid_comparison_reports_only_missing_uids() {
        // Normal deletion detection: UIDs missing from the server set are deleted.
        let stored_uids = vec![1u32, 2, 3, 4];
        let server_uids = vec![1u32, 3];

        let mut deleted =
            Store::compute_uid_deletions(&stored_uids, &server_uids, Some(2), "INBOX");
        deleted.sort_unstable();

        assert_eq!(deleted, vec![2, 4]);
    }

    #[test]
    fn test_unchanged_modseq_shortcut_only_applies_with_qresync() {
        // Baseline conditions that would normally justify skipping deletion
        // detection: we have a stored modseq, it is unchanged, and CHANGEDSINCE
        // returned no messages.
        let baseline_modseq = Some(42u64);
        let current_modseq = 42u64;
        let no_changed_messages = true;

        // On a QRESYNC server, an unchanged modseq is trustworthy: skip is allowed.
        assert!(
            Store::can_skip_deletion_detection_on_unchanged_modseq(
                true,
                baseline_modseq,
                current_modseq,
                no_changed_messages,
                7,
                Some(7),
            ),
            "QRESYNC server with unchanged modseq should be allowed to skip deletion detection"
        );

        // On a plain CONDSTORE server (no QRESYNC), an EXPUNGE need not bump
        // HIGHESTMODSEQ, so the shortcut is unsound and must NOT be taken —
        // otherwise expunged messages linger as ghosts.
        assert!(
            !Store::can_skip_deletion_detection_on_unchanged_modseq(
                false,
                baseline_modseq,
                current_modseq,
                no_changed_messages,
                7,
                Some(7),
            ),
            "non-QRESYNC server must not skip deletion detection on unchanged modseq"
        );
    }

    #[test]
    fn test_unchanged_modseq_shortcut_requires_all_baseline_conditions() {
        // Even with QRESYNC, the shortcut only applies when there is a baseline
        // modseq, the modseq is unchanged, and no messages changed.
        assert!(
            !Store::can_skip_deletion_detection_on_unchanged_modseq(
                true,
                None,
                0,
                true,
                7,
                Some(7)
            ),
            "no baseline modseq (first sync) must not skip deletion detection"
        );
        assert!(
            !Store::can_skip_deletion_detection_on_unchanged_modseq(
                true,
                Some(42),
                43,
                true,
                7,
                Some(7)
            ),
            "changed modseq must not skip deletion detection"
        );
        assert!(
            !Store::can_skip_deletion_detection_on_unchanged_modseq(
                true,
                Some(42),
                42,
                false,
                7,
                Some(7)
            ),
            "changed messages present must not skip deletion detection"
        );
    }

    /// Drive the halving algorithm against a set of UIDs the "server" cannot
    /// describe, mirroring `fetch_shapes_isolating` without a live connection.
    ///
    /// Returns which UIDs were isolated, which were read, and how many fetches
    /// it took.
    fn narrow_over(uids: &[u32], bad: &[u32]) -> (Vec<u32>, Vec<u32>, usize) {
        let mut queue: Vec<Vec<u32>> = vec![uids.to_vec()];
        let (mut unreadable, mut read, mut attempts) = (Vec::new(), Vec::new(), 0);

        while let Some(batch) = queue.pop() {
            attempts += 1;
            assert!(attempts < 10_000, "narrowing must terminate");
            if batch.iter().any(|uid| bad.contains(uid)) {
                match batch.as_slice() {
                    [uid] => unreadable.push(*uid),
                    _ => {
                        let middle = batch.len() / 2;
                        queue.push(batch[middle..].to_vec());
                        queue.push(batch[..middle].to_vec());
                    }
                }
            } else {
                read.extend(batch);
            }
        }

        unreadable.sort_unstable();
        read.sort_unstable();
        (unreadable, read, attempts)
    }

    #[test]
    fn a_batch_that_parses_is_never_split() {
        let uids: Vec<u32> = (1..=300).collect();
        let (unreadable, read, attempts) = narrow_over(&uids, &[]);
        assert!(unreadable.is_empty());
        assert_eq!(read, uids);
        assert_eq!(
            attempts, 1,
            "the whole point is one round trip when all is well"
        );
    }

    #[test]
    fn one_bad_message_is_isolated_and_the_rest_still_arrive() {
        // The failure this was written for: a single message with a NIL media
        // type used to fail the whole batch, and every later pass over it.
        let uids: Vec<u32> = (1..=300).collect();
        let (unreadable, read, attempts) = narrow_over(&uids, &[177]);

        assert_eq!(unreadable, vec![177]);
        assert_eq!(read.len(), 299, "the other 299 must not be lost with it");
        assert!(!read.contains(&177));
        assert!(
            attempts < 25,
            "halving should cost about 2·log2(300) fetches, not 300; got {attempts}"
        );
    }

    #[test]
    fn several_bad_messages_are_all_found() {
        let uids: Vec<u32> = (1..=64).collect();
        let bad = [1, 2, 33, 64];
        let (unreadable, read, _) = narrow_over(&uids, &bad);

        assert_eq!(unreadable, bad);
        assert_eq!(read.len(), 60);
    }

    #[test]
    fn every_uid_is_accounted_for_exactly_once() {
        // A UID dropped by the split would be neither stored nor marked, and
        // so would be re-selected on every pass forever — the very loop this
        // is meant to break.
        let uids: Vec<u32> = (1..=100).collect();
        let (unreadable, read, _) = narrow_over(&uids, &[7, 8, 9, 50]);

        let mut all: Vec<u32> = read.iter().chain(unreadable.iter()).copied().collect();
        all.sort_unstable();
        assert_eq!(all, uids);
    }

    #[test]
    fn a_wholly_unreadable_batch_still_terminates() {
        let uids: Vec<u32> = (1..=16).collect();
        let (unreadable, read, _) = narrow_over(&uids, &uids);
        assert_eq!(unreadable, uids);
        assert!(read.is_empty());
    }

    fn synced_folder(uidvalidity: u32, highest_uid: u32, modseq: u64) -> Folder {
        Folder {
            id: Some(1),
            account_name: "Work".to_string(),
            name: "Archive".to_string(),
            uidvalidity: Some(uidvalidity),
            highest_uid: Some(highest_uid),
            highest_modseq: Some(modseq),
            last_sync_time: None,
            sync_version: 0,
        }
    }

    fn server_state(
        messages: u32,
        uid_next: u32,
        uid_validity: u32,
        modseq: u64,
    ) -> crate::imap::FolderState {
        crate::imap::FolderState {
            messages,
            uid_next: Some(uid_next),
            uid_validity: Some(uid_validity),
            highest_modseq: Some(modseq),
        }
    }

    /// Skipping a folder is only safe when every signal agrees. Each of these
    /// is a change that exactly one signal reports.
    #[test]
    fn a_folder_is_skipped_only_when_nothing_at_all_has_moved() {
        let stored = synced_folder(1, 500, 85);

        // Nothing moved: the whole point.
        assert!(Store::folder_is_unchanged(
            &stored,
            300,
            &server_state(300, 501, 1, 85)
        ));

        // New mail: UIDNEXT moves, and on a frozen-MODSEQ server it is the only
        // thing that does.
        assert!(!Store::folder_is_unchanged(
            &stored,
            300,
            &server_state(301, 502, 1, 85)
        ));

        // Mail deleted: the count moves while UIDNEXT does not.
        assert!(!Store::folder_is_unchanged(
            &stored,
            300,
            &server_state(299, 501, 1, 85)
        ));

        // A flag changed: only MODSEQ moves. Missing this would let a message
        // read elsewhere stay unread here forever.
        assert!(!Store::folder_is_unchanged(
            &stored,
            300,
            &server_state(300, 501, 1, 86)
        ));

        // UIDVALIDITY changed: every stored UID is meaningless.
        assert!(!Store::folder_is_unchanged(
            &stored,
            300,
            &server_state(300, 501, 2, 85)
        ));

        // Our own count disagrees with the server's, whatever the counters say.
        assert!(!Store::folder_is_unchanged(
            &stored,
            299,
            &server_state(300, 501, 1, 85)
        ));
    }

    #[test]
    fn a_folder_without_a_baseline_is_never_skipped() {
        // First sync: nothing to compare against.
        let mut never_synced = synced_folder(1, 500, 85);
        never_synced.highest_modseq = None;
        assert!(!Store::folder_is_unchanged(
            &never_synced,
            300,
            &server_state(300, 501, 1, 85)
        ));

        let mut no_uidvalidity = synced_folder(1, 500, 85);
        no_uidvalidity.uidvalidity = None;
        assert!(!Store::folder_is_unchanged(
            &no_uidvalidity,
            300,
            &server_state(300, 501, 1, 85)
        ));
    }

    #[test]
    fn a_server_that_will_not_say_is_never_skipped() {
        let stored = synced_folder(1, 500, 85);
        // STATUS without UIDNEXT or UIDVALIDITY tells us nothing usable.
        let mut vague = server_state(300, 501, 1, 85);
        vague.uid_next = None;
        assert!(!Store::folder_is_unchanged(&stored, 300, &vague));

        let mut vaguer = server_state(300, 501, 1, 85);
        vaguer.uid_validity = None;
        assert!(!Store::folder_is_unchanged(&stored, 300, &vaguer));
    }

    /// Comparing UID lists must answer both questions, not just one.
    #[test]
    fn reconciliation_finds_mail_that_is_missing_locally() {
        // The half that did not exist: the server has 4 and 5, we do not.
        assert_eq!(
            Store::compute_uid_additions(&[1, 2, 3], &[1, 2, 3, 4, 5]),
            vec![4, 5]
        );

        // Both directions at once — 2 was expunged, 9 never arrived.
        let stored = [1, 2, 3];
        let server = [1, 3, 9];
        assert_eq!(Store::compute_uid_additions(&stored, &server), vec![9]);
        assert_eq!(
            Store::compute_uid_deletions(&stored, &server, Some(3), "Archive"),
            vec![2]
        );

        // Agreement means no work.
        assert!(Store::compute_uid_additions(&[1, 2, 3], &[1, 2, 3]).is_empty());

        // Oldest first, so a capped repair makes progress from the far end
        // rather than re-fetching the same recent mail each pass.
        let missing = Store::compute_uid_additions(&[], &[70, 12, 45, 3]);
        assert_eq!(missing, vec![3, 12, 45, 70]);
    }

    /// A server whose MODSEQ never advances must not be able to hide
    /// deletions behind an unchanged counter.
    #[test]
    fn a_message_count_that_disagrees_defeats_the_unchanged_modseq_shortcut() {
        // Everything the shortcut asks for is satisfied — QRESYNC, a baseline,
        // an unchanged modseq, no changed messages — and it would have been
        // taken before the count was cross-checked.
        assert!(
            !Store::can_skip_deletion_detection_on_unchanged_modseq(
                true,
                Some(85),
                85,
                true,
                58_951,       // what we hold
                Some(62_756)  // what the server says it has
            ),
            "EXISTS disagreeing with the stored count means the mailbox moved, \
             whatever the modseq claims"
        );

        // The same in the other direction: messages disappeared.
        assert!(!Store::can_skip_deletion_detection_on_unchanged_modseq(
            true,
            Some(85),
            85,
            true,
            100,
            Some(90)
        ));

        // Agreement still allows the shortcut, so the common case stays cheap.
        assert!(Store::can_skip_deletion_detection_on_unchanged_modseq(
            true,
            Some(85),
            85,
            true,
            58_951,
            Some(58_951)
        ));

        // No EXISTS to compare with: fall back to the old behaviour rather
        // than forcing a full UID comparison on every sync.
        assert!(Store::can_skip_deletion_detection_on_unchanged_modseq(
            true,
            Some(85),
            85,
            true,
            58_951,
            None
        ));
    }

    #[test]
    fn test_plan_deletion_detection_table() {
        // (has_baseline_modseq, uidvalidity_changed, modseq_unchanged,
        //  skip_requested) -> (expected_plan, expected should_run, skip_reason)
        //
        // These lock in the EXACT branching (and priority order) that used to
        // live inline in sync_folder_impl.
        let cases: &[(bool, bool, bool, bool, DeletionDetectionPlan, bool, &str)] = &[
            // Normal add/delete/update diff: baseline present, nothing else set.
            (
                true,
                false,
                false,
                false,
                DeletionDetectionPlan::Run,
                true,
                "none",
            ),
            // First sync always wins, even if other flags are set.
            (
                false,
                false,
                false,
                false,
                DeletionDetectionPlan::SkipFirstSync,
                false,
                "first_sync",
            ),
            (
                false,
                true,
                true,
                true,
                DeletionDetectionPlan::SkipFirstSync,
                false,
                "first_sync",
            ),
            // UIDVALIDITY change beats modseq-unchanged and skip-requested.
            (
                true,
                true,
                true,
                true,
                DeletionDetectionPlan::SkipUidValidityChanged,
                false,
                "uidvalidity_changed",
            ),
            // Modseq unchanged (QRESYNC): mailbox identical, skip.
            (
                true,
                false,
                true,
                false,
                DeletionDetectionPlan::SkipModseqUnchanged,
                false,
                "modseq_unchanged",
            ),
            // Modseq-unchanged beats an also-set skip_requested (priority order).
            (
                true,
                false,
                true,
                true,
                DeletionDetectionPlan::SkipModseqUnchanged,
                false,
                "modseq_unchanged",
            ),
            // Caller-requested skip (e.g. MOVE destination) is lowest priority.
            (
                true,
                false,
                false,
                true,
                DeletionDetectionPlan::SkipRequested,
                false,
                "skip_requested",
            ),
        ];

        for &(has_baseline, uidv_changed, modseq_unchanged, skip_req, expected, run, reason) in
            cases
        {
            let plan = Store::plan_deletion_detection(
                has_baseline,
                uidv_changed,
                modseq_unchanged,
                skip_req,
            );
            assert_eq!(
                plan, expected,
                "plan for (baseline={has_baseline}, uidv={uidv_changed}, \
                 modseq_unchanged={modseq_unchanged}, skip={skip_req})"
            );
            assert_eq!(plan.should_run(), run, "should_run for {plan:?}");
            assert_eq!(plan.skip_reason(), reason, "skip_reason for {plan:?}");
        }
    }

    #[test]
    fn test_plan_deletion_detection_run_matches_original_condition() {
        // Exhaustively verify that `should_run()` equals the original inline
        // boolean: has_baseline && !uidv_changed && !modseq_unchanged && !skip.
        for bits in 0u8..16 {
            let has_baseline = bits & 0b0001 != 0;
            let uidv_changed = bits & 0b0010 != 0;
            let modseq_unchanged = bits & 0b0100 != 0;
            let skip_req = bits & 0b1000 != 0;

            let expected_run = has_baseline && !uidv_changed && !modseq_unchanged && !skip_req;
            let plan = Store::plan_deletion_detection(
                has_baseline,
                uidv_changed,
                modseq_unchanged,
                skip_req,
            );
            assert_eq!(
                plan.should_run(),
                expected_run,
                "run decision diverged from original condition for bits={bits:04b}"
            );
        }
    }

    #[test]
    fn test_vanished_result_is_complete_accepts_consistent_counts() {
        // Nothing expunged and the counts already agree — VANISHED is right.
        assert!(Store::vanished_result_is_complete(4, 0, 0, Some(4)));

        // The reported deletions exactly explain the difference.
        assert!(Store::vanished_result_is_complete(17, 13, 0, Some(4)));

        // A healthy sync that picked up new mail: 4 stored, 6 just fetched,
        // server has 10. Counting the fetch is what keeps this off the
        // expensive fallback.
        assert!(Store::vanished_result_is_complete(4, 0, 6, Some(10)));
    }

    /// Holding fewer messages than the server is the signal that mail is
    /// missing, and discarding it is what let a folder lag forever.
    #[test]
    fn a_shortfall_against_exists_forces_a_full_comparison() {
        // Private/Archive as it stood: 59,034 stored, nothing expunged,
        // nothing fetched, server holding 62,756. This was previously waved
        // through as "not fetched yet" and never revisited.
        assert!(!Store::vanished_result_is_complete(
            59_034,
            0,
            0,
            Some(62_756)
        ));

        // Still short after accounting for what this sync fetched.
        assert!(!Store::vanished_result_is_complete(4, 0, 2, Some(10)));

        // Exactly accounted for: no fallback needed.
        assert!(Store::vanished_result_is_complete(4, 0, 6, Some(10)));
    }

    #[test]
    fn test_vanished_result_is_complete_rejects_orphaned_rows() {
        // The real-world regression: 17 cached rows, the server holds 4, and
        // VANISHED reports nothing because those messages were expunged at or
        // below the stored baseline modseq. Holding more rows than the server
        // has messages proves the answer is incomplete.
        assert!(!Store::vanished_result_is_complete(17, 0, 0, Some(4)));

        // Still short by one after applying the reported deletions.
        assert!(!Store::vanished_result_is_complete(17, 12, 0, Some(4)));

        // Every message gone from an empty mailbox.
        assert!(!Store::vanished_result_is_complete(5, 0, 0, Some(0)));
    }

    #[test]
    fn test_vanished_result_is_complete_trusts_unknown_exists() {
        // Without EXISTS there is nothing to cross-check against; keep the
        // previous behaviour rather than forcing a UID fetch every sync.
        assert!(Store::vanished_result_is_complete(17, 0, 0, None));
    }

    #[test]
    fn test_uidvalidity_mismatch() {
        // Both known and differing -> genuine mismatch.
        assert!(Store::uidvalidity_mismatch(Some(1), Some(2)));
        // Both known and equal -> no mismatch.
        assert!(!Store::uidvalidity_mismatch(Some(7), Some(7)));
        // Unknown on either side -> no mismatch (matches the let-chain guard,
        // which required BOTH the stored value and the server value to be present).
        assert!(!Store::uidvalidity_mismatch(None, Some(2)));
        assert!(!Store::uidvalidity_mismatch(Some(1), None));
        assert!(!Store::uidvalidity_mismatch(None, None));
    }

    #[test]
    fn test_compute_highest_uid() {
        // highest_uid = uid_next - 1, since UIDNEXT is the next-to-be-assigned UID.
        assert_eq!(Store::compute_highest_uid(Some(101)), 100);
        assert_eq!(Store::compute_highest_uid(Some(1)), 0);
        // Saturating: an absent or zero UIDNEXT must not underflow.
        assert_eq!(Store::compute_highest_uid(Some(0)), 0);
        assert_eq!(Store::compute_highest_uid(None), 0);
    }

    #[test]
    fn test_concurrent_per_folder_sync_version_detection() {
        use std::sync::Barrier;

        let _db_path = ":memory:";
        let db = Arc::new(Mutex::new(create_test_db()));
        let barrier = Arc::new(Barrier::new(2));

        // Create a test folder
        let folder_id = {
            let mut db = db.lock().expect("test db lock poisoned");
            db.get_or_create_folder("test_account", "INBOX")
                .expect("failed to create folder")
        };

        // Clone for thread
        let db_clone = Arc::clone(&db);
        let barrier_clone = Arc::clone(&barrier);

        // Thread 1: Read baseline folder sync_version, wait, then try to commit
        let handle1 = thread::spawn(move || {
            let mut db = db_clone.lock().expect("test db lock poisoned");

            // Transaction 1: Read baseline folder sync_version
            let baseline: i64 = db
                .transaction(|tx| {
                    let version: i64 = tx.query_row(
                        "SELECT sync_version FROM folders WHERE id = ?1",
                        params![folder_id],
                        |row| row.get(0),
                    )?;
                    Ok(version)
                })
                .expect("failed to read baseline");

            // Release lock so Thread 2 can modify
            drop(db);

            // Wait for Thread 2 to modify folder's sync_version
            barrier_clone.wait();

            // Re-acquire lock
            let mut db = db_clone.lock().expect("test db lock poisoned");

            // Transaction 2: Try to commit with stale baseline

            db.transaction(|tx| {
                let current: i64 = tx.query_row(
                    "SELECT sync_version FROM folders WHERE id = ?1",
                    params![folder_id],
                    |row| row.get(0),
                )?;

                if current != baseline {
                    return Err(Error::ConcurrencyConflict {
                        operation: "sync_folder(INBOX)".to_string(),
                        reason: format!(
                            "Folder modified by another Orb instance during sync (version {} -> {})",
                            baseline, current
                        ),
                    });
                }

                Ok(())
            })
        });

        // Main thread: Wait for Thread 1 to read baseline, then modify folder's sync_version
        {
            // Give Thread 1 time to read baseline and release lock
            thread::sleep(std::time::Duration::from_millis(10));

            let mut db = db.lock().expect("test db lock poisoned");
            db.transaction(|tx| {
                tx.execute(
                    "UPDATE folders SET sync_version = sync_version + 1 WHERE id = ?1",
                    params![folder_id],
                )?;
                Ok(())
            })
            .expect("failed to increment folder sync_version");

            // Release lock before signaling barrier
            drop(db);
        }

        // Signal Thread 1 that modification is done
        barrier.wait();

        // Thread 1 should detect the conflict
        let result = handle1.join().expect("thread 1 panicked");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::ConcurrencyConflict { .. }
        ));
    }

    #[test]
    fn test_concurrency_conflict_is_retryable() {
        let err = Error::ConcurrencyConflict {
            operation: "sync_folder".to_string(),
            reason: "sync_version changed".to_string(),
        };

        assert!(err.is_retryable());
    }

    #[test]
    fn test_atomic_uidvalidity_change_in_transaction() {
        let mut db = create_test_db();

        // Create a folder
        let folder_id = db
            .get_or_create_folder("test_account", "INBOX")
            .expect("failed to create folder");

        // Insert some test messages
        db.transaction(|tx| {
            for uid in 1..=5 {
                tx.execute(
                    "INSERT INTO messages (folder_id, uid, message_id, flags, date, size)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        folder_id,
                        uid,
                        format!("msg-{}", uid),
                        "[]",
                        chrono::Utc::now().to_rfc3339(),
                        1000
                    ],
                )?;
            }
            Ok(())
        })
        .expect("failed to insert test messages");

        // Verify messages exist
        let count_before: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
                params![folder_id],
                |row: &rusqlite::Row| row.get(0),
            )
            .expect("failed to count messages");
        assert_eq!(count_before, 5);

        // Simulate UIDVALIDITY change handling in a single transaction
        db.transaction(|tx| {
            // Read baseline folder sync_version
            let _baseline: i64 = tx.query_row(
                "SELECT sync_version FROM folders WHERE id = ?1",
                params![folder_id],
                |row| row.get(0),
            )?;

            // Detect UIDVALIDITY change and clear messages atomically
            tx.execute("DELETE FROM messages WHERE folder_id = ?1", params![folder_id])?;

            // Update folder state and increment sync_version
            tx.execute(
                "UPDATE folders SET uidvalidity = ?1, highest_modseq = NULL, sync_version = sync_version + 1 WHERE id = ?2",
                params![999999, folder_id],
            )?;

            Ok(())
        })
        .expect("failed to handle UIDVALIDITY change");

        // Verify all messages were cleared
        let count_after: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
                params![folder_id],
                |row: &rusqlite::Row| row.get(0),
            )
            .expect("failed to count messages");
        assert_eq!(count_after, 0);

        // Verify UIDVALIDITY was updated
        let new_uidvalidity: Option<u32> = db
            .conn
            .query_row(
                "SELECT uidvalidity FROM folders WHERE id = ?1",
                params![folder_id],
                |row: &rusqlite::Row| row.get(0),
            )
            .expect("failed to read UIDVALIDITY");
        assert_eq!(new_uidvalidity, Some(999999));
    }

    #[test]
    fn test_concurrent_folder_creation() {
        let _db_path = ":memory:";
        let db = Arc::new(Mutex::new(create_test_db()));

        let handles: Vec<_> = (0..5)
            .map(|_i| {
                let db_clone = Arc::clone(&db);
                thread::spawn(move || {
                    let mut db = db_clone.lock().expect("test db lock poisoned");
                    db.get_or_create_folder("test_account", "INBOX")
                })
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("thread panicked"))
            .collect();

        // All threads should succeed
        for result in &results {
            assert!(result.is_ok());
        }

        // All threads should get the same folder_id
        let folder_ids: Vec<i64> = results.into_iter().map(|r| r.unwrap()).collect();
        assert!(folder_ids.windows(2).all(|w| w[0] == w[1]));

        // Verify only one folder was created
        let db = db.lock().expect("test db lock poisoned");
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM folders WHERE account_name = 'test_account' AND name = 'INBOX'",
                [],
                |row: &rusqlite::Row| row.get(0),
            )
            .expect("failed to count folders");
        assert_eq!(count, 1);
    }

    #[test]
    fn test_atomic_uid_reading_with_baseline() {
        let mut db = create_test_db();

        // Create a folder
        let folder_id = db
            .get_or_create_folder("test_account", "INBOX")
            .expect("failed to create folder");

        // Insert test messages
        db.transaction(|tx| {
            for uid in 1..=3 {
                tx.execute(
                    "INSERT INTO messages (folder_id, uid, message_id, flags, date, size)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        folder_id,
                        uid,
                        format!("msg-{}", uid),
                        "[]",
                        chrono::Utc::now().to_rfc3339(),
                        1000
                    ],
                )?;
            }
            Ok(())
        })
        .expect("failed to insert test messages");

        // Transaction 1: Read baseline state atomically
        let (_baseline_version, stored_uids) = db
            .transaction(|tx| {
                let version: i64 = tx.query_row(
                    "SELECT sync_version FROM folders WHERE id = ?1",
                    params![folder_id],
                    |row| row.get(0),
                )?;

                let uids = Database::get_folder_uids_in_tx(tx, folder_id)?;

                Ok((version, uids))
            })
            .expect("failed to read baseline");

        assert_eq!(stored_uids, vec![1, 2, 3]);

        // This verifies that UIDs are read atomically with folder's sync_version
        // In real concurrent scenarios, this prevents TOCTOU races
    }

    #[test]
    fn test_different_folders_sync_independently() {
        let mut db = create_test_db();

        // Create two folders
        let inbox_id = db
            .get_or_create_folder("test", "INBOX")
            .expect("failed to create INBOX");
        let sent_id = db
            .get_or_create_folder("test", "Sent")
            .expect("failed to create Sent");

        // Read baseline for INBOX
        let inbox_baseline: i64 = db
            .conn
            .query_row(
                "SELECT sync_version FROM folders WHERE id = ?1",
                params![inbox_id],
                |row: &rusqlite::Row| row.get(0),
            )
            .expect("failed to read INBOX baseline");

        // Modify Sent folder (simulating another instance syncing Sent)
        db.transaction(|tx| {
            tx.execute(
                "UPDATE folders SET sync_version = sync_version + 1 WHERE id = ?1",
                params![sent_id],
            )?;
            Ok(())
        })
        .expect("failed to modify Sent folder");

        // INBOX sync should succeed (no conflict) because it uses per-folder versioning
        let result = db.transaction(|tx| {
            let current: i64 = tx.query_row(
                "SELECT sync_version FROM folders WHERE id = ?1",
                params![inbox_id],
                |row| row.get(0),
            )?;

            if current != inbox_baseline {
                return Err(Error::ConcurrencyConflict {
                    operation: "sync_folder(INBOX)".to_string(),
                    reason: "INBOX version changed".to_string(),
                });
            }
            Ok(())
        });

        assert!(
            result.is_ok(),
            "INBOX sync should not conflict when Sent is modified by another instance"
        );

        // Verify Sent was actually modified but INBOX was not
        let inbox_version: i64 = db
            .conn
            .query_row(
                "SELECT sync_version FROM folders WHERE id = ?1",
                params![inbox_id],
                |row: &rusqlite::Row| row.get(0),
            )
            .expect("failed to read INBOX version");
        let sent_version: i64 = db
            .conn
            .query_row(
                "SELECT sync_version FROM folders WHERE id = ?1",
                params![sent_id],
                |row: &rusqlite::Row| row.get(0),
            )
            .expect("failed to read Sent version");

        assert_eq!(
            inbox_version, inbox_baseline,
            "INBOX version should be unchanged"
        );
        assert_eq!(sent_version, 1, "Sent version should be incremented");
    }

    /// A sync must stay callable through a shared reference.
    ///
    /// Never executed — it exists to fail compilation if `sync` goes back to
    /// `&mut self`. That would force the FFI to serialize the whole store
    /// again, so every read would queue behind whatever sync is running, which
    /// on a cold 198k-message folder meant a five-minute freeze. Concurrency is
    /// already handled below this level: writes take the write lock, and a sync
    /// takes a per-folder lock in the database.
    #[allow(dead_code)]
    async fn sync_is_callable_on_a_shared_store(store: &Store) {
        let _ = store.sync(None, None, None, false).await;
    }

    #[test]
    fn store_can_be_shared_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Store>();
    }

    #[test]
    fn test_sync_lock_prevents_concurrent_sync() {
        let mut db = create_test_db();

        // First instance acquires lock
        let got_lock1 = db
            .try_acquire_sync_lock("test_account", "INBOX")
            .expect("failed to try lock");
        assert!(got_lock1, "first instance should get the lock");

        // Second instance tries to acquire same lock (should fail)
        let got_lock2 = db
            .try_acquire_sync_lock("test_account", "INBOX")
            .expect("failed to try lock");
        assert!(!got_lock2, "second instance should not get the lock");

        // Release the lock
        db.release_sync_lock("test_account", "INBOX")
            .expect("failed to release lock");

        // Now the lock can be acquired again
        let got_lock3 = db
            .try_acquire_sync_lock("test_account", "INBOX")
            .expect("failed to try lock");
        assert!(got_lock3, "should get lock after release");

        db.release_sync_lock("test_account", "INBOX").ok();
    }

    #[test]
    fn test_sync_lock_different_folders() {
        let mut db = create_test_db();

        // Acquire locks for different folders
        let got_inbox = db
            .try_acquire_sync_lock("test_account", "INBOX")
            .expect("failed to lock INBOX");
        let got_sent = db
            .try_acquire_sync_lock("test_account", "Sent")
            .expect("failed to lock Sent");

        assert!(got_inbox, "should get INBOX lock");
        assert!(got_sent, "should get Sent lock - different folder");

        db.release_sync_lock("test_account", "INBOX").ok();
        db.release_sync_lock("test_account", "Sent").ok();
    }

    #[test]
    fn test_sync_lock_different_accounts() {
        let mut db = create_test_db();

        // Acquire locks for same folder name but different accounts
        let got_acc1 = db
            .try_acquire_sync_lock("account1", "INBOX")
            .expect("failed to lock account1 INBOX");
        let got_acc2 = db
            .try_acquire_sync_lock("account2", "INBOX")
            .expect("failed to lock account2 INBOX");

        assert!(got_acc1, "should get account1 INBOX lock");
        assert!(
            got_acc2,
            "should get account2 INBOX lock - different account"
        );

        db.release_sync_lock("account1", "INBOX").ok();
        db.release_sync_lock("account2", "INBOX").ok();
    }

    #[test]
    fn test_sync_lock_cleanup_stale() {
        let mut db = create_test_db();

        // Manually insert a stale lock (older than 300s + 60s buffer = 360s)
        db.conn
            .execute(
                "INSERT INTO sync_locks (account_name, folder_name, locked_at, lock_holder)
                 VALUES (?1, ?2, datetime('now', '-361 seconds'), ?3)",
                params!["test_account", "INBOX", "stale:12345"],
            )
            .expect("failed to insert stale lock");

        // Try to acquire lock - should succeed after cleaning up stale lock
        let got_lock = db
            .try_acquire_sync_lock("test_account", "INBOX")
            .expect("failed to acquire lock");
        assert!(
            got_lock,
            "should get lock after cleaning up stale lock (older than timeout + buffer)"
        );

        db.release_sync_lock("test_account", "INBOX").ok();
    }

    #[test]
    fn test_sync_lock_release_only_own_lock() {
        let mut db = create_test_db();

        // First instance acquires lock
        let got_lock = db
            .try_acquire_sync_lock("test_account", "INBOX")
            .expect("failed to acquire lock");
        assert!(got_lock, "should get the lock");

        // Manually insert a lock from a different holder
        db.conn
            .execute(
                "DELETE FROM sync_locks WHERE account_name = ?1 AND folder_name = ?2",
                params!["test_account", "INBOX"],
            )
            .ok();
        db.conn
            .execute(
                "INSERT INTO sync_locks (account_name, folder_name, locked_at, lock_holder)
                 VALUES (?1, ?2, datetime('now'), ?3)",
                params!["test_account", "INBOX", "other:99999"],
            )
            .expect("failed to insert other holder's lock");

        // Try to release - should not release other holder's lock
        db.release_sync_lock("test_account", "INBOX")
            .expect("release should not fail");

        // Lock should still exist
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sync_locks WHERE account_name = ?1 AND folder_name = ?2",
                params!["test_account", "INBOX"],
                |row| row.get(0),
            )
            .expect("failed to count locks");
        assert_eq!(count, 1, "other holder's lock should still exist");

        // Clean up
        db.conn
            .execute(
                "DELETE FROM sync_locks WHERE account_name = ?1 AND folder_name = ?2",
                params!["test_account", "INBOX"],
            )
            .ok();
    }

    #[test]
    fn test_sync_lock_case_insensitive_folder_names() {
        let mut db = create_test_db();

        // Acquire lock with uppercase folder name
        let got_upper = db
            .try_acquire_sync_lock("test_account", "INBOX")
            .expect("failed to lock INBOX");
        assert!(got_upper, "should get lock for INBOX");

        // Try to acquire with lowercase - should fail (same folder)
        let got_lower = db
            .try_acquire_sync_lock("test_account", "inbox")
            .expect("failed to try lock inbox");
        assert!(
            !got_lower,
            "should not get lock for 'inbox' when 'INBOX' is locked (case-insensitive)"
        );

        // Release using mixed case - should work
        db.release_sync_lock("test_account", "Inbox")
            .expect("should release with different case");

        // Now lowercase should succeed
        let got_lower2 = db
            .try_acquire_sync_lock("test_account", "inbox")
            .expect("failed to lock inbox");
        assert!(got_lower2, "should get lock after release");

        db.release_sync_lock("test_account", "INBOX").ok();
    }

    #[test]
    fn test_sync_lock_renewal() {
        let mut db = create_test_db();

        // Acquire lock
        let got_lock = db
            .try_acquire_sync_lock("test_account", "INBOX")
            .expect("failed to acquire lock");
        assert!(got_lock, "should get the lock");

        // Get initial locked_at timestamp
        let initial_time: String = db
            .conn
            .query_row(
                "SELECT locked_at FROM sync_locks WHERE account_name = ?1 AND folder_name = ?2",
                params!["test_account", "INBOX"],
                |row| row.get(0),
            )
            .expect("failed to get lock time");

        // Sleep to ensure timestamp changes (SQLite datetime has 1-second precision)
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Renew the lock
        let renewed = db
            .renew_sync_lock("test_account", "INBOX")
            .expect("failed to renew lock");
        assert!(renewed, "should successfully renew our lock");

        // Get renewed timestamp
        let renewed_time: String = db
            .conn
            .query_row(
                "SELECT locked_at FROM sync_locks WHERE account_name = ?1 AND folder_name = ?2",
                params!["test_account", "INBOX"],
                |row| row.get(0),
            )
            .expect("failed to get renewed time");

        assert_ne!(
            initial_time, renewed_time,
            "lock timestamp should be updated after renewal"
        );

        db.release_sync_lock("test_account", "INBOX").ok();
    }

    #[test]
    fn test_sync_lock_renewal_not_holder() {
        let mut db = create_test_db();

        // Try to renew a lock we don't hold
        let renewed = db
            .renew_sync_lock("test_account", "INBOX")
            .expect("renew should not error");
        assert!(!renewed, "should return false when we don't hold the lock");
    }

    #[test]
    fn test_sync_lock_concurrent_connections() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        // Create a temporary database file (not in-memory, to allow multiple connections)
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("orb_test_{}.db", std::process::id()));
        let db_path_str = db_path.to_str().unwrap();

        // Initialize the database
        {
            let db = Database::new(db_path_str).expect("failed to create db");
            db.init_schema().expect("failed to init schema");
        }

        let barrier = Arc::new(Barrier::new(2));
        let db_path_clone1 = db_path_str.to_string();
        let db_path_clone2 = db_path_str.to_string();
        let barrier_clone = Arc::clone(&barrier);

        // Thread 1: Acquire lock
        let handle1 = thread::spawn(move || {
            let mut db1 = Database::new(&db_path_clone1).expect("failed to open db in thread 1");

            // Acquire lock
            let got_lock = db1
                .try_acquire_sync_lock("test_account", "INBOX")
                .expect("failed to acquire lock in thread 1");
            assert!(got_lock, "thread 1 should get the lock");

            // Wait for thread 2 to try acquiring
            barrier_clone.wait();

            // Sleep to let thread 2 verify it can't get the lock
            thread::sleep(std::time::Duration::from_millis(100));

            // Release lock
            db1.release_sync_lock("test_account", "INBOX")
                .expect("failed to release lock in thread 1");
        });

        // Thread 2: Try to acquire same lock (should fail)
        let handle2 = thread::spawn(move || {
            let mut db2 = Database::new(&db_path_clone2).expect("failed to open db in thread 2");

            // Wait for thread 1 to acquire lock
            barrier.wait();

            // Try to acquire lock - should fail
            let got_lock = db2
                .try_acquire_sync_lock("test_account", "INBOX")
                .expect("failed to try lock in thread 2");
            assert!(
                !got_lock,
                "thread 2 should not get lock while thread 1 holds it"
            );

            // Wait for thread 1 to release
            thread::sleep(std::time::Duration::from_millis(200));

            // Now should succeed
            let got_lock2 = db2
                .try_acquire_sync_lock("test_account", "INBOX")
                .expect("failed to acquire lock in thread 2 after release");
            assert!(
                got_lock2,
                "thread 2 should get lock after thread 1 releases"
            );

            db2.release_sync_lock("test_account", "INBOX").ok();
        });

        handle1.join().expect("thread 1 panicked");
        handle2.join().expect("thread 2 panicked");

        // Clean up
        std::fs::remove_file(&db_path).ok();
    }

    /// A store on a temporary database with one account, `A`, whose
    /// special folders are the defaults.
    fn store_with_account() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    \
             imap_port: 993\n    imap_username: u\n    from: a@x\n    junk_folder: Spam\n"
        ))
        .expect("config");
        (dir, Store::new(config).expect("store"))
    }

    /// What a sync announces is the unread mail it brought to a watched
    /// folder: not a message a rule has filed away since, not one it marked
    /// read, and nothing from a folder that is not watched.
    #[test]
    fn new_mail_is_the_unread_mail_a_sync_brought_to_a_watched_folder() {
        let (_dir, store) = store_with_account();
        let mut db = store.writer().expect("db");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("INBOX");
        let archive = db.get_or_create_folder("A", "Archive").expect("Archive");
        let unread = db
            .insert_message(&crate::models::Message::new(inbox, 1))
            .expect("insert");
        let mut seen = crate::models::Message::new(inbox, 2);
        seen.flags = vec!["\\Seen".to_string()];
        let read = db.insert_message(&seen).expect("insert");
        let archived = db
            .insert_message(&crate::models::Message::new(archive, 3))
            .expect("insert");
        let filed_away = archived + 100;
        let account = store.get_account_config("A").expect("account");

        let mail = store
            .new_mail_among(&account, "INBOX", &[unread, read, filed_away])
            .expect("new mail");
        assert_eq!(mail.iter().map(|m| m.id).collect::<Vec<_>>(), vec![unread]);
        assert_eq!(mail[0].folder, "INBOX");
        assert!(
            store
                .new_mail_among(&account, "Archive", &[archived])
                .expect("new mail")
                .is_empty(),
            "Archive is not watched"
        );
    }

    #[derive(Default)]
    struct OutboxEar {
        heard: std::sync::Mutex<Vec<Vec<i64>>>,
    }

    impl OutboxListener for OutboxEar {
        fn on_outbox_changed(&self, failed: Vec<FailedSend>) {
            let ids = failed.iter().map(|f| f.id).collect();
            self.heard.lock().unwrap().push(ids);
        }
    }

    fn queue_send(store: &Store, from: Option<&str>) -> i64 {
        let email = crate::smtp::OutgoingEmail {
            to: vec!["b@x".into()],
            subject: "Hello".into(),
            body: "Text".into(),
            from: from.map(String::from),
            attachments: vec![crate::smtp::EmailAttachment {
                filename: "a.pdf".into(),
                content_type: "application/pdf".into(),
                bytes: vec![1, 2, 3],
            }],
            ..Default::default()
        };
        store
            .writer()
            .unwrap()
            .enqueue_outbox("A", OUTBOX_SEND, &serde_json::to_string(&email).unwrap())
            .unwrap()
    }

    /// A refused message stays in the Outbox, with its reason, and no drain
    /// picks it up again; the flag writes queued beside it are not shown.
    #[test]
    fn a_failed_send_stays_in_the_outbox_and_out_of_the_drain() {
        let (_dir, store) = store_with_account();
        let id = queue_send(&store, None);
        store
            .writer()
            .unwrap()
            .enqueue_outbox("A", OUTBOX_FLAGS, "{}")
            .unwrap();
        assert_eq!(store.outbox_len().unwrap(), 2);

        let listed = store.outbox().unwrap();
        assert_eq!(listed.len(), 1);
        let queued = &listed[0];
        assert_eq!(queued.id, id);
        assert_eq!(queued.sending_as, "A");
        assert_eq!(queued.from, "a@x");
        assert_eq!(queued.to, vec!["b@x".to_string()]);
        assert_eq!(queued.subject, "Hello");
        assert_eq!(queued.attachments, vec!["a.pdf".to_string()]);
        assert!(queued.failed_at.is_none());

        store
            .writer()
            .unwrap()
            .outbox_mark_failed(id, "535 Incorrect authentication data")
            .unwrap();
        assert_eq!(store.outbox_len().unwrap(), 1, "the drain skips it");
        assert!(
            store
                .reader()
                .unwrap()
                .outbox_queued("A")
                .unwrap()
                .iter()
                .all(|item| item.id != id)
        );
        let queued = &store.outbox().unwrap()[0];
        assert!(queued.failed_at.is_some());
        assert_eq!(queued.attempts, 1);
        assert_eq!(
            queued.error.as_deref(),
            Some("535 Incorrect authentication data")
        );
    }

    /// Retry puts a failed message back in line even when the drain that
    /// follows cannot reach the server; whoever listens hears of it.
    #[tokio::test]
    async fn retrying_puts_a_failed_message_back_in_line() {
        let (_dir, store) = store_with_account();
        let ear = Arc::new(OutboxEar::default());
        store.set_outbox_listener(ear.clone());
        let id = queue_send(&store, None);
        store
            .writer()
            .unwrap()
            .outbox_mark_failed(id, "refused")
            .unwrap();

        store.retry_outbox(id).await.expect("retry");
        let queued = &store.outbox().unwrap()[0];
        assert!(queued.failed_at.is_none());
        assert_eq!(queued.error.as_deref(), Some("refused"));
        assert_eq!(store.outbox_len().unwrap(), 1);
        assert!(!ear.heard.lock().unwrap().is_empty());

        assert!(store.retry_outbox(id + 100).await.is_err());
    }

    /// Delete takes the message and nothing else queued.
    #[tokio::test]
    async fn deleting_from_the_outbox_takes_only_the_message() {
        let (_dir, store) = store_with_account();
        let ear = Arc::new(OutboxEar::default());
        store.set_outbox_listener(ear.clone());
        let flags = store
            .writer()
            .unwrap()
            .enqueue_outbox("A", OUTBOX_FLAGS, "{}")
            .unwrap();
        let id = queue_send(&store, None);

        store.discard_outbox(flags).await.unwrap();
        assert_eq!(store.outbox_len().unwrap(), 2, "a flag write is not mail");
        assert!(ear.heard.lock().unwrap().is_empty());

        store.discard_outbox(id).await.unwrap();
        assert!(store.outbox().unwrap().is_empty());
        assert_eq!(store.outbox_len().unwrap(), 1);
        assert_eq!(ear.heard.lock().unwrap().len(), 1);
    }

    /// A special folder the server lists is there, synced or not: a trash
    /// folder set `sync: false` was reported missing at every start. One the
    /// server does not list is still reported.
    #[test]
    fn a_special_folder_is_checked_against_the_servers_list_not_the_synced_one() {
        let (_dir, store) = store_with_account();
        let listed = |names: &[&str]| {
            names
                .iter()
                .map(|name| imap::ListedFolder {
                    name: name.to_string(),
                    special_use: None,
                    delimiter: Some("/".into()),
                })
                .collect::<Vec<_>>()
        };
        {
            let mut db = store.writer().unwrap();
            db.get_or_create_folder("A", "INBOX").unwrap();
            db.replace_server_folders("A", &listed(&["INBOX", "Drafts", "Sent", "Trash", "Spam"]))
                .unwrap();
        }
        assert_eq!(
            store.special_folder_warnings().unwrap(),
            Vec::<String>::new()
        );

        store
            .writer()
            .unwrap()
            .replace_server_folders("A", &listed(&["INBOX", "Drafts", "Sent", "Spam"]))
            .unwrap();
        store.forget_listed_folders("A");
        assert_eq!(
            store.special_folder_warnings().unwrap(),
            vec!["A: trash_folder = 'Trash' does not exist on the server".to_string()]
        );
    }

    /// Marking read is what the list shows at once, so it must not wait for
    /// the server: the account here has no server to reach and no password,
    /// and the change is still made and queued.
    #[tokio::test]
    async fn a_flag_change_is_queued_without_asking_the_server() {
        let (_dir, store) = store_with_account();
        store
            .queue_read_status("A", "INBOX", &[1, 2], true)
            .await
            .expect("queued without a server");
        store
            .queue_flag_status("A", "INBOX", &[3], true)
            .await
            .expect("queued without a server");
        assert_eq!(store.outbox_len().unwrap(), 2);
        assert!(
            store
                .queue_read_status("Nobody", "INBOX", &[1], true)
                .await
                .is_err()
        );
    }

    /// A sent reply marks what it answers, once, on every stored copy; and
    /// the time of the user's reply is read from Sent, not from a reply
    /// somebody else sent to the same message.
    #[tokio::test]
    async fn a_reply_marks_its_original_answered_and_dates_the_answer() {
        let (_dir, store) = store_with_account();
        {
            let mut db = store.writer().unwrap();
            let inbox = db.get_or_create_folder("A", "INBOX").unwrap();
            let sent = db.get_or_create_folder("A", "Sent").unwrap();
            let message =
                |folder_id: i64, uid: u32, id: &str, reply_to: Option<&str>, date: &str| {
                    crate::Message {
                        id: None,
                        folder_id,
                        uid,
                        message_id: Some(id.to_string()),
                        subject: Some("Question".to_string()),
                        from: Some("Bob <bob@example.com>".to_string()),
                        to: None,
                        cc: None,
                        date: Some(date.to_string()),
                        body_text: None,
                        body_html: None,
                        rfc822: None,
                        size: None,
                        flags: Vec::new(),
                        modseq: None,
                        shape: None,
                        headers: Some(crate::threads::ThreadHeaders {
                            in_reply_to: reply_to.map(str::to_string),
                            references: None,
                            list_id: None,
                            extra: Vec::new(),
                        }),
                    }
                };
            db.insert_message(&message(
                inbox,
                1,
                "<q@example.com>",
                None,
                "2026-08-01T09:00:00Z",
            ))
            .unwrap();
            db.insert_message(&message(
                sent,
                5,
                "<a@example.com>",
                Some("<q@example.com>"),
                "2026-08-02T12:45:00Z",
            ))
            .unwrap();
            // Newer, but somebody else's answer, in the inbox.
            db.insert_message(&message(
                inbox,
                2,
                "<b@example.com>",
                Some("<q@example.com>"),
                "2026-08-03T08:00:00Z",
            ))
            .unwrap();
        }

        assert_eq!(
            store.last_reply_to("q@example.com").unwrap().as_deref(),
            Some("2026-08-02T12:45:00Z")
        );
        assert_eq!(store.last_reply_to("<b@example.com>").unwrap(), None);

        assert_eq!(
            store
                .mark_original("q@example.com", "\\Answered")
                .await
                .unwrap(),
            ["A"]
        );
        assert_eq!(store.outbox_len().unwrap(), 1);
        let copies = store.find_messages_by_rfc_id("<q@example.com>").unwrap();
        assert!(copies[0].message.flags.iter().any(|f| f == "\\Answered"));
        // Answered already: nothing more to write.
        assert!(
            store
                .mark_original("q@example.com", "\\Answered")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.outbox_len().unwrap(), 1);
        // A forward of it is a second, separate mark.
        assert_eq!(
            store
                .mark_original("q@example.com", "$Forwarded")
                .await
                .unwrap(),
            ["A"]
        );
        assert_eq!(store.outbox_len().unwrap(), 2);
        let copies = store.find_messages_by_rfc_id("<q@example.com>").unwrap();
        assert!(copies[0].message.flags.iter().any(|f| f == "$Forwarded"));
    }

    fn listed(name: &str, role: Option<&str>) -> imap::ListedFolder {
        imap::ListedFolder {
            name: name.to_string(),
            special_use: role.map(str::to_string),
            delimiter: Some("/".to_string()),
        }
    }

    /// Before the first LIST the names stand as configured; afterwards a
    /// default the server has no folder for gives way to the folder the
    /// server marks for the role, while a configured name the server has
    /// keeps its say.
    #[tokio::test]
    async fn special_folders_follow_the_roles_the_server_announces() {
        let (_dir, store) = store_with_account();
        let before = store.get_account_config("A").unwrap();
        assert_eq!(before.trash_folder, "Trash");
        assert_eq!(before.junk_folder.as_deref(), Some("Spam"));

        store
            .record_listing(
                "A",
                &[
                    listed("INBOX", None),
                    listed("[Gmail]/Trash", Some("\\Trash")),
                    listed("[Gmail]/Sent Mail", Some("\\Sent")),
                    listed("[Gmail]/All Mail", Some("\\Archive")),
                    listed("Spam", None),
                    listed("[Gmail]/Spam", Some("\\Junk")),
                    listed("Drafts", None),
                ],
            )
            .await
            .expect("record");
        let after = store.get_account_config("A").unwrap();
        assert_eq!(after.trash_folder, "[Gmail]/Trash");
        assert_eq!(after.sent_folder, "[Gmail]/Sent Mail");
        assert_eq!(after.drafts_folder, "Drafts", "the server has the default");
        assert_eq!(after.archive_folder.as_deref(), Some("[Gmail]/All Mail"));
        assert_eq!(
            after.junk_folder.as_deref(),
            Some("Spam"),
            "a configured name the server has wins over the role"
        );
    }

    /// The sidebar gets each folder's role, by the server's flag or the
    /// config, and its place in the tree, with the name below its parent.
    #[tokio::test]
    async fn the_sidebar_learns_each_folders_role_and_place() {
        let (_dir, store) = store_with_account();
        {
            let mut db = store.writer().unwrap();
            for name in [
                "INBOX",
                "Archive",
                "Archive/2025",
                "Bin",
                "Drafts",
                "Projects",
                "Projects/Orb",
            ] {
                db.get_or_create_folder("A", name).unwrap();
            }
        }
        store
            .record_listing(
                "A",
                &[
                    listed("INBOX", None),
                    listed("Archive", None),
                    listed("Archive/2025", None),
                    listed("Bin", Some("\\Trash")),
                    listed("Drafts", None),
                    listed("Projects", None),
                    listed("Projects/Orb", None),
                ],
            )
            .await
            .expect("record");
        let folders = store.get_visible_folders("A").unwrap();
        type Shape<'a> = (&'a str, Option<&'a str>, u32, Option<&'a str>, &'a str);
        let shape: Vec<Shape> = folders
            .iter()
            .map(|f| {
                (
                    f.name(),
                    f.role.as_deref(),
                    f.depth,
                    f.parent.as_deref(),
                    f.label.as_str(),
                )
            })
            .collect();
        assert!(shape.contains(&("INBOX", Some("inbox"), 0, None, "INBOX")));
        assert!(shape.contains(&("Bin", Some("trash"), 0, None, "Bin")));
        assert!(shape.contains(&("Drafts", Some("drafts"), 0, None, "Drafts")));
        assert!(shape.contains(&("Projects/Orb", None, 1, Some("Projects"), "Orb")));
        let archive = shape.iter().position(|f| f.0 == "Archive").unwrap();
        assert_eq!(
            shape[archive + 1],
            ("Archive/2025", None, 1, Some("Archive"), "2025")
        );
        assert_eq!(folders[archive + 1].display_name, "Archive/2025");
        assert!(matches!(folders[archive].sort, MessageSort::DateNewest));
    }

    /// The reading folders are listed with their role, found under the
    /// name the server gives them, rather than as the account's folders.
    #[tokio::test]
    async fn the_reading_folders_are_listed_with_their_role() {
        let (_dir, store) = store_with_account();
        let mut config = (*store.config()).clone();
        config.reading = Some(crate::config::ReadingConfig {
            account: "A".into(),
            folder: "Reading".into(),
            archive_folder: "Reading Archive".into(),
            feeds: Vec::new(),
            poll_minutes: 60,
        });
        store.replace_config(Arc::new(config));
        assert_eq!(store.reading_folder(false), None, "not listed yet");
        {
            let mut db = store.writer().unwrap();
            for name in ["INBOX", "INBOX.Reading", "INBOX.Reading Archive"] {
                db.get_or_create_folder("A", name).unwrap();
            }
        }
        store
            .record_listing(
                "A",
                &[
                    listed("INBOX", None),
                    listed("INBOX.Reading", None),
                    listed("INBOX.Reading Archive", None),
                ],
            )
            .await
            .expect("record");
        let roles: Vec<(String, Option<String>)> = store
            .get_visible_folders("A")
            .unwrap()
            .iter()
            .map(|f| (f.name().to_string(), f.role.clone()))
            .collect();
        assert!(roles.contains(&("INBOX.Reading".into(), Some("reading".into()))));
        assert!(roles.contains(&(
            "INBOX.Reading Archive".into(),
            Some("reading-archive".into())
        )));
        assert_eq!(
            store.reading_folder(true),
            Some(("A".into(), "INBOX.Reading Archive".into()))
        );
    }

    /// A folder the server no longer lists is forgotten here, with its
    /// messages; the ones it still lists stay.
    #[tokio::test]
    async fn a_folder_gone_from_the_server_is_forgotten() {
        let (_dir, store) = store_with_account();
        {
            let mut db = store.writer().unwrap();
            let inbox = db.get_or_create_folder("A", "INBOX").unwrap();
            let old = db.get_or_create_folder("A", "Old").unwrap();
            db.insert_message(&crate::models::Message::new(inbox, 1))
                .unwrap();
            db.insert_message(&crate::models::Message::new(old, 1))
                .unwrap();
        }
        store
            .record_listing("A", &[listed("INBOX", None), listed("New", None)])
            .await
            .expect("record");
        let names: Vec<String> = store
            .reader()
            .unwrap()
            .get_folders_by_account("A")
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(
            names,
            vec!["INBOX"],
            "New is not created until it is synced"
        );
        let count: i64 = store
            .reader()
            .unwrap()
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// A renamed folder keeps its messages and its staged actions.
    #[test]
    fn a_renamed_folder_takes_its_messages_and_staged_actions_along() {
        let (_dir, store) = store_with_account();
        let mut db = store.writer().unwrap();
        let old = db.get_or_create_folder("A", "Old").unwrap();
        db.insert_message(&crate::models::Message::new(old, 7))
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO pending_actions (account, folder, uid, action_type, created_at)
                 VALUES ('A', 'Old', 7, 'delete', 'now')",
                [],
            )
            .unwrap();
        db.rename_folder("A", "Old", "New").unwrap();
        let folder = db.get_folder_by_name("A", "New").unwrap().expect("renamed");
        assert_eq!(folder.id, Some(old));
        assert!(db.get_folder_by_name("A", "Old").unwrap().is_none());
        let staged: String = db
            .conn
            .query_row(
                "SELECT folder FROM pending_actions WHERE uid = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(staged, "New");
    }

    #[tokio::test]
    async fn test_graceful_shutdown_releases_all_locks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test_shutdown.db");

        let config = Config {
            db_path: db_path.to_str().unwrap().to_string(),
            accounts: vec![],
            bindings: Default::default(),
            log_file: None,
            theme: Default::default(),
            sidebar_mode: Default::default(),
            virtual_folders: Default::default(),
            pinned: None,
            pins: Vec::new(),
            subject_munges: Default::default(),
            message_links: Default::default(),
            load_remote_images_from: Vec::new(),
            contacts: Default::default(),
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
        };

        let store = Store::new(config).unwrap();

        // Acquire some locks
        {
            let mut db = store.writer().unwrap();
            assert!(db.try_acquire_sync_lock("account1", "INBOX").unwrap());
            assert!(db.try_acquire_sync_lock("account1", "Sent").unwrap());
            assert!(db.try_acquire_sync_lock("account2", "INBOX").unwrap());
        }

        // Verify locks are held
        {
            let mut db = store.writer().unwrap();
            assert!(
                !db.try_acquire_sync_lock("account1", "INBOX").unwrap(),
                "Lock should already be held"
            );
        }

        // Call shutdown
        let released_count = store.shutdown().await.unwrap();
        assert_eq!(released_count, 3, "Should release all 3 locks");

        // Verify locks are released
        {
            let mut db = store.writer().unwrap();
            assert!(
                db.try_acquire_sync_lock("account1", "INBOX").unwrap(),
                "Lock should be available after shutdown"
            );
            assert!(
                db.try_acquire_sync_lock("account1", "Sent").unwrap(),
                "Lock should be available after shutdown"
            );
            assert!(
                db.try_acquire_sync_lock("account2", "INBOX").unwrap(),
                "Lock should be available after shutdown"
            );
        }
    }

    /// Regression test for the write-lock scoping fix.
    ///
    /// The sync loop used to hold the global write lock across the whole
    /// per-folder sync, including network I/O, so any user write action
    /// (delete/move/mark-read) was blocked for the entire duration of a sync.
    ///
    /// After the fix, sync only holds the write lock for the short DB writes
    /// (via `Store::locked_write`) and releases it during the network phase.
    /// This test drives the same lock discipline the sync loop now uses and
    /// asserts that a concurrent user write can acquire the lock while the sync
    /// is mid-fetch. It also asserts the negative: while the lock is genuinely
    /// held, a write does block (so the positive assertion is meaningful).
    #[tokio::test]
    async fn write_lock_released_during_sync_network_phase() {
        use std::time::Duration;
        use tokio::sync::oneshot;

        let write_lock = Arc::new(tokio::sync::Mutex::new(()));

        // Negative control: while the lock is actually held, a write must block.
        {
            let _held = write_lock.lock().await;
            let blocked = tokio::time::timeout(
                Duration::from_millis(200),
                Store::locked_write(&write_lock, || Ok::<_, Error>(())),
            )
            .await;
            assert!(
                blocked.is_err(),
                "a write must block while the write lock is held"
            );
        }

        let (net_started_tx, net_started_rx) = oneshot::channel::<()>();
        let (resume_tx, resume_rx) = oneshot::channel::<()>();
        let (sync_done_tx, sync_done_rx) = oneshot::channel::<()>();

        // Simulated sync task following the same lock discipline as
        // `sync_folder_impl`: guarded DB write (tx1) -> lock-free network I/O
        // -> guarded DB write (tx2).
        let sync_lock = write_lock.clone();
        let sync_task = tokio::spawn(async move {
            // Transaction 1 (guarded, lock released immediately after).
            Store::locked_write(&sync_lock, || Ok::<_, Error>(()))
                .await
                .unwrap();

            // Entering the network phase: the write lock is now free.
            net_started_tx.send(()).unwrap();

            // Simulate a slow network fetch. We only proceed once the test has
            // performed its user write, proving the write did not have to wait
            // for the sync to complete.
            resume_rx.await.unwrap();

            // Transaction 2 (guarded).
            Store::locked_write(&sync_lock, || Ok::<_, Error>(()))
                .await
                .unwrap();
            sync_done_tx.send(()).unwrap();
        });

        // Wait until the sync task is in its network phase.
        net_started_rx.await.unwrap();

        // A user write action must be able to acquire the write lock right now,
        // while the sync is still mid-fetch. Bound it with a timeout so a
        // regression (lock held across the network phase) fails fast instead of
        // hanging the test suite.
        let user_write = Store::locked_write(&write_lock, || Ok::<_, Error>(42));
        let result = tokio::time::timeout(Duration::from_secs(5), user_write)
            .await
            .expect("user write was blocked while sync was in its network phase")
            .expect("user write failed");
        assert_eq!(result, 42);

        // Let the sync finish and make sure everything joins cleanly.
        resume_tx.send(()).unwrap();
        sync_done_rx.await.unwrap();
        sync_task.await.unwrap();
    }

    // ========================================================================
    // Staged (pending) actions — Store API
    // ========================================================================

    fn pending_test_store(db_path: &str) -> Store {
        let config = Config {
            db_path: db_path.to_string(),
            accounts: vec![],
            bindings: Default::default(),
            log_file: None,
            theme: Default::default(),
            sidebar_mode: Default::default(),
            virtual_folders: Default::default(),
            pinned: None,
            pins: Vec::new(),
            subject_munges: Default::default(),
            message_links: Default::default(),
            load_remote_images_from: Vec::new(),
            contacts: Default::default(),
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
        };
        Store::new(config).unwrap()
    }

    #[tokio::test]
    async fn test_store_stage_list_discard_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("pending.db");
        let store = pending_test_store(db_path.to_str().unwrap());

        // Stage a batch of deletes and a move.
        store
            .stage_action("acct", "INBOX", &[1, 2], PENDING_ACTION_DELETE, None)
            .await
            .unwrap();
        store
            .stage_action(
                "acct",
                "INBOX",
                &[3],
                PENDING_ACTION_MOVE,
                Some("Archive".to_string()),
            )
            .await
            .unwrap();

        let all = store.list_pending(&PendingScope::All).unwrap();
        assert_eq!(all.len(), 3);

        let counts = store
            .pending_counts(&PendingScope::Account {
                account_name: "acct".to_string(),
            })
            .unwrap();
        assert_eq!(counts.iter().map(|c| c.count).sum::<u32>(), 3);

        // Unstage one, then discard the rest.
        store.unstage_action("acct", "INBOX", &[2]).await.unwrap();
        assert_eq!(store.list_pending(&PendingScope::All).unwrap().len(), 2);

        let discarded = store.discard_pending(&PendingScope::All).await.unwrap();
        assert_eq!(discarded, 2);
        assert!(store.list_pending(&PendingScope::All).unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_store_stage_move_requires_target() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("pending.db");
        let store = pending_test_store(db_path.to_str().unwrap());

        // A move with no target is rejected and stages nothing.
        assert!(matches!(
            store
                .stage_action("acct", "INBOX", &[1], PENDING_ACTION_MOVE, None)
                .await,
            Err(Error::InvalidInput(_))
        ));
        assert!(store.list_pending(&PendingScope::All).unwrap().is_empty());

        // An unknown action type is rejected too.
        assert!(matches!(
            store
                .stage_action("acct", "INBOX", &[1], "frobnicate", None)
                .await,
            Err(Error::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn test_store_execute_pending_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("pending.db");
        let store = pending_test_store(db_path.to_str().unwrap());

        // Nothing staged: execute does no network I/O and reports zero.
        let execution = store.execute_pending(&PendingScope::All).await.unwrap();
        assert_eq!(execution, PendingExecution::default());
    }

    /// `x` must not wait for the server: the staged mail leaves the stored
    /// copy at once, its writes wait in the outbox, and a sync leaves the
    /// queued UIDs out until they have been carried out.
    #[tokio::test]
    async fn executing_staged_actions_queues_them_and_holds_them_back_from_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("pending.db");
        let store = pending_test_store(db_path.to_str().unwrap());
        let inbox = {
            let mut db = store.writer().unwrap();
            db.init_schema().unwrap();
            let inbox = db.get_or_create_folder("acct", "INBOX").unwrap();
            for uid in 1..=4 {
                db.insert_message(&crate::models::Message {
                    id: None,
                    folder_id: inbox,
                    uid,
                    message_id: Some(format!("<m{uid}@example.com>")),
                    subject: Some(format!("Message {uid}")),
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
            }
            inbox
        };
        store
            .stage_action(
                "acct",
                "INBOX",
                &[1, 2],
                PENDING_ACTION_MOVE,
                Some("Archive".into()),
            )
            .await
            .unwrap();
        store
            .stage_action("acct", "INBOX", &[3], PENDING_ACTION_DELETE, None)
            .await
            .unwrap();

        let execution = store.execute_pending(&PendingScope::All).await.unwrap();
        assert_eq!(execution.executed, 3);
        assert!(store.list_pending(&PendingScope::All).unwrap().is_empty());
        let mut db = store.writer().unwrap();
        assert_eq!(db.get_message_count(inbox).unwrap(), 1);
        assert_eq!(db.outbox_len(Some("acct")).unwrap(), 2);

        let held_back = db
            .transaction(|tx| Store::queued_move_uids(tx, "acct", "INBOX"))
            .unwrap();
        assert_eq!(held_back, HashSet::from([1, 2, 3]));
        let elsewhere = db
            .transaction(|tx| Store::queued_move_uids(tx, "acct", "Archive"))
            .unwrap();
        assert!(elsewhere.is_empty());
    }

    /// The safety-critical execute decision (trash-vs-permanent-vs-move) is pure
    /// and must be exact, so it is testable without a live IMAP server.
    #[test]
    fn test_resolve_pending_execution_op() {
        // delete in a normal folder -> recoverable move to Trash.
        assert_eq!(
            resolve_pending_execution_op(PENDING_ACTION_DELETE, "INBOX", "Trash", None).unwrap(),
            PendingExecutionOp::MoveToTrash {
                trash: "Trash".to_string()
            }
        );
        // delete already in Trash -> permanent delete.
        assert_eq!(
            resolve_pending_execution_op(PENDING_ACTION_DELETE, "Trash", "Trash", None).unwrap(),
            PendingExecutionOp::PermanentDelete
        );
        // move -> to its target folder (whitespace trimmed).
        assert_eq!(
            resolve_pending_execution_op(PENDING_ACTION_MOVE, "INBOX", "Trash", Some("Archive"))
                .unwrap(),
            PendingExecutionOp::Move {
                target: "Archive".to_string()
            }
        );
        // move without a (non-empty) target -> error, never a silent delete.
        assert!(resolve_pending_execution_op(PENDING_ACTION_MOVE, "INBOX", "Trash", None).is_err());
        assert!(
            resolve_pending_execution_op(PENDING_ACTION_MOVE, "INBOX", "Trash", Some("  "))
                .is_err()
        );
        // unknown action type -> error.
        assert!(resolve_pending_execution_op("frobnicate", "INBOX", "Trash", None).is_err());
    }
}

#[cfg(test)]
mod link_lookup_tests {
    use super::*;
    use crate::models::Message;

    /// A database with two accounts, three folders and a handful of messages —
    /// enough for every lookup the task-link paths make.
    fn seeded_db() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let mut db = Database::new(&path).expect("db");
        db.init_schema().expect("schema");

        let inbox = db.get_or_create_folder("Work", "INBOX").expect("folder");
        let archive = db.get_or_create_folder("Work", "Archive").expect("folder");
        let private = db.get_or_create_folder("Private", "INBOX").expect("folder");

        let message = |folder_id: i64, uid: u32, rfc: Option<&str>, subject: &str| {
            db.insert_message(&Message {
                id: None,
                folder_id,
                uid,
                message_id: rfc.map(str::to_string),
                subject: Some(subject.to_string()),
                from: Some("Alice <alice@example.com>".to_string()),
                to: None,
                cc: None,
                date: Some("2026-08-01T09:00:00Z".to_string()),
                body_text: Some(format!("body of {subject}")),
                body_html: None,
                rfc822: None,
                size: None,
                flags: Vec::new(),
                modseq: None,
                shape: None,
                headers: None,
            })
            .expect("insert")
        };

        // The same Message-ID filed twice under one account, and once under
        // another: the shape that makes preferring the cached folder matter.
        message(inbox, 1, Some("<budget@example.com>"), "Budget");
        message(archive, 2, Some("<budget@example.com>"), "Budget");
        message(private, 3, Some("<budget@example.com>"), "Budget");
        // Stored without brackets, as an import or a non-conforming server does.
        message(inbox, 4, Some("bare@example.com"), "Bare");
        // No Message-ID at all: unlinkable, and must never match anything.
        message(inbox, 5, None, "Anonymous");

        (dir, path)
    }

    #[test]
    fn message_id_variants_offers_both_spellings() {
        assert_eq!(
            message_id_variants("<a@b>"),
            ["<a@b>".to_string(), "a@b".to_string()]
        );
        assert_eq!(
            message_id_variants("a@b"),
            ["<a@b>".to_string(), "a@b".to_string()]
        );
        // Surrounding whitespace is header noise, not part of the identity.
        assert_eq!(
            message_id_variants("  <a@b>  "),
            ["<a@b>".to_string(), "a@b".to_string()]
        );
    }

    #[test]
    fn message_id_variants_refuses_to_produce_an_empty_search_term() {
        // An empty term would match every message whose header was absent, so
        // both variants have to come back empty and be skipped by the caller.
        assert_eq!(message_id_variants(""), [String::new(), String::new()]);
        assert_eq!(message_id_variants("<>"), [String::new(), String::new()]);
        assert_eq!(message_id_variants("   "), [String::new(), String::new()]);
    }

    #[test]
    fn a_link_finds_every_local_copy_of_its_message() {
        let (_dir, path) = seeded_db();
        let db = Database::new(&path).expect("db");

        let found = find_messages_by_rfc_id(&db.conn, &message_id_variants("<budget@example.com>"))
            .unwrap();
        assert_eq!(found.len(), 3);
        let places: HashSet<(String, String)> = found
            .iter()
            .map(|item| (item.account_name.clone(), item.folder_name.clone()))
            .collect();
        assert!(places.contains(&("Work".to_string(), "INBOX".to_string())));
        assert!(places.contains(&("Work".to_string(), "Archive".to_string())));
        assert!(places.contains(&("Private".to_string(), "INBOX".to_string())));
    }

    #[test]
    fn a_link_matches_a_message_id_stored_without_its_brackets() {
        let (_dir, path) = seeded_db();
        let db = Database::new(&path).expect("db");

        // Asked for with brackets, stored without: the same message either way.
        let found =
            find_messages_by_rfc_id(&db.conn, &message_id_variants("<bare@example.com>")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].message.subject.as_deref(), Some("Bare"));

        let bare =
            find_messages_by_rfc_id(&db.conn, &message_id_variants("bare@example.com")).unwrap();
        assert_eq!(bare.len(), 1);
    }

    #[test]
    fn an_unknown_message_id_finds_nothing_rather_than_everything() {
        let (_dir, path) = seeded_db();
        let db = Database::new(&path).expect("db");

        let found =
            find_messages_by_rfc_id(&db.conn, &message_id_variants("<never@example.com>")).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn the_cached_account_and_folder_decide_which_copy_opens() {
        let (_dir, path) = seeded_db();
        let db = Database::new(&path).expect("db");
        let candidates =
            find_messages_by_rfc_id(&db.conn, &message_id_variants("<budget@example.com>"))
                .unwrap();

        let archived =
            prefer_located_message(candidates.clone(), Some("Work"), Some("Archive")).unwrap();
        assert_eq!(archived.folder_name, "Archive");
        assert_eq!(archived.account_name, "Work");

        // The folder moved but the account did not: stay in the account.
        let moved = prefer_located_message(candidates.clone(), Some("Work"), Some("Gone")).unwrap();
        assert_eq!(moved.account_name, "Work");

        // Nothing remembered: any copy is better than refusing to open one.
        assert!(prefer_located_message(candidates, None, None).is_some());
    }

    #[test]
    fn preferring_a_copy_of_nothing_is_nothing() {
        assert!(prefer_located_message(Vec::new(), Some("Work"), Some("INBOX")).is_none());
    }

    #[test]
    fn the_task_column_reads_every_visible_row_in_one_query() {
        let (_dir, path) = seeded_db();
        let db = Database::new(&path).expect("db");
        let all: Vec<i64> = db
            .conn
            .prepare("SELECT id FROM messages ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();

        let ids = rfc_message_ids(&db.conn, &all).unwrap();
        // Four of the five messages have a Message-ID; the fifth cannot be
        // linked and so is simply absent rather than mapped to an empty string.
        assert_eq!(ids.len(), 4);
        assert!(ids.values().all(|id| !id.is_empty()));
    }

    #[test]
    fn link_sources_keep_the_selection_order_and_carry_the_cache() {
        let (_dir, path) = seeded_db();
        let db = Database::new(&path).expect("db");
        let all: Vec<i64> = db
            .conn
            .prepare("SELECT id FROM messages ORDER BY id DESC")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();

        let sources = message_link_sources(&db.conn, &all).unwrap();
        assert_eq!(
            sources.iter().map(|s| s.local_id).collect::<Vec<_>>(),
            all,
            "a selection creates one task per message, in the selection's order"
        );

        let anonymous = sources
            .iter()
            .find(|s| s.subject.as_deref() == Some("Anonymous"))
            .expect("the message with no Message-ID is still described");
        assert!(anonymous.rfc_message_id.is_none());

        let budget = sources
            .iter()
            .find(|s| s.local_id == all[all.len() - 1])
            .expect("first message");
        assert_eq!(budget.account_name, "Work");
        assert_eq!(budget.folder_name, "INBOX");
        assert_eq!(budget.from.as_deref(), Some("Alice <alice@example.com>"));
        assert_eq!(budget.date.as_deref(), Some("2026-08-01T09:00:00Z"));
    }

    #[test]
    fn forgetting_the_tasks_folder_takes_its_mail_out_of_the_search_index() {
        let (_dir, path) = seeded_db();
        let db = Database::new(&path).expect("db");

        let indexed = |db: &Database, term: &str| -> i64 {
            db.conn
                .query_row(
                    "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                    params![term],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(indexed(&db, "Budget"), 3);

        let removed = forget_folder(&db.conn, "Work", "Archive").unwrap();
        assert_eq!(removed, 1);

        // The row, the folder and — the reason the delete is explicit rather
        // than left to the cascade — the full-text entry all go.
        assert_eq!(indexed(&db, "Budget"), 2);
        assert!(
            db.get_folder_by_name("Work", "Archive").unwrap().is_none(),
            "the folder row goes with its messages"
        );
        assert!(
            db.get_folder_by_name("Work", "INBOX").unwrap().is_some(),
            "the other folders are untouched"
        );
    }

    #[test]
    fn forgetting_a_folder_that_was_never_synced_does_nothing() {
        let (_dir, path) = seeded_db();
        let db = Database::new(&path).expect("db");
        assert_eq!(forget_folder(&db.conn, "Work", "Tasks").unwrap(), 0);
    }

    #[test]
    fn the_search_starts_where_the_message_was_last_seen() {
        let listed = vec![
            "INBOX".to_string(),
            "Archive".to_string(),
            "Tasks".to_string(),
        ];
        let order = search_folder_order(Some("Archive"), &listed, |f| f == "Tasks");

        assert_eq!(order, vec!["Archive".to_string(), "INBOX".to_string()]);
    }

    #[test]
    fn the_search_covers_folders_this_machine_has_never_synced() {
        let listed = vec![
            "INBOX".to_string(),
            "Projects/2026".to_string(),
            "Archive".to_string(),
        ];
        let order = search_folder_order(None, &listed, |_| false);

        // INBOX first even though the listing is arbitrary, then everything
        // else exactly once and in the server's own order.
        assert_eq!(
            order,
            vec![
                "INBOX".to_string(),
                "Projects/2026".to_string(),
                "Archive".to_string()
            ]
        );
    }

    #[test]
    fn the_search_never_opens_the_tasks_folder() {
        let listed = vec!["INBOX".to_string(), "INBOX.Tasks".to_string()];
        let order = search_folder_order(Some("INBOX.Tasks"), &listed, |f| f == "INBOX.Tasks");

        assert_eq!(
            order,
            vec!["INBOX".to_string()],
            "a task message must never reach the mail reader, not even as the \
             preferred folder"
        );
    }

    #[test]
    fn a_folder_named_twice_is_searched_once() {
        // The preferred folder is normally also in the listing; searching it
        // twice would double the cost of the common case.
        let listed = vec!["INBOX".to_string(), "Archive".to_string()];
        let order = search_folder_order(Some("archive"), &listed, |_| false);
        assert_eq!(order.len(), 2);
        assert_eq!(order[0], "archive");
    }
}
