//! The reading list's two folders: made on the server when they are missing,
//! and found again under the names the server gives them.

use super::Store;
use crate::filters;
use crate::imap::ImapClient;
use crate::tasks::plan_tasks_folder;

impl Store {
    /// Create whichever reading folder `account` lacks, if it holds the
    /// reading list, and return the names of those created so that the sync
    /// that listed the folders picks them up too.
    ///
    /// A folder the server will not create is a warning, not a failed sync:
    /// the mail still has to arrive.
    pub(super) async fn ensure_reading_folders(
        &self,
        client: &mut ImapClient,
        account: &str,
        delimiter: Option<&str>,
        existing: &[String],
    ) -> Vec<String> {
        let config = self.config();
        let Some(reading) = config.reading.as_ref().filter(|r| r.account == account) else {
            return Vec::new();
        };
        let mut created = Vec::new();
        for requested in [&reading.folder, &reading.archive_folder] {
            let plan = plan_tasks_folder(requested, delimiter, existing);
            if !plan.create {
                continue;
            }
            crate::logger::info(&format!(
                "{account}: creating the reading folder '{}' (configured as '{requested}')",
                plan.name
            ));
            match client.create_folder(&plan.name).await {
                Ok(()) => created.push(plan.name),
                Err(error) => crate::logger::warn(&format!(
                    "{account}: could not create the reading folder '{}': {error}",
                    plan.name
                )),
            }
        }
        created
    }

    /// The reading folder, or with `archive` its archive, as `(account,
    /// folder)` under the name the server gives it; `None` without a
    /// `reading` section or before the folder has been listed.
    pub fn reading_folder(&self, archive: bool) -> Option<(String, String)> {
        let config = self.config();
        let reading = config.reading.as_ref()?;
        let requested = if archive {
            &reading.archive_folder
        } else {
            &reading.folder
        };
        let existing: Vec<String> = self
            .listed_folders(&reading.account)
            .iter()
            .map(|folder| folder.name.clone())
            .collect();
        let folder = filters::resolve_folder(requested, &existing)?;
        Some((reading.account.clone(), folder))
    }
}
