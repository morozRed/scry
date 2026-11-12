//! One reading of the machine: processes, sockets, command labels, stamps and projects of
//! the viewer's own processes, and the pulse. The TUI, CLI and MCP server all take snapshots
//! from here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::path::Path;

use anyhow::Result;

use crate::home::Home;
use crate::label::{self, CommandLabel, Facts, LiveFiles};
use crate::model::{ProcId, Project, Proto, Snapshot, Stamps};
use crate::sys::procinfo::ArgsReader;
use crate::sys::pulse::PulseSampler;
use crate::{project, sys};

/// Which of the viewer's processes get a command label. Building one can look up each
/// argument as a file, and every CLI command builds a fresh engine, so a command labels only
/// the processes it shows. A multiprocessing worker's parent, which names it, comes along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Labels {
    /// Every one, for `scry snapshot`.
    All,
    /// Those the socket table names as listening, for `ports`, `why` and `stop :port`.
    Listeners,
    /// One pid, for `stop <pid>`.
    Pid(i32),
}

/// What Scry read from one process's arguments.
struct Args {
    /// The executable the arguments belong to. An `exec` keeps the pid and start time but
    /// changes this, and the arguments are read again.
    exe: Option<String>,
    argv: Vec<String>,
    stamps: Stamps,
    worker: bool,
    /// The label and the listening ports it was built with. It is built again when they change.
    label: Option<(Vec<u16>, CommandLabel)>,
}

pub struct Engine {
    labels: Labels,
    reader: ArgsReader,
    /// Arguments are read once per process and executable.
    args: HashMap<ProcId, Args>,
    files: LiveFiles,
    pulse: PulseSampler,
    home: Option<Home>,
    /// Account names by uid. Accounts rarely change while Scry runs.
    names: HashMap<u32, String>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// An engine that labels every one of the viewer's processes.
    pub fn new() -> Self {
        Engine::labelling(Labels::All)
    }

    pub fn labelling(labels: Labels) -> Self {
        Engine {
            labels,
            reader: ArgsReader::new(),
            args: HashMap::new(),
            files: LiveFiles::new(),
            pulse: PulseSampler::new(),
            home: Home::from_env(),
            names: HashMap::new(),
        }
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        let mut snap = sys::scan()?;
        snap.sockets = sys::sockets::read_all()?;
        self.files.refresh();
        let live: HashSet<ProcId> = snap.processes.iter().map(|p| p.id).collect();
        self.args.retain(|id, _| live.contains(id));
        let mut listening: HashMap<i32, Vec<(Proto, u16)>> = HashMap::new();
        for s in snap.sockets.iter().filter(|s| s.is_listening()) {
            listening
                .entry(s.pid)
                .or_default()
                .push((s.proto, s.local_port));
        }
        let mut roots: HashMap<String, Option<Project>> = HashMap::new();
        let home = self.home.as_ref().map(Home::path);
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
                            worker: label::multiprocessing_worker(&read.argv),
                            argv: read.argv,
                            stamps: read.stamps,
                            label: None,
                        },
                    );
                }
            }
            if let Some(a) = self.args.get(&p.id) {
                p.stamps = a.stamps.clone();
                p.worker = a.worker;
            }
            if let Some(cwd) = &p.cwd {
                p.project = roots
                    .entry(cwd.clone())
                    .or_insert_with(|| project::find_root(Path::new(cwd), home.as_deref()))
                    .clone();
            }
        }
        let wanted = self.wanted(&snap, &listening);
        // A worker's parent often opened the socket and keeps it, as `uvicorn --reload`
        // does, so its label may show the port too, once its own files confirm it.
        let mut candidates = listening.clone();
        for (pid, ports) in &listening {
            if let Some(parent) = snap.process(*pid).and_then(|p| snap.worker_parent(p)) {
                candidates
                    .entry(parent.id.pid)
                    .or_default()
                    .extend(ports.iter().copied());
            }
        }
        for p in snap
            .processes
            .iter_mut()
            .filter(|p| p.uid == snap.viewer_uid && wanted.contains(&p.id.pid))
        {
            let Some(a) = self.args.get_mut(&p.id) else {
                continue;
            };
            let ports = held(p.id.pid, candidates.get(&p.id.pid));
            if a.label.as_ref().is_none_or(|(built, _)| *built != ports) {
                let facts = Facts {
                    exe: p.exe.as_deref(),
                    name: &p.name,
                    cwd: p.cwd.as_deref().map(Path::new),
                    home: self.home.as_ref(),
                    ports: &ports,
                    files: &self.files,
                };
                a.label = Some((ports.clone(), CommandLabel::build(&a.argv, &facts)));
            }
            p.command = a.label.as_ref().map(|(_, l)| l.clone());
        }
        snap.user_names = self.user_names(&snap);
        snap.pulse = self.pulse.sample();
        Ok(snap)
    }

    /// The pids to label: what [`Labels`] names, and the ancestors that name its workers.
    fn wanted(&self, snap: &Snapshot, listening: &HashMap<i32, Vec<(Proto, u16)>>) -> HashSet<i32> {
        let mut wanted: HashSet<i32> = match self.labels {
            Labels::All => return snap.own_processes().map(|p| p.id.pid).collect(),
            Labels::Listeners => listening.keys().copied().collect(),
            Labels::Pid(pid) => HashSet::from([pid]),
        };
        let parents: Vec<i32> = wanted
            .iter()
            .filter_map(|&pid| snap.process(pid))
            .filter_map(|p| snap.worker_parent(p))
            .map(|p| p.id.pid)
            .collect();
        wanted.extend(parents);
        wanted
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

/// The ports among `candidates`, from the socket table, that `pid` holds through its own
/// open files. The table keeps `so_last_pid`, which can name a process that has since closed
/// or handed off the socket, and a number in its arguments must not show for a port it lost.
fn held(pid: i32, candidates: Option<&Vec<(Proto, u16)>>) -> Vec<u16> {
    match candidates {
        Some(candidates) => confirmed(
            candidates,
            &sys::sockets::listening_ports(pid).unwrap_or_default(),
        ),
        None => Vec::new(),
    }
}

fn confirmed(table: &[(Proto, u16)], open: &[(Proto, u16)]) -> Vec<u16> {
    let mut ports: Vec<u16> = table
        .iter()
        .filter(|t| open.contains(t))
        .map(|&(_, port)| port)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
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
    use std::io::{BufRead, BufReader, Write};
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

    #[test]
    fn only_ports_a_process_still_holds_are_its_ports() {
        let table = [(Proto::Tcp, 3000), (Proto::Udp, 5353), (Proto::Tcp, 8080)];
        let open = [(Proto::Tcp, 8080), (Proto::Udp, 3000), (Proto::Udp, 5353)];
        assert_eq!(confirmed(&table, &open), [5353, 8080]);
        assert_eq!(confirmed(&table, &[]), Vec::<u16>::new());
    }

    /// A Python process that opens a listener, forks a child that keeps it, and execs into a
    /// sleep with the port as an argument. Its own copy closes on the exec, so the socket
    /// table's `so_last_pid` names it for a port it no longer holds.
    #[test]
    fn a_stale_table_pid_does_not_make_a_number_show() {
        let script = r#"
import os, socket, sys, time
s = socket.socket()
s.bind(("127.0.0.1", 0))
s.listen()
port = s.getsockname()[1]
child = os.fork()
if child == 0:
    time.sleep(30)
    os._exit(0)
code = "import sys, time; print(sys.argv[1], sys.argv[2], flush=True); time.sleep(30)"
os.execv(sys.executable, [sys.executable, "-c", code, str(port), str(child)])
"#;
        let mut parent = Command::new("python3")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3");
        let mut line = String::new();
        BufReader::new(parent.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let (port, child) = line.trim().split_once(' ').expect("port and child pid");
        let (port, child): (u16, i32) = (port.parse().unwrap(), child.parse().unwrap());
        let pid = parent.id() as i32;
        let snap = Engine::new().snapshot().unwrap();
        let named: Vec<i32> = snap
            .sockets
            .iter()
            .filter(|s| s.is_listening() && s.local_port == port)
            .map(|s| s.pid)
            .collect();
        unsafe { libc::kill(child, libc::SIGKILL) };
        parent.kill().unwrap();
        parent.wait().unwrap();
        assert_eq!(
            named,
            [pid],
            "the table names the parent, which closed the socket"
        );
        let label = snap.process(pid).unwrap().command.as_ref().unwrap();
        assert_eq!(label.to_string(), "python3 -c …", "not the port {port}");
    }

    #[test]
    fn a_command_labels_only_the_processes_it_shows() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sleeper = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let (me, other) = (std::process::id() as i32, sleeper.id() as i32);
        let labelled = |labels| {
            let snap = Engine::labelling(labels).snapshot().unwrap();
            let has = |pid| snap.process(pid).is_some_and(|p| p.command.is_some());
            (has(me), has(other))
        };
        assert_eq!(labelled(Labels::All), (true, true));
        assert_eq!(labelled(Labels::Listeners), (true, false));
        assert_eq!(labelled(Labels::Pid(other)), (false, true));
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        drop(listener);
    }
}
