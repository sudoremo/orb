//! The threaded view of a folder, a message's whole conversation across the
//! account, and the backfill of the headers threads are built from.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params, params_from_iter};

use super::Store;
use crate::Result;
use crate::db::Database;
use crate::imap::AccountConnectionConfig;
use crate::models::{MessageListItem, SearchResultItem};
use crate::threads::{self, Expanded, ThreadHeaders, ThreadMember};

/// The thread a message belongs to. Every message has one since migration
/// 24 — its own, for one stored before threads existed — so the queries read
/// the column and its index.
const THREAD_KEY: &str = "thread_id";

/// [`THREAD_KEY`] for a query where `messages` is aliased `m`.
const M_THREAD_KEY: &str = "m.thread_id";

/// Messages per header fetch in the backfill.
const HEADER_BATCH_SIZE: usize = 500;

/// Rounds of looking for more of a conversation in other folders. Each round
/// follows one more link; a conversation that needs more than this many is a
/// mailing list, and the view says what it found.
const CONVERSATION_ROUNDS: usize = 4;

/// Ids per statement, below what SQLite lets one bind.
const KEY_CHUNK: usize = 400;

/// A row of the threaded list: a thread, or a member of an expanded one.
#[derive(Debug, Clone)]
pub struct ThreadRow {
    pub message: MessageListItem,
    pub thread_id: String,
    /// Members of the thread; 1 for a message on its own.
    pub count: u32,
    pub unread: u32,
    pub expanded: bool,
    /// 0 for a thread's own row, which is its oldest message; members count
    /// from 1, one deeper per reply.
    pub depth: u32,
    /// The date of the thread's newest message, for the thread's row.
    pub latest: Option<String>,
    /// Every member with where it lives, oldest first, on the row of a
    /// thread shown collapsed: the row stands for all of them. Empty on every
    /// other row.
    pub members: Vec<(Location, MessageListItem)>,
}

/// Where [`Store::merged_thread_row`] found a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPlace {
    /// The row, counted from 0, with the thread expanded when `expand` says.
    pub row: usize,
    pub thread: String,
    /// Whether the thread has members to show under its row.
    pub expand: bool,
}

/// The account and folder a row of a merged list came from.
pub type Location = (String, String);

/// A thread member as the list needs it.
struct Member {
    item: MessageListItem,
    in_reply_to: Option<String>,
}

impl Store {
    /// A page of the folder as conversations: one row per thread, newest
    /// first, with the members of the threads in `expanded` under theirs.
    /// Also returns how many rows the whole list has.
    pub fn thread_list(
        &self,
        account: &str,
        folder: &str,
        offset: usize,
        limit: usize,
        expanded: &[String],
    ) -> Result<(Vec<ThreadRow>, usize)> {
        let (rows, total) = self.merged_thread_list(
            &[(account.to_string(), folder.to_string())],
            offset,
            limit,
            expanded,
        )?;
        Ok((rows.into_iter().map(|(_, row)| row).collect(), total))
    }

    /// [`Store::thread_list`] over several folders read as one list — every
    /// account's inbox, for Today — each row with the account and folder it
    /// came from. A thread with members in two of the folders is one thread.
    /// A folder that does not exist is skipped, as in
    /// [`Store::get_merged_message_list`].
    pub fn merged_thread_list(
        &self,
        folders: &[(String, String)],
        offset: usize,
        limit: usize,
        expanded: &[String],
    ) -> Result<(Vec<(Location, ThreadRow)>, usize)> {
        let db = self.reader()?;
        let locations = merged_locations(&db, folders)?;
        if locations.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let in_folders = in_folders_clause(&locations);
        let thread_count: usize = db.conn.query_row(
            &format!("SELECT COUNT(DISTINCT {THREAD_KEY}) FROM messages WHERE {in_folders}"),
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;

        let placed = place_expanded(&db, &in_folders, expanded)?;
        let total = threads::flat_total(thread_count, &placed);
        if offset >= total || limit == 0 {
            return Ok((Vec::new(), total));
        }

        let start = threads::locate(offset, &placed);
        let mut stmt = db.conn.prepare(&format!(
            "SELECT {THREAD_KEY} AS key FROM messages
             WHERE {in_folders}
             GROUP BY key
             ORDER BY MAX(COALESCE(date_unix, -1)) DESC, key ASC
             LIMIT ?1 OFFSET ?2"
        ))?;
        let keys: Vec<String> = stmt
            .query_map(params![limit as i64, start.thread as i64], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        let mut rows = Vec::with_capacity(limit);
        let mut skip = start.member;
        for key in keys {
            if rows.len() >= limit {
                break;
            }
            let members = thread_members(&db, &in_folders, &key)?;
            let Some(root) = members.first() else {
                continue;
            };
            let is_expanded = expanded.contains(&key);
            let count = members.len() as u32;
            let unread = members
                .iter()
                .filter(|m| !has_flag(&m.item.flags, "Seen"))
                .count() as u32;
            let latest = members
                .iter()
                .filter(|m| m.item.date.is_some())
                .max_by_key(|m| threads::date_key(m.item.date.as_deref()))
                .and_then(|m| m.item.date.clone());
            let depths = threads::depths(
                &members
                    .iter()
                    .map(|m| ThreadMember {
                        message_id: m.item.message_id.as_deref(),
                        in_reply_to: m.in_reply_to.as_deref(),
                    })
                    .collect::<Vec<_>>(),
            );
            let collapsed_members: Vec<(Location, MessageListItem)> = if is_expanded || count < 2 {
                Vec::new()
            } else {
                members
                    .iter()
                    .filter_map(|m| {
                        locations
                            .get(&m.item.folder_id)
                            .map(|location| (location.clone(), m.item.clone()))
                    })
                    .collect()
            };
            let shown: Vec<(usize, &Member)> = if is_expanded {
                members.iter().enumerate().collect()
            } else {
                vec![(0, root)]
            };
            for (position, member) in shown {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if rows.len() >= limit {
                    break;
                }
                let Some(location) = locations.get(&member.item.folder_id) else {
                    continue;
                };
                rows.push((
                    location.clone(),
                    ThreadRow {
                        message: member.item.clone(),
                        thread_id: key.clone(),
                        count,
                        unread,
                        expanded: is_expanded,
                        // A member is at least one in: a reply by subject alone
                        // answers nothing named, yet sits under the root.
                        depth: if position == 0 {
                            0
                        } else {
                            depths[position].max(1)
                        },
                        latest: latest.clone(),
                        members: if position == 0 {
                            collapsed_members.clone()
                        } else {
                            Vec::new()
                        },
                    },
                ));
            }
        }
        Ok((rows, total))
    }

    /// Where a message stands in [`Store::merged_thread_list`] once its own
    /// thread is expanded too, so a list can open the thread and the page the
    /// message is on. `None` when none of `folders` holds it.
    pub fn merged_thread_row(
        &self,
        folders: &[(String, String)],
        id: i64,
        expanded: &[String],
    ) -> Result<Option<ThreadPlace>> {
        let db = self.reader()?;
        let locations = merged_locations(&db, folders)?;
        if locations.is_empty() {
            return Ok(None);
        }
        let in_folders = in_folders_clause(&locations);
        let Some(key) = db
            .conn
            .query_row(
                &format!("SELECT {THREAD_KEY} FROM messages WHERE {in_folders} AND id = ?1"),
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let members = thread_members(&db, &in_folders, &key)?;
        let Some(member) = members.iter().position(|m| m.item.id == id) else {
            return Ok(None);
        };
        let Some(own) = place(&db, &in_folders, &key)? else {
            return Ok(None);
        };
        let others: Vec<String> = expanded.iter().filter(|k| **k != key).cloned().collect();
        let above: usize = place_expanded(&db, &in_folders, &others)?
            .iter()
            .filter(|e| e.rank < own.rank)
            .map(|e| e.members - 1)
            .sum();
        let expand = members.len() > 1;
        Ok(Some(ThreadPlace {
            row: own.rank + above + if expand { member } else { 0 },
            thread: key,
            expand,
        }))
    }

    /// The members of the threads `keys` within `folders` — one folder, or
    /// Today's inboxes — each with where it lives: thread by thread in the
    /// order given, oldest first within each. A thread with no member there
    /// is left out.
    pub fn merged_thread_members(
        &self,
        folders: &[(String, String)],
        keys: &[String],
    ) -> Result<Vec<(Location, MessageListItem)>> {
        let db = self.reader()?;
        let locations = merged_locations(&db, folders)?;
        if locations.is_empty() {
            return Ok(Vec::new());
        }
        let in_folders = in_folders_clause(&locations);
        let mut found = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for key in keys {
            if !seen.insert(key.as_str()) {
                continue;
            }
            for member in thread_members(&db, &in_folders, key)? {
                if let Some(location) = locations.get(&member.item.folder_id) {
                    found.push((location.clone(), member.item));
                }
            }
        }
        Ok(found)
    }

    /// Where items dated `times` (unix seconds) would stand among the rows
    /// of [`Store::merged_thread_list`]: for each, how many rows come before
    /// it — the threads whose newest message is newer, an expanded one with
    /// every member it shows. An item as new as a thread stands above it.
    pub fn merged_thread_ranks(
        &self,
        folders: &[(String, String)],
        times: &[i64],
        expanded: &[String],
    ) -> Result<Vec<usize>> {
        let db = self.reader()?;
        let locations = merged_locations(&db, folders)?;
        if locations.is_empty() {
            return Ok(vec![0; times.len()]);
        }
        let in_folders = in_folders_clause(&locations);
        let mut open: Vec<(i64, usize)> = Vec::new();
        for key in expanded {
            let (newest, members): (Option<i64>, i64) = db.conn.query_row(
                &format!(
                    "SELECT MAX(COALESCE(date_unix, -1)), COUNT(*) FROM messages
                     WHERE {in_folders} AND {THREAD_KEY} = ?1"
                ),
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if members > 0 {
                open.push((newest.unwrap_or(-1), members as usize));
            }
        }
        // Each thread's newest message once, rather than grouping the
        // inboxes again for every item placed.
        let mut stmt = db.conn.prepare(&format!(
            "SELECT MAX(COALESCE(date_unix, -1)) FROM messages
             WHERE {in_folders} GROUP BY {THREAD_KEY}"
        ))?;
        let mut newest: Vec<i64> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        newest.sort_unstable();
        Ok(times
            .iter()
            .map(|&time| {
                let threads = newest.len() - newest.partition_point(|&n| n <= time);
                let members: usize = open
                    .iter()
                    .filter(|(newest, _)| *newest > time)
                    .map(|(_, members)| members - 1)
                    .sum();
                threads + members
            })
            .collect())
    }

    /// The whole conversation a message is part of, across every folder of
    /// its account: the messages of its thread here, those elsewhere that
    /// answer or are answered by them, and so on. Oldest first.
    pub fn thread_messages(&self, message_id: i64) -> Result<Vec<SearchResultItem>> {
        let db = self.reader()?;
        let Some((folder_id, key, account)) = db
            .conn
            .query_row(
                &format!(
                    "SELECT m.folder_id, {M_THREAD_KEY}, f.account_name
                     FROM messages m JOIN folders f ON f.id = m.folder_id WHERE m.id = ?1"
                ),
                params![message_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(Vec::new());
        };

        let mut known: HashSet<String> = HashSet::new();
        let mut frontier: Vec<String> = vec![key];
        let mut found: HashMap<i64, SearchResultItem> = HashMap::new();
        let mut seed = true;
        for _ in 0..=CONVERSATION_ROUNDS {
            let mut next: Vec<String> = Vec::new();
            for chunk in frontier.chunks(KEY_CHUNK) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                // The first round is the thread here; the later ones follow
                // the identifiers into the other folders.
                // The later rounds are confined to the account's folders
                // first, so each of the three lookups walks an index of
                // those rows rather than the whole table.
                let scope = if seed {
                    format!("m.folder_id = {folder_id} AND {M_THREAD_KEY} IN ({placeholders})")
                } else {
                    format!(
                        "f.account_name = ?
                         AND m.folder_id IN (SELECT id FROM folders WHERE account_name = ?)
                         AND (m.message_id IN ({placeholders})
                              OR m.in_reply_to IN ({placeholders})
                              OR {M_THREAD_KEY} IN ({placeholders}))"
                    )
                };
                let mut stmt = db.conn.prepare(&format!(
                    "SELECT m.id, m.folder_id, m.uid, m.message_id, m.subject, m.from_addr,
                            m.to_addr, m.cc_addr, m.date, m.size, m.flags, m.modseq,
                            m.in_reply_to, m.references_hdr, {M_THREAD_KEY}, f.name
                     FROM messages m JOIN folders f ON f.id = m.folder_id
                     WHERE {scope}"
                ))?;
                let mut parameters: Vec<rusqlite::types::Value> = Vec::new();
                if !seed {
                    parameters.push(account.clone().into());
                    parameters.push(account.clone().into());
                }
                let repeats = if seed { 1 } else { 3 };
                for _ in 0..repeats {
                    parameters.extend(
                        chunk
                            .iter()
                            .map(|k| rusqlite::types::Value::from(k.clone())),
                    );
                }
                let rows = stmt.query_map(params_from_iter(parameters), |row| {
                    let item = list_item_from_row(row)?;
                    Ok((
                        item,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<String>>(13)?,
                        row.get::<_, String>(14)?,
                        row.get::<_, String>(15)?,
                    ))
                })?;
                for row in rows {
                    let (item, in_reply_to, references, thread_key, folder_name) = row?;
                    let mut links: Vec<String> = Vec::new();
                    if let Some(id) = &item.message_id {
                        links.push(id.clone());
                    }
                    links.push(thread_key);
                    links.extend(
                        ThreadHeaders {
                            in_reply_to,
                            references,
                            list_id: None,
                            extra: Vec::new(),
                        }
                        .parents(),
                    );
                    for link in links {
                        if known.insert(link.clone()) {
                            next.push(link);
                        }
                    }
                    found.entry(item.id).or_insert(SearchResultItem {
                        message: item,
                        account_name: account.clone(),
                        folder_name,
                    });
                }
            }
            seed = false;
            if next.is_empty() {
                break;
            }
            frontier = next;
        }

        let mut conversation: Vec<SearchResultItem> = found.into_values().collect();
        conversation.sort_by_key(|hit| {
            (
                threads::date_key(hit.message.date.as_deref()),
                hit.message.id,
            )
        });
        Ok(conversation)
    }

    /// Fetch the threading headers of messages stored before the sync asked
    /// for them, a folder at a time. Cheap — three header lines per message —
    /// and what gives an existing mailbox its conversations.
    pub async fn fetch_pending_thread_headers(
        &self,
        account_name: &str,
        folders: &[String],
        budget: usize,
    ) -> Result<usize> {
        let account = self
            .get_account_config(account_name)
            .ok_or_else(|| crate::Error::NotFound(format!("Account {account_name} not found")))?;
        let mut done = 0usize;
        let mut remaining = budget;
        for folder_name in folders {
            if remaining == 0 {
                break;
            }
            let (folder_id, pending) = {
                let db = self.reader()?;
                let Some(folder) = db.get_folder_by_name(account_name, folder_name)? else {
                    continue;
                };
                let folder_id = folder.id.expect("folder must have id");
                (
                    folder_id,
                    db.messages_without_thread_headers(folder_id, remaining)?,
                )
            };
            if pending.is_empty() {
                continue;
            }

            let password = self.credentials.imap(&account).await?;
            self.connection_pool
                .register_account(
                    &account.name,
                    AccountConnectionConfig::for_account(&account, password),
                )
                .await;
            let mut client = self
                .connection_pool
                .acquire(&account.name, "thread headers")
                .await?;
            let outcome: Result<()> = async {
                self.select_for_uids(&mut client, account_name, folder_name)
                    .await?;
                for chunk in pending.chunks(HEADER_BATCH_SIZE) {
                    let uids: Vec<u32> = chunk.iter().map(|(_, uid)| *uid).collect();
                    let fields = crate::imap::backfill_header_fields();
                    let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
                    let mut listed = Vec::new();
                    for set in crate::imap::uid_sets(&uids) {
                        listed.extend(client.fetch_header_listing(&set, &fields).await?);
                    }
                    let by_uid: HashMap<u32, Vec<u8>> = listed
                        .into_iter()
                        .map(|listing| (listing.uid, listing.headers))
                        .collect();
                    let _guard = self.write_lock.lock().await;
                    let mut db = self.writer()?;
                    db.transaction(|tx| {
                        for (id, uid) in chunk {
                            match by_uid.get(uid) {
                                Some(block) => {
                                    let headers = ThreadHeaders::parse(block);
                                    tx.execute(
                                        "UPDATE messages SET in_reply_to = ?1, references_hdr = ?2,
                                             list_id = ?3, extra_headers = ?4, headers_synced = 1
                                         WHERE id = ?5",
                                        params![
                                            headers.in_reply_to,
                                            headers.references,
                                            headers.list_id,
                                            extra_headers_json(&headers),
                                            id
                                        ],
                                    )?;
                                    Database::assign_thread(tx, folder_id, *id)?;
                                }
                                // Gone from the server: the next sync removes
                                // it; until then it is not asked about again.
                                None => {
                                    tx.execute(
                                        "UPDATE messages SET headers_synced = 0 WHERE id = ?1",
                                        params![id],
                                    )?;
                                }
                            }
                        }
                        Ok(())
                    })?;
                    done += chunk.len();
                    remaining = remaining.saturating_sub(chunk.len());
                }
                Ok(())
            }
            .await;
            self.connection_pool
                .give_back(&account.name, client, outcome.as_ref().err())
                .await;
            outcome?;
        }
        Ok(done)
    }

    /// How many messages still lack their threading headers.
    pub fn thread_header_backfill_pending(&self) -> Result<usize> {
        Ok(self.reader()?.pending_thread_header_count()? as usize)
    }
}

/// The extra headers as the column stores them: a JSON list of pairs, or
/// nothing when there are none.
fn extra_headers_json(headers: &ThreadHeaders) -> Option<String> {
    (!headers.extra.is_empty()).then(|| serde_json::to_string(&headers.extra).unwrap_or_default())
}

/// The ids of those of `folders` that exist, each with its account and
/// folder. A folder that does not is skipped: the inboxes of an account
/// that has not synced yet are simply empty.
pub(super) fn merged_locations(
    db: &Database,
    folders: &[(String, String)],
) -> Result<HashMap<i64, Location>> {
    let mut locations = HashMap::new();
    for (account, folder) in folders {
        if let Some(id) = db
            .get_folder_by_name(account, folder)?
            .and_then(|stored| stored.id)
        {
            locations.insert(id, (account.clone(), folder.clone()));
        }
    }
    Ok(locations)
}

/// `folder_id IN (…)` for the folders of `locations`. Folder ids are the
/// database's own integers, so they are written into the statement rather
/// than bound.
pub(super) fn in_folders_clause(locations: &HashMap<i64, Location>) -> String {
    format!(
        "folder_id IN ({})",
        locations
            .keys()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn has_flag(flags: &[String], name: &str) -> bool {
    flags
        .iter()
        .any(|flag| flag.trim_start_matches('\\').eq_ignore_ascii_case(name))
}

/// A list item from a row selected as `SELECT id, folder_id, uid, message_id,
/// subject, from_addr, to_addr, cc_addr, date, size, flags, modseq, ...`.
fn list_item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageListItem> {
    let flags_str = row.get::<_, Option<String>>(10)?.unwrap_or_default();
    let flags: Vec<String> = serde_json::from_str(&flags_str).unwrap_or_else(|_| {
        flags_str
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    });
    Ok(MessageListItem {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        uid: row.get(2)?,
        message_id: row.get(3)?,
        subject: row.get(4)?,
        from: row.get(5)?,
        to: row.get(6)?,
        cc: row.get(7)?,
        date: row.get(8)?,
        size: row.get(9)?,
        flags,
        modseq: row.get(11)?,
    })
}

/// Where each of the threads `keys` stands among the threads of the folders
/// `in_folders` names, ordered by rank; a thread with no member there is left
/// out.
fn place_expanded(db: &Database, in_folders: &str, keys: &[String]) -> Result<Vec<Expanded>> {
    let mut placed = Vec::new();
    for key in keys {
        if let Some(expanded) = place(db, in_folders, key)? {
            placed.push(expanded);
        }
    }
    placed.sort_by_key(|e| e.rank);
    Ok(placed)
}

/// How many threads stand above `key` — newest message first, then by key —
/// and how many members it has.
fn place(db: &Database, in_folders: &str, key: &str) -> Result<Option<Expanded>> {
    let Some((newest, members)) = db
        .conn
        .query_row(
            &format!(
                "SELECT MAX(COALESCE(date_unix, -1)), COUNT(*) FROM messages
                 WHERE {in_folders} AND {THREAD_KEY} = ?1"
            ),
            params![key],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?)),
        )
        .ok()
        .filter(|(_, members)| *members > 0)
    else {
        return Ok(None);
    };
    let newest = newest.unwrap_or(-1);
    let rank: i64 = db.conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM (
                 SELECT {THREAD_KEY} AS key, MAX(COALESCE(date_unix, -1)) AS newest
                 FROM messages WHERE {in_folders} GROUP BY key)
             WHERE newest > ?1 OR (newest = ?1 AND key < ?2)"
        ),
        params![newest, key],
        |row| row.get(0),
    )?;
    Ok(Some(Expanded {
        rank: rank as usize,
        members: members as usize,
    }))
}

/// The members of a thread, oldest first.
/// The members of a thread within the folders `in_folders` names, oldest first.
fn thread_members(db: &Database, in_folders: &str, key: &str) -> Result<Vec<Member>> {
    let mut stmt = db.conn.prepare(&format!(
        "SELECT id, folder_id, uid, message_id, subject, from_addr, to_addr, cc_addr,
                date, size, flags, modseq, in_reply_to
         FROM messages
         WHERE {in_folders} AND {THREAD_KEY} = ?1
         ORDER BY date_unix ASC NULLS LAST, id ASC"
    ))?;
    let rows = stmt.query_map(params![key], |row| {
        Ok(Member {
            item: list_item_from_row(row)?,
            in_reply_to: row.get(12)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Message;

    fn stored(
        uid: u32,
        subject: &str,
        message_id: &str,
        in_reply_to: Option<&str>,
        date: &str,
    ) -> Message {
        let mut message = Message::new(1, uid);
        message.subject = Some(subject.to_string());
        message.message_id = Some(message_id.to_string());
        message.date = Some(date.to_string());
        message.headers = Some(ThreadHeaders {
            in_reply_to: in_reply_to.map(str::to_string),
            references: None,
            list_id: None,
            extra: Vec::new(),
        });
        message
    }

    fn store_with(messages: &[Message]) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("db.sqlite").to_str().unwrap().to_string();
        let config: crate::Config = serde_saphyr::from_str(&format!(
            "db_path: {db_path}\naccounts:\n  - name: A\n    imap_server: x\n    imap_port: 993\n    imap_username: u\n    from: a@x\n"
        ))
        .expect("config");
        let store = Store::new(config).expect("store");
        let mut db = store.writer().expect("writer");
        let folder = db.get_or_create_folder("A", "INBOX").expect("folder");
        assert_eq!(folder, 1);
        db.transaction(|tx| {
            Database::bulk_insert_messages_in_tx(tx, messages)?;
            Ok(())
        })
        .expect("insert");
        (dir, store)
    }

    #[test]
    fn replies_join_their_parent_s_thread_even_when_stored_first() {
        let (_dir, store) = store_with(&[
            // The reply arrives before what it answers, as a first sync does.
            stored(
                3,
                "Re: Plan",
                "<c@x>",
                Some("<a@x>"),
                "2026-01-03T00:00:00Z",
            ),
            stored(1, "Plan", "<a@x>", None, "2026-01-01T00:00:00Z"),
            stored(
                2,
                "Re: Plan",
                "<b@x>",
                Some("<a@x>"),
                "2026-01-02T00:00:00Z",
            ),
            // A reply by subject alone: no parent named, but the subject says.
            stored(4, "AW: Plan", "<d@x>", None, "2026-01-04T00:00:00Z"),
            stored(5, "Lunch", "<e@x>", None, "2026-01-05T00:00:00Z"),
            // The same subject without a prefix is a thread of its own.
            stored(6, "Plan", "<f@x>", None, "2026-01-06T00:00:00Z"),
        ]);
        let (rows, total) = store.thread_list("A", "INBOX", 0, 10, &[]).expect("list");
        assert_eq!(total, 3);
        let summary: Vec<(u32, u32, u32)> = rows
            .iter()
            .map(|r| (r.message.uid, r.count, r.depth))
            .collect();
        // Newest thread first: Plan (uid 6), then Lunch, then the conversation.
        assert_eq!(summary, vec![(6, 1, 0), (5, 1, 0), (1, 4, 0)]);
        assert_eq!(rows[2].latest.as_deref(), Some("2026-01-04T00:00:00Z"));

        let expanded = vec!["<a@x>".to_string()];
        let (rows, total) = store
            .thread_list("A", "INBOX", 0, 10, &expanded)
            .expect("list");
        assert_eq!(total, 6);
        let summary: Vec<(u32, u32)> = rows.iter().map(|r| (r.message.uid, r.depth)).collect();
        assert_eq!(
            summary,
            vec![(6, 0), (5, 0), (1, 0), (2, 1), (3, 1), (4, 1)]
        );

        // A page that starts inside the expanded thread.
        let (rows, _) = store
            .thread_list("A", "INBOX", 4, 10, &expanded)
            .expect("list");
        let uids: Vec<u32> = rows.iter().map(|r| r.message.uid).collect();
        assert_eq!(uids, vec![3, 4]);
    }

    #[test]
    fn a_message_is_found_in_the_threaded_list_with_its_thread_open() {
        let (_dir, store) = store_with(&[
            stored(1, "Plan", "<a@x>", None, "2026-01-01T00:00:00Z"),
            stored(
                2,
                "Re: Plan",
                "<b@x>",
                Some("<a@x>"),
                "2026-01-02T00:00:00Z",
            ),
            stored(
                3,
                "Re: Plan",
                "<c@x>",
                Some("<b@x>"),
                "2026-01-03T00:00:00Z",
            ),
            stored(4, "Lunch", "<e@x>", None, "2026-01-05T00:00:00Z"),
            stored(5, "Coffee", "<g@x>", None, "2026-01-06T00:00:00Z"),
            stored(
                6,
                "Re: Coffee",
                "<h@x>",
                Some("<g@x>"),
                "2026-01-07T00:00:00Z",
            ),
        ]);
        let folders = [("A".to_string(), "INBOX".to_string())];
        let id_of = |uid: u32| {
            let (rows, _) = store
                .thread_list(
                    "A",
                    "INBOX",
                    0,
                    10,
                    &["<a@x>".to_string(), "<g@x>".to_string()],
                )
                .unwrap();
            rows.iter()
                .find(|r| r.message.uid == uid)
                .unwrap()
                .message
                .id
        };

        // Coffee (two rows once open), Lunch, then Plan and its replies.
        let place = store
            .merged_thread_row(&folders, id_of(3), &[])
            .unwrap()
            .unwrap();
        assert_eq!((place.row, place.expand), (4, true));
        assert_eq!(place.thread, "<a@x>");
        let (rows, _) = store
            .thread_list("A", "INBOX", 0, 10, std::slice::from_ref(&place.thread))
            .unwrap();
        assert_eq!(rows[place.row].message.uid, 3);

        // The open Coffee thread above pushes it down by its reply.
        let place = store
            .merged_thread_row(&folders, id_of(3), &["<g@x>".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(place.row, 5);

        // A thread of one has nothing to open.
        let place = store
            .merged_thread_row(&folders, id_of(4), &[])
            .unwrap()
            .unwrap();
        assert_eq!((place.row, place.expand), (1, false));

        let other = [("A".to_string(), "Missing".to_string())];
        assert_eq!(
            store.merged_thread_row(&other, id_of(4), &[]).unwrap(),
            None
        );
    }

    #[test]
    fn a_message_is_found_in_several_inboxes_read_as_one_list() {
        let (_dir, store) = store_with(&[
            stored(1, "Plan", "<a@x>", None, "2026-01-01T00:00:00Z"),
            stored(2, "Lunch", "<b@x>", None, "2026-01-03T00:00:00Z"),
        ]);
        let mut db = store.writer().unwrap();
        let other = db.get_or_create_folder("A", "Other/INBOX").unwrap();
        let mut later = stored(1, "Later", "<z@x>", None, "2026-01-02T00:00:00Z");
        later.folder_id = other;
        db.transaction(|tx| {
            Database::bulk_insert_messages_in_tx(tx, &[later])?;
            Ok(())
        })
        .unwrap();
        drop(db);

        let folders = [
            ("A".to_string(), "INBOX".to_string()),
            ("A".to_string(), "Other/INBOX".to_string()),
        ];
        let list = store
            .get_merged_message_list(&folders, 0, 10, crate::store::MessageSort::DateNewest)
            .unwrap();
        for (row, (_, item)) in list.iter().enumerate() {
            let found = store
                .merged_message_row(&folders, item.id, crate::store::MessageSort::DateNewest)
                .unwrap();
            assert_eq!(found, Some(row));
        }
        let inbox = [("A".to_string(), "INBOX".to_string())];
        let later = list[1].1.id;
        assert_eq!(
            store
                .merged_message_row(&inbox, later, crate::store::MessageSort::DateNewest)
                .unwrap(),
            None
        );
    }

    #[test]
    fn several_inboxes_read_as_one_threaded_list() {
        let (_dir, store) = store_with(&[
            stored(1, "Plan", "<a@x>", None, "2026-01-01T00:00:00Z"),
            stored(
                2,
                "Re: Plan",
                "<b@x>",
                Some("<a@x>"),
                "2026-01-03T00:00:00Z",
            ),
        ]);
        let mut db = store.writer().unwrap();
        let other = db.get_or_create_folder("A", "Other/INBOX").unwrap();
        let mut lunch = stored(1, "Lunch", "<l@x>", None, "2026-01-02T00:00:00Z");
        lunch.folder_id = other;
        let mut later = stored(2, "Later", "<z@x>", None, "2026-01-04T00:00:00Z");
        later.folder_id = other;
        db.transaction(|tx| {
            Database::bulk_insert_messages_in_tx(tx, &[lunch, later])?;
            Ok(())
        })
        .unwrap();
        drop(db);

        let folders = [
            ("A".to_string(), "INBOX".to_string()),
            ("A".to_string(), "Other/INBOX".to_string()),
            ("A".to_string(), "Missing".to_string()),
        ];
        let (rows, total) = store
            .merged_thread_list(&folders, 0, 10, &["<a@x>".to_string()])
            .expect("list");
        assert_eq!(total, 4);
        let summary: Vec<(&str, &str, u32)> = rows
            .iter()
            .map(|(location, row)| {
                (
                    location.1.as_str(),
                    row.message.subject.as_deref().unwrap_or(""),
                    row.depth,
                )
            })
            .collect();
        // Newest thread first, whichever inbox it is in.
        assert_eq!(
            summary,
            vec![
                ("Other/INBOX", "Later", 0),
                ("INBOX", "Plan", 0),
                ("INBOX", "Re: Plan", 1),
                ("Other/INBOX", "Lunch", 0),
            ]
        );
    }

    #[test]
    fn a_collapsed_thread_s_row_carries_its_members() {
        let (_dir, store) = store_with(&[
            stored(1, "Plan", "<a@x>", None, "2026-01-01T00:00:00Z"),
            stored(2, "Lunch", "<l@x>", None, "2026-01-02T00:00:00Z"),
            stored(
                3,
                "Re: Plan",
                "<b@x>",
                Some("<a@x>"),
                "2026-01-03T00:00:00Z",
            ),
        ]);
        let mut db = store.writer().unwrap();
        let other = db.get_or_create_folder("A", "Other/INBOX").unwrap();
        let mut later = stored(1, "Later", "<z@x>", None, "2026-01-04T00:00:00Z");
        later.folder_id = other;
        db.transaction(|tx| {
            Database::bulk_insert_messages_in_tx(tx, &[later])?;
            Ok(())
        })
        .unwrap();
        drop(db);
        let folders = [
            ("A".to_string(), "INBOX".to_string()),
            ("A".to_string(), "Other/INBOX".to_string()),
        ];

        // Collapsed, the thread's row stands for both members, oldest first,
        // each with its folder; a thread of one carries none.
        let (rows, _) = store.merged_thread_list(&folders, 0, 10, &[]).unwrap();
        let members = |row: &ThreadRow| -> Vec<(String, u32)> {
            row.members
                .iter()
                .map(|(location, item)| (location.1.clone(), item.uid))
                .collect()
        };
        let subjects: Vec<&str> = rows
            .iter()
            .map(|(_, row)| row.message.subject.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(subjects, vec!["Later", "Plan", "Lunch"]);
        assert!(rows[0].1.members.is_empty());
        assert_eq!(
            members(&rows[1].1),
            vec![("INBOX".to_string(), 1), ("INBOX".to_string(), 3)]
        );
        assert!(rows[2].1.members.is_empty());

        // Expanded, each row is a message of its own again.
        let (rows, _) = store
            .merged_thread_list(&folders, 0, 10, &["<a@x>".to_string()])
            .unwrap();
        assert!(rows.iter().all(|(_, row)| row.members.is_empty()));

        // The members of threads by key, in the order asked, each thread once.
        let found = store
            .merged_thread_members(
                &folders,
                &[
                    "<z@x>".to_string(),
                    "<a@x>".to_string(),
                    "<z@x>".to_string(),
                ],
            )
            .unwrap();
        let summary: Vec<(&str, &str)> = found
            .iter()
            .map(|(location, item)| (location.1.as_str(), item.subject.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Other/INBOX", "Later"),
                ("INBOX", "Plan"),
                ("INBOX", "Re: Plan")
            ]
        );
        // A folder the list does not read from is not asked.
        let inbox = [("A".to_string(), "INBOX".to_string())];
        let found = store
            .merged_thread_members(&inbox, &["<z@x>".to_string()])
            .unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn items_are_ranked_among_the_inbox_rows_by_date() {
        let (_dir, store) = store_with(&[
            stored(1, "Plan", "<a@x>", None, "2026-01-01T00:00:00Z"),
            stored(
                2,
                "Re: Plan",
                "<b@x>",
                Some("<a@x>"),
                "2026-01-03T00:00:00Z",
            ),
        ]);
        let mut db = store.writer().unwrap();
        let other = db.get_or_create_folder("A", "Other/INBOX").unwrap();
        let mut lunch = stored(1, "Lunch", "<l@x>", None, "2026-01-02T00:00:00Z");
        lunch.folder_id = other;
        let mut later = stored(2, "Later", "<z@x>", None, "2026-01-04T00:00:00Z");
        later.folder_id = other;
        db.transaction(|tx| {
            Database::bulk_insert_messages_in_tx(tx, &[lunch, later])?;
            Ok(())
        })
        .unwrap();
        drop(db);

        let folders = [
            ("A".to_string(), "INBOX".to_string()),
            ("A".to_string(), "Other/INBOX".to_string()),
            ("A".to_string(), "Missing".to_string()),
        ];
        let at = |date: &str| {
            chrono::DateTime::parse_from_rfc3339(date)
                .unwrap()
                .timestamp()
        };
        // Before everything, between Lunch and Re: Plan, as new as Later.
        let times = [
            at("2026-01-05T00:00:00Z"),
            at("2026-01-02T12:00:00Z"),
            at("2026-01-04T00:00:00Z"),
        ];
        assert_eq!(
            store.merged_message_ranks(&folders, &times, true).unwrap(),
            vec![0, 2, 0]
        );
        assert_eq!(
            store.merged_message_ranks(&folders, &times, false).unwrap(),
            vec![4, 2, 3]
        );
        // Threads: Later, Plan (newest 01-03), Lunch; expanded, Plan shows two rows.
        assert_eq!(
            store.merged_thread_ranks(&folders, &times, &[]).unwrap(),
            vec![0, 2, 0]
        );
        assert_eq!(
            store
                .merged_thread_ranks(&folders, &times, &["<a@x>".to_string()])
                .unwrap(),
            vec![0, 3, 0]
        );
    }

    #[test]
    fn a_conversation_is_followed_into_the_other_folders() {
        let (_dir, store) = store_with(&[
            stored(1, "Plan", "<a@x>", None, "2026-01-01T00:00:00Z"),
            stored(
                2,
                "Re: Plan",
                "<b@x>",
                Some("<a@x>"),
                "2026-01-02T00:00:00Z",
            ),
        ]);
        // The answer sent from here lives in Sent, and the answer to that
        // came back to INBOX naming only the Sent message.
        let mut db = store.writer().unwrap();
        let sent = db.get_or_create_folder("A", "Sent").unwrap();
        let mut reply = stored(
            1,
            "Re: Plan",
            "<s@x>",
            Some("<b@x>"),
            "2026-01-03T00:00:00Z",
        );
        reply.folder_id = sent;
        let answer = stored(
            3,
            "Re: Plan",
            "<c@x>",
            Some("<s@x>"),
            "2026-01-04T00:00:00Z",
        );
        db.transaction(|tx| {
            Database::bulk_insert_messages_in_tx(tx, &[reply, answer])?;
            Ok(())
        })
        .unwrap();
        let root_id: i64 = db
            .conn
            .query_row(
                "SELECT id FROM messages WHERE message_id = '<a@x>'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let conversation = store.thread_messages(root_id).expect("conversation");
        let seen: Vec<(String, String)> = conversation
            .iter()
            .map(|hit| {
                (
                    hit.folder_name.clone(),
                    hit.message.message_id.clone().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                ("INBOX".to_string(), "<a@x>".to_string()),
                ("INBOX".to_string(), "<b@x>".to_string()),
                ("Sent".to_string(), "<s@x>".to_string()),
                ("INBOX".to_string(), "<c@x>".to_string()),
            ]
        );
    }
}
