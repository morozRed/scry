//! Loading recorded snapshots, and redacting a live one before it is written.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::home::Home;
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
pub fn home_dir() -> Result<Home> {
    match Home::from_env() {
        Some(home) => Ok(home),
        None => bail!("cannot tell the home directory, so a redacted snapshot cannot be written"),
    }
}

/// Rewrites anything that names the person. In every path the home directory becomes `~`,
/// by any name that reaches it, and the account name anywhere else becomes `…`. Connected
/// sockets, which name their peers, are dropped. Command labels hold no paths.
pub fn redact(mut snap: Snapshot, home: &Home) -> Snapshot {
    snap.sockets.retain(|s| s.is_listening());
    for p in &mut snap.processes {
        let project = p.project.as_mut().map(|r| &mut r.root);
        for path in [p.exe.as_mut(), p.cwd.as_mut(), project]
            .into_iter()
            .flatten()
        {
            *path = home.redact(path);
        }
    }
    snap
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
                worker: false,
            }],
            ..Snapshot::default()
        }
    }

    fn home(path: &str) -> Home {
        Home::new(Path::new(path)).unwrap()
    }

    #[test]
    fn redact_rewrites_home_but_not_a_lookalike_prefix() {
        let s = redact(snap_with_exe("/Users/ann/bin/x"), &home("/Users/ann"));
        assert_eq!(s.processes[0].exe.as_deref(), Some("~/bin/x"));
        let s = redact(snap_with_exe("/Users/ann/bin/x"), &home("/Users/ann/"));
        assert_eq!(s.processes[0].exe.as_deref(), Some("~/bin/x"));
        let s = redact(snap_with_exe("/Users/anna/bin/x"), &home("/Users/ann"));
        assert_eq!(s.processes[0].exe.as_deref(), Some("/Users/…a/bin/x"));
    }

    #[test]
    fn redact_rewrites_home_in_the_working_directory() {
        let s = redact(snap_with_exe("/Users/dev/bin/x"), &home("/Users/dev"));
        assert_eq!(s.processes[0].cwd.as_deref(), Some("~/bin/x"));
    }

    #[test]
    fn redact_hides_the_account_name_in_any_path_and_case() {
        let ann = home("/Users/ann");
        for (path, want) in [
            ("/System/Volumes/Data/Users/ann/acme", "~/acme"),
            ("/USERS/ANN/acme", "~/acme"),
            ("/private/tmp/Ann-build/x", "/private/tmp/…-build/x"),
            ("/Volumes/Backup/Users/ann", "/Volumes/Backup/Users/…"),
        ] {
            let mut s = snap_with_exe(path);
            s.processes[0].project = Some(crate::model::Project {
                root: path.into(),
                kind: crate::model::ProjectKind::Git,
            });
            let s = redact(s, &ann);
            let p = &s.processes[0];
            assert_eq!(p.exe.as_deref(), Some(want), "{path}");
            assert_eq!(p.cwd.as_deref(), Some(want), "{path}");
            assert_eq!(p.project.as_ref().unwrap().root, want, "{path}");
        }
    }

    #[test]
    fn redact_keeps_only_listening_sockets() {
        let mut s = parse(include_str!("../../../fixtures/typical.json")).unwrap();
        s.processes.clear();
        let s = redact(s, &home("/Users/dev"));
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
