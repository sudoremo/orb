//! Task management.
//!
//! Tasks are stored as RFC822 messages in one designated IMAP folder — that
//! folder is the source of truth and the SQLite tables are a cache that must be
//! fully rebuildable from it. The module is laid out along that division:
//!
//! - [`model`] — the data model and the rules that govern a mutation
//!   (the two dates, the log, recurrence advancement).
//! - [`repository`] — the SQLite cache.
//! - [`rfc822`] — the `X-Orb-Task-*` wire format.
//! - [`sync`] — reconciliation between the two.
//! - [`reminders`] — scheduling and firing.
//! - [`dates`] — the fuzzy date parser behind the date picker. Pure and
//!   clock-free: it takes "today" as an argument so its behaviour is a
//!   function of its input, which is what makes a parser this fuzzy testable.
//! - [`recurrence`] — the `RRULE` subset and the arithmetic that moves a
//!   recurring task to its next occurrence.

pub mod bodies;
pub mod dates;
pub mod export;
pub mod fuzzy_recurrence;
mod model;
pub mod recurrence;
mod reminders;
mod repository;
pub(crate) mod rfc822;
mod sync;
pub mod things;

pub use dates::{
    DEFAULT_REMINDER_TIME, DateLocale, DateSuggestion, suggest_dates, suggest_datetimes,
};
pub use fuzzy_recurrence::{
    RecurrenceSuggestion, describe as describe_recurrence, suggest as suggest_recurrence,
};
pub use model::{
    CachedEmailMetadata, ChangeContext, DateField, ReminderShift, Task, TaskAttachment, TaskChange,
    TaskDates, TaskError, TaskEvent, TaskLinkedEmail, TaskListItem, TaskLogEntry, TaskReminder,
    TaskStep, TaskSyncState, TaskTag, TaskUpdates, TaskView, civil_day_from_unix, civil_day_unix,
    format_timestamp, is_wire_id, merge_logs, parse_civil_date, tag_label,
};
pub use recurrence::{
    AdvanceFrom, Advancement, ByDay, Frequency, RecurrenceEnd, RecurrenceError, RecurrenceRule,
    ReminderOffset, TaskSchedule, advance, shift_reminders,
};
pub use reminders::{
    CollectingNotifier, DueReminder, MISSED_REMINDER_GRACE_HOURS, MissedSummary, MissedTriage,
    NOTIFICATION_ID_PREFIX, NotificationHost, NotificationPlan, NotificationWindow,
    PENDING_NOTIFICATION_LIMIT, ReminderCheckResult, ReminderNotifier, ReminderService,
    ScheduledNotification, StderrNotifier, diff_notifications, is_task_notification,
    missed_reminder_grace, notification_window, parse_notification_id, triage_missed,
};
pub use repository::{
    COMPLETED_PAGE_SIZE, FixedZone, LocalZone, MutationOutcome, POSITION_STEP, PurgedTask,
    RegistryState, SystemZone, TRASH_RETENTION_DAYS, TaskIndexRow, TaskQuery, TaskRepository,
    TaskSnapshot, UndoEntry, UndoStep, UploadBacklog,
};
pub use rfc822::{
    IDENTITY_HEADERS, ParsedAttachment, ParsedTagRegistry, ParsedTask, TAG_REGISTRY_ID,
    TASK_VERSION, TaskEmailBuilder, TaskEmailParser, TaskIdentity, TaskMessage, TaskMessageKind,
};
pub use sync::{
    DEFAULT_TASKS_FOLDER, DownloadTarget, ExpungeReason, ExpungeTarget, FolderPlan,
    ListedAttachment, ListedMessage, Listing, LocalDeletion, Rebind, RegistryAction, SyncPlan,
    TaskSyncReport, TaskSyncService, UploadReason, UploadTarget, attachment_cache_root,
    cached_attachment_path, ensure_tasks_folder, plan_tasks_folder, read_listing, reconcile,
    remove_cached_attachments, set_attachment_cache_root,
};

/// The canonical spelling a task link is stored under: with angle brackets.
///
/// Links are written and read through this so that a message whose header was
/// stored bare and one whose header kept its brackets are the same link. Finding
/// the message afterwards still matches both spellings — this only settles what
/// the link's own key looks like.
pub fn canonical_message_id(rfc_message_id: &str) -> Option<String> {
    let [bracketed, _] = crate::store::message_id_variants(rfc_message_id);
    (!bracketed.is_empty()).then_some(bracketed)
}

/// A title for a task, never empty.
///
/// §2 makes the title required, and a task called nothing at all is a row the
/// user cannot find again. A message with no subject still deserves its task.
pub fn normalise_title(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "(no subject)".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The messages a text names by `message://` URL — the form Apple Mail and
/// Things write a link to a mail in — as canonical link keys, in order and
/// each once.
///
/// The URL is the message's `Message-ID` percent-encoded, angle brackets and
/// all, so the same message is found here as through any other link.
pub fn message_url_ids(text: &str) -> Vec<String> {
    let pattern = regex::Regex::new(r#"(?i)\bmessage:(?://)?([^\s<>"'/][^\s<>"']*)"#)
        .expect("a valid pattern");
    let mut ids: Vec<String> = Vec::new();
    for found in pattern.captures_iter(text) {
        let encoded = found[1].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        let Some(id) = percent_decode(encoded)
            .filter(|id| id.contains('@'))
            .as_deref()
            .and_then(canonical_message_id)
        else {
            continue;
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// `text` with its `%XX` escapes decoded; `None` where that is not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_url_names_its_message_id() {
        let notes = "See message://%3c20261021.4f2a9c%40example.com%3e.";
        assert_eq!(
            message_url_ids(notes),
            vec!["<20261021.4f2a9c@example.com>"]
        );
    }

    #[test]
    fn message_urls_are_found_in_any_spelling_and_once_each() {
        let notes = "message://%3Ca@b%3E\n(message:%3ca%40b%3e)\nMESSAGE://%3Cc@d%3E\nhttps://e.f";
        assert_eq!(message_url_ids(notes), vec!["<a@b>", "<c@d>"]);
    }

    #[test]
    fn a_message_url_without_an_id_names_nothing() {
        assert!(message_url_ids("message:// and message:%3cx%3e").is_empty());
    }
}
