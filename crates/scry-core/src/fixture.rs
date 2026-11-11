//! Loading recorded snapshots, and redacting a live one before it is written.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::model::{SNAPSHOT_SCHEMA, Snapshot};

pub fn load(path: &Path) -> Result<Snapshot> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse(text: &str) -> Result<Snapshot> {
    let snap: Snapshot = serde_json::from_str(text)?;
    if snap.schema != SNAPSHOT_SCHEMA {
        bail!(
            "snapshot schema {} is not the supported {SNAPSHOT_SCHEMA}",
            snap.schema
        );
    }
    Ok(snap)
}

/// The home directory to redact, from `HOME` or the password database.
pub fn home_dir() -> Result<String> {
    let home = std::env::home_dir()
        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
        .unwrap_or_default();
    if home.is_empty() {
        bail!("cannot tell the home directory, so a redacted snapshot cannot be written");
    }
    Ok(home)
}

/// Rewrites anything that names the person: the home directory becomes `~` in paths, and
/// connected sockets, which name their peers, are dropped. Command labels hold no paths.
pub fn redact(mut snap: Snapshot, home: &str) -> Snapshot {
    let home = home.trim_end_matches('/');
    snap.sockets.retain(|s| s.is_listening());
    for p in &mut snap.processes {
        for path in [p.exe.as_mut(), p.cwd.as_mut()].into_iter().flatten() {
            tilde(path, home);
        }
        if let Some(project) = &mut p.project {
            tilde(&mut project.root, home);
        }
    }
    snap
}

/// `home` or `home/...` becomes `~` or `~/...`; a lookalike prefix such as `home2` does not.
pub fn tilde(path: &mut String, home: &str) {
    if let Some(rest) = path.strip_prefix(home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        *path = format!("~{rest}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcId, Process};

    fn snap_with_exe(exe: &str) -> Snapshot {
        Snapshot {
            schema: SNAPSHOT_SCHEMA,
            taken_at_ms: 0,
            viewer_uid: 501,
            processes: vec![Process {
                id: ProcId {
                    pid: 10,
                    start_us: 1,
                },
                ppid: 1,
                uid: 501,
                name: "x".into(),
                exe: Some(exe.into()),
                footprint: None,
                command: None,
                cwd: Some(exe.into()),
                project: None,
                stamps: Default::default(),
            }],
            ..Snapshot::default()
        }
    }

    #[test]
    fn redact_rewrites_home_but_not_a_lookalike_prefix() {
        let s = redact(snap_with_exe("/Users/ann/bin/x"), "/Users/ann");
        assert_eq!(s.processes[0].exe.as_deref(), Some("~/bin/x"));
        let s = redact(snap_with_exe("/Users/ann/bin/x"), "/Users/ann/");
        assert_eq!(s.processes[0].exe.as_deref(), Some("~/bin/x"));
        let s = redact(snap_with_exe("/Users/anna/bin/x"), "/Users/ann");
        assert_eq!(s.processes[0].exe.as_deref(), Some("/Users/anna/bin/x"));
    }

    #[test]
    fn redact_rewrites_home_in_the_working_directory() {
        let s = redact(snap_with_exe("/Users/dev/bin/x"), "/Users/dev");
        assert_eq!(s.processes[0].cwd.as_deref(), Some("~/bin/x"));
    }

    #[test]
    fn redact_keeps_only_listening_sockets() {
        let mut s = parse(include_str!("../../../fixtures/typical.json")).unwrap();
        s.processes.clear();
        let s = redact(s, "/Users/dev");
        assert!(!s.sockets.is_empty());
        assert!(s.sockets.iter().all(|s| s.is_listening()));
    }

    #[test]
    fn the_committed_fixture_loads() {
        let s = parse(include_str!("../../../fixtures/typical.json")).unwrap();
        assert!(!s.sockets.is_empty());
    }

    #[test]
    fn parse_refuses_another_schema() {
        let mut s = snap_with_exe("/bin/x");
        s.schema = 99;
        assert!(parse(&serde_json::to_string(&s).unwrap()).is_err());
    }
}
