//! The task export (todo-spec §20): every task, written to a directory as a
//! snapshot that Orb reads back (§21) and any other tool can read without it.
//!
//! This module writes a snapshot from data already read and from payloads
//! handed over by the caller, so it knows nothing of IMAP or of the database;
//! `OrbHandle::export_tasks` gathers both.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::model::{
    CachedEmailMetadata, Task, TaskAttachment, TaskDates, TaskEvent, TaskLinkedEmail, TaskLogEntry,
    TaskReminder, TaskStep, TaskTag, format_timestamp, is_wire_id, parse_civil_date,
};
use crate::{Error, Result};

/// `format` in every `tasks.json`.
pub const FORMAT: &str = "orb-tasks";
/// The version this build writes, and the newest it reads.
pub const FORMAT_VERSION: u32 = 1;
/// Every snapshot's directory name starts with this.
const PREFIX: &str = "orb-tasks-";
const PARTIAL: &str = ".partial";

// ---------------------------------------------------------------------------
// The format. Field order is the order on disk, so two snapshots diff cleanly.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format: String,
    pub format_version: u32,
    pub exported_at: String,
    pub orb_version: String,
    pub device_id: Option<String>,
    pub account: String,
    pub folder: String,
    /// False when a payload could not be fetched; see each attachment's
    /// `missing`.
    pub complete: bool,
    pub tags: Vec<ExportTag>,
    pub tasks: Vec<ExportTask>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportTag {
    pub id: String,
    pub name: String,
    pub colour: Option<String>,
    pub position: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportTask {
    pub id: String,
    pub revision: i64,
    pub title: String,
    pub body: Option<String>,
    pub tags: Vec<String>,
    pub postpone_date: Option<String>,
    pub due_date: Option<String>,
    pub reminders: Vec<ExportReminder>,
    pub recurrence: Option<ExportRecurrence>,
    pub position: f64,
    pub steps: Vec<ExportStep>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub deleted_at: Option<String>,
    pub links: Vec<ExportLink>,
    pub attachments: Vec<ExportAttachment>,
    pub log: Vec<ExportLogEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportReminder {
    pub remind_at: String,
    pub fired: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportRecurrence {
    pub rule: String,
    pub from_completion: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportStep {
    pub id: String,
    pub title: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportLink {
    /// `mail`, or `chat` for a chat message (todo-spec §28). Absent in a
    /// snapshot from before chat links, where every link is mail.
    #[serde(default = "mail_kind")]
    pub kind: String,
    pub message_id: String,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub date: Option<String>,
    pub account: Option<String>,
    pub folder: Option<String>,
    /// A chat link's parts, spelled out for readers other than Orb; the id
    /// carries them all, and is what an import reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<ExportChatLink>,
}

fn mail_kind() -> String {
    "mail".to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportChatLink {
    pub server: String,
    pub room_id: String,
    pub message_id: String,
    pub thread_id: Option<String>,
    pub room: Option<String>,
    pub permalink: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportAttachment {
    pub id: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    pub sha256: Option<String>,
    /// Inside the snapshot, or `None` when the payload is `missing`.
    pub path: Option<String>,
    pub missing: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportLogEntry {
    pub id: String,
    pub at: String,
    pub device: Option<String>,
    pub event: String,
    pub detail: Option<serde_json::Value>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

// ---------------------------------------------------------------------------
// What the caller hands in, and what comes back.
// ---------------------------------------------------------------------------

/// One task with everything that travels with it.
pub struct SourceTask {
    pub task: Task,
    pub reminders: Vec<TaskReminder>,
    pub links: Vec<TaskLinkedEmail>,
    pub attachments: Vec<TaskAttachment>,
}

/// Everything a snapshot holds, read in one transaction.
pub struct ExportSource {
    pub device_id: Option<String>,
    pub account: String,
    pub folder: String,
    pub tags: Vec<TaskTag>,
    pub tasks: Vec<SourceTask>,
}

/// How a snapshot turned out.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportOutcome {
    pub path: PathBuf,
    pub complete: bool,
    pub tasks: usize,
    pub attachments: usize,
    /// Payloads that could not be fetched, with why.
    pub missing: Vec<String>,
    /// The snapshot's size on disk, hard-linked payloads included.
    pub bytes: u64,
    /// Older snapshots retention removed.
    pub removed: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// Writing one.
// ---------------------------------------------------------------------------

/// Write a snapshot into `directory` and apply retention.
///
/// `payload` hands over the file holding an attachment's payload — from the
/// cache, or fetched — or says why it cannot. Nothing is visible under the
/// final name until the snapshot is complete on disk and has been read back.
pub fn write_snapshot(
    directory: &Path,
    source: &ExportSource,
    now: DateTime<Utc>,
    hostname: &str,
    keep: usize,
    mut payload: impl FnMut(&Task, &TaskAttachment) -> std::result::Result<PathBuf, String>,
) -> Result<ExportOutcome> {
    create_private_dir(directory)?;
    remove_partials(directory)?;

    let host = sanitise_host(hostname);
    // Two exports within a second would share a name: the later one takes
    // the next second that is free.
    let mut stamped = now;
    let name = loop {
        let name = format!("{PREFIX}{}-{host}", stamped.format("%Y%m%dT%H%M%SZ"));
        if !directory.join(&name).exists() {
            break name;
        }
        stamped += chrono::Duration::seconds(1);
    };
    let target = directory.join(&name);
    let partial = directory.join(format!("{name}{PARTIAL}"));
    create_private_dir(&partial)?;
    let previous = newest_complete(directory)?;

    let mut missing = Vec::new();
    let mut attachment_count = 0;
    let mut tasks: Vec<&SourceTask> = source.tasks.iter().collect();
    tasks.sort_by(|a, b| {
        a.task
            .position
            .total_cmp(&b.task.position)
            .then_with(|| a.task.task_uuid.cmp(&b.task.task_uuid))
    });

    let mut exported = Vec::with_capacity(tasks.len());
    for source_task in tasks {
        let mut attachments = Vec::new();
        for attachment in &source_task.attachments {
            attachment_count += 1;
            let relative = format!(
                "attachments/{}/{}",
                attachment.attachment_uuid,
                safe_filename(&attachment.filename)
            );
            let destination = partial.join(&relative);
            // The id came off the wire; one that is a path would place the
            // payload outside the snapshot, so it is a missing payload instead.
            let placed = if is_wire_id(&attachment.attachment_uuid) {
                place_payload(
                    previous.as_deref().map(|p| p.join(&relative)),
                    &destination,
                    || payload(&source_task.task, attachment),
                )
            } else {
                Err("the attachment's id is not an identifier".to_string())
            };
            attachments.push(match placed {
                Ok(()) => ExportAttachment {
                    id: attachment.attachment_uuid.clone(),
                    filename: attachment.filename.clone(),
                    content_type: attachment.content_type.clone(),
                    size: attachment.size,
                    sha256: Some(sha256_file(&destination)?),
                    path: Some(relative),
                    missing: None,
                },
                Err(why) => {
                    missing.push(format!(
                        "{} ({}): {why}",
                        attachment.filename, source_task.task.subject
                    ));
                    ExportAttachment {
                        id: attachment.attachment_uuid.clone(),
                        filename: attachment.filename.clone(),
                        content_type: attachment.content_type.clone(),
                        size: attachment.size,
                        sha256: None,
                        path: None,
                        missing: Some(why),
                    }
                }
            });
        }
        exported.push(export_task(source_task, attachments));
    }

    let mut tags: Vec<ExportTag> = source
        .tags
        .iter()
        .map(|tag| ExportTag {
            id: tag.tag_uuid.clone(),
            name: tag.name.clone(),
            colour: tag.colour.clone(),
            position: tag.position,
        })
        .collect();
    tags.sort_by(|a, b| {
        a.position
            .total_cmp(&b.position)
            .then_with(|| a.id.cmp(&b.id))
    });

    let snapshot = Snapshot {
        format: FORMAT.to_string(),
        format_version: FORMAT_VERSION,
        exported_at: format_timestamp(now),
        orb_version: env!("CARGO_PKG_VERSION").to_string(),
        device_id: source.device_id.clone(),
        account: source.account.clone(),
        folder: source.folder.clone(),
        complete: missing.is_empty(),
        tags,
        tasks: exported,
    };
    write_private(&partial.join("tasks.json"), &pretty(&snapshot)?)?;
    write_private(&partial.join("schema.json"), SCHEMA.as_bytes())?;
    write_private(&partial.join("README.txt"), readme(&snapshot).as_bytes())?;
    let manifest = build_manifest(&partial)?;
    write_private(&partial.join("manifest.json"), &pretty(&manifest)?)?;
    sync_dir(&partial)?;

    // Read back before the rename: a snapshot that does not check is no
    // snapshot, and must not displace one that does.
    verify(&partial)?;
    fs::rename(&partial, &target)?;
    sync_dir(directory)?;

    let removed = if snapshot.complete {
        apply_retention(directory, &host, keep.max(1), &target)?
    } else {
        Vec::new()
    };
    Ok(ExportOutcome {
        bytes: dir_size(&target)?,
        path: target,
        complete: snapshot.complete,
        tasks: snapshot.tasks.len(),
        attachments: attachment_count,
        missing,
        removed,
    })
}

fn export_task(source: &SourceTask, attachments: Vec<ExportAttachment>) -> ExportTask {
    let task = &source.task;
    let mut log: Vec<ExportLogEntry> = task
        .log
        .iter()
        .map(|entry| ExportLogEntry {
            id: entry.entry_uuid.clone(),
            at: entry.occurred_at.clone(),
            device: entry.device_id.clone(),
            event: entry.event_type().to_string(),
            detail: entry
                .detail_json()
                .and_then(|json| serde_json::from_str(&json).ok()),
            text: entry.rendered_text(),
        })
        .collect();
    log.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    let mut reminders: Vec<ExportReminder> = source
        .reminders
        .iter()
        .map(|r| ExportReminder {
            remind_at: r.remind_at.clone(),
            fired: r.is_triggered,
        })
        .collect();
    reminders.sort_by(|a, b| a.remind_at.cmp(&b.remind_at));
    let mut tags = task.tags.clone();
    tags.sort();
    ExportTask {
        id: task.task_uuid.clone(),
        revision: task.revision,
        title: task.subject.clone(),
        body: task.body_text.clone(),
        tags,
        postpone_date: task.postpone_date.clone(),
        due_date: task.due_date.clone(),
        reminders,
        recurrence: task.recurrence.as_ref().map(|rule| ExportRecurrence {
            rule: rule.clone(),
            from_completion: task.recurrence_from_completion,
        }),
        position: task.position,
        steps: task
            .steps
            .iter()
            .map(|s| ExportStep {
                id: s.id.clone(),
                title: s.title.clone(),
                done: s.done,
            })
            .collect(),
        created_at: task.created_at.clone(),
        updated_at: task.updated_at.clone(),
        completed_at: task.completed_at.clone(),
        deleted_at: task.deleted_at.clone(),
        links: source
            .links
            .iter()
            .map(|l| {
                let chat = crate::chat::ChatLink::parse(&l.message_id);
                ExportLink {
                    kind: if chat.is_some() { "chat" } else { "mail" }.to_string(),
                    message_id: l.message_id.clone(),
                    subject: l.cached_subject.clone(),
                    from: l.cached_from.clone(),
                    date: l.cached_date.clone(),
                    account: l.cached_account.clone(),
                    folder: l.cached_folder.clone(),
                    chat: chat.map(|c| ExportChatLink {
                        server: c.host,
                        room_id: c.rid,
                        message_id: c.message,
                        thread_id: (!c.thread.is_empty()).then_some(c.thread),
                        room: l.cached_account.clone(),
                        permalink: l.cached_folder.clone(),
                    }),
                }
            })
            .collect(),
        attachments,
        log,
    }
}

/// Put a payload in place: a hard link to the previous snapshot's copy when
/// there is one — a payload never changes under its UUID — else a copy of the
/// file `fetch` hands over.
fn place_payload(
    previous: Option<PathBuf>,
    destination: &Path,
    fetch: impl FnOnce() -> std::result::Result<PathBuf, String>,
) -> std::result::Result<(), String> {
    if let Some(parent) = destination.parent() {
        create_private_dir(parent).map_err(|e| e.to_string())?;
    }
    if let Some(previous) = previous
        && previous.is_file()
        && fs::hard_link(&previous, destination).is_ok()
    {
        return Ok(());
    }
    let source = fetch()?;
    fs::copy(&source, destination).map_err(|e| format!("cannot copy {}: {e}", source.display()))?;
    set_private(destination).map_err(|e| e.to_string())?;
    Ok(())
}

/// Every file of the snapshot but the manifest, with its size and hash.
fn build_manifest(root: &Path) -> Result<Manifest> {
    let mut files = Vec::new();
    for path in walk(root)? {
        let relative = path
            .strip_prefix(root)
            .map_err(|e| Error::Sync(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "manifest.json" {
            continue;
        }
        files.push(ManifestEntry {
            size: fs::metadata(&path)?.len(),
            sha256: sha256_file(&path)?,
            path: relative,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Manifest { files })
}

/// Read a snapshot back: `tasks.json` parses and is ours, and every file the
/// manifest names is there with its size and hash.
pub fn verify(root: &Path) -> Result<Snapshot> {
    let snapshot: Snapshot = serde_json::from_slice(&fs::read(root.join("tasks.json"))?)?;
    if snapshot.format != FORMAT {
        return Err(Error::Sync(format!(
            "{} is not an Orb task export",
            root.display()
        )));
    }
    if snapshot.format_version > FORMAT_VERSION {
        return Err(Error::Sync(format!(
            "{} is format version {}; this build reads up to {FORMAT_VERSION}",
            root.display(),
            snapshot.format_version
        )));
    }
    let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    for entry in &manifest.files {
        let path = root.join(&entry.path);
        let size = fs::metadata(&path)
            .map_err(|e| Error::Sync(format!("{} is missing: {e}", entry.path)))?
            .len();
        if size != entry.size || sha256_file(&path)? != entry.sha256 {
            return Err(Error::Sync(format!(
                "{} does not match the manifest",
                entry.path
            )));
        }
    }
    Ok(snapshot)
}

// ---------------------------------------------------------------------------
// Reading one back (§21).
// ---------------------------------------------------------------------------

/// `tasks.json` of a snapshot: ours, and in a version this build reads.
/// Payloads are checked one task at a time by [`imported`], so a damaged file
/// stops its own task and not the whole import.
pub fn read_snapshot(root: &Path) -> Result<Snapshot> {
    let raw = fs::read(root.join("tasks.json"))
        .map_err(|e| Error::Sync(format!("{} has no tasks.json: {e}", root.display())))?;
    let value: serde_json::Value = serde_json::from_slice(&raw)?;
    if value.get("format").and_then(serde_json::Value::as_str) != Some(FORMAT) {
        return Err(Error::Sync(format!(
            "{} is not an Orb task export",
            root.display()
        )));
    }
    let version = value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version > u64::from(FORMAT_VERSION) {
        return Err(Error::Sync(format!(
            "{} is format version {version}; this build reads up to {FORMAT_VERSION}",
            root.display()
        )));
    }
    Ok(serde_json::from_value(value)?)
}

/// An exported task as the model has it, with each attachment's payload file
/// in the snapshot: `None` where it is missing or not what the export
/// recorded.
pub struct ImportedTask {
    pub task: Task,
    pub reminders: Vec<TaskReminder>,
    pub links: Vec<TaskLinkedEmail>,
    pub attachments: Vec<(TaskAttachment, Option<PathBuf>)>,
    /// Payloads whose bytes do not match their SHA-256.
    pub damaged: Vec<String>,
}

/// Turn an exported task back into the model's, for `account`.
pub fn imported(root: &Path, exported: &ExportTask, account: &str) -> ImportedTask {
    let mut task = Task::bare(
        exported.id.clone(),
        exported.title.clone(),
        account.to_string(),
        exported.position,
        exported.created_at.clone(),
    );
    task.revision = exported.revision;
    task.body_text = exported.body.clone();
    task.set_dates(TaskDates::new(
        exported.postpone_date.as_deref().and_then(parse_civil_date),
        exported.due_date.as_deref().and_then(parse_civil_date),
    ));
    task.tags = exported.tags.clone();
    task.recurrence = exported.recurrence.as_ref().map(|r| r.rule.clone());
    task.recurrence_from_completion = exported
        .recurrence
        .as_ref()
        .is_some_and(|r| r.from_completion);
    task.steps = exported
        .steps
        .iter()
        .map(|s| TaskStep {
            id: s.id.clone(),
            title: s.title.clone(),
            done: s.done,
        })
        .collect();
    task.updated_at = exported.updated_at.clone();
    task.completed_at = exported.completed_at.clone();
    task.is_completed = exported.completed_at.is_some();
    task.deleted_at = exported.deleted_at.clone();
    task.log = exported
        .log
        .iter()
        .map(|entry| TaskLogEntry {
            id: None,
            task_id: None,
            entry_uuid: entry.id.clone(),
            occurred_at: entry.at.clone(),
            occurred_at_unix: DateTime::parse_from_rfc3339(&entry.at)
                .map(|t| t.timestamp())
                .unwrap_or(0),
            device_id: entry.device.clone(),
            event: TaskEvent::from_parts(
                &entry.event,
                entry.detail.as_ref().map(|d| d.to_string()).as_deref(),
            ),
        })
        .collect();

    let reminders = exported
        .reminders
        .iter()
        .map(|r| {
            let unix = DateTime::parse_from_rfc3339(&r.remind_at)
                .map(|t| t.timestamp())
                .unwrap_or(0);
            let mut reminder = TaskReminder::new(0, r.remind_at.clone(), unix);
            reminder.is_triggered = r.fired;
            reminder
        })
        .collect();
    let links = exported
        .links
        .iter()
        .map(|l| {
            TaskLinkedEmail::new(
                0,
                l.message_id.clone(),
                CachedEmailMetadata {
                    subject: l.subject.clone(),
                    from: l.from.clone(),
                    date: l.date.clone(),
                    account: l.account.clone(),
                    folder: l.folder.clone(),
                },
            )
        })
        .collect();

    // The ids name directories under the attachment cache once imported, so
    // an export whose ids are not identifiers is not one Orb wrote; its task
    // is reported and left out, as one with a damaged payload is.
    let mut damaged = Vec::new();
    if !is_wire_id(&exported.id) {
        damaged.push(format!("task id '{}'", exported.id));
    }
    let mut attachments = Vec::new();
    for a in &exported.attachments {
        if !is_wire_id(&a.id) {
            damaged.push(format!("attachment id '{}'", a.id));
            continue;
        }
        let payload = match (&a.path, &a.sha256) {
            (Some(path), Some(sha)) => {
                let file = root.join(path);
                match sha256_file(&file) {
                    Ok(actual) if &actual == sha => Some(file),
                    _ => {
                        damaged.push(a.filename.clone());
                        None
                    }
                }
            }
            _ => None,
        };
        attachments.push((
            TaskAttachment {
                id: None,
                task_id: 0,
                attachment_uuid: a.id.clone(),
                filename: a.filename.clone(),
                content_type: a.content_type.clone(),
                size: a.size,
                file_path: TaskAttachment::cache_path(&exported.id, &a.id, &a.filename),
                imap_uid: None,
                message_id: None,
                is_cached: payload.is_some(),
            },
            payload,
        ));
    }
    ImportedTask {
        task,
        reminders,
        links,
        attachments,
        damaged,
    }
}

// ---------------------------------------------------------------------------
// The directory: partials, retention, the newest snapshot.
// ---------------------------------------------------------------------------

/// A snapshot in the directory, by name.
#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    pub path: PathBuf,
    /// `YYYYMMDDTHHMMSSZ`, which sorts by time.
    pub stamp: String,
    pub host: String,
    pub complete: bool,
}

/// Every finished snapshot in `directory`, newest first.
pub fn snapshots(directory: &Path) -> Result<Vec<SnapshotEntry>> {
    Ok(newest_first(directory).collect())
}

/// The newest complete snapshot in `directory`.
pub fn newest_complete_snapshot(directory: &Path) -> Result<Option<SnapshotEntry>> {
    Ok(newest_first(directory).find(|s| s.complete))
}

/// The finished snapshots in `directory`, newest first. Whether one is complete
/// is read from its `tasks.json`, megabytes each, so only as far as the walk
/// goes: the newest complete snapshot is usually the first.
fn newest_first(directory: &Path) -> impl Iterator<Item = SnapshotEntry> {
    let mut named = Vec::new();
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix(PREFIX) else {
                continue;
            };
            if name.ends_with(PARTIAL) || !entry.path().is_dir() {
                continue;
            }
            let Some((stamp, host)) = rest.split_once('-') else {
                continue;
            };
            named.push((entry.path(), stamp.to_string(), host.to_string()));
        }
    }
    named.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    named.into_iter().map(|(path, stamp, host)| SnapshotEntry {
        complete: is_complete(&path),
        path,
        stamp,
        host,
    })
}

/// Whether a snapshot's `tasks.json` says it is complete; an unreadable one is not.
fn is_complete(snapshot: &Path) -> bool {
    #[derive(serde::Deserialize)]
    struct Completeness {
        #[serde(default)]
        complete: bool,
    }
    fs::read(snapshot.join("tasks.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<Completeness>(&raw).ok())
        .is_some_and(|c| c.complete)
}

/// The newest complete snapshot, which payloads are hard-linked from.
fn newest_complete(directory: &Path) -> Result<Option<PathBuf>> {
    Ok(newest_complete_snapshot(directory)?.map(|s| s.path))
}

/// When the newest complete snapshot in `directory` was taken.
pub fn last_complete(directory: &Path) -> Result<Option<DateTime<Utc>>> {
    Ok(newest_complete_snapshot(directory)?
        .and_then(|s| chrono::NaiveDateTime::parse_from_str(&s.stamp, "%Y%m%dT%H%M%SZ").ok())
        .map(|naive| naive.and_utc()))
}

/// A `.partial` left by an export that did not finish.
fn remove_partials(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(PREFIX) && name.ends_with(PARTIAL) && entry.path().is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

/// Keep this machine's newest `keep` complete snapshots, and drop its
/// incomplete ones older than the newest complete one. Another machine's
/// snapshots in a shared directory are its own business; the snapshot just
/// written is never removed.
fn apply_retention(
    directory: &Path,
    host: &str,
    keep: usize,
    newest: &Path,
) -> Result<Vec<PathBuf>> {
    let mine: Vec<SnapshotEntry> = snapshots(directory)?
        .into_iter()
        .filter(|s| s.host == host)
        .collect();
    let mut removed = Vec::new();
    let mut kept = 0;
    for snapshot in mine {
        if snapshot.path == newest {
            kept += 1;
            continue;
        }
        let goes = if snapshot.complete {
            kept += 1;
            kept > keep
        } else {
            true
        };
        if goes {
            fs::remove_dir_all(&snapshot.path)?;
            removed.push(snapshot.path);
        }
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Files.
// ---------------------------------------------------------------------------

fn pretty<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut out = serde_json::to_vec_pretty(value)?;
    out.push(b'\n');
    Ok(out)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn dir_size(root: &Path) -> Result<u64> {
    walk(root)?.iter().map(|p| Ok(fs::metadata(p)?.len())).sum()
}

/// A filename as a path component: no separators, nothing hidden, never empty.
fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned.to_string()
    }
}

fn sanitise_host(host: &str) -> String {
    let cleaned: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

/// Write a file readable by the user alone, and on disk before returning.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    // Every file in it first: a payload copied or linked is not synced yet.
    for file in walk(path)? {
        fs::File::open(&file)?.sync_all()?;
    }
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn readme(snapshot: &Snapshot) -> String {
    format!(
        "An export of Orb's tasks ({} of them), taken {} from the folder '{}' of the\n\
         account '{}'.\n\n\
         tasks.json     every task and the tag registry; see schema.json\n\
         attachments/   each attachment's payload, by attachment id and filename\n\
         manifest.json  every file's size and SHA-256\n\
         schema.json    a JSON Schema of tasks.json\n\n\
         Orb reads it back with :import-tasks. Format \"{}\", version {}.{}\n",
        snapshot.tasks.len(),
        snapshot.exported_at,
        snapshot.folder,
        snapshot.account,
        snapshot.format,
        snapshot.format_version,
        if snapshot.complete {
            ""
        } else {
            "\n\nINCOMPLETE: some payloads could not be fetched; see \"missing\" in tasks.json."
        }
    )
}

/// The JSON Schema of `tasks.json`, so a reader needs nothing but the snapshot.
pub const SCHEMA: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Orb task export",
  "type": "object",
  "required": ["format", "format_version", "exported_at", "account", "folder", "complete", "tags", "tasks"],
  "properties": {
    "format": { "const": "orb-tasks" },
    "format_version": { "type": "integer", "minimum": 1 },
    "exported_at": { "type": "string", "format": "date-time" },
    "orb_version": { "type": "string" },
    "device_id": { "type": ["string", "null"] },
    "account": { "type": "string" },
    "folder": { "type": "string" },
    "complete": { "type": "boolean" },
    "tags": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["id", "name", "position"],
        "properties": {
          "id": { "type": "string" },
          "name": { "type": "string" },
          "colour": { "type": ["string", "null"] },
          "position": { "type": "number" }
        }
      }
    },
    "tasks": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["id", "revision", "title", "tags", "reminders", "position", "steps",
                     "created_at", "updated_at", "links", "attachments", "log"],
        "properties": {
          "id": { "type": "string", "description": "The task's UUID, its identity" },
          "revision": { "type": "integer" },
          "title": { "type": "string" },
          "body": { "type": ["string", "null"] },
          "tags": { "type": "array", "items": { "type": "string" }, "description": "Tag ids" },
          "postpone_date": { "type": ["string", "null"], "format": "date" },
          "due_date": { "type": ["string", "null"], "format": "date" },
          "reminders": {
            "type": "array",
            "items": {
              "type": "object",
              "required": ["remind_at", "fired"],
              "properties": {
                "remind_at": { "type": "string", "format": "date-time" },
                "fired": { "type": "boolean" }
              }
            }
          },
          "recurrence": {
            "type": ["object", "null"],
            "properties": {
              "rule": { "type": "string", "description": "An RFC 5545 RRULE subset" },
              "from_completion": { "type": "boolean" }
            }
          },
          "position": { "type": "number" },
          "steps": {
            "type": "array",
            "items": {
              "type": "object",
              "required": ["id", "title", "done"],
              "properties": {
                "id": { "type": "string" },
                "title": { "type": "string" },
                "done": { "type": "boolean" }
              }
            }
          },
          "created_at": { "type": "string", "format": "date-time" },
          "updated_at": { "type": "string", "format": "date-time" },
          "completed_at": { "type": ["string", "null"], "format": "date-time" },
          "deleted_at": { "type": ["string", "null"], "format": "date-time" },
          "links": {
            "type": "array",
            "items": {
              "type": "object",
              "required": ["message_id"],
              "properties": {
                "kind": { "enum": ["mail", "chat"] },
                "message_id": { "type": "string" },
                "subject": { "type": ["string", "null"] },
                "from": { "type": ["string", "null"] },
                "date": { "type": ["string", "null"] },
                "account": { "type": ["string", "null"] },
                "folder": { "type": ["string", "null"] },
                "chat": {
                  "type": "object",
                  "required": ["server", "room_id", "message_id"],
                  "properties": {
                    "server": { "type": "string" },
                    "room_id": { "type": "string" },
                    "message_id": { "type": "string" },
                    "thread_id": { "type": ["string", "null"] },
                    "room": { "type": ["string", "null"] },
                    "permalink": { "type": ["string", "null"] }
                  }
                }
              }
            }
          },
          "attachments": {
            "type": "array",
            "items": {
              "type": "object",
              "required": ["id", "filename"],
              "properties": {
                "id": { "type": "string" },
                "filename": { "type": "string" },
                "content_type": { "type": ["string", "null"] },
                "size": { "type": ["integer", "null"] },
                "sha256": { "type": ["string", "null"] },
                "path": { "type": ["string", "null"], "description": "Inside the snapshot" },
                "missing": { "type": ["string", "null"], "description": "Why the payload is not here" }
              }
            }
          },
          "log": {
            "type": "array",
            "items": {
              "type": "object",
              "required": ["id", "at", "event", "text"],
              "properties": {
                "id": { "type": "string" },
                "at": { "type": "string", "format": "date-time" },
                "device": { "type": ["string", "null"] },
                "event": { "type": "string" },
                "detail": {},
                "text": { "type": "string" }
              }
            }
          }
        }
      }
    }
  }
}
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::model::{ChangeContext, TaskStep};
    use chrono::TimeZone;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// One task with a step, a reminder, a link, two log entries and an
    /// attachment whose payload is `payload`.
    fn source(payload_dir: &Path) -> (ExportSource, PathBuf) {
        let ctx = ChangeContext::new(
            at("2026-10-20T08:00:00Z"),
            chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap(),
        )
        .with_device_id(Some("device-a"));
        let mut task = Task::create(
            "Review the roadmap".to_string(),
            "Work".to_string(),
            1.0,
            &ctx,
        )
        .task;
        task.steps = vec![TaskStep::new("Read it".to_string())];
        let attachment =
            TaskAttachment::create(1, &task.task_uuid, "notes.pdf".to_string(), None, Some(5));
        let payload = payload_dir.join("payload");
        fs::write(&payload, b"%PDF-").unwrap();
        let source = ExportSource {
            device_id: Some("device-a".to_string()),
            account: "Work".to_string(),
            folder: "Tasks".to_string(),
            tags: vec![TaskTag::new("work".to_string(), 1.0)],
            tasks: vec![SourceTask {
                task,
                reminders: vec![TaskReminder::new(
                    1,
                    "2026-10-21T07:00:00+00:00".to_string(),
                    0,
                )],
                links: vec![
                    TaskLinkedEmail::new(
                        1,
                        "<a@example.com>".to_string(),
                        crate::tasks::CachedEmailMetadata {
                            subject: Some("Roadmap".to_string()),
                            ..Default::default()
                        },
                    ),
                    TaskLinkedEmail::new(
                        1,
                        "chat://chat.example.com/r1/m1?thread=t1".to_string(),
                        crate::tasks::CachedEmailMetadata {
                            subject: Some("Can you look at the log?".to_string()),
                            from: Some("Sara".to_string()),
                            account: Some("#dev".to_string()),
                            folder: Some(
                                "https://chat.example.com/channel/dev/thread/t1?msg=m1".to_string(),
                            ),
                            ..Default::default()
                        },
                    ),
                ],
                attachments: vec![attachment],
            }],
        };
        (source, payload)
    }

    #[test]
    fn a_snapshot_round_trips_and_checks_against_its_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (source, payload) = source(dir.path());
        let out = dir.path().join("exports");
        let outcome = write_snapshot(
            &out,
            &source,
            at("2026-10-21T08:00:00Z"),
            "studio.local",
            30,
            |_, _| Ok(payload.clone()),
        )
        .unwrap();
        assert!(outcome.complete);
        assert_eq!(
            outcome.path.file_name().unwrap(),
            "orb-tasks-20261021T080000Z-studio.local"
        );

        let snapshot = verify(&outcome.path).unwrap();
        let task = &snapshot.tasks[0];
        assert_eq!(task.title, "Review the roadmap");
        assert_eq!(task.steps[0].title, "Read it");
        assert_eq!(task.links[0].subject.as_deref(), Some("Roadmap"));
        assert_eq!(task.links[0].kind, "mail");
        assert_eq!(task.links[0].chat, None);
        assert_eq!(task.links[1].kind, "chat");
        assert_eq!(
            task.links[1].chat,
            Some(ExportChatLink {
                server: "chat.example.com".to_string(),
                room_id: "r1".to_string(),
                message_id: "m1".to_string(),
                thread_id: Some("t1".to_string()),
                room: Some("#dev".to_string()),
                permalink: Some(
                    "https://chat.example.com/channel/dev/thread/t1?msg=m1".to_string()
                ),
            })
        );
        assert_eq!(task.log[0].event, "created");
        let file = task.attachments[0].path.as_ref().unwrap();
        assert_eq!(fs::read(outcome.path.join(file)).unwrap(), b"%PDF-");
        for name in ["tasks.json", "schema.json", "README.txt", "manifest.json"] {
            assert!(outcome.path.join(name).is_file(), "{name}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&outcome.path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(outcome.path.join("tasks.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        // A tampered payload no longer checks.
        fs::write(outcome.path.join(file), b"changed").unwrap();
        assert!(verify(&outcome.path).is_err());
    }

    /// The same tasks give the same file, but for the time it was taken —
    /// what lets a directory of snapshots diff cleanly.
    #[test]
    fn the_output_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let (source, payload) = source(dir.path());
        let out = dir.path().join("exports");
        let write = |time: &str| {
            let outcome = write_snapshot(&out, &source, at(time), "host", 30, |_, _| {
                Ok(payload.clone())
            })
            .unwrap();
            fs::read_to_string(outcome.path.join("tasks.json"))
                .unwrap()
                .lines()
                .filter(|line| !line.contains("\"exported_at\""))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(write("2026-10-21T08:00:00Z"), write("2026-10-22T08:00:00Z"));
    }

    #[test]
    fn two_exports_in_one_second_both_land() {
        let dir = tempfile::tempdir().unwrap();
        let (source, payload) = source(dir.path());
        let out = dir.path().join("exports");
        let now = at("2026-10-21T08:00:00Z");
        let first =
            write_snapshot(&out, &source, now, "host", 30, |_, _| Ok(payload.clone())).unwrap();
        let second =
            write_snapshot(&out, &source, now, "host", 30, |_, _| Ok(payload.clone())).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(
            second.path.file_name().unwrap(),
            "orb-tasks-20261021T080001Z-host"
        );
    }

    #[test]
    fn a_payload_that_cannot_be_fetched_makes_an_incomplete_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (source, _) = source(dir.path());
        let out = dir.path().join("exports");
        let outcome = write_snapshot(
            &out,
            &source,
            at("2026-10-21T08:00:00Z"),
            "host",
            30,
            |_, _| Err("offline".to_string()),
        )
        .unwrap();
        assert!(!outcome.complete);
        assert_eq!(outcome.missing.len(), 1);
        let snapshot = verify(&outcome.path).unwrap();
        assert!(!snapshot.complete);
        assert_eq!(
            snapshot.tasks[0].attachments[0].missing.as_deref(),
            Some("offline")
        );
        assert!(last_complete(&out).unwrap().is_none());
    }

    #[test]
    fn the_last_complete_snapshot_is_found_behind_an_incomplete_one() {
        let dir = tempfile::tempdir().unwrap();
        let (source, payload) = source(dir.path());
        let out = dir.path().join("exports");
        let complete = write_snapshot(
            &out,
            &source,
            at("2026-10-21T08:00:00Z"),
            "host",
            30,
            |_, _| Ok(payload.clone()),
        )
        .unwrap();
        // Another machine's, which this one's retention leaves alone.
        let incomplete = out.join("orb-tasks-20261022T080000Z-other");
        fs::create_dir_all(&incomplete).unwrap();
        fs::write(incomplete.join("tasks.json"), br#"{"complete": false}"#).unwrap();
        assert_eq!(
            last_complete(&out).unwrap(),
            Some(at("2026-10-21T08:00:00Z"))
        );
        assert_eq!(
            newest_complete_snapshot(&out).unwrap().map(|s| s.path),
            Some(complete.path)
        );
        let all = snapshots(&out).unwrap();
        assert_eq!(
            all.iter().map(|s| s.complete).collect::<Vec<_>>(),
            [false, true]
        );
    }

    /// A crash between write and rename leaves a `.partial` and every earlier
    /// snapshot as it was; the next export clears the `.partial`.
    #[test]
    fn a_crashed_export_leaves_the_earlier_snapshots_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (source, payload) = source(dir.path());
        let out = dir.path().join("exports");
        let first = write_snapshot(
            &out,
            &source,
            at("2026-10-21T08:00:00Z"),
            "host",
            30,
            |_, _| Ok(payload.clone()),
        )
        .unwrap();
        let stale = out.join("orb-tasks-20261022T080000Z-host.partial");
        fs::create_dir_all(stale.join("attachments")).unwrap();
        fs::write(stale.join("tasks.json"), b"{").unwrap();
        assert!(verify(&first.path).is_ok());
        assert_eq!(snapshots(&out).unwrap().len(), 1);

        write_snapshot(
            &out,
            &source,
            at("2026-10-23T08:00:00Z"),
            "host",
            30,
            |_, _| Ok(payload.clone()),
        )
        .unwrap();
        assert!(!stale.exists());
        assert!(verify(&first.path).is_ok());
    }

    /// The newest `keep` complete snapshots stay; the only complete one
    /// always does; another machine's are never touched.
    #[test]
    fn retention_keeps_the_newest_and_never_the_last_or_anothers() {
        let dir = tempfile::tempdir().unwrap();
        let (source, payload) = source(dir.path());
        let out = dir.path().join("exports");
        let other = write_snapshot(
            &out,
            &source,
            at("2026-10-20T08:00:00Z"),
            "laptop",
            1,
            |_, _| Ok(payload.clone()),
        )
        .unwrap();
        for day in 21..=24 {
            let time = Utc.with_ymd_and_hms(2026, 10, day, 8, 0, 0).unwrap();
            write_snapshot(&out, &source, time, "studio", 2, |_, _| Ok(payload.clone())).unwrap();
        }
        let stamps: Vec<String> = snapshots(&out)
            .unwrap()
            .into_iter()
            .filter(|s| s.host == "studio")
            .map(|s| s.stamp)
            .collect();
        assert_eq!(stamps, ["20261024T080000Z", "20261023T080000Z"]);
        assert!(other.path.exists());

        // Keep 0 still keeps the one just written.
        let last = write_snapshot(
            &out,
            &source,
            at("2026-10-25T08:00:00Z"),
            "studio",
            0,
            |_, _| Ok(payload.clone()),
        )
        .unwrap();
        assert!(last.path.exists());
    }

    /// A `tasks.json` as version 1 writes it, kept verbatim: every build must
    /// read it, whatever the format becomes (§21).
    #[test]
    fn a_version_one_snapshot_stays_readable() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tasks.json"),
            r#"{
  "format": "orb-tasks",
  "format_version": 1,
  "exported_at": "2026-10-21T08:00:00+00:00",
  "orb_version": "0.9.0",
  "device_id": "device-a",
  "account": "Work",
  "folder": "Tasks",
  "complete": true,
  "tags": [{ "id": "tag-1", "name": "work", "colour": null, "position": 1.0 }],
  "tasks": [{
    "id": "task-1", "revision": 7, "title": "Review the roadmap", "body": "Sections 2 and 4",
    "tags": ["tag-1"], "postpone_date": "2026-10-21", "due_date": "2026-10-23",
    "reminders": [{ "remind_at": "2026-10-22T07:00:00+00:00", "fired": false }],
    "recurrence": { "rule": "FREQ=WEEKLY", "from_completion": false },
    "position": 3.5,
    "steps": [{ "id": "step-1", "title": "Read it", "done": true }],
    "created_at": "2026-10-20T08:00:00+00:00", "updated_at": "2026-10-21T07:00:00+00:00",
    "completed_at": null, "deleted_at": null,
    "links": [{ "message_id": "<a@example.com>", "subject": "Roadmap", "from": "Sara",
                "date": null, "account": "Work", "folder": "INBOX" }],
    "attachments": [],
    "log": [{ "id": "log-1", "at": "2026-10-20T08:00:00+00:00", "device": "device-a",
              "event": "created", "detail": { "event": "created", "title": "Review the roadmap" },
              "text": "Created 'Review the roadmap'" }]
  }]
}"#,
        )
        .unwrap();
        let snapshot = read_snapshot(dir.path()).unwrap();
        let task = imported(dir.path(), &snapshot.tasks[0], "Work");
        assert_eq!(task.task.task_uuid, "task-1");
        assert_eq!(task.task.revision, 7);
        assert_eq!(task.task.due_date.as_deref(), Some("2026-10-23"));
        assert_eq!(task.task.steps[0].title, "Read it");
        assert!(matches!(task.task.log[0].event, TaskEvent::Created { .. }));
        assert_eq!(task.reminders.len(), 1);
        assert_eq!(task.links[0].message_id, "<a@example.com>");

        // A newer format is refused by name rather than guessed at.
        let raw = fs::read_to_string(dir.path().join("tasks.json"))
            .unwrap()
            .replace("\"format_version\": 1", "\"format_version\": 2");
        fs::write(dir.path().join("tasks.json"), raw).unwrap();
        let refused = read_snapshot(dir.path()).err().unwrap().to_string();
        assert!(refused.contains("format version 2"), "{refused}");
    }

    /// A payload in the previous snapshot is linked rather than fetched again.
    #[test]
    fn a_payload_already_exported_is_linked_not_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let (source, payload) = source(dir.path());
        let out = dir.path().join("exports");
        let first = write_snapshot(
            &out,
            &source,
            at("2026-10-21T08:00:00Z"),
            "host",
            30,
            |_, _| Ok(payload.clone()),
        )
        .unwrap();
        let mut fetched = 0;
        let second = write_snapshot(
            &out,
            &source,
            at("2026-10-22T08:00:00Z"),
            "host",
            30,
            |_, _| {
                fetched += 1;
                Ok(payload.clone())
            },
        )
        .unwrap();
        assert_eq!(fetched, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let file = verify(&second.path).unwrap().tasks[0].attachments[0]
                .path
                .clone()
                .unwrap();
            assert_eq!(
                fs::metadata(first.path.join(&file)).unwrap().ino(),
                fs::metadata(second.path.join(&file)).unwrap().ino()
            );
        }
    }
}
