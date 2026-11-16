//! Cold and warm sync timings against the real accounts.
//!
//! Ignored by default: it reads the machine's own `~/.config/orb/config.yml`
//! (or the file `ORB_CONFIG` names) and talks to the real servers. It only
//! reads from them, and it syncs into a scratch database that is deleted
//! afterwards; the real database is not touched. Run with
//! `cargo test --test live_sync_timing -- --ignored --nocapture`.
//!
//! What it prints is the baseline the plan keeps: how long a sync of every
//! folder of every account takes from nothing, and how long it takes when
//! nothing has changed since.

use std::time::Instant;

use orb_core::Store;
use orb_core::imap::trace;

#[tokio::test]
#[ignore]
async fn cold_and_warm_sync_of_the_real_accounts() {
    let path = std::env::var("ORB_CONFIG").unwrap_or_else(|_| "~/.config/orb/config.yml".into());
    let mut config = orb_core::config::read_from_path(&path).expect("read the config");

    let scratch = tempfile::tempdir().expect("scratch directory");
    config.db_path = scratch
        .path()
        .join("scratch.db")
        .to_string_lossy()
        .into_owned();
    let log = scratch.path().join("trace.log");
    config.log_file = Some(log.to_string_lossy().into_owned());
    orb_core::logger::init(config.log_file.clone());

    let store = Store::new(config).expect("open the scratch store");
    for label in ["cold", "warm"] {
        let started = Instant::now();
        let report = store.sync(None, None, None, false).await.expect("sync");
        println!(
            "{label}: {:?} accounts={} folders={} added={} updated={} deleted={} errors={:?}",
            started.elapsed(),
            report.accounts_synced,
            report.folders_synced,
            report.messages_added,
            report.messages_updated,
            report.messages_deleted,
            report.errors
        );
    }

    println!("{}", trace::report());
    println!("--- trace log ---");
    println!(
        "{}",
        std::fs::read_to_string(&log).unwrap_or_else(|_| "(no log written)".into())
    );
}
