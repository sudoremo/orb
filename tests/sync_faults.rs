//! The sync under a network that misbehaves.
//!
//! Each test puts a fault-injecting proxy between the real `Store` and the
//! Dovecot container and asserts one thing: that the operation *ends*, within
//! a bound, with a visible error or a recovery. A sync that never returns is
//! the failure these exist to reproduce, so every test is wrapped in a
//! timeout that fails it with a plain message rather than hanging the suite.
//!
//! Run with: `cargo test --test sync_faults -- --ignored --nocapture`

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::imap_container::{DovecotContainer, create_rfc822_message};
use common::orb_config::config_for;
use common::proxy::{Fault, FaultProxy};
use common::test_context::TestContext;
use orb_core::config::Config;
use orb_core::imap::trace;
use orb_core::{StateUpdate, Store};
use tokio::time::timeout;

/// The bound every operation has to end within. Generous against the 60 s
/// operation timeout every IMAP command carries.
const BOUND: Duration = Duration::from_secs(90);

/// How long the IDLE tests let one IDLE run before it is re-issued. The
/// re-issue is how a frozen connection is found out, so the tests shorten it
/// from the ten minutes of use to seconds.
const IDLE_CYCLE: Duration = Duration::from_secs(15);

/// Enough messages that a FETCH of the folder outlasts the first few
/// kilobytes the proxy lets through.
const MESSAGES: usize = 2000;

/// Where in the server's byte stream the fault strikes: past the handshake,
/// LOGIN, LIST, STATUS and SELECT, and well inside the FETCH.
const MID_FETCH: u64 = 64 * 1024;

struct Setup {
    _ctx: TestContext,
    container: DovecotContainer,
    proxy: FaultProxy,
    config: Config,
}

async fn setup(messages: usize) -> Setup {
    let container = DovecotContainer::start()
        .await
        .expect("start the Dovecot container");
    if messages > 0 {
        let batch: Vec<Vec<u8>> = (0..messages)
            .map(|i| {
                create_rfc822_message(
                    "sender@example.com",
                    "testuser@example.com",
                    &format!("Message {i}"),
                    "A body of modest length, repeated for every message.",
                )
            })
            .collect();
        container
            .prepopulate_mailbox("INBOX", &batch)
            .await
            .expect("populate INBOX");
    }
    let proxy = FaultProxy::start(&container.host, container.imaps_port).await;
    let ctx = TestContext::new();
    let config = config_for(&ctx, "127.0.0.1", proxy.port);
    Setup {
        _ctx: ctx,
        container,
        proxy,
        config,
    }
}

fn store(config: &Config) -> Store {
    Store::new(config.clone()).expect("open the store")
}

/// Run a sync of `folder` and insist that it ends.
async fn bounded_sync(store: &Store, folder: &str) -> (orb_core::SyncReport, Duration) {
    let started = Instant::now();
    let result = timeout(
        BOUND,
        store.sync(Some("Test".into()), Some(folder.into()), None, false),
    )
    .await;
    let elapsed = started.elapsed();
    let report = result
        .unwrap_or_else(|_| panic!("the sync of {folder} did not end within {BOUND:?}; it is hung"))
        .expect("a sync returns a report even when a folder fails");
    (report, elapsed)
}

fn stored_count(config: &Config, folder: &str) -> usize {
    let db = orb_core::Database::new(&config.db_path).expect("open the database");
    let folder = db
        .get_folder_by_name("Test", folder)
        .expect("query the folder")
        .expect("the folder exists locally");
    db.get_message_count(folder.id.expect("folder id"))
        .expect("count the messages")
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn a_dropped_socket_mid_fetch_is_retried_and_the_folder_completes() {
    let setup = setup(MESSAGES).await;
    let store = store(&setup.config);
    let before = trace::connections_opened();

    setup.proxy.plan(Fault::Drop, MID_FETCH).await;
    let (report, elapsed) = bounded_sync(&store, "INBOX").await;

    println!(
        "dropped mid-fetch: ended in {elapsed:?}, errors={:?}, connections opened={}",
        report.errors,
        trace::connections_opened() - before
    );
    println!("{}", trace::report());
    assert!(
        report.errors.is_empty(),
        "the retry should have recovered: {:?}",
        report.errors
    );
    assert_eq!(stored_count(&setup.config, "INBOX"), MESSAGES);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn a_frozen_socket_mid_fetch_ends_within_the_operation_timeout() {
    let setup = setup(MESSAGES).await;
    let store = store(&setup.config);

    setup.proxy.plan(Fault::Freeze, MID_FETCH).await;
    let (report, elapsed) = bounded_sync(&store, "INBOX").await;

    println!(
        "frozen mid-fetch: ended in {elapsed:?}, errors={:?}",
        report.errors
    );
    println!("{}", trace::report());
    // Ending at all is the point; whether the retry then completed the folder
    // is reported, not required.
    assert!(
        elapsed <= Duration::from_secs(orb_core::constants::IMAP_OPERATION_TIMEOUT_SECONDS + 15),
        "a frozen FETCH took {elapsed:?} to end; the operation timeout is {}s",
        orb_core::constants::IMAP_OPERATION_TIMEOUT_SECONDS
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn a_pooled_connection_that_died_during_sleep_does_not_hang_the_next_sync() {
    let setup = setup(50).await;
    let store = store(&setup.config);

    let (first, _) = bounded_sync(&store, "INBOX").await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(setup.proxy.connections_accepted(), 1);

    // The machine sleeps; the NAT forgets the flow; the pooled connection is
    // still "fresh" by the pool's clock when the next sync asks for it.
    setup.proxy.freeze_all().await;
    let (second, elapsed) = bounded_sync(&store, "INBOX").await;

    println!(
        "dead pooled connection: second sync ended in {elapsed:?}, errors={:?}, \
         connections={}",
        second.errors,
        setup.proxy.connections_accepted()
    );
    println!("{}", trace::report());
    assert!(
        second.errors.is_empty(),
        "the dead connection should have been replaced and the sync completed: {:?}",
        second.errors
    );
    assert_eq!(
        setup.proxy.connections_accepted(),
        2,
        "one new connection, to replace the dead one"
    );
}

// ---------------------------------------------------------------------------
// The outbox
// ---------------------------------------------------------------------------

/// A flag changed while the server is out of reach changes locally at once,
/// waits in the outbox, and reaches the server with the next sync.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_flag_set_while_offline_is_kept_locally_and_reaches_the_server_later() {
    let setup = setup(3).await;
    let store = store(&setup.config);
    let (first, _) = bounded_sync(&store, "INBOX").await;
    assert_eq!(first.messages_added, 3, "{:?}", first.errors);

    // Everything open freezes, and nothing new gets through either.
    setup.proxy.freeze_all().await;
    setup.proxy.refuse_new().await;

    let started = Instant::now();
    timeout(BOUND, store.set_read_status("Test", "INBOX", &[1, 2], true))
        .await
        .expect("marking read must end even with the server gone")
        .expect("marking read while offline is not an error");
    println!("offline mark-read returned after {:?}", started.elapsed());

    let db = orb_core::Database::new(&setup.config.db_path).expect("open the database");
    let folder = db
        .get_folder_by_name("Test", "INBOX")
        .expect("query")
        .expect("folder");
    let flags = db.folder_flags(folder.id.expect("id")).expect("flags");
    assert!(
        flags[&1].iter().any(|f| f == "\\Seen") && flags[&2].iter().any(|f| f == "\\Seen"),
        "the local copy must show the change at once: {flags:?}"
    );
    assert!(!flags[&3].iter().any(|f| f == "\\Seen"));
    assert_eq!(
        store.outbox_len().expect("outbox"),
        1,
        "the write waits in the outbox"
    );

    // The network comes back; the next sync drains the outbox first.
    setup.proxy.accept_new().await;
    let (second, _) = bounded_sync(&store, "INBOX").await;
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert_eq!(
        store.outbox_len().expect("outbox"),
        0,
        "the outbox is drained by the sync"
    );

    // The harness reports flags in async-imap's own spelling, without the
    // backslash.
    let on_server = setup
        .container
        .fetch_flags("INBOX", "1:3")
        .await
        .expect("flags from the server");
    println!("server flags after the sync: {on_server:?}");
    assert!(on_server[0].iter().any(|f| f == "Seen"), "{on_server:?}");
    assert!(on_server[1].iter().any(|f| f == "Seen"), "{on_server:?}");
    assert!(!on_server[2].iter().any(|f| f == "Seen"), "{on_server:?}");
    println!("{}", trace::report());
}

/// A write the user makes while the body backfill waits on a server that has
/// stopped answering is made at once: the backfill holds the write lock for
/// what it stores, never for what it fetches.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_write_is_made_at_once_while_the_body_backfill_waits_on_the_server() {
    let setup = setup(200).await;
    let store = store(&setup.config);
    let (first, _) = bounded_sync(&store, "INBOX").await;
    assert_eq!(first.messages_added, 200, "{:?}", first.errors);
    let folders = vec!["INBOX".to_string()];
    while store
        .fetch_pending_shapes("Test", &folders, 500)
        .await
        .expect("shapes")
        > 0
    {}
    let (pending, _) = store.body_backfill_progress().expect("progress");
    assert!(pending > 0, "the backfill needs bodies left to fetch");

    setup.proxy.freeze_all().await;
    // On this task rather than a spawned one: the pass is not `Send`.
    let backfill = store.fetch_pending_bodies("Test", &folders, 500, None);
    tokio::pin!(backfill);
    tokio::select! {
        ended = &mut backfill => panic!("the backfill should be waiting on the frozen connection: {ended:?}"),
        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
    }

    let started = Instant::now();
    let write = timeout(
        Duration::from_secs(5),
        store.queue_read_status("Test", "INBOX", &[1], true),
    );
    let changed = tokio::select! {
        ended = &mut backfill => panic!("the backfill should be waiting on the frozen connection: {ended:?}"),
        written = write => written
            .expect("marking read must not wait for the backfill's fetch")
            .expect("marking read"),
    };
    println!(
        "mark-read beside a stalled backfill: {:?}",
        started.elapsed()
    );
    assert_eq!(changed, vec![1]);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn a_no_from_select_ends_at_once_with_a_visible_error() {
    let setup = setup(0).await;
    let store = store(&setup.config);
    let before = trace::connections_opened();

    let (report, elapsed) = bounded_sync(&store, "DoesNotExist").await;
    let opened = trace::connections_opened() - before;

    println!(
        "NO from SELECT: ended in {elapsed:?}, errors={:?}, connections opened={opened}",
        report.errors
    );
    println!("{}", trace::report());
    assert!(
        report.errors.iter().any(|e| e.contains("DoesNotExist")),
        "the failed folder must be named in the report: {:?}",
        report.errors
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "a NO is final; it took {elapsed:?} to give up"
    );
    assert_eq!(opened, 1, "a NO is final; nothing to reconnect for");
}

/// What a push notification does: sync the folder again, seconds after the
/// last sync of it, on the connection that still has it selected.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_sync_right_after_a_sync_sees_the_mail_that_arrived_between() {
    let setup = setup(10).await;
    let store = store(&setup.config);

    let (first, _) = bounded_sync(&store, "INBOX").await;
    assert_eq!(first.messages_added, 10, "{:?}", first.errors);

    setup
        .container
        .prepopulate_mailbox(
            "INBOX",
            &[create_rfc822_message(
                "sender@example.com",
                "testuser@example.com",
                "Arrived between two syncs",
                "The push notification for this is what triggers the second sync.",
            )],
        )
        .await
        .expect("deliver one");

    let (second, elapsed) = bounded_sync(&store, "INBOX").await;
    println!(
        "sync after sync: ended in {elapsed:?}, added={} errors={:?}",
        second.messages_added, second.errors
    );
    println!("{}", trace::report());
    assert_eq!(
        second.messages_added, 1,
        "the message delivered between the two syncs must be fetched by the second"
    );
}

// ---------------------------------------------------------------------------
// IDLE
// ---------------------------------------------------------------------------

struct Watching {
    service: orb_core::services::IdleService,
    updates: tokio::sync::mpsc::UnboundedReceiver<StateUpdate>,
    shutdown: tokio_util::sync::CancellationToken,
}

async fn watch_inbox(config: &Config) -> Watching {
    let (tx, updates) = tokio::sync::mpsc::unbounded_channel();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut service =
        orb_core::services::IdleService::new(tx, shutdown.clone()).with_cycle(IDLE_CYCLE);
    let account = Arc::new(config.accounts[0].clone());
    service
        .ensure_watching(account, "INBOX")
        .await
        .expect("start watching INBOX");
    // The worker connects, selects and issues IDLE on its own time; a delivery
    // before the IDLE is in place is seen by the SELECT, not announced.
    // Against the local container that takes tens of milliseconds.
    tokio::time::sleep(Duration::from_secs(2)).await;
    Watching {
        service,
        updates,
        shutdown,
    }
}

/// Deliver a message and wait for the watcher to report it.
async fn deliver_and_expect_change(
    setup: &Setup,
    watching: &mut Watching,
    subject: &str,
    within: Duration,
) -> Option<Duration> {
    setup
        .container
        .prepopulate_mailbox(
            "INBOX",
            &[create_rfc822_message(
                "sender@example.com",
                "testuser@example.com",
                subject,
                "Delivered while idling.",
            )],
        )
        .await
        .expect("deliver a message");
    let started = Instant::now();
    loop {
        let remaining = within.checked_sub(started.elapsed())?;
        match timeout(remaining, watching.updates.recv()).await {
            Ok(Some(StateUpdate::ExternalChange { folder, .. })) if folder == "INBOX" => {
                return Some(started.elapsed());
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn an_idle_connection_closed_by_the_server_is_reestablished() {
    let setup = setup(0).await;
    let mut watching = watch_inbox(&setup.config).await;

    let first = deliver_and_expect_change(&setup, &mut watching, "first", Duration::from_secs(30))
        .await
        .expect("IDLE should report the first delivery");
    println!("closed IDLE: first delivery noticed after {first:?}");

    setup.proxy.drop_all().await;
    let second =
        deliver_and_expect_change(&setup, &mut watching, "second", Duration::from_secs(60)).await;
    println!(
        "closed IDLE: second delivery noticed after {second:?}; connections={}",
        setup.proxy.connections_accepted()
    );
    watching.shutdown.cancel();
    watching.service.shutdown().await;
    assert!(
        second.is_some(),
        "the worker should have reconnected and noticed the second delivery"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn a_frozen_idle_connection_is_replaced_and_the_next_change_still_arrives() {
    let setup = setup(0).await;
    let mut watching = watch_inbox(&setup.config).await;

    let first = deliver_and_expect_change(&setup, &mut watching, "first", Duration::from_secs(30))
        .await
        .expect("IDLE should report the first delivery");
    println!("frozen IDLE: first delivery noticed after {first:?}");

    // The server is still there; the packets are not. This is also what the
    // worker sees after the machine slept past the IDLE interval.
    setup.proxy.freeze_all().await;
    let second = deliver_and_expect_change(&setup, &mut watching, "second", BOUND).await;
    println!(
        "frozen IDLE: second delivery noticed after {second:?}; connections={}",
        setup.proxy.connections_accepted()
    );
    watching.shutdown.cancel();
    watching.service.shutdown().await;
    assert!(
        second.is_some(),
        "the worker should have given up on the frozen connection within {BOUND:?} \
         and noticed the second delivery on a new one"
    );
}
