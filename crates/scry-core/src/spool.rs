//! The spool: hooks append one JSON line per [`SessionEvent`] to `spool/events.jsonl`, and the
//! daemon takes them into the store.
//!
//! A hook opens the file with `O_APPEND` and writes its line in one `write`, so lines from
//! hooks running at once never interleave. The daemon never truncates a file a hook may hold
//! open: it renames `events.jsonl` to `taken-<ms>.jsonl`, and reads and deletes a renamed file
//! only after [`SETTLE`], by which time any hook that opened it before the rename has
//! written. Ingest is idempotent by event id, so a crash between reading and deleting only
//! means the file is read again. Without the daemon, clients read every file here in memory.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::session::SessionEvent;

/// How long a renamed file waits before it is read. Hooks finish in well under 50 ms.
pub const SETTLE: Duration = Duration::from_secs(2);

/// The largest line Scry writes or reads. A longer one is skipped as damaged.
const MAX_LINE: usize = 4096;

const LIVE: &str = "events.jsonl";

/// Appends one event to the spool in `dir`, creating the file as 0600.
pub fn append(dir: &Path, event: &SessionEvent) -> Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    anyhow::ensure!(line.len() <= MAX_LINE, "event too long");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(LIVE))
        .with_context(|| format!("opening the spool in {}", dir.display()))?;
    f.write_all(&line)?;
    Ok(())
}

/// Every event in the spool that passes the shape checks, oldest file first, without
/// changing anything.
pub fn read_all(dir: &Path) -> Vec<SessionEvent> {
    let mut files = taken(dir);
    files.sort();
    files.push((i64::MAX, dir.join(LIVE)));
    files.iter().flat_map(|(_, f)| read_file(f)).collect()
}

/// For the daemon: renames the live file aside, then hands every renamed file older than
/// [`SETTLE`] to `take` and deletes it once `take` succeeds.
pub fn drain(
    dir: &Path,
    now_ms: i64,
    mut take: impl FnMut(&[SessionEvent]) -> Result<()>,
) -> Result<usize> {
    let live = dir.join(LIVE);
    if fs::metadata(&live).is_ok_and(|m| m.len() > 0) {
        let aside = dir.join(format!("taken-{now_ms}.jsonl"));
        if !aside.exists() {
            fs::rename(&live, &aside)?;
        }
    }
    let mut files = taken(dir);
    files.sort();
    let mut n = 0;
    for (at_ms, file) in files {
        if now_ms - at_ms < SETTLE.as_millis() as i64 {
            continue;
        }
        let events = read_file(&file);
        take(&events)?;
        n += events.len();
        fs::remove_file(&file)?;
    }
    Ok(n)
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

fn read_file(path: &Path) -> Vec<SessionEvent> {
    let Ok(text) = fs::read(path) else {
        return Vec::new();
    };
    text.split(|&b| b == b'\n')
        .filter(|l| !l.is_empty() && l.len() <= MAX_LINE)
        .filter_map(|l| serde_json::from_slice::<SessionEvent>(l).ok())
        .filter(SessionEvent::valid)
        .collect()
}

#[cfg(test)]
mod tests {
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
        let ids: Vec<String> = read_all(tmp.path()).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["a", "c"]);
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(tmp.path().join(LIVE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn drain_takes_a_renamed_file_only_after_it_settles() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        append(dir, &event("a")).unwrap();
        let mut got = Vec::new();
        let n = drain(dir, 10_000, |e| {
            got.extend(e.iter().map(|e| e.id.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 0, "renamed, not yet settled");
        append(dir, &event("b")).unwrap();
        assert_eq!(read_all(dir).len(), 2, "clients still see both");
        let n = drain(dir, 13_000, |e| {
            got.extend(e.iter().map(|e| e.id.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!((n, got.clone()), (1, vec!["a".to_string()]));
        drain(dir, 16_000, |e| {
            got.extend(e.iter().map(|e| e.id.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, ["a", "b"]);
        assert!(read_all(dir).is_empty());
        // A failed take keeps the file for the next round.
        append(dir, &event("c")).unwrap();
        drain(dir, 20_000, |_| Ok(())).unwrap();
        assert!(drain(dir, 30_000, |_| anyhow::bail!("store busy")).is_err());
        assert_eq!(read_all(dir).len(), 1);
    }
}
