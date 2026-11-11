use serde::{Deserialize, Serialize};

pub const SNAPSHOT_SCHEMA: u32 = 1;

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: u32,
    pub taken_at_ms: i64,
    /// The uid the snapshot was taken as. "Yours" in every view means this uid.
    pub viewer_uid: u32,
    pub processes: Vec<Process>,
}

impl Snapshot {
    pub fn own_processes(&self) -> impl Iterator<Item = &Process> {
        self.processes.iter().filter(|p| p.uid == self.viewer_uid)
    }
}
