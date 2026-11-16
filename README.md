# Orb

A fast, keyboard-driven email client for IMAP. Orb is a
[Tauri](https://tauri.app) app built on top of a Rust core library
(`orb-core`) that handles IMAP sync, local caching, sending, and search.

The interface is driven by your own vim-style keybindings from
`config.yml`, and the body of a message is written in Neovim, running inside
the compose window, rather than in a text box.

> [!WARNING]
> **Early Development Version**
>
> Orb is currently in early development and may contain bugs or incomplete features.
> Use at your own risk. Always ensure you have backups of your email data and test
> with a non-critical account first. The authors are not responsible for any data
> loss or other issues that may arise from using this software.

## Features

- **Multi-account IMAP** – Sync several IMAP accounts, browsed in a unified
  sidebar; folders can be created, renamed, deleted and subscribed from the
  app, and the special folders are found by the roles the server announces.
  See [Folders](#folders).
- **Local SQLite cache** – Messages are cached locally for fast, offline browsing.
  In the background Orb fetches the text of every message, for search, and the
  HTML of the newest 300 in each folder — the open folder first — so recent
  mail opens without waiting on the server. Mail that has just arrived is
  fetched whole by the sync that finds it, up to 20 messages and 4 MB at a
  time.
- **Send mail (SMTP)** – Compose, reply, reply-all, and forward, with Cc/Bcc
  support. A reply goes to the sender's `Reply-To` address when there is one,
  and a reply to a message you sent goes to its recipients again. It carries
  `In-Reply-To` and `References` built from the message it answers, so it
  stays in its thread in other clients. Once it is sent, the message it
  answers is marked answered (`\Answered`) on its server, as other clients do;
  a forward marks the message it forwards `$Forwarded`, and names it in an
  `X-Forwarded-Message-Id` header, so a forward saved as a draft still knows.
- **Identities** – An account can send as several addresses, each with its own
  signature and `Reply-To`, and optionally its own SMTP server — for an
  address whose mail is forwarded into the account but must be sent
  elsewhere. A reply or forward is written as the address the original was
  sent to, and the compose window switches between them.
- **Filter rules** – Rules in the config file act on mail as it arrives: file
  it, mark it read or flagged, delete it to Trash, make a task of it. A rule
  runs once per message, on the first Orb that syncs it; `:filter` runs the
  rules on mail already here. See [Filter rules](#filter-rules-optional).
- **Conversations** – A folder can be shown as threads, built from
  `In-Reply-To` and `References` with the subject as a fallback, opened and
  closed one at a time; and any message's whole conversation can be shown,
  gathered from every folder of its account. See
  [Conversations](#conversations).
- **Highlight rules** – Rules in the config file change how a message looks:
  a colour, a background, an icon, a label chip, bold, italic or dimmed, and
  a count in the sidebar. Evaluated as the row is drawn, so they apply to
  every message at once. See [Highlight rules](#highlight-rules-optional).
- **Push (IMAP IDLE)** – INBOX, and any folder marked `watch`, is watched with
  RFC 2177 IDLE, so new mail arrives when the server sends it rather than at the
  next sync. A notification names the folder that changed, so only that folder
  is synced; a burst of notifications is one sync and one more, not one each.
- **Periodic sync** – Every account is synced whole at its `sync_interval_seconds`
  (default five minutes), which is also how folders that are not watched, and
  servers without IDLE, get their mail. After the machine wakes from sleep, or
  moves to another network — another Wi-Fi, a cable, a VPN — every connection
  is reopened and every account synced.
- **Any server** – TLS or STARTTLS on IMAP, TLS or STARTTLS on SMTP, a
  password or an OAuth 2.0 token (Gmail, Office 365), and a sync that works
  without CONDSTORE (Exchange) by listing the folder instead of asking what
  changed. See [OAuth 2.0](#oauth-20).
- **One connection per account** – Everything that talks to a server — syncs,
  marking read, moving, opening a message — shares one authenticated connection
  kept warm in a pool. Every command on it has a timeout, the socket has TCP
  keepalive, and a connection that stops answering is replaced rather than
  waited for.
- **Local-first flags and an outbox** – Marking a message read or flagged,
  moving or deleting it, and applying staged actions, changes the local copy
  at once and queues the server write; offline, the change stays and reaches the server with the
  next sync. A message sent while
  the mail server cannot be reached, or asks to be tried later (a 4xx reply),
  waits in the [Outbox](#outbox) and goes out when it can; a message the
  server refuses (a 5xx reply, a wrong password among them) is an error at
  once, never queued.
- **Full-text search** – `/` searches everything from every view: mail in
  every folder of every account, backed by SQLite FTS5, and every task, the
  tasks grouped by the date they come up; plus `~` search by meaning over a
  local embedding model. See [Search](#search). The words searched for are marked in the list and the reader, `n` and
  `N` walk them, and the results stay open across a sync, refreshed in place.
  Results come most relevant first — newest first for an `OR` or a search by
  conditions alone, closest first for `~` — or in the order `search.sort`
  names, and the toolbar's sort menu or `sort-by` orders them by date, sender
  or subject instead. The order is applied before the 500-result limit, so
  oldest first finds the oldest matches. `sort-by relevance` goes back.
- **HTML mail rendering** – HTML bodies render in a sandboxed iframe with no
  scripts, no same-origin access and no network: remote images, stylesheets
  and fonts — tracking pixels included — are blocked by a content security
  policy, and images the message embeds are shown from its own parts. A
  message with something remote in it says so above the body, with a button
  (or `load-remote-images`) that lets its images load, this once; senders in
  `load_remote_images_from`, and messages a `load_remote_images` rule
  matches, get theirs without asking. Stylesheets and fonts
  stay blocked either way, and tracking pixels — images a pixel or less in
  size, hidden ones, and those from hosts that serve nothing else — are
  taken out of the message, so they stay out once its images load. A link
  shows where it really goes when the pointer rests on it. Plain-text bodies render as monospaced text,
  the signature greyed out from its `-- ` line to the end, or to the quote
  below it.
- **Suspect mail** – A sender whose name is another address than the real
  one shows the real address in red behind a warning mark, in the list and
  in the reader. Under the header, the reader says when the receiving
  server could not verify the sender (DMARC failed, or SPF failed with no
  DKIM signature passing, as the topmost `Authentication-Results` records
  it), and when replies go to an address outside the sender's domain. A
  link that shows one address and opens another, or whose address is a bare
  IP, hides the site behind a name and an `@`, or is spelled with
  international characters, asks before it opens. Every link opens without
  its tracking parameters (`utm_*`, `fbclid`, `gclid`, `mc_eid` and the
  like). Attachments that are dangerous to open are marked and asked about
  (see [Attachments](#attachments)).
- **Attachments** – Attach files to outgoing messages. A forward carries the
  original's attachments along, and fails rather than go out without them when
  they cannot be unpacked. Incoming attachments are listed under the message's
  headers with their size, and can be opened, shown in Quick Look, saved and
  dragged out of the window (see [Attachments](#attachments)).
- **New-mail notifications and the Dock badge** – Unread mail that arrives in
  a watched folder is announced, one notification per account; clicking it
  shows the message (see [Jumping to a message](#jumping-to-a-message)). A
  message a [filter rule](#filter-rules-optional) files away or marks read is
  not announced, and neither is the mail a folder's first sync brings. The
  Dock badge counts the unread mail in the inboxes.
- **Undo** – ⌘Z takes back the last action, whatever it acted on — a read,
  flag, move, archive or delete, a task change, a chat thread marked done —
  and ⇧⌘Z does it again; the status line says when it can.
- **Account status** – The sidebar marks an account whose server refused the
  login, presented a bad certificate or could not be reached, and
  `account-status` says what happened and since when.
- **Tasks over IMAP** – A task list that syncs, stored as ordinary messages in
  one designated IMAP folder, so every machine sees the same list with no server
  of its own. Postpone and due dates, recurrence, tags, attachments and an
  append-only per-task log. Reminders are handed to macOS as scheduled
  notifications, so they fire whether or not Orb is running. One keystroke turns
  the message you are reading into a task, and the link survives the message
  being moved, renamed or deleted. See [Tasks](#tasks).
- **A reading list** – Newsletters set aside with `b` from any account, and
  the items of RSS, Atom and JSON feeds, in two folders of one account: one
  for what is still to read, one for what has been read. An item marked must read is flagged. Plain IMAP folders and
  flags, so every machine and every other client sees the same list. See
  [Reading](#reading).
- **Rocket.Chat** – One chat server beside the mail: Waiting lists the
  threads that wait for an answer, one row each with the newest message as its
  preview, and the reader shows the conversation with a composer under it.
  Reading in Orb marks nothing read on the server; answering and done do. See
  [Chat](#chat).
- **Your keybindings** – The `bindings` tables in `config.yml` drive the UI
  directly, including counts (`5j`), multi-key sequences (`gg`) and chained
  commands (`stage-read && mark-move Archive && advance`). The same command
  vocabulary backs the command line (`:`), the menu bar and the right-click
  menu, and `?` lays every key in force over the window, to browse or search.
- **Compose in Neovim** – `C` and `r` open a compose window with your own
  Neovim embedded in it: your `init.lua`, plugins and colour scheme, drawn by
  the window in the app's font. Dead keys and input methods work as anywhere
  else on the Mac, and `⌘V` pastes. Plugins can tell they run inside Orb from
  the client name `orb` (`nvim_get_chan_info`). The signature is shown
  below the text rather than in it, where nothing can edit it by accident.
  Without Neovim on the `PATH`, or after quitting it, the body is edited in a
  plain text field instead.
- **Mail templates** – YAML files with placeholders in braces, inserted into a
  message or reply with `⌘T`; the placeholders are filled in from fields under
  the header, and Send stops at one still in braces (see
  [Templates](#templates)).
- **Virtualized message list** – Folders of any size scroll smoothly; only the
  visible rows exist in the DOM and pages are fetched as you reach them.
- **Staged pending actions** – An aerc-style workflow: stage a delete or move,
  see it marked in the list, then apply everything staged in the folder as a
  batch (see [Pending actions](#pending-actions)).
- **Delete** – Deleting moves the message to the account's Trash folder,
  whether at once (⌘⌫, `delete-immediate`) or staged (`mark-delete`). In Trash
  itself it deletes for good: the immediate delete asks first, the staged one
  when it is applied.

## Requirements

- macOS 14 or later
- A Rust toolchain (Rust 2024 edition)
- Node with [pnpm](https://pnpm.io)
- Neovim on the `PATH`, for composing (optional — without it the body is
  written in a plain text field)

The `PATH` is the one your login shell has: at launch Orb asks `$SHELL` for
it, so `nvim`, `pass`, `op`, `gpg` and a password command installed by
Homebrew are found whether Orb was started from a terminal or from the Dock.

## Build & Run

```bash
./start.sh            # build a debug bundle from the checkout and launch it
./start.sh --release  # the same, optimised
./start.sh --dev      # the bare binary, with the frontend reloading on save
cd app && pnpm tauri build   # produce a release bundle
```

`start.sh` signs the bundle with a certificate when the machine has one —
Developer ID, else Apple Development — so it shares the permissions macOS
granted the installed Orb. Signed ad hoc, a bundle is a new app to macOS
after every build, and Accessibility and Contacts are asked for again. Signed
with a certificate, it runs under the hardened runtime, and
`app/src-tauri/Orb.entitlements` declares the address book for it. Only a
bundle may post notifications: under `--dev` no reminder is scheduled and no
new mail is announced.

The Rust core is compiled as a normal path dependency of `app/src-tauri`, so
there is no separate build step. Run the core's test suite with `cargo test`
from the repository root.

### Testing the UI without a mailbox

`app/harness.html` drives the real frontend against a stubbed backend and a
synthetic folder, which is how the message list is exercised at sizes no test
mailbox reaches:

```bash
cd app && pnpm dev     # then open http://localhost:1420/harness.html?total=200000
```

## Configuration

Orb is configured via a YAML file at **`~/.config/orb/config.yml`**. On a
launch that finds no such file, Orb writes a template there with the required
keys, opens it in your text editor and quits; fill in the account and launch
again.

Set `ORB_CONFIG` to read another file instead, for example to run a second
Orb against a test account with a database of its own:

```bash
ORB_CONFIG=~/orb-test/config.yml ./start.sh
```

[`config.example.yml`](config.example.yml) shows every key with a comment and
its default. A key Orb does not know is an error at launch, so a typo cannot
pass as a setting that quietly does nothing; the error names the key and the
line, in a dialog when Orb was started from the Dock and on stderr as well
when it was started from a terminal.

### Changes while Orb runs

The file is re-read within a couple of seconds of being saved. Most keys take
effect at once: the bindings, the reply settings, the identities and their
signatures, the filter and highlight rules, the special folder names,
folder visibility, order and display names, the remote-image allowlist, the
reader settings, the subject munges and the contacts. The status line says
`config.yml reloaded`, and names what needs a restart when something does:
`db_path`, `log_file`, `embedding_model`, `tasks`, and an account whose
server, port, username, password command, sync interval or `sync`/`watch`
flags changed — the connections and the sync schedule were built from those
at launch. A saved file that does not parse changes nothing: the status line
carries the error, and the previous configuration stays in force.

Most values can be written bare, as in the examples below. Quote a key or a
value that begins with one of `! & * # [ ] { } | > % @ ~ ' "` or a backtick,
that is `-` or `?` followed by a space, or that contains `: ` or ` #`. Bare
`~` and `null` are YAML's null and are rejected where a string is expected,
so a binding on `~` is written `"~"`. Bracketed keys such as `<c-r>` and bare
letters need no quotes. Single quotes keep a backslash a backslash, which is
what a regular expression wants; double quotes make `\uXXXX` an escape.

### Minimal configuration

```yaml
# Path to the local SQLite cache (required; supports ~ expansion)
db_path: ~/.local/share/orb/mail.db

accounts:
  - name: Personal
    imap_server: imap.example.com
    imap_port: 993
    imap_username: user@example.com
    imap_password_command: pass email/example.com
    from: Your Name <user@example.com>
```

### Account reference

Each entry in `accounts` supports the following keys:

| Key | Required | Description |
|-----|----------|-------------|
| `name` | yes | Display name for the account. |
| `imap_server` | yes | IMAP server hostname. |
| `imap_port` | yes | IMAP port (usually `993` for TLS). |
| `imap_username` | yes | IMAP login username. |
| `imap_password_command` | no | Shell command that prints the password to stdout. Without one, see [Passwords](#passwords). |
| `imap_encryption` | no | `tls` (default; TLS from the first byte, port 993) or `starttls` (a plain connection, usually port 143, upgraded before the login). |
| `imap_auth` | no | `password` (default) or `xoauth2`, where the password command prints an OAuth 2.0 access token; see [OAuth 2.0](#oauth-20). |
| `from` | yes | Address used for outgoing mail, e.g. `Name <user@example.com>`. |
| `smtp_server` | no | SMTP hostname (defaults to `imap_server`). |
| `smtp_port` | no | SMTP port (defaults to `587`, STARTTLS). |
| `smtp_encryption` | no | `starttls` (port 587) or `tls` (TLS from the first byte, port 465). Unset, the port decides: `465` means `tls`, anything else `starttls`. |
| `smtp_auth` | no | `password` (default) or `xoauth2`, like `imap_auth`. |
| `smtp_username` | no | SMTP username (defaults to `imap_username`). |
| `smtp_password_command` | no | SMTP password command (defaults to `imap_password_command`; without either, the IMAP password is used for the same login, and another login is asked for). |
| `signature_file` | no | Path to the signature of the account's own address. The `-- ` line that marks it off is added unless the file starts with one. |
| `identities` | no | Further addresses the account sends as (see [Identities](#identities)). |
| `drafts_folder` | no | Drafts folder name (default `Drafts`, or the folder the server marks `\Drafts`; see [Folders](#folders)). |
| `sent_folder` | no | Sent folder name (default `Sent`, or the server's `\Sent`). |
| `trash_folder` | no | Trash folder name (default `Trash`, or the server's `\Trash`). |
| `archive_folder` | no | Where `archive` files a message (default `Archive`, or the server's `\Archive`). |
| `junk_folder` | no | Where `spam` files a message and `not-spam` takes it from (default `Junk`, or the server's `\Junk`). |
| `default_sort` | no | Default sort order (see below; default `date_newest`). |
| `folder_order` | no | Ordered list of folder names to pin to the top of the list. |
| `folders` | no | Per-folder overrides, keyed by the server-side folder name (see below). |
| `sync_interval_seconds` | no | How often every synced folder of the account is synced whether or not the server said anything (default `300`). |
| `threads` | no | Open the account's folders as conversations (default `false`); a folder's own `threads` overrides it. See [Conversations](#conversations). |

Sort orders: `date_newest`, `date_oldest`, `subject_asc`, `subject_desc`,
`sender_asc`, `sender_desc`. A folder opens in its own `sort`, else in the
account's `default_sort`; the toolbar's sort menu changes the order for as
long as the folder stays open.

Per-folder overrides go under the account's `folders`, keyed by the folder's
server-side name:

```yaml
accounts:
  - name: Personal
    # ...
    folders:
      INBOX:
        visible: true          # show in sidebar (default: true)
        sync: true             # sync from IMAP (default: true)
        show_unread: true      # show unread count badge (default: true)
        show_total: false      # show total count badge (default: false)
        display_name: Inbox    # custom display name (optional)
        sort: date_newest      # override sort order for this folder (optional)
      Mailing Lists:
        show_unread: false
      Work:
        watch: true            # watch with IMAP IDLE too (default: false; INBOX always is)
        threads: true          # open as conversations (default: the account's `threads`)
```

A server allows only a few IDLE connections per login, so at most three folders
per account are watched: INBOX and the first two marked `watch`.

### Identities

An account's `from` and `signature_file` are its default identity, named
like the account. Further identities are listed under the account:

```yaml
accounts:
  - name: Work
    from: Sam Example <sam@example.com>
    signature_file: ~/.config/orb/signatures/work.txt
    identities:
      - name: Support
        from: Example Support <support@example.com>
        signature_file: ~/.config/orb/signatures/support.txt
        reply_to: support@example.com            # optional
        addresses:                               # what selects it, besides `from`
          - help@example.com
        domains:                                 # recipients it writes to
          - customer.example
      - name: Club
        from: Sam <sam@club.example>             # forwarded here, sent there
        smtp_server: smtp.club.example
        smtp_username: sam@club.example
        smtp_password_command: pass club
      - name: Short
        from: Sam <sam@example.com>
        signature: |                             # inline, instead of a file
          Sam
```

| Key | Required | Description |
|-----|----------|-------------|
| `name` | yes | Shown in the compose window. Unique within the account, and not the account's own name. |
| `from` | yes | The `From` of mail written as this identity. |
| `signature_file` | no | Path to its signature. |
| `signature` | no | Its signature inline; use this or `signature_file`, not both. |
| `reply_to` | no | The `Reply-To` of mail written as this identity. |
| `addresses` | no | Further addresses that select this identity. |
| `domains` | no | Recipient domains written to as this identity (see [Checks before sending](#checks-before-sending)). An account's own `domains` key does the same for its default identity. |
| `smtp_server` | no | An SMTP server of the identity's own, which mail written as it is sent over. |
| `smtp_username` | with `smtp_server` | Its login. |
| `smtp_port`, `smtp_encryption`, `smtp_auth` | no | As for the account, and with the same defaults. |
| `smtp_password_command` | no | Prints its password. Without one it is looked up in the Keychain or asked for, never taken from the account. |

A reply or forward is written as the identity the original was sent from,
or else as the first identity it was addressed to, looking at its `To`, then
its `Cc`, then its `Delivered-To` (which is where a list or a Bcc shows the
address); otherwise as the default identity. A new message starts as the default identity. Reply-all leaves out
every address of every identity.

An identity changes `From`, `Reply-To` and the signature, nothing else: the
message is sent over the account's SMTP login, filed in the account's Sent
folder, and its Message-ID carries the domain of the account's `from`.
An identity with an `smtp_server` of its own is the exception: it is sent
over that server, and its Message-ID carries its own domain, but drafts and
the Sent copy still go to the account. That is how to send as an address that
has no mailbox of its own here — forward its mail to the account, and replies
to it are written as it by the rule above. A message queued while offline
goes out over the same server.

The compose window's From line shows the identity's name and address, with
the account at its end. `⌘I` (or a click on the address) lists every identity
of every account; type a few letters of the address or the account to narrow
it, and `Enter` writes as the one chosen. The signature in the body is
replaced with the new identity's. An identity of another account moves the
draft as well: it is saved to that account's Drafts folder, the copy in the
old one is deleted, and the message is sent over that account. A signature that was edited,
or that some other identity wrote, is left as it is rather than joined by a
second one; a body without any gets the new signature where a fresh draft
would have it. A reopened draft comes back as the identity it was saved as.

### Checks before sending

Before a message goes, the compose window checks it. Each finding is shown as a
**Check** line under the header while you write, and pressing Send asks
whether to send anyway (**Send**) or go back to the draft. What is checked:

- A reply or forward written as an identity the original was not sent to,
  while it was sent to another one: say `support@example.com`, but the reply
  goes from `sam@example.com`.
- A reply or forward of mail delivered (`Delivered-To`) to an address that
  no identity owns, such as an alias or a catch-all. The reply then goes out
  from the default identity. Add the address to an identity's `addresses` to
  have its replies written as that identity.
- A recipient in the `domains` of another identity than the one written as.
  A domain covers its subdomains as well.
- A placeholder of a template still written in braces (see
  [Templates](#templates)). This one asks the other way round: `Enter` goes
  back to fill it in, and sending anyway takes `⌥⌘Enter` or a click.
- A mention of an attachment in your own text (without the quote, the
  signature and a forwarded message) when nothing is attached. The words are
  set by `compose.attachment_words`. Each one matches at the start of a word,
  so `attach` also finds `attached`:

```yaml
compose:
  # The default: English, German, French and Italian. [] turns the check off.
  attachment_words: [attach, enclos, anbei, anhang, anhäng, angehängt,
    beigefügt, beiliegend, ci-joint, pièce jointe, pièces jointes, allegat]
```

### Global settings

```yaml
db_path: ~/.local/share/orb/mail.db   # required
# Optional log. Warnings, every connection Orb opens (with the reason), and
# one line per phase of every folder sync: elapsed time, bytes on the wire,
# message count and the connection used. `sync-stats` shows the same for the
# last sync of each folder without opening the file.
log_file: ~/.local/share/orb/orb.log   # rotated at 10 MB; the previous one is kept as .1

# Sidebar layout: multi_account (default) or single_account. Parsed but not
# used by the app, which always shows every account.
sidebar_mode: multi_account

# Which rendering a message opens in when it carries both an HTML and a
# plain-text version: html (default) or plain. The toggle in the reader still
# switches between them for the message on screen; this is where it starts. A
# message with only one rendering shows that one either way.
default_message_view: html

# How the windows look.
appearance:
  theme: system          # follow the system (default), or dark, or light
  font_size: 13          # pixels; ⌘+ and ⌘- change it on this machine, ⌘0 comes back
  editor_font_size: 15   # Neovim in the compose window; unset follows font_size
  dark_html_mail: true   # draw HTML mail dark in a dark window; toggle-dark-mail switches
  look: hybrid           # hybrid (default), or terminal; see "The window"

# Directory holding the embedding model used by `~` semantic search. Unset
# means semantic search is off; exact search is unaffected.
embedding_model: ~/.config/orb/model

# How a reply quotes the message it answers.
reply:
  quote: true                                  # include the original at all
  prefix: "> "                                 # what each quoted line begins with
  attribution: "On {date}, {sender} wrote:"    # empty for no attribution line
  signature: below                             # or `above`: before the quote

# Senders whose HTML mail may load its remote images without asking: full
# addresses, or domains (with or without a leading @) that stand for every
# address at them and at their subdomains. Everyone else's remote images stay
# blocked until asked for per message. Stylesheets and fonts are never loaded.
load_remote_images_from:
  - newsletter@example.com
  - shop.example

# Messages whose remote images load without asking, for what a sender alone
# cannot say: the same conditions as a highlight rule's `when`, which must
# name at least one. `name` is optional, `accounts` defaults to every account.
load_remote_images:
  - name: Shop newsletter
    when:
      list_id: "*.shop.example"
  - accounts: [Work]
    when:
      from: "*@reports.example.com"
      subject: "Weekly report*"

# Seconds a message must stay on screen before it is marked read.
# The reader follows the cursor, so this is what separates reading a
# message from passing over it on the way somewhere else. Moving the
# cursor cancels it. Set to 0 to never mark automatically — the
# `stage-read` binding still works either way.
mark_read_after_seconds: 2.0

# What the Dock icon counts: unread mail in a folder (default INBOX) of one
# account, or of every account without `account`. Up to four badges, one per
# corner — top right, top left, bottom right, bottom left — each in its
# `color` (#rrggbb, red by default). Without it, one badge counts the unread
# mail in every inbox.
dock_badges:
  - account: Private
  - account: Work
    color: "#0a84ff"

# Where Ctrl+H/J/K/L go once Orb has nothing to move to that way: run with
# left, right, up or down, typically to focus the window of the application
# that lies there. A path, not a shell line; `~` is expanded.
navigation:
  escalate_command: ~/bin/navigate

# How the message list is laid out.
list:
  columns: [meta, tasks, from, subject, date]  # which are shown; `to` for recipients
  date_format: auto                            # or strftime-style; default theme.date_format
  density: comfortable                         # or compact, or spacious

# How search results are shown.
search:
  sort: date_newest       # relevance (default), date_newest, date_oldest, subject_asc,
                          # subject_desc, sender_asc or sender_desc
```

A folder can have its own `columns` — `to` instead of `from` for Sent, say —
under its entry in the account's `folders`. The date format `auto`, the
default, shows a date as fully as the column has room for: the time today,
`Yesterday` and the weekday this week, the day and month this year, and the
year with every other date — `14:22`, `Mon 09:10`, `4 Nov 14:22`,
`4 Nov 2025`. A narrow column drops the time first and never the year. A
strftime format takes `%Y %y %m %d %e %H %I %p %M %S %b %B %a %A %j %F %R
%T`, with `-` after the `%` for no padding; `theme.date_format` is honoured
as the default.

`search.sort` is the order a search's mail starts in until the toolbar or
`sort-by` picks another: by relevance, or as a folder sorts. A search by
meaning (`~`) is always closest first. The density sets
the row height: `compact` fits more rows, `spacious` gives each more room —
20, 24 or 30 px in the Hybrid look, 20, 24 or 28 in the Terminal look, at
the default text size.

Every message is one line, laid out the same whatever order `columns` names
them in: a status column (the unread dot, ✕ for a staged delete, or an arrow
once you have answered or forwarded the message — `r` or `f` in the Terminal
look), the
sender, the recipients, the subject with the opening of the message's text
after it, the badges, the date. `meta` shows the status column and the
badges — the folder of a search hit, highlight icons and labels, where a
staged move goes, the attachment clip and the flag — and `tasks` the badge of
the message's open tasks. The sender shows the name with the address beside
it, and the address is never cut short: when room runs out, the name gives
way, and a sender without a name is shown by the address once. A sender
whose name is itself another address — `"service@paypal.com"
<security@paypa1-support.ru>` — is how a forged message passes for a known
one, so its real address is drawn in red behind a warning mark, in the list
and in the reader. A row grows
with the text size. There is no header row: the order is the toolbar's. A list with more
below fades out at the bottom.

With `navigation.escalate_command` set, a Ctrl+H/J/K/L that Orb has no use
for where it was pressed is handed to that command — in the mailbox, one
pressed toward the window's edge from the last pane that way (or one the
active table does not bind); in the compose window, Ctrl-H and Ctrl-L outside
Neovim, and Ctrl-J and Ctrl-K past the bottom or top field. Neovim's own
mappings see Ctrl-H and Ctrl-L first. This is how a window manager that walks
between applications with those keys carries on through Orb, provided it lets
them reach Orb in the first place. The command runs as Orb's child, so macOS
asks it of Orb's Accessibility permission; Orb asks for it at launch while the
command is set. Started from a terminal, Orb drops `TMUX`, `TMUX_PANE` and
`HYTERM_SOCK` from its environment, so neither the command nor Neovim takes
itself to be in that terminal's pane.

### Filter rules (optional)

Rules run on mail as it arrives in a synced folder. They are a convenience
beside server-side rules (Sieve), not a replacement: a rule only runs while
an Orb is running, and it runs on the first Orb that syncs the message.

```yaml
filters:
  - name: Newsletters
    accounts: [Work]             # default: every account
    folders: [INBOX]             # default: INBOX
    when:                        # every condition must hold; use two rules for OR
      from: "*@news.example.com" # glob, case-insensitive, matched on the address
      subject: "re:^\\[news\\]"  # `re:` selects a regular expression
      list_id: "*"               # any List-Id header
      header:                    # any header, by name
        X-Spam-Flag: "YES"
      has_attachment: true
    then:
      move: Newsletters          # a folder of the same account
      reading: false             # to the reading list, from any account
      read: true
      flag: false                # false is the same as leaving the key out
      delete: false              # to Trash, never for good
      task: false                # create a task linked to the message
      stop: true                 # no later rule sees this message
```

**Conditions.** A rule without `when` matches every message. `from`, `to`
and `cc` match the address of any entry in the field; with a `name:` prefix
(`from: "name:Anna *"`) they match the display name instead. `subject`,
`list_id` and each `header` match the decoded header text, and `list_id`
also matches the identifier alone (`list.example.com` out of
`The List <list.example.com>`). A header the message does not have matches
nothing, not even `*`; a message without a subject has the empty subject, so
`subject: "*"` does match it. A glob matches the whole value and ignores case; `*`
is any run of characters and `?` one. A pattern after `re:` is a regular
expression, found anywhere in the value and case-sensitive unless it says
`(?i)`. Quote a value YAML would read as something else — `YES`, `no`, `1.0`.

**Actions.** They happen in this order: `read` and `flag`, then `move`,
`reading` or `delete` (the moved message keeps its flags), then `task`. A rule
has at most one of the three. `move` names a
folder of the account; a folder the server keeps under `INBOX.` or `INBOX/`
is found there. `reading` moves the message to the [reading
list](#reading-optional), from any account: from another account than the
reading list's, it is copied there and then deleted, as a move between
accounts is. `delete` moves to the account's Trash folder and does nothing
inside Trash. `task` creates a task titled with the message's subject and
linked to it, where the [tasks](#tasks-optional) are configured — unless a
task is linked to the message already. A message without a `Message-ID` gets
no task, since nothing could tell its task from a second one. A message goes through the rules in
order; after a rule with `stop`, or after a move or delete, no later rule
sees it.

**When rules run.** After every sync of a folder, on the messages that arrived
since the last sync. Mail that was in a folder before Orb first synced it is
never filtered, nor is mail that was there before an upgrade to a version with
rules. A rule runs once per message, however often the folder is synced, and
a message that comes back to the folder — moved back by hand, or by an undo —
is left where it was put. Two Orbs sharing a database never run the rules on
the same mail. Two Orbs on separate machines converge, since every action can
be done twice: a message the other Orb filed first leaves this one's list at
once, unannounced, and the task is made by the Orb whose move the server
confirms took the message. A rule with `task` and no `move` has no such
tie-breaker, so two Orbs that sync the same moment can each make a task.

A new rule does not touch old mail unless asked: `:filter` runs the rules on
the selection, or on the whole folder when nothing is selected — in Today, on
every inbox it shows — and
`:filter --all` on every folder a rule watches. Both act on mail the rules
have already seen, so a rule written today can be run on yesterday's mail.

**What they did.** Every message a rule acted on is a line in the log naming
the rule and the actions, and the review panel (`p`, `review-pending`) lists
what the rules did in each account's last sync that did anything, with what
failed. A rule that does not parse is left out and named in the status line at
launch, with the key that is wrong; so is a `move` to a folder the account does
not have, and a `task` without a `tasks` section. A rule with the same name as
another runs, but the log cannot tell them apart.

### Highlight rules (optional)

A highlight changes how a message looks without touching it on the server:
a row for the boss, a colour for anything from the bank, a chip on every
invoice. It is evaluated when the row is drawn, so it applies at once to
every message, old and new, and a rule changed in the config shows after the
reload with nothing to re-run.

```yaml
highlights:
  - name: Boss
    accounts: [Work]                  # default: every account
    when:                             # the same vocabulary as a filter's `when`
      from: anna@example.com
    style:
      color: "#e6b422"                # text colour of sender and subject
      icon: fa-crown                  # any Font Awesome icon the app bundles

  - name: Invoices
    when:
      any:                            # OR, for the rule that needs it
        - subject: "re:(?i)invoice|rechnung"
        - has_attachment: true
          from: "*@billing.example.com"
      body: payment due               # the body contains this, once fetched
    style:
      background: rgba(76, 141, 255, 0.10)
      label: invoice                  # a small chip at the end of the subject
      label_color: "#4c8dff"
      bold: true

  - name: Newsletters
    when:
      list_id: "*"
    style:
      dim: true                       # the row recedes instead of standing out
```

**Conditions.** Everything a [filter](#filter-rules-optional)'s `when`
accepts, plus `any:` — a list of condition sets of which at least one must
hold, besides everything else in the `when` — `unread`, `flagged`, `folder`
(a glob or `re:` on the folder's name), `older_than` and `newer_than` (a
number and a unit: `30m`, `12h`, `7d`, `2w`, against the message's date) and
`body`, text the message's body contains, ignoring case. A `body` condition
is false until the body has been fetched, so a message can light up a moment
after it appears. `list_id` is answered for every message: the sync fetches
`List-Id` beside the envelope. A header named with `header:` is fetched the
same way from the moment a rule names it. For the mail already here, the
background backfill fetches the headers again once, newest first, the first
time any rule or view names a header; until it reaches a message, the
message matches only where its full source is stored.

**Appearance.** `color` (sender and subject), `background` (at low alpha, so
the cursor and the selection still read over it), `icon` and `icon_color`
and `label` and `label_color` (an icon and a chip among the row's badges,
before the date), `bold`, `italic`,
`dim`, and `sidebar_badge: true` to count the rule's unread messages next to
each folder, in the rule's colour. A colour is `#rgb`, `#rrggbb`,
`#rrggbbaa`, `rgb()`, `rgba()` or one of the theme's named colours `accent`,
`warning`, `danger`, `success` and `muted`. A background that does not
contrast enough with the text is darkened or lightened until it does, and
the status line says so at launch. An icon is a Font Awesome name, with or
without `fa-`; a name the bundled set does not have is a warning rather than
an empty box.

Where several rules match a message, the first sets the colour and the
background and every rule contributes its icon and its label, in order, up to
three of each. Where the badges do not all fit, as in Today's columns, a
later rule's label gives way first, down to a dot of its colour. On the
cursor's row the colour is lightened just enough to read on the cursor's
fill, so a red rule stays a red tint there rather than turning white.
The reader repeats the icons and the chips beside the subject,
and search results and Today draw highlights as the folders do, so a message
never looks different in two places.

`:highlights` lists the rules and how many messages of the current folder
each one matches, which is the quickest way to debug a glob. A rule that does
not parse is left out and named in the status line at launch, with the key
that is wrong.

### Subject rewriting (optional)

A subject rewriting rule changes how a subject reads in the message list,
without touching the message: it shortens the subjects a tool sends, or puts
an icon in front of them. The stored subject stays as it came, so search,
sorting and a reply's `Re:` all go by the original, and the reader shows the
original in full, with the rules' icons beside it.

```yaml
subject_munges:
  - pattern: '^\[JIRA\] \((\w+-\d+)\) '   # a regular expression
    replacement: '$1 '                  # $1, $2 … are the capture groups
    icon: fa-bug                        # drawn before the subject
    icon_color: "#4c8dff"

  - pattern: '^(Re|Fwd): (Re|Fwd): '
    replacement: '$1: '

  - pattern: '^Build failed'           # no replacement: an icon only
    icon: nf-fa-circle_xmark
    icon_color: danger
```

The rules run in order, each on what the one before it left, and every rule
that matches contributes its icon, once. Single quotes keep a backslash a
backslash. `replacement` may be left out, which keeps the subject as it is.
An icon is a Font Awesome name, with or without `fa-`, brand icons such as
`fa-github` included; a Nerd Font name of
the Font Awesome set (`nf-fa-…`) is read as the same icon, and the other
Nerd Font sets are not available. A name the bundled set does not have is
left out rather than drawn as an empty box. `icon_color` takes the same
colours a [highlight](#highlight-rules-optional) does. A rule whose pattern
does not compile is left out, and a colour that does not parse leaves the
icon in the default colour; both are warnings in the log.

### Message links (optional)

A message link rule turns a message into a web address: the issue a
tracker's notification is about, the build a CI mail reports. `gx`
(`open-link`) opens the address of the first rule that matches the message
under the cursor, in the browser. On a task it opens what it would open on
the task's first linked message — a task made from a tracker's mail opens
the issue — else a web address written on the first line of its notes, else
one in its title, else what a rule finds in its title.

```yaml
message_links:
  - name: issue                         # optional; `open-link issue` picks this rule
    pattern: 'Issue #(\d+)'             # a regular expression
    url: 'https://tracker.example.com/go/$1'   # $1, $2 … are the capture groups
  - name: ci-run
    match: body                         # subject (the default) or body
    from: '*@ci.example.com'            # optional; only mail from this sender
    pattern: 'https://ci\.example\.com/\S+/runs/\d+'   # no url: the match itself

bindings:
  index_normal:
    i: open-link                        # on a key of your own, besides gx
```

A rule matches the subject as it arrived, so a [subject
rewriting](#subject-rewriting-optional) rule cannot hide what it looks for.
With `match: body` it reads the plain-text body instead, or the HTML one
when there is no plain text; a message whose body has not been downloaded
yet does not match. `from` is an address pattern like a [filter
rule](#filter-rules-optional)'s: a glob over the address, `name:` for the
display name, `re:` for a regular expression. Without `url` the rule opens
what the pattern matched. A rule on a task's title has no sender and no
body, so only one without `from` and `match: body` applies there.
Only `http`, `https` and `mailto` addresses are opened. A message no rule
matches says so in the status line.

### Tasks (optional)

Tasks are off until one account is designated to hold them:

```yaml
tasks:
  account: Work      # the entry in accounts whose IMAP folder holds the tasks
  folder: Tasks      # server-side name, used verbatim — no prefix is assumed
  locale: en         # date-picker language: en or de
  keep_completed_in_today: true   # completed tasks stay in Today until midnight
  completed_first_in_today: false # list them above the open tasks instead
  archive_after_days: 30           # completed longer ago moves to Archived; 0 never
  export:                          # optional: snapshots of every task
    directory: ~/Backups/orb-tasks
    interval_hours: 24
    keep: 30
```

The folder is created on first sync if it does not exist, and is resolved
against the server's namespace, so a server that keeps everything under `INBOX`
gets `INBOX.Tasks` without the config having to say so. Do not write a leading
`.` — that is a Courier/Maildir convention, and on a Dovecot server it creates a
folder literally called `.Tasks` next to the real one.

Zimbra and other groupware servers keep a built-in `Tasks` folder that they do
not offer over IMAP. Orb cannot open it and cannot create another folder with
its name, and says so; pick another name, such as `folder: Orb Tasks`.

The designated account stays an ordinary mail account; only that one folder
leaves the mail world. It is not listed in the sidebar, not a move target, not
synced as mail, not indexed for search, never counted in an unread badge and
never a source of a new-mail notification.

`locale` selects the month and weekday names the date picker reads (`morgen`,
`Dienstag`, `Sept`) and the order it reads numeric dates in. It is a setting
rather than a guess from `LANG`, because a given string has to mean the same day
every time Orb is launched.

### Reading (optional)

There is no reading list until one account is named to hold it:

```yaml
reading:
  account: Private                 # the entry in accounts that holds the reading list
  folder: Reading                  # what is still to read; default Reading
  archive_folder: Reading Archive  # what has been read; default Reading Archive
  feeds:                           # RSS, Atom and JSON feeds; optional
    - url: https://this-week-in-rust.org/rss.xml
    - url: https://example.com/feed.xml
      name: Example                # the sender its items show; default the feed's title
  poll_minutes: 60                 # how often each feed is asked; default 60, at least 5
```

Both folders are created on the first sync if they do not exist. Their names
are resolved against the server's namespace in the same way as the tasks
folder's. The archive sits beside the reading folder, not inside it. Unlike
the tasks folder, both are ordinary mail folders: they are synced, and search
finds what is in them. They are shown under Reading in the sidebar rather than
among the account's folders. They are not offered as targets in the move
pickers, since `b` is how mail gets into the reading list.

A newsletter rule with `reading: true` sends mail to the reading list as it
arrives. Match on `list_id`, which almost every newsletter sets (see
[Filter rules](#filter-rules-optional)):

```yaml
filters:
  - name: Newsletters
    accounts: [Work, Private]
    when: { list_id: "*.substack.com" }
    then: { reading: true }
```

Each feed's new items arrive in the reading folder as unread messages, sent
by the feed. The title is the subject, and the body is the item's content, or
its summary when the feed carries no content. A feed polled for the first time
brings in its ten newest items, not its whole history. Every Mac polls, and
an item is still added only once: before adding it, Orb looks for it in both
reading folders. An item you archive or delete is not brought back on the Mac
where you did it. It can come back if another Mac polls the feed for the
first time later. A feed that cannot be fetched or read is shown in Feed Status.

### Chat (optional)

Chat is off until a Rocket.Chat server is named:

```yaml
chat:
  server: https://chat.example.com   # https:// and the host, no path
  sign_in: sso                       # or token_command
  # token_command: …                 # with token_command: prints a user id and a token
  waiting_days: 14                   # how far back the first sync lets threads wait
  rooms:                             # per room, by its name in the server's addresses
    general: { waiting: false }      # never in Waiting; still read and counted
    support: { notify: false }       # waits, without a notification
  reactions: [":+1:", ":heart:", ":joy:", ":tada:", ":eyes:"]   # offered on a message, in this order
  links:                             # text in a message that becomes a link
    - { pattern: '#(\d+)', url: 'https://tracker.example.com/issues/$1' }
  badge: false                       # true: the Dock badge counts Waiting too
```

With `sign_in: sso`, the default, Orb opens the server's own sign-in page in a
window of its own the first time (Go ▸ Sign In to Chat…, or the button in an
empty Waiting), and keeps the session it hands the web client in the Keychain.
That works with single sign-on (OAuth), which the server's API does not take
directly. The session lasts as long as the server lets it; once it ends,
Waiting says so and offers the sign-in again. With `sign_in: token_command`,
the command prints the user id and a personal access token, separated by
whitespace, for a server that allows those.

`waiting_days` bounds the first sync of a server: older threads never start
waiting, so a busy server does not begin with every thread it ever had.

`rooms` takes `waiting: false` for a room whose threads never call for an
answer, and `notify: false` for one whose threads wait without a
notification. `reactions` are the emoji a message offers to react with, as
Rocket.Chat shortcodes; the default is the list above. `links` turns text in
a message into a link: `pattern` is a JavaScript regular expression, and `url`
the address a match opens, with `$1` to `$9` for the pattern's groups and `$0`
for the whole match — above, `#4711` opens the tracker's issue 4711. Text
already in a link or in code is left as it is; a pattern that is not a regular
expression is named in the status line and skipped. Room settings and links take
effect when the file is saved; `server`, `sign_in`
and `token_command` need a restart.

#### Muting a room

A room can be muted for a while from Orb, without touching its settings on the
server: `zq`, a right-click on the room in the sidebar or on one of its
threads, or Go ▸ Mute Chat Room…. A mute lasts 1 hour, 4 hours, until
midnight, until a time of day, or until you unmute it.

While a room is muted its threads still wait, but they stay out of Waiting,
out of Today, out of the notifications and out of the Dock badge. The room's
own row in the sidebar still counts them, in grey, beside a struck-out bell,
and in the room itself they keep their mark, dimmed. The conversation's header
says how long the mute lasts; a click on it unmutes. A thread in which someone
names you, or calls the whole room with `@here` or `@all`, comes through as
if the room were not muted, notification and all.

When a mute ends, the threads that waited under it come back into Waiting
without a notification. A mute travels to your other Macs through the tasks
account, as a thread marked done does. Muting and unmuting are undone with
`⌘Z`.

### Recipients (optional)

Addressing a message completes as you type, from two address books that are
ranked together: everyone the mail already knows about, and the system address
book. Down opens the list without typing — on an empty field it offers the
people you write to most.

The first is built from the messages themselves — nothing to import and nothing
to maintain. Someone you have written to outranks someone who merely appeared
in a header, and someone you wrote to last week outranks someone you wrote to
in 2019, which is what makes the first suggestion usually the right one. It is
built once on first run (a few seconds on a large mailbox, on a background
thread) and updated with new mail after that.

macOS Contacts is asked for the first time completions are needed, which is
when macOS shows its permission dialogue. Refusing it costs nothing but those
entries. In a development run there is no bundle to carry the usage
description, so the system address book is skipped entirely.

Shortcuts of your own outrank both, and a group inserts everyone in it at once:

```yaml
contacts:
  auto_extract: true   # learn from the mail (default). false to use only the
                       # entries below and the system address book.
  aliases:
    - shortcut: ns
      email: nina.berger@example.com
      name: Nina Berger
  groups:
    - shortcut: team
      emails: [ada@example.com, grace@example.com]
      description: Development Team
```

### Views and saved searches (optional)

A virtual folder is a view in the sidebar, under **Views** between the task
views and the accounts: a merged view, or a saved search. Opening one runs
its query and shows the hits as a search does — the newest five hundred, as
a search does — and `q` leaves it.

```yaml
virtual_folders:
  - name: All Inboxes
    source: all_inboxes   # every account's INBOX
  - name: Unread
    source: all_unread    # `is:unread` everywhere
    icon: fa-envelope     # a Font Awesome icon, or an emoji; the search glass otherwise
  - name: Work Mail
    source: custom        # these folders, merged
    folders:
      - { account: Work, folder: INBOX }
  - name: Invoices
    source: search        # a saved search, in the search language
    query: s:invoice OR b:rechnung has:attachment
```

A saved search can ask about a header, which no search can otherwise reach:

```yaml
  - name: My Issues
    icon: fa-user-pen
    source: search
    query: in:Archive f:tracker@example.com header:X-Assigned-To-Me=true
```

The sync fetches a header a view names from the moment the view names it,
and the background backfill fetches it for the mail already here, newest
first. A view updates while it is open, so a view of a folder marked `watch`
shows new mail as it arrives.

### Pinned (optional)

The top of the sidebar holds what `pinned` lists, in its order and under no
heading: the places you go to most, each a key away. `Ctrl-1` opens the first,
`Ctrl-2` the second, up to `Ctrl-9` (`open-pinned <n>`), from a text field
too, such as the composer or the search; while Ctrl is held on its own, each
pin shows the digit that opens it over its icon.

```yaml
pinned:
  - today                                         # a task view
  - { account: Work, folder: INBOX, display_name: Work }
  - { account: Private, folder: INBOX, display_name: Private }
  - waiting                                       # chat's Waiting
  - { view: All Inboxes }                         # one of virtual_folders, by name
```

A bare name pins a built-in view: `today`, `upcoming`, `someday` and
`completed` of the tasks, `reading` (the reading list) and `must-read`, and
`waiting`. A pinned view leaves its section, so it is in the sidebar once; a
pinned folder stays in its account as well, and is named by `display_name`
(`Account: Folder` without one). Without `pinned`, Today alone is pinned; a
list that leaves Today out puts it first under **Tasks**, and `pinned: []`
pins nothing. A pin of something the config does not have — an account, a
view, a section — is left out with a warning at launch. `pinned_folders`, the
older name of the key, still reads.

`gp` or `⇧⌘P` (`toggle-pin`) pins what is open — or, in the sidebar, what
the cursor is on — below the pins `pinned` lists, and unpins it again: the
chat you are in the middle of, say, a `Ctrl` key away while you work on
something else. Such a pin numbers on from the config's, in the order pinned,
and is remembered on this Mac rather than written to `config.yml`. Drag it
up or down among the others pinned this way to change that order, and with
it its `Ctrl` key; the config's pins stay first, in the config's order. A folder
pinned this way leaves its account's tree, as a view leaves its section, and
is named `Account: Folder`; a room leaves Direct or Channels. What `pinned`
lists stays pinned until the config says otherwise, and a pin of something
gone — a room left, a folder deleted — is not drawn until it is back.

### Passwords

A password comes from a command that prints it to stdout, which works with
any password manager:

```yaml
# pass (password-store)
imap_password_command: pass email/gmail

# macOS Keychain
imap_password_command: security find-generic-password -a user@gmail.com -s imap.gmail.com -w

# 1Password CLI
imap_password_command: op item get 'Gmail' --fields password

# GPG-encrypted file
imap_password_command: gpg --quiet --decrypt ~/.email-password.gpg
```

Without a command, Orb looks in your login Keychain for an item it filed
earlier and otherwise asks: a prompt opens in the window when the account
first connects, and in the compose window at the first send when the SMTP
login differs from the IMAP one. The prompt offers to keep the answer in the
Keychain, under the service `Orb` and the login `username@server`, so the
next launch does not ask. A password the server refuses is forgotten and
asked for again, with the prompt saying so; a dismissed prompt fails that
login until the next sync asks again.

### OAuth 2.0

Gmail and Office 365 take an OAuth 2.0 access token in the password's
place, over SASL XOAUTH2. With `imap_auth: xoauth2` (and `smtp_auth: xoauth2`
for sending) the password command prints that token rather than a password,
and the account's `imap_username` is the address the token was issued for:

```yaml
imap_server: imap.gmail.com
imap_port: 993
imap_username: me@gmail.com
imap_auth: xoauth2
imap_password_command: oauth2ms --encode-xoauth2=false   # or mutt_oauth2.py ~/.gmail.tokens
smtp_server: smtp.gmail.com
smtp_port: 465
smtp_auth: xoauth2
```

Getting the token is the command's job: `mutt_oauth2.py`, `oauth2ms`,
`gmail-oauth2-tools` and the like register an app with the provider, open
the browser once, and print a fresh access token from the refresh token they
keep. Orb runs the command when the account first connects, keeps the token
for the session, and runs the command again when a login is refused — which
is what an expired token looks like — so a command that refreshes is enough.

## Keyboard shortcuts

Orb uses vim-style, single-key shortcuts in the message list (no modifier
needed), alongside standard macOS menu shortcuts.

### In-app (message list)

| Key | Action | Command |
|-----|--------|---------|
| `j` / `k`, `↓` / `↑` | Next / previous message | `next-message`, `prev-message` |
| `gg` / `G` | First / last message | `top`, `bottom` |
| `Ctrl-o` | Back to where the last [jump to a message](#jumping-to-a-message) was made from | `jump-back` |
| `Ctrl-d` / `Ctrl-u` | 10 messages down / up | `next-page`, `prev-page` |
| `gt` / `gT` | Next / previous folder | `next-folder`, `prev-folder` |
| `gf`, `⌘K` | Go to anything the sidebar can open — a folder of any account, a view, a task view, the reading list, Waiting, a chat room — found by a few letters of its name (`work arch` finds Work's Archive), even when it is folded away; in the tasks window the task views and tags, in the chat window the rooms. It is the [command palette](#the-command-palette) opened on its places | `go-to` |
| `⇧⌘K` | The [command palette](#the-command-palette): any command by a few letters of its title | `command-palette` |
| `Ctrl-1` … `Ctrl-9` | Open the first … ninth [pin](#pinned-optional) at the top of the sidebar | `open-pinned 1` … `open-pinned 9` |
| `gp`, `⇧⌘P` | [Pin](#pinned-optional) what is open — a folder, a view, a task view, Waiting, a chat room — below the config's pins, or unpin it; in the sidebar, what the cursor is on. `⇧⌘P` works from a text field too, such as the chat composer | `toggle-pin` |
| `Enter`, `l` | Open the message in the reader | `open-message-detail` |
| `h` | Back: from the reader to the list, from the list to the sidebar | `back` |
| `q` | Close the reader, or the panel in it, or the window of a message or task | `close` |
| `u` | Toggle read / unread | `toggle-read` |
| `!` | Toggle flag | `toggle-flag` |
| `d` / `D` | Stage a delete / move to Trash now | `mark-delete`, `delete-immediate` |
| `s` / `S` | Stage a move / move now, to a folder picked from a list | `mark-move`, `move-immediate` |
| `c` | Copy now, to a folder picked from a list; the originals stay | `copy-immediate` |
| `a` | Archive now; in the reading list, to its archive, and back | `archive` |
| `b` | Set aside in the [reading list](#reading) | `read-later` |
| `U` | Unstage | `unmark` |
| `x` / `X` | Apply / discard the staged actions in the folder; in Today, in every inbox it shows | `expunge`, `discard-pending` |
| `p` | Review everything staged | `review-pending` |
| `Space` | Select / deselect the message and move on | `select-message` |
| `J` / `K`, `Shift-↓` / `Shift-↑` | Extend the selection down / up | `extend-selection-down`, `extend-selection-up` |
| `⌘A` | Select every message in the folder; in the reader, the text of the message or the task, ready for `⌘C` | `select-all` |
| `Esc` | Clear the selection; so does moving the cursor with `j`/`k`, the arrows, paging or `gg`/`G`. With nothing selected, take off the filter, then leave the search results | `clear-selection` |
| `Ctrl-r` | Sync all accounts | `sync` |
| `C` | Compose | `compose` |
| `r` / `R` / `f` | Reply / reply-all / forward | `reply`, `reply-all`, `forward` |
| `t` / `T` | Create a task from the message(s) / show its tasks | `create-task-from-message`, `open-tasks-for-message` |
| `Ctrl-t` | Create a task from the message(s), postponed to a date the picker asks for, or due on it after `Ctrl-e` | `create-task-from-message postpone` |
| `o` | Create a task, in the open task view or in Today, with its title and notes open to write | `create-task` |
| `zt` | Show the folder, or Today's mail, as conversations, or as messages again | `toggle-threads` |
| `zl` | Show Today's mail and tasks one above the other, or side by side | `toggle-today-layout` |
| `zx` | Show Today as one timeline of mail, chat and reading beside the tasks, or go back to the layout it left | `toggle-today-mixed` |
| `zr` | Show the [reading list](#reading) in Today, below the tasks, or put it away | `toggle-today-reading` |
| `zw` | Show [Waiting](#chat) in Today, below the tasks, or put it away | `toggle-today-chat` |
| `zq` | [Mute](#muting-a-room) the chat room under the cursor — the open room, the thread's, or the room in the sidebar — for a while: the picker offers 1 hour, 4 hours, until tomorrow, until you unmute it, or takes `90m`, `2d` or `17:00`; on a muted room it offers Unmute first | `chat-mute-room` |
| `zQ` | List the muted rooms and until when; choosing one unmutes it | `chat-muted-rooms` |
| `zm` | Maximize: the list fills the window, the folders and the reader slide over it as drawers; again, the panes come back (see [Maximizing](#maximizing)) | `toggle-maximize` |
| `zs` | Put the folders away, the list and the reader taking their width; again, they come back. `Ctrl-h` slides them out over the list while they have the keyboard. The button beside the traffic lights does the same, and the choice is kept across restarts | `toggle-sidebar` |
| `za` | Open or close the thread under the cursor; in a [conversation](#conversations) in the reader, the message the keys are on; in the sidebar, fold or unfold the heading, account or folder | `toggle-thread` |
| `zR` / `zM` | Open / close every message of the [conversation](#conversations) in the reader | `open-all-messages`, `close-all-messages` |
| `gc` | Show the whole conversation the message belongs to, from every folder of its account | `open-thread` |
| `gx` | Open a feed item's article, or the web address a [message link](#message-links-optional) rule finds in the subject or the body | `open-link` |
| `Tab` | Move between the two halves of the current view | `focus-next-section` |
| `Ctrl-h` / `Ctrl-j` / `Ctrl-k` / `Ctrl-l` | Move to the pane to the left, below, above or to the right — the folders, the list, the reader, and in Today the mail and the tasks each — by where they sit on screen; between two panes equally near, the one left last. Today's tasks follow its mail — below it when stacked, to its right in columns — and the reader is below both. Past the last pane that way, the key goes to `navigation.escalate_command`. A click anywhere in a pane gives it the keyboard too | `focus-pane left`, `focus-pane down`, `focus-pane up`, `focus-pane right` |
| `/` then text | [Search](#search) mail and tasks, in a field that floats over the window until `Enter` or `Esc` | `search-mode` |
| `gs` | Over search results that found both, show the mail and the tasks, the mail, or the tasks | `search-kind` |
| `F` then text | [Filter](#filter) the list on screen as you type; `Enter` keeps the filter, `Esc` takes it off | `filter-mode`, `clear-filter` |
| `n` / `N` | Next / previous match of the search | `next-match`, `prev-match` |
| `:` | Open the command line, where any command can be typed by name | `command-mode` |
| `?` | Show every key in force over the window, and find one by typing | `help` |
| `⌘Z` / `⇧⌘Z` | [Undo](#undo) the last action, of any kind / redo it | `undo`, `redo` |
| `⌘+` / `⌘-` / `⌘0` | Larger / smaller text, and back to the configured size | `zoom-in`, `zoom-out`, `zoom-reset` |

`q` no longer quits: ⌘Q and the `quit` command do.

### Maximizing

`zm` (`toggle-maximize`) lets the list fill the window — a folder, a task
view, or in Today the half the keyboard is in, `Tab` (or `Ctrl-l` /
`Ctrl-h` in columns) switching to the other — and `zm` again brings the
folders and the reader back. While it is maximized the list keeps its width, and the other
panes come out over it as drawers instead of taking a share of it:

- The reader is out over the right from the start, showing the message or
  task under the cursor while the keyboard stays in the list, so `j` / `k`
  leaf through the messages in it. `q`, or `Esc` with nothing selected, puts
  it away; so does `toggle-detail` pressed in the reader.
- `Ctrl-l` or `Enter` gives the reader the keyboard, and brings it
  out again if it was put away. `Esc` or `Ctrl-h` gives the keyboard back to
  the list and leaves the reader out.
- `Ctrl-h` from the list slides the folders out from the left while they
  have the keyboard; `Ctrl-l` or opening a folder puts them away.
- In Today the reader is below the list instead, in a row of its own as it
  is unmaximized, so it covers none of the list; `Ctrl-j` gives it the
  keyboard and `Ctrl-k` takes it back, and the line between them can be
  dragged.

A message the cursor passes while the reader is away is neither loaded nor
marked read. Maximized from the reader, the reader covers the whole list, and
leaving it puts it away.

The command line completes as you type: above the status line it lists the
commands the name could be, each with what it takes, what it does and its
key, so `:` also teaches the bindings. The letters typed need only appear in
order — `clsl` finds `clear-selection` — and a name that starts with them
comes first. `Tab`, `↓` or `Ctrl-j` fill in the next one, `Shift-Tab`, `↑`
or `Ctrl-k` the one before; once the name is followed by a space, the line
shows what the command takes. On an empty line, `↑` and `↓` go through the
commands run before.

`?` (or Help ▸ Keyboard Shortcuts) lays every key over the window, wherever
the keyboard is — in mail, a task or a chat thread. It opens on **Here**:
every key that works where `?` was pressed, by what it is about. The tabs
beside it show each binding table on its own (Mail, Reader, Tasks, Task
detail, Editing, Chat, Composer, Dates), and **All commands** lists every
command with its keys here, bound or not. Type to search: a key (`d`, `gg`,
`⌃d`) says what it does, and words (`arch`, `move folder`) find the keys for
it, with each tab counting its matches. `↑` and `↓` choose a row, `⏎` runs
it, `Tab` and `Shift-Tab` move between tabs, `Esc` clears the search and
then closes, as does `?` on an empty search. Everything comes from the tables
in force, so a key rebound in `config.yml` is listed as it now is.

A key that starts longer bindings — `g`, `z` — shows what can follow it
when you pause after it: a panel over the bottom right lists each next key
with what it does, for the table in force (chat's keys in chat, the task
keys in a task list). Typed on without a pause, it never appears; the next
key or `Esc` puts it away.

These commands have no default key; bind them in `bindings.index_normal` or
type them after `:`. A key with Command is written `<d-y>` (⌘Y), and with
Shift as well `<d-s-y>` (⇧⌘Y):

| Command | Action |
|---------|--------|
| `quit` | Quit Orb |
| `sync-folder` | Sync the current folder only (`sync-all` is the same as `sync`) |
| `sync-stats` | Show, in the reader, what the last sync of every folder did phase by phase, and how many connections Orb has opened since launch and why |
| `account-status` | Show each account's standing with its server: syncing, ok, login refused, certificate, unreachable, with the last error and when it happened |
| `console` | Open the console in a tab beside the mailbox: the log as it is written, and the commands the app ran lately with how long they took and what went wrong |
| `load-remote-images` | Let the message on screen load its remote images, this once |
| `notification-settings` | Open System Settings at Orb's notifications, to allow them again after they were refused |
| `view html` / `view text` / `view raw` | Show the message on screen as HTML, as plain text, or as its source |
| `toggle-dark-mail` | Draw HTML mail dark, or as it came, for this session (see [The window](#the-window)) |
| `clear-completed` | Hide the tasks completed so far from Today, on every Mac and iPhone, until midnight (see [Tasks](#tasks)) |
| `export-tasks` | Export every task to `tasks.export.directory` now (see [Tasks](#tasks)) |
| `set-up-iphone` | Show the code that sets Orb up on an iPhone, the tasks account's password included (see [Tasks](#tasks)) |
| `import-tasks [path] [--include-purged]` | Import a task snapshot, after saying what it will do (see [Tasks](#tasks)) |
| `import-things [path]` | Import Things 3, after saying what it will bring over (see [Tasks](#tasks)) |
| `link-mail-in-notes` | Link the mail every task's notes name by `message://` link to that task (see [Tasks](#tasks)) |
| `work-on` / `work-on clear` | Mark the task, or the selection, as being worked on, or take the mark off; `clear` takes it off every task (default `w` in a task list) |
| `toggle-look` / `look hybrid` / `look terminal` | Switch every window to the other look, or to the one named, on this machine (see [The window](#the-window)) |
| `stage-read` | Mark read |
| `advance` | Move on from a message just staged: to the next one, or to the previous one when the next is staged already (or there is none) and the previous is not — so working up the list keeps going up. For the end of a chain: `mark-delete && advance` |
| `mark-move <folder>`, `move-immediate <folder>`, `copy-immediate <folder>` | The same as `s`, `S` and `c`, with the folder named instead of picked |
| `move-across`, `copy-across` | Move or copy into a folder of any account, picked from a list of them all; across accounts the messages travel through this machine, whole, with their flags, and a move deletes from the source only what arrived. Undoable, see [Undo](#undo). |
| `spam` / `not-spam` | Move to the account's junk folder now / back to INBOX from it |
| `expunge-all` | Apply the staged actions in every folder of every account |
| `outbox` / `retry-outbox` | Show the [Outbox](#outbox) / send everything in it now |
| `discard-pending all` | Discard the staged actions everywhere |
| `filter`, `filter --all` | Run the [filter rules](#filter-rules-optional) on the selection or the current folder (in Today, its inboxes); with `--all`, on every folder a rule watches |
| `highlights` | List the [highlight rules](#highlight-rules-optional) and how many messages of the current folder each matches |
| `open-attachment [n]`, `quick-look [n]`, `save-attachment [n]` | Open, preview or save attachment `n` (the first by default) of the message on screen |
| `create-folder <name>`, `rename-folder <name>`, `delete-folder` | Make a folder in the open folder's account, rename the open folder, or delete it with its messages (after asking); see [Folders](#folders) |
| `subscribe-folder`, `unsubscribe-folder` | Mark the open folder subscribed, or not, for the clients that list only subscribed folders |
| `toggle-detail` | Toggle between the list and the reader |
| `sort-by date\|from\|subject [asc\|desc]` | Sort the list in the direction named; without one, the field it already has turns the direction round. Search results also take `relevance`, their own order |
| `open-reading list\|must-read\|archive` | Open a view of the [reading list](#reading) |
| `poll-feeds` | Ask every feed of the [reading list](#reading) for new items now |
| `feed-status` | Show each feed: when it was last asked, and what went wrong |
| `today-mail <account>\|all` | Show one account's inbox in Today's mail, or every inbox |
| `toggle-today-layout [stacked\|columns\|mixed]` | Show Today's mail above its tasks, beside them, or as one [timeline](#today-mixed) with the chat and the reading; without an argument, stacked or columns, whichever is not in force |
| `toggle-today-mixed` | Show Today as one timeline beside the tasks, or go back to the layout it left |
| `toggle-today-reading [on\|off]` | Show the reading list in Today, or put it away; without an argument, the other one |
| `toggle-today-mail [on\|off]` | Show the mail in Today, or put it away; without an argument, the other one |
| `toggle-today-tasks [on\|off]` | Show the tasks in Today, or put them away; without an argument, the other one |
| `today-menu` | Open the View menu in Today's toolbar: the arrow keys (or `j`/`k`) walk it, `Enter` flips a switch, `Esc` closes |
| `today-reading must-read\|new` | Show the must-read items in Today's reading section, or the new ones |
| `sort-menu` | Open the toolbar's sort menu: the arrow keys (or `j`/`k`) walk it, `Enter` picks, `Esc` closes |

A sync reports a folder that another sync — in this window or another Orb
process — was holding at the time as *skipped (busy)* rather than as synced;
the core asks for it again a few seconds later. The first sync of a folder is
fetched in pages of five thousand messages and each page is committed as it
arrives, so a large folder fills in while the rest is still coming, and a sync
interrupted halfway resumes from where it stopped.

### The command palette

`⇧⌘K` (Go ▸ Command Palette…) opens one box for everything, as VS Code's
Quick Open does. What the field starts with says what it lists:

| Prefix | Lists | Enter |
|--------|-------|-------|
| nothing | The places the sidebar can open — what `gf` and `⌘K` open it on | Opens it |
| `>` | Every command, by its title: `stage del` finds Stage for Deletion | Runs it |
| `#` | The tags | Opens the tag in the tasks window; elsewhere searches for `tag:` |
| `@` | People from the address book, as typed | Searches the mail from or to them |
| `/` | Mail and tasks the search finds for the words typed | Opens the message or the task |

Typing or deleting the prefix moves between the lists. Every list in the
app that narrows as you type — the palette, the folder and tag pickers, the
templates, the completions of the command line — ranks the same way: a name
that is what you typed first, then one that starts with it, then one with a
word that starts with it, then one that holds it, and last one that holds its
letters in order. So `gsi` puts the tag *GSI* before *Garden Seedling
Inventory*, and `inv` puts *Inventory* before *Seedling Inventory*.

A command's row
says what group it belongs to and the key that does the same, so the
palette also teaches the bindings. With nothing typed, the commands run from
the palette last come first, under "recently used" (kept on this Mac), and
the rest follow by group; while typing, those used recently rank higher. A
toggle is named for the way it would go — Mark as Read on unread mail, Mark
as Unread on read mail — and found by either name.

Only the commands that make sense in the window are listed: no mail
commands in the tasks window, nothing of the mailbox in the chat window, no
task or chat commands without a tasks or chat section in `config.yml`. A
command that cannot act on what the cursor is on — Unstage on a message
with nothing staged, Merge Tasks with one task selected — stays in the list,
greyed out, and `Enter` on it says why rather than running it. A command
acts on what the cursor was on when the palette opened; if a sync moved the
cursor meanwhile, it does nothing and says so. A count typed before `⇧⌘K` is
passed on to the command picked.

A command that needs a word asks for it in the same box, one step after
another: New Folder… its name, Rename Tag… the tag and then its new name,
Today: Show One Account's Mail… the account. `Esc` at any step cancels.
Those that pick their own argument — Stage a Move… its folder, Add Tag… its
tag — open their picker as their key does.

### In-app (task list)

Active when the cursor is in a task list — the Today view's upper half, or any
of the Tasks views. Layered over the message-list bindings, so anything not
named here still does what it does in the list.

| Key | Action | Command |
|-----|--------|---------|
| `o` | New task | `create-task` |
| `Enter`, `l` | Open the task | `open-task-detail` |
| `e` | Edit title / body | `edit-task` |
| `x` / `X` | Complete / un-complete | `complete-task`, `uncomplete-task` |
| `d` / `U` | Move to trash / restore; with steps selected in the detail, remove those steps | `delete-task`, `restore-task` |
| `s` / `D` | Postpone date / due date (opens the picker); the two are independent, and a task shows from the earlier one, so one due today stays in Today however far it is postponed | `postpone-task`, `set-due-date` |
| `S` | Postpone and due date at once (opens the picker) | `set-both-dates` |
| `c` | Clear both dates | `clear-dates` |
| `r` | Add a reminder | `add-reminder` |
| `a` / `A` | Add / remove a tag | `add-tag`, `remove-tag` |
| `m` / `M` | Link a message / open the linked message | `link-message`, `open-linked-message` |
| `gx` | Open what `gx` opens on the first linked message, else the web address on the first line of the notes or in the title, or the one a [message link](#message-links-optional) rule finds in the title; the addresses in the notes can be clicked too | `open-link` |
| `R` | Make the task repeat: a list of the usual rules, or `:set-recurrence every monday`, `monthly on the 1st`, `every 2 weeks until June`, `every day 5 times`, in English or German; `clear-recurrence` stops it, `recurrence-from-completion on\|off` says what the next date counts from | `set-recurrence` |
| `f` | Attach files, picked from a dialog; files dropped on the detail pane are attached too | `attach-file` |
| `+` | Add a step at the end of the checklist, written in place | `add-step` |
| | Merge the selected tasks into one, after asking how (also in their context menu) | `merge-tasks` |
| `E` | Empty the trash, for good, after asking (also a button in the Trash view) | `empty-task-trash` |
| `⌘↑` / `⌘↓` | Move the task up / down the global order; with the mouse, drag a task — or a selected task, which brings the whole selection along as one block even if it was apart — to where it goes | `move-task-up`, `move-task-down` |
| `u` / `Ctrl-y` | [Undo](#undo) / redo the last action, as `⌘Z` / `⇧⌘Z` do | `undo`, `redo` |
| `zg` | List Today, Upcoming and Someday under their tags, or in one list again (see *Grouping by tag*) | `toggle-group-by-tag` |
| `zd` | List Today's tasks due today or earlier above the rest, or not (see *Due first*) | `toggle-due-first` |
| `Space` / `Esc` | Select / clear the selection, the checklist's too; moving the cursor clears it too. With nothing selected, `Esc` takes off the filter, then leaves search results | `select-task`, `clear-task-selection` |
| `J` / `K`, `Shift-↓` / `Shift-↑` | Extend the selection down / up | `extend-selection-down`, `extend-selection-up` |
| `⌘A` | Select every task in the list | `select-all` |
| `Ctrl-r` | Sync tasks now | `sync-tasks` |
| | Say whether the open task has reached the server, with its revision and when it was created and last changed | `task-info` |

Every date and completion command applies to the whole selection, and every one
of them is undoable with `u` afterwards. A click selects a task, `⌘`-click adds
one or takes it out, and Shift-click selects a range. The selection stays on its
tasks after an action, a drag or a sync, so a second key acts on the same
tasks; `Esc` or moving the cursor lets it go. A right-click on a task outside
the selection acts on that task alone. Clicking a task's box completes just
that task, even when it is selected.

Tags are managed by name: `rename-tag <old> <new>`, `recolour-tag <name>
<colour>` (a CSS colour, or `none`) and `delete-tag <name>`, which takes the
tag off every task and leaves the tasks; `a` with no name picks from the
existing tags, so `urgent` and `Urgent` cannot both come to exist; a name
no tag has is offered last as a new tag, even while it finds others.

Everything the keys do is reachable with the pointer too: a right-click on a
task opens a menu of its commands — and on a message a menu of the message's —
in which what does not apply to what was clicked is greyed out (Un-complete
for an open task, Unmark for a message with nothing staged), a toggle
says which way it goes (Mark as Read, Unflag), and each entry shows the key
your bindings give it — at the right, as macOS shows keys, or after the label
for a sequence such as `gx`.

The detail pane shows what is set and nothing else: the title beside its
box, then a chip for each set date, reminder, repeat and tag — a click
changes it, its cross clears it, and a reminder's chip snoozes it (10
minutes, an hour, three hours, tomorrow 9:00) — and one line naming the keys
for what can still be added, each a button too. The notes, the linked mail,
the files and the history follow. Each linked message says whether it was
answered — "Replied 12:45" in green, with the time of your last reply when
it is in a Sent folder, or "No reply" in amber, since an unanswered message
is usually why the task exists. A reminder's notification carries the same snooze
as buttons of its own: an hour, or until tomorrow 9:00.

In the task detail view (`bindings.task_detail`): `h` / `Esc` go back,
`Enter` edits, `j` / `k` scroll the pane. While a title or body is being edited
(`bindings.task_edit`): `Enter` or `Ctrl-s` accepts, `Esc` cancels — `Ctrl-s`
exists because `Enter` in the body has to be a newline.

### In-app (chat)

Active in Waiting, in the list and in the conversation alike (`bindings.chat_normal`,
layered over the message-list bindings).

| Key | Action | Command |
|-----|--------|---------|
| `j` / `k`, `↓` / `↑` | Next / previous thread; the reader follows | `chat-next`, `chat-prev` |
| `gg` / `G` | First / last thread | `chat-first`, `chat-last` |
| `l` | Give the conversation the keyboard | `chat-open` |
| `r`, `i`, `Enter` | Write in the thread: the composer takes the keyboard | `chat-reply` |
| `C` | Start a chat with the people picked (as `gn` everywhere) | `chat-new-message` |
| `a` | Done: out of Waiting until someone writes again, and read in Rocket.Chat | `chat-done` |
| `t` / `Ctrl-t` | Make a task of the thread's newest message, which takes the thread out of Waiting; `Ctrl-t` asks for the date | `chat-create-task`, `chat-create-task postpone` |
| `u` | Mark the thread read, in Orb and in Rocket.Chat — or unread, from its newest message, when it is read | `chat-toggle-read` |
| `za` | Show every reply of a long thread, or fold the earlier ones away again | `chat-toggle-fold` |
| `F` | Filter the threads by who wrote, the room and the text | `filter-mode` |
| `gx` | Open the thread in Rocket.Chat | `chat-open-in-browser` |
| `Ctrl-d` / `Ctrl-u` | Scroll the conversation half a page | `chat-scroll-down`, `chat-scroll-up` |
| `Space` | Select the thread, or unselect it, and move down | `chat-select` |
| `J` / `K`, `Shift-↓` / `Shift-↑` | Extend the selection down / up | `chat-extend-down`, `chat-extend-up` |
| `⌘A` | Select every thread in Waiting | `chat-select-all` |
| `Esc` | Drop the selection | `chat-clear-selection` |
| `+` | React to the newest message somebody else wrote, from the configured reactions | `chat-react` |
| `⌘U` | Attach files to the message being written | `chat-attach` |
| `⌘Z` | [Undo](#undo) the last action, such as done | `undo` |

`a`, `t` and `u` act on every selected thread, or on the one under the cursor
without a selection. ⌘-click adds a thread to the selection or takes it out, and
Shift-click selects from the last thread touched. A right-click opens the
thread's menu — Reply, Done, Open in Rocket.Chat, Check for Messages, Mute
Room, Unmute Room, Leave Room — which acts on the selection when the thread
clicked is in it (`chat-menu`). A right-click on a room in the sidebar offers
Mute Room, Unmute Room, Pin and Leave Room. A key
that acts on mail (`d`, `!`, `u` …) is refused while chat has the cursor,
rather than acting on the mail list behind it.

In the composer (`bindings.chat_compose`): `Enter` or `⌘Enter` sends, and
the composer keeps the keyboard for the next message while the conversation
stays on screen — in a room; in Waiting and Today the answered thread leaves,
and the keyboard goes back to the list. `Shift-Enter` starts a new line, `Esc` gives the keyboard back to the list and
keeps what was typed, for that thread, until it is sent — across a restart
too. `↑` in an empty composer edits your last message in the conversation;
the × beside *Editing your message* gives the edit up. `⌘U` attaches files here as well.

| Command | Action |
|---------|--------|
| `open-chat` | Open Waiting |
| `chat-open-room` | Open a room or a direct message found by name (default `gr`) |
| `chat-new-message` | Start a chat with one person or several (default `gn`, and `C` in chat) |
| `chat-write-room` | Write to the room itself, outside any thread (default `c` in chat) |
| `chat-reply-thread` | Start a thread on the newest message of a room's timeline (default `R` in chat) |
| `chat-leave-room` | Leave the room, or hide a direct message, after asking |
| `chat-mute-room [1h\|90m\|2d\|today\|17:00\|forever\|off]` | [Mute](#muting-a-room) the room for that long; without an argument the picker asks, and `off` unmutes (default `zq`) |
| `chat-unmute-room` | End the room's mute |
| `chat-muted-rooms` | List the muted rooms; choosing one unmutes it (default `zQ`) |
| `chat-create-room <name> [private]` | Create a room, or a private group, and open it; without a name, the command line asks |
| `chat-also-in-room` | Also show a thread reply in the room, or not |
| `chat-sync` | Check the chat server for messages now |
| `chat-sign-in` | Sign in, on the server's own sign-in page |
| `chat-sign-out` | Sign out and forget the session |

### In-app (date picker)

Opened by `s`, `D`, `S` and `r`. A text input at the top with a calendar below:
typing replaces the calendar with parsed suggestions, clearing the input brings
it back. Type `tomorrow`, `3d`, `next tuesday`, `eom`, `Sept 2028 12`,
`2026-09-12`, or `tomorrow 9am` for a reminder.

| Key | Action | Command |
|-----|--------|---------|
| `Enter` | Take the highlighted suggestion, or the calendar's day | `accept` |
| `Esc` | Cancel, changing nothing | `cancel` |
| `Ctrl-d` | Clear the date (on an empty input) | `clear-date` |
| `↓` / `↑`, `Ctrl-n` / `Ctrl-p`, `Tab` | Move through the suggestions | `move-down`, `move-up` |
| `←` / `→` | Previous / next day on the calendar | `move-left`, `move-right` |
| `PageUp` / `PageDown`, `Ctrl-b` / `Ctrl-f` | Previous / next month | `prev-month`, `next-month` |
| `Ctrl-t`, `Home` | Jump to today | `today` |
| `Ctrl-e` | Set the other date instead: from "Postpone until" to "Due" and back (only where the picker offers both) | `switch-field` |

`create-task-from-message postpone` (`Ctrl-t` on a message) offers both: the
task is made postponed until the date picked, or, after `Ctrl-e`, due on it
and with no postpone date.

Only bracketed keys can be bound in `bindings.date_picker`: a bare character
belongs to the text input, since `s`, `d` and `3` are the parser's own
vocabulary.

The bindings can be overridden in `config.yml` under `bindings`, one table per mode:
`index_normal`, `index_detail` (layered over `index_normal` while the reader has
focus; empty by default), `task_normal`, `task_detail`, `task_edit`,
`date_picker` and `compose_normal`. A table in the config is merged over the built-in one rather than
replacing it, so a config written before a command existed keeps working; bind a
key to `noop` to switch a default off. The `compose_edit`, `command` and
`task_detail_terminal` tables are parsed but not used by the app.

```yaml
bindings:
  index_normal:
    <c-r>: sync
    "!": toggle-flag
    a: stage-read && mark-move Archive && advance
    # A task for the message, and the message filed, in one key.
    A: create-task-from-message && stage-read && mark-move Archive && advance
  date_picker:
    <c-t>: today
```

A chain is one action: a step that fails stops the rest, so the message is
not filed when its task could not be made, nor when the picker of
`create-task-from-message postpone` is dismissed; the status line says what every
step did, and one ⌘Z takes the whole key back.

`commands` gives a chain a name of your own, which then works wherever a
command does: on a key, on the command line (`:file-and-next`) and in the
[command palette](#the-command-palette), under "Your commands" with its
`title` (the name when there is none). One may name another; a built-in
name cannot be taken over.

```yaml
bindings:
  commands:
    file-and-next:
      run: stage-read && mark-move Archive && advance
      title: File and Move On
    task-and-file: create-task-from-message && file-and-next   # the short form
  index_normal:
    a: file-and-next
```

The compose window keeps everything about the message in one header block —
From, To, Cc, Subject, and the files once there are any. Bcc gets a line of
its own from the **Bcc** button at the end of the Cc line, or by itself when
the draft already has one. A recipient is dragged to another place in its
line, or to another of To, Cc and Bcc; Bcc shows while a drag lasts, and
stays only when the recipient is dropped there. `⌥↓` and `⌥↑` take the
selected recipient (clicked, or the last one after `Backspace`) to the line
below or above. A recipient dropped on a line that has its address already
leaves its own line, and is not doubled. `Esc` during a drag, or letting go
outside the lines, puts it back. The title bar holds Send, **Save & close**,
**Discard**, Attach, Follow up, the switch between Neovim and the plain text
field, and the draft's state (`Saved 14:02`, or what went wrong). Below the
header is the **Check** line when there is something to look at before
sending (see [Checks before sending](#checks-before-sending)). Below the body
is only Neovim's own status line.

`⌘Enter` sends and `⌘S` saves the draft. The draft is also saved every half
minute while it changes, on `:w` in Neovim, when an attachment is added or
taken off, and when the window closes. A save and a send both take the body
as it stands in Neovim, written or not, so there is no need to `:w` first;
files dropped on the window go out with the message. A saved draft reopened
from the Drafts folder comes back with its recipients, Bcc included, its
threading headers and its attachments.

**Save & close** (`⇧⌘S`) saves the draft and closes the window. If the save
fails, the window stays open and says why. Closing the window saves too.
**Discard** asks first, then deletes the draft from the Drafts folder and
closes the window. A discarded draft cannot be brought back.

Neovim does not hold the signature. It shows it as lines below your text —
or above the quote, with `reply.signature: above` — that the cursor cannot
reach, drawn in the `OrbSignature` highlight group (linked to `Comment`; set
it in your colour scheme to change it). What you type at the end goes above
it, and it is put back into the body for every save and for the send. To
change the signature for one message, switch to **Plain text**: the field
holds the whole body, signature included, and a signature edited there stays
part of the text when you switch back. A draft whose signature was edited, or
that carries another identity's, opens with it in the text in the same way.

`Ctrl-t`, or **Follow up**, asks for a day to follow the message up on; the
button then names it. When the message is sent — or queued, if the server
cannot be reached — a task linked to it is made, titled from the subject and
postponed to that day, as a task made from a message is. Clearing the date in
the picker takes the follow-up back. The day belongs to the window, not to
the draft: a draft reopened later has none. It needs a `tasks` section in
`config.yml`, and if the task cannot be made the message still goes and the
window stays open to say so.

### Templates

A template is a YAML file in `~/.config/orb/templates/` — the folder beside
the config file, or `compose.templates_dir` when it is set. Every `.yml` and
`.yaml` file directly in it is one template:

```yaml
name: Einladung zum internen Workshop      # what the picker lists
subject: Vorbereitung auf den Workshop   # optional
accounts: [Work]                           # optional; every account without it
body: |
  Lieber {Vorname}

  Für den bevorstehenden Workshop vom {Datum} bitten wir Dich, …

  [1] {Webseite}
```

```yaml
compose:
  templates_dir: ~/Documents/mail-templates   # optional
```

A word in braces is a placeholder, and the word is its label: `{Vorname}`
and `{{Vorname}}` are the same placeholder, written as `{Vorname}` in the
draft. The label starts with a letter and holds letters, digits, spaces, `_`,
`-` and `.`, so code or JSON in braces stays text. The blank lines a body
starts and ends with are dropped.

Orb reads the folder each time the picker opens, so a template written or
changed in any editor is there without telling Orb. A file that cannot be
read is listed at the end of the picker with the reason.

A template is picked once the message is under way: write a new message,
reply or forward as always, then press `⌘T` or **Template** in the title bar.
The picker lists the templates of the account written from first, then those
for every account, and filters them as you type; the right side shows the
one under the cursor as it will be inserted. `Enter` inserts it, `⌘E` opens
its file in a tab of the compose window's Neovim, `Esc` closes the picker.

The template goes in where the cursor is — in a reply above the quote, where
the cursor starts: in place of the cursor's line when that line is blank,
below it otherwise. Its subject fills the Subject line only while that is
empty, so a reply keeps its `Re:`. In Neovim the insertion is one change, and
`u` takes it back.

A **Fill in** line under the header then has one field per placeholder, in
the order they first appear. Typing in a field writes the text wherever the
placeholder stands — in the body and in a subject the template filled — as
you type; clearing the field puts the placeholder back. `Tab` goes to the
next field; `Enter`, `Esc` or `Tab` past the last hands the keyboard to the
body, at the first placeholder still open. Once all are filled, that closes
the line. A placeholder whose text you edit by hand in the body is left to
you from then on.

Orb fills in what it knows, and the field says where the value came from:
`{Vorname}`, `{FirstName}`, `{First name}`, `{Prénom}` or `{Nome}` with the
first To recipient's first name, `{Name}` or `{Full name}` with their whole
name — taken from the name in the address, and following To until you type
over it. In Neovim a placeholder still open is drawn in `OrbPlaceholder`
(linked to `DiagnosticWarn`), a filled one in `OrbPlaceholderFilled` (linked
to `Visual`); set them in your colour scheme to change them.

Replacing the body as a whole — switching between Neovim and the plain text
field, or to an identity with another signature — closes the Fill in line.
Its placeholders are still checked before sending, as long as the window
stays open; a draft reopened from Drafts is not.

`bindings.compose_normal` binds the window's commands: `send` (default
`<d-enter>`, that is `⌘Enter`; `<d-s-d>` would be `⇧⌘D`), `save` (default
`<d-s>`), `save-and-close` (default `<d-s-s>`, that is `⇧⌘S`), `discard`
(unbound), `pick-sender` (default `<d-i>`, that is `⌘I`), `cycle-identity`
(unbound; the next identity, through every account's, without the list),
`follow-up` (default `<c-t>`), `template` (default `<d-t>`, that is `⌘T`),
and `next-field` and
`prev-field` (default `<c-j>` and `<c-k>`), which move the keyboard
down and up through From (when there is another identity to choose), To,
Cc, Bcc (when shown), Subject, the Fill in line (while there is one) and the
body; past either end they go to
`navigation.escalate_command` when it is set. These keys
are taken from the keyboard before Neovim sees them, so `<c-j>` and `<c-k>`
no longer reach Neovim's own mappings; bind them to `noop` to give them back,
or move the commands to a chord Neovim can spare — a key without a modifier is
a character in a field or in Neovim and is never taken. Any other entry in the
table is ignored. Everything else is Neovim's own, except the ⌘ chords, which
stay with the window and the menu bar.

The body is plain text — nothing is converted to HTML — and Neovim edits it
with the `mail` filetype, so your settings for mail apply. It is sent as
`format=flowed` (RFC 3676): a line ending in a space continues in the next
one, and the recipient's client may rewrap the paragraph to its window; a line
without one is a hard break. Neovim writes such lines itself with
`formatoptions+=w` and a `textwidth`, for example:

```lua
vim.api.nvim_create_autocmd('FileType', {
  pattern = 'mail',
  callback = function() vim.opt_local.formatoptions:append('aw') end,
})
```

Keep anything that strips trailing whitespace on save away from `mail`
buffers: it turns every soft break into a hard one. The signature is out of
its reach, and a line that is only `--` goes out as the `-- ` that marks a
signature, as does the first line of a signature file that starts with `--`.

### Menu shortcuts

| Shortcut | Menu item | Action |
|----------|-----------|--------|
| `⌘N` | New Message | Compose |
| `⌘F` | Search Mail | Open the search field |
| | Command Line | Open the `:` command line |
| | Show as HTML / Plain Text / Source | `view html`, `view text`, `view raw` |
| | Draw HTML Mail Dark | `toggle-dark-mail` |
| | Load Remote Images | `load-remote-images` |
| | Show as Conversations / Expand or Collapse Conversation / Open Whole Conversation / Open or Close All Messages of Conversation | `toggle-threads`, `toggle-thread`, `open-thread`, `open-all-messages`, `close-all-messages` |
| `⌘+` / `⌘-` / `⌘0` | Make Text Bigger / Smaller / Actual Size | `zoom-in`, `zoom-out`, `zoom-reset` |
| | Switch Between Hybrid and Terminal | `toggle-look` |
| | Show or Hide Sidebar | `toggle-sidebar` |
| | Show Mail in Today / Show Tasks in Today | `toggle-today-mail`, `toggle-today-tasks` |
| | Show Reading in Today | `toggle-today-reading` |
| | Show Chat in Today | `toggle-today-chat` |
| | Console | `console` |
| `⌘K` | Go to… | `go-to` |
| `⇧⌘K` | Command Palette… | `command-palette` |
| `⌃⌘↓` / `⌃⌘↑` | Next / Previous Folder | `next-folder`, `prev-folder` |
| | Today / Upcoming / Someday / Completed Tasks / Archived Tasks / Task Trash | `open-task-view …` |
| | To Read / Must Read / Reading Archive | `open-reading list`, `open-reading must-read`, `open-reading archive` |
| | Staged Actions | `review-pending` |
| | Outbox | `outbox` |
| | Chat: Waiting / Open Chat Room… / Mute Chat Room… / Muted Chat Rooms… / New Chat Message… / New Chat Room… / Sign In to Chat… / Sign Out of Chat | `open-chat`, `chat-open-room`, `chat-mute-room`, `chat-muted-rooms`, `chat-new-message`, `chat-create-room`, `chat-sign-in`, `chat-sign-out` |
| | Window ▸ Chat Window | `open-chat-window` |
| `⌘R` / `⇧⌘R` | Reply / Reply All | Reply / reply-all |
| `⇧⌘F` | Forward | Forward |
| | Edit Draft | `edit-draft` |
| `⇧⌘U` | Mark as Read/Unread | Toggle read / unread |
| `⇧⌘L` | Flag | Toggle flag |
| `⌃⌘A` | Archive | Move to the account's archive folder now; from the reading list, to its archive, and back |
| | Read Later | `read-later` |
| | Move to… / Copy to… | Move or copy now, to a folder picked from a list |
| | Move / Copy to Another Account… | `move-across`, `copy-across` |
| `⇧⌘J` | Move to Junk | Move to the account's junk folder now |
| | Not Junk | Move back to INBOX from the junk folder |
| `⌘⌫` | Move to Trash | Move to Trash now; in Trash, delete for good after confirming |
| | Stage for Deletion / Stage a Move… / Unstage | `mark-delete`, `mark-move`, `unmark` |
| | Open / Save… / Quick Look Attachment | `open-attachment`, `save-attachment`, `quick-look` |
| | New Task from Message / Tasks for Message | `create-task-from-message`, `open-tasks-for-message` |
| `⇧⌘N` | Get New Mail | Sync all accounts |
| | Account Status | Show each account's standing with its server |
| | Get New Feed Items / Feed Status | `poll-feeds`, `feed-status` |
| | Outbox / Send Everything in the Outbox | `outbox`, `retry-outbox` |
| | New Folder… / Rename Folder… / Delete Folder / Subscribe / Unsubscribe | The folder commands |
| `⇧⌘P` | Review Staged Actions | Show everything staged, by folder, with Apply and Discard |
| `⇧⌘E` | Apply Staged Actions in Folder | Apply the staged actions in the current folder |
| | Apply All Staged Actions | Apply the staged actions everywhere |
| | Discard Staged Actions in Folder / Discard All Staged Actions | Drop staged actions without applying them |
| | Tasks menu | `open-tasks-window`, `create-task`, `edit-task`, `complete-task`, `uncomplete-task`, `clear-completed`, `merge-tasks`, `export-tasks`, `import-tasks`, `import-things`, `set-up-iphone`, `set-due-date`, `postpone-task`, `set-recurrence`, `add-reminder`, `attach-file`, `delete-task`, `restore-task`, `empty-task-trash`, `sync-tasks` |
| | Help menu | `help`, `account-status`, `sync-stats` |
| `⌘1` – `⌘3` | First / Second / Third Tab | Switch window tab |

Every entry runs the command named, exactly as a key bound to it would, so a
menu item does what its shortcut does. `⌘A` is the keymap's `select-all` in
the lists and the reader, and the system's Select All in a text field.

## The window

Orb follows the system appearance, light or dark, unless `appearance.theme`
fixes one.

The cursor of the pane the keyboard is in is filled with the accent colour,
its text in white, as menus mark their current item; in the other panes it
keeps a grey fill, so the open message and folder stay easy to find. Rows
selected with `Space` and `J`/`K` are tinted with the accent.

Orb comes in two looks, chosen with `appearance.look`. **Hybrid**, the
default, sets text in SF Pro and what is scanned in columns — dates, counts,
keys, the status line — in SF Mono, on 24 px rows. **Terminal** is
monospace throughout and denser, on 24 px rows, in the manner of mutt or
aerc: each row starts with a flag column — `N` for unread, `!` for flagged,
`+` for files, `D` and `M` for a staged delete or move — and then the date.
Icons become glyphs and brackets (`[ ]` and `[x]` for a task, `▸` and `▾`
for a fold), the reader's and the task's headers are `Key value` lines, the
window's own labels are lowercase while names, subjects and folders keep
their case, and the status line is a modeline that starts with the mode
(`MAIL`, `TODAY`, `TASKS`, `SEARCH`, `CMD`). Both looks show the same
things and take the same keys. `toggle-look`, also in the View menu,
switches every window to the other look on this machine; `look hybrid` and
`look terminal` name one. The machine keeps that choice over the config's
until the config names a different look, which then takes effect when the
file is saved.

In a dark window HTML mail is drawn dark too — the frame inverts
the page and puts its pictures back — so a white newsletter reads as a dark
one; `toggle-dark-mail` shows the message as it came, and
`appearance.dark_html_mail: false` leaves every message as it came. `⌘+`,
`⌘-` and `⌘0` set the text size for this machine, in every window;
`appearance.font_size` is what `⌘0` comes back to.
`appearance.editor_font_size` gives Neovim in the compose window a size of
its own, which `⌘+` and `⌘-` scale along with the rest. A click into an HTML
message hands the keyboard straight back to the list, so the keys keep
working; the pointer still selects text, and a link in it opens in the
default browser (a `mailto:` link in the mail app). The
lists are listboxes with their cursor and selection announced, and the status
line is a live region, for a screen reader.

Keys can be typed ahead: each command runs after the ones typed before it,
on the list as they left it, so `a` then `j` archives the message `a` was
pressed on and then moves down, however long the server takes. Marking read
and flagging change the list at once and reach the server in the
background.

The reader's header gives the subject, the sender's name and address beside
the date (`Today 12:04`, `Tue 13 Oct, 09:12`), the recipients by name with
`me` for your own addresses and `cc` before the copies, and the message's
files and linked tasks as chips: a file opens on a click, with Quick Look and
Save at its end and dragging copies it out; a task's box completes it, its
title opens it and its clock postpones it. The reader and the task detail
start at the same edge as the list's text.

The reader always shows the message under the cursor: moving
the cursor shows the new message's headers at once, with a loading line
until its body is in, and never leaves the previous message on screen.

An empty list says what is empty — a folder, a task view, a search, or Today
with no task due and no mail in any inbox — and offers the keys that lead on
from there, such as `o` for a new task and `C` to compose; each can be clicked
too. The reader pane goes away until the list has something in it again.

**The toolbar** names what the list shows — the folder and its account, the
task view and the date, or the search and its number of results. Beside a
folder's name is the order in force ("Date ↓"); its menu picks the field, the
direction and whether the folder shows as conversations (`zt`). The search box
opens the search field (`/`) and, while results or a task filter are showing,
holds the query, with a button to leave it. Over search results that found
mail and tasks, All / Mail / Tasks shows both or one kind, with their counts.

**The status line** says on the left what was just done and how to take it
back: `⌘Z` after anything that can be undone, `U` after a message is staged.
On the right it keeps the steady state: how many actions are staged, where
the cursor is in the list, and when the accounts last synced, or which of them
could not.

The main window remembers its size and position across launches. Closing it
hides it rather than quitting: syncs go on, the Dock badge keeps counting,
and a click on the Dock icon brings the window back where it was. ⌘Q quits,
after asking if there are staged actions or task changes not yet on the
server; open drafts are saved to Drafts first.

**A window of its own.** A double-click on a message or a task opens it in a
window of its own, beside the list it came from; a draft opens in a compose
window, as `Enter` does. Each message and each task has one such window, and
double-clicking it again brings that window forward. The window shows the
reader, or the task detail, alone, under a toolbar that says where the
message lives, and every key of the reader or of the task detail works on
it. A linked task in a message's window, and a linked message in a task's,
open in windows of their own too. `q` or ⌘W closes the window; it also
closes once its message is filed, deleted or moved, or its task trashed.

## Search

`/` opens the search field from any view, Today and the task views
included, and `Enter` searches: mail in every folder of every account,
every task but the trashed ones, or both. The switch at the end of the
field — Mail, Tasks, All — says which, and `⌃T` steps through it; the field
opens on the last one chosen, on this Mac. In the tasks window it searches
the tasks.

Words are matched everywhere: the subject, the sender, the recipients and
the body of a message, and the title, the notes and the tag names of a task.
Each word of several must be found, each anywhere, case and accents aside.
A word is found as the start of a word in the body — `invoic` finds
"invoices" — and anywhere in the subject, the sender and the recipients:
`thom` finds "Thomas", and `mill` "jmiller@example.com". A prefix narrows
a word to one field, and the rest of the query filters:

| Term | Matches |
|------|---------|
| `f:` / `from:` | The sender's name or address containing it |
| `t:` / `to:`, `cc:` | A recipient's name or address containing it |
| `s:` / `subject:` | The subject; a task's title |
| `b:` / `body:` | The body; a task's notes |
| `tag:` | A task's tag, by the start of its name |
| `"exact phrase"` | The words next to each other |
| `in:Archive`, `acct:Work` | Mail in a folder, or of an account |
| `is:unread`, `is:read`, `is:flagged`, `is:answered`, `has:attachment` | Mail in that state |
| `after:2026-01-01`, `before:2026-06-30` | Mail by date |
| `header:X-Priority=1`, `header:List-Unsubscribe` | Mail with a header containing the text after `=`, or with the header at all. Only headers Orb records: `List-Id`, and those a view, a highlight rule or a `load_remote_images` rule names |
| `~words` | Mail by meaning rather than by word (needs the embedding model) |
| `A OR B`, `-term`, `NOT term` | Either; not (`OR` and `NOT` in capitals) |

`f:anna s:invoice` is mail from Anna with "invoice" in the subject. A part of
a query that only mail can match — a sender, a folder, a state, a date —
finds no task; `f:anna OR tag:work` finds Anna's mail and the work tasks.

The field writes the language for you. As a word is typed, the lines under
it say what it could mean: anywhere, in the subject, in the body, a person
from the address book as sender or recipient, a task's tag — and after a
prefix, the folders (`in:`), the accounts (`acct:`), the states (`is:`).
After `f:`, `t:`, `cc:` and `tag:` the first line is the word as typed —
`f:thom` searches for every sender containing "thom", whoever the address
book offers under it. `↑` and `↓` choose a line, `Tab` puts it in as a
chip, and `Enter` searches. An
operator typed whole becomes the same chip, a click on a chip turns it back
into text, and `Backspace` at the start of the text takes the last one back.
The line at the bottom lists the operators.

The row under the field says where to search: everywhere, the folder that
was open, one account's mail, or Folders…, a list of every folder in which
`Space` or a click ticks the ones to search. `⌃F` steps through them. A
search narrowed to an account or to folders finds mail only, and the
toolbar says where it looked. Searching for tasks, the row is gone: tasks
are in no folder.

The results are laid out as Today is: the mail first, then the tasks, `Tab`
between them, and both sides of the stacked or column layout. Every message
names its folder, the words searched for are marked in the list and the
reader, and `n` and `N` walk them. The mail comes most relevant first —
newest first for an `OR` or a search by conditions alone, closest first for
`~` — or in the order `search.sort` sets (see
[Configuration](#configuration)), and the toolbar's sort
menu or `sort-by` orders it by date, sender or subject instead; the order is applied before the 500-result limit, so oldest
first finds the oldest matches, and `sort-by relevance` goes back. The tasks
stand under the date they come up, the earlier of their postpone and due
date: Today, with the overdue; a heading for each of the next six days; one
for each month after that; Someday for the undated; and Completed, folded
until `za` or a click opens it. All / Mail / Tasks in the toolbar, or `gs`,
shows both or one kind, whichever the field asked for; the choice is
remembered for the next search, as the field's switch is. The results stay open across a sync, refreshed in
place, and `Esc` goes back to the view the search was made from. In the
field, `/` opened over results holds their query: `Esc` with it untouched
leaves the search as well, and `Esc` after changing it puts the field away
and keeps the results.

### Filter

`F`, or Filter in the toolbar, puts a bar on top of the list and narrows
what is on screen as you type, without leaving it: a folder, Today, a task
view, search results. In Today it narrows everything Today shows alike:
the mail, the tasks, and the threads that wait and the reading items when
they are shown — in their sections, or on the [timeline](#today-mixed).
`Enter` gives the list the keyboard and keeps the filter; `Esc`, in the bar
or on the list with nothing selected, takes it off; opening another view
drops it.

In a folder and in Today's inboxes the filter reads the search language
above (`f:anna`, `is:unread`, a word anywhere in a message); in a task view
it matches the title, the notes and the tag names, and `tag:` a tag; over
search results it matches what the rows show — the sender, the subject, the
first line, the folder. A tag in the tasks window's sidebar filters by it
the same way.

In Today the reading items take the search language too; a thread matches
by its author, its room and its text, and of the search language it knows
`is:unread`, `is:read` and `f:`. `is:mail`, `is:task`, `is:chat` and
`is:reading` keep one kind of row and put the others away — `is:chat` the
threads alone, `is:mail is:task` the mail and the tasks — and the rest of the
text narrows what is kept.

## Folders

The sidebar shows the folders the server lists, named as they were written:
a server spells `Entwürfe` as `Entw&APw-rfe` on the wire, and Orb reads that
back. A folder deleted or renamed in another client is gone from the sidebar
after the next sync of the account, with the copy of its messages here.

`create-folder <name>` makes a folder in the open folder's account and syncs
it; `rename-folder <name>` renames the open folder, and the staged actions
and filter history follow it; `delete-folder` deletes the open folder with
every message in it, on the server, after asking. `subscribe-folder` and
`unsubscribe-folder` set the flag other clients use to list only some
folders; Orb itself lists every folder. A name may be anything the server
allows, including a `.` or `/` for a folder inside another, in the server's
own hierarchy.

A folder inside another is drawn under it, one step in, by its own name:
`Archive/2025` is `2025` under Archive. A folder with folders inside it has
a disclosure arrow that folds them away; an account's heading folds the whole
account, and still shows how much is unread in it; every other heading —
Tasks, Reading, Views, Chat, and Tags in the tasks window — folds what is under it,
and opening a view under a folded heading opens the heading. Click the arrow
or the heading, or press `za` or Enter on it in the sidebar; `za` on a folder without any
inside it folds the one it is in. What is folded is remembered on this Mac. A
server that keeps every folder inside `INBOX` (`INBOX.Sent`, as Courier and
Cyrus do) has them drawn beside the inbox rather than inside it.

Each folder's icon says what it is for — inbox, drafts, sent, archive, junk,
trash — by the role the server gives it or the config names it for, whatever
it is called.

The special folders — drafts, sent, trash, archive, junk — are the ones the
config names, with the defaults `Drafts`, `Sent`, `Trash`, `Archive` and
`Junk`. When the server has no folder of that name but marks one for the
role (RFC 6154: `\Drafts`, `\Sent`, `\Trash`, `\Archive`, `\Junk`, as
Gmail, Fastmail and a configured Dovecot do), that folder is used instead, so
the special folders need no config on such a server. A name the config gives
that the server has wins over the role.

## Conversations

`zt` (`toggle-threads`) shows the open folder as conversations, and so does
the switch of two icons — a message, and messages stacked — in a folder's
toolbar and in Today's Mail heading, before the choice of inbox. A
conversation is one row per thread, with the subject of the thread's oldest
message, the member count and a caret after it, and the newest member's
date; the newest conversation comes first whatever the sort said, and the
order button says so. The row stands for the whole conversation: it names who wrote the
newest message and shows its opening words, it is unread while any member
is, flagged or with files when any is, staged when every member is staged
alike, and it carries the [highlights](#highlight-rules-optional) of every
member and the tasks linked to any. `za` (`toggle-thread`) opens the thread
under the cursor — its members appear under it, oldest first, each indented
one step per reply under a tree mark, each a row of its own again — and
closes it again.

The reader shows a conversation's row as the conversation: every message,
oldest first, those not yet read and the newest open, the others a line
each with their opening words; three or more read messages above the first
open one fold into a line of their own. Each open message has its own body,
its files and what its headers say against the sender; the HTML and Text
switch applies to all of them. Resting on the row reads every message of
it, after the same `mark_read_after_seconds` a message takes. `Enter` gives
the reader the keyboard on the first unread message; there `j`/`k` move
between the messages, `gg`/`G` go to the first and the newest, `Enter` or
`za` opens or closes the one the keys are on, `zR`/`zM` (`open-all-messages`,
`close-all-messages`) open or close them all, `Ctrl-d`/`Ctrl-u` scroll a
page, and `h` goes back to the list. A click on a message's line opens or
closes it.

What is done to a conversation's row is done to every message of it, in
whichever folder each lives: read, flag, stage a delete or a move, archive,
move, copy, and so on, a selection of rows included. `t` makes one task of
it, from its newest message. Reply, forward, `gx`, the attachment commands
and `load-remote-images` answer one message: the newest, or, while the
reader has the keyboard, the one its keys are on. To act on one message
alone from the list, open the thread with `za`. The sort has no say in a threaded list. A folder opens the way `threads` in its
config says, and `zt` changes it for as long as it stays open. In Today, `zt`
shows the inboxes' mail as conversations, a thread that spans two inboxes
being one; Today remembers the choice on this Mac, apart from any folder's.

A thread is built from the headers: a message joins the thread of the message
it answers, named by `In-Reply-To` or, failing that, by the last entry of
`References` that is in the folder. A reply by its subject alone — `Re:`,
`AW:`, `Fwd:` and the like, with the list tags removed — that answers nothing
in the folder joins the oldest message with that subject; a message without
such a prefix starts a thread of its own, however the subject reads. The
headers come with every sync; for mail synced before Orb asked for them, the
body backfill fetches them first, three lines per message, so an existing
mailbox has its conversations within minutes of upgrading.

`gc` (`open-thread`) shows the whole conversation a message is part of,
gathered from every folder of its account — the replies in Sent included —
oldest first, as a list of its own that `q` leaves. The list follows the
headers across folders, up to four links deep.

## On a slow connection, or none

Reading never waits for the server when there is something stored to read.
`Enter` moves to the reader at once, with the message's headers and a line
saying the body is loading, and every key typed after it runs straight away.
A message whose text the body backfill has stored opens from that text, and
its HTML replaces it once the server sends it; offline, the text stays.

When an account's server does not answer within eight seconds, what you are
waiting on gives up on it, and for the next thirty seconds does not try it
again but shows what is stored. A sync that reaches the server ends that at
once. A message with nothing stored says it could not be loaded. Each
message's sender check is asked once a session.

## Jumping to a message

Clicking a new-mail notification, or opening a task's linked mail (`M`), jumps
to the message: the cursor lands on it, its row lights up for a moment, and
the reader shows it. Where it lands, in this order:

1. **The list on screen**, when it lists the message — Today included, in any
   layout. A message further down is read in where it stands; in a list shown
   as conversations, its conversation is opened around it.
2. **Its folder**, when the list on screen does not have it, or has a filter
   that hides it. The filter stays with the view it was set in.
3. **Its conversation**, as `gc` shows it, when no folder in the sidebar holds
   it.

When the jump leaves the view, the status line names where it went. `Ctrl-o`
(`jump-back`) goes back to where the jump was made from — the view, its
filter and its cursor — and again for the jump before that.

## Pending actions

Orb supports an aerc-style staged-action workflow so destructive operations
need not be a single irreversible keypress:

1. **Stage** – `d` (`mark-delete`) stages a delete, `s` (`mark-move`) a move
   to a folder picked from a list, or named: `mark-move Archive`. The action is
   recorded but not yet applied; the message is marked in the list, and the
   sidebar counts what is staged in each folder, in amber, beside its unread
   count.
2. **Review** – `p` (`review-pending`, ⇧⌘P) lists everything staged, by
   folder and with each message's subject, with Apply and Discard for each
   folder and for all of it. Below that it lists what the
   [filter rules](#filter-rules-optional) did in the last sync.
3. **Unstage** – `U` (`unmark`) removes the staged action again; `X`
   (`discard-pending`) drops everything staged in the folder, and
   `discard-pending all` everything everywhere. Nothing reaches the server.
4. **Execute** – `x` (`expunge`, ⇧⌘E) applies the staged actions in the current
   folder, `expunge-all` those in every folder. A staged delete moves the
   message to Trash; in Trash itself it deletes permanently. The messages
   leave the list at once and the server follows through the outbox, offline
   included; a move the server refuses puts its messages back.

`D` (`delete-immediate`, ⌘⌫) skips the staging but not Trash: it moves the
message there at once, and in Trash itself deletes it for good after asking.
`S` (`move-immediate`), `a` (`archive`), `spam` and `not-spam` move at once
too. Like applied staged actions they leave the list at once and reach the
server through the outbox, so they never wait on it. `c` (`copy-immediate`)
copies at once, on the server. `move-across`
and `copy-across` take a message into another account: fetched whole from
the one server and appended to the other, flags and all, in batches, and for
a move deleted from the source once they are there. Every immediate move,
read, flag, copy and move can be undone, across accounts too — see
[Undo](#undo).

A delete or move is not carried out on the server when the folder's
UIDVALIDITY there no longer matches the one it was synced under — after a
restore from backup, say, or a migration to another server — because the
message numbers it would send name different mail now. The next sync of the
folder puts the list right.

## Outbox

A message sent while its mail server cannot be reached waits in the Outbox
and is tried again with every sync. If the server then refuses it — a wrong
password, a rejected recipient — it stays there as **not sent**, with the
server's reason, and is not tried again on its own: nothing that has not
gone out leaves the Outbox without you.

While it holds anything, the Outbox is in the sidebar below the pins: an
amber count of the messages waiting, or a red one of those refused. The
status line says the same (`1 not sent`, `2 waiting to send`), and a refused
message is announced with a notification. A click on any of them opens the
Outbox in the reader, refused messages first, each with its recipients, the
login it goes out over, when it was queued, why it has not gone, and three
buttons:

| Button | Does |
|---|---|
| Retry / Send now | Put it back in line and try to send it now. After a refused login the password is fetched again — its command run again, or asked for instead of read from the Keychain — so a corrected password needs no restart. |
| Edit | Open it in a compose window. It moves to Drafts with the window's first save, or leaves with the send; closed unsaved, it stays in the Outbox. |
| Delete | Delete it unsent, after asking. |

`outbox` opens it, `retry-outbox` sends everything in it now; both are in
the Mailbox menu.

## Attachments

A message's attachments are listed under its headers, each with its size.
Clicking a name opens it with its default application — except for the file
types macOS would run rather than display, which are revealed in Finder so
that opening them stays a decision. Files that are dangerous to open for
another reason — HTML and SVG, Office documents with macros, disk images,
Windows programs, and names that hide their real type behind a
right-to-left mark — are shown in red, and opening one asks first. The eye shows it in Quick Look, the arrow
saves it where you choose, and a row can be dragged out of the window onto the
Finder or another application. `open-attachment`, `quick-look` and
`save-attachment` do the same from the keyboard, by the attachment's number.

An attachment is unpacked from the message's full source. The body backfill
fetches that source, after the bodies, for every message that has attachments,
so they open while offline; a message the backfill has not reached yet fetches
it on demand. Files are written once under `~/.orb/attachments/mail/`.

## Undo

⌘Z takes back the last action, and ⇧⌘Z does it again. There is one history for
everything — mail, tasks and chat — shared by every window, so ⌘Z takes back
the last thing done wherever it was done: a message archived from Today's mail
is taken back from Today's task list as well. The last fifty actions can be
taken back, for as long as Orb runs; the history starts empty at launch. Doing
something new forgets what could have been redone.

An action is one key, however much it did: a whole selection, or every step of
a chain such as `create-task-from-message && stage-read && mark-move Archive`.
The status line says `⌘Z to undo` after anything that can be undone, and what
came of the undo when it runs.

What can be undone:

- **Mail** — `toggle-read` and `stage-read`, `toggle-flag`, `move-immediate`,
  `copy-immediate`, `move-across`, `copy-across`, `archive`, `read-later`,
  `spam`, `not-spam`, and `delete-immediate` when it moves to Trash. Only what
  the action changed is put back: marking three messages read, one of which
  already was, leaves that one read when undone.
- **Staged actions** — `mark-delete`, `mark-move` and `unmark` put back what
  was staged on the messages before; `discard-pending` stages again what it
  dropped. `expunge` and `expunge-all` are undone by moving the messages back
  where they were, unstaged; what they deleted for good, out of Trash, stays
  deleted.
- **Folders** — `create-folder`, undone by deleting the folder again as long
  as nothing has been filed into it; `rename-folder`; `subscribe-folder` and
  `unsubscribe-folder`. `delete-folder` cannot be undone, and asks first.
- **Filters** — a `filter` run, as one action: the flags it set are cleared
  again where it set them, the messages it moved come back, and the tasks it
  made go to the task trash. The rules do not file the messages again on the
  next sync, since their log says they did once.
- **Tasks** — every change to a task (see the task spec, §10.3), and a task
  made from a message, a sent message or a chat thread; a reminder, an
  attachment or a linked message added or removed; `work-on clear`; and a tag
  created, renamed, recoloured or deleted — a deleted tag comes back on every
  task it was taken off.
- **Chat** — `chat-done`, and a task made from a thread, which also lets the
  thread wait again; `chat-toggle-read`, on the server too; a reaction added
  or taken back; and `chat-leave-room`, which joins a channel again or shows a
  hidden direct message again — leaving a private group cannot be undone,
  since coming back to one takes an invitation.

A move is undone by carrying out what the outbox still holds, then asking the
server for the messages in their new folder by `Message-ID` and moving them
back, so it works whether or not the destination has been synced since; a
message moved again by someone else in the meantime stays where they put it.
A move of messages without a `Message-ID` cannot be undone, and the status line
does not offer it. A copy is undone by deleting the copies for good — only the
ones whose original is still where it was copied from, so that undo never
deletes the last one left — and a message the destination had before the copy
or the move is never taken out of it.

Sending mail, sending, editing or deleting a chat message, a permanent
delete, `delete-folder`, emptying the task trash and importing tasks have no
undo; the destructive ones among them ask first.

## The console

`console` opens a tab beside the mailbox with two things: the last few
hundred lines of the log as they are written, whether or not `log_file` sends
them to a file, and the commands the app ran lately — syncs, sends, moves,
backfills — each with when it started, how long it took and what went wrong,
newest first. It is the place to look when something did not happen.

The log file, when there is one, is rotated at 10 MB: the file becomes `.1`,
replacing the previous `.1`, and a fresh one takes its place. A database
written by a newer Orb is refused at launch, naming both versions, rather
than written around.

## Account status

Every sync — on a key, from a push, on the timer — records what became of
each account. An account whose last sync did not go through is marked in the
sidebar beside its name: a key for a refused login, a shield for a certificate
the connection could not accept, an unplugged lead for a server that could
not be reached, and a warning sign for anything else. A turning arrow shows
while a sync runs. The mark's tooltip, and the `account-status` panel, carry
the server's own words, the folder concerned and when it happened; the panel
also says when the account last synced cleanly. A tasks folder that cannot be
synced marks the tasks account the same way, even while its mail syncs.

## New-mail notifications and the Dock badge

Unread mail that arrives in a watched folder — INBOX, and any folder marked
`watch` — is announced with a notification, one per account per sync: a
single message by its sender and subject, several as a count with the first
few subjects under it. Clicking it brings Orb forward and opens the newest of
them. What was in the mailbox when Orb started is not announced. The Dock badge
counts the unread mail in every account's INBOX and follows every sync and
every message marked read. `dock_badges` puts up to four counts on the icon
instead, one per corner and each in its own colour — an account's inbox, or
any folder; see [Global settings](#global-settings).

Notifications need the notification permission, which Orb asks for at the
first launch. Once refused, macOS does not ask again, and only System
Settings › Notifications › Orb can switch it back on. A task's Reminders field
says so while it is off, with a button that opens that page. In a
`start.sh --dev` run there is no bundle for the system to address, so nothing
is posted and the log says so.

## Tasks

Enabled by the [`[tasks]`](#tasks-optional) section. Tasks live as ordinary
RFC822 messages in one IMAP folder — that folder is the source of truth, and the
local SQLite tables are a cache that can be deleted and rebuilt from it.

**Views.** *Today* sits at the top of the sidebar, as the first
[pin](#pinned-optional) unless `pinned` says otherwise, and is the landing view: everything
postponed to today or earlier, everything due today or earlier, everything
overdue, and everything completed today (struck through until midnight, so the
action stays undoable) — a repeating task too, though it is already waiting
for its next date. `clear-completed` (Tasks ▸ Clear Completed from Today)
hides the tasks completed so far from Today until midnight, on every Mac and
iPhone that shares the tasks folder (the iPhone's Today has it in its ⋯ menu,
as *Hide Completed*); they stay in *Completed*, and a task completed
afterwards shows as usual. `tasks.keep_completed_in_today: false` leaves completed tasks out
of Today altogether, and `tasks.completed_first_in_today: true` lists them
above the open tasks rather than in their place in the order. Below it, *Upcoming* (every open task with
a date, the overdue and today's included, in date order under a heading for
today, one for each of the next six days, one for each month of the coming
year and one for each year after that), *Someday* (no dates at all), *Completed*, *Archived* and *Trash*.
A task completed longer ago than `tasks.archive_after_days` (default 30)
leaves Completed for Archived; nothing on the server changes, and
un-completing it brings it back like any other. Today shows the
inboxes' mail and its tasks on one grid, mail first, under a *Mail* and a
*Tasks* label with their counts: a task's box sits in the mail's status column,
its title where the sender starts, its tags (`#name`) and marks among the
badges and its due date in the date column. A due date that is today or past
is red, there, in the task's detail and on its chip under a message; an overdue
one is marked `!` besides. The tags show in either layout. With more than one account, a switch at the end of the *Mail* label shows
every inbox (*All*) or one account's; the choice is remembered on this Mac, and
`today-mail <account>` (or `today-mail all`) makes it from the keyboard. Today
opens with the cursor on the first message, or on the first task when there is
no mail. `j`/`k` walk the mail and the tasks in Today as one list; `Tab` jumps
between the halves, and `Ctrl-j`/`Ctrl-k` walk the mail, the tasks and the
reader below them. The half the keyboard is in has the accent edge and its label in the
accent; the other keeps its cursor in grey, as any pane without the
keyboard does. The line between them can be dragged, and the tasks then
keep that share of the pane; a double-click on it goes back to fitting them,
up to half the pane.

**Grouping by tag.** The tag button at the right end of the *Tasks* label in
Today, Upcoming and Someday (`zg`, `toggle-group-by-tag`, View ▸ Group Tasks by
Tag) lists the tasks under their tags rather than in one list — in Upcoming,
rather than under its dates. The tags come in the registry's order, each with
the number of its tasks, and *No tag* last. A task with several tags is listed
under each: it is still one task, counted once, and completing it under one
heading completes it under all. `⌘↑`/`⌘↓` and a drag move a task within its
heading. The button is pressed while it is on, and the choice holds for all
three views and is remembered on this Mac.

**Due first.** Before the tag button in Today's *Tasks* label sits a button
with how many open tasks are due today or earlier, in red; it shows the
number whether it is on or not, and in every layout, and has none once
nothing is due. Pressed (`zd`, `toggle-due-first`, View ▸ List Due Tasks
First), it lists those tasks above the rest: under *Due*, with their number,
and the rest under *Other*; grouped by tag, within each tag, set apart by a
line. A completed task is no longer due and leaves the band, and Completed
first still puts the completed tasks on top. Each band keeps the one manual
order, so `⌘↑`/`⌘↓` and a drag move a task within its band, between its
neighbours there. The choice is remembered on this Mac.

**What Today shows.** The *View* menu in Today's toolbar (`today-menu`)
switches each of Today's sections on or off — *Mail*, *Tasks*, and once
they exist *Chat* and *Reading* — and chooses the layout. It stays open
while its switches are flipped, so several go at once; `Esc` or a click
elsewhere closes it. The sections stand in that order, top to bottom; a
section put away keeps its place and its cursor for when it comes back, and
`j`/`k`, `Tab` and `Ctrl-h`/`j`/`k`/`l` pass it by. With the mail put away,
the first section shown takes the room the mail had. Today shows one
section at least, so the last one cannot be put away. Making a task, or
opening one, brings the tasks back. `toggle-today-mail` and
`toggle-today-tasks`, and View ▸ Show Mail in Today and Show Tasks in Today,
do the same from the keyboard and the menu bar. What is shown is
remembered on this Mac.

*Columns* in the View menu, `zl` and View ▸ Switch Today Between Stacked
and Columns put the mail and the tasks side by side instead:
the mail on the left, the tasks on the right, each the full height of the
pane. The rows are shorter there: no snippet, no address beside a sender's
name, no tags, and fewer badges when they would crowd out the subject. A
sender whose name is another address keeps its warning mark and shows the
name in red; the reader shows the address. `j`/`k` stay within their column,
`gg`/`G` go to its ends, and `Tab` or `Ctrl-h`/`Ctrl-l` move to the other
column. The mail takes
three fifths of the width until the line between the columns is dragged; the
columns then keep their shares, and a double-click on the line gives the mail
its three fifths back. The layout and the widths are remembered on this Mac.

<a id="today-mixed"></a>*Mixed* in the View menu, `zx` and View ▸ Show Today
as One Timeline keep the tasks in their column on the right and make the left
column one timeline, headed *Timeline*: the mail, and the threads that wait
and the reading items when *Chat* and *Reading* are shown, each at its
moment — a message when it was sent (a conversation at its newest message),
a thread when its newest message was written, a reading item when it was
published. A kind column before the sender says what each row is. There are
no groups: what still waits from yesterday is today's too. The order button
beside the View menu turns the timeline over, *Newest first* or *Oldest
first*; shown as conversations it is newest first only. `j`/`k`, `gg`/`G`
run through the timeline, and `Tab` or `Ctrl-h`/`Ctrl-l` go to the tasks and
back. The keys are those of the row under the cursor — on a thread `a` is
done and `r` replies in the chat, on a message or a reading item they act as
on mail — and the reader follows: a message, or a conversation with its
composer. The status line names the kind beside the position. When `a` or
any other key takes the row under the cursor off the timeline, the cursor
goes to the row below it, whatever its kind — or, on the last row, to the
one above.
`Space`, `J`/`K` and Shift- or ⌘-click select rows of every kind together,
and `Esc` lets them all go. With rows selected, an action applies to them
rather than to the cursor, each kind its own way: `a` archives the messages
and the reading items and is done for the threads, `t` makes a task of each,
and `toggle-read` marks each read or unread. The timeline
lists every thread that waits; a thread that gets a new message moves to its
new moment, and the cursor goes with it. `zx` again, or another layout in the menu, goes
back to the layout Mixed was entered from.

**Chat in Today.** Once there is a [chat server](#chat), *Chat* in the View
menu shows the threads that wait in Today, in a section headed
*Chat* below the tasks, and above the reading section when that is shown;
`zw` and View ▸ Show Chat in Today do the same. In columns it sits under the
tasks. It is a pane of its own: it takes the accent edge alone when the
keyboard is in it, and maximized from it, it is all the list shows. It lists
at most thirty, with a link to the rest, and says *Nothing waits* while
nothing does. The line above it can be dragged, and it then keeps that share
of the pane; a double-click on the line goes back to fitting it. `j`/`k` run
on from the last task into it and on into the reading section, and `Tab`
goes from the tasks to it, then to the reading section and back to the
mail. In it
the chat keys hold — `r` replies, `a` is done, `gx` opens the thread in
Rocket.Chat — and the reader shows the conversation with its composer.
Whether it is shown is remembered on this Mac.

**Reading in Today.** Once there is a [reading list](#reading), *Reading*
in the View menu shows it in Today, below the tasks and the chat. `zr`
and View ▸ Show Reading in Today do the same. In columns it sits under the
tasks. A switch in its heading chooses what it lists: the must-read items,
or the new ones. It lists at most thirty; a link below it opens the rest.
The line above it can be dragged, and it then keeps that share of the pane;
a double-click on the line goes back to fitting it, up to a third of the
pane.
`j`/`k` run on from the last task or thread into it, and `Tab` goes from
the tasks and the chat to it and back to the mail. The keys act on the item under its cursor as on a
message, and the reader shows that item. Whether it is shown, and what it
lists, is remembered on this Mac.

**The tasks window.** Tasks ▸ Tasks Window (`open-tasks-window`) opens a
second window that shows tasks only — for a second screen, beside the
mailbox. Its sidebar has the task views and the tags; a tag filters the open
view, and a second click lifts the filter. Today there has no mail half. The
detail sits beside the list while the window is at least 900 px wide and
below it when narrower; beside it, the line between them can be dragged, and
a double-click on it gives the list its own width back. A linked message opens in the mailbox, which comes to
the front. The menu bar acts on whichever of the two windows is in front, and
the window keeps its size and place across launches.

**Working on.** `w` (`work-on`) marks the task under the cursor, or the
selection, as the one being worked on; again takes the mark off, and
`work-on clear` takes it off every task. The task keeps its place and gains
a ▶ (`>` in the Terminal look); the status line names it from anywhere —
or how many there are — and opens it on a click, and Today in the sidebar
carries the mark too. Completing or trashing a task takes the mark off. The
mark is this Mac's alone: it is not synced and not logged (todo-spec §19).

**Merging.** Select two or more tasks and choose *Merge…* from their
context menu (or Tasks ▸ Merge Tasks…, `merge-tasks`). A dialog asks which
task stays — the first selected unless you pick another — and proposes its
title, all their notes (the others' titles included, so nothing is lost)
and the earliest postpone and due date, each of which you can change; ⌘↩
merges. Every step, tag, reminder still to come, linked message and
attachment comes along, and the other tasks go to the Trash. ⌘Z takes the
whole merge back. On the iPhone, select the tasks and tap *Merge* in the
bar below (todo-spec §29).

**Steps.** A task can carry a checklist: what doing it consists of, ticked
off one by one. `+` (`add-step`), or *Step* in the detail, opens a new step
at the end of the checklist to type into: Enter keeps it and opens the next,
Escape leaves. A double-click on a step, or `edit-step`, renames it in place.
A click on a step's box ticks it, and its cross takes it away. A click on a
step selects it, ⌘-click adds one to the selection and Shift-click a range;
`d`, Backspace or Delete, or the cross on any of them, removes the selected
steps at once and leaves the task alone, and `x` ticks them all, or reopens
them all when all are done. A right-click on a step offers
Rename, Mark as Done and Delete Step, for every selected step. Drag a step to
move it. Dragging a selected step moves all the selected steps, and steps that
were apart land together.
`toggle-step [n]` ticks step *n*, the selected steps, or the next open one;
`add-step <title>`, `rename-step <n> <title>` and `remove-step <n>` do the
same from the command line. On the iPhone a step is added in the field below
the checklist, renamed or deleted from its context menu, and deleted by a
swipe too. *Select Steps* in the ⋯ menu, or *Select* in a step's context
menu, picks several steps. The bar below then marks them done or deletes
them, and dragging one of them moves them all.
The detail shows the steps under their count and a bar, with the next open
one stressed, and the list shows the count beside the task (`[2/4]` in the
Terminal look). Every change is an edit of the task, logged and undoable; a
repeating task starts its checklist afresh when it moves to its next date.
A task with steps is written in a newer form that an older Orb leaves alone
rather than rewrite without its steps (todo-spec §17).

**Exporting tasks.** Every task lives on one IMAP server, so `tasks.export`
writes all of them — open, completed, archived and trashed, with their
steps, reminders, links, files and whole history — to a directory on this
Mac. A snapshot is taken whenever the newest complete one is older than
`interval_hours` (default 24): at launch, after a wake and while Orb runs,
and on `export-tasks` (Tasks ▸ Export Tasks Now). Each is a directory of its
own, `orb-tasks-<time>-<host>/`:

- `tasks.json`: the tags and every task, in a stable order that diffs
  cleanly;
- `attachments/<id>/<filename>`: each file as it is;
- `manifest.json`: every file's size and SHA-256;
- `schema.json`: a JSON Schema of `tasks.json`, so nothing else is needed to
  read it;
- `README.txt`.

The format is in `todo-spec.md` §20. A snapshot appears under its name only
once it is complete on disk and has been read back; a file that cannot be
fetched leaves it marked incomplete, and the next export tries again. The
newest `keep` (default 30) complete snapshots of this Mac are kept, never
fewer than one, and another Mac's in the same directory are left alone.
Everything is readable by you alone. A failed export says so in a
notification, and the status line says "task export failed" or "task export
overdue" until one succeeds; `sync-stats` names the last complete one.

`import-tasks [snapshot]` (Tasks ▸ Import Tasks…) reads one back: it says
first how many tasks are new, newer in the snapshot or as new here, which
are left out and why, and imports only when you confirm. Tasks are matched by
their id, never by title; a newer copy replaces yours, an older one only adds
its history, and what is imported is uploaded so it wins on every machine. A
task you purged here stays purged unless you add `--include-purged`; a file
whose bytes are not what the export recorded leaves its task out
(todo-spec §21).

**Importing Things 3.** `import-things` (Tasks ▸ Import from Things 3…) reads
Things' own database — a copy of it, so Things can stay open and is never
changed — or the `main.sqlite` you name. It says first what it will bring
over and imports only when you confirm. Titles, notes, start and due dates,
tags, checklists (as steps), reminders, completed and cancelled to-dos and
repeating ones come over; a to-do's project and area become tags, and a
mail its notes link to (`message://…`, as Things writes it) becomes a
linked message. A
repetition Orb cannot express, such as "the second Monday", leaves its task
undated with a note in its history. The Things trash, and projects and
headings themselves, are left out. Importing again brings over what changed
in Things since — new to-dos, edits, and to-dos moved to its trash — except
into a task you have edited in Orb in the meantime: those stay as they are,
and the dry run names them (todo-spec §22).

**Mail links in notes.** A `message://…` link in a task's notes — the form
Apple Mail and Things write a link to a mail in — opens that mail in Orb
when clicked, found the way a linked message is. `link-mail-in-notes` turns
every such link in every task's notes into a linked message of that task,
as one step `u` takes back; run it once for tasks imported before the import
did that itself. A mail that is not on this Mac is looked for on the server
of the tasks account when it is opened.

**Dates.** A task has an optional postpone date (hide it until then) and an
optional due date (when it actually has to be done), both local civil dates. All
date-dependent views are re-evaluated at local midnight, on wake from sleep and
on a timezone change, so an Orb left open overnight is right in the morning.

**Reminders.** Any number per task, each a local date and time, registered with
macOS as scheduled notifications so they fire whether or not Orb is running.
Clicking one names the task. Reminders missed while the machine was asleep fire
on the next launch if they are less than 24 hours overdue; anything older is
collected into a single summary notification and one log entry per task.

**Recurrence.** A subset of RFC 5545 `RRULE` — `FREQ`, `INTERVAL`, `BYDAY`,
`BYMONTHDAY`, `UNTIL`/`COUNT`. Completing a recurring task logs the completion
and moves the task to its next occurrence rather than retiring it, so a
recurring task is one timeline instead of a graveyard of copies. Advancement is
relative to the scheduled date so a task done three days late does not drift;
per task, it can advance from the completion date instead.

**Mail integration.** `t` on a message — or on a whole selection — creates one
task each, titled from the subject and postponed to today, so it is in Today,
at the top like every new task.
The mail stays on screen and the status line says what was made; ⌘Z takes it
back. `Ctrl-t` (`create-task-from-message postpone`) asks for the date first
— the picker's *clear* makes the tasks undated, in Someday — and
`create-task-from-message edit` opens a single message's task for
editing instead; the two combine. `Ctrl-t` in the compose window does the same
for the message being written, once it is sent. The link records the RFC `Message-ID` plus
the subject, sender, date, account and last-known folder, so opening it years
later finds the message even after it has been moved, and shows the cached
details rather than an error if it is gone for good. The message list shows a
count of open tasks per message and the detail view lists them.

**Chat integration.** `t` in [chat](#chat) makes a task of a thread's newest
message (`Ctrl-t` asks for the date first), and a message's own *Make a task*
tool makes one of that message: a task links one message, not the whole
conversation. The thread leaves Waiting, as with done, and ⌘Z takes both
back. The task lists the message with its room and author, *Replied* once you
have written in its thread after it, and opening it goes to the message. The
message shows its tasks, and a thread with an open task carries the task mark
in its row.

**Sync.** Every edit is an APPEND followed by a delete, because IMAP messages
are immutable. Tasks are reconciled by their own UUID rather than by
`Message-ID`, and a task that appears in two messages resolves to the highest
revision. A crash between the append and the delete leaves a recoverable
duplicate, never a gap. Every change goes up on its own a second after the last
keystroke of a burst, without a full sync: only the changed tasks are sent.
Once a minute Orb asks the server whether another machine changed the folder,
and reads it in full only if one did. Edits made offline queue and upload in
revision order on reconnect. The per-task log is the one part exempt from
last-writer-wins: it is merged as a union, so two machines appending entries
lose neither.

**Saved or not.** A task with changes the server does not have yet is marked
with a dot in the list, and with *Not yet on the server* in its detail. The
*Tasks* heading in the sidebar shows how many changes are still on their way,
and says *saved* when none are. While the tasks folder cannot be synced, the
heading carries a warning instead, the status line counts the changes kept on
this machine only, and the tasks account is marked in the sidebar and in
`account-status` with the reason.

**Attachments.** A task's attachments live in messages of their own, so editing
a task never re-uploads a payload. Opening one downloads it if this machine has
not seen it, caches it under `~/.orb/attachments/tasks/`, and hands it to the
system — except for the file types macOS would *run* rather than display, which
are revealed in Finder so that opening them stays a decision.

**Trash.** Deleting a task is a soft delete that syncs; trashed tasks are
expunged after 30 days or immediately via empty-trash. Deleting a recurring task
deletes the series. A purge records what it removed until the messages behind it
are expunged, so emptying the trash offline cannot bring it back on the next
sync. A task abandoned before it is ever named — `o` and then escape — is erased
rather than trashed, on every machine.

**On an iPhone.** Orb for iPhone (`ios/`) opens the same tasks folder and
syncs it by the same rules. `set-up-iphone` (Tasks ▸ Set Up iPhone…) shows
the code it sets itself up from: the tasks account, its folder, the date
language and the Today settings, and the account's password, obtained as any
login obtains it. The code is as sensitive as the password, and says so; each
works for five minutes, a new one replaces it while the dialog stays open, and
Escape closes it.

On the iPhone, *Select* in a list's ⋯ menu picks several tasks. The bar
below completes them, or reopens them when all are completed, and postpones,
merges or deletes them. Dragging one of the selected tasks moves them all, as
one block.

## Reading

The reading list is a place for mail you want to read later but not now,
such as newsletters, and for the items of the RSS, Atom and JSON feeds you
subscribe to. It needs a [`reading` section](#reading-optional) in the
config. The sidebar shows it under **Reading**, below the tasks, in three
views:

- **To read** is the reading folder. Its badge counts the unread items.
- **Must read** shows the flagged items in the reading folder.
- **Archive** is the archive folder, which holds what you have read or put
  aside.

Feeds are polled in the background, each one every `poll_minutes`.
Mailbox ▸ Get New Feed Items (`poll-feeds`) asks all of them at once.
Mailbox ▸ Feed Status (`feed-status`) shows when each feed was last asked, and
why one could not be read. A feed item starts with a link to its article,
and `gx` opens the article in the browser.

`b` (`read-later`, Message ▸ Read Later) sets the message under the cursor,
or the selection, aside in the reading folder. Within the reading list's own
account this is a move, and ⌘Z takes it back. From another account the
message is copied over and then deleted, as Move to Another Account does, and
⌘Z cannot take it back. A [filter rule](#filter-rules-optional) with
`reading: true` does the same as mail arrives.

In the reading list the usual keys keep their meaning:

- `!` marks an item must read, or unmarks it.
- `a` files it in the reading archive. In the archive, `a` puts it back in
  the reading folder.
- `u` marks it read or unread.

An item's state is plain IMAP: unread, `\Flagged`, or in the archive
folder. Every machine, and every other mail client, sees the same list.

Today can show the must-read or the new items below its tasks: see
*Reading in Today* under [Tasks](#tasks).

## Chat

With a [`chat` section](#chat-optional), the sidebar ends with a Chat group,
and Waiting in it lists the threads that wait for an answer, newest first. A
thread is one row: who wrote the newest message, the room (`#name`, `Direct`,
or the people of a group message), the thread's first message as its title and
the newest message as its preview. A direct message's or a room's messages
outside any thread count as one thread. `@` in the status column marks a thread
that names you since you last wrote in it; the dot, one you have not looked at
since its newest message.

**Rooms.** Under Waiting, *Direct* and *Channels* list the rooms you are in,
folded until opened, each with how many of its threads wait. A room opens on
all its threads, not only those that wait: its own timeline first (*In the
room*), then the threads by their newest message, the arrow marking those
you answered last. A room with no threads yet, as most direct messages are,
shows its conversation alone, without the list of its one row; the list
comes back once a thread starts. Opening a room puts the keyboard in its
composer, and so do double-clicking a thread — in Waiting, a room, Today's
Chat or its timeline — and opening one from a notification, so the first key
typed is already in the text; `Esc` leaves it. A single click, and `j` and
`k`, leave the keyboard with the list, so `a`, `t` or `u` act on the thread
clicked; `r`, `i` or `Enter` then write in it. A click with `⇧` or `⌘` selects.
`c` writes to the room itself, outside any thread — from Waiting too, where it
opens the thread's room first.

A direct message with one person shows how they stand instead of an icon:
green online, amber away, red busy, a ring offline.

Any message of a room's own timeline can start a thread: *Reply in a thread*
among the tools of the message under the pointer, or `R` for the newest one
somebody else wrote. The composer then says which message it answers, and
its × writes to the room instead. On a server with threads turned off, which
Orb asks the server about when it connects, neither is offered.

Leave Room… in a thread's menu, or `:chat-leave-room`, leaves the room after
asking; a direct message is hidden instead, and comes back when either of you
writes. Rocket.Chat does not let a room's last owner leave it, and says so.
Go ▸ New Chat Room… (`:chat-create-room name`, `private` for a private group)
creates a room and opens it.

`gr` (Go ▸ Open Chat Room…) finds a room or a person by name. A room opens;
a person opens your direct messages with them, or starts them; a public room
you are not in is joined, then opened.

`gn` (Go ▸ New Chat Message…, `C` in chat) starts a chat with the people you
pick: type a name, `Enter` adds the person, and the field is ready for the
next; `Backspace` on the empty field takes the last one back. `Enter` on the
empty field, or `⌘Enter` at any time, opens the chat — a direct message with
one person, one conversation with several — with the keyboard in its
composer. A chat with the same people that exists already is opened rather
than made again.

A thread waits when its newest message is somebody else's. It leaves Waiting
when you answer — in Orb or anywhere else — or with done (`a`), and comes back
when someone writes again. With a [tasks account](#tasks-optional), done and
task marks reach your other Macs through it, as the tags do; without one they
stay on this Mac. Every room you are in counts, except those the config
sets to `waiting: false`.

Reading a thread in Orb marks nothing read in Rocket.Chat, so what you have not
dealt with stays unread there too. Answering and done mark it read on the
server, so the phone stops showing it, and so does `u`, which marks a read
thread unread again as well.

The reader shows the conversation oldest first and opens at the newest
message, with a line above what came since you last wrote, the files and
reactions of each message, and the composer under it. A long thread shows its
first message and its newest replies, from the one before what is new; the
replies between fold away (*N earlier replies*, or `za`). A quoted message
shows above the text that quotes it, with its author; a click on it goes to
it when it is in the conversation. A file opens with its default application
on a click — fetched from the server the first time, revealed in Finder
instead when opening it would run it. An image shows as a preview, at most
320 × 240 and in its own shape, from the thumbnail the server made of it; a
click opens the image itself. It opens with the 200
newest messages; scrolled to the top, or with a click on *Earlier messages*, it
brings in the next hundred, from Orb's database and then from the server, as
far back as the room goes. A thread reply can also be shown in
the room (the box beside Send). Orb keeps the chat server's messages in its
database: new ones arrive over the server's realtime connection, and a full
check runs every few minutes as well — every minute while that connection is
down, which the status line says. The status line also says when chat is signed
out or the server cannot be reached; mail carries on either way.

**Notifications.** A thread that starts waiting is announced: who wrote, the
room, and the text — several at once as one count. A click opens the thread,
in the chat window when one is open. What waits when Orb starts is not
announced again. A room with `notify: false` stays quiet, and so does one
whose notifications Rocket.Chat itself has off, or set to mentions where the
thread does not name you. The Dock badge counts mail only, unless
`chat.badge: true` adds the threads in Waiting.

**The chat window.** Window ▸ Chat Window (`open-chat-window`) opens a window
that shows chat only, to keep open on a second screen. Its sidebar is Waiting,
Direct and Channels; its list has two lines a thread — who wrote last and
when, then the room and the text — and sits beside the conversation while the
window is at least 900 px wide, with a line between them to drag, below it
when narrower. Both windows show the same Waiting: a reply, a done or a task
in one shows in the other at once. Keys that would go to mail, tasks or
reading say so and do nothing; the menu bar's chat items act on the chat
window while it is in front. It keeps its size and place across launches.

**Writing.** In the composer, `@` completes people — those of the
conversation at once, anyone on the server from two letters on — `#` rooms,
and `:` emoji from two letters on; `Tab`
or `Enter` takes the suggestion, `↑` and `↓` choose, `Esc` closes the list.
Files come from `⌘U`, from the clipboard, or dropped on the window, and show
above the text until sent; their × takes one back. A message under the pointer
offers its tools beside it: the configured reactions, each a click away (a
lit one is yours, and a click takes it back), then reply in a thread, make a
[task](#tasks) of it, and — for your own messages — edit and delete, which asks first and removes the message for everyone. A reaction
under a message joins it with a click, or takes yours back.

## Project layout

- `src/` – The Rust `orb-core` library: IMAP sync, SQLite cache/FTS, SMTP send,
  attachments, config parsing, and the API the app is built on (`src/ffi.rs`).
- `app/` – The Tauri app. `app/src-tauri` holds the Rust commands, which are
  thin shims over `orb-core`; `app/src` holds the frontend.
- `app/harness.html` – Drives the frontend against a stubbed backend, for
  testing the message list at sizes no real mailbox reaches.
- `tests/` – Rust integration tests.

## License

MIT License — Copyright (c) 2025 sudoremo

See [LICENSE](LICENSE) for details.
