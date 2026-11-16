//! Copying messages, and moving them between accounts.
//!
//! Within an account a copy is the server's `UID COPY`. Between accounts
//! there is no server-side path: the messages are fetched whole from the one
//! server, appended with their flags to the other, and — for a move — deleted
//! from where they came once every one of them has arrived.

use std::collections::HashMap;

use super::{Store, WriteOutcome};
use crate::Result;

/// A batch as fetched from the source: the flags by UID, and the bodies.
type Fetched = (HashMap<u32, Vec<String>>, Vec<(u32, Vec<u8>)>);

/// Messages fetched whole per round trip when crossing accounts, and the
/// bytes of them by their stored sizes: whole messages can be megabytes, and a
/// batch has to fit in memory twice over.
const TRANSFER_BATCH: usize = 20;
const TRANSFER_BATCH_BYTES: u64 = 32 * 1024 * 1024;

/// What a transfer between accounts did.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TransferOutcome {
    /// Messages that arrived in the other account.
    pub transferred: usize,
    /// Messages the source server no longer had, and so could not be sent.
    pub missing: usize,
    /// Why the list does not yet show what the servers have, when it does not.
    pub warning: Option<String>,
}

impl Store {
    /// Copy messages to another folder of the same account, on the server.
    /// Nothing changes here: the next sync of the target shows the copies.
    pub async fn copy_messages(
        &self,
        account: &str,
        folder: &str,
        target: &str,
        uids: &[u32],
    ) -> Result<WriteOutcome> {
        if uids.is_empty() {
            return Ok(WriteOutcome::default());
        }
        let account_config = self.account_config(account)?;
        self.with_connection(&account_config, "copy", async |client| {
            self.select_for_uids(client, account, folder).await?;
            client.copy_messages(uids, target).await
        })
        .await?;
        Ok(WriteOutcome::default())
    }

    /// Move or copy messages into a folder of another account.
    ///
    /// Each batch is fetched whole from the source and appended, flags and
    /// all, to the target; a move then deletes from the source exactly the
    /// messages the target accepted, so a failure halfway leaves copies in
    /// the target and nothing missing from the source. A message the source
    /// server no longer has is counted rather than failed.
    pub async fn transfer_messages(
        &self,
        from_account: &str,
        from_folder: &str,
        to_account: &str,
        to_folder: &str,
        uids: &[u32],
        keep_source: bool,
    ) -> Result<TransferOutcome> {
        let mut outcome = TransferOutcome::default();
        if uids.is_empty() {
            return Ok(outcome);
        }
        let source = self.account_config(from_account)?;
        let target = self.account_config(to_account)?;

        let mut appended: Vec<u32> = Vec::new();
        for batch in self.transfer_batches(from_account, from_folder, uids)? {
            let batch = batch.as_slice();
            // Flags and bodies from the source, in two commands on one
            // connection. A UID the server answers twice is one message.
            let (flags, mut bodies): Fetched = self
                .with_connection(&source, "transfer: fetch", async |client| {
                    self.select_for_uids(client, from_account, from_folder)
                        .await?;
                    let flags = client.fetch_flags(batch).await?;
                    let bodies = client.fetch_bodies(batch).await?;
                    Ok((flags, bodies))
                })
                .await?;
            let mut seen = std::collections::HashSet::new();
            bodies.retain(|(uid, _)| seen.insert(*uid));
            outcome.missing += batch.len().saturating_sub(bodies.len());

            // Into the target, with the flags the source had.
            let arrived: Vec<u32> = self
                .with_connection(&target, "transfer: append", async |client| {
                    let mut arrived = Vec::with_capacity(bodies.len());
                    for (uid, body) in &bodies {
                        let kept = system_flags(flags.get(uid).map(Vec::as_slice).unwrap_or(&[]));
                        client
                            .append_with_flags(to_folder, body, kept.as_deref())
                            .await?;
                        arrived.push(*uid);
                    }
                    Ok(arrived)
                })
                .await?;
            outcome.transferred += arrived.len();
            appended.extend(arrived);
        }

        if !keep_source && !appended.is_empty() {
            let deleted = self
                .delete_immediate(from_account, from_folder, &appended)
                .await?;
            outcome.warning = deleted.warning;
        }
        Ok(outcome)
    }
}

impl Store {
    /// The UIDs in batches a transfer fetches at once: at most
    /// [`TRANSFER_BATCH`] messages and about [`TRANSFER_BATCH_BYTES`] of them
    /// by their stored sizes, so a run of large attachments is not held in
    /// memory twice over by the score.
    fn transfer_batches(&self, account: &str, folder: &str, uids: &[u32]) -> Result<Vec<Vec<u32>>> {
        let sizes: HashMap<u32, u64> = {
            let db = self.reader()?;
            match db.get_folder_by_name(account, folder)?.and_then(|f| f.id) {
                Some(folder_id) => {
                    let mut sizes = HashMap::new();
                    for chunk in uids.chunks(500) {
                        let placeholders = vec!["?"; chunk.len()].join(",");
                        let mut stmt = db.conn.prepare(&format!(
                            "SELECT uid, size FROM messages
                             WHERE folder_id = ? AND uid IN ({placeholders})"
                        ))?;
                        let parameters = std::iter::once(folder_id)
                            .chain(chunk.iter().map(|uid| i64::from(*uid)));
                        for row in stmt
                            .query_map(rusqlite::params_from_iter(parameters), |row| {
                                Ok((row.get::<_, u32>(0)?, row.get::<_, Option<i64>>(1)?))
                            })?
                        {
                            let (uid, size) = row?;
                            sizes.insert(uid, size.unwrap_or(0).max(0) as u64);
                        }
                    }
                    sizes
                }
                None => HashMap::new(),
            }
        };
        let mut batches: Vec<Vec<u32>> = Vec::new();
        let mut current: Vec<u32> = Vec::new();
        let mut bytes: u64 = 0;
        for uid in uids {
            let size = sizes.get(uid).copied().unwrap_or(0);
            if !current.is_empty()
                && (current.len() >= TRANSFER_BATCH || bytes + size > TRANSFER_BATCH_BYTES)
            {
                batches.push(std::mem::take(&mut current));
                bytes = 0;
            }
            current.push(*uid);
            bytes += size;
        }
        if !current.is_empty() {
            batches.push(current);
        }
        Ok(batches)
    }
}

/// The flags worth carrying to another server, as an APPEND flag list:
/// the permanent system flags, not `\Recent` and not a keyword the other
/// server may refuse.
fn system_flags(flags: &[String]) -> Option<String> {
    let kept: Vec<&str> = flags
        .iter()
        .map(String::as_str)
        .filter(|flag| {
            matches!(
                flag.trim_start_matches('\\').to_ascii_lowercase().as_str(),
                "seen" | "flagged" | "answered" | "draft"
            )
        })
        .collect();
    (!kept.is_empty()).then(|| {
        format!(
            "({})",
            kept.iter()
                .map(|flag| format!("\\{}", flag.trim_start_matches('\\')))
                .collect::<Vec<_>>()
                .join(" ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_permanent_system_flags_travel() {
        let flags = vec![
            "\\Seen".to_string(),
            "\\Recent".to_string(),
            "$Label1".to_string(),
            "Flagged".to_string(),
        ];
        assert_eq!(system_flags(&flags).as_deref(), Some("(\\Seen \\Flagged)"));
        assert_eq!(system_flags(&["\\Recent".to_string()]), None);
        assert_eq!(system_flags(&[]), None);
    }
}
