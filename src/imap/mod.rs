//! IMAP client with CONDSTORE support for efficient synchronization.
//!
//! This module provides an IMAP client that requires CONDSTORE (RFC 7162)
//! support for tracking message modifications. Key features:
//!
//! - Batched streaming fetch for memory efficiency
//! - CHANGEDSINCE queries for incremental sync
//! - Consistency checking via HIGHESTMODSEQ comparison
//!
//! # Sync Protocol
//!
//! 1. SELECT mailbox with CONDSTORE to get HIGHESTMODSEQ snapshot
//! 2. FETCH with CHANGEDSINCE to get modified messages
//! 3. Save snapshot HIGHESTMODSEQ (not max seen) for next sync
//!
//! # Limitations
//!
//! - Does NOT detect deleted messages (requires QRESYNC or UID comparison)
//! - Does NOT support servers without CONDSTORE
//! - UIDVALIDITY changes require full resync
//! - ENABLE response not validated (trusts server capabilities, RFC 5161)

use crate::Result;
use crate::error::Error;
use crate::logger;
use crate::models::Message;
use chrono::DateTime;
use secrecy::{ExposeSecret, SecretString};
use std::time::{Duration, Instant};

use crate::mime::parse_mail;

/// IMAP capability names
const CAPABILITY_CONDSTORE: &str = "CONDSTORE";
const CAPABILITY_QRESYNC: &str = "QRESYNC";
const CAPABILITY_IDLE: &str = "IDLE";
/// UIDPLUS (RFC 4315) enables `UID EXPUNGE`, which purges only the specified UIDs.
const CAPABILITY_UIDPLUS: &str = "UIDPLUS";
/// MOVE (RFC 6851) enables `UID MOVE`, which relocates messages without expunging
/// the whole mailbox.
const CAPABILITY_MOVE: &str = "MOVE";

pub mod counted;
pub mod pool;
pub mod structure;
pub mod trace;
pub mod utf7;

use async_imap::Session as ImapSession;
use async_imap::imap_proto::types::{
    AttributeValue, Envelope, NameAttribute, Response, ResponseCode, Status, UidSetMember,
};
use async_native_tls::TlsStream;
pub use counted::{ByteCounter, Counted};
use futures::StreamExt;
pub use pool::{AccountConnectionConfig, ConnectionPool, PoolStats};
use rfc2047_decoder::{Decoder as Rfc2047Decoder, RecoverStrategy};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncReadCompatExt;
pub use trace::SyncTrace;

/// Fields requested for every message during a header sync.
///
/// BODYSTRUCTURE is deliberately absent, despite riding along for free. A
/// structure the parser cannot read — `(NIL "ALTERNATE" ...)`, which RFC 3501
/// does not permit and which this mailbox contains — fails the parse of the
/// entire FETCH response, so one such message anywhere in a folder makes the
/// whole folder unsyncable, permanently, because every later sync asks the same
/// question.
///
/// Recovering inside the sync is not possible either: abandoning a
/// half-consumed response leaves unread bytes in the socket, so a retry on the
/// same connection reads nothing and reports success. That failure cost a full
/// re-index of two archives before it was understood.
///
/// Structure is therefore left entirely to the background shaping pass, which
/// asks for it in small batches and can narrow a failing batch down to the one
/// message responsible. Sync stores headers; everything derived comes after.
///
/// The three headers beside the envelope are what threading and the
/// highlight rules need and ENVELOPE does not carry: a few hundred bytes per
/// message at most, asked for with PEEK so nothing is marked read.
const IMAP_FIELDS: &str = "(FLAGS ENVELOPE RFC822.SIZE MODSEQ";

/// [`IMAP_FIELDS`] for a server without CONDSTORE, which has no MODSEQ to
/// give and refuses to be asked for one.
const IMAP_FIELDS_PLAIN: &str = "(FLAGS ENVELOPE RFC822.SIZE";

/// The headers every sync fetches beside the envelope, and the ones the
/// highlight rules name on top; see [`set_extra_headers`].
static EXTRA_HEADERS: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

/// Name the headers the sync fetches on top of `References` and `List-Id`:
/// what the highlight rules ask about with `header:`. Process-wide, set from
/// the config at launch and at every reload, so every connection — pooled,
/// idle, backfill — asks the same question.
pub fn set_extra_headers(names: Vec<String>) {
    let mut names: Vec<String> = names
        .into_iter()
        .map(|name| name.trim().to_ascii_uppercase())
        .filter(|name| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .collect();
    names.sort();
    names.dedup();
    *EXTRA_HEADERS.write().unwrap_or_else(|e| e.into_inner()) = names;
}

/// The `HEADER.FIELDS` list of a sync fetch, closing the item list.
fn header_fields_item() -> String {
    let extra = EXTRA_HEADERS.read().unwrap_or_else(|e| e.into_inner());
    let mut names = vec!["REFERENCES".to_string(), "LIST-ID".to_string()];
    names.extend(extra.iter().cloned());
    format!(" BODY.PEEK[HEADER.FIELDS ({})])", names.join(" "))
}

/// The header names a backfill asks for: the threading ones and the extra.
pub(crate) fn backfill_header_fields() -> Vec<String> {
    let extra = EXTRA_HEADERS.read().unwrap_or_else(|e| e.into_inner());
    let mut names = vec![
        "IN-REPLY-TO".to_string(),
        "REFERENCES".to_string(),
        "LIST-ID".to_string(),
    ];
    names.extend(extra.iter().cloned());
    names
}

pub type Session = ImapSession<TlsStream<tokio_util::compat::Compat<Counted<TcpStream>>>>;

/// How long one command, or one response inside a stream, may go unanswered.
const OPERATION_TIMEOUT: Duration =
    Duration::from_secs(crate::constants::IMAP_OPERATION_TIMEOUT_SECONDS);

/// How long a liveness probe may take; see `IMAP_PROBE_TIMEOUT_SECONDS`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(crate::constants::IMAP_PROBE_TIMEOUT_SECONDS);

/// How long to listen, after a stream has ended, for the EOF that would mean
/// it ended because the connection did. See [`ImapClient::ended_cleanly`].
const STREAM_END_PROBE: Duration = Duration::from_millis(10);

/// Run one command with the operation timeout on it.
///
/// A command that times out leaves the connection with a response in flight,
/// so the error it returns is a connection error and the caller drops the
/// connection; retrying on the same socket would read the late answer as the
/// reply to the next question.
async fn bounded<T, E>(
    operation: &str,
    future: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T>
where
    E: Into<Error>,
{
    within(OPERATION_TIMEOUT, operation, future).await
}

async fn within<T, E>(
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

/// The next item of a response stream, with the operation timeout on the wait.
async fn next_bounded<S, T, E>(stream: &mut S, operation: &str) -> Result<Option<T>>
where
    S: futures::Stream<Item = std::result::Result<T, E>> + Unpin,
    E: Into<Error>,
{
    match tokio::time::timeout(OPERATION_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(item))) => Ok(Some(item)),
        Ok(Some(Err(error))) => Err(error.into()),
        Ok(None) => Ok(None),
        Err(_) => Err(Error::timeout(operation, OPERATION_TIMEOUT)),
    }
}

/// The error for a connection that ended in the middle of `operation`.
fn connection_ended(operation: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("the connection ended during {operation}, before the server had answered"),
    ))
}

/// Put the kernel's keepalive on an IMAP socket.
///
/// Without it a connection whose path has quietly gone — a NAT that forgot
/// the flow, a network that changed while the machine slept — looks exactly
/// like a quiet server until a command times out on it, sixty seconds at a
/// time. With it the kernel finds out within about two minutes of the path
/// going away and the next read fails at once.
fn set_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    use crate::constants::{
        TCP_KEEPALIVE_IDLE_SECONDS, TCP_KEEPALIVE_INTERVAL_SECONDS, TCP_KEEPALIVE_RETRIES,
    };
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(TCP_KEEPALIVE_IDLE_SECONDS))
        .with_interval(Duration::from_secs(TCP_KEEPALIVE_INTERVAL_SECONDS))
        .with_retries(TCP_KEEPALIVE_RETRIES);
    socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// How the server's certificate is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMode {
    /// Verify the certificate chain and the host name, as any client must.
    Verify,
    /// Accept any certificate. For the self-signed one a test container
    /// presents, and nothing else.
    AcceptInvalid,
}

/// A session that has been opened and logged in, before any client is made
/// of it.
pub(crate) struct OpenedSession {
    pub session: Session,
    /// The capability atoms the server advertised, as it spelled them.
    pub capabilities: Vec<String>,
    pub bytes: ByteCounter,
    pub id: u64,
}

/// Every TLS connect and LOGIN this crate makes goes through here.
///
/// One place rather than three means one log line and one ledger entry per
/// connection, with the `reason` the caller gave and the `cause` a new socket
/// was needed. Counting connections is only worth anything when nothing is
/// opened behind the count's back.
/// How to reach a server and log in: the account's connection settings with
/// the password, or the token, already obtained.
#[derive(Debug, Clone)]
pub struct ConnectionSettings {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password: SecretString,
    pub tls: TlsMode,
    pub encryption: crate::config::Encryption,
    pub auth: crate::config::AuthMethod,
}

impl ConnectionSettings {
    /// An account's settings, with the password the caller obtained.
    pub fn for_account(account: &crate::config::AccountConfig, password: SecretString) -> Self {
        Self {
            server: account.imap_server.clone(),
            port: account.imap_port,
            username: account.imap_username.clone(),
            password,
            tls: account.tls_mode(),
            encryption: account.imap_encryption,
            auth: account.imap_auth,
        }
    }

    /// TLS from the first byte, logging in with a password.
    pub fn plain(
        server: &str,
        port: u16,
        username: &str,
        password: &SecretString,
        tls: TlsMode,
    ) -> Self {
        Self {
            server: server.to_string(),
            port,
            username: username.to_string(),
            password: password.clone(),
            tls,
            encryption: crate::config::Encryption::Tls,
            auth: crate::config::AuthMethod::Password,
        }
    }
}

/// The SASL XOAUTH2 exchange (RFC 7628 as Gmail and Office 365 use it): one
/// response carrying the user and the bearer token, and an empty one to
/// whatever the server says next, which is its way of refusing.
struct Xoauth2 {
    user: String,
    token: String,
    answered: bool,
}

impl async_imap::Authenticator for Xoauth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        if self.answered {
            return String::new();
        }
        self.answered = true;
        xoauth2_response(&self.user, &self.token)
    }
}

/// The initial client response of XOAUTH2, before base64.
pub(crate) fn xoauth2_response(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

pub(crate) async fn open_session(
    settings: &ConnectionSettings,
    reason: &str,
    cause: &str,
) -> Result<OpenedSession> {
    use crate::config::{AuthMethod, Encryption};

    let id = trace::next_connection_id();
    let started = std::time::Instant::now();
    let at = std::time::SystemTime::now();
    let server = settings.server.as_str();
    let username = settings.username.as_str();
    let addr = format!("{server}:{}", settings.port);

    let attempt = async {
        let tcp_stream = TcpStream::connect(&addr).await?;
        if let Err(error) = set_keepalive(&tcp_stream) {
            logger::warn(&format!(
                "imap: #{id}: could not set TCP keepalive: {error}"
            ));
        }
        let (counted, bytes) = Counted::new(tcp_stream);

        // Certificate verification on, TLS 1.2 at least; the insecure mode
        // exists for a container's self-signed certificate and says so.
        let connector = async_native_tls::TlsConnector::new()
            .danger_accept_invalid_certs(settings.tls == TlsMode::AcceptInvalid)
            .min_protocol_version(Some(async_native_tls::Protocol::Tlsv12));
        let tls_stream = match settings.encryption {
            Encryption::Tls => connector.connect(server, counted.compat()).await?,
            Encryption::Starttls => {
                // The connection stays plain until the server agrees to
                // upgrade it, and nothing but STARTTLS is said on it before.
                let mut plain = async_imap::Client::new(counted.compat());
                plain.run_command_and_check_ok("STARTTLS", None).await?;
                connector.connect(server, plain.into_inner()).await?
            }
        };

        let client = async_imap::Client::new(tls_stream);
        let mut session = match settings.auth {
            AuthMethod::Password => client
                .login(username, settings.password.expose_secret())
                .await
                .map_err(|e| Error::AuthenticationFailed {
                    details: e.0.to_string(),
                })?,
            AuthMethod::Xoauth2 => client
                .authenticate(
                    "XOAUTH2",
                    Xoauth2 {
                        user: username.to_string(),
                        token: settings.password.expose_secret().to_string(),
                        answered: false,
                    },
                )
                .await
                .map_err(|e| Error::AuthenticationFailed {
                    details: format!("XOAUTH2: {}", e.0),
                })?,
        };

        let capabilities = session
            .capabilities()
            .await?
            .iter()
            .filter_map(|cap| match cap {
                async_imap::types::Capability::Atom(name) => Some(name.to_string()),
                _ => None,
            })
            .collect();

        Ok::<_, Error>(OpenedSession {
            session,
            capabilities,
            bytes,
            id,
        })
    };

    // One bound over the whole of connecting, the TLS handshake, LOGIN and
    // CAPABILITY: each of them is a place a half-reachable server can stall.
    let result = within(
        Duration::from_secs(crate::constants::IMAP_CONNECTION_TIMEOUT_SECONDS),
        &format!("connecting to {addr}"),
        attempt,
    )
    .await;
    let took = started.elapsed();
    match &result {
        Ok(opened) => logger::info(&format!(
            "imap: #{id} {reason} ({cause}): connected and logged in to {addr} as {username} \
             in {took:?}, {} bytes",
            opened.bytes.total()
        )),
        Err(error) => logger::warn(&format!(
            "imap: #{id} {reason} ({cause}): connecting to {addr} as {username} failed after \
             {took:?}: {error}"
        )),
    }
    trace::record_connection(trace::ConnectionRecord {
        id,
        reason: reason.to_string(),
        cause: cause.to_string(),
        server: addr,
        at,
        took,
        outcome: result.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    });
    result
}

/// The first UID we have never seen, if the server's UIDNEXT implies there is
/// one.
///
/// This exists because a MODSEQ cannot be trusted as the sole record of what
/// changed. CONDSTORE is an optimisation — "nothing has changed since X" — and
/// a server that advertises it but does not advance the counter turns an
/// optimisation into permanent, silent mail loss: `CHANGEDSINCE` keeps
/// returning nothing, so new mail is never fetched and no amount of manual
/// refreshing helps, because a manual sync takes the same path.
///
/// That is not hypothetical. One hoster reports `HIGHESTMODSEQ 85` for a mailbox
/// of 62,756 messages, and stopped moving; 3,805 messages were missing locally
/// before this check existed.
///
/// UIDNEXT is the independent signal, and it arrives in the same SELECT
/// response, so checking it costs nothing. UIDs are strictly increasing within
/// a UIDVALIDITY, so anything at or above our highest known UID plus one is
/// mail we have never had a chance to see.
pub(crate) fn uid_gap(stored_max_uid: Option<u32>, uid_next: u32) -> Option<u32> {
    // Nothing stored: the caller is doing a full fetch anyway.
    let highest_seen = stored_max_uid?;
    // UIDNEXT is the UID the *next* message will get, so mail exists above us
    // only when it is more than one past our highest.
    (uid_next > highest_seen.saturating_add(1)).then(|| highest_seen.saturating_add(1))
}

/// Flags that belong to a connection rather than to a message.
///
/// `\Recent` means "this session is the first to see this message" (RFC 3501
/// §2.3.2), so it is true once, for whoever looked first, and false forever
/// after. Storing it means every later comparison against the server reports a
/// difference that is not one — which is noise in the ordinary case and, when
/// looking for genuine flag drift, camouflage.
fn is_session_flag(flag: &str) -> bool {
    flag.eq_ignore_ascii_case("\\Recent")
}

/// Which of `held` fall inside any of the reported `ranges`.
///
/// The ranges come from a server's VANISHED response and may be enormous —
/// `1:*` covers every UID there could ever be. Walking the UIDs we hold and
/// asking which are covered is bounded by the mailbox; walking the ranges and
/// listing what is in them is not. Nor is testing every UID against every
/// range: a large archive brings tens of thousands of ranges, and that product
/// ran for most of a minute. The ranges are merged once and searched.
fn vanished_within(ranges: &[std::ops::RangeInclusive<u32>], held: &[u32]) -> Vec<u32> {
    let mut sorted: Vec<(u32, u32)> = ranges
        .iter()
        .map(|range| {
            let (a, b) = (*range.start(), *range.end());
            (a.min(b), a.max(b))
        })
        .collect();
    sorted.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(sorted.len());
    for (start, end) in sorted {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    held.iter()
        .copied()
        .filter(|&uid| {
            let after = merged.partition_point(|&(start, _)| start <= uid);
            after > 0 && merged[after - 1].1 >= uid
        })
        .collect()
}

/// Decode a header value that may contain RFC 2047 encoded-words.
///
/// Liberal on purpose, in both directions:
///
/// - Over-long encoded-words are decoded rather than rejected. RFC 2047 §2
///   limits an encoded-word to 75 characters, but that is a rule for whoever
///   *writes* the header; a reader that enforces it just loses the text. Real
///   senders exceed it — a vendor's bug-tracker notification arrives as a single
///   110-character encoded-word — and every other mail client shows it fine.
/// - Anything still undecodable falls back to the raw bytes, lossily.
///
/// The fallback is the important half. This previously ended in `.ok()`, so a
/// decode failure silently discarded the whole header and the message showed as
/// "(no subject)" — strictly worse than showing the undecoded text, because it
/// destroys information the user could otherwise read straight through.
fn decode_header(bytes: &[u8]) -> String {
    Rfc2047Decoder::new()
        .too_long_encoded_word_strategy(RecoverStrategy::Decode)
        .decode(bytes)
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned())
}

/// The UID in a FETCH response, when the server included one.
fn attribute_uid(attributes: &[AttributeValue<'_>]) -> Option<u32> {
    attributes.iter().find_map(|attribute| match attribute {
        AttributeValue::Uid(uid) => Some(*uid),
        _ => None,
    })
}

fn attribute_envelope<'a>(attributes: &'a [AttributeValue<'_>]) -> Option<&'a Envelope<'a>> {
    attributes.iter().find_map(|attribute| match attribute {
        AttributeValue::Envelope(envelope) => Some(&**envelope),
        _ => None,
    })
}

/// The flags in a FETCH response, in wire form and without the session flags.
fn attribute_flags(attributes: &[AttributeValue<'_>]) -> Vec<String> {
    attributes
        .iter()
        .filter_map(|attribute| match attribute {
            AttributeValue::Flags(raw) => Some(raw.iter()),
            _ => None,
        })
        .flatten()
        .map(|raw| ImapClient::flag_to_wire(&async_imap::types::Flag::from(raw.as_ref())))
        .filter(|flag| !is_session_flag(flag))
        .collect()
}

/// The whole message, when `BODY[]` or `RFC822` was asked for.
fn attribute_body<'a>(attributes: &'a [AttributeValue<'_>]) -> Option<&'a [u8]> {
    attributes.iter().find_map(|attribute| match attribute {
        AttributeValue::BodySection {
            section: None,
            data: Some(body),
            ..
        }
        | AttributeValue::Rfc822(Some(body)) => Some(body.as_ref()),
        _ => None,
    })
}

/// The header block, when `BODY.PEEK[HEADER.FIELDS (...)]` was asked for.
fn attribute_header_block<'a>(attributes: &'a [AttributeValue<'_>]) -> Option<&'a [u8]> {
    use async_imap::imap_proto::{MessageSection, SectionPath};
    attributes.iter().find_map(|attribute| match attribute {
        AttributeValue::BodySection {
            section: Some(SectionPath::Full(MessageSection::Header)),
            data: Some(block),
            ..
        } => Some(block.as_ref()),
        _ => None,
    })
}

/// A message as a FETCH response describes it. `None` without a UID, which
/// the sync cannot do anything with.
fn message_from_attributes(folder_id: i64, attributes: &[AttributeValue<'_>]) -> Option<Message> {
    let uid = attribute_uid(attributes)?;
    let flags = attribute_flags(attributes);
    // The threading headers: the block the sync asked for, with In-Reply-To
    // from the envelope, which carries it already.
    let headers = attribute_header_block(attributes).map(|block| {
        let mut headers = crate::threads::ThreadHeaders::parse(block);
        headers.in_reply_to = attribute_envelope(attributes)
            .and_then(|envelope| envelope.in_reply_to.as_ref())
            .map(|bytes| {
                String::from_utf8_lossy(bytes)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|value| !value.is_empty())
            .or(headers.in_reply_to);
        headers
    });

    let shape = attributes.iter().find_map(|attribute| match attribute {
        AttributeValue::BodyStructure(structure) => Some(structure::shape_of(structure)),
        _ => None,
    });

    let addresses = |list: Option<&Vec<async_imap::imap_proto::Address<'_>>>| -> Option<String> {
        let formatted: Vec<String> = list?
            .iter()
            .filter_map(|address| {
                ImapClient::format_email_address(
                    address.name.as_deref(),
                    address.mailbox.as_deref(),
                    address.host.as_deref(),
                )
            })
            .collect();
        (!formatted.is_empty()).then(|| formatted.join(", "))
    };

    let (subject, from, to, cc, date, message_id) = match attribute_envelope(attributes) {
        Some(envelope) => (
            envelope.subject.as_ref().map(|bytes| decode_header(bytes)),
            envelope
                .from
                .as_ref()
                .and_then(|list| list.first())
                .and_then(|address| {
                    ImapClient::format_email_address(
                        address.name.as_deref(),
                        address.mailbox.as_deref(),
                        address.host.as_deref(),
                    )
                }),
            addresses(envelope.to.as_ref()),
            addresses(envelope.cc.as_ref()),
            envelope
                .date
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
                .and_then(|text| ImapClient::normalize_date(&text)),
            envelope
                .message_id
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok()),
        ),
        None => (None, None, None, None, None, None),
    };

    let size = attributes.iter().find_map(|attribute| match attribute {
        AttributeValue::Rfc822Size(size) => Some(*size),
        _ => None,
    });
    let modseq = attributes.iter().find_map(|attribute| match attribute {
        AttributeValue::ModSeq(modseq) => Some(*modseq),
        _ => None,
    });
    let body = attribute_body(attributes);

    Some(Message {
        id: None,
        folder_id,
        uid,
        message_id,
        subject,
        from,
        to,
        cc,
        date,
        body_text: body.and_then(extract_body_text),
        body_html: body.and_then(extract_body_html),
        rfc822: body.map(|bytes| bytes.to_vec()),
        size,
        flags,
        modseq,
        shape,
        headers,
    })
}

/// Mailbox metadata from SELECT command with CONDSTORE
#[derive(Debug, Clone)]
pub struct MailboxInfo {
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub highest_modseq: Option<u64>,
    pub exists: u32,
}

/// IMAP client that requires CONDSTORE support
pub struct ImapClient {
    session: Session,
    current_mailbox: Option<MailboxInfo>,
    /// Whether the server has CONDSTORE (RFC 7162); without it there is no
    /// MODSEQ to ask about, and a sync lists the folder's UIDs instead.
    has_condstore: bool,
    has_qresync: bool,
    has_idle: bool,
    /// Whether the server advertises UIDPLUS (RFC 4315), enabling `UID EXPUNGE`.
    has_uidplus: bool,
    /// Whether the server advertises MOVE (RFC 6851), enabling `UID MOVE`.
    has_move: bool,
    /// The connection's number in the ledger, which the trace and the log
    /// call it by.
    id: u64,
    bytes: ByteCounter,
    opened: Instant,
    /// The mailbox SELECT last succeeded on, which the server answers STATUS
    /// for from this session's own view rather than from the mailbox.
    selected: Option<String>,
}

/// Strategy for purging `\Deleted` messages after a delete/move operation.
///
/// A plain `EXPUNGE` removes *every* message flagged `\Deleted` in the mailbox,
/// including ones flagged by another client or the user. To avoid destroying
/// unrelated mail we pick a strategy based on server capabilities and mailbox state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpungeStrategy {
    /// Server supports UIDPLUS: purge exactly the affected UIDs via `UID EXPUNGE`.
    UidExpunge,
    /// No UIDPLUS, but no *other* `\Deleted` messages exist, so a plain `EXPUNGE`
    /// only removes the intended messages and is therefore safe.
    PlainExpunge,
    /// No UIDPLUS *and* other `\Deleted` messages exist: a plain `EXPUNGE` would
    /// destroy unrelated mail, so skip purging. The intended messages remain
    /// flagged `\Deleted` and will be removed by a later full expunge/CLOSE.
    SkipUnsafe,
}

/// Decide how to purge the affected UIDs without destroying unrelated `\Deleted`
/// mail. Pure decision logic, separated out so it can be unit-tested without a
/// live IMAP server.
fn decide_expunge_strategy(has_uidplus: bool, other_deleted_exists: bool) -> ExpungeStrategy {
    if has_uidplus {
        ExpungeStrategy::UidExpunge
    } else if other_deleted_exists {
        ExpungeStrategy::SkipUnsafe
    } else {
        ExpungeStrategy::PlainExpunge
    }
}

impl ImapClient {
    /// Returns a reference to the currently selected mailbox information
    pub fn current_mailbox(&self) -> Option<&MailboxInfo> {
        self.current_mailbox.as_ref()
    }

    /// Returns whether the server supports IDLE (RFC 2177)
    pub fn has_idle(&self) -> bool {
        self.has_idle
    }

    /// Returns a mutable reference to the underlying session for IDLE operations
    ///
    /// IDLE requires exclusive access to the session, so this exposes the session
    /// for use with async_imap's idle() method.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Connect to the server and log in, verifying its certificate.
    ///
    /// `reason` says who wants the connection, in their words ("sync",
    /// "mark read", "open message"); it is logged with the connection and
    /// tallied in [`trace::report`], which is how the number of connections
    /// an hour of use costs can be read off.
    ///
    /// Returns an error if the server does not support CONDSTORE.
    pub async fn connect(
        server: &str,
        port: u16,
        username: &str,
        password: &SecretString,
        reason: &str,
    ) -> Result<Self> {
        Self::open(
            &ConnectionSettings::plain(server, port, username, password, TlsMode::Verify),
            reason,
            "direct",
        )
        .await
    }

    /// Connect over TLS but accept invalid and self-signed certificates.
    /// **Only for test containers.**
    ///
    /// Deliberately not `#[cfg(test)]`. The container-backed suites in `tests/`
    /// are separate crates and cannot see anything gated on it, and the task
    /// sync reliability tests (spec §14.1) have to drive a real `ImapClient`
    /// against Dovecot — a test double would prove nothing about APPEND, UID
    /// SEARCH, UID EXPUNGE or UIDVALIDITY, which is the entire point of them.
    /// Hidden from the docs so it is not mistaken for a supported way to
    /// connect.
    #[doc(hidden)]
    pub async fn connect_insecure(
        server: &str,
        port: u16,
        username: &str,
        password: &SecretString,
        reason: &str,
    ) -> Result<Self> {
        Self::open(
            &ConnectionSettings::plain(server, port, username, password, TlsMode::AcceptInvalid),
            reason,
            "direct",
        )
        .await
    }

    /// Connect and log in as `settings` say: the way an account's connection
    /// is configured, encryption and login method included.
    pub async fn connect_with(settings: &ConnectionSettings, reason: &str) -> Result<Self> {
        Self::open(settings, reason, "direct").await
    }

    /// Open a session and make a client of it.
    ///
    /// `cause` is why a new socket is being opened rather than an existing one
    /// reused; the pool says "no pooled connection", "stale" or "NOOP failed",
    /// everything else says "direct".
    pub(crate) async fn open(
        settings: &ConnectionSettings,
        reason: &str,
        cause: &str,
    ) -> Result<Self> {
        let opened = open_session(settings, reason, cause).await?;
        let OpenedSession {
            mut session,
            capabilities,
            bytes,
            id,
        } = opened;

        let has = |name: &str| capabilities.iter().any(|cap| cap == name);

        // CONDSTORE makes a sync of an unchanged folder one round trip;
        // without it (Exchange, some hosted servers) every sync lists the
        // folder's UIDs, which is slower but just as right. QRESYNC extends
        // CONDSTORE with deletion tracking; IDLE gives push; UIDPLUS and
        // MOVE let a purge or a move be scoped to exact UIDs.
        let has_condstore = has(CAPABILITY_CONDSTORE);
        let has_qresync = has_condstore && has(CAPABILITY_QRESYNC);
        let has_idle = has(CAPABILITY_IDLE);
        let has_uidplus = has(CAPABILITY_UIDPLUS);
        let has_move = has(CAPABILITY_MOVE);
        if !has_condstore {
            logger::info(&format!(
                "imap: #{id}: {} has no CONDSTORE; syncing by listing UIDs",
                settings.server
            ));
        }

        // Enable QRESYNC if available (implies CONDSTORE), otherwise just
        // CONDSTORE. The ENABLED response is not validated (RFC 5161): the
        // async_imap crate's run_command() doesn't expose it.
        if has_condstore {
            let extension = if has_qresync {
                CAPABILITY_QRESYNC
            } else {
                CAPABILITY_CONDSTORE
            };
            if let Err(e) = bounded(
                "ENABLE",
                session.run_command_and_check_ok(&format!("ENABLE {}", extension)),
            )
            .await
            {
                let _ = session.logout().await;
                return Err(e);
            }
        }

        Ok(ImapClient {
            session,
            current_mailbox: None,
            has_condstore,
            has_qresync,
            has_idle,
            has_uidplus,
            has_move,
            id,
            bytes,
            opened: Instant::now(),
            selected: None,
        })
    }

    /// The number this connection was given when it was opened; what the
    /// trace and the log call it.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Bytes this connection has moved so far, both directions, TLS included.
    pub fn bytes_on_wire(&self) -> u64 {
        self.bytes.total()
    }

    /// How long ago the connection was opened.
    pub fn age(&self) -> Duration {
        self.opened.elapsed()
    }

    /// Say goodbye, briefly. A server that will not answer LOGOUT within the
    /// probe timeout is not worth waiting for; the socket closes with the
    /// client either way.
    pub async fn disconnect(&mut self) -> Result<()> {
        within(PROBE_TIMEOUT, "LOGOUT", self.session.logout()).await
    }

    /// The folders of the account that can be selected: `LIST "" "*"` without
    /// its `\Noselect` entries.
    ///
    /// A server lists hierarchy nodes that hold no mail of their own —
    /// `[Gmail]`, a Maildir++ parent whose children exist but which never was
    /// created itself — and marks them `\Noselect` (RFC 3501 §7.2.2). They
    /// are not mailboxes: SELECT on one is refused, so a sync that treats them
    /// as folders fails on every pass, and a search that walks the account
    /// trips over them on every folder.
    pub async fn list_folders(&mut self) -> Result<Vec<String>> {
        Ok(self
            .list_folders_detailed()
            .await?
            .into_iter()
            .map(|folder| folder.name)
            .collect())
    }

    /// Every selectable mailbox, with the role the server gives it.
    pub async fn list_folders_detailed(&mut self) -> Result<Vec<ListedFolder>> {
        let mut folders = Vec::new();
        {
            let list_stream = bounded("LIST", self.session.list(Some(""), Some("*"))).await?;
            futures::pin_mut!(list_stream);
            while let Some(name) = next_bounded(&mut list_stream, "LIST").await? {
                if is_selectable(name.attributes()) {
                    folders.push(ListedFolder {
                        name: name.name().to_string(),
                        special_use: special_use(name.attributes()),
                        delimiter: name.delimiter().map(str::to_string),
                    });
                }
            }
        }
        self.ended_cleanly("LIST").await?;
        Ok(folders)
    }

    /// Create a new folder
    pub async fn create_folder(&mut self, name: &str) -> Result<()> {
        bounded("CREATE", self.session.create(name)).await
    }

    /// Give a folder another name; its messages come with it.
    pub async fn rename_folder(&mut self, from: &str, to: &str) -> Result<()> {
        bounded("RENAME", self.session.rename(from, to)).await
    }

    /// Delete a folder, with every message in it.
    pub async fn delete_folder(&mut self, name: &str) -> Result<()> {
        bounded("DELETE", self.session.delete(name)).await
    }

    /// Mark a folder subscribed, or not, for the clients that list only those.
    pub async fn set_subscribed(&mut self, name: &str, subscribed: bool) -> Result<()> {
        if subscribed {
            bounded("SUBSCRIBE", self.session.subscribe(name)).await
        } else {
            bounded("UNSUBSCRIBE", self.session.unsubscribe(name)).await
        }
    }

    /// The character the server separates hierarchy levels with, and whether it
    /// has one at all.
    ///
    /// `LIST "" ""` is the RFC 3501 §6.3.8 special case: it answers with the
    /// delimiter and the root, without listing a single mailbox. That is the
    /// portable half of "resolve a folder name against the server's namespace".
    ///
    /// The other half would be `NAMESPACE` (RFC 2342), and it is deliberately
    /// not used: neither async-imap 0.11 nor imap-proto 0.16 knows the command
    /// or its response, so issuing it raw would leave a response in the socket
    /// that the parser cannot read — and a half-consumed response makes every
    /// later command on the connection read the wrong bytes and quietly return
    /// nothing. That failure mode has already cost this codebase two archives
    /// (see [`IMAP_FIELDS`]). The delimiter plus the existing folder list is
    /// enough to tell `Tasks` from `INBOX.Tasks`, which is all the namespace was
    /// wanted for.
    pub async fn hierarchy_delimiter(&mut self) -> Result<Option<String>> {
        let mut delimiter = None;
        {
            let list_stream = bounded("LIST", self.session.list(Some(""), Some(""))).await?;
            futures::pin_mut!(list_stream);
            while let Some(name) = next_bounded(&mut list_stream, "LIST").await? {
                if delimiter.is_none() {
                    delimiter = name.delimiter().map(|d| d.to_string());
                }
            }
        }
        self.ended_cleanly("LIST").await?;
        Ok(delimiter)
    }

    pub async fn select_mailbox(&mut self, name: &str) -> Result<()> {
        // A SELECT that fails leaves no mailbox selected (RFC 3501 §6.3.1).
        self.selected = None;
        let mailbox = if self.has_condstore {
            bounded("SELECT", self.session.select_condstore(name)).await?
        } else {
            bounded("SELECT", self.session.select(name)).await?
        };
        self.current_mailbox = Some(MailboxInfo {
            uid_validity: mailbox.uid_validity,
            uid_next: mailbox.uid_next,
            highest_modseq: mailbox.highest_modseq,
            exists: mailbox.exists,
        });
        self.selected = Some(name.to_string());
        Ok(())
    }

    /// The mailbox this connection has selected, if any.
    pub fn selected_mailbox(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Ask what state a mailbox is in, without selecting it.
    ///
    /// One round trip, against the two that SELECT and FETCH cost, and it does
    /// not disturb whatever mailbox is currently selected. Enough to decide
    /// whether a folder is worth syncing at all: most folders have not changed
    /// since the last sync, and asking each of them to open and describe itself
    /// is where the time in a full sync goes.
    ///
    /// For the mailbox this very connection has selected the answer comes
    /// from the session's own view, which Dovecot brings up to date only on a
    /// NOOP; without one a sync of the folder that was synced a moment ago is
    /// told nothing has changed, whatever has. So that case costs a NOOP first.
    pub async fn folder_state(&mut self, mailbox: &str) -> Result<FolderState> {
        if self.selected.as_deref() == Some(mailbox) {
            self.noop().await?;
        }
        let items = if self.has_condstore {
            "(MESSAGES UIDNEXT UIDVALIDITY HIGHESTMODSEQ)"
        } else {
            "(MESSAGES UIDNEXT UIDVALIDITY)"
        };
        let mailbox_info = bounded("STATUS", self.session.status(mailbox, items)).await?;
        Ok(FolderState {
            messages: mailbox_info.exists,
            uid_next: mailbox_info.uid_next,
            uid_validity: mailbox_info.uid_validity,
            highest_modseq: mailbox_info.highest_modseq,
        })
    }

    /// Everything the server has to say in answer to one command, up to and
    /// including its tagged response, which `on_response` sees last: a COPY's
    /// COPYUID travels on it.
    ///
    /// The stream API hides how a response ended: a FETCH stream ends the same
    /// way whether the tagged OK arrived, the server said NO, or the connection
    /// was cut, so a sync that received a third of a folder stored it as the
    /// whole folder and recorded the folder as up to date. Reading the
    /// responses one at a time, with the operation timeout on every wait,
    /// makes the three ends three outcomes: `Ok`, [`Error::Imap`] and a
    /// connection error.
    async fn run_until_done(
        &mut self,
        command: &str,
        operation: &str,
        mut on_response: impl FnMut(&Response<'_>),
    ) -> Result<()> {
        // Whatever arrived before this command is not an answer to it.
        while self.session.unsolicited_responses.try_recv().is_ok() {}

        let tag = bounded(operation, self.session.run_command(command)).await?;
        loop {
            let response = within(OPERATION_TIMEOUT, operation, self.session.read_response())
                .await?
                .ok_or_else(|| connection_ended(operation))?;
            match response.parsed() {
                Response::Done {
                    tag: done,
                    status,
                    code,
                    information,
                } if *done == tag => {
                    return match status {
                        Status::Ok => {
                            on_response(response.parsed());
                            Ok(())
                        }
                        status => Err(Error::Imap(format!(
                            "{operation} answered {status:?}: {}{}",
                            information.as_deref().unwrap_or(""),
                            code.as_ref()
                                .map(|code| format!(" [{code:?}]"))
                                .unwrap_or_default()
                        ))),
                    };
                }
                // A tagged response to another command cannot arrive: the
                // client never has two in flight.
                Response::Done { .. } => {}
                other => on_response(other),
            }
        }
    }

    /// Whether the stream that just ended did so because the server finished
    /// rather than because the connection did.
    ///
    /// The stream API ends a FETCH stream on the tagged response and on EOF
    /// alike, and says which in neither case. They are told apart by looking
    /// for more: after a tagged response the server says nothing until the
    /// next command, so a short wait finds nothing; after EOF the stream ends
    /// again at once. An untagged response that happens to arrive in the
    /// window is not an answer to anything and is dropped.
    async fn ended_cleanly(&mut self, operation: &str) -> Result<()> {
        match tokio::time::timeout(STREAM_END_PROBE, self.session.read_response()).await {
            Err(_) | Ok(Ok(Some(_))) => Ok(()),
            Ok(Ok(None)) => Err(connection_ended(operation)),
            Ok(Err(error)) => Err(Error::Io(error)),
        }
    }

    /// One `UID FETCH` of the sync fields, read to its tagged response.
    ///
    /// `progress` is told how many messages have arrived so far, every ten.
    async fn fetch_sync_fields(
        &mut self,
        folder_id: i64,
        uid_set: &str,
        changed_since: Option<u64>,
        mut progress: Option<&mut (dyn FnMut(usize) + Send)>,
    ) -> Result<Vec<Message>> {
        let headers = header_fields_item();
        let command = match (changed_since, self.has_condstore) {
            (Some(modseq), true) => {
                format!("UID FETCH {uid_set} {IMAP_FIELDS}{headers} (CHANGEDSINCE {modseq})")
            }
            (Some(_), false) => {
                return Err(Error::Sync(
                    "CHANGEDSINCE asked of a server without CONDSTORE".to_string(),
                ));
            }
            (None, true) => format!("UID FETCH {uid_set} {IMAP_FIELDS}{headers}"),
            (None, false) => format!("UID FETCH {uid_set} {IMAP_FIELDS_PLAIN}{headers}"),
        };
        let mut messages = Vec::new();
        self.run_until_done(&command, "FETCH", |response| {
            if let Response::Fetch(_, attributes) = response
                && let Some(message) = message_from_attributes(folder_id, attributes)
            {
                messages.push(message);
                if let Some(progress) = progress.as_deref_mut()
                    && messages.len().is_multiple_of(10)
                {
                    progress(messages.len());
                }
            }
        })
        .await?;
        Ok(messages)
    }

    /// The messages whose MODSEQ moved past `modseq`: new mail, and mail whose
    /// flags changed. The mailbox must be selected.
    ///
    /// Deleted messages are not reported: CHANGEDSINCE says what changed, not
    /// what went. See [`Self::fetch_vanished_uids`] and [`Self::fetch_all_uids`]
    /// for that.
    pub async fn fetch_changed_since(
        &mut self,
        folder_id: i64,
        modseq: u64,
        progress: Option<&mut (dyn FnMut(usize) + Send)>,
    ) -> Result<Vec<Message>> {
        self.fetch_sync_fields(folder_id, "1:*", Some(modseq), progress)
            .await
    }

    /// Fetch specific UIDs, with the sync fields.
    ///
    /// The mailbox must be selected. UIDs the server no longer has are simply
    /// absent from the result: a message can go between the listing that named
    /// it and this fetch, and that is not an error.
    pub async fn fetch_uids(&mut self, folder_id: i64, uids: &[u32]) -> Result<Vec<Message>> {
        self.fetch_uids_with_progress(folder_id, uids, None).await
    }

    /// [`Self::fetch_uids`], telling `progress` how many have arrived so far.
    pub async fn fetch_uids_with_progress(
        &mut self,
        folder_id: i64,
        uids: &[u32],
        progress: Option<&mut (dyn FnMut(usize) + Send)>,
    ) -> Result<Vec<Message>> {
        let mut progress = progress;
        let mut messages = Vec::new();
        for set in uid_sets(uids) {
            let before = messages.len();
            let fetched = match progress.as_deref_mut() {
                Some(report) => {
                    let mut overall = |n: usize| report(before + n);
                    self.fetch_sync_fields(folder_id, &set, None, Some(&mut overall))
                        .await?
                }
                None => self.fetch_sync_fields(folder_id, &set, None, None).await?,
            };
            messages.extend(fetched);
        }
        Ok(messages)
    }

    /// Every message from `first` to the end of the mailbox.
    ///
    /// RFC 3501 §6.4.8: a range `X:*` always includes the last message, even
    /// when X is above it, so the result can hold one message the caller
    /// already has.
    pub async fn fetch_from_uid(&mut self, folder_id: i64, first: u32) -> Result<Vec<Message>> {
        self.fetch_sync_fields(folder_id, &format!("{first}:*"), None, None)
            .await
    }

    /// Fetch just the addresses of several messages.
    ///
    /// ENVELOPE only, so no body and no structure. Returns
    /// `(uid, from, to, cc)` for each message the server describes.
    pub async fn fetch_addresses(
        &mut self,
        uids: &[u32],
    ) -> Result<Vec<(u32, Option<String>, Option<String>, Option<String>)>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let join = |addrs: Option<&Vec<async_imap::imap_proto::Address<'_>>>| -> Option<String> {
            let list: Vec<String> = addrs?
                .iter()
                .filter_map(|a| {
                    Self::format_email_address(
                        a.name.as_deref(),
                        a.mailbox.as_deref(),
                        a.host.as_deref(),
                    )
                })
                .collect();
            (!list.is_empty()).then(|| list.join(", "))
        };

        let mut out = Vec::new();
        for set in uid_sets(uids) {
            let command = format!("UID FETCH {set} (ENVELOPE)");
            self.run_until_done(&command, "FETCH", |response| {
                let Response::Fetch(_, attributes) = response else {
                    return;
                };
                let (Some(uid), Some(envelope)) =
                    (attribute_uid(attributes), attribute_envelope(attributes))
                else {
                    return;
                };
                out.push((
                    uid,
                    join(envelope.from.as_ref()).or_else(|| join(envelope.sender.as_ref())),
                    join(envelope.to.as_ref()),
                    join(envelope.cc.as_ref()),
                ));
            })
            .await?;
        }
        Ok(out)
    }

    /// Format an address from the envelope as `Name <mailbox@host>`.
    ///
    /// The display name was previously dropped, so every stored address was
    /// bare: the list had nothing to show above the address, the reader showed
    /// only the address, and `f:` could not match a sender by name — searching
    /// for a person only worked if their name happened to appear in their
    /// address.
    ///
    /// The name is an encoded-word as often as not, and is decoded here for the
    /// same reason the subject is: what is stored should be what is read.
    fn format_email_address(
        name: Option<&[u8]>,
        mailbox: Option<&[u8]>,
        host: Option<&[u8]>,
    ) -> Option<String> {
        let mailbox_str = String::from_utf8(mailbox?.to_vec()).ok()?;
        // A NIL host is not a reason to drop the address. RFC 3501 §7.4.2 uses
        // one to mark the start of an address group, and a server sends one for
        // an address with no domain — which is what a draft holds while it is
        // still being typed. Discarding it meant reopening a draft lost the
        // half-written recipient. The group terminator has a NIL mailbox as
        // well and is still dropped, by the line above.
        let address = match host.and_then(|h| String::from_utf8(h.to_vec()).ok()) {
            Some(host_str) if !host_str.is_empty() => format!("{mailbox_str}@{host_str}"),
            _ => mailbox_str,
        };

        match name.map(decode_header) {
            // A name that merely repeats the address adds nothing but width.
            Some(name) if !name.trim().is_empty() && name.trim() != address => {
                Some(format!("{} <{address}>", name.trim()))
            }
            _ => Some(address),
        }
    }

    /// Normalize date string to RFC3339/ISO8601 format for consistent storage and sorting
    /// Uses mailparse::dateparse which handles real-world email date quirks
    fn normalize_date(date_str: &str) -> Option<String> {
        use chrono::{TimeZone, Utc};

        // Try parsing as RFC 3339/ISO 8601 (already normalized)
        if let Ok(dt) = DateTime::parse_from_rfc3339(date_str) {
            return Some(dt.to_rfc3339());
        }

        // Use mailparse's dateparse which handles email date quirks
        // (wrong day-of-week, timezone names like GMT/PST, parenthesized comments, etc.)
        if let Ok(timestamp) = mailparse::dateparse(date_str) {
            let dt = Utc.timestamp_opt(timestamp, 0).single()?;
            return Some(dt.to_rfc3339());
        }

        None
    }

    /// Serialize an IMAP flag to its wire representation (RFC 3501 §2.3.2).
    ///
    /// System flags keep their leading backslash; a custom keyword is stored
    /// verbatim. This previously used `format!("{:?}", flag)`, which emitted
    /// Rust debug output — `Seen`, `Flagged`, `Custom("Old")` — and so never
    /// matched the `\Seen` / `\Flagged` that the rest of the codebase compares
    /// against. The visible effect was every message reporting as unread and
    /// never flagged.
    fn flag_to_wire(flag: &async_imap::types::Flag<'_>) -> String {
        use async_imap::types::Flag;
        match flag {
            Flag::Seen => "\\Seen".to_string(),
            Flag::Answered => "\\Answered".to_string(),
            Flag::Flagged => "\\Flagged".to_string(),
            Flag::Deleted => "\\Deleted".to_string(),
            Flag::Draft => "\\Draft".to_string(),
            Flag::Recent => "\\Recent".to_string(),
            Flag::MayCreate => "\\*".to_string(),
            Flag::Custom(name) => name.to_string(),
        }
    }

    /// Check if server supports QRESYNC extension
    pub fn supports_qresync(&self) -> bool {
        self.has_qresync
    }

    /// Whether the server has CONDSTORE, and so a MODSEQ to sync by.
    pub fn supports_condstore(&self) -> bool {
        self.has_condstore
    }

    /// Behave as if the server had no CONDSTORE, for a test of that path
    /// against a server that has it.
    #[doc(hidden)]
    pub fn pretend_no_condstore(&mut self) {
        self.has_condstore = false;
        self.has_qresync = false;
    }

    /// Fetch vanished UIDs using QRESYNC
    ///
    /// Which of `of_interest` the server says have been expunged since `modseq`.
    ///
    /// Takes the UIDs to ask about rather than returning everything the server
    /// reports, because what it reports can be `1:*` — every UID that could
    /// ever have existed. Expanding that into a list produced two billion
    /// entries, eight gigabytes and a twenty-second stall, for an answer that
    /// was then thrown away for being implausible. A mailbox emptied elsewhere
    /// therefore never got cleaned up here.
    ///
    /// Only UIDs actually held can be deleted, so intersecting here rather than
    /// afterwards costs nothing and bounds the work by what is stored.
    pub async fn fetch_vanished_uids(
        &mut self,
        _uidvalidity: u32,
        modseq: u64,
        of_interest: &[u32],
    ) -> Result<Vec<u32>> {
        if !self.has_qresync {
            return Err(Error::Imap(
                "Server does not support QRESYNC - cannot fetch vanished UIDs".into(),
            ));
        }

        // UID FETCH with the VANISHED modifier (RFC 7162): the server answers
        // with VANISHED responses before the tagged OK. The ranges are kept as
        // the server states them: `1:*` is one range and four billion UIDs.
        let command = format!("UID FETCH 1:* (UID) (CHANGEDSINCE {modseq} VANISHED)");
        let mut vanished: Vec<std::ops::RangeInclusive<u32>> = Vec::new();
        self.run_until_done(&command, "FETCH VANISHED", |response| {
            if let Response::Vanished { uids, .. } = response {
                vanished.extend(uids.iter().cloned());
            }
        })
        .await?;

        let expunged = vanished_within(&vanished, of_interest);
        logger::debug(&format!(
            "QRESYNC VANISHED: {} of {} stored UIDs expunged since modseq {} \
             ({} range(s) reported)",
            expunged.len(),
            of_interest.len(),
            modseq,
            vanished.len()
        ));

        Ok(expunged)
    }

    /// The flags the server currently holds for these UIDs.
    ///
    /// Flags are normally learned from CONDSTORE: ask for what changed since a
    /// modseq and the server says. That depends on the server advancing its
    /// HIGHESTMODSEQ when a flag changes, and not every server does — one here
    /// advertises CONDSTORE and leaves the value where it is, so a message read
    /// in another client stays unread in this one indefinitely, with nothing
    /// anywhere reporting a problem.
    ///
    /// This asks outright instead. It is the sync every client did before
    /// CONDSTORE existed, which is why it is affordable: FLAGS is a few bytes
    /// per message and no body is touched.
    pub async fn fetch_flags(
        &mut self,
        uids: &[u32],
    ) -> Result<std::collections::HashMap<u32, Vec<String>>> {
        let mut found = std::collections::HashMap::new();
        if uids.is_empty() {
            return Ok(found);
        }

        for set in uid_sets(uids) {
            let command = format!("UID FETCH {set} (FLAGS)");
            self.run_until_done(&command, "FETCH FLAGS", |response| {
                if let Response::Fetch(_, attributes) = response
                    && let Some(uid) = attribute_uid(attributes)
                {
                    let mut flags = attribute_flags(attributes);
                    flags.sort();
                    found.insert(uid, flags);
                }
            })
            .await?;
        }

        Ok(found)
    }

    /// Every UID in the selected mailbox, read to the tagged response.
    ///
    /// The fallback for deletion detection on a server without QRESYNC, and
    /// the listing a first sync is paged over. Complete by construction: a
    /// connection cut partway through is an error, not a shorter list — a
    /// shorter list taken for the whole mailbox would delete locally whatever
    /// it did not reach.
    pub async fn fetch_all_uids(&mut self) -> Result<Vec<u32>> {
        let mut uids = Vec::new();
        self.run_until_done("UID FETCH 1:* (UID)", "FETCH UID", |response| {
            if let Response::Fetch(_, attributes) = response
                && let Some(uid) = attribute_uid(attributes)
            {
                uids.push(uid);
            }
        })
        .await?;
        Ok(uids)
    }

    /// List a UID range with nothing but a few named header fields.
    ///
    /// The cheapest complete answer to "what is in this folder": one round
    /// trip, a few dozen bytes per message, no bodies and no `\Seen` (PEEK
    /// throughout). Small folders whose contents must be reconciled exactly —
    /// the task folder — can afford to ask this on every sync, and that is the
    /// point.
    ///
    /// It exists because a MODSEQ delta cannot be trusted as the record of what
    /// changed. One server here reports the same `HIGHESTMODSEQ` forever
    /// ([`uid_gap`] documents the mailbox it was found on), so a sync that only
    /// asked "what changed since X" would freeze on that account and report
    /// success while doing so. Listing everything cannot freeze: what the
    /// server sends *is* the current state, and anything absent from it is
    /// absent from the folder.
    ///
    /// `HEADER.FIELDS` is a request for named headers, and the reply carries
    /// them as one header block per message — parse it with
    /// `mailparse::parse_headers` or an equivalent.
    pub async fn fetch_header_listing(
        &mut self,
        uid_set: &str,
        fields: &[&str],
    ) -> Result<Vec<HeaderListing>> {
        if uid_set.is_empty() || fields.is_empty() {
            return Ok(Vec::new());
        }

        let query = format!(
            "(UID FLAGS BODY.PEEK[HEADER.FIELDS ({})])",
            fields.join(" ")
        );
        let mut listed = Vec::new();
        {
            let stream = bounded("FETCH", self.session.uid_fetch(uid_set, &query)).await?;
            futures::pin_mut!(stream);
            while let Some(fetch) = next_bounded(&mut stream, "FETCH").await? {
                let Some(uid) = fetch.uid else { continue };
                let flags: Vec<String> = fetch
                    .flags()
                    .map(|flag| Self::flag_to_wire(&flag))
                    .filter(|flag| !is_session_flag(flag))
                    .collect();
                listed.push(HeaderListing {
                    uid,
                    flags,
                    // A message with none of the requested headers still
                    // answers, with an empty section. It is kept rather than
                    // dropped: the caller is reconciling a folder and needs to
                    // know the UID exists, whatever is in it.
                    headers: fetch.header().map(|h| h.to_vec()).unwrap_or_default(),
                });
            }
        }
        // A listing is what a folder is reconciled against, so a listing cut
        // short must be an error rather than a shorter folder.
        self.ended_cleanly("FETCH").await?;

        Ok(listed)
    }

    /// Fetch whole messages for several UIDs in one round trip, without
    /// marking any of them read.
    ///
    /// [`Self::fetch_message_body`] answers the same question for one UID.
    /// Messages the server does not return are simply absent from the
    /// result; a UID can be expunged between the listing and this fetch, and
    /// that is not an error.
    pub async fn fetch_bodies(&mut self, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let mut bodies = Vec::new();
        for set in uid_sets(uids) {
            let command = format!("UID FETCH {set} (UID BODY.PEEK[])");
            self.run_until_done(&command, "FETCH BODY", |response| {
                if let Response::Fetch(_, attributes) = response
                    && let Some(uid) = attribute_uid(attributes)
                    && let Some(body) = attribute_body(attributes)
                {
                    bodies.push((uid, body.to_vec()));
                }
            })
            .await?;
        }

        Ok(bodies)
    }

    /// `UID SEARCH HEADER <name> <value>` in the selected mailbox.
    ///
    /// Two things need this. An APPEND says only that it worked, never which
    /// UID it produced — this build's server does not offer UIDPLUS's
    /// `APPENDUID` through async-imap — so the only way to find a message just
    /// written is to search for the `Message-ID` it was written with. And a
    /// task linked to a mail years ago has to be resolvable after the message
    /// has been moved between folders, when the stored UID means nothing and
    /// only the `Message-ID` still identifies it.
    ///
    /// Returned sorted, because SEARCH results arrive as a set and "the newest
    /// match" has to be well defined.
    pub async fn uid_search_header(&mut self, header: &str, value: &str) -> Result<Vec<u32>> {
        let query = format!("HEADER {} {}", header, quote_imap_string(value));
        let found = bounded("UID SEARCH", self.session.uid_search(&query)).await?;
        let mut uids: Vec<u32> = found.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    /// The UIDs in the selected mailbox carrying this RFC `Message-ID`.
    ///
    /// Normally one, occasionally none, and more than one when the same message
    /// was delivered twice — which is why this reports all of them rather than
    /// picking.
    pub async fn uid_search_message_id(&mut self, message_id: &str) -> Result<Vec<u32>> {
        self.uid_search_header("Message-ID", message_id).await
    }

    /// Fetch message body for a single UID
    ///
    /// Returns (body_text, body_html, rfc822_bytes) for the message.
    /// Returns error if message not found on server (deleted).
    /// Used for on-demand body fetching when viewing message detail.
    pub async fn fetch_message_body(
        &mut self,
        uid: u32,
    ) -> Result<(Option<String>, Option<String>, Option<Vec<u8>>)> {
        let mut found: Option<Option<Vec<u8>>> = None;
        // `PEEK`: showing a message is not reading it, which the reader's own
        // timer decides. And a FETCH of this UID without the body — the
        // server announcing its flags — must not take the body back: it once
        // did, and a new message opened as having no text.
        self.run_until_done(
            &format!("UID FETCH {uid} (UID BODY.PEEK[])"),
            "FETCH BODY",
            |response| {
                if let Response::Fetch(_, attributes) = response
                    && attribute_uid(attributes) == Some(uid)
                {
                    match attribute_body(attributes) {
                        Some(body) => found = Some(Some(body.to_vec())),
                        None if found.is_none() => found = Some(None),
                        None => {}
                    }
                }
            },
        )
        .await?;

        match found {
            // No FETCH response: the message is no longer on the server.
            None => Err(Error::NotFound(format!(
                "Message UID {} not found on server (may have been deleted)",
                uid
            ))),
            Some(Some(body)) => Ok((
                extract_body_text(&body),
                extract_body_html(&body),
                Some(body),
            )),
            // The message exists but has no body content (rare but possible).
            Some(None) => Ok((None, None, None)),
        }
    }

    /// Fetch BODYSTRUCTURE for several messages at once.
    ///
    /// Metadata only: the server describes each part tree without sending any
    /// of it, so this is cheap enough to run over an entire mailbox. Needed
    /// because a message synced before Orb started asking for BODYSTRUCTURE
    /// never gets one otherwise — sync only re-fetches messages whose modseq
    /// changed, and an old message's never will.
    ///
    /// A structure the parser cannot read is reported as
    /// [`Error::ImapProtocolError`], which tells the caller two things: the
    /// batch contains a message worth isolating, and this connection can no
    /// longer be used. Abandoning a response half-read leaves its remaining
    /// bytes in the socket, so the next command on the same connection parses
    /// those instead and quietly returns nothing. Narrowing therefore belongs
    /// with whoever can reconnect between attempts, which is the store.
    pub async fn fetch_shapes(
        &mut self,
        uids: &[u32],
    ) -> Result<std::collections::HashMap<u32, structure::MessageShape>> {
        if uids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let mut shapes = std::collections::HashMap::new();
        for set in uid_sets(uids) {
            let command = format!("UID FETCH {set} (BODYSTRUCTURE)");
            let result = self
                .run_until_done(&command, "FETCH BODYSTRUCTURE", |response| {
                    if let Response::Fetch(_, attributes) = response
                        && let Some(uid) = attribute_uid(attributes)
                    {
                        for attribute in attributes {
                            if let AttributeValue::BodyStructure(structure) = attribute {
                                shapes.insert(uid, structure::shape_of(structure));
                            }
                        }
                    }
                })
                .await;
            match result {
                Ok(()) => {}
                Err(Error::ImapProtocolError { details, source }) => {
                    return Err(Error::ImapProtocolError {
                        details: format!("BODYSTRUCTURE could not be parsed: {details}"),
                        source,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(shapes)
    }

    /// Fetch one MIME part from each of several messages, in one round trip.
    ///
    /// A FETCH applies a single part spec to every message in its range, so the
    /// caller must pass UIDs that all keep their text at `part` — grouping by
    /// part path is what makes a batch possible at all. Messages that do not
    /// have that part simply come back without it and are skipped.
    ///
    /// PEEK throughout: fetching a body must never mark mail as read.
    pub async fn fetch_parts(
        &mut self,
        uids: &[u32],
        part: &str,
    ) -> Result<std::collections::HashMap<u32, String>> {
        use async_imap::imap_proto::{MessageSection, SectionPath};

        let mut decoded = std::collections::HashMap::new();
        if uids.is_empty() {
            return Ok(decoded);
        }

        let path: Vec<u32> = part
            .split('.')
            .map(|p| p.parse::<u32>())
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| Error::InvalidInput(format!("bad MIME part path '{part}'")))?;

        let query = format!("(BODY.PEEK[{part}.MIME] BODY.PEEK[{part}])");
        let body_path = SectionPath::Part(path.clone(), None);
        let mime_path = SectionPath::Part(path, Some(MessageSection::Mime));

        for set in uid_sets(uids) {
            {
                let stream = bounded("FETCH", self.session.uid_fetch(set, &query)).await?;
                futures::pin_mut!(stream);
                while let Some(fetch) = next_bounded(&mut stream, "FETCH").await? {
                    let Some(uid) = fetch.uid else { continue };
                    let (Some(headers), Some(body)) =
                        (fetch.section(&mime_path), fetch.section(&body_path))
                    else {
                        continue;
                    };
                    if let Some(text) = decode_part(headers, body) {
                        decoded.insert(uid, text);
                    }
                }
            }
            self.ended_cleanly("FETCH").await?;
        }

        Ok(decoded)
    }

    // ========== IMAP Write Operations ==========

    /// Store flags on messages using UID STORE command.
    ///
    /// # Arguments
    /// * `uids` - Message UIDs to modify
    /// * `flags` - Flags to add or remove (e.g., "\\Seen", "\\Flagged", "\\Deleted")
    /// * `add` - true to add flags (+FLAGS), false to remove (-FLAGS)
    pub async fn store_flags(&mut self, uids: &[u32], flags: &[&str], add: bool) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        let flags_str = flags.join(" ");
        let query = if add {
            format!("+FLAGS ({})", flags_str)
        } else {
            format!("-FLAGS ({})", flags_str)
        };

        for uid_set in uid_sets(uids) {
            logger::debug(&format!(
                "UID STORE {} {}",
                uid_set,
                if add { "+FLAGS" } else { "-FLAGS" }
            ));
            // The responses carry the new flags, which nothing here needs; the
            // command is complete at its tagged response.
            self.run_until_done(&format!("UID STORE {uid_set} {query}"), "UID STORE", |_| {})
                .await?;
        }
        Ok(())
    }

    /// Move messages to another folder.
    ///
    /// Prefers `UID MOVE` (RFC 6851) when the server advertises the MOVE
    /// capability, which relocates exactly the requested UIDs without touching
    /// other messages. Otherwise falls back to the portable
    /// COPY + STORE \Deleted + scoped-expunge sequence. The expunge step is
    /// scoped (see [`Self::expunge_scoped`]) so that messages another client
    /// flagged `\Deleted` are never destroyed.
    ///
    /// A UID the folder no longer has is no error to either command: the
    /// server moves what it finds and says OK. Which UIDs that was is read
    /// from the COPYUID response (RFC 4315) and returned, for a caller that
    /// must know whether it was this move that relocated a message.
    ///
    /// # Arguments
    /// * `uids` - Message UIDs to move
    /// * `destination` - Target mailbox name
    pub async fn move_messages(&mut self, uids: &[u32], destination: &str) -> Result<MoveOutcome> {
        if uids.is_empty() {
            return Ok(MoveOutcome {
                confirmed: Some(Vec::new()),
            });
        }

        logger::debug(&format!("Moving {} UIDs to {}", uids.len(), destination));
        let destination = quote_imap_string(destination);
        let mut copied: Vec<u32> = Vec::new();
        let mut collect = |response: &Response<'_>| {
            let code = match response {
                Response::Data { code, .. } | Response::Done { code, .. } => code,
                _ => &None,
            };
            if let Some(ResponseCode::CopyUid(_, source, _)) = code {
                copied.extend(uids_in(source));
            }
        };

        // Preferred path: UID MOVE (RFC 6851) atomically relocates only the
        // requested UIDs and never expunges unrelated \Deleted messages.
        if self.has_move {
            for uid_set in uid_sets(uids) {
                self.run_until_done(
                    &format!("UID MOVE {uid_set} {destination}"),
                    "UID MOVE",
                    &mut collect,
                )
                .await?;
            }
            return Ok(MoveOutcome {
                confirmed: self.has_uidplus.then_some(copied),
            });
        }

        // Fallback: COPY + mark \Deleted + scoped expunge.
        // Step 1: Copy messages to destination
        for uid_set in uid_sets(uids) {
            self.run_until_done(
                &format!("UID COPY {uid_set} {destination}"),
                "UID COPY",
                &mut collect,
            )
            .await?;
        }

        // Step 2: Mark originals as deleted
        self.store_flags(uids, &["\\Deleted"], true).await?;

        // Step 3: Purge, scoped to just these UIDs to protect unrelated mail
        self.expunge_scoped(uids).await?;

        Ok(MoveOutcome {
            confirmed: self.has_uidplus.then_some(copied),
        })
    }

    /// Copy messages to another folder of the same account, on the server.
    pub async fn copy_messages(&mut self, uids: &[u32], destination: &str) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        for uid_set in uid_sets(uids) {
            bounded("UID COPY", self.session.uid_copy(&uid_set, destination)).await?;
        }
        Ok(())
    }

    /// Delete messages permanently.
    ///
    /// Sets \Deleted flag and expunges immediately.
    ///
    /// # Arguments
    /// * `uids` - Message UIDs to delete
    pub async fn delete_messages(&mut self, uids: &[u32]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        logger::debug(&format!("Deleting {} UIDs", uids.len()));

        // Mark as deleted
        self.store_flags(uids, &["\\Deleted"], true).await?;

        // Purge, scoped to just these UIDs so that messages another client (or
        // the user) flagged \Deleted are not destroyed.
        self.expunge_scoped(uids).await?;

        Ok(())
    }

    /// Purge the given UIDs, scoping the removal so that unrelated messages
    /// flagged `\Deleted` (e.g. by another client) are never destroyed.
    ///
    /// Assumes the caller has already set `\Deleted` on `uids`.
    ///
    /// - With UIDPLUS (RFC 4315) we issue `UID EXPUNGE`, which only removes
    ///   `\Deleted` messages within the given UID set.
    /// - Without UIDPLUS, a plain `EXPUNGE` removes *all* `\Deleted` messages,
    ///   so we only run it when no *other* `\Deleted` messages exist. If other
    ///   `\Deleted` messages are present we skip the purge (documented
    ///   limitation): the intended messages stay flagged `\Deleted` and will be
    ///   removed by a later full expunge/CLOSE rather than risk destroying
    ///   unrelated mail now.
    async fn expunge_scoped(&mut self, uids: &[u32]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        // Only probe for other \Deleted messages when we might fall back to a
        // plain EXPUNGE (i.e. no UIDPLUS). With UIDPLUS the probe is unnecessary.
        let other_deleted_exists = if self.has_uidplus {
            false
        } else {
            self.other_deleted_messages_exist(uids).await?
        };

        match decide_expunge_strategy(self.has_uidplus, other_deleted_exists) {
            ExpungeStrategy::UidExpunge => {
                for uid_set in uid_sets(uids) {
                    logger::debug(&format!("UID EXPUNGE {}", uid_set));
                    self.run_until_done(&format!("UID EXPUNGE {uid_set}"), "UID EXPUNGE", |_| {})
                        .await?;
                }
                Ok(())
            }
            ExpungeStrategy::PlainExpunge => {
                logger::debug(&format!(
                    "EXPUNGE (no UIDPLUS, no other \\Deleted messages) for {} UIDs",
                    uids.len()
                ));
                self.run_until_done("EXPUNGE", "EXPUNGE", |_| {}).await
            }
            ExpungeStrategy::SkipUnsafe => {
                logger::warn(&format!(
                    "Server lacks UIDPLUS and other \\Deleted messages exist; \
                     skipping EXPUNGE to avoid destroying unrelated mail. \
                     {} UIDs remain flagged \\Deleted and will be purged by a \
                     later full expunge.",
                    uids.len()
                ));
                Ok(())
            }
        }
    }

    /// Returns whether any message *other than* `uids` is flagged `\Deleted` in
    /// the currently selected mailbox, via `UID SEARCH DELETED`.
    async fn other_deleted_messages_exist(&mut self, uids: &[u32]) -> Result<bool> {
        let deleted = bounded("UID SEARCH", self.session.uid_search("DELETED")).await?;
        Ok(deleted.iter().any(|uid| !uids.contains(uid)))
    }

    /// Append a message to a mailbox (used for saving to Sent folder).
    ///
    /// # Arguments
    /// * `mailbox` - Target mailbox name (e.g., "Sent Messages")
    /// * `content` - RFC822 message bytes
    /// * `mark_seen` - Whether to mark the message as \Seen
    pub async fn append_message(
        &mut self,
        mailbox: &str,
        content: &[u8],
        mark_seen: bool,
    ) -> Result<()> {
        let flags = if mark_seen { Some(r"(\Seen)") } else { None };
        self.append_with_flags(mailbox, content, flags).await
    }

    /// Append a message with an explicit flag list.
    ///
    /// A draft needs `\Draft` as well as `\Seen`: without it other clients
    /// show it as ordinary mail sitting in the Drafts folder rather than as
    /// something still being written.
    pub async fn append_with_flags(
        &mut self,
        mailbox: &str,
        content: &[u8],
        flags: Option<&str>,
    ) -> Result<()> {
        check_mailbox_name(mailbox)?;
        logger::debug(&format!(
            "APPEND to {} ({} bytes, flags={})",
            mailbox,
            content.len(),
            flags.unwrap_or("none")
        ));

        // A message with attachments can be tens of megabytes over an uplink
        // of a megabit, so APPEND gets more time than any other command.
        within(
            OPERATION_TIMEOUT * 5,
            "APPEND",
            self.session.append(mailbox, flags, None, content),
        )
        .await
        .inspect_err(|error| {
            logger::warn(&format!("APPEND to {} failed: {}", mailbox, error));
        })
    }

    /// NOOP: nothing, but the server has to say so.
    ///
    /// The server's answer may carry what changed in the selected mailbox
    /// since it last said anything; those responses are not read by anyone
    /// here, so they are dropped rather than left to pile up.
    pub async fn noop(&mut self) -> Result<()> {
        bounded("NOOP", self.session.noop()).await?;
        while self.session.unsolicited_responses.try_recv().is_ok() {}
        Ok(())
    }

    /// Is the server still there? A NOOP with the short probe timeout, for a
    /// connection that has sat idle long enough to doubt.
    pub async fn probe(&mut self) -> Result<()> {
        within(PROBE_TIMEOUT, "NOOP", self.session.noop()).await?;
        while self.session.unsolicited_responses.try_recv().is_ok() {}
        Ok(())
    }

    /// Get folder status without selecting the mailbox.
    ///
    /// Uses IMAP STATUS command to get message counts quickly (~20ms).
    /// Does NOT change the currently selected mailbox.
    ///
    /// # Arguments
    /// * `mailbox` - Mailbox name to query
    ///
    /// # Returns
    /// * `FolderStatus` with total and unseen message counts
    pub async fn get_folder_status(&mut self, mailbox: &str) -> Result<FolderStatus> {
        use std::time::Instant;
        let start = Instant::now();

        // STATUS mailbox (MESSAGES UNSEEN)
        let mailbox_status =
            bounded("STATUS", self.session.status(mailbox, "(MESSAGES UNSEEN)")).await?;

        // async-imap returns a Mailbox struct for STATUS responses
        // For STATUS, `exists` contains the total message count (MESSAGES)
        // and `unseen` contains the unseen count
        let total = mailbox_status.exists;
        let unseen = mailbox_status.unseen.unwrap_or(0);

        logger::debug(&format!(
            "STATUS {} took {:?} (total: {}, unseen: {})",
            mailbox,
            start.elapsed(),
            total,
            unseen
        ));

        Ok(FolderStatus { total, unseen })
    }

    /// Get status for multiple folders in sequence.
    ///
    /// More efficient than multiple get_folder_status calls when connection
    /// is already established.
    ///
    /// # Arguments
    /// * `mailboxes` - List of mailbox names to query
    ///
    /// # Returns
    /// * Vec of (mailbox_name, FolderStatus) tuples
    pub async fn get_multiple_folder_status(
        &mut self,
        mailboxes: &[&str],
    ) -> Result<Vec<(String, FolderStatus)>> {
        use std::time::Instant;
        let start = Instant::now();

        let mut results = Vec::with_capacity(mailboxes.len());

        for mailbox in mailboxes {
            match self.get_folder_status(mailbox).await {
                Ok(status) => results.push((mailbox.to_string(), status)),
                Err(e) => {
                    // Log but don't fail the entire operation
                    logger::warn(&format!("STATUS {} failed: {}", mailbox, e));
                }
            }
        }

        logger::debug(&format!(
            "STATUS for {} folders took {:?}",
            mailboxes.len(),
            start.elapsed()
        ));

        Ok(results)
    }
}

/// One message as a header-only listing sees it: its UID, its flags and the
/// raw block of the header fields that were asked for.
///
/// The headers are handed back as bytes rather than parsed because what counts
/// as a header depends entirely on who asked: this module has no business
/// knowing what `X-Orb-Task-Id` means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderListing {
    pub uid: u32,
    pub flags: Vec<String>,
    pub headers: Vec<u8>,
}

impl HeaderListing {
    /// Whether the server has this message on its way out.
    ///
    /// A `\Deleted` message still appears in every listing until somebody
    /// expunges, and treating it as live resurrects whatever was deleted: the
    /// row is gone locally, the message is still listed, and the next sync
    /// downloads it back as something unknown.
    pub fn is_deleted(&self) -> bool {
        self.flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Deleted"))
    }
}

/// Wrap a value as an IMAP quoted string (RFC 3501 §4.3).
///
/// `"` and `\` are the only characters that need escaping inside one. It
/// matters more than it looks: the value comes from a `Message-ID` that was
/// written by whoever sent the mail, so an unescaped quote would end the string
/// early and turn the rest of it into search keys — at best a syntax error, at
/// worst a different search than the one intended.
fn quote_imap_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        // CR and LF cannot appear in a quoted string at all; dropping them is
        // the only safe reading, since keeping them would split the command.
        match c {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '\r' | '\n' => {}
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Refuse a mailbox name that cannot be put on the wire whole.
///
/// The IMAP library quotes the name in every command but APPEND, where it
/// writes it between quotes as it is: a quote in the name would end the
/// string early and a line break would start a second command on the
/// authenticated connection. Nothing Orb's own folders are called has either;
/// a configured or server-given name that does is an error, not a command.
fn check_mailbox_name(mailbox: &str) -> Result<()> {
    if mailbox.is_empty() || mailbox.contains(['"', '\\', '\r', '\n']) {
        return Err(Error::InvalidInput(format!(
            "mailbox name {mailbox:?} cannot be sent in an APPEND command"
        )));
    }
    Ok(())
}

/// What a mailbox says about itself, from STATUS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderState {
    pub messages: u32,
    pub uid_next: Option<u32>,
    pub uid_validity: Option<u32>,
    pub highest_modseq: Option<u64>,
}

/// Folder status from IMAP STATUS command
#[derive(Debug, Clone, Default)]
pub struct FolderStatus {
    /// Total number of messages in the folder
    pub total: u32,
    /// Number of unseen (unread) messages
    pub unseen: u32,
}

/// Format a UID set for an IMAP command.
///
/// Runs of consecutive UIDs collapse to `first:last`, as RFC 3501 §9 allows.
/// Without that a command names every UID individually, and selecting a whole
/// folder builds a command line megabytes long — past what servers accept, and
/// on a large folder the difference between one short command and a connection
/// dropped mid-sentence. A contiguous selection of a quarter of a million
/// messages becomes eleven characters.
/// The longest UID set one command carries, in bytes. RFC 7162 §4 asks
/// clients to keep a command line to about 8192 octets, and a server with a
/// shorter limit does not say so: it waits for the rest of a line it will
/// never get, and the command times out with nothing answered. A page of a
/// cold sync over a folder with sparse UIDs — 205 000 messages spread over
/// 885 000 UIDs — wrote a 19 KiB set and never got a reply.
const MAX_UID_SET_BYTES: usize = 7_000;

/// What a move did, as far as the server said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveOutcome {
    /// The UIDs the server confirmed it moved, in the order it named them.
    /// `None` when it does not say: without UIDPLUS there is no COPYUID, and
    /// a move that found nothing cannot be told from one that moved it all.
    pub confirmed: Option<Vec<u32>>,
}

/// The UIDs a COPYUID set names, each range written out.
fn uids_in(set: &[UidSetMember]) -> Vec<u32> {
    set.iter()
        .flat_map(|member| match member {
            UidSetMember::Uid(uid) => *uid..=*uid,
            UidSetMember::UidRange(range) => range.clone(),
        })
        .collect()
}

/// `uids` as UID sets of at most [`MAX_UID_SET_BYTES`] each, in ascending
/// order: one command per set covers them all.
pub(crate) fn uid_sets(uids: &[u32]) -> Vec<String> {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut tokens: Vec<String> = Vec::new();
    let mut iter = sorted.into_iter();
    if let Some(first) = iter.next() {
        let (mut start, mut end) = (first, first);
        for uid in iter {
            if uid == end + 1 {
                end = uid;
                continue;
            }
            tokens.push(range_token(start, end));
            start = uid;
            end = uid;
        }
        tokens.push(range_token(start, end));
    }

    let mut sets: Vec<String> = Vec::new();
    let mut current = String::new();
    for token in tokens {
        if !current.is_empty() && current.len() + 1 + token.len() > MAX_UID_SET_BYTES {
            sets.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(',');
        }
        current.push_str(&token);
    }
    if !current.is_empty() {
        sets.push(current);
    }
    sets
}

/// Whether a listed name is a mailbox, as opposed to a node of the hierarchy
/// that only exists to have children.
/// A mailbox as LIST describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedFolder {
    pub name: String,
    /// The role the server gives the mailbox (RFC 6154): `\Sent`, `\Trash`,
    /// `\Drafts`, `\Junk` or `\Archive`, in that spelling.
    pub special_use: Option<String>,
    /// The character the server separates hierarchy levels with in this
    /// name, or none for a server with a flat namespace.
    pub delimiter: Option<String>,
}

/// The RFC 6154 role among a mailbox's LIST attributes, if any.
fn special_use(attributes: &[NameAttribute<'_>]) -> Option<String> {
    attributes.iter().find_map(|attribute| match attribute {
        NameAttribute::Sent => Some("\\Sent".to_string()),
        NameAttribute::Trash => Some("\\Trash".to_string()),
        NameAttribute::Drafts => Some("\\Drafts".to_string()),
        NameAttribute::Junk => Some("\\Junk".to_string()),
        NameAttribute::Archive => Some("\\Archive".to_string()),
        NameAttribute::Extension(name) => {
            let name = name.as_ref();
            ["\\Sent", "\\Trash", "\\Drafts", "\\Junk", "\\Archive"]
                .iter()
                .find(|role| role.eq_ignore_ascii_case(name))
                .map(|role| role.to_string())
        }
        _ => None,
    })
}

fn is_selectable(attributes: &[NameAttribute<'_>]) -> bool {
    !attributes
        .iter()
        .any(|attribute| matches!(attribute, NameAttribute::NoSelect))
}

/// `5` for a single UID, `5:9` for a run of them.
fn range_token(start: u32, end: u32) -> String {
    if start == end {
        format!("{start}")
    } else {
        format!("{start}:{end}")
    }
}

/// Extract plain text body from RFC822 email bytes
pub(crate) fn extract_body_text(bytes: &[u8]) -> Option<String> {
    let parsed = parse_mail(bytes).ok()?;
    extract_text_part(&parsed, "text/plain")
}

/// Extract HTML body from RFC822 email bytes
pub(crate) fn extract_body_html(bytes: &[u8]) -> Option<String> {
    let parsed = parse_mail(bytes).ok()?;
    extract_text_part(&parsed, "text/html")
}

/// Recursively extract text content of specified MIME type from parsed email
fn extract_text_part(mail: &mailparse::ParsedMail, content_type: &str) -> Option<String> {
    let ctype = mail.ctype.mimetype.to_lowercase();

    // Direct match
    if ctype == content_type {
        return mail.get_body().ok();
    }

    // For multipart messages, search parts
    if ctype.starts_with("multipart/") {
        // For multipart/alternative, prefer the requested type
        // For multipart/mixed, search all parts
        for part in &mail.subparts {
            if let Some(body) = extract_text_part(part, content_type) {
                return Some(body);
            }
        }
    }

    None
}

/// Decode one fetched MIME part into text.
///
/// The part's own MIME headers are fetched alongside its body and prepended
/// here so `mailparse` can do the decoding: it handles both the transfer
/// encoding (base64, quoted-printable) and the charset declared in
/// `Content-Type`. Doing that by hand would mean reimplementing charset
/// conversion, and a mailbox contains plenty of latin-1.
fn decode_part(mime_headers: &[u8], body: &[u8]) -> Option<String> {
    let mut combined = Vec::with_capacity(mime_headers.len() + body.len() + 2);
    combined.extend_from_slice(mime_headers);
    // The headers section may or may not carry its own terminating blank line.
    if !mime_headers.ends_with(b"\r\n\r\n") && !mime_headers.ends_with(b"\n\n") {
        combined.extend_from_slice(b"\r\n");
    }
    combined.extend_from_slice(body);

    let parsed = parse_mail(&combined).ok()?;
    parsed.get_body().ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::RangeInclusive;

    /// APPEND is the one command the library does not quote the mailbox name
    /// for, so a name that would break out of the quotes is refused here.
    #[test]
    fn a_mailbox_name_that_cannot_be_quoted_is_refused_for_append() {
        for name in ["INBOX", "Tasks", "INBOX.Sent Items", "Ärger", "a'b"] {
            assert!(check_mailbox_name(name).is_ok(), "{name}");
        }
        for name in ["", "a\"b", "a\\b", "a\r\nX LOGOUT", "a\nb"] {
            assert!(check_mailbox_name(name).is_err(), "{name:?}");
        }
    }

    /// `[Gmail]` and the like are listed alongside the real folders, flagged
    /// `\Noselect`, and a sync that took them for folders failed on each.
    #[test]
    fn a_noselect_node_is_not_a_folder() {
        assert!(is_selectable(&[]));
        assert!(is_selectable(&[
            NameAttribute::Extension("\\HasChildren".into()),
            NameAttribute::Marked,
        ]));
        assert!(!is_selectable(&[NameAttribute::NoSelect]));
        assert!(!is_selectable(&[
            NameAttribute::Extension("\\HasChildren".into()),
            NameAttribute::NoSelect,
            NameAttribute::NoInferiors,
        ]));
    }

    /// The display name is the part a person recognises, and it used to be
    /// thrown away.
    #[test]
    fn an_address_keeps_the_name_beside_it() {
        let f = |n: Option<&[u8]>, m: &[u8], h: &[u8]| {
            ImapClient::format_email_address(n, Some(m), Some(h))
        };

        assert_eq!(
            f(Some(b"Thomas Muster"), b"thomas", b"example.com").as_deref(),
            Some("Thomas Muster <thomas@example.com>")
        );

        // An encoded name is decoded, like every other header.
        assert_eq!(
            f(Some(b"=?utf-8?Q?Gr=C3=BCezi?="), b"a", b"b.ch").as_deref(),
            Some("Grüezi <a@b.ch>")
        );

        // No name, an empty one, or one that just repeats the address: the
        // address alone, rather than saying it twice.
        assert_eq!(f(None, b"a", b"b.com").as_deref(), Some("a@b.com"));
        assert_eq!(f(Some(b"   "), b"a", b"b.com").as_deref(), Some("a@b.com"));
        assert_eq!(
            f(Some(b"a@b.com"), b"a", b"b.com").as_deref(),
            Some("a@b.com")
        );

        // Without a mailbox there is no address to show at all — this is also
        // the group terminator, which carries nothing.
        assert_eq!(
            ImapClient::format_email_address(Some(b"Name"), None, Some(b"h")),
            None
        );
    }

    /// `\Recent` is true for whoever looks first and false for everyone after,
    /// so storing it guarantees a difference against the server that is not a
    /// difference — noise in the ordinary case, camouflage when hunting drift.
    #[test]
    fn session_flags_are_not_message_flags() {
        assert!(is_session_flag("\\Recent"));
        assert!(is_session_flag("\\recent"));
        assert!(!is_session_flag("\\Seen"));
        assert!(!is_session_flag("\\Flagged"));
        assert!(!is_session_flag("Recent"));
    }

    /// Naming every UID individually builds a command line megabytes long for
    /// a folder-sized selection, which servers refuse.
    #[test]
    fn a_uid_set_collapses_runs_into_ranges() {
        assert_eq!(uid_sets(&[1, 2, 3, 4, 5]), ["1:5"]);
        assert_eq!(uid_sets(&[1, 2, 3, 7, 9, 10]), ["1:3,7,9:10"]);
        assert_eq!(uid_sets(&[5]), ["5"]);
        assert!(uid_sets(&[]).is_empty());
        // Unsorted and duplicated input is still one clean run.
        assert_eq!(uid_sets(&[3, 1, 2, 2]), ["1:3"]);
        // A gap of one is a gap.
        assert_eq!(uid_sets(&[1, 3]), ["1,3"]);
    }

    /// The case this exists for: a whole folder selected.
    #[test]
    fn a_whole_folder_fits_in_a_command() {
        let all: Vec<u32> = (1..=262_027).collect();
        assert_eq!(uid_sets(&all), ["1:262027"]);
    }

    /// A page of a cold sync over sparse UIDs: 5 000 messages, every other
    /// UID, high in the range. One set would be 35 KiB; each command now
    /// carries at most `MAX_UID_SET_BYTES`, and together they name every UID
    /// once, in order.
    #[test]
    fn a_sparse_page_is_split_into_command_lines_a_server_accepts() {
        let page: Vec<u32> = (0..5_000).map(|n| 880_000 + 2 * n).collect();
        let sets = uid_sets(&page);
        assert!(sets.len() > 1, "split into several commands");
        assert!(sets.iter().all(|set| set.len() <= MAX_UID_SET_BYTES));
        let named: Vec<u32> = sets
            .iter()
            .flat_map(|set| set.split(','))
            .map(|uid| uid.parse().unwrap())
            .collect();
        assert_eq!(named, page);
    }

    /// The response that stalled every sync: a server reporting `1:*`, which
    /// the parser turned into two billion UIDs and eight gigabytes before the
    /// answer was thrown away for being implausible — so a mailbox emptied in
    /// another client was never cleaned up here.
    #[test]
    fn a_vanished_range_covering_everything_costs_nothing() {
        let everything = vec![RangeInclusive::new(1, u32::MAX)];
        let held = [861_221, 861_207];
        assert_eq!(vanished_within(&everything, &held), vec![861_221, 861_207]);
    }

    #[test]
    fn only_uids_actually_held_are_reported_gone() {
        let ranges = vec![10..=20, 100..=100];
        // 5 and 50 are outside every range; 30 is between two of them.
        assert_eq!(
            vanished_within(&ranges, &[5, 15, 30, 50, 100]),
            vec![15, 100]
        );
    }

    #[test]
    fn overlapping_unordered_and_reversed_ranges_are_all_honoured() {
        let ranges = vec![50..=60, 1..=3, 55..=70, RangeInclusive::new(90, 80), 4..=4];
        assert_eq!(
            vanished_within(&ranges, &[1, 4, 5, 65, 71, 85, 91]),
            vec![1, 4, 65, 85]
        );
    }

    /// The launch sync of a 205 000-message archive whose server reported
    /// 41 529 ranges spent fifty seconds here and stalled every other sync.
    #[test]
    fn many_ranges_over_a_large_mailbox_are_cheap() {
        let ranges: Vec<RangeInclusive<u32>> =
            (0..50_000u32).map(|n| n * 10..=n * 10 + 2).collect();
        let held: Vec<u32> = (0..250_000u32).map(|n| n * 2 + 1).collect();
        let started = std::time::Instant::now();
        let gone = vanished_within(&ranges, &held);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(gone.len(), 50_000);
    }

    #[test]
    fn nothing_reported_means_nothing_deleted() {
        assert!(vanished_within(&[], &[1, 2, 3]).is_empty());
        assert!(vanished_within(&[RangeInclusive::new(1, 100)], &[]).is_empty());
    }

    /// A draft being written holds a recipient that is not an address yet, and
    /// dropping it meant reopening the draft showed an empty To field.
    #[test]
    fn an_address_without_a_domain_is_kept_as_written() {
        assert_eq!(
            ImapClient::format_email_address(None, Some(b"foo"), None).as_deref(),
            Some("foo")
        );
        // An empty host is the same case, written differently.
        assert_eq!(
            ImapClient::format_email_address(None, Some(b"foo"), Some(b"")).as_deref(),
            Some("foo")
        );
        // A group name arrives the same way and is worth showing too.
        assert_eq!(
            ImapClient::format_email_address(None, Some(b"Undisclosed recipients"), None)
                .as_deref(),
            Some("Undisclosed recipients")
        );
    }

    /// The XOAUTH2 initial response as Gmail and Office 365 read it, before
    /// the base64 the SASL exchange puts around it.
    #[test]
    fn the_xoauth2_response_carries_the_user_and_the_bearer_token() {
        assert_eq!(
            xoauth2_response("me@example.com", "ya29.token"),
            "user=me@example.com\x01auth=Bearer ya29.token\x01\x01"
        );
        use async_imap::Authenticator;
        let mut auth = Xoauth2 {
            user: "me@example.com".into(),
            token: "t".into(),
            answered: false,
        };
        assert!(!auth.process(b"").is_empty());
        assert!(
            auth.process(b"eyJzdGF0dXMiOiI0MDEifQ==").is_empty(),
            "a refusal gets an empty line"
        );
    }

    #[test]
    fn a_header_is_decoded_leniently_and_never_silently_dropped() {
        // The real subject that showed as "(no subject)": one encoded-word of
        // 110 characters, well past RFC 2047's 75-character limit, which the
        // decoder rejects outright by default.
        let long = b"=?utf-8?Q?[Ticket=200001234]:=20Exported=20report=20shows=20the=20wrong=20totals=20in=20every=20section=20of=20it?=";
        assert!(long.len() > 75);
        assert_eq!(
            decode_header(long),
            "[Ticket 0001234]: Exported report shows the wrong totals in every section of it"
        );

        // Ordinary encoded-words keep working.
        assert_eq!(decode_header(b"=?utf-8?B?SGVsbG8=?="), "Hello");
        assert_eq!(decode_header(b"=?iso-8859-1?Q?Gr=FC=DFe?="), "Grüße");

        // Plain text passes through untouched.
        assert_eq!(decode_header(b"RE: Hotel booking"), "RE: Hotel booking");

        // Undecodable input falls back to the raw text rather than to nothing.
        // Showing the encoded form is worse than showing the words, and far
        // better than showing an empty subject.
        let broken = b"=?definitely-not-a-charset?Q?whatever?=";
        let decoded = decode_header(broken);
        assert!(
            !decoded.is_empty(),
            "a failed decode must not erase the header"
        );

        // Invalid UTF-8 is carried across lossily instead of being discarded.
        assert!(!decode_header(&[0xff, 0xfe, b'h', b'i']).is_empty());
    }

    /// The bug this was written for: one hoster reports `HIGHESTMODSEQ 85` for
    /// a 62,756-message mailbox and never moves it, so `CHANGEDSINCE 85`
    /// returns nothing and new mail is never fetched — permanently, since a
    /// manual refresh takes the same path. UIDNEXT is the way out.
    #[test]
    fn a_uidnext_beyond_what_is_stored_reveals_mail_the_modseq_hid() {
        // Private/INBOX as it actually stood: newest stored UID 69619, server
        // UIDNEXT 69707. Everything from 69620 up had never been seen.
        assert_eq!(uid_gap(Some(69_619), 69_707), Some(69_620));

        // Exactly caught up: UIDNEXT is one past the highest stored UID.
        assert_eq!(uid_gap(Some(69_706), 69_707), None);

        // A single new message.
        assert_eq!(uid_gap(Some(10), 12), Some(11));
    }

    #[test]
    fn no_gap_is_reported_when_there_is_nothing_to_recover() {
        // Empty mailbox, nothing stored: the caller does a full fetch anyway.
        assert_eq!(uid_gap(None, 1), None);
        assert_eq!(uid_gap(None, 9_999), None);

        // A server reporting a UIDNEXT behind our highest UID is nonsense, but
        // must not produce a range that would re-fetch the whole mailbox.
        assert_eq!(uid_gap(Some(500), 100), None);
        assert_eq!(uid_gap(Some(500), 0), None);

        // Saturation rather than overflow at the top of the UID space.
        assert_eq!(uid_gap(Some(u32::MAX), u32::MAX), None);
    }

    #[test]
    fn uidplus_always_uses_uid_expunge() {
        // With UIDPLUS the purge is scoped to the given UID set regardless of
        // whether other \Deleted messages exist, so unrelated mail survives.
        assert_eq!(
            decide_expunge_strategy(true, false),
            ExpungeStrategy::UidExpunge
        );
        assert_eq!(
            decide_expunge_strategy(true, true),
            ExpungeStrategy::UidExpunge
        );
    }

    #[test]
    fn no_uidplus_plain_expunge_only_when_no_other_deleted() {
        // Safe: a plain EXPUNGE removes only the intended messages.
        assert_eq!(
            decide_expunge_strategy(false, false),
            ExpungeStrategy::PlainExpunge
        );
    }

    /// A `Message-ID` is written by whoever sent the mail, so it is not a safe
    /// string: an unescaped quote would end the quoted string early and turn
    /// the remainder into further search keys.
    #[test]
    fn a_search_value_cannot_escape_its_quoted_string() {
        assert_eq!(
            quote_imap_string("<abc@example.com>"),
            "\"<abc@example.com>\""
        );
        assert_eq!(quote_imap_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_imap_string("a\\b"), "\"a\\\\b\"");
        // A newline would end the command line itself.
        assert_eq!(quote_imap_string("<a\r\n DELETED@x>"), "\"<a DELETED@x>\"");
        assert_eq!(quote_imap_string(""), "\"\"");
    }

    /// A message on its way out must not be mistaken for a live one: after a
    /// local delete whose expunge did not land, counting it as present
    /// downloads the deleted thing straight back.
    #[test]
    fn a_deleted_message_is_recognised_from_its_flags() {
        let listing = |flags: &[&str]| HeaderListing {
            uid: 1,
            flags: flags.iter().map(|f| f.to_string()).collect(),
            headers: Vec::new(),
        };

        assert!(listing(&["\\Seen", "\\Deleted"]).is_deleted());
        // Flags come off the wire with whatever case the server used.
        assert!(listing(&["\\deleted"]).is_deleted());
        assert!(!listing(&["\\Seen"]).is_deleted());
        assert!(!listing(&[]).is_deleted());
        // A keyword that merely contains the word is not the system flag.
        assert!(!listing(&["Deleted-Later"]).is_deleted());
    }

    #[test]
    fn no_uidplus_skips_when_other_deleted_present() {
        // Regression guard: without UIDPLUS, a plain EXPUNGE would also purge a
        // second \Deleted message flagged elsewhere. We must NOT do that.
        assert_eq!(
            decide_expunge_strategy(false, true),
            ExpungeStrategy::SkipUnsafe
        );
    }

    /// A COPYUID set mixes single UIDs with ranges; both are moved UIDs.
    #[test]
    fn copyuid_sets_are_written_out() {
        assert_eq!(
            uids_in(&[
                UidSetMember::Uid(7),
                UidSetMember::UidRange(10..=12),
                UidSetMember::Uid(20)
            ]),
            vec![7, 10, 11, 12, 20]
        );
        assert!(uids_in(&[]).is_empty());
    }
}
