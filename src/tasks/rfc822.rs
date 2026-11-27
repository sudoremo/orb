//! The `X-Orb-Task-*` wire format, version 2.
//!
//! The tasks folder is the source of truth and the SQLite tables are a cache
//! that must be fully rebuildable from it (§3), so everything a task *is* has to
//! survive a round trip through this module. What follows are the decisions that
//! shape the format, each of which exists because the obvious alternative loses
//! data.
//!
//! # Three kinds of message share one folder
//!
//! `X-Orb-Task-Type` says which: `task`, `tag-registry` (§7), `attachment`
//! (§9), `chat-marks` (chat-spec §6) or `today-cleared` (§10.2). A kind an
//! older build cannot read it
//! so leaves where it is. Reconciliation has to classify a message from its headers alone,
//! because the §3.3 listing fetches headers only — a folder-wide body fetch on
//! every sync is exactly the design that a 10 MB attachment makes untenable.
//!
//! # Identity is a header, never the `Message-ID`
//!
//! IMAP messages are immutable, so every edit is APPEND-then-delete and the
//! `Message-ID` therefore changes on every edit: it identifies an *edit*, not a
//! *task*. [`TaskEmailBuilder::build`] mints a fresh one on every call for that
//! reason — reusing the old one would put two different messages on the server
//! under one `Message-ID`, and the superseded-message hunt would have no way to
//! tell which of them to expunge. `X-Orb-Task-Id` is the identity, generated
//! once before the first append and never again, which is what makes a retried
//! append idempotent at the task level (§3.5).
//!
//! # An attachment's payload is not in the task message
//!
//! Renaming a task rewrites the task message. If payloads lived inside it, that
//! rename would re-upload every attached file (§9). So the task message carries
//! only descriptors — `X-Orb-Task-Attachments`, one header per attachment,
//! giving the attachment's uuid, size, content type and filename — and each
//! payload lives in its own message carrying its owner's `X-Orb-Task-Id`. The
//! type system enforces it: [`TaskEmailBuilder::build`] is handed
//! [`TaskAttachment`] descriptors, which have nowhere to put bytes.
//!
//! # The log is a MIME part, not headers
//!
//! The log (§11) is unbounded, contains free text, and is the one part of a task
//! exempt from last-writer-wins: on conflict it is merged as the union of
//! entries keyed by `entry_uuid`. A union merge can only be correct if every
//! entry survives a round trip through a build that has never heard of it, so
//! entries are serialised with their `event_type` and their `detail` payload
//! kept *verbatim as a string* — re-encoding the payload through a typed value
//! would reorder its keys and make two builds disagree about a byte-identical
//! entry. [`TaskEvent::from_parts`] is the matching half of that contract.
//!
//! # Every part is base64
//!
//! RFC 5322 caps a line at 998 octets and a task's notes are free text with no
//! such cap; `Content-Transfer-Encoding: 8bit` also asks the server to accept
//! raw 8-bit octets in an APPEND literal. Base64 makes every body 7-bit and
//! short-lined by construction, which removes both failure modes at the cost of
//! not being able to read a task message with `less`.
//!
//! # Version 1 is unreadable, deliberately
//!
//! Version 1 never shipped (§0), and its headers mean different things: it had
//! no task id, so a v1 message has no identity to reconcile by, and its
//! `Snooze-Until` predates the postpone/due vocabulary. A partial parse would
//! therefore invent an identity and mint a duplicate task on every sync. Such a
//! message is logged and skipped, and left on the server untouched.

use chrono::{DateTime, Utc};
use mailparse::{MailHeaderMap, ParsedMail};
use rfc2047_decoder::{Decoder as Rfc2047Decoder, RecoverStrategy};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model::{
    Task, TaskAttachment, TaskDates, TaskEvent, TaskLinkedEmail, TaskLogEntry, TaskReminder,
    TaskTag, format_timestamp, is_wire_id, parse_civil_date,
};
use super::recurrence::{RecurrenceRule, Series};
use crate::{Result, logger};

/// Prefix shared by every Orb task header.
const HEADER_PREFIX: &str = "X-Orb-Task-";

/// The wire format this build writes for everything but a task with steps.
pub const TASK_VERSION: u8 = 2;
/// A task with steps (todo-spec §17): version 2 plus the steps part, written
/// apart so that an older build skips it rather than writing it back without
/// its steps. This build reads both; anything else is skipped.
pub const TASK_VERSION_STEPS: u8 = 3;

/// The highest revision a message may carry. One edit is one bump, so this
/// is more edits than a task will ever see, and far enough below `i64::MAX`
/// that bumping past it cannot overflow; it is also exact as a JSON number.
pub const MAX_REVISION: i64 = 1 << 53;

const H_VERSION: &str = "X-Orb-Task-Version";
const H_TYPE: &str = "X-Orb-Task-Type";
const H_ID: &str = "X-Orb-Task-Id";
const H_REVISION: &str = "X-Orb-Task-Revision";
const H_ATTACHMENT_ID: &str = "X-Orb-Task-Attachment-Id";
const H_ATTACHMENTS: &str = "X-Orb-Task-Attachments";
const H_CREATED: &str = "X-Orb-Task-Created";
const H_UPDATED: &str = "X-Orb-Task-Updated";
const H_COMPLETED_AT: &str = "X-Orb-Task-Completed-At";
const H_DELETED_AT: &str = "X-Orb-Task-Deleted-At";
const H_POSTPONE: &str = "X-Orb-Task-Postpone";
const H_DUE: &str = "X-Orb-Task-Due";
const H_TAGS: &str = "X-Orb-Task-Tags";
const H_RECURRENCE: &str = "X-Orb-Task-Recurrence";
const H_RECURRENCE_FROM: &str = "X-Orb-Task-Recurrence-From";
/// The series (§32), in the text form of [`Series`].
const H_RECURRENCE_SERIES: &str = "X-Orb-Task-Recurrence-Series";
const H_POSITION: &str = "X-Orb-Task-Position";
const H_REMINDERS: &str = "X-Orb-Task-Reminders";
const H_PART: &str = "X-Orb-Task-Part";

/// Value of [`H_RECURRENCE_FROM`] that selects habit-style advancement.
const RECURRENCE_FROM_COMPLETION: &str = "completion";

/// Marks the MIME part holding the task log.
const PART_LOG: &str = "log";
/// Marks the MIME part holding linked-message metadata.
const PART_LINKS: &str = "links";
const PART_STEPS: &str = "steps";

/// The exact headers the §3.3 listing has to ask for.
///
/// Reconciliation runs on a full `UID FETCH 1:* (UID FLAGS BODY.PEEK[HEADER.FIELDS
/// (...)])` of the whole folder, never on a MODSEQ delta — one of the servers in
/// use never advances `HIGHESTMODSEQ`, so a delta design freezes silently on it.
/// Keeping the list here means the fetch and [`TaskEmailParser::parse_identity`]
/// cannot drift apart: a header that is fetched but not parsed is waste, and one
/// that is parsed but not fetched is a task that reconciliation decides has no
/// identity.
pub const IDENTITY_HEADERS: [&str; 5] = [H_VERSION, H_TYPE, H_ID, H_REVISION, H_ATTACHMENT_ID];

/// `X-Orb-Task-Id` of the tag registry message.
///
/// The registry is a singleton, but it is synced by the same identity/revision
/// rules as a task (§7), and those rules are stated in terms of a task id. A
/// well-known constant gives it one: two registry messages in the folder are
/// then the ordinary "one id, two messages" conflict, resolved by highest
/// revision, rather than a special case reconciliation has to know about.
pub const TAG_REGISTRY_ID: &str = "00000000-0000-4000-8000-000000000001";

/// `X-Orb-Task-Id` of the message holding Waiting's marks (chat-spec §6),
/// a singleton under the registry's rules for the same reason.
pub const CHAT_MARKS_ID: &str = "00000000-0000-4000-8000-000000000002";

/// `X-Orb-Task-Id` of the message holding when Today's completed tasks were
/// last cleared (§10.2), a singleton under the registry's rules too.
pub const TODAY_CLEARED_ID: &str = "00000000-0000-4000-8000-000000000003";

/// Which kind of message in the tasks folder a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskMessageKind {
    Task,
    TagRegistry,
    Attachment,
    ChatMarks,
    TodayCleared,
}

impl TaskMessageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskMessageKind::Task => "task",
            TaskMessageKind::TagRegistry => "tag-registry",
            TaskMessageKind::Attachment => "attachment",
            TaskMessageKind::ChatMarks => "chat-marks",
            TaskMessageKind::TodayCleared => "today-cleared",
        }
    }

    /// Read the `X-Orb-Task-Type` value.
    ///
    /// An absent type is a task: the header is always written, but a message
    /// that reached the folder without one is far more likely to be a task from
    /// some other Orb build than a payload, and guessing "task" costs a failed
    /// parse where guessing "attachment" would discard one.
    fn parse(value: Option<&str>) -> Option<Self> {
        match value.map(str::trim) {
            None | Some("task") => Some(TaskMessageKind::Task),
            Some("tag-registry") => Some(TaskMessageKind::TagRegistry),
            Some("attachment") => Some(TaskMessageKind::Attachment),
            Some("chat-marks") => Some(TaskMessageKind::ChatMarks),
            Some("today-cleared") => Some(TaskMessageKind::TodayCleared),
            Some(_) => None,
        }
    }
}

/// What the §3.3 listing learns about one message without fetching its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIdentity {
    pub kind: TaskMessageKind,
    /// `X-Orb-Task-Id`: the task's identity, or the owning task's for an
    /// attachment, or [`TAG_REGISTRY_ID`] for the registry.
    pub task_uuid: String,
    /// Compared against the local cache to decide whether the body is worth
    /// fetching. Attachment messages are immutable and report 0.
    pub revision: i64,
    /// Set only on an attachment message, and what lets the orphan sweep of §9
    /// work from the listing alone.
    pub attachment_uuid: Option<String>,
}

/// A parsed message from the tasks folder.
#[derive(Debug)]
pub enum TaskMessage {
    /// Boxed because a task carries its whole log and the other two variants are
    /// small; an unboxed enum would make every attachment descriptor pay for it.
    Task(Box<ParsedTask>),
    TagRegistry(ParsedTagRegistry),
    Attachment(ParsedAttachment),
    ChatMarks(ParsedChatMarks),
    TodayCleared(ParsedTodayCleared),
}

impl TaskMessage {
    pub fn kind(&self) -> TaskMessageKind {
        match self {
            TaskMessage::Task(_) => TaskMessageKind::Task,
            TaskMessage::TagRegistry(_) => TaskMessageKind::TagRegistry,
            TaskMessage::Attachment(_) => TaskMessageKind::Attachment,
            TaskMessage::ChatMarks(_) => TaskMessageKind::ChatMarks,
            TaskMessage::TodayCleared(_) => TaskMessageKind::TodayCleared,
        }
    }

    /// The task variant, or `None` for the other two kinds.
    pub fn into_task(self) -> Option<ParsedTask> {
        match self {
            TaskMessage::Task(parsed) => Some(*parsed),
            _ => None,
        }
    }
}

/// A task reconstructed from its message.
///
/// Row ids are absent throughout: they are cache-local and are reassigned when
/// the database is rebuilt (§14.5), so the persistence layer fills them in with
/// [`ParsedTask::set_task_id`] once it knows them.
#[derive(Debug)]
pub struct ParsedTask {
    /// The task, log included.
    pub task: Task,
    /// Reminders, with `task_id` 0 until the task is inserted.
    pub reminders: Vec<TaskReminder>,
    /// Linked messages with their cached metadata, `task_id` 0 until inserted.
    pub linked_emails: Vec<TaskLinkedEmail>,
    /// Attachment *descriptors*. The payloads live in their own messages and
    /// arrive as [`ParsedAttachment`]; these carry no `imap_uid` or
    /// `message_id` because the task message does not know where its payloads
    /// ended up.
    pub attachments: Vec<TaskAttachment>,
}

impl ParsedTask {
    /// Stamp the task's row id onto everything that references it.
    pub fn set_task_id(&mut self, task_id: i64) {
        self.task.id = Some(task_id);
        for entry in &mut self.task.log {
            entry.task_id = Some(task_id);
        }
        for reminder in &mut self.reminders {
            reminder.task_id = task_id;
        }
        for linked_email in &mut self.linked_emails {
            linked_email.task_id = task_id;
        }
        for attachment in &mut self.attachments {
            attachment.task_id = task_id;
        }
    }
}

/// Waiting's marks (chat-spec §6) reconstructed from their message.
#[derive(Debug, Clone)]
pub struct ParsedChatMarks {
    pub revision: i64,
    pub marks: Vec<crate::chat::marks::SharedMark>,
    pub message_id: Option<String>,
    pub imap_uid: Option<u32>,
}

/// When Today's completed tasks were last cleared (§10.2), from its message.
#[derive(Debug, Clone)]
pub struct ParsedTodayCleared {
    pub revision: i64,
    pub cleared_at: Option<DateTime<Utc>>,
    pub message_id: Option<String>,
    pub imap_uid: Option<u32>,
}

/// The tag registry (§7) reconstructed from its message.
#[derive(Debug)]
pub struct ParsedTagRegistry {
    /// Resolved against the local registry revision by the same highest-wins
    /// rule tasks use.
    pub revision: i64,
    pub tags: Vec<TaskTag>,
    pub message_id: Option<String>,
    pub imap_uid: Option<u32>,
}

/// One attachment payload message (§9).
#[derive(Debug)]
pub struct ParsedAttachment {
    /// The owning task's `X-Orb-Task-Id`. An attachment whose owner is not in
    /// the folder is the orphan §9 has garbage-collected on sync.
    pub task_uuid: String,
    /// The descriptor, with `imap_uid`/`message_id` filled in from this message
    /// — that is how the payload is found again in order to expunge it.
    pub attachment: TaskAttachment,
    /// The decoded file bytes, ready to write into the disposable cache.
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------

/// Builds the three kinds of message in the tasks folder.
pub struct TaskEmailBuilder;

impl TaskEmailBuilder {
    /// Build the message for a task.
    ///
    /// Returns the raw message and the `Message-ID` it was given. That id is
    /// freshly minted on every call, because this message *is* a new message:
    /// the caller records it as the task's current location only once the APPEND
    /// has been confirmed, and the previous one is what gets expunged.
    ///
    /// `attachments` are descriptors only — see the module docs for why the
    /// payloads are somewhere else.
    pub fn build(
        task: &Task,
        reminders: &[TaskReminder],
        linked_emails: &[TaskLinkedEmail],
        attachments: &[TaskAttachment],
    ) -> Result<(Vec<u8>, String)> {
        let message_id = Self::generate_message_id();
        let boundary = generate_boundary();

        let mut email = String::new();
        email.push_str(&header_line("From", "tasks@orb.invalid"));
        email.push_str(&header_line("To", "tasks@orb.invalid"));
        email.push_str(&header_line("Subject", &task.subject));
        email.push_str(&header_line("Date", &format_rfc2822_date(&task.updated_at)));
        email.push_str(&header_line("Message-ID", &message_id));
        email.push_str(&header_line("MIME-Version", "1.0"));

        let version = if task.steps.is_empty() {
            TASK_VERSION
        } else {
            TASK_VERSION_STEPS
        };
        email.push_str(&header_line(H_VERSION, &version.to_string()));
        email.push_str(&header_line(H_TYPE, TaskMessageKind::Task.as_str()));
        email.push_str(&header_line(H_ID, &task.task_uuid));
        email.push_str(&header_line(H_REVISION, &task.revision.to_string()));
        email.push_str(&header_line(H_CREATED, &task.created_at));
        email.push_str(&header_line(H_UPDATED, &task.updated_at));

        // `is_completed` is deliberately not its own header: it is exactly
        // "`completed_at` is set", and two headers that can disagree are a bug
        // waiting for the one message where they do.
        if let Some(completed_at) = &task.completed_at {
            email.push_str(&header_line(H_COMPLETED_AT, completed_at));
        }
        // The soft delete of §10.1. Its whole point is that it travels: a task
        // trashed here has to disappear everywhere, which a local-only flag
        // cannot do.
        if let Some(deleted_at) = &task.deleted_at {
            email.push_str(&header_line(H_DELETED_AT, deleted_at));
        }
        if let Some(postpone) = &task.postpone_date {
            email.push_str(&header_line(H_POSTPONE, postpone));
        }
        if let Some(due) = &task.due_date {
            email.push_str(&header_line(H_DUE, due));
        }
        // Tag *ids*, so renaming a tag rewrites the one registry message rather
        // than every task that carries it (§7).
        if !task.tags.is_empty() {
            email.push_str(&header_line(H_TAGS, &task.tags.join(" ")));
        }
        if let Some(recurrence) = &task.recurrence {
            email.push_str(&header_line(H_RECURRENCE, recurrence));
            if task.recurrence_from_completion {
                email.push_str(&header_line(H_RECURRENCE_FROM, RECURRENCE_FROM_COMPLETION));
            }
            if let Some(series) = &task.recurrence_series {
                email.push_str(&header_line(H_RECURRENCE_SERIES, series));
            }
        }
        email.push_str(&header_line(H_POSITION, &format_position(task.position)));

        // Reminders are instants and nothing else, so they fit a header without
        // any escaping question. `is_triggered` stays behind: §5.2 makes firing
        // per-machine on purpose, and putting it on the wire would let the first
        // machine to fire suppress the notification on all the others.
        //
        // The separator carries a space so the line has somewhere to fold: a
        // task can carry any number of reminders, and thirty of them run past
        // the 998-octet hard limit on an unfoldable line.
        if !reminders.is_empty() {
            let value = reminders
                .iter()
                .map(|reminder| reminder.remind_at.trim())
                .collect::<Vec<_>>()
                .join("; ");
            email.push_str(&header_line(H_REMINDERS, &value));
        }

        // One header per attachment rather than one header listing all of them:
        // a filename is free text, and a single line holding several of them
        // needs an escaping scheme for the separator. One descriptor per line
        // with the filename last needs none.
        for attachment in attachments {
            email.push_str(&header_line(
                H_ATTACHMENTS,
                &attachment_descriptor(attachment),
            ));
        }

        email.push_str(&header_line(
            "Content-Type",
            &format!("multipart/mixed; boundary=\"{boundary}\""),
        ));
        email.push_str("\r\n");

        // Part 1: the notes.
        email.push_str(&format!("--{boundary}\r\n"));
        email.push_str(&header_line(
            "Content-Type",
            "text/plain; charset=\"UTF-8\"",
        ));
        email.push_str(&header_line("Content-Transfer-Encoding", "base64"));
        email.push_str("\r\n");
        let body = task.body_text.as_deref().unwrap_or("");
        email.push_str(&base64_body(to_crlf(body).as_bytes()));

        // Part 2: the log (§11).
        email.push_str(&format!("--{boundary}\r\n"));
        email.push_str(&json_part_headers(PART_LOG, "orb-task-log.json"));
        let log_json = serde_json::to_string(&WireLogEntry::from_entries(&task.log))
            .map_err(|error| invalid(format!("cannot serialise the task log: {error}")))?;
        email.push_str(&base64_body(log_json.as_bytes()));

        // Part 3: linked messages (§6), present only when there are any.
        //
        // A structured part rather than headers because the cached subject and
        // sender are what keep the link readable after the message itself is
        // gone forever, and they are arbitrary text.
        if !linked_emails.is_empty() {
            email.push_str(&format!("--{boundary}\r\n"));
            email.push_str(&json_part_headers(PART_LINKS, "orb-task-links.json"));
            let links_json = serde_json::to_string(&WireLink::from_links(linked_emails))
                .map_err(|error| invalid(format!("cannot serialise the task links: {error}")))?;
            email.push_str(&base64_body(links_json.as_bytes()));
        }

        // Part 4: the steps (§17), present only when there are any.
        if !task.steps.is_empty() {
            email.push_str(&format!("--{boundary}\r\n"));
            email.push_str(&json_part_headers(PART_STEPS, "orb-task-steps.json"));
            let steps_json = serde_json::to_string(&task.steps)
                .map_err(|error| invalid(format!("cannot serialise the task steps: {error}")))?;
            email.push_str(&base64_body(steps_json.as_bytes()));
        }

        email.push_str(&format!("--{boundary}--\r\n"));

        Ok((email.into_bytes(), message_id))
    }

    /// Build the tag registry message (§7).
    ///
    /// `revision` is the registry's own, bumped on every rename or deletion, and
    /// resolved against another device's copy by the same highest-wins rule that
    /// governs a task.
    pub fn build_tag_registry(
        tags: &[TaskTag],
        revision: i64,
        now: DateTime<Utc>,
    ) -> Result<(Vec<u8>, String)> {
        let message_id = Self::generate_message_id();

        let mut email = String::new();
        email.push_str(&header_line("From", "tasks@orb.invalid"));
        email.push_str(&header_line("To", "tasks@orb.invalid"));
        email.push_str(&header_line("Subject", "Orb tag registry"));
        email.push_str(&header_line("Date", &now.format(RFC2822).to_string()));
        email.push_str(&header_line("Message-ID", &message_id));
        email.push_str(&header_line("MIME-Version", "1.0"));
        email.push_str(&header_line(H_VERSION, &TASK_VERSION.to_string()));
        email.push_str(&header_line(H_TYPE, TaskMessageKind::TagRegistry.as_str()));
        email.push_str(&header_line(H_ID, TAG_REGISTRY_ID));
        email.push_str(&header_line(H_REVISION, &revision.to_string()));
        email.push_str(&header_line(
            "Content-Type",
            "application/json; charset=\"UTF-8\"",
        ));
        email.push_str(&header_line("Content-Transfer-Encoding", "base64"));
        email.push_str("\r\n");

        let json = serde_json::to_string(&WireTag::from_tags(tags))
            .map_err(|error| invalid(format!("cannot serialise the tag registry: {error}")))?;
        email.push_str(&base64_body(json.as_bytes()));

        Ok((email.into_bytes(), message_id))
    }

    /// Build the message holding Waiting's marks (chat-spec §6), under the
    /// registry's rules: a revision of its own, a fresh `Message-ID` each time.
    pub fn build_chat_marks(
        marks: &[crate::chat::marks::SharedMark],
        revision: i64,
        now: DateTime<Utc>,
    ) -> Result<(Vec<u8>, String)> {
        let message_id = Self::generate_message_id();

        let mut email = String::new();
        email.push_str(&header_line("From", "tasks@orb.invalid"));
        email.push_str(&header_line("To", "tasks@orb.invalid"));
        email.push_str(&header_line("Subject", "Orb chat marks"));
        email.push_str(&header_line("Date", &now.format(RFC2822).to_string()));
        email.push_str(&header_line("Message-ID", &message_id));
        email.push_str(&header_line("MIME-Version", "1.0"));
        email.push_str(&header_line(H_VERSION, &TASK_VERSION.to_string()));
        email.push_str(&header_line(H_TYPE, TaskMessageKind::ChatMarks.as_str()));
        email.push_str(&header_line(H_ID, CHAT_MARKS_ID));
        email.push_str(&header_line(H_REVISION, &revision.to_string()));
        email.push_str(&header_line(
            "Content-Type",
            "application/json; charset=\"UTF-8\"",
        ));
        email.push_str(&header_line("Content-Transfer-Encoding", "base64"));
        email.push_str("\r\n");

        let json = serde_json::to_string(marks)
            .map_err(|error| invalid(format!("cannot serialise the chat marks: {error}")))?;
        email.push_str(&base64_body(json.as_bytes()));

        Ok((email.into_bytes(), message_id))
    }

    /// Build the message holding when Today's completed tasks were last
    /// cleared (§10.2), under the registry's rules.
    pub fn build_today_cleared(
        cleared_at: DateTime<Utc>,
        revision: i64,
        now: DateTime<Utc>,
    ) -> Result<(Vec<u8>, String)> {
        let message_id = Self::generate_message_id();

        let mut email = String::new();
        email.push_str(&header_line("From", "tasks@orb.invalid"));
        email.push_str(&header_line("To", "tasks@orb.invalid"));
        email.push_str(&header_line("Subject", "Orb today cleared"));
        email.push_str(&header_line("Date", &now.format(RFC2822).to_string()));
        email.push_str(&header_line("Message-ID", &message_id));
        email.push_str(&header_line("MIME-Version", "1.0"));
        email.push_str(&header_line(H_VERSION, &TASK_VERSION.to_string()));
        email.push_str(&header_line(H_TYPE, TaskMessageKind::TodayCleared.as_str()));
        email.push_str(&header_line(H_ID, TODAY_CLEARED_ID));
        email.push_str(&header_line(H_REVISION, &revision.to_string()));
        email.push_str(&header_line(
            "Content-Type",
            "application/json; charset=\"UTF-8\"",
        ));
        email.push_str(&header_line("Content-Transfer-Encoding", "base64"));
        email.push_str("\r\n");

        let json = serde_json::json!({ "cleared_at": format_timestamp(cleared_at) }).to_string();
        email.push_str(&base64_body(json.as_bytes()));

        Ok((email.into_bytes(), message_id))
    }

    /// Build the message carrying one attachment's payload (§9).
    ///
    /// Written once, when the attachment is added, and never rewritten: nothing
    /// in a descriptor can change without the file changing, and a file that
    /// changes is a new attachment. That is what makes editing a task title free
    /// no matter how large its attachments are.
    pub fn build_attachment(
        task_uuid: &str,
        attachment: &TaskAttachment,
        payload: &[u8],
        now: DateTime<Utc>,
    ) -> Result<(Vec<u8>, String)> {
        let message_id = Self::generate_message_id();
        let content_type = attachment
            .content_type
            .as_deref()
            .unwrap_or("application/octet-stream");

        // The size on the wire is the size of what is actually here, not
        // whatever the caller believed: a descriptor that disagrees with its own
        // payload is a download that reports the wrong progress and a cache that
        // cannot tell a truncated file from a complete one.
        let mut descriptor = attachment.clone();
        descriptor.size = Some(payload.len() as i64);

        let mut email = String::new();
        email.push_str(&header_line("From", "tasks@orb.invalid"));
        email.push_str(&header_line("To", "tasks@orb.invalid"));
        email.push_str(&header_line(
            "Subject",
            &format!("Orb attachment: {}", attachment.filename),
        ));
        email.push_str(&header_line("Date", &now.format(RFC2822).to_string()));
        email.push_str(&header_line("Message-ID", &message_id));
        email.push_str(&header_line("MIME-Version", "1.0"));
        email.push_str(&header_line(H_VERSION, &TASK_VERSION.to_string()));
        email.push_str(&header_line(H_TYPE, TaskMessageKind::Attachment.as_str()));
        email.push_str(&header_line(H_ID, task_uuid));
        // Repeated from the descriptor below so that the identity listing, which
        // fetches a fixed short set of headers, can sweep orphaned payloads
        // without fetching a descriptor line that carries an arbitrary filename.
        email.push_str(&header_line(H_ATTACHMENT_ID, &descriptor.attachment_uuid));
        email.push_str(&header_line(
            H_ATTACHMENTS,
            &attachment_descriptor(&descriptor),
        ));
        email.push_str(&header_line(
            "Content-Type",
            &format!(
                "{content_type}; name=\"{}\"",
                quotable(&descriptor.filename)
            ),
        ));
        email.push_str(&header_line(
            "Content-Disposition",
            &format!(
                "attachment; filename=\"{}\"",
                quotable(&descriptor.filename)
            ),
        ));
        email.push_str(&header_line("Content-Transfer-Encoding", "base64"));
        email.push_str("\r\n");
        email.push_str(&base64_body(payload));

        Ok((email.into_bytes(), message_id))
    }

    /// Mint a `Message-ID` for a message about to be appended.
    pub fn generate_message_id() -> String {
        format!("<{}.task@orb.invalid>", Uuid::new_v4())
    }
}

/// The `X-Orb-Task-Attachments` value for one attachment.
///
/// `filename` comes last and runs to the end of the line, which is what lets it
/// hold a semicolon, a quote or a space without an escaping scheme: the parser
/// stops splitting once it reaches it. Non-trivial filenames are RFC 2047
/// encoded anyway, so the common case is also a single unambiguous token.
fn attachment_descriptor(attachment: &TaskAttachment) -> String {
    let mut value = attachment.attachment_uuid.clone();
    if let Some(size) = attachment.size {
        value.push_str(&format!("; size={size}"));
    }
    if let Some(content_type) = &attachment.content_type {
        value.push_str(&format!("; type={content_type}"));
    }
    value.push_str(&format!(
        "; filename={}",
        encode_token(&attachment.filename)
    ));
    value
}

/// Headers for one of the structured JSON parts.
fn json_part_headers(part: &str, filename: &str) -> String {
    let mut headers = String::new();
    headers.push_str(&header_line(
        "Content-Type",
        "application/json; charset=\"UTF-8\"",
    ));
    headers.push_str(&header_line("Content-Transfer-Encoding", "base64"));
    headers.push_str(&header_line(
        "Content-Disposition",
        &format!("inline; filename=\"{filename}\""),
    ));
    // The part is found by this header, not by its content type or its
    // filename: a client that re-encodes a message on its way through is
    // allowed to change either of those, and the union merge cannot afford to
    // lose the log because a part got renamed.
    headers.push_str(&header_line(H_PART, part));
    headers.push_str("\r\n");
    headers
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Reads the three kinds of message back.
pub struct TaskEmailParser;

impl TaskEmailParser {
    /// Parse any message from the tasks folder.
    ///
    /// Errors — including a version this build cannot read — mean "skip this
    /// message", never "delete it": the message stays on the server for a build
    /// that does understand it.
    pub fn parse(
        email_bytes: &[u8],
        account_name: &str,
        imap_uid: Option<u32>,
    ) -> Result<TaskMessage> {
        let mail = crate::mime::parse_mail(email_bytes).map_err(crate::Error::Parse)?;
        let kind = check_version_and_kind(&mail.headers)?;

        match kind {
            TaskMessageKind::Task => Ok(TaskMessage::Task(Box::new(parse_task(
                &mail,
                account_name,
                imap_uid,
            )?))),
            TaskMessageKind::TagRegistry => {
                Ok(TaskMessage::TagRegistry(parse_registry(&mail, imap_uid)?))
            }
            TaskMessageKind::Attachment => {
                Ok(TaskMessage::Attachment(parse_attachment(&mail, imap_uid)?))
            }
            TaskMessageKind::ChatMarks => {
                Ok(TaskMessage::ChatMarks(parse_chat_marks(&mail, imap_uid)?))
            }
            TaskMessageKind::TodayCleared => Ok(TaskMessage::TodayCleared(parse_today_cleared(
                &mail, imap_uid,
            )?)),
        }
    }

    /// Parse a task message, rejecting the other two kinds.
    ///
    /// A convenience for the callers that already know what they fetched — a
    /// body fetched because its revision moved, for instance.
    pub fn parse_task(
        email_bytes: &[u8],
        account_name: &str,
        imap_uid: Option<u32>,
    ) -> Result<ParsedTask> {
        match Self::parse(email_bytes, account_name, imap_uid)? {
            TaskMessage::Task(parsed) => Ok(*parsed),
            other => Err(invalid(format!(
                "expected a task message but found a {} message",
                other.kind().as_str()
            ))),
        }
    }

    /// Read identity out of a header-only fetch.
    ///
    /// Takes the bytes of `BODY.PEEK[HEADER.FIELDS ({IDENTITY_HEADERS})]`, which
    /// is a header block with no body at all, so it parses the headers directly
    /// rather than going through a full MIME parse. Whole messages work too.
    pub fn parse_identity(header_bytes: &[u8]) -> Result<TaskIdentity> {
        let (headers, _) = mailparse::parse_headers(header_bytes).map_err(crate::Error::Parse)?;
        let kind = check_version_and_kind(&headers)?;

        let task_uuid = required_id(&headers)?;
        Ok(TaskIdentity {
            kind,
            task_uuid,
            revision: revision_header(&headers),
            attachment_uuid: optional_id(&headers, H_ATTACHMENT_ID)?,
        })
    }
}

/// Check `X-Orb-Task-Version` and classify the message.
///
/// The one gate every parse goes through, so that "version 1 is never partially
/// parsed" is a property of the module rather than a rule each caller has to
/// remember.
fn check_version_and_kind(headers: &[mailparse::MailHeader<'_>]) -> Result<TaskMessageKind> {
    let raw = task_header(headers, H_VERSION)
        .ok_or_else(|| invalid(format!("not a task message: no {H_VERSION} header")))?;
    let version: u8 = raw
        .parse()
        .map_err(|_| invalid(format!("{H_VERSION} is '{raw}', which is not a version")))?;

    if version != TASK_VERSION && version != TASK_VERSION_STEPS {
        // Logged rather than silently dropped: a folder full of unreadable
        // messages is a migration that needs a person, and the only trace of it
        // is this line.
        logger::warn(&format!(
            "Skipping a task message in wire format version {version}; this build reads version \
             {TASK_VERSION} and {TASK_VERSION_STEPS} only. The message is left on the server \
             untouched."
        ));
        return Err(invalid(format!(
            "unsupported task wire format version {version} (this build reads {TASK_VERSION} and \
             {TASK_VERSION_STEPS})"
        )));
    }

    let type_header = task_header(headers, H_TYPE);
    TaskMessageKind::parse(type_header.as_deref()).ok_or_else(|| {
        invalid(format!(
            "unknown {H_TYPE} '{}'",
            type_header.unwrap_or_default()
        ))
    })
}

/// `X-Orb-Task-Id`, which every version 2 message must carry.
fn required_id(headers: &[mailparse::MailHeader<'_>]) -> Result<String> {
    optional_id(headers, H_ID)?
        .ok_or_else(|| invalid(format!("task message has no {H_ID}: it has no identity")))
}

/// `X-Orb-Task-Revision`: 0 when missing, unreadable or outside
/// `0..=MAX_REVISION`. A revision bumps by one per edit, so one beyond the
/// bound was not counted up to; it would pin the task above every edit made
/// here and overflow the next bump.
fn revision_header(headers: &[mailparse::MailHeader<'_>]) -> i64 {
    task_header(headers, H_REVISION)
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|revision| (0..=MAX_REVISION).contains(revision))
        .unwrap_or(0)
}

/// An id header, when present: refused unless it is an identifier
/// ([`is_wire_id`]), because the ids name directories under the attachment
/// cache and a message is written by whoever can reach the folder.
fn optional_id(headers: &[mailparse::MailHeader<'_>], name: &str) -> Result<Option<String>> {
    match task_header(headers, name) {
        Some(id) if is_wire_id(&id) => Ok(Some(id)),
        Some(id) => Err(invalid(format!(
            "{name} '{}' is not an identifier",
            truncate_bytes(&id, 80)
        ))),
        None => Ok(None),
    }
}

fn parse_task(
    mail: &ParsedMail<'_>,
    account_name: &str,
    imap_uid: Option<u32>,
) -> Result<ParsedTask> {
    let headers = &mail.headers;
    let task_uuid = required_id(headers)?;

    let created_at =
        task_header(headers, H_CREATED).unwrap_or_else(|| format_timestamp(Utc::now()));
    let subject = headers.get_first_value("subject").unwrap_or_default();
    // A position that is not a number is no position: NaN cannot be stored,
    // let alone sorted by.
    let position = task_header(headers, H_POSITION)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|position| position.is_finite())
        .unwrap_or(0.0);

    let mut task = Task::bare(
        task_uuid.clone(),
        subject,
        account_name.to_string(),
        position,
        created_at,
    );

    task.revision = revision_header(headers);
    task.updated_at = task_header(headers, H_UPDATED).unwrap_or_else(|| task.created_at.clone());
    task.completed_at = task_header(headers, H_COMPLETED_AT);
    task.is_completed = task.completed_at.is_some();
    task.deleted_at = task_header(headers, H_DELETED_AT);
    // Read once here, as the dates are: a rule this build cannot read is
    // dropped with a line in the log rather than kept to fail on every open,
    // completion and edit of the task.
    task.recurrence =
        task_header(headers, H_RECURRENCE).filter(|rule| match RecurrenceRule::parse(rule) {
            Ok(_) => true,
            Err(error) => {
                logger::warn(&format!(
                    "task {task_uuid}: dropping an unreadable recurrence rule: {error}"
                ));
                false
            }
        });
    task.recurrence_from_completion =
        task_header(headers, H_RECURRENCE_FROM).is_some_and(|value| {
            value
                .trim()
                .eq_ignore_ascii_case(RECURRENCE_FROM_COMPLETION)
        });
    // A series only with a rule, and only one this build can read: the task
    // then repeats by its dates, as one written before there were series.
    task.recurrence_series = task_header(headers, H_RECURRENCE_SERIES)
        .filter(|_| task.recurrence.is_some())
        .filter(|series| match series.parse::<Series>() {
            Ok(_) => true,
            Err(error) => {
                logger::warn(&format!(
                    "task {task_uuid}: dropping an unreadable series: {error}"
                ));
                false
            }
        });
    task.tags = task_header(headers, H_TAGS)
        .map(|value| {
            value
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    task.imap_uid = imap_uid;
    task.message_id = headers.get_first_value("message-id");
    // Straight off the server, so by definition it holds nothing the server has
    // not seen.
    task.is_dirty = false;

    // Dates go through `set_dates` so the `_unix` companions are derived by the
    // one function that knows they are civil days and not instants; a message
    // that carries an unreadable date loses that date rather than the task.
    let postpone = task_header(headers, H_POSTPONE).and_then(|raw| parse_civil_date(&raw));
    let due = task_header(headers, H_DUE).and_then(|raw| parse_civil_date(&raw));
    task.set_dates(TaskDates::new(postpone, due));

    task.log = read_log_part(mail);
    task.steps = read_steps_part(mail);
    task.body_text = read_body_text(mail);

    let reminders = task_header(headers, H_REMINDERS)
        .map(|value| parse_reminders(&value))
        .unwrap_or_default();

    let attachments = headers
        .get_all_values(H_ATTACHMENTS)
        .iter()
        .filter_map(|value| parse_attachment_descriptor(&task_uuid, value))
        .collect();

    Ok(ParsedTask {
        task,
        reminders,
        linked_emails: read_links_part(mail),
        attachments,
    })
}

fn parse_registry(mail: &ParsedMail<'_>, imap_uid: Option<u32>) -> Result<ParsedTagRegistry> {
    let revision = revision_header(&mail.headers);
    let body = mail
        .get_body_raw()
        .map_err(|error| invalid(format!("cannot decode the tag registry body: {error}")))?;
    let wire: Vec<WireTag> = serde_json::from_slice(&body)
        .map_err(|error| invalid(format!("cannot read the tag registry: {error}")))?;

    Ok(ParsedTagRegistry {
        revision,
        tags: wire.into_iter().map(WireTag::into_tag).collect(),
        message_id: mail.headers.get_first_value("message-id"),
        imap_uid,
    })
}

fn parse_chat_marks(mail: &ParsedMail<'_>, imap_uid: Option<u32>) -> Result<ParsedChatMarks> {
    let revision = revision_header(&mail.headers);
    let body = mail
        .get_body_raw()
        .map_err(|error| invalid(format!("cannot decode the chat marks: {error}")))?;
    let marks = serde_json::from_slice(&body)
        .map_err(|error| invalid(format!("cannot read the chat marks: {error}")))?;
    Ok(ParsedChatMarks {
        revision,
        marks,
        message_id: mail.headers.get_first_value("message-id"),
        imap_uid,
    })
}

fn parse_today_cleared(mail: &ParsedMail<'_>, imap_uid: Option<u32>) -> Result<ParsedTodayCleared> {
    #[derive(Deserialize)]
    struct Wire {
        cleared_at: Option<String>,
    }
    let revision = revision_header(&mail.headers);
    let body = mail
        .get_body_raw()
        .map_err(|error| invalid(format!("cannot decode the Today cut-off: {error}")))?;
    let wire: Wire = serde_json::from_slice(&body)
        .map_err(|error| invalid(format!("cannot read the Today cut-off: {error}")))?;
    Ok(ParsedTodayCleared {
        revision,
        cleared_at: wire
            .cleared_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc)),
        message_id: mail.headers.get_first_value("message-id"),
        imap_uid,
    })
}

fn parse_attachment(mail: &ParsedMail<'_>, imap_uid: Option<u32>) -> Result<ParsedAttachment> {
    let task_uuid = required_id(&mail.headers)?;
    let descriptor = mail
        .headers
        .get_all_values(H_ATTACHMENTS)
        .first()
        .and_then(|value| parse_attachment_descriptor(&task_uuid, value))
        .ok_or_else(|| {
            invalid(format!(
                "attachment message for task {task_uuid} has no readable {H_ATTACHMENTS} \
                 descriptor"
            ))
        })?;

    // The payload part is the root unless something re-wrapped the message in a
    // multipart on its way through, in which case it is the first subpart.
    let part = if mail.subparts.is_empty() {
        mail
    } else {
        &mail.subparts[0]
    };
    let payload = part
        .get_body_raw()
        .map_err(|error| invalid(format!("cannot decode the attachment payload: {error}")))?;

    let mut attachment = descriptor;
    attachment.imap_uid = imap_uid;
    attachment.message_id = mail.headers.get_first_value("message-id");
    // What the message actually contains beats what its descriptor claims.
    attachment.size = Some(payload.len() as i64);

    Ok(ParsedAttachment {
        task_uuid,
        attachment,
        payload,
    })
}

/// Read one `X-Orb-Task-Attachments` descriptor.
///
/// `filename` is the last parameter and takes the rest of the line, so a
/// filename containing the separator cannot truncate the descriptor. Returns
/// `None` for a descriptor whose uuid is missing or not an identifier
/// ([`is_wire_id`]): a payload that cannot be named cannot be matched to its
/// message either, and one named as a path must not be.
fn parse_attachment_descriptor(task_uuid: &str, value: &str) -> Option<TaskAttachment> {
    let value = value.trim();
    let (uuid, mut rest) = match value.split_once(';') {
        Some((uuid, rest)) => (uuid.trim(), rest),
        None => (value, ""),
    };
    if !is_wire_id(uuid) {
        logger::warn(&format!(
            "task {task_uuid}: dropping an attachment descriptor whose id '{}' is not an \
             identifier",
            truncate_bytes(uuid, 80)
        ));
        return None;
    }

    let mut filename = None;
    let mut content_type = None;
    let mut size = None;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let Some((key, tail)) = trimmed.split_once('=') else {
            break;
        };
        let key = key.trim();
        if key.eq_ignore_ascii_case("filename") {
            filename = Some(decode_token(tail.trim()));
            break;
        }
        let (param, next) = match tail.split_once(';') {
            Some((param, next)) => (param, next),
            None => (tail, ""),
        };
        match key.to_ascii_lowercase().as_str() {
            "type" => content_type = sanitise_content_type(param.trim()),
            "size" => size = param.trim().parse::<i64>().ok(),
            _ => {}
        }
        rest = next;
    }

    let filename = filename.filter(|name| !name.is_empty()).unwrap_or_else(|| {
        // A descriptor with no filename still describes a real file; naming it
        // after its uuid keeps it downloadable instead of dropping it.
        format!("{uuid}.bin")
    });

    Some(TaskAttachment {
        id: None,
        task_id: 0,
        file_path: TaskAttachment::cache_path(task_uuid, uuid, &filename),
        attachment_uuid: uuid.to_string(),
        filename,
        content_type,
        size,
        imap_uid: None,
        message_id: None,
        is_cached: false,
    })
}

/// A content type off the wire, or `None` if it is not one.
///
/// A descriptor's content type is written straight back out — into this task's
/// next message, and into the `Content-Type` of the payload message — so it is a
/// remote string that ends up in a structured header. Restricting it to the
/// characters a MIME type is made of means it cannot carry a parameter, a quote
/// or anything else that changes the shape of the header it lands in.
fn sanitise_content_type(value: &str) -> Option<String> {
    let ok = !value.is_empty()
        && value.len() <= 127
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '+' | '.' | '_'));
    ok.then(|| value.to_ascii_lowercase())
}

/// Reminders from the semicolon-separated header.
fn parse_reminders(value: &str) -> Vec<TaskReminder> {
    value
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| match DateTime::parse_from_rfc3339(entry) {
            Ok(instant) => Some(TaskReminder::new(0, entry.to_string(), instant.timestamp())),
            Err(_) => {
                // One unreadable reminder must not cost the task its other
                // reminders, and it is not worth failing the whole message over.
                logger::warn(&format!("Ignoring unreadable task reminder '{entry}'"));
                None
            }
        })
        .collect()
}

/// The notes, with the transport's CRLF line endings undone.
fn read_body_text(mail: &ParsedMail<'_>) -> Option<String> {
    let part = find_part(mail, |part| {
        part_marker(part).is_none() && part.ctype.mimetype.eq_ignore_ascii_case("text/plain")
    })?;
    let text = part.get_body().ok()?;
    let text = text.replace("\r\n", "\n");
    (!text.is_empty()).then_some(text)
}

/// The log part (§11), or an empty log if the message has none.
fn read_log_part(mail: &ParsedMail<'_>) -> Vec<TaskLogEntry> {
    let Some(part) = find_part(mail, |part| part_marker(part).as_deref() == Some(PART_LOG)) else {
        return Vec::new();
    };
    let Ok(raw) = part.get_body_raw() else {
        return Vec::new();
    };
    match serde_json::from_slice::<Vec<WireLogEntry>>(&raw) {
        Ok(entries) => entries.into_iter().map(WireLogEntry::into_entry).collect(),
        Err(error) => {
            // A log that cannot be read is history this device will not be able
            // to re-upload, so it is worth a line in the log file even though
            // the task itself is still perfectly usable.
            logger::warn(&format!("Cannot read the task log part: {error}"));
            Vec::new()
        }
    }
}

/// The linked-message part (§6), or nothing.
fn read_links_part(mail: &ParsedMail<'_>) -> Vec<TaskLinkedEmail> {
    let Some(part) = find_part(mail, |part| {
        part_marker(part).as_deref() == Some(PART_LINKS)
    }) else {
        return Vec::new();
    };
    let Ok(raw) = part.get_body_raw() else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<WireLink>>(&raw)
        .map(|links| links.into_iter().map(WireLink::into_link).collect())
        .unwrap_or_default()
}

/// The steps part (§17), or none.
fn read_steps_part(mail: &ParsedMail<'_>) -> Vec<crate::tasks::TaskStep> {
    let Some(part) = find_part(mail, |part| {
        part_marker(part).as_deref() == Some(PART_STEPS)
    }) else {
        return Vec::new();
    };
    let Ok(raw) = part.get_body_raw() else {
        return Vec::new();
    };
    serde_json::from_slice(&raw).unwrap_or_default()
}

/// The `X-Orb-Task-Part` marker on a MIME part, if it has one.
fn part_marker(part: &ParsedMail<'_>) -> Option<String> {
    part.headers
        .get_first_value(H_PART)
        .map(|value| value.trim().to_ascii_lowercase())
}

/// The first part satisfying `predicate`, searching the root and its subparts.
fn find_part<'a>(
    mail: &'a ParsedMail<'a>,
    predicate: impl Fn(&ParsedMail<'a>) -> bool + Copy,
) -> Option<&'a ParsedMail<'a>> {
    if mail.subparts.is_empty() {
        return predicate(mail).then_some(mail);
    }
    mail.subparts
        .iter()
        .find_map(|part| find_part(part, predicate))
}

/// A task header's value, trimmed, with empty treated as absent.
///
/// Reads through `mailparse`, so a folded header arrives unfolded and any
/// RFC 2047 encoded words in it are already decoded.
fn task_header(headers: &[mailparse::MailHeader<'_>], name: &str) -> Option<String> {
    debug_assert!(name.starts_with(HEADER_PREFIX));
    headers
        .get_first_value(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn invalid(message: String) -> crate::Error {
    crate::Error::InvalidInput(message)
}

// ---------------------------------------------------------------------------
// The JSON parts
// ---------------------------------------------------------------------------

/// One log entry as it appears in the log part.
///
/// `detail` is a *string*, not a nested object, and that is the whole point: an
/// entry written by a build that knows an event type this one does not has to
/// come back out byte for byte, or the union merge of §11 turns into a slow leak
/// of the other machine's history. Round-tripping it through a typed JSON value
/// would reorder its keys and make two builds disagree about an entry neither of
/// them changed.
#[derive(Debug, Serialize, Deserialize)]
struct WireLogEntry {
    entry_uuid: String,
    occurred_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
    event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// The rendered timeline text. Written for anything reading this folder that
    /// is not Orb, and ignored on the way back in: the text is derived from the
    /// event, so an entry written by an older build renders in this build's
    /// wording rather than being frozen at the moment it was recorded.
    #[serde(default)]
    text: String,
}

impl WireLogEntry {
    fn from_entries(entries: &[TaskLogEntry]) -> Vec<Self> {
        entries
            .iter()
            .map(|entry| Self {
                entry_uuid: entry.entry_uuid.clone(),
                occurred_at: entry.occurred_at.clone(),
                device_id: entry.device_id.clone(),
                event_type: entry.event_type().to_string(),
                detail: entry.detail_json(),
                text: entry.rendered_text(),
            })
            .collect()
    }

    fn into_entry(self) -> TaskLogEntry {
        // Derived rather than carried: the timestamp is the sort key of the
        // merged log, and two representations of it on the wire could disagree.
        let occurred_at_unix = DateTime::parse_from_rfc3339(&self.occurred_at)
            .map(|instant| instant.timestamp())
            .unwrap_or(0);
        TaskLogEntry {
            id: None,
            task_id: None,
            event: TaskEvent::from_parts(&self.event_type, self.detail.as_deref()),
            entry_uuid: self.entry_uuid,
            occurred_at: self.occurred_at,
            occurred_at_unix,
            device_id: self.device_id,
        }
    }
}

/// One linked message and the metadata cached with it.
#[derive(Debug, Serialize, Deserialize)]
struct WireLink {
    message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    /// Where the message was last seen; the server search of §6.2 tries it
    /// first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
}

impl WireLink {
    fn from_links(links: &[TaskLinkedEmail]) -> Vec<Self> {
        links
            .iter()
            .map(|link| Self {
                message_id: link.message_id.clone(),
                subject: link.cached_subject.clone(),
                from: link.cached_from.clone(),
                date: link.cached_date.clone(),
                account: link.cached_account.clone(),
                folder: link.cached_folder.clone(),
            })
            .collect()
    }

    fn into_link(self) -> TaskLinkedEmail {
        TaskLinkedEmail {
            id: None,
            task_id: 0,
            message_id: self.message_id,
            cached_subject: self.subject,
            cached_from: self.from,
            cached_date: self.date,
            cached_account: self.account,
            cached_folder: self.folder,
        }
    }
}

/// One tag in the registry message.
#[derive(Debug, Serialize, Deserialize)]
struct WireTag {
    tag_uuid: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    colour: Option<String>,
    #[serde(default)]
    position: f64,
}

impl WireTag {
    fn from_tags(tags: &[TaskTag]) -> Vec<Self> {
        tags.iter()
            .map(|tag| Self {
                tag_uuid: tag.tag_uuid.clone(),
                name: tag.name.clone(),
                colour: tag.colour.clone(),
                position: tag.position,
            })
            .collect()
    }

    fn into_tag(self) -> TaskTag {
        TaskTag {
            id: None,
            tag_uuid: self.tag_uuid,
            name: self.name,
            colour: self.colour,
            position: self.position,
        }
    }
}

// ---------------------------------------------------------------------------
// Header encoding
// ---------------------------------------------------------------------------

/// RFC 5322 §2.1.1 recommends no more than 78 characters per line.
const MAX_LINE: usize = 78;
/// RFC 2047 §2: an encoded word may not exceed 75 characters, *including* its
/// charset, encoding and delimiters.
const MAX_ENCODED_WORD: usize = 75;
/// `=?UTF-8?Q?` … `?=`.
const ENCODED_WORD_OVERHEAD: usize = 12;
const ENCODED_WORD_PREFIX: &str = "=?UTF-8?Q?";

/// The `Date` header's format.
const RFC2822: &str = "%a, %d %b %Y %H:%M:%S %z";

/// Render one header field, encoded and folded, terminated by CRLF.
///
/// Long values are the interesting case, and the old hand-rolled encoder got
/// both halves of it wrong: it emitted one unbounded encoded word, so a long
/// unicode title produced a line far past the 998-octet hard limit — which
/// servers and intermediaries are entitled to reject or mangle, and a mangled
/// `Subject` is a task that comes back from the server with a corrupted title.
/// This folds instead: ASCII values at whitespace, non-ASCII values into a
/// sequence of encoded words, each split on a *character* boundary so every word
/// decodes on its own.
pub(crate) fn header_line(name: &str, value: &str) -> String {
    let mut line = if needs_encoding(value) {
        fold_encoded(name, value)
    } else {
        fold_ascii(name, value)
    };
    line.push_str("\r\n");
    line
}

/// Does this value have to be sent as encoded words?
///
/// Non-ASCII and control characters obviously do. So does a value that merely
/// *contains* `=?`: left alone it would be decoded as an encoded word on the way
/// back in, so a task titled `=?UTF-8?Q?x?=` would come back titled `x`.
fn needs_encoding(value: &str) -> bool {
    value.contains("=?") || value.chars().any(|c| !c.is_ascii() || c.is_ascii_control())
}

/// Fold a plain ASCII value at whitespace.
///
/// Only a single space with non-space neighbours is a fold point, because
/// unfolding replaces the fold with exactly one space (RFC 5322 §2.2.3): folding
/// anywhere else would silently rewrite the value's whitespace, and a task title
/// that loses a space every time it is uploaded is a task title that decays.
fn fold_ascii(name: &str, value: &str) -> String {
    let points = fold_points(value);
    let mut out = format!("{name}: ");
    let mut column = out.len();
    let mut start = 0;

    while start < value.len() {
        let remaining = value.len() - start;
        if column + remaining <= MAX_LINE {
            break;
        }
        // The last fold point that fits, or — when nothing fits — the first one
        // past the limit, which overshoots by less than the alternative of not
        // folding at all.
        let candidates = points.iter().copied().filter(|point| *point > start);
        let fit = candidates
            .clone()
            .take_while(|point| column + (*point - start) <= MAX_LINE)
            .last();
        let Some(point) = fit.or_else(|| candidates.clone().next()) else {
            break;
        };
        out.push_str(&value[start..point]);
        out.push_str("\r\n ");
        column = 1;
        start = point + 1;
    }

    out.push_str(&value[start..]);
    out
}

/// Byte offsets of the spaces this value may legally be folded at.
fn fold_points(value: &str) -> Vec<usize> {
    let bytes = value.as_bytes();
    (1..bytes.len().saturating_sub(1))
        .filter(|index| {
            bytes[*index] == b' ' && bytes[index - 1] != b' ' && bytes[index + 1] != b' '
        })
        .collect()
}

/// Fold a value into RFC 2047 encoded words, one per line.
///
/// Whitespace between two adjacent encoded words is discarded by the decoder
/// (RFC 2047 §6.2), which is what makes the fold invisible: the words reassemble
/// into the original string with no separator inserted.
fn fold_encoded(name: &str, value: &str) -> String {
    let first_budget = MAX_LINE
        .saturating_sub(name.len() + 2 + ENCODED_WORD_OVERHEAD)
        .min(MAX_ENCODED_WORD - ENCODED_WORD_OVERHEAD);
    let rest_budget = MAX_LINE
        .saturating_sub(1 + ENCODED_WORD_OVERHEAD)
        .min(MAX_ENCODED_WORD - ENCODED_WORD_OVERHEAD);

    let words = encode_words(value, first_budget, rest_budget);
    format!("{name}: {}", words.join("\r\n "))
}

/// Q-encode `value` into encoded words no longer than the given budgets.
///
/// The split is on character boundaries, never on byte boundaries: an encoded
/// word has to be decodable on its own, so half of a `ä` in one word and half in
/// the next produces two undecodable words and, in a lenient decoder, a
/// replacement character where a letter used to be.
fn encode_words(value: &str, first_budget: usize, rest_budget: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut payload = String::new();
    let mut budget = first_budget.max(4);

    for c in value.chars() {
        let encoded = q_encode_char(c);
        // A single character can be wider than the budget (a 4-byte emoji costs
        // 12 characters); it still has to go somewhere, so it goes in a word of
        // its own rather than looping forever.
        if !payload.is_empty() && payload.len() + encoded.len() > budget {
            words.push(format!("{ENCODED_WORD_PREFIX}{payload}?="));
            payload.clear();
            budget = rest_budget.max(4);
        }
        payload.push_str(&encoded);
    }
    if !payload.is_empty() || words.is_empty() {
        words.push(format!("{ENCODED_WORD_PREFIX}{payload}?="));
    }
    words
}

/// Q-encode one character.
///
/// Deliberately conservative: everything outside the alphanumerics and a handful
/// of unambiguous symbols is escaped, so an encoded word is a single token that
/// is legal in every context this module puts one in — an unstructured `Subject`
/// and a parameter of `X-Orb-Task-Attachments` alike. A less strict rule would
/// need a different encoder per context, which is one more thing to get wrong on
/// the one filename that contains a semicolon.
fn q_encode_char(c: char) -> String {
    if c == ' ' {
        return "_".to_string();
    }
    if c.is_ascii_alphanumeric() || matches!(c, '!' | '*' | '+' | '-' | '/') {
        return c.to_string();
    }
    let mut buffer = [0u8; 4];
    c.encode_utf8(&mut buffer)
        .as_bytes()
        .iter()
        .map(|byte| format!("={byte:02X}"))
        .collect()
}

/// A filename as it appears in a descriptor: either a bare token or one encoded
/// word.
///
/// Encoding anything that is not plainly a filename keeps the descriptor a
/// sequence of tokens, so the common case stays readable and the awkward case —
/// a name with a space, a quote or a semicolon in it — cannot be misread as the
/// end of a parameter.
fn encode_token(value: &str) -> String {
    let simple = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'));
    if simple {
        return value.to_string();
    }
    // One word, never folded. Folding here would be worse than a long line: the
    // first word follows `filename=` and so is invisible to an RFC 2047 decoder,
    // while a second word after the fold *would* be decoded, and the two halves
    // would come back in different states.
    //
    // The length is bounded instead, at the 255 bytes no filesystem exceeds, so
    // the worst case (every byte escaped, three characters each) still leaves
    // the line far inside RFC 5322's 998-octet hard limit. A longer name than
    // that could not have come off a disk, and a truncated label beats an
    // unparseable descriptor.
    encode_words(&truncate_bytes(value, 255), usize::MAX, usize::MAX).join("")
}

/// The longest prefix of `value` that fits in `limit` bytes without splitting a
/// character.
fn truncate_bytes(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Undo [`encode_token`].
///
/// `mailparse` cannot do this one: RFC 2047 only recognises an encoded word
/// that begins a value or follows whitespace, and this one follows the `=` of
/// `filename=`. Decoding it here rather than emitting `filename= =?…?=` keeps
/// the descriptor a sequence of unambiguous tokens — a space in there would put
/// the header's meaning at the mercy of how an intermediary refolds it.
///
/// Undecodable input comes back verbatim: a filename is a label, and a label
/// nobody can read still beats a download that cannot be named.
fn decode_token(value: &str) -> String {
    if !value.starts_with("=?") {
        return value.to_string();
    }
    Rfc2047Decoder::new()
        .too_long_encoded_word_strategy(RecoverStrategy::Decode)
        .decode(value.as_bytes())
        .unwrap_or_else(|_| value.to_string())
}

/// A value safe to put inside a quoted string, for the MIME headers other mail
/// software reads.
fn quotable(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '"' | '\\' => '_',
            c if c.is_ascii() => c,
            // Non-ASCII in a `filename=` parameter needs RFC 2231, which nothing
            // here reads back: Orb takes the name from its own descriptor
            // header, and this copy exists only so a generic mail client shows
            // something sensible.
            _ => '_',
        })
        .collect()
}

/// Convert a date string to the `Date` header's format.
fn format_rfc2822_date(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|instant| instant.format(RFC2822).to_string())
        .unwrap_or_else(|_| Utc::now().format(RFC2822).to_string())
}

/// Render a position without an exponent or a trailing `.0`, so it parses back
/// to the same `f64`.
fn format_position(position: f64) -> String {
    if position.is_finite() {
        format!("{position}")
    } else {
        "0".to_string()
    }
}

/// Normalise text to the CRLF line endings MIME asks for.
fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// A MIME boundary that cannot occur in the content it delimits.
fn generate_boundary() -> String {
    // The uuid makes a collision with base64 content impossible in practice, and
    // the prefix makes the message readable to a person looking at raw source.
    format!("----=_Orb_{}", Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// RFC 2045 §6.8 caps an encoded line at 76 characters.
const BASE64_LINE: usize = 76;

/// Encode to base64, no line breaks.
///
/// Hand-rolled because the crate has no base64 dependency and this is twenty
/// lines; `mailparse` handles the decoding.
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[(triple >> 18 & 0x3F) as usize] as char);
        out.push(BASE64_ALPHABET[(triple >> 12 & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[(triple >> 6 & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A part body: base64 in 76-character lines, ending in a CRLF so the next MIME
/// boundary starts on its own line.
pub(crate) fn base64_body(data: &[u8]) -> String {
    let encoded = base64_encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / BASE64_LINE * 2 + 2);
    for chunk in encoded.as_bytes().chunks(BASE64_LINE) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\r\n");
    }
    if encoded.is_empty() {
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::model::{CachedEmailMetadata, ChangeContext, DateField, civil_day_unix};
    use super::*;
    use chrono::NaiveDate;

    fn context() -> ChangeContext<'static> {
        ChangeContext::new(
            DateTime::parse_from_rfc3339("2026-08-06T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            NaiveDate::from_ymd_opt(2026, 8, 6).unwrap(),
        )
        .with_device_id(Some("device-a"))
    }

    fn sample_task() -> Task {
        Task::create(
            "Buy milk".to_string(),
            "Work".to_string(),
            1024.0,
            &context(),
        )
        .task
    }

    fn build_and_parse(task: &Task) -> ParsedTask {
        let (bytes, _) = TaskEmailBuilder::build(task, &[], &[], &[]).unwrap();
        TaskEmailParser::parse_task(&bytes, "Work", Some(7)).unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-06T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    // -- version gate ------------------------------------------------------

    #[test]
    fn version_two_is_written() {
        let (bytes, _) = TaskEmailBuilder::build(&sample_task(), &[], &[], &[]).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("X-Orb-Task-Version: 2\r\n"));
        assert!(text.contains("X-Orb-Task-Type: task\r\n"));
    }

    #[test]
    fn version_one_is_rejected_outright() {
        // A version 1 message: no task id at all, and dates under the old
        // vocabulary. Parsing any of it would mint a duplicate task.
        let v1 = b"From: tasks@orb.invalid\r\n\
                   Subject: Old task\r\n\
                   X-Orb-Task-Version: 1\r\n\
                   X-Orb-Task-Snooze-Until: 2025-12-31\r\n\
                   Content-Type: text/plain\r\n\r\n\
                   body";
        let error = TaskEmailParser::parse(v1, "Work", Some(1)).unwrap_err();
        let message = error.to_string();
        assert!(message.contains('1'), "{message}");
        assert!(message.contains('2'), "{message}");
    }

    #[test]
    fn a_message_without_the_version_header_is_not_a_task() {
        let mail = b"From: a@example.com\r\nSubject: Hello\r\n\r\nBody";
        assert!(TaskEmailParser::parse(mail, "Work", None).is_err());
    }

    #[test]
    fn an_unreadable_version_is_rejected() {
        let mail = b"X-Orb-Task-Version: two\r\nX-Orb-Task-Id: x\r\n\r\n";
        assert!(TaskEmailParser::parse(mail, "Work", None).is_err());
    }

    #[test]
    fn a_task_message_without_an_id_is_rejected() {
        let mail = b"X-Orb-Task-Version: 2\r\nSubject: No identity\r\n\r\n";
        assert!(TaskEmailParser::parse(mail, "Work", None).is_err());
    }

    /// The id names a directory under the attachment cache, so a message whose
    /// id is a path is no task at all — refused whole, by both parsers, and
    /// left on the server as any unreadable message is.
    #[test]
    fn a_task_message_whose_id_is_a_path_is_rejected() {
        for id in ["../../..", "..", "a/b", "a\\b", "a.b", "x y", "é"] {
            let mail =
                format!("X-Orb-Task-Version: 2\r\nX-Orb-Task-Id: {id}\r\nSubject: s\r\n\r\n");
            let parsed = TaskEmailParser::parse(mail.as_bytes(), "Work", None);
            assert!(parsed.is_err(), "{id:?} was accepted");
            let identity = TaskEmailParser::parse_identity(mail.as_bytes());
            assert!(identity.is_err(), "{id:?} was accepted as an identity");
        }
    }

    /// A revision beyond what edits can count up to is no revision: taken as
    /// written, it would pin the task above every edit made here and overflow
    /// the next bump.
    #[test]
    fn a_revision_beyond_the_bound_reads_as_zero() {
        for (raw, expected) in [
            ("7", 7),
            ("0", 0),
            ("-1", 0),
            ("abc", 0),
            (&i64::MAX.to_string(), 0),
            (&(MAX_REVISION + 1).to_string(), 0),
            (&MAX_REVISION.to_string(), MAX_REVISION),
        ] {
            let mail = format!(
                "X-Orb-Task-Version: 2\r\nX-Orb-Task-Id: task-a\r\nX-Orb-Task-Revision: {raw}\r\n\
                 Subject: s\r\n\r\n"
            );
            let identity = TaskEmailParser::parse_identity(mail.as_bytes()).unwrap();
            assert_eq!(identity.revision, expected, "{raw}");
            let TaskMessage::Task(parsed) =
                TaskEmailParser::parse(mail.as_bytes(), "Work", None).unwrap()
            else {
                panic!("a task");
            };
            assert_eq!(parsed.task.revision, expected, "{raw}");
        }
    }

    /// `NaN` parses as a number and cannot be stored as one.
    #[test]
    fn a_position_that_is_not_a_finite_number_reads_as_zero() {
        for raw in ["NaN", "inf", "-inf", "x"] {
            let mail = format!(
                "X-Orb-Task-Version: 2\r\nX-Orb-Task-Id: task-a\r\nX-Orb-Task-Position: {raw}\r\n\
                 Subject: s\r\n\r\n"
            );
            let TaskMessage::Task(parsed) =
                TaskEmailParser::parse(mail.as_bytes(), "Work", None).unwrap()
            else {
                panic!("a task");
            };
            assert_eq!(parsed.task.position, 0.0, "{raw}");
        }
    }

    #[test]
    fn an_attachment_message_whose_attachment_id_is_a_path_is_rejected() {
        let mail = b"X-Orb-Task-Version: 2\r\nX-Orb-Task-Type: attachment\r\n\
                     X-Orb-Task-Id: task-a\r\nX-Orb-Task-Attachment-Id: ../../x\r\n\r\n";
        assert!(TaskEmailParser::parse_identity(mail).is_err());
    }

    /// A descriptor naming its payload as a path is dropped; the task itself
    /// is kept, since nothing else about it is wrong.
    #[test]
    fn an_attachment_descriptor_whose_id_is_a_path_is_dropped() {
        let mail = b"X-Orb-Task-Version: 2\r\nX-Orb-Task-Id: task-a\r\nSubject: s\r\n\
                     X-Orb-Task-Attachments: ../../../Library/LaunchAgents/com.x; size=1; \
                     filename=agent.plist\r\n\
                     X-Orb-Task-Attachments: att-1; size=1; filename=notes.txt\r\n\r\n";
        let TaskMessage::Task(parsed) = TaskEmailParser::parse(mail, "Work", None).unwrap() else {
            panic!("a task");
        };
        let ids: Vec<&str> = parsed
            .attachments
            .iter()
            .map(|a| a.attachment_uuid.as_str())
            .collect();
        assert_eq!(ids, vec!["att-1"]);
        assert_eq!(
            parsed.attachments[0].file_path,
            "tasks/task-a/att-1-notes.txt"
        );
    }

    // -- task round trip ---------------------------------------------------

    #[test]
    fn a_task_round_trips_through_its_message() {
        let mut task = sample_task();
        task.revision = 7;
        task.body_text = Some("Two litres.\nSemi-skimmed.".to_string());
        task.postpone_date = Some("2026-08-10".to_string());
        task.due_date = Some("2026-08-12".to_string());
        task.postpone_date_unix = Some(civil_day_unix(
            NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
        ));
        task.due_date_unix = Some(civil_day_unix(
            NaiveDate::from_ymd_opt(2026, 8, 12).unwrap(),
        ));
        task.tags = vec!["tag-a".to_string(), "tag-b".to_string()];
        task.recurrence = Some("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO".to_string());
        task.recurrence_from_completion = true;
        task.recurrence_series =
            Some("day=2026-08-10; due=2; reminders=-1T18:00,0T09:00".to_string());
        task.position = 2048.5;

        let parsed = build_and_parse(&task);

        assert_eq!(parsed.task.task_uuid, task.task_uuid);
        assert_eq!(parsed.task.revision, 7);
        assert_eq!(parsed.task.subject, "Buy milk");
        assert_eq!(
            parsed.task.body_text.as_deref(),
            Some("Two litres.\nSemi-skimmed.")
        );
        assert_eq!(parsed.task.postpone_date.as_deref(), Some("2026-08-10"));
        assert_eq!(parsed.task.due_date.as_deref(), Some("2026-08-12"));
        assert_eq!(parsed.task.postpone_date_unix, task.postpone_date_unix);
        assert_eq!(parsed.task.due_date_unix, task.due_date_unix);
        assert_eq!(parsed.task.tags, vec!["tag-a", "tag-b"]);
        assert_eq!(
            parsed.task.recurrence.as_deref(),
            Some("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO")
        );
        assert!(parsed.task.recurrence_from_completion);
        assert_eq!(parsed.task.recurrence_series, task.recurrence_series);
        assert_eq!(parsed.task.position, 2048.5);
        assert_eq!(parsed.task.created_at, task.created_at);
        assert_eq!(parsed.task.updated_at, task.updated_at);
        assert_eq!(parsed.task.account_name, "Work");
        assert_eq!(parsed.task.imap_uid, Some(7));
        assert!(!parsed.task.is_dirty);
    }

    /// A rule the build cannot read is dropped on arrival, as an unreadable
    /// date is, rather than kept to fail every open of the task.
    #[test]
    fn a_series_is_read_only_with_a_rule_and_only_when_it_can_be() {
        let message = |extra: &str| {
            format!("X-Orb-Task-Version: 2\r\nX-Orb-Task-Id: task-a\r\n{extra}Subject: s\r\n\r\n")
        };
        let series = |extra: &str| {
            let TaskMessage::Task(parsed) =
                TaskEmailParser::parse(message(extra).as_bytes(), "Work", None).unwrap()
            else {
                panic!("a task");
            };
            parsed.task.recurrence_series
        };
        let rule = "X-Orb-Task-Recurrence: FREQ=WEEKLY\r\n";
        assert_eq!(
            series(&format!(
                "{rule}X-Orb-Task-Recurrence-Series: day=2026-08-10; due=2\r\n"
            ))
            .as_deref(),
            Some("day=2026-08-10; due=2")
        );
        assert_eq!(
            series("X-Orb-Task-Recurrence-Series: day=2026-08-10\r\n"),
            None
        );
        assert_eq!(
            series(&format!(
                "{rule}X-Orb-Task-Recurrence-Series: day=someday\r\n"
            )),
            None
        );
        assert_eq!(series(rule), None);
    }

    #[test]
    fn an_unreadable_recurrence_rule_is_dropped_on_arrival() {
        for rule in ["FREQ=WEEKLY;BYDAY=aéa", "FREQ=NEVER", "x"] {
            let mail = format!(
                "X-Orb-Task-Version: 2\r\nX-Orb-Task-Id: task-a\r\nX-Orb-Task-Recurrence: {rule}\r\n\
                 Subject: s\r\n\r\n"
            );
            let TaskMessage::Task(parsed) =
                TaskEmailParser::parse(mail.as_bytes(), "Work", None).unwrap()
            else {
                panic!("a task");
            };
            assert!(parsed.task.recurrence.is_none(), "{rule}");
        }
        let mail = b"X-Orb-Task-Version: 2\r\nX-Orb-Task-Id: task-a\r\n\
                     X-Orb-Task-Recurrence: FREQ=WEEKLY;BYDAY=MO\r\nSubject: s\r\n\r\n";
        let TaskMessage::Task(parsed) = TaskEmailParser::parse(mail, "Work", None).unwrap() else {
            panic!("a task");
        };
        assert_eq!(
            parsed.task.recurrence.as_deref(),
            Some("FREQ=WEEKLY;BYDAY=MO")
        );
    }

    #[test]
    fn an_empty_task_round_trips() {
        let parsed = build_and_parse(&sample_task());
        assert_eq!(parsed.task.body_text, None);
        assert!(parsed.task.postpone_date.is_none());
        assert!(parsed.task.due_date.is_none());
        assert!(parsed.task.tags.is_empty());
        assert!(parsed.task.recurrence.is_none());
        assert!(!parsed.task.recurrence_from_completion);
        assert!(parsed.reminders.is_empty());
        assert!(parsed.linked_emails.is_empty());
        assert!(parsed.attachments.is_empty());
    }

    #[test]
    fn completion_travels_as_a_single_timestamp() {
        let mut task = sample_task();
        task.is_completed = true;
        task.completed_at = Some("2026-08-06T11:00:00+00:00".to_string());

        let parsed = build_and_parse(&task);
        assert!(parsed.task.is_completed);
        assert_eq!(
            parsed.task.completed_at.as_deref(),
            Some("2026-08-06T11:00:00+00:00")
        );
    }

    #[test]
    fn the_soft_delete_travels() {
        let mut task = sample_task();
        task.deleted_at = Some("2026-08-06T11:00:00+00:00".to_string());

        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        assert!(
            String::from_utf8(bytes.clone())
                .unwrap()
                .contains("X-Orb-Task-Deleted-At: 2026-08-06T11:00:00+00:00")
        );

        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        assert_eq!(
            parsed.task.deleted_at.as_deref(),
            Some("2026-08-06T11:00:00+00:00")
        );
    }

    #[test]
    fn a_postpone_date_after_the_due_date_travels_as_it_is() {
        let mut task = sample_task();
        task.postpone_date = Some("2026-09-01".to_string());
        task.due_date = Some("2026-08-12".to_string());

        let parsed = build_and_parse(&task);
        assert_eq!(parsed.task.postpone_date.as_deref(), Some("2026-09-01"));
        assert_eq!(parsed.task.due_date.as_deref(), Some("2026-08-12"));
    }

    #[test]
    fn every_edit_gets_its_own_message_id() {
        let task = sample_task();
        let (_, first) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        let (_, second) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        assert_ne!(
            first, second,
            "a rewritten task must not reuse the Message-ID of the message it supersedes"
        );
    }

    #[test]
    fn local_state_stays_local() {
        let mut task = sample_task();
        task.is_working_on = true;
        task.is_dirty = true;
        task.modseq = Some(99);
        task.id = Some(42);

        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("Working-On"));
        assert!(!text.contains("Dirty"));
        assert!(!text.contains("Modseq"));
    }

    // -- reminders and links ----------------------------------------------

    #[test]
    fn reminders_round_trip_without_their_local_firing_state() {
        let reminders = vec![
            TaskReminder::new(0, "2026-08-10T09:00:00+00:00".to_string(), 0),
            {
                let mut fired = TaskReminder::new(0, "2026-08-12T14:30:00+00:00".to_string(), 0);
                fired.is_triggered = true;
                fired
            },
        ];

        let (bytes, _) = TaskEmailBuilder::build(&sample_task(), &reminders, &[], &[]).unwrap();
        assert!(
            !String::from_utf8(bytes.clone())
                .unwrap()
                .contains("riggered")
        );

        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        assert_eq!(parsed.reminders.len(), 2);
        assert_eq!(parsed.reminders[0].remind_at, "2026-08-10T09:00:00+00:00");
        assert_eq!(
            parsed.reminders[1].remind_at_unix,
            DateTime::parse_from_rfc3339("2026-08-12T14:30:00+00:00")
                .unwrap()
                .timestamp()
        );
        assert!(
            !parsed.reminders[1].is_triggered,
            "firing is per machine (§5.2) and must not arrive from the wire"
        );
    }

    #[test]
    fn an_unreadable_reminder_does_not_cost_the_others() {
        let parsed = parse_reminders("2026-08-10T09:00:00+00:00; not-a-date ;");
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn linked_messages_round_trip_with_their_cache() {
        let links = vec![TaskLinkedEmail::new(
            0,
            "<abc@example.com>".to_string(),
            CachedEmailMetadata {
                subject: Some("Rechnung für Ihre Bestellung".to_string()),
                from: Some("Buchhaltung <billing@example.com>".to_string()),
                date: Some("2026-08-01T08:00:00+00:00".to_string()),
                account: Some("Work".to_string()),
                folder: Some("INBOX/Archive".to_string()),
            },
        )];

        let (bytes, _) = TaskEmailBuilder::build(&sample_task(), &[], &links, &[]).unwrap();
        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();

        assert_eq!(parsed.linked_emails.len(), 1);
        let link = &parsed.linked_emails[0];
        assert_eq!(link.message_id, "<abc@example.com>");
        assert_eq!(
            link.cached_subject.as_deref(),
            Some("Rechnung für Ihre Bestellung")
        );
        assert_eq!(link.cached_folder.as_deref(), Some("INBOX/Archive"));
    }

    /// A chat link travels as a mail link does (todo-spec §28): an Orb that
    /// predates chat links reads it, keeps it, and writes it back.
    #[test]
    fn a_chat_link_travels_in_the_links_part() {
        let links = vec![TaskLinkedEmail::new(
            0,
            "chat://chat.example.com/r1/m1?thread=t1".to_string(),
            CachedEmailMetadata {
                subject: Some("Can you look at the log?".to_string()),
                from: Some("Sara".to_string()),
                date: Some("2026-08-01T08:00:00.000Z".to_string()),
                account: Some("#dev".to_string()),
                folder: Some("https://chat.example.com/channel/dev/thread/t1?msg=m1".to_string()),
            },
        )];
        let (bytes, _) = TaskEmailBuilder::build(&sample_task(), &[], &links, &[]).unwrap();
        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        let link = &parsed.linked_emails[0];
        assert_eq!(link.message_id, "chat://chat.example.com/r1/m1?thread=t1");
        assert_eq!(link.cached_account.as_deref(), Some("#dev"));
        assert_eq!(
            link.cached_folder.as_deref(),
            Some("https://chat.example.com/channel/dev/thread/t1?msg=m1")
        );
    }

    /// Steps travel in a part of their own, and a task that has any is
    /// written as version 3, which an older build skips rather than rewrites
    /// without them (todo-spec §17).
    #[test]
    fn steps_round_trip_and_raise_the_version() {
        let mut task = sample_task();
        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("X-Orb-Task-Version: 2\r\n"));

        task.steps = vec![
            crate::tasks::TaskStep::new("Read sections 1–3".to_string()),
            crate::tasks::TaskStep {
                done: true,
                ..crate::tasks::TaskStep::new("Check the numbers".to_string())
            },
        ];
        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("X-Orb-Task-Version: 3\r\n"));
        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        assert_eq!(parsed.task.steps, task.steps);
    }

    // -- the log -----------------------------------------------------------

    #[test]
    fn the_log_round_trips_entry_for_entry() {
        let ctx = context();
        let mut task = sample_task();
        task.log.push(TaskLogEntry::new(
            TaskEvent::DateChanged {
                field: DateField::Due,
                from: None,
                to: Some("2026-08-12".to_string()),
            },
            &ctx,
        ));
        task.log.push(TaskLogEntry::new(TaskEvent::Trashed, &ctx));

        let parsed = build_and_parse(&task);

        assert_eq!(parsed.task.log.len(), task.log.len());
        for (before, after) in task.log.iter().zip(parsed.task.log.iter()) {
            assert_eq!(before.entry_uuid, after.entry_uuid);
            assert_eq!(before.occurred_at, after.occurred_at);
            assert_eq!(before.occurred_at_unix, after.occurred_at_unix);
            assert_eq!(before.device_id, after.device_id);
            assert_eq!(before.event, after.event);
            assert_eq!(before.rendered_text(), after.rendered_text());
        }
    }

    #[test]
    fn an_unknown_event_type_survives_a_round_trip_verbatim() {
        // The union merge of §11 means entries written by a newer build pass
        // through this one on their way back to the server. If they did not
        // survive, running an old build on one machine would amputate the
        // shared history of every machine.
        let mut task = sample_task();
        let mut entry = TaskLogEntry::new(TaskEvent::Uncompleted, &context());
        entry.event = TaskEvent::Unknown {
            event_type: "delegated".to_string(),
            detail: Some(r#"{"event":"delegated","to":"Anna","note":"ü"}"#.to_string()),
        };
        task.log.push(entry.clone());

        let parsed = build_and_parse(&task);
        let last = parsed.task.log.last().unwrap();
        assert_eq!(last.event, entry.event);
        assert_eq!(last.detail_json(), entry.detail_json());
    }

    #[test]
    fn the_rendered_text_is_on_the_wire_but_is_not_read_back() {
        let task = sample_task();
        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        let mail = mailparse::parse_mail(&bytes).unwrap();
        let part = find_part(&mail, |part| part_marker(part).as_deref() == Some(PART_LOG)).unwrap();
        let json = String::from_utf8(part.get_body_raw().unwrap()).unwrap();
        assert!(json.contains("Created 'Buy milk'"), "{json}");

        // Text is derived from the event, so an entry written by a build with
        // different wording still renders in this build's.
        let doctored = json.replace("Created 'Buy milk'", "ancient wording");
        let entries: Vec<WireLogEntry> = serde_json::from_str(&doctored).unwrap();
        let entry = entries.into_iter().next().unwrap().into_entry();
        assert_eq!(entry.rendered_text(), "Created 'Buy milk'");
    }

    // -- attachments -------------------------------------------------------

    #[test]
    fn a_task_message_lists_attachments_but_never_carries_their_payload() {
        let payload = b"PAYLOAD-MARKER-0123456789-this-is-the-file-body".repeat(64);
        let attachment = TaskAttachment::create(
            0,
            "task-uuid",
            "invoice.pdf".to_string(),
            Some("application/pdf".to_string()),
            Some(payload.len() as i64),
        );

        let (task_bytes, _) =
            TaskEmailBuilder::build(&sample_task(), &[], &[], std::slice::from_ref(&attachment))
                .unwrap();
        let task_text = String::from_utf8(task_bytes.clone()).unwrap();

        assert!(task_text.contains(&attachment.attachment_uuid));
        assert!(task_text.contains("filename=invoice.pdf"));
        assert!(task_text.contains("type=application/pdf"));
        // Neither raw nor encoded: editing a title must never re-upload a file.
        assert!(!task_text.contains("PAYLOAD-MARKER"));
        assert!(!task_text.contains(&base64_encode(&payload)[..40]));
        assert!(
            task_bytes.len() < payload.len(),
            "the task message is smaller than its attachment's payload"
        );

        let parsed = TaskEmailParser::parse_task(&task_bytes, "Work", None).unwrap();
        assert_eq!(parsed.attachments.len(), 1);
        let descriptor = &parsed.attachments[0];
        assert_eq!(descriptor.attachment_uuid, attachment.attachment_uuid);
        assert_eq!(descriptor.filename, "invoice.pdf");
        assert_eq!(descriptor.content_type.as_deref(), Some("application/pdf"));
        assert_eq!(descriptor.size, Some(payload.len() as i64));
        assert!(!descriptor.is_cached);
    }

    #[test]
    fn an_attachment_message_round_trips_its_payload() {
        let payload: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
        let attachment = TaskAttachment::create(
            0,
            "task-uuid",
            "Rechnung ü.pdf".to_string(),
            Some("application/pdf".to_string()),
            None,
        );

        let (bytes, message_id) =
            TaskEmailBuilder::build_attachment("task-uuid", &attachment, &payload, now()).unwrap();

        let message = TaskEmailParser::parse(&bytes, "Work", Some(19)).unwrap();
        assert_eq!(message.kind(), TaskMessageKind::Attachment);
        let TaskMessage::Attachment(parsed) = message else {
            panic!("expected an attachment message");
        };

        assert_eq!(parsed.task_uuid, "task-uuid");
        assert_eq!(parsed.payload, payload);
        assert_eq!(
            parsed.attachment.attachment_uuid,
            attachment.attachment_uuid
        );
        assert_eq!(parsed.attachment.filename, "Rechnung ü.pdf");
        assert_eq!(parsed.attachment.size, Some(payload.len() as i64));
        assert_eq!(parsed.attachment.imap_uid, Some(19));
        assert_eq!(parsed.attachment.message_id, Some(message_id));
        assert_eq!(
            parsed.attachment.file_path,
            TaskAttachment::cache_path("task-uuid", &attachment.attachment_uuid, "Rechnung ü.pdf")
        );
    }

    #[test]
    fn an_empty_attachment_payload_round_trips() {
        let attachment =
            TaskAttachment::create(0, "task-uuid", "empty.txt".to_string(), None, None);
        let (bytes, _) =
            TaskEmailBuilder::build_attachment("task-uuid", &attachment, &[], now()).unwrap();
        let TaskMessage::Attachment(parsed) = TaskEmailParser::parse(&bytes, "Work", None).unwrap()
        else {
            panic!("expected an attachment message");
        };
        assert!(parsed.payload.is_empty());
        assert_eq!(parsed.attachment.size, Some(0));
    }

    #[test]
    fn a_hostile_filename_cannot_escape_the_cache_directory() {
        let descriptor =
            parse_attachment_descriptor("task-uuid", "att-1; size=3; filename=../../.ssh/id_rsa")
                .unwrap();
        assert!(descriptor.file_path.starts_with("tasks/task-uuid/att-1-"));
        assert_eq!(
            descriptor.file_path.matches('/').count(),
            2,
            "a filename off the wire must not add path segments: {}",
            descriptor.file_path
        );
    }

    #[test]
    fn many_reminders_stay_inside_the_line_limits() {
        let reminders: Vec<TaskReminder> = (1..=28)
            .map(|day| TaskReminder::new(0, format!("2026-09-{day:02}T09:00:00+00:00"), 0))
            .collect();

        let (bytes, _) = TaskEmailBuilder::build(&sample_task(), &reminders, &[], &[]).unwrap();
        for line in String::from_utf8(bytes.clone()).unwrap().split("\r\n") {
            assert!(line.len() <= 998, "line of {} octets", line.len());
        }

        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        assert_eq!(parsed.reminders.len(), 28);
        assert_eq!(parsed.reminders[27].remind_at, "2026-09-28T09:00:00+00:00");
    }

    #[test]
    fn an_absurd_filename_cannot_overrun_a_header_line() {
        let attachment = TaskAttachment::create(
            0,
            "task-uuid",
            format!("{}.pdf", "\u{fc}".repeat(400)),
            None,
            Some(1),
        );
        let (bytes, _) =
            TaskEmailBuilder::build_attachment("task-uuid", &attachment, b"x", now()).unwrap();
        for line in String::from_utf8(bytes.clone()).unwrap().split("\r\n") {
            assert!(line.len() <= 998, "line of {} octets", line.len());
        }

        let TaskMessage::Attachment(parsed) = TaskEmailParser::parse(&bytes, "Work", None).unwrap()
        else {
            panic!("expected an attachment message");
        };
        // Truncated to the filesystem limit, on a character boundary, and the
        // fold at the parameter separators left the encoded word alone.
        assert!(parsed.attachment.filename.chars().all(|c| c == '\u{fc}'));
        assert_eq!(parsed.attachment.filename.chars().count(), 127);
    }

    #[test]
    fn a_content_type_that_is_not_one_is_dropped() {
        // It would be written back into a structured header, so it may not
        // carry anything that changes that header's shape.
        let descriptor = parse_attachment_descriptor(
            "task-uuid",
            "att-1; type=text/plain\"; boundary=x; filename=a.txt",
        )
        .unwrap();
        assert_eq!(descriptor.content_type, None);
        assert_eq!(descriptor.filename, "a.txt");
    }

    #[test]
    fn a_filename_may_contain_the_parameter_separator() {
        let attachment = TaskAttachment::create(
            0,
            "task-uuid",
            "weird; type=text/html name.txt".to_string(),
            Some("text/plain".to_string()),
            Some(3),
        );
        let descriptor = attachment_descriptor(&attachment);
        let parsed = parse_attachment_descriptor("task-uuid", &descriptor).unwrap();
        assert_eq!(parsed.filename, "weird; type=text/html name.txt");
        assert_eq!(parsed.content_type.as_deref(), Some("text/plain"));
        assert_eq!(parsed.size, Some(3));
    }

    #[test]
    fn several_attachments_each_get_their_own_header() {
        let task = sample_task();
        let attachments: Vec<TaskAttachment> = (0..3)
            .map(|index| {
                TaskAttachment::create(
                    0,
                    &task.task_uuid,
                    format!("file-{index}.txt"),
                    Some("text/plain".to_string()),
                    Some(index),
                )
            })
            .collect();

        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &attachments).unwrap();
        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        assert_eq!(parsed.attachments.len(), 3);
        for (before, after) in attachments.iter().zip(parsed.attachments.iter()) {
            assert_eq!(before.attachment_uuid, after.attachment_uuid);
            assert_eq!(before.filename, after.filename);
            assert_eq!(before.file_path, after.file_path);
        }
    }

    // -- the chat marks ----------------------------------------------------

    #[test]
    fn the_chat_marks_round_trip() {
        let marks = vec![crate::chat::marks::SharedMark {
            server: "chat.example.com".into(),
            room: "r1".into(),
            thread: "t1".into(),
            kind: "done".into(),
            at: 1_700_000_000_000,
        }];
        let (bytes, message_id) = TaskEmailBuilder::build_chat_marks(&marks, 7, now()).unwrap();
        let header_end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let identity = TaskEmailParser::parse_identity(&bytes[..header_end + 4]).unwrap();
        assert_eq!(identity.kind, TaskMessageKind::ChatMarks);
        assert_eq!(
            (identity.task_uuid.as_str(), identity.revision),
            (CHAT_MARKS_ID, 7)
        );
        let TaskMessage::ChatMarks(parsed) =
            TaskEmailParser::parse(&bytes, "Work", Some(9)).unwrap()
        else {
            panic!("expected the chat marks");
        };
        assert_eq!(parsed.marks, marks);
        assert_eq!(parsed.message_id, Some(message_id));
    }

    // -- the Today cut-off -------------------------------------------------

    #[test]
    fn the_today_cut_off_round_trips() {
        let at = now();
        let (bytes, message_id) = TaskEmailBuilder::build_today_cleared(at, 3, now()).unwrap();
        let header_end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let identity = TaskEmailParser::parse_identity(&bytes[..header_end + 4]).unwrap();
        assert_eq!(identity.kind, TaskMessageKind::TodayCleared);
        assert_eq!(
            (identity.task_uuid.as_str(), identity.revision),
            (TODAY_CLEARED_ID, 3)
        );
        let TaskMessage::TodayCleared(parsed) =
            TaskEmailParser::parse(&bytes, "Work", Some(9)).unwrap()
        else {
            panic!("expected the Today cut-off");
        };
        assert_eq!(parsed.cleared_at, Some(at));
        assert_eq!(parsed.message_id, Some(message_id));
    }

    // -- the tag registry --------------------------------------------------

    #[test]
    fn the_tag_registry_round_trips() {
        let mut tags = vec![
            TaskTag::new("Büro".to_string(), 1024.0),
            TaskTag::new("errands".to_string(), 2048.0),
        ];
        tags[0].colour = Some("#ff8800".to_string());

        let (bytes, message_id) = TaskEmailBuilder::build_tag_registry(&tags, 4, now()).unwrap();
        let message = TaskEmailParser::parse(&bytes, "Work", Some(3)).unwrap();
        assert_eq!(message.kind(), TaskMessageKind::TagRegistry);
        let TaskMessage::TagRegistry(registry) = message else {
            panic!("expected a registry message");
        };

        assert_eq!(registry.revision, 4);
        assert_eq!(registry.imap_uid, Some(3));
        assert_eq!(registry.message_id, Some(message_id));
        assert_eq!(registry.tags.len(), 2);
        assert_eq!(registry.tags[0].name, "Büro");
        assert_eq!(registry.tags[0].tag_uuid, tags[0].tag_uuid);
        assert_eq!(registry.tags[0].colour.as_deref(), Some("#ff8800"));
        assert_eq!(registry.tags[1].position, 2048.0);
    }

    #[test]
    fn the_registry_has_a_well_known_identity() {
        let (bytes, _) = TaskEmailBuilder::build_tag_registry(&[], 1, now()).unwrap();
        let identity = TaskEmailParser::parse_identity(&bytes).unwrap();
        assert_eq!(identity.kind, TaskMessageKind::TagRegistry);
        assert_eq!(identity.task_uuid, TAG_REGISTRY_ID);
        assert_eq!(identity.revision, 1);
    }

    #[test]
    fn a_task_message_is_not_mistaken_for_a_registry() {
        let (bytes, _) = TaskEmailBuilder::build(&sample_task(), &[], &[], &[]).unwrap();
        assert!(
            TaskEmailParser::parse(&bytes, "Work", None)
                .unwrap()
                .into_task()
                .is_some()
        );
    }

    // -- the identity listing ----------------------------------------------

    #[test]
    fn identity_parses_from_a_header_only_fetch() {
        // What `BODY.PEEK[HEADER.FIELDS (...)]` returns: the requested headers
        // and nothing else, with no body at all.
        let fetched = b"X-Orb-Task-Version: 2\r\n\
                        X-Orb-Task-Type: task\r\n\
                        X-Orb-Task-Id: 11111111-2222-3333-4444-555555555555\r\n\
                        X-Orb-Task-Revision: 12\r\n\r\n";
        let identity = TaskEmailParser::parse_identity(fetched).unwrap();
        assert_eq!(identity.kind, TaskMessageKind::Task);
        assert_eq!(identity.task_uuid, "11111111-2222-3333-4444-555555555555");
        assert_eq!(identity.revision, 12);
        assert_eq!(identity.attachment_uuid, None);
    }

    #[test]
    fn identity_headers_cover_everything_identity_needs() {
        let attachment = TaskAttachment::create(0, "task-uuid", "a.txt".to_string(), None, Some(1));
        let (bytes, _) =
            TaskEmailBuilder::build_attachment("task-uuid", &attachment, b"hi", now()).unwrap();

        // Keep only the headers the listing fetch asks for, exactly as a server
        // would, and check identity still resolves.
        let text = String::from_utf8(bytes).unwrap();
        let mut fetched = String::new();
        for line in text.split("\r\n") {
            if IDENTITY_HEADERS.iter().any(|name| {
                line.to_lowercase()
                    .starts_with(&format!("{}:", name.to_lowercase()))
            }) {
                fetched.push_str(line);
                fetched.push_str("\r\n");
            }
        }
        fetched.push_str("\r\n");

        let identity = TaskEmailParser::parse_identity(fetched.as_bytes()).unwrap();
        assert_eq!(identity.kind, TaskMessageKind::Attachment);
        assert_eq!(identity.task_uuid, "task-uuid");
        assert_eq!(
            identity.attachment_uuid.as_deref(),
            Some(attachment.attachment_uuid.as_str())
        );
    }

    #[test]
    fn identity_rejects_version_one() {
        let fetched = b"X-Orb-Task-Version: 1\r\nX-Orb-Task-Id: old\r\n\r\n";
        assert!(TaskEmailParser::parse_identity(fetched).is_err());
    }

    // -- header encoding ---------------------------------------------------

    fn parsed_header(line: &str) -> String {
        let (header, _) = mailparse::parse_header(line.as_bytes()).unwrap();
        header.get_value()
    }

    #[test]
    fn a_long_unicode_title_folds_and_round_trips() {
        let title = "Überprüfung der Gartenplanung für die Saison mit allen Beeten, Wegen \
                     und Bewässerungsänderungen"
            .to_string();
        let line = header_line("Subject", &title);

        for raw in line.trim_end_matches("\r\n").split("\r\n") {
            assert!(
                raw.len() <= MAX_LINE,
                "line of {} characters: {raw}",
                raw.len()
            );
            assert!(raw.is_ascii(), "an encoded header must be ASCII: {raw}");
        }
        assert!(line.contains("\r\n "), "a title this long must fold");
        assert_eq!(parsed_header(line.trim_end_matches("\r\n")), title);
    }

    #[test]
    fn a_long_unicode_title_survives_the_whole_message() {
        let title = "Öl, Käse und Grünkohl für das Wochenende — sowie eine sehr lange \
                     Erläuterung, warum genau diese Zeile länger als achtundsiebzig Zeichen ist"
            .to_string();
        let mut task = sample_task();
        task.subject = title.clone();

        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for line in text.split("\r\n") {
            assert!(line.len() <= 998, "RFC 5322 hard limit: {line}");
        }

        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        assert_eq!(parsed.task.subject, title);
    }

    #[test]
    fn encoded_words_split_on_character_boundaries() {
        // Every word must decode on its own; a byte-boundary split would leave
        // half a character in each.
        let title = "ü".repeat(60);
        let line = header_line("Subject", &title);
        let words: Vec<&str> = line
            .trim_end_matches("\r\n")
            .split("\r\n ")
            .flat_map(|chunk| chunk.split(' '))
            .filter(|chunk| chunk.starts_with("=?"))
            .collect();
        assert!(
            words.len() > 1,
            "60 two-byte characters must need several words"
        );
        for word in words {
            assert!(word.len() <= MAX_ENCODED_WORD, "{word}");
            let decoded = parsed_header(&format!("X: {word}"));
            assert!(decoded.chars().all(|c| c == 'ü'), "{decoded}");
        }
    }

    #[test]
    fn a_long_ascii_title_folds_without_rewriting_its_whitespace() {
        let title = "a quite long entirely ascii task title that will certainly not fit \
                     on one single line of a message header";
        let line = header_line("Subject", title);
        assert!(line.contains("\r\n "));
        for raw in line.trim_end_matches("\r\n").split("\r\n") {
            assert!(raw.len() <= MAX_LINE, "{raw}");
        }
        assert_eq!(parsed_header(line.trim_end_matches("\r\n")), title);
    }

    #[test]
    fn double_spaces_are_never_folded_away() {
        let title = format!("column{}value", " ".repeat(40));
        let line = header_line("Subject", &title);
        assert_eq!(parsed_header(line.trim_end_matches("\r\n")), title);
    }

    #[test]
    fn a_title_that_looks_like_an_encoded_word_is_not_decoded() {
        let title = "=?UTF-8?Q?not_really_encoded?=";
        let mut task = sample_task();
        task.subject = title.to_string();

        let parsed = build_and_parse(&task);
        assert_eq!(parsed.task.subject, title);
    }

    #[test]
    fn an_unfoldable_token_is_left_whole() {
        // A single word longer than the line: folding inside it would change
        // the value, so it is emitted as-is.
        let value = "x".repeat(200);
        let line = header_line("X-Orb-Task-Id", &value);
        assert!(!line.contains("\r\n "));
        assert_eq!(parsed_header(line.trim_end_matches("\r\n")), value);
    }

    #[test]
    fn control_characters_never_reach_a_header() {
        let mut task = sample_task();
        task.subject = "line one\r\nX-Injected: yes".to_string();

        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        // The characters survive inside the encoded word; what must not survive
        // is a line break in front of them, which would make them a header.
        assert!(!text.contains("\r\nX-Injected"));

        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        assert_eq!(parsed.task.subject, "line one\r\nX-Injected: yes");
    }

    // -- base64 ------------------------------------------------------------

    #[test]
    fn base64_matches_the_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"Hello PDF"), "SGVsbG8gUERG");
    }

    #[test]
    fn base64_bodies_respect_the_line_limit() {
        let body = base64_body(&vec![b'x'; 1000]);
        assert!(body.ends_with("\r\n"));
        for line in body.trim_end_matches("\r\n").split("\r\n") {
            assert!(line.len() <= BASE64_LINE, "{}", line.len());
        }
    }

    #[test]
    fn a_body_of_very_long_lines_survives() {
        // The reason nothing here is sent as 8bit: RFC 5322 caps a line at 998
        // octets and a note has no such cap.
        let mut task = sample_task();
        task.body_text = Some(format!("{}\n{}", "ä".repeat(2000), "b".repeat(2000)));

        let (bytes, _) = TaskEmailBuilder::build(&task, &[], &[], &[]).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for line in text.split("\r\n") {
            assert!(line.len() <= 998, "line of {} octets", line.len());
        }

        let parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        assert_eq!(parsed.task.body_text, task.body_text);
    }

    #[test]
    fn line_endings_are_normalised_in_both_directions() {
        let mut task = sample_task();
        task.body_text = Some("first\r\nsecond\nthird".to_string());

        let parsed = build_and_parse(&task);
        assert_eq!(
            parsed.task.body_text.as_deref(),
            Some("first\nsecond\nthird")
        );
    }

    #[test]
    fn set_task_id_reaches_everything_that_references_the_task() {
        let mut task = sample_task();
        task.log
            .push(TaskLogEntry::new(TaskEvent::Trashed, &context()));
        let attachment =
            TaskAttachment::create(0, &task.task_uuid, "a.txt".to_string(), None, Some(1));
        let reminders = vec![TaskReminder::new(
            0,
            "2026-08-10T09:00:00+00:00".to_string(),
            0,
        )];
        let links = vec![TaskLinkedEmail::new(
            0,
            "<a@example.com>".to_string(),
            CachedEmailMetadata::default(),
        )];

        let (bytes, _) =
            TaskEmailBuilder::build(&task, &reminders, &links, std::slice::from_ref(&attachment))
                .unwrap();
        let mut parsed = TaskEmailParser::parse_task(&bytes, "Work", None).unwrap();
        parsed.set_task_id(77);

        assert_eq!(parsed.task.id, Some(77));
        assert!(parsed.task.log.iter().all(|e| e.task_id == Some(77)));
        assert!(parsed.reminders.iter().all(|r| r.task_id == 77));
        assert!(parsed.linked_emails.iter().all(|l| l.task_id == 77));
        assert!(parsed.attachments.iter().all(|a| a.task_id == 77));
    }
}
