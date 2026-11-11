use serde::{Deserialize, Serialize};

use super::{Detail, Evidence, State, Target};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    Tcp,
    Udp,
}

/// One listening socket and who holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRow {
    pub id: String,
    pub port: u16,
    pub proto: Proto,
    /// `*`, `127.0.0.1`, `::1`.
    pub addr: String,
    pub command: String,
    pub pid: i32,
    pub project: Option<String>,
    /// The owning session, as `claude acme/wt-auth`.
    pub session: Option<String>,
    /// `None` when it is the viewer's own process.
    pub owner: Option<String>,
    pub ram: Option<u64>,
    pub age_s: u64,
    pub evidence: Option<Evidence>,
    pub state: State,
    pub detail: Detail,
}

impl PortRow {
    /// Only the viewer's own processes can be stopped.
    pub fn stop_target(&self) -> Option<Target> {
        (self.owner.is_none() && self.state != State::System).then(|| Target {
            pid: self.pid,
            command: self.command.clone(),
            session: self.session.clone(),
            evidence: self.evidence,
            age_s: self.age_s,
            frees: self.ram,
        })
    }
}
