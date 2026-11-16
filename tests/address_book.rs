//! Building the address book from a real mailbox.
//!
//! Ignored by default: it needs this machine's own database. Run with
//! `cargo test --test address_book -- --ignored --nocapture`.
//!
//! Works on a copy, so it neither holds the running app's write lock nor
//! changes anything the app is using. What it answers is whether the rebuild
//! is fast enough to run at startup, and whether the suggestions it produces
//! are the ones a person would expect — neither of which a fixture can say.

use orb_core::db::Database;

fn copy_of_the_real_database() -> Option<(tempfile::TempDir, String)> {
    let config = orb_core::config::read().ok()?;
    let source = shellexpand::tilde(&config.db_path).to_string();
    if !std::path::Path::new(&source).exists() {
        eprintln!("no database at {source}; skipping");
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    let target = dir.path().join("copy.sqlite");
    std::fs::copy(&source, &target).ok()?;
    // The write-ahead log holds anything the running app has not checkpointed.
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::copy(
            format!("{source}{suffix}"),
            format!("{}{suffix}", target.display()),
        );
    }
    let path = target.to_str()?.to_string();
    Some((dir, path))
}

#[test]
#[ignore]
fn rebuilding_from_a_real_mailbox() {
    let Some((_dir, path)) = copy_of_the_real_database() else {
        return;
    };
    let mut db = Database::new(&path).expect("open");

    let config = orb_core::config::read().expect("config");
    let sent: Vec<i64> = config
        .accounts
        .iter()
        .flat_map(|account| {
            db.get_folders_by_account(&account.name)
                .expect("folders")
                .into_iter()
                .filter(|f| f.name.eq_ignore_ascii_case(&account.sent_folder))
                .filter_map(|f| f.id)
                .inspect(|id| {
                    println!(
                        "sent folder: {} / {} (id {id})",
                        account.name, account.sent_folder
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(!sent.is_empty(), "the accounts must have sent folders");

    let started = std::time::Instant::now();
    let total = db.learn_correspondents(&sent).expect("first pass");
    println!("first pass: {total:?} in {:?}", started.elapsed());
    assert!(
        total.is_some_and(|t| t > 0),
        "a real mailbox has correspondents"
    );

    // The pass that runs on every launch. With no new mail there is nothing to
    // do, and saying so is the whole point — the alternative is re-reading a
    // 1.4 GB database every time the app opens.
    let started = std::time::Instant::now();
    let again = db.learn_correspondents(&sent).expect("second pass");
    println!("second pass: {again:?} in {:?}", started.elapsed());
    assert_eq!(again, None, "nothing arrived between the two passes");

    // Who the ranking actually puts first, for a few things a person would type.
    let service = orb_core::ContactService::new(&path, &config.contacts);
    for query in ["an", "ma", "a"] {
        println!("\n{query:?}");
        for s in service.suggest(query, 6) {
            println!(
                "  {:>5}  {:<34} {}",
                s.score,
                s.recipients
                    .iter()
                    .map(|r| r.address.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                s.name.as_deref().unwrap_or("")
            );
        }
    }
}

/// The exact path the compose window takes, against the real config and the
/// real database — so a failure here is the backend's and a failure only in the
/// window is the window's.
#[test]
#[ignore]
fn suggesting_recipients_the_way_the_window_asks() {
    let config_path = shellexpand::tilde("~/.config/orb/config.yml").to_string();
    let handle = orb_core::ffi::init(config_path).expect("init");

    // An empty query is the list that opens on Down, before anything is typed:
    // the people actually written to.
    let usual = handle.suggest_recipients(String::new(), 8);
    println!("\nbefore typing anything → {} suggestions", usual.len());
    for s in &usual {
        println!("  {:>5}  {}", s.score, s.recipients[0].address);
    }
    assert!(
        !usual.is_empty(),
        "a real mailbox has people who are written to"
    );

    for query in ["s", "an", "ma"] {
        let found = handle.suggest_recipients(query.to_string(), 8);
        println!("\n{query:?} → {} suggestions", found.len());
        for s in &found {
            println!(
                "  {:>5}  {:?}  {}",
                s.score,
                s.source,
                s.recipients
                    .iter()
                    .map(|r| r.address.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        assert!(!found.is_empty(), "{query:?} must suggest somebody");
    }
}

/// Read-only: what the server holds in Work/INBOX versus what is stored.
#[test]
#[ignore]
fn what_the_inbox_actually_holds() {
    let config = orb_core::config::read().expect("config");
    let account = config
        .accounts
        .iter()
        .find(|a| a.name == "Work")
        .expect("Work account");

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let password = account
            .get_imap_password()
            .expect("password")
            .expect("a password command");
        let mut client = orb_core::imap::ImapClient::connect(
            &account.imap_server,
            account.imap_port,
            &account.imap_username,
            &password,
            "address book test",
        )
        .await
        .expect("connect");
        client.select_mailbox("INBOX").await.expect("select");
        let uids = client.fetch_all_uids().await.expect("uids");
        println!("server INBOX holds {} messages: {uids:?}", uids.len());

        let info = client.current_mailbox();
        println!(
            "uidnext={:?} uidvalidity={:?}",
            info.and_then(|m| m.uid_next),
            info.and_then(|m| m.uid_validity)
        );
    });
}

/// Read-only: ask the real server what has vanished from Work/INBOX since the
/// modseq stored locally, with the UIDs stored locally.
///
/// This is the case that stalled: the server answers with a range covering
/// everything, which used to become two billion UIDs and a twenty-second wait.
#[test]
#[ignore]
fn what_the_server_says_has_vanished() {
    let config = orb_core::config::read().expect("config");
    let account = config
        .accounts
        .iter()
        .find(|a| a.name == "Work")
        .expect("Work account");

    let db = orb_core::db::Database::new(&shellexpand::tilde(&config.db_path)).expect("db");
    let folder = db
        .get_folders_by_account("Work")
        .expect("folders")
        .into_iter()
        .find(|f| f.name == "INBOX")
        .expect("inbox");
    let stored = db.get_folder_uids(folder.id.expect("id")).expect("uids");
    println!("stored: {stored:?}, modseq {:?}", folder.highest_modseq);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let password = account
            .get_imap_password()
            .expect("password")
            .expect("a password command");
        let mut client = orb_core::imap::ImapClient::connect(
            &account.imap_server,
            account.imap_port,
            &account.imap_username,
            &password,
            "address book test",
        )
        .await
        .expect("connect");
        client.select_mailbox("INBOX").await.expect("select");

        let started = std::time::Instant::now();
        let gone = client
            .fetch_vanished_uids(
                folder.uidvalidity.unwrap_or(1),
                folder.highest_modseq.unwrap_or(0),
                &stored,
            )
            .await
            .expect("vanished");
        println!("vanished: {gone:?} in {:?}", started.elapsed());
        assert_eq!(gone, stored, "the inbox is empty on the server");
    });
}

/// Read-only: collecting every UID of the largest real folder, which is what a
/// whole-folder selection does before acting.
#[test]
#[ignore]
fn collecting_every_uid_of_a_large_folder() {
    let config = orb_core::config::read().expect("config");
    let store = orb_core::store::Store::new(config.clone()).expect("store");

    for (account, folder) in [("Work", "Archive"), ("Private", "Archive")] {
        let started = std::time::Instant::now();
        let uids = store.folder_uids(account, folder).expect("uids");
        println!(
            "{account}/{folder}: {} uids in {:?}",
            uids.len(),
            started.elapsed()
        );
    }
}

/// Read-only: how far the stored flags have drifted from the server's.
///
/// The Private account's server advertises CONDSTORE and never advances its
/// HIGHESTMODSEQ, so flag changes made elsewhere are invisible to a sync that
/// asks "what changed since". This measures the damage.
#[test]
#[ignore]
fn how_far_the_flags_have_drifted() {
    let config = orb_core::config::read().expect("config");
    let db = orb_core::db::Database::new(&shellexpand::tilde(&config.db_path)).expect("db");
    let rt = tokio::runtime::Runtime::new().expect("runtime");

    for account in &config.accounts {
        let password = account
            .get_imap_password()
            .expect("password")
            .expect("a password command");
        for folder_name in ["INBOX", "Archive"] {
            let Some(folder) = db
                .get_folders_by_account(&account.name)
                .expect("folders")
                .into_iter()
                .find(|f| f.name == folder_name)
            else {
                continue;
            };
            let folder_id = folder.id.expect("id");
            let mut stored = db.get_folder_uids(folder_id).expect("uids");
            stored.sort_unstable();
            // The most recent thousand, which is where flags actually change.
            let window: Vec<u32> = stored.iter().rev().take(1000).copied().collect();
            if window.is_empty() {
                continue;
            }

            rt.block_on(async {
                let mut client = orb_core::imap::ImapClient::connect(
                    &account.imap_server,
                    account.imap_port,
                    &account.imap_username,
                    &password,
                    "address book test",
                )
                .await
                .expect("connect");
                client.select_mailbox(folder_name).await.expect("select");

                let started = std::time::Instant::now();
                let server = client.fetch_flags(&window).await.expect("flags");
                let elapsed = started.elapsed();

                let ours_all = db.folder_flags(folder_id).expect("stored flags");
                let mut drifted = 0;
                let mut examples = Vec::new();
                for uid in &window {
                    let Some(theirs) = server.get(uid) else {
                        continue;
                    };
                    let empty = Vec::new();
                    let ours_sorted = ours_all.get(uid).unwrap_or(&empty);
                    if ours_sorted != theirs {
                        drifted += 1;
                        if examples.len() < 4 {
                            examples
                                .push(format!("uid {uid}: ours {ours_sorted:?} server {theirs:?}"));
                        }
                    }
                }
                println!(
                    "{}/{folder_name}: {drifted} of {} drifted (flags fetched in {elapsed:?})",
                    account.name,
                    window.len()
                );
                for e in examples {
                    println!("    {e}");
                }
            });
        }
    }
}

/// A flag changed by another client, on the account whose server never
/// advances its HIGHESTMODSEQ.
///
/// Uses a custom keyword rather than `\Seen`, so nothing the user would notice
/// changes: read state, flag state and everything else are left alone. It is
/// compared the same way regardless — the reconciliation compares whole flag
/// lists — and it is removed again at the end.
///
/// Run with `cargo test --test address_book -- --ignored --nocapture drift`.
#[test]
#[ignore]
fn a_flag_set_elsewhere_is_noticed() {
    const PROBE: &str = "$OrbFlagProbe";
    const ACCOUNT: &str = "Private";
    const FOLDER: &str = "INBOX";

    let config = orb_core::config::read().expect("config");
    let account = config
        .accounts
        .iter()
        .find(|a| a.name == ACCOUNT)
        .expect("the account")
        .clone();
    let db_path = shellexpand::tilde(&config.db_path).to_string();
    let store = orb_core::store::Store::new(config.clone()).expect("store");
    let rt = tokio::runtime::Runtime::new().expect("runtime");

    let folder = {
        let db = orb_core::db::Database::new(&db_path).expect("db");
        db.get_folders_by_account(ACCOUNT)
            .expect("folders")
            .into_iter()
            .find(|f| f.name == FOLDER)
            .expect("the folder")
    };
    let folder_id = folder.id.expect("id");
    println!(
        "{ACCOUNT}/{FOLDER}: stored modseq {:?} — the value that never moves",
        folder.highest_modseq
    );

    let uid = {
        let db = orb_core::db::Database::new(&db_path).expect("db");
        *db.get_folder_uids(folder_id)
            .expect("uids")
            .last()
            .expect("at least one message")
    };
    println!("probing uid {uid}");

    let flags_now = || {
        let db = orb_core::db::Database::new(&db_path).expect("db");
        db.folder_flags(folder_id)
            .expect("flags")
            .get(&uid)
            .cloned()
            .unwrap_or_default()
    };
    let before = flags_now();
    println!("stored before: {before:?}");

    let connect = || async {
        let password = account
            .get_imap_password()
            .expect("password")
            .expect("a password command");
        let mut client = orb_core::imap::ImapClient::connect(
            &account.imap_server,
            account.imap_port,
            &account.imap_username,
            &password,
            "address book test",
        )
        .await
        .expect("connect");
        client.select_mailbox(FOLDER).await.expect("select");
        client
    };

    // Another client sets a flag.
    rt.block_on(async {
        let mut other = connect().await;
        other
            .store_flags(&[uid], &[PROBE], true)
            .await
            .expect("set the probe flag");
    });

    // Make the folder due for a check, then sync as the app does on launch.
    {
        let db = orb_core::db::Database::new(&db_path).expect("db");
        db.clear_flags_checked(folder_id).expect("clear");
    }
    rt.block_on(async {
        store
            .sync(Some(ACCOUNT.into()), Some(FOLDER.into()), None, false)
            .await
            .expect("sync");
    });

    let after = flags_now();
    println!("stored after sync: {after:?}");

    // Put the mailbox back exactly as it was, whatever the assertion says.
    rt.block_on(async {
        let mut other = connect().await;
        other
            .store_flags(&[uid], &[PROBE], false)
            .await
            .expect("remove the probe flag");
    });
    {
        let db = orb_core::db::Database::new(&db_path).expect("db");
        db.clear_flags_checked(folder_id).expect("clear");
    }
    rt.block_on(async {
        store
            .sync(Some(ACCOUNT.into()), Some(FOLDER.into()), None, false)
            .await
            .expect("resync");
    });
    let cleaned = flags_now();
    println!("stored after cleanup: {cleaned:?}");

    assert!(
        after.iter().any(|f| f == PROBE),
        "a flag set by another client must reach the database: {after:?}"
    );
    // And the other direction: a flag *removed* elsewhere must also arrive.
    // This is the case that gating on the modseq missed — the removal moved the
    // modseq and CONDSTORE still said nothing.
    assert_eq!(cleaned, before, "a flag removed elsewhere must also arrive");
}

/// Read-only: what the server holds for the probed message.
#[test]
#[ignore]
fn what_the_server_holds_for_the_probe() {
    let config = orb_core::config::read().expect("config");
    let account = config
        .accounts
        .iter()
        .find(|a| a.name == "Private")
        .expect("account");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let password = account
            .get_imap_password()
            .expect("password")
            .expect("a password command");
        let mut client = orb_core::imap::ImapClient::connect(
            &account.imap_server,
            account.imap_port,
            &account.imap_username,
            &password,
            "address book test",
        )
        .await
        .expect("connect");
        client.select_mailbox("INBOX").await.expect("select");
        let flags = client.fetch_flags(&[69719]).await.expect("flags");
        println!("server flags for 69719: {flags:?}");
    });
}

/// What a full sync costs now that flags are verified on a schedule.
#[test]
#[ignore]
fn what_a_full_sync_costs() {
    let config = orb_core::config::read().expect("config");
    let store = orb_core::store::Store::new(config).expect("store");
    let rt = tokio::runtime::Runtime::new().expect("runtime");

    // Twice: the first pass checks every folder's flags, the second finds them
    // all checked within the interval and skips — which is the steady state.
    for pass in 1..=2 {
        let started = std::time::Instant::now();
        let report = rt
            .block_on(store.sync(None, None, None, false))
            .expect("sync");
        println!(
            "pass {pass}: {:?} — {} folders, +{} new, {} updated, {} removed",
            started.elapsed(),
            report.folders_synced,
            report.messages_added,
            report.messages_updated,
            report.messages_deleted
        );
    }
}

/// The cost of the flag sweep alone, with every folder due.
#[test]
#[ignore]
fn what_the_flag_sweep_costs() {
    let config = orb_core::config::read().expect("config");
    let store = orb_core::store::Store::new(config).expect("store");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let started = std::time::Instant::now();
    let report = rt
        .block_on(store.sync(None, None, None, false))
        .expect("sync");
    println!(
        "every folder due: {:?} — {} corrected",
        started.elapsed(),
        report.messages_updated
    );
}
