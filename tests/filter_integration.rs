//! The filter rules against a Dovecot container: what a rule does to new
//! mail, what it leaves alone, and what two Orbs on one account do to the
//! same message.

mod common;

use common::imap_container::{DovecotContainer, create_rfc822_message};
use common::orb_config::config_with;
use common::test_context::TestContext;
use orb_core::{Database, Store};

fn message(from: &str, subject: &str) -> Vec<u8> {
    create_rfc822_message(from, "testuser@example.com", subject, "Body")
}

fn db(ctx: &TestContext) -> Database {
    Database::new(ctx.db_path.to_str().unwrap()).expect("open the database")
}

fn stored_uids(ctx: &TestContext, folder: &str) -> Vec<u32> {
    let db = db(ctx);
    let folder = db
        .get_folder_by_name("Test", folder)
        .expect("query the folder")
        .unwrap_or_else(|| panic!("{folder} is not in the database"));
    let mut stmt = db
        .connection()
        .prepare("SELECT uid FROM messages WHERE folder_id = ?1 ORDER BY uid")
        .unwrap();
    stmt.query_map([folder.id.unwrap()], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

async fn sync_inbox(store: &Store) -> orb_core::SyncReport {
    let report = store
        .sync(Some("Test".into()), Some("INBOX".into()), None, false)
        .await
        .expect("sync INBOX");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    report
}

/// The flags of every message of a folder, by sequence.
async fn flags(container: &DovecotContainer, folder: &str) -> Vec<Vec<String>> {
    if container.get_message_count(folder).await.expect("count") == 0 {
        return Vec::new();
    }
    container.fetch_flags(folder, "1:*").await.expect("flags")
}

const RULES: &str = "\
filters:
  - name: News
    when:
      from: '*@news.example.com'
    then:
      move: News
      read: true
  - name: Boss
    when:
      from: boss@example.com
    then:
      flag: true
      stop: true
  - name: Everything
    then:
      read: true
";

/// A rule runs on mail that arrives after it, once: the old mail of a folder is
/// never touched, a message is filtered by the sync that brought it and by no
/// later one, `stop` keeps the rules after it from seeing the message, and a
/// message moved back by hand is not filed away a second time — while
/// `:filter` runs the rules regardless.
#[tokio::test]
#[ignore = "requires Docker"]
async fn rules_run_once_on_new_mail_and_stop_where_told() {
    let container = DovecotContainer::start().await.expect("start Dovecot");
    container
        .prepopulate_mailbox(
            "INBOX",
            &[
                message("old@news.example.com", "Old news"),
                message("boss@example.com", "Old order"),
            ],
        )
        .await
        .expect("old mail");
    container.create_folder("News").await.expect("News");

    let ctx = TestContext::new();
    let store = Store::new(config_with(
        &ctx,
        &container.host,
        container.imaps_port,
        RULES,
    ))
    .expect("open the store");

    // The first sync finds mail that was there before Orb: none of it is
    // filtered, however well it matches, and none of it is news.
    let report = sync_inbox(&store).await;
    assert_eq!(report.messages_filtered, 0);
    assert!(report.new_mail.is_empty(), "{:?}", report.new_mail);
    assert_eq!(container.get_message_count("INBOX").await.unwrap(), 2);
    assert_eq!(container.get_message_count("News").await.unwrap(), 0);
    assert!(
        flags(&container, "INBOX")
            .await
            .iter()
            .all(|flags| flags.is_empty()),
        "old mail is left as it was"
    );

    container
        .prepopulate_mailbox(
            "INBOX",
            &[
                message("desk@news.example.com", "Weekly digest"),
                message("boss@example.com", "Now"),
                message("friend@example.com", "Hi"),
            ],
        )
        .await
        .expect("new mail");
    let report = sync_inbox(&store).await;
    assert_eq!(report.messages_filtered, 3, "{:?}", report.filter_warnings);
    assert!(
        report.filter_warnings.is_empty(),
        "{:?}",
        report.filter_warnings
    );

    // The digest is in News and read; the boss is flagged and, because of
    // `stop`, not read; the friend is read by the catch-all.
    assert_eq!(container.get_message_count("News").await.unwrap(), 1);
    assert_eq!(
        flags(&container, "News").await,
        vec![vec!["Seen".to_string()]]
    );
    let inbox = flags(&container, "INBOX").await;
    assert_eq!(inbox.len(), 4, "{inbox:?}");
    assert_eq!(inbox[2], vec!["Flagged".to_string()], "the boss: {inbox:?}");
    assert_eq!(inbox[3], vec!["Seen".to_string()], "the friend: {inbox:?}");
    assert_eq!(
        stored_uids(&ctx, "INBOX").len(),
        4,
        "the move is reflected here"
    );
    // What the sync announces is what the rules left unread in INBOX: the
    // digest has gone to News, the friend is read, the boss is news.
    assert_eq!(
        report
            .new_mail
            .iter()
            .map(|m| m.subject.as_deref().unwrap_or(""))
            .collect::<Vec<_>>(),
        vec!["Now"]
    );

    // Another sync does nothing to the same mail: the boss stays unread, and
    // nothing is announced twice.
    let report = sync_inbox(&store).await;
    assert_eq!(report.messages_filtered, 0);
    assert!(report.new_mail.is_empty(), "{:?}", report.new_mail);
    assert_eq!(flags(&container, "INBOX").await, inbox);

    // What the panel lists: the last run that did anything.
    let activity = store.filter_activity().expect("activity");
    let listed: Vec<(String, String)> = activity
        .iter()
        .map(|a| (a.rule.clone(), a.actions.clone()))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("News".to_string(), "read, move to News".to_string()),
            ("Boss".to_string(), "flag, stop".to_string()),
            ("Everything".to_string(), "read".to_string()),
        ]
    );
    assert_eq!(activity[0].subject.as_deref(), Some("Weekly digest"));

    // The digest moved back by hand gets a new UID above the watermark, and is
    // still left where it was put.
    store
        .sync(Some("Test".into()), Some("News".into()), None, false)
        .await
        .expect("sync News");
    let news_uid = stored_uids(&ctx, "News")[0];
    store
        .move_immediate("Test", "News", "INBOX", &[news_uid])
        .await
        .expect("move it back");
    let report = sync_inbox(&store).await;
    assert_eq!(report.messages_filtered, 0, "a message moved back stays");
    assert_eq!(container.get_message_count("INBOX").await.unwrap(), 5);
    assert_eq!(container.get_message_count("News").await.unwrap(), 0);

    // `:filter` is the request to run the rules on what is here, and does.
    let outcome = store
        .run_filters("Test", "INBOX", None)
        .await
        .expect("run the filters");
    assert_eq!(outcome.filtered, 5, "{:?}", outcome.warnings);
    assert_eq!(container.get_message_count("News").await.unwrap(), 2);
    assert_eq!(container.get_message_count("INBOX").await.unwrap(), 3);
    assert!(
        flags(&container, "INBOX")
            .await
            .iter()
            .all(|flags| !flags.is_empty()),
        "every message left is read or flagged"
    );

    // What the run did is described for undo, and the move is found again
    // in its folder by the messages' Message-IDs.
    let moves: Vec<_> = outcome
        .undo
        .iter()
        .filter_map(|step| match step {
            orb_core::undo::Step::Move {
                from,
                to,
                message_ids,
                ..
            } => Some((from.clone(), to.clone(), message_ids.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(moves.len(), 1, "{:?}", outcome.undo);
    let (from, to, ids) = &moves[0];
    assert_eq!(
        (from.as_str(), to.as_str(), ids.len()),
        ("News", "INBOX", 2)
    );
    store
        .move_by_message_ids("Test", from, to, ids)
        .await
        .expect("undo the move");
    assert_eq!(container.get_message_count("News").await.unwrap(), 0);
    assert!(
        outcome
            .undo
            .iter()
            .any(|step| matches!(step, orb_core::undo::Step::Read { read: false, .. })),
        "the read is undone by marking unread: {:?}",
        outcome.undo
    );
}

/// A rule that moves to a folder the account does not have is a warning —
/// at startup, and again when it fails to run — and its other actions still
/// happen.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_rule_whose_folder_is_missing_warns_and_does_the_rest() {
    let container = DovecotContainer::start().await.expect("start Dovecot");
    let ctx = TestContext::new();
    let rules = "\
filters:
  - name: Lists
    when:
      list_id: '*'
    then:
      move: Lists
      read: true
";
    let store = Store::new(config_with(
        &ctx,
        &container.host,
        container.imaps_port,
        rules,
    ))
    .expect("open the store");
    sync_inbox(&store).await;
    let warnings = store.filter_warnings().expect("warnings");
    assert_eq!(
        warnings,
        vec!["filter 'Lists': Test has no folder 'Lists' to move to".to_string()]
    );

    let mut listed = message("announce@example.com", "Release");
    let headers_end = listed.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    listed.splice(
        headers_end..headers_end,
        b"\r\nList-Id: Announcements <announce.example.com>"
            .iter()
            .copied(),
    );
    container
        .prepopulate_mailbox("INBOX", &[listed])
        .await
        .expect("a list message");
    let report = sync_inbox(&store).await;
    assert_eq!(report.messages_filtered, 1);
    assert_eq!(
        report.filter_warnings,
        vec!["filter 'Lists' on Test/INBOX: Test has no folder 'Lists'".to_string()]
    );
    assert_eq!(container.get_message_count("INBOX").await.unwrap(), 1);
    assert_eq!(
        flags(&container, "INBOX").await,
        vec![vec!["Seen".to_string()]]
    );
    let activity = store.filter_activity().expect("activity");
    assert_eq!(activity.len(), 1);
    assert!(
        activity[0].actions.contains("(failed:"),
        "{}",
        activity[0].actions
    );
}

/// Two Orbs on one account, each with its own database, apply the same rule
/// to the same message: the message is filed once, and the task is created
/// by the Orb that filed it.
#[tokio::test]
#[ignore = "requires Docker"]
async fn two_machines_file_a_message_once_and_make_one_task() {
    let container = DovecotContainer::start().await.expect("start Dovecot");
    container.create_folder("News").await.expect("News");
    container.create_folder("Tasks").await.expect("Tasks");
    let rule = |folder: &str| {
        format!(
            "\
tasks:
  account: Test
  folder: Tasks
filters:
  - name: News
    folders: [{folder}]
    when:
      from: '*@news.example.com'
    then:
      move: News
      task: true
"
        )
    };

    let ctx_a = TestContext::new();
    let ctx_b = TestContext::new();
    let a = Store::new(config_with(
        &ctx_a,
        &container.host,
        container.imaps_port,
        &rule("INBOX"),
    ))
    .expect("open A");
    // B's rule watches a folder nothing arrives in, so B stores the message
    // without acting on it — the state of an Orb that synced before the
    // other one filed the message.
    let b = Store::new(config_with(
        &ctx_b,
        &container.host,
        container.imaps_port,
        &rule("Elsewhere"),
    ))
    .expect("open B");
    sync_inbox(&a).await;
    sync_inbox(&b).await;

    container
        .prepopulate_mailbox("INBOX", &[message("desk@news.example.com", "Digest")])
        .await
        .expect("new mail");
    let report = sync_inbox(&b).await;
    assert_eq!(report.messages_filtered, 0);
    assert_eq!(stored_uids(&ctx_b, "INBOX").len(), 1, "B has the message");

    let report = sync_inbox(&a).await;
    assert_eq!(report.messages_filtered, 1, "{:?}", report.filter_warnings);
    assert!(
        report.filter_warnings.is_empty(),
        "{:?}",
        report.filter_warnings
    );
    assert_eq!(container.get_message_count("News").await.unwrap(), 1);
    assert_eq!(task_count(&ctx_a), 1, "A made the task");

    // B now runs the rule on the message it still has: the server no longer
    // has it in INBOX, so B does nothing and makes no task of its own — and
    // stops listing the message, rather than wait for a sync to notice.
    b.replace_config(std::sync::Arc::new(config_with(
        &ctx_b,
        &container.host,
        container.imaps_port,
        &rule("INBOX"),
    )));
    let outcome = b
        .run_filters("Test", "INBOX", None)
        .await
        .expect("B runs the filters");
    assert_eq!(outcome.filtered, 0, "{:?}", outcome.warnings);
    assert_eq!(task_count(&ctx_b), 0);
    assert_eq!(container.get_message_count("News").await.unwrap(), 1);
    assert!(
        stored_uids(&ctx_b, "INBOX").is_empty(),
        "B no longer lists the message A filed"
    );

    // A, asked again, finds a task linked already and leaves it at one.
    let outcome = a
        .run_filters("Test", "News", None)
        .await
        .expect("A runs the filters");
    assert_eq!(outcome.filtered, 0, "News is not a folder the rule watches");
    let report = sync_inbox(&a).await;
    assert_eq!(report.messages_filtered, 0);
    assert_eq!(task_count(&ctx_a), 1);
}

fn task_count(ctx: &TestContext) -> i64 {
    db(ctx)
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count tasks")
}

/// The reading folders are made by the first full sync, and a rule with
/// `reading` files a newsletter there.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_reading_rule_files_a_newsletter_in_the_reading_folder() {
    let container = DovecotContainer::start().await.expect("start Dovecot");
    let ctx = TestContext::new();
    let extra = "\
reading:
  account: Test
filters:
  - name: Newsletters
    when:
      list_id: '*'
    then:
      reading: true
";
    let store = Store::new(config_with(
        &ctx,
        &container.host,
        container.imaps_port,
        extra,
    ))
    .expect("open the store");
    let report = store
        .sync(Some("Test".into()), None, None, false)
        .await
        .expect("sync the account");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let (_, reading) = store.reading_folder(false).expect("the reading folder");
    let (_, archive) = store.reading_folder(true).expect("the reading archive");
    assert_eq!(container.get_message_count(&reading).await.unwrap(), 0);
    assert_eq!(container.get_message_count(&archive).await.unwrap(), 0);

    let mut listed = message("desk@news.example.com", "Weekly digest");
    let headers_end = listed.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    listed.splice(
        headers_end..headers_end,
        b"\r\nList-Id: News <news.example.com>".iter().copied(),
    );
    container
        .prepopulate_mailbox("INBOX", &[listed])
        .await
        .expect("a newsletter");
    let report = sync_inbox(&store).await;
    assert_eq!(report.messages_filtered, 1, "{:?}", report.filter_warnings);
    assert!(
        report.filter_warnings.is_empty(),
        "{:?}",
        report.filter_warnings
    );
    assert_eq!(container.get_message_count("INBOX").await.unwrap(), 0);
    assert_eq!(container.get_message_count(&reading).await.unwrap(), 1);
}
