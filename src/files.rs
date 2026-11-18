//! Files and directories readable by the user alone.
//!
//! What Orb writes — the log, the database, cached attachments, a config
//! template, a draft on its way to the editor — is mail, and mail is
//! private. `std::fs` creates files and directories with the umask's
//! permissions, which on a shared machine leaves them readable by everyone,
//! so everything Orb creates goes through here instead.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Create `path` and the directories above it, each one Orb creates with
/// mode 0700. A directory that already exists is left as it is: it may be
/// the user's, or the system's, and its permissions are not Orb's to change.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        missing.push(cursor);
        match cursor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
            _ => break,
        }
    }
    for dir in missing.into_iter().rev() {
        match fs::create_dir(dir) {
            Ok(()) => set_dir_private(dir)?,
            // Made by another instance between the look and the call.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Give a directory of Orb's own mode 0700, whoever created it.
pub fn set_dir_private(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let _ = path;
    Ok(())
}

/// Give an existing file mode 0600.
pub fn set_private(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

/// Write `bytes` to `path`, replacing what was there, as a file readable by
/// the user alone, and on disk before returning.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = private_options().write(true).truncate(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Open `path` for appending, creating it readable by the user alone.
pub fn open_private_append(path: &Path) -> io::Result<File> {
    private_options().append(true).open(path)
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn what_is_created_is_readable_by_the_user_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a").join("b");
        create_private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(dir.parent().unwrap()), 0o700);

        let written = dir.join("written");
        write_private(&written, b"one").unwrap();
        write_private(&written, b"two").unwrap();
        assert_eq!(mode(&written), 0o600);
        assert_eq!(fs::read(&written).unwrap(), b"two");

        let appended = dir.join("appended");
        open_private_append(&appended)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        open_private_append(&appended)
            .unwrap()
            .write_all(b"y")
            .unwrap();
        assert_eq!(mode(&appended), 0o600);
        assert_eq!(fs::read(&appended).unwrap(), b"xy");

        let loose = dir.join("loose");
        fs::write(&loose, b"").unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).unwrap();
        set_private(&loose).unwrap();
        assert_eq!(mode(&loose), 0o600);
    }

    /// A directory that is already there — the user's, or the system's — is
    /// not Orb's to tighten; only what Orb makes is made private.
    #[test]
    fn a_directory_that_exists_keeps_its_permissions() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&shared.join("mine")).unwrap();
        assert_eq!(mode(&shared), 0o755);
        assert_eq!(mode(&shared.join("mine")), 0o700);
        create_private_dir(&shared).unwrap();
        assert_eq!(mode(&shared), 0o755);
        set_dir_private(&shared).unwrap();
        assert_eq!(mode(&shared), 0o700);
    }
}
