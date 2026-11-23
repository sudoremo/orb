//! Filling in message bodies after the headers are already synced.
//!
//! Header sync is what makes the app usable, so it stays fast and stores no
//! body content. This fills the gap afterwards, in the background: for every
//! message whose `BODYSTRUCTURE` said it has a readable text part but whose
//! body is not stored yet, fetch that part alone.
//!
//! Only the text parts are fetched, never the whole message — full source
//! averages a couple of hundred kilobytes once attachments are counted, and
//! text is a couple of kilobytes.
//!
//! The work is ordered newest-first and processed in bounded batches, so it is
//! resumable by construction: progress is whatever is already in the database,
//! and an interrupted run simply leaves the rest pending.

use std::collections::HashMap;

use crate::Result;
use crate::db::Database;
use crate::imap::ImapClient;

/// One message needing its body fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBody {
    pub message_id: i64,
    pub uid: u32,
    /// MIME path of the text part, from the recorded shape.
    pub part: String,
}

/// How a batch went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BodyFetchReport {
    /// Bodies fetched and stored.
    pub fetched: usize,
    /// Messages the server had no usable part for.
    pub missing: usize,
}

impl BodyFetchReport {
    pub fn merge(&mut self, other: BodyFetchReport) {
        self.fetched += other.fetched;
        self.missing += other.missing;
    }
}

/// Group messages by the MIME path their text lives at.
///
/// A FETCH applies one part spec to every message in its range, so a batch can
/// only cover messages that share a path. In practice a handful of shapes —
/// `1` for plain messages, `1.1` for the usual `alternative` — cover nearly
/// everything, so this still yields few, large batches rather than many small
/// ones.
pub fn group_by_part(pending: &[PendingBody]) -> HashMap<&str, Vec<u32>> {
    let mut groups: HashMap<&str, Vec<u32>> = HashMap::new();
    for message in pending {
        groups
            .entry(message.part.as_str())
            .or_default()
            .push(message.uid);
    }
    groups
}

/// Split a UID list into chunks, so one FETCH cannot become unboundedly large.
pub fn batches(uids: &[u32], size: usize) -> Vec<Vec<u32>> {
    if size == 0 {
        return vec![uids.to_vec()];
    }
    uids.chunks(size).map(|chunk| chunk.to_vec()).collect()
}

/// Fetch and store the bodies for `pending`, which must all be in `folder`.
///
/// Returns what was fetched. Messages the server declines to produce a part for
/// are counted as missing rather than retried forever; they are left pending and
/// a later pass will try again, which is the right behaviour for a transient
/// server error and harmless for a genuinely bodyless message.
///
/// `write_lock` is held for storing each batch and never while the server is
/// asked: a write the user makes must not wait for the network.
pub async fn fetch_bodies_for_folder(
    client: &mut ImapClient,
    db: &Database,
    write_lock: &tokio::sync::Mutex<()>,
    folder: &str,
    pending: &[PendingBody],
    batch_size: usize,
) -> Result<BodyFetchReport> {
    if pending.is_empty() {
        return Ok(BodyFetchReport::default());
    }

    client.select_mailbox(folder).await?;

    let by_uid: HashMap<u32, i64> = pending.iter().map(|p| (p.uid, p.message_id)).collect();
    let mut report = BodyFetchReport::default();

    for (part, uids) in group_by_part(pending) {
        for chunk in batches(&uids, batch_size) {
            let fetched = client.fetch_parts(&chunk, part).await?;

            let _guard = write_lock.lock().await;
            for uid in &chunk {
                let Some(&message_id) = by_uid.get(uid) else {
                    continue;
                };
                match fetched.get(uid) {
                    Some(text) => {
                        db.set_message_body_text(message_id, text)?;
                        report.fetched += 1;
                    }
                    None => report.missing += 1,
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(uid: u32, part: &str) -> PendingBody {
        PendingBody {
            message_id: uid as i64,
            uid,
            part: part.to_string(),
        }
    }

    #[test]
    fn groups_messages_that_share_a_part_path() {
        let work = vec![
            pending(1, "1"),
            pending(2, "1.1"),
            pending(3, "1"),
            pending(4, "1.1"),
            pending(5, "2"),
        ];
        let groups = group_by_part(&work);

        assert_eq!(groups.len(), 3);
        assert_eq!(groups["1"], vec![1, 3]);
        assert_eq!(groups["1.1"], vec![2, 4]);
        assert_eq!(groups["2"], vec![5]);
    }

    #[test]
    fn grouping_preserves_order_within_a_group() {
        // Work is handed over newest-first; a batch must not reshuffle it, or
        // the newest messages stop arriving first.
        let work = vec![pending(9, "1"), pending(7, "1"), pending(3, "1")];
        assert_eq!(group_by_part(&work)["1"], vec![9, 7, 3]);
    }

    #[test]
    fn batches_split_on_the_requested_size() {
        assert_eq!(
            batches(&[1, 2, 3, 4, 5], 2),
            vec![vec![1, 2], vec![3, 4], vec![5]]
        );
        assert_eq!(batches(&[1, 2], 5), vec![vec![1, 2]]);
        assert!(batches(&[], 5).is_empty());
    }

    #[test]
    fn a_zero_batch_size_does_not_divide_by_zero() {
        assert_eq!(batches(&[1, 2, 3], 0), vec![vec![1, 2, 3]]);
    }

    #[test]
    fn report_merges() {
        let mut total = BodyFetchReport::default();
        total.merge(BodyFetchReport {
            fetched: 3,
            missing: 1,
        });
        total.merge(BodyFetchReport {
            fetched: 2,
            missing: 0,
        });
        assert_eq!(
            total,
            BodyFetchReport {
                fetched: 5,
                missing: 1
            }
        );
    }
}
