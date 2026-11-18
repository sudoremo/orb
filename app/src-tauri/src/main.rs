//! Orb's desktop app: a web UI driven directly by `orb_core`.
//!
//! Every command here is a thin shim over the `OrbHandle` API. Nothing about
//! the sync engine, store or IMAP layer lives here — this is the presentation
//! layer and its bridge, and that separation is what makes the core testable
//! without a UI at all.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use orb_core::draft::DraftTemplate;
use std::sync::atomic::{AtomicI64, Ordering};

use orb_core::ffi::{
    Account, AttachmentData, AttachmentMeta, BindingsConfig, BodyBackfillResult, BoxFuture,
    ConfigListener, ConfigReload, DateSuggestionInfo, FolderInfo, InlineImage, LinkedMessage,
    MessageDetail, MessageHeader, MessageSortOrder, MessageTaskCount, OrbError, OrbHandle,
    PasswordAnswer, PasswordPrompter, PasswordRequest, PendingActionInfo, PendingActionType,
    PendingFolderCount, PendingScope, PushListener, SearchResult, SearchScope, SendMailInput,
    SendMailResult, SyncProgressListener, SyncProgressUpdate, SyncResult, TaskAttachmentInfo,
    TaskCatchUp, TaskDetail, TaskEdit, TaskLinkInfo, TaskLogItem, TaskMergePlan,
    TaskNotificationPlan, TaskReminderInfo, TaskSummary, TaskSyncEvent, TaskSyncListener,
    TaskSyncResult, TaskSyncStatus, TaskTagInfo, TaskViewName, ThreadPlace, UiSettings, UndoReport,
};
use orb_core::services::AccountHealth;
use orb_core::store::{DeleteOutcome, PendingExecution, WriteOutcome};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

type Handle = Arc<OrbHandle>;

/// The config file was re-read; the payload is a [`ConfigReload`].
const CONFIG_RELOADED_EVENT: &str = "config-reloaded";

/// A login needs a password nobody has; the payload is a [`PasswordPrompt`].
const PASSWORD_NEEDED_EVENT: &str = "password-needed";

/// What the window shows to ask for a password, and the id the answer
/// comes back under.
#[derive(Clone, serde::Serialize)]
struct PasswordPrompt {
    id: u64,
    account: String,
    kind: String,
    username: String,
    server: String,
    retry: bool,
}

/// The prompts waiting for an answer. A prompt is answered by the
/// `answer_password` command with the id it was shown under; a window that
/// opens after the prompt was raised asks for what is pending.
#[derive(Default)]
struct PasswordPrompts {
    next_id: std::sync::atomic::AtomicU64,
    pending: std::sync::Mutex<
        Vec<(
            PasswordPrompt,
            tokio::sync::oneshot::Sender<Option<PasswordAnswer>>,
        )>,
    >,
}

/// Asks for a password in whichever window has the keyboard, falling back to
/// the main window; the core waits on the answer.
struct WindowPrompter {
    app: AppHandle,
}

impl PasswordPrompter for WindowPrompter {
    fn ask(&self, request: PasswordRequest) -> BoxFuture<'static, Option<PasswordAnswer>> {
        let prompts = self.app.state::<Arc<PasswordPrompts>>().inner().clone();
        let prompt = PasswordPrompt {
            id: prompts.next_id.fetch_add(1, Ordering::Relaxed),
            account: request.account,
            kind: request.kind,
            username: request.username,
            server: request.server,
            retry: request.retry,
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        prompts
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((prompt.clone(), tx));

        let target = self
            .app
            .webview_windows()
            .into_values()
            .find(|w| w.is_focused().unwrap_or(false))
            .or_else(|| self.app.get_webview_window("main"));
        match target {
            Some(window) => {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.emit(PASSWORD_NEEDED_EVENT, prompt);
            }
            None => {
                let _ = self.app.emit(PASSWORD_NEEDED_EVENT, prompt);
            }
        }
        Box::pin(async move { rx.await.ok().flatten() })
    }
}

/// The prompts nobody has answered yet, for a window that opened after they
/// were raised.
#[tauri::command]
fn pending_password_prompts(prompts: State<'_, Arc<PasswordPrompts>>) -> Vec<PasswordPrompt> {
    prompts
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(prompt, _)| prompt.clone())
        .collect()
}

/// Answer a prompt. `password` is `None` when it was dismissed; `store` asks
/// for the password to be kept in the Keychain.
#[tauri::command]
fn answer_password(
    prompts: State<'_, Arc<PasswordPrompts>>,
    id: u64,
    password: Option<String>,
    store: bool,
) {
    let mut pending = prompts.pending.lock().unwrap_or_else(|e| e.into_inner());
    let Some(at) = pending.iter().position(|(prompt, _)| prompt.id == id) else {
        return;
    };
    let (_, tx) = pending.remove(at);
    let _ = tx.send(password.map(|password| PasswordAnswer {
        password: password.into(),
        store,
    }));
}

/// Tells every window the config was re-read, so bindings and settings are
/// fetched afresh and the status line says what, if anything, needs a restart.
struct ConfigBridge {
    app: AppHandle,
}

impl ConfigListener for ConfigBridge {
    fn on_config_reloaded(&self, outcome: ConfigReload) {
        let _ = self.app.emit(CONFIG_RELOADED_EVENT, outcome);
        if let Some(orb) = self.app.try_state::<Handle>() {
            update_badge_later(&self.app, orb.inner());
        }
    }
}

/// An account's standing changed, or a sync began or ended; the sidebar
/// re-reads the statuses.
const ACCOUNT_STATUS_EVENT: &str = "account-status";

/// Put the unread counts `dock_badges` asks for on the Dock — the inboxes'
/// when it asks for none — or take them off at zero.
pub(crate) fn update_badge(app: &AppHandle, orb: &Handle) {
    let badges = match orb.dock_badges() {
        Ok(Some(counts)) => dock::Badges::Drawn(counts),
        Ok(None) => dock::Badges::Native(
            orb.inbox_unread_count().unwrap_or(0) + orb.chat_badge_count().unwrap_or(0),
        ),
        Err(error) => {
            orb_core::logger::warn(&format!("dock: the counts could not be read: {error}"));
            return;
        }
    };
    dock::show(app, badges);
}

/// Update the badge after the command has answered: the window waits for
/// the answer, and the badge is not what it is waiting for.
fn update_badge_later(app: &AppHandle, orb: &Handle) {
    let (app, orb) = (app.clone(), orb.clone());
    tauri::async_runtime::spawn_blocking(move || update_badge(&app, &orb));
}

/// Everything a finished sync changes besides the list: the badge, the
/// notifications and the account's standing. `new_mail` is what the sync
/// itself reports, after its filter rules ran: a message a rule filed away is
/// not in it, and a sync announces only the mail it brought, so two syncs
/// ending close together never announce the same message.
fn after_sync(app: &AppHandle, orb: &Handle, new_mail: &[orb_core::ffi::NewMailItem]) {
    update_badge(app, orb);
    notifications::notify_new_mail(new_mail);
    let _ = app.emit(ACCOUNT_STATUS_EVENT, ());
}

/// What a sync command's answer says arrived, or nothing when it failed.
fn new_mail_of(result: &Result<SyncResult, OrbError>) -> &[orb_core::ffi::NewMailItem] {
    result
        .as_ref()
        .map(|synced| synced.new_mail.as_slice())
        .unwrap_or_default()
}

/// Run a blocking `orb_core` call off the webview thread.
///
/// Every `OrbHandle` method blocks on the core's own tokio runtime, so invoking
/// one straight from a command would stall the UI for the length of an IMAP
/// round-trip. `spawn_blocking` moves it to the blocking pool, where entering
/// another runtime is permitted.
async fn offload<T, F>(label: &'static str, f: F) -> Result<T, OrbError>
where
    F: FnOnce() -> Result<T, OrbError> + Send + 'static,
    T: Send + 'static,
{
    // Log both ends: without a visible status bar this is the only way to tell
    // "the command never fired" from "the command fired and hung".
    orb_core::logger::info(&format!("cmd {label}: start"));
    let started = std::time::Instant::now();
    // A panic inside the closure arrives as a join error. Propagating it
    // without a line here made a crashed command indistinguishable from a
    // hung one.
    let result = match tauri::async_runtime::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => {
            orb_core::logger::warn(&format!("cmd {label}: worker thread failed: {e}"));
            record_job(label, started, Some(format!("worker thread failed: {e}")));
            return Err(OrbError::SyncError {
                message: format!("worker thread failed: {e}"),
            });
        }
    };
    match &result {
        Ok(_) => orb_core::logger::info(&format!("cmd {label}: ok")),
        Err(e) => orb_core::logger::warn(&format!("cmd {label}: {e}")),
    }
    record_job(label, started, result.as_ref().err().map(|e| e.to_string()));
    result
}

/// One command the shell ran for the frontend, for the console.
#[derive(Debug, Clone, serde::Serialize)]
struct JobRecord {
    label: String,
    /// When it started, as seconds since the epoch.
    started_unix: u64,
    took_ms: u64,
    error: Option<String>,
}

/// Jobs the console lists; the paging of the list is left out, since a
/// scroll would drown everything else in `messages`.
const JOBS_KEPT: usize = 200;

static JOBS: std::sync::Mutex<std::collections::VecDeque<JobRecord>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

fn record_job(label: &str, started: std::time::Instant, error: Option<String>) {
    // The commands the list polls on every scroll and redraw would drown the
    // rest; they are kept only when they failed.
    if matches!(
        label,
        "messages"
            | "message"
            | "highlight_badges"
            | "pending_counts"
            | "thread_total"
            | "inbox_thread_total"
            | "inbox_ranks"
            | "highlight_terms"
            | "count_tasks"
            | "today_task_count"
            | "folders"
            | "virtual_folders"
            | "pins"
            | "folder_uids"
            | "open_task_counts"
            | "attachments"
            | "inline_images"
    ) && error.is_none()
    {
        return;
    }
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    if jobs.len() == JOBS_KEPT {
        jobs.pop_front();
    }
    let began = std::time::SystemTime::now()
        .checked_sub(started.elapsed())
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or(0);
    jobs.push_back(JobRecord {
        label: label.to_string(),
        started_unix: began,
        took_ms: started.elapsed().as_millis() as u64,
        error,
    });
}

/// The commands the shell ran lately, newest first, with what went wrong.
#[tauri::command]
async fn recent_jobs() -> Result<Vec<JobRecord>, OrbError> {
    let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    Ok(jobs.iter().rev().cloned().collect())
}

/// The last lines of the log, oldest first, and where the log is written.
#[tauri::command]
async fn log_recent(
    orb: State<'_, Handle>,
    lines: u32,
) -> Result<(Vec<String>, Option<String>), OrbError> {
    let orb = orb.inner().clone();
    Ok((orb.log_recent(lines), orb.log_path()))
}

/// A line from the window into the log: what the shell cannot time, such as
/// how long the window took to draw an answer.
#[tauri::command]
async fn log_frontend(message: String) -> Result<(), OrbError> {
    orb_core::logger::info(&format!("window: {message}"));
    Ok(())
}

/// Open the window that shows tasks only, for a second screen beside the
/// mailbox, or bring the open one forward.
///
/// The mailbox's own page, which reads its label and leaves the mail out.
/// Tabbed apart from the mailbox, since the point is to see both at once, and
/// its frame is remembered like the mailbox's.
#[tauri::command]
async fn open_tasks_window(app: tauri::AppHandle) -> Result<(), OrbError> {
    if let Some(open) = app.get_webview_window("tasks") {
        let _ = open.unminimize();
        let _ = open.show();
        let _ = open.set_focus();
        return Ok(());
    }
    let window = tauri::WebviewWindowBuilder::new(&app, "tasks", tauri::WebviewUrl::default())
        .title("Tasks")
        .inner_size(1100.0, 760.0)
        .min_inner_size(520.0, 360.0)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .tabbing_identifier("orb-tasks")
        .transparent(true)
        .effects(
            tauri::window::EffectsBuilder::new()
                .effect(tauri::window::Effect::Sidebar)
                .state(tauri::window::EffectState::FollowsWindowActiveState)
                .build(),
        )
        .initialization_script("document.documentElement.dataset.vibrant = '';")
        .on_navigation(navigation_guard(&app, "tasks"))
        .build()
        .map_err(|e| OrbError::ConfigError {
            message: format!("could not open the tasks window: {e}"),
        })?;
    #[cfg(target_os = "macos")]
    give_toolbar(&app, &window);
    #[cfg(not(target_os = "macos"))]
    let _ = window;
    Ok(())
}

/// Open the window that shows chat only, for a second screen, or bring the
/// open one forward (PLAN.md 15.9).
///
/// The mailbox's own page again, which reads its label and shows Waiting
/// and the rooms; the frame is remembered as the tasks window's is.
#[tauri::command]
async fn open_chat_window(app: tauri::AppHandle) -> Result<(), OrbError> {
    if let Some(open) = app.get_webview_window("chat") {
        let _ = open.unminimize();
        let _ = open.show();
        let _ = open.set_focus();
        return Ok(());
    }
    let window = tauri::WebviewWindowBuilder::new(&app, "chat", tauri::WebviewUrl::default())
        .title("Chat")
        .inner_size(1000.0, 800.0)
        .min_inner_size(520.0, 360.0)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .tabbing_identifier("orb-chat")
        .transparent(true)
        .effects(
            tauri::window::EffectsBuilder::new()
                .effect(tauri::window::Effect::Sidebar)
                .state(tauri::window::EffectState::FollowsWindowActiveState)
                .build(),
        )
        .initialization_script("document.documentElement.dataset.vibrant = '';")
        .on_navigation(navigation_guard(&app, "chat"))
        .build()
        .map_err(|e| OrbError::ConfigError {
            message: format!("could not open the chat window: {e}"),
        })?;
    #[cfg(target_os = "macos")]
    give_toolbar(&app, &window);
    #[cfg(not(target_os = "macos"))]
    let _ = window;
    Ok(())
}

/// Open a message or a task in a window of its own, or bring forward the one
/// already showing it.
///
/// The mailbox's page again, which reads from the label what to show and
/// leaves out the sidebar and the list. One window per item, so the label is
/// the item: `message-<id>` or `task-<id>`. Not a tab of the mailbox, since
/// the point is to see it beside the list it came from.
#[tauri::command]
async fn open_item_window(
    app: tauri::AppHandle,
    kind: String,
    id: i64,
    title: String,
) -> Result<(), OrbError> {
    if kind != "message" && kind != "task" {
        return Err(OrbError::ConfigError {
            message: format!("no window shows a {kind}"),
        });
    }
    let label = format!("{kind}-{id}");
    if let Some(open) = app.get_webview_window(&label) {
        let _ = open.unminimize();
        let _ = open.show();
        let _ = open.set_focus();
        return Ok(());
    }
    let (width, height) = if kind == "message" {
        (860.0, 900.0)
    } else {
        (640.0, 720.0)
    };
    let window =
        tauri::WebviewWindowBuilder::new(&app, label.clone(), tauri::WebviewUrl::default())
            .title(title)
            .inner_size(width, height)
            .min_inner_size(420.0, 320.0)
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true)
            .tabbing_identifier(&label)
            .on_navigation(navigation_guard(&app, &label))
            .build()
            .map_err(|e| OrbError::ConfigError {
                message: format!("could not open a window for the {kind}: {e}"),
            })?;
    #[cfg(target_os = "macos")]
    give_toolbar(&app, &window);
    #[cfg(not(target_os = "macos"))]
    let _ = window;
    Ok(())
}

/// Bring the mailbox forward, for the tasks window when it opens a message.
#[tauri::command]
async fn show_mailbox(app: tauri::AppHandle) -> Result<(), OrbError> {
    show_main_window(&app);
    Ok(())
}

/// Open the console — the log as it is written, and the recent jobs — as a
/// tab beside the mailbox, or bring the open one forward.
#[tauri::command]
async fn open_console(app: tauri::AppHandle) -> Result<(), OrbError> {
    if let Some(open) = app.get_webview_window("console") {
        let _ = open.set_focus();
        return Ok(());
    }
    let host = app.get_webview_window("main");
    let window = tauri::WebviewWindowBuilder::new(
        &app,
        "console",
        tauri::WebviewUrl::App("console.html".into()),
    )
    .title("Console")
    .inner_size(900.0, 520.0)
    .tabbing_identifier("orb")
    .on_navigation(navigation_guard(&app, "console"))
    .build()
    .map_err(|e| OrbError::ConfigError {
        message: format!("could not open the console: {e}"),
    })?;
    #[cfg(target_os = "macos")]
    if let Some(sibling) = host {
        let joining = window.clone();
        if let Err(error) = app.run_on_main_thread(move || add_as_tab(&sibling, &joining)) {
            orb_core::logger::warn(&format!("could not reach the main thread to tab: {error}"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (host, window);
    Ok(())
}

mod accessibility;
mod chat;
mod contacts;
mod dock;
mod notifications;

#[tauri::command]
async fn accounts(orb: State<'_, Handle>) -> Result<Vec<Account>, OrbError> {
    let orb = orb.inner().clone();
    offload("accounts", move || orb.get_accounts()).await
}

/// Configuration problems the user should know about at startup.
///
/// The core's own list, plus whatever the platform can only find out by asking
/// the system. A refused notification permission is the example that matters:
/// §5.1 calls a silently unpermitted reminder the worst possible failure here,
/// and the core cannot see it — only the notification centre knows, and only
/// this process can ask.
#[tauri::command]
async fn config_warnings(orb: State<'_, Handle>) -> Result<Vec<String>, OrbError> {
    let orb = orb.inner().clone();
    offload("config_warnings", move || {
        let mut warnings = orb.config_warnings()?;
        warnings.extend(notifications::warnings());
        Ok(warnings)
    })
    .await
}

/// Whether reminders can reach the notification centre, for the notice that
/// says so where reminders are set.
#[tauri::command]
async fn notification_permission() -> Result<notifications::Permission, OrbError> {
    offload(
        "notification_permission",
        || Ok(notifications::permission()),
    )
    .await
}

/// Open System Settings at Orb's notification entry, the one place a refused
/// permission can be granted again.
#[tauri::command]
fn open_notification_settings(app: AppHandle) {
    notifications::open_settings(&app);
}

/// Hand a Ctrl+H/J/K/L Orb has no use for to `navigation.escalate_command`.
#[tauri::command]
async fn escalate(orb: State<'_, Handle>, direction: String) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("escalate", move || orb.escalate(&direction)).await
}

/// Settings the frontend has to honour, such as the mark-read delay.
#[tauri::command]
async fn ui_settings(orb: State<'_, Handle>) -> Result<UiSettings, OrbError> {
    let orb = orb.inner().clone();
    offload("ui_settings", move || Ok(orb.ui_settings())).await
}

/// The message exactly as the server has it, headers and all.
#[tauri::command]
async fn raw_message(orb: State<'_, Handle>, id: i64) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("raw_message", move || orb.raw_message(id)).await
}

#[tauri::command]
async fn folders(orb: State<'_, Handle>) -> Result<Vec<FolderInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("folders", move || orb.get_all_folders()).await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn messages(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    offset: u32,
    limit: u32,
    sort: Option<MessageSortOrder>,
    threads: Option<bool>,
    expanded: Option<Vec<String>>,
) -> Result<Vec<MessageHeader>, OrbError> {
    let orb = orb.inner().clone();
    offload("messages", move || {
        if threads.unwrap_or(false) {
            orb.get_threads(account, folder, offset, limit, expanded.unwrap_or_default())
        } else {
            orb.get_messages(account, folder, offset, limit, sort)
        }
    })
    .await
}

/// The row a message is on in its folder's list, for opening its page.
#[tauri::command]
async fn message_row(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    id: i64,
    sort: Option<MessageSortOrder>,
) -> Result<Option<u32>, OrbError> {
    let orb = orb.inner().clone();
    offload("message_row", move || {
        orb.message_row(account, folder, id, sort)
    })
    .await
}

/// The row a message is on in several folders read as one list.
#[tauri::command]
async fn inbox_message_row(
    orb: State<'_, Handle>,
    folders: Vec<(String, String)>,
    id: i64,
    sort: Option<MessageSortOrder>,
) -> Result<Option<u32>, OrbError> {
    let orb = orb.inner().clone();
    offload("inbox_message_row", move || {
        orb.inbox_message_row(folders, id, sort)
    })
    .await
}

/// Where a message stands in a threaded list, its thread expanded.
#[tauri::command]
async fn thread_row(
    orb: State<'_, Handle>,
    folders: Vec<(String, String)>,
    id: i64,
    expanded: Vec<String>,
) -> Result<Option<ThreadPlace>, OrbError> {
    let orb = orb.inner().clone();
    offload("thread_row", move || orb.thread_row(folders, id, expanded)).await
}

/// A page of several folders as one list: every account's inbox, for Today.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn inbox_messages(
    orb: State<'_, Handle>,
    folders: Vec<(String, String)>,
    offset: u32,
    limit: u32,
    sort: Option<MessageSortOrder>,
    threads: Option<bool>,
    expanded: Option<Vec<String>>,
) -> Result<Vec<MessageHeader>, OrbError> {
    let orb = orb.inner().clone();
    offload("inbox_messages", move || {
        if threads.unwrap_or(false) {
            orb.get_inbox_threads(folders, offset, limit, expanded.unwrap_or_default())
        } else {
            orb.get_inbox_messages(folders, offset, limit, sort)
        }
    })
    .await
}

/// How many rows the threaded list of several folders has.
#[tauri::command]
async fn inbox_thread_total(
    orb: State<'_, Handle>,
    folders: Vec<(String, String)>,
    expanded: Option<Vec<String>>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    offload("inbox_thread_total", move || {
        orb.inbox_thread_total(folders, expanded.unwrap_or_default())
    })
    .await
}

/// Where items dated `times` (unix seconds) stand among Today's inbox rows.
#[tauri::command]
async fn inbox_ranks(
    orb: State<'_, Handle>,
    folders: Vec<(String, String)>,
    times: Vec<i64>,
    newest_first: bool,
    threads: Option<bool>,
    expanded: Option<Vec<String>>,
) -> Result<Vec<u32>, OrbError> {
    let orb = orb.inner().clone();
    offload("inbox_ranks", move || {
        orb.inbox_ranks(
            folders,
            times,
            newest_first,
            threads.unwrap_or(false),
            expanded.unwrap_or_default(),
        )
    })
    .await
}

/// How many messages a folder holds now.
#[tauri::command]
async fn folder_total(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    offload("folder_total", move || orb.folder_total(account, folder)).await
}

/// The web addresses a message points to.
#[tauri::command]
async fn message_links(
    orb: State<'_, Handle>,
    id: i64,
) -> Result<Vec<orb_core::message_links::MessageLink>, OrbError> {
    let orb = orb.inner().clone();
    offload("message_links", move || orb.message_links(id)).await
}

/// The web addresses a task points to, the first to open first.
#[tauri::command]
async fn task_links(
    orb: State<'_, Handle>,
    id: i64,
) -> Result<Vec<orb_core::message_links::MessageLink>, OrbError> {
    let orb = orb.inner().clone();
    offload("task_links", move || orb.task_links(id)).await
}

/// How many rows the threaded list of a folder has.
#[tauri::command]
async fn thread_total(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    expanded: Option<Vec<String>>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    offload("thread_total", move || {
        orb.thread_total(account, folder, expanded.unwrap_or_default())
    })
    .await
}

/// The whole conversation a message is part of, across the account.
#[tauri::command]
async fn thread_messages(orb: State<'_, Handle>, id: i64) -> Result<Vec<SearchResult>, OrbError> {
    let orb = orb.inner().clone();
    offload("thread_messages", move || orb.thread_messages(id)).await
}

/// The members of threads within the folders a threaded list is read from,
/// oldest first: what a collapsed thread's row stands for.
#[tauri::command]
async fn thread_members(
    orb: State<'_, Handle>,
    folders: Vec<(String, String)>,
    threads: Vec<String>,
) -> Result<Vec<MessageHeader>, OrbError> {
    let orb = orb.inner().clone();
    offload("thread_members", move || {
        orb.thread_members(folders, threads)
    })
    .await
}

/// Search the mailbox with the query language: all of it, or the folders,
/// the folder or the account `scope` names.
#[tauri::command]
async fn search(
    orb: State<'_, Handle>,
    query: String,
    sort: Option<MessageSortOrder>,
    scope: Option<SearchScope>,
) -> Result<Vec<SearchResult>, OrbError> {
    let orb = orb.inner().clone();
    offload("search", move || {
        orb.search_messages(query, scope.unwrap_or(SearchScope::All), sort)
    })
    .await
}

/// The messages of the list on screen a filter keeps, from these folders.
#[tauri::command]
async fn filter_messages(
    orb: State<'_, Handle>,
    query: String,
    folders: Vec<(String, String)>,
    sort: Option<MessageSortOrder>,
) -> Result<Vec<SearchResult>, OrbError> {
    let orb = orb.inner().clone();
    offload("filter_messages", move || {
        orb.filter_messages(query, folders, sort)
    })
    .await
}

#[tauri::command]
async fn message(orb: State<'_, Handle>, id: i64) -> Result<MessageDetail, OrbError> {
    let orb = orb.inner().clone();
    offload("message", move || orb.get_message_stored_first(id)).await
}

/// The rest of a message `message` answered from what is stored, from the
/// server.
#[tauri::command]
async fn message_complete(orb: State<'_, Handle>, id: i64) -> Result<MessageDetail, OrbError> {
    let orb = orb.inner().clone();
    offload("message_complete", move || orb.get_message(id)).await
}

#[tauri::command]
async fn inline_images(orb: State<'_, Handle>, id: i64) -> Result<Vec<InlineImage>, OrbError> {
    let orb = orb.inner().clone();
    offload("inline_images", move || orb.inline_images(id)).await
}

#[tauri::command]
async fn mark_read(
    app: AppHandle,
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    uids: Vec<u32>,
    read: bool,
    undo: Option<String>,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    offload("mark_read", move || {
        orb.mark_read(account, folder, uids, read, undo)
    })
    .await?;
    update_badge_later(&app, &badge);
    Ok(())
}

#[tauri::command]
async fn mark_flagged(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    uids: Vec<u32>,
    flagged: bool,
    undo: Option<String>,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("mark_flagged", move || {
        orb.mark_flagged(account, folder, uids, flagged, undo)
    })
    .await
}

/// Stage a destructive action locally; nothing reaches the server until
/// `execute_pending_actions` flushes it.
#[tauri::command]
async fn stage_action(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    uids: Vec<u32>,
    action: PendingActionType,
    target: Option<String>,
    undo: Option<String>,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("stage_action", move || {
        orb.stage_action(account, folder, uids, action, target, undo)
    })
    .await
}

/// Every UID in a folder, so a whole-folder selection can act without reading
/// the folder.
#[tauri::command]
async fn folder_uids(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
) -> Result<Vec<u32>, OrbError> {
    let orb = orb.inner().clone();
    offload("folder_uids", move || orb.folder_uids(account, folder)).await
}

#[tauri::command]
async fn unstage_action(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    uids: Vec<u32>,
    undo: Option<String>,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("unstage_action", move || {
        orb.unstage_action(account, folder, uids, undo)
    })
    .await
}

/// Flush staged actions for one folder. Returns how many were applied, and
/// the warnings for what the list does not show yet.
#[tauri::command]
async fn execute_pending(
    app: AppHandle,
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    undo: Option<String>,
) -> Result<PendingExecution, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = offload("execute_pending", move || {
        orb.execute_pending_actions(
            PendingScope::Folder {
                account_name: account,
                folder_name: folder,
            },
            undo,
        )
    })
    .await;
    update_badge_later(&app, &badge);
    result
}

/// Flush every staged action in every folder of every account.
#[tauri::command]
async fn execute_pending_all(
    app: AppHandle,
    orb: State<'_, Handle>,
    undo: Option<String>,
) -> Result<PendingExecution, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = offload("execute_pending_all", move || {
        orb.execute_pending_actions(PendingScope::All, undo)
    })
    .await;
    update_badge_later(&app, &badge);
    result
}

/// The sidebar's views: saved searches and merged views, as queries.
#[tauri::command]
async fn virtual_folders(
    orb: State<'_, Handle>,
) -> Result<Vec<orb_core::ffi::VirtualFolderInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("virtual_folders", move || Ok(orb.virtual_folders())).await
}

/// The top of the sidebar, in the order `pinned` gives.
#[tauri::command]
async fn pins(orb: State<'_, Handle>) -> Result<Vec<orb_core::ffi::PinInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("pins", move || Ok(orb.pins())).await
}

/// Copy messages to another folder of the same account.
#[tauri::command]
async fn copy_messages(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    target: String,
    uids: Vec<u32>,
    undo: Option<String>,
) -> Result<WriteOutcome, OrbError> {
    let orb = orb.inner().clone();
    offload("copy_messages", move || {
        orb.copy_messages(account, folder, target, uids, undo)
    })
    .await
}

/// Move or copy messages into a folder of another account.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn transfer_messages(
    app: AppHandle,
    orb: State<'_, Handle>,
    from_account: String,
    from_folder: String,
    to_account: String,
    to_folder: String,
    uids: Vec<u32>,
    keep_source: bool,
    undo: Option<String>,
) -> Result<orb_core::store::TransferOutcome, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = offload("transfer_messages", move || {
        orb.transfer_messages(
            from_account,
            from_folder,
            to_account,
            to_folder,
            uids,
            keep_source,
            undo,
        )
    })
    .await;
    update_badge_later(&app, &badge);
    result
}

/// Create a folder on the server.
#[tauri::command]
async fn create_folder(
    orb: State<'_, Handle>,
    account: String,
    name: String,
) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("create_folder", move || orb.create_folder(account, name)).await
}

/// Rename a folder, on the server and here.
#[tauri::command]
async fn rename_folder(
    orb: State<'_, Handle>,
    account: String,
    from: String,
    to: String,
) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("rename_folder", move || {
        orb.rename_folder(account, from, to)
    })
    .await
}

/// Delete a folder with its messages, on the server and here.
#[tauri::command]
async fn delete_folder(
    orb: State<'_, Handle>,
    account: String,
    name: String,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    offload("delete_folder", move || orb.delete_folder(account, name)).await
}

/// Subscribe to a folder, or unsubscribe from it.
#[tauri::command]
async fn set_folder_subscribed(
    orb: State<'_, Handle>,
    account: String,
    name: String,
    subscribed: bool,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("set_folder_subscribed", move || {
        orb.set_folder_subscribed(account, name, subscribed)
    })
    .await
}

/// Run the filter rules on `uids` of a folder, or on all of it.
#[tauri::command]
async fn run_filters(
    app: AppHandle,
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    uids: Option<Vec<u32>>,
    undo: Option<String>,
) -> Result<orb_core::store::FilterOutcome, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = offload("run_filters", move || {
        orb.run_filters(account, folder, uids, undo)
    })
    .await;
    update_badge_later(&app, &badge);
    result
}

/// Run the filter rules on every folder some rule watches.
#[tauri::command]
async fn run_filters_all(
    app: AppHandle,
    orb: State<'_, Handle>,
    undo: Option<String>,
) -> Result<orb_core::store::FilterOutcome, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = offload("run_filters_all", move || orb.run_filters_everywhere(undo)).await;
    update_badge_later(&app, &badge);
    result
}

/// What the filter rules did in each account's last run, for the review panel.
#[tauri::command]
async fn filter_activity(
    orb: State<'_, Handle>,
) -> Result<Vec<orb_core::store::FilterActivity>, OrbError> {
    let orb = orb.inner().clone();
    offload("filter_activity", move || orb.filter_activity()).await
}

/// The highlight rules with their styles, in rule order.
#[tauri::command]
async fn highlight_styles(
    orb: State<'_, Handle>,
) -> Result<Vec<orb_core::highlights::HighlightStyle>, OrbError> {
    let orb = orb.inner().clone();
    offload("highlight_styles", move || Ok(orb.highlight_styles())).await
}

/// How many messages of a folder each highlight rule matches.
#[tauri::command]
async fn highlight_counts(
    orb: State<'_, Handle>,
    account: String,
    folder: String,
) -> Result<Vec<orb_core::store::HighlightCount>, OrbError> {
    let orb = orb.inner().clone();
    offload("highlight_counts", move || {
        orb.highlight_counts(account, folder)
    })
    .await
}

/// The unread counts of the badged highlight rules, per folder.
#[tauri::command]
async fn highlight_badges(
    orb: State<'_, Handle>,
) -> Result<Vec<orb_core::store::HighlightBadge>, OrbError> {
    let orb = orb.inner().clone();
    offload("highlight_badges", move || orb.highlight_badges()).await
}

/// Every staged action, with the message it names, for the review panel.
#[tauri::command]
async fn pending_actions(orb: State<'_, Handle>) -> Result<Vec<PendingActionInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("pending_actions", move || {
        orb.list_pending_actions(PendingScope::All)
    })
    .await
}

/// How many actions are staged per folder, for the sidebar.
#[tauri::command]
async fn pending_counts(orb: State<'_, Handle>) -> Result<Vec<PendingFolderCount>, OrbError> {
    let orb = orb.inner().clone();
    offload("pending_counts", move || {
        orb.pending_action_counts(PendingScope::All)
    })
    .await
}

/// Forget the staged actions of one folder, or of everything when no folder
/// is named. Returns how many were dropped.
#[tauri::command]
async fn discard_pending(
    orb: State<'_, Handle>,
    account: Option<String>,
    folder: Option<String>,
    undo: Option<String>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    let scope = match (account, folder) {
        (Some(account_name), Some(folder_name)) => PendingScope::Folder {
            account_name,
            folder_name,
        },
        (Some(account_name), None) => PendingScope::Account { account_name },
        _ => PendingScope::All,
    };
    offload("discard_pending", move || {
        orb.discard_pending_actions(scope, undo)
    })
    .await
}

#[tauri::command]
async fn delete_messages(
    app: AppHandle,
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    uids: Vec<u32>,
    undo: Option<String>,
) -> Result<DeleteOutcome, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = offload("delete_messages", move || {
        orb.delete_messages(account, folder, uids, undo)
    })
    .await;
    update_badge_later(&app, &badge);
    result
}

#[tauri::command]
fn trash_folder(orb: State<'_, Handle>, account: String) -> String {
    orb.trash_folder(account)
}

#[tauri::command]
async fn move_messages(
    app: AppHandle,
    orb: State<'_, Handle>,
    account: String,
    folder: String,
    target: String,
    uids: Vec<u32>,
    undo: Option<String>,
) -> Result<WriteOutcome, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = offload("move_messages", move || {
        orb.move_messages(account, folder, target, uids, undo)
    })
    .await;
    update_badge_later(&app, &badge);
    result
}

/// Each account's standing with its server, for the sidebar and its panel.
#[tauri::command]
async fn account_statuses(orb: State<'_, Handle>) -> Result<Vec<AccountHealth>, OrbError> {
    let orb = orb.inner().clone();
    offload("account_statuses", move || Ok(orb.account_statuses())).await
}

/// The words of a search query worth marking in the list and the reader.
#[tauri::command]
async fn highlight_terms(orb: State<'_, Handle>, query: String) -> Result<Vec<String>, OrbError> {
    let orb = orb.inner().clone();
    offload("highlight_terms", move || Ok(orb.highlight_terms(query))).await
}

/// The attachments of a message, with size and type.
#[tauri::command]
async fn attachments(orb: State<'_, Handle>, id: i64) -> Result<Vec<AttachmentMeta>, OrbError> {
    let orb = orb.inner().clone();
    offload("attachments", move || orb.list_attachments(id)).await
}

/// What a message's headers say about its sender (see
/// `OrbHandle::sender_check`).
#[tauri::command]
async fn sender_check(
    orb: State<'_, Handle>,
    id: i64,
) -> Result<orb_core::safety::SenderCheck, OrbError> {
    let orb = orb.inner().clone();
    offload("sender_check", move || orb.sender_check(id)).await
}

/// The attachment as a file on disk, for a drag out of the window.
#[tauri::command]
async fn attachment_path(
    orb: State<'_, Handle>,
    id: i64,
    part: String,
) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("attachment_path", move || orb.attachment_path(id, part)).await
}

/// Open an attachment with its default application — or reveal it in Finder
/// when opening it would run it, and ask first when it is dangerous to open.
#[tauri::command]
async fn open_attachment(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
    part: String,
) -> Result<OpenedAttachment, OrbError> {
    let orb = orb.inner().clone();
    let path = offload("open_attachment", move || orb.attachment_path(id, part)).await?;
    open_on_disk(&app, path).await
}

/// Show an attachment in Quick Look.
#[tauri::command]
async fn quick_look_attachment(
    orb: State<'_, Handle>,
    id: i64,
    part: String,
) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    let path = offload("quick_look_attachment", move || {
        orb.attachment_path(id, part)
    })
    .await?;
    // The path is one argument, never a shell string; `qlmanage` is the
    // system's own previewer and exits with its window.
    std::process::Command::new("qlmanage")
        .arg("-p")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| OrbError::ConfigError {
            message: format!("could not open Quick Look: {e}"),
        })?;
    Ok(path)
}

/// The image a dragged attachment travels under when it is not one itself.
///
/// The drag plugin wants an image file on disk, and a bundle's icon has no
/// path a running binary should assume, so the compiled-in icon is written
/// to the attachment cache once and reused.
#[tauri::command]
async fn drag_icon() -> Result<String, OrbError> {
    const ICON: &[u8] = include_bytes!("../icons/32x32.png");
    let path = orb_core::tasks::attachment_cache_root().join("drag-icon.png");
    let fresh = std::fs::metadata(&path)
        .map(|m| m.len() == ICON.len() as u64)
        .unwrap_or(false);
    if !fresh {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| OrbError::ConfigError {
                message: format!("could not create {}: {e}", parent.display()),
            })?;
        }
        std::fs::write(&path, ICON).map_err(|e| OrbError::ConfigError {
            message: format!("could not write {}: {e}", path.display()),
        })?;
    }
    Ok(path.to_string_lossy().into_owned())
}

/// Save an attachment where the user chooses: the save dialog is asked here,
/// so the path a message's bytes are written to is one a person picked and
/// nothing the page could name. `None` when the dialog was cancelled.
#[tauri::command]
async fn save_attachment(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
    part: String,
) -> Result<Option<String>, OrbError> {
    let orb = orb.inner().clone();
    let data = offload("save_attachment", move || orb.extract_attachment(id, part)).await?;
    let dialog = app
        .dialog()
        .file()
        .set_file_name(&data.filename)
        .set_title(format!("Save {}", data.filename));
    let picked = tauri::async_runtime::spawn_blocking(move || dialog.blocking_save_file())
        .await
        .map_err(|e| OrbError::ConfigError {
            message: format!("the save dialog failed: {e}"),
        })?;
    let Some(path) = picked.and_then(|picked| picked.into_path().ok()) else {
        return Ok(None);
    };
    std::fs::write(&path, &data.data).map_err(|e| OrbError::ConfigError {
        message: format!("could not write {}: {e}", path.display()),
    })?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Forwards sync progress from the core to the webview as it happens.
struct ProgressBridge {
    app: tauri::AppHandle,
}

impl SyncProgressListener for ProgressBridge {
    fn on_progress(&self, update: SyncProgressUpdate) {
        let _ = self.app.emit("sync-progress", update);
    }
}

/// Tells the webview about the syncs the core runs on its own: on a push
/// notification, on the periodic timer, and after the machine wakes.
///
/// The syncs run in the core so that mail arrives whether or not anyone is
/// looking; only the display is left to do here.
struct PushBridge {
    app: tauri::AppHandle,
}

impl PushListener for PushBridge {
    fn on_started(&self, _account: String, _folder: Option<String>) {
        let _ = self.app.emit(ACCOUNT_STATUS_EVENT, ());
    }

    fn on_synced(&self, account: String, folder: Option<String>, result: SyncResult) {
        // Only disturb the interface when something actually changed. Servers
        // announce plenty that does not, and the timer fires on nothing most
        // of the time.
        if result.messages_added + result.messages_updated + result.messages_deleted > 0 {
            let _ = self.app.emit(
                "folder-changed",
                (account.clone(), folder.clone().unwrap_or_default()),
            );
        }
        for error in &result.errors {
            let _ = self.app.emit("push-error", format!("{account}: {error}"));
        }
        if let Some(orb) = self.app.try_state::<Handle>() {
            after_sync(&self.app, orb.inner(), &result.new_mail);
        }
    }

    fn on_error(&self, account: String, folder: Option<String>, error: String) {
        let _ = self.app.emit(
            "push-error",
            format!(
                "{account}/{}: {error}",
                folder.unwrap_or_else(|| "*".to_string())
            ),
        );
        let _ = self.app.emit(ACCOUNT_STATUS_EVENT, ());
    }
}

/// Save a draft to the account's Drafts folder, replacing an earlier save.
#[tauri::command]
async fn save_draft(
    orb: State<'_, Handle>,
    input: SendMailInput,
    replacing: Option<u32>,
) -> Result<Option<u32>, OrbError> {
    let orb = orb.inner().clone();
    offload("save_draft", move || orb.save_draft(input, replacing)).await
}

/// Remove a saved draft, once it has been sent or abandoned.
#[tauri::command]
async fn discard_draft(orb: State<'_, Handle>, account: String, uid: u32) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("discard_draft", move || orb.discard_draft(account, uid)).await
}

/// The Outbox: every queued message, waiting or failed.
#[tauri::command]
async fn outbox(orb: State<'_, Handle>) -> Result<Vec<orb_core::store::QueuedSend>, OrbError> {
    let orb = orb.inner().clone();
    offload("outbox", move || orb.outbox()).await
}

/// Put a failed message back in line and try to send it now.
#[tauri::command]
async fn retry_outbox(orb: State<'_, Handle>, id: i64) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("retry_outbox", move || orb.retry_outbox(id)).await
}

/// Delete a queued message unsent.
#[tauri::command]
async fn discard_outbox(orb: State<'_, Handle>, id: i64) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    offload("discard_outbox", move || orb.discard_outbox(id)).await
}

/// A queued message, to be written on again.
#[derive(serde::Serialize)]
struct OutboxDraft {
    account: String,
    draft: DraftTemplate,
}

#[tauri::command]
async fn edit_outbox(orb: State<'_, Handle>, id: i64) -> Result<OutboxDraft, OrbError> {
    let orb = orb.inner().clone();
    offload("edit_outbox", move || {
        orb.edit_outbox(id)
            .map(|(account, draft)| OutboxDraft { account, draft })
    })
    .await
}

/// Write a message's attachments to disk, so a forward can carry them.
#[tauri::command]
async fn detach_attachments(orb: State<'_, Handle>, id: i64) -> Result<Vec<String>, OrbError> {
    let orb = orb.inner().clone();
    offload("detach_attachments", move || orb.detach_attachments(id)).await
}

/// The draft a reply starts from, quoted and signed as configured.
#[tauri::command]
async fn reply_draft(
    orb: State<'_, Handle>,
    id: i64,
    all: bool,
) -> Result<DraftTemplate, OrbError> {
    let orb = orb.inner().clone();
    offload("reply_draft", move || orb.reply_draft(id, all)).await
}

/// The draft a forward starts from, carrying the original's attachments.
#[tauri::command]
async fn forward_draft(orb: State<'_, Handle>, id: i64) -> Result<DraftTemplate, OrbError> {
    let orb = orb.inner().clone();
    offload("forward_draft", move || orb.forward_draft(id)).await
}

/// Completions for a partly-typed recipient.
///
/// Two address books answer: the one the mail built, which knows who is
/// actually written to, and the system's, which knows the people put there on
/// purpose. Both are scored by the same function in the core so the merged
/// order means something — otherwise the list would be two lists stacked, and
/// the better match could sit below the worse one purely by source.
#[tauri::command]
async fn suggest_recipients(
    orb: State<'_, Handle>,
    query: String,
    limit: u32,
) -> Result<Vec<orb_core::ContactSuggestion>, OrbError> {
    let orb = orb.inner().clone();
    // Reading the address book is a database scan, which has no business
    // running on the async runtime: it is short, but so is the interval
    // between two keystrokes, and everything else waiting on that thread would
    // queue behind it. Going through `offload` also puts the call in the log,
    // which is the only way to tell a completion list that found nobody from
    // one that was never asked.
    offload("suggest_recipients", move || {
        let mut all = orb.suggest_recipients(query.clone(), limit);

        all.extend(contacts::entries().into_iter().filter_map(|entry| {
            let score = orb_core::contacts::score(&query, entry.name.as_deref(), &entry.address);
            (score > 0).then(|| orb_core::ContactSuggestion {
                name: entry.name.clone(),
                recipients: vec![orb_core::Recipient {
                    name: entry.name,
                    address: entry.address,
                }],
                source: orb_core::contacts::Source::System,
                score,
            })
        }));

        Ok(orb_core::contacts::merge(all, limit as usize))
    })
    .await
}

/// The draft a new message starts from: the default identity's signature.
#[tauri::command]
async fn new_draft(orb: State<'_, Handle>, account: String) -> Result<DraftTemplate, OrbError> {
    let orb = orb.inner().clone();
    offload("new_draft", move || Ok(orb.new_draft(account))).await
}

/// A saved draft, to be written on.
#[tauri::command]
async fn edit_draft(orb: State<'_, Handle>, id: i64) -> Result<DraftTemplate, OrbError> {
    let orb = orb.inner().clone();
    offload("edit_draft", move || orb.edit_draft(id)).await
}

/// The identities an account sends as, the default one first.
#[tauri::command]
async fn identities(
    orb: State<'_, Handle>,
    account: String,
) -> Result<Vec<orb_core::ffi::IdentityInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("identities", move || Ok(orb.identities(account))).await
}

/// Every identity of every account, for the compose window's From.
#[tauri::command]
async fn senders(orb: State<'_, Handle>) -> Result<Vec<orb_core::ffi::IdentityInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("senders", move || Ok(orb.senders())).await
}

/// A body written as one identity, with the other's signature; `to_account`
/// when the other is another account's.
#[tauri::command]
async fn switch_identity(
    orb: State<'_, Handle>,
    account: String,
    from: String,
    to_account: Option<String>,
    to: String,
    body: String,
) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("switch_identity", move || {
        orb.switch_identity(account, from, to_account, to, body)
    })
    .await
}

/// What is worth a second look about a draft before it is sent.
#[tauri::command]
async fn draft_warnings(
    orb: State<'_, Handle>,
    input: orb_core::ffi::DraftWarningsInput,
) -> Result<Vec<orb_core::draft::DraftWarning>, OrbError> {
    let orb = orb.inner().clone();
    offload("draft_warnings", move || Ok(orb.draft_warnings(input))).await
}

/// The mail templates an account may use, with what is known for their
/// placeholders in a message to `to`.
#[tauri::command]
async fn templates(
    orb: State<'_, Handle>,
    account: String,
    to: Vec<String>,
) -> Result<orb_core::ffi::TemplatesForDraft, OrbError> {
    let orb = orb.inner().clone();
    offload("templates", move || Ok(orb.templates(account, to))).await
}

/// A template, read again from its file, ready to go into a draft.
#[tauri::command]
async fn use_template(
    orb: State<'_, Handle>,
    path: String,
    to: Vec<String>,
) -> Result<orb_core::ffi::TemplateUse, OrbError> {
    let orb = orb.inner().clone();
    offload("use_template", move || orb.use_template(path, to)).await
}

/// What is known for a template's placeholders in a message to `to`.
#[tauri::command]
async fn template_suggestions(
    orb: State<'_, Handle>,
    names: Vec<String>,
    to: Vec<String>,
) -> Result<std::collections::HashMap<String, String>, OrbError> {
    let orb = orb.inner().clone();
    offload("template_suggestions", move || {
        Ok(orb.template_suggestions(names, to))
    })
    .await
}

/// The drafts handed to compose windows that have not collected them yet,
/// by window label. A window is created before it can be spoken to, so the
/// draft waits here for the window to ask, rather than travelling in the URL
/// where a long quoted body is a long URL.
#[derive(Default)]
struct ComposeHandover(std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>);

/// The draft this compose window was opened with, once.
#[tauri::command]
fn take_compose_draft(
    window: tauri::WebviewWindow,
    handover: State<'_, ComposeHandover>,
) -> Option<serde_json::Value> {
    handover.inner().0.lock().unwrap().remove(window.label())
}

/// Open a draft in a window of its own.
///
/// A real window rather than a tab drawn by the app: every window here shares a
/// tabbing identifier, so macOS groups them into tabs, gives them a tab bar,
/// and lets one be dragged out into a window of its own. That is the behaviour
/// wanted, and none of it is code here.
#[tauri::command]
async fn open_compose(app: tauri::AppHandle, draft: serde_json::Value) -> Result<(), OrbError> {
    // Each window needs a label of its own, and the label outlives the window,
    // so it counts rather than being reused.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // The mailbox window is the tab group. A draft joins it, so one window
    // holds the mail and everything being written about it, and the tab bar
    // moves between them — rather than drafts forming a second group of their
    // own alongside the window they came from.
    let host = app.get_webview_window("main").or_else(|| {
        app.webview_windows()
            .into_iter()
            .find(|(label, _)| label.starts_with("compose-"))
            .map(|(_, window)| window)
    });

    let label = format!("compose-{n}");
    app.state::<ComposeHandover>()
        .inner()
        .0
        .lock()
        .unwrap()
        .insert(label.clone(), draft);
    let window = tauri::WebviewWindowBuilder::new(
        &app,
        label.clone(),
        tauri::WebviewUrl::App("compose.html".into()),
    )
    .title("New message")
    .inner_size(680.0, 420.0)
    .title_bar_style(tauri::TitleBarStyle::Overlay)
    .hidden_title(true)
    // The same identity as the mailbox window, so they are one group.
    .tabbing_identifier("orb")
    .on_navigation(navigation_guard(&app, &label))
    .build()
    .map_err(|e| {
        // No window will ever collect it.
        app.state::<ComposeHandover>()
            .inner()
            .0
            .lock()
            .unwrap()
            .remove(&label);
        OrbError::ConfigError {
            message: format!("could not open a compose window: {e}"),
        }
    })?;

    #[cfg(target_os = "macos")]
    give_toolbar(&app, &window);
    #[cfg(target_os = "macos")]
    if let Some(sibling) = host {
        // AppKit only listens on the main thread, and a command runs on a
        // worker — which is why asking to tab did nothing at all rather than
        // failing. Hopping threads is the whole fix.
        let joining = window.clone();
        if let Err(error) = app.run_on_main_thread(move || add_as_tab(&sibling, &joining)) {
            orb_core::logger::warn(&format!("could not reach the main thread to tab: {error}"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = host;

    Ok(())
}

/// Neovim in a compose window: `nvim --embed`, drawn by the window. The
/// sessions live in the core ([`orb_core::editor`]); these are the shims.
///
/// Keys, mouse events and resizes skip [`offload`]'s job record: they come
/// by the dozen a second, and the console's list of recent jobs is for work
/// worth reading about.
async fn editor_call<T, F>(f: F) -> Result<T, OrbError>
where
    F: FnOnce() -> Result<T, OrbError> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| OrbError::ConfigError {
            message: format!("the editor call failed: {e}"),
        })?
}

/// Refuse an editor call from any window but the one the editor belongs to.
///
/// What goes into an editor is run by Neovim, with everything the user's
/// Neovim can do — `:!` runs a shell. The commands are callable from every
/// window of the app, so without this a page that had got script into one
/// window could type into the editor of another. The owner is the window
/// that opened the editor, taken from the window itself rather than from an
/// argument.
fn editor_of_window(orb: &Handle, id: &str, window: &tauri::WebviewWindow) -> Result<(), OrbError> {
    if editor_belongs(orb.editor_owner(id).as_deref(), window.label()) {
        Ok(())
    } else {
        Err(OrbError::ConfigError {
            message: format!("editor {id} is not this window's"),
        })
    }
}

fn editor_belongs(owner: Option<&str>, label: &str) -> bool {
    owner == Some(label)
}

/// Start Neovim on a draft's body, with the identity's signature shown
/// below it; what it draws arrives on `events`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn editor_open(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    body: String,
    signature: Option<String>,
    cols: u32,
    rows: u32,
    events: tauri::ipc::Channel<orb_core::editor::EditorEvent>,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    let sink: orb_core::editor::EditorSink = Arc::new(move |event| {
        let _ = events.send(event);
    });
    let owner = window.label().to_string();
    offload("editor_open", move || {
        orb.editor_open(id, owner, body, signature, cols, rows, sink)
    })
    .await
}

/// Keys for Neovim, in its notation.
#[tauri::command]
async fn editor_input(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    keys: String,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_input(id, keys)).await
}

/// Text pasted into Neovim.
#[tauri::command]
async fn editor_paste(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    text: String,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_paste(id, text)).await
}

/// A mouse event on Neovim's grid.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn editor_mouse(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    button: String,
    action: String,
    modifier: String,
    row: u32,
    col: u32,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_mouse(id, button, action, modifier, row, col)).await
}

/// The editor gained or lost the keyboard.
#[tauri::command]
async fn editor_focus(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    gained: bool,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_focus(id, gained)).await
}

/// The editor's grid is now `cols` × `rows`.
#[tauri::command]
async fn editor_resize(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    cols: u32,
    rows: u32,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_resize(id, cols, rows)).await
}

/// The draft as it stands in Neovim, written or not.
#[tauri::command]
async fn editor_buffer(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
) -> Result<String, OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_buffer(id)).await
}

/// Replace the draft in Neovim, with the signature below it, and write it.
#[tauri::command]
async fn editor_set_buffer(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    body: String,
    signature: Option<String>,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    offload("editor_set_buffer", move || {
        orb.editor_set_buffer(id, body, signature)
    })
    .await
}

/// Put a template's text into the draft in Neovim, at the cursor.
#[tauri::command]
async fn editor_insert_template(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    text: orb_core::templates::Expanded,
) -> Result<bool, OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_insert_template(id, text)).await
}

/// Fill a placeholder wherever it stands in the draft in Neovim.
#[tauri::command]
async fn editor_fill_placeholder(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    name: String,
    value: String,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_fill_placeholder(id, name, value)).await
}

/// Put Neovim's cursor on a placeholder.
#[tauri::command]
async fn editor_goto_placeholder(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    name: String,
) -> Result<bool, OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_goto_placeholder(id, name)).await
}

/// Open a file in a tab of Neovim, beside the draft.
#[tauri::command]
async fn editor_edit_file(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
    path: String,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_edit_file(id, path)).await
}

/// Stop Neovim and remove its file.
#[tauri::command]
async fn editor_close(
    orb: State<'_, Handle>,
    window: tauri::WebviewWindow,
    id: String,
) -> Result<(), OrbError> {
    editor_of_window(&orb, &id, &window)?;
    let orb = orb.inner().clone();
    editor_call(move || orb.editor_close(id)).await
}

/// Move between tabs of the window group.
///
/// `which` is "next", "previous", or a 1-based tab number.
///
/// Done here rather than by a menu accelerator because a key equivalent on
/// macOS is a *character*, and the characters differ by keyboard layout: the
/// key that carries ö on a Swiss German layout carries `;` on a US one, and
/// pressing it with Alt produces a third thing again. The caller therefore
/// matches on the physical key and asks for what it wants by name.
#[tauri::command]
async fn select_tab(app: tauri::AppHandle, which: String) -> Result<(), OrbError> {
    switch_tab(&app, which);
    Ok(())
}

/// Move between tabs. Shared by the shortcut and the menu, so the two cannot
/// come to mean different things.
fn switch_tab(app: &tauri::AppHandle, which: String) {
    #[cfg(target_os = "macos")]
    {
        let focused = app
            .webview_windows()
            .into_values()
            .find(|w| w.is_focused().unwrap_or(false));
        let Some(window) = focused else { return };
        // The window handle crosses threads; the raw NSWindow pointer does
        // not, so it is fetched on the far side.
        app.run_on_main_thread(move || {
            let Ok(ns) = window.ns_window() else { return };
            use objc2::msg_send;
            use objc2::runtime::AnyObject;

            // SAFETY: the pointer comes from Tauri's NSWindow accessor and this
            // closure runs on the main thread, which AppKit requires.
            unsafe {
                let window = ns as *mut AnyObject;
                match which.as_str() {
                    "next" => {
                        let _: () = msg_send![window, selectNextTab: std::ptr::null::<AnyObject>()];
                    }
                    "previous" => {
                        let _: () =
                            msg_send![window, selectPreviousTab: std::ptr::null::<AnyObject>()];
                    }
                    number => {
                        let Ok(n) = number.parse::<usize>() else {
                            return;
                        };
                        let tabs: *mut AnyObject = msg_send![window, tabbedWindows];
                        if tabs.is_null() {
                            return;
                        }
                        let count: usize = msg_send![tabs, count];
                        // 1-based, and out of range selects nothing rather than
                        // the nearest tab: asking for a tab that is not there
                        // is a mistake, not a request to go somewhere else.
                        if n == 0 || n > count {
                            return;
                        }
                        let target: *mut AnyObject = msg_send![tabs, objectAtIndex: n - 1];
                        let _: () =
                            msg_send![target, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
                    }
                }
            }
        })
        .unwrap_or_else(|e| {
            orb_core::logger::warn(&format!(
                "could not reach the main thread to switch tabs: {e}"
            ));
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, which);
}

/// The application menu.
///
/// Every item is a command the app already has, so a menu entry and its
/// keybinding cannot drift apart: choosing "Reply" runs exactly what `r` runs.
/// The shortcuts follow Mail's, because those are what a hand reaches for on
/// this platform — ⌘N to write, ⌘R to reply, ⇧⌘N to fetch.
///
/// Editing, windowing and the application menu itself are the system's own
/// items rather than reimplementations: they already do the right thing in
/// every window, including the text fields in a compose window.
fn build_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};

    // `id` is the command that runs, so the menu speaks the same vocabulary as
    // the keymap and the context menu.
    let item = |id: &str, label: &str, accel: &str| {
        MenuItemBuilder::with_id(id, label)
            .accelerator(accel)
            .build(app)
    };

    let app_menu = SubmenuBuilder::new(app, "Orb")
        .item(&PredefinedMenuItem::about(app, None, None)?)
        .separator()
        .item(&PredefinedMenuItem::services(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::hide(app, None)?)
        .item(&PredefinedMenuItem::hide_others(app, None)?)
        .item(&PredefinedMenuItem::show_all(app, None)?)
        .separator()
        // Not the predefined Quit, which terminates on the spot: this one
        // runs `quit`, which asks about staged work and lets drafts save.
        .item(&item("quit", "Quit Orb", "CmdOrCtrl+Q")?)
        .build()?;

    let file = SubmenuBuilder::new(app, "File")
        .item(&item("compose", "New Message", "CmdOrCtrl+N")?)
        .separator()
        .item(&PredefinedMenuItem::close_window(app, None)?)
        .build()?;

    let plain = |id: &str, label: &str| MenuItemBuilder::with_id(id, label).build(app);

    // Select All stays the predefined item: the webview sees ⌘A first and the
    // keymap takes it (`<d-a>` is `select-all`), so the item only fires where
    // the page leaves the key alone — a text field, where selecting the text
    // is what it should do.
    let edit = SubmenuBuilder::new(app, "Edit")
        .item(&PredefinedMenuItem::undo(app, None)?)
        .item(&PredefinedMenuItem::redo(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, None)?)
        .item(&PredefinedMenuItem::copy(app, None)?)
        .item(&PredefinedMenuItem::paste(app, None)?)
        .item(&PredefinedMenuItem::select_all(app, None)?)
        .separator()
        .item(&item("search-mode", "Search Mail", "CmdOrCtrl+F")?)
        .item(&plain("command-mode", "Command Line")?)
        .build()?;

    let view = SubmenuBuilder::new(app, "View")
        .item(&plain("view html", "Show as HTML")?)
        .item(&plain("view text", "Show as Plain Text")?)
        .item(&plain("view raw", "Show Source")?)
        .item(&plain("toggle-dark-mail", "Draw HTML Mail Dark")?)
        .item(&plain("load-remote-images", "Load Remote Images")?)
        .separator()
        .item(&plain("toggle-threads", "Show as Conversations")?)
        .item(&plain("toggle-thread", "Expand or Collapse Conversation")?)
        .item(&plain("open-thread", "Open Whole Conversation")?)
        .item(&plain(
            "open-all-messages",
            "Open All Messages of Conversation",
        )?)
        .item(&plain(
            "close-all-messages",
            "Close All Messages of Conversation",
        )?)
        .separator()
        .item(&item("zoom-in", "Make Text Bigger", "CmdOrCtrl+=")?)
        .item(&item("zoom-out", "Make Text Smaller", "CmdOrCtrl+-")?)
        .item(&item("zoom-reset", "Actual Size", "CmdOrCtrl+0")?)
        .item(&plain("toggle-look", "Switch Between Hybrid and Terminal")?)
        .item(&plain("toggle-sidebar", "Show or Hide Sidebar")?)
        .item(&plain(
            "toggle-today-layout",
            "Switch Today Between Stacked and Columns",
        )?)
        .item(&plain("toggle-today-mixed", "Show Today as One Timeline")?)
        .item(&plain("toggle-today-mail", "Show Mail in Today")?)
        .item(&plain("toggle-today-tasks", "Show Tasks in Today")?)
        .item(&plain("toggle-today-reading", "Show Reading in Today")?)
        .item(&plain("toggle-today-chat", "Show Chat in Today")?)
        .item(&plain("toggle-group-by-tag", "Group Tasks by Tag")?)
        .item(&plain("toggle-due-first", "List Due Tasks First")?)
        .separator()
        .item(&plain("console", "Console")?)
        .build()?;

    let go = SubmenuBuilder::new(app, "Go")
        .item(&item("go-to", "Go to…", "CmdOrCtrl+K")?)
        .item(&item(
            "command-palette",
            "Command Palette…",
            "CmdOrCtrl+Shift+K",
        )?)
        .separator()
        .item(&item("next-folder", "Next Folder", "Ctrl+CmdOrCtrl+Down")?)
        .item(&item(
            "prev-folder",
            "Previous Folder",
            "Ctrl+CmdOrCtrl+Up",
        )?)
        .separator()
        .item(&plain("open-task-view today", "Today")?)
        .item(&plain("open-task-view upcoming", "Upcoming")?)
        .item(&plain("open-task-view someday", "Someday")?)
        .item(&plain("open-task-view completed", "Completed Tasks")?)
        .item(&plain("open-task-view archive", "Archived Tasks")?)
        .item(&plain("open-task-view trash", "Task Trash")?)
        .separator()
        .item(&plain("open-reading list", "To Read")?)
        .item(&plain("open-reading must-read", "Must Read")?)
        .item(&plain("open-reading archive", "Reading Archive")?)
        .separator()
        .item(&plain("open-chat", "Chat: Waiting")?)
        .item(&plain("chat-open-room", "Open Chat Room…")?)
        .item(&plain("chat-mute-room", "Mute Chat Room…")?)
        .item(&plain("chat-muted-rooms", "Muted Chat Rooms…")?)
        .item(&plain("chat-new-message", "New Chat Message…")?)
        .item(&plain("chat-create-room", "New Chat Room…")?)
        .item(&plain("chat-sign-in", "Sign In to Chat…")?)
        .item(&plain("chat-sign-out", "Sign Out of Chat")?)
        .separator()
        .item(&plain("review-pending", "Staged Actions")?)
        .item(&plain("outbox", "Outbox")?)
        .build()?;

    let message = SubmenuBuilder::new(app, "Message")
        .item(&item("reply", "Reply", "CmdOrCtrl+R")?)
        .item(&item("reply-all", "Reply All", "Shift+CmdOrCtrl+R")?)
        .item(&item("forward", "Forward", "Shift+CmdOrCtrl+F")?)
        .item(&plain("edit-draft", "Edit Draft")?)
        .separator()
        .item(&item(
            "toggle-read",
            "Mark as Read/Unread",
            "Shift+CmdOrCtrl+U",
        )?)
        .item(&item("toggle-flag", "Flag", "Shift+CmdOrCtrl+L")?)
        .separator()
        .item(&item("archive", "Archive", "Ctrl+CmdOrCtrl+A")?)
        .item(&plain("read-later", "Read Later")?)
        .item(&plain("move-immediate", "Move to…")?)
        .item(&plain("copy-immediate", "Copy to…")?)
        .item(&plain("move-across", "Move to Another Account…")?)
        .item(&plain("copy-across", "Copy to Another Account…")?)
        .item(&item("spam", "Move to Junk", "Shift+CmdOrCtrl+J")?)
        .item(&plain("not-spam", "Not Junk")?)
        .separator()
        .item(&item(
            "delete-immediate",
            "Move to Trash",
            "CmdOrCtrl+Backspace",
        )?)
        .separator()
        .item(&plain("mark-delete", "Stage for Deletion")?)
        .item(&plain("mark-move", "Stage a Move…")?)
        .item(&plain("unmark", "Unstage")?)
        .separator()
        .item(&plain("open-attachment", "Open Attachment")?)
        .item(&plain("save-attachment", "Save Attachment…")?)
        .item(&plain("quick-look", "Quick Look Attachment")?)
        .separator()
        .item(&plain("create-task-from-message", "New Task from Message")?)
        .item(&plain("open-tasks-for-message", "Tasks for Message")?)
        .build()?;

    let tasks = SubmenuBuilder::new(app, "Tasks")
        .item(&plain("open-tasks-window", "Tasks Window")?)
        .separator()
        .item(&plain("create-task", "New Task")?)
        .item(&plain("edit-task", "Edit Task")?)
        .item(&plain("complete-task", "Complete Task")?)
        .item(&plain("uncomplete-task", "Reopen Task")?)
        .item(&plain("clear-completed", "Clear Completed from Today")?)
        .item(&plain("merge-tasks", "Merge Tasks…")?)
        .separator()
        .item(&plain("set-due-date", "Set Due Date…")?)
        .item(&plain("postpone-task", "Postpone…")?)
        .item(&plain("set-recurrence", "Set Recurrence…")?)
        .item(&plain("add-reminder", "Add Reminder…")?)
        .item(&plain("attach-file", "Attach File…")?)
        .separator()
        .item(&plain("delete-task", "Delete Task")?)
        .item(&plain("restore-task", "Restore Task")?)
        .item(&plain("empty-task-trash", "Empty Task Trash")?)
        .separator()
        .item(&plain("sync-tasks", "Sync Tasks")?)
        .item(&plain("export-tasks", "Export Tasks Now")?)
        .item(&plain("import-tasks", "Import Tasks…")?)
        .item(&plain("import-things", "Import from Things 3…")?)
        .separator()
        .item(&plain("set-up-iphone", "Set Up iPhone…")?)
        .build()?;

    let help = SubmenuBuilder::new(app, "Help")
        .item(&plain("help", "Keyboard Shortcuts")?)
        .item(&plain("account-status", "Account Status")?)
        .item(&plain("feed-status", "Feed Status")?)
        .item(&plain("sync-stats", "Sync Statistics")?)
        .build()?;

    let mailbox = SubmenuBuilder::new(app, "Mailbox")
        .item(&item("sync", "Get New Mail", "Shift+CmdOrCtrl+N")?)
        .item(&plain("poll-feeds", "Get New Feed Items")?)
        .item(&plain("account-status", "Account Status")?)
        .item(&plain("feed-status", "Feed Status")?)
        .separator()
        .item(&plain("outbox", "Outbox")?)
        .item(&plain("retry-outbox", "Send Everything in the Outbox")?)
        .separator()
        .item(&plain("create-folder", "New Folder…")?)
        .item(&plain("rename-folder", "Rename Folder…")?)
        .item(&plain("delete-folder", "Delete Folder")?)
        .item(&plain("subscribe-folder", "Subscribe")?)
        .item(&plain("unsubscribe-folder", "Unsubscribe")?)
        .separator()
        .item(&plain("review-pending", "Review Staged Actions")?)
        .item(&item(
            "expunge",
            "Apply Staged Actions in Folder",
            "Shift+CmdOrCtrl+E",
        )?)
        .item(&MenuItemBuilder::with_id("expunge-all", "Apply All Staged Actions").build(app)?)
        .item(
            &MenuItemBuilder::with_id("discard-pending", "Discard Staged Actions in Folder")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id("discard-pending all", "Discard All Staged Actions")
                .build(app)?,
        )
        .build()?;

    let window = SubmenuBuilder::new(app, "Window")
        .item(&PredefinedMenuItem::minimize(app, None)?)
        .item(&PredefinedMenuItem::maximize(app, None)?)
        .item(&PredefinedMenuItem::fullscreen(app, None)?)
        .separator()
        .item(&plain("open-chat-window", "Chat Window")?)
        .separator()
        .item(&item("select-tab-1", "First Tab", "CmdOrCtrl+1")?)
        .item(&item("select-tab-2", "Second Tab", "CmdOrCtrl+2")?)
        .item(&item("select-tab-3", "Third Tab", "CmdOrCtrl+3")?)
        .separator()
        .item(&PredefinedMenuItem::bring_all_to_front(app, None)?)
        .build()?;

    // NOTE: handing this to AppKit as the windows menu (
    // `set_as_windows_menu_for_nsapp`) drops the items added above, so the tab
    // entries are kept and the automatic window list given up.

    MenuBuilder::new(app)
        .items(&[
            &app_menu, &file, &edit, &view, &go, &message, &mailbox, &tasks, &help, &window,
        ])
        .build()
}

/// Put `window` in `sibling`'s tab group.
///
/// A shared tabbing identifier is not enough on its own: it only groups windows
/// automatically when the system is set to prefer tabs when opening documents,
/// which it is not by default, so every draft came up as a loose window.
/// Telling AppKit directly makes drafts tab regardless of that setting, while
/// leaving everything the tab bar offers — dragging one out, moving between
/// them — exactly as the system provides it.
#[cfg(target_os = "macos")]
fn add_as_tab(sibling: &tauri::WebviewWindow, window: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let (host, guest) = match (sibling.ns_window(), window.ns_window()) {
        (Ok(host), Ok(guest)) => (host, guest),
        (host, guest) => {
            orb_core::logger::warn(&format!(
                "no NSWindow to tab with (host ok: {}, guest ok: {})",
                host.is_ok(),
                guest.is_ok()
            ));
            return;
        }
    };
    // SAFETY: both pointers come from Tauri's own NSWindow accessors, and this
    // runs on the main thread, which is where AppKit requires it.
    unsafe {
        let host = host as *mut AnyObject;
        let guest = guest as *mut AnyObject;
        // NSWindowAbove == 1: the new tab goes after the one it joins.
        let _: () = msg_send![host, addTabbedWindow: guest, ordered: 1isize];
        // Whether it took. `tabbedWindows` is nil for a window that is not in a
        // group, so this says plainly whether the call did anything rather than
        // leaving it to be guessed from the screen.
        let tabs: *mut AnyObject = msg_send![host, tabbedWindows];
        let count: usize = if tabs.is_null() {
            0
        } else {
            msg_send![tabs, count]
        };
        orb_core::logger::info(&format!("compose window tabbed; group now holds {count}"));
    }
}

/// The strip at the top of a window whose content runs under its title bar,
/// in points.
#[derive(Clone, Copy, Default, serde::Serialize)]
struct TitleBar {
    /// The toolbar's band, which the traffic lights are centred in.
    band: f64,
    /// Everything the content runs under: the band, and below it the tab bar
    /// AppKit adds once a second window joins the group.
    strip: f64,
}

/// Measure the strip at the top of the window, which the frontend keeps for
/// the traffic lights, the drag area and the tab bar.
#[tauri::command]
async fn title_bar(app: AppHandle, window: tauri::WebviewWindow) -> TitleBar {
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let measured = window.clone();
        // AppKit answers on the main thread only.
        let asked = app.run_on_main_thread(move || {
            let _ = tx.send(measure_title_bar(&measured));
        });
        if asked.is_ok() {
            if let Ok(Some(bar)) = rx.recv() {
                return bar;
            }
        }
        TitleBar::default()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, window);
        TitleBar::default()
    }
}

/// Give a window whose content runs under its title bar a toolbar.
///
/// AppKit then draws a title bar as tall as a toolbar's and centres the
/// traffic lights in it, which is the frontend's toolbar row; the tab bar of a
/// tab group goes below it. Tauri's `traffic_light_position` cannot do this:
/// it only makes the title bar taller, and the lights stay at its top.
#[cfg(target_os = "macos")]
fn give_toolbar(app: &AppHandle, window: &tauri::WebviewWindow) {
    let window = window.clone();
    // AppKit only listens on the main thread.
    if let Err(error) = app.run_on_main_thread(move || add_toolbar(&window)) {
        orb_core::logger::warn(&format!(
            "could not reach the main thread for the toolbar: {error}"
        ));
    }
}

#[cfg(target_os = "macos")]
fn add_toolbar(window: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSString;

    /// `NSWindowToolbarStyleUnifiedCompact`: a 40 pt band, with the tab bar
    /// directly below it. The full-height style leaves a soft edge of 14 pt
    /// between the two.
    const UNIFIED_COMPACT: isize = 4;

    let Ok(ns_window) = window.ns_window() else {
        return;
    };
    let Some(class) = AnyClass::get(c"NSToolbar") else {
        return;
    };
    // SAFETY: the window pointer comes from Tauri's own NSWindow accessor, and
    // this runs on the main thread, which is where AppKit requires it.
    unsafe {
        let ns_window = ns_window as *mut AnyObject;
        let identifier = NSString::from_str("orb");
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        let toolbar: Retained<AnyObject> = msg_send![allocated, initWithIdentifier: &*identifier];
        // An empty toolbar keeps the height of a bare title bar; one item,
        // the system's flexible space, gives it a toolbar's.
        let space = NSString::from_str("NSToolbarFlexibleSpaceItem");
        let _: () = msg_send![&*toolbar, insertItemWithItemIdentifier: &*space, atIndex: 0isize];
        let _: () = msg_send![ns_window, setToolbar: &*toolbar];
        let _: () = msg_send![ns_window, setToolbarStyle: UNIFIED_COMPACT];
    }
}

#[cfg(target_os = "macos")]
fn measure_title_bar(window: &tauri::WebviewWindow) -> Option<TitleBar> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSRect;

    /// How far below the window's top edge `view` ends.
    unsafe fn bottom(view: *mut AnyObject, window_height: f64) -> f64 {
        let bounds: NSRect = msg_send![view, bounds];
        let nil: *mut AnyObject = std::ptr::null_mut();
        let in_window: NSRect = msg_send![view, convertRect: bounds, toView: nil];
        window_height - in_window.origin.y
    }

    /// Where the visible tab bar under `view` ends, if there is one. AppKit
    /// names no public class for it, so it is found by its class's name.
    unsafe fn tab_bar_bottom(view: *mut AnyObject, depth: usize, window_height: f64) -> f64 {
        let hidden: bool = msg_send![view, isHidden];
        if hidden {
            return 0.0;
        }
        let class: *const AnyClass = msg_send![view, class];
        if (*class).name().to_string_lossy().contains("TabBar") {
            return bottom(view, window_height);
        }
        if depth == 0 {
            return 0.0;
        }
        let children: *mut AnyObject = msg_send![view, subviews];
        let count: usize = msg_send![children, count];
        (0..count)
            .map(|i| {
                let child: *mut AnyObject = msg_send![children, objectAtIndex: i];
                tab_bar_bottom(child, depth - 1, window_height)
            })
            .fold(0.0, f64::max)
    }

    let ns_window = window.ns_window().ok()? as *mut AnyObject;
    // SAFETY: the pointer comes from Tauri's own NSWindow accessor, and this
    // runs on the main thread, which is where AppKit requires it.
    unsafe {
        let frame: NSRect = msg_send![ns_window, frame];
        let height = frame.size.height;
        // NSWindowCloseButton. The band is the one the lights are centred in.
        let close: *mut AnyObject = msg_send![ns_window, standardWindowButton: 0usize];
        if close.is_null() {
            return None;
        }
        let close_size: NSRect = msg_send![close, bounds];
        let band = 2.0 * (bottom(close, height) - close_size.size.height / 2.0);
        // The close button's container is the title bar's, tab bar and all.
        let parent: *mut AnyObject = msg_send![close, superview];
        let container: *mut AnyObject = msg_send![parent, superview];
        let tabs = if container.is_null() {
            0.0
        } else {
            tab_bar_bottom(container, 6, height)
        };
        Some(TitleBar {
            band,
            strip: band.max(tabs),
        })
    }
}

/// Put Orb's icon in the Dock when it is not running from a bundle.
///
/// `tauri dev` runs the bare executable out of `target/debug`, and a loose
/// Mach-O has no `Info.plist` and no `.icns` for the Dock to read, so it comes
/// up wearing the generic executable icon. `bundle.icon` cannot help with that:
/// it only describes what a *built* `.app` is assembled from.
///
/// A bundle built with Xcode 26 names its icon in `CFBundleIconName`: the
/// Liquid Glass icon compiled from `icons/Orb.icon`, which follows the Dock's
/// appearance — dark, clear, tinted. A static image would replace it with the
/// default rendering, so such a bundle is left alone. Everywhere else — in
/// development, and in a bundle that only carries `icon.icns` — the image is
/// set. The bytes are compiled in rather than read from `icons/`, because the
/// path to that directory is a property of the source tree, which a running
/// binary has no business assuming still exists.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSBundle, NSData, NSString};

    let key = NSString::from_str("CFBundleIconName");
    if NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&key)
        .is_some()
    {
        return;
    }

    // 256px: the largest the Dock asks for, at 128pt on a Retina display.
    const ICON: &[u8] = include_bytes!("../icons/128x128@2x.png");

    let data = NSData::with_bytes(ICON);
    // SAFETY: `NSImage` and `NSApplication` are AppKit classes linked into every
    // Cocoa process, and this runs from the setup hook, which is on the main
    // thread. A decode failure yields nil, which `setApplicationIconImage:`
    // documents as "go back to the bundle's icon" rather than as a crash.
    unsafe {
        let image: *mut AnyObject = msg_send![objc2::class!(NSImage), alloc];
        let image: *mut AnyObject = msg_send![image, initWithData: &*data];
        if image.is_null() {
            orb_core::logger::warn("the Dock icon could not be decoded; leaving it as it was");
            return;
        }
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];
    }
}

/// Sync a single folder — the interactive refresh.
///
/// Bounded in a way `sync_all` is not: it takes the store mutex for one folder
/// instead of every folder of every account.
#[tauri::command]
async fn sync_folder(
    app: tauri::AppHandle,
    orb: State<'_, Handle>,
    account: String,
    folder: String,
) -> Result<SyncResult, OrbError> {
    let orb = orb.inner().clone();
    let after = orb.clone();
    let progress = app.clone();
    let result = offload("sync_folder", move || {
        orb.sync_folder(
            account,
            folder,
            Some(Box::new(ProgressBridge { app: progress })),
        )
    })
    .await;
    after_sync(&app, &after, new_mail_of(&result));
    result
}

/// Fetch a bounded number of missing bodies.
///
/// Driven repeatedly by the frontend rather than as one long call, so it can be
/// stopped between passes and cannot occupy a worker indefinitely.
#[tauri::command]
async fn fetch_bodies(
    app: tauri::AppHandle,
    orb: State<'_, Handle>,
    account: String,
    folders: Vec<String>,
    budget: u32,
) -> Result<BodyBackfillResult, OrbError> {
    let orb = orb.inner().clone();
    offload("fetch_bodies", move || {
        orb.fetch_bodies(
            account,
            folders,
            budget,
            Some(Box::new(ProgressBridge { app })),
        )
    })
    .await
}

#[tauri::command]
async fn body_backfill_progress(orb: State<'_, Handle>) -> Result<BodyBackfillResult, OrbError> {
    let orb = orb.inner().clone();
    offload("body_backfill_progress", move || {
        orb.body_backfill_progress()
    })
    .await
}

/// Sync everything, tasks included.
///
/// The core runs a task sync alongside a whole-mailbox mail sync — one moment
/// when Orb talks to the server, rather than a task list that lags the mail by
/// an unrelated interval — so this emits `tasks-changed` as well. Without it,
/// a task another machine created would sit in the database, synced and
/// invisible, until something else happened to refresh the list.
#[tauri::command]
async fn sync_all(app: tauri::AppHandle, orb: State<'_, Handle>) -> Result<SyncResult, OrbError> {
    let orb = orb.inner().clone();
    let after = orb.clone();
    let progress = app.clone();
    let result = mutate_tasks(app.clone(), "sync_all", move || {
        orb.sync_all(Some(Box::new(ProgressBridge { app: progress })))
    })
    .await;
    after_sync(&app, &after, new_mail_of(&result));
    result
}

/// The last sync of every folder, phase by phase, and the connection tally.
#[tauri::command]
async fn sync_stats(orb: State<'_, Handle>) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("sync_stats", move || Ok(orb.sync_stats())).await
}

#[tauri::command]
async fn bindings(orb: State<'_, Handle>) -> Result<BindingsConfig, OrbError> {
    let orb = orb.inner().clone();
    offload("bindings", move || Ok(orb.get_bindings())).await
}

#[tauri::command]
async fn send_mail(
    app: tauri::AppHandle,
    orb: State<'_, Handle>,
    input: SendMailInput,
) -> Result<SendMailResult, OrbError> {
    let orb = orb.inner().clone();
    let reply = input.in_reply_to.is_some() || input.forwards.is_some();
    let result = offload("send_mail", move || orb.send_mail(input)).await?;
    // The original is marked answered or forwarded here and now, so the push that follows
    // finds nothing new and would never redraw the list that shows it.
    if reply && result.sent {
        let _ = app.emit("folder-changed", (String::new(), String::new()));
    }
    if result.follow_up.is_some() {
        if let Some(orb) = app.try_state::<Handle>() {
            orb.task_changed();
        }
        let _ = app.emit(TASKS_CHANGED_EVENT, ());
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------
//
// Every command below is the same thin shim the mail commands are: the whole of
// the task engine — views, ordering, recurrence, the wire format, undo — lives
// in `orb_core`, and this file only carries it across the bridge.
//
// Two conventions worth knowing before adding to this section:
//
// - Parameter names are single words (`id`, `ids`, `task`, `message`, `at`).
//   Tauri renames command arguments to camelCase on the way in, so a `task_id`
//   here would have to be spelled `taskId` in the frontend — a rename waiting
//   to be got wrong in a language where the mistake is a runtime rejection
//   rather than a compile error.
// - Anything that changes a task goes through `mutate_tasks`, which emits
//   `tasks-changed` on success and hands the change to the core's uploader.
//   The frontend refreshes from one listener rather than every call site
//   remembering to, which is the same shape as the `sync-progress` bridge
//   above.

/// Emitted after any command that changed the task list.
///
/// Deliberately payload-free. A task mutation can move a task between views,
/// change the Today badge, advance a recurrence and re-order the list all at
/// once, so a payload describing "what changed" would either be a lie or a
/// second implementation of the views. Refetching what is on screen is cheap —
/// the task list is hundreds of rows, not a mailbox.
const TASKS_CHANGED_EVENT: &str = "tasks-changed";

/// Run a task mutation and tell the frontend the list moved.
///
/// The event fires only on success: a rejected edit — a postpone date after the
/// due date, an `RRULE` that will not parse (§2, §8) — leaves the list exactly
/// as it was, and a refresh triggered by a failure would make an error look
/// like something happened.
async fn mutate_tasks<T, F>(app: AppHandle, label: &'static str, f: F) -> Result<T, OrbError>
where
    F: FnOnce() -> Result<T, OrbError> + Send + 'static,
    T: Send + 'static,
{
    let result = offload(label, f).await;
    if result.is_ok() {
        if let Some(orb) = app.try_state::<Handle>() {
            orb.task_changed();
        }
        let _ = app.emit(TASKS_CHANGED_EVENT, ());
    }
    result
}

/// Emitted when a message is queued, sent, refused, retried or deleted.
/// Payload-free: the window asks `outbox`.
const OUTBOX_EVENT: &str = "outbox-changed";

/// Tells the windows of changes to the Outbox, and the user of a refusal.
struct OutboxBridge {
    app: tauri::AppHandle,
}

impl orb_core::store::OutboxListener for OutboxBridge {
    fn on_outbox_changed(&self, failed: Vec<orb_core::store::FailedSend>) {
        for send in &failed {
            notifications::notify_send_failed(send);
        }
        let _ = self.app.emit(OUTBOX_EVENT, ());
    }
}

/// Emitted when the number of task changes still to reach the server, or the
/// reason they cannot, may have changed. Payload-free: the window asks
/// `task_sync_status`.
const TASK_SYNC_EVENT: &str = "task-sync";

/// Tells the windows what the core's task uploader did (todo-spec §23).
struct TaskSyncBridge {
    app: tauri::AppHandle,
}

impl TaskSyncListener for TaskSyncBridge {
    fn on_task_sync(&self, event: TaskSyncEvent) {
        if event.tasks_changed {
            let _ = self.app.emit(TASKS_CHANGED_EVENT, ());
        }
        if event.problem_changed {
            let _ = self.app.emit(ACCOUNT_STATUS_EVENT, ());
        }
        let _ = self.app.emit(TASK_SYNC_EVENT, ());
    }
}

/// Whether every task change is on the server, and why not if not.
#[tauri::command]
async fn task_sync_status(orb: State<'_, Handle>) -> Result<TaskSyncStatus, OrbError> {
    let orb = orb.inner().clone();
    offload("task_sync_status", move || orb.task_sync_status()).await
}

/// One view of the task list, optionally filtered by the search box (§4.5).
#[tauri::command]
async fn list_tasks(
    orb: State<'_, Handle>,
    view: TaskViewName,
    search: Option<String>,
    page: Option<u32>,
) -> Result<Vec<TaskSummary>, OrbError> {
    let orb = orb.inner().clone();
    offload("list_tasks", move || orb.list_tasks(view, search, page)).await
}

/// What merging `others` into `keep` would make, for the dialog.
#[tauri::command]
async fn plan_task_merge(
    orb: State<'_, Handle>,
    keep: i64,
    others: Vec<i64>,
) -> Result<TaskMergePlan, OrbError> {
    let orb = orb.inner().clone();
    offload("plan_task_merge", move || orb.plan_task_merge(keep, others)).await
}

/// Merge `others` into `keep`, with the title, notes and dates the dialog
/// settled on; the others go to the Trash.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn merge_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
    keep: i64,
    others: Vec<i64>,
    title: String,
    body: Option<String>,
    postpone: Option<String>,
    due: Option<String>,
) -> Result<TaskDetail, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "merge_tasks", move || {
        orb.merge_tasks(keep, others, title, body, postpone, due)
    })
    .await
}

/// Hide the tasks completed so far from Today, on every machine (§10.2).
#[tauri::command]
async fn clear_completed_from_today(
    app: AppHandle,
    orb: State<'_, Handle>,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "clear_completed_from_today", move || {
        orb.clear_completed_from_today()
    })
    .await
}

/// Every task a search finds, whatever its view (§4.5).
#[tauri::command]
async fn search_tasks(orb: State<'_, Handle>, query: String) -> Result<Vec<TaskSummary>, OrbError> {
    let orb = orb.inner().clone();
    offload("search_tasks", move || orb.search_tasks(query)).await
}

/// How many tasks a view holds, ignoring pagination.
#[tauri::command]
async fn count_tasks(
    orb: State<'_, Handle>,
    view: TaskViewName,
    search: Option<String>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    offload("count_tasks", move || orb.count_tasks(view, search)).await
}

/// Whether a designated tasks account is configured at all (§1).
///
/// Cheap and infallible on purpose: the interface asks this before it offers a
/// single task view, so that a configuration with no `tasks` section produces
/// a mail client rather than five views that each fail.
#[tauri::command]
async fn tasks_configured(orb: State<'_, Handle>) -> Result<bool, OrbError> {
    Ok(orb.tasks_configured())
}

/// The number on the Today badge: open tasks only (§4.1).
#[tauri::command]
async fn today_task_count(orb: State<'_, Handle>) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    offload("today_task_count", move || orb.today_task_count()).await
}

/// When every date-dependent view next has to be re-evaluated (§4.3).
///
/// The native side already watches this and emits `task-day-changed`; the
/// command exists so a frontend that wants to show "rolls over at ..." can,
/// without a second definition of what midnight means.
#[tauri::command]
async fn next_task_midnight(orb: State<'_, Handle>) -> Result<String, OrbError> {
    let orb = orb.inner().clone();
    offload("next_task_midnight", move || orb.next_task_midnight()).await
}

/// Readings of what was typed into the date picker, best first (§12.1).
///
/// "Today" and the locale are the core's business, not an argument: a
/// suggestion is only correct relative to a particular local day, and the
/// WebView's `Date` is in whatever zone it decided on — the exact mismatch that
/// would make the picker offer a day the Today view disagrees with.
///
/// An empty list is a real answer and means "no matching date"; §12.1 is
/// explicit that unparseable input shows nothing rather than a guess.
#[tauri::command]
async fn suggest_dates(
    orb: State<'_, Handle>,
    input: String,
) -> Result<Vec<DateSuggestionInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("suggest_dates", move || orb.suggest_dates(input)).await
}

/// The readings of a recurrence typed by a person (§8).
#[tauri::command]
async fn suggest_recurrence(
    orb: State<'_, Handle>,
    input: String,
) -> Result<Vec<orb_core::ffi::RecurrenceSuggestionInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("suggest_recurrence", move || orb.suggest_recurrence(input)).await
}

/// [`suggest_dates`] for a reminder, which is a day *and* a time (§5.3).
#[tauri::command]
async fn suggest_datetimes(
    orb: State<'_, Handle>,
    input: String,
) -> Result<Vec<DateSuggestionInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("suggest_datetimes", move || orb.suggest_datetimes(input)).await
}

/// One task with everything the detail view shows.
#[tauri::command]
async fn get_task(orb: State<'_, Handle>, id: i64) -> Result<TaskDetail, OrbError> {
    let orb = orb.inner().clone();
    offload("get_task", move || orb.get_task(id)).await
}

/// A task's timeline (§11), oldest first.
#[tauri::command]
async fn task_log(orb: State<'_, Handle>, id: i64) -> Result<Vec<TaskLogItem>, OrbError> {
    let orb = orb.inner().clone();
    offload("task_log", move || orb.task_log(id)).await
}

/// Create a task. The caller opens it for editing straight away.
#[tauri::command]
async fn create_task(
    app: AppHandle,
    orb: State<'_, Handle>,
    title: String,
    // One word rather than `postpone_date`: Tauri renames a command's arguments
    // to camelCase on the JavaScript side, and this file has no other
    // multi-word argument to make that convention visible at the call sites.
    postpone: Option<String>,
) -> Result<TaskDetail, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "create_task", move || orb.create_task(title, postpone)).await
}

/// Edit one task.
///
/// `edit` carries an explicit `clear_*` beside every nullable field, because
/// JSON cannot tell "set this to nothing" from "leave it alone" — see
/// `TaskEdit` in the core for why that distinction is not negotiable here.
#[tauri::command]
async fn update_task(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
    edit: TaskEdit,
) -> Result<TaskDetail, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "update_task", move || orb.update_task(id, edit)).await
}

/// How often the export timer looks whether an export is due. The directory
/// says when the last one was, so a check after a sleep or a relaunch is
/// enough to catch up; the first waits for the launch to settle.
const EXPORT_CHECK: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const EXPORT_FIRST_CHECK: std::time::Duration = std::time::Duration::from_secs(2 * 60);
const TASK_EXPORT_EVENT: &str = "task-export";

/// Export the tasks whenever one is due (todo-spec §20.3), and say how it
/// went: to every window as an event, and as a notification when it failed.
/// How often the feed timer looks whether a feed is due; each feed says
/// itself how often it is polled (`reading.poll_minutes`).
const FEED_CHECK: std::time::Duration = std::time::Duration::from_secs(60);
const FEED_FIRST_CHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// Poll the reading list's feeds as they fall due, and show what arrived.
fn start_feed_polls(app: AppHandle, orb: Handle) {
    std::thread::spawn(move || {
        std::thread::sleep(FEED_FIRST_CHECK);
        loop {
            if let Ok(report) = orb.poll_feeds(false) {
                announce_feeds(&app, &orb, &report);
            }
            std::thread::sleep(FEED_CHECK);
        }
    });
}

/// New items show as mail arriving in the reading folder does.
fn announce_feeds(app: &AppHandle, orb: &Handle, report: &orb_core::store::FeedPollReport) {
    if report.appended == 0 {
        return;
    }
    if let Some((account, folder)) = orb.reading_folder() {
        let _ = app.emit("folder-changed", (account, folder));
    }
}

/// Poll every feed of the reading list now.
#[tauri::command]
async fn poll_feeds(
    app: AppHandle,
    orb: State<'_, Handle>,
) -> Result<orb_core::store::FeedPollReport, OrbError> {
    let orb = orb.inner().clone();
    let report = offload("poll_feeds", {
        let orb = orb.clone();
        move || orb.poll_feeds(true)
    })
    .await?;
    announce_feeds(&app, &orb, &report);
    Ok(report)
}

/// Every feed of the reading list, as this machine last heard from it.
#[tauri::command]
async fn feed_statuses(
    orb: State<'_, Handle>,
) -> Result<Vec<orb_core::store::FeedStatus>, OrbError> {
    let orb = orb.inner().clone();
    offload("feed_statuses", move || orb.feed_statuses()).await
}

fn start_task_export(app: AppHandle, orb: Handle) {
    std::thread::spawn(move || {
        std::thread::sleep(EXPORT_FIRST_CHECK);
        loop {
            if let Some(outcome) = orb.export_tasks_if_due() {
                announce_export(&app, outcome);
            }
            std::thread::sleep(EXPORT_CHECK);
        }
    });
}

fn announce_export(app: &AppHandle, outcome: Result<orb_core::ffi::TaskExportResult, OrbError>) {
    let payload = match &outcome {
        Ok(done) => {
            if !done.complete {
                notifications::notify_export_problem(&format!(
                    "The snapshot is incomplete: {}",
                    done.missing.join("; ")
                ));
            }
            serde_json::json!({ "result": done, "error": null })
        }
        Err(e) => {
            notifications::notify_export_problem(&e.to_string());
            serde_json::json!({ "result": null, "error": e.to_string() })
        }
    };
    let _ = app.emit(TASK_EXPORT_EVENT, payload);
}

/// The code that sets Orb up on an iPhone, password included — after a
/// native dialog has said so. The code is the account's password on the
/// screen, so making one is a decision a person takes in a dialog the page
/// cannot answer; `None` when the dialog was cancelled.
#[tauri::command]
async fn iphone_setup_code(
    app: AppHandle,
    orb: State<'_, Handle>,
) -> Result<Option<orb_core::ffi::IphoneSetupCode>, OrbError> {
    let dialog = app
        .dialog()
        .message(
            "The code holds the tasks account's password. Anyone who sees or photographs it \
             can read this account's mail. It works for five minutes, and goes away when Orb \
             leaves the front.",
        )
        .title("Show the code that sets up Orb on an iPhone?")
        .kind(MessageDialogKind::Warning)
        .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
            "Show the Code".to_string(),
            "Cancel".to_string(),
        ));
    let go = tauri::async_runtime::spawn_blocking(move || dialog.blocking_show())
        .await
        .unwrap_or(false);
    if !go {
        return Ok(None);
    }
    let orb = orb.inner().clone();
    offload("iphone_setup_code", move || orb.iphone_setup_code())
        .await
        .map(Some)
}

/// Export the tasks now, to the configured directory (todo-spec §20.3).
#[tauri::command]
async fn export_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
) -> Result<orb_core::ffi::TaskExportResult, OrbError> {
    let orb = orb.inner().clone();
    let outcome = offload("export_tasks", move || orb.export_tasks()).await;
    if let Ok(done) = &outcome {
        if !done.complete {
            notifications::notify_export_problem(&format!(
                "The snapshot is incomplete: {}",
                done.missing.join("; ")
            ));
        }
    }
    let _ = app.emit(TASK_EXPORT_EVENT, ());
    outcome
}

/// Import a task snapshot (todo-spec §21): with `apply` false, say what it
/// would do; with it, do it.
#[tauri::command]
async fn import_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
    path: String,
    apply: bool,
    include_purged: bool,
) -> Result<orb_core::ffi::TaskImportPlan, OrbError> {
    let orb = orb.inner().clone();
    if apply {
        mutate_tasks(app, "import_tasks", move || {
            orb.import_tasks(path, true, include_purged)
        })
        .await
    } else {
        offload("import_tasks", move || {
            orb.import_tasks(path, false, include_purged)
        })
        .await
    }
}

/// Import Things 3 (todo-spec §22): with `apply` false, say what it would do.
#[tauri::command]
async fn import_things(
    app: AppHandle,
    orb: State<'_, Handle>,
    path: Option<String>,
    apply: bool,
) -> Result<orb_core::ffi::ThingsImportPlan, OrbError> {
    let orb = orb.inner().clone();
    if apply {
        mutate_tasks(app, "import_things", move || orb.import_things(path, true)).await
    } else {
        offload("import_things", move || orb.import_things(path, false)).await
    }
}

/// Where the task export stands, for the status line.
#[tauri::command]
async fn task_export_status(
    orb: State<'_, Handle>,
) -> Result<orb_core::ffi::TaskExportStatus, OrbError> {
    let orb = orb.inner().clone();
    offload("task_export_status", move || Ok(orb.task_export_status())).await
}

/// The tasks marked as being worked on on this machine (§19).
#[tauri::command]
async fn working_on_tasks(orb: State<'_, Handle>) -> Result<Vec<TaskSummary>, OrbError> {
    let orb = orb.inner().clone();
    offload("working_on_tasks", move || orb.working_on_tasks()).await
}

/// Take the working-on mark off every task (§19).
#[tauri::command]
async fn clear_working_on(app: AppHandle, orb: State<'_, Handle>) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "clear_working_on", move || orb.clear_working_on()).await
}

/// Change a task's checklist (§17).
#[tauri::command]
async fn edit_task_step(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
    edit: orb_core::ffi::StepEdit,
) -> Result<TaskDetail, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "edit_task_step", move || orb.edit_task_step(id, edit)).await
}

/// Apply one edit to a whole selection, as a single undo step (§10.3).
#[tauri::command]
async fn update_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
    edit: TaskEdit,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "update_tasks", move || orb.update_tasks(ids, edit)).await
}

/// Complete a selection. A recurring task advances instead of ending (§8).
#[tauri::command]
async fn complete_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "complete_tasks", move || orb.complete_tasks(ids)).await
}

#[tauri::command]
async fn uncomplete_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "uncomplete_tasks", move || orb.uncomplete_tasks(ids)).await
}

/// Soft-delete a selection into the Trash view (§10.1).
#[tauri::command]
async fn trash_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "trash_tasks", move || orb.trash_tasks(ids)).await
}

#[tauri::command]
async fn restore_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "restore_tasks", move || orb.restore_tasks(ids)).await
}

/// Expunge the trash for good. Not undoable, and the core clears both stacks.
#[tauri::command]
async fn empty_task_trash(app: AppHandle, orb: State<'_, Handle>) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "empty_task_trash", move || orb.empty_task_trash()).await
}

/// Extensions macOS would *run*, launch or install rather than display.
///
/// An attachment arrives over the network and its filename comes with it, so
/// handing one of these to `open` is handing an unread message the ability to
/// execute: programs and scripts, installers and archives of them, and the
/// link files Launch Services opens by following whatever address they hold.
/// These are revealed in Finder instead, where opening them is a decision
/// somebody makes on purpose.
const EXECUTABLE_SUFFIXES: &[&str] = &[
    ".app",
    ".command",
    ".tool",
    ".sh",
    ".bash",
    ".zsh",
    ".csh",
    ".ksh",
    ".fish",
    ".scpt",
    ".scptd",
    ".applescript",
    ".workflow",
    ".action",
    ".terminal",
    ".pkg",
    ".mpkg",
    ".xip",
    ".jar",
    ".inetloc",
    ".webloc",
    ".fileloc",
    ".url",
    ".mobileconfig",
];

/// What happened when an attachment was opened.
#[derive(serde::Serialize)]
pub(crate) struct OpenedAttachment {
    path: String,
    /// True when the file was revealed in Finder rather than opened, because
    /// opening it would have run it.
    revealed: bool,
    /// True when the file was dangerous to open and the user said no.
    declined: bool,
}

/// Materialise a task attachment on disk and hand it to the platform (§9).
///
/// Downloading, saving and opening are one gesture here: the payload's home is
/// the cache, so there is nothing to choose a location for, and what the person
/// wanted was to look at the file.
#[tauri::command]
async fn open_task_attachment(
    app: AppHandle,
    orb: State<'_, Handle>,
    task: i64,
    attachment: String,
) -> Result<OpenedAttachment, OrbError> {
    let orb = orb.inner().clone();
    let path = offload("open_task_attachment", move || {
        orb.task_attachment_path(task, attachment)
    })
    .await?;
    open_on_disk(&app, path).await
}

/// Whether macOS would run `name` rather than display it (see
/// [`EXECUTABLE_SUFFIXES`]). Trailing dots and spaces do not count: Finder
/// hides them and Launch Services looks past them.
fn runs_rather_than_displays(name: &str) -> bool {
    let lowered = name.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    EXECUTABLE_SUFFIXES
        .iter()
        .any(|suffix| lowered.ends_with(suffix))
}

/// Hand a file to the system — or reveal it in Finder when opening it would
/// run it, and ask first when opening it is dangerous for another reason.
///
/// The question is asked here, as a native dialog, rather than left to the
/// page: the page's judgement is advice, and a page that had got script could
/// skip its own.
pub(crate) async fn open_on_disk(
    app: &AppHandle,
    path: String,
) -> Result<OpenedAttachment, OrbError> {
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let revealed = runs_rather_than_displays(&name);
    let risk = if revealed {
        None
    } else {
        orb_core::safety::attachment_risk(&name)
    };
    if let Some(risk) = risk {
        let dialog = app
            .dialog()
            .message(format!(
                "This file {risk}. Open it only if you expected it from this sender."
            ))
            .title(format!("Open {name}?"))
            .kind(MessageDialogKind::Warning)
            .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                "Open".to_string(),
                "Cancel".to_string(),
            ));
        let go = tauri::async_runtime::spawn_blocking(move || dialog.blocking_show())
            .await
            .unwrap_or(false);
        if !go {
            return Ok(OpenedAttachment {
                path,
                revealed: false,
                declined: true,
            });
        }
    }

    // The path is a single argument to `open`, never a shell string, so a
    // filename full of metacharacters is a filename and nothing more.
    let mut command = std::process::Command::new("open");
    if revealed {
        command.arg("-R");
    }
    command
        .arg(&path)
        .spawn()
        .map_err(|e| OrbError::ConfigError {
            message: format!("could not open the attachment: {e}"),
        })?;

    Ok(OpenedAttachment {
        path,
        revealed,
        declined: false,
    })
}

/// Erase a task that was never really made, without going through the trash.
///
/// The escape hatch of `create-task`: the row exists only so the detail view
/// has something to edit, and abandoning that edit must leave nothing behind.
#[tauri::command]
async fn discard_task(app: AppHandle, orb: State<'_, Handle>, id: i64) -> Result<bool, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "discard_task", move || orb.discard_task(id)).await
}

/// Set the postpone date of a selection (§12). `date` is `YYYY-MM-DD`; `null`
/// clears it.
#[tauri::command]
async fn set_task_postpone_date(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
    date: Option<String>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "set_task_postpone_date", move || {
        orb.set_task_postpone_date(ids, date)
    })
    .await
}

/// Set the due date of a selection (§12). Setting it before an existing
/// postpone date clamps that date rather than failing.
#[tauri::command]
async fn set_task_due_date(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
    date: Option<String>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "set_task_due_date", move || {
        orb.set_task_due_date(ids, date)
    })
    .await
}

/// Clear both dates of a selection, sending it back to Someday.
#[tauri::command]
async fn clear_task_dates(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "clear_task_dates", move || orb.clear_task_dates(ids)).await
}

/// Move a task in the single global order (§4.4).
///
/// `before` is the task it lands after and `after` the task it lands before;
/// `null` for either is the corresponding end of the list.
#[tauri::command]
async fn move_task(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
    before: Option<i64>,
    after: Option<i64>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "move_task", move || orb.move_task(id, before, after)).await
}

/// Move a selection as one block between two neighbours.
#[tauri::command]
async fn move_tasks(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
    before: Option<i64>,
    after: Option<i64>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "move_tasks", move || {
        orb.move_tasks(ids, before, after)
    })
    .await
}

/// Take back the last action of any kind: mail, tasks or chat.
///
/// Returns what was undone, for the status line, or `null` when there is
/// nothing left to take back.
#[tauri::command]
async fn undo(app: AppHandle, orb: State<'_, Handle>) -> Result<Option<UndoReport>, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = mutate_tasks(app.clone(), "undo", move || orb.undo()).await;
    update_badge_later(&app, &badge);
    result
}

#[tauri::command]
async fn redo(app: AppHandle, orb: State<'_, Handle>) -> Result<Option<UndoReport>, OrbError> {
    let orb = orb.inner().clone();
    let badge = orb.clone();
    let result = mutate_tasks(app.clone(), "redo", move || orb.redo()).await;
    update_badge_later(&app, &badge);
    result
}

/// Start a chain of actions that one undo takes back whole.
#[tauri::command]
fn begin_undo_group(orb: State<'_, Handle>) -> u64 {
    orb.begin_undo_group()
}

#[tauri::command]
fn end_undo_group(orb: State<'_, Handle>, mark: u64, label: Option<String>) {
    orb.end_undo_group(mark, label);
}

/// The tag registry (§7).
#[tauri::command]
async fn list_task_tags(orb: State<'_, Handle>) -> Result<Vec<TaskTagInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("list_task_tags", move || orb.list_task_tags()).await
}

#[tauri::command]
async fn create_task_tag(
    app: AppHandle,
    orb: State<'_, Handle>,
    name: String,
    colour: Option<String>,
) -> Result<TaskTagInfo, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "create_task_tag", move || {
        orb.create_task_tag(name, colour)
    })
    .await
}

/// Rename or recolour a tag. One registry message is rewritten, not every task
/// that carries the tag — which is why tasks store tag ids and not names (§7).
#[tauri::command]
async fn update_task_tag(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: String,
    name: String,
    colour: Option<String>,
) -> Result<bool, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "update_task_tag", move || {
        orb.update_task_tag(id, name, colour)
    })
    .await
}

/// Delete a tag. Removes it from tasks; never removes the tasks (§7).
#[tauri::command]
async fn delete_task_tag(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: String,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "delete_task_tag", move || orb.delete_task_tag(id)).await
}

#[tauri::command]
async fn add_task_tag(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
    tag: String,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "add_task_tag", move || orb.add_task_tag(ids, tag)).await
}

#[tauri::command]
async fn remove_task_tag(
    app: AppHandle,
    orb: State<'_, Handle>,
    ids: Vec<i64>,
    tag: String,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "remove_task_tag", move || {
        orb.remove_task_tag(ids, tag)
    })
    .await
}

/// Replace a task's whole tag set.
#[tauri::command]
async fn set_task_tags(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
    tags: Vec<String>,
) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "set_task_tags", move || orb.set_task_tags(id, tags)).await
}

/// A task's reminders (§5).
#[tauri::command]
async fn list_task_reminders(
    orb: State<'_, Handle>,
    task: i64,
) -> Result<Vec<TaskReminderInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("list_task_reminders", move || orb.list_task_reminders(task)).await
}

/// Add a reminder. `at` is a local RFC3339 instant, as the date picker produces
/// it (§5.3).
///
/// The core re-arms the OS's pending window as part of this, so a reminder set
/// now is scheduled with the system before the command returns — including one
/// set offline (§3.6).
#[tauri::command]
async fn add_task_reminder(
    app: AppHandle,
    orb: State<'_, Handle>,
    task: i64,
    at: String,
) -> Result<i64, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "add_task_reminder", move || {
        orb.add_task_reminder(task, at)
    })
    .await
}

#[tauri::command]
async fn delete_task_reminder(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "delete_task_reminder", move || {
        orb.delete_task_reminder(id)
    })
    .await
}

/// Push a reminder back. Returns its new instant.
#[tauri::command]
async fn snooze_task_reminder(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
    minutes: i64,
) -> Result<i64, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "snooze_task_reminder", move || {
        orb.snooze_task_reminder(id, minutes)
    })
    .await
}

/// What the OS should be holding pending, given what it says it holds (§5.1).
///
/// The native side applies this on its own after every mutation, sync and local
/// midnight; the command is here for a frontend that wants to show the window,
/// or to force a re-fill after a run of edits.
#[tauri::command]
async fn task_notification_plan(
    orb: State<'_, Handle>,
    pending: Vec<String>,
) -> Result<TaskNotificationPlan, OrbError> {
    let orb = orb.inner().clone();
    offload("task_notification_plan", move || {
        orb.task_notification_plan(pending)
    })
    .await
}

/// Report that a notification was delivered, so the launch sweep does not
/// show it again (§5.2).
#[tauri::command]
async fn record_task_notification_delivered(
    orb: State<'_, Handle>,
    identifier: String,
) -> Result<bool, OrbError> {
    let orb = orb.inner().clone();
    offload("record_task_notification_delivered", move || {
        orb.record_task_notification_delivered(identifier)
    })
    .await
}

/// The launch sweep (§5.2).
///
/// Already run once natively at startup, which is where the notifications
/// actually come from. Exposed so the frontend can run it again after a long
/// suspend without waiting for a relaunch.
#[tauri::command]
async fn catch_up_task_reminders(orb: State<'_, Handle>) -> Result<TaskCatchUp, OrbError> {
    let orb = orb.inner().clone();
    offload("catch_up_task_reminders", move || {
        orb.catch_up_task_reminders()
    })
    .await
}

/// A task's attachments — metadata only; payloads come from
/// `task_attachment_data` (§9).
#[tauri::command]
async fn list_task_attachments(
    orb: State<'_, Handle>,
    task: i64,
) -> Result<Vec<TaskAttachmentInfo>, OrbError> {
    let orb = orb.inner().clone();
    offload("list_task_attachments", move || {
        orb.list_task_attachments(task)
    })
    .await
}

#[tauri::command]
async fn add_task_attachment(
    app: AppHandle,
    orb: State<'_, Handle>,
    task: i64,
    path: String,
) -> Result<TaskAttachmentInfo, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "add_task_attachment", move || {
        orb.add_task_attachment(task, path)
    })
    .await
}

#[tauri::command]
async fn delete_task_attachment(
    app: AppHandle,
    orb: State<'_, Handle>,
    id: i64,
) -> Result<(), OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "delete_task_attachment", move || {
        orb.delete_task_attachment(id)
    })
    .await
}

/// The bytes of an attachment, downloading them if this machine has never seen
/// them. A network round trip on a cache miss, so run it as a background task.
#[tauri::command]
async fn task_attachment_data(
    orb: State<'_, Handle>,
    task: i64,
    attachment: String,
) -> Result<AttachmentData, OrbError> {
    let orb = orb.inner().clone();
    offload("task_attachment_data", move || {
        orb.task_attachment_data(task, attachment)
    })
    .await
}

/// Sync the tasks folder on its own (§3).
///
/// Tasks also ride along with every whole-mailbox mail sync; this is the
/// explicit "sync tasks" a key can be bound to.
#[tauri::command]
async fn sync_tasks(app: AppHandle, orb: State<'_, Handle>) -> Result<TaskSyncResult, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "sync_tasks", move || orb.sync_tasks()).await
}

/// One task per message in a selection, each linked to its message (§6.1).
#[tauri::command]
async fn create_tasks_from_messages(
    app: AppHandle,
    orb: State<'_, Handle>,
    messages: Vec<i64>,
    postpone: Option<String>,
    due: Option<String>,
) -> Result<Vec<TaskDetail>, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "create_tasks_from_messages", move || {
        orb.create_tasks_from_messages(messages, postpone, due)
    })
    .await
}

/// Link an existing message to an existing task.
#[tauri::command]
async fn link_message_to_task(
    app: AppHandle,
    orb: State<'_, Handle>,
    task: i64,
    message: i64,
) -> Result<TaskLinkInfo, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "link_message_to_task", move || {
        orb.link_message_to_task(task, message)
    })
    .await
}

/// Remove a link, leaving both the task and the message alone. `message` is the
/// RFC `Message-ID`, not a row id — the link outlives the row.
#[tauri::command]
async fn unlink_message_from_task(
    app: AppHandle,
    orb: State<'_, Handle>,
    task: i64,
    message: String,
) -> Result<bool, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "unlink_message_from_task", move || {
        orb.unlink_message_from_task(task, message)
    })
    .await
}

/// Link the mail every task's notes name by `message://` URL; how many tasks
/// changed.
#[tauri::command]
async fn link_mail_in_notes(app: AppHandle, orb: State<'_, Handle>) -> Result<u32, OrbError> {
    let orb = orb.inner().clone();
    mutate_tasks(app, "link_mail_in_notes", move || orb.link_mail_in_notes()).await
}

/// Resolve a link from the local index alone — §6.2 step 1, instant and
/// network-free, so a task detail can call it while rendering.
#[tauri::command]
async fn linked_message(
    orb: State<'_, Handle>,
    task: i64,
    message: String,
) -> Result<LinkedMessage, OrbError> {
    let orb = orb.inner().clone();
    offload("linked_message", move || orb.linked_message(task, message)).await
}

/// Resolve a link the slow way — §6.2 steps 2 and 3.
///
/// Searches the designated account folder by folder for the `Message-ID`,
/// starting with the one the link remembers. That is a round trip per folder,
/// which is why §6.2 has this run as a visible background task (`runTask`) and
/// never during a render. A miss is not an error: the cached metadata comes
/// back marked `cached`, which is the read-only "message no longer available"
/// state.
#[tauri::command]
async fn resolve_linked_message(
    orb: State<'_, Handle>,
    task: i64,
    message: String,
) -> Result<LinkedMessage, OrbError> {
    let orb = orb.inner().clone();
    offload("resolve_linked_message", move || {
        orb.resolve_linked_message(task, message)
    })
    .await
}

/// How many open tasks each of these messages has — the message list's column
/// (§6.3).
///
/// Two queries for the whole visible set, never one per row. Messages with no
/// tasks are absent from the result rather than reported as zero, because the
/// column draws nothing for them.
#[tauri::command]
async fn open_task_counts(
    orb: State<'_, Handle>,
    messages: Vec<i64>,
) -> Result<Vec<MessageTaskCount>, OrbError> {
    let orb = orb.inner().clone();
    offload("open_task_counts", move || orb.open_task_counts(messages)).await
}

/// The tasks linked to one message, for the detail view (§6.3).
///
/// Ordinary summaries, so they can be acted on in place with the same commands
/// the task list uses.
#[tauri::command]
async fn tasks_for_message(
    orb: State<'_, Handle>,
    message: i64,
) -> Result<Vec<TaskSummary>, OrbError> {
    let orb = orb.inner().clone();
    offload("tasks_for_message", move || orb.tasks_for_message(message)).await
}

// ---------------------------------------------------------------------------
// Native context menu
// ---------------------------------------------------------------------------

/// One entry of the message context menu: (menu id, label).
///
/// The ids are the same command names the keyboard uses, so a menu entry and a
/// binding cannot drift apart — both end up in the frontend's command registry.
const MESSAGE_MENU: &[(&str, &str)] = &[
    ("open-message-detail", "Open"),
    ("reply", "Reply"),
    ("", "---"),
    ("toggle-read", "Toggle Read"),
    ("toggle-flag", "Toggle Flag"),
    ("", "---"),
    ("archive", "Archive"),
    ("move-immediate", "Move to…"),
    ("spam", "Move to Junk"),
    ("delete-immediate", "Move to Trash"),
    ("", "---"),
    ("mark-delete", "Mark for Deletion"),
    ("mark-move", "Mark to Move…"),
    ("unmark", "Unmark"),
    ("expunge", "Execute Marked Actions"),
    ("", "---"),
    ("sync", "Sync Folder"),
];

/// The task list's context menu: every entry a command the task modes have.
const TASK_MENU: &[(&str, &str)] = &[
    ("open-task-detail", "Open"),
    ("edit-task", "Edit"),
    ("", "---"),
    ("complete-task", "Complete"),
    ("uncomplete-task", "Un-complete"),
    ("work-on", "Working On"),
    ("", "---"),
    ("postpone-task", "Postpone…"),
    ("set-due-date", "Due…"),
    ("clear-dates", "Clear Dates"),
    ("set-recurrence", "Repeat…"),
    ("", "---"),
    ("add-reminder", "Add Reminder…"),
    ("add-tag", "Add Tag…"),
    ("attach-file", "Attach File…"),
    ("link-message", "Link Message…"),
    ("", "---"),
    ("merge-tasks", "Merge…"),
    ("delete-task", "Move to Trash"),
    ("restore-task", "Restore"),
];

/// The context menu of a step in a task's checklist (§17).
const STEP_MENU: &[(&str, &str)] = &[
    ("edit-step", "Rename"),
    ("toggle-step", "Mark as Done"),
    ("", "---"),
    ("remove-step", "Delete Step"),
];

/// The context menu of a chat thread in Waiting.
const CHAT_MENU: &[(&str, &str)] = &[
    ("chat-reply", "Reply"),
    ("chat-done", "Done"),
    ("", "---"),
    ("chat-open-in-browser", "Open in Rocket.Chat"),
    ("", "---"),
    ("chat-sync", "Check for Messages"),
    (SUBMENU_START, "Mute Room"),
    ("chat-mute-room 1h", "For 1 Hour"),
    ("chat-mute-room 4h", "For 4 Hours"),
    ("chat-mute-room today", "Until Tomorrow"),
    ("chat-mute-room forever", "Until I Unmute"),
    ("", "---"),
    ("chat-mute-room", "For…"),
    (SUBMENU_END, ""),
    ("chat-unmute-room", "Unmute Room"),
    ("chat-leave-room", "Leave Room…"),
];

/// The context menu of a chat room in the sidebar.
const CHAT_ROOM_MENU: &[(&str, &str)] = &[
    (SUBMENU_START, "Mute Room"),
    ("chat-mute-room 1h", "For 1 Hour"),
    ("chat-mute-room 4h", "For 4 Hours"),
    ("chat-mute-room today", "Until Tomorrow"),
    ("chat-mute-room forever", "Until I Unmute"),
    ("", "---"),
    ("chat-mute-room", "For…"),
    (SUBMENU_END, ""),
    ("chat-unmute-room", "Unmute Room"),
    ("", "---"),
    ("toggle-pin", "Pin to Sidebar"),
    ("chat-leave-room", "Leave Room…"),
];

/// Show the native chat context menu at the pointer; see
/// [`open_message_menu`] for how a choice comes back.
#[tauri::command]
fn open_chat_menu(
    app: tauri::AppHandle,
    window: tauri::Window,
    disabled: Option<Vec<String>>,
    labels: Option<std::collections::HashMap<String, String>>,
    accelerators: Option<std::collections::HashMap<String, String>>,
    hints: Option<std::collections::HashMap<String, String>>,
) -> Result<(), OrbError> {
    popup_menu(
        &app,
        &window,
        CHAT_MENU,
        &disabled.unwrap_or_default(),
        &labels.unwrap_or_default(),
        &MenuKeys {
            accelerators: accelerators.unwrap_or_default(),
            hints: hints.unwrap_or_default(),
        },
    )
}

/// Show the native context menu of a chat room in the sidebar; see
/// [`open_message_menu`] for how a choice comes back.
#[tauri::command]
fn open_chat_room_menu(
    app: tauri::AppHandle,
    window: tauri::Window,
    disabled: Option<Vec<String>>,
    labels: Option<std::collections::HashMap<String, String>>,
    accelerators: Option<std::collections::HashMap<String, String>>,
    hints: Option<std::collections::HashMap<String, String>>,
) -> Result<(), OrbError> {
    popup_menu(
        &app,
        &window,
        CHAT_ROOM_MENU,
        &disabled.unwrap_or_default(),
        &labels.unwrap_or_default(),
        &MenuKeys {
            accelerators: accelerators.unwrap_or_default(),
            hints: hints.unwrap_or_default(),
        },
    )
}

/// Show the native task context menu at the pointer; see
/// [`open_message_menu`] for how a choice comes back.
#[tauri::command]
fn open_task_menu(
    app: tauri::AppHandle,
    window: tauri::Window,
    disabled: Option<Vec<String>>,
    labels: Option<std::collections::HashMap<String, String>>,
    accelerators: Option<std::collections::HashMap<String, String>>,
    hints: Option<std::collections::HashMap<String, String>>,
) -> Result<(), OrbError> {
    popup_menu(
        &app,
        &window,
        TASK_MENU,
        &disabled.unwrap_or_default(),
        &labels.unwrap_or_default(),
        &MenuKeys {
            accelerators: accelerators.unwrap_or_default(),
            hints: hints.unwrap_or_default(),
        },
    )
}

/// Show the context menu of the selected steps; see [`open_message_menu`]
/// for how a choice comes back.
#[tauri::command]
fn open_step_menu(
    app: tauri::AppHandle,
    window: tauri::Window,
    disabled: Option<Vec<String>>,
    labels: Option<std::collections::HashMap<String, String>>,
) -> Result<(), OrbError> {
    popup_menu(
        &app,
        &window,
        STEP_MENU,
        &disabled.unwrap_or_default(),
        &labels.unwrap_or_default(),
        &MenuKeys {
            accelerators: Default::default(),
            hints: Default::default(),
        },
    )
}

/// The keys bound to a menu's commands, as the frontend found them: a key
/// equivalent where the menu can draw one, a hint after the label where it
/// cannot (a sequence such as `gx`).
struct MenuKeys {
    accelerators: std::collections::HashMap<String, String>,
    hints: std::collections::HashMap<String, String>,
}

/// Build a menu of command entries and show it at the pointer.
///
/// The entries the frontend names in `disabled` do not apply to what was
/// clicked, and are greyed out; `labels` renames an entry for it — "Mark as
/// Read" rather than a toggle that does not say which way it goes. Each entry
/// shows the key bound to it.
fn popup_menu(
    app: &tauri::AppHandle,
    window: &tauri::Window,
    entries: &[(&str, &str)],
    disabled: &[String],
    labels: &std::collections::HashMap<String, String>,
    keys: &MenuKeys,
) -> Result<(), OrbError> {
    use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder, SubmenuBuilder};

    let failed = |what: &str, e: tauri::Error| OrbError::ConfigError {
        message: format!("{what}: {e}"),
    };
    let item = |id: &str, label: &str| -> Result<MenuItem<tauri::Wry>, OrbError> {
        let label = labels.get(id).map(String::as_str).unwrap_or(label);
        let enabled = !disabled.iter().any(|d| d == id);
        let plain = |text: &str| {
            MenuItemBuilder::with_id(id, text)
                .enabled(enabled)
                .build(app)
                .map_err(|e| failed(&format!("menu item '{label}'"), e))
        };
        let hinted = |hint: &str| plain(&format!("{label}   {hint}"));
        // A key the menu cannot take as a key equivalent goes after the label.
        match (keys.accelerators.get(id), keys.hints.get(id)) {
            (Some(key), _) => MenuItemBuilder::with_id(id, label)
                .enabled(enabled)
                .accelerator(key)
                .build(app)
                .or_else(|_| hinted(key)),
            (None, Some(hint)) => hinted(hint),
            (None, None) => plain(label),
        }
    };

    let mut builder = MenuBuilder::new(app);
    let mut entries = entries.iter();
    while let Some((id, label)) = entries.next() {
        if *label == "---" {
            builder = builder.separator();
        } else if *id == SUBMENU_START {
            // The entries up to its end go into a submenu of their own.
            let mut submenu = SubmenuBuilder::new(app, *label);
            for (id, label) in entries.by_ref().take_while(|(id, _)| *id != SUBMENU_END) {
                submenu = submenu.item(&item(id, label)?);
            }
            builder = builder.item(&submenu.build().map_err(|e| failed(label, e))?);
        } else {
            builder = builder.item(&item(id, label)?);
        }
    }
    let menu = builder.build().map_err(|e| failed("context menu", e))?;
    window
        .popup_menu(&menu)
        .map_err(|e| failed("could not show context menu", e))?;
    Ok(())
}

/// In a context menu's entries, what starts a submenu, labelled by its label,
/// and what ends it.
const SUBMENU_START: &str = ">";
const SUBMENU_END: &str = "<";

/// Show the native message context menu at the pointer.
///
/// Selecting an entry emits `context-menu-command` carrying the command string,
/// which the frontend runs through the same registry the keybindings use. A
/// dismissed menu simply emits nothing.
#[tauri::command]
fn open_message_menu(
    app: tauri::AppHandle,
    window: tauri::Window,
    disabled: Option<Vec<String>>,
    labels: Option<std::collections::HashMap<String, String>>,
    accelerators: Option<std::collections::HashMap<String, String>>,
    hints: Option<std::collections::HashMap<String, String>>,
) -> Result<(), OrbError> {
    popup_menu(
        &app,
        &window,
        MESSAGE_MENU,
        &disabled.unwrap_or_default(),
        &labels.unwrap_or_default(),
        &MenuKeys {
            accelerators: accelerators.unwrap_or_default(),
            hints: hints.unwrap_or_default(),
        },
    )
}

/// Set once the app has decided to go, so the exit that follows is not
/// stopped and turned into a question again.
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Quit the app, once the mailbox window has asked about anything at stake.
///
/// Every compose window is closed first, the way the user would close it,
/// so each saves its draft before the process goes; an exit straight away
/// would lose whatever was written since the last save. A window that has
/// not finished after ten seconds is not waited for.
#[tauri::command]
async fn quit_app(app: tauri::AppHandle) {
    QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
    let composing = || {
        app.webview_windows()
            .into_iter()
            .filter(|(label, _)| label.starts_with("compose-"))
            .map(|(_, window)| window)
            .collect::<Vec<_>>()
    };
    for window in composing() {
        let _ = window.close();
    }
    for _ in 0..100 {
        if composing().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    app.exit(0);
}

/// Commands that make sense whichever window is in front; the rest act on
/// the mailbox and are dropped while another window has the focus.
const GLOBAL_MENU_COMMANDS: &[&str] = &[
    "compose",
    "console",
    "help",
    "quit",
    "zoom-in",
    "zoom-out",
    "zoom-reset",
    "toggle-look",
    "open-tasks-window",
    "open-chat-window",
    "set-up-iphone",
    // Chat lives in the mailbox window, and in the chat window when it is in front.
    "open-chat",
    "chat-open-room",
    "chat-muted-rooms",
    "chat-new-message",
    "chat-create-room",
    "chat-sign-in",
    "chat-sign-out",
];

/// Open a link from a message in the user's browser.
///
/// Mail is hostile input, so the scheme is checked here rather than trusted
/// from the page: `http`, `https` and `mailto` are the only ones a message has
/// any business asking for. Anything else — `file:` reading off the disk,
/// `javascript:`, a custom scheme registered by some other application — is
/// refused. The URL is handed to the system opener as a single argument, so it
/// is never interpreted by a shell, and without its tracking parameters.
#[tauri::command]
async fn open_external(url: String) -> Result<(), OrbError> {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
        return Err(OrbError::ConfigError {
            message: format!("refusing to open a {scheme:?} link from a message"),
        });
    }

    std::process::Command::new("open")
        .arg(orb_core::safety::without_tracking_params(&url))
        .spawn()
        .map_err(|e| OrbError::ConfigError {
            message: format!("could not open link: {e}"),
        })?;
    Ok(())
}

/// The host a link the reader found suspicious is sent to in place of its
/// own address, with the address, the link's text and the reason in the
/// query. Reserved, so a guard that somehow let it through would open
/// nothing.
const LINK_CHECK_HOST: &str = "link-check.orb.invalid";

/// The event a suspicious link is handed back to its window with, to ask
/// before opening it.
const LINK_CHECK_EVENT: &str = "link-check";

/// The navigation guard of the window `label` (see [`stay_in_app`]).
fn navigation_guard(app: &AppHandle, label: &str) -> impl Fn(&tauri::Url) -> bool + Send + 'static {
    let app = app.clone();
    let label = label.to_string();
    move |url| stay_in_app(&app, &label, url)
}

/// The navigation guard of every window: a link leaving the app opens in the
/// browser, or the mail app for `mailto:`, and the window stays where it is.
///
/// A link in a message is how this is reached: the body is in a sandboxed
/// iframe, which cannot run scripts and so cannot report a click any other way
/// than by navigating the window. Catching it here is what turns that into
/// opening a browser — or, for a link the reader rewrote to
/// [`LINK_CHECK_HOST`], into asking first. Any other scheme, `file:` or one
/// some other application registered, goes nowhere.
fn stay_in_app(app: &AppHandle, label: &str, url: &tauri::Url) -> bool {
    let own = app
        .get_webview_window(label)
        .and_then(|window| window.url().ok());
    match navigation(url, own.as_ref()) {
        Navigation::Stay => true,
        Navigation::Ask => {
            let _ = app.emit_to(label, LINK_CHECK_EVENT, url.to_string());
            false
        }
        Navigation::Open => {
            let target = orb_core::safety::without_tracking_params(url.as_str());
            // Off the navigation decision, which has to answer now.
            std::thread::spawn(move || {
                let _ = std::process::Command::new("open").arg(&target).spawn();
            });
            false
        }
        Navigation::Refuse => false,
    }
}

/// What a window does with a navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Navigation {
    /// The window's own page, or a frame's: let it happen.
    Stay,
    /// A link the reader found suspicious: hand it back and ask.
    Ask,
    /// A link out: open it in the browser or the mail app, stay put.
    Open,
    /// Nothing a message has any business asking for.
    Refuse,
}

/// Where a navigation of a window whose page is `own` goes.
///
/// The window's page is the only page it ever shows: a navigation to the same
/// address stays (a reload, a hash), and so does a frame's `about:srcdoc` or
/// `about:blank`. Anything else at the app's own origin — another path, which
/// is where a relative link in a message would go — is refused rather than
/// loaded, as is `localhost` on any other port, which would show whatever
/// service listens there inside the window's chrome. A link to a site opens
/// outside.
fn navigation(url: &tauri::Url, own: Option<&tauri::Url>) -> Navigation {
    if url.scheme() == "about" && matches!(url.path(), "srcdoc" | "blank") {
        return Navigation::Stay;
    }
    if own.is_some_and(|own| {
        own.scheme() == url.scheme()
            && own.host_str() == url.host_str()
            && own.port_or_known_default() == url.port_or_known_default()
            && own.path() == url.path()
    }) {
        return Navigation::Stay;
    }
    match url.scheme() {
        "http" | "https" if url.host_str() == Some(LINK_CHECK_HOST) => Navigation::Ask,
        "http" | "https" if url.host_str().is_some_and(is_this_machine) => Navigation::Refuse,
        "http" | "https" | "mailto" => Navigation::Open,
        _ => Navigation::Refuse,
    }
}

/// Whether a host names this machine, where no message has anything to show.
fn is_this_machine(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("tauri.localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Build the main window.
///
/// Built here rather than declared in the configuration so that its
/// navigation can be watched (see `stay_in_app`).
///
/// Closing the window hides it rather than destroying it, the way a Mac app
/// stays in the Dock with no windows: the syncs go on, the badge keeps
/// counting, and a click on the Dock icon brings the same window back with
/// the folder and message it was on.
fn build_main_window(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    let window = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
        .title("Orb")
        .inner_size(1500.0, 950.0)
        .center()
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .tabbing_identifier("orb")
        // The sidebar is drawn translucent over the system's sidebar
        // material, as Mail and Finder draw theirs; every other pane paints
        // over it. The page is told so it can leave its own background out.
        .transparent(true)
        .effects(
            tauri::window::EffectsBuilder::new()
                .effect(tauri::window::Effect::Sidebar)
                .state(tauri::window::EffectState::FollowsWindowActiveState)
                .build(),
        )
        .initialization_script("document.documentElement.dataset.vibrant = '';")
        .on_navigation(navigation_guard(app, "main"))
        .build()?;
    #[cfg(target_os = "macos")]
    give_toolbar(app, &window);
    let hiding = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = hiding.hide();
        }
    });
    Ok(window)
}

/// Bring the main window back: shown and in front if it exists, built anew
/// if it does not.
fn show_main_window(app: &AppHandle) {
    match app.get_webview_window("main") {
        Some(window) => {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
        None => {
            if let Err(error) = build_main_window(app) {
                orb_core::logger::warn(&format!("could not reopen the main window: {error}"));
            }
        }
    }
}

/// How the launch went: the core is up, or the user has to be told why not.
enum Launch {
    Ready(Handle),
    /// No config file: a template was written to the path and opened.
    FirstRun {
        path: String,
    },
    /// The config could not be read, or the core could not start on it.
    Failed {
        message: String,
    },
}

/// Start the core, answering a missing config with a template.
///
/// A launch from the Dock has no terminal to print to: whatever goes wrong
/// here is shown in a dialog once the app is up, and the process exits when
/// it is dismissed.
fn launch(path: &str) -> Launch {
    match orb_core::ffi::init(path.to_string()) {
        Ok(handle) => Launch::Ready(handle),
        Err(OrbError::ConfigMissing { path }) => match orb_core::config::write_template(&path) {
            Ok(written) => {
                let _ = std::process::Command::new("open")
                    .arg("-t")
                    .arg(&written)
                    .spawn();
                Launch::FirstRun {
                    path: written.display().to_string(),
                }
            }
            Err(error) => Launch::Failed {
                message: format!(
                    "Orb has no configuration, and could not write one to {path}: {error}"
                ),
            },
        },
        Err(error) => Launch::Failed {
            message: format!("Orb could not start from {path}.\n\n{error}"),
        },
    }
}

fn main() {
    orb_core::logger::install_panic_hook();
    // A launch from the Finder has launchd's PATH, which has none of the
    // directories the password command, nvim, pass, op or gpg live in. Done
    // before anything else runs, so no thread is reading the environment
    // while it changes.
    if let Some(login) = orb_core::environment::login_shell_path() {
        let current = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", orb_core::environment::merged_path(&login, &current));
    }
    // Started from a terminal, Orb inherits the variables that tell a program
    // which tmux or hyterm pane it runs in. Orb runs in none, and a child that
    // believed them — `navigate`, from the escalate command or from Neovim's
    // own mappings — would move between that terminal's panes, not windows.
    for pane in ["TMUX", "TMUX_PANE", "HYTERM_SOCK"] {
        std::env::remove_var(pane);
    }

    let launch = launch(&orb_core::config::path());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_drag::init())
        // Size and position of the main, tasks and chat windows survive a
        // relaunch. Not a compose window: it is sized for its draft, and its
        // label is new every time.
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_filter(|label| label == "main" || label == "tasks" || label == "chat")
                .build(),
        )
        .menu(build_menu)
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            set_dock_icon();

            let handle = match launch {
                Launch::Ready(handle) => handle,
                Launch::FirstRun { path } => {
                    app.dialog()
                        .message(format!(
                            "Orb has no configuration yet.\n\nA template was written to {path} and opened in your editor. Fill in your account and launch Orb again."
                        ))
                        .title("Welcome to Orb")
                        .kind(MessageDialogKind::Info)
                        .show(|_| std::process::exit(0));
                    return Ok(());
                }
                Launch::Failed { message } => {
                    app.dialog()
                        .message(message)
                        .title("Orb could not start")
                        .kind(MessageDialogKind::Error)
                        .show(|_| std::process::exit(1));
                    return Ok(());
                }
            };

            build_main_window(app.handle())?;

            // Asked for now rather than on the first key that escalates, so the
            // grant is in place by the time one does.
            if handle.escalates() {
                accessibility::request();
            }

            // A password no command and no Keychain item provides is asked for
            // in the window; registered before the first sync can need one.
            app.manage(Arc::new(PasswordPrompts::default()));
            handle.set_password_prompter(Box::new(WindowPrompter {
                app: app.handle().clone(),
            }));

            // Ask the servers to speak first. Failing to start push is not
            // fatal: syncing still happens on launch and on demand, so the
            // only thing lost is promptness.
            if let Err(error) = handle.start_push(Box::new(PushBridge {
                app: app.handle().clone(),
            })) {
                eprintln!("orb: push notifications unavailable: {error:?}");
            }
            handle.set_outbox_listener(Arc::new(OutboxBridge {
                app: app.handle().clone(),
            }));
            handle.start_task_uploads(Box::new(TaskSyncBridge {
                app: app.handle().clone(),
            }));
            handle.start_config_watch(Box::new(ConfigBridge {
                app: app.handle().clone(),
            }));
            handle.start_chat(Box::new(chat::ChatBridge {
                app: app.handle().clone(),
            }));
            // What waits at launch has been seen; only what starts waiting
            // after this is announced.
            app.manage(chat::ChatWatermark(AtomicI64::new(
                handle.chat_waiting_watermark(),
            )));
            // Read the system address book now, in the background, so that a
            // keystroke never has to. Asking Contacts.framework for permission
            // is a dialogue, and a dialogue has no business between two
            // letters of a name.
            contacts::warm_up();

            // Learn who the mail knows, if new mail has arrived since last
            // time. Reading every message takes seconds on a large mailbox, so
            // it happens on its own thread: nothing on screen waits for it, and
            // completion works from whatever the book already holds.
            let learning = handle.clone();
            std::thread::spawn(move || match learning.refresh_address_book() {
                Ok(Some(total)) => eprintln!("orb: address book holds {total} correspondents"),
                Ok(None) => {}
                Err(error) => eprintln!("orb: could not build the address book: {error:?}"),
            });

            // The handle has to be reachable before notifications are started:
            // a click arriving from a notification looks the state up rather
            // than carrying its own copy, so that it can be delivered to
            // whatever the app is by then.
            app.manage(handle.clone());
            update_badge(app.handle(), &handle);

            // Reminders (§5.1). This has to happen here, on the main thread and
            // before the run loop starts, because the notification delegate
            // must be installed before the app finishes launching — a click
            // that woke Orb from not running is otherwise delivered to nobody.
            // Everything slow it needs to do it moves to a thread of its own.
            notifications::install(app.handle(), &handle);
            start_task_export(app.handle().clone(), handle.clone());
            start_feed_polls(app.handle().clone(), handle.clone());

            app.manage(ComposeHandover::default());
            Ok(())
        })
        // Menu selections are delivered app-wide, so forward the chosen command
        // to the frontend rather than acting on it here.
        // A compose window that goes away without closing its editor — a
        // reload, its web content dying — must not leave Neovim running or
        // its draft held in the handover until the app quits.
        .on_window_event(|window, event| {
            if !matches!(event, tauri::WindowEvent::Destroyed) {
                return;
            }
            let label = window.label().to_string();
            if !label.starts_with("compose-") {
                return;
            }
            if let Some(handover) = window.try_state::<ComposeHandover>() {
                handover.inner().0.lock().unwrap().remove(&label);
            }
            if let Some(orb) = window.try_state::<Handle>() {
                let orb = orb.inner().clone();
                tauri::async_runtime::spawn_blocking(move || orb.editor_close_owned_by(label));
            }
        })
        .on_menu_event(|app, event| {
            let command = event.id().0.clone();
            // Tab selection is the window's business, not the mailbox's, so it
            // is handled here rather than sent to a webview that may not be
            // the one being switched away from.
            if let Some(number) = command.strip_prefix("select-tab-") {
                switch_tab(app, number.to_string());
                return;
            }
            // A command goes to the window in front: the tasks window runs
            // its own, and a mailbox command chosen while a draft is in front
            // would act on the mail selected in the window behind it. What
            // makes sense whichever window is in front goes to the mailbox.
            let front = app
                .webview_windows()
                .into_iter()
                .find(|(_, window)| window.is_focused().unwrap_or(false))
                .map(|(label, _)| label);
            let chat_command = command == "open-chat" || command.starts_with("chat-");
            let target = match front.as_deref() {
                // The chat window answers chat's own commands itself.
                Some("chat") if chat_command => "chat",
                _ if GLOBAL_MENU_COMMANDS.contains(&command.as_str()) => "main",
                None | Some("main") => "main",
                Some("tasks") => "tasks",
                Some("chat") => "chat",
                // A message or task window has no sidebar to go anywhere in.
                Some(_) if command == "go-to" => return,
                Some(item) if item.starts_with("message-") || item.starts_with("task-") => item,
                Some(_) => return,
            };
            if !command.is_empty() {
                let _ = app.emit_to(target, "context-menu-command", command);
            }
        })
        .invoke_handler(tauri::generate_handler![
            escalate,
            accounts,
            title_bar,
            sync_stats,
            config_warnings,
            notification_permission,
            open_notification_settings,
            pending_password_prompts,
            answer_password,
            ui_settings,
            raw_message,
            save_draft,
            discard_draft,
            outbox,
            retry_outbox,
            discard_outbox,
            edit_outbox,
            open_compose,
            take_compose_draft,
            detach_attachments,
            reply_draft,
            forward_draft,
            new_draft,
            edit_draft,
            identities,
            senders,
            switch_identity,
            draft_warnings,
            templates,
            use_template,
            template_suggestions,
            suggest_recipients,
            select_tab,
            editor_open,
            editor_input,
            editor_paste,
            editor_mouse,
            editor_resize,
            editor_focus,
            editor_buffer,
            editor_set_buffer,
            editor_insert_template,
            editor_fill_placeholder,
            editor_goto_placeholder,
            editor_edit_file,
            editor_close,
            folders,
            messages,
            inbox_messages,
            inbox_message_row,
            thread_row,
            filter_messages,
            inbox_thread_total,
            inbox_ranks,
            folder_total,
            thread_total,
            thread_messages,
            thread_members,
            message,
            message_complete,
            inline_images,
            search,
            mark_read,
            mark_flagged,
            poll_feeds,
            feed_statuses,
            stage_action,
            unstage_action,
            folder_uids,
            execute_pending,
            execute_pending_all,
            virtual_folders,
            pins,
            recent_jobs,
            log_recent,
            log_frontend,
            open_console,
            open_tasks_window,
            open_chat_window,
            chat::chat_server,
            chat::chat_status,
            chat::chat_waiting,
            chat::chat_rooms,
            chat::chat_conversation,
            chat::chat_older,
            chat::chat_room_threads,
            chat::chat_find,
            chat::chat_open_direct,
            chat::chat_join,
            chat::chat_leave,
            chat::chat_reactions,
            chat::chat_threads,
            chat::chat_links,
            chat::chat_send_files,
            chat::chat_edit,
            chat::chat_delete,
            chat::chat_react,
            chat::chat_set_read,
            chat::chat_open_group,
            chat::chat_create_task,
            chat::chat_open_file,
            chat::chat_file_preview,
            chat::chat_create_room,
            open_chat_menu,
            open_chat_room_menu,
            chat::chat_mark_seen,
            chat::chat_done,
            chat::chat_mute,
            chat::chat_unmute,
            chat::chat_send,
            chat::chat_sync,
            chat::chat_sign_out,
            chat::chat_sign_in_window,
            chat::chat_open_in_browser,
            open_item_window,
            edit_task_step,
            working_on_tasks,
            export_tasks,
            iphone_setup_code,
            task_export_status,
            import_tasks,
            import_things,
            clear_working_on,
            show_mailbox,
            copy_messages,
            transfer_messages,
            create_folder,
            rename_folder,
            delete_folder,
            set_folder_subscribed,
            run_filters,
            run_filters_all,
            filter_activity,
            highlight_styles,
            highlight_counts,
            highlight_badges,
            pending_actions,
            pending_counts,
            discard_pending,
            delete_messages,
            trash_folder,
            move_messages,
            account_statuses,
            highlight_terms,
            attachments,
            attachment_path,
            open_attachment,
            sender_check,
            quick_look_attachment,
            save_attachment,
            drag_icon,
            sync_folder,
            sync_all,
            fetch_bodies,
            body_backfill_progress,
            bindings,
            send_mail,
            quit_app,
            open_external,
            message_links,
            task_links,
            message_row,
            open_message_menu,
            open_task_menu,
            open_step_menu,
            suggest_recurrence,
            list_tasks,
            clear_completed_from_today,
            plan_task_merge,
            merge_tasks,
            search_tasks,
            count_tasks,
            today_task_count,
            next_task_midnight,
            suggest_dates,
            suggest_datetimes,
            get_task,
            task_log,
            create_task,
            update_task,
            update_tasks,
            complete_tasks,
            uncomplete_tasks,
            trash_tasks,
            restore_tasks,
            empty_task_trash,
            set_task_postpone_date,
            set_task_due_date,
            clear_task_dates,
            move_task,
            move_tasks,
            undo,
            redo,
            begin_undo_group,
            end_undo_group,
            list_task_tags,
            create_task_tag,
            update_task_tag,
            delete_task_tag,
            add_task_tag,
            remove_task_tag,
            set_task_tags,
            list_task_reminders,
            add_task_reminder,
            delete_task_reminder,
            snooze_task_reminder,
            task_notification_plan,
            record_task_notification_delivered,
            catch_up_task_reminders,
            tasks_configured,
            discard_task,
            open_task_attachment,
            list_task_attachments,
            add_task_attachment,
            delete_task_attachment,
            task_attachment_data,
            sync_tasks,
            task_sync_status,
            create_tasks_from_messages,
            link_message_to_task,
            unlink_message_from_task,
            link_mail_in_notes,
            linked_message,
            resolve_linked_message,
            open_task_counts,
            tasks_for_message
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            // ⌘Q from the Dock or the system, rather than from the menu: the
            // mailbox window asks about anything at stake, and quits through
            // `quit_app`, which lets this one through.
            tauri::RunEvent::ExitRequested { code: None, api, .. }
                if !QUITTING.load(std::sync::atomic::Ordering::SeqCst)
                    && app.get_webview_window("main").is_some() =>
            {
                api.prevent_exit();
                let _ = app.emit_to("main", "context-menu-command", "quit");
            }
            // The process is about to go: let go of the server and the folder
            // locks first, so another instance does not wait six minutes for
            // what this one was syncing.
            tauri::RunEvent::Exit => {
                if let Some(handle) = app.try_state::<Handle>() {
                    handle.editor_close_all();
                    handle.shutdown();
                }
            }
            // The Dock icon was clicked with no window showing: the main
            // window was closed, which only hid it.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen {
                has_visible_windows: false,
                ..
            } if app.try_state::<Handle>().is_some() => show_main_window(app),
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What macOS would run is revealed rather than opened: programs, scripts,
    /// installers, archives of them, and the link files that open whatever
    /// address they hold. Trailing dots and spaces, which Finder hides, do not
    /// turn one into something else.
    #[test]
    fn what_the_system_would_run_is_revealed_not_opened() {
        for name in [
            "setup.app",
            "run.command",
            "run.tool",
            "Rechnung.inetloc",
            "page.webloc",
            "link.url",
            "elsewhere.fileloc",
            "thing.jar",
            "profile.mobileconfig",
            "RUN.SH",
            "run.sh.",
            "run.command ",
            "bundle.mpkg",
            "archive.xip",
        ] {
            assert!(runs_rather_than_displays(name), "{name:?}");
        }
        for name in [
            "report.pdf",
            "notes.txt",
            "photo.jpg",
            "invoice.html",
            "disk.dmg",
            "",
        ] {
            assert!(!runs_rather_than_displays(name), "{name:?}");
        }
    }

    /// A window shows its own page and nothing else: the same address stays,
    /// a frame's `about:` documents stay, a site opens outside, and anything
    /// at the app's origin, on this machine or in another scheme is refused.
    #[test]
    fn a_window_navigates_only_to_its_own_page_and_sends_the_rest_out() {
        let url = |s: &str| tauri::Url::parse(s).unwrap();
        let own = url("tauri://localhost/index.html");
        let decide = |s: &str| navigation(&url(s), Some(&own));
        assert_eq!(decide("tauri://localhost/index.html"), Navigation::Stay);
        assert_eq!(decide("tauri://localhost/index.html#top"), Navigation::Stay);
        assert_eq!(decide("about:srcdoc"), Navigation::Stay);
        assert_eq!(decide("about:srcdoc#section"), Navigation::Stay);
        assert_eq!(decide("about:blank"), Navigation::Stay);
        assert_eq!(decide("https://example.com/news"), Navigation::Open);
        assert_eq!(decide("mailto:a@example.com"), Navigation::Open);
        assert_eq!(
            decide("https://link-check.orb.invalid/?url=x"),
            Navigation::Ask
        );
        // A relative link in a message, resolved against the app's own page.
        assert_eq!(
            decide("tauri://localhost/unsubscribe.html"),
            Navigation::Refuse
        );
        // A protocol-relative link, resolved against the app's own scheme.
        assert_eq!(decide("tauri://evil.example/x"), Navigation::Refuse);
        assert_eq!(decide("http://localhost:8888/tree"), Navigation::Refuse);
        assert_eq!(decide("http://127.0.0.1:9/"), Navigation::Refuse);
        assert_eq!(decide("http://[::1]:9/"), Navigation::Refuse);
        assert_eq!(decide("file:///etc/passwd"), Navigation::Refuse);
        assert_eq!(decide("javascript:alert(1)"), Navigation::Refuse);
        assert_eq!(decide("vnc://attacker.example/"), Navigation::Refuse);

        // In development the page is served by Vite; the same rule holds.
        let dev = url("http://localhost:1420/index.html");
        let decide = |s: &str| navigation(&url(s), Some(&dev));
        assert_eq!(decide("http://localhost:1420/index.html"), Navigation::Stay);
        assert_eq!(
            decide("http://localhost:1420/other.html"),
            Navigation::Refuse
        );
        assert_eq!(decide("http://localhost:8888/"), Navigation::Refuse);
        assert_eq!(decide("https://example.com/"), Navigation::Open);

        // Without a page to compare with, nothing at the app's origin stays.
        assert_eq!(
            navigation(&url("tauri://localhost/index.html"), None),
            Navigation::Refuse
        );
    }

    /// An editor answers to the window that opened it and to no other; an id
    /// no editor has belongs to nobody.
    #[test]
    fn an_editor_belongs_to_its_window_alone() {
        assert!(editor_belongs(Some("compose-3"), "compose-3"));
        assert!(!editor_belongs(Some("compose-3"), "main"));
        assert!(!editor_belongs(Some("compose-3"), "compose-4"));
        assert!(!editor_belongs(None, "main"));
    }

    /// The exact strings `sortOrder()` in `src/main.ts` puts on the wire.
    ///
    /// The frontend and the FFI enum are only coupled by these names, so a
    /// rename on either side would otherwise fail at runtime as a rejected
    /// command rather than at build time.
    const FRONTEND_SORT_NAMES: [&str; 6] = [
        "DateNewest",
        "DateOldest",
        "SubjectAsc",
        "SubjectDesc",
        "SenderAsc",
        "SenderDesc",
    ];

    #[test]
    fn frontend_sort_names_deserialize_to_the_ffi_enum() {
        for name in FRONTEND_SORT_NAMES {
            let json = format!("\"{name}\"");
            let parsed: MessageSortOrder = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("frontend sends {name}, which does not parse: {e}"));
            // Round-trip so a future rename cannot silently alias two variants.
            assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
        }
    }

    /// The view names the task sidebar puts on the wire.
    ///
    /// Same contract as the sort order above: the frontend and the FFI enum are
    /// coupled by nothing but these strings, so a rename on either side would
    /// surface as a rejected command at runtime rather than a build failure.
    const FRONTEND_TASK_VIEWS: [&str; 5] = ["today", "upcoming", "someday", "completed", "trash"];

    #[test]
    fn frontend_task_view_names_deserialize_to_the_ffi_enum() {
        for name in FRONTEND_TASK_VIEWS {
            let json = format!("\"{name}\"");
            let parsed: TaskViewName = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("frontend sends {name}, which does not parse: {e}"));
            assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
        }
    }

    /// A `TaskEdit` with nothing in it must mean "change nothing".
    ///
    /// The frontend sends only the fields it touched, so every other field
    /// arrives missing. If a missing `clear_due_date` deserialised as anything
    /// but `false`, renaming a task would quietly unset its dates — which is
    /// the failure mode `TaskEdit`'s flat shape exists to prevent.
    #[test]
    fn an_empty_edit_clears_nothing() {
        let edit: TaskEdit = serde_json::from_str("{}").expect("an empty edit is a valid edit");
        assert!(edit.title.is_none());
        assert!(!edit.clear_body);
        assert!(!edit.clear_postpone_date);
        assert!(!edit.clear_due_date);
        assert!(!edit.clear_recurrence);

        // And a partial one leaves the untouched fields alone.
        let renamed: TaskEdit = serde_json::from_str(r#"{"title":"Buy milk"}"#).unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Buy milk"));
        assert!(!renamed.clear_due_date);
    }

    #[test]
    fn omitted_sort_is_none() {
        // `messages` takes Option<MessageSortOrder>; leaving it out must mean
        // "use the folder's configured order", not an error.
        let absent: Option<MessageSortOrder> = serde_json::from_str("null").unwrap();
        assert!(absent.is_none());
    }
}
