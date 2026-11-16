//! The sync against a folder far larger than any test mailbox.
//!
//! Prints what it measures; the numbers belong in the plan as the baseline.
//! `ORB_SCALE_MESSAGES` overrides the folder size (default 100 000).
//!
//! Run with: `cargo test --test sync_scale -- --ignored --nocapture`

mod common;

use std::time::Instant;

use common::imap_container::{DovecotContainer, create_rfc822_message};
use common::orb_config::config_for;
use common::test_context::TestContext;
use orb_core::Store;
use orb_core::imap::{ImapClient, SyncTrace, trace};
use secrecy::SecretString;

const DEFAULT_MESSAGES: usize = 100_000;

fn folder_size() -> usize {
    std::env::var("ORB_SCALE_MESSAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MESSAGES)
}

/// Resident set size of this process, as the kernel reports it.
fn rss_mib() -> f64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

#[tokio::test]
#[ignore = "requires Docker - stress test"]
async fn a_hundred_thousand_message_folder() {
    let count = folder_size();
    let container = DovecotContainer::start()
        .await
        .expect("start the Dovecot container");

    let started = Instant::now();
    container
        .populate_inbox_maildir(count)
        .await
        .expect("populate INBOX");
    println!("populated {count} messages in {:?}", started.elapsed());
    assert_eq!(
        container.get_message_count("INBOX").await.expect("count") as usize,
        count
    );

    let ctx = TestContext::new();
    let config = config_for(&ctx, &container.host, container.imaps_port);
    let store = Store::new(config.clone()).expect("open the store");
    let rss_before = rss_mib();

    let started = Instant::now();
    let cold = store
        .sync(Some("Test".into()), Some("INBOX".into()), None, false)
        .await
        .expect("cold sync");
    println!(
        "cold sync: {:?} added={} errors={:?} rss {:.0} -> {:.0} MiB",
        started.elapsed(),
        cold.messages_added,
        cold.errors,
        rss_before,
        rss_mib()
    );
    assert_eq!(cold.messages_added as usize, count);

    let started = Instant::now();
    let warm = store
        .sync(Some("Test".into()), Some("INBOX".into()), None, false)
        .await
        .expect("warm sync");
    println!(
        "warm sync, nothing changed: {:?} added={} updated={} deleted={} errors={:?}",
        started.elapsed(),
        warm.messages_added,
        warm.messages_updated,
        warm.messages_deleted,
        warm.errors
    );

    // One arrival and one departure: the incremental path, with VANISHED.
    //
    // On a fresh connection, deliberately. The pooled one still has INBOX
    // selected, and Dovecot answers STATUS for the selected mailbox from the
    // session's own view, which nothing here has refreshed — so the sync
    // would call the folder unchanged and the measurement would be of
    // nothing. `tests/sync_faults.rs` reproduces that; this measures the
    // path the fix will take.
    store.connection_pool().close_all().await;
    container
        .prepopulate_mailbox(
            "INBOX",
            &[create_rfc822_message(
                "sender@example.com",
                "testuser@example.com",
                "One more",
                "Arrived after the cold sync.",
            )],
        )
        .await
        .expect("deliver one");
    container
        .delete_and_expunge("INBOX", "1")
        .await
        .expect("expunge one");
    let started = Instant::now();
    let incremental = store
        .sync(Some("Test".into()), Some("INBOX".into()), None, false)
        .await
        .expect("incremental sync");
    println!(
        "incremental sync, +1 -1: {:?} added={} updated={} deleted={} errors={:?}",
        started.elapsed(),
        incremental.messages_added,
        incremental.messages_updated,
        incremental.messages_deleted,
        incremental.errors
    );
    assert_eq!(incremental.messages_added, 1);
    assert_eq!(incremental.messages_deleted, 1);

    // The deletion-detection fallback for servers without QRESYNC: list every
    // UID. Measured on its own, because Dovecot has QRESYNC and the sync above
    // never takes this path.
    let (user, password) = DovecotContainer::credentials();
    let mut client = ImapClient::connect_insecure(
        &container.host,
        container.imaps_port,
        user,
        &SecretString::from(password.to_string()),
        "scale test, uid listing",
    )
    .await
    .expect("connect");
    client.select_mailbox("INBOX").await.expect("select");
    let mut listing_trace = SyncTrace::begin("Test", "INBOX (uid listing)", client.id());
    let phase = listing_trace.start("uid-listing", client.bytes_on_wire());
    let started = Instant::now();
    let uids = client.fetch_all_uids().await.expect("list every UID");
    let took = started.elapsed();
    listing_trace.end(phase, client.bytes_on_wire(), Some(uids.len()), "");
    listing_trace.finish(Ok(()));
    println!("full UID listing of {} messages: {took:?}", uids.len());

    println!("{}", trace::report());
}

/// Whether a second warm connection per account pays off: the body backfill
/// runs a fixed number of rounds while the folder is synced every quarter
/// second, as push and the timer do in the app, once with one idle
/// connection kept per account and once with two. Prints, for each, how long
/// the backfill took, how many syncs ran and how long they took, and how many
/// connections were opened.
///
/// `ORB_BACKFILL_MESSAGES` sets the folder size (default 20 000) and
/// `ORB_BACKFILL_ROUNDS` the rounds of 500 (default 10).
#[tokio::test]
#[ignore = "requires Docker - measurement"]
async fn a_second_connection_beside_the_backfill() {
    let count: usize = std::env::var("ORB_BACKFILL_MESSAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let rounds: usize = std::env::var("ORB_BACKFILL_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let container = DovecotContainer::start()
        .await
        .expect("start the Dovecot container");
    container
        .populate_inbox_maildir(count)
        .await
        .expect("populate INBOX");

    for kept in [1, 2] {
        let ctx = TestContext::new();
        let config = config_for(&ctx, &container.host, container.imaps_port);
        let store = std::sync::Arc::new(Store::new(config).expect("open the store"));
        store.connection_pool().set_idle_per_account(kept);
        store
            .sync(Some("Test".into()), Some("INBOX".into()), None, false)
            .await
            .expect("cold sync");

        let opened_before = trace::connections_opened();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let syncer = {
            let store = store.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut took = Vec::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let started = Instant::now();
                    let _ = store
                        .sync(Some("Test".into()), Some("INBOX".into()), None, false)
                        .await;
                    took.push(started.elapsed());
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                took
            })
        };

        let folders = vec!["INBOX".to_string()];
        let started = Instant::now();
        let mut bodies = 0;
        for _ in 0..rounds {
            while store
                .fetch_pending_shapes("Test", &folders, 500)
                .await
                .expect("shapes")
                > 0
            {}
            bodies += store
                .fetch_pending_bodies("Test", &folders, 500, None)
                .await
                .expect("bodies")
                .fetched;
        }
        let backfill = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let syncs = syncer.await.expect("syncer");
        let opened = trace::connections_opened() - opened_before;
        let mean = syncs.iter().sum::<std::time::Duration>() / syncs.len().max(1) as u32;
        let worst = syncs.iter().max().copied().unwrap_or_default();
        println!(
            "{kept} kept: backfill of {bodies} bodies {backfill:?}; {} syncs beside it, \
             mean {mean:?}, worst {worst:?}; {opened} connections opened",
            syncs.len()
        );
    }
}
