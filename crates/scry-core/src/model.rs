//! The snapshot: processes, sockets and the pulse as plain, serializable data.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::catalog::Role;
use crate::label::CommandLabel;

pub const SNAPSHOT_SCHEMA: u32 = 2;

/// A process is identified by pid plus start time, so a reused pid never matches an old record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcId {
    pub pid: i32,
    /// Microseconds since the Unix epoch.
    pub start_us: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub id: ProcId,
    pub ppid: i32,
    pub uid: u32,
    pub name: String,
    /// Absolute path, with the home directory written as `~` in recorded snapshots.
    pub exe: Option<String>,
    /// Physical footprint in bytes, as Activity Monitor's Memory column. Readable only for
    /// the viewer's own processes without root.
    pub footprint: Option<u64>,
    /// The command line as a label of safe parts. Raw arguments are never stored here. Only
    /// the viewer's own processes have one: other users' arguments are unreadable without root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandLabel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Project>,
    #[serde(default, skip_serializing_if = "Stamps::is_empty")]
    pub stamps: Stamps,
    /// A Python multiprocessing worker, such as the server process `uvicorn --reload`
    /// starts. Its arguments say only that, so views name it by [`Snapshot::worker_parent`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worker: bool,
    /// The controlling terminal, as `ttys004`, while the process has a live one. The kernel
    /// drops it when the terminal closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    /// What the agent catalog recognised from the executable path and arguments: an agent,
    /// or something on the never-reap list. Only the viewer's own processes are classified,
    /// because only their arguments are readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// For an interpreter, the real path of the script it runs; see [`crate::keep`]. Read
    /// to match the keep list and never written to a snapshot or JSON.
    #[serde(skip)]
    pub script: Option<String>,
    /// For a program that is not an interpreter, whether an argument names an existing
    /// file, such as a config or a script it runs. Its executable alone would then keep it
    /// whatever file it is given; see [`crate::keep::entry_for`]. Never written out.
    #[serde(skip)]
    pub file_args: bool,
}

/// The allowlisted agent keys from a process's environment. Every other variable is dropped
/// while the environment is parsed, and a value of any other shape than an id is dropped
/// too, so a stamp never carries text such as a header or a password into an output.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_pid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scry_session: Option<String>,
}

impl Stamps {
    pub fn is_empty(&self) -> bool {
        *self == Stamps::default()
    }

    /// Takes one `KEY=value` environment entry, keeping it only if the key is allowlisted
    /// and the value has the key's shape: an id of 1 to 64 letters, digits, `-` and `_`
    /// (which a UUID is), or digits for `CLAUDE_PID`.
    pub fn take(&mut self, entry: &[u8]) {
        let Some(eq) = entry.iter().position(|&b| b == b'=') else {
            return;
        };
        let (key, value) = (&entry[..eq], &entry[eq + 1..]);
        let id = || {
            let ok = (1..=64).contains(&value.len())
                && value
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
            ok.then(|| String::from_utf8_lossy(value).into_owned())
        };
        match key {
            b"CLAUDE_CODE_SESSION_ID" => self.claude_code_session_id = id(),
            b"CLAUDE_PID" => {
                self.claude_pid = std::str::from_utf8(value)
                    .ok()
                    .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|v| v.parse().ok())
            }
            b"CODEX_SESSION_ID" => self.codex_session_id = id(),
            b"SCRY_SESSION" => self.scry_session = id(),
            _ => {}
        }
    }

    /// The agent and session id a stamp names, strongest first.
    pub fn session(&self) -> Option<(Agent, &str, &'static str)> {
        self.sessions().into_iter().next()
    }

    /// Every agent and session id the stamps name, strongest first. A process started by
    /// one agent inside another's session carries both.
    pub fn sessions(&self) -> Vec<(Agent, &str, &'static str)> {
        [
            (Agent::ClaudeCode, &self.scry_session, "SCRY_SESSION"),
            (
                Agent::ClaudeCode,
                &self.claude_code_session_id,
                "CLAUDE_CODE_SESSION_ID",
            ),
            (Agent::Codex, &self.codex_session_id, "CODEX_SESSION_ID"),
        ]
        .into_iter()
        .filter_map(|(agent, id, key)| Some((agent, id.as_deref()?, key)))
        .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    ClaudeCode,
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
    Gemini,
}

impl Agent {
    pub const ALL: [Agent; 4] = [
        Agent::ClaudeCode,
        Agent::Codex,
        Agent::OpenCode,
        Agent::Gemini,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::OpenCode => "OpenCode",
            Agent::Gemini => "Gemini CLI",
        }
    }

    /// The command that starts it, as lists name it.
    pub fn short(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude",
            Agent::Codex => "codex",
            Agent::OpenCode => "opencode",
            Agent::Gemini => "gemini",
        }
    }

    /// The inverse of [`Agent::short`], for the store and the spool.
    pub fn from_short(s: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.short() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Absolute path, with the home directory written as `~` in recorded snapshots.
    pub root: String,
    pub kind: ProjectKind,
}

/// What marked the directory as a project root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    Git,
    Worktree,
    Submodule,
    Node,
    Cargo,
    Go,
    Python,
}

impl ProjectKind {
    /// The file or directory that marks this kind of root.
    pub fn marker(self) -> &'static str {
        match self {
            ProjectKind::Git | ProjectKind::Worktree | ProjectKind::Submodule => ".git",
            ProjectKind::Node => "package.json",
            ProjectKind::Cargo => "Cargo.toml",
            ProjectKind::Go => "go.mod",
            ProjectKind::Python => "pyproject.toml",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn name(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// Which address families a socket serves. `Dual` is an IPv6 socket that also accepts IPv4,
/// which netstat prints as `tcp46`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpFamily {
    V4,
    V6,
    Dual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    CloseWait,
    FinWait1,
    Closing,
    LastAck,
    FinWait2,
    TimeWait,
}

impl TcpState {
    /// The `TCPS_*` numbering from <netinet/tcp_fsm.h>.
    pub fn from_raw(v: i32) -> Option<TcpState> {
        use TcpState::*;
        Some(match v {
            0 => Closed,
            1 => Listen,
            2 => SynSent,
            3 => SynReceived,
            4 => Established,
            5 => CloseWait,
            6 => FinWait1,
            7 => Closing,
            8 => LastAck,
            9 => FinWait2,
            10 => TimeWait,
            _ => return None,
        })
    }
}

/// One TCP or UDP socket from the kernel's table, for any user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Socket {
    pub proto: Proto,
    pub family: IpFamily,
    pub local_ip: IpAddr,
    pub local_port: u16,
    pub remote_ip: IpAddr,
    pub remote_port: u16,
    /// `None` for UDP.
    pub state: Option<TcpState>,
    /// The last process that used the socket (`so_last_pid`). This is the pid netstat shows.
    pub pid: i32,
    /// The process the socket acts for when another one opened it (`so_e_pid`).
    pub epid: i32,
    pub uid: u32,
}

impl Socket {
    /// A TCP listener, or a UDP socket bound to a port and not connected to a peer.
    pub fn is_listening(&self) -> bool {
        match self.proto {
            Proto::Tcp => self.state == Some(TcpState::Listen),
            Proto::Udp => self.local_port != 0 && self.remote_port == 0,
        }
    }
}

/// A pulse metric that might not have a value yet. CPU needs two readings, so the first
/// one says so instead of showing zero.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Reading<T> {
    #[default]
    WarmingUp,
    Ready(T),
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    Normal,
    Warn,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swap {
    pub used: u64,
    pub total: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pulse {
    /// Busy share of all cores since the previous reading, 0 to 100.
    pub cpu_percent: Reading<f32>,
    pub memory: Reading<MemoryPressure>,
    pub swap: Reading<Swap>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: u32,
    pub taken_at_ms: i64,
    /// The uid the snapshot was taken as. "Yours" in every view means this uid.
    pub viewer_uid: u32,
    pub processes: Vec<Process>,
    #[serde(default)]
    pub sockets: Vec<Socket>,
    /// Account names for the other uids that own processes or sockets. The viewer is "you"
    /// and needs none.
    #[serde(default)]
    pub user_names: BTreeMap<u32, String>,
    #[serde(default)]
    pub pulse: Pulse,
}

/// One `statfs` reading of a volume. Bytes, from block counts times the block size.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSample {
    pub at_ms: i64,
    /// Mount point.
    pub volume: String,
    pub total_bytes: u64,
    /// Free blocks, including those reserved for root.
    pub free_bytes: u64,
    /// What an unprivileged process can still write.
    pub avail_bytes: u64,
    /// What Finder calls available: free space plus what macOS purges for an important
    /// write. The Disk view shows this, and the forecast follows it. None in samples recorded
    /// before Scry kept it.
    #[serde(default)]
    pub important_bytes: Option<u64>,
}

impl Process {
    /// What to call the process: its command label when Scry could read its arguments, its
    /// name otherwise.
    pub fn label(&self) -> String {
        match &self.command {
            Some(c) => c.to_string(),
            None => self.name.clone(),
        }
    }
}

impl Snapshot {
    pub fn own_processes(&self) -> impl Iterator<Item = &Process> {
        self.processes.iter().filter(|p| p.uid == self.viewer_uid)
    }

    pub fn process(&self, pid: i32) -> Option<&Process> {
        self.processes.iter().find(|p| p.id.pid == pid)
    }

    /// The process a multiprocessing worker works for: its nearest ancestor that is not a
    /// worker, since a forkserver stands between its workers and the program. Only the
    /// worker's own user's processes count, so an orphaned worker has none.
    pub fn worker_parent<'a>(&'a self, p: &Process) -> Option<&'a Process> {
        if !p.worker {
            return None;
        }
        let parent = |pid| self.process(pid).filter(|a| a.uid == p.uid && a.id.pid > 1);
        let mut q = parent(p.ppid)?;
        for _ in 0..3 {
            if !q.worker {
                return Some(q);
            }
            q = parent(q.ppid)?;
        }
        None
    }

    /// "you" for the viewer, the account name for others, `uid N` when the name is unknown.
    pub fn owner_name(&self, uid: u32) -> String {
        if uid == self.viewer_uid {
            return "you".into();
        }
        self.user_names
            .get(&uid)
            .cloned()
            .unwrap_or_else(|| format!("uid {uid}"))
    }
}
