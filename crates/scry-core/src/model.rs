//! The snapshot: processes, sockets and the pulse as plain, serializable data.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

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
    /// The command line after `redact::argv`. Raw arguments are never stored. Readable only
    /// for the viewer's own processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Project>,
    #[serde(default, skip_serializing_if = "Stamps::is_empty")]
    pub stamps: Stamps,
}

/// The allowlisted agent keys from a process's environment. Every other variable is dropped
/// while the environment is parsed.
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

    /// Takes one `KEY=value` environment entry, keeping it only if the key is allowlisted.
    pub fn take(&mut self, entry: &[u8]) {
        let Some(eq) = entry.iter().position(|&b| b == b'=') else {
            return;
        };
        let (key, value) = (&entry[..eq], &entry[eq + 1..]);
        let text = || String::from_utf8_lossy(value).into_owned();
        match key {
            b"CLAUDE_CODE_SESSION_ID" => self.claude_code_session_id = Some(text()),
            b"CLAUDE_PID" => self.claude_pid = text().parse().ok(),
            b"CODEX_SESSION_ID" => self.codex_session_id = Some(text()),
            b"SCRY_SESSION" => self.scry_session = Some(text()),
            _ => {}
        }
    }

    /// The agent and session id a stamp names, strongest first.
    pub fn session(&self) -> Option<(Agent, &str, &'static str)> {
        if let Some(id) = &self.scry_session {
            return Some((Agent::ClaudeCode, id, "SCRY_SESSION"));
        }
        if let Some(id) = &self.claude_code_session_id {
            return Some((Agent::ClaudeCode, id, "CLAUDE_CODE_SESSION_ID"));
        }
        if let Some(id) = &self.codex_session_id {
            return Some((Agent::Codex, id, "CODEX_SESSION_ID"));
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    ClaudeCode,
    Codex,
}

impl Agent {
    pub fn title(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
        }
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
}

impl Snapshot {
    pub fn own_processes(&self) -> impl Iterator<Item = &Process> {
        self.processes.iter().filter(|p| p.uid == self.viewer_uid)
    }

    pub fn process(&self, pid: i32) -> Option<&Process> {
        self.processes.iter().find(|p| p.id.pid == pid)
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
