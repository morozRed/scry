//! Reaping: stopping the processes the leftover rule selects, for `scry reap` and the TUI's
//! `X`.
//!
//! The list comes from the same [`Lineage`] every view reads, children before their parents.
//! Right before each signal the process is read again: it must still be a leftover, and its
//! identity `(pid, start_time)` must still match, so neither a pid reused since the list was
//! shown nor a session that came back is ever signalled. SIGTERM comes first, with up to 3 s
//! to exit; SIGKILL only when asked, after the same checks once more.

use std::cmp::Reverse;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::attribution::{Lineage, Verdict};
use crate::control::{self, Host, Outcome, Refusal, StopTarget};
use crate::home::Home;
use crate::live::Collector;
use crate::model::{Agent, ProcId, Snapshot};
use crate::paths::Paths;
use crate::store::{AuditRow, Store};
use crate::view::Evidence;
use crate::view::agents::{self, Leftover};

/// One process to reap, as the confirmation and the report name it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReapTarget {
    pub id: ProcId,
    pub command: String,
    pub agent: Agent,
    pub session: String,
    pub evidence: Evidence,
    pub ram: Option<u64>,
}

impl From<&Leftover> for ReapTarget {
    fn from(l: &Leftover) -> Self {
        ReapTarget {
            id: ProcId {
                pid: l.pid,
                start_us: l.start_us,
            },
            command: l.command.clone(),
            agent: l.agent,
            session: l.session.clone(),
            evidence: l.evidence,
            ram: l.ram,
        }
    }
}

/// The leftovers to reap, children before their parents, so nothing is re-parented to
/// launchd halfway. With `session`, only that session's: its id, or the start of it.
pub fn plan(
    snap: &Snapshot,
    lineage: &Lineage,
    home: Option<&Home>,
    session: Option<&str>,
) -> Vec<Leftover> {
    let mut list: Vec<Leftover> = agents::leftovers(snap, lineage, home)
        .into_iter()
        .filter(|l| {
            session.is_none_or(|s| {
                !s.is_empty() && (l.session.starts_with(s) || s.starts_with(&l.session))
            })
        })
        .collect();
    children_first(snap, &mut list);
    list
}

/// Deepest first, then newest first among equals.
fn children_first(snap: &Snapshot, list: &mut [Leftover]) {
    let depth = |pid: i32| {
        let mut d = 0;
        let mut at = pid;
        while let Some(p) = snap.process(at).filter(|p| p.ppid > 1 && d < 64) {
            d += 1;
            at = p.ppid;
        }
        d
    };
    list.sort_by_key(|l| (Reverse(depth(l.pid)), Reverse(l.start_us), l.pid));
}

/// Reads whether a process is still a leftover, right before a signal.
pub trait Judge {
    fn still_leftover(&mut self, id: ProcId) -> Result<(), Refusal>;
}

/// The live judge: a fresh reading of the machine for each question.
pub struct LiveJudge<'a>(pub &'a mut Collector);

impl Judge for LiveJudge<'_> {
    fn still_leftover(&mut self, id: ProcId) -> Result<(), Refusal> {
        let r = self.0.read().map_err(|e| Refusal::NotLeftover {
            pid: id.pid,
            why: format!("the processes could not be read again: {e:#}"),
        })?;
        verdict_of(&r.snap, &r.lineage, id)
    }
}

/// Whether `id` is in `snap` and a leftover in `lineage`.
pub fn verdict_of(snap: &Snapshot, lineage: &Lineage, id: ProcId) -> Result<(), Refusal> {
    if snap.process(id.pid).is_none_or(|p| p.id != id) {
        return Err(Refusal::Changed { pid: id.pid });
    }
    match lineage.verdict(id.pid) {
        Some(Verdict::Leftover { .. }) => Ok(()),
        Some(Verdict::Spared { why }) => Err(Refusal::NotLeftover {
            pid: id.pid,
            why: why.sentence(),
        }),
        None => Err(Refusal::NotLeftover {
            pid: id.pid,
            why: "Scry does not judge it.".into(),
        }),
    }
}

/// What happened to one target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Done {
    pub target: ReapTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
}

impl Done {
    fn new(target: ReapTarget, result: Result<Outcome, Refusal>) -> Done {
        let (outcome, refusal) = match result {
            Ok(o) => (Some(o), None),
            Err(r) => (None, Some(r)),
        };
        Done {
            target,
            outcome,
            refusal,
        }
    }

    pub fn stopped(&self) -> bool {
        matches!(self.outcome, Some(Outcome::Exited { .. }))
    }

    pub fn still_running(&self) -> bool {
        matches!(
            self.outcome,
            Some(Outcome::StillRunning { .. } | Outcome::KillRefused { .. })
        )
    }

    /// What the audit row and the text report say happened.
    pub fn result(&self) -> String {
        match (&self.outcome, &self.refusal) {
            (Some(o), _) => outcome_words(o),
            (None, Some(r)) => format!("not signalled: {r}"),
            (None, None) => "not signalled".into(),
        }
    }
}

/// An outcome in words, for audit rows and reports.
pub fn outcome_words(o: &Outcome) -> String {
    match o {
        Outcome::Exited { signal, after_ms } => format!("exited after {signal} in {after_ms} ms"),
        Outcome::StillRunning { after_ms } => {
            format!("still running {after_ms} ms after the last signal")
        }
        Outcome::KillRefused { why, .. } => {
            format!("still running after SIGTERM; SIGKILL not sent: {why}")
        }
    }
}

/// Every target's outcome, in the order they were acted on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub done: Vec<Done>,
}

impl Report {
    pub fn stopped(&self) -> usize {
        self.done.iter().filter(|d| d.stopped()).count()
    }

    pub fn still_running(&self) -> Vec<&Done> {
        self.done.iter().filter(|d| d.still_running()).collect()
    }

    pub fn refused(&self) -> usize {
        self.done.iter().filter(|d| d.refusal.is_some()).count()
    }

    /// `Stopped 12, 1 still running.`
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("stopped {}", self.stopped())];
        let running = self.still_running().len();
        if running > 0 {
            parts.push(format!("{running} still running"));
        }
        let refused = self.refused();
        if refused > 0 {
            parts.push(format!("{refused} not signalled"));
        }
        let mut s = parts.join(", ");
        s[..1].make_ascii_uppercase();
        s + "."
    }

    /// As `scry stop`: 2 when something is still running, 0 when something was stopped, else
    /// 1, since nothing was signalled.
    pub fn exit_code(&self) -> i32 {
        if !self.still_running().is_empty() {
            2
        } else if self.stopped() > 0 || self.done.is_empty() {
            0
        } else {
            1
        }
    }
}

/// Stops each target in order, children first as [`plan`] put them. `audit` sees each
/// outcome as it happens.
pub fn run(
    targets: &[ReapTarget],
    kill: bool,
    grace: Duration,
    judge: &mut impl Judge,
    audit: &mut dyn FnMut(&Done),
) -> Report {
    run_on(&control::Live, targets, kill, grace, judge, audit)
}

pub(crate) fn run_on(
    host: &impl Host,
    targets: &[ReapTarget],
    kill: bool,
    grace: Duration,
    judge: &mut impl Judge,
    audit: &mut dyn FnMut(&Done),
) -> Report {
    let mut report = Report::default();
    for t in targets {
        let stop = StopTarget {
            id: t.id,
            process: t.command.clone(),
            worker_of: None,
            port: None,
        };
        let result =
            control::stop_checked(host, &stop, kill, grace, &mut || judge.still_leftover(t.id));
        let done = Done::new(t.clone(), result);
        audit(&done);
        report.done.push(done);
    }
    report
}

/// SIGKILL for targets that outlived SIGTERM, after a second confirmation. Each is checked
/// as before every signal.
pub fn kill_all(
    targets: &[ReapTarget],
    judge: &mut impl Judge,
    audit: &mut dyn FnMut(&Done),
) -> Report {
    let mut report = Report::default();
    for t in targets {
        let stop = StopTarget {
            id: t.id,
            process: t.command.clone(),
            worker_of: None,
            port: None,
        };
        let result = control::kill(&stop, &mut || judge.still_leftover(t.id));
        let done = Done::new(t.clone(), result);
        audit(&done);
        report.done.push(done);
    }
    report
}

/// Refuses when the config turns destructive actions off, or cannot be read.
pub fn allowed(paths: &Paths) -> Result<(), Refusal> {
    match crate::config::Config::load(paths) {
        Ok((c, _)) if c.destructive => Ok(()),
        Ok((c, _)) => Err(Refusal::Disabled {
            config: paths.tilde(&paths.config),
            line: c.destructive_line,
        }),
        Err(e) => Err(Refusal::Disabled {
            config: format!("{} (unreadable: {})", paths.tilde(&paths.config), e.message),
            line: e.line,
        }),
    }
}

/// The store, opened for audit rows before anything is signalled.
pub fn audit_store(paths: &Paths) -> Result<Store, Refusal> {
    paths
        .ensure_state()
        .and_then(|_| Store::open(&paths.db()))
        .map_err(|e| Refusal::Unaudited {
            error: format!("{e:#}"),
        })
}

/// One audit row: who acted, what was done, to which process, and the result.
pub fn audit_row(actor: &str, action: &str, id: ProcId, command: &str, result: &str) -> AuditRow {
    AuditRow {
        at_ms: crate::sys::now_ms(),
        actor: actor.into(),
        action: action.into(),
        target: format!("pid {} started {} `{command}`", id.pid, id.start_us),
        result: result.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{self, Liveness};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    fn target(pid: i32, start_us: i64) -> ReapTarget {
        ReapTarget {
            id: ProcId { pid, start_us },
            command: format!("job {pid}"),
            agent: Agent::ClaudeCode,
            session: "s1".into(),
            evidence: Evidence::Stamp,
            ram: None,
        }
    }

    /// Processes that answer through `Host` and log every call in one shared log.
    #[derive(Default)]
    struct Fake {
        alive: RefCell<HashMap<i32, i64>>,
        ignores_term: Vec<i32>,
        log: RefCell<Vec<String>>,
    }

    impl Host for Fake {
        fn liveness(&self, pid: i32) -> Option<Liveness> {
            self.log.borrow_mut().push(format!("liveness {pid}"));
            self.alive.borrow().get(&pid).map(|&start_us| Liveness {
                start_us,
                zombie: false,
            })
        }

        fn holds_port(&self, _: i32, _: crate::model::Proto, _: u16) -> Option<bool> {
            unreachable!("reap names no ports")
        }

        fn signal(&self, pid: i32, sig: libc::c_int) -> std::io::Result<()> {
            let name = if sig == libc::SIGKILL { "KILL" } else { "TERM" };
            self.log.borrow_mut().push(format!("{name} {pid}"));
            if !self.alive.borrow().contains_key(&pid) {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            if sig == libc::SIGKILL || !self.ignores_term.contains(&pid) {
                self.alive.borrow_mut().remove(&pid);
            }
            Ok(())
        }
    }

    /// Answers from a fixed set of leftovers, logging each question.
    struct Judged<'a> {
        leftovers: Vec<i32>,
        log: &'a RefCell<Vec<String>>,
    }

    impl Judge for Judged<'_> {
        fn still_leftover(&mut self, id: ProcId) -> Result<(), Refusal> {
            self.log.borrow_mut().push(format!("judge {}", id.pid));
            if self.leftovers.contains(&id.pid) {
                Ok(())
            } else {
                Err(Refusal::NotLeftover {
                    pid: id.pid,
                    why: "Its agent process 900 is still running.".into(),
                })
            }
        }
    }

    #[test]
    fn each_signal_follows_a_verdict_and_an_identity_check() {
        let host = Fake {
            alive: RefCell::new(HashMap::from([(10, 1), (11, 1), (12, 1)])),
            ignores_term: vec![11],
            ..Fake::default()
        };
        let mut judge = Judged {
            leftovers: vec![10, 11],
            log: &host.log,
        };
        let targets = [target(10, 1), target(11, 1), target(12, 1)];
        let mut audited = Vec::new();
        let report = run_on(
            &host,
            &targets,
            false,
            Duration::ZERO,
            &mut judge,
            &mut |d| audited.push(d.result()),
        );
        let log = host.log.borrow().clone();
        assert_eq!(
            log,
            [
                "judge 10",
                "liveness 10",
                "TERM 10",
                "liveness 10",
                "judge 11",
                "liveness 11",
                "TERM 11",
                "liveness 11",
                "judge 12",
            ]
        );
        assert_eq!(report.stopped(), 1);
        assert_eq!(report.still_running().len(), 1);
        assert!(matches!(
            report.done[2].refusal,
            Some(Refusal::NotLeftover { pid: 12, .. })
        ));
        assert_eq!(audited.len(), 3, "every target is audited");
        assert_eq!(report.summary(), "Stopped 1, 1 still running, 1 not signalled.");
        assert_eq!(report.exit_code(), 2);
        assert!(host.alive.borrow().contains_key(&12), "spared");
    }

    #[test]
    fn sigkill_comes_only_with_kill_and_after_the_same_checks() {
        let host = Fake {
            alive: RefCell::new(HashMap::from([(10, 1)])),
            ignores_term: vec![10],
            ..Fake::default()
        };
        let mut judge = Judged {
            leftovers: vec![10],
            log: &host.log,
        };
        let report = run_on(
            &host,
            &[target(10, 1)],
            true,
            Duration::ZERO,
            &mut judge,
            &mut |_| {},
        );
        assert_eq!(
            *host.log.borrow(),
            [
                "judge 10",
                "liveness 10",
                "TERM 10",
                "liveness 10",
                "judge 10",
                "liveness 10",
                "KILL 10",
                "liveness 10",
            ]
        );
        assert!(report.done[0].stopped());
        assert_eq!(report.exit_code(), 0);

        // A session that came back between SIGTERM and SIGKILL keeps it.
        let host = Fake {
            alive: RefCell::new(HashMap::from([(10, 1)])),
            ignores_term: vec![10],
            ..Fake::default()
        };
        let mut calls = 0;
        struct Once<'a>(&'a mut i32);
        impl Judge for Once<'_> {
            fn still_leftover(&mut self, id: ProcId) -> Result<(), Refusal> {
                *self.0 += 1;
                if *self.0 == 1 {
                    Ok(())
                } else {
                    Err(Refusal::NotLeftover {
                        pid: id.pid,
                        why: "Its agent process 950 is still running.".into(),
                    })
                }
            }
        }
        let report = run_on(
            &host,
            &[target(10, 1)],
            true,
            Duration::ZERO,
            &mut Once(&mut calls),
            &mut |_| {},
        );
        assert!(!host.log.borrow().contains(&"KILL 10".to_string()));
        assert!(matches!(
            report.done[0].outcome,
            Some(Outcome::KillRefused { .. })
        ));
        assert_eq!(report.exit_code(), 2);
    }

    /// `sleep` under `sh`, ready once the shell has set its trap.
    fn spawn() -> (Child, ProcId) {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "echo ready; exec sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let pid = child.id() as i32;
        let start_us = sys::liveness(pid).unwrap().start_us;
        (child, ProcId { pid, start_us })
    }

    struct Everything;
    impl Judge for Everything {
        fn still_leftover(&mut self, _: ProcId) -> Result<(), Refusal> {
            Ok(())
        }
    }

    #[test]
    fn a_pid_reused_between_listing_and_signal_is_refused() {
        let (mut child, id) = spawn();
        // Listed with another start time: the pid now belongs to a different process.
        let listed = target(id.pid, id.start_us + 1);
        let report = run(
            &[listed],
            true,
            Duration::from_millis(200),
            &mut Everything,
            &mut |_| {},
        );
        assert_eq!(
            report.done[0].refusal,
            Some(Refusal::Changed { pid: id.pid })
        );
        assert_eq!(report.exit_code(), 1);
        assert!(child.try_wait().unwrap().is_none(), "never signalled");

        let report = run(
            &[target(id.pid, id.start_us)],
            false,
            control::TERM_GRACE,
            &mut Everything,
            &mut |_| {},
        );
        assert!(report.done[0].stopped(), "{report:?}");
        child.wait().unwrap();
    }

    fn snap_of(procs: &[(i32, i32)]) -> Snapshot {
        let processes = procs
            .iter()
            .map(|&(pid, ppid)| {
                serde_json::from_value(serde_json::json!({
                    "id": {"pid": pid, "start_us": pid},
                    "ppid": ppid,
                    "uid": 501,
                    "name": "node",
                    "exe": null,
                    "footprint": null,
                }))
                .unwrap()
            })
            .collect();
        Snapshot {
            viewer_uid: 501,
            processes,
            ..Snapshot::default()
        }
    }

    #[test]
    fn children_come_before_their_parents() {
        // 10 → 11 → 12, and 20 alone; listed oldest first.
        let snap = snap_of(&[(10, 1), (11, 10), (12, 11), (20, 1)]);
        let leftover = |pid: i32| Leftover {
            pid,
            start_us: pid as i64,
            command: "node".into(),
            agent: Agent::ClaudeCode,
            session: "s1".into(),
            evidence: Evidence::Stamp,
            how: String::new(),
            agent_pid: 900,
            age_s: 600,
            ram: None,
            ports: Vec::new(),
            cwd: None,
            caveat: String::new(),
        };
        let mut list: Vec<Leftover> = [10, 11, 12, 20].into_iter().map(leftover).collect();
        children_first(&snap, &mut list);
        let order: Vec<i32> = list.iter().map(|l| l.pid).collect();
        assert_eq!(order, [12, 11, 20, 10]);
    }
}
