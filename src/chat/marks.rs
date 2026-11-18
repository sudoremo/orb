//! Waiting's marks on every Mac (chat-spec §6).
//!
//! Done and task marks are the user's own decisions about a thread, and a
//! mute (chat-spec §13) one about a room, so they
//! travel to the other Macs through the tasks account, as the tag registry
//! does (todo-spec §7): one message in the tasks folder holding every mark,
//! under a well-known id, synced by revision and told apart by `Message-ID`.
//! Two Macs that both changed marks since the copy they share are merged
//! thread by thread against it ([`merge`]), so neither loses its marks.
//!
//! The set this Mac knows is kept in `chat_state`: the revision, whether it
//! has changes the server has not seen, which copy it is, and the base —
//! the marks as the copy this Mac last wrote or read had them.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::store;

/// How long a mark travels: one older than this is left out of what is
/// uploaded. A thread nobody wrote in for that long is past waiting on the
/// other Macs, and the folder's message does not grow for good.
pub const KEEP_DAYS: i64 = 120;

const KEY_REVISION: &str = "marks:revision";
const KEY_DIRTY: &str = "marks:dirty";
const KEY_MESSAGE: &str = "marks:message_id";
const KEY_BASE: &str = "marks:base";

/// One mark as it travels: the thread by server, room and thread id, and the
/// mark's kind and time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMark {
    /// The server's host.
    pub server: String,
    pub room: String,
    /// The thread's id; empty for a room's own timeline.
    pub thread: String,
    /// `done`, `task`, or `mute` on the thread [`store::MUTE_THREAD`].
    pub kind: String,
    /// The thread's newest message when it was marked, in ms; for a mute,
    /// when it ends.
    pub at: i64,
}

/// How far ahead of now a done or task mark may be dated and still be taken
/// in: a clock a little fast on another Mac, not a mark pinned past every
/// message the thread will ever get. A mute is dated by when it ends, and
/// may end never.
const MARK_LEAD_MS: i64 = 24 * 60 * 60 * 1000;

impl SharedMark {
    fn key(&self) -> (String, String, String) {
        (self.server.clone(), self.room.clone(), self.thread.clone())
    }

    /// Whether a mark another Mac sent is one this Mac could have made. The
    /// message carrying it is written by whoever can reach the tasks folder,
    /// so a kind the store does not know, a mark naming nothing, or one dated
    /// into the far future is left out rather than applied.
    pub fn is_well_formed(&self, now_ms: i64) -> bool {
        let known = matches!(
            self.kind.as_str(),
            store::MARK_DONE | store::MARK_TASK | store::MARK_MUTE
        );
        let named = !self.server.is_empty() && !self.room.is_empty();
        let timely =
            self.kind == store::MARK_MUTE || self.at <= now_ms.saturating_add(MARK_LEAD_MS);
        known && named && timely
    }
}

/// Where this Mac's marks stand against the server's copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarksState {
    pub revision: i64,
    /// Changes made here that the server has not seen.
    pub dirty: bool,
    /// Which copy this Mac last wrote or read.
    pub message_id: Option<String>,
}

fn has_chat(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'chat_state')",
        [],
        |row| row.get(0),
    )
}

pub fn state(conn: &Connection) -> rusqlite::Result<MarksState> {
    if !has_chat(conn)? {
        return Ok(MarksState::default());
    }
    Ok(MarksState {
        revision: store::state_i64(conn, KEY_REVISION)?.unwrap_or(0),
        dirty: store::state_i64(conn, KEY_DIRTY)?.unwrap_or(0) != 0,
        message_id: store::state(conn, KEY_MESSAGE)?,
    })
}

/// A mark was set or taken back here: the server has to hear of it.
pub fn touch(conn: &Connection) -> rusqlite::Result<()> {
    let revision = store::state_i64(conn, KEY_REVISION)?.unwrap_or(0) + 1;
    store::set_state(conn, KEY_REVISION, &revision.to_string())?;
    store::set_state(conn, KEY_DIRTY, "1")
}

fn base(conn: &Connection) -> rusqlite::Result<Vec<SharedMark>> {
    Ok(store::state(conn, KEY_BASE)?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default())
}

/// The server this database's chat tables are of, by host.
fn host(conn: &Connection) -> rusqlite::Result<Option<String>> {
    Ok(store::state(conn, store::STATE_SERVER)?
        .map(|server| super::ChatLink::new(&server, "", "", "").host))
}

/// The marks this Mac has: its own server's from `chat_marks`, and any other
/// server's as the base has them, which this Mac never changes.
pub fn local(conn: &Connection) -> rusqlite::Result<Vec<SharedMark>> {
    let host = host(conn)?;
    let mut marks: Vec<SharedMark> = base(conn)?
        .into_iter()
        .filter(|m| Some(&m.server) != host.as_ref())
        .collect();
    if let Some(host) = host {
        let mut stmt = conn.prepare("SELECT rid, thread, kind, at FROM chat_marks")?;
        let rows = stmt.query_map([], |row| {
            Ok(SharedMark {
                server: host.clone(),
                room: row.get(0)?,
                thread: row.get(1)?,
                kind: row.get(2)?,
                at: row.get(3)?,
            })
        })?;
        for row in rows {
            marks.push(row?);
        }
    }
    marks.sort_by_key(SharedMark::key);
    Ok(marks)
}

/// What goes up: the marks of the last [`KEEP_DAYS`], and the mutes that
/// have not ended, whose `at` is when they end.
pub fn outgoing(conn: &Connection, now_ms: i64) -> rusqlite::Result<(Vec<SharedMark>, i64)> {
    let cutoff = now_ms - KEEP_DAYS * 86_400_000;
    let marks = local(conn)?
        .into_iter()
        .filter(|m| {
            if m.kind == store::MARK_MUTE {
                m.at > now_ms
            } else {
                m.at >= cutoff
            }
        })
        .collect();
    Ok((marks, state(conn)?.revision))
}

/// Two sets of marks merged against the set they last shared, thread by
/// thread. A mark either side set or changed is kept; one either side took
/// back, and the other left as it was, stays gone; a thread both sides
/// marked differently keeps the later mark.
pub fn merge(base: &[SharedMark], local: &[SharedMark], remote: &[SharedMark]) -> Vec<SharedMark> {
    fn index(marks: &[SharedMark]) -> BTreeMap<(String, String, String), &SharedMark> {
        marks.iter().map(|m| (m.key(), m)).collect()
    }
    fn later<'a>(a: &'a SharedMark, b: &'a SharedMark) -> &'a SharedMark {
        if b.at > a.at { b } else { a }
    }
    let (base, ours, theirs) = (index(base), index(local), index(remote));
    let mut keys: Vec<&(String, String, String)> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter_map(|key| {
            let kept = match (base.get(key), ours.get(key), theirs.get(key)) {
                (Some(b), Some(l), Some(r)) => Some(if l == b {
                    *r
                } else if r == b {
                    *l
                } else {
                    later(l, r)
                }),
                (Some(b), Some(l), None) => (l != b).then_some(*l),
                (Some(b), None, Some(r)) => (r != b).then_some(*r),
                (None, Some(l), Some(r)) => Some(later(l, r)),
                (_, Some(l), None) => Some(*l),
                (_, None, Some(r)) => Some(*r),
                (_, None, None) => None,
            };
            kept.cloned()
        })
        .collect()
}

/// Make this Mac's marks `marks`: its own server's into `chat_marks`, every
/// one into the base.
fn write(conn: &Connection, marks: &[SharedMark]) -> rusqlite::Result<()> {
    if let Some(host) = host(conn)? {
        let mine: Vec<&SharedMark> = marks.iter().filter(|m| m.server == host).collect();
        let before = local(conn)?;
        for gone in before
            .iter()
            .filter(|b| b.server == host && !mine.iter().any(|m| m.key() == b.key()))
        {
            store::remove_mark(conn, &gone.room, &gone.thread)?;
        }
        for mark in mine {
            store::set_mark(conn, &mark.room, &mark.thread, &mark.kind, mark.at)?;
        }
        store::bump_version(conn)?;
    }
    Ok(())
}

fn set_base(conn: &Connection, marks: &[SharedMark]) -> rusqlite::Result<()> {
    let json = serde_json::to_string(marks).unwrap_or_else(|_| "[]".into());
    store::set_state(conn, KEY_BASE, &json)
}

/// Take in another Mac's copy, which this Mac had not seen: merged with
/// what this Mac has against the base, so a mark older than what travels
/// stays here. Clean afterwards, unless this Mac has changes of its own to
/// send over it, which then go up at a revision above both.
pub fn take_in(
    conn: &Connection,
    remote: &[SharedMark],
    revision: i64,
    message_id: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let remote: Vec<SharedMark> = remote
        .iter()
        .filter(|mark| mark.is_well_formed(now_ms))
        .cloned()
        .collect();
    let tx = conn.unchecked_transaction()?;
    let dirty = state(&tx)?.dirty;
    let merged = merge(&base(&tx)?, &local(&tx)?, &remote);
    write(&tx, &merged)?;
    set_base(&tx, &remote)?;
    let revision = if dirty {
        state(&tx)?.revision.max(revision) + 1
    } else {
        revision
    };
    store::set_state(&tx, KEY_REVISION, &revision.to_string())?;
    store::set_state(&tx, KEY_DIRTY, if dirty { "1" } else { "0" })?;
    match message_id {
        Some(id) => store::set_state(&tx, KEY_MESSAGE, id)?,
        None => {
            tx.execute(
                "DELETE FROM chat_state WHERE key = ?1",
                params![KEY_MESSAGE],
            )?;
        }
    }
    tx.commit()
}

/// The copy at `revision` reached the server as `message_id`. Clean, unless
/// a mark changed here while it was on its way.
pub fn uploaded(
    conn: &Connection,
    revision: i64,
    message_id: &str,
    marks: &[SharedMark],
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    store::set_state(&tx, KEY_MESSAGE, message_id)?;
    set_base(&tx, marks)?;
    let clean = state(&tx)?.revision == revision;
    if clean {
        store::set_state(&tx, KEY_DIRTY, "0")?;
    }
    tx.commit()?;
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(thread: &str, kind: &str, at: i64) -> SharedMark {
        SharedMark {
            server: "chat.example.com".into(),
            room: "r".into(),
            thread: thread.into(),
            kind: kind.into(),
            at,
        }
    }

    #[test]
    fn a_merge_keeps_what_either_side_did() {
        let base = [
            mark("a", "done", 10),
            mark("b", "done", 10),
            mark("c", "done", 10),
        ];
        // Here: a taken back, d marked. There: b taken back, e marked, c done later.
        let local = [
            mark("b", "done", 10),
            mark("c", "done", 10),
            mark("d", "task", 20),
        ];
        let remote = [
            mark("a", "done", 10),
            mark("c", "done", 30),
            mark("e", "done", 25),
        ];
        let merged = merge(&base, &local, &remote);
        let threads: Vec<(&str, i64)> = merged.iter().map(|m| (m.thread.as_str(), m.at)).collect();
        assert_eq!(threads, [("c", 30), ("d", 20), ("e", 25)]);
    }

    #[test]
    fn a_thread_both_sides_marked_keeps_the_later_mark() {
        let merged = merge(&[], &[mark("a", "task", 40)], &[mark("a", "done", 30)]);
        assert_eq!(merged, [mark("a", "task", 40)]);
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::CHAT_SCHEMA).unwrap();
        store::set_state(&conn, store::STATE_SERVER, "https://chat.example.com").unwrap();
        conn
    }

    /// The message carrying another Mac's marks is written by whoever can
    /// reach the tasks folder, so a mark this Mac could not have made is left
    /// out: an unknown kind, one naming no room, a done mark dated into the
    /// far future. A mute that never ends is a mark the user makes.
    #[test]
    fn a_mark_this_mac_could_not_have_made_is_left_out() {
        let conn = db();
        let now = 1_000_000;
        let marks = [
            mark("a", "done", 10),
            mark("b", "boom", 10),
            SharedMark {
                room: "".into(),
                ..mark("c", "done", 10)
            },
            mark("d", "done", i64::MAX),
            mark("e", "task", now + MARK_LEAD_MS + 1),
            mark("f", "task", now + MARK_LEAD_MS),
            mark(store::MUTE_THREAD, store::MARK_MUTE, store::MUTE_FOREVER),
        ];
        take_in(&conn, &marks, 1, Some("<m@x>"), now).unwrap();
        let kept: Vec<String> = local(&conn)
            .unwrap()
            .into_iter()
            .map(|m| m.thread)
            .collect();
        assert_eq!(kept, ["*", "a", "f"]);
    }

    #[test]
    fn another_macs_marks_come_in_and_ones_own_go_up() {
        let conn = db();
        store::set_mark(&conn, "r", "a", "done", 10).unwrap();
        touch(&conn).unwrap();
        assert_eq!(
            state(&conn).unwrap(),
            MarksState {
                revision: 1,
                dirty: true,
                message_id: None
            }
        );

        // The other Mac had b marked, and a server this one does not use.
        let elsewhere = SharedMark {
            server: "other.example.com".into(),
            ..mark("x", "done", 5)
        };
        take_in(
            &conn,
            &[mark("b", "done", 20), elsewhere.clone()],
            3,
            Some("<m@x>"),
            1_000,
        )
        .unwrap();
        assert_eq!(
            store::mark(&conn, "r", "b").unwrap(),
            Some(("done".into(), 20))
        );
        assert_eq!(
            store::mark(&conn, "r", "a").unwrap(),
            Some(("done".into(), 10))
        );
        let s = state(&conn).unwrap();
        assert!(s.dirty);
        assert_eq!(s.revision, 4);

        let (up, revision) = outgoing(&conn, 30).unwrap();
        assert_eq!(up.len(), 3, "both Macs' marks and the other server's");
        assert!(up.contains(&elsewhere));
        assert!(uploaded(&conn, revision, "<n@x>", &up).unwrap());
        assert!(!state(&conn).unwrap().dirty);

        // Taken back on the other Mac: gone here too.
        take_in(
            &conn,
            &[mark("b", "done", 20), elsewhere],
            5,
            Some("<o@x>"),
            1_000,
        )
        .unwrap();
        assert_eq!(store::mark(&conn, "r", "a").unwrap(), None);
        assert_eq!(state(&conn).unwrap().revision, 5);
    }

    #[test]
    fn a_mute_travels_until_it_ends_however_long_ago_it_was_set() {
        let conn = db();
        let now = KEEP_DAYS * 86_400_000 * 2;
        store::set_mark(
            &conn,
            "a",
            store::MUTE_THREAD,
            store::MARK_MUTE,
            store::MUTE_FOREVER,
        )
        .unwrap();
        store::set_mark(&conn, "b", store::MUTE_THREAD, store::MARK_MUTE, now + 1).unwrap();
        store::set_mark(&conn, "c", store::MUTE_THREAD, store::MARK_MUTE, now - 1).unwrap();
        let (up, _) = outgoing(&conn, now).unwrap();
        let rooms: Vec<&str> = up.iter().map(|m| m.room.as_str()).collect();
        assert_eq!(rooms, ["a", "b"]);
    }

    #[test]
    fn two_macs_muting_one_room_keep_the_longer_mute() {
        let mute = |at| SharedMark {
            thread: store::MUTE_THREAD.into(),
            ..mark("", store::MARK_MUTE, at)
        };
        let merged = merge(&[], &[mute(100)], &[mute(store::MUTE_FOREVER)]);
        assert_eq!(merged, [mute(store::MUTE_FOREVER)]);
        // Unmuted on the other Mac, left alone here: unmuted.
        assert!(merge(&[mute(100)], &[mute(100)], &[]).is_empty());
    }

    #[test]
    fn old_marks_stay_here_but_do_not_travel() {
        let conn = db();
        store::set_mark(&conn, "r", "old", "done", 0).unwrap();
        store::set_mark(&conn, "r", "new", "done", KEEP_DAYS * 86_400_000).unwrap();
        let (up, _) = outgoing(&conn, KEEP_DAYS * 86_400_000 + 1).unwrap();
        let threads: Vec<&str> = up.iter().map(|m| m.thread.as_str()).collect();
        assert_eq!(threads, ["new"]);
        uploaded(&conn, 0, "<m@x>", &up).unwrap();
        take_in(&conn, &up, 1, Some("<m@x>"), 1_000).unwrap();
        assert!(store::mark(&conn, "r", "old").unwrap().is_some());
    }
}
