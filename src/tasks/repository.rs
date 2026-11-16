//! The SQLite cache behind the task feature.
//!
//! The IMAP folder is the source of truth; these tables are a cache that must be
//! fully rebuildable from it (§3). Three rules follow from that, and between
//! them they explain most of the shape of this file.
//!
//! **Every mutation bumps the revision.** Conflicts between two machines are
//! decided by highest revision, ties by highest UID (§3.1), and the dirty flag
//! is cleared by a compare-and-swap on the revision that was uploaded (§3.2). A
//! write path that changes a field without bumping the revision is therefore not
//! a shortcut, it is a lost edit: the other machine's older copy wins the next
//! reconciliation and silently overwrites it. That is why local edits go through
//! exactly one door — [`TaskRepository::apply_to`], which delegates the rules to
//! [`TaskUpdates::apply`] and writes what comes back — and why the raw row
//! writers below are private or explicitly labelled cache-writes.
//!
//! **Dates are civil dates in the machine's own timezone.** Everything a view
//! asks about a date ("is this today?", "is this in the future?") is a question
//! about calendar days, so both sides of every comparison are encoded with
//! [`civil_day_unix`] and the timezone is only consulted to decide *which*
//! calendar day it currently is. The old code compared `NaiveDate::and_utc()`
//! against the wall clock, which in CET made tasks appear and vanish at 01:00
//! (§4.3). The zone is injected ([`LocalZone`]) rather than read from the
//! process environment, because a view whose correctness depends on the
//! machine's timezone has to be testable at a timezone the test chooses.
//!
//! **Every multi-step mutation is one `IMMEDIATE` transaction.** A task's row,
//! its tag links and its log entries are one fact; a crash that lands two of the
//! three leaves a task whose timeline disagrees with itself and whose next
//! upload publishes the disagreement. [`TaskRepository::in_transaction`] opens
//! one, or joins the caller's if there already is one, so that grouping a sync
//! apply into a larger transaction stays possible.

use std::collections::{HashMap, HashSet};

use chrono::{
    DateTime, Days, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc,
};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::model::{
    CachedEmailMetadata, ChangeContext, ReminderShift, Task, TaskAttachment, TaskChange,
    TaskLinkedEmail, TaskListItem, TaskLogEntry, TaskReminder, TaskStep, TaskSyncState, TaskTag,
    TaskUpdates, TaskView, civil_day_unix, format_timestamp, merge_logs,
};
use crate::Result;
use crate::db::Database;

/// The gap a fresh position leaves to its neighbour.
///
/// 1024 rather than 1: the point of a `REAL` position is that inserting between
/// two tasks writes one row, and it can only keep doing that while there is
/// room between them. A power of two also means the midpoints stay exactly
/// representable for the first ten insertions.
pub const POSITION_STEP: f64 = 1024.0;

/// Below this gap between neighbours, the list is renormalised instead of
/// halved again.
///
/// Repeated insertion between the same two tasks halves the gap every time, so
/// f64 runs out after roughly fifty moves and the "midpoint" silently becomes
/// one of the endpoints — two tasks with the same position, in an order that
/// depends on the tiebreak rather than on what the user dragged. The threshold
/// is absolute and enormously larger than the ulp of any position this list will
/// hold (positions are multiples of 1024, so an ulp near 10^6 is about 10^-10),
/// which is deliberate: renormalisation is cheap and rare, and the failure it
/// prevents is silent.
pub const MIN_POSITION_GAP: f64 = 1e-6;

/// How long a trashed task survives before it is expunged for good (§10.1).
pub const TRASH_RETENTION_DAYS: i64 = 30;

/// Default page size for the Completed view, which is paginated rather than
/// capped at an arbitrary hundred (§10.2).
pub const COMPLETED_PAGE_SIZE: usize = 50;

/// Where the tag registry's revision lives.
///
/// The registry syncs as its own message with the same identity/revision rules
/// as a task (§7), but it has no table of its own to carry that revision —
/// `task_tags` holds the entries, not the document. `sync_meta` is the existing
/// integer-valued key/value store, and one row there is cheaper than a table
/// with one row in it.
const TAG_REGISTRY_REVISION_KEY: &str = "task_tag_registry_revision";
/// 1 while this machine has tag changes the server has not seen.
const TAG_REGISTRY_DIRTY_KEY: &str = "task_tag_registry_dirty";
/// The `Message-ID` of the registry copy this machine last wrote or read.
const TAG_REGISTRY_MESSAGE_KEY: &str = "task_tag_registry_message_id";
/// The tags as that copy had them, as JSON: the base a three-way merge
/// compares both machines' changes against (§7).
const TAG_REGISTRY_BASE_KEY: &str = "task_tag_registry_base";

/// When Today's completed tasks were last cleared (§10.2), as RFC 3339, and
/// the same bookkeeping for its message as the registry keeps for its own.
const TODAY_CLEARED_AT_KEY: &str = "today_cleared_at";
const TODAY_CLEARED_REVISION_KEY: &str = "today_cleared_revision";
const TODAY_CLEARED_DIRTY_KEY: &str = "today_cleared_dirty";
const TODAY_CLEARED_MESSAGE_KEY: &str = "today_cleared_message_id";

/// What this machine knows about its copy of the tag registry, for deciding
/// whether it or the server's is the one to keep (§7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryState {
    pub revision: i64,
    /// Tag changes made here that have not been uploaded. Without it, two
    /// machines that each changed a tag offline reach the same revision, and
    /// whichever syncs second never sends its change.
    pub dirty: bool,
    /// Which copy this is. Two copies at one revision are two machines'
    /// edits, and the Message-ID is what tells them apart.
    pub message_id: Option<String>,
}

impl From<i64> for RegistryState {
    fn from(revision: i64) -> Self {
        Self {
            revision,
            ..Default::default()
        }
    }
}

/// Every column of `tasks`, in the order [`TaskRepository::row_to_task`] reads
/// them. Spelled out rather than `SELECT *` so that adding a column cannot
/// silently shift the indices out from under the mapper.
const TASK_COLUMNS: &str = "t.id, t.task_uuid, t.revision, t.subject, t.body_text, \
     t.snooze_until, t.snooze_until_unix, t.deadline, t.deadline_unix, \
     t.created_at, t.updated_at, t.completed_at, t.deleted_at, \
     t.recurrence, t.recurrence_from_completion, t.position, \
     t.is_working_on, t.is_completed, t.is_dirty, \
     t.account_name, t.imap_uid, t.message_id, t.modseq, t.steps";

/// The columns a list view needs: no body, no log, no recurrence rule — only
/// whether there is one. A list of two hundred tasks should not carry two
/// hundred timelines.
const LIST_COLUMNS: &str = "t.id, t.task_uuid, t.subject, t.snooze_until, t.snooze_until_unix, \
     t.deadline, t.deadline_unix, t.completed_at, t.deleted_at, \
     t.recurrence IS NOT NULL, t.position, t.is_working_on, t.is_completed, t.is_dirty, \
     t.account_name, t.imap_uid, \
     COALESCE(json_array_length(t.steps), 0), \
     (SELECT COUNT(*) FROM json_each(t.steps) WHERE json_extract(value, '$.done'))";

/// The `steps` column: the checklist as JSON, NULL when there is none.
fn steps_json(steps: &[TaskStep]) -> Option<String> {
    if steps.is_empty() {
        None
    } else {
        serde_json::to_string(steps).ok()
    }
}

/// The earliest of a task's two dates, for the Upcoming view's ordering.
///
/// `MIN()` with two arguments is SQLite's scalar minimum, and it returns NULL if
/// either argument is NULL — hence the cross-`COALESCE`, which makes both
/// arguments non-NULL whenever the task has at least one date at all.
const EARLIEST_DATE: &str = "MIN(COALESCE(t.snooze_until_unix, t.deadline_unix), \
     COALESCE(t.deadline_unix, t.snooze_until_unix))";

// ---------------------------------------------------------------------------
// Timezone
// ---------------------------------------------------------------------------

/// The machine's timezone, as the date-dependent views need it.
///
/// Two operations, because the views need both directions. Which calendar day
/// it is *now* decides what Today contains; which instants bound a calendar day
/// decides whether a task completed at 23:30 was completed "today" — that
/// question is about an instant (`completed_at`) rather than a stored date, and
/// answering it in UTC drops a completion out of Today an hour before midnight
/// in CET, exactly the bug §4.3 is about.
///
/// It is a trait rather than a call to `chrono::Local` so a test can pin the
/// zone: a view whose correctness depends on the timezone is untestable if the
/// timezone is whatever the machine running the test happens to be set to.
pub trait LocalZone: Send + Sync {
    /// The local civil date an instant falls on.
    fn civil_date(&self, at: DateTime<Utc>) -> NaiveDate;

    /// The instant local midnight of the given civil day falls at.
    ///
    /// Implementations must return the *first* instant of that local day.
    /// Spring-forward can delete local midnight outright (Brazil, Chile,
    /// Lebanon have all done it), and a day that begins at 01:00 still begins.
    fn midnight(&self, day: NaiveDate) -> DateTime<Utc>;

    /// The local wall clock at an instant: the date *and* the time of day.
    ///
    /// Derived from the two questions above — which local day `now` falls in,
    /// and how far past that day's first instant it is — rather than from a
    /// fixed offset, so it is right on both of the days DST makes 23 and 25
    /// hours long.
    fn local_now(&self, now: DateTime<Utc>) -> NaiveDateTime {
        let today = self.civil_date(now);
        let midnight = self.midnight(today);
        let elapsed = now.signed_duration_since(midnight);
        // A zone whose `midnight` and `civil_date` disagree would put this
        // outside the day; clamping keeps the wall clock a wall clock instead of
        // rolling the date under the caller.
        let seconds = elapsed.num_seconds().clamp(0, 86_399);
        today.and_hms_opt(0, 0, 0).unwrap_or_default() + Duration::seconds(seconds)
    }
}

/// The zone chrono reports for this machine, DST rules and all.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemZone;

impl LocalZone for SystemZone {
    fn civil_date(&self, at: DateTime<Utc>) -> NaiveDate {
        at.with_timezone(&chrono::Local).date_naive()
    }

    fn midnight(&self, day: NaiveDate) -> DateTime<Utc> {
        first_instant_of(&chrono::Local, day)
    }
}

/// A zone with a single fixed offset.
///
/// For tests, and for the case where the offset is known rather than the zone.
/// It has no DST rules, which makes it the wrong thing to run a real machine on
/// and the right thing to pin a test to.
#[derive(Debug, Clone, Copy)]
pub struct FixedZone(FixedOffset);

impl FixedZone {
    pub fn new(offset: FixedOffset) -> Self {
        Self(offset)
    }

    /// A zone this many hours east of UTC, or `None` if that is not an offset.
    pub fn east_hours(hours: i32) -> Option<Self> {
        FixedOffset::east_opt(hours * 3600).map(Self)
    }

    pub fn utc() -> Self {
        Self(FixedOffset::east_opt(0).expect("UTC is an offset"))
    }
}

impl LocalZone for FixedZone {
    fn civil_date(&self, at: DateTime<Utc>) -> NaiveDate {
        at.with_timezone(&self.0).date_naive()
    }

    fn midnight(&self, day: NaiveDate) -> DateTime<Utc> {
        first_instant_of(&self.0, day)
    }
}

/// The first instant of a local civil day in an arbitrary chrono timezone.
///
/// Walks forward a minute at a time on the rare day whose midnight does not
/// exist, and takes the earlier of the two on the rare day whose midnight
/// happens twice; on every other day it returns on the first iteration.
fn first_instant_of<Tz: TimeZone>(zone: &Tz, day: NaiveDate) -> DateTime<Utc> {
    for minutes in 0..(24 * 60) {
        let Some(local) = day
            .and_hms_opt(0, 0, 0)
            .and_then(|midnight| midnight.checked_add_signed(chrono::Duration::minutes(minutes)))
        else {
            break;
        };
        match zone.from_local_datetime(&local) {
            LocalResult::Single(at) => return at.with_timezone(&Utc),
            LocalResult::Ambiguous(earlier, _) => return earlier.with_timezone(&Utc),
            LocalResult::None => continue,
        }
    }
    // Unreachable for any real zone: a day in which no local time exists is not
    // a day. Falling back to UTC keeps the function total rather than panicking
    // in a query path.
    Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// What to list: a view, the local date it is evaluated against, an optional
/// search filter and an optional page.
///
/// `today` is a parameter rather than something this reads off the clock so
/// that a caller re-evaluating its views at local midnight (§4.3) and a test
/// asking what the list looked like on a particular day take the same path.
#[derive(Debug, Clone)]
pub struct TaskQuery<'q> {
    pub view: TaskView,
    pub today: NaiveDate,
    /// The search box's contents. Filters the current view and never leaves it
    /// (§4.5).
    pub search: Option<&'q str>,
    /// Page size, or `None` for the whole view.
    pub limit: Option<usize>,
    pub offset: usize,
    /// Today shows tasks completed after this instant, not only after
    /// midnight: what `clear-completed` set on this machine (§10.2).
    pub completed_since: Option<DateTime<Utc>>,
    /// Whether Today shows today's completed tasks at all (§10.2).
    pub completed_in_today: bool,
    /// Completed before this instant is archived (§18): out of Completed and
    /// in Archive. `None` archives nothing.
    pub archived_before: Option<DateTime<Utc>>,
}

impl<'q> TaskQuery<'q> {
    pub fn new(view: TaskView, today: NaiveDate) -> Self {
        Self {
            view,
            today,
            search: None,
            limit: None,
            offset: 0,
            completed_since: None,
            completed_in_today: true,
            archived_before: None,
        }
    }

    /// Where Completed ends and Archive begins (§18).
    pub fn with_archived_before(mut self, cutoff: Option<DateTime<Utc>>) -> Self {
        self.archived_before = cutoff;
        self
    }

    /// Today's completed tasks: shown or not, and from when (§10.2).
    pub fn with_completed(mut self, shown: bool, since: Option<DateTime<Utc>>) -> Self {
        self.completed_in_today = shown;
        self.completed_since = since;
        self
    }

    /// Filter by the search box. A blank box is no filter, so that the caller
    /// does not have to special-case the empty string on every keystroke.
    pub fn with_search(mut self, search: &'q str) -> Self {
        self.search = if search.trim().is_empty() {
            None
        } else {
            Some(search)
        };
        self
    }

    pub fn page(mut self, limit: usize, offset: usize) -> Self {
        self.limit = Some(limit);
        self.offset = offset;
        self
    }

    /// The nth page of the Completed view, newest first.
    pub fn completed_page(today: NaiveDate, page: usize) -> Self {
        Self::new(TaskView::Completed, today).page(COMPLETED_PAGE_SIZE, page * COMPLETED_PAGE_SIZE)
    }
}

/// A search box's contents, split into `tag:` terms and free text.
#[derive(Debug, Default, PartialEq, Eq)]
struct SearchTerms {
    tags: Vec<String>,
    text: Vec<String>,
}

impl SearchTerms {
    fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.text.is_empty()
    }
}

/// Split a search box into terms.
///
/// Quote-aware so that `tag:"next actions"` is one term, and case-insensitive
/// on the `tag:` prefix because the input is typed in a hurry. Everything that
/// is not a `tag:` term is free text, and free-text terms are ANDed: typing more
/// narrows the list, which is the only behaviour that makes a live filter
/// usable.
fn parse_search(input: &str) -> SearchTerms {
    let mut terms = SearchTerms::default();
    let mut token = String::new();
    let mut quoted = false;

    let flush = |token: &mut String, terms: &mut SearchTerms| {
        let value = std::mem::take(token);
        if value.is_empty() {
            return;
        }
        match value
            .char_indices()
            .nth(4)
            .map(|(index, _)| index)
            .or(Some(value.len()))
            .filter(|_| value.len() >= 4)
        {
            Some(split) if value[..split].eq_ignore_ascii_case("tag:") => {
                let name = value[split..].trim();
                if !name.is_empty() {
                    terms.tags.push(name.to_string());
                }
            }
            _ => terms.text.push(value),
        }
    };

    for character in input.chars() {
        match character {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => flush(&mut token, &mut terms),
            c => token.push(c),
        }
    }
    flush(&mut token, &mut terms);
    terms
}

/// Escape a user's search text for `LIKE ... ESCAPE '\'`.
///
/// Without this a `%` typed into the search box matches everything and a `_`
/// matches any character, which reads as a broken filter rather than as a
/// feature nobody asked for.
fn like_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

/// The state of one task before a mutation touched it.
///
/// Undo restores these values as a *new* revision rather than rewinding history
/// (§10.3), so this holds the fields an edit can move and nothing else — not the
/// revision, not `updated_at`, not the log. Rewinding those would mean writing a
/// revision the server has already seen, which is how an undo on one machine
/// silently loses an unrelated edit made on another.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSnapshot {
    pub task_id: i64,
    /// Kept for diagnostics and for a caller that wants to name the task in an
    /// "undone" message; the restore itself goes by row id.
    pub task_uuid: String,
    pub subject: String,
    pub body_text: Option<String>,
    pub postpone_date: Option<String>,
    pub due_date: Option<String>,
    pub tags: Vec<String>,
    pub recurrence: Option<String>,
    pub recurrence_from_completion: bool,
    pub position: f64,
    pub steps: Vec<TaskStep>,
    pub is_working_on: bool,
    pub is_completed: bool,
    pub is_trashed: bool,
}

impl TaskSnapshot {
    /// Capture a task's pre-state.
    ///
    /// The dates are taken through [`Task::dates`], which repairs an
    /// inconsistent stored pair, rather than from the raw columns: a snapshot
    /// that could not be re-applied would be an undo key that does nothing, and
    /// the one moment a user reaches for undo is the moment they least want to
    /// be told the task is unrepresentable.
    pub fn of(task: &Task) -> Option<Self> {
        let dates = task.dates();
        Some(Self {
            task_id: task.id?,
            task_uuid: task.task_uuid.clone(),
            subject: task.subject.clone(),
            body_text: task.body_text.clone(),
            postpone_date: dates.postpone().map(|date| date.to_string()),
            due_date: dates.due().map(|date| date.to_string()),
            tags: task.tags.clone(),
            recurrence: task.recurrence.clone(),
            recurrence_from_completion: task.recurrence_from_completion,
            position: task.position,
            steps: task.steps.clone(),
            is_working_on: task.is_working_on,
            is_completed: task.is_completed,
            is_trashed: task.deleted_at.is_some(),
        })
    }

    /// The edit that restores this state.
    fn as_updates(&self) -> TaskUpdates {
        TaskUpdates {
            subject: Some(self.subject.clone()),
            body_text: Some(self.body_text.clone()),
            postpone_date: Some(self.postpone_date.clone()),
            due_date: Some(self.due_date.clone()),
            tags: Some(self.tags.clone()),
            recurrence: Some(self.recurrence.clone()),
            recurrence_from_completion: Some(self.recurrence_from_completion),
            is_working_on: Some(self.is_working_on),
            is_completed: Some(self.is_completed),
            is_trashed: Some(self.is_trashed),
            position: Some(self.position),
            steps: Some(self.steps.clone()),
        }
    }
}

/// One task's share of an undoable mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum UndoStep {
    /// Put these field values back.
    Restore(Box<TaskSnapshot>),
    /// The task did not exist before the mutation, so undoing it trashes the
    /// task. Deliberately not a hard delete: the task may already have been
    /// appended to the server, and a soft delete syncs as a deletion (§10.1)
    /// while a hard one leaves the message behind to be downloaded again.
    Trash { task_id: i64 },
}

/// Everything needed to undo one user action, including a bulk one.
///
/// A multi-selection completed with one keystroke is one undo step, not twelve
/// (§10.3), so the unit of undo is the action rather than the task.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UndoEntry {
    /// What to call the action in "Undone: …", e.g. `complete 3 tasks`.
    pub label: String,
    pub steps: Vec<UndoStep>,
}

impl UndoEntry {
    /// The undo entry for a freshly created task.
    pub fn for_creation(task: &Task, label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            steps: task
                .id
                .map(|task_id| UndoStep::Trash { task_id })
                .into_iter()
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// What a mutation did.
#[derive(Debug, Clone, Default)]
pub struct MutationOutcome {
    /// The pre-state, ready to be pushed onto an undo stack. Undoing it yields
    /// another `MutationOutcome` whose `undo` is the redo.
    pub undo: UndoEntry,
    /// The tasks that actually changed. An update that moved nothing writes
    /// nothing, bumps no revision and appears here as an empty list.
    pub changed: Vec<i64>,
    /// Recurring tasks whose completion advanced them, with the anchors their
    /// reminders have to be dragged between. Reminders live in their own table,
    /// so moving them is the reminder layer's job — this is the instruction.
    pub reminder_shifts: Vec<(i64, ReminderShift)>,
}

impl MutationOutcome {
    /// Name the action, for the undo stack.
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.undo.label = label.into();
        self
    }

    pub fn is_noop(&self) -> bool {
        self.changed.is_empty()
    }
}

/// A task that has been expunged from the local cache and has to be expunged
/// from the server too.
///
/// The UIDs are the whole point: once the row is gone there is nothing left to
/// tell the sync layer which messages to delete, and a task message left behind
/// on the server comes back on the next full listing as a resurrected task.
#[derive(Debug, Clone)]
pub struct PurgedTask {
    pub id: i64,
    pub task_uuid: String,
    pub imap_uid: Option<u32>,
    pub message_id: Option<String>,
    /// UIDs of the attachment messages belonging to the task (§9): deleting a
    /// task expunges its attachments too, or they linger as orphans.
    pub attachment_uids: Vec<u32>,
}

/// One row of the local index the sync layer reconciles the server listing
/// against (§3.3): which task ids we have, at which revision, in which message.
#[derive(Debug, Clone)]
pub struct TaskIndexRow {
    pub id: i64,
    pub task_uuid: String,
    pub revision: i64,
    pub imap_uid: Option<u32>,
    pub message_id: Option<String>,
    pub is_dirty: bool,
    pub is_trashed: bool,
}

// ---------------------------------------------------------------------------
// The repository
// ---------------------------------------------------------------------------

/// Task database operations.
pub struct TaskRepository<'a> {
    db: &'a Database,
    zone: Box<dyn LocalZone>,
}

impl<'a> TaskRepository<'a> {
    /// A repository that evaluates dates in the machine's own timezone.
    pub fn new(db: &'a Database) -> Self {
        Self {
            db,
            zone: Box::new(SystemZone),
        }
    }

    /// A repository pinned to a given timezone.
    pub fn with_zone(db: &'a Database, zone: impl LocalZone + 'static) -> Self {
        Self {
            db,
            zone: Box::new(zone),
        }
    }

    /// The local civil date right now — what every view means by "today".
    pub fn today(&self) -> NaiveDate {
        self.zone.civil_date(Utc::now())
    }

    /// The local civil date at a given instant.
    pub fn today_at(&self, now: DateTime<Utc>) -> NaiveDate {
        self.zone.civil_date(now)
    }

    /// The local wall clock at a given instant: the date *and* the time of day.
    ///
    /// Derived from the two questions [`LocalZone`] already answers — how far
    /// `now` is past the local midnight of the local day it falls in — rather
    /// than by adding a fixed offset, so it stays right on the 23- and 25-hour
    /// days. The reminder parser needs it (§5.3: `9am` typed at noon means
    /// tomorrow), and it has to be the *same* wall clock the views are evaluated
    /// against or the picker and Today would disagree about what day it is.
    pub fn local_now(&self, now: DateTime<Utc>) -> NaiveDateTime {
        self.zone.local_now(now)
    }

    /// When the date-dependent views next have to be re-evaluated (§4.3).
    ///
    /// Derived from the zone rather than from "now plus 24 hours" because the
    /// day a DST change falls on is 23 or 25 hours long, and an Orb left open
    /// overnight has to show the right Today list in the morning.
    pub fn next_local_midnight(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let today = self.zone.civil_date(now);
        let tomorrow = today.succ_opt().unwrap_or(today);
        self.zone.midnight(tomorrow)
    }

    /// A change context stamped with this machine's device id.
    ///
    /// The id is read from the sync state rather than taken from the caller so
    /// that every log entry this machine appends is attributable without every
    /// call site having to remember to attribute it.
    pub fn context_at<'c>(
        &self,
        now: DateTime<Utc>,
        device_id: Option<&'c str>,
    ) -> ChangeContext<'c> {
        ChangeContext::new(now, self.today_at(now)).with_device_id(device_id)
    }

    /// The instants that bound a local civil day.
    ///
    /// `completed_at` and `deleted_at` are compared with them as text, which
    /// works because every writer goes through [`format_timestamp`], whose
    /// fixed-width RFC3339-in-UTC form sorts chronologically; it is the reason
    /// that function exists.
    fn local_day_bounds(&self, day: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
        let start = self.zone.midnight(day);
        let end = self.zone.midnight(day.succ_opt().unwrap_or(day));
        (start, end)
    }

    /// Run a group of statements as one unit.
    ///
    /// Opens `BEGIN IMMEDIATE` if this is the outermost write, and otherwise
    /// simply joins the transaction the caller already opened. The nesting case
    /// is not hypothetical: the sync layer wraps a whole downloaded-task apply
    /// in one transaction and calls several repository methods inside it, and
    /// SQLite rejects a nested `BEGIN`. Joining is also the *correct* answer —
    /// splitting the inner group into its own transaction would let the caller's
    /// group commit in pieces.
    fn in_transaction<T>(&self, work: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        if self.db.conn.is_autocommit() {
            let tx = self.db.immediate_transaction()?;
            let outcome = work(&tx)?;
            tx.commit()?;
            Ok(outcome)
        } else {
            work(&self.db.conn)
        }
    }

    // ========================================================================
    // Views (§4.1)
    // ========================================================================

    /// List a view, filtered and paginated.
    pub fn list(&self, query: &TaskQuery<'_>) -> Result<Vec<TaskListItem>> {
        let (sql, params) = self.view_sql(query, LIST_COLUMNS, true);
        let mut stmt = self.db.conn.prepare(&sql)?;
        let mut items = stmt
            .query_map(params_from_iter(params.iter()), Self::row_to_list_item)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        self.hydrate_list_items(&self.db.conn, &mut items)?;
        if query.view == TaskView::Today {
            // Open and not dated into Today: only the completion clause of
            // the query can have brought it.
            let today = civil_day_unix(query.today);
            let dated = |unix: Option<i64>| unix.is_some_and(|day| day <= today);
            for item in &mut items {
                item.is_done_for_today = !item.is_completed
                    && item.is_recurring
                    && !dated(item.postpone_date_unix)
                    && !dated(item.due_date_unix);
            }
        }
        Ok(items)
    }

    /// The tasks marked as being worked on (§19): open, not trashed, in the
    /// list's order.
    pub fn working_on(&self) -> Result<Vec<TaskListItem>> {
        let sql = format!(
            "SELECT {LIST_COLUMNS} FROM tasks t
             WHERE t.is_working_on = 1 AND t.deleted_at IS NULL AND t.is_completed = 0
             ORDER BY t.position ASC, t.id ASC"
        );
        let mut stmt = self.db.conn.prepare(&sql)?;
        let mut items = stmt
            .query_map([], Self::row_to_list_item)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        self.hydrate_list_items(&self.db.conn, &mut items)?;
        Ok(items)
    }

    /// Take the working-on mark off every task (§19), and return the tasks
    /// that had it. Local state: no revision, no log entry.
    pub fn clear_working_on(&self) -> Result<Vec<i64>> {
        let ids = self.in_transaction(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM tasks WHERE is_working_on = 1")?;
            let ids = stmt
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            conn.execute(
                "UPDATE tasks SET is_working_on = 0 WHERE is_working_on = 1",
                [],
            )?;
            Ok(ids)
        })?;
        Ok(ids)
    }

    /// Put the working-on mark on these tasks, or take it off: how clearing
    /// it is undone. Local state, like [`TaskRepository::clear_working_on`].
    pub fn set_working_on(&self, task_ids: &[i64], on: bool) -> Result<()> {
        for id in task_ids {
            self.db.conn.execute(
                "UPDATE tasks SET is_working_on = ?1 WHERE id = ?2",
                params![on, id],
            )?;
        }
        Ok(())
    }

    /// How many tasks a view holds, ignoring pagination.
    pub fn count(&self, query: &TaskQuery<'_>) -> Result<usize> {
        let (sql, params) = self.view_sql(query, "COUNT(*)", false);
        let count: i64 = self
            .db
            .conn
            .query_row(&sql, params_from_iter(params.iter()), |row| row.get(0))?;
        Ok(count.max(0) as usize)
    }

    /// The badge on the Today entry: open tasks only (§4.1).
    ///
    /// Tasks completed today are in the Today *view* — they stay visible, struck
    /// through, until the next local midnight — but counting them would mean the
    /// badge stopped falling as work got done, which is the one thing a badge is
    /// for.
    pub fn today_badge_count(&self, today: NaiveDate) -> Result<usize> {
        let count: i64 = self.db.conn.query_row(
            "SELECT COUNT(*) FROM tasks t
             WHERE t.deleted_at IS NULL AND t.is_completed = 0
               AND (t.snooze_until_unix <= ?1 OR t.deadline_unix <= ?1)",
            params![civil_day_unix(today)],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// The Today view: everything actionable now, plus today's completions.
    pub fn today_tasks(&self, today: NaiveDate) -> Result<Vec<TaskListItem>> {
        self.list(&TaskQuery::new(TaskView::Today, today))
    }

    /// The Upcoming view, ordered by the earliest of the two dates.
    pub fn upcoming_tasks(&self, today: NaiveDate) -> Result<Vec<TaskListItem>> {
        self.list(&TaskQuery::new(TaskView::Upcoming, today))
    }

    /// The Someday view: open and undated.
    pub fn someday_tasks(&self, today: NaiveDate) -> Result<Vec<TaskListItem>> {
        self.list(&TaskQuery::new(TaskView::Someday, today))
    }

    /// One page of the Completed view, newest completion first.
    pub fn completed_tasks(&self, today: NaiveDate, page: usize) -> Result<Vec<TaskListItem>> {
        self.list(&TaskQuery::completed_page(today, page))
    }

    /// The Trash view, most recently trashed first.
    pub fn trashed_tasks(&self, today: NaiveDate) -> Result<Vec<TaskListItem>> {
        self.list(&TaskQuery::new(TaskView::Trash, today))
    }

    /// Build a view's SQL and its parameters.
    ///
    /// Every predicate here mirrors [`TaskView::includes`] and the partial
    /// indexes in the schema; the three have to say the same thing, and the
    /// tests in this file are where that is pinned down. Note what is *not*
    /// here: no view but Trash mentions `deleted_at IS NOT NULL`, because a
    /// trashed task must vanish from every other view (§10.1).
    fn view_sql(
        &self,
        query: &TaskQuery<'_>,
        columns: &str,
        ordered: bool,
    ) -> (String, Vec<Value>) {
        let today = civil_day_unix(query.today);
        let mut params: Vec<Value> = Vec::new();
        let mut sql = format!("SELECT {columns} FROM tasks t WHERE ");

        match query.view {
            TaskView::Today => {
                // Overdue needs no clause of its own: a due date in the past is
                // a due date of at most today. Undated tasks match neither
                // date clause and so live in Someday, which is the behaviour
                // change §4.1 spells out — the old query's
                // `snooze_until_unix IS NULL` put every undated task here.
                let (start, end) = self.local_day_bounds(query.today);
                // Completed today, and after the cut-off a clear set; an
                // empty range when completed tasks are not shown (§10.2).
                let start = match query.completed_since {
                    Some(since) if since > start => since,
                    _ => start,
                };
                let start = if query.completed_in_today { start } else { end };
                // A repeating task completed today is open again, dated
                // ahead; its completion is in the log.
                sql.push_str(
                    "t.deleted_at IS NULL AND ( \
                       (t.is_completed = 0 \
                         AND (t.snooze_until_unix <= ? OR t.deadline_unix <= ?)) \
                       OR (t.is_completed = 1 AND t.completed_at >= ? AND t.completed_at < ?) \
                       OR (t.is_completed = 0 AND t.recurrence IS NOT NULL AND EXISTS ( \
                         SELECT 1 FROM task_log l WHERE l.task_id = t.id \
                           AND l.event_type = 'completed' \
                           AND l.occurred_at_unix >= ? AND l.occurred_at_unix < ?)))",
                );
                params.push(Value::Integer(today));
                params.push(Value::Integer(today));
                params.push(Value::Text(format_timestamp(start)));
                params.push(Value::Text(format_timestamp(end)));
                params.push(Value::Integer(start.timestamp()));
                params.push(Value::Integer(end.timestamp()));
            }
            TaskView::Upcoming => {
                sql.push_str(
                    "t.deleted_at IS NULL AND t.is_completed = 0 \
                     AND (t.snooze_until_unix IS NOT NULL OR t.deadline_unix IS NOT NULL)",
                );
            }
            TaskView::Someday => {
                sql.push_str(
                    "t.deleted_at IS NULL AND t.is_completed = 0 \
                     AND t.snooze_until_unix IS NULL AND t.deadline_unix IS NULL",
                );
            }
            TaskView::Completed => {
                sql.push_str("t.deleted_at IS NULL AND t.is_completed = 1");
                if let Some(cutoff) = query.archived_before {
                    sql.push_str(" AND t.completed_at >= ?");
                    params.push(Value::Text(format_timestamp(cutoff)));
                }
            }
            TaskView::Archive => match query.archived_before {
                Some(cutoff) => {
                    sql.push_str(
                        "t.deleted_at IS NULL AND t.is_completed = 1 AND t.completed_at < ?",
                    );
                    params.push(Value::Text(format_timestamp(cutoff)));
                }
                None => sql.push('0'),
            },
            TaskView::Trash => {
                sql.push_str("t.deleted_at IS NOT NULL");
            }
        }

        if let Some(search) = query.search {
            let terms = parse_search(search);
            if !terms.is_empty() {
                Self::push_search(&mut sql, &mut params, &terms);
            }
        }

        if ordered {
            // One global manual order (§4.4): every view is a projection of it,
            // so none of them sorts by anything else — not even by the
            // "working on" flag, which the old Today query floated to the top
            // and which made two views of the same list disagree. Upcoming is
            // the exception the spec itself grants: it is ordered by date,
            // because a plan is read chronologically.
            let order = match query.view {
                TaskView::Upcoming => format!("{EARLIEST_DATE} ASC, t.position ASC, t.id ASC"),
                TaskView::Completed | TaskView::Archive => {
                    "t.completed_at DESC, t.id DESC".to_string()
                }
                TaskView::Trash => "t.deleted_at DESC, t.id DESC".to_string(),
                _ => "t.position ASC, t.id ASC".to_string(),
            };
            sql.push_str(" ORDER BY ");
            sql.push_str(&order);

            if let Some(limit) = query.limit {
                sql.push_str(" LIMIT ? OFFSET ?");
                params.push(Value::Integer(limit as i64));
                params.push(Value::Integer(query.offset as i64));
            }
        }

        (sql, params)
    }

    /// AND the search terms onto a view's predicate (§4.5).
    ///
    /// Deliberately `LIKE` over the three fields rather than the message FTS
    /// index: tasks are hundreds of rows with short titles, the filter has to
    /// react to every keystroke including a two-character prefix, and the FTS
    /// index is a tokenised index of *messages* — feeding tasks into it would
    /// make them turn up in mail search results.
    fn push_search(sql: &mut String, params: &mut Vec<Value>, terms: &SearchTerms) {
        for tag in &terms.tags {
            // Prefix-matched, because the filter runs while the name is still
            // being typed. `LIKE` is case-insensitive for ASCII in SQLite,
            // which is the behaviour a tag filter wants.
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM task_tag_links l \
                   JOIN task_tags g ON g.tag_uuid = l.tag_uuid \
                   WHERE l.task_id = t.id AND g.name LIKE ? ESCAPE '\\')",
            );
            params.push(Value::Text(format!("{}%", like_escape(tag))));
        }

        for text in &terms.text {
            sql.push_str(
                " AND (t.subject LIKE ? ESCAPE '\\' \
                   OR IFNULL(t.body_text, '') LIKE ? ESCAPE '\\' \
                   OR EXISTS (SELECT 1 FROM task_tag_links l \
                     JOIN task_tags g ON g.tag_uuid = l.tag_uuid \
                     WHERE l.task_id = t.id AND g.name LIKE ? ESCAPE '\\'))",
            );
            let pattern = format!("%{}%", like_escape(text));
            params.push(Value::Text(pattern.clone()));
            params.push(Value::Text(pattern.clone()));
            params.push(Value::Text(pattern));
        }
    }

    /// Every task a search query finds, whatever view it is in (§4.5), the
    /// trashed ones aside.
    ///
    /// The query is the mail search's, so one field searches both. Bare words
    /// match title, body and tag names, `s:` the title, `b:` the body and
    /// `tag:` a tag name; an alternative of `OR` that asks for something only
    /// mail has (`f:`, `in:`, `is:unread`, `~` …) finds no task.
    ///
    /// Open tasks come first by the date they come up — the earlier of
    /// postpone and due date, the undated last — then the completed ones,
    /// newest first: the order §26 groups them in.
    pub fn search(&self, query: &crate::search::query::Query) -> Result<Vec<TaskListItem>> {
        let mut params: Vec<Value> = Vec::new();
        let groups: Vec<String> = query
            .groups()
            .filter_map(|group| Self::search_group(group, &mut params))
            .collect();
        if groups.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {LIST_COLUMNS} FROM tasks t
             WHERE t.deleted_at IS NULL AND (({}))
             ORDER BY t.is_completed ASC,
               CASE WHEN t.is_completed = 0 THEN {EARLIEST_DATE} END IS NULL ASC,
               CASE WHEN t.is_completed = 0 THEN {EARLIEST_DATE} END ASC,
               t.completed_at DESC, t.position ASC, t.id ASC",
            groups.join(") OR (")
        );
        let mut stmt = self.db.conn.prepare(&sql)?;
        let mut items = stmt
            .query_map(params_from_iter(params.iter()), Self::row_to_list_item)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        self.hydrate_list_items(&self.db.conn, &mut items)?;
        Ok(items)
    }

    /// One alternative of a search as a condition on tasks, or `None` when it
    /// asks for something no task has.
    fn search_group(
        group: &[crate::search::query::Clause],
        params: &mut Vec<Value>,
    ) -> Option<String> {
        use crate::search::query::{Field, Term};

        const TAGGED: &str = "EXISTS (SELECT 1 FROM task_tag_links l \
             JOIN task_tags g ON g.tag_uuid = l.tag_uuid \
             WHERE l.task_id = t.id AND g.name LIKE ? ESCAPE '\\')";
        let mut conditions = Vec::new();
        for clause in group {
            let (field, value) = match &clause.term {
                Term::Text { field, value } | Term::Phrase { field, value } => (*field, value),
                _ => return None,
            };
            let contains = format!("%{}%", like_escape(value));
            let tag = value
                .get(..4)
                .filter(|prefix| prefix.eq_ignore_ascii_case("tag:"))
                .map(|_| value[4..].trim());
            let condition = match (field, tag) {
                (Field::Any, Some("")) => continue,
                (Field::Any, Some(name)) => {
                    params.push(Value::Text(format!("{}%", like_escape(name))));
                    TAGGED.to_string()
                }
                (Field::Any, None) => {
                    params.extend([Value::Text(contains.clone()), Value::Text(contains.clone())]);
                    params.push(Value::Text(contains));
                    format!(
                        "(t.subject LIKE ? ESCAPE '\\' \
                         OR IFNULL(t.body_text, '') LIKE ? ESCAPE '\\' OR {TAGGED})"
                    )
                }
                (Field::Subject, _) => {
                    params.push(Value::Text(contains));
                    "t.subject LIKE ? ESCAPE '\\'".to_string()
                }
                (Field::Body, _) => {
                    params.push(Value::Text(contains));
                    "IFNULL(t.body_text, '') LIKE ? ESCAPE '\\'".to_string()
                }
                (Field::From | Field::To | Field::Cc, _) => return None,
            };
            conditions.push(if clause.negated {
                format!("NOT {condition}")
            } else {
                condition
            });
        }
        if conditions.is_empty() {
            return None;
        }
        Some(conditions.join(" AND "))
    }

    /// The tasks linked to a message (§6.3), for the message detail view.
    pub fn tasks_for_message(&self, message_id: &str) -> Result<Vec<TaskListItem>> {
        let sql = format!(
            "SELECT {LIST_COLUMNS} FROM tasks t
             JOIN task_linked_emails e ON e.task_id = t.id
             WHERE e.message_id = ?1 AND t.deleted_at IS NULL
             ORDER BY t.position ASC, t.id ASC"
        );
        let mut stmt = self.db.conn.prepare(&sql)?;
        let mut items = stmt
            .query_map(params![message_id], Self::row_to_list_item)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        self.hydrate_list_items(&self.db.conn, &mut items)?;
        Ok(items)
    }

    /// How many open tasks each of these messages has (§6.3).
    ///
    /// One query for the whole visible page rather than one per row: the message
    /// list asks this on every render, and a query per row is what makes a list
    /// stutter. Messages with no tasks are absent from the map rather than
    /// present with a zero — the column renders nothing for them.
    pub fn open_task_counts_for_messages(
        &self,
        message_ids: &[String],
    ) -> Result<HashMap<String, usize>> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; message_ids.len()].join(", ");
        let sql = format!(
            "SELECT e.message_id, COUNT(*) FROM task_linked_emails e
             JOIN tasks t ON t.id = e.task_id
             WHERE e.message_id IN ({placeholders})
               AND t.is_completed = 0 AND t.deleted_at IS NULL
             GROUP BY e.message_id"
        );
        let mut stmt = self.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(message_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    // ========================================================================
    // Reading tasks
    // ========================================================================

    /// A task by row id, with its tags and its whole log.
    ///
    /// The log is loaded rather than left empty because completing a recurring
    /// task counts the completions in it to decide which occurrence it is on
    /// (`COUNT=10` has to survive two devices completing the same series), so a
    /// task loaded without its log would advance a series past its own count.
    pub fn get_task(&self, task_id: i64) -> Result<Option<Task>> {
        Self::load_task(&self.db.conn, task_id)
    }

    /// A task by its stable identity — what reconciliation looks up (§3.1).
    pub fn get_task_by_uuid(&self, task_uuid: &str) -> Result<Option<Task>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks t WHERE t.task_uuid = ?1");
        let task = self
            .db
            .conn
            .query_row(&sql, params![task_uuid], Self::row_to_task)
            .optional()?;
        match task {
            Some(task) => Ok(Some(Self::hydrate_task(&self.db.conn, task)?)),
            None => Ok(None),
        }
    }

    /// A task by the `Message-ID` of the message it currently lives in.
    ///
    /// Useful only for finding the superseded message after a rewrite: the
    /// `Message-ID` changes on every edit and is not identity. Anything looking
    /// for *a task* wants [`TaskRepository::get_task_by_uuid`].
    pub fn get_task_by_message_id(&self, message_id: &str) -> Result<Option<Task>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks t WHERE t.message_id = ?1");
        let task = self
            .db
            .conn
            .query_row(&sql, params![message_id], Self::row_to_task)
            .optional()?;
        match task {
            Some(task) => Ok(Some(Self::hydrate_task(&self.db.conn, task)?)),
            None => Ok(None),
        }
    }

    /// The local index the server listing is reconciled against (§3.3).
    pub fn task_index(&self, account_name: &str) -> Result<Vec<TaskIndexRow>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT id, task_uuid, revision, imap_uid, message_id, is_dirty, deleted_at
             FROM tasks WHERE account_name = ?1 ORDER BY revision ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![account_name], |row| {
            Ok(TaskIndexRow {
                id: row.get(0)?,
                task_uuid: row.get(1)?,
                revision: row.get(2)?,
                imap_uid: row.get(3)?,
                message_id: row.get(4)?,
                is_dirty: row.get(5)?,
                is_trashed: row.get::<_, Option<String>>(6)?.is_some(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Tasks with local changes the server has not confirmed, oldest revision
    /// first — the order the backlog has to reach the server in (§3.6).
    pub fn get_dirty_tasks(&self, account_name: &str) -> Result<Vec<Task>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM tasks t
             WHERE t.is_dirty = 1 AND t.account_name = ?1
             ORDER BY t.revision ASC, t.id ASC"
        );
        let mut stmt = self.db.conn.prepare(&sql)?;
        let tasks = stmt
            .query_map(params![account_name], Self::row_to_task)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        tasks
            .into_iter()
            .map(|task| Self::hydrate_task(&self.db.conn, task))
            .collect()
    }

    /// What this machine has that the server does not, without loading it.
    pub fn upload_backlog(&self, account_name: &str) -> Result<UploadBacklog> {
        let conn = &self.db.conn;
        let (dirty, unlocated): (u32, u32) = conn.query_row(
            "SELECT COALESCE(SUM(is_dirty = 1), 0),
                    COALESCE(SUM(is_dirty = 0 AND imap_uid IS NULL), 0)
             FROM tasks WHERE account_name = ?1",
            params![account_name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let expunges: u32 =
            conn.query_row("SELECT COUNT(*) FROM task_expunges", [], |row| row.get(0))?;
        Ok(UploadBacklog {
            dirty,
            unlocated,
            registry: self.tag_registry_state()?.dirty,
            chat_marks: crate::chat::marks::state(conn)?.dirty,
            today_cleared: self.today_cleared_state()?.dirty,
            expunges,
        })
    }
}

/// The task changes made on this machine that have not reached the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadBacklog {
    /// Tasks edited, or created, here and not uploaded since.
    pub dirty: u32,
    /// Clean tasks whose message is not known, as after a `UIDVALIDITY`
    /// change; only a listing finds them again.
    pub unlocated: u32,
    /// Whether the tag registry has changes the server has not seen.
    pub registry: bool,
    /// Whether Waiting's marks have changes the server has not seen.
    pub chat_marks: bool,
    /// Whether a `clear-completed` here has not reached the server.
    pub today_cleared: bool,
    /// Purged tasks whose messages are still to be expunged.
    pub expunges: u32,
}

impl UploadBacklog {
    /// The number the window shows: every change still to go up.
    pub fn count(&self) -> u32 {
        self.dirty
            + self.unlocated
            + u32::from(self.registry)
            + u32::from(self.chat_marks)
            + u32::from(self.today_cleared)
            + self.expunges
    }

    /// Whether uploading the dirty tasks alone settles it. Everything else
    /// needs the listing a full sync reads.
    pub fn only_dirty_tasks(&self) -> bool {
        self.unlocated == 0
            && !self.registry
            && !self.chat_marks
            && !self.today_cleared
            && self.expunges == 0
    }
}

// ---------------------------------------------------------------------------
// Mutation
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    /// Create a task at the top of the global order, where a task just added
    /// is looked for.
    ///
    /// The identity is minted here, before anything is appended to the server,
    /// which is what makes a retried append idempotent at the task level: a lost
    /// APPEND response produces a duplicate *message* that reconciliation
    /// collapses, never a duplicate *task* (§3.5).
    pub fn create_task(
        &self,
        subject: &str,
        account_name: &str,
        ctx: &ChangeContext<'_>,
    ) -> Result<Task> {
        self.in_transaction(|conn| {
            let position = Self::top_position(conn, account_name)?;
            let change = Task::create(subject.to_string(), account_name.to_string(), position, ctx);
            let mut task = change.task;
            let id = Self::insert_task_row(conn, &task)?;
            task.id = Some(id);
            Self::replace_tag_links(conn, id, &task.tags)?;
            Self::insert_log_entries(conn, id, &change.appended)?;
            Ok(task)
        })
    }

    /// Apply an edit to one task.
    pub fn apply(
        &self,
        task_id: i64,
        updates: &TaskUpdates,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        self.apply_to(&[task_id], updates, ctx)
    }

    /// Apply one edit to a selection of tasks, atomically.
    ///
    /// The single door every local edit goes through. It loads each task with
    /// its log, hands the rules to [`TaskUpdates::apply`] — which checks the
    /// dates, writes the log entries, advances a recurring task
    /// rather than completing it, and bumps the revision only if something
    /// really moved — and writes back what comes out. The whole selection is one
    /// transaction, so a bulk complete over twelve tasks either lands or does
    /// not, and one undo entry covers all of it (§10.3).
    ///
    /// Ids that no longer exist are skipped rather than failing the batch: a
    /// selection can outlive a task that sync deleted underneath it, and taking
    /// the other eleven tasks down with it would be the worse answer.
    pub fn apply_to(
        &self,
        task_ids: &[i64],
        updates: &TaskUpdates,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        self.in_transaction(|conn| {
            let registry = Self::load_tag_registry(conn)?;
            let mut outcome = MutationOutcome::default();
            for &task_id in task_ids {
                let Some(before) = Self::load_task(conn, task_id)? else {
                    continue;
                };
                let change = updates.apply(&before, &registry, ctx)?;
                Self::commit_change(conn, &before, change, &mut outcome)?;
            }
            Ok(outcome)
        })
    }

    /// Write one applied change: the row, its tag links, its new log entries.
    ///
    /// A change that moved nothing writes nothing at all — no revision bump, no
    /// dirty flag, no `updated_at`. A spurious bump is not merely wasted work:
    /// it is a revision this machine would win a conflict with, against a real
    /// edit made on another one.
    fn commit_change(
        conn: &Connection,
        before: &Task,
        change: TaskChange,
        outcome: &mut MutationOutcome,
    ) -> Result<()> {
        let Some(task_id) = before.id else {
            return Ok(());
        };
        // The focus flag is deliberately not a "change" — it neither logs nor
        // bumps the revision (it is local UI state) — but it does have to reach
        // the row, so it is checked separately rather than falling through the
        // no-op gate with everything else.
        let focus_moved = change.task.is_working_on != before.is_working_on;
        if change.is_noop() && !focus_moved {
            return Ok(());
        }

        if let Some(snapshot) = TaskSnapshot::of(before) {
            outcome
                .undo
                .steps
                .push(UndoStep::Restore(Box::new(snapshot)));
        }
        Self::write_task_row(conn, task_id, &change.task)?;
        if change.task.tags != before.tags {
            Self::replace_tag_links(conn, task_id, &change.task.tags)?;
        }
        Self::insert_log_entries(conn, task_id, &change.appended)?;
        outcome.changed.push(task_id);
        if let Some(shift) = change.reminder_shift {
            outcome.reminder_shifts.push((task_id, shift));
        }
        Ok(())
    }

    /// Complete a selection (§10.2), advancing any recurring tasks in it (§8).
    pub fn complete_tasks(
        &self,
        task_ids: &[i64],
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        let updates = TaskUpdates::new().with_is_completed(true);
        Ok(self
            .apply_to(task_ids, &updates, ctx)?
            .labelled(label("complete", task_ids.len())))
    }

    /// Un-complete a selection, from the Completed view or from a task
    /// completed a moment ago.
    pub fn uncomplete_tasks(
        &self,
        task_ids: &[i64],
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        let updates = TaskUpdates::new().with_is_completed(false);
        Ok(self
            .apply_to(task_ids, &updates, ctx)?
            .labelled(label("un-complete", task_ids.len())))
    }

    /// Soft-delete a selection (§10.1): the row and the server message both
    /// stay, so the deletion itself syncs and can be undone.
    pub fn trash_tasks(
        &self,
        task_ids: &[i64],
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        let updates = TaskUpdates::new().with_is_trashed(true);
        Ok(self
            .apply_to(task_ids, &updates, ctx)?
            .labelled(label("delete", task_ids.len())))
    }

    /// Restore a selection from the trash.
    pub fn restore_tasks(
        &self,
        task_ids: &[i64],
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        let updates = TaskUpdates::new().with_is_trashed(false);
        Ok(self
            .apply_to(task_ids, &updates, ctx)?
            .labelled(label("restore", task_ids.len())))
    }

    /// Set (or clear) the postpone date of a selection (§12).
    pub fn set_postpone_date(
        &self,
        task_ids: &[i64],
        date: Option<NaiveDate>,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        let updates = TaskUpdates::new().with_postpone_date(date);
        Ok(self
            .apply_to(task_ids, &updates, ctx)?
            .labelled(label("postpone", task_ids.len())))
    }

    /// Set (or clear) the due date of a selection. Moving it earlier than the
    /// postpone date drags the postpone date along, and both moves are logged.
    pub fn set_due_date(
        &self,
        task_ids: &[i64],
        date: Option<NaiveDate>,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        let updates = TaskUpdates::new().with_due_date(date);
        Ok(self
            .apply_to(task_ids, &updates, ctx)?
            .labelled(label("set due date", task_ids.len())))
    }

    /// Clear both dates, moving a selection into Someday.
    pub fn clear_dates(
        &self,
        task_ids: &[i64],
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        let updates = TaskUpdates::new()
            .with_postpone_date(None)
            .with_due_date(None);
        Ok(self
            .apply_to(task_ids, &updates, ctx)?
            .labelled(label("clear dates", task_ids.len())))
    }

    /// Add a tag to a selection, leaving each task's other tags alone.
    ///
    /// Not expressible as one [`TaskUpdates`] — that carries the complete new
    /// set — so the new set is computed per task inside one transaction.
    pub fn add_tag_to(
        &self,
        task_ids: &[i64],
        tag_uuid: &str,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        self.retag(task_ids, ctx, label("tag", task_ids.len()), |tags| {
            if !tags.iter().any(|tag| tag == tag_uuid) {
                tags.push(tag_uuid.to_string());
            }
        })
    }

    /// Remove a tag from a selection, leaving the tasks themselves alone.
    pub fn remove_tag_from(
        &self,
        task_ids: &[i64],
        tag_uuid: &str,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        self.retag(task_ids, ctx, label("untag", task_ids.len()), |tags| {
            tags.retain(|tag| tag != tag_uuid);
        })
    }

    /// Replace a task's tag set wholesale.
    pub fn set_tags(
        &self,
        task_id: i64,
        tags: Vec<String>,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        let updates = TaskUpdates::new().with_tags(tags);
        Ok(self.apply(task_id, &updates, ctx)?.labelled("change tags"))
    }

    /// Apply a per-task edit of the tag set to a selection, in one transaction.
    fn retag(
        &self,
        task_ids: &[i64],
        ctx: &ChangeContext<'_>,
        label: String,
        edit: impl Fn(&mut Vec<String>),
    ) -> Result<MutationOutcome> {
        self.in_transaction(|conn| {
            let registry = Self::load_tag_registry(conn)?;
            let mut outcome = MutationOutcome::default();
            for &task_id in task_ids {
                let Some(before) = Self::load_task(conn, task_id)? else {
                    continue;
                };
                let mut tags = before.tags.clone();
                edit(&mut tags);
                let change = TaskUpdates::new()
                    .with_tags(tags)
                    .apply(&before, &registry, ctx)?;
                Self::commit_change(conn, &before, change, &mut outcome)?;
            }
            Ok(outcome.labelled(label))
        })
    }

    // ========================================================================
    // Undo (§10.3)
    // ========================================================================

    /// Capture the pre-state of a selection, before mutating it by some route
    /// that does not return a [`MutationOutcome`].
    pub fn capture_undo(&self, task_ids: &[i64], label: impl Into<String>) -> Result<UndoEntry> {
        let mut entry = UndoEntry {
            label: label.into(),
            steps: Vec::new(),
        };
        for &task_id in task_ids {
            if let Some(task) = Self::load_task(&self.db.conn, task_id)?
                && let Some(snapshot) = TaskSnapshot::of(&task)
            {
                entry.steps.push(UndoStep::Restore(Box::new(snapshot)));
            }
        }
        Ok(entry)
    }

    /// Undo a mutation by restoring its pre-state as a **new** revision.
    ///
    /// Not a rewind: the log keeps both the original change and the restoration,
    /// and the revision goes up rather than back. Rewinding would mean
    /// re-publishing a revision the server has already seen, which loses
    /// whatever another machine did in the meantime — an undo that can destroy
    /// an unrelated edit is worse than no undo.
    ///
    /// The returned outcome's `undo` is the redo.
    pub fn undo(&self, entry: &UndoEntry, ctx: &ChangeContext<'_>) -> Result<MutationOutcome> {
        self.in_transaction(|conn| {
            let registry = Self::load_tag_registry(conn)?;
            let mut outcome = MutationOutcome::default();
            for step in &entry.steps {
                let (task_id, updates) = match step {
                    UndoStep::Restore(snapshot) => (snapshot.task_id, snapshot.as_updates()),
                    UndoStep::Trash { task_id } => {
                        (*task_id, TaskUpdates::new().with_is_trashed(true))
                    }
                };
                let Some(before) = Self::load_task(conn, task_id)? else {
                    continue;
                };
                let change = updates.apply(&before, &registry, ctx)?;
                Self::commit_change(conn, &before, change, &mut outcome)?;
            }
            Ok(outcome.labelled(entry.label.clone()))
        })
    }

    // ========================================================================
    // Ordering (§4.4)
    // ========================================================================

    /// The end of the global order.
    fn end_position(conn: &Connection, account_name: &str) -> Result<f64> {
        let highest: Option<f64> = conn.query_row(
            "SELECT MAX(position) FROM tasks WHERE account_name = ?1",
            params![account_name],
            |row| row.get(0),
        )?;
        Ok(highest.unwrap_or(0.0) + POSITION_STEP)
    }

    /// The position a new task goes to: above the first of the global order.
    fn top_position(conn: &Connection, account_name: &str) -> Result<f64> {
        let lowest: Option<f64> = conn.query_row(
            "SELECT MIN(position) FROM tasks WHERE account_name = ?1",
            params![account_name],
            |row| row.get(0),
        )?;
        Ok(lowest.map_or(POSITION_STEP, |lowest| lowest - POSITION_STEP))
    }

    /// The midpoint of two positions — where a task dropped between them goes.
    pub fn calculate_position_between(before: f64, after: f64) -> f64 {
        (before + after) / 2.0
    }

    /// Move a task between two neighbours, identified by row id.
    ///
    /// `before` is the task it lands after and `after` the task it lands before;
    /// `None` for either means the corresponding end of the list. Writes exactly
    /// one row — except on the rare pass where the gap has collapsed, when the
    /// whole list is renormalised first (see
    /// [`TaskRepository::renormalise_positions`]).
    pub fn move_task(
        &self,
        task_id: i64,
        before: Option<i64>,
        after: Option<i64>,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        self.in_transaction(|conn| {
            let Some(task) = Self::load_task(conn, task_id)? else {
                return Ok(MutationOutcome::default());
            };
            let account = task.account_name.clone();
            let position = Self::slot_between(conn, &account, before, after, ctx)?;
            let registry = Self::load_tag_registry(conn)?;
            // Reloaded, because renormalisation inside `slot_between` may have
            // rewritten this very task's row: applying the edit to the stale
            // copy would put the old position (and the old revision) back.
            let Some(task) = Self::load_task(conn, task_id)? else {
                return Ok(MutationOutcome::default());
            };
            let change = TaskUpdates::new()
                .with_position(position)
                .apply(&task, &registry, ctx)?;
            let mut outcome = MutationOutcome::default();
            Self::commit_change(conn, &task, change, &mut outcome)?;
            Ok(outcome.labelled("reorder"))
        })
    }

    /// Move several tasks, in the order given, as one block between two
    /// neighbours — a selection dragged together, whether or not it was
    /// together before. One transaction, one undo step.
    pub fn move_tasks(
        &self,
        task_ids: &[i64],
        before: Option<i64>,
        after: Option<i64>,
        ctx: &ChangeContext<'_>,
    ) -> Result<MutationOutcome> {
        self.in_transaction(|_| {
            let mut outcome = MutationOutcome::default();
            // Each lands directly after the one before it (see
            // `tightest_upper_bound`), which keeps the block together.
            let mut previous = before;
            for &task_id in task_ids {
                let moved = self.move_task(task_id, previous, after, ctx)?;
                outcome.undo.steps.extend(moved.undo.steps);
                outcome.changed.extend(moved.changed);
                previous = Some(task_id);
            }
            Ok(outcome.labelled(label("reorder", task_ids.len())))
        })
    }

    /// The position for a task landing between two neighbours, renormalising
    /// the list first if there is no longer room between them.
    fn slot_between(
        conn: &Connection,
        account_name: &str,
        before: Option<i64>,
        after: Option<i64>,
        ctx: &ChangeContext<'_>,
    ) -> Result<f64> {
        let mut low = Self::position_of(conn, before)?;
        let mut high = Self::tightest_upper_bound(conn, account_name, low, after)?;

        if let (Some(low_value), Some(high_value)) = (low, high) {
            if low_value >= high_value {
                return Err(crate::Error::InvalidInput(format!(
                    "cannot place a task after position {low_value} and before {high_value}"
                )));
            }
            let midpoint = Self::calculate_position_between(low_value, high_value);
            // Both tests matter. The gap threshold catches the case before it
            // becomes visible; the equality catches the case where f64 has
            // already run out despite it, which is the one that silently
            // produces two tasks with the same position.
            if high_value - low_value <= MIN_POSITION_GAP
                || midpoint <= low_value
                || midpoint >= high_value
            {
                Self::renormalise(conn, account_name, ctx)?;
                low = Self::position_of(conn, before)?;
                high = Self::tightest_upper_bound(conn, account_name, low, after)?;
            }
        }

        Ok(match (low, high) {
            (Some(low), Some(high)) => Self::calculate_position_between(low, high),
            // Moving to the top: one step above the first task, so no other row
            // has to be touched.
            (None, Some(high)) => high - POSITION_STEP,
            (Some(low), None) => low + POSITION_STEP,
            (None, None) => Self::end_position(conn, account_name)?,
        })
    }

    /// The position the new task has to stay below.
    ///
    /// Normally that is the `after` neighbour's, but a caller that names two
    /// tasks with something between them (a stale list, a selection dragged past
    /// a filtered-out task) would otherwise get the midpoint of a span that is
    /// already occupied — two tasks with the same position, ordered by the
    /// tiebreak rather than by the drag. Tightening the bound to the first
    /// position above `before` reads the request as "put it directly after this
    /// one", which is what a drag means anyway, and makes the operation total.
    fn tightest_upper_bound(
        conn: &Connection,
        account_name: &str,
        low: Option<f64>,
        after: Option<i64>,
    ) -> Result<Option<f64>> {
        let named = Self::position_of(conn, after)?;
        let Some(low) = low else {
            return Ok(named);
        };
        let next: Option<f64> = conn.query_row(
            "SELECT MIN(position) FROM tasks WHERE account_name = ?1 AND position > ?2",
            params![account_name, low],
            |row| row.get(0),
        )?;
        Ok(match (named, next) {
            (Some(named), Some(next)) => Some(named.min(next)),
            (named, next) => named.or(next),
        })
    }

    fn position_of(conn: &Connection, task_id: Option<i64>) -> Result<Option<f64>> {
        match task_id {
            None => Ok(None),
            Some(task_id) => Ok(conn
                .query_row(
                    "SELECT position FROM tasks WHERE id = ?1",
                    params![task_id],
                    |row| row.get::<_, f64>(0),
                )
                .optional()?),
        }
    }

    /// Rewrite the whole list to multiples of 1024 (§4.4).
    ///
    /// Repeated insertion between the same two tasks halves the gap each time,
    /// so after roughly fifty moves the midpoint is no longer strictly between
    /// its neighbours and the manual order quietly stops being an order. This is
    /// the pass that buys the room back. It is rare and must be correct rather
    /// than clever: it walks the account's tasks in their current order,
    /// including completed and trashed ones (there is one global order and they
    /// are in it), and rewrites only the rows whose position actually changes.
    ///
    /// Each rewritten task is marked dirty with a bumped revision, because the
    /// position is on the wire and a position the server never hears about is a
    /// position the next sync overwrites. No log entry, though: this is a
    /// mechanical repair, not something the user did, and a timeline full of
    /// "Reordered" for a pass nobody asked for is a timeline nobody reads.
    pub fn renormalise_positions(
        &self,
        account_name: &str,
        ctx: &ChangeContext<'_>,
    ) -> Result<usize> {
        self.in_transaction(|conn| Self::renormalise(conn, account_name, ctx))
    }

    fn renormalise(
        conn: &Connection,
        account_name: &str,
        ctx: &ChangeContext<'_>,
    ) -> Result<usize> {
        let mut stmt = conn.prepare(
            "SELECT id, position FROM tasks WHERE account_name = ?1 ORDER BY position ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![account_name], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let now = format_timestamp(ctx.now);
        let mut rewritten = 0usize;
        for (index, (task_id, position)) in rows.into_iter().enumerate() {
            let target = (index as f64 + 1.0) * POSITION_STEP;
            if position == target {
                continue;
            }
            conn.execute(
                "UPDATE tasks
                 SET position = ?1, revision = revision + 1, updated_at = ?2, is_dirty = 1
                 WHERE id = ?3",
                params![target, now, task_id],
            )?;
            rewritten += 1;
        }
        Ok(rewritten)
    }
}

/// "complete", "complete 3 tasks" — the undo stack's label for an action.
fn label(action: &str, count: usize) -> String {
    if count == 1 {
        action.to_string()
    } else {
        format!("{action} {count} tasks")
    }
}

// ---------------------------------------------------------------------------
// Tags (§7)
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    /// The tag registry, in manual order.
    pub fn list_tags(&self) -> Result<Vec<TaskTag>> {
        Self::load_tag_registry(&self.db.conn)
    }

    /// One tag by its identity.
    pub fn get_tag(&self, tag_uuid: &str) -> Result<Option<TaskTag>> {
        Ok(self
            .db
            .conn
            .query_row(
                "SELECT id, tag_uuid, name, colour, position FROM task_tags WHERE tag_uuid = ?1",
                params![tag_uuid],
                Self::row_to_tag,
            )
            .optional()?)
    }

    /// A tag by name, case-insensitively — what `tag:<name>` resolves through.
    pub fn find_tag_by_name(&self, name: &str) -> Result<Option<TaskTag>> {
        Ok(self
            .db
            .conn
            .query_row(
                "SELECT id, tag_uuid, name, colour, position FROM task_tags
                 WHERE name = ?1 COLLATE NOCASE",
                params![name],
                Self::row_to_tag,
            )
            .optional()?)
    }

    /// Add a tag to the registry.
    ///
    /// Bumps the registry's revision, which is what makes the new entry win
    /// against an older copy of the registry on another machine — the registry
    /// syncs by the same identity/revision rules as a task.
    pub fn create_tag(&self, name: &str, colour: Option<&str>) -> Result<TaskTag> {
        self.in_transaction(|conn| {
            let position: Option<f64> =
                conn.query_row("SELECT MAX(position) FROM task_tags", [], |row| row.get(0))?;
            let mut tag = TaskTag::new(name.to_string(), position.unwrap_or(0.0) + POSITION_STEP);
            tag.colour = colour.map(str::to_string);
            let id = Self::insert_tag(conn, &tag)?;
            tag.id = Some(id);
            Self::bump_registry_revision(conn)?;
            Ok(tag)
        })
    }

    /// Rename a tag, or recolour it.
    ///
    /// One row and one message: tasks store tag *ids*, never names (§7), so a
    /// rename does not touch a single task and no task is marked dirty. That is
    /// the entire reason the header carries ids.
    pub fn update_tag(&self, tag_uuid: &str, name: &str, colour: Option<&str>) -> Result<bool> {
        self.in_transaction(|conn| {
            let updated = conn.execute(
                "UPDATE task_tags SET name = ?1, colour = ?2 WHERE tag_uuid = ?3",
                params![name, colour, tag_uuid],
            )?;
            if updated > 0 {
                Self::bump_registry_revision(conn)?;
            }
            Ok(updated > 0)
        })
    }

    /// Delete a tag from the registry and detach it from every task.
    ///
    /// Removing a tag removes it from tasks but never removes the tasks (§7).
    /// Each affected task is edited through the ordinary mutation path, so it
    /// gets a `TagRemoved` log entry, a bumped revision and the dirty flag —
    /// without that flag the tag id would stay in the task's header on the
    /// server, and the next machine to sync would render a tag that no longer
    /// exists.
    ///
    /// Returns what happened to the tasks it was on, whose `undo` puts the tag
    /// back on them. The registry row is deleted *after* the tasks are edited,
    /// so the log entries can still record the name the tag had.
    pub fn delete_tag(&self, tag_uuid: &str, ctx: &ChangeContext<'_>) -> Result<MutationOutcome> {
        self.in_transaction(|conn| {
            let registry = Self::load_tag_registry(conn)?;
            let mut stmt =
                conn.prepare("SELECT task_id FROM task_tag_links WHERE tag_uuid = ?1")?;
            let task_ids = stmt
                .query_map(params![tag_uuid], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);

            let mut outcome = MutationOutcome::default();
            for task_id in &task_ids {
                let Some(before) = Self::load_task(conn, *task_id)? else {
                    continue;
                };
                let mut tags = before.tags.clone();
                tags.retain(|tag| tag != tag_uuid);
                let change = TaskUpdates::new()
                    .with_tags(tags)
                    .apply(&before, &registry, ctx)?;
                Self::commit_change(conn, &before, change, &mut outcome)?;
            }

            conn.execute(
                "DELETE FROM task_tags WHERE tag_uuid = ?1",
                params![tag_uuid],
            )?;
            Self::bump_registry_revision(conn)?;
            Ok(outcome)
        })
    }

    /// Make a tag's registry entry `tag`, or take it out with `None`, and
    /// return what it was: how creating, renaming, recolouring and deleting a
    /// tag are undone. The tasks carrying it are not touched here.
    pub fn restore_tag(&self, tag_uuid: &str, tag: Option<&TaskTag>) -> Result<Option<TaskTag>> {
        let before = self.get_tag(tag_uuid)?;
        self.in_transaction(|conn| {
            match tag {
                Some(tag) => {
                    Self::insert_tag(conn, tag)?;
                }
                None => {
                    conn.execute(
                        "DELETE FROM task_tags WHERE tag_uuid = ?1",
                        params![tag_uuid],
                    )?;
                }
            }
            Self::bump_registry_revision(conn)?;
            Ok(())
        })?;
        Ok(before)
    }

    /// Replace the whole registry with the one from a synced registry message.
    ///
    /// Tags that vanish here are *not* detached from tasks: a task referencing
    /// an unknown tag id renders the id as a placeholder rather than dropping it
    /// (§7), because the registry message may simply not have caught up, and
    /// unresolvable is not deleted. Dropping the link would also lose the tag on
    /// this task's next upload — a sync-order accident turned into data loss.
    pub fn replace_tag_registry(
        &self,
        tags: &[TaskTag],
        revision: i64,
        message_id: Option<&str>,
    ) -> Result<()> {
        self.in_transaction(|conn| {
            conn.execute("DELETE FROM task_tags", [])?;
            for tag in tags {
                Self::insert_tag(conn, tag)?;
            }
            Self::set_registry_revision(conn, revision)?;
            Self::set_meta(conn, TAG_REGISTRY_DIRTY_KEY, &0)?;
            match message_id {
                Some(id) => Self::set_meta(conn, TAG_REGISTRY_MESSAGE_KEY, &id)?,
                None => Self::clear_meta(conn, TAG_REGISTRY_MESSAGE_KEY)?,
            }
            Self::set_registry_base(conn, tags)?;
            Ok(())
        })
    }

    /// Put a merge of this machine's tags and another machine's in place, at
    /// `revision`, still to be uploaded (§7).
    pub fn merge_tag_registry(&self, tags: &[TaskTag], revision: i64) -> Result<()> {
        self.in_transaction(|conn| {
            conn.execute("DELETE FROM task_tags", [])?;
            for tag in tags {
                Self::insert_tag(conn, tag)?;
            }
            Self::set_registry_revision(conn, revision)?;
            Self::set_meta(conn, TAG_REGISTRY_DIRTY_KEY, &1)?;
            Ok(())
        })
    }

    /// The tags as the registry copy this machine last wrote or read had
    /// them; `None` before one was recorded.
    pub fn tag_registry_base(&self) -> Result<Option<Vec<TaskTag>>> {
        let json: Option<String> = self
            .db
            .conn
            .query_row(
                "SELECT value FROM sync_meta WHERE key = ?1",
                params![TAG_REGISTRY_BASE_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(json.and_then(|json| serde_json::from_str(&json).ok()))
    }

    fn set_registry_base(conn: &Connection, tags: &[TaskTag]) -> Result<()> {
        let base: Vec<TaskTag> = tags
            .iter()
            .map(|tag| TaskTag {
                id: None,
                ..tag.clone()
            })
            .collect();
        let json = serde_json::to_string(&base).unwrap_or_else(|_| "[]".into());
        Self::set_meta(conn, TAG_REGISTRY_BASE_KEY, &json)
    }

    /// Record that the registry at `revision` reached the server as
    /// `message_id`. A compare-and-swap like a task's (§3.2): a tag changed
    /// while the upload ran leaves the registry dirty, to go up next time.
    /// Returns whether the registry is now clean.
    pub fn mark_registry_uploaded(
        &self,
        revision: i64,
        message_id: &str,
        uploaded: &[TaskTag],
    ) -> Result<bool> {
        self.in_transaction(|conn| {
            Self::set_meta(conn, TAG_REGISTRY_MESSAGE_KEY, &message_id)?;
            Self::set_registry_base(conn, uploaded)?;
            if Self::registry_revision(conn)? != revision {
                return Ok(false);
            }
            Self::set_meta(conn, TAG_REGISTRY_DIRTY_KEY, &0)?;
            Ok(true)
        })
    }

    /// The registry's revision, whether it has unsent changes, and which copy
    /// it is.
    pub fn tag_registry_state(&self) -> Result<RegistryState> {
        let conn = &self.db.conn;
        let dirty: Option<i64> = conn
            .query_row(
                "SELECT value FROM sync_meta WHERE key = ?1",
                params![TAG_REGISTRY_DIRTY_KEY],
                |row| row.get(0),
            )
            .optional()?;
        let message_id: Option<String> = conn
            .query_row(
                "SELECT value FROM sync_meta WHERE key = ?1",
                params![TAG_REGISTRY_MESSAGE_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(RegistryState {
            revision: Self::registry_revision(conn)?,
            dirty: dirty.unwrap_or(0) != 0,
            message_id,
        })
    }

    fn meta<T: rusqlite::types::FromSql>(conn: &Connection, key: &str) -> Result<Option<T>> {
        Ok(conn
            .query_row(
                "SELECT value FROM sync_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The instant up to which Today's completed tasks are hidden (§10.2),
    /// on whichever machine `clear-completed` was given.
    pub fn today_cleared_at(&self) -> Result<Option<DateTime<Utc>>> {
        let at: Option<String> = Self::meta(&self.db.conn, TODAY_CLEARED_AT_KEY)?;
        Ok(at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc)))
    }

    /// Hide the tasks completed up to `now` from Today, here and, once
    /// uploaded, on every other machine.
    pub fn clear_completed_from_today(&self, now: DateTime<Utc>) -> Result<()> {
        self.in_transaction(|conn| {
            let revision = Self::meta::<i64>(conn, TODAY_CLEARED_REVISION_KEY)?.unwrap_or(0) + 1;
            Self::set_meta(conn, TODAY_CLEARED_AT_KEY, &format_timestamp(now))?;
            Self::set_meta(conn, TODAY_CLEARED_REVISION_KEY, &revision)?;
            Self::set_meta(conn, TODAY_CLEARED_DIRTY_KEY, &1)?;
            Ok(())
        })
    }

    /// The cut-off's revision, whether it is unsent, and which copy it is.
    pub fn today_cleared_state(&self) -> Result<RegistryState> {
        let conn = &self.db.conn;
        Ok(RegistryState {
            revision: Self::meta(conn, TODAY_CLEARED_REVISION_KEY)?.unwrap_or(0),
            dirty: Self::meta::<i64>(conn, TODAY_CLEARED_DIRTY_KEY)?.unwrap_or(0) != 0,
            message_id: Self::meta(conn, TODAY_CLEARED_MESSAGE_KEY)?,
        })
    }

    /// Take in another machine's cut-off. The later of the two holds: a
    /// clearing on either machine hides what was completed before it. A
    /// cut-off of this machine's own still unsent goes up above both.
    pub fn take_in_today_cleared(
        &self,
        remote: Option<DateTime<Utc>>,
        revision: i64,
        message_id: Option<&str>,
    ) -> Result<()> {
        let local = self.today_cleared_at()?;
        self.in_transaction(|conn| {
            let dirty = Self::meta::<i64>(conn, TODAY_CLEARED_DIRTY_KEY)?.unwrap_or(0) != 0;
            if let Some(at) = local.max(remote) {
                Self::set_meta(conn, TODAY_CLEARED_AT_KEY, &format_timestamp(at))?;
            }
            let revision = if dirty {
                Self::meta::<i64>(conn, TODAY_CLEARED_REVISION_KEY)?
                    .unwrap_or(0)
                    .max(revision)
                    + 1
            } else {
                revision
            };
            Self::set_meta(conn, TODAY_CLEARED_REVISION_KEY, &revision)?;
            match message_id {
                Some(id) => Self::set_meta(conn, TODAY_CLEARED_MESSAGE_KEY, &id)?,
                None => Self::clear_meta(conn, TODAY_CLEARED_MESSAGE_KEY)?,
            }
            Ok(())
        })
    }

    /// The cut-off at `revision` reached the server as `message_id`; clean
    /// unless it was cleared again meanwhile (§3.2).
    pub fn mark_today_cleared_uploaded(&self, revision: i64, message_id: &str) -> Result<bool> {
        self.in_transaction(|conn| {
            Self::set_meta(conn, TODAY_CLEARED_MESSAGE_KEY, &message_id)?;
            if Self::meta::<i64>(conn, TODAY_CLEARED_REVISION_KEY)?.unwrap_or(0) != revision {
                return Ok(false);
            }
            Self::set_meta(conn, TODAY_CLEARED_DIRTY_KEY, &0)?;
            Ok(true)
        })
    }

    fn set_meta(conn: &Connection, key: &str, value: &dyn rusqlite::ToSql) -> Result<()> {
        conn.execute(
            "INSERT INTO sync_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn clear_meta(conn: &Connection, key: &str) -> Result<()> {
        conn.execute("DELETE FROM sync_meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// The registry's current revision, for conflict resolution against a
    /// synced registry message.
    pub fn tag_registry_revision(&self) -> Result<i64> {
        Self::registry_revision(&self.db.conn)
    }

    /// The tag ids a task carries, in the order they were linked.
    pub fn tags_of(&self, task_id: i64) -> Result<Vec<String>> {
        Self::load_task_tags(&self.db.conn, task_id)
    }

    fn insert_tag(conn: &Connection, tag: &TaskTag) -> Result<i64> {
        conn.execute(
            "INSERT INTO task_tags (tag_uuid, name, colour, position) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(tag_uuid) DO UPDATE SET
                name = excluded.name, colour = excluded.colour, position = excluded.position",
            params![tag.tag_uuid, tag.name, tag.colour, tag.position],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn registry_revision(conn: &Connection) -> Result<i64> {
        Ok(conn
            .query_row(
                "SELECT value FROM sync_meta WHERE key = ?1",
                params![TAG_REGISTRY_REVISION_KEY],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    fn set_registry_revision(conn: &Connection, revision: i64) -> Result<()> {
        conn.execute(
            "INSERT INTO sync_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![TAG_REGISTRY_REVISION_KEY, revision],
        )?;
        Ok(())
    }

    fn bump_registry_revision(conn: &Connection) -> Result<i64> {
        let next = Self::registry_revision(conn)? + 1;
        Self::set_registry_revision(conn, next)?;
        Self::set_meta(conn, TAG_REGISTRY_DIRTY_KEY, &1)?;
        Ok(next)
    }
}

// ---------------------------------------------------------------------------
// The log (§11)
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    /// A task's timeline, oldest first.
    pub fn get_log(&self, task_id: i64) -> Result<Vec<TaskLogEntry>> {
        Self::load_log(&self.db.conn, task_id)
    }

    /// Append entries to a task's log.
    ///
    /// Idempotent by construction: `entry_uuid` is UNIQUE and the insert ignores
    /// conflicts, so re-applying a log that has already been seen changes
    /// nothing. That is what makes the union merge of §11 safe to run on every
    /// sync. Returns how many entries were genuinely new.
    pub fn append_log(&self, task_id: i64, entries: &[TaskLogEntry]) -> Result<usize> {
        self.in_transaction(|conn| Self::insert_log_entries(conn, task_id, entries))
    }

    /// Merge another device's view of a task's log into ours.
    ///
    /// The log is the one part of a task exempt from last-writer-wins (§11):
    /// two devices that appended different entries did not disagree about
    /// anything, they each observed something that really happened, so the
    /// result is the union keyed by `entry_uuid`, sorted by time. Running it
    /// twice with the same input is a no-op.
    pub fn merge_log(&self, task_id: i64, incoming: &[TaskLogEntry]) -> Result<Vec<TaskLogEntry>> {
        self.in_transaction(|conn| {
            let local = Self::load_log(conn, task_id)?;
            let merged = merge_logs(&local, incoming);
            Self::insert_log_entries(conn, task_id, &merged)?;
            Ok(merged)
        })
    }

    /// Take in another copy's log (§11) without touching the task's fields,
    /// revision or dirty flag: for a task this machine is about to upload over
    /// another machine's copy, so the upload carries both histories. Returns
    /// how many entries were new.
    pub fn merge_remote_log(&self, task_uuid: &str, entries: &[TaskLogEntry]) -> Result<usize> {
        self.in_transaction(|conn| {
            let task_id: Option<i64> = conn
                .query_row(
                    "SELECT id FROM tasks WHERE task_uuid = ?1",
                    params![task_uuid],
                    |row| row.get(0),
                )
                .optional()?;
            match task_id {
                Some(task_id) => Self::insert_log_entries(conn, task_id, entries),
                None => Ok(0),
            }
        })
    }

    fn insert_log_entries(
        conn: &Connection,
        task_id: i64,
        entries: &[TaskLogEntry],
    ) -> Result<usize> {
        let mut inserted = 0usize;
        for entry in entries {
            inserted += conn.execute(
                "INSERT INTO task_log
                    (task_id, entry_uuid, occurred_at, occurred_at_unix, device_id, event_type, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(entry_uuid) DO NOTHING",
                params![
                    task_id,
                    entry.entry_uuid,
                    entry.occurred_at,
                    entry.occurred_at_unix,
                    entry.device_id,
                    entry.event_type(),
                    entry.detail_json(),
                ],
            )?;
        }
        Ok(inserted)
    }

    fn load_log(conn: &Connection, task_id: i64) -> Result<Vec<TaskLogEntry>> {
        let mut stmt = conn.prepare(
            "SELECT id, entry_uuid, occurred_at, occurred_at_unix, device_id, event_type, detail
             FROM task_log WHERE task_id = ?1
             ORDER BY occurred_at_unix ASC, entry_uuid ASC",
        )?;
        let entries = stmt
            .query_map(params![task_id], |row| {
                let event_type: String = row.get(5)?;
                let detail: Option<String> = row.get(6)?;
                Ok(TaskLogEntry {
                    id: row.get(0)?,
                    task_id: Some(task_id),
                    entry_uuid: row.get(1)?,
                    occurred_at: row.get(2)?,
                    occurred_at_unix: row.get(3)?,
                    device_id: row.get(4)?,
                    event: super::model::TaskEvent::from_parts(&event_type, detail.as_deref()),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// Trash and permanent deletion (§10.1)
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    /// Expunge trashed tasks older than 30 days.
    ///
    /// The one path on which a task's log is trimmed, because it is the one path
    /// on which the task itself stops existing. Returns the messages the sync
    /// layer now has to expunge from the server — once the row is gone nothing
    /// else knows they exist, and a task message left behind comes back as a
    /// resurrected task on the next full listing.
    pub fn purge_expired_trash(&self, now: DateTime<Utc>) -> Result<Vec<PurgedTask>> {
        let cutoff = now
            .checked_sub_days(Days::new(TRASH_RETENTION_DAYS as u64))
            .unwrap_or(now);
        self.purge(Some(format_timestamp(cutoff)))
    }

    /// Empty the trash now.
    pub fn empty_trash(&self) -> Result<Vec<PurgedTask>> {
        self.purge(None)
    }

    /// Permanently delete one task, whatever state it is in.
    ///
    /// This is what a server-side hard delete is applied as (§10.1): a message
    /// deleted by something other than Orb is indistinguishable from an
    /// intentional expunge, so it is not softened into a trashing.
    pub fn delete_task(&self, task_id: i64) -> Result<Option<PurgedTask>> {
        self.in_transaction(|conn| {
            let Some(purged) = Self::purge_descriptor(conn, task_id)? else {
                return Ok(None);
            };
            // Reminders, links, attachments, tag links and log entries all
            // cascade from the row; that cascade is the reason the foreign keys
            // exist.
            conn.execute("DELETE FROM tasks WHERE id = ?1", params![task_id])?;
            Ok(Some(purged))
        })
    }

    /// Permanently delete one task and record that its messages must go.
    ///
    /// The discard path: a task abandoned before it was ever named, or one the
    /// caller means to erase rather than trash. Unlike [`Self::delete_task`],
    /// which applies a deletion the server has *already* performed, this leaves
    /// a tombstone — the messages are still out there.
    pub fn discard_task(&self, task_id: i64) -> Result<Option<PurgedTask>> {
        self.in_transaction(|conn| {
            let Some(purged) = Self::purge_descriptor(conn, task_id)? else {
                return Ok(None);
            };
            Self::record_expunge(conn, &purged)?;
            conn.execute("DELETE FROM tasks WHERE id = ?1", params![task_id])?;
            Ok(Some(purged))
        })
    }

    /// Tasks purged locally whose messages the server may still hold.
    pub fn pending_expunges(&self) -> Result<Vec<PurgedTask>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT task_uuid, imap_uid, message_id, attachment_uids FROM task_expunges",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let uids: Option<String> = row.get(3)?;
                Ok(PurgedTask {
                    // The row it named is gone, so there is no id to give back;
                    // nothing downstream of a tombstone looks at one.
                    id: 0,
                    task_uuid: row.get(0)?,
                    imap_uid: row.get(1)?,
                    message_id: row.get(2)?,
                    attachment_uids: uids
                        .unwrap_or_default()
                        .split(',')
                        .filter_map(|uid| uid.parse().ok())
                        .collect(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Forget a tombstone, once the server has stopped listing the task.
    pub fn forget_expunges(&self, task_uuids: &[String]) -> Result<usize> {
        if task_uuids.is_empty() {
            return Ok(0);
        }
        self.in_transaction(|conn| {
            let mut removed = 0;
            for uuid in task_uuids {
                removed += conn.execute(
                    "DELETE FROM task_expunges WHERE task_uuid = ?1",
                    params![uuid],
                )?;
            }
            Ok(removed)
        })
    }

    /// Note that a purged task's messages are still on the server.
    ///
    /// Written in the same transaction as the delete, so there is no instant in
    /// which the row is gone and the record of it is not.
    fn record_expunge(conn: &Connection, purged: &PurgedTask) -> Result<()> {
        let attachment_uids = purged
            .attachment_uids
            .iter()
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        conn.execute(
            "INSERT OR REPLACE INTO task_expunges
                (task_uuid, imap_uid, message_id, attachment_uids, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                purged.task_uuid,
                purged.imap_uid,
                purged.message_id,
                attachment_uids,
                format_timestamp(Utc::now()),
            ],
        )?;
        Ok(())
    }

    fn purge(&self, cutoff: Option<String>) -> Result<Vec<PurgedTask>> {
        self.in_transaction(|conn| {
            // `deleted_at` is compared as text, which is chronological because
            // every writer formats it through `format_timestamp`; a cutoff of
            // `None` is "everything in the trash", i.e. empty-trash.
            // "9999" sorts after every timestamp this century, so the sentinel
            // needs no second query.
            let cutoff = cutoff.as_deref().unwrap_or("9999");
            let mut stmt = conn
                .prepare("SELECT id FROM tasks WHERE deleted_at IS NOT NULL AND deleted_at < ?1")?;
            let ids = stmt
                .query_map(params![cutoff], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);

            let mut purged = Vec::with_capacity(ids.len());
            for task_id in ids {
                if let Some(descriptor) = Self::purge_descriptor(conn, task_id)? {
                    // The tombstone goes in first and in the same transaction.
                    // A purge whose expunge never happens — the app quits, the
                    // connection drops, the server refuses — must still be
                    // remembered, or the next full listing reads these task ids
                    // as tasks from another machine and downloads the trash
                    // back.
                    Self::record_expunge(conn, &descriptor)?;
                    conn.execute("DELETE FROM tasks WHERE id = ?1", params![task_id])?;
                    purged.push(descriptor);
                }
            }
            Ok(purged)
        })
    }

    /// Everything the sync layer will need about a task once its row is gone.
    fn purge_descriptor(conn: &Connection, task_id: i64) -> Result<Option<PurgedTask>> {
        let row = conn
            .query_row(
                "SELECT task_uuid, imap_uid, message_id FROM tasks WHERE id = ?1",
                params![task_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<u32>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((task_uuid, imap_uid, message_id)) = row else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "SELECT imap_uid FROM task_attachments WHERE task_id = ?1 AND imap_uid IS NOT NULL",
        )?;
        let attachment_uids = stmt
            .query_map(params![task_id], |row| row.get::<_, u32>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Some(PurgedTask {
            id: task_id,
            task_uuid,
            imap_uid,
            message_id,
            attachment_uids,
        }))
    }
}

// ---------------------------------------------------------------------------
// Reminders (§5)
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    /// Add a reminder to a task.
    ///
    /// Bumps the task's revision and marks it dirty for the same reason
    /// [`TaskRepository::link_email`] does: reminders live in their own table
    /// but travel *inside* the task message (`X-Orb-Task-Reminders`). A reminder
    /// added without the bump stays on this machine forever, and — worse — the
    /// next download of that task, whose wire form does not mention it, deletes
    /// it. §14 requirement 2 is "fires on every machine", so the reminder has to
    /// be published like any other field of the task.
    ///
    /// `is_triggered` is the deliberate exception and is never a reason to
    /// publish anything (§5.2): it is per-machine state, so
    /// [`TaskRepository::mark_reminder_triggered`] bumps nothing.
    pub fn insert_reminder(&self, reminder: &TaskReminder, ctx: &ChangeContext<'_>) -> Result<i64> {
        self.in_transaction(|conn| {
            let id = Self::insert_reminder_row(conn, reminder)?;
            Self::touch_task(conn, reminder.task_id, ctx)?;
            Ok(id)
        })
    }

    /// Add a reminder from an RFC3339 timestamp.
    pub fn add_reminder(
        &self,
        task_id: i64,
        remind_at: &str,
        ctx: &ChangeContext<'_>,
    ) -> Result<i64> {
        let parsed = DateTime::parse_from_rfc3339(remind_at)
            .map_err(|_| crate::Error::InvalidInput(format!("invalid datetime: {remind_at}")))?;
        let reminder = TaskReminder::new(task_id, remind_at.to_string(), parsed.timestamp());
        self.insert_reminder(&reminder, ctx)
    }

    /// Write one reminder row and nothing else.
    ///
    /// The download path's primitive, and the building block the publishing
    /// wrappers above are built from. Kept private because a caller that writes
    /// a reminder row without deciding whether the task now needs re-uploading
    /// has almost certainly forgotten to decide.
    fn insert_reminder_row(conn: &Connection, reminder: &TaskReminder) -> Result<i64> {
        conn.execute(
            "INSERT INTO task_reminders (task_id, remind_at, remind_at_unix, is_triggered)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                reminder.task_id,
                reminder.remind_at,
                reminder.remind_at_unix,
                reminder.is_triggered,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// A task's reminders, soonest first.
    pub fn get_reminders(&self, task_id: i64) -> Result<Vec<TaskReminder>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT id, task_id, remind_at, remind_at_unix, is_triggered
             FROM task_reminders WHERE task_id = ?1 ORDER BY remind_at_unix ASC",
        )?;
        let rows = stmt.query_map(params![task_id], Self::row_to_reminder)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One reminder by row id.
    pub fn get_reminder(&self, reminder_id: i64) -> Result<Option<TaskReminder>> {
        Ok(self
            .db
            .conn
            .query_row(
                "SELECT id, task_id, remind_at, remind_at_unix, is_triggered
                 FROM task_reminders WHERE id = ?1",
                params![reminder_id],
                Self::row_to_reminder,
            )
            .optional()?)
    }

    /// Reminders that have come due and not yet fired on this machine.
    pub fn get_pending_reminders(&self, now_unix: i64) -> Result<Vec<(TaskReminder, String)>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT r.id, r.task_id, r.remind_at, r.remind_at_unix, r.is_triggered, t.subject
             FROM task_reminders r JOIN tasks t ON t.id = r.task_id
             WHERE r.is_triggered = 0 AND r.remind_at_unix <= ?1
               AND t.is_completed = 0 AND t.deleted_at IS NULL
             ORDER BY r.remind_at_unix ASC",
        )?;
        let rows = stmt.query_map(params![now_unix], |row| {
            Ok((Self::row_to_reminder(row)?, row.get::<_, String>(5)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Alias of [`TaskRepository::get_pending_reminders`], for callers that
    /// read better with "due".
    pub fn get_due_reminders(&self, now_unix: i64) -> Result<Vec<(TaskReminder, String)>> {
        self.get_pending_reminders(now_unix)
    }

    /// The next reminders that should hold an OS notification slot (§5.1).
    ///
    /// macOS keeps at most 64 pending local notifications, so the window is
    /// filled with the nearest ones and re-filled on every sync, launch and
    /// local midnight. The task's uuid comes along because the notification's
    /// identifier is derived from it — a row id would not survive the cache
    /// being deleted and resynced, and reconciliation has to be idempotent
    /// across exactly that.
    pub fn upcoming_reminders(
        &self,
        from_unix: i64,
        limit: usize,
    ) -> Result<Vec<(TaskReminder, String, String)>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT r.id, r.task_id, r.remind_at, r.remind_at_unix, r.is_triggered,
                    t.task_uuid, t.subject
             FROM task_reminders r JOIN tasks t ON t.id = r.task_id
             WHERE r.remind_at_unix >= ?1 AND t.is_completed = 0 AND t.deleted_at IS NULL
             ORDER BY r.remind_at_unix ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![from_unix, limit as i64], |row| {
            Ok((
                Self::row_to_reminder(row)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Untriggered reminders inside a time range, for an upcoming-reminders
    /// display.
    pub fn get_reminders_in_range(
        &self,
        from_unix: i64,
        until_unix: i64,
    ) -> Result<Vec<(TaskReminder, String)>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT r.id, r.task_id, r.remind_at, r.remind_at_unix, r.is_triggered, t.subject
             FROM task_reminders r JOIN tasks t ON t.id = r.task_id
             WHERE r.is_triggered = 0 AND r.remind_at_unix > ?1 AND r.remind_at_unix <= ?2
               AND t.is_completed = 0 AND t.deleted_at IS NULL
             ORDER BY r.remind_at_unix ASC",
        )?;
        let rows = stmt.query_map(params![from_unix, until_unix], |row| {
            Ok((Self::row_to_reminder(row)?, row.get::<_, String>(5)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record that this machine has fired a reminder.
    ///
    /// Local per machine and deliberately not on the wire (§5.2): every machine
    /// is supposed to notify its own user, so this bumps no revision and marks
    /// nothing dirty.
    pub fn mark_reminder_triggered(&self, reminder_id: i64) -> Result<()> {
        self.db.conn.execute(
            "UPDATE task_reminders SET is_triggered = 1 WHERE id = ?1",
            params![reminder_id],
        )?;
        Ok(())
    }

    /// Swap a task's whole reminder set for a new one, in one transaction and
    /// one revision bump.
    ///
    /// The entry point for a rewrite that is not an add or a remove — a
    /// recurrence advance moving every reminder forward
    /// ([`ReminderService::follow_recurrence`](super::reminders::ReminderService::follow_recurrence)).
    /// Doing it as N deletes and N inserts would work, but it would bump the
    /// revision 2N times for one user action and make the task's history read
    /// like a dozen edits; more importantly it would leave the set momentarily
    /// short, and a sync that read it there would publish the gap.
    ///
    /// Row ids are not preserved, and nothing depends on them: a notification's
    /// identity comes from the task's uuid and the reminder's instant precisely
    /// so that it survives the cache being rebuilt (§5.1).
    pub fn replace_reminders(
        &self,
        task_id: i64,
        reminders: &[TaskReminder],
        ctx: &ChangeContext<'_>,
    ) -> Result<usize> {
        self.in_transaction(|conn| {
            conn.execute(
                "DELETE FROM task_reminders WHERE task_id = ?1",
                params![task_id],
            )?;
            for reminder in reminders {
                Self::insert_reminder_row(
                    conn,
                    &TaskReminder {
                        task_id,
                        ..reminder.clone()
                    },
                )?;
            }
            Self::touch_task(conn, task_id, ctx)?;
            Ok(reminders.len())
        })
    }

    /// Remove one reminder, publishing the smaller set.
    ///
    /// Returns whether a row was actually removed, so a double delete does not
    /// bump a revision for nothing.
    pub fn delete_reminder(&self, reminder_id: i64, ctx: &ChangeContext<'_>) -> Result<bool> {
        self.in_transaction(|conn| {
            let task_id: Option<i64> = conn
                .query_row(
                    "SELECT task_id FROM task_reminders WHERE id = ?1",
                    params![reminder_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(task_id) = task_id else {
                return Ok(false);
            };
            conn.execute(
                "DELETE FROM task_reminders WHERE id = ?1",
                params![reminder_id],
            )?;
            Self::touch_task(conn, task_id, ctx)?;
            Ok(true)
        })
    }

    /// The task a reminder belongs to.
    pub fn reminder_task(&self, reminder_id: i64) -> Result<Option<i64>> {
        Ok(self
            .db
            .conn
            .query_row(
                "SELECT task_id FROM task_reminders WHERE id = ?1",
                params![reminder_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Drop every reminder of a task.
    pub fn delete_reminders(&self, task_id: i64, ctx: &ChangeContext<'_>) -> Result<usize> {
        self.in_transaction(|conn| {
            let removed = conn.execute(
                "DELETE FROM task_reminders WHERE task_id = ?1",
                params![task_id],
            )?;
            if removed > 0 {
                Self::touch_task(conn, task_id, ctx)?;
            }
            Ok(removed)
        })
    }

    /// Discard fired reminders older than a cutoff.
    ///
    /// Deliberately does **not** touch the tasks, and deliberately deletes
    /// nothing that has not already fired on this machine. The row is part of
    /// the task's wire form, so publishing this pruning would delete the
    /// reminder on machines that never fired it; leaving the task clean means
    /// the pruning stays local until some real edit rewrites the message, which
    /// is the lesser of the two evils. A reminder that has fired has done its
    /// job, and a recurring task's reminders are moved forward by
    /// [`ReminderService::follow_recurrence`](super::reminders::ReminderService::follow_recurrence)
    /// rather than left behind for this to collect.
    pub fn delete_old_triggered_reminders(&self, cutoff_unix: i64) -> Result<usize> {
        Ok(self.db.conn.execute(
            "DELETE FROM task_reminders WHERE is_triggered = 1 AND remind_at_unix < ?1",
            params![cutoff_unix],
        )?)
    }

    fn row_to_reminder(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskReminder> {
        Ok(TaskReminder {
            id: row.get(0)?,
            task_id: row.get(1)?,
            remind_at: row.get(2)?,
            remind_at_unix: row.get(3)?,
            is_triggered: row.get(4)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Linked messages (§6)
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    /// Link a message to a task, with the cached metadata that keeps the link
    /// readable after the message itself is gone.
    ///
    /// Bumps the task's revision and logs the link, because the links travel in
    /// the task message: a link that never reaches the server is a link the next
    /// sync deletes.
    pub fn link_email(
        &self,
        task_id: i64,
        message_id: &str,
        metadata: &CachedEmailMetadata,
        ctx: &ChangeContext<'_>,
    ) -> Result<i64> {
        self.in_transaction(|conn| {
            let link = TaskLinkedEmail::new(task_id, message_id.to_string(), metadata.clone());
            let existing = conn
                .query_row(
                    "SELECT id, cached_subject, cached_from, cached_date, cached_account,
                            cached_folder
                     FROM task_linked_emails WHERE task_id = ?1 AND message_id = ?2",
                    params![task_id, message_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            CachedEmailMetadata {
                                subject: row.get(1)?,
                                from: row.get(2)?,
                                date: row.get(3)?,
                                account: row.get(4)?,
                                folder: row.get(5)?,
                            },
                        ))
                    },
                )
                .optional()?;
            // Linking a message that is linked already adds no link, no log
            // entry and no revision; only a change in what is remembered about
            // it — the folder it moved to — is a change to the task.
            if let Some((id, cached)) = existing {
                if cached != *metadata {
                    Self::insert_linked_email(conn, &link)?;
                    Self::touch_task(conn, task_id, ctx)?;
                }
                return Ok(id);
            }
            let id = Self::insert_linked_email(conn, &link)?;
            Self::insert_log_entries(
                conn,
                task_id,
                &[TaskLogEntry::new(
                    super::model::TaskEvent::LinkAdded {
                        message_id: message_id.to_string(),
                        subject: metadata.subject.clone(),
                    },
                    ctx,
                )],
            )?;
            Self::touch_task(conn, task_id, ctx)?;
            Ok(id)
        })
    }

    /// Unlink a message from a task.
    pub fn unlink_email(
        &self,
        task_id: i64,
        message_id: &str,
        ctx: &ChangeContext<'_>,
    ) -> Result<bool> {
        self.in_transaction(|conn| {
            let removed = conn.execute(
                "DELETE FROM task_linked_emails WHERE task_id = ?1 AND message_id = ?2",
                params![task_id, message_id],
            )?;
            if removed == 0 {
                return Ok(false);
            }
            Self::insert_log_entries(
                conn,
                task_id,
                &[TaskLogEntry::new(
                    super::model::TaskEvent::LinkRemoved {
                        message_id: message_id.to_string(),
                    },
                    ctx,
                )],
            )?;
            Self::touch_task(conn, task_id, ctx)?;
            Ok(true)
        })
    }

    /// A task's linked messages, newest message first.
    pub fn get_linked_emails(&self, task_id: i64) -> Result<Vec<TaskLinkedEmail>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT id, task_id, message_id, cached_subject, cached_from, cached_date,
                    cached_account, cached_folder
             FROM task_linked_emails WHERE task_id = ?1
             ORDER BY cached_date DESC, id ASC",
        )?;
        let rows = stmt.query_map(params![task_id], |row| {
            Ok(TaskLinkedEmail {
                id: row.get(0)?,
                task_id: row.get(1)?,
                message_id: row.get(2)?,
                cached_subject: row.get(3)?,
                cached_from: row.get(4)?,
                cached_date: row.get(5)?,
                cached_account: row.get(6)?,
                cached_folder: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Refresh the cached metadata of a link — in particular the folder the
    /// message was last seen in, which the resolution order of §6.2 tries first.
    ///
    /// Local bookkeeping about somebody else's message: no revision bump, and
    /// the task stays clean.
    pub fn update_linked_email_cache(
        &self,
        message_id: &str,
        metadata: &CachedEmailMetadata,
    ) -> Result<()> {
        self.db.conn.execute(
            "UPDATE task_linked_emails SET
                cached_subject = ?1, cached_from = ?2, cached_date = ?3,
                cached_account = ?4, cached_folder = ?5
             WHERE message_id = ?6",
            params![
                metadata.subject,
                metadata.from,
                metadata.date,
                metadata.account,
                metadata.folder,
                message_id,
            ],
        )?;
        Ok(())
    }

    /// Drop every link of a task, without touching the task.
    pub fn delete_linked_emails(&self, task_id: i64) -> Result<()> {
        self.db.conn.execute(
            "DELETE FROM task_linked_emails WHERE task_id = ?1",
            params![task_id],
        )?;
        Ok(())
    }

    fn insert_linked_email(conn: &Connection, link: &TaskLinkedEmail) -> Result<i64> {
        conn.execute(
            "INSERT INTO task_linked_emails
                (task_id, message_id, cached_subject, cached_from, cached_date,
                 cached_account, cached_folder)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(task_id, message_id) DO UPDATE SET
                cached_subject = excluded.cached_subject,
                cached_from = excluded.cached_from,
                cached_date = excluded.cached_date,
                cached_account = excluded.cached_account,
                cached_folder = excluded.cached_folder",
            params![
                link.task_id,
                link.message_id,
                link.cached_subject,
                link.cached_from,
                link.cached_date,
                link.cached_account,
                link.cached_folder,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }
}

// ---------------------------------------------------------------------------
// Attachments (§9)
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    /// Record an attachment on a task.
    ///
    /// The task message lists its attachments' ids, so adding one changes the
    /// task message and therefore bumps the revision — even though the payload
    /// travels in a message of its own precisely so that editing a title does
    /// not re-upload it.
    pub fn add_attachment(
        &self,
        attachment: &TaskAttachment,
        ctx: &ChangeContext<'_>,
    ) -> Result<i64> {
        self.in_transaction(|conn| {
            let id = Self::insert_attachment(conn, attachment)?;
            Self::touch_task(conn, attachment.task_id, ctx)?;
            Ok(id)
        })
    }

    /// A task's attachments.
    pub fn get_attachments(&self, task_id: i64) -> Result<Vec<TaskAttachment>> {
        let mut stmt = self.db.conn.prepare(
            "SELECT id, task_id, attachment_uuid, filename, content_type, size, file_path,
                    imap_uid, message_id, is_cached
             FROM task_attachments WHERE task_id = ?1 ORDER BY filename ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![task_id], Self::row_to_attachment)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Remove an attachment from a task.
    pub fn delete_attachment(&self, attachment_id: i64, ctx: &ChangeContext<'_>) -> Result<()> {
        self.in_transaction(|conn| {
            let task_id: Option<i64> = conn
                .query_row(
                    "SELECT task_id FROM task_attachments WHERE id = ?1",
                    params![attachment_id],
                    |row| row.get(0),
                )
                .optional()?;
            conn.execute(
                "DELETE FROM task_attachments WHERE id = ?1",
                params![attachment_id],
            )?;
            if let Some(task_id) = task_id {
                Self::touch_task(conn, task_id, ctx)?;
            }
            Ok(())
        })
    }

    /// The task an attachment belongs to, with all of that task's
    /// attachments.
    pub fn attachment_task(
        &self,
        attachment_id: i64,
    ) -> Result<Option<(i64, Vec<TaskAttachment>)>> {
        let task_id: Option<i64> = self
            .db
            .conn
            .query_row(
                "SELECT task_id FROM task_attachments WHERE id = ?1",
                params![attachment_id],
                |row| row.get(0),
            )
            .optional()?;
        match task_id {
            Some(task_id) => Ok(Some((task_id, self.get_attachments(task_id)?))),
            None => Ok(None),
        }
    }

    /// Give a task exactly `attachments`, and return the ones it had: how
    /// adding and removing an attachment are undone. An attachment put back
    /// keeps its identity, so its payload — still on the server, since its
    /// task is — is found again rather than uploaded twice.
    pub fn replace_attachments(
        &self,
        task_id: i64,
        attachments: &[TaskAttachment],
        ctx: &ChangeContext<'_>,
    ) -> Result<Vec<TaskAttachment>> {
        let before = self.get_attachments(task_id)?;
        self.in_transaction(|conn| {
            conn.execute(
                "DELETE FROM task_attachments WHERE task_id = ?1",
                params![task_id],
            )?;
            for attachment in attachments {
                Self::insert_attachment(conn, attachment)?;
            }
            Self::touch_task(conn, task_id, ctx)?;
            Ok(())
        })?;
        Ok(before)
    }

    /// Note where an attachment's payload now lives on the server, or that it
    /// has been downloaded into the local cache.
    ///
    /// Neither fact is on the wire — the cache is disposable and rebuildable
    /// (§9) — so this deliberately does not bump the revision.
    pub fn update_attachment_storage(
        &self,
        attachment_uuid: &str,
        imap_uid: Option<u32>,
        message_id: Option<&str>,
        is_cached: bool,
    ) -> Result<()> {
        self.db.conn.execute(
            "UPDATE task_attachments
             SET imap_uid = COALESCE(?1, imap_uid),
                 message_id = COALESCE(?2, message_id),
                 is_cached = ?3
             WHERE attachment_uuid = ?4",
            params![imap_uid, message_id, is_cached, attachment_uuid],
        )?;
        Ok(())
    }

    fn insert_attachment(conn: &Connection, attachment: &TaskAttachment) -> Result<i64> {
        conn.execute(
            "INSERT INTO task_attachments
                (task_id, attachment_uuid, filename, content_type, size, file_path,
                 imap_uid, message_id, is_cached)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(attachment_uuid) DO UPDATE SET
                filename = excluded.filename,
                content_type = excluded.content_type,
                size = excluded.size,
                imap_uid = COALESCE(excluded.imap_uid, task_attachments.imap_uid),
                message_id = COALESCE(excluded.message_id, task_attachments.message_id)",
            params![
                attachment.task_id,
                attachment.attachment_uuid,
                attachment.filename,
                attachment.content_type,
                attachment.size,
                attachment.file_path,
                attachment.imap_uid,
                attachment.message_id,
                attachment.is_cached,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn row_to_attachment(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskAttachment> {
        Ok(TaskAttachment {
            id: row.get(0)?,
            task_id: row.get(1)?,
            attachment_uuid: row.get(2)?,
            filename: row.get(3)?,
            content_type: row.get(4)?,
            size: row.get(5)?,
            file_path: row.get(6)?,
            imap_uid: row.get(7)?,
            message_id: row.get(8)?,
            is_cached: row.get(9)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Cache writes and sync state (§3)
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    /// Insert a task exactly as given: row, tag links and log.
    ///
    /// A cache write, not a mutation — it stores the revision it is handed
    /// rather than bumping one, because the caller is the sync layer applying a
    /// revision the server already has. Local edits go through
    /// [`TaskRepository::apply_to`].
    pub fn insert_task(&self, task: &Task) -> Result<i64> {
        self.in_transaction(|conn| {
            let id = Self::insert_task_row(conn, task)?;
            Self::replace_tag_links(conn, id, &task.tags)?;
            Self::insert_log_entries(conn, id, &task.log)?;
            Ok(id)
        })
    }

    /// Overwrite a task's row, tag links and log with the given state.
    ///
    /// The other cache write. Same warning as above: it writes the revision it
    /// is given.
    pub fn overwrite_task(&self, task: &Task) -> Result<()> {
        let Some(task_id) = task.id else {
            return Err(crate::Error::InvalidInput(
                "cannot overwrite a task without an id".to_string(),
            ));
        };
        self.in_transaction(|conn| {
            Self::write_task_row(conn, task_id, task)?;
            Self::replace_tag_links(conn, task_id, &task.tags)?;
            Self::insert_log_entries(conn, task_id, &task.log)?;
            Ok(())
        })
    }

    /// Apply a task downloaded from the server, with everything that hangs off
    /// it, as one atomic unit (§3.5).
    ///
    /// The caller decides *whether* the server copy wins — that is the revision
    /// rule of §3.1 and it does not belong in a storage method. What belongs
    /// here is what "applying" means, and three parts of it are easy to get
    /// wrong:
    ///
    /// - the log is **merged**, not replaced (§11), so entries this machine
    ///   appended and has not uploaded yet survive the download;
    /// - `is_triggered` is preserved per reminder, because it is local to this
    ///   machine (§5.2) and rebuilding the reminders from the wire would make
    ///   every reminder fire again;
    /// - a cached attachment payload is preserved, matched by attachment id, so
    ///   a task edit elsewhere does not throw away a 10 MB download here.
    ///
    /// Attachments that the incoming task no longer lists are dropped; that is
    /// the deletion of an attachment arriving from another machine.
    pub fn store_remote_task(
        &self,
        incoming: &Task,
        reminders: &[TaskReminder],
        links: &[TaskLinkedEmail],
        attachments: &[TaskAttachment],
    ) -> Result<i64> {
        self.in_transaction(|conn| {
            let existing = Self::load_task_by_uuid(conn, &incoming.task_uuid)?;

            let mut task = incoming.clone();
            task.is_dirty = false;

            let task_id = match &existing {
                Some(existing) => {
                    let task_id = existing.id.unwrap_or_default();
                    task.id = Some(task_id);
                    // Local focus state (§2): it never travels, so an incoming
                    // copy has nothing to say about it.
                    task.is_working_on = existing.is_working_on;
                    let merged = merge_logs(&existing.log, &incoming.log);
                    Self::write_task_row(conn, task_id, &task)?;
                    Self::insert_log_entries(conn, task_id, &merged)?;
                    task_id
                }
                None => {
                    let task_id = Self::insert_task_row(conn, &task)?;
                    Self::insert_log_entries(conn, task_id, &task.log)?;
                    task_id
                }
            };

            Self::replace_tag_links(conn, task_id, &task.tags)?;

            // Reminders: rebuilt from the wire, but the fired flags are this
            // machine's own and are matched back by instant.
            let already_fired: HashSet<i64> = self
                .get_reminders(task_id)?
                .into_iter()
                .filter(|reminder| reminder.is_triggered)
                .map(|reminder| reminder.remind_at_unix)
                .collect();
            conn.execute(
                "DELETE FROM task_reminders WHERE task_id = ?1",
                params![task_id],
            )?;
            for reminder in reminders {
                conn.execute(
                    "INSERT INTO task_reminders (task_id, remind_at, remind_at_unix, is_triggered)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        task_id,
                        reminder.remind_at,
                        reminder.remind_at_unix,
                        already_fired.contains(&reminder.remind_at_unix),
                    ],
                )?;
            }

            conn.execute(
                "DELETE FROM task_linked_emails WHERE task_id = ?1",
                params![task_id],
            )?;
            for link in links {
                let mut link = link.clone();
                link.task_id = task_id;
                Self::insert_linked_email(conn, &link)?;
            }

            let listed: HashSet<&str> = attachments
                .iter()
                .map(|attachment| attachment.attachment_uuid.as_str())
                .collect();
            for existing in self.get_attachments(task_id)? {
                if !listed.contains(existing.attachment_uuid.as_str()) {
                    conn.execute(
                        "DELETE FROM task_attachments WHERE id = ?1",
                        params![existing.id],
                    )?;
                }
            }
            for attachment in attachments {
                // The upsert keeps `file_path` and `is_cached` on conflict: the
                // local payload is still where it was, and the incoming
                // descriptor knows nothing about this machine's cache.
                let mut attachment = attachment.clone();
                attachment.task_id = task_id;
                Self::insert_attachment(conn, &attachment)?;
            }

            Ok(task_id)
        })
    }

    /// Write a task read from an export (todo-spec §21) as `store_remote_task`
    /// writes a downloaded one — the log merged, this machine's fired flags
    /// and cached payloads kept — then above both copies' revisions and dirty,
    /// so the next sync uploads it and it wins on every machine.
    pub fn import_task(
        &self,
        task: &Task,
        reminders: &[TaskReminder],
        links: &[TaskLinkedEmail],
        attachments: &[TaskAttachment],
    ) -> Result<i64> {
        self.in_transaction(|conn| {
            let local = Self::load_task_by_uuid(conn, &task.task_uuid)?.map(|t| t.revision);
            let task_id = self.store_remote_task(task, reminders, links, attachments)?;
            let revision = local.unwrap_or(0).max(task.revision) + 1;
            conn.execute(
                "UPDATE tasks SET revision = ?1, is_dirty = 1 WHERE id = ?2",
                params![revision, task_id],
            )?;
            Ok(task_id)
        })
    }

    /// Add the tags of an export this machine does not have, by id; a tag it
    /// has keeps its name here. Returns how many were added; any addition is
    /// a registry change, to be uploaded (§7).
    pub fn import_tags(&self, tags: &[TaskTag]) -> Result<usize> {
        self.in_transaction(|conn| {
            let mut added = 0;
            for tag in tags {
                let known: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM task_tags WHERE tag_uuid = ?1",
                        params![tag.tag_uuid],
                        |row| row.get(0),
                    )
                    .optional()?;
                if known.is_none() {
                    Self::insert_tag(conn, tag)?;
                    added += 1;
                }
            }
            if added > 0 {
                Self::bump_registry_revision(conn)?;
            }
            Ok(added)
        })
    }

    /// Whether a purge of this task is still remembered (§10.1).
    pub fn is_tombstoned(&self, task_uuid: &str) -> Result<bool> {
        Ok(self
            .db
            .conn
            .query_row(
                "SELECT 1 FROM task_expunges WHERE task_uuid = ?1",
                params![task_uuid],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Record a successful upload, clearing the dirty flag only if the task is
    /// still at the revision that was uploaded (§3.2).
    ///
    /// The compare-and-swap is the whole point. Clearing the flag
    /// unconditionally marks an edit made *while the upload was in flight* as
    /// clean, and that edit is then never uploaded — it survives locally until
    /// the next download overwrites it, at which point it is gone. The UID and
    /// `Message-ID` are recorded either way: that message is the newest one on
    /// the server, and the next upload needs to know which one to expunge.
    ///
    /// Returns whether the task came out clean.
    pub fn mark_uploaded(
        &self,
        task_id: i64,
        uploaded_revision: i64,
        uid: u32,
        message_id: &str,
    ) -> Result<bool> {
        self.in_transaction(|conn| {
            let current: Option<i64> = conn
                .query_row(
                    "SELECT revision FROM tasks WHERE id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(current) = current else {
                return Ok(false);
            };
            let clean = current == uploaded_revision;
            conn.execute(
                "UPDATE tasks SET imap_uid = ?1, message_id = ?2, is_dirty = ?3 WHERE id = ?4",
                params![uid, message_id, !clean, task_id],
            )?;
            Ok(clean)
        })
    }

    /// Forget every cached UID for an account after a `UIDVALIDITY` bump (§3.4).
    ///
    /// Nothing is deleted: a `UIDVALIDITY` change means "your UIDs are
    /// meaningless", not "the tasks are gone". The `Message-ID`s are kept too —
    /// they are still valid, and reconciliation runs by task id anyway.
    pub fn clear_uids_for_account(&self, account_name: &str) -> Result<usize> {
        Ok(self.db.conn.execute(
            "UPDATE tasks SET imap_uid = NULL, modseq = NULL WHERE account_name = ?1",
            params![account_name],
        )?)
    }

    /// Sync state for an account.
    pub fn get_sync_state(&self, account_name: &str) -> Result<Option<TaskSyncState>> {
        Ok(self
            .db
            .conn
            .query_row(
                "SELECT id, account_name, folder_name, uidvalidity, highest_modseq, last_sync_time,
                        last_listing_at, last_listing_at_unix, last_listing_count, device_id
                 FROM task_sync_state WHERE account_name = ?1",
                params![account_name],
                |row| {
                    Ok(TaskSyncState {
                        id: row.get(0)?,
                        account_name: row.get(1)?,
                        folder_name: row.get(2)?,
                        uidvalidity: row.get(3)?,
                        highest_modseq: row
                            .get::<_, Option<i64>>(4)?
                            .map(|modseq| modseq.max(0) as u64),
                        last_sync_time: row.get(5)?,
                        last_listing_at: row.get(6)?,
                        last_listing_at_unix: row.get(7)?,
                        last_listing_count: row.get(8)?,
                        device_id: row.get(9)?,
                    })
                },
            )
            .optional()?)
    }

    /// Create or update an account's sync state.
    ///
    /// `highest_modseq` is clamped to SQLite's integer range on the way in.
    /// Mod-sequences are 63-bit by RFC 7162 so a real server cannot overflow it,
    /// but rusqlite refuses a `u64` above `i64::MAX` at *runtime*, and the cost
    /// of that refusal is out of all proportion to the field: the whole state
    /// write fails, `sync()` treats a state failure as fatal, and task sync
    /// stops for good — over a number that is written for diagnostics and never
    /// read back to decide anything (§3.3 forbids relying on it).
    pub fn upsert_sync_state(&self, state: &TaskSyncState) -> Result<()> {
        let highest_modseq = state
            .highest_modseq
            .map(|modseq| i64::try_from(modseq).unwrap_or(i64::MAX));
        self.db.conn.execute(
            "INSERT INTO task_sync_state
                (account_name, folder_name, uidvalidity, highest_modseq, last_sync_time,
                 last_listing_at, last_listing_at_unix, last_listing_count, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(account_name) DO UPDATE SET
                folder_name = excluded.folder_name,
                uidvalidity = excluded.uidvalidity,
                highest_modseq = excluded.highest_modseq,
                last_sync_time = excluded.last_sync_time,
                last_listing_at = excluded.last_listing_at,
                last_listing_at_unix = excluded.last_listing_at_unix,
                last_listing_count = excluded.last_listing_count,
                device_id = COALESCE(excluded.device_id, task_sync_state.device_id)",
            params![
                state.account_name,
                state.folder_name,
                state.uidvalidity,
                highest_modseq,
                state.last_sync_time,
                state.last_listing_at,
                state.last_listing_at_unix,
                state.last_listing_count,
                state.device_id,
            ],
        )?;
        Ok(())
    }

    /// Record that the folder has just been listed in full (§3.3).
    pub fn record_listing(&self, account_name: &str, count: i64, at: DateTime<Utc>) -> Result<()> {
        self.db.conn.execute(
            "UPDATE task_sync_state
             SET last_listing_at = ?1, last_listing_at_unix = ?2, last_listing_count = ?3
             WHERE account_name = ?4",
            params![format_timestamp(at), at.timestamp(), count, account_name],
        )?;
        Ok(())
    }

    /// This machine's device id, generated on first use.
    ///
    /// Generated rather than configured, and stored in the database rather than
    /// in the config file, so that copying a config to a second machine does not
    /// give two machines the same identity — which would make the log's
    /// "which machine did this" column a lie exactly when it matters.
    pub fn ensure_device_id(&self, account_name: &str) -> Result<String> {
        self.in_transaction(|conn| {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT device_id FROM task_sync_state WHERE account_name = ?1",
                    params![account_name],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            if let Some(device_id) = existing {
                return Ok(device_id);
            }
            let device_id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO task_sync_state (account_name, folder_name, device_id)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(account_name) DO UPDATE SET device_id = excluded.device_id",
                params![account_name, TaskSyncState::DEFAULT_FOLDER, device_id],
            )?;
            Ok(device_id)
        })
    }
}

// ---------------------------------------------------------------------------
// Row mapping and other private plumbing
// ---------------------------------------------------------------------------

impl TaskRepository<'_> {
    fn load_task(conn: &Connection, task_id: i64) -> Result<Option<Task>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks t WHERE t.id = ?1");
        let task = conn
            .query_row(&sql, params![task_id], Self::row_to_task)
            .optional()?;
        match task {
            Some(task) => Ok(Some(Self::hydrate_task(conn, task)?)),
            None => Ok(None),
        }
    }

    fn load_task_by_uuid(conn: &Connection, task_uuid: &str) -> Result<Option<Task>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks t WHERE t.task_uuid = ?1");
        let task = conn
            .query_row(&sql, params![task_uuid], Self::row_to_task)
            .optional()?;
        match task {
            Some(task) => Ok(Some(Self::hydrate_task(conn, task)?)),
            None => Ok(None),
        }
    }

    /// Attach the two things that live in other tables and that the mutation
    /// path cannot work without: the tag set and the log.
    fn hydrate_task(conn: &Connection, mut task: Task) -> Result<Task> {
        if let Some(task_id) = task.id {
            task.tags = Self::load_task_tags(conn, task_id)?;
            task.log = Self::load_log(conn, task_id)?;
        }
        Ok(task)
    }

    fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
        Ok(Task {
            id: row.get(0)?,
            task_uuid: row.get(1)?,
            revision: row.get(2)?,
            subject: row.get(3)?,
            body_text: row.get(4)?,
            postpone_date: row.get(5)?,
            postpone_date_unix: row.get(6)?,
            due_date: row.get(7)?,
            due_date_unix: row.get(8)?,
            tags: Vec::new(),
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
            completed_at: row.get(11)?,
            deleted_at: row.get(12)?,
            recurrence: row.get(13)?,
            recurrence_from_completion: row.get(14)?,
            position: row.get(15)?,
            is_working_on: row.get(16)?,
            is_completed: row.get(17)?,
            is_dirty: row.get(18)?,
            account_name: row.get(19)?,
            imap_uid: row.get(20)?,
            message_id: row.get(21)?,
            modseq: row.get(22)?,
            steps: row
                .get::<_, Option<String>>(23)?
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
            log: Vec::new(),
        })
    }

    fn row_to_list_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskListItem> {
        Ok(TaskListItem {
            id: row.get(0)?,
            task_uuid: row.get(1)?,
            subject: row.get(2)?,
            postpone_date: row.get(3)?,
            postpone_date_unix: row.get(4)?,
            due_date: row.get(5)?,
            due_date_unix: row.get(6)?,
            completed_at: row.get(7)?,
            deleted_at: row.get(8)?,
            tags: Vec::new(),
            is_recurring: row.get(9)?,
            position: row.get(10)?,
            is_working_on: row.get(11)?,
            is_completed: row.get(12)?,
            is_dirty: row.get(13)?,
            account_name: row.get(14)?,
            imap_uid: row.get(15)?,
            steps_total: row.get(16)?,
            steps_done: row.get(17)?,
            linked_email_count: 0,
            reminder_count: 0,
            attachment_count: 0,
            is_done_for_today: false,
        })
    }

    fn row_to_tag(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskTag> {
        Ok(TaskTag {
            id: row.get(0)?,
            tag_uuid: row.get(1)?,
            name: row.get(2)?,
            colour: row.get(3)?,
            position: row.get(4)?,
        })
    }

    /// Fill in the counts and tag sets a list needs.
    ///
    /// Four grouped queries for the whole page rather than four per row: a list
    /// view renders on every keystroke of the search box, and a query per row is
    /// what turns that into a stutter.
    fn hydrate_list_items(&self, conn: &Connection, items: &mut [TaskListItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let ids: Vec<Value> = items.iter().map(|item| Value::Integer(item.id)).collect();
        let placeholders = vec!["?"; ids.len()].join(", ");

        let counts = |table: &str, column: &str| -> Result<HashMap<i64, usize>> {
            let sql = format!(
                "SELECT {column}, COUNT(*) FROM {table} WHERE {column} IN ({placeholders}) \
                 GROUP BY {column}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(ids.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as usize))
            })?;
            Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
        };

        let links = counts("task_linked_emails", "task_id")?;
        let reminders = counts("task_reminders", "task_id")?;
        let attachments = counts("task_attachments", "task_id")?;

        let sql = format!(
            "SELECT task_id, tag_uuid FROM task_tag_links WHERE task_id IN ({placeholders}) \
             ORDER BY id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut tags: HashMap<i64, Vec<String>> = HashMap::new();
        let rows = stmt.query_map(params_from_iter(ids.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (task_id, tag_uuid) = row?;
            tags.entry(task_id).or_default().push(tag_uuid);
        }

        for item in items.iter_mut() {
            item.linked_email_count = links.get(&item.id).copied().unwrap_or(0);
            item.reminder_count = reminders.get(&item.id).copied().unwrap_or(0);
            item.attachment_count = attachments.get(&item.id).copied().unwrap_or(0);
            item.tags = tags.remove(&item.id).unwrap_or_default();
        }
        Ok(())
    }

    fn load_tag_registry(conn: &Connection) -> Result<Vec<TaskTag>> {
        let mut stmt = conn.prepare(
            "SELECT id, tag_uuid, name, colour, position FROM task_tags
             ORDER BY position ASC, id ASC",
        )?;
        let rows = stmt.query_map([], Self::row_to_tag)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn load_task_tags(conn: &Connection, task_id: i64) -> Result<Vec<String>> {
        let mut stmt =
            conn.prepare("SELECT tag_uuid FROM task_tag_links WHERE task_id = ?1 ORDER BY id ASC")?;
        let rows = stmt.query_map(params![task_id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace a task's tag links.
    ///
    /// Unknown tag ids are stored like any other (§7): `task_tag_links` has no
    /// foreign key into the registry precisely so a task can arrive referencing
    /// a tag whose registry entry has not synced yet, and render it as a
    /// placeholder rather than lose it.
    fn replace_tag_links(conn: &Connection, task_id: i64, tags: &[String]) -> Result<()> {
        conn.execute(
            "DELETE FROM task_tag_links WHERE task_id = ?1",
            params![task_id],
        )?;
        for tag_uuid in tags {
            conn.execute(
                "INSERT INTO task_tag_links (task_id, tag_uuid) VALUES (?1, ?2)
                 ON CONFLICT(task_id, tag_uuid) DO NOTHING",
                params![task_id, tag_uuid],
            )?;
        }
        Ok(())
    }

    fn insert_task_row(conn: &Connection, task: &Task) -> Result<i64> {
        conn.execute(
            "INSERT INTO tasks
                (task_uuid, revision, subject, body_text, snooze_until, snooze_until_unix,
                 deadline, deadline_unix, created_at, updated_at, completed_at, deleted_at,
                 recurrence, recurrence_from_completion, position, is_working_on, is_completed,
                 is_dirty, account_name, imap_uid, message_id, modseq, steps)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     ?18, ?19, ?20, ?21, ?22, ?23)",
            params![
                task.task_uuid,
                task.revision,
                task.subject,
                task.body_text,
                task.postpone_date,
                task.postpone_date_unix,
                task.due_date,
                task.due_date_unix,
                task.created_at,
                task.updated_at,
                task.completed_at,
                task.deleted_at,
                task.recurrence,
                task.recurrence_from_completion,
                task.position,
                task.is_working_on,
                task.is_completed,
                task.is_dirty,
                task.account_name,
                task.imap_uid,
                task.message_id,
                task.modseq,
                steps_json(&task.steps),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Write every mutable column of a task's row.
    ///
    /// `task_uuid` is not in the list: identity does not change, and an UPDATE
    /// that could change it would be a way to merge two tasks by accident.
    fn write_task_row(conn: &Connection, task_id: i64, task: &Task) -> Result<()> {
        conn.execute(
            "UPDATE tasks SET
                revision = ?1, subject = ?2, body_text = ?3,
                snooze_until = ?4, snooze_until_unix = ?5, deadline = ?6, deadline_unix = ?7,
                created_at = ?8, updated_at = ?9, completed_at = ?10, deleted_at = ?11,
                recurrence = ?12, recurrence_from_completion = ?13, position = ?14,
                is_working_on = ?15, is_completed = ?16, is_dirty = ?17,
                account_name = ?18, imap_uid = ?19, message_id = ?20, modseq = ?21,
                steps = ?23
             WHERE id = ?22",
            params![
                task.revision,
                task.subject,
                task.body_text,
                task.postpone_date,
                task.postpone_date_unix,
                task.due_date,
                task.due_date_unix,
                task.created_at,
                task.updated_at,
                task.completed_at,
                task.deleted_at,
                task.recurrence,
                task.recurrence_from_completion,
                task.position,
                task.is_working_on,
                task.is_completed,
                task.is_dirty,
                task.account_name,
                task.imap_uid,
                task.message_id,
                task.modseq,
                task_id,
                steps_json(&task.steps),
            ],
        )?;
        Ok(())
    }

    /// Bump the revision of a task changed through something other than
    /// [`TaskUpdates`] — a link, an attachment.
    ///
    /// Those live in their own tables but travel *inside* the task message, so
    /// skipping the bump would leave the task clean with an unpublished change,
    /// and the next download would quietly delete it.
    fn touch_task(conn: &Connection, task_id: i64, ctx: &ChangeContext<'_>) -> Result<()> {
        conn.execute(
            "UPDATE tasks SET revision = revision + 1, updated_at = ?1, is_dirty = 1 WHERE id = ?2",
            params![format_timestamp(ctx.now), task_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{TaskEvent, tag_label};
    use super::*;
    use crate::db::Database;
    use chrono::{Datelike, Duration, Weekday};

    const ACCOUNT: &str = "Work";

    fn database() -> Database {
        let db = Database::new(":memory:").expect("open database");
        db.init_schema().expect("init schema");
        db
    }

    /// Every test that is not specifically about DST runs two hours east of
    /// UTC, which is enough to make a UTC-based comparison give the wrong
    /// answer for two hours of every day — the bug §4.3 describes.
    fn repo(db: &Database) -> TaskRepository<'_> {
        TaskRepository::with_zone(db, FixedZone::east_hours(2).expect("offset"))
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("test instant")
            .with_timezone(&Utc)
    }

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("test date")
    }

    fn ctx_at(now: &str, today: &str) -> ChangeContext<'static> {
        ChangeContext::new(at(now), day(today)).with_device_id(Some("device-a"))
    }

    /// The reference "now" for tests that do not care what time it is.
    fn ctx() -> ChangeContext<'static> {
        ctx_at("2026-08-06T10:00:00+00:00", "2026-08-06")
    }

    fn create(repo: &TaskRepository<'_>, subject: &str) -> Task {
        repo.create_task(subject, ACCOUNT, &ctx())
            .expect("create task")
    }

    fn create_dated(
        repo: &TaskRepository<'_>,
        subject: &str,
        postpone: Option<&str>,
        due: Option<&str>,
    ) -> i64 {
        let task = create(repo, subject);
        let id = task.id.expect("id");
        let mut updates = TaskUpdates::new();
        if let Some(postpone) = postpone {
            updates = updates.with_postpone_date(Some(day(postpone)));
        }
        if let Some(due) = due {
            updates = updates.with_due_date(Some(day(due)));
        }
        repo.apply(id, &updates, &ctx()).expect("set dates");
        id
    }

    fn subjects(items: &[TaskListItem]) -> Vec<&str> {
        items.iter().map(|item| item.subject.as_str()).collect()
    }

    fn reload(repo: &TaskRepository<'_>, task_id: i64) -> Task {
        repo.get_task(task_id).expect("get task").expect("exists")
    }

    /// Mark a task as uploaded so that a later assertion about the dirty flag
    /// is about the mutation under test rather than about its creation.
    fn mark_clean(repo: &TaskRepository<'_>, task_id: i64, uid: u32) {
        let revision = reload(repo, task_id).revision;
        assert!(
            repo.mark_uploaded(task_id, revision, uid, &format!("<{uid}@orb>"))
                .expect("mark uploaded")
        );
    }

    /// Central European time with the real EU transition rules: forward on the
    /// last Sunday of March at 01:00 UTC, back on the last Sunday of October.
    ///
    /// A fixed offset cannot show whether the date logic is DST-safe, because
    /// under a fixed offset every day is the same length. This can.
    struct CentralEurope;

    impl CentralEurope {
        fn offset_at(instant: DateTime<Utc>) -> FixedOffset {
            let year = instant.year();
            let summer =
                instant >= Self::transition(year, 3) && instant < Self::transition(year, 10);
            FixedOffset::east_opt(if summer { 7200 } else { 3600 }).expect("offset")
        }

        fn transition(year: i32, month: u32) -> DateTime<Utc> {
            let mut date = NaiveDate::from_ymd_opt(year, month, 31).expect("month end");
            while date.weekday() != Weekday::Sun {
                date = date.pred_opt().expect("previous day");
            }
            Utc.from_utc_datetime(&date.and_hms_opt(1, 0, 0).expect("01:00"))
        }
    }

    impl LocalZone for CentralEurope {
        fn civil_date(&self, at: DateTime<Utc>) -> NaiveDate {
            at.with_timezone(&Self::offset_at(at)).date_naive()
        }

        fn midnight(&self, day: NaiveDate) -> DateTime<Utc> {
            // Local midnight always exists here: both transitions happen at
            // 02:00/03:00 local.
            let local = Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight"));
            local - Duration::seconds(Self::offset_at(local).local_minus_utc() as i64)
        }
    }

    // --- local civil dates (§4.3) ----------------------------------------

    #[test]
    fn the_upload_backlog_counts_what_the_server_does_not_have() {
        let db = database();
        let repo = repo(&db);
        let first = create(&repo, "Written here").id.expect("id");
        let second = create(&repo, "Uploaded").id.expect("id");
        mark_clean(&repo, second, 7);
        let backlog = repo.upload_backlog(ACCOUNT).expect("backlog");
        assert_eq!((backlog.dirty, backlog.unlocated), (1, 0));
        assert!(backlog.only_dirty_tasks());

        // A UIDVALIDITY change forgets where the uploaded one lives; only a
        // listing finds it again.
        mark_clean(&repo, first, 8);
        repo.clear_uids_for_account(ACCOUNT).expect("clear uids");
        let backlog = repo.upload_backlog(ACCOUNT).expect("backlog");
        assert_eq!((backlog.dirty, backlog.unlocated), (0, 2));
        assert_eq!(backlog.count(), 2);
        assert!(!backlog.only_dirty_tasks());
    }

    #[test]
    fn the_local_date_rolls_over_at_local_midnight_not_at_utc_midnight() {
        let db = database();
        let repo = repo(&db);
        // 00:30 local on the 7th is still the 6th in UTC. Comparing against
        // `Utc::now()` — what the old code did — makes Today change over at
        // 01:00 local, which is the failure this asserts against.
        assert_eq!(repo.today_at(at("2026-08-06T21:59:00Z")), day("2026-08-06"));
        assert_eq!(repo.today_at(at("2026-08-06T22:30:00Z")), day("2026-08-07"));
    }

    #[test]
    fn a_completed_task_stays_in_today_until_the_next_local_midnight() {
        let db = database();
        let repo = repo(&db);
        let late = create(&repo, "Filed the tax return");
        let id = late.id.expect("id");

        // 23:30 local on the 6th — still the 6th, though UTC says otherwise.
        repo.complete_tasks(&[id], &ctx_at("2026-08-06T21:30:00+00:00", "2026-08-06"))
            .expect("complete");

        assert_eq!(
            subjects(&repo.today_tasks(day("2026-08-06")).expect("today")),
            vec!["Filed the tax return"],
        );
        assert!(
            repo.today_tasks(day("2026-08-07"))
                .expect("today")
                .is_empty(),
            "the completion must drop out at the next local midnight",
        );
    }

    /// `clear-completed` hides what was completed before it, and only from
    /// Today; the setting hides every completion from Today (§10.2).
    /// A task completed before the cut-off is in the Archive and not in
    /// Completed; without a cut-off the Archive is empty (§18).
    #[test]
    fn old_completions_move_from_completed_to_the_archive() {
        let db = database();
        let repo = repo(&db);
        let old = create(&repo, "Filed last year's taxes").id.expect("id");
        let recent = create(&repo, "Paid the rent").id.expect("id");
        repo.complete_tasks(&[old], &ctx_at("2026-06-01T10:00:00+00:00", "2026-06-01"))
            .expect("complete");
        repo.complete_tasks(
            &[recent],
            &ctx_at("2026-08-05T10:00:00+00:00", "2026-08-05"),
        )
        .expect("complete");
        let titles = |view: TaskView, cutoff: Option<&str>| -> Vec<String> {
            let query =
                TaskQuery::new(view, day("2026-08-06")).with_archived_before(cutoff.map(at));
            subjects(&repo.list(&query).expect("list"))
                .into_iter()
                .map(str::to_string)
                .collect()
        };
        let cutoff = Some("2026-07-07T10:00:00Z");
        assert_eq!(titles(TaskView::Completed, cutoff), ["Paid the rent"]);
        assert_eq!(
            titles(TaskView::Archive, cutoff),
            ["Filed last year's taxes"]
        );
        assert_eq!(titles(TaskView::Completed, None).len(), 2);
        assert!(titles(TaskView::Archive, None).is_empty());

        // Un-completing brings it back like any other task.
        repo.uncomplete_tasks(&[old], &ctx()).expect("uncomplete");
        assert!(titles(TaskView::Archive, cutoff).is_empty());
    }

    #[test]
    fn todays_completed_tasks_can_be_cleared_or_left_out() {
        let db = database();
        let repo = repo(&db);
        let early = create(&repo, "Paid the invoice").id.expect("id");
        let late = create(&repo, "Booked the room").id.expect("id");
        repo.complete_tasks(&[early], &ctx_at("2026-08-06T08:00:00+00:00", "2026-08-06"))
            .expect("complete");
        repo.complete_tasks(&[late], &ctx_at("2026-08-06T12:00:00+00:00", "2026-08-06"))
            .expect("complete");
        let today = |query: TaskQuery| -> Vec<String> {
            subjects(&repo.list(&query).expect("today"))
                .into_iter()
                .map(str::to_string)
                .collect()
        };

        let cleared = TaskQuery::new(TaskView::Today, day("2026-08-06"))
            .with_completed(true, Some(at("2026-08-06T10:00:00Z")));
        assert_eq!(today(cleared), vec!["Booked the room"]);

        let left_out =
            TaskQuery::new(TaskView::Today, day("2026-08-06")).with_completed(false, None);
        assert!(today(left_out).is_empty());

        // Completed still has both.
        assert_eq!(
            today(
                TaskQuery::new(TaskView::Completed, day("2026-08-06")).with_completed(false, None)
            )
            .len(),
            2
        );
    }

    #[test]
    fn a_completion_just_after_local_midnight_belongs_to_the_new_day() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Wrote the release notes").id.expect("id");

        // 00:30 local on the 7th, which is 22:30 UTC on the 6th. A UTC-based
        // day boundary files this under the 6th and drops it out of Today
        // immediately.
        repo.complete_tasks(&[id], &ctx_at("2026-08-06T22:30:00+00:00", "2026-08-07"))
            .expect("complete");

        assert!(
            repo.today_tasks(day("2026-08-06"))
                .expect("today")
                .is_empty()
        );
        assert_eq!(repo.today_tasks(day("2026-08-07")).expect("today").len(), 1);
    }

    #[test]
    fn today_follows_the_local_calendar_across_a_dst_transition() {
        let db = database();
        let repo = TaskRepository::with_zone(&db, CentralEurope);
        create_dated(&repo, "Clocks go forward", None, Some("2026-03-29"));

        // 23:30 local on the 28th (+01:00): not yet.
        let before = repo.today_at(at("2026-03-28T22:30:00Z"));
        assert_eq!(before, day("2026-03-28"));
        assert!(repo.today_tasks(before).expect("today").is_empty());

        // 00:30 local on the 29th, half an hour before the clocks jump.
        let during = repo.today_at(at("2026-03-28T23:30:00Z"));
        assert_eq!(during, day("2026-03-29"));
        assert_eq!(repo.today_tasks(during).expect("today").len(), 1);

        // 23:30 local on the 29th, now at +02:00: the same civil day.
        let after = repo.today_at(at("2026-03-29T21:30:00Z"));
        assert_eq!(after, day("2026-03-29"));
        assert_eq!(repo.today_tasks(after).expect("today").len(), 1);
    }

    /// The reminder parser is anchored on the local wall clock (§5.3), and the
    /// clock has to be the one the views use or `9am` would be tomorrow on one
    /// side of the app and today on the other.
    #[test]
    fn the_local_wall_clock_agrees_with_the_local_calendar() {
        let db = database();
        let repo = TaskRepository::with_zone(&db, CentralEurope);

        // 23:30 UTC on the 5th of August is 01:30 local on the 6th (+02:00):
        // the date has already rolled over and the time is past midnight, not
        // an hour and a half short of it.
        let past_midnight = repo.local_now(at("2026-08-05T23:30:00Z"));
        assert_eq!(past_midnight.date(), day("2026-08-06"));
        assert_eq!(past_midnight.time().to_string(), "01:30:00");

        // The winter offset is an hour smaller, and the wall clock has to
        // follow it rather than a frozen +02:00.
        let winter = repo.local_now(at("2026-12-05T23:30:00Z"));
        assert_eq!(winter.date(), day("2026-12-06"));
        assert_eq!(winter.time().to_string(), "00:30:00");

        // On the 25-hour day the clocks go back inside, the wall clock still
        // stays inside the day it reports.
        let long_day = repo.local_now(at("2026-10-25T23:30:00Z"));
        assert_eq!(long_day.date(), repo.today_at(at("2026-10-25T23:30:00Z")));
    }

    #[test]
    fn a_task_scheduled_a_year_out_appears_on_its_date_and_not_the_evening_before() {
        // §14.1: the reliability requirement this whole feature exists for.
        let db = database();
        let repo = TaskRepository::with_zone(&db, CentralEurope);
        create_dated(&repo, "Renew the domain", None, Some("2027-08-06"));

        let evening_before = repo.today_at(at("2027-08-05T21:30:00Z"));
        assert_eq!(evening_before, day("2027-08-05"));
        assert!(repo.today_tasks(evening_before).expect("today").is_empty());
        assert_eq!(
            repo.upcoming_tasks(evening_before).expect("upcoming").len(),
            1,
        );

        let the_day = repo.today_at(at("2027-08-05T22:30:00Z"));
        assert_eq!(the_day, day("2027-08-06"));
        assert_eq!(
            subjects(&repo.today_tasks(the_day).expect("today")),
            vec!["Renew the domain"]
        );
    }

    #[test]
    fn the_day_the_clocks_go_forward_is_twenty_three_hours_long() {
        // The midnight timer of §4.3 has to be scheduled from the zone, not
        // from "now plus 24 hours", or it drifts an hour twice a year.
        let db = database();
        let repo = TaskRepository::with_zone(&db, CentralEurope);
        let start = repo.next_local_midnight(at("2026-03-28T12:00:00Z"));
        let next = repo.next_local_midnight(start);
        assert_eq!(
            start,
            at("2026-03-28T23:00:00Z"),
            "midnight opening the 29th"
        );
        assert_eq!(next - start, Duration::hours(23));
    }

    #[test]
    fn the_system_zone_agrees_with_itself_about_where_a_day_starts() {
        let zone = SystemZone;
        for date in ["2026-01-01", "2026-03-29", "2026-08-06", "2026-10-25"] {
            let date = day(date);
            assert_eq!(
                zone.civil_date(zone.midnight(date)),
                date,
                "the first instant of a day must fall on that day",
            );
        }
    }

    // --- the five views (§4.1) -------------------------------------------

    #[test]
    fn undated_tasks_live_in_someday_and_not_in_today() {
        // The behaviour change §4.1 demands: the old query's
        // `snooze_until_unix IS NULL` put every undated task in Today, which
        // made Today the whole backlog.
        let db = database();
        let repo = repo(&db);
        create(&repo, "Someday, learn the cello");

        assert!(
            repo.today_tasks(day("2026-08-06"))
                .expect("today")
                .is_empty()
        );
        assert_eq!(
            subjects(&repo.someday_tasks(day("2026-08-06")).expect("someday")),
            vec!["Someday, learn the cello"],
        );
    }

    #[test]
    fn today_holds_what_is_postponed_due_or_overdue() {
        let db = database();
        let repo = repo(&db);
        create_dated(&repo, "Postponed to today", Some("2026-08-06"), None);
        create_dated(&repo, "Due today", None, Some("2026-08-06"));
        create_dated(&repo, "Overdue", None, Some("2026-07-30"));
        create_dated(&repo, "Postponed to tomorrow", Some("2026-08-07"), None);
        create_dated(&repo, "Due next month", None, Some("2026-09-06"));
        create(&repo, "Undated");

        let today = repo.today_tasks(day("2026-08-06")).expect("today");
        // Newest first: a new task goes on top.
        assert_eq!(
            subjects(&today),
            vec!["Overdue", "Due today", "Postponed to today"]
        );

        let overdue = today
            .iter()
            .find(|item| item.subject == "Overdue")
            .expect("overdue task");
        assert!(
            overdue.is_overdue(day("2026-08-06")),
            "overdue tasks are rendered distinctly from merely due ones",
        );
    }

    #[test]
    fn upcoming_holds_every_dated_task_in_date_order() {
        let db = database();
        let repo = repo(&db);
        create_dated(&repo, "In a month", None, Some("2026-09-06"));
        create_dated(&repo, "Tomorrow", Some("2026-08-07"), None);
        create_dated(&repo, "Next week", Some("2026-08-13"), Some("2026-08-20"));
        create_dated(&repo, "Today", None, Some("2026-08-06"));
        create_dated(&repo, "Overdue", None, Some("2026-08-01"));
        create(&repo, "Undated");

        assert_eq!(
            subjects(&repo.upcoming_tasks(day("2026-08-06")).expect("upcoming")),
            vec!["Overdue", "Today", "Tomorrow", "Next week", "In a month"],
            "ordered by the earliest of the two dates, not by manual position",
        );
    }

    #[test]
    fn the_later_today_cut_off_holds_and_one_cleared_here_goes_up() {
        let db = database();
        let repo = repo(&db);
        assert_eq!(repo.today_cleared_at().unwrap(), None);

        repo.clear_completed_from_today(at("2026-08-06T10:00:00Z"))
            .unwrap();
        let state = repo.today_cleared_state().unwrap();
        assert!(state.dirty);
        assert_eq!(state.revision, 1);

        // Another machine's earlier clearing does not bring the tasks back,
        // and this one's still goes up, above both.
        repo.take_in_today_cleared(Some(at("2026-08-06T09:00:00Z")), 4, Some("<a@x>"))
            .unwrap();
        assert_eq!(
            repo.today_cleared_at().unwrap(),
            Some(at("2026-08-06T10:00:00Z"))
        );
        let state = repo.today_cleared_state().unwrap();
        assert!(state.dirty);
        assert_eq!(state.revision, 5);
        assert!(repo.mark_today_cleared_uploaded(5, "<b@x>").unwrap());
        assert!(!repo.today_cleared_state().unwrap().dirty);

        // A later one from elsewhere is taken as it is.
        repo.take_in_today_cleared(Some(at("2026-08-06T11:00:00Z")), 6, Some("<c@x>"))
            .unwrap();
        assert_eq!(
            repo.today_cleared_at().unwrap(),
            Some(at("2026-08-06T11:00:00Z"))
        );
        assert_eq!(
            repo.today_cleared_state().unwrap(),
            RegistryState {
                revision: 6,
                dirty: false,
                message_id: Some("<c@x>".into())
            }
        );
    }

    #[test]
    fn completed_is_newest_first_and_paginated() {
        let db = database();
        let repo = repo(&db);
        for (index, subject) in ["First", "Second", "Third"].iter().enumerate() {
            let id = create(&repo, subject).id.expect("id");
            let hour = 9 + index;
            repo.complete_tasks(
                &[id],
                &ctx_at(&format!("2026-08-06T{hour:02}:00:00+00:00"), "2026-08-06"),
            )
            .expect("complete");
        }

        let page = repo
            .list(&TaskQuery::new(TaskView::Completed, day("2026-08-06")).page(2, 0))
            .expect("first page");
        assert_eq!(subjects(&page), vec!["Third", "Second"]);

        let second = repo
            .list(&TaskQuery::new(TaskView::Completed, day("2026-08-06")).page(2, 2))
            .expect("second page");
        assert_eq!(subjects(&second), vec!["First"]);
    }

    #[test]
    fn a_trashed_task_leaves_every_view_but_trash() {
        let db = database();
        let repo = repo(&db);
        let id = create_dated(&repo, "Cancelled", None, Some("2026-08-06"));
        repo.trash_tasks(&[id], &ctx()).expect("trash");

        let today = day("2026-08-06");
        assert!(repo.today_tasks(today).expect("today").is_empty());
        assert!(repo.someday_tasks(today).expect("someday").is_empty());
        assert!(repo.upcoming_tasks(today).expect("upcoming").is_empty());
        assert!(
            repo.completed_tasks(today, 0)
                .expect("completed")
                .is_empty()
        );
        assert_eq!(
            subjects(&repo.trashed_tasks(today).expect("trash")),
            vec!["Cancelled"]
        );
    }

    #[test]
    fn the_today_badge_counts_open_tasks_only() {
        let db = database();
        let repo = repo(&db);
        create_dated(&repo, "Still to do", None, Some("2026-08-06"));
        let done = create_dated(&repo, "Already done", None, Some("2026-08-06"));
        repo.complete_tasks(&[done], &ctx()).expect("complete");

        assert_eq!(repo.today_tasks(day("2026-08-06")).expect("today").len(), 2);
        assert_eq!(
            repo.today_badge_count(day("2026-08-06")).expect("badge"),
            1,
            "a badge that does not fall as work gets done is not a badge",
        );
    }

    // --- ordering (§4.4) --------------------------------------------------

    #[test]
    fn sixty_insertions_between_the_same_neighbours_stay_strictly_ordered() {
        // Halving the gap runs out of f64 after roughly fifty moves; this is
        // the renormalisation pass being exercised the only way it ever is.
        let db = database();
        let repo = repo(&db);
        let bottom = create(&repo, "Bottom").id.expect("id");
        let top = create(&repo, "Top").id.expect("id");
        let today = day("2026-08-06");

        for index in 0..60 {
            let inserted = create(&repo, &format!("Task {index}")).id.expect("id");
            repo.move_task(inserted, Some(top), Some(bottom), &ctx())
                .expect("move between");

            let items = repo.someday_tasks(today).expect("list");
            let mut previous = f64::NEG_INFINITY;
            for item in &items {
                assert!(
                    item.position > previous,
                    "positions collapsed at insertion {index}: {:?}",
                    items.iter().map(|item| item.position).collect::<Vec<_>>(),
                );
                previous = item.position;
            }
            assert_eq!(items.first().map(|item| item.id), Some(top));
            assert_eq!(items.last().map(|item| item.id), Some(bottom));
            assert_eq!(
                items.get(1).map(|item| item.id),
                Some(inserted),
                "the newest insertion sits directly below the top task",
            );
        }
        assert_eq!(repo.someday_tasks(today).expect("list").len(), 62);
        assert!(
            reload(&repo, bottom).position > 2.0 * POSITION_STEP,
            "the run must have exhausted the gap and renormalised at least once",
        );
    }

    #[test]
    fn a_selection_moves_as_one_block_in_its_own_order() {
        let db = database();
        let repo = repo(&db);
        let ids: Vec<i64> = ["E", "D", "C", "B", "A"]
            .iter()
            .map(|subject| create(&repo, subject).id.expect("id"))
            .collect();
        let id = |subject: &str| ids[4 - (subject.as_bytes()[0] - b'A') as usize];
        let order = || -> Vec<String> {
            subjects(&repo.someday_tasks(day("2026-08-06")).expect("list"))
                .into_iter()
                .map(str::to_string)
                .collect()
        };
        assert_eq!(order(), ["A", "B", "C", "D", "E"]);

        repo.move_tasks(&[id("B"), id("D")], Some(id("E")), None, &ctx())
            .expect("to the end");
        assert_eq!(order(), ["A", "C", "E", "B", "D"]);

        let outcome = repo
            .move_tasks(&[id("C"), id("D")], None, Some(id("A")), &ctx())
            .expect("to the top");
        assert_eq!(order(), ["C", "D", "A", "E", "B"]);
        assert_eq!(outcome.undo.steps.len(), 2, "one undo step for the block");
    }

    #[test]
    fn renormalisation_rewrites_the_list_to_multiples_of_the_step() {
        let db = database();
        let repo = repo(&db);
        // First on a step, Second a step below it, Third between them.
        let first = create(&repo, "First").id.expect("id");
        let second = create(&repo, "Second").id.expect("id");
        repo.move_task(second, Some(first), None, &ctx())
            .expect("move");
        let third = create(&repo, "Third").id.expect("id");
        repo.move_task(third, Some(first), Some(second), &ctx())
            .expect("move");
        for (index, id) in [first, second, third].iter().enumerate() {
            mark_clean(&repo, *id, index as u32 + 1);
        }
        let before: Vec<i64> = [first, second, third]
            .iter()
            .map(|id| reload(&repo, *id).revision)
            .collect();

        let rewritten = repo
            .renormalise_positions(ACCOUNT, &ctx())
            .expect("renormalise");
        assert_eq!(rewritten, 2, "only the rows that actually move are written");

        let items = repo.someday_tasks(day("2026-08-06")).expect("list");
        assert_eq!(subjects(&items), vec!["First", "Third", "Second"]);
        assert_eq!(
            items.iter().map(|item| item.position).collect::<Vec<_>>(),
            vec![1024.0, 2048.0, 3072.0],
        );

        for (id, was) in [(third, before[2]), (second, before[1])] {
            let task = reload(&repo, id);
            assert!(
                task.is_dirty,
                "a rewritten position has to reach the server"
            );
            assert!(
                task.revision > was,
                "a position the server never hears about is overwritten by the next sync",
            );
        }
        assert_eq!(
            reload(&repo, first).revision,
            before[0],
            "the row that did not move is not rewritten",
        );
    }

    #[test]
    fn moving_a_task_to_the_top_writes_one_row() {
        let db = database();
        let repo = repo(&db);
        let first = create(&repo, "First").id.expect("id");
        let second = create(&repo, "Second").id.expect("id");
        let untouched = reload(&repo, first).revision;

        repo.move_task(second, None, Some(first), &ctx())
            .expect("move to top");

        assert_eq!(
            subjects(&repo.someday_tasks(day("2026-08-06")).expect("list")),
            vec!["Second", "First"],
        );
        assert_eq!(
            reload(&repo, first).revision,
            untouched,
            "a drag writes exactly one row",
        );
    }

    // --- revisions --------------------------------------------------------

    #[test]
    fn every_mutation_bumps_the_revision_and_stamps_updated_at() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Draft the proposal").id.expect("id");
        mark_clean(&repo, id, 1);
        let before = reload(&repo, id);

        let later = ctx_at("2026-08-06T15:00:00+00:00", "2026-08-06");
        repo.apply(
            id,
            &TaskUpdates::new().with_subject("Send the proposal".into()),
            &later,
        )
        .expect("rename");

        let after = reload(&repo, id);
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(after.updated_at, format_timestamp(later.now));
        assert!(after.is_dirty);
    }

    #[test]
    fn an_update_that_changes_nothing_bumps_nothing() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Water the plants").id.expect("id");
        mark_clean(&repo, id, 1);
        let before = reload(&repo, id);

        let outcome = repo
            .apply(
                id,
                &TaskUpdates::new().with_subject("Water the plants".into()),
                &ctx(),
            )
            .expect("apply");

        assert!(outcome.is_noop());
        assert!(outcome.undo.is_empty());
        let after = reload(&repo, id);
        assert_eq!(after.revision, before.revision);
        assert!(
            !after.is_dirty,
            "a spurious revision would win a conflict against a real edit elsewhere",
        );
    }

    #[test]
    fn the_focus_flag_reaches_the_row_without_bumping_the_revision() {
        // "Currently working on" is local UI state: it has to persist, but it
        // must not queue an upload or hand this machine a revision high enough
        // to beat a real edit made on another one.
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Focus on this").id.expect("id");
        mark_clean(&repo, id, 1);
        let before = reload(&repo, id).revision;

        repo.apply(id, &TaskUpdates::new().with_is_working_on(true), &ctx())
            .expect("focus");

        let task = reload(&repo, id);
        assert!(task.is_working_on);
        assert_eq!(task.revision, before);
        assert!(!task.is_dirty);
    }

    #[test]
    fn a_mutation_joins_a_transaction_the_caller_already_opened() {
        // The sync layer groups a whole downloaded-task apply into one
        // transaction and calls repository methods inside it; SQLite rejects a
        // nested BEGIN, and splitting the inner group out would let the outer
        // one commit in pieces.
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Grouped").id.expect("id");

        {
            let tx = db.immediate_transaction().expect("transaction");
            repo.apply(
                id,
                &TaskUpdates::new().with_subject("Renamed".into()),
                &ctx(),
            )
            .expect("edit inside the caller's transaction");
            // Dropped without committing: the whole group rolls back.
            drop(tx);
        }

        assert_eq!(
            reload(&repo, id).subject,
            "Grouped",
            "the edit belonged to the caller's transaction, not to one of its own",
        );
    }

    #[test]
    fn linking_a_message_bumps_the_revision_and_logs_it() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Reply to Anna").id.expect("id");
        mark_clean(&repo, id, 1);
        let before = reload(&repo, id).revision;

        let metadata = CachedEmailMetadata {
            subject: Some("Contract draft".to_string()),
            ..CachedEmailMetadata::default()
        };
        repo.link_email(id, "<abc@example.com>", &metadata, &ctx())
            .expect("link");

        let task = reload(&repo, id);
        assert_eq!(task.revision, before + 1);
        assert!(task.is_dirty);
        assert!(
            task.log
                .iter()
                .any(|entry| matches!(entry.event, TaskEvent::LinkAdded { .. }))
        );
        assert_eq!(repo.get_linked_emails(id).expect("links").len(), 1);

        repo.unlink_email(id, "<abc@example.com>", &ctx())
            .expect("unlink");
        assert!(repo.get_linked_emails(id).expect("links").is_empty());
        assert_eq!(reload(&repo, id).revision, before + 2);
    }

    /// Pressing the link key again on the same message changed nothing, yet
    /// each press made a new revision and a log entry.
    #[test]
    fn linking_a_linked_message_again_changes_nothing() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Reply to Anna").id.expect("id");
        let metadata = CachedEmailMetadata {
            subject: Some("Contract draft".to_string()),
            folder: Some("INBOX".to_string()),
            ..CachedEmailMetadata::default()
        };
        repo.link_email(id, "<abc@example.com>", &metadata, &ctx())
            .expect("link");
        let linked = reload(&repo, id);

        repo.link_email(id, "<abc@example.com>", &metadata, &ctx())
            .expect("link again");
        let again = reload(&repo, id);
        assert_eq!(again.revision, linked.revision);
        assert_eq!(again.log.len(), linked.log.len());
        assert_eq!(repo.get_linked_emails(id).expect("links").len(), 1);

        // The message has moved since: the remembered folder is brought up to
        // date, which is a change to the task, but not a second link.
        let moved = CachedEmailMetadata {
            folder: Some("Archive".to_string()),
            ..metadata
        };
        repo.link_email(id, "<abc@example.com>", &moved, &ctx())
            .expect("link after the move");
        let after = reload(&repo, id);
        assert_eq!(after.revision, linked.revision + 1);
        assert_eq!(after.log.len(), linked.log.len());
        let links = repo.get_linked_emails(id).expect("links");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].cached_folder.as_deref(), Some("Archive"));
    }

    // --- tags (§7) --------------------------------------------------------

    #[test]
    fn deleting_a_tag_detaches_it_and_keeps_the_tasks() {
        let db = database();
        let repo = repo(&db);
        let errands = repo.create_tag("errands", None).expect("create tag");
        let first = create(&repo, "Buy milk").id.expect("id");
        let second = create(&repo, "Collect parcel").id.expect("id");
        repo.add_tag_to(&[first, second], &errands.tag_uuid, &ctx())
            .expect("tag");
        mark_clean(&repo, first, 1);
        mark_clean(&repo, second, 2);
        let revision_before = repo.tag_registry_revision().expect("revision");

        let touched = repo.delete_tag(&errands.tag_uuid, &ctx()).expect("delete");

        assert_eq!(touched.changed.len(), 2);
        assert!(repo.list_tags().expect("tags").is_empty());
        assert!(repo.tag_registry_revision().expect("revision") > revision_before);
        for id in [first, second] {
            let task = reload(&repo, id);
            assert!(task.tags.is_empty(), "the tag is detached");
            assert!(
                task.is_dirty,
                "the task's header still lists the tag until it is rewritten",
            );
            assert!(
                task.log
                    .iter()
                    .any(|entry| matches!(entry.event, TaskEvent::TagRemoved { .. }))
            );
        }
        assert_eq!(
            repo.someday_tasks(day("2026-08-06")).expect("list").len(),
            2
        );
    }

    #[test]
    fn renaming_a_tag_touches_no_task() {
        // Tasks store tag ids, never names, which is the entire reason a rename
        // rewrites one message instead of every task that uses it.
        let db = database();
        let repo = repo(&db);
        let tag = repo.create_tag("erands", None).expect("create tag");
        let id = create(&repo, "Buy milk").id.expect("id");
        repo.add_tag_to(&[id], &tag.tag_uuid, &ctx()).expect("tag");
        mark_clean(&repo, id, 1);
        let before = reload(&repo, id);

        assert!(
            repo.update_tag(&tag.tag_uuid, "errands", Some("#ff8800"))
                .expect("rename")
        );

        let after = reload(&repo, id);
        assert_eq!(after.revision, before.revision);
        assert!(!after.is_dirty);
        let renamed = repo.get_tag(&tag.tag_uuid).expect("get").expect("exists");
        assert_eq!(renamed.name, "errands");
        assert_eq!(renamed.colour.as_deref(), Some("#ff8800"));
    }

    /// A tag change marks the registry as unsent until the revision that
    /// changed went up; a change made while it was uploading keeps it unsent.
    #[test]
    fn the_registry_stays_unsent_until_its_revision_is_uploaded() {
        let db = database();
        let repo = repo(&db);
        assert!(!repo.tag_registry_state().expect("state").dirty);

        repo.create_tag("Errands", None).expect("tag");
        let state = repo.tag_registry_state().expect("state");
        assert!(state.dirty, "a new tag has not been sent");

        repo.create_tag("Work", None)
            .expect("tag during the upload");
        assert!(
            !repo
                .mark_registry_uploaded(state.revision, "<registry.a@orb>", &[])
                .expect("mark"),
            "the upload carried an older revision"
        );
        let after = repo.tag_registry_state().expect("state");
        assert!(after.dirty, "the second tag still has to go up");

        assert!(
            repo.mark_registry_uploaded(after.revision, "<registry.b@orb>", &[])
                .expect("mark")
        );
        let sent = repo.tag_registry_state().expect("state");
        assert!(!sent.dirty);
        assert_eq!(sent.message_id.as_deref(), Some("<registry.b@orb>"));

        repo.replace_tag_registry(&[], sent.revision + 1, Some("<registry.c@orb>"))
            .expect("download");
        let downloaded = repo.tag_registry_state().expect("state");
        assert!(!downloaded.dirty);
        assert_eq!(downloaded.message_id.as_deref(), Some("<registry.c@orb>"));
    }

    #[test]
    fn a_tag_id_with_no_registry_entry_is_kept_and_rendered_as_a_placeholder() {
        // Unresolvable is not deleted (§7): the registry message may simply not
        // have synced yet, and dropping the link would lose the tag for good on
        // this task's next upload.
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Tagged elsewhere").id.expect("id");
        repo.set_tags(id, vec!["ghost-tag-uuid".to_string()], &ctx())
            .expect("set tags");

        repo.replace_tag_registry(&[], 7, None)
            .expect("replace registry");

        assert_eq!(reload(&repo, id).tags, vec!["ghost-tag-uuid".to_string()]);
        assert_eq!(tag_label("ghost-tag-uuid", &[]), "ghost-tag-uuid");
        assert_eq!(repo.tag_registry_revision().expect("revision"), 7);
    }

    // --- the log (§11) ----------------------------------------------------

    #[test]
    fn appending_the_same_entries_twice_changes_nothing() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Keep a diary").id.expect("id");
        let entry = TaskLogEntry::new(TaskEvent::BodyChanged, &ctx());

        assert_eq!(
            repo.append_log(id, std::slice::from_ref(&entry))
                .expect("append"),
            1
        );
        assert_eq!(
            repo.append_log(id, std::slice::from_ref(&entry))
                .expect("append"),
            0,
            "re-applying a log that has already been seen must change nothing",
        );
        assert_eq!(repo.get_log(id).expect("log").len(), 2);
    }

    #[test]
    fn merging_two_devices_logs_keeps_both() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Shared task").id.expect("id");
        let local = TaskLogEntry::new(TaskEvent::BodyChanged, &ctx());
        repo.append_log(id, std::slice::from_ref(&local))
            .expect("append");

        let remote_ctx = ChangeContext::new(at("2026-08-06T11:00:00+00:00"), day("2026-08-06"))
            .with_device_id(Some("device-b"));
        let remote = TaskLogEntry::new(TaskEvent::Uncompleted, &remote_ctx);

        let merged = repo
            .merge_log(id, std::slice::from_ref(&remote))
            .expect("merge");
        assert_eq!(merged.len(), 3, "creation plus one entry from each device");
        let again = repo.merge_log(id, &[remote]).expect("merge again");
        assert_eq!(again.len(), 3, "the union merge is idempotent");
        assert!(
            merged
                .iter()
                .any(|entry| entry.device_id.as_deref() == Some("device-b"))
        );
        assert!(
            merged
                .iter()
                .any(|entry| entry.entry_uuid == local.entry_uuid)
        );
    }

    // --- completion and trash (§10) ---------------------------------------

    #[test]
    fn trashing_is_a_soft_delete_and_restoring_undoes_it() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Abandoned idea").id.expect("id");

        repo.trash_tasks(&[id], &ctx()).expect("trash");
        assert!(reload(&repo, id).deleted_at.is_some());

        repo.restore_tasks(&[id], &ctx()).expect("restore");
        let task = reload(&repo, id);
        assert!(task.deleted_at.is_none());
        assert!(
            task.log
                .iter()
                .any(|entry| matches!(entry.event, TaskEvent::Trashed))
        );
        assert!(
            task.log
                .iter()
                .any(|entry| matches!(entry.event, TaskEvent::Restored))
        );
    }

    #[test]
    fn only_trash_older_than_thirty_days_is_purged() {
        let db = database();
        let repo = repo(&db);
        let old = create(&repo, "Trashed in June").id.expect("id");
        let recent = create(&repo, "Trashed yesterday").id.expect("id");
        repo.trash_tasks(&[old], &ctx_at("2026-06-01T10:00:00+00:00", "2026-06-01"))
            .expect("trash");
        repo.trash_tasks(
            &[recent],
            &ctx_at("2026-08-05T10:00:00+00:00", "2026-08-05"),
        )
        .expect("trash");
        mark_clean(&repo, old, 17);

        let purged = repo
            .purge_expired_trash(at("2026-08-06T10:00:00+00:00"))
            .expect("purge");

        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0].id, old);
        assert_eq!(
            purged[0].imap_uid,
            Some(17),
            "the sync layer has to expunge the message this row pointed at",
        );
        assert!(repo.get_task(old).expect("get").is_none());
        assert!(repo.get_task(recent).expect("get").is_some());
    }

    #[test]
    fn emptying_the_trash_removes_everything_in_it() {
        let db = database();
        let repo = repo(&db);
        let first = create(&repo, "One").id.expect("id");
        let second = create(&repo, "Two").id.expect("id");
        let kept = create(&repo, "Three").id.expect("id");
        repo.trash_tasks(&[first, second], &ctx()).expect("trash");

        let purged = repo.empty_trash().expect("empty");

        assert_eq!(purged.len(), 2);
        assert!(repo.get_task(kept).expect("get").is_some());
        assert!(
            repo.trashed_tasks(day("2026-08-06"))
                .expect("trash")
                .is_empty()
        );
        assert_eq!(
            repo.get_log(first).expect("log").len(),
            0,
            "the log goes with the task"
        );
    }

    /// Steps are stored with the task and counted for the list; a recurring
    /// task that advances starts its checklist afresh (todo-spec §17).
    #[test]
    fn steps_are_kept_counted_and_reopened_by_an_advance() {
        let db = database();
        let repo = repo(&db);
        let id = create_dated(&repo, "Weekly review", None, Some("2026-08-06"));
        let steps = vec![
            TaskStep {
                done: true,
                ..TaskStep::new("Inbox to zero".to_string())
            },
            TaskStep::new("Plan the week".to_string()),
        ];
        repo.apply(id, &TaskUpdates::new().with_steps(steps.clone()), &ctx())
            .expect("steps");
        assert_eq!(reload(&repo, id).steps, steps);
        let listed = repo
            .list(&TaskQuery::new(TaskView::Today, day("2026-08-06")))
            .expect("today");
        assert_eq!((listed[0].steps_done, listed[0].steps_total), (1, 2));

        let mut updates = TaskUpdates::new();
        updates.recurrence = Some(Some("FREQ=WEEKLY".to_string()));
        repo.apply(id, &updates, &ctx()).expect("set recurrence");
        repo.complete_tasks(&[id], &ctx()).expect("complete");
        let task = reload(&repo, id);
        assert_eq!(task.due_date.as_deref(), Some("2026-08-13"));
        assert!(task.steps.iter().all(|step| !step.done));
        assert_eq!(task.steps.len(), 2);
    }

    #[test]
    fn a_repeating_task_completed_today_stays_in_today_as_done() {
        let db = database();
        let repo = repo(&db);
        let id = create_dated(&repo, "Water the plants", None, Some("2026-08-06"));
        let mut updates = TaskUpdates::new();
        updates.recurrence = Some(Some("FREQ=DAILY".to_string()));
        repo.apply(id, &updates, &ctx()).expect("set recurrence");
        repo.complete_tasks(&[id], &ctx()).expect("complete");

        let today = repo.today_tasks(day("2026-08-06")).expect("today");
        let watered = today
            .iter()
            .find(|item| item.id == id)
            .expect("still in Today");
        assert!(watered.is_done_for_today);
        assert!(!watered.is_completed, "open again for tomorrow");

        let cleared = TaskQuery::new(TaskView::Today, day("2026-08-06"))
            .with_completed(true, Some(at("2026-08-06T11:00:00Z")));
        assert!(
            !repo
                .list(&cleared)
                .expect("today")
                .iter()
                .any(|item| item.id == id)
        );
        let hidden = TaskQuery::new(TaskView::Today, day("2026-08-06")).with_completed(false, None);
        assert!(
            !repo
                .list(&hidden)
                .expect("today")
                .iter()
                .any(|item| item.id == id)
        );
        // Tomorrow it is simply due.
        let tomorrow = repo.today_tasks(day("2026-08-07")).expect("tomorrow");
        let due = tomorrow.iter().find(|item| item.id == id).expect("due");
        assert!(!due.is_done_for_today);
    }

    #[test]
    fn completing_a_recurring_task_advances_it_instead_of_ending_it() {
        let db = database();
        let repo = repo(&db);
        let id = create_dated(&repo, "Water the plants", None, Some("2026-08-06"));
        let mut updates = TaskUpdates::new();
        updates.recurrence = Some(Some("FREQ=WEEKLY".to_string()));
        repo.apply(id, &updates, &ctx()).expect("set recurrence");

        let outcome = repo.complete_tasks(&[id], &ctx()).expect("complete");

        let task = reload(&repo, id);
        assert!(
            !task.is_completed,
            "the series keeps its identity and its log"
        );
        assert_eq!(task.due_date.as_deref(), Some("2026-08-13"));
        assert_eq!(
            outcome.reminder_shifts.len(),
            1,
            "reminders follow the task forward",
        );
        assert!(
            task.log
                .iter()
                .any(|entry| matches!(entry.event, TaskEvent::RecurrenceAdvanced { .. }))
        );
        assert!(
            task.log
                .iter()
                .any(|entry| matches!(entry.event, TaskEvent::Completed { .. })),
            "the Completed view shows a recurring task by its logged completions",
        );
    }

    // --- undo (§10.3) -----------------------------------------------------

    #[test]
    fn undo_restores_the_previous_values_as_a_new_revision() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Book the flights").id.expect("id");
        let outcome = repo
            .set_due_date(&[id], Some(day("2026-09-01")), &ctx())
            .expect("set due");
        let after_edit = reload(&repo, id);

        let redo = repo.undo(&outcome.undo, &ctx()).expect("undo");

        let restored = reload(&repo, id);
        assert_eq!(restored.due_date, None);
        assert!(
            restored.revision > after_edit.revision,
            "undo moves history forward, it does not rewind it",
        );
        assert_eq!(
            restored
                .log
                .iter()
                .filter(|entry| matches!(entry.event, TaskEvent::DateChanged { .. }))
                .count(),
            2,
            "both the edit and its undo stay in the timeline",
        );

        repo.undo(&redo.undo, &ctx()).expect("redo");
        assert_eq!(
            reload(&repo, id).due_date.as_deref(),
            Some("2026-09-01"),
            "the undo of an undo is a redo",
        );
    }

    #[test]
    fn undo_of_a_bulk_completion_restores_every_task_in_the_selection() {
        let db = database();
        let repo = repo(&db);
        let ids: Vec<i64> = ["One", "Two", "Three"]
            .iter()
            .map(|subject| create(&repo, subject).id.expect("id"))
            .collect();

        let outcome = repo.complete_tasks(&ids, &ctx()).expect("complete");
        assert_eq!(outcome.changed.len(), 3);
        assert_eq!(outcome.undo.label, "complete 3 tasks");

        repo.undo(&outcome.undo, &ctx()).expect("undo");

        for id in ids {
            assert!(!reload(&repo, id).is_completed);
        }
    }

    #[test]
    fn undo_of_a_creation_trashes_the_task() {
        let db = database();
        let repo = repo(&db);
        let task = create(&repo, "Created by accident");
        let id = task.id.expect("id");

        let undo = UndoEntry::for_creation(&task, "create");
        repo.undo(&undo, &ctx()).expect("undo");

        assert!(
            reload(&repo, id).deleted_at.is_some(),
            "a soft delete syncs; a hard one leaves the message behind",
        );
    }

    // --- search (§4.5) ----------------------------------------------------

    #[test]
    fn search_matches_title_body_and_tag_name() {
        let db = database();
        let repo = repo(&db);
        let tag = repo.create_tag("garden", None).expect("tag");
        create(&repo, "Call the plumber");
        let noted = create(&repo, "Read the manual").id.expect("id");
        repo.apply(
            noted,
            &TaskUpdates::new().with_body_text(Some("the plumber left instructions".into())),
            &ctx(),
        )
        .expect("body");
        let tagged = create(&repo, "Mow the lawn").id.expect("id");
        repo.add_tag_to(&[tagged], &tag.tag_uuid, &ctx())
            .expect("tag");

        let today = day("2026-08-06");
        let matches = |needle: &str| {
            let query = TaskQuery::new(TaskView::Someday, today).with_search(needle);
            subjects(&repo.list(&query).expect("search"))
                .iter()
                .map(|subject| subject.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            matches("plumber"),
            vec!["Read the manual", "Call the plumber"]
        );
        assert_eq!(matches("garden"), vec!["Mow the lawn"]);
        assert_eq!(matches("call plumber"), vec!["Call the plumber"]);
        assert!(matches("nothing here").is_empty());
    }

    #[test]
    fn a_tag_term_filters_by_tag_and_never_leaves_the_view() {
        let db = database();
        let repo = repo(&db);
        let tag = repo.create_tag("errands", None).expect("tag");
        let open = create(&repo, "Buy stamps").id.expect("id");
        let done = create(&repo, "Buy bread").id.expect("id");
        repo.add_tag_to(&[open, done], &tag.tag_uuid, &ctx())
            .expect("tag");
        repo.complete_tasks(&[done], &ctx()).expect("complete");

        let today = day("2026-08-06");
        let someday = repo
            .list(&TaskQuery::new(TaskView::Someday, today).with_search("tag:errands"))
            .expect("search");
        assert_eq!(subjects(&someday), vec!["Buy stamps"]);

        let completed = repo
            .list(&TaskQuery::new(TaskView::Completed, today).with_search("tag:err"))
            .expect("search");
        assert_eq!(
            subjects(&completed),
            vec!["Buy bread"],
            "search filters the current view rather than searching across views",
        );
    }

    #[test]
    fn a_percent_typed_into_the_search_box_is_not_a_wildcard() {
        let db = database();
        let repo = repo(&db);
        create(&repo, "Increase margin by 5%");
        create(&repo, "Anything at all");

        let query = TaskQuery::new(TaskView::Someday, day("2026-08-06")).with_search("%");
        assert_eq!(
            subjects(&repo.list(&query).expect("search")),
            vec!["Increase margin by 5%"],
        );
    }

    fn found(repo: &TaskRepository<'_>, query: &str) -> Vec<String> {
        let query = crate::search::query::parse(query).expect("query");
        subjects(&repo.search(&query).expect("search"))
            .iter()
            .map(|subject| subject.to_string())
            .collect()
    }

    #[test]
    fn search_finds_tasks_in_every_view_but_the_trash() {
        let db = database();
        let repo = repo(&db);
        create_dated(&repo, "Roadmap next week", Some("2026-08-12"), None);
        create(&repo, "Roadmap someday");
        create_dated(&repo, "Roadmap overdue", None, Some("2026-08-01"));
        create_dated(&repo, "Roadmap today", Some("2026-08-06"), None);
        let done = create(&repo, "Roadmap done").id.expect("id");
        repo.complete_tasks(&[done], &ctx()).expect("complete");
        let gone = create(&repo, "Roadmap trashed").id.expect("id");
        repo.trash_tasks(&[gone], &ctx()).expect("trash");
        create(&repo, "Something else");

        assert_eq!(
            found(&repo, "roadmap"),
            vec![
                "Roadmap overdue",
                "Roadmap today",
                "Roadmap next week",
                "Roadmap someday",
                "Roadmap done",
            ],
            "by the date each comes up, the undated after, the completed last",
        );
    }

    #[test]
    fn search_reads_the_fields_of_the_mail_query_language() {
        let db = database();
        let repo = repo(&db);
        let tag = repo.create_tag("garden", None).expect("tag");
        create(&repo, "Call the plumber");
        let noted = create(&repo, "Read the manual").id.expect("id");
        repo.apply(
            noted,
            &TaskUpdates::new().with_body_text(Some("the plumber left instructions".into())),
            &ctx(),
        )
        .expect("body");
        let tagged = create(&repo, "Mow the lawn").id.expect("id");
        repo.add_tag_to(&[tagged], &tag.tag_uuid, &ctx())
            .expect("tag");

        assert_eq!(found(&repo, "s:plumber"), vec!["Call the plumber"]);
        assert_eq!(found(&repo, "b:plumber"), vec!["Read the manual"]);
        assert_eq!(found(&repo, "tag:gar"), vec!["Mow the lawn"]);
        assert_eq!(found(&repo, "plumber -s:call"), vec!["Read the manual"]);
        assert_eq!(
            found(&repo, "s:call OR tag:garden"),
            vec!["Mow the lawn", "Call the plumber"]
        );
        assert!(found(&repo, "f:plumber").is_empty(), "tasks have no sender");
        assert!(found(&repo, "plumber is:unread").is_empty());
        assert_eq!(
            found(&repo, "f:anna OR s:plumber"),
            vec!["Call the plumber"],
            "an alternative only mail can match leaves the others",
        );
    }

    #[test]
    fn search_terms_are_split_on_quotes_and_the_tag_prefix() {
        let terms = parse_search("  TAG:\"next actions\" tag:errands write the  spec ");
        assert_eq!(terms.tags, vec!["next actions", "errands"]);
        assert_eq!(terms.text, vec!["write", "the", "spec"]);
        assert!(parse_search("   ").is_empty());
        assert_eq!(parse_search("tag:").text, Vec::<String>::new());
    }

    // --- sync-facing behaviour (§3) ---------------------------------------

    #[test]
    fn the_dirty_flag_is_cleared_only_for_the_revision_that_was_uploaded() {
        // §3.2: an edit made while the upload was in flight must not be marked
        // clean, or it is never uploaded and the next download destroys it.
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Long upload").id.expect("id");
        let uploaded_revision = reload(&repo, id).revision;
        repo.apply(
            id,
            &TaskUpdates::new().with_subject("Edited mid-flight".into()),
            &ctx(),
        )
        .expect("edit");

        let cleared = repo
            .mark_uploaded(id, uploaded_revision, 42, "<stale@orb>")
            .expect("mark uploaded");

        let task = reload(&repo, id);
        assert!(!cleared);
        assert!(
            task.is_dirty,
            "the concurrent edit still has to reach the server"
        );
        assert_eq!(
            task.message_id.as_deref(),
            Some("<stale@orb>"),
            "the uploaded message still has to be found and expunged",
        );

        assert!(
            repo.mark_uploaded(id, task.revision, 43, "<current@orb>")
                .expect("mark uploaded")
        );
        assert!(!reload(&repo, id).is_dirty);
    }

    /// A reminder is stored in its own table but published inside the task
    /// message, so setting one is an edit to the task whether or not any field
    /// of the task itself moved. Without the bump the reminder never leaves this
    /// machine, and the next download — whose wire form does not mention it —
    /// deletes it (§14 requirement 2).
    #[test]
    fn adding_a_reminder_queues_the_task_for_upload() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Renew the passport").id.expect("id");
        mark_clean(&repo, id, 7);
        let before = reload(&repo, id).revision;

        repo.add_reminder(id, "2027-08-06T09:00:00+02:00", &ctx())
            .expect("reminder");

        let after = reload(&repo, id);
        assert!(after.is_dirty, "the reminder has to reach the server");
        assert_eq!(after.revision, before + 1);
    }

    #[test]
    fn removing_a_reminder_publishes_the_smaller_set() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Renew the passport").id.expect("id");
        let reminder_id = repo
            .add_reminder(id, "2027-08-06T09:00:00+02:00", &ctx())
            .expect("reminder");
        mark_clean(&repo, id, 7);
        let before = reload(&repo, id).revision;

        assert!(repo.delete_reminder(reminder_id, &ctx()).expect("delete"));
        let after = reload(&repo, id);
        assert!(after.is_dirty);
        assert_eq!(after.revision, before + 1);

        // A second delete finds nothing and must not invent an edit — a
        // revision bump with no change is a pointless upload and, on the other
        // machine, a pointless conflict.
        mark_clean(&repo, id, 8);
        assert!(!repo.delete_reminder(reminder_id, &ctx()).expect("delete"));
        assert!(!reload(&repo, id).is_dirty);
    }

    /// Whether *this* machine has fired a reminder is nobody else's business
    /// (§5.2), so it must not queue an upload — otherwise every machine
    /// re-uploads every task every morning and each one looks to the others like
    /// a fresh edit.
    #[test]
    fn firing_a_reminder_publishes_nothing() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Renew the passport").id.expect("id");
        let reminder_id = repo
            .add_reminder(id, "2027-08-06T09:00:00+02:00", &ctx())
            .expect("reminder");
        mark_clean(&repo, id, 7);
        let before = reload(&repo, id).revision;

        repo.mark_reminder_triggered(reminder_id).expect("trigger");

        let after = reload(&repo, id);
        assert!(!after.is_dirty);
        assert_eq!(after.revision, before);
    }

    /// A recurrence advance moves every reminder at once. Doing that as N
    /// deletes and N inserts would bump the revision 2N times for one user
    /// action; more to the point it would leave the set momentarily short, and a
    /// sync that read it there would publish the gap.
    #[test]
    fn replacing_the_reminder_set_is_one_edit() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Water the plants").id.expect("id");
        for hour in ["07", "08", "09"] {
            repo.add_reminder(id, &format!("2027-08-06T{hour}:00:00+02:00"), &ctx())
                .expect("reminder");
        }
        mark_clean(&repo, id, 7);
        let before = reload(&repo, id).revision;

        let moved: Vec<TaskReminder> = repo
            .get_reminders(id)
            .expect("reminders")
            .into_iter()
            .map(|reminder| {
                TaskReminder::new(
                    id,
                    reminder.remind_at.clone(),
                    reminder.remind_at_unix + 86_400,
                )
            })
            .collect();
        assert_eq!(repo.replace_reminders(id, &moved, &ctx()).expect("swap"), 3);

        let after = reload(&repo, id);
        assert_eq!(after.revision, before + 1, "one action, one revision");
        assert!(after.is_dirty);
        let stored = repo.get_reminders(id).expect("reminders");
        assert_eq!(stored.len(), 3);
        assert_eq!(
            stored.iter().map(|r| r.remind_at_unix).collect::<Vec<_>>(),
            moved.iter().map(|r| r.remind_at_unix).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_downloaded_task_keeps_local_reminder_state_and_unions_the_log() {
        let db = database();
        let repo = repo(&db);
        let local = create(&repo, "Shared task");
        let id = local.id.expect("id");
        let reminder_id = repo
            .insert_reminder(
                &TaskReminder::new(id, "2026-08-06T09:00:00Z".into(), 1_775_000_000),
                &ctx(),
            )
            .expect("reminder");
        repo.mark_reminder_triggered(reminder_id).expect("trigger");

        let mut incoming = Task::bare(
            local.task_uuid.clone(),
            "Renamed on the other machine".to_string(),
            ACCOUNT.to_string(),
            local.position,
            local.created_at.clone(),
        );
        incoming.revision = local.revision + 1;
        let remote_ctx = ChangeContext::new(at("2026-08-06T12:00:00+00:00"), day("2026-08-06"))
            .with_device_id(Some("device-b"));
        incoming.log = vec![TaskLogEntry::new(TaskEvent::BodyChanged, &remote_ctx)];

        let stored = repo
            .store_remote_task(
                &incoming,
                &[TaskReminder::new(
                    0,
                    "2026-08-06T09:00:00Z".into(),
                    1_775_000_000,
                )],
                &[],
                &[],
            )
            .expect("store");

        assert_eq!(
            stored, id,
            "the task is matched by its uuid, not its message id"
        );
        let task = reload(&repo, id);
        assert_eq!(task.subject, "Renamed on the other machine");
        assert!(!task.is_dirty);
        assert_eq!(
            task.log.len(),
            2,
            "the creation entry survives the download"
        );
        let reminders = repo.get_reminders(id).expect("reminders");
        assert_eq!(reminders.len(), 1);
        assert!(
            reminders[0].is_triggered,
            "is_triggered is local per machine and must not be reset by a sync",
        );
    }

    #[test]
    fn the_local_index_reports_what_reconciliation_needs() {
        let db = database();
        let repo = repo(&db);
        let id = create(&repo, "Indexed").id.expect("id");
        mark_clean(&repo, id, 9);

        let index = repo.task_index(ACCOUNT).expect("index");
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].task_uuid, reload(&repo, id).task_uuid);
        assert_eq!(index[0].imap_uid, Some(9));
        assert!(!index[0].is_dirty);
        assert!(!index[0].is_trashed);
    }

    #[test]
    fn a_device_id_is_generated_once_and_then_reused() {
        let db = database();
        let repo = repo(&db);
        let first = repo.ensure_device_id(ACCOUNT).expect("device id");
        let second = repo.ensure_device_id(ACCOUNT).expect("device id");
        assert_eq!(first, second);
        assert!(!first.is_empty());
        assert_eq!(
            repo.get_sync_state(ACCOUNT)
                .expect("state")
                .expect("exists")
                .device_id,
            Some(first),
        );
    }

    /// A mod-sequence out of SQLite's range must not be able to stop task sync.
    /// The field is written for diagnostics and never read back to decide
    /// anything (§3.3), but an unguarded `u64` makes rusqlite refuse the whole
    /// state write at runtime, and `sync()` treats that as fatal.
    #[test]
    fn an_out_of_range_modseq_does_not_break_the_sync_state() {
        let db = database();
        let repo = repo(&db);
        let mut state = TaskSyncState::new(ACCOUNT.to_string());
        state.uidvalidity = Some(42);
        state.highest_modseq = Some(u64::MAX);
        repo.upsert_sync_state(&state).expect("state must persist");

        let stored = repo.get_sync_state(ACCOUNT).expect("read").expect("exists");
        assert_eq!(
            stored.uidvalidity,
            Some(42),
            "the fields that matter survive"
        );
        assert_eq!(stored.highest_modseq, Some(i64::MAX as u64));

        // And an ordinary value is untouched.
        state.highest_modseq = Some(9_001);
        repo.upsert_sync_state(&state).expect("state");
        assert_eq!(
            repo.get_sync_state(ACCOUNT)
                .expect("read")
                .expect("exists")
                .highest_modseq,
            Some(9_001)
        );
    }

    #[test]
    fn a_position_slot_between_neighbours_in_the_wrong_order_is_refused() {
        let db = database();
        let repo = repo(&db);
        let moved = create(&repo, "Moved").id.expect("id");
        let second = create(&repo, "Second").id.expect("id");
        let first = create(&repo, "First").id.expect("id");

        let error = repo.move_task(moved, Some(second), Some(first), &ctx());
        assert!(
            error.is_err(),
            "a slot that is not a slot is a bug, not a position"
        );
    }

    #[test]
    fn counts_and_tags_reach_the_list_without_a_query_per_row() {
        let db = database();
        let repo = repo(&db);
        let tag = repo.create_tag("home", None).expect("tag");
        let id = create(&repo, "With everything").id.expect("id");
        repo.add_tag_to(&[id], &tag.tag_uuid, &ctx()).expect("tag");
        repo.link_email(
            id,
            "<m@example.com>",
            &CachedEmailMetadata::default(),
            &ctx(),
        )
        .expect("link");
        repo.add_reminder(id, "2026-08-06T09:00:00+00:00", &ctx())
            .expect("reminder");

        let items = repo.someday_tasks(day("2026-08-06")).expect("list");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].tags, vec![tag.tag_uuid]);
        assert_eq!(items[0].linked_email_count, 1);
        assert_eq!(items[0].reminder_count, 1);
        assert_eq!(items[0].attachment_count, 0);
    }

    #[test]
    fn open_task_counts_for_messages_skip_completed_and_trashed_tasks() {
        let db = database();
        let repo = repo(&db);
        let open = create(&repo, "Open").id.expect("id");
        let done = create(&repo, "Done").id.expect("id");
        for id in [open, done] {
            repo.link_email(
                id,
                "<thread@example.com>",
                &CachedEmailMetadata::default(),
                &ctx(),
            )
            .expect("link");
        }
        repo.complete_tasks(&[done], &ctx()).expect("complete");

        let counts = repo
            .open_task_counts_for_messages(&["<thread@example.com>".to_string()])
            .expect("counts");
        assert_eq!(counts.get("<thread@example.com>"), Some(&1));
        assert!(
            !counts.contains_key("<absent@example.com>"),
            "zero renders as nothing, not as a zero",
        );
    }
}
