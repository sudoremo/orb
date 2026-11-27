//! The undo history: one stack for every action the session can take back,
//! whatever it acted on — mail, tasks or chat.
//!
//! One stack rather than one per kind, because undo means "the last thing I
//! did": a key that skips the message just archived to take back a task
//! completed a minute earlier is a key nobody can predict. The stack lives in
//! the core, not in a window, so every window takes back the same last action.
//!
//! An entry holds the steps that put the state back, not a closure: carrying
//! out a step yields the step that reverses it again, which is what makes
//! every undo redoable without a second description of each action.

use crate::store::PendingActionRecord;
use crate::tasks::{CachedEmailMetadata, TaskAttachment, TaskReminder, TaskTag, UndoEntry};

/// How many actions can be taken back.
///
/// Undo is "the last thing I did", not a version history (todo-spec §10.3). A
/// bounded stack also bounds the memory a bulk action over a large selection
/// holds onto.
pub const DEPTH: usize = 50;

/// One part of an action, as what puts it back.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Task field values to restore, or tasks to trash (§10.3).
    Tasks(UndoEntry),
    /// Create a mail folder, or delete it while it is still empty. `name` as
    /// the server spells it.
    Folder {
        account: String,
        name: String,
        exists: bool,
    },
    /// Rename a mail folder; both names as the server spells them.
    FolderName {
        account: String,
        from: String,
        to: String,
    },
    /// Subscribe to a mail folder, or unsubscribe from it.
    Subscribed {
        account: String,
        name: String,
        subscribed: bool,
    },
    /// Make a tag's registry entry this, or take it out (§7).
    Tag {
        tag_uuid: String,
        tag: Option<TaskTag>,
    },
    /// Give a task exactly these reminders (§8), and the series they are an
    /// occurrence of (§32).
    Reminders {
        task_id: i64,
        reminders: Vec<TaskReminder>,
        series: Option<String>,
    },
    /// Give a task exactly these attachments (§9).
    Attachments {
        task_id: i64,
        attachments: Vec<TaskAttachment>,
    },
    /// Link a message to a task, remembered as `metadata`; with none, unlink
    /// it (§6).
    Link {
        task_id: i64,
        message_id: String,
        metadata: Option<CachedEmailMetadata>,
    },
    /// Put the working-on mark on these tasks, or take it off (§19).
    WorkingOn { task_ids: Vec<i64>, on: bool },
    /// Mark these messages read, or unread.
    Read {
        account: String,
        folder: String,
        uids: Vec<u32>,
        read: bool,
    },
    /// Flag these messages, or unflag them.
    Flagged {
        account: String,
        folder: String,
        uids: Vec<u32>,
        flagged: bool,
    },
    /// Move the messages named by their `Message-ID` out of `from` into `to`.
    ///
    /// By `Message-ID` because a move gives a message a new UID in its new
    /// folder, which this machine may not have synced yet. A message that is
    /// no longer in `from` — filed again by somebody else since — stays where
    /// they put it.
    Move {
        account: String,
        from: String,
        to: String,
        message_ids: Vec<String>,
    },
    /// Move the messages named by their `Message-ID` from one account's
    /// folder into another account's: fetched whole and appended, then
    /// deleted from `from` once they have arrived.
    MoveAcross {
        from: Place,
        to: Place,
        message_ids: Vec<String>,
    },
    /// Copy the messages named by their `Message-ID` from `from` into `to`,
    /// within an account or across.
    Copy {
        from: Place,
        to: Place,
        message_ids: Vec<String>,
    },
    /// Delete for good the copies in `to` of the messages in `from`; a
    /// message no longer in `from` is not a copy any more, and stays.
    Uncopy {
        from: Place,
        to: Place,
        message_ids: Vec<String>,
    },
    /// Make the staged actions on these messages exactly `actions`.
    Staged {
        account: String,
        folder: String,
        uids: Vec<u32>,
        actions: Vec<PendingActionRecord>,
    },
    /// Mark a chat thread read, or unread, in Orb and on the server.
    ChatRead {
        rid: String,
        thread: String,
        read: bool,
    },
    /// Add the user's reaction to a chat message, or take it back.
    ChatReaction {
        rid: String,
        id: String,
        emoji: String,
        on: bool,
    },
    /// Be in a chat room, or leave it; `kind` is the room's, `c`, `p` or `d`.
    ChatRoom {
        rid: String,
        kind: String,
        member: bool,
    },
    /// Give a chat thread this mark, or none (chat-spec).
    ChatMark {
        rid: String,
        thread: String,
        mark: Option<(String, i64)>,
    },
}

/// A folder of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub account: String,
    pub folder: String,
}

impl Place {
    pub fn new(account: &str, folder: &str) -> Self {
        Self {
            account: account.to_string(),
            folder: folder.to_string(),
        }
    }
}

/// One action: everything one key did, taken back by one key.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// What to call the action in "undone: …", e.g. `complete 3 tasks`.
    pub label: String,
    /// In the order they were done; undone last first.
    pub steps: Vec<Step>,
    /// When it was recorded, in the order of the session, for grouping.
    seq: u64,
}

impl Entry {
    /// The entry that reverses this one, given the reverse of each step in
    /// the order they were carried out (last first).
    pub fn reversed(&self, inverses: Vec<Step>) -> Entry {
        Entry {
            label: self.label.clone(),
            steps: inverses.into_iter().rev().collect(),
            seq: self.seq,
        }
    }
}

/// The undo and redo stacks.
///
/// Held in memory rather than in the database because undo is a property of
/// this session: taking back an action from three launches ago, after other
/// machines have changed the same things, is not undo — it is a stale write
/// dressed up as one.
#[derive(Debug, Default)]
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    next: u64,
}

impl History {
    /// Record an action, and drop any redo: the future being redone no longer
    /// follows from the present. An action with nothing to take back is not
    /// recorded.
    pub fn record(&mut self, label: impl Into<String>, steps: Vec<Step>) {
        let steps: Vec<Step> = steps.into_iter().filter(|s| !s.is_empty()).collect();
        if steps.is_empty() {
            return;
        }
        self.next += 1;
        let entry = Entry {
            label: label.into(),
            steps,
            seq: self.next,
        };
        push_bounded(&mut self.undo, entry);
        self.redo.clear();
    }

    /// The most recent action, taken off the stack to be undone.
    pub fn pop_undo(&mut self) -> Option<Entry> {
        self.undo.pop()
    }

    /// The most recently undone action, taken off the stack to be redone.
    pub fn pop_redo(&mut self) -> Option<Entry> {
        self.redo.pop()
    }

    /// Keep what reverses an undo, so it can be redone.
    pub fn push_redo(&mut self, entry: Entry) {
        if !entry.steps.is_empty() {
            push_bounded(&mut self.redo, entry);
        }
    }

    /// Put an action back on the undo stack after a redo, keeping the rest of
    /// the redo stack: a second redo must still be possible.
    pub fn push_undo(&mut self, entry: Entry) {
        if !entry.steps.is_empty() {
            push_bounded(&mut self.undo, entry);
        }
    }

    /// Where a group of actions starts; see [`History::merge_since`].
    pub fn mark(&self) -> u64 {
        self.next
    }

    /// Make every action recorded since `mark` one, so that a chain of
    /// commands run by one key is taken back by one key. `label` names the
    /// whole; without one, the parts' labels are joined.
    pub fn merge_since(&mut self, mark: u64, label: Option<String>) {
        let from = self
            .undo
            .iter()
            .position(|entry| entry.seq > mark)
            .unwrap_or(self.undo.len());
        let parts: Vec<Entry> = self.undo.split_off(from);
        if parts.is_empty() {
            return;
        }
        let label = label.unwrap_or_else(|| {
            parts
                .iter()
                .map(|entry| entry.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        });
        let seq = parts.last().map_or(self.next, |entry| entry.seq);
        let steps = parts.into_iter().flat_map(|entry| entry.steps).collect();
        self.undo.push(Entry { label, steps, seq });
    }

    /// Keep only the steps `keep` approves of, dropping entries left empty —
    /// for steps that name something that no longer exists, and so could
    /// only ever be a no-op.
    pub fn retain_steps(&mut self, keep: impl Fn(&Step) -> bool) {
        for stack in [&mut self.undo, &mut self.redo] {
            for entry in stack.iter_mut() {
                entry.steps.retain(&keep);
            }
            stack.retain(|entry| !entry.steps.is_empty());
        }
    }
}

impl Step {
    /// Whether carrying the step out would change nothing.
    fn is_empty(&self) -> bool {
        match self {
            Step::Tasks(entry) => entry.is_empty(),
            Step::Read { uids, .. } | Step::Flagged { uids, .. } => uids.is_empty(),
            Step::Move { message_ids, .. }
            | Step::MoveAcross { message_ids, .. }
            | Step::Copy { message_ids, .. }
            | Step::Uncopy { message_ids, .. } => message_ids.is_empty(),
            Step::Staged { uids, .. } => uids.is_empty(),
            Step::WorkingOn { task_ids, .. } => task_ids.is_empty(),
            Step::Folder { .. } | Step::FolderName { .. } | Step::Subscribed { .. } => false,
            Step::Tag { .. }
            | Step::Reminders { .. }
            | Step::Attachments { .. }
            | Step::Link { .. } => false,
            Step::ChatMark { .. }
            | Step::ChatRead { .. }
            | Step::ChatReaction { .. }
            | Step::ChatRoom { .. } => false,
        }
    }
}

fn push_bounded(stack: &mut Vec<Entry>, entry: Entry) {
    stack.push(entry);
    if stack.len() > DEPTH {
        stack.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::UndoStep;

    fn trash(task_id: i64) -> Step {
        Step::Tasks(UndoEntry {
            label: String::new(),
            steps: vec![UndoStep::Trash { task_id }],
        })
    }

    fn read(uid: u32) -> Step {
        Step::Read {
            account: "A".into(),
            folder: "INBOX".into(),
            uids: vec![uid],
            read: false,
        }
    }

    #[test]
    fn something_new_forgets_the_redo() {
        let mut history = History::default();
        history.record("complete 1 task", vec![trash(1)]);
        let entry = history.pop_undo().unwrap();
        history.push_redo(entry);
        history.record("mark 1 message read", vec![read(1)]);
        assert!(
            history.pop_redo().is_none(),
            "the future being redone no longer follows from the present"
        );
    }

    #[test]
    fn a_redo_keeps_the_rest_of_the_redo_stack() {
        let mut history = History::default();
        history.record("one", vec![trash(1)]);
        history.record("two", vec![read(2)]);
        for _ in 0..2 {
            let entry = history.pop_undo().unwrap();
            history.push_redo(entry);
        }
        let first = history.pop_redo().unwrap();
        assert_eq!(first.label, "one");
        history.push_undo(first);
        assert_eq!(history.pop_redo().unwrap().label, "two");
    }

    #[test]
    fn an_action_that_changed_nothing_is_not_recorded() {
        let mut history = History::default();
        history.record("nothing", vec![Step::Tasks(UndoEntry::default())]);
        history.record("nothing either", vec![]);
        assert!(history.pop_undo().is_none());
    }

    #[test]
    fn the_history_is_bounded_and_forgets_the_oldest() {
        let mut history = History::default();
        for id in 0..(DEPTH as i64 + 10) {
            history.record(format!("edit {id}"), vec![trash(id)]);
        }
        let mut labels = Vec::new();
        while let Some(entry) = history.pop_undo() {
            labels.push(entry.label);
        }
        assert_eq!(labels.len(), DEPTH);
        assert_eq!(labels.last().unwrap(), "edit 10");
    }

    #[test]
    fn a_group_is_one_action_of_every_kind_in_it() {
        let mut history = History::default();
        history.record("before", vec![read(9)]);
        let mark = history.mark();
        history.record("create 1 task", vec![trash(1)]);
        history.record("archive “Hello”", vec![read(1)]);
        history.merge_since(mark, None);

        let group = history.pop_undo().unwrap();
        assert_eq!(group.label, "create 1 task, archive “Hello”");
        assert_eq!(group.steps, vec![trash(1), read(1)]);
        assert_eq!(history.pop_undo().unwrap().label, "before");
    }

    #[test]
    fn a_group_takes_its_label_when_given_one() {
        let mut history = History::default();
        let mark = history.mark();
        history.record("done: a", vec![read(1)]);
        history.record("done: b", vec![read(2)]);
        history.merge_since(mark, Some("done: 2 threads".into()));
        assert_eq!(history.pop_undo().unwrap().label, "done: 2 threads");
    }

    #[test]
    fn a_reversed_entry_runs_its_steps_in_the_original_order() {
        let mut history = History::default();
        history.record("two steps", vec![read(1), read(2)]);
        let entry = history.pop_undo().unwrap();
        // Undone last first, so the inverses arrive as 2, 1.
        let back = entry.reversed(vec![read(2), read(1)]);
        assert_eq!(back.steps, vec![read(1), read(2)]);
    }

    #[test]
    fn steps_about_something_gone_are_dropped_and_empty_entries_with_them() {
        let mut history = History::default();
        history.record("mixed", vec![trash(1), read(1)]);
        history.record("task only", vec![trash(1)]);
        history.retain_steps(|step| step != &trash(1));
        let left = history.pop_undo().unwrap();
        assert_eq!(left.label, "mixed");
        assert_eq!(left.steps, vec![read(1)]);
        assert!(history.pop_undo().is_none());
    }
}
