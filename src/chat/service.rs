//! The chat service: keeps the database in step with the server and carries
//! out what the user does.
//!
//! One loop per Orb process. It finds a session (the Keychain, or
//! `token_command`), syncs everything that changed since the last pass, then
//! waits: the realtime stream names the rooms to sync next, a timer runs a
//! full pass as a safety net, and waking from sleep or moving to another
//! network reconnects. Several Orbs on one machine share the database; a pass
//! takes the `sync_locks` row of the server so that only one writes at a
//! time, and every Orb tells its window when the database's chat version
//! moved, whoever moved it.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::client::{RestClient, Session};
use super::live::{self, LiveEvent};
use super::store::{self, ChatRoom, Conversation, WaitingThread};
use super::{ChatError, keychain_key};
use crate::config::{ChatConfig, ChatSignIn};
use crate::db::Database;
use crate::keychain::PasswordStore;

/// How chat stands, for the status line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChatStatus {
    /// Looking for a session or making the first pass.
    Connecting,
    /// There is no session, or the server ended it.
    SignedOut,
    /// Synced, and the realtime stream is up.
    Live,
    /// Synced, but the realtime stream is down: changes arrive by timer.
    Polling { reason: String },
    /// The server cannot be reached at all.
    Offline { reason: String },
}

/// What the service tells the app.
pub trait ChatListener: Send + Sync + 'static {
    /// The chat tables changed: re-read what is on screen.
    fn on_changed(&self);
    /// The status changed.
    fn on_status(&self, status: &ChatStatus);
}

#[derive(Debug)]
enum Trigger {
    Full,
    Room(String),
    SignedIn,
    SignedOut,
    Presence { username: String, status: String },
}

/// A full pass this often while the stream is up, as a safety net …
const LIVE_PASS_EVERY: Duration = Duration::from_secs(300);
/// … and this often while it is down.
const POLL_PASS_EVERY: Duration = Duration::from_secs(60);
/// How long the stream waits before connecting again, doubling to the cap.
const RECONNECT_FIRST: Duration = Duration::from_secs(2);
const RECONNECT_CAP: Duration = Duration::from_secs(120);
/// How long room events are gathered before they are synced together.
const ROOM_SETTLE: Duration = Duration::from_millis(250);
/// The first sync reaches back this far at least, so a conversation has its
/// context even when Waiting looks back less.
const HISTORY_DAYS: i64 = 30;
/// Clock skew allowed between Orb and the server when asking what changed.
const SKEW_MS: i64 = 10_000;
const DAY_MS: i64 = 86_400_000;
/// The sync lock's account; the folder is the server.
const LOCK_ACCOUNT: &str = "chat";
/// A conversation opened asks for its room to be checked at most this often.
const ROOM_CHECK_EVERY: Duration = Duration::from_secs(60);
/// How many first messages of threads, older than the sync's reach, one pass
/// fetches.
const ROOTS_PER_PASS: usize = 50;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub struct ChatService {
    config: RwLock<ChatConfig>,
    db_path: String,
    keychain: Arc<dyn PasswordStore>,
    listener: Arc<dyn ChatListener>,
    triggers: mpsc::UnboundedSender<Trigger>,
    client: RwLock<Option<RestClient>>,
    me: RwLock<Option<(String, String)>>,
    status: Mutex<ChatStatus>,
    seen_version: Mutex<i64>,
    /// Everyone's status by username, from the server and the stream; not
    /// kept in the database, since it is only ever true now.
    presence: RwLock<HashMap<String, String>>,
    /// When a conversation last asked for its room to be checked.
    rooms_checked: Mutex<HashMap<String, std::time::Instant>>,
    shutdown: CancellationToken,
}

impl ChatService {
    /// Start the service on the current tokio runtime.
    pub fn start(
        config: ChatConfig,
        db_path: String,
        keychain: Arc<dyn PasswordStore>,
        listener: Arc<dyn ChatListener>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let (triggers, rx) = mpsc::unbounded_channel();
        let service = Arc::new(Self {
            config: RwLock::new(config),
            db_path,
            keychain,
            listener,
            triggers,
            client: RwLock::new(None),
            me: RwLock::new(None),
            status: Mutex::new(ChatStatus::Connecting),
            seen_version: Mutex::new(-1),
            presence: RwLock::new(HashMap::new()),
            rooms_checked: Mutex::new(HashMap::new()),
            shutdown,
        });
        tokio::spawn(Arc::clone(&service).run(rx));
        service
    }

    pub fn config(&self) -> ChatConfig {
        self.config.read().expect("chat config").clone()
    }

    /// Follow a reloaded config: the rooms' settings take effect at once.
    pub fn update_config(&self, config: ChatConfig) {
        *self.config.write().expect("chat config") = config;
        self.listener.on_changed();
    }

    pub fn status(&self) -> ChatStatus {
        self.status.lock().expect("chat status").clone()
    }

    fn set_status(&self, status: ChatStatus) {
        let mut current = self.status.lock().expect("chat status");
        if *current != status {
            *current = status.clone();
            drop(current);
            crate::logger::info(&format!("chat: {status:?}"));
            self.listener.on_status(&status);
        }
    }

    fn db(&self) -> Result<Database, ChatError> {
        Ok(Database::new(&self.db_path)?)
    }

    fn client(&self) -> Result<RestClient, ChatError> {
        self.client
            .read()
            .expect("chat client")
            .clone()
            .ok_or(ChatError::SignedOut)
    }

    fn me(&self) -> Result<(String, String), ChatError> {
        self.me
            .read()
            .expect("chat me")
            .clone()
            .ok_or(ChatError::SignedOut)
    }

    /// Tell the window if the chat tables changed since it was last told,
    /// whichever Orb changed them.
    fn notify_if_changed(&self) {
        let Ok(db) = self.db() else { return };
        let Ok(version) = store::version(&db.conn) else {
            return;
        };
        let mut seen = self.seen_version.lock().expect("chat version");
        if *seen != version {
            *seen = version;
            drop(seen);
            self.listener.on_changed();
        }
    }

    fn changed(&self, db: &Database) -> Result<(), ChatError> {
        store::bump_version(&db.conn)?;
        *self.seen_version.lock().expect("chat version") = store::version(&db.conn)?;
        self.listener.on_changed();
        Ok(())
    }

    /// Ask the server whether it has threads, and keep the answer for when it
    /// cannot be asked (chat-spec §4).
    async fn learn_threads(&self, client: &RestClient) {
        let enabled = match client.threads_enabled().await {
            Ok(enabled) => enabled,
            Err(e) => {
                crate::logger::warn(&format!("chat: whether threads are on: {e}"));
                return;
            }
        };
        let write = || -> Result<(), ChatError> {
            let db = self.db()?;
            let value = if enabled { "1" } else { "0" };
            if store::state(&db.conn, store::STATE_THREADS)?.as_deref() != Some(value) {
                store::set_state(&db.conn, store::STATE_THREADS, value)?;
                self.changed(&db)?;
            }
            Ok(())
        };
        if let Err(e) = write() {
            crate::logger::warn(&format!("chat: {e}"));
        }
    }

    /// Whether the server has threads turned on; until it has said, it has.
    pub fn threads(&self) -> bool {
        self.db()
            .and_then(|db| Ok(store::state(&db.conn, store::STATE_THREADS)?))
            .map_or(true, |v| v.as_deref() != Some("0"))
    }

    /// Wait until the user signs in again; `false` on shutdown.
    async fn wait_for_sign_in(&self, rx: &mut mpsc::UnboundedReceiver<Trigger>) -> bool {
        *self.client.write().expect("chat client") = None;
        self.set_status(ChatStatus::SignedOut);
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return false,
                trigger = rx.recv() => match trigger {
                    Some(Trigger::SignedIn) => return true,
                    Some(_) => {}
                    None => return false,
                },
            }
        }
    }

    /// The server ended the session: forget it, and wait for a new one.
    async fn signed_out(&self, rx: &mut mpsc::UnboundedReceiver<Trigger>) -> bool {
        crate::logger::warn("chat: the server no longer accepts the session");
        let config = self.config();
        if config.sign_in == ChatSignIn::Sso {
            let _ = self.keychain.delete(&keychain_key(&config.server));
        }
        self.wait_for_sign_in(rx).await
    }

    // ----- The session -----

    async fn find_session(&self) -> Option<Session> {
        let config = self.config();
        match config.sign_in {
            ChatSignIn::Sso => {
                let secret = self.keychain.get(&keychain_key(&config.server))?;
                let mut words = secret.expose_secret().split_whitespace();
                let (user, token) = (words.next()?, words.next()?);
                Some(Session {
                    user_id: user.to_string(),
                    token: SecretString::from(token.to_string()),
                })
            }
            ChatSignIn::TokenCommand => {
                let result = tokio::task::spawn_blocking(move || config.run_token_command()).await;
                match result {
                    Ok(Ok((user_id, token))) => Some(Session { user_id, token }),
                    Ok(Err(problem)) => {
                        crate::logger::warn(&format!("chat: {problem}"));
                        None
                    }
                    Err(_) => None,
                }
            }
        }
    }

    /// Take a session the sign-in window caught: check it with the server,
    /// keep it, and start syncing with it.
    pub async fn sign_in(&self, user_id: String, token: SecretString) -> Result<(), ChatError> {
        let config = self.config();
        let session = Session { user_id, token };
        RestClient::new(&config.server, session.clone())?
            .me()
            .await?;
        let secret = SecretString::from(format!(
            "{} {}",
            session.user_id,
            session.token.expose_secret()
        ));
        self.keychain
            .set(&keychain_key(&config.server), &secret)
            .map_err(ChatError::Database)?;
        let _ = self.triggers.send(Trigger::SignedIn);
        Ok(())
    }

    /// Forget the session, here and in the Keychain.
    pub fn sign_out(&self) {
        let config = self.config();
        if config.sign_in == ChatSignIn::Sso {
            let _ = self.keychain.delete(&keychain_key(&config.server));
        }
        let _ = self.triggers.send(Trigger::SignedOut);
    }

    /// Run a full pass now, as `sync` does for mail.
    pub fn sync_now(&self) {
        let _ = self.triggers.send(Trigger::Full);
    }

    // ----- The loop -----

    async fn run(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Trigger>) {
        let (network_tx, mut network_rx) = mpsc::unbounded_channel();
        tokio::spawn(crate::services::network::watch(
            network_tx,
            self.shutdown.clone(),
        ));
        let mut wake = WakeDetector::new();
        let mut ticks = tokio::time::interval(Duration::from_secs(15));
        let mut offline_wait = RECONNECT_FIRST;
        let mut network_open = true;

        'session: loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            self.set_status(ChatStatus::Connecting);
            let Some(session) = self.find_session().await else {
                if self.wait_for_sign_in(&mut rx).await {
                    continue 'session;
                }
                return;
            };

            let server = self.config().server;
            let client = match RestClient::new(&server, session.clone()) {
                Ok(client) => client,
                Err(e) => {
                    self.set_status(ChatStatus::Offline {
                        reason: e.to_string(),
                    });
                    return;
                }
            };
            match client.me().await {
                Ok(me) => {
                    if let Err(e) = self.adopt(&server, &me.id, &me.username) {
                        crate::logger::warn(&format!("chat: {e}"));
                    }
                    *self.me.write().expect("chat me") = Some((me.id, me.username));
                    self.learn_threads(&client).await;
                    *self.client.write().expect("chat client") = Some(client);
                    offline_wait = RECONNECT_FIRST;
                }
                Err(ChatError::SignedOut) => {
                    if self.signed_out(&mut rx).await {
                        continue 'session;
                    }
                    return;
                }
                Err(e) => {
                    self.set_status(ChatStatus::Offline {
                        reason: e.to_string(),
                    });
                    tokio::select! {
                        _ = self.shutdown.cancelled() => return,
                        _ = tokio::time::sleep(offline_wait) => {}
                        changed = network_rx.recv(), if network_open => network_open = changed.is_some(),
                        trigger = rx.recv() => if trigger.is_none() { return },
                    }
                    offline_wait = (offline_wait * 2).min(RECONNECT_CAP);
                    continue 'session;
                }
            }

            match self.pass(None).await {
                Ok(()) => {}
                Err(ChatError::SignedOut) => {
                    if self.signed_out(&mut rx).await {
                        continue 'session;
                    }
                    return;
                }
                Err(e) => crate::logger::warn(&format!("chat: first pass: {e}")),
            }
            self.load_presence().await;
            self.notify_if_changed();
            self.set_status(ChatStatus::Polling {
                reason: "connecting".into(),
            });

            // The stream runs beside the loop, for as long as the session does.
            let stream_stop = self.shutdown.child_token();
            let (live_tx, mut live_rx) = mpsc::unbounded_channel();
            tokio::spawn(stream(
                server.clone(),
                session.clone(),
                live_tx,
                self.triggers.clone(),
                stream_stop.clone(),
            ));
            let mut last_pass = tokio::time::Instant::now();
            // A channel whose sender is gone answers `None` at once, every
            // time; polled again it would spin the loop.
            let mut stream_open = true;

            loop {
                let pass_every = if self.status() == ChatStatus::Live {
                    LIVE_PASS_EVERY
                } else {
                    POLL_PASS_EVERY
                };
                tokio::select! {
                    _ = self.shutdown.cancelled() => return,
                    _ = ticks.tick() => {
                        if wake.tick() {
                            crate::logger::info("chat: woke from sleep; reconnecting");
                            stream_stop.cancel();
                            continue 'session;
                        }
                        if last_pass.elapsed() >= pass_every {
                            last_pass = tokio::time::Instant::now();
                            self.pass_logged(None).await;
                        } else {
                            self.notify_if_changed();
                        }
                    }
                    changed = network_rx.recv(), if network_open => match changed {
                        Some(()) => {
                            crate::logger::info("chat: the network changed; reconnecting");
                            stream_stop.cancel();
                            continue 'session;
                        }
                        None => network_open = false,
                    },
                    event = live_rx.recv(), if stream_open => match event {
                        Some(StreamState::Up) => {
                            self.set_status(ChatStatus::Live);
                            // What happened while the stream was down.
                            self.pass_logged(None).await;
                        }
                        Some(StreamState::Down(reason)) => {
                            self.set_status(ChatStatus::Polling { reason });
                        }
                        None => {
                            stream_open = false;
                            crate::logger::warn("chat: the realtime stream ended");
                        }
                    },
                    trigger = rx.recv() => match trigger {
                        Some(Trigger::Full) => {
                            last_pass = tokio::time::Instant::now();
                            self.pass_logged(None).await;
                        }
                        Some(Trigger::Room(rid)) => {
                            let mut rooms = BTreeSet::from([rid]);
                            tokio::time::sleep(ROOM_SETTLE).await;
                            // Put back what is not a room only after the
                            // queue is drained: sent back at once, it would be
                            // received again by the same loop, forever.
                            let mut others = Vec::new();
                            while let Ok(next) = rx.try_recv() {
                                match next {
                                    Trigger::Room(rid) => { rooms.insert(rid); }
                                    other => others.push(other),
                                }
                            }
                            for other in others {
                                let _ = self.triggers.send(other);
                            }
                            self.pass_logged(Some(rooms)).await;
                        }
                        Some(Trigger::Presence { username, status }) => self.set_presence(username, status),
                        Some(Trigger::SignedIn) => {
                            stream_stop.cancel();
                            continue 'session;
                        }
                        Some(Trigger::SignedOut) => {
                            stream_stop.cancel();
                            if self.signed_out(&mut rx).await {
                                continue 'session;
                            }
                            return;
                        }
                        None => return,
                    },
                }
            }
        }
    }

    async fn pass_logged(&self, rooms: Option<BTreeSet<String>>) {
        match self.pass(rooms).await {
            Ok(()) => {}
            Err(ChatError::SignedOut) => {
                let _ = self.triggers.send(Trigger::SignedOut);
            }
            Err(e) => crate::logger::warn(&format!("chat: sync: {e}")),
        }
        self.notify_if_changed();
    }

    /// Start afresh when the session is of another server or another user
    /// than what the database holds, and set where Waiting starts looking.
    fn adopt(&self, server: &str, user_id: &str, username: &str) -> Result<(), ChatError> {
        let db = self.db()?;
        let conn = &db.conn;
        let same = store::state(conn, store::STATE_SERVER)?.as_deref() == Some(server)
            && store::state(conn, store::STATE_USER_ID)?.as_deref() == Some(user_id);
        if !same {
            store::wipe(conn)?;
            store::set_state(conn, store::STATE_SERVER, server)?;
            store::set_state(conn, store::STATE_USER_ID, user_id)?;
        }
        store::set_state(conn, store::STATE_USERNAME, username)?;
        if store::state_i64(conn, store::STATE_BASELINE)?.is_none() {
            let days = i64::from(self.config().waiting_days);
            store::set_state(
                conn,
                store::STATE_BASELINE,
                &(now_ms() - days * DAY_MS).to_string(),
            )?;
        }
        Ok(())
    }

    /// Sync the subscriptions and rooms and every room behind, or only
    /// `rooms`, under the server's sync lock.
    async fn pass(&self, rooms: Option<BTreeSet<String>>) -> Result<(), ChatError> {
        self.pass_with(rooms, false).await
    }

    /// A full pass that waits for the sync lock rather than leaving the work
    /// to whoever holds it: for an action that needs its result at once.
    async fn pass_now(&self) -> Result<(), ChatError> {
        self.pass_with(None, true).await
    }

    async fn pass_with(
        &self,
        rooms: Option<BTreeSet<String>>,
        wait: bool,
    ) -> Result<(), ChatError> {
        let client = self.client()?;
        let server = client.server().to_string();
        let mut acquired = self
            .db()?
            .acquire_sync_lock(LOCK_ACCOUNT, &server)?
            .acquired();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while wait && !acquired && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            acquired = self
                .db()?
                .acquire_sync_lock(LOCK_ACCOUNT, &server)?
                .acquired();
        }
        if !acquired {
            // Another Orb is at it; its writes reach this window through the
            // version, and this pass is run again shortly.
            let triggers = self.triggers.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let _ = triggers.send(Trigger::Full);
            });
            return Ok(());
        }
        let result = self.pass_locked(&client, rooms).await;
        if let Ok(mut db) = self.db() {
            let _ = db.release_sync_lock(LOCK_ACCOUNT, &server);
        }
        if result? {
            self.changed(&self.db()?)?;
        }
        Ok(())
    }

    async fn pass_locked(
        &self,
        client: &RestClient,
        rooms: Option<BTreeSet<String>>,
    ) -> Result<bool, ChatError> {
        // Owned, and borrowed only between awaits: a connection is not `Sync`.
        let db = self.db()?;
        let (user_id, username) = self.me()?;
        let history_since =
            now_ms() - HISTORY_DAYS.max(i64::from(self.config().waiting_days)) * DAY_MS;
        let mut wrote = false;

        let rooms: Vec<(String, i64)> = match rooms {
            Some(rooms) => {
                let mut out = Vec::new();
                for rid in rooms {
                    match store::synced_until(&db.conn, &rid)? {
                        Some(since) => out.push((rid, since)),
                        // A room Orb does not know yet: the subscriptions say what it is.
                        None => return Box::pin(self.pass_locked(client, None)).await,
                    }
                }
                out
            }
            None => {
                let started = now_ms();
                let since = store::state_i64(&db.conn, store::STATE_SUBSCRIPTIONS_SYNCED)?;
                let subs = client.subscriptions(since).await?;
                for sub in &subs.updated {
                    store::upsert_subscription(&db.conn, sub)?;
                }
                for rid in &subs.removed {
                    store::remove_room(&db.conn, rid)?;
                }
                wrote |= !subs.updated.is_empty() || !subs.removed.is_empty();
                store::set_state(
                    &db.conn,
                    store::STATE_SUBSCRIPTIONS_SYNCED,
                    &(started - SKEW_MS).to_string(),
                )?;

                let since = store::state_i64(&db.conn, store::STATE_ROOMS_SYNCED)?;
                let changed = client.rooms(since).await?;
                for room in &changed.updated {
                    store::upsert_room(&db.conn, room)?;
                }
                wrote |= !changed.updated.is_empty();
                store::set_state(
                    &db.conn,
                    store::STATE_ROOMS_SYNCED,
                    &(started - SKEW_MS).to_string(),
                )?;
                store::rooms_behind(&db.conn, history_since)?
            }
        };

        let mut roots_missing = BTreeSet::new();
        for (rid, since) in rooms {
            // One room the server cannot answer for must not hold up the rest.
            let changes = match client.sync_messages(&rid, since).await {
                Ok(changes) => changes,
                Err(ChatError::SignedOut) => return Err(ChatError::SignedOut),
                Err(e) => {
                    crate::logger::warn(&format!("chat: room {rid}: {e}"));
                    continue;
                }
            };
            let mut until = since;
            for message in &changes.updated {
                store::upsert_message(&db.conn, message, &user_id, &username)?;
                until = until.max(message.updated_at);
                if let Some(tmid) = &message.tmid {
                    roots_missing.insert(tmid.clone());
                }
            }
            for id in &changes.removed {
                store::delete_message(&db.conn, id)?;
            }
            wrote |= !changes.updated.is_empty() || !changes.removed.is_empty();
            store::set_synced_until(&db.conn, &rid, until)?;
        }

        for tmid in roots_missing.into_iter().take(ROOTS_PER_PASS * 4) {
            if store::has_message(&db.conn, &tmid)? {
                continue;
            }
            match client.message(&tmid).await {
                Ok(root) => {
                    store::upsert_message(&db.conn, &root, &user_id, &username)?;
                    wrote = true;
                }
                Err(ChatError::SignedOut) => return Err(ChatError::SignedOut),
                Err(e) => crate::logger::debug(&format!("chat: thread start {tmid}: {e}")),
            }
        }
        Ok(wrote)
    }

    // ----- What the app reads -----

    fn waiting_room(config: &ChatConfig) -> impl Fn(&ChatRoom) -> bool + '_ {
        move |room: &ChatRoom| {
            room.name
                .as_deref()
                .is_none_or(|name| config.room(name).waiting)
        }
    }

    /// The threads that wait, newest first.
    pub fn waiting(&self) -> Result<Vec<WaitingThread>, ChatError> {
        let Ok((user_id, _)) = self.me().or_else(|_| self.stored_me()) else {
            return Ok(Vec::new());
        };
        let db = self.db()?;
        let baseline = store::state_i64(&db.conn, store::STATE_BASELINE)?.unwrap_or(0);
        let config = self.config();
        Ok(store::waiting(
            &db.conn,
            &user_id,
            baseline,
            &Self::waiting_room(&config),
        )?)
    }

    /// The threads that started waiting after `after` (ms) and may notify, and
    /// the watermark to pass next time: the room's `notify` in the config
    /// and the server's own preference for it both have their say. A muted
    /// room's thread stays quiet until a message names the user or calls the
    /// room. The watermark passes the quiet ones too, so a mute that ends
    /// announces nothing of what came while it held.
    pub fn new_waiting_since(&self, after: i64) -> Result<(Vec<WaitingThread>, i64), ChatError> {
        // A muted room's thread is announced when something calls it.
        let since = |row: &WaitingThread| {
            if row.room.muted {
                row.called_since
            } else {
                Some(row.waiting_since)
            }
        };
        let rows = self.waiting()?;
        let watermark = rows
            .iter()
            .flat_map(|r| [Some(r.waiting_since), since(r)])
            .flatten()
            .fold(after, i64::max);
        let db = self.db()?;
        let config = self.config();
        let mut out = Vec::new();
        for row in rows {
            if since(&row).is_none_or(|at| at <= after) {
                continue;
            }
            let configured = row
                .room
                .name
                .as_deref()
                .is_none_or(|name| config.room(name).notify);
            let server = store::room_notifies(&db.conn, &row.rid)?;
            let allowed = match server.as_str() {
                store::NOTIFY_NOTHING => false,
                store::NOTIFY_MENTIONS => row.mention || row.room.kind == "d",
                _ => true,
            };
            if configured && allowed {
                out.push(row);
            }
        }
        out.sort_by_key(|r| since(r).unwrap_or(r.waiting_since));
        Ok((out, watermark))
    }

    /// The newest a thread started waiting, to count from at launch.
    pub fn waiting_watermark(&self) -> i64 {
        self.waiting()
            .map(|rows| rows.iter().map(|r| r.waiting_since).max().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Who the database was last synced for, while the server cannot say.
    fn stored_me(&self) -> Result<(String, String), ChatError> {
        let db = self.db()?;
        let user = store::state(&db.conn, store::STATE_USER_ID)?;
        let name = store::state(&db.conn, store::STATE_USERNAME)?;
        match (user, name) {
            (Some(user), Some(name)) => Ok((user, name)),
            _ => Err(ChatError::SignedOut),
        }
    }

    /// The rooms the user is in, a direct message with one person carrying
    /// that person's status.
    pub fn rooms(&self) -> Result<Vec<ChatRoom>, ChatError> {
        let mut rooms = store::rooms(&self.db()?.conn)?;
        let presence = self.presence.read().expect("chat presence");
        for room in &mut rooms {
            if room.kind == "d" && room.label == "Direct" {
                room.presence = room
                    .name
                    .as_ref()
                    .and_then(|name| presence.get(name).cloned());
            }
        }
        Ok(rooms)
    }

    /// Everyone's status, as the server has it now.
    async fn load_presence(&self) {
        let Ok(client) = self.client() else { return };
        match client.presence().await {
            Ok(all) => *self.presence.write().expect("chat presence") = all.into_iter().collect(),
            Err(e) => crate::logger::info(&format!("chat: presence: {e}")),
        }
    }

    /// Someone's status changed; the window hears of it when it is someone
    /// the user has direct messages with.
    fn set_presence(&self, username: String, status: String) {
        let changed = {
            let mut presence = self.presence.write().expect("chat presence");
            presence.insert(username.clone(), status.clone()) != Some(status)
        };
        let partner = self.db().is_ok_and(|db| {
            store::rooms(&db.conn).is_ok_and(|rooms| {
                rooms
                    .iter()
                    .any(|r| r.kind == "d" && r.name.as_deref() == Some(username.as_str()))
            })
        });
        if changed && partner {
            self.listener.on_changed();
        }
    }

    /// Leave a room, or hide a direct message, and forget it here.
    ///
    /// Returns the room's kind, which says whether it can be come back to:
    /// see [`ChatService::rejoin`].
    pub async fn leave(&self, rid: &str) -> Result<String, ChatError> {
        let room = store::room(&self.db()?.conn, rid)?
            .ok_or_else(|| ChatError::Database(format!("no room {rid}")))?;
        self.client()?.leave(&room.kind, rid).await?;
        let db = self.db()?;
        store::remove_room(&db.conn, rid)?;
        self.changed(&db)?;
        Ok(room.kind)
    }

    /// Come back to a room left with [`ChatService::leave`], and sync it: join
    /// a channel again, or show a hidden direct message. A private group
    /// takes an invitation, which is not the user's to give themselves.
    pub async fn rejoin(&self, rid: &str, kind: &str) -> Result<(), ChatError> {
        match kind {
            "c" => self.client()?.join(rid).await?,
            "p" => {
                return Err(ChatError::Server(
                    "a private group takes an invitation to come back to".into(),
                ));
            }
            _ => self.client()?.reopen_direct(rid).await?,
        }
        self.pass_now().await
    }

    /// Create a channel, or a private group, and sync it; returns its id.
    pub async fn create_room(&self, name: &str, private: bool) -> Result<String, ChatError> {
        let rid = self.client()?.create_room(name, private).await?;
        self.pass_now().await?;
        Ok(rid)
    }

    /// A thread for the reader. Its first message and replies older than the
    /// sync's reach are fetched first.
    pub async fn conversation(
        &self,
        rid: &str,
        thread: &str,
    ) -> Result<Option<Conversation>, ChatError> {
        let (user_id, username) = self.me().or_else(|_| self.stored_me())?;
        self.check_room(rid);
        if !thread.is_empty()
            && let Ok(client) = self.client()
        {
            let db = self.db()?;
            let have: i64 = db.conn.query_row(
                "SELECT COUNT(*) FROM chat_messages WHERE tmid = ?1",
                [thread],
                |row| row.get(0),
            )?;
            let expected: Option<i64> = db
                .conn
                .query_row(
                    "SELECT tcount FROM chat_messages WHERE id = ?1",
                    [thread],
                    |row| row.get(0),
                )
                .ok();
            if expected.is_none_or(|n| n > have) {
                if expected.is_none() {
                    let root = client.message(thread).await?;
                    store::upsert_message(&db.conn, &root, &user_id, &username)?;
                }
                for message in client.thread_messages(thread).await? {
                    store::upsert_message(&db.conn, &message, &user_id, &username)?;
                }
            }
        }
        let db = self.db()?;
        let waiting = self
            .waiting()?
            .into_iter()
            .find(|w| w.rid == rid && w.thread == thread);
        Ok(store::conversation(
            &db.conn, rid, thread, &user_id, &username, waiting,
        )?)
    }

    /// Ask the server what changed in a room, whose newest message may be
    /// what Orb holds: a reaction or an edit made while the stream was down
    /// leaves the room looking synced (chat-spec §8).
    fn check_room(&self, rid: &str) {
        let mut checked = self.rooms_checked.lock().expect("chat rooms checked");
        let now = std::time::Instant::now();
        if checked
            .get(rid)
            .is_some_and(|at| now.duration_since(*at) < ROOM_CHECK_EVERY)
        {
            return;
        }
        checked.insert(rid.to_string(), now);
        let _ = self.triggers.send(Trigger::Room(rid.to_string()));
    }

    /// The page of messages before `before`: from the database, and for a
    /// room's timeline from the server once the database has run out.
    pub async fn older(
        &self,
        rid: &str,
        thread: &str,
        before: i64,
    ) -> Result<store::OlderMessages, ChatError> {
        let (user_id, username) = self.me().or_else(|_| self.stored_me())?;
        let page = |db: &Database| {
            store::messages(
                &db.conn,
                rid,
                thread,
                &user_id,
                &username,
                Some(before),
                store::OLDER_PAGE,
            )
        };
        let db = self.db()?;
        let mut messages = page(&db)?;
        let short = (messages.len() as i64) < store::OLDER_PAGE;
        if short && thread.is_empty() && !store::history_complete(&db.conn, rid)? {
            let oldest = messages.first().map_or(before, |m| m.ts);
            if let (Ok(client), Some(room)) = (self.client(), store::room(&db.conn, rid)?) {
                let fetched = client
                    .history(&room.kind, rid, oldest, store::OLDER_PAGE)
                    .await?;
                for message in &fetched {
                    store::upsert_message(&db.conn, message, &user_id, &username)?;
                }
                if (fetched.len() as i64) < store::OLDER_PAGE {
                    store::set_history_complete(&db.conn, rid)?;
                }
                messages = page(&db)?;
            }
        }
        let has_older = store::reaches_back(&db.conn, rid, thread, messages.first().map(|m| m.ts))?;
        Ok(store::OlderMessages {
            messages,
            has_older,
        })
    }

    /// The threads of a room, its own timeline first.
    pub fn room_threads(&self, rid: &str) -> Result<Vec<WaitingThread>, ChatError> {
        let (user_id, _) = self.me().or_else(|_| self.stored_me())?;
        let db = self.db()?;
        let baseline = store::state_i64(&db.conn, store::STATE_BASELINE)?.unwrap_or(0);
        let config = self.config();
        Ok(store::room_threads(
            &db.conn,
            &user_id,
            rid,
            baseline,
            &Self::waiting_room(&config),
        )?)
    }

    /// What the room picker offers for `term`: the user's rooms matching it,
    /// then people and public rooms the server finds.
    pub async fn find(&self, term: &str) -> Result<Vec<store::ChatPick>, ChatError> {
        let term = term.trim();
        let needle = term.to_lowercase();
        let rooms = store::rooms(&self.db()?.conn)?;
        let matches = |text: Option<&str>| text.is_some_and(|t| t.to_lowercase().contains(&needle));
        let mut picks: Vec<store::ChatPick> = rooms
            .iter()
            .filter(|r| {
                needle.is_empty() || matches(r.name.as_deref()) || matches(r.fname.as_deref())
            })
            .map(|r| store::ChatPick {
                kind: "room".into(),
                rid: Some(r.rid.clone()),
                username: (r.kind == "d").then(|| r.name.clone()).flatten(),
                label: if r.kind == "d" {
                    r.fname
                        .clone()
                        .or(r.name.clone())
                        .unwrap_or_else(|| r.label.clone())
                } else {
                    r.label.clone()
                },
                detail: if r.kind == "d" {
                    "direct".into()
                } else {
                    String::new()
                },
            })
            .collect();
        if needle.chars().count() < 2 {
            return Ok(picks);
        }
        let Ok(client) = self.client() else {
            return Ok(picks);
        };
        let found = client.spotlight(term).await?;
        let (me, _) = self.me()?;
        for user in found.users {
            if user.id == me
                || picks
                    .iter()
                    .any(|p| p.username.as_deref() == Some(&user.username))
            {
                continue;
            }
            picks.push(store::ChatPick {
                kind: "person".into(),
                rid: None,
                label: user
                    .name
                    .clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or(user.username.clone()),
                detail: format!("@{} · new message", user.username),
                username: Some(user.username),
            });
        }
        for room in found.rooms {
            if rooms.iter().any(|r| r.rid == room.id) || room.t.as_deref() != Some("c") {
                continue;
            }
            picks.push(store::ChatPick {
                kind: "join".into(),
                rid: Some(room.id),
                username: None,
                label: format!("#{}", room.name.or(room.fname).unwrap_or_default()),
                detail: "join".into(),
            });
        }
        Ok(picks)
    }

    /// The direct message room with `username`, made if need be, and synced
    /// so that it can be opened at once.
    pub async fn open_direct(&self, username: &str) -> Result<String, ChatError> {
        let rid = self.client()?.open_direct(username).await?;
        self.pass_now().await?;
        Ok(rid)
    }

    /// The conversation with several people at once, made if there is none
    /// yet, and synced; with one person, the direct message with them.
    pub async fn open_group(&self, usernames: &[String]) -> Result<String, ChatError> {
        let rid = match usernames {
            [one] => self.client()?.open_direct(one).await?,
            _ => self.client()?.open_group(usernames).await?,
        };
        self.pass_now().await?;
        Ok(rid)
    }

    /// Join a public channel, and sync it.
    pub async fn join(&self, rid: &str) -> Result<(), ChatError> {
        self.client()?.join(rid).await?;
        self.pass_now().await
    }

    // ----- What the user does -----

    /// The user has seen the thread up to its newest message.
    pub fn mark_seen(&self, rid: &str, thread: &str) -> Result<(), ChatError> {
        let db = self.db()?;
        if let Some(at) = store::newest_at(&db.conn, rid, thread)? {
            store::set_seen(&db.conn, rid, thread, at)?;
            self.changed(&db)?;
        }
        Ok(())
    }

    /// Mark a thread read on the server: the room's timeline as the room, a
    /// thread as itself.
    async fn mark_read_on_server(&self, client: &RestClient, rid: &str, thread: &str) {
        let result = if thread.is_empty() {
            client.mark_read(rid).await
        } else {
            client.mark_thread_read(thread).await
        };
        if let Err(e) = result {
            crate::logger::warn(&format!("chat: could not mark {rid}/{thread} read: {e}"));
        }
    }

    /// Take a thread out of Waiting (`done` or `task`), and mark it read on
    /// the server. Returns the mark it replaced, for undo. The other Macs
    /// hear of it through the tasks account ([`super::marks`]).
    pub async fn mark(
        &self,
        rid: &str,
        thread: &str,
        kind: &str,
    ) -> Result<Option<(String, i64)>, ChatError> {
        let previous = {
            let db = self.db()?;
            let at = store::newest_at(&db.conn, rid, thread)?.unwrap_or_else(now_ms);
            let previous = store::mark(&db.conn, rid, thread)?;
            store::set_mark(&db.conn, rid, thread, kind, at)?;
            store::set_seen(&db.conn, rid, thread, at)?;
            super::marks::touch(&db.conn)?;
            self.changed(&db)?;
            previous
        };
        if let Ok(client) = self.client() {
            self.mark_read_on_server(&client, rid, thread).await;
        }
        Ok(previous)
    }

    /// Put back the mark a [`ChatService::mark`] replaced. Returns the mark it
    /// replaced in turn, so that putting it back can be undone too.
    pub fn restore_mark(
        &self,
        rid: &str,
        thread: &str,
        previous: Option<(String, i64)>,
    ) -> Result<Option<(String, i64)>, ChatError> {
        let db = self.db()?;
        let current = store::mark(&db.conn, rid, thread)?;
        match previous {
            Some((kind, at)) => store::set_mark(&db.conn, rid, thread, &kind, at)?,
            None => store::remove_mark(&db.conn, rid, thread)?,
        }
        super::marks::touch(&db.conn)?;
        self.changed(&db)?;
        Ok(current)
    }

    /// Mute a room until `ends` (ms), or until the user unmutes it (`None`)
    /// (chat-spec §13). Returns the mark it replaced, for undo, which
    /// [`ChatService::restore_mark`] on [`store::MUTE_THREAD`] puts back.
    pub fn mute(&self, rid: &str, ends: Option<i64>) -> Result<Option<(String, i64)>, ChatError> {
        let ends = ends.unwrap_or(store::MUTE_FOREVER);
        self.restore_mark(
            rid,
            store::MUTE_THREAD,
            Some((store::MARK_MUTE.into(), ends)),
        )
    }

    /// End a room's mute. Returns the mark it took away, for undo.
    pub fn unmute(&self, rid: &str) -> Result<Option<(String, i64)>, ChatError> {
        self.restore_mark(rid, store::MUTE_THREAD, None)
    }

    /// Send `text` into a thread (or the room's timeline), and mark the thread
    /// read on the server: the user has answered it.
    pub async fn send(
        &self,
        rid: &str,
        thread: &str,
        text: &str,
        tshow: bool,
    ) -> Result<(), ChatError> {
        let client = self.client()?;
        let (user_id, username) = self.me()?;
        let tmid = (!thread.is_empty()).then_some(thread);
        let sent = client.send(rid, tmid, text, tshow).await?;
        {
            let db = self.db()?;
            store::upsert_message(&db.conn, &sent, &user_id, &username)?;
            store::set_seen(&db.conn, rid, thread, sent.ts)?;
            self.changed(&db)?;
        }
        self.mark_read_on_server(&client, rid, thread).await;
        Ok(())
    }

    /// Post files to a thread (or the room's timeline), `text` as the first
    /// one's message, and mark the thread read on the server.
    pub async fn send_files(
        &self,
        rid: &str,
        thread: &str,
        files: Vec<ChatUpload>,
        text: &str,
    ) -> Result<(), ChatError> {
        let client = self.client()?;
        let (user_id, username) = self.me()?;
        let tmid = (!thread.is_empty()).then_some(thread);
        let mut message = text;
        for file in files {
            let sent = client
                .upload(rid, tmid, &file.name, &file.mime, file.bytes, message)
                .await?;
            message = "";
            let db = self.db()?;
            store::upsert_message(&db.conn, &sent, &user_id, &username)?;
            store::set_seen(&db.conn, rid, thread, sent.ts)?;
            self.changed(&db)?;
        }
        self.mark_read_on_server(&client, rid, thread).await;
        Ok(())
    }

    /// Replace the text of one of the user's messages.
    pub async fn edit(&self, rid: &str, id: &str, text: &str) -> Result<(), ChatError> {
        self.client()?.update_message(rid, id, text).await?;
        self.sync_room_now(rid).await
    }

    /// Delete one of the user's messages.
    pub async fn delete(&self, rid: &str, id: &str) -> Result<(), ChatError> {
        self.client()?.delete_message(rid, id).await?;
        let db = self.db()?;
        store::delete_message(&db.conn, id)?;
        self.changed(&db)
    }

    /// Mark a thread read or unread, in Orb and on the server. Unread starts
    /// at the thread's newest message.
    pub async fn set_read(&self, rid: &str, thread: &str, read: bool) -> Result<(), ChatError> {
        let newest = {
            let db = self.db()?;
            if read {
                if let Some(at) = store::newest_at(&db.conn, rid, thread)? {
                    store::set_seen(&db.conn, rid, thread, at)?;
                }
            } else {
                store::unsee(&db.conn, rid, thread)?;
            }
            self.changed(&db)?;
            store::newest_id(&db.conn, rid, thread)?
        };
        let client = self.client()?;
        if read {
            self.mark_read_on_server(&client, rid, thread).await;
        } else if let Some(id) = newest {
            client.mark_unread(&id).await?;
        }
        Ok(())
    }

    /// A message's file, fetched from the server: its name and its bytes.
    pub async fn file(&self, id: &str, index: usize) -> Result<(String, Vec<u8>), ChatError> {
        let file = {
            let db = self.db()?;
            store::message_file(&db.conn, id, index)?
        }
        .ok_or_else(|| ChatError::Server(format!("message {id} has no file {index}")))?;
        let path = file.path.ok_or_else(|| {
            ChatError::Server(format!("{}: the server gave no address for it", file.name))
        })?;
        let bytes = self.client()?.download(&path).await?;
        Ok((file.name, bytes))
    }

    /// A message's image preview, fetched from the server: its type and its
    /// bytes, or `None` for a file that has none.
    pub async fn file_preview(
        &self,
        id: &str,
        index: usize,
    ) -> Result<Option<(Option<String>, Vec<u8>)>, ChatError> {
        let file = {
            let db = self.db()?;
            store::message_file(&db.conn, id, index)?
        };
        let Some((path, mime)) = file.and_then(|f| f.preview.map(|p| (p, f.mime))) else {
            return Ok(None);
        };
        let bytes = self.client()?.download(&path).await?;
        Ok(Some((mime, bytes)))
    }

    /// Add or take back the user's reaction to a message.
    pub async fn react(&self, rid: &str, id: &str, emoji: &str, on: bool) -> Result<(), ChatError> {
        self.client()?.react(id, emoji, on).await?;
        self.sync_room_now(rid).await
    }

    /// Sync one room now, waiting for the lock: an edit shows at once.
    async fn sync_room_now(&self, rid: &str) -> Result<(), ChatError> {
        self.pass_with(Some(BTreeSet::from([rid.to_string()])), true)
            .await
    }

    /// A link that opens the thread in Rocket.Chat.
    pub fn permalink(&self, rid: &str, thread: &str) -> Result<String, ChatError> {
        let db = self.db()?;
        let room = store::room(&db.conn, rid)?
            .ok_or_else(|| ChatError::Database(format!("no room {rid}")))?;
        Ok(super::permalink(&self.config().server, &room, thread, None))
    }

    /// The message a task is made from — `message`, or the thread's newest —
    /// with what the task's link keeps of it (todo-spec §28).
    pub fn task_source(
        &self,
        rid: &str,
        thread: &str,
        message: Option<&str>,
    ) -> Result<ChatTaskSource, ChatError> {
        let db = self.db()?;
        let room = store::room(&db.conn, rid)?
            .ok_or_else(|| ChatError::Database(format!("no room {rid}")))?;
        let source = store::task_source(&db.conn, rid, thread, message)?
            .ok_or_else(|| ChatError::Database(format!("no message to make a task of in {rid}")))?;
        let server = &self.config().server;
        Ok(ChatTaskSource {
            link: super::ChatLink::new(server, rid, &source.id, thread).id(),
            permalink: super::permalink(server, &room, thread, Some(&source.id)),
            room: room.label,
            author: source.author,
            text: source.body,
            date: super::client::iso(source.ts),
        })
    }
}

/// A chat message, as a task made from it links it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatTaskSource {
    /// The link's id: [`super::ChatLink::id`].
    pub link: String,
    pub permalink: String,
    pub room: String,
    pub author: String,
    pub text: String,
    /// RFC 3339.
    pub date: String,
}

/// A file to post: its name, its type, and what is in it.
pub struct ChatUpload {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// What the realtime stream reports to the loop.
enum StreamState {
    Up,
    Down(String),
}

/// Keep the realtime stream up until `stop`: connect, report, and after a
/// failure wait and connect again, longer each time.
async fn stream(
    server: String,
    session: Session,
    state: mpsc::UnboundedSender<StreamState>,
    triggers: mpsc::UnboundedSender<Trigger>,
    stop: CancellationToken,
) {
    let mut wait = RECONNECT_FIRST;
    loop {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let forward = {
            let state = state.clone();
            let triggers = triggers.clone();
            tokio::spawn(async move {
                while let Some(event) = events_rx.recv().await {
                    match event {
                        LiveEvent::Connected => drop(state.send(StreamState::Up)),
                        LiveEvent::Room(rid) => drop(triggers.send(Trigger::Room(rid))),
                        LiveEvent::Subscriptions => drop(triggers.send(Trigger::Full)),
                        LiveEvent::Presence { username, status } => {
                            drop(triggers.send(Trigger::Presence { username, status }))
                        }
                    }
                }
            })
        };
        let result = live::run(&server, &session, &events_tx, &stop).await;
        drop(events_tx);
        let _ = forward.await;
        match result {
            Ok(()) => return,
            Err(ChatError::SignedOut) => {
                let _ = triggers.send(Trigger::SignedOut);
                return;
            }
            Err(e) => {
                crate::logger::info(&format!("chat: realtime: {e}"));
                let _ = state.send(StreamState::Down(e.to_string()));
            }
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(RECONNECT_CAP);
    }
}

/// Notices a sleep by the wall clock jumping past the timer.
struct WakeDetector {
    last: SystemTime,
}

impl WakeDetector {
    fn new() -> Self {
        Self {
            last: SystemTime::now(),
        }
    }

    /// Whether more time passed since the last tick than a tick can explain.
    fn tick(&mut self) -> bool {
        let now = SystemTime::now();
        let gap = now.duration_since(self.last).unwrap_or_default();
        self.last = now;
        gap > Duration::from_secs(75)
    }
}
