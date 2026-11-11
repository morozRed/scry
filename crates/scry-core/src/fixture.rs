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

/// Rewrites anything that names the person: the home directory becomes `~`.
pub fn redact(mut snap: Snapshot, home: &str) -> Snapshot {
    let home = home.trim_end_matches('/');
    for p in &mut snap.processes {
        if let Some(exe) = &mut p.exe
            && let Some(rest) = exe.strip_prefix(home)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            *exe = format!("~{rest}");
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
            }],
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
    fn parse_refuses_another_schema() {
        let mut s = snap_with_exe("/bin/x");
        s.schema = 99;
        assert!(parse(&serde_json::to_string(&s).unwrap()).is_err());
    }
}
