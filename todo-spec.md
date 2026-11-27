# Task management in Orb

Orb gains task management, tightly integrated with the existing email
functionality. Tasks sync over IMAP so every machine sees the same list.

Orb is meant to be used all day, every day. The reliability bar is absolute: a task
scheduled for a date a year from now must show up on that date, on every machine,
without exception.

## 0. Starting point (historical)

This section describes the code as it was before the specification was
implemented. It is kept because the later sections refer to it; everything it
calls unwired is wired now.

A large part of the backend already exists but is **not wired into the app**:

- `src/tasks/` — `model.rs`, `repository.rs` (CRUD, float positions, linked
  emails, attachment metadata, reminders, sync state), `rfc822.rs` (the
  `X-Orb-Task-*` wire format, version 1), `sync.rs`, `reminders.rs`.
- Schema migration v8 creates `tasks`, `task_reminders`, `task_linked_emails`,
  `task_attachments`, `task_sync_state`.
- `BindingsConfig` already declares the `task_normal`, `task_detail`,
  `task_detail_terminal`, `task_edit` and `date_picker` modes, and my
  config already has a `bindings.task_normal` table.

Nothing outside `src/tasks/` calls `TaskRepository`, `TaskSyncService` or
`ReminderService` — there is no `OrbHandle` method, no Tauri command and no UI.
(`app/src/tasks.ts` is the background-job widget; unrelated name collision.)

The wire format has never shipped, so it is **not** kept compatible. Bump
`X-Orb-Task-Version` to `2` and treat version 1 messages as unreadable (log and
skip). The DB schema changes are additive migrations on top of the current
version (v17 at time of writing).

## 1. Account and folder

One **designated tasks account**, named in the config:

```yaml
tasks:
  account: Work
  folder: Tasks            # server-side name, no assumed prefix
```

The designated account is an ordinary mail account and keeps working as one.
That means the tasks folder must be excluded everywhere mail folders are
treated as mail:

- not listed in the mailbox sidebar, not selectable as a move target;
- not synced by the mail sync path, not indexed into the message FTS/embedding
  index, never counted in unread badges, never a source of new-mail
  notifications;
- excluded from "All Inboxes" and from search results.

The folder name is taken from the config verbatim. At startup, resolve it
against the server's namespace (`NAMESPACE`, or the `LIST` delimiter and
prefix) and **fail loudly** with a config warning if it cannot be created or
selected. Do not assume a leading `.` — that is a Courier/Maildir convention,
not universal.

Because there is exactly one tasks account, there is exactly one task list and
one global ordering.

## 2. Data model

A task has:

- **id** — a UUID (`X-Orb-Task-Id`), generated on creation, stable forever.
  This is the task's identity. It is *not* the RFC `Message-ID`, which changes
  every time the task is rewritten to the server.
- **revision** — a monotonically increasing integer, incremented on every local
  change. Used for conflict resolution.
- **title** — required.
- **body** — optional plain text.
- **tags** — optional set, referencing the tag registry (§7).
- **postpone date** — optional local civil date. Hides the task until then.
- **due date** — optional local civil date. When it must actually be done.
- **reminders** — zero or more local date+time values (§5).
- **attachments** — zero or more (§9).
- **linked messages** — zero or more RFC Message-IDs with cached metadata (§6).
- **recurrence** — optional rule (§8).
- **position** — `REAL`, the global manual order (§4.4).
- **completed_at** — set when completed, cleared when un-completed.
- **deleted_at** — set when trashed (§10).
- **log** — append-only event set (§11).
- **created_at / updated_at**.
- **steps** — an ordered checklist (§17).

Beside these, each machine keeps a **working on** mark per task (§19). It is
local state, like a cursor: not part of the task, not on the wire, not in
the log.

The two dates are independent: either can be set to any day, and setting
one never changes the other. A task shows from the earlier of the two, so a
postpone date after the due date is allowed and changes little — a task due
today stays in Today whatever it was postponed to, and one due later is in
Upcoming, on its due date.

## 3. Storage and synchronisation

Tasks are stored as RFC822 messages in the tasks folder, one message per task,
with `X-Orb-Task-*` headers. This is the source of truth; the SQLite tables are
a cache that must be fully rebuildable from the folder.

IMAP messages are immutable, so **every edit is APPEND-then-delete**. Getting
this right is the whole of the reliability requirement.

### 3.1 Identity and conflict resolution

- Each task message carries `X-Orb-Task-Id` (stable UUID) and
  `X-Orb-Task-Revision` (integer).
- Reconciliation groups server messages **by task id**, not by `Message-ID`.
- When a task id appears in more than one message (two devices edited, or an
  append succeeded but the delete did not), the winner is: **highest revision;
  ties broken by highest UID**. The winner is applied locally; the losers are
  expunged only after the winner has been confirmed present on the server.
- The current blanket "server wins" in `sync.rs` is **removed**. A server copy
  with a lower revision than the local task never overwrites it; instead the
  local task is re-uploaded.
- The log is exempt from last-writer-wins and is merged as a union (§11).
- Two copies at the **same revision** are two machines' edits from one
  revision, told apart by `Message-ID` (every upload has its own). A clean
  local task whose server copy has the same revision but another
  `Message-ID` downloads it: that copy was uploaded last, so it holds the
  highest UID and wins. Without this, both machines keep their own version,
  each believing itself in step, until the next edit. The same `Message-ID`
  under another UID is the same edit moved, and only its UID is recorded.
- A machine that uploads over another machine's copy — a local edit against a
  server copy it did not write — first takes in that copy's log, so the
  upload carries both histories and no machine's entries are dropped.

### 3.2 The dirty flag must not lose concurrent edits

`mark_uploaded()` currently clears `is_dirty` unconditionally, so an edit made
while an upload is in flight is marked clean and never uploaded. Clearing the
dirty flag is a **compare-and-swap** on the revision that was uploaded: only
clear if the task's revision is still the one that went to the server.

### 3.3 Do not rely on MODSEQ

`TaskSyncState.highest_modseq` must not be the only thing driving task sync.
Some servers never advance `HIGHESTMODSEQ` (already documented at
`src/imap/mod.rs:91`), so a MODSEQ-delta design silently freezes on
such an account — the precise failure this spec exists to prevent.

The tasks folder is small (hundreds of messages, tiny bodies). Every sync:

1. `SELECT` the folder, compare `UIDVALIDITY`.
2. `UID FETCH 1:* (UID FLAGS BODY.PEEK[HEADER.FIELDS (X-Orb-Task-Id
   X-Orb-Task-Revision)])` — a full listing of ids and revisions.
3. Fetch bodies only for task ids whose revision differs from the local cache,
   that are unknown locally, or whose copy at the same revision is another
   machine's edit (§3.1).
4. Any locally-known task id absent from the listing was deleted on the server;
   apply the deletion locally.

MODSEQ may be used as a *fast path* ("nothing changed") but never as the sole
record of what changed.

### 3.4 UIDVALIDITY

If `UIDVALIDITY` changed, discard all cached UIDs for the folder and do a full
reconciliation by task id. Nothing is deleted locally on this path — a
`UIDVALIDITY` bump means "your UIDs are meaningless", not "the tasks are gone".
`TaskSyncState::uidvalidity_changed()` exists but is currently never acted on.

### 3.5 Crash safety

- Every apply (task row + reminders + tags + links + log) runs in a single
  `IMMEDIATE` transaction, as `store_downloaded_task` already does.
- An append that succeeds followed by a delete that fails must be recoverable:
  the duplicate is resolved by the revision rule in §3.1 on the next sync.
- An append whose response is lost must not create a second task: the task id
  is generated *before* the append, so a retry is idempotent at the task level.

### 3.6 Offline

All task edits work offline against the local DB and are queued as dirty. Sync
uploads them in revision order on reconnect. A task created offline is fully
usable — including reminders (§5), which are scheduled locally regardless of
sync state.

## 4. Views

### 4.1 Sidebar

Above the mailboxes:

- **Today** — the landing view. Two lists in one view:
  - tasks with `postpone <= today` **or** `due <= today`, plus tasks completed
    today (§10.2), plus overdue tasks;
  - all inbox mail. This **is** the existing `All Inboxes` virtual folder, not
    a second implementation of it.

Below Today, a **Tasks** section:

- **Upcoming** — every open task with a postpone or due date: the overdue,
  today's and the future's, ordered by date under the headings of §26.
  Without this, a task postponed to September is invisible everywhere
  until September and cannot be re-planned; and with today's in it too, the
  whole plan reads in one list.
- **Someday** — tasks with no postpone and no due date. Undated tasks live
  here, *not* in Today. (The current `get_today_tasks` query includes
  `snooze_until IS NULL`; that is wrong and must change.)
- **Completed**
- **Trash**

There is no second entry named "Today" inside the Tasks section; the top-level
Today entry is the only one. In the mailbox window the top of the sidebar is
what the config's `pinned` lists, Today alone by default; a `pinned` that
leaves Today out moves it to the head of the Tasks section, still the only
Today there is. The tasks window always has it above the section.

Today shows a badge with the number of open tasks in it. Overdue tasks are
visually distinct from merely due ones. A due date that is today or past is
red wherever a due date is shown — a task's row, its detail, its chip on a
message, the widgets — and the overdue carry a mark besides, so the two stay
told apart.

### 4.2 Focus model in the Today view

The mail list comes first and the task list after it, and the view opens on
the first message (on the first task when there is no mail). The two are one
continuous `j`/`k` traversal: moving past the last message moves into the first
task and vice versa. `<tab>` jumps between the two sections directly. The task
section is sized to its content and never squeezed to a sliver by a long mail
list; the mail list takes the remaining space and scrolls.

A second layout, chosen per machine, puts the two side by side: the mail on
the left, the tasks on the right, each the full height of the pane, with
shorter rows. There each list is traversed on its own — `j`/`k` stop at the
ends of the column — and `<tab>` moves between the columns.

### 4.3 Midnight

"Today" is evaluated against the **local** civil date. Dates are stored as
`YYYY-MM-DD` and compared in the local timezone, DST-safe. (The current code
compares via `and_utc()`, so in CET tasks would appear and vanish at 01:00.)

A timer re-evaluates every date-dependent view at local midnight, and again on
wake from sleep and on timezone change. An Orb left open overnight must show
the correct Today list in the morning without a restart.

### 4.4 Ordering

One **global** manual order. Every view is a filtered projection of it; there
is no per-view ordering. A new task goes to the top of it, where a task just
added is looked for.

A selection is dragged as one block, in list order, wherever its tasks were;
each lands directly after the one before it. A selection is held by task, not
by position: it outlives a reload, a reorder or a sync, and a task that leaves
the view leaves it.

`position` is a `REAL`, so a drag writes exactly one row per task moved: moving to the top is
`first.position - 1024.0`, moving between two tasks is their midpoint
(`TaskRepository::calculate_position_between`). No renumbering of the rest of
the list.

Repeated insertion between the same two neighbours exhausts f64 precision after
roughly 50 moves. When the gap between neighbours falls below a threshold,
renormalise the whole list to multiples of 1024 in one transaction and mark the
affected tasks dirty. This is rare and must be correct rather than clever.

Concurrent reorders on two devices resolve per task by the revision rule; the
resulting interleaving may differ from either device's intent. That is
acceptable — ordering is a preference, not data.

### 4.5 Search

Two gestures, kept apart:

- **The filter** (`F`) narrows the list on screen and never leaves it: a
  live filter over title, body and tag names of the current view, plus a
  `tag:<name>` term. `<esc>` clears it; leaving the view drops it.
- **The search** (`/`) looks everywhere, from every view: mail in every
  folder, and every task in every view but Trash. The query is the mail
  search's: bare words match title, body and tag names, `s:` the title, `b:`
  the body, `tag:` a tag name, and `OR` and `-` work as for mail. An
  alternative asking for something only mail has (`f:`, `in:`,
  `is:unread`, `~` …) finds no task. §26 says how the tasks found are shown.

Task search is separate from the message FTS/embedding index: a task list is
thousands of rows at most, and `LIKE` over it is fast enough.

## 5. Reminders

A task can have any number of reminders, each a local date **and** time.

Reminders appear as persistent macOS-native notifications on every machine.
Clicking one opens Orb on that task.

### 5.1 Delivery must not depend on Orb running

Reminders are registered as **OS-level scheduled local notifications**
(`UNUserNotificationCenter`), so they fire whether or not Orb is running. An
in-app polling loop is not sufficient and is not the mechanism.

- On startup and after every task sync, **reconcile** the set of scheduled OS
  notifications against the set of future reminders: cancel notifications for
  reminders that were deleted or moved, schedule ones that are new. Reconcile
  by a stable identifier derived from task id + reminder id, so reconciliation
  is idempotent.
- macOS caps the number of pending local notifications (64). Schedule the
  nearest N reminders and re-fill the window on every sync, on every launch and
  at local midnight.
- Notification permission is requested on first use and its absence is surfaced
  as a config warning — silently unpermitted notifications are the worst
  possible failure here.

### 5.2 Firing state

`is_triggered` is **local per machine** and is deliberately not part of the
wire format: each machine fires its own notification, which is the intent.

Missed reminders (machine asleep, Orb closed for a week) fire on the next
launch only if they are less than **24 hours** overdue; older ones are marked
triggered silently and surfaced as an "N missed reminders" entry in the task's
log and a single summary notification. A year of missed reminders must never
arrive as a year of notifications at once.

### 5.3 Setting reminders

Reminders are set from the task detail view. The input uses the same fuzzy
parser as the date picker (§12), extended to accept a time
("tomorrow 9am", "next tuesday 14:00"). A reminder without an explicit time
defaults to 09:00 local.

## 6. Email integration

This is the point of building tasks into a mail client rather than using a
separate app.

### 6.1 Creating a task from a message

From the message list or the message detail view, one keystroke creates a task
linked to the current message (or to every message in the selection, one task
each). The task's title is pre-filled from the subject (after subject munging),
and the task is postponed to today, so it is in Today. The mail stays on screen:
creating the task says so in the status line and is undoable from there, so the
same keystroke can go on to file the message. Opening the new task for editing
instead is the command's `edit` form. The link stores the RFC `Message-ID` plus cached
subject, from, date, account and last-known folder — the cache is what makes
the task readable even if the message is gone forever.

A task may link more than one message; a message may have more than one task.

### 6.2 Jumping from a task back to the message

Years later, and after the message has been moved between folders, opening the
link must land on the message. Resolution order:

1. **Local index** — look up the RFC `Message-ID` in the local store. This
   requires a new index on `messages.message_id` and a lookup function; neither
   exists today.
2. **Server search** — on a miss, `UID SEARCH HEADER Message-ID "<...>"` on the
   designated account, trying the cached last-known folder first, then the
   remaining folders. On a hit, update the cached folder, sync that message and
   open it.
3. **Cached fallback** — if the message cannot be found anywhere, show the
   cached subject/from/date read-only, marked "message no longer available".
   The task is never broken by a missing message.

Steps 2 and 3 run as visible background tasks (`runTask`), because a
multi-folder server search is not instantaneous.

### 6.3 Showing tasks on messages

- **Message list**: a column showing the number of open tasks linked to the
  message. Zero renders as nothing, not as "0".
- **Message detail**: the linked tasks are listed below the metadata block,
  each with its title, dates and completion state. They are actionable in place
  — complete, postpone, open.

Both are driven by an index on `task_linked_emails.message_id` (already
present) joined against the visible message set, and must not cost a query per
row.

## 7. Tags

Tags are managed separately with full CRUD. Removing a tag removes it from
tasks but never removes the tasks.

Tags on a task are stored in an `X-Orb-Task-Tags` header as a list of **tag
ids** (UUIDs), not names, so renaming a tag does not require rewriting every
task message.

The tag registry itself — id, name, optional colour — lives in a dedicated
registry message in the tasks folder (`X-Orb-Task-Type: tag-registry`), synced
with the same identity/revision rules as tasks. A tag rename rewrites one
message. Like a task, the registry is marked dirty by a local change and
uploaded even when the server is at the same revision, and it remembers the
`Message-ID` of the copy it last wrote or read, so another machine's registry
at the same revision is downloaded (§3.1).

The registry is the one thing besides the log that is not last-writer-wins.
A machine that changed tags and finds another machine's newer registry on
the server merges the two tag by tag against the registry they last shared
(the *base*, recorded with every registry this machine writes or reads): what
either side created, renamed or recoloured is kept, a tag deleted on one side
and left alone on the other stays deleted, a tag deleted on one side and
changed on the other is kept, and a tag both sides changed differently is
taken from the machine that merges. The merge is uploaded at a revision above
both. Without a recorded base, every tag either side has is kept.

Deleting a tag increments the registry revision and marks the tasks that
referenced it dirty so their headers are rewritten. A task referencing an
unknown tag id renders the id as a placeholder rather than dropping it —
unresolvable is not the same as deleted.

Tags are filterable from any task view via `tag:<name>` in the search input.

## 8. Recurrence

A task can carry a recurrence rule (`X-Orb-Task-Recurrence`), expressed as a
subset of RFC 5545 `RRULE`: `FREQ=DAILY|WEEKLY|MONTHLY|YEARLY`, `INTERVAL`,
`BYDAY`, `BYMONTHDAY`, plus an optional `UNTIL` or `COUNT`.

Semantics:

- Completing a recurring task **does not** mark that task completed forever. It
  writes a completion entry to the log, then advances the task's dates to the
  next occurrence and clears `completed_at`. The task keeps its identity, its
  tags, its links and its log — the history of a recurring task is one
  timeline, not a graveyard of copies.
- Advancement is relative to the **scheduled** date, not the completion date,
  so a task done three days late does not drift. An option per task selects
  "advance from completion date" for habit-style tasks.
- Reminders are expressed as offsets from the task's date when the task
  recurs, so they follow it forward. §32 says where those offsets and the
  deadline's are kept, and what moving one occurrence does.
- The Completed view shows recurring tasks by their logged completions, not as
  a single row.
- Deleting a recurring task deletes the whole series. There is no
  "this occurrence only".

## 9. Attachments

A task can have attachments, and they sync.

Because every task edit rewrites the task message, attachment payloads are
**not** stored inside it. Each attachment is a separate message in the tasks
folder (`X-Orb-Task-Type: attachment`, `X-Orb-Task-Id` referencing its task,
plus its own attachment id), containing the payload as a normal MIME part. The
task message lists its attachments' ids in `X-Orb-Task-Attachments` with
filename, content type and size.

Consequences to implement:

- Editing a task's title never re-uploads a 10 MB file.
- Attachment payloads download lazily and cache in
  `~/.orb/attachments/tasks/<task-id>/`; the local cache is disposable and
  rebuildable.
- Deleting a task expunges its attachment messages too. An orphaned attachment
  message (task gone, attachment left) is garbage-collected on sync.

The current `TaskEmailBuilder` ignores attachments entirely
(`src/tasks/rfc822.rs:32`), so this is new work.

## 10. Completion and trash

### 10.1 Trash

Deleting a task is a soft delete: `deleted_at` is set, the task moves to the
Trash view and disappears from every other view. The message stays in the tasks
folder with `X-Orb-Task-Deleted-At` set, so the deletion syncs.

Trashed tasks are permanently expunged after 30 days, or immediately via
"empty trash". Restoring clears `deleted_at`.

A task message that is hard-deleted on the server by something other than Orb
is applied as a permanent local deletion — it is indistinguishable from an
intentional expunge.

### 10.2 Completion

- Completing a task keeps it visible in Today, struck through, until the next
  local midnight. Vanishing instantly makes the action feel unconfirmable and
  makes undo hard to reach.
- A repeating task completed today is open again for its next date (§8), but
  Today shows it the same way, ticked and struck through, until midnight —
  found by the completion in its log. Ticking it again does nothing: it would
  complete the next date, which nobody meant.
- Two ways out of that, both about Today alone — the Completed view, the log
  and undo are unaffected:
  - `clear-completed` hides the tasks completed so far from Today until
    midnight, on every machine. It writes nothing to any task: the cut-off
    instant is one message of its own in the tasks folder
    (`X-Orb-Task-Type: today-cleared`), synced under the registry's rules
    (§7) and merged by taking the later instant. A task completed after it
    shows as usual.
  - `tasks.keep_completed_in_today: false` leaves completed tasks out of
    Today altogether; the status line's ⌘Z still takes a completion back.
- Un-completing is available from the Completed view and from a
  just-completed task.
- The Completed view is ordered by completion time, newest first, and is
  paginated rather than capped at an arbitrary 100.

### 10.3 Undo

Every task mutation — complete, delete, postpone, set due, reorder, tag change,
including bulk operations over a multi-selection — is undoable with a single
key immediately afterwards. Undo is backed by the log (§11): it restores the
previous field values as a new revision rather than rewinding history.

## 11. Task log

Every task keeps an append-only log so its timeline can be viewed: creation,
title and body edits, date changes (with old and new value), tag changes,
completion and un-completion, recurrence advancement, reminder fired,
link added or removed, trash and restore.

Each entry has its own UUID, a timestamp, a device identifier and a
machine-readable event type plus rendered text.

The log is stored in the task message as a structured MIME part and is the one
part of the task **exempt from last-writer-wins**: on conflict the log is
merged as the **union** of entries keyed by entry UUID, sorted by timestamp.
Two devices appending entries must not lose either device's entries.

Log entries are never edited or deleted; the log is trimmed only when the task
itself is permanently expunged.

## 12. Setting dates

In a task view, with one or more tasks selected:

- `s` opens the picker for the **postpone** date;
- `D` opens the picker for the **due** date.

Both are ordinary bindings in `bindings.task_normal` and are configurable,
consistent with every other mode in Orb. (The original `Cmd+S` /
`Cmd+Shift+D` proposal is dropped: `keys.ts:60` lowercases `metaKey`+letter, so
`Cmd+Shift+D` is currently indistinguishable from `Cmd+D`, and `Cmd+S` collides
with save. Cmd bindings can be added later once the notation grows a
`<d-s-x>` form.)

The picker opens with a text input focused at the top and a calendar below.
Typing in the input replaces the calendar with a list of suggestions; clearing
the input brings the calendar back. `<enter>` accepts the highlighted
suggestion, or the calendar's selected day when the input is empty. `<esc>`
cancels. An empty input plus `<c-d>` **clears** the date — unsetting a date must
be as easy as setting one.

All suggestions are relative to the current local date. In each suggestion the
part after the `-` is right-aligned and rendered in a lighter colour; for dates
more than a month out it shows the relative distance ("in 5 weeks") rather than
the weekday.

### 12.1 Accepted input

Assuming today is Aug 6, 2026:

| Input | Suggestions |
| --- | --- |
| `tomorr` | Tomorrow — Aug 7 |
| `toda` | Today — Aug 6 |
| `3` | September 3 — Thu · in 3 days — Aug 9 |
| `Sept` | September 1 — Tue |
| `Sept 2028` | September 1, 2028 — Wed |
| `Sept 2028 12` | September 12, 2028 — Tue |
| `in 3 days` | August 9 — Sun |
| `in 2 weeks` | August 20 — Thu |
| `in 3 months` | November 6 — Fri |
| `in 1 year` | August 6, 2027 — Fri |
| `next tuesday` | August 11 — Tue |
| `tue` | Tuesday — Aug 11 |
| `next week` | Monday — Aug 10 |
| `next month` | September 1 — Tue |
| `eom` | End of month — Aug 31 |
| `weekend` | Saturday — Aug 8 |
| `3d` / `2w` / `3m` / `1y` | same as the spelled-out forms |
| `+3` | in 3 days — Aug 9 |
| `2026-09-12` / `12.9.` / `12/9` | September 12 — Sat |

Rules:

- Relative offsets (`in n days/weeks/months/years`, and the short `3d`/`2w`
  forms) are accepted with and without the leading `in`, singular and plural.
- Weekday names, full or abbreviated, resolve to the **next** such weekday;
  `next <weekday>` resolves to the same day unless today *is* that weekday, in
  which case it means a week out. Both readings are offered when they differ,
  most likely first.
- A bare number is the next occurrence of that day of the month, and also
  offers the "in n days" reading.
- Out-of-year suggestions appear only when a year is typed. Offering
  September 2027 and September 2028 for `Sept` is noise; `Sept 15` is far more
  likely than `Sept 2028`.
- Parsing is locale-aware for month and weekday names (German and English), and
  for numeric date order.
- Input that parses to nothing shows "no matching date", never a silently wrong
  guess.

For reminders the same parser also accepts a time
(`tomorrow 9am`, `next tuesday 14:00`, `in 2 hours`), defaulting to 09:00.

## 13. Keybindings

New commands, all bound in `bindings.task_normal`, `bindings.task_detail`,
`bindings.task_edit` and `bindings.date_picker`, with sensible defaults
that follow the existing vocabulary:

`create-task`, `complete-task`, `uncomplete-task`, `delete-task`,
`restore-task`, `postpone-task`, `set-due-date`, `clear-dates`, `add-reminder`,
`edit-task`, `add-tag`, `remove-tag`, `link-message`, `open-linked-message`,
`move-task-up`, `move-task-down`, `undo`.

From the mail modes: `create-task-from-message` (list and detail), and
`open-tasks-for-message` in the detail view.

Defaults ship in the example configuration and the README, both of which are
kept current as part of this work.

## 14. Reliability requirements

Non-negotiable, and each one gets a test:

1. A task scheduled for a date one year out appears on that date, on every
   machine, in the correct local timezone, across DST boundaries.
2. A reminder set today fires at its time in a year, on every machine, whether
   or not Orb was running or the machine was awake at the time it was set.
3. No edit is ever lost: not to a concurrent edit on another machine, not to an
   upload racing a local edit, not to a crash mid-sync.
4. No task is ever silently lost: a crash between APPEND and DELETE leaves a
   recoverable duplicate, not a gap.
5. The local database is a cache. Deleting it and resyncing reproduces every
   task, tag, log entry and attachment exactly.
6. A server that never advances `HIGHESTMODSEQ` syncs correctly.

### 14.1 Tests

Integration tests against the Dovecot container in `tests/`, alongside the
existing store/imap/smtp suites:

- concurrent edit of one task on two simulated devices → highest revision wins,
  loser expunged, no duplicate survives;
- offline edit, then reconnect → change uploaded, dirty flag cleared only for
  the uploaded revision;
- edit racing an in-flight upload → the later edit still reaches the server;
- crash injected between APPEND and DELETE → next sync converges to one message;
- `UIDVALIDITY` change → full reconciliation, zero task loss;
- a task message hard-deleted server-side → deleted locally;
- a server stubbed to freeze `HIGHESTMODSEQ` → changes still detected;
- log entries appended on two devices → union preserved;
- date arithmetic across DST transitions and timezone changes;
- reminder reconciliation: schedule, move, delete, and the 64-notification cap.

Property tests for the fuzzy date parser: every suggestion it produces must
re-parse to the same date, and no input may produce a date in the past for a
future-only phrase.

## 15. Out of scope

- Subtasks: tasks inside tasks, with dates, tags and a life of their own. A
  task's checklist is §17.
- Sharing or assigning tasks to other people.
- CalDAV / Reminders.app interoperability.
- Per-occurrence editing of recurring tasks.

## 16. Invariants that must not be weakened

Each of these exists because its absence silently loses data, and each has a
container test in `tests/task_sync_integration.rs`. If a change needs one of
them relaxed to compile or to pass, the caller is wrong.

- **Identity is the task UUID, never the RFC `Message-ID`** (§3.1). Every edit
  is APPEND-then-delete, so a Message-ID identifies an edit, not a task.
  Conflicts resolve by highest revision, ties by highest UID.
- **Sync never relies on MODSEQ** (§3.3). Every sync reads a full header
  listing. Some servers advertise CONDSTORE and never advance `HIGHESTMODSEQ`;
  a delta-only design freezes there with no error.
- **Clearing the dirty flag is a compare-and-swap** against the revision that
  was uploaded (§3.2), or an edit made during an in-flight upload is marked
  clean and never sent.
- **A purge writes a tombstone in the same transaction as the delete** (§10.1).
  Reconciliation reads a task id with no local row as a task from another
  machine and downloads it; the tombstone is what stops an emptied trash from
  coming back on the next sync.
- **The per-task log is union-merged on `entry_uuid`** (§11), the one thing
  exempt from last-writer-wins. An upload over another machine's copy merges
  that copy's log first.
- **Two copies at one revision are told apart by `Message-ID`** (§3.1), for
  tasks and the tag registry alike. Comparing revisions alone leaves two
  machines on different versions of one task for good.
- **Adding or removing a reminder bumps the task's revision.** Reminders live
  in their own table but travel inside the task message; without the bump a
  reminder added locally is never uploaded and is deleted by the next download
  of that task.
- **`reminder_fired` log entries do not bump the revision.** Otherwise every
  machine re-uploads every task each morning. They ride along on the next edit
  and converge through the union merge.
- **An attachment that macOS would run rather than display** (`.app`,
  `.command`, `.pkg` and friends) is revealed in Finder, never opened (§9). An
  attachment is network input.
- **An export is complete on disk before it has its name** (§20.2): written to
  a `.partial`, synced, read back against its manifest and only then renamed,
  so a crash leaves the earlier snapshots as they were. Tested in
  `src/tasks/export.rs`.
- **Retention never removes the last complete snapshot, nor another
  machine's** (§20.2), and removes nothing until a new snapshot is complete.
  Tested in `src/tasks/export.rs`.
- **An export restores what it was taken from** (§21): imported into an
  empty folder and an empty database, synced, and exported again, it gives
  the same `tasks.json` but for time, device and revisions, and the same
  files. Tested without a server in `src/ffi.rs` and through one in
  `tests/task_export_roundtrip.rs`.
- **Default task bindings are vim letters, not `Cmd` chords.** The chords
  belong to the menu bar and the system (`⌘Z`, `⌘N`). The key normaliser in
  `app/src/keys.ts` tells `Cmd+Shift+D` (`<d-s-d>`) from `Cmd+D` (`<d-d>`), so a
  user may bind either, but no default takes one.

## 17. Steps

A task may carry an ordered checklist of **steps**: what doing it consists
of, ticked off one by one.

- A step has an id (a UUID, minted when it is added), a title and a done
  state — nothing else: no dates, tags, reminders or notes of its own. A step
  that needs those is a task.
- Steps are part of the task (§2). Adding, removing, renaming, reordering and
  ticking one is an edit of the task: it bumps the revision, sets the dirty
  flag and is resolved by §3.1 like any other field. Two machines editing
  different steps of one task at once are a conflict in which the higher
  revision wins; the log (§11), merged as a union, still has both.
- Every change but a reorder is logged: `step_added`, `step_removed`,
  `step_renamed` (old and new title), `step_completed` and `step_reopened`,
  each with the step's title at the time.
- On the wire the steps are a JSON part of the task message, `X-Orb-Task-Part:
  steps`, present only when there are any:
  `[{"id": "…", "title": "…", "done": false}, …]` in their order. A task
  that has steps is written as `X-Orb-Task-Version: 3`, every other task as
  version 2, and this build reads both. An older build skips a version 3
  message and leaves it untouched (§14), so it never writes the task back
  without its steps.
- Completing a task leaves its steps as they are. A recurring task that
  advances (§8) reopens all its steps for the next occurrence.
- The list shows a task's progress as done/total beside its title; the
  detail shows the checklist, with the first open step marked as the next.
  Steps are selected as list rows are — a click, ⌘-click, Shift-click — and
  a selection is removed, ticked or reopened as one edit; a step is moved by
  dragging it, and a selected step carries the selection with it as one
  block, in checklist order, again as one edit.
- Steps are written in place: adding one opens an empty field at the end of
  the checklist, which Enter keeps before opening the next; a double-click
  opens a step's title. With steps selected in the open task's checklist,
  deleting removes those steps, never the task.

## 18. Archive

Completed (§10.2) would keep every finished task forever, the recent ones
drowned in the old. A task completed longer ago than
`tasks.archive_after_days` (default 30; `0` never archives) leaves Completed
for the **Archived** view (`archive` where a command names it).

- **Derived, never stored.** Archived is "completed before now minus the
  setting", worked out when a view is read. No task is rewritten on the day
  it is archived — a stored flag would rewrite every finished task once, on
  every machine at once — and two machines with different settings simply
  show different views, with nothing to conflict.
- Archived is ordered by completion time, newest first, and paged like
  Completed. Nothing moves on the server: the message stays in the tasks
  folder.
- Un-completing an archived task brings it back like any other; the view's
  filter (§4.5) searches it as every view searches its own tasks.
- A recurring task is archived only once its series has finished, since until
  then completing it advances it (§8) and leaves it open.

## 19. Working on

A task can be marked as the one being worked on: the reminder, wherever the
window is, of what the user meant to be doing.

- **Local to the machine.** What one desk is working on says nothing about
  another's. The mark is neither a change nor a revision (§3), is not in the
  task's message and not in the log (§11); a copy of the task arriving from
  the server keeps the mark this machine set.
- Several tasks may carry it at once — work is often split between a task
  and the one it waits on — though one is the usual case.
- `work-on` toggles it on the task under the cursor or on the selection;
  `work-on clear` takes it off every task. Completing or trashing a task
  takes it off that task, so a finished task never claims to be in
  progress.
- **Seen from anywhere**: the status line names the task (or how many there
  are) and opens it on a click, and the Today entry in the sidebar carries a
  mark. In a list the task keeps its place (§4.4) and is marked, not moved.

## 20. Export

The tasks folder is the source of truth (§3), so every copy of a task lives on
one IMAP server. An export writes every task — open, completed, archived and
trashed — to a directory on this machine, in a documented format that Orb
reads back (§21) and that any other tool can read without Orb.

### 20.1 A snapshot

A snapshot is a directory, `orb-tasks-<YYYYMMDDTHHMMSSZ>-<hostname>/`, the
time in UTC:

- `tasks.json` — everything below.
- `attachments/<attachment-uuid>/<filename>` — each payload as a plain file.
- `schema.json` — a JSON Schema of `tasks.json`, so a reader needs nothing
  but the snapshot.
- `manifest.json` — `{"files": [{"path", "size", "sha256"}]}` for every other
  file, so a snapshot can be checked without being read into Orb.
- `README.txt` — a few lines on what the files are.

`tasks.json`:

```json
{
  "format": "orb-tasks",
  "format_version": 1,
  "exported_at": "2026-10-21T08:00:00Z",
  "orb_version": "0.9.0",
  "device_id": "…",
  "account": "Work",
  "folder": "Tasks",
  "complete": true,
  "tags": [{ "id": "…", "name": "work", "colour": "#e06060", "position": 1.0 }],
  "tasks": [{
    "id": "…", "revision": 7, "title": "…", "body": "…",
    "tags": ["…"], "postpone_date": "2026-10-21", "due_date": null,
    "reminders": [{ "remind_at": "2026-10-21T07:00:00Z", "fired": false }],
    "recurrence": { "rule": "FREQ=WEEKLY", "from_completion": false },
    "position": 3.5,
    "steps": [{ "id": "…", "title": "…", "done": false }],
    "created_at": "…", "updated_at": "…", "completed_at": null, "deleted_at": null,
    "links": [{ "message_id": "<…>", "subject": "…", "from": "…", "date": "…",
                "account": "…", "folder": "…" }],
    "attachments": [{ "id": "…", "filename": "…", "content_type": "…", "size": 1234,
                      "sha256": "…", "path": "attachments/…/…", "missing": null }],
    "log": [{ "id": "…", "at": "…", "device": "…", "event": "completed",
              "detail": { … }, "text": "Completed on 2026-10-21" }]
  }]
}
```

- **Everything on the wire for a task** (§2, §17), and nothing local: no row
  ids, UIDs, `Message-ID`s of task messages, MODSEQ, dirty flag or working-on
  mark (§19). A dirty task is exported as it is here: an unsynced edit is
  exactly what the server lacks.
- **Dates as this spec has them**: civil dates `YYYY-MM-DD`, instants RFC 3339
  in UTC; never a bare Unix number.
- **Stable output**: pretty-printed, keys in a fixed order, tasks by position,
  log entries by time, so two snapshots `diff` cleanly and a directory of them
  can be kept under git.
- **Incomplete is not failed.** A payload that cannot be fetched is recorded
  with `"path": null` and `"missing": "<why>"`, and `"complete": false`. The
  next export tries again.
- **Compatibility**: `format_version` rises only for a change an older reader
  would misread. A reader refuses a version newer than it knows, by name.

### 20.2 Writing one

- One read transaction for the whole snapshot, so a sync that commits midway
  cannot mix revisions.
- Every attachment payload, not only the cached ones (§9); one already in the
  previous complete snapshot under the same UUID is hard-linked from it.
- Written to `<name>.partial/`, synced to disk, checked by reading `tasks.json`
  back against the manifest, and only then renamed. A `.partial` left by a crash
  is deleted by the next export.
- **Retention**: the newest `keep` complete snapshots stay; older ones go only
  after a new one is complete, and the last complete snapshot never goes.
- The directory and its files are private to the user (`0700`/`0600`).
- One exporter at a time per machine, by a lock in the database; machines
  sharing one directory never collide, because the hostname is in the name.

### 20.3 When

`tasks.export` in the config (`directory`, `interval_hours`, `keep`) turns it
on. An export runs when the newest complete snapshot in the directory is older
than the interval — at launch, after a wake and on a timer — and on
`export-tasks`. The directory is the record of the last export, so nothing is
skipped because Orb was closed. A failed or incomplete export says so in the
status line and as a notification, and the warning stays while no complete
snapshot is younger than twice the interval.

## 21. Import

`import-tasks <snapshot>` reads a snapshot of §20 back.

- **A dry run first.** It says how many tasks are new here, newer in the
  snapshot, and as new or newer here; which are purged here and left out;
  which tags the registry gains; and what cannot be imported. Nothing is
  written until that is confirmed.
- **By task id, never by title** (§3.1). A task unknown here is created at its
  exported id. A known one is replaced only when the snapshot's revision is
  higher. The log is union-merged either way (§11), so an import never loses
  history the database had. Whatever is written is uploaded at a revision
  above both copies, so it wins on every machine.
- **A purged task stays purged** unless the import is told otherwise
  (`--include-purged`). Purged means a tombstone is still remembered (§10.1),
  or the task is trashed in the snapshot and gone here — the trash was emptied
  since, and the tombstone may already be forgotten. Bringing it back
  regardless would undo the purge on every machine.
- **Tags** the registry lacks are added by id, which uploads the registry; a
  tag it has keeps its name here (§7).
- **Attachments** are put in the cache from the snapshot, to go up with their
  task. A payload that does not match its SHA-256 stops its task, which the
  dry run names; one the snapshot lacks is left out of it.
- A snapshot in a format version newer than this build reads is refused, by
  name; every older one is read.

## 22. Importing Things 3

Things has no export. Its data is one SQLite database in its group container
(`~/Library/Group Containers/JLMPQHK86H.com.culturedcode.ThingsMac/ThingsData-*/Things Database.thingsdatabase/main.sqlite`),
which `import-things [path]` reads — always a copy, with its `-wal` and `-shm`,
so a running Things is neither locked nor written to.

- **A dry run first**, as for §21: how many to-dos become tasks, how many are
  done or repeating, what is left out and why; nothing is written until that
  is confirmed.
- **Each to-do's id is derived from its Things id**, the same on every import,
  so a later import finds what an earlier one wrote. Every import that
  writes a task leaves a mark in its log: the Things id, a fingerprint of
  everything the import carries over, and the revision the import left the
  task at. The log's other entries from Things have derived ids too, so
  importing again never doubles them (§11's union merge).
- **Importing again** brings over what changed in Things since:
  - a to-do whose fingerprint matches the last mark is unchanged and left
    alone;
  - one that changed, into a task still at the mark's revision — nothing
    edited in Orb since — updates the task, which keeps its place in the
    list, its linked mail and its files, and gets a new mark;
  - one that changed, into a task edited in Orb since, is left alone and
    named in the dry run: Orb's edit wins. Only a placeholder date (below)
    is cleared from such a task, as an edit of its own;
  - a to-do moved to the Things trash moves the task to Orb's trash, on the
    same condition;
  - a task purged from Orb's trash is not brought back.
  A task from an import before marks existed was left at revision 1, and is
  treated as marked there with nothing known of what it was.
- **The mapping**:
  - title and notes;
  - the start date to the postpone date, and the deadline to the due date —
    both stored by Things as `year << 16 | month << 12 | day << 7`; Today and
    This Evening are a start date of today; Inbox, Anytime and Someday
    without one are undated, and so is a date in the year 4000 or later,
    which Things keeps as a placeholder on repeating to-dos;
  - tags by name, and the title of the to-do's project and of its area as
    tags too, since Orb has neither;
  - the checklist to steps (§17), in order, ticked as they were;
  - a `message://` URL in the notes — the percent-encoded `Message-ID` of a
    mail — to a linked message (§6), beside the links the task has in Orb;
    the notes keep the URL;
  - a reminder time (`hour << 26 | minute << 20`) on the start date to a
    reminder (§5);
  - completed to-dos as completed at their completion time, and cancelled
    ones as completed with a log entry saying they were cancelled;
  - a repeating to-do to a recurrence (§8) where its rule — a property list
    of unit, interval, weekdays or days of the month, end — fits the RRULE
    subset, dated at its open instance; where it does not ("the second
    Monday"), an undated task with a log entry naming the rule. The open
    instance itself is the repeating to-do and is not imported again; past
    instances come over as completed tasks.
- **Left out**: the Things trash, and projects and headings as such.
- Imported tasks are written dirty, to be uploaded on the next sync (§3).

## 23. Uploading as you go

A task change reaches the server within seconds, not at the next full sync.

- **One upload per burst.** Every change to a task nudges an uploader in the
  core. It waits until the changes have stopped for a second, and never more
  than five seconds after the first, then uploads. Typing a title or a
  reorder that renumbers a list is one APPEND-then-delete per task, not one
  per keystroke.
- **A push lists nothing.** The upload sends only the dirty tasks, in
  revision order, exactly as step 7 of a full sync does, and deletes each
  task's previously recorded message. It runs only when the folder is as this
  machine last left it; otherwise it is a full sync (§3.3). It is a full sync
  too when the backlog holds anything a listing is needed for — a task whose
  message is not known, a tag registry change, a purge still to expunge — and
  when `UIDVALIDITY` is not the recorded one (§3.4).
- **"As this machine left it"** is the folder's STATUS: `UIDVALIDITY`,
  `UIDNEXT` and the message count, recorded after every round. Every edit on
  any machine is an APPEND, so `UIDNEXT` moves with each; the count catches a
  purge, which only deletes. MODSEQ plays no part (§3.3). After a push the
  record moves on only if this machine's own APPENDs account for all of
  `UIDNEXT`'s movement; if anything else moved it, the next round lists the
  folder. A server that reports no `UIDNEXT` gets a full sync every round.
- **Other machines' edits.** Every minute the folder's STATUS is compared
  with the record, and the folder is listed in full only if they differ. The
  same minute retries an upload that failed, which is how an offline change
  goes up on reconnect (§3.6).
- **One round at a time** in a process: a push, a poll and a full sync never
  overlap, or the same task would be uploaded twice.
- **Saved or not, always visible.** A task that is dirty, or has never been
  uploaded, is marked in the list and in the detail. The sidebar shows the
  number of changes still to go up beside *Tasks*, and *saved* when there
  are none. While the folder cannot be synced, the reason is shown there, in
  the status line and in the tasks account's standing, until a round
  succeeds.
- **A name the server keeps for itself.** Groupware servers such as Zimbra
  keep a `Tasks` folder that LIST does not show and SELECT cannot open: CREATE
  then answers that it exists. That combination is reported as a name clash
  that only another `tasks.folder` resolves (§1).

## 24. Completed first in Today

Today lists its tasks in the global order (§4.4), so a task completed today
stays where it was, struck through (§10.2). *Completed first* lists the
completed ones — and a repeating task done for today — above the open ones
instead, each group still in the global order.

- It is a way of showing Today, not an order: nothing is written to any
  task, and no other view is affected.
- It is a setting of the config, `tasks.completed_first_in_today`, off
  unless set; every window of the app follows it.

## 25. Following up a message

A task can be asked for on a message before it is sent, to follow it up: the
compose window takes a date, and once the message is sent a task linked to it
is made, titled from the subject (after subject munging) and postponed to
that date. From the message list and the detail view, the `postpone` form of
`create-task-from-message` asks for the date the same way before making its
tasks (§6.1).

- The link is the message as sent: its `Message-ID` is chosen before it goes
  out, so the task links what the recipients received. The cached folder is
  the account's Sent folder, where the copy is put.
- A message queued because the server cannot be reached gets its task when it
  is queued, and keeps its `Message-ID` until it is sent.
- A follow-up that cannot be made — no tasks configured, a date that is not
  one — is refused before the message is sent. Once the message has gone, a
  task that still fails is reported, and the message is not taken back.
- Dismissing the picker makes nothing: a chain that would have filed the
  message stops there. Clearing the date makes the tasks from the mail
  undated, in Someday, and in the compose window takes the follow-up back.
- Making the follow-up is one undo step, like any task made from a message.

## 26. Search results

The tasks a search finds (§4.5), and the tasks of Upcoming (§4.1), are
grouped by the date they come up — the earlier of postpone and due date, as
Today and Upcoming read it — under a heading each, in this order:

- **Today**: everything dated today or earlier, the overdue included and
  marked as they are in Today.
- **One heading per day** for the six days after today, the date's number
  big and the day beside it ("Tomorrow", then the weekday).
- **One heading per month** after that, for the months of the coming year
  (this month and the eleven after it), its name ("November"), with the year
  when it is not this year's.
- **One heading per year** after that ("2028").
- **Someday**: the undated. Search results only.
- **Completed**, newest first, folded until opened. Search results only.

Within a group, the earlier date first, then the global order (§4.4). The
date on a task's row is still its due date.

## 27. Reading in Today

Today can show a third list below its tasks: the reading list's items
(README, "Reading"). It is off until asked for, and it does not replace
either list of §4.1. The inbox mail is still All Inboxes.

- It shows either the must-read items (flagged) or the new ones (unread) in
  the reading folder, chosen by a switch in its heading. At most a
  screenful is listed; a link below opens the full view.
- Whether it is shown, and what it shows, is kept on this machine, like
  Today's layout (§24).
- It extends the focus model of §4.2. Stacked, `j`/`k` run from the last
  task into the first item and back. In columns it sits under the tasks,
  and `j`/`k` run through both. `<tab>` goes mail, tasks, reading, and back
  to the mail.
- Its items are mail. A key acts on the item under its cursor as it would
  on a message, and the reader shows that item. The section has a cursor
  and no selection.

## 28. Tasks from chat

A task can be made from a chat message (PLAN.md, Phase 15) as from mail
(§6.1). A task links one message, never a whole conversation: a thread
moves on, and what the task is about is what was asked.

- In the list, `t` makes a task of the thread's newest message, one task
  per thread of the selection; `Ctrl-t` asks for the postpone date first, as
  for mail. A message's own tool makes a task of that message. The task is
  titled from the message's first line, cut near 80 characters, and
  postponed to today unless a date was asked for.
- Making it takes the thread out of Waiting and marks it read on the
  server, as done does. A new message brings the thread back.
- Undo trashes the tasks and lets the threads wait as before.

### 28.1 The link

A chat link is a linked message of §2 and travels in the task message's
links part with the mail links. Its id is
`chat://<host>/<room id>/<message id>`, with `?thread=<thread id>` for a
message of a thread — never a bracketed `Message-ID`, so the two kinds never
meet. The cached fields of §6.1 hold what the link keeps of it:

| Field | For a chat message |
|---|---|
| subject | its text |
| from | its author |
| date | when it was written, RFC 3339 |
| account | the room's name as Orb shows it (`#dev`, `Direct`) |
| folder | its permalink |

The shape is the mail link's on purpose. An Orb that predates chat links
reads one as a mail link it cannot find (§6.2 step 3), keeps it, and writes
it back unchanged, so a task edited on such a Mac loses nothing. Checked
against §16: the task's identity, revision, log and sync are untouched; a
link is added through the same path as a mail link, which bumps the
revision.

### 28.2 Showing it

- The task detail lists the message with its room, author and text, and
  its reply state: replied once you have written in its thread — in its room
  outside any thread, for a message of the room's own timeline — after it.
  Opening it opens the room, the thread and the message, in the mailbox
  window; from a task window or the tasks window, the mailbox comes to the
  front first.
- In the conversation, a message carries the tasks made from it, not those
  in the trash; a click opens one. A thread with an open task made from one
  of its messages carries the task mark in its row.

### 28.3 Export

A link in `tasks.json` (§20.1) carries `"kind": "mail"` or `"kind":
"chat"`; a snapshot without it is all mail. A chat link also spells out its
parts for readers other than Orb:

```json
{ "kind": "chat", "message_id": "chat://chat.example.com/r1/m1?thread=t1",
  "subject": "Can you look at the log?", "from": "Sara", "date": "…",
  "account": "#dev", "folder": "https://chat.example.com/channel/dev/thread/t1?msg=m1",
  "chat": { "server": "chat.example.com", "room_id": "r1", "message_id": "m1",
            "thread_id": "t1", "room": "#dev",
            "permalink": "https://chat.example.com/channel/dev/thread/t1?msg=m1" } }
```

An import reads the id and the cached fields, as for mail. `format_version`
stays 1: an older reader reads a chat link as a mail link, which is what an
older Orb makes of it on the wire too.

## 29. Merging tasks

Two or more tasks that turn out to be one are merged into one of them.

- The tasks selected are merged into the one chosen to stay, the first of
  them unless another is picked. It keeps its id, its log, its place in the
  global order (§4.4) and its recurrence; the others go to the Trash
  (§10.1), restorable like anything trashed.
- Before anything is done a dialog proposes, and lets the user change:
  - the title: the kept task's;
  - the notes: the kept task's, then each other task's title and notes, so
    no title is lost;
  - the postpone and the due date: the earliest of each among the tasks,
    or any of their dates, or none.
- Everything else comes along without asking: every step (§17) in order,
  the kept task's first; every tag; every reminder still to come (§5) not
  already on the kept task; every linked message (§6); and every attachment
  (§9), copied to the kept task, since an attachment's payload belongs to
  its task's id and would go with it when the trash is emptied. The copies
  are fetched before anything is written, so a merge that cannot fetch one
  changes nothing.
- The whole merge is one undo step (§10.3): undoing it restores the kept
  task's fields, steps, tags, reminders, links and attachments, and brings
  the others back out of the Trash.

## 30. Grouping by tag

Today, Upcoming and Someday can list their tasks under their tags instead
of in one list (Upcoming: instead of under its dates, §26). One switch turns
it on for all three; it is a preference of the device, not synced.

- One heading per tag, in the registry's order (§7), each with the number
  of tasks under it. A tag no task in the view carries has no heading. A tag
  the registry cannot resolve comes after the known ones.
- **No tag** last, for the tasks without one.
- A task with several tags is listed under each. It is still one task: the
  view's count counts it once, a command on a selection holding it twice acts
  on it once, and whatever is done to it shows under every heading.
- Within a group the order is the view's own: the global order (§4.4) in
  Today and Someday, with Completed first applied within each group (§24);
  the date order in Upcoming.
- A drag or a keyboard move stays within the task's group, and places the
  task between its neighbours there in the global order.

## 31. Due first in Today

Today can list the tasks due today or earlier above the rest. A switch in its
Tasks heading turns it on; it is a preference of the device, not synced.

- *Due* means open — neither completed nor done for today (§10.2) — with a
  due date on or before today. Completing a task takes it out.
- The switch carries the number of such tasks in Today, in red, whether it is
  on or not; with none it shows no number.
- It is a way of showing Today, not an order: nothing is written to any task,
  and no other view is affected.
- Today's bands, top to bottom: the completed tasks under Completed first
  (§24), the due ones, the rest. Each band is in the global order (§4.4).
- Ungrouped, the due tasks stand under a *Due* heading with their number and
  the rest under *Other*. Grouped by tag (§30), the bands apply within each
  group, the due tasks set apart from the rest by a line.
- A drag or a keyboard move stays within the task's band, and places the task
  between its neighbours there in the global order — as within a tag's group.

## 32. The series of a repeating task

A repeating task (§8) is one occurrence of a series at a time. The rule gives
each occurrence its **day**; the series says what hangs on that day, so that
the deadline and the reminders are the same distance from it every time, and
so that moving one occurrence by hand changes neither the series nor where
the next one falls.

The series travels in `X-Orb-Task-Recurrence-Series`, beside the rule:

```
X-Orb-Task-Recurrence-Series: day=2026-11-30; due=2; reminders=-1T18:00,0T09:00
```

- `day` — the day the rule gave the current occurrence. The next one is
  counted from it (§8), not from the task's dates.
- `anchor=due` — the day is the task's due date: it is due on its day and
  shown before it. Absent, the day is the postpone date.
- `due=n` — the deadline is `n` days after the day, `0` on it. Absent, there
  is none. Only with the day on the postpone date.
- `reminders` — each as `<days>T<HH:MM>`, the days counted from the day and
  negative before it; `none` for a series said to have no reminders. Absent,
  nobody has said: the series then takes the task's reminders as they stand
  when the occurrence next moves, and says so from then on.
- A part a version does not know is passed over. A series that cannot be read
  is dropped, and the task repeats by its dates, as one written before there
  were series.

The task's own dates and reminders stay what they were: the current
occurrence, worked out from the series. A client that knows nothing of the
series shows the task on the right days and reminds at the right times.

Rules:

- Making a dated task repeat states its series from its dates: the postpone
  date is the day and the due date so many days after it; with a due date
  alone, the due date is the day. A task with no date states none until it
  has one, and the day it is first completed on is then its day.
- With the day on the postpone date, setting a **postpone date** moves this
  occurrence: its deadline and its reminders go with it, the series stays, and
  the next occurrence is where the rule puts it. Setting a **due date** says
  how long after its day the series is due, counted from where this
  occurrence is; a date before that day is refused, and clearing it is a
  series without a deadline.
- With the day on the due date, setting a due date moves this occurrence, and
  a postpone date hides this occurrence only. Setting both at once puts the
  day on the postpone date from then on.
- Clearing both dates leaves this occurrence undated and the series as it is.
- A reminder added to a repeating task is one of the series, as far from the
  day of this occurrence as it was set; one removed is removed from the
  series. Either is one revision, the series with the reminder (§16).
- Whenever the day of the occurrence changes — a completion, a move by hand,
  an undo of either — the task's reminders are replaced by those of the series
  on the new day, not yet fired. A reminder that is not of the series, such as
  a snooze (§5), is not carried over; one that fired and was cleared away is
  there again.
- A task that stops repeating has no series. Its dates and reminders stay.
- The date picker (§12) reads the deadline and the reminders of a repeating
  task against its day, and labels every reading with its distance from it:

  | Input | Reading |
  |---|---|
  | `on the day`, `same day`, `am Tag` | the day |
  | `2 days after`, `1 week later`, `+3`, `2 Tage später` | after it |
  | `day before`, `2 days before`, `-1`, `am Vortag`, `1 Tag vorher` | before it, for a reminder |
  | `on the day 9am`, `day before 18:00`, `7:30` | a reminder; a time alone is on the day |

  A date said any other way is taken as that date and kept as its distance.
  Nothing before the day is offered for the deadline.
- An export (§20) carries the series as `recurrence.series`, in the header's
  form.
