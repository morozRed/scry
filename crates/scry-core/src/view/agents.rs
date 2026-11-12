//! The Agents view model, built from a [`Lineage`], and the reap rule's target selection.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{Detail, Evidence, Fact, State, Target, human_age, human_bytes};
use crate::attribution::{AgentState, Lineage, Member, SessionView, Spare, Verdict};
use crate::home::Home;
use crate::model::{Agent, Process, Snapshot};
use crate::session::{SessionKey, Source};

/// One line of the agent tree, flattened depth first. Sessions sit at depth 0 and the
/// processes they own below them, so columns align across depths in every skin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub depth: u8,
    /// The agent for a session (`claude`), the command for a process (`next dev`).
    pub label: String,
    /// The project for a session.
    pub context: Option<String>,
    pub pid: Option<i32>,
    pub ram: Option<u64>,
    #[serde(default)]
    pub ports: Vec<u16>,
    pub age_s: u64,
    pub evidence: Option<Evidence>,
    pub state: Option<State>,
    pub detail: Detail,
}

impl AgentRow {
    pub fn session_name(&self) -> String {
        match &self.context {
            Some(c) => format!("{} {c}", self.label),
            None => self.label.clone(),
        }
    }
}

/// What `X` would reap: leftovers tied by `stamp` or `tree`, never a `guess`, children
/// before their parents so nothing is re-parented to launchd mid-way.
pub fn reap_targets(rows: &[AgentRow]) -> Vec<Target> {
    let mut picked: Vec<(u8, usize, Target)> = Vec::new();
    let mut session: Option<String> = None;
    for (i, row) in rows.iter().enumerate() {
        if row.depth == 0 {
            session = Some(row.session_name());
        }
        let exact = matches!(row.evidence, Some(Evidence::Stamp | Evidence::Tree));
        if let (Some(State::Leftover), true, Some(pid)) = (row.state, exact, row.pid) {
            picked.push((
                row.depth,
                i,
                Target {
                    pid,
                    command: row.label.clone(),
                    session: session.clone(),
                    evidence: row.evidence,
                    age_s: row.age_s,
                    frees: row.ram,
                },
            ));
        }
    }
    picked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    picked.into_iter().map(|(_, _, t)| t).collect()
}

/// The stop target for one process row, with the session it belongs to.
pub fn stop_target(rows: &[AgentRow], index: usize) -> Option<Target> {
    let row = rows.get(index)?;
    let pid = row.pid?;
    let session = rows[..=index]
        .iter()
        .rev()
        .find(|r| r.depth == 0)
        .filter(|s| s.pid != Some(pid))
        .map(AgentRow::session_name);
    Some(Target {
        pid,
        command: row.label.clone(),
        session,
        evidence: row.evidence,
        age_s: row.age_s,
        frees: row.ram,
    })
}

/// What `scry leftovers` lists: one process the leftover rule selects, with why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leftover {
    pub pid: i32,
    pub start_us: i64,
    pub command: String,
    pub agent: Agent,
    /// The session's id, or `pid N` when Scry never learned it.
    pub session: String,
    pub evidence: Evidence,
    /// The evidence, in a sentence.
    pub how: String,
    /// The session's agent process, which has exited.
    pub agent_pid: i32,
    pub age_s: u64,
    pub ram: Option<u64>,
    #[serde(default)]
    pub ports: Vec<u16>,
    pub cwd: Option<String>,
}

/// Every process the leftover rule selects, oldest first.
pub fn leftovers(snap: &Snapshot, lineage: &Lineage, home: Option<&Home>) -> Vec<Leftover> {
    let ports = listening(snap);
    let mut out: Vec<Leftover> = snap
        .own_processes()
        .filter_map(|p| match lineage.verdict(p.id.pid)? {
            Verdict::Leftover {
                session,
                how,
                agent_pid,
            } => Some(Leftover {
                pid: p.id.pid,
                start_us: p.id.start_us,
                command: label(snap, p),
                agent: session.agent,
                session: session.id.short(),
                evidence: how.evidence(),
                how: how.sentence(),
                agent_pid: *agent_pid,
                age_s: age(snap, p.id.start_us),
                ram: p.footprint,
                ports: ports.get(&p.id.pid).cloned().unwrap_or_default(),
                cwd: p.cwd.as_deref().map(|c| shown(home, c)),
            }),
            Verdict::Spared { .. } => None,
        })
        .collect();
    out.sort_by_key(|l| (l.start_us, l.pid));
    out
}

/// The Agents view: each session, then its processes as a tree. Sessions holding
/// leftovers come first, then live ones, newest first within each.
pub fn rows(snap: &Snapshot, lineage: &Lineage, home: Option<&Home>) -> Vec<AgentRow> {
    let ports = listening(snap);
    let mut order: Vec<&SessionView> = lineage.sessions.iter().collect();
    let left = |s: &SessionView| {
        s.members
            .iter()
            .filter(|m| lineage.is_leftover(m.proc.pid))
            .count()
    };
    let rank = |s: &SessionView| match (left(s) > 0, &s.agent) {
        (true, _) => 0,
        (false, AgentState::Live { .. }) if !s.ended() => 1,
        _ => 2,
    };
    order.sort_by_key(|s| (rank(s), std::cmp::Reverse(started_us(snap, s))));
    let mut out = Vec::new();
    for s in order {
        out.push(session_row(snap, lineage, s, &ports, home, left(s)));
        let pids: HashSet<i32> = s.members.iter().map(|m| m.proc.pid).collect();
        let mut kids: BTreeMap<i32, Vec<&Member>> = BTreeMap::new();
        let mut tops = Vec::new();
        for m in &s.members {
            match snap.process(m.proc.pid).map(|p| p.ppid) {
                Some(pp) if pids.contains(&pp) => kids.entry(pp).or_default().push(m),
                _ => tops.push(m),
            }
        }
        let mut stack: Vec<(&Member, u8)> = tops.into_iter().rev().map(|m| (m, 1)).collect();
        while let Some((m, depth)) = stack.pop() {
            if let Some(p) = snap.process(m.proc.pid) {
                out.push(member_row(snap, lineage, s, m, p, depth, &ports, home));
            }
            for k in kids.get(&m.proc.pid).into_iter().flatten().rev() {
                stack.push((k, depth.saturating_add(1).min(8)));
            }
        }
    }
    out
}

fn session_row(
    snap: &Snapshot,
    lineage: &Lineage,
    s: &SessionView,
    ports: &HashMap<i32, Vec<u16>>,
    home: Option<&Home>,
    leftovers: usize,
) -> AgentRow {
    let agent = match s.agent {
        AgentState::Live { pid, .. } => snap.process(pid),
        _ => None,
    };
    let procs: Vec<&Process> = agent
        .into_iter()
        .chain(s.members.iter().filter_map(|m| snap.process(m.proc.pid)))
        .collect();
    let ram: u64 = procs.iter().filter_map(|p| p.footprint).sum();
    let mut held: Vec<u16> = procs
        .iter()
        .flat_map(|p| ports.get(&p.id.pid).cloned().unwrap_or_default())
        .collect();
    held.sort_unstable();
    held.dedup();
    let cwd = s
        .session
        .as_ref()
        .and_then(|k| k.cwd.as_deref())
        .or_else(|| agent.and_then(|a| a.cwd.as_deref()))
        .or_else(|| procs.iter().find_map(|p| p.cwd.as_deref()));
    let project = agent
        .or_else(|| procs.first().copied())
        .and_then(|p| p.project.as_ref())
        .map(|r| r.root.as_str())
        .or(cwd);
    let context = project.map(|p| short_dir(home, p));
    let title = s.key.agent.title();
    let now_ms = snap.taken_at_ms;
    let ended_ms = s.session.as_ref().and_then(|k| k.ended_at_ms);
    let state = if leftovers > 0 || s.ended() {
        Some(State::Ended)
    } else if matches!(s.agent, AgentState::Live { .. }) {
        Some(State::Live)
    } else {
        None
    };
    let left_ram: u64 = s
        .members
        .iter()
        .filter(|m| lineage.is_leftover(m.proc.pid))
        .filter_map(|m| snap.process(m.proc.pid)?.footprint)
        .sum();
    let left_ports: Vec<String> = s
        .members
        .iter()
        .filter(|m| lineage.is_leftover(m.proc.pid))
        .flat_map(|m| ports.get(&m.proc.pid).cloned().unwrap_or_default())
        .map(|p| format!(":{p}"))
        .collect();
    let in_dir = cwd
        .map(|c| format!(" in {}", shown(home, c)))
        .unwrap_or_default();
    let ago = |ms: i64| human_age(((now_ms - ms).max(0) / 1000) as u64);
    let sentence = match (leftovers, s.agent) {
        (n @ 1.., _) => {
            let when = ended_ms
                .map(|e| format!(" {} ago", ago(e)))
                .unwrap_or_default();
            let holds = if left_ports.is_empty() {
                human_bytes(left_ram)
            } else {
                let shown = match left_ports.len() {
                    n @ 4.. => format!("{} and {} more ports", left_ports[..3].join(", "), n - 3),
                    _ => left_ports.join(", "),
                };
                format!("{} and {shown}", human_bytes(left_ram))
            };
            format!(
                "This {title} session{in_dir} ended{when} and left {} running. {} {holds}.",
                plural(n, "process", "processes"),
                if n == 1 {
                    "It holds"
                } else {
                    "Together they hold"
                }
            )
        }
        (0, AgentState::Live { pid, .. }) if !s.ended() => format!(
            "This {title} session runs{in_dir} as agent process {pid}, with {}.",
            plural(s.members.len(), "process", "processes")
        ),
        (0, AgentState::Live { .. }) => format!(
            "This {title} session{in_dir} ended, and its agent process went on to another session. Its processes stay with that agent."
        ),
        (0, AgentState::Gone { pid, .. }) => format!(
            "This {title} session{in_dir} ended: agent process {pid} is gone. None of its processes is a leftover yet; each row says why."
        ),
        (0, AgentState::Unknown) => format!(
            "Scry has not seen the agent process of this {title} session{in_dir}, so it cannot tell whether the session ended.{}",
            if s.key.agent == Agent::ClaudeCode {
                " `scry hook install claude` records it."
            } else {
                ""
            }
        ),
    };
    let mut evidence = Vec::new();
    if let Some(k) = &s.session
        && k.source == Source::Hook
    {
        if let Some(t) = k.started_at_ms {
            evidence.push(format!("SessionStart hook {} ago (exact)", ago(t)));
        }
        if let Some(t) = ended_ms {
            let why = k
                .end_reason
                .as_deref()
                .map(|r| format!(", reason {r}"))
                .unwrap_or_default();
            evidence.push(format!("Session ended {} ago{why}", ago(t)));
        }
    }
    match s.agent {
        AgentState::Live { pid, .. } => evidence.push(format!("Agent process {pid} is running")),
        AgentState::Gone { pid, .. } => evidence.push(format!("Agent process {pid} is gone")),
        AgentState::Unknown => evidence.push("No agent process seen".into()),
    }
    let mut facts = vec![fact("session", s.key.id.short())];
    if let Some(e) = ended_ms {
        facts.push(fact("ended", format!("{} ago", ago(e))));
    }
    if let Some(c) = cwd {
        facts.push(fact("cwd", shown(home, c)));
    }
    AgentRow {
        id: session_id(&s.key),
        depth: 0,
        label: s.key.agent.short().into(),
        context: context.clone(),
        pid: agent.map(|a| a.id.pid),
        ram: (ram > 0).then_some(ram),
        ports: held,
        age_s: age(snap, started_us(snap, s)),
        evidence: None,
        state,
        detail: Detail {
            title: match &context {
                Some(c) => format!("{} · {c}", s.key.agent.short()),
                None => s.key.agent.short().into(),
            },
            facts,
            sentence,
            evidence,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn member_row(
    snap: &Snapshot,
    lineage: &Lineage,
    s: &SessionView,
    m: &Member,
    p: &Process,
    depth: u8,
    ports: &HashMap<i32, Vec<u16>>,
    home: Option<&Home>,
) -> AgentRow {
    let command = label(snap, p);
    let verdict = lineage.verdict(p.id.pid);
    let state = match verdict {
        Some(Verdict::Leftover { .. }) => Some(State::Leftover),
        Some(Verdict::Spared {
            why: Spare::Kept { .. },
        }) => Some(State::Kept),
        Some(Verdict::Spared {
            why: Spare::OnlyGuess,
        }) => Some(State::Unowned),
        _ => None,
    };
    let title = s.key.agent.title();
    let in_dir = p
        .cwd
        .as_deref()
        .map(|c| format!(" in {}", shown(home, c)))
        .unwrap_or_default();
    let sentence = match verdict {
        Some(Verdict::Leftover { agent_pid, .. }) => format!(
            "{command} is still running{in_dir}. The {title} session that started it ended: agent process {agent_pid} is gone."
        ),
        Some(Verdict::Spared { why }) => format!(
            "{command} runs{in_dir} under this {title} session. It is not a leftover: {}",
            why.sentence()
        ),
        None => format!("{command} runs{in_dir} under this {title} session."),
    };
    let mut facts = vec![fact("pid", p.id.pid.to_string())];
    if let Some(t) = &p.tty {
        facts.push(fact("tty", t.clone()));
    }
    if let Some(c) = &p.cwd {
        facts.push(fact("cwd", shown(home, c)));
    }
    AgentRow {
        id: format!("p-{}", p.id.pid),
        depth,
        label: command.clone(),
        context: None,
        pid: Some(p.id.pid),
        ram: p.footprint,
        ports: ports.get(&p.id.pid).cloned().unwrap_or_default(),
        age_s: age(snap, p.id.start_us),
        evidence: Some(m.how.evidence()),
        state,
        detail: Detail {
            title: command,
            facts,
            sentence,
            evidence: vec![m.how.sentence()],
        },
    }
}

/// The Agents row id of a session, which the Overview jumps to.
pub fn session_id(key: &SessionKey) -> String {
    format!("s-{}-{}", key.agent.short(), key.id.encode())
}

fn started_us(snap: &Snapshot, s: &SessionView) -> i64 {
    if let Some(t) = s.session.as_ref().and_then(|k| k.started_at_ms) {
        return t * 1000;
    }
    if let AgentState::Live { start_us, .. } = s.agent {
        return start_us;
    }
    s.members
        .iter()
        .map(|m| m.proc.start_us)
        .min()
        .unwrap_or(snap.taken_at_ms * 1000)
}

fn age(snap: &Snapshot, start_us: i64) -> u64 {
    ((snap.taken_at_ms * 1000 - start_us).max(0) / 1_000_000) as u64
}

/// Listening ports by pid.
pub fn listening(snap: &Snapshot) -> HashMap<i32, Vec<u16>> {
    let mut m: HashMap<i32, Vec<u16>> = HashMap::new();
    for s in snap.sockets.iter().filter(|s| s.is_listening()) {
        let v = m.entry(s.pid).or_default();
        if !v.contains(&s.local_port) {
            v.push(s.local_port);
        }
    }
    for v in m.values_mut() {
        v.sort_unstable();
    }
    m
}

fn label(snap: &Snapshot, p: &Process) -> String {
    snap.worker_parent(p).unwrap_or(p).label()
}

fn shown(home: Option<&Home>, path: &str) -> String {
    home.map_or_else(|| path.to_string(), |h| h.redact(path))
}

/// The last directory of a path, which names a project well enough beside its agent.
fn short_dir(home: Option<&Home>, path: &str) -> String {
    let p = shown(home, path);
    match p.rsplit_once('/') {
        Some((_, last)) if !last.is_empty() => last.to_string(),
        _ => p,
    }
}

fn fact(label: &str, value: String) -> Fact {
    Fact {
        label: label.into(),
        value,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, depth: u8, state: Option<State>, evidence: Option<Evidence>) -> AgentRow {
        AgentRow {
            id: id.into(),
            depth,
            label: id.into(),
            context: (depth == 0).then(|| "proj".into()),
            pid: Some(id.len() as i32 * 100 + depth as i32),
            ram: Some(1),
            ports: vec![],
            age_s: 60,
            evidence,
            state,
            detail: Detail::default(),
        }
    }

    #[test]
    fn reap_takes_exact_leftovers_children_first_and_never_a_guess() {
        let rows = vec![
            row("claude", 0, Some(State::Ended), None),
            row("npm", 1, Some(State::Leftover), Some(Evidence::Stamp)),
            row("next", 2, Some(State::Leftover), Some(Evidence::Tree)),
            row("chrome", 1, Some(State::Leftover), Some(Evidence::Guess)),
            row("mcp", 1, Some(State::Live), Some(Evidence::Stamp)),
        ];
        let picked: Vec<_> = reap_targets(&rows).into_iter().map(|t| t.command).collect();
        assert_eq!(picked, ["next", "npm"]);
        assert_eq!(
            reap_targets(&rows)[0].session.as_deref(),
            Some("claude proj")
        );
    }

    #[test]
    fn stop_target_names_the_owning_session() {
        let rows = vec![
            row("claude", 0, Some(State::Ended), None),
            row("next", 1, Some(State::Leftover), Some(Evidence::Stamp)),
        ];
        let t = stop_target(&rows, 1).unwrap();
        assert_eq!(t.session.as_deref(), Some("claude proj"));
        assert_eq!(stop_target(&rows, 0).unwrap().session, None);
    }

    mod live {
        use crate::attribution::{Known, Lineage, Policy};
        use crate::catalog::Role;
        use crate::model::{
            Agent, IpFamily, ProcId, Process, Proto, SNAPSHOT_SCHEMA, Snapshot, Socket, Stamps,
            TcpState,
        };
        use crate::view::{Board, State, Tab};

        const NOW_MS: i64 = 100_000_000;

        fn p(pid: i32, ppid: i32, name: &str, age_s: i64) -> Process {
            Process {
                id: ProcId {
                    pid,
                    start_us: (NOW_MS - age_s * 1000) * 1000,
                },
                ppid,
                uid: 501,
                name: name.into(),
                exe: Some(format!("/usr/local/bin/{name}")),
                footprint: Some(10 << 20),
                command: None,
                cwd: Some("/Users/dev/sources/acme".into()),
                project: None,
                stamps: Stamps::default(),
                worker: false,
                tty: None,
                role: None,
            }
        }

        fn snap() -> Snapshot {
            let mut agent = p(900, 1, "claude", 7200);
            agent.role = Some(Role::Agent(Agent::ClaudeCode));
            let mut left = p(1000, 1, "next", 3600);
            left.stamps.claude_code_session_id = Some("4f2c9a1e-dead".into());
            left.stamps.claude_pid = Some(800);
            let mut parent = p(999, 1, "npm", 3600);
            parent.stamps = left.stamps.clone();
            left.ppid = 999;
            Snapshot {
                schema: SNAPSHOT_SCHEMA,
                taken_at_ms: NOW_MS,
                viewer_uid: 501,
                processes: vec![agent, p(901, 900, "node", 600), parent, left],
                sockets: vec![Socket {
                    proto: Proto::Tcp,
                    family: IpFamily::V4,
                    local_ip: "127.0.0.1".parse().unwrap(),
                    local_port: 3000,
                    remote_ip: "0.0.0.0".parse().unwrap(),
                    remote_port: 0,
                    state: Some(TcpState::Listen),
                    pid: 1000,
                    epid: 1000,
                    uid: 501,
                }],
                ..Snapshot::default()
            }
        }

        #[test]
        fn the_board_puts_leftovers_first_in_agents_and_overview() {
            let s = snap();
            let policy = Policy {
                scry: None,
                ..Policy::default()
            };
            let lineage = Lineage::build(&s, &Known::default(), &policy);
            let board = Board::build(&s, &lineage, None);
            let rows = board.agents.ready().unwrap();
            let lines: Vec<(u8, &str, Option<State>)> = rows
                .iter()
                .map(|r| (r.depth, r.label.as_str(), r.state))
                .collect();
            assert_eq!(
                lines,
                [
                    (0, "claude", Some(State::Ended)),
                    (1, "npm", Some(State::Leftover)),
                    (2, "next", Some(State::Leftover)),
                    (0, "claude", Some(State::Live)),
                    (1, "node", None),
                ]
            );
            assert_eq!(rows[0].ports, [3000]);
            assert!(
                rows[0].detail.sentence.contains("left 2 processes running"),
                "{}",
                rows[0].detail.sentence
            );
            assert!(
                rows[4]
                    .detail
                    .sentence
                    .contains("agent process 900 is still running")
            );

            let o = &board.overview;
            let tabs: Vec<Tab> = o.attention.iter().map(|a| a.tab).collect();
            assert_eq!(tabs, [Tab::Agents, Tab::Ports]);
            assert_eq!(o.attention[0].title, "claude left 2 processes running");
            assert_eq!(o.attention[0].row.as_deref(), Some(rows[0].id.as_str()));
            assert_eq!(o.attention[1].title, ":3000 is held by an ended session");
            assert_eq!(o.attention[1].row.as_deref(), Some("tcp-3000-1000"));
            assert_eq!(o.checked.len(), 3);
            let ports = board.ports.ready().unwrap();
            assert_eq!(ports[0].state, Some(State::Leftover));

            let left = super::super::leftovers(&s, &lineage, None);
            let pids: Vec<i32> = left.iter().map(|l| l.pid).collect();
            assert_eq!(pids, [999, 1000]);
            assert_eq!(left[1].agent_pid, 800);
            assert_eq!(left[1].ports, [3000]);
        }
    }
}
