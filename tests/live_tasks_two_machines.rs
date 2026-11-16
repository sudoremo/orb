//! The task sync against a real IMAP server, from two machines.
//!
//! Ignored by default, and only ever run against the account `ORB_CONFIG`
//! names — the test account, never the machine's own config. Each "machine"
//! is an `OrbHandle` on a database of its own, which is all that tells two
//! Macs apart. The test touches only the task it creates, and deletes it for
//! good at the end. Run with
//! `ORB_CONFIG=… cargo test --test live_tasks_two_machines -- --ignored --nocapture`.

use std::collections::BTreeSet;
use std::sync::Arc;

use orb_core::ffi::{OrbHandle, TaskDetail, TaskEdit, TaskViewName};

/// How the test's tasks are titled, so a run can find what an earlier one
/// left behind.
const TITLE: &str = "Two-machine check";

/// A machine: the handle, and the directory its database lives in.
struct Machine {
    name: &'static str,
    orb: Arc<OrbHandle>,
    _dir: tempfile::TempDir,
}

impl Machine {
    fn start(name: &'static str, base: &str) -> Self {
        let dir = tempfile::tempdir().expect("a directory for the machine");
        let config = std::fs::read_to_string(base).expect("read the config");
        let config: String = config
            .lines()
            .filter(|line| !line.starts_with("db_path:") && !line.starts_with("log_file:"))
            .map(|line| format!("{line}\n"))
            .collect();
        let path = dir.path().join("config.yml");
        std::fs::write(
            &path,
            format!(
                "db_path: {}\nlog_file: {}\n{config}",
                dir.path().join("mail.db").display(),
                dir.path().join("orb.log").display()
            ),
        )
        .unwrap();
        let orb = orb_core::ffi::init(path.to_string_lossy().into_owned()).expect("start");
        assert!(orb.tasks_configured(), "the config names a tasks account");
        Self {
            name,
            orb,
            _dir: dir,
        }
    }

    fn sync(&self) {
        let report = self.orb.sync_tasks().expect("sync the tasks");
        println!(
            "{}: listed {} downloaded {} uploaded {} deleted {} purged {} expunged {}",
            self.name,
            report.listed,
            report.downloaded,
            report.uploaded,
            report.deleted,
            report.purged,
            report.expunged
        );
    }

    /// The task with this identity, wherever it is, if this machine has it.
    fn find(&self, uuid: &str) -> Option<TaskDetail> {
        [
            TaskViewName::Today,
            TaskViewName::Upcoming,
            TaskViewName::Someday,
            TaskViewName::Completed,
            TaskViewName::Trash,
        ]
        .into_iter()
        .flat_map(|view| self.orb.list_tasks(view, None, None).expect("list"))
        .find(|task| task.uuid == uuid)
        .map(|task| self.orb.get_task(task.id).expect("the task"))
    }

    fn get(&self, uuid: &str) -> TaskDetail {
        self.find(uuid)
            .unwrap_or_else(|| panic!("{} has no task {uuid}", self.name))
    }

    fn log(&self, uuid: &str) -> BTreeSet<String> {
        self.orb
            .task_log(self.get(uuid).task.id)
            .expect("log")
            .into_iter()
            .map(|entry| entry.entry_uuid)
            .collect()
    }
}

#[test]
#[ignore = "talks to the account ORB_CONFIG names"]
fn a_task_travels_between_two_machines_and_is_gone_from_both() {
    let base = std::env::var("ORB_CONFIG")
        .expect("ORB_CONFIG must name the test account's config; this test writes to it");
    let a = Machine::start("A", &base);
    let b = Machine::start("B", &base);
    a.sync();

    // What an earlier run that failed half-way left behind, and nothing else.
    let leftovers: Vec<i64> = [
        TaskViewName::Today,
        TaskViewName::Upcoming,
        TaskViewName::Someday,
        TaskViewName::Completed,
        TaskViewName::Trash,
    ]
    .into_iter()
    .flat_map(|view| a.orb.list_tasks(view, None, None).expect("list"))
    .filter(|task| task.title.starts_with(TITLE))
    .map(|task| task.id)
    .collect();
    for id in &leftovers {
        a.orb.discard_task(*id).expect("discard a leftover");
    }
    if !leftovers.is_empty() {
        println!("removed {} task(s) left by an earlier run", leftovers.len());
        a.sync();
    }
    b.sync();

    // Created on A, with a due date and a reminder, and found on B.
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let title = format!("{TITLE} {stamp}");
    let created = a.orb.create_task(title.clone(), None).expect("create");
    let uuid = created.task.uuid.clone();
    a.orb
        .update_task(
            created.task.id,
            TaskEdit {
                due_date: Some("2031-03-14".into()),
                ..Default::default()
            },
        )
        .expect("set a due date");
    a.orb
        .add_task_reminder(created.task.id, "2031-03-13T09:00:00+01:00".into())
        .expect("add a reminder");
    a.sync();
    b.sync();
    let on_b = b.get(&uuid);
    assert_eq!(on_b.task.title, title);
    assert_eq!(on_b.task.due_date.as_deref(), Some("2031-03-14"));
    assert_eq!(
        on_b.reminders.len(),
        1,
        "the reminder travels with the task"
    );

    // Renamed on B, and A follows.
    b.orb
        .update_task(
            on_b.task.id,
            TaskEdit {
                title: Some(format!("{title} (renamed on B)")),
                ..Default::default()
            },
        )
        .expect("rename");
    b.sync();
    a.sync();
    assert_eq!(a.get(&uuid).task.title, format!("{title} (renamed on B)"));

    // Both edit before either syncs: they converge on one version, and the
    // log keeps every entry from both.
    a.orb
        .update_task(
            a.get(&uuid).task.id,
            TaskEdit {
                body: Some("Written on A while B was editing.".into()),
                ..Default::default()
            },
        )
        .expect("edit on A");
    b.orb
        .update_task(
            b.get(&uuid).task.id,
            TaskEdit {
                due_date: Some("2031-04-01".into()),
                ..Default::default()
            },
        )
        .expect("edit on B");
    a.sync();
    b.sync();
    a.sync();
    let (on_a, on_b) = (a.get(&uuid), b.get(&uuid));
    assert_eq!(on_a.revision, on_b.revision, "one revision on both");
    assert_eq!(on_a.task.title, on_b.task.title);
    assert_eq!(on_a.task.due_date, on_b.task.due_date);
    assert_eq!(on_a.body, on_b.body);
    let (log_a, log_b) = (a.log(&uuid), b.log(&uuid));
    assert_eq!(log_a, log_b, "the log is the union on both machines");
    println!(
        "converged on revision {}: due {:?}, body {:?}, {} log entries",
        on_a.revision,
        on_a.task.due_date,
        on_a.body,
        log_a.len()
    );

    // Completed on A, seen on B; trashed on B, seen on A.
    a.orb
        .complete_tasks(vec![a.get(&uuid).task.id])
        .expect("complete");
    a.sync();
    b.sync();
    assert!(
        b.get(&uuid).task.completed_at.is_some(),
        "completed on B too"
    );
    b.orb
        .trash_tasks(vec![b.get(&uuid).task.id])
        .expect("trash");
    b.sync();
    a.sync();
    assert!(a.get(&uuid).task.deleted_at.is_some(), "in A's trash too");

    // Deleted for good on A: gone from B, and it stays gone.
    assert!(a.orb.discard_task(a.get(&uuid).task.id).expect("discard"));
    a.sync();
    b.sync();
    a.sync();
    b.sync();
    assert!(b.find(&uuid).is_none(), "gone from B");
    assert!(a.find(&uuid).is_none(), "and does not come back to A");

    // A third machine starting from nothing finds no trace of it on the server.
    let c = Machine::start("C", &base);
    c.sync();
    assert!(c.find(&uuid).is_none(), "not on the server");
}
