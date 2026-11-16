//! Reading a message's shape from its `BODYSTRUCTURE`.
//!
//! `BODYSTRUCTURE` is MIME *metadata*: the server describes the part tree
//! without sending any content. Asking for it during the normal header sync
//! costs no extra round trip and no body bytes, and answers the two questions
//! that otherwise require downloading the whole message:
//!
//! - which part holds the readable text, so the body worker can fetch that part
//!   alone instead of the message and its attachments;
//! - whether there are attachments at all.
//!
//! Part numbering follows RFC 3501 §6.4.5: a non-multipart message is part `1`,
//! the children of a multipart are `1`, `2`, …, and nesting appends (`1.2.1`).

use async_imap::imap_proto::{BodyStructure, ContentEncoding, ContentType};

/// What a message is made of, as far as the server will tell us for free.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageShape {
    /// Part path of the `text/plain` body, if there is one.
    pub text_part: Option<String>,
    /// Part path of the `text/html` body, if there is one.
    pub html_part: Option<String>,
    /// Size in bytes the server reports for the chosen text part.
    pub text_bytes: Option<u32>,
    /// Transfer encoding of the chosen text part.
    ///
    /// Knowing it up front means the worker can fetch the part body alone and
    /// decode locally, instead of also fetching its MIME headers to find out.
    pub text_encoding: Option<String>,
    /// True when any part looks like an attachment rather than a body.
    pub has_attachments: bool,
}

/// A part is an attachment when it says so, or when it is a leaf that is
/// neither text nor part of the alternative/related body machinery.
///
/// Inline images referenced by HTML (`Content-Disposition: inline`) are not
/// attachments for display purposes, so only an explicit `attachment`
/// disposition or a non-text leaf counts.
fn is_attachment(ty: &ContentType<'_>, disposition: Option<&str>) -> bool {
    if let Some(d) = disposition {
        if d.eq_ignore_ascii_case("attachment") {
            return true;
        }
        if d.eq_ignore_ascii_case("inline") {
            return false;
        }
    }
    !ty.ty.eq_ignore_ascii_case("text") && !ty.ty.eq_ignore_ascii_case("multipart")
}

/// Extension to a part path: `""` -> `"1"`, `"1"` -> `"1.2"`.
fn child_path(prefix: &str, index: usize) -> String {
    if prefix.is_empty() {
        index.to_string()
    } else {
        format!("{prefix}.{index}")
    }
}

/// The wire name of a transfer encoding, as it appears in a MIME header.
fn encoding_name(encoding: &ContentEncoding<'_>) -> String {
    match encoding {
        ContentEncoding::SevenBit => "7bit".to_string(),
        ContentEncoding::EightBit => "8bit".to_string(),
        ContentEncoding::Binary => "binary".to_string(),
        ContentEncoding::Base64 => "base64".to_string(),
        ContentEncoding::QuotedPrintable => "quoted-printable".to_string(),
        ContentEncoding::Other(other) => other.to_string(),
    }
}

fn disposition_of(common: &async_imap::imap_proto::BodyContentCommon<'_>) -> Option<String> {
    common.disposition.as_ref().map(|d| d.ty.to_string())
}

fn walk(node: &BodyStructure<'_>, path: &str, shape: &mut MessageShape) {
    match node {
        BodyStructure::Text { common, other, .. } => {
            // A non-multipart message has no path of its own yet; it is part 1.
            let part = if path.is_empty() {
                "1".to_string()
            } else {
                path.to_string()
            };
            let disposition = disposition_of(common);

            if is_attachment(&common.ty, disposition.as_deref()) {
                // A text/* part explicitly marked as an attachment — a .txt or
                // .csv file, not the message body.
                shape.has_attachments = true;
                return;
            }
            // A meeting request carries its invitation as a text/calendar
            // alternative beside the plain and HTML text, unmarked and
            // unnamed; it is the file the reader offers (`invite.ics`).
            if common.ty.subtype.eq_ignore_ascii_case("calendar") {
                shape.has_attachments = true;
                return;
            }

            // First of each kind wins: servers list the least-preferred
            // alternative first, but the plain part is the one worth indexing
            // and the first is the simplest to reason about.
            if common.ty.subtype.eq_ignore_ascii_case("plain") && shape.text_part.is_none() {
                shape.text_part = Some(part);
                shape.text_bytes = Some(other.octets);
                shape.text_encoding = Some(encoding_name(&other.transfer_encoding));
            } else if common.ty.subtype.eq_ignore_ascii_case("html") && shape.html_part.is_none() {
                shape.html_part = Some(part);
            }
        }

        BodyStructure::Basic { common, .. } => {
            if is_attachment(&common.ty, disposition_of(common).as_deref()) {
                shape.has_attachments = true;
            }
        }

        // An embedded message (message/rfc822). Its own parts are addressable,
        // but treating a forwarded mail as an attachment matches what a user
        // expects to see.
        BodyStructure::Message { .. } => {
            shape.has_attachments = true;
        }

        BodyStructure::Multipart { bodies, .. } => {
            for (i, child) in bodies.iter().enumerate() {
                walk(child, &child_path(path, i + 1), shape);
            }
        }
    }
}

/// Read a message's shape from its `BODYSTRUCTURE`.
pub fn shape_of(structure: &BodyStructure<'_>) -> MessageShape {
    let mut shape = MessageShape::default();
    walk(structure, "", &mut shape);
    shape
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_imap::imap_proto::{BodyContentCommon, BodyContentSinglePart, ContentType};
    use std::borrow::Cow;

    fn content_type(ty: &'static str, subtype: &'static str) -> ContentType<'static> {
        ContentType {
            ty: Cow::Borrowed(ty),
            subtype: Cow::Borrowed(subtype),
            params: None,
        }
    }

    fn common(ty: &'static str, subtype: &'static str) -> BodyContentCommon<'static> {
        BodyContentCommon {
            ty: content_type(ty, subtype),
            disposition: None,
            language: None,
            location: None,
        }
    }

    fn single(octets: u32) -> BodyContentSinglePart<'static> {
        BodyContentSinglePart {
            id: None,
            md5: None,
            description: None,
            transfer_encoding: ContentEncoding::SevenBit,
            octets,
        }
    }

    fn text(subtype: &'static str, octets: u32) -> BodyStructure<'static> {
        BodyStructure::Text {
            common: common("text", subtype),
            other: single(octets),
            lines: 1,
            extension: None,
        }
    }

    fn attachment() -> BodyStructure<'static> {
        let mut c = common("application", "pdf");
        c.disposition = Some(async_imap::imap_proto::ContentDisposition {
            ty: Cow::Borrowed("attachment"),
            params: None,
        });
        BodyStructure::Basic {
            common: c,
            other: single(999),
            extension: None,
        }
    }

    fn multipart(
        subtype: &'static str,
        bodies: Vec<BodyStructure<'static>>,
    ) -> BodyStructure<'static> {
        BodyStructure::Multipart {
            common: common("multipart", subtype),
            bodies,
            extension: None,
        }
    }

    #[test]
    fn plain_only_message_is_part_one() {
        let shape = shape_of(&text("plain", 120));
        assert_eq!(shape.text_part.as_deref(), Some("1"));
        assert_eq!(shape.text_bytes, Some(120));
        assert_eq!(shape.text_encoding.as_deref(), Some("7bit"));
        assert!(shape.html_part.is_none());
        assert!(!shape.has_attachments);
    }

    #[test]
    fn alternative_finds_both_bodies() {
        let shape = shape_of(&multipart(
            "alternative",
            vec![text("plain", 10), text("html", 20)],
        ));
        assert_eq!(shape.text_part.as_deref(), Some("1"));
        assert_eq!(shape.html_part.as_deref(), Some("2"));
        assert!(!shape.has_attachments);
    }

    #[test]
    fn nested_alternative_inside_mixed_is_addressed_by_path() {
        // The common shape for "message with an attachment": mixed[ alternative[
        // plain, html ], pdf ].
        let shape = shape_of(&multipart(
            "mixed",
            vec![
                multipart("alternative", vec![text("plain", 10), text("html", 20)]),
                attachment(),
            ],
        ));
        assert_eq!(shape.text_part.as_deref(), Some("1.1"));
        assert_eq!(shape.html_part.as_deref(), Some("1.2"));
        assert!(shape.has_attachments);
    }

    #[test]
    fn html_only_message_has_no_text_part() {
        let shape = shape_of(&multipart("alternative", vec![text("html", 20)]));
        assert!(shape.text_part.is_none());
        assert_eq!(shape.html_part.as_deref(), Some("1"));
    }

    #[test]
    fn a_text_part_marked_as_an_attachment_is_not_the_body() {
        let mut c = common("text", "plain");
        c.disposition = Some(async_imap::imap_proto::ContentDisposition {
            ty: Cow::Borrowed("attachment"),
            params: None,
        });
        let csv = BodyStructure::Text {
            common: c,
            other: single(50),
            lines: 3,
            extension: None,
        };
        let shape = shape_of(&multipart("mixed", vec![text("plain", 10), csv]));

        assert_eq!(shape.text_part.as_deref(), Some("1"), "the real body wins");
        assert!(shape.has_attachments, "the attached .txt still counts");
    }

    #[test]
    fn inline_image_is_not_an_attachment() {
        let mut c = common("image", "png");
        c.disposition = Some(async_imap::imap_proto::ContentDisposition {
            ty: Cow::Borrowed("inline"),
            params: None,
        });
        let inline_image = BodyStructure::Basic {
            common: c,
            other: single(400),
            extension: None,
        };
        let shape = shape_of(&multipart("related", vec![text("html", 20), inline_image]));
        assert!(
            !shape.has_attachments,
            "an image referenced by the HTML body is not an attachment"
        );
    }

    #[test]
    fn a_meeting_request_s_calendar_alternative_counts_as_an_attachment() {
        let shape = shape_of(&multipart(
            "alternative",
            vec![text("plain", 10), text("html", 20), text("calendar", 900)],
        ));
        assert_eq!(shape.text_part.as_deref(), Some("1"));
        assert_eq!(shape.html_part.as_deref(), Some("2"));
        assert!(shape.has_attachments);
    }

    #[test]
    fn undisposed_binary_leaf_counts_as_an_attachment() {
        // Plenty of servers omit Content-Disposition entirely.
        let bare = BodyStructure::Basic {
            common: common("application", "zip"),
            other: single(1000),
            extension: None,
        };
        let shape = shape_of(&multipart("mixed", vec![text("plain", 10), bare]));
        assert!(shape.has_attachments);
    }
}
