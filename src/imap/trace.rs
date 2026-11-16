//! What the last sync of every folder did, phase by phase, and where every
//! connection came from.
//!
//! Two registries, both process-wide and both bounded: the last trace per
//! folder, and a ledger of the connections opened since launch. Each phase is
//! also written to the log at debug level as it ends, so a log file holds the
//! whole history and the registries hold what `sync-stats` shows.
//!
//! The sync has been tuned one symptom at a time; this is the instrument that
//! says what it does as a whole, so that every change to it has a before and
//! an after.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::logger;

/// One phase of a sync, as it went.
#[derive(Clone, Debug)]
pub struct Phase {
    pub name: &'static str,
    pub elapsed: Duration,
    /// Bytes on the wire during the phase, both directions, TLS included.
    pub bytes: u64,
    pub messages: Option<usize>,
    pub detail: String,
}

/// A phase that has begun and not yet ended.
#[must_use = "a phase that is started must be ended, or it is not recorded"]
pub struct PhaseStart {
    name: &'static str,
    at: Instant,
    bytes: u64,
}

/// The trace of one sync of one folder, or of the account-level work before
/// any folder (connecting, listing) when `folder` is empty.
#[derive(Clone, Debug)]
pub struct SyncTrace {
    account: String,
    folder: String,
    connection: u64,
    started_at: SystemTime,
    started: Instant,
    phases: Vec<Phase>,
    skipped: Option<String>,
}

impl SyncTrace {
    /// Start tracing. `connection` is the id of the connection the sync holds;
    /// 0 when it has none yet.
    pub fn begin(account: &str, folder: &str, connection: u64) -> Self {
        Self {
            account: account.to_string(),
            folder: folder.to_string(),
            connection,
            started_at: SystemTime::now(),
            started: Instant::now(),
            phases: Vec::new(),
            skipped: None,
        }
    }

    /// The sync is now on another connection, after a reconnect.
    pub fn connection(&mut self, id: u64) {
        self.connection = id;
    }

    /// Begin a phase. `bytes_so_far` is the connection's count at this moment,
    /// so that the phase can report what it alone moved.
    pub fn start(&self, name: &'static str, bytes_so_far: u64) -> PhaseStart {
        PhaseStart {
            name,
            at: Instant::now(),
            bytes: bytes_so_far,
        }
    }

    /// End a phase and write its line to the log.
    pub fn end(
        &mut self,
        start: PhaseStart,
        bytes_so_far: u64,
        messages: Option<usize>,
        detail: impl Into<String>,
    ) {
        let phase = Phase {
            name: start.name,
            elapsed: start.at.elapsed(),
            bytes: bytes_so_far.saturating_sub(start.bytes),
            messages,
            detail: detail.into(),
        };
        logger::debug(&format!(
            "trace[{}] {}: {} {} {}conn=#{}{}",
            self.subject(),
            phase.name,
            format_duration(phase.elapsed),
            format_bytes(phase.bytes),
            phase
                .messages
                .map(|n| format!("messages={n} "))
                .unwrap_or_default(),
            self.connection,
            if phase.detail.is_empty() {
                String::new()
            } else {
                format!(" {}", phase.detail)
            }
        ));
        self.phases.push(phase);
    }

    /// Record something that took no time worth measuring: a retry, a lock
    /// that was not obtained, a decision.
    pub fn note(&mut self, name: &'static str, detail: impl Into<String>) {
        let start = self.start(name, 0);
        self.end(start, 0, None, detail);
    }

    /// The folder was not synced, and this is why.
    pub fn skipped(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.note("skip", reason.clone());
        self.skipped = Some(reason);
    }

    /// The sync ended. Writes the summary line and keeps the trace as the
    /// folder's latest.
    pub fn finish(self, result: Result<(), String>) {
        let outcome = match (&self.skipped, result) {
            (Some(reason), _) => format!("skipped: {reason}"),
            (None, Ok(())) => "ok".to_string(),
            (None, Err(error)) => format!("error: {error}"),
        };
        let total = self.started.elapsed();
        logger::debug(&format!(
            "trace[{}] done: {} total={} bytes={} phases={}",
            self.subject(),
            outcome,
            format_duration(total),
            format_bytes(self.phases.iter().map(|p| p.bytes).sum()),
            self.phases.len()
        ));
        let key = (self.account.clone(), self.folder.clone());
        let recorded = Recorded {
            trace: self,
            outcome,
            total,
        };
        if let Ok(mut traces) = LAST_TRACES.lock() {
            traces.insert(key, recorded);
        }
    }

    fn subject(&self) -> String {
        if self.folder.is_empty() {
            self.account.clone()
        } else {
            format!("{}/{}", self.account, self.folder)
        }
    }
}

/// A finished trace, as the registry holds it.
#[derive(Clone, Debug)]
struct Recorded {
    trace: SyncTrace,
    outcome: String,
    total: Duration,
}

static LAST_TRACES: Mutex<BTreeMap<(String, String), Recorded>> = Mutex::new(BTreeMap::new());

/// Forget every trace. For tests that assert on what one sync recorded.
pub fn clear() {
    if let Ok(mut traces) = LAST_TRACES.lock() {
        traces.clear();
    }
}

/// The phases of the last sync of `folder`, or `None` if it was never synced
/// in this process.
pub fn last_phases(account: &str, folder: &str) -> Option<Vec<Phase>> {
    LAST_TRACES
        .lock()
        .ok()?
        .get(&(account.to_string(), folder.to_string()))
        .map(|recorded| recorded.trace.phases.clone())
}

// ---------------------------------------------------------------------------
// The connection ledger
// ---------------------------------------------------------------------------

/// One connection that was opened, or that failed to open.
#[derive(Clone, Debug)]
pub struct ConnectionRecord {
    pub id: u64,
    /// What the connection is for: who asked, in their words.
    pub reason: String,
    /// Why a new socket was needed rather than an existing one.
    pub cause: String,
    pub server: String,
    pub at: SystemTime,
    pub took: Duration,
    pub outcome: Result<(), String>,
}

#[derive(Default)]
struct Ledger {
    opened: u64,
    failed: u64,
    by_reason: BTreeMap<String, u64>,
    recent: VecDeque<ConnectionRecord>,
}

/// How many recent connections the ledger keeps.
const RECENT_CONNECTIONS: usize = 40;

static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh id for a connection about to be opened.
pub fn next_connection_id() -> u64 {
    NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Note a connection attempt, successful or not.
pub fn record_connection(record: ConnectionRecord) {
    if let Ok(mut guard) = LEDGER.lock() {
        let ledger = guard.get_or_insert_with(Ledger::default);
        match &record.outcome {
            Ok(()) => {
                ledger.opened += 1;
                *ledger.by_reason.entry(record.reason.clone()).or_insert(0) += 1;
            }
            Err(_) => ledger.failed += 1,
        }
        if ledger.recent.len() == RECENT_CONNECTIONS {
            ledger.recent.pop_front();
        }
        ledger.recent.push_back(record);
    }
}

/// How many connections have been opened since launch.
pub fn connections_opened() -> u64 {
    LEDGER
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|ledger| ledger.opened))
        .unwrap_or(0)
}

/// The report `sync-stats` shows: every folder's last trace, then the
/// connection tally.
pub fn report() -> String {
    let mut out = String::new();

    out.push_str("Last sync per folder\n");
    match LAST_TRACES.lock() {
        Ok(traces) if !traces.is_empty() => {
            for recorded in traces.values() {
                let trace = &recorded.trace;
                let subject = if trace.folder.is_empty() {
                    format!("{} (account)", trace.account)
                } else {
                    trace.subject()
                };
                out.push_str(&format!(
                    "\n{subject}  {}  {}  {}  conn #{}\n",
                    format_time(trace.started_at),
                    recorded.outcome,
                    format_duration(recorded.total),
                    trace.connection
                ));
                for phase in &trace.phases {
                    out.push_str(&format!(
                        "  {:<12}{:>9}{:>10}  {}{}\n",
                        phase.name,
                        format_duration(phase.elapsed),
                        format_bytes(phase.bytes),
                        phase
                            .messages
                            .map(|n| format!("messages={n} "))
                            .unwrap_or_default(),
                        phase.detail
                    ));
                }
            }
        }
        _ => out.push_str("\n  (nothing synced yet)\n"),
    }

    out.push_str("\nConnections since launch\n");
    match LEDGER.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(ledger) => {
                out.push_str(&format!(
                    "\n  opened {}  failed {}\n",
                    ledger.opened, ledger.failed
                ));
                for (reason, count) in &ledger.by_reason {
                    out.push_str(&format!("  {count:>6}  {reason}\n"));
                }
                out.push_str("\nMost recent connections\n\n");
                for record in ledger.recent.iter().rev() {
                    out.push_str(&format!(
                        "  #{:<5}{}  {:>8}  {:<10}  {} ({})\n",
                        record.id,
                        format_time(record.at),
                        format_duration(record.took),
                        match &record.outcome {
                            Ok(()) => "ok".to_string(),
                            Err(error) => format!("failed: {error}"),
                        },
                        record.reason,
                        record.cause
                    ));
                }
            }
            None => out.push_str("\n  (none)\n"),
        },
        Err(_) => out.push_str("\n  (unavailable)\n"),
    }

    out
}

fn format_duration(duration: Duration) -> String {
    let millis = duration.as_secs_f64() * 1000.0;
    if millis < 1.0 {
        format!("{}µs", duration.as_micros())
    } else if millis < 1000.0 {
        format!("{millis:.0}ms")
    } else {
        format!("{:.2}s", duration.as_secs_f64())
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.1} MiB", b / (KIB * KIB))
    }
}

fn format_time(at: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(at)
        .format("%H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_measure_their_own_bytes() {
        let mut trace = SyncTrace::begin("acct", "INBOX", 7);
        let select = trace.start("select", 100);
        trace.end(select, 350, None, "exists=3");
        let fetch = trace.start("fetch", 350);
        trace.end(fetch, 1350, Some(3), "");
        trace.finish(Ok(()));

        let phases = last_phases("acct", "INBOX").expect("recorded");
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].name, "select");
        assert_eq!(phases[0].bytes, 250);
        assert_eq!(phases[1].bytes, 1000);
        assert_eq!(phases[1].messages, Some(3));
    }

    #[test]
    fn a_skipped_folder_reports_the_reason() {
        let mut trace = SyncTrace::begin("acct", "Skipped", 1);
        trace.skipped("locked by another instance");
        trace.finish(Ok(()));
        let phases = last_phases("acct", "Skipped").expect("recorded");
        assert_eq!(phases[0].name, "skip");
        assert!(report().contains("skipped: locked by another instance"));
    }

    #[test]
    fn the_ledger_counts_by_reason() {
        let before = connections_opened();
        record_connection(ConnectionRecord {
            id: next_connection_id(),
            reason: "ledger test".into(),
            cause: "direct".into(),
            server: "imap.example.com:993".into(),
            at: SystemTime::now(),
            took: Duration::from_millis(12),
            outcome: Ok(()),
        });
        record_connection(ConnectionRecord {
            id: next_connection_id(),
            reason: "ledger test".into(),
            cause: "direct".into(),
            server: "imap.example.com:993".into(),
            at: SystemTime::now(),
            took: Duration::from_millis(12),
            outcome: Err("refused".into()),
        });
        assert_eq!(connections_opened(), before + 1);
        let report = report();
        assert!(report.contains("ledger test"), "{report}");
        assert!(report.contains("failed: refused"), "{report}");
    }

    #[test]
    fn durations_and_sizes_read_naturally() {
        assert_eq!(format_duration(Duration::from_micros(400)), "400µs");
        assert_eq!(format_duration(Duration::from_millis(42)), "42ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.50s");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
