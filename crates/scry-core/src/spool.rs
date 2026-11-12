//! The spool: hooks append one JSON line per [`SessionEvent`] to `spool/events.jsonl`, and the
//! daemon takes them into the store.
//!
//! Appends and the daemon's rename are serialised with `flock` on the live file. A hook
//! writes only while it holds the lock on the file still named `events.jsonl`, so once the
//! daemon has renamed it to `taken-<ms>.jsonl` under the lock, nothing writes to that file
//! again and it can be read at once. Each event goes out in one `write` of one whole line;
//! should a write ever stop short, the next append starts a new line first, and readers skip
//! the damaged line. Ingest is idempotent by event id, so a crash between reading and
//! deleting only means the file is read again, and a file that cannot be read is left for
//! the next pass. Without the daemon, clients read every file here in memory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::session::SessionEvent;

/// The largest line Scry writes or reads. A longer one is skipped as damaged.
const MAX_LINE: usize = 4096;

const LIVE: &str = "events.jsonl";

/// Appends one event to the spool in `dir`, creating the file as 0600.
pub fn append(dir: &Path, event: &SessionEvent) -> Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    anyhow::ensure!(line.len() <= MAX_LINE, "event too long");
    let path = dir.join(LIVE);
    let f =
        locked_live(&path).with_context(|| format!("opening the spool in {}", dir.display()))?;
    // A line cut short by a failed write would swallow this one.
    let len = f.metadata()?.len();
    let mut last = *b"\n";
    if len > 0 {
        f.read_exact_at(&mut last, len - 1)?;
    }
    let mut buf = Vec::with_capacity(line.len() + 1);
    if last[0] != b'\n' {
        buf.push(b'\n');
    }
    buf.extend_from_slice(&line);
    let n = (&f).write(&buf)?;
    anyhow::ensure!(n == buf.len(), "short write to the spool");
    Ok(())
}

/// The live file, open for appending and locked. A file renamed away between the open and
/// the lock belongs to the daemon now, so the open is tried again.
fn locked_live(path: &Path) -> io::Result<File> {
    loop {
        let f = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        lock(&f)?;
        let held = f.metadata()?;
        match fs::metadata(path) {
            Ok(now) if now.ino() == held.ino() && now.dev() == held.dev() => return Ok(f),
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
}

fn lock(f: &File) -> io::Result<()> {
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Every event in the spool that passes the shape checks, oldest file first, without
/// changing anything. A file that cannot be read is skipped.
pub fn read_all(dir: &Path) -> Vec<SessionEvent> {
    let mut files = taken(dir);
    files.sort();
    files.push((i64::MAX, dir.join(LIVE)));
    files
        .iter()
        .filter_map(|(_, f)| read_file(f).ok())
        .flatten()
        .collect()
}

/// For the daemon: renames the live file aside under its lock, then hands every renamed
/// file to `take` and deletes it once `take` succeeds. A file that cannot be read is kept
/// for a later pass.
pub fn drain(
    dir: &Path,
    now_ms: i64,
    mut take: impl FnMut(&[SessionEvent]) -> Result<()>,
) -> Result<usize> {
    let live = dir.join(LIVE);
    let aside = dir.join(format!("taken-{now_ms}.jsonl"));
    if fs::metadata(&live).is_ok_and(|m| m.len() > 0)
        && !aside.exists()
        && let Some(_held) = locked_live_existing(&live)?
    {
        // Renamed while locked, so no append lands in it afterwards.
        fs::rename(&live, &aside)?;
    }
    let mut files = taken(dir);
    files.sort();
    let mut n = 0;
    for (_, file) in files {
        let Ok(events) = read_file(&file) else {
            continue;
        };
        take(&events)?;
        n += events.len();
        fs::remove_file(&file)?;
    }
    Ok(n)
}

/// The live file locked, if there is one; the daemon never creates it.
fn locked_live_existing(path: &Path) -> io::Result<Option<File>> {
    loop {
        let f = match OpenOptions::new().append(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        lock(&f)?;
        let held = f.metadata()?;
        match fs::metadata(path) {
            Ok(now) if now.ino() == held.ino() && now.dev() == held.dev() => return Ok(Some(f)),
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

fn taken(dir: &Path) -> Vec<(i64, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let ms = name
                .to_str()?
                .strip_prefix("taken-")?
                .strip_suffix(".jsonl")?
                .parse()
                .ok()?;
            Some((ms, e.path()))
        })
        .collect()
}

/// The valid events in one file. A line that does not parse, such as one cut short, is
/// skipped up to the next newline. A missing file holds none.
fn read_file(path: &Path) -> io::Result<Vec<SessionEvent>> {
    let text = match fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty() && l.len() <= MAX_LINE)
        .filter_map(|l| serde_json::from_slice::<SessionEvent>(l).ok())
        .filter(SessionEvent::valid)
        .collect())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::model::Agent;
    use crate::session::EventKind;

    fn event(id: &str) -> SessionEvent {
        SessionEvent {
            id: id.into(),
            at_ms: 1,
            agent: Agent::ClaudeCode,
            session_id: "s1".into(),
            kind: EventKind::Start { source: None },
            cwd: Some("/x".into()),
            agent_proc: None,
        }
    }

    fn ids(events: Vec<SessionEvent>) -> Vec<String> {
        events.into_iter().map(|e| e.id).collect()
    }

    #[test]
    fn appended_events_read_back_and_damaged_lines_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &event("a")).unwrap();
        let mut f = OpenOptions::new()
            .append(true)
            .open(tmp.path().join(LIVE))
            .unwrap();
        f.write_all(b"{not json\n{\"id\":\"bad id\"}\n").unwrap();
        let mut bad = event("b");
        bad.session_id = "../etc".into();
        append(tmp.path(), &bad).unwrap();
        append(tmp.path(), &event("c")).unwrap();
        assert_eq!(ids(read_all(tmp.path())), ["a", "c"]);
        let mode = fs::metadata(tmp.path().join(LIVE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn an_append_after_a_line_cut_short_starts_a_new_line() {
        let tmp = tempfile::tempdir().unwrap();
        append(tmp.path(), &event("a")).unwrap();
        // What a write that ran out of space leaves: half a line, no newline.
        let mut f = OpenOptions::new()
            .append(true)
            .open(tmp.path().join(LIVE))
            .unwrap();
        f.write_all(br#"{"id":"b","at_ms":1,"ag"#).unwrap();
        append(tmp.path(), &event("c")).unwrap();
        assert_eq!(ids(read_all(tmp.path())), ["a", "c"]);
        // A reader resyncs at the next newline even when no writer repaired the line.
        let other = tempfile::tempdir().unwrap();
        let line = serde_json::to_string(&event("d")).unwrap();
        fs::write(
            other.path().join(LIVE),
            format!("{{\"id\":\"x\",\"at\n{line}\n"),
        )
        .unwrap();
        assert_eq!(ids(read_all(other.path())), ["d"]);
    }

    #[test]
    fn drain_takes_the_live_file_and_keeps_one_that_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        append(dir, &event("a")).unwrap();
        let mut got = Vec::new();
        let n = drain(dir, 10_000, |e| {
            got.extend(e.iter().map(|e| e.id.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!((n, got.clone()), (1, vec!["a".to_string()]));
        assert!(read_all(dir).is_empty());
        // A failed take keeps the file for the next round.
        append(dir, &event("c")).unwrap();
        assert!(drain(dir, 20_000, |_| anyhow::bail!("store busy")).is_err());
        assert_eq!(read_all(dir).len(), 1);
        drain(dir, 30_000, |e| {
            got.extend(e.iter().map(|e| e.id.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, ["a", "c"]);
    }

    #[test]
    fn an_unreadable_file_is_kept_for_later_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        append(dir, &event("a")).unwrap();
        let held = dir.join("taken-5.jsonl");
        fs::rename(dir.join(LIVE), &held).unwrap();
        fs::set_permissions(&held, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&held).is_ok() {
            // Running as root: the mode does not stop the read, so there is nothing to test.
            return;
        }
        let mut got = Vec::new();
        drain(dir, 10_000, |e| {
            got.extend(e.iter().map(|e| e.id.clone()));
            Ok(())
        })
        .unwrap();
        assert!(got.is_empty());
        assert!(held.exists(), "kept for a later pass");
        fs::set_permissions(&held, fs::Permissions::from_mode(0o600)).unwrap();
        drain(dir, 20_000, |e| {
            got.extend(e.iter().map(|e| e.id.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, ["a"]);
        assert!(!held.exists());
    }

    #[test]
    fn an_append_held_up_during_a_rotation_is_not_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        append(&dir, &event("a")).unwrap();
        // A hook has the live file open and locked, as if paused in the middle of an append.
        let hook = locked_live(&dir.join(LIVE)).unwrap();
        let (tx, rx) = mpsc::channel();
        let d = dir.clone();
        let daemon = std::thread::spawn(move || {
            let mut got = Vec::new();
            drain(&d, 10_000, |e| {
                got.extend(e.iter().map(|e| e.id.clone()));
                Ok(())
            })
            .unwrap();
            tx.send(()).unwrap();
            got
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the rotation waits for the append"
        );
        let line = serde_json::to_string(&event("b")).unwrap() + "\n";
        (&hook).write_all(line.as_bytes()).unwrap();
        drop(hook);
        let first = daemon.join().unwrap();
        // The next hook opens the live file again, not the one renamed away.
        append(&dir, &event("c")).unwrap();
        let mut got = first;
        drain(&dir, 20_000, |e| {
            got.extend(e.iter().map(|e| e.id.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, ["a", "b", "c"]);
    }
}
