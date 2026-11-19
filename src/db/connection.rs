use crate::Result;
use crate::constants::{CLOCK_SKEW_BUFFER_SECONDS, DB_BUSY_TIMEOUT_MS, SYNC_LOCK_TIMEOUT_SECONDS};
use crate::models::{Folder, Message, MessageListItem, SearchResultItem};
use crate::store::{
    MessageSort, PendingActionCount, PendingActionRecord, PendingScope, SearchScope,
};
use chrono::DateTime;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Result of reconciling the folders table against the current configuration.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Number of folder rows removed.
    pub folders_removed: usize,
    /// Number of message rows removed (via `ON DELETE CASCADE`).
    pub messages_removed: usize,
}

/// Current schema version - increment when making schema changes
///
/// Unchanged by the vector index: it adds no column to any table, is created
/// idempotently beside the full-text index, and holds only data derived from
/// `messages`. There is nothing for a migration to do.
const SCHEMA_VERSION: i32 = 38;

/// Make `vec0` available to every connection opened from here on.
///
/// sqlite-vec is a SQLite extension, registered as an auto-extension so that
/// each new connection gets it — including the ones the pool opens later.
/// Registering more than once is harmless but pointless, hence the `Once`.
///
/// If this fails the vector table cannot be created, which is reported where
/// the schema is built rather than here: exact search does not need it and
/// must keep working without it.
fn register_vector_extension() {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is the extension entry point sqlite-vec
        // exports for exactly this purpose, and it is registered before any
        // connection is opened.
        unsafe {
            let result = rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut i8,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
            if result != rusqlite::ffi::SQLITE_OK {
                crate::logger::warn(
                    "could not register the vector extension; semantic search will be \
                     unavailable and exact search unaffected",
                );
            }
        }
    });
}

/// Definition of the full-text index and the triggers that maintain it.
///
/// Shared between the initial schema and the migration that rebuilds it, so the
/// two cannot describe different indexes.
///
/// External content: the index refers to `messages_search`, a view over
/// `messages` and `message_bodies`, by rowid rather than keeping its own copy
/// of every indexed column. That mattered little while bodies were absent; now
/// that they are fetched for the whole mailbox it is the difference between
/// one copy of the mail text and two.
///
/// The tokenizer stems, so a search for a word also finds its inflections, and
/// folds diacritics.
/// How close to the best match a result must be to be worth showing.
///
/// Relative because closeness is not comparable between queries: a phrase with
/// an obvious answer scores higher throughout than a vague one, so a fixed
/// number would flood one and starve the other.
const SEMANTIC_RELATIVE_FLOOR: f32 = 0.8;

/// The floor below which nothing is relevant, whatever the best match was.
///
/// Stops a query with no good answer from returning a long tail of things that
/// are merely least unlike it.
const SEMANTIC_ABSOLUTE_FLOOR: f32 = 0.3;

/// How many messages the vector index is asked for before re-ranking.
///
/// Large enough that a query narrowed by a sender or a folder still has
/// something left after filtering, small enough that re-embedding the
/// shortlist stays in the low milliseconds.
const SEMANTIC_CANDIDATES: usize = 600;

/// A message's id and the text it is embedded from: `(id, subject, body)`.
///
/// Named because the same tuple is both the indexing queue and the
/// nearest-neighbour shortlist, and both are fed to `embed::embedding_text`.
pub type EmbeddableMessage = (i64, Option<String>, Option<String>);

/// The vector index.
///
/// Binary-quantised: one bit per dimension, so 32 bytes a message rather than
/// 1KB. At 290,000 messages that is 16MB against 305MB, and a search of 3.5ms
/// against 24ms. What quantisation costs in ranking quality is bought back by
/// re-scoring the candidates it returns at full precision — see `search::embed`.
///
/// Not external-content and not trigger-maintained, unlike the full-text index:
/// a vector cannot be derived in SQL, so rows are written by the indexing pass
/// and a message whose text changes is re-embedded by it.
const VECTOR_SCHEMA: &str = "
    CREATE VIRTUAL TABLE IF NOT EXISTS message_vectors USING vec0(
        message_id INTEGER PRIMARY KEY,
        embedding bit[256]
    );
";

const FTS_SCHEMA: &str = "
    -- What the index reads: the headers from `messages`, the text from
    -- `message_bodies`, which holds it apart so that a row of `messages` stays
    -- small.
    CREATE VIEW IF NOT EXISTS messages_search AS
        SELECT m.id AS id, m.subject AS subject, m.from_addr AS from_addr,
               m.to_addr AS to_addr, m.cc_addr AS cc_addr, b.body_text AS body_text
        FROM messages m LEFT JOIN message_bodies b ON b.message_id = m.id;

    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        subject,
        from_addr,
        to_addr,
        cc_addr,
        body_text,
        content = 'messages_search',
        content_rowid = 'id',
        tokenize = 'porter unicode61 remove_diacritics 2'
    );

    -- An entry is indexed with exactly what the view shows for it, and taken
    -- out with exactly that again: an external-content index cannot read a row
    -- that is already gone, and a 'delete' with other values corrupts it.

    CREATE TRIGGER IF NOT EXISTS messages_fts_ai AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, cc_addr, body_text)
        VALUES (new.id, new.subject, new.from_addr, new.to_addr, new.cc_addr,
                (SELECT body_text FROM message_bodies WHERE message_id = new.id));
    END;

    -- The body goes with its message, after the index has been told what it was.
    CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, subject, from_addr, to_addr, cc_addr,
                                 body_text)
        VALUES ('delete', old.id, old.subject, old.from_addr, old.to_addr, old.cc_addr,
                (SELECT body_text FROM message_bodies WHERE message_id = old.id));
        DELETE FROM message_bodies WHERE message_id = old.id;
    END;

    -- Scoped to the indexed columns and guarded by a WHEN clause that only
    -- re-indexes when a value actually changes: the sync upsert always names
    -- these columns, so `AFTER UPDATE OF` alone still fires on every flag-only
    -- re-sync. `IS NOT` compares NULL-safely.
    CREATE TRIGGER IF NOT EXISTS messages_fts_au
        AFTER UPDATE OF subject, from_addr, to_addr, cc_addr ON messages
        WHEN new.subject IS NOT old.subject
            OR new.from_addr IS NOT old.from_addr
            OR new.to_addr IS NOT old.to_addr
            OR new.cc_addr IS NOT old.cc_addr
        BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, subject, from_addr, to_addr, cc_addr,
                                 body_text)
        VALUES ('delete', old.id, old.subject, old.from_addr, old.to_addr, old.cc_addr,
                (SELECT body_text FROM message_bodies WHERE message_id = old.id));
        INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, cc_addr, body_text)
        VALUES (new.id, new.subject, new.from_addr, new.to_addr, new.cc_addr,
                (SELECT body_text FROM message_bodies WHERE message_id = new.id));
    END;

    -- A body arriving, changing or going re-indexes its message, and keeps
    -- `messages.has_body_text` true to it for the body backfill's index.
    CREATE TRIGGER IF NOT EXISTS message_bodies_ai AFTER INSERT ON message_bodies BEGIN
        UPDATE messages SET has_body_text = 1
            WHERE id = new.message_id AND new.body_text IS NOT NULL;
        INSERT INTO messages_fts(messages_fts, rowid, subject, from_addr, to_addr, cc_addr,
                                 body_text)
            SELECT 'delete', id, subject, from_addr, to_addr, cc_addr, NULL FROM messages
            WHERE id = new.message_id AND new.body_text IS NOT NULL;
        INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, cc_addr, body_text)
            SELECT id, subject, from_addr, to_addr, cc_addr, new.body_text FROM messages
            WHERE id = new.message_id AND new.body_text IS NOT NULL;
    END;

    CREATE TRIGGER IF NOT EXISTS message_bodies_au
        AFTER UPDATE OF body_text ON message_bodies
        WHEN new.body_text IS NOT old.body_text
        BEGIN
        UPDATE messages
            SET has_body_text = CASE WHEN new.body_text IS NULL THEN NULL ELSE 1 END
            WHERE id = new.message_id;
        INSERT INTO messages_fts(messages_fts, rowid, subject, from_addr, to_addr, cc_addr,
                                 body_text)
            SELECT 'delete', id, subject, from_addr, to_addr, cc_addr, old.body_text
            FROM messages WHERE id = new.message_id;
        INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, cc_addr, body_text)
            SELECT id, subject, from_addr, to_addr, cc_addr, new.body_text FROM messages
            WHERE id = new.message_id;
    END;

    -- Only while the message is still there: deleting a message takes its
    -- body out of the index itself.
    CREATE TRIGGER IF NOT EXISTS message_bodies_ad AFTER DELETE ON message_bodies BEGIN
        UPDATE messages SET has_body_text = NULL WHERE id = old.message_id;
        INSERT INTO messages_fts(messages_fts, rowid, subject, from_addr, to_addr, cc_addr,
                                 body_text)
            SELECT 'delete', id, subject, from_addr, to_addr, cc_addr, old.body_text
            FROM messages WHERE id = old.message_id AND old.body_text IS NOT NULL;
        INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, cc_addr, body_text)
            SELECT id, subject, from_addr, to_addr, cc_addr, NULL FROM messages
            WHERE id = old.message_id AND old.body_text IS NOT NULL;
    END;
";

/// The tables every part of Orb but the task feature depends on.
///
/// Extracted from `init_schema` unchanged, down to its indentation: SQLite
/// stores the text of a `CREATE` statement as it was written, so reflowing one
/// makes a newly created database differ from an existing one in
/// `sqlite_master` for no reason at all. Having it as a constant is what lets
/// a test build a database in an older version's shape and migrate it.
const CORE_SCHEMA: &str = "
            CREATE TABLE IF NOT EXISTS folders (
                id INTEGER PRIMARY KEY,
                account_name TEXT NOT NULL,
                name TEXT NOT NULL,
                uidvalidity INTEGER,
                highest_uid INTEGER,
                highest_modseq INTEGER,
                last_sync_time TEXT,
                flags_checked_at TEXT,
                sync_version INTEGER NOT NULL DEFAULT 0,
                UNIQUE(account_name, name)
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY,
                folder_id INTEGER NOT NULL,
                uid INTEGER NOT NULL,
                message_id TEXT,
                subject TEXT,
                from_addr TEXT,
                to_addr TEXT,
                cc_addr TEXT,
                date TEXT,
                body_text TEXT,
                body_html TEXT,
                rfc822 BLOB,
                -- 1/0 once the shape is known; NULL while it is not.
                has_attachments INTEGER,
                -- MIME part paths from BODYSTRUCTURE, recorded during sync so the
                -- body worker can fetch just the readable text.
                text_part TEXT,
                html_part TEXT,
                text_encoding TEXT,
                text_bytes INTEGER,
                -- 1 when the server's BODYSTRUCTURE could not be parsed, which
                -- is why this message has no parts. Separates a malformed
                -- description from a message that genuinely has no text.
                shape_unreadable INTEGER,
                size INTEGER,
                flags TEXT,
                modseq INTEGER,
                FOREIGN KEY (folder_id) REFERENCES folders(id) ON DELETE CASCADE,
                UNIQUE(folder_id, uid)
            );

            CREATE INDEX IF NOT EXISTS idx_messages_folder_id ON messages(folder_id);
            CREATE INDEX IF NOT EXISTS idx_messages_folder_uid ON messages(folder_id, uid);
            CREATE INDEX IF NOT EXISTS idx_messages_folder_date ON messages(folder_id, date DESC);

            -- The full-text index and its triggers live in `FTS_SCHEMA`, applied
            -- just after this batch, so the definition here and the one the
            -- migration recreates cannot drift apart.

            -- Multi-instance support: sync versioning
            CREATE TABLE IF NOT EXISTS sync_meta (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );

            -- Global, strictly-monotonic change counter for cross-process
            -- change detection. Every folder insert/update/delete bumps this
            -- value by one. Because it only ever increases, a deletion can no
            -- longer mask a concurrent change (which a collision-prone
            -- SUM(sync_version) aggregate could, since a deletion decreases the
            -- sum back to a previously-seen value).
            CREATE TRIGGER IF NOT EXISTS folders_bump_change_counter_ai
            AFTER INSERT ON folders BEGIN
                UPDATE sync_meta SET value = value + 1 WHERE key = 'sync_version';
            END;
            CREATE TRIGGER IF NOT EXISTS folders_bump_change_counter_au
            AFTER UPDATE ON folders BEGIN
                UPDATE sync_meta SET value = value + 1 WHERE key = 'sync_version';
            END;
            CREATE TRIGGER IF NOT EXISTS folders_bump_change_counter_ad
            AFTER DELETE ON folders BEGIN
                UPDATE sync_meta SET value = value + 1 WHERE key = 'sync_version';
            END;

            -- Multi-instance support: sync coordination locks
            CREATE TABLE IF NOT EXISTS sync_locks (
                account_name TEXT NOT NULL,
                folder_name TEXT NOT NULL,
                locked_at TEXT NOT NULL,
                lock_holder TEXT NOT NULL,
                PRIMARY KEY (account_name, folder_name)
            );

            -- Contacts table for address autocompletion
            CREATE TABLE IF NOT EXISTS contacts (
                id INTEGER PRIMARY KEY,
                email TEXT NOT NULL UNIQUE COLLATE NOCASE,
                display_name TEXT,
                sent_count INTEGER NOT NULL DEFAULT 0,
                received_count INTEGER NOT NULL DEFAULT 0,
                last_sent TEXT,
                last_received TEXT,
                is_manual BOOLEAN NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_contacts_email ON contacts(email);
            CREATE INDEX IF NOT EXISTS idx_contacts_sent_count ON contacts(sent_count DESC);

            -- The task tables and their indexes live in `TASK_TABLES_SCHEMA`
            -- and `TASK_INDEXES_SCHEMA`, applied after the migrations for the
            -- same reason the full-text index is: their indexes name columns
            -- that migration 18 adds, so on an existing database this batch
            -- would fail before that migration could run.

            -- Staged (pending) destructive actions: delete/move operations the
            -- user has marked but not yet executed (aerc-style staging). Persisted
            -- so they survive app relaunch and are shared across windows. At most
            -- one pending action per (account, folder, uid) — a later stage for
            -- the same message replaces the earlier one (last action wins).
            CREATE TABLE IF NOT EXISTS pending_actions (
                id INTEGER PRIMARY KEY,
                account TEXT NOT NULL,
                folder TEXT NOT NULL,
                uid INTEGER NOT NULL,
                action_type TEXT NOT NULL CHECK (action_type IN ('delete', 'move')),
                target_folder TEXT,
                created_at TEXT NOT NULL,
                UNIQUE(account, folder, uid)
            );
            CREATE INDEX IF NOT EXISTS idx_pending_actions_folder
                ON pending_actions(account, folder);
            -- Server writes made locally first and still to be carried out:
            -- flag changes, the moves and deletes of applied staged actions,
            -- and mail that could not reach SMTP. Drained in order, per
            -- account, whenever the server is reachable.
            CREATE TABLE IF NOT EXISTS outbox (
                id INTEGER PRIMARY KEY,
                account TEXT NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN ('flags', 'send', 'move')),
                payload TEXT NOT NULL,
                created_at TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                -- A send the server refused: kept, but no longer drained.
                failed_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_outbox_account ON outbox(account, id);
";

/// The outbox, as of schema v19. Also part of the base schema above.
const OUTBOX_SCHEMA: &str = "
            -- Server writes made locally first and still to be carried out:
            -- flag changes made while offline, and mail that could not reach
            -- SMTP. Drained in order, per account, whenever the server is
            -- reachable.
            CREATE TABLE IF NOT EXISTS outbox (
                id INTEGER PRIMARY KEY,
                account TEXT NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN ('flags', 'send')),
                payload TEXT NOT NULL,
                created_at TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_outbox_account ON outbox(account, id);
";

/// What the filter rules remember, as of schema v20: one row per message a
/// rule acted on. It keeps a message from being filtered twice in one folder
/// — a move undone puts it back under a UID above the watermark — and it is
/// what the review panel lists as the filters' last work.
const FILTER_LOG_SCHEMA: &str = "
            CREATE TABLE IF NOT EXISTS filter_log (
                id INTEGER PRIMARY KEY,
                account TEXT NOT NULL,
                folder TEXT NOT NULL,
                message_id TEXT,
                rule TEXT NOT NULL,
                actions TEXT NOT NULL,
                subject TEXT,
                from_addr TEXT,
                -- When the run that did this started, shared by every row of it.
                run_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_filter_log_message
                ON filter_log(account, folder, message_id);
            CREATE INDEX IF NOT EXISTS idx_filter_log_run ON filter_log(account, run_at);
";

/// The server's folder list as of the last LIST of each account, with the
/// role it gives each (schema v22) and its hierarchy delimiter (v27).
const SERVER_FOLDERS_SCHEMA: &str = "
            CREATE TABLE IF NOT EXISTS server_folders (
                account_name TEXT NOT NULL,
                name TEXT NOT NULL,
                special_use TEXT,
                delimiter TEXT,
                PRIMARY KEY (account_name, name)
            );
";

/// What Orb keeps of the chat server (schema v33): its rooms with the
/// subscription state, the messages synced so far, the marks that take a
/// thread out of Waiting, and the sync's own bookkeeping. Times are
/// milliseconds since the epoch, as the server gives them.
///
/// A thread is named by its room and the id of its first message; the room's
/// own timeline, outside any thread, by its room and an empty thread id.
pub(crate) const CHAT_SCHEMA: &str = "
            CREATE TABLE IF NOT EXISTS chat_rooms (
                rid TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT,
                fname TEXT,
                topic TEXT,
                users_count INTEGER,
                last_message_at INTEGER,
                updated_at INTEGER NOT NULL DEFAULT 0,
                synced_until INTEGER,
                subscribed INTEGER NOT NULL DEFAULT 1,
                unread INTEGER NOT NULL DEFAULT 0,
                user_mentions INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS chat_messages (
                id TEXT PRIMARY KEY,
                rid TEXT NOT NULL,
                tmid TEXT,
                author_id TEXT NOT NULL,
                author_username TEXT,
                author_name TEXT,
                body TEXT NOT NULL,
                ts INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                edited_at INTEGER,
                system_type TEXT,
                tcount INTEGER NOT NULL DEFAULT 0,
                tlm INTEGER,
                mentions_me INTEGER NOT NULL DEFAULT 0,
                files TEXT,
                reactions TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_chat_messages_room ON chat_messages(rid, ts);
            CREATE INDEX IF NOT EXISTS idx_chat_messages_thread ON chat_messages(tmid, ts)
                WHERE tmid IS NOT NULL;
            CREATE TABLE IF NOT EXISTS chat_marks (
                rid TEXT NOT NULL,
                thread TEXT NOT NULL,
                kind TEXT NOT NULL,
                at INTEGER NOT NULL,
                PRIMARY KEY (rid, thread)
            );
            CREATE TABLE IF NOT EXISTS chat_seen (
                rid TEXT NOT NULL,
                thread TEXT NOT NULL,
                at INTEGER NOT NULL,
                PRIMARY KEY (rid, thread)
            );
            CREATE TABLE IF NOT EXISTS chat_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
";

/// Migration 34: the messages a chat message quotes.
pub(crate) const CHAT_QUOTES_SCHEMA: &str = "ALTER TABLE chat_messages ADD COLUMN quotes TEXT;";

/// Schema v35: a refused send stays in the outbox, marked rather than drained.
const OUTBOX_FAILED_SCHEMA: &str = "ALTER TABLE outbox ADD COLUMN failed_at TEXT;";

/// Schema v36: every room's messages are fetched again, back to the start of
/// the history Orb keeps, so that an image already cached gains its preview.
const CHAT_REFETCH_SCHEMA: &str = "UPDATE chat_rooms SET synced_until = NULL;";

/// Migration 37: whether a chat message calls the whole room (`@here`,
/// `@all`), which a muted room still lets through.
pub(crate) const CHAT_MENTIONS_ALL_SCHEMA: &str =
    "ALTER TABLE chat_messages ADD COLUMN mentions_all INTEGER NOT NULL DEFAULT 0;";

/// How long the list's snippet of a message may be, in characters.
/// Schema v38: the running focus (`crate::focus`), of which there is at most
/// one. `ends_at` is NULL for a focus the user ends.
const FOCUS_SCHEMA: &str = "
            CREATE TABLE IF NOT EXISTS focus_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                mode TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                ends_at INTEGER
            );
";

pub const SNIPPET_CHARS: usize = 200;

/// A message's text as one line for the list: quoted lines and the
/// attribution above them left out, whitespace collapsed, cut at
/// [`SNIPPET_CHARS`] characters, and nothing after the signature marker.
pub fn snippet(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line == "--" {
            break;
        }
        if line.starts_with('>') || (line.ends_with("wrote:") && line.len() > 6) {
            continue;
        }
        for word in line.split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
        }
        if out.chars().count() >= SNIPPET_CHARS {
            break;
        }
    }
    match out.char_indices().nth(SNIPPET_CHARS) {
        Some((at, _)) => out[..at].to_string(),
        None => out,
    }
}

/// The extra headers as the column stores them: a JSON list of pairs, or
/// nothing when there are none.
fn extra_headers_json(headers: &crate::threads::ThreadHeaders) -> Option<String> {
    (!headers.extra.is_empty()).then(|| serde_json::to_string(&headers.extra).unwrap_or_default())
}

/// The reading list's feeds, as of schema v32: what this machine last heard
/// from each feed, and every item id it has seen, so that an item archived or
/// deleted is not brought back. One text, applied by migration 32 and by
/// `init_schema`, as the task tables are.
const FEED_TABLES_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS feeds (
        url TEXT PRIMARY KEY,
        title TEXT,
        etag TEXT,
        last_modified TEXT,
        -- Unix seconds.
        polled_at INTEGER,
        last_error TEXT
    );
    CREATE TABLE IF NOT EXISTS feed_items (
        item_id TEXT PRIMARY KEY,
        feed_url TEXT NOT NULL,
        -- Unix seconds.
        seen_at INTEGER NOT NULL
    );
";

/// What this machine last heard from a feed (see [`FEED_TABLES_SCHEMA`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeedState {
    pub title: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Unix seconds.
    pub polled_at: Option<i64>,
    pub last_error: Option<String>,
}

/// The task tables, as of schema v18.
///
/// One definition, applied by migration 18 and again by `init_schema` once the
/// migrations have run, so the shape a fresh install is created in and the
/// shape an upgrade arrives at are the same text rather than two texts that
/// have to be kept in step. They were not: v8 declared these tables twice —
/// once in migration 8, once in the fresh-database batch — and the two had
/// already drifted apart in whitespace, which is enough to make
/// `sqlite_master` differ between two installs of the same release.
///
/// Applied *after* the migrations, like `FTS_SCHEMA` and for the same reason:
/// the indexes below name columns (`deleted_at`, `revision`) that migration 18
/// adds, so on an existing database this batch cannot run before it.
///
/// Tables and indexes are separate constants because migration 18 rebuilds the
/// v8 tables by renaming them aside — a renamed table keeps its indexes, and
/// with them their names, so the new indexes can only be created once the old
/// tables have been dropped.
const TASK_TABLES_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS tasks (
        id INTEGER PRIMARY KEY,

        -- The task's identity, generated on creation and stable forever. It is
        -- deliberately not `message_id`: every edit is an APPEND followed by a
        -- delete, so the RFC Message-ID of the message a task currently lives
        -- in changes constantly and identifies an edit, not a task.
        -- Reconciliation groups server messages by this.
        task_uuid TEXT NOT NULL UNIQUE,

        -- Incremented on every local change. Conflicts between two devices are
        -- decided by highest revision, ties by highest UID, so this is the only
        -- thing standing between a slow round trip and a lost edit.
        revision INTEGER NOT NULL DEFAULT 0,

        subject TEXT NOT NULL,
        body_text TEXT,

        -- Temporal properties (TEXT for display, INTEGER for queries).
        -- `snooze_until` is the postpone date and `deadline` the due date;
        -- the v8 names are kept so existing rows and queries keep working.
        snooze_until TEXT,
        snooze_until_unix INTEGER,
        deadline TEXT,
        deadline_unix INTEGER,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        completed_at TEXT,

        -- Soft delete. A trashed task keeps its row (and its message, with the
        -- deletion in a header) so that the deletion syncs and can be undone;
        -- only the 30-day sweep or an explicit empty-trash removes it.
        deleted_at TEXT,

        -- An RFC 5545 RRULE subset, NULL for a one-off task. Completing a
        -- recurring task advances these dates instead of ending the task, so
        -- the rule lives on the task rather than on a completion.
        recurrence TEXT,
        -- Advance from the completion date rather than from the scheduled one,
        -- for habit-style tasks that should not drift when done late.
        recurrence_from_completion INTEGER NOT NULL DEFAULT 0,

        -- The single global manual order. REAL so that a move writes one row.
        position REAL NOT NULL,

        is_working_on BOOLEAN NOT NULL DEFAULT 0,
        is_completed BOOLEAN NOT NULL DEFAULT 0,
        is_dirty BOOLEAN NOT NULL DEFAULT 0,

        account_name TEXT NOT NULL,
        imap_uid INTEGER,
        -- The Message-ID of the server message this task is currently stored
        -- in, kept only so that message can be found and expunged after the
        -- next rewrite. Not UNIQUE any more, and never identity.
        message_id TEXT,
        modseq INTEGER,

        -- The checklist of §17 as JSON, NULL when there is none.
        steps TEXT,

        UNIQUE(account_name, imap_uid)
    );

    CREATE TABLE IF NOT EXISTS task_reminders (
        id INTEGER PRIMARY KEY,
        task_id INTEGER NOT NULL,
        remind_at TEXT NOT NULL,
        remind_at_unix INTEGER NOT NULL,
        -- Local to this machine and deliberately absent from the wire format:
        -- every machine fires its own notification.
        is_triggered BOOLEAN NOT NULL DEFAULT 0,
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS task_linked_emails (
        id INTEGER PRIMARY KEY,
        task_id INTEGER NOT NULL,
        message_id TEXT NOT NULL,
        -- The cache is the point: it is what keeps the link readable years
        -- later, after the message has been moved, archived or deleted.
        cached_subject TEXT,
        cached_from TEXT,
        cached_date TEXT,
        cached_account TEXT,
        cached_folder TEXT,
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
        UNIQUE(task_id, message_id)
    );

    CREATE TABLE IF NOT EXISTS task_attachments (
        id INTEGER PRIMARY KEY,
        task_id INTEGER NOT NULL,
        -- The attachment's own identity, listed in the task message's header
        -- and carried by the separate message that holds the payload.
        attachment_uuid TEXT NOT NULL UNIQUE,
        filename TEXT NOT NULL,
        content_type TEXT,
        size INTEGER,
        -- Where the payload is cached, whether or not it has been downloaded
        -- yet: the path is derived from the task and attachment ids, so it can
        -- be written down before there is a file at it.
        file_path TEXT NOT NULL,
        -- The message the payload lives in. Payloads are separate messages so
        -- that editing a task's title does not re-upload a 10 MB file, which
        -- means an attachment has a UID and a Message-ID of its own.
        imap_uid INTEGER,
        message_id TEXT,
        -- Whether `file_path` currently holds the payload. The cache is
        -- disposable; this is what lets it be re-fetched rather than trusted.
        is_cached INTEGER NOT NULL DEFAULT 0,
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
    );

    -- The tag registry. It is synced as its own message in the tasks folder,
    -- so a rename rewrites one message instead of every task that uses the tag.
    CREATE TABLE IF NOT EXISTS task_tags (
        id INTEGER PRIMARY KEY,
        tag_uuid TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        colour TEXT,
        position REAL NOT NULL DEFAULT 0
    );

    -- Which tags a task carries. `tag_uuid` is intentionally not a foreign key
    -- into `task_tags`: a task may arrive from the server referencing a tag
    -- whose registry entry has not synced yet, and an unresolvable tag is
    -- rendered as a placeholder rather than silently dropped. A real foreign
    -- key would drop it.
    CREATE TABLE IF NOT EXISTS task_tag_links (
        id INTEGER PRIMARY KEY,
        task_id INTEGER NOT NULL,
        tag_uuid TEXT NOT NULL,
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
        UNIQUE(task_id, tag_uuid)
    );

    -- The append-only task log. Merged as a union rather than by
    -- last-writer-wins, which only works because every entry carries its own
    -- id: `entry_uuid` UNIQUE is what makes re-applying a log idempotent, and
    -- so what stops two devices from overwriting each other's history.
    CREATE TABLE IF NOT EXISTS task_log (
        id INTEGER PRIMARY KEY,
        task_id INTEGER NOT NULL,
        entry_uuid TEXT NOT NULL UNIQUE,
        occurred_at TEXT NOT NULL,
        occurred_at_unix INTEGER NOT NULL,
        -- Which machine appended the entry, so a merged timeline can say so.
        device_id TEXT,
        event_type TEXT NOT NULL,
        -- JSON: the machine-readable payload of the event (old and new values
        -- of a date change, the tag that was added, and so on).
        detail TEXT,
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS task_sync_state (
        id INTEGER PRIMARY KEY,
        account_name TEXT NOT NULL UNIQUE,
        -- No assumed prefix: a leading dot is a Courier/Maildir convention,
        -- not a universal one, so the default is the plain name and the
        -- configured value is used verbatim.
        folder_name TEXT NOT NULL DEFAULT 'Tasks',
        uidvalidity INTEGER,
        -- A fast path only. One server here never advances HIGHESTMODSEQ, so
        -- a sync that trusted this as the record of what changed would freeze
        -- silently; the columns below are what the full listing runs on.
        highest_modseq INTEGER,
        last_sync_time TEXT,
        -- When the folder was last listed in full (every UID with its task id
        -- and revision). Reconciliation is driven by that listing, and this is
        -- what lets a sync decide it is due for one.
        last_listing_at TEXT,
        last_listing_at_unix INTEGER,
        -- How many messages that listing saw, compared against the folder's
        -- message count on the next sync: a server that reports no change
        -- while the count moved has lied, and the listing runs anyway.
        last_listing_count INTEGER,
        -- This machine's identity, generated once and stamped into every log
        -- entry it appends. It belongs here rather than in the config because
        -- it must be generated, not configured, and must not travel when a
        -- config file is copied to another machine.
        device_id TEXT
    );

    -- Tasks that have been purged locally and whose messages are still on the
    -- server (§10.1).
    --
    -- Purging a task deletes the only row that knew its message existed, and
    -- reconciliation reads a task id it has no local row for as 'a task from
    -- another machine' and downloads it. Without a record of the purge, then,
    -- emptying the trash resurrects everything in it on the next sync, and so
    -- does an expunge that failed halfway.
    --
    -- Durable rather than in-memory for the same reason: the gap between the
    -- purge and the sync that expunges may contain a quit, a crash or a week
    -- offline. A row lives until the server stops listing the task.
    CREATE TABLE IF NOT EXISTS task_expunges (
        id INTEGER PRIMARY KEY,
        task_uuid TEXT NOT NULL UNIQUE,
        -- Where the task and its attachment payloads were last seen. Advisory:
        -- a UID is only ever acted on if the current listing shows it too,
        -- because after a UIDVALIDITY bump it names somebody else's mail.
        imap_uid INTEGER,
        message_id TEXT,
        attachment_uids TEXT,
        recorded_at TEXT NOT NULL
    );
";

/// The indexes over the task tables, as of schema v18.
///
/// Every view in the sidebar is a filtered projection of one global list, so
/// each gets a partial index whose predicate is the view's filter — including
/// `deleted_at IS NULL`, since a trashed task must vanish from every view but
/// Trash, and a partial index is the cheapest possible way to say so.
const TASK_INDEXES_SCHEMA: &str = "
    -- Today asks two questions — what is postponed until at most today, and
    -- what is due by at most today — and Upcoming asks which open task has
    -- either date at all. Two single-column indexes rather than one
    -- composite: the Today predicate is an OR of the two dates, which SQLite
    -- can only answer by unioning two index scans.
    CREATE INDEX IF NOT EXISTS idx_tasks_open_postpone ON tasks(snooze_until_unix)
        WHERE is_completed = 0 AND deleted_at IS NULL;
    CREATE INDEX IF NOT EXISTS idx_tasks_open_due ON tasks(deadline_unix)
        WHERE is_completed = 0 AND deleted_at IS NULL;

    -- Someday: open, undated, in manual order. The 'undated' half of the
    -- filter is in the index, so the scan never visits a dated task.
    CREATE INDEX IF NOT EXISTS idx_tasks_someday ON tasks(position)
        WHERE is_completed = 0 AND deleted_at IS NULL
            AND snooze_until_unix IS NULL AND deadline_unix IS NULL;

    -- Completed: newest first and paginated, which this turns into a backwards
    -- scan and a LIMIT rather than a sort of the whole history.
    CREATE INDEX IF NOT EXISTS idx_tasks_completed ON tasks(completed_at DESC)
        WHERE is_completed = 1 AND deleted_at IS NULL;

    -- Trash, and the 30-day sweep that empties it; both are ordered by when
    -- the task was trashed.
    CREATE INDEX IF NOT EXISTS idx_tasks_trash ON tasks(deleted_at DESC)
        WHERE deleted_at IS NOT NULL;

    -- The manual order itself, which every view above is a projection of and
    -- which the renormalisation pass walks end to end.
    CREATE INDEX IF NOT EXISTS idx_tasks_position ON tasks(position)
        WHERE is_completed = 0 AND deleted_at IS NULL;

    -- The upload queue, in revision order because that is the order the
    -- backlog has to reach the server in. Deliberately not filtered on
    -- `deleted_at`: a trashed task is dirty precisely so its deletion syncs.
    CREATE INDEX IF NOT EXISTS idx_tasks_dirty ON tasks(revision)
        WHERE is_dirty = 1;

    CREATE INDEX IF NOT EXISTS idx_tasks_account ON tasks(account_name);

    -- Lookup by `task_uuid` needs no index of its own: the UNIQUE constraint
    -- on the column already is one.

    CREATE INDEX IF NOT EXISTS idx_reminders_task ON task_reminders(task_id);
    CREATE INDEX IF NOT EXISTS idx_reminders_pending ON task_reminders(remind_at_unix)
        WHERE is_triggered = 0;

    CREATE INDEX IF NOT EXISTS idx_linked_emails_task ON task_linked_emails(task_id);
    -- The message list asks 'how many open tasks does each of these messages
    -- have' once per page, not once per row; this is what makes that a join.
    CREATE INDEX IF NOT EXISTS idx_linked_emails_message_id ON task_linked_emails(message_id);

    CREATE INDEX IF NOT EXISTS idx_attachments_task ON task_attachments(task_id);

    -- 'Which tasks carry this tag', for tag:<name> filtering and for marking
    -- tasks dirty when a tag is deleted from the registry.
    CREATE INDEX IF NOT EXISTS idx_task_tag_links_tag ON task_tag_links(tag_uuid);

    -- A task's timeline, read in order.
    CREATE INDEX IF NOT EXISTS idx_task_log_task ON task_log(task_id, occurred_at_unix);
";

/// A folder's unread messages, newest first; see [`Database::unread_message_list`].
/// Left to itself, SQLite walks the folder in date order to save the sort, and
/// so reads every row of it for its flags; the unread are few, the sort cheap.
const UNREAD_MESSAGE_LIST: &str = "
    SELECT id, folder_id, uid, message_id, subject, from_addr, to_addr, cc_addr,
           date, size, flags, modseq
    FROM messages INDEXED BY idx_messages_folder_flags
    WHERE folder_id = ?1 AND (flags IS NULL OR flags NOT LIKE '%Seen%')
    ORDER BY date_unix DESC";

pub struct Database {
    pub(crate) conn: Connection,
}

impl Database {
    pub fn new(path: &str) -> Result<Self> {
        register_vector_extension();

        // SQLite creates a missing database file but not the directory holding
        // it, and answers a missing directory with a bare "unable to open
        // database file". A first launch against the default `db_path` puts the
        // database in a `db/` subdirectory that has never existed, so without
        // this Orb refuses to start until the directory is made by hand.
        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            crate::files::create_private_dir(parent).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "could not create the database directory {}: {e}",
                        parent.display()
                    ),
                )
            })?;
        }

        let conn = Connection::open(path)?;

        // Enable critical pragmas for multi-instance safety and performance
        // Note: mmap_size removed as it can cause "database disk image is malformed"
        // errors on some systems when combined with WAL mode
        conn.execute_batch(&format!(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = MEMORY;
            PRAGMA busy_timeout = {};
        ",
            DB_BUSY_TIMEOUT_MS
        ))?;

        // The search finds part of a header case and accents aside, which
        // neither LIKE (ASCII case only) nor the index (whole words) does.
        conn.create_scalar_function(
            "orb_contains",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let haystack = ctx.get_raw(0).as_str_or_null().unwrap_or(None);
                let needle = ctx.get_raw(1).as_str_or_null().unwrap_or(None);
                Ok(match (haystack, needle) {
                    (Some(haystack), Some(needle)) => {
                        crate::search::contains_folded(haystack, needle)
                    }
                    _ => false,
                })
            },
        )?;

        Ok(Database { conn })
    }

    /// The connection itself, for a query the API has no method for: the
    /// integration tests read tables directly to check what a sync wrote.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Get the current schema version from the database
    /// Whether a table (or virtual table) exists.
    fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |row| row.get(0),
        )?)
    }

    fn get_schema_version(&self) -> Result<i32> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok(version)
    }

    /// Set the schema version in the database
    fn set_schema_version(&self, version: i32) -> Result<()> {
        // PRAGMA doesn't support parameters, so we need to format directly
        // This is safe since version is an i32
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {}", version))?;
        Ok(())
    }

    /// Check whether `table` already has a column named `column`.
    ///
    /// Used to make `ADD COLUMN` migrations idempotent: if a previous migration
    /// attempt was interrupted after the `ALTER` committed but before the schema
    /// version was bumped, re-running the migration would otherwise fail with a
    /// "duplicate column name" error and leave the database permanently
    /// unopenable.
    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        // PRAGMA table_info() does not support bound parameters for the table
        // name; the table name here is a compile-time constant, so this is safe.
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let exists = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|name| name == column);
        Ok(exists)
    }

    /// Run schema migrations to bring database up to current version.
    ///
    /// Each migration step (its schema changes together with its version bump)
    /// runs inside a single `BEGIN IMMEDIATE` transaction. This guarantees that
    /// a step is applied atomically: a crash or force-quit mid-migration leaves
    /// the database either fully at the previous version or fully at the new
    /// one, never half-migrated. Every step is also idempotent so that re-running
    /// after an interruption is always safe.
    fn run_migrations(&self, from_version: i32) -> Result<()> {
        // Migration from version 0 to 1: add date index
        if from_version < 1 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_messages_folder_date ON messages(folder_id, date DESC);
                 PRAGMA user_version = 1;",
            )?;
            tx.commit()?;
        }

        // Migration from version 1 to 2: convert comma-separated flags to JSON
        if from_version < 2 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            // Read all messages and convert flags from "flag1,flag2" to JSON array
            let mut stmt = tx.prepare(
                "SELECT id, flags FROM messages WHERE flags IS NOT NULL AND flags != ''",
            )?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            drop(stmt);

            for (id, flags_csv) in rows {
                // Skip if already looks like JSON (starts with [)
                if flags_csv.trim().starts_with('[') {
                    continue;
                }
                let flags: Vec<&str> = flags_csv.split(',').filter(|s| !s.is_empty()).collect();
                let flags_json = serde_json::to_string(&flags).unwrap_or_else(|_| "[]".to_string());
                tx.execute(
                    "UPDATE messages SET flags = ? WHERE id = ?",
                    params![flags_json, id],
                )?;
            }
            tx.execute_batch("PRAGMA user_version = 2;")?;
            tx.commit()?;
        }

        // Migration from version 2 to 3: normalize dates to RFC3339
        if from_version < 3 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            // Read all messages with non-null dates that aren't already RFC3339
            let mut stmt =
                tx.prepare("SELECT id, date FROM messages WHERE date IS NOT NULL AND date != ''")?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            drop(stmt);

            for (id, date_str) in rows {
                // Skip if already RFC3339 (contains 'T' separator)
                if date_str.contains('T') {
                    continue;
                }
                if let Some(normalized) = Self::normalize_date_for_migration(&date_str) {
                    tx.execute(
                        "UPDATE messages SET date = ? WHERE id = ?",
                        params![normalized, id],
                    )?;
                }
            }
            tx.execute_batch("PRAGMA user_version = 3;")?;
            tx.commit()?;
        }

        // Migration from version 3 to 4: re-run date normalization with fixed parser
        // (v3 migration may have run with broken day-of-week validation)
        if from_version < 4 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let mut stmt =
                tx.prepare("SELECT id, date FROM messages WHERE date IS NOT NULL AND date != ''")?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            drop(stmt);

            for (id, date_str) in rows {
                if date_str.contains('T') {
                    continue;
                }
                if let Some(normalized) = Self::normalize_date_for_migration(&date_str) {
                    tx.execute(
                        "UPDATE messages SET date = ? WHERE id = ?",
                        params![normalized, id],
                    )?;
                }
            }
            tx.execute_batch("PRAGMA user_version = 4;")?;
            tx.commit()?;
        }

        // Migration from version 4 to 5: re-run with mailparse::dateparse
        if from_version < 5 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let mut stmt =
                tx.prepare("SELECT id, date FROM messages WHERE date IS NOT NULL AND date != ''")?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            drop(stmt);

            for (id, date_str) in rows {
                if date_str.contains('T') {
                    continue;
                }
                if let Some(normalized) = Self::normalize_date_for_migration(&date_str) {
                    tx.execute(
                        "UPDATE messages SET date = ? WHERE id = ?",
                        params![normalized, id],
                    )?;
                }
            }
            tx.execute_batch("PRAGMA user_version = 5;")?;
            tx.commit()?;
        }

        // Migration from version 5 to 6: add date_unix column for reliable sorting
        if from_version < 6 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            // Add date_unix column (idempotent: skip if a prior interrupted run
            // already added it, otherwise re-running fails with "duplicate column
            // name" and bricks the database).
            if !Self::column_exists(&tx, "messages", "date_unix")? {
                tx.execute_batch("ALTER TABLE messages ADD COLUMN date_unix INTEGER;")?;
            }

            // Populate date_unix from existing dates
            let mut stmt =
                tx.prepare("SELECT id, date FROM messages WHERE date IS NOT NULL AND date != ''")?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            drop(stmt);

            for (id, date_str) in rows {
                if let Some(timestamp) = Self::parse_date_to_unix(&date_str) {
                    tx.execute(
                        "UPDATE messages SET date_unix = ? WHERE id = ?",
                        params![timestamp, id],
                    )?;
                }
            }

            // Create index on date_unix for efficient sorting
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_messages_folder_date_unix ON messages(folder_id, date_unix DESC);
                 PRAGMA user_version = 6;"
            )?;
            tx.commit()?;
        }

        // Migration from version 6 to 7: add contacts table for address autocompletion
        if from_version < 7 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS contacts (
                    id INTEGER PRIMARY KEY,
                    email TEXT NOT NULL UNIQUE COLLATE NOCASE,
                    display_name TEXT,
                    sent_count INTEGER NOT NULL DEFAULT 0,
                    received_count INTEGER NOT NULL DEFAULT 0,
                    last_sent TEXT,
                    last_received TEXT,
                    is_manual BOOLEAN NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_contacts_email ON contacts(email);
                CREATE INDEX IF NOT EXISTS idx_contacts_sent_count ON contacts(sent_count DESC);
                PRAGMA user_version = 7;",
            )?;
            tx.commit()?;
        }

        // Migration from version 7 to 8: add task management tables
        if from_version < 8 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "
                -- Task management tables
                CREATE TABLE IF NOT EXISTS tasks (
                    id INTEGER PRIMARY KEY,
                    subject TEXT NOT NULL,
                    body_text TEXT,

                    -- Temporal properties (TEXT for display, INTEGER for queries)
                    snooze_until TEXT,
                    snooze_until_unix INTEGER,
                    deadline TEXT,
                    deadline_unix INTEGER,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    completed_at TEXT,

                    -- Ordering (floating point for arbitrary positioning)
                    position REAL NOT NULL,

                    -- Status flags
                    is_working_on BOOLEAN NOT NULL DEFAULT 0,
                    is_completed BOOLEAN NOT NULL DEFAULT 0,
                    is_dirty BOOLEAN NOT NULL DEFAULT 0,

                    -- IMAP sync fields
                    account_name TEXT NOT NULL,
                    imap_uid INTEGER,
                    message_id TEXT UNIQUE,
                    modseq INTEGER,

                    UNIQUE(account_name, imap_uid)
                );

                CREATE INDEX IF NOT EXISTS idx_tasks_today ON tasks(snooze_until_unix)
                    WHERE is_completed = 0;
                CREATE INDEX IF NOT EXISTS idx_tasks_today_full ON tasks(is_completed, is_working_on, snooze_until_unix)
                    WHERE is_completed = 0;
                CREATE INDEX IF NOT EXISTS idx_tasks_deadline ON tasks(deadline_unix)
                    WHERE is_completed = 0 AND deadline_unix IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_tasks_position ON tasks(position)
                    WHERE is_completed = 0;
                CREATE INDEX IF NOT EXISTS idx_tasks_working_on ON tasks(is_working_on)
                    WHERE is_working_on = 1;
                CREATE INDEX IF NOT EXISTS idx_tasks_account ON tasks(account_name);
                CREATE INDEX IF NOT EXISTS idx_tasks_dirty ON tasks(is_dirty)
                    WHERE is_dirty = 1;

                -- Task reminders
                CREATE TABLE IF NOT EXISTS task_reminders (
                    id INTEGER PRIMARY KEY,
                    task_id INTEGER NOT NULL,
                    remind_at TEXT NOT NULL,
                    remind_at_unix INTEGER NOT NULL,
                    is_triggered BOOLEAN NOT NULL DEFAULT 0,
                    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS idx_reminders_task ON task_reminders(task_id);
                CREATE INDEX IF NOT EXISTS idx_reminders_pending ON task_reminders(remind_at_unix)
                    WHERE is_triggered = 0;

                -- Linked emails (Message-ID + cached metadata)
                CREATE TABLE IF NOT EXISTS task_linked_emails (
                    id INTEGER PRIMARY KEY,
                    task_id INTEGER NOT NULL,
                    message_id TEXT NOT NULL,
                    cached_subject TEXT,
                    cached_from TEXT,
                    cached_date TEXT,
                    cached_account TEXT,
                    cached_folder TEXT,
                    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
                    UNIQUE(task_id, message_id)
                );

                CREATE INDEX IF NOT EXISTS idx_linked_emails_task ON task_linked_emails(task_id);
                CREATE INDEX IF NOT EXISTS idx_linked_emails_message_id ON task_linked_emails(message_id);

                -- Task attachments (file-based storage, only metadata in DB)
                CREATE TABLE IF NOT EXISTS task_attachments (
                    id INTEGER PRIMARY KEY,
                    task_id INTEGER NOT NULL,
                    filename TEXT NOT NULL,
                    content_type TEXT,
                    size INTEGER,
                    file_path TEXT NOT NULL,
                    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS idx_attachments_task ON task_attachments(task_id);

                -- Task sync state (per-account tracking)
                CREATE TABLE IF NOT EXISTS task_sync_state (
                    id INTEGER PRIMARY KEY,
                    account_name TEXT NOT NULL UNIQUE,
                    folder_name TEXT NOT NULL DEFAULT '.Tasks',
                    uidvalidity INTEGER,
                    highest_modseq INTEGER,
                    last_sync_time TEXT
                );

                PRAGMA user_version = 8;
                ",
            )?;
            tx.commit()?;
        }

        // Migration from version 8 to 9: populate the full-text search index.
        //
        // The index is now created and filled at the end of `init_schema`,
        // after every migration, because an external-content index names
        // columns of `messages` and cannot exist before the table has them.
        // This backfill therefore only applies to a database that still has the
        // old standalone index; on any other it would run before the table
        // exists, and its work is done by the rebuild regardless.
        //
        // Migrations 9 to 11 read `messages.body_text`, which migration 30 moved
        // out; on a database past it they have nothing left to do.
        let bodies_in_row = Database::column_exists(&self.conn, "messages", "body_text")?;
        if from_version < 9 && bodies_in_row && Database::table_exists(&self.conn, "messages_fts")?
        {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "DELETE FROM messages_fts;
                 INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, body_text)
                     SELECT id, subject, from_addr, to_addr, body_text FROM messages;
                 PRAGMA user_version = 9;",
            )?;
            tx.commit()?;
        }

        // Migration from version 9 to 10: scope the FTS AFTER UPDATE trigger to
        // only the indexed columns.
        //
        // The original `messages_fts_au` trigger fired on *every* UPDATE to the
        // messages table, so flag-only sync updates (which never touch an
        // indexed column) performed a needless DELETE+INSERT that re-indexed the
        // whole message body — write amplification on large sync batches. On an
        // existing database the trigger was already created (by `init_schema`'s
        // `CREATE TRIGGER IF NOT EXISTS` batch) with the old unscoped
        // definition, so we DROP it and recreate it scoped to the indexed
        // columns (`AFTER UPDATE OF ...`). The INSERT/DELETE triggers are left
        // untouched. `DROP TRIGGER IF EXISTS` + `CREATE TRIGGER IF NOT EXISTS`
        // makes this idempotent. Re-indexing behaviour for indexed-column
        // changes is unchanged, so search results for equivalent data are
        // identical.
        if from_version < 10 && bodies_in_row && Database::table_exists(&self.conn, "messages_fts")?
        {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "DROP TRIGGER IF EXISTS messages_fts_au;
                 CREATE TRIGGER IF NOT EXISTS messages_fts_au
                     AFTER UPDATE OF subject, from_addr, to_addr, body_text ON messages BEGIN
                     DELETE FROM messages_fts WHERE rowid = old.id;
                     INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, body_text)
                     VALUES (new.id, new.subject, new.from_addr, new.to_addr, new.body_text);
                 END;
                 PRAGMA user_version = 10;",
            )?;
            tx.commit()?;
        }

        // Migration from version 10 to 11: add a value-change WHEN guard to the
        // FTS AFTER UPDATE trigger.
        //
        // Scoping to `AFTER UPDATE OF <cols>` (v10) was not enough in practice:
        // the live sync upsert (`bulk_insert_messages_in_tx`, ON CONFLICT DO
        // UPDATE SET) always names subject/from_addr/to_addr/body_text (body_text
        // via COALESCE), so the trigger still fired — and re-indexed the whole
        // body — on every flag-only re-sync. Adding a `WHEN` clause that only
        // re-indexes when an indexed column's VALUE actually changed eliminates
        // the redundant DELETE+INSERT while preserving search results for
        // equivalent data. `IS NOT` is NULL-safe.
        if from_version < 11 && bodies_in_row {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "DROP TRIGGER IF EXISTS messages_fts_au;
                 CREATE TRIGGER IF NOT EXISTS messages_fts_au
                     AFTER UPDATE OF subject, from_addr, to_addr, body_text ON messages
                     WHEN new.subject IS NOT old.subject
                         OR new.from_addr IS NOT old.from_addr
                         OR new.to_addr IS NOT old.to_addr
                         OR new.body_text IS NOT old.body_text
                     BEGIN
                     DELETE FROM messages_fts WHERE rowid = old.id;
                     INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, body_text)
                     VALUES (new.id, new.subject, new.from_addr, new.to_addr, new.body_text);
                 END;
                 PRAGMA user_version = 11;",
            )?;
            tx.commit()?;
        }

        // Migration from version 11 to 12: add the persisted `pending_actions`
        // table backing the staged delete/move feature. Idempotent
        // CREATE TABLE / CREATE INDEX IF NOT EXISTS so a re-run after an
        // interrupted migration is safe.
        if from_version < 12 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_actions (
                     id INTEGER PRIMARY KEY,
                     account TEXT NOT NULL,
                     folder TEXT NOT NULL,
                     uid INTEGER NOT NULL,
                     action_type TEXT NOT NULL CHECK (action_type IN ('delete', 'move')),
                     target_folder TEXT,
                     created_at TEXT NOT NULL,
                     UNIQUE(account, folder, uid)
                 );
                 CREATE INDEX IF NOT EXISTS idx_pending_actions_folder
                     ON pending_actions(account, folder);
                 PRAGMA user_version = 12;",
            )?;
            tx.commit()?;
        }

        // Migration from version 12 to 13: persist whether a message has
        // attachments.
        //
        // The list previously answered this by reading each row's full RFC822
        // source and parsing its MIME tree, on every page load. That is a
        // pointer chase over megabytes to decide whether to draw a paperclip:
        // one real folder here holds 5.8 MB across three cached messages, and a
        // single message can be 10 MB. Store the answer once, when the source is
        // saved, and let the list read a column.
        //
        // Rows whose source was cached before this migration are backfilled by
        // parsing them one final time. `has_attachments` stays NULL for rows
        // with no cached source, where the answer is genuinely unknown until the
        // body is fetched.
        if from_version < 13 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;

            // `ALTER TABLE ADD COLUMN` has no IF NOT EXISTS; tolerate a column
            // left behind by an interrupted run.
            let already_added = tx
                .execute(
                    "ALTER TABLE messages ADD COLUMN has_attachments INTEGER",
                    [],
                )
                .is_err();

            // A database old enough to predate `rfc822` has no cached sources
            // to backfill from, and later migrations will add the column.
            let backfilled = if !Database::column_exists(&self.conn, "messages", "rfc822")? {
                0
            } else {
                let mut stmt =
                    tx.prepare("SELECT id, rfc822 FROM messages WHERE rfc822 IS NOT NULL")?;
                let rows: Vec<(i64, Vec<u8>)> = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                let total = rows.len();
                for (id, rfc822) in rows {
                    let has = crate::attachments::has_attachments(&rfc822);
                    tx.execute(
                        "UPDATE messages SET has_attachments = ?1 WHERE id = ?2",
                        rusqlite::params![has as i32, id],
                    )?;
                }
                total
            };

            tx.execute_batch("PRAGMA user_version = 13;")?;
            tx.commit()?;

            crate::logger::info(&format!(
                "migration 13: has_attachments column {}, backfilled {} cached messages",
                if already_added {
                    "already present"
                } else {
                    "added"
                },
                backfilled
            ));
        }

        // Migration from version 13 to 14: record what each message is made of.
        //
        // BODYSTRUCTURE is now requested during the header sync, which gives the
        // part path of the readable text without downloading anything. Storing
        // it is what lets the body worker fetch that part alone, and lets it
        // batch by grouping messages that share a path — a FETCH applies one
        // part spec to every message in its range.
        //
        // Existing rows are left NULL and refill on their next sync.
        if from_version < 14 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            for column in [
                "text_part TEXT",
                "html_part TEXT",
                "text_encoding TEXT",
                "text_bytes INTEGER",
            ] {
                // No IF NOT EXISTS for ADD COLUMN; tolerate an interrupted run.
                let _ = tx.execute(&format!("ALTER TABLE messages ADD COLUMN {column}"), []);
            }
            // Finding work for the body fetcher is a hot query once the worker
            // runs: "messages in this folder with a text part and no body yet".
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_messages_body_pending
                     ON messages(folder_id, text_part) WHERE body_text IS NULL;
                 PRAGMA user_version = 14;",
            )?;
            tx.commit()?;
        }

        // Migration from version 14 to 15: rebuild the full-text index as
        // external content, with stemming and Cc indexed.
        //
        // Only the teardown lives here: `init_schema` recreates the index and
        // fills it once every migration has run, because an external-content
        // index names columns of `messages` and cannot be created before the
        // table has them.
        if from_version < 15 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "DROP TRIGGER IF EXISTS messages_fts_ai;
                 DROP TRIGGER IF EXISTS messages_fts_au;
                 DROP TRIGGER IF EXISTS messages_fts_ad;
                 DROP TRIGGER IF EXISTS message_bodies_ai;
                 DROP TRIGGER IF EXISTS message_bodies_au;
                 DROP TRIGGER IF EXISTS message_bodies_ad;
                 DROP TABLE IF EXISTS messages_fts;",
            )?;
            tx.execute_batch("PRAGMA user_version = 15;")?;
            tx.commit()?;
            crate::logger::info("migration 15: full-text index rebuilt as external content");
        }

        // Migration from version 15 to 16: remember which messages the server
        // described in a way that could not be parsed.
        //
        // Without this they are indistinguishable from messages that genuinely
        // have no text part, since both end up with a NULL `text_part`. Keeping
        // them apart is what makes it possible to come back for them later —
        // by fetching the full source and parsing it leniently — instead of
        // silently leaving them out of search forever.
        if from_version < 16 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            // No IF NOT EXISTS for ADD COLUMN; tolerate an interrupted run.
            let _ = tx.execute(
                "ALTER TABLE messages ADD COLUMN shape_unreadable INTEGER",
                [],
            );
            tx.execute_batch("PRAGMA user_version = 16;")?;
            tx.commit()?;
        }

        // Migration from version 16 to 17: remember when a folder's flags were
        // last checked against the server.
        //
        // Flags are normally learned from CONDSTORE, which depends on the
        // server advancing HIGHESTMODSEQ when one changes. A server here
        // advertises CONDSTORE and never advances it, so a message read
        // elsewhere stays unread here forever. The check that catches this is
        // affordable but not free, and this column is what keeps it from
        // running on every folder on every sync.
        if from_version < 17 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            // No IF NOT EXISTS for ADD COLUMN; tolerate an interrupted run.
            let _ = tx.execute("ALTER TABLE folders ADD COLUMN flags_checked_at TEXT", []);
            tx.execute_batch("PRAGMA user_version = 17;")?;
            tx.commit()?;
            crate::logger::info("migration 17: folders remember when flags were last checked");
        }

        // Migration from version 17 to 18: the task schema the task feature
        // actually needs — a stable identity, revisions, soft delete,
        // recurrence, tags, an append-only log, attachments that live in their
        // own messages — plus a Message-ID index on `messages`.
        //
        // This cannot be done with `ALTER TABLE`. The v8 `tasks` table declares
        // `message_id TEXT UNIQUE`, and SQLite cannot drop a constraint; that
        // constraint has to go, because the Message-ID is now the id of the
        // message a task is *currently* stored in and changes on every
        // APPEND-then-delete rewrite. So the v8 tables are renamed aside, the
        // v18 tables are created from `TASK_TABLES_SCHEMA`, the rows are copied
        // across, and the old tables are dropped.
        //
        // All five v8 tables are rebuilt, not only the two that change shape.
        // The v8 tables were declared twice — in migration 8 and in the
        // fresh-database batch — with texts that had drifted apart, so an
        // install that migrated and an install created fresh already carried
        // different `sqlite_master` entries for the same tables. Rebuilding
        // every one of them from a single constant makes the two identical
        // again, and keeps them that way.
        //
        // The renames run with foreign keys enabled and no `legacy_alter_table`
        // pragma, so SQLite repoints the child tables' `REFERENCES tasks(id)`
        // at `tasks_pre_v18` as it goes. That is exactly what is wanted: the
        // old rows stay consistent with the old parent while they are being
        // read, and the new child tables are created referencing the new one.
        // For the same reason the old children are dropped before the old
        // parent, or dropping the parent would trip their foreign keys.
        if from_version < 18 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;

            // A database that has no `tasks` table at all (one that never ran
            // migration 8) needs no rebuild, only the tables themselves. A
            // database whose `tasks` table already has `task_uuid` was rebuilt
            // by an interrupted earlier attempt at this migration; re-running
            // must not rebuild it a second time and hand out fresh identities.
            let rebuild = Database::table_exists(&tx, "tasks")?
                && !Database::column_exists(&tx, "tasks", "task_uuid")?;

            // The parent first, so that the children's `REFERENCES tasks(id)`
            // is repointed at the old table while it is still the one holding
            // their rows. Each step asks whether its table is there: migration
            // 8 created all five in one transaction, so they always arrive
            // together, but a migration that cannot cope with a database it did
            // not expect is a database that can never be opened again.
            const LEGACY_TASK_TABLES: [&str; 5] = [
                "tasks",
                "task_reminders",
                "task_linked_emails",
                "task_attachments",
                "task_sync_state",
            ];

            if rebuild {
                for table in LEGACY_TASK_TABLES {
                    if Database::table_exists(&tx, table)? {
                        tx.execute_batch(&format!(
                            "ALTER TABLE {table} RENAME TO {table}_pre_v18;"
                        ))?;
                    }
                }
            }

            tx.execute_batch(TASK_TABLES_SCHEMA)?;

            let mut migrated_tasks = 0usize;
            if rebuild {
                // Tasks first, one statement per row: each row needs its own
                // generated identity, and every child row's foreign key needs
                // its parent to be there already. Rows keep their `id`, so the
                // children can be copied verbatim.
                if Database::table_exists(&tx, "tasks_pre_v18")? {
                    let ids: Vec<i64> = {
                        let mut stmt = tx.prepare("SELECT id FROM tasks_pre_v18 ORDER BY id")?;
                        stmt.query_map([], |row| row.get(0))?
                            .collect::<rusqlite::Result<Vec<i64>>>()?
                    };
                    migrated_tasks = ids.len();
                    for id in ids {
                        // Revision 0 for every existing task: none of them has
                        // ever been on a server in the v2 wire format, so there
                        // is no higher revision anywhere for a 0 to lose to.
                        tx.execute(
                            "INSERT INTO tasks
                                (id, task_uuid, revision, subject, body_text, snooze_until,
                                 snooze_until_unix, deadline, deadline_unix, created_at,
                                 updated_at, completed_at, position, is_working_on, is_completed,
                                 is_dirty, account_name, imap_uid, message_id, modseq)
                             SELECT id, ?1, 0, subject, body_text, snooze_until,
                                 snooze_until_unix, deadline, deadline_unix, created_at,
                                 updated_at, completed_at, position, is_working_on, is_completed,
                                 is_dirty, account_name, imap_uid, message_id, modseq
                             FROM tasks_pre_v18 WHERE id = ?2",
                            params![uuid::Uuid::new_v4().to_string(), id],
                        )?;
                    }
                }

                if Database::table_exists(&tx, "task_reminders_pre_v18")? {
                    tx.execute_batch(
                        "INSERT INTO task_reminders
                             (id, task_id, remind_at, remind_at_unix, is_triggered)
                         SELECT id, task_id, remind_at, remind_at_unix, is_triggered
                         FROM task_reminders_pre_v18;",
                    )?;
                }

                if Database::table_exists(&tx, "task_linked_emails_pre_v18")? {
                    tx.execute_batch(
                        "INSERT INTO task_linked_emails
                             (id, task_id, message_id, cached_subject, cached_from, cached_date,
                              cached_account, cached_folder)
                         SELECT id, task_id, message_id, cached_subject, cached_from, cached_date,
                              cached_account, cached_folder
                         FROM task_linked_emails_pre_v18;",
                    )?;
                }

                if Database::table_exists(&tx, "task_sync_state_pre_v18")? {
                    tx.execute_batch(
                        "INSERT INTO task_sync_state
                             (id, account_name, folder_name, uidvalidity, highest_modseq,
                              last_sync_time)
                         SELECT id, account_name, folder_name, uidvalidity, highest_modseq,
                              last_sync_time
                         FROM task_sync_state_pre_v18;",
                    )?;
                }

                // Attachments, like tasks, need a generated identity each.
                // Their server columns start empty and `is_cached` at 0: a
                // pre-v18 attachment has no message of its own on the server
                // yet, so it is metadata pointing at a local file, and the
                // first sync that touches its task is what gives it one.
                if Database::table_exists(&tx, "task_attachments_pre_v18")? {
                    let ids: Vec<i64> = {
                        let mut stmt =
                            tx.prepare("SELECT id FROM task_attachments_pre_v18 ORDER BY id")?;
                        stmt.query_map([], |row| row.get(0))?
                            .collect::<rusqlite::Result<Vec<i64>>>()?
                    };
                    for id in ids {
                        tx.execute(
                            "INSERT INTO task_attachments
                                (id, task_id, attachment_uuid, filename, content_type, size,
                                 file_path, is_cached)
                             SELECT id, task_id, ?1, filename, content_type, size, file_path, 0
                             FROM task_attachments_pre_v18 WHERE id = ?2",
                            params![uuid::Uuid::new_v4().to_string(), id],
                        )?;
                    }
                }

                // Children before the parent, or the implicit delete a DROP
                // performs trips their foreign keys. And only now: dropping a
                // table takes its indexes with it, which is what frees the v8
                // index names for the v18 definitions below.
                for table in LEGACY_TASK_TABLES.iter().rev() {
                    if Database::table_exists(&tx, &format!("{table}_pre_v18"))? {
                        tx.execute_batch(&format!("DROP TABLE {table}_pre_v18;"))?;
                    }
                }
            }

            tx.execute_batch(TASK_INDEXES_SCHEMA)?;

            // Resolving a task's link back to its message starts with an RFC
            // Message-ID lookup in the local store, and there was no index for
            // one: a message linked years ago, and moved between folders since,
            // would otherwise cost a scan of every message in the database.
            //
            // Guarded because a migration that cannot run is a database that
            // cannot be opened, and `messages` is old enough that a database
            // predating some of its columns is conceivable. Every real one has
            // `message_id` — it is a v0 column — so the guard costs nothing.
            if Database::column_exists(&tx, "messages", "message_id")? {
                tx.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_messages_message_id ON messages(message_id);",
                )?;
            }
            tx.execute_batch("PRAGMA user_version = 18;")?;
            tx.commit()?;

            crate::logger::info(&format!(
                "migration 18: task tables rebuilt with stable identities ({} existing {} \
                 given one), messages indexed by Message-ID",
                migrated_tasks,
                if migrated_tasks == 1 { "task" } else { "tasks" }
            ));
        }

        // Migration from version 18 to 19: the outbox of server writes made
        // locally first. Idempotent, like the other table additions.
        if from_version < 19 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(&format!(
                "{}
                 PRAGMA user_version = 19;",
                OUTBOX_SCHEMA
            ))?;
            tx.commit()?;
        }

        // Migration from version 19 to 20: filter rules. A folder's
        // `last_filtered_uid` is the highest UID its rules have seen; NULL
        // until the folder's first complete sync sets it, so that mail already
        // there is never filtered.
        if from_version < 20 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            // No IF NOT EXISTS for ADD COLUMN; tolerate an interrupted run.
            let _ = tx.execute(
                "ALTER TABLE folders ADD COLUMN last_filtered_uid INTEGER",
                [],
            );
            tx.execute_batch(&format!(
                "{}
                 PRAGMA user_version = 20;",
                FILTER_LOG_SCHEMA
            ))?;
            tx.commit()?;
        }

        // Migration from version 20 to 21: conversations. The headers a thread
        // is built from, the thread each message belongs to, and the subject
        // as threads compare it. `headers_synced` is NULL for a message stored
        // before the sync asked for the headers, which the backfill fills in.
        if from_version < 21 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            for column in [
                "in_reply_to TEXT",
                "references_hdr TEXT",
                "list_id TEXT",
                "thread_id TEXT",
                "norm_subject TEXT",
                "subject_reply INTEGER",
                "headers_synced INTEGER",
            ] {
                // No IF NOT EXISTS for ADD COLUMN; tolerate an interrupted run.
                let _ = tx.execute(&format!("ALTER TABLE messages ADD COLUMN {column}"), []);
            }
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_messages_folder_thread
                     ON messages(folder_id, thread_id);
                 CREATE INDEX IF NOT EXISTS idx_messages_folder_reply
                     ON messages(folder_id, in_reply_to);
                 CREATE INDEX IF NOT EXISTS idx_messages_folder_subject
                     ON messages(folder_id, norm_subject);
                 PRAGMA user_version = 21;",
            )?;
            Self::fill_subject_columns(&tx)?;
            tx.commit()?;
        }

        // Migration from version 21 to 22: what the server's LIST said, so
        // that the folders it has and the roles it gives them (RFC 6154) are
        // known without asking again.
        if from_version < 22 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(&format!(
                "{}
                 PRAGMA user_version = 22;",
                SERVER_FOLDERS_SCHEMA
            ))?;
            tx.commit()?;
        }

        // Migration from version 22 to 23: the headers the highlight rules
        // name, fetched beside the envelope, as a JSON list of pairs.
        if from_version < 23 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            // No IF NOT EXISTS for ADD COLUMN; tolerate an interrupted run.
            let _ = tx.execute("ALTER TABLE messages ADD COLUMN extra_headers TEXT", []);
            tx.execute_batch("PRAGMA user_version = 23;")?;
            tx.commit()?;
        }

        // Migration from version 23 to 24: every message is in a thread —
        // its own, for one stored before threads existed — so the thread
        // queries read the column and its index rather than an expression;
        // the subject columns a database that reached 21 without them; and
        // the indexes a conversation and a threaded page are read by.
        if from_version < 24 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute(
                "UPDATE messages SET thread_id = 'local:' || id WHERE thread_id IS NULL",
                [],
            )?;
            Self::fill_subject_columns(&tx)?;
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_messages_folder_thread_date
                     ON messages(folder_id, thread_id, date_unix);
                 CREATE INDEX IF NOT EXISTS idx_messages_in_reply_to
                     ON messages(in_reply_to);
                 PRAGMA user_version = 24;",
            )?;
            tx.commit()?;
        }

        // Migration from version 24 to 25: a folder's messages by Message-ID,
        // which threading looks up for every message it stores (see
        // `assign_thread`).
        // Guarded like migration 18's index on the same column.
        if from_version < 25 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if Database::column_exists(&tx, "messages", "message_id")? {
                tx.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_messages_folder_message
                         ON messages(folder_id, message_id);",
                )?;
            }
            tx.execute_batch("PRAGMA user_version = 25;")?;
            tx.commit()?;
        }

        // Migration from version 25 to 26: the outbox takes the moves and
        // deletes of applied staged actions. SQLite cannot alter a CHECK, so
        // the table is rebuilt with what it holds.
        if from_version < 26 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "CREATE TABLE outbox_v26 (
                     id INTEGER PRIMARY KEY,
                     account TEXT NOT NULL,
                     kind TEXT NOT NULL CHECK (kind IN ('flags', 'send', 'move')),
                     payload TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     attempts INTEGER NOT NULL DEFAULT 0,
                     last_error TEXT
                 );
                 INSERT INTO outbox_v26
                     SELECT id, account, kind, payload, created_at, attempts, last_error
                     FROM outbox;
                 DROP TABLE outbox;
                 ALTER TABLE outbox_v26 RENAME TO outbox;
                 CREATE INDEX IF NOT EXISTS idx_outbox_account ON outbox(account, id);
                 PRAGMA user_version = 26;",
            )?;
            tx.commit()?;
        }

        // Migration from version 26 to 27: the hierarchy delimiter LIST gives
        // each folder, so the sidebar can nest them. A row from before it has
        // none until the next LIST, and is drawn flat until then.
        if from_version < 27 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if !Database::column_exists(&tx, "server_folders", "delimiter")? {
                tx.execute_batch("ALTER TABLE server_folders ADD COLUMN delimiter TEXT;")?;
            }
            tx.execute_batch("PRAGMA user_version = 27;")?;
            tx.commit()?;
        }

        // Migration from version 27 to 28: unread counts from an index. A
        // row holds the bodies and the source before `flags`, so reading the
        // flags of a row walks the whole of it: counting the unread mail of a
        // 200 000-message folder read the folder's bodies, and the Dock badge
        // read the whole mailbox's on every apply and every sync. Building the
        // index reads every row once, which is why it is logged.
        if from_version < 28 {
            let started = std::time::Instant::now();
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_messages_folder_flags ON messages(folder_id, flags);
                 PRAGMA user_version = 28;",
            )?;
            tx.commit()?;
            crate::logger::info(&format!(
                "migration 28: unread counts indexed in {:.1}s",
                started.elapsed().as_secs_f64()
            ));
        }

        // Migration from version 28 to 29: a task's steps (todo-spec §17). A
        // database without the task tables yet gets the column when they are
        // made.
        if from_version < 29 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if Database::table_exists(&tx, "tasks")?
                && !Database::column_exists(&tx, "tasks", "steps")?
            {
                tx.execute_batch("ALTER TABLE tasks ADD COLUMN steps TEXT;")?;
            }
            tx.execute_batch("PRAGMA user_version = 29;")?;
            tx.commit()?;
        }

        // Migration from version 29 to 30: the bodies and the source out of the
        // message row, into `message_bodies`.
        //
        // They sat before `flags`, `size`, `modseq` and `date_unix`, so reading
        // any of those walked the row's whole overflow chain: a page in the
        // middle of a 200 000-message folder took 20 s. The copy runs in
        // batches, each its own transaction, and resumes where it stopped; the
        // full-text index is dropped first and rebuilt over the new table once
        // the schema is in place, by `init_schema`.
        if from_version < 30 {
            self.move_bodies_out()?;
        }

        // Migration from version 30 to 31: meeting requests' invitations. The
        // text/calendar alternative of one was taken for body text, so an
        // invitation synced before was recorded as having no attachments,
        // and its source is never fetched for them. Forgetting the shape of
        // the recent mail that could be one — plain and HTML text and nothing
        // attached — has the shape backfill ask the server again, for the
        // structure alone. Older invitations are of meetings that are over.
        if from_version < 31 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let reset = tx.execute(
                "UPDATE messages SET has_attachments = NULL
                 WHERE has_attachments = 0 AND shape_unreadable IS NULL
                   AND text_part IS NOT NULL AND html_part IS NOT NULL
                   AND date_unix > CAST(strftime('%s', 'now', '-90 days') AS INTEGER)",
                [],
            )?;
            tx.execute_batch("PRAGMA user_version = 31;")?;
            tx.commit()?;
            crate::logger::info(&format!(
                "migration 31: {reset} recent messages to be shaped again for invitations"
            ));
        }

        if from_version < 32 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(FEED_TABLES_SCHEMA)?;
            tx.execute_batch("PRAGMA user_version = 32;")?;
            tx.commit()?;
        }

        if from_version < 33 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(CHAT_SCHEMA)?;
            tx.execute_batch("PRAGMA user_version = 33;")?;
            tx.commit()?;
        }

        if from_version < 34 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if !Self::column_exists(&tx, "chat_messages", "quotes")? {
                tx.execute_batch(CHAT_QUOTES_SCHEMA)?;
            }
            tx.execute_batch("PRAGMA user_version = 34;")?;
            tx.commit()?;
        }

        if from_version < 35 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if !Self::column_exists(&tx, "outbox", "failed_at")? {
                tx.execute_batch(OUTBOX_FAILED_SCHEMA)?;
            }
            tx.execute_batch("PRAGMA user_version = 35;")?;
            tx.commit()?;
        }

        if from_version < 36 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if Self::column_exists(&tx, "chat_rooms", "synced_until")? {
                tx.execute_batch(CHAT_REFETCH_SCHEMA)?;
            }
            tx.execute_batch("PRAGMA user_version = 36;")?;
            tx.commit()?;
        }

        if from_version < 37 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if Self::column_exists(&tx, "chat_messages", "id")?
                && !Self::column_exists(&tx, "chat_messages", "mentions_all")?
            {
                tx.execute_batch(CHAT_MENTIONS_ALL_SCHEMA)?;
            }
            tx.execute_batch("PRAGMA user_version = 37;")?;
            tx.commit()?;
        }

        if from_version < 38 {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(FOCUS_SCHEMA)?;
            tx.execute_batch("PRAGMA user_version = 38;")?;
            tx.commit()?;
        }

        Ok(())
    }

    /// Migration 30; see `run_migrations`.
    fn move_bodies_out(&self) -> Result<()> {
        const BATCH: i64 = 2_000;
        let started = std::time::Instant::now();

        // The columns cannot be dropped while an index or a trigger names them.
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS message_bodies (
                 message_id INTEGER PRIMARY KEY,
                 body_text TEXT,
                 body_html TEXT,
                 rfc822 BLOB
             );
             DROP TRIGGER IF EXISTS messages_fts_ai;
             DROP TRIGGER IF EXISTS messages_fts_au;
             DROP TRIGGER IF EXISTS messages_fts_ad;
             DROP TRIGGER IF EXISTS message_bodies_ai;
             DROP TRIGGER IF EXISTS message_bodies_au;
             DROP TRIGGER IF EXISTS message_bodies_ad;
             DROP TABLE IF EXISTS messages_fts;
             DROP INDEX IF EXISTS idx_messages_body_pending;",
        )?;
        if !Database::column_exists(&tx, "messages", "has_body_text")? {
            tx.execute_batch("ALTER TABLE messages ADD COLUMN has_body_text INTEGER;")?;
        }
        tx.commit()?;

        // A database old enough may lack some of the three; what it lacks it
        // has nothing of to move.
        let mut present = Vec::new();
        for column in ["body_text", "body_html", "rfc822"] {
            if Database::column_exists(&self.conn, "messages", column)? {
                present.push(column);
            }
        }
        let column_or_null = |column: &str| {
            if present.contains(&column) {
                column.to_string()
            } else {
                "NULL".to_string()
            }
        };
        let any_body = present
            .iter()
            .map(|column| format!("{column} IS NOT NULL"))
            .collect::<Vec<_>>()
            .join(" OR ");
        if !present.is_empty() {
            let mut moved = 0usize;
            let mut batches = 0usize;
            let mut after = 0i64;
            loop {
                let tx = rusqlite::Transaction::new_unchecked(
                    &self.conn,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                let last: Option<i64> = tx.query_row(
                    &format!(
                        "SELECT MAX(id) FROM (
                             SELECT id FROM messages WHERE id > ?1 AND ({any_body})
                             ORDER BY id LIMIT ?2)"
                    ),
                    params![after, BATCH],
                    |row| row.get(0),
                )?;
                let Some(last) = last else {
                    tx.commit()?;
                    break;
                };
                moved += tx.execute(
                    &format!(
                        "INSERT INTO message_bodies (message_id, body_text, body_html, rfc822)
                         SELECT id, {}, {}, {} FROM messages
                         WHERE id > ?1 AND id <= ?2 AND ({any_body})
                         ON CONFLICT(message_id) DO UPDATE SET
                             body_text = excluded.body_text,
                             body_html = excluded.body_html,
                             rfc822 = excluded.rfc822",
                        column_or_null("body_text"),
                        column_or_null("body_html"),
                        column_or_null("rfc822"),
                    ),
                    params![after, last],
                )?;
                let cleared = present
                    .iter()
                    .map(|column| format!("{column} = NULL"))
                    .collect::<Vec<_>>()
                    .join(", ");
                tx.execute(
                    &format!(
                        "UPDATE messages
                         SET has_body_text = CASE WHEN {} IS NULL THEN NULL ELSE 1 END,
                             {cleared}
                         WHERE id > ?1 AND id <= ?2",
                        column_or_null("body_text"),
                    ),
                    params![after, last],
                )?;
                tx.commit()?;
                after = last;
                batches += 1;
                if batches.is_multiple_of(25) {
                    crate::logger::info(&format!(
                        "migration 30: {moved} bodies moved so far, {:.0}s",
                        started.elapsed().as_secs_f64()
                    ));
                }
            }
            crate::logger::info(&format!(
                "migration 30: {moved} bodies moved in {:.1}s",
                started.elapsed().as_secs_f64()
            ));
        }

        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        for column in ["body_text", "body_html", "rfc822"] {
            if Database::column_exists(&tx, "messages", column)? {
                tx.execute_batch(&format!("ALTER TABLE messages DROP COLUMN {column};"))?;
            }
        }
        tx.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_messages_body_pending
                 ON messages(folder_id, text_part) WHERE has_body_text IS NULL;
             PRAGMA user_version = 30;",
        )?;
        tx.commit()?;
        crate::logger::info(&format!(
            "migration 30: bodies kept apart from the message rows, {:.1}s; the search index \
             is rebuilt next",
            started.elapsed().as_secs_f64()
        ));
        Ok(())
    }

    /// Give every message without them the subject as threads compare it,
    /// and whether it is a reply by that subject.
    fn fill_subject_columns(tx: &rusqlite::Transaction) -> Result<()> {
        let rows: Vec<(i64, Option<String>)> = {
            let mut stmt =
                tx.prepare("SELECT id, subject FROM messages WHERE norm_subject IS NULL")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut update =
            tx.prepare("UPDATE messages SET norm_subject = ?1, subject_reply = ?2 WHERE id = ?3")?;
        for (id, subject) in rows {
            let (norm, reply) = crate::threads::normalise_subject(subject.as_deref());
            update.execute(params![norm, reply, id])?;
        }
        Ok(())
    }

    /// Normalize date string to RFC3339 format (used by migration)
    /// Uses mailparse::dateparse which handles real-world email date quirks
    fn normalize_date_for_migration(date_str: &str) -> Option<String> {
        use chrono::{TimeZone, Utc};

        // Try RFC 3339 first (already normalized)
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

    /// Parse date string to Unix timestamp for sorting
    /// Handles both RFC 3339/ISO 8601 and RFC 2822 email date formats
    pub fn parse_date_to_unix(date_str: &str) -> Option<i64> {
        // Try RFC 3339 first
        if let Ok(dt) = DateTime::parse_from_rfc3339(date_str) {
            return Some(dt.timestamp());
        }

        // Use mailparse's dateparse which handles email date quirks
        if let Ok(timestamp) = mailparse::dateparse(date_str) {
            return Some(timestamp);
        }

        None
    }

    pub fn init_schema(&self) -> Result<()> {
        let current_version = self.get_schema_version()?;
        // A database a newer Orb wrote has tables and columns this build
        // does not know, and a sync that wrote around them would leave it
        // unreadable to the Orb that made it. Refuse, and say so.
        if current_version > SCHEMA_VERSION {
            return Err(crate::Error::InvalidConfig {
                field: "db_path".to_string(),
                reason: format!(
                    "the database is from a newer Orb (schema {current_version}; this build \
                     knows {SCHEMA_VERSION}). Use that Orb, or point db_path at a new file."
                ),
            });
        }
        self.conn.execute_batch(CORE_SCHEMA)?;

        // Initialize sync_version if needed
        self.conn.execute(
            "INSERT OR IGNORE INTO sync_meta (key, value) VALUES ('sync_version', 0)",
            [],
        )?;

        // Run migrations if database already existed with older schema
        if current_version < SCHEMA_VERSION {
            self.run_migrations(current_version)?;
            self.set_schema_version(SCHEMA_VERSION)?;
        }

        // The task tables come after the migrations, and are the definition of
        // their shape rather than a copy of it: migration 18 builds exactly
        // this out of the v8 tables, so a database that upgraded into it and
        // one created in it are the same schema, not two schemas that happen to
        // agree. On any database that has run migration 18 every statement here
        // is a no-op; what it buys is that there is only one text.
        self.conn.execute_batch(TASK_TABLES_SCHEMA)?;
        self.conn.execute_batch(TASK_INDEXES_SCHEMA)?;
        self.conn.execute_batch(FEED_TABLES_SCHEMA)?;

        // The full-text index goes last. It is external-content, so it names
        // columns of `messages` and cannot be created until every migration
        // that adds one has run. Migration 15 drops the previous index, so on
        // an upgrade this recreates it; on a new database it is the only
        // creation. Either way, an index that did not exist a moment ago is
        // empty and needs filling from the content table.
        let existed: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages_fts')",
            [],
            |row| row.get(0),
        )?;
        if existed {
            self.conn.execute_batch(FTS_SCHEMA)?;
        } else {
            // Made and filled in one transaction: an index left empty by a
            // rebuild that failed would be taken for a full one at the next
            // start, and nothing would be found. 'rebuild' is the documented
            // way to fill an external-content index and is far faster than
            // inserting row by row.
            let started = std::time::Instant::now();
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(FTS_SCHEMA)?;
            tx.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');")?;
            tx.commit()?;
            if started.elapsed() > std::time::Duration::from_secs(1) {
                crate::logger::info(&format!(
                    "search index rebuilt in {:.1}s",
                    started.elapsed().as_secs_f64()
                ));
            }
        }

        // The vector index, likewise last, and likewise a virtual table that
        // has to be created rather than migrated. Unlike the full-text index it
        // is allowed to fail: it needs an extension that may not have
        // registered, and semantic search is the only thing that depends on it.
        if let Err(error) = self.conn.execute_batch(VECTOR_SCHEMA) {
            crate::logger::warn(&format!(
                "vector index unavailable ({error}); semantic search is off and exact search \
                 is unaffected"
            ));
        }

        Ok(())
    }

    /// Whether the vector index exists, and so whether ranking by meaning can
    /// be attempted at all.
    pub fn has_vector_index(&self) -> bool {
        Database::table_exists(&self.conn, "message_vectors").unwrap_or(false)
    }

    /// Execute a function within an IMMEDIATE transaction
    /// This is the foundation for optimistic concurrency control
    pub fn transaction<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T>,
    {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        match f(&tx) {
            Ok(result) => {
                tx.commit()?;
                Ok(result)
            }
            Err(e) => {
                // Rollback happens automatically when tx is dropped
                Err(e)
            }
        }
    }

    /// Begin an IMMEDIATE transaction from a shared reference.
    ///
    /// Unlike [`Database::transaction`], this does not require `&mut self`, which
    /// lets callers that only hold a shared `&Database` (e.g. the task sync
    /// service, whose repository methods all take `&self` and run their writes on
    /// `self.conn`) group several statements into a single atomic unit. Because
    /// SQLite serialises writes per connection, statements executed on `self.conn`
    /// while the returned transaction is alive run inside it; dropping the
    /// transaction without calling `commit` rolls the whole group back.
    ///
    /// Uses `BEGIN IMMEDIATE` so the write lock is taken up front, matching the
    /// discipline of [`Database::transaction`] and preventing a concurrent writer
    /// from interleaving mid-sequence.
    pub(crate) fn immediate_transaction(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?)
    }

    // Your task: implement these
    pub fn insert_folder(&self, account_name: &str, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO folders (account_name, name) VALUES (?1, ?2)",
            params![account_name, name],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Get or create a folder atomically
    ///
    /// Uses INSERT OR IGNORE to avoid race conditions when multiple instances
    /// try to create the same folder. Returns the folder ID whether it was
    /// newly created or already existed.
    pub fn get_or_create_folder(&mut self, account_name: &str, name: &str) -> Result<i64> {
        self.transaction(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO folders (account_name, name) VALUES (?1, ?2)",
                params![account_name, name],
            )?;

            let id: i64 = tx.query_row(
                "SELECT id FROM folders WHERE account_name = ?1 AND name = ?2",
                params![account_name, name],
                |row| row.get(0),
            )?;

            Ok(id)
        })
    }

    pub fn get_folder_by_name(&self, account_name: &str, name: &str) -> Result<Option<Folder>> {
        let result = self.conn.query_row(
            "SELECT id, account_name, name, uidvalidity, highest_uid, highest_modseq, last_sync_time, sync_version
             FROM folders
             WHERE account_name = ?1 AND name = ?2",
            params![account_name, name],
            |row| {
                Ok(Folder {
                    id: Some(row.get(0)?),
                    account_name: row.get(1)?,
                    name: row.get(2)?,
                    uidvalidity: row.get(3)?,
                    highest_uid: row.get(4)?,
                    highest_modseq: row.get(5)?,
                    last_sync_time: row.get(6)?,
                    sync_version: row.get(7)?,
                })
            },
        );

        match result {
            Ok(folder) => Ok(Some(folder)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get a folder by its database ID
    pub fn get_folder_by_id(&self, folder_id: i64) -> Result<Option<Folder>> {
        let result = self.conn.query_row(
            "SELECT id, account_name, name, uidvalidity, highest_uid, highest_modseq, last_sync_time, sync_version
             FROM folders
             WHERE id = ?1",
            params![folder_id],
            |row| {
                Ok(Folder {
                    id: Some(row.get(0)?),
                    account_name: row.get(1)?,
                    name: row.get(2)?,
                    uidvalidity: row.get(3)?,
                    highest_uid: row.get(4)?,
                    highest_modseq: row.get(5)?,
                    last_sync_time: row.get(6)?,
                    sync_version: row.get(7)?,
                })
            },
        );

        match result {
            Ok(folder) => Ok(Some(folder)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all folders for an account
    ///
    /// Returns all folders that have been synced for the specified account,
    /// ordered by name.
    pub fn get_folders_by_account(&self, account_name: &str) -> Result<Vec<Folder>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account_name, name, uidvalidity, highest_uid, highest_modseq, last_sync_time, sync_version
             FROM folders
             WHERE account_name = ?1
             ORDER BY name ASC",
        )?;

        let rows = stmt.query_map(params![account_name], |row| {
            Ok(Folder {
                id: Some(row.get(0)?),
                account_name: row.get(1)?,
                name: row.get(2)?,
                uidvalidity: row.get(3)?,
                highest_uid: row.get(4)?,
                highest_modseq: row.get(5)?,
                last_sync_time: row.get(6)?,
                sync_version: row.get(7)?,
            })
        })?;

        let folders: Vec<Folder> = rows.collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(folders)
    }

    /// Prune folder rows (and their messages) whose account is no longer in the
    /// config.
    ///
    /// A folder is considered orphaned and removed when its `account_name` is not
    /// present in `valid_accounts` (the account was removed or renamed in the
    /// config). This is what fixes the "duplicate folder display" bug after an
    /// account rename.
    ///
    /// Folders that merely have `sync = false` in the config are intentionally
    /// NOT pruned: `sync = false` is a persistent "show but don't auto-sync"
    /// preference (orthogonal to `visible`), not a removal. Deleting such a folder
    /// would destroy its cached messages permanently — sync never re-creates a
    /// `sync = false` folder — so its row and messages are preserved.
    ///
    /// The scan and all deletes run inside a single IMMEDIATE transaction so a
    /// crash can never leave a half-pruned database. Deleting a folder cascades
    /// to its messages because `messages.folder_id` is declared
    /// `ON DELETE CASCADE` (foreign keys are enabled per-connection at open).
    ///
    /// Note this keys off the *config*, never runtime/selection state: a folder
    /// that is simply not currently selected is never removed, only folders whose
    /// account is gone.
    pub fn prune_orphaned_folders(
        &mut self,
        valid_accounts: &HashSet<String>,
    ) -> Result<ReconcileReport> {
        self.transaction(|tx| {
            // Collect orphaned folder ids first so we don't mutate while the
            // read statement is still borrowing the connection.
            let to_delete: Vec<i64> = {
                let mut stmt = tx.prepare("SELECT id, account_name FROM folders")?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?;

                let mut ids = Vec::new();
                for row in rows {
                    let (id, account) = row?;
                    if !valid_accounts.contains(&account) {
                        ids.push(id);
                    }
                }
                ids
            };

            let mut report = ReconcileReport::default();
            for id in to_delete {
                // Count messages before the cascade removes them (for reporting).
                let msg_count: usize = tx.query_row(
                    "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
                    params![id],
                    |r| r.get(0),
                )?;
                tx.execute("DELETE FROM folders WHERE id = ?1", params![id])?;
                report.folders_removed += 1;
                report.messages_removed += msg_count;
            }

            Ok(report)
        })
    }

    /// Get message counts for a folder (unread_count, total_count)
    pub fn get_folder_counts(&self, folder_id: i64) -> Result<(u32, u32)> {
        let total: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
            params![folder_id],
            |row| row.get(0),
        )?;

        // Flags are stored as a JSON array in wire form, `["\\Seen"]`, and
        // were comma-separated before that; `%Seen%` reads both.
        let unread: u32 = self.conn.query_row(
            r#"SELECT COUNT(*) FROM messages
               WHERE folder_id = ?1
               AND (flags IS NULL OR flags NOT LIKE '%Seen%')"#,
            params![folder_id],
            |row| row.get(0),
        )?;

        Ok((unread, total))
    }

    /// Update folder's sync state after successful sync
    pub fn update_folder_sync_state(
        &self,
        folder_id: i64,
        uidvalidity: u32,
        highest_uid: u32,
        highest_modseq: u64,
        sync_time: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE folders
             SET uidvalidity = ?1,
                 highest_uid = ?2,
                 highest_modseq = ?3,
                 last_sync_time = ?4
             WHERE id = ?5",
            params![
                uidvalidity,
                highest_uid,
                highest_modseq,
                sync_time,
                folder_id
            ],
        )?;
        Ok(())
    }

    pub fn insert_message(&self, msg: &Message) -> Result<i64> {
        // Store flags as JSON array for proper handling of special characters
        let flags_str = serde_json::to_string(&msg.flags).unwrap_or_else(|_| "[]".to_string());

        // Parse date to Unix timestamp for reliable sorting
        let date_unix = msg.date.as_ref().and_then(|d| Self::parse_date_to_unix(d));

        let (norm_subject, subject_reply) =
            crate::threads::normalise_subject(msg.subject.as_deref());
        let headers = msg.headers.clone().unwrap_or_default();
        self.conn.execute(
            "INSERT INTO messages
                (folder_id, uid, message_id, subject, from_addr, to_addr, cc_addr,
                 date, date_unix, size, flags, modseq,
                 in_reply_to, references_hdr, list_id, norm_subject, subject_reply,
                 headers_synced, extra_headers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                msg.folder_id,
                msg.uid,
                msg.message_id,
                msg.subject,
                msg.from,
                msg.to,
                msg.cc,
                msg.date,
                date_unix,
                msg.size,
                flags_str,
                msg.modseq,
                headers.in_reply_to,
                headers.references,
                headers.list_id,
                norm_subject,
                subject_reply,
                msg.headers.as_ref().map(|_| 1),
                extra_headers_json(&headers),
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        Self::store_body(&self.conn, id, msg)?;
        Self::assign_thread(&self.conn, msg.folder_id, id)?;
        Ok(id)
    }

    /// Keep what `msg` carries of its body, source and text with message `id`,
    /// leaving what it does not carry as it was.
    fn store_body(conn: &rusqlite::Connection, id: i64, msg: &Message) -> Result<()> {
        if msg.body_text.is_none() && msg.body_html.is_none() && msg.rfc822.is_none() {
            return Ok(());
        }
        // An upsert, never a REPLACE: a REPLACE deletes without the delete
        // trigger, and the search index would keep the old text.
        conn.execute(
            "INSERT INTO message_bodies (message_id, body_text, body_html, rfc822)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(message_id) DO UPDATE SET
                 body_text = COALESCE(excluded.body_text, message_bodies.body_text),
                 body_html = COALESCE(excluded.body_html, message_bodies.body_html),
                 rfc822 = COALESCE(excluded.rfc822, message_bodies.rfc822)",
            params![id, msg.body_text, msg.body_html, msg.rfc822],
        )?;
        Ok(())
    }

    /// Put a stored message into its thread, and pull into that thread
    /// whatever already answered it.
    ///
    /// The thread is the one of the nearest message here that this one
    /// answers — by `In-Reply-To`, then by `References` from the end — or,
    /// for a reply by its subject that answers nothing here, that of the
    /// oldest message with the same subject; otherwise a thread of its own.
    /// Then every message that names this one as its parent, and every reply
    /// by subject that founded a thread of its own for want of this one,
    /// joins the thread: threads are relabelled whole, so a child stored
    /// before its parent — the first sync pages newest first — ends up right.
    ///
    /// Runs for every message stored, so each lookup names its index: left
    /// to choose, SQLite took the `thread_id` conditions for a reason to walk
    /// the folder's threads, and a cold sync became quadratic in the folder's
    /// size — 30 000 messages took 90 s, a 205 000-message archive hours.
    pub(crate) fn assign_thread(conn: &Connection, folder_id: i64, id: i64) -> Result<()> {
        /// What threading reads back of a stored message.
        type Stored = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<bool>,
            Option<String>,
        );
        let (message_id, in_reply_to, references, norm_subject, subject_reply, old_thread): Stored =
            conn.query_row(
                "SELECT message_id, in_reply_to, references_hdr, norm_subject, subject_reply,
                        thread_id
                 FROM messages WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?;
        let message_id = message_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        let headers = crate::threads::ThreadHeaders {
            in_reply_to,
            references,
            list_id: None,
            extra: Vec::new(),
        };

        let mut thread: Option<String> = None;
        for parent in headers.parents() {
            if let Some(found) = conn
                .query_row(
                    "SELECT thread_id FROM messages INDEXED BY idx_messages_folder_message
                     WHERE folder_id = ?1 AND message_id = ?2 AND thread_id IS NOT NULL AND id != ?3
                     LIMIT 1",
                    params![folder_id, parent, id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                thread = Some(found);
                break;
            }
        }
        if thread.is_none()
            && subject_reply.unwrap_or(false)
            && let Some(subject) = norm_subject.as_deref().filter(|s| !s.is_empty())
        {
            thread = conn
                .query_row(
                    "SELECT thread_id FROM messages INDEXED BY idx_messages_folder_subject
                     WHERE folder_id = ?1 AND norm_subject = ?2 AND thread_id IS NOT NULL AND id != ?3
                     ORDER BY date_unix ASC NULLS LAST, id ASC LIMIT 1",
                    params![folder_id, subject, id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
        }
        let own = crate::threads::own_thread_id(id, message_id.as_deref());
        let thread = thread.unwrap_or_else(|| own.clone());
        // A message assigned again — the backfill brought its headers — takes
        // the thread it founded along, replies and all; a message that had
        // merely joined one moves alone.
        match old_thread {
            Some(old) if old != thread && old == own => {
                conn.execute(
                    "UPDATE messages SET thread_id = ?1 WHERE folder_id = ?2 AND thread_id = ?3",
                    params![thread, folder_id, old],
                )?;
            }
            _ => {
                conn.execute(
                    "UPDATE messages SET thread_id = ?1 WHERE id = ?2",
                    params![thread, id],
                )?;
            }
        }

        // What answered this message before it arrived. A reply that founded
        // a thread of its own for want of this one brings that thread whole;
        // a reply that had joined some other conversation by its subject
        // comes over alone — relabelling its thread would drag an unrelated
        // conversation along.
        let Some(message_id) = message_id else {
            return Ok(());
        };
        conn.execute(
            "UPDATE messages INDEXED BY idx_messages_folder_reply SET thread_id = ?1
             WHERE folder_id = ?2 AND in_reply_to = ?3 AND thread_id != ?1
               AND thread_id != message_id AND thread_id != 'local:' || id",
            params![thread, folder_id, message_id],
        )?;
        let mut orphaned: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT thread_id FROM messages INDEXED BY idx_messages_folder_reply
                 WHERE folder_id = ?1 AND in_reply_to = ?2 AND thread_id != ?3",
            )?;
            let rows = stmt.query_map(params![folder_id, message_id, thread], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if !subject_reply.unwrap_or(false)
            && let Some(subject) = norm_subject.as_deref().filter(|s| !s.is_empty())
        {
            // Replies by subject that founded threads of their own, for want
            // of this one: a thread whose name is its founder's own id.
            let mut stmt = conn.prepare(
                "SELECT DISTINCT m.thread_id FROM messages m INDEXED BY idx_messages_folder_subject
                 WHERE m.folder_id = ?1 AND m.norm_subject = ?2 AND m.subject_reply = 1
                   AND m.thread_id IS NOT NULL AND m.thread_id != ?3
                   AND (m.thread_id = m.message_id OR m.thread_id = 'local:' || m.id)",
            )?;
            let rows = stmt.query_map(params![folder_id, subject, thread], |row| row.get(0))?;
            orphaned.extend(rows.collect::<rusqlite::Result<Vec<String>>>()?);
        }
        for old in orphaned {
            conn.execute(
                "UPDATE messages SET thread_id = ?1 WHERE folder_id = ?2 AND thread_id = ?3",
                params![thread, folder_id, old],
            )?;
        }
        Ok(())
    }

    /// Replace what is known of an account's folders on the server with what
    /// LIST just said.
    pub fn replace_server_folders(
        &mut self,
        account_name: &str,
        folders: &[crate::imap::ListedFolder],
    ) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM server_folders WHERE account_name = ?1",
                params![account_name],
            )?;
            let mut insert = tx.prepare(
                "INSERT INTO server_folders (account_name, name, special_use, delimiter)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for folder in folders {
                insert.execute(params![
                    account_name,
                    folder.name,
                    folder.special_use,
                    folder.delimiter
                ])?;
            }
            Ok(())
        })
    }

    /// The account's folders as the server last listed them, with their roles.
    /// Empty for an account never listed.
    pub fn server_folders(&self, account_name: &str) -> Result<Vec<crate::imap::ListedFolder>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, special_use, delimiter FROM server_folders
             WHERE account_name = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![account_name], |row| {
            Ok(crate::imap::ListedFolder {
                name: row.get(0)?,
                special_use: row.get(1)?,
                delimiter: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A header the sync recorded beside the envelope (see
    /// `Config::extra_header_names`), by name in any case.
    pub fn extra_header(&self, message_id: i64, name: &str) -> Result<Option<String>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT extra_headers FROM messages WHERE id = ?1",
                params![message_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let pairs: Vec<(String, String)> = json
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        Ok(pairs
            .into_iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_string()))
    }

    /// What this machine last heard from a feed; `None` before its first poll.
    pub fn feed_state(&self, url: &str) -> Result<Option<FeedState>> {
        Ok(self
            .conn
            .query_row(
                "SELECT title, etag, last_modified, polled_at, last_error FROM feeds WHERE url = ?1",
                params![url],
                |row| {
                    Ok(FeedState {
                        title: row.get(0)?,
                        etag: row.get(1)?,
                        last_modified: row.get(2)?,
                        polled_at: row.get(3)?,
                        last_error: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Record a poll of a feed.
    pub fn set_feed_state(&self, url: &str, state: &FeedState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO feeds (url, title, etag, last_modified, polled_at, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(url) DO UPDATE SET title = excluded.title, etag = excluded.etag,
                 last_modified = excluded.last_modified, polled_at = excluded.polled_at,
                 last_error = excluded.last_error",
            params![
                url,
                state.title,
                state.etag,
                state.last_modified,
                state.polled_at,
                state.last_error
            ],
        )?;
        Ok(())
    }

    /// Whether this machine has seen a feed item before.
    pub fn feed_item_seen(&self, item_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM feed_items WHERE item_id = ?1",
                params![item_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Whether anything of a feed has been seen here: before that, a poll is
    /// the first, and brings in the newest items only.
    pub fn feed_has_items(&self, feed_url: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM feed_items WHERE feed_url = ?1 LIMIT 1",
                params![feed_url],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Remember feed items as seen, so they are never brought in again here.
    pub fn mark_feed_items_seen(
        &mut self,
        feed_url: &str,
        item_ids: &[&str],
        at: i64,
    ) -> Result<()> {
        self.transaction(|tx| {
            for id in item_ids {
                tx.execute(
                    "INSERT OR IGNORE INTO feed_items (item_id, feed_url, seen_at) VALUES (?1, ?2, ?3)",
                    params![id, feed_url, at],
                )?;
            }
            Ok(())
        })
    }

    /// Follow a folder's new name everywhere its old one is recorded.
    ///
    /// The folders inside it follow, as the server renames them (RFC 3501
    /// §6.3.5), under either hierarchy delimiter. A stale row already under
    /// the new name — a folder the server no longer has — gives way.
    pub fn rename_folder(&mut self, account_name: &str, from: &str, to: &str) -> Result<()> {
        // `?2` is the old name as it is, `?4` the same escaped for LIKE: a
        // folder called `Work_2024` must not take `WorkX2024` along.
        let escaped = crate::db::like_escape(from);
        let inside = |column: &str| {
            format!(
                "({column} = ?2 OR {column} LIKE ?4 || '.%' ESCAPE '\\' \
                 OR {column} LIKE ?4 || '/%' ESCAPE '\\')"
            )
        };
        let renamed = |column: &str| format!("?3 || substr({column}, length(?2) + 1)");
        self.transaction(|tx| {
            tx.execute(
                &format!(
                    "DELETE FROM folders WHERE account_name = ?1
                     AND name IN (SELECT {} FROM folders
                                  WHERE account_name = ?1 AND {})",
                    renamed("name"),
                    inside("name")
                ),
                params![account_name, from, to, escaped],
            )?;
            tx.execute(
                &format!(
                    "UPDATE folders SET name = {} WHERE account_name = ?1 AND {}",
                    renamed("name"),
                    inside("name")
                ),
                params![account_name, from, to, escaped],
            )?;
            for (table, account_column, column) in [
                ("pending_actions", "account", "folder"),
                ("filter_log", "account", "folder"),
                ("server_folders", "account_name", "name"),
            ] {
                tx.execute(
                    &format!(
                        "UPDATE {table} SET {column} = {} WHERE {account_column} = ?1 AND {}",
                        renamed(column),
                        inside(column)
                    ),
                    params![account_name, from, to, escaped],
                )?;
            }
            tx.execute(
                &format!(
                    "UPDATE pending_actions SET target_folder = {} WHERE account = ?1 AND {}",
                    renamed("target_folder"),
                    inside("target_folder")
                ),
                params![account_name, from, to, escaped],
            )?;
            Ok(())
        })
    }

    /// Store the threading headers the backfill fetched for a message, and
    /// put the message into its thread with them.
    pub fn record_thread_headers(
        &self,
        id: i64,
        folder_id: i64,
        headers: &crate::threads::ThreadHeaders,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET in_reply_to = ?1, references_hdr = ?2, list_id = ?3,
                 headers_synced = 1
             WHERE id = ?4",
            params![headers.in_reply_to, headers.references, headers.list_id, id],
        )?;
        Self::assign_thread(&self.conn, folder_id, id)
    }

    /// Messages whose threading headers were never fetched, newest first.
    pub fn messages_without_thread_headers(
        &self,
        folder_id: i64,
        limit: usize,
    ) -> Result<Vec<(i64, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid FROM messages
             WHERE folder_id = ?1 AND (headers_synced IS NULL OR headers_synced = 2)
             ORDER BY date_unix DESC NULLS LAST
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![folder_id, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many messages still lack their threading headers.
    pub fn pending_thread_header_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE headers_synced IS NULL OR headers_synced = 2",
            [],
            |r| r.get(0),
        )?)
    }

    /// Have the backfill fetch the headers of every stored message again when
    /// `names` holds a header it has never been asked for, so that a rule or a
    /// view naming a header finds the mail already here. `headers_synced = 2`
    /// keeps what was recorded until the backfill replaces it. Returns how
    /// many messages were marked.
    pub fn refetch_headers_for_new_names(&mut self, names: &[String]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut new = Vec::new();
        for name in names {
            let key = format!("extra_header:{name}");
            let known: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM sync_meta WHERE key = ?1)",
                params![key],
                |r| r.get(0),
            )?;
            if !known {
                new.push(key);
            }
        }
        if new.is_empty() {
            return Ok(0);
        }
        let marked = tx.execute(
            "UPDATE messages SET headers_synced = 2 WHERE headers_synced = 1",
            [],
        )?;
        for key in new {
            tx.execute(
                "INSERT INTO sync_meta (key, value) VALUES (?1, 1)",
                params![key],
            )?;
        }
        tx.commit()?;
        Ok(marked)
    }

    /// Bulk insert or replace messages within an existing transaction
    ///
    /// Helper for use in optimistic concurrency pattern. Takes an existing
    /// transaction to ensure atomicity with other operations.
    ///
    /// Returns the row ids of the messages inserted, and how many were
    /// updated instead.
    pub(crate) fn bulk_insert_messages_in_tx(
        tx: &rusqlite::Transaction,
        messages: &[Message],
    ) -> Result<(Vec<i64>, usize)> {
        if messages.is_empty() {
            return Ok((Vec::new(), 0));
        }

        let mut inserted = Vec::new();
        let mut updated_count = 0;

        for msg in messages {
            // Determine up front whether this (folder_id, uid) already exists so we
            // can correctly classify the upsert below as a genuine insert vs. an
            // update. last_insert_rowid() is NOT reliable here: ON CONFLICT DO UPDATE
            // leaves it unchanged, so after any real insert it stays above the
            // baseline and would misclassify every following flag-only update as a
            // newly added message (inflating messages_added / spurious new-mail).
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
                    params![msg.folder_id, msg.uid],
                    |row| row.get(0),
                )
                .optional()?;

            // Store flags as JSON array for proper handling of special characters
            let flags_str = serde_json::to_string(&msg.flags).unwrap_or_else(|_| "[]".to_string());

            // Parse date to Unix timestamp for reliable sorting
            let date_unix = msg
                .date
                .as_ref()
                .and_then(|d| Database::parse_date_to_unix(d));

            let (norm_subject, subject_reply) =
                crate::threads::normalise_subject(msg.subject.as_deref());
            let headers = msg.headers.clone().unwrap_or_default();
            tx.execute(
                "INSERT INTO messages
                    (folder_id, uid, message_id, subject, from_addr, to_addr, cc_addr,
                     date, date_unix, size, flags, modseq,
                     text_part, html_part, text_encoding, text_bytes, has_attachments,
                     in_reply_to, references_hdr, list_id, norm_subject, subject_reply,
                     headers_synced, extra_headers)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)
                 ON CONFLICT(folder_id, uid) DO UPDATE SET
                    message_id = excluded.message_id,
                    subject = excluded.subject,
                    from_addr = excluded.from_addr,
                    to_addr = excluded.to_addr,
                    cc_addr = excluded.cc_addr,
                    date = excluded.date,
                    date_unix = excluded.date_unix,
                    size = excluded.size,
                    flags = excluded.flags,
                    modseq = excluded.modseq,
                    -- A flag-only sync carries no BODYSTRUCTURE, so keep what is
                    -- already known rather than erasing it.
                    text_part = COALESCE(excluded.text_part, messages.text_part),
                    html_part = COALESCE(excluded.html_part, messages.html_part),
                    text_encoding = COALESCE(excluded.text_encoding, messages.text_encoding),
                    text_bytes = COALESCE(excluded.text_bytes, messages.text_bytes),
                    has_attachments =
                        COALESCE(excluded.has_attachments, messages.has_attachments),
                    -- A flag-only sync carries the headers too; a fetch that
                    -- did not keeps what the backfill found.
                    in_reply_to = COALESCE(excluded.in_reply_to, messages.in_reply_to),
                    references_hdr = COALESCE(excluded.references_hdr, messages.references_hdr),
                    list_id = COALESCE(excluded.list_id, messages.list_id),
                    norm_subject = excluded.norm_subject,
                    subject_reply = excluded.subject_reply,
                    headers_synced = COALESCE(excluded.headers_synced, messages.headers_synced),
                    extra_headers = COALESCE(excluded.extra_headers, messages.extra_headers)",
                params![
                    msg.folder_id,
                    msg.uid,
                    msg.message_id,
                    msg.subject,
                    msg.from,
                    msg.to,
                    msg.cc,
                    msg.date,
                    date_unix,
                    msg.size,
                    flags_str,
                    msg.modseq,
                    msg.shape.as_ref().and_then(|s| s.text_part.as_deref()),
                    msg.shape.as_ref().and_then(|s| s.html_part.as_deref()),
                    msg.shape.as_ref().and_then(|s| s.text_encoding.as_deref()),
                    msg.shape.as_ref().and_then(|s| s.text_bytes),
                    msg.shape.as_ref().map(|s| i32::from(s.has_attachments)),
                    headers.in_reply_to,
                    headers.references,
                    headers.list_id,
                    norm_subject,
                    subject_reply,
                    msg.headers.as_ref().map(|_| 1),
                    extra_headers_json(&headers),
                ],
            )?;

            match existing {
                Some(id) => {
                    updated_count += 1;
                    Self::store_body(tx, id, msg)?;
                }
                None => {
                    let id = tx.last_insert_rowid();
                    inserted.push(id);
                    Self::store_body(tx, id, msg)?;
                    Self::assign_thread(tx, msg.folder_id, id)?;
                }
            }
        }

        Ok((inserted, updated_count))
    }

    /// Bulk insert or replace messages in a single transaction
    ///
    /// Uses INSERT OR REPLACE for upsert semantics. If a message with the same
    /// (folder_id, uid) exists, it will be updated. This is safe for CONDSTORE
    /// syncs where messages may be fetched multiple times.
    ///
    /// Returns (inserted_count, updated_count)
    pub fn bulk_insert_messages(&mut self, messages: &[Message]) -> Result<(usize, usize)> {
        if messages.is_empty() {
            return Ok((0, 0));
        }

        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (inserted, updated) = Self::bulk_insert_messages_in_tx(&tx, messages)?;
        tx.commit()?;
        Ok((inserted.len(), updated))
    }

    pub fn get_messages_by_folder(&self, folder_id: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                m.id,
                m.folder_id,
                m.uid,
                m.message_id,
                m.subject,
                m.from_addr,
                m.to_addr,
                m.cc_addr,
                m.date,
                b.body_text,
                b.body_html,
                b.rfc822,
                m.size,
                m.flags,
                modseq
             FROM messages m
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.folder_id = ?1
             ORDER BY m.date_unix DESC NULLS LAST",
        )?;

        let rows = stmt.query_map(params![folder_id], |row| {
            // Parse flags from JSON array (or fall back to comma-separated for legacy data)
            let flags_str = row.get::<_, Option<String>>(13)?.unwrap_or_default();
            let flags: Vec<String> = serde_json::from_str(&flags_str).unwrap_or_else(|_| {
                // Fallback for legacy comma-separated format
                flags_str
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            });

            Ok(Message {
                id: Some(row.get(0)?),
                folder_id: row.get(1)?,
                uid: row.get(2)?,
                message_id: row.get(3)?,
                subject: row.get(4)?,
                from: row.get(5)?,
                to: row.get(6)?,
                cc: row.get(7)?,
                date: row.get(8)?,
                body_text: row.get(9)?,
                body_html: row.get(10)?,
                rfc822: row.get(11)?,
                size: row.get(12)?,
                flags,
                modseq: row.get(14)?,
                shape: None,
                headers: None,
            })
        });

        let messages: Vec<Message> = rows?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// Delete messages by UIDs within an existing transaction
    ///
    /// Helper for use in optimistic concurrency pattern. Takes an existing
    /// transaction to ensure atomicity with other operations.
    ///
    /// Batches deletions to avoid SQLite's variable limit (default 999).
    pub(crate) fn delete_messages_by_uids_in_tx(
        tx: &rusqlite::Transaction,
        folder_id: i64,
        uids: &[u32],
    ) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }

        // SQLite has a default limit of 999 variables. We use 500 to leave room
        // for the folder_id parameter and any other query overhead.
        const BATCH_SIZE: usize = 500;

        let mut total_deleted = 0;

        for batch in uids.chunks(BATCH_SIZE) {
            let placeholders = batch.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let query = format!(
                "DELETE FROM messages WHERE folder_id = ? AND uid IN ({})",
                placeholders
            );

            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(folder_id)];
            for uid in batch {
                params_vec.push(Box::new(*uid));
            }
            let params_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();

            let deleted_count = tx.execute(&query, params_refs.as_slice())?;
            total_deleted += deleted_count;
        }

        Ok(total_deleted)
    }

    // ========================================================================
    // Pending (staged) actions
    // ========================================================================

    /// Build a `WHERE` fragment and its bound parameters for a [`PendingScope`].
    ///
    /// The fragment references the `account` and `folder` columns of the
    /// `pending_actions` table and is always non-empty (`1 = 1` for the
    /// whole-mailbox case) so it can be interpolated after `WHERE`.
    fn pending_scope_filter(scope: &PendingScope) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        match scope {
            PendingScope::All => ("1 = 1".to_string(), Vec::new()),
            PendingScope::Account { account_name } => (
                "account = ?1".to_string(),
                vec![Box::new(account_name.clone())],
            ),
            PendingScope::Folder {
                account_name,
                folder_name,
            } => (
                "account = ?1 AND folder = ?2".to_string(),
                vec![
                    Box::new(account_name.clone()),
                    Box::new(folder_name.clone()),
                ],
            ),
        }
    }

    /// Stage (persist) a single destructive action for a message.
    ///
    /// At most one action exists per `(account, folder, uid)`: staging again for
    /// the same message replaces the previous action (last action wins).
    pub fn stage_pending_action(
        &self,
        account: &str,
        folder: &str,
        uid: u32,
        action_type: &str,
        target_folder: Option<&str>,
        created_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pending_actions
                 (account, folder, uid, action_type, target_folder, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account, folder, uid) DO UPDATE SET
                 action_type = excluded.action_type,
                 target_folder = excluded.target_folder,
                 created_at = excluded.created_at",
            params![account, folder, uid, action_type, target_folder, created_at],
        )?;
        Ok(())
    }

    /// Remove a single staged action (unmark). Returns the number of rows
    /// removed (0 if the message had no pending action).
    /// Drop the staged actions for these UIDs.
    ///
    /// Takes a list because every other action on a selection does: unstaging
    /// fifty messages one statement at a time meant fifty round trips through
    /// the write lock for what is one DELETE.
    pub fn unstage_pending_actions(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
    ) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }
        // Built rather than bound because SQLite has no array parameter. The
        // values are `u32`, so there is nothing here a string could smuggle.
        let placeholders = uids
            .iter()
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        Ok(self.conn.execute(
            &format!(
                "DELETE FROM pending_actions
                 WHERE account = ?1 AND folder = ?2 AND uid IN ({placeholders})"
            ),
            params![account, folder],
        )?)
    }

    /// List staged actions within a scope, ordered deterministically.
    pub fn list_pending_actions(&self, scope: &PendingScope) -> Result<Vec<PendingActionRecord>> {
        let (where_clause, params) = Self::pending_scope_filter(scope);
        let sql = format!(
            "SELECT account, folder, uid, action_type, target_folder, created_at
             FROM pending_actions
             WHERE {}
             ORDER BY account, folder, uid",
            where_clause
        );
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                Ok(PendingActionRecord {
                    account: row.get(0)?,
                    folder: row.get(1)?,
                    uid: row.get(2)?,
                    action_type: row.get(3)?,
                    target_folder: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Count staged actions per (account, folder) within a scope.
    pub fn pending_action_counts(&self, scope: &PendingScope) -> Result<Vec<PendingActionCount>> {
        let (where_clause, params) = Self::pending_scope_filter(scope);
        let sql = format!(
            "SELECT account, folder, COUNT(*)
             FROM pending_actions
             WHERE {}
             GROUP BY account, folder
             ORDER BY account, folder",
            where_clause
        );
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                let count: i64 = row.get(2)?;
                Ok(PendingActionCount {
                    account: row.get(0)?,
                    folder: row.get(1)?,
                    count: count as u32,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Clear (discard) all staged actions within a scope without executing
    /// them. Returns the number of rows removed.
    pub fn clear_pending_actions(&self, scope: &PendingScope) -> Result<usize> {
        let (where_clause, params) = Self::pending_scope_filter(scope);
        let sql = format!("DELETE FROM pending_actions WHERE {}", where_clause);
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        Ok(self.conn.execute(&sql, params_refs.as_slice())?)
    }

    /// Remove the staged-action rows for a specific set of UIDs in one folder.
    ///
    /// Used after an [`crate::store::Store::execute_pending`] batch has applied
    /// its underlying delete/move on the server, to clear only the rows that
    /// actually executed.
    pub fn delete_pending_actions_for_uids(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
    ) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }
        const BATCH_SIZE: usize = 500;
        let mut total = 0;
        for batch in uids.chunks(BATCH_SIZE) {
            let placeholders = batch.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let query = format!(
                "DELETE FROM pending_actions
                 WHERE account = ? AND folder = ? AND uid IN ({})",
                placeholders
            );
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
                vec![Box::new(account.to_string()), Box::new(folder.to_string())];
            for uid in batch {
                params_vec.push(Box::new(*uid));
            }
            let params_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            total += self.conn.execute(&query, params_refs.as_slice())?;
        }
        Ok(total)
    }

    /// Delete staged actions for the given uids ONLY when they still match the
    /// exact action snapshot that was executed (same account/folder/uid AND the
    /// same `action_type` + `target_folder`).
    ///
    /// `execute_pending` reads staged rows, runs the server op, then clears the
    /// rows. If another window re-stages one of those uids as a *different*
    /// action mid-flush (the upsert updates the same row), an unconditional
    /// delete-by-uid would silently drop that new action. Matching on the
    /// snapshot leaves a concurrently-changed row intact (it is applied on the
    /// next flush, or pruned if its uid later vanishes). `target_folder IS ?` is
    /// NULL-safe.
    pub fn delete_pending_actions_matching(
        &self,
        account: &str,
        folder: &str,
        uids: &[u32],
        action_type: &str,
        target_folder: Option<&str>,
    ) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }
        const BATCH_SIZE: usize = 500;
        let mut total = 0;
        for batch in uids.chunks(BATCH_SIZE) {
            let placeholders = batch.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let query = format!(
                "DELETE FROM pending_actions
                 WHERE account = ? AND folder = ? AND action_type = ?
                   AND target_folder IS ? AND uid IN ({})",
                placeholders
            );
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(account.to_string()),
                Box::new(folder.to_string()),
                Box::new(action_type.to_string()),
                Box::new(target_folder.map(|s| s.to_string())),
            ];
            for uid in batch {
                params_vec.push(Box::new(*uid));
            }
            let params_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            total += self.conn.execute(&query, params_refs.as_slice())?;
        }
        Ok(total)
    }

    /// Prune staged actions for a folder whose UID no longer exists in the local
    /// `messages` table, within an existing sync transaction.
    ///
    /// Called after a folder resync has applied its inserts/deletions, so a
    /// message deleted on the server (by another client) cannot leave a
    /// dangling staged action behind.
    pub(crate) fn prune_pending_actions_in_tx(
        tx: &rusqlite::Transaction,
        account: &str,
        folder: &str,
        folder_id: i64,
    ) -> Result<usize> {
        Ok(tx.execute(
            "DELETE FROM pending_actions
             WHERE account = ?1 AND folder = ?2
               AND uid NOT IN (SELECT uid FROM messages WHERE folder_id = ?3)",
            params![account, folder, folder_id],
        )?)
    }

    /// Remove all staged actions for a folder within an existing transaction.
    ///
    /// Called when a folder's UIDVALIDITY changes: every stored UID is now
    /// meaningless, so all staged actions for that folder must be dropped.
    pub(crate) fn clear_pending_actions_for_folder_in_tx(
        tx: &rusqlite::Transaction,
        account: &str,
        folder: &str,
    ) -> Result<usize> {
        Ok(tx.execute(
            "DELETE FROM pending_actions WHERE account = ?1 AND folder = ?2",
            params![account, folder],
        )?)
    }

    /// Maintain persisted staged actions after a folder resync, within the sync
    /// transaction. On a UIDVALIDITY change every stored UID is meaningless, so
    /// drop the folder's staged actions wholesale; otherwise prune only those
    /// whose UID has vanished from the freshly-synced folder. Extracted from the
    /// sync loop so the exact clear-vs-prune decision is directly unit-testable.
    pub(crate) fn maintain_pending_actions_after_sync_in_tx(
        tx: &rusqlite::Transaction,
        account: &str,
        folder: &str,
        folder_id: i64,
        uidvalidity_changed: bool,
    ) -> Result<usize> {
        if uidvalidity_changed {
            Self::clear_pending_actions_for_folder_in_tx(tx, account, folder)
        } else {
            Self::prune_pending_actions_in_tx(tx, account, folder, folder_id)
        }
    }

    /// Delete messages by UIDs (hard delete)
    ///
    /// Used when messages are deleted on the server and need to be removed from
    /// the local database. Wrapped in a transaction for safety.
    pub fn delete_messages_by_uids(&mut self, folder_id: i64, uids: &[u32]) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }

        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let deleted_count = Self::delete_messages_by_uids_in_tx(&tx, folder_id, uids)?;
        tx.commit()?;
        Ok(deleted_count)
    }

    /// Get all message UIDs for a folder within an existing transaction
    ///
    /// Helper for use in optimistic concurrency pattern. Takes an existing
    /// transaction to ensure UIDs are read atomically with baseline state.
    pub(crate) fn get_folder_uids_in_tx(
        tx: &rusqlite::Transaction,
        folder_id: i64,
    ) -> Result<Vec<u32>> {
        let mut stmt = tx.prepare("SELECT uid FROM messages WHERE folder_id = ?1 ORDER BY uid")?;

        let uids = stmt
            .query_map(params![folder_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(uids)
    }

    /// Get all message UIDs for a folder
    ///
    /// Used for deletion detection via UID comparison when QRESYNC is not available.
    pub fn get_folder_uids(&self, folder_id: i64) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid FROM messages WHERE folder_id = ?1 ORDER BY uid")?;

        let uids = stmt
            .query_map(params![folder_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(uids)
    }

    /// Whether this folder's flags are due to be checked against the server.
    ///
    /// Kept per folder in the database rather than in memory so that restarting
    /// does not make every folder due at once — on a server whose modseq never
    /// moves, that would mean a flag fetch for every folder on every launch.
    pub fn flags_check_is_due(
        &self,
        folder_id: i64,
        interval: std::time::Duration,
    ) -> Result<bool> {
        let last: Option<String> = self
            .conn
            .query_row(
                "SELECT flags_checked_at FROM folders WHERE id = ?1",
                params![folder_id],
                |row| row.get(0),
            )
            .unwrap_or(None);

        let Some(last) = last.as_deref() else {
            return Ok(true);
        };
        let Ok(when) = chrono::DateTime::parse_from_rfc3339(last) else {
            // Unreadable means unknown, and unknown means check.
            return Ok(true);
        };
        let age = chrono::Utc::now().signed_duration_since(when.with_timezone(&chrono::Utc));
        Ok(age.to_std().is_ok_and(|age| age >= interval))
    }

    /// Record that this folder's flags have just been checked.
    pub fn note_flags_checked(&self, folder_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE folders SET flags_checked_at = ?1 WHERE id = ?2",
            params![chrono::Utc::now().to_rfc3339(), folder_id],
        )?;
        Ok(())
    }

    /// Forget when this folder's flags were checked, making one due now.
    ///
    /// For a test that needs the check to run, and for a command that asks for
    /// one outright.
    pub fn clear_flags_checked(&self, folder_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE folders SET flags_checked_at = NULL WHERE id = ?1",
            params![folder_id],
        )?;
        Ok(())
    }

    /// The flags stored for every message in a folder, keyed by UID.
    ///
    /// One query rather than one per message: this is compared against the
    /// server's answer for the same set, so both sides arrive as a map.
    pub fn folder_flags(
        &self,
        folder_id: i64,
    ) -> Result<std::collections::HashMap<u32, Vec<String>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid, flags FROM messages WHERE folder_id = ?1")?;
        let rows = stmt.query_map(params![folder_id], |row| {
            let uid: u32 = row.get(0)?;
            let raw: Option<String> = row.get(1)?;
            Ok((uid, raw))
        })?;

        let mut found = std::collections::HashMap::new();
        for row in rows {
            let (uid, raw) = row?;
            let mut flags: Vec<String> =
                serde_json::from_str(raw.as_deref().unwrap_or("[]")).unwrap_or_default();
            flags.sort();
            found.insert(uid, flags);
        }
        Ok(found)
    }

    /// Replace the flags of messages whose UIDs are given, in one transaction.
    ///
    /// Returns how many rows changed. Only the messages that actually differ
    /// should be passed: rewriting a flag to its current value still costs a
    /// write and, through the FTS triggers, an index update.
    pub fn replace_flags(
        &mut self,
        folder_id: i64,
        flags_by_uid: &[(u32, Vec<String>)],
    ) -> Result<usize> {
        if flags_by_uid.is_empty() {
            return Ok(0);
        }
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut changed = 0;
        {
            let mut stmt =
                tx.prepare("UPDATE messages SET flags = ?1 WHERE folder_id = ?2 AND uid = ?3")?;
            for (uid, flags) in flags_by_uid {
                let encoded = serde_json::to_string(flags).unwrap_or_else(|_| "[]".to_string());
                changed += stmt.execute(params![encoded, folder_id, uid])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Clear all messages from a folder
    ///
    /// Used when UIDVALIDITY changes, which invalidates all existing UIDs and requires
    /// a full resync.
    pub fn clear_folder_messages(&mut self, folder_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM messages WHERE folder_id = ?1",
            params![folder_id],
        )?;
        Ok(())
    }

    /// Get paginated message list for a folder without body content
    ///
    /// Returns lightweight message metadata for UI rendering, excluding
    /// body_text, body_html, and rfc822 fields for performance.
    ///
    /// # Arguments
    ///
    /// * `folder_id` - Database folder ID
    /// * `offset` - Number of messages to skip (for pagination)
    /// * `limit` - Maximum number of messages to return
    /// * `sort` - Sort order for the message list
    pub fn get_message_list(
        &self,
        folder_id: i64,
        offset: usize,
        limit: usize,
        sort: MessageSort,
    ) -> Result<Vec<MessageListItem>> {
        self.get_message_list_in(&[folder_id], offset, limit, sort)
    }

    /// The row a message is on in its folder's list in `sort` order, counted
    /// from 0, so a list can load the page it is on; `None` when the folder
    /// does not hold it.
    pub fn message_row(&self, folder_id: i64, id: i64, sort: MessageSort) -> Result<Option<usize>> {
        self.message_row_in(&[folder_id], id, sort)
    }

    /// [`Database::message_row`] in the list [`Database::get_message_list_in`]
    /// reads from several folders.
    pub fn message_row_in(
        &self,
        folder_ids: &[i64],
        id: i64,
        sort: MessageSort,
    ) -> Result<Option<usize>> {
        if folder_ids.is_empty() {
            return Ok(None);
        }
        let in_folders = folder_ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let query = format!(
            "SELECT row FROM (
                 SELECT id, ROW_NUMBER() OVER (ORDER BY {}) - 1 AS row
                 FROM messages WHERE folder_id IN ({in_folders})
             ) WHERE id = ?1",
            sort.as_sql()
        );
        let row: Option<i64> = self
            .conn
            .query_row(&query, params![id], |row| row.get(0))
            .optional()?;
        Ok(row.map(|row| row as usize))
    }

    /// The same page over several folders at once, in `sort` order across
    /// all of them; each item says which folder it is from.
    pub fn get_message_list_in(
        &self,
        folder_ids: &[i64],
        offset: usize,
        limit: usize,
        sort: MessageSort,
    ) -> Result<Vec<MessageListItem>> {
        if folder_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; folder_ids.len()].join(",");
        let query = format!(
            "SELECT
                id,
                folder_id,
                uid,
                message_id,
                subject,
                from_addr,
                to_addr,
                cc_addr,
                date,
                size,
                flags,
                modseq
             FROM messages
             WHERE folder_id IN ({placeholders})
             ORDER BY {}
             LIMIT ? OFFSET ?",
            sort.as_sql()
        );

        let mut stmt = self.conn.prepare(&query)?;
        let parameters: Vec<rusqlite::types::Value> = folder_ids
            .iter()
            .map(|id| rusqlite::types::Value::from(*id))
            .chain([
                rusqlite::types::Value::from(limit as i64),
                rusqlite::types::Value::from(offset as i64),
            ])
            .collect();

        let rows = stmt.query_map(rusqlite::params_from_iter(parameters), |row| {
            // Parse flags from JSON array (or fall back to comma-separated for legacy data)
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
        });

        let messages: Vec<MessageListItem> = rows?.collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(messages)
    }

    /// Every unread message of a folder, newest first: what the sidebar's
    /// highlight badges count over, since only unread mail is counted.
    pub fn unread_message_list(&self, folder_id: i64) -> Result<Vec<MessageListItem>> {
        let mut stmt = self.conn.prepare(UNREAD_MESSAGE_LIST)?;
        let rows = stmt.query_map(params![folder_id], |row| {
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
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Return the single newest unread message across the whole mailbox (every
    /// folder of every account), or `None` when there is no unread mail.
    ///
    /// "Unread" uses the same definition as [`Database::get_folder_counts`]: a
    /// message whose JSON `flags` array does not contain `"Seen"`. Rows are
    /// ordered by `date_unix` descending (newest first, undated last), so the
    /// returned item is the most recently-dated unread message — the correct
    /// representative sender/subject/id for a new-mail notification regardless
    /// of which folder the mail arrived in.
    pub fn latest_unread_message(&self) -> Result<Option<MessageListItem>> {
        let mut stmt = self.conn.prepare(
            r#"SELECT
                id,
                folder_id,
                uid,
                message_id,
                subject,
                from_addr,
                to_addr,
                cc_addr,
                date,
                size,
                flags,
                modseq
             FROM messages
             WHERE (flags IS NULL OR flags NOT LIKE '%Seen%')
             ORDER BY date_unix DESC NULLS LAST
             LIMIT 1"#,
        )?;

        let mut rows = stmt.query_map([], |row| {
            // Parse flags from JSON array (or fall back to comma-separated for legacy data)
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
        })?;

        match rows.next() {
            Some(item) => Ok(Some(item?)),
            None => Ok(None),
        }
    }

    /// Order every plausible message by how close its meaning is to `text`.
    ///
    /// Two stages, which is what makes a one-bit-per-dimension index good
    /// enough. The index answers roughly and very fast — 16MB and about 3.5ms
    /// over 290,000 messages, against 305MB and 24ms at full precision — and
    /// then the shortlist it returns is re-embedded and scored exactly. The
    /// index only has to get the right messages into the shortlist; the order
    /// they come back in does not have to be right, because it is recomputed.
    ///
    /// Re-embedding a few hundred messages costs single-digit milliseconds,
    /// which is the only reason this is worth doing rather than storing full
    /// vectors.
    ///
    /// `None` means ranking by meaning is not possible — no model, no index, or
    /// nothing indexed — as distinct from `Some(empty)`, which means it is
    /// possible and nothing matched.
    fn rank_by_meaning(
        &self,
        text: &str,
        model_directory: Option<&str>,
    ) -> Result<Option<std::collections::HashMap<i64, f32>>> {
        use crate::search::embed;

        if !self.has_vector_index() {
            return Ok(None);
        }
        let Some(embedder) = embed::shared(model_directory) else {
            return Ok(None);
        };

        let query_vector = embedder.encode_one(text);
        if query_vector.is_empty() {
            return Ok(None);
        }

        // Generous, because the filters in the rest of the query are applied
        // after this: a shortlist that only just covers the limit would come
        // back empty as soon as it is narrowed to one sender.
        let candidates = self.nearest_messages(&embed::pack(&query_vector), SEMANTIC_CANDIDATES)?;
        if candidates.is_empty() {
            // The index exists but holds nothing yet.
            let (_, indexed) = self.embedding_progress()?;
            return Ok(if indexed == 0 {
                None
            } else {
                Some(std::collections::HashMap::new())
            });
        }

        let texts: Vec<String> = candidates
            .iter()
            .map(|(_, subject, body)| embed::embedding_text(subject.as_deref(), body.as_deref()))
            .collect();
        let vectors = embedder.encode(&texts);

        let scored: Vec<(i64, f32)> = candidates
            .iter()
            .zip(&vectors)
            .map(|((id, _, _), vector)| (*id, embed::cosine(&query_vector, vector)))
            .collect();

        // Nearest-neighbour search always returns its k nearest, however far
        // away they are, so without a cut every query answers with the whole
        // shortlist and the tail is noise. What counts as close depends on the
        // query — the best match for one phrase scored 0.80 and for another
        // 0.66 — so the cut is mostly relative to the best match, with a low
        // absolute floor for when nothing is really relevant at all.
        let best = scored.iter().map(|(_, s)| *s).fold(f32::MIN, f32::max);
        let cutoff = (best * SEMANTIC_RELATIVE_FLOOR).max(SEMANTIC_ABSOLUTE_FLOOR);

        Ok(Some(
            scored
                .into_iter()
                .filter(|(_, score)| *score >= cutoff)
                .collect(),
        ))
    }

    /// Full-text search across the whole mailbox (or a narrower scope).
    ///
    /// Runs `query` against the `messages_fts` index (subject / from / to /
    /// body), joins back to `messages` and `folders`, and returns the matching
    /// messages newest-first, each tagged with its owning account and folder so
    /// the caller can open it regardless of which page is currently loaded.
    ///
    /// `query` is raw user input; it is converted into a safe FTS5 MATCH
    /// expression via the query compiler. A query with no
    /// searchable tokens returns an empty result set rather than erroring.
    ///
    /// # Arguments
    ///
    /// * `query` - Raw user search text.
    /// * `scope` - Restrict to a folder, an account, or the whole mailbox.
    /// * `limit` - Maximum number of results to return.
    pub fn search_messages(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: usize,
        model_directory: Option<&str>,
    ) -> Result<Vec<SearchResultItem>> {
        self.search_messages_sorted(query, scope, limit, model_directory, None)
    }

    /// [`Self::search_messages`] in the order `sort` gives, applied before
    /// `limit`, so a search sorted oldest first finds the oldest matches
    /// rather than reordering the most relevant ones. `None` is the query's
    /// own order: by relevance, by meaning for `~`, newest first for an `OR`
    /// or a query of conditions alone.
    pub fn search_messages_sorted(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: usize,
        model_directory: Option<&str>,
        sort: Option<MessageSort>,
    ) -> Result<Vec<SearchResultItem>> {
        // A query that will not parse is reported. Searching happens when the
        // query is submitted, not as it is typed, so at that point unparseable
        // input is a mistake to be told about — returning an empty result made
        // "f: thomas needs a value" indistinguishable from "no such mail".
        let parsed = crate::search::parse(query).map_err(|e| {
            crate::Error::InvalidInput(format!("{} (at character {})", e.message, e.at + 1))
        })?;
        let groups = crate::search::compile::compile_groups(&parsed);
        if groups.len() == 1 {
            return self.search_compiled(&groups[0], scope, limit, model_directory, sort);
        }

        // `OR`: each alternative is a search of its own, and the answers are
        // joined, newest first — a ranking by relevance means nothing across
        // two questions.
        let mut seen = HashSet::new();
        let mut merged: Vec<SearchResultItem> = Vec::new();
        for compiled in &groups {
            for hit in self.search_compiled(compiled, scope, limit, model_directory, sort)? {
                if seen.insert(hit.message.id) {
                    merged.push(hit);
                }
            }
        }
        let sort = sort.unwrap_or(MessageSort::DateNewest);
        merged.sort_by(|a, b| compare_hits(sort, a, b));
        merged.truncate(limit);
        Ok(merged)
    }

    /// One compiled conjunction, answered.
    fn search_compiled(
        &self,
        compiled: &crate::search::compile::Compiled,
        scope: &SearchScope,
        limit: usize,
        model_directory: Option<&str>,
        sort: Option<MessageSort>,
    ) -> Result<Vec<SearchResultItem>> {
        // A query with nothing in it at all would list the whole mailbox.
        if compiled.conditions.is_empty() && compiled.semantic.is_none() {
            return Ok(Vec::new());
        }

        // A `~` query is answered by the vector index and narrowed by whatever
        // else the query says, so the ranking comes from meaning rather than
        // from word overlap.
        let ranking = match &compiled.semantic {
            Some(text) => match self.rank_by_meaning(text, model_directory)? {
                Some(ranking) => Some(ranking),
                // No model, no index, or nothing indexed yet. Returning
                // everything ordered by date would look like a working search
                // over the wrong thing.
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        // Relevance is the index's score for the query's words, recency among
        // equal scores; a hit the index did not score — its word only inside
        // a header — comes after the scored ones. Without words, or in an
        // order asked for, the score is not computed at all.
        let rank = compiled
            .rank_match
            .as_ref()
            .filter(|_| ranking.is_none() && sort.is_none());
        //
        // The scores are computed once, as a table: joined as a subquery,
        // SQLite ran the match again for every row, which over a word every
        // message of 100 000 has took minutes.
        let (with, from, order) = match rank {
            Some(_) => (
                "WITH r AS MATERIALIZED (
                     SELECT rowid AS id, bm25(messages_fts) AS score
                     FROM messages_fts WHERE messages_fts MATCH ?)",
                "FROM messages m
                 JOIN folders f ON f.id = m.folder_id
                 LEFT JOIN r ON r.id = m.id",
                "ORDER BY r.score IS NULL, r.score, m.date_unix DESC NULLS LAST",
            ),
            None => (
                "",
                "FROM messages m
                 JOIN folders f ON f.id = m.folder_id",
                "ORDER BY m.date_unix DESC NULLS LAST",
            ),
        };

        let mut sql = format!(
            "{with}
             SELECT
                m.id,
                m.folder_id,
                m.uid,
                m.message_id,
                m.subject,
                m.from_addr,
                m.to_addr,
                m.cc_addr,
                m.date,
                m.size,
                m.flags,
                m.modseq,
                f.account_name,
                f.name
             {from}
             WHERE 1 = 1"
        );

        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(match_expression) = rank {
            params.push(Box::new(match_expression.clone()));
        }
        // Conditions carry their own placeholders, and the compiled values are
        // in the same order, so walk them together: each condition consumes as
        // many values as it has `?`.
        let mut values = compiled.params.iter();
        for condition in &compiled.conditions {
            sql.push_str(" AND ");
            sql.push_str(condition);
            for _ in 0..condition.matches('?').count() {
                let value = values
                    .next()
                    .expect("compile() must emit one value per placeholder");
                params.push(Box::new(value.clone()));
            }
        }
        debug_assert!(
            values.next().is_none(),
            "compile() emitted more values than placeholders"
        );

        // The scope the caller asked for narrows whatever the query already says.
        match scope {
            SearchScope::All => {}
            SearchScope::Account { account_name } => {
                sql.push_str(" AND f.account_name = ?");
                params.push(Box::new(account_name.clone()));
            }
            SearchScope::Folder {
                account_name,
                folder_name,
            } => {
                sql.push_str(" AND f.account_name = ? AND f.name = ?");
                params.push(Box::new(account_name.clone()));
                params.push(Box::new(folder_name.clone()));
            }
            SearchScope::Folders(folders) if folders.is_empty() => sql.push_str(" AND 0"),
            SearchScope::Folders(folders) => {
                let each = vec!["(f.account_name = ? AND f.name = ?)"; folders.len()];
                sql.push_str(&format!(" AND ({})", each.join(" OR ")));
                for (account, folder) in folders {
                    params.push(Box::new(account.clone()));
                    params.push(Box::new(folder.clone()));
                }
            }
        }

        // A semantic query is restricted to its shortlist and ordered by the
        // exact scores, not by the index's rough ones and not by date. The rows
        // come back unordered and are sorted below, because the order lives in
        // Rust rather than in SQL.
        if let Some(ranking) = &ranking {
            if ranking.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders = std::iter::repeat_n("?", ranking.len())
                .collect::<Vec<_>>()
                .join(",");
            sql.push_str(&format!(" AND m.id IN ({placeholders})"));
            for id in ranking.keys() {
                params.push(Box::new(*id));
            }
        } else {
            match sort {
                Some(sort) => {
                    sql.push_str(" ORDER BY ");
                    sql.push_str(&sort.as_sql_on("m"));
                }
                None => {
                    sql.push(' ');
                    sql.push_str(order);
                }
            }
        }
        sql.push_str(" LIMIT ?");
        // The shortlist is already bounded; taking `limit` before ranking would
        // cut the best matches off by whatever SQL happened to return first.
        params.push(Box::new(if ranking.is_some() {
            SEMANTIC_CANDIDATES as i64
        } else {
            limit as i64
        }));

        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            // Parse flags from JSON array (or fall back to comma-separated for legacy data)
            let flags_str = row.get::<_, Option<String>>(10)?.unwrap_or_default();
            let flags: Vec<String> = serde_json::from_str(&flags_str).unwrap_or_else(|_| {
                flags_str
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            });

            let message = MessageListItem {
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
            };

            Ok(SearchResultItem {
                message,
                account_name: row.get(12)?,
                folder_name: row.get(13)?,
            })
        })?;

        let mut results: Vec<SearchResultItem> =
            rows.collect::<std::result::Result<Vec<_>, _>>()?;

        // Order by meaning, then cut to what was asked for. Sorting here rather
        // than in SQL keeps the scores at full precision: they were computed
        // against the query vector, not read back out of a one-bit index.
        if let Some(ranking) = ranking {
            results.sort_by(|a, b| {
                let score = |item: &SearchResultItem| {
                    ranking.get(&item.message.id).copied().unwrap_or(f32::MIN)
                };
                score(b)
                    .partial_cmp(&score(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            results.truncate(limit);
            // The closest by meaning are the hits; the order asked for is how
            // they are shown.
            if let Some(sort) = sort {
                results.sort_by(|a, b| compare_hits(sort, a, b));
            }
        }

        Ok(results)
    }

    /// Get total message count for a folder
    ///
    /// # Arguments
    ///
    /// * `folder_id` - Database ID of the folder
    pub fn get_message_count(&self, folder_id: i64) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM messages WHERE folder_id = ?1")?;
        let count: i64 = stmt.query_row([folder_id], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Get a single message by its database ID
    ///
    /// # Arguments
    ///
    /// * `message_id` - Database ID of the message
    pub fn get_message_by_id(&self, message_id: i64) -> Result<Option<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                m.id,
                m.folder_id,
                m.uid,
                m.message_id,
                m.subject,
                m.from_addr,
                m.to_addr,
                m.cc_addr,
                m.date,
                b.body_text,
                b.body_html,
                b.rfc822,
                m.size,
                m.flags,
                m.modseq,
                m.in_reply_to,
                m.references_hdr,
                m.list_id,
                m.headers_synced
             FROM messages m
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.id = ?1",
        )?;

        let result = stmt.query_row(params![message_id], |row| {
            // Parse flags from JSON array (or fall back to comma-separated for legacy data)
            let flags_str = row.get::<_, Option<String>>(13)?.unwrap_or_default();
            let flags: Vec<String> = serde_json::from_str(&flags_str).unwrap_or_else(|_| {
                flags_str
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            });

            Ok(Message {
                id: Some(row.get(0)?),
                folder_id: row.get(1)?,
                uid: row.get(2)?,
                message_id: row.get(3)?,
                subject: row.get(4)?,
                from: row.get(5)?,
                to: row.get(6)?,
                cc: row.get(7)?,
                date: row.get(8)?,
                body_text: row.get(9)?,
                body_html: row.get(10)?,
                rfc822: row.get(11)?,
                size: row.get(12)?,
                flags,
                modseq: row.get(14)?,
                shape: None,
                headers: row
                    .get::<_, Option<i64>>(18)?
                    .map(|_| crate::threads::ThreadHeaders {
                        in_reply_to: row.get(15).unwrap_or(None),
                        references: row.get(16).unwrap_or(None),
                        list_id: row.get(17).unwrap_or(None),
                        extra: Vec::new(),
                    }),
            })
        });

        match result {
            Ok(msg) => Ok(Some(msg)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Determine which of the given messages have attachments, using their
    /// cached RFC822 source.
    ///
    /// Only messages whose full source has already been fetched (rfc822 IS NOT
    /// NULL) can be evaluated; messages without a cached body are simply absent
    /// from the returned set (reported as "no attachments" until their body is
    /// fetched on demand). This performs a single query over the requested ids
    /// and parses each cached body with the MIME walker.
    pub fn messages_with_attachments(&self, ids: &[i64]) -> Result<HashSet<i64>> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }

        // Reads the stored answer rather than re-parsing each message's MIME
        // tree; see migration 13. In chunks: a whole folder's worth of ids is
        // more than one statement may bind.
        let mut result = HashSet::new();
        for chunk in ids.chunks(500) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id FROM messages WHERE has_attachments = 1 AND id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                row.get::<_, i64>(0)
            })?;
            for row in rows {
                result.insert(row?);
            }
        }
        Ok(result)
    }

    /// The opening of each message's text, for the list's snippet: the
    /// first lines that are not quoted, on one line, at most
    /// [`SNIPPET_CHARS`] characters. A message whose text has not been
    /// fetched yet has none.
    pub fn snippets(&self, ids: &[i64]) -> Result<HashMap<i64, String>> {
        let mut result = HashMap::new();
        for chunk in ids.chunks(500) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            // A prefix rather than the whole body: enough for a line, however
            // long the message.
            let sql = format!(
                "SELECT message_id, substr(body_text, 1, 2000) FROM message_bodies
                 WHERE body_text IS NOT NULL AND message_id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, text) = row?;
                let snippet = snippet(&text);
                if !snippet.is_empty() {
                    result.insert(id, snippet);
                }
            }
        }
        Ok(result)
    }

    /// Store a message's full source, fetched for the sake of its attachments.
    ///
    /// The readable body is left as it is: the source carries the same text,
    /// but re-deriving it here would risk replacing a body the worker already
    /// indexed with a differently decoded one.
    pub fn set_message_source(&self, message_id: i64, rfc822: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO message_bodies (message_id, rfc822) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET rfc822 = excluded.rfc822",
            params![message_id, rfc822],
        )?;
        self.conn.execute(
            "UPDATE messages SET has_attachments = ?1 WHERE id = ?2",
            params![
                i32::from(crate::attachments::has_attachments(rfc822)),
                message_id
            ],
        )?;
        Ok(())
    }

    /// Messages in a folder with attachments and a readable body but no stored
    /// source — the ones whose attachments cannot be opened offline yet.
    ///
    /// Newest first, like the body backfill, because recent mail is what is
    /// opened.
    pub fn messages_needing_source(&self, folder_id: i64, limit: usize) -> Result<Vec<(i64, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.uid FROM messages m
             JOIN message_bodies b ON b.message_id = m.id
             WHERE m.folder_id = ?1 AND m.has_attachments = 1 AND b.rfc822 IS NULL
               AND (b.body_text IS NOT NULL OR b.body_html IS NOT NULL)
             ORDER BY m.date_unix DESC NULLS LAST
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![folder_id, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, u32>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many messages still want their source for their attachments.
    pub fn source_backfill_pending(&self) -> Result<usize> {
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages m
             JOIN message_bodies b ON b.message_id = m.id
             WHERE m.has_attachments = 1 AND b.rfc822 IS NULL
               AND (b.body_text IS NOT NULL OR b.body_html IS NOT NULL)",
            [],
            |r| r.get(0),
        )?;
        Ok(pending as usize)
    }

    /// The row id, subject and sender of one message, by its place in a folder.
    pub fn message_headline(&self, folder_id: i64, uid: u32) -> Result<Option<Headline>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, subject, from_addr FROM messages WHERE folder_id = ?1 AND uid = ?2",
        )?;
        let mut rows = stmt.query_map(params![folder_id, uid], |row| {
            Ok(Headline {
                id: row.get(0)?,
                subject: row.get(1)?,
                from: row.get(2)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    /// The row id of each of the given UIDs in a folder that is stored.
    pub fn ids_for_uids(&self, folder_id: i64, uids: &[u32]) -> Result<HashMap<u32, i64>> {
        if uids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = std::iter::repeat_n("?", uids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT uid, id FROM messages WHERE folder_id = ? AND uid IN ({placeholders})");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut values: Vec<rusqlite::types::Value> = vec![folder_id.into()];
        values.extend(
            uids.iter()
                .map(|uid| rusqlite::types::Value::from(*uid as i64)),
        );
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// The RFC `Message-ID` of each of the given UIDs in a folder, for those
    /// that have one.
    pub fn message_ids_for_uids(&self, folder_id: i64, uids: &[u32]) -> Result<Vec<String>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", uids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT message_id FROM messages
             WHERE folder_id = ? AND uid IN ({placeholders})
               AND message_id IS NOT NULL AND message_id <> ''"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut values: Vec<rusqlite::types::Value> = vec![folder_id.into()];
        values.extend(
            uids.iter()
                .map(|uid| rusqlite::types::Value::from(*uid as i64)),
        );
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The unread ones among the messages `ids` name, oldest first, with where
    /// they live. What a new-mail notification is made from: a message a rule
    /// filed away is no longer here, and one a rule marked read is not unread.
    pub fn unread_messages_among(&self, ids: &[i64]) -> Result<Vec<NewMessage>> {
        let mut found = Vec::new();
        for chunk in ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut stmt = self.conn.prepare(&format!(
                r#"SELECT m.id, f.account_name, f.name, m.from_addr, m.subject
                   FROM messages m JOIN folders f ON f.id = m.folder_id
                   WHERE m.id IN ({placeholders})
                     AND (m.flags IS NULL OR m.flags NOT LIKE '%Seen%')
                   ORDER BY m.id"#
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok(NewMessage {
                    id: row.get(0)?,
                    account: row.get(1)?,
                    folder: row.get(2)?,
                    from: row.get(3)?,
                    subject: row.get(4)?,
                })
            })?;
            found.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(found)
    }

    /// The focus that was started and not ended, whether or not its time is up.
    pub fn focus_session(&self) -> Result<Option<crate::focus::FocusSession>> {
        Ok(self
            .conn
            .query_row(
                "SELECT mode, started_at, ends_at FROM focus_state WHERE id = 1",
                [],
                |row| {
                    Ok(crate::focus::FocusSession {
                        mode: row.get(0)?,
                        started_at: row.get(1)?,
                        ends_at: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Start `session`, in place of the one running.
    pub fn set_focus_session(&self, session: &crate::focus::FocusSession) -> Result<()> {
        self.conn.execute(
            "INSERT INTO focus_state (id, mode, started_at, ends_at) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET mode = excluded.mode,
                 started_at = excluded.started_at, ends_at = excluded.ends_at",
            params![session.mode, session.started_at, session.ends_at],
        )?;
        Ok(())
    }

    /// End the focus that started at `started_at`, or whichever runs. One
    /// started since, by another Orb on this database, is left running.
    pub fn clear_focus_session(&self, started_at: Option<i64>) -> Result<()> {
        self.conn.execute(
            "DELETE FROM focus_state WHERE id = 1 AND (?1 IS NULL OR started_at = ?1)",
            params![started_at],
        )?;
        Ok(())
    }

    /// Unread mail across every account's INBOX, for the Dock badge.
    pub fn inbox_unread_count(&self) -> Result<u32> {
        // The inboxes first, so the count runs on the folder index rather
        // than over every message of every folder.
        Ok(self.conn.query_row(
            r#"SELECT COUNT(*) FROM messages
               WHERE folder_id IN (SELECT id FROM folders WHERE UPPER(name) = 'INBOX')
                 AND (flags IS NULL OR flags NOT LIKE '%Seen%')"#,
            [],
            |r| r.get(0),
        )?)
    }

    /// Unread mail in the folder named `folder` — INBOX in any case — of
    /// `account`, or of every account.
    pub fn unread_count_in(&self, account: Option<&str>, folder: &str) -> Result<u32> {
        Ok(self.conn.query_row(
            r#"SELECT COUNT(*) FROM messages
               WHERE folder_id IN (
                   SELECT id FROM folders
                   WHERE (?1 IS NULL OR account_name = ?1)
                     AND (name = ?2 OR (UPPER(?2) = 'INBOX' AND UPPER(name) = 'INBOX')))
                 AND (flags IS NULL OR flags NOT LIKE '%Seen%')"#,
            params![account, folder],
            |r| r.get(0),
        )?)
    }

    /// Store a body fetched by the background worker.
    ///
    /// Only the text is written: the worker never downloads the full source, so
    /// there is no `rfc822` to record and `has_attachments` is already known
    /// from the message's shape.
    pub fn set_message_body_text(&self, message_id: i64, body_text: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO message_bodies (message_id, body_text) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET body_text = excluded.body_text",
            params![message_id, body_text],
        )?;
        Ok(())
    }

    /// Record what a message is made of, for one synced before Orb asked.
    pub fn set_message_shape(
        &self,
        message_id: i64,
        shape: &crate::imap::structure::MessageShape,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE messages
             SET text_part = ?1, html_part = ?2, text_encoding = ?3, text_bytes = ?4,
                 has_attachments = ?5
             WHERE id = ?6",
            params![
                shape.text_part,
                shape.html_part,
                shape.text_encoding,
                shape.text_bytes,
                i32::from(shape.has_attachments),
                message_id
            ],
        )?;
        Ok(())
    }

    /// Record that the server's description of a message could not be parsed.
    ///
    /// Marks the shape as known — with no parts — so the message is not
    /// selected again on every pass, while keeping the reason, which is what
    /// separates it from a message that simply has no text to fetch.
    pub fn mark_shape_unreadable(&self, message_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages
             SET shape_unreadable = 1, has_attachments = COALESCE(has_attachments, 0)
             WHERE id = ?1",
            params![message_id],
        )?;
        Ok(())
    }

    /// How many messages the server described unreadably.
    pub fn unreadable_shape_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE shape_unreadable = 1",
            [],
            |r| r.get(0),
        )?)
    }

    /// Messages whose shape is not known yet, newest first.
    ///
    /// `has_attachments` is the marker: it is set for every shaped message,
    /// including one with no text part at all, so a message that genuinely has
    /// no body is not selected again on every pass.
    pub fn messages_without_shape(&self, folder_id: i64, limit: usize) -> Result<Vec<(i64, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid FROM messages
             WHERE folder_id = ?1 AND has_attachments IS NULL
             ORDER BY date_unix DESC NULLS LAST
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![folder_id, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many messages still lack a shape, and how many exist.
    pub fn pending_shape_count(&self) -> Result<(i64, i64)> {
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE has_attachments IS NULL",
            [],
            |r| r.get(0),
        )?;
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        Ok((pending, total))
    }

    /// Messages whose stored addresses carry no display name.
    ///
    /// Only useful while filling in names that were never captured; a message
    /// synced now keeps its names. Returns `(id, uid)` per folder.
    pub fn messages_without_sender_name(
        &self,
        folder_id: i64,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<(i64, u32)>> {
        // Walked by id rather than by "still has no name", because a message
        // whose sender genuinely has no display name never acquires one and
        // would be handed back on every pass forever.
        let mut stmt = self.conn.prepare(
            "SELECT id, uid FROM messages
             WHERE folder_id = ?1 AND id > ?2
               AND from_addr IS NOT NULL AND from_addr NOT LIKE '%<%'
             ORDER BY id
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![folder_id, after_id, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace a message's stored addresses.
    pub fn set_message_addresses(
        &self,
        message_id: i64,
        from: Option<&str>,
        to: Option<&str>,
        cc: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET from_addr = COALESCE(?1, from_addr),
                                 to_addr   = COALESCE(?2, to_addr),
                                 cc_addr   = COALESCE(?3, cc_addr)
             WHERE id = ?4",
            params![from, to, cc, message_id],
        )?;
        Ok(())
    }

    /// Whether the message's structure says it has an HTML part.
    ///
    /// Distinguishes "no HTML stored because there is none" from "no HTML
    /// stored because only the text part was ever fetched".
    pub fn message_has_html_part(&self, message_id: i64) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT html_part IS NOT NULL FROM messages WHERE id = ?1",
            params![message_id],
            |r| r.get(0),
        )?)
    }

    /// The account and folder a message belongs to.
    pub fn folder_of_message(&self, message_id: i64) -> Result<Option<(String, String)>> {
        let found = self.conn.query_row(
            "SELECT f.account_name, f.name
             FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE m.id = ?1",
            params![message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        match found {
            Ok(pair) => Ok(Some(pair)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Messages needing an embedding, newest first.
    ///
    /// Anything with text and no vector. Newest-first for the same reason the
    /// body backfill is: over a long run, the mail worth finding soonest is the
    /// mail nearest to now.
    pub fn messages_without_embedding(&self, limit: usize) -> Result<Vec<EmbeddableMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.subject, b.body_text
             FROM messages m
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE (m.subject IS NOT NULL OR m.has_body_text = 1)
               -- Not while a body is still coming. Embedding the subject alone
               -- and never revisiting it would leave the message permanently
               -- indexed by one line of the text it actually has.
               AND (m.has_body_text = 1 OR m.text_part IS NULL)
               AND NOT EXISTS (SELECT 1 FROM message_vectors v WHERE v.message_id = m.id)
             ORDER BY m.date_unix DESC NULLS LAST
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Store a message's packed embedding, replacing any it already had.
    pub fn set_message_embedding(&self, message_id: i64, packed: &[u8]) -> Result<()> {
        self.conn.execute(
            "DELETE FROM message_vectors WHERE message_id = ?1",
            params![message_id],
        )?;
        self.conn.execute(
            "INSERT INTO message_vectors(message_id, embedding) VALUES (?1, vec_bit(?2))",
            params![message_id, packed],
        )?;
        Ok(())
    }

    /// How many messages have text but no embedding, and how many have one.
    pub fn embedding_progress(&self) -> Result<(i64, i64)> {
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages m
             WHERE (m.subject IS NOT NULL OR m.has_body_text = 1)
               AND (m.has_body_text = 1 OR m.text_part IS NULL)
               AND NOT EXISTS (SELECT 1 FROM message_vectors v WHERE v.message_id = m.id)",
            [],
            |r| r.get(0),
        )?;
        let indexed: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM message_vectors", [], |r| r.get(0))?;
        Ok((pending, indexed))
    }

    /// The `k` messages whose meaning is closest to `packed`, nearest first.
    ///
    /// Only the ids and their text: the caller re-scores them at full precision,
    /// so what comes back here is a shortlist, not an answer.
    pub fn nearest_messages(&self, packed: &[u8], k: usize) -> Result<Vec<EmbeddableMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.subject, b.body_text
             FROM message_vectors v
             JOIN messages m ON m.id = v.message_id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE v.embedding MATCH vec_bit(?1) AND k = ?2",
        )?;
        let rows = stmt.query_map(params![packed, k as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Messages whose body has not been fetched yet, newest first.
    ///
    /// Newest-first matters: the backfill runs for a long time over a large
    /// mailbox, and the mail worth searching soonest is the mail nearest to now.
    pub fn pending_bodies(
        &self,
        folder_id: i64,
        limit: usize,
    ) -> Result<Vec<crate::tasks::bodies::PendingBody>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, text_part
             FROM messages
             WHERE folder_id = ?1 AND has_body_text IS NULL AND text_part IS NOT NULL
             ORDER BY date_unix DESC NULLS LAST
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![folder_id, limit as i64], |row| {
            Ok(crate::tasks::bodies::PendingBody {
                message_id: row.get(0)?,
                uid: row.get(1)?,
                part: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The newest `window` messages of a folder that have an HTML part not
    /// stored yet: the ones most likely to be opened next, and which would
    /// otherwise wait on the server when they are.
    pub fn pending_recent_html(
        &self,
        folder_id: i64,
        window: usize,
        limit: usize,
    ) -> Result<Vec<crate::tasks::bodies::PendingBody>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.uid, m.html_part
             FROM (SELECT id, uid, html_part, date_unix FROM messages
                   WHERE folder_id = ?1
                   ORDER BY date_unix DESC NULLS LAST
                   LIMIT ?2) m
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.html_part IS NOT NULL AND b.body_html IS NULL
             ORDER BY m.date_unix DESC NULLS LAST
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![folder_id, window as i64, limit as i64], |row| {
            Ok(crate::tasks::bodies::PendingBody {
                message_id: row.get(0)?,
                uid: row.get(1)?,
                part: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Store a message's HTML rendering, leaving its text and source alone.
    pub fn set_message_body_html(&self, message_id: i64, body_html: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO message_bodies (message_id, body_html) VALUES (?1, ?2)
             ON CONFLICT(message_id) DO UPDATE SET body_html = excluded.body_html",
            params![message_id, body_html],
        )?;
        Ok(())
    }

    /// How many messages still need a body, for progress reporting.
    pub fn pending_body_count(&self) -> Result<(i64, i64)> {
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE has_body_text IS NULL AND text_part IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE text_part IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok((pending, total))
    }

    /// Update message body fields (body_text, body_html, rfc822)
    ///
    /// Used for on-demand body fetching when viewing message detail.
    pub fn update_message_body(
        &self,
        message_id: i64,
        body_text: Option<&str>,
        body_html: Option<&str>,
        rfc822: Option<&[u8]>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO message_bodies (message_id, body_text, body_html, rfc822)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(message_id) DO UPDATE SET
                 body_text = excluded.body_text,
                 body_html = excluded.body_html,
                 rfc822 = excluded.rfc822",
            params![message_id, body_text, body_html, rfc822],
        )?;
        self.conn.execute(
            "UPDATE messages SET has_attachments = ?1 WHERE id = ?2",
            params![
                // Computed once here so the message list never has to parse.
                rfc822
                    .map(crate::attachments::has_attachments)
                    .map(i32::from),
                message_id
            ],
        )?;
        Ok(())
    }

    /// Try to acquire sync lock for a folder
    ///
    /// Returns true if we got the lock, false if another instance is syncing.
    /// Automatically cleans up stale locks older than SYNC_LOCK_TIMEOUT_SECONDS.
    ///
    /// Note: This mechanism works best on local filesystems. On network filesystems
    /// (NFS, SMB), SQLite's locking guarantees may be weaker.
    pub fn try_acquire_sync_lock(&mut self, account: &str, folder: &str) -> Result<bool> {
        Ok(self.acquire_sync_lock(account, folder)?.acquired())
    }

    /// [`Self::try_acquire_sync_lock`], saying who holds the lock when it is
    /// held, so that a skipped folder can be reported with its reason.
    pub fn acquire_sync_lock(&mut self, account: &str, folder: &str) -> Result<LockAttempt> {
        let normalized_folder = normalize_folder_name(folder);

        self.transaction(|tx| {
            // Clean up stale locks (older than timeout + clock skew buffer)
            // The buffer accounts for clock differences between machines
            let cleanup_threshold = SYNC_LOCK_TIMEOUT_SECONDS + CLOCK_SKEW_BUFFER_SECONDS;
            let cleanup_sql = format!(
                "DELETE FROM sync_locks
                 WHERE datetime(locked_at) < datetime('now', '-{} seconds')",
                cleanup_threshold
            );
            tx.execute(&cleanup_sql, [])?;

            // Get unique lock identifier (hostname:pid for cross-machine uniqueness)
            let lock_holder = get_lock_holder();

            // Try to acquire lock (using normalized folder name)
            match tx.execute(
                "INSERT INTO sync_locks (account_name, folder_name, locked_at, lock_holder)
                 VALUES (?1, ?2, datetime('now'), ?3)",
                params![account, normalized_folder, lock_holder],
            ) {
                Ok(_) => Ok(LockAttempt::Acquired),
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    let holder: String = tx
                        .query_row(
                            "SELECT lock_holder FROM sync_locks
                             WHERE account_name = ?1 AND folder_name = ?2",
                            params![account, normalized_folder],
                            |row| row.get(0),
                        )
                        .unwrap_or_else(|_| "unknown".to_string());
                    Ok(LockAttempt::Held { holder })
                }
                Err(e) => Err(e.into()),
            }
        })
    }

    /// Break the locks left on this host by processes that no longer exist.
    ///
    /// A lock survives its holder when the process is killed mid-sync, and
    /// until it ages out — six minutes — every instance skips that folder
    /// and reports it synced. The holder is `hostname:pid:uuid`, so a lock
    /// from this host whose process is gone can be told from one held by a
    /// second instance that is still running. Locks from other hosts are
    /// left alone: their processes cannot be asked.
    ///
    /// `alive` answers whether a process id on this host is still running.
    /// Returns how many locks were broken.
    pub fn break_dead_locks(&mut self, alive: impl Fn(u32) -> bool) -> Result<usize> {
        let hostname = local_hostname();
        let own_pid = std::process::id();

        self.transaction(|tx| {
            let mut stmt =
                tx.prepare("SELECT account_name, folder_name, lock_holder FROM sync_locks")?;
            let held: Vec<(String, String, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);

            let mut broken = 0;
            for (account, folder, holder) in held {
                let Some((host, pid)) = parse_lock_holder(&holder) else {
                    continue;
                };
                if host != hostname || pid == own_pid || alive(pid) {
                    continue;
                }
                broken += tx.execute(
                    "DELETE FROM sync_locks
                     WHERE account_name = ?1 AND folder_name = ?2 AND lock_holder = ?3",
                    params![account, folder, holder],
                )?;
                crate::logger::warn(&format!(
                    "sync lock on {account}/{folder} was left by process {pid}, which is gone; \
                     broken"
                ));
            }
            Ok(broken)
        })
    }

    /// Release sync lock for a folder
    ///
    /// Only releases the lock if we are the lock holder.
    pub fn release_sync_lock(&mut self, account: &str, folder: &str) -> Result<()> {
        let normalized_folder = normalize_folder_name(folder);
        let lock_holder = get_lock_holder();

        self.transaction(|tx| {
            tx.execute(
                "DELETE FROM sync_locks
                 WHERE account_name = ?1 AND folder_name = ?2 AND lock_holder = ?3",
                params![account, normalized_folder, lock_holder],
            )?;
            Ok(())
        })
    }

    /// Renew sync lock to prevent it from becoming stale during long operations
    ///
    /// Updates the locked_at timestamp if we are the current lock holder.
    /// Returns Ok(true) if the lock was renewed, Ok(false) if we don't hold the lock.
    pub fn renew_sync_lock(&mut self, account: &str, folder: &str) -> Result<bool> {
        let normalized_folder = normalize_folder_name(folder);
        let lock_holder = get_lock_holder();

        self.transaction(|tx| {
            let rows_updated = tx.execute(
                "UPDATE sync_locks
                 SET locked_at = datetime('now')
                 WHERE account_name = ?1 AND folder_name = ?2 AND lock_holder = ?3",
                params![account, normalized_folder, lock_holder],
            )?;
            Ok(rows_updated > 0)
        })
    }

    /// Release all sync locks held by this instance
    ///
    /// This should be called during graceful shutdown to clean up any locks
    /// that may have been acquired during sync operations.
    /// Returns the number of locks released.
    pub fn release_all_locks(&mut self) -> Result<usize> {
        let lock_holder = get_lock_holder();

        self.transaction(|tx| {
            let count = tx.execute(
                "DELETE FROM sync_locks WHERE lock_holder = ?1",
                params![lock_holder],
            )?;
            Ok(count)
        })
    }

    // ========== Outbox ==========

    /// Queue a server write to be carried out when the server is reachable.
    /// Returns the item's id.
    pub fn enqueue_outbox(&self, account: &str, kind: &str, payload: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO outbox (account, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![account, kind, payload, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The account's queued writes, oldest first.
    ///
    /// Order matters: a flag set and then cleared must reach the server in
    /// that order, so the queue is drained in sequence and stops at the first
    /// item that cannot be carried out. There is no retry delay: the queue is
    /// drained when a connection has just been proven to work, and never
    /// otherwise, so holding an item back would only make it miss that.
    ///
    /// A send that has failed for good is not among them: it waits for the
    /// user, not for the server.
    pub fn outbox_queued(&self, account: &str) -> Result<Vec<OutboxItem>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {OUTBOX_COLUMNS} FROM outbox
             WHERE account = ?1 AND failed_at IS NULL
             ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![account], OutboxItem::from_row)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// How many writes are still to be carried out, for the account or for
    /// all of them. Failed sends do not count: no drain will carry them out.
    pub fn outbox_len(&self, account: Option<&str>) -> Result<usize> {
        let count: i64 = match account {
            Some(account) => self.conn.query_row(
                "SELECT COUNT(*) FROM outbox WHERE account = ?1 AND failed_at IS NULL",
                params![account],
                |row| row.get(0),
            )?,
            None => self.conn.query_row(
                "SELECT COUNT(*) FROM outbox WHERE failed_at IS NULL",
                [],
                |row| row.get(0),
            )?,
        };
        Ok(count as usize)
    }

    /// Every queued message of every account, waiting or failed, oldest
    /// first: what the Outbox shows.
    pub fn outbox_sends(&self) -> Result<Vec<OutboxItem>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {OUTBOX_COLUMNS} FROM outbox WHERE kind = 'send' ORDER BY id"
        ))?;
        let rows = stmt.query_map([], OutboxItem::from_row)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// One queued message, waiting or failed.
    pub fn outbox_send(&self, id: i64) -> Result<Option<OutboxItem>> {
        self.conn
            .query_row(
                &format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE id = ?1 AND kind = 'send'"),
                params![id],
                OutboxItem::from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// The server refused the queued message: keep it, with the reason, and
    /// leave it out of every drain until [`Self::outbox_retry`].
    pub fn outbox_mark_failed(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET attempts = attempts + 1, last_error = ?2, failed_at = ?3
             WHERE id = ?1",
            params![id, error, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Put a failed message back in line for the next drain. Returns whether
    /// there was such a message.
    pub fn outbox_retry(&self, id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE outbox SET failed_at = NULL WHERE id = ?1 AND kind = 'send'",
            params![id],
        )?;
        Ok(changed > 0)
    }

    /// Drop a queued message unsent. Returns whether there was one; other
    /// kinds of queued write are left alone.
    pub fn outbox_discard_send(&self, id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM outbox WHERE id = ?1 AND kind = 'send'",
            params![id],
        )?;
        Ok(changed > 0)
    }

    /// The write reached the server; forget it.
    pub fn outbox_done(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// The write failed and stays queued; what went wrong is kept for the
    /// log and for whoever asks why the queue is not moving.
    pub fn outbox_failed(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }

    /// Add or remove one flag on the stored copies of these messages, before
    /// the server hears of it.
    ///
    /// Returns the UIDs that changed, which is what undoing the change has to
    /// put back. Flags are stored as a JSON list in wire form; a flag already
    /// in the state asked for is left alone.
    pub fn set_flag_locally(
        &mut self,
        folder_id: i64,
        uids: &[u32],
        flag: &str,
        add: bool,
    ) -> Result<Vec<u32>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut changed = Vec::new();
        {
            let mut read =
                tx.prepare("SELECT flags FROM messages WHERE folder_id = ?1 AND uid = ?2")?;
            let mut write =
                tx.prepare("UPDATE messages SET flags = ?1 WHERE folder_id = ?2 AND uid = ?3")?;
            for uid in uids {
                let raw: Option<String> =
                    match read.query_row(params![folder_id, uid], |row| row.get(0)) {
                        Ok(raw) => raw,
                        Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                        Err(e) => return Err(e.into()),
                    };
                let mut flags: Vec<String> =
                    serde_json::from_str(raw.as_deref().unwrap_or("[]")).unwrap_or_default();
                let present = flags.iter().any(|f| f.eq_ignore_ascii_case(flag));
                if present == add {
                    continue;
                }
                if add {
                    flags.push(flag.to_string());
                } else {
                    flags.retain(|f| !f.eq_ignore_ascii_case(flag));
                }
                let encoded = serde_json::to_string(&flags).unwrap_or_else(|_| "[]".to_string());
                if write.execute(params![encoded, folder_id, uid])? > 0 {
                    changed.push(*uid);
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    // ========== Contact Methods ==========

    /// Upsert a contact (insert or update on conflict)
    ///
    /// If is_sent is true, increments sent_count and updates last_sent.
    /// If is_sent is false, increments received_count and updates last_received.
    pub fn upsert_contact(
        &self,
        email: &str,
        display_name: Option<&str>,
        is_sent: bool,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();

        if is_sent {
            self.conn.execute(
                "INSERT INTO contacts (email, display_name, sent_count, last_sent)
                 VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(email) DO UPDATE SET
                    display_name = COALESCE(?2, display_name),
                    sent_count = sent_count + 1,
                    last_sent = ?3",
                params![email, display_name, now],
            )?;
        } else {
            self.conn.execute(
                "INSERT INTO contacts (email, display_name, received_count, last_received)
                 VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(email) DO UPDATE SET
                    display_name = COALESCE(?2, display_name),
                    received_count = received_count + 1,
                    last_received = ?3",
                params![email, display_name, now],
            )?;
        }

        Ok(())
    }

    /// Contacts whose name or address contains `query`.
    ///
    /// Deliberately the widest match the scorer recognises, and no narrower:
    /// ranking is done in one place, in Rust, so that what counts as a match is
    /// not split between here and there where the two would drift apart.
    ///
    /// A LIKE with a leading wildcard cannot use an index, so this is a scan.
    /// At the size an address book reaches — one row per correspondent rather
    /// than one per message — that costs a millisecond or two, which is well
    /// inside a keystroke.
    pub fn match_contacts(&self, query: &str, limit: usize) -> Result<Vec<Contact>> {
        // The wildcards are ours; anything in the query that looks like one is
        // a literal, or a search for `%` returns the entire address book.
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");

        let mut stmt = self.conn.prepare(
            "SELECT id, email, display_name, sent_count, received_count, last_sent, last_received, is_manual
             FROM contacts
             WHERE email LIKE ?1 ESCAPE '\\' OR display_name LIKE ?1 ESCAPE '\\'
             ORDER BY sent_count DESC, received_count DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![pattern, limit], Self::read_contact)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn read_contact(row: &rusqlite::Row<'_>) -> rusqlite::Result<Contact> {
        Ok(Contact {
            id: row.get(0)?,
            email: row.get(1)?,
            display_name: row.get(2)?,
            sent_count: row.get(3)?,
            received_count: row.get(4)?,
            last_sent: row.get(5)?,
            last_received: row.get(6)?,
            is_manual: row.get(7)?,
        })
    }

    /// The correspondents written to most, most recently first among equals.
    ///
    /// Only those actually written to. Everyone who has ever sent mail is a
    /// list of strangers, and it would bury the few people worth offering.
    pub fn most_written_to(&self, limit: usize) -> Result<Vec<Contact>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, email, display_name, sent_count, received_count, last_sent, last_received, is_manual
             FROM contacts
             WHERE sent_count > 0
             ORDER BY sent_count DESC, last_sent DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], Self::read_contact)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// How many correspondents the address book holds.
    pub fn contact_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))?)
    }

    /// The largest message id the address book has already learned from.
    ///
    /// `None` when it has never been built. Mail arrives with larger ids, so
    /// anything above this mark is unlearned — which is what makes the update
    /// after the first one cost milliseconds instead of seconds.
    ///
    /// Deletions do not lower the mark and so do not trigger a rebuild, which
    /// is the right answer: someone whose mail you deleted is still someone you
    /// corresponded with.
    fn contacts_built_through(&self) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT value FROM sync_meta WHERE key = 'contacts_built_through'",
                [],
                |row| row.get(0),
            )
            .ok()
    }

    /// The largest message id there is.
    fn latest_message_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |row| {
                row.get(0)
            })?)
    }

    /// Record which mail the address book has now seen.
    fn note_contacts_built_through(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_meta (key, value) VALUES ('contacts_built_through', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![id],
        )?;
        Ok(())
    }

    /// Bring the address book up to date with the mail.
    ///
    /// Full the first time and incremental afterwards, because reading every
    /// message takes seconds on a large mailbox and mail arrives constantly —
    /// paying that on every launch to learn about four new messages is the kind
    /// of cost that never shows up in a test and is felt every day.
    ///
    /// Returns how many correspondents the book holds, or `None` when there was
    /// nothing new to learn.
    pub fn learn_correspondents(&mut self, sent_folders: &[i64]) -> Result<Option<usize>> {
        let built_through = self.contacts_built_through();
        let latest = self.latest_message_id()?;
        if built_through.is_some_and(|mark| mark >= latest) {
            return Ok(None);
        }

        self.rebuild_contacts(sent_folders, built_through)?;
        self.note_contacts_built_through(latest)?;
        Ok(Some(self.contact_count()? as usize))
    }

    /// Rebuild the address book from the mail.
    ///
    /// `sent_folders` names the folders whose messages were written by the user
    /// rather than to them. That distinction cannot be made from a column: the
    /// `to` of a message in the inbox is the user themselves, and counting it as
    /// somebody they write to — which the previous version of this did — puts
    /// the user at the top of their own completion list and everyone they
    /// actually correspond with below it.
    ///
    /// Addresses are parsed rather than stored as written, so `Nina Berger
    /// <nina.berger@example.com>` becomes a name and an address instead of one
    /// string that answers to neither a search for the name nor one for the
    /// address.
    ///
    /// `since` is the largest message id already accounted for; messages up to
    /// and including it are skipped. Passing `None` starts again from nothing,
    /// discarding what was learned before — anything else would double-count
    /// every message on the second pass.
    ///
    /// Rows added by hand are left alone either way.
    pub fn rebuild_contacts(&mut self, sent_folders: &[i64], since: Option<i64>) -> Result<usize> {
        use std::collections::HashMap;

        /// What is known about one correspondent while the tally runs.
        #[derive(Default)]
        struct Tally {
            name: Option<String>,
            /// When that name was seen, so the most recent one wins: people
            /// change employers, and the old name is the wrong one to offer.
            name_date: Option<String>,
            sent: i64,
            received: i64,
            last_sent: Option<String>,
            last_received: Option<String>,
        }

        impl Tally {
            fn observe(&mut self, name: Option<&str>, date: Option<&str>, sent: bool) {
                if let Some(name) = name.filter(|n| !n.trim().is_empty())
                    && date >= self.name_date.as_deref()
                {
                    self.name = Some(name.to_string());
                    self.name_date = date.map(String::from);
                }
                let (count, last) = if sent {
                    (&mut self.sent, &mut self.last_sent)
                } else {
                    (&mut self.received, &mut self.last_received)
                };
                *count += 1;
                if date > last.as_deref() {
                    *last = date.map(String::from);
                }
            }
        }

        let sent: std::collections::HashSet<i64> = sent_folders.iter().copied().collect();
        let mut book: HashMap<String, Tally> = HashMap::new();

        {
            let mut stmt = self.conn.prepare(
                "SELECT folder_id, from_addr, to_addr, cc_addr, date FROM messages
                 WHERE id > ?1",
            )?;
            let mut rows = stmt.query(params![since.unwrap_or(0)])?;
            while let Some(row) = rows.next()? {
                let folder_id: i64 = row.get(0)?;
                let from: Option<String> = row.get(1)?;
                let to: Option<String> = row.get(2)?;
                let cc: Option<String> = row.get(3)?;
                let date: Option<String> = row.get(4)?;
                let outgoing = sent.contains(&folder_id);

                // Who a message is addressed to is somebody the user wrote to —
                // but only if the user wrote the message.
                for field in [to, cc] {
                    for r in crate::contacts::parse_list(field.as_deref().unwrap_or(""))
                        .into_iter()
                        .filter(crate::contacts::Recipient::is_address)
                    {
                        book.entry(r.address).or_default().observe(
                            r.name.as_deref(),
                            date.as_deref(),
                            outgoing,
                        );
                    }
                }
                // The sender of a message the user received is a correspondent;
                // the sender of one they sent is the user themselves.
                if !outgoing
                    && let Some(r) = crate::contacts::parse_one(from.as_deref().unwrap_or(""))
                        .filter(crate::contacts::Recipient::is_address)
                {
                    book.entry(r.address).or_default().observe(
                        r.name.as_deref(),
                        date.as_deref(),
                        false,
                    );
                }
            }
        }

        let total = book.len();
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        if since.is_none() {
            tx.execute("DELETE FROM contacts WHERE is_manual = 0", [])?;
        }
        {
            let mut insert = tx.prepare(
                "INSERT INTO contacts (email, display_name, sent_count, received_count,
                                       last_sent, last_received)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(email) DO UPDATE SET
                    display_name = COALESCE(excluded.display_name, display_name),
                    sent_count = sent_count + excluded.sent_count,
                    received_count = received_count + excluded.received_count,
                    last_sent = MAX(COALESCE(last_sent, ''), COALESCE(excluded.last_sent, '')),
                    last_received = MAX(COALESCE(last_received, ''),
                                        COALESCE(excluded.last_received, ''))",
            )?;
            for (address, tally) in book {
                insert.execute(params![
                    address,
                    tally.name,
                    tally.sent,
                    tally.received,
                    tally.last_sent,
                    tally.last_received,
                ])?;
            }
        }
        tx.commit()?;

        Ok(total)
    }
}

/// Contact record from database
#[derive(Debug, Clone)]
pub struct Contact {
    pub id: i64,
    pub email: String,
    pub display_name: Option<String>,
    pub sent_count: i64,
    pub received_count: i64,
    pub last_sent: Option<String>,
    pub last_received: Option<String>,
    pub is_manual: bool,
}

/// Normalize folder name for consistent lock identification
///
/// IMAP folder names may be case-sensitive or case-insensitive depending on
/// the server. We normalize to uppercase to ensure consistent locking behavior.
fn normalize_folder_name(folder: &str) -> String {
    folder.to_uppercase()
}

/// Instance UUID generated once per process
static INSTANCE_UUID: OnceLock<String> = OnceLock::new();

/// Get unique lock holder identifier (hostname:pid:uuid)
///
/// This ensures uniqueness across different machines and process restarts.
/// The UUID prevents issues with PID reuse after crashes.
fn get_lock_holder() -> String {
    let uuid = INSTANCE_UUID.get_or_init(|| uuid::Uuid::new_v4().to_string());
    format!("{}:{}:{}", local_hostname(), std::process::id(), uuid)
}

fn local_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Two hits in `sort` order, as [`MessageSort::as_sql`] orders rows: a
/// message without a date last either way, ties by id.
fn compare_hits(
    sort: MessageSort,
    a: &SearchResultItem,
    b: &SearchResultItem,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (&a.message, &b.message);
    let by_date = |newest: bool| {
        let date = |m: &MessageListItem| m.date.as_deref().and_then(Database::parse_date_to_unix);
        match (date(a), date(b)) {
            (Some(x), Some(y)) if newest => y.cmp(&x).then(b.id.cmp(&a.id)),
            (Some(x), Some(y)) => x.cmp(&y).then(a.id.cmp(&b.id)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => b.id.cmp(&a.id),
        }
    };
    match sort {
        MessageSort::DateNewest => by_date(true),
        MessageSort::DateOldest => by_date(false),
        MessageSort::SubjectAsc => a.subject.cmp(&b.subject).then(b.id.cmp(&a.id)),
        MessageSort::SubjectDesc => b.subject.cmp(&a.subject).then(b.id.cmp(&a.id)),
        MessageSort::SenderAsc => a.from.cmp(&b.from).then(b.id.cmp(&a.id)),
        MessageSort::SenderDesc => b.from.cmp(&a.from).then(b.id.cmp(&a.id)),
    }
}

/// The host and process of a lock holder, from `hostname:pid:uuid`.
fn parse_lock_holder(holder: &str) -> Option<(&str, u32)> {
    let mut parts = holder.rsplitn(3, ':');
    let _uuid = parts.next()?;
    let pid = parts.next()?.parse().ok()?;
    let host = parts.next()?;
    Some((host, pid))
}

/// What a list needs to say about a message: its row id, subject and sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headline {
    pub id: i64,
    pub subject: Option<String>,
    pub from: Option<String>,
}

/// A message a sync brought, with where it lives.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NewMessage {
    pub id: i64,
    pub account: String,
    pub folder: String,
    pub from: Option<String>,
    pub subject: Option<String>,
}

/// What asking for a folder's sync lock came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockAttempt {
    Acquired,
    /// Somebody else has it: `hostname:pid:uuid`.
    Held {
        holder: String,
    },
}

impl LockAttempt {
    pub fn acquired(&self) -> bool {
        matches!(self, LockAttempt::Acquired)
    }
}

const OUTBOX_COLUMNS: &str =
    "id, account, kind, payload, attempts, last_error, created_at, failed_at";

/// One queued server write.
#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub id: i64,
    pub account: String,
    /// `flags`, `move` or `send`.
    pub kind: String,
    /// JSON, read by whoever handles the kind.
    pub payload: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339; set once a send has been refused for good.
    pub failed_at: Option<String>,
}

impl OutboxItem {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(OutboxItem {
            id: row.get(0)?,
            account: row.get(1)?,
            kind: row.get(2)?,
            payload: row.get(3)?,
            attempts: row.get(4)?,
            last_error: row.get(5)?,
            created_at: row.get(6)?,
            failed_at: row.get(7)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snippet_is_the_opening_of_the_text_on_one_line() {
        let text = "Hi Sam,\n\n  attached is the  first draft.\n\nOn Tue, Sara wrote:\n> old\n> text\n-- \nSara";
        assert_eq!(snippet(text), "Hi Sam, attached is the first draft.");
        let long = "word ".repeat(100);
        assert_eq!(snippet(&long).chars().count(), SNIPPET_CHARS);
        assert_eq!(snippet("> only quoted\n"), "");
    }

    /// A database with one received folder and one sent folder, ready to be
    /// filled with mail.
    fn mailbox() -> Database {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        db.get_or_create_folder("Work", "INBOX").expect("inbox");
        db.get_or_create_folder("Work", "Sent").expect("sent");
        db
    }

    fn folder_id(db: &Database, name: &str) -> i64 {
        db.get_folders_by_account("Work")
            .expect("folders")
            .into_iter()
            .find(|f| f.name == name)
            .and_then(|f| f.id)
            .expect("folder")
    }

    fn add_message(db: &Database, folder: i64, uid: u32, from: &str, to: &str, date: &str) {
        db.conn
            .execute(
                "INSERT INTO messages (folder_id, uid, from_addr, to_addr, date)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![folder, uid, from, to, date],
            )
            .expect("insert message");
    }

    fn contact(db: &Database, email: &str) -> Contact {
        db.match_contacts(email, 10)
            .expect("match")
            .into_iter()
            .find(|c| c.email == email)
            .unwrap_or_else(|| panic!("{email} is not in the address book"))
    }

    /// Flags are only worth re-checking now and then; every folder on every
    /// sync would be a fetch per folder on a server whose modseq never moves.
    #[test]
    fn a_flag_check_is_due_once_and_then_not_again() {
        let mut db = mailbox();
        let inbox = folder_id(&db, "INBOX");
        let minute = std::time::Duration::from_secs(60);

        // Never checked: due.
        assert!(db.flags_check_is_due(inbox, minute).expect("due"));

        db.note_flags_checked(inbox).expect("note");
        assert!(!db.flags_check_is_due(inbox, minute).expect("due"));

        // A zero interval means every sync, which is what a test or an
        // impatient setting would ask for.
        assert!(
            db.flags_check_is_due(inbox, std::time::Duration::ZERO)
                .expect("due")
        );

        // An unreadable timestamp is unknown, and unknown means check.
        db.conn
            .execute(
                "UPDATE folders SET flags_checked_at = 'nonsense' WHERE id = ?1",
                params![inbox],
            )
            .expect("scribble");
        assert!(db.flags_check_is_due(inbox, minute).expect("due"));
        let _ = &mut db;
    }

    /// What the reconciliation writes back.
    #[test]
    fn flags_are_replaced_only_where_they_differ() {
        let mut db = mailbox();
        let inbox = folder_id(&db, "INBOX");
        add_message(&db, inbox, 1, "a@x.com", "me@x.com", "2026-01-01T00:00:00Z");
        add_message(&db, inbox, 2, "b@x.com", "me@x.com", "2026-01-02T00:00:00Z");

        let changed = db
            .replace_flags(inbox, &[(1, vec!["\\Seen".to_string()])])
            .expect("replace");
        assert_eq!(changed, 1);

        let flags = db.folder_flags(inbox).expect("flags");
        assert_eq!(flags.get(&1).unwrap(), &vec!["\\Seen".to_string()]);
        // The message nobody mentioned is untouched.
        assert!(flags.get(&2).unwrap().is_empty());

        // Nothing to do is not an error.
        assert_eq!(db.replace_flags(inbox, &[]).expect("empty"), 0);
    }

    /// The distinction the previous version of this got backwards: who a
    /// message is addressed to is a correspondent only if the user wrote it.
    #[test]
    fn only_mail_the_user_sent_counts_as_writing_to_someone() {
        let mut db = mailbox();
        let inbox = folder_id(&db, "INBOX");
        let sent = folder_id(&db, "Sent");

        add_message(
            &db,
            inbox,
            1,
            "Ada Lovelace <ada@example.com>",
            "me@example.com",
            "2026-01-01T00:00:00Z",
        );
        add_message(
            &db,
            sent,
            2,
            "me@example.com",
            "Grace Hopper <grace@example.com>",
            "2026-01-02T00:00:00Z",
        );

        db.learn_correspondents(&[sent]).expect("learn");

        // Written to.
        assert_eq!(contact(&db, "grace@example.com").sent_count, 1);
        assert_eq!(contact(&db, "grace@example.com").received_count, 0);
        // Heard from.
        assert_eq!(contact(&db, "ada@example.com").received_count, 1);
        assert_eq!(contact(&db, "ada@example.com").sent_count, 0);
        // The user's own address appears as the recipient of the message they
        // received; that must not read as writing to themselves.
        assert_eq!(contact(&db, "me@example.com").sent_count, 0);
    }

    #[test]
    fn a_name_is_kept_beside_the_address() {
        let mut db = mailbox();
        let inbox = folder_id(&db, "INBOX");
        add_message(
            &db,
            inbox,
            1,
            "Ada Lovelace <ada@example.com>",
            "me@example.com",
            "2026-01-01T00:00:00Z",
        );
        db.learn_correspondents(&[]).expect("learn");
        assert_eq!(
            contact(&db, "ada@example.com").display_name.as_deref(),
            Some("Ada Lovelace")
        );
    }

    /// The most recent name wins: people change employers, and the old name is
    /// the wrong one to offer.
    #[test]
    fn the_newest_name_is_the_one_offered() {
        let mut db = mailbox();
        let inbox = folder_id(&db, "INBOX");
        add_message(
            &db,
            inbox,
            1,
            "Ada Byron <ada@example.com>",
            "me@x.com",
            "2020-01-01T00:00:00Z",
        );
        add_message(
            &db,
            inbox,
            2,
            "Ada Lovelace <ada@example.com>",
            "me@x.com",
            "2026-01-01T00:00:00Z",
        );
        db.learn_correspondents(&[]).expect("learn");
        assert_eq!(
            contact(&db, "ada@example.com").display_name.as_deref(),
            Some("Ada Lovelace")
        );
    }

    /// The update that runs on every launch. It must learn what is new without
    /// counting what it has already seen a second time.
    #[test]
    fn learning_again_counts_new_mail_only() {
        let mut db = mailbox();
        let inbox = folder_id(&db, "INBOX");
        add_message(
            &db,
            inbox,
            1,
            "Ada <ada@example.com>",
            "me@x.com",
            "2026-01-01T00:00:00Z",
        );

        assert_eq!(db.learn_correspondents(&[]).expect("first"), Some(2));
        assert_eq!(contact(&db, "ada@example.com").received_count, 1);

        // Nothing has arrived: there is nothing to do, and saying so is what
        // keeps a launch from re-reading the whole mailbox.
        assert_eq!(db.learn_correspondents(&[]).expect("second"), None);
        assert_eq!(contact(&db, "ada@example.com").received_count, 1);

        // Now something has.
        add_message(
            &db,
            inbox,
            2,
            "Ada <ada@example.com>",
            "me@x.com",
            "2026-02-01T00:00:00Z",
        );
        assert!(db.learn_correspondents(&[]).expect("third").is_some());
        assert_eq!(contact(&db, "ada@example.com").received_count, 2);
    }

    /// A half-typed recipient in a draft must not become a permanent suggestion.
    #[test]
    fn a_recipient_that_is_not_an_address_is_not_learned() {
        let mut db = mailbox();
        let sent = folder_id(&db, "Sent");
        add_message(
            &db,
            sent,
            1,
            "me@x.com",
            "foo, real@example.com",
            "2026-01-01T00:00:00Z",
        );
        db.learn_correspondents(&[sent]).expect("learn");

        assert!(db.match_contacts("foo", 10).expect("match").is_empty());
        assert_eq!(contact(&db, "real@example.com").sent_count, 1);
    }

    /// A search for `%` would otherwise return the entire address book.
    #[test]
    fn a_wildcard_in_a_query_is_a_literal() {
        let mut db = mailbox();
        let inbox = folder_id(&db, "INBOX");
        add_message(
            &db,
            inbox,
            1,
            "Ada <ada@example.com>",
            "me@x.com",
            "2026-01-01T00:00:00Z",
        );
        db.learn_correspondents(&[]).expect("learn");
        assert!(db.match_contacts("%", 10).expect("match").is_empty());
    }

    /// Contacts added by hand are not the mail's to delete.
    #[test]
    fn a_hand_added_contact_survives_a_rebuild() {
        let mut db = mailbox();
        db.conn
            .execute(
                "INSERT INTO contacts (email, display_name, is_manual) VALUES ('kept@x.com', 'Kept', 1)",
                [],
            )
            .expect("insert");
        db.rebuild_contacts(&[], None).expect("rebuild");
        assert_eq!(
            contact(&db, "kept@x.com").display_name.as_deref(),
            Some("Kept")
        );
    }

    #[test]
    fn test_normalize_date_rfc2822() {
        let date = "Wed, 9 Nov 2022 16:18:40 +0100";
        let result = Database::normalize_date_for_migration(date);
        assert!(result.is_some());
        assert!(result.unwrap().contains("T"));
    }

    #[test]
    fn test_normalize_date_with_timezone_name() {
        let date = "Wed, 9 Jul 2008 15:23:03 +0200 (CEST)";
        let result = Database::normalize_date_for_migration(date);
        assert!(result.is_some(), "Failed to parse: {}", date);
        let normalized = result.unwrap();
        assert!(normalized.contains("T"), "Not RFC3339: {}", normalized);
    }

    #[test]
    fn test_normalize_date_single_digit_hour() {
        let date = "Wed, 9 Apr 2008 9:33:14 +0200";
        let result = Database::normalize_date_for_migration(date);
        assert!(result.is_some(), "Failed to parse: {}", date);
    }

    #[test]
    fn test_normalize_date_no_timezone() {
        let date = "Wed, 6 Jan 2016 23:25:35";
        let result = Database::normalize_date_for_migration(date);
        assert!(result.is_some(), "Failed to parse: {}", date);
    }

    #[test]
    fn test_normalize_date_tz_with_colon() {
        let date = "Sun, 14 Aug 2016 13:35:19 +01:00";
        let result = Database::normalize_date_for_migration(date);
        assert!(result.is_some(), "Failed to parse: {}", date);
    }

    #[test]
    fn test_normalize_date_wrong_day_of_week() {
        // Feb 7, 2012 was actually a Tuesday, but email says Wednesday
        // Our parser should handle this by stripping the day-of-week
        let date = "Wed, 7 Feb 2012 12:59:23 +0200";
        let result = Database::normalize_date_for_migration(date);
        assert!(
            result.is_some(),
            "Failed to parse date with incorrect day-of-week: {}",
            date
        );
        let normalized = result.unwrap();
        assert!(
            normalized.contains("2012-02-07"),
            "Wrong date in: {}",
            normalized
        );
    }

    #[test]
    fn test_normalize_date_two_digit_day() {
        let date = "Wed, 07 Feb 2012 12:59:23 +0200";
        let result = Database::normalize_date_for_migration(date);
        assert!(result.is_some(), "Failed to parse: {}", date);
        let normalized = result.unwrap();
        assert!(
            normalized.contains("2012-02-07"),
            "Wrong date in: {}",
            normalized
        );
    }

    #[test]
    fn test_normalize_date_correct_day_of_week() {
        // Feb 7, 2012 was a Tuesday - this should work too
        let date = "Tue, 7 Feb 2012 12:59:23 +0200";
        let result = Database::normalize_date_for_migration(date);
        assert!(result.is_some(), "Failed to parse: {}", date);
        let normalized = result.unwrap();
        assert!(
            normalized.contains("2012-02-07"),
            "Wrong date in: {}",
            normalized
        );
    }

    /// There is one focus at a time, and ending one that another Orb has
    /// since replaced leaves the newer one running.
    #[test]
    fn a_focus_session_is_kept_replaced_and_ended() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        assert_eq!(db.focus_session().unwrap(), None);

        let first = crate::focus::FocusSession {
            mode: "deep".to_string(),
            started_at: 1_000,
            ends_at: Some(9_000),
        };
        db.set_focus_session(&first).unwrap();
        assert_eq!(db.focus_session().unwrap(), Some(first.clone()));

        let second = crate::focus::FocusSession {
            mode: "batched".to_string(),
            started_at: 2_000,
            ends_at: None,
        };
        db.set_focus_session(&second).unwrap();
        assert_eq!(db.focus_session().unwrap(), Some(second.clone()));

        db.clear_focus_session(Some(first.started_at)).unwrap();
        assert_eq!(db.focus_session().unwrap(), Some(second));
        db.clear_focus_session(None).unwrap();
        assert_eq!(db.focus_session().unwrap(), None);
    }

    /// Re-running a migration step whose `ADD COLUMN` already took effect must not
    /// fail with "duplicate column name". Before the fix this bricked the database:
    /// the `date_unix` ALTER was applied but the schema version was never bumped, so
    /// the next open re-ran the ALTER and errored out permanently.
    #[test]
    fn test_migration_add_column_is_idempotent() {
        let db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("initial schema init failed");

        // Sanity: fully migrated to the current version.
        assert_eq!(db.get_schema_version().unwrap(), SCHEMA_VERSION);
        assert!(
            Database::column_exists(&db.conn, "messages", "date_unix").unwrap(),
            "date_unix column should exist after migrations"
        );

        // Simulate an interrupted upgrade: the v5->v6 ALTER already committed
        // (date_unix exists) but the recorded version was rolled back to 5.
        db.set_schema_version(5).unwrap();

        // Re-running the migration must succeed (no duplicate-column error) and
        // bring the schema all the way back up to the current version.
        db.run_migrations(5)
            .expect("re-running migration after interrupted upgrade must not error");
        assert_eq!(db.get_schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// A database from before v35 keeps what its outbox holds, and every
    /// queued send in it starts out waiting, not failed.
    #[test]
    fn an_outbox_from_before_v35_gains_the_failed_mark() {
        let db = Database::new(":memory:").expect("db");
        db.conn
            .execute_batch(
                "CREATE TABLE outbox (
                     id INTEGER PRIMARY KEY,
                     account TEXT NOT NULL,
                     kind TEXT NOT NULL CHECK (kind IN ('flags', 'send', 'move')),
                     payload TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     attempts INTEGER NOT NULL DEFAULT 0,
                     last_error TEXT
                 );
                 INSERT INTO outbox (account, kind, payload, created_at, attempts, last_error)
                 VALUES ('A', 'send', '{}', '2026-10-28T10:29:36Z', 3, '535 refused');",
            )
            .unwrap();
        db.set_schema_version(34).unwrap();

        db.run_migrations(34).expect("migrate");
        assert_eq!(db.get_schema_version().unwrap(), SCHEMA_VERSION);
        let sends = db.outbox_sends().unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].attempts, 3);
        assert!(sends[0].failed_at.is_none());
        assert_eq!(db.outbox_len(Some("A")).unwrap(), 1);
    }

    /// From before v36, every room is fetched again: what it holds stays, and
    /// the next sync starts from the beginning of the history kept.
    #[test]
    fn chat_rooms_from_before_v36_are_fetched_again() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO chat_rooms (rid, kind, name, last_message_at, synced_until)
                 VALUES ('r', 'c', 'dev', 20, 20);",
            )
            .unwrap();
        db.set_schema_version(35).unwrap();

        db.run_migrations(35).expect("migrate");
        let (count, synced): (i64, Option<i64>) = db
            .conn
            .query_row(
                "SELECT count(*), max(synced_until) FROM chat_rooms",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, synced), (1, None));
    }

    /// A message the server describes unreadably must drop out of the work
    /// queue, or the backfill retries it on every pass forever — which is what
    /// a single `(NIL "ALTERNATE" ...)` structure did.
    /// The sync stores flags in wire form, `\Seen`, and a count that looked
    /// for `"Seen"` with the quote in front never found one: every message was
    /// unread to the sidebar, the Dock and the notifications.
    #[test]
    fn a_read_message_is_not_counted_as_unread() {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let mut read = crate::models::Message::new(inbox, 1);
        read.flags = vec!["\\Seen".to_string()];
        let unread = crate::models::Message::new(inbox, 2);
        db.insert_message(&read).expect("insert");
        db.insert_message(&unread).expect("insert");
        assert_eq!(db.get_folder_counts(inbox).unwrap(), (1, 2));
        assert_eq!(db.inbox_unread_count().unwrap(), 1);
        let listed = db.unread_message_list(inbox).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].uid, 2);
        assert_eq!(db.unread_messages_among(&[1, 2]).unwrap().len(), 1);
        assert_eq!(db.latest_unread_message().unwrap().unwrap().uid, 2);
    }

    /// The plan of a query, as SQLite would run it.
    fn plan(db: &Database, sql: &str) -> String {
        let mut stmt = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(3)).unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n")
    }

    /// The bodies and the source come before `flags` in a row, so a count
    /// that reads the rows reads every body it counts: on a real mailbox the
    /// Dock badge took four seconds that way, after every apply and sync.
    /// The unread counts are answered from an index, never from the rows.
    #[test]
    fn unread_counts_never_read_the_messages_themselves() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let folder = plan(
            &db,
            "SELECT COUNT(*) FROM messages WHERE folder_id = 1 \
             AND (flags IS NULL OR flags NOT LIKE '%Seen%')",
        );
        assert!(
            folder.contains("COVERING INDEX idx_messages_folder_flags"),
            "{folder}"
        );
        let inboxes = plan(
            &db,
            "SELECT COUNT(*) FROM messages \
             WHERE folder_id IN (SELECT id FROM folders WHERE UPPER(name) = 'INBOX') \
             AND (flags IS NULL OR flags NOT LIKE '%Seen%')",
        );
        assert!(!inboxes.contains("SCAN messages"), "{inboxes}");
        assert!(
            inboxes.contains("COVERING INDEX idx_messages_folder_flags"),
            "{inboxes}"
        );
    }

    /// The highlight badges list a folder's unread mail at every launch. Walked
    /// in date order, that read every row of every folder to look at its
    /// flags: five seconds on a cold 8 GB database, with the window waiting.
    /// The flags index picks out the unread rows, and only those are read.
    #[test]
    fn the_unread_list_reads_only_the_unread_rows() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let list = plan(&db, &UNREAD_MESSAGE_LIST.replace("?1", "1"));
        assert!(
            list.contains("USING INDEX idx_messages_folder_flags"),
            "{list}"
        );
    }

    #[test]
    fn an_unreadable_shape_is_recorded_and_not_asked_about_again() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let folder = db.insert_folder("Work", "INBOX").expect("folder");

        let message = Message {
            id: None,
            folder_id: folder,
            uid: 760402,
            message_id: Some("<broken@example.com>".to_string()),
            subject: Some("Has an unparseable structure".to_string()),
            from: Some("sender@example.com".to_string()),
            to: Some("me@example.com".to_string()),
            cc: None,
            date: Some("2026-03-01T10:00:00Z".to_string()),
            body_text: None,
            body_html: None,
            rfc822: None,
            size: Some(1),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        };
        let id = db.insert_message(&message).expect("insert");

        // Before: it is work waiting to be done.
        assert_eq!(
            db.messages_without_shape(folder, 10).unwrap(),
            vec![(id, 760402)]
        );

        db.mark_shape_unreadable(id).expect("mark");

        // After: no longer selected, by either pass.
        assert!(
            db.messages_without_shape(folder, 10).unwrap().is_empty(),
            "a marked message must not come back as needing a shape"
        );
        assert!(
            db.pending_bodies(folder, 10).unwrap().is_empty(),
            "and must not be queued for a body it has no part path for"
        );
        // But the reason survives, so these can be recovered later rather than
        // being indistinguishable from mail that simply has no text.
        assert_eq!(db.unreadable_shape_count().unwrap(), 1);
    }

    /// Only the newest messages of a folder are offered for an HTML prefetch,
    /// and one whose HTML is stored is not offered again.
    #[test]
    fn recent_html_is_the_newest_of_a_folder_still_without_it() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let folder = db.insert_folder("Work", "INBOX").expect("folder");

        let shape = crate::imap::structure::MessageShape {
            text_part: Some("1.1".to_string()),
            html_part: Some("1.2".to_string()),
            text_bytes: Some(10),
            text_encoding: None,
            has_attachments: false,
        };
        let ids: Vec<i64> = (1..=4u32)
            .map(|day| {
                db.insert_message(&Message {
                    id: None,
                    folder_id: folder,
                    uid: day,
                    message_id: Some(format!("<{day}@example.com>")),
                    subject: Some(format!("Day {day}")),
                    from: Some("sender@example.com".to_string()),
                    to: Some("me@example.com".to_string()),
                    cc: None,
                    date: Some(format!("2026-03-0{day}T10:00:00Z")),
                    body_text: None,
                    body_html: None,
                    rfc822: None,
                    size: Some(1),
                    flags: vec![],
                    modseq: Some(1),
                    shape: None,
                    headers: None,
                })
                .expect("insert")
            })
            .collect();
        for id in &ids {
            db.set_message_shape(*id, &shape).unwrap();
        }
        let uids = |pending: Vec<crate::tasks::bodies::PendingBody>| {
            pending
                .iter()
                .map(|p| (p.uid, p.part.clone()))
                .collect::<Vec<_>>()
        };

        // The newest two, newest first, with the HTML part's path.
        assert_eq!(
            uids(db.pending_recent_html(folder, 2, 10).unwrap()),
            vec![(4, "1.2".to_string()), (3, "1.2".to_string())]
        );

        // A text body does not settle it; the HTML does.
        db.set_message_body_text(ids[3], "text").unwrap();
        db.set_message_body_html(ids[3], "<p>html</p>").unwrap();
        assert_eq!(
            uids(db.pending_recent_html(folder, 2, 10).unwrap()),
            vec![(3, "1.2".to_string())]
        );
        let stored = db.get_message_by_id(ids[3]).unwrap().unwrap();
        assert_eq!(stored.body_text.as_deref(), Some("text"));
        assert_eq!(stored.body_html.as_deref(), Some("<p>html</p>"));
    }

    /// A search sorted by date or sender is in that order, and the limit
    /// takes the first of that order rather than the most relevant hits.
    #[test]
    fn a_sorted_search_orders_before_it_cuts() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let inbox = db.insert_folder("A", "INBOX").expect("folder");
        for (uid, from, date, subject) in [
            (1, "carol@x", "2026-03-01T00:00:00Z", "budget"),
            (2, "alice@x", "2026-01-01T00:00:00Z", "budget"),
            (3, "bob@x", "2026-02-01T00:00:00Z", "budget budget budget"),
        ] {
            let mut message = crate::models::Message::new(inbox, uid);
            message.subject = Some(subject.into());
            message.from = Some(from.into());
            message.date = Some(date.into());
            db.insert_message(&message).expect("insert");
        }
        let senders = |sort, limit| -> Vec<String> {
            db.search_messages_sorted("budget", &SearchScope::All, limit, None, sort)
                .unwrap()
                .into_iter()
                .map(|hit| hit.message.from.unwrap())
                .collect()
        };

        assert_eq!(senders(None, 1), vec!["bob@x"], "the most relevant first");
        assert_eq!(
            senders(Some(MessageSort::DateOldest), 1),
            vec!["alice@x"],
            "the oldest match, not the oldest of the most relevant"
        );
        assert_eq!(
            senders(Some(MessageSort::SenderDesc), 10),
            vec!["carol@x", "bob@x", "alice@x"]
        );
        let either = |sort| -> Vec<String> {
            db.search_messages_sorted("s:budget OR f:alice", &SearchScope::All, 10, None, sort)
                .unwrap()
                .into_iter()
                .map(|hit| hit.message.from.unwrap())
                .collect()
        };
        assert_eq!(
            either(None),
            vec!["carol@x", "bob@x", "alice@x"],
            "newest first"
        );
        assert_eq!(
            either(Some(MessageSort::DateOldest)),
            vec!["alice@x", "bob@x", "carol@x"]
        );
    }

    /// The query language, end to end against a real index.
    #[test]
    fn test_search_query_language_against_the_index() {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let work = db.insert_folder("Work", "INBOX").expect("folder");
        let archive = db.insert_folder("Work", "Archive").expect("folder");

        let msg = |folder_id: i64,
                   uid: u32,
                   subject: &str,
                   from: &str,
                   to: &str,
                   body: &str,
                   flags: Vec<String>,
                   date: &str| Message {
            id: None,
            folder_id,
            uid,
            message_id: Some(format!("<{uid}@example.com>")),
            subject: Some(subject.to_string()),
            from: Some(from.to_string()),
            to: Some(to.to_string()),
            cc: Some("Some Name <cc@example.com>".to_string()),
            date: Some(date.to_string()),
            body_text: Some(body.to_string()),
            body_html: None,
            rfc822: None,
            size: Some(1),
            flags,
            modseq: Some(1),
            shape: None,
            headers: None,
        };

        db.bulk_insert_messages(&[
            msg(
                work,
                1,
                "Quarterly invoice",
                "thomas@example.com",
                "sam@example.com",
                "please find the refund attached",
                vec![],
                "2026-03-01T10:00:00Z",
            ),
            msg(
                work,
                2,
                "Lunch",
                "noreply@example.com",
                "sam@example.com",
                "sandwiches",
                vec!["\\Seen".to_string()],
                "2026-03-02T10:00:00Z",
            ),
            msg(
                archive,
                3,
                "Old invoice",
                "thomas@example.com",
                "someone@example.com",
                "archived refund",
                vec!["\\Seen".to_string()],
                "2025-01-05T10:00:00Z",
            ),
        ])
        .expect("insert");

        let find = |q: &str| -> Vec<u32> {
            let mut uids: Vec<u32> = db
                .search_messages(q, &SearchScope::All, 50, None)
                .unwrap()
                .into_iter()
                .map(|r| r.message.uid)
                .collect();
            uids.sort_unstable();
            uids
        };

        assert_eq!(find("f:thomas"), vec![1, 3], "sender field");
        assert_eq!(find("s:invoice"), vec![1, 3], "subject field");
        assert_eq!(find("b:sandwiches"), vec![2], "body field");
        assert_eq!(find("t:someone"), vec![3], "recipient field");
        assert_eq!(find("cc:'Some Name'"), vec![1, 2, 3], "quoted cc phrase");
        assert_eq!(find("s:\"old invoice\""), vec![3], "quoted subject phrase");
        assert_eq!(
            find("s:\"invoice old\""),
            Vec::<u32>::new(),
            "a phrase keeps its order"
        );

        assert_eq!(find("f:thomas s:invoice"), vec![1, 3], "terms combine");
        assert_eq!(find("invoice -in:Archive"), vec![1], "negated folder");
        assert_eq!(find("f:thomas -b:archived"), vec![1], "negated text");

        assert_eq!(
            find("b:sandwiches OR in:Archive"),
            vec![2, 3],
            "either side of OR"
        );
        assert_eq!(
            find("f:thomas NOT in:Archive"),
            vec![1],
            "NOT excludes like -"
        );
        assert_eq!(
            find("in:Archive OR f:noreply OR b:refund"),
            vec![1, 2, 3],
            "several alternatives"
        );

        assert_eq!(find("is:unread"), vec![1], "unread state");
        assert_eq!(find("is:read"), vec![2, 3], "read state");
        assert_eq!(find("in:Archive"), vec![3], "folder scope, no text at all");
        assert_eq!(find("acct:Work in:INBOX"), vec![1, 2], "account and folder");

        assert_eq!(find("after:2026-01-01"), vec![1, 2], "date lower bound");
        assert_eq!(find("before:2026-01-01"), vec![3], "date upper bound");
        assert_eq!(
            find("f:thomas after:2026-01-01"),
            vec![1],
            "text and date together"
        );

        // Stemming, from the porter tokenizer.
        assert_eq!(
            find("s:invoices"),
            vec![1, 3],
            "a search for the plural finds the singular"
        );
        assert_eq!(find("b:refunds"), vec![1, 3], "and in the body");

        // Part of a word: the start of one by the index, anywhere in a header.
        assert_eq!(find("f:thom"), vec![1, 3], "the start of a sender");
        assert_eq!(find("f:HOMAS@EX"), vec![1, 3], "inside it, case aside");
        assert_eq!(find("noreply"), vec![2], "a bare word finds a sender");
        assert_eq!(find("someone"), vec![3], "and a recipient");
        assert_eq!(
            find("sandw"),
            vec![2],
            "and the start of a word of the body"
        );
        assert_eq!(find("refund thom"), vec![1, 3], "every word, each anywhere");
        assert_eq!(find("refund -thom"), Vec::<u32>::new(), "a word excluded");

        // Nothing matches, rather than everything.
        assert!(find("f:nobody").is_empty());
        assert!(
            find("").is_empty(),
            "an empty query must not list the mailbox"
        );
        // A query that will not parse says so. Returning nothing made a
        // mistyped query look like a mailbox with no such mail in it.
        let broken = db.search_messages("s:'unclosed", &SearchScope::All, 50, None);
        assert!(broken.is_err(), "an unparseable query must be reported");
        assert!(
            format!("{}", broken.unwrap_err()).contains("quote"),
            "and the reason must say what was wrong"
        );

        // A space after the prefix is accepted, so this finds what `f:thomas`
        // finds rather than nothing.
        assert_eq!(find("f: thomas"), vec![1, 3], "space after the prefix");
    }

    /// A flag-only sync carries no BODYSTRUCTURE. It must not erase what an
    /// earlier full sync recorded, or the body worker loses track of which part
    /// to fetch and the paperclip disappears from the list.
    #[test]
    fn test_flag_only_sync_preserves_the_message_shape() {
        use crate::imap::structure::MessageShape;

        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let folder_id = db.insert_folder("acct", "INBOX").expect("insert folder");

        let base = |shape: Option<MessageShape>, flags: Vec<String>| Message {
            id: None,
            folder_id,
            uid: 7,
            message_id: Some("<m@example.com>".to_string()),
            subject: Some("s".to_string()),
            from: Some("a@example.com".to_string()),
            to: None,
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: None,
            body_html: None,
            rfc822: None,
            size: Some(1),
            flags,
            modseq: Some(1),
            shape,
            headers: None,
        };

        db.bulk_insert_messages(&[base(
            Some(MessageShape {
                text_part: Some("1.1".to_string()),
                html_part: Some("1.2".to_string()),
                text_bytes: Some(400),
                text_encoding: Some("base64".to_string()),
                has_attachments: true,
            }),
            vec![],
        )])
        .expect("initial sync");

        // Flag-only sync: same UID, no BODYSTRUCTURE.
        db.bulk_insert_messages(&[base(None, vec!["\\Seen".to_string()])])
            .expect("flag-only sync");

        let stored: MessageShape = db
            .conn
            .query_row(
                "SELECT text_part, html_part, text_encoding, text_bytes, has_attachments
                 FROM messages WHERE folder_id = ?1 AND uid = 7",
                params![folder_id],
                |r| {
                    Ok(MessageShape {
                        text_part: r.get(0)?,
                        html_part: r.get(1)?,
                        text_encoding: r.get(2)?,
                        text_bytes: r.get(3)?,
                        has_attachments: r.get::<_, Option<i32>>(4)? == Some(1),
                    })
                },
            )
            .expect("row");

        assert_eq!(
            stored,
            MessageShape {
                text_part: Some("1.1".to_string()),
                html_part: Some("1.2".to_string()),
                text_bytes: Some(400),
                text_encoding: Some("base64".to_string()),
                has_attachments: true,
            }
        );
    }

    /// Several folders read as one list come back in one order across all of
    /// them and page like a single folder, each row saying where it is from.
    #[test]
    fn several_folders_page_as_one_list() {
        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let work = db.insert_folder("work", "INBOX").expect("insert folder");
        let home = db.insert_folder("home", "INBOX").expect("insert folder");
        let other = db.insert_folder("work", "Archive").expect("insert folder");
        let row = |folder_id: i64, uid: u32, day: u32| Message {
            id: None,
            folder_id,
            uid,
            message_id: Some(format!("<m-{folder_id}-{uid}@example.com>")),
            subject: Some(format!("{folder_id}/{uid}")),
            from: Some("a@example.com".to_string()),
            to: Some("b@example.com".to_string()),
            cc: None,
            date: Some(format!("2026-07-{day:02}T10:00:00Z")),
            body_text: None,
            body_html: None,
            rfc822: None,
            size: Some(1),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        };
        db.bulk_insert_messages(&[row(work, 1, 1), row(work, 2, 4), row(other, 1, 5)])
            .expect("insert");
        db.bulk_insert_messages(&[row(home, 1, 2), row(home, 2, 3)])
            .expect("insert");

        let page = |offset| {
            db.get_message_list_in(
                &[work, home],
                offset,
                2,
                crate::store::MessageSort::DateNewest,
            )
            .expect("list")
            .into_iter()
            .map(|m| (m.folder_id, m.uid))
            .collect::<Vec<_>>()
        };
        assert_eq!(
            page(0),
            vec![(work, 2), (home, 2)],
            "newest first, across both"
        );
        assert_eq!(
            page(2),
            vec![(home, 1), (work, 1)],
            "the next page follows on"
        );
        assert!(page(4).is_empty(), "the archive is not part of it");
        assert!(
            db.get_message_list_in(&[], 0, 2, crate::store::MessageSort::DateNewest)
                .unwrap()
                .is_empty()
        );
    }

    /// The list's attachment flag must come from the stored column, not from
    /// re-parsing each message. Saving a body is what computes it.
    #[test]
    fn test_has_attachments_is_stored_when_the_body_is_saved() {
        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let folder_id = db.insert_folder("acct", "INBOX").expect("insert folder");

        let row = |uid: u32| Message {
            id: None,
            folder_id,
            uid,
            message_id: Some(format!("<m-{uid}@example.com>")),
            subject: Some("s".to_string()),
            from: Some("a@example.com".to_string()),
            to: Some("b@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: None,
            body_html: None,
            rfc822: None,
            size: Some(1),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        };
        db.bulk_insert_messages(&[row(1), row(2), row(3)])
            .expect("insert");
        let ids: Vec<i64> = db
            .get_message_list(folder_id, 0, 10, crate::store::MessageSort::DateNewest)
            .expect("list")
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids.len(), 3);

        let plain = b"Subject: plain\r\n\r\njust text\r\n";
        let with_attachment = b"Subject: report\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nbody\r\n--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"r.pdf\"\r\n\r\ndata\r\n--b--\r\n";

        db.update_message_body(ids[0], Some("just text"), None, Some(plain))
            .unwrap();
        db.update_message_body(ids[1], Some("body"), None, Some(with_attachment))
            .unwrap();

        let found = db.messages_with_attachments(&ids).unwrap();

        assert!(found.contains(&ids[1]), "attachment must be recorded");
        assert!(!found.contains(&ids[0]), "plain message has none");
        assert!(
            !found.contains(&ids[2]),
            "a message whose source was never fetched must not be reported as having one"
        );
    }

    #[test]
    fn recent_mail_that_could_be_an_invitation_is_shaped_again() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let path = dir.path().join("orb_test.db");
        let path_str = path.to_str().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        {
            let mut db = Database::new(path_str).expect("db");
            db.init_schema().expect("schema");
            let folder = db.get_or_create_folder("A", "INBOX").expect("folder");
            // (uid, has_attachments, text_part, html_part, age in days)
            for (uid, has, text, html, days) in [
                (1, Some(0), Some("1"), Some("2"), 2),
                (2, Some(0), Some("1"), Some("2"), 400),
                (3, Some(0), Some("1"), None, 2),
                (4, Some(1), Some("1"), Some("2"), 2),
            ] {
                db.conn
                    .execute(
                        "INSERT INTO messages (folder_id, uid, has_attachments, text_part, html_part, date_unix)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![folder, uid, has, text, html, now - days * 86_400],
                    )
                    .expect("insert");
            }
            db.conn.execute_batch("PRAGMA user_version = 30;").unwrap();
        }

        let db = Database::new(path_str).expect("reopen");
        db.init_schema().expect("migrate");
        let shapes: Vec<(u32, Option<i32>)> = db
            .conn
            .prepare("SELECT uid, has_attachments FROM messages ORDER BY uid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            shapes,
            vec![(1, None), (2, Some(0)), (3, Some(0)), (4, Some(1))],
            "only recent plain-and-HTML mail without attachments is asked about again"
        );
    }

    #[test]
    fn test_interrupted_upgrade_reopens_cleanly() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let path = dir.path().join("orb_test.db");
        let path_str = path.to_str().unwrap();

        // Build a pre-v6 style database directly: messages table WITHOUT the
        // date_unix column, and user_version pinned at 5.
        {
            let conn = Connection::open(path_str).unwrap();
            conn.execute_batch(
                // Include the content columns that have existed on `messages`
                // since schema v0 (subject/from_addr/to_addr/body_text): the
                // v8->v9 FTS backfill migration reads them, and any real pre-v6
                // database would have them.
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    folder_id INTEGER NOT NULL,
                    uid INTEGER NOT NULL,
                    subject TEXT,
                    from_addr TEXT,
                    to_addr TEXT,
                    cc_addr TEXT,
                    body_text TEXT,
                    date TEXT,
                    flags TEXT
                 );
                 PRAGMA user_version = 5;",
            )
            .unwrap();

            // Simulate the crash window: the v5->v6 ALTER commits...
            conn.execute_batch("ALTER TABLE messages ADD COLUMN date_unix INTEGER;")
                .unwrap();
            // ...but the process is force-quit before the version is bumped, so
            // user_version is still 5 on disk. (No PRAGMA user_version write here.)
            assert_eq!(
                conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                    .unwrap(),
                5
            );
        }

        // Reopen: init_schema must recover cleanly and reach the current version.
        let db = Database::new(path_str).expect("failed to reopen db");
        db.init_schema()
            .expect("reopening a half-migrated database must succeed");
        assert_eq!(db.get_schema_version().unwrap(), SCHEMA_VERSION);
        assert!(Database::column_exists(&db.conn, "messages", "date_unix").unwrap());
    }

    /// A flag-only / CHANGEDSINCE sync fetches no message BODY, so the upserted
    /// rows carry NULL body_text/body_html/rfc822. The bulk upsert must NOT
    /// overwrite already-cached bodies with those NULLs. Before the fix the
    /// ON CONFLICT clause unconditionally wrote `excluded.body_*`, wiping the
    /// cached body on every flag update.
    #[test]
    fn test_flag_only_upsert_preserves_cached_body() {
        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let folder_id = db.insert_folder("acct", "INBOX").expect("insert folder");

        // Initial sync: message stored WITH a full body.
        let with_body = Message {
            id: None,
            folder_id,
            uid: 42,
            message_id: Some("<msg-42@example.com>".to_string()),
            subject: Some("Hello".to_string()),
            from: Some("a@example.com".to_string()),
            to: Some("b@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: Some("cached plain body".to_string()),
            body_html: Some("<p>cached html body</p>".to_string()),
            rfc822: Some(b"raw rfc822 bytes".to_vec()),
            size: Some(1234),
            flags: vec!["Seen".to_string()],
            modseq: Some(100),
            shape: None,
            headers: None,
        };
        db.bulk_insert_messages(&[with_body])
            .expect("initial insert");

        // Flag-only sync: same UID, NO body fetched (NULL bodies), new flag/modseq.
        let flag_only = Message {
            id: None,
            folder_id,
            uid: 42,
            message_id: Some("<msg-42@example.com>".to_string()),
            subject: Some("Hello".to_string()),
            from: Some("a@example.com".to_string()),
            to: Some("b@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: None,
            body_html: None,
            rfc822: None,
            size: Some(1234),
            flags: vec!["Seen".to_string(), "Flagged".to_string()],
            modseq: Some(101),
            shape: None,
            headers: None,
        };
        db.bulk_insert_messages(&[flag_only])
            .expect("flag-only upsert");

        let messages = db.get_messages_by_folder(folder_id).expect("read back");
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];

        // Bodies must be preserved from the initial sync.
        assert_eq!(msg.body_text.as_deref(), Some("cached plain body"));
        assert_eq!(msg.body_html.as_deref(), Some("<p>cached html body</p>"));
        assert_eq!(msg.rfc822.as_deref(), Some(&b"raw rfc822 bytes"[..]));

        // Flag update from the flag-only sync must still have been applied.
        assert!(msg.flags.contains(&"Flagged".to_string()));
        assert_eq!(msg.modseq, Some(101));
    }

    /// The bulk upsert must count only genuinely new UIDs as "inserted" and
    /// existing UIDs (e.g. flag-only updates from a CHANGEDSINCE sync) as
    /// "updated". Before the fix the insert/update classification relied on
    /// last_insert_rowid(), which ON CONFLICT DO UPDATE leaves unchanged, so a
    /// batch mixing new and existing UIDs over-counted inserts and produced
    /// spurious "new mail" notifications.
    #[test]
    fn test_bulk_upsert_insert_update_count_ignores_flag_updates() {
        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let folder_id = db.insert_folder("acct", "INBOX").expect("insert folder");

        let make_msg = |uid: u32, flags: Vec<String>, modseq: u64| Message {
            id: None,
            folder_id,
            uid,
            message_id: Some(format!("<msg-{uid}@example.com>")),
            subject: Some(format!("Subject {uid}")),
            from: Some("a@example.com".to_string()),
            to: Some("b@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: Some("body".to_string()),
            body_html: None,
            rfc822: None,
            size: Some(10),
            flags,
            modseq: Some(modseq),
            shape: None,
            headers: None,
        };

        // Initial sync: three brand-new messages.
        let initial = vec![
            make_msg(1, vec!["Seen".to_string()], 100),
            make_msg(2, vec![], 100),
            make_msg(3, vec![], 100),
        ];
        let (inserted, updated) = db.bulk_insert_messages(&initial).expect("initial insert");
        assert_eq!(inserted, 3, "all three UIDs are new");
        assert_eq!(updated, 0);

        // Second sync: a batch mixing one genuinely new UID (4) with two flag-only
        // updates of existing UIDs (1, 2). The new UID is placed FIRST on purpose:
        // under the old last_insert_rowid() heuristic the real insert bumps the
        // rowid above the baseline, so every following update was misclassified as
        // an insert. Only UID 4 must count as added.
        let mixed = vec![
            make_msg(4, vec![], 100),
            make_msg(1, vec!["Seen".to_string(), "Flagged".to_string()], 101),
            make_msg(2, vec!["Seen".to_string()], 101),
        ];
        let (inserted, updated) = db.bulk_insert_messages(&mixed).expect("mixed upsert");
        assert_eq!(inserted, 1, "only UID 4 is genuinely new");
        assert_eq!(updated, 2, "UIDs 1 and 2 are flag-only updates");
    }

    /// Full-text search must match on subject, sender, and body, exclude
    /// non-matching messages, span multiple folders/accounts (whole-mailbox),
    /// and honour scope restrictions.
    #[test]
    fn test_fts_search_matches_subject_from_and_body() {
        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let inbox = db.insert_folder("work", "INBOX").expect("insert folder");
        let other = db
            .insert_folder("personal", "Archive")
            .expect("insert folder");

        let msg = |folder_id: i64, uid: u32, subject: &str, from: &str, body: &str| Message {
            id: None,
            folder_id,
            uid,
            message_id: Some(format!("<msg-{uid}@example.com>")),
            subject: Some(subject.to_string()),
            from: Some(from.to_string()),
            to: Some("me@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: Some(body.to_string()),
            body_html: None,
            rfc822: None,
            size: Some(100),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        };

        db.bulk_insert_messages(&[
            msg(
                inbox,
                1,
                "Quarterly invoice",
                "billing@acme.com",
                "Please pay",
            ),
            msg(
                inbox,
                2,
                "Lunch plans",
                "friend@example.com",
                "Pizza on Friday",
            ),
            msg(
                other,
                3,
                "Weekend trip",
                "travel@example.com",
                "Your invoice is attached",
            ),
        ])
        .expect("insert messages");

        // Match by subject.
        let by_subject = db
            .search_messages("invoice", &SearchScope::All, 100, None)
            .expect("search by subject");
        let uids: Vec<u32> = by_subject.iter().map(|r| r.message.uid).collect();
        assert!(uids.contains(&1), "subject 'Quarterly invoice' must match");
        // Match by body (message 3 mentions 'invoice' only in its body).
        assert!(
            uids.contains(&3),
            "body 'Your invoice is attached' must match"
        );
        assert!(!uids.contains(&2), "non-matching message must be excluded");

        // Whole-mailbox search spans multiple accounts/folders.
        let accounts: std::collections::HashSet<&str> =
            by_subject.iter().map(|r| r.account_name.as_str()).collect();
        assert!(accounts.contains("work") && accounts.contains("personal"));

        // Match by sender address.
        let by_from = db
            .search_messages("acme", &SearchScope::All, 100, None)
            .expect("search by from");
        assert_eq!(by_from.len(), 1);
        assert_eq!(by_from[0].message.uid, 1);

        // Scope restriction: limit to the personal/Archive folder.
        let scoped = db
            .search_messages(
                "invoice",
                &SearchScope::Folder {
                    account_name: "personal".to_string(),
                    folder_name: "Archive".to_string(),
                },
                100,
                None,
            )
            .expect("scoped search");
        assert_eq!(scoped.len(), 1, "scope must exclude the work/INBOX hit");
        assert_eq!(scoped[0].message.uid, 3);

        // Several folders: what a filter over Today's inboxes asks.
        let several = |folders: &[(&str, &str)]| {
            let folders = folders
                .iter()
                .map(|(a, f)| (a.to_string(), f.to_string()))
                .collect();
            db.search_messages("invoice", &SearchScope::Folders(folders), 100, None)
                .expect("folders search")
                .len()
        };
        assert_eq!(several(&[("personal", "Archive"), ("work", "INBOX")]), 2);
        assert_eq!(several(&[("personal", "Archive")]), 1);
        assert_eq!(several(&[]), 0, "no folder is nothing to search");

        // An empty / whitespace-only query yields no results (and does not error).
        assert!(
            db.search_messages("   ", &SearchScope::All, 100, None)
                .expect("blank search")
                .is_empty()
        );
    }

    /// The FTS index must stay in sync as messages change and are deleted: an
    /// updated subject becomes findable under its new terms (and not the old),
    /// and a deleted message disappears from results.
    #[test]
    fn test_fts_index_updates_on_change_and_delete() {
        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let folder_id = db.insert_folder("work", "INBOX").expect("insert folder");

        let mut base = Message {
            id: None,
            folder_id,
            uid: 7,
            message_id: Some("<msg-7@example.com>".to_string()),
            subject: Some("Original topic".to_string()),
            from: Some("a@example.com".to_string()),
            to: Some("me@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: Some("hello world".to_string()),
            body_html: None,
            rfc822: None,
            size: Some(10),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        };
        db.bulk_insert_messages(std::slice::from_ref(&base))
            .expect("initial insert");

        assert_eq!(
            db.search_messages("Original", &SearchScope::All, 100, None)
                .unwrap()
                .len(),
            1,
            "message is findable by its initial subject"
        );

        // Update the subject via an upsert (fires the AFTER UPDATE trigger).
        base.subject = Some("Renamed heading".to_string());
        base.modseq = Some(2);
        db.bulk_insert_messages(std::slice::from_ref(&base))
            .expect("update subject");

        assert!(
            db.search_messages("Original", &SearchScope::All, 100, None)
                .unwrap()
                .is_empty(),
            "the old subject term must no longer match after an update"
        );
        assert_eq!(
            db.search_messages("Renamed", &SearchScope::All, 100, None)
                .unwrap()
                .len(),
            1,
            "the new subject term must match after an update"
        );

        // Deleting the message must remove it from the index.
        db.delete_messages_by_uids(folder_id, &[7])
            .expect("delete message");
        assert!(
            db.search_messages("Renamed", &SearchScope::All, 100, None)
                .unwrap()
                .is_empty(),
            "a deleted message must not appear in search results"
        );
    }

    /// A flag-only UPDATE must NOT re-index the FTS row (avoiding write
    /// amplification), while an UPDATE that touches an indexed column
    /// (subject/from/to/body) still re-indexes.
    ///
    /// To detect *whether the AFTER UPDATE trigger fired* — not merely whether
    /// content ended up identical — we deliberately desynchronize the FTS index
    /// from the base table by writing a sentinel token directly into the
    /// standalone FTS row. If the trigger fires, the DELETE+INSERT rebuilds the
    /// FTS row from `messages` and the sentinel disappears; if it does not fire,
    /// the sentinel survives. This is a precise characterization of re-indexing.
    /// A flag-only write must not re-index, and an indexed-column write must.
    ///
    /// The first half is a property of the trigger's WHEN clause, checked by
    /// reading the clause: with an external-content index there is no safe way
    /// to observe "did not re-index" from the outside, because the only probe —
    /// making the index disagree with the content table — is precisely what
    /// corrupts such an index, and a later delete would then remove the wrong
    /// terms. The second half is observable directly.
    #[test]
    fn test_fts_flag_only_update_does_not_reindex() {
        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let folder_id = db.insert_folder("work", "INBOX").expect("insert folder");

        let base = Message {
            id: None,
            folder_id,
            uid: 42,
            message_id: Some("<msg-42@example.com>".to_string()),
            subject: Some("Quarterly report".to_string()),
            from: Some("a@example.com".to_string()),
            to: Some("me@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: Some("budget figures".to_string()),
            body_html: None,
            rfc822: None,
            size: Some(10),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        };
        db.bulk_insert_messages(std::slice::from_ref(&base))
            .expect("initial insert");

        let trigger: String = db
            .conn
            .query_row(
                "SELECT group_concat(sql, ' ') FROM sqlite_master
                 WHERE type = 'trigger' AND name IN ('messages_fts_au', 'message_bodies_au')",
                [],
                |row| row.get(0),
            )
            .expect("update triggers must exist");

        // Scoped to the indexed columns, so a write to any other column cannot
        // fire it at all.
        assert!(
            trigger.contains("UPDATE OF"),
            "trigger must be column-scoped"
        );
        assert!(
            !trigger.contains("flags"),
            "flags must not be an indexed column"
        );
        // And guarded, so naming an indexed column without changing its value —
        // which the sync upsert does on every flag-only pass — still does not
        // re-index.
        assert!(trigger.contains("WHEN"), "trigger must be value-guarded");
        for column in ["subject", "from_addr", "to_addr", "cc_addr", "body_text"] {
            assert!(
                trigger.contains(&format!("new.{column} IS NOT old.{column}")),
                "the guard must cover {column}"
            );
        }

        let msg_id: i64 = db
            .conn
            .query_row(
                "SELECT id FROM messages WHERE folder_id = ?1 AND uid = 42",
                params![folder_id],
                |row| row.get(0),
            )
            .expect("fetch message id");

        // A flag-only write leaves the index correct.
        db.conn
            .execute(
                "UPDATE messages SET flags = ?1 WHERE id = ?2",
                params!["[\"\\\\Seen\"]", msg_id],
            )
            .expect("flag-only update");
        assert_eq!(
            db.search_messages("Quarterly", &SearchScope::All, 100, None)
                .unwrap()
                .len(),
            1
        );

        // An indexed-column write re-indexes: the old term goes, the new arrives.
        db.conn
            .execute(
                "UPDATE messages SET subject = 'Annual summary' WHERE id = ?1",
                params![msg_id],
            )
            .expect("subject update");
        assert!(
            db.search_messages("Quarterly", &SearchScope::All, 100, None)
                .unwrap()
                .is_empty(),
            "the old term must be gone after re-indexing"
        );
        assert_eq!(
            db.search_messages("Annual", &SearchScope::All, 100, None)
                .unwrap()
                .len(),
            1,
            "the new term must be searchable after re-indexing"
        );
    }

    /// Opening a database built against the old standalone index must leave it
    /// with the current one.
    ///
    /// Migration 15 tears the old index down and `init_schema` rebuilds it, so
    /// what matters is the end state after opening — not what any single
    /// migration did on the way.
    #[test]
    fn test_opening_an_old_database_rebuilds_the_index() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("old.db");
        let path_str = path.to_str().unwrap();

        {
            let db = Database::new(path_str).expect("create");
            db.init_schema().expect("schema init failed");

            // Emulate a pre-v15 database: the standalone index, with the
            // unscoped trigger that shipped before v10.
            db.conn
                .execute_batch(
                    "DROP TRIGGER IF EXISTS messages_fts_ai;
                     DROP TRIGGER IF EXISTS messages_fts_au;
                     DROP TRIGGER IF EXISTS messages_fts_ad;
                     DROP TABLE IF EXISTS messages_fts;
                     CREATE VIRTUAL TABLE messages_fts USING fts5(
                         subject, from_addr, to_addr, body_text, tokenize = 'unicode61');
                     CREATE TRIGGER messages_fts_au AFTER UPDATE ON messages BEGIN
                         DELETE FROM messages_fts WHERE rowid = old.id;
                         INSERT INTO messages_fts(rowid, subject, from_addr, to_addr, body_text)
                         VALUES (new.id, new.subject, new.from_addr, new.to_addr, new.body_text);
                     END;",
                )
                .expect("install the old index");
            db.set_schema_version(9).expect("pin to 9");
        }

        let mut db = Database::new(path_str).expect("reopen");
        db.init_schema()
            .expect("reopening an old database must succeed");

        let table: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts'",
                [],
                |row| row.get(0),
            )
            .expect("index must exist");
        assert!(
            table.contains("content = 'messages_search'"),
            "the index must be external-content, got: {table}"
        );
        assert!(table.contains("porter"), "the tokenizer must stem");
        assert!(table.contains("cc_addr"), "Cc must be indexed");

        let trigger: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'messages_fts_au'",
                [],
                |row| row.get(0),
            )
            .expect("update trigger must exist");
        assert!(
            trigger.contains("UPDATE OF"),
            "trigger must be column-scoped"
        );
        assert!(
            !trigger.contains("DELETE FROM messages_fts"),
            "an external-content index must be deleted from with the 'delete' \
             command, not a DELETE statement — that is what corrupted it before"
        );

        // And it works: a message inserted after the rebuild is findable.
        let folder_id = db.insert_folder("work", "INBOX").expect("folder");
        db.bulk_insert_messages(&[Message {
            id: None,
            folder_id,
            uid: 1,
            message_id: None,
            subject: Some("Rebuilt index".to_string()),
            from: Some("a@example.com".to_string()),
            to: None,
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: Some("searchable".to_string()),
            body_html: None,
            rfc822: None,
            size: Some(1),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        }])
        .expect("insert");

        assert_eq!(
            db.search_messages("searchable", &SearchScope::All, 10, None)
                .unwrap()
                .len(),
            1
        );
    }

    /// A flag-only re-sync going through the REAL upsert path
    /// (`bulk_insert_messages`, which names the indexed columns in its SET
    /// clause) must NOT re-index the FTS row. This is the value-change WHEN guard
    /// (schema v11): `AFTER UPDATE OF` alone still fires here because the upsert
    /// names body_text (via COALESCE), so only the WHEN clause prevents the
    /// redundant DELETE+INSERT. A genuine body change must still re-index.
    #[test]
    fn test_fts_flag_only_upsert_does_not_reindex_but_body_change_does() {
        let mut db = Database::new(":memory:").expect("failed to create db");
        db.init_schema().expect("schema init failed");
        let folder_id = db.insert_folder("work", "INBOX").expect("insert folder");

        let base = Message {
            id: None,
            folder_id,
            uid: 1,
            message_id: Some("<m@example.com>".to_string()),
            subject: Some("Quarterly Report".to_string()),
            from: Some("cfo@example.com".to_string()),
            to: Some("me@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: Some("original confidential figures".to_string()),
            body_html: None,
            rfc822: None,
            size: Some(10),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        };
        db.bulk_insert_messages(std::slice::from_ref(&base))
            .expect("insert");

        let id: i64 = db
            .conn
            .query_row(
                "SELECT id FROM messages WHERE folder_id = ?1 AND uid = 1",
                params![folder_id],
                |row| row.get(0),
            )
            .expect("fetch id");

        let _ = id;

        // Flag-only re-sync: same envelope, body not re-fetched (None -> COALESCE
        // preserves), only flags/modseq change. The index must stay correct.
        let mut flag_update = base.clone();
        flag_update.body_text = None;
        flag_update.flags = vec!["\\Seen".to_string()];
        flag_update.modseq = Some(2);
        db.bulk_insert_messages(&[flag_update])
            .expect("flag-only upsert");

        assert_eq!(
            db.search_messages("original", &SearchScope::All, 100, None)
                .unwrap()
                .len(),
            1,
            "a flag-only upsert must leave the body indexed"
        );

        // A genuine body change must re-index: the old terms go, the new arrive.
        let mut body_change = base.clone();
        body_change.body_text = Some("brandnew contents".to_string());
        body_change.modseq = Some(3);
        db.bulk_insert_messages(&[body_change])
            .expect("body-change upsert");

        assert!(
            db.search_messages("original", &SearchScope::All, 100, None)
                .unwrap()
                .is_empty(),
            "a body change must re-index (old body term gone)"
        );
        assert_eq!(
            db.search_messages("brandnew", &SearchScope::All, 100, None)
                .unwrap()
                .len(),
            1,
            "the new body term must be searchable after re-indexing"
        );
    }

    // ========================================================================
    // Pending (staged) actions
    // ========================================================================

    fn stage_msg_message(folder_id: i64, uid: u32) -> Message {
        Message {
            id: None,
            folder_id,
            uid,
            message_id: Some(format!("<m-{}@x>", uid)),
            subject: Some(format!("Subject {}", uid)),
            from: Some("a@example.com".to_string()),
            to: Some("b@example.com".to_string()),
            cc: None,
            date: Some("2026-07-16T10:00:00Z".to_string()),
            body_text: Some("body".to_string()),
            body_html: None,
            rfc822: None,
            size: Some(10),
            flags: vec![],
            modseq: Some(1),
            shape: None,
            headers: None,
        }
    }

    #[test]
    fn test_stage_pending_action_persists_and_survives_reopen() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("pending.db");
        let path_str = path.to_str().unwrap();

        {
            let db = Database::new(path_str).expect("open db");
            db.init_schema().expect("init schema");
            db.stage_pending_action("acct", "INBOX", 7, "delete", None, "2026-07-16T00:00:00Z")
                .expect("stage delete");
            db.stage_pending_action(
                "acct",
                "INBOX",
                8,
                "move",
                Some("Archive"),
                "2026-07-16T00:00:01Z",
            )
            .expect("stage move");
        }

        // Reopen: rows must still be there (persisted, not in-memory only).
        let db = Database::new(path_str).expect("reopen db");
        db.init_schema().expect("re-init schema");
        let rows = db
            .list_pending_actions(&PendingScope::Folder {
                account_name: "acct".to_string(),
                folder_name: "INBOX".to_string(),
            })
            .expect("list");
        assert_eq!(rows.len(), 2);
        let del = rows.iter().find(|r| r.uid == 7).expect("uid 7");
        assert_eq!(del.action_type, "delete");
        assert_eq!(del.target_folder, None);
        let mv = rows.iter().find(|r| r.uid == 8).expect("uid 8");
        assert_eq!(mv.action_type, "move");
        assert_eq!(mv.target_folder.as_deref(), Some("Archive"));
    }

    #[test]
    fn test_stage_pending_action_upsert_last_action_wins() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");

        db.stage_pending_action("acct", "INBOX", 5, "delete", None, "2026-07-16T00:00:00Z")
            .expect("stage delete");
        // Re-stage the same message as a move: must replace, not duplicate.
        db.stage_pending_action(
            "acct",
            "INBOX",
            5,
            "move",
            Some("Archive"),
            "2026-07-16T00:00:05Z",
        )
        .expect("stage move");

        let rows = db.list_pending_actions(&PendingScope::All).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action_type, "move");
        assert_eq!(rows[0].target_folder.as_deref(), Some("Archive"));
    }

    #[test]
    fn test_unstage_and_clear_pending_actions() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");

        db.stage_pending_action("acct", "INBOX", 1, "delete", None, "t")
            .unwrap();
        db.stage_pending_action("acct", "INBOX", 2, "delete", None, "t")
            .unwrap();
        db.stage_pending_action("acct", "Sent", 3, "delete", None, "t")
            .unwrap();

        // Unstage a single message, and one that was never staged.
        assert_eq!(
            db.unstage_pending_actions("acct", "INBOX", &[1]).unwrap(),
            1
        );
        assert_eq!(
            db.unstage_pending_actions("acct", "INBOX", &[999]).unwrap(),
            0
        );
        // Several at once, which is what a selection asks for. The empty case
        // is a command with nothing to act on, not an error.
        assert_eq!(db.unstage_pending_actions("acct", "INBOX", &[]).unwrap(), 0);

        // Discard (clear) a whole folder scope without touching other folders.
        let cleared = db
            .clear_pending_actions(&PendingScope::Folder {
                account_name: "acct".to_string(),
                folder_name: "INBOX".to_string(),
            })
            .unwrap();
        assert_eq!(cleared, 1); // only uid 2 remained in INBOX
        assert_eq!(
            db.list_pending_actions(&PendingScope::All).unwrap().len(),
            1
        );
    }

    #[test]
    fn test_pending_action_counts_and_scopes() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");

        db.stage_pending_action("a1", "INBOX", 1, "delete", None, "t")
            .unwrap();
        db.stage_pending_action("a1", "INBOX", 2, "delete", None, "t")
            .unwrap();
        db.stage_pending_action("a1", "Sent", 3, "move", Some("Archive"), "t")
            .unwrap();
        db.stage_pending_action("a2", "INBOX", 4, "delete", None, "t")
            .unwrap();

        let all = db.pending_action_counts(&PendingScope::All).unwrap();
        let total: u32 = all.iter().map(|c| c.count).sum();
        assert_eq!(total, 4);
        assert_eq!(all.len(), 3); // (a1,INBOX), (a1,Sent), (a2,INBOX)

        let a1 = db
            .pending_action_counts(&PendingScope::Account {
                account_name: "a1".to_string(),
            })
            .unwrap();
        let a1_total: u32 = a1.iter().map(|c| c.count).sum();
        assert_eq!(a1_total, 3);

        let inbox = db
            .list_pending_actions(&PendingScope::Folder {
                account_name: "a1".to_string(),
                folder_name: "INBOX".to_string(),
            })
            .unwrap();
        assert_eq!(inbox.len(), 2);
    }

    #[test]
    fn test_delete_pending_actions_for_uids() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");

        for uid in 1..=5 {
            db.stage_pending_action("acct", "INBOX", uid, "delete", None, "t")
                .unwrap();
        }
        // Simulate execute clearing only the rows whose op succeeded.
        let removed = db
            .delete_pending_actions_for_uids("acct", "INBOX", &[2, 4])
            .unwrap();
        assert_eq!(removed, 2);
        let remaining: Vec<u32> = db
            .list_pending_actions(&PendingScope::All)
            .unwrap()
            .into_iter()
            .map(|r| r.uid)
            .collect();
        assert_eq!(remaining, vec![1, 3, 5]);
    }

    #[test]
    fn test_prune_pending_actions_removes_vanished_uid() {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let folder_id = db.insert_folder("acct", "INBOX").expect("folder");

        // UIDs 1 and 3 exist locally; UID 2 was deleted on the server.
        db.bulk_insert_messages(&[
            stage_msg_message(folder_id, 1),
            stage_msg_message(folder_id, 3),
        ])
        .expect("insert messages");

        for uid in [1, 2, 3] {
            db.stage_pending_action("acct", "INBOX", uid, "delete", None, "t")
                .unwrap();
        }

        let tx = db.conn.transaction().expect("tx");
        let pruned =
            Database::prune_pending_actions_in_tx(&tx, "acct", "INBOX", folder_id).expect("prune");
        tx.commit().expect("commit");
        assert_eq!(pruned, 1); // only uid 2 (vanished) removed

        let remaining: Vec<u32> = db
            .list_pending_actions(&PendingScope::All)
            .unwrap()
            .into_iter()
            .map(|r| r.uid)
            .collect();
        assert_eq!(remaining, vec![1, 3]);
    }

    #[test]
    fn test_clear_pending_actions_for_folder_in_tx_on_uidvalidity_change() {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");

        db.stage_pending_action("acct", "INBOX", 1, "delete", None, "t")
            .unwrap();
        db.stage_pending_action("acct", "INBOX", 2, "move", Some("Archive"), "t")
            .unwrap();
        db.stage_pending_action("acct", "Sent", 9, "delete", None, "t")
            .unwrap();

        let tx = db.conn.transaction().expect("tx");
        let cleared = Database::clear_pending_actions_for_folder_in_tx(&tx, "acct", "INBOX")
            .expect("clear folder");
        tx.commit().expect("commit");
        assert_eq!(cleared, 2);

        // Other folders are untouched.
        let remaining: Vec<u32> = db
            .list_pending_actions(&PendingScope::All)
            .unwrap()
            .into_iter()
            .map(|r| r.uid)
            .collect();
        assert_eq!(remaining, vec![9]);
    }

    /// The sync-path maintenance helper must prune on a normal sync and clear the
    /// whole folder on a UIDVALIDITY change — the exact branch the sync loop runs.
    #[test]
    fn test_maintain_pending_actions_after_sync_in_tx() {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let folder_id = db.insert_folder("acct", "INBOX").expect("folder");
        db.bulk_insert_messages(&[
            stage_msg_message(folder_id, 1),
            stage_msg_message(folder_id, 3),
        ])
        .expect("insert");
        for uid in [1, 2, 3] {
            db.stage_pending_action("acct", "INBOX", uid, "delete", None, "t")
                .unwrap();
        }

        // Normal sync (uidvalidity unchanged): prune only the vanished uid (2).
        let tx = db.conn.transaction().expect("tx");
        Database::maintain_pending_actions_after_sync_in_tx(&tx, "acct", "INBOX", folder_id, false)
            .expect("maintain prune");
        tx.commit().expect("commit");
        let remaining: Vec<u32> = db
            .list_pending_actions(&PendingScope::All)
            .unwrap()
            .into_iter()
            .map(|r| r.uid)
            .collect();
        assert_eq!(remaining, vec![1, 3]);

        // UIDVALIDITY change: clear the folder's staged actions wholesale.
        let tx = db.conn.transaction().expect("tx");
        Database::maintain_pending_actions_after_sync_in_tx(&tx, "acct", "INBOX", folder_id, true)
            .expect("maintain clear");
        tx.commit().expect("commit");
        assert!(
            db.list_pending_actions(&PendingScope::All)
                .unwrap()
                .is_empty(),
            "a UIDVALIDITY change clears the folder's staged actions"
        );
    }

    /// Clearing executed rows by the snapshot that was applied must NOT clobber a
    /// row another window re-staged as a different action mid-flush.
    #[test]
    fn test_delete_pending_actions_matching_leaves_restaged_row() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");

        db.stage_pending_action("acct", "INBOX", 1, "delete", None, "t")
            .unwrap();
        db.stage_pending_action("acct", "INBOX", 2, "delete", None, "t")
            .unwrap();
        // Mid-flush, uid 2 is re-staged as a move (upsert updates the same row).
        db.stage_pending_action("acct", "INBOX", 2, "move", Some("Archive"), "t2")
            .unwrap();

        // Clearing the executed "delete" snapshot removes only uid 1; uid 2's new
        // move survives (applied on the next flush, or pruned if its uid vanishes).
        let removed = db
            .delete_pending_actions_matching("acct", "INBOX", &[1, 2], "delete", None)
            .expect("matching delete");
        assert_eq!(removed, 1);

        let remaining: Vec<(u32, String)> = db
            .list_pending_actions(&PendingScope::All)
            .unwrap()
            .into_iter()
            .map(|r| (r.uid, r.action_type))
            .collect();
        assert_eq!(remaining, vec![(2, "move".to_string())]);
    }

    /// The task tables exactly as version 17 created them, on the path a fresh
    /// v17 install took. Kept verbatim so that migration 18 is exercised
    /// against what is really on disk, rather than against a modern schema with
    /// the version number turned down.
    const V17_TASK_SCHEMA: &str = "
            -- Task management tables
            CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY,
                subject TEXT NOT NULL,
                body_text TEXT,
                snooze_until TEXT,
                snooze_until_unix INTEGER,
                deadline TEXT,
                deadline_unix INTEGER,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                completed_at TEXT,
                position REAL NOT NULL,
                is_working_on BOOLEAN NOT NULL DEFAULT 0,
                is_completed BOOLEAN NOT NULL DEFAULT 0,
                is_dirty BOOLEAN NOT NULL DEFAULT 0,
                account_name TEXT NOT NULL,
                imap_uid INTEGER,
                message_id TEXT UNIQUE,
                modseq INTEGER,
                UNIQUE(account_name, imap_uid)
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_today ON tasks(snooze_until_unix) WHERE is_completed = 0;
            CREATE INDEX IF NOT EXISTS idx_tasks_today_full ON tasks(is_completed, is_working_on, snooze_until_unix) WHERE is_completed = 0;
            CREATE INDEX IF NOT EXISTS idx_tasks_deadline ON tasks(deadline_unix) WHERE is_completed = 0 AND deadline_unix IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_tasks_position ON tasks(position) WHERE is_completed = 0;
            CREATE INDEX IF NOT EXISTS idx_tasks_working_on ON tasks(is_working_on) WHERE is_working_on = 1;
            CREATE INDEX IF NOT EXISTS idx_tasks_account ON tasks(account_name);
            CREATE INDEX IF NOT EXISTS idx_tasks_dirty ON tasks(is_dirty) WHERE is_dirty = 1;

            CREATE TABLE IF NOT EXISTS task_reminders (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL,
                remind_at TEXT NOT NULL,
                remind_at_unix INTEGER NOT NULL,
                is_triggered BOOLEAN NOT NULL DEFAULT 0,
                FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_reminders_task ON task_reminders(task_id);
            CREATE INDEX IF NOT EXISTS idx_reminders_pending ON task_reminders(remind_at_unix) WHERE is_triggered = 0;

            CREATE TABLE IF NOT EXISTS task_linked_emails (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL,
                message_id TEXT NOT NULL,
                cached_subject TEXT,
                cached_from TEXT,
                cached_date TEXT,
                cached_account TEXT,
                cached_folder TEXT,
                FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
                UNIQUE(task_id, message_id)
            );
            CREATE INDEX IF NOT EXISTS idx_linked_emails_task ON task_linked_emails(task_id);
            CREATE INDEX IF NOT EXISTS idx_linked_emails_message_id ON task_linked_emails(message_id);

            CREATE TABLE IF NOT EXISTS task_attachments (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL,
                filename TEXT NOT NULL,
                content_type TEXT,
                size INTEGER,
                file_path TEXT NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_attachments_task ON task_attachments(task_id);

            CREATE TABLE IF NOT EXISTS task_sync_state (
                id INTEGER PRIMARY KEY,
                account_name TEXT NOT NULL UNIQUE,
                folder_name TEXT NOT NULL DEFAULT '.Tasks',
                uidvalidity INTEGER,
                highest_modseq INTEGER,
                last_sync_time TEXT
            );
";

    /// What a version 17 database owes to its migrations rather than to its
    /// schema batch: `date_unix`, which every database gets by `ALTER` because
    /// `CORE_SCHEMA` has never declared it, and the two indexes over columns
    /// the migrations added.
    ///
    /// Reproduced with the text those migrations wrote, down to the line
    /// breaks: `sqlite_master` keeps a statement as it was given, so an index
    /// declared over two lines is not the same entry as one declared over one.
    const V17_MIGRATED_SCHEMA: &str = "ALTER TABLE messages ADD COLUMN date_unix INTEGER;
                 CREATE INDEX IF NOT EXISTS idx_messages_folder_date_unix ON messages(folder_id, date_unix DESC);
                 CREATE INDEX IF NOT EXISTS idx_messages_body_pending
                     ON messages(folder_id, text_part) WHERE body_text IS NULL;";

    /// A database in the shape version 17 left behind: everything but the
    /// tasks from `CORE_SCHEMA` (which has not changed), the v8 task tables,
    /// the indexes the migrations added along the way, and the version pinned
    /// at 17.
    fn v17_database() -> Database {
        let db = Database::new(":memory:").expect("db");
        db.conn.execute_batch(CORE_SCHEMA).expect("core schema");
        db.conn.execute_batch(V17_TASK_SCHEMA).expect("v8 tasks");
        db.conn
            .execute_batch(V17_MIGRATED_SCHEMA)
            .expect("what the migrations added");
        db.conn
            .execute(
                "INSERT OR IGNORE INTO sync_meta (key, value) VALUES ('sync_version', 0)",
                [],
            )
            .expect("sync_meta");
        db.set_schema_version(17).expect("pin the version");
        db
    }

    /// Every object in the schema, sorted, so that the order things happened to
    /// be created in is not part of the comparison but their definitions are.
    fn schema_objects(db: &Database) -> Vec<(String, String, String)> {
        let mut stmt = db
            .conn
            .prepare("SELECT type, name, COALESCE(sql, '') FROM sqlite_master ORDER BY type, name")
            .expect("prepare");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query")
            .collect::<rusqlite::Result<Vec<(String, String, String)>>>()
            .expect("collect")
    }

    /// How SQLite says it would answer a query, as one string.
    fn query_plan(db: &Database, sql: &str) -> String {
        let mut stmt = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap_or_else(|error| panic!("{sql}\ncannot be planned: {error}"));
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(3))
            .expect("plan")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("collect plan");
        rows.join("; ")
    }

    /// A parent that arrives after a reply which, for want of it, had joined
    /// an older conversation by its subject: the reply comes over, the older
    /// conversation stays whole.
    #[test]
    fn a_late_parent_does_not_drag_an_unrelated_conversation_along() {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let inbox = db.get_or_create_folder("A", "INBOX").expect("folder");
        let message = |uid: u32, subject: &str, id: &str, parent: Option<&str>, date: &str| {
            let mut m = crate::models::Message::new(inbox, uid);
            m.subject = Some(subject.to_string());
            m.message_id = Some(id.to_string());
            m.date = Some(date.to_string());
            m.headers = Some(crate::threads::ThreadHeaders {
                in_reply_to: parent.map(str::to_string),
                ..Default::default()
            });
            m
        };
        // March: "Plan" and a reply to it.
        db.insert_message(&message(1, "Plan", "<old@x>", None, "2026-03-01T00:00:00Z"))
            .unwrap();
        db.insert_message(&message(
            2,
            "Re: Plan",
            "<old2@x>",
            Some("<old@x>"),
            "2026-03-02T00:00:00Z",
        ))
        .unwrap();
        // September: the reply to a new "Plan" arrives before the new "Plan".
        db.insert_message(&message(
            4,
            "Re: Plan",
            "<c@x>",
            Some("<p@x>"),
            "2026-09-02T00:00:00Z",
        ))
        .unwrap();
        db.insert_message(&message(3, "Plan", "<p@x>", None, "2026-09-01T00:00:00Z"))
            .unwrap();
        let thread_of = |uid: u32| -> String {
            db.conn
                .query_row(
                    "SELECT thread_id FROM messages WHERE uid = ?1",
                    [uid],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(thread_of(1), "<old@x>");
        assert_eq!(thread_of(2), "<old@x>", "March stays as it was");
        assert_eq!(thread_of(3), "<p@x>");
        assert_eq!(thread_of(4), "<p@x>", "the reply found its parent");
    }

    /// A renamed folder takes the folders inside it along, and a stale row
    /// under the new name gives way.
    #[test]
    fn a_rename_follows_the_folders_inside() {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        for name in ["Work", "Work.Reports", "Work/Old", "Workshop", "Jobs"] {
            db.get_or_create_folder("A", name).expect("folder");
        }
        db.rename_folder("A", "Work", "Jobs").expect("rename");
        let mut names: Vec<String> = db
            .get_folders_by_account("A")
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["Jobs", "Jobs.Reports", "Jobs/Old", "Workshop"]);
    }

    /// `_` and `%` in a folder's name are letters of the name, not wildcards:
    /// renaming `Wor_` must not take `Work` along.
    #[test]
    fn a_rename_takes_a_wildcard_in_the_name_literally() {
        let mut db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        for name in ["Wor_", "Wor_.Sub", "Work", "Work.Reports", "Wo%", "Wo%.Sub"] {
            db.get_or_create_folder("A", name).expect("folder");
        }
        db.rename_folder("A", "Wor_", "X").expect("rename");
        db.rename_folder("A", "Wo%", "Y").expect("rename");
        let mut names: Vec<String> = db
            .get_folders_by_account("A")
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["Work", "Work.Reports", "X", "X.Sub", "Y", "Y.Sub"]
        );
    }

    /// A database a newer build wrote is refused, named with both versions,
    /// rather than written around.
    #[test]
    fn a_database_from_a_newer_orb_is_refused() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("create");
        db.set_schema_version(SCHEMA_VERSION + 1)
            .expect("pin ahead");
        let error = db.init_schema().expect_err("refused");
        let text = error.to_string();
        assert!(text.contains("newer Orb"), "{text}");
        assert!(
            text.contains(&format!("schema {}", SCHEMA_VERSION + 1)),
            "{text}"
        );
    }

    /// A database in the shape version 8 left behind: the core tables as they
    /// stood before every column the later migrations added, the v8 task
    /// tables, the standalone full-text index of the time, and the version
    /// pinned at 8. Built from the current text by dropping what came later,
    /// which SQLite can do since 3.35.
    fn v8_database() -> Database {
        let db = Database::new(":memory:").expect("db");
        db.conn.execute_batch(CORE_SCHEMA).expect("core schema");
        for column in [
            "has_attachments",
            "text_part",
            "html_part",
            "text_encoding",
            "text_bytes",
            "shape_unreadable",
        ] {
            db.conn
                .execute(&format!("ALTER TABLE messages DROP COLUMN {column}"), [])
                .expect("drop a later column");
        }
        db.conn
            .execute("ALTER TABLE folders DROP COLUMN flags_checked_at", [])
            .expect("drop a later column");
        for table in ["pending_actions", "outbox"] {
            db.conn
                .execute(&format!("DROP TABLE {table}"), [])
                .expect("drop a later table");
        }
        db.conn
            .execute_batch(
                "ALTER TABLE messages ADD COLUMN date_unix INTEGER;
                 CREATE INDEX IF NOT EXISTS idx_messages_folder_date_unix
                     ON messages(folder_id, date_unix DESC);",
            )
            .expect("what migration 6 added");
        db.conn.execute_batch(V17_TASK_SCHEMA).expect("v8 tasks");
        db.conn
            .execute_batch(
                "CREATE VIRTUAL TABLE messages_fts USING fts5(subject, from_addr, to_addr, body_text);",
            )
            .expect("the standalone index of v8");
        db.conn
            .execute(
                "INSERT OR IGNORE INTO sync_meta (key, value) VALUES ('sync_version', 0)",
                [],
            )
            .expect("sync_meta");
        db.conn
            .execute_batch(
                "INSERT INTO folders (account_name, name) VALUES ('Work', 'INBOX');
                 INSERT INTO messages (folder_id, uid, message_id, subject, from_addr, date, flags)
                     VALUES (1, 1, '<a@x>', 'Hello', 'a@x', '2026-01-01T00:00:00Z', '[\"\\\\Seen\"]');
                 INSERT INTO messages (folder_id, uid, message_id, subject, from_addr, date, flags)
                     VALUES (1, 2, '<b@x>', 'Re: Hello', 'b@x', '2026-01-02T00:00:00Z', '[]');
                 INSERT INTO tasks (subject, account_name, created_at, updated_at, position)
                     VALUES ('Call back', 'Work', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1.0);",
            )
            .expect("some rows");
        db.set_schema_version(8).expect("pin the version");
        db
    }

    /// A version 8 database walks every migration to the current version,
    /// keeps its mail and its task, and ends in the same schema as a fresh
    /// one.
    #[test]
    fn a_version_8_database_walks_to_the_current_version() {
        let upgraded = v8_database();
        upgraded.init_schema().expect("every migration from 8 runs");
        assert_eq!(upgraded.get_schema_version().unwrap(), SCHEMA_VERSION);

        let messages: i64 = upgraded
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(messages, 2, "the mail survives");
        let (unread, total) = upgraded.get_folder_counts(1).unwrap();
        assert_eq!((unread, total), (1, 2));
        let tasks: i64 = upgraded
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tasks, 1, "the task survives");
        let hits = upgraded
            .search_messages("hello", &SearchScope::All, 10, None)
            .expect("the index was rebuilt");
        assert_eq!(hits.len(), 2);

        // The same objects and the same columns as a fresh database. Not the
        // same `CREATE` text: a column added by `ALTER` sits at the end of the
        // table in the text, where a fresh table declares it in its place.
        let fresh = Database::new(":memory:").expect("db");
        fresh.init_schema().expect("create at the current version");
        let names = |db: &Database| -> Vec<(String, String)> {
            schema_objects(db)
                .into_iter()
                .map(|(kind, name, _)| (kind, name))
                .filter(|(_, name)| !name.starts_with("sqlite_"))
                .collect()
        };
        assert_eq!(
            names(&upgraded),
            names(&fresh),
            "the same tables, indexes and triggers"
        );
        let columns = |db: &Database, table: &str| -> Vec<String> {
            let mut stmt = db
                .conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let mut columns: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            columns.sort();
            columns
        };
        for (kind, name) in names(&fresh) {
            if kind == "table" {
                assert_eq!(
                    columns(&upgraded, &name),
                    columns(&fresh, &name),
                    "columns of {name}"
                );
            }
        }
    }

    /// A version 17 database holding three messages with their bodies in the
    /// row, as every database did before migration 30.
    fn database_with_bodies_in_the_row() -> Database {
        let db = v17_database();
        db.conn
            .execute_batch(
                "INSERT INTO folders (id, account_name, name) VALUES (1, 'A', 'INBOX');
                 INSERT INTO messages (id, folder_id, uid, subject, body_text, body_html, rfc822,
                                       text_part, flags)
                 VALUES (1, 1, 1, 'Budget', 'the quarterly figures', '<p>figures</p>',
                         X'4652', '1', '[]'),
                        (2, 1, 2, 'Lunch', 'tuesday at noon', NULL, NULL, '1', '[]'),
                        (3, 1, 3, 'Waiting', NULL, NULL, NULL, '1', '[]');",
            )
            .expect("messages with their bodies");
        db
    }

    /// Migration 30 takes the bodies out of the message row and leaves them
    /// readable, searchable and counted as fetched.
    #[test]
    fn the_bodies_move_out_of_the_message_row() {
        let db = database_with_bodies_in_the_row();
        db.init_schema().expect("upgrade");

        for column in ["body_text", "body_html", "rfc822"] {
            assert!(!Database::column_exists(&db.conn, "messages", column).unwrap());
        }
        let first = db.get_message_by_id(1).unwrap().expect("message");
        assert_eq!(first.body_text.as_deref(), Some("the quarterly figures"));
        assert_eq!(first.body_html.as_deref(), Some("<p>figures</p>"));
        assert_eq!(first.rfc822.as_deref(), Some(&b"FR"[..]));
        assert_eq!(
            db.search_messages("quarterly", &SearchScope::All, 10, None)
                .unwrap()
                .len(),
            1,
            "the moved text is searched"
        );
        // Only the message whose text never came is still waiting for it.
        let pending: Vec<i64> = db
            .pending_bodies(1, 10)
            .unwrap()
            .into_iter()
            .map(|body| body.message_id)
            .collect();
        assert_eq!(pending, vec![3]);
    }

    /// A migration stopped between two batches picks up where it stopped.
    #[test]
    fn an_interrupted_move_of_the_bodies_resumes() {
        let db = database_with_bodies_in_the_row();
        // What the first batch of a run that was then killed leaves behind.
        db.conn
            .execute_batch(
                "CREATE TABLE message_bodies (
                     message_id INTEGER PRIMARY KEY,
                     body_text TEXT,
                     body_html TEXT,
                     rfc822 BLOB
                 );
                 ALTER TABLE messages ADD COLUMN has_body_text INTEGER;
                 INSERT INTO message_bodies SELECT id, body_text, body_html, rfc822
                     FROM messages WHERE id = 1;
                 UPDATE messages SET has_body_text = 1, body_text = NULL, body_html = NULL,
                     rfc822 = NULL WHERE id = 1;",
            )
            .expect("half a migration");
        db.init_schema().expect("resume");

        assert_eq!(db.get_schema_version().unwrap(), SCHEMA_VERSION);
        for (id, text) in [(1, "the quarterly figures"), (2, "tuesday at noon")] {
            let message = db.get_message_by_id(id).unwrap().expect("message");
            assert_eq!(message.body_text.as_deref(), Some(text));
        }
        assert_eq!(
            db.search_messages("noon", &SearchScope::All, 10, None)
                .unwrap()
                .len(),
            1
        );
    }

    /// The index follows a body that arrives, changes and goes with its
    /// message, and the body table keeps nothing of a deleted message.
    #[test]
    fn the_search_index_follows_the_bodies_kept_apart() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let inbox = db.insert_folder("A", "INBOX").expect("folder");
        let mut message = crate::models::Message::new(inbox, 1);
        message.subject = Some("Note".into());
        let id = db.insert_message(&message).expect("insert");
        let found = |term: &str| {
            db.search_messages(term, &SearchScope::All, 10, None)
                .unwrap()
                .len()
        };

        db.set_message_body_text(id, "apples and pears").unwrap();
        assert_eq!(found("apples"), 1);
        db.set_message_body_text(id, "only pears now").unwrap();
        assert_eq!(found("apples"), 0);
        assert_eq!(found("pears"), 1);
        db.conn
            .execute("UPDATE messages SET subject = 'Memo' WHERE id = ?1", [id])
            .unwrap();
        assert_eq!(found("memo"), 1);
        assert_eq!(
            found("pears"),
            1,
            "the body stays indexed under a new subject"
        );

        let bodies = || -> i64 {
            db.conn
                .query_row("SELECT COUNT(*) FROM message_bodies", [], |row| row.get(0))
                .unwrap()
        };
        db.conn
            .execute("DELETE FROM messages WHERE id = ?1", [id])
            .unwrap();
        assert_eq!(found("pears"), 0);
        assert_eq!(bodies(), 0);

        // A folder deleted takes its messages' bodies along, through the cascade.
        let other = db.insert_message(&message).expect("insert");
        db.set_message_body_text(other, "plums").unwrap();
        db.conn
            .execute("DELETE FROM folders WHERE id = ?1", [inbox])
            .unwrap();
        assert_eq!(found("plums"), 0);
        assert_eq!(bodies(), 0);
        db.conn
            .execute_batch("INSERT INTO messages_fts(messages_fts) VALUES ('integrity-check');")
            .expect("the index agrees with what it was built from");
    }

    /// The promise the migration has to keep: a database that upgraded and a
    /// database that was created fresh are the same database. They were not
    /// before v18 — the v8 task tables were declared once in migration 8 and
    /// once in the fresh-database batch, and the two texts had drifted — which
    /// is why migration 18 rebuilds all of them from one constant.
    #[test]
    fn an_upgraded_database_and_a_new_one_have_the_same_schema() {
        let upgraded = v17_database();
        upgraded
            .init_schema()
            .expect("upgrade to the current version");

        let fresh = Database::new(":memory:").expect("db");
        fresh.init_schema().expect("create at the current version");

        assert_eq!(upgraded.get_schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(fresh.get_schema_version().unwrap(), SCHEMA_VERSION);

        // Reported as a list rather than as the first mismatch: a schema that
        // has drifted has usually drifted in several places, and finding them
        // one test run at a time is how drift survives.
        let (upgraded, fresh) = (schema_objects(&upgraded), schema_objects(&fresh));
        let mut differences: Vec<String> = Vec::new();
        for object in upgraded.iter() {
            match fresh.iter().find(|o| o.1 == object.1) {
                None => differences.push(format!("{} exists only after an upgrade", object.1)),
                Some(other) if other.2 != object.2 => differences.push(format!(
                    "{} is defined differently:\n  upgraded: {}\n  new:      {}",
                    object.1, object.2, other.2
                )),
                Some(_) => {}
            }
        }
        for object in fresh.iter() {
            if !upgraded.iter().any(|o| o.1 == object.1) {
                differences.push(format!("{} exists only in a new database", object.1));
            }
        }
        assert!(
            differences.is_empty(),
            "an upgraded database is not the same as a new one:\n{}",
            differences.join("\n")
        );
    }

    /// A populated database must survive the rebuild: identities are handed
    /// out, nothing is dropped, and the rows that hang off a task still hang
    /// off it afterwards.
    #[test]
    fn the_upgrade_gives_every_existing_task_an_identity() {
        let db = v17_database();
        db.conn
            .execute_batch(
                "INSERT INTO tasks (id, subject, created_at, updated_at, position,
                                    account_name, imap_uid, message_id, is_dirty)
                 VALUES (1, 'Buy milk', '2026-08-01T09:00:00Z', '2026-08-01T09:00:00Z',
                         1024.0, 'Work', 7, '<one@orb.local>', 1),
                        (2, 'Call Bob', '2026-08-02T09:00:00Z', '2026-08-02T09:00:00Z',
                         2048.0, 'Work', 8, NULL, 0);
                 INSERT INTO task_reminders (task_id, remind_at, remind_at_unix)
                 VALUES (1, '2026-08-06T09:00:00Z', 1786006800);
                 INSERT INTO task_linked_emails (task_id, message_id, cached_subject)
                 VALUES (1, '<mail@example.com>', 'Invoice');
                 INSERT INTO task_attachments (task_id, filename, file_path)
                 VALUES (1, 'invoice.pdf', '/tmp/invoice.pdf');
                 INSERT INTO task_sync_state (account_name, uidvalidity, folder_name)
                 VALUES ('Work', 42, 'INBOX.Tasks');",
            )
            .expect("populate the old shape");

        db.init_schema().expect("upgrade");

        // Every task kept its row and its id, and gained an identity of its own.
        let tasks: Vec<(i64, String, String, i64)> = {
            let mut stmt = db
                .conn
                .prepare("SELECT id, subject, task_uuid, revision FROM tasks ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].1, "Buy milk");
        assert_eq!(tasks[1].1, "Call Bob");
        assert!(tasks.iter().all(|t| t.2.len() == 36), "{tasks:?}");
        assert_ne!(tasks[0].2, tasks[1].2, "two tasks, two identities");
        assert!(tasks.iter().all(|t| t.3 == 0), "revisions start at zero");

        // The children came across and still belong to their task.
        let children: (i64, i64, i64) = db
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM task_reminders WHERE task_id = 1),
                        (SELECT COUNT(*) FROM task_linked_emails WHERE task_id = 1),
                        (SELECT COUNT(*) FROM task_attachments WHERE task_id = 1)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(children, (1, 1, 1));

        // The attachment has an identity too, and counts as not yet cached:
        // there is no message on the server holding its payload.
        let (attachment_uuid, is_cached): (String, i64) = db
            .conn
            .query_row(
                "SELECT attachment_uuid, is_cached FROM task_attachments",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(attachment_uuid.len(), 36);
        assert_eq!(is_cached, 0);

        // Sync state survived, including a folder name that is not the default.
        let (folder, uidvalidity): (String, i64) = db
            .conn
            .query_row(
                "SELECT folder_name, uidvalidity FROM task_sync_state",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(folder, "INBOX.Tasks");
        assert_eq!(uidvalidity, 42);

        // Nothing was left pointing at a table that is no longer there, which
        // is the way a rebuild like this goes wrong: SQLite repoints a child's
        // `REFERENCES` when its parent is renamed, so a child that was not
        // rebuilt alongside it would still name `tasks_pre_v18`.
        let violations: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .expect("foreign key check");
        assert_eq!(violations, 0, "the rebuild left a dangling foreign key");

        // The foreign keys survived the rebuild: deleting a task still takes
        // everything hanging off it with it.
        db.conn
            .execute("DELETE FROM tasks WHERE id = 1", [])
            .expect("delete");
        let orphans: i64 = db
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM task_reminders)
                      + (SELECT COUNT(*) FROM task_linked_emails)
                      + (SELECT COUNT(*) FROM task_attachments)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0, "ON DELETE CASCADE was lost in the rebuild");
    }

    /// The Message-ID is the id of the message a task is currently stored in,
    /// and every edit rewrites the task into a new one. Two tasks may therefore
    /// name the same message — during a rewrite, or after a duplicate the
    /// revision rule has not resolved yet — and the v8 UNIQUE constraint would
    /// have turned that into a failed sync rather than a resolvable conflict.
    #[test]
    fn a_message_id_no_longer_claims_to_identify_a_task() {
        let db = v17_database();
        db.init_schema().expect("upgrade");

        db.conn
            .execute_batch(
                "INSERT INTO tasks (task_uuid, subject, created_at, updated_at, position,
                                    account_name, imap_uid, message_id)
                 VALUES ('a', 'One', 'now', 'now', 1.0, 'Work', 1, '<same@orb.local>'),
                        ('b', 'Two', 'now', 'now', 2.0, 'Work', 2, '<same@orb.local>');",
            )
            .expect("two tasks may name one message");

        // The identity, on the other hand, is an identity.
        let clash = db.conn.execute_batch(
            "INSERT INTO tasks (task_uuid, subject, created_at, updated_at, position,
                                account_name, imap_uid)
             VALUES ('a', 'Three', 'now', 'now', 3.0, 'Work', 3);",
        );
        assert!(clash.is_err(), "task_uuid must be unique");
    }

    /// Each view in the sidebar is a filtered projection of one global list, so
    /// each needs an index whose predicate matches its filter — otherwise
    /// 'Someday' is a scan of every task that ever existed, trashed ones
    /// included. This asserts SQLite actually reaches for them.
    #[test]
    fn every_task_view_has_an_index_that_excludes_the_trash() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");

        let cases = [
            (
                "idx_tasks_open_postpone",
                "SELECT id FROM tasks INDEXED BY idx_tasks_open_postpone
                 WHERE is_completed = 0 AND deleted_at IS NULL
                   AND snooze_until_unix <= 100 ORDER BY snooze_until_unix",
            ),
            (
                "idx_tasks_open_due",
                "SELECT id FROM tasks INDEXED BY idx_tasks_open_due
                 WHERE is_completed = 0 AND deleted_at IS NULL
                   AND deadline_unix <= 100 ORDER BY deadline_unix",
            ),
            (
                "idx_tasks_someday",
                "SELECT id FROM tasks INDEXED BY idx_tasks_someday
                 WHERE is_completed = 0 AND deleted_at IS NULL
                   AND snooze_until_unix IS NULL AND deadline_unix IS NULL ORDER BY position",
            ),
            (
                "idx_tasks_completed",
                "SELECT id FROM tasks INDEXED BY idx_tasks_completed
                 WHERE is_completed = 1 AND deleted_at IS NULL
                 ORDER BY completed_at DESC LIMIT 50",
            ),
            (
                "idx_tasks_trash",
                "SELECT id FROM tasks INDEXED BY idx_tasks_trash
                 WHERE deleted_at IS NOT NULL ORDER BY deleted_at DESC",
            ),
            (
                "idx_tasks_dirty",
                "SELECT id FROM tasks INDEXED BY idx_tasks_dirty
                 WHERE is_dirty = 1 ORDER BY revision",
            ),
            (
                "idx_task_log_task",
                "SELECT entry_uuid FROM task_log INDEXED BY idx_task_log_task
                 WHERE task_id = 1 ORDER BY occurred_at_unix",
            ),
            (
                "idx_task_tag_links_tag",
                "SELECT task_id FROM task_tag_links INDEXED BY idx_task_tag_links_tag
                 WHERE tag_uuid = 'x'",
            ),
        ];

        // `INDEXED BY` is the assertion. SQLite refuses to prepare a query it
        // cannot answer with the named index, and a partial index whose
        // predicate it cannot prove the query implies is one it cannot answer
        // with — which is exactly the mistake to catch, since an index whose
        // predicate does not match its view is an index nothing ever uses.
        // Which index the planner would pick unaided depends on statistics
        // that an empty table does not have; whether the index fits the view
        // does not.
        for (index, sql) in cases {
            let plan = query_plan(&db, sql);
            assert!(
                plan.contains(index),
                "the view backed by {index} would be answered by: {plan}"
            );
        }
    }

    /// Resolving a task's link back to its message starts with a Message-ID
    /// lookup in the local store. Without an index that is a scan of every
    /// message in the database, which is the one thing this lookup cannot be:
    /// it runs while the user waits, years after the link was made.
    #[test]
    fn a_message_can_be_found_by_its_message_id_without_a_scan() {
        let db = Database::new(":memory:").expect("db");
        db.init_schema().expect("schema");
        let plan = query_plan(
            &db,
            "SELECT id, folder_id FROM messages WHERE message_id = '<x@example.com>'",
        );
        assert!(
            plan.contains("idx_messages_message_id"),
            "the Message-ID lookup would be answered by: {plan}"
        );
    }

    /// An upgrade interrupted after its transaction committed but before the
    /// version was written re-runs the whole step on the next open. It must not
    /// rebuild the tables a second time: that would hand every task a new
    /// identity, and an identity that changes is not one.
    #[test]
    fn re_running_the_task_migration_keeps_the_identities_it_handed_out() {
        let db = v17_database();
        db.conn
            .execute_batch(
                "INSERT INTO tasks (id, subject, created_at, updated_at, position, account_name)
                 VALUES (1, 'Buy milk', 'now', 'now', 1024.0, 'Work');",
            )
            .expect("populate");
        db.init_schema().expect("upgrade");

        let first: String = db
            .conn
            .query_row("SELECT task_uuid FROM tasks WHERE id = 1", [], |r| r.get(0))
            .unwrap();

        db.set_schema_version(17).expect("as if it never finished");
        db.run_migrations(17).expect("re-running must be safe");

        let second: String = db
            .conn
            .query_row("SELECT task_uuid FROM tasks WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first, second, "a re-run must not re-identify the tasks");
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
