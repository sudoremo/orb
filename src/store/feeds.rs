//! Polling the reading list's feeds: each new item appended to the reading
//! folder once, however many Orbs poll the same feed.

use chrono::Utc;

use super::Store;
use crate::Result;
use crate::db::FeedState;
use crate::feeds::{self, FeedItem, Fetched};

/// What a poll did.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct FeedPollReport {
    /// Feeds asked for news.
    pub polled: usize,
    /// Items appended to the reading folder by this Orb. Two Orbs polling in
    /// the same instant may both count an item they keep one copy of.
    pub appended: usize,
    /// What went wrong, a line per feed.
    pub problems: Vec<String>,
}

/// A feed's state on this machine, for the status panel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FeedStatus {
    pub url: String,
    pub name: Option<String>,
    /// Unix seconds.
    pub polled_at: Option<i64>,
    pub last_error: Option<String>,
}

/// One feed's new items, waiting for the server.
struct Pending {
    url: String,
    name: String,
    items: Vec<FeedItem>,
    /// Recorded once the items are on the server, so that a failed APPEND
    /// is retried rather than answered "not modified" next time.
    state: FeedState,
}

impl Store {
    /// Ask every feed that is due — or every feed, with `force` — for new
    /// items, and append them to the reading folder.
    ///
    /// Each item is looked for on the server before it is appended, in the
    /// reading folder and its archive, and looked for again afterwards: when
    /// two Orbs appended it at once, the copy with the higher UID goes, on
    /// both, so they converge on one. An item seen here once is never brought
    /// in again here, so one archived and deleted stays gone.
    pub async fn poll_feeds(&self, force: bool) -> Result<FeedPollReport> {
        let mut report = FeedPollReport::default();
        let config = self.config();
        let Some(reading) = config.reading.clone() else {
            return Ok(report);
        };
        if reading.feeds.is_empty() {
            return Ok(report);
        }
        let now = Utc::now().timestamp();
        let interval = i64::from(reading.poll_minutes) * 60;
        let client = match feeds::http_client() {
            Ok(client) => client,
            Err(error) => {
                report.problems.push(error);
                return Ok(report);
            }
        };

        // Ask each feed that is due
        let mut pending = Vec::new();
        for feed in &reading.feeds {
            let url = feed.url.trim().to_string();
            let previous = self.reader()?.feed_state(&url)?;
            let due = force
                || previous
                    .as_ref()
                    .and_then(|state| state.polled_at)
                    .is_none_or(|at| now - at >= interval);
            if !due {
                continue;
            }
            report.polled += 1;
            let mut state = previous.clone().unwrap_or_default();
            state.polled_at = Some(now);
            let fetched = feeds::fetch(
                &client,
                &url,
                state.etag.as_deref(),
                state.last_modified.as_deref(),
            )
            .await
            .and_then(|fetched| match fetched {
                Fetched::NotModified => Ok(None),
                Fetched::Body {
                    bytes,
                    etag,
                    last_modified,
                } => feeds::parse(&url, &bytes).map(|parsed| Some((parsed, etag, last_modified))),
            });
            let (parsed, etag, last_modified) = match fetched {
                Ok(Some(found)) => found,
                Ok(None) => {
                    state.last_error = None;
                    self.writer()?.set_feed_state(&url, &state)?;
                    continue;
                }
                Err(error) => {
                    crate::logger::warn(&format!("feed {url}: {error}"));
                    report.problems.push(format!("{url}: {error}"));
                    state.last_error = Some(error);
                    self.writer()?.set_feed_state(&url, &state)?;
                    continue;
                }
            };
            state.title = parsed.title.clone();
            state.etag = etag;
            state.last_modified = last_modified;
            state.last_error = None;

            let db = self.reader()?;
            let first = !db.feed_has_items(&url)?;
            let mut unseen = Vec::new();
            for item in parsed.items {
                if !db.feed_item_seen(&item.id)? {
                    unseen.push(item);
                }
            }
            drop(db);
            // A feed polled here for the first time brings in its newest items
            // only; the rest are remembered as seen.
            let items: Vec<FeedItem> = if first {
                let keep: Vec<String> = feeds::newest(&unseen, feeds::FIRST_POLL_ITEMS)
                    .iter()
                    .map(|item| item.id.clone())
                    .collect();
                let (kept, passed): (Vec<FeedItem>, Vec<FeedItem>) =
                    unseen.into_iter().partition(|item| keep.contains(&item.id));
                let ids: Vec<&str> = passed.iter().map(|item| item.id.as_str()).collect();
                self.writer()?.mark_feed_items_seen(&url, &ids, now)?;
                kept
            } else {
                unseen
            };
            let name = feed
                .name
                .clone()
                .or(parsed.title)
                .unwrap_or_else(|| url.clone());
            pending.push(Pending {
                url,
                name,
                items,
                state,
            });
        }

        // Append what is new, once
        let new_items: usize = pending.iter().map(|p| p.items.len()).sum();
        if new_items > 0 {
            match self.reading_folder(false) {
                Some((account, folder)) => {
                    let archive = self.reading_folder(true).map(|(_, archive)| archive);
                    match self
                        .append_feed_items(&account, &folder, archive.as_deref(), &pending)
                        .await
                    {
                        Ok(appended) => {
                            report.appended = appended;
                            if appended > 0
                                && let Err(error) = self
                                    .sync(Some(account.clone()), Some(folder.clone()), None, false)
                                    .await
                            {
                                crate::logger::warn(&format!(
                                    "feeds: appended, but the reading folder did not sync: {error}"
                                ));
                            }
                        }
                        Err(error) => {
                            crate::logger::warn(&format!("feeds: {error}"));
                            report.problems.push(format!("reading folder: {error}"));
                            return Ok(report);
                        }
                    }
                }
                None => {
                    report.problems.push(
                        "the reading folder does not exist yet; the next sync makes it".to_string(),
                    );
                    return Ok(report);
                }
            }
        }

        // Remember what was seen, and how each feed answered
        let mut db = self.writer()?;
        for feed in &pending {
            let ids: Vec<&str> = feed.items.iter().map(|item| item.id.as_str()).collect();
            db.mark_feed_items_seen(&feed.url, &ids, now)?;
            db.set_feed_state(&feed.url, &feed.state)?;
        }
        Ok(report)
    }

    /// Append the items the server has in neither reading folder, and keep one
    /// copy of each where two Orbs raced. Returns how many were appended here.
    async fn append_feed_items(
        &self,
        account: &str,
        folder: &str,
        archive: Option<&str>,
        pending: &[Pending],
    ) -> Result<usize> {
        let account_config = self.account_config(account)?;
        let now = Utc::now();
        self.with_connection(&account_config, "feeds", async |client| {
            let mut archived = std::collections::HashSet::new();
            if let Some(archive) = archive {
                client.select_mailbox(archive).await?;
                for item in pending.iter().flat_map(|p| &p.items) {
                    if !client
                        .uid_search_header(feeds::ID_HEADER, &item.id)
                        .await?
                        .is_empty()
                    {
                        archived.insert(item.id.clone());
                    }
                }
            }
            client.select_mailbox(folder).await?;
            let mut appended = 0;
            for feed in pending {
                for item in feed
                    .items
                    .iter()
                    .filter(|item| !archived.contains(&item.id))
                {
                    if client
                        .uid_search_header(feeds::ID_HEADER, &item.id)
                        .await?
                        .is_empty()
                    {
                        let message = feeds::build_message(&feed.name, item, now);
                        client.append_with_flags(folder, &message, None).await?;
                        appended += 1;
                    }
                    // Another Orb may have appended it at the same moment.
                    let copies = client.uid_search_header(feeds::ID_HEADER, &item.id).await?;
                    if copies.len() > 1 {
                        client.delete_messages(&copies[1..]).await?;
                    }
                }
            }
            Ok(appended)
        })
        .await
    }

    /// Every configured feed, as this machine last heard from it.
    pub fn feed_statuses(&self) -> Result<Vec<FeedStatus>> {
        let config = self.config();
        let Some(reading) = config.reading.as_ref() else {
            return Ok(Vec::new());
        };
        let db = self.reader()?;
        reading
            .feeds
            .iter()
            .map(|feed| {
                let state = db.feed_state(feed.url.trim())?.unwrap_or_default();
                Ok(FeedStatus {
                    url: feed.url.trim().to_string(),
                    name: feed.name.clone().or(state.title),
                    polled_at: state.polled_at,
                    last_error: state.last_error,
                })
            })
            .collect()
    }
}
