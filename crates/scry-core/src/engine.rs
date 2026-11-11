//! One reading of the machine: processes, sockets, command labels, stamps and projects of
//! the viewer's own processes, and the pulse. The TUI, CLI and MCP server all take snapshots
//! from here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::label::{CommandLabel, Facts, LiveFiles};
use crate::model::{ProcId, Project, Snapshot, Stamps};
use crate::sys::procinfo::ArgsReader;
use crate::sys::pulse::PulseSampler;
use crate::{project, sys};

/// What Scry read from one process's arguments.
struct Args {
    /// The executable the arguments belong to. An `exec` keeps the pid and start time but
    /// changes this, and the arguments are read again.
    exe: Option<String>,
    argv: Vec<String>,
    stamps: Stamps,
    /// The label and the listening ports it was built with. It is built again when they change.
    label: Option<(Vec<u16>, CommandLabel)>,
}

pub struct Engine {
    reader: ArgsReader,
    /// Arguments are read once per process and executable.
    args: HashMap<ProcId, Args>,
    files: LiveFiles,
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
            files: LiveFiles::new(),
            pulse: PulseSampler::new(),
            home: std::env::home_dir(),
            names: HashMap::new(),
        }
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        let mut snap = sys::scan()?;
        snap.sockets = sys::sockets::read_all()?;
        let live: HashSet<ProcId> = snap.processes.iter().map(|p| p.id).collect();
        self.args.retain(|id, _| live.contains(id));
        let mut listening: HashMap<i32, Vec<u16>> = HashMap::new();
        for s in snap.sockets.iter().filter(|s| s.is_listening()) {
            listening.entry(s.pid).or_default().push(s.local_port);
        }
        for ports in listening.values_mut() {
            ports.sort_unstable();
            ports.dedup();
        }
        let mut roots: HashMap<String, Option<Project>> = HashMap::new();
        for p in snap
            .processes
            .iter_mut()
            .filter(|p| p.uid == snap.viewer_uid)
        {
            p.cwd = sys::procinfo::cwd(p.id.pid);
            if self.args.get(&p.id).is_none_or(|a| a.exe != p.exe) {
                self.args.remove(&p.id);
                if let Some(mut read) = self.reader.read(p.id.pid) {
                    // A process that rewrites its title leaves the old argument slots empty.
                    while read.argv.last().is_some_and(|a| a.is_empty()) {
                        read.argv.pop();
                    }
                    self.args.insert(
                        p.id,
                        Args {
                            exe: p.exe.clone(),
                            argv: read.argv,
                            stamps: read.stamps,
                            label: None,
                        },
                    );
                }
            }
            if let Some(a) = self.args.get_mut(&p.id) {
                let ports = listening.get(&p.id.pid).map_or(&[][..], Vec::as_slice);
                if a.label.as_ref().is_none_or(|(built, _)| built != ports) {
                    let facts = Facts {
                        exe: p.exe.as_deref(),
                        name: &p.name,
                        cwd: p.cwd.as_deref().map(Path::new),
                        home: self.home.as_deref(),
                        ports,
                        files: &self.files,
                    };
                    a.label = Some((ports.to_vec(), CommandLabel::build(&a.argv, &facts)));
                }
                p.command = a.label.as_ref().map(|(_, l)| l.clone());
                p.stamps = a.stamps.clone();
            }
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

    /// The raw arguments of one of the viewer's processes, as read for the last snapshot. They
    /// can hold secrets: show them only when a person asks, and never put them in a snapshot,
    /// JSON or the store.
    pub fn command_line(&self, id: ProcId) -> Option<&[String]> {
        self.args.get(&id).map(|a| a.argv.as_slice())
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
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn a_snapshot_joins_this_process_with_its_label_cwd_and_project() {
        let mut e = Engine::new();
        let snap = e.snapshot().unwrap();
        let me = snap.process(std::process::id() as i32).expect("self");
        let exe = me.exe.as_deref().expect("own executable readable");
        let name = exe.rsplit('/').next().unwrap();
        let label = me.command.as_ref().expect("own label").to_string();
        assert!(label.starts_with(name), "{label} names {exe}");
        let here = std::env::current_dir().unwrap();
        assert_eq!(me.cwd.as_deref(), here.to_str());
        let root = me.project.as_ref().expect("the crate is in a project");
        assert!(here.starts_with(&root.root), "{root:?}");
        assert!(!snap.sockets.is_empty());
        assert_eq!(snap.pulse.cpu_percent, crate::model::Reading::WarmingUp);
        let raw = e
            .command_line(me.id)
            .expect("raw arguments kept for --show-command");
        assert_eq!(raw, std::env::args().collect::<Vec<_>>());
        assert_eq!(snap.owner_name(0), "root");
        assert!(
            snap.processes
                .iter()
                .all(|p| p.uid == snap.viewer_uid || snap.user_names.contains_key(&p.uid)),
            "every process owner has a name"
        );
    }

    #[test]
    fn an_exec_reads_the_arguments_again() {
        // The shell waits on stdin, so the test decides when it execs.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read go; exec sleep 30"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let mut e = Engine::new();
        let label = |e: &mut Engine| {
            let snap = e.snapshot().unwrap();
            let p = snap.process(pid).expect("child listed");
            (p.id, p.command.as_ref().map(|c| c.to_string()))
        };
        // macOS's /bin/sh runs bash, so the label names whichever executable it is.
        let (_, before) = label(&mut e);
        assert!(
            before.as_deref().is_some_and(|l| l.ends_with("sh -c …")),
            "{before:?}"
        );
        writeln!(child.stdin.take().unwrap(), "go").unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let id = loop {
            match label(&mut e) {
                (id, Some(l)) if l == "sleep …" => break id,
                _ => assert!(Instant::now() < until, "never saw the exec"),
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(e.command_line(id).unwrap(), ["sleep", "30"]);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
