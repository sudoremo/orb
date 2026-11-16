//! Feeds for the reading list: an RSS, Atom or JSON feed read into items, and
//! each item made into a message for the reading folder.
//!
//! Every Orb polls the feeds, so the same item can be appended on two machines.
//! An item is therefore known by an id derived from the feed and the item
//! alone, carried in [`ID_HEADER`] and in a `Message-ID` made from it, so that
//! a second Orb finds it on the server before appending it again; see
//! `docs/CONCURRENCY_DESIGN.md`.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::tasks::rfc822::{base64_body, header_line};

/// The header that names a feed item, whichever machine appended it.
pub const ID_HEADER: &str = "X-Orb-Feed-Item-Id";
/// The header with the item's article, which `open-link` opens.
pub const LINK_HEADER: &str = "X-Orb-Feed-Link";
/// The items a feed brings in when it is first polled: its newest, not its
/// whole history.
pub const FIRST_POLL_ITEMS: usize = 10;

/// A feed's answer to a poll.
#[derive(Debug)]
pub enum Fetched {
    /// Nothing new since the `ETag` or the date we sent.
    NotModified,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// The largest feed read; anything bigger is not a feed anybody reads.
const MAX_FEED_BYTES: usize = 10 * 1024 * 1024;

/// The client every poll uses: a timeout, and a name the publisher can see.
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("Orb/", env!("CARGO_PKG_VERSION"), " (feed reader)"))
        .build()
        .map_err(|e| e.to_string())
}

/// Ask a feed for what it has, conditionally when it said how to.
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Fetched, String> {
    let mut request = client.get(url);
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if let Some(date) = last_modified {
        request = request.header(reqwest::header::IF_MODIFIED_SINCE, date);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Fetched::NotModified);
    }
    if !status.is_success() {
        return Err(format!("the server answered {status}"));
    }
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_FEED_BYTES)
    {
        return Err("the feed is larger than 10 MB".to_string());
    }
    let header = |name: reqwest::header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let etag = header(reqwest::header::ETAG);
    let last_modified = header(reqwest::header::LAST_MODIFIED);
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FEED_BYTES {
        return Err("the feed is larger than 10 MB".to_string());
    }
    Ok(Fetched::Body {
        bytes: bytes.to_vec(),
        etag,
        last_modified,
    })
}

/// A feed as read.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFeed {
    pub title: Option<String>,
    pub items: Vec<FeedItem>,
}

/// One item of a feed.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedItem {
    /// Derived from the feed and the item's own id; see [`item_id`].
    pub id: String,
    pub title: String,
    pub link: Option<String>,
    pub published: Option<DateTime<Utc>>,
    /// The content, or else the summary, as HTML.
    pub html: String,
}

/// Read a feed. An item with no id, no link and no title cannot be told from
/// the next one and is left out.
pub fn parse(feed_url: &str, bytes: &[u8]) -> Result<ParsedFeed, String> {
    // The crate falls back to a random id for an item with neither link nor
    // title, which would be a new item on every poll; an empty one is left out.
    let parser = feed_rs::parser::Builder::new()
        .id_generator(|links, title, _| {
            links
                .first()
                .map(|link| link.href.clone())
                .or_else(|| title.as_ref().map(|t| t.content.clone()))
                .unwrap_or_default()
        })
        .build();
    let feed = parser.parse(bytes).map_err(|e| e.to_string())?;
    let items = feed
        .entries
        .into_iter()
        .filter(|entry| !entry.id.trim().is_empty())
        .map(|entry| {
            let link = entry
                .links
                .iter()
                .find(|link| link.rel.as_deref().is_none_or(|rel| rel == "alternate"))
                .or(entry.links.first())
                .map(|link| link.href.clone());
            let html = entry
                .content
                .and_then(|content| {
                    let html = content.content_type.essence().to_string().contains("html");
                    content
                        .body
                        .map(|body| if html { body } else { plain_to_html(&body) })
                })
                .or_else(|| entry.summary.map(|text| text_to_html(&text)))
                .unwrap_or_default();
            FeedItem {
                id: item_id(feed_url, &entry.id),
                title: entry
                    .title
                    .map(|t| decode_entities(t.content.trim()))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "(untitled)".to_string()),
                link,
                published: entry.published.or(entry.updated),
                html,
            }
        })
        .collect();
    Ok(ParsedFeed {
        title: feed
            .title
            .map(|t| decode_entities(t.content.trim()))
            .filter(|t| !t.is_empty()),
        items,
    })
}

/// An item's id: the feed's address and the item's own id, hashed, so that
/// two feeds that number their items alike do not collide.
pub fn item_id(feed_url: &str, guid: &str) -> String {
    let digest = Sha256::digest(format!("{}\n{}", feed_url.trim(), guid.trim()).as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// The newest items first, as many as a first poll brings in.
pub fn newest(items: &[FeedItem], count: usize) -> Vec<&FeedItem> {
    let mut sorted: Vec<&FeedItem> = items.iter().collect();
    // Undated items keep the feed's own order, which is usually newest first.
    if items.iter().any(|item| item.published.is_some()) {
        sorted.sort_by_key(|item| std::cmp::Reverse(item.published));
    }
    sorted.into_iter().take(count).collect()
}

/// The item as a message: the title as the subject, the feed as the sender,
/// the article's link first in the body and in [`LINK_HEADER`].
pub fn build_message(feed_name: &str, item: &FeedItem, now: DateTime<Utc>) -> Vec<u8> {
    let date = item.published.unwrap_or(now);
    let link = item
        .link
        .as_deref()
        .map(|href| {
            format!(
                "<p><a href=\"{}\">Open the article</a></p>\n",
                escape_html(href)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"></head><body>\n{link}{}\n</body></html>\n",
        item.html
    );
    let sender = feed_name.replace(['"', '\\', '\r', '\n'], " ");
    let mut message = String::new();
    message.push_str(&header_line(
        "From",
        &format!("\"{sender}\" <feed@orb.invalid>"),
    ));
    message.push_str(&header_line(
        "Subject",
        &item.title.replace(['\r', '\n'], " "),
    ));
    message.push_str(&header_line("Date", &date.to_rfc2822()));
    message.push_str(&header_line(
        "Message-ID",
        &format!("<{}@feed.orb.invalid>", item.id),
    ));
    message.push_str(&header_line(ID_HEADER, &item.id));
    if let Some(href) = item.link.as_deref() {
        message.push_str(&header_line(LINK_HEADER, href.trim()));
    }
    message.push_str(&header_line("MIME-Version", "1.0"));
    message.push_str(&header_line("Content-Type", "text/html; charset=utf-8"));
    message.push_str(&header_line("Content-Transfer-Encoding", "base64"));
    message.push_str("\r\n");
    message.push_str(&base64_body(body.as_bytes()));
    message.into_bytes()
}

fn text_to_html(text: &feed_rs::model::Text) -> String {
    if text.content_type.essence().to_string().contains("html") {
        text.content.clone()
    } else {
        plain_to_html(&text.content)
    }
}

fn plain_to_html(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| format!("<p>{}</p>", escape_html(paragraph.trim())))
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Titles often arrive with the entities their HTML had.
fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#8217;", "\u{2019}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Rust Weekly</title><link>https://example.com</link>
<item><title>Issue 2 &amp; more</title><link>https://example.com/2</link><guid>issue-2</guid>
  <pubDate>Tue, 20 Oct 2026 08:00:00 GMT</pubDate><description>&lt;b&gt;Second&lt;/b&gt;</description></item>
<item><title>Issue 1</title><link>https://example.com/1</link><guid>issue-1</guid>
  <pubDate>Tue, 13 Oct 2026 08:00:00 GMT</pubDate><description>First</description></item>
<item><description>No title, no link, no guid</description></item>
</channel></rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"><title>A blog</title><id>urn:blog</id>
<entry><title>Hello</title><id>urn:post:1</id><link href="https://blog.example/hello"/>
  <updated>2026-10-01T10:00:00Z</updated><content type="html">&lt;p&gt;Hi&lt;/p&gt;</content></entry>
</feed>"#;

    #[test]
    fn an_rss_feed_is_read_into_items_that_can_be_told_apart() {
        let feed = parse("https://example.com/rss", RSS.as_bytes()).unwrap();
        assert_eq!(feed.title.as_deref(), Some("Rust Weekly"));
        assert_eq!(
            feed.items.len(),
            2,
            "the item with nothing to know it by is left out"
        );
        let first = &feed.items[0];
        assert_eq!(first.title, "Issue 2 & more");
        assert_eq!(first.link.as_deref(), Some("https://example.com/2"));
        assert!(first.html.contains("Second"), "{}", first.html);
        assert_eq!(first.id, item_id("https://example.com/rss", "issue-2"));
        // The same item read again is the same item.
        let again = parse("https://example.com/rss", RSS.as_bytes()).unwrap();
        assert_eq!(again.items[0].id, first.id);
        // The same guid in another feed is another item.
        assert_ne!(item_id("https://other.example/rss", "issue-2"), first.id);
    }

    #[test]
    fn an_atom_feed_is_read_too() {
        let feed = parse("https://blog.example/atom", ATOM.as_bytes()).unwrap();
        assert_eq!(feed.items.len(), 1);
        assert_eq!(
            feed.items[0].link.as_deref(),
            Some("https://blog.example/hello")
        );
        assert!(
            feed.items[0].html.contains("<p>Hi</p>"),
            "{}",
            feed.items[0].html
        );
        assert!(feed.items[0].published.is_some());
    }

    #[test]
    fn something_that_is_not_a_feed_is_an_error() {
        assert!(parse("https://example.com", b"<html><body>Hi</body></html>").is_err());
    }

    #[test]
    fn a_first_poll_takes_the_newest_items() {
        let feed = parse("https://example.com/rss", RSS.as_bytes()).unwrap();
        let newest = newest(&feed.items, 1);
        assert_eq!(newest[0].title, "Issue 2 & more");
    }

    #[test]
    fn an_item_becomes_a_message_mail_can_parse() {
        let feed = parse("https://example.com/rss", RSS.as_bytes()).unwrap();
        let bytes = build_message("Rust Weekly – news", &feed.items[0], Utc::now());
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        let header = |name: &str| {
            parsed
                .headers
                .iter()
                .find(|h| h.get_key_ref().eq_ignore_ascii_case(name))
                .map(|h| h.get_value())
        };
        assert_eq!(header("Subject").as_deref(), Some("Issue 2 & more"));
        assert_eq!(
            header("From").as_deref(),
            Some("\"Rust Weekly – news\" <feed@orb.invalid>")
        );
        assert_eq!(header(ID_HEADER), Some(feed.items[0].id.clone()));
        assert_eq!(
            header(LINK_HEADER).as_deref(),
            Some("https://example.com/2")
        );
        assert_eq!(
            header("Message-ID"),
            Some(format!("<{}@feed.orb.invalid>", feed.items[0].id))
        );
        assert_eq!(parsed.ctype.mimetype, "text/html");
        let body = parsed.get_body().unwrap();
        assert!(
            body.contains("<a href=\"https://example.com/2\">Open the article</a>"),
            "{body}"
        );
        assert!(body.contains("Second"), "{body}");
    }
}
