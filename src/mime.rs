//! Parsing a message's MIME tree, with a bound on how much tree there is.
//!
//! `mailparse` descends into every `multipart/*` part with a recursive call
//! and no limit, so a sender decides how deep the parse goes. A few hundred
//! kilobytes of nested boundaries overflow a worker thread's stack, and a
//! stack overflow is an abort, not an error — the whole process, once on the
//! message's arrival and again on every open. Every parse of a message goes
//! through [`parse_mail`], which refuses a message with more multipart parts
//! than [`MAX_MULTIPARTS`] before `mailparse` sees it.

use mailparse::{MailParseError, ParsedMail};

/// The most `multipart/*` parts a message may declare. A message with a text
/// body, an HTML body, a signature and a dozen attachments declares three or
/// four; nothing anybody writes comes near this.
pub const MAX_MULTIPARTS: usize = 64;

/// What a message over the bound is refused with.
pub const TOO_MANY_PARTS: &str = "the message declares more MIME parts than Orb reads";

/// Parse `bytes` as a message, as `mailparse::parse_mail` does, refusing one
/// that declares more than [`MAX_MULTIPARTS`] multipart parts.
pub fn parse_mail(bytes: &[u8]) -> Result<ParsedMail<'_>, MailParseError> {
    if multiparts_declared(bytes) > MAX_MULTIPARTS {
        return Err(MailParseError::Generic(TOO_MANY_PARTS));
    }
    mailparse::parse_mail(bytes)
}

/// How many times the message declares a `multipart/` type, case aside.
///
/// Every level of nesting is one declaration, so this bounds the depth of
/// the tree from above without building any of it. Counted over the whole
/// message, not the headers alone, since each nested part carries its own.
fn multiparts_declared(bytes: &[u8]) -> usize {
    const NEEDLE: &[u8] = b"multipart/";
    bytes
        .windows(NEEDLE.len())
        .filter(|window| window.eq_ignore_ascii_case(NEEDLE))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(depth: usize) -> Vec<u8> {
        let mut message = String::from("From: a@example.com\r\nSubject: x\r\n");
        for level in 0..depth {
            message.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=b{level}\r\n\r\n--b{level}\r\n"
            ));
        }
        message.push_str("Content-Type: text/plain\r\n\r\nhi\r\n");
        message.into_bytes()
    }

    #[test]
    fn an_ordinary_message_parses() {
        let message = nested(3);
        let parsed = parse_mail(&message).unwrap();
        assert_eq!(parsed.ctype.mimetype, "multipart/mixed");
        assert_eq!(parsed.subparts.len(), 1);
    }

    /// Five thousand levels overflow a two-megabyte stack; the parse must
    /// refuse them before descending.
    #[test]
    fn a_message_nested_past_the_bound_is_refused_not_parsed() {
        let message = nested(5000);
        let error = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || parse_mail(&message).map(|_| ()).unwrap_err().to_string())
            .unwrap()
            .join()
            .unwrap();
        assert!(error.contains(TOO_MANY_PARTS), "{error}");
    }

    #[test]
    fn the_bound_is_on_declarations_and_case_does_not_matter() {
        assert_eq!(
            multiparts_declared(b"Content-Type: MULTIPART/Alternative"),
            1
        );
        assert_eq!(multiparts_declared(&nested(MAX_MULTIPARTS)), MAX_MULTIPARTS);
        assert!(parse_mail(&nested(MAX_MULTIPARTS)).is_ok());
        assert!(parse_mail(&nested(MAX_MULTIPARTS + 1)).is_err());
    }
}
