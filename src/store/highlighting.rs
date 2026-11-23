//! Finding what the highlight rules of [`crate::highlights`] need to know
//! about a message, and evaluating them over a page, a folder, or the unread
//! mail of every folder.

use std::collections::{HashMap, HashSet};

use rusqlite::params_from_iter;

use super::Store;
use crate::Result;
use crate::db::Database;
use crate::highlights::{HighlightFacts, HighlightSet};
use crate::models::MessageListItem;
use crate::rules::MessageFacts;

/// Rows per query when a whole folder is walked.
const HIGHLIGHT_PAGE: usize = 2_000;

/// Ids per statement, below what SQLite lets one bind.
const HIGHLIGHT_CHUNK: usize = 500;

/// Bytes of a stored source read for its headers. A header past the cut is
/// not seen.
const HEADER_BLOCK_LIMIT: usize = 32 * 1024;

/// How many messages of a folder a rule matches, for `:highlights`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HighlightCount {
    pub rule: String,
    pub count: u32,
}

/// A rule's unread messages in one folder, for the sidebar.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HighlightBadge {
    pub account: String,
    pub folder: String,
    pub rule: String,
    pub count: u32,
}

impl Store {
    /// The highlight rules each of `items` matches, by message id. Items that
    /// match nothing are absent.
    pub fn highlights_for(
        &self,
        account: &str,
        folder: &str,
        items: &[MessageListItem],
    ) -> Result<HashMap<i64, Vec<String>>> {
        self.rules_matching(&self.config().highlights, account, folder, items)
    }

    /// The rules of `set` each of `items` matches, by message id.
    fn rules_matching(
        &self,
        set: &HighlightSet,
        account: &str,
        folder: &str,
        items: &[MessageListItem],
    ) -> Result<HashMap<i64, Vec<String>>> {
        if items.is_empty() || set.for_account(account).is_empty() {
            return Ok(HashMap::new());
        }
        let db = self.reader()?;
        let ids: Vec<i64> = items.iter().map(|item| item.id).collect();
        let with_attachments = db.messages_with_attachments(&ids)?;
        let body_hits = body_hits(&db, &ids, &set.body_needles())?;
        let headers = if set.needs_headers() {
            stored_headers(&db, &ids)?
        } else {
            HashMap::new()
        };
        let now = chrono::Utc::now();
        let none = HashSet::new();

        let mut matched = HashMap::new();
        for item in items {
            let mut base = message_facts(item, with_attachments.contains(&item.id));
            for block in headers.get(&item.id).into_iter().flatten() {
                base.add_header_block(block);
            }
            let facts = HighlightFacts {
                base: &base,
                unread: !has_flag(&item.flags, "Seen"),
                flagged: has_flag(&item.flags, "Flagged"),
                folder,
                age_seconds: age_seconds(item.date.as_deref(), now),
                body_hits: body_hits.get(&item.id).unwrap_or(&none),
            };
            let names = set.matching(account, &facts);
            if !names.is_empty() {
                matched.insert(item.id, names);
            }
        }
        Ok(matched)
    }

    /// The highlight rules one message matches, given its folder.
    pub fn highlights_for_message(&self, message_id: i64) -> Result<Vec<String>> {
        self.rules_matching_message(&self.config().highlights, message_id)
    }

    /// The first `load_remote_images` rule one message matches, if any.
    pub fn remote_images_rule_for_message(&self, message_id: i64) -> Result<Option<String>> {
        let config = self.config();
        Ok(self
            .rules_matching_message(&config.load_remote_images.0, message_id)?
            .into_iter()
            .next())
    }

    fn rules_matching_message(&self, set: &HighlightSet, message_id: i64) -> Result<Vec<String>> {
        if set.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.reader()?;
        let Some(message) = db.get_message_by_id(message_id)? else {
            return Ok(Vec::new());
        };
        let Some(folder) = db.get_folder_by_id(message.folder_id)? else {
            return Ok(Vec::new());
        };
        let item = MessageListItem {
            id: message_id,
            folder_id: message.folder_id,
            uid: message.uid,
            message_id: message.message_id,
            subject: message.subject,
            from: message.from,
            to: message.to,
            cc: message.cc,
            date: message.date,
            size: message.size,
            flags: message.flags,
            modseq: message.modseq,
        };
        Ok(self
            .rules_matching(set, &folder.account_name, &folder.name, &[item])?
            .remove(&message_id)
            .unwrap_or_default())
    }

    /// Which of the messages `ids` match a highlight rule with `breaks_focus`:
    /// the mail a focus announces all the same.
    pub fn focus_breakers(&self, ids: &[i64]) -> Result<HashSet<i64>> {
        let set = self.config().highlights.breaking_focus();
        let mut breakers = HashSet::new();
        if set.is_empty() {
            return Ok(breakers);
        }
        for &id in ids {
            if !self.rules_matching_message(&set, id)?.is_empty() {
                breakers.insert(id);
            }
        }
        Ok(breakers)
    }

    /// The unread mail in the inboxes that matches a rule with
    /// `breaks_focus`: what the Dock badge counts during a focus that shows
    /// the exceptions only.
    pub fn focus_breaker_count(&self) -> Result<u32> {
        let config = self.config();
        let set = config.highlights.breaking_focus();
        if set.is_empty() {
            return Ok(0);
        }
        let db = self.reader()?;
        let mut count = 0;
        for account in &config.accounts {
            for folder in db.get_folders_by_account(&account.name)? {
                let Some(folder_id) = folder.id else { continue };
                if !folder.name.eq_ignore_ascii_case("INBOX") {
                    continue;
                }
                let unread = db.unread_message_list(folder_id)?;
                count += self
                    .rules_matching(&set, &account.name, &folder.name, &unread)?
                    .len() as u32;
            }
        }
        Ok(count)
    }

    /// How many messages of a folder each rule matches, in rule order, for
    /// `:highlights`. Walks the whole folder, so it is a diagnostic, not a
    /// thing to call per render.
    pub fn highlight_counts(&self, account: &str, folder: &str) -> Result<Vec<HighlightCount>> {
        let config = self.config();
        let rules = config.highlights.for_account(account);
        let mut counts: Vec<HighlightCount> = rules
            .iter()
            .map(|rule| HighlightCount {
                rule: rule.name.clone(),
                count: 0,
            })
            .collect();
        if counts.is_empty() {
            return Ok(counts);
        }
        let Some(folder_id) = self
            .reader()?
            .get_folder_by_name(account, folder)?
            .and_then(|stored| stored.id)
        else {
            return Ok(counts);
        };
        let mut offset = 0;
        loop {
            let page = self.reader()?.get_message_list(
                folder_id,
                offset,
                HIGHLIGHT_PAGE,
                super::MessageSort::DateNewest,
            )?;
            if page.is_empty() {
                break;
            }
            offset += page.len();
            for names in self.highlights_for(account, folder, &page)?.values() {
                for name in names {
                    if let Some(count) = counts.iter_mut().find(|c| &c.rule == name) {
                        count.count += 1;
                    }
                }
            }
        }
        Ok(counts)
    }

    /// For every rule with `sidebar_badge`, how many unread messages of each
    /// folder it matches. Only those rules are evaluated, over the unread
    /// mail, and where they all ask for a header, over the unread mail that
    /// has one stored: this runs on every redraw of the sidebar.
    pub fn highlight_badges(&self) -> Result<Vec<HighlightBadge>> {
        let set = self.config().highlights.badged();
        if set.is_empty() {
            return Ok(Vec::new());
        }
        let config = self.config();
        let db = self.reader()?;
        let mut badges = Vec::new();
        for account in &config.accounts {
            let badged: Vec<&str> = set
                .for_account(&account.name)
                .into_iter()
                .map(|rule| rule.name.as_str())
                .collect();
            if badged.is_empty() {
                continue;
            }
            let headers_only = set.all_require_a_header(&account.name);
            for folder in db.get_folders_by_account(&account.name)? {
                let Some(folder_id) = folder.id else { continue };
                let unread = if headers_only {
                    db.unread_message_list_with_headers(folder_id)?
                } else {
                    db.unread_message_list(folder_id)?
                };
                let mut counts: HashMap<&str, u32> = HashMap::new();
                for names in self
                    .rules_matching(&set, &account.name, &folder.name, &unread)?
                    .values()
                {
                    for name in names {
                        if let Some(rule) = badged.iter().find(|rule| *rule == name) {
                            *counts.entry(rule).or_default() += 1;
                        }
                    }
                }
                for rule in &badged {
                    if let Some(count) = counts.get(rule) {
                        badges.push(HighlightBadge {
                            account: account.name.clone(),
                            folder: folder.name.clone(),
                            rule: rule.to_string(),
                            count: *count,
                        });
                    }
                }
            }
        }
        Ok(badges)
    }
}

fn message_facts(item: &MessageListItem, has_attachment: bool) -> MessageFacts {
    MessageFacts {
        from: item.from.clone(),
        to: item.to.clone(),
        cc: item.cc.clone(),
        subject: item.subject.clone(),
        has_attachment,
        headers: HashMap::new(),
    }
}

fn has_flag(flags: &[String], name: &str) -> bool {
    flags
        .iter()
        .any(|flag| flag.trim_start_matches('\\').eq_ignore_ascii_case(name))
}

/// Seconds between a stored RFC 3339 date and now; `None` for no readable date.
fn age_seconds(date: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let date = chrono::DateTime::parse_from_rfc3339(date?).ok()?;
    Some((now - date.with_timezone(&chrono::Utc)).num_seconds())
}

/// For each message, the `body` texts its stored body contains. Asked of the
/// database rather than read: a page of bodies is megabytes, and a message
/// whose body is not here yet simply matches none, which is the rule.
fn body_hits(
    db: &Database,
    ids: &[i64],
    needles: &[String],
) -> Result<HashMap<i64, HashSet<String>>> {
    let mut hits: HashMap<i64, HashSet<String>> = HashMap::new();
    if needles.is_empty() {
        return Ok(hits);
    }
    for chunk in ids.chunks(HIGHLIGHT_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        for needle in needles {
            let mut stmt = db.conn.prepare(&format!(
                "SELECT message_id FROM message_bodies
                 WHERE message_id IN ({placeholders}) AND body_text IS NOT NULL
                   AND instr(lower(body_text), ?) > 0"
            ))?;
            let parameters = chunk
                .iter()
                .map(|id| rusqlite::types::Value::Integer(*id))
                .chain(std::iter::once(rusqlite::types::Value::Text(
                    needle.clone(),
                )));
            for id in stmt.query_map(params_from_iter(parameters), |row| row.get::<_, i64>(0))? {
                hits.entry(id?).or_default().insert(needle.clone());
            }
        }
    }
    Ok(hits)
}

/// The header block each message offers a `list_id` or `header` condition:
/// `List-Id` and the headers the rules name, as the sync recorded them, and
/// for a message whose source is stored the source's own block as well. A
/// header recorded by neither matches nothing. Two blocks rather than one
/// joined: each ends at a blank line, and a parser stops at the first.
fn stored_headers(db: &Database, ids: &[i64]) -> Result<HashMap<i64, Vec<Vec<u8>>>> {
    let mut headers: HashMap<i64, Vec<Vec<u8>>> = HashMap::new();
    for chunk in ids.chunks(HIGHLIGHT_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = db.conn.prepare(&format!(
            "SELECT id, list_id, extra_headers FROM messages
             WHERE id IN ({placeholders}) AND (list_id IS NOT NULL OR extra_headers IS NOT NULL)"
        ))?;
        for row in stmt.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })? {
            let (id, list_id, extra) = row?;
            let recorded = crate::threads::ThreadHeaders {
                in_reply_to: None,
                references: None,
                list_id,
                extra: extra
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
            };
            headers.entry(id).or_default().push(recorded.as_block());
        }
    }
    for chunk in ids.chunks(HIGHLIGHT_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        // The start of the source alone, cut in SQL: a source is the whole
        // message, attachments included, and a page of those is not worth
        // reading for a header block.
        let mut stmt = db.conn.prepare(&format!(
            "SELECT message_id, substr(rfc822, 1, {HEADER_BLOCK_LIMIT})
             FROM message_bodies WHERE message_id IN ({placeholders}) AND length(rfc822) > 0"
        ))?;
        for row in stmt.query_map(params_from_iter(chunk.iter()), |row| {
            // Optional all the same: SQLite cuts an empty BLOB to NULL, and a
            // source that cannot be read is no reason to fail the list.
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        })? {
            let (id, source) = row?;
            let Some(source) = source else { continue };
            let end = source
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|at| at + 2)
                .or_else(|| {
                    source
                        .windows(2)
                        .position(|w| w == b"\n\n")
                        .map(|at| at + 1)
                })
                .unwrap_or(source.len());
            headers.entry(id).or_default().push(source[..end].to_vec());
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_is_counted_from_the_stored_date() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-14T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(age_seconds(Some("2026-10-13T12:00:00Z"), now), Some(86_400));
        assert_eq!(age_seconds(Some("yesterday"), now), None);
        assert_eq!(age_seconds(None, now), None);
    }

    #[test]
    fn flags_are_read_with_or_without_their_backslash() {
        assert!(has_flag(&["\\Seen".to_string()], "Seen"));
        assert!(has_flag(&["Seen".to_string()], "Seen"));
        assert!(!has_flag(&["Flagged".to_string()], "Seen"));
    }
}

#[cfg(test)]
mod column_tests {
    use crate::models::Message;
    use crate::store::Store;
    use crate::threads::ThreadHeaders;

    /// A `list_id` or `header` condition is answered from what the sync
    /// recorded beside the envelope, so it reaches every message and not only
    /// the ones whose source is stored.
    #[test]
    fn list_id_and_named_headers_come_from_the_sync_s_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nhighlights:\n  - name: Lists\n    when: {{ list_id: '*' }}\n    \
             style: {{ dim: true }}\n  - name: Spam\n    when: {{ header: {{ X-Spam-Flag: 'YES' }} }}\n    \
             style: {{ bold: true }}\n"
        ))
        .expect("config");
        assert_eq!(
            config.highlights.extra_header_names(),
            vec!["X-SPAM-FLAG".to_string()]
        );
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let mut listed = Message::new(inbox, 1);
        listed.headers = Some(ThreadHeaders {
            list_id: Some("News <news.example.com>".into()),
            ..ThreadHeaders::default()
        });
        let mut spam = Message::new(inbox, 2);
        spam.headers = Some(ThreadHeaders {
            extra: vec![("x-spam-flag".to_string(), "YES".to_string())],
            ..ThreadHeaders::default()
        });
        let plain = Message::new(inbox, 3);
        let ids: Vec<i64> = [&listed, &spam, &plain]
            .iter()
            .map(|m| db.insert_message(m).expect("insert"))
            .collect();
        let items = db.unread_message_list(inbox).expect("list");
        let matched = store
            .highlights_for("A", "INBOX", &items)
            .expect("highlights");
        assert_eq!(matched.get(&ids[0]), Some(&vec!["Lists".to_string()]));
        assert_eq!(matched.get(&ids[1]), Some(&vec!["Spam".to_string()]));
        assert_eq!(matched.get(&ids[2]), None);
    }

    /// Only a rule with `breaks_focus` lets mail through a focus, and only
    /// the inboxes' unread mail is counted for the badge.
    #[test]
    fn mail_breaks_a_focus_by_a_rule_that_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nhighlights:\n  - name: Urgent\n    \
             when: {{ header: {{ X-Severity: '1' }} }}\n    breaks_focus: true\n  - name: Lists\n    \
             when: {{ list_id: '*' }}\n    style: {{ dim: true }}\n"
        ))
        .expect("config");
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let urgent_headers = || {
            Some(ThreadHeaders {
                extra: vec![("x-severity".to_string(), "1".to_string())],
                ..ThreadHeaders::default()
            })
        };
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let other = db.get_or_create_folder("A", "Projects").expect("folder");
        let mut urgent = Message::new(inbox, 1);
        urgent.headers = urgent_headers();
        let mut listed = Message::new(inbox, 2);
        listed.headers = Some(ThreadHeaders {
            list_id: Some("News <news.example.com>".into()),
            ..ThreadHeaders::default()
        });
        let plain = Message::new(inbox, 3);
        let mut read = Message::new(inbox, 4);
        read.headers = urgent_headers();
        read.flags = vec!["\\Seen".to_string()];
        let mut elsewhere = Message::new(other, 1);
        elsewhere.headers = urgent_headers();
        let ids: Vec<i64> = [&urgent, &listed, &plain, &read, &elsewhere]
            .iter()
            .map(|m| db.insert_message(m).expect("insert"))
            .collect();

        let breakers = store.focus_breakers(&ids).expect("breakers");
        assert_eq!(
            breakers,
            std::collections::HashSet::from([ids[0], ids[3], ids[4]])
        );
        assert_eq!(store.focus_breaker_count().expect("count"), 1);
    }

    /// The sidebar counts the unread mail of the rules with `sidebar_badge`,
    /// whether the header was recorded by the sync or is in a stored source,
    /// and no other rule.
    #[test]
    fn badges_count_the_unread_mail_of_the_badged_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nhighlights:\n  - name: Urgent\n    \
             when: {{ header: {{ X-Severity: '1' }} }}\n    style: {{ sidebar_badge: true }}\n  \
             - name: Unread\n    when: {{ unread: true }}\n    style: {{ bold: true }}\n"
        ))
        .expect("config");
        assert!(config.highlights.badged().all_require_a_header("A"));
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let urgent_headers = || {
            Some(ThreadHeaders {
                extra: vec![("x-severity".to_string(), "1".to_string())],
                ..ThreadHeaders::default()
            })
        };
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let mut recorded = Message::new(inbox, 1);
        recorded.headers = urgent_headers();
        let in_source = Message::new(inbox, 2);
        let plain = Message::new(inbox, 3);
        let mut read = Message::new(inbox, 4);
        read.headers = urgent_headers();
        read.flags = vec!["\\Seen".to_string()];
        let ids: Vec<i64> = [&recorded, &in_source, &plain, &read]
            .iter()
            .map(|m| db.insert_message(m).expect("insert"))
            .collect();
        db.set_message_source(ids[1], b"Subject: Issue\r\nX-Severity: 1\r\n\r\nBody\r\n")
            .expect("source");

        let with_headers: Vec<i64> = db
            .unread_message_list_with_headers(inbox)
            .expect("list")
            .iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(with_headers.len(), 2, "{with_headers:?}");
        assert!(with_headers.contains(&ids[0]) && with_headers.contains(&ids[1]));

        let badges = store.highlight_badges().expect("badges");
        assert_eq!(badges.len(), 1, "{badges:?}");
        assert_eq!(
            (
                badges[0].folder.as_str(),
                badges[0].rule.as_str(),
                badges[0].count
            ),
            ("INBOX", "Urgent", 2)
        );
    }

    /// A badged rule that asks for no header is counted over all the unread
    /// mail, also beside one that does.
    #[test]
    fn a_badged_rule_without_a_header_counts_mail_that_has_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nhighlights:\n  - name: Urgent\n    \
             when: {{ header: {{ X-Severity: '1' }} }}\n    style: {{ sidebar_badge: true }}\n  \
             - name: Builds\n    when: {{ subject: 'Build*' }}\n    style: {{ sidebar_badge: true }}\n"
        ))
        .expect("config");
        assert!(!config.highlights.badged().all_require_a_header("A"));
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let mut build = Message::new(inbox, 1);
        build.subject = Some("Build failed".to_string());
        db.insert_message(&build).expect("insert");
        db.insert_message(&Message::new(inbox, 2)).expect("insert");

        let badges = store.highlight_badges().expect("badges");
        assert_eq!(badges.len(), 1, "{badges:?}");
        assert_eq!((badges[0].rule.as_str(), badges[0].count), ("Builds", 1));
    }

    /// Without a rule that breaks a focus, nothing does.
    #[test]
    fn no_mail_breaks_a_focus_without_such_a_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nhighlights:\n  - name: All\n    style: {{ dim: true }}\n"
        ))
        .expect("config");
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let id = db.insert_message(&Message::new(inbox, 1)).expect("insert");
        assert!(store.focus_breakers(&[id]).expect("breakers").is_empty());
        assert_eq!(store.focus_breaker_count().expect("count"), 0);
    }

    /// A header the sync did not record, because no rule named it yet, is
    /// still read from a stored source when the sync recorded another one.
    #[test]
    fn a_stored_source_answers_beside_the_sync_s_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nhighlights:\n  - name: Urgent\n    \
             when: {{ header: {{ X-Severity: '1' }} }}\n    style: {{ bold: true }}\n"
        ))
        .expect("config");
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let mut older = Message::new(inbox, 1);
        older.headers = Some(ThreadHeaders {
            list_id: Some("Issues <issues.example.com>".into()),
            extra: vec![("x-concerns-me".to_string(), "true".to_string())],
            ..ThreadHeaders::default()
        });
        let id = db.insert_message(&older).expect("insert");
        db.set_message_source(
            id,
            b"Subject: Issue\r\nX-Concerns-Me: true\r\nX-Severity: 1\r\n\r\nBody\r\n",
        )
        .expect("source");
        let items = db.unread_message_list(inbox).expect("list");
        let matched = store
            .highlights_for("A", "INBOX", &items)
            .expect("highlights");
        assert_eq!(matched.get(&id), Some(&vec!["Urgent".to_string()]));
    }

    /// An empty stored source — one message in a real Archive had one — is a
    /// message without a header block, not a failure of the folder's list:
    /// SQLite cuts an empty BLOB to NULL, not to an empty one.
    #[test]
    fn an_empty_stored_source_leaves_the_list_working() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nhighlights:\n  - name: Urgent\n    \
             when: {{ header: {{ X-Severity: '1' }} }}\n    style: {{ bold: true }}\n"
        ))
        .expect("config");
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let mut message = Message::new(inbox, 1);
        message.headers = Some(ThreadHeaders {
            list_id: Some("Issues <issues.example.com>".into()),
            ..ThreadHeaders::default()
        });
        let id = db.insert_message(&message).expect("insert");
        db.set_message_source(id, b"").expect("source");
        let items = db.unread_message_list(inbox).expect("list");
        let matched = store
            .highlights_for("A", "INBOX", &items)
            .expect("highlights");
        assert_eq!(matched.get(&id), None);
    }

    /// A `load_remote_images` rule is answered from the same record, and the
    /// headers it names are fetched by the sync like a highlight's.
    #[test]
    fn remote_image_rules_match_one_message_by_its_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nload_remote_images:\n  - name: Shop\n    \
             when: {{ header: {{ X-Mailer: 'ShopMailer*' }} }}\n"
        ))
        .expect("config");
        assert_eq!(config.extra_header_names(), vec!["X-MAILER".to_string()]);
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let mut shop = Message::new(inbox, 1);
        shop.headers = Some(ThreadHeaders {
            extra: vec![("x-mailer".to_string(), "ShopMailer 3".to_string())],
            ..ThreadHeaders::default()
        });
        let plain = Message::new(inbox, 2);
        let shop = db.insert_message(&shop).expect("insert");
        let plain = db.insert_message(&plain).expect("insert");
        assert_eq!(
            store.remote_images_rule_for_message(shop).expect("rule"),
            Some("Shop".to_string())
        );
        assert_eq!(
            store.remote_images_rule_for_message(plain).expect("rule"),
            None
        );
        assert_eq!(
            store.highlights_for_message(shop).expect("highlights"),
            Vec::<String>::new()
        );
    }

    /// A view that asks about a header has the sync fetch it, and its search
    /// finds the messages that carry it.
    #[test]
    fn a_view_searches_by_a_header_the_sync_records_for_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
             imap_username: u\n    from: a@x\nvirtual_folders:\n  - name: Mine\n    source: search\n    \
             query: in:Archive header:X-Owner=true\n"
        ))
        .expect("config");
        assert_eq!(config.extra_header_names(), vec!["X-OWNER".to_string()]);
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let archive = db.get_or_create_folder("A", "Archive").expect("folder");
        let owned = |uid: u32, value: &str| {
            let mut message = Message::new(archive, uid);
            message.headers = Some(ThreadHeaders {
                extra: vec![("x-owner".to_string(), value.to_string())],
                ..ThreadHeaders::default()
            });
            message
        };
        let mine = db.insert_message(&owned(1, "true")).expect("insert");
        db.insert_message(&owned(2, "false")).expect("insert");
        db.insert_message(&Message::new(archive, 3))
            .expect("insert");
        drop(db);
        let found = |query: &str| -> Vec<i64> {
            store
                .search_messages(query, &crate::store::SearchScope::All)
                .expect("search")
                .into_iter()
                .map(|m| m.id)
                .collect()
        };
        assert_eq!(found("in:Archive header:X-Owner=true"), vec![mine]);
        assert_eq!(found("header:x-owner").len(), 2);
        assert_eq!(found("in:Archive -header:X-Owner").len(), 1);
    }

    /// A header named for the first time is fetched again for the mail
    /// already here, through the backfill, and only the first time.
    #[test]
    fn a_newly_named_header_is_fetched_again_for_stored_mail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let yaml = |query: &str| {
            format!(
                "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    \
                 imap_username: u\n    from: a@x\nvirtual_folders:\n  - name: Mine\n    source: search\n    \
                 query: {query}\n"
            )
        };
        let config = |query: &str| -> crate::Config {
            serde_saphyr::from_str(&yaml(query)).expect("config")
        };
        let store = Store::new(config("in:Archive")).expect("store");
        let mut db = store.writer().expect("writer");
        let archive = db.get_or_create_folder("A", "Archive").expect("folder");
        let id = db
            .insert_message(&Message::new(archive, 1))
            .expect("insert");
        db.conn
            .execute("UPDATE messages SET headers_synced = 1 WHERE id = ?1", [id])
            .expect("synced");
        assert_eq!(store.thread_header_backfill_pending().expect("pending"), 0);

        store.replace_config(std::sync::Arc::new(config("header:X-Owner=true")));
        assert_eq!(store.thread_header_backfill_pending().expect("pending"), 1);
        assert_eq!(
            db.messages_without_thread_headers(archive, 10)
                .expect("pending"),
            vec![(id, 1)]
        );
        assert!(
            db.get_message_by_id(id)
                .expect("message")
                .expect("there")
                .headers
                .is_some()
        );

        db.conn
            .execute("UPDATE messages SET headers_synced = 1 WHERE id = ?1", [id])
            .expect("fetched");
        drop(store);
        let store = Store::new(config("header:x-owner")).expect("store");
        assert_eq!(store.thread_header_backfill_pending().expect("pending"), 0);
    }
}
