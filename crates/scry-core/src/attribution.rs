//! Which session owns each process, and whether it is a leftover (PLAN.md section 5).
//!
//! [`Lineage::build`] is the one place both questions are answered. It takes a snapshot and
//! what Scry knows from before (sessions and recorded ancestry from the store, and hook
//! events from the spool) and returns every session with its processes, and one
//! [`Verdict`] per process of the viewer's. Views, the CLI and the daemon all read these;
//! none of them decides ownership or leftovers on its own.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use serde::Serialize;

use crate::catalog::{Infra, Role};
use crate::model::{Agent, ProcId, Process, Snapshot};
use crate::session::{
    AGENT_EXITED, EventKind, Part, Session, SessionEvent, SessionId, SessionKey, SessionProcess,
    Source,
};
use crate::view::Evidence;

/// How old a process must be before it can be a leftover. A dev server started a minute
/// before its session ended is more likely to be restarted than forgotten.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(300);

/// How far back the store is read: sessions ended and processes last seen within this.
pub const HISTORY: Duration = Duration::from_secs(7 * 86_400);

/// What Scry knew before this snapshot.
#[derive(Clone, Debug, Default)]
pub struct Known {
    pub sessions: Vec<Session>,
    pub processes: Vec<SessionProcess>,
}

impl Known {
    /// Folds hook events into sessions and agent processes, oldest first. The daemon writes
    /// the result to the store; clients without a daemon apply the spool in memory.
    pub fn apply(&mut self, events: &[SessionEvent]) {
        let mut events: Vec<&SessionEvent> = events.iter().filter(|e| e.valid()).collect();
        events.sort_by_key(|e| e.at_ms);
        for e in events {
            let key = e.key();
            let (started_at_ms, ended_at_ms, end_reason) = match &e.kind {
                EventKind::Start { .. } => (Some(e.at_ms), None, None),
                EventKind::End { reason } => (None, Some(e.at_ms), reason.clone()),
            };
            match self.sessions.iter_mut().find(|s| s.key == key) {
                Some(s) => {
                    if matches!(e.kind, EventKind::Start { .. }) && s.reopened_by(e.at_ms) {
                        s.ended_at_ms = None;
                        s.end_reason = None;
                    }
                    s.cwd = s.cwd.take().or(e.cwd.clone());
                    s.started_at_ms = s.started_at_ms.or(started_at_ms);
                    s.ended_at_ms = s.ended_at_ms.or(ended_at_ms);
                    s.end_reason = s.end_reason.take().or(end_reason);
                    s.source = Source::Hook;
                }
                None => self.sessions.push(Session {
                    key: key.clone(),
                    cwd: e.cwd.clone(),
                    started_at_ms,
                    ended_at_ms,
                    end_reason,
                    source: Source::Hook,
                }),
            }
            if let Some(proc) = e.agent_proc {
                self.note(SessionProcess {
                    session: key,
                    proc,
                    part: Part::Agent,
                    first_seen_ms: e.at_ms,
                    last_seen_ms: e.at_ms,
                    evidence: Evidence::Tree,
                });
            }
        }
    }

    fn note(&mut self, m: SessionProcess) {
        match self
            .processes
            .iter_mut()
            .find(|o| o.session == m.session && o.proc == m.proc)
        {
            Some(o) => o.last_seen_ms = o.last_seen_ms.max(m.last_seen_ms),
            None => self.processes.push(m),
        }
    }
}

/// The rule's settings.
#[derive(Clone, Debug)]
pub struct Policy {
    pub grace: Duration,
    /// Process names or executable paths no rule selects, from the config.
    pub keep: Vec<String>,
    /// Scry's own pid. Its tree is never a leftover.
    pub scry: Option<i32>,
    /// Why the config, and so the keep list, could not be read. While it is set no process
    /// is a leftover: an empty keep list would select what the user asked to keep.
    pub config_error: Option<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            grace: DEFAULT_GRACE,
            keep: Vec::new(),
            scry: Some(std::process::id() as i32),
            config_error: None,
        }
    }
}

/// Whether a session's agent process is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentState {
    Live {
        pid: i32,
        start_us: i64,
    },
    /// Scry knew the agent process, and it has exited. `start_us` is unknown when only a
    /// `CLAUDE_PID` stamp named it.
    Gone {
        pid: i32,
        start_us: Option<i64>,
    },
    /// A process that may be the agent still runs, but the catalog does not name it as one:
    /// its executable could not be read, or the binary was renamed. It is not gone.
    Unrecognised {
        pid: i32,
        start_us: i64,
    },
    /// No agent process is known for the session, so Scry cannot tell whether it ended.
    Unknown,
}

/// How a process was tied to its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum How {
    /// An allowlisted key in its environment.
    Stamp { key: &'static str },
    /// A live parent chain up to the agent process.
    Tree { agent: i32 },
    /// The daemon saw it in the agent's tree while the agent lived.
    Recorded { evidence: Evidence, since_ms: i64 },
    /// It started inside the session's time window, in its directory.
    Guess,
}

impl How {
    pub fn evidence(self) -> Evidence {
        match self {
            How::Stamp { .. } => Evidence::Stamp,
            How::Tree { .. } => Evidence::Tree,
            How::Recorded { evidence, .. } => evidence,
            How::Guess => Evidence::Guess,
        }
    }

    /// The evidence line the detail pane and `--json` show.
    pub fn sentence(self) -> String {
        match self {
            How::Stamp { key } => format!("{key} in the process environment (exact)"),
            How::Tree { agent } => format!("A child of agent process {agent} (exact)"),
            How::Recorded { .. } => {
                "Seen in the agent's process tree before the agent exited (exact)".into()
            }
            How::Guess => {
                "Started inside the session's time window in its directory (guess)".into()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Member {
    pub proc: ProcId,
    pub how: How,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionView {
    pub key: SessionKey,
    /// What the hooks and the store know of it.
    pub session: Option<Session>,
    pub agent: AgentState,
    /// The session's processes other than its agent's, parents before children.
    pub members: Vec<Member>,
}

impl SessionView {
    pub fn ended(&self) -> bool {
        matches!(self.agent, AgentState::Gone { .. })
            || self
                .session
                .as_ref()
                .is_some_and(|s| s.ended_at_ms.is_some())
    }
}

/// Whether a process is a leftover, with the reason. Exactly one per process of the
/// viewer's that is not itself an agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Leftover {
        session: SessionKey,
        how: How,
        /// The session's agent process, which has exited.
        agent_pid: i32,
    },
    Spared {
        #[serde(flatten)]
        why: Spare,
    },
}

/// Why a process is not a leftover: the first rule it fails, in the order of PLAN.md.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "spared", rename_all = "snake_case")]
pub enum Spare {
    /// Scry itself, or something it started.
    Scry,
    /// No session owns it. A process re-parented to launchd lands here unless a stamp or
    /// recorded ancestry ties it to a session.
    Unattributed,
    /// Only a time-and-place guess ties it to a session.
    OnlyGuess,
    AgentLive {
        pid: i32,
    },
    AgentUnknown,
    /// Its agent process may still run, but the catalog does not recognise it.
    AgentUnrecognised {
        pid: i32,
    },
    NeverReap {
        what: Infra,
    },
    /// A live process above it is on the never-reap list, such as a `tmux` server or an
    /// editor: that process holds it, so a stamp it inherited says nothing about whether
    /// it was abandoned.
    UnderNeverReap {
        pid: i32,
        what: Infra,
    },
    /// A live multiplexer or app with the same stamp started before it. A job it started
    /// and that then detached (a daemon from a `tmux` pane) carries the stamp too, so the
    /// stamp does not show the session started it.
    StampCarried {
        pid: i32,
        what: Infra,
    },
    Tty {
        tty: String,
    },
    TtyDescendant {
        pid: i32,
        tty: String,
    },
    Young {
        age_s: u64,
        grace_s: u64,
    },
    Kept {
        entry: String,
    },
    /// The config could not be read, so the keep list is unknown.
    ConfigUnread {
        error: String,
    },
}

impl Spare {
    /// Why, as the detail pane says it.
    pub fn sentence(&self) -> String {
        match self {
            Spare::Scry => "It is part of Scry.".into(),
            Spare::Unattributed => "No session is known to own it.".into(),
            Spare::OnlyGuess => "Its link to the session is only a guess.".into(),
            Spare::AgentLive { pid } => format!("Its agent process {pid} is still running."),
            Spare::AgentUnknown => {
                "Scry has not seen its agent process, so it cannot tell whether the session ended."
                    .into()
            }
            Spare::AgentUnrecognised { pid } => format!(
                "Process {pid}, which may be its agent, still runs, though Scry does not recognise it as one."
            ),
            Spare::NeverReap { what } => format!("It is never reaped: {}.", what.why()),
            Spare::UnderNeverReap { pid, what } => format!(
                "It runs under process {pid}, which is never reaped: {}.",
                what.why()
            ),
            Spare::StampCarried { pid, what } => format!(
                "Process {pid} carries the same stamp and started first ({}), so the stamp may have come through it.",
                what.why()
            ),
            Spare::Tty { tty } => format!("It still has a terminal ({tty})."),
            Spare::TtyDescendant { pid, tty } => {
                format!("Its child {pid} still has a terminal ({tty}).")
            }
            Spare::Young { age_s, grace_s } => format!(
                "It is {} old, under the {} grace period.",
                crate::view::human_age(*age_s),
                crate::view::human_age(*grace_s)
            ),
            Spare::Kept { entry } => format!("It is on your keep list as `{entry}`."),
            Spare::ConfigUnread { error } => format!(
                "Your config could not be read, so Scry selects no leftovers until it is fixed: {error}"
            ),
        }
    }
}

/// Every session and every verdict for one snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Lineage {
    pub sessions: Vec<SessionView>,
    /// Keyed by pid; each verdict is for the process with that pid in the snapshot.
    pub verdicts: BTreeMap<i32, Verdict>,
}

struct Ctx<'a> {
    by_pid: HashMap<i32, &'a Process>,
    hosts: HashMap<i32, Agent>,
    /// Live multiplexers and apps with a stamp. Each hands its environment to every job it
    /// starts, long after the session that stamped it.
    carriers: Vec<(&'a Process, Infra)>,
    /// Live agents with a stamp: an agent another agent started, such as a headless Gemini
    /// that Claude Code ran, inherits the outer session's stamp and passes it on.
    nested: Vec<&'a Process>,
}

/// Whether `q` carries `session`'s stamp.
fn carries(q: &Process, session: &SessionKey) -> bool {
    q.stamps.sessions().iter().any(|&(agent, id, _)| {
        agent == session.agent && session.id == SessionId::Known(id.to_string())
    })
}

impl<'a> Ctx<'a> {
    fn new(snap: &'a Snapshot) -> Self {
        let by_pid: HashMap<i32, &Process> = snap.processes.iter().map(|p| (p.id.pid, p)).collect();
        let hosts: HashMap<i32, Agent> = snap
            .own_processes()
            .filter_map(|p| Some((p.id.pid, p.role.and_then(Role::hosts)?)))
            .collect();
        let carriers = snap
            .own_processes()
            .filter(|p| !p.stamps.is_empty())
            .filter_map(|p| match p.role.and_then(Role::never_reap)? {
                what @ (Infra::Multiplexer | Infra::AppHelper) => Some((p, what)),
                _ => None,
            })
            .collect();
        let nested = snap
            .own_processes()
            .filter(|p| !p.stamps.is_empty() && hosts.contains_key(&p.id.pid))
            .collect();
        Ctx {
            by_pid,
            hosts,
            carriers,
            nested,
        }
    }

    /// A live carrier with `session`'s stamp that started before `p`, so `p`'s copy of the
    /// stamp may have come through it rather than from the session.
    fn carrier_of(&self, p: &Process, session: &SessionKey) -> Option<(i32, Infra)> {
        self.carriers
            .iter()
            .filter(|(q, _)| q.id.pid != p.id.pid && q.id.start_us <= p.id.start_us)
            .find(|(q, _)| carries(q, session))
            .map(|(q, what)| (q.id.pid, *what))
    }

    /// A live agent with `session`'s stamp that started before `p`. It may have started `p`
    /// and still be working with it, so it keeps `session`'s processes alive the way the
    /// session's own agent would.
    fn nested_agent(&self, p: &Process, session: &SessionKey) -> Option<AgentState> {
        self.nested
            .iter()
            .filter(|q| q.id.pid != p.id.pid && q.id.start_us <= p.id.start_us)
            .find(|q| carries(q, session))
            .map(|q| AgentState::Live {
                pid: q.id.pid,
                start_us: q.id.start_us,
            })
    }

    /// The nearest agent process above `p`, through live parents.
    fn host_above(&self, p: &Process) -> Option<&'a Process> {
        let mut pid = p.ppid;
        for _ in 0..64 {
            if pid <= 1 {
                return None;
            }
            let q = self.by_pid.get(&pid)?;
            if self.hosts.contains_key(&pid) {
                return Some(q);
            }
            pid = q.ppid;
        }
        None
    }

    /// The nearest live process above `p` on the never-reap list.
    fn never_reap_above(&self, p: &Process) -> Option<(i32, Infra)> {
        let mut pid = p.ppid;
        for _ in 0..64 {
            if pid <= 1 {
                return None;
            }
            let q = self.by_pid.get(&pid)?;
            if let Some(what) = q.role.and_then(Role::never_reap) {
                return Some((pid, what));
            }
            pid = q.ppid;
        }
        None
    }

    fn descends_from(&self, p: &Process, ancestor: i32) -> bool {
        let mut pid = p.id.pid;
        for _ in 0..64 {
            if pid == ancestor {
                return true;
            }
            match self.by_pid.get(&pid) {
                Some(q) if q.ppid > 1 => pid = q.ppid,
                _ => return false,
            }
        }
        false
    }
}

impl Lineage {
    pub fn build(snap: &Snapshot, known: &Known, policy: &Policy) -> Lineage {
        let cx = Ctx::new(snap);
        let now_us = snap.taken_at_ms * 1000;

        // Each agent process's current session: the newest open one a hook tied to it,
        // else the newest stamp among the processes below it, else a stand-in.
        let mut current: HashMap<i32, SessionKey> = HashMap::new();
        for (&pid, &agent) in &cx.hosts {
            let id = cx.by_pid[&pid].id;
            let hooked = known
                .processes
                .iter()
                .filter(|m| m.part == Part::Agent && m.proc == id && m.session.agent == agent)
                .filter_map(|m| {
                    let s = known.sessions.iter().find(|s| s.key == m.session);
                    (s.is_none_or(|s| s.ended_at_ms.is_none())).then(|| {
                        (
                            s.and_then(|s| s.started_at_ms).unwrap_or(m.first_seen_ms),
                            &m.session,
                        )
                    })
                })
                .max_by_key(|(at, _)| *at)
                .map(|(_, k)| k.clone());
            let stamped = || {
                snap.own_processes()
                    .filter(|p| !cx.hosts.contains_key(&p.id.pid))
                    .filter(|p| {
                        cx.host_above(p).is_some_and(|h| h.id.pid == pid)
                            || (p.stamps.claude_pid == Some(pid) && p.id.start_us >= id.start_us)
                    })
                    .filter_map(|p| {
                        let (a, sid, _) = p.stamps.session()?;
                        (a == agent).then_some((p.id.start_us, sid))
                    })
                    .max_by_key(|(start, _)| *start)
                    .map(|(_, sid)| SessionKey {
                        agent,
                        id: SessionId::Known(sid.to_string()),
                    })
            };
            let key = hooked.or_else(stamped).unwrap_or(SessionKey {
                agent,
                id: SessionId::Unnamed(id),
            });
            current.insert(pid, key);
        }

        // Agent processes each session is known to have had, live or not.
        let mut agents: BTreeMap<SessionKey, Vec<(i32, Option<i64>)>> = BTreeMap::new();
        let mut link = |key: &SessionKey, pid: i32, start: Option<i64>| {
            let v = agents.entry(key.clone()).or_default();
            if !v.contains(&(pid, start)) {
                v.push((pid, start));
            }
        };
        for (&pid, key) in &current {
            link(key, pid, Some(cx.by_pid[&pid].id.start_us));
        }
        for m in known.processes.iter().filter(|m| m.part == Part::Agent) {
            link(&m.session, m.proc.pid, Some(m.proc.start_us));
        }

        // Owners, and every session that claims each process. A process can be recorded under
        // several sessions (one agent process hosts them in turn), and none of those
        // records replaces another.
        let mut recorded: HashMap<ProcId, Vec<&SessionProcess>> = HashMap::new();
        for m in known
            .processes
            .iter()
            .filter(|m| m.part == Part::Child && m.evidence.exact())
        {
            recorded.entry(m.proc).or_default().push(m);
        }
        for list in recorded.values_mut() {
            list.sort_by_key(|m| (m.first_seen_ms, m.session.clone()));
        }
        let mut owners: BTreeMap<i32, (SessionKey, How)> = BTreeMap::new();
        let mut claims: HashMap<i32, Vec<SessionKey>> = HashMap::new();
        for p in snap.own_processes() {
            if cx.hosts.contains_key(&p.id.pid) {
                continue;
            }
            let host = cx.host_above(p);
            // A stamp from a session that ended before the process started was inherited
            // through something that outlived the session, so it names the wrong one.
            let stamps: Vec<(SessionKey, &'static str)> = p
                .stamps
                .sessions()
                .into_iter()
                .map(|(agent, sid, key)| {
                    let session = SessionKey {
                        agent,
                        id: SessionId::Known(sid.to_string()),
                    };
                    (session, key)
                })
                .filter(|(session, _)| !ended_before(known, session, p.id.start_us))
                .collect();
            for (session, _) in stamps.iter().filter(|(s, _)| s.agent == Agent::ClaudeCode) {
                if let Some(cp) = p.stamps.claude_pid {
                    // The holder of the pid may be the agent only if it started first.
                    match cx.by_pid.get(&cp) {
                        Some(a) if a.id.start_us <= p.id.start_us => {
                            link(session, cp, Some(a.id.start_us))
                        }
                        _ => link(session, cp, None),
                    }
                }
            }
            // A live agent above wins over a stamp of another agent's: Claude Code starting
            // Codex passes its stamp on to Codex's workers.
            let stamp = match host {
                Some(h) => stamps.iter().find(|(s, _)| s.agent == cx.hosts[&h.id.pid]),
                None => stamps.first(),
            };
            let owner = if let Some((session, key)) = stamp {
                if let Some(h) = host {
                    link(session, h.id.pid, Some(h.id.start_us));
                }
                Some((session.clone(), How::Stamp { key }))
            } else if let Some(h) = host {
                Some((current[&h.id.pid].clone(), How::Tree { agent: h.id.pid }))
            } else if let Some(m) = recorded.get(&p.id).and_then(|l| l.first()) {
                Some((
                    m.session.clone(),
                    How::Recorded {
                        evidence: m.evidence,
                        since_ms: m.first_seen_ms,
                    },
                ))
            } else {
                guess(p, known).map(|k| (k, How::Guess))
            };
            if let Some(o) = owner {
                let mut c = vec![o.0.clone()];
                c.extend(stamps.iter().map(|(s, _)| s.clone()));
                c.extend(
                    recorded
                        .get(&p.id)
                        .into_iter()
                        .flatten()
                        .map(|m| m.session.clone()),
                );
                let mut seen = HashSet::new();
                c.retain(|k| seen.insert(k.clone()));
                claims.insert(p.id.pid, c);
                owners.insert(p.id.pid, o);
            }
        }

        let state = |key: &SessionKey| -> AgentState {
            let Some(list) = agents.get(key) else {
                return AgentState::Unknown;
            };
            // No start means the live holder of the pid was checked and is not it.
            let running = |&&(pid, start): &&(i32, Option<i64>)| {
                cx.by_pid
                    .get(&pid)
                    .is_some_and(|p| start == Some(p.id.start_us))
            };
            if let Some(&(pid, Some(start_us))) = list
                .iter()
                .filter(running)
                .find(|(pid, _)| cx.hosts.contains_key(pid))
            {
                return AgentState::Live { pid, start_us };
            }
            // Still running though the catalog does not name it: not proof it exited.
            if let Some(&(pid, Some(start_us))) = list.iter().find(running) {
                return AgentState::Unrecognised { pid, start_us };
            }
            let &(pid, start_us) = list.last().expect("a listed session has an agent");
            AgentState::Gone { pid, start_us }
        };

        // Sessions: every one with an agent process or a member, plus open hook sessions.
        let mut keys: Vec<SessionKey> = current.values().cloned().collect();
        keys.extend(owners.values().map(|(k, _)| k.clone()));
        keys.sort();
        keys.dedup();
        let mut sessions: Vec<SessionView> = keys
            .into_iter()
            .map(|key| SessionView {
                session: known.sessions.iter().find(|s| s.key == key).cloned(),
                agent: state(&key),
                members: Vec::new(),
                key,
            })
            .collect();
        let index: HashMap<SessionKey, usize> = sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (s.key.clone(), i))
            .collect();
        for p in &snap.processes {
            if let Some((key, how)) = owners.get(&p.id.pid) {
                sessions[index[key]].members.push(Member {
                    proc: p.id,
                    how: *how,
                });
            }
        }
        for s in &mut sessions {
            s.members.sort_by_key(|m| (m.proc.start_us, m.proc.pid));
        }

        // Verdicts.
        let mut children: HashMap<i32, Vec<&Process>> = HashMap::new();
        for p in snap.own_processes() {
            children.entry(p.ppid).or_default().push(p);
        }
        let mut verdicts = BTreeMap::new();
        for p in snap.own_processes() {
            if cx.hosts.contains_key(&p.id.pid) {
                continue;
            }
            let owner = owners.get(&p.id.pid);
            // A leftover only when every session that claims it has lost its agent.
            let agent = claims.get(&p.id.pid).map(|keys| {
                let states: Vec<AgentState> = keys
                    .iter()
                    .map(|k| match state(k) {
                        live @ AgentState::Live { .. } => live,
                        other => cx.nested_agent(p, k).unwrap_or(other),
                    })
                    .collect();
                let live = states.iter().find(|a| matches!(a, AgentState::Live { .. }));
                let unsure = states
                    .iter()
                    .find(|a| !matches!(a, AgentState::Gone { .. }));
                *live.or(unsure).unwrap_or(&states[0])
            });
            let v = judge(p, owner, agent, &cx, &children, policy, now_us);
            verdicts.insert(p.id.pid, v);
        }
        Lineage { sessions, verdicts }
    }

    pub fn verdict(&self, pid: i32) -> Option<&Verdict> {
        self.verdicts.get(&pid)
    }

    pub fn is_leftover(&self, pid: i32) -> bool {
        matches!(self.verdicts.get(&pid), Some(Verdict::Leftover { .. }))
    }

    pub fn session_of(&self, pid: i32) -> Option<(&SessionView, &Member)> {
        self.sessions
            .iter()
            .find_map(|s| Some((s, s.members.iter().find(|m| m.proc.pid == pid)?)))
    }

    /// What the daemon writes after a scan: sessions it learned of, ended ones it saw end,
    /// and every exact link of a live process, which is how a tree outlives its agent.
    pub fn records(&self, snap: &Snapshot, now_ms: i64) -> (Vec<Session>, Vec<SessionProcess>) {
        let mut sessions = Vec::new();
        let mut procs = Vec::new();
        let start_ms = |pid: i32| snap.process(pid).map(|p| p.id.start_us / 1000);
        for s in &self.sessions {
            let ended = matches!(s.agent, AgentState::Gone { .. })
                && s.session.as_ref().is_none_or(|k| k.ended_at_ms.is_none());
            let source = match (&s.session, &s.key.id) {
                (Some(k), _) => k.source,
                (None, SessionId::Unnamed(_)) => Source::Scan,
                (None, SessionId::Known(_)) => Source::Stamp,
            };
            sessions.push(Session {
                key: s.key.clone(),
                cwd: None,
                started_at_ms: match s.agent {
                    AgentState::Live { pid, .. } => start_ms(pid),
                    _ => None,
                },
                ended_at_ms: ended.then_some(now_ms),
                end_reason: ended.then(|| AGENT_EXITED.to_string()),
                source,
            });
            if let AgentState::Live { pid, start_us } = s.agent {
                procs.push(SessionProcess {
                    session: s.key.clone(),
                    proc: ProcId { pid, start_us },
                    part: Part::Agent,
                    first_seen_ms: now_ms,
                    last_seen_ms: now_ms,
                    evidence: Evidence::Tree,
                });
            }
            for m in &s.members {
                let evidence = match m.how {
                    How::Stamp { .. } => Evidence::Stamp,
                    How::Tree { .. } => Evidence::Tree,
                    How::Recorded { evidence, .. } => evidence,
                    How::Guess => continue,
                };
                procs.push(SessionProcess {
                    session: s.key.clone(),
                    proc: m.proc,
                    part: Part::Child,
                    first_seen_ms: now_ms,
                    last_seen_ms: now_ms,
                    evidence,
                });
            }
        }
        (sessions, procs)
    }
}

/// Whether the store or a hook says `session` ended before a process started at `start_us`.
fn ended_before(known: &Known, session: &SessionKey, start_us: i64) -> bool {
    known
        .sessions
        .iter()
        .find(|s| s.key == *session)
        .and_then(|s| s.ended_at_ms)
        .is_some_and(|end| start_us / 1000 > end)
}

/// The newest session whose time window holds the process's start and whose directory
/// holds its working directory. A session started in the home directory or `/` guesses
/// nothing, since everything is inside it.
fn guess(p: &Process, known: &Known) -> Option<SessionKey> {
    let cwd = p.cwd.as_deref()?;
    let start_ms = p.id.start_us / 1000;
    known
        .sessions
        .iter()
        .filter(|s| {
            let Some(dir) = s.cwd.as_deref().filter(|d| d.matches('/').count() >= 3) else {
                return false;
            };
            let inside = cwd == dir || cwd.starts_with(&format!("{dir}/"));
            let after = s.started_at_ms.is_some_and(|t| start_ms >= t);
            let before = s.ended_at_ms.is_none_or(|t| start_ms <= t);
            inside && after && before
        })
        .max_by_key(|s| s.started_at_ms)
        .map(|s| s.key.clone())
}

/// The leftover rule, in PLAN.md's order. The first rule a process fails is its reason.
fn judge(
    p: &Process,
    owner: Option<&(SessionKey, How)>,
    agent: Option<AgentState>,
    cx: &Ctx,
    children: &HashMap<i32, Vec<&Process>>,
    policy: &Policy,
    now_us: i64,
) -> Verdict {
    let spared = |why| Verdict::Spared { why };
    if policy.scry.is_some_and(|me| cx.descends_from(p, me)) {
        return spared(Spare::Scry);
    }
    let Some((session, how)) = owner else {
        return spared(Spare::Unattributed);
    };
    if !how.evidence().exact() {
        return spared(Spare::OnlyGuess);
    }
    let agent_pid = match agent {
        Some(AgentState::Gone { pid, .. }) => pid,
        Some(AgentState::Live { pid, .. }) => return spared(Spare::AgentLive { pid }),
        Some(AgentState::Unrecognised { pid, .. }) => {
            return spared(Spare::AgentUnrecognised { pid });
        }
        _ => return spared(Spare::AgentUnknown),
    };
    if let Some(what) = p.role.and_then(Role::never_reap) {
        return spared(Spare::NeverReap { what });
    }
    if let Some((pid, what)) = cx.never_reap_above(p) {
        return spared(Spare::UnderNeverReap { pid, what });
    }
    // Recorded stamp evidence is the same stamp, read before the process dropped it (a job
    // that exec'd with a clean environment), so a carrier spares it just as well.
    let stamped = matches!(
        how,
        How::Stamp { .. }
            | How::Recorded {
                evidence: Evidence::Stamp,
                ..
            }
    );
    if stamped && let Some((pid, what)) = cx.carrier_of(p, session)
    {
        return spared(Spare::StampCarried { pid, what });
    }
    if let Some(tty) = &p.tty {
        return spared(Spare::Tty { tty: tty.clone() });
    }
    if let Some((pid, tty)) = tty_below(p, children) {
        return spared(Spare::TtyDescendant { pid, tty });
    }
    let age_s = ((now_us - p.id.start_us).max(0) / 1_000_000) as u64;
    if age_s < policy.grace.as_secs() {
        return spared(Spare::Young {
            age_s,
            grace_s: policy.grace.as_secs(),
        });
    }
    if let Some(entry) = kept(p, &policy.keep) {
        return spared(Spare::Kept { entry });
    }
    if let Some(error) = &policy.config_error {
        return spared(Spare::ConfigUnread {
            error: error.clone(),
        });
    }
    Verdict::Leftover {
        session: session.clone(),
        how: *how,
        agent_pid,
    }
}

fn tty_below(p: &Process, children: &HashMap<i32, Vec<&Process>>) -> Option<(i32, String)> {
    let mut stack = vec![p.id.pid];
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        for c in children.get(&pid).into_iter().flatten() {
            if let Some(tty) = &c.tty {
                return Some((c.id.pid, tty.clone()));
            }
            stack.push(c.id.pid);
        }
    }
    None
}

/// A keep entry names a process by its name or its executable's full path.
fn kept(p: &Process, keep: &[String]) -> Option<String> {
    keep.iter()
        .find(|k| **k == p.name || p.exe.as_deref() == Some(k.as_str()))
        .cloned()
}

#[cfg(test)]
mod tests;
