# Chat specification

Orb reads one Rocket.Chat server into its database and answers from there
(PLAN.md, Phase 15). This document says what the chat part of Orb promises:
the data it keeps, the rule that decides what waits, what reaches the server
and what never does. Code comments cite its sections (`chat-spec §5`), so
sections are never renumbered; new ones are appended. The invariants are in
§12.

## 1. Scope

- One server per configuration (`chat.server`), Rocket.Chat 8 and later.
- The unit is the **thread**. Waiting lists threads, not messages and not
  rooms, so a busy room with one open question is one row.
- Orb is a client like the web client: everything it writes on the server is
  something the user did — a message, an edit, a reaction, a room joined or
  left, a read mark.

## 2. Session

- `sign_in: sso` (the default): the server's own sign-in page opens in a
  window of Orb's; once it has signed in, Orb takes the session the web
  client was handed (`Meteor.loginToken`, `Meteor.userId`) and closes the
  window. Single sign-on (OAuth) works this way although the REST API does
  not take it directly.
- `sign_in: token_command`: the command prints a user id and a personal
  access token, separated by whitespace.
- The session is kept in the Keychain under `chat:<host>` as `<user id>
  <token>`. A 401 from the server signs out: Waiting says so and offers the
  sign-in again; the database stays.
- A session for another server or another user than the database's starts
  afresh: the chat tables are emptied before anything of the new session is
  written (§12).

## 3. Data

All in the main database (migrations 33 and 34):

- `chat_rooms` — the rooms the user is subscribed to, with kind (`c`, `p`,
  `d`), name, display name, topic, member count, the newest message's time,
  and how far the room is synced.
- `chat_messages` — messages by their server id, with room, thread (`tmid`),
  author, text, time, edit time, system type, reply count, whether they name
  the user, files, reactions and quotes. The server's copy wins: a message
  is replaced whole when it changes there.
- `chat_marks` — per thread, the user's own decision: `done` or `task`, and
  the thread's newest message when it was taken (§6).
- `chat_seen` — per thread, up to when the user has looked at it in Orb.
- `chat_state` — key/value: the server and user the tables belong to, the
  username, the Waiting baseline (§5), the version (§9), per room the
  server's notification preference (`notify:<room>`), per room whether its
  history is complete, and the marks' sync state (`marks:*`, §6).

Presence (online, away, busy, offline) is kept in memory only.

## 4. Threads

A message belongs to exactly one thread:

- a reply (`tmid` set) to its thread, named by the thread's first message;
- a message with replies of its own to the thread it starts;
- anything else to the room's own **timeline**, named by the room and the
  empty thread id.

A thread's first message also shows in the timeline, where it was written,
with its reply count. The timeline of a direct message is the whole
conversation, unless replies were made in threads.

Whether the server has threads at all is its public setting
`Threads_enabled`, asked for on connecting and kept in `chat_state`
(`threads`). Turned off, Orb offers no way to start a thread; threads made
before stay readable.

## 5. The Waiting rule

A thread **waits** when all of these hold:

1. its newest message that is not a system message is somebody else's;
2. that message is newer than the thread's mark, if it has one (§6);
3. it is no older than the **baseline**: the moment of the first sync, less
   `waiting_days` (default 14). A server's whole past never starts waiting;
4. its room may wait: `rooms.<name>.waiting` is not false.

Nothing else decides it. In particular, reading the thread in Orb or
elsewhere does not, and neither does the server's unread count, which by
default counts only mentions in a channel.

A thread waits **since** the first of the messages after the user's last
one, or after its mark if that is later: a mark answers the thread as a
message of the user's would. It is marked as **naming the user** when one of those mentions them,
and as **unread** when the user has not looked at it in Orb since its newest
message (`chat_seen`).

A thread leaves Waiting when the user writes in it (in Orb or anywhere),
marks it done, or makes a task of one of its messages; it comes back when
somebody writes again.

A thread of a muted room still waits by this rule; §13 says where it shows.

## 6. Marks across Macs

Done and task marks are the user's and travel to every Mac through the
tasks account (todo-spec §1), in one message of the tasks folder:

- `X-Orb-Task-Type: chat-marks`, `X-Orb-Task-Id:
  00000000-0000-4000-8000-000000000002`, a revision and a fresh `Message-ID`
  each time, as the tag registry (todo-spec §7). A build that predates it
  cannot read the message and leaves it where it is.
- The body is a JSON array of marks: `server` (the host), `room`, `thread`,
  `kind` and `at` (ms).
- It syncs by the registry's rules: a higher revision is downloaded, a
  local change is uploaded over every copy, and two copies at one revision
  are told apart by `Message-ID`.
- When both this Mac and another changed marks since the copy they share
  (the **base**, kept with the state), the two are merged thread by
  thread against it: a mark either side set is kept; a mark either side
  took back, and the other left as it was, is gone; a thread both marked
  differently keeps the mark with the later `at`. The result goes up at a
  revision above both.
- Marks of another server pass through untouched.
- A mark older than 120 days does not travel, so the message does not grow
  for good; it stays on the Mac that has it.

Setting a mark, and taking one back by undo, asks for an upload at once, as
a task edit does (todo-spec §23).

## 7. Read state on the server

Orb reads through the REST API, which leaves the server's read state alone.

- Reading a thread in Orb marks nothing read on the server.
- Answering a thread, marking it done and making a task of it mark it read
  there: the room for the timeline, the thread for a thread.
- `u` marks a thread read there, or unread again from its newest message.

## 8. Sync

- A pass syncs the subscriptions and rooms that changed, then every room
  whose newest message is newer than what the database holds, from where
  it stopped (less 10 s of clock skew). The first sync of a room reaches
  back `max(30, waiting_days)` days, so a conversation has its context.
- A conversation opens with its 200 newest messages; scrolling to the top
  brings the next 100, from the database and then from the server, until
  the server has no older ones.
- The realtime stream only names what changed; every change is read back
  over REST, so a lost event costs nothing but time. A reaction or an edit
  leaves a room's newest message where it was, so what the stream missed
  of it is found when the conversation is opened: that asks the server what
  changed in its room, at most once a minute. A full pass runs every
  5 minutes while the stream is up and every minute while it is down.
- Concurrency (the sync lock, the version) is in
  `docs/CONCURRENCY_DESIGN.md`, "Chat".

## 9. Change signalling

Every write to the chat tables bumps `version` in `chat_state`. Each Orb
compares it with the one it last told its windows about, every 15 s and
after every pass; a change redraws Waiting in every window, whichever Orb
made it.

## 10. Notifications

A thread that starts waiting (§5) after the last one announced is announced
once: who wrote, the room and the text, or one count for several at once.
What waits when Orb starts is not announced. A room stays quiet when
`rooms.<name>.notify` is false, when the server's preference for it is
`nothing` or notifications are off, or when it is `mentions` and the thread
neither names the user nor is a direct message. A muted room (§13) stays
quiet too.

## 11. Configuration

```yaml
chat:
  server: https://chat.example.com   # required
  sign_in: sso                       # or token_command
  token_command: …                   # with token_command
  waiting_days: 14                   # §5
  rooms:                             # by the room's name in its address
    general: { waiting: false }      # never waits
    support: { notify: false }       # waits without a notification
  reactions: [":+1:", ":heart:", ":joy:", ":tada:", ":eyes:"]
  badge: false                       # the Dock badge counts Waiting too
```

`server`, `sign_in` and `token_command` need a restart; the rest takes effect
when the file is saved.

## 12. Invariants

- **Reading never marks read.** Only an answer, done, a task, `u` and the
  web client itself change the server's read state (§7). A view, a preview,
  a notification or a sync never does.
- **The server's copy of a message wins.** A message is stored as the
  server last gave it, whole; Orb keeps no edits of its own to a message.
- **The Waiting rule reads only §5.** Anything that should take a thread out
  of Waiting does it through a message of the user's or a mark.
- **A mark is the user's, and every Mac has it.** No sync, server change or
  merge drops a mark the user set and did not take back (§6).
- **One server per database.** A session for another server or user empties
  the chat tables first, so two servers' rooms never mix.
- **Tests that write to a real server write only to the user's own direct
  message with themself**, and mark nothing read; everything else is tested
  against a local Rocket.Chat (`tests/chat_integration.rs`).

## 13. Mutes

A room can be muted in Orb, until a moment or until the user unmutes it. The
server's own notification settings are left alone, so the web client and the
phone go on as before.

- A thread of a muted room **waits muted** when it waits (§5) and none of
  the messages it waits for names the user or calls the room
  (`@here`, `@all`). A thread that waits muted is left out of Waiting, Today,
  the notifications and the Dock badge. Its room's row in the sidebar still
  counts it, in grey, and the room's own list shows it, dimmed.
- A thread that names the user or calls the room waits as if the room were
  not muted. It is announced (§10) from the first such message, not from
  when it started waiting.
- What waited muted is not announced when the mute ends: the notifications'
  watermark passes it while the mute holds.
- A mute is a mark (§6) of its room on the thread `*`, which no message id
  can be, with the kind `mute` and, as `at`, when it ends: `i64::MAX` for a
  mute the user ends. It travels and merges as every mark does, so two Macs
  that muted one room differently keep the mute that ends later, and a mute
  either side ended is gone. A mute that has ended does not travel, however
  recently it was set; one that has not travels however long ago it was.
- Muting and unmuting are undone as a mark is.

## 14. Focus

A focus (README, "Focus") decides which of the threads that start waiting
(§10) are announced as they do: all of them when its mode says `chat: on`,
those that name the user for `mentions`, none for `off`. The threads of a
room with `rooms.<name>.breaks_focus` are announced in every mode.

- A thread held back is announced when the focus ends, or when the mode's
  interval comes round, if it still waits and its room is not muted (§13).
- Waiting, Today and the sidebar's counts are as without a focus.
- With `badge: exceptions` the Dock counts, of Waiting, the threads the mode
  would announce, when `chat.badge` counts chat at all.
- A focus is local: nothing of it reaches the server or another Mac.
