//! The task data model.
//!
//! Every type here mirrors a table created by schema v18, and the vocabulary is
//! the spec's: a task has a **postpone date** (hide it until then) and a **due
//! date** (when it must actually be done). The SQLite columns are still named
//! `snooze_until` and `deadline` — renaming them would have meant a second
//! rebuild of the `tasks` table for no behavioural gain — so the mapping from
//! field to column is spelled out on each field.
//!
//! Two things in here are load-bearing enough to say up front.
//!
//! **`task_uuid` is identity; `message_id` is not.** IMAP messages are
//! immutable, so every edit to a task is an APPEND of a new message followed by
//! a delete of the old one. The RFC `Message-ID` therefore changes on every
//! single edit: it identifies *an edit*, not *a task*. A reconciliation that
//! groups server messages by `Message-ID` sees one task edited on two machines
//! as two unrelated tasks and duplicates it, and sees an append whose response
//! was lost as a second task rather than a retry of the first. That is the bug
//! §3.1 exists to prevent, and the only defence is that `task_uuid` is
//! generated once, before the first append, and never changes again.
//!
//! **Dates are civil dates, never instants.** `postpone_date` /
//! `due_date` hold `YYYY-MM-DD` and the `_unix` companions hold that calendar
//! day encoded as midnight UTC ([`civil_day_unix`]) purely so an index can be
//! range-scanned. They are *not* points in time: comparing them against
//! `Utc::now().timestamp()` is what made tasks appear and vanish at 01:00 in
//! CET (§4.3). Everything that asks "is this today?" must encode the *local*
//! civil date the same way and compare civil to civil.

use std::collections::HashMap;

use chrono::{DateTime, Days, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::recurrence::{
    AdvanceFrom, Advancement, RecurrenceError, RecurrenceRule, TaskSchedule, advance,
};

/// Everything that can be wrong with a task edit.
///
/// These are all user-facing: the spec asks for a rejected postpone date to be
/// reported in the status line rather than silently swallowed or silently
/// applied, so the messages are written to be read by a person.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TaskError {
    #[error("'{value}' is not a {field} date of the form YYYY-MM-DD")]
    InvalidDate { field: &'static str, value: String },

    #[error("invalid recurrence rule: {0}")]
    InvalidRecurrence(#[from] RecurrenceError),

    #[error("'{0}' is not a task view")]
    UnknownView(String),
}

impl From<TaskError> for crate::Error {
    fn from(error: TaskError) -> Self {
        crate::Error::InvalidInput(error.to_string())
    }
}

/// The civil date the epoch fell on, and the origin every stored day number is
/// measured from.
const EPOCH_DAY: NaiveDate = match NaiveDate::from_ymd_opt(1970, 1, 1) {
    Some(date) => date,
    None => unreachable!(),
};

/// Encode a civil date as the `_unix` companion column.
///
/// The value is the date's midnight *as if it were UTC*, i.e. whole days since
/// the epoch times 86400. That deliberately throws the timezone away: the
/// column exists so `WHERE deadline_unix <= ?` can use an index, and the only
/// requirement on it is that both sides of every comparison are encoded by this
/// same function. Feed it the machine's *local* today and the comparison is a
/// comparison of calendar days, which is what "due today" means and what
/// survives DST unchanged.
pub fn civil_day_unix(date: NaiveDate) -> i64 {
    (date - EPOCH_DAY).num_days() * 86_400
}

/// Decode a `_unix` companion column back to its civil date.
pub fn civil_day_from_unix(value: i64) -> Option<NaiveDate> {
    let days = value.div_euclid(86_400);
    if days >= 0 {
        EPOCH_DAY.checked_add_days(Days::new(days as u64))
    } else {
        EPOCH_DAY.checked_sub_days(Days::new(days.unsigned_abs()))
    }
}

/// Parse a stored `YYYY-MM-DD` date.
pub fn parse_civil_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok()
}

/// Render an instant for the timestamp columns.
///
/// Fixed-width RFC3339 in UTC with no fractional seconds, because
/// `completed_at` and `deleted_at` are ordered and swept *as text* (their
/// indexes sort the text), so every writer has to agree on a format that sorts
/// chronologically. Seconds precision matches the `+00:00` offset form already
/// written elsewhere in the codebase, which is what a mixed column would have
/// to sort against.
pub fn format_timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Everything a mutation needs from the outside world.
///
/// Passed in rather than read from the clock inside the model so that the whole
/// mutation layer is deterministic and testable, and — more importantly — so
/// that `today` is forced to be a decision the *caller* makes. There is no
/// correct way to derive the local civil date from a `DateTime<Utc>` inside
/// here, and a model that tried would reintroduce the midnight bug of §4.3.
#[derive(Debug, Clone, Copy)]
pub struct ChangeContext<'a> {
    /// The instant the change happens, for the timestamp columns.
    pub now: DateTime<Utc>,
    /// The machine's *local* civil date. Recurrence advancement and completion
    /// both mean "today" in the user's calendar, not in UTC.
    pub today: NaiveDate,
    /// This machine's identity, stamped into every log entry it appends so a
    /// merged timeline can say where an entry came from. `None` before the id
    /// has been generated.
    pub device_id: Option<&'a str>,
}

impl<'a> ChangeContext<'a> {
    pub fn new(now: DateTime<Utc>, today: NaiveDate) -> Self {
        Self {
            now,
            today,
            device_id: None,
        }
    }

    pub fn with_device_id(mut self, device_id: Option<&'a str>) -> Self {
        self.device_id = device_id;
        self
    }
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// A task's two dates, independent of each other (§2).
///
/// A postpone date after the due date is allowed: the task shows from the
/// earlier of the two, so a due date that has come keeps the task in Today
/// whatever it was postponed to. Nothing is clamped, so neither date is ever
/// changed by setting the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskDates {
    postpone: Option<NaiveDate>,
    due: Option<NaiveDate>,
}

impl TaskDates {
    pub fn new(postpone: Option<NaiveDate>, due: Option<NaiveDate>) -> Self {
        Self { postpone, due }
    }

    pub fn postpone(&self) -> Option<NaiveDate> {
        self.postpone
    }

    pub fn due(&self) -> Option<NaiveDate> {
        self.due
    }

    /// The date the task sits on: the postpone date if there is one, otherwise
    /// the due date. Matches [`TaskSchedule::anchor`], which recurrence
    /// advances from.
    pub fn anchor(&self) -> Option<NaiveDate> {
        self.postpone.or(self.due)
    }

    /// Are both dates unset? The Someday view is exactly this.
    pub fn is_undated(&self) -> bool {
        self.postpone.is_none() && self.due.is_none()
    }

    pub fn with_postpone(self, postpone: Option<NaiveDate>) -> Self {
        Self::new(postpone, self.due)
    }

    pub fn with_due(self, due: Option<NaiveDate>) -> Self {
        Self::new(self.postpone, due)
    }
}

impl From<TaskDates> for TaskSchedule {
    fn from(dates: TaskDates) -> Self {
        TaskSchedule::new(dates.postpone, dates.due)
    }
}

impl From<TaskSchedule> for TaskDates {
    fn from(schedule: TaskSchedule) -> Self {
        TaskDates::new(schedule.postpone, schedule.due)
    }
}

/// Which of the two dates a log entry is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateField {
    Postpone,
    Due,
}

impl DateField {
    pub fn as_str(&self) -> &'static str {
        match self {
            DateField::Postpone => "postpone",
            DateField::Due => "due",
        }
    }

    /// How the field is named in rendered log text.
    fn label(&self) -> &'static str {
        match self {
            DateField::Postpone => "Postpone date",
            DateField::Due => "Due date",
        }
    }
}

// ---------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------

/// What happened to a task, machine-readable.
///
/// Every entry carries both this and the text rendered from it ([`Display`]).
/// The enum is what undo (§10.3) and the Completed view (§8: recurring tasks
/// are listed by their logged completions) read; the text is what the timeline
/// shows. Deriving the text from the enum rather than storing a second copy of
/// it means an entry written by an older build renders in this build's wording.
///
/// [`TaskEvent::Unknown`] is the reason this can be stored and re-read without
/// loss. The log is merged as a union and is exempt from last-writer-wins
/// (§11), so an entry written by a *newer* build passes through this one on its
/// way back to the server; if it could not be represented it would be dropped,
/// and one machine running an older build would silently amputate the shared
/// history. Unknown keeps the raw JSON verbatim instead. (Within a wire-format
/// version the payload shapes are fixed, so unknown *fields* on a known event
/// are not preserved — only unknown event types are.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TaskEvent {
    Created {
        title: String,
    },
    TitleChanged {
        from: String,
        to: String,
    },
    /// Bodies can be arbitrarily long, and a log that quoted both versions of
    /// every edit would be larger than the task; the timeline records that the
    /// body changed and when.
    BodyChanged,
    DateChanged {
        field: DateField,
        from: Option<String>,
        to: Option<String>,
    },
    TagAdded {
        tag_uuid: String,
        /// The tag's name at the time, so the entry still reads sensibly after
        /// the tag has been deleted from the registry.
        name: Option<String>,
    },
    TagRemoved {
        tag_uuid: String,
        name: Option<String>,
    },
    RecurrenceChanged {
        from: Option<String>,
        to: Option<String>,
    },
    /// A completion that moved the series on instead of ending it.
    RecurrenceAdvanced {
        from: String,
        to: String,
        /// Occurrences the advance stepped over because the completion was
        /// late. Zero on a normal advance.
        skipped: u32,
    },
    Completed {
        /// The local civil date it was completed on, which is what the
        /// Completed view groups by.
        on: String,
    },
    Uncompleted,
    /// Recorded because undo restores previous field values (§10.3) and a
    /// reorder is one of the mutations that must be undoable.
    Reordered {
        from: f64,
        to: f64,
    },
    ReminderFired {
        remind_at: String,
    },
    /// The summary §5.2 requires instead of a year of notifications at once.
    RemindersMissed {
        count: u32,
    },
    LinkAdded {
        message_id: String,
        subject: Option<String>,
    },
    LinkRemoved {
        message_id: String,
    },
    /// The steps of §17, each by its title at the time.
    StepAdded {
        title: String,
    },
    StepRemoved {
        title: String,
    },
    StepRenamed {
        from: String,
        to: String,
    },
    StepCompleted {
        title: String,
    },
    StepReopened {
        title: String,
    },
    Trashed,
    Restored,
    /// Brought in from elsewhere — another app (§22) — with what to know about
    /// it. An import's own mark also names where the task came from, what it
    /// was there, and the revision the import left it at, so that a later
    /// import can tell a change made there from one made here.
    Imported {
        note: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<i64>,
    },
    /// An event type this build does not know, kept verbatim.
    Unknown {
        event_type: String,
        detail: Option<String>,
    },
}

impl TaskEvent {
    /// The value stored in `task_log.event_type`, and the wire tag.
    pub fn kind(&self) -> &str {
        match self {
            TaskEvent::Created { .. } => "created",
            TaskEvent::TitleChanged { .. } => "title_changed",
            TaskEvent::BodyChanged => "body_changed",
            TaskEvent::DateChanged { .. } => "date_changed",
            TaskEvent::TagAdded { .. } => "tag_added",
            TaskEvent::TagRemoved { .. } => "tag_removed",
            TaskEvent::RecurrenceChanged { .. } => "recurrence_changed",
            TaskEvent::RecurrenceAdvanced { .. } => "recurrence_advanced",
            TaskEvent::Completed { .. } => "completed",
            TaskEvent::Uncompleted => "uncompleted",
            TaskEvent::Reordered { .. } => "reordered",
            TaskEvent::ReminderFired { .. } => "reminder_fired",
            TaskEvent::RemindersMissed { .. } => "reminders_missed",
            TaskEvent::LinkAdded { .. } => "link_added",
            TaskEvent::LinkRemoved { .. } => "link_removed",
            TaskEvent::StepAdded { .. } => "step_added",
            TaskEvent::StepRemoved { .. } => "step_removed",
            TaskEvent::StepRenamed { .. } => "step_renamed",
            TaskEvent::StepCompleted { .. } => "step_completed",
            TaskEvent::StepReopened { .. } => "step_reopened",
            TaskEvent::Trashed => "trashed",
            TaskEvent::Restored => "restored",
            TaskEvent::Imported { .. } => "imported",
            TaskEvent::Unknown { event_type, .. } => event_type,
        }
    }

    /// The value stored in `task_log.detail`.
    pub fn detail_json(&self) -> Option<String> {
        match self {
            TaskEvent::Unknown { detail, .. } => detail.clone(),
            known => serde_json::to_string(known).ok(),
        }
    }

    /// Rebuild an event from its two stored columns.
    ///
    /// Infallible by construction: anything that does not round-trip becomes
    /// [`TaskEvent::Unknown`] with the raw detail attached, because a log entry
    /// that fails to parse must still be re-uploadable. A parse error here
    /// would delete somebody's history.
    pub fn from_parts(event_type: &str, detail: Option<&str>) -> Self {
        let parsed = detail.and_then(|json| serde_json::from_str::<TaskEvent>(json).ok());
        match parsed {
            Some(event) if event.kind() == event_type => event,
            _ => TaskEvent::Unknown {
                event_type: event_type.to_string(),
                detail: detail.map(str::to_string),
            },
        }
    }
}

impl std::fmt::Display for TaskEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskEvent::Created { title } => write!(f, "Created '{title}'"),
            TaskEvent::TitleChanged { from, to } => write!(f, "Renamed '{from}' to '{to}'"),
            TaskEvent::BodyChanged => write!(f, "Edited the notes"),
            TaskEvent::DateChanged { field, from, to } => match (from, to) {
                (None, Some(to)) => write!(f, "{} set to {to}", field.label()),
                (Some(from), None) => write!(f, "{} cleared (was {from})", field.label()),
                (Some(from), Some(to)) => {
                    write!(f, "{} moved from {from} to {to}", field.label())
                }
                (None, None) => write!(f, "{} unchanged", field.label()),
            },
            TaskEvent::TagAdded { tag_uuid, name } => {
                write!(f, "Tagged {}", name.as_deref().unwrap_or(tag_uuid))
            }
            TaskEvent::TagRemoved { tag_uuid, name } => {
                write!(f, "Untagged {}", name.as_deref().unwrap_or(tag_uuid))
            }
            TaskEvent::RecurrenceChanged { from, to } => match (from, to) {
                (_, Some(to)) => write!(f, "Repeats {to}"),
                (Some(from), None) => write!(f, "No longer repeats (was {from})"),
                (None, None) => write!(f, "Recurrence unchanged"),
            },
            TaskEvent::RecurrenceAdvanced { from, to, skipped } => {
                if *skipped > 0 {
                    write!(f, "Advanced from {from} to {to}, skipping {skipped}")
                } else {
                    write!(f, "Advanced from {from} to {to}")
                }
            }
            TaskEvent::Completed { on } => write!(f, "Completed on {on}"),
            TaskEvent::Uncompleted => write!(f, "Marked as not completed"),
            TaskEvent::Reordered { .. } => write!(f, "Reordered"),
            TaskEvent::ReminderFired { remind_at } => write!(f, "Reminder fired at {remind_at}"),
            TaskEvent::RemindersMissed { count } => write!(f, "{count} missed reminders"),
            TaskEvent::LinkAdded {
                message_id,
                subject,
            } => write!(f, "Linked {}", subject.as_deref().unwrap_or(message_id)),
            TaskEvent::LinkRemoved { message_id } => write!(f, "Unlinked {message_id}"),
            TaskEvent::StepAdded { title } => write!(f, "Step '{title}' added"),
            TaskEvent::StepRemoved { title } => write!(f, "Step '{title}' removed"),
            TaskEvent::StepRenamed { from, to } => write!(f, "Step '{from}' renamed to '{to}'"),
            TaskEvent::StepCompleted { title } => write!(f, "Step '{title}' done"),
            TaskEvent::StepReopened { title } => write!(f, "Step '{title}' reopened"),
            TaskEvent::Trashed => write!(f, "Moved to trash"),
            TaskEvent::Restored => write!(f, "Restored from trash"),
            TaskEvent::Imported { note, .. } => write!(f, "{note}"),
            TaskEvent::Unknown { event_type, .. } => write!(f, "{event_type}"),
        }
    }
}

/// One entry in a task's append-only log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskLogEntry {
    /// Database row id; `None` until inserted.
    pub id: Option<i64>,

    /// Owning task's row id; `None` for an entry built before its task has one.
    pub task_id: Option<i64>,

    /// The entry's own identity. This is what makes the union merge of §11
    /// work: two devices that appended different entries keep both, and a
    /// device that re-applies a log it has already seen changes nothing.
    /// Without it "merge" would have to mean "pick one log and throw the other
    /// away".
    pub entry_uuid: String,

    /// When it happened (RFC3339 UTC, see [`format_timestamp`]).
    pub occurred_at: String,

    /// The same instant as a real Unix timestamp — unlike the date columns,
    /// this one *is* an instant, because a log entry happened at a moment
    /// rather than on a calendar day.
    pub occurred_at_unix: i64,

    /// Which machine appended it.
    pub device_id: Option<String>,

    /// What happened.
    pub event: TaskEvent,
}

impl TaskLogEntry {
    /// Append-time constructor: mints the entry's uuid and stamps the context.
    pub fn new(event: TaskEvent, ctx: &ChangeContext<'_>) -> Self {
        Self {
            id: None,
            task_id: None,
            entry_uuid: Uuid::new_v4().to_string(),
            occurred_at: format_timestamp(ctx.now),
            occurred_at_unix: ctx.now.timestamp(),
            device_id: ctx.device_id.map(str::to_string),
            event,
        }
    }

    /// The `event_type` column.
    pub fn event_type(&self) -> &str {
        self.event.kind()
    }

    /// The `detail` column.
    pub fn detail_json(&self) -> Option<String> {
        self.event.detail_json()
    }

    /// The timeline text.
    pub fn rendered_text(&self) -> String {
        self.event.to_string()
    }
}

/// Merge two views of one task's log.
///
/// The union keyed by `entry_uuid`, sorted by time — §11's rule, and the one
/// place in the whole model where the loser of a conflict is not discarded.
/// Everything else about a task is last-writer-wins, but two devices appending
/// to the log did not disagree about anything: they each observed something
/// that really happened, and dropping either set would be losing history that
/// cannot be reconstructed.
///
/// Ties on the timestamp are broken by `entry_uuid` so that every device
/// arrives at the same order for the same set of entries.
pub fn merge_logs(local: &[TaskLogEntry], remote: &[TaskLogEntry]) -> Vec<TaskLogEntry> {
    let mut merged: HashMap<&str, &TaskLogEntry> = HashMap::new();
    for entry in local.iter().chain(remote.iter()) {
        // The local copy wins a uuid collision: it is the one that already has
        // row ids attached, and the two are the same entry by definition.
        merged.entry(entry.entry_uuid.as_str()).or_insert(entry);
    }

    let mut entries: Vec<TaskLogEntry> = merged.into_values().cloned().collect();
    entries.sort_by(|a, b| {
        a.occurred_at_unix
            .cmp(&b.occurred_at_unix)
            .then_with(|| a.entry_uuid.cmp(&b.entry_uuid))
    });
    entries
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

/// An entry in the tag registry.
///
/// Tasks reference tags by `tag_uuid`, never by name, so renaming a tag
/// rewrites the one registry message instead of every task message that uses it
/// (§7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskTag {
    /// Database row id; `None` until inserted.
    pub id: Option<i64>,

    /// The tag's identity, and what tasks store.
    pub tag_uuid: String,

    pub name: String,

    /// Optional display colour, as the UI wants to read it (e.g. `#ff8800`).
    pub colour: Option<String>,

    /// Manual order in the tag list, `REAL` for the same reason task positions
    /// are.
    pub position: f64,
}

impl TaskTag {
    pub fn new(name: String, position: f64) -> Self {
        Self {
            id: None,
            tag_uuid: Uuid::new_v4().to_string(),
            name,
            colour: None,
            position,
        }
    }
}

/// The label to show for a tag id a task carries.
///
/// An id with no registry entry renders as the id itself rather than
/// disappearing. §7 is explicit about this: the registry message may simply not
/// have synced yet, and *unresolvable* is not *deleted*. Dropping the tag would
/// also lose it on the next upload, turning a sync-order accident into
/// permanent data loss.
pub fn tag_label<'a>(tag_uuid: &'a str, registry: &'a [TaskTag]) -> &'a str {
    registry
        .iter()
        .find(|tag| tag.tag_uuid == tag_uuid)
        .map(|tag| tag.name.as_str())
        .unwrap_or(tag_uuid)
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// A step of a task (§17): a title and whether it is done, nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStep {
    /// Minted when the step is added, so an edit can name the step it means
    /// whatever else moved.
    pub id: String,
    pub title: String,
    pub done: bool,
}

impl TaskStep {
    pub fn new(title: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            title,
            done: false,
        }
    }
}

/// The log entries for a change from one checklist to another (§17): by id,
/// so a step renamed and ticked in one edit says both. A reorder alone is a
/// change but not an entry.
fn step_change_entries(
    before: &[TaskStep],
    after: &[TaskStep],
    ctx: &ChangeContext<'_>,
) -> Vec<TaskLogEntry> {
    let mut entries = Vec::new();
    for old in before {
        match after.iter().find(|step| step.id == old.id) {
            None => entries.push(TaskLogEntry::new(
                TaskEvent::StepRemoved {
                    title: old.title.clone(),
                },
                ctx,
            )),
            Some(new) => {
                if new.title != old.title {
                    entries.push(TaskLogEntry::new(
                        TaskEvent::StepRenamed {
                            from: old.title.clone(),
                            to: new.title.clone(),
                        },
                        ctx,
                    ));
                }
                if new.done != old.done {
                    let title = new.title.clone();
                    let event = if new.done {
                        TaskEvent::StepCompleted { title }
                    } else {
                        TaskEvent::StepReopened { title }
                    };
                    entries.push(TaskLogEntry::new(event, ctx));
                }
            }
        }
    }
    for new in after {
        if !before.iter().any(|step| step.id == new.id) {
            entries.push(TaskLogEntry::new(
                TaskEvent::StepAdded {
                    title: new.title.clone(),
                },
                ctx,
            ));
        }
    }
    entries
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// A task.
///
/// Stored as one RFC822 message in the tasks folder — that message is the
/// source of truth and this row is a cache of it (§3), which is why every field
/// that matters is on the wire and why the local row ids are never anybody's
/// identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Local database row id; `None` if not yet saved.
    ///
    /// Local only, and disposable with the cache: §14.5 requires that deleting
    /// the database and resyncing reproduces everything, and it will not
    /// reproduce these numbers. Nothing that leaves this machine may reference
    /// one.
    pub id: Option<i64>,

    /// **The task's identity** (`X-Orb-Task-Id`): a UUID minted once, before
    /// the first append, and stable forever. Reconciliation groups server
    /// messages by this.
    pub task_uuid: String,

    /// Incremented on every local change. Conflicts are decided by highest
    /// revision, ties by highest UID (§3.1), and clearing the dirty flag is a
    /// compare-and-swap on it (§3.2), so an edit that forgets to bump it is an
    /// edit that can be silently lost.
    pub revision: i64,

    /// Task title. Required.
    pub subject: String,

    /// Free-form notes, plain text.
    pub body_text: Option<String>,

    /// Postpone date, `YYYY-MM-DD`. Hides the task until then. Column
    /// `snooze_until`.
    pub postpone_date: Option<String>,

    /// [`civil_day_unix`] of `postpone_date`. Column `snooze_until_unix`.
    pub postpone_date_unix: Option<i64>,

    /// Due date, `YYYY-MM-DD`. When it must actually be done. Column
    /// `deadline`.
    pub due_date: Option<String>,

    /// [`civil_day_unix`] of `due_date`. Column `deadline_unix`.
    pub due_date_unix: Option<i64>,

    /// Tag ids (not names) this task carries, from `task_tag_links`.
    pub tags: Vec<String>,

    /// Creation timestamp (RFC3339 UTC).
    pub created_at: String,

    /// Last local modification (RFC3339 UTC).
    pub updated_at: String,

    /// When it was completed, if it is.
    pub completed_at: Option<String>,

    /// When it was trashed, if it is. A soft delete: the row and the server
    /// message both stay, with `X-Orb-Task-Deleted-At` set, so the deletion
    /// itself syncs and can be undone (§10.1).
    pub deleted_at: Option<String>,

    /// An RFC 5545 `RRULE` subset (§8), canonical form, or `None` for a one-off
    /// task. Parse it with [`Task::recurrence_rule`].
    pub recurrence: Option<String>,

    /// Advance from the completion date rather than the scheduled one, for
    /// habit-style tasks.
    pub recurrence_from_completion: bool,

    /// Position in the single global manual order (§4.4).
    pub position: f64,

    /// The checklist of §17, in order.
    #[serde(default)]
    pub steps: Vec<TaskStep>,

    /// "Currently working on" focus flag. Local UI state, not a log-worthy
    /// event.
    pub is_working_on: bool,

    pub is_completed: bool,

    /// Has local changes the server has not confirmed. Cleared only by a
    /// compare-and-swap on `revision` (§3.2).
    pub is_dirty: bool,

    /// The designated tasks account (§1).
    pub account_name: String,

    /// UID of the message this task currently lives in, `None` before the first
    /// successful append.
    pub imap_uid: Option<u32>,

    /// **Not identity.** The RFC `Message-ID` of the server message this task
    /// is *currently* stored in. Every edit appends a new message and deletes
    /// the old one, so this changes on every edit and identifies an edit rather
    /// than a task — keep it only to find the superseded message and expunge
    /// it. Anything that needs to know *which task* this is reads
    /// [`Task::task_uuid`].
    pub message_id: Option<String>,

    /// CONDSTORE modification sequence, a fast path only: one server here never
    /// advances `HIGHESTMODSEQ`, so nothing may depend on it moving (§3.3).
    pub modseq: Option<u64>,

    /// The append-only log (§11), oldest first.
    pub log: Vec<TaskLogEntry>,
}

/// A task plus what its mutation produced.
///
/// Returned instead of mutating in place because a change is never only the new
/// field values: it is also the log entries that have to be inserted in the
/// same transaction, and — for a recurring task — the dates the reminders have
/// to be dragged to. Handing those back as data makes the persistence layer's
/// job mechanical and keeps the rules in one testable place.
#[derive(Debug, Clone)]
pub struct TaskChange {
    /// The task in its new state. Already contains `appended` in its log.
    pub task: Task,

    /// Entries to insert into `task_log`. A change can be real without being
    /// log-worthy (the focus flag), so this can be empty on a genuine edit —
    /// `changed` is the flag to branch on.
    pub appended: Vec<TaskLogEntry>,

    /// Whether any field actually moved. `false` means the update was a no-op
    /// and neither the revision nor the dirty flag was touched.
    pub changed: bool,

    /// Set when a completion advanced a recurring task, naming the anchor the
    /// reminders move from and to. Feed it to
    /// [`super::recurrence::shift_reminders`]; reminders live in their own
    /// table, so the model cannot move them itself.
    pub reminder_shift: Option<ReminderShift>,
}

impl TaskChange {
    /// Did anything actually change?
    ///
    /// A no-op update must not bump the revision or set the dirty flag: it
    /// would queue a pointless upload and, worse, win a conflict against a real
    /// edit made elsewhere.
    pub fn is_noop(&self) -> bool {
        !self.changed
    }
}

/// The two anchors a recurrence advance moved a task between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderShift {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl Task {
    /// Create a task, with the `Created` entry its log starts with.
    ///
    /// The uuid is minted here, before anything is appended to the server,
    /// which is what makes a retried append idempotent at the task level: the
    /// second attempt carries the same `X-Orb-Task-Id`, so a lost APPEND
    /// response produces a duplicate *message* that reconciliation collapses,
    /// never a duplicate *task* (§3.5).
    pub fn create(
        subject: String,
        account_name: String,
        position: f64,
        ctx: &ChangeContext<'_>,
    ) -> TaskChange {
        let mut task = Self::bare(
            Uuid::new_v4().to_string(),
            subject,
            account_name,
            position,
            format_timestamp(ctx.now),
        );
        // Revision 1, not 0: creation is itself a change, and a task that has
        // never been uploaded still has to beat nothing.
        task.revision = 1;
        task.is_dirty = true;

        let entry = TaskLogEntry::new(
            TaskEvent::Created {
                title: task.subject.clone(),
            },
            ctx,
        );
        task.log.push(entry.clone());

        TaskChange {
            task,
            appended: vec![entry],
            reminder_shift: None,
            changed: true,
        }
    }

    /// A task with the given identity and nothing else, for reconstruction.
    ///
    /// The load paths — a database row, a message from the server — already
    /// know every field and must *not* mint an identity, bump a revision or
    /// write a log entry; they fill the fields in. Use [`Task::create`] for a
    /// task that is genuinely new.
    pub fn bare(
        task_uuid: String,
        subject: String,
        account_name: String,
        position: f64,
        created_at: String,
    ) -> Self {
        Self {
            id: None,
            task_uuid,
            revision: 0,
            subject,
            body_text: None,
            postpone_date: None,
            postpone_date_unix: None,
            due_date: None,
            due_date_unix: None,
            tags: Vec::new(),
            updated_at: created_at.clone(),
            created_at,
            completed_at: None,
            deleted_at: None,
            recurrence: None,
            recurrence_from_completion: false,
            position,
            steps: Vec::new(),
            is_working_on: false,
            is_completed: false,
            is_dirty: false,
            account_name,
            imap_uid: None,
            message_id: None,
            modseq: None,
            log: Vec::new(),
        }
    }

    /// The postpone date, or `None` if unset or unreadable.
    pub fn postpone_date(&self) -> Option<NaiveDate> {
        self.postpone_date.as_deref().and_then(parse_civil_date)
    }

    /// The due date, or `None` if unset or unreadable.
    pub fn due_date(&self) -> Option<NaiveDate> {
        self.due_date.as_deref().and_then(parse_civil_date)
    }

    /// The task's dates, a date that cannot be read as none.
    ///
    /// Lenient by design — views have to render every task, including one an
    /// older build wrote badly. The edit path uses [`Task::dates_checked`],
    /// which refuses to guess.
    pub fn dates(&self) -> TaskDates {
        TaskDates::new(self.postpone_date(), self.due_date())
    }

    /// The task's dates, failing on a stored value that is not a date.
    ///
    /// Used before an edit: writing new dates over a row whose old ones could
    /// not be read would silently discard a schedule, and a task that fails to
    /// appear on its date is precisely the failure this feature is not allowed
    /// to have.
    pub fn dates_checked(&self) -> Result<TaskDates, TaskError> {
        let postpone = parse_optional_date(self.postpone_date.as_deref(), "postpone")?;
        let due = parse_optional_date(self.due_date.as_deref(), "due")?;
        Ok(TaskDates::new(postpone, due))
    }

    /// Write a validated pair of dates into the text and `_unix` columns.
    pub fn set_dates(&mut self, dates: TaskDates) {
        let (postpone, postpone_unix) = date_columns(dates.postpone());
        let (due, due_unix) = date_columns(dates.due());
        self.postpone_date = postpone;
        self.postpone_date_unix = postpone_unix;
        self.due_date = due;
        self.due_date_unix = due_unix;
    }

    /// The parsed recurrence rule, if the task has one.
    pub fn recurrence_rule(&self) -> Result<Option<RecurrenceRule>, TaskError> {
        match self.recurrence.as_deref() {
            Some(rule) => Ok(Some(RecurrenceRule::parse(rule)?)),
            None => Ok(None),
        }
    }

    /// Which occurrence of a recurring series the task currently sits on,
    /// counting from one. This is the `occurrences_elapsed` argument
    /// [`advance`] takes.
    ///
    /// Derived from the log — one past the completions recorded in it — rather
    /// than stored in a column, and that is deliberate: `COUNT=10` has to
    /// survive two devices completing the same series, and a scalar column
    /// would be resolved by last-writer-wins, so one device's completions would
    /// erase the other's and the series would outlive its count. The log is
    /// merged as a union (§11), so counting its completions is the one tally
    /// here that cannot go backwards.
    pub fn current_occurrence(&self) -> u32 {
        let completions = self
            .log
            .iter()
            .filter(|entry| matches!(entry.event, TaskEvent::Completed { .. }))
            .count();
        u32::try_from(completions)
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    }

    /// Open: neither completed nor trashed. What the Today badge counts.
    pub fn is_open(&self) -> bool {
        !self.is_completed && self.deleted_at.is_none()
    }

    /// Past its due date and not done. Rendered distinctly from merely due
    /// (§4.1).
    pub fn is_overdue(&self, today: NaiveDate) -> bool {
        self.is_open() && self.due_date().is_some_and(|due| due < today)
    }

    /// Due within the next three days.
    pub fn is_due_soon(&self, today: NaiveDate) -> bool {
        if !self.is_open() || self.is_overdue(today) {
            return false;
        }
        match (self.due_date(), today.checked_add_days(Days::new(3))) {
            (Some(due), Some(limit)) => due <= limit,
            _ => false,
        }
    }

    /// Was it completed on the given local date? Completed tasks stay in Today
    /// until the next local midnight (§10.2), and this is that rule.
    pub fn was_completed_on(&self, day: NaiveDate) -> bool {
        self.completed_at
            .as_deref()
            .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
            .is_some_and(|stamp| stamp.date_naive() == day)
    }
}

/// Both stored representations of an optional date.
fn date_columns(date: Option<NaiveDate>) -> (Option<String>, Option<i64>) {
    match date {
        Some(date) => (Some(date.to_string()), Some(civil_day_unix(date))),
        None => (None, None),
    }
}

fn parse_optional_date(
    value: Option<&str>,
    field: &'static str,
) -> Result<Option<NaiveDate>, TaskError> {
    match value {
        None => Ok(None),
        Some(raw) => parse_civil_date(raw)
            .map(Some)
            .ok_or_else(|| TaskError::InvalidDate {
                field,
                value: raw.to_string(),
            }),
    }
}

/// Lightweight task metadata for list views.
///
/// Excludes the body and the log, which is the whole point: a list of two
/// hundred tasks should not carry two hundred timelines across the FFI
/// boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskListItem {
    pub id: i64,
    pub task_uuid: String,
    pub subject: String,
    pub postpone_date: Option<String>,
    pub postpone_date_unix: Option<i64>,
    pub due_date: Option<String>,
    pub due_date_unix: Option<i64>,
    pub completed_at: Option<String>,
    pub deleted_at: Option<String>,
    pub tags: Vec<String>,
    /// Whether the task repeats, without the cost of parsing the rule.
    pub is_recurring: bool,
    pub position: f64,
    pub is_working_on: bool,
    pub is_completed: bool,
    pub is_dirty: bool,
    pub account_name: String,
    pub imap_uid: Option<u32>,

    /// The checklist of §17: how many steps, and how many are done.
    pub steps_total: u32,
    pub steps_done: u32,
    pub linked_email_count: usize,
    pub reminder_count: usize,
    pub attachment_count: usize,
    /// A repeating task completed today, in Today: open again for its next
    /// date, and shown done until midnight like any task completed today
    /// (§10.2).
    pub is_done_for_today: bool,
}

impl TaskListItem {
    pub fn is_open(&self) -> bool {
        !self.is_completed && self.deleted_at.is_none()
    }

    /// Past its due date and not done, comparing civil day to civil day.
    pub fn is_overdue(&self, today: NaiveDate) -> bool {
        self.is_open()
            && self
                .due_date_unix
                .is_some_and(|due| due < civil_day_unix(today))
    }
}

// ---------------------------------------------------------------------------
// Reminders, links, attachments
// ---------------------------------------------------------------------------

/// A reminder: a local date *and* time at which the task should interrupt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskReminder {
    pub id: Option<i64>,
    pub task_id: i64,

    /// Local date and time (RFC3339).
    pub remind_at: String,

    /// The same moment as a Unix timestamp, for the pending-reminder index.
    pub remind_at_unix: i64,

    /// Whether this machine has fired it. Local per machine and deliberately
    /// absent from the wire format (§5.2): every machine is supposed to notify
    /// its own user.
    pub is_triggered: bool,
}

impl TaskReminder {
    pub fn new(task_id: i64, remind_at: String, remind_at_unix: i64) -> Self {
        Self {
            id: None,
            task_id,
            remind_at,
            remind_at_unix,
            is_triggered: false,
        }
    }

    /// The identifier this reminder is scheduled under with
    /// `UNUserNotificationCenter`.
    ///
    /// Derived from the task's uuid and the reminder's instant rather than from
    /// any row id, because reconciliation has to be idempotent across a rebuilt
    /// cache (§5.1, §14.5): row ids are reassigned when the database is deleted
    /// and resynced, so an id-derived identifier would leave every already
    /// scheduled notification orphaned — un-cancellable, and duplicated by the
    /// next reconcile. This form recomputes to the same string on every machine
    /// and after every rebuild, so scheduling twice is a no-op and a moved
    /// reminder cancels cleanly.
    pub fn notification_id(&self, task_uuid: &str) -> String {
        format!("orb.task.{task_uuid}.{}", self.remind_at_unix)
    }
}

/// A link between a task and an email (§6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLinkedEmail {
    pub id: Option<i64>,
    pub task_id: i64,

    /// RFC `Message-ID` of the linked message — here it *is* the identity,
    /// because a mail message, unlike a task message, is never rewritten.
    pub message_id: String,

    pub cached_subject: Option<String>,
    pub cached_from: Option<String>,
    pub cached_date: Option<String>,
    pub cached_account: Option<String>,

    /// Where the message was last seen. Tried first by the server search of
    /// §6.2 and updated when the message turns up elsewhere.
    pub cached_folder: Option<String>,
}

impl TaskLinkedEmail {
    pub fn new(task_id: i64, message_id: String, metadata: CachedEmailMetadata) -> Self {
        Self {
            id: None,
            task_id,
            message_id,
            cached_subject: metadata.subject,
            cached_from: metadata.from,
            cached_date: metadata.date,
            cached_account: metadata.account,
            cached_folder: metadata.folder,
        }
    }
}

/// The snapshot taken when a message is linked.
///
/// The cache is the point of the link: years later the message may have been
/// moved, archived or deleted, and §6.2 requires the task to stay readable
/// anyway — "message no longer available" plus the subject and sender it had,
/// never a broken task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CachedEmailMetadata {
    pub subject: Option<String>,
    pub from: Option<String>,
    pub date: Option<String>,
    pub account: Option<String>,
    pub folder: Option<String>,
}

/// An attachment on a task (§9).
///
/// The payload lives in its own message in the tasks folder, not inside the
/// task message, because every task edit rewrites the task message — inlining
/// the payload would re-upload a 10 MB file to rename a task. The task message
/// lists only these descriptors; the payload is fetched on demand and cached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskAttachment {
    pub id: Option<i64>,
    pub task_id: i64,

    /// The attachment's own identity, carried by both the task message's
    /// `X-Orb-Task-Attachments` header and the message holding the payload.
    pub attachment_uuid: String,

    pub filename: String,
    pub content_type: Option<String>,
    pub size: Option<i64>,

    /// Where the payload is cached, relative to the attachments directory —
    /// whether or not it has been downloaded yet. It is derived from the two
    /// uuids, so it can be written down before there is a file at it.
    pub file_path: String,

    /// UID of the message holding the payload, `None` until it has been
    /// appended or located.
    pub imap_uid: Option<u32>,

    /// `Message-ID` of that message, kept for the same reason a task's is: to
    /// find it again in order to expunge it.
    pub message_id: Option<String>,

    /// Whether `file_path` currently holds the payload. The cache is
    /// disposable; this is what lets it be re-fetched rather than trusted.
    pub is_cached: bool,
}

impl TaskAttachment {
    /// Describe a new attachment, minting its identity and cache path.
    pub fn create(
        task_id: i64,
        task_uuid: &str,
        filename: String,
        content_type: Option<String>,
        size: Option<i64>,
    ) -> Self {
        let attachment_uuid = Uuid::new_v4().to_string();
        let file_path = Self::cache_path(task_uuid, &attachment_uuid, &filename);
        Self {
            id: None,
            task_id,
            attachment_uuid,
            filename,
            content_type,
            size,
            file_path,
            imap_uid: None,
            message_id: None,
            is_cached: false,
        }
    }

    /// The cache path for an attachment, relative to the attachments root.
    ///
    /// Prefixed with the attachment's uuid so two files of the same name on one
    /// task do not collide, and stripped of anything path-like: the filename
    /// and both ids arrive from a remote message, so `../../.ssh` is something
    /// somebody can put on the wire in any of the three. The parser refuses an
    /// id that is not a [`is_wire_id`] identifier; an id that reaches here all
    /// the same is reduced to one as well, so the path is three components
    /// under `tasks/` whatever it was built from.
    pub fn cache_path(task_uuid: &str, attachment_uuid: &str, filename: &str) -> String {
        format!(
            "tasks/{}/{}-{}",
            id_component(task_uuid),
            id_component(attachment_uuid),
            sanitise_filename(filename)
        )
    }
}

/// The longest identifier a task message may carry.
pub const MAX_WIRE_ID_LEN: usize = 64;

/// Whether `id` is an identifier Orb accepts off the wire: a task's, an
/// attachment's or a registry's.
///
/// Orb mints UUIDs, and what is accepted is the alphabet a UUID is spelled in,
/// plus `_`, up to [`MAX_WIRE_ID_LEN`] bytes. The ids name directories under
/// the attachment cache, which is why a dot, a separator or a control
/// character is refused rather than escaped: `..` must not be an id, and an
/// id must not be a path.
pub fn is_wire_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_WIRE_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An id as a single path component: itself when it is a [`is_wire_id`]
/// identifier, else what is left of it with everything else replaced.
fn id_component(id: &str) -> String {
    if is_wire_id(id) {
        return id.to_string();
    }
    let reduced: String = id
        .bytes()
        .take(MAX_WIRE_ID_LEN)
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                b as char
            } else {
                '_'
            }
        })
        .collect();
    if reduced.is_empty() {
        "_".to_string()
    } else {
        reduced
    }
}

/// Reduce a filename to something that cannot escape its directory.
fn sanitise_filename(filename: &str) -> String {
    let cleaned: String = filename
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(['.', ' ']);
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Per-account task sync state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSyncState {
    pub id: Option<i64>,

    /// The designated tasks account (§1).
    pub account_name: String,

    /// The server-side folder name, taken from the config verbatim. No assumed
    /// prefix: a leading `.` is a Courier/Maildir convention, not a universal
    /// one.
    pub folder_name: String,

    pub uidvalidity: Option<u32>,

    /// A fast path only, never the record of what changed (§3.3).
    pub highest_modseq: Option<u64>,

    pub last_sync_time: Option<String>,

    /// When the folder was last listed in full — every UID with its task id and
    /// revision. That listing, not a MODSEQ delta, is what reconciliation runs
    /// on, and this is what lets a sync decide one is due.
    pub last_listing_at: Option<String>,
    pub last_listing_at_unix: Option<i64>,

    /// How many messages that listing saw. Compared against the folder's
    /// message count on the next sync: a server claiming nothing changed while
    /// the count moved has lied, and the listing runs anyway.
    pub last_listing_count: Option<i64>,

    /// This machine's identity, stamped into every log entry it appends.
    /// Generated rather than configured, and stored here rather than in the
    /// config file so that copying a config to a second machine does not give
    /// two machines the same identity.
    pub device_id: Option<String>,
}

impl TaskSyncState {
    /// Default folder name when the config does not name one.
    pub const DEFAULT_FOLDER: &'static str = "Tasks";

    pub fn new(account_name: String) -> Self {
        Self {
            id: None,
            account_name,
            folder_name: Self::DEFAULT_FOLDER.to_string(),
            uidvalidity: None,
            highest_modseq: None,
            last_sync_time: None,
            last_listing_at: None,
            last_listing_at_unix: None,
            last_listing_count: None,
            device_id: None,
        }
    }

    /// Did `UIDVALIDITY` change? A change means "your UIDs are meaningless",
    /// not "the tasks are gone" (§3.4).
    pub fn uidvalidity_changed(&self, new_uidvalidity: u32) -> bool {
        match self.uidvalidity {
            Some(current) => current != new_uidvalidity,
            // First sync: nothing to invalidate.
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// The five task views of §4.1.
///
/// Each is a filtered projection of the one global list — there is no per-view
/// ordering and no per-view list. The predicates below mirror the partial
/// indexes in the schema; keeping the two in step is what makes each view an
/// index scan instead of a table scan, and [`TaskView::includes`] is the
/// executable statement of what each view means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskView {
    /// Actionable now: postponed until at most today, or due by at most today
    /// (which covers overdue), plus tasks completed today, which linger struck
    /// through until the next local midnight so the action feels confirmed and
    /// undo stays reachable (§10.2).
    Today,
    /// Every open dated task — overdue, today's and the future's — as a plan
    /// read by date. Without this view a task postponed to September is
    /// invisible everywhere until September and cannot be re-planned.
    Upcoming,
    /// Open and undated. Undated tasks live here, *not* in Today.
    Someday,
    /// Completed, and not yet long enough ago to be archived (§18).
    Completed,
    /// Completed longer ago than `tasks.archive_after_days` (§18).
    Archive,
    /// Soft-deleted tasks, and the only view that shows them.
    Trash,
}

impl TaskView {
    /// The stable identifier used across the FFI boundary and in the config.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskView::Today => "today",
            TaskView::Upcoming => "upcoming",
            TaskView::Someday => "someday",
            TaskView::Completed => "completed",
            TaskView::Archive => "archive",
            TaskView::Trash => "trash",
        }
    }

    /// Every view, in sidebar order.
    pub const ALL: [TaskView; 6] = [
        TaskView::Today,
        TaskView::Upcoming,
        TaskView::Someday,
        TaskView::Completed,
        TaskView::Archive,
        TaskView::Trash,
    ];

    /// Does this task belong in this view on this local date?
    ///
    /// Someday and Upcoming partition the open tasks into the undated and the
    /// dated, and Today is the part of Upcoming dated at most today. The other
    /// overlap is a task completed today, which is in both Today and
    /// Completed.
    pub fn includes(&self, task: &Task, today: NaiveDate) -> bool {
        // A trashed task is in the Trash and nowhere else. Checked first
        // because it overrides every other predicate.
        if task.deleted_at.is_some() {
            return matches!(self, TaskView::Trash);
        }

        match self {
            TaskView::Trash => false,
            // Split by how long ago, against a setting this predicate is not
            // given: the queries make that split (§18).
            TaskView::Completed | TaskView::Archive => task.is_completed,
            TaskView::Today => {
                if task.is_completed {
                    return task.was_completed_on(today);
                }
                // Overdue needs no clause of its own: a due date in the past is
                // a due date at most today.
                let dates = task.dates();
                dates.postpone().is_some_and(|date| date <= today)
                    || dates.due().is_some_and(|date| date <= today)
            }
            TaskView::Upcoming => !task.is_completed && !task.dates().is_undated(),
            TaskView::Someday => !task.is_completed && task.dates().is_undated(),
        }
    }
}

impl std::str::FromStr for TaskView {
    type Err = TaskError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        TaskView::ALL
            .into_iter()
            .find(|view| view.as_str() == value)
            .ok_or_else(|| TaskError::UnknownView(value.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Updates
// ---------------------------------------------------------------------------

/// A partial edit of a task.
///
/// Every field is `Option`: absent means "leave alone". The clearable fields
/// are `Option<Option<_>>`, where `Some(None)` means "clear it" — the
/// distinction between *not mentioned* and *explicitly emptied* is the
/// difference between a rename and a wiped due date.
///
/// Dates and the recurrence rule are carried as the strings the wire and the
/// FFI boundary use (`YYYY-MM-DD`, an `RRULE`) rather than as parsed values, so
/// this type stays serialisable without pulling chrono's serde feature into the
/// workspace. They are parsed and validated in [`TaskUpdates::apply`], which is
/// the only place that may turn one into a stored value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskUpdates {
    pub subject: Option<String>,
    pub body_text: Option<Option<String>>,

    /// `YYYY-MM-DD`, or `Some(None)` to clear.
    pub postpone_date: Option<Option<String>>,
    /// `YYYY-MM-DD`, or `Some(None)` to clear.
    pub due_date: Option<Option<String>>,

    /// The complete new set of tag ids; the diff against the old set becomes
    /// the log entries.
    pub tags: Option<Vec<String>>,

    /// An `RRULE`, or `Some(None)` to stop repeating.
    pub recurrence: Option<Option<String>>,
    pub recurrence_from_completion: Option<bool>,

    pub is_working_on: Option<bool>,

    /// The complete new checklist (§17); the diff against the old one becomes
    /// the log entries.
    pub steps: Option<Vec<TaskStep>>,

    /// `Some(true)` completes — which, for a recurring task, advances it
    /// instead. `Some(false)` un-completes.
    pub is_completed: Option<bool>,

    /// `Some(true)` trashes (soft delete), `Some(false)` restores.
    pub is_trashed: Option<bool>,

    pub position: Option<f64>,
}

impl TaskUpdates {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_subject(mut self, subject: String) -> Self {
        self.subject = Some(subject);
        self
    }

    pub fn with_body_text(mut self, body_text: Option<String>) -> Self {
        self.body_text = Some(body_text);
        self
    }

    pub fn with_postpone_date(mut self, postpone_date: Option<NaiveDate>) -> Self {
        self.postpone_date = Some(postpone_date.map(|date| date.to_string()));
        self
    }

    pub fn with_due_date(mut self, due_date: Option<NaiveDate>) -> Self {
        self.due_date = Some(due_date.map(|date| date.to_string()));
        self
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = Some(tags);
        self
    }

    pub fn with_recurrence(mut self, rule: Option<&RecurrenceRule>) -> Self {
        self.recurrence = Some(rule.map(|rule| rule.to_string()));
        self
    }

    pub fn with_recurrence_from_completion(mut self, from_completion: bool) -> Self {
        self.recurrence_from_completion = Some(from_completion);
        self
    }

    pub fn with_steps(mut self, steps: Vec<TaskStep>) -> Self {
        self.steps = Some(steps);
        self
    }

    pub fn with_is_working_on(mut self, is_working_on: bool) -> Self {
        self.is_working_on = Some(is_working_on);
        self
    }

    pub fn with_is_completed(mut self, is_completed: bool) -> Self {
        self.is_completed = Some(is_completed);
        self
    }

    pub fn with_is_trashed(mut self, is_trashed: bool) -> Self {
        self.is_trashed = Some(is_trashed);
        self
    }

    pub fn with_position(mut self, position: f64) -> Self {
        self.position = Some(position);
        self
    }

    /// Apply these updates to a task.
    ///
    /// This is the only sanctioned way to change a task, and it is a function
    /// of the old task rather than a mutation of it so that the caller cannot
    /// half-apply an edit that turns out to be invalid. It:
    ///
    /// - refuses a date that is not a date;
    /// - writes a log entry for every field that actually changed, which is
    ///   what undo (§10.3) and the timeline (§11) read;
    /// - advances a recurring task instead of completing it (§8);
    /// - bumps the revision and sets the dirty flag **only if something
    ///   changed**, because a spurious revision bump is a conflict this task
    ///   would win against a real edit made on another machine.
    ///
    /// `registry` is consulted only to record tag *names* alongside their ids
    /// in the log, so an entry still reads as "Tagged errands" after the tag
    /// has been deleted.
    pub fn apply(
        &self,
        task: &Task,
        registry: &[TaskTag],
        ctx: &ChangeContext<'_>,
    ) -> Result<TaskChange, TaskError> {
        let mut next = task.clone();
        let mut appended: Vec<TaskLogEntry> = Vec::new();
        let mut reminder_shift = None;
        // Tracked separately from `appended` because not every real change is
        // log-worthy: the focus flag changes several times an hour and would
        // bury the timeline it was written into.
        let mut changed = false;

        if let Some(subject) = &self.subject
            && *subject != next.subject
        {
            appended.push(TaskLogEntry::new(
                TaskEvent::TitleChanged {
                    from: next.subject.clone(),
                    to: subject.clone(),
                },
                ctx,
            ));
            next.subject = subject.clone();
            changed = true;
        }

        if let Some(body) = &self.body_text
            && *body != next.body_text
        {
            appended.push(TaskLogEntry::new(TaskEvent::BodyChanged, ctx));
            next.body_text = body.clone();
            changed = true;
        }

        // Dates.
        let before = next.dates_checked()?;
        let postpone = self
            .postpone_date
            .as_ref()
            .map(|raw| parse_optional_date(raw.as_deref(), "postpone"))
            .transpose()?;
        let due = self
            .due_date
            .as_ref()
            .map(|raw| parse_optional_date(raw.as_deref(), "due"))
            .transpose()?;
        let after = match (postpone, due) {
            (Some(postpone), Some(due)) => TaskDates::new(postpone, due),
            (Some(postpone), None) => before.with_postpone(postpone),
            (None, Some(due)) => before.with_due(due),
            (None, None) => before,
        };
        if after != before {
            next.set_dates(after);
            appended.extend(date_change_entries(before, after, ctx));
            changed = true;
        }

        if let Some(tags) = &self.tags {
            // Compared as a set rather than as a sequence: tags are unordered
            // on the wire, and treating a reshuffle of the same ids as an edit
            // would upload a revision that says nothing.
            let entries = tag_change_entries(&next.tags, tags, registry, ctx);
            if !entries.is_empty() {
                appended.extend(entries);
                next.tags = tags.clone();
                changed = true;
            }
        }

        if let Some(recurrence) = &self.recurrence {
            // Parse before storing: the canonical form is what goes in the
            // column, so a rule typed in any accepted spelling compares equal
            // to itself on the next edit.
            let canonical = match recurrence {
                Some(raw) => Some(RecurrenceRule::parse(raw)?.to_string()),
                None => None,
            };
            if canonical != next.recurrence {
                appended.push(TaskLogEntry::new(
                    TaskEvent::RecurrenceChanged {
                        from: next.recurrence.clone(),
                        to: canonical.clone(),
                    },
                    ctx,
                ));
                next.recurrence = canonical;
                changed = true;
            }
        }

        // Not logged: the flag only decides how the *next* advance is computed,
        // and that advance logs what it did. It does travel on the wire, so it
        // is a real change.
        if let Some(from_completion) = self.recurrence_from_completion
            && from_completion != next.recurrence_from_completion
        {
            next.recurrence_from_completion = from_completion;
            changed = true;
        }

        if let Some(steps) = &self.steps
            && *steps != next.steps
        {
            appended.extend(step_change_entries(&next.steps, steps, ctx));
            next.steps = steps.clone();
            changed = true;
        }

        // Neither logged nor a change: "currently working on" is local focus
        // state. Bumping the revision for it would queue an upload that says
        // nothing and, worse, hand this machine a revision high enough to beat
        // a real edit made on another one.
        if let Some(is_working_on) = self.is_working_on {
            next.is_working_on = is_working_on;
        }

        if let Some(position) = self.position
            && position != next.position
        {
            appended.push(TaskLogEntry::new(
                TaskEvent::Reordered {
                    from: next.position,
                    to: position,
                },
                ctx,
            ));
            next.position = position;
            changed = true;
        }

        if let Some(is_completed) = self.is_completed
            && is_completed != next.is_completed
        {
            if is_completed {
                reminder_shift = complete(&mut next, ctx, &mut appended)?;
                // Done is not in progress (§19).
                next.is_working_on = false;
            } else {
                next.is_completed = false;
                next.completed_at = None;
                appended.push(TaskLogEntry::new(TaskEvent::Uncompleted, ctx));
            }
            changed = true;
        }

        if let Some(is_trashed) = self.is_trashed
            && is_trashed != next.deleted_at.is_some()
        {
            if is_trashed {
                next.is_working_on = false;
                next.deleted_at = Some(format_timestamp(ctx.now));
                appended.push(TaskLogEntry::new(TaskEvent::Trashed, ctx));
            } else {
                next.deleted_at = None;
                appended.push(TaskLogEntry::new(TaskEvent::Restored, ctx));
            }
            changed = true;
        }

        if changed {
            next.revision += 1;
            next.updated_at = format_timestamp(ctx.now);
            next.is_dirty = true;
            next.log.extend(appended.iter().cloned());
        }

        Ok(TaskChange {
            task: next,
            appended,
            reminder_shift,
            changed,
        })
    }
}

/// Complete a task, or advance it if it recurs.
///
/// §8: completing a recurring task does not end it. It logs the completion,
/// moves the dates to the next occurrence and stays open, keeping its identity,
/// its tags, its links and its log — one timeline rather than a graveyard of
/// copies. The series only really ends when `UNTIL`/`COUNT` says so, which
/// [`Advancement::Finished`] reports, and then the ordinary completion applies.
fn complete(
    task: &mut Task,
    ctx: &ChangeContext<'_>,
    appended: &mut Vec<TaskLogEntry>,
) -> Result<Option<ReminderShift>, TaskError> {
    appended.push(TaskLogEntry::new(
        TaskEvent::Completed {
            on: ctx.today.to_string(),
        },
        ctx,
    ));

    let rule = task.recurrence_rule()?;
    let Some(rule) = rule else {
        task.is_completed = true;
        task.completed_at = Some(format_timestamp(ctx.now));
        return Ok(None);
    };

    let schedule = task.dates_checked()?;
    let mode = if task.recurrence_from_completion {
        AdvanceFrom::Completion
    } else {
        AdvanceFrom::Scheduled
    };
    // The occurrence being completed. The entry for this completion went into
    // `appended`, not into the task's log, so the count is still the one for
    // the occurrence the task is sitting on.
    let elapsed = task.current_occurrence();

    match advance(&rule, schedule.into(), elapsed, ctx.today, mode) {
        Advancement::Next(advanced) => {
            let from = schedule.anchor().unwrap_or(ctx.today);
            task.set_dates(advanced.schedule.into());
            task.is_completed = false;
            task.completed_at = None;
            // The next occurrence starts its checklist afresh (§17).
            for step in &mut task.steps {
                step.done = false;
            }
            appended.push(TaskLogEntry::new(
                TaskEvent::RecurrenceAdvanced {
                    from: from.to_string(),
                    to: advanced.anchor.to_string(),
                    skipped: advanced.steps.saturating_sub(1),
                },
                ctx,
            ));
            Ok(Some(ReminderShift {
                from,
                to: advanced.anchor,
            }))
        }
        Advancement::Finished => {
            task.is_completed = true;
            task.completed_at = Some(format_timestamp(ctx.now));
            Ok(None)
        }
    }
}

/// One entry per date that moved.
fn date_change_entries(
    before: TaskDates,
    after: TaskDates,
    ctx: &ChangeContext<'_>,
) -> Vec<TaskLogEntry> {
    let mut entries = Vec::new();
    if before.postpone() != after.postpone() {
        entries.push(TaskLogEntry::new(
            TaskEvent::DateChanged {
                field: DateField::Postpone,
                from: before.postpone().map(|date| date.to_string()),
                to: after.postpone().map(|date| date.to_string()),
            },
            ctx,
        ));
    }
    if before.due() != after.due() {
        entries.push(TaskLogEntry::new(
            TaskEvent::DateChanged {
                field: DateField::Due,
                from: before.due().map(|date| date.to_string()),
                to: after.due().map(|date| date.to_string()),
            },
            ctx,
        ));
    }
    entries
}

/// The added/removed diff between two tag sets, as log entries.
fn tag_change_entries(
    before: &[String],
    after: &[String],
    registry: &[TaskTag],
    ctx: &ChangeContext<'_>,
) -> Vec<TaskLogEntry> {
    let name_of = |tag_uuid: &str| {
        registry
            .iter()
            .find(|tag| tag.tag_uuid == tag_uuid)
            .map(|tag| tag.name.clone())
    };

    let mut entries = Vec::new();
    for tag_uuid in after.iter().filter(|tag| !before.contains(tag)) {
        entries.push(TaskLogEntry::new(
            TaskEvent::TagAdded {
                tag_uuid: tag_uuid.clone(),
                name: name_of(tag_uuid),
            },
            ctx,
        ));
    }
    for tag_uuid in before.iter().filter(|tag| !after.contains(tag)) {
        entries.push(TaskLogEntry::new(
            TaskEvent::TagRemoved {
                tag_uuid: tag_uuid.clone(),
                name: name_of(tag_uuid),
            },
            ctx,
        ));
    }
    entries
}

#[cfg(test)]
mod tests {

    /// An id off the wire names a directory under the attachment cache, so it
    /// is an identifier or nothing: no dots, no separators, no `..`.
    #[test]
    fn a_wire_id_is_an_identifier_and_never_a_path() {
        use super::{MAX_WIRE_ID_LEN, is_wire_id};
        assert!(is_wire_id("11111111-2222-3333-4444-555555555555"));
        assert!(is_wire_id("task-a"));
        assert!(is_wire_id("things_step_1"));
        assert!(!is_wire_id(""));
        assert!(!is_wire_id("."));
        assert!(!is_wire_id(".."));
        assert!(!is_wire_id("../.."));
        assert!(!is_wire_id("a/b"));
        assert!(!is_wire_id("a\\b"));
        assert!(!is_wire_id("a.b"));
        assert!(!is_wire_id("a b"));
        assert!(!is_wire_id("a\u{0}b"));
        assert!(!is_wire_id("é"));
        assert!(!is_wire_id(&"a".repeat(MAX_WIRE_ID_LEN + 1)));
        assert!(is_wire_id(&"a".repeat(MAX_WIRE_ID_LEN)));
    }

    /// Whatever the three parts of a cache path were built from, the path is
    /// three plain components under `tasks/`.
    #[test]
    fn a_cache_path_stays_three_components_under_tasks() {
        use super::TaskAttachment;
        use std::path::{Component, Path};
        for (task, attachment, name) in [
            ("../../..", "x", "a.plist"),
            ("t", "../../../Library/LaunchAgents/com.x", "agent.plist"),
            ("t", "a", "../../.ssh/authorized_keys"),
            ("", "", ""),
            ("/", "\\", "/"),
        ] {
            let path = TaskAttachment::cache_path(task, attachment, name);
            let components: Vec<_> = Path::new(&path).components().collect();
            assert_eq!(components.len(), 3, "{path}");
            assert!(
                components.iter().all(|c| matches!(c, Component::Normal(_))),
                "{path}"
            );
            assert_eq!(components[0], Component::Normal("tasks".as_ref()));
        }
        assert_eq!(
            TaskAttachment::cache_path("task-a", "att-1", "notes.txt"),
            "tasks/task-a/att-1-notes.txt"
        );
    }

    /// Every change to a checklist but a reorder is logged by the step's
    /// title, and any change is an edit with a revision (todo-spec §17).
    #[test]
    fn a_checklist_change_logs_each_step_and_bumps_the_revision() {
        let ctx = ctx();
        let task = Task::create(
            "Review the roadmap".to_string(),
            "Work".to_string(),
            1.0,
            &ctx,
        )
        .task;
        let read = TaskStep::new("Read it".to_string());
        let send = TaskStep::new("Send comments".to_string());
        let added = TaskUpdates::new()
            .with_steps(vec![read.clone(), send.clone()])
            .apply(&task, &[], &ctx)
            .unwrap();
        assert!(added.changed);
        assert_eq!(added.task.revision, task.revision + 1);
        let texts = |change: &TaskChange| -> Vec<String> {
            change
                .appended
                .iter()
                .map(TaskLogEntry::rendered_text)
                .collect()
        };
        assert_eq!(
            texts(&added),
            ["Step 'Read it' added", "Step 'Send comments' added"]
        );

        let ticked = TaskStep {
            done: true,
            title: "Read all of it".to_string(),
            ..read.clone()
        };
        let edited = TaskUpdates::new()
            .with_steps(vec![send.clone(), ticked])
            .apply(&added.task, &[], &ctx)
            .unwrap();
        assert_eq!(
            texts(&edited),
            [
                "Step 'Read it' renamed to 'Read all of it'",
                "Step 'Read all of it' done"
            ]
        );

        let reordered = TaskUpdates::new()
            .with_steps(edited.task.steps.iter().rev().cloned().collect())
            .apply(&edited.task, &[], &ctx)
            .unwrap();
        assert!(reordered.changed);
        assert!(reordered.appended.is_empty());

        let unchanged = TaskUpdates::new()
            .with_steps(reordered.task.steps.clone())
            .apply(&reordered.task, &[], &ctx)
            .unwrap();
        assert!(!unchanged.changed);
    }
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("test date")
    }

    fn instant(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("test instant")
            .with_timezone(&Utc)
    }

    fn ctx() -> ChangeContext<'static> {
        ChangeContext::new(instant("2026-08-06T10:00:00+00:00"), date("2026-08-06"))
            .with_device_id(Some("device-a"))
    }

    fn task() -> Task {
        Task::create("Write the spec".into(), "Work".into(), 1024.0, &ctx()).task
    }

    // --- dates -----------------------------------------------------------

    #[test]
    fn a_civil_day_survives_the_round_trip_through_its_index_column() {
        for day in ["1970-01-01", "1969-07-20", "2026-08-06", "2027-08-06"] {
            let parsed = date(day);
            assert_eq!(civil_day_from_unix(civil_day_unix(parsed)), Some(parsed));
        }
    }

    #[test]
    fn a_civil_day_is_encoded_as_midnight_utc_so_both_sides_of_a_query_agree() {
        // The encoding has to be exactly "days since the epoch", or a query
        // that encodes today the same way would compare against the wrong day.
        assert_eq!(civil_day_unix(date("1970-01-02")), 86_400);
        assert_eq!(civil_day_unix(date("1969-12-31")), -86_400);
    }

    #[test]
    fn either_date_moves_without_the_other() {
        let mut task = task();
        task.set_dates(TaskDates::new(
            Some(date("2026-08-20")),
            Some(date("2026-08-25")),
        ));

        let earlier_due = TaskUpdates::new()
            .with_due_date(Some(date("2026-08-10")))
            .apply(&task, &[], &ctx())
            .unwrap();
        assert_eq!(
            earlier_due.task.postpone_date.as_deref(),
            Some("2026-08-20")
        );
        assert_eq!(earlier_due.task.due_date.as_deref(), Some("2026-08-10"));

        let later_postpone = TaskUpdates::new()
            .with_postpone_date(Some(date("2026-09-01")))
            .apply(&task, &[], &ctx())
            .unwrap();
        assert_eq!(
            later_postpone.task.postpone_date.as_deref(),
            Some("2026-09-01")
        );
        assert_eq!(later_postpone.task.due_date.as_deref(), Some("2026-08-25"));
        let fields: Vec<DateField> = later_postpone
            .appended
            .iter()
            .filter_map(|entry| match &entry.event {
                TaskEvent::DateChanged { field, .. } => Some(*field),
                _ => None,
            })
            .collect();
        assert_eq!(fields, vec![DateField::Postpone]);
    }

    #[test]
    fn a_date_that_is_not_a_date_is_reported_rather_than_silently_dropped() {
        let mut task = task();
        task.postpone_date = Some("someday".to_string());
        assert!(matches!(
            task.dates_checked(),
            Err(TaskError::InvalidDate { .. })
        ));
        // Views still have to render it, so the lenient accessor copes.
        assert!(task.dates().is_undated());
    }

    #[test]
    fn setting_a_date_writes_both_the_text_and_the_index_column() {
        let change = TaskUpdates::new()
            .with_postpone_date(Some(date("2026-08-20")))
            .apply(&task(), &[], &ctx())
            .unwrap();
        assert_eq!(change.task.postpone_date.as_deref(), Some("2026-08-20"));
        assert_eq!(
            change.task.postpone_date_unix,
            Some(civil_day_unix(date("2026-08-20")))
        );
    }

    // --- revisions and the dirty flag ------------------------------------

    #[test]
    fn a_new_task_starts_at_revision_one_with_a_creation_entry() {
        let change = Task::create("Buy milk".into(), "Work".into(), 1024.0, &ctx());
        assert_eq!(change.task.revision, 1);
        assert!(change.task.is_dirty);
        assert!(!change.task.task_uuid.is_empty());
        assert_eq!(change.appended.len(), 1);
        assert_eq!(change.appended[0].event_type(), "created");
        assert_eq!(change.task.log.len(), 1);
    }

    #[test]
    fn an_update_that_changes_nothing_does_not_bump_the_revision() {
        let task = task();
        let change = TaskUpdates::new()
            .with_subject(task.subject.clone())
            .apply(&task, &[], &ctx())
            .unwrap();
        assert!(change.is_noop());
        assert_eq!(change.task.revision, task.revision);
        assert_eq!(change.task.updated_at, task.updated_at);
    }

    #[test]
    fn a_real_edit_bumps_the_revision_once_and_marks_the_task_dirty() {
        let mut task = task();
        task.is_dirty = false;
        let change = TaskUpdates::new()
            .with_subject("Write the other spec".into())
            .with_body_text(Some("notes".into()))
            .apply(&task, &[], &ctx())
            .unwrap();
        assert_eq!(change.task.revision, task.revision + 1);
        assert!(change.task.is_dirty);
        assert_eq!(change.appended.len(), 2);
    }

    // --- completion and recurrence ---------------------------------------

    #[test]
    fn completing_a_one_off_task_completes_it() {
        let change = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&task(), &[], &ctx())
            .unwrap();
        assert!(change.task.is_completed);
        assert!(change.task.completed_at.is_some());
        assert!(change.reminder_shift.is_none());
    }

    #[test]
    fn completing_a_recurring_task_advances_it_instead_of_ending_it() {
        let mut task = task();
        task.recurrence = Some("FREQ=WEEKLY".to_string());
        task.set_dates(TaskDates::new(Some(date("2026-08-06")), None));

        let change = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&task, &[], &ctx())
            .unwrap();

        assert!(!change.task.is_completed);
        assert_eq!(change.task.completed_at, None);
        assert_eq!(change.task.postpone_date.as_deref(), Some("2026-08-13"));
        // The identity and the timeline survive; there is no copy.
        assert_eq!(change.task.task_uuid, task.task_uuid);
        assert_eq!(
            change.reminder_shift,
            Some(ReminderShift {
                from: date("2026-08-06"),
                to: date("2026-08-13"),
            })
        );
        let kinds: Vec<&str> = change
            .appended
            .iter()
            .map(|entry| entry.event_type())
            .collect();
        assert_eq!(kinds, vec!["completed", "recurrence_advanced"]);
    }

    #[test]
    fn a_recurring_task_keeps_the_gap_between_its_two_dates() {
        let mut task = task();
        task.recurrence = Some("FREQ=WEEKLY".to_string());
        task.set_dates(TaskDates::new(
            Some(date("2026-08-06")),
            Some(date("2026-08-08")),
        ));

        let change = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&task, &[], &ctx())
            .unwrap();

        assert_eq!(change.task.postpone_date.as_deref(), Some("2026-08-13"));
        assert_eq!(change.task.due_date.as_deref(), Some("2026-08-15"));
    }

    #[test]
    fn a_recurring_task_really_ends_when_its_count_runs_out() {
        let mut task = task();
        task.recurrence = Some("FREQ=WEEKLY;COUNT=1".to_string());
        task.set_dates(TaskDates::new(Some(date("2026-08-06")), None));

        let change = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&task, &[], &ctx())
            .unwrap();

        assert!(change.task.is_completed);
        assert!(change.task.completed_at.is_some());
        assert!(change.reminder_shift.is_none());
    }

    #[test]
    fn the_occurrence_a_series_sits_on_is_read_back_out_of_the_log() {
        let mut task = task();
        task.recurrence = Some("FREQ=DAILY;COUNT=2".to_string());
        task.set_dates(TaskDates::new(Some(date("2026-08-06")), None));
        assert_eq!(task.current_occurrence(), 1);

        // First completion advances onto occurrence 2; the second exhausts
        // COUNT=2 and really ends the series.
        let first = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&task, &[], &ctx())
            .unwrap()
            .task;
        assert!(!first.is_completed);
        assert_eq!(first.current_occurrence(), 2);

        let ctx = ChangeContext::new(instant("2026-08-07T10:00:00+00:00"), date("2026-08-07"));
        let second = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&first, &[], &ctx)
            .unwrap()
            .task;
        assert!(second.is_completed);
    }

    #[test]
    fn uncompleting_clears_the_completion_stamp() {
        let completed = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&task(), &[], &ctx())
            .unwrap()
            .task;
        let change = TaskUpdates::new()
            .with_is_completed(false)
            .apply(&completed, &[], &ctx())
            .unwrap();
        assert!(!change.task.is_completed);
        assert_eq!(change.task.completed_at, None);
        assert_eq!(change.appended[0].event_type(), "uncompleted");
    }

    // --- trash ------------------------------------------------------------

    #[test]
    fn trashing_is_a_soft_delete_and_restoring_undoes_it() {
        let trashed = TaskUpdates::new()
            .with_is_trashed(true)
            .apply(&task(), &[], &ctx())
            .unwrap()
            .task;
        assert!(trashed.deleted_at.is_some());
        assert!(!trashed.is_open());

        let restored = TaskUpdates::new()
            .with_is_trashed(false)
            .apply(&trashed, &[], &ctx())
            .unwrap()
            .task;
        assert_eq!(restored.deleted_at, None);
        assert_eq!(restored.revision, trashed.revision + 1);
    }

    // --- tags -------------------------------------------------------------

    #[test]
    fn a_tag_change_is_logged_with_the_name_the_tag_had_at_the_time() {
        let tag = TaskTag::new("errands".into(), 1024.0);
        let registry = vec![tag.clone()];

        let change = TaskUpdates::new()
            .with_tags(vec![tag.tag_uuid.clone()])
            .apply(&task(), &registry, &ctx())
            .unwrap();

        assert_eq!(change.task.tags, vec![tag.tag_uuid.clone()]);
        assert_eq!(
            change.appended[0].event,
            TaskEvent::TagAdded {
                tag_uuid: tag.tag_uuid.clone(),
                name: Some("errands".into()),
            }
        );
        assert_eq!(change.appended[0].rendered_text(), "Tagged errands");
    }

    #[test]
    fn removing_a_tag_logs_the_removal() {
        let tag = TaskTag::new("errands".into(), 1024.0);
        let mut task = task();
        task.tags = vec![tag.tag_uuid.clone()];

        let change = TaskUpdates::new()
            .with_tags(Vec::new())
            .apply(&task, std::slice::from_ref(&tag), &ctx())
            .unwrap();

        assert!(change.task.tags.is_empty());
        assert_eq!(change.appended[0].event_type(), "tag_removed");
    }

    #[test]
    fn an_unresolvable_tag_id_renders_as_itself_rather_than_vanishing() {
        let known = TaskTag::new("errands".into(), 1024.0);
        assert_eq!(
            tag_label(&known.tag_uuid, std::slice::from_ref(&known)),
            "errands"
        );
        assert_eq!(
            tag_label("not-in-the-registry", &[known]),
            "not-in-the-registry"
        );
    }

    // --- the log ----------------------------------------------------------

    #[test]
    fn an_event_survives_the_round_trip_through_its_two_columns() {
        let events = vec![
            TaskEvent::Created {
                title: "Buy milk".into(),
            },
            TaskEvent::DateChanged {
                field: DateField::Due,
                from: None,
                to: Some("2026-08-20".into()),
            },
            TaskEvent::Uncompleted,
            TaskEvent::RemindersMissed { count: 12 },
            TaskEvent::Reordered {
                from: 1024.0,
                to: 512.0,
            },
        ];
        for event in events {
            let restored = TaskEvent::from_parts(event.kind(), event.detail_json().as_deref());
            assert_eq!(restored, event);
        }
    }

    #[test]
    fn an_event_type_this_build_does_not_know_is_kept_verbatim() {
        // A newer build's entry passes through this one on its way back to the
        // server; losing it would amputate the shared history.
        let restored =
            TaskEvent::from_parts("delegated", Some(r#"{"event":"delegated","to":"x"}"#));
        assert_eq!(
            restored,
            TaskEvent::Unknown {
                event_type: "delegated".into(),
                detail: Some(r#"{"event":"delegated","to":"x"}"#.into()),
            }
        );
        assert_eq!(restored.kind(), "delegated");
        assert_eq!(
            restored.detail_json().as_deref(),
            Some(r#"{"event":"delegated","to":"x"}"#)
        );
    }

    #[test]
    fn a_detail_that_does_not_match_its_event_type_is_kept_verbatim() {
        let restored = TaskEvent::from_parts("created", Some("not json at all"));
        assert!(matches!(restored, TaskEvent::Unknown { .. }));
        assert_eq!(restored.detail_json().as_deref(), Some("not json at all"));
    }

    #[test]
    fn merging_two_logs_keeps_every_entry_from_both() {
        let ctx = ctx();
        let shared = TaskLogEntry::new(TaskEvent::BodyChanged, &ctx);
        let mine = TaskLogEntry::new(TaskEvent::Trashed, &ctx);
        let mut theirs = TaskLogEntry::new(TaskEvent::Restored, &ctx);
        theirs.occurred_at_unix += 60;

        let merged = merge_logs(
            &[shared.clone(), mine.clone()],
            &[shared.clone(), theirs.clone()],
        );

        assert_eq!(merged.len(), 3);
        assert_eq!(merged.last().unwrap().entry_uuid, theirs.entry_uuid);
        assert!(merged.iter().any(|e| e.entry_uuid == mine.entry_uuid));
    }

    #[test]
    fn merging_a_log_with_itself_changes_nothing() {
        let ctx = ctx();
        let entries = vec![
            TaskLogEntry::new(TaskEvent::BodyChanged, &ctx),
            TaskLogEntry::new(TaskEvent::Trashed, &ctx),
        ];
        let once = merge_logs(&entries, &[]);
        let twice = merge_logs(&once, &entries);
        assert_eq!(once, twice);
    }

    // --- views ------------------------------------------------------------

    fn dated(postpone: Option<&str>, due: Option<&str>) -> Task {
        let mut task = task();
        task.set_dates(TaskDates::new(postpone.map(date), due.map(date)));
        task
    }

    #[test]
    fn upcoming_holds_every_dated_task_and_today_the_ones_due_by_today() {
        use TaskView::{Someday, Today, Upcoming};
        let today = date("2026-08-06");
        let cases = [
            (dated(Some("2026-08-06"), None), vec![Today, Upcoming]),
            (dated(Some("2026-08-01"), None), vec![Today, Upcoming]),
            (dated(None, Some("2026-07-01")), vec![Today, Upcoming]), // overdue
            (dated(Some("2026-09-01"), None), vec![Upcoming]),
            (dated(None, Some("2026-09-01")), vec![Upcoming]),
            (dated(None, None), vec![Someday]),
        ];
        for (task, expected) in cases {
            for view in [Today, Upcoming, Someday] {
                assert_eq!(
                    view.includes(&task, today),
                    expected.contains(&view),
                    "{:?} in {view:?}",
                    task.dates()
                );
            }
        }
    }

    #[test]
    fn an_undated_task_is_in_someday_and_never_in_today() {
        let today = date("2026-08-06");
        let task = dated(None, None);
        assert!(TaskView::Someday.includes(&task, today));
        assert!(!TaskView::Today.includes(&task, today));
    }

    #[test]
    fn a_task_completed_today_lingers_in_today_until_midnight() {
        let change = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&dated(Some("2026-08-06"), None), &[], &ctx())
            .unwrap();
        assert!(TaskView::Today.includes(&change.task, date("2026-08-06")));
        assert!(!TaskView::Today.includes(&change.task, date("2026-08-07")));
        assert!(TaskView::Completed.includes(&change.task, date("2026-08-07")));
    }

    #[test]
    fn a_trashed_task_is_in_the_trash_and_nowhere_else() {
        let today = date("2026-08-06");
        let trashed = TaskUpdates::new()
            .with_is_trashed(true)
            .apply(&dated(Some("2026-08-06"), None), &[], &ctx())
            .unwrap()
            .task;
        assert!(TaskView::Trash.includes(&trashed, today));
        for view in [
            TaskView::Today,
            TaskView::Upcoming,
            TaskView::Someday,
            TaskView::Completed,
        ] {
            assert!(!view.includes(&trashed, today), "{view:?}");
        }
    }

    #[test]
    fn a_view_name_survives_the_round_trip_across_the_ffi_boundary() {
        for view in TaskView::ALL {
            assert_eq!(view.as_str().parse::<TaskView>(), Ok(view));
        }
        assert_eq!(
            "inbox".parse::<TaskView>(),
            Err(TaskError::UnknownView("inbox".into()))
        );
    }

    #[test]
    fn overdue_is_decided_on_civil_days_not_instants() {
        let task = dated(None, Some("2026-08-05"));
        assert!(task.is_overdue(date("2026-08-06")));
        assert!(!task.is_overdue(date("2026-08-05")));
        assert!(!task.is_overdue(date("2026-08-04")));
    }

    #[test]
    fn a_completed_task_is_never_overdue() {
        let completed = TaskUpdates::new()
            .with_is_completed(true)
            .apply(&dated(None, Some("2026-07-01")), &[], &ctx())
            .unwrap()
            .task;
        assert!(!completed.is_overdue(date("2026-08-06")));
    }

    #[test]
    fn due_soon_is_the_next_three_days() {
        assert!(dated(None, Some("2026-08-09")).is_due_soon(date("2026-08-06")));
        assert!(!dated(None, Some("2026-08-10")).is_due_soon(date("2026-08-06")));
        assert!(!dated(None, Some("2026-08-01")).is_due_soon(date("2026-08-06")));
    }

    // --- reminders and attachments ---------------------------------------

    #[test]
    fn a_notification_id_depends_only_on_things_that_survive_a_rebuilt_cache() {
        let reminder = TaskReminder::new(1, "2026-08-06T09:00:00+00:00".into(), 1_785_920_400);
        let rebuilt = TaskReminder::new(97, "2026-08-06T09:00:00+00:00".into(), 1_785_920_400);
        assert_eq!(
            reminder.notification_id("task-uuid"),
            rebuilt.notification_id("task-uuid")
        );
        assert_ne!(
            reminder.notification_id("task-uuid"),
            reminder.notification_id("other-uuid")
        );
    }

    #[test]
    fn moving_a_reminder_changes_the_identifier_it_is_scheduled_under() {
        let before = TaskReminder::new(1, "2026-08-06T09:00:00+00:00".into(), 1_785_920_400);
        let after = TaskReminder::new(1, "2026-08-07T09:00:00+00:00".into(), 1_786_006_800);
        assert_ne!(
            before.notification_id("task-uuid"),
            after.notification_id("task-uuid")
        );
    }

    #[test]
    fn an_attachment_filename_from_the_wire_cannot_escape_its_directory() {
        // The filename arrives from a remote message, so it is hostile input.
        // What makes it safe is that the result is exactly three components
        // deep and none of them traverses.
        let path = TaskAttachment::cache_path("task", "att", "../../.ssh/authorized_keys");
        let components: Vec<&str> = path.split('/').collect();
        assert_eq!(components.len(), 3, "{path}");
        assert_eq!(&components[..2], &["tasks", "task"]);
        assert!(!components.iter().any(|part| *part == ".." || *part == "."));
        assert_eq!(path, "tasks/task/att-_.._.ssh_authorized_keys");
    }

    #[test]
    fn a_filename_that_sanitises_away_to_nothing_still_has_a_name() {
        let path = TaskAttachment::cache_path("task", "att", "..");
        assert_eq!(path, "tasks/task/att-attachment");
    }

    #[test]
    fn two_attachments_with_the_same_name_get_different_paths() {
        let one = TaskAttachment::create(1, "task", "report.pdf".into(), None, None);
        let two = TaskAttachment::create(1, "task", "report.pdf".into(), None, None);
        assert_ne!(one.file_path, two.file_path);
        assert!(!one.is_cached);
    }

    // --- sync state -------------------------------------------------------

    #[test]
    fn the_default_tasks_folder_carries_no_prefix() {
        assert_eq!(TaskSyncState::new("Work".into()).folder_name, "Tasks");
    }

    #[test]
    fn a_first_sync_is_not_a_uidvalidity_change() {
        let mut state = TaskSyncState::new("Work".into());
        assert!(!state.uidvalidity_changed(42));
        state.uidvalidity = Some(42);
        assert!(!state.uidvalidity_changed(42));
        assert!(state.uidvalidity_changed(43));
    }
}
