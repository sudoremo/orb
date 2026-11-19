//! The chat tables (schema v32) and what is read from them.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::client::{WireAttachment, WireMessage, WireRoom, WireSubscription};

/// What takes a thread out of Waiting until someone writes again.
pub const MARK_DONE: &str = "done";
pub const MARK_TASK: &str = "task";

/// A mute is a mark of its room on this reserved thread, which no message id
/// can be, so that it travels with the marks (chat-spec §13). Its `at` is
/// when the mute ends, [`MUTE_FOREVER`] for one the user ends.
pub const MUTE_THREAD: &str = "*";
pub const MARK_MUTE: &str = "mute";
pub const MUTE_FOREVER: i64 = i64::MAX;

/// Keys of `chat_state`.
pub const STATE_SERVER: &str = "server";
pub const STATE_USER_ID: &str = "user_id";
pub const STATE_USERNAME: &str = "username";
/// Messages older than this (milliseconds) never make a thread wait.
pub const STATE_BASELINE: &str = "baseline";
pub const STATE_SUBSCRIPTIONS_SYNCED: &str = "subscriptions_synced";
pub const STATE_ROOMS_SYNCED: &str = "rooms_synced";
/// Counts the writes a sync made, so every Orb on the machine can tell the
/// database changed under it.
pub const STATE_VERSION: &str = "version";
/// Whether the server has threads turned on (`Threads_enabled`), as it last
/// said: `1` or `0`.
pub const STATE_THREADS: &str = "threads";

pub fn state(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM chat_state WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .optional()
}

pub fn state_i64(conn: &Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    Ok(state(conn, key)?.and_then(|v| v.parse().ok()))
}

pub fn set_state(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO chat_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn version(conn: &Connection) -> rusqlite::Result<i64> {
    Ok(state_i64(conn, STATE_VERSION)?.unwrap_or(0))
}

pub fn bump_version(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO chat_state (key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
        [STATE_VERSION],
    )?;
    Ok(())
}

/// Forget everything of the server, for a session on another server or of
/// another user. The marks go too: they name rooms of the old server.
pub fn wipe(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DELETE FROM chat_messages; DELETE FROM chat_rooms; DELETE FROM chat_marks;
         DELETE FROM chat_seen; DELETE FROM chat_state;",
    )
}

pub fn upsert_subscription(conn: &Connection, sub: &WireSubscription) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO chat_rooms (rid, kind, name, fname, subscribed, unread, user_mentions)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(rid) DO UPDATE SET
            kind = excluded.kind,
            name = COALESCE(excluded.name, chat_rooms.name),
            fname = COALESCE(excluded.fname, chat_rooms.fname),
            subscribed = excluded.subscribed,
            unread = excluded.unread,
            user_mentions = excluded.user_mentions",
        params![
            sub.rid,
            sub.t.as_deref().unwrap_or("c"),
            sub.name,
            sub.fname,
            sub.open.unwrap_or(true),
            sub.unread.unwrap_or(0),
            sub.user_mentions.unwrap_or(0),
        ],
    )?;
    let notify = if sub.disable_notifications == Some(true) {
        NOTIFY_NOTHING
    } else {
        match sub.desktop_notifications.as_deref() {
            Some(NOTIFY_NOTHING) => NOTIFY_NOTHING,
            Some(NOTIFY_MENTIONS) => NOTIFY_MENTIONS,
            _ => NOTIFY_ALL,
        }
    };
    set_state(conn, &notify_key(&sub.rid), notify)
}

/// What the server's notification preference lets through for a room.
pub const NOTIFY_ALL: &str = "all";
pub const NOTIFY_MENTIONS: &str = "mentions";
pub const NOTIFY_NOTHING: &str = "nothing";

fn notify_key(rid: &str) -> String {
    format!("notify:{rid}")
}

/// The server's notification preference for a room: [`NOTIFY_ALL`] until
/// its subscription says otherwise.
pub fn room_notifies(conn: &Connection, rid: &str) -> rusqlite::Result<String> {
    Ok(state(conn, &notify_key(rid))?.unwrap_or_else(|| NOTIFY_ALL.to_string()))
}

/// Record a room's details. Its name and display name are kept when the room
/// has none of its own, as a direct message has not: those come from the
/// subscription.
pub fn upsert_room(conn: &Connection, room: &WireRoom) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO chat_rooms (rid, kind, name, fname, topic, users_count, last_message_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(rid) DO UPDATE SET
            kind = excluded.kind,
            name = COALESCE(chat_rooms.name, excluded.name),
            fname = COALESCE(chat_rooms.fname, excluded.fname),
            topic = excluded.topic,
            users_count = excluded.users_count,
            last_message_at = MAX(COALESCE(chat_rooms.last_message_at, 0), COALESCE(excluded.last_message_at, 0)),
            updated_at = excluded.updated_at",
        params![
            room.id,
            room.t,
            room.name,
            room.fname,
            room.topic,
            room.users_count,
            room.lm,
            room.updated_at.unwrap_or(0),
        ],
    )?;
    Ok(())
}

/// Forget a room the user left, with its messages and marks.
pub fn remove_room(conn: &Connection, rid: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM chat_messages WHERE rid = ?1", [rid])?;
    conn.execute("DELETE FROM chat_marks WHERE rid = ?1", [rid])?;
    conn.execute("DELETE FROM chat_seen WHERE rid = ?1", [rid])?;
    conn.execute("DELETE FROM chat_rooms WHERE rid = ?1", [rid])?;
    conn.execute("DELETE FROM chat_state WHERE key = ?1", [notify_key(rid)])?;
    Ok(())
}

/// The subscribed rooms whose newest message is newer than what has been
/// synced of them, with the time to sync each from.
pub fn rooms_behind(conn: &Connection, default_since: i64) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT rid, synced_until FROM chat_rooms
         WHERE subscribed = 1 AND last_message_at IS NOT NULL
           AND (synced_until IS NULL OR last_message_at > synced_until)",
    )?;
    let rows = stmt.query_map([], |row| {
        let since: Option<i64> = row.get(1)?;
        Ok((row.get::<_, String>(0)?, since.unwrap_or(default_since)))
    })?;
    rows.collect()
}

/// When the room was last synced up to; `None` if never.
pub fn synced_until(conn: &Connection, rid: &str) -> rusqlite::Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT synced_until FROM chat_rooms WHERE rid = ?1",
            [rid],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

pub fn set_synced_until(conn: &Connection, rid: &str, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE chat_rooms SET synced_until = MAX(COALESCE(synced_until, 0), ?2),
            last_message_at = MAX(COALESCE(last_message_at, 0), ?2)
         WHERE rid = ?1",
        params![rid, at],
    )?;
    Ok(())
}

/// A file attached to a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatFile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The server path the file is downloaded from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The server path of an image's preview, and the image's size in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// Largest image shown as its own preview when the server made no thumbnail.
const PREVIEW_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// A file's attachment: the one linking to it by id, or for a file without
/// one, the one at its place — a quote has an attachment too, and can come
/// first.
fn attachment_of<'a>(
    message: &'a WireMessage,
    index: usize,
    id: Option<&str>,
) -> Option<&'a WireAttachment> {
    match id {
        Some(id) => {
            let segment = format!("/{id}/");
            message.attachments.iter().find(|a| {
                a.title_link
                    .as_deref()
                    .is_some_and(|link| link.contains(&segment))
            })
        }
        None => message
            .attachments
            .get(index)
            .filter(|a| a.message_link.is_none()),
    }
}

/// Whether the file is only the preview of another: an attachment shows it,
/// and none links to it.
fn is_preview_only(message: &WireMessage, id: Option<&str>) -> bool {
    let Some(id) = id else { return false };
    let segment = format!("/{id}/");
    let names = |link: &Option<String>| link.as_deref().is_some_and(|l| l.contains(&segment));
    message.attachments.iter().any(|a| names(&a.image_url))
        && !message.attachments.iter().any(|a| names(&a.title_link))
}

fn file_of(
    name: String,
    mime: Option<String>,
    size: Option<u64>,
    attachment: Option<&WireAttachment>,
) -> ChatFile {
    let path = attachment.and_then(|a| a.title_link.clone());
    let preview = attachment
        .and_then(|a| a.image_url.clone())
        .filter(|url| Some(url) != path.as_ref() || size.is_some_and(|s| s <= PREVIEW_MAX_BYTES));
    let dimensions = attachment.and_then(|a| a.image_dimensions);
    ChatFile {
        name,
        mime,
        size,
        path,
        preview,
        width: dimensions.map(|d| d.width),
        height: dimensions.map(|d| d.height),
    }
}

/// Leave out the thumbnail the server lists beside an image it made one of:
/// `thumb-` and the image's name, with no address of its own. It is the
/// image's preview, not a file anyone sent.
fn without_thumbnails(mut files: Vec<ChatFile>) -> Vec<ChatFile> {
    let names: HashSet<String> = files.iter().map(|f| f.name.clone()).collect();
    files.retain(|f| {
        f.path.is_some()
            || !f
                .name
                .strip_prefix("thumb-")
                .is_some_and(|image| names.contains(image))
    });
    files
}

fn files_from_json(json: Option<String>) -> Vec<ChatFile> {
    json.and_then(|f| serde_json::from_str(&f).ok())
        .map(without_thumbnails)
        .unwrap_or_default()
}

/// A message another message quotes, as the quoting message carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatQuote {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub text: String,
    /// The quoted message's permalink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// A reaction, with how many gave it and whether the user is among them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatReaction {
    pub emoji: String,
    pub count: usize,
    pub mine: bool,
}

/// Whether `message` names the user, by id or by username.
fn mentions(message: &WireMessage, user_id: &str, username: &str) -> bool {
    message
        .mentions
        .iter()
        .any(|m| m.id.as_deref() == Some(user_id) || m.username.as_deref() == Some(username))
}

/// Whether `message` calls the whole room, with `@here` or `@all`.
fn mentions_everyone(message: &WireMessage) -> bool {
    message.mentions.iter().any(|m| {
        [m.id.as_deref(), m.username.as_deref()]
            .into_iter()
            .flatten()
            .any(|name| name == "here" || name == "all")
    })
}

fn files_of(message: &WireMessage) -> Vec<ChatFile> {
    let mut files: Vec<ChatFile> = message
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| !is_preview_only(message, f.id.as_deref()))
        .map(|(i, f)| {
            file_of(
                f.name.clone().unwrap_or_else(|| "file".to_string()),
                f.mime.clone(),
                f.size,
                attachment_of(message, i, f.id.as_deref()),
            )
        })
        .collect();
    if files.is_empty() {
        files = message
            .attachments
            .iter()
            .filter(|a| a.title_link.is_some())
            .map(|a| {
                file_of(
                    a.title.clone().unwrap_or_else(|| "file".to_string()),
                    None,
                    None,
                    Some(a),
                )
            })
            .collect();
    }
    without_thumbnails(files)
}

fn quotes_of(message: &WireMessage) -> Vec<ChatQuote> {
    message
        .attachments
        .iter()
        .filter(|a| a.message_link.is_some())
        .map(|a| ChatQuote {
            author: a.author_name.clone(),
            text: a.text.clone().unwrap_or_default(),
            link: a.message_link.clone(),
        })
        .collect()
}

/// Store a message as the server has it now.
pub fn upsert_message(
    conn: &Connection,
    message: &WireMessage,
    user_id: &str,
    username: &str,
) -> rusqlite::Result<()> {
    let files = files_of(message);
    let files = (!files.is_empty()).then(|| serde_json::to_string(&files).unwrap_or_default());
    let quotes = quotes_of(message);
    let quotes = (!quotes.is_empty()).then(|| serde_json::to_string(&quotes).unwrap_or_default());
    let reactions = message
        .reactions
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|r| {
            let map: HashMap<&String, &Vec<String>> =
                r.iter().map(|(k, v)| (k, &v.usernames)).collect();
            serde_json::to_string(&map).unwrap_or_default()
        });
    conn.execute(
        "INSERT INTO chat_messages (id, rid, tmid, author_id, author_username, author_name, body,
            ts, updated_at, edited_at, system_type, tcount, tlm, mentions_me, files, reactions, quotes,
            mentions_all)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
         ON CONFLICT(id) DO UPDATE SET
            rid = excluded.rid, tmid = excluded.tmid, author_id = excluded.author_id,
            author_username = excluded.author_username, author_name = excluded.author_name,
            body = excluded.body, ts = excluded.ts, updated_at = excluded.updated_at,
            edited_at = excluded.edited_at, system_type = excluded.system_type,
            tcount = excluded.tcount, tlm = excluded.tlm, mentions_me = excluded.mentions_me,
            files = excluded.files, reactions = excluded.reactions, quotes = excluded.quotes,
            mentions_all = excluded.mentions_all",
        params![
            message.id,
            message.rid,
            message.tmid,
            message.u.id,
            message.u.username,
            message.u.name,
            message.msg.as_deref().unwrap_or(""),
            message.ts,
            message.updated_at,
            message.edited_at,
            message.t,
            message.tcount.unwrap_or(0),
            message.tlm,
            mentions(message, user_id, username),
            files,
            reactions,
            quotes,
            mentions_everyone(message),
        ],
    )?;
    Ok(())
}

pub fn delete_message(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM chat_messages WHERE id = ?1", [id])?;
    Ok(())
}

pub fn has_message(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM chat_messages WHERE id = ?1",
        [id],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

/// Take a thread out of Waiting until a message newer than `at` arrives.
pub fn set_mark(
    conn: &Connection,
    rid: &str,
    thread: &str,
    kind: &str,
    at: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO chat_marks (rid, thread, kind, at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(rid, thread) DO UPDATE SET kind = excluded.kind, at = excluded.at",
        params![rid, thread, kind, at],
    )?;
    Ok(())
}

pub fn remove_mark(conn: &Connection, rid: &str, thread: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM chat_marks WHERE rid = ?1 AND thread = ?2",
        params![rid, thread],
    )?;
    Ok(())
}

pub fn mark(conn: &Connection, rid: &str, thread: &str) -> rusqlite::Result<Option<(String, i64)>> {
    conn.query_row(
        "SELECT kind, at FROM chat_marks WHERE rid = ?1 AND thread = ?2",
        params![rid, thread],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

/// Remember that the user has seen a thread up to `at`.
pub fn set_seen(conn: &Connection, rid: &str, thread: &str, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO chat_seen (rid, thread, at) VALUES (?1, ?2, ?3)
         ON CONFLICT(rid, thread) DO UPDATE SET at = MAX(chat_seen.at, excluded.at)",
        params![rid, thread, at],
    )?;
    Ok(())
}

/// Make the thread unread from its newest message on: seen up to just before it.
pub fn unsee(conn: &Connection, rid: &str, thread: &str) -> rusqlite::Result<()> {
    if let Some(at) = newest_at(conn, rid, thread)? {
        conn.execute(
            "INSERT INTO chat_seen (rid, thread, at) VALUES (?1, ?2, ?3)
             ON CONFLICT(rid, thread) DO UPDATE SET at = excluded.at",
            params![rid, thread, at - 1],
        )?;
    }
    Ok(())
}

fn seen(conn: &Connection, rid: &str, thread: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT at FROM chat_seen WHERE rid = ?1 AND thread = ?2",
        params![rid, thread],
        |row| row.get(0),
    )
    .optional()
}

/// A room as the app shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRoom {
    pub rid: String,
    /// `c` a channel, `p` a private group, `d` a direct message, `l` a
    /// livechat.
    pub kind: String,
    pub name: Option<String>,
    pub fname: Option<String>,
    /// What a row says the room is: `#name`, `Direct`, or the people of a
    /// group message.
    pub label: String,
    pub unread: i64,
    pub mentions: i64,
    /// The other person's status, for a direct message with one person.
    #[serde(default)]
    pub presence: Option<String>,
    /// Whether the room is muted now (chat-spec §13).
    #[serde(default)]
    pub muted: bool,
    /// When the mute ends, in ms; `None` for one the user ends.
    #[serde(default)]
    pub muted_until: Option<i64>,
}

fn room_label(kind: &str, name: Option<&str>, fname: Option<&str>, users: Option<i64>) -> String {
    match kind {
        "c" | "p" => format!("#{}", name.or(fname).unwrap_or("room")),
        "d" if users.unwrap_or(2) > 2 => fname.or(name).unwrap_or("Group").to_string(),
        "d" => "Direct".to_string(),
        _ => fname.or(name).unwrap_or("room").to_string(),
    }
}

fn room_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatRoom> {
    let kind: String = row.get(1)?;
    let name: Option<String> = row.get(2)?;
    let fname: Option<String> = row.get(3)?;
    let users: Option<i64> = row.get(4)?;
    let mute_ends: Option<i64> = row.get(7)?;
    let muted = mute_ends.is_some_and(|ends| ends > now_ms());
    Ok(ChatRoom {
        rid: row.get(0)?,
        label: room_label(&kind, name.as_deref(), fname.as_deref(), users),
        kind,
        name,
        fname,
        unread: row.get(5)?,
        mentions: row.get(6)?,
        presence: None,
        muted,
        muted_until: mute_ends.filter(|&ends| muted && ends != MUTE_FOREVER),
    })
}

const ROOM_COLUMNS: &str = "rid, kind, name, fname, users_count, unread, user_mentions,
    (SELECT at FROM chat_marks k
     WHERE k.rid = chat_rooms.rid AND k.thread = '*' AND k.kind = 'mute')";

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// When the room's mute ends, as its mark has it, whether or not that has
/// passed; [`MUTE_FOREVER`] for one the user ends.
pub fn mute(conn: &Connection, rid: &str) -> rusqlite::Result<Option<i64>> {
    Ok(mark(conn, rid, MUTE_THREAD)?
        .filter(|(kind, _)| kind == MARK_MUTE)
        .map(|(_, ends)| ends))
}

pub fn room(conn: &Connection, rid: &str) -> rusqlite::Result<Option<ChatRoom>> {
    conn.query_row(
        &format!("SELECT {ROOM_COLUMNS} FROM chat_rooms WHERE rid = ?1"),
        [rid],
        room_from_row,
    )
    .optional()
}

/// The subscribed rooms, those with the newest messages first.
pub fn rooms(conn: &Connection) -> rusqlite::Result<Vec<ChatRoom>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ROOM_COLUMNS} FROM chat_rooms WHERE subscribed = 1
         ORDER BY COALESCE(last_message_at, 0) DESC"
    ))?;
    let rows = stmt.query_map([], room_from_row)?;
    rows.collect()
}

/// Something the room picker offers: a room the user is in, a public room
/// they can join, or a person to write to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatPick {
    /// `room`, `join` or `person`.
    pub kind: String,
    /// The room's id; for a person, the direct message room if there is one.
    pub rid: Option<String>,
    /// For a person, the username to write to.
    pub username: Option<String>,
    pub label: String,
    pub detail: String,
}

/// A thread that waits for an answer, as a row of Waiting shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitingThread {
    pub rid: String,
    /// The id of the thread's first message; empty for the room's timeline.
    pub thread: String,
    pub room: ChatRoom,
    /// The thread's first message; `None` for the room's timeline, or when the
    /// first message is older than what has been synced.
    pub title: Option<String>,
    pub replies: i64,
    /// Who wrote the newest message, and what.
    pub author: String,
    pub preview: String,
    pub newest_at: i64,
    /// The first message since the user last wrote in the thread.
    pub waiting_since: i64,
    /// Whether a message since then names the user.
    pub mention: bool,
    /// The usernames of those whose messages since then name the user.
    #[serde(default)]
    pub mentioned_by: Vec<String>,
    /// Whether the room is muted and no message since then names the user
    /// or calls the room: the thread waits, but only its room shows it.
    #[serde(default)]
    pub muted: bool,
    /// The first message since then that names the user or calls the room:
    /// when a muted room's thread is announced.
    #[serde(default)]
    pub called_since: Option<i64>,
    /// Whether the newest message is newer than what the user has seen.
    pub unread: bool,
    /// Whether the newest message carries files.
    pub files: bool,
    /// Whether a task made from one of the thread's messages is still open.
    pub task: bool,
    /// Whether the thread waits for the user (always, in Waiting).
    pub waits: bool,
    /// Whether the newest message is the user's.
    pub mine_last: bool,
}

/// One message, as the conversation shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub rid: String,
    pub tmid: Option<String>,
    pub author_id: String,
    pub author_username: Option<String>,
    pub author_name: Option<String>,
    pub mine: bool,
    pub body: String,
    pub ts: i64,
    pub edited: bool,
    /// The kind of a system message; `None` for what someone wrote.
    pub system: Option<String>,
    /// Replies to the message, when it starts a thread.
    pub replies: i64,
    pub mentions_me: bool,
    pub files: Vec<ChatFile>,
    pub reactions: Vec<ChatReaction>,
    pub quotes: Vec<ChatQuote>,
    /// The tasks made from the message, but those in the trash.
    pub tasks: Vec<ChatTaskChip>,
}

/// A task made from a chat message, as the message shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatTaskChip {
    pub id: i64,
    pub title: String,
    pub done: bool,
}

/// A thread with its messages, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub rid: String,
    pub thread: String,
    pub room: ChatRoom,
    pub title: Option<String>,
    pub messages: Vec<ChatMessage>,
    /// Whether there are messages older than the first shown, in the
    /// database or on the server: what scrolling to the top brings in.
    pub has_older: bool,
    /// Messages newer than this are new to the user: the later of the last
    /// time they saw the thread and their own last message in it.
    pub new_after: Option<i64>,
    /// How the thread stands in Waiting.
    pub waiting: Option<WaitingThread>,
    pub mark: Option<String>,
}

/// The thread a message belongs to: its thread if it is a reply, itself if it
/// starts one, otherwise the room's timeline.
fn thread_of(id: &str, tmid: Option<&str>, tcount: i64) -> String {
    match tmid {
        Some(tmid) => tmid.to_string(),
        None if tcount > 0 => id.to_string(),
        None => String::new(),
    }
}

/// The first line of a message, whitespace collapsed, for a row.
pub fn preview(body: &str) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(200).collect();
    if flat.chars().count() > 200 {
        out.push('…');
    }
    out
}

fn display_name(username: Option<String>, name: Option<String>) -> String {
    name.filter(|n| !n.is_empty())
        .or(username)
        .unwrap_or_else(|| "someone".to_string())
}

/// The threads that wait for the user, newest first.
///
/// A thread waits when its room may (`waiting_room`), its newest message is
/// not the user's, and that message is newer than the thread's mark. Messages
/// older than `baseline` are not looked at, and neither are system messages.
pub fn waiting(
    conn: &Connection,
    user_id: &str,
    baseline: i64,
    waiting_room: &dyn Fn(&ChatRoom) -> bool,
) -> rusqlite::Result<Vec<WaitingThread>> {
    let mut rows = thread_rows(conn, user_id, None, baseline, baseline, waiting_room)?;
    rows.retain(|row| row.waits);
    Ok(rows)
}

/// Every thread of one room, the room's own timeline first and the rest by
/// their newest message, each saying whether it waits.
pub fn room_threads(
    conn: &Connection,
    user_id: &str,
    rid: &str,
    baseline: i64,
    waiting_room: &dyn Fn(&ChatRoom) -> bool,
) -> rusqlite::Result<Vec<WaitingThread>> {
    let mut rows = thread_rows(conn, user_id, Some(rid), 0, baseline, waiting_room)?;
    // The room's timeline leads, as the row `c` writes to.
    rows.sort_by_key(|row| (!row.thread.is_empty(), std::cmp::Reverse(row.newest_at)));
    Ok(rows)
}

/// One row per thread, of every subscribed room or of `only` one, from the
/// messages since `since`; `waits` as [`waiting`] decides it, counting only
/// messages since `baseline`. Newest first.
fn thread_rows(
    conn: &Connection,
    user_id: &str,
    only: Option<&str>,
    since: i64,
    baseline: i64,
    waiting_room: &dyn Fn(&ChatRoom) -> bool,
) -> rusqlite::Result<Vec<WaitingThread>> {
    struct Acc {
        newest_id: String,
        newest_mine: bool,
        newest_at: i64,
        since: Option<i64>,
        mention: bool,
        mentioned_by: Vec<String>,
        called: Option<i64>,
        newest_files: bool,
    }
    let mut stmt = conn.prepare(
        "SELECT m.id, m.rid, m.tmid, m.tcount, m.author_id, m.ts, m.mentions_me, m.files,
            m.mentions_all, m.author_username
         FROM chat_messages m JOIN chat_rooms r ON r.rid = m.rid AND r.subscribed = 1
         WHERE m.system_type IS NULL AND m.ts >= ?1 AND (?2 IS NULL OR m.rid = ?2)
         ORDER BY m.ts",
    )?;
    let marks: HashMap<(String, String), i64> = conn
        .prepare("SELECT rid, thread, at FROM chat_marks WHERE thread <> ?1")?
        .query_map([MUTE_THREAD], |row| {
            Ok(((row.get(0)?, row.get(1)?), row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut threads: HashMap<(String, String), Acc> = HashMap::new();
    let mut rows = stmt.query(params![since, only])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let rid: String = row.get(1)?;
        let tmid: Option<String> = row.get(2)?;
        let thread = thread_of(&id, tmid.as_deref(), row.get(3)?);
        let mine = row.get::<_, String>(4)? == user_id;
        let ts: i64 = row.get(5)?;
        let mentions_me: bool = row.get(6)?;
        let files: Option<String> = row.get(7)?;
        let mentions_all: bool = row.get(8)?;
        let author: Option<String> = row.get(9)?;
        let key = (rid, thread);
        // A mark answers the thread as the user's own message would.
        let answered = mine || marks.get(&key).is_some_and(|&at| ts <= at);
        let acc = threads.entry(key).or_insert(Acc {
            newest_id: String::new(),
            newest_mine: false,
            newest_at: 0,
            since: None,
            mention: false,
            mentioned_by: Vec::new(),
            called: None,
            newest_files: false,
        });
        acc.newest_id = id;
        acc.newest_mine = mine;
        acc.newest_at = ts;
        acc.newest_files = files.is_some();
        if answered {
            acc.since = None;
            acc.mention = false;
            acc.mentioned_by.clear();
            acc.called = None;
        } else {
            acc.since.get_or_insert(ts);
            acc.mention |= mentions_me;
            if let Some(author) = author.filter(|a| mentions_me && !acc.mentioned_by.contains(a)) {
                acc.mentioned_by.push(author);
            }
            if mentions_me || mentions_all {
                acc.called.get_or_insert(ts);
            }
        }
    }
    drop(rows);

    let mut rooms: HashMap<String, Option<ChatRoom>> = HashMap::new();
    let open_tasks = threads_with_open_tasks(conn)?;
    let mut out = Vec::new();
    for ((rid, thread), acc) in threads {
        let answered = marks
            .get(&(rid.clone(), thread.clone()))
            .is_some_and(|&at| at >= acc.newest_at);
        let room = match rooms.get(&rid) {
            Some(room) => room.clone(),
            None => {
                let room = room(conn, &rid)?;
                rooms.insert(rid.clone(), room.clone());
                room
            }
        };
        let Some(room) = room else { continue };
        let waits =
            !acc.newest_mine && !answered && acc.newest_at >= baseline && waiting_room(&room);
        // Waiting looks at nothing else; a room's list shows every thread.
        if only.is_none() && !waits {
            continue;
        }
        let (author, body): (String, String) = conn.query_row(
            "SELECT author_username, author_name, body FROM chat_messages WHERE id = ?1",
            [&acc.newest_id],
            |row| Ok((display_name(row.get(0)?, row.get(1)?), row.get(2)?)),
        )?;
        let (title, replies) = if thread.is_empty() {
            (None, 0)
        } else {
            conn.query_row(
                "SELECT body, tcount FROM chat_messages WHERE id = ?1",
                [&thread],
                |row| Ok((Some(preview(&row.get::<_, String>(0)?)), row.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, 0))
        };
        let seen_at = seen(conn, &rid, &thread)?;
        out.push(WaitingThread {
            title,
            replies,
            author,
            preview: preview(&body),
            newest_at: acc.newest_at,
            waiting_since: acc.since.unwrap_or(acc.newest_at),
            mention: acc.mention,
            mentioned_by: acc.mentioned_by,
            muted: room.muted && acc.called.is_none(),
            called_since: acc.called,
            unread: waits && seen_at.is_none_or(|at| at < acc.newest_at),
            files: acc.newest_files,
            task: open_tasks.contains(&(rid.clone(), thread.clone())),
            waits,
            mine_last: acc.newest_mine,
            rid,
            thread,
            room,
        });
    }
    out.sort_by_key(|w| std::cmp::Reverse(w.newest_at));
    Ok(out)
}

fn message_from_row(
    row: &rusqlite::Row<'_>,
    user_id: &str,
    username: &str,
) -> rusqlite::Result<ChatMessage> {
    let files: Option<String> = row.get(12)?;
    let reactions: Option<String> = row.get(13)?;
    let quotes: Option<String> = row.get(14)?;
    let author_id: String = row.get(3)?;
    let reactions: HashMap<String, Vec<String>> = reactions
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default();
    let mut reactions: Vec<ChatReaction> = reactions
        .into_iter()
        .map(|(emoji, people)| ChatReaction {
            mine: people.iter().any(|p| p == username),
            count: people.len(),
            emoji,
        })
        .collect();
    reactions.sort_by(|a, b| a.emoji.cmp(&b.emoji));
    Ok(ChatMessage {
        id: row.get(0)?,
        rid: row.get(1)?,
        tmid: row.get(2)?,
        mine: author_id == user_id,
        author_id,
        author_username: row.get(4)?,
        author_name: row.get(5)?,
        body: row.get(6)?,
        ts: row.get(7)?,
        edited: row.get::<_, Option<i64>>(8)?.is_some(),
        system: row.get(9)?,
        replies: row.get(10)?,
        mentions_me: row.get(11)?,
        files: files_from_json(files),
        reactions,
        quotes: quotes
            .and_then(|q| serde_json::from_str(&q).ok())
            .unwrap_or_default(),
        tasks: Vec::new(),
    })
}

const MESSAGE_COLUMNS: &str = "id, rid, tmid, author_id, author_username, author_name, body, ts,
    edited_at, system_type, tcount, mentions_me, files, reactions, quotes";

/// How many messages a conversation opens with, the newest of them.
pub const FIRST_PAGE: i64 = 200;
/// How many older messages scrolling to the top brings in at a time.
pub const OLDER_PAGE: i64 = 100;

/// Up to `limit` messages of a thread older than `before` (all, without it),
/// oldest first: its first message and its replies, or the room's own
/// timeline.
pub fn messages(
    conn: &Connection,
    rid: &str,
    thread: &str,
    user_id: &str,
    username: &str,
    before: Option<i64>,
    limit: i64,
) -> rusqlite::Result<Vec<ChatMessage>> {
    let before = before.unwrap_or(i64::MAX);
    let mut out = if thread.is_empty() {
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM chat_messages
             WHERE rid = ?1 AND tmid IS NULL AND ts < ?2 ORDER BY ts DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![rid, before, limit], |row| {
            message_from_row(row, user_id, username)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM chat_messages
             WHERE (id = ?1 OR tmid = ?1) AND ts < ?2 ORDER BY ts DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![thread, before, limit], |row| {
            message_from_row(row, user_id, username)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    out.reverse();
    let mut tasks = linked_tasks(conn, rid)?;
    for message in &mut out {
        message.tasks = tasks.remove(&message.id).unwrap_or_default();
    }
    Ok(out)
}

// ----- Tasks made from messages (todo-spec §28) -----

/// Whether the tasks' tables are in this database: the chat store's own tests
/// run without them.
fn has_tasks(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'task_linked_emails')",
        [],
        |row| row.get(0),
    )
}

/// The links of tasks not in the trash to the room's messages, with their tasks.
fn room_task_links(
    conn: &Connection,
    rid: &str,
) -> rusqlite::Result<Vec<(super::ChatLink, ChatTaskChip)>> {
    if !has_tasks(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT e.message_id, t.id, t.subject, t.completed_at IS NOT NULL
         FROM task_linked_emails e JOIN tasks t ON t.id = e.task_id
         WHERE t.deleted_at IS NULL AND e.message_id LIKE 'chat://%/' || ?1 || '/%'
         ORDER BY t.position",
    )?;
    let rows = stmt.query_map([rid], |row| {
        Ok((
            row.get::<_, String>(0)?,
            ChatTaskChip {
                id: row.get(1)?,
                title: row.get(2)?,
                done: row.get(3)?,
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, chip) = row?;
        if let Some(link) = super::ChatLink::parse(&id).filter(|l| l.rid == rid) {
            out.push((link, chip));
        }
    }
    Ok(out)
}

/// The tasks made from the room's messages, by message.
fn linked_tasks(
    conn: &Connection,
    rid: &str,
) -> rusqlite::Result<HashMap<String, Vec<ChatTaskChip>>> {
    let mut out: HashMap<String, Vec<ChatTaskChip>> = HashMap::new();
    for (link, chip) in room_task_links(conn, rid)? {
        out.entry(link.message).or_default().push(chip);
    }
    Ok(out)
}

/// The threads, by room and thread, with a task made from them still open.
fn threads_with_open_tasks(conn: &Connection) -> rusqlite::Result<HashSet<(String, String)>> {
    if !has_tasks(conn)? {
        return Ok(HashSet::new());
    }
    let mut stmt = conn.prepare(
        "SELECT e.message_id FROM task_linked_emails e JOIN tasks t ON t.id = e.task_id
         WHERE t.deleted_at IS NULL AND t.completed_at IS NULL AND e.message_id LIKE 'chat://%'",
    )?;
    let ids = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = HashSet::new();
    for id in ids {
        if let Some(link) = super::ChatLink::parse(&id?) {
            out.insert((link.rid, link.thread));
        }
    }
    Ok(out)
}

/// When the user last wrote in the thread — in the room's own timeline for
/// `thread` empty — after `after` (ms), if they did.
pub fn my_reply_after(
    conn: &Connection,
    rid: &str,
    thread: &str,
    after: i64,
) -> rusqlite::Result<Option<i64>> {
    let Some(me) = state(conn, STATE_USER_ID)? else {
        return Ok(None);
    };
    conn.query_row(
        "SELECT MAX(ts) FROM chat_messages
         WHERE rid = ?1 AND author_id = ?2 AND ts > ?3 AND system_type IS NULL
           AND (CASE WHEN ?4 = '' THEN tmid IS NULL ELSE tmid = ?4 END)",
        params![rid, me, after, thread],
        |row| row.get(0),
    )
}

/// One message, for a task made from it.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSource {
    pub id: String,
    pub author: String,
    pub body: String,
    pub ts: i64,
}

/// The message a task is made from: `message`, or the thread's newest.
pub fn task_source(
    conn: &Connection,
    rid: &str,
    thread: &str,
    message: Option<&str>,
) -> rusqlite::Result<Option<TaskSource>> {
    let id = match message {
        Some(id) => Some(id.to_string()),
        None => newest_id(conn, rid, thread)?,
    };
    let Some(id) = id else { return Ok(None) };
    conn.query_row(
        "SELECT id, author_username, author_name, body, ts FROM chat_messages
         WHERE id = ?1 AND rid = ?2",
        params![id, rid],
        |row| {
            Ok(TaskSource {
                id: row.get(0)?,
                author: display_name(row.get(1)?, row.get(2)?),
                body: row.get(3)?,
                ts: row.get(4)?,
            })
        },
    )
    .optional()
}

/// Whether the database holds messages of the thread older than `before`.
pub fn has_older(
    conn: &Connection,
    rid: &str,
    thread: &str,
    before: i64,
) -> rusqlite::Result<bool> {
    let sql = if thread.is_empty() {
        "SELECT EXISTS(SELECT 1 FROM chat_messages WHERE rid = ?1 AND tmid IS NULL AND ts < ?2)"
    } else {
        "SELECT EXISTS(SELECT 1 FROM chat_messages WHERE (id = ?1 OR tmid = ?1) AND ts < ?2)"
    };
    let key = if thread.is_empty() { rid } else { thread };
    conn.query_row(sql, params![key, before], |row| row.get(0))
}

/// Whether the server has no messages of the room's timeline older than the
/// oldest in the database: once asked for older ones and short of a page.
pub fn history_complete(conn: &Connection, rid: &str) -> rusqlite::Result<bool> {
    Ok(state(conn, &format!("history_complete:{rid}"))?.is_some())
}

pub fn set_history_complete(conn: &Connection, rid: &str) -> rusqlite::Result<()> {
    set_state(conn, &format!("history_complete:{rid}"), "1")
}

/// A page of older messages, oldest first, and whether there are more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OlderMessages {
    pub messages: Vec<ChatMessage>,
    pub has_older: bool,
}

/// Whether the conversation can reach further back than `oldest`: in the
/// database, or, for a room's timeline, on the server. A thread is fetched
/// whole when it is opened, so its start is in the database.
pub fn reaches_back(
    conn: &Connection,
    rid: &str,
    thread: &str,
    oldest: Option<i64>,
) -> rusqlite::Result<bool> {
    let Some(oldest) = oldest else {
        return Ok(thread.is_empty() && !history_complete(conn, rid)?);
    };
    Ok(has_older(conn, rid, thread, oldest)?
        || (thread.is_empty() && !history_complete(conn, rid)?))
}

/// A thread for the reader.
pub fn conversation(
    conn: &Connection,
    rid: &str,
    thread: &str,
    user_id: &str,
    username: &str,
    waiting_now: Option<WaitingThread>,
) -> rusqlite::Result<Option<Conversation>> {
    let Some(room) = room(conn, rid)? else {
        return Ok(None);
    };
    let messages = messages(conn, rid, thread, user_id, username, None, FIRST_PAGE)?;
    // From the database rather than the page: in a long thread the first
    // message is not among the newest.
    let title = if thread.is_empty() {
        None
    } else {
        conn.query_row(
            "SELECT body FROM chat_messages WHERE id = ?1",
            [thread],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|body| preview(&body))
    };
    let has_older = reaches_back(conn, rid, thread, messages.first().map(|m| m.ts))?;
    let my_last = messages.iter().filter(|m| m.mine).map(|m| m.ts).max();
    let new_after = match (seen(conn, rid, thread)?, my_last) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    Ok(Some(Conversation {
        rid: rid.to_string(),
        thread: thread.to_string(),
        room,
        title,
        messages,
        has_older,
        new_after,
        mark: mark(conn, rid, thread)?.map(|(kind, _)| kind),
        waiting: waiting_now,
    }))
}

/// The id of the newest message of a thread that is not a system message.
pub fn newest_id(conn: &Connection, rid: &str, thread: &str) -> rusqlite::Result<Option<String>> {
    let sql = if thread.is_empty() {
        "SELECT id FROM chat_messages
         WHERE rid = ?1 AND tmid IS NULL AND tcount = 0 AND system_type IS NULL AND ?2 = ''
         ORDER BY ts DESC LIMIT 1"
    } else {
        "SELECT id FROM chat_messages
         WHERE rid = ?1 AND (id = ?2 OR tmid = ?2) AND system_type IS NULL
         ORDER BY ts DESC LIMIT 1"
    };
    conn.query_row(sql, params![rid, thread], |row| row.get(0))
        .optional()
}

/// A file of a message: its name and its path on the server.
pub fn message_file(
    conn: &Connection,
    id: &str,
    index: usize,
) -> rusqlite::Result<Option<ChatFile>> {
    let files: Option<Option<String>> = conn
        .query_row(
            "SELECT files FROM chat_messages WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let mut files = files_from_json(files.flatten());
    Ok((index < files.len()).then(|| files.swap_remove(index)))
}

/// The newest message of a thread that is not a system message.
pub fn newest_at(conn: &Connection, rid: &str, thread: &str) -> rusqlite::Result<Option<i64>> {
    let sql = if thread.is_empty() {
        "SELECT MAX(ts) FROM chat_messages
         WHERE rid = ?1 AND tmid IS NULL AND tcount = 0 AND system_type IS NULL AND ?2 = ''"
    } else {
        "SELECT MAX(ts) FROM chat_messages
         WHERE rid = ?1 AND (id = ?2 OR tmid = ?2) AND system_type IS NULL"
    };
    conn.query_row(sql, params![rid, thread], |row| row.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::client::WireUser;

    const ME: &str = "me-id";

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::CHAT_SCHEMA).unwrap();
        conn.execute_batch(crate::db::CHAT_QUOTES_SCHEMA).unwrap();
        conn.execute_batch(crate::db::CHAT_MENTIONS_ALL_SCHEMA)
            .unwrap();
        conn
    }

    fn room(conn: &Connection, rid: &str, kind: &str, name: &str) {
        upsert_subscription(
            conn,
            &WireSubscription {
                rid: rid.into(),
                t: Some(kind.into()),
                name: Some(name.into()),
                fname: Some(name.into()),
                open: Some(true),
                unread: Some(0),
                user_mentions: Some(0),
                disable_notifications: None,
                desktop_notifications: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn a_room_notifies_as_its_subscription_says() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        assert_eq!(room_notifies(&conn, "r").unwrap(), NOTIFY_ALL);
        let sub = |disable: Option<bool>, desktop: Option<&str>| WireSubscription {
            rid: "r".into(),
            t: Some("c".into()),
            name: Some("dev".into()),
            fname: None,
            open: Some(true),
            unread: Some(0),
            user_mentions: Some(0),
            disable_notifications: disable,
            desktop_notifications: desktop.map(Into::into),
        };
        upsert_subscription(&conn, &sub(None, Some("mentions"))).unwrap();
        assert_eq!(room_notifies(&conn, "r").unwrap(), NOTIFY_MENTIONS);
        upsert_subscription(&conn, &sub(Some(true), Some("all"))).unwrap();
        assert_eq!(room_notifies(&conn, "r").unwrap(), NOTIFY_NOTHING);
        upsert_subscription(&conn, &sub(None, Some("default"))).unwrap();
        assert_eq!(room_notifies(&conn, "r").unwrap(), NOTIFY_ALL);
    }

    struct Msg<'a> {
        id: &'a str,
        rid: &'a str,
        tmid: Option<&'a str>,
        author: &'a str,
        ts: i64,
        tcount: i64,
        mentions_me: bool,
    }

    fn msg(conn: &Connection, m: Msg<'_>) {
        let wire = WireMessage {
            id: m.id.into(),
            rid: m.rid.into(),
            tmid: m.tmid.map(Into::into),
            msg: Some(format!("text of {}", m.id)),
            ts: m.ts,
            u: WireUser {
                id: m.author.into(),
                username: Some(m.author.into()),
                name: None,
            },
            updated_at: m.ts,
            edited_at: None,
            t: None,
            tcount: Some(m.tcount),
            tlm: None,
            mentions: if m.mentions_me {
                vec![crate::chat::client::WireMention {
                    id: Some(ME.into()),
                    username: Some("me".into()),
                }]
            } else {
                vec![]
            },
            files: vec![],
            attachments: vec![],
            reactions: None,
        };
        upsert_message(conn, &wire, ME, "me").unwrap();
    }

    fn plain<'a>(id: &'a str, rid: &'a str, author: &'a str, ts: i64) -> Msg<'a> {
        Msg {
            id,
            rid,
            tmid: None,
            author,
            ts,
            tcount: 0,
            mentions_me: false,
        }
    }

    fn keys(conn: &Connection) -> Vec<(String, String)> {
        waiting(conn, ME, 0, &|_| true)
            .unwrap()
            .into_iter()
            .map(|w| (w.rid, w.thread))
            .collect()
    }

    #[test]
    fn a_thread_waits_until_the_newest_message_is_mine() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(
            &conn,
            Msg {
                tcount: 2,
                ..plain("root", "r", "lukas", 10)
            },
        );
        msg(
            &conn,
            Msg {
                tmid: Some("root"),
                ..plain("a", "r", "sara", 20)
            },
        );
        assert_eq!(keys(&conn), vec![("r".to_string(), "root".to_string())]);

        msg(
            &conn,
            Msg {
                tmid: Some("root"),
                ..plain("b", "r", ME, 30)
            },
        );
        assert!(keys(&conn).is_empty(), "my reply answers it");

        msg(
            &conn,
            Msg {
                tmid: Some("root"),
                ..plain("c", "r", "sara", 40)
            },
        );
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].waiting_since, 40,
            "waiting since the first message after mine"
        );
        assert_eq!(rows[0].title.as_deref(), Some("text of root"));
        assert_eq!(rows[0].replies, 2);
    }

    #[test]
    fn the_rooms_timeline_is_a_thread_of_its_own() {
        let conn = db();
        room(&conn, "d", "d", "lukas");
        msg(&conn, plain("1", "d", "lukas", 10));
        msg(
            &conn,
            Msg {
                tcount: 1,
                ..plain("2", "d", "lukas", 20)
            },
        );
        msg(
            &conn,
            Msg {
                tmid: Some("2"),
                ..plain("3", "d", ME, 30)
            },
        );
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert_eq!(rows.len(), 1, "the thread is answered, the timeline is not");
        assert_eq!(rows[0].thread, "");
        assert_eq!(rows[0].room.label, "Direct");
        assert_eq!(rows[0].preview, "text of 1");
    }

    #[test]
    fn a_mark_holds_until_someone_writes_again() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(&conn, plain("1", "r", "lukas", 10));
        set_mark(&conn, "r", "", MARK_TASK, 10).unwrap();
        assert!(keys(&conn).is_empty());

        msg(&conn, plain("2", "r", "lukas", 20));
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert_eq!(rows.len(), 1);
    }

    fn called(conn: &Connection, id: &str, rid: &str, ts: i64, name: &str) {
        let mut wire = WireMessage {
            id: id.into(),
            rid: rid.into(),
            tmid: None,
            msg: Some(format!("@{name}")),
            ts,
            u: WireUser {
                id: "lukas".into(),
                username: Some("lukas".into()),
                name: None,
            },
            updated_at: ts,
            edited_at: None,
            t: None,
            tcount: Some(0),
            tlm: None,
            mentions: vec![],
            files: vec![],
            attachments: vec![],
            reactions: None,
        };
        wire.mentions.push(crate::chat::client::WireMention {
            id: Some(name.into()),
            username: Some(name.into()),
        });
        upsert_message(conn, &wire, ME, "me").unwrap();
    }

    #[test]
    fn a_muted_rooms_threads_wait_muted_until_the_room_is_called() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(&conn, plain("1", "r", "lukas", 10));
        set_mark(&conn, "r", MUTE_THREAD, MARK_MUTE, MUTE_FOREVER).unwrap();
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert_eq!(rows.len(), 1, "a muted thread still waits");
        assert!(rows[0].muted);
        assert!(rows[0].room.muted);
        assert_eq!(rows[0].room.muted_until, None);

        called(&conn, "2", "r", 20, "here");
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert!(!rows[0].muted, "@here comes through");
        assert!(!rows[0].mention, "but does not name the user");
        assert_eq!(rows[0].waiting_since, 10);
        assert_eq!(
            rows[0].called_since,
            Some(20),
            "announced from when it was called"
        );

        msg(&conn, plain("3", "r", ME, 30));
        msg(&conn, plain("4", "r", "lukas", 40));
        assert!(
            waiting(&conn, ME, 0, &|_| true).unwrap()[0].muted,
            "answered, muted again"
        );
        msg(
            &conn,
            Msg {
                mentions_me: true,
                ..plain("5", "r", "lukas", 50)
            },
        );
        assert!(!waiting(&conn, ME, 0, &|_| true).unwrap()[0].muted);
    }

    #[test]
    fn a_thread_marked_done_comes_back_with_only_what_came_after() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        called(&conn, "1", "r", 10, "all");
        msg(
            &conn,
            Msg {
                mentions_me: true,
                ..plain("2", "r", "lukas", 20)
            },
        );
        set_mark(&conn, "r", "", MARK_DONE, 20).unwrap();
        set_mark(&conn, "r", MUTE_THREAD, MARK_MUTE, MUTE_FOREVER).unwrap();
        msg(&conn, plain("3", "r", "lukas", 30));
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert_eq!(rows.len(), 1, "somebody wrote again");
        assert!(rows[0].muted, "the @all was dealt with by the mark");
        assert_eq!(rows[0].called_since, None);
        assert!(!rows[0].mention);
        assert_eq!(rows[0].waiting_since, 30);
    }

    #[test]
    fn a_mute_that_has_ended_holds_nothing() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(&conn, plain("1", "r", "lukas", 10));
        set_mark(&conn, "r", MUTE_THREAD, MARK_MUTE, now_ms() - 1).unwrap();
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert!(!rows[0].muted);
        assert!(!rows[0].room.muted);
        assert_eq!(rows[0].room.muted_until, None);

        let ends = now_ms() + 3_600_000;
        set_mark(&conn, "r", MUTE_THREAD, MARK_MUTE, ends).unwrap();
        assert_eq!(
            super::room(&conn, "r").unwrap().unwrap().muted_until,
            Some(ends)
        );
        assert_eq!(mute(&conn, "r").unwrap(), Some(ends));
    }

    #[test]
    fn rooms_that_may_not_wait_and_old_messages_are_left_out() {
        let conn = db();
        room(&conn, "g", "c", "general");
        room(&conn, "r", "c", "dev");
        msg(&conn, plain("1", "g", "lukas", 10));
        msg(&conn, plain("2", "r", "lukas", 5));
        let rows = waiting(&conn, ME, 8, &|room| {
            room.name.as_deref() != Some("general")
        })
        .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn a_mention_since_my_last_message_is_flagged() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(
            &conn,
            Msg {
                mentions_me: true,
                ..plain("1", "r", "sara", 10)
            },
        );
        msg(&conn, plain("2", "r", ME, 20));
        msg(&conn, plain("3", "r", "sara", 30));
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert!(!rows[0].mention, "the mention was answered");
        msg(
            &conn,
            Msg {
                mentions_me: true,
                ..plain("4", "r", "sara", 40)
            },
        );
        assert!(waiting(&conn, ME, 0, &|_| true).unwrap()[0].mention);
    }

    /// Who named the user since the thread started waiting, each once, and
    /// nobody whose mention the user has answered.
    #[test]
    fn those_who_named_me_since_my_last_message_are_listed() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        let naming = |id, author, ts| Msg {
            mentions_me: true,
            ..plain(id, "r", author, ts)
        };
        msg(&conn, naming("1", "lukas", 10));
        msg(&conn, plain("2", "r", ME, 20));
        msg(&conn, plain("3", "r", "lukas", 30));
        msg(&conn, naming("4", "sara", 40));
        msg(&conn, naming("5", "sara", 50));
        msg(&conn, naming("6", "anna", 60));
        let rows = waiting(&conn, ME, 0, &|_| true).unwrap();
        assert_eq!(rows[0].mentioned_by, ["sara", "anna"]);
    }

    #[test]
    fn seeing_a_thread_makes_it_read_until_the_next_message() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(&conn, plain("1", "r", "sara", 10));
        assert!(waiting(&conn, ME, 0, &|_| true).unwrap()[0].unread);
        set_seen(&conn, "r", "", 10).unwrap();
        assert!(!waiting(&conn, ME, 0, &|_| true).unwrap()[0].unread);
        msg(&conn, plain("2", "r", "sara", 20));
        assert!(waiting(&conn, ME, 0, &|_| true).unwrap()[0].unread);
    }

    /// A long room opens on its newest messages, and the rest comes a page at
    /// a time, oldest first within each page, until the start is reached.
    #[test]
    fn a_long_timeline_opens_on_its_newest_page_and_pages_back() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        let total = FIRST_PAGE + OLDER_PAGE + 10;
        for i in 0..total {
            msg(&conn, plain(&format!("m{i}"), "r", "lukas", 1000 + i));
        }
        let c = conversation(&conn, "r", "", ME, "me", None)
            .unwrap()
            .unwrap();
        assert_eq!(c.messages.len() as i64, FIRST_PAGE);
        assert_eq!(c.messages.last().unwrap().id, format!("m{}", total - 1));
        assert!(c.has_older, "older messages are in the database");

        let first = c.messages[0].ts;
        let page = messages(&conn, "r", "", ME, "me", Some(first), OLDER_PAGE).unwrap();
        assert_eq!(page.len() as i64, OLDER_PAGE);
        assert!(page.windows(2).all(|w| w[0].ts < w[1].ts), "oldest first");
        assert_eq!(page.last().unwrap().ts, first - 1);

        let rest = messages(&conn, "r", "", ME, "me", Some(page[0].ts), OLDER_PAGE).unwrap();
        assert_eq!(rest.len(), 10);
        // The database has run out; until the server says so, it may have more.
        assert!(reaches_back(&conn, "r", "", Some(rest[0].ts)).unwrap());
        set_history_complete(&conn, "r").unwrap();
        assert!(!reaches_back(&conn, "r", "", Some(rest[0].ts)).unwrap());
    }

    /// A room lists every thread, its own timeline first, and says which wait.
    #[test]
    fn a_room_lists_its_timeline_first_then_its_threads_by_newest() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(&conn, plain("1", "r", "lukas", 10));
        msg(
            &conn,
            Msg {
                tcount: 1,
                ..plain("a", "r", "sara", 20)
            },
        );
        msg(
            &conn,
            Msg {
                tmid: Some("a"),
                ..plain("a1", "r", ME, 30)
            },
        );
        msg(
            &conn,
            Msg {
                tcount: 1,
                ..plain("b", "r", "sara", 40)
            },
        );
        msg(
            &conn,
            Msg {
                tmid: Some("b"),
                ..plain("b1", "r", "lukas", 50)
            },
        );
        let rows = room_threads(&conn, ME, "r", 0, &|_| true).unwrap();
        let keys: Vec<_> = rows
            .iter()
            .map(|r| (r.thread.as_str(), r.waits, r.mine_last))
            .collect();
        assert_eq!(
            keys,
            [("", true, false), ("b", true, false), ("a", false, true)]
        );
        // Waiting is the same rows, those that wait.
        assert_eq!(waiting(&conn, ME, 0, &|_| true).unwrap().len(), 2);
    }

    #[test]
    fn a_conversation_is_its_thread_oldest_first() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(
            &conn,
            Msg {
                tcount: 2,
                ..plain("root", "r", "lukas", 10)
            },
        );
        msg(&conn, plain("other", "r", "lukas", 15));
        msg(
            &conn,
            Msg {
                tmid: Some("root"),
                ..plain("a", "r", ME, 20)
            },
        );
        msg(
            &conn,
            Msg {
                tmid: Some("root"),
                ..plain("b", "r", "sara", 30)
            },
        );
        let c = conversation(&conn, "r", "root", ME, "me", None)
            .unwrap()
            .unwrap();
        let ids: Vec<_> = c.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["root", "a", "b"]);
        assert_eq!(c.new_after, Some(20));
        assert_eq!(c.room.label, "#dev");
        assert!(c.messages[1].mine);

        let timeline = conversation(&conn, "r", "", ME, "me", None)
            .unwrap()
            .unwrap();
        let ids: Vec<_> = timeline.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["root", "other"],
            "the timeline shows a thread's first message"
        );
    }

    #[test]
    fn a_quote_keeps_its_author_text_and_link_and_files_their_address() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        let wire: WireMessage = serde_json::from_value(serde_json::json!({
            "_id": "q", "rid": "r", "msg": "[ ](https://chat/channel/dev?msg=m1) Agreed",
            "ts": 10, "_updatedAt": 10, "u": { "_id": "lukas", "username": "lukas" },
            "attachments": [
                { "message_link": "https://chat/channel/dev?msg=m1", "author_name": "sara", "text": "Ship it?" },
                { "title": "ci.log", "title_link": "/file-upload/f1/ci.log" }
            ]
        }))
        .unwrap();
        upsert_message(&conn, &wire, ME, "me").unwrap();
        let c = conversation(&conn, "r", "", ME, "me", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            c.messages[0].quotes,
            [ChatQuote {
                author: Some("sara".into()),
                text: "Ship it?".into(),
                link: Some("https://chat/channel/dev?msg=m1".into()),
            }]
        );
        let file = message_file(&conn, "q", 0).unwrap().unwrap();
        assert_eq!(file.path.as_deref(), Some("/file-upload/f1/ci.log"));
        assert!(message_file(&conn, "q", 1).unwrap().is_none());
    }

    /// An image arrives as Rocket.Chat sends it: the image and the thumbnail
    /// it made, and one attachment linking the first and previewing with the
    /// second. The thumbnail is the image's preview, not a second file.
    #[test]
    fn an_image_is_one_file_with_its_thumbnail_as_preview() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        let wire: WireMessage = serde_json::from_value(serde_json::json!({
            "_id": "i", "rid": "r", "msg": "", "ts": 10, "_updatedAt": 10,
            "u": { "_id": "lukas", "username": "lukas" },
            "files": [
                { "_id": "A", "name": "boxes.png", "type": "image/png", "size": 28081 },
                { "_id": "B", "name": "thumb-boxes.png", "type": "image/png", "size": 15376 }
            ],
            "attachments": [
                { "message_link": "https://chat/channel/dev?msg=m1", "text": "Look" },
                {
                    "title": "boxes.png", "title_link": "/file-upload/A/boxes.png",
                    "image_url": "/file-upload/B/thumb-boxes.png",
                    "image_dimensions": { "width": 572, "height": 750 }
                }
            ]
        }))
        .unwrap();
        upsert_message(&conn, &wire, ME, "me").unwrap();
        let c = conversation(&conn, "r", "", ME, "me", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            c.messages[0].files,
            [ChatFile {
                name: "boxes.png".into(),
                mime: Some("image/png".into()),
                size: Some(28081),
                path: Some("/file-upload/A/boxes.png".into()),
                preview: Some("/file-upload/B/thumb-boxes.png".into()),
                width: Some(572),
                height: Some(750),
            }]
        );
        assert!(message_file(&conn, "i", 1).unwrap().is_none());
    }

    /// A message stored before previews still lists the thumbnail, with no
    /// address of its own; it is left out when read, and the files keep
    /// their places for opening.
    #[test]
    fn a_thumbnail_stored_as_a_file_is_left_out() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        let wire: WireMessage = serde_json::from_value(serde_json::json!({
            "_id": "i", "rid": "r", "msg": "", "ts": 10, "_updatedAt": 10,
            "u": { "_id": "lukas", "username": "lukas" }
        }))
        .unwrap();
        upsert_message(&conn, &wire, ME, "me").unwrap();
        conn.execute(
            "UPDATE chat_messages SET files = ?1 WHERE id = 'i'",
            [r#"[{"name":"boxes.png","path":"/file-upload/A/boxes.png"},{"name":"thumb-boxes.png"},{"name":"thumb-notes.txt","path":"/file-upload/C/thumb-notes.txt"}]"#],
        )
        .unwrap();
        let c = conversation(&conn, "r", "", ME, "me", None)
            .unwrap()
            .unwrap();
        let names: Vec<&str> = c.messages[0]
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, ["boxes.png", "thumb-notes.txt"]);
        let second = message_file(&conn, "i", 1).unwrap().unwrap();
        assert_eq!(second.name, "thumb-notes.txt");
    }

    /// An image the server made no thumbnail of is its own preview, unless it
    /// is too large to draw in the conversation.
    #[test]
    fn an_image_without_a_thumbnail_previews_itself_when_small() {
        let image = |size: u64| -> ChatFile {
            let wire: WireMessage = serde_json::from_value(serde_json::json!({
                "_id": "i", "rid": "r", "msg": "", "ts": 10, "_updatedAt": 10,
                "u": { "_id": "lukas", "username": "lukas" },
                "files": [{ "_id": "A", "name": "a.gif", "type": "image/gif", "size": size }],
                "attachments": [{ "title_link": "/file-upload/A/a.gif", "image_url": "/file-upload/A/a.gif" }]
            }))
            .unwrap();
            files_of(&wire).remove(0)
        };
        assert_eq!(image(1000).preview.as_deref(), Some("/file-upload/A/a.gif"));
        assert_eq!(image(PREVIEW_MAX_BYTES + 1).preview, None);
    }

    /// With the tasks' tables beside it, a thread shows the open tasks made
    /// from it, and a message the tasks made from it.
    #[test]
    fn a_task_made_from_a_message_marks_its_thread_and_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orb.db");
        let db = crate::db::Database::new(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();
        let conn = &db.conn;
        room(conn, "r", "c", "dev");
        msg(conn, plain("m1", "r", "lukas", 10));
        let task = |id: i64, done: bool| {
            conn.execute(
                "INSERT INTO tasks (id, task_uuid, subject, created_at, updated_at, completed_at,
                    position, account_name)
                 VALUES (?1, ?2, 'Look at it', '', '', ?3, 1.0, 'Work')",
                params![id, format!("uuid-{id}"), done.then_some("2026-10-25")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_linked_emails (task_id, message_id)
                 VALUES (?1, 'chat://chat.example.com/r/m1')",
                [id],
            )
            .unwrap();
        };
        task(1, true);
        assert!(!waiting(conn, ME, 0, &|_| true).unwrap()[0].task);
        task(2, false);
        assert!(waiting(conn, ME, 0, &|_| true).unwrap()[0].task);
        let c = conversation(conn, "r", "", ME, "me", None)
            .unwrap()
            .unwrap();
        let done: Vec<bool> = c.messages[0].tasks.iter().map(|t| t.done).collect();
        assert_eq!(done, [true, false]);
    }

    #[test]
    fn my_reply_counts_in_the_thread_after_the_message() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        set_state(&conn, STATE_USER_ID, ME).unwrap();
        msg(&conn, plain("1", "r", "lukas", 10));
        msg(
            &conn,
            Msg {
                tmid: Some("1"),
                ..plain("2", "r", ME, 20)
            },
        );
        assert_eq!(my_reply_after(&conn, "r", "", 10).unwrap(), None);
        assert_eq!(my_reply_after(&conn, "r", "1", 10).unwrap(), Some(20));
        msg(&conn, plain("3", "r", ME, 30));
        assert_eq!(my_reply_after(&conn, "r", "", 10).unwrap(), Some(30));
    }

    #[test]
    fn unread_starts_at_the_newest_message() {
        let conn = db();
        room(&conn, "r", "c", "dev");
        msg(&conn, plain("a", "r", "lukas", 10));
        msg(&conn, plain("b", "r", "lukas", 20));
        set_seen(&conn, "r", "", 20).unwrap();
        assert!(!waiting(&conn, ME, 0, &|_| true).unwrap()[0].unread);
        unsee(&conn, "r", "").unwrap();
        assert!(waiting(&conn, ME, 0, &|_| true).unwrap()[0].unread);
        assert_eq!(newest_id(&conn, "r", "").unwrap().as_deref(), Some("b"));
    }
}
