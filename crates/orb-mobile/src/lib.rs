//! The Swift API of Orb: what the iOS app may call, and nothing else.
//!
//! Every method delegates to [`OrbHandle`]. A method is added here when the
//! app first needs it, so that nothing reaches Swift by accident (IOS_PLAN.md,
//! "A separate crate for the bindings").

use std::sync::Arc;

use orb_core::config::CredentialKind;
use orb_core::ffi::{
    DateSuggestionInfo, OrbError, OrbHandle, RecurrenceSuggestionInfo, StepEdit,
    TaskAttachmentInfo, TaskCatchUp, TaskDetail, TaskEdit, TaskMergePlan, TaskNotificationPlan,
    TaskSummary, TaskSyncEvent, TaskSyncListener, TaskSyncResult, TaskSyncStatus, TaskTagInfo,
    TaskViewName, UndoReport,
};
use orb_core::keychain::{Keychain, PasswordStore};
use secrecy::SecretString;

uniffi::setup_scaffolding!();

/// Hears of the task uploader's rounds; implemented in Swift.
#[uniffi::export(with_foreign)]
pub trait TaskSyncObserver: Send + Sync {
    fn on_task_sync(&self, event: TaskSyncEvent);
}

struct ObserverBridge(Arc<dyn TaskSyncObserver>);

impl TaskSyncListener for ObserverBridge {
    fn on_task_sync(&self, event: TaskSyncEvent) {
        self.0.on_task_sync(event)
    }
}

/// The account the tasks folder is on, as the setup screen asks for it.
#[derive(uniffi::Record, Debug, Clone, PartialEq)]
pub struct TasksAccountSetup {
    pub imap_server: String,
    pub imap_port: u16,
    pub imap_username: String,
    /// The folder's name, taken verbatim (todo-spec §1).
    pub folder: String,
    /// The date picker's language (§12): `en` or `de`.
    #[uniffi(default = "en")]
    pub locale: String,
}

/// What a setup code from the Mac carries (IOS_PLAN.md 2.1).
#[derive(uniffi::Record, Clone, PartialEq)]
pub struct SetupCode {
    pub setup: TasksAccountSetup,
    pub password: String,
    pub today: TodaySettings,
}

/// Written out without the password.
impl std::fmt::Debug for SetupCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupCode")
            .field("setup", &self.setup)
            .field("password", &"[redacted]")
            .field("today", &self.today)
            .finish()
    }
}

/// Read a scanned setup code, refusing anything else and a code older than
/// its five minutes.
#[uniffi::export]
pub fn read_setup_code(text: String) -> Result<SetupCode, OrbError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let payload = orb_core::setup_code::SetupPayload::decode(&text, now).map_err(|error| {
        OrbError::InvalidInput {
            message: error.to_string(),
        }
    })?;
    Ok(SetupCode {
        setup: TasksAccountSetup {
            imap_server: payload.imap_server,
            imap_port: payload.imap_port,
            imap_username: payload.imap_username,
            folder: payload.folder,
            locale: payload.locale,
        },
        password: payload.password,
        today: TodaySettings {
            keep_completed_in_today: payload.keep_completed_in_today,
            completed_first_in_today: payload.completed_first_in_today,
            archive_after_days: payload.archive_after_days,
        },
    })
}

/// What the setup screen learns from the server before anything is saved.
#[derive(uniffi::Record, Debug)]
pub struct AccountCheck {
    /// The tasks folder as the server names it, when it already exists.
    /// `None` is a folder Orb will create on its first sync — a warning, not
    /// an error: a misspelt name would otherwise start an empty task list.
    pub folder: Option<String>,
}

/// Log in to the server of `setup` and look for its tasks folder, without
/// writing anything. A refused login or an unreachable server is an error.
#[uniffi::export]
pub fn check_tasks_account(
    setup: TasksAccountSetup,
    password: String,
) -> Result<AccountCheck, OrbError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| OrbError::ConfigError {
            message: format!("could not start the check: {e}"),
        })?;
    runtime.block_on(async {
        let mut client = orb_core::imap::ImapClient::connect(
            &setup.imap_server,
            setup.imap_port,
            &setup.imap_username,
            &SecretString::from(password),
            "account check",
        )
        .await
        .map_err(|error| plainly(&setup, error))?;
        let folders = client
            .list_folders()
            .await
            .map_err(|error| plainly(&setup, error))?;
        Ok(AccountCheck {
            folder: find_folder(&folders, &setup.folder),
        })
    })
}

/// A failed check in the words of the setup screen, for the three kinds a
/// person can act on; the core's own message otherwise.
fn plainly(setup: &TasksAccountSetup, error: orb_core::Error) -> OrbError {
    use orb_core::error::FailureKind;
    let message = match error.kind() {
        FailureKind::Auth => "The server refused this username and password.".to_string(),
        FailureKind::Certificate => format!(
            "The certificate of {} could not be verified.",
            setup.imap_server
        ),
        FailureKind::Unreachable => format!(
            "{} could not be reached on port {}.",
            setup.imap_server, setup.imap_port
        ),
        _ => return OrbError::from(error),
    };
    OrbError::InvalidInput { message }
}

/// A failed task sync in the words of the app, for the three kinds a person
/// can act on; the core's own message otherwise.
fn problem_in_words(problem: &str, server: &str) -> String {
    use orb_core::error::{FailureKind, classify_text};
    match classify_text(problem) {
        FailureKind::Auth => {
            "The server refused the password. Changes wait here until it is put right.".to_string()
        }
        FailureKind::Certificate => {
            format!("The certificate of {server} could not be verified. Changes wait here.")
        }
        FailureKind::Unreachable => {
            format!("{server} cannot be reached. Changes wait here and upload once it can.")
        }
        _ => problem.to_string(),
    }
}

/// `wanted` among the server's folders: as spelt, or under a namespace such
/// as `INBOX.` that the server puts every folder in.
fn find_folder(folders: &[String], wanted: &str) -> Option<String> {
    folders
        .iter()
        .find(|f| f.as_str() == wanted)
        .or_else(|| {
            folders.iter().find(|f| {
                f.strip_suffix(wanted)
                    .and_then(|prefix| prefix.strip_suffix(['.', '/']))
                    .is_some_and(|namespace| namespace.eq_ignore_ascii_case("INBOX"))
            })
        })
        .cloned()
}

/// The name the account is given in the configuration the app writes.
const ACCOUNT_NAME: &str = "Tasks";

/// What Today shows, kept on this iPhone as each Mac keeps its own (§18,
/// §24).
#[derive(uniffi::Record, Debug, Clone, PartialEq)]
pub struct TodaySettings {
    pub keep_completed_in_today: bool,
    pub completed_first_in_today: bool,
    pub archive_after_days: u32,
}

impl Default for TodaySettings {
    fn default() -> Self {
        TodaySettings {
            keep_completed_in_today: true,
            completed_first_in_today: false,
            archive_after_days: 30,
        }
    }
}

/// Write a configuration for `setup` to `config_path`, with the database and
/// the log in `data_dir`. The password is not part of it: it goes to the
/// Keychain through [`Orb::store_password`].
#[uniffi::export(default(today = None))]
pub fn write_tasks_config(
    config_path: String,
    data_dir: String,
    setup: TasksAccountSetup,
    today: Option<TodaySettings>,
) -> Result<(), OrbError> {
    write_config(
        &config_path,
        std::path::Path::new(&data_dir),
        &setup,
        &today.unwrap_or_default(),
    )
}

fn write_config(
    config_path: &str,
    data_dir: &std::path::Path,
    setup: &TasksAccountSetup,
    today: &TodaySettings,
) -> Result<(), OrbError> {
    // A JSON string is a YAML double-quoted scalar, escapes included.
    let quote = |value: &str| serde_json::Value::from(value).to_string();
    let yaml = format!(
        "db_path: {db}\n\
         log_file: {log}\n\
         \n\
         accounts:\n\
         \x20 - name: {name}\n\
         \x20   imap_server: {server}\n\
         \x20   imap_port: {port}\n\
         \x20   imap_username: {user}\n\
         \x20   from: {user}\n\
         \n\
         tasks:\n\
         \x20 account: {name}\n\
         \x20 folder: {folder}\n\
         \x20 locale: {locale}\n\
         \x20 keep_completed_in_today: {keep}\n\
         \x20 completed_first_in_today: {first}\n\
         \x20 archive_after_days: {archive}\n",
        db = quote(&data_dir.join("orb.db").to_string_lossy()),
        log = quote(&data_dir.join("orb.log").to_string_lossy()),
        name = quote(ACCOUNT_NAME),
        server = quote(&setup.imap_server),
        port = setup.imap_port,
        user = quote(&setup.imap_username),
        folder = quote(&setup.folder),
        locale = quote(&setup.locale),
        keep = today.keep_completed_in_today,
        first = today.completed_first_in_today,
        archive = today.archive_after_days,
    );
    std::fs::create_dir_all(data_dir)
        .and_then(|()| std::fs::write(config_path, yaml))
        .map_err(|e| OrbError::ConfigError {
            message: format!("could not write {config_path}: {e}"),
        })
}

/// Orb, opened on one configuration.
#[derive(uniffi::Object)]
pub struct Orb {
    handle: Arc<OrbHandle>,
    config_path: String,
}

#[uniffi::export]
impl Orb {
    /// Open the configuration at `config_path` and the database it names.
    #[uniffi::constructor]
    pub fn open(config_path: String) -> Result<Arc<Self>, OrbError> {
        let handle = orb_core::ffi::init(config_path.clone())?;
        // Beside the database rather than under `~`, which iOS does not let
        // an app write to; and not in Caches, which iOS empties, while an
        // attachment added here lives only in this cache until it is uploaded.
        if let Some(data_dir) = std::path::Path::new(&handle.config().db_path).parent() {
            orb_core::tasks::set_attachment_cache_root(data_dir.join("attachments"));
        }
        Ok(Arc::new(Orb {
            handle,
            config_path,
        }))
    }

    /// Whether the configuration has a tasks folder at all.
    pub fn tasks_configured(&self) -> bool {
        self.handle.tasks_configured()
    }

    /// The account the tasks live in, for asking its password.
    pub fn tasks_account(&self) -> Option<String> {
        self.handle
            .config()
            .tasks
            .as_ref()
            .map(|t| t.account.clone())
    }

    pub fn today_settings(&self) -> TodaySettings {
        self.handle
            .config()
            .tasks
            .as_ref()
            .map(|tasks| TodaySettings {
                keep_completed_in_today: tasks.keep_completed_in_today,
                completed_first_in_today: tasks.completed_first_in_today,
                archive_after_days: tasks.archive_after_days,
            })
            .unwrap_or_default()
    }

    /// Keep `settings` in the configuration and apply them at once.
    pub fn set_today_settings(&self, settings: TodaySettings) -> Result<(), OrbError> {
        let config = self.handle.config();
        let not_set_up = || OrbError::ConfigError {
            message: "no tasks account is set up".to_string(),
        };
        let tasks = config.tasks.as_ref().ok_or_else(not_set_up)?;
        let account = config
            .accounts
            .iter()
            .find(|a| a.name == tasks.account)
            .ok_or_else(not_set_up)?;
        let setup = TasksAccountSetup {
            imap_server: account.imap_server.clone(),
            imap_port: account.imap_port,
            imap_username: account.imap_username.clone(),
            folder: tasks.folder.clone(),
            locale: tasks.locale.clone(),
        };
        let data_dir = std::path::Path::new(&config.db_path)
            .parent()
            .ok_or_else(not_set_up)?;
        write_config(&self.config_path, data_dir, &setup, &settings)?;
        match self.handle.reload_config().error {
            Some(message) => Err(OrbError::ConfigError { message }),
            None => Ok(()),
        }
    }

    /// The tasks account as `username @ server`.
    pub fn account_description(&self) -> Option<String> {
        let config = self.handle.config();
        let tasks = config.tasks.as_ref()?;
        let account = config.accounts.iter().find(|a| a.name == tasks.account)?;
        Some(format!(
            "{} @ {}",
            account.imap_username, account.imap_server
        ))
    }

    /// Whether the Keychain holds the IMAP password of `account`.
    pub fn has_password(&self, account: String) -> Result<bool, OrbError> {
        let key = self.credential_key(&account)?;
        Ok(Keychain.get(&key).is_some())
    }

    /// File the IMAP password of `account` in the Keychain, where the core
    /// looks for it before every login.
    pub fn store_password(&self, account: String, password: String) -> Result<(), OrbError> {
        let key = self.credential_key(&account)?;
        Keychain
            .set(&key, &SecretString::from(password))
            .map_err(|message| OrbError::ConfigError { message })
    }

    /// One view, narrowed by `filter` when there is one: title, notes and
    /// tag names, plus `tag:<name>` (§4.5).
    pub fn list_tasks(
        &self,
        view: TaskViewName,
        filter: Option<String>,
    ) -> Result<Vec<TaskSummary>, OrbError> {
        let filter = filter.filter(|f| !f.trim().is_empty());
        self.handle.list_tasks(view, filter, None)
    }

    /// Move tasks, in the order given, as one block in the global order
    /// (§4.4): after `previous` and before `next`, `None` being an end.
    pub fn move_tasks(
        &self,
        task_ids: Vec<i64>,
        previous: Option<i64>,
        next: Option<i64>,
    ) -> Result<u32, OrbError> {
        self.changed(self.handle.move_tasks(task_ids, previous, next))
    }

    /// Every task a search finds, in every view but Trash, in the order §26
    /// groups them in.
    pub fn search_tasks(&self, query: String) -> Result<Vec<TaskSummary>, OrbError> {
        self.handle.search_tasks(query)
    }

    pub fn count_tasks(&self, view: TaskViewName) -> Result<u32, OrbError> {
        self.handle.count_tasks(view, None)
    }

    pub fn working_on_tasks(&self) -> Result<Vec<TaskSummary>, OrbError> {
        self.handle.working_on_tasks()
    }

    /// A new task; `postpone_date` is `YYYY-MM-DD`.
    pub fn create_task(
        &self,
        title: String,
        postpone_date: Option<String>,
    ) -> Result<TaskSummary, OrbError> {
        let detail = self.handle.create_task(title, postpone_date)?;
        self.handle.task_changed();
        Ok(detail.task)
    }

    pub fn complete_tasks(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.changed(self.handle.complete_tasks(task_ids))
    }

    /// What merging `others` into `keep` would make, for the sheet.
    pub fn plan_task_merge(&self, keep: i64, others: Vec<i64>) -> Result<TaskMergePlan, OrbError> {
        self.handle.plan_task_merge(keep, others)
    }

    /// Merge `others` into `keep`; the others go to the Trash.
    pub fn merge_tasks(
        &self,
        keep: i64,
        others: Vec<i64>,
        title: String,
        notes: Option<String>,
        postpone_date: Option<String>,
        due_date: Option<String>,
    ) -> Result<TaskDetail, OrbError> {
        let detail =
            self.handle
                .merge_tasks(keep, others, title, notes, postpone_date, due_date)?;
        self.handle.task_changed();
        Ok(detail)
    }

    /// Hide the tasks completed so far from Today, on every device (§10.2).
    pub fn clear_completed_from_today(&self) -> Result<(), OrbError> {
        self.changed(self.handle.clear_completed_from_today().map(|()| 0))
            .map(|_| ())
    }

    pub fn uncomplete_tasks(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.changed(self.handle.uncomplete_tasks(task_ids))
    }

    pub fn trash_tasks(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.changed(self.handle.trash_tasks(task_ids))
    }

    pub fn restore_tasks(&self, task_ids: Vec<i64>) -> Result<u32, OrbError> {
        self.changed(self.handle.restore_tasks(task_ids))
    }

    /// Set the due date to `date`, `YYYY-MM-DD`; `None` clears it.
    pub fn set_due_date(&self, task_ids: Vec<i64>, date: Option<String>) -> Result<u32, OrbError> {
        self.changed(self.handle.set_task_due_date(task_ids, date))
    }

    /// Postpone to `date`, `YYYY-MM-DD`; `None` clears it.
    pub fn set_postpone_date(
        &self,
        task_ids: Vec<i64>,
        date: Option<String>,
    ) -> Result<u32, OrbError> {
        self.changed(self.handle.set_task_postpone_date(task_ids, date))
    }

    /// Mark the task as the one being worked on, or clear the mark (§19).
    pub fn set_working_on(&self, task_id: i64, on: bool) -> Result<TaskSummary, OrbError> {
        let edit = TaskEdit {
            is_working_on: Some(on),
            ..TaskEdit::default()
        };
        Ok(self.edit(task_id, edit)?.task)
    }

    /// A task with everything its detail screen shows.
    pub fn get_task(&self, task_id: i64) -> Result<TaskDetail, OrbError> {
        self.handle.get_task(task_id)
    }

    pub fn set_title(&self, task_id: i64, title: String) -> Result<TaskDetail, OrbError> {
        self.edit(
            task_id,
            TaskEdit {
                title: Some(title),
                ..TaskEdit::default()
            },
        )
    }

    /// The notes; `None` or blank clears them.
    pub fn set_notes(&self, task_id: i64, notes: Option<String>) -> Result<TaskDetail, OrbError> {
        let notes = notes.filter(|n| !n.trim().is_empty());
        self.edit(
            task_id,
            TaskEdit {
                clear_body: notes.is_none(),
                body: notes,
                ..TaskEdit::default()
            },
        )
    }

    /// Repeat by `rrule` (§8), from the completion date if
    /// `from_completion`; `None` stops repeating.
    pub fn set_recurrence(
        &self,
        task_id: i64,
        rrule: Option<String>,
        from_completion: bool,
    ) -> Result<TaskDetail, OrbError> {
        self.edit(
            task_id,
            TaskEdit {
                clear_recurrence: rrule.is_none(),
                recurrence_from_completion: rrule.as_ref().map(|_| from_completion),
                recurrence: rrule,
                ..TaskEdit::default()
            },
        )
    }

    pub fn add_step(&self, task_id: i64, title: String) -> Result<TaskDetail, OrbError> {
        self.step(task_id, StepEdit::Add { title })
    }

    /// Tick or untick steps — a selection — as one edit.
    pub fn set_steps_done(
        &self,
        task_id: i64,
        step_ids: Vec<String>,
        done: bool,
    ) -> Result<TaskDetail, OrbError> {
        self.step(
            task_id,
            StepEdit::Done {
                ids: step_ids,
                done,
            },
        )
    }

    pub fn rename_step(
        &self,
        task_id: i64,
        step_id: String,
        title: String,
    ) -> Result<TaskDetail, OrbError> {
        self.step(task_id, StepEdit::Rename { id: step_id, title })
    }

    /// Take steps — a selection — out of the checklist as one edit.
    pub fn remove_steps(
        &self,
        task_id: i64,
        step_ids: Vec<String>,
    ) -> Result<TaskDetail, OrbError> {
        self.step(task_id, StepEdit::Remove { ids: step_ids })
    }

    /// Move the steps, side by side in checklist order, to position `to`
    /// among the steps that stay where they are.
    pub fn move_steps(
        &self,
        task_id: i64,
        step_ids: Vec<String>,
        to: u32,
    ) -> Result<TaskDetail, OrbError> {
        self.step(
            task_id,
            StepEdit::Move {
                ids: step_ids,
                to: to as usize,
            },
        )
    }

    /// The tag registry (§7).
    pub fn list_tags(&self) -> Result<Vec<TaskTagInfo>, OrbError> {
        self.handle.list_task_tags()
    }

    pub fn create_tag(&self, name: String) -> Result<TaskTagInfo, OrbError> {
        let tag = self.handle.create_task_tag(name, None)?;
        self.handle.task_changed();
        Ok(tag)
    }

    /// Rename or recolour a tag; tasks carry its id, so none is touched (§7).
    pub fn update_tag(
        &self,
        tag_id: String,
        name: String,
        colour: Option<String>,
    ) -> Result<bool, OrbError> {
        let updated = self.handle.update_task_tag(tag_id, name, colour)?;
        self.handle.task_changed();
        Ok(updated)
    }

    /// Delete a tag, taking it off its tasks and never deleting them (§7).
    pub fn delete_tag(&self, tag_id: String) -> Result<u32, OrbError> {
        self.changed(self.handle.delete_task_tag(tag_id))
    }

    /// Attach the file at `path` to a task; it is copied, and uploaded with
    /// the next sync as a message of its own (§9).
    pub fn add_attachment(
        &self,
        task_id: i64,
        path: String,
    ) -> Result<TaskAttachmentInfo, OrbError> {
        let attachment = self.handle.add_task_attachment(task_id, path)?;
        self.handle.task_changed();
        Ok(attachment)
    }

    pub fn delete_attachment(&self, attachment_id: i64) -> Result<(), OrbError> {
        self.handle.delete_task_attachment(attachment_id)?;
        self.handle.task_changed();
        Ok(())
    }

    /// Where the attachment is on this iPhone, downloading it first if need
    /// be: a round trip, so off the main thread.
    pub fn attachment_path(
        &self,
        task_id: i64,
        attachment_uuid: String,
    ) -> Result<String, OrbError> {
        self.handle.task_attachment_path(task_id, attachment_uuid)
    }

    /// The task's whole set of tags.
    pub fn set_tags(&self, task_id: i64, tag_ids: Vec<String>) -> Result<u32, OrbError> {
        self.changed(self.handle.set_task_tags(task_id, tag_ids))
    }

    /// A reminder at `remind_at`, a local RFC 3339 date and time.
    pub fn add_reminder(&self, task_id: i64, remind_at: String) -> Result<i64, OrbError> {
        let id = self.handle.add_task_reminder(task_id, remind_at)?;
        self.handle.task_changed();
        Ok(id)
    }

    pub fn delete_reminder(&self, reminder_id: i64) -> Result<(), OrbError> {
        self.handle.delete_task_reminder(reminder_id)?;
        self.handle.task_changed();
        Ok(())
    }

    /// The readings of a date typed by a person (§12.1); none is "no
    /// matching date".
    pub fn suggest_dates(&self, input: String) -> Result<Vec<DateSuggestionInfo>, OrbError> {
        self.handle.suggest_dates(input)
    }

    /// As [`Orb::suggest_dates`], with a time of day, for reminders.
    pub fn suggest_datetimes(&self, input: String) -> Result<Vec<DateSuggestionInfo>, OrbError> {
        self.handle.suggest_datetimes(input)
    }

    /// The readings of a recurrence typed by a person (§8).
    pub fn suggest_recurrence(
        &self,
        input: String,
    ) -> Result<Vec<RecurrenceSuggestionInfo>, OrbError> {
        self.handle.suggest_recurrence(input)
    }

    /// What iOS should hold pending, given the identifiers it holds (§5.1).
    /// Apply it in order: cancel, then schedule.
    pub fn notification_plan(
        &self,
        pending: Vec<String>,
    ) -> Result<TaskNotificationPlan, OrbError> {
        self.handle.task_notification_plan(pending)
    }

    /// iOS showed the notification `identifier`, so the launch sweep must not
    /// show it again (§5.2).
    pub fn record_notification_delivered(&self, identifier: String) -> Result<bool, OrbError> {
        self.handle.record_task_notification_delivered(identifier)
    }

    /// The launch sweep (§5.2): the reminders to deliver now, and a summary of
    /// the older ones.
    pub fn catch_up_reminders(&self) -> Result<TaskCatchUp, OrbError> {
        self.handle.catch_up_task_reminders()
    }

    /// Snooze the reminder the notification `identifier` stands for by
    /// `minutes`; `false` when it names none still here.
    pub fn snooze_notification(&self, identifier: String, minutes: i64) -> Result<bool, OrbError> {
        let snoozed = self.handle.snooze_task_notification(identifier, minutes)?;
        self.handle.task_changed();
        Ok(snoozed)
    }

    /// Minutes from now until 09:00 tomorrow, for "Tomorrow" on a notification.
    pub fn minutes_until_tomorrow_nine(&self) -> i64 {
        orb_core::ffi::minutes_until_tomorrow_nine()
    }

    pub fn undo(&self) -> Result<Option<UndoReport>, OrbError> {
        let report = self.handle.undo()?;
        self.handle.task_changed();
        Ok(report)
    }

    /// List the tasks folder and reconcile it (todo-spec §3.3). Blocks for
    /// the length of the round trip: call it off the main thread.
    pub fn sync_tasks(&self) -> Result<TaskSyncResult, OrbError> {
        self.handle.sync_tasks()
    }

    /// Upload changes as they are made and take in other machines', telling
    /// `observer` after each round (todo-spec §23).
    pub fn start_task_uploads(&self, observer: Arc<dyn TaskSyncObserver>) {
        self.handle
            .start_task_uploads(Box::new(ObserverBridge(observer)))
    }

    /// Stop the uploader before iOS suspends the app; start it again with
    /// [`Orb::start_task_uploads`].
    pub fn stop_task_uploads(&self) {
        self.handle.stop_task_uploads()
    }

    /// Whether every change is on the server, and why not if the last sync
    /// failed — said for a person, not as the error chain.
    pub fn task_sync_status(&self) -> Result<TaskSyncStatus, OrbError> {
        let mut status = self.handle.task_sync_status()?;
        let config = self.handle.config();
        let server = config
            .tasks
            .as_ref()
            .and_then(|tasks| config.accounts.iter().find(|a| a.name == tasks.account))
            .map(|account| account.imap_server.clone())
            .unwrap_or_default();
        status.problem = status
            .problem
            .map(|problem| problem_in_words(&problem, &server));
        Ok(status)
    }

    /// The device has moved to another network (`NWPathMonitor`).
    pub fn network_changed(&self) {
        self.handle.network_changed()
    }

    /// Let go of the server for good, before the process ends.
    pub fn shutdown(&self) {
        self.handle.shutdown()
    }
}

impl Orb {
    fn credential_key(&self, account: &str) -> Result<String, OrbError> {
        let config = self.handle.config();
        let account = config
            .accounts
            .iter()
            .find(|a| a.name == account)
            .ok_or_else(|| OrbError::NotFound {
                message: format!("no account {account:?}"),
            })?;
        Ok(account.credential(CredentialKind::Imap).key())
    }

    fn edit(&self, task_id: i64, edit: TaskEdit) -> Result<TaskDetail, OrbError> {
        let detail = self.handle.update_task(task_id, edit)?;
        self.handle.task_changed();
        Ok(detail)
    }

    fn step(&self, task_id: i64, edit: StepEdit) -> Result<TaskDetail, OrbError> {
        let detail = self.handle.edit_task_step(task_id, edit)?;
        self.handle.task_changed();
        Ok(detail)
    }

    /// A change to tasks is uploaded as it is made (todo-spec §23).
    fn changed(&self, result: Result<u32, OrbError>) -> Result<u32, OrbError> {
        if result.is_ok() {
            self.handle.task_changed();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_configuration_reads_back_with_the_values_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yml");
        write_tasks_config(
            config_path.to_string_lossy().into_owned(),
            dir.path().join("data").to_string_lossy().into_owned(),
            TasksAccountSetup {
                imap_server: "imap.example.com".into(),
                imap_port: 993,
                imap_username: "me@example.com".into(),
                folder: "INBOX.Tasks \"mine\": #1".into(),
                locale: "en".into(),
            },
            None,
        )
        .unwrap();

        let config = orb_core::config::read_from_path(&config_path.to_string_lossy()).unwrap();
        assert_eq!(config.accounts[0].imap_server, "imap.example.com");
        assert_eq!(config.accounts[0].imap_username, "me@example.com");
        let tasks = config.tasks.unwrap();
        assert_eq!(tasks.account, ACCOUNT_NAME);
        assert_eq!(tasks.folder, "INBOX.Tasks \"mine\": #1");
    }

    /// The attachment cache is one per process; set it before any test opens
    /// Orb, so that it never points into a test's directory that another
    /// test is about to delete.
    fn shared_attachment_cache() {
        static CACHE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let dir = CACHE.get_or_init(|| tempfile::tempdir().unwrap());
        orb_core::tasks::set_attachment_cache_root(dir.path().to_path_buf());
    }

    fn open_offline(dir: &tempfile::TempDir) -> Arc<Orb> {
        shared_attachment_cache();
        let config_path = dir.path().join("config.yml").to_string_lossy().into_owned();
        write_tasks_config(
            config_path.clone(),
            dir.path().join("data").to_string_lossy().into_owned(),
            TasksAccountSetup {
                imap_server: "imap.invalid".into(),
                imap_port: 993,
                imap_username: "me@example.invalid".into(),
                folder: "Tasks".into(),
                locale: "en".into(),
            },
            None,
        )
        .unwrap();
        Orb::open(config_path).unwrap()
    }

    #[test]
    fn a_step_is_added_ticked_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let orb = open_offline(&dir);
        let task = orb.create_task("Pack for the trip".into(), None).unwrap();

        let detail = orb.add_step(task.id, "Passport".into()).unwrap();
        assert_eq!(detail.steps.len(), 1);
        let step = detail.steps[0].id.clone();
        let detail = orb.set_steps_done(task.id, vec![step], true).unwrap();

        assert!(detail.steps[0].done);
        assert_eq!(orb.get_task(task.id).unwrap().task.steps_done, 1);
    }

    #[test]
    fn today_settings_are_kept_and_applied_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let orb = open_offline(&dir);
        assert_eq!(orb.today_settings(), TodaySettings::default());

        let settings = TodaySettings {
            keep_completed_in_today: false,
            completed_first_in_today: true,
            archive_after_days: 7,
        };
        orb.set_today_settings(settings.clone()).unwrap();
        assert_eq!(orb.today_settings(), settings);

        let reopened = Orb::open(orb.config_path.clone()).unwrap();
        assert_eq!(reopened.today_settings(), settings);
        assert_eq!(reopened.tasks_account().as_deref(), Some(ACCOUNT_NAME));
    }

    #[test]
    fn the_tasks_folder_is_found_as_spelt_or_under_the_inbox_namespace() {
        let folders = [
            "INBOX".to_string(),
            "INBOX.Tasks".to_string(),
            "Archive".to_string(),
        ];
        assert_eq!(
            find_folder(&folders, "Tasks").as_deref(),
            Some("INBOX.Tasks")
        );
        assert_eq!(find_folder(&folders, "Archive").as_deref(), Some("Archive"));
        assert_eq!(find_folder(&folders, "Taks"), None);
        assert_eq!(find_folder(&["Work/Tasks".to_string()], "Tasks"), None);
    }

    #[test]
    fn an_unreachable_server_fails_the_check() {
        let error = check_tasks_account(
            TasksAccountSetup {
                imap_server: "imap.invalid".into(),
                imap_port: 993,
                imap_username: "me@example.invalid".into(),
                folder: "Tasks".into(),
                locale: "en".into(),
            },
            "unused".into(),
        )
        .expect_err("no such server");
        assert_eq!(
            error.to_string(),
            "imap.invalid could not be reached on port 993."
        );
    }

    #[test]
    fn an_attachment_is_kept_beside_the_database_and_opened_from_there() {
        let dir = tempfile::tempdir().unwrap();
        let orb = open_offline(&dir);
        let task = orb.create_task("Read the contract".into(), None).unwrap();
        let file = dir.path().join("contract.pdf");
        std::fs::write(&file, b"%PDF-1.4 test").unwrap();

        let attachment = orb
            .add_attachment(task.id, file.to_string_lossy().into_owned())
            .unwrap();
        let path = orb
            .attachment_path(task.id, attachment.attachment_uuid.clone())
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.4 test");
        assert_eq!(orb.get_task(task.id).unwrap().attachments.len(), 1);

        orb.delete_attachment(attachment.id).unwrap();
        assert!(orb.get_task(task.id).unwrap().attachments.is_empty());
    }

    #[test]
    fn a_sync_problem_is_said_in_words() {
        let dns = "Sync error: IO error: failed to lookup address information: nodename nor \
                   servname provided, or not known";
        assert_eq!(
            problem_in_words(dns, "imap.example.com"),
            "imap.example.com cannot be reached. Changes wait here and upload once it can."
        );
        assert!(
            problem_in_words("Login failed: AUTHENTICATIONFAILED", "x")
                .starts_with("The server refused")
        );
        assert_eq!(
            problem_in_words("Folder is read-only", "x"),
            "Folder is read-only"
        );
    }

    fn code(expires_in: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        orb_core::setup_code::SetupPayload::new(
            "imap.example.com".into(),
            993,
            "me@example.com".into(),
            "s3cret".into(),
            "Orb Tasks".into(),
            "de".into(),
            false,
            true,
            14,
            now + expires_in - orb_core::setup_code::LIFETIME_SECONDS,
        )
        .encode()
    }

    #[test]
    fn a_setup_code_reads_as_the_setup_it_carries() {
        let read = read_setup_code(code(60)).unwrap();
        assert_eq!(read.setup.imap_server, "imap.example.com");
        assert_eq!(read.setup.folder, "Orb Tasks");
        assert_eq!(read.setup.locale, "de");
        assert_eq!(read.password, "s3cret");
        assert_eq!(
            read.today,
            TodaySettings {
                keep_completed_in_today: false,
                completed_first_in_today: true,
                archive_after_days: 14,
            }
        );

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yml").to_string_lossy().into_owned();
        write_tasks_config(
            config_path.clone(),
            dir.path().join("data").to_string_lossy().into_owned(),
            read.setup,
            Some(read.today.clone()),
        )
        .unwrap();
        let config = orb_core::config::read_from_path(&config_path).unwrap();
        let tasks = config.tasks.unwrap();
        assert_eq!(tasks.locale, "de");
        assert!(tasks.completed_first_in_today);
        assert_eq!(tasks.archive_after_days, 14);
    }

    #[test]
    fn an_expired_or_foreign_code_is_refused_in_words() {
        let expired = read_setup_code(code(-1)).unwrap_err();
        assert!(expired.to_string().contains("expired"), "{expired}");
        let foreign = read_setup_code("https://example.com".into()).unwrap_err();
        assert!(
            foreign.to_string().contains("not an Orb setup code"),
            "{foreign}"
        );
    }
}
