//! One reading of the machine: processes, sockets, the viewer's own command lines, stamps
//! and projects, and the pulse. The TUI, CLI and MCP server all take snapshots from here.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::model::{ProcId, Project, Snapshot, Stamps};
use crate::sys::procinfo::ArgsReader;
use crate::sys::pulse::PulseSampler;
use crate::{project, redact, sys};

struct Args {
    argv: Vec<String>,
    stamps: Stamps,
}

pub struct Engine {
    reader: ArgsReader,
    /// Arguments and stamps never change after exec, so each process is read once.
    args: HashMap<ProcId, Args>,
    pulse: PulseSampler,
    home: Option<PathBuf>,
    /// Account names by uid. Accounts rarely change while Scry runs.
    names: HashMap<u32, String>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            reader: ArgsReader::new(),
            args: HashMap::new(),
            pulse: PulseSampler::new(),
            home: std::env::home_dir(),
            names: HashMap::new(),
        }
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        let mut snap = sys::scan()?;
        snap.sockets = sys::sockets::read_all()?;
        self.args
            .retain(|id, _| snap.processes.iter().any(|p| p.id == *id));
        let mut roots: HashMap<String, Option<Project>> = HashMap::new();
        for p in snap
            .processes
            .iter_mut()
            .filter(|p| p.uid == snap.viewer_uid)
        {
            if !self.args.contains_key(&p.id)
                && let Some(mut read) = self.reader.read(p.id.pid)
            {
                // A process that rewrites its title leaves the old argument slots empty.
                while read.argv.last().is_some_and(|a| a.is_empty()) {
                    read.argv.pop();
                }
                self.args.insert(
                    p.id,
                    Args {
                        argv: redact::argv(&read.argv),
                        stamps: read.stamps,
                    },
                );
            }
            if let Some(a) = self.args.get(&p.id) {
                p.argv = Some(a.argv.clone());
                p.stamps = a.stamps.clone();
            }
            p.cwd = sys::procinfo::cwd(p.id.pid);
            if let Some(cwd) = &p.cwd {
                let home = self.home.as_deref();
                p.project = roots
                    .entry(cwd.clone())
                    .or_insert_with(|| project::find_root(Path::new(cwd), home))
                    .clone();
            }
        }
        snap.user_names = self.user_names(&snap);
        snap.pulse = self.pulse.sample();
        Ok(snap)
    }

    /// Names for every uid other than the viewer's that owns a process or a socket.
    fn user_names(&mut self, snap: &Snapshot) -> BTreeMap<u32, String> {
        let uids = snap
            .processes
            .iter()
            .map(|p| p.uid)
            .chain(snap.sockets.iter().map(|s| s.uid))
            .filter(|&uid| uid != snap.viewer_uid);
        let mut names = BTreeMap::new();
        for uid in uids {
            let name = self
                .names
                .entry(uid)
                .or_insert_with(|| user_name(uid).unwrap_or_else(|| format!("uid {uid}")));
            names.insert(uid, name.clone());
        }
        names
    }
}

fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 1024];
    loop {
        let mut pw = MaybeUninit::<libc::passwd>::zeroed();
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(uid, pw.as_mut_ptr(), buf.as_mut_ptr(), buf.len(), &mut out)
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || out.is_null() {
            return None;
        }
        let name = unsafe { CStr::from_ptr((*out).pw_name) };
        return Some(name.to_string_lossy().into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_joins_this_process_with_its_args_cwd_and_project() {
        let mut e = Engine::new();
        let snap = e.snapshot().unwrap();
        let me = snap.process(std::process::id() as i32).expect("self");
        assert!(me.argv.as_ref().is_some_and(|a| !a.is_empty()));
        let here = std::env::current_dir().unwrap();
        assert_eq!(me.cwd.as_deref(), here.to_str());
        let root = me.project.as_ref().expect("the crate is in a project");
        assert!(here.starts_with(&root.root), "{root:?}");
        assert!(!snap.sockets.is_empty());
        assert_eq!(snap.pulse.cpu_percent, crate::model::Reading::WarmingUp);
        assert!(
            e.args.contains_key(&me.id),
            "args cached for the next snapshot"
        );
        assert_eq!(snap.owner_name(0), "root");
        assert!(
            snap.processes
                .iter()
                .all(|p| p.uid == snap.viewer_uid || snap.user_names.contains_key(&p.uid)),
            "every process owner has a name"
        );
    }
}
