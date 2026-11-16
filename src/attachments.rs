//! MIME attachment enumeration.
//!
//! Given a message's raw RFC822 source, walk the MIME tree with `mailparse`
//! and report the attachments it contains. An "attachment" is a leaf MIME part
//! that either declares `Content-Disposition: attachment` or carries a filename
//! (via the disposition `filename` param or the content-type `name` param).
//! Inline parts with a filename (e.g. embedded images referenced by a
//! `Content-ID`) are reported too, flagged via [`AttachmentMeta::is_inline`].
//! The main `text/plain` / `text/html` body parts — which have no filename and
//! no attachment disposition — are excluded.
//!
//! A meeting request is the exception to "a filename or a disposition": its
//! invitation is a `text/calendar` alternative beside the plain and HTML text,
//! unmarked and unnamed, and it is reported as `invite.ics` — unless the
//! message also attaches the invitation as a named file, as some senders do,
//! which is then the one reported.

use mailparse::{DispositionType, MailHeaderMap, ParsedMail, parse_mail};

/// Metadata describing a single attachment discovered in a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    /// Suggested filename. Falls back to a generated name when the part
    /// declares an attachment disposition without an explicit filename.
    pub filename: String,
    /// MIME type of the part, e.g. `application/pdf` or `image/png`.
    pub content_type: String,
    /// Size in bytes of the *decoded* content.
    pub size: u64,
    /// Stable identifier for the part within the MIME tree: a dotted path of
    /// zero-based subpart indices from the root (e.g. `"1"`, `"1.0"`). An empty
    /// string denotes the root part itself (a single-part message that is
    /// entirely an attachment). Used to re-locate the part for extraction.
    pub part_id: String,
    /// True when the part is displayed inline (`Content-Disposition: inline`)
    /// rather than as a downloadable attachment.
    pub is_inline: bool,
    /// The `Content-ID` header value (angle brackets stripped), if present.
    /// Used to resolve `cid:` references from HTML bodies.
    pub content_id: Option<String>,
}

/// Parse `rfc822` and return metadata for every attachment part it contains.
///
/// Returns an empty vector for a plain-text (or otherwise attachment-free)
/// message, or when the bytes cannot be parsed as a MIME message.
pub fn list_attachments(rfc822: &[u8]) -> Vec<AttachmentMeta> {
    let Ok(parsed) = parse_mail(rfc822) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut invites = Vec::new();
    walk_part(&parsed, String::new(), &mut out, &mut invites);
    if !out.iter().any(is_calendar) {
        out.extend(invites);
    }
    out
}

/// Whether an attachment is a calendar file, by its type or its name.
fn is_calendar(a: &AttachmentMeta) -> bool {
    a.content_type == "text/calendar"
        || a.content_type == "application/ics"
        || a.filename.to_ascii_lowercase().ends_with(".ics")
}

/// True when the message contains at least one attachment.
pub fn has_attachments(rfc822: &[u8]) -> bool {
    !list_attachments(rfc822).is_empty()
}

/// The decoded contents of a single attachment part, ready to write to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedAttachment {
    /// Suggested filename for saving.
    pub filename: String,
    /// MIME type of the part, e.g. `application/pdf`.
    pub content_type: String,
    /// The part's content with its Content-Transfer-Encoding (base64 /
    /// quoted-printable) already decoded — the raw bytes of the file.
    pub data: Vec<u8>,
}

/// Re-parse `rfc822`, locate the part identified by `part_id` (the dotted path
/// produced by [`list_attachments`]), and return its decoded bytes together with
/// a filename and content type.
///
/// Returns `None` when the bytes cannot be parsed, the `part_id` does not
/// resolve to a part, or the part's body cannot be decoded.
pub fn extract_attachment(rfc822: &[u8], part_id: &str) -> Option<ExtractedAttachment> {
    let parsed = parse_mail(rfc822).ok()?;
    let part = find_part(&parsed, part_id)?;
    // `get_body_raw` applies the Content-Transfer-Encoding decoding
    // (base64 / quoted-printable), yielding the original file bytes.
    let data = part.get_body_raw().ok()?;
    let content_type = part.ctype.mimetype.to_lowercase();
    let filename = part_filename(part).unwrap_or_else(|| default_filename(&content_type, 0));
    Some(ExtractedAttachment {
        filename,
        content_type,
        data,
    })
}

/// Navigate to the sub-part addressed by `part_id`, a dotted path of zero-based
/// subpart indices from the root (matching the ids assigned in `walk_part`). An
/// empty `part_id` denotes the root part itself.
fn find_part<'a>(root: &'a ParsedMail<'a>, part_id: &str) -> Option<&'a ParsedMail<'a>> {
    if part_id.is_empty() {
        return Some(root);
    }
    let mut current = root;
    for segment in part_id.split('.') {
        let index: usize = segment.parse().ok()?;
        current = current.subparts.get(index)?;
    }
    Some(current)
}

/// Extract the filename for a part from either the Content-Disposition
/// `filename` param or the Content-Type `name` param.
fn part_filename(part: &ParsedMail) -> Option<String> {
    let disposition = part.get_content_disposition();
    if let Some(name) = disposition.params.get("filename") {
        return Some(name.clone());
    }
    part.ctype.params.get("name").cloned()
}

/// Recursively walk the MIME tree, collecting attachment parts. Attachment
/// parts are not descended into (a `message/rfc822` or otherwise structured
/// attachment is reported as a single unit). An unnamed calendar alternative
/// goes to `invites`, for `list_attachments` to weigh against the rest.
fn walk_part(
    part: &ParsedMail,
    path: String,
    out: &mut Vec<AttachmentMeta>,
    invites: &mut Vec<AttachmentMeta>,
) {
    let disposition = part.get_content_disposition();
    let is_attachment_disposition = disposition.disposition == DispositionType::Attachment;
    let filename = part_filename(part);

    // A part counts as an attachment when it declares an attachment
    // disposition or carries a filename. Body parts (text/plain, text/html)
    // have neither and fall through to the recursion below.
    if is_attachment_disposition || filename.is_some() {
        let content_type = part.ctype.mimetype.to_lowercase();
        let size = part.get_body_raw().map(|b| b.len() as u64).unwrap_or(0);
        let content_id = part
            .get_headers()
            .get_first_value("Content-ID")
            .map(|v| v.trim_matches(|c| c == '<' || c == '>').to_string())
            .filter(|v| !v.is_empty());

        let filename = filename.unwrap_or_else(|| default_filename(&content_type, out.len()));

        out.push(AttachmentMeta {
            filename,
            content_type,
            size,
            part_id: path,
            is_inline: disposition.disposition == DispositionType::Inline,
            content_id,
        });
        return;
    }

    if part.subparts.is_empty() && part.ctype.mimetype.eq_ignore_ascii_case("text/calendar") {
        invites.push(AttachmentMeta {
            filename: default_filename("text/calendar", 0),
            content_type: "text/calendar".to_string(),
            size: part.get_body_raw().map(|b| b.len() as u64).unwrap_or(0),
            part_id: path,
            is_inline: false,
            content_id: None,
        });
        return;
    }

    for (index, sub) in part.subparts.iter().enumerate() {
        let child_path = if path.is_empty() {
            index.to_string()
        } else {
            format!("{path}.{index}")
        };
        walk_part(sub, child_path, out, invites);
    }
}

/// Build a fallback filename for an attachment part that declares no name,
/// deriving a plausible extension from the MIME subtype.
fn default_filename(content_type: &str, index: usize) -> String {
    if content_type == "text/calendar" {
        return "invite.ics".to_string();
    }
    let ext = content_type
        .split_once('/')
        .map(|(_, sub)| sub)
        .filter(|sub| !sub.is_empty() && sub.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    format!("attachment-{}.{}", index + 1, ext)
}

/// Guess a MIME content type for an outgoing attachment from its filename
/// extension. Used when the app attaches a file the user picked from disk and
/// only the path is known. Falls back to `application/octet-stream` for unknown
/// or missing extensions so every attachment carries a valid content type.
pub fn guess_content_type(filename: &str) -> String {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let mime = match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" | "gzip" => "application/gzip",
        "tar" => "application/x-tar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    };
    mime.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRLF: &str = "\r\n";

    fn multipart_with_pdf() -> Vec<u8> {
        // A multipart/mixed message with a plain-text body and one PDF
        // attachment. The PDF payload is base64 "Hello PDF" -> "SGVsbG8gUERG".
        [
            "From: alice@example.com",
            "To: bob@example.com",
            "Subject: Report",
            "MIME-Version: 1.0",
            "Content-Type: multipart/mixed; boundary=\"BOUND\"",
            "",
            "--BOUND",
            "Content-Type: text/plain; charset=utf-8",
            "",
            "See attached.",
            "--BOUND",
            "Content-Type: application/pdf; name=\"report.pdf\"",
            "Content-Disposition: attachment; filename=\"report.pdf\"",
            "Content-Transfer-Encoding: base64",
            "",
            "SGVsbG8gUERG",
            "--BOUND--",
            "",
        ]
        .join(CRLF)
        .into_bytes()
    }

    #[test]
    fn enumerates_attachment_metadata() {
        let atts = list_attachments(&multipart_with_pdf());
        assert_eq!(atts.len(), 1);
        let a = &atts[0];
        assert_eq!(a.filename, "report.pdf");
        assert_eq!(a.content_type, "application/pdf");
        // Decoded "Hello PDF" is 9 bytes.
        assert_eq!(a.size, 9);
        assert_eq!(a.part_id, "1");
        assert!(!a.is_inline);
        assert!(a.content_id.is_none());
    }

    #[test]
    fn has_attachments_true_for_multipart_with_attachment() {
        assert!(has_attachments(&multipart_with_pdf()));
    }

    #[test]
    fn plain_text_message_has_no_attachments() {
        let msg = [
            "From: alice@example.com",
            "To: bob@example.com",
            "Subject: Hi",
            "Content-Type: text/plain; charset=utf-8",
            "",
            "Just a note, no attachments here.",
            "",
        ]
        .join(CRLF)
        .into_bytes();

        assert!(list_attachments(&msg).is_empty());
        assert!(!has_attachments(&msg));
    }

    #[test]
    fn alternative_body_parts_are_not_attachments() {
        let msg = [
            "From: alice@example.com",
            "Subject: Newsletter",
            "MIME-Version: 1.0",
            "Content-Type: multipart/alternative; boundary=\"ALT\"",
            "",
            "--ALT",
            "Content-Type: text/plain; charset=utf-8",
            "",
            "Plain version.",
            "--ALT",
            "Content-Type: text/html; charset=utf-8",
            "",
            "<p>HTML version.</p>",
            "--ALT--",
            "",
        ]
        .join(CRLF)
        .into_bytes();

        assert!(list_attachments(&msg).is_empty());
    }

    #[test]
    fn a_meeting_request_s_calendar_alternative_is_reported_as_invite_ics() {
        let msg = [
            "From: alice@example.com",
            "Subject: Sync",
            "MIME-Version: 1.0",
            "Content-Type: multipart/alternative; boundary=\"ALT\"",
            "",
            "--ALT",
            "Content-Type: text/plain; charset=utf-8",
            "",
            "A meeting.",
            "--ALT",
            "Content-Type: text/html; charset=utf-8",
            "",
            "<p>A meeting.</p>",
            "--ALT",
            "Content-Type: text/calendar; charset=utf-8; method=REQUEST",
            "",
            "BEGIN:VCALENDAR",
            "END:VCALENDAR",
            "--ALT--",
            "",
        ]
        .join(CRLF)
        .into_bytes();

        let atts = list_attachments(&msg);
        assert_eq!(atts.len(), 1, "the plain and HTML text stay the body");
        assert_eq!(atts[0].filename, "invite.ics");
        assert_eq!(atts[0].content_type, "text/calendar");
        assert_eq!(atts[0].part_id, "2");
        assert!(!atts[0].is_inline);
        assert!(has_attachments(&msg));

        let file = extract_attachment(&msg, "2").expect("extract");
        assert_eq!(file.filename, "invite.ics");
        assert!(String::from_utf8_lossy(&file.data).starts_with("BEGIN:VCALENDAR"));
    }

    #[test]
    fn an_invitation_also_attached_by_name_is_reported_once() {
        let msg = [
            "Subject: Sync",
            "MIME-Version: 1.0",
            "Content-Type: multipart/mixed; boundary=\"MIX\"",
            "",
            "--MIX",
            "Content-Type: multipart/alternative; boundary=\"ALT\"",
            "",
            "--ALT",
            "Content-Type: text/plain; charset=utf-8",
            "",
            "A meeting.",
            "--ALT",
            "Content-Type: text/calendar; charset=utf-8; method=REQUEST",
            "",
            "BEGIN:VCALENDAR",
            "END:VCALENDAR",
            "--ALT--",
            "--MIX",
            "Content-Type: application/ics; name=\"meeting.ics\"",
            "Content-Disposition: attachment; filename=\"meeting.ics\"",
            "",
            "BEGIN:VCALENDAR",
            "END:VCALENDAR",
            "--MIX--",
            "",
        ]
        .join(CRLF)
        .into_bytes();

        let atts = list_attachments(&msg);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "meeting.ics");
    }

    #[test]
    fn inline_image_is_reported_with_content_id() {
        // multipart/related body with an inline image referenced by cid.
        let msg = [
            "From: alice@example.com",
            "Subject: Photo",
            "MIME-Version: 1.0",
            "Content-Type: multipart/related; boundary=\"REL\"",
            "",
            "--REL",
            "Content-Type: text/html; charset=utf-8",
            "",
            "<img src=\"cid:img1\">",
            "--REL",
            "Content-Type: image/png; name=\"pixel.png\"",
            "Content-Disposition: inline; filename=\"pixel.png\"",
            "Content-Transfer-Encoding: base64",
            "Content-ID: <img1>",
            "",
            "aGVsbG8=",
            "--REL--",
            "",
        ]
        .join(CRLF)
        .into_bytes();

        let atts = list_attachments(&msg);
        assert_eq!(atts.len(), 1);
        let a = &atts[0];
        assert_eq!(a.filename, "pixel.png");
        assert_eq!(a.content_type, "image/png");
        assert!(a.is_inline);
        assert_eq!(a.content_id.as_deref(), Some("img1"));
        assert_eq!(a.part_id, "1");
    }

    #[test]
    fn multiple_attachments_get_distinct_part_ids() {
        let msg = [
            "Subject: Two files",
            "MIME-Version: 1.0",
            "Content-Type: multipart/mixed; boundary=\"B\"",
            "",
            "--B",
            "Content-Type: text/plain",
            "",
            "body",
            "--B",
            "Content-Type: text/plain; name=\"a.txt\"",
            "Content-Disposition: attachment; filename=\"a.txt\"",
            "",
            "aaa",
            "--B",
            "Content-Type: text/plain; name=\"b.txt\"",
            "Content-Disposition: attachment; filename=\"b.txt\"",
            "",
            "bbbb",
            "--B--",
            "",
        ]
        .join(CRLF)
        .into_bytes();

        let atts = list_attachments(&msg);
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].filename, "a.txt");
        assert_eq!(atts[0].part_id, "1");
        assert_eq!(atts[1].filename, "b.txt");
        assert_eq!(atts[1].part_id, "2");
    }

    #[test]
    fn unparseable_bytes_yield_no_attachments() {
        assert!(list_attachments(&[]).is_empty());
    }

    #[test]
    fn extract_attachment_returns_decoded_bytes() {
        // The PDF part is base64 "SGVsbG8gUERG" == "Hello PDF"; extraction must
        // round-trip to those exact decoded bytes.
        let rfc822 = multipart_with_pdf();

        // Confirm the part id we extract matches what enumeration reports.
        let atts = list_attachments(&rfc822);
        assert_eq!(atts[0].part_id, "1");

        let extracted = extract_attachment(&rfc822, "1").expect("part 1 must resolve");
        assert_eq!(extracted.data, b"Hello PDF");
        assert_eq!(extracted.filename, "report.pdf");
        assert_eq!(extracted.content_type, "application/pdf");
    }

    #[test]
    fn extract_attachment_decodes_quoted_printable() {
        // A quoted-printable text attachment: "=C3=A9" decodes to "é".
        let msg = [
            "Subject: QP",
            "MIME-Version: 1.0",
            "Content-Type: multipart/mixed; boundary=\"B\"",
            "",
            "--B",
            "Content-Type: text/plain",
            "",
            "body",
            "--B",
            "Content-Type: text/plain; name=\"note.txt\"",
            "Content-Disposition: attachment; filename=\"note.txt\"",
            "Content-Transfer-Encoding: quoted-printable",
            "",
            "caf=C3=A9",
            "--B--",
            "",
        ]
        .join(CRLF)
        .into_bytes();

        let extracted = extract_attachment(&msg, "1").expect("part 1 must resolve");
        assert_eq!(extracted.data, "café".as_bytes());
        assert_eq!(extracted.filename, "note.txt");
    }

    #[test]
    fn extract_attachment_unknown_part_id_returns_none() {
        let rfc822 = multipart_with_pdf();
        assert!(extract_attachment(&rfc822, "9").is_none());
        assert!(extract_attachment(&rfc822, "not-a-number").is_none());
    }

    #[test]
    fn guess_content_type_maps_known_extensions() {
        assert_eq!(guess_content_type("report.pdf"), "application/pdf");
        assert_eq!(guess_content_type("photo.PNG"), "image/png");
        assert_eq!(guess_content_type("pic.jpeg"), "image/jpeg");
        assert_eq!(guess_content_type("notes.txt"), "text/plain");
        assert_eq!(
            guess_content_type("/some/dir/archive.zip"),
            "application/zip"
        );
    }

    #[test]
    fn guess_content_type_falls_back_to_octet_stream() {
        assert_eq!(
            guess_content_type("mystery.xyzzy"),
            "application/octet-stream"
        );
        assert_eq!(
            guess_content_type("noextension"),
            "application/octet-stream"
        );
    }
}
