//! Performance tuning constants for Orb
//!
//! These constants are optimized for responsiveness and multi-instance safety.

// === Database Settings ===

/// SQLite busy timeout in milliseconds
///
/// How long to wait for database locks before giving up.
/// 5 seconds allows other instances to complete short operations.
pub const DB_BUSY_TIMEOUT_MS: i64 = 5000;

/// Sync lock timeout in seconds
///
/// Locks older than this are considered stale and can be cleaned up.
/// 300 seconds (5 minutes) balances responsiveness with allowing
/// time for long sync operations to complete.
pub const SYNC_LOCK_TIMEOUT_SECONDS: i64 = 300;

/// Clock skew buffer for multi-machine deployments (seconds)
///
/// Added to SYNC_LOCK_TIMEOUT when cleaning up stale locks to account
/// for clock differences between machines. Prevents premature cleanup
/// of valid locks when clocks are not perfectly synchronized.
pub const CLOCK_SKEW_BUFFER_SECONDS: i64 = 60;

// === Retry Settings ===

/// Maximum number of retry attempts for general operations
pub const MAX_RETRY_ATTEMPTS: usize = 3;

/// Maximum number of retry attempts for IMAP sync operations
/// Higher than general retries because network issues are common during long syncs
pub const MAX_SYNC_RETRY_ATTEMPTS: usize = 5;

/// Initial delay between sync retries in milliseconds
pub const SYNC_RETRY_INITIAL_DELAY_MS: u64 = 1000;

/// Maximum delay between sync retries in milliseconds
pub const SYNC_RETRY_MAX_DELAY_MS: u64 = 30000;

// === Network Settings ===

/// IMAP connection timeout in seconds
pub const IMAP_CONNECTION_TIMEOUT_SECONDS: u64 = 30;

/// How long any one IMAP command, or one response inside a FETCH stream, may
/// go unanswered before the connection is given up on.
///
/// Bounds every command the client sends. A command that hits it leaves the
/// connection in an unknown state, so the connection is dropped and the
/// operation retried on a new one.
pub const IMAP_OPERATION_TIMEOUT_SECONDS: u64 = 60;

/// How long a liveness probe may take: the NOOP the pool sends before handing
/// out a connection that has sat idle, and the DONE that ends an IDLE.
///
/// Much shorter than the operation timeout, because a healthy server answers
/// either within a round trip, and the whole point of the probe is to find
/// out quickly whether the connection is still there.
pub const IMAP_PROBE_TIMEOUT_SECONDS: u64 = 10;

/// How long getting a connection may take for something the user is waiting
/// on, such as a message being opened. Sync keeps the longer timeouts above.
pub const IMAP_INTERACTIVE_CONNECT_TIMEOUT_SECONDS: u64 = 8;

/// For how long after a server was found unreachable what the user is waiting
/// on does not try it again, but answers from what is stored. A sync that
/// reaches it ends this at once.
pub const IMAP_UNREACHABLE_HOLD_SECONDS: u64 = 30;

/// A pooled connection unused for longer than this is probed with a NOOP
/// before it is handed out again.
pub const IMAP_PROBE_AFTER_IDLE_SECONDS: u64 = 30;

/// TCP keepalive: idle time before the first probe, the interval between
/// probes, and how many unanswered probes end the connection.
///
/// A connection whose path has silently gone away — a NAT that forgot the
/// flow, a network that changed under a sleeping machine — otherwise looks
/// exactly like a quiet server until the next command times out. With
/// keepalive the kernel notices within about two minutes and the next read
/// fails at once.
pub const TCP_KEEPALIVE_IDLE_SECONDS: u64 = 60;
pub const TCP_KEEPALIVE_INTERVAL_SECONDS: u64 = 15;
pub const TCP_KEEPALIVE_RETRIES: u32 = 4;

/// How long the password command may run before it is given up on.
///
/// Long enough for a keychain prompt or a pinentry that waits for a person;
/// bounded so that one that never answers does not hang a sync forever.
pub const PASSWORD_COMMAND_TIMEOUT_SECONDS: u64 = 120;

// === Sync Scheduling ===

/// How often every synced folder of an account is synced when the account's
/// configuration does not say.
pub const DEFAULT_SYNC_INTERVAL_SECONDS: u64 = 300;

/// Messages fetched per page on the first sync of a folder.
///
/// Each page is committed before the next is fetched, so a huge folder
/// neither holds everything in memory nor holds one write transaction for
/// minutes.
pub const INITIAL_FETCH_PAGE: usize = 5_000;

/// How often a running sync renews its folder lock, independently of what the
/// fetch is doing. Well inside `SYNC_LOCK_TIMEOUT_SECONDS`.
pub const SYNC_LOCK_RENEW_INTERVAL_SECONDS: u64 = 60;

/// Idle IMAP connections the pool keeps per account; one handed back beyond
/// that is closed. Two, so that the body backfill and a sync running beside
/// it each find a warm connection instead of one of them logging in again:
/// see "The connection pool" in docs/CONCURRENCY_DESIGN.md for what that was
/// measured to buy. With up to three IDLE connections that is five per
/// account, well within what servers allow (ten per user and address by
/// Dovecot's default, fifteen at Gmail).
pub const POOL_IDLE_PER_ACCOUNT: usize = 2;

/// How often the wake detector looks at the clock, and how much wall-clock
/// time has to pass in one tick before it counts as a sleep.
pub const WAKE_DETECT_TICK_SECONDS: u64 = 15;
pub const WAKE_GAP_SECONDS: u64 = 60;

/// How long a push-triggered sync that found its folder locked waits before
/// asking again, and how many times it does so.
pub const SYNC_SKIPPED_RETRY_DELAY_SECONDS: u64 = 5;
pub const SYNC_SKIPPED_RETRY_ATTEMPTS: u32 = 6;

// === UI Settings ===

/// Default number of messages to load per batch
///
/// Smaller batch = faster initial load, more frequent reloads
/// Larger batch = slower initial load, fewer reloads
/// 50 is a good balance for most email clients
pub const DEFAULT_MESSAGE_BATCH_SIZE: usize = 50;

/// Size of the message window for pagination
///
/// This determines how many messages are kept in memory at once.
/// Set to 3x typical viewport (~50 rows) to provide smooth scrolling
/// buffer while keeping memory usage bounded.
pub const MESSAGE_WINDOW_SIZE: usize = 150;

/// Pagination load threshold
///
/// When cursor is within this many messages of the window boundary,
/// trigger loading the next/previous batch. This gives time for the
/// async load to complete before the user hits the boundary.
pub const PAGINATION_LOAD_THRESHOLD: usize = 20;

/// Maximum number of messages to display in the UI
///
/// Prevents performance issues with very large mailboxes
pub const MAX_MESSAGES_DISPLAYED: usize = 1000;

// === External Change Detection ===

/// How often to check for external database changes (in seconds)
///
/// Lower = more responsive to other instances, higher CPU usage
/// Higher = less CPU usage, slower to detect changes
/// 2 seconds is a good balance for multi-instance scenarios
pub const EXTERNAL_CHANGE_CHECK_INTERVAL_SECONDS: u64 = 2;

// === Task uploads (todo-spec §23) ===

/// How long the task changes have to stop before they are uploaded.
///
/// Every upload is an APPEND and a delete, so typing a title or a reorder
/// that renumbers a list would otherwise send one superseded message after
/// another.
pub const TASK_UPLOAD_DEBOUNCE_MS: u64 = 1_000;

/// The longest a burst of task changes holds the upload back.
pub const TASK_UPLOAD_DEBOUNCE_MAX_MS: u64 = 5_000;

/// How often the tasks folder is asked whether another machine changed it,
/// and an upload that failed is tried again.
pub const TASK_POLL_INTERVAL_SECONDS: u64 = 60;
