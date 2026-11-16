//! The accounts' passwords, each obtained once per session.
//!
//! Every connection used to run the account's `imap_password_command` afresh:
//! a sync, a mark-read, an IDLE reconnect, each one a process spawn, each one
//! blocking a runtime worker for as long as a keychain or a pinentry took to
//! answer. The cache obtains each password once, on a blocking thread, and
//! keeps it until a login fails with it.
//!
//! Where a password comes from, in order:
//!
//! 1. the account's command, when it has one;
//! 2. the Keychain item Orb filed under the login's name on an earlier run;
//! 3. whoever is at the keyboard, through the [`PasswordPrompter`] the
//!    application registers — and if they tick the box, the answer is filed
//!    in the Keychain for next time.
//!
//! An SMTP login without a command of its own shares the IMAP password when
//! it is the same username on the same server, which it is by default.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as SyncMutex, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use secrecy::SecretString;
use tokio::sync::Mutex;

use crate::config::{AccountConfig, Credential, CredentialKind};
use crate::error::Error;
use crate::keychain::{Keychain, PasswordStore};
use crate::{Result, logger};

/// How long the password command may run.
const PASSWORD_COMMAND_TIMEOUT: Duration =
    Duration::from_secs(crate::constants::PASSWORD_COMMAND_TIMEOUT_SECONDS);

/// What the cache needs to know to ask for a password.
#[derive(Debug, Clone)]
pub struct PasswordRequest {
    /// The account's name in the config.
    pub account: String,
    /// `IMAP` or `SMTP`.
    pub kind: String,
    pub username: String,
    pub server: String,
    /// Whether the server refused the last password given for this login.
    pub retry: bool,
}

/// What came back from a prompt.
pub struct PasswordAnswer {
    pub password: SecretString,
    /// Whether to file the password in the Keychain for the next run.
    pub store: bool,
}

/// Whoever can ask the user for a password: the application, which has a
/// window to ask in. The future resolves to `None` when the prompt was
/// dismissed. It is not timed out here — a person answers when they are back
/// at the machine, and until then the login that needs it waits.
pub trait PasswordPrompter: Send + Sync + 'static {
    fn ask(&self, request: PasswordRequest) -> BoxFuture<'static, Option<PasswordAnswer>>;
}

/// One login's password, and whether the last one was refused.
#[derive(Default)]
struct Slot {
    password: Option<SecretString>,
    refused: bool,
}

/// One login's slot, under a lock of its own.
type Slots = HashMap<(CredentialKind, String), Arc<Mutex<Slot>>>;

/// Passwords by account and kind, filled on first use.
pub struct PasswordCache {
    /// One lock per login, so a prompt for one account does not hold up
    /// another's sync; the outer lock is held only to find the slot.
    slots: SyncMutex<Slots>,
    store: Box<dyn PasswordStore>,
    prompter: RwLock<Option<Arc<dyn PasswordPrompter>>>,
}

impl Default for PasswordCache {
    fn default() -> Self {
        Self::with_store(Box::new(Keychain))
    }
}

impl PasswordCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A cache that files passwords somewhere other than the Keychain.
    pub fn with_store(store: Box<dyn PasswordStore>) -> Self {
        Self {
            slots: SyncMutex::new(HashMap::new()),
            store,
            prompter: RwLock::new(None),
        }
    }

    /// Register who to ask when no command and no Keychain item answers.
    /// Without one, such a login fails with an error saying so.
    pub fn set_prompter(&self, prompter: Arc<dyn PasswordPrompter>) {
        *self.prompter.write().unwrap_or_else(|e| e.into_inner()) = Some(prompter);
    }

    /// The account's IMAP password, obtained if this is the first time it is
    /// asked for.
    pub async fn imap(&self, account: &AccountConfig) -> Result<SecretString> {
        self.lookup(CredentialKind::Imap, account).await
    }

    /// The account's SMTP password, which defaults to the IMAP one.
    pub async fn smtp(&self, account: &AccountConfig) -> Result<SecretString> {
        self.lookup(Self::smtp_slot(account), account).await
    }

    /// Where the SMTP password is kept: in the IMAP slot when the two logins
    /// are one and SMTP has no command of its own.
    fn smtp_slot(account: &AccountConfig) -> CredentialKind {
        let shares_imap_login = account.password_command(CredentialKind::Smtp).is_none()
            && account
                .credential(CredentialKind::Smtp)
                .same_login(&account.credential(CredentialKind::Imap));
        if shares_imap_login {
            CredentialKind::Imap
        } else {
            CredentialKind::Smtp
        }
    }

    /// Forget an account's IMAP password, because a login with it failed.
    /// The next request obtains it again — and, where it came from the
    /// Keychain or a prompt, asks rather than reading back what was refused.
    pub async fn forget_imap(&self, account_name: &str) {
        self.forget(CredentialKind::Imap, account_name).await;
    }

    /// Forget the password [`Self::smtp`] gave for this login, because the
    /// mail server refused it — the IMAP one where the two are shared.
    pub async fn forget_smtp(&self, account: &AccountConfig) {
        self.forget(Self::smtp_slot(account), &account.name).await;
    }

    async fn forget(&self, kind: CredentialKind, account_name: &str) {
        let slot = self.slot(kind, account_name);
        let mut slot = slot.lock().await;
        if slot.password.take().is_some() {
            slot.refused = true;
            logger::info(&format!(
                "credentials: forgot the {} password of '{account_name}' after a failed login",
                kind.as_str()
            ));
        }
    }

    fn slot(&self, kind: CredentialKind, account_name: &str) -> Arc<Mutex<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((kind, account_name.to_string()))
            .or_default()
            .clone()
    }

    async fn lookup(&self, kind: CredentialKind, account: &AccountConfig) -> Result<SecretString> {
        // The slot's lock is held across the command on purpose: two syncs
        // asking at once should run the command once, not race two keychain
        // prompts.
        let slot = self.slot(kind, &account.name);
        let mut slot = slot.lock().await;
        if let Some(password) = &slot.password {
            return Ok(password.clone());
        }

        let password = match account.password_command(kind) {
            Some(_) => self.run_command(kind, account).await?,
            None => {
                let credential = account.credential(kind);
                match self.stored_password(&credential, slot.refused) {
                    Some(password) => password,
                    None => {
                        self.prompted_password(account, credential, slot.refused)
                            .await?
                    }
                }
            }
        };
        slot.password = Some(password.clone());
        slot.refused = false;
        Ok(password)
    }

    async fn run_command(
        &self,
        kind: CredentialKind,
        account: &AccountConfig,
    ) -> Result<SecretString> {
        let name = account.name.clone();
        let config = account.clone();
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            PASSWORD_COMMAND_TIMEOUT,
            tokio::task::spawn_blocking(move || match kind {
                CredentialKind::Imap => config.get_imap_password(),
                CredentialKind::Smtp => config.get_smtp_password(),
            }),
        )
        .await;
        let kind = kind.as_str();
        let password = match outcome {
            Ok(Ok(Ok(Some(password)))) => password,
            Ok(Ok(Ok(None))) => unreachable!("run_command is only called with a command"),
            Ok(Ok(Err(error))) => return Err(Error::from(error)),
            Ok(Err(join)) => {
                return Err(Error::Sync(format!(
                    "the {kind} password command of '{name}' could not be run: {join}"
                )));
            }
            Err(_) => {
                return Err(Error::Timeout {
                    operation: format!("{kind} password command of '{name}'"),
                    seconds: PASSWORD_COMMAND_TIMEOUT.as_secs(),
                });
            }
        };
        logger::info(&format!(
            "credentials: ran the {kind} password command of '{name}' in {:?}",
            started.elapsed()
        ));
        Ok(password)
    }

    /// The Keychain's answer, unless the server has just refused it.
    fn stored_password(&self, credential: &Credential, refused: bool) -> Option<SecretString> {
        if refused {
            return None;
        }
        let password = self.store.get(&credential.key())?;
        logger::info(&format!(
            "credentials: read the {} password of {} from the Keychain",
            credential.kind.as_str(),
            credential.key()
        ));
        Some(password)
    }

    async fn prompted_password(
        &self,
        account: &AccountConfig,
        credential: Credential,
        retry: bool,
    ) -> Result<SecretString> {
        let field = format!(
            "accounts[{}].{}_password_command",
            account.name,
            credential.kind.as_str().to_ascii_lowercase()
        );
        let prompter = self
            .prompter
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(prompter) = prompter else {
            return Err(Error::InvalidConfig {
                field,
                reason: "no password command is set and nothing is stored in the Keychain"
                    .to_string(),
            });
        };
        let request = PasswordRequest {
            account: account.name.clone(),
            kind: credential.kind.as_str().to_string(),
            username: credential.username.clone(),
            server: credential.server.clone(),
            retry,
        };
        let Some(answer) = prompter.ask(request).await else {
            return Err(Error::InvalidConfig {
                field,
                reason: "no password was entered".to_string(),
            });
        };
        if answer.store {
            match self.store.set(&credential.key(), &answer.password) {
                Ok(()) => logger::info(&format!(
                    "credentials: stored the password of {} in the Keychain",
                    credential.key()
                )),
                Err(error) => logger::warn(&format!(
                    "credentials: could not store the password of {} in the Keychain: {error}",
                    credential.key()
                )),
            }
        }
        Ok(answer.password)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(feature = "desktop")]
    fn account(command: &str) -> AccountConfig {
        serde_saphyr::from_str(&format!(
            "name: Test\nimap_server: imap.example.com\nimap_port: 993\n\
             imap_username: user\nimap_password_command: {command}\nfrom: Test <t@example.com>\n"
        ))
        .expect("an account")
    }

    fn account_without_command(extra: &str) -> AccountConfig {
        serde_saphyr::from_str(&format!(
            "name: Test\nimap_server: imap.example.com\nimap_port: 993\n\
             imap_username: user\nfrom: Test <t@example.com>\n{extra}"
        ))
        .expect("an account")
    }

    #[derive(Default)]
    struct MemoryStore(SyncMutex<HashMap<String, String>>);

    impl PasswordStore for MemoryStore {
        fn get(&self, key: &str) -> Option<SecretString> {
            self.0
                .lock()
                .unwrap()
                .get(key)
                .map(|p| SecretString::from(p.clone()))
        }

        fn set(&self, key: &str, password: &SecretString) -> std::result::Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_string(), password.expose_secret().to_string());
            Ok(())
        }
    }

    /// Answers every prompt with the same password, counting the prompts.
    struct Typist {
        password: &'static str,
        store: bool,
        asked: AtomicUsize,
        last: SyncMutex<Option<PasswordRequest>>,
    }

    impl Typist {
        fn new(password: &'static str, store: bool) -> Arc<Self> {
            Arc::new(Self {
                password,
                store,
                asked: AtomicUsize::new(0),
                last: SyncMutex::new(None),
            })
        }
    }

    impl PasswordPrompter for Typist {
        fn ask(&self, request: PasswordRequest) -> BoxFuture<'static, Option<PasswordAnswer>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(request);
            let answer = PasswordAnswer {
                password: SecretString::from(self.password.to_string()),
                store: self.store,
            };
            Box::pin(async move { Some(answer) })
        }
    }

    struct Dismisser;

    impl PasswordPrompter for Dismisser {
        fn ask(&self, _request: PasswordRequest) -> BoxFuture<'static, Option<PasswordAnswer>> {
            Box::pin(async { None })
        }
    }

    fn cache_with(store: MemoryStore) -> PasswordCache {
        PasswordCache::with_store(Box::new(store))
    }

    #[tokio::test]
    #[cfg(feature = "desktop")]
    async fn the_command_runs_once_and_the_answer_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let counter = dir.path().join("runs");
        let cache = PasswordCache::new();
        let account = account(&format!(
            "sh -c 'echo run >> {} && echo secret'",
            counter.display()
        ));

        for _ in 0..3 {
            let password = cache.imap(&account).await.expect("a password");
            assert_eq!(password.expose_secret(), "secret");
        }
        let runs = std::fs::read_to_string(&counter).expect("the counter file");
        assert_eq!(runs.lines().count(), 1, "the command ran more than once");

        cache.forget_imap("Test").await;
        cache.imap(&account).await.expect("a password");
        let runs = std::fs::read_to_string(&counter).expect("the counter file");
        assert_eq!(
            runs.lines().count(),
            2,
            "forgetting should run the command again"
        );
    }

    #[tokio::test]
    #[cfg(feature = "desktop")]
    async fn forgetting_a_refused_smtp_password_runs_its_command_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let counter = dir.path().join("runs");
        let cache = PasswordCache::new();
        let mut account = account("echo imap");
        account.smtp_password_command = Some(format!(
            "sh -c 'echo run >> {} && echo smtp'",
            counter.display()
        ));
        let runs = || std::fs::read_to_string(&counter).unwrap().lines().count();

        cache.smtp(&account).await.expect("a password");
        cache.smtp(&account).await.expect("a password");
        assert_eq!(runs(), 1);

        cache.forget_smtp(&account).await;
        let password = cache.smtp(&account).await.expect("a password");
        assert_eq!(password.expose_secret(), "smtp");
        assert_eq!(runs(), 2, "forgetting should run the SMTP command again");
    }

    #[tokio::test]
    #[cfg(feature = "desktop")]
    async fn a_failing_command_is_not_cached() {
        let cache = PasswordCache::new();
        let failing = account("false");
        assert!(cache.imap(&failing).await.is_err());
        // Nothing was stored, so a working command for the same account runs.
        let working = account("echo later");
        assert_eq!(
            cache
                .imap(&working)
                .await
                .expect("a password")
                .expose_secret(),
            "later"
        );
    }

    #[tokio::test]
    async fn without_a_command_the_keychain_answers_before_anyone_is_asked() {
        let store = MemoryStore::default();
        store
            .set(
                "user@imap.example.com",
                &SecretString::from("kept".to_string()),
            )
            .unwrap();
        let cache = cache_with(store);
        let typist = Typist::new("typed", false);
        cache.set_prompter(typist.clone());

        let account = account_without_command("");
        assert_eq!(cache.imap(&account).await.unwrap().expose_secret(), "kept");
        assert_eq!(typist.asked.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_prompt_answers_once_and_may_file_the_answer() {
        let cache = cache_with(MemoryStore::default());
        let typist = Typist::new("typed", true);
        cache.set_prompter(typist.clone());

        let account = account_without_command("");
        for _ in 0..2 {
            assert_eq!(cache.imap(&account).await.unwrap().expose_secret(), "typed");
        }
        assert_eq!(typist.asked.load(Ordering::SeqCst), 1);
        let request = typist.last.lock().unwrap().clone().expect("a request");
        assert_eq!(request.account, "Test");
        assert_eq!(request.kind, "IMAP");
        assert_eq!(request.username, "user");
        assert_eq!(request.server, "imap.example.com");
        assert!(!request.retry);
        assert_eq!(
            cache
                .store
                .get("user@imap.example.com")
                .unwrap()
                .expose_secret(),
            "typed"
        );
    }

    #[tokio::test]
    async fn a_refused_password_is_asked_for_again_rather_than_read_back() {
        let store = MemoryStore::default();
        store
            .set(
                "user@imap.example.com",
                &SecretString::from("stale".to_string()),
            )
            .unwrap();
        let cache = cache_with(store);
        let typist = Typist::new("fresh", true);
        cache.set_prompter(typist.clone());

        let account = account_without_command("");
        assert_eq!(cache.imap(&account).await.unwrap().expose_secret(), "stale");
        cache.forget_imap("Test").await;
        assert_eq!(cache.imap(&account).await.unwrap().expose_secret(), "fresh");
        let request = typist.last.lock().unwrap().clone().expect("a request");
        assert!(
            request.retry,
            "the prompt should say the last one was refused"
        );
        assert_eq!(
            cache
                .store
                .get("user@imap.example.com")
                .unwrap()
                .expose_secret(),
            "fresh",
            "the Keychain item is replaced"
        );
    }

    #[tokio::test]
    async fn smtp_shares_the_imap_password_unless_it_is_another_login() {
        let cache = cache_with(MemoryStore::default());
        let typist = Typist::new("typed", false);
        cache.set_prompter(typist.clone());

        let same = account_without_command("");
        cache.imap(&same).await.unwrap();
        assert_eq!(cache.smtp(&same).await.unwrap().expose_secret(), "typed");
        assert_eq!(
            typist.asked.load(Ordering::SeqCst),
            1,
            "one login, one prompt"
        );

        let other = account_without_command("smtp_server: smtp.example.com\n");
        cache.smtp(&other).await.unwrap();
        assert_eq!(typist.asked.load(Ordering::SeqCst), 2);
        let request = typist.last.lock().unwrap().clone().expect("a request");
        assert_eq!(request.kind, "SMTP");
        assert_eq!(request.server, "smtp.example.com");
    }

    #[tokio::test]
    async fn a_dismissed_prompt_and_a_missing_prompter_are_errors_that_name_the_key() {
        let account = account_without_command("");

        let cache = cache_with(MemoryStore::default());
        let error = cache.imap(&account).await.expect_err("no prompter");
        assert!(
            error
                .to_string()
                .contains("accounts[Test].imap_password_command"),
            "{error}"
        );

        cache.set_prompter(Arc::new(Dismisser));
        let error = cache.imap(&account).await.expect_err("dismissed");
        assert!(
            error.to_string().contains("no password was entered"),
            "{error}"
        );
    }
}
