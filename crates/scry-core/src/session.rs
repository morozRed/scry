//! Sessions and the processes that belong to them, as plain data (docs/architecture.md, "Which session owns a process").
//!
//! One agent process can host many sessions over time: `/clear`, `resume` and `compact`
//! start a new one in the same process. So a [`Session`] does not own a process; a
//! [`SessionProcess`] links the two for a time range, with the evidence for the link.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::{Agent, ProcId};
use crate::view::Evidence;

/// Which session. Most come with the agent's own id. An agent process seen without one (no
/// hook, no stamped child yet) gets a stand-in named after the process, so its children can
/// still be grouped under it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionId {
    Known(String),
    Unnamed(ProcId),
}

impl SessionId {
    /// The text kept in the store. A known id never holds `:`, so the two cannot collide.
    pub fn encode(&self) -> String {
        match self {
            SessionId::Known(id) => id.clone(),
            SessionId::Unnamed(p) => format!("pid:{}:{}", p.pid, p.start_us),
        }
    }

    pub fn decode(s: &str) -> Option<SessionId> {
        match s.strip_prefix("pid:") {
            Some(rest) => {
                let (pid, start) = rest.split_once(':')?;
                Some(SessionId::Unnamed(ProcId {
                    pid: pid.parse().ok()?,
                    start_us: start.parse().ok()?,
                }))
            }
            None => valid_id(s).then(|| SessionId::Known(s.to_string())),
        }
    }

    /// `4f2c9a1e`: the start of a known id, or `pid 1234` for a stand-in.
    pub fn short(&self) -> String {
        match self {
            SessionId::Known(id) => id.chars().take(8).collect(),
            SessionId::Unnamed(p) => format!("pid {}", p.pid),
        }
    }
}

/// An id has the shape of the ids agents hand out: 1 to 64 letters, digits, `-` and `_`. The
/// same check guards env stamps, so nothing else reaches the store, the spool or an output.
pub fn valid_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionKey {
    pub agent: Agent,
    pub id: SessionId,
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.agent.short(), self.id.short())
    }
}

/// Where Scry learned of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A SessionStart or SessionEnd hook.
    Hook,
    /// An env stamp on one of its processes.
    Stamp,
    /// An agent process seen in a scan.
    Scan,
}

impl Source {
    pub fn word(self) -> &'static str {
        match self {
            Source::Hook => "hook",
            Source::Stamp => "stamp",
            Source::Scan => "scan",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        [Source::Hook, Source::Stamp, Source::Scan]
            .into_iter()
            .find(|v| v.word() == s)
    }
}

/// Why a session ended. Claude Code's SessionEnd reasons are kept as it sends them, after the
/// id shape check; `agent_exited` is Scry's own, for an agent process that went away.
pub const AGENT_EXITED: &str = "agent_exited";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub key: SessionKey,
    /// The agent's working directory when the session started.
    pub cwd: Option<String>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub end_reason: Option<String>,
    pub source: Source,
}

impl Session {
    /// Whether a SessionStart at `at_ms` reopens it: `resume` starts an ended session again,
    /// and a start older than the end changes nothing.
    pub fn reopened_by(&self, at_ms: i64) -> bool {
        self.ended_at_ms.is_some_and(|end| end <= at_ms)
    }
}

/// A process's part in a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// The agent process hosting the session.
    Agent,
    /// Anything the session started.
    Child,
}

impl Part {
    pub fn word(self) -> &'static str {
        match self {
            Part::Agent => "agent",
            Part::Child => "child",
        }
    }

    pub fn parse(s: &str) -> Option<Part> {
        [Part::Agent, Part::Child]
            .into_iter()
            .find(|v| v.word() == s)
    }
}

/// One process in one session, over the time Scry saw it there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProcess {
    pub session: SessionKey,
    pub proc: ProcId,
    pub part: Part,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub evidence: Evidence,
}

/// What a hook reports, normalized across agents. One line of the spool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Random, so ingest can skip a line it already took.
    pub id: String,
    pub at_ms: i64,
    pub agent: Agent,
    pub session_id: String,
    pub kind: EventKind,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The agent process the hook ran under, found by walking up from the hook's parent.
    #[serde(default)]
    pub agent_proc: Option<ProcId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    /// `startup`, `resume`, `clear` or `compact` for Claude Code.
    Start { source: Option<String> },
    /// `clear`, `logout`, `prompt_input_exit`, `other` for Claude Code.
    End { reason: Option<String> },
}

impl SessionEvent {
    pub fn key(&self) -> SessionKey {
        SessionKey {
            agent: self.agent,
            id: SessionId::Known(self.session_id.clone()),
        }
    }

    /// The shape checks every reader applies before trusting a line.
    pub fn valid(&self) -> bool {
        let word = |s: &Option<String>| s.as_deref().is_none_or(valid_id);
        valid_id(&self.id)
            && valid_id(&self.session_id)
            && match &self.kind {
                EventKind::Start { source } => word(source),
                EventKind::End { reason } => word(reason),
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_reject_other_shapes() {
        let unnamed = SessionId::Unnamed(ProcId {
            pid: 12,
            start_us: 34,
        });
        for id in [SessionId::Known("4f2c9a1e-x_1".into()), unnamed] {
            assert_eq!(SessionId::decode(&id.encode()), Some(id));
        }
        for bad in ["", "a b", "../x", "pid:x:1", &"a".repeat(65)] {
            assert_eq!(SessionId::decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_event_with_a_bad_field_is_not_valid() {
        let ok = SessionEvent {
            id: "e1".into(),
            at_ms: 1,
            agent: Agent::ClaudeCode,
            session_id: "s1".into(),
            kind: EventKind::Start {
                source: Some("startup".into()),
            },
            cwd: None,
            agent_proc: None,
        };
        assert!(ok.valid());
        let mut bad = ok.clone();
        bad.session_id = "s 1".into();
        assert!(!bad.valid());
        let mut bad = ok;
        bad.kind = EventKind::End {
            reason: Some("x\u{1b}".into()),
        };
        assert!(!bad.valid());
    }
}
