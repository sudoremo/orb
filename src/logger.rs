use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Bytes a log file may reach before it is rotated: the file becomes `.1`,
/// replacing the previous `.1`, and a fresh file takes its place. Two files
/// bound the disk at twice this, which is weeks of ordinary use.
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Writes between two looks at the file's size, so that rotation costs a
/// `metadata` call now and then rather than on every line.
const SIZE_CHECK_EVERY: u32 = 256;

/// Lines kept in memory for the console, with or without a log file.
const RECENT_LINES: usize = 500;

struct Logger {
    file: Option<File>,
    path: Option<String>,
    writes_since_check: u32,
    recent: VecDeque<String>,
}

/// Global logger instance - holds the file handle for efficient logging
static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

/// Initialize the logger with an optional log file path
///
/// Returns true if initialization succeeded, false if already initialized
pub fn init(log_file: Option<String>) -> bool {
    let file = log_file.as_deref().and_then(open_for_append);
    LOGGER
        .set(Mutex::new(Logger {
            file,
            path: log_file,
            writes_since_check: 0,
            recent: VecDeque::with_capacity(RECENT_LINES),
        }))
        .is_ok()
}

fn open_for_append(path: &str) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Where the log is written, if anywhere.
pub fn path() -> Option<String> {
    LOGGER
        .get()
        .and_then(|logger| logger.lock().ok())
        .and_then(|logger| logger.path.clone())
}

/// The last `count` lines logged by this process, oldest first.
pub fn recent(count: usize) -> Vec<String> {
    LOGGER
        .get()
        .and_then(|logger| logger.lock().ok())
        .map(|logger| {
            logger
                .recent
                .iter()
                .rev()
                .take(count)
                .rev()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Log every panic, on whatever thread, before the default hook prints it.
///
/// A panic on a thread nobody joins — the address book, notifications, the
/// editor's reader — otherwise ends that thread with a line on a stderr that
/// a Finder launch sends nowhere, and the feature it ran simply stops.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Without waiting for the log: a panic while the log itself is being
        // written holds its lock on this very thread.
        let line = panic_line(
            std::thread::current().name(),
            info.location()
                .map(|l| format!("{}:{}", l.file(), l.line())),
            info.payload(),
        );
        if LOGGER.get().is_some_and(|logger| logger.try_lock().is_ok()) {
            warn(&line);
        }
        default(info);
    }));
}

/// The log line for a panic: where, on which thread, and what it said.
fn panic_line(
    thread: Option<&str>,
    location: Option<String>,
    payload: &(dyn std::any::Any + Send),
) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_string());
    format!(
        "panic on thread '{}' at {}: {message}",
        thread.unwrap_or("unnamed"),
        location.as_deref().unwrap_or("an unknown place")
    )
}

/// Log a warning message to the log file (if configured)
pub fn warn(message: &str) {
    log_message("WARN", message);
}

/// Log an info message to the log file (if configured)
pub fn info(message: &str) {
    log_message("INFO", message);
}

/// Log a debug message to the log file (if configured)
pub fn debug(message: &str) {
    log_message("DEBUG", message);
}

/// Internal logging function - writes to cached file handle
fn log_message(level: &str, message: &str) {
    let Some(mutex) = LOGGER.get() else { return };
    let Ok(mut logger) = mutex.lock() else { return };
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = format!("[{}] {}: {}", timestamp, level, message);

    if logger.recent.len() == RECENT_LINES {
        logger.recent.pop_front();
    }
    logger.recent.push_back(line.clone());

    if logger.file.is_some() {
        logger.writes_since_check += 1;
        if logger.writes_since_check >= SIZE_CHECK_EVERY {
            logger.writes_since_check = 0;
            rotate_if_large(&mut logger);
        }
    }
    if let Some(file) = logger.file.as_mut() {
        let _ = writeln!(file, "{line}");
    }
}

/// Turn a log file past the bound into `.1` and start a new one. A rotation
/// that fails leaves the file as it is; the log keeps growing rather than
/// stopping.
fn rotate_if_large(logger: &mut Logger) {
    let Some(path) = logger.path.clone() else {
        return;
    };
    let Ok(size) = std::fs::metadata(&path).map(|m| m.len()) else {
        return;
    };
    if size < LOG_MAX_BYTES {
        return;
    }
    let rotated = format!("{path}.1");
    if std::fs::rename(&path, &rotated).is_ok() {
        logger.file = open_for_append(&path);
        if let Some(file) = logger.file.as_mut() {
            let _ = writeln!(
                file,
                "[{}] INFO: log rotated; the previous {} bytes are in {rotated}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                size
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_is_logged_with_its_thread_place_and_message() {
        let text: Box<dyn std::any::Any + Send> = Box::new("index out of range");
        assert_eq!(
            panic_line(
                Some("contacts"),
                Some("src/contacts.rs:80".into()),
                text.as_ref()
            ),
            "panic on thread 'contacts' at src/contacts.rs:80: index out of range"
        );
        let formatted: Box<dyn std::any::Any + Send> = Box::new(String::from("bad 7"));
        assert_eq!(
            panic_line(None, None, formatted.as_ref()),
            "panic on thread 'unnamed' at an unknown place: bad 7"
        );
    }

    /// A file past the bound becomes `.1` and a fresh one takes its place,
    /// with the lines that follow in the new file.
    #[test]
    fn a_large_log_is_rotated_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("orb.log");
        std::fs::write(&path, vec![b'x'; (LOG_MAX_BYTES + 1) as usize]).expect("big log");
        let mut logger = Logger {
            file: open_for_append(path.to_str().unwrap()),
            path: Some(path.to_str().unwrap().to_string()),
            writes_since_check: 0,
            recent: VecDeque::new(),
        };
        rotate_if_large(&mut logger);
        let rotated = dir.path().join("orb.log.1");
        assert!(rotated.exists(), "the old log is kept as .1");
        assert!(std::fs::metadata(&path).unwrap().len() < 200, "a fresh log");
        rotate_if_large(&mut logger);
        assert!(
            std::fs::metadata(&rotated).unwrap().len() > LOG_MAX_BYTES,
            "a small log is left alone"
        );
    }
}
