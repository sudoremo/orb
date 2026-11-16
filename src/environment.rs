//! The environment a launch from the Finder does not get.
//!
//! An app opened from the Dock inherits launchd's PATH, which is the system's
//! four directories and nothing the user's shell adds: no Homebrew, no
//! `~/.cargo/bin`, no `~/.local/bin`. The password command, `nvim`, `pass`,
//! `op` and `gpg` all live in those, so a bundled Orb would find none of them.
//! The answer is to ask the user's own shell what its PATH is, once, at
//! startup — the way editors launched from the Dock do.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the shell may take to start and print its PATH. A shell whose
/// startup files wait for something — a prompt, a network mount — would
/// otherwise hold the launch.
const SHELL_TIMEOUT: Duration = Duration::from_secs(5);

const MARKER: &str = "ORB_PATH=";

/// The PATH of an interactive login shell of the user's `$SHELL`, or `None`
/// when there is no shell, it fails, or it takes too long.
///
/// Interactive as well as login, because a PATH set in `.zshrc` or `.bashrc`
/// is only seen by an interactive shell, and that is where most people set
/// it. The shell's own chatter is ignored: only the marked line counts.
pub fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let output = run_bounded(
        Command::new(&shell)
            .args(["-ilc", &format!("printf '{MARKER}%s\\n' \"$PATH\"")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        SHELL_TIMEOUT,
    )?;
    let path = output
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(MARKER))?
        .trim()
        .to_string();
    (!path.is_empty()).then_some(path)
}

/// The login shell's PATH, followed by whatever the process already had that
/// the shell did not name — so a launch from a terminal with something extra
/// on its PATH keeps it.
pub fn merged_path(login: &str, current: &str) -> String {
    let mut entries: Vec<&str> = Vec::new();
    for entry in login.split(':').chain(current.split(':')) {
        if !entry.is_empty() && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries.join(":")
}

/// Run the command, returning its stdout, or `None` if it fails or outlives
/// `timeout`, in which case it is killed.
fn run_bounded(command: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stdout.read_to_string(&mut buffer);
        buffer
    });

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
    reader.join().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_path_comes_first_and_nothing_is_lost_or_repeated() {
        assert_eq!(
            merged_path("/opt/homebrew/bin:/usr/bin", "/usr/bin:/bin:/extra"),
            "/opt/homebrew/bin:/usr/bin:/bin:/extra"
        );
        assert_eq!(merged_path("", "/usr/bin"), "/usr/bin");
        assert_eq!(merged_path("/a::/b", ""), "/a:/b");
    }

    #[test]
    fn the_marked_line_is_read_and_a_hanging_shell_is_given_up_on() {
        let output = run_bounded(
            Command::new("sh")
                .args(["-c", "echo noise; echo ORB_PATH=/x:/y"])
                .stdout(Stdio::piped()),
            SHELL_TIMEOUT,
        )
        .expect("output");
        assert_eq!(
            output.lines().rev().find_map(|l| l.strip_prefix(MARKER)),
            Some("/x:/y")
        );

        let started = Instant::now();
        let hung = run_bounded(
            Command::new("sh")
                .args(["-c", "sleep 30"])
                .stdout(Stdio::piped()),
            Duration::from_millis(200),
        );
        assert!(hung.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));

        assert!(run_bounded(Command::new("false").stdout(Stdio::piped()), SHELL_TIMEOUT).is_none());
    }

    /// Whatever shell the machine has, asking it must not hang or fail the
    /// process; the answer may be `None` on a build machine with no shell.
    #[test]
    fn asking_the_real_shell_terminates() {
        let _ = login_shell_path();
    }
}
