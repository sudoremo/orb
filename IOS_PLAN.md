# Orb on the iPhone

This is the plan for an iOS app: Orb's mail and tasks on the iPhone, native to
the platform, sharing the core with the desktop app and leaving the desktop
app as it is. It was written on 2026-10-22 against the code of that day; file
references are a starting point, not a specification.

The list below is kept the way `PLAN.md` keeps its own: delete an item when it
is done, tick a subphase when it is empty, tick a phase when its subphases are
all ticked, and commit the plan with the work.

## Progress

- [x] Phase 0: A core that builds for iOS
  - [x] 0.1 Desktop-only code behind a feature
  - [x] 0.2 The iOS targets in CI
- [x] Phase 1: The bridge
  - [x] 1.1 `orb-mobile`
  - [x] 1.2 The XCFramework and the Swift package
  - [x] 1.3 A first screen
- [ ] Phase 2: Reading mail
  - [ ] 2.1 Accounts and configuration
  - [ ] 2.2 Folders, list and threads
  - [ ] 2.3 The reader
  - [ ] 2.4 Acting on mail
  - [ ] 2.5 Search
- [ ] Phase 3: Writing mail
  - [ ] 3.1 Compose and send
  - [ ] 3.2 Drafts, attachments and contacts
  - [ ] 3.3 The share extension
- [ ] Phase 4: Tasks
  - [ ] 4.1 Lists and Today
  - [ ] 4.2 Detail and editing
  - [x] 4.3 Reminders
- [ ] Phase 5: Living in the background
  - [ ] 5.1 Background refresh
  - [ ] 5.2 Leaving the foreground safely
  - [ ] 5.3 Badges and notifications
- [ ] Phase 6: The rest of the platform
  - [ ] 6.1 Widgets
  - [ ] 6.2 Shortcuts and Spotlight
  - [ ] 6.3 iPad and a hardware keyboard

---

## Decisions

**A SwiftUI app over `orb-core`, not Tauri on iOS.** Tauri 2 can build for
iOS, but `app/src` is a desktop interface — panes, a command line, an
embedded Neovim grid, Ctrl+H/J/K/L — and the phone needs a different one
regardless. In a webview it would not get what makes an iOS app feel like one:
the system navigation transitions, swipe actions, context menus, sheets,
Dynamic Type, VoiceOver and the system materials. Sharing the frontend would
also fill it with `if (mobile)` branches, which is exactly the disturbance to
the desktop this plan must avoid.

**The core is what is shared.** Mail, sync, storage and tasks live in
`orb-core`, and `OrbHandle` (`src/ffi.rs`) is already a complete API that the
platform reaches through traits (`PushListener`, `PasswordPrompter`,
`TaskNotificationHost`, `PasswordStore`). Mail and tasks both sync through
IMAP, so the phone and the desktop converge without a server of Orb's own.

**A separate crate for the bindings.** The SwiftUI client removed in
`30d05f92` exported the `OrbHandle` impl block through UniFFI, which made
every method placed there public API — that is how an internal helper got
published. This time the Swift API is a thin crate of its own,
`crates/orb-mobile`, that delegates to `OrbHandle`: nothing is exported that
is not written there. The data types in `src/ffi.rs` derive the UniFFI traits
only under a `uniffi` feature, off by default. Generated bindings are built,
never committed.

**New mail arrives by background refresh, not by push.** iOS closes the IMAP
IDLE connection once the app is in the background, and instant notifications
would need a relay server holding the connection and the account's
credentials. Orb does without: it syncs when it comes to the foreground and
whenever iOS grants a background refresh. New mail can therefore show up
late, at a time iOS decides.

**Tasks first.** The first version does tasks only, as
`docs/mockups/ios-tasks.html` shows it: Today, Tasks and Search tabs. Mail,
chat and the reading list get tabs of their own later. Phase 4 therefore
follows Phase 1, and Phases 2 and 3 come after it. The mockup is the reference
for the screens, the gestures and the vocabulary.

**The iOS side is checked locally, not in CI.** GitHub bills macOS runner
minutes ten times over, more than a free account has to spare.
`scripts/test-ios.sh` runs every iOS check and is part of the pre-commit gates
in `CLAUDE.md`. Phase 0.2 and 1.2 say "CI" for what that script now does.

**The phone keeps a working set, not the mailbox.** Decided 2026-11-01.
Headers are kept for a window, six months by default, in the folders that
sync (Inbox, Sent, Drafts, Archive); older mail is listed from the server
on request and found by server search. Bodies and attachments live within
a fixed budget, about 500 MB by default and adjustable in Settings; the
least recently opened go first, and never a draft or anything not yet
uploaded. The Mac keeps syncing everything: each of these is a setting
whose default is today's behaviour.

**Setup is a code from the Mac** (decided 2026-11-02). Tasks ▸ Set Up
iPhone… on the Mac shows a QR code with everything the phone needs, the
password included; it says so, and works for five minutes. The phone scans
it, or reads it from a photo, checks the account and is set up.

**The desktop stays as it is**, apart from that one dialog. The core's default features are today's
behaviour, every gate in `CLAUDE.md` keeps running unchanged, and nothing in
`app/` changes for this plan.

**Drift is a decision, not an accident.** The removed SwiftUI client fell
behind silently. The iOS app is a smaller product by design, but a feature
the desktop gains is either carried over or recorded below under "Not on
iOS" — never just skipped.

## Layout

```
Cargo.toml          orb-core, as today
app/                the Tauri desktop app, unchanged
crates/orb-mobile/  the UniFFI layer over OrbHandle
ios/                XcodeGen project.yml, the SwiftUI app, OrbKit (Swift package)
scripts/build-xcframework.sh
```

---

## Phase 0: A core that builds for iOS

Nothing a user can see. When it is done, `orb-core` builds for the iPhone and
the simulator, and the desktop builds and behaves exactly as before.

### 0.1 Desktop-only code behind a feature

### 0.2 The iOS targets in CI

## Phase 1: The bridge

### 1.1 `orb-mobile`

### 1.2 The XCFramework and the Swift package

### 1.3 A first screen

## Phase 2: Reading mail

### 2.1 Accounts and configuration

- **The config lives in the app container**, in the same YAML schema as on
  the desktop, read with `ffi::init`. iOS writes it; the user never edits it
  by hand.
- **Set up a mail account in the app**: name, address and SMTP server
  beside what the tasks setup already asks and checks.
- **Import a desktop `config.yml`** from Files or iCloud Drive. Keys the
  phone cannot honour (password commands, the editor, `escalate`) become
  `config_warnings`, and the import asks for the passwords they would have
  supplied.
- **Measure a header's cost** on a large synthetic mailbox (extend
  `tests/body_scale.rs` to report bytes per message, index included), to
  confirm the six-month default.
- **A header window in the core sync**: headers only `SINCE` a date, and
  older batches fetched on request ("Load older" at the end of a list).
  Read `docs/CONCURRENCY_DESIGN.md` first.
- **A storage budget** for bodies and attachments, evicting the least
  recently opened; bodies newer than a month are fetched ahead, the rest
  when opened. Settings show the space used and offer "Free space now".
- **Folders outside the synced set** listed from the server when opened.
- **Search by meaning is opt-in**: the embedding model is downloaded on
  request, not shipped in the app.

### 2.2 Folders, list and threads

- **A tab bar** — Mail, Today, Tasks, Search — over a `NavigationSplitView`
  that collapses to a stack on the iPhone.
- **Folders** per account, with the virtual folders and unread counts.
- **The message list** as a native `List`: sender, subject, preview, date,
  flags and highlight badges, threads collapsed. Pull to refresh asks
  `request_sync`.

### 2.3 The reader

- **A `WKWebView` for the body**, showing the HTML the core already prepares,
  inline images included and remote content blocked as on the desktop.
- **The thread above or below the message**, from `thread_messages`.
- **Attachments** previewed with Quick Look and shared through the share
  sheet.

### 2.4 Acting on mail

- **Swipe actions** for archive, delete, flag and read, and the rest in a
  long-press context menu, all through the existing `OrbHandle` actions.
- **Move and copy** with a folder picker, undo by shaking or from a toast.
- **Staged actions** reviewed and executed from a sheet.

### 2.5 Search

- **`.searchable`** with scopes from `SearchScope`, recent searches and the
  saved searches from the config.
- **Search on the server** as a second step, an IMAP `SEARCH` over the
  whole mailbox, for what lies outside the header window or the cached
  bodies; offline, the app says why nothing older is found.

## Phase 3: Writing mail

### 3.1 Compose and send

- **A compose sheet**: identity, recipients as tokens, subject, body in a
  native text view, signatures from the config. Reply, reply all and forward
  start from `reply_draft` and `forward_draft`.
- **The outbox drains** as on the desktop; a mail sent without a network
  waits there.

### 3.2 Drafts, attachments and contacts

- **Drafts** saved to the server as on the desktop, and opened from the
  Drafts folder.
- **Attachments** from Photos and Files.
- **Recipients completed** from the iOS address book through
  `CNContactStore`, passed to the core as the desktop's `contacts.rs` does.

### 3.3 The share extension

- **Share to Orb as a new mail**, beside the new task it already makes,
  once Mail has arrived.

## Phase 4: Tasks

Read `todo-spec.md`, and §16 before touching anything that syncs.

### 4.1 Lists and Today

- **Today's mail in Today**, as conversations, once Mail has arrived (§4.1).
- **Reorder by long press and drag**, without entering Select first (§4.4).
  Select mode has the grips; a long press opens the context menu.
- **Quick add tints a chip's words in place** in the title, as the mockup
  draws them; they are chips below the title for now.
- **A tag's list through the search**, once the core's search reads a quoted
  tag name (PLAN.md 5.2); it is read through the views' filters until then.


### 4.2 Detail and editing

- **Try attachments by hand**: adding from Photos and Files and opening in
  Quick Look are built, and the core's side is tested, but the pickers run
  out of process where a UI test cannot reach them.

### 4.3 Reminders

## Phase 5: Living in the background

### 5.1 Background refresh

- **A task sync bounded by the time iOS grants.** The refresh syncs the
  tasks folder, but the core cannot cut a task sync short: it runs to its
  IMAP timeouts (30 s to connect, 60 s an operation), past the 30 s a refresh
  gets. Stopping it cleanly — the folder lock released, the database and the
  server consistent — needs §16 read first.
- **Verify the refresh on a device.** iOS does not schedule refreshes in the
  simulator, and simulating one needs the debugger attached (developer mode).
- **The inboxes in the refresh**, once Mail has arrived, the most recently
  used account first; IDLE on coming to the foreground, stopped on leaving.

### 5.2 Leaving the foreground safely

- **Finish the mail in flight** within the grace period, once Mail has
  arrived: drain the outbox, run staged actions, save the draft being
  written. Task changes already go up in it.
- **Never move the database into the App Group container.** iOS terminates
  an app that holds a SQLite lock in a shared container while suspended
  (`0xdead10cc`). It lives in the app's own container; widgets and the share
  extension get snapshots instead (3.3, 6.1).

### 5.3 Badges and notifications

- **New mail notifications** after a background sync, from
  `new_mail_since`, grouped by account.
- **The app icon badge** counts Today's open tasks; whether the inbox's
  unread count joins it is a decision for when Mail arrives.

## Phase 6: The rest of the platform

### 6.1 Widgets

- **Tick a task off from the Home Screen** and check that it lands: the
  widgets are built and draw the snapshot, but no test reaches a widget on
  the Home Screen, so the tick (a live-activity intent run in the app's
  process) has only been compiled.
- **An unread widget**, once Mail has arrived.

### 6.2 Shortcuts and Spotlight

- **New mail as an App Intent**, and recent mail in Spotlight, once Mail
  has arrived.
- **Check Spotlight end to end on a device.** Open tasks are indexed and a
  result opens its task, but the simulator's Spotlight could not be driven
  from a UI test.
- **Run Add Task from Shortcuts or Siri once by hand**: no test reaches it.

### 6.3 iPad and a hardware keyboard

- **The Tasks tab side by side on the iPad** as Today and Search are; and
  folders, list and reader once Mail has arrived.
- **Keyboard shortcuts from `BindingsConfig`** where they map to an action
  the app has. The standard ones are there: ⌘N, ⌘Z, ⌘1, ⌘2, ⌘F and ⌘R.
- **The command palette on the iPad**, ⌘K and ⇧⌘K with a hardware keyboard
  as on the Mac (README, "The command palette"): the places, the task
  commands by their titles from `commanddocs.ts`, `#` the tags and `/` the
  tasks found by the search, greyed out where they cannot act.
- **The icon badge test on the iPad**, whose Home Screen does not expose the
  badge to UI tests; it is skipped there.

---

## Not on iOS

What the desktop has and the phone deliberately does not. Add to this list
rather than skipping a feature silently.

- **Neovim as the editor.** Compose uses the native text view.
- **Password commands and `escalate`.** No process can be started on iOS;
  passwords live in the keychain.
- **Push for new mail.** No relay server: new mail arrives by background
  refresh (see Decisions).
- **The Terminal look and the command line.**
- **Task export and import (Phase 12 of `PLAN.md`).**
