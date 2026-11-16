//! The reading list's feeds against a Dovecot container and a feed served
//! from this process: each item appended once, however often and from however
//! many Orbs the feed is polled.

mod common;

use common::imap_container::DovecotContainer;
use common::orb_config::config_with;
use common::test_context::TestContext;
use orb_core::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn rss(items: usize) -> String {
    let entries: String = (1..=items)
        .map(|n| {
            format!(
                "<item><title>Issue {n}</title><link>https://example.com/{n}</link>\
                 <guid>issue-{n}</guid><pubDate>Tue, {n:02} Oct 2026 08:00:00 GMT</pubDate>\
                 <description>Number {n}</description></item>"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>Weekly</title>{entries}</channel></rss>"
    )
}

/// Serve the feed, whatever is asked, until the test ends; the body can be
/// changed between polls.
async fn serve(feed: std::sync::Arc<std::sync::Mutex<String>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = feed.lock().unwrap().clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/feed.xml")
}

fn store(ctx: &TestContext, container: &DovecotContainer, url: &str) -> Store {
    let extra = format!("reading:\n  account: Test\n  feeds:\n    - url: {url}\n");
    Store::new(config_with(
        ctx,
        &container.host,
        container.imaps_port,
        &extra,
    ))
    .expect("open the store")
}

/// A first poll brings in the newest ten; polling again, or from a second
/// Orb with a database of its own, brings in only what is new since.
#[tokio::test]
#[ignore = "requires Docker"]
async fn each_feed_item_is_appended_once_from_two_machines() {
    let container = DovecotContainer::start().await.expect("start Dovecot");
    let feed = std::sync::Arc::new(std::sync::Mutex::new(rss(12)));
    let url = serve(feed.clone()).await;

    let first_ctx = TestContext::new();
    let first = store(&first_ctx, &container, &url);
    first
        .sync(Some("Test".into()), None, None, false)
        .await
        .expect("a sync makes the reading folders");
    let (_, reading) = first.reading_folder(false).expect("the reading folder");

    let report = first.poll_feeds(false).await.expect("poll");
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.appended, 10, "the newest ten of twelve");
    assert_eq!(container.get_message_count(&reading).await.unwrap(), 10);

    // Not due again yet, and forced it finds nothing new.
    assert_eq!(first.poll_feeds(false).await.unwrap().polled, 0);
    assert_eq!(first.poll_feeds(true).await.unwrap().appended, 0);

    // A second Orb: its first poll finds the ten on the server.
    let second_ctx = TestContext::new();
    let second = store(&second_ctx, &container, &url);
    second
        .sync(Some("Test".into()), None, None, false)
        .await
        .expect("sync the second");
    assert_eq!(second.poll_feeds(true).await.unwrap().appended, 0);
    assert_eq!(container.get_message_count(&reading).await.unwrap(), 10);

    // A new issue: appended by whichever Orb polls first, once.
    *feed.lock().unwrap() = rss(13);
    let (one, two) = tokio::join!(first.poll_feeds(true), second.poll_feeds(true));
    // Both may append it in the same instant; the copy with the higher UID
    // then goes, on both.
    let appended = one.unwrap().appended + two.unwrap().appended;
    assert!((1..=2).contains(&appended), "{appended}");
    assert_eq!(container.get_message_count(&reading).await.unwrap(), 11);

    // The item is unread mail in the reading folder, with its article.
    first
        .sync(Some("Test".into()), Some(reading.clone()), None, false)
        .await
        .expect("sync the reading folder");
    let flags = container.fetch_flags(&reading, "1:*").await.expect("flags");
    assert!(
        flags.iter().all(|f| !f.iter().any(|flag| flag == "Seen")),
        "{flags:?}"
    );
    let db = orb_core::Database::new(first_ctx.db_path.to_str().unwrap()).unwrap();
    let id: i64 = db
        .connection()
        .query_row(
            "SELECT id FROM messages WHERE subject = 'Issue 13'",
            [],
            |row| row.get(0),
        )
        .expect("the new issue is stored");
    assert_eq!(
        db.extra_header(id, orb_core::feeds::LINK_HEADER)
            .unwrap()
            .as_deref(),
        Some("https://example.com/13")
    );
}
