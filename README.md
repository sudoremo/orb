# Orb

A keyboard-driven mail client for IMAP, with a task tracker and a reading
list that sync over IMAP too, and a Rocket.Chat client beside them. Orb is a
[Tauri](https://tauri.app) app on a Rust core (`orb-core`). The keys are
vim-style and yours to change in `config.yml`; the body of a message is
written in your own Neovim, embedded in the compose window.

> [!WARNING]
> Orb is in early development. Expect bugs. Keep backups of your mail and try
> it on a non-critical account first.

## Features

- **Mail from any IMAP server.** Several accounts in one sidebar, TLS or
  STARTTLS, password or OAuth 2.0, push with IDLE, a sync every few minutes,
  and a sync that works without CONDSTORE. Folders can be created, renamed,
  deleted and subscribed from the app. See [Folders](#folders).
- **Fast and offline.** Everything is cached in SQLite. Recent mail opens
  without the server, reading never waits on it, and a change made offline
  reaches the server with the next sync. See [Offline](#offline).
- **Sending.** Compose, reply, reply-all and forward, with several identities
  per account, attachments, templates, checks before sending, an outbox for
  what cannot go out yet, and a follow-up task made when a message is sent.
  See [Composing](#composing).
- **Rules.** Filter rules act on mail as it arrives. Highlight rules colour,
  mark and badge messages. Subject rewriting shortens what tools send.
  Message links open the issue or build a message is about. See
  [Filter rules](#filter-rules).
- **Conversations.** A folder as threads, a thread opened in place, and any
  message's whole conversation gathered from every folder. See
  [Conversations](#conversations).
- **Search.** Full-text search over every account and every task, with a
  small query language, and search by meaning with a local model. A filter
  narrows the list on screen as you type. See [Search](#search).
- **Safe rendering.** HTML mail in a sandbox with no scripts and no network.
  Remote images load only when asked, tracking pixels are removed, and
  forged senders, suspicious links and dangerous attachments are marked.
- **Staged actions.** Stage deletes and moves, review them, apply them as a
  batch. See [Staged actions](#staged-actions).
- **Undo.** One key takes back the last action, whatever it was and wherever
  it was done. See [Undo](#undo).
- **Tasks over IMAP.** A task list stored as messages in one folder, so every
  machine sees the same list. Dates, recurrence, tags, steps, reminders,
  attachments and a log per task. One key turns a message or a chat thread
  into a task. An iPhone app shares the folder. See [Tasks](#tasks).
- **A reading list.** Newsletters set aside with one key, and RSS, Atom and
  JSON feeds, in two IMAP folders. See [Reading](#reading).
- **Rocket.Chat.** The threads that wait for your answer, one row each, with
  the conversation and a composer in the reader. See [Chat](#chat).
- **Your keys.** Every key is a command, and the same commands back the
  command line, the command palette, the menu bar and the context menus.
  Counts, sequences and chains (`stage-read && mark-move Archive && advance`)
  work, and `?` shows every key in force. See [Keys](#keys).
- **Two looks.** Hybrid, with proportional text, or Terminal, monospace and
  dense in the manner of mutt. See [The window](#the-window).

## Requirements

- macOS 14 or later
- A Rust toolchain (edition 2024)
- Node with [pnpm](https://pnpm.io)
- Neovim on the `PATH`, for composing. Without it the body is written in a
  plain text field.

Orb takes the `PATH` from your login shell, so tools installed by Homebrew
are found whether Orb starts from a terminal or from the Dock.

## Build and run

```bash
./start.sh            # build a debug bundle and launch it
./start.sh --release  # the same, optimised
./start.sh --dev      # the bare binary, with the frontend reloading on save
cd app && pnpm tauri build   # a release bundle
```

`start.sh` signs the bundle with a Developer ID or Apple Development
certificate when the machine has one, so it keeps the permissions macOS
granted the installed Orb. Signed ad hoc, every build is a new app to macOS
and is asked for Accessibility and Contacts again. Only a bundle can post
notifications: under `--dev` nothing is announced and no reminder fires.

The Rust core is a path dependency of `app/src-tauri`, so there is no
separate build step. `cargo test` in the repository root runs its tests.

### The UI without a mailbox

`app/harness.html` runs the frontend against a stubbed backend and a
synthetic folder of any size:

```bash
cd app && pnpm dev     # then open http://localhost:1420/harness.html?total=200000
```

### Releasing

A tag `vX.Y.Z` on GitHub builds the macOS app as a universal binary and
attaches the DMG to a draft release for the tag (`.github/workflows/release.yml`);
publish the draft once you have looked at it. The tag has to match the version
in `app/src-tauri/tauri.conf.json` and `app/src-tauri/Cargo.toml`, so bump
those first:

```bash
git tag v0.2.0 && git push origin v0.2.0
```

The bundle is signed with a Developer ID certificate and notarized when the
repository holds these secrets, best kept in a GitHub environment named
`release` whose deployment branches are restricted to `v*` tags:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | The *Developer ID Application* certificate with its private key, exported from Keychain Access as `.p12` and encoded with `openssl base64 -A -in certificate.p12`. |
| `APPLE_CERTIFICATE_PASSWORD` | The password given when exporting it. |
| `APPLE_SIGNING_IDENTITY` | Its name, `Developer ID Application: Your Name (TEAMID)`. |
| `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_CONTENT` | An App Store Connect API key with the Developer role (Users and Access ▸ Integrations): the issuer ID, the key ID and the contents of the downloaded `.p8` file. Used for notarization; without them the bundle is signed but not notarized. |

Without `APPLE_CERTIFICATE` the workflow still runs and signs the bundle ad
hoc, so a fork builds without an Apple account. The same workflow run by hand
(Actions ▸ Release ▸ Run workflow) builds the bundle as a workflow artifact
without creating a release.

The iOS app goes to TestFlight through Xcode Cloud, which builds and signs on
Apple's side, so no certificate or profile leaves Apple. The checkout holds
neither the Xcode project nor OrbKit's binary; `ios/ci_scripts/ci_post_clone.sh`
installs Rust and `xcodegen` and generates both before the build, and checks
that the tag matches `MARKETING_VERSION` in `ios/project.yml`. To set it up
once, open `ios/Orb.xcodeproj` in Xcode (Product ▸ Xcode Cloud ▸ Create
Workflow), connect the GitHub repository, and give the workflow:

- a start condition on tags matching `v*`;
- an Archive action for iOS with TestFlight (Internal Testing) as its
  post-action;
- the environment variable `ORB_DEVELOPMENT_TEAM` with your team ID, which the
  script writes to `ios/Local.xcconfig` on the build machine.

## Configuration

Orb reads **`~/.config/orb/config.yml`**. On a launch that finds no such
file, it writes a template with the required keys, opens it in your editor
and quits. Set `ORB_CONFIG` to read another file instead, for example a test
account with a database of its own:

```bash
ORB_CONFIG=~/orb-test/config.yml ./start.sh
```

[`config.example.yml`](config.example.yml) shows every key with its default.
A key Orb does not know is an error at launch that names the key and the
line, in a dialog when started from the Dock and on stderr when started from
a terminal.

Most values can be written bare. Quote a key or a value that begins with one
of `! & * # [ ] { } | > % @ ~ ' "` or a backtick, that is `-` or `?` followed
by a space, or that contains `: ` or ` #`. Bare `~` and `null` are YAML's
null, so a binding on `~` is written `"~"`. Single quotes keep a backslash a
backslash, which regular expressions want.

### Changes while Orb runs

The file is re-read within a couple of seconds of being saved, and the status
line says `config.yml reloaded`. Most keys take effect at once. These need a
restart, and the status line names them when they change:

- `db_path`, `log_file`, `embedding_model`;
- `tasks.account`, `tasks.folder` and `tasks.locale`;
- `chat.server`, `chat.sign_in` and `chat.token_command`;
- adding, removing or renaming an account;
- an account's server, port, username, password command, encryption, auth
  method, `sync_interval_seconds`, or any folder's `sync` or `watch` flag.

A saved file that does not parse changes nothing: the status line carries the
error and the previous configuration stays in force.

### Minimal configuration

```yaml
db_path: ~/.local/share/orb/mail.db   # the local cache; ~ is expanded

accounts:
  - name: Personal
    imap_server: imap.example.com
    imap_port: 993
    imap_username: user@example.com
    imap_password_command: pass email/example.com
    from: Your Name <user@example.com>
```

### Accounts

Each entry in `accounts` takes:

| Key | Required | Description |
|-----|----------|-------------|
| `name` | yes | Display name; what other sections refer to the account by. |
| `imap_server`, `imap_port` | yes | IMAP host and port (usually `993`). |
| `imap_username` | yes | IMAP login. |
| `imap_password_command` | no | Shell command that prints the password. Without one, see [Passwords](#passwords). |
| `imap_encryption` | no | `tls` (default, port 993) or `starttls` (a plain connection, usually port 143, upgraded before the login). |
| `imap_auth` | no | `password` (default) or `xoauth2`; see [OAuth 2.0](#oauth-20). |
| `from` | yes | The address outgoing mail is sent as, e.g. `Name <user@example.com>`. |
| `smtp_server` | no | Defaults to `imap_server`. |
| `smtp_port` | no | Default `587`. |
| `smtp_encryption` | no | `starttls` or `tls`. Unset, the port decides: `465` is `tls`, anything else `starttls`. |
| `smtp_auth` | no | `password` (default) or `xoauth2`. |
| `smtp_username` | no | Defaults to `imap_username`. |
| `smtp_password_command` | no | Defaults to `imap_password_command`. Without either, the IMAP password serves the same login and another login is asked for. |
| `signature_file` | no | Signature of the account's own address. The `-- ` line is added unless the file starts with one. |
| `identities` | no | Further addresses the account sends as; see [Identities](#identities). |
| `domains` | no | Recipient domains written to as the account's own address; see [Checks before sending](#checks-before-sending). |
| `drafts_folder`, `sent_folder`, `trash_folder` | no | Defaults `Drafts`, `Sent`, `Trash`, or the folder the server marks for the role; see [Folders](#folders). |
| `archive_folder` | no | Where `archive` files a message. Default `Archive`, or the server's `\Archive`. |
| `junk_folder` | no | Where `spam` files a message and `not-spam` takes it from. Default `Junk`, or the server's `\Junk`. |
| `default_sort` | no | `date_newest` (default), `date_oldest`, `subject_asc`, `subject_desc`, `sender_asc`, `sender_desc`. |
| `folder_order` | no | Folders listed first, in this order; the rest follow alphabetically. |
| `folders` | no | Per-folder settings, below. |
| `sync_interval_seconds` | no | How often every synced folder is synced whether or not the server said anything. Default `300`. |
| `threads` | no | Open the account's folders as [conversations](#conversations). Default `false`; a folder's own `threads` overrides it. |

A folder opens in its own `sort`, else in the account's `default_sort`. The
toolbar's sort menu changes the order for as long as the folder stays open.

Per-folder settings go under `folders`, keyed by the server-side name:

```yaml
    folders:
      INBOX:
        visible: true          # in the sidebar (default true)
        sync: true             # fetched from IMAP (default true)
        show_unread: true      # unread badge (default true)
        show_total: false      # total badge (default false)
        display_name: Inbox    # the server name otherwise
        sort: date_newest      # overrides default_sort
      Sent:
        columns: [meta, to, subject, date]   # this folder's own columns; see list.columns
      Work:
        watch: true            # watch with IMAP IDLE too (default false)
        threads: true          # open as conversations (default: the account's threads)
```

INBOX is always watched with IDLE. A server allows only a few IDLE
connections per login, so at most two more folders per account are watched:
the first two marked `watch`, in alphabetical order. The rest are named in
the log and covered by the periodic sync.

### Identities

An account's `from` and `signature_file` are its default identity, named like
the account. Further identities go under the account:

```yaml
accounts:
  - name: Work
    from: Sam Example <sam@example.com>
    signature_file: ~/.config/orb/signatures/work.txt
    identities:
      - name: Support
        from: Example Support <support@example.com>
        signature_file: ~/.config/orb/signatures/support.txt
        reply_to: support@example.com
        addresses: [help@example.com]        # also select this identity
        domains: [customer.example]          # recipients it writes to
      - name: Club
        from: Sam <sam@club.example>         # forwarded here, sent there
        smtp_server: smtp.club.example
        smtp_username: sam@club.example
        smtp_password_command: pass club
      - name: Short
        from: Sam <sam@example.com>
        signature: |                         # inline, instead of a file
          Sam
```

| Key | Required | Description |
|-----|----------|-------------|
| `name` | yes | Shown in the compose window. Unique within the account, and not the account's name. |
| `from` | yes | The `From` of mail written as this identity. |
| `signature_file`, `signature` | no | Its signature, from a file or inline. One of the two. |
| `reply_to` | no | Its `Reply-To`. |
| `addresses` | no | Further addresses that select this identity. |
| `domains` | no | Recipient domains written to as this identity. |
| `smtp_server` | no | A server of its own, which mail written as it is sent over. |
| `smtp_username` | with `smtp_server` | Its login. |
| `smtp_port`, `smtp_encryption`, `smtp_auth` | no | As for the account, with the same defaults. |
| `smtp_password_command` | no | Prints its password. Without one it is looked up in the Keychain or asked for, never taken from the account. |

A reply or forward is written as the identity the original was sent from, or
else as the first identity it was addressed to (`To`, then `Cc`, then
`Delivered-To`), otherwise as the default identity. A new message starts as
the default identity. Reply-all leaves out every address of every identity.

An identity changes `From`, `Reply-To` and the signature. The message is sent
over the account's SMTP login, filed in the account's Sent folder, and its
Message-ID carries the account's domain. An identity with an `smtp_server` of
its own is sent over that server and carries its own domain; drafts and the
Sent copy still go to the account. A message queued while offline goes out
over the same server.

In the compose window, `⌘I` (or a click on the From line) lists every
identity of every account; type to narrow, `Enter` picks. The signature is
replaced with the new identity's, unless it was edited or belongs to another
identity. An identity of another account moves the draft to that account's
Drafts folder and sends over that account. A reopened draft comes back as the
identity it was saved as.

### Checks before sending

Each finding is a **Check** line under the header while you write. Send asks
whether to send anyway or go back:

- A reply or forward written as an identity the original was not sent to,
  while it was sent to another one.
- A reply or forward of mail delivered (`Delivered-To`) to an address no
  identity owns, such as an alias or a catch-all. The reply goes out from the
  default identity; add the address to an identity's `addresses` to change
  that.
- A recipient in the `domains` of another identity than the one written as.
  A domain covers its subdomains.
- A placeholder of a [template](#templates) still in braces. This one asks
  the other way round: `Enter` goes back to fill it in, `⌥⌘Enter` sends
  anyway.
- A mention of an attachment in your own text (quote, signature and forwarded
  message aside) when nothing is attached. The words come from
  `compose.attachment_words`, each matched at the start of a word:

```yaml
compose:
  # The default: English, German, French and Italian. [] turns the check off.
  attachment_words: [attach, enclos, anbei, anhang, anhäng, angehängt,
    beigefügt, beiliegend, ci-joint, pièce jointe, pièces jointes, allegat]
```

### Global settings

```yaml
db_path: ~/.local/share/orb/mail.db     # required
log_file: ~/.local/share/orb/orb.log    # optional; rotated at 10 MB, the previous one kept as .1

# Which rendering a message opens in when it has both: html (default) or
# plain. The reader's toggle still switches for the message on screen.
default_message_view: html

appearance:
  theme: system          # or dark, or light
  font_size: 13          # pixels; ⌘+ and ⌘- change it on this machine, ⌘0 comes back
  editor_font_size: 15   # Neovim in the compose window; unset follows font_size
  dark_html_mail: true   # draw HTML mail dark in a dark window; toggle-dark-mail switches
  look: hybrid           # or terminal; see The window

# The model directory for `~` search by meaning. Unset, semantic search is
# off unless ORB_EMBEDDING_MODEL in the environment names one.
embedding_model: ~/.config/orb/model

reply:
  quote: true                                  # include the original
  prefix: "> "
  attribution: "On {date}, {sender} wrote:"    # empty for none
  signature: below                             # or above: before the quote

# Senders whose remote images load without asking: addresses, or domains
# (with or without @) covering every address at them and their subdomains.
load_remote_images_from:
  - newsletter@example.com
  - shop.example

# Messages whose remote images load without asking: a highlight rule's
# `when`, with at least one condition. `name` is optional, `accounts`
# defaults to every account.
load_remote_images:
  - name: Shop newsletter
    when: { list_id: "*.shop.example" }
  - accounts: [Work]
    when: { from: "*@reports.example.com", subject: "Weekly report*" }

# Seconds a message must stay on screen before it is marked read. Moving the
# cursor cancels it; 0 never marks automatically (`stage-read` still works).
mark_read_after_seconds: 2.0

# Dock badges, up to four, one per corner (top right, top left, bottom
# right, bottom left): the unread mail in `folder` (default INBOX) of
# `account`, or of every account without one, in `color` (default red).
# Without this, one badge counts the unread mail in every inbox.
dock_badges:
  - account: Private
  - account: Work
    folder: INBOX
    color: "#0a84ff"

# Where Ctrl-h/j/k/l go once Orb has nothing to move to that way: run with
# left, right, up or down. A path, not a shell line; ~ is expanded.
navigation:
  escalate_command: ~/bin/navigate

list:
  columns: [meta, tasks, from, subject, date]  # `to` for recipients
  date_format: auto                            # or strftime-style; default theme.date_format
  density: comfortable                         # or compact, or spacious

search:
  sort: relevance         # or date_newest, date_oldest, subject_asc, subject_desc,
                          # sender_asc, sender_desc

theme:
  date_format: auto
  column_widths:
    from: auto            # or a number of characters
    from_min: 12          # bounds of auto
    from_max: 30
    date: 14
```

`theme.colors`, `theme.icons`, `theme.borders` and `theme.use_nerd_fonts`
are parsed but not used.

**Columns.** Every message is one line, laid out the same whatever order
`columns` names: a status column (unread dot, `✕` for a staged delete, an
arrow once answered or forwarded), sender, recipients, subject with the
opening of the text after it, badges, date. `meta` is the status column and
the badges (the folder of a search hit, highlight icons and labels, a staged
move's target, the attachment clip, the flag); `tasks` is the badge of the
message's open tasks. The sender shows name and address; when room runs out
the name gives way, never the address. A sender whose name is itself another
address is drawn with the real address in red behind a warning mark. A folder
can have its own `columns`, `to` instead of `from` for Sent, say.

**Dates.** `auto` shows a date as fully as the column has room for: the time
today, `Yesterday` or the weekday this week, day and month this year, and the
year otherwise (`14:22`, `Mon 09:10`, `4 Nov 14:22`, `4 Nov 2025`). A narrow
column drops the time first and never the year. A strftime format takes
`%Y %y %m %d %e %H %I %p %M %S %b %B %a %A %j %F %R %T`, with `-` after the
`%` for no padding.

**Density.** Rows are 20, 24 or 30 px in the Hybrid look and 20, 24 or 28 in
the Terminal look, at the default text size.

**Search order.** `search.sort` is the order a search's mail starts in until
the toolbar or `sort-by` picks another. A search by meaning (`~`) is always
closest first.

**Escalation.** With `navigation.escalate_command` set, a `Ctrl-h/J/K/L` that
Orb has no pane for in that direction is handed to the command: in the
mailbox, from the last pane that way; in the compose window, `Ctrl-h` and
`Ctrl-l` outside Neovim and `Ctrl-j`/`Ctrl-k` past the bottom or top field.
Neovim's own mappings see `Ctrl-h` and `Ctrl-l` first. The command runs as
Orb's child, so it uses Orb's Accessibility permission, which Orb asks for at
launch while the command is set. Started from a terminal, Orb drops `TMUX`,
`TMUX_PANE` and `HYTERM_SOCK` from its environment.

### Filter rules

Rules run on mail as it arrives in a synced folder, once per message, on the
first Orb that syncs it. They work beside server-side rules (Sieve), not in
their place: a rule only runs while an Orb is running.

```yaml
filters:
  - name: Newsletters
    accounts: [Work]             # default: every account
    folders: [INBOX]             # default: INBOX
    when:                        # every condition must hold; two rules for OR
      from: "*@news.example.com" # glob on the address, case-insensitive
      subject: "re:^\\[news\\]"  # re: selects a regular expression
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
      task: false                # a task linked to the message
      stop: true                 # no later rule sees this message
```

**Conditions.** A rule without `when` matches every message. `from`, `to`
and `cc` match the address of any entry in the field; with a `name:` prefix
(`from: "name:Anna *"`) they match the display name. `subject`, `list_id` and
each `header` match the decoded header text; `list_id` also matches the
identifier alone (`list.example.com` out of `The List <list.example.com>`).
A header the message does not have matches nothing, not even `*`; a missing
subject is the empty subject, so `subject: "*"` matches it. A glob matches
the whole value and ignores case; `*` is any run of characters and `?` one. A
pattern after `re:` is found anywhere in the value and is case-sensitive
unless it says `(?i)`. Quote a value YAML would read as something else
(`YES`, `no`, `1.0`).

**Actions**, in this order: `read` and `flag`; then one of `move`, `reading`
or `delete` (the message keeps its flags); then `task`. `move` names a folder
of the account; one the server keeps under `INBOX.` or `INBOX/` is found
there. `reading` moves the message to the [reading list](#reading), from
another account by copy and delete. `delete` moves to Trash and does nothing
inside Trash. `task` creates a task titled with the subject and linked to the
message, unless one is linked already; a message without a `Message-ID` gets
no task. After a rule with `stop`, or after a move or delete, no later rule
sees the message.

**When they run.** After every sync of a folder, on the messages that arrived
since the last sync. Mail that was in a folder before Orb first synced it is
never filtered, and a message that comes back to the folder is left where it
was put. Two Orbs sharing a database never run the rules on the same mail,
and two on separate machines converge; a rule with `task` and no
`move` can make a task on each when both sync the same moment. `:filter` runs
the rules on the selection, or on the whole folder (in Today, on every inbox
it shows), and `:filter --all` on every folder a rule watches, mail the rules
have already seen included.

**What they did.** Every message a rule acted on is a line in the log, and
the review panel (`p`) lists what the rules did in each account's last sync.
A rule that does not parse, a `move` to a folder the account does not have,
and a `task` without a `tasks` section are left out and named in the status
line at launch.

### Highlight rules

A highlight changes how a message looks without touching it on the server.
It is evaluated as the row is drawn, so it applies to every message at once,
and a changed rule shows after the reload.

```yaml
highlights:
  - name: Boss
    accounts: [Work]                  # default: every account
    when:                             # a filter's `when`, plus the keys below
      from: anna@example.com
    style:
      color: "#e6b422"                # sender and subject
      icon: fa-crown                  # a Font Awesome icon the app bundles

  - name: Invoices
    when:
      any:                            # OR: at least one of these
        - subject: "re:(?i)invoice|rechnung"
        - has_attachment: true
          from: "*@billing.example.com"
      body: payment due               # the body contains this, once fetched
    style:
      background: rgba(76, 141, 255, 0.10)
      label: invoice                  # a chip after the subject
      label_color: "#4c8dff"
      bold: true

  - name: Newsletters
    when: { list_id: "*" }
    style: { dim: true }
```

**Conditions.** Everything a [filter](#filter-rules)'s `when` accepts, plus
`any` (a list of condition sets of which one must hold, besides the rest of
the `when`), `unread`, `flagged`, `folder` (a glob or `re:` on the folder
name), `older_than` and `newer_than` (`30m`, `12h`, `7d`, `2w`, against the
message's date) and `body` (text the body contains, ignoring case). `body` is
false until the body has been fetched. `list_id` is fetched with every
message; a header named with `header:` is fetched from the moment a rule
names it, and the backfill fetches it for the mail already here, newest
first.

**Style.** `color`, `background` (kept at low alpha so cursor and selection
still read), `icon` and `icon_color`, `label` and `label_color`, `bold`,
`italic`, `dim`, and `sidebar_badge: true` to count the rule's unread
messages beside each folder in the rule's colour. A colour is `#rgb`,
`#rrggbb`, `#rrggbbaa`, `rgb()`, `rgba()` or one of `accent`, `warning`,
`danger`, `success`, `muted`. A background that contrasts too little with the
text is adjusted until it reads, and the status line says so at launch. An
icon is a Font Awesome name, with or without `fa-`; an unknown name is a
warning.

Where several rules match, the first sets colour and background, and every
rule contributes its icon and label, in order, up to three of each. On the
cursor's row the colour is lightened just enough to read on the cursor's fill.
The
reader repeats icons and chips beside the subject; search results and Today
draw highlights as folders do. `:highlights` lists the rules with how many
messages of the current folder each matches. A rule that does not parse is
left out and named in the status line at launch.

### Subject rewriting

A rule changes how a subject reads in the message list, without touching the
message: search, sorting and a reply's `Re:` use the original, and the reader
shows it in full, with the rules' icons beside it.

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

Rules run in order, each on what the one before left, and every matching rule
contributes its icon once. `replacement` may be left out. An icon is a Font
Awesome name, with or without `fa-`, brand icons included; a Nerd Font name
of the Font Awesome set (`nf-fa-…`) is read as the same icon. `icon_color`
takes the colours a [highlight](#highlight-rules) does. A pattern that does
not compile is left out, and a colour that does not parse leaves the icon in
the default colour; both are warnings in the log.

### Message links

A rule turns a message into a web address: the issue a tracker's notification
is about, the build a CI mail reports. `gx` (`open-link`) opens the address
of the first matching rule in the browser.

```yaml
message_links:
  - name: issue                         # optional; `open-link issue` picks this rule
    pattern: 'Issue #(\d+)'             # a regular expression
    url: 'https://tracker.example.com/go/$1'   # $1, $2 … are the capture groups
  - name: ci-run
    match: body                         # subject (the default) or body
    from: '*@ci.example.com'            # optional: only mail from this sender
    pattern: 'https://ci\.example\.com/\S+/runs/\d+'   # no url: the match itself
```

A rule matches the subject as it arrived, before any [subject
rewriting](#subject-rewriting). With `match: body` it reads the plain-text
body, or the HTML one when there is no plain text; a body not downloaded yet
does not match. `from` is an address pattern as in a [filter](#filter-rules).
Without `url` the rule opens what the pattern matched. Only `http`, `https`
and `mailto` addresses are opened; a message no rule matches says so in the
status line.

On a task, `gx` opens the first of these that exists:

1. what it would open on the task's first linked message;
2. a web address on the first line of its notes, or in its title;
3. what a rule without `from` and `match: body` finds in its title.

### Configuring tasks

Tasks are off until one account is named to hold them:

```yaml
tasks:
  account: Work      # the entry in accounts whose IMAP folder holds the tasks
  folder: Tasks      # server-side name, used verbatim; default Tasks
  locale: en         # date-picker language: en or de
  keep_completed_in_today: true    # completed tasks stay in Today until midnight
  completed_first_in_today: false  # list them above the open tasks instead
  archive_after_days: 30           # completed longer ago moves to Archived; 0 never
  export:                          # optional: snapshots of every task
    directory: ~/Backups/orb-tasks
    interval_hours: 24
    keep: 30
```

The folder is created on the first sync and resolved against the server's
namespace, so a server that keeps everything under `INBOX` gets `INBOX.Tasks`
on its own. Do not write a leading `.`: on Dovecot that creates a folder
literally called `.Tasks`. Zimbra and other groupware servers have a built-in
`Tasks` folder that IMAP cannot open; pick another name there, such as
`Orb Tasks`.

The account stays an ordinary mail account. Only the tasks folder leaves the
mail world: it is not in the sidebar, not a move target, not synced as mail,
not searched, not counted and never announced.

`locale` sets the month and weekday names the date picker reads (`morgen`,
`Dienstag`, `Sept`) and the order of numeric dates. It is a setting rather
than a guess from `LANG`, so a date string means the same day on every
launch. See [Tasks](#tasks) for what the tasks do.

### Configuring the reading list

There is no reading list until one account is named to hold it:

```yaml
reading:
  account: Private                 # the entry in accounts that holds the list
  folder: Reading                  # still to read; default Reading
  archive_folder: Reading Archive  # read; default Reading Archive
  feeds:                           # RSS, Atom and JSON feeds; optional
    - url: https://feeds.example.org/rust.xml
    - url: https://example.com/feed.xml
      name: Example                # the sender its items show; default the feed's title
  poll_minutes: 60                 # how often each feed is asked; at least 5
```

Both folders are created on the first sync and resolved against the server's
namespace, like the tasks folder. They are ordinary mail folders, synced and
searched, shown under **Reading** in the sidebar rather than among the
account's folders, and left out of the move pickers: `b` is how mail gets
into the reading list.

A [filter rule](#filter-rules) with `reading: true` sends mail there as it
arrives, from any account. Newsletters almost always set `List-Id`:

```yaml
filters:
  - name: Newsletters
    accounts: [Work, Private]
    when: { list_id: "*.newsletters.example" }
    then: { reading: true }
```

A feed's new items arrive in the reading folder as unread messages from the
feed, the title as subject and the content (or the summary) as body. A feed
polled for the first time brings its ten newest items. Every Mac polls, and an
item is added only once, by looking for it in both reading folders first; an
item you archived or deleted can come back only if another Mac polls the feed
for the first time later. Feed Status (`feed-status`) names a feed that cannot
be fetched or read. See [Reading](#reading) for the views.

### Configuring chat

Chat is off until a Rocket.Chat server is named:

```yaml
chat:
  server: https://chat.example.com   # https:// and the host, no path
  sign_in: sso                       # or token_command
  # token_command: …                 # prints a user id and a token, separated by whitespace
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
window the first time (Go ▸ Sign In to Chat…, or the button in an empty
Waiting) and keeps the session in the Keychain. This works with single sign-on,
which the server's API does not take directly. When the session ends, Waiting
says so and offers the sign-in again. With `sign_in: token_command` the command
prints a user id and a personal access token, for a server that allows those.

`waiting_days` bounds the first sync: older threads never start waiting.
`rooms` takes `waiting: false` for a room whose threads never need an answer
and `notify: false` for one that waits quietly. `reactions` are Rocket.Chat
shortcodes. In `links`, `pattern` is a JavaScript regular expression and `url`
the address a match opens, with `$1` to `$9` for its groups and `$0` for the
whole match; text already in a link or in code is left alone, and a pattern
that does not compile is named in the status line and skipped.

`rooms`, `reactions` and `links` apply when the file is saved; `server`,
`sign_in` and `token_command` need a restart. Muting a room needs no config:
see [Chat](#chat).

### Recipients

Addressing a message completes as you type, from everyone the mail already
knows about and from macOS Contacts, ranked together: someone you wrote to
outranks someone who only appeared in a header, and recently outranks long
ago. `↓` on an empty field offers the people you write to most. The address
book is built from the mail on the first run, in the background, and kept up
with new mail. macOS Contacts is asked for permission the first time
completions are needed; refusing costs only those entries, and a development
run skips them, since it has no bundle to carry the usage description.

Shortcuts of your own outrank both; a group inserts everyone in it:

```yaml
contacts:
  auto_extract: true   # learn from the mail (default); false uses only the
                       # entries below and the system address book
  aliases:
    - shortcut: ns
      email: nina.berger@example.com
      name: Nina Berger
  groups:
    - shortcut: team
      emails: [ada@example.com, grace@example.com]
      description: Development Team
```

### Views and saved searches

A view is a virtual folder in the sidebar, under **Views** between the task
views and the accounts. Opening one runs its query and shows the newest five
hundred hits as a [search](#search) does; `q` leaves it.

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
  - name: My Issues
    icon: fa-user-pen
    source: search
    query: in:Archive f:tracker@example.com header:X-Assigned-To-Me=true
```

A saved search is the one place a query can ask about a header. The sync
fetches a header from the moment a view names it, and the background backfill
fetches it for the mail already here, newest first. An open view updates as
mail arrives.

### Pinned

The top of the sidebar holds what `pinned` lists, in that order and under no
heading. `Ctrl-1` opens the first and `Ctrl-9` the ninth (`open-pinned <n>`),
from a text field too; while Ctrl is held, each pin shows its digit.

```yaml
pinned:
  - today                                         # a task view
  - { account: Work, folder: INBOX, display_name: Work }
  - { account: Private, folder: INBOX, display_name: Private }
  - waiting                                       # chat's Waiting
  - { view: All Inboxes }                         # one of virtual_folders, by name
```

A bare name pins a built-in view: `today`, `upcoming`, `someday`,
`completed`, `reading`, `must-read` and `waiting`. A pinned view leaves its
section; a pinned folder stays in its account too and is named by
`display_name` (`Account: Folder` without one). Without `pinned`, Today alone
is pinned; a list that leaves Today out puts it first under **Tasks**, and
`pinned: []` pins nothing. A pin of something the config does not have is
left out with a warning at launch. `pinned_folders` is the key's older name
and still reads.

`gp` or `⇧⌘P` (`toggle-pin`) pins what is open, or what the sidebar's cursor
is on, below the config's pins, and unpins it again. These pins number on
from the config's, are remembered on this Mac rather than written to
`config.yml`, and can be dragged into another order. A folder pinned this way
leaves its account's tree and is named `Account: Folder`; a room leaves Direct
or Channels. A pin of something gone, a room left or a folder deleted, is not
drawn until it is back.

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
earlier and otherwise asks: in the window when the account first connects,
and in the compose window at the first send when the SMTP login differs from
the IMAP one. The prompt offers to keep the answer in the Keychain, under the
service `Orb` and the login `username@server`. A password the server refuses
is forgotten and asked for again; a dismissed prompt fails that login until
the next sync asks again.

### OAuth 2.0

Gmail and Office 365 take an OAuth 2.0 access token in the password's place,
over SASL XOAUTH2. With `imap_auth: xoauth2` (and `smtp_auth: xoauth2` for
sending) the password command prints that token, and `imap_username` is the
address the token was issued for:

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

Getting the token is the command's job: `mutt_oauth2.py`, `oauth2ms` and the
like register an app with the provider, open the browser once, and print a
fresh access token from the refresh token they keep. Orb runs the command when
the account first connects, keeps the token for the session, and runs it
again when a login is refused, which is what an expired token looks like.

### Key bindings

The built-in keys (see [Keys](#keys)) are overridden under `bindings`, one
table per mode. A table is merged over the built-in one rather than replacing
it, so a config written before a command existed keeps working; bind a key to
`noop` to switch a default off.

| Table | Where it applies |
|-------|------------------|
| `index_normal` | The mail list, and the base of every other table |
| `index_detail` | Layered over `index_normal` while the reader has the keyboard; empty by default |
| `task_normal` | Task lists, layered over `index_normal` |
| `task_detail` | The task detail pane, layered over `task_normal` |
| `task_edit` | While a task's title or notes are being edited |
| `date_picker` | The date picker; bracketed keys only, since bare characters belong to its text input |
| `chat_normal` | The chat list and conversation, layered over `index_normal` |
| `chat_compose` | The chat composer while it has the keyboard |
| `compose_normal` | The compose window (see [Composing](#composing)) |
| `commands` | Chains with names of your own, see below |

`compose_edit`, `command`, `task_detail_terminal`, `compose_terminal` and
`message_detail_terminal` are parsed but not used.

A key is a bare character or a bracketed name: `<c-s>` is Ctrl-s, `<d-y>` ⌘Y,
`<d-s-y>` ⇧⌘Y, and `<enter>`, `<esc>`, `<space>`, `<tab>`, `<up>` and the
like as written. Keys chain into sequences (`gg`), take counts (`5j`), and
commands chain with `&&`:

```yaml
bindings:
  index_normal:
    <c-s>: sync
    "!": toggle-flag
    a: stage-read && mark-move Archive && advance
    # A task for the message, and the message filed, in one key.
    A: create-task-from-message && stage-read && mark-move Archive && advance
  date_picker:
    <c-t>: today
```

A chain is one action: a step that fails stops the rest, so the message is
not filed when its task could not be made or its date picker was dismissed.
The status line says what each step did, and one `u` takes the whole key back.

`commands` gives a chain a name, which then works wherever a command does: on
a key, on the command line (`:file-and-next`) and in the
[command palette](#command-line-palette-and-help) under "Your commands" with
its `title` (the name when there is none). One may name another; a built-in
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

## Keys

The lists take vim-style keys without a modifier. Every key runs a command,
and every command can also be typed after `:`, picked from the
[command palette](#command-line-palette-and-help), or found in the menu bar
and the right-click menu. A count works where it makes sense (`5j`), and `?`
lays every key in force over the window. The tables below are the defaults;
[Key bindings](#key-bindings) says how to change them.

### Mail keys

Active in a folder, a search, a view, and Today's mail. Mode-specific tables
(tasks, chat) are layered over this one, so what they do not name still works
there.

| Key | Action | Command |
|-----|--------|---------|
| `j` / `k`, `↓` / `↑` | Next / previous message | `next-message`, `prev-message` |
| `gg` / `G` | First / last message | `top`, `bottom` |
| `Ctrl-d` / `Ctrl-u` | 10 messages down / up | `next-page`, `prev-page` |
| `Ctrl-o` | Back to where the last [jump to a message](#jumping-to-a-message) was made from | `jump-back` |
| `Enter`, `l` | Open the message in the reader | `open-message-detail` |
| `h` | Back: from the reader to the list, from the list to the sidebar | `back` |
| `q` | Close the reader, a panel, or the window of a message or task | `close` |
| `Tab` | Move between the two halves of the view | `focus-next-section` |
| `Ctrl-h` / `Ctrl-j` / `Ctrl-k` / `Ctrl-l` | Move to the pane left, below, above, right; past the last, to `navigation.escalate_command`. A click in a pane gives it the keyboard too | `focus-pane left|down|up|right` |
| `gt` / `gT` | Next / previous folder | `next-folder`, `prev-folder` |
| `gf`, `⌘K` | Go to anything in the sidebar by a few letters of its name | `go-to` |
| `⇧⌘K` | The [command palette](#command-line-palette-and-help) | `command-palette` |
| `Ctrl-1` … `Ctrl-9` | Open the first … ninth [pin](#pinned); works from a text field too | `open-pinned <n>` |
| `gp`, `⇧⌘P` | Pin what is open (in the sidebar, what the cursor is on), or unpin it | `toggle-pin` |
| `m` | Mark read / unread | `toggle-read` |
| `!` | Flag / unflag | `toggle-flag` |
| `d` / `D` | Stage a delete / move to Trash now | `mark-delete`, `delete-immediate` |
| `s` / `S` | Stage a move / move now, to a folder picked from a list | `mark-move`, `move-immediate` |
| `c` | Copy now, to a folder picked from a list | `copy-immediate` |
| `a` | Archive now; in the [reading list](#reading), to its archive and back | `archive` |
| `b` | Set aside in the [reading list](#reading) | `read-later` |
| `U` | Unstage | `unmark` |
| `x` / `X` | Apply / discard the [staged actions](#staged-actions) in the folder (in Today, in its inboxes) | `expunge`, `discard-pending` |
| `p` | Review everything staged | `review-pending` |
| `Space` | Select / deselect and move down | `select-message` |
| `J` / `K`, `Shift-↓` / `Shift-↑` | Extend the selection down / up | `extend-selection-down`, `extend-selection-up` |
| `⌘A` | Select every message; in the reader, its text | `select-all` |
| `Esc` | Clear the selection; with none, take off the filter, then leave the search | `clear-selection` |
| `u`, `⌘Z` | [Undo](#undo) the last action | `undo` |
| `Ctrl-r`, `⇧⌘Z` | Redo | `redo` |
| `Ctrl-s` | Sync every account | `sync` |
| `C` | Compose | `compose` |
| `r` / `R` / `f` | Reply / reply all / forward | `reply`, `reply-all`, `forward` |
| `t` / `T` | Make a task of the message or selection / show its tasks | `create-task-from-message`, `open-tasks-for-message` |
| `Ctrl-t` | Make a task, postponed to a date the picker asks for (due on it after `Ctrl-e`) | `create-task-from-message postpone` |
| `o` | New task, with its title and notes open to write | `create-task` |
| `/` | [Search](#search) mail and tasks | `search-mode` |
| `n` / `N` | Next / previous match | `next-match`, `prev-match` |
| `gs` | Over results with both, show mail and tasks, mail, or tasks | `search-kind [all|mail|tasks]` |
| `F` | [Filter](#filter) the list as you type; `Enter` keeps it, `Esc` takes it off | `filter-mode` |
| `:` | The command line | `command-mode` |
| `?` | Every key in force, searchable | `help` |
| `zt` | Show the folder, or Today's mail, as [conversations](#conversations), or not | `toggle-threads` |
| `za` | Open or close the thread under the cursor; in the sidebar, fold or unfold | `toggle-thread` |
| `zR` / `zM` | Open / close every message of the conversation in the reader | `open-all-messages`, `close-all-messages` |
| `gc` | Show the whole conversation, from every folder of the account | `open-thread` |
| `gx` | Open a feed item's article, or the address a [message link](#message-links) rule finds | `open-link [rule]` |
| `zl` | Today: mail above the tasks, or beside them | `toggle-today-layout` |
| `zx` | Today: one timeline of mail, chat and reading beside the tasks, or back | `toggle-today-mixed` |
| `zr` / `zw` | Today: show or put away the [reading list](#reading) / [Waiting](#chat) | `toggle-today-reading`, `toggle-today-chat` |
| `zm` | Maximize the focused pane, or bring the panes back (see [The window](#the-window)) | `toggle-maximize` |
| `zs` | Put the folders away, or bring them back | `toggle-sidebar` |
| `gr` / `gn` | Open a chat room by name / start a chat with people picked | `chat-open-room`, `chat-new-message` |
| `zq` / `zQ` | Mute the chat room under the cursor for a while / list the muted rooms | `chat-mute-room`, `chat-muted-rooms` |
| `⌘+` / `⌘-` / `⌘0` | Larger / smaller text, and back | `zoom-in`, `zoom-out`, `zoom-reset` |

`q` never quits; `⌘Q` and `quit` do.

### Reader and conversations

The reader shows the message under the cursor and follows it. `Enter` or `l`
gives it the keyboard; there the mail keys still work on the message shown,
and `h` goes back. `bindings.index_detail` is layered over the mail table while
the reader has the keyboard; it is empty by default.

On a [conversation](#conversations) in the reader: `j` / `k` move between the
messages, `gg` / `G` go to the first and the newest, `Enter` or `za` opens or
closes the one the keys are on, `zR` / `zM` open or close all, `Ctrl-d` /
`Ctrl-u` scroll by most of a page. Reply, forward, `gx` and the attachment
commands act on the message the keys are on; everything else on every message
of the conversation.

### Task keys

Active in a task list: Today's tasks and the task views
(`bindings.task_normal`, over the mail table).

| Key | Action | Command |
|-----|--------|---------|
| `o` | New task | `create-task` |
| `Enter`, `l` | Open the task | `open-task-detail` |
| `e` | Edit title and notes | `edit-task [title|body]` |
| `x` / `X` | Complete / reopen | `complete-task`, `uncomplete-task` |
| `w` | Mark as being worked on, or not (`work-on clear` takes the mark off every task) | `work-on [clear]` |
| `d` / `U` | Move to the trash / restore; with steps selected, remove those steps | `delete-task`, `restore-task` |
| `s` / `D` | Postpone date / due date, from the [date picker](#date-picker). A task shows from the earlier of the two | `postpone-task`, `set-due-date` |
| `S` | Both dates at once | `set-both-dates` |
| `c` | Clear both dates | `clear-dates` |
| `r` | Add a reminder | `add-reminder` |
| `R` | Make the task repeat: a list of the usual rules, or `:set-recurrence every monday`, `monthly on the 1st`, `every 2 weeks until June`, in English or German | `set-recurrence [rule]` |
| `a` / `A` | Add / remove a tag | `add-tag`, `remove-tag` |
| `m` / `M` | Link the message in the reader / open the linked message | `link-message`, `open-linked-message` |
| `f` | Attach files; files dropped on the detail are attached too | `attach-file` |
| `+` | Add a step at the end of the checklist | `add-step [title]` |
| `Backspace`, `Delete` | Remove the selected steps | `remove-step [n]` |
| `gx` | Open what the first linked message opens, else the address on the first line of the notes or in the title | `open-link` |
| `E` | Empty the trash, after asking | `empty-task-trash` |
| `⌘↑` / `⌘↓` | Move the task up / down; dragging works too, a selection as one block | `move-task-up`, `move-task-down` |
| `zg` | Group Today, Upcoming and Someday by tag, or not | `toggle-group-by-tag` |
| `zd` | List Today's tasks due today or earlier first, or not | `toggle-due-first` |
| `Space` / `Esc` | Select / clear the selection, the checklist's too | `select-task`, `clear-task-selection` |
| `Ctrl-s` | Sync the tasks folder | `sync-tasks` |

Also without a key: `merge-tasks` (the selection into one, after asking how),
`task-info` (whether the task is on the server, with its revision),
`clear-recurrence`, `recurrence-from-completion [off]`, `toggle-step [n]`,
`edit-step [n]`, `rename-step <n> <title>`, `rename-tag <old> <new>`,
`recolour-tag <name> <colour>` and `delete-tag <name>`.

Every date and completion command acts on the whole selection, and `u` takes
it back. A click selects a task, `⌘`-click adds or removes one, Shift-click
selects a range; the selection stays after an action or a sync.

In the task detail (`bindings.task_detail`): `h` / `Esc` go back, `Enter`
edits, `j` / `k` scroll. While a title or body is edited
(`bindings.task_edit`): `Enter`, `Ctrl-s` or `⌘S` accepts, `Esc` cancels,
`Tab` / `Shift-Tab` move between title and notes. `Ctrl-s` exists because
`Enter` in the notes is a newline.

### Chat keys

Active in Waiting, a room, and Today's chat, in the list and the conversation
alike (`bindings.chat_normal`, over the mail table). A mail key pressed here
is refused rather than acting on the mail behind the chat.

| Key | Action | Command |
|-----|--------|---------|
| `j` / `k`, `↓` / `↑` | Next / previous thread | `chat-next`, `chat-prev` |
| `gg` / `G` | First / last thread | `chat-first`, `chat-last` |
| `l` | Give the conversation the keyboard | `chat-open` |
| `r`, `i`, `Enter` | Write in the thread | `chat-reply` |
| `c` | Write to the room itself, outside any thread | `chat-write-room` |
| `R` | Start a thread on the newest message of the room's timeline | `chat-reply-thread` |
| `C` | Start a chat with people picked (as `gn` everywhere) | `chat-new-message` |
| `a` | Done: out of Waiting until someone writes again, read in Rocket.Chat | `chat-done` |
| `m` | Mark the thread read, in Orb and in Rocket.Chat, or unread | `chat-toggle-read` |
| `t` / `Ctrl-t` | Make a task of the thread's newest message; `Ctrl-t` asks for the date | `chat-create-task [postpone]` |
| `za` | Show every reply of a long thread, or fold the earlier ones | `chat-toggle-fold` |
| `+` | React to the newest message somebody else wrote | `chat-react` |
| `⌘U` | Attach files to the message being written | `chat-attach` |
| `gx` | Open the thread in Rocket.Chat | `chat-open-in-browser` |
| `Ctrl-d` / `Ctrl-u` | Scroll the conversation half a page | `chat-scroll-down`, `chat-scroll-up` |
| `Space`, `J` / `K`, `⌘A`, `Esc` | Select, extend, select all, clear | `chat-select`, `chat-extend-down`, `chat-extend-up`, `chat-select-all`, `chat-clear-selection` |

`a`, `t` and `m` act on every selected thread. In the composer
(`bindings.chat_compose`): `Enter` or `⌘Enter` sends, `Shift-Enter` starts a
new line, `Esc` gives the keyboard back and keeps the text, `↑` in an empty
composer edits your last message, `⌘U` attaches.

Also without a key: `open-chat` (Waiting), `chat-leave-room`,
`chat-unmute-room`, `chat-create-room <name> [private]`, `chat-also-in-room`,
`chat-sync`, `chat-sign-in`, `chat-sign-out`, `chat-menu`.

### Date picker

Opened by `s`, `D`, `S`, `r` and `Ctrl-t`. A text input with a calendar below;
typing replaces the calendar with suggestions. Type `tomorrow`, `3d`,
`next tuesday`, `eom`, `Sept 2028 12`, `2026-09-12`, or `tomorrow 9am` for a
reminder.

| Key | Action | Command |
|-----|--------|---------|
| `Enter` | Take the suggestion, or the calendar's day | `accept` |
| `Esc` | Cancel | `cancel` |
| `Ctrl-d` | Clear the date (on an empty input) | `clear-date` |
| `↓` / `↑`, `Ctrl-n` / `Ctrl-p`, `Tab` / `Shift-Tab` | Through the suggestions | `move-down`, `move-up` |
| `←` / `→` | Previous / next day | `move-left`, `move-right` |
| `PageUp` / `PageDown`, `Ctrl-b` / `Ctrl-f` | Previous / next month | `prev-month`, `next-month` |
| `Ctrl-t`, `Home` | Today | `today` |
| `Ctrl-e` | Set the other date instead: postpone or due, where both are offered | `switch-field` |

Only bracketed keys can be bound in `bindings.date_picker`; a bare character
belongs to the input.

### Command line, palette and help

`:` opens the command line. It completes as you type: the letters need only
appear in order (`clsl` finds `clear-selection`), and each candidate shows its
arguments, what it does and its key. `Tab`, `↓` or `Ctrl-j` fill in the next
one, `Shift-Tab`, `↑` or `Ctrl-k` the one before; on an empty line `↑` and
`↓` go through the history.

`⇧⌘K` (Go ▸ Command Palette…) is one box for everything. The first character
says what it lists:

| Prefix | Lists | Enter |
|--------|-------|-------|
| nothing | The places the sidebar can open, as `gf` and `⌘K` | Opens it |
| `>` | Every command, by title; `stage del` finds Stage for Deletion | Runs it |
| `#` | The tags | Opens the tag in the tasks window; elsewhere searches `tag:` |
| `@` | People from the address book | Searches mail from or to them |
| `/` | Mail and tasks found for the words | Opens it |

Every narrowing list in Orb ranks the same way: an exact name first, then one
that starts with the text, then a word that starts with it, then one that
contains it, then one with its letters in order. Recently used commands come
first, and a toggle is named for the way it would go (Mark as Read on unread
mail). Only commands that make sense in the window are listed; one that cannot
act on what the cursor is on stays greyed out, and `Enter` says why. A command
that needs a word asks for it in the same box, step by step; `Esc` cancels. A
count typed before `⇧⌘K` is passed on.

`?` (Help ▸ Keyboard Shortcuts) lays every key over the window. It opens on
**Here**, the keys that work where it was pressed; the other tabs show each
table (Mail, Reader, Tasks, Task detail, Editing, Chat, Composer, Dates), and
**All commands** every command with its keys. Type a key (`d`, `gg`, `⌃d`) to
see what it does, or words to find the key; `↑` / `↓` choose, `Enter` runs,
`Tab` moves between tabs, `Esc` clears and then closes. A rebound key is shown
as it now is.

After a prefix key (`g`, `z`), a pause shows a panel of what can follow it.

### Commands without a key

Type them after `:`, or bind them in `config.yml` (a key with Command is
`<d-y>`, with Shift as well `<d-s-y>`).

| Command | Action |
|---------|--------|
| `quit` | Quit Orb |
| `sync-folder` | Sync the open folder only; `sync-all` is `sync` |
| `sync-stats` | What the last sync of every folder did, and the connections opened since launch |
| `account-status` | Each account's standing with its server, with the last error |
| `console` | The log as it is written, and the commands the app ran lately |
| `backfill-status`, `fetch-bodies`, `stop-fetch-bodies` | How much of the mailbox has its bodies; start or stop filling them in |
| `load-remote-images` | Let the message on screen load its remote images, once |
| `notification-settings` | Open System Settings at Orb's notifications |
| `view html|text|raw` | Show the message as HTML, plain text or source |
| `toggle-dark-mail` | Draw HTML mail dark, or as it came, this session |
| `toggle-look`, `look hybrid|terminal` | Switch the look, or name one (see [The window](#the-window)) |
| `stage-read` | Mark read |
| `advance` | Move on from a message just staged: down, or up when the next is staged too |
| `mark-move <folder>`, `move-immediate <folder>`, `copy-immediate <folder>` | As `s`, `S`, `c`, with the folder named |
| `move-across`, `copy-across` | Move or copy into a folder of any account, picked from a list of them all |
| `spam` / `not-spam` | To the junk folder now / back to INBOX |
| `expunge-all`, `discard-pending all` | Apply, or discard, the staged actions everywhere |
| `outbox` / `retry-outbox` | Show the [Outbox](#outbox) / send everything in it now |
| `filter`, `filter --all` | Run the [filter rules](#filter-rules) on the selection or folder / on every folder a rule watches |
| `highlights` | The [highlight rules](#highlight-rules) and how many messages of the folder each matches |
| `open-attachment [n]`, `quick-look [n]`, `save-attachment [n]` | Open, preview or save attachment `n` (the first by default) |
| `create-folder <name>`, `rename-folder <name>`, `delete-folder`, `subscribe-folder`, `unsubscribe-folder` | See [Folders](#folders) |
| `toggle-detail` | Open or close the reader |
| `next-item`, `prev-item`, `scroll-down`, `scroll-up` | Move a row, or scroll half a page, in any list |
| `search <query>`, `clear-search`, `clear-filter` | Search from the command line; leave the search; take off the filter |
| `sort-by date|from|subject|relevance [asc|desc]` | Sort the list; without a direction, turn it round |
| `sort-menu`, `today-menu` | Open the toolbar's sort menu, or Today's View menu; `j` / `k` walk it, `Enter` picks, `Esc` closes |
| `open-task-view today|upcoming|someday|completed|archive|trash`, `today` | Open a task view |
| `open-reading list|must-read|archive`, `poll-feeds`, `feed-status` | Open a [reading](#reading) view; ask every feed now; what went wrong with one |
| `today-mail <account>|all` | One account's inbox in Today's mail, or every inbox |
| `toggle-today-layout [stacked|columns|mixed]`, `toggle-today-mail [on|off]`, `toggle-today-tasks [on|off]`, `toggle-today-reading [on|off]`, `toggle-today-chat [on|off]`, `today-reading must-read|new` | What Today shows and how (see [Tasks](#tasks)) |
| `clear-completed` | Hide the tasks completed so far from Today until midnight, on every machine |
| `create-task-from-message [postpone] [edit]` | A task per selected message; `edit` opens it to write |
| `export-tasks`, `import-tasks [path] [--include-purged]`, `import-things [path]`, `link-mail-in-notes`, `set-up-iphone` | See [Tasks](#tasks) |
| `open-task-link <task> <message-id>` | Open a task's linked message in the mailbox |
| `open-tasks-window`, `open-chat-window` | A window with tasks only, or chat only |

### Menu

Every menu item runs the command named, as a key bound to it would.

| Shortcut | Item | Command |
|----------|------|---------|
| `⌘Q` | Orb ▸ Quit Orb | `quit` |
| `⌘N` | File ▸ New Message | `compose` |
| `⌘F` | Edit ▸ Search Mail | `search-mode` |
| | Edit ▸ Command Line | `command-mode` |
| `⌘=` / `⌘-` / `⌘0` | View ▸ Make Text Bigger / Smaller / Actual Size | `zoom-in`, `zoom-out`, `zoom-reset` |
| | View ▸ Show as HTML / Plain Text / Source, Draw HTML Mail Dark, Load Remote Images, the conversation items, Switch Between Hybrid and Terminal, Show or Hide Sidebar, Today's layout and section switches, Group Tasks by Tag, List Due Tasks First, Console | `view …`, `toggle-dark-mail`, `load-remote-images`, `toggle-threads`, `toggle-thread`, `open-thread`, `open-all-messages`, `close-all-messages`, `toggle-look`, `toggle-sidebar`, `toggle-today-layout`, `toggle-today-mixed`, `toggle-today-mail`, `toggle-today-tasks`, `toggle-today-reading`, `toggle-today-chat`, `toggle-group-by-tag`, `toggle-due-first`, `console` |
| `⌘K` / `⇧⌘K` | Go ▸ Go to… / Command Palette… | `go-to`, `command-palette` |
| `⌃⌘↓` / `⌃⌘↑` | Go ▸ Next / Previous Folder | `next-folder`, `prev-folder` |
| | Go ▸ the task views, the reading views, the chat items, Staged Actions, Outbox | `open-task-view …`, `open-reading …`, `open-chat`, `chat-open-room`, `chat-mute-room`, `chat-muted-rooms`, `chat-new-message`, `chat-create-room`, `chat-sign-in`, `chat-sign-out`, `review-pending`, `outbox` |
| `⌘R` / `⇧⌘R` / `⇧⌘F` | Message ▸ Reply / Reply All / Forward | `reply`, `reply-all`, `forward` |
| `⇧⌘U` / `⇧⌘L` | Message ▸ Mark as Read/Unread / Flag | `toggle-read`, `toggle-flag` |
| `⌃⌘A` / `⇧⌘J` / `⌘⌫` | Message ▸ Archive / Move to Junk / Move to Trash | `archive`, `spam`, `delete-immediate` |
| | Message ▸ Edit Draft, Read Later, Move to…, Copy to…, Move or Copy to Another Account…, Not Junk, Stage for Deletion, Stage a Move…, Unstage, the attachment items, New Task from Message, Tasks for Message | `edit-draft`, `read-later`, `move-immediate`, `copy-immediate`, `move-across`, `copy-across`, `not-spam`, `mark-delete`, `mark-move`, `unmark`, `open-attachment`, `save-attachment`, `quick-look`, `create-task-from-message`, `open-tasks-for-message` |
| `⇧⌘N` | Mailbox ▸ Get New Mail | `sync` |
| `⇧⌘E` | Mailbox ▸ Apply Staged Actions in Folder | `expunge` |
| | Mailbox ▸ Get New Feed Items, Account Status, Feed Status, Outbox, Send Everything in the Outbox, the folder items, Review Staged Actions, Apply All, Discard in Folder, Discard All | `poll-feeds`, `account-status`, `feed-status`, `outbox`, `retry-outbox`, `create-folder`, `rename-folder`, `delete-folder`, `subscribe-folder`, `unsubscribe-folder`, `review-pending`, `expunge-all`, `discard-pending`, `discard-pending all` |
| | Tasks ▸ … | `open-tasks-window`, `create-task`, `edit-task`, `complete-task`, `uncomplete-task`, `clear-completed`, `merge-tasks`, `set-due-date`, `postpone-task`, `set-recurrence`, `add-reminder`, `attach-file`, `delete-task`, `restore-task`, `empty-task-trash`, `sync-tasks`, `export-tasks`, `import-tasks`, `import-things`, `set-up-iphone` |
| `⌘1` – `⌘3` | Window ▸ First / Second / Third Tab | Switch the window's tab |
| | Window ▸ Chat Window | `open-chat-window` |
| | Help ▸ Keyboard Shortcuts, Account Status, Feed Status, Sync Statistics | `help`, `account-status`, `feed-status`, `sync-stats` |

`⌘A` is `select-all` in the lists and the reader, and the system's Select All
in a text field.

## Mail

**Sync.** INBOX and the folders marked `watch` are watched with IMAP IDLE, so
new mail arrives when the server sends it; every account is also synced whole
every `sync_interval_seconds` (default five minutes), which covers the other
folders and servers without IDLE. After the machine wakes, or moves to another
network, every connection is reopened and every account synced. In the
background Orb fetches the text of every message for search, and the HTML of
the newest 300 in each folder, the open folder first, so recent mail opens
without the server. Everything that talks to a server shares one connection
per account, with timeouts and keepalive; one that stops answering is
replaced. Marking read or flagged, moving, deleting and applying staged
actions change the local copy at once and reach the server in the background.

**Reading.** HTML renders in a sandbox with no scripts and no network. Remote
images, stylesheets and fonts are blocked; a message with something remote
says so above the body, with a button (`load-remote-images`) that lets its
images load this once, and senders in `load_remote_images_from` or matched by
a `load_remote_images` rule get theirs without asking. Tracking pixels are
taken out of the message, stylesheets and fonts stay blocked either way, and a
link shows where it really goes when the pointer rests on it. Plain text
renders monospaced, the signature greyed out from its `-- ` line.

**Suspect mail.** A sender whose display name is another address than the real
one shows the real address in red behind a warning mark. The reader says when
the receiving server could not verify the sender (DMARC failed, or SPF failed
with no passing DKIM signature), and when replies go to an address outside the
sender's domain. A link that shows one address and opens another, or whose
address is a bare IP, hides the site behind a name and an `@`, or is spelled
with international characters, asks before it opens. Every link opens without
its tracking parameters (`utm_*`, `fbclid`, `gclid`, `mc_eid` and the like).
Dangerous [attachments](#attachments) are marked and asked about.

**Replies and forwards.** A reply goes to the sender's `Reply-To` when there
is one; a reply to a message you sent goes to its recipients again. It
carries `In-Reply-To` and `References`, so it stays in its thread in other
clients, and once sent the original is marked answered on the server. A
forward carries the original's attachments along and marks the message
forwarded. A sent message is filed in the account's Sent folder.

**Delete.** Deleting moves the message to the account's Trash folder, at once
(`D`) or staged (`d`). In Trash itself it deletes for good: the immediate
delete asks first, the staged one when it is applied. See
[Staged actions](#staged-actions).

## Composing

`C`, `r`, `R` and `f` open a compose window. The header holds From, To, Cc,
Subject and the files; the **Bcc** button at the end of the Cc line adds a
Bcc line. Recipients complete as you type (see [Recipients](#recipients)).
A recipient can be dragged to another place or another line (Bcc shows
while a drag lasts, and stays only if one is dropped there; `Esc` puts it
back); `⌥↓` / `⌥↑`
move the selected one a line down or up. The title bar holds Send,
**Save & close**, **Discard**, Attach, Follow up, the switch between Neovim
and the plain text field, and the draft's state. Files dropped on the window
are attached.

| Key | Action | Command |
|-----|--------|---------|
| `⌘Enter` | Send | `send` |
| `⌘S` | Save the draft | `save` |
| `⇧⌘S` | Save the draft and close the window | `save-and-close` |
| | Discard: delete the draft, after asking | `discard` |
| `⌘I` | Pick the identity to write as, from every account's | `pick-sender` |
| | The next identity, without the list | `cycle-identity` |
| `Ctrl-t` | Follow up: a day to come back to the message | `follow-up` |
| `⌘T` | Insert a [template](#templates) | `template` |
| `Ctrl-j` / `Ctrl-k` | Next / previous field, From to body; past either end, `navigation.escalate_command` | `next-field`, `prev-field` |

These are `bindings.compose_normal`, taken before Neovim sees them; bind one
to `noop` to give it back. A key without a modifier is never taken. The ⌘
chords stay with the window and the menu bar; everything else is Neovim's.

**Drafts.** The draft is saved every half minute while it changes, on `:w`,
when an attachment is added or removed, and when the window closes. Save and
Send take the body as it stands in Neovim, written or not. A draft reopened
from Drafts comes back with its recipients, headers and attachments, as the
identity it was saved as. **Discard** cannot be undone.

**Identity.** `⌘I`, or a click on the From address, lists every identity;
type to narrow, `Enter` picks. The signature is replaced with the new
identity's; one that was edited is left alone. An identity of another account
moves the draft to that account's Drafts and sends over that account. See
[Identities](#identities) for which identity a reply starts as.

**Signature.** Neovim does not hold it. It is drawn below the text (or above
the quote with `reply.signature: above`) in the `OrbSignature` highlight
group, out of the cursor's reach, and put back for every save and send. To
edit it for one message, switch to **Plain text**, where the field holds the
whole body.

**Follow up.** `Ctrl-t` asks for a day. When the message is sent or queued, a
task linked to it is made, postponed to that day. Clearing the date takes it
back. The day belongs to the window, not the draft. Needs a
[tasks section](#configuring-tasks).

**Checks.** What the window finds wrong is shown as a **Check** line under
the header, and Send asks before sending anyway. See
[Checks before sending](#checks-before-sending).

**Format.** The body is plain text, edited with the `mail` filetype and sent
as `format=flowed` (RFC 3676): a line ending in a space continues in the next
one. Neovim writes such lines with `formatoptions+=w` and a `textwidth`:

```lua
vim.api.nvim_create_autocmd('FileType', {
  pattern = 'mail',
  callback = function() vim.opt_local.formatoptions:append('aw') end,
})
```

Keep anything that strips trailing whitespace on save away from `mail`
buffers. A line that is only `--` goes out as the `-- ` that marks a
signature. Dead keys, input methods and `⌘V` work as anywhere on the Mac,
and plugins can tell they run in Orb from the client name `orb`.

### Templates

A template is a YAML file in `~/.config/orb/templates/`, or in
`compose.templates_dir`:

```yaml
name: Workshop invitation                # what the picker lists
subject: Preparing for the workshop      # optional
accounts: [Work]                         # optional; every account without it
body: |
  Dear {FirstName}

  For the workshop on {Date} we ask you to …

  [1] {Website}
```

A word in braces is a placeholder (`{Name}` and `{{Name}}` are the same). It
starts with a letter and holds letters, digits, spaces, `_`, `-` and `.`, so
code in braces stays text. The folder is read each time the picker opens.

Write the message, then press `⌘T` or **Template**. The picker lists the
account's templates first, filters as you type and previews the one under
the cursor; `Enter` inserts it at the cursor (in a reply, above the quote),
`⌘E` opens its file in Neovim, `Esc` closes. The subject fills the Subject
line only while that is empty. In Neovim the insertion is one change.

A **Fill in** line under the header then has a field per placeholder.
Typing fills it in wherever it stands; `Tab` goes to the next field, `Enter`
or `Esc` to the body. `{FirstName}`, `{Vorname}`, `{Prénom}`, `{Nome}` and
`{Name}` are filled from the first To recipient. In Neovim an open placeholder
is drawn in `OrbPlaceholder`, a filled one in `OrbPlaceholderFilled`. Send
stops at a placeholder still in braces; `⌥⌘Enter` sends anyway.

## The window

Orb follows the system appearance, light or dark, unless `appearance.theme`
fixes one. The cursor of the pane that has the keyboard is filled with the
accent colour; in the other panes it stays grey, so the open message and
folder are easy to find. Rows selected with `Space` and `J`/`K` are tinted
with the accent.

There are two looks, chosen with `appearance.look`. **Hybrid**, the default,
sets text in SF Pro and what is scanned in columns (dates, counts, keys, the
status line) in SF Mono. **Terminal** is monospace throughout, in the manner
of mutt or aerc: each row starts with a flag column (`N` unread, `!` flagged,
`+` files, `D` and `M` staged delete or move), icons become glyphs and
brackets, headers are `Key value` lines, and the status line is a modeline
that starts with the mode (`MAIL`, `TODAY`, `TASKS`, `SEARCH`, `CMD`). Both
looks show the same things and take the same keys. `toggle-look` (View menu)
switches every window on this machine; `look hybrid` and `look terminal` name
one. That choice holds until the config names a different look.

In a dark window HTML mail is drawn dark as well; `toggle-dark-mail` shows the
message as it came, and `appearance.dark_html_mail: false` leaves every
message as it came. `⌘+`, `⌘-` and `⌘0` set the text size on this machine,
in every window; `appearance.font_size` is what `⌘0` returns to, and
`appearance.editor_font_size` gives Neovim in the compose window a size of its
own that scales along. A click into an HTML message hands the keyboard straight
back to the list; the pointer still selects text, and a link opens in the
default browser (a `mailto:` link in the mail app). The lists announce cursor
and selection, and the status line is a live region, for a screen reader.

Keys can be typed ahead: each command runs after the ones before it, on the
list as they left it, so `a` then `j` archives the message `a` was pressed on
and then moves down, however long the server takes.

**The reader** shows the message under the cursor, headers first, with a
loading line until the body is in. Its header gives the subject, the sender's
name and address, the date, and the recipients (`me` for your own addresses,
`cc` before the copies). The message's files and linked tasks follow as chips.
A file opens on a click, with Quick Look and Save at its end, and can be
dragged out. A task's box completes it, its title opens it, its clock
postpones it.

**An empty list** says what is empty (a folder, a task view, a search, Today)
and offers the keys that lead on, such as `o` for a new task and `C` to
compose; each can be clicked. The reader goes away until the list has
something in it again.

**The toolbar** names what the list shows: the folder and its account, the task
view and the date, or the search and its number of results. Beside a folder's
name is the order in force ("Date ↓"); its menu picks the field, the direction
and whether the folder shows as [conversations](#conversations). The search box
opens the search field and, while results or a filter are showing, holds the
query with a button to leave it. Over results that found mail and tasks,
All / Mail / Tasks shows both or one kind.

**The status line** says on the left what was just done and how to take it
back: `⌘Z` after anything that can be undone, `U` after a message is staged.
On the right it keeps the steady state: how many actions are staged, where the
cursor is, and when the accounts last synced, or which of them could not.

**The main window** remembers its size and position. Closing it hides it
rather than quitting: syncs go on, the Dock badge keeps counting, and a click
on the Dock icon brings it back. `⌘Q` quits, after asking if there are staged
actions or task changes not yet on the server; open drafts are saved first.

**A window of its own.** A double-click on a message or a task opens it in a
window beside the list; a draft opens in a compose window, as `Enter` does.
Each message and each task has one such window, and double-clicking again
brings it forward. It shows the reader or the task detail alone, under a
toolbar that says where the message lives, and takes every key of the reader
or the detail. Linked tasks and messages open in windows of their own too.
`q` or `⌘W` closes it, and so does filing, deleting or moving its message, or
trashing its task.

### Maximizing

`zm` (`toggle-maximize`) lets the list fill the window: a folder, a task view,
or in Today the half the keyboard is in, with `Tab` (or `Ctrl-h`/`Ctrl-l` in
columns) switching to the other. `zm` again brings the panes back. While
maximized, the other panes come out over the list as drawers:

- The reader is out over the right from the start, showing the message or
  task under the cursor while the keyboard stays in the list. `q`, or `Esc`
  with nothing selected, puts it away; so does `toggle-detail` in the reader.
- `Ctrl-l` or `Enter` gives the reader the keyboard, and brings it out again
  if it was put away. `Esc` or `Ctrl-h` gives the keyboard back to the list
  and leaves the reader out.
- `Ctrl-h` from the list slides the folders out from the left while they have
  the keyboard; `Ctrl-l` or opening a folder puts them away.
- In Today the reader is below the list, as it is unmaximized; `Ctrl-j` gives
  it the keyboard and `Ctrl-k` takes it back.

A message the cursor passes while the reader is away is neither loaded nor
marked read. Maximized from the reader, the reader covers the whole list, and
leaving it puts it away.

`zs` (`toggle-sidebar`, or the button beside the traffic lights) puts the
folders away and gives their width to the list and the reader; `Ctrl-h` slides
them out over the list while they have the keyboard. The choice is kept across
restarts.

## Search

`/` opens the search field from any view, and `Enter` searches mail in every
folder of every account, every task but the trashed ones, or both. The switch
at the end of the field (Mail, Tasks, All) says which, `⌃T` steps through it,
and the field opens on the last choice. In the tasks window it searches the
tasks.

Words are matched in the subject, the sender, the recipients and the body of a
message, and in the title, the notes and the tags of a task. Each word must be
found, anywhere, case and accents aside. In the body a word matches the start
of a word (`invoic` finds "invoices"); in the subject, sender and recipients
it matches anywhere (`mill` finds "jmiller@example.com"). A prefix narrows a
word to one field:

| Term | Matches |
|------|---------|
| `f:` / `from:` | The sender's name or address |
| `t:` / `to:`, `cc:` | A recipient's name or address |
| `s:` / `subject:` | The subject; a task's title |
| `b:` / `body:` | The body; a task's notes |
| `tag:` | A task's tag, by the start of its name |
| `"exact phrase"` | The words next to each other |
| `in:Archive`, `acct:Work` | Mail in a folder, or of an account |
| `is:unread`, `is:read`, `is:flagged`, `is:answered`, `has:attachment` | Mail in that state |
| `after:2026-01-01`, `before:2026-06-30` | Mail by date |
| `header:X-Priority=1`, `header:List-Unsubscribe` | Mail with a header containing the text after `=`, or with the header at all. Only headers Orb records: `List-Id`, and those a view, a highlight rule or a `load_remote_images` rule names |
| `~words` | Mail by meaning rather than by word (needs `embedding_model`) |
| `A OR B`, `-term`, `NOT term` | Either; not (`OR` and `NOT` in capitals) |

`f:anna s:invoice` is mail from Anna with "invoice" in the subject. A term
only mail can match (a sender, a folder, a state, a date) finds no task;
`f:anna OR tag:work` finds Anna's mail and the work tasks.

The field completes as you type: the lines under it say what the word could
mean, and after a prefix list the folders, accounts or states. `↑`/`↓` choose
a line, `Tab` puts it in as a chip, `Enter` searches. An operator typed whole
becomes a chip too; a click on a chip turns it back into text, and
`Backspace` at the start of the text takes the last one back. The row under
the field says where to search: everywhere, the folder that was open, one
account, or Folders…, a list in which `Space` ticks the ones to search; `⌃F`
steps through them. A search narrowed to an account or to folders finds mail
only.

Results are laid out as Today is: mail first, then tasks, `Tab` between them.
Every message names its folder, the words searched for are marked in the list
and the reader, and `n`/`N` walk them. Mail comes most relevant first (newest
first for an `OR` or a search by conditions alone, closest first for `~`), or
in the order `search.sort` sets; the toolbar's sort menu or `sort-by` orders
it by date, sender or subject instead, applied before the 500-result limit,
and `sort-by relevance` goes back. Tasks stand under the date they come up:
Today with the overdue, a heading for each of the next six days, one per
month after that, Someday for the undated, and Completed, folded until `za`
opens it. All / Mail / Tasks in the toolbar, or `gs`, shows both or one kind,
and the choice is remembered. Results stay open across a sync, refreshed in
place; `Esc` goes back to the view the search was made from. `/` over results
holds their query: `Esc` with it untouched leaves the search, `Esc` after
changing it keeps the results.

### Filter

`F`, or Filter in the toolbar, puts a bar on top of the list and narrows what
is on screen as you type, in a folder, Today, a task view or search results.
`Enter` gives the list the keyboard and keeps the filter; `Esc`, in the bar or
on the list with nothing selected, takes it off; opening another view drops
it.

In a folder and in Today's inboxes the filter reads the search language above;
in a task view it matches the title, the notes and the tags, and `tag:` a tag;
over search results it matches what the rows show. A tag in the tasks window's
sidebar filters the same way. In Today it narrows every section alike; a
thread matches by author, room and text, and knows `is:unread`, `is:read` and
`f:`. There, `is:mail`, `is:task`, `is:chat` and `is:reading` keep one kind of
row (`is:mail is:task` two), and the rest of the text narrows what is kept.

## Folders

The sidebar shows the folders the server lists, by their names as written
(`Entw&APw-rfe` on the wire reads back as `Entwürfe`). A folder deleted or
renamed in another client is gone after the next sync of the account. A folder
inside another is drawn under it, one step in, by its own name; a server that
keeps everything inside `INBOX` (Courier, Cyrus) has them drawn beside the
inbox instead. A folder with folders inside has a disclosure arrow; an
account's heading folds the whole account and still shows its unread count;
every other heading (Tasks, Reading, Views, Chat, Tags) folds what is under
it. Click the arrow, or press `za` or `Enter` on it; `za` on a folder without
any inside folds the one it is in. What is folded is remembered on this Mac.

`create-folder <name>` makes a folder in the open folder's account;
`rename-folder <name>` renames the open folder, and staged actions and filter
history follow it; `delete-folder` deletes it with every message, on the
server, after asking. `subscribe-folder` and `unsubscribe-folder` set the flag
other clients use to list only some folders; Orb lists every folder. A name
may hold the server's own `.` or `/` for a folder inside another.

Each folder's icon says what it is for (inbox, drafts, sent, archive, junk,
trash). The special folders are the ones the config names, with the defaults
`Drafts`, `Sent`, `Trash`, `Archive` and `Junk`; when the server has no folder
of that name but marks one for the role (RFC 6154, as Gmail, Fastmail and
Dovecot do), that folder is used, so the special folders need no config there.
A configured name the server has wins over the role.

## Conversations

`zt` (`toggle-threads`), or the two-icon switch in a folder's toolbar and in
Today's Mail heading, shows the open folder as conversations, one row per
thread, newest conversation first whatever the sort says. The row carries the
subject of the oldest message, the member count with a caret, the newest
member's date, and who wrote last with their opening words. It is unread
while any member is, flagged or with files when any is, staged when every
member is staged alike, and carries the [highlights](#highlight-rules) and
tasks of every member. `za`
(`toggle-thread`) opens the thread under the cursor, its members under it,
oldest first and indented one step per reply, and closes it again.

The reader shows the conversation: every message oldest first, the unread
and the newest open, the others a line each with their opening words; three or
more read messages above the first open one fold into a line. The HTML/Text
switch applies to all of them. Resting on the row reads every message of it
after `mark_read_after_seconds`. `Enter` gives the reader the keyboard on the
first unread message; there `j`/`k` move between messages, `gg`/`G` go to the
first and the newest, `Enter` or `za` opens or closes the one the keys are
on, `zR`/`zM` open or close them all, `Ctrl-d`/`Ctrl-u` scroll, and `h` goes
back. A click on a message's line opens or closes it.

What is done to a conversation's row is done to every message of it, in
whichever folder each lives: read, flag, stage, archive, move, copy, with a
selection of rows too. `t` makes one task of it, from its newest message.
Reply, forward, `gx`, the attachment commands and `load-remote-images` act on
one message: the newest, or the one the reader's keys are on. To act on one
message alone from the list, open the thread with `za`. A folder opens the way
`threads` in its config says, and `zt` changes it while it stays open. In
Today, `zt` shows the inboxes' mail as conversations, a thread that spans two
inboxes being one; Today remembers the choice on this Mac.

A message joins the thread of the message it answers (`In-Reply-To`, else the
last entry of `References` that is in the folder). A reply by subject alone
(`Re:`, `AW:`, `Fwd:` and the like) that answers nothing in the folder joins
the oldest message with that subject; a message without such a prefix starts
its own thread. Mail synced before Orb asked for these headers gets them from
the background backfill within minutes.

`gc` (`open-thread`) shows the whole conversation a message is part of,
gathered from every folder of its account, the replies in Sent included,
oldest first, as a list that `q` leaves. It follows the headers across
folders up to four links deep.

## Offline

Reading never waits for the server when there is something stored. `Enter`
moves to the reader at once with the headers and a loading line, and every key
typed after it runs straight away. A message whose text the backfill has
stored opens from that text, and its HTML replaces it once the server sends it;
offline, the text stays.

When a server does not answer within eight seconds, what you are waiting on
gives up on it, and for the next thirty seconds shows what is stored instead
of asking again. A sync that reaches the server ends that at once. A message
with nothing stored says it could not be loaded.

Flags, moves, deletes and staged actions change the local copy at once and
reach the server with the next sync; mail sent while the server cannot be
reached waits in the [Outbox](#outbox).

## Jumping to a message

Clicking a new-mail notification, or opening a task's linked mail (`M`), jumps
to the message: the cursor lands on it, its row lights up for a moment, and
the reader shows it. It lands, in this order, in the list on screen when it
holds the message (Today included; a conversation is opened around it); else
in its folder, when the list does not have it or a filter hides it; else in
its conversation as `gc` shows it, when no folder in the sidebar holds it.
When the jump leaves the view, the status line names where it went. `Ctrl-o`
(`jump-back`) goes back to where the jump was made from, view, filter and
cursor, and again for the jump before that.

## Staged actions

An aerc-style workflow, so that a delete or a move need not be a single
irreversible keypress:

1. **Stage.** `d` (`mark-delete`) stages a delete, `s` (`mark-move`) a move
   to a folder picked from a list or named (`mark-move Archive`). The message
   is marked in the list, and the sidebar counts what is staged in each
   folder, in amber, beside its unread count.
2. **Review.** `p` (`review-pending`, Mailbox ▸ Review Staged Actions) lists
   everything staged, by folder, with Apply and Discard for each folder and
   for all of it, and below that what the [filter rules](#filter-rules) did in
   the last sync.
3. **Unstage.** `U` (`unmark`) takes the staged action off again; `X`
   (`discard-pending`) drops everything staged in the folder,
   `discard-pending all` everywhere. Nothing reaches the server.
4. **Apply.** `x` (`expunge`, `⇧⌘E`) applies the staged actions in the
   current folder, `expunge-all` in every folder. A staged delete moves the
   message to Trash; in Trash it deletes for good. The messages leave the list
   at once and the server follows, offline included; a move the server
   refuses puts its messages back.

`D` (`delete-immediate`, `⌘⌫`) skips the staging but not Trash: it moves the
message there at once, and in Trash deletes it for good after asking. `S`
(`move-immediate`), `a` (`archive`), `spam` and `not-spam` move at once too,
and like applied staged actions never wait on the server. `c`
(`copy-immediate`) copies at once. `move-across` and `copy-across` take a
message into another account: fetched whole from one server and appended to
the other, flags and all, and for a move deleted from the source once there.
Every immediate action can be undone, across accounts too (see [Undo](#undo)).

A delete or move is not carried out on the server when the folder was restored
from a backup or migrated since it was synced; the next sync of the folder
puts the list right.

## Outbox

A message sent while its mail server cannot be reached, or asks to be tried
later, waits in the Outbox and is tried again with every sync. A message the
server refuses (a wrong password, a rejected recipient) stays there as **not
sent**, with the server's reason, and is not tried again on its own: nothing
unsent leaves the Outbox without you.

While it holds anything, the Outbox is in the sidebar below the pins, with an
amber count of messages waiting or a red one of those refused; the status line
says the same, and a refused message is announced with a notification. A click
opens the Outbox in the reader, refused messages first, each with its
recipients, the login it goes out over, when it was queued, why it has not
gone, and three buttons:

| Button | Does |
|---|---|
| Retry / Send now | Put it back in line and try now. After a refused login the password is fetched again, so a corrected password needs no restart. |
| Edit | Open it in a compose window. It moves to Drafts with the window's first save, or leaves with the send; closed unsaved, it stays in the Outbox. |
| Delete | Delete it unsent, after asking. |

`outbox` opens it and `retry-outbox` sends everything in it now; both are in
the Mailbox menu.

## Attachments

A message's attachments are listed under its headers with their size. Clicking
a name opens it with its default application, except for file types macOS
would run rather than display, which are revealed in Finder instead. Files
that are dangerous to open for another reason (HTML and SVG, Office documents
with macros, disk images, Windows programs, names that hide their type behind
a right-to-left mark) are shown in red, and opening one asks first. The eye
shows it in Quick Look, the arrow saves it where you choose, and a row can be
dragged out onto the Finder or another application. `open-attachment`,
`quick-look` and `save-attachment` do the same from the keyboard, by the
attachment's number.

Attachments are unpacked from the message's full source, which the backfill
fetches for every message that has them, so they open offline; a message it
has not reached yet fetches on demand. Files are written once under
`~/.orb/attachments/mail/`.

## Undo

`u` or `⌘Z` takes back the last action, and `Ctrl-r` or `⇧⌘Z` does it again.
There is one history for everything, mail, tasks and chat, shared by every
window, so undo takes back the last thing done wherever it was done. The last
fifty actions can be taken back while Orb runs; the history starts empty at
launch, and doing something new forgets what could have been redone. An
action is one key, however much it did: a whole selection, or every step of a
chain such as `create-task-from-message && stage-read && mark-move Archive`.
The status line says `⌘Z to undo` after anything that can be undone.

What can be undone:

- **Mail:** `toggle-read` and `stage-read`, `toggle-flag`, `move-immediate`,
  `copy-immediate`, `move-across`, `copy-across`, `archive`, `read-later`,
  `spam`, `not-spam`, and `delete-immediate` when it moves to Trash. Only what
  the action changed is put back.
- **Staged actions:** `mark-delete`, `mark-move` and `unmark` put back what
  was staged before; `discard-pending` stages again what it dropped;
  `expunge` and `expunge-all` move the messages back, unstaged. What was
  deleted for good, out of Trash, stays deleted.
- **Folders:** `create-folder` (while nothing has been filed into it),
  `rename-folder`, `subscribe-folder`, `unsubscribe-folder`. `delete-folder`
  cannot be undone, and asks first.
- **Filters:** a `filter` run, as one action: flags cleared again, moved
  messages back, tasks made to the task trash. The rules do not file the
  messages again.
- **Tasks:** every change to a task; a task made from a message, a sent
  message or a chat thread; a reminder, attachment or linked message added or
  removed; `work-on clear`; a tag created, renamed, recoloured or deleted.
- **Chat:** `chat-done`, and a task made from a thread; `chat-toggle-read`,
  on the server too; a reaction; and `chat-leave-room`, except leaving a
  private group, which takes an invitation to come back.

A move is undone by asking the server for the messages in their new folder by
`Message-ID` and moving them back, so it works whether or not the destination
has been synced; a message someone else moved on in the meantime stays where
they put it, and messages without a `Message-ID` cannot be moved back. A copy
is undone by deleting the copies, only where the original is still in place,
so undo never deletes the last one left.

Sending mail, sending, editing or deleting a chat message, a permanent
delete, `delete-folder`, emptying the task trash and importing tasks have no
undo; the destructive ones ask first.

## Console and account status

`console` opens a tab beside the mailbox with the last few hundred lines of
the log as they are written, whether or not `log_file` sends them to a file,
and the commands the app ran lately (syncs, sends, moves, backfills), each
with when it started, how long it took and what went wrong. It is the place to
look when something did not happen. `sync-stats` shows what the last sync of
every folder did, phase by phase, and how many connections Orb has opened
since launch. A folder another sync was holding at the time is reported as
*skipped (busy)* and asked for again a few seconds later; a large folder
fills in while its first sync is still running, and a sync interrupted
halfway resumes where it stopped.

The log file is rotated at 10 MB; the previous one is kept as `.1`. A
database written by a newer Orb is refused at launch, naming both versions.

Every sync records what became of each account. An account whose last sync
failed is marked in the sidebar: a key for a refused login, a shield for a
certificate the connection could not accept, an unplugged lead for a server
that could not be reached, a warning sign for anything else; a turning arrow
shows while a sync runs. The mark's tooltip and `account-status` carry the
server's own words, the folder concerned, when it happened and when the
account last synced cleanly. A tasks folder that cannot be synced marks the
tasks account the same way.

## Notifications and the Dock badge

Unread mail that arrives in a watched folder (INBOX, and any folder marked
`watch`) is announced with a notification, one per account per sync: a single
message by sender and subject, several as a count with the first subjects
under it. Clicking it brings Orb forward and opens the newest of them (see
[Jumping to a message](#jumping-to-a-message)). What was in the mailbox when
Orb started is not announced, nor is a message a [filter rule](#filter-rules)
files away or marks read. The Dock badge counts the unread mail in every
account's INBOX; `dock_badges` puts up to four counts on the icon instead, one
per corner in its own colour (see [Global settings](#global-settings)), and
`chat.badge` adds the threads waiting in [chat](#chat).

Notifications need the permission Orb asks for at first launch. Once refused,
only System Settings › Notifications › Orb can switch it back on;
`notification-settings` opens that page, and a task's Reminders field says so
while it is off. Under `start.sh --dev` there is no bundle to post them, so
nothing is announced and the log says so.

## Tasks

Tasks need a [`tasks` section](#configuring-tasks). They live as ordinary
messages in one IMAP folder, which is the source of truth; the local tables
are a cache that can be deleted and rebuilt from it.

The task views sit in the sidebar under **Tasks**:

| View | Shows |
|------|-------|
| Today | Everything postponed or due today or earlier, the overdue, and what was completed today, struck through until midnight |
| Upcoming | Every open task with a date, under a heading for today, each of the next six days, each month of the coming year and each year after |
| Someday | Tasks without any date |
| Completed | Completed tasks, newest first |
| Archived | Tasks completed more than `tasks.archive_after_days` ago (default 30); un-completing one brings it back |
| Trash | Deleted tasks, expunged after 30 days or on `E` |

A task has an optional postpone date (hidden until then) and an optional due
date. A task shows from the earlier of the two, so one due today stays in
Today however far it is postponed. A due date today or earlier is red in the
list, in the detail and on the task's chip under a message; an overdue one is
marked `!` as well.

The detail pane shows what is set: the title beside its box, a chip for each
date, reminder, repeat and tag, one line of keys for what can still be added,
then the notes, the linked mail, the files and the history. A click on a chip
changes it and its cross clears it; a reminder's chip snoozes it for 10
minutes, an hour, three hours or until tomorrow 9:00. Each linked message says
*Replied* with the time of your last reply, or *No reply*.

Every date and completion command applies to the whole selection, and `u`
takes it back. A click selects a task, ⌘-click adds or removes one,
Shift-click selects a range, and the selection stays on its tasks after an
action, a drag or a sync. A right-click on a task opens a menu of its
commands, each with its key; a right-click outside the selection acts on that
task alone. Clicking a task's box completes just that task.

Tags are managed by name: `rename-tag <old> <new>`, `recolour-tag <name>
<colour>` (a CSS colour or `none`) and `delete-tag <name>`, which takes the
tag off every task. `a` without a name picks from the existing tags, so
`urgent` and `Urgent` cannot both come to exist; a new name is offered last.

Tasks ▸ Tasks Window (`open-tasks-window`) opens a second window with tasks
only, for a second screen. Its sidebar lists the task views and the tags; a
tag filters the open view, a second click lifts the filter. The detail sits
beside the list from 900 px of width and below it when narrower. A linked
message opens in the mailbox. The window keeps its size and place.

### Today

Today is the landing view and the first [pin](#pinned) unless `pinned` says
otherwise. It shows the inboxes' mail and the tasks on one grid, mail first,
under a *Mail* and a *Tasks* label with their counts. A task's box sits in the
status column, its title where the sender starts, its tags (`#name`) among the
badges and its due date in the date column. With several accounts, a switch at
the end of the *Mail* label shows every inbox (*All*) or one account's;
`today-mail <account>` or `today-mail all` does the same. Today opens with the
cursor on the first message, or on the first task when there is no mail.
`j`/`k` walk the
mail and the tasks as one list, `Tab` jumps between the halves, and
`Ctrl-j`/`Ctrl-k` walk the mail, the tasks and the reader below them. The
line between the halves can be dragged; a double-click fits the tasks again.

A completed task stays in Today, struck through, until midnight. A repeating
task shows the same way although it already waits for its next date.
`clear-completed` (Tasks ▸ Clear Completed from Today) hides the tasks
completed so far until midnight, on every Mac and iPhone sharing the folder;
`tasks.keep_completed_in_today: false` leaves completed tasks out of Today
altogether, and `tasks.completed_first_in_today: true` lists them above the
open ones.

The *View* menu in Today's toolbar (`today-menu`) switches each section on or
off and chooses the layout. It stays open while its switches are flipped.
Today always shows at least one section; a section put away keeps its place
and its cursor, and making or opening a task brings the tasks back. The same
from the keyboard and the View menu of the menu bar:

| Section or layout | Key | Command |
|---|---|---|
| Mail | | `toggle-today-mail [on\|off]` |
| Tasks | | `toggle-today-tasks [on\|off]` |
| Chat: the threads that [wait](#chat), below the tasks, at most thirty | `zw` | `toggle-today-chat [on\|off]` |
| Reading: the must-read or the new [reading items](#reading), below the tasks and the chat, at most thirty | `zr` | `toggle-today-reading [on\|off]`, `today-reading must-read\|new` |
| Stacked or columns: the mail above the tasks, or beside them | `zl` | `toggle-today-layout [stacked\|columns\|mixed]` |
| Mixed: one timeline beside the tasks, or back to the layout it left | `zx` | `toggle-today-mixed` |
| Group by tag: Today, Upcoming and Someday list their tasks under their tags, *No tag* last | `zg` | `toggle-group-by-tag` |
| Due first: the tasks due today or earlier above the rest, under *Due* and *Other* | `zd` | `toggle-due-first` |

In **columns** the mail is on the left and the tasks on the right, each the
full height of the pane, with shorter rows: no snippet, no address beside a
sender's name, no tags. `j`/`k` stay in their column and `Tab` or
`Ctrl-h`/`Ctrl-l` cross. The mail takes three fifths of the width until the
line between them is dragged; a double-click gives it back.

**Mixed** keeps the tasks on the right and makes the left column one
timeline of the mail, the waiting threads and the reading items, each at its
moment, with a kind column before the sender and no groups. The order button
turns it over, newest or oldest first, and the status line names the kind of
row beside the position. The keys are those of the row under
the cursor: on a thread `a` is done and `r` replies, on a message or a
reading item they act as on mail. With rows of several kinds selected, `a`
archives the messages and the items and is done for the threads, `t` makes a
task of each, and `toggle-read` marks each.

The chat and reading sections are panes of their own: `j`/`k` run on from the
last task into them, `Tab` visits them in turn, and each can be dragged
taller or shorter (a double-click fits it again). In them the keys of the row
apply: in chat `r` replies, `a` is done, `gx` opens the thread in Rocket.Chat;
in reading the keys act as on a message.

Everything Today shows, and its layout, is remembered on this Mac.

### Steps, merging and working on

A task can carry a checklist. `+` (`add-step`), or *Step* in the detail, opens
a new step at the end to type into: `Enter` keeps it and opens the next,
`Esc` leaves. A double-click or `edit-step` renames a step in place, its box
ticks it, its cross removes it. A click selects a step, ⌘-click adds one and
Shift-click a range; then `d`, `Backspace` or `Delete` removes the selected
steps and `x` ticks them all, or reopens them all when all are done. Drag a
step to move it; a selected step brings the whole selection along. From the
command line: `toggle-step [n]`, `add-step <title>`, `rename-step <n>
<title>`, `remove-step <n>`. The detail shows the steps under their count and
a bar, the list shows the count beside the task (`[2/4]` in the Terminal
look). A repeating task starts its checklist afresh on its next date. A task
with steps is written in a form an older Orb leaves alone (todo-spec §17).

Select two or more tasks and choose *Merge…* from their context menu (Tasks ▸
Merge Tasks…, `merge-tasks`). A dialog asks which task stays and proposes its
title, all their notes and the earliest dates, each of which you can change;
⌘↩ merges. Steps, tags, reminders, linked messages and attachments come
along, the other tasks go to the Trash, and `u` takes the whole merge back
(todo-spec §29).

`w` (`work-on`) marks the task, or the selection, as the one being worked on;
again takes the mark off, `work-on clear` takes it off every task. The task
gains a ▶ (`>` in the Terminal look), the status line names it from anywhere
and opens it on a click, and Today in the sidebar carries the mark.
Completing or trashing the task clears it. The mark is this Mac's alone and
is not synced (todo-spec §19).

### Dates, reminders and recurrence

`s`, `D` and `S` set the postpone date, the due date, or both, in the [date
picker](#date-picker); `c` clears both. The date views are re-evaluated at
local midnight, on wake and on a timezone change.

`r` adds a reminder, any number per task, each a local date and time.
Reminders are handed to macOS as scheduled notifications, so they fire
whether or not Orb runs, and clicking one names the task. A reminder missed
while the machine slept fires at the next launch if it is less than 24 hours
overdue; older ones are collected into one summary notification.

`R` makes a task repeat, from a list of the usual rules or spelled out:
`:set-recurrence every monday`, `monthly on the 1st`, `every 2 weeks until
June`, `every day 5 times`, in English or German. `clear-recurrence` stops
it, and `recurrence-from-completion on|off` says whether the next date
counts from the scheduled date (the default, so a task done late does not
drift) or from the completion. Completing a repeating task logs the
completion and moves it to its next date, so it stays one task.

### Mail and chat integration

`t` on a message, or on a selection, creates one task each, titled from the
subject and postponed to today. The mail stays on screen and `u` takes the
tasks back. `Ctrl-t` (`create-task-from-message postpone`) asks for the date
first, and clearing it in the picker makes the task undated;
`create-task-from-message edit` opens the task for editing instead, and the
two combine. `Ctrl-t` in the [compose window](#composing) does the same for
the message being written, once it is sent.

The link records the `Message-ID` with the subject, sender, date, account and
last folder, so it survives the message being moved and shows what it knew
when the message is gone. The list shows a count of open tasks per message,
the reader lists them, and `M` on a task opens its message (see [Jumping to a
message](#jumping-to-a-message)). In a task list, `m` (`link-message`) links
the message in the reader to the task by hand.

A `message://…` link in a task's notes, as Apple Mail and Things write one,
opens that mail in Orb on a click. `link-mail-in-notes` turns every such link
into a linked message of its task, as one undo step.

In [chat](#chat), `t` makes a task of a thread's newest message (`Ctrl-t`
asks for the date), and a message's own *Make a task* tool makes one of that
message. The thread leaves Waiting as with done, and `u` takes both back. The
task lists the message with its room and author and says *Replied* once you
have written in the thread after it.

### Sync, export and import

Every change goes up on its own a second after the last keystroke, and
another machine's changes are picked up within a minute. Edits made offline
queue and upload on reconnect. A task's history is merged as a union, so two
machines appending entries lose neither; everything else is last writer
wins. On the server a change is a new message followed by the deletion of
the old one, so a crash between the two leaves a duplicate that the next
sync resolves, never a gap.

A task with changes not yet on the server carries a dot in the list and *Not
yet on the server* in its detail. The *Tasks* heading in the sidebar counts
the changes still on their way and says *saved* when there are none. While
the folder cannot be synced, the heading carries a warning and the tasks
account is marked in the sidebar and in `account-status`. `task-info` says
whether the open task has reached the server, with its revision and dates.

Deleting a task is a soft delete that syncs; `E` empties the trash for good,
after asking. Deleting a repeating task deletes the series. A task abandoned
before it is named (`o` and then `Esc`) is erased rather than trashed.

A task's files live in messages of their own, so editing a task never
re-uploads them. Opening one downloads it once, caches it under
`~/.orb/attachments/tasks/`, and hands it to the system; a file macOS would
run is revealed in Finder instead.

`tasks.export` writes a snapshot of every task, open, completed, archived and
trashed, with steps, reminders, links, files and history, to a directory on
this Mac whenever the newest complete one is older than `interval_hours`
(default 24), and on `export-tasks` (Tasks ▸ Export Tasks Now). Each snapshot
is a directory `orb-tasks-<time>-<host>/` holding `tasks.json`,
`attachments/<id>/<filename>`, `manifest.json` with every file's size and
SHA-256, `schema.json` and a `README.txt` (todo-spec §20). The newest `keep`
(default 30) complete snapshots of this Mac are kept; another Mac's are left
alone. A failed export is a notification, and the status line says so until
one succeeds.

`import-tasks [snapshot] [--include-purged]` (Tasks ▸ Import Tasks…) reads a
snapshot back: it says first what is new, newer or left out and why, and
imports only when you confirm. Tasks are matched by id; a newer copy replaces
yours, an older one only adds its history, and the result is uploaded so it
wins everywhere. A task purged here stays purged without `--include-purged`
(todo-spec §21).

`import-things [path]` (Tasks ▸ Import from Things 3…) reads a copy of Things'
own database, or the `main.sqlite` you name, says what it will bring over and
imports when you confirm: titles, notes, dates, tags, checklists, reminders,
completed, cancelled and repeating to-dos; projects and areas become tags,
`message://` links become linked messages. A repetition Orb cannot express
leaves the task undated with a note in its history. Importing again brings
over what changed in Things since, except into tasks edited in Orb meanwhile
(todo-spec §22).

Neither import can be undone; both ask first.

### On an iPhone

Orb for iPhone (`ios/`) opens the same tasks folder and syncs it by the same
rules. `set-up-iphone` (Tasks ▸ Set Up iPhone…) shows the code it sets itself
up from: the tasks account, its folder, the date language, the Today settings
and the account's password. The code is as sensitive as the password; each
one works for five minutes.

On the iPhone, *Select* in a list's ⋯ menu picks several tasks; the bar below
completes, postpones, merges or deletes them, and dragging one of them moves
them all. *Hide Completed* in Today's ⋯ menu is `clear-completed`. A step is
added in the field below the checklist and renamed or deleted from its
context menu or by a swipe; *Select Steps* picks several.

## Reading

The reading list holds mail to read later, newsletters say, and the items of
the RSS, Atom and JSON feeds you subscribe to. It needs a [`reading`
section](#configuring-the-reading-list). The sidebar shows it under
**Reading**, below the tasks, in three views: **To read** (the reading folder;
its badge counts the unread items), **Must read** (the flagged items in it)
and **Archive** (the archive folder).

`b` (`read-later`, Message ▸ Read Later) sets the message under the cursor,
or the selection, aside in the reading folder. Within the reading list's own
account that is a move, and `u` takes it back; from another account the
message is copied over and deleted, which cannot be undone. A [filter
rule](#filter-rules) with `reading: true` does the same as mail arrives.

Feeds are polled every `poll_minutes`; Mailbox ▸ Get New Feed Items
(`poll-feeds`) asks them all now, and Feed Status (`feed-status`) says when
each was last asked and what went wrong. A feed item starts with a link to
its article, and `gx` opens the article in the browser.

In the reading list the usual keys keep their meaning: `!` marks an item
must read or not, `a` files it in the archive and brings it back from there,
`m` marks it read or unread. The state is plain IMAP, so every machine and
every other client sees the same list. Today can show the must-read or the
new items below its tasks (see [Today](#today)).

## Chat

With a [`chat` section](#configuring-chat), the sidebar ends with a **Chat**
group. **Waiting** lists the threads that wait for an answer, newest first,
one row each: who wrote the newest message, the room (`#name`, `Direct`, or
the people of a group message), the thread's first message as its title and
the newest as its preview. A room's or a direct message's messages outside
any thread count as one thread. `@` in the status column marks a thread that
names you since you last wrote in it; the dot, one you have not looked at
since its newest message.

A thread waits when its newest message is somebody else's. It leaves Waiting
when you answer, in Orb or anywhere else, or with done (`a`), and comes back
when someone writes again. Every room counts except those with `waiting:
false`. Reading a thread in Orb marks nothing read on the server. Answering
and done do, and so does `m`, which also marks a read thread unread again. With a [tasks
account](#configuring-tasks), done, task marks and mutes reach your other
Macs through it; without one they stay on this Mac.

`a`, `t` and `m` act on every selected thread, or on the one under the
cursor. ⌘-click and Shift-click select; a right-click opens the thread's menu
(Reply, Done, Open in Rocket.Chat, Check for Messages, Mute Room, Unmute Room,
Leave Room), which acts on the selection when the clicked thread is in it. A
key that acts on mail is refused while chat has the cursor.

The reader shows the conversation oldest first and opens at the newest
message, with a line above what came since you last wrote, each message's
files and reactions, and the composer below. A long thread shows its first
message and the newest replies; the ones between fold away (*N earlier
replies*, or `za`). A quoted message shows above the text that quotes it; a
click goes to it. A file opens with its default application on a click,
fetched the first time, or is revealed in Finder when opening it would run
it; an image shows as a preview, and a click opens it. The conversation opens
with the 200 newest messages and brings in a hundred more when scrolled to
the top or on *Earlier messages*. New messages arrive over the server's
realtime connection, with a full check every few minutes, every minute while
that connection is down. The status line says when chat is signed out or the
server cannot be reached; mail carries on.

**Notifications.** A thread that starts waiting is announced with who wrote,
the room and the text; several at once as one count. A click opens the
thread, in the chat window when one is open. What waits when Orb starts is
not announced. A room with `notify: false` stays quiet, and so does one
whose notifications Rocket.Chat has off, or set to mentions where the thread
does not name you. The Dock badge counts mail only unless `chat.badge: true`
adds Waiting.

**The chat window.** Window ▸ Chat Window (`open-chat-window`) opens a window
with chat only, for a second screen: Waiting, Direct and Channels in its
sidebar, two lines a thread in its list, and the conversation beside it from
900 px of width, below it when narrower. Both windows show the same Waiting.
Keys for mail, tasks or reading say so and do nothing. The window keeps its
size and place.

### Rooms and threads

Under Waiting, **Direct** and **Channels** list the rooms you are in, folded
until opened, each with how many of its threads wait. A room opens on all its
threads: its own timeline first (*In the room*), then the threads by their
newest message, with an arrow on those you answered last. A room with no
threads yet shows its conversation alone. A direct message with one person
shows how they stand: green online, amber away, red busy, a ring offline.

Opening a room, double-clicking a thread and opening one from a notification
put the keyboard in the composer, so the first key typed is already text;
`Esc` leaves it. A single click, `j` and `k` leave the keyboard with the list,
so `a`, `t` or `m` act on the thread; `r`, `i` or `Enter` write in it.

| What | Key | Command |
|------|-----|---------|
| Write to the room itself, outside any thread; from Waiting it opens the room first | `c` | `chat-write-room` |
| Start a thread on the newest message somebody else wrote in the room; *Reply in a thread* among a message's tools does it for any message | `R` | `chat-reply-thread` |
| Find a room or a person by name; a person opens or starts your direct messages, a public room you are not in is joined | `gr` | `chat-open-room` |
| Start a chat with the people you pick: `Enter` adds one, `Enter` on the empty field or `⌘Enter` opens it; an existing chat with the same people is opened instead | `gn`, `C` in chat | `chat-new-message` |
| Create a room, or a private group, and open it | | `chat-create-room <name> [private]` |
| Leave the room after asking; a direct message is hidden and comes back when either of you writes | | `chat-leave-room` |
| Open the thread in Rocket.Chat | `gx` | `chat-open-in-browser` |
| Check the server for messages now | | `chat-sync` |
| Sign in on the server's own page, or sign out and forget the session | | `chat-sign-in`, `chat-sign-out` |

On a server with threads turned off, neither reply in a thread nor `R` is
offered. Rocket.Chat does not let a room's last owner leave it.

### Writing

`r`, `i` or `Enter` on a thread put the keyboard in the composer. `Enter` or
`⌘Enter` sends, `Shift-Enter` starts a new line, `Esc` gives the keyboard
back to the list and keeps what was typed for that thread until it is sent,
across a restart too. In a room the composer keeps the keyboard for the next
message; in Waiting and Today the answered thread leaves and the keyboard
goes back to the list. `↑` in an empty composer edits your last message; the
× beside *Editing your message* gives the edit up. A thread reply can also be
shown in the room (the box beside Send, `chat-also-in-room`).

`@` completes people (those of the conversation at once, anyone on the server
from two letters), `#` rooms, `:` emoji from two letters; `Tab` or `Enter`
takes the suggestion. Files come from `⌘U` (`chat-attach`), the clipboard, or
a drop on the window, and show above the text until sent. `+` (`chat-react`)
reacts to the newest message somebody else wrote with one of the configured
reactions. A message under the pointer offers its tools: the reactions (a lit
one is yours, and a click takes it back), reply in a thread, make a [task](#tasks), and for your own messages edit and
delete, which asks first and removes the message for everyone. Sending,
editing and deleting a message cannot be undone.

### Muting a room

A room can be muted for a while without touching its settings on the server:
`zq` (`chat-mute-room`) on the open room, a thread's room or the room in the
sidebar, a right-click on it, or Go ▸ Mute Chat Room…. The picker offers 1
hour, 4 hours, until tomorrow and until you unmute it, and takes `90m`, `2d`
or `17:00`; on a muted room it offers Unmute first. From the command line:
`chat-mute-room [1h|90m|2d|today|17:00|forever|off]`, `chat-unmute-room`.
`zQ` (`chat-muted-rooms`) lists the muted rooms and until when; choosing one
unmutes it.

A muted room's threads still wait, but stay out of Waiting, Today, the
notifications and the Dock badge. The room's row in the sidebar counts them
in grey beside a struck-out bell, the conversation's header says how long the
mute lasts, and a click on it unmutes. A thread that names you, or calls the
room with `@here` or `@all`, comes through anyway. When a mute ends, the
threads come back into Waiting without a notification. Muting and unmuting
are undone with `u`.

## Project layout

- `src/` – the Rust `orb-core` library: IMAP sync, the SQLite cache and search, SMTP, tasks, chat, and the API the app is built on (`src/ffi.rs`).
- `app/src-tauri/` – the Tauri shell: commands, menu, notifications.
- `app/src/` – the frontend, vanilla TypeScript.
- `app/harness.html` – the frontend against a stubbed backend, for testing without a mailbox.
- `crates/orb-mobile/` and `ios/` – the UniFFI layer and the iPhone app.
- `tests/` – Rust integration tests.
- `docs/` – design documents and mockups.
- `todo-spec.md`, `chat-spec.md` – the task and chat specifications the code cites.

## License

MIT License — Copyright (c) 2025 sudoremo

See [LICENSE](LICENSE) for details.
