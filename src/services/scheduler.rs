//! Keeps the mailbox in step with the server without being asked.
//!
//! Three things make a folder worth syncing, and all three arrive here:
//!
//! - the server says so, through an IDLE notification (`IdleService`);
//! - time has passed: every account is synced whole at its configured
//!   interval, which is also how accounts whose server has no IDLE get their
//!   mail;
//! - the machine has woken from sleep, which every connection it held did
//!   not survive. The pool is emptied, the IDLE connections are reopened and
//!   every account is synced. The same happens when the machine moves to
//!   another network (`services::network`).
//!
//! Requests are coalesced per folder: a burst of deliveries is one sync and
//! one more, not one per delivery, and a request for a folder that is being
//! synced runs again when that sync ends rather than being dropped. A sync
//! that found its folder locked by another sync asks again shortly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::commands::StateUpdate;
use crate::config::{AccountConfig, Config};
use crate::constants::{
    DEFAULT_SYNC_INTERVAL_SECONDS, SYNC_SKIPPED_RETRY_ATTEMPTS, SYNC_SKIPPED_RETRY_DELAY_SECONDS,
    WAKE_DETECT_TICK_SECONDS, WAKE_GAP_SECONDS,
};
use crate::error::FailureKind;
use crate::services::IdleService;
use crate::store::{Store, SyncReport};

/// What a sync the scheduler ran came to.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// A sync began. `folder` is `None` for a sync of the whole account.
    Started {
        account: String,
        folder: Option<String>,
    },
    /// A sync ended, with its report. `folder` is `None` for a sync of the
    /// whole account.
    Synced {
        account: String,
        folder: Option<String>,
        report: SyncReport,
    },
    /// A sync failed before it could report.
    Failed {
        account: String,
        folder: Option<String>,
        kind: FailureKind,
        error: String,
    },
}

/// Who hears about the syncs the scheduler runs.
pub type SyncEventListener = Arc<dyn Fn(SyncEvent) + Send + Sync>;

/// One folder, or one whole account.
type Key = (String, Option<String>);

/// A request to sync something, and why.
#[derive(Debug, Clone)]
struct Request {
    key: Key,
    reason: &'static str,
    /// How many times this request has been re-issued because the folder was
    /// locked when it ran.
    skipped_before: u32,
}

/// The end of a sync the loop spawned.
struct Done {
    key: Key,
    skipped_before: u32,
    result: Result<SyncReport, (FailureKind, String)>,
}

/// Hands the scheduler's loop its requests; cheap to clone.
#[derive(Clone)]
pub struct SyncScheduler {
    requests: mpsc::UnboundedSender<Request>,
    shutdown: CancellationToken,
}

impl SyncScheduler {
    /// Start the scheduler on the current runtime.
    ///
    /// It runs until `shutdown` is cancelled. `listener` is told about every
    /// sync it runs.
    pub fn start(
        store: Arc<Store>,
        config: Arc<Config>,
        listener: SyncEventListener,
        shutdown: CancellationToken,
    ) -> Self {
        let (requests, inbox) = mpsc::unbounded_channel();
        let scheduler = Self {
            requests: requests.clone(),
            shutdown: shutdown.clone(),
        };
        tokio::spawn(run(store, config, listener, shutdown, requests, inbox));
        scheduler
    }

    /// Ask for a sync of one folder, or of a whole account when `folder` is
    /// `None`. Coalesced with whatever is already running or queued.
    pub fn request(&self, account: &str, folder: Option<&str>) {
        let _ = self.requests.send(Request {
            key: (account.to_string(), folder.map(str::to_string)),
            reason: "requested",
            skipped_before: 0,
        });
    }

    /// Stop the loop and every IDLE connection it holds.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

/// Which folders of an account to watch with IDLE: INBOX, then the ones the
/// configuration marks, in alphabetical order, as many as the server allows
/// connections for. The IDLE service evicts beyond that cap, so handing it
/// more would only open connections to close them again.
pub fn watched_folders(account: &AccountConfig) -> Vec<String> {
    let mut folders = vec!["INBOX".to_string()];
    let mut marked: Vec<&String> = account
        .folders
        .iter()
        .filter(|(name, folder)| folder.watch && folder.sync && name.as_str() != "INBOX")
        .map(|(name, _)| name)
        .collect();
    marked.sort();
    let room = super::idle::MAX_IDLE_PER_ACCOUNT - folders.len();
    if marked.len() > room {
        crate::logger::warn(&format!(
            "{}: only {room} folders besides INBOX can be watched; not watching {}",
            account.name,
            marked[room..]
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        marked.truncate(room);
    }
    folders.extend(marked.into_iter().cloned());
    folders
}

/// How often an account is synced whole.
fn sync_interval(account: &AccountConfig) -> Duration {
    Duration::from_secs(
        account
            .sync_interval_seconds
            .unwrap_or(DEFAULT_SYNC_INTERVAL_SECONDS)
            .max(1),
    )
}

/// The syncs running and the ones waiting for them to end.
#[derive(Default)]
struct Coalescer {
    running: HashSet<Key>,
    queued: HashMap<Key, Request>,
}

impl Coalescer {
    /// Whether this request should run now. If the same thing is running
    /// already, it is noted and runs once more when that ends.
    fn admit(&mut self, request: Request) -> Option<Request> {
        if self.running.contains(&request.key) {
            self.queued.entry(request.key.clone()).or_insert(request);
            return None;
        }
        self.running.insert(request.key.clone());
        Some(request)
    }

    /// A sync ended; what, if anything, runs next for the same key.
    fn finished(&mut self, key: &Key) -> Option<Request> {
        self.running.remove(key);
        let next = self.queued.remove(key)?;
        self.running.insert(key.clone());
        Some(next)
    }
}

/// A clock that notices when it was not running.
///
/// A sleeping machine runs no ticks, so a tick that finds far more wall-clock
/// time gone than it slept for is how a wake announces itself. The monotonic
/// clock cannot be used for this: on Darwin it does not advance during sleep,
/// which is exactly what makes the comparison work.
struct WakeDetector {
    last_tick: SystemTime,
}

impl WakeDetector {
    fn new() -> Self {
        Self {
            last_tick: SystemTime::now(),
        }
    }

    /// Record a tick; `Some(gap)` when the machine evidently slept since the
    /// last one.
    fn tick(&mut self) -> Option<Duration> {
        let now = SystemTime::now();
        let gap = now.duration_since(self.last_tick).unwrap_or(Duration::ZERO);
        self.last_tick = now;
        Self::is_wake(gap, Duration::from_secs(WAKE_DETECT_TICK_SECONDS)).then_some(gap)
    }

    /// Whether `gap` between two ticks of `tick` is a sleep rather than a
    /// scheduler running late.
    fn is_wake(gap: Duration, tick: Duration) -> bool {
        gap > tick + Duration::from_secs(WAKE_GAP_SECONDS)
    }
}

async fn run(
    store: Arc<Store>,
    config: Arc<Config>,
    listener: SyncEventListener,
    shutdown: CancellationToken,
    requests: mpsc::UnboundedSender<Request>,
    mut inbox: mpsc::UnboundedReceiver<Request>,
) {
    let (idle_tx, mut idle_rx) = mpsc::unbounded_channel::<StateUpdate>();
    let mut idle =
        IdleService::new(idle_tx, shutdown.clone()).with_credentials(store.credentials());
    let accounts: Vec<Arc<AccountConfig>> = config.accounts.iter().cloned().map(Arc::new).collect();
    for account in &accounts {
        for folder in watched_folders(account) {
            if let Err(error) = idle.ensure_watching(account.clone(), &folder).await {
                crate::logger::warn(&format!(
                    "{}/{folder}: no push ({error}); the periodic sync covers it",
                    account.name
                ));
            }
        }
    }

    // Every account is due one full sync per interval. Not one at launch:
    // the app syncs everything as it opens, and a second sync of the same
    // folders would only find them locked.
    let mut due: Vec<(Arc<AccountConfig>, Instant)> = accounts
        .iter()
        .map(|account| (account.clone(), Instant::now() + sync_interval(account)))
        .collect();

    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Done>();
    let mut coalescer = Coalescer::default();
    let mut wake = WakeDetector::new();
    // An interval rather than a sleep made anew each turn of the loop: a
    // sleep restarted whenever another branch won would never fire on a
    // busy day, and the busy day is when the machine was closed and opened.
    let mut ticks = tokio::time::interval(Duration::from_secs(WAKE_DETECT_TICK_SECONDS));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (network_tx, mut network_rx) = mpsc::unbounded_channel::<()>();
    tokio::spawn(crate::services::network::watch(
        network_tx,
        shutdown.clone(),
    ));

    loop {
        let next_due = due.iter().map(|(_, at)| *at).min();
        tokio::select! {
            _ = shutdown.cancelled() => {
                idle.shutdown().await;
                return;
            }
            Some(request) = inbox.recv() => {
                if let Some(request) = coalescer.admit(request) {
                    spawn_sync(&store, &listener, request, done_tx.clone());
                }
            }
            Some(update) = idle_rx.recv() => {
                if let StateUpdate::ExternalChange { account, folder } = update {
                    let _ = requests.send(Request {
                        key: (account, Some(folder)),
                        reason: "push",
                        skipped_before: 0,
                    });
                }
            }
            Some(done) = done_rx.recv() => {
                let (account, folder) = done.key.clone();
                match &done.result {
                    Ok(report) => {
                        // A folder another sync held is a folder this sync did
                        // not do; ask again shortly, a bounded number of times.
                        if report.folders_skipped > 0
                            && done.skipped_before < SYNC_SKIPPED_RETRY_ATTEMPTS
                        {
                            let again = Request {
                                key: done.key.clone(),
                                reason: "skipped, locked",
                                skipped_before: done.skipped_before + 1,
                            };
                            let requests = requests.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_secs(
                                    SYNC_SKIPPED_RETRY_DELAY_SECONDS,
                                ))
                                .await;
                                let _ = requests.send(again);
                            });
                        }
                        listener(SyncEvent::Synced {
                            account,
                            folder,
                            report: report.clone(),
                        });
                    }
                    Err((kind, error)) => listener(SyncEvent::Failed {
                        account,
                        folder,
                        kind: *kind,
                        error: error.clone(),
                    }),
                }
                if let Some(next) = coalescer.finished(&done.key) {
                    spawn_sync(&store, &listener, next, done_tx.clone());
                }
            }
            _ = sleep_until(next_due) => {
                let now = Instant::now();
                for (account, at) in due.iter_mut() {
                    if *at <= now {
                        *at = now + sync_interval(account);
                        let _ = requests.send(Request {
                            key: (account.name.clone(), None),
                            reason: "periodic",
                            skipped_before: 0,
                        });
                    }
                }
            }
            _ = ticks.tick() => {
                if let Some(gap) = wake.tick() {
                    crate::logger::info(&format!(
                        "scheduler: the machine slept for about {gap:?}; reconnecting and syncing"
                    ));
                    reconnect(&store, &mut idle, &mut due).await;
                }
            }
            Some(()) = network_rx.recv() => {
                reconnect(&store, &mut idle, &mut due).await;
            }
        }
    }
}

/// Start over after a sleep or a move to another network: every connection
/// from before is dead, whatever the sockets say, so the pool is emptied, the
/// IDLE connections are reopened and every account is due a sync now.
async fn reconnect(
    store: &Store,
    idle: &mut IdleService,
    due: &mut [(Arc<AccountConfig>, Instant)],
) {
    store.connection_pool().close_all().await;
    idle.restart_all().await;
    let now = Instant::now();
    for (_, at) in due.iter_mut() {
        *at = now;
    }
}

/// Sleep until `at`, or forever when there is nothing scheduled.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

fn spawn_sync(
    store: &Arc<Store>,
    listener: &SyncEventListener,
    request: Request,
    done: mpsc::UnboundedSender<Done>,
) {
    let store = store.clone();
    let (account, folder) = request.key.clone();
    listener(SyncEvent::Started {
        account: account.clone(),
        folder: folder.clone(),
    });
    tokio::spawn(async move {
        crate::logger::debug(&format!(
            "scheduler: syncing {account}/{} ({})",
            folder.as_deref().unwrap_or("*"),
            request.reason
        ));
        let result = store
            .sync(Some(account), folder, None, false)
            .await
            .map_err(|e| (e.kind(), e.to_string()));
        let _ = done.send(Done {
            key: request.key,
            skipped_before: request.skipped_before,
            result,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(folder: Option<&str>) -> Key {
        ("Test".to_string(), folder.map(str::to_string))
    }

    fn request(folder: Option<&str>) -> Request {
        Request {
            key: key(folder),
            reason: "test",
            skipped_before: 0,
        }
    }

    /// A burst of notifications for one folder is one sync now and one more
    /// afterwards, never one per notification and never none.
    #[test]
    fn requests_for_a_running_sync_collapse_into_one_more_run() {
        let mut coalescer = Coalescer::default();
        assert!(coalescer.admit(request(Some("INBOX"))).is_some());
        for _ in 0..5 {
            assert!(coalescer.admit(request(Some("INBOX"))).is_none());
        }
        let again = coalescer.finished(&key(Some("INBOX")));
        assert!(
            again.is_some(),
            "a request made during the sync runs after it"
        );
        assert!(
            coalescer.finished(&key(Some("INBOX"))).is_none(),
            "and only once"
        );
    }

    /// A folder and the whole account are different things to sync; neither
    /// waits for the other here (the folder lock sorts out their overlap).
    #[test]
    fn different_keys_run_side_by_side() {
        let mut coalescer = Coalescer::default();
        assert!(coalescer.admit(request(Some("INBOX"))).is_some());
        assert!(coalescer.admit(request(None)).is_some());
        assert!(coalescer.admit(request(Some("Sent"))).is_some());
    }

    /// A tick that comes late by a few seconds is a busy scheduler; one that
    /// comes late by minutes is a machine that was asleep.
    #[test]
    fn a_wake_is_a_gap_well_beyond_the_tick() {
        let tick = Duration::from_secs(WAKE_DETECT_TICK_SECONDS);
        assert!(!WakeDetector::is_wake(tick, tick));
        assert!(!WakeDetector::is_wake(tick + Duration::from_secs(30), tick));
        assert!(WakeDetector::is_wake(
            tick + Duration::from_secs(WAKE_GAP_SECONDS + 1),
            tick
        ));
        assert!(WakeDetector::is_wake(Duration::from_secs(8 * 3600), tick));
    }

    #[test]
    fn inbox_is_always_watched_and_marked_folders_follow_in_order() {
        let account: AccountConfig = serde_saphyr::from_str(
            "name: Test\nimap_server: imap.example.com\nimap_port: 993\nimap_username: u\n\
             imap_password_command: echo x\nfrom: t@example.com\n\
             folders:\n  Work: { watch: true }\n  Archive: { watch: true }\n  \
             Lists: { watch: true, sync: false }\n  INBOX: { watch: true }\n",
        )
        .expect("an account");
        assert_eq!(watched_folders(&account), vec!["INBOX", "Archive", "Work"]);
        assert_eq!(
            sync_interval(&account),
            Duration::from_secs(DEFAULT_SYNC_INTERVAL_SECONDS)
        );
    }

    #[test]
    fn marked_folders_beyond_the_idle_cap_are_left_out() {
        let account: AccountConfig = serde_saphyr::from_str(
            "name: Test\nimap_server: imap.example.com\nimap_port: 993\nimap_username: u\n\
             imap_password_command: echo x\nfrom: t@example.com\n\
             folders:\n  Work: { watch: true }\n  Archive: { watch: true }\n  \
             Lists: { watch: true }\n  Builds: { watch: true }\n",
        )
        .expect("an account");
        let watched = watched_folders(&account);
        assert_eq!(watched.len(), super::super::idle::MAX_IDLE_PER_ACCOUNT);
        assert_eq!(watched, vec!["INBOX", "Archive", "Builds"]);
    }
}
