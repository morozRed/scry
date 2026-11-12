use super::*;
use crate::catalog::Infra;
use crate::model::{SNAPSHOT_SCHEMA, Stamps};

const NOW_MS: i64 = 10_000_000;
const HOUR_US: i64 = 3_600_000_000;

fn at(ago_us: i64) -> i64 {
    NOW_MS * 1000 - ago_us
}

fn proc(pid: i32, ppid: i32, name: &str) -> Process {
    Process {
        id: ProcId {
            pid,
            start_us: at(HOUR_US) + pid as i64,
        },
        ppid,
        uid: 501,
        name: name.into(),
        exe: Some(format!("/usr/local/bin/{name}")),
        footprint: Some(1 << 20),
        command: None,
        cwd: Some("/Users/dev/sources/acme".into()),
        project: None,
        stamps: Stamps::default(),
        worker: false,
        tty: None,
        role: None,
    }
}

fn claude(pid: i32) -> Process {
    let mut p = proc(pid, 1, "claude");
    p.role = Some(Role::Agent(Agent::ClaudeCode));
    p.id.start_us -= HOUR_US;
    p
}

fn stamped(mut p: Process, session: &str, claude_pid: Option<i32>) -> Process {
    p.stamps.claude_code_session_id = Some(session.into());
    p.stamps.claude_pid = claude_pid;
    p
}

fn snap(processes: Vec<Process>) -> Snapshot {
    Snapshot {
        schema: SNAPSHOT_SCHEMA,
        taken_at_ms: NOW_MS,
        viewer_uid: 501,
        processes,
        ..Snapshot::default()
    }
}

fn policy() -> Policy {
    Policy {
        grace: Duration::from_secs(300),
        keep: vec!["postgres".into()],
        scry: None,
    }
}

fn build(s: &Snapshot) -> Lineage {
    Lineage::build(s, &Known::default(), &policy())
}

fn spare(l: &Lineage, pid: i32) -> Spare {
    match l.verdict(pid) {
        Some(Verdict::Spared { why }) => why.clone(),
        other => panic!("{pid}: {other:?}"),
    }
}

fn key(id: &str) -> SessionKey {
    SessionKey {
        agent: Agent::ClaudeCode,
        id: SessionId::Known(id.into()),
    }
}

#[test]
fn a_stamped_child_of_an_exited_agent_is_a_leftover() {
    // CLAUDE_PID names pid 900, which is gone.
    let s = snap(vec![stamped(proc(1000, 1, "node"), "s1", Some(900))]);
    let l = build(&s);
    assert_eq!(
        l.verdict(1000),
        Some(&Verdict::Leftover {
            session: key("s1"),
            how: How::Stamp {
                key: "CLAUDE_CODE_SESSION_ID"
            },
            agent_pid: 900,
        })
    );
    let session = &l.sessions[0];
    assert!(session.ended());
    assert_eq!(
        session.agent,
        AgentState::Gone {
            pid: 900,
            start_us: None
        }
    );
}

#[test]
fn a_reused_agent_pid_does_not_keep_the_session_alive() {
    // Pid 900 runs again, but started after the child, so it is not the child's agent.
    let mut reused = claude(900);
    reused.id.start_us = at(60_000_000);
    let s = snap(vec![
        reused,
        stamped(proc(1000, 1, "node"), "s1", Some(900)),
    ]);
    assert!(build(&s).is_leftover(1000));
}

#[test]
fn a_live_agent_spares_its_children_whether_by_stamp_or_tree() {
    let s = snap(vec![
        claude(900),
        stamped(proc(1000, 1, "node"), "s1", Some(900)),
        proc(1001, 900, "npm"),
        proc(1002, 1001, "next"),
    ]);
    let l = build(&s);
    for pid in [1000, 1001, 1002] {
        assert_eq!(spare(&l, pid), Spare::AgentLive { pid: 900 }, "{pid}");
    }
    let (session, m) = l.session_of(1002).unwrap();
    assert_eq!(m.how, How::Tree { agent: 900 });
    assert_eq!(
        session.key,
        key("s1"),
        "the agent's session, named by its stamped child"
    );
    assert!(
        l.verdict(900).is_none(),
        "an agent is a session, not a member"
    );
}

#[test]
fn unattributed_and_guessed_processes_are_never_leftovers() {
    let mut known = Known::default();
    known.sessions.push(Session {
        key: key("s1"),
        cwd: Some("/Users/dev/sources/acme".into()),
        started_at_ms: Some(NOW_MS - 2 * 3_600_000),
        ended_at_ms: Some(NOW_MS - 1_000),
        end_reason: None,
        source: Source::Hook,
    });
    let s = snap(vec![proc(1000, 1, "python3")]);
    let l = Lineage::build(&s, &known, &policy());
    assert_eq!(spare(&l, 1000), Spare::OnlyGuess);
    assert_eq!(l.session_of(1000).unwrap().1.how, How::Guess);
    let l = build(&s);
    assert_eq!(spare(&l, 1000), Spare::Unattributed);
}

#[test]
fn a_session_without_a_known_agent_is_not_judged_ended() {
    // An MCP server carries the session id but no CLAUDE_PID, and its parent is gone.
    let s = snap(vec![stamped(proc(1000, 1, "node"), "s1", None)]);
    let l = build(&s);
    assert_eq!(spare(&l, 1000), Spare::AgentUnknown);
    assert!(!l.sessions[0].ended());
}

#[test]
fn the_never_reap_list_ttys_grace_and_keep_list_each_spare() {
    let dead = |p| stamped(p, "s1", Some(900));
    let mut tmux = dead(proc(1000, 1, "tmux"));
    tmux.role = Some(Role::NeverReap(Infra::Multiplexer));
    let mut on_tty = dead(proc(1001, 1, "vim"));
    on_tty.tty = Some("ttys004".into());
    let parent = dead(proc(1002, 1, "zsh"));
    let mut child = dead(proc(1003, 1002, "less"));
    child.tty = Some("ttys005".into());
    let mut young = dead(proc(1004, 1, "node"));
    young.id.start_us = at(60_000_000);
    let kept = dead(proc(1005, 1, "postgres"));
    let s = snap(vec![tmux, on_tty, parent, child, young, kept]);
    let l = build(&s);
    assert_eq!(
        spare(&l, 1000),
        Spare::NeverReap {
            what: Infra::Multiplexer
        }
    );
    assert_eq!(
        spare(&l, 1001),
        Spare::Tty {
            tty: "ttys004".into()
        }
    );
    assert_eq!(
        spare(&l, 1002),
        Spare::TtyDescendant {
            pid: 1003,
            tty: "ttys005".into()
        }
    );
    assert_eq!(
        spare(&l, 1004),
        Spare::Young {
            age_s: 60,
            grace_s: 300
        }
    );
    assert_eq!(
        spare(&l, 1005),
        Spare::Kept {
            entry: "postgres".into()
        }
    );
}

#[test]
fn clear_starts_a_new_session_and_the_old_one_is_not_reaped() {
    // One agent process hosted s1, then /clear started s2. A child of s1 stays.
    let agent = claude(900);
    let ev = |id: &str, sid: &str, at_ms, kind| SessionEvent {
        id: id.into(),
        at_ms,
        agent: Agent::ClaudeCode,
        session_id: sid.into(),
        kind,
        cwd: Some("/Users/dev/sources/acme".into()),
        agent_proc: Some(agent.id),
    };
    let mut known = Known::default();
    known.apply(&[
        ev(
            "e1",
            "s1",
            NOW_MS - 3_000_000,
            EventKind::Start { source: None },
        ),
        ev(
            "e2",
            "s1",
            NOW_MS - 2_000_000,
            EventKind::End {
                reason: Some("clear".into()),
            },
        ),
        ev(
            "e3",
            "s2",
            NOW_MS - 2_000_000,
            EventKind::Start {
                source: Some("clear".into()),
            },
        ),
    ]);
    let s = snap(vec![agent, stamped(proc(1000, 1, "node"), "s1", None)]);
    let l = Lineage::build(&s, &known, &policy());
    assert_eq!(spare(&l, 1000), Spare::AgentLive { pid: 900 });
    let names: Vec<_> = l.sessions.iter().map(|s| s.key.id.short()).collect();
    assert_eq!(names, ["s1", "s2"]);
    let s1 = &l.sessions[0];
    assert!(s1.ended(), "ended by SessionEnd, though its agent lives");
    assert_eq!(
        s1.session.as_ref().unwrap().end_reason.as_deref(),
        Some("clear")
    );
}

#[test]
fn recorded_ancestry_keeps_a_child_attributed_after_its_agent_dies() {
    let agent = claude(900);
    let live = snap(vec![
        agent.clone(),
        proc(1001, 900, "npm"),
        proc(1002, 1001, "next"),
    ]);
    let first = build(&live);
    let (sessions, procs) = first.records(&live, NOW_MS);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].source, Source::Scan);
    assert_eq!(procs.len(), 3, "the agent and two children");

    // The agent exits; npm and next are re-parented to launchd.
    let mut npm = proc(1001, 1, "npm");
    npm.id = live.processes[1].id;
    let mut next = proc(1002, 1001, "next");
    next.id = live.processes[2].id;
    let later = snap(vec![npm, next]);
    let known = Known {
        sessions,
        processes: procs,
    };
    let l = Lineage::build(&later, &known, &policy());
    for pid in [1001, 1002] {
        match l.verdict(pid) {
            Some(Verdict::Leftover { how, agent_pid, .. }) => {
                assert!(matches!(
                    how,
                    How::Recorded {
                        evidence: Evidence::Tree,
                        ..
                    }
                ));
                assert_eq!(*agent_pid, 900);
            }
            other => panic!("{pid}: {other:?}"),
        }
    }
    let (sessions, _) = l.records(&later, NOW_MS + 5);
    assert_eq!(sessions[0].end_reason.as_deref(), Some(AGENT_EXITED));
}

#[test]
fn scrys_own_tree_is_spared() {
    let s = snap(vec![
        stamped(proc(1000, 1, "scry"), "s1", Some(900)),
        stamped(proc(1001, 1000, "sh"), "s1", Some(900)),
    ]);
    let l = Lineage::build(
        &s,
        &Known::default(),
        &Policy {
            scry: Some(1000),
            ..policy()
        },
    );
    assert_eq!(spare(&l, 1000), Spare::Scry);
    assert_eq!(spare(&l, 1001), Spare::Scry);
}

#[test]
fn other_users_processes_get_no_verdict() {
    let mut p = stamped(proc(1000, 1, "node"), "s1", Some(900));
    p.uid = 0;
    assert!(build(&snap(vec![p])).verdict(1000).is_none());
}
