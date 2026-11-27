//! Reminders (§5): the core-side half of OS-scheduled local notifications.
//!
//! The delivery mechanism is *not* an in-app poll. A reminder set today for a
//! date a year out has to fire on that date whether or not Orb is running and
//! whether or not the machine was awake when it was set (§14.2), and no loop
//! inside a process that is not running can do that. What can is
//! `UNUserNotificationCenter`: the notification is handed to the OS ahead of
//! time and the OS delivers it. Everything in this module exists to keep the
//! set of notifications the OS holds equal to the set of reminders the database
//! holds.
//!
//! **Reconciliation is a diff, not a rebuild.** Cancelling everything and
//! re-scheduling it on every sync would work exactly until a cancel succeeded
//! and the re-schedule did not, and it would re-arm notifications the OS had
//! already fired. Instead the desired set is computed
//! ([`ReminderService::desired_notifications`]), compared against the
//! identifiers the OS reports as pending ([`diff_notifications`]), and only the
//! difference is applied. The identifier is derived from the task's uuid and the
//! reminder's instant ([`TaskReminder::notification_id`]) rather than from a row
//! id, because the SQLite side is a cache that can be deleted and resynced (§3,
//! §14.5) and row ids do not survive that. Two consequences fall out and both
//! are load-bearing: scheduling the same reminder twice is idempotent, and
//! *moving* a reminder changes its identifier, so the old notification is
//! cancelled and the new one scheduled rather than the old one silently
//! surviving at the wrong time.
//!
//! **The window is capped.** macOS keeps at most
//! [`PENDING_NOTIFICATION_LIMIT`] pending local notification requests and
//! silently discards the rest, so the nearest N are scheduled and the window is
//! re-filled on every sync, on every launch and at local midnight (§5.1) — the
//! three moments at which the set can have changed without this process having
//! been told.
//!
//! **Firing state is local per machine.** `is_triggered` is deliberately not
//! part of the wire format (§5.2): every machine is supposed to notify its own
//! user, so a reminder fired on the laptop must still fire on the desktop.
//! Nothing in this module bumps a revision or sets the dirty flag, and the log
//! entries it writes ride along on the next upload the task makes for some other
//! reason. The contract in the other direction matters just as much: the
//! platform layer must report what the OS actually delivered
//! ([`ReminderService::record_fired`]), because that is the only thing standing
//! between the launch sweep and a duplicate notification for every reminder the
//! OS already showed.
//!
//! **A year of missed reminders is one notification.** The launch sweep (§5.2)
//! fires only what is less than [`MISSED_REMINDER_GRACE_HOURS`] hours overdue;
//! everything older is marked fired in silence, counted into one
//! `reminders_missed` log entry per task, and surfaced as a single
//! [`MissedSummary`].
//!
//! **The clock and the timezone are parameters.** Every entry point takes the
//! instant it is evaluated at, and the service is generic over the timezone, so
//! a test can pin both. A reminder is a *wall-clock* commitment — "09:00 on the
//! 15th" — so turning one into an instant and back has to go through real zone
//! rules rather than a fixed offset: an offset frozen at the moment the reminder
//! was created is an hour wrong on the other side of every DST transition, which
//! is exactly the class of bug §4.3 exists to prevent.

use std::collections::{HashMap, HashSet};
use std::fmt;

use chrono::{
    DateTime, Duration, Local, LocalResult, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc,
};
use serde::{Deserialize, Serialize};

use super::model::{ChangeContext, ReminderShift, TaskEvent, TaskLogEntry, TaskReminder};
use super::recurrence::ReminderOffset;
use super::repository::{LocalZone, TaskRepository};
use crate::Result;
use crate::db::Database;
use crate::logger;

/// How many notification requests the OS will hold pending for us.
///
/// macOS's limit is 64, and the 65th request is not rejected — it is accepted
/// and then quietly dropped, which is the worst possible failure mode for a
/// feature whose entire promise is "it will fire". The window is therefore
/// filled deliberately with the nearest reminders and re-filled whenever
/// something could have changed.
pub const PENDING_NOTIFICATION_LIMIT: usize = 64;

/// How overdue a reminder may be and still be worth firing on launch (§5.2).
pub const MISSED_REMINDER_GRACE_HOURS: i64 = 24;

/// The prefix every task reminder notification identifier carries.
///
/// Reconciliation cancels pending notifications that are no longer wanted, and
/// "no longer wanted" must not be allowed to mean "scheduled by some other part
/// of Orb". Only identifiers with this prefix are ever cancelled.
pub const NOTIFICATION_ID_PREFIX: &str = "orb.task.";

/// Is this identifier one of ours to cancel?
pub fn is_task_notification(identifier: &str) -> bool {
    identifier.starts_with(NOTIFICATION_ID_PREFIX)
}

/// Read a notification identifier back as the task and instant it names.
///
/// The inverse of [`TaskReminder::notification_id`], and the reason the
/// identifier is a structured string rather than an opaque one: everything the
/// OS hands back — a delivered notification on launch, a click, a snooze action
/// — is an identifier, never a row id, so without this there is no way to say
/// which reminder just fired. The uuid is unambiguous because a UUID contains no
/// `.`, which is exactly why the instant is appended last.
pub fn parse_notification_id(identifier: &str) -> Option<(&str, i64)> {
    let (task_uuid, unix) = identifier
        .strip_prefix(NOTIFICATION_ID_PREFIX)?
        .rsplit_once('.')?;
    if task_uuid.is_empty() {
        return None;
    }
    Some((task_uuid, unix.parse().ok()?))
}

/// The grace period of §5.2 as a duration.
/// The wall clock of an instant on this machine, as the reminders are read.
pub fn local_wall_clock(unix: i64) -> NaiveDateTime {
    DateTime::from_timestamp(unix, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
        .naive_local()
}

pub fn missed_reminder_grace() -> Duration {
    Duration::hours(MISSED_REMINDER_GRACE_HOURS)
}

// ---------------------------------------------------------------------------
// Timezone plumbing
// ---------------------------------------------------------------------------

/// Adapts any chrono timezone to the repository's [`LocalZone`].
///
/// The repository asks a zone two questions (which civil day is it, and where
/// does a civil day begin); this module needs the two inverse ones (what is the
/// wall clock of this instant, and which instant is this wall clock). Rather
/// than widen `LocalZone` — whose two operations are exactly what its queries
/// need — the service is generic over a real `chrono::TimeZone`, which answers
/// all four, and hands the repository this adapter so both halves agree about
/// what day it is.
#[derive(Debug, Clone, Copy)]
struct RepositoryZone<Tz>(Tz);

impl<Tz> LocalZone for RepositoryZone<Tz>
where
    Tz: TimeZone + Send + Sync + 'static,
{
    fn civil_date(&self, at: DateTime<Utc>) -> NaiveDate {
        at.with_timezone(&self.0).date_naive()
    }

    fn midnight(&self, day: NaiveDate) -> DateTime<Utc> {
        first_existing_instant(&self.0, day.and_hms_opt(0, 0, 0).unwrap_or_default())
    }
}

/// The instant a local wall-clock time falls at, for a wall clock that may not
/// exist or may exist twice.
///
/// Spring-forward deletes an hour of wall clock — including local midnight
/// itself in Brazil, Chile and Lebanon — and autumn repeats one. A reminder set
/// for a wall clock the calendar then skips must still fire: it is moved
/// forward to the first instant that does exist, because a reminder an hour late
/// is recoverable and a reminder that silently never fires is the failure this
/// whole design is built to avoid. A wall clock that happens twice fires the
/// first time, which is the one the user will read as "09:00".
fn first_existing_instant<Tz: TimeZone>(zone: &Tz, local: NaiveDateTime) -> DateTime<Utc> {
    for minutes in 0..(24 * 60) {
        let Some(candidate) = local.checked_add_signed(Duration::minutes(minutes)) else {
            break;
        };
        match zone.from_local_datetime(&candidate) {
            LocalResult::Single(at) => return at.with_timezone(&Utc),
            LocalResult::Ambiguous(earlier, _) => return earlier.with_timezone(&Utc),
            LocalResult::None => continue,
        }
    }
    // Unreachable for any real zone: a day in which no local time exists is not
    // a day. Interpreting the wall clock as UTC keeps the function total rather
    // than panicking somewhere on the notification path.
    Utc.from_utc_datetime(&local)
}

// ---------------------------------------------------------------------------
// The desired set of OS notifications
// ---------------------------------------------------------------------------

/// One notification the OS should be holding.
///
/// Times are carried as strings and a Unix timestamp rather than as chrono
/// types because this crosses the FFI boundary on its way to
/// `UNUserNotificationCenter`, and chrono's serde support is not compiled in.
/// `fire_at` keeps the local UTC offset, so the platform layer can read
/// calendar components straight out of it for a calendar trigger; `fire_at_unix`
/// is the ordering key and what a time-interval trigger is computed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ScheduledNotification {
    /// `orb.task.<task uuid>.<remind_at unix>` — see the module docs for why it
    /// is derived rather than stored.
    pub identifier: String,

    pub task_id: i64,

    /// What a click has to open (§5). The uuid rather than the row id, for the
    /// same reason the identifier is derived from it.
    pub task_uuid: String,

    /// The row this came from, so a delivery can be reported back with
    /// [`ReminderService::record_fired`]. `None` only for a row read before it
    /// was assigned an id, which the query path cannot produce.
    pub reminder_id: Option<i64>,

    /// The task's title, which is the notification's title.
    pub title: String,

    /// RFC3339 with the machine's local offset.
    pub fire_at: String,

    pub fire_at_unix: i64,
}

/// The nearest reminders that fit in the OS's pending window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationWindow {
    /// What to schedule, soonest first.
    pub scheduled: Vec<ScheduledNotification>,

    /// How many future reminders were eligible before the cap was applied.
    pub eligible: usize,
}

impl NotificationWindow {
    /// Did the cap bite? A caller that wants to warn, or to re-fill more
    /// eagerly than the next local midnight, needs to know.
    pub fn is_capped(&self) -> bool {
        self.eligible > self.scheduled.len()
    }
}

/// The nearest `limit` reminders that still lie in the future.
///
/// Past reminders never take a slot: the OS fires a request with a past trigger
/// immediately, which would turn every reconcile into a re-delivery of
/// everything already fired. Overdue reminders are the launch sweep's business
/// ([`ReminderService::catch_up_at`]), not the window's.
///
/// Duplicate identifiers are collapsed. Two reminders on one task at the same
/// instant are one identifier and therefore one notification — which is also
/// what the user meant.
pub fn notification_window(
    candidates: Vec<ScheduledNotification>,
    now: DateTime<Utc>,
    limit: usize,
) -> NotificationWindow {
    let now_unix = now.timestamp();
    let mut future: Vec<ScheduledNotification> = candidates
        .into_iter()
        .filter(|notification| notification.fire_at_unix > now_unix)
        .collect();

    // Sorted by instant, ties broken by identifier so that two machines — and
    // two runs on one machine — always choose the same members for a full
    // window.
    future.sort_by(|left, right| {
        left.fire_at_unix
            .cmp(&right.fire_at_unix)
            .then_with(|| left.identifier.cmp(&right.identifier))
    });

    let mut seen = HashSet::new();
    future.retain(|notification| seen.insert(notification.identifier.clone()));

    let eligible = future.len();
    future.truncate(limit);
    NotificationWindow {
        scheduled: future,
        eligible,
    }
}

/// What has to change for the OS's pending set to equal the desired set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationPlan {
    /// Requests to add.
    pub schedule: Vec<ScheduledNotification>,

    /// Identifiers to withdraw: reminders that were deleted, moved, fired, or
    /// whose task was completed or trashed.
    pub cancel: Vec<String>,

    /// Already pending and still wanted. Untouched — re-adding them would work,
    /// but a reconcile that rewrites everything cannot be told apart from one
    /// that found nothing to do, and this is the field that proves convergence.
    pub unchanged: Vec<String>,

    /// Whether more reminders were eligible than there were slots.
    pub capped: bool,
}

impl NotificationPlan {
    /// Is the OS already holding exactly the right set?
    pub fn is_noop(&self) -> bool {
        self.schedule.is_empty() && self.cancel.is_empty()
    }
}

/// Diff the OS's pending set against the desired one.
///
/// Pure, and the whole reason reconciliation converges: run it, apply it, run it
/// again and the second run is a no-op. Identifiers that are not ours are left
/// strictly alone.
pub fn diff_notifications(
    pending: &[String],
    desired: &[ScheduledNotification],
) -> NotificationPlan {
    let wanted: HashSet<&str> = desired
        .iter()
        .map(|notification| notification.identifier.as_str())
        .collect();
    let already: HashSet<&str> = pending.iter().map(String::as_str).collect();

    let mut cancel: Vec<String> = pending
        .iter()
        .filter(|identifier| is_task_notification(identifier))
        .filter(|identifier| !wanted.contains(identifier.as_str()))
        .cloned()
        .collect();
    cancel.sort();
    cancel.dedup();

    let (unchanged, schedule): (Vec<_>, Vec<_>) = desired
        .iter()
        .cloned()
        .partition(|notification| already.contains(notification.identifier.as_str()));

    NotificationPlan {
        schedule,
        cancel,
        unchanged: unchanged
            .into_iter()
            .map(|notification| notification.identifier)
            .collect(),
        capped: false,
    }
}

/// The platform's notification centre, as this module needs it.
///
/// Three operations, all of which `UNUserNotificationCenter` has directly.
/// Keeping it a trait is what lets the reconciliation loop — the part that has
/// to converge — be tested without a notification centre.
pub trait NotificationHost {
    /// The identifiers the OS is currently holding pending.
    fn pending_identifiers(&self) -> Result<Vec<String>>;

    /// Add a request. Adding one whose identifier is already pending replaces
    /// it, which is what makes a repeated schedule harmless.
    fn schedule(&self, notification: &ScheduledNotification) -> Result<()>;

    /// Withdraw pending requests.
    fn cancel(&self, identifiers: &[String]) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Reminders that have already come due
// ---------------------------------------------------------------------------

/// A reminder that is due now and has not fired on this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DueReminder {
    pub reminder: TaskReminder,
    pub task_id: i64,
    /// The task's identity, so a click can open it and so the delivered
    /// notification carries the same identifier a scheduled one would have.
    pub task_uuid: String,
    /// The task's title, for display.
    pub task_subject: String,
}

impl DueReminder {
    /// The identifier this would have been scheduled under.
    pub fn notification_id(&self) -> String {
        self.reminder.notification_id(&self.task_uuid)
    }

    /// How long ago it should have fired. Negative for a reminder that has not
    /// come due yet, which the query path does not produce.
    pub fn overdue_by(&self, now: DateTime<Utc>) -> Duration {
        Duration::seconds(now.timestamp() - self.reminder.remind_at_unix)
    }

    /// Too old to fire on its own (§5.2).
    pub fn is_stale(&self, now: DateTime<Utc>, grace: Duration) -> bool {
        self.overdue_by(now) >= grace
    }
}

/// The one notification that stands in for everything too old to fire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissedSummary {
    /// How many reminders were silenced.
    pub count: usize,
    /// Across how many tasks.
    pub tasks: usize,
    /// The oldest one's local time, so the summary can say how far back this
    /// goes rather than just how much of it there is.
    pub oldest: Option<String>,
}

impl MissedSummary {
    /// The identifier the summary is delivered under.
    ///
    /// Deliberately outside [`NOTIFICATION_ID_PREFIX`]: the summary is
    /// delivered immediately rather than scheduled, so it must never look to
    /// reconciliation like a stale task notification to cancel.
    pub const IDENTIFIER: &'static str = "orb.reminders.missed";

    /// The notification's body: where the missed reminders came from, since the
    /// title only says how many there were.
    pub fn body(&self) -> String {
        let tasks = if self.tasks == 1 {
            "1 task".to_string()
        } else {
            format!("{} tasks", self.tasks)
        };
        match &self.oldest {
            Some(oldest) => format!("Across {tasks}, oldest {oldest}"),
            None => format!("Across {tasks}"),
        }
    }
}

impl fmt::Display for MissedSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.count == 1 {
            write!(f, "1 missed reminder")
        } else {
            write!(f, "{} missed reminders", self.count)
        }
    }
}

/// Overdue reminders split by whether they are still worth firing.
#[derive(Debug, Default)]
pub struct MissedTriage {
    /// Recent enough to deliver (§5.2).
    pub deliver: Vec<DueReminder>,
    /// Older than the grace period: silenced, counted, summarised.
    pub stale: Vec<DueReminder>,
}

/// Split overdue reminders at the grace boundary.
///
/// Pure, because the boundary is the entire policy: a machine that has been
/// closed for a week must produce one notification rather than a week of them,
/// and a reminder from this morning must still arrive.
pub fn triage_missed(
    overdue: Vec<DueReminder>,
    now: DateTime<Utc>,
    grace: Duration,
) -> MissedTriage {
    let mut triage = MissedTriage::default();
    for reminder in overdue {
        if reminder.is_stale(now, grace) {
            triage.stale.push(reminder);
        } else {
            triage.deliver.push(reminder);
        }
    }
    triage
}

/// What the launch sweep decided.
#[derive(Debug, Default)]
pub struct ReminderCheckResult {
    /// Reminders to deliver now, soonest first.
    pub due_reminders: Vec<DueReminder>,

    /// The single summary standing in for everything older than the grace
    /// period, if there was any.
    pub missed: Option<MissedSummary>,

    /// How many reminders the sweep marked as fired on this machine. Includes
    /// the silenced ones — the point of marking them is that they never come
    /// back.
    pub triggered_count: usize,
}

impl ReminderCheckResult {
    pub fn new() -> Self {
        Self::default()
    }

    /// Nothing to show the user.
    pub fn is_empty(&self) -> bool {
        self.due_reminders.is_empty() && self.missed.is_none()
    }

    /// Hand everything to a notifier, in the order the user should see it.
    pub fn notify(&self, notifier: &dyn ReminderNotifier) {
        for reminder in &self.due_reminders {
            notifier.notify(reminder);
        }
        if let Some(summary) = &self.missed {
            notifier.notify_missed(summary);
        }
    }
}

/// Delivery of a notification that is due *now*, as opposed to one scheduled
/// with the OS for later.
pub trait ReminderNotifier: Send + Sync {
    /// A reminder that came due while this machine could not be told by the OS.
    fn notify(&self, reminder: &DueReminder);

    /// The one summary for everything too old to fire (§5.2).
    fn notify_missed(&self, summary: &MissedSummary);
}

/// Logs to stderr. For debugging and for a headless build.
#[derive(Debug, Default)]
pub struct StderrNotifier;

impl ReminderNotifier for StderrNotifier {
    fn notify(&self, reminder: &DueReminder) {
        eprintln!(
            "[REMINDER] {} (due: {})",
            reminder.task_subject, reminder.reminder.remind_at
        );
    }

    fn notify_missed(&self, summary: &MissedSummary) {
        eprintln!("[REMINDER] {summary} — {}", summary.body());
    }
}

/// Collects instead of delivering, for tests.
#[derive(Debug, Default)]
pub struct CollectingNotifier {
    notifications: std::sync::Mutex<Vec<DueReminder>>,
    summaries: std::sync::Mutex<Vec<MissedSummary>>,
}

impl CollectingNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_notifications(&self) -> Vec<DueReminder> {
        let mut guard = self.notifications.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    pub fn take_summaries(&self) -> Vec<MissedSummary> {
        let mut guard = self.summaries.lock().unwrap();
        std::mem::take(&mut *guard)
    }
}

impl ReminderNotifier for CollectingNotifier {
    fn notify(&self, reminder: &DueReminder) {
        self.notifications.lock().unwrap().push(reminder.clone());
    }

    fn notify_missed(&self, summary: &MissedSummary) {
        self.summaries.lock().unwrap().push(summary.clone());
    }
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// Reminder scheduling and firing.
///
/// Generic over the timezone so that a test can pin one: every question this
/// answers is about a wall clock, and a wall clock is a function of the zone.
/// The default is the machine's own zone; tests use a fixed offset, or a zone
/// with a DST rule when that is what is under test.
pub struct ReminderService<'a, Tz = Local> {
    db: &'a Database,
    zone: Tz,
}

impl<'a> ReminderService<'a, Local> {
    /// A service that works in the machine's own timezone.
    pub fn new(db: &'a Database) -> Self {
        Self { db, zone: Local }
    }
}

impl<'a, Tz> ReminderService<'a, Tz>
where
    Tz: TimeZone + Clone + Send + Sync + 'static,
    Tz::Offset: fmt::Display,
{
    /// A service pinned to a given timezone.
    pub fn with_zone(db: &'a Database, zone: Tz) -> Self {
        Self { db, zone }
    }

    /// A repository that agrees with this service about what day it is.
    fn repository(&self) -> TaskRepository<'a> {
        TaskRepository::with_zone(self.db, RepositoryZone(self.zone.clone()))
    }

    /// When the window has to be re-filled if nothing else happens first
    /// (§5.1): the next local midnight, which is also when every date-dependent
    /// view is re-evaluated (§4.3).
    pub fn next_refill_at(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        self.repository().next_local_midnight(now)
    }

    // -- reconciliation ----------------------------------------------------

    /// The notifications the OS should be holding at `now`.
    ///
    /// Reminders belonging to completed or trashed tasks are excluded by the
    /// query, and reminders this machine has already fired are excluded here —
    /// which together are most of what reconciliation cancels. Twice the window
    /// is fetched so that the filtering cannot leave the window short of slots
    /// it could have filled.
    pub fn desired_notifications(&self, now: DateTime<Utc>) -> Result<NotificationWindow> {
        let repo = self.repository();
        let candidates = repo.upcoming_reminders(
            now.timestamp(),
            PENDING_NOTIFICATION_LIMIT.saturating_mul(2),
        )?;

        let notifications = candidates
            .into_iter()
            .filter(|(reminder, _, _)| !reminder.is_triggered)
            .map(|(reminder, task_uuid, subject)| {
                self.notification_for(&reminder, &task_uuid, &subject)
            })
            .collect();

        Ok(notification_window(
            notifications,
            now,
            PENDING_NOTIFICATION_LIMIT,
        ))
    }

    /// What to cancel and what to schedule, given what the OS says it is
    /// holding.
    pub fn reconcile_at(&self, pending: &[String], now: DateTime<Utc>) -> Result<NotificationPlan> {
        let window = self.desired_notifications(now)?;
        let mut plan = diff_notifications(pending, &window.scheduled);
        plan.capped = window.is_capped();
        Ok(plan)
    }

    /// [`ReminderService::reconcile_at`] against the wall clock.
    pub fn reconcile(&self, pending: &[String]) -> Result<NotificationPlan> {
        self.reconcile_at(pending, Utc::now())
    }

    /// Reconcile and apply, in the one order that is safe to interrupt.
    ///
    /// Cancels first: a notification withdrawn and not replaced is a reminder
    /// that fails to fire once and is picked up by the next reconcile, whereas
    /// scheduling before cancelling would leave a moved reminder armed at both
    /// times and fire the wrong one for real.
    pub fn apply<H: NotificationHost + ?Sized>(
        &self,
        host: &H,
        now: DateTime<Utc>,
    ) -> Result<NotificationPlan> {
        let pending = host.pending_identifiers()?;
        let plan = self.reconcile_at(&pending, now)?;
        if !plan.cancel.is_empty() {
            host.cancel(&plan.cancel)?;
        }
        for notification in &plan.schedule {
            host.schedule(notification)?;
        }
        Ok(plan)
    }

    // -- firing ------------------------------------------------------------

    /// The launch sweep of §5.2.
    ///
    /// Everything overdue and unfired is marked fired here, before anything is
    /// delivered, so that a second sweep — a crash and a relaunch, two windows
    /// racing — cannot deliver any of it twice. The cost of that ordering is
    /// that a notification lost between this call and the notifier is lost for
    /// good; the alternative is a reminder that arrives every time Orb starts,
    /// which is worse and is unbounded.
    ///
    /// Reminders the OS already delivered are not in this set, provided the
    /// platform layer reported them with [`ReminderService::record_fired`].
    pub fn catch_up_at(
        &self,
        now: DateTime<Utc>,
        device_id: Option<&str>,
    ) -> Result<ReminderCheckResult> {
        let repo = self.repository();
        let overdue = repo.get_pending_reminders(now.timestamp())?;
        if overdue.is_empty() {
            return Ok(ReminderCheckResult::new());
        }

        let mut uuids: HashMap<i64, Option<String>> = HashMap::new();
        let mut due = Vec::with_capacity(overdue.len());
        for (reminder, task_subject) in overdue {
            let task_id = reminder.task_id;
            let uuid = match uuids.get(&task_id) {
                Some(uuid) => uuid.clone(),
                None => {
                    let uuid = repo.get_task(task_id)?.map(|task| task.task_uuid);
                    uuids.insert(task_id, uuid.clone());
                    uuid
                }
            };
            let Some(task_uuid) = uuid else {
                // The join says the row exists, so this is a cache being
                // rebuilt underneath us. Skipping leaves the reminder unfired
                // and untouched, which the next sweep will pick up.
                logger::warn(&format!(
                    "task reminder {:?} has no task row {task_id}; skipping",
                    reminder.id
                ));
                continue;
            };
            due.push(DueReminder {
                reminder,
                task_id,
                task_uuid,
                task_subject,
            });
        }

        let triage = triage_missed(due, now, missed_reminder_grace());
        let ctx = repo.context_at(now, device_id);
        let summary = self.summarise(&triage.stale);
        let triggered_count = self.record_sweep(&repo, &triage, &ctx)?;

        Ok(ReminderCheckResult {
            due_reminders: triage.deliver,
            missed: summary,
            triggered_count,
        })
    }

    /// [`ReminderService::catch_up_at`] against the wall clock.
    pub fn catch_up(&self, device_id: Option<&str>) -> Result<ReminderCheckResult> {
        self.catch_up_at(Utc::now(), device_id)
    }

    /// Record that the OS delivered a notification.
    ///
    /// This is what keeps the launch sweep from re-delivering everything the OS
    /// showed while Orb was closed, so the platform layer has to call it for
    /// every delivered notification it learns about — from the delegate while
    /// running, and from the delivered-notification list on launch.
    ///
    /// Marks fired and writes the log entry; bumps no revision and marks nothing
    /// dirty, because firing state is local per machine (§5.2). The entry
    /// travels on the next upload the task makes for a reason of its own.
    pub fn record_fired(
        &self,
        reminder_id: i64,
        now: DateTime<Utc>,
        device_id: Option<&str>,
    ) -> Result<bool> {
        let repo = self.repository();
        let Some(reminder) = repo.get_reminder(reminder_id)? else {
            return Ok(false);
        };
        if reminder.is_triggered {
            return Ok(false);
        }
        let ctx = repo.context_at(now, device_id);
        repo.mark_reminder_triggered(reminder_id)?;
        repo.append_log(
            reminder.task_id,
            &[TaskLogEntry::new(
                TaskEvent::ReminderFired {
                    remind_at: reminder.remind_at.clone(),
                },
                &ctx,
            )],
        )?;
        Ok(true)
    }

    /// [`ReminderService::record_fired`] for a notification the OS names by its
    /// identifier, which is the only name it knows.
    ///
    /// Resolving it through the task's uuid and the reminder's instant rather
    /// than through a row id is what makes the report survive the cache being
    /// deleted and resynced (§14.5): the identifier means the same thing before
    /// and after, and a row id does not. An identifier that resolves to nothing
    /// — a reminder deleted while its notification was pending — is not an
    /// error, it is simply nothing to record.
    pub fn record_delivered(
        &self,
        identifier: &str,
        now: DateTime<Utc>,
        device_id: Option<&str>,
    ) -> Result<bool> {
        let Some((task_uuid, remind_at_unix)) = parse_notification_id(identifier) else {
            return Ok(false);
        };
        let repo = self.repository();
        let Some(task_id) = repo.get_task_by_uuid(task_uuid)?.and_then(|task| task.id) else {
            return Ok(false);
        };
        let fired = repo
            .get_reminders(task_id)?
            .into_iter()
            .find(|reminder| reminder.remind_at_unix == remind_at_unix)
            .and_then(|reminder| reminder.id);
        match fired {
            Some(reminder_id) => self.record_fired(reminder_id, now, device_id),
            None => Ok(false),
        }
    }

    /// Push a reminder out and re-arm it, as a notification action does.
    ///
    /// The original is marked fired rather than moved: it *did* fire, and the
    /// snooze is a new commitment. The new one is picked up by the next
    /// reconcile.
    pub fn snooze_reminder(
        &self,
        reminder_id: i64,
        minutes: i64,
        now: DateTime<Utc>,
        device_id: Option<&str>,
    ) -> Result<i64> {
        let repo = self.repository();
        let Some(original) = repo.get_reminder(reminder_id)? else {
            return Err(crate::Error::NotFound(format!("Reminder {reminder_id}")));
        };
        self.record_fired(reminder_id, now, device_id)?;

        let at = now + Duration::minutes(minutes);
        let ctx = repo.context_at(now, device_id);
        repo.insert_reminder(
            &TaskReminder::new(original.task_id, self.format_local(at), at.timestamp()),
            &ctx,
        )
    }

    /// Discard fired reminders older than `max_age_days`.
    ///
    /// Only fired ones, and only old ones: the table is also the record of what
    /// a task's reminders *are*, and a reminder deleted here is a reminder that
    /// never fires again on any occurrence of a recurring task.
    pub fn cleanup_old_reminders(&self, max_age_days: i64, now: DateTime<Utc>) -> Result<usize> {
        self.repository()
            .delete_old_triggered_reminders((now - Duration::days(max_age_days)).timestamp())
    }

    // -- recurrence --------------------------------------------------------

    /// Drag a recurring task's reminders to the day its occurrence is on now
    /// (§8, §32).
    ///
    /// The shifts come from whatever moved that day — a completion, a postpone
    /// date, an undo of either — as `MutationOutcome::reminder_shifts`, because
    /// the mutation path owns the task row and this module owns the reminder
    /// rows. Each reminder keeps its distance in whole days and its wall-clock
    /// time, so "the evening before" stays the evening before; the arithmetic
    /// is [`ReminderOffset`]'s, on civil dates, and only the conversion back to
    /// an instant consults the zone — which is what keeps a shift across a DST
    /// boundary from sliding every reminder by an hour.
    ///
    /// The moved reminders are un-fired. They have to be: a reminder marked
    /// fired for last month's occurrence would sit in the table at its new time
    /// and never be scheduled, and a recurring task whose reminders stop after
    /// the first occurrence is not a recurring reminder.
    pub fn follow_recurrence(
        &self,
        shifts: &[(i64, ReminderShift)],
        ctx: &ChangeContext<'_>,
    ) -> Result<usize> {
        let mut moved = 0;
        for (task_id, shift) in shifts {
            moved += self.follow_task(*task_id, *shift, ctx)?;
        }
        Ok(moved)
    }

    /// [`ReminderService::follow_recurrence`] for one task.
    ///
    /// The reminders of the occurrence on `shift.to` are what the series says
    /// they are (§32). A series that does not say yet takes them from where
    /// they stand against `shift.from`, and says so from then on, so that a
    /// reminder that fired and was cleared away, or one a snooze added, does
    /// not change what the next occurrence gets.
    ///
    /// The moved set is published: reminders travel inside the task message, so
    /// a machine that never completes this occurrence has to learn the new times
    /// from the wire rather than recompute them — it does not know the rule was
    /// applied, only what the message says.
    pub fn follow_task(
        &self,
        task_id: i64,
        shift: ReminderShift,
        ctx: &ChangeContext<'_>,
    ) -> Result<usize> {
        if shift.from == shift.to {
            return Ok(0);
        }
        let repo = self.repository();
        let reminders = repo.get_reminders(task_id)?;
        let mut series = match repo.get_task(task_id)? {
            Some(task) => task.series()?,
            None => return Ok(0),
        };
        let stated = series.as_ref().and_then(|series| series.reminders.clone());
        if reminders.is_empty() && stated.is_none() {
            return Ok(0);
        }

        let offsets = stated
            .clone()
            .unwrap_or_else(|| self.offsets(&reminders, shift.from));
        let mut wanted: Vec<NaiveDateTime> = offsets
            .iter()
            .filter_map(|offset| offset.apply(shift.to))
            .collect();
        wanted.sort();
        wanted.dedup();

        // A reminder that stays where it is keeps its row, and with it whether
        // it has fired here.
        let mut moved = 0;
        let mut next = Vec::with_capacity(wanted.len());
        for local in wanted {
            let at = self.instant_of(local);
            match reminders
                .iter()
                .find(|r| r.remind_at_unix == at.timestamp())
            {
                Some(kept) => next.push(kept.clone()),
                None => {
                    next.push(TaskReminder::new(
                        task_id,
                        self.format_local(at),
                        at.timestamp(),
                    ));
                    moved += 1;
                }
            }
        }
        let states = stated.is_none() && series.is_some();
        if moved == 0 && next.len() == reminders.len() && !states {
            return Ok(0);
        }
        match &mut series {
            Some(series) => {
                series.reminders = Some(offsets);
                repo.replace_reminders_in_series(task_id, &next, Some(&series.to_string()), ctx)?;
            }
            None => {
                repo.replace_reminders(task_id, &next, ctx)?;
            }
        }
        Ok(moved)
    }

    /// Add a reminder to a task. On a recurring task it is one of the series
    /// (§32): every occurrence gets it, as far from its day as this one is.
    pub fn add_reminder(
        &self,
        task_id: i64,
        remind_at: &str,
        ctx: &ChangeContext<'_>,
    ) -> Result<i64> {
        let repo = self.repository();
        let series = match repo.get_task(task_id)? {
            Some(task) => task
                .series_in_effect()?
                .map(|series| (series.occurrence_day(task.dates().into()), series)),
            None => None,
        };
        let Some((day, mut series)) = series else {
            return repo.add_reminder(task_id, remind_at, ctx);
        };
        let at = DateTime::parse_from_rfc3339(remind_at)
            .map_err(|e| crate::Error::InvalidInput(format!("'{remind_at}': {e}")))?;
        let mut offsets = match series.reminders.take() {
            Some(offsets) => offsets,
            None => self.offsets(&repo.get_reminders(task_id)?, day),
        };
        let added = ReminderOffset::between(day, self.wall_clock(at.timestamp()));
        if !offsets.contains(&added) {
            offsets.push(added);
        }
        series.reminders = Some(offsets);
        repo.add_reminder_in_series(task_id, remind_at, &series.to_string(), ctx)
    }

    /// Remove a reminder, from the series as well when its task repeats.
    pub fn delete_reminder(&self, reminder_id: i64, ctx: &ChangeContext<'_>) -> Result<bool> {
        let repo = self.repository();
        let Some(reminder) = repo.get_reminder(reminder_id)? else {
            return Ok(false);
        };
        let series = match repo.get_task(reminder.task_id)? {
            Some(task) => task
                .series_in_effect()?
                .map(|series| (series.occurrence_day(task.dates().into()), series)),
            None => None,
        };
        let Some((day, mut series)) = series else {
            return repo.delete_reminder(reminder_id, ctx);
        };
        let mut offsets = match series.reminders.take() {
            Some(offsets) => offsets,
            None => self.offsets(&repo.get_reminders(reminder.task_id)?, day),
        };
        let removed = ReminderOffset::between(day, self.wall_clock(reminder.remind_at_unix));
        if let Some(at) = offsets.iter().position(|offset| *offset == removed) {
            offsets.remove(at);
        }
        series.reminders = Some(offsets);
        repo.delete_reminder_in_series(reminder_id, &series.to_string(), ctx)
    }

    /// Where a task's reminders stand against a day.
    fn offsets(&self, reminders: &[TaskReminder], day: NaiveDate) -> Vec<ReminderOffset> {
        let mut offsets: Vec<ReminderOffset> = Vec::new();
        for reminder in reminders {
            let offset = ReminderOffset::between(day, self.wall_clock(reminder.remind_at_unix));
            if !offsets.contains(&offset) {
                offsets.push(offset);
            }
        }
        offsets
    }

    // -- plumbing ----------------------------------------------------------

    /// Mark the whole sweep fired and write its log entries, as one unit.
    ///
    /// One `reminders_missed` entry per task rather than one per reminder: the
    /// log is a timeline a human reads, and a year of missed reminders is one
    /// thing that happened to the task, not three hundred (§5.2).
    fn record_sweep(
        &self,
        repo: &TaskRepository<'_>,
        triage: &MissedTriage,
        ctx: &ChangeContext<'_>,
    ) -> Result<usize> {
        let tx = self.transaction()?;
        let mut triggered = 0;

        for due in &triage.deliver {
            if let Some(id) = due.reminder.id {
                repo.mark_reminder_triggered(id)?;
                repo.append_log(
                    due.task_id,
                    &[TaskLogEntry::new(
                        TaskEvent::ReminderFired {
                            remind_at: due.reminder.remind_at.clone(),
                        },
                        ctx,
                    )],
                )?;
                triggered += 1;
            }
        }

        let mut per_task: Vec<(i64, u32)> = Vec::new();
        for due in &triage.stale {
            if let Some(id) = due.reminder.id {
                repo.mark_reminder_triggered(id)?;
                triggered += 1;
            }
            match per_task.iter_mut().find(|(id, _)| *id == due.task_id) {
                Some((_, count)) => *count = count.saturating_add(1),
                None => per_task.push((due.task_id, 1)),
            }
        }
        for (task_id, count) in per_task {
            repo.append_log(
                task_id,
                &[TaskLogEntry::new(TaskEvent::RemindersMissed { count }, ctx)],
            )?;
        }

        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(triggered)
    }

    /// One summary for the whole silenced set, or none if nothing was silenced.
    fn summarise(&self, stale: &[DueReminder]) -> Option<MissedSummary> {
        if stale.is_empty() {
            return None;
        }
        let mut tasks: Vec<i64> = stale.iter().map(|due| due.task_id).collect();
        tasks.sort_unstable();
        tasks.dedup();
        let oldest = stale
            .iter()
            .map(|due| due.reminder.remind_at_unix)
            .min()
            .map(|unix| self.format_local(DateTime::from_timestamp(unix, 0).unwrap_or_default()));
        Some(MissedSummary {
            count: stale.len(),
            tasks: tasks.len(),
            oldest,
        })
    }

    /// An `IMMEDIATE` transaction, or `None` when the caller already opened one.
    ///
    /// Mirrors the repository's rule: the sweep marks rows and writes log
    /// entries and those are one fact, but a caller that has already wrapped a
    /// larger group must not have it split, and SQLite rejects a nested `BEGIN`.
    fn transaction(&self) -> Result<Option<rusqlite::Transaction<'_>>> {
        if self.db.conn.is_autocommit() {
            Ok(Some(self.db.immediate_transaction()?))
        } else {
            Ok(None)
        }
    }

    fn notification_for(
        &self,
        reminder: &TaskReminder,
        task_uuid: &str,
        subject: &str,
    ) -> ScheduledNotification {
        // An unrepresentable timestamp degrades to the epoch, which the window
        // then filters out as long past: a corrupt row costs one notification
        // rather than the whole reconcile.
        let at = DateTime::from_timestamp(reminder.remind_at_unix, 0).unwrap_or_default();
        ScheduledNotification {
            identifier: reminder.notification_id(task_uuid),
            task_id: reminder.task_id,
            task_uuid: task_uuid.to_string(),
            reminder_id: reminder.id,
            title: subject.to_string(),
            fire_at: self.format_local(at),
            fire_at_unix: reminder.remind_at_unix,
        }
    }

    /// The wall clock this machine reads an instant as.
    fn wall_clock(&self, unix: i64) -> NaiveDateTime {
        DateTime::from_timestamp(unix, 0)
            .unwrap_or_default()
            .with_timezone(&self.zone)
            .naive_local()
    }

    /// The instant a wall clock falls at, DST holes and repeats included.
    fn instant_of(&self, local: NaiveDateTime) -> DateTime<Utc> {
        first_existing_instant(&self.zone, local)
    }

    /// An instant as local RFC3339 — the form `task_reminders.remind_at` holds
    /// and the form the notification layer reads calendar components from.
    fn format_local(&self, at: DateTime<Utc>) -> String {
        at.with_timezone(&self.zone)
            .to_rfc3339_opts(SecondsFormat::Secs, false)
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{Task, TaskUpdates};
    use super::*;
    use chrono::FixedOffset;
    use std::sync::Mutex;

    const ACCOUNT: &str = "Work";

    // -- a zone with a DST rule, since chrono-tz is not a dependency ---------

    /// Central European time, with the 2026 and 2027 transitions.
    ///
    /// A fixed offset cannot express the thing under test: a reminder is a wall
    /// clock commitment, and the whole question is whether moving one across a
    /// transition keeps the wall clock or slides it by an hour.
    #[derive(Debug, Clone, Copy)]
    struct Cet;

    impl Cet {
        /// The UTC instants at which summer time starts and ends.
        const SUMMER: [(i64, i64); 2] = [
            // 2026-03-29T01:00Z .. 2026-10-25T01:00Z
            (1_774_746_000, 1_792_890_000),
            // 2027-03-28T01:00Z .. 2027-10-31T01:00Z
            (1_806_195_600, 1_824_944_400),
        ];

        fn winter() -> FixedOffset {
            FixedOffset::east_opt(3600).expect("offset")
        }

        fn summer() -> FixedOffset {
            FixedOffset::east_opt(7200).expect("offset")
        }

        fn is_summer(utc: i64) -> bool {
            Self::SUMMER
                .iter()
                .any(|(start, end)| utc >= *start && utc < *end)
        }
    }

    impl TimeZone for Cet {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Self {
            Cet
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).expect("midnight"))
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<FixedOffset> {
            // Summer first, because in the repeated autumn hour the summer
            // reading is the earlier instant.
            let valid: Vec<FixedOffset> = [Self::summer(), Self::winter()]
                .into_iter()
                .filter(|offset| {
                    let utc = local.and_utc().timestamp() - i64::from(offset.local_minus_utc());
                    Self::is_summer(utc) == (*offset == Self::summer())
                })
                .collect();
            match valid.as_slice() {
                [] => LocalResult::None,
                [only] => LocalResult::Single(*only),
                [earlier, later] => LocalResult::Ambiguous(*earlier, *later),
                _ => unreachable!("two candidate offsets"),
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).expect("midnight"))
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            if Self::is_summer(utc.and_utc().timestamp()) {
                Self::summer()
            } else {
                Self::winter()
            }
        }
    }

    // -- fixtures -----------------------------------------------------------

    fn database() -> Database {
        let db = Database::new(":memory:").expect("open database");
        db.init_schema().expect("init schema");
        db
    }

    /// The reference zone for tests that are not about DST: two hours east, far
    /// enough from UTC that a UTC-based answer is visibly the wrong one.
    fn zone() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).expect("offset")
    }

    fn service(db: &Database) -> ReminderService<'_, FixedOffset> {
        ReminderService::with_zone(db, zone())
    }

    fn repo(db: &Database) -> TaskRepository<'_> {
        TaskRepository::with_zone(db, RepositoryZone(zone()))
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("instant")
            .with_timezone(&Utc)
    }

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("date")
    }

    fn ctx(now: &str) -> ChangeContext<'static> {
        ChangeContext::new(at(now), at(now).with_timezone(&zone()).date_naive())
            .with_device_id(Some("device-a"))
    }

    fn create(db: &Database, subject: &str) -> Task {
        repo(db)
            .create_task(subject, ACCOUNT, &ctx("2026-08-06T10:00:00+00:00"))
            .expect("create task")
    }

    /// A task with one reminder at the given instant, returning both ids.
    fn task_with_reminder(db: &Database, subject: &str, remind_at: &str) -> (Task, i64) {
        let task = create(db, subject);
        let id = task.id.expect("task id");
        let reminder_id = repo(db)
            .add_reminder(id, remind_at, &ctx("2026-08-06T10:00:00+00:00"))
            .expect("add reminder");
        (task, reminder_id)
    }

    fn notification(uuid: &str, unix: i64) -> ScheduledNotification {
        ScheduledNotification {
            identifier: format!("orb.task.{uuid}.{unix}"),
            task_id: 1,
            task_uuid: uuid.to_string(),
            reminder_id: Some(1),
            title: "Task".into(),
            fire_at: "2026-08-06T09:00:00+02:00".into(),
            fire_at_unix: unix,
        }
    }

    fn identifiers(notifications: &[ScheduledNotification]) -> Vec<String> {
        notifications
            .iter()
            .map(|notification| notification.identifier.clone())
            .collect()
    }

    /// A notification centre that behaves the way `UNUserNotificationCenter`
    /// does: adding an identifier that is already pending replaces it.
    #[derive(Debug, Default)]
    struct RecordingHost {
        pending: Mutex<Vec<String>>,
        scheduled: Mutex<usize>,
        cancelled: Mutex<usize>,
    }

    impl NotificationHost for RecordingHost {
        fn pending_identifiers(&self) -> Result<Vec<String>> {
            Ok(self.pending.lock().unwrap().clone())
        }

        fn schedule(&self, notification: &ScheduledNotification) -> Result<()> {
            let mut pending = self.pending.lock().unwrap();
            if !pending.contains(&notification.identifier) {
                pending.push(notification.identifier.clone());
            }
            *self.scheduled.lock().unwrap() += 1;
            Ok(())
        }

        fn cancel(&self, identifiers: &[String]) -> Result<()> {
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|identifier| !identifiers.contains(identifier));
            *self.cancelled.lock().unwrap() += identifiers.len();
            Ok(())
        }
    }

    // -- identifiers --------------------------------------------------------

    #[test]
    fn the_identifier_prefix_matches_what_the_model_mints() {
        let reminder = TaskReminder::new(1, "2026-08-06T09:00:00+02:00".into(), 1_785_913_200);
        assert!(is_task_notification(&reminder.notification_id("uuid")));
    }

    #[test]
    fn an_identifier_reads_back_as_the_reminder_it_names() {
        let reminder = TaskReminder::new(1, "2026-08-06T09:00:00+02:00".into(), 1_785_913_200);
        let identifier = reminder.notification_id("9f1c-uuid");
        assert_eq!(
            parse_notification_id(&identifier),
            Some(("9f1c-uuid", 1_785_913_200))
        );
    }

    #[test]
    fn an_identifier_that_is_not_ours_reads_back_as_nothing() {
        assert_eq!(parse_notification_id("orb.mail.new-message.7"), None);
        assert_eq!(parse_notification_id(MissedSummary::IDENTIFIER), None);
        assert_eq!(parse_notification_id("orb.task..17"), None);
        assert_eq!(parse_notification_id("orb.task.uuid.later"), None);
    }

    #[test]
    fn the_missed_summary_is_never_mistaken_for_a_scheduled_reminder() {
        // It is delivered rather than scheduled, so reconciliation must never
        // see it as a stale identifier of ours to cancel.
        assert!(!is_task_notification(MissedSummary::IDENTIFIER));
    }

    // -- the diff -----------------------------------------------------------

    #[test]
    fn a_new_reminder_is_scheduled_and_nothing_else_is_touched() {
        let desired = vec![notification("a", 100), notification("b", 200)];
        let plan = diff_notifications(&identifiers(&desired[..1]), &desired);
        assert_eq!(identifiers(&plan.schedule), vec!["orb.task.b.200"]);
        assert_eq!(plan.unchanged, vec!["orb.task.a.100"]);
        assert!(plan.cancel.is_empty());
    }

    #[test]
    fn a_deleted_reminder_is_cancelled() {
        let pending = vec!["orb.task.a.100".to_string(), "orb.task.b.200".to_string()];
        let plan = diff_notifications(&pending, &[notification("a", 100)]);
        assert_eq!(plan.cancel, vec!["orb.task.b.200"]);
        assert!(plan.schedule.is_empty());
    }

    #[test]
    fn moving_a_reminder_cancels_the_old_time_and_schedules_the_new_one() {
        // The identifier carries the instant precisely so that a move is a
        // cancel plus a schedule rather than a stale notification that survives
        // at the wrong time.
        let plan = diff_notifications(&["orb.task.a.100".to_string()], &[notification("a", 200)]);
        assert_eq!(plan.cancel, vec!["orb.task.a.100"]);
        assert_eq!(identifiers(&plan.schedule), vec!["orb.task.a.200"]);
    }

    #[test]
    fn applying_a_plan_twice_converges() {
        let desired = vec![notification("a", 100), notification("b", 200)];
        let first = diff_notifications(&[], &desired);
        assert_eq!(first.schedule.len(), 2);

        // Whatever the first run scheduled is now pending.
        let second = diff_notifications(&identifiers(&first.schedule), &desired);
        assert!(second.is_noop(), "{second:?}");
        assert_eq!(second.unchanged.len(), 2);
    }

    #[test]
    fn a_notification_belonging_to_something_else_is_left_alone() {
        let pending = vec![
            "orb.mail.new-message.7".to_string(),
            MissedSummary::IDENTIFIER.to_string(),
            "orb.task.a.100".to_string(),
        ];
        let plan = diff_notifications(&pending, &[]);
        assert_eq!(plan.cancel, vec!["orb.task.a.100"]);
    }

    // -- the window ---------------------------------------------------------

    #[test]
    fn the_os_limit_is_the_documented_sixty_four() {
        assert_eq!(PENDING_NOTIFICATION_LIMIT, 64);
    }

    #[test]
    fn the_window_holds_the_nearest_reminders_and_reports_the_cap() {
        let now = at("2026-08-06T00:00:00Z");
        let candidates: Vec<ScheduledNotification> = (1..=10)
            .map(|index| notification(&format!("t{index}"), now.timestamp() + index * 60))
            .collect();

        let window = notification_window(candidates, now, 3);
        assert_eq!(
            identifiers(&window.scheduled),
            identifiers(&[
                notification("t1", now.timestamp() + 60),
                notification("t2", now.timestamp() + 120),
                notification("t3", now.timestamp() + 180),
            ]),
        );
        assert_eq!(window.eligible, 10);
        assert!(window.is_capped());
    }

    #[test]
    fn a_window_with_room_to_spare_is_not_capped() {
        let now = at("2026-08-06T00:00:00Z");
        let window = notification_window(vec![notification("a", now.timestamp() + 60)], now, 64);
        assert_eq!(window.scheduled.len(), 1);
        assert!(!window.is_capped());
    }

    #[test]
    fn a_reminder_in_the_past_never_takes_a_slot() {
        // The OS delivers a request with a past trigger immediately, so leaving
        // one in the window would re-deliver it on every reconcile.
        let now = at("2026-08-06T00:00:00Z");
        let window = notification_window(
            vec![
                notification("past", now.timestamp() - 1),
                notification("now", now.timestamp()),
                notification("future", now.timestamp() + 1),
            ],
            now,
            64,
        );
        assert_eq!(
            identifiers(&window.scheduled),
            identifiers(&[notification("future", now.timestamp() + 1)]),
        );
    }

    #[test]
    fn two_reminders_at_the_same_instant_on_one_task_are_one_notification() {
        let now = at("2026-08-06T00:00:00Z");
        let window = notification_window(
            vec![
                notification("a", now.timestamp() + 60),
                notification("a", now.timestamp() + 60),
            ],
            now,
            64,
        );
        assert_eq!(window.scheduled.len(), 1);
        assert_eq!(window.eligible, 1);
    }

    // -- reconciliation against the database --------------------------------

    #[test]
    fn a_reminder_in_the_database_becomes_a_scheduled_notification() {
        let db = database();
        let (task, reminder_id) =
            task_with_reminder(&db, "Call the dentist", "2026-08-07T09:00:00+02:00");
        let now = at("2026-08-06T10:00:00Z");

        let plan = service(&db).reconcile_at(&[], now).expect("reconcile");
        assert_eq!(plan.schedule.len(), 1);
        let scheduled = &plan.schedule[0];
        assert_eq!(
            scheduled.identifier,
            format!(
                "orb.task.{}.{}",
                task.task_uuid,
                at("2026-08-07T09:00:00+02:00").timestamp()
            )
        );
        assert_eq!(scheduled.title, "Call the dentist");
        assert_eq!(scheduled.task_uuid, task.task_uuid);
        assert_eq!(scheduled.reminder_id, Some(reminder_id));
        assert_eq!(scheduled.fire_at, "2026-08-07T09:00:00+02:00");
    }

    #[test]
    fn reconciling_twice_against_a_host_schedules_nothing_the_second_time() {
        let db = database();
        task_with_reminder(&db, "Call the dentist", "2026-08-07T09:00:00+02:00");
        task_with_reminder(&db, "Water the plants", "2026-08-08T18:00:00+02:00");
        let now = at("2026-08-06T10:00:00Z");
        let service = service(&db);
        let host = RecordingHost::default();

        let first = service.apply(&host, now).expect("apply");
        assert_eq!(first.schedule.len(), 2);

        let second = service.apply(&host, now).expect("apply again");
        assert!(second.is_noop(), "{second:?}");
        assert_eq!(*host.scheduled.lock().unwrap(), 2, "no re-scheduling");
        assert_eq!(*host.cancelled.lock().unwrap(), 0);
    }

    #[test]
    fn completing_a_task_withdraws_its_notification() {
        let db = database();
        let (task, _) = task_with_reminder(&db, "Call the dentist", "2026-08-07T09:00:00+02:00");
        let now = at("2026-08-06T10:00:00Z");
        let service = service(&db);
        let host = RecordingHost::default();
        service.apply(&host, now).expect("apply");

        repo(&db)
            .complete_tasks(&[task.id.expect("id")], &ctx("2026-08-06T10:00:00+00:00"))
            .expect("complete");

        let plan = service.apply(&host, now).expect("apply again");
        assert_eq!(plan.cancel.len(), 1);
        assert!(host.pending_identifiers().expect("pending").is_empty());
    }

    #[test]
    fn a_reminder_this_machine_has_already_fired_is_not_re_armed() {
        let db = database();
        let (_, reminder_id) =
            task_with_reminder(&db, "Call the dentist", "2026-08-07T09:00:00+02:00");
        repo(&db)
            .mark_reminder_triggered(reminder_id)
            .expect("trigger");

        let plan = service(&db)
            .reconcile_at(&[], at("2026-08-06T10:00:00Z"))
            .expect("reconcile");
        assert!(plan.schedule.is_empty(), "{plan:?}");
    }

    #[test]
    fn the_window_is_refilled_at_the_next_local_midnight() {
        let db = database();
        // 23:30 local on the 6th is 21:30 UTC; the refill is 22:00 UTC, which
        // is midnight local — not 24 hours later, and not UTC midnight.
        let refill = service(&db).next_refill_at(at("2026-08-06T21:30:00Z"));
        assert_eq!(refill, at("2026-08-06T22:00:00Z"));
    }

    // -- the missed-reminder boundary ---------------------------------------

    #[test]
    fn a_reminder_just_under_a_day_overdue_still_fires() {
        let db = database();
        task_with_reminder(&db, "Call the dentist", "2026-08-06T09:00:00+02:00");
        // 09:00+02:00 is 07:00Z; 23:59:59 later is 06:59:59Z the next day.
        let result = service(&db)
            .catch_up_at(at("2026-08-07T06:59:59Z"), Some("device-a"))
            .expect("catch up");

        assert_eq!(result.due_reminders.len(), 1);
        assert!(result.missed.is_none());
        assert_eq!(result.triggered_count, 1);
    }

    #[test]
    fn a_reminder_exactly_a_day_overdue_is_summarised_instead() {
        let db = database();
        task_with_reminder(&db, "Call the dentist", "2026-08-06T09:00:00+02:00");
        let result = service(&db)
            .catch_up_at(at("2026-08-07T07:00:00Z"), Some("device-a"))
            .expect("catch up");

        assert!(result.due_reminders.is_empty());
        let summary = result.missed.expect("summary");
        assert_eq!(summary.count, 1);
        assert_eq!(summary.tasks, 1);
        assert_eq!(summary.to_string(), "1 missed reminder");
        assert_eq!(result.triggered_count, 1);
    }

    #[test]
    fn a_year_of_missed_reminders_arrives_as_one_notification() {
        let db = database();
        let task = create(&db, "Water the plants");
        let task_id = task.id.expect("id");
        let repo = repo(&db);
        for day_of_year in 0..300 {
            let instant = at("2025-09-01T07:00:00Z") + Duration::days(day_of_year);
            repo.add_reminder(
                task_id,
                &instant.to_rfc3339(),
                &ctx("2026-08-06T10:00:00+00:00"),
            )
            .expect("add");
        }
        // One that is still recent, to prove the two sets are separated rather
        // than the whole sweep being silenced.
        repo.add_reminder(
            task_id,
            "2026-08-06T07:00:00+00:00",
            &ctx("2026-08-06T10:00:00+00:00"),
        )
        .expect("add recent");

        let notifier = CollectingNotifier::new();
        let result = service(&db)
            .catch_up_at(at("2026-08-06T10:00:00Z"), Some("device-a"))
            .expect("catch up");
        result.notify(&notifier);

        assert_eq!(notifier.take_notifications().len(), 1, "one live reminder");
        let summaries = notifier.take_summaries();
        assert_eq!(summaries.len(), 1, "one summary for everything else");
        assert_eq!(summaries[0].count, 300);
        assert_eq!(summaries[0].tasks, 1);
        assert_eq!(result.triggered_count, 301);

        // And the log records it once, not three hundred times.
        let missed: Vec<_> = repo
            .get_log(task_id)
            .expect("log")
            .into_iter()
            .filter(|entry| entry.event_type() == "reminders_missed")
            .collect();
        assert_eq!(missed.len(), 1);
        assert_eq!(missed[0].rendered_text(), "300 missed reminders");
    }

    #[test]
    fn a_second_sweep_delivers_nothing_a_second_time() {
        let db = database();
        task_with_reminder(&db, "Call the dentist", "2026-08-06T09:00:00+02:00");
        let service = service(&db);
        let now = at("2026-08-06T10:00:00Z");

        assert_eq!(
            service
                .catch_up_at(now, Some("device-a"))
                .expect("catch up")
                .due_reminders
                .len(),
            1
        );
        let again = service.catch_up_at(now, Some("device-a")).expect("again");
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn missed_reminders_on_several_tasks_are_counted_per_task() {
        let db = database();
        let repo = repo(&db);
        let first = create(&db, "First").id.expect("id");
        let second = create(&db, "Second").id.expect("id");
        for (task_id, count) in [(first, 3), (second, 1)] {
            for index in 0..count {
                let instant = at("2026-01-01T08:00:00Z") + Duration::days(index);
                repo.add_reminder(
                    task_id,
                    &instant.to_rfc3339(),
                    &ctx("2026-08-06T10:00:00+00:00"),
                )
                .expect("add");
            }
        }

        let result = service(&db)
            .catch_up_at(at("2026-08-06T10:00:00Z"), Some("device-a"))
            .expect("catch up");
        let summary = result.missed.expect("summary");
        assert_eq!(summary.count, 4);
        assert_eq!(summary.tasks, 2);
        assert_eq!(
            summary.body(),
            "Across 2 tasks, oldest 2026-01-01T10:00:00+02:00"
        );

        let count_of = |task_id: i64| -> String {
            repo.get_log(task_id)
                .expect("log")
                .into_iter()
                .find(|entry| entry.event_type() == "reminders_missed")
                .expect("entry")
                .rendered_text()
        };
        assert_eq!(count_of(first), "3 missed reminders");
        assert_eq!(count_of(second), "1 missed reminders");
    }

    #[test]
    fn a_trashed_task_does_not_shout_on_launch() {
        let db = database();
        let (task, _) = task_with_reminder(&db, "Call the dentist", "2026-08-06T09:00:00+02:00");
        repo(&db)
            .trash_tasks(&[task.id.expect("id")], &ctx("2026-08-06T10:00:00+00:00"))
            .expect("trash");

        let result = service(&db)
            .catch_up_at(at("2026-08-06T10:00:00Z"), Some("device-a"))
            .expect("catch up");
        assert!(result.is_empty(), "{result:?}");
    }

    #[test]
    fn a_delivered_notification_is_recorded_and_recorded_only_once() {
        let db = database();
        let (task, reminder_id) =
            task_with_reminder(&db, "Call the dentist", "2026-08-07T09:00:00+02:00");
        let service = service(&db);
        let now = at("2026-08-07T07:00:01Z");

        assert!(
            service
                .record_fired(reminder_id, now, Some("device-a"))
                .expect("record")
        );
        assert!(
            !service
                .record_fired(reminder_id, now, Some("device-a"))
                .expect("record again"),
            "a second report of the same delivery is not a second firing"
        );

        let fired: Vec<_> = repo(&db)
            .get_log(task.id.expect("id"))
            .expect("log")
            .into_iter()
            .filter(|entry| entry.event_type() == "reminder_fired")
            .collect();
        assert_eq!(fired.len(), 1);

        // And the launch sweep no longer sees it.
        let result = service.catch_up_at(now, Some("device-a")).expect("sweep");
        assert!(result.is_empty(), "{result:?}");
    }

    #[test]
    fn a_delivery_the_os_reports_by_identifier_is_recorded_too() {
        // What the OS hands back on launch is the identifier it was scheduled
        // under, so this is the path that actually keeps the sweep quiet.
        let db = database();
        let (task, _) = task_with_reminder(&db, "Call the dentist", "2026-08-07T09:00:00+02:00");
        let service = service(&db);
        let now = at("2026-08-07T07:00:01Z");
        let identifier = format!(
            "orb.task.{}.{}",
            task.task_uuid,
            at("2026-08-07T09:00:00+02:00").timestamp()
        );

        assert!(
            service
                .record_delivered(&identifier, now, Some("device-a"))
                .expect("record")
        );
        assert!(
            !service
                .record_delivered(&identifier, now, Some("device-a"))
                .expect("record again")
        );
        assert!(
            !service
                .record_delivered("orb.task.gone.17", now, Some("device-a"))
                .expect("unknown identifier"),
            "an identifier that resolves to nothing is not an error"
        );
        assert!(
            service
                .catch_up_at(now, Some("device-a"))
                .expect("sweep")
                .is_empty()
        );
    }

    #[test]
    fn snoozing_fires_the_original_and_arms_a_new_one() {
        let db = database();
        let (task, reminder_id) =
            task_with_reminder(&db, "Call the dentist", "2026-08-06T09:00:00+02:00");
        let service = service(&db);
        let now = at("2026-08-06T07:05:00Z");

        let snoozed = service
            .snooze_reminder(reminder_id, 10, now, Some("device-a"))
            .expect("snooze");
        let reminders = repo(&db)
            .get_reminders(task.id.expect("id"))
            .expect("reminders");
        assert_eq!(reminders.len(), 2);
        assert!(
            reminders
                .iter()
                .any(|r| r.id == Some(reminder_id) && r.is_triggered)
        );

        let new = reminders
            .iter()
            .find(|r| r.id == Some(snoozed))
            .expect("new reminder");
        assert_eq!(new.remind_at, "2026-08-06T09:15:00+02:00");
        assert!(!new.is_triggered);
    }

    // -- recurrence ---------------------------------------------------------

    #[test]
    fn reminders_follow_a_recurring_task_forward() {
        let db = database();
        let (task, _) = task_with_reminder(&db, "Water the plants", "2026-08-06T09:00:00+02:00");
        let task_id = task.id.expect("id");
        // A second reminder the evening before, to prove the offsets are kept
        // rather than every reminder being pinned to the new date.
        repo(&db)
            .add_reminder(
                task_id,
                "2026-08-05T18:00:00+02:00",
                &ctx("2026-08-06T10:00:00+00:00"),
            )
            .expect("add");

        let moved = service(&db)
            .follow_task(
                task_id,
                ReminderShift {
                    from: day("2026-08-06"),
                    to: day("2026-09-06"),
                },
                &ctx("2026-08-06T10:00:00+00:00"),
            )
            .expect("follow");
        assert_eq!(moved, 2);

        let reminders = repo(&db).get_reminders(task_id).expect("reminders");
        let times: Vec<&str> = reminders
            .iter()
            .map(|reminder| reminder.remind_at.as_str())
            .collect();
        assert_eq!(
            times,
            vec!["2026-09-05T18:00:00+02:00", "2026-09-06T09:00:00+02:00"]
        );
    }

    // -- the series of a recurring task (§32) -----------------------------

    const NOW: &str = "2026-08-06T10:00:00+00:00";

    /// A task postponed to Monday 2026-08-10 that repeats weekly.
    fn weekly(db: &Database) -> i64 {
        let id = create(db, "Water the plants").id.expect("id");
        let updates = TaskUpdates::new()
            .with_postpone_date(Some(day("2026-08-10")))
            .with_recurrence(Some(&"FREQ=WEEKLY".parse().expect("rule")));
        repo(db).apply(id, &updates, &ctx(NOW)).expect("repeat");
        id
    }

    /// Apply an edit and let the reminders follow it, as the app does.
    fn edit(db: &Database, id: i64, updates: TaskUpdates, now: &str) {
        let outcome = repo(db).apply(id, &updates, &ctx(now)).expect("edit");
        service(db)
            .follow_recurrence(&outcome.reminder_shifts, &ctx(now))
            .expect("follow");
    }

    fn series(db: &Database, id: i64) -> Option<String> {
        repo(db)
            .get_task(id)
            .expect("task")
            .expect("there")
            .recurrence_series
    }

    fn times(db: &Database, id: i64) -> Vec<String> {
        repo(db)
            .get_reminders(id)
            .expect("reminders")
            .into_iter()
            .map(|reminder| reminder.remind_at)
            .collect()
    }

    #[test]
    fn a_reminder_on_a_recurring_task_is_one_of_its_series() {
        let db = database();
        let id = weekly(&db);
        let revision = repo(&db).get_task(id).unwrap().unwrap().revision;

        service(&db)
            .add_reminder(id, "2026-08-09T18:00:00+02:00", &ctx(NOW))
            .expect("add");
        let on_the_day = service(&db)
            .add_reminder(id, "2026-08-10T09:00:00+02:00", &ctx(NOW))
            .expect("add");
        assert_eq!(
            series(&db, id).as_deref(),
            Some("day=2026-08-10; reminders=-1T18:00,0T09:00")
        );
        // One revision each, as for any reminder (§16).
        let task = repo(&db).get_task(id).unwrap().unwrap();
        assert_eq!(task.revision, revision + 2);
        assert!(task.is_dirty);

        assert!(
            service(&db)
                .delete_reminder(on_the_day, &ctx(NOW))
                .expect("delete")
        );
        assert_eq!(
            series(&db, id).as_deref(),
            Some("day=2026-08-10; reminders=-1T18:00")
        );
        assert_eq!(times(&db, id), vec!["2026-08-09T18:00:00+02:00"]);
    }

    #[test]
    fn a_reminder_on_a_task_that_does_not_repeat_states_no_series() {
        let db = database();
        let id = create(&db, "Call the plumber").id.expect("id");
        let reminder = service(&db)
            .add_reminder(id, "2026-08-09T18:00:00+02:00", &ctx(NOW))
            .expect("add");
        assert_eq!(series(&db, id), None);
        assert!(service(&db).delete_reminder(reminder, &ctx(NOW)).unwrap());
        assert!(times(&db, id).is_empty());
    }

    #[test]
    fn the_reminders_go_with_an_occurrence_moved_by_hand_and_back_to_the_series_after_it() {
        let db = database();
        let id = weekly(&db);
        service(&db)
            .add_reminder(id, "2026-08-09T18:00:00+02:00", &ctx(NOW))
            .expect("add");

        // Wednesday instead of Monday, this once.
        edit(
            &db,
            id,
            TaskUpdates::new().with_postpone_date(Some(day("2026-08-12"))),
            NOW,
        );
        assert_eq!(times(&db, id), vec!["2026-08-11T18:00:00+02:00"]);

        // Done that Wednesday: the next one is Monday again, the evening before.
        edit(
            &db,
            id,
            TaskUpdates::new().with_is_completed(true),
            "2026-08-12T10:00:00+00:00",
        );
        assert_eq!(times(&db, id), vec!["2026-08-16T18:00:00+02:00"]);
        assert_eq!(
            series(&db, id).as_deref(),
            Some("day=2026-08-17; reminders=-1T18:00")
        );
    }

    #[test]
    fn the_next_occurrence_has_the_series_reminders_whatever_became_of_this_ones() {
        let db = database();
        let id = weekly(&db);
        let reminder = service(&db)
            .add_reminder(id, "2026-08-10T09:00:00+02:00", &ctx(NOW))
            .expect("add");

        // It fires, is snoozed by an hour, and both are long cleared away by
        // the time the task is done.
        service(&db)
            .snooze_reminder(reminder, 60, at("2026-08-10T07:00:00Z"), Some("device-a"))
            .expect("snooze");
        assert_eq!(times(&db, id).len(), 2);
        db.conn
            .execute("DELETE FROM task_reminders WHERE task_id = ?1", [id])
            .expect("clear");

        edit(
            &db,
            id,
            TaskUpdates::new().with_is_completed(true),
            "2026-08-14T10:00:00+00:00",
        );
        assert_eq!(times(&db, id), vec!["2026-08-17T09:00:00+02:00"]);
        assert!(!repo(&db).get_reminders(id).unwrap()[0].is_triggered);
    }

    #[test]
    fn a_series_that_states_no_reminders_takes_them_as_they_stand_and_states_them_then() {
        // A task written before there were series: its rule, its dates and a
        // reminder, as they come off the wire.
        let db = database();
        let id = create(&db, "Water the plants").id.expect("id");
        db.conn
            .execute(
                "UPDATE tasks SET recurrence = 'FREQ=WEEKLY', snooze_until = '2026-08-10',
                 snooze_until_unix = 1786320000 WHERE id = ?1",
                [id],
            )
            .expect("an older task");
        repo(&db)
            .add_reminder(id, "2026-08-09T18:00:00+02:00", &ctx(NOW))
            .expect("add");
        assert_eq!(series(&db, id), None);

        edit(
            &db,
            id,
            TaskUpdates::new().with_is_completed(true),
            "2026-08-10T10:00:00+00:00",
        );
        assert_eq!(times(&db, id), vec!["2026-08-16T18:00:00+02:00"]);
        assert_eq!(
            series(&db, id).as_deref(),
            Some("day=2026-08-17; reminders=-1T18:00")
        );
    }

    #[test]
    fn undoing_a_completion_puts_the_reminders_back_on_the_occurrence_it_left() {
        let db = database();
        let id = weekly(&db);
        service(&db)
            .add_reminder(id, "2026-08-09T18:00:00+02:00", &ctx(NOW))
            .expect("add");

        let done = repo(&db)
            .complete_tasks(&[id], &ctx("2026-08-10T10:00:00+00:00"))
            .expect("complete");
        service(&db)
            .follow_recurrence(&done.reminder_shifts, &ctx("2026-08-10T10:00:00+00:00"))
            .expect("follow");
        assert_eq!(times(&db, id), vec!["2026-08-16T18:00:00+02:00"]);

        let undone = repo(&db)
            .undo(&done.undo, &ctx("2026-08-10T10:01:00+00:00"))
            .expect("undo");
        service(&db)
            .follow_recurrence(&undone.reminder_shifts, &ctx("2026-08-10T10:01:00+00:00"))
            .expect("follow");
        assert_eq!(times(&db, id), vec!["2026-08-09T18:00:00+02:00"]);
        assert_eq!(
            series(&db, id).as_deref(),
            Some("day=2026-08-10; reminders=-1T18:00")
        );
    }

    #[test]
    fn a_moved_reminder_is_armed_again_even_if_it_had_already_fired() {
        // Otherwise a recurring task's reminder fires for its first occurrence
        // and never again.
        let db = database();
        let (task, reminder_id) =
            task_with_reminder(&db, "Water the plants", "2026-08-06T09:00:00+02:00");
        let task_id = task.id.expect("id");
        repo(&db)
            .mark_reminder_triggered(reminder_id)
            .expect("trigger");

        service(&db)
            .follow_task(
                task_id,
                ReminderShift {
                    from: day("2026-08-06"),
                    to: day("2026-08-13"),
                },
                &ctx("2026-08-06T10:00:00+00:00"),
            )
            .expect("follow");

        let reminders = repo(&db).get_reminders(task_id).expect("reminders");
        assert_eq!(reminders.len(), 1);
        assert!(!reminders[0].is_triggered);
        assert_eq!(reminders[0].remind_at, "2026-08-13T09:00:00+02:00");
    }

    #[test]
    fn a_shift_that_moves_nothing_writes_nothing() {
        let db = database();
        let (task, reminder_id) =
            task_with_reminder(&db, "Water the plants", "2026-08-06T09:00:00+02:00");
        let moved = service(&db)
            .follow_task(
                task.id.expect("id"),
                ReminderShift {
                    from: day("2026-08-06"),
                    to: day("2026-08-06"),
                },
                &ctx("2026-08-06T10:00:00+00:00"),
            )
            .expect("follow");
        assert_eq!(moved, 0);
        assert_eq!(
            repo(&db)
                .get_reminders(task.id.expect("id"))
                .expect("reminders")[0]
                .id,
            Some(reminder_id),
            "the row was not rewritten"
        );
    }

    // -- DST ----------------------------------------------------------------

    #[test]
    fn the_test_zone_really_has_a_dst_rule() {
        assert_eq!(
            Cet.offset_from_utc_datetime(&at("2026-01-15T12:00:00Z").naive_utc()),
            Cet::winter()
        );
        assert_eq!(
            Cet.offset_from_utc_datetime(&at("2026-07-15T12:00:00Z").naive_utc()),
            Cet::summer()
        );
    }

    #[test]
    fn a_reminder_dragged_across_a_dst_boundary_keeps_its_wall_clock() {
        // Winter to summer: 09:00 local is 08:00Z in March and 07:00Z in April.
        // Reusing the offset the reminder was created with would leave it at
        // 10:00 local — the hour-off failure §4.3 is about.
        let db = database();
        let db_repo = TaskRepository::with_zone(&db, RepositoryZone(Cet));
        let task = db_repo
            .create_task("Pay the rent", ACCOUNT, &ctx("2026-03-15T08:00:00+00:00"))
            .expect("task");
        let task_id = task.id.expect("id");
        db_repo
            .add_reminder(
                task_id,
                "2026-03-15T09:00:00+01:00",
                &ctx("2026-08-06T10:00:00+00:00"),
            )
            .expect("add");

        let moved = ReminderService::with_zone(&db, Cet)
            .follow_task(
                task_id,
                ReminderShift {
                    from: day("2026-03-15"),
                    to: day("2026-04-15"),
                },
                &ctx("2026-08-06T10:00:00+00:00"),
            )
            .expect("follow");
        assert_eq!(moved, 1);

        let reminders = db_repo.get_reminders(task_id).expect("reminders");
        assert_eq!(reminders[0].remind_at, "2026-04-15T09:00:00+02:00");
        assert_eq!(
            reminders[0].remind_at_unix,
            at("2026-04-15T07:00:00Z").timestamp()
        );
    }

    #[test]
    fn a_reminder_in_the_hour_that_spring_forward_deletes_still_fires() {
        // 02:30 local does not exist on 2026-03-29 in CET. Dropping the
        // reminder is the one outcome that is not allowed, so it moves to the
        // first wall clock that does exist.
        let instant = first_existing_instant(
            &Cet,
            day("2026-03-29").and_hms_opt(2, 30, 0).expect("wall clock"),
        );
        assert_eq!(instant, at("2026-03-29T01:00:00Z"));
        assert_eq!(
            instant
                .with_timezone(&Cet)
                .to_rfc3339_opts(SecondsFormat::Secs, false),
            "2026-03-29T03:00:00+02:00"
        );
    }

    #[test]
    fn local_midnight_on_a_dst_day_is_still_the_start_of_that_day() {
        let db = database();
        let refill =
            ReminderService::with_zone(&db, Cet).next_refill_at(at("2026-03-28T23:30:00Z"));
        // 00:30 local on the 29th; the next local midnight is the 30th, and
        // that day is 23 hours long.
        assert_eq!(refill, at("2026-03-29T22:00:00Z"));
    }
}
