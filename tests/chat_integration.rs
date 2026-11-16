//! The chat service against a real Rocket.Chat (PLAN.md, Phase 15).
//!
//! Orb signs in as one user; a second user, driven through the REST API,
//! writes to it. The questions are the ones a unit test cannot answer: that
//! the realtime stream really names the rooms to sync, that Waiting follows
//! what the server says, and that reading leaves the server's unread state
//! alone while answering clears it.
//!
//! `cargo test --test chat_integration -- --ignored` (Docker), or against a
//! running server with `ORB_TEST_ROCKETCHAT=http://127.0.0.1:3900`.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::rocketchat_container::{RocketChat, TestUser};
use orb_core::chat::{ChatListener, ChatService, ChatStatus, WaitingThread};
use orb_core::config::{ChatConfig, ChatSignIn};
use orb_core::keychain::PasswordStore;
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

struct Counter(AtomicUsize);

impl ChatListener for Counter {
    fn on_changed(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn on_status(&self, _status: &ChatStatus) {}
}

#[derive(Default)]
struct NoKeychain(Mutex<HashMap<String, String>>);

impl PasswordStore for NoKeychain {
    fn get(&self, key: &str) -> Option<SecretString> {
        self.0
            .lock()
            .unwrap()
            .get(key)
            .map(|v| SecretString::from(v.clone()))
    }
    fn set(&self, key: &str, password: &SecretString) -> Result<(), String> {
        use secrecy::ExposeSecret;
        self.0
            .lock()
            .unwrap()
            .insert(key.to_string(), password.expose_secret().to_string());
        Ok(())
    }
}

struct Orb {
    service: Arc<ChatService>,
    db_path: String,
    /// Taken by a test that starts Orb again on the same database.
    dir: Option<tempfile::TempDir>,
    shutdown: CancellationToken,
}

impl Drop for Orb {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn orb_for(server: &RocketChat, user: &TestUser) -> Orb {
    orb_in(server, user, tempfile::tempdir().unwrap())
}

/// An Orb on the database in `dir`, as a restart finds it.
fn orb_in(server: &RocketChat, user: &TestUser, dir: tempfile::TempDir) -> Orb {
    let db_path = dir.path().join("orb.db").to_string_lossy().to_string();
    orb_core::Database::new(&db_path)
        .unwrap()
        .init_schema()
        .unwrap();
    let config = ChatConfig {
        server: server.url.clone(),
        sign_in: ChatSignIn::TokenCommand,
        token_command: Some(format!("echo {} {}", user.id, user.token)),
        waiting_days: 14,
        rooms: HashMap::new(),
        reactions: vec![":+1:".into()],
        links: Vec::new(),
        badge: false,
    };
    let shutdown = CancellationToken::new();
    let service = ChatService::start(
        config,
        db_path.clone(),
        Arc::new(NoKeychain::default()),
        Arc::new(Counter(AtomicUsize::new(0))),
        shutdown.clone(),
    );
    Orb {
        service,
        db_path,
        dir: Some(dir),
        shutdown,
    }
}

/// Poll `check` until it holds, for up to `seconds`.
async fn eventually<T>(seconds: u64, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not within {seconds} s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Run a test with a deadline, printing the service's log if it fails, so a
/// hang or a failure says what the sync was doing.
async fn bounded(test: impl std::future::Future<Output = ()>) {
    use futures::FutureExt;
    orb_core::logger::init(None);
    let outcome = tokio::time::timeout(
        Duration::from_secs(240),
        std::panic::AssertUnwindSafe(test).catch_unwind(),
    )
    .await;
    if !matches!(outcome, Ok(Ok(()))) {
        for line in orb_core::logger::recent(200) {
            eprintln!("{line}");
        }
    }
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("the test did not finish within 240 s"),
    }
}

fn waiting_in(orb: &Orb, rid: &str) -> Vec<WaitingThread> {
    orb.service
        .waiting()
        .unwrap()
        .into_iter()
        .filter(|w| w.rid == rid)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn waiting_follows_the_server_and_reading_marks_nothing_read() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let sara = server.user("sara").await.unwrap();
        let rid = server.channel(&[&sara, &sam]).await.unwrap();

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;

        // A message in the room's timeline waits, without Orb marking it read.
        server
            .send(&sara, &rid, None, "Deploy at four?")
            .await
            .unwrap();
        let rows = eventually(15, || {
            Some(waiting_in(&orb, &rid)).filter(|r| !r.is_empty())
        })
        .await;
        assert_eq!(rows[0].thread, "");
        assert_eq!(rows[0].preview, "Deploy at four?");
        let conversation = orb.service.conversation(&rid, "").await.unwrap().unwrap();
        assert_eq!(conversation.messages.len(), 1);
        orb.service.mark_seen(&rid, "").unwrap();
        assert!(
            server.unread(&sam, &rid).await.unwrap(),
            "reading in Orb marks nothing read"
        );

        // `u` marks it read on the server, and unread again.
        orb.service.set_read(&rid, "", true).await.unwrap();
        assert!(!server.unread(&sam, &rid).await.unwrap());
        orb.service.set_read(&rid, "", false).await.unwrap();
        assert!(server.unread(&sam, &rid).await.unwrap());
        assert!(waiting_in(&orb, &rid)[0].unread);

        // Answering takes it out of Waiting and marks the room read on the server.
        orb.service
            .send(&rid, "", "Yes, at four.", false)
            .await
            .unwrap();
        assert!(waiting_in(&orb, &rid).is_empty());
        assert!(!server.unread(&sam, &rid).await.unwrap());

        // A thread is a row of its own, and a mention says so.
        let root = server
            .send(&sara, &rid, None, "Release 0.9: what is missing?")
            .await
            .unwrap();
        server
            .send(
                &sara,
                &rid,
                Some(&root),
                &format!("@{} the notarisation fails", sam.username),
            )
            .await
            .unwrap();
        let thread = eventually(15, || {
            waiting_in(&orb, &rid)
                .into_iter()
                .find(|w| w.thread == root)
        })
        .await;
        assert!(thread.mention);
        assert_eq!(
            thread.title.as_deref(),
            Some("Release 0.9: what is missing?")
        );
        let conversation = orb
            .service
            .conversation(&rid, &root)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(conversation.messages.len(), 2);

        // Done holds until someone writes again.
        orb.service.mark(&rid, &root, "done").await.unwrap();
        assert!(waiting_in(&orb, &rid).iter().all(|w| w.thread != root));
        server
            .send(&sara, &rid, Some(&root), "Any news?")
            .await
            .unwrap();
        eventually(15, || {
            waiting_in(&orb, &rid)
                .into_iter()
                .find(|w| w.thread == root)
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn a_direct_message_waits_and_a_reply_from_elsewhere_answers_it() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let lukas = server.user("lukas").await.unwrap();
        let rid = server.direct(&lukas, &sam).await.unwrap();

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;

        server
            .send(&lukas, &rid, None, "Can you look at the PR?")
            .await
            .unwrap();
        let rows = eventually(15, || {
            Some(waiting_in(&orb, &rid)).filter(|r| !r.is_empty())
        })
        .await;
        assert_eq!(rows[0].room.label, "Direct");
        assert_eq!(rows[0].author, "lukas");

        // Answered in Rocket.Chat itself, not in Orb.
        server.send(&sam, &rid, None, "On it.").await.unwrap();
        eventually(15, || waiting_in(&orb, &rid).is_empty().then_some(())).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn older_messages_come_from_the_server_once_the_database_runs_out() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let sara = server.user("sara").await.unwrap();
        let rid = server.channel(&[&sara, &sam]).await.unwrap();
        for i in 0..3 {
            server
                .send(&sara, &rid, None, &format!("message {i}"))
                .await
                .unwrap();
        }

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;
        eventually(15, || {
            Some(waiting_in(&orb, &rid)).filter(|r| !r.is_empty())
        })
        .await;

        // As if the messages were older than the first sync reached.
        rusqlite::Connection::open(&orb.db_path)
            .unwrap()
            .execute("DELETE FROM chat_messages WHERE rid = ?1", [&rid])
            .unwrap();
        let now = chrono::Utc::now().timestamp_millis() + 60_000;
        let page = orb.service.older(&rid, "", now).await.unwrap();
        let bodies: Vec<_> = page
            .messages
            .iter()
            .filter(|m| m.system.is_none())
            .map(|m| m.body.as_str())
            .collect();
        assert_eq!(
            bodies,
            ["message 0", "message 1", "message 2"],
            "oldest first"
        );
        assert!(
            !page.has_older,
            "short of a page: the room's start is reached"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn the_picker_finds_people_and_rooms_and_opens_or_joins_them() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let lucia = server.user("lucia").await.unwrap();
        let rid = server.channel(&[&lucia]).await.unwrap();

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;

        // A person found by name opens a new direct message, synced at once.
        let picks = orb.service.find("lucia").await.unwrap();
        let person = picks
            .iter()
            .find(|p| p.kind == "person")
            .expect("Lucia is found");
        let direct = orb
            .service
            .open_direct(person.username.as_deref().unwrap())
            .await
            .unwrap();
        assert!(orb.service.rooms().unwrap().iter().any(|r| r.rid == direct));

        // Several people at once: one conversation with all of them, the same
        // one when asked again.
        let sara = server.user("sara").await.unwrap();
        let people = vec![lucia.username.clone(), sara.username.clone()];
        let group = orb.service.open_group(&people).await.unwrap();
        assert_ne!(group, direct);
        let room = orb
            .service
            .rooms()
            .unwrap()
            .into_iter()
            .find(|r| r.rid == group)
            .expect("the group conversation is synced");
        assert_eq!(room.kind, "d");
        assert_eq!(orb.service.open_group(&people).await.unwrap(), group);

        // A public room the user is not in is offered for joining.
        let room_name = {
            let picks = orb.service.find("room-").await.unwrap();
            let join = picks
                .iter()
                .find(|p| p.rid.as_deref() == Some(rid.as_str()))
                .expect("the channel is found");
            assert_eq!(join.kind, "join");
            join.label.clone()
        };
        orb.service.join(&rid).await.unwrap();
        assert!(
            orb.service.rooms().unwrap().iter().any(|r| r.rid == rid),
            "{room_name} is joined"
        );

        server.send(&lucia, &rid, None, "Welcome!").await.unwrap();
        let threads = eventually(15, || {
            Some(orb.service.room_threads(&rid).unwrap()).filter(|t| !t.is_empty())
        })
        .await;
        assert_eq!(threads[0].thread, "", "the room's own timeline leads");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn presence_follows_the_stream_and_rooms_are_created_and_left() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let lukas = server.user("lukas").await.unwrap();
        let rid = server.direct(&lukas, &sam).await.unwrap();
        server.send(&lukas, &rid, None, "Hi").await.unwrap();

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;
        eventually(15, || {
            orb.service
                .rooms()
                .unwrap()
                .into_iter()
                .find(|r| r.rid == rid)
        })
        .await;

        // Lukas comes online with a session of his own, and then steps away:
        // both reach the direct message's room. (Without a connection the
        // server keeps him offline whatever he sets.)
        let lukas_orb = orb_for(&server, &lukas);
        eventually(60, || {
            (lukas_orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;
        eventually(15, || {
            orb.service
                .rooms()
                .unwrap()
                .into_iter()
                .find(|r| r.rid == rid && r.presence.as_deref() == Some("online"))
        })
        .await;
        server.set_status(&lukas, "away").await.unwrap();
        eventually(15, || {
            orb.service
                .rooms()
                .unwrap()
                .into_iter()
                .find(|r| r.rid == rid && r.presence.as_deref() == Some("away"))
        })
        .await;

        // A room created here is synced at once.
        let name = format!("made-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
        let made = orb.service.create_room(&name, false).await.unwrap();
        assert!(orb.service.rooms().unwrap().iter().any(|r| r.rid == made));
        // Its last owner cannot leave it: the server says why.
        let refused = orb.service.leave(&made).await.unwrap_err();
        assert!(refused.to_string().contains("last owner"), "{refused}");

        // Leaving someone else's room forgets it here.
        let theirs = server.channel(&[&lukas, &sam]).await.unwrap();
        eventually(15, || {
            orb.service
                .rooms()
                .unwrap()
                .into_iter()
                .find(|r| r.rid == theirs)
        })
        .await;
        assert_eq!(orb.service.leave(&theirs).await.unwrap(), "c");
        assert!(orb.service.rooms().unwrap().iter().all(|r| r.rid != theirs));
        // Undone: joined again, and synced at once.
        orb.service.rejoin(&theirs, "c").await.unwrap();
        assert!(orb.service.rooms().unwrap().iter().any(|r| r.rid == theirs));

        // A direct message hidden comes back the same way.
        assert_eq!(orb.service.leave(&rid).await.unwrap(), "d");
        assert!(orb.service.rooms().unwrap().iter().all(|r| r.rid != rid));
        orb.service.rejoin(&rid, "d").await.unwrap();
        assert!(orb.service.rooms().unwrap().iter().any(|r| r.rid == rid));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn files_edits_reactions_and_deletions_reach_the_server_and_come_back() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let sara = server.user("sara").await.unwrap();
        let rid = server.channel(&[&sara, &sam]).await.unwrap();
        let root = server.send(&sara, &rid, None, "The log?").await.unwrap();

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;
        eventually(15, || {
            Some(waiting_in(&orb, &rid)).filter(|r| !r.is_empty())
        })
        .await;

        // A task made from the room's newest message links it by room and id.
        let source = orb.service.task_source(&rid, "", None).unwrap();
        assert!(
            source.link.starts_with("chat://") && source.link.ends_with(&format!("/{rid}/{root}")),
            "{}",
            source.link
        );
        assert!(source.permalink.ends_with(&format!("?msg={root}")));
        assert_eq!(source.text, "The log?");

        // A file into a thread, the text as its message.
        let file = orb_core::chat::ChatUpload {
            name: "notarize.log".into(),
            mime: "text/plain".into(),
            bytes: b"codesign failed".to_vec(),
        };
        orb.service
            .send_files(&rid, &root, vec![file], "Here it is")
            .await
            .unwrap();
        let thread = orb
            .service
            .conversation(&rid, &root)
            .await
            .unwrap()
            .unwrap();
        let mine = thread
            .messages
            .iter()
            .find(|m| m.mine)
            .expect("the upload is in the thread");
        assert_eq!(
            mine.files.first().map(|f| f.name.as_str()),
            Some("notarize.log")
        );
        assert_eq!(mine.body, "Here it is");
        let (name, bytes) = orb.service.file(&mine.id, 0).await.unwrap();
        assert_eq!(
            (name.as_str(), bytes.as_slice()),
            ("notarize.log", &b"codesign failed"[..])
        );

        // Edited, reacted to, then deleted.
        orb.service
            .edit(&rid, &mine.id, "Here it is, from CI")
            .await
            .unwrap();
        orb.service.react(&rid, &root, ":+1:", true).await.unwrap();
        let thread = orb
            .service
            .conversation(&rid, &root)
            .await
            .unwrap()
            .unwrap();
        let edited = thread.messages.iter().find(|m| m.id == mine.id).unwrap();
        assert_eq!(edited.body, "Here it is, from CI");
        assert!(edited.edited);
        let first = thread.messages.iter().find(|m| m.id == root).unwrap();
        assert_eq!(
            first.reactions.first().map(|r| (r.emoji.as_str(), r.mine)),
            Some((":+1:", true))
        );

        orb.service.delete(&rid, &mine.id).await.unwrap();
        let thread = orb
            .service
            .conversation(&rid, &root)
            .await
            .unwrap()
            .unwrap();
        assert!(thread.messages.iter().all(|m| m.id != mine.id));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn another_persons_reaction_to_an_older_message_comes_back() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let sara = server.user("sara").await.unwrap();
        let rid = server.channel(&[&sara, &sam]).await.unwrap();
        let first = server
            .send(&sam, &rid, None, "Lunch at noon?")
            .await
            .unwrap();
        let second = server.send(&sam, &rid, None, "Or at one").await.unwrap();
        server.send(&sara, &rid, None, "Later today").await.unwrap();

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;
        eventually(15, || {
            Some(waiting_in(&orb, &rid)).filter(|r| !r.is_empty())
        })
        .await;
        let reactions_of = |db_path: &str, id: &str| -> Option<String> {
            let conn = rusqlite::Connection::open(db_path).ok()?;
            conn.query_row(
                "SELECT reactions FROM chat_messages WHERE id = ?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        };

        // Not the room's newest message: only the stream says it changed.
        server.react(&sara, &first, ":+1:").await.unwrap();
        let reactions = eventually(20, || reactions_of(&orb.db_path, &first)).await;
        assert!(reactions.contains("sara"), "{reactions}");

        // Reacted to while Orb is not running, the room looks synced to the
        // next pass; opening the conversation asks the server.
        let mut orb = orb;
        let dir = orb.dir.take().unwrap();
        let db_path = orb.db_path.clone();
        drop(orb);
        tokio::time::sleep(Duration::from_secs(1)).await;
        server.react(&sara, &second, ":tada:").await.unwrap();
        let orb = orb_in(&server, &sam, dir);
        assert_eq!(orb.db_path, db_path);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;
        orb.service.conversation(&rid, "").await.unwrap();
        let reactions = eventually(20, || reactions_of(&orb.db_path, &second)).await;
        assert!(reactions.contains("sara"), "{reactions}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn a_thread_that_starts_waiting_is_announced_unless_its_room_is_muted() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let sara = server.user("sara").await.unwrap();
        let loud = server.channel(&[&sara, &sam]).await.unwrap();
        let quiet = server.channel(&[&sara, &sam]).await.unwrap();
        server
            .set_notifications(&sam, &quiet, "nothing")
            .await
            .unwrap();

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;
        let since = orb.service.waiting_watermark();

        server.send(&sara, &quiet, None, "Shh").await.unwrap();
        server
            .send(&sara, &loud, None, "Deploy at four?")
            .await
            .unwrap();
        eventually(15, || {
            Some(waiting_in(&orb, &loud)).filter(|r| !r.is_empty())
        })
        .await;
        eventually(15, || {
            Some(waiting_in(&orb, &quiet)).filter(|r| !r.is_empty())
        })
        .await;
        let (announced, watermark) = orb.service.new_waiting_since(since).unwrap();
        let rooms: Vec<&str> = announced.iter().map(|t| t.rid.as_str()).collect();
        assert_eq!(rooms, [loud.as_str()]);
        assert!(watermark > since);
        let (again, _) = orb.service.new_waiting_since(watermark).unwrap();
        assert!(again.is_empty(), "announced once");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker or ORB_TEST_ROCKETCHAT"]
async fn a_room_muted_in_orb_is_quiet_until_it_is_called() {
    bounded(async {
        let server = RocketChat::start().await.expect("Rocket.Chat starts");
        let sam = server.user("sam").await.unwrap();
        let sara = server.user("sara").await.unwrap();
        let room = server.channel(&[&sara, &sam]).await.unwrap();

        let orb = orb_for(&server, &sam);
        eventually(60, || {
            (orb.service.status() == ChatStatus::Live).then_some(())
        })
        .await;
        orb.service.mute(&room, None).unwrap();
        let since = orb.service.waiting_watermark();

        server.send(&sara, &room, None, "Shh").await.unwrap();
        let rows = eventually(15, || {
            Some(waiting_in(&orb, &room)).filter(|r| !r.is_empty())
        })
        .await;
        assert!(rows[0].muted, "it waits, muted");
        let (announced, watermark) = orb.service.new_waiting_since(since).unwrap();
        assert!(announced.is_empty(), "a muted room is quiet");

        server
            .send(&sara, &room, None, "@here deploy at four?")
            .await
            .unwrap();
        eventually(15, || {
            Some(waiting_in(&orb, &room)).filter(|r| r.iter().any(|t| !t.muted))
        })
        .await;
        let (announced, _) = orb.service.new_waiting_since(watermark).unwrap();
        let rooms: Vec<&str> = announced.iter().map(|t| t.rid.as_str()).collect();
        assert_eq!(rooms, [room.as_str()], "@here comes through");

        orb.service.unmute(&room).unwrap();
        assert!(!waiting_in(&orb, &room)[0].room.muted);
    })
    .await;
}
