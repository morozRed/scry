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
        config_error: None,
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
    // Another session's tmux, so it carries no stamp to the others.
    let mut tmux = stamped(proc(1000, 1, "tmux"), "s0", Some(900));
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

/// The role the catalog gives an executable run with these arguments.
fn classified(mut p: Process, exe: &str, argv: &[&str]) -> Process {
    let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
    p.exe = Some(exe.into());
    p.role = crate::catalog::classify(exe, &argv).map(|r| r.role);
    p
}

fn codex(pid: i32) -> Process {
    let mut p = proc(pid, 1, "codex");
    p.role = Some(Role::Agent(Agent::Codex));
    p.id.start_us -= HOUR_US;
    p
}

fn ended(id: &str, at_ms: i64) -> Session {
    Session {
        key: key(id),
        cwd: None,
        started_at_ms: Some(0),
        ended_at_ms: Some(at_ms),
        end_reason: Some("prompt_input_exit".into()),
        source: Source::Hook,
    }
}

fn recorded(session: SessionKey, proc: ProcId, part: Part, first_seen_ms: i64) -> SessionProcess {
    SessionProcess {
        session,
        proc,
        part,
        first_seen_ms,
        last_seen_ms: first_seen_ms,
        evidence: Evidence::Tree,
    }
}

#[test]
fn key_agents_and_ssh_masters_a_session_started_are_never_leftovers() {
    // A signed commit starts the gpg trio with the session's stamp; every later session
    // uses them.
    let dead = |p| stamped(p, "s1", Some(900));
    let gpg = "/opt/homebrew/Cellar/gnupg/2.4.8";
    let s = snap(vec![
        classified(
            dead(proc(1000, 1, "keyboxd")),
            &format!("{gpg}/libexec/keyboxd"),
            &["keyboxd", "--homedir", ".gnupg", "--daemon"],
        ),
        classified(
            dead(proc(1001, 1, "gpg-agent")),
            &format!("{gpg}/bin/gpg-agent"),
            &["gpg-agent", "--homedir", ".gnupg", "--daemon"],
        ),
        classified(
            dead(proc(1002, 1001, "scdaemon")),
            &format!("{gpg}/libexec/scdaemon"),
            &["scdaemon", "--multi-server"],
        ),
        classified(
            dead(proc(1003, 1, "ssh-agent")),
            "/usr/bin/ssh-agent",
            &["ssh-agent", "-s"],
        ),
        classified(
            dead(proc(1004, 1, "ssh")),
            "/usr/bin/ssh",
            &["ssh: /Users/dev/.ssh/cm-build [mux]"],
        ),
        classified(
            dead(proc(1005, 1, "ssh")),
            "/usr/bin/ssh",
            &["ssh", "-p", "2222", "build.example.com", "sleep", "1000"],
        ),
    ]);
    let l = build(&s);
    for pid in 1000..=1004 {
        assert!(
            matches!(spare(&l, pid), Spare::NeverReap { .. }),
            "{pid}: {:?}",
            l.verdict(pid)
        );
    }
    assert!(l.is_leftover(1005), "a plain ssh is still judged");
}

#[test]
fn a_job_under_a_live_never_reap_process_is_spared_whatever_its_stamp() {
    // tmux was first started inside a session that has ended; `tmux run-shell -b` starts a
    // job later, which inherits the server's environment and has no terminal.
    let dead = |p| stamped(p, "s1", Some(900));
    let tmux = classified(
        dead(proc(1000, 1, "tmux")),
        "/opt/homebrew/bin/tmux",
        &["tmux", "new-session", "-d"],
    );
    let job = dead(proc(1001, 1000, "sh"));
    let grandchild = dead(proc(1002, 1001, "node"));
    // A language server under a VS Code helper that an agent opened.
    let helper = classified(
        dead(proc(1010, 1, "Code Helper (Plugin)")),
        "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)",
        &["Code Helper (Plugin)"],
    );
    let server = dead(proc(1011, 1010, "rust-analyzer"));
    // A daemon started from a tmux pane detaches to launchd with the server's stamp.
    let daemon = dead(proc(1003, 1, "redis-server"));
    // Started before tmux, so its stamp cannot have come through tmux.
    let before = dead(proc(999, 1, "node"));
    let s = snap(vec![tmux, job, grandchild, helper, server, daemon, before]);
    let l = build(&s);
    for (pid, above) in [(1001, 1000), (1002, 1000), (1011, 1010)] {
        assert!(
            matches!(spare(&l, pid), Spare::UnderNeverReap { pid: p, .. } if p == above),
            "{pid}: {:?}",
            l.verdict(pid)
        );
    }
    assert_eq!(
        spare(&l, 1003),
        Spare::StampCarried {
            pid: 1000,
            what: Infra::Multiplexer
        }
    );
    assert!(l.is_leftover(999));
}

#[test]
fn a_stamp_from_a_session_that_ended_before_the_process_started_is_ignored() {
    // s1 ended two hours ago; the process started an hour ago with s1's stamp, which it
    // inherited through something that outlived s1.
    let mut known = Known::default();
    known.sessions.push(ended("s1", NOW_MS - 7_200_000));
    let s = snap(vec![stamped(proc(1000, 1, "node"), "s1", Some(900))]);
    let l = Lineage::build(&s, &known, &policy());
    assert_eq!(spare(&l, 1000), Spare::Unattributed);

    // Ended after it started: still s1's, and a leftover.
    known.sessions[0].ended_at_ms = Some(NOW_MS - 60_000);
    let l = Lineage::build(&s, &known, &policy());
    assert!(l.is_leftover(1000));
}

#[test]
fn an_agent_the_catalog_does_not_recognise_is_not_gone() {
    // CLAUDE_PID names a live process the catalog does not know: a renamed binary, or one
    // whose executable could not be read. Its children are not leftovers.
    let mut wrapper = proc(900, 1, "claude-wrapper");
    wrapper.id.start_us -= HOUR_US;
    let s = snap(vec![
        wrapper.clone(),
        stamped(proc(1000, 900, "node"), "s1", Some(900)),
        stamped(proc(1001, 1, "node"), "s1", Some(900)),
    ]);
    let l = build(&s);
    for pid in [1000, 1001] {
        assert_eq!(
            spare(&l, pid),
            Spare::AgentUnrecognised { pid: 900 },
            "{pid}"
        );
    }
    assert!(!l.sessions[0].ended());

    // A recorded agent still running unrecognised keeps its recorded child.
    let child = proc(1002, 1, "node");
    let known = Known {
        sessions: Vec::new(),
        processes: vec![
            recorded(key("s2"), wrapper.id, Part::Agent, NOW_MS - 3_000_000),
            recorded(key("s2"), child.id, Part::Child, NOW_MS - 3_000_000),
        ],
    };
    let l = Lineage::build(&snap(vec![wrapper, child]), &known, &policy());
    assert_eq!(spare(&l, 1002), Spare::AgentUnrecognised { pid: 900 });
}

#[test]
fn a_process_recorded_under_several_sessions_needs_every_agent_gone() {
    // Agent 900 started the worker in s1, then /clear moved it to s2, so the worker is
    // recorded under both. 900 exited; agent 950 resumed s1.
    let gone = ProcId {
        pid: 900,
        start_us: at(3 * HOUR_US),
    };
    let resumed = claude(950);
    let worker = proc(1000, 1, "node");
    let rows = |first: &str, second: &str| {
        vec![
            recorded(key("s1"), gone, Part::Agent, NOW_MS - 5_000_000),
            recorded(key("s2"), gone, Part::Agent, NOW_MS - 4_000_000),
            recorded(key("s1"), resumed.id, Part::Agent, NOW_MS - 1_000_000),
            recorded(key(first), worker.id, Part::Child, NOW_MS - 4_500_000),
            recorded(key(second), worker.id, Part::Child, NOW_MS - 3_500_000),
        ]
    };
    for (first, second) in [("s1", "s2"), ("s2", "s1")] {
        let known = Known {
            sessions: Vec::new(),
            processes: rows(first, second),
        };
        let l = Lineage::build(
            &snap(vec![resumed.clone(), worker.clone()]),
            &known,
            &policy(),
        );
        assert_eq!(
            spare(&l, 1000),
            Spare::AgentLive { pid: 950 },
            "recorded first under {first}"
        );
        assert_eq!(
            l.session_of(1000).unwrap().0.key,
            key(first),
            "the first record names the owner"
        );
    }
    // With no agent left for either session, it is a leftover.
    let known = Known {
        sessions: Vec::new(),
        processes: rows("s1", "s2")
            .into_iter()
            .filter(|m| m.proc != resumed.id)
            .collect(),
    };
    assert!(Lineage::build(&snap(vec![worker]), &known, &policy()).is_leftover(1000));
}

#[test]
fn a_live_agent_of_another_kind_wins_over_an_inherited_stamp() {
    // Claude Code 900 started Codex 950 and exited. Codex's workers carry Claude's stamps,
    // one with Codex's own as well.
    let mut both = stamped(proc(1000, 950, "python3"), "s1", Some(900));
    both.stamps.scry_session = Some("s1".into());
    both.stamps.codex_session_id = Some("c1".into());
    let only_claude = stamped(proc(1001, 950, "node"), "s1", Some(900));
    let s = snap(vec![codex(950), both, only_claude]);
    let l = build(&s);
    for pid in [1000, 1001] {
        assert_eq!(spare(&l, pid), Spare::AgentLive { pid: 950 }, "{pid}");
    }
    let (session, m) = l.session_of(1000).unwrap();
    assert_eq!(session.key.agent, Agent::Codex);
    assert_eq!(
        m.how,
        How::Stamp {
            key: "CODEX_SESSION_ID"
        }
    );
    assert_eq!(l.session_of(1001).unwrap().1.how, How::Tree { agent: 950 });

    // Re-parented once Codex exits too, the Codex stamp still claims it, and nothing shows
    // that Codex session's agent is gone.
    let mut orphan = stamped(proc(1002, 1, "python3"), "s1", Some(900));
    orphan.stamps.codex_session_id = Some("c1".into());
    assert_eq!(
        spare(&build(&snap(vec![orphan])), 1002),
        Spare::AgentUnknown
    );
}

#[test]
fn an_unreadable_config_stops_leftover_selection() {
    let s = snap(vec![stamped(proc(1000, 1, "node"), "s1", Some(900))]);
    let broken = Policy {
        config_error: Some("config.toml line 3: expected `=`".into()),
        ..policy()
    };
    let l = Lineage::build(&s, &Known::default(), &broken);
    assert!(matches!(spare(&l, 1000), Spare::ConfigUnread { .. }));
    assert!(build(&s).is_leftover(1000));
}

#[test]
fn a_resumed_session_is_open_again() {
    let old = claude(900);
    let new = claude(950);
    let ev = |id: &str, at_ms, kind, agent: &Process| SessionEvent {
        id: id.into(),
        at_ms,
        agent: Agent::ClaudeCode,
        session_id: "s1".into(),
        kind,
        cwd: Some("/Users/dev/sources/acme".into()),
        agent_proc: Some(agent.id),
    };
    let start = |source: Option<&str>| EventKind::Start {
        source: source.map(String::from),
    };
    let end = EventKind::End {
        reason: Some("prompt_input_exit".into()),
    };
    let mut known = Known::default();
    known.apply(&[
        ev("e1", NOW_MS - 7_000_000, start(None), &old),
        ev("e2", NOW_MS - 3_000_000, end.clone(), &old),
        ev("e3", NOW_MS - 1_000_000, start(Some("resume")), &new),
    ]);
    let s1 = &known.sessions[0];
    assert_eq!((s1.ended_at_ms, s1.end_reason.as_deref()), (None, None));
    assert_eq!(s1.started_at_ms, Some(NOW_MS - 7_000_000));
    let l = Lineage::build(&snap(vec![new.clone()]), &known, &policy());
    assert!(!l.sessions[0].ended());
    assert_eq!(
        l.sessions[0].agent,
        AgentState::Live {
            pid: 950,
            start_us: new.id.start_us
        }
    );

    // A start older than the end, read late, leaves the session ended.
    let mut known = Known::default();
    known.apply(&[ev("e2", NOW_MS - 3_000_000, end, &old)]);
    known.apply(&[ev("e1", NOW_MS - 7_000_000, start(None), &old)]);
    assert_eq!(known.sessions[0].ended_at_ms, Some(NOW_MS - 3_000_000));
}
