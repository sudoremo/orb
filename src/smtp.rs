use std::time::Duration;

use lettre::message::header::{self, ContentType, HeaderName, HeaderValue, Headers};
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use secrecy::ExposeSecret;

/// SMTP connection timeout in seconds
const SMTP_TIMEOUT_SECS: u64 = 30;

use crate::config::AccountConfig;
use secrecy::SecretString;

#[derive(Debug)]
pub enum SmtpError {
    InvalidAddress(String),
    BuildError(String),
    /// The server answered, and the answer was no.
    SendError(String),
    /// The server refused the login (534, 535): the password, not the
    /// message, is what has to change.
    AuthRefused(String),
    /// The server could not be reached, or stopped answering. Nothing is wrong
    /// with the message; waiting is the right response.
    Unreachable(String),
    ConfigError(String),
}

impl std::fmt::Display for SmtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmtpError::InvalidAddress(e) => write!(f, "Invalid email address: {}", e),
            SmtpError::BuildError(e) => write!(f, "Failed to build message: {}", e),
            SmtpError::SendError(e) => write!(f, "Failed to send email: {}", e),
            SmtpError::AuthRefused(e) => write!(f, "The mail server refused the login: {}", e),
            SmtpError::Unreachable(e) => write!(f, "Mail server unreachable: {}", e),
            SmtpError::ConfigError(e) => write!(f, "Configuration error: {}", e),
        }
    }
}

impl std::error::Error for SmtpError {}

/// Validates an email address has proper structure.
/// Checks that:
/// - Address contains exactly one @
/// - Local part (before @) is non-empty
/// - Domain part (after @) is non-empty and contains at least one dot
fn is_valid_email_address(addr: &str) -> bool {
    let addr = addr.trim();
    let parts: Vec<&str> = addr.split('@').collect();

    // Must have exactly one @
    if parts.len() != 2 {
        return false;
    }

    let local = parts[0];
    let domain = parts[1];

    // Local part must be non-empty
    if local.is_empty() {
        return false;
    }

    // Domain must be non-empty and contain at least one dot
    if domain.is_empty() || !domain.contains('.') {
        return false;
    }

    // Domain parts must be non-empty (no leading/trailing dots, no consecutive dots)
    let domain_parts: Vec<&str> = domain.split('.').collect();
    if domain_parts.iter().any(|p| p.is_empty()) {
        return false;
    }

    true
}

/// A single file to attach to an outgoing email.
///
/// Carries the already-read file contents so the SMTP layer stays free of
/// filesystem access; callers (e.g. the FFI `send_mail`) read the file and
/// infer the MIME type before handing it over.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmailAttachment {
    /// Filename shown to the recipient (the `filename` disposition param).
    pub filename: String,
    /// MIME type of the payload, e.g. `application/pdf`.
    pub content_type: String,
    /// Raw (undecoded) file bytes; lettre applies the transfer encoding.
    pub bytes: Vec<u8>,
}

/// Email to be sent
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OutgoingEmail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// In-Reply-To header (Message-ID of the message being replied to)
    pub in_reply_to: Option<String>,
    /// References header (for email threading)
    pub references: Option<String>,
    /// The Message-ID of the message this one forwards, sent as
    /// `X-Forwarded-Message-Id` the way Apple Mail does.
    #[serde(default)]
    pub forwards: Option<String>,
    /// The identity's `From`, when it is not the account's. Absent from a
    /// message queued before identities existed, hence the default.
    #[serde(default)]
    pub from: Option<String>,
    /// The identity's `Reply-To`.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Files to attach. When non-empty the message is rendered as
    /// `multipart/mixed` with the body followed by one attachment part each.
    #[serde(default)]
    pub attachments: Vec<EmailAttachment>,
    /// The `Message-ID` to send under, chosen before the message goes out so
    /// a task can be linked to it — and kept by a queued message, which is
    /// built again when it is finally sent. Lettre makes one up when absent.
    #[serde(default)]
    pub message_id: Option<String>,
}

impl OutgoingEmail {
    /// Who the message is from: the identity's address, or the account's.
    pub fn sender<'a>(&'a self, account: &'a AccountConfig) -> &'a str {
        self.from.as_deref().unwrap_or(&account.from)
    }

    /// Validate the email before sending
    pub fn validate(&self) -> Result<(), SmtpError> {
        if self.to.is_empty() {
            return Err(SmtpError::InvalidAddress(
                "No recipients specified".to_string(),
            ));
        }

        for addr in &self.to {
            if !is_valid_email_address(addr) {
                return Err(SmtpError::InvalidAddress(format!(
                    "Invalid To address: {}",
                    addr
                )));
            }
        }

        for addr in &self.cc {
            if !is_valid_email_address(addr) {
                return Err(SmtpError::InvalidAddress(format!(
                    "Invalid Cc address: {}",
                    addr
                )));
            }
        }

        for addr in &self.bcc {
            if !is_valid_email_address(addr) {
                return Err(SmtpError::InvalidAddress(format!(
                    "Invalid Bcc address: {}",
                    addr
                )));
            }
        }

        if self.subject.is_empty() {
            return Err(SmtpError::BuildError("Subject is required".to_string()));
        }

        if self.body.trim().is_empty() {
            return Err(SmtpError::BuildError(
                "Message body is required".to_string(),
            ));
        }

        Ok(())
    }
}

/// Build a lettre [`Message`] from a parsed `from` mailbox and an
/// [`OutgoingEmail`].
///
/// The plain-text body is wrapped in a `multipart/alternative`. When the email
/// carries attachments the whole thing becomes a `multipart/mixed` with the
/// body first and one `SinglePart` per attachment, each declaring
/// `Content-Disposition: attachment` with its filename and content type.
///
/// Pure and free of I/O so the MIME structure can be unit-tested without an
/// account or a live SMTP connection.
/// Build the RFC822 bytes of a message without sending it.
///
/// A draft is written from the same body and attachments the send path builds,
/// but its recipient headers are written verbatim rather than parsed. A draft is
/// half-finished by definition: `to` may hold `foo`, a name whose address has
/// not been typed yet, or nothing at all, and none of that is an error until the
/// message is actually sent. Requiring an address to save meant the moment a
/// draft most needed keeping — mid-word, before the window is closed — was
/// exactly when it could not be.
///
/// The headers are still built by lettre, so a subject keeps its RFC 2047
/// encoding and its folding. Only the addresses go in as typed, which is what
/// makes reopening the draft show what was written rather than a corrected
/// version of it.
pub fn draft_bytes(
    account: &crate::config::AccountConfig,
    email: &OutgoingEmail,
) -> Result<Vec<u8>, SmtpError> {
    let from: Mailbox = email
        .sender(account)
        .parse()
        .map_err(|e| SmtpError::InvalidAddress(format!("Invalid From address: {e}")))?;

    let mut headers = Headers::new();
    headers.set(header::From::from(lettre::message::Mailboxes::from(from)));
    if let Some(reply_to) = &email.reply_to {
        // Checked as a send checks it, so a `reply_to` mistyped in the config
        // fails at the first save rather than at the first send.
        let parsed: Mailbox = reply_to.parse().map_err(|e| {
            SmtpError::InvalidAddress(format!("Invalid Reply-To address '{reply_to}': {e}"))
        })?;
        headers.insert_raw(raw("Reply-To", &parsed.to_string()));
    }
    headers.set(header::Subject::from(email.subject.clone()));
    headers.set(header::Date::now());
    // Both are added by lettre when it builds a message and so have to be added
    // here too. Without MIME-Version a strict reader is entitled to treat the
    // multipart as plain text, and a message with no Message-ID is one a reply
    // cannot be threaded onto.
    headers.set(header::MIME_VERSION_1_0);
    headers.set(header::MessageId::from(new_message_id(account)));
    if let Some(id) = &email.in_reply_to {
        headers.insert_raw(raw("In-Reply-To", id));
    }
    if let Some(refs) = &email.references {
        headers.insert_raw(raw("References", refs));
    }
    if let Some(id) = &email.forwards {
        headers.insert_raw(raw(FORWARDED_HEADER, id));
    }
    for (name, list) in [("To", &email.to), ("Cc", &email.cc), ("Bcc", &email.bcc)] {
        let joined = address_list(list);
        // An empty header line says "no recipients" no more clearly than no
        // line at all, and some servers dislike it.
        if !joined.is_empty() {
            headers.insert_raw(raw(name, &joined));
        }
    }

    let mut bytes = headers.to_string().into_bytes();
    bytes.extend_from_slice(&body_part(email)?.formatted());
    Ok(bytes)
}

/// A header taken as typed.
///
/// Newlines are stripped rather than escaped: a value carrying one would end
/// the header block early and turn everything after it into the body, so a
/// pasted address that happens to span lines must not be able to do that.
/// Names the message a forward forwards, in the sent mail and in its draft.
pub const FORWARDED_HEADER: &str = "X-Forwarded-Message-Id";

fn raw(name: &'static str, value: &str) -> HeaderValue {
    let flattened = value.replace(['\r', '\n'], " ");
    HeaderValue::new(HeaderName::new_from_ascii_str(name), flattened)
}

/// A fresh `Message-ID` for a message written from `account`.
pub fn new_message_id(account: &crate::config::AccountConfig) -> String {
    format!(
        "<{}@{}>",
        uuid::Uuid::new_v4(),
        // The sender's own domain, rather than this machine's hostname: it is
        // the one part of the address that is certainly routable, and a laptop
        // name in a Message-ID tells the world where the mail was written.
        message_id_domain(&account.from)
    )
}

/// The domain to stamp into a Message-ID, taken from the sender's own address.
///
/// Falls back to a name reserved for exactly this — an address that cannot be
/// resolved is better than one belonging to somebody else.
fn message_id_domain(from: &str) -> String {
    from.rsplit_once('@')
        .map(|(_, domain)| domain.trim_end_matches('>').trim().to_string())
        .filter(|domain| !domain.is_empty())
        .unwrap_or_else(|| "invalid".to_string())
}

/// Join addresses the way a header lists them, dropping blanks.
fn address_list(addresses: &[String]) -> String {
    addresses
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn build_message(from: Mailbox, email: &OutgoingEmail) -> Result<Message, SmtpError> {
    // Parse To addresses
    let to_addresses: Vec<Mailbox> = email
        .to
        .iter()
        .map(|addr| {
            addr.parse().map_err(|e| {
                SmtpError::InvalidAddress(format!("Invalid To address '{}': {}", addr, e))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Parse Cc addresses
    let cc_addresses: Vec<Mailbox> = email
        .cc
        .iter()
        .filter(|addr| !addr.is_empty())
        .map(|addr| {
            addr.parse().map_err(|e| {
                SmtpError::InvalidAddress(format!("Invalid Cc address '{}': {}", addr, e))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Parse Bcc addresses
    let bcc_addresses: Vec<Mailbox> = email
        .bcc
        .iter()
        .filter(|addr| !addr.is_empty())
        .map(|addr| {
            addr.parse().map_err(|e| {
                SmtpError::InvalidAddress(format!("Invalid Bcc address '{}': {}", addr, e))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Build message
    let mut builder = Message::builder().from(from).subject(&email.subject);

    // Add To recipients
    for to in to_addresses {
        builder = builder.to(to);
    }

    // Add Cc recipients
    for cc in cc_addresses {
        builder = builder.cc(cc);
    }

    // Add Bcc recipients
    for bcc in bcc_addresses {
        builder = builder.bcc(bcc);
    }

    // Add In-Reply-To header for threading
    if let Some(ref msg_id) = email.in_reply_to {
        builder = builder.in_reply_to(msg_id.to_string());
    }

    // Add References header for threading
    if let Some(ref refs) = email.references {
        builder = builder.references(refs.to_string());
    }

    if let Some(id) = &email.forwards {
        builder = builder.raw_header(raw(FORWARDED_HEADER, id));
    }

    if let Some(id) = &email.message_id {
        builder = builder.message_id(Some(id.clone()));
    }

    if let Some(reply_to) = &email.reply_to {
        let reply_to: Mailbox = reply_to.parse().map_err(|e| {
            SmtpError::InvalidAddress(format!("Invalid Reply-To address '{reply_to}': {e}"))
        })?;
        builder = builder.reply_to(reply_to);
    }

    builder
        .multipart(body_part(email)?)
        .map_err(|e| SmtpError::BuildError(e.to_string()))
}

/// The MIME body: the text, and any attachments beside it.
///
/// Shared by sending and by saving a draft, so a draft has the structure the
/// sent message will have. Only the headers around it differ.
fn body_part(email: &OutgoingEmail) -> Result<MultiPart, SmtpError> {
    // Convert line endings to CRLF (RFC 2822 requirement). A `--` on a line
    // of its own is a signature marker whose space was trimmed on the way.
    let body_crlf = email
        .body
        .lines()
        .map(|line| if line == "--" { "-- " } else { line })
        .collect::<Vec<_>>()
        .join("\r\n");

    // Build Content-Type with format=flowed (RFC 3676)
    // Vim's mail ftplugin with formatoptions+=w adds trailing spaces to wrapped lines,
    // which indicate soft line breaks that can be reflowed by the recipient's client.
    let content_type: ContentType = "text/plain; charset=utf-8; format=flowed".parse().unwrap();

    // The body always lives inside a multipart/alternative.
    let text = MultiPart::alternative()
        .singlepart(SinglePart::builder().header(content_type).body(body_crlf));

    if email.attachments.is_empty() {
        // No attachments: keep the simple alternative structure.
        return Ok(text);
    }

    // With attachments: wrap body + attachment parts in multipart/mixed.
    let mut mixed = MultiPart::mixed().multipart(text);
    for att in &email.attachments {
        // Fall back to a generic binary type when the declared content-type
        // is unparseable so a bad type never aborts the whole send.
        let ct: ContentType = att
            .content_type
            .parse()
            .unwrap_or_else(|_| "application/octet-stream".parse().unwrap());
        mixed = mixed.singlepart(Attachment::new(att.filename.clone()).body(att.bytes.clone(), ct));
    }
    Ok(mixed)
}

/// Send an email using SMTP and return the RFC822 message bytes for saving to Sent folder.
///
/// Returns the formatted RFC822 message bytes on success, which can be used
/// to save a copy to the Sent folder via IMAP APPEND.
pub async fn send_email(
    account: &AccountConfig,
    email: &OutgoingEmail,
    password: &SecretString,
) -> Result<Vec<u8>, SmtpError> {
    // Validate email first
    email.validate()?;

    let from: Mailbox = email
        .sender(account)
        .parse()
        .map_err(|e| SmtpError::InvalidAddress(format!("Invalid From address: {}", e)))?;

    // Build the MIME message (body + any attachments)
    let message = build_message(from, email)?;

    // Get the RFC822 formatted message bytes before sending
    let rfc822_bytes = message.formatted();

    // Create SMTP transport
    // Use expose_secret() to access password only when needed for authentication
    let creds = Credentials::new(
        account.smtp_username().to_string(),
        password.expose_secret().to_string(),
    );

    // TLS from the first byte on 465, STARTTLS on 587, as the config says
    // or the port implies; XOAUTH2 where the login is a token.
    let builder = match account.smtp_encryption() {
        crate::config::Encryption::Tls => {
            AsyncSmtpTransport::<Tokio1Executor>::relay(account.smtp_server())
        }
        crate::config::Encryption::Starttls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(account.smtp_server())
        }
    }
    .map_err(|e| SmtpError::SendError(format!("Failed to create SMTP transport: {}", e)))?;
    let mechanisms = match account.smtp_auth {
        crate::config::AuthMethod::Password => vec![Mechanism::Plain, Mechanism::Login],
        crate::config::AuthMethod::Xoauth2 => vec![Mechanism::Xoauth2],
    };
    let mailer: AsyncSmtpTransport<Tokio1Executor> = builder
        .credentials(creds)
        .authentication(mechanisms)
        .port(account.smtp_port())
        .timeout(Some(Duration::from_secs(SMTP_TIMEOUT_SECS)))
        .build();

    // Send the email
    mailer.send(message).await.map_err(classify_send_error)?;

    Ok(rfc822_bytes)
}

/// Tell a server that said no from a server that could not be reached.
///
/// A 5xx reply is an answer: the message, the recipient or the credentials
/// are the problem, and sending it again unchanged gets the same reply. A 4xx
/// reply is the server asking to be tried again later (RFC 5321 §4.2.1), and
/// is treated like the network — connection refused, a timeout, a TLS
/// failure, a connection that dropped mid-dialogue: the message deserves
/// another try.
///
/// lettre's `is_response` is not a negative reply but one it could not read,
/// which is also what a connection that closes mid-reply produces; the
/// negative replies are `is_permanent` and `is_transient`.
fn classify_send_error(error: lettre::transport::smtp::Error) -> SmtpError {
    let code = error.status().map(u16::from);
    if matches!(code, Some(534 | 535)) {
        SmtpError::AuthRefused(error.to_string())
    } else if error.is_permanent() || error.is_client() {
        SmtpError::SendError(error.to_string())
    } else {
        SmtpError::Unreachable(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailparse::MailHeaderMap;

    pub(super) fn draft_account() -> crate::config::AccountConfig {
        serde_saphyr::from_str(
            "name: Test\n\
             imap_server: imap.example.com\n\
             imap_port: 993\n\
             imap_username: user\n\
             imap_password_command: echo test\n\
             from: Me <me@example.com>\n",
        )
        .expect("the test account must describe a usable account")
    }

    pub(super) fn draft_email(to: Vec<&str>) -> OutgoingEmail {
        OutgoingEmail {
            to: to.into_iter().map(String::from).collect(),
            cc: vec![],
            bcc: vec![],
            subject: "Half written".to_string(),
            body: "Only the beginning".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments: vec![],
            message_id: None,
        }
    }

    /// The reported bug: a recipient that is still being typed.
    #[test]
    fn draft_keeps_an_unparseable_recipient() {
        let bytes = draft_bytes(&draft_account(), &draft_email(vec!["foo"]))
            .expect("a half-typed address must not stop a draft being saved");
        let parsed = mailparse::parse_mail(&bytes).expect("the draft must be a parseable message");
        assert_eq!(parsed.headers.get_first_value("To").as_deref(), Some("foo"));
    }

    /// The same bug one step earlier: nothing typed at all.
    #[test]
    fn draft_survives_having_no_recipient() {
        let bytes = draft_bytes(&draft_account(), &draft_email(vec![]))
            .expect("a draft with no recipient yet must still be savable");
        let parsed = mailparse::parse_mail(&bytes).expect("the draft must be a parseable message");
        assert!(parsed.headers.get_first_value("To").is_none());
        assert_eq!(
            parsed.headers.get_first_value("Subject").as_deref(),
            Some("Half written")
        );
    }

    /// What was typed is what comes back, so reopening a draft does not quietly
    /// correct it.
    #[test]
    fn draft_lists_recipients_as_typed() {
        let mut email = draft_email(vec!["foo", "Ada <ada@example.com>"]);
        email.cc = vec!["  ".to_string(), "cc@example.com".to_string()];
        let bytes = draft_bytes(&draft_account(), &email).unwrap();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        assert_eq!(
            parsed.headers.get_first_value("To").as_deref(),
            Some("foo, Ada <ada@example.com>")
        );
        // The blank entry is dropped rather than left as a stray comma.
        assert_eq!(
            parsed.headers.get_first_value("Cc").as_deref(),
            Some("cc@example.com")
        );
    }

    /// A draft is read back by the same code that reads any other message, so
    /// the body has to survive the round trip.
    #[test]
    fn draft_body_and_attachments_survive_the_round_trip() {
        let mut email = draft_email(vec!["foo"]);
        email.body = "First line\nSecond line".to_string();
        email.attachments = vec![EmailAttachment {
            filename: "notes.txt".to_string(),
            content_type: "text/plain".to_string(),
            bytes: b"attached".to_vec(),
        }];
        let bytes = draft_bytes(&draft_account(), &email).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("multipart/mixed"), "{text}");
        assert!(text.contains("First line"), "{text}");
        assert!(text.contains("notes.txt"), "{text}");
    }

    /// Subjects are still encoded by lettre; only the addresses go in verbatim.
    #[test]
    fn draft_encodes_a_non_ascii_subject() {
        let mut email = draft_email(vec![]);
        email.subject = "Grüezi".to_string();
        let bytes = draft_bytes(&draft_account(), &email).unwrap();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        assert_eq!(
            parsed.headers.get_first_value("Subject").as_deref(),
            Some("Grüezi")
        );
    }

    /// A Message-ID is what a reply is threaded onto, and it is added by lettre
    /// when it builds a message — so it has to be added here too.
    #[test]
    fn draft_carries_mime_version_and_a_message_id() {
        let bytes = draft_bytes(&draft_account(), &draft_email(vec!["foo"])).unwrap();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        assert_eq!(
            parsed.headers.get_first_value("MIME-Version").as_deref(),
            Some("1.0")
        );
        let id = parsed.headers.get_first_value("Message-ID").unwrap();
        assert!(id.starts_with('<') && id.ends_with("@example.com>"), "{id}");
    }

    /// An identity changes who the message is from and where answers go; the
    /// Message-ID stays the account's, like the Sent folder.
    #[test]
    fn a_draft_is_from_the_identity_it_was_written_as() {
        let mut email = draft_email(vec!["foo"]);
        email.from = Some("Example Support <support@other.example>".to_string());
        email.reply_to = Some("support@other.example".to_string());
        let bytes = draft_bytes(&draft_account(), &email).unwrap();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        let from = parsed.headers.get_first_value("From").unwrap();
        assert!(from.contains("support@other.example"), "{from}");
        assert_eq!(
            parsed.headers.get_first_value("Reply-To").as_deref(),
            Some("support@other.example")
        );
        let id = parsed.headers.get_first_value("Message-ID").unwrap();
        assert!(id.ends_with("@example.com>"), "{id}");
    }

    #[test]
    fn a_sent_message_carries_the_identity_reply_to() {
        let mut email = draft_email(vec!["ada@example.com"]);
        email.reply_to = Some("Support <support@example.com>".to_string());
        let from: Mailbox = "support@example.com".parse().unwrap();
        let bytes = build_message(from, &email).unwrap().formatted();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        assert_eq!(
            parsed.headers.get_first_value("Reply-To").as_deref(),
            Some("Support <support@example.com>")
        );
    }

    /// A forward names what it forwards, sent or saved, so the original can be
    /// marked once it goes — even from a draft reopened days later.
    #[test]
    fn a_forward_names_the_message_it_forwards() {
        let mut email = draft_email(vec!["ada@example.com"]);
        email.forwards = Some("<orig@example.com>".to_string());
        let draft = draft_bytes(&draft_account(), &email).unwrap();
        let from: Mailbox = "me@example.com".parse().unwrap();
        let sent = build_message(from, &email).unwrap().formatted();
        for bytes in [draft, sent] {
            assert_eq!(
                crate::draft::ParentHeaders::parse(&bytes)
                    .forwards
                    .as_deref(),
                Some("<orig@example.com>")
            );
        }
    }

    /// A Message-ID chosen beforehand is the one the message goes out with,
    /// which is what a follow-up task is linked by.
    #[test]
    fn a_message_is_sent_under_the_message_id_it_was_given() {
        let mut email = draft_email(vec!["ada@example.com"]);
        email.message_id = Some("<chosen@example.com>".to_string());
        let from: Mailbox = "me@example.com".parse().unwrap();
        let bytes = build_message(from, &email).unwrap().formatted();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        assert_eq!(
            parsed.headers.get_first_value("Message-ID").as_deref(),
            Some("<chosen@example.com>")
        );
    }

    #[test]
    fn message_id_domain_falls_back_when_there_is_no_domain() {
        assert_eq!(message_id_domain("Me <me@example.com>"), "example.com");
        assert_eq!(message_id_domain("me@example.com"), "example.com");
        assert_eq!(message_id_domain("nonsense"), "invalid");
    }

    /// A newline in a pasted address would end the header block and turn the
    /// rest of the draft into the body.
    #[test]
    fn draft_cannot_be_split_by_a_newline_in_an_address() {
        let bytes = draft_bytes(
            &draft_account(),
            &draft_email(vec!["foo\r\nSubject: injected"]),
        )
        .unwrap();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        assert_eq!(
            parsed.headers.get_first_value("Subject").as_deref(),
            Some("Half written")
        );
    }

    #[test]
    fn test_valid_email_addresses() {
        assert!(is_valid_email_address("user@example.com"));
        assert!(is_valid_email_address("user.name@example.com"));
        assert!(is_valid_email_address("user@sub.example.com"));
        assert!(is_valid_email_address("  user@example.com  ")); // trimmed
    }

    fn email_with_attachments(attachments: Vec<EmailAttachment>) -> OutgoingEmail {
        OutgoingEmail {
            to: vec!["bob@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Report".to_string(),
            body: "See attached.".to_string(),
            in_reply_to: None,
            references: None,
            forwards: None,
            from: None,
            reply_to: None,
            attachments,
            message_id: None,
        }
    }

    #[test]
    fn test_build_message_without_attachments_is_not_mixed() {
        let from: Mailbox = "alice@example.com".parse().unwrap();
        let email = email_with_attachments(vec![]);
        let message = build_message(from, &email).expect("message builds");
        let rendered = String::from_utf8(message.formatted()).unwrap();

        assert!(rendered.contains("multipart/alternative"));
        assert!(!rendered.contains("multipart/mixed"));
        assert!(!rendered.contains("Content-Disposition: attachment"));
    }

    #[test]
    fn test_build_message_with_attachment_renders_mixed_multipart() {
        let from: Mailbox = "alice@example.com".parse().unwrap();
        let email = email_with_attachments(vec![EmailAttachment {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            bytes: b"Hello PDF".to_vec(),
        }]);
        let message = build_message(from, &email).expect("message builds");
        let rendered = String::from_utf8(message.formatted()).unwrap();

        // The message must be multipart/mixed carrying the attachment part with
        // its filename, content type, and an attachment disposition.
        assert!(rendered.contains("multipart/mixed"));
        assert!(rendered.contains("application/pdf"));
        assert!(rendered.contains("Content-Disposition: attachment"));
        assert!(rendered.contains("report.pdf"));
        // The body must still be present alongside the attachment.
        assert!(rendered.contains("See attached."));
    }

    #[test]
    fn test_build_message_falls_back_to_octet_stream_for_bad_content_type() {
        let from: Mailbox = "alice@example.com".parse().unwrap();
        let email = email_with_attachments(vec![EmailAttachment {
            filename: "mystery.bin".to_string(),
            content_type: "not a mime type".to_string(),
            bytes: b"\x00\x01\x02".to_vec(),
        }]);
        let message = build_message(from, &email).expect("message builds despite bad type");
        let rendered = String::from_utf8(message.formatted()).unwrap();

        assert!(rendered.contains("application/octet-stream"));
        assert!(rendered.contains("mystery.bin"));
    }

    /// A whitespace-trimming editor turns `-- ` into `--`; the message
    /// still goes out with the marker a reader's client folds on.
    #[test]
    fn a_trimmed_signature_marker_goes_out_with_its_space() {
        let mut email = draft_email(vec!["ada@example.com"]);
        email.body = "Thanks!\n\n--\nSam\n".to_string();
        let bytes = draft_bytes(&draft_account(), &email).unwrap();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        let text = parsed.subparts[0].get_body().unwrap();
        assert!(text.contains("\r\n-- \r\nSam"), "got: {text:?}");
    }

    #[test]
    fn test_invalid_email_addresses() {
        assert!(!is_valid_email_address(""));
        assert!(!is_valid_email_address("user"));
        assert!(!is_valid_email_address("user@"));
        assert!(!is_valid_email_address("@example.com"));
        assert!(!is_valid_email_address("user@example")); // no dot in domain
        assert!(!is_valid_email_address("user@@example.com"));
        assert!(!is_valid_email_address("user@.example.com")); // leading dot
        assert!(!is_valid_email_address("user@example.com.")); // trailing dot
        assert!(!is_valid_email_address("user@example..com")); // consecutive dots
    }

    /// Send through a one-shot server on localhost that greets, offers AUTH
    /// and answers the login with `auth_reply` — or, given `None`, hangs up
    /// right after the greeting — and classify what lettre makes of it.
    async fn classify_against(auth_reply: Option<&'static str>) -> SmtpError {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();
            write.write_all(b"220 fake ESMTP\r\n").await.unwrap();
            let Some(auth_reply) = auth_reply else {
                return;
            };
            while let Ok(Some(line)) = lines.next_line().await {
                let reply = if line.starts_with("EHLO") {
                    "250-fake\r\n250 AUTH PLAIN LOGIN\r\n"
                } else if line.starts_with("AUTH") {
                    auth_reply
                } else {
                    "221 bye\r\n"
                };
                if write.write_all(reply.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let mailer: AsyncSmtpTransport<Tokio1Executor> =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
                .port(port)
                .credentials(Credentials::new("user".into(), "wrong".into()))
                .authentication(vec![Mechanism::Plain])
                .timeout(Some(Duration::from_secs(5)))
                .build();
        let message = Message::builder()
            .from("me@example.com".parse().unwrap())
            .to("you@example.com".parse().unwrap())
            .body(String::from("hello"))
            .unwrap();
        let error = mailer.send(message).await.expect_err("the send must fail");
        server.abort();
        classify_send_error(error)
    }

    #[tokio::test]
    async fn a_refused_login_is_an_answer_not_an_unreachable_server() {
        let error = classify_against(Some("535 5.7.8 Incorrect authentication data\r\n")).await;
        assert!(matches!(error, SmtpError::AuthRefused(_)), "{error:?}");
    }

    #[tokio::test]
    async fn another_permanent_reply_is_a_refusal() {
        let error = classify_against(Some("554 5.7.1 Relay access denied\r\n")).await;
        assert!(matches!(error, SmtpError::SendError(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_transient_reply_is_tried_again() {
        let error = classify_against(Some("454 4.7.0 Temporary authentication failure\r\n")).await;
        assert!(matches!(error, SmtpError::Unreachable(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_dropped_connection_is_tried_again() {
        let error = classify_against(None).await;
        assert!(matches!(error, SmtpError::Unreachable(_)), "{error:?}");
    }
}
