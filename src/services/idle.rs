//! IMAP IDLE: one connection per watched folder, told by the server when the
//! folder changes.
//!
//! - At most `MAX_IDLE_PER_ACCOUNT` connections per account, INBOX always
//!   among them; the least recently looked-at folder makes room.
//! - IDLE is re-issued every `IDLE_CYCLE` (RFC 2177 advises under 29 min),
//!   which is also how often a frozen connection is found out: the DONE that
//!   ends an IDLE is bounded by the probe timeout, and a connection that does
//!   not answer it is replaced.
//! - Every command on the connection is bounded, so a worker never hangs.
//! - A change is reported on every reconnect, not only on a notification:
//!   mail delivered while the worker was reconnecting is otherwise found at
//!   the next delivery. The first connect at launch is not one — the launch
//!   sync covers it — but a connect after the machine woke is.
//! - Reconnects back off exponentially, and the backoff resets once a
//!   connection has stayed up for a while.
//!
//! Each connection runs in its own task and sends
//! `StateUpdate::ExternalChange` for its folder.

use crate::Result;
use crate::commands::StateUpdate;
use crate::config::AccountConfig;
use crate::credentials::PasswordCache;
use crate::error::Error;
use async_imap::extensions::idle::IdleResponse;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Maximum IDLE connections per account
pub(crate) const MAX_IDLE_PER_ACCOUNT: usize = 3;

/// How long one IDLE runs before it is ended and re-issued.
///
/// RFC 2177 allows 29 minutes; shorter here because ending an IDLE is also the
/// liveness check. A connection whose packets have stopped — a NAT that forgot
/// the flow, a network that changed — answers nothing, including the DONE, and
/// is found out at the end of the cycle rather than half an hour later. TCP
/// keepalive catches most such cases sooner; this catches the rest.
const IDLE_CYCLE: Duration = Duration::from_secs(10 * 60);

/// Reconnect delay after connection failure
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Maximum reconnect delay (exponential backoff cap)
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(300);

/// How long a session must stay connected before it is considered "healthy".
///
/// If a session survives at least this long before failing, the accumulated
/// reconnect backoff is reset to its initial value so that a transient error
/// after a long-lived healthy connection does not keep new-mail detection
/// delayed by a stale multi-minute backoff.
const HEALTHY_SESSION_THRESHOLD: Duration = Duration::from_secs(120);

/// How long any one command on the IDLE connection may take.
const OPERATION_TIMEOUT: Duration =
    Duration::from_secs(crate::constants::IMAP_OPERATION_TIMEOUT_SECONDS);

/// How long the one-round-trip commands around an IDLE — the DONE that ends
/// it, the NOOP between two, the IDLE's own handshake — may take. A server
/// that is there answers within a round trip; one that does not is gone.
const DONE_TIMEOUT: Duration = Duration::from_secs(crate::constants::IMAP_PROBE_TIMEOUT_SECONDS);

/// Tracks the exponential reconnect backoff for an IDLE worker.
///
/// The delay grows on each consecutive failure but is reset to the initial
/// value once a connection has stayed healthy for [`HEALTHY_SESSION_THRESHOLD`].
struct ReconnectBackoff {
    delay: Duration,
}

impl ReconnectBackoff {
    /// Create a backoff starting at the initial reconnect delay.
    fn new() -> Self {
        Self {
            delay: INITIAL_RECONNECT_DELAY,
        }
    }

    /// Record how long the just-ended session lasted.
    ///
    /// If it stayed connected long enough to be considered healthy, reset the
    /// backoff to its initial value.
    fn on_session_ended(&mut self, session_duration: Duration) {
        if session_duration >= HEALTHY_SESSION_THRESHOLD {
            self.delay = INITIAL_RECONNECT_DELAY;
        }
    }

    /// The delay to wait before the next reconnect attempt.
    fn current_delay(&self) -> Duration {
        self.delay
    }

    /// Grow the delay after a failed reconnect (exponential backoff, capped).
    fn grow(&mut self) {
        self.delay = (self.delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

/// Handle to a running IDLE worker
struct IdleWorkerHandle {
    /// Token to stop the worker
    stop_token: CancellationToken,
    /// When this folder was last accessed (for LRU eviction)
    last_access: std::time::Instant,
}

/// Key for identifying an IDLE worker
type WorkerKey = (String, String); // (account, folder)

/// IDLE service managing multiple IDLE connections
pub struct IdleService {
    /// Active IDLE workers by (account, folder)
    workers: HashMap<WorkerKey, IdleWorkerHandle>,
    /// Where changes are reported.
    tx_update: mpsc::UnboundedSender<StateUpdate>,
    /// Shutdown token
    shutdown: CancellationToken,
    credentials: Arc<PasswordCache>,
    /// How long one IDLE runs before it is re-issued; see [`IDLE_CYCLE`].
    cycle: Duration,
    /// The accounts whose folders are watched, for restarting their workers.
    configs: HashMap<String, Arc<AccountConfig>>,
}

impl IdleService {
    /// Create a new IdleService
    pub fn new(tx_update: mpsc::UnboundedSender<StateUpdate>, shutdown: CancellationToken) -> Self {
        Self {
            workers: HashMap::new(),
            tx_update,
            shutdown,
            credentials: PasswordCache::new(),
            cycle: IDLE_CYCLE,
            configs: HashMap::new(),
        }
    }

    /// Share the passwords with whoever else connects, so the password command
    /// runs once per session rather than once per IDLE connection.
    pub fn with_credentials(mut self, credentials: Arc<PasswordCache>) -> Self {
        self.credentials = credentials;
        self
    }

    /// Re-issue IDLE every `cycle` instead of the default.
    ///
    /// For a test that wants a frozen connection found out in seconds rather
    /// than minutes; in use the default is right.
    pub fn with_cycle(mut self, cycle: Duration) -> Self {
        self.cycle = cycle;
        self
    }

    /// Ensure IDLE is running for the specified folder
    ///
    /// If too many connections exist for this account, evicts the least recently
    /// used folder (except INBOX which is always kept).
    pub async fn ensure_watching(
        &mut self,
        account_config: Arc<AccountConfig>,
        folder: &str,
    ) -> Result<()> {
        let account = &account_config.name;
        let key = (account.clone(), folder.to_string());

        // Already watching this folder - update access time
        if let Some(handle) = self.workers.get_mut(&key) {
            handle.last_access = std::time::Instant::now();
            return Ok(());
        }

        // Check connection count for this account
        let account_workers: Vec<_> = self
            .workers
            .iter()
            .filter(|((acc, _), _)| acc == account)
            .map(|(k, v)| (k.clone(), v.last_access))
            .collect();

        // If at limit, evict LRU (but never INBOX)
        if account_workers.len() >= MAX_IDLE_PER_ACCOUNT {
            if let Some((evict_key, _)) = account_workers
                .iter()
                .filter(|((_, f), _)| f != "INBOX")
                .min_by_key(|(_, last_access)| *last_access)
            {
                self.stop_watching(&evict_key.0, &evict_key.1).await;
            } else {
                // All slots are INBOX (shouldn't happen) - can't evict
                crate::logger::warn(&format!(
                    "Cannot start IDLE for {}/{}: all slots in use",
                    account, folder
                ));
                return Ok(());
            }
        }

        // Start new IDLE worker
        self.start_worker(account_config, folder.to_string(), false)
            .await;
        Ok(())
    }

    /// Stop watching a specific folder
    pub async fn stop_watching(&mut self, account: &str, folder: &str) {
        let key = (account.to_string(), folder.to_string());
        if let Some(handle) = self.workers.remove(&key) {
            handle.stop_token.cancel();
            crate::logger::info(&format!("Stopped IDLE for {}/{}", account, folder));
        }
    }

    /// Stop all IDLE workers for an account
    pub async fn stop_account(&mut self, account: &str) {
        let keys_to_remove: Vec<_> = self
            .workers
            .keys()
            .filter(|(acc, _)| acc == account)
            .cloned()
            .collect();

        for key in keys_to_remove {
            if let Some(handle) = self.workers.remove(&key) {
                handle.stop_token.cancel();
            }
        }
    }

    /// Stop every worker and start them again, on new connections.
    ///
    /// For a machine that has woken from sleep: every IDLE connection it held
    /// is dead, and the worker would find that out only at the end of its
    /// cycle. Each restarted worker reports a change as it connects, so the
    /// folders are synced too.
    pub async fn restart_all(&mut self) {
        let keys: Vec<WorkerKey> = self.workers.keys().cloned().collect();
        for key in &keys {
            if let Some(handle) = self.workers.remove(key) {
                handle.stop_token.cancel();
            }
        }
        for (account, folder) in keys {
            if let Some(config) = self.configs.get(&account).cloned() {
                self.start_worker(config, folder, true).await;
            }
        }
    }

    /// Shutdown all IDLE workers
    pub async fn shutdown(&mut self) {
        for (_, handle) in self.workers.drain() {
            handle.stop_token.cancel();
        }
    }

    /// Start a new IDLE worker for the given folder. `report_first_connect`
    /// says whether its first connection counts as a reconnect.
    async fn start_worker(
        &mut self,
        account_config: Arc<AccountConfig>,
        folder: String,
        report_first_connect: bool,
    ) {
        let account = account_config.name.clone();
        let key = (account.clone(), folder.clone());
        let folder_for_worker = folder.clone();

        let stop_token = CancellationToken::new();
        let worker_stop = stop_token.clone();
        let global_shutdown = self.shutdown.clone();
        let tx_update = self.tx_update.clone();
        let credentials = self.credentials.clone();
        let cycle = self.cycle;
        self.configs.insert(account.clone(), account_config.clone());

        // Spawn the worker task
        tokio::spawn(async move {
            idle_worker(
                account_config,
                folder_for_worker,
                tx_update,
                worker_stop,
                global_shutdown,
                credentials,
                cycle,
                report_first_connect,
            )
            .await;
        });

        self.workers.insert(
            key,
            IdleWorkerHandle {
                stop_token,
                last_access: std::time::Instant::now(),
            },
        );

        crate::logger::info(&format!("Started IDLE for {}/{}", account, folder));
    }

    /// Get list of folders currently being watched for an account
    pub fn watched_folders(&self, account: &str) -> Vec<String> {
        self.workers
            .keys()
            .filter(|(acc, _)| acc == account)
            .map(|(_, folder)| folder.clone())
            .collect()
    }

    /// Check if a specific folder is being watched
    pub fn is_watching(&self, account: &str, folder: &str) -> bool {
        self.workers
            .contains_key(&(account.to_string(), folder.to_string()))
    }
}

/// IDLE worker that maintains a connection and listens for changes
#[allow(clippy::too_many_arguments)]
async fn idle_worker(
    account_config: Arc<AccountConfig>,
    folder: String,
    tx_update: mpsc::UnboundedSender<StateUpdate>,
    stop_token: CancellationToken,
    global_shutdown: CancellationToken,
    credentials: Arc<PasswordCache>,
    cycle: Duration,
    report_first_connect: bool,
) {
    let account = &account_config.name;
    let mut backoff = ReconnectBackoff::new();
    let mut report_connect = report_first_connect;

    loop {
        // Check for shutdown
        if stop_token.is_cancelled() || global_shutdown.is_cancelled() {
            crate::logger::info(&format!(
                "IDLE worker for {}/{} shutting down",
                account, folder
            ));
            return;
        }

        // Try to connect and run IDLE
        let session_start = std::time::Instant::now();
        match run_idle_session(
            &account_config,
            &folder,
            &tx_update,
            &stop_token,
            &credentials,
            cycle,
            report_connect,
        )
        .await
        {
            Ok(()) => {
                // Clean exit (manual stop or shutdown)
                return;
            }
            Err(Error::UnsupportedServer { server, reason }) => {
                // Nothing to wait for: the server will not grow IDLE by
                // trying again. The periodic sync is how this account's mail
                // arrives.
                crate::logger::warn(&format!(
                    "{account}/{folder}: no push from {server} ({reason}); the periodic sync \
                     covers it"
                ));
                return;
            }
            Err(e) => {
                // Whatever comes next is a reconnect, and mail may have
                // arrived in the meantime.
                report_connect = true;
                if matches!(e, Error::AuthenticationFailed { .. }) {
                    credentials.forget_imap(account).await;
                }
                // If the connection stayed healthy for a reasonable period before
                // failing, reset the backoff so we reconnect promptly instead of
                // carrying a stale multi-minute delay from earlier transient errors.
                backoff.on_session_ended(session_start.elapsed());
                let reconnect_delay = backoff.current_delay();

                crate::logger::warn(&format!(
                    "IDLE connection for {}/{} failed: {}. Reconnecting in {:?}",
                    account, folder, e, reconnect_delay
                ));

                // Wait before reconnecting (with cancellation check)
                tokio::select! {
                    _ = tokio::time::sleep(reconnect_delay) => {}
                    _ = stop_token.cancelled() => return,
                    _ = global_shutdown.cancelled() => return,
                }

                // Exponential backoff
                backoff.grow();
            }
        }
    }
}

/// One command on the IDLE connection, bounded.
async fn bounded<T, E>(
    limit: Duration,
    operation: &str,
    future: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T>
where
    E: Into<Error>,
{
    match tokio::time::timeout(limit, future).await {
        Ok(result) => result.map_err(Into::into),
        Err(_) => Err(Error::timeout(operation, limit)),
    }
}

/// Run a single IDLE session until interrupted or error
async fn run_idle_session(
    account_config: &AccountConfig,
    folder: &str,
    tx_update: &mpsc::UnboundedSender<StateUpdate>,
    stop_token: &CancellationToken,
    credentials: &PasswordCache,
    cycle: Duration,
    report_connect: bool,
) -> Result<()> {
    let password = credentials.imap(account_config).await?;

    // A session rather than an ImapClient: IDLE consumes the session and
    // hands it back, which a client that owns it cannot allow.
    let opened = crate::imap::open_session(
        &crate::imap::ConnectionSettings::for_account(account_config, password),
        &format!("idle {}/{}", account_config.name, folder),
        "direct",
    )
    .await?;
    let mut session = opened.session;
    let has_idle = opened.capabilities.iter().any(|cap| cap == "IDLE");

    if !has_idle {
        let _ = bounded(DONE_TIMEOUT, "LOGOUT", session.logout()).await;
        crate::logger::warn(&format!(
            "Server {} does not support IDLE",
            account_config.imap_server
        ));
        return Err(Error::UnsupportedServer {
            server: account_config.imap_server.clone(),
            reason: "IDLE extension not supported".to_string(),
        });
    }

    bounded(OPERATION_TIMEOUT, "SELECT", session.select(folder)).await?;

    crate::logger::info(&format!(
        "IDLE #{} established for {}/{}",
        opened.id, account_config.name, folder
    ));

    // Whatever arrived while there was no connection to hear of it: a
    // reconnect is itself a reason to sync. Nothing is lost when there was
    // nothing — a sync of an unchanged folder is one STATUS.
    if report_connect {
        let _ = tx_update.send(StateUpdate::ExternalChange {
            account: account_config.name.clone(),
            folder: folder.to_string(),
        });
    }

    // IDLE loop - re-establish every cycle, per RFC 2177, and to find out
    // whether the connection is still there. The session is consumed and
    // returned by idle operations.
    let mut current_session = session;

    loop {
        if stop_token.is_cancelled() {
            let _ = bounded(DONE_TIMEOUT, "LOGOUT", current_session.logout()).await;
            return Ok(());
        }

        // Start IDLE - this consumes the session. The server's continuation
        // is one round trip away, like the DONE; a server that takes longer
        // is not one to wait a minute for.
        let mut idle = current_session.idle();
        bounded(DONE_TIMEOUT, "IDLE", idle.init()).await?;

        let (idle_wait, interrupt) = idle.wait_with_timeout(cycle);

        // Wait for notification, timeout, or cancellation
        let result = tokio::select! {
            res = idle_wait => res,
            _ = stop_token.cancelled() => {
                drop(interrupt);
                // End IDLE and logout; neither may hang the shutdown.
                if let Ok(mut session) = bounded(DONE_TIMEOUT, "DONE", idle.done()).await {
                    let _ = bounded(DONE_TIMEOUT, "LOGOUT", session.logout()).await;
                }
                return Ok(());
            }
        };

        // End IDLE mode - get the session back. A server that does not
        // acknowledge the DONE within the probe timeout is not there any more,
        // whatever the socket says; the error makes the worker reconnect.
        let mut session = bounded(DONE_TIMEOUT, "DONE", idle.done()).await?;

        match result {
            Ok(IdleResponse::ManualInterrupt) => {
                // Interrupted by our code - check if cancelled
                if stop_token.is_cancelled() {
                    let _ = bounded(DONE_TIMEOUT, "LOGOUT", session.logout()).await;
                    return Ok(());
                }
            }
            Ok(IdleResponse::Timeout) => {
                // The cycle ended with nothing to report: a NOOP, then IDLE
                // again. The NOOP is the other half of the liveness check.
                bounded(DONE_TIMEOUT, "NOOP", session.noop()).await?;
            }
            Ok(IdleResponse::NewData(_data)) => {
                // Server sent notification - trigger sync
                crate::logger::info(&format!(
                    "IDLE notification for {}/{}",
                    account_config.name, folder
                ));

                let _ = tx_update.send(StateUpdate::ExternalChange {
                    account: account_config.name.clone(),
                    folder: folder.to_string(),
                });

                // Do NOOP to get latest state before restarting IDLE
                bounded(DONE_TIMEOUT, "NOOP", session.noop()).await?;
            }
            Err(e) => {
                // IDLE failed - let outer loop handle reconnection
                return Err(e.into());
            }
        }

        // Continue loop with the session
        current_session = session;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_at_initial_delay() {
        let backoff = ReconnectBackoff::new();
        assert_eq!(backoff.current_delay(), INITIAL_RECONNECT_DELAY);
    }

    #[test]
    fn backoff_grows_exponentially_and_caps_at_max() {
        let mut backoff = ReconnectBackoff::new();
        // Many consecutive failures must never exceed the cap.
        for _ in 0..100 {
            backoff.grow();
        }
        assert_eq!(backoff.current_delay(), MAX_RECONNECT_DELAY);
    }

    #[test]
    fn short_session_does_not_reset_backoff() {
        let mut backoff = ReconnectBackoff::new();
        backoff.grow();
        backoff.grow();
        let grown = backoff.current_delay();
        assert!(grown > INITIAL_RECONNECT_DELAY);

        // A session that failed quickly is not healthy; keep the accumulated delay.
        backoff.on_session_ended(HEALTHY_SESSION_THRESHOLD - Duration::from_secs(1));
        assert_eq!(backoff.current_delay(), grown);
    }

    #[test]
    fn backoff_resets_to_initial_after_healthy_period() {
        let mut backoff = ReconnectBackoff::new();

        // Simulate several transient failures, each with a short-lived session,
        // growing the backoff well past the initial delay.
        for _ in 0..5 {
            backoff.on_session_ended(Duration::from_secs(1));
            backoff.grow();
        }
        assert!(
            backoff.current_delay() > INITIAL_RECONNECT_DELAY,
            "backoff should have grown after repeated transient failures"
        );

        // Now a connection stays healthy for a reasonable period before failing.
        backoff.on_session_ended(HEALTHY_SESSION_THRESHOLD);

        // The next reconnect should use the initial delay again, not the stale
        // multi-minute backoff.
        assert_eq!(
            backoff.current_delay(),
            INITIAL_RECONNECT_DELAY,
            "backoff should reset to the initial delay after a healthy session"
        );
    }

    /// The cycle is the upper bound on how long a frozen connection goes
    /// unnoticed, so it has to stay well under what RFC 2177 allows and
    /// above what a server would call a client that will not sit still.
    #[test]
    fn the_idle_cycle_is_inside_the_rfc_bound() {
        assert!(IDLE_CYCLE < Duration::from_secs(29 * 60));
        assert!(IDLE_CYCLE >= Duration::from_secs(60));
    }
}
