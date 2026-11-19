
/**
 * Mirrors `orb_core::ffi::BindingsConfig`; a table nobody configured comes back
 * as null rather than as an empty object.
 *
 * Every table stays optional on this side even though the core now sends all of
 * them. A missing key here means "use the built-in defaults", which is the same
 * thing a null means, so a core built before a mode existed degrades to working
 * defaults instead of to a mode with no keys at all.
 */
export interface BindingsConfig {
  index_normal: Record<string, string> | null;
  index_detail: Record<string, string> | null;
  task_normal?: Record<string, string> | null;
  task_detail?: Record<string, string> | null;
  task_detail_terminal?: Record<string, string> | null;
  task_edit?: Record<string, string> | null;
  date_picker?: Record<string, string> | null;
  chat_normal?: Record<string, string> | null;
  chat_compose?: Record<string, string> | null;
  /** The user's own commands, by name: what each runs, and its title in the palette. */
  commands?: Record<string, UserCommand> | null;
}
/** A command of the user's own (`bindings.commands`). */
export interface UserCommand {
  run: string;
  title: string | null;
}
/**
 * Chat (PLAN.md, Phase 15), over `index_normal` in the chat list and the
 * conversation alike: the reader follows the thread under the cursor.
 */
export const DEFAULT_CHAT_BINDINGS: Record<string, string> = {
  j: "chat-next",
  k: "chat-prev",
  "<down>": "chat-next",
  "<up>": "chat-prev",
  gg: "chat-first",
  G: "chat-last",
  l: "chat-open",
  "<enter>": "chat-reply",
  r: "chat-reply",
  i: "chat-reply",
  a: "chat-done",
  m: "chat-toggle-read",
  t: "chat-create-task",
  "<c-t>": "chat-create-task postpone",
  za: "chat-toggle-fold",
  c: "chat-write-room",
  C: "chat-new-message",
  R: "chat-reply-thread",
  "+": "chat-react",
  "<d-u>": "chat-attach",
  gx: "chat-open-in-browser",
  "<c-d>": "chat-scroll-down",
  "<c-u>": "chat-scroll-up",
  "<space>": "chat-select",
  J: "chat-extend-down",
  K: "chat-extend-up",
  "<s-down>": "chat-extend-down",
  "<s-up>": "chat-extend-up",
  "<d-a>": "chat-select-all",
  "<esc>": "chat-clear-selection",
};
/**
 * The composer, while it has the keyboard. The text field owns every key not
 * named here; Shift-Enter starts a new line.
 */
export const DEFAULT_CHAT_COMPOSE_BINDINGS: Record<string, string> = {
  "<enter>": "chat-send",
  "<d-enter>": "chat-send",
  "<esc>": "chat-leave-composer",
  "<d-u>": "chat-attach",
};
/**
 * The built-in bindings, which `bindings.index_normal` in `config.yml` is
 * merged over (see `loadBindings`). Keys are in canonical form.
 */
export const DEFAULT_BINDINGS: Record<string, string> = {
  j: "next-message",
  k: "prev-message",
  "<down>": "next-message",
  "<up>": "prev-message",
  "<c-d>": "next-page",
  "<c-u>": "prev-page",
  gg: "top",
  G: "bottom",
  // Back to where a jump to a message was made from, as vim's jump list.
  "<c-o>": "jump-back",
  "<enter>": "open-message-detail",
  l: "open-message-detail",
  h: "back",
  // Read or unread on `m`, as Thunderbird marks it; `u` is vim's undo.
  m: "toggle-read",
  "!": "toggle-flag",
  "<c-s>": "sync",
  C: "compose",
  r: "reply",
  R: "reply-all",
  f: "forward",
  "/": "search-mode",
  // The filter narrows the list on screen; `/` searches everywhere (§4.5).
  F: "filter-mode",
  ":": "command-mode",
  // The aerc-style staging workflow, on the keys aerc users reach for: stage
  // with the lower-case letter, do it now with the capital, apply with `x`.
  d: "mark-delete",
  D: "delete-immediate",
  s: "mark-move",
  S: "move-immediate",
  c: "copy-immediate",
  U: "unmark",
  x: "expunge",
  X: "discard-pending",
  p: "review-pending",
  a: "archive",
  // Set aside to read later, as a bookmark is.
  b: "read-later",
  // Folders cycle on vim's tab keys, since the sidebar is the thing with
  // tabs' role here; `n` walks the matches of a search; `?` is help.
  gt: "next-folder",
  gT: "prev-folder",
  // Anything in the sidebar, by name: a folder, a view, a chat room.
  gf: "go-to",
  "<d-k>": "go-to",
  // Every command, by a few letters of its name: the same box on `>`.
  "<d-s-k>": "command-palette",
  // The pins at the top of the sidebar, by their place: `<c-1>` is the first.
  "<c-1>": "open-pinned 1",
  "<c-2>": "open-pinned 2",
  "<c-3>": "open-pinned 3",
  "<c-4>": "open-pinned 4",
  "<c-5>": "open-pinned 5",
  "<c-6>": "open-pinned 6",
  "<c-7>": "open-pinned 7",
  "<c-8>": "open-pinned 8",
  "<c-9>": "open-pinned 9",
  // What is open, pinned below those, or unpinned.
  gp: "toggle-pin",
  "<d-s-p>": "toggle-pin",
  n: "next-match",
  N: "prev-match",
  // A search's mail and tasks, its mail, its tasks (§26).
  gs: "search-kind",
  // Conversations, on vim's fold keys: `zt` shows the folder as threads,
  // `za` opens one as vim opens a fold, and `gc` shows the whole
  // conversation a message belongs to.
  zt: "toggle-threads",
  za: "toggle-thread",
  // …and in the reader, every message of a conversation open or closed,
  // as vim's zR and zM open and close every fold.
  zR: "open-all-messages",
  zM: "close-all-messages",
  gc: "open-thread",
  // Today's mail above its tasks, or beside them (§4.2).
  zl: "toggle-today-layout",
  // …or the mail, the chat and the reading as one timeline beside the tasks.
  zx: "toggle-today-mixed",
  // The reading list in Today, or not.
  zr: "toggle-today-reading",
  zw: "toggle-today-chat",
  gr: "chat-open-room",
  gn: "chat-new-message",
  // A chat room quiet for a while, and the list of those that are.
  zq: "chat-mute-room",
  zQ: "chat-muted-rooms",
  // A while without notifications: how long, and in which mode.
  zf: "focus-mode",
  // The focused pane over the whole window, as tmux zooms one: `zm`.
  zm: "toggle-maximize",
  // The folders put away, the other panes taking their width.
  zs: "toggle-sidebar",
  // vim's "open the address under the cursor", for the one a subject names.
  gx: "open-link",
  "?": "help",
  // `q` closes what is open — the reader, a panel — rather than the app,
  // which ⌘Q and `quit` still do.
  q: "close",
  "<space>": "select-message",
  J: "extend-selection-down",
  K: "extend-selection-up",
  "<s-down>": "extend-selection-down",
  "<s-up>": "extend-selection-up",
  "<d-a>": "select-all",
  "<d-=>": "zoom-in",
  "<d-+>": "zoom-in",
  "<d-->": "zoom-out",
  "<d-0>": "zoom-reset",
  "<esc>": "clear-selection",
  // Tasks, from the mail modes (§13). `t` makes a task out of what is selected,
  // `<c-t>` asks for its date first; `T` shows what already refers to it.
  t: "create-task-from-message",
  "<c-t>": "create-task-from-message postpone",
  T: "open-tasks-for-message",
  // A new task from anywhere, as in a task list: Today opens on its mail.
  o: "create-task",
  // `<tab>` moves between the two halves of whatever is on screen — the two
  // lists of Today (§4.2), and the list and the reader everywhere else.
  "<tab>": "focus-next-section",
  // Ctrl+H/J/K/L to the pane that way, as between vim's splits; past the
  // last one, out of Orb by `navigation.escalate_command`.
  "<c-h>": "focus-pane left",
  "<c-j>": "focus-pane down",
  "<c-k>": "focus-pane up",
  "<c-l>": "focus-pane right",
  // Undo on vim's keys and on the platform's, from every mode: §10.3 asks
  // for a single key *immediately afterwards*, and the mutation that most
  // wants taking back is the bulk one made from a mail selection.
  u: "undo",
  "<c-r>": "redo",
  "<d-z>": "undo",
  "<d-s-z>": "redo",
};
/**
 * Built-in `bindings.task_normal` (§13).
 *
 * Layered over `index_normal`, so navigation, search, sync and quit keep
 * working in a task view without being restated. Every key here is a default
 * the config can override, and `noop` is how one is turned off — the same
 * contract the mail bindings have.
 *
 * `s` and `D` are the two the spec names outright, on plain keys like every
 * other task command.
 */
export const DEFAULT_TASK_BINDINGS: Record<string, string> = {
  o: "create-task",
  "<enter>": "open-task-detail",
  l: "open-task-detail",
  x: "complete-task",
  w: "work-on",
  X: "uncomplete-task",
  d: "delete-task",
  U: "restore-task",
  s: "postpone-task",
  D: "set-due-date",
  // Both dates at once, beside the two that set one each.
  S: "set-both-dates",
  c: "clear-dates",
  r: "add-reminder",
  e: "edit-task",
  a: "add-tag",
  A: "remove-tag",
  m: "link-message",
  M: "open-linked-message",
  R: "set-recurrence",
  f: "attach-file",
  "+": "add-step",
  // Only what is selected in the checklist; with nothing selected, nothing.
  "<backspace>": "remove-step",
  "<delete>": "remove-step",
  E: "empty-task-trash",
  "<d-up>": "move-task-up",
  "<d-down>": "move-task-down",
  "<space>": "select-task",
  "<esc>": "clear-task-selection",
  "<c-s>": "sync-tasks",
  zg: "toggle-group-by-tag",
  zd: "toggle-due-first",
};
/** Built-in `bindings.task_detail`, layered over `task_normal` in turn. */
export const DEFAULT_TASK_DETAIL_BINDINGS: Record<string, string> = {
  h: "back",
  "<esc>": "back",
  "<enter>": "edit-task",
  // In the detail pane there is one task and nothing to walk, so the motions
  // scroll the pane instead of moving a cursor that is not there.
  j: "scroll-detail-down",
  k: "scroll-detail-up",
};
/**
 * Built-in `bindings.task_edit`.
 *
 * Consulted by the field itself rather than by the global keymap, which never
 * sees a keystroke aimed at an input. `<c-s>` exists because the body is prose:
 * `<enter>` there has to be a newline, so accepting needs a key of its own.
 */
export const DEFAULT_TASK_EDIT_BINDINGS: Record<string, string> = {
  "<enter>": "accept-edit",
  // Both spellings of save. `<c-s>` exists because the notes are prose, where
  // `<enter>` has to be a newline; `<d-s>` because this is the key that means
  // save on this platform, and reaching for it and getting the webview's
  // "save page" instead is the kind of small betrayal that costs a paragraph.
  "<c-s>": "accept-edit",
  "<d-s>": "accept-edit",
  "<esc>": "cancel-edit",
  // With the title and the notes both open, Tab moves between them. Bound
  // rather than left to the browser's own focus order: the pane grows a button
  // whenever a task has a reminder, a link or an attachment, and native
  // tabbing walks into those instead — which put the caret somewhere invisible
  // and sent the next sentence to whatever had focus.
  "<tab>": "next-edit-field",
  "<s-tab>": "prev-edit-field",
};
