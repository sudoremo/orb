//! Carrying out the filter rules of [`crate::filters`] on a folder's mail.
//!
//! After every sync of a folder the rules see the mail above the folder's
//! `last_filtered_uid`, and the watermark moves past it, so a rule acts on a
//! message once however often the folder is synced. `:filter` runs the rules
//! on mail already here, watermark or not.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, params_from_iter};

use super::{
    MessageLinkSource, PENDING_ACTION_DELETE, PendingExecutionOp, Store, SyncReport,
    message_link_sources, resolve_pending_execution_op,
};
use crate::config::AccountConfig;
use crate::filters::{self, FilterRule};
use crate::imap::{ImapClient, uid_sets};
use crate::rules::MessageFacts;
use crate::{Error, Result};

/// UIDs per header fetch and per database query, so that `:filter --all` on a
/// large folder is a series of bounded commands rather than one huge one.
const FILTER_CHUNK: usize = 500;

/// How long the filter log keeps what the rules did.
const FILTER_LOG_RETENTION_DAYS: i64 = 90;

/// What a run of the filters did.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct FilterOutcome {
    /// Messages a rule acted on.
    pub filtered: usize,
    /// What a rule could not do, one line each.
    pub warnings: Vec<String>,
    /// What puts back what the rules did, for undo.
    #[serde(skip)]
    pub undo: Vec<crate::undo::Step>,
}

impl FilterOutcome {
    pub fn merge(&mut self, other: FilterOutcome) {
        self.filtered += other.filtered;
        self.warnings.extend(other.warnings);
        self.undo.extend(other.undo);
    }
}

/// One message a rule acted on, as the review panel lists it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FilterActivity {
    pub account: String,
    pub folder: String,
    pub rule: String,
    /// The actions, and what failed of them.
    pub actions: String,
    pub subject: Option<String>,
    pub from: Option<String>,
    /// When the run started.
    pub at: String,
}

/// A message a rule may act on.
struct Candidate {
    id: i64,
    uid: u32,
    message_id: Option<String>,
    facts: MessageFacts,
}

/// A line for the filter log, written once the run is over.
struct LogRow {
    message_id: Option<String>,
    rule: String,
    actions: String,
    subject: Option<String>,
    from: Option<String>,
}

impl Store {
    /// Run the rules on the mail a sync of `folder` brought.
    ///
    /// The range is claimed by moving the watermark before anything is done,
    /// so a second Orb on this database never acts on the same mail. When the
    /// connection fails midway the range is handed back for the next sync;
    /// every action can safely be done twice.
    pub(super) async fn filter_new_mail(
        &self,
        client: &mut ImapClient,
        account: &AccountConfig,
        folder: &str,
        run_at: &str,
    ) -> Result<FilterOutcome> {
        self.begin_filter_watermark(&account.name, folder).await?;
        let Some((folder_id, watermark)) = self.filter_watermark(&account.name, folder)? else {
            return Ok(FilterOutcome::default());
        };
        let uids: Vec<u32> = {
            let db = self.reader()?;
            let mut stmt = db.conn.prepare(
                "SELECT uid FROM messages WHERE folder_id = ?1 AND uid > ?2 ORDER BY uid",
            )?;
            stmt.query_map(params![folder_id, watermark], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let Some(&newest) = uids.last() else {
            return Ok(FilterOutcome::default());
        };
        if !self
            .move_filter_watermark(folder_id, watermark, newest)
            .await?
        {
            return Ok(FilterOutcome::default());
        }

        match self
            .apply_filters(client, account, folder, folder_id, &uids, true, run_at)
            .await
        {
            // Nothing was finished: hand the range back to the next sync. Not
            // after a UIDVALIDITY change, whose next sync resets the watermark
            // anyway.
            Err(error) if !matches!(error, Error::UidValidityChanged { .. }) => {
                let advice = "`:filter` runs the rules on them";
                match self
                    .move_filter_watermark(folder_id, newest, watermark)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => crate::logger::warn(&format!(
                        "filters: {}/{folder}: UIDs {}..={newest} are left unfiltered; another \
                         Orb moved the watermark past them meanwhile; {advice}",
                        account.name,
                        watermark + 1
                    )),
                    Err(undo) => crate::logger::warn(&format!(
                        "filters: {}/{folder}: UIDs {}..={newest} are left unfiltered, the \
                         watermark could not be moved back ({undo}); {advice}",
                        account.name,
                        watermark + 1
                    )),
                }
                Err(error)
            }
            other => other,
        }
    }

    /// [`Self::filter_new_mail`] for the sync loop. What it did goes into the
    /// report; a failure is a warning rather than a failure of the sync. A
    /// connection that broke is replaced in `client`, and only when that fails
    /// too does the error come back, for the loop to stop on.
    pub(super) async fn filter_after_sync(
        &self,
        client: &mut ImapClient,
        account: &AccountConfig,
        password: &secrecy::SecretString,
        folder: &str,
        run_at: &str,
        report: &mut SyncReport,
    ) -> Result<()> {
        let error = match self.filter_new_mail(client, account, folder, run_at).await {
            Ok(outcome) => {
                report.messages_filtered += outcome.filtered;
                report.filter_warnings.extend(outcome.warnings);
                return Ok(());
            }
            Err(error) => error,
        };
        crate::logger::warn(&format!(
            "filters: {}/{folder} not run: {error}",
            account.name
        ));
        report.filter_warnings.push(format!(
            "{}/{folder}: filters not run: {error}",
            account.name
        ));
        if !error.is_connection_error() {
            return Ok(());
        }
        match Self::reconnect_imap(
            account,
            password,
            &format!("sync, after filtering {folder}"),
        )
        .await
        {
            Ok(fresh) => {
                *client = fresh;
                Ok(())
            }
            Err(reconnect_error) => {
                report.fail(
                    &account.name,
                    Some(folder),
                    reconnect_error.kind(),
                    format!("Reconnection failed: {reconnect_error} - skipping remaining folders"),
                );
                Err(error)
            }
        }
    }

    /// Give a folder whose mail has never been filtered its starting point:
    /// the highest UID of its last complete sync. Mail that was there before
    /// is never filtered, and a folder not yet completely synced gets none.
    pub(super) async fn begin_filter_watermark(&self, account: &str, folder: &str) -> Result<()> {
        Self::locked_write(&self.write_lock, || {
            self.writer()?.conn.execute(
                "UPDATE folders SET last_filtered_uid = highest_uid
                 WHERE account_name = ?1 AND name = ?2
                   AND last_filtered_uid IS NULL AND highest_uid IS NOT NULL",
                params![account, folder],
            )?;
            Ok(())
        })
        .await
    }

    /// The folder's id and watermark, when it has one.
    fn filter_watermark(&self, account: &str, folder: &str) -> Result<Option<(i64, u32)>> {
        let row = self.reader()?.conn.query_row(
            "SELECT id, last_filtered_uid FROM folders WHERE account_name = ?1 AND name = ?2",
            params![account, folder],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<u32>>(1)?)),
        );
        match row {
            Ok((id, Some(watermark))) => Ok(Some((id, watermark))),
            Ok((_, None)) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Move the watermark from `from` to `to`; false when it was no longer at
    /// `from`, because someone else moved it first.
    async fn move_filter_watermark(&self, folder_id: i64, from: u32, to: u32) -> Result<bool> {
        Self::locked_write(&self.write_lock, || {
            let changed = self.writer()?.conn.execute(
                "UPDATE folders SET last_filtered_uid = ?3
                 WHERE id = ?1 AND last_filtered_uid = ?2",
                params![folder_id, from, to],
            )?;
            Ok(changed == 1)
        })
        .await
    }

    /// Run the rules on mail already here: `uids` of one folder, or all of it
    /// when `None`. Neither the watermark nor what the rules did before is
    /// consulted, so a rule added today can be run on last month's mail.
    pub async fn run_filters(
        &self,
        account: &str,
        folder: &str,
        uids: Option<Vec<u32>>,
    ) -> Result<FilterOutcome> {
        let account_config = self.account_config(account)?;
        if self.config().filters.for_folder(account, folder).is_empty() {
            return Ok(FilterOutcome::default());
        }
        let Some(folder_id) = self
            .reader()?
            .get_folder_by_name(account, folder)?
            .and_then(|stored| stored.id)
        else {
            return Ok(FilterOutcome::default());
        };
        let uids = match uids {
            Some(uids) => uids,
            None => self.folder_uids(account, folder)?,
        };
        if uids.is_empty() {
            return Ok(FilterOutcome::default());
        }
        let run_at = chrono::Utc::now().to_rfc3339();
        self.with_connection(&account_config, "filter", async |client| {
            self.apply_filters(
                client,
                &account_config,
                folder,
                folder_id,
                &uids,
                false,
                &run_at,
            )
            .await
        })
        .await
    }

    /// Run the rules on everything synced: every folder of every account that
    /// a rule watches. An account that fails is a warning, and the others
    /// still run.
    pub async fn run_filters_everywhere(&self) -> Result<FilterOutcome> {
        let config = self.config();
        let mut total = FilterOutcome::default();
        for account in &config.accounts {
            for stored in self.reader()?.get_folders_by_account(&account.name)? {
                if config
                    .filters
                    .for_folder(&account.name, &stored.name)
                    .is_empty()
                {
                    continue;
                }
                match self.run_filters(&account.name, &stored.name, None).await {
                    Ok(outcome) => total.merge(outcome),
                    Err(error) => total
                        .warnings
                        .push(format!("{}/{}: {error}", account.name, stored.name)),
                }
            }
        }
        Ok(total)
    }

    /// Run the rules that watch `folder` on the given UIDs, on a client that
    /// may have any folder selected.
    #[allow(clippy::too_many_arguments)]
    async fn apply_filters(
        &self,
        client: &mut ImapClient,
        account: &AccountConfig,
        folder: &str,
        folder_id: i64,
        uids: &[u32],
        automatic: bool,
        run_at: &str,
    ) -> Result<FilterOutcome> {
        let config = self.config();
        let rules = config.filters.for_folder(&account.name, folder);
        let mut outcome = FilterOutcome::default();
        if rules.is_empty() || uids.is_empty() {
            return Ok(outcome);
        }

        let mut candidates = self.filter_candidates(folder_id, uids)?;
        if automatic {
            let seen = self.already_filtered(&account.name, folder, &candidates)?;
            candidates.retain(|c| c.message_id.as_ref().is_none_or(|id| !seen.contains(id)));
        }
        if candidates.is_empty() {
            return Ok(outcome);
        }

        // The headers the rules need. The answer also says which messages are
        // still here: one another Orb has filed away since is not this run's
        // to act on, and above all not to make a second task for. Nor is it
        // to stay listed, or be announced, as if it were still here.
        self.select_for_uids(client, &account.name, folder).await?;
        let fields = filters::header_fields(&rules);
        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        let mut present: HashMap<u32, Vec<u8>> = HashMap::new();
        for chunk in candidates.chunks(FILTER_CHUNK) {
            for set in uid_sets(&chunk.iter().map(|c| c.uid).collect::<Vec<_>>()) {
                for listing in client.fetch_header_listing(&set, &fields).await? {
                    present.insert(listing.uid, listing.headers);
                }
            }
        }
        let gone: Vec<u32> = candidates
            .iter()
            .filter(|candidate| !present.contains_key(&candidate.uid))
            .map(|candidate| candidate.uid)
            .collect();
        if !gone.is_empty()
            && let Err(error) = self
                .delete_messages_by_uids(&account.name, folder, &gone)
                .await
        {
            crate::logger::warn(&format!(
                "filters: {}/{folder}: {} message(s) the server no longer has are still listed \
                 here ({error}); the next sync puts that right",
                account.name,
                gone.len()
            ));
        }
        candidates.retain_mut(|candidate| match present.get(&candidate.uid) {
            Some(block) => {
                candidate.facts.add_header_block(block);
                true
            }
            None => false,
        });

        let mut by_uid: HashMap<u32, (i64, Option<String>)> = HashMap::new();
        let mut messages = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            by_uid.insert(candidate.uid, (candidate.id, candidate.message_id));
            messages.push((candidate.uid, candidate.facts));
        }
        let batches = filters::plan(&rules, &messages);
        if batches.is_empty() {
            return Ok(outcome);
        }

        // The database knows the folders a sync of the account listed; a
        // sync of one folder before that asks the server, once.
        let mut existing: Vec<String> = self
            .reader()?
            .get_folders_by_account(&account.name)?
            .into_iter()
            .map(|stored| stored.name)
            .collect();
        let mut listed_server = false;
        for batch in &batches {
            if let Some(target) = &batch.rule.then.move_to
                && !listed_server
                && filters::resolve_folder(target, &existing).is_none()
            {
                listed_server = true;
                for name in client.list_folders().await? {
                    if !existing.contains(&name) {
                        existing.push(name);
                    }
                }
            }
        }
        let mut acted: HashSet<u32> = HashSet::new();
        let mut log = Vec::new();
        for batch in batches {
            let ids: Vec<i64> = batch.uids.iter().map(|uid| by_uid[uid].0).collect();
            let uid_of: HashMap<i64, u32> = ids
                .iter()
                .copied()
                .zip(batch.uids.iter().copied())
                .collect();
            let sources = {
                let db = self.reader()?;
                message_link_sources(&db.conn, &ids)?
            };
            let problems = self
                .apply_batch(
                    client,
                    account,
                    (folder, folder_id),
                    batch.rule,
                    &batch.uids,
                    &existing,
                    (&sources, &uid_of),
                    &mut outcome.undo,
                )
                .await?;

            let actions = match problems.is_empty() {
                true => batch.rule.then.describe(),
                false => format!(
                    "{} (failed: {})",
                    batch.rule.then.describe(),
                    problems.join("; ")
                ),
            };
            for problem in &problems {
                outcome.warnings.push(format!(
                    "filter '{}' on {}/{folder}: {problem}",
                    batch.rule.name, account.name
                ));
            }
            for source in &sources {
                let uid = uid_of.get(&source.local_id).copied().unwrap_or(0);
                crate::logger::info(&format!(
                    "filter '{}': {}/{folder} UID {uid} {}: {actions}",
                    batch.rule.name,
                    account.name,
                    source
                        .rfc_message_id
                        .as_deref()
                        .unwrap_or("(no Message-ID)"),
                ));
                log.push(LogRow {
                    message_id: source.rfc_message_id.clone(),
                    rule: batch.rule.name.clone(),
                    actions: actions.clone(),
                    subject: source.subject.clone(),
                    from: source.from.clone(),
                });
            }
            acted.extend(batch.uids.iter().copied());
        }
        outcome.filtered = acted.len();
        self.write_filter_log(&account.name, folder, run_at, &log)
            .await?;
        Ok(outcome)
    }

    /// Carry out one rule's actions on its messages: the flags, then the move
    /// (the moved copies keep their flags), then the tasks, linked to where
    /// the messages ended up. A task is made only for a message this move
    /// relocated, as the server confirms it: one another Orb filed meanwhile
    /// gets its task there. Returns what could not be done; a connection that
    /// fails is an error, since nothing after it can be done either. What puts
    /// each action back is added to `undo`.
    #[allow(clippy::too_many_arguments)]
    async fn apply_batch(
        &self,
        client: &mut ImapClient,
        account: &AccountConfig,
        (folder, folder_id): (&str, i64),
        rule: &FilterRule,
        uids: &[u32],
        existing: &[String],
        (sources, uid_of): (&[MessageLinkSource], &HashMap<i64, u32>),
        undo: &mut Vec<crate::undo::Step>,
    ) -> Result<Vec<String>> {
        let then = &rule.then;
        let mut problems = Vec::new();
        let message_ids: Vec<String> = sources
            .iter()
            .filter_map(|source| source.rfc_message_id.clone())
            .collect();

        let mut flags = Vec::new();
        if then.read {
            flags.push("\\Seen");
        }
        if then.flag {
            flags.push("\\Flagged");
        }
        for flag in flags {
            match client.store_flags(uids, &[flag], true).await {
                Ok(()) => {
                    let changed = Self::locked_write(&self.write_lock, || {
                        self.writer()?.set_flag_locally(folder_id, uids, flag, true)
                    })
                    .await?;
                    let (account, folder) = (account.name.clone(), folder.to_string());
                    undo.push(match flag {
                        "\\Seen" => crate::undo::Step::Read {
                            account,
                            folder,
                            uids: changed,
                            read: false,
                        },
                        _ => crate::undo::Step::Flagged {
                            account,
                            folder,
                            uids: changed,
                            flagged: false,
                        },
                    });
                }
                Err(error) if error.is_connection_error() => return Err(error),
                Err(error) => problems.push(format!("{flag}: {error}")),
            }
        }

        let reading = then
            .reading
            .then(|| self.config().reading.clone())
            .flatten();
        if then.reading && reading.is_none() {
            problems.push("reading: there is no `reading` section".to_string());
        }
        let mut ended_in = folder.to_string();
        let mut moved: Option<Vec<u32>> = None;
        let elsewhere = reading.as_ref().filter(|r| r.account != account.name);
        if let Some(reading) = elsewhere {
            // Another account's folder: no server moves a message there, so it
            // travels through this machine as a cross-account move does.
            match self.reading_folder(false) {
                Some((to_account, to_folder)) => match self
                    .transfer_messages(&account.name, folder, &to_account, &to_folder, uids, false)
                    .await
                {
                    Ok(outcome) => {
                        undo.push(crate::undo::Step::MoveAcross {
                            from: crate::undo::Place::new(&to_account, &to_folder),
                            to: crate::undo::Place::new(&account.name, folder),
                            message_ids: message_ids.clone(),
                        });
                        ended_in = to_folder;
                        problems.extend(outcome.warning);
                    }
                    Err(error) if error.is_connection_error() => return Err(error),
                    Err(error) => problems.push(format!("reading: {error}")),
                },
                None => problems.push(format!(
                    "{} has no folder '{}' yet",
                    reading.account, reading.folder
                )),
            }
        }

        let destination = if elsewhere.is_some() {
            Ok(None)
        } else if let Some(target) = then
            .move_to
            .as_ref()
            .or(reading.as_ref().map(|r| &r.folder))
        {
            match filters::resolve_folder(target, existing) {
                Some(found) if found == folder => Ok(None),
                Some(found) => Ok(Some(found)),
                None => Err(format!("{} has no folder '{target}'", account.name)),
            }
        } else if then.delete {
            // The Trash as the server spells it, like a `move` target; the
            // configured name when the account has no such folder yet.
            let trash = filters::resolve_folder(&account.trash_folder, existing)
                .unwrap_or_else(|| account.trash_folder.clone());
            match resolve_pending_execution_op(PENDING_ACTION_DELETE, folder, &trash, None)? {
                PendingExecutionOp::MoveToTrash { trash } => Ok(Some(trash)),
                _ => Err(format!(
                    "{folder} is the Trash, and a filter never deletes for good"
                )),
            }
        } else {
            Ok(None)
        };
        match destination {
            Ok(Some(target)) => match client.move_messages(uids, &target).await {
                Ok(outcome) => {
                    moved = outcome.confirmed;
                    undo.push(crate::undo::Step::Move {
                        account: account.name.clone(),
                        from: target.clone(),
                        to: folder.to_string(),
                        message_ids: message_ids.clone(),
                    });
                    ended_in = target.clone();
                    if let Err(error) = self
                        .delete_messages_by_uids(&account.name, folder, uids)
                        .await
                    {
                        crate::logger::warn(&format!(
                            "filter '{}': {}/{folder}: moved to {target} on the server but still \
                             listed here ({error}); the next sync puts that right",
                            rule.name, account.name
                        ));
                    }
                }
                Err(error) if error.is_connection_error() => return Err(error),
                Err(error) => problems.push(format!("move to {target}: {error}")),
            },
            Ok(None) => {}
            Err(why) => problems.push(why),
        }

        if then.task {
            let own = task_sources(sources, uid_of, moved.as_deref());
            if own.len() < sources.len() {
                crate::logger::info(&format!(
                    "filter '{}': {}/{folder}: {} of {} message(s) were filed by another Orb \
                     first, which makes their tasks",
                    rule.name,
                    account.name,
                    sources.len() - own.len(),
                    sources.len()
                ));
            }
            let made = self.create_filter_tasks(&own, &ended_in);
            if let Ok((created, _)) = &made {
                undo.push(crate::undo::Step::Tasks(crate::tasks::UndoEntry {
                    label: String::new(),
                    steps: created
                        .iter()
                        .map(|&task_id| crate::tasks::UndoStep::Trash { task_id })
                        .collect(),
                }));
            }
            match made {
                Ok((_, 0)) => {}
                Ok((_, unlinkable)) => problems.push(format!(
                    "task: {unlinkable} message(s) without a Message-ID got none, since a task \
                     for them could not be told from a second one"
                )),
                Err(error) => problems.push(format!("task: {error}")),
            }
        }

        Ok(problems)
    }

    /// One task per message, linked to it — unless a task is linked to it
    /// already, which is how a message another Orb filtered first gets one
    /// task rather than two. A message without a Message-ID gets none: the
    /// link is what tells its task from a second one, and a rule run again
    /// would make one every time. Returns the tasks created and how many
    /// messages were left without one.
    fn create_filter_tasks(
        &self,
        sources: &[&MessageLinkSource],
        folder: &str,
    ) -> Result<(Vec<i64>, usize)> {
        let config = self.config();
        let Some(tasks) = &config.tasks else {
            return Err(Error::NotFound(
                "no `tasks` section in the configuration".to_string(),
            ));
        };
        let munger = crate::subject_munger::SubjectMunger::from_config(&config.subject_munges);
        let db = self.writer()?;
        let repo = crate::tasks::TaskRepository::new(&db);
        let device_id = repo.ensure_device_id(&tasks.account)?;
        let ctx = repo.context_at(chrono::Utc::now(), Some(device_id.as_str()));

        let mut created = Vec::new();
        let mut unlinkable = 0;
        for source in sources {
            let Some(link) = source
                .rfc_message_id
                .as_deref()
                .and_then(crate::tasks::canonical_message_id)
            else {
                unlinkable += 1;
                continue;
            };
            if !repo.tasks_for_message(&link)?.is_empty() {
                continue;
            }
            let title = crate::tasks::normalise_title(&munger.munge(source.subject.as_deref()));
            let task = repo.create_task(&title, &tasks.account, &ctx)?;
            let task_id = task.id.unwrap_or_default();
            repo.link_email(
                task_id,
                &link,
                &crate::tasks::CachedEmailMetadata {
                    subject: source.subject.clone(),
                    from: source.from.clone(),
                    date: source.date.clone(),
                    account: Some(source.account_name.clone()),
                    folder: Some(folder.to_string()),
                },
                &ctx,
            )?;
            created.push(task_id);
        }
        Ok((created, unlinkable))
    }

    /// What the rules get to see of the given messages.
    fn filter_candidates(&self, folder_id: i64, uids: &[u32]) -> Result<Vec<Candidate>> {
        let db = self.reader()?;
        let mut candidates = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(FILTER_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut stmt = db.conn.prepare(&format!(
                "SELECT id, uid, message_id, subject, from_addr, to_addr, cc_addr, has_attachments
                 FROM messages WHERE folder_id = ? AND uid IN ({placeholders})"
            ))?;
            let parameters =
                std::iter::once(folder_id).chain(chunk.iter().map(|uid| i64::from(*uid)));
            let rows = stmt.query_map(params_from_iter(parameters), |row| {
                Ok(Candidate {
                    id: row.get(0)?,
                    uid: row.get(1)?,
                    message_id: row
                        .get::<_, Option<String>>(2)?
                        .filter(|id| !id.trim().is_empty()),
                    facts: MessageFacts {
                        subject: row.get(3)?,
                        from: row.get(4)?,
                        to: row.get(5)?,
                        cc: row.get(6)?,
                        has_attachment: row.get::<_, Option<bool>>(7)?.unwrap_or(false),
                        headers: HashMap::new(),
                    },
                })
            })?;
            for row in rows {
                candidates.push(row?);
            }
        }
        candidates.sort_by_key(|c| c.uid);
        Ok(candidates)
    }

    /// The Message-IDs among `candidates` that a rule already acted on in this
    /// folder. A message moved back by hand, or by an undo, arrives under a
    /// new UID above the watermark, and must not be filed away again.
    fn already_filtered(
        &self,
        account: &str,
        folder: &str,
        candidates: &[Candidate],
    ) -> Result<HashSet<String>> {
        let ids: Vec<&str> = candidates
            .iter()
            .filter_map(|c| c.message_id.as_deref())
            .collect();
        let db = self.reader()?;
        let mut seen = HashSet::new();
        for chunk in ids.chunks(FILTER_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut stmt = db.conn.prepare(&format!(
                "SELECT DISTINCT message_id FROM filter_log
                 WHERE account = ? AND folder = ? AND message_id IN ({placeholders})"
            ))?;
            let parameters = [account, folder].into_iter().chain(chunk.iter().copied());
            for id in stmt.query_map(params_from_iter(parameters), |row| row.get::<_, String>(0))? {
                seen.insert(id?);
            }
        }
        Ok(seen)
    }

    /// Record what a run did, and forget what runs long past did.
    async fn write_filter_log(
        &self,
        account: &str,
        folder: &str,
        run_at: &str,
        rows: &[LogRow],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::days(FILTER_LOG_RETENTION_DAYS)).to_rfc3339();
        Self::locked_write(&self.write_lock, || {
            self.writer()?.transaction(|tx| {
                tx.execute("DELETE FROM filter_log WHERE run_at < ?1", params![cutoff])?;
                let mut insert = tx.prepare(
                    "INSERT INTO filter_log
                         (account, folder, message_id, rule, actions, subject, from_addr, run_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for row in rows {
                    insert.execute(params![
                        account,
                        folder,
                        row.message_id,
                        row.rule,
                        row.actions,
                        row.subject,
                        row.from,
                        run_at
                    ])?;
                }
                Ok(())
            })
        })
        .await
    }

    /// What the rules did in each account's last run that did anything.
    pub fn filter_activity(&self) -> Result<Vec<FilterActivity>> {
        let db = self.reader()?;
        let mut stmt = db.conn.prepare(
            "SELECT account, folder, rule, actions, subject, from_addr, run_at
             FROM filter_log l
             WHERE run_at = (SELECT MAX(run_at) FROM filter_log m WHERE m.account = l.account)
             ORDER BY account, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(FilterActivity {
                account: row.get(0)?,
                folder: row.get(1)?,
                rule: row.get(2)?,
                actions: row.get(3)?,
                subject: row.get(4)?,
                from: row.get(5)?,
                at: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// What is wrong with the filter rules: the ones that did not parse, and
    /// the folders they move to that an account does not have. An account not
    /// synced yet is not judged.
    pub fn filter_warnings(&self) -> Result<Vec<String>> {
        let config = self.config();
        let mut warnings = config.filters.warnings.clone();
        let db = self.reader()?;
        for account in &config.accounts {
            let existing: Vec<String> = db
                .get_folders_by_account(&account.name)?
                .into_iter()
                .map(|stored| stored.name)
                .collect();
            if existing.is_empty() {
                continue;
            }
            for rule in &config.filters.rules {
                let Some(target) = &rule.then.move_to else {
                    continue;
                };
                if (rule.accounts.is_empty() || rule.accounts.contains(&account.name))
                    && filters::resolve_folder(target, &existing).is_none()
                {
                    warnings.push(format!(
                        "filter '{}': {} has no folder '{target}' to move to",
                        rule.name, account.name
                    ));
                }
            }
        }
        Ok(warnings)
    }
}

/// The messages a rule's `task` is for: all of them, unless the server said
/// which the move relocated, in which case only those. The rest were gone
/// before the move — filed by another Orb, which makes their tasks.
fn task_sources<'a>(
    sources: &'a [MessageLinkSource],
    uid_of: &HashMap<i64, u32>,
    moved: Option<&[u32]>,
) -> Vec<&'a MessageLinkSource> {
    sources
        .iter()
        .filter(|source| match moved {
            Some(moved) => uid_of
                .get(&source.local_id)
                .is_some_and(|uid| moved.contains(uid)),
            None => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(local_id: i64) -> MessageLinkSource {
        MessageLinkSource {
            local_id,
            rfc_message_id: Some(format!("<{local_id}@example.com>")),
            subject: None,
            from: None,
            date: None,
            account_name: "A".to_string(),
            folder_name: "INBOX".to_string(),
        }
    }

    /// Two Orbs run the same rule on the same message; the server's COPYUID
    /// says whose move took it, and only that Orb makes the task. A server
    /// that does not say leaves the decision as it was: a task for each.
    #[test]
    fn a_task_is_made_for_what_this_move_relocated() {
        let sources = vec![source(1), source(2), source(3)];
        let uid_of: HashMap<i64, u32> = [(1, 11), (2, 12), (3, 13)].into_iter().collect();

        let own = task_sources(&sources, &uid_of, Some(&[11, 13]));
        assert_eq!(
            own.iter().map(|s| s.local_id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(task_sources(&sources, &uid_of, Some(&[])).is_empty());
        assert_eq!(task_sources(&sources, &uid_of, None).len(), 3);
    }
}
