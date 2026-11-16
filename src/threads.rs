//! Conversations: how messages are grouped into threads, and how a threaded
//! list is laid out.
//!
//! A message joins the thread of the message it answers — named by
//! `In-Reply-To`, or by the last entry of `References` that is here — and
//! otherwise, when it is a reply by its subject, the thread of the oldest
//! message with the same subject. The grouping itself happens in SQL as
//! messages are stored (`Database::assign_thread_in_tx`); this module holds the
//! parts of it that are pure.

use std::collections::HashMap;

/// The headers of a message that threading is built from, as the sync fetches
/// them beside the envelope. `None` on a [`crate::models::Message`] means they
/// were never fetched, which is not the same as a message that has none.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadHeaders {
    pub in_reply_to: Option<String>,
    /// The `References` header as one line, identifiers separated by a space.
    pub references: Option<String>,
    pub list_id: Option<String>,
    /// The headers the highlight rules name, by lower-cased name, decoded:
    /// whatever else the sync was asked to fetch beside the three above.
    #[serde(default)]
    pub extra: Vec<(String, String)>,
}

impl ThreadHeaders {
    /// Read them off a header block, as `HEADER.FIELDS` returns it. Every
    /// header in the block that is not one of the three is kept in `extra`.
    pub fn parse(block: &[u8]) -> Self {
        let Ok((headers, _)) = mailparse::parse_headers(block) else {
            return Self::default();
        };
        let extra = headers
            .iter()
            .filter(|header| {
                !matches!(
                    header.get_key().to_ascii_lowercase().as_str(),
                    "in-reply-to" | "references" | "list-id"
                )
            })
            .map(|header| (header.get_key().to_ascii_lowercase(), header.get_value()))
            .collect();
        Self::from_parsed(&headers, extra)
    }

    fn from_parsed(headers: &[mailparse::MailHeader<'_>], extra: Vec<(String, String)>) -> Self {
        use mailparse::MailHeaderMap;
        let identifiers = |name: &str| {
            headers
                .get_first_value(name)
                .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|value| !value.is_empty())
        };
        Self {
            in_reply_to: identifiers("In-Reply-To"),
            references: identifiers("References"),
            list_id: headers
                .get_first_value("List-Id")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            extra,
        }
    }

    /// The headers as a block again, for a rule that reads header blocks:
    /// `List-Id` and the extra ones.
    pub fn as_block(&self) -> Vec<u8> {
        let mut block = String::new();
        if let Some(list_id) = &self.list_id {
            block.push_str(&format!("List-Id: {list_id}\r\n"));
        }
        for (name, value) in &self.extra {
            block.push_str(&format!("{name}: {value}\r\n"));
        }
        block.push_str("\r\n");
        block.into_bytes()
    }

    /// The identifiers of the messages this one answers, nearest first: the
    /// one in `In-Reply-To`, then `References` from its end.
    pub fn parents(&self) -> Vec<String> {
        let mut parents: Vec<String> = Vec::new();
        for id in self
            .in_reply_to
            .iter()
            .flat_map(|v| v.split_whitespace())
            .chain(
                self.references
                    .iter()
                    .flat_map(|v| v.split_whitespace().rev()),
            )
        {
            let id = id.trim();
            if id.starts_with('<') && id.ends_with('>') && !parents.iter().any(|p| p == id) {
                parents.push(id.to_string());
            }
        }
        parents
    }
}

/// A subject as threads compare it: without the reply and forward prefixes
/// and the list tags, lower-cased, whitespace folded. The second value says
/// whether a prefix was there — a message with `Re:` is a reply looking for
/// its thread; one without is a thread of its own until a reply arrives.
pub fn normalise_subject(subject: Option<&str>) -> (String, bool) {
    let mut rest = subject.unwrap_or("").trim();
    let mut was_reply = false;
    loop {
        let before = rest;
        // `Re:`, `Re[2]:`, `AW:`, `Fwd:`, `WG:`, `TR:` and the like.
        if let Some(colon) = rest.find(':')
            && colon <= 8
        {
            let prefix = rest[..colon].trim().to_ascii_lowercase();
            let prefix = prefix.split('[').next().unwrap_or("").trim();
            if matches!(
                prefix,
                "re" | "aw" | "sv" | "vs" | "antw" | "fw" | "fwd" | "wg" | "tr" | "rv" | "fs"
            ) {
                was_reply = true;
                rest = rest[colon + 1..].trim_start();
            }
        }
        // A list tag: `[orb-dev]`.
        if rest.starts_with('[')
            && let Some(close) = rest.find(']')
        {
            rest = rest[close + 1..].trim_start();
        }
        if rest == before {
            break;
        }
    }
    let folded = rest
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    (folded, was_reply)
}

/// A stored RFC 3339 date as an instant, for ordering; a date that will not
/// parse, or none, sorts first. The strings themselves do not order: one
/// kept with its own offset compares against the UTC ones by its digits.
pub fn date_key(date: Option<&str>) -> i64 {
    date.and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|parsed| parsed.timestamp())
        .unwrap_or(i64::MIN)
}

/// The thread a message founds when it answers nothing here: its own
/// Message-ID, or a name of its own for a message without one.
pub fn own_thread_id(row_id: i64, message_id: Option<&str>) -> String {
    match message_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => format!("local:{row_id}"),
    }
}

/// A message of a thread, as its depth is worked out.
pub struct ThreadMember<'a> {
    pub message_id: Option<&'a str>,
    pub in_reply_to: Option<&'a str>,
}

/// How deep each member of a thread sits: a reply to a member is one deeper
/// than it, anything else is at the root level. `members` are in the order
/// the list shows them, oldest first.
pub fn depths(members: &[ThreadMember<'_>]) -> Vec<u32> {
    let mut depth_of: HashMap<&str, u32> = HashMap::new();
    let mut depths = Vec::with_capacity(members.len());
    for member in members {
        let depth = member
            .in_reply_to
            .and_then(|parent| depth_of.get(parent).copied())
            .map(|parent| parent + 1)
            .unwrap_or(0);
        if let Some(id) = member.message_id {
            depth_of.entry(id).or_insert(depth);
        }
        depths.push(depth);
    }
    depths
}

/// An expanded thread's place in the threaded list: its rank among the
/// threads, and how many members it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expanded {
    pub rank: usize,
    pub members: usize,
}

/// Where a row of the flat, threaded list falls: in which thread (by rank)
/// and, inside an expanded thread, at which member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlatPosition {
    pub thread: usize,
    /// 0 for the thread's own row; members count from 1.
    pub member: usize,
}

/// The thread a flat row belongs to. Collapsed threads take one row each;
/// an expanded one takes one per member. `expanded` is sorted by rank.
pub fn locate(flat: usize, expanded: &[Expanded]) -> FlatPosition {
    let mut extra = 0;
    for e in expanded {
        let start = e.rank + extra;
        if flat < start {
            break;
        }
        let end = start + e.members;
        if flat < end {
            return FlatPosition {
                thread: e.rank,
                member: flat - start,
            };
        }
        extra += e.members - 1;
    }
    FlatPosition {
        thread: flat - extra,
        member: 0,
    }
}

/// How many rows the threaded list has, given the number of threads.
pub fn flat_total(threads: usize, expanded: &[Expanded]) -> usize {
    threads + expanded.iter().map(|e| e.members - 1).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subject_is_compared_without_its_prefixes_and_tags() {
        assert_eq!(
            normalise_subject(Some("Re: AW: [orb-dev]  Weekly   digest")),
            ("weekly digest".to_string(), true)
        );
        assert_eq!(
            normalise_subject(Some("Weekly digest")),
            ("weekly digest".to_string(), false)
        );
        assert_eq!(
            normalise_subject(Some("Re[2]: Hi")),
            ("hi".to_string(), true)
        );
        assert_eq!(normalise_subject(Some("Fwd: Hi")), ("hi".to_string(), true));
        assert_eq!(
            normalise_subject(Some("Invoice: October")),
            ("invoice: october".to_string(), false),
            "a colon in the subject is not a prefix"
        );
        assert_eq!(normalise_subject(None), (String::new(), false));
    }

    #[test]
    fn parents_are_named_nearest_first_without_repeats() {
        let headers = ThreadHeaders::parse(
            b"In-Reply-To: <b@x>\r\nReferences: <a@x>\r\n <b@x>\r\nList-Id: L <l.x>\r\n\
              X-Spam-Flag: YES\r\n\r\n",
        );
        assert_eq!(headers.references.as_deref(), Some("<a@x> <b@x>"));
        assert_eq!(headers.list_id.as_deref(), Some("L <l.x>"));
        assert_eq!(
            headers.extra,
            vec![("x-spam-flag".to_string(), "YES".to_string())]
        );
        assert_eq!(
            String::from_utf8(headers.as_block()).unwrap(),
            "List-Id: L <l.x>\r\nx-spam-flag: YES\r\n\r\n"
        );
        assert_eq!(headers.parents(), vec!["<b@x>", "<a@x>"]);
        assert!(ThreadHeaders::default().parents().is_empty());
    }

    #[test]
    fn depth_follows_the_reply_chain_within_the_thread() {
        let members = [
            ThreadMember {
                message_id: Some("<a>"),
                in_reply_to: None,
            },
            ThreadMember {
                message_id: Some("<b>"),
                in_reply_to: Some("<a>"),
            },
            ThreadMember {
                message_id: Some("<c>"),
                in_reply_to: Some("<b>"),
            },
            ThreadMember {
                message_id: Some("<d>"),
                in_reply_to: Some("<missing>"),
            },
            ThreadMember {
                message_id: None,
                in_reply_to: Some("<a>"),
            },
        ];
        assert_eq!(depths(&members), vec![0, 1, 2, 0, 1]);
    }

    #[test]
    fn a_flat_row_is_found_in_its_thread() {
        // Threads 0..5; thread 1 expanded with 3 members, thread 3 with 2.
        let expanded = [
            Expanded {
                rank: 1,
                members: 3,
            },
            Expanded {
                rank: 3,
                members: 2,
            },
        ];
        let at = |flat| locate(flat, &expanded);
        assert_eq!(
            at(0),
            FlatPosition {
                thread: 0,
                member: 0
            }
        );
        assert_eq!(
            at(1),
            FlatPosition {
                thread: 1,
                member: 0
            }
        );
        assert_eq!(
            at(2),
            FlatPosition {
                thread: 1,
                member: 1
            }
        );
        assert_eq!(
            at(3),
            FlatPosition {
                thread: 1,
                member: 2
            }
        );
        assert_eq!(
            at(4),
            FlatPosition {
                thread: 2,
                member: 0
            }
        );
        assert_eq!(
            at(5),
            FlatPosition {
                thread: 3,
                member: 0
            }
        );
        assert_eq!(
            at(6),
            FlatPosition {
                thread: 3,
                member: 1
            }
        );
        assert_eq!(
            at(7),
            FlatPosition {
                thread: 4,
                member: 0
            }
        );
        assert_eq!(flat_total(5, &expanded), 8);
        assert_eq!(
            locate(4, &[]),
            FlatPosition {
                thread: 4,
                member: 0
            }
        );
    }
}
