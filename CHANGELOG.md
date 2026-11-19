# Changelog

What changed for people using Orb, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions
follow [semantic versioning](https://semver.org/). A release tag `vX.Y.Z` turns
the section for `X.Y.Z` into the notes of the GitHub release.

## [Unreleased]

The first release.

### Mail

- Mail from any IMAP server: several accounts in one sidebar, TLS or STARTTLS,
  password or OAuth 2.0, push with IDLE, a sync every few minutes, and a sync
  that works without CONDSTORE. Folders can be created, renamed, deleted and
  subscribed from the app.
- Everything cached in SQLite: recent mail opens without the server, reading
  never waits on it, and a change made offline reaches the server with the
  next sync.
- Composing in your own Neovim, embedded in the compose window: compose,
  reply, reply-all and forward, several identities per account, attachments,
  templates, checks before sending, an outbox for what cannot go out yet, and
  a follow-up task made when a message is sent.
- Filter rules act on mail as it arrives, highlight rules colour, mark and
  badge messages, subject rewriting shortens what tools send, and message
  links open the issue or build a message is about.
- A folder as threads, a thread opened in place, and any message's whole
  conversation gathered from every folder.
- Full-text search over every account and every task with a small query
  language, search by meaning with a local model, and a filter that narrows
  the list as you type.
- HTML mail rendered in a sandbox with no scripts and no network. Remote
  images load only when asked, tracking pixels are removed, and forged
  senders, suspicious links and dangerous attachments are marked.
- Staged deletes and moves, reviewed and applied as a batch, and one key that
  takes back the last action, whatever it was.

### Tasks, reading list and chat

- A task list stored as messages in one IMAP folder, so every machine sees the
  same list: dates, recurrence, tags, steps, reminders, attachments and a log
  per task. One key turns a message or a chat thread into a task.
- A reading list: newsletters set aside with one key, and RSS, Atom and JSON
  feeds, kept in two IMAP folders.
- Rocket.Chat: the threads that wait for your answer, one row each, with the
  conversation and a composer in the reader.

### The app

- Every key is a command, and the same commands back the command line, the
  command palette, the menu bar and the context menus. Counts, sequences and
  chains work, and `?` shows every key in force.
- Two looks, Hybrid and Terminal.
- A focus (`zf`) holds back the notifications and the Dock badge for a while,
  in modes the config defines; mail a highlight rule marks with
  `breaks_focus` still gets through, and what was held is announced
  afterwards.
- An iPhone app for the tasks that shares the IMAP folder, with widgets, a
  share extension and reminders as notifications.
