# Orb: the road to a daily driver

This is the working plan for turning Orb into a complete mail client and task
tracker. It replaces `todo.md`, the stabilization plan and the design documents
that described the deleted TUI. Every item below was checked against the code on
2026-10-14; file references are a starting point, not a specification.

Phases are ordered by what hurts most. Within a phase, items are ordered by
impact. An item is done when it works in the app, has a test where one is
possible, and the README describes it.

The task-management specification stays in `todo-spec.md`; the code cites its
section numbers, so its numbering must not change.

## Progress

- [x] Phase 0: Housekeeping, and a sync that can be trusted
  - [x] 0.1 Housekeeping
  - [x] 0.2 Configuration format: YAML instead of TOML
  - [x] 0.3 Mail sync: assess
  - [x] 0.4 Mail sync: fix
- [x] Phase 1: Stop losing mail and leaking privacy
  - [x] 1.1 Deleting, HTML and the sending account
  - [x] 1.2 Write safety in the core
  - [x] 1.3 Reply headers
- [x] Phase 2: Daily-driver mail
  - [x] 2.1 Reading and acting on mail
  - [x] 2.2 Launch and configuration
- [x] Phase 3: Identities, signatures, filter rules and highlights
  - [x] 3.1 Identities and signatures
  - [x] 3.2 Filter rules
  - [x] 3.3 Highlight rules
- [ ] Phase 4: The rest of a mail client
  - [ ] 4.1 Threading
  - [x] 4.2 Folders
  - [x] 4.3 Authentication and transport
  - [x] 4.4 Cross-account move and copy
  - [x] 4.5 List formatting and saved searches
  - [x] 4.6 Logging, diagnostics and schema safety
- [ ] Phase 5: Task tracker completion
  - [x] 5.1 Real-account validation
  - [ ] 5.2 Editing what the backend already supports
  - [x] 5.3 Pointer coverage
- [ ] Phase 6: UI, UX and frontend health
  - [x] 6.1 Appearance, focus and accessibility
  - [ ] 6.2 Menu bar and drafts
  - [ ] 6.3 Frontend structure and races
  - [x] 6.4 Dialogs and panics
  - [x] 6.5 Neovim drawn by Orb, not by a terminal
  - [x] 6.6 Range selection
- [ ] Phase 7: Sync follow-ups
  - [ ] 7.1 Real-account baselines
  - [ ] 7.2 Network changes and a second connection
  - [x] 7.3 Tasks that are visibly saved
- [x] Phase 8: Design foundation
  - [x] 8.1 Tokens, type and spacing
  - [x] 8.2 Markup both looks can share
  - [x] 8.3 Window chrome
- [x] Phase 9: The Hybrid look
  - [x] 9.1 Sidebar
  - [x] 9.2 Message list
  - [x] 9.3 Today on one grid
  - [x] 9.4 Toolbar and status line
  - [x] 9.5 Reader and task detail
  - [x] 9.6 Empty states and the compose window
- [x] Phase 10: The Terminal look
  - [x] 10.1 The setting
  - [x] 10.2 The override layer
- [x] Phase 11: Features from the mockups
  - [x] 11.1 Spoofed senders
  - [x] 11.2 Reply status of linked mail
  - [x] 11.3 Command line completion and sort direction
  - [x] 11.4 A window for tasks
  - [x] 11.5 Steps
  - [x] 11.6 Archiving done tasks
  - [x] 11.7 Working on
- [x] Phase 12: Task export and import
  - [x] 12.1 The format, in the spec
  - [x] 12.2 Writing a snapshot
  - [x] 12.3 Schedule, configuration and visibility
  - [x] 12.4 Import
  - [x] 12.5 Import from Things 3
- [x] Phase 13: Search and filter
  - [x] 13.1 Searching every task
  - [x] 13.2 One result list for mail and tasks
  - [x] 13.3 The filter
  - [x] 13.4 Saying what to search, and where
- [x] Phase 14: Reading
  - [x] 14.1 The reading folders
  - [x] 14.2 Reading in the window
  - [x] 14.3 Feeds
  - [x] 14.4 Reading in Today
- [ ] Phase 15: Rocket.Chat
  - [ ] 15.1 Spike and specification
  - [x] 15.2 Signing in
  - [x] 15.3 The chat store and its sync
  - [x] 15.4 Live updates
  - [x] 15.5 Waiting
  - [x] 15.6 Rooms
  - [x] 15.7 Writing
  - [x] 15.8 Tasks from chat
  - [x] 15.9 The chat window
  - [x] 15.10 Notifications
  - [x] 15.11 Muted rooms
- [x] Phase 16: Today as one timeline
  - [x] 16.1 The layout
  - [x] 16.2 The filter across kinds
  - [x] 16.3 Selecting across kinds

**How this list is kept.** Work through the subphases in order. When an item
in a subphase is finished, delete it from the subphase; when a subphase has no
items left, tick its box; when every subphase of a phase is ticked, tick the
phase. Update this file in the same commit as the work that finished the item,
every time, so the plan on `main` always says what is left. The "Later" list
is not a phase.

---

## Phase 0: Housekeeping, and a sync that can be trusted

Two halves. The housekeeping is cheap and independent, and it removes the
noise that makes the rest harder to see. The sync work is neither cheap nor
small, but everything after it stands on it: a client whose sync hangs, skips
folders silently or burns a TLS handshake per keystroke cannot be made a daily
driver by adding features on top.

### 0.1 Housekeeping

### 0.2 Configuration format: YAML instead of TOML

### 0.3 Mail sync: assess

### 0.4 Mail sync: fix

The instruments from 0.3: the per-folder trace and `sync-stats`, the
connection ledger, `tests/sync_faults.rs` (now in the CI docker job),
`tests/sync_scale.rs` and `tests/live_sync_timing.rs`. Every fix here is
measured against them.

Baseline, 2026-10-14, Dovecot container, one folder of 100 000 messages:

| Sync | Total | Where the time goes |
|------|-------|---------------------|
| Cold, before the fixes | 16.5 s | FETCH 6.7 s, 30 MiB on the wire; one write transaction 9.6 s; RSS 29 → 130 MiB |
| Cold, after the fixes | 17.8 s | UID listing 0.6 s, then 20 pages of 5 000 fetched and each committed before the next; RSS 29 → 43 MiB |
| Cold, with threading, 2026-10-15, release build | 9.2 s | Before its lookups named their indexes, threading made the cold sync quadratic: 30 000 messages took 90 s, now 2.6 s |

Two real accounts, 2026-10-15, release build (`tests/live_sync_timing.rs`):
a groupware server and a shared hoster, 17 folders, the largest an archive
of 205 000 messages over 885 000 UIDs.

| Sync | Total | Notes |
|------|-------|-------|
| Cold | 22 min 31 s | 313 906 messages; 2 connections opened, none failed, no timeout. Before this run's two fixes it never finished: the archive's first page was a 19 KiB UID set the server never answered, and threading made every page slower than the last |
| Warm | 6.1 s | 3 added, 1 updated, 2 deleted since the cold sync |
| Warm, unchanged | 69 ms | one NOOP and one STATUS (the NOOP because the folder is the one this connection has selected) |
| Incremental, +1 −1, fresh connection | 188 ms | stored UIDs 25 ms, SELECT 69 ms, CHANGEDSINCE 3 ms, VANISHED 5 ms, write 1 ms |
| Full UID listing (no-QRESYNC fallback) | 566 ms | 2.6 MiB; now run only when the message count moved |

Under injected faults (`tests/sync_faults.rs`, same container): a connection
dropped mid-FETCH is retried on a new one and the folder completes in 1.5 s; a
frozen FETCH ends at the 60 s command timeout and the retry completes it; a
pooled connection dead after sleep costs one 60 s timeout, then a reconnect; a
frozen IDLE connection is replaced within 15 s of being noticed; a flag set
while offline is kept locally and reaches the server with the next sync.

## Phase 1: Stop losing mail and leaking privacy

Each of these is wrong today and is the kind of thing a daily driver cannot have.

### 1.1 Deleting, HTML and the sending account

### 1.2 Write safety in the core

### 1.3 Reply headers

## Phase 2: Daily-driver mail

What is needed before the mail side can be used all day without reaching for
another client. The README's keyboard tables were written for this phase; the
README now documents what exists, and the gaps are here.

### 2.1 Reading and acting on mail

### 2.2 Launch and configuration

## Phase 3: Identities, signatures, filter rules and highlights

All three configured in `config.yml`, deliberately without a GUI. Keys are
provisional until implemented. Filter rules and highlights share one matching
engine; build it once, for both.

### 3.1 Identities and signatures

### 3.2 Filter rules

### 3.3 Highlight rules

## Phase 4: The rest of a mail client

Things a complete client has that Orb lacks, roughly in the order a user
would miss them.

### 4.1 Threading

- **Quoted text in a conversation.** The reader of a conversation shows each
  message whole, so a reply that quotes the one above it repeats it down the
  stack. Fold the quote of an earlier member — the `>` lines and their
  attribution in text, `blockquote[type=cite]`, `.gmail_quote` and Outlook's
  divider in HTML — behind a line that unfolds it.

### 4.2 Folders

### 4.3 Authentication and transport

### 4.4 Cross-account move and copy

### 4.5 List formatting and saved searches

### 4.6 Logging, diagnostics and schema safety

## Phase 5: Task tracker completion

The backend and the keyboard surface are complete per `todo-spec.md`; what
remains is reachability and validation.

### 5.1 Real-account validation

### 5.2 Editing what the backend already supports

- The task search cannot name a tag of several words: `tag:"next actions"`
  finds nothing, nor does `tag:"errands"`, because the query goes through
  the mail parser, which has no `tag:` field and splits the quoted value off.
  The views' filter reads the same term correctly (`split_search` in
  `src/tasks/repository.rs`).

### 5.3 Pointer coverage

## Phase 6: UI, UX and frontend health

### 6.1 Appearance, focus and accessibility

### 6.2 Menu bar and drafts

- Maximize (`zm`) is keyboard-only: the View menu has no item for it, and a
  drawer the pointer brought out (a double-click on a row) cannot be put away
  with the pointer — a click on the list beside it, or a close button, would.

### 6.3 Frontend structure and races

- Playwright tests fail now and then with nothing under `app/` changed:
  `chat.spec.ts:322` (Chat in Today as a pane of its own) about one run in
  four on 2026-10-29 and, in the evening, nearly every full run, while it
  passes run alone; `compose.spec.ts:156` (Ctrl-J and Ctrl-K between the
  header's lines and the body) and `compose.spec.ts:105` (Save & close)
  once each; `compose.spec.ts:131` (a message taken from the Outbox) about
  one run in ten on 2026-11-15, run alone too, where `discard_outbox 7` is
  not among the calls when the window closes — and on 2026-11-17 two or
  three of six with `--repeat-each 6`, failing in `page.evaluate` because
  `__mock` is not yet there after a `goto`. Load-dependent races, by the
  look of it.

### 6.4 Dialogs and panics

### 6.5 Neovim drawn by Orb, not by a terminal

### 6.6 Range selection

## Phase 7: Sync follow-ups

What Phase 0 left open: two measurements that need the real accounts, and two
refinements of a sync that already holds up under the fault suite.

### 7.1 Real-account baselines

- The connection tally over an hour of the app on the real accounts
  (`sync-stats`), which only running the app can give.
- A week of real-account logs with no repeating retry, which is the last of
  the 0.4 acceptance criteria and can only be observed.
- **Say that a migration is running.** The database is opened before any
  window exists (`launch` in `app/src-tauri/src/main.rs`), so a long
  migration is a Dock icon that bounces with nothing on screen. Migration 30
  took 42 s on a 200 000-message, 1.9 GB database (`tests/body_scale.rs`),
  which is about a minute and a half on a 4.6 GB one. Open the window first
  and show what the migration is doing.

### 7.2 Network changes and a second connection

- **Validate the sync without CONDSTORE.** The path that lists UIDs instead
  of asking CHANGEDSINCE is exercised against Dovecot with CONDSTORE turned
  off in the client, not against a server that lacks it. Run it against an
  Exchange or an Office 365 mailbox before relying on it.
- **Fetch a header a new rule names for the mail already synced.** The
  header backfill (`fetch_pending_thread_headers`) only visits messages never
  header-synced, so mail synced before a highlight or filter named a header
  matches only where its source is stored. Remember which header names each
  message was fetched with, and fetch the newly named ones in the background
  when the set grows.

- **Keep the reader's background fetches from piling up on a thin link.**
  Each message the cursor rests on asks `message_complete` for its HTML, and
  the pool opens a connection for every one it has no idle one for. Drop the
  ones for messages the cursor has since left, and let one interactive fetch
  per account run at a time, the newest first.

### 7.3 Tasks that are visibly saved

## Phase 8: Design foundation

The redesign drawn in `docs/mockups/main-window.html`: two looks, Hybrid and
Terminal, on one markup. The mockup is the visual reference for Phases 8–11;
its notes say what each state is meant to show. This phase changes nothing a
user would call a new design — it makes the next two possible without a second
rewrite.

### 8.1 Tokens, type and spacing

### 8.2 Markup both looks can share

### 8.3 Window chrome

## Phase 9: The Hybrid look

Screen by screen, each subphase shippable on its own: SF Pro for text, SF Mono
for what is scanned in columns, single-line rows. Every subphase updates the
style snapshots and the README where a behaviour it describes changes.

### 9.1 Sidebar

### 9.2 Message list

### 9.3 Today on one grid

### 9.4 Toolbar and status line

### 9.5 Reader and task detail

### 9.6 Empty states and the compose window

## Phase 10: The Terminal look

### 10.1 The setting

### 10.2 The override layer

## Phase 11: Features from the mockups

New behaviour the mockups show, and task features found missing in daily
use. Each is a change of its own, after the looks.

### 11.1 Spoofed senders

### 11.2 Reply status of linked mail

### 11.3 Command line completion and sort direction

### 11.4 A window for tasks

### 11.5 Steps

### 11.6 Archiving done tasks

### 11.7 Working on

## Phase 12: Task export and import

The tasks folder is the source of truth and the database a cache of it (§3),
so every copy of a task Orb keeps lives on one IMAP server. A server that loses
the folder, a sync bug that passes the invariants of §16, or an emptied trash
(§10.1, purged for good after 30 days) takes the history with it. This phase
writes every task, past, present and future, to a directory on a schedule, in
a documented JSON format that Orb can read back and that any other tool can
read without Orb's help.

An export nobody has restored from is a hope, not a backup. That is why the
phase ends with import, and why the round trip is a test.

### 12.1 The format, in the spec

The invariants of §20 join §16 with their tests, in 12.2 and 12.4.

### 12.2 Writing a snapshot

### 12.3 Schedule, configuration and visibility

### 12.4 Import

### 12.5 Import from Things 3

A later import is recognised by ids derived from the Things ids rather
than by a new `X-Orb-Imported-From` header, which needed no change to the
task message; what it last brought over, and whether Orb has edited the task
since, is a mark in the task's log (todo-spec §22).

## Phase 13: Search and filter

Ideas item 3. Search and filter are two gestures today only in name: `/` in a
task view filters that view, and `/` anywhere else searches mail, so Today
cannot find mail and no view can find a task outside itself. The field
search language (`f:`, `s:`, `in:` …) exists but nothing in the window says
so. `docs/mockups/main-window.html` draws the result, in the states Search
field, Search, Search tasks, Search scope and Filter: `/` searches
everything from everywhere, `F` narrows the list on screen.

### 13.1 Searching every task

### 13.2 One result list for mail and tasks

### 13.3 The filter

### 13.4 Saying what to search, and where

## Phase 14: Reading

A reading list beside the mail and the tasks: newsletters set aside for later,
and RSS and Atom feeds, in one list. Every item is a message in a reading
folder on one IMAP account, so the list syncs between Macs as the tasks do,
and the reader, search, highlights and the dark rendering of newsletters
work on it unchanged. An item has three states, all plain IMAP: *new*
(unread), *must read* (`\Flagged`) and *archived* (moved to the reading
archive folder). Keywords are avoided on purpose: Orb does not read
`PERMANENTFLAGS`, and a server that refuses a keyword drops the change.

### 14.1 The reading folders

### 14.2 Reading in the window

### 14.3 Feeds

### 14.4 Reading in Today

## Phase 15: Rocket.Chat

Today a chat message that needs an answer lives in a second app, and opening
it there marks the whole room read, so it cannot be left as a reminder.
This phase makes Rocket.Chat a third source beside mail and tasks, and
eventually the only chat client needed. The centre is **Waiting**: one row
per thread that waits for an answer, with the newest message as its preview.
A thread leaves Waiting when you reply, or when you dismiss it or make a task
of it, and comes back when someone writes again. `docs/mockups/main-window.html`
draws it, in the states Chat, Chat reply, Chat room, Chat window and Chat
sign-in.

The rules every subphase keeps:

- **Reading marks nothing read on the server.** Orb reads through the REST
  API, which leaves the read state alone; only `subscriptions.read` changes
  it, and Orb calls it for replying, done (`a`) and making a task (`t`),
  never for viewing.
- **A thread is the unit.** A thread (`tmid`) is one row. So is a direct
  message's or a room's timeline outside any thread.
- **Nothing existing gets busier.** Chat is a group at the foot of the
  sidebar, folded until wanted, and does not appear in Today, the Inbox or
  the task views. Joining Today is a later, opt-in question.
- **Logic in the core.** `src/chat/` owns the client, the store and the
  Waiting rule; Tauri commands stay shims over `OrbHandle`.

### 15.1 Spike and specification

The server Orb is first meant for runs Rocket.Chat 8.9 with one custom OAuth
service (redirect style) and 90-day login tokens; the client is built and tested
against a local 8.9 (`tests/chat_integration.rs`). What is left:

- Sign in to that server through the sign-in window, which needs the
  user's own SSO, and run a day of real use against it.
- Find out whether end-to-end encrypted rooms are in use. If they are, Orb
  shows them as "encrypted — open in Rocket.Chat" at first.

### 15.2 Signing in

### 15.3 The chat store and its sync

### 15.4 Live updates

### 15.5 Waiting

### 15.6 Rooms

### 15.7 Writing

### 15.8 Tasks from chat

### 15.9 The chat window

### 15.10 Notifications

### 15.11 Muted rooms

## Phase 16: Today as one timeline

Mixed, a third layout of Today beside Stacked and
Columns. The tasks keep their column on the right; on the left the mail,
the threads that wait and the reading items are one timeline, each at its
moment. `docs/mockups/main-window.html` draws it, in the states Mixed and
Mixed: chat. The mail stays the paged list; the core places the threads and
the reading among it (`inbox_ranks`), and the cursor on the timeline is the
cursor of the section whose row it is on, so the keys and the reader are
that kind's.

### 16.1 The layout

### 16.2 The filter across kinds

### 16.3 Selecting across kinds

## Later

Worth doing, not on the path to daily use.

- HTML composition and RFC 3676 space-stuffing for `format=flowed`.
- PGP and S/MIME.
- Send later and undo send, on the outbox.
- Multiple identities chosen by recipient domain.
- Spam classification with a local model, mindful of several Orbs running.
- Calendar invitation detection with "open in Calendar".
- Pinned, persistent tabs.
- A built-in OAuth 2.0 sign-in for Gmail and Office 365, instead of a token
  from a password command: it needs client ids registered for Orb with
  Google (a Cloud project) and Microsoft (an Entra app registration).
- The embedded Neovim's command line, completion menu and messages as widgets
  in the app's style (`ext_cmdline`, `ext_popupmenu`, `ext_messages`), and
  `ext_multigrid` if floating windows drawn into the one grid look wrong.
- Address book editing and vCard import.
- Today in the tasks window grouped into Overdue, Today and Done today, as
  the mockup's Tasks window state draws it.
- Per-occurrence editing of recurring tasks (out of scope in `todo-spec.md`
  §15).
- Full article text for feeds that only carry a summary, fetched from the
  publisher's page and reduced to the article, behind a setting: it is a
  request to the publisher for every item.
- Subscribing to and unsubscribing from feeds in the window, and OPML import
  and export, instead of editing the config.
- Chat in search (`/` with a Chat kind beside Mail and Tasks).
- End-to-end encrypted Rocket.Chat rooms, and replying from a notification.
- Mail templates made from Orb: Save as template from the compose window and
  New template in the picker, as the mockup's Templates states draw them.
- A draft saved with a template's placeholders still open keeps them: reopened
  from Drafts, it shows the Fill in line again and Send still checks them.
