//! The second-connection measurement of `tests/sync_scale.rs`, over a real
//! network, where a new connection costs a TLS handshake and a login.
//!
//! Ignored by default, and only ever run against the account `ORB_CONFIG`
//! names — never the machine's own config: it creates a scratch folder on
//! that account, fills it, and deletes it again. Run with
//! `ORB_CONFIG=… cargo test --test live_second_connection -- --ignored --nocapture`.
//! `ORB_LIVE_MESSAGES` sets the folder size (default 2 000).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use orb_core::Store;
use orb_core::imap::{ConnectionSettings, ImapClient, trace};

const FOLDER: &str = "OrbScratchMeasurement";

fn message(n: usize) -> Vec<u8> {
    format!(
        "From: Orb Measurement <measurement@orb.invalid>\r\n\
         To: measurement@orb.invalid\r\n\
         Subject: Measurement message {n}\r\n\
         Date: Thu, 15 Oct 2026 10:00:00 +0200\r\n\
         Message-ID: <measurement-{n}@orb.invalid>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n\
         {}\r\n",
        "A line of body text for the backfill to fetch. ".repeat(20)
    )
    .into_bytes()
}

#[tokio::test]
#[ignore = "talks to the account ORB_CONFIG names"]
async fn a_second_connection_over_the_network() {
    let path = std::env::var("ORB_CONFIG")
        .expect("ORB_CONFIG must name the test account's config; this test writes to it");
    let count: usize = std::env::var("ORB_LIVE_MESSAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let base = orb_core::config::read_from_path(&path).expect("read the config");
    let account = base.accounts.first().expect("an account").clone();

    // The scratch folder, filled over one connection.
    let password = Store::new({
        let mut config = base.clone();
        let scratch = tempfile::tempdir().unwrap().keep();
        config.db_path = scratch.join("setup.db").to_string_lossy().into_owned();
        config
    })
    .expect("store")
    .credentials()
    .imap(&account)
    .await
    .expect("password");
    let settings = ConnectionSettings::for_account(&account, password);
    let mut client = ImapClient::connect_with(&settings, "measurement setup")
        .await
        .expect("connect");
    let _ = client.delete_folder(FOLDER).await;
    client
        .create_folder(FOLDER)
        .await
        .expect("create the folder");
    let started = Instant::now();
    for n in 0..count {
        client
            .append_with_flags(FOLDER, &message(n), Some("(\\Seen)"))
            .await
            .expect("append");
    }
    println!(
        "filled {FOLDER} with {count} messages in {:?}",
        started.elapsed()
    );

    let rounds = count.div_ceil(500);
    for kept in [1, 2] {
        let scratch = tempfile::tempdir().expect("scratch");
        let mut config = base.clone();
        config.db_path = scratch.path().join("m.db").to_string_lossy().into_owned();
        let store = Arc::new(Store::new(config).expect("open the store"));
        store.connection_pool().set_idle_per_account(kept);
        store
            .sync(Some(account.name.clone()), None, None, false)
            .await
            .expect("cold sync");

        let opened_before = trace::connections_opened();
        let stop = Arc::new(AtomicBool::new(false));
        let syncer = {
            let (store, stop, name) = (store.clone(), stop.clone(), account.name.clone());
            tokio::spawn(async move {
                let mut took = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    let _ = store
                        .sync(Some(name.clone()), Some(FOLDER.into()), None, false)
                        .await;
                    took.push(started.elapsed());
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                took
            })
        };

        let folders = vec![FOLDER.to_string()];
        let started = Instant::now();
        let mut bodies = 0;
        for _ in 0..rounds {
            while store
                .fetch_pending_shapes(&account.name, &folders, 500)
                .await
                .expect("shapes")
                > 0
            {}
            bodies += store
                .fetch_pending_bodies(&account.name, &folders, 500, None)
                .await
                .expect("bodies")
                .fetched;
        }
        let backfill = started.elapsed();
        stop.store(true, Ordering::Relaxed);
        let syncs = syncer.await.expect("syncer");
        let opened = trace::connections_opened() - opened_before;
        let mean = syncs.iter().sum::<Duration>() / syncs.len().max(1) as u32;
        let worst = syncs.iter().max().copied().unwrap_or_default();
        println!(
            "{kept} kept: backfill of {bodies} bodies {backfill:?}; {} syncs beside it, \
             mean {mean:?}, worst {worst:?}; {opened} connections opened",
            syncs.len()
        );
        store.connection_pool().close_all().await;
    }

    client
        .delete_folder(FOLDER)
        .await
        .expect("delete the folder");
    println!("deleted {FOLDER}");
}
