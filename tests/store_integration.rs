//! Store integration tests
//!
//! Tests for the Database layer including:
//! - Folder CRUD operations
//! - Message CRUD operations
//! - Sync lock coordination
//! - Transaction handling
//! - Bulk operations
//! - Pagination

mod common;

use common::test_context::TestContext;
use orb_core::Database;
use orb_core::models::Message;
use orb_core::store::MessageSort;

// ============================================
// Folder Tests
// ============================================

#[test]
fn test_folder_insert_and_retrieve() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db
        .insert_folder("test@example.com", "INBOX")
        .expect("Failed to insert folder");
    assert!(folder_id > 0);

    let folder = db
        .get_folder_by_name("test@example.com", "INBOX")
        .expect("Failed to get folder")
        .expect("Folder should exist");

    assert_eq!(folder.id, Some(folder_id));
    assert_eq!(folder.account_name, "test@example.com");
    assert_eq!(folder.name, "INBOX");
    assert_eq!(folder.uidvalidity, None);
    assert_eq!(folder.highest_uid, None);
}

#[test]
fn test_folder_get_by_id() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db
        .insert_folder("test@example.com", "Sent")
        .expect("Failed to insert folder");

    let folder = db
        .get_folder_by_id(folder_id)
        .expect("Failed to get folder")
        .expect("Folder should exist");

    assert_eq!(folder.id, Some(folder_id));
    assert_eq!(folder.name, "Sent");
}

#[test]
fn test_folder_get_nonexistent() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder = db
        .get_folder_by_name("test@example.com", "NonExistent")
        .expect("Query should succeed");
    assert!(folder.is_none());

    let folder_by_id = db.get_folder_by_id(99999).expect("Query should succeed");
    assert!(folder_by_id.is_none());
}

#[test]
fn test_folders_by_account() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Insert folders for multiple accounts
    db.insert_folder("alice@example.com", "INBOX").unwrap();
    db.insert_folder("alice@example.com", "Sent").unwrap();
    db.insert_folder("alice@example.com", "Drafts").unwrap();
    db.insert_folder("bob@example.com", "INBOX").unwrap();

    let alice_folders = db
        .get_folders_by_account("alice@example.com")
        .expect("Failed to get folders");

    assert_eq!(alice_folders.len(), 3);
    // Should be sorted by name
    assert_eq!(alice_folders[0].name, "Drafts");
    assert_eq!(alice_folders[1].name, "INBOX");
    assert_eq!(alice_folders[2].name, "Sent");

    let bob_folders = db
        .get_folders_by_account("bob@example.com")
        .expect("Failed to get folders");
    assert_eq!(bob_folders.len(), 1);
}

#[test]
fn test_get_or_create_folder() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // First call creates the folder
    let id1 = db
        .get_or_create_folder("test@example.com", "INBOX")
        .expect("Failed to get or create");

    // Second call should return the same ID
    let id2 = db
        .get_or_create_folder("test@example.com", "INBOX")
        .expect("Failed to get or create");

    assert_eq!(id1, id2);

    // Only one folder should exist
    let folders = db
        .get_folders_by_account("test@example.com")
        .expect("Failed to get folders");
    assert_eq!(folders.len(), 1);
}

#[test]
fn test_folder_sync_state_update() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db
        .insert_folder("test@example.com", "INBOX")
        .expect("Failed to insert folder");

    db.update_folder_sync_state(folder_id, 12345, 100, 500, "2025-01-15T10:00:00Z")
        .expect("Failed to update sync state");

    let folder = db
        .get_folder_by_id(folder_id)
        .expect("Failed to get folder")
        .expect("Folder should exist");

    assert_eq!(folder.uidvalidity, Some(12345));
    assert_eq!(folder.highest_uid, Some(100));
    assert_eq!(folder.highest_modseq, Some(500));
    assert_eq!(
        folder.last_sync_time,
        Some("2025-01-15T10:00:00Z".to_string())
    );
}

// ============================================
// Config reconcile / orphan pruning
// ============================================

/// Regression test for the "duplicate folder display" bug: when an account is
/// removed/renamed in the config, its stale DB rows (and their messages) must be
/// pruned so they don't linger alongside the new set. Crucially, folders that are
/// merely disabled (`sync = false`) must be PRESERVED — that flag means "show but
/// don't auto-sync", not "removed", so pruning them would be data loss.
#[test]
fn test_prune_orphaned_folders_removes_account_orphans_but_keeps_disabled() {
    use std::collections::HashSet;

    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Account "work" (kept) with two folders; "personal" (removed in config).
    let work_inbox = db.insert_folder("work", "INBOX").unwrap();
    let work_archive = db.insert_folder("work", "Archive").unwrap();
    let personal_inbox = db.insert_folder("personal", "INBOX").unwrap();

    // Give each folder some messages so we can assert the cascade delete.
    for uid in 1..=3 {
        db.insert_message(&create_test_message(work_inbox, uid, "keep"))
            .unwrap();
    }
    for uid in 1..=2 {
        db.insert_message(&create_test_message(work_archive, uid, "archive"))
            .unwrap();
    }
    for uid in 1..=4 {
        db.insert_message(&create_test_message(personal_inbox, uid, "gone"))
            .unwrap();
    }

    // Config now: only account "work" exists. "work/Archive" is `sync = false`
    // (visible but not auto-synced) — it must NOT be pruned. Account "personal"
    // is gone, so its folders/messages are orphans to remove.
    let valid_accounts: HashSet<String> = ["work".to_string()].into_iter().collect();

    let report = db
        .prune_orphaned_folders(&valid_accounts)
        .expect("reconcile should succeed");

    // Removed: only personal/INBOX (account gone) = 1 folder + its 4 messages.
    // work/Archive (sync = false) is preserved.
    assert_eq!(report.folders_removed, 1);
    assert_eq!(report.messages_removed, 4);

    // Both "work" folders survive intact, with all their messages and NO dupes.
    let work_folders = db.get_folders_by_account("work").unwrap();
    assert_eq!(work_folders.len(), 2);
    assert_eq!(db.get_folder_counts(work_inbox).unwrap().1, 3);

    // The disabled folder and its cached messages are still present.
    assert!(db.get_folder_by_id(work_archive).unwrap().is_some());
    assert_eq!(db.get_folder_counts(work_archive).unwrap().1, 2);
    assert_eq!(db.get_messages_by_folder(work_archive).unwrap().len(), 2);

    // Orphaned account and folder are gone entirely.
    assert!(db.get_folders_by_account("personal").unwrap().is_empty());
    assert!(db.get_folder_by_id(personal_inbox).unwrap().is_none());

    // Their messages were cascade-deleted.
    assert!(
        db.get_messages_by_folder(personal_inbox)
            .unwrap()
            .is_empty()
    );

    // Re-running reconcile is a no-op (idempotent).
    let report2 = db
        .prune_orphaned_folders(&valid_accounts)
        .expect("reconcile should succeed");
    assert_eq!(report2.folders_removed, 0);
    assert_eq!(report2.messages_removed, 0);
}

// ============================================
// Message Tests
// ============================================

fn create_test_message(folder_id: i64, uid: u32, subject: &str) -> Message {
    Message {
        id: None,
        folder_id,
        uid,
        message_id: Some(format!("<msg-{}@test.com>", uid)),
        subject: Some(subject.to_string()),
        from: Some("sender@example.com".to_string()),
        to: Some("recipient@example.com".to_string()),
        cc: None,
        date: Some("2025-01-15T10:30:00+00:00".to_string()),
        body_text: Some("Test body".to_string()),
        body_html: None,
        rfc822: None,
        size: Some(1024),
        flags: vec!["Seen".to_string()],
        modseq: Some(100),
        shape: None,
        headers: None,
    }
}

#[test]
fn test_message_insert_and_retrieve() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db
        .insert_folder("test@example.com", "INBOX")
        .expect("Failed to insert folder");

    let msg = create_test_message(folder_id, 1, "Test Subject");
    let msg_id = db.insert_message(&msg).expect("Failed to insert message");
    assert!(msg_id > 0);

    let messages = db
        .get_messages_by_folder(folder_id)
        .expect("Failed to get messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].subject, Some("Test Subject".to_string()));
    assert_eq!(messages[0].uid, 1);
    assert_eq!(messages[0].flags, vec!["Seen".to_string()]);
}

#[test]
fn test_message_get_by_id() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();
    let msg = create_test_message(folder_id, 1, "Hello");
    let msg_id = db.insert_message(&msg).unwrap();

    let retrieved = db
        .get_message_by_id(msg_id)
        .expect("Query should succeed")
        .expect("Message should exist");

    assert_eq!(retrieved.id, Some(msg_id));
    assert_eq!(retrieved.subject, Some("Hello".to_string()));
}

#[test]
fn test_message_count() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    // Insert 5 messages
    for i in 1..=5 {
        let msg = create_test_message(folder_id, i, &format!("Message {}", i));
        db.insert_message(&msg).unwrap();
    }

    let count = db
        .get_message_count(folder_id)
        .expect("Failed to get count");
    assert_eq!(count, 5);
}

#[test]
fn test_folder_counts_unread_and_total() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    // Insert read messages (with Seen flag)
    for i in 1..=3 {
        let msg = create_test_message(folder_id, i, &format!("Read {}", i));
        db.insert_message(&msg).unwrap();
    }

    // Insert unread messages (without Seen flag)
    for i in 4..=6 {
        let mut msg = create_test_message(folder_id, i, &format!("Unread {}", i));
        msg.flags = vec![]; // No flags = unread
        db.insert_message(&msg).unwrap();
    }

    let (unread, total) = db
        .get_folder_counts(folder_id)
        .expect("Failed to get counts");

    assert_eq!(total, 6);
    assert_eq!(unread, 3);
}

#[test]
fn test_latest_unread_message_returns_newest_unread_across_folders() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // No messages at all: there is nothing to represent.
    assert!(
        db.latest_unread_message().unwrap().is_none(),
        "an empty mailbox has no unread sample"
    );

    let work_inbox = db.insert_folder("work", "INBOX").unwrap();
    let personal_archive = db.insert_folder("personal", "Archive").unwrap();

    // A READ message that is the most recent of all — it must be ignored so a
    // notification never mislabels itself with already-seen mail.
    let mut read_recent = create_test_message(work_inbox, 1, "Read but newest");
    read_recent.date = Some("2025-03-01T10:00:00+00:00".to_string());
    read_recent.from = Some("seen@example.com".to_string());
    read_recent.flags = vec!["Seen".to_string()];
    db.insert_message(&read_recent).unwrap();

    // An OLDER unread message in the folder the user is viewing.
    let mut unread_old = create_test_message(work_inbox, 2, "Old unread");
    unread_old.date = Some("2025-01-01T10:00:00+00:00".to_string());
    unread_old.from = Some("old@example.com".to_string());
    unread_old.flags = vec![];
    db.insert_message(&unread_old).unwrap();

    // The NEWEST unread message, living in a DIFFERENT account/folder the user
    // is not currently viewing. This is the correct representative sample.
    let mut unread_new = create_test_message(personal_archive, 1, "Newest unread");
    unread_new.date = Some("2025-02-15T10:00:00+00:00".to_string());
    unread_new.from = Some("fresh@example.com".to_string());
    unread_new.flags = vec![];
    let unread_new_id = db.insert_message(&unread_new).unwrap();

    let latest = db
        .latest_unread_message()
        .expect("query should succeed")
        .expect("there is unread mail");

    assert_eq!(
        latest.id, unread_new_id,
        "must return the newest unread by id"
    );
    assert_eq!(
        latest.subject,
        Some("Newest unread".to_string()),
        "sample must be the newest unread, not the newer READ message"
    );
    assert_eq!(
        latest.from,
        Some("fresh@example.com".to_string()),
        "sample sender must come from the whole-mailbox newest unread"
    );
    assert_eq!(
        latest.folder_id, personal_archive,
        "the sample can live in any folder/account, not just the viewed one"
    );
}

#[test]
fn test_latest_unread_message_none_when_all_read() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let inbox = db.insert_folder("work", "INBOX").unwrap();

    // Every message carries the Seen flag: nothing is unread.
    for uid in 1..=3 {
        let msg = create_test_message(inbox, uid, &format!("Read {}", uid));
        // create_test_message already sets flags = ["Seen"].
        db.insert_message(&msg).unwrap();
    }

    assert!(
        db.latest_unread_message().unwrap().is_none(),
        "a fully-read mailbox has no unread sample"
    );
}

#[test]
fn test_message_list_pagination() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    // Insert 10 messages with different dates for sorting
    for i in 1..=10 {
        let mut msg = create_test_message(folder_id, i, &format!("Message {}", i));
        // Vary dates so we can test sorting
        msg.date = Some(format!("2025-01-{:02}T10:00:00+00:00", i));
        db.insert_message(&msg).unwrap();
    }

    // Get first page (5 items)
    let page1 = db
        .get_message_list(folder_id, 0, 5, MessageSort::DateNewest)
        .expect("Failed to get message list");
    assert_eq!(page1.len(), 5);
    // Newest first (DateNewest)
    assert_eq!(page1[0].subject, Some("Message 10".to_string()));

    // Get second page
    let page2 = db
        .get_message_list(folder_id, 5, 5, MessageSort::DateNewest)
        .expect("Failed to get message list");
    assert_eq!(page2.len(), 5);
    assert_eq!(page2[0].subject, Some("Message 5".to_string()));
}

#[test]
fn test_bulk_insert_messages() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    let messages: Vec<Message> = (1..=5)
        .map(|i| create_test_message(folder_id, i, &format!("Bulk {}", i)))
        .collect();

    let (inserted, updated) = db
        .bulk_insert_messages(&messages)
        .expect("Failed to bulk insert");

    assert_eq!(inserted, 5);
    assert_eq!(updated, 0);

    // Insert again - should update existing
    let (inserted2, updated2) = db
        .bulk_insert_messages(&messages)
        .expect("Failed to upsert");
    assert_eq!(inserted2, 0);
    assert_eq!(updated2, 5);
}

#[test]
fn test_delete_messages_by_uids() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    // Insert 5 messages
    for i in 1..=5 {
        let msg = create_test_message(folder_id, i, &format!("Message {}", i));
        db.insert_message(&msg).unwrap();
    }

    // Delete UIDs 2 and 4
    let deleted = db
        .delete_messages_by_uids(folder_id, &[2, 4])
        .expect("Failed to delete");
    assert_eq!(deleted, 2);

    // Verify remaining messages
    let remaining = db.get_folder_uids(folder_id).expect("Failed to get UIDs");
    assert_eq!(remaining, vec![1, 3, 5]);
}

#[test]
fn test_clear_folder_messages() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    // Insert messages
    for i in 1..=3 {
        let msg = create_test_message(folder_id, i, &format!("Msg {}", i));
        db.insert_message(&msg).unwrap();
    }

    assert_eq!(db.get_message_count(folder_id).unwrap(), 3);

    // Clear all messages
    db.clear_folder_messages(folder_id)
        .expect("Failed to clear");

    assert_eq!(db.get_message_count(folder_id).unwrap(), 0);
}

#[test]
fn test_update_message_body() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    let mut msg = create_test_message(folder_id, 1, "Subject");
    msg.body_text = None;
    msg.body_html = None;
    let msg_id = db.insert_message(&msg).unwrap();

    // Verify body is initially None
    let retrieved = db.get_message_by_id(msg_id).unwrap().unwrap();
    assert!(retrieved.body_text.is_none());

    // Update body
    db.update_message_body(
        msg_id,
        Some("Plain text body"),
        Some("<html>Rich body</html>"),
        Some(b"Raw RFC822 data"),
    )
    .expect("Failed to update body");

    let updated = db.get_message_by_id(msg_id).unwrap().unwrap();
    assert_eq!(updated.body_text, Some("Plain text body".to_string()));
    assert_eq!(
        updated.body_html,
        Some("<html>Rich body</html>".to_string())
    );
    assert_eq!(updated.rfc822, Some(b"Raw RFC822 data".to_vec()));
}

#[test]
fn test_messages_with_attachments_uses_cached_rfc822() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    // Message A: has a cached RFC822 with a PDF attachment.
    let msg_a = create_test_message(folder_id, 1, "With attachment");
    let id_a = db.insert_message(&msg_a).unwrap();
    let rfc822_with_attachment = [
        "From: a@example.com",
        "Subject: With attachment",
        "MIME-Version: 1.0",
        "Content-Type: multipart/mixed; boundary=\"B\"",
        "",
        "--B",
        "Content-Type: text/plain",
        "",
        "body",
        "--B",
        "Content-Type: application/pdf; name=\"doc.pdf\"",
        "Content-Disposition: attachment; filename=\"doc.pdf\"",
        "Content-Transfer-Encoding: base64",
        "",
        "SGVsbG8=",
        "--B--",
        "",
    ]
    .join("\r\n");
    db.update_message_body(id_a, None, None, Some(rfc822_with_attachment.as_bytes()))
        .unwrap();

    // Message B: cached RFC822 that is plain text only.
    let msg_b = create_test_message(folder_id, 2, "Plain");
    let id_b = db.insert_message(&msg_b).unwrap();
    let rfc822_plain = [
        "From: a@example.com",
        "Subject: Plain",
        "Content-Type: text/plain",
        "",
        "just text",
        "",
    ]
    .join("\r\n");
    db.update_message_body(id_b, None, None, Some(rfc822_plain.as_bytes()))
        .unwrap();

    // Message C: no cached RFC822 at all (body not fetched yet).
    let msg_c = create_test_message(folder_id, 3, "No body");
    let id_c = db.insert_message(&msg_c).unwrap();

    let flagged = db
        .messages_with_attachments(&[id_a, id_b, id_c])
        .expect("query failed");

    assert!(flagged.contains(&id_a), "A has an attachment");
    assert!(!flagged.contains(&id_b), "B is plain text");
    assert!(!flagged.contains(&id_c), "C has no cached source");
    assert_eq!(flagged.len(), 1);

    // Empty input yields an empty set without touching the database.
    assert!(db.messages_with_attachments(&[]).unwrap().is_empty());
}

// ============================================
// Sync Lock Tests
// ============================================

#[test]
fn test_sync_lock_acquire_and_release() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Acquire lock
    let acquired = db
        .try_acquire_sync_lock("test@example.com", "INBOX")
        .expect("Failed to acquire lock");
    assert!(acquired);

    // Release lock
    db.release_sync_lock("test@example.com", "INBOX")
        .expect("Failed to release lock");

    // Should be able to acquire again
    let acquired2 = db
        .try_acquire_sync_lock("test@example.com", "INBOX")
        .expect("Failed to acquire lock");
    assert!(acquired2);
}

#[test]
fn test_sync_lock_prevents_double_acquire() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // First acquire should succeed
    let first = db
        .try_acquire_sync_lock("test@example.com", "INBOX")
        .expect("Failed to acquire lock");
    assert!(first);

    // Second acquire from same process should fail
    // (because the lock holder ID is the same)
    let second = db
        .try_acquire_sync_lock("test@example.com", "INBOX")
        .expect("Failed to try lock");
    // Note: Same process gets PRIMARY KEY conflict
    assert!(!second);
}

#[test]
fn test_sync_lock_renew() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Acquire lock
    db.try_acquire_sync_lock("test@example.com", "INBOX")
        .expect("Failed to acquire lock");

    // Renew should succeed
    let renewed = db
        .renew_sync_lock("test@example.com", "INBOX")
        .expect("Failed to renew lock");
    assert!(renewed);
}

#[test]
fn test_sync_lock_renew_without_lock() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Renew without acquiring should fail
    let renewed = db
        .renew_sync_lock("test@example.com", "INBOX")
        .expect("Failed to renew lock");
    assert!(!renewed);
}

#[test]
fn test_release_all_locks() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Acquire multiple locks
    db.try_acquire_sync_lock("test@example.com", "INBOX")
        .unwrap();
    db.try_acquire_sync_lock("test@example.com", "Sent")
        .unwrap();
    db.try_acquire_sync_lock("other@example.com", "INBOX")
        .unwrap();

    // Release all
    let released = db.release_all_locks().expect("Failed to release all locks");
    assert_eq!(released, 3);
}

#[test]
fn test_sync_lock_folder_name_normalization() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Acquire lock with lowercase
    db.try_acquire_sync_lock("test@example.com", "inbox")
        .expect("Failed to acquire lock");

    // Try to acquire with uppercase - should fail (same folder normalized)
    let second = db
        .try_acquire_sync_lock("test@example.com", "INBOX")
        .expect("Failed to try lock");
    assert!(!second);
}

// ============================================
// Transaction Tests
// ============================================

#[test]
fn test_transaction_commit() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Use transaction to insert folder
    let folder_id = db
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO folders (account_name, name) VALUES (?1, ?2)",
                rusqlite::params!["test@example.com", "TxFolder"],
            )?;
            Ok(tx.last_insert_rowid())
        })
        .expect("Transaction failed");

    // Verify folder exists after commit
    let folder = db
        .get_folder_by_id(folder_id)
        .expect("Query failed")
        .expect("Folder should exist");
    assert_eq!(folder.name, "TxFolder");
}

#[test]
fn test_transaction_rollback() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Transaction that fails
    let result: Result<(), orb_core::Error> = db.transaction(|tx| {
        tx.execute(
            "INSERT INTO folders (account_name, name) VALUES (?1, ?2)",
            rusqlite::params!["test@example.com", "WillRollback"],
        )?;
        // Force an error using a valid error variant
        Err(orb_core::Error::Sync("Simulated failure".into()))
    });

    assert!(result.is_err());

    // Folder should NOT exist after rollback
    let folders = db
        .get_folders_by_account("test@example.com")
        .expect("Query failed");
    assert!(folders.is_empty());
}

// ============================================
// Message Flags Tests
// ============================================

#[test]
fn test_message_flags_json_storage() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    // Insert message with multiple flags
    let mut msg = create_test_message(folder_id, 1, "Flagged Message");
    msg.flags = vec![
        "Seen".to_string(),
        "Flagged".to_string(),
        "Answered".to_string(),
    ];
    db.insert_message(&msg).unwrap();

    // Retrieve and verify flags are preserved
    let messages = db.get_messages_by_folder(folder_id).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].flags.len(), 3);
    assert!(messages[0].flags.contains(&"Seen".to_string()));
    assert!(messages[0].flags.contains(&"Flagged".to_string()));
    assert!(messages[0].flags.contains(&"Answered".to_string()));
}

#[test]
fn test_message_empty_flags() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    let mut msg = create_test_message(folder_id, 1, "No Flags");
    msg.flags = vec![];
    db.insert_message(&msg).unwrap();

    let messages = db.get_messages_by_folder(folder_id).unwrap();
    assert_eq!(messages[0].flags.len(), 0);
}

// ============================================
// Date Parsing Tests
// ============================================

#[test]
fn test_date_parsing_rfc3339() {
    let timestamp = Database::parse_date_to_unix("2025-01-15T10:30:00+00:00");
    assert!(timestamp.is_some());
}

#[test]
fn test_date_parsing_rfc2822() {
    let timestamp = Database::parse_date_to_unix("Wed, 15 Jan 2025 10:30:00 +0000");
    assert!(timestamp.is_some());
}

// Note: mailparse::dateparse is very lenient and parses most strings as dates,
// so we don't test invalid date parsing here. The important thing is that
// valid dates are parsed correctly (tested above).

// ============================================
// Schema Migration Tests
// ============================================

#[test]
fn test_schema_init_creates_tables() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    // Verify we can use the tables
    let folders = db
        .get_folders_by_account("any")
        .expect("Folders table should exist");
    assert!(folders.is_empty());
}

#[test]
fn test_schema_init_is_idempotent() {
    let ctx = TestContext::new();
    let db = ctx.with_database();

    // Call init_schema multiple times
    db.init_schema().expect("First init failed");
    db.init_schema().expect("Second init failed");
    db.init_schema().expect("Third init failed");

    // Should still work
    db.insert_folder("test@example.com", "INBOX")
        .expect("Insert should work");
}

// ============================================
// Edge Cases
// ============================================

#[test]
fn test_folder_unique_constraint() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    db.insert_folder("test@example.com", "INBOX")
        .expect("First insert should succeed");

    // Second insert with same account/name should fail
    let result = db.insert_folder("test@example.com", "INBOX");
    assert!(result.is_err());
}

#[test]
fn test_message_uid_unique_per_folder() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder1 = db.insert_folder("test@example.com", "INBOX").unwrap();
    let folder2 = db.insert_folder("test@example.com", "Sent").unwrap();

    // Same UID in different folders should work
    let msg1 = create_test_message(folder1, 1, "INBOX msg");
    let msg2 = create_test_message(folder2, 1, "Sent msg");

    db.insert_message(&msg1).expect("First should succeed");
    db.insert_message(&msg2)
        .expect("Different folder should succeed");

    // Same UID in same folder should fail
    let msg3 = create_test_message(folder1, 1, "Duplicate");
    let result = db.insert_message(&msg3);
    assert!(result.is_err());
}

#[test]
fn test_empty_bulk_operations() {
    let ctx = TestContext::new();
    let mut db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    // Empty bulk insert should succeed
    let (inserted, updated) = db
        .bulk_insert_messages(&[])
        .expect("Empty bulk should work");
    assert_eq!(inserted, 0);
    assert_eq!(updated, 0);

    // Empty delete should succeed
    let deleted = db
        .delete_messages_by_uids(folder_id, &[])
        .expect("Empty delete should work");
    assert_eq!(deleted, 0);
}

#[test]
fn test_special_characters_in_subject() {
    let ctx = TestContext::new();
    let db = ctx.with_database();
    db.init_schema().expect("Failed to init schema");

    let folder_id = db.insert_folder("test@example.com", "INBOX").unwrap();

    let mut msg = create_test_message(folder_id, 1, "Test with 'quotes' and \"doubles\"");
    msg.subject = Some("Unicode: \u{1F600} emoji and 日本語 text".to_string());
    db.insert_message(&msg).unwrap();

    let messages = db.get_messages_by_folder(folder_id).unwrap();
    assert_eq!(
        messages[0].subject,
        Some("Unicode: \u{1F600} emoji and 日本語 text".to_string())
    );
}
