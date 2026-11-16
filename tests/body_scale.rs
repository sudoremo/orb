//! What it costs to read the small columns of a large folder whose messages
//! carry their bodies (PLAN 7.1).
//!
//! Prints what it measures. `ORB_BODY_SCALE_MESSAGES` sets the folder size
//! (default 50 000); every message carries about 9 KB of text, HTML and
//! source, more than a database page, as real mail does.
//!
//! Run with: `cargo test --release --test body_scale -- --ignored --nocapture`
//!
//! `ORB_BODY_SCALE_KEEP=<path>` builds the database there and keeps it, and
//! `migrating_a_kept_database` then times opening it with this build — which
//! is how long the first launch after a schema change takes on a mailbox of
//! that size.

mod common;

use std::time::{Duration, Instant};

use common::orb_config::config_for;
use common::test_context::TestContext;
use orb_core::Database;
use orb_core::Store;
use orb_core::models::Message;
use orb_core::store::MessageSort;

const DEFAULT_MESSAGES: usize = 50_000;

fn folder_size() -> usize {
    std::env::var("ORB_BODY_SCALE_MESSAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MESSAGES)
}

fn message(folder_id: i64, uid: u32) -> Message {
    let words = format!("message {uid} talks about the quarterly figures at length. ");
    Message {
        id: None,
        folder_id,
        uid,
        message_id: Some(format!("<scale-{uid}@test.local>")),
        subject: Some(format!("Message {uid}")),
        from: Some("sender@example.com".to_string()),
        to: Some("me@example.com".to_string()),
        cc: None,
        date: Some(format!(
            "2025-01-{:02}T10:{:02}:00+00:00",
            1 + uid % 28,
            uid % 60
        )),
        body_text: Some(words.repeat(25)),
        body_html: Some(format!("<p>{}</p>", words.repeat(50))),
        rfc822: Some(words.repeat(65).into_bytes()),
        size: Some(9_000),
        flags: if uid.is_multiple_of(7) {
            vec![]
        } else {
            vec!["\\Seen".to_string()]
        },
        modseq: Some(u64::from(uid)),
        shape: None,
        headers: None,
    }
}

/// The middle of three runs, in milliseconds.
fn time<T>(what: &str, mut run: impl FnMut() -> T) {
    let mut runs: Vec<Duration> = (0..3)
        .map(|_| {
            let started = Instant::now();
            std::hint::black_box(run());
            started.elapsed()
        })
        .collect();
    runs.sort();
    println!("{what:<40} {:>9.1} ms", runs[1].as_secs_f64() * 1000.0);
}

#[test]
#[ignore = "builds a database of several hundred megabytes"]
fn reading_a_large_folder_with_its_bodies() {
    let count = folder_size();
    let ctx = TestContext::new();
    let mut config = config_for(&ctx, "localhost", 1);
    if let Ok(keep) = std::env::var("ORB_BODY_SCALE_KEEP") {
        config.db_path = keep;
    }
    let db_path = config.db_path.clone();
    let store = Store::new(config).expect("store");

    let mut db = Database::new(&db_path).expect("database");
    let inbox = db.insert_folder("Test", "INBOX").expect("folder");
    db.insert_folder("Test", "Archive").expect("folder");
    let started = Instant::now();
    for chunk in (1..=count as u32).collect::<Vec<_>>().chunks(5_000) {
        let messages: Vec<Message> = chunk.iter().map(|uid| message(inbox, *uid)).collect();
        db.bulk_insert_messages(&messages).expect("insert");
    }
    drop(db);
    let bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    println!(
        "{count} messages, {:.0} MB, stored in {:.1} s",
        bytes as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );

    time("sidebar: folders with counts", || {
        store.get_folders("Test").expect("folders")
    });
    time("Dock badge: unread in every inbox", || {
        store.inbox_unread_count().expect("unread")
    });
    time("first page of 200, newest first", || {
        store
            .get_message_list("Test", "INBOX", 0, 200, MessageSort::DateNewest)
            .expect("page")
    });
    time("page of 200 in the middle", || {
        store
            .get_message_list("Test", "INBOX", count / 2, 200, MessageSort::DateNewest)
            .expect("page")
    });
    time("first page of 200, by sender", || {
        store
            .get_message_list("Test", "INBOX", 0, 200, MessageSort::SenderAsc)
            .expect("page")
    });

    let all = orb_core::store::SearchScope::All;
    for (what, query) in [
        ("search: a word of every message", "quarterly"),
        ("search: the start of a rare word", "4242"),
        ("search: part of a sender", "f:ender@exa"),
        ("search: two words", "figures 4242"),
    ] {
        time(&format!("{what}, by relevance"), || {
            store
                .search_message_results(query, &all, None)
                .expect("search")
        });
        time(&format!("{what}, newest first"), || {
            store
                .search_message_results(query, &all, Some(MessageSort::DateNewest))
                .expect("search")
        });
    }
}

#[test]
#[ignore = "opens the database ORB_BODY_SCALE_KEEP names"]
fn migrating_a_kept_database() {
    let path = std::env::var("ORB_BODY_SCALE_KEEP").expect("ORB_BODY_SCALE_KEEP");
    let started = Instant::now();
    Database::new(&path)
        .expect("open")
        .init_schema()
        .expect("migrate");
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "opened and migrated in {:.1} s; {:.0} MB",
        started.elapsed().as_secs_f64(),
        bytes as f64 / 1e6
    );
}
