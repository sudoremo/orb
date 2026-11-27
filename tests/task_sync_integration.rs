//! The reliability suite for task synchronisation (spec §14.1).
//!
//! Tasks live as RFC822 messages in one IMAP folder, and that folder — not the
//! SQLite cache — is the source of truth. IMAP messages are immutable, so every
//! edit is an APPEND followed by a delete, and every interesting failure lives
//! in the gap between those two operations: a crash, a second machine, an edit
//! that lands while the upload is in flight, a server that recycles its UIDs or
//! never advances its MODSEQ. Unit tests can prove the *decisions*
//! (`tasks::reconcile` is pure and thoroughly covered in-file); only a real
//! server can prove the *sequence*, because the sequence is where APPEND, UID
//! SEARCH, UID EXPUNGE and UIDVALIDITY actually behave the way the design
//! assumes they do.
//!
//! Every scenario here is one of the non-negotiable requirements of §14, in the
//! order they are listed in §14.1. Two of them — date arithmetic across DST and
//! the reminder window's 64-notification cap — are deliberately server-free and
//! therefore not `#[ignore]`d: a clock and a timezone are not something Dovecot
//! can tell us about, and those two requirements deserve to run on every
//! `cargo test`.
//!
//! Run the container-backed half with:
//! `cargo test --test task_sync_integration -- --ignored`
//!
//! ## How a second machine is simulated
//!
//! A "device" is a temp directory with its own database and its own IMAP
//! connection, pointed at the same account name and the same server folder.
//! That is exactly what two laptops are, and nothing about the sync code knows
//! the difference.
//!
//! Two scenarios need a message to reach the server *without* the sync engine
//! being allowed to finish its job — a concurrent upload from a device that
//! could not see the other's message yet, and a crash between APPEND and
//! DELETE. Those are produced by [`Device::append_current`], which performs the
//! first half of `TaskSyncService::upload` verbatim (build the message, APPEND
//! it, find it again by `Message-ID`) and then stops. The result on the server
//! is byte-for-byte what a real interrupted upload leaves behind, which is the
//! only thing the next sync can see anyway.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeZone, Utc, Weekday};
use futures::TryStreamExt;
use secrecy::SecretString;

use common::imap_container::DovecotContainer;
use common::test_context::TestContext;

use orb_core::imap::ImapClient;
use orb_core::tasks::{
    CachedEmailMetadata, ChangeContext, FixedZone, IDENTITY_HEADERS, ListedMessage, LocalZone,
    NOTIFICATION_ID_PREFIX, NotificationHost, PENDING_NOTIFICATION_LIMIT, ReminderService,
    ScheduledNotification, Task, TaskAttachment, TaskEmailBuilder, TaskMessageKind, TaskRepository,
    TaskSyncReport, TaskSyncService, TaskUpdates, attachment_cache_root, read_listing,
    set_attachment_cache_root,
};

/// The designated tasks account (§1). Both simulated devices use the same name,
/// because they are the same account seen from two machines.
const ACCOUNT: &str = "tasks@test.local";

/// The folder as it is written in the config. Deliberately without a leading
/// dot: `.Tasks` is a Courier convention and on Dovecot it creates a second,
/// invisible mailbox beside the real one.
const TASKS_FOLDER: &str = "Tasks";

/// The `HIGHESTMODSEQ` a frozen server is pretended to report: the highest
/// value RFC 7162 allows, so nothing the server does can ever exceed it.
const FROZEN_MODSEQ: u64 = i64::MAX as u64;

// ===========================================================================
// The harness
// ===========================================================================

/// One simulated machine: its own cache, its own connection, the same folder.
struct Device {
    /// Used in every panic message — a failure in a two-device test is useless
    /// without knowing which device saw it.
    label: &'static str,
    /// Owns the temp directory the database lives in; dropping it deletes the
    /// cache, which is what makes each device independent.
    _context: TestContext,
    account: String,
    folder: String,
    device_id: String,
    service: TaskSyncService,
    client: ImapClient,
}

impl Device {
    /// Bring a machine online: fresh cache, fresh connection, folder resolved
    /// and created if this is the first device to get there.
    async fn start(container: &DovecotContainer, label: &'static str) -> Self {
        let context = TestContext::new();
        let db = context.with_database();
        db.init_schema()
            .unwrap_or_else(|error| panic!("{label}: schema init failed: {error}"));

        let mut client = connect(container).await;
        let mut service = TaskSyncService::with_database(db, ACCOUNT, TASKS_FOLDER);
        let folder = service
            .resolve_folder(&mut client)
            .await
            .unwrap_or_else(|error| panic!("{label}: the tasks folder is unusable: {error}"));

        let device_id = TaskRepository::new(service.database())
            .ensure_device_id(ACCOUNT)
            .unwrap_or_else(|error| panic!("{label}: no device id: {error}"));

        Self {
            label,
            _context: context,
            account: ACCOUNT.to_string(),
            folder,
            device_id,
            service,
            client,
        }
    }

    /// Replace the connection, as a restart or a reconnect after an outage
    /// does. Needed wherever the server is altered underneath a selected
    /// mailbox: a session whose mailbox was deleted out from under it is not
    /// something a client is expected to keep using.
    async fn reconnect(&mut self, container: &DovecotContainer) {
        self.client = connect(container).await;
        self.service
            .resolve_folder(&mut self.client)
            .await
            .unwrap_or_else(|error| panic!("{}: reconnect failed: {error}", self.label));
    }

    fn repo(&self) -> TaskRepository<'_> {
        TaskRepository::new(self.service.database())
    }

    /// A change context stamped with this machine's device id, so log entries
    /// are attributable to the device that wrote them — which is the whole
    /// point of the union merge in §11.
    fn change_context(&self) -> ChangeContext<'_> {
        self.repo().context_at(Utc::now(), Some(&self.device_id))
    }

    fn create_task(&self, subject: &str) -> Task {
        let ctx = self.change_context();
        self.repo()
            .create_task(subject, &self.account, &ctx)
            .unwrap_or_else(|error| panic!("{}: creating a task failed: {error}", self.label))
    }

    /// Edit a title, which is the cheapest change that bumps a revision, writes
    /// a log entry and marks the task dirty.
    fn edit_subject(&self, task_id: i64, subject: &str) {
        let ctx = self.change_context();
        let outcome = self
            .repo()
            .apply(
                task_id,
                &TaskUpdates::new().with_subject(subject.to_string()),
                &ctx,
            )
            .unwrap_or_else(|error| panic!("{}: editing task {task_id}: {error}", self.label));
        assert!(
            !outcome.is_noop(),
            "{}: editing the title of task {task_id} changed nothing",
            self.label
        );
    }

    fn find_task(&self, task_uuid: &str) -> Option<Task> {
        self.repo()
            .get_task_by_uuid(task_uuid)
            .unwrap_or_else(|error| panic!("{}: reading task {task_uuid}: {error}", self.label))
    }

    fn task(&self, task_uuid: &str) -> Task {
        self.find_task(task_uuid)
            .unwrap_or_else(|| panic!("{}: task {task_uuid} is missing locally", self.label))
    }

    fn task_id(&self, task_uuid: &str) -> i64 {
        self.task(task_uuid)
            .id
            .unwrap_or_else(|| panic!("{}: task {task_uuid} has no row id", self.label))
    }

    /// The uuids of a task's log entries — the identity the union merge of §11
    /// is keyed by.
    fn log_uuids(&self, task_id: i64) -> BTreeSet<String> {
        self.repo()
            .get_log(task_id)
            .unwrap_or_else(|error| panic!("{}: reading the log: {error}", self.label))
            .into_iter()
            .map(|entry| entry.entry_uuid)
            .collect()
    }

    /// One full sync, with the report's error list treated as a failure.
    ///
    /// `TaskSyncService::sync` collects per-item failures and keeps going,
    /// which is right in production and wrong in a test: an assertion that
    /// passes while the sync quietly reported "upload failed" proves nothing.
    async fn sync(&mut self) -> TaskSyncReport {
        let report = self
            .service
            .sync(&mut self.client)
            .await
            .unwrap_or_else(|error| panic!("{}: sync failed: {error}", self.label));
        assert!(
            report.errors.is_empty(),
            "{}: sync reported errors: {:?}",
            self.label,
            report.errors
        );
        report
    }

    /// The folder as the reconciler sees it: one entry per message, identity
    /// headers only.
    async fn listing(&mut self) -> Vec<ListedMessage> {
        self.client
            .select_mailbox(&self.folder)
            .await
            .unwrap_or_else(|error| panic!("{}: SELECT {}: {error}", self.label, self.folder));

        let mut fields: Vec<&str> = IDENTITY_HEADERS.to_vec();
        fields.push("Message-ID");
        let entries = self
            .client
            .fetch_header_listing("1:*", &fields)
            .await
            .unwrap_or_else(|error| panic!("{}: listing the folder: {error}", self.label));

        let listing = read_listing(&entries);
        assert!(
            listing.unreadable.is_empty(),
            "{}: the folder holds {} message(s) this build cannot identify",
            self.label,
            listing.unreadable.len()
        );
        listing.messages
    }

    /// Every message the server currently holds for one task id. More than one
    /// means an unresolved duplicate; none means the task is not on the server.
    async fn messages_for(&mut self, task_uuid: &str) -> Vec<ListedMessage> {
        self.listing()
            .await
            .into_iter()
            .filter(|message| {
                matches!(message.identity.kind, TaskMessageKind::Task)
                    && message.identity.task_uuid == task_uuid
            })
            .collect()
    }

    /// Every message in the folder, bodies included — used to restore a folder
    /// under a new `UIDVALIDITY`.
    async fn folder_bodies(&mut self) -> Vec<Vec<u8>> {
        self.client
            .select_mailbox(&self.folder)
            .await
            .unwrap_or_else(|error| panic!("{}: SELECT {}: {error}", self.label, self.folder));
        let uids = self
            .client
            .fetch_all_uids()
            .await
            .unwrap_or_else(|error| panic!("{}: listing UIDs: {error}", self.label));
        self.client
            .fetch_bodies(&uids)
            .await
            .unwrap_or_else(|error| panic!("{}: fetching bodies: {error}", self.label))
            .into_iter()
            .map(|(_, body)| body)
            .collect()
    }

    /// The first half of `TaskSyncService::upload`, and nothing else: build the
    /// task's message, APPEND it, find it again by `Message-ID`.
    ///
    /// This is where every crash in §3.5 is injected. What it leaves on the
    /// server is precisely what an upload interrupted after the APPEND leaves
    /// behind — the local cache still says whatever it said before, and the old
    /// message is still there.
    async fn append_current(&mut self, task_uuid: &str) -> (u32, String) {
        let (bytes, message_id) = {
            let repo = self.repo();
            let task = self.task(task_uuid);
            let task_id = task.id.expect("a stored task has a row id");
            TaskEmailBuilder::build(
                &task,
                &repo.get_reminders(task_id).expect("reminders"),
                &repo.get_linked_emails(task_id).expect("links"),
                &repo.get_attachments(task_id).expect("attachments"),
            )
            .unwrap_or_else(|error| panic!("{}: building the message: {error}", self.label))
        };

        self.client
            .append_message(&self.folder, &bytes, true)
            .await
            .unwrap_or_else(|error| panic!("{}: APPEND failed: {error}", self.label));
        self.client
            .select_mailbox(&self.folder)
            .await
            .unwrap_or_else(|error| panic!("{}: SELECT after APPEND: {error}", self.label));

        let found = self
            .client
            .uid_search_message_id(&message_id)
            .await
            .unwrap_or_else(|error| panic!("{}: searching for the append: {error}", self.label));
        let uid = *found.last().unwrap_or_else(|| {
            panic!(
                "{}: the message just appended cannot be found by Message-ID {message_id}",
                self.label
            )
        });
        (uid, message_id)
    }

    /// A complete upload performed out of band: what a second device does when
    /// its listing was taken before the other device's append landed.
    ///
    /// Identical to what `TaskSyncService::upload` does, minus the deletion of
    /// the messages it supersedes — which this device cannot do, because it has
    /// never seen them.
    async fn upload_out_of_band(&mut self, task_uuid: &str) -> u32 {
        let revision = self.task(task_uuid).revision;
        let task_id = self.task_id(task_uuid);
        let (uid, message_id) = self.append_current(task_uuid).await;
        self.repo()
            .mark_uploaded(task_id, revision, uid, &message_id)
            .unwrap_or_else(|error| panic!("{}: recording the upload: {error}", self.label));
        uid
    }

    /// The `UIDVALIDITY` this device last recorded for the folder.
    fn recorded_uidvalidity(&self) -> Option<u32> {
        self.repo()
            .get_sync_state(&self.account)
            .expect("sync state")
            .and_then(|state| state.uidvalidity)
    }

    /// Make this device believe the server has reported the same
    /// `HIGHESTMODSEQ` forever, and one that nothing can ever exceed.
    ///
    /// This is the shared hoster's failure mode (`src/imap/mod.rs:91`) in its
    /// strongest form: any sync that gated its work on "has the MODSEQ moved
    /// since we last looked" would conclude that nothing has changed and do
    /// nothing at all, for the rest of the account's life.
    ///
    /// `i64::MAX` rather than `u64::MAX` because RFC 7162 defines a
    /// mod-sequence as an unsigned *63-bit* value, so this is the ceiling —
    /// and it is also the largest integer SQLite will store.
    fn freeze_modseq(&self) {
        let repo = self.repo();
        let mut state = repo
            .get_sync_state(&self.account)
            .expect("sync state")
            .expect("a device that has synced has sync state");
        state.highest_modseq = Some(FROZEN_MODSEQ);
        state.last_sync_time = Some(orb_core::tasks::format_timestamp(Utc::now()));
        repo.upsert_sync_state(&state).expect("freezing the modseq");
    }
}

/// Connect to the container's IMAPS port, accepting its self-signed
/// certificate. This is the real `ImapClient`, not a test double: the whole
/// point is to exercise APPEND, UID SEARCH and UID EXPUNGE as they are shipped.
async fn connect(container: &DovecotContainer) -> ImapClient {
    let (user, password) = DovecotContainer::credentials();
    ImapClient::connect_insecure(
        &container.host,
        container.imaps_port,
        user,
        &SecretString::from(password.to_string()),
        "task sync test",
    )
    .await
    .expect("failed to connect to the Dovecot container")
}

/// Expunge a message the way a client that is not Orb would: flag and purge,
/// with no regard for what any task cache believes (§10.1).
async fn foreign_expunge(container: &DovecotContainer, folder: &str, uid: u32) {
    let mut session = container
        .connect_plain()
        .await
        .expect("foreign client failed to connect");
    session.select(folder).await.expect("foreign SELECT failed");
    let store = session
        .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
        .await
        .expect("foreign UID STORE failed");
    let _: Vec<_> = store.try_collect().await.expect("store consume");
    let expunge = session
        .uid_expunge(uid.to_string())
        .await
        .expect("foreign UID EXPUNGE failed");
    let _: Vec<_> = expunge.try_collect().await.expect("expunge consume");
    session.logout().await.ok();
}

/// Recreate a folder from scratch and refill it, which is what a restore from
/// backup looks like from the client's side: the same messages, new UIDs, and a
/// new `UIDVALIDITY`.
///
/// The sleep is not decoration. Dovecot derives a new mailbox's `UIDVALIDITY`
/// from the wall clock, so a delete and a create inside the same second produce
/// the same value and the test would silently stop testing anything. The
/// caller asserts the value really changed.
async fn recreate_folder_with(container: &DovecotContainer, folder: &str, messages: &[Vec<u8>]) {
    let mut session = container
        .connect_plain()
        .await
        .expect("failed to connect for the folder rebuild");
    session.delete(folder).await.expect("DELETE failed");
    session.logout().await.ok();

    tokio::time::sleep(Duration::from_millis(2100)).await;

    let mut session = container
        .connect_plain()
        .await
        .expect("failed to reconnect for the folder rebuild");
    session.create(folder).await.expect("CREATE failed");
    for message in messages {
        session
            .append(folder, Some(r"(\Seen)"), None, message)
            .await
            .expect("APPEND during the rebuild failed");
    }
    session.logout().await.ok();
}

// ===========================================================================
// §14.1 — concurrent edit on two devices
// ===========================================================================

/// Two machines edit the same task without seeing each other's message: the
/// highest revision wins, the loser is expunged, and no duplicate survives.
///
/// The duplicate is genuine. Device B's message is built from B's cache and
/// appended by B's connection at a moment when B's last listing predates A's
/// upload — the situation two laptops on two trains produce, and the situation
/// the "group by task id, highest revision, ties by UID" rule exists for.
#[tokio::test]
#[ignore = "requires Docker"]
async fn concurrent_edit_keeps_the_highest_revision_and_leaves_no_duplicate() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;

    let created = a.create_task("Buy milk");
    let uuid = created.task_uuid.clone();
    a.sync().await;
    b.sync().await;
    assert_eq!(
        b.task(&uuid).subject,
        "Buy milk",
        "device B should have the task after its first sync"
    );

    // Both machines edit offline. B edits twice, so B is at the higher
    // revision and must win regardless of who reaches the server first.
    a.edit_subject(a.task_id(&uuid), "Buy milk (A)");
    b.edit_subject(b.task_id(&uuid), "Buy oat milk");
    b.edit_subject(b.task_id(&uuid), "Buy oat milk and bread");
    assert_eq!(a.task(&uuid).revision, 2);
    assert_eq!(b.task(&uuid).revision, 3);

    // A syncs first and cleanly replaces its own message.
    a.sync().await;
    assert_eq!(
        a.messages_for(&uuid).await.len(),
        1,
        "A's own upload must replace its previous message, not accumulate"
    );

    // B's upload was built before it could see A's, so it lands beside it.
    b.upload_out_of_band(&uuid).await;
    assert_eq!(
        a.messages_for(&uuid).await.len(),
        2,
        "the folder should now hold two messages for one task id"
    );

    // The next sync resolves it.
    let report = a.sync().await;
    assert!(report.expunged >= 1, "the loser must be expunged");

    let survivors = a.messages_for(&uuid).await;
    assert_eq!(survivors.len(), 1, "exactly one message may survive");
    assert_eq!(
        survivors[0].identity.revision, 3,
        "the surviving message must be the highest revision"
    );

    let resolved = a.task(&uuid);
    assert_eq!(resolved.subject, "Buy oat milk and bread");
    assert_eq!(resolved.revision, 3);

    // And B, whose copy won, neither re-uploads nor resurrects anything.
    b.sync().await;
    assert_eq!(b.messages_for(&uuid).await.len(), 1);
    assert_eq!(b.task(&uuid).revision, 3);
    assert!(
        !b.task(&uuid).is_dirty,
        "the winning device has nothing left to send"
    );
}

/// Both machines edit the task once from the same revision, so both copies
/// carry the same revision number. The one uploaded last wins (§3.1, ties by
/// highest UID), and both machines must end up with it — not each with its
/// own, believing itself in step. Found against a real server.
#[tokio::test]
#[ignore = "requires Docker"]
async fn edits_from_one_revision_on_two_machines_converge_on_one_copy() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;

    let uuid = a.create_task("Call the plumber").task_uuid.clone();
    a.sync().await;
    b.sync().await;

    a.edit_subject(a.task_id(&uuid), "Call the plumber (A)");
    b.edit_subject(b.task_id(&uuid), "Call the plumber (B)");
    assert_eq!(a.task(&uuid).revision, b.task(&uuid).revision);

    a.sync().await;
    b.sync().await;
    a.sync().await;

    let (on_a, on_b) = (a.task(&uuid), b.task(&uuid));
    assert_eq!(on_a.subject, on_b.subject, "one version on both machines");
    assert_eq!(on_a.subject, "Call the plumber (B)", "the last upload wins");
    assert_eq!(a.messages_for(&uuid).await.len(), 1);
    assert_eq!(
        a.log_uuids(on_a.id.unwrap()),
        b.log_uuids(on_b.id.unwrap()),
        "and the log keeps both edits"
    );
}

/// The tag registry, changed on two machines between syncs, is merged tag by
/// tag (§7): both machines' new tags survive, a deleted tag stays deleted, and
/// both machines end with one registry.
#[tokio::test]
#[ignore = "requires Docker"]
async fn tag_changes_from_one_registry_revision_converge_on_one_registry() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;
    a.repo().create_tag("Errands", None).expect("tag");
    a.sync().await;
    b.sync().await;

    a.repo().create_tag("From A", None).expect("tag on A");
    b.repo().create_tag("From B", None).expect("tag on B");
    a.sync().await;
    b.sync().await;
    a.sync().await;

    let names = |device: &Device| -> BTreeSet<String> {
        device
            .repo()
            .list_tags()
            .expect("tags")
            .into_iter()
            .map(|tag| tag.name)
            .collect()
    };
    assert_eq!(names(&a), names(&b), "one registry on both machines");
    assert!(
        names(&a).contains("From A"),
        "the first machine's tag is kept"
    );
    assert!(names(&a).contains("From B"), "and the second machine's");
    assert!(!b.repo().tag_registry_state().expect("state").dirty);

    // A deletion on one machine while the other creates a tag: merged, the
    // deleted tag stays deleted and the new one arrives.
    let errands = a
        .repo()
        .list_tags()
        .expect("tags")
        .into_iter()
        .find(|tag| tag.name == "Errands")
        .expect("Errands");
    let ctx = a.change_context();
    a.repo()
        .delete_tag(&errands.tag_uuid, &ctx)
        .expect("delete on A");
    b.repo().create_tag("Later on B", None).expect("tag on B");
    a.sync().await;
    b.sync().await;
    a.sync().await;
    assert_eq!(names(&a), names(&b), "one registry on both machines again");
    assert!(
        !names(&a).contains("Errands"),
        "the deleted tag stays deleted"
    );
    assert!(names(&a).contains("Later on B"));
}

/// Waiting's marks travel between machines (chat-spec §6): each machine's
/// marks reach the other, a mark taken back on one goes on both, and a new
/// machine finds them all.
#[tokio::test]
#[ignore = "requires Docker"]
async fn chat_marks_from_two_machines_converge() {
    use orb_core::chat::{marks, store};

    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");
    let now = chrono::Utc::now().timestamp_millis();
    let chat = |device: &Device| {
        store::set_state(
            device.service.database().connection(),
            store::STATE_SERVER,
            "https://chat.example.com",
        )
        .expect("server");
    };
    let mark = |device: &Device, thread: &str| {
        let conn = device.service.database().connection();
        store::set_mark(conn, "r", thread, "done", now).expect("mark");
        marks::touch(conn).expect("touch");
    };
    let threads = |device: &Device| -> BTreeSet<String> {
        let conn = device.service.database().connection();
        let mut stmt = conn
            .prepare("SELECT thread FROM chat_marks")
            .expect("query");
        stmt.query_map([], |row| row.get(0))
            .expect("rows")
            .map(|row| row.expect("row"))
            .collect()
    };

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;
    chat(&a);
    chat(&b);
    mark(&a, "t1");
    a.sync().await;
    b.sync().await;
    assert_eq!(threads(&b), BTreeSet::from(["t1".to_string()]));

    // Both mark a thread before either syncs: merged, nothing lost.
    mark(&a, "t2");
    mark(&b, "t3");
    a.sync().await;
    b.sync().await;
    a.sync().await;
    let all: BTreeSet<String> = ["t1", "t2", "t3"].map(String::from).into();
    assert_eq!(threads(&a), all);
    assert_eq!(threads(&b), all);

    // Taken back on B: gone on A too.
    let conn = b.service.database().connection();
    store::remove_mark(conn, "r", "t1").expect("remove");
    marks::touch(conn).expect("touch");
    b.sync().await;
    a.sync().await;
    let left: BTreeSet<String> = ["t2", "t3"].map(String::from).into();
    assert_eq!(threads(&a), left);
    assert!(
        !marks::state(a.service.database().connection())
            .expect("state")
            .dirty
    );
    assert!(
        !marks::state(b.service.database().connection())
            .expect("state")
            .dirty
    );

    let mut c = Device::start(&container, "device-c").await;
    chat(&c);
    c.sync().await;
    assert_eq!(threads(&c), left, "a new machine finds every mark");
}

// ===========================================================================
// §14.1 — offline edit, then reconnect
// ===========================================================================

/// A task created and edited with no server in the picture reaches the server
/// on the next sync, and the dirty flag clears for the revision that went up.
#[tokio::test]
#[ignore = "requires Docker"]
async fn an_offline_edit_uploads_on_reconnect_and_clears_the_dirty_flag() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;

    // Everything here happens against the local cache alone (§3.6).
    let created = a.create_task("Write the spec");
    let uuid = created.task_uuid.clone();
    let task_id = created.id.expect("a created task has a row id");
    a.edit_subject(task_id, "Write the task spec");
    a.repo()
        .set_due_date(
            &[task_id],
            Some(Utc::now().date_naive() + chrono::Duration::days(400)),
            &a.change_context(),
        )
        .expect("setting a due date offline must work");

    let offline = a.task(&uuid);
    assert!(offline.is_dirty, "an unsynced edit is queued as dirty");
    assert_eq!(offline.revision, 3);
    assert!(offline.imap_uid.is_none());
    assert!(
        a.messages_for(&uuid).await.is_empty(),
        "nothing should be on the server yet"
    );

    let report = a.sync().await;
    assert_eq!(report.uploaded, 1, "the backlog is one task");

    let uploaded = a.task(&uuid);
    assert!(
        !uploaded.is_dirty,
        "the uploaded revision clears the dirty flag"
    );
    assert!(uploaded.imap_uid.is_some(), "the task now has a location");
    assert_eq!(
        uploaded.revision, 3,
        "uploading must not invent a new revision"
    );
    assert!(
        a.repo()
            .get_dirty_tasks(ACCOUNT)
            .expect("dirty set")
            .is_empty(),
        "nothing is left queued"
    );

    let messages = a.messages_for(&uuid).await;
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].identity.revision, 3,
        "the server holds the revision the local task is at"
    );

    // The flag really cleared: a second sync finds nothing to do.
    let again = a.sync().await;
    assert_eq!(again.uploaded, 0);
    assert!(
        again.is_empty(),
        "a sync with nothing to do must move nothing: {again:?}"
    );
}

/// An edit goes up on its own, without a listing, and replaces its previous
/// message (§23).
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_push_uploads_an_edit_without_listing_and_replaces_its_message() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut fresh = Device::start(&container, "fresh").await;
    fresh.create_task("Never listed");
    assert!(
        fresh
            .service
            .push(&mut fresh.client)
            .await
            .expect("push")
            .is_none(),
        "a device that has never listed the folder needs a full sync"
    );

    let mut a = Device::start(&container, "device-a").await;
    let created = a.create_task("Draft the agenda");
    let uuid = created.task_uuid.clone();
    let task_id = created.id.expect("row id");
    a.sync().await;
    let first = a.task(&uuid).imap_uid.expect("uploaded");

    a.edit_subject(task_id, "Draft the agenda for Monday");
    let report = a
        .service
        .push(&mut a.client)
        .await
        .expect("push")
        .expect("a device in step with the folder can push");
    assert_eq!(report.uploaded, 1);
    assert_eq!(report.listed, 0, "a push reads no listing");

    let pushed = a.task(&uuid);
    assert!(!pushed.is_dirty);
    let messages = a.messages_for(&uuid).await;
    assert_eq!(
        messages.len(),
        1,
        "the previous message is gone: {messages:?}"
    );
    assert_eq!(messages[0].identity.revision, pushed.revision);
    assert_ne!(messages[0].uid, first);
    assert_eq!(Some(messages[0].uid), pushed.imap_uid);

    // Nothing left to send: a push is a no-op.
    let again = a.service.push(&mut a.client).await.expect("push");
    assert!(again.is_some_and(|report| report.is_empty()));
}

// ===========================================================================
// §14.1 — an edit racing an in-flight upload (§3.2)
// ===========================================================================

/// An edit made while an upload is in flight must not be marked clean.
///
/// This is the compare-and-swap of §3.2, reproduced at full size: the message
/// for revision 2 is genuinely on the server, the user's next edit lands before
/// the flag is cleared, and `mark_uploaded` is then asked to clear the flag for
/// a revision that is no longer the current one. Clearing it there is how an
/// edit disappears — marked clean, never uploaded, overwritten at the next
/// download — so the assertion is that it does not.
#[tokio::test]
#[ignore = "requires Docker"]
async fn an_edit_racing_an_upload_still_reaches_the_server() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;

    let created = a.create_task("Book flights");
    let uuid = created.task_uuid.clone();
    let task_id = created.id.expect("row id");
    a.sync().await;

    a.edit_subject(task_id, "Book flights to Lisbon");
    assert_eq!(a.task(&uuid).revision, 2);

    // The upload for revision 2 reaches the server ...
    let (uid, message_id) = a.append_current(&uuid).await;
    // ... and the user edits again before the response is processed.
    a.edit_subject(task_id, "Book flights to Lisbon in May");
    assert_eq!(a.task(&uuid).revision, 3);

    let cleared = a
        .repo()
        .mark_uploaded(task_id, 2, uid, &message_id)
        .expect("recording the upload");
    assert!(
        !cleared,
        "clearing the dirty flag must be a compare-and-swap on the uploaded revision"
    );
    let racing = a.task(&uuid);
    assert!(
        racing.is_dirty,
        "the task edited during the upload stays queued"
    );
    assert_eq!(
        racing.imap_uid,
        Some(uid),
        "the location is recorded either way — the next upload has to know what to replace"
    );

    // The next sync carries the later edit up.
    let report = a.sync().await;
    assert_eq!(report.uploaded, 1);
    let messages = a.messages_for(&uuid).await;
    assert_eq!(messages.len(), 1, "the superseded message must be gone");
    assert_eq!(messages[0].identity.revision, 3);
    assert!(!a.task(&uuid).is_dirty);

    // And the edit is on the server in full, not merely as a revision number:
    // a machine that has never seen this task reads it back.
    let mut fresh = Device::start(&container, "fresh").await;
    fresh.sync().await;
    assert_eq!(
        fresh.task(&uuid).subject,
        "Book flights to Lisbon in May",
        "the edit that raced the upload must be the one another machine sees"
    );
}

// ===========================================================================
// §14.1 — a crash between APPEND and DELETE (§3.5, §14 requirement 4)
// ===========================================================================

/// Both crash points between APPEND and DELETE converge on one message and
/// lose nothing.
///
/// The order of operations is APPEND, search, record, delete. A crash can land
/// in two places, and they leave different wreckage:
///
/// 1. after the APPEND, before anything is recorded — the cache still points at
///    the old message and still believes it has unsent changes;
/// 2. after the location is recorded, before the old message is deleted — the
///    cache is clean and correct, and the folder holds a stale duplicate.
///
/// Both are exercised here, in sequence, on the same task.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_crash_between_append_and_delete_converges_on_one_message() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;

    let created = a.create_task("Renew the certificate");
    let uuid = created.task_uuid.clone();
    let task_id = created.id.expect("row id");
    a.sync().await;

    // -- crash point 1: appended, nothing recorded ---------------------------
    a.edit_subject(task_id, "Renew the TLS certificate");
    a.append_current(&uuid).await;
    assert_eq!(
        a.messages_for(&uuid).await.len(),
        2,
        "a crash after the APPEND leaves a duplicate, which is the recoverable failure"
    );
    assert!(
        a.task(&uuid).is_dirty,
        "nothing was recorded, so the task is still queued"
    );

    let report = a.sync().await;
    assert!(report.errors.is_empty());
    let messages = a.messages_for(&uuid).await;
    assert_eq!(messages.len(), 1, "the next sync converges on one message");
    assert_eq!(
        a.task(&uuid).subject,
        "Renew the TLS certificate",
        "the edit that was mid-flight during the crash is not lost"
    );
    assert!(!a.task(&uuid).is_dirty);

    // -- crash point 2: appended and recorded, never deleted -----------------
    a.edit_subject(task_id, "Renew the TLS certificate before June");
    let revision = a.task(&uuid).revision;
    let (uid, message_id) = a.append_current(&uuid).await;
    let clean = a
        .repo()
        .mark_uploaded(task_id, revision, uid, &message_id)
        .expect("recording the upload");
    assert!(clean, "no edit raced this upload");
    assert_eq!(
        a.messages_for(&uuid).await.len(),
        2,
        "the message this upload replaced is still there"
    );

    let report = a.sync().await;
    assert!(report.expunged >= 1);
    let messages = a.messages_for(&uuid).await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].identity.revision, revision);
    assert_eq!(
        a.task(&uuid).subject,
        "Renew the TLS certificate before June"
    );

    // A machine that only ever sees the end state agrees with all of it.
    let mut fresh = Device::start(&container, "fresh").await;
    fresh.sync().await;
    let recovered = fresh.task(&uuid);
    assert_eq!(recovered.subject, "Renew the TLS certificate before June");
    assert_eq!(recovered.revision, revision);
}

// ===========================================================================
// §14.1 — UIDVALIDITY change (§3.4)
// ===========================================================================

/// A `UIDVALIDITY` bump means "your UIDs are meaningless", not "the tasks are
/// gone": the folder is reconciled by task id, every cached location is
/// repaired, and nothing is deleted or duplicated.
///
/// The server-side surgery here is a restore from backup — the folder is
/// deleted and refilled with the very same messages in the opposite order, so
/// the `UIDVALIDITY` is new and so is every UID. That is the shape of the
/// accident this rule exists for, and the one where deleting local tasks
/// because "their UIDs are not on the server" would destroy the list.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_uidvalidity_change_reconciles_without_losing_a_task() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;

    let subjects = ["File taxes", "Service the bike", "Call the dentist"];
    let uuids: Vec<String> = subjects
        .iter()
        .map(|subject| a.create_task(subject).task_uuid)
        .collect();
    a.sync().await;

    let before_uidvalidity = a.recorded_uidvalidity();
    let uids_before: Vec<Option<u32>> = uuids.iter().map(|uuid| a.task(uuid).imap_uid).collect();
    assert!(
        uids_before.iter().all(Option::is_some),
        "every task should have a location before the bump"
    );

    // Restore the folder: same messages, reversed, hence new UIDs everywhere.
    let mut bodies = a.folder_bodies().await;
    assert_eq!(bodies.len(), 3);
    bodies.reverse();
    recreate_folder_with(&container, &a.folder, &bodies).await;
    a.reconnect(&container).await;

    let report = a.sync().await;
    assert!(
        report.uidvalidity_reset,
        "the bump has to be noticed — the assertions below are meaningless otherwise"
    );
    assert_ne!(
        a.recorded_uidvalidity(),
        before_uidvalidity,
        "the container did not actually change UIDVALIDITY; the scenario did not happen"
    );
    assert_eq!(
        report.deleted, 0,
        "a UIDVALIDITY bump must never delete a task locally"
    );

    // Zero task loss, zero duplication, and every location repaired.
    for (index, uuid) in uuids.iter().enumerate() {
        let task = a.task(uuid);
        assert_eq!(task.subject, subjects[index]);
        assert_eq!(task.revision, 1, "reconciliation is not an edit");
        assert!(
            task.imap_uid.is_some(),
            "the cached location must be repaired, not merely discarded"
        );
        assert_eq!(
            a.messages_for(uuid).await.len(),
            1,
            "no task may be re-uploaded as a duplicate"
        );
    }
    assert_eq!(a.listing().await.len(), 3);
    assert!(
        uuids
            .iter()
            .zip(&uids_before)
            .any(|(uuid, before)| a.task(uuid).imap_uid != *before),
        "the rebuilt folder should have moved at least one UID; \
         if not, this test is no longer exercising a rebind"
    );

    // A second sync has nothing left to do — the repair was durable.
    let settled = a.sync().await;
    assert!(
        settled.is_empty(),
        "still moving after the bump: {settled:?}"
    );
}

// ===========================================================================
// §14.1 — a task message hard-deleted on the server (§10.1)
// ===========================================================================

/// A task message expunged by something other than Orb is applied as a
/// permanent local deletion, and only that one task is affected.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_message_hard_deleted_on_the_server_is_deleted_locally() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;

    let doomed = a.create_task("Cancel the subscription").task_uuid;
    let survivor = a.create_task("Water the plants").task_uuid;
    a.sync().await;

    let uid = a.task(&doomed).imap_uid.expect("uploaded tasks have a UID");
    foreign_expunge(&container, &a.folder, uid).await;

    let report = a.sync().await;
    assert_eq!(report.deleted, 1, "exactly one task should be removed");
    assert!(
        a.find_task(&doomed).is_none(),
        "a hard delete on the server is indistinguishable from an intentional expunge"
    );
    assert!(
        a.find_task(&survivor).is_some(),
        "the other task must be untouched"
    );
    assert_eq!(a.listing().await.len(), 1);

    // The deletion does not bounce back: nothing re-uploads the dead task.
    let settled = a.sync().await;
    assert!(settled.is_empty(), "still moving: {settled:?}");
    assert!(a.find_task(&doomed).is_none());
}

// ===========================================================================
// §14.1 — a server whose HIGHESTMODSEQ never advances (§3.3, §14 req 6)
// ===========================================================================

/// The most important test here. One hoster reports the same `HIGHESTMODSEQ`
/// forever, so a sync that decides what to do from a MODSEQ delta freezes on
/// that account and reports success while doing so — silently, for as long as
/// the account exists.
///
/// The simulation is stronger than the real server: the recorded MODSEQ is
/// pinned to `u64::MAX` before every sync, so any implementation that asked
/// "has anything changed since the value I stored" would conclude "no" for
/// every change that could ever be made. Three different kinds of change are
/// then made from another device, and each one has to be detected anyway —
/// because the listing is the record of what is in the folder, and a listing
/// cannot freeze.
#[tokio::test]
#[ignore = "requires Docker"]
async fn changes_are_detected_when_the_servers_modseq_never_advances() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;

    let first = b.create_task("Read the CONDSTORE RFC").task_uuid;
    b.sync().await;
    a.sync().await;
    assert!(a.find_task(&first).is_some());

    // -- a task created elsewhere --------------------------------------------
    let second = b.create_task("Read the QRESYNC RFC").task_uuid;
    b.sync().await;
    a.freeze_modseq();
    let report = a.sync().await;
    assert_eq!(
        report.listed, 2,
        "the whole folder must be listed on every sync, never a delta"
    );
    assert!(
        a.find_task(&second).is_some(),
        "a new task must be found even when the MODSEQ says nothing changed"
    );

    // -- an edit made elsewhere ----------------------------------------------
    b.edit_subject(b.task_id(&first), "Read RFC 7162 properly");
    b.sync().await;
    a.freeze_modseq();
    a.sync().await;
    assert_eq!(
        a.task(&first).subject,
        "Read RFC 7162 properly",
        "an edit must be found even when the MODSEQ says nothing changed"
    );

    // -- a deletion made elsewhere -------------------------------------------
    // Deletion is the harshest case: there is no new message to stumble over,
    // only an absence, and an absence is invisible to any delta protocol.
    let uid = a.task(&second).imap_uid.expect("a synced task has a UID");
    foreign_expunge(&container, &a.folder, uid).await;
    a.freeze_modseq();
    let report = a.sync().await;
    assert_eq!(
        report.deleted, 1,
        "a deletion must be found even when the MODSEQ says nothing changed"
    );
    assert!(a.find_task(&second).is_none());

    // The MODSEQ is recorded for diagnostics, so the frozen value is replaced
    // by whatever the server actually said — it is simply never consulted.
    let recorded = a
        .repo()
        .get_sync_state(ACCOUNT)
        .expect("sync state")
        .and_then(|state| state.highest_modseq);
    assert_ne!(
        recorded,
        Some(FROZEN_MODSEQ),
        "the frozen value should have been overwritten by the server's own"
    );
}

// ===========================================================================
// §14.1 — log entries appended on two devices (§11)
// ===========================================================================

/// The log is the one part of a task exempt from last-writer-wins: two devices
/// appending entries must both keep theirs, in both directions.
///
/// The device whose *task* loses the conflict still keeps its history — that is
/// the whole reason the merge is a union keyed by entry uuid rather than a
/// copy of whichever message won.
#[tokio::test]
#[ignore = "requires Docker"]
async fn log_entries_from_two_devices_are_merged_as_a_union() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;

    let created = a.create_task("Plan the trip");
    let uuid = created.task_uuid.clone();
    a.sync().await;
    b.sync().await;

    let creation_entry = a.log_uuids(a.task_id(&uuid));
    assert_eq!(creation_entry.len(), 1, "creation writes one log entry");

    // Each device edits offline; B twice, so B's task wins the field values.
    a.edit_subject(a.task_id(&uuid), "Plan the trip to Porto");
    b.edit_subject(b.task_id(&uuid), "Plan the trip to Lisbon");
    b.edit_subject(b.task_id(&uuid), "Plan the trip to Lisbon in May");

    let from_a: BTreeSet<String> = a
        .log_uuids(a.task_id(&uuid))
        .difference(&creation_entry)
        .cloned()
        .collect();
    let from_b: BTreeSet<String> = b
        .log_uuids(b.task_id(&uuid))
        .difference(&creation_entry)
        .cloned()
        .collect();
    assert_eq!(from_a.len(), 1);
    assert_eq!(from_b.len(), 2);

    // A uploads first and loses the field values to B's higher revision.
    a.sync().await;
    b.sync().await;
    a.sync().await;

    assert_eq!(
        a.task(&uuid).subject,
        "Plan the trip to Lisbon in May",
        "the higher revision wins the fields"
    );
    let merged = a.log_uuids(a.task_id(&uuid));
    assert!(
        from_a.is_subset(&merged),
        "the losing device must keep its own history"
    );
    assert!(
        from_b.is_subset(&merged),
        "the winning device's history must arrive"
    );
    assert_eq!(merged.len(), 4, "creation plus three edits, no duplicates");

    // And the union travels back: B learns about A's entry on the next round.
    a.edit_subject(a.task_id(&uuid), "Plan the trip to Lisbon in June");
    a.sync().await;
    b.sync().await;

    let b_log = b.log_uuids(b.task_id(&uuid));
    assert!(
        from_a.is_subset(&b_log),
        "the entry written on A must reach B"
    );
    assert_eq!(b_log.len(), 5);

    // Re-syncing does not duplicate entries: the merge is keyed by uuid and is
    // therefore idempotent.
    b.sync().await;
    a.sync().await;
    assert_eq!(b.log_uuids(b.task_id(&uuid)).len(), 5);
    assert_eq!(a.log_uuids(a.task_id(&uuid)).len(), 5);
}

// ===========================================================================
// §14.1 — the local database deleted and resynced (§14 requirement 5)
// ===========================================================================

/// The SQLite tables are a cache. Deleting them and syncing again reproduces
/// every task, tag, log entry, link, reminder and attachment exactly.
///
/// This is the requirement that makes the folder — not the database — the
/// source of truth, and it is the one that decides whether a lost laptop is an
/// inconvenience or a data loss event.
#[tokio::test]
#[ignore = "requires Docker"]
async fn deleting_the_local_database_and_resyncing_reproduces_everything() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;

    // A task with one of everything.
    let tag = a
        .repo()
        .create_tag("travel", Some("#3b82f6"))
        .expect("creating a tag");
    let created = a.create_task("Book the hotel");
    let uuid = created.task_uuid.clone();
    let task_id = created.id.expect("row id");

    let remind_at = Utc::now() + chrono::Duration::days(365);
    a.repo()
        .add_reminder(task_id, &remind_at.to_rfc3339(), &a.change_context())
        .expect("adding a reminder");

    {
        let ctx = a.change_context();
        let repo = a.repo();
        repo.apply(
            task_id,
            &TaskUpdates::new()
                .with_body_text(Some("Two nights, near the water.".to_string()))
                .with_tags(vec![tag.tag_uuid.clone()])
                .with_due_date(Some(Utc::now().date_naive() + chrono::Duration::days(370))),
            &ctx,
        )
        .expect("editing the task");
        repo.link_email(
            task_id,
            "<booking-confirmation@example.com>",
            &CachedEmailMetadata {
                subject: Some("Your booking".to_string()),
                from: Some("hotel@example.com".to_string()),
                date: Some("2026-08-06T09:00:00Z".to_string()),
                account: Some(ACCOUNT.to_string()),
                folder: Some("INBOX".to_string()),
            },
            &ctx,
        )
        .expect("linking a message");
    }

    // Point the attachment cache at a temp directory before anything touches
    // it. The default is `~/.orb/attachments`, and a test that downloads a
    // payload would otherwise write into the running user's real cache — and
    // then have to remember to sweep up after itself, which is the kind of
    // cleanup that gets skipped the one time a test panics.
    let cache = tempfile::tempdir().expect("a temp directory for the attachment cache");
    set_attachment_cache_root(cache.path().to_path_buf());

    // The attachment payload travels in a message of its own (§9), so it has to
    // exist in the local cache before it can be uploaded.
    let payload = b"itinerary line one\nitinerary line two\n".to_vec();
    let attachment_path = {
        let mut attachment = TaskAttachment::create(
            task_id,
            &uuid,
            "itinerary.txt".to_string(),
            Some("text/plain".to_string()),
            Some(payload.len() as i64),
        );
        let path = attachment_cache_root().join(&attachment.file_path);
        std::fs::create_dir_all(path.parent().expect("a cache path has a parent"))
            .expect("creating the attachment cache directory");
        std::fs::write(&path, &payload).expect("writing the payload");
        attachment.is_cached = true;
        a.repo()
            .add_attachment(&attachment, &a.change_context())
            .expect("recording the attachment");
        path
    };

    let report = a.sync().await;
    assert_eq!(report.attachments_uploaded, 1);
    assert_eq!(report.uploaded, 1);

    let original = a.task(&uuid);
    let original_log = a.log_uuids(task_id);

    // A machine that has never seen any of this, with an empty database.
    let mut fresh = Device::start(&container, "rebuilt").await;
    fresh.sync().await;

    let rebuilt = fresh.task(&uuid);
    let rebuilt_id = rebuilt.id.expect("row id");
    assert_eq!(rebuilt.task_uuid, original.task_uuid);
    assert_eq!(rebuilt.subject, original.subject);
    assert_eq!(rebuilt.body_text, original.body_text);
    assert_eq!(rebuilt.revision, original.revision);
    assert_eq!(rebuilt.due_date, original.due_date);
    assert_eq!(rebuilt.postpone_date, original.postpone_date);
    assert_eq!(rebuilt.created_at, original.created_at);
    assert_eq!(rebuilt.updated_at, original.updated_at);
    assert_eq!(rebuilt.tags, original.tags);
    assert_eq!(rebuilt.completed_at, original.completed_at);
    assert_eq!(rebuilt.deleted_at, original.deleted_at);

    // The tag registry is its own message and comes back with it (§7).
    let rebuilt_tags = fresh.repo().list_tags().expect("tags");
    assert_eq!(rebuilt_tags.len(), 1);
    assert_eq!(rebuilt_tags[0].tag_uuid, tag.tag_uuid);
    assert_eq!(rebuilt_tags[0].name, "travel");
    assert_eq!(rebuilt_tags[0].colour.as_deref(), Some("#3b82f6"));
    assert_eq!(
        fresh.repo().tag_registry_revision().expect("revision"),
        a.repo().tag_registry_revision().expect("revision"),
    );

    // The log (§11).
    assert_eq!(fresh.log_uuids(rebuilt_id), original_log);
    assert!(!original_log.is_empty());

    // The link, with the cache that keeps it readable if the mail is gone.
    let links = fresh.repo().get_linked_emails(rebuilt_id).expect("links");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].message_id, "<booking-confirmation@example.com>");
    assert_eq!(links[0].cached_subject.as_deref(), Some("Your booking"));
    assert_eq!(links[0].cached_from.as_deref(), Some("hotel@example.com"));

    // The reminder (§5) — the one thing that has to survive a year and a
    // reinstall.
    let reminders = fresh.repo().get_reminders(rebuilt_id).expect("reminders");
    assert_eq!(reminders.len(), 1);
    assert_eq!(reminders[0].remind_at_unix, remind_at.timestamp());

    // The attachment descriptor, bound to the payload message the listing found.
    let attachments = fresh
        .repo()
        .get_attachments(rebuilt_id)
        .expect("attachments");
    assert_eq!(attachments.len(), 1);
    let rebuilt_attachment = attachments[0].clone();
    assert_eq!(rebuilt_attachment.filename, "itinerary.txt");
    assert_eq!(
        rebuilt_attachment.content_type.as_deref(),
        Some("text/plain")
    );
    assert_eq!(rebuilt_attachment.size, Some(payload.len() as i64));
    assert!(
        rebuilt_attachment.imap_uid.is_some(),
        "the payload's location must be learned from the listing"
    );

    // And the payload itself, fetched lazily. The cached file is removed first,
    // so what is compared is what came back off the server rather than what the
    // first device happened to leave on disk.
    std::fs::remove_file(&attachment_path).expect("clearing the cached payload");
    let restored = fresh
        .service
        .download_attachment(&mut fresh.client, &uuid, &rebuilt_attachment)
        .await
        .expect("downloading the payload");
    assert_eq!(
        std::fs::read(&restored).expect("reading the restored payload"),
        payload,
        "the attachment payload must come back byte for byte"
    );
}

// ===========================================================================
// §14.1 — date arithmetic across DST and a timezone change (§4.3, §14 req 1)
// ===========================================================================

/// Central European Time with the EU's real daylight-saving rules.
///
/// A fixed offset cannot express the thing under test: the bug this guards
/// against is `and_utc()`, which makes a task appear and vanish at 01:00 local
/// instead of at midnight, and it is only visible where the offset is not zero
/// *and* changes twice a year. The rule is the EU's: CEST from 01:00 UTC on the
/// last Sunday of March until 01:00 UTC on the last Sunday of October.
struct CentralEurope;

impl CentralEurope {
    fn winter() -> FixedOffset {
        FixedOffset::east_opt(3600).expect("+01:00 is a valid offset")
    }

    fn summer() -> FixedOffset {
        FixedOffset::east_opt(7200).expect("+02:00 is a valid offset")
    }

    /// The last Sunday of a month — where the EU puts both transitions.
    fn last_sunday(year: i32, month: u32) -> NaiveDate {
        let first_next = if month == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)
        }
        .expect("a valid first-of-month");

        let mut day = first_next.pred_opt().expect("a valid last-of-month");
        while day.weekday() != Weekday::Sun {
            day = day.pred_opt().expect("walking back inside one month");
        }
        day
    }

    fn offset_at(at: DateTime<Utc>) -> FixedOffset {
        let year = at.year();
        let starts = Utc
            .from_utc_datetime(
                &Self::last_sunday(year, 3)
                    .and_hms_opt(1, 0, 0)
                    .expect("01:00 exists"),
            )
            .to_utc();
        let ends = Utc
            .from_utc_datetime(
                &Self::last_sunday(year, 10)
                    .and_hms_opt(1, 0, 0)
                    .expect("01:00 exists"),
            )
            .to_utc();
        if at >= starts && at < ends {
            Self::summer()
        } else {
            Self::winter()
        }
    }
}

impl LocalZone for CentralEurope {
    fn civil_date(&self, at: DateTime<Utc>) -> NaiveDate {
        (at + Self::offset_at(at)).date_naive()
    }

    fn midnight(&self, day: NaiveDate) -> DateTime<Utc> {
        let local_midnight = day.and_hms_opt(0, 0, 0).expect("midnight exists");
        // Try each offset and keep the one that is actually in force at the
        // instant it produces. The EU moves its clocks at 02:00/03:00 local, so
        // local midnight is never inside a gap and exactly one candidate is
        // ever consistent.
        for offset in [Self::winter(), Self::summer()] {
            let candidate = (local_midnight - offset).and_utc();
            if Self::offset_at(candidate) == offset {
                return candidate;
            }
        }
        (local_midnight - Self::winter()).and_utc()
    }
}

/// A date a year out arrives on that date in the local timezone, on both sides
/// of a DST transition, and moves when the machine's timezone does.
///
/// No server is involved: a clock and a timezone are not something Dovecot can
/// answer for, and this requirement is important enough to run on every
/// `cargo test` rather than only when Docker is up.
#[test]
fn a_date_a_year_out_lands_on_that_local_day_across_dst_and_timezone_changes() {
    // Self-check on the fixture: if these move, the assertions below are
    // testing a timezone that does not exist.
    assert_eq!(
        CentralEurope::last_sunday(2027, 3),
        NaiveDate::from_ymd_opt(2027, 3, 28).unwrap()
    );
    assert_eq!(
        CentralEurope::last_sunday(2026, 10),
        NaiveDate::from_ymd_opt(2026, 10, 25).unwrap()
    );

    let context = TestContext::new();
    let db = context.with_database();
    db.init_schema().expect("schema");

    let repo = TaskRepository::with_zone(&db, CentralEurope);

    // Scheduled a year and a bit ahead, onto the day *after* the spring
    // transition — the day the clocks have already moved for.
    let created_at = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
    let target = NaiveDate::from_ymd_opt(2027, 3, 29).unwrap();

    let ctx = repo.context_at(created_at, None);
    let task = repo
        .create_task("Renew the passport", ACCOUNT, &ctx)
        .expect("creating the task");
    let task_id = task.id.expect("row id");
    repo.set_postpone_date(&[task_id], Some(target), &ctx)
        .expect("scheduling the task");

    // 23:59:59 local on the day before, which is 21:59:59 UTC in CEST.
    let just_before = Utc.with_ymd_and_hms(2027, 3, 28, 21, 59, 59).unwrap();
    // Local midnight on the target day.
    let at_midnight = Utc.with_ymd_and_hms(2027, 3, 28, 22, 0, 0).unwrap();
    // 01:30 local on the target day — where the `and_utc()` bug used to put
    // the boundary, so a task would appear an hour late.
    let after_midnight = Utc.with_ymd_and_hms(2027, 3, 28, 23, 30, 0).unwrap();

    assert_eq!(
        repo.today_at(just_before),
        NaiveDate::from_ymd_opt(2027, 3, 28).unwrap()
    );
    assert_eq!(repo.today_at(at_midnight), target);
    assert_eq!(repo.today_at(after_midnight), target);

    let before_day = repo.today_at(just_before);
    assert!(
        !repo
            .today_tasks(before_day)
            .expect("today")
            .iter()
            .any(|item| item.id == task_id),
        "the task must not appear before its day begins"
    );
    assert!(
        repo.upcoming_tasks(before_day)
            .expect("upcoming")
            .iter()
            .any(|item| item.id == task_id),
        "until then it lives in Upcoming, where it can be re-planned"
    );
    assert!(
        repo.today_tasks(repo.today_at(at_midnight))
            .expect("today")
            .iter()
            .any(|item| item.id == task_id),
        "at local midnight, a year later, the task is in Today"
    );

    // The transition days are 23 and 25 hours long, and the midnight timer of
    // §4.3 has to be derived from the zone rather than from "now plus 24h".
    let spring_day = CentralEurope.midnight(NaiveDate::from_ymd_opt(2027, 3, 29).unwrap())
        - CentralEurope.midnight(NaiveDate::from_ymd_opt(2027, 3, 28).unwrap());
    assert_eq!(spring_day, chrono::Duration::hours(23));
    let autumn_day = CentralEurope.midnight(NaiveDate::from_ymd_opt(2026, 10, 26).unwrap())
        - CentralEurope.midnight(NaiveDate::from_ymd_opt(2026, 10, 25).unwrap());
    assert_eq!(autumn_day, chrono::Duration::hours(25));
    assert_eq!(
        repo.next_local_midnight(Utc.with_ymd_and_hms(2027, 3, 28, 12, 0, 0).unwrap()),
        at_midnight,
        "an Orb left open across the transition must re-evaluate at the right instant"
    );

    // The machine is carried eleven hours west. The data does not move; the
    // civil date does, and the views follow it.
    let far_west = TaskRepository::with_zone(&db, FixedZone::east_hours(-11).expect("UTC-11"));
    // Local midnight of the target day, eleven hours west: thirteen hours after
    // the same day began in Central Europe.
    let west_midnight = FixedZone::east_hours(-11).expect("UTC-11").midnight(target);
    assert_eq!(
        west_midnight,
        Utc.with_ymd_and_hms(2027, 3, 29, 11, 0, 0).unwrap()
    );
    assert_eq!(
        far_west.today_at(at_midnight),
        NaiveDate::from_ymd_opt(2027, 3, 28).unwrap(),
        "eleven hours west it is still the day before"
    );
    assert!(
        !far_west
            .today_tasks(far_west.today_at(at_midnight))
            .expect("today")
            .iter()
            .any(|item| item.id == task_id),
        "the same instant belongs to a different civil day there"
    );
    assert!(
        far_west
            .today_tasks(far_west.today_at(west_midnight))
            .expect("today")
            .iter()
            .any(|item| item.id == task_id),
        "and the task arrives when that day does"
    );
}

// ===========================================================================
// §14.1 — reminder reconciliation (§5.1)
// ===========================================================================

/// A stand-in for `UNUserNotificationCenter`: a set of pending requests keyed
/// by identifier, which is exactly the contract the real one offers.
#[derive(Default)]
struct FakeNotificationCentre {
    pending: Mutex<BTreeMap<String, ScheduledNotification>>,
}

impl FakeNotificationCentre {
    fn identifiers(&self) -> Vec<String> {
        self.pending
            .lock()
            .expect("the fake centre is never poisoned")
            .keys()
            .cloned()
            .collect()
    }

    fn len(&self) -> usize {
        self.pending
            .lock()
            .expect("the fake centre is never poisoned")
            .len()
    }
}

impl NotificationHost for FakeNotificationCentre {
    fn pending_identifiers(&self) -> orb_core::Result<Vec<String>> {
        Ok(self.identifiers())
    }

    fn schedule(&self, notification: &ScheduledNotification) -> orb_core::Result<()> {
        // Scheduling an identifier that is already pending replaces it, as the
        // real centre does; that is what makes a repeated reconcile harmless.
        self.pending
            .lock()
            .expect("the fake centre is never poisoned")
            .insert(notification.identifier.clone(), notification.clone());
        Ok(())
    }

    fn cancel(&self, identifiers: &[String]) -> orb_core::Result<()> {
        let mut pending = self
            .pending
            .lock()
            .expect("the fake centre is never poisoned");
        for identifier in identifiers {
            pending.remove(identifier);
        }
        Ok(())
    }
}

/// A reminder set on one machine is scheduled with the OS on the other, and
/// moving or deleting it moves or withdraws exactly one notification.
///
/// The first half crosses the wire, because a reminder that syncs but is never
/// registered with the OS is a reminder that only fires on the machine it was
/// typed on — the failure §5.1 exists to prevent. The move and the delete are
/// performed locally: scheduling is per machine by design (`is_triggered` is
/// deliberately not on the wire, §5.2), so reconciliation is a per-machine
/// diff and is tested as one.
///
/// The reminder is deliberately set *after* the task has already been uploaded
/// once. Riding along on a first upload would prove nothing: reminders live in
/// their own table but travel inside the task message, so a reminder added on
/// its own has to bump the task's revision and mark it dirty by itself, or it
/// stays on the machine it was typed on forever — and worse, the next download
/// of that task, whose wire form does not mention it, deletes it. §14
/// requirement 2 is "fires on every machine".
#[tokio::test]
#[ignore = "requires Docker"]
async fn reminders_are_scheduled_moved_and_cancelled_by_reconciliation() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;

    let created = b.create_task("Renew the passport");
    let uuid = created.task_uuid.clone();
    let b_task_id = created.id.expect("row id");
    b.sync().await;
    assert!(
        !b.task(&uuid).is_dirty,
        "the task must be clean before the reminder, or this proves nothing"
    );

    // B sets a reminder a year out. Nothing else about the task changes.
    let remind_at = (Utc::now() + chrono::Duration::days(365))
        .with_timezone(&Utc)
        .with_nanosecond_zero();
    b.repo()
        .add_reminder(b_task_id, &remind_at.to_rfc3339(), &b.change_context())
        .expect("adding a reminder");
    let dirtied = b.task(&uuid);
    assert!(
        dirtied.is_dirty,
        "a reminder is part of the task message; adding one must queue an upload"
    );
    assert!(
        dirtied.revision > created.revision,
        "the revision must move, or the conflict rule cannot see the change"
    );
    b.sync().await;

    a.sync().await;
    let task_id = a.task_id(&uuid);
    let reminders = a.repo().get_reminders(task_id).expect("reminders");
    assert_eq!(
        reminders.len(),
        1,
        "the reminder must reach the other machine"
    );
    assert_eq!(reminders[0].remind_at_unix, remind_at.timestamp());

    let centre = FakeNotificationCentre::default();
    let service = ReminderService::new(a.service.database());
    let now = Utc::now();

    let scheduled = format!("{NOTIFICATION_ID_PREFIX}{uuid}.{}", remind_at.timestamp());
    let plan = service.apply(&centre, now).expect("reconciling");
    assert_eq!(plan.schedule.len(), 1);
    assert_eq!(centre.identifiers(), vec![scheduled.clone()]);

    // Reconciliation is idempotent: the identifier is derived from the task
    // uuid and the instant, so the same reminder is never scheduled twice.
    let again = service.apply(&centre, now).expect("reconciling");
    assert!(
        again.is_noop(),
        "a second reconcile must do nothing: {again:?}"
    );
    assert_eq!(centre.len(), 1);

    // -- moved ---------------------------------------------------------------
    let moved_to = remind_at + chrono::Duration::hours(3);
    assert!(
        a.repo()
            .delete_reminder(reminders[0].id.expect("reminder id"), &a.change_context())
            .expect("removing the old reminder"),
        "the reminder must actually have been there"
    );
    a.repo()
        .add_reminder(task_id, &moved_to.to_rfc3339(), &a.change_context())
        .expect("adding the moved reminder");

    let moved = format!("{NOTIFICATION_ID_PREFIX}{uuid}.{}", moved_to.timestamp());
    let plan = service.apply(&centre, now).expect("reconciling");
    assert_eq!(
        plan.cancel,
        vec![scheduled.clone()],
        "the notification at the old time must be withdrawn"
    );
    assert_eq!(plan.schedule.len(), 1);
    assert_eq!(
        centre.identifiers(),
        vec![moved.clone()],
        "a moved reminder must not stay armed at both times"
    );

    // -- deleted -------------------------------------------------------------
    assert_eq!(
        a.repo()
            .delete_reminders(task_id, &a.change_context())
            .expect("removing every reminder"),
        1
    );
    let plan = service.apply(&centre, now).expect("reconciling");
    assert_eq!(plan.cancel, vec![moved]);
    assert!(
        centre.identifiers().is_empty(),
        "a deleted reminder leaves nothing pending"
    );

    // -- the 64-notification cap ---------------------------------------------
    // macOS holds at most 64 pending local notifications, so the window has to
    // be the nearest 64 and nothing else — the reminder in three days must
    // never be dropped in favour of the one in seventy.
    let mut expected: Vec<String> = Vec::new();
    let filler = a.create_task("Water the plants");
    let filler_id = filler.id.expect("row id");
    for day in 1..=70i64 {
        let at = (now + chrono::Duration::days(day)).with_nanosecond_zero();
        a.repo()
            .add_reminder(filler_id, &at.to_rfc3339(), &a.change_context())
            .expect("adding a reminder");
        if day <= PENDING_NOTIFICATION_LIMIT as i64 {
            expected.push(format!(
                "{NOTIFICATION_ID_PREFIX}{}.{}",
                filler.task_uuid,
                at.timestamp()
            ));
        }
    }

    let plan = service.apply(&centre, now).expect("reconciling");
    assert!(plan.capped, "70 future reminders do not fit in the window");
    assert_eq!(centre.len(), PENDING_NOTIFICATION_LIMIT);
    expected.sort();
    let mut held = centre.identifiers();
    held.sort();
    assert_eq!(
        held, expected,
        "the window must hold the nearest 64 reminders, in full"
    );
}

/// §32: the series of a repeating task travels with it, and the machine that
/// did not complete an occurrence has the next one exactly as the machine that
/// did — dates, series and reminders — from the message alone.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_series_travels_and_both_machines_agree_on_the_next_occurrence() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;

    // A: a weekly task from a week today, with a reminder the evening before.
    let created = a.create_task("Water the plants");
    let uuid = created.task_uuid.clone();
    let a_id = created.id.expect("row id");
    let ctx = a.change_context();
    let day = ctx.today + chrono::Duration::days(7);
    let updates = TaskUpdates::new()
        .with_postpone_date(Some(day))
        .with_recurrence(Some(&"FREQ=WEEKLY".parse().expect("rule")));
    a.repo().apply(a_id, &updates, &ctx).expect("repeat");
    let evening_before = chrono::Local
        .from_local_datetime(
            &(day - chrono::Duration::days(1))
                .and_hms_opt(18, 0, 0)
                .unwrap(),
        )
        .single()
        .expect("a local time");
    ReminderService::new(a.service.database())
        .add_reminder(a_id, &evening_before.to_rfc3339(), &ctx)
        .expect("adding a reminder");
    let stated = a.task(&uuid).recurrence_series.expect("a series");
    assert_eq!(stated, format!("day={day}; reminders=-1T18:00"));
    a.sync().await;

    b.sync().await;
    assert_eq!(
        b.task(&uuid).recurrence_series.as_deref(),
        Some(stated.as_str()),
        "the series must reach the other machine"
    );

    // B completes it: the next occurrence, a week on.
    let b_id = b.task_id(&uuid);
    let ctx = b.change_context();
    let done = b.repo().complete_tasks(&[b_id], &ctx).expect("complete");
    ReminderService::new(b.service.database())
        .follow_recurrence(&done.reminder_shifts, &ctx)
        .expect("follow");
    b.sync().await;
    a.sync().await;

    let next = day + chrono::Duration::days(7);
    for device in [&a, &b] {
        let task = device.task(&uuid);
        assert_eq!(
            task.postpone_date,
            Some(next.to_string()),
            "{}",
            device.label
        );
        assert_eq!(
            task.recurrence_series,
            Some(format!("day={next}; reminders=-1T18:00")),
            "{}",
            device.label
        );
        let reminders = device
            .repo()
            .get_reminders(device.task_id(&uuid))
            .expect("reminders");
        assert_eq!(reminders.len(), 1, "{}", device.label);
        assert_eq!(
            orb_core::tasks::local_wall_clock(reminders[0].remind_at_unix),
            (next - chrono::Duration::days(1))
                .and_hms_opt(18, 0, 0)
                .unwrap(),
            "{}",
            device.label
        );
    }
    assert!(!a.task(&uuid).is_dirty && !b.task(&uuid).is_dirty);
}

/// Truncate to whole seconds.
///
/// A notification identifier is derived from a unix timestamp, so a reminder
/// stored with sub-second precision would be scheduled under one identifier and
/// looked for under another. Real input never has sub-second precision; a test
/// built from `Utc::now()` does.
trait WholeSeconds {
    fn with_nanosecond_zero(self) -> DateTime<Utc>;
}

impl WholeSeconds for DateTime<Utc> {
    fn with_nanosecond_zero(self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.timestamp(), 0).unwrap_or(self)
    }
}

// ===========================================================================
// §10.1 — a purge that outlives the sync that carries it out
// ===========================================================================

/// Emptying the trash must not bring it back on the next sync.
///
/// Purging deletes the only row that knew a task's message existed, and
/// reconciliation reads a task id with no local row as "a task from another
/// machine" and downloads it. Emptying the trash by hand happens with no
/// connection in the picture, so the expunge cannot be part of it — without a
/// record of the purge that survives until the next sync, everything emptied
/// comes straight back.
#[tokio::test]
#[ignore = "requires Docker"]
async fn emptying_the_trash_does_not_bring_it_back() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;

    let created = a.create_task("Cancel the subscription");
    let uuid = created.task_uuid.clone();
    a.sync().await;
    b.sync().await;
    assert_eq!(
        b.find_task(&uuid).map(|t| t.subject),
        Some("Cancel the subscription".to_string())
    );

    // Trash it and empty the trash, both with no server in the picture — which
    // is the ordinary case: emptying the trash is a keystroke, not a sync.
    let ctx = a.change_context();
    a.repo()
        .trash_tasks(&[a.task_id(&uuid)], &ctx)
        .expect("trash the task");
    let purged = a.repo().empty_trash().expect("empty the trash");
    assert_eq!(purged.len(), 1, "the trashed task should have been purged");
    assert!(a.find_task(&uuid).is_none(), "the row is gone locally");

    // The record of the purge is what has to survive until a sync can act on
    // it. Before that sync the message is still on the server.
    let pending = a.repo().pending_expunges().expect("pending expunges");
    assert_eq!(
        pending.len(),
        1,
        "the purge must be remembered until its message is expunged"
    );
    assert_eq!(pending[0].task_uuid, uuid);
    assert_eq!(
        a.messages_for(&uuid).await.len(),
        1,
        "nothing has expunged it yet"
    );

    // The next sync expunges it and — the point of the test — does not read the
    // message it has yet to delete as a new task.
    a.sync().await;
    assert!(
        a.find_task(&uuid).is_none(),
        "the emptied task must not come back"
    );
    assert!(
        a.messages_for(&uuid).await.is_empty(),
        "its message must be gone from the server"
    );
    assert!(
        a.repo()
            .pending_expunges()
            .expect("pending expunges")
            .is_empty(),
        "a settled purge must stop being remembered, or the record grows forever"
    );

    // And the other machine applies the deletion rather than uploading its own
    // copy back.
    b.sync().await;
    assert!(
        b.find_task(&uuid).is_none(),
        "the deletion must reach the other machine"
    );
    assert!(b.messages_for(&uuid).await.is_empty());

    // A second sync on both, because a resurrection would show up one round
    // later than the purge that caused it.
    a.sync().await;
    b.sync().await;
    assert!(a.find_task(&uuid).is_none());
    assert!(b.find_task(&uuid).is_none());
}

/// A task discarded before it was ever named leaves nothing anywhere.
///
/// `create-task` mints a row so the detail view has something to edit, and
/// abandoning that edit erases it. If a sync got there first the message is
/// already on the server, so this is a purge like any other and the tombstone
/// is what removes it.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_discarded_task_is_erased_from_the_server_too() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut a = Device::start(&container, "device-a").await;

    let created = a.create_task("New task");
    let uuid = created.task_uuid.clone();
    let task_id = a.task_id(&uuid);
    a.sync().await;
    assert_eq!(a.messages_for(&uuid).await.len(), 1);

    let discarded = a.repo().discard_task(task_id).expect("discard the task");
    assert!(
        discarded.is_some(),
        "the task should have been there to discard"
    );
    assert!(a.find_task(&uuid).is_none());

    a.sync().await;
    assert!(
        a.messages_for(&uuid).await.is_empty(),
        "a discarded task must not survive on the server"
    );
    a.sync().await;
    assert!(
        a.find_task(&uuid).is_none(),
        "and must not be downloaded back"
    );
}

// ===========================================================================
// §6.2 — a link that survives the message being filed elsewhere
// ===========================================================================

/// Finding a linked message by `Message-ID` after it has moved folders.
///
/// The promise of §6.2 is that a task made from a mail years ago still opens
/// that mail, and the reason it can is that the link is by `Message-ID` rather
/// than by folder and UID: a move changes both of those and changes nothing
/// about the header. This exercises the primitive the resolution rests on —
/// step 2 of §6.2, the search across folders — against a real server.
///
/// The step above it, `Store::find_message_on_server`, cannot be reached from
/// this harness: its connection pool connects over TLS with verification on,
/// and the container serves a self-signed certificate. Turning verification off
/// in a mail client to make a test reachable is the wrong trade, so what is
/// tested here is the layer that does the work.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_linked_message_is_found_again_after_it_moves_folders() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");

    let mut client = connect(&container).await;
    for folder in ["Linked", "Linked.Archive"] {
        client.create_folder(folder).await.ok();
    }

    // A message with a Message-ID a task would have stored, and a decoy in the
    // destination folder so that "found it" cannot be "found the only message".
    let message_id = "<kept-for-years@example.com>";
    let message = common::imap_container::create_rfc822_message(
        "sender@example.com",
        "me@example.com",
        "The contract",
        "Signed and returned.",
    );
    let mut with_id = format!("Message-ID: {message_id}\r\n").into_bytes();
    with_id.extend_from_slice(&message);

    let decoy = common::imap_container::create_rfc822_message(
        "someone@example.com",
        "me@example.com",
        "Unrelated",
        "Nothing to do with it.",
    );

    client
        .append_message("Linked", &with_id, true)
        .await
        .expect("APPEND the linked message");
    client
        .append_message("Linked.Archive", &decoy, true)
        .await
        .expect("APPEND the decoy");

    // Where the task would have cached it.
    client
        .select_mailbox("Linked")
        .await
        .expect("SELECT the original folder");
    let before = client
        .uid_search_message_id(message_id)
        .await
        .expect("searching the original folder");
    assert_eq!(before.len(), 1, "the message should be where it was filed");

    // Filed away, exactly as archiving from any other client would do it.
    client
        .move_messages(&before, "Linked.Archive")
        .await
        .expect("MOVE to the archive");

    // The cached folder now knows nothing, which is the situation a task link
    // is in years later.
    client
        .select_mailbox("Linked")
        .await
        .expect("re-SELECT the original folder");
    assert!(
        client
            .uid_search_message_id(message_id)
            .await
            .expect("searching the emptied folder")
            .is_empty(),
        "the message must be gone from where it was"
    );

    // And the search finds it where it went, without knowing where that was.
    client
        .select_mailbox("Linked.Archive")
        .await
        .expect("SELECT the archive");
    let after = client
        .uid_search_message_id(message_id)
        .await
        .expect("searching the archive");
    assert_eq!(
        after.len(),
        1,
        "the moved message must be findable by its Message-ID alone"
    );

    // A Message-ID that was never there answers empty rather than matching the
    // decoy — the search has to be exact, or a link would open the wrong mail.
    assert!(
        client
            .uid_search_message_id("<never-sent@example.com>")
            .await
            .expect("searching for an absent Message-ID")
            .is_empty(),
        "an unknown Message-ID must match nothing"
    );
}

/// `clear-completed` on one machine hides Today's completed tasks on the
/// others (§10.2): the later cut-off holds everywhere, and a new machine
/// finds it.
#[tokio::test]
#[ignore = "requires Docker"]
async fn the_today_cut_off_travels_and_the_later_one_holds() {
    let container = DovecotContainer::start()
        .await
        .expect("Failed to start container");
    let at = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text)
            .expect("instant")
            .with_timezone(&chrono::Utc)
    };
    let cut_off = |device: &Device| device.repo().today_cleared_at().expect("cut-off");
    let clear = |device: &Device, text: &str| {
        device
            .repo()
            .clear_completed_from_today(at(text))
            .expect("clear");
    };

    let mut a = Device::start(&container, "device-a").await;
    let mut b = Device::start(&container, "device-b").await;
    clear(&a, "2026-08-06T10:00:00Z");
    a.sync().await;
    b.sync().await;
    assert_eq!(cut_off(&b), Some(at("2026-08-06T10:00:00Z")));

    // Both clear before either syncs: the later one holds on both.
    clear(&a, "2026-08-06T12:00:00Z");
    clear(&b, "2026-08-06T11:00:00Z");
    a.sync().await;
    b.sync().await;
    a.sync().await;
    assert_eq!(cut_off(&a), Some(at("2026-08-06T12:00:00Z")));
    assert_eq!(cut_off(&b), Some(at("2026-08-06T12:00:00Z")));

    let mut c = Device::start(&container, "device-c").await;
    c.sync().await;
    assert_eq!(cut_off(&c), Some(at("2026-08-06T12:00:00Z")));
}
