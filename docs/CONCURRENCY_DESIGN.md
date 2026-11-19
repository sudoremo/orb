# Concurrency and Multi-Instance Model

What the Orb core (`src/`) guarantees when several threads, several syncs, or
several Orb processes touch the same database and the same IMAP account. Read
this before changing sync, the store or the schema.

## Overview

- The desktop app (`app/src-tauri`) is a thin shell over `OrbHandle`
  (`src/ffi.rs`). All mail, sync and storage logic lives in the core.
- State lives in a single SQLite database (`Config::db_path`) in WAL mode.
  Every core call opens its own connection; there is no pool and no in-memory
  cache shared between calls.
- Concurrency control has three layers:
  1. **In-process:** `Store::write_lock`, a `tokio::sync::Mutex`, serialises
     database writes inside one process.
  2. **Between syncs:** the `sync_locks` table keeps two syncs, whether in the
     same process or in different ones, from working on the same folder at
     once.
  3. **Between a sync and any other writer:** optimistic locking on
     `folders.sync_version` detects a write that lands while a sync is
     fetching, and makes the sync retry from a fresh baseline.
- IMAP IDLE tells the core when a folder changed on the server (whichever
  client changed it), and the core syncs that folder. A scheduler
  (`services::SyncScheduler`) also syncs every account at its interval, and
  after the machine wakes from sleep or moves to another network
  (`services::network`, which watches the machine's own addresses): both
  empty the connection pool, reopen IDLE and sync every account.

## Threading model (app ↔ core)

- `OrbHandle` owns one multi-threaded tokio `Runtime`, created in `ffi::init`.
  Its public methods are synchronous: each one `block_on`s that runtime
  (`OrbHandle::run_sync`, `mark_read`, `delete_messages`, …).
- Tauri commands are `async` and never call `OrbHandle` directly. They go
  through `offload` (`app/src-tauri/src/main.rs`), which runs the call on
  `tauri::async_runtime::spawn_blocking`. Blocking on the core's runtime is
  allowed from a blocking-pool thread, and the webview thread is never stalled
  by IMAP round-trips. A panic inside the closure comes back as a
  `SyncError` ("worker thread failed"), not as a hang.
- Several commands can therefore run at the same time, each on its own
  blocking thread and each with its own SQLite connection. Nothing in the app
  layer serialises them. Correctness rests on the three layers listed above.
- Long-lived work is spawned onto the core runtime:
  - `OrbHandle::start_push` starts the `SyncScheduler`, which owns the IDLE
    service (`services::IdleService`), a periodic timer per account and a
    wake detector. Every request it receives — a push notification, a timer
    tick, a wake — is coalesced per folder (or per account) and run as a
    `Store::sync` task on the runtime; a request for something already
    running is run once more when it ends. The result goes to the app's
    `PushListener` (`on_synced` / `on_error`), and the app emits
    `folder-changed` only if the sync changed something.
  - `forward_progress` turns `StateUpdate::SyncProgress` and
    `StateUpdate::RetryAttempt` into `SyncProgressListener::on_progress`,
    which the app emits as `sync-progress`.
  - `OrbHandle::start_chat` starts the chat service (`chat::ChatService`)
    when the config has a `chat` section: one loop per process, described
    under [Chat](#chat).
  - `OrbHandle::shutdown`, called from the app's exit handler, cancels the
    scheduler and the IDLE watches, logs out the pooled connections and
    releases this instance's folder locks, all within five seconds.
- `Store::sync` syncs all accounts concurrently (`join_all`) and the folders
  of each account one after another on that account's IMAP connection, after
  draining the account's outbox on it.

`StateUpdate` (`src/commands.rs`) has three variants: `SyncProgress`,
`RetryAttempt` and `ExternalChange`.

## Database

### Connection setup

`Database::new` (`src/db/connection.rs`) creates the parent directory, opens
the file and sets:

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA busy_timeout = 5000;   -- constants::DB_BUSY_TIMEOUT_MS
```

`mmap_size` is deliberately not set (see the comment in `Database::new`).
`Store::reader` and `Store::writer` are both just `Database::new`: they differ
in intent, not in mode. WAL lets readers run alongside the single writer, and
`busy_timeout` makes a writer that finds the database locked wait up to 5 s
before failing with `SQLITE_BUSY`.

### Transactions

- `Database::transaction` and `Database::immediate_transaction` use
  `TransactionBehavior::Immediate`: the write lock is taken at `BEGIN`, so a
  transaction never fails halfway through because it needed to upgrade from
  reading to writing. All schema migrations do the same.
- Every write transaction is IMMEDIATE, including the bulk message insert
  and the UID deletes.
- Never `await` while holding a transaction: closures passed to `transaction`
  are synchronous. IMAP work happens between transactions, not inside them.

### `write_lock`

`Store::write_lock` serialises writes within a process. It exists because
concurrent writes corrupted the FTS index. Sync takes it only around each
short database step (`Store::locked_write`) and releases it during network
I/O, so user actions such as delete or move are not blocked while a sync
fetches. Other writers (`Store::delete_messages_by_uids`,
`Store::reconcile_with_config`, `Store::shutdown`, the body fetchers in
`ffi.rs`) hold it for their whole write. It does nothing across processes.
Across processes, SQLite's own locking applies.

### Schema facts that concurrency relies on

- `messages.folder_id` uses `ON DELETE CASCADE`, so deleting a folder never
  leaves orphaned messages. This needs `foreign_keys = ON` on every
  connection, which `Database::new` sets.
- A message's body, HTML and source live in `message_bodies`, keyed by the
  message id, so that a row of `messages` stays small enough to read without
  its bodies. The `messages_fts_ad` trigger deletes the body with its
  message, cascaded deletes included; the full-text index reads both tables
  through the `messages_search` view, and the triggers on both keep it in
  step. A body is written with an upsert, never `INSERT OR REPLACE`, whose
  delete fires no trigger.
- `folders.sync_version` is a per-folder counter for optimistic locking (see
  below).
- `sync_meta('sync_version')` is a global counter that only ever increases.
  Triggers (`folders_bump_change_counter_ai/au/ad`) bump it on every insert,
  update or delete of a `folders` row.
- `sync_locks(account_name, folder_name, locked_at, lock_holder)` has its
  primary key on `(account_name, folder_name)`.

### Location requirements

- Keep the database on a **local** filesystem. WAL coordinates through a
  shared-memory file (`-shm`) and needs every process on the same host. SQLite
  does not support WAL over network filesystems (NFS, SMB) at all.
- Do not put it in a folder that a sync service watches (iCloud Drive,
  Dropbox, Google Drive). These services copy the main file and the `-wal`
  and `-shm` files independently, and that corrupts the database.
- Several Orb processes on **one** machine may share the database. Several
  machines must each have their own database and sync from IMAP
  independently.

## Multi-instance coordination

### Sync locks

`Store::sync_folder` wraps every folder sync:

1. `Database::acquire_sync_lock` runs one IMMEDIATE transaction. It first
   deletes locks older than `SYNC_LOCK_TIMEOUT_SECONDS + CLOCK_SKEW_BUFFER_SECONDS`
   (300 + 60 s, `src/constants.rs`), then `INSERT`s the lock. A primary-key
   violation means "held", and the holder is read back.
2. If the lock is held, the folder is **skipped**: `SyncReport::folders_skipped`
   is incremented and `SyncReport::skipped` names the folder and the holder.
   It is not counted as synced. This happens just as much for a second sync
   of the same folder in the *same* process, for example a push-triggered
   sync overlapping a manual one; the scheduler asks again a few seconds
   later, up to six times.
3. While the sync runs, a `LockRenewer` task refreshes `locked_at` every
   `SYNC_LOCK_RENEW_INTERVAL_SECONDS` (60 s), so a fetch longer than the
   stale cutoff keeps its lock.
4. `Database::release_sync_lock` runs when `sync_folder_impl` returns, whether
   it succeeded or failed. It deletes only rows whose `lock_holder` is our
   own.
5. At startup, `Database::break_dead_locks` removes locks whose holder is a
   process on this host that no longer exists (`kill(pid, 0)`). Locks from
   other hosts and from live processes are left alone.

Folder names are upper-cased (`normalize_folder_name`) before locking.
`lock_holder` is `hostname:pid:uuid` (`get_lock_holder`): the `hostname`
crate plus a per-process UUID, so a reused PID never matches an old lock.

`Store::shutdown` closes the IMAP pool and calls
`Database::release_all_locks`. The app calls it, through `OrbHandle::shutdown`,
from its exit handler.

### Optimistic locking on `folders.sync_version`

`Store::sync_folder_impl` follows the two-transaction pattern:

0. **SELECT first.** The folder row is created only once the server has
   answered the SELECT, so a folder the server does not have is never created
   locally. SELECT also yields UIDVALIDITY, EXISTS and the HIGHESTMODSEQ
   snapshot.
1. **Transaction 1:** read the folder row (including `sync_version`) and the
   stored UIDs together. Reading both in one transaction keeps deletion
   detection from comparing against a different moment than the baseline. A
   changed UIDVALIDITY clears the folder in a transaction of its own (version
   checked and bumped) before anything is fetched.
2. **IMAP, no transaction, no write lock.** Incremental: fetch with
   CHANGEDSINCE, then the UIDNEXT gap only if CHANGEDSINCE brought nothing
   from it. First sync: list every UID, then fetch the ones not stored in
   pages of `INITIAL_FETCH_PAGE` (5 000), **committing each page** in its own
   version-checked transaction that also records UIDVALIDITY; `highest_modseq`
   stays NULL until the end, which is what marks the first sync as unfinished
   and resumable. Then detect deletions and fetch missing UIDs (at most
   `MISSING_MESSAGE_REPAIR_BUDGET`).
3. **Transaction 2 (IMMEDIATE, under `write_lock`):** re-read
   `folders.sync_version`. If it differs from the expected version (the
   baseline, advanced by every page committed), fail with
   `Error::ConcurrencyConflict`. Otherwise, in that one transaction: insert or
   update the remaining messages, delete vanished UIDs, prune staged actions
   (`maintain_pending_actions_after_sync_in_tx`), and update
   `uidvalidity`/`highest_uid`/`highest_modseq`/`last_sync_time` with
   `sync_version = sync_version + 1`.

A conflict sleeps for 100 ms and starts again from Transaction 1, for up to
`MAX_RETRY_ATTEMPTS` (3) attempts; pages already committed are not fetched
again.

**Invariant:** any write that changes a folder's message set outside sync must
bump that folder's `sync_version` in the same transaction. Otherwise a sync
that is fetching at that moment re-inserts what was just deleted.
`Store::delete_messages_by_uids` does this, and so do the other
`sync_version = sync_version + 1` sites in `src/store.rs`. A new writer must
do the same.

**Invariant:** a server command that names UIDs — a delete, a move, a queued
flag write, a body fetch — goes through `Store::select_for_uids`, which
selects the folder and refuses with `Error::UidValidityChanged` when the
UIDVALIDITY the server reports differs from the one the database recorded.
The UIDs in the database were issued under the recorded value; under a new
one they name other mail, or nothing. The next sync of the folder sees the
change and clears it (step 1 above). A write that succeeds on the server and
then fails locally is not rolled back — it cannot be — but is reported
(`WriteOutcome::warning`) and reconciled by the next sync.

### Filter rules

`Store::filter_new_mail` runs after each folder sync (and after the
"unchanged" fast path), on the sync's own connection, over the messages whose
UID is above `folders.last_filtered_uid`. The watermark is NULL until the
folder's first complete sync and is then set to `highest_uid`, so mail that was
there first is never filtered; a UIDVALIDITY change resets it. Before acting,
the range is claimed by a compare-and-set on the watermark, so two processes on
one database never run the rules on the same mail. A connection error during
the run moves the watermark back for the next sync; every action is idempotent
(flags are flags, a moved message is gone, a task is only made where none is
linked), so a range run twice converges. `filter_log` records every message a
rule acted on, by Message-ID, and the automatic run skips a message already
logged for its folder — which is what keeps a move that was undone from being
redone. The flags are written to the server first and then locally; the move
goes through `Store::delete_messages_by_uids`, which bumps `sync_version`.

Two Orbs on separate machines share no database, only the server, and both
store a new message before either has filed it. The run's header fetch is
also the test of which messages are still in the folder: a candidate the
server no longer has was filed by the other Orb (or deleted by anyone), and
is removed locally right there — `delete_messages_by_uids` again — rather
than listed, and announced, until a sync detects the deletion. Both Orbs may
still reach the move; the server moves what it finds and says OK either way,
so `ImapClient::move_messages` reads the COPYUID response (RFC 4315, which
MOVE carries too) and returns the UIDs it confirms. The `task` action is
made only for those: the task follows the message. Without UIDPLUS there is
no COPYUID, and the task is made as before.

### New-mail notifications

The sync reports what to announce rather than the app working it out from
the database. Transaction 2 of `sync_folder_impl` keeps the row ids it
inserted (`FolderSyncReport::added_ids`; a first sync keeps none, a folder's
past is not news), and after the filter rules have run on the folder
`Store::new_mail_among` reads back the ones that are still there and unread,
if the folder is watched — `SyncReport::new_mail`, which `SyncResult` carries
to the app and `notify_new_mail` posts. So a message a rule files away or
marks read is never announced, a sync of another account or folder ending
while this one's rules still run announces nothing of this one's mail, and
two Orbs on one database announce a message once: in the process whose sync
stored it.

### Focus

The running focus is one row of `focus_state` (mode, start, end), so every
Orb on the database sees it and it outlives a restart. What a focus holds
back is kept in memory by the process whose sync or chat stream brought it
(`OrbHandle::focus_held`): that process announces it when the focus ends or
its interval comes round, which it finds out by asking every 15 s
(`OrbHandle::focus_tick`). An Orb that ends the focus deletes the row; the
others see the row gone at their next tick and announce what they held. The
delete by the clock names the session's start, so a focus started meanwhile
by another Orb is left running. What a process held when it quit is not
announced.

### Feeds

Every Orb polls the reading list's feeds (`Store::poll_feeds`, every minute
from a thread in the app, each feed when its `reading.poll_minutes` have
passed), on every machine, so the same item can be appended to the reading
folder by several of them. An item is known by `feeds::item_id`, a hash of the
feed's address and the item's own id, carried in `X-Orb-Feed-Item-Id` and in a
`Message-ID` made from it; neither depends on the machine or the moment.
Before appending an item the poll searches the reading archive and the reading
folder for that header, and after appending it searches the reading folder
again: two copies mean another Orb appended it in the same instant, and every
copy but the one with the lowest UID is deleted. Both Orbs delete the same
copy, so they converge on one. The UIDs come from a search on the same
connection right after its SELECT, so they cannot name another mailbox.

Locally, `feed_items` remembers every item id this database has seen, so an
item archived and deleted is never appended again from here. A feed's `ETag`
and `Last-Modified` in `feeds` are only written once its items are on the
server: written first, a failed APPEND followed by a "not modified" would lose
them. The one gap is an item deleted on one machine before another has polled
it at all; that machine finds it on neither folder and appends it again. The
poll writes nothing to the message tables; it syncs the reading folder
afterwards, which does.

### Threads

A message joins its thread inside the transaction that stores it
(`Database::assign_thread`, called from `bulk_insert_messages_in_tx` and
`insert_message`), so a thread is never half-assigned across transactions and
the sync's version check covers it. Every message has a `thread_id` — its own
Message-ID, or `local:<rowid>` for one without — since migration 24, and the
thread queries read that column through `idx_messages_folder_thread_date`;
`NULL` must never be written to it. A message assigned again (the header
backfill) takes the thread it founded along; a reply that had joined some other
conversation by its subject moves alone, so a late parent never relabels an
unrelated thread. The backfill writes under `write_lock` and changes no folder's
message set.

### Chat

`chat::ChatService::run` is one loop per Orb process, on the core runtime. It
finds a session (the Keychain, or `token_command`), runs a pass, starts the
realtime stream beside itself (`chat::live`, a DDP websocket) and waits on:

- the stream, which names the rooms that changed (`Trigger::Room`; the rooms
  named within `ROOM_SETTLE`, 250 ms, go into one pass), says that the
  subscriptions changed (`Trigger::Full`), or that somebody's presence did —
  presence is kept in memory and writes nothing;
- a 15 s tick, which runs a full pass every `LIVE_PASS_EVERY` (5 min) while
  the stream is up and every `POLL_PASS_EVERY` (1 min) while it is down, and
  otherwise only checks the version (below);
- a wake from sleep and a network change, which end the session: it
  reconnects, runs a pass and starts a new stream;
- sign-in and sign-out from the app.

The stream is only ever a change notifier. Everything it reports is read back
over REST in a pass, so a missed or doubled event costs a pass at most, and
the tick catches what the stream never said.

**The sync lock.** A pass takes the `sync_locks` row
(`acquire_sync_lock("chat", <server>)`), the same table and the same expiry
and renewal rules as a folder sync, so one Orb at a time writes the chat
tables from the server. A pass that finds the row held is skipped, and the
loop asks again 5 s later; the holder's writes reach this window through the
version. An action that needs the result at once (sending, editing, joining a
room) waits for the row instead, up to 20 s (`pass_now`). The row is released
whether the pass succeeds or fails. The user's own changes — a mark, what was
seen — are single statements on their own tables and take no lock.

**The version.** `chat_state` holds a counter that every change to the chat
tables bumps (`store::bump_version`), whichever Orb makes it. Each process
remembers the version it last told its windows about and compares on every
tick and after every pass; a different number raises
`ChatListener::on_changed`, and the app emits `chat-changed` to every window.
So a second Orb's pass, or a mark made in it, reaches this one's windows
within 15 s, and its own windows at once.

**Notifications.** The app keeps a watermark per process: the newest moment a
thread started waiting that it has announced, seeded at launch with what
waits already. On every `on_changed` it asks for the threads that started
waiting after it (`new_waiting_since`) and moves it with a compare-and-max,
so two changes raised together announce once.

**Marks between Macs.** Done and task marks travel in one message of the
tasks folder, written by the task sync under the registry's rules, not by
the chat loop (chat-spec §6). A downloaded copy is merged into `chat_marks`
in one transaction and bumps the version, so the windows follow on the next
tick.

**Starting afresh.** A session for another server or another user than the
database's wipes the chat tables first (`ChatService::adopt`), in one
statement per table, before anything of the new session is written.

### External change signalling

- **From the server:** `services::idle::run_idle_session` holds one IDLE
  connection per watched folder (at most `MAX_IDLE_PER_ACCOUNT` = 3 per
  account; INBOX and the folders the config marks `watch`). It re-issues IDLE
  every 10 min — the DONE that ends an IDLE is bounded by the probe timeout,
  so this is also how a frozen connection is found out — and reconnects with
  exponential backoff (5 s to 300 s). It sends `StateUpdate::ExternalChange`
  on `NewData` and on every reconnect (the first connect at launch is covered
  by the launch sync; a connect after a wake counts as a reconnect), so mail
  delivered while it was reconnecting is not left for the next delivery. The scheduler turns each
  into a sync. A server without IDLE ends the worker; the periodic sync
  covers that account.
- **From another process writing the same database:** nothing notifies the
  app of mail. The `sync_meta('sync_version')` counter records that something
  changed, but nothing reads it. Chat is the exception: its own version is
  read every 15 s (see [Chat](#chat)).

## Error handling and retry

- Errors are typed. `From<async_imap::error::Error>` keeps transport failures
  as `Error::Io`, maps an unparseable response to `Error::ImapProtocolError`
  and the server's NO/BAD to `Error::Imap`; a refused login is
  `Error::AuthenticationFailed`. `Error::is_connection_error` (timeouts, I/O,
  TLS, protocol errors) decides whether a connection goes back into the pool;
  `Error::is_retryable` (the same plus `ConcurrencyConflict` and
  `SQLITE_BUSY`/`SQLITE_LOCKED`) whether an operation is tried again. Nothing
  reads an error's text.
- Every IMAP command carries `IMAP_OPERATION_TIMEOUT_SECONDS` (60 s), applied
  per command and per response within a FETCH; a timeout is a connection
  error. Liveness probes (the pool's NOOP, the IDLE's DONE) carry
  `IMAP_PROBE_TIMEOUT_SECONDS` (10 s). The sockets have TCP keepalive. A
  FETCH is read to its tagged response (`ImapClient::run_until_done`), so a
  connection cut mid-fetch is an error rather than a shorter folder, and a
  NO or BAD to a FETCH is an error rather than an empty result.
- `Store::sync_folder_with_retry` retries a folder sync up to
  `MAX_SYNC_RETRY_ATTEMPTS` (5) times when `Error::is_retryable` holds.
  Backoff starts at 1 s, doubles with up to 25 % jitter, and is capped at
  30 s. Before a retry of a connection error it reconnects; for other
  retryable errors it keeps the connection. It sends
  `StateUpdate::RetryAttempt`, which reaches the app as a `retrying` progress
  stage. After the last attempt it returns `Error::RetryExhausted`. A
  `ConcurrencyConflict` that outlasted its three attempts inside
  `sync_folder_impl` is not retried again here.
- In `Store::sync`, a failed folder adds a line to `SyncReport::errors` and
  the remaining folders continue. If the error leaves the connection unusable
  (`is_broken_connection_error`, which includes `RetryExhausted`), the client
  is replaced before the next folder, so a dead connection never goes back
  into the pool. Every other operation reaches the server through
  `Store::with_connection`, which returns the pooled connection only when the
  operation did not fail with a connection error.
- **The connection pool** keeps up to two idle connections per account
  (`POOL_IDLE_PER_ACCOUNT`) and never makes a caller wait: when both are in
  use, a third is opened, and the one handed back beyond the two is closed.
  Two because the body backfill and the syncs that push and the timer start
  overlap all the time. With one kept, every overlap cost a login. Measured
  with 500-body backfill rounds while the folder was synced every 250 ms
  (`tests/sync_scale.rs`, `tests/live_second_connection.rs`):

  | Server | Kept | Backfill | Syncs beside it, mean / worst | Logins |
  |---|---|---|---|---|
  | Dovecot in Docker, 20 000 messages | 1 | 4.9 s for 5 000 | 123 / 314 ms | 10 |
  | | 2 | 4.8 s | 106 / 362 ms | 1 |
  | A shared hoster, 2 000 messages | 1 | 5.7 s for 2 000 | 946 ms / 1.33 s | 2 |
  | | 2 | 5.7 s | 745 / 991 ms | 1 |

  The backfill is no faster — it is one connection's work either way — but
  the syncs beside it are spared a login, a fifth of their time over a real
  network, and the server sees one login instead of one every half second.
- A sync whose messages were stored is kept even if the flag check that
  follows (`Store::reconcile_flags`) fails. That failure is only logged.
- At the FFI boundary, core errors become `OrbError` (`impl From<OrbCoreError>
  for OrbError` in `src/ffi.rs`). `offload` logs the start and end of every
  command, so a hung command can be told apart from a failed one.
- Every folder sync records a trace (`src/imap/trace.rs`): one phase per
  line at debug level — connect, list, status, select, fetch, vanished or
  uid-listing, repair, write, flags — with elapsed time, bytes on the wire
  (counted under TLS by `imap::Counted`), message count and the connection
  id. Retries, a lock that was not obtained and an unchanged folder are
  phases too. Every TLS connect and LOGIN goes through `imap::open_session`,
  which logs it with the caller's reason and counts it in a process-wide
  ledger. The `sync-stats` command shows the last trace per folder and the
  ledger.

## Passwords

`credentials::PasswordCache`, shared by the store and the IDLE service, runs
each account's password command once per session on a blocking thread with a
`PASSWORD_COMMAND_TIMEOUT_SECONDS` (120 s) bound, and keeps the result. A
refused login forgets it, so the next attempt runs the command again.

## The outbox

Flag changes (`Store::set_read_status`, `Store::set_flag_status`) are written
to the local copy first and queued in the `outbox` table, then carried out at
once if the server answers. A message the SMTP server cannot be reached for is
queued the same way (`Store::send_mail` returns `SendOutcome::Queued`); a
message the server refuses is an error, never queued. `Store::drain_outbox`
runs the account's queue in order on a pooled connection and at the start of
every sync of the account, stopping at the first item the server cannot be
reached for; an item the server refuses is dropped with a warning, except a
send, which is kept as a draft. The queue is per account and ordered, so a
flag set and then cleared reaches the server in that order.

Applied staged actions go the same way, and so do the immediate moves and
deletes (`Store::move_messages`, `Store::delete_messages`). Each calls
`Store::queue_move`, which deletes the messages from the stored copy, bumps the
folder's `sync_version` and queues a `move` item (a target folder, or none for
a permanent delete, plus the UIDVALIDITY the UIDs were issued under) in one
transaction; `OrbHandle::execute_pending_actions`, `OrbHandle::move_messages`
and `OrbHandle::delete_messages` drain on a spawned task and then sync the
destinations. Undoing a move (`OrbHandle::undo`) drains first, waiting for a
drain that is running: until the queued move is carried out, its messages are
not in the destination to be found by Message-ID. The drain checks the queued UIDVALIDITY rather
than the stored one, and a move the server refuses fetches the messages back
into the stored copy, since no later sync would: they have not changed.

The undo history (`crate::undo::History`) lives in the `OrbHandle`, behind a
mutex, so every window of one process shares it and a second Orb process has
its own. An undo takes its entry off the stack under the lock and carries it
out without it, so a write recorded meanwhile is not held up; if the first step
fails, the entry goes back.

**Invariant:** until its move is carried out, a queued UID is still on the
server in its old folder, and a sync must not store it there again.
Transaction 2 of `sync_folder_impl` leaves out every UID
`Store::queued_move_uids` names; the `sync_version` bump makes a sync that was
already fetching start over and reach that filter.

The app does not wait for the server: `OrbHandle::mark_read` and
`OrbHandle::mark_flagged` call `Store::queue_read_status` and
`Store::queue_flag_status`, which change the stored copy and queue the write,
and then drain the outbox on a spawned task. A drain holds the account's
`outbox_drains` lock for as long as it runs, so two drains never read the same
items — a queued send would otherwise go out twice. A sync's drain waits for a
running one; that cannot deadlock, because a drain that needs a connection
while the sync holds the account's is given a new one by the pool.

## Known gaps

- **Changes from another process go unseen.** Nothing polls the
  `sync_meta('sync_version')` counter, so a second instance's mail writes
  appear only after the next query the UI happens to make. (Chat polls its
  own version.)
- **Two Orbs on one machine both announce chat.** Each process keeps its own
  notification watermark, so a thread that starts waiting is announced once
  by each.
- **Chat's marks wait for the task sync.** Done and task marks reach the
  other Macs with the next task upload (chat-spec §6): within seconds when
  the tasks account is reachable, and not at all without a tasks account.
