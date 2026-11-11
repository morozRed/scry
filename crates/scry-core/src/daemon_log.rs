//! `daemon.log`: every line starts with a UTC timestamp, and once the file reaches `CAP` it
//! moves to `daemon.log.1`, replacing the one before. launchd opens the log as the daemon's
//! stderr before the daemon starts, so after a move the new file takes over stderr too and a
//! panic message still lands in it.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

const CAP: u64 = 1 << 20;

/// The log that stderr writes to, once `attach` has seen that it does. A terminal or a pipe
/// is never rotated.
static ROTATING: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Rotates `path` from now on if stderr is that file, as it is under launchd.
pub fn attach(path: &Path) {
    let same = fd_file(libc::STDERR_FILENO).is_some_and(|(dev, ino, _)| {
        fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == (dev, ino))
    });
    if same {
        *ROTATING.lock().unwrap_or_else(PoisonError::into_inner) = Some(path.to_path_buf());
    }
}

/// Writes `<UTC time> scry daemon: <msg>` to stderr, rotating the log first if it is full.
pub fn line(msg: &str) {
    let rotating = ROTATING.lock().unwrap_or_else(PoisonError::into_inner);
    let failed = rotating
        .as_deref()
        .and_then(|path| rotate_if_full(path, CAP, libc::STDERR_FILENO).err());
    let now = utc_stamp(SystemTime::now());
    let mut err = io::stderr().lock();
    if let Some(e) = failed {
        let _ = writeln!(err, "{now} scry daemon: cannot rotate the log: {e}");
    }
    let _ = writeln!(err, "{now} scry daemon: {msg}");
}

/// `<path>.1`.
fn rotated(path: &Path) -> PathBuf {
    let mut s = OsString::from(path.as_os_str());
    s.push(".1");
    PathBuf::from(s)
}

/// When the file behind `fd` holds `cap` bytes or more, moves `path` to `<path>.1`, creates a
/// new 0600 `path` and points `fd` at it. Returns whether it rotated.
fn rotate_if_full(path: &Path, cap: u64, fd: RawFd) -> io::Result<bool> {
    let Some((dev, ino, size)) = fd_file(fd) else {
        return Ok(false);
    };
    if size < cap {
        return Ok(false);
    }
    // If `path` is no longer the file `fd` writes to, someone moved or removed it; leave
    // whatever is there and only start a new file.
    if fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == (dev, ino)) {
        fs::rename(path, rotated(path))?;
    }
    let new = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    if unsafe { libc::dup2(new.as_raw_fd(), fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

/// Device, inode and size of the file open on `fd`.
fn fd_file(fd: RawFd) -> Option<(u64, u64, u64)> {
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    Some((st.st_dev as u64, st.st_ino, st.st_size as u64))
}

/// `2026-11-11T17:04:05Z`.
pub fn utc_stamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or_default();
    let mut tm = MaybeUninit::<libc::tm>::zeroed();
    if unsafe { libc::gmtime_r(&secs, tm.as_mut_ptr()) }.is_null() {
        return secs.to_string();
    }
    let tm = unsafe { tm.assume_init() };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_full_log_moves_aside_and_the_descriptor_follows_the_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.log");
        fs::write(&path, "a".repeat(100)).unwrap();
        let mut out = OpenOptions::new().append(true).open(&path).unwrap();
        let fd = out.as_raw_fd();

        assert!(!rotate_if_full(&path, 101, fd).unwrap(), "under the cap");
        assert!(rotate_if_full(&path, 100, fd).unwrap());
        writeln!(out, "after").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");
        assert_eq!(fs::read_to_string(rotated(&path)).unwrap(), "a".repeat(100));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        write!(out, "{}", "b".repeat(200)).unwrap();
        assert!(rotate_if_full(&path, 100, fd).unwrap());
        assert!(
            fs::read_to_string(rotated(&path))
                .unwrap()
                .starts_with("after\nbb"),
            "one old file is kept"
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);

        // Someone removed the log by hand: the next rotation starts a new one.
        write!(out, "{}", "c".repeat(200)).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(rotate_if_full(&path, 100, fd).unwrap());
        writeln!(out, "back").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "back\n");
    }

    #[test]
    fn utc_stamp_formats_the_epoch() {
        assert_eq!(utc_stamp(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }
}
