//! Reconciliation between the tasks folder and the local cache.
//!
//! The folder is the source of truth; the SQLite tables are a cache that must
//! be fully rebuildable from it. IMAP messages are immutable, so every edit is
//! an APPEND followed by a delete, and getting *that* right is the whole of the
//! reliability requirement: a task scheduled a year out has to appear on that
//! day, on every machine, and no edit may ever be lost to a concurrent edit, to
//! an upload racing a local edit, or to a crash mid-sync.
//!
//! Four decisions carry that weight.
//!
//! **Identity is `X-Orb-Task-Id`, never the RFC `Message-ID`.** A task is
//! rewritten on every edit, so its `Message-ID` identifies an edit, not a task.
//! Reconciliation therefore groups the folder by task id, and when one id
//! appears in several messages — two devices edited, or an append succeeded and
//! the delete did not — the winner is the highest revision, ties broken by the
//! highest UID. The losers are expunged only *after* the winner is confirmed
//! present on the server. There is deliberately no "server wins" rule: a server
//! copy with a lower revision than the local task never overwrites it, the local
//! task is re-uploaded instead. The one exemption is the log, which is merged as
//! a union of entries keyed by entry uuid ([`super::model::merge_logs`]), so a
//! device whose *task* lost still keeps its history.
//!
//! **Nothing depends on MODSEQ.** One hoster reports the same `HIGHESTMODSEQ`
//! forever (documented at `crate::imap`), so a CONDSTORE-delta design would
//! freeze silently on exactly one of the two accounts this is built for, while
//! reporting success. Every sync instead lists the whole folder — UID plus the
//! identity headers, a few dozen bytes a message — and reconciles against it.
//! The folder holds hundreds of tiny messages, so a full listing is one round
//! trip; a MODSEQ is recorded for diagnostics and never read back as the record
//! of what changed. Anything absent from the listing was deleted on the server,
//! which is also the only way a hard delete by another client can be detected.
//!
//! **A `UIDVALIDITY` bump means "your UIDs are meaningless", not "the tasks are
//! gone".** Cached UIDs are dropped and the folder is reconciled by task id;
//! nothing is deleted locally on that path, and a local task the server no
//! longer has is re-uploaded rather than removed.
//!
//! **Crash safety comes from the uuid being older than the append.** The task
//! id is minted when the task is created, long before it is uploaded, so a
//! retried append cannot mint a second task — it produces a duplicate *message*
//! for one task, which the revision rule resolves on the next sync. An append
//! whose response was lost, an append whose delete failed, and a crash between
//! the two all converge on one message and never on a gap.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};

use super::model::{
    Task, TaskAttachment, TaskLinkedEmail, TaskReminder, TaskSyncState, TaskTag, format_timestamp,
    is_wire_id,
};
use super::repository::{PurgedTask, RegistryState, TaskIndexRow, TaskRepository};
use super::rfc822::{
    IDENTITY_HEADERS, TaskEmailBuilder, TaskEmailParser, TaskIdentity, TaskMessage, TaskMessageKind,
};
use crate::db::Database;
use crate::imap::{HeaderListing, ImapClient};
use crate::{Result, logger};

/// Folder name used when the config does not name one.
///
/// No leading dot. `.Tasks` is a Courier/Maildir convention; on a Dovecot
/// server it creates a folder literally called `.Tasks` beside the real one, and
/// on Gmail it is refused outright.
pub const DEFAULT_TASKS_FOLDER: &str = TaskSyncState::DEFAULT_FOLDER;

/// Where attachment payloads are cached, before `~` is expanded.
const ATTACHMENT_CACHE_ROOT: &str = "~/.orb/attachments";

/// The header fields the reconciliation listing asks for.
///
/// The identity headers plus `Message-ID`. The `Message-ID` is not identity —
/// that is the entire point of `X-Orb-Task-Id` — but it is what a message is
/// found by when its UID has been invalidated, so the listing is also what
/// repairs the cached one.
fn listing_fields() -> Vec<&'static str> {
    let mut fields = IDENTITY_HEADERS.to_vec();
    fields.push("Message-ID");
    fields
}

// ---------------------------------------------------------------------------
// Resolving the folder against the server's namespace (§1)
// ---------------------------------------------------------------------------

/// What the configured folder name turns out to mean on this server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderPlan {
    /// The name to `SELECT`.
    pub name: String,
    /// Whether it has to be created first.
    pub create: bool,
}

/// Resolve a configured folder name against the server's namespace.
///
/// The name from the config is used verbatim wherever the server will accept
/// it. The one thing that has to be worked out is whether this server keeps
/// personal mailboxes under a prefix: on Courier and on Dovecot's Maildir++
/// layout every folder lives under `INBOX.`, so a bare `Tasks` would be created
/// as a second top-level mailbox that no other client shows next to the mail.
/// On Dovecot's default layout, and on Gmail, there is no prefix and prepending
/// one invents a folder nobody asked for.
///
/// The prefix is therefore *observed*, not assumed: it is only used when every
/// folder the server lists already lives under it. `NAMESPACE` would answer this
/// directly but cannot be issued safely — see
/// [`crate::imap::ImapClient::hierarchy_delimiter`] — and the delimiter plus the
/// folder list is enough.
///
/// A name that already contains the delimiter is treated as fully qualified and
/// left alone, so a config can always override the guess by writing the whole
/// path.
pub fn plan_tasks_folder(
    requested: &str,
    delimiter: Option<&str>,
    existing: &[String],
) -> FolderPlan {
    let requested = requested.trim();

    // Already there under exactly this name, or under this name in a different
    // case — mailbox names are case-sensitive except for INBOX, so the server's
    // spelling is the one to use.
    if let Some(found) = existing
        .iter()
        .find(|name| name.as_str() == requested || name.eq_ignore_ascii_case(requested))
    {
        return FolderPlan {
            name: found.clone(),
            create: false,
        };
    }

    let prefix = delimiter.and_then(|delimiter| namespace_prefix(delimiter, existing));

    // A qualified name means the config has already decided; anything else
    // would be second-guessing it.
    let qualified = match (prefix.as_deref(), delimiter) {
        (Some(prefix), Some(delimiter)) if !requested.contains(delimiter) => {
            format!("{prefix}{requested}")
        }
        _ => requested.to_string(),
    };

    match existing
        .iter()
        .find(|name| name.as_str() == qualified || name.eq_ignore_ascii_case(&qualified))
    {
        Some(found) => FolderPlan {
            name: found.clone(),
            create: false,
        },
        None => FolderPlan {
            name: qualified,
            create: true,
        },
    }
}

/// The prefix every personal mailbox on this server sits under, if there is one.
///
/// Derived from the folder list rather than guessed: a prefix is only reported
/// when *every* folder other than INBOX itself is inside it. One stray
/// top-level folder is enough to conclude that this server does not require the
/// prefix, which is the safe way round — creating `Tasks` beside the mail is
/// visible and fixable, while creating `INBOX.Tasks` on a server that does not
/// namespace hides it inside the inbox.
fn namespace_prefix(delimiter: &str, existing: &[String]) -> Option<String> {
    if delimiter.is_empty() {
        return None;
    }

    let mut prefix: Option<String> = None;
    for name in existing {
        if name.eq_ignore_ascii_case("INBOX") {
            continue;
        }
        let Some((root, _)) = name.split_once(delimiter) else {
            // A folder that sits outside every prefix: this server does not
            // require one, so neither does the tasks folder.
            return None;
        };
        let candidate = format!("{root}{delimiter}");
        match &prefix {
            // Folders under two different roots: an ordinary flat hierarchy
            // that happens to have sub-folders.
            Some(seen) if *seen != candidate => return None,
            Some(_) => {}
            None => prefix = Some(candidate),
        }
    }

    prefix
}

/// Make sure the tasks folder exists and can be selected, and report what it is
/// really called.
///
/// Failure here is a configuration problem and is reported as one: without a
/// folder there is no task storage at all, and the worst outcome would be a
/// client that quietly keeps every task on one machine. The mailbox is left
/// selected on success, which is what a sync does next anyway.
pub async fn ensure_tasks_folder(client: &mut ImapClient, requested: &str) -> Result<String> {
    let existing = client.list_folders().await.unwrap_or_default();
    let delimiter = client.hierarchy_delimiter().await.unwrap_or(None);
    let plan = plan_tasks_folder(requested, delimiter.as_deref(), &existing);
    let mut create_refused = None;

    if plan.create {
        logger::info(&format!(
            "Creating tasks folder '{}' (configured as '{requested}', delimiter {:?})",
            plan.name, delimiter
        ));
        // A CREATE that fails because the folder already exists is not a
        // failure: two clients may have raced, and the SELECT below is the
        // authority on whether the folder is usable.
        if let Err(error) = client.create_folder(&plan.name).await {
            logger::warn(&format!(
                "CREATE '{}' failed ({error}); trying to select it anyway",
                plan.name
            ));
            create_refused = Some(error.to_string());
        }
    }

    if let Err(error) = client.select_mailbox(&plan.name).await {
        let reason = unusable_folder_reason(
            &plan.name,
            requested,
            create_refused.as_deref(),
            &error.to_string(),
        );
        logger::warn(&reason);
        return Err(crate::Error::InvalidConfig {
            field: "tasks.folder".to_string(),
            reason,
        });
    }

    Ok(plan.name)
}

/// Why the tasks folder cannot be used, in words that say what to do about it.
///
/// One failure gets a sentence of its own. Zimbra and other groupware servers
/// keep a built-in `Tasks` folder that they do not offer over IMAP: LIST leaves
/// it out, so it is created, CREATE answers that it exists, and SELECT fails.
/// The name is taken, and only another one helps. The server says so in the
/// text of its answer, which is all there is to tell it by.
fn unusable_folder_reason(
    name: &str,
    requested: &str,
    create_error: Option<&str>,
    select_error: &str,
) -> String {
    if create_error.is_some_and(|error| error.to_ascii_lowercase().contains("exist")) {
        return format!(
            "the tasks folder '{name}' (configured as '{requested}') is taken by a folder this \
             server does not offer over IMAP, such as the built-in Tasks folder of Zimbra and \
             other groupware servers. Choose another name in tasks.folder; until then tasks are \
             kept on this machine only."
        );
    }
    format!(
        "the tasks folder '{name}' (configured as '{requested}') cannot be created or selected: \
         {select_error}. Tasks are kept on this machine only until this is fixed."
    )
}

// ---------------------------------------------------------------------------
// The listing (§3.3)
// ---------------------------------------------------------------------------

/// One message in the tasks folder, as the listing sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedMessage {
    pub uid: u32,
    pub identity: TaskIdentity,
    /// The RFC `Message-ID`, which is how this message is found again once its
    /// UID has been invalidated.
    pub message_id: Option<String>,
}

/// What a full listing of the folder came back with.
#[derive(Debug, Default)]
pub struct Listing {
    /// Messages this build can identify.
    pub messages: Vec<ListedMessage>,
    /// UIDs whose headers could not be read: a version this build does not
    /// understand, or something that is not an Orb task message at all. They
    /// are counted and then left completely alone — never downloaded, never
    /// deleted — because the one thing worse than not reading a message is
    /// destroying it.
    pub unreadable: Vec<u32>,
    /// UIDs the server already has on their way out.
    pub deleted: Vec<u32>,
}

/// Read a header-only fetch of the folder into identities.
pub fn read_listing(entries: &[HeaderListing]) -> Listing {
    let mut listing = Listing::default();

    for entry in entries {
        // A message flagged `\Deleted` is still listed until somebody expunges.
        // Counting it as live undoes deletions: the row is gone locally, the
        // message is still there, and the next sync downloads it back as a task
        // it has never seen.
        if entry.is_deleted() {
            listing.deleted.push(entry.uid);
            continue;
        }

        match TaskEmailParser::parse_identity(&entry.headers) {
            Ok(identity) => listing.messages.push(ListedMessage {
                uid: entry.uid,
                identity,
                message_id: read_message_id(&entry.headers),
            }),
            Err(_) => listing.unreadable.push(entry.uid),
        }
    }

    listing
}

/// The `Message-ID` out of a raw header block.
fn read_message_id(headers: &[u8]) -> Option<String> {
    use mailparse::MailHeaderMap;
    let (parsed, _) = mailparse::parse_headers(headers).ok()?;
    parsed
        .get_first_value("message-id")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Reconciliation (§3.1)
// ---------------------------------------------------------------------------

/// A message whose body has to be fetched and applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTarget {
    pub uid: u32,
    pub task_uuid: String,
    pub revision: i64,
}

/// Why a task is being uploaded. Carried for the log, and because the reasons
/// are the specification: each one is a way an edit could otherwise be lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadReason {
    /// Local changes the server has not seen.
    LocalEdit,
    /// The server's copy is at a lower revision than the local task; it must
    /// not overwrite it, so the local task goes up instead.
    LocalAhead,
    /// Created here and never uploaded — offline creation, or an upload that
    /// has not run yet.
    NeverUploaded,
    /// Locally edited and absent from the server: re-created rather than
    /// deleted, because the edit would otherwise vanish.
    MissingOnServer,
    /// Absent from the server after a `UIDVALIDITY` bump. Nothing is deleted on
    /// that path, so the local copy is restored to the server.
    UidValidityReset,
}

/// A task that has to go up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadTarget {
    pub task_id: i64,
    pub task_uuid: String,
    pub reason: UploadReason,
    /// Every message the server currently holds for this task id. They are
    /// expunged only once the new message has been confirmed present.
    pub supersedes: Vec<u32>,
    /// Another machine's copy among them, whose log has to be taken in
    /// before this upload replaces it (§11): the fields are this machine's,
    /// the history is both.
    pub merge_log_from: Option<u32>,
}

/// A task that is in sync but whose cached location has moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebind {
    pub task_id: i64,
    pub task_uuid: String,
    pub uid: u32,
    pub revision: i64,
    pub message_id: String,
}

/// A task the server no longer has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDeletion {
    pub task_id: i64,
    pub task_uuid: String,
}

/// Why a message is being removed from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpungeReason {
    /// A losing copy of a task id that exists more than once.
    Duplicate,
    /// An attachment payload whose task exists nowhere any more.
    Orphan,
}

/// A message to remove from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpungeTarget {
    pub uid: u32,
    pub reason: ExpungeReason,
}

/// An attachment payload message the server holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedAttachment {
    pub uid: u32,
    pub task_uuid: String,
    pub attachment_uuid: String,
    pub message_id: Option<String>,
}

/// What to do about the tag registry (§7), which syncs by the same rules as a
/// task under a well-known id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RegistryAction {
    /// Both sides agree, or neither side has one.
    #[default]
    Idle,
    Download(DownloadTarget),
    Upload {
        supersedes: Vec<u32>,
    },
    /// Both machines changed tags since the copy they share: take in the
    /// other machine's registry from `from`, merge it with this one tag by
    /// tag, and upload the result over everything in `supersedes`.
    Merge {
        from: DownloadTarget,
        supersedes: Vec<u32>,
    },
}

/// Everything a sync is going to do, decided before any of it is done.
///
/// Separating the decision from the doing is what makes the rules testable: the
/// duplicate resolution, the lower-revision-never-wins rule and the
/// `UIDVALIDITY` exemption are properties of this function, provable without a
/// server.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub download: Vec<DownloadTarget>,
    pub upload: Vec<UploadTarget>,
    pub rebind: Vec<Rebind>,
    pub delete_local: Vec<LocalDeletion>,
    pub expunge: Vec<ExpungeTarget>,
    pub attachments: Vec<ListedAttachment>,
    pub registry: RegistryAction,
    /// Attachment messages that carry no attachment id, and so cannot be
    /// matched to anything. Counted, never touched.
    pub unidentified: usize,
}

impl SyncPlan {
    pub fn is_empty(&self) -> bool {
        self.download.is_empty()
            && self.upload.is_empty()
            && self.rebind.is_empty()
            && self.delete_local.is_empty()
            && self.expunge.is_empty()
            && self.registry == RegistryAction::Idle
    }
}

/// Decide what to do with a folder listing and a local index.
///
/// `uidvalidity_changed` suppresses every local deletion: after a bump the UIDs
/// are meaningless but the tasks are not gone, and a folder that has been
/// restored from a backup must not take the local copies with it.
pub fn reconcile(
    local: &[TaskIndexRow],
    listing: &[ListedMessage],
    local_registry: impl Into<RegistryState>,
    uidvalidity_changed: bool,
) -> SyncPlan {
    let local_registry = local_registry.into();
    let mut plan = SyncPlan::default();

    // Group by what each message is, and by the id it claims.
    let mut server_tasks: HashMap<&str, Vec<&ListedMessage>> = HashMap::new();
    let mut registry_messages: Vec<&ListedMessage> = Vec::new();
    let mut server_attachments: HashMap<&str, Vec<&ListedMessage>> = HashMap::new();

    for message in listing {
        match message.identity.kind {
            TaskMessageKind::Task => server_tasks
                .entry(message.identity.task_uuid.as_str())
                .or_default()
                .push(message),
            TaskMessageKind::TagRegistry => registry_messages.push(message),
            // Planned apart, by [`plan_chat_marks`] and [`plan_today_cleared`].
            TaskMessageKind::ChatMarks | TaskMessageKind::TodayCleared => {}
            TaskMessageKind::Attachment => match message.identity.attachment_uuid.as_deref() {
                Some(attachment_uuid) => server_attachments
                    .entry(attachment_uuid)
                    .or_default()
                    .push(message),
                None => plan.unidentified += 1,
            },
        }
    }

    let known: HashMap<&str, &TaskIndexRow> = local
        .iter()
        .map(|row| (row.task_uuid.as_str(), row))
        .collect();

    let mut task_uuids: Vec<&str> = known
        .keys()
        .copied()
        .chain(server_tasks.keys().copied())
        .collect();
    task_uuids.sort_unstable();
    task_uuids.dedup();

    for task_uuid in task_uuids {
        let row = known.get(task_uuid).copied();
        let group = server_tasks.get(task_uuid);

        match (row, group) {
            // A task from another machine, or the first sync after the cache
            // was deleted.
            (None, Some(group)) => {
                let (winner, losers) = pick_winner(group);
                plan.download.push(target(winner));
                plan.expunge
                    .extend(expunge_all(&losers, ExpungeReason::Duplicate));
            }

            (Some(row), Some(group)) => {
                let (winner, losers) = pick_winner(group);
                if winner.identity.revision > row.revision {
                    // The server is ahead. A local edit at a lower revision
                    // loses the field values by last-writer-wins, but not its
                    // history: the log is union-merged when the download is
                    // applied.
                    plan.download.push(target(winner));
                    plan.expunge
                        .extend(expunge_all(&losers, ExpungeReason::Duplicate));
                } else if winner.identity.revision < row.revision || row.is_dirty {
                    // The local copy is ahead, or carries changes that never
                    // reached the server. Everything currently on the server
                    // for this id is superseded — including the winner, which
                    // is why no expunge is planned here: they go once the new
                    // message is confirmed present.
                    let reason = if winner.identity.revision < row.revision {
                        UploadReason::LocalAhead
                    } else {
                        UploadReason::LocalEdit
                    };
                    plan.upload.push(UploadTarget {
                        task_id: row.id,
                        task_uuid: task_uuid.to_string(),
                        reason,
                        supersedes: uids_of(group),
                        merge_log_from: another_edit(row.message_id.as_deref(), winner)
                            .then_some(winner.uid),
                    });
                } else if another_edit(row.message_id.as_deref(), winner) {
                    // The same revision, but not the copy this machine wrote
                    // or read: two machines edited the task from one revision,
                    // and the other one uploaded last. Its copy wins (highest
                    // UID, §3.1); without this both keep their own version,
                    // each believing it is in step, until the next edit.
                    plan.download.push(target(winner));
                    plan.expunge
                        .extend(expunge_all(&losers, ExpungeReason::Duplicate));
                } else {
                    // In step. The cached location may still be stale — after a
                    // UIDVALIDITY bump it certainly is — and repairing it here
                    // costs nothing, while leaving it wrong means the next
                    // upload cannot find the message it has to replace.
                    if let Some(rebind) = rebind_if_moved(row, winner) {
                        plan.rebind.push(rebind);
                    }
                    plan.expunge
                        .extend(expunge_all(&losers, ExpungeReason::Duplicate));
                }
            }

            (Some(row), None) => {
                if row.imap_uid.is_none() && row.message_id.is_none() {
                    // Never uploaded: created offline, or created while the
                    // last sync was already running.
                    plan.upload.push(UploadTarget {
                        task_id: row.id,
                        task_uuid: task_uuid.to_string(),
                        reason: UploadReason::NeverUploaded,
                        supersedes: Vec::new(),
                        merge_log_from: None,
                    });
                } else if uidvalidity_changed {
                    // §3.4: a bump invalidates UIDs, not tasks. Nothing is
                    // deleted on this path.
                    plan.upload.push(UploadTarget {
                        task_id: row.id,
                        task_uuid: task_uuid.to_string(),
                        reason: UploadReason::UidValidityReset,
                        supersedes: Vec::new(),
                        merge_log_from: None,
                    });
                } else if row.is_dirty {
                    // Deleted on the server while this machine had unsent
                    // changes. Re-creating it is the only outcome that does not
                    // silently discard an edit; the deletion can be repeated,
                    // the edit cannot be recovered.
                    plan.upload.push(UploadTarget {
                        task_id: row.id,
                        task_uuid: task_uuid.to_string(),
                        reason: UploadReason::MissingOnServer,
                        supersedes: Vec::new(),
                        merge_log_from: None,
                    });
                } else {
                    // Gone from the folder with nothing local to protect: an
                    // expunge here or by another client, and indistinguishable
                    // either way (§10.1).
                    plan.delete_local.push(LocalDeletion {
                        task_id: row.id,
                        task_uuid: task_uuid.to_string(),
                    });
                }
            }

            (None, None) => unreachable!("a task id came from one of the two sides"),
        }
    }

    plan.registry = plan_registry(&registry_messages, &local_registry, &mut plan.expunge);

    for (attachment_uuid, group) in server_attachments {
        // A payload never changes under its id, so the first copy written is
        // the payload, and a later one under the same id is either a retried
        // APPEND of the same bytes or somebody else's bytes. Neither is worth
        // the risk of picking it: the revision header, which decides between
        // copies of a task, is nothing a payload message writes and anything
        // a forged one can. The extras are left alone; they go when the task
        // is purged, with every other message of its id.
        let winner = first_written(&group);

        let owner = winner.identity.task_uuid.as_str();
        if server_tasks.contains_key(owner) || known.contains_key(owner) {
            plan.attachments.push(ListedAttachment {
                uid: winner.uid,
                task_uuid: owner.to_string(),
                attachment_uuid: attachment_uuid.to_string(),
                message_id: winner.message_id.clone(),
            });
        } else {
            // §9: the task is gone from both sides, so nothing will ever ask
            // for this payload again.
            plan.expunge.push(ExpungeTarget {
                uid: winner.uid,
                reason: ExpungeReason::Orphan,
            });
        }
    }

    // Deterministic output, so a plan can be compared and a sync's work is
    // ordered by UID rather than by hash iteration.
    plan.download.sort_by_key(|target| target.uid);
    plan.upload.sort_by_key(|target| target.task_id);
    plan.rebind.sort_by_key(|target| target.task_id);
    plan.delete_local.sort_by_key(|target| target.task_id);
    plan.expunge.sort_by_key(|target| target.uid);
    plan.attachments.sort_by_key(|target| target.uid);

    plan
}

/// Which copy of a task id wins, and which are the losers.
///
/// Highest revision, ties broken by highest UID. The tie-break matters more
/// than it looks: two messages at the same revision are what an append retried
/// after a lost response leaves behind, and picking the newest one means the
/// copy that was written last is the one kept.
fn pick_winner<'m>(group: &[&'m ListedMessage]) -> (&'m ListedMessage, Vec<&'m ListedMessage>) {
    let winner = group
        .iter()
        .copied()
        .max_by_key(|message| (message.identity.revision, message.uid))
        .expect("a group exists because it has at least one message");
    let losers = group
        .iter()
        .copied()
        .filter(|message| message.uid != winner.uid)
        .collect();
    (winner, losers)
}

/// The copy of an attachment payload that was written first: the lowest UID.
fn first_written<'m>(group: &[&'m ListedMessage]) -> &'m ListedMessage {
    group
        .iter()
        .copied()
        .min_by_key(|message| message.uid)
        .expect("a group exists because it has at least one message")
}

fn target(message: &ListedMessage) -> DownloadTarget {
    DownloadTarget {
        uid: message.uid,
        task_uuid: message.identity.task_uuid.clone(),
        revision: message.identity.revision,
    }
}

fn uids_of(group: &[&ListedMessage]) -> Vec<u32> {
    let mut uids: Vec<u32> = group.iter().map(|message| message.uid).collect();
    uids.sort_unstable();
    uids
}

fn expunge_all(group: &[&ListedMessage], reason: ExpungeReason) -> Vec<ExpungeTarget> {
    group
        .iter()
        .map(|message| ExpungeTarget {
            uid: message.uid,
            reason,
        })
        .collect()
}

/// Whether the server's copy at this machine's revision is another machine's
/// edit rather than the copy this machine has. Every upload has a
/// `Message-ID` of its own, so a different one is a different edit; a
/// different UID under the same `Message-ID` is the same edit moved, as after
/// a `UIDVALIDITY` bump. Unknown on either side, it is taken for the same
/// copy, which is what this machine assumed before it recorded them.
fn another_edit(local: Option<&str>, winner: &ListedMessage) -> bool {
    matches!(
        (local, winner.message_id.as_deref()),
        (Some(ours), Some(theirs)) if ours != theirs
    )
}

/// A rebind, when the cached UID or `Message-ID` no longer matches the server.
fn rebind_if_moved(row: &TaskIndexRow, winner: &ListedMessage) -> Option<Rebind> {
    let message_id = winner
        .message_id
        .clone()
        .or_else(|| row.message_id.clone())?;
    let moved = row.imap_uid != Some(winner.uid) || row.message_id.as_ref() != Some(&message_id);
    moved.then(|| Rebind {
        task_id: row.id,
        task_uuid: row.task_uuid.clone(),
        uid: winner.uid,
        revision: row.revision,
        message_id,
    })
}

/// The tag registry follows the task rules under a well-known id, so two
/// registry messages are the ordinary "one id, two copies" conflict rather than
/// a special case.
fn plan_registry(
    messages: &[&ListedMessage],
    local: &RegistryState,
    expunge: &mut Vec<ExpungeTarget>,
) -> RegistryAction {
    let local_revision = local.revision;
    if messages.is_empty() {
        // A registry that exists locally but not on the server: either this is
        // the first machine, or the message was deleted. Either way the local
        // one is the only copy there is.
        return if local_revision > 0 {
            RegistryAction::Upload {
                supersedes: Vec::new(),
            }
        } else {
            RegistryAction::Idle
        };
    }

    let (winner, losers) = pick_winner(messages);
    // The server's copy is one this machine has not seen: newer, or another
    // machine's at the same revision. An unknown copy of its own counts as
    // seen, as it always did.
    let unseen = winner.identity.revision > local_revision
        || another_edit(local.message_id.as_deref(), winner);
    if unseen && local.dirty {
        // Changed on both sides: merged tag by tag, never one side dropped.
        RegistryAction::Merge {
            from: target(winner),
            supersedes: uids_of(messages),
        }
    } else if unseen {
        expunge.extend(expunge_all(&losers, ExpungeReason::Duplicate));
        RegistryAction::Download(target(winner))
    } else if winner.identity.revision < local_revision || local.dirty {
        // Ahead, or changed here and not yet sent.
        RegistryAction::Upload {
            supersedes: uids_of(messages),
        }
    } else {
        expunge.extend(expunge_all(&losers, ExpungeReason::Duplicate));
        RegistryAction::Idle
    }
}

/// Waiting's marks (chat-spec §6) follow the registry's rules under their own
/// well-known id; the losing copies go with the plan's other expunges.
pub fn plan_chat_marks(
    listing: &[ListedMessage],
    local: &crate::chat::marks::MarksState,
    expunge: &mut Vec<ExpungeTarget>,
) -> RegistryAction {
    let messages: Vec<&ListedMessage> = listing
        .iter()
        .filter(|message| message.identity.kind == TaskMessageKind::ChatMarks)
        .collect();
    let state = RegistryState {
        revision: local.revision,
        dirty: local.dirty,
        message_id: local.message_id.clone(),
    };
    plan_registry(&messages, &state, expunge)
}

/// The Today cut-off (§10.2) follows the registry's rules under its own
/// well-known id. Its merge is the later instant, which needs no base.
pub fn plan_today_cleared(
    listing: &[ListedMessage],
    local: &RegistryState,
    expunge: &mut Vec<ExpungeTarget>,
) -> RegistryAction {
    let messages: Vec<&ListedMessage> = listing
        .iter()
        .filter(|message| message.identity.kind == TaskMessageKind::TodayCleared)
        .collect();
    plan_registry(&messages, local, expunge)
}

/// Two machines' tag registries merged against the registry they last
/// shared (`base`), tag by tag (§7). What either side created, renamed or
/// recoloured is kept; what either side deleted, and the other left as it
/// was, stays deleted; a tag both sides changed differently is taken from
/// `local`, the side about to upload. Without a base — a registry from
/// before one was recorded — every tag either side has is kept.
pub fn merge_tag_registries(
    base: Option<&[TaskTag]>,
    local: &[TaskTag],
    remote: &[TaskTag],
) -> Vec<TaskTag> {
    use std::collections::BTreeMap;

    fn same(a: &TaskTag, b: &TaskTag) -> bool {
        a.name == b.name && a.colour == b.colour && a.position == b.position
    }
    let index = |tags: &[TaskTag]| -> BTreeMap<String, TaskTag> {
        tags.iter()
            .map(|tag| (tag.tag_uuid.clone(), tag.clone()))
            .collect()
    };
    let base = index(base.unwrap_or(&[]));
    let ours = index(local);
    let theirs = index(remote);

    let mut ids: Vec<&String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    ids.sort();
    ids.dedup();

    let mut merged = Vec::new();
    for id in ids {
        let kept = match (base.get(id), ours.get(id), theirs.get(id)) {
            (Some(b), Some(l), Some(r)) => Some(if same(l, b) { r } else { l }),
            // Deleted on one side: gone, unless the other side changed it.
            (Some(b), Some(l), None) => (!same(l, b)).then_some(l),
            (Some(b), None, Some(r)) => (!same(r, b)).then_some(r),
            (Some(_), None, None) => None,
            (None, Some(l), _) => Some(l),
            (None, None, Some(r)) => Some(r),
            (None, None, None) => None,
        };
        if let Some(tag) = kept {
            merged.push(TaskTag {
                id: None,
                ..tag.clone()
            });
        }
    }
    merged.sort_by(|a, b| {
        a.position
            .partial_cmp(&b.position)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.tag_uuid.cmp(&b.tag_uuid))
    });
    merged
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// What one sync did.
#[derive(Debug, Default, Clone)]
pub struct TaskSyncReport {
    /// Messages the folder listing returned.
    pub listed: usize,
    /// Tasks applied from the server.
    pub downloaded: usize,
    /// Tasks written to the server.
    pub uploaded: usize,
    /// Tasks removed locally because the server no longer has them.
    pub deleted: usize,
    /// Trashed tasks past their retention, removed from both sides.
    pub purged: usize,
    /// Messages expunged: losing duplicates, superseded copies, orphans.
    pub expunged: usize,
    /// Cached UIDs repaired without moving any task data.
    pub rebound: usize,
    /// Attachment payloads uploaded.
    pub attachments_uploaded: usize,
    /// Whether the folder's `UIDVALIDITY` had changed since the last sync.
    pub uidvalidity_reset: bool,
    /// Messages in the folder this build cannot read. Left untouched.
    pub unreadable: usize,
    /// Failures that did not stop the sync.
    pub errors: Vec<String>,
}

impl TaskSyncReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn merge(&mut self, other: TaskSyncReport) {
        self.listed += other.listed;
        self.downloaded += other.downloaded;
        self.uploaded += other.uploaded;
        self.deleted += other.deleted;
        self.purged += other.purged;
        self.expunged += other.expunged;
        self.rebound += other.rebound;
        self.attachments_uploaded += other.attachments_uploaded;
        self.uidvalidity_reset |= other.uidvalidity_reset;
        self.unreadable += other.unreadable;
        self.errors.extend(other.errors);
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Whether the sync moved anything at all. A listing on its own does not
    /// count: reading the folder and finding it unchanged is the normal case.
    pub fn is_empty(&self) -> bool {
        self.downloaded == 0
            && self.uploaded == 0
            && self.deleted == 0
            && self.purged == 0
            && self.expunged == 0
            && self.rebound == 0
            && self.attachments_uploaded == 0
    }
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// Everything needed to write one task to the server.
struct UploadJob {
    task: Task,
    reminders: Vec<TaskReminder>,
    links: Vec<TaskLinkedEmail>,
    attachments: Vec<TaskAttachment>,
    supersedes: Vec<u32>,
    reason: UploadReason,
}

/// Bidirectional sync of the tasks folder.
///
/// Owns its database handle rather than borrowing one: a sync is spawned onto
/// the runtime, and a future holding `&Database` is not `Send` because a
/// `rusqlite::Connection` is not `Sync`. The same reason is why no
/// [`TaskRepository`] is ever held across an `await` in here.
pub struct TaskSyncService {
    db: Database,
    account_name: String,
    folder: String,
}

impl TaskSyncService {
    /// Open the cache at `db_path` for the designated tasks account.
    pub fn open(
        db_path: &str,
        account_name: impl Into<String>,
        folder: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::with_database(
            Database::new(db_path)?,
            account_name,
            folder,
        ))
    }

    /// Take an already-open database, for tests and for callers that have one.
    pub fn with_database(
        db: Database,
        account_name: impl Into<String>,
        folder: impl Into<String>,
    ) -> Self {
        Self {
            db,
            account_name: account_name.into(),
            folder: folder.into(),
        }
    }

    pub fn account_name(&self) -> &str {
        &self.account_name
    }

    /// The folder as it is known on the server, once resolved.
    pub fn folder(&self) -> &str {
        &self.folder
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Resolve the configured folder against the server's namespace, create it
    /// if it is missing, and remember what it turned out to be called.
    ///
    /// Recording the resolved name means the rest of the app can find the
    /// folder without asking the server again — and, more importantly, that the
    /// name a task's messages are stored under survives a restart even if the
    /// server's folder list is briefly unavailable.
    pub async fn resolve_folder(&mut self, client: &mut ImapClient) -> Result<String> {
        let resolved = ensure_tasks_folder(client, &self.folder).await?;
        self.folder = resolved.clone();

        let repo = TaskRepository::new(&self.db);
        let mut state = repo
            .get_sync_state(&self.account_name)?
            .unwrap_or_else(|| TaskSyncState::new(self.account_name.clone()));
        if state.folder_name != resolved {
            state.folder_name = resolved.clone();
            repo.upsert_sync_state(&state)?;
        }

        Ok(resolved)
    }

    /// One full sync of the tasks folder.
    ///
    /// The order is not arbitrary. The listing comes first, because everything
    /// else is decided against it. Expired trash is purged next, so its
    /// messages leave the server in the same pass that removed the rows — a
    /// purged task whose message survives comes back from the dead on the next
    /// listing. Downloads run before uploads so that a task the server has
    /// already moved on is not uploaded from a stale copy. Uploads read the
    /// dirty set *after* the downloads for the same reason. Expunges run last
    /// within each step, always after the copy that replaces them is confirmed
    /// present on the server.
    pub async fn sync(&mut self, client: &mut ImapClient) -> Result<TaskSyncReport> {
        let mut report = TaskSyncReport::new();
        let now = Utc::now();

        // 1. SELECT, and compare UIDVALIDITY (§3.4).
        //
        // A folder that cannot be selected is resolved again rather than
        // treated as fatal: it may have been renamed into the server's
        // namespace, or deleted outright by another client, and re-creating it
        // is strictly better than a client that stops syncing tasks until
        // somebody reads a log.
        if let Err(error) = client.select_mailbox(&self.folder).await {
            logger::warn(&format!(
                "{}: '{}' could not be selected ({error}); resolving the tasks folder again.",
                self.account_name, self.folder
            ));
            self.resolve_folder(client).await?;
        }
        let mailbox = client.current_mailbox().cloned();
        let uidvalidity = mailbox.as_ref().and_then(|info| info.uid_validity);
        let highest_modseq = mailbox.as_ref().and_then(|info| info.highest_modseq);
        let exists = mailbox.as_ref().map(|info| info.exists).unwrap_or(0);

        let (device_id, uidvalidity_changed) = {
            let repo = TaskRepository::new(&self.db);
            let device_id = repo.ensure_device_id(&self.account_name)?;
            let state = repo.get_sync_state(&self.account_name)?;
            let changed = match (state.as_ref(), uidvalidity) {
                (Some(state), Some(current)) => state.uidvalidity_changed(current),
                _ => false,
            };
            if changed {
                // Not a data loss event, and specifically not a reason to
                // delete anything: the UIDs stop meaning what they meant, and
                // the reconciliation below runs on task ids, which do not.
                logger::warn(&format!(
                    "{}: UIDVALIDITY of '{}' changed to {:?}; every cached UID is being discarded \
                     and the folder reconciled by task id. No task is deleted on this path.",
                    self.account_name, self.folder, uidvalidity
                ));
                repo.clear_uids_for_account(&self.account_name)?;
            }
            (device_id, changed)
        };
        report.uidvalidity_reset = uidvalidity_changed;

        // 2. The full listing. Never a MODSEQ delta — see the module docs.
        let entries = client
            .fetch_header_listing("1:*", &listing_fields())
            .await?;
        let listing = read_listing(&entries);
        report.listed = entries.len();
        report.unreadable = listing.unreadable.len();
        if !listing.unreadable.is_empty() {
            logger::warn(&format!(
                "{}: {} message(s) in '{}' could not be identified as tasks and were left \
                 untouched (UIDs {:?})",
                self.account_name,
                listing.unreadable.len(),
                self.folder,
                listing.unreadable,
            ));
        }
        if exists as usize != entries.len() {
            // Not fatal — a message can arrive between SELECT and FETCH — but
            // a persistent mismatch is the signature of a server that answers a
            // full listing with less than it holds, which is the one thing this
            // design cannot tolerate.
            logger::debug(&format!(
                "{}: '{}' reports EXISTS {exists} but the listing returned {}",
                self.account_name,
                self.folder,
                entries.len()
            ));
        }

        // Only UIDs seen in *this* listing may ever be expunged. A cached UID
        // can belong to a different message after a UIDVALIDITY bump, and
        // deleting the wrong message here destroys mail.
        let listed_uids: HashSet<u32> = entries.iter().map(|entry| entry.uid).collect();

        // 3. Expired trash, and the messages that go with it (§10.1).
        //
        // The tombstones left by every earlier purge are drained here too, and
        // in the same breath, because they are the same job: a task whose row
        // is gone and whose message is not. Emptying the trash by hand cannot
        // expunge anything itself — there may be no connection at the time —
        // and an expunge that failed last sync has to be retried, so the record
        // of what to remove outlives both.
        let expired = {
            let repo = TaskRepository::new(&self.db);
            repo.purge_expired_trash(now)?
        };
        report.purged = expired.len();

        let purged = {
            let repo = TaskRepository::new(&self.db);
            let mut pending = repo.pending_expunges()?;
            // `purge_expired_trash` wrote its own tombstones a moment ago, so
            // reading them back is what keeps the two paths identical rather
            // than merely similar — and de-duplicates them at the same time.
            let known: HashSet<&str> = pending.iter().map(|t| t.task_uuid.as_str()).collect();
            let extra: Vec<PurgedTask> = expired
                .iter()
                .filter(|task| !known.contains(task.task_uuid.as_str()))
                .cloned()
                .collect();
            pending.extend(extra);
            pending
        };

        let purged_uuids: HashSet<&str> =
            purged.iter().map(|task| task.task_uuid.as_str()).collect();
        if !purged.is_empty() {
            let uids = purge_uids(&purged, &listing, &listed_uids);
            let expunged = match client.delete_messages(&uids).await {
                Ok(()) => {
                    report.expunged += uids.len();
                    true
                }
                Err(error) => {
                    report
                        .errors
                        .push(format!("expunging purged task messages failed: {error}"));
                    false
                }
            };
            for task in &purged {
                remove_cached_attachments(&task.task_uuid);
            }

            // A tombstone is forgotten once the server has stopped listing the
            // task — either because this expunge removed it, or because it was
            // never there. Keeping it after a failed expunge is the whole point:
            // the retry is the next sync. Keeping it forever would be a slow
            // leak, which is why "not in the listing" is enough to drop it.
            let listed_uuids: HashSet<&str> = listing
                .messages
                .iter()
                .map(|message| message.identity.task_uuid.as_str())
                .collect();
            let settled: Vec<String> = purged
                .iter()
                .filter(|task| expunged || !listed_uuids.contains(task.task_uuid.as_str()))
                .map(|task| task.task_uuid.clone())
                .collect();
            let repo = TaskRepository::new(&self.db);
            if let Err(error) = repo.forget_expunges(&settled) {
                report
                    .errors
                    .push(format!("clearing expunge records failed: {error}"));
            }
        }

        // 4. Decide everything, against the listing minus what was just purged.
        let live: Vec<ListedMessage> = listing
            .messages
            .iter()
            .filter(|message| !purged_uuids.contains(message.identity.task_uuid.as_str()))
            .cloned()
            .collect();

        let (local, registry) = {
            let repo = TaskRepository::new(&self.db);
            (
                repo.task_index(&self.account_name)?,
                repo.tag_registry_state()?,
            )
        };
        let mut plan = reconcile(&local, &live, registry, uidvalidity_changed);
        let marks_state = crate::chat::marks::state(&self.db.conn)?;
        let marks = plan_chat_marks(&live, &marks_state, &mut plan.expunge);
        let cleared_state = TaskRepository::new(&self.db).today_cleared_state()?;
        let cleared = plan_today_cleared(&live, &cleared_state, &mut plan.expunge);
        // A payload message with no attachment id belongs to nothing that can be
        // named; it is counted with the rest of what this build cannot read, and
        // like them it is left where it is.
        report.unreadable += plan.unidentified;

        // 5. Downloads: task bodies and, if it moved, the tag registry.
        let mut wanted: Vec<u32> = plan.download.iter().map(|target| target.uid).collect();
        if let RegistryAction::Download(ref target) = plan.registry {
            wanted.push(target.uid);
        }
        if let RegistryAction::Download(ref target) = marks {
            wanted.push(target.uid);
        }
        // A merge of the cut-off is a download that stays dirty.
        if let RegistryAction::Download(ref target)
        | RegistryAction::Merge {
            from: ref target, ..
        } = cleared
        {
            wanted.push(target.uid);
        }
        if !wanted.is_empty() {
            let bodies = client.fetch_bodies(&wanted).await?;
            self.apply_downloads(&bodies, &mut report);
        }

        // 6. Rebinds: the cached location of tasks that are otherwise in step.
        if !plan.rebind.is_empty() {
            let repo = TaskRepository::new(&self.db);
            for rebind in &plan.rebind {
                // `mark_uploaded` is the compare-and-swap that records where a
                // task lives; a rebind is the same statement with the revision
                // it already has, so a task that was edited in the meantime
                // keeps its dirty flag.
                match repo.mark_uploaded(
                    rebind.task_id,
                    rebind.revision,
                    rebind.uid,
                    &rebind.message_id,
                ) {
                    Ok(_) => report.rebound += 1,
                    Err(error) => report.errors.push(format!(
                        "recording the location of task {} failed: {error}",
                        rebind.task_uuid
                    )),
                }
            }
        }

        // 7a. The logs of other machines' copies that the uploads below are
        //     about to replace, so no machine's history is dropped (§11).
        let merge: Vec<u32> = plan
            .upload
            .iter()
            .filter_map(|target| target.merge_log_from)
            .collect();
        if !merge.is_empty() {
            let bodies = client.fetch_bodies(&merge).await?;
            self.merge_logs_from(&bodies, &mut report);
        }

        // 7. Uploads. The dirty set is read now, after the downloads, so a task
        //    the server has already moved past is not written back from a stale
        //    copy — and so that a task edited *while* this sync was fetching is
        //    included rather than missed.
        let jobs = self.upload_jobs(&plan)?;
        for job in jobs {
            if let Err(error) = self.upload(client, job, &mut report).await {
                report.errors.push(format!("upload failed: {error}"));
            }
        }

        // 8. The tag registry: merged with another machine's first if both
        //    changed it, then uploaded if this machine's copy is the newer.
        if let RegistryAction::Merge {
            ref from,
            ref supersedes,
        } = plan.registry
        {
            let merged = match self.merge_registry(client, from).await {
                Ok(()) => {
                    self.upload_registry(client, supersedes, now, &mut report)
                        .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = merged {
                report
                    .errors
                    .push(format!("merging the tag registry failed: {error}"));
            }
        }
        if let RegistryAction::Upload { ref supersedes } = plan.registry
            && let Err(error) = self
                .upload_registry(client, supersedes, now, &mut report)
                .await
        {
            report
                .errors
                .push(format!("uploading the tag registry failed: {error}"));
        }

        // 8b. Waiting's marks, by the same rules as the registry.
        if let RegistryAction::Merge {
            ref from,
            ref supersedes,
        } = marks
        {
            let merged = match self.merge_chat_marks(client, from).await {
                Ok(()) => {
                    self.upload_chat_marks(client, supersedes, now, &mut report)
                        .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = merged {
                report
                    .errors
                    .push(format!("merging the chat marks failed: {error}"));
            }
        }
        if let RegistryAction::Upload { ref supersedes } = marks
            && let Err(error) = self
                .upload_chat_marks(client, supersedes, now, &mut report)
                .await
        {
            report
                .errors
                .push(format!("uploading the chat marks failed: {error}"));
        }

        // 8c. The Today cut-off, by the same rules.
        if let RegistryAction::Upload { ref supersedes }
        | RegistryAction::Merge { ref supersedes, .. } = cleared
            && let Err(error) = self
                .upload_today_cleared(client, supersedes, now, &mut report)
                .await
        {
            report
                .errors
                .push(format!("uploading the Today cut-off failed: {error}"));
        }

        // 9. Local deletions: tasks the server no longer has (§3.3 step 4).
        if !plan.delete_local.is_empty() {
            let repo = TaskRepository::new(&self.db);
            for deletion in &plan.delete_local {
                match repo.delete_task(deletion.task_id) {
                    Ok(Some(_)) => {
                        report.deleted += 1;
                        remove_cached_attachments(&deletion.task_uuid);
                    }
                    Ok(None) => {}
                    Err(error) => report.errors.push(format!(
                        "deleting task {} locally failed: {error}",
                        deletion.task_uuid
                    )),
                }
            }
        }

        // 10. Attachment payloads the server holds: record where they are, so
        //     they can be fetched on demand from any machine. The index is read
        //     again rather than reused, so that a task downloaded a moment ago
        //     — the whole of it, on a machine whose cache was just deleted —
        //     learns where its payloads are in this same pass.
        let current = {
            let repo = TaskRepository::new(&self.db);
            repo.task_index(&self.account_name)?
        };
        self.bind_attachments(&plan.attachments, &current, &mut report);

        // 11. Losing duplicates and orphaned payloads. Every one of these is a
        //     message whose replacement has already been confirmed present.
        let doomed: Vec<u32> = plan
            .expunge
            .iter()
            .map(|target| target.uid)
            .filter(|uid| listed_uids.contains(uid))
            .collect();
        if !doomed.is_empty() {
            match client.delete_messages(&doomed).await {
                Ok(()) => report.expunged += doomed.len(),
                Err(error) => report
                    .errors
                    .push(format!("expunging superseded messages failed: {error}")),
            }
        }

        // 12. Record what was seen. The MODSEQ is stored for diagnostics only:
        //     nothing reads it back as the record of what changed.
        {
            let repo = TaskRepository::new(&self.db);
            let mut state = repo
                .get_sync_state(&self.account_name)?
                .unwrap_or_else(|| TaskSyncState::new(self.account_name.clone()));
            state.folder_name = self.folder.clone();
            state.uidvalidity = uidvalidity;
            state.highest_modseq = highest_modseq;
            state.last_sync_time = Some(format_timestamp(now));
            state.last_listing_at = Some(format_timestamp(now));
            state.last_listing_at_unix = Some(now.timestamp());
            state.last_listing_count = Some(entries.len() as i64);
            state.device_id = Some(device_id);
            repo.upsert_sync_state(&state)?;
        }

        Ok(report)
    }

    /// The folder the last sync resolved, if one ever has.
    pub fn resolved_folder(&self) -> Result<Option<String>> {
        let repo = TaskRepository::new(&self.db);
        Ok(repo
            .get_sync_state(&self.account_name)?
            .filter(|state| state.uidvalidity.is_some())
            .map(|state| state.folder_name))
    }

    /// Upload the tasks edited here, without listing the folder (§23).
    ///
    /// The caller has established that nobody else changed the folder since
    /// the last listing, so the server holds exactly what this machine last
    /// saw: every dirty task goes up as in step 7 of [`Self::sync`], and the
    /// message it replaces is its own recorded one. Returns `None` when that
    /// is not enough — a changed `UIDVALIDITY`, a folder that cannot be
    /// selected, or a backlog with more in it than dirty tasks — and only a
    /// full sync will do.
    pub async fn push(&mut self, client: &mut ImapClient) -> Result<Option<TaskSyncReport>> {
        let (state, backlog) = {
            let repo = TaskRepository::new(&self.db);
            (
                repo.get_sync_state(&self.account_name)?,
                repo.upload_backlog(&self.account_name)?,
            )
        };
        let Some(state) = state else {
            return Ok(None);
        };
        let Some(known) = state.uidvalidity else {
            return Ok(None);
        };
        if !backlog.only_dirty_tasks() {
            return Ok(None);
        }
        let mut report = TaskSyncReport::new();
        if backlog.dirty == 0 {
            return Ok(Some(report));
        }

        self.folder = state.folder_name;
        if let Err(error) = client.select_mailbox(&self.folder).await {
            if error.is_connection_error() {
                return Err(error);
            }
            return Ok(None);
        }
        // The recorded UIDs, which the uploads delete, were issued under the
        // recorded UIDVALIDITY; under another one they name other messages.
        if client.current_mailbox().and_then(|info| info.uid_validity) != Some(known) {
            return Ok(None);
        }

        for job in self.upload_jobs(&SyncPlan::default())? {
            if let Err(error) = self.upload(client, job, &mut report).await {
                if error.is_connection_error() {
                    return Err(error);
                }
                report.errors.push(format!("upload failed: {error}"));
            }
        }
        Ok(Some(report))
    }

    /// Apply fetched bodies: tasks, and the tag registry when it came along.
    ///
    /// Every apply re-checks the local revision before overwriting. The plan was
    /// decided against a snapshot taken before the fetch, and the user may have
    /// edited a task while it was in flight; without this check that edit would
    /// be overwritten by a copy that is now older than it. The rule is the same
    /// one the plan uses — a lower revision never wins — enforced at the last
    /// possible moment, which is the only place it can be enforced honestly.
    fn apply_downloads(&self, bodies: &[(u32, Vec<u8>)], report: &mut TaskSyncReport) {
        let repo = TaskRepository::new(&self.db);

        for (uid, bytes) in bodies {
            let parsed = match TaskEmailParser::parse(bytes, &self.account_name, Some(*uid)) {
                Ok(parsed) => parsed,
                Err(error) => {
                    // Unreadable, including a wire version this build does not
                    // know. Skipped, never deleted.
                    report
                        .errors
                        .push(format!("UID {uid} could not be read: {error}"));
                    continue;
                }
            };

            match parsed {
                TaskMessage::Task(parsed) => {
                    let incoming = parsed.task;
                    match repo.get_task_by_uuid(&incoming.task_uuid) {
                        Ok(Some(existing)) if existing.revision > incoming.revision => {
                            logger::info(&format!(
                                "Task {} was edited locally to revision {} while revision {} was \
                                 downloading; keeping the local copy and re-uploading it.",
                                incoming.task_uuid, existing.revision, incoming.revision
                            ));
                            continue;
                        }
                        Err(error) => {
                            report.errors.push(format!(
                                "reading local task {} failed: {error}",
                                incoming.task_uuid
                            ));
                            continue;
                        }
                        _ => {}
                    }

                    match repo.store_remote_task(
                        &incoming,
                        &parsed.reminders,
                        &parsed.linked_emails,
                        &parsed.attachments,
                    ) {
                        Ok(_) => report.downloaded += 1,
                        Err(error) => report.errors.push(format!(
                            "applying task {} failed: {error}",
                            incoming.task_uuid
                        )),
                    }
                }

                TaskMessage::TagRegistry(registry) => {
                    if let Err(error) = repo.replace_tag_registry(
                        &registry.tags,
                        registry.revision,
                        registry.message_id.as_deref(),
                    ) {
                        report
                            .errors
                            .push(format!("applying the tag registry failed: {error}"));
                    }
                }

                // Payloads are fetched on demand, not on sync (§9). Their
                // location is recorded from the listing instead.
                TaskMessage::Attachment(_) => {}

                // Counted with the tasks: it changes what Today lists.
                TaskMessage::TodayCleared(cleared) => {
                    match TaskRepository::new(&self.db).take_in_today_cleared(
                        cleared.cleared_at,
                        cleared.revision,
                        cleared.message_id.as_deref(),
                    ) {
                        Ok(()) => report.downloaded += 1,
                        Err(error) => report
                            .errors
                            .push(format!("applying the Today cut-off failed: {error}")),
                    }
                }

                TaskMessage::ChatMarks(marks) => {
                    if let Err(error) = crate::chat::marks::take_in(
                        &self.db.conn,
                        &marks.marks,
                        marks.revision,
                        marks.message_id.as_deref(),
                        Utc::now().timestamp_millis(),
                    ) {
                        report
                            .errors
                            .push(format!("applying the chat marks failed: {error}"));
                    }
                }
            }
        }
    }

    /// Take in the logs of other machines' copies (see step 7a of the sync).
    fn merge_logs_from(&self, bodies: &[(u32, Vec<u8>)], report: &mut TaskSyncReport) {
        let repo = TaskRepository::new(&self.db);
        for (uid, bytes) in bodies {
            match TaskEmailParser::parse(bytes, &self.account_name, Some(*uid)) {
                Ok(TaskMessage::Task(parsed)) => {
                    if let Err(error) =
                        repo.merge_remote_log(&parsed.task.task_uuid, &parsed.task.log)
                    {
                        report.errors.push(format!(
                            "merging the log of task {} failed: {error}",
                            parsed.task.task_uuid
                        ));
                    }
                }
                Ok(_) => {}
                Err(error) => report
                    .errors
                    .push(format!("UID {uid} could not be read: {error}")),
            }
        }
    }

    /// Load everything the pending uploads need, in the order the backlog has to
    /// reach the server in: oldest revision first (§3.6).
    fn upload_jobs(&self, plan: &SyncPlan) -> Result<Vec<UploadJob>> {
        let repo = TaskRepository::new(&self.db);

        let planned: HashMap<i64, &UploadTarget> = plan
            .upload
            .iter()
            .map(|target| (target.task_id, target))
            .collect();

        // A task the server is ahead on is never uploaded, even if it is still
        // dirty: being dirty here means its download failed, and answering that
        // with an older copy starts a fight between two machines that the next
        // listing has to settle anyway. The download is retried instead.
        let downloading: HashSet<&str> = plan
            .download
            .iter()
            .map(|target| target.task_uuid.as_str())
            .collect();

        // The dirty set is the authority on what has to go up; the plan adds
        // the tasks that are clean locally but wrong or missing on the server.
        let mut tasks = repo.get_dirty_tasks(&self.account_name)?;
        tasks.retain(|task| !downloading.contains(task.task_uuid.as_str()));
        let dirty: HashSet<i64> = tasks.iter().filter_map(|task| task.id).collect();
        for target in &plan.upload {
            if !dirty.contains(&target.task_id)
                && let Some(task) = repo.get_task(target.task_id)?
            {
                tasks.push(task);
            }
        }
        tasks.sort_by_key(|task| (task.revision, task.id));

        let mut jobs = Vec::with_capacity(tasks.len());
        for task in tasks {
            let Some(task_id) = task.id else { continue };
            let target = planned.get(&task_id);
            jobs.push(UploadJob {
                reminders: repo.get_reminders(task_id)?,
                links: repo.get_linked_emails(task_id)?,
                attachments: repo.get_attachments(task_id)?,
                supersedes: target
                    .map(|target| target.supersedes.clone())
                    .unwrap_or_default(),
                reason: target
                    .map(|target| target.reason)
                    .unwrap_or(UploadReason::LocalEdit),
                task,
            });
        }

        Ok(jobs)
    }

    /// Write one task to the server: APPEND, confirm, record, then delete what
    /// it replaced.
    ///
    /// The sequence is the reliability guarantee. The task's uuid was minted
    /// when the task was created, so a retry after a lost response cannot
    /// produce a second task — only a second message for the same task, which
    /// the revision rule resolves. The new message is located by the
    /// `Message-ID` it was built with, because an APPEND reports success and not
    /// a UID. Nothing is deleted until that search has found the new copy: a
    /// crash anywhere before it leaves a duplicate, which is recoverable, rather
    /// than a gap, which is not.
    async fn upload(
        &mut self,
        client: &mut ImapClient,
        job: UploadJob,
        report: &mut TaskSyncReport,
    ) -> Result<()> {
        let Some(task_id) = job.task.id else {
            return Ok(());
        };

        // Payloads first: the task message lists its attachments, so a
        // descriptor must never reach the server ahead of the payload it
        // describes. A payload that fails to upload leaves the task dirty, so
        // both are retried together on the next sync.
        let mut attachments = job.attachments;
        for attachment in &mut attachments {
            if attachment.imap_uid.is_some() {
                continue;
            }
            match self
                .upload_attachment(client, &job.task.task_uuid, attachment)
                .await
            {
                Ok(true) => report.attachments_uploaded += 1,
                Ok(false) => {}
                Err(error) => {
                    report.errors.push(format!(
                        "uploading attachment '{}' of task {} failed: {error}",
                        attachment.filename, job.task.task_uuid
                    ));
                    return Ok(());
                }
            }
        }

        let revision = job.task.revision;
        let previous_uid = job.task.imap_uid;
        let (bytes, message_id) =
            TaskEmailBuilder::build(&job.task, &job.reminders, &job.links, &attachments)?;

        // \Seen on purpose: the tasks folder is not a mailbox anybody reads, and
        // a folder full of "unread" messages is what another mail client would
        // otherwise show.
        client.append_message(&self.folder, &bytes, true).await?;

        let found = client.uid_search_message_id(&message_id).await?;
        let Some(&new_uid) = found.last() else {
            // The message is on the server — APPEND said so — but this
            // connection cannot see it yet. Leaving the task dirty and touching
            // nothing is the only safe move: the next listing finds it by task
            // id, and a duplicate (if the append is retried) is resolved by
            // revision and UID.
            logger::warn(&format!(
                "Task {} was appended to '{}' but could not be found again by Message-ID {}; \
                 leaving it dirty for the next sync.",
                job.task.task_uuid, self.folder, message_id
            ));
            return Ok(());
        };

        // §3.2: clearing the dirty flag is a compare-and-swap on the revision
        // that went to the server. An edit made while this upload was in flight
        // bumped the revision, and must stay dirty — otherwise it is marked
        // clean, never uploaded, and lost at the next download.
        let clean = {
            let repo = TaskRepository::new(&self.db);
            repo.mark_uploaded(task_id, revision, new_uid, &message_id)?
        };
        report.uploaded += 1;
        if !clean {
            logger::info(&format!(
                "Task {} was edited while revision {revision} was uploading; it stays dirty and \
                 goes up again on the next sync.",
                job.task.task_uuid
            ));
        }
        logger::debug(&format!(
            "Uploaded task {} revision {revision} as UID {new_uid} ({:?})",
            job.task.task_uuid, job.reason
        ));

        // Only now: everything this task used to live in. The winner is
        // confirmed present, so the losers can go.
        let mut superseded: Vec<u32> = job
            .supersedes
            .iter()
            .copied()
            .chain(previous_uid)
            .filter(|uid| *uid != new_uid)
            .collect();
        superseded.sort_unstable();
        superseded.dedup();
        if !superseded.is_empty() {
            match client.delete_messages(&superseded).await {
                Ok(()) => report.expunged += superseded.len(),
                // A failed delete is not a failed upload: the duplicate is
                // resolved by the revision rule on the next sync (§3.5).
                Err(error) => report.errors.push(format!(
                    "task {} was uploaded but its previous message(s) {superseded:?} could not be \
                     removed: {error}",
                    job.task.task_uuid
                )),
            }
        }

        Ok(())
    }

    /// Upload one attachment payload as its own message (§9).
    ///
    /// Returns whether anything was written. A payload that is not in the local
    /// cache cannot be uploaded from here — it exists on some other machine —
    /// and that is not an error.
    async fn upload_attachment(
        &mut self,
        client: &mut ImapClient,
        task_uuid: &str,
        attachment: &mut TaskAttachment,
    ) -> Result<bool> {
        let path = cached_attachment_path(&attachment.file_path)?;
        if !attachment.is_cached || !path.exists() {
            return Ok(false);
        }
        let payload = std::fs::read(&path)?;

        let (bytes, message_id) =
            TaskEmailBuilder::build_attachment(task_uuid, attachment, &payload, Utc::now())?;
        client.append_message(&self.folder, &bytes, true).await?;

        let found = client.uid_search_message_id(&message_id).await?;
        let Some(&uid) = found.last() else {
            logger::warn(&format!(
                "Attachment '{}' of task {task_uuid} was appended but could not be found again by \
                 Message-ID {message_id}; it will be retried.",
                attachment.filename
            ));
            return Ok(false);
        };

        {
            let repo = TaskRepository::new(&self.db);
            repo.update_attachment_storage(
                &attachment.attachment_uuid,
                Some(uid),
                Some(&message_id),
                attachment.is_cached,
            )?;
        }
        attachment.imap_uid = Some(uid);
        attachment.message_id = Some(message_id);

        Ok(true)
    }

    /// Write the tag registry, which is one message for the whole registry so
    /// that renaming a tag does not rewrite every task that carries it (§7).
    /// Take in another machine's registry, merged with this one's (§7).
    async fn merge_registry(
        &mut self,
        client: &mut ImapClient,
        from: &DownloadTarget,
    ) -> Result<()> {
        let bodies = client.fetch_bodies(&[from.uid]).await?;
        let Some((uid, bytes)) = bodies.first() else {
            return Ok(());
        };
        let TaskMessage::TagRegistry(remote) =
            TaskEmailParser::parse(bytes, &self.account_name, Some(*uid))?
        else {
            return Ok(());
        };
        let repo = TaskRepository::new(&self.db);
        let local = repo.list_tags()?;
        let base = repo.tag_registry_base()?;
        let merged = merge_tag_registries(base.as_deref(), &local, &remote.tags);
        let revision = repo.tag_registry_revision()?.max(remote.revision) + 1;
        repo.merge_tag_registry(&merged, revision)?;
        logger::info(&format!(
            "Merged the tag registry with another machine's: {} tags here, {} there, {} after",
            local.len(),
            remote.tags.len(),
            merged.len()
        ));
        Ok(())
    }

    async fn merge_chat_marks(
        &mut self,
        client: &mut ImapClient,
        from: &DownloadTarget,
    ) -> Result<()> {
        let bodies = client.fetch_bodies(&[from.uid]).await?;
        let Some((uid, bytes)) = bodies.first() else {
            return Ok(());
        };
        let TaskMessage::ChatMarks(remote) =
            TaskEmailParser::parse(bytes, &self.account_name, Some(*uid))?
        else {
            return Ok(());
        };
        crate::chat::marks::take_in(
            &self.db.conn,
            &remote.marks,
            remote.revision,
            remote.message_id.as_deref(),
            Utc::now().timestamp_millis(),
        )?;
        logger::info(&format!(
            "Merged the chat marks with another machine's: {} there",
            remote.marks.len()
        ));
        Ok(())
    }

    async fn upload_chat_marks(
        &mut self,
        client: &mut ImapClient,
        supersedes: &[u32],
        now: DateTime<Utc>,
        report: &mut TaskSyncReport,
    ) -> Result<()> {
        let (marks, revision) =
            crate::chat::marks::outgoing(&self.db.conn, now.timestamp_millis())?;
        let (bytes, message_id) = TaskEmailBuilder::build_chat_marks(&marks, revision, now)?;
        client.append_message(&self.folder, &bytes, true).await?;

        let found = client.uid_search_message_id(&message_id).await?;
        let Some(&new_uid) = found.last() else {
            logger::warn(
                "The chat marks were appended but could not be found again by Message-ID; \
                 leaving the previous copy in place.",
            );
            return Ok(());
        };
        crate::chat::marks::uploaded(&self.db.conn, revision, &message_id, &marks)?;

        let doomed: Vec<u32> = supersedes
            .iter()
            .copied()
            .filter(|uid| *uid != new_uid)
            .collect();
        if !doomed.is_empty() {
            client.delete_messages(&doomed).await?;
            report.expunged += doomed.len();
        }
        Ok(())
    }

    async fn upload_today_cleared(
        &mut self,
        client: &mut ImapClient,
        supersedes: &[u32],
        now: DateTime<Utc>,
        report: &mut TaskSyncReport,
    ) -> Result<()> {
        let (cleared_at, revision) = {
            let repo = TaskRepository::new(&self.db);
            (
                repo.today_cleared_at()?,
                repo.today_cleared_state()?.revision,
            )
        };
        let Some(cleared_at) = cleared_at else {
            return Ok(());
        };
        let (bytes, message_id) = TaskEmailBuilder::build_today_cleared(cleared_at, revision, now)?;
        client.append_message(&self.folder, &bytes, true).await?;

        let found = client.uid_search_message_id(&message_id).await?;
        let Some(&new_uid) = found.last() else {
            logger::warn(
                "The Today cut-off was appended but could not be found again by Message-ID; \
                 leaving the previous copy in place.",
            );
            return Ok(());
        };
        TaskRepository::new(&self.db).mark_today_cleared_uploaded(revision, &message_id)?;

        let doomed: Vec<u32> = supersedes
            .iter()
            .copied()
            .filter(|uid| *uid != new_uid)
            .collect();
        if !doomed.is_empty() {
            client.delete_messages(&doomed).await?;
            report.expunged += doomed.len();
        }
        Ok(())
    }

    async fn upload_registry(
        &mut self,
        client: &mut ImapClient,
        supersedes: &[u32],
        now: DateTime<Utc>,
        report: &mut TaskSyncReport,
    ) -> Result<()> {
        let (tags, revision) = {
            let repo = TaskRepository::new(&self.db);
            (repo.list_tags()?, repo.tag_registry_revision()?)
        };

        let (bytes, message_id) = TaskEmailBuilder::build_tag_registry(&tags, revision, now)?;
        client.append_message(&self.folder, &bytes, true).await?;

        let found = client.uid_search_message_id(&message_id).await?;
        let Some(&new_uid) = found.last() else {
            logger::warn(
                "The tag registry was appended but could not be found again by Message-ID; \
                 leaving the previous copy in place.",
            );
            return Ok(());
        };

        TaskRepository::new(&self.db).mark_registry_uploaded(revision, &message_id, &tags)?;

        let doomed: Vec<u32> = supersedes
            .iter()
            .copied()
            .filter(|uid| *uid != new_uid)
            .collect();
        if !doomed.is_empty() {
            client.delete_messages(&doomed).await?;
            report.expunged += doomed.len();
        }

        Ok(())
    }

    /// Record where each attachment payload lives on the server.
    ///
    /// This is how a second machine learns that a payload exists at all: the
    /// task message lists the descriptor, the listing says which message holds
    /// the bytes. `is_cached` is passed through unchanged — it describes this
    /// machine's disposable cache and has nothing to do with the server.
    fn bind_attachments(
        &self,
        listed: &[ListedAttachment],
        local: &[TaskIndexRow],
        report: &mut TaskSyncReport,
    ) {
        if listed.is_empty() {
            return;
        }

        let repo = TaskRepository::new(&self.db);
        let task_ids: HashMap<&str, i64> = local
            .iter()
            .map(|row| (row.task_uuid.as_str(), row.id))
            .collect();

        let mut by_task: HashMap<i64, Vec<&ListedAttachment>> = HashMap::new();
        for attachment in listed {
            if let Some(task_id) = task_ids.get(attachment.task_uuid.as_str()) {
                by_task.entry(*task_id).or_default().push(attachment);
            }
        }

        for (task_id, listed) in by_task {
            let known = match repo.get_attachments(task_id) {
                Ok(known) => known,
                Err(error) => {
                    report
                        .errors
                        .push(format!("reading attachments of task {task_id}: {error}"));
                    continue;
                }
            };
            for attachment in known {
                let Some(found) = listed
                    .iter()
                    .find(|candidate| candidate.attachment_uuid == attachment.attachment_uuid)
                else {
                    continue;
                };
                if attachment.imap_uid == Some(found.uid)
                    && attachment.message_id == found.message_id
                {
                    continue;
                }
                if let Err(error) = repo.update_attachment_storage(
                    &attachment.attachment_uuid,
                    Some(found.uid),
                    found.message_id.as_deref(),
                    attachment.is_cached,
                ) {
                    report.errors.push(format!(
                        "recording the location of attachment '{}': {error}",
                        attachment.filename
                    ));
                }
            }
        }
    }

    /// Fetch an attachment payload into the local cache and return its path.
    ///
    /// Lazy on purpose: a sync must not drag ten megabytes down because a task
    /// mentions a file. The cache is disposable — if the file is missing it is
    /// simply fetched again, which is also what makes deleting the whole cache
    /// a safe thing to do.
    pub async fn download_attachment(
        &mut self,
        client: &mut ImapClient,
        task_uuid: &str,
        attachment: &TaskAttachment,
    ) -> Result<PathBuf> {
        let path = cached_attachment_path(&attachment.file_path)?;
        if attachment.is_cached && path.exists() {
            return Ok(path);
        }

        client.select_mailbox(&self.folder).await?;

        // The UID is the fast way in; the Message-ID is the one that still works
        // after a UIDVALIDITY bump.
        let mut candidates: Vec<u32> = attachment.imap_uid.into_iter().collect();
        if candidates.is_empty()
            && let Some(message_id) = attachment.message_id.as_deref()
        {
            candidates = client.uid_search_message_id(message_id).await?;
        }
        let Some(&uid) = candidates.last() else {
            return Err(crate::Error::NotFound(format!(
                "attachment '{}' has no message on the server yet",
                attachment.filename
            )));
        };

        let bodies = client.fetch_bodies(&[uid]).await?;
        let Some((_, bytes)) = bodies.first() else {
            return Err(crate::Error::NotFound(format!(
                "the message holding attachment '{}' (UID {uid}) is gone",
                attachment.filename
            )));
        };

        let parsed = match TaskEmailParser::parse(bytes, &self.account_name, Some(uid))? {
            TaskMessage::Attachment(parsed) => parsed,
            other => {
                return Err(crate::Error::InvalidInput(format!(
                    "UID {uid} is a {} message, not an attachment payload",
                    other.kind().as_str()
                )));
            }
        };
        if parsed.attachment.attachment_uuid != attachment.attachment_uuid {
            return Err(crate::Error::InvalidInput(format!(
                "UID {uid} holds attachment {} but {} was asked for",
                parsed.attachment.attachment_uuid, attachment.attachment_uuid
            )));
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &parsed.payload)?;

        {
            let repo = TaskRepository::new(&self.db);
            repo.update_attachment_storage(
                &attachment.attachment_uuid,
                Some(uid),
                parsed.attachment.message_id.as_deref(),
                true,
            )?;
        }

        logger::debug(&format!(
            "Cached attachment '{}' of task {task_uuid} ({} bytes)",
            attachment.filename,
            parsed.payload.len()
        ));
        Ok(path)
    }
}

/// Which messages a set of purged tasks own, restricted to what this listing
/// actually saw.
///
/// The restriction is the point: a cached UID may name a different message
/// after a `UIDVALIDITY` bump, and expunging by a stale UID destroys somebody
/// else's mail. Everything here was observed in this listing, moments ago.
fn purge_uids(purged: &[PurgedTask], listing: &Listing, listed: &HashSet<u32>) -> Vec<u32> {
    let uuids: HashSet<&str> = purged.iter().map(|task| task.task_uuid.as_str()).collect();

    let mut uids: Vec<u32> = listing
        .messages
        .iter()
        .filter(|message| uuids.contains(message.identity.task_uuid.as_str()))
        .map(|message| message.uid)
        .collect();

    // The cached UIDs as well, for a message the listing did not show — one
    // already flagged `\Deleted`, for instance.
    for task in purged {
        uids.extend(task.imap_uid.filter(|uid| listed.contains(uid)));
        uids.extend(
            task.attachment_uids
                .iter()
                .copied()
                .filter(|uid| listed.contains(uid)),
        );
    }

    uids.sort_unstable();
    uids.dedup();
    uids
}

/// An override for the cache root, for tests and for a relocated cache.
static CACHE_ROOT_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Point the attachment cache somewhere other than the home directory.
///
/// Exists because the default is `~/.orb/attachments`, and a test that exercises
/// the download path would otherwise write into the running user's real cache —
/// a test that leaves files in a home directory is one that cannot be run
/// twice with confidence. Settable once, before the first use; a second call is
/// ignored rather than allowed to move the cache under a running sync.
pub fn set_attachment_cache_root(root: PathBuf) -> bool {
    CACHE_ROOT_OVERRIDE.set(root).is_ok()
}

/// Where attachment payloads are cached.
pub fn attachment_cache_root() -> PathBuf {
    CACHE_ROOT_OVERRIDE
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(shellexpand::tilde(ATTACHMENT_CACHE_ROOT).into_owned()))
}

/// The absolute path of a cached payload, from the relative one stored on the
/// attachment row.
///
/// The relative path was built by [`TaskAttachment::cache_path`] from ids and
/// a filename that came off the wire, and a row written before those were
/// checked may still hold anything. So the path is read back as components,
/// and one that is not plain names under the root — a `..`, an absolute path,
/// an empty one — is refused rather than joined.
pub fn cached_attachment_path(relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    let mut components = path.components().peekable();
    if components.peek().is_none()
        || !components.all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(crate::Error::InvalidInput(format!(
            "attachment path '{relative}' does not stay inside the cache"
        )));
    }
    Ok(attachment_cache_root().join(path))
}

/// The directory a task's payloads are cached under, or `None` for an id that
/// is not an identifier — which no task of Orb's has, and which must not be
/// joined onto the cache root, since `..` would name the root's parent.
fn task_cache_dir(task_uuid: &str) -> Option<PathBuf> {
    is_wire_id(task_uuid).then(|| attachment_cache_root().join("tasks").join(task_uuid))
}

/// Drop a task's cached payloads once the task itself is gone for good.
///
/// Best effort by design: the cache is disposable, so a file that cannot be
/// removed is a wasted megabyte and never a correctness problem.
pub fn remove_cached_attachments(task_uuid: &str) {
    let Some(dir) = task_cache_dir(task_uuid) else {
        logger::warn(&format!(
            "not removing the attachment cache of '{task_uuid}': the id is not an identifier"
        ));
        return;
    };
    if dir.exists()
        && let Err(error) = std::fs::remove_dir_all(&dir)
    {
        logger::debug(&format!(
            "could not remove the attachment cache at {}: {error}",
            dir.display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::rfc822::TAG_REGISTRY_ID;

    /// A sync is spawned onto the runtime, so its future has to be `Send`. It
    /// stops being `Send` the moment a `TaskRepository` — which holds a
    /// `&Database`, and `rusqlite::Connection` is not `Sync` — is held across
    /// an `await`. This never runs; it only has to compile.
    #[allow(dead_code)]
    fn a_sync_can_be_spawned(service: &mut TaskSyncService, client: &mut ImapClient) {
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&service.sync(client));
    }

    /// A cache path is plain names under the root, or it is nothing: a row
    /// that holds `..` must not be joined onto the root and read or written.
    #[test]
    fn a_cache_path_that_leaves_the_cache_is_refused() {
        for relative in ["../../..", "tasks/../../x", "/etc/passwd", "", "./t/a"] {
            assert!(cached_attachment_path(relative).is_err(), "{relative:?}");
        }
        let kept = cached_attachment_path("tasks/task-a/att-1-notes.txt").unwrap();
        assert!(kept.starts_with(attachment_cache_root()));
        assert!(kept.ends_with("tasks/task-a/att-1-notes.txt"));
    }

    /// Removing a task's cache is `remove_dir_all` on a directory named by
    /// the id; an id that is a path names nothing to remove.
    #[test]
    fn a_task_id_that_is_a_path_names_no_cache_directory() {
        assert!(task_cache_dir("../..").is_none());
        assert!(task_cache_dir("..").is_none());
        assert!(task_cache_dir("").is_none());
        assert!(task_cache_dir("a/b").is_none());
        let dir = task_cache_dir("task-a").unwrap();
        assert_eq!(dir, attachment_cache_root().join("tasks").join("task-a"));
    }

    fn identity(kind: TaskMessageKind, uuid: &str, revision: i64) -> TaskIdentity {
        TaskIdentity {
            kind,
            task_uuid: uuid.to_string(),
            revision,
            attachment_uuid: None,
        }
    }

    fn message(uid: u32, uuid: &str, revision: i64) -> ListedMessage {
        ListedMessage {
            uid,
            identity: identity(TaskMessageKind::Task, uuid, revision),
            // One edit, one Message-ID, wherever the message is: a moved
            // message keeps it, which is how a move is told from a new edit.
            message_id: Some(format!("<{uuid}.{revision}@orb>")),
        }
    }

    fn attachment_message(uid: u32, task_uuid: &str, attachment_uuid: &str) -> ListedMessage {
        ListedMessage {
            uid,
            identity: TaskIdentity {
                kind: TaskMessageKind::Attachment,
                task_uuid: task_uuid.to_string(),
                revision: 1,
                attachment_uuid: Some(attachment_uuid.to_string()),
            },
            message_id: Some(format!("<{attachment_uuid}@orb>")),
        }
    }

    fn registry_message(uid: u32, revision: i64) -> ListedMessage {
        ListedMessage {
            uid,
            identity: identity(TaskMessageKind::TagRegistry, TAG_REGISTRY_ID, revision),
            message_id: Some(format!("<registry.{revision}@orb>")),
        }
    }

    fn row(id: i64, uuid: &str, revision: i64, uid: Option<u32>) -> TaskIndexRow {
        TaskIndexRow {
            id,
            task_uuid: uuid.to_string(),
            revision,
            imap_uid: uid,
            message_id: uid.map(|_| format!("<{uuid}.{revision}@orb>")),
            is_dirty: false,
            is_trashed: false,
        }
    }

    // -- folder resolution ---------------------------------------------------

    /// A leading `.` is a Courier convention, not a universal one. Prepending
    /// it on a Dovecot server creates a folder called `.Tasks` beside the real
    /// one, which then holds a second, invisible task list.
    #[test]
    fn a_flat_server_gets_the_name_exactly_as_configured() {
        let existing = vec!["INBOX".to_string(), "Sent".to_string(), "Trash".to_string()];
        assert_eq!(
            plan_tasks_folder("Tasks", Some("/"), &existing),
            FolderPlan {
                name: "Tasks".to_string(),
                create: true
            }
        );
        // Even when the delimiter *is* a dot.
        assert_eq!(
            plan_tasks_folder("Tasks", Some("."), &existing),
            FolderPlan {
                name: "Tasks".to_string(),
                create: true
            }
        );
        // And when the server reports no hierarchy at all.
        assert_eq!(
            plan_tasks_folder("Tasks", None, &existing),
            FolderPlan {
                name: "Tasks".to_string(),
                create: true
            }
        );
    }

    /// On a Maildir++ server every personal mailbox lives under `INBOX.`, and a
    /// top-level `Tasks` would sit outside the namespace where no other client
    /// shows it.
    #[test]
    fn a_namespaced_server_gets_the_prefix_it_uses() {
        let existing = vec![
            "INBOX".to_string(),
            "INBOX.Sent".to_string(),
            "INBOX.Trash".to_string(),
        ];
        assert_eq!(
            plan_tasks_folder("Tasks", Some("."), &existing),
            FolderPlan {
                name: "INBOX.Tasks".to_string(),
                create: true
            }
        );

        // One folder outside the prefix means the server does not require one.
        let mixed = vec![
            "INBOX".to_string(),
            "INBOX.Sent".to_string(),
            "Archive".to_string(),
        ];
        assert_eq!(
            plan_tasks_folder("Tasks", Some("."), &mixed).name,
            "Tasks".to_string()
        );
    }

    #[test]
    fn an_existing_folder_is_used_as_the_server_spells_it() {
        let existing = vec!["INBOX".to_string(), "tasks".to_string()];
        assert_eq!(
            plan_tasks_folder("Tasks", Some("/"), &existing),
            FolderPlan {
                name: "tasks".to_string(),
                create: false
            }
        );

        // Already there under the namespaced name.
        let namespaced = vec![
            "INBOX".to_string(),
            "INBOX.Sent".to_string(),
            "INBOX.Tasks".to_string(),
        ];
        assert_eq!(
            plan_tasks_folder("Tasks", Some("."), &namespaced),
            FolderPlan {
                name: "INBOX.Tasks".to_string(),
                create: false
            }
        );
    }

    /// A config that spells out the whole path has already decided; guessing on
    /// top of it would produce `INBOX.INBOX.Tasks`.
    #[test]
    fn a_name_the_server_keeps_for_itself_is_called_a_clash() {
        let clash = unusable_folder_reason(
            "Tasks",
            "Tasks",
            Some("NO CREATE failed: object with that name already exists"),
            "NO SELECT failed",
        );
        assert!(clash.contains("not offer over IMAP"), "{clash}");
        assert!(clash.contains("tasks.folder"), "{clash}");

        let other = unusable_folder_reason("Tasks", "Tasks", None, "NO permission denied");
        assert!(other.contains("permission denied"), "{other}");
        assert!(!other.contains("not offer over IMAP"), "{other}");
    }

    #[test]
    fn a_qualified_name_is_left_alone() {
        let existing = vec!["INBOX".to_string(), "INBOX.Sent".to_string()];
        assert_eq!(
            plan_tasks_folder("INBOX.Tasks", Some("."), &existing),
            FolderPlan {
                name: "INBOX.Tasks".to_string(),
                create: true
            }
        );
        // An empty server (first run) cannot suggest a prefix either.
        assert_eq!(
            plan_tasks_folder("Tasks", Some("."), &[]),
            FolderPlan {
                name: "Tasks".to_string(),
                create: true
            }
        );
    }

    // -- reconciliation ------------------------------------------------------

    /// The core rule of §3.1. Two messages for one task id — two devices
    /// editing, or an append whose delete never ran — resolve to the highest
    /// revision, and the loser goes only because the winner is right there in
    /// the listing.
    #[test]
    fn one_task_id_in_two_messages_resolves_to_the_highest_revision() {
        let listing = vec![message(10, "task-a", 4), message(11, "task-a", 7)];
        let plan = reconcile(&[], &listing, 0, false);

        assert_eq!(
            plan.download,
            vec![DownloadTarget {
                uid: 11,
                task_uuid: "task-a".to_string(),
                revision: 7
            }]
        );
        assert_eq!(
            plan.expunge,
            vec![ExpungeTarget {
                uid: 10,
                reason: ExpungeReason::Duplicate
            }]
        );
    }

    /// The tie-break exists for the append that was retried after a lost
    /// response: two identical revisions, and the copy written last is the one
    /// to keep.
    #[test]
    fn a_tie_on_revision_is_broken_by_the_highest_uid() {
        let listing = vec![
            message(31, "task-a", 5),
            message(12, "task-a", 5),
            message(20, "task-a", 5),
        ];
        let plan = reconcile(&[], &listing, 0, false);

        assert_eq!(plan.download.len(), 1);
        assert_eq!(plan.download[0].uid, 31);
        assert_eq!(
            plan.expunge
                .iter()
                .map(|target| target.uid)
                .collect::<Vec<_>>(),
            vec![12, 20]
        );
    }

    /// The rule the old "server wins" broke: a server copy at a lower revision
    /// must never overwrite the local task. It is the local task that goes up,
    /// and nothing is expunged until the new message is confirmed present —
    /// which is why the losers ride along on the upload rather than being
    /// planned as expunges here.
    #[test]
    fn a_lower_revision_on_the_server_never_overwrites_the_local_task() {
        let local = vec![row(1, "task-a", 9, Some(10))];
        let listing = vec![message(10, "task-a", 4), message(11, "task-a", 6)];
        let plan = reconcile(&local, &listing, 0, false);

        assert!(
            plan.download.is_empty(),
            "the older copy must not be applied"
        );
        assert!(
            plan.expunge.is_empty(),
            "nothing goes before the upload lands"
        );
        assert_eq!(
            plan.upload,
            vec![UploadTarget {
                task_id: 1,
                task_uuid: "task-a".to_string(),
                reason: UploadReason::LocalAhead,
                supersedes: vec![10, 11],
                // Not this machine's copy, so its log is taken in first.
                merge_log_from: Some(11),
            }]
        );
    }

    /// A higher revision on the server wins even against a dirty local task —
    /// last-writer-wins by revision is the stated rule — and the local device's
    /// history survives anyway, because the log is union-merged when the
    /// download is applied.
    #[test]
    fn a_higher_revision_on_the_server_is_applied() {
        let mut dirty = row(1, "task-a", 4, Some(10));
        dirty.is_dirty = true;
        let listing = vec![message(11, "task-a", 5)];

        let plan = reconcile(&[dirty], &listing, 0, false);
        assert_eq!(plan.download.len(), 1);
        assert!(plan.upload.is_empty());
    }

    /// Equal revisions with local changes still has to go up: not every edit
    /// that marks a task dirty bumps the revision on its own, and a dirty task
    /// left alone is an edit that never reaches the server.
    #[test]
    fn a_dirty_task_at_the_same_revision_is_uploaded() {
        let mut dirty = row(1, "task-a", 5, Some(10));
        dirty.is_dirty = true;
        let listing = vec![message(10, "task-a", 5)];

        let plan = reconcile(&[dirty], &listing, 0, false);
        assert_eq!(plan.upload.len(), 1);
        assert_eq!(plan.upload[0].reason, UploadReason::LocalEdit);
        assert_eq!(plan.upload[0].supersedes, vec![10]);
        assert!(plan.download.is_empty());
    }

    /// §14.4, both halves of it. A crash between the APPEND and the delete
    /// leaves two messages for one task; where exactly it crashed decides
    /// whether the local copy is already recorded as living in the new one.
    /// Either way the next sync converges on a single message and never on a
    /// gap.
    #[test]
    fn a_crash_between_append_and_delete_converges_on_one_message() {
        // Crashed after the compare-and-swap: the task knows it lives in UID
        // 11, and the copy it replaced is simply expunged.
        let recorded = vec![row(1, "task-a", 5, Some(11))];
        let listing = vec![message(10, "task-a", 4), message(11, "task-a", 5)];
        let plan = reconcile(&recorded, &listing, 0, false);

        assert!(plan.download.is_empty(), "nothing moved");
        assert!(plan.upload.is_empty());
        assert_eq!(
            plan.expunge,
            vec![ExpungeTarget {
                uid: 10,
                reason: ExpungeReason::Duplicate
            }]
        );

        // Crashed before it: the task is still dirty and still points at the
        // message it was replacing, so it goes up again — and both of the
        // copies now on the server are superseded by the one that lands.
        let mut unrecorded = row(1, "task-a", 5, Some(10));
        unrecorded.is_dirty = true;
        let plan = reconcile(&[unrecorded], &listing, 0, false);

        assert!(
            plan.expunge.is_empty(),
            "nothing may go before the replacement is confirmed present"
        );
        assert_eq!(plan.upload[0].supersedes, vec![10, 11]);
    }

    /// The other half of §3.3 step 4: a task id that is not in the listing is
    /// gone from the folder, whether Orb expunged it or something else did.
    #[test]
    fn a_task_missing_from_the_listing_is_deleted_locally() {
        let local = vec![row(1, "task-a", 3, Some(10)), row(2, "task-b", 1, Some(11))];
        let listing = vec![message(11, "task-b", 1)];

        let plan = reconcile(&local, &listing, 0, false);
        assert_eq!(
            plan.delete_local,
            vec![LocalDeletion {
                task_id: 1,
                task_uuid: "task-a".to_string()
            }]
        );
    }

    /// A task created offline has no UID and no Message-ID, so "absent from the
    /// server" says nothing about it having been deleted there. Deleting it
    /// would throw away a task the moment the machine reconnected.
    #[test]
    fn a_task_that_was_never_uploaded_is_uploaded_not_deleted() {
        let mut fresh = row(1, "task-a", 1, None);
        fresh.is_dirty = true;
        fresh.message_id = None;

        let plan = reconcile(&[fresh], &[], 0, false);
        assert!(plan.delete_local.is_empty());
        assert_eq!(plan.upload.len(), 1);
        assert_eq!(plan.upload[0].reason, UploadReason::NeverUploaded);
    }

    /// A task with unsent changes that the server no longer has: the deletion
    /// can be repeated, the edit cannot be recovered, so the edit wins.
    #[test]
    fn a_dirty_task_missing_from_the_server_is_restored_not_dropped() {
        let mut dirty = row(1, "task-a", 3, Some(10));
        dirty.is_dirty = true;

        let plan = reconcile(&[dirty], &[], 0, false);
        assert!(plan.delete_local.is_empty());
        assert_eq!(plan.upload[0].reason, UploadReason::MissingOnServer);
    }

    /// §3.4. A `UIDVALIDITY` bump means the UIDs stopped meaning anything, not
    /// that the tasks are gone: the folder is reconciled by task id, and
    /// nothing at all is deleted locally on that path.
    #[test]
    fn a_uidvalidity_change_never_deletes_a_task() {
        let local = vec![
            row(1, "task-a", 3, Some(10)),
            row(2, "task-b", 2, Some(11)),
            row(3, "task-c", 1, Some(12)),
        ];
        // The folder came back with new UIDs, and one task missing entirely.
        let listing = vec![message(900, "task-a", 3), message(901, "task-b", 2)];

        let plan = reconcile(&local, &listing, 0, true);

        assert!(
            plan.delete_local.is_empty(),
            "a UIDVALIDITY bump is not a deletion"
        );
        assert_eq!(plan.upload.len(), 1, "the missing task is restored");
        assert_eq!(plan.upload[0].task_uuid, "task-c");
        assert_eq!(plan.upload[0].reason, UploadReason::UidValidityReset);

        // The two that are still there only need their cached location repaired.
        let rebound: Vec<u32> = plan.rebind.iter().map(|rebind| rebind.uid).collect();
        assert_eq!(rebound, vec![900, 901]);
        assert!(
            plan.download.is_empty(),
            "nothing moved, so nothing is fetched"
        );
    }

    /// Without a bump, a task whose UID merely moved is still in step: the
    /// cached location is repaired and no data is touched.
    #[test]
    fn a_moved_uid_is_rebound_rather_than_re_downloaded() {
        let local = vec![row(1, "task-a", 3, Some(10))];
        let listing = vec![message(77, "task-a", 3)];

        let plan = reconcile(&local, &listing, 0, false);
        assert!(plan.download.is_empty());
        assert!(plan.upload.is_empty());
        assert_eq!(plan.rebind.len(), 1);
        assert_eq!(plan.rebind[0].uid, 77);
        assert_eq!(plan.rebind[0].revision, 3, "the revision is not changed");
    }

    /// Two machines edited the task from one revision; the other one
    /// uploaded last. Its copy has the same revision under another
    /// `Message-ID`, and it is downloaded rather than taken for this
    /// machine's own — or both would keep their own version for good.
    #[test]
    fn another_machines_edit_at_the_same_revision_is_downloaded() {
        let local = vec![row(1, "task-a", 4, Some(10))];
        let mut theirs = message(12, "task-a", 4);
        theirs.message_id = Some("<task-a.4.from-the-other-machine@orb>".into());

        let plan = reconcile(&local, &[theirs], 0, false);
        assert_eq!(plan.download.len(), 1, "{plan:?}");
        assert_eq!(plan.download[0].uid, 12);
        assert!(plan.rebind.is_empty());
        assert!(plan.upload.is_empty());
    }

    /// A row from before Message-IDs were recorded is taken to be in step, as
    /// it always was.
    #[test]
    fn a_row_without_a_message_id_is_rebound_as_before() {
        let mut local = row(1, "task-a", 4, Some(10));
        local.message_id = None;
        let plan = reconcile(&[local], &[message(12, "task-a", 4)], 0, false);
        assert!(plan.download.is_empty());
        assert_eq!(plan.rebind.len(), 1);
    }

    /// The tag registry by the same rules: a tag changed here and not yet
    /// sent goes up even at the server's revision, and another machine's
    /// registry at this machine's revision comes down.
    #[test]
    fn the_registry_sends_unsent_changes_and_takes_another_machines() {
        let server = registry_message(30, 5);
        let ours = server.message_id.clone();

        let dirty = RegistryState {
            revision: 5,
            dirty: true,
            message_id: ours.clone(),
        };
        let plan = reconcile(&[], std::slice::from_ref(&server), dirty, false);
        assert_eq!(
            plan.registry,
            RegistryAction::Upload {
                supersedes: vec![30]
            }
        );

        let clean_same = RegistryState {
            revision: 5,
            dirty: false,
            message_id: ours,
        };
        let plan = reconcile(&[], std::slice::from_ref(&server), clean_same, false);
        assert_eq!(plan.registry, RegistryAction::Idle);

        let clean_other = RegistryState {
            revision: 5,
            dirty: false,
            message_id: Some("<registry.5.mine@orb>".into()),
        };
        let plan = reconcile(&[], std::slice::from_ref(&server), clean_other, false);
        assert!(
            matches!(
                plan.registry,
                RegistryAction::Download(DownloadTarget { uid: 30, .. })
            ),
            "{:?}",
            plan.registry
        );
    }

    fn tag(uuid: &str, name: &str) -> TaskTag {
        TaskTag {
            id: None,
            tag_uuid: uuid.into(),
            name: name.into(),
            colour: None,
            position: 0.0,
        }
    }

    fn names(tags: &[TaskTag]) -> Vec<String> {
        let mut names: Vec<String> = tags.iter().map(|t| t.name.clone()).collect();
        names.sort();
        names
    }

    /// §7: each side's creations, renames and deletions survive a merge, and a
    /// tag both sides changed is taken from the side that uploads.
    #[test]
    fn two_registries_merge_tag_by_tag_against_the_shared_base() {
        let base = vec![tag("e", "Errands"), tag("w", "Work"), tag("h", "Home")];
        // Here: created "Mine", deleted "Home", renamed "Work".
        let local = vec![tag("e", "Errands"), tag("w", "Office"), tag("m", "Mine")];
        // There: created "Theirs", deleted "Errands", renamed "Work" too.
        let remote = vec![tag("w", "Job"), tag("h", "Home"), tag("t", "Theirs")];

        let merged = merge_tag_registries(Some(&base), &local, &remote);
        assert_eq!(names(&merged), ["Mine", "Office", "Theirs"]);
    }

    /// A deletion on one side does not remove a tag the other side changed.
    #[test]
    fn a_tag_changed_on_one_side_survives_its_deletion_on_the_other() {
        let base = vec![tag("e", "Errands")];
        let merged = merge_tag_registries(Some(&base), &[tag("e", "Chores")], &[]);
        assert_eq!(names(&merged), ["Chores"]);
    }

    /// Without a recorded base, nothing either side has is dropped.
    #[test]
    fn without_a_base_every_tag_is_kept() {
        let merged = merge_tag_registries(None, &[tag("a", "A")], &[tag("b", "B")]);
        assert_eq!(names(&merged), ["A", "B"]);
    }

    /// Changed here and another machine's copy on the server: merge, then
    /// upload over every registry copy.
    #[test]
    fn a_registry_changed_on_both_sides_is_merged() {
        let server = registry_message(30, 6);
        let local = RegistryState {
            revision: 5,
            dirty: true,
            message_id: Some("<registry.5@orb>".into()),
        };
        let plan = reconcile(&[], std::slice::from_ref(&server), local, false);
        assert!(
            matches!(
                &plan.registry,
                RegistryAction::Merge { from, supersedes } if from.uid == 30 && supersedes == &vec![30]
            ),
            "{:?}",
            plan.registry
        );
    }

    /// The ordinary case, and the one that has to stay cheap: everything agrees
    /// and the sync does nothing.
    #[test]
    fn a_folder_in_step_produces_no_work() {
        let local = vec![row(1, "task-a", 3, Some(10)), row(2, "task-b", 1, Some(11))];
        let listing = vec![message(10, "task-a", 3), message(11, "task-b", 1)];

        let plan = reconcile(&local, &listing, 0, false);
        assert!(plan.is_empty(), "{plan:?}");
    }

    /// §9: a payload whose task exists nowhere is unreachable by construction.
    /// One whose task is only known locally is not — it is a task that has not
    /// been uploaded yet, and expunging its payload would lose the file.
    #[test]
    fn an_attachment_is_collected_only_when_its_task_is_gone_from_both_sides() {
        let local = vec![row(1, "task-a", 1, Some(10))];
        let listing = vec![
            message(10, "task-a", 1),
            attachment_message(20, "task-a", "att-1"),
            attachment_message(21, "task-gone", "att-2"),
            attachment_message(22, "task-local-only", "att-3"),
        ];
        let mut local_only = row(2, "task-local-only", 1, None);
        local_only.is_dirty = true;
        local_only.message_id = None;
        let local = [local, vec![local_only]].concat();

        let plan = reconcile(&local, &listing, 0, false);

        assert_eq!(
            plan.expunge,
            vec![ExpungeTarget {
                uid: 21,
                reason: ExpungeReason::Orphan
            }]
        );
        assert_eq!(
            plan.attachments
                .iter()
                .map(|attachment| attachment.uid)
                .collect::<Vec<_>>(),
            vec![20, 22]
        );
    }

    /// Two copies of one payload: the first written is the payload, whatever
    /// revision a later one claims, and nothing is expunged on the strength
    /// of a header a payload message does not write.
    #[test]
    fn a_duplicated_attachment_payload_keeps_the_first_copy_and_expunges_none() {
        let local = vec![row(1, "task-a", 1, Some(10))];
        let mut later = attachment_message(25, "task-a", "att-1");
        later.identity.revision = 9;
        let listing = vec![
            message(10, "task-a", 1),
            later,
            attachment_message(20, "task-a", "att-1"),
        ];

        let plan = reconcile(&local, &listing, 0, false);
        assert!(plan.expunge.is_empty(), "{:?}", plan.expunge);
        assert_eq!(plan.attachments.len(), 1);
        assert_eq!(plan.attachments[0].uid, 20);
    }

    /// The registry syncs by the task rules under a well-known id, so it needs
    /// no special case beyond deciding which side is newer.
    #[test]
    fn the_tag_registry_follows_the_same_revision_rule() {
        // The server is ahead: fetch it.
        let plan = reconcile(&[], &[registry_message(5, 3)], 2, false);
        assert!(matches!(
            plan.registry,
            RegistryAction::Download(DownloadTarget { uid: 5, .. })
        ));

        // This machine is ahead: write it, superseding what is there.
        let plan = reconcile(&[], &[registry_message(5, 1)], 4, false);
        assert_eq!(
            plan.registry,
            RegistryAction::Upload {
                supersedes: vec![5]
            }
        );

        // Nobody has one yet.
        assert_eq!(reconcile(&[], &[], 0, false).registry, RegistryAction::Idle);

        // Local tags but no registry message: this is the first machine.
        assert_eq!(
            reconcile(&[], &[], 2, false).registry,
            RegistryAction::Upload {
                supersedes: Vec::new()
            }
        );

        // Both agree; a duplicate registry message is still resolved.
        let plan = reconcile(
            &[],
            &[registry_message(5, 3), registry_message(9, 3)],
            3,
            false,
        );
        assert_eq!(plan.registry, RegistryAction::Idle);
        assert_eq!(
            plan.expunge,
            vec![ExpungeTarget {
                uid: 5,
                reason: ExpungeReason::Duplicate
            }]
        );
    }

    /// A message this build cannot identify is counted and then left completely
    /// alone. Deleting it would destroy data written by a version that knows
    /// more than this one.
    #[test]
    fn an_unidentifiable_message_is_never_touched() {
        let entries = vec![
            HeaderListing {
                uid: 1,
                flags: Vec::new(),
                headers: b"X-Orb-Task-Version: 2\r\nX-Orb-Task-Type: task\r\n\
                           X-Orb-Task-Id: task-a\r\nX-Orb-Task-Revision: 3\r\n\
                           Message-ID: <a@orb>\r\n"
                    .to_vec(),
            },
            // A wire version from the future.
            HeaderListing {
                uid: 2,
                flags: Vec::new(),
                headers: b"X-Orb-Task-Version: 99\r\nX-Orb-Task-Id: task-b\r\n".to_vec(),
            },
            // Not a task message at all.
            HeaderListing {
                uid: 3,
                flags: Vec::new(),
                headers: b"Subject: hello\r\n".to_vec(),
            },
        ];

        let listing = read_listing(&entries);
        assert_eq!(listing.messages.len(), 1);
        assert_eq!(listing.messages[0].identity.task_uuid, "task-a");
        assert_eq!(listing.messages[0].identity.revision, 3);
        assert_eq!(listing.messages[0].message_id.as_deref(), Some("<a@orb>"));
        assert_eq!(listing.unreadable, vec![2, 3]);

        let plan = reconcile(&[], &listing.messages, 0, false);
        assert_eq!(plan.download.len(), 1);
        assert!(plan.expunge.is_empty());
    }

    /// A `\Deleted` message is on its way out. Counting it as live is how a
    /// deleted task comes back: the row is gone, the message is still listed,
    /// and the next sync downloads it as something it has never seen.
    #[test]
    fn a_message_flagged_deleted_is_not_a_live_task() {
        let entries = vec![HeaderListing {
            uid: 7,
            flags: vec!["\\Seen".to_string(), "\\Deleted".to_string()],
            headers: b"X-Orb-Task-Version: 2\r\nX-Orb-Task-Type: task\r\n\
                       X-Orb-Task-Id: task-a\r\nX-Orb-Task-Revision: 1\r\n"
                .to_vec(),
        }];

        let listing = read_listing(&entries);
        assert!(listing.messages.is_empty());
        assert_eq!(listing.deleted, vec![7]);

        // And so the local task follows it.
        let plan = reconcile(&[row(1, "task-a", 1, Some(7))], &listing.messages, 0, false);
        assert_eq!(plan.delete_local.len(), 1);
    }

    /// A purge has to take the task's messages with it: a row deleted while its
    /// message stays behind is a task that comes back on the next listing.
    #[test]
    fn purging_a_task_expunges_its_message_and_its_payloads() {
        let listing = Listing {
            messages: vec![
                message(10, "task-a", 3),
                attachment_message(11, "task-a", "att-1"),
                message(12, "task-b", 1),
            ],
            unreadable: Vec::new(),
            deleted: Vec::new(),
        };
        let listed: HashSet<u32> = [10, 11, 12, 13].into_iter().collect();

        let purged = vec![PurgedTask {
            id: 1,
            task_uuid: "task-a".to_string(),
            // A cached UID the listing did not show — already flagged deleted,
            // for instance — is included as well.
            imap_uid: Some(13),
            message_id: None,
            attachment_uids: vec![11],
        }];

        assert_eq!(purge_uids(&purged, &listing, &listed), vec![10, 11, 13]);
    }

    /// Expunging by a UID that was not in this listing is how another client's
    /// mail gets destroyed: after a UIDVALIDITY bump the same number names a
    /// different message.
    #[test]
    fn a_stale_uid_is_never_expunged() {
        let listing = Listing::default();
        let listed: HashSet<u32> = HashSet::new();
        let purged = vec![PurgedTask {
            id: 1,
            task_uuid: "task-a".to_string(),
            imap_uid: Some(4242),
            message_id: None,
            attachment_uids: vec![4243],
        }];

        assert!(purge_uids(&purged, &listing, &listed).is_empty());
    }

    #[test]
    fn the_report_adds_up() {
        let mut first = TaskSyncReport {
            downloaded: 2,
            uploaded: 1,
            expunged: 3,
            ..Default::default()
        };
        first.merge(TaskSyncReport {
            uploaded: 2,
            deleted: 1,
            uidvalidity_reset: true,
            errors: vec!["nope".to_string()],
            ..Default::default()
        });

        assert_eq!(first.downloaded, 2);
        assert_eq!(first.uploaded, 3);
        assert_eq!(first.deleted, 1);
        assert_eq!(first.expunged, 3);
        assert!(first.uidvalidity_reset);
        assert!(first.has_errors());
        assert!(!first.is_empty());

        // A sync that only listed the folder did nothing.
        let listing_only = TaskSyncReport {
            listed: 40,
            ..Default::default()
        };
        assert!(listing_only.is_empty());
    }

    /// The listing is a plain `1:*` fetch of a fixed set of headers; keeping
    /// that set beside the parser is what stops the two from drifting apart.
    #[test]
    fn the_listing_asks_for_the_headers_it_parses() {
        let fields = listing_fields();
        for header in IDENTITY_HEADERS {
            assert!(
                fields.contains(&header),
                "{header} is parsed but not fetched"
            );
        }
        assert!(fields.contains(&"Message-ID"));
    }
}
