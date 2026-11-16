//! What the interface says about each account's connection.
//!
//! A sync that fails leaves its reason in a report that is read once and
//! shown as "· 2 errors"; this keeps the reason, per account, so the sidebar
//! can mark the account and a panel can say what is wrong and since when.
//! Every sync — manual, pushed, periodic — reports into it, so it is never
//! more than one sync out of date.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::FailureKind;
use crate::store::SyncReport;

/// The one word the sidebar shows for an account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// No sync has run yet since launch.
    #[default]
    Unknown,
    /// A sync is running now.
    Syncing,
    /// The last sync did everything it set out to do.
    Ok,
    /// The server refused the login.
    AuthFailed,
    /// The TLS handshake failed; see the message for the certificate's fault.
    Certificate,
    /// The server could not be reached, or stopped answering.
    Unreachable,
    /// The last sync had some other failure; the message says which.
    Error,
}

impl From<FailureKind> for HealthState {
    fn from(kind: FailureKind) -> Self {
        match kind {
            FailureKind::Auth => HealthState::AuthFailed,
            FailureKind::Certificate => HealthState::Certificate,
            FailureKind::Unreachable => HealthState::Unreachable,
            FailureKind::Server | FailureKind::Other => HealthState::Error,
        }
    }
}

/// One account's standing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccountHealth {
    pub account: String,
    pub state: HealthState,
    /// What went wrong last, kept until a sync succeeds.
    pub message: Option<String>,
    /// The folder the failure concerned, when it concerned one.
    pub folder: Option<String>,
    /// When the state last changed, RFC 3339 UTC.
    pub at: Option<String>,
    /// When a sync last finished without a failure, RFC 3339 UTC.
    pub last_ok: Option<String>,
    /// Why the tasks folder, on the tasks account, cannot be synced while it
    /// cannot. Apart from `state`: the mail can be fine while the tasks are
    /// not.
    #[serde(default)]
    pub tasks: Option<String>,
}

#[derive(Default)]
struct Entry {
    health: AccountHealth,
    /// Syncs running now. A folder sync and an account sync can overlap, so
    /// "syncing" ends when the last of them does.
    running: u32,
    /// The outcome under the running syncs, to go back to when they end.
    settled: HealthState,
}

/// The accounts' standing, shared between the manual syncs and the scheduler.
pub struct HealthBook {
    entries: Mutex<HashMap<String, Entry>>,
}

impl Default for AccountHealth {
    fn default() -> Self {
        AccountHealth {
            account: String::new(),
            state: HealthState::Unknown,
            message: None,
            folder: None,
            at: None,
            last_ok: None,
            tasks: None,
        }
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl HealthBook {
    pub fn new(accounts: impl IntoIterator<Item = String>) -> Self {
        let entries = accounts
            .into_iter()
            .map(|name| {
                let mut entry = Entry::default();
                entry.health.account = name.clone();
                (name, entry)
            })
            .collect();
        HealthBook {
            entries: Mutex::new(entries),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut HashMap<String, Entry>) -> T) -> T {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut entries)
    }

    /// A sync of `account` began.
    pub fn started(&self, account: &str) {
        self.with(|entries| {
            let Some(entry) = entries.get_mut(account) else {
                return;
            };
            if entry.running == 0 {
                entry.settled = entry.health.state;
            }
            entry.running += 1;
            entry.health.state = HealthState::Syncing;
        });
    }

    /// A sync of `account` ended with `report`. Returns whether the standing
    /// changed from what it was before the sync, which is when a listener
    /// has something new to show.
    pub fn finished(&self, account: &str, report: &SyncReport) -> bool {
        let outcome = match report.worst_failure(account) {
            Some(failure) => (
                HealthState::from(failure.kind),
                Some(failure.message.clone()),
                failure.folder.clone(),
            ),
            None => (HealthState::Ok, None, None),
        };
        self.settle(account, outcome)
    }

    /// A sync of `account` failed outright.
    pub fn failed(
        &self,
        account: &str,
        folder: Option<&str>,
        kind: FailureKind,
        message: &str,
    ) -> bool {
        self.settle(
            account,
            (
                HealthState::from(kind),
                Some(message.to_string()),
                folder.map(str::to_string),
            ),
        )
    }

    /// The tasks folder of `account` failed to sync with `problem`, or, with
    /// `None`, synced.
    pub fn tasks_problem(&self, account: &str, problem: Option<String>) {
        self.with(|entries| {
            if let Some(entry) = entries.get_mut(account) {
                entry.health.tasks = problem;
            }
        });
    }

    fn settle(
        &self,
        account: &str,
        (state, message, folder): (HealthState, Option<String>, Option<String>),
    ) -> bool {
        self.with(|entries| {
            let Some(entry) = entries.get_mut(account) else {
                return false;
            };
            let before = entry.settled;
            entry.running = entry.running.saturating_sub(1);
            let stamp = now();
            if state == HealthState::Ok {
                entry.health.last_ok = Some(stamp.clone());
            }
            if state != before || entry.health.message != message {
                entry.health.at = Some(stamp);
            }
            entry.health.message = message;
            entry.health.folder = folder;
            entry.settled = state;
            entry.health.state = if entry.running > 0 {
                HealthState::Syncing
            } else {
                state
            };
            before != state
        })
    }

    /// Every account's standing, in the configured order.
    pub fn snapshot(&self, order: &[String]) -> Vec<AccountHealth> {
        self.with(|entries| {
            order
                .iter()
                .filter_map(|name| entries.get(name).map(|e| e.health.clone()))
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SyncFailure;

    fn report(failures: Vec<(FailureKind, &str)>) -> SyncReport {
        SyncReport {
            failures: failures
                .into_iter()
                .map(|(kind, message)| SyncFailure {
                    account: "Work".into(),
                    folder: Some("INBOX".into()),
                    kind,
                    message: message.into(),
                })
                .collect(),
            ..SyncReport::default()
        }
    }

    #[test]
    fn a_refused_login_outranks_the_folder_errors_that_followed_it() {
        let book = HealthBook::new(["Work".to_string()]);
        book.started("Work");
        assert_eq!(
            book.snapshot(&["Work".into()])[0].state,
            HealthState::Syncing
        );

        let changed = book.finished(
            "Work",
            &report(vec![
                (FailureKind::Server, "Folder 'Sent': NO"),
                (FailureKind::Auth, "Login failed: nope"),
            ]),
        );
        assert!(changed);
        let health = &book.snapshot(&["Work".into()])[0];
        assert_eq!(health.state, HealthState::AuthFailed);
        assert_eq!(health.message.as_deref(), Some("Login failed: nope"));
        assert!(health.at.is_some());
        assert!(health.last_ok.is_none());
    }

    #[test]
    fn overlapping_syncs_stay_syncing_until_the_last_one_ends() {
        let book = HealthBook::new(["Work".to_string()]);
        book.started("Work");
        book.started("Work");
        book.finished("Work", &report(vec![]));
        assert_eq!(
            book.snapshot(&["Work".into()])[0].state,
            HealthState::Syncing
        );
        book.finished("Work", &report(vec![]));
        let health = &book.snapshot(&["Work".into()])[0];
        assert_eq!(health.state, HealthState::Ok);
        assert!(health.last_ok.is_some());
    }

    #[test]
    fn a_success_clears_the_message_and_an_unknown_account_is_ignored() {
        let book = HealthBook::new(["Work".to_string()]);
        book.failed("Work", None, FailureKind::Unreachable, "connection refused");
        assert_eq!(
            book.snapshot(&["Work".into()])[0].state,
            HealthState::Unreachable
        );
        book.started("Work");
        book.finished("Work", &report(vec![]));
        let health = &book.snapshot(&["Work".into()])[0];
        assert_eq!(health.state, HealthState::Ok);
        assert!(health.message.is_none());
        book.started("Nobody");
        assert_eq!(book.snapshot(&["Work".into(), "Nobody".into()]).len(), 1);
    }
}
