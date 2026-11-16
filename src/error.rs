use thiserror::Error;

/// Comprehensive error types with full context for debugging and user feedback
#[derive(Error, Debug)]
pub enum Error {
    // === IMAP Errors (with context) ===
    #[error("IMAP sync failed for folder '{folder}' in account '{account}': {reason}")]
    ImapSyncFailed {
        account: String,
        folder: String,
        reason: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("IMAP protocol error: {details}")]
    ImapProtocolError {
        details: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("IMAP error: {0}")]
    Imap(String),

    #[error("Unsupported server '{server}': {reason}")]
    UnsupportedServer { server: String, reason: String },

    /// The account's server cannot be reached just now, so what was asked
    /// of it was not attempted, or given up on quickly.
    #[error("The server of '{account}' cannot be reached ({reason})")]
    Unreachable { account: String, reason: String },

    /// The server refused the credentials. Not retried, and the cached
    /// password is forgotten so the next attempt runs the command again.
    #[error("Login failed: {details}")]
    AuthenticationFailed { details: String },

    // === Database Errors (with operation context) ===
    #[error("Database operation '{operation}' failed")]
    DatabaseError {
        operation: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Database transaction failed: {operation}")]
    TransactionFailed {
        operation: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("Database connection pool error: {0}")]
    PoolError(String),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    // === Consistency Errors ===
    #[error("UIDVALIDITY changed for folder '{folder}' ({old} → {new}), full resync required")]
    UidValidityChanged {
        folder: String,
        account: String,
        old: u32,
        new: u32,
    },

    // === Configuration Errors ===
    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("Invalid configuration: {field}: {reason}")]
    InvalidConfig { field: String, reason: String },

    // === Data Errors ===
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Email parse error: {0}")]
    Parse(#[from] mailparse::MailParseError),

    // === Sync Errors ===
    #[error("Sync error: {0}")]
    Sync(String),

    #[error("Sync aborted: {reason}")]
    SyncAborted { reason: String },

    // === Network Errors ===
    #[error("Network timeout after {seconds}s: {operation}")]
    Timeout { operation: String, seconds: u64 },

    #[error("TLS error: {0}")]
    Tls(#[from] async_native_tls::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // === Retry Exhausted ===
    #[error("Operation '{operation}' failed after {attempts} attempts: {last_error}")]
    RetryExhausted {
        operation: String,
        attempts: usize,
        last_error: String,
    },

    // === Concurrency Errors ===
    #[error("Concurrency conflict in '{operation}': {reason}")]
    ConcurrencyConflict { operation: String, reason: String },
}

/// A payload that cannot be read or written is a programming error on this
/// side, or a database row from another version; neither a retry nor a
/// reconnect helps.
impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Sync(format!("serialization failed: {err}"))
    }
}

/// Transport failures keep their type; the server's own answers become
/// [`Error::Imap`].
///
/// Everything that decides whether a connection can be reused or an operation
/// retried reads the variant, so a dropped socket must arrive as [`Error::Io`]
/// and not as the text "ConnectionLost" inside a string. The one failure that
/// is neither is a response the parser cannot read: the connection is left
/// with unread bytes in it and is as unusable as a dropped one, but asking the
/// same question again gets the same unreadable answer, so it is a protocol
/// error rather than a transport one.
impl From<async_imap::error::Error> for Error {
    fn from(err: async_imap::error::Error) -> Self {
        use async_imap::error::Error as ImapError;
        match err {
            // imap-proto's parse failures surface as an I/O error of kind
            // `Other`, worded by `async_imap::imap_stream`.
            ImapError::Io(io)
                if io.kind() == std::io::ErrorKind::Other
                    && io.to_string().contains("during parsing of") =>
            {
                Error::ImapProtocolError {
                    details: format!("response could not be parsed: {io}"),
                    source: None,
                }
            }
            ImapError::Io(io) => Error::Io(io),
            ImapError::ConnectionLost => Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "the server closed the connection",
            )),
            ImapError::Parse(parse) => Error::ImapProtocolError {
                details: format!("response could not be parsed: {parse}"),
                source: None,
            },
            ImapError::No(details) => Error::Imap(format!("NO {details}")),
            ImapError::Bad(details) => Error::Imap(format!("BAD {details}")),
            other => Error::Imap(other.to_string()),
        }
    }
}

// Helper methods for error context
impl Error {
    /// Create an IMAP sync error with context
    pub fn imap_sync_failed(
        account: impl Into<String>,
        folder: impl Into<String>,
        reason: impl Into<String>,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Error::ImapSyncFailed {
            account: account.into(),
            folder: folder.into(),
            reason: reason.into(),
            source,
        }
    }

    /// Create a database error with operation context
    pub fn database_error(operation: impl Into<String>, source: rusqlite::Error) -> Self {
        Error::DatabaseError {
            operation: operation.into(),
            source,
        }
    }

    /// Create a UIDVALIDITY change error
    pub fn uidvalidity_changed(
        account: impl Into<String>,
        folder: impl Into<String>,
        old: u32,
        new: u32,
    ) -> Self {
        Error::UidValidityChanged {
            folder: folder.into(),
            account: account.into(),
            old,
            new,
        }
    }

    /// A timeout, with the operation that hit it.
    pub fn timeout(operation: impl Into<String>, after: std::time::Duration) -> Self {
        Error::Timeout {
            operation: operation.into(),
            seconds: after.as_secs(),
        }
    }

    /// Whether asking again, after a pause, has a chance of a different answer.
    ///
    /// Transport failures and lock contention do; the server's own NO and BAD
    /// are its final word, and so is a response the parser cannot read.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Timeout { .. } | Error::Io(_) | Error::Tls(_) => true,
            Error::ConcurrencyConflict { .. } => true,
            Error::Database(e) => is_busy(e),
            Error::DatabaseError { source, .. } | Error::TransactionFailed { source, .. } => {
                is_busy(source)
            }
            Error::ImapProtocolError { .. }
            | Error::Imap(_)
            | Error::UidValidityChanged { .. }
            | Error::InvalidInput(_)
            | Error::NotFound(_)
            | Error::SyncAborted { .. }
            | Error::RetryExhausted { .. }
            | Error::ImapSyncFailed { .. }
            | Error::Config(_)
            | Error::InvalidConfig { .. }
            | Error::Parse(_)
            | Error::Sync(_)
            | Error::PoolError(_)
            | Error::AuthenticationFailed { .. }
            | Error::Unreachable { .. }
            | Error::UnsupportedServer { .. } => false,
        }
    }

    /// Whether the connection that produced this error is still usable.
    ///
    /// A connection that is not must never go back into the pool: handing it
    /// to the next operation fails that one the same way. Narrower than
    /// [`Self::is_retryable`], which also covers database contention that
    /// leaves the IMAP connection perfectly healthy, and wider in one place:
    /// a response the parser could not read leaves its remaining bytes in
    /// the socket, so the connection is lost even though the server is fine.
    pub fn is_connection_error(&self) -> bool {
        matches!(
            self,
            Error::Timeout { .. } | Error::Io(_) | Error::Tls(_) | Error::ImapProtocolError { .. }
        )
    }
}

/// What kind of thing went wrong, for an interface that has to say more than
/// "error" about an account.
///
/// The error type carries the detail; this is the coarse reading of it that a
/// sidebar glyph or a status panel can act on. `Auth`, `Certificate` and
/// `Unreachable` are the three a person can do something about without
/// reading a log: fix the password command, trust or replace the certificate,
/// check the network. `Server` is the server's own refusal of a command, and
/// `Other` is everything that is neither the connection's fault nor the
/// server's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The server refused the credentials.
    Auth,
    /// The TLS handshake failed, which against a mail server is nearly always
    /// the certificate.
    Certificate,
    /// The server could not be reached, or stopped answering.
    Unreachable,
    /// The server answered a command with NO or BAD, or with something that
    /// could not be read.
    Server,
    /// Anything else: the database, the configuration, a parse failure.
    Other,
}

impl FailureKind {
    /// How much a kind deserves the user's attention, highest first.
    ///
    /// Used to pick the one failure to show for an account when a sync
    /// produced several: a refused login is the reason nothing else worked,
    /// so it outranks the folder errors that followed it.
    pub fn severity(self) -> u8 {
        match self {
            FailureKind::Auth => 4,
            FailureKind::Certificate => 3,
            FailureKind::Unreachable => 2,
            FailureKind::Server => 1,
            FailureKind::Other => 0,
        }
    }
}

impl Error {
    /// The coarse kind of this error. See [`FailureKind`].
    pub fn kind(&self) -> FailureKind {
        match self {
            Error::AuthenticationFailed { .. } => FailureKind::Auth,
            Error::Tls(_) => FailureKind::Certificate,
            Error::Timeout { .. } | Error::Io(_) | Error::RetryExhausted { .. } => {
                FailureKind::Unreachable
            }
            Error::Imap(_)
            | Error::ImapProtocolError { .. }
            | Error::UnsupportedServer { .. }
            | Error::UidValidityChanged { .. } => FailureKind::Server,
            // A folder sync names its cause in text only; the cause decides.
            Error::ImapSyncFailed { reason, .. } => classify_text(reason),
            _ => FailureKind::Other,
        }
    }
}

/// The kind of a failure that only survives as text — a folder sync's reason,
/// or an error the scheduler carried across a task boundary as a string.
pub fn classify_text(text: &str) -> FailureKind {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("login failed") || lowered.contains("authenticat") {
        FailureKind::Auth
    } else if lowered.contains("certificate") || lowered.contains("tls error") {
        FailureKind::Certificate
    } else if lowered.contains("timeout")
        || lowered.contains("timed out")
        || lowered.contains("connection")
        || lowered.contains("io error")
        || lowered.contains("unreachable")
    {
        FailureKind::Unreachable
    } else if lowered.contains("imap error") || lowered.contains("protocol error") {
        FailureKind::Server
    } else {
        FailureKind::Other
    }
}

/// SQLite's two ways of saying "somebody else is writing".
fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::DatabaseBusy
                || err.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retryable_errors() {
        assert!(
            Error::Timeout {
                operation: "sync".to_string(),
                seconds: 30
            }
            .is_retryable()
        );

        assert!(
            !Error::UidValidityChanged {
                folder: "INBOX".to_string(),
                account: "test".to_string(),
                old: 1,
                new: 2,
            }
            .is_retryable()
        );
    }

    #[test]
    fn test_concurrency_conflict_is_retryable() {
        let err = Error::ConcurrencyConflict {
            operation: "sync_folder".to_string(),
            reason: "sync_version changed".to_string(),
        };

        assert!(err.is_retryable());
    }

    /// The sidebar has three glyphs to show and a log is not one of them, so
    /// every error has to fall into a kind a person can act on.
    #[test]
    fn errors_classify_into_the_kinds_the_interface_shows() {
        let login = Error::AuthenticationFailed {
            details: "invalid credentials".into(),
        };
        assert_eq!(login.kind(), FailureKind::Auth);
        assert_eq!(
            Error::timeout("connecting", std::time::Duration::from_secs(30)).kind(),
            FailureKind::Unreachable
        );
        assert_eq!(
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused"
            ))
            .kind(),
            FailureKind::Unreachable
        );
        assert_eq!(
            Error::Imap("NO such mailbox".into()).kind(),
            FailureKind::Server
        );
        assert_eq!(Error::Sync("odd".into()).kind(), FailureKind::Other);
        assert_eq!(
            Error::imap_sync_failed("Work", "INBOX", "Login failed: bad password", None).kind(),
            FailureKind::Auth
        );
        assert_eq!(
            classify_text("TLS error: the certificate was not trusted"),
            FailureKind::Certificate
        );
        assert!(FailureKind::Auth.severity() > FailureKind::Unreachable.severity());
    }

    /// A dropped socket arrives typed, whatever async-imap called it, so the
    /// decision to reconnect never reads an error's text.
    #[test]
    fn transport_failures_are_connection_errors_and_retryable() {
        use async_imap::error::Error as ImapError;
        use std::io::{Error as IoError, ErrorKind};

        for raw in [
            ImapError::ConnectionLost,
            ImapError::Io(IoError::new(ErrorKind::UnexpectedEof, "eof")),
            ImapError::Io(IoError::new(ErrorKind::ConnectionReset, "reset")),
            ImapError::Io(IoError::new(ErrorKind::BrokenPipe, "pipe")),
            ImapError::Io(IoError::new(ErrorKind::TimedOut, "timed out")),
        ] {
            let description = format!("{raw:?}");
            let err = Error::from(raw);
            assert!(
                err.is_connection_error(),
                "{description} should be a connection error"
            );
            assert!(err.is_retryable(), "{description} should be retryable");
        }

        let timeout = Error::timeout("FETCH", std::time::Duration::from_secs(60));
        assert!(timeout.is_connection_error());
        assert!(timeout.is_retryable());
    }

    /// The server's NO and BAD are its final word: not a reason to reconnect,
    /// and asking again gets the same answer.
    #[test]
    fn server_refusals_are_final_and_leave_the_connection_usable() {
        use async_imap::error::Error as ImapError;

        for raw in [
            ImapError::No("code: None, info: Some(\"Mailbox doesn't exist\")".into()),
            ImapError::Bad("code: None, info: Some(\"Error in IMAP command\")".into()),
        ] {
            let err = Error::from(raw);
            assert!(!err.is_retryable(), "{err} should not be retried");
            assert!(
                !err.is_connection_error(),
                "{err} leaves the connection usable"
            );
            assert!(matches!(err, Error::Imap(_)));
        }

        let login = Error::AuthenticationFailed {
            details: "invalid credentials".to_string(),
        };
        assert!(!login.is_retryable());
        assert!(!login.is_connection_error());
    }

    /// An unreadable response leaves unread bytes in the socket: the connection
    /// is lost, but a retry would only read the same bytes again.
    #[test]
    fn an_unparseable_response_poisons_the_connection_without_being_retried() {
        use async_imap::error::Error as ImapError;
        use std::io::Error as IoError;

        let nom = ImapError::Io(IoError::other(
            "Error(Error { input: [42], code: TakeWhile1 }) during parsing of \"* 1 FETCH (\"",
        ));
        let err = Error::from(nom);
        assert!(matches!(err, Error::ImapProtocolError { .. }), "{err:?}");
        assert!(err.is_connection_error());
        assert!(!err.is_retryable());

        let parse = Error::from(ImapError::Parse(async_imap::error::ParseError::Unexpected(
            "x".into(),
        )));
        assert!(matches!(parse, Error::ImapProtocolError { .. }));
        assert!(parse.is_connection_error());
    }
}
