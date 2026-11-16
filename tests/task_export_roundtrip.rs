//! A task export survives a round trip through a server (todo-spec §20, §21).
//!
//! Machine A writes tasks of every kind the export has to carry — repeating,
//! trashed, completed, with a file — syncs them and exports. The server's
//! folder is then emptied, and machine B, on a database of its own, imports
//! the snapshot and syncs it back up. Machine C, on a third database, takes
//! everything from the server and exports again: the two `tasks.json` agree
//! but for when, where and at which revision they were written.
//!
//! The same round trip without a server is a unit test in `ffi.rs`; this one
//! proves the wire format carries every field there and back. Needs Docker:
//! `cargo test --test task_export_roundtrip -- --ignored`.

mod common;

use std::sync::Arc;

use common::imap_container::DovecotContainer;
use common::orb_config::config_with;
use common::test_context::TestContext;

use orb_core::ffi::{OrbHandle, StepEdit, TaskEdit};

const TASKS: &str = "tasks:\n  account: Test\n  folder: Tasks\n";

/// A machine: a handle on a database of its own, on the shared server.
fn machine(container: &DovecotContainer, ctx: &TestContext) -> Arc<OrbHandle> {
    let config = config_with(ctx, &container.host, container.imaps_port, TASKS);
    let path = ctx.config_path.to_string_lossy().into_owned();
    orb_core::ffi::init_with(config, path).expect("start the machine")
}

/// `tasks.json` without what differs by machine and moment.
fn normalised(snapshot: &str) -> serde_json::Value {
    let raw = std::fs::read(std::path::Path::new(snapshot).join("tasks.json")).expect("tasks.json");
    let mut json: serde_json::Value = serde_json::from_slice(&raw).expect("JSON");
    for key in ["exported_at", "device_id", "hostname"] {
        json[key] = serde_json::Value::Null;
    }
    for task in json["tasks"].as_array_mut().expect("tasks") {
        task["revision"] = serde_json::Value::Null;
    }
    json
}

#[test]
#[ignore = "needs Docker"]
fn an_export_survives_a_round_trip_through_a_server() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let container = runtime
        .block_on(DovecotContainer::start())
        .expect("start Dovecot");
    let files = tempfile::tempdir().expect("a directory");

    // A: one task of every kind, synced, exported.
    let ctx_a = TestContext::new();
    let a = machine(&container, &ctx_a);
    let repeating = a
        .create_task(
            "Water the plants".to_string(),
            Some("2026-08-06".to_string()),
        )
        .unwrap();
    a.update_task(
        repeating.task.id,
        TaskEdit {
            recurrence: Some("FREQ=WEEKLY".to_string()),
            body: Some("The ones on the balcony too.".to_string()),
            ..TaskEdit::default()
        },
    )
    .unwrap();
    a.edit_task_step(
        repeating.task.id,
        StepEdit::Add {
            title: "Fill the can".to_string(),
        },
    )
    .unwrap();
    let done = a
        .create_task("File the receipts".to_string(), None)
        .unwrap();
    a.complete_tasks(vec![done.task.id]).unwrap();
    let trashed = a.create_task("Old idea".to_string(), None).unwrap();
    a.trash_tasks(vec![trashed.task.id]).unwrap();
    let filed = a
        .create_task(
            "Sign the contract".to_string(),
            Some("2026-09-01".to_string()),
        )
        .unwrap();
    let pdf = files.path().join("contract.pdf");
    std::fs::write(&pdf, b"%PDF-1.4 not really a contract").unwrap();
    a.add_task_attachment(filed.task.id, pdf.to_string_lossy().into_owned())
        .unwrap();
    a.sync_tasks().expect("A syncs");
    let first = a
        .export_tasks_to(&files.path().join("a"), 30)
        .expect("A exports");
    assert!(first.complete, "A's export is missing {:?}", first.missing);
    assert_eq!((first.tasks, first.attachments), (4, 1));

    // The server forgets everything.
    runtime
        .block_on(container.delete_and_expunge("Tasks", "1:*"))
        .expect("empty the folder");

    // B: a new machine imports the snapshot and puts it back on the server.
    let ctx_b = TestContext::new();
    let b = machine(&container, &ctx_b);
    b.sync_tasks().expect("B finds the folder empty");
    let plan = b
        .import_tasks(first.path.clone(), true, true)
        .expect("B imports");
    assert_eq!(plan.new, 4, "{:?}", plan.problems);
    b.sync_tasks().expect("B uploads");

    // C: a third machine takes it all from the server and exports again.
    let ctx_c = TestContext::new();
    let c = machine(&container, &ctx_c);
    c.sync_tasks().expect("C downloads");
    let second = c
        .export_tasks_to(&files.path().join("c"), 30)
        .expect("C exports");
    assert!(
        second.complete,
        "C's export is missing {:?}",
        second.missing
    );

    assert_eq!(normalised(&first.path), normalised(&second.path));
    let payload = |snapshot: &str| {
        let root = std::path::Path::new(snapshot).join("attachments");
        let mut found = Vec::new();
        for entry in walk(&root) {
            found.push(std::fs::read(entry).unwrap());
        }
        found
    };
    assert_eq!(
        payload(&first.path),
        payload(&second.path),
        "the file came back as it was"
    );

    drop((a, b, c));
    runtime.block_on(async move { drop(container) });
}

/// Every file under `root`, in a stable order.
fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}
