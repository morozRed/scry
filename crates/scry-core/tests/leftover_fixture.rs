//! PLAN.md M2's exit test: a fixture agent spawns a child that ignores SIGHUP, then exits.
//! The child must come out as a leftover with `stamp` evidence, from a live scan.
//!
//! The fixture agent is this test binary, copied to where Claude Code's native installer puts
//! its binary so the catalog recognises it (macOS kills copies of system binaries such as
//! bash). Run with `SCRY_FIXTURE_AGENT` naming the original binary, its `fixture_agent` test acts as the agent: like
//! Claude Code, it hands its child `CLAUDE_CODE_SESSION_ID` and `CLAUDE_PID`; its
//! `fixture_child` test is that child. It runs in a
//! session of its own, without a terminal, as an agent driven by an app would.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use scry_core::attribution::{
    AgentState, DEFAULT_GRACE, How, Known, Lineage, Policy, Spare, Verdict,
};
use scry_core::catalog::Role;
use scry_core::engine::{Engine, Labels};
use scry_core::model::Agent;

const SESSION: &str = "0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0";

fn policy(grace: Duration) -> Policy {
    Policy {
        grace,
        keep: Vec::new(),
        scry: None,
        config_error: None,
    }
}

fn scan() -> (scry_core::Snapshot, Lineage) {
    let snap = Engine::labelling(Labels::None).snapshot().unwrap();
    let lineage = Lineage::build(&snap, &Known::default(), &policy(Duration::ZERO));
    (snap, lineage)
}

struct Reap(i32);

impl Drop for Reap {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

#[test]
fn a_child_that_ignores_sighup_outlives_its_agent_as_a_stamped_leftover() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".local/share/claude/versions");
    std::fs::create_dir_all(&dir).unwrap();
    let agent_exe = dir.join("9.9.9");
    std::fs::copy(std::env::current_exe().unwrap(), &agent_exe).unwrap();

    let mut agent = Command::new(&agent_exe);
    agent
        .args(["fixture_agent", "--exact", "--nocapture", "--quiet"])
        .env("SCRY_FIXTURE_AGENT", std::env::current_exe().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // A session of its own and no controlling terminal.
    unsafe {
        agent.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    };
    let mut agent = agent.spawn().unwrap();
    let agent_pid = agent.id() as i32;
    let mut out = BufReader::new(agent.stdout.take().unwrap());
    let child: i32 = loop {
        let mut line = String::new();
        assert!(
            out.read_line(&mut line).unwrap() > 0,
            "the agent printed no child"
        );
        if let Some(pid) = line.trim().strip_prefix("child ") {
            break pid.parse().unwrap();
        }
    };
    let _reap = Reap(child);

    // While the agent runs, it is a Claude Code session and its child is not a leftover.
    let (snap, l) = scan();
    let a = snap.process(agent_pid).unwrap();
    assert_eq!(a.role, Some(Role::Agent(Agent::ClaudeCode)));
    assert_eq!(a.tty, None);
    assert_eq!(
        l.verdict(child),
        Some(&Verdict::Spared {
            why: Spare::AgentLive { pid: agent_pid }
        })
    );

    // The child ignores SIGHUP, as a dev server left in a closed terminal would.
    unsafe { libc::kill(child, libc::SIGHUP) };
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(unsafe { libc::kill(child, 0) }, 0, "survived SIGHUP");

    writeln!(agent.stdin.take().unwrap(), "go").unwrap();
    agent.wait().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let (snap, l) = loop {
        let (snap, l) = scan();
        if snap.process(child).is_some_and(|p| p.ppid == 1) {
            break (snap, l);
        }
        assert!(Instant::now() < deadline, "child never re-parented");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(snap.process(agent_pid).is_none());
    match l.verdict(child) {
        Some(Verdict::Leftover {
            session,
            how,
            agent_pid: gone,
        }) => {
            assert_eq!(session.id.short(), &SESSION[..8]);
            assert_eq!(
                *how,
                How::Stamp {
                    key: "CLAUDE_CODE_SESSION_ID"
                }
            );
            assert_eq!(*gone, agent_pid);
        }
        other => panic!("not a leftover: {other:?}"),
    }
    let (session, _) = l.session_of(child).unwrap();
    assert!(matches!(session.agent, AgentState::Gone { pid, .. } if pid == agent_pid));

    // The default grace period spares it until it is five minutes old.
    let spared = Lineage::build(&snap, &Known::default(), &policy(DEFAULT_GRACE));
    assert!(matches!(
        spared.verdict(child),
        Some(Verdict::Spared {
            why: Spare::Young { .. }
        })
    ));
}

/// The fixture agent's body, run only inside the copied binary.
#[test]
fn fixture_agent() {
    if std::env::var_os("SCRY_FIXTURE_AGENT").is_none() {
        return;
    }
    // The child is the test binary from where cargo built it, not the agent's copy: macOS
    // hides the environment of its own binaries, such as /bin/sleep, so their stamps cannot
    // be read.
    let child = Command::new(std::env::var_os("SCRY_FIXTURE_AGENT").unwrap())
        .args(["fixture_child", "--exact", "--nocapture", "--quiet"])
        .env_remove("SCRY_FIXTURE_AGENT")
        .env("SCRY_FIXTURE_CHILD", "1")
        .env("CLAUDE_CODE_SESSION_ID", SESSION)
        .env("CLAUDE_PID", std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    println!("child {}", child.id());
    std::io::stdout().flush().unwrap();
    let mut go = String::new();
    std::io::stdin().read_line(&mut go).unwrap();
    // Exits without waiting for the child, which launchd adopts.
    std::process::exit(0);
}

/// The fixture agent's child: ignores SIGHUP and waits to be killed.
#[test]
fn fixture_child() {
    if std::env::var_os("SCRY_FIXTURE_CHILD").is_none() {
        return;
    }
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    std::thread::sleep(Duration::from_secs(600));
}
