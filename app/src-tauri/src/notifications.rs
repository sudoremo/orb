//! Task reminders, delivered by the operating system (§5.1).
//!
//! The reliability bar for a reminder is that it fires at its time whether or
//! not Orb is running, so the mechanism has to be a request handed to the
//! system *ahead of time* rather than anything Orb itself watches for. That is
//! `UNUserNotificationCenter` with a calendar trigger: the request survives
//! quitting the app, logging out, sleeping and rebooting, because it belongs to
//! the notification daemon and not to this process.
//!
//! Two alternatives were considered and rejected:
//!
//! - **An in-app timer.** Explicitly not the mechanism (§5.1). A reminder set
//!   for a year out would need Orb open at that minute, a year later.
//! - **`tauri-plugin-notification`.** Its `schedule` API is iOS/Android only;
//!   on macOS it posts immediately, so a "scheduled" reminder would be
//!   delivered the moment it was created. Verified against the plugin's own
//!   platform support table rather than assumed.
//!
//! Nothing here decides *what* should be pending. The core owns that: the
//! reminder service produces a plan — a set to cancel and a set to schedule,
//! diffed against what the OS says it already holds — and this module is the
//! pair of hands that applies it. That split is what makes the interesting part
//! (the 64-slot window, the identifier derivation, the missed-reminder triage)
//! testable without a notification centre at all, and it is why the only state
//! this file keeps is the permission answer.
//!
//! ## Wall clock, not instant
//!
//! Reminders are scheduled with `UNCalendarNotificationTrigger` on the local
//! wall clock rather than `UNTimeIntervalNotificationTrigger` on a delta. A
//! reminder is "a local date **and** time" (§5), so a machine carried into
//! another timezone should still see 09:00 at 09:00; and a delta computed once
//! is not re-evaluated when the clock is corrected, which for a reminder a year
//! out is the difference between the right morning and the wrong one.
//!
//! ## Bundling and signing
//!
//! `UNUserNotificationCenter::currentNotificationCenter` raises — it does not
//! return an error — when the process has no bundle identifier, which is
//! exactly the case under `tauri dev`, where the bare binary is run. Every
//! entry point therefore checks for a bundle first and degrades to "no OS
//! notifications" rather than taking the app down. The same applies to an
//! unsigned bundle, which the daemon refuses to register; `tauri.conf.json`
//! ad-hoc signs the app for that reason.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orb_core::ffi::OrbHandle;
use tauri::{AppHandle, Emitter, Manager};

type Handle = std::sync::Arc<OrbHandle>;

/// Clicking a reminder emits this, carrying the task the user asked to see.
///
/// The frontend owns what "opening" means — which view to land in, whether the
/// detail pane opens — so the platform side stops at naming the task.
pub const TASK_OPENED_EVENT: &str = "task-notification-opened";

/// The local civil date rolled over (§4.3).
///
/// Every date-dependent view is wrong the instant this fires and right again as
/// soon as it is acted on. An Orb left open overnight must show the correct
/// Today list in the morning without a restart, and this is the only thing that
/// makes that true.
pub const DAY_CHANGED_EVENT: &str = "task-day-changed";

/// Reminders that came due while nobody was watching, from the launch sweep.
pub const REMINDERS_MISSED_EVENT: &str = "task-reminders-missed";

/// Clicking a new-mail notification emits this, carrying the message's row id.
pub const MAIL_OPENED_EVENT: &str = "mail-notification-opened";

/// The prefix of a new-mail notification's identifier, followed by the id of
/// the message it opens. Outside the `orb.task.` namespace the core owns, so
/// a reminder reconcile never mistakes one for a reminder.
const MAIL_IDENTIFIER_PREFIX: &str = "orb.mail.";

/// Clicking a failed-send notification emits this: the Outbox is to open.
pub const OUTBOX_OPENED_EVENT: &str = "outbox-notification-opened";

/// The prefix of a failed-send notification's identifier, followed by the
/// queued message's id.
const OUTBOX_IDENTIFIER_PREFIX: &str = "orb.outbox.";

/// Clicking a chat notification emits this, carrying the thread to open.
pub const CHAT_OPENED_EVENT: &str = "chat-notification-opened";

/// The prefix of a chat notification's identifier, followed by the room id,
/// a slash and the thread id (empty for a room's own timeline).
const CHAT_IDENTIFIER_PREFIX: &str = "orb.chat.";

/// How many threads that start waiting at once get a notification each;
/// more are one notification that counts them.
const CHAT_THREADS_SHOWN: usize = 3;

/// What a notification click tells the frontend.
#[derive(Clone, serde::Serialize)]
pub struct OpenedTask {
    /// The notification's identifier, as the OS held it.
    pub identifier: String,
    /// The task's stable identity (§2) — never a row id, which differs between
    /// machines and is not what the identifier encodes.
    pub task_uuid: String,
    /// The local row id, when the task could be found in one of the views.
    /// `None` for a task this machine has not synced yet, in which case the
    /// frontend still has the uuid to look up after the next sync.
    pub task_id: Option<i64>,
}

/// How long to wait for the notification centre to answer.
///
/// Every read from `UNUserNotificationCenter` is a completion handler on a
/// queue we do not control, and this module is called from blocking workers
/// that must not be parked forever by a daemon that never replies. Timing out
/// is safe in the one direction that matters: an empty "pending" answer makes
/// the core's diff schedule everything, and scheduling a request whose
/// identifier is already pending replaces it rather than duplicating it.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the midnight watcher looks at the clock.
///
/// A deadline in a `sleep` would be wrong twice over: Darwin's monotonic clock
/// does not advance while the machine is asleep, so a laptop shut overnight
/// would wake with hours still to wait, and a timezone change would leave the
/// old deadline standing. Ticking against the wall clock and re-asking the core
/// for the next midnight each time covers sleep, timezone changes and DST
/// without a separate notification for any of them (§4.3).
const MIDNIGHT_TICK: Duration = Duration::from_secs(60);

/// How much wall clock has to pass in one tick before it counts as a wake.
///
/// Five minutes rather than two ticks: a machine under load, or one whose
/// scheduler has parked this thread, can lose a minute or two without having
/// slept, and treating that as a wake would re-post the missed-reminder summary
/// for no reason. A real sleep is minutes at the very least and usually hours.
const WAKE_GAP: Duration = Duration::from_secs(300);

/// True once the authorization request has come back refused.
///
/// The live authorization status is the better signal and is what
/// [`warnings`] reads first; this covers the window in which the user has
/// dismissed the dialogue but the daemon has not yet settled on `Denied`.
static AUTHORIZATION_REFUSED: AtomicBool = AtomicBool::new(false);

/// Whether the notification centre could be reached at launch, so a new-mail
/// announcement knows not to try.
static MAIL_NOTIFICATIONS_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// How many messages a single new-mail notification names before it counts.
const MAIL_SUBJECTS_SHOWN: usize = 3;

/// Announce new unread mail, one notification per account.
///
/// One message is announced by its sender and subject; several are announced
/// as a count with the first few subjects under it. Clicking either opens the
/// newest of them. Per account rather than per message, because a burst of
/// twenty deliveries is one event to the person reading them, and twenty
/// banners are a reason to turn notifications off.
pub fn notify_new_mail(items: &[orb_core::ffi::NewMailItem]) {
    if items.is_empty() || !MAIL_NOTIFICATIONS_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let mut by_account: Vec<(&str, Vec<&orb_core::ffi::NewMailItem>)> = Vec::new();
    for item in items {
        match by_account
            .iter_mut()
            .find(|(name, _)| *name == item.account_name)
        {
            Some((_, list)) => list.push(item),
            None => by_account.push((&item.account_name, vec![item])),
        }
    }
    for (account, messages) in by_account {
        // Oldest first came in; the newest is what a click should land on.
        let newest = messages[messages.len() - 1];
        let identifier = format!("{MAIL_IDENTIFIER_PREFIX}{}", newest.id);
        let (title, body) = if messages.len() == 1 {
            (
                format!(
                    "{} · {account}",
                    sender_name(newest.from_display.as_deref())
                ),
                newest
                    .subject
                    .clone()
                    .unwrap_or_else(|| "(no subject)".to_string()),
            )
        } else {
            let mut lines: Vec<String> = messages
                .iter()
                .rev()
                .take(MAIL_SUBJECTS_SHOWN)
                .map(|m| {
                    format!(
                        "{}: {}",
                        sender_name(m.from_display.as_deref()),
                        m.subject.as_deref().unwrap_or("(no subject)")
                    )
                })
                .collect();
            if messages.len() > MAIL_SUBJECTS_SHOWN {
                lines.push(format!("and {} more", messages.len() - MAIL_SUBJECTS_SHOWN));
            }
            (
                format!("{} new messages · {account}", messages.len()),
                lines.join("\n"),
            )
        };
        platform::post(&Request {
            identifier: &identifier,
            title: &title,
            body,
            fire_at: None,
            thread: &format!("com.sudoremo.orb.mail.{account}"),
        });
    }
}

/// Announce the chat threads that started waiting (PLAN.md 15.10): each by
/// who wrote and where, or several as one count. A click opens the thread.
pub fn notify_chat(threads: &[orb_core::chat::WaitingThread]) {
    if threads.is_empty()
        || !MAIL_NOTIFICATIONS_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let group = "com.sudoremo.orb.chat";
    if threads.len() <= CHAT_THREADS_SHOWN {
        for thread in threads {
            let identifier = format!("{CHAT_IDENTIFIER_PREFIX}{}/{}", thread.rid, thread.thread);
            let body = match &thread.title {
                Some(title) => format!("{title}\n{}", thread.preview),
                None => thread.preview.clone(),
            };
            platform::post(&Request {
                identifier: &identifier,
                title: &format!("{} · {}", thread.author, thread.room.label),
                body,
                fire_at: None,
                thread: group,
            });
        }
        return;
    }
    let newest = &threads[threads.len() - 1];
    let identifier = format!("{CHAT_IDENTIFIER_PREFIX}{}/{}", newest.rid, newest.thread);
    let lines: Vec<String> = threads
        .iter()
        .rev()
        .take(CHAT_THREADS_SHOWN)
        .map(|t| format!("{} · {}: {}", t.author, t.room.label, t.preview))
        .chain(
            (threads.len() > CHAT_THREADS_SHOWN)
                .then(|| format!("and {} more", threads.len() - CHAT_THREADS_SHOWN)),
        )
        .collect();
    platform::post(&Request {
        identifier: &identifier,
        title: &format!("{} threads waiting", threads.len()),
        body: lines.join("\n"),
        fire_at: None,
        thread: group,
    });
}

/// Say that the task export failed or is incomplete (todo-spec §20.3): a
/// backup that fails quietly is one nobody finds out about until it is needed.
pub fn notify_export_problem(problem: &str) {
    if !MAIL_NOTIFICATIONS_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    platform::post(&Request {
        identifier: "com.sudoremo.orb.export",
        title: "Orb could not export the tasks",
        body: problem.to_string(),
        fire_at: None,
        thread: "com.sudoremo.orb.export",
    });
}

/// Say that the mail server refused a queued message: it waits in the
/// Outbox, and nobody would look there unprompted.
pub fn notify_send_failed(failed: &orb_core::store::FailedSend) {
    if !MAIL_NOTIFICATIONS_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let identifier = format!("{OUTBOX_IDENTIFIER_PREFIX}{}", failed.id);
    let subject = if failed.subject.trim().is_empty() {
        "(no subject)"
    } else {
        failed.subject.trim()
    };
    let title = format!("Not sent: {subject}");
    platform::post(&Request {
        identifier: &identifier,
        title: &title,
        body: format!(
            "{} · {} · It waits in the Outbox.",
            failed.account, failed.reason
        ),
        fire_at: None,
        thread: "com.sudoremo.orb.outbox",
    });
}

/// The display name out of a `From` header, or the address when it has none.
fn sender_name(from: Option<&str>) -> String {
    let from = from.unwrap_or("").trim();
    if from.is_empty() {
        return "(unknown sender)".to_string();
    }
    match from.rsplit_once('<') {
        Some((name, _)) if !name.trim().is_empty() => name.trim().trim_matches('"').to_string(),
        Some((_, rest)) => rest.trim_end_matches('>').to_string(),
        None => from.to_string(),
    }
}

/// Start the platform side of reminders.
///
/// Called once from `setup`, on the main thread, before the run loop starts —
/// the notification delegate has to be in place before the app finishes
/// launching or a click that woke the app is delivered to nobody.
///
/// Everything that touches the database or blocks on the notification centre
/// then moves to a thread of its own. Registering the host reconciles
/// immediately, and that reconcile blocks on the daemon's answer; doing it on
/// the main thread would be a deadlock waiting for a queue that is waiting for
/// the main thread.
pub fn install(app: &AppHandle, orb: &Handle) {
    // The delegate and the permission are for new mail as much as for
    // reminders, so they are set up whether or not tasks are configured.
    let available = platform::prepare(app);
    MAIL_NOTIFICATIONS_AVAILABLE.store(available, std::sync::atomic::Ordering::Relaxed);

    // The rest is reminders, and without a designated tasks account there are
    // none to schedule, none to sweep and no date-dependent view to
    // re-evaluate at midnight. Returning here keeps a config that simply does
    // not use tasks from logging a configuration error on every launch, which
    // reads as a fault rather than as a choice.
    if !orb.tasks_configured() {
        orb_core::logger::info("tasks: no account configured, reminders not installed");
        return;
    }

    let app = app.clone();
    let orb = orb.clone();
    std::thread::spawn(move || {
        if available {
            // Two sweeps, in this order and for one reason: anything the OS
            // already showed while Orb was closed must be recorded as
            // delivered *before* the catch-up decides what still needs firing,
            // or every reminder the user has already seen arrives a second
            // time on launch (§5.2).
            record_delivered(&orb);
            orb.set_notification_host(Box::new(SystemNotifications));
            catch_up(&app, &orb);
        }
        // The day still turns over on a machine with no notification
        // permission, and the views still have to follow it.
        watch_for_midnight(&app, &orb);
    });
}

/// Configuration problems worth telling the user about (§5.1).
///
/// Surfaced through the same channel as a misconfigured trash folder, because
/// the failure is the same shape: everything looks like it is working, and
/// nothing arrives. A reminder that was silently never permitted is the worst
/// outcome this feature has, so it is stated in the status line rather than
/// left in a log.
pub fn warnings() -> Vec<String> {
    permission_warning().into_iter().collect()
}

/// Whether this process may post notifications, as the frontend shows it.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Granted,
    /// Refused, in the dialogue or in System Settings. macOS does not ask a
    /// second time, so only the user can undo it, in System Settings.
    Denied,
    /// Not answered yet: the dialogue is still up, or was never shown.
    Undetermined,
    /// No notification centre at all: a build without an application bundle.
    Unavailable,
}

pub fn permission() -> Permission {
    platform::permission()
}

/// Open System Settings › Notifications at Orb's own entry.
pub fn open_settings(app: &AppHandle) {
    let identifier = &app.config().identifier;
    let url = format!(
        "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id={identifier}"
    );
    if let Err(error) = std::process::Command::new("open").arg(&url).spawn() {
        orb_core::logger::warn(&format!(
            "could not open the notification settings: {error}"
        ));
    }
}

fn permission_warning() -> Option<String> {
    match permission() {
        Permission::Unavailable => Some(
            "notifications are unavailable in this build (no application bundle), \
             so task reminders will only appear while Orb is running"
                .to_string(),
        ),
        Permission::Denied => Some(
            "notification permission is denied, so task reminders will not appear — \
             grant it under System Settings › Notifications › Orb"
                .to_string(),
        ),
        Permission::Granted | Permission::Undetermined => None,
    }
}

/// Applies the core's plan to the notification centre.
///
/// Registered with [`OrbHandle::set_notification_host`], after which the core
/// reconciles on its own after every task mutation and every task sync —
/// nothing in the UI has to remember to.
struct SystemNotifications;

impl orb_core::ffi::TaskNotificationHost for SystemNotifications {
    fn pending_identifiers(&self) -> Vec<String> {
        platform::pending_identifiers()
    }

    fn schedule(&self, notification: orb_core::tasks::ScheduledNotification) {
        platform::post(&Request {
            identifier: &notification.identifier,
            title: &notification.title,
            body: reminder_body(&notification.fire_at),
            fire_at: Some(&notification.fire_at),
            thread: TASK_THREAD_IDENTIFIER,
        });
    }

    fn cancel(&self, identifiers: Vec<String>) {
        platform::cancel(&identifiers);
    }
}

/// One notification, as this module hands it to the system.
struct Request<'a> {
    identifier: &'a str,
    title: &'a str,
    body: String,
    /// Local RFC3339 wall clock to fire at, or `None` to deliver immediately.
    fire_at: Option<&'a str>,
    /// What Notification Centre groups the notification under.
    thread: &'a str,
}

/// Everything a reminder from Orb is grouped under in Notification Centre.
const TASK_THREAD_IDENTIFIER: &str = "com.sudoremo.orb.tasks";

/// The line under a reminder's title.
///
/// The task's own title is the headline; what a glance needs beside it is when
/// it was meant to happen, which the notification's own timestamp does not say
/// once it has been sitting in Notification Centre for a day.
fn reminder_body(fire_at: &str) -> String {
    match clock_of(fire_at) {
        Some(time) => format!("Reminder · {time}"),
        None => "Reminder".to_string(),
    }
}

/// `HH:MM` out of a local RFC3339 stamp, or nothing if it is not one.
fn clock_of(stamp: &str) -> Option<&str> {
    let clock = stamp.get(11..16)?;
    let mut parts = clock.split(':');
    let hour = parts.next()?;
    let minute = parts.next()?;
    (hour.len() == 2
        && minute.len() == 2
        && hour.bytes().all(|b| b.is_ascii_digit())
        && minute.bytes().all(|b| b.is_ascii_digit()))
    .then_some(clock)
}

/// Tell the core about everything the OS has already shown.
///
/// The delegate reports deliveries while Orb is running; this is the other
/// half, for the notifications that arrived while it was not. Without it the
/// launch sweep has no way to tell "the user has already seen this" from "this
/// never fired", and defaults to firing it again.
fn record_delivered(orb: &Handle) {
    for identifier in platform::delivered_identifiers() {
        if !orb_core::tasks::is_task_notification(&identifier) {
            continue;
        }
        if let Err(error) = orb.record_task_notification_delivered(identifier.clone()) {
            orb_core::logger::warn(&format!(
                "could not record notification {identifier} as delivered: {error}"
            ));
        }
    }
}

/// The launch sweep (§5.2).
///
/// Reminders less than 24 hours overdue are delivered now; anything older is
/// collapsed by the core into a single summary, because a year of missed
/// reminders arriving as a year of notifications is not a reminder, it is a
/// reason to turn notifications off.
fn catch_up(app: &AppHandle, orb: &Handle) {
    let sweep = match orb.catch_up_task_reminders() {
        Ok(sweep) => sweep,
        Err(error) => {
            orb_core::logger::warn(&format!("could not sweep missed reminders: {error}"));
            return;
        }
    };

    for due in &sweep.due {
        platform::post(&Request {
            identifier: &due.notification_id,
            title: &due.title,
            body: reminder_body(&due.remind_at),
            // Already overdue, so there is nothing to wait for: a request with
            // no trigger is delivered at once.
            fire_at: None,
            thread: TASK_THREAD_IDENTIFIER,
        });
    }

    if let Some(summary) = &sweep.missed_summary {
        platform::post(&Request {
            // A fixed identifier, so a relaunch replaces yesterday's summary
            // rather than stacking another one beside it.
            identifier: MISSED_SUMMARY_IDENTIFIER,
            title: "Missed task reminders",
            body: summary.clone(),
            fire_at: None,
            thread: TASK_THREAD_IDENTIFIER,
        });
        let _ = app.emit(REMINDERS_MISSED_EVENT, sweep.missed_count);
    }
}

/// The identifier the "N missed reminders" summary is posted under.
///
/// Deliberately outside the `orb.task.` namespace the core owns: it belongs to
/// no reminder row, and a reconcile must never mistake it for a scheduled one
/// and cancel it.
const MISSED_SUMMARY_IDENTIFIER: &str = "orb.tasks.missed-summary";

/// Watch the wall clock for the local civil date turning over (§4.3).
///
/// Also the third reconcile trigger §5.1 asks for: launch and every task sync
/// are the core's own, and midnight is this. The pending window holds the
/// nearest 64 reminders, so a day passing is exactly when the next few become
/// reachable.
fn watch_for_midnight(app: &AppHandle, orb: &Handle) {
    let mut deadline = match next_midnight(orb) {
        Some(deadline) => deadline,
        None => {
            // No tasks account configured: there is no local midnight to
            // watch, and asking again every minute would only fill the log.
            orb_core::logger::info("tasks: no account configured, midnight watch not armed");
            return;
        }
    };

    // The wall clock as it was one tick ago. A sleeping machine does not run
    // this thread, so a gap far larger than the tick is how a wake announces
    // itself — no workspace notification to observe, and nothing to register.
    let mut ticked_at = now_unix();

    loop {
        std::thread::sleep(MIDNIGHT_TICK);

        // §5.2 asks for the missed sweep at launch; a machine that was asleep
        // for a week has missed exactly what that sweep is for, and waiting for
        // the next launch to run it could be another week. The threshold is
        // generous because a thread that was merely descheduled must not look
        // like a wake and re-post a summary.
        let elapsed = now_unix() - ticked_at;
        ticked_at = now_unix();
        if elapsed > WAKE_GAP.as_secs() as i64 {
            orb_core::logger::info(&format!(
                "tasks: {elapsed}s passed in a {}s tick — treating it as a wake",
                MIDNIGHT_TICK.as_secs()
            ));
            catch_up(app, orb);
            reconcile(orb);
        }

        let Some(next) = next_midnight(orb) else {
            continue;
        };
        // Two ways to be past the boundary. The obvious one is that the
        // deadline arrived. The other is that the core now reports a *different*
        // next midnight than the one being waited on, which happens when the
        // machine's timezone changed underneath us — and in that case the civil
        // date may well have changed with it.
        let arrived = now_unix() >= deadline;
        let moved = (next - deadline).abs() > MIDNIGHT_TICK.as_secs() as i64;
        if !arrived && !moved {
            continue;
        }

        deadline = next;
        let _ = app.emit(DAY_CHANGED_EVENT, ());
        // Re-fill the 64-slot window against the new day.
        reconcile(orb);
    }
}

/// Reconcile the OS's pending set against what the core wants pending.
///
/// The pull form of the same diff the registered host applies: ask the daemon
/// what it holds, ask the core what it should hold, apply the difference.
/// Cancel before schedule, because a moved reminder cancelled second would
/// leave the OS armed at both times.
fn reconcile(orb: &Handle) {
    let pending = platform::pending_identifiers();
    let plan = match orb.task_notification_plan(pending) {
        Ok(plan) => plan,
        Err(error) => {
            orb_core::logger::warn(&format!("could not plan task notifications: {error}"));
            return;
        }
    };
    platform::cancel(&plan.cancel);
    for notification in &plan.schedule {
        platform::post(&Request {
            identifier: &notification.identifier,
            title: &notification.title,
            body: reminder_body(&notification.fire_at),
            fire_at: Some(&notification.fire_at),
            thread: TASK_THREAD_IDENTIFIER,
        });
    }
}

/// The next local midnight as a unix instant, straight from the core.
///
/// The core owns the timezone arithmetic — the day a DST change falls on is 23
/// or 25 hours long, and "now plus 24 hours" is wrong on both of them.
fn next_midnight(orb: &Handle) -> Option<i64> {
    let stamp = orb.next_task_midnight().ok()?;
    unix_seconds(&stamp)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Which task a notification identifier belongs to, as a row this machine can
/// open.
///
/// The identifier carries the task's uuid, which is its identity everywhere
/// (§2); the row id is local and has to be looked up. Scanning the views costs
/// four queries, which is nothing on a click and avoids widening the core's API
/// for a lookup used once per notification.
///
/// Trash is not scanned: a reminder on a trashed task should never have been
/// scheduled, and landing the user in the trash would be a stranger outcome
/// than landing them nowhere.
fn locate_task(orb: &Handle, task_uuid: &str) -> Option<i64> {
    use orb_core::ffi::TaskViewName::{Archive, Completed, Someday, Today, Upcoming};

    [Today, Upcoming, Someday, Completed, Archive]
        .into_iter()
        .find_map(|view| {
            orb.list_tasks(view, None, None)
                .ok()?
                .into_iter()
                .find(|task| task.uuid == task_uuid)
                .map(|task| task.id)
        })
}

/// Act on a snooze button: the reminder is marked fired and re-armed, and
/// the OS's pending requests follow. Off the main thread, like a click.
fn snoozed(app: &AppHandle, identifier: String, minutes: i64) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(orb) = app.try_state::<Handle>().map(|state| state.inner().clone()) else {
            return;
        };
        match orb.snooze_task_notification(identifier.clone(), minutes) {
            Ok(true) => orb_core::logger::info(&format!(
                "notifications: {identifier} snoozed for {minutes} minutes"
            )),
            Ok(false) => orb_core::logger::warn(&format!(
                "notifications: {identifier} names no reminder to snooze"
            )),
            Err(error) => orb_core::logger::warn(&format!(
                "notifications: could not snooze {identifier}: {error}"
            )),
        }
        let _ = app.emit("tasks-changed", ());
    });
}

/// Act on a click: record it, name the task, and put Orb in front of it.
///
/// Called from the notification delegate, which runs on the main thread, so
/// everything that touches the database is moved off it — a click must not
/// stall AppKit behind a query.
fn opened(app: &AppHandle, identifier: String) {
    // A chat thread opens in the chat window when there is one.
    if let Some((rid, thread)) = identifier
        .strip_prefix(CHAT_IDENTIFIER_PREFIX)
        .and_then(|rest| rest.split_once('/'))
    {
        let label = if app.get_webview_window("chat").is_some() {
            "chat"
        } else {
            "main"
        };
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
        let _ = app.emit_to(label, CHAT_OPENED_EVENT, (rid, thread));
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }

    if identifier.starts_with(OUTBOX_IDENTIFIER_PREFIX) {
        let _ = app.emit(OUTBOX_OPENED_EVENT, ());
        return;
    }

    if let Some(id) = identifier.strip_prefix(MAIL_IDENTIFIER_PREFIX) {
        if let Ok(id) = id.parse::<i64>() {
            let _ = app.emit(MAIL_OPENED_EVENT, id);
        }
        return;
    }

    let Some((task_uuid, _)) = orb_core::tasks::parse_notification_id(&identifier) else {
        return;
    };
    let task_uuid = task_uuid.to_string();

    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(orb) = app.try_state::<Handle>().map(|state| state.inner().clone()) else {
            return;
        };
        // A click *is* a delivery, and the OS does not tell us twice.
        if let Err(error) = orb.record_task_notification_delivered(identifier.clone()) {
            orb_core::logger::warn(&format!(
                "could not record {identifier} as delivered: {error}"
            ));
        }
        let task_id = locate_task(&orb, &task_uuid);
        let _ = app.emit(
            TASK_OPENED_EVENT,
            OpenedTask {
                identifier,
                task_uuid,
                task_id,
            },
        );
    });
}

// ---------------------------------------------------------------------------
// Wall-clock arithmetic
// ---------------------------------------------------------------------------

/// A local date and time, taken apart for `NSDateComponents`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WallClock {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

/// Split an RFC3339 stamp into its wall-clock fields.
///
/// Only the fields, deliberately: the offset is discarded because a calendar
/// trigger fires on the machine's *current* timezone, which is the wall-clock
/// semantics §5 asks for. Written by hand rather than by pulling `chrono` into
/// the app crate for six integers — the core already owns every date decision
/// that has consequences, and this one has none.
fn wall_clock(stamp: &str) -> Option<WallClock> {
    let bytes = stamp.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    // The separator is 'T' by the letter of RFC3339, but a space is common
    // enough in stamps that have been through a human that refusing it would
    // be pedantry with a cost.
    if bytes[10] != b'T' && bytes[10] != b't' && bytes[10] != b' ' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    Some(WallClock {
        year: number(&stamp[0..4])?,
        month: number(&stamp[5..7])?,
        day: number(&stamp[8..10])?,
        hour: number(&stamp[11..13])?,
        minute: number(&stamp[14..16])?,
        second: number(&stamp[17..19])?,
    })
}

/// A fixed-width decimal field, rejecting anything that is not all digits.
fn number(field: &str) -> Option<i64> {
    field
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| field.parse().ok())
        .flatten()
}

/// The offset an RFC3339 stamp carries, in seconds east of UTC.
fn offset_seconds(stamp: &str) -> Option<i64> {
    let tail = stamp.get(19..)?;
    // Fractional seconds sit between the seconds and the offset.
    let tail = match tail.strip_prefix('.') {
        Some(rest) => {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            &rest[digits..]
        }
        None => tail,
    };
    if tail == "Z" || tail == "z" {
        return Some(0);
    }
    let (sign, rest) = match tail.as_bytes().first()? {
        b'+' => (1, &tail[1..]),
        b'-' => (-1, &tail[1..]),
        _ => return None,
    };
    let (hours, minutes) = match rest.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        // `+0200` is legal in the wild even though RFC3339 wants the colon.
        None if rest.len() == 4 => (&rest[..2], &rest[2..]),
        None => return None,
    };
    Some(sign * (number(hours)? * 3600 + number(minutes)? * 60))
}

/// An RFC3339 stamp as a unix instant.
///
/// Unlike [`wall_clock`], this one *does* honour the offset: it is used to
/// decide when the next local midnight arrives, which is a question about an
/// instant, not about a wall clock.
fn unix_seconds(stamp: &str) -> Option<i64> {
    let clock = wall_clock(stamp)?;
    let days = days_from_civil(clock.year, clock.month, clock.day);
    let seconds = days * 86_400 + clock.hour * 3600 + clock.minute * 60 + clock.second;
    Some(seconds - offset_seconds(stamp)?)
}

/// Days from 1970-01-01 to a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`, which is exact for every year in range
/// and has no table, no leap-year special cases and no branch on century.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // March-based year: February, with its variable length, becomes the last
    // month, which is what removes every leap-year branch below.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400; // [0, 399]
    let shifted_month = (month + 9) % 12; // March = 0
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---------------------------------------------------------------------------
// The notification centre itself
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    use super::{Permission, Request, ANSWER_TIMEOUT, AUTHORIZATION_REFUSED};
    use std::ptr::NonNull;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, AnyThread, DefinedClass};
    use objc2_foundation::{
        NSArray, NSBundle, NSDateComponents, NSError, NSObject, NSObjectProtocol, NSSet, NSString,
    };
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNAuthorizationStatus, UNCalendarNotificationTrigger,
        UNMutableNotificationContent, UNNotification, UNNotificationAction,
        UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNNotificationSettings, UNNotificationSound, UNUserNotificationCenter,
        UNUserNotificationCenterDelegate,
    };
    use tauri::{AppHandle, Manager};

    /// The notification centre, or nothing at all.
    ///
    /// `currentNotificationCenter` raises an Objective-C exception — which
    /// unwinds through Rust as an abort — when the process has no bundle
    /// identifier. That is every `tauri dev` run, so the check is not a
    /// nicety: without it, developing the app would crash it at launch.
    fn centre() -> Option<Retained<UNUserNotificationCenter>> {
        NSBundle::mainBundle().bundleIdentifier()?;
        Some(UNUserNotificationCenter::currentNotificationCenter())
    }

    /// Install the delegate and ask for permission. Main thread, at launch.
    ///
    /// Returns whether the notification centre is usable at all. A `false` here
    /// is not fatal: tasks, reminders and the launch sweep all keep working
    /// against the local database — what is lost is delivery while Orb is
    /// closed, which is exactly what [`super::warnings`] tells the user.
    pub(super) fn prepare(app: &AppHandle) -> bool {
        let Some(centre) = centre() else {
            orb_core::logger::info(
                "notifications: no bundle identifier, so reminders will not be scheduled with the OS",
            );
            return false;
        };

        // The delegate has to be in place before the app finishes launching, or
        // a click that woke Orb from not running is delivered to nobody.
        let delegate = Delegate::new(app.clone());
        centre.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        register_task_category(&centre);
        // `delegate` is a weak property, so the object has to outlive this
        // scope. There is exactly one of it and it lives as long as the
        // process, so it is leaked deliberately rather than parked in a static
        // that would have to justify being `Sync`.
        std::mem::forget(delegate);

        // Asking is idempotent: after the first answer the system returns it
        // without showing anything, so this runs on every launch rather than
        // being remembered — the user may have changed their mind in System
        // Settings, and a remembered "yes" would never notice.
        let answered = RcBlock::new(|granted: objc2::runtime::Bool, error: *mut NSError| {
            if !granted.as_bool() {
                AUTHORIZATION_REFUSED.store(true, Ordering::Relaxed);
            }
            // SAFETY: the framework hands the block a live error or null, and
            // it is only read here.
            let reason = unsafe { error.as_ref() }
                .map(|e| format!(": {}", e.localizedDescription()))
                .unwrap_or_default();
            orb_core::logger::info(&format!(
                "notifications: permission {}{reason}",
                if granted.as_bool() {
                    "granted"
                } else {
                    "refused"
                }
            ));
        });
        centre.requestAuthorizationWithOptions_completionHandler(
            // No badge: the Today count belongs in the sidebar, where it can
            // say what it is counting.
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &answered,
        );

        true
    }

    /// Whether notifications may be posted, as the notification centre says.
    ///
    /// A refusal the daemon has not settled on yet counts as one: the user has
    /// dismissed the dialogue, and saying "not asked yet" would be wrong.
    pub(super) fn permission() -> Permission {
        let Some(centre) = centre() else {
            return Permission::Unavailable;
        };

        let (tx, rx) = mpsc::channel();
        let read = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: the settings object is live for the call.
            let _ = tx.send(unsafe { settings.as_ref() }.authorizationStatus());
        });
        centre.getNotificationSettingsWithCompletionHandler(&read);

        // No answer is not a refusal: better no notice than a false one.
        let Ok(status) = rx.recv_timeout(ANSWER_TIMEOUT) else {
            return Permission::Undetermined;
        };
        if status == UNAuthorizationStatus::Denied {
            Permission::Denied
        } else if status == UNAuthorizationStatus::NotDetermined {
            if AUTHORIZATION_REFUSED.load(Ordering::Relaxed) {
                Permission::Denied
            } else {
                Permission::Undetermined
            }
        } else {
            Permission::Granted
        }
    }

    /// What the OS is holding pending, ours and everyone else's.
    ///
    /// Everything, not just `orb.task.` — the core's diff is what decides what
    /// is ours, and filtering here would mean two places had an opinion.
    pub(super) fn pending_identifiers() -> Vec<String> {
        let Some(centre) = centre() else {
            return Vec::new();
        };
        let (tx, rx) = mpsc::channel();
        let read = RcBlock::new(move |requests: NonNull<NSArray<UNNotificationRequest>>| {
            // SAFETY: the array is live for the duration of the call.
            let requests = unsafe { requests.as_ref() };
            let _ = tx.send(
                requests
                    .iter()
                    .map(|request| request.identifier().to_string())
                    .collect::<Vec<_>>(),
            );
        });
        centre.getPendingNotificationRequestsWithCompletionHandler(&read);
        rx.recv_timeout(ANSWER_TIMEOUT).unwrap_or_else(|_| {
            orb_core::logger::warn("notifications: the centre did not say what it holds pending");
            Vec::new()
        })
    }

    /// What the OS has already shown, so a delivery can be recorded (§5.2).
    pub(super) fn delivered_identifiers() -> Vec<String> {
        let Some(centre) = centre() else {
            return Vec::new();
        };
        let (tx, rx) = mpsc::channel();
        let read = RcBlock::new(move |delivered: NonNull<NSArray<UNNotification>>| {
            // SAFETY: the array is live for the duration of the call.
            let delivered = unsafe { delivered.as_ref() };
            let _ = tx.send(
                delivered
                    .iter()
                    .map(|notification| notification.request().identifier().to_string())
                    .collect::<Vec<_>>(),
            );
        });
        centre.getDeliveredNotificationsWithCompletionHandler(&read);
        rx.recv_timeout(ANSWER_TIMEOUT).unwrap_or_default()
    }

    /// The category every task reminder is posted under, which is what gives
    /// it its buttons: snooze for an hour, or until tomorrow morning.
    const TASK_CATEGORY: &str = "orb.task.reminder";

    /// The buttons a reminder carries, as `snooze-<minutes>`; "tomorrow" is
    /// worked out at the click.
    pub(super) const SNOOZE_HOUR: &str = "snooze-60";
    pub(super) const SNOOZE_TOMORROW: &str = "snooze-tomorrow";

    fn register_task_category(centre: &UNUserNotificationCenter) {
        let hour = UNNotificationAction::actionWithIdentifier_title_options(
            &NSString::from_str(SNOOZE_HOUR),
            &NSString::from_str("Snooze 1 hour"),
            UNNotificationActionOptions::empty(),
        );
        let tomorrow = UNNotificationAction::actionWithIdentifier_title_options(
            &NSString::from_str(SNOOZE_TOMORROW),
            &NSString::from_str("Snooze until tomorrow 9:00"),
            UNNotificationActionOptions::empty(),
        );
        let actions = NSArray::from_retained_slice(&[hour, tomorrow]);
        let category =
            UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                &NSString::from_str(TASK_CATEGORY),
                &actions,
                &NSArray::new(),
                UNNotificationCategoryOptions::empty(),
            );
        let categories = NSSet::from_retained_slice(&[category]);
        centre.setNotificationCategories(&categories);
    }

    /// Hand a request to the system.
    ///
    /// Adding a request whose identifier is already pending replaces it, which
    /// is what makes the core's reconcile idempotent and a repeated schedule
    /// harmless.
    pub(super) fn post(request: &Request<'_>) {
        let Some(centre) = centre() else {
            return;
        };

        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(request.title));
        content.setBody(&NSString::from_str(&request.body));
        content.setThreadIdentifier(&NSString::from_str(request.thread));
        if request.thread == super::TASK_THREAD_IDENTIFIER {
            content.setCategoryIdentifier(&NSString::from_str(TASK_CATEGORY));
        }
        content.setSound(Some(&UNNotificationSound::defaultSound()));

        let trigger = request.fire_at.and_then(|fire_at| {
            let clock = super::wall_clock(fire_at)?;
            let components = NSDateComponents::new();
            components.setYear(clock.year as isize);
            components.setMonth(clock.month as isize);
            components.setDay(clock.day as isize);
            components.setHour(clock.hour as isize);
            components.setMinute(clock.minute as isize);
            components.setSecond(clock.second as isize);
            // No time zone set, so the components are matched against the
            // machine's current calendar — the wall-clock semantics §5 asks
            // for. `repeats: false`: a recurring task advances its dates and
            // the core schedules the next occurrence, so a repeating trigger
            // would be a second, disagreeing source of truth.
            Some(
                UNCalendarNotificationTrigger::triggerWithDateMatchingComponents_repeats(
                    &components,
                    false,
                ),
            )
        });
        if request.fire_at.is_some() && trigger.is_none() {
            orb_core::logger::warn(&format!(
                "notifications: {} has an unreadable fire time, not scheduled",
                request.identifier
            ));
            return;
        }

        let native = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(request.identifier),
            &content,
            trigger.as_deref().map(|t| &**t),
        );

        let identifier = request.identifier.to_string();
        let reported = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: live error or null, read only here.
            if let Some(error) = unsafe { error.as_ref() } {
                orb_core::logger::warn(&format!(
                    "notifications: could not schedule {identifier}: {}",
                    error.localizedDescription()
                ));
            }
        });
        centre.addNotificationRequest_withCompletionHandler(&native, Some(&reported));
    }

    /// Withdraw pending requests by identifier.
    pub(super) fn cancel(identifiers: &[String]) {
        if identifiers.is_empty() {
            return;
        }
        let Some(centre) = centre() else {
            return;
        };
        let strings: Vec<Retained<NSString>> = identifiers
            .iter()
            .map(|id| NSString::from_str(id))
            .collect();
        let array = NSArray::from_retained_slice(&strings);
        centre.removePendingNotificationRequestsWithIdentifiers(&array);
    }

    define_class!(
        // SAFETY:
        // - NSObject has no subclassing requirements.
        // - `Delegate` does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "OrbTaskNotificationDelegate"]
        #[ivars = AppHandle]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            /// A reminder came due while Orb is in the foreground.
            ///
            /// Without this the system swallows it: the default for a running
            /// app is to assume it will show the news itself, which for a
            /// reminder set weeks ago is precisely wrong.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _centre: &UNUserNotificationCenter,
                notification: &UNNotification,
                completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                let identifier = notification.request().identifier().to_string();
                let app = self.ivars().clone();
                // Off the main thread: recording a delivery is a database
                // write, and AppKit is waiting on this method to return. Only
                // reminders are recorded; a new-mail banner is not a delivery
                // the core keeps books on.
                if orb_core::tasks::is_task_notification(&identifier) {
                    tauri::async_runtime::spawn_blocking(move || {
                        if let Some(orb) = app.try_state::<super::Handle>() {
                            let orb = orb.inner().clone();
                            let _ = orb.record_task_notification_delivered(identifier);
                        }
                    });
                }
                (*completion).call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }

            /// The user clicked a reminder.
            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _centre: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                completion: &block2::DynBlock<dyn Fn()>,
            ) {
                let identifier = response.notification().request().identifier().to_string();
                let action = response.actionIdentifier().to_string();
                match action.as_str() {
                    SNOOZE_HOUR => super::snoozed(self.ivars(), identifier, 60),
                    SNOOZE_TOMORROW => super::snoozed(
                        self.ivars(),
                        identifier,
                        orb_core::ffi::minutes_until_tomorrow_nine(),
                    ),
                    _ => super::opened(self.ivars(), identifier),
                }
                (*completion).call(());
            }
        }
    );

    impl Delegate {
        fn new(app: AppHandle) -> Retained<Self> {
            let this = Self::alloc().set_ivars(app);
            // SAFETY: `init` on the NSObject superclass, which is what
            // `define_class!` leaves in place for a class with no initialiser
            // of its own.
            unsafe { msg_send![super(this), init] }
        }
    }
}

/// Everything above, on a platform with no notification centre.
///
/// Orb ships for macOS, but the crate still has to build elsewhere for the sake
/// of `cargo check` on a contributor's machine, and a reminder that cannot be
/// scheduled is a warning rather than a compile error.
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{Permission, Request};
    use tauri::AppHandle;

    pub(super) fn prepare(_app: &AppHandle) -> bool {
        false
    }

    pub(super) fn permission() -> Permission {
        Permission::Unavailable
    }

    pub(super) fn pending_identifiers() -> Vec<String> {
        Vec::new()
    }

    pub(super) fn delivered_identifiers() -> Vec<String> {
        Vec::new()
    }

    pub(super) fn post(_request: &Request<'_>) {}

    pub(super) fn cancel(_identifiers: &[String]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_reads_a_local_stamp() {
        let clock = wall_clock("2026-08-06T09:30:15+02:00").expect("a valid stamp");
        assert_eq!(
            clock,
            WallClock {
                year: 2026,
                month: 8,
                day: 6,
                hour: 9,
                minute: 30,
                second: 15,
            }
        );
    }

    #[test]
    fn wall_clock_ignores_the_offset() {
        // The same wall clock in two zones must schedule at the same *local*
        // time, which is the whole point of a calendar trigger (§5).
        let here = wall_clock("2027-03-01T09:00:00+01:00").unwrap();
        let there = wall_clock("2027-03-01T09:00:00-08:00").unwrap();
        assert_eq!(here, there);
    }

    #[test]
    fn wall_clock_rejects_anything_that_is_not_one() {
        for bad in [
            "",
            "2026-08-06",
            "2026-08-06T09:30",
            "not a date at all!!!!!",
            "20260806T093015Z",
            "2026-8-06T09:30:15Z",
            "2026-08-06Tab:30:15Z",
        ] {
            assert!(wall_clock(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn days_from_civil_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        // 2000 is a leap year and 1900 is not: the case a naive /4 gets wrong.
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 29),
            1
        );
    }

    #[test]
    fn unix_seconds_honours_the_offset() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2000-01-01T00:00:00Z"), Some(946_684_800));
        // Midnight in CEST is 22:00 UTC the day before.
        assert_eq!(
            unix_seconds("2026-08-07T00:00:00+02:00"),
            Some(1_786_053_600)
        );
        // The same instant, spelled three ways.
        let z = unix_seconds("2026-08-06T22:00:00Z");
        assert_eq!(z, unix_seconds("2026-08-07T00:00:00+02:00"));
        assert_eq!(z, unix_seconds("2026-08-06T17:00:00-05:00"));
        assert_eq!(z, unix_seconds("2026-08-07T00:00:00+0200"));
    }

    #[test]
    fn unix_seconds_tolerates_fractional_seconds() {
        assert_eq!(
            unix_seconds("2026-08-06T22:00:00.123456Z"),
            unix_seconds("2026-08-06T22:00:00Z")
        );
    }

    #[test]
    fn unix_seconds_refuses_a_stamp_with_no_offset() {
        // A naive stamp would silently be read as UTC, which is a midnight
        // timer that fires at the wrong hour in every zone but one.
        assert_eq!(unix_seconds("2026-08-06T22:00:00"), None);
    }

    #[test]
    fn the_reminder_body_names_the_time() {
        assert_eq!(
            reminder_body("2026-08-06T09:30:00+02:00"),
            "Reminder · 09:30"
        );
        // A stamp the core would never produce still has to render something.
        assert_eq!(reminder_body("whenever"), "Reminder");
    }

    #[test]
    fn the_missed_summary_is_not_mistaken_for_a_reminder() {
        // A reconcile cancels everything of the core's that is no longer
        // wanted. The summary belongs to no reminder row, so it must fall
        // outside the namespace the core claims — or every launch would
        // withdraw the summary it had just posted.
        assert!(!orb_core::tasks::is_task_notification(
            MISSED_SUMMARY_IDENTIFIER
        ));
    }

    #[test]
    fn a_scheduled_identifier_names_its_task() {
        // The click path has nothing but the identifier to go on, so the
        // round-trip through the core's own parser is the contract.
        let identifier = format!(
            "{}abc-123.1786000000",
            orb_core::tasks::NOTIFICATION_ID_PREFIX
        );
        assert!(orb_core::tasks::is_task_notification(&identifier));
        let (uuid, at) = orb_core::tasks::parse_notification_id(&identifier).expect("parses");
        assert_eq!(uuid, "abc-123");
        assert_eq!(at, 1_786_000_000);
    }
}
