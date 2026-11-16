//! IMAP connection pool for maintaining persistent connections.
//!
//! This module provides connection pooling to avoid the overhead of
//! TLS handshake + authentication for each sync operation (~50-100ms savings).
//!
//! # Design
//!
//! - A few warm connections per account (`POOL_IDLE_PER_ACCOUNT`); a caller
//!   never waits for one, and opens another when all are in use
//! - Automatic keep-alive using NOOP
//! - Transparent reconnection on stale/broken connections
//! - Thread-safe for use across async tasks

use crate::Result;
use crate::error::Error;
use crate::imap::ImapClient;
use crate::logger;
use secrecy::SecretString;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// A connection unused for longer than this is asked whether it is still
/// there before it is handed out.
const PROBE_AFTER_IDLE: Duration =
    Duration::from_secs(crate::constants::IMAP_PROBE_AFTER_IDLE_SECONDS);

/// A pooled IMAP connection and when it was last in somebody's hands.
struct PooledConnection {
    client: ImapClient,
    /// When the connection was released. It was in use, and so known to be
    /// alive, until then; what happened to it since is what the probe asks.
    released_at: Instant,
}

impl PooledConnection {
    fn new(client: ImapClient) -> Self {
        Self {
            client,
            released_at: Instant::now(),
        }
    }

    fn needs_probe(&self) -> bool {
        self.released_at.elapsed() > PROBE_AFTER_IDLE
    }
}

/// Connection pool configuration for an account
#[derive(Clone)]
pub struct AccountConnectionConfig {
    pub settings: crate::imap::ConnectionSettings,
}

impl AccountConnectionConfig {
    /// What the pool needs to know about an account, with the password the
    /// caller has already obtained.
    pub fn for_account(account: &crate::config::AccountConfig, password: SecretString) -> Self {
        Self {
            settings: crate::imap::ConnectionSettings::for_account(account, password),
        }
    }
}

/// IMAP connection pool that keeps a few warm connections per account
pub struct ConnectionPool {
    /// The idle connections of each account, the most recently used last.
    connections: Mutex<HashMap<String, Vec<PooledConnection>>>,
    configs: Mutex<HashMap<String, AccountConnectionConfig>>,
    /// How many idle connections an account keeps; one handed back beyond
    /// that is closed.
    idle_per_account: std::sync::atomic::AtomicUsize,
    /// The accounts whose server could not be reached, and when that was
    /// last found. Set by any acquire that fails for want of the server and
    /// cleared by any that succeeds, sync's included.
    unreachable: std::sync::Mutex<HashMap<String, Instant>>,
    /// How long [`Self::acquire_interactive`] waits for a connection.
    interactive_timeout: Duration,
}

impl ConnectionPool {
    /// Create a new empty connection pool
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            configs: Mutex::new(HashMap::new()),
            idle_per_account: std::sync::atomic::AtomicUsize::new(
                crate::constants::POOL_IDLE_PER_ACCOUNT,
            ),
            unreachable: std::sync::Mutex::new(HashMap::new()),
            interactive_timeout: Duration::from_secs(
                crate::constants::IMAP_INTERACTIVE_CONNECT_TIMEOUT_SECONDS,
            ),
        }
    }

    /// When the account's server was last found unreachable, if it has not
    /// been reached since.
    pub fn unreachable_since(&self, account_name: &str) -> Option<Instant> {
        self.unreachable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account_name)
            .copied()
    }

    fn record_reach(&self, account_name: &str, reached: bool) {
        let mut unreachable = self.unreachable.lock().unwrap_or_else(|e| e.into_inner());
        if reached {
            unreachable.remove(account_name);
        } else {
            unreachable.insert(account_name.to_string(), Instant::now());
        }
    }

    /// A connection for something the user is waiting on — a message being
    /// opened, its sender checked — as [`Self::acquire`] gives one, but
    /// without the long waits that suit a sync in the background.
    ///
    /// Fails at once while the server was found unreachable within
    /// [`crate::constants::IMAP_UNREACHABLE_HOLD_SECONDS`], and otherwise
    /// gives connecting [`crate::constants::IMAP_INTERACTIVE_CONNECT_TIMEOUT_SECONDS`]
    /// rather than the connect and probe timeouts. Only getting the
    /// connection is bounded: a large message on a slow link still arrives,
    /// under the per-response timeout every command has.
    pub async fn acquire_interactive(
        &self,
        account_name: &str,
        reason: &str,
    ) -> Result<ImapClient> {
        let hold = Duration::from_secs(crate::constants::IMAP_UNREACHABLE_HOLD_SECONDS);
        if let Some(since) = self.unreachable_since(account_name)
            && since.elapsed() < hold
        {
            return Err(Error::Unreachable {
                account: account_name.to_string(),
                reason: format!("not reached {}s ago", since.elapsed().as_secs()),
            });
        }
        match tokio::time::timeout(self.interactive_timeout, self.acquire(account_name, reason))
            .await
        {
            Ok(result) => result,
            Err(_) => {
                self.record_reach(account_name, false);
                Err(Error::Unreachable {
                    account: account_name.to_string(),
                    reason: format!("no answer within {:?}", self.interactive_timeout),
                })
            }
        }
    }

    /// Keep up to `count` idle connections per account (at least one).
    pub fn set_idle_per_account(&self, count: usize) {
        self.idle_per_account
            .store(count.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// Register an account's connection configuration
    pub async fn register_account(&self, account_name: &str, config: AccountConnectionConfig) {
        let mut configs = self.configs.lock().await;
        configs.insert(account_name.to_string(), config);
    }

    /// Get a connection from the pool, creating one if necessary.
    ///
    /// The returned connection is removed from the pool and must be
    /// returned using `release()` when done, or dropped with
    /// [`Self::give_back`] saying so when it has failed.
    ///
    /// A connection that has sat in the pool for a while is probed with a
    /// NOOP first, on the short probe timeout: a connection the server, a NAT
    /// or a night's sleep has quietly closed looks fresh by every clock the
    /// pool has, and the alternative is handing it out and finding out when
    /// the first real command times out a minute later. No age limit beyond
    /// that — a connection that answers is a connection worth keeping, and
    /// the whole point of a pool is one authenticated connection per account
    /// that stays warm.
    ///
    /// `reason` is what the connection is for, in the caller's words; it goes
    /// into the log and the connection ledger whenever a new one has to be
    /// opened.
    pub async fn acquire(&self, account_name: &str, reason: &str) -> Result<ImapClient> {
        let result = self.acquire_connection(account_name, reason).await;
        match &result {
            Ok(_) => self.record_reach(account_name, true),
            // Refused credentials are an answer: the server is there.
            Err(Error::AuthenticationFailed { .. }) => self.record_reach(account_name, true),
            Err(_) => self.record_reach(account_name, false),
        }
        result
    }

    async fn acquire_connection(&self, account_name: &str, reason: &str) -> Result<ImapClient> {
        let start = Instant::now();

        let existing = {
            let mut connections = self.connections.lock().await;
            connections.get_mut(account_name).and_then(Vec::pop)
        };

        if let Some(mut pooled) = existing {
            if pooled.needs_probe() {
                match pooled.client.probe().await {
                    Ok(()) => logger::debug(&format!(
                        "pool: #{} for '{}' answered a NOOP after {:?} idle",
                        pooled.client.id(),
                        account_name,
                        pooled.released_at.elapsed()
                    )),
                    Err(e) => {
                        logger::debug(&format!(
                            "pool: #{} for '{}' failed a NOOP after {:?} idle ({}), opening a \
                             new one",
                            pooled.client.id(),
                            account_name,
                            pooled.released_at.elapsed(),
                            e
                        ));
                        drop(pooled);
                        return self
                            .create_new_connection(account_name, reason, "pooled NOOP failed")
                            .await;
                    }
                }
            }

            logger::debug(&format!(
                "pool: reusing #{} for '{}' ({}, acquired in {:?})",
                pooled.client.id(),
                account_name,
                reason,
                start.elapsed()
            ));
            return Ok(pooled.client);
        }

        self.create_new_connection(account_name, reason, "no pooled connection")
            .await
    }

    /// Return a connection to the pool for reuse.
    pub async fn release(&self, account_name: &str, client: ImapClient) {
        let mut connections = self.connections.lock().await;
        let id = client.id();
        let idle = connections.entry(account_name.to_string()).or_default();
        idle.push(PooledConnection::new(client));
        let keep = self
            .idle_per_account
            .load(std::sync::atomic::Ordering::Relaxed);
        if idle.len() > keep {
            // The longest idle goes: the others were in use more recently.
            let dropped = idle.remove(0);
            logger::debug(&format!(
                "pool: #{} for '{}' released, closing #{} beyond the {} kept",
                id,
                account_name,
                dropped.client.id(),
                keep
            ));
        } else {
            logger::debug(&format!("pool: #{} for '{}' released", id, account_name));
        }
    }

    /// Return a connection the caller is done with, keeping it only if it is
    /// known to be usable.
    ///
    /// A connection that produced a connection error is dropped: handing it
    /// to the next caller fails that caller the same way. One that produced
    /// a server refusal, or no error at all, goes back.
    pub async fn give_back(
        &self,
        account_name: &str,
        client: ImapClient,
        failed_with: Option<&Error>,
    ) {
        match failed_with {
            Some(error) if error.is_connection_error() => {
                logger::debug(&format!(
                    "pool: #{} for '{}' dropped after a connection error: {}",
                    client.id(),
                    account_name,
                    error
                ));
                drop(client);
            }
            _ => self.release(account_name, client).await,
        }
    }

    /// Remove and close all connections from the pool
    pub async fn close_all(&self) {
        let mut connections = self.connections.lock().await;
        for (account_name, idle) in connections.drain() {
            logger::debug(&format!(
                "pool: closing {} connection(s) for '{}'",
                idle.len(),
                account_name
            ));
            // ImapClient's Drop will handle disconnection
            drop(idle);
        }
    }

    /// Remove and close connection for a specific account
    pub async fn close(&self, account_name: &str) {
        let mut connections = self.connections.lock().await;
        if let Some(idle) = connections.remove(account_name) {
            logger::debug(&format!(
                "pool: closing {} connection(s) for '{}'",
                idle.len(),
                account_name
            ));
            drop(idle);
        }
    }

    /// Get pool statistics for debugging
    pub async fn stats(&self) -> PoolStats {
        let connections = self.connections.lock().await;
        PoolStats {
            total_connections: connections.values().map(Vec::len).sum(),
            accounts: connections
                .iter()
                .filter(|(_, idle)| !idle.is_empty())
                .map(|(account, _)| account.clone())
                .collect(),
        }
    }

    /// Create a new connection for an account
    async fn create_new_connection(
        &self,
        account_name: &str,
        reason: &str,
        cause: &str,
    ) -> Result<ImapClient> {
        let start = Instant::now();

        // Get config for this account
        let config = {
            let configs = self.configs.lock().await;
            configs.get(account_name).cloned()
        };

        let Some(config) = config else {
            return Err(crate::error::Error::NotFound(format!(
                "No connection config registered for account '{}'",
                account_name
            )));
        };

        let client = ImapClient::open(&config.settings, reason, cause).await?;

        logger::debug(&format!(
            "pool: opened #{} for '{}' in {:?}",
            client.id(),
            account_name,
            start.elapsed()
        ));

        Ok(client)
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Pool statistics for debugging
#[derive(Debug)]
pub struct PoolStats {
    pub total_connections: usize,
    pub accounts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap::TlsMode;

    /// A server that takes the connection and then says nothing, as one at
    /// the far end of a link that has gone bad: an interactive acquire gives
    /// up on it within its own short wait, and the next one does not try
    /// until the hold is over. A plain acquire is not held back.
    #[tokio::test]
    async fn an_unreachable_server_is_given_up_on_quickly_and_then_not_tried() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let silent = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });

        let mut pool = ConnectionPool::new();
        pool.interactive_timeout = Duration::from_millis(300);
        pool.register_account(
            "silent",
            AccountConnectionConfig {
                settings: crate::imap::ConnectionSettings::plain(
                    "127.0.0.1",
                    port,
                    "user",
                    &SecretString::from("password".to_string()),
                    TlsMode::Verify,
                ),
            },
        )
        .await;

        let started = Instant::now();
        let first = pool.acquire_interactive("silent", "test").await;
        assert!(matches!(first, Err(Error::Unreachable { .. })));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(pool.unreachable_since("silent").is_some());

        let again = Instant::now();
        let second = pool.acquire_interactive("silent", "test").await;
        assert!(matches!(second, Err(Error::Unreachable { .. })));
        assert!(
            again.elapsed() < Duration::from_millis(50),
            "not tried again"
        );

        pool.record_reach("silent", true);
        assert!(pool.unreachable_since("silent").is_none());
        silent.abort();
    }

    #[test]
    fn test_pool_creation() {
        let pool = ConnectionPool::new();
        // Just verify it creates without panic
        drop(pool);
    }

    #[tokio::test]
    async fn test_pool_register_account() {
        let pool = ConnectionPool::new();

        pool.register_account(
            "test_account",
            AccountConnectionConfig {
                settings: crate::imap::ConnectionSettings::plain(
                    "imap.example.com",
                    993,
                    "user",
                    &SecretString::from("password".to_string()),
                    TlsMode::Verify,
                ),
            },
        )
        .await;

        // Verify config was registered by checking stats
        // (we can't acquire without a real server, but we can check registration)
        let configs = pool.configs.lock().await;
        assert!(configs.contains_key("test_account"));
        assert_eq!(
            configs.get("test_account").unwrap().settings.server,
            "imap.example.com"
        );
        assert_eq!(configs.get("test_account").unwrap().settings.port, 993);
    }

    #[tokio::test]
    async fn test_pool_register_multiple_accounts() {
        let pool = ConnectionPool::new();

        // Register multiple accounts
        for i in 0..5 {
            pool.register_account(
                &format!("account_{}", i),
                AccountConnectionConfig {
                    settings: crate::imap::ConnectionSettings::plain(
                        &format!("imap{}.example.com", i),
                        993,
                        &format!("user{}", i),
                        &SecretString::from("password".to_string()),
                        TlsMode::Verify,
                    ),
                },
            )
            .await;
        }

        let configs = pool.configs.lock().await;
        assert_eq!(configs.len(), 5);
        for i in 0..5 {
            assert!(configs.contains_key(&format!("account_{}", i)));
        }
    }

    #[tokio::test]
    async fn test_pool_register_overwrites_existing() {
        let pool = ConnectionPool::new();

        // Register account
        pool.register_account(
            "test_account",
            AccountConnectionConfig {
                settings: crate::imap::ConnectionSettings::plain(
                    "old.example.com",
                    993,
                    "user",
                    &SecretString::from("password".to_string()),
                    TlsMode::Verify,
                ),
            },
        )
        .await;

        // Register same account with different config
        pool.register_account(
            "test_account",
            AccountConnectionConfig {
                settings: crate::imap::ConnectionSettings::plain(
                    "new.example.com",
                    143,
                    "newuser",
                    &SecretString::from("newpassword".to_string()),
                    TlsMode::Verify,
                ),
            },
        )
        .await;

        let configs = pool.configs.lock().await;
        assert_eq!(configs.len(), 1);
        assert_eq!(
            configs.get("test_account").unwrap().settings.server,
            "new.example.com"
        );
        assert_eq!(configs.get("test_account").unwrap().settings.port, 143);
    }

    #[tokio::test]
    async fn test_pool_acquire_fails_without_registration() {
        let pool = ConnectionPool::new();

        // Try to acquire without registering - should fail
        let result = pool.acquire("nonexistent_account", "test").await;
        assert!(result.is_err());

        // Check error message
        match result {
            Err(e) => {
                let err_str = e.to_string();
                assert!(
                    err_str.contains("No connection config registered"),
                    "Error should mention missing config: {}",
                    err_str
                );
            }
            Ok(_) => panic!("Expected error but got success"),
        }
    }

    #[tokio::test]
    async fn test_pool_stats() {
        let pool = ConnectionPool::new();

        // Empty pool
        let stats = pool.stats().await;
        assert_eq!(stats.total_connections, 0);
        assert!(stats.accounts.is_empty());
    }

    #[tokio::test]
    async fn test_pool_close_all() {
        let pool = ConnectionPool::new();

        // Register accounts
        for i in 0..3 {
            pool.register_account(
                &format!("account_{}", i),
                AccountConnectionConfig {
                    settings: crate::imap::ConnectionSettings::plain(
                        "imap.example.com",
                        993,
                        "user",
                        &SecretString::from("password".to_string()),
                        TlsMode::Verify,
                    ),
                },
            )
            .await;
        }

        // Close all (even without connections, should not panic)
        pool.close_all().await;

        let stats = pool.stats().await;
        assert_eq!(stats.total_connections, 0);
    }

    #[tokio::test]
    async fn test_pool_close_specific_account() {
        let pool = ConnectionPool::new();

        // Register account
        pool.register_account(
            "test_account",
            AccountConnectionConfig {
                settings: crate::imap::ConnectionSettings::plain(
                    "imap.example.com",
                    993,
                    "user",
                    &SecretString::from("password".to_string()),
                    TlsMode::Verify,
                ),
            },
        )
        .await;

        // Close specific account (even without connection, should not panic)
        pool.close("test_account").await;
        pool.close("nonexistent").await; // Should also not panic
    }
}
