//! Reaping: stopping the processes the leftover rule selects, for `scry reap` and the TUI's
//! `X`.
//!
//! The list comes from the same [`Lineage`] every view reads, children before their parents
//! by the real process tree. Right before each signal, SIGKILL included, a [`Judge`] reads
//! the machine and the config again, with nothing cached from an earlier reading: stopping
//! must still be allowed, the process must still be the viewer's own and not the system's,
//! and still a leftover of the session it was listed under. An [`Audit`] row then says what
//! is about to be sent, and [`crate::control`] checks the identity last. SIGTERM comes
//! first, with up to 3 s to exit; SIGKILL only when asked, after the same steps once more.

use std::cmp::Reverse;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::attribution::{Lineage, Verdict};
use crate::control::{self, Gate, Host, Outcome, Refusal, StopTarget};
use crate::engine::{Engine, Labels};
use crate::home::Home;
use crate::live::Collector;
use crate::model::{ProcId, Process, Snapshot};
use crate::paths::Paths;
use crate::store::{AuditRow, Store};
use crate::view::Evidence;
use crate::view::agents::{self, Leftover};

/// One process to reap, as the confirmation and the report name it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReapTarget {
    pub id: ProcId,
    pub command: String,
    /// The agent and the start of the session id, as `claude 0f1e2d3c`.
    pub session: String,
    /// The full id of the session it was listed under. The check before each signal
    /// refuses when the process now belongs to another.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub evidence: Option<Evidence>,
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
            session: format!("{} {}", l.agent.short(), l.session),
            session_id: Some(l.session_id.clone()),
            evidence: Some(l.evidence),
            ram: l.ram,
        }
    }
}

/// The leftovers to reap, children before their parents, so nothing is re-parented to
/// launchd halfway. With `session`, only that session's: its full id, or the start of an id
/// no other session's starts with. What [`control::guard`] refuses is never listed.
pub fn plan(
    snap: &Snapshot,
    lineage: &Lineage,
    home: Option<&Home>,
    session: Option<&str>,
) -> Result<Vec<Leftover>, Refusal> {
    let mut list = agents::leftovers(snap, lineage, home);
    if let Some(s) = session {
        let id = session_named(lineage, &list, s)?;
        list.retain(|l| l.session_id == id);
    }
    list.retain(|l| {
        snap.process(l.pid)
            .is_some_and(|p| control::guard(p, snap).is_ok())
    });
    children_first(snap, &mut list);
    Ok(list)
}

/// The one session `s` names: the session whose id it is, else the only one whose id
/// starts with it.
fn session_named(lineage: &Lineage, list: &[Leftover], s: &str) -> Result<String, Refusal> {
    let mut ids: Vec<String> = lineage
        .sessions
        .iter()
        .map(|v| v.key.id.encode())
        .chain(list.iter().map(|l| l.session_id.clone()))
        .collect();
    ids.sort();
    ids.dedup();
    if ids.iter().any(|id| id == s) {
        return Ok(s.to_string());
    }
    let starting: Vec<String> = ids
        .into_iter()
        .filter(|id| !s.is_empty() && id.starts_with(s))
        .collect();
    match starting.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(Refusal::NoSession {
            session: s.to_string(),
        }),
        _ => Err(Refusal::AmbiguousSession {
            session: s.to_string(),
            ids: starting,
        }),
    }
}

/// Deepest first by the parent chain, then newest first among equals.
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

/// The check right before each signal, ahead of the audit row and the identity check.
pub trait Judge {
    fn check(&mut self, t: &ReapTarget) -> Result<(), Refusal>;
    /// After the audit row, right before the signal: `destructive` and, for a reap, the
    /// keep list, read from the config once more.
    fn confirm(&mut self, t: &ReapTarget) -> Result<(), Refusal>;
}

/// The judge for a reap: the config and the machine read afresh for each question, by an
/// engine of its own, so no argument, stamp or role cached from the listing is used.
pub struct LiveJudge {
    paths: Paths,
    /// The process as the last check read it, for the keep list's last word.
    checked: Option<Process>,
}

impl LiveJudge {
    pub fn new(paths: Paths) -> LiveJudge {
        LiveJudge {
            paths,
            checked: None,
        }
    }
}

impl Judge for LiveJudge {
    fn check(&mut self, t: &ReapTarget) -> Result<(), Refusal> {
        self.checked = None;
        allowed(&self.paths)?;
        // A new collector reads the keep list and grace period again, and its engine has
        // never seen the process.
        let mut c = Collector::with_paths(Labels::None, Some(self.paths.clone()));
        let r = c.read().map_err(|e| Refusal::NotLeftover {
            pid: t.id.pid,
            why: format!("the processes could not be read again: {e:#}"),
        })?;
        verdict_of(&r.snap, &r.lineage, t.id, t.session_id.as_deref())?;
        self.checked = r.snap.process(t.id.pid).cloned();
        Ok(())
    }

    fn confirm(&mut self, t: &ReapTarget) -> Result<(), Refusal> {
        let config = allowed(&self.paths)?;
        let Some(p) = self.checked.as_ref().filter(|p| p.id == t.id) else {
            return Err(Refusal::Changed { pid: t.id.pid });
        };
        let kept = config
            .keep
            .iter()
            .map(|k| crate::keep::expand(k, &self.paths))
            .find(|k| crate::keep::matches(p, k));
        match kept {
            Some(entry) => Err(Refusal::NotLeftover {
                pid: t.id.pid,
                why: crate::attribution::Spare::Kept { entry }.sentence(),
            }),
            None => Ok(()),
        }
    }
}

/// The judge for a stop a person chose, any process of theirs: `scry stop`'s own checks,
/// with the config and the machine read afresh.
pub struct StopJudge {
    paths: Paths,
}

impl StopJudge {
    pub fn new(paths: Paths) -> StopJudge {
        StopJudge { paths }
    }
}

impl Judge for StopJudge {
    fn check(&mut self, t: &ReapTarget) -> Result<(), Refusal> {
        allowed(&self.paths)?;
        let snap =
            Engine::labelling(Labels::None)
                .snapshot()
                .map_err(|e| Refusal::SignalFailed {
                    pid: t.id.pid,
                    error: format!("the processes could not be read again: {e:#}"),
                })?;
        let now = control::resolve(&snap, control::Target::Pid(t.id.pid))?;
        if now.id == t.id {
            Ok(())
        } else {
            Err(Refusal::Changed { pid: t.id.pid })
        }
    }

    /// A stop names its process itself, so the keep list, which only spares processes from
    /// the leftover rule, does not refuse it. `destructive` does.
    fn confirm(&mut self, _: &ReapTarget) -> Result<(), Refusal> {
        allowed(&self.paths).map(|_| ())
    }
}

/// Whether `id` is in `snap`, one Scry may stop, and a leftover in `lineage`, of `session`
/// when one is named.
pub fn verdict_of(
    snap: &Snapshot,
    lineage: &Lineage,
    id: ProcId,
    session: Option<&str>,
) -> Result<(), Refusal> {
    let Some(p) = snap.process(id.pid).filter(|p| p.id == id) else {
        return Err(Refusal::Changed { pid: id.pid });
    };
    control::guard(p, snap)?;
    match lineage.verdict(id.pid) {
        Some(Verdict::Leftover { session: s, .. }) => match session {
            Some(want) if s.id.encode() != want => Err(Refusal::NotLeftover {
                pid: id.pid,
                why: format!("It is now a leftover of another session, {s}."),
            }),
            _ => Ok(()),
        },
        Some(Verdict::Spared { why }) => Err(Refusal::NotLeftover {
            pid: id.pid,
            why: why.sentence(),
        }),
        None => Err(Refusal::NotLeftover {
            pid: id.pid,
            why: match p.role.and_then(crate::catalog::Role::hosts) {
                Some(agent) => format!("It is now a {} agent process.", agent.title()),
                None => "Scry does not judge it.".into(),
            },
        }),
    }
}

/// Where each action is written before it happens and after: an intent row right before
/// each signal, and the outcome once a target is done.
pub trait Audit {
    /// An error refuses the signal.
    fn intent(&mut self, t: &ReapTarget, signal: &str) -> Result<(), Refusal>;
    fn outcome(&mut self, d: &Done) -> Result<(), String>;
}

/// The audit table in Scry's store.
pub struct StoreAudit {
    store: Store,
    actor: &'static str,
    action: String,
}

impl StoreAudit {
    /// Opens the store, or says why nothing can be signalled.
    pub fn open(paths: &Paths, actor: &'static str, action: &str) -> Result<StoreAudit, Refusal> {
        Ok(StoreAudit {
            store: audit_store(paths)?,
            actor,
            action: action.into(),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
}

impl Audit for StoreAudit {
    fn intent(&mut self, t: &ReapTarget, signal: &str) -> Result<(), Refusal> {
        let row = audit_row(
            self.actor,
            &self.action,
            t.id,
            &t.command,
            &format!("sending {signal}"),
        );
        self.store
            .append_audit(&row)
            .map_err(|e| Refusal::Unaudited {
                error: format!("{e:#}"),
            })
    }

    fn outcome(&mut self, d: &Done) -> Result<(), String> {
        let row = audit_row(
            self.actor,
            &self.action,
            d.target.id,
            &d.target.command,
            &d.result(),
        );
        self.store.append_audit(&row).map_err(|e| format!("{e:#}"))
    }
}

/// The judge and the audit together, before each signal to one target.
struct Step<'a, J: Judge, A: Audit + ?Sized> {
    judge: &'a mut J,
    audit: &'a mut A,
    target: &'a ReapTarget,
}

impl<J: Judge, A: Audit + ?Sized> Gate for Step<'_, J, A> {
    fn check(&mut self, id: ProcId) -> Result<(), Refusal> {
        debug_assert_eq!(id, self.target.id);
        self.judge.check(self.target)
    }

    fn intend(&mut self, _: ProcId, signal: &str) -> Result<(), Refusal> {
        self.audit.intent(self.target, signal)
    }

    fn confirm(&mut self, id: ProcId) -> Result<(), Refusal> {
        debug_assert_eq!(id, self.target.id);
        self.judge.confirm(self.target)
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
    /// Why the row with the outcome could not be written, after the signal was sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unaudited: Option<String>,
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
            unaudited: None,
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
            (None, Some(r)) => format!("not signalled: {}", r.reason()),
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

    /// The outcomes that were sent but could not be written down.
    pub fn unaudited(&self) -> Vec<&Done> {
        self.done.iter().filter(|d| d.unaudited.is_some()).collect()
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

fn stop_target(t: &ReapTarget) -> StopTarget {
    StopTarget {
        id: t.id,
        process: t.command.clone(),
        worker_of: None,
        port: None,
    }
}

/// Stops each target in order, children first as [`plan`] put them. `each` sees each
/// outcome as it happens, after its audit row.
pub fn run(
    targets: &[ReapTarget],
    kill: bool,
    grace: Duration,
    judge: &mut impl Judge,
    audit: &mut dyn Audit,
    each: &mut dyn FnMut(&Done),
) -> Report {
    run_on(&control::Live, targets, kill, grace, judge, audit, each)
}

pub(crate) fn run_on(
    host: &impl Host,
    targets: &[ReapTarget],
    kill: bool,
    grace: Duration,
    judge: &mut impl Judge,
    audit: &mut dyn Audit,
    each: &mut dyn FnMut(&Done),
) -> Report {
    let mut report = Report::default();
    for t in targets {
        let mut step = Step {
            judge: &mut *judge,
            audit: &mut *audit,
            target: t,
        };
        let result = control::stop_on(host, &stop_target(t), kill, grace, &mut step);
        report.done.push(finish(t, result, audit, each));
    }
    report
}

/// One stop a person chose, by pid or by port, through the same steps: `judge`, then the
/// audit row, then the identity, before each signal.
pub fn stop_one(
    t: &StopTarget,
    kill: bool,
    grace: Duration,
    judge: &mut impl Judge,
    audit: &mut dyn Audit,
) -> Done {
    let target = ReapTarget {
        id: t.id,
        command: t.process.clone(),
        session: String::new(),
        session_id: None,
        evidence: None,
        ram: None,
    };
    let mut step = Step {
        judge: &mut *judge,
        audit: &mut *audit,
        target: &target,
    };
    let result = control::stop(t, kill, grace, &mut step);
    finish(&target, result, audit, &mut |_| {})
}

/// SIGKILL for targets that outlived SIGTERM, after a second confirmation. Each goes through
/// the same steps as before every signal.
pub fn kill_all(
    targets: &[ReapTarget],
    judge: &mut impl Judge,
    audit: &mut dyn Audit,
    each: &mut dyn FnMut(&Done),
) -> Report {
    kill_all_on(&control::Live, targets, judge, audit, each)
}

pub(crate) fn kill_all_on(
    host: &impl Host,
    targets: &[ReapTarget],
    judge: &mut impl Judge,
    audit: &mut dyn Audit,
    each: &mut dyn FnMut(&Done),
) -> Report {
    let mut report = Report::default();
    for t in targets {
        let mut step = Step {
            judge: &mut *judge,
            audit: &mut *audit,
            target: t,
        };
        let result = control::kill_on(host, &stop_target(t), &mut step);
        report.done.push(finish(t, result, audit, each));
    }
    report
}

fn finish(
    t: &ReapTarget,
    result: Result<Outcome, Refusal>,
    audit: &mut dyn Audit,
    each: &mut dyn FnMut(&Done),
) -> Done {
    let mut done = Done::new(t.clone(), result);
    if let Err(e) = audit.outcome(&done) {
        done.unaudited = Some(e);
    }
    each(&done);
    done
}

/// The config, read now, or a refusal when it turns destructive actions off or cannot be
/// read.
pub fn allowed(paths: &Paths) -> Result<crate::config::Config, Refusal> {
    match crate::config::Config::load(paths) {
        Ok((c, _)) if c.destructive => Ok(c),
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

/// The store, opened for audit rows before anything is signalled. Each row is synced to
/// disk before `append_audit` returns, so an intent row survives a crash or power loss
/// right after the signal it announces.
pub fn audit_store(paths: &Paths) -> Result<Store, Refusal> {
    paths
        .ensure_state()
        .and_then(|_| Store::open_durable(&paths.db()))
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
    use crate::model::Agent;
    use crate::sys::{self, Liveness};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    fn target(pid: i32, start_us: i64) -> ReapTarget {
        ReapTarget {
            id: ProcId { pid, start_us },
            command: format!("job {pid}"),
            session: "claude s1".into(),
            session_id: None,
            evidence: Some(Evidence::Stamp),
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

    struct Running;

    impl control::Watch for Running {
        fn changed(&self) -> bool {
            false
        }
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

        fn watch(&self, pid: i32) -> std::io::Result<Box<dyn control::Watch>> {
            if !self.alive.borrow().contains_key(&pid) {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(Box::new(Running))
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
        fn check(&mut self, t: &ReapTarget) -> Result<(), Refusal> {
            self.log.borrow_mut().push(format!("judge {}", t.id.pid));
            if self.leftovers.contains(&t.id.pid) {
                Ok(())
            } else {
                Err(Refusal::NotLeftover {
                    pid: t.id.pid,
                    why: "Its agent process 900 is still running.".into(),
                })
            }
        }

        fn confirm(&mut self, t: &ReapTarget) -> Result<(), Refusal> {
            self.log.borrow_mut().push(format!("confirm {}", t.id.pid));
            Ok(())
        }
    }

    /// Audit rows kept in memory, in the shared log, until `fails` says the store broke.
    struct Rows<'a> {
        log: &'a RefCell<Vec<String>>,
        rows: Vec<String>,
        fails: bool,
    }

    impl<'a> Rows<'a> {
        fn new(log: &'a RefCell<Vec<String>>) -> Self {
            Rows {
                log,
                rows: Vec::new(),
                fails: false,
            }
        }
    }

    impl Audit for Rows<'_> {
        fn intent(&mut self, t: &ReapTarget, signal: &str) -> Result<(), Refusal> {
            if self.fails {
                return Err(Refusal::Unaudited {
                    error: "disk full".into(),
                });
            }
            self.log
                .borrow_mut()
                .push(format!("audit {} {signal}", t.id.pid));
            self.rows.push(format!("{} sending {signal}", t.id.pid));
            Ok(())
        }

        fn outcome(&mut self, d: &Done) -> Result<(), String> {
            if self.fails {
                return Err("disk full".into());
            }
            self.rows
                .push(format!("{} {}", d.target.id.pid, d.result()));
            Ok(())
        }
    }

    #[test]
    fn each_signal_follows_a_verdict_an_audit_row_and_an_identity_check() {
        let host = Fake {
            alive: RefCell::new(HashMap::from([(10, 1), (11, 1), (12, 1)])),
            ignores_term: vec![11],
            ..Fake::default()
        };
        let mut judge = Judged {
            leftovers: vec![10, 11],
            log: &host.log,
        };
        let mut audit = Rows::new(&host.log);
        let targets = [target(10, 1), target(11, 1), target(12, 1)];
        let mut seen = 0;
        let report = run_on(
            &host,
            &targets,
            false,
            Duration::ZERO,
            &mut judge,
            &mut audit,
            &mut |_| seen += 1,
        );
        let log = host.log.borrow().clone();
        assert_eq!(
            log,
            [
                "judge 10",
                "audit 10 SIGTERM",
                "confirm 10",
                "liveness 10",
                "TERM 10",
                "liveness 10",
                "judge 11",
                "audit 11 SIGTERM",
                "confirm 11",
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
        assert_eq!(seen, 3);
        assert_eq!(
            audit.rows,
            [
                "10 sending SIGTERM",
                "10 exited after SIGTERM in 0 ms",
                "11 sending SIGTERM",
                "11 still running 0 ms after the last signal",
                "12 not signalled: pid 12 is no longer a leftover: Its agent process 900 is still running.",
            ]
        );
        assert_eq!(
            report.summary(),
            "Stopped 1, 1 still running, 1 not signalled."
        );
        assert_eq!(report.exit_code(), 2);
        assert!(host.alive.borrow().contains_key(&12), "spared");
    }

    #[test]
    fn nothing_is_signalled_when_its_audit_row_cannot_be_written() {
        let host = Fake {
            alive: RefCell::new(HashMap::from([(10, 1)])),
            ..Fake::default()
        };
        let mut judge = Judged {
            leftovers: vec![10],
            log: &host.log,
        };
        let mut audit = Rows::new(&host.log);
        audit.fails = true;
        for kill in [false, true] {
            let report = run_on(
                &host,
                &[target(10, 1)],
                kill,
                Duration::ZERO,
                &mut judge,
                &mut audit,
                &mut |_| {},
            );
            assert!(matches!(
                report.done[0].refusal,
                Some(Refusal::Unaudited { .. })
            ));
            assert_eq!(report.done[0].unaudited.as_deref(), Some("disk full"));
            let report = kill_all_on(&host, &[target(10, 1)], &mut judge, &mut audit, &mut |_| {});
            assert!(matches!(
                report.done[0].refusal,
                Some(Refusal::Unaudited { .. })
            ));
        }
        assert!(
            host.log.borrow().iter().all(|l| l.starts_with("judge")),
            "{:?}",
            host.log.borrow()
        );
        assert!(host.alive.borrow().contains_key(&10));
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
        let mut audit = Rows::new(&host.log);
        let report = run_on(
            &host,
            &[target(10, 1)],
            true,
            Duration::ZERO,
            &mut judge,
            &mut audit,
            &mut |_| {},
        );
        assert_eq!(
            *host.log.borrow(),
            [
                "judge 10",
                "audit 10 SIGTERM",
                "confirm 10",
                "liveness 10",
                "TERM 10",
                "liveness 10",
                "judge 10",
                "audit 10 SIGKILL",
                "confirm 10",
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
            fn check(&mut self, t: &ReapTarget) -> Result<(), Refusal> {
                *self.0 += 1;
                if *self.0 == 1 {
                    Ok(())
                } else {
                    Err(Refusal::NotLeftover {
                        pid: t.id.pid,
                        why: "Its agent process 950 is still running.".into(),
                    })
                }
            }
            fn confirm(&mut self, _: &ReapTarget) -> Result<(), Refusal> {
                Ok(())
            }
        }
        let mut audit = Rows::new(&host.log);
        let report = run_on(
            &host,
            &[target(10, 1)],
            true,
            Duration::ZERO,
            &mut Once(&mut calls),
            &mut audit,
            &mut |_| {},
        );
        assert!(!host.log.borrow().contains(&"KILL 10".to_string()));
        assert_eq!(
            report.done[0].result(),
            "still running after SIGTERM; SIGKILL not sent: pid 10 is no longer a leftover: Its agent process 950 is still running."
        );
        assert_eq!(report.exit_code(), 2);
    }

    /// `sleep`, exec'd already when `spawn` returns, so no later exec reads as a change.
    fn spawn() -> (Child, ProcId) {
        let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        let start_us = sys::liveness(pid).unwrap().start_us;
        (child, ProcId { pid, start_us })
    }

    struct Everything;
    impl Judge for Everything {
        fn check(&mut self, _: &ReapTarget) -> Result<(), Refusal> {
            Ok(())
        }
        fn confirm(&mut self, _: &ReapTarget) -> Result<(), Refusal> {
            Ok(())
        }
    }

    #[test]
    fn a_pid_reused_between_listing_and_signal_is_refused() {
        let (mut child, id) = spawn();
        let log = RefCell::new(Vec::new());
        let mut audit = Rows::new(&log);
        // Listed with another start time: the pid now belongs to a different process.
        let listed = target(id.pid, id.start_us + 1);
        let report = run(
            &[listed],
            true,
            Duration::from_millis(200),
            &mut Everything,
            &mut audit,
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
            &mut audit,
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

    /// A process stamped by `session`, whose agent, pid 800, is gone: a leftover by the rule.
    fn stamped(s: &mut Snapshot, pid: i32, exe: &str, session: &str) {
        let mut p = snap_of(&[(pid, 1)]).processes.remove(0);
        p.id.start_us = 1;
        p.exe = Some(exe.into());
        p.stamps.claude_code_session_id = Some(session.into());
        p.stamps.claude_pid = Some(800);
        s.processes.push(p);
    }

    fn stamped_leftover(pid: i32, exe: &str) -> Snapshot {
        let mut s = snap_of(&[]);
        s.taken_at_ms = 10_000_000;
        stamped(&mut s, pid, exe, "5e55a0aa-1111-2222");
        s
    }

    fn rule(s: &Snapshot) -> Lineage {
        let policy = crate::attribution::Policy {
            grace: Duration::ZERO,
            scry: None,
            ..crate::attribution::Policy::default()
        };
        Lineage::build(s, &crate::attribution::Known::default(), &policy)
    }

    #[test]
    fn a_system_process_is_never_reaped_whatever_the_rule_says() {
        for (pid, exe) in [
            (4000, "/usr/sbin/httpd"),
            (4000, "/System/Library/x"),
            (50, "/opt/x"),
        ] {
            let s = stamped_leftover(pid, exe);
            let l = rule(&s);
            assert!(l.is_leftover(pid), "the rule selects it");
            let id = s.processes[0].id;
            assert!(
                matches!(verdict_of(&s, &l, id, None), Err(Refusal::Protected { .. })),
                "{exe}: {:?}",
                verdict_of(&s, &l, id, None)
            );
            assert_eq!(plan(&s, &l, None, None), Ok(Vec::new()), "never listed");
        }
        let s = stamped_leftover(4000, "/opt/homebrew/bin/node");
        let l = rule(&s);
        assert_eq!(verdict_of(&s, &l, s.processes[0].id, None), Ok(()));
    }

    #[test]
    fn session_takes_a_full_id_or_a_prefix_only_one_session_has() {
        let mut s = snap_of(&[]);
        s.taken_at_ms = 10_000_000;
        stamped(&mut s, 4001, "/opt/x", "12345678-aaaa");
        stamped(&mut s, 4002, "/opt/x", "12345678-bbbb");
        stamped(&mut s, 4003, "/opt/x", "99999999-cccc");
        let l = rule(&s);
        let pids = |session: &str| -> Result<Vec<i32>, Refusal> {
            Ok(plan(&s, &l, None, Some(session))?
                .iter()
                .map(|x| x.pid)
                .collect())
        };
        assert_eq!(pids("12345678-aaaa"), Ok(vec![4001]));
        assert_eq!(pids("12345678-b"), Ok(vec![4002]));
        assert_eq!(pids("9"), Ok(vec![4003]));
        for ambiguous in ["12345678", "12345678-", "1"] {
            assert_eq!(
                pids(ambiguous),
                Err(Refusal::AmbiguousSession {
                    session: ambiguous.into(),
                    ids: vec!["12345678-aaaa".into(), "12345678-bbbb".into()],
                })
            );
        }
        for none in ["12345678-aaaa-more", "12345678-c", ""] {
            assert_eq!(
                pids(none),
                Err(Refusal::NoSession {
                    session: none.into()
                })
            );
        }

        // Each signal confirms the session it was listed under.
        let id = s.process(4001).unwrap().id;
        assert_eq!(verdict_of(&s, &l, id, Some("12345678-aaaa")), Ok(()));
        assert!(matches!(
            verdict_of(&s, &l, id, Some("12345678-bbbb")),
            Err(Refusal::NotLeftover { .. })
        ));
    }

    /// A copy of this test binary, detached from this test's tree and terminal, stamped by
    /// a session whose agent is gone. macOS hides the environment of its own binaries, such
    /// as `/bin/bash`, so a stamp is only seen on a program like this one. Each line written
    /// to its fifo lets it take one more step: first it execs itself without the stamp, so
    /// its executable stays the same; then it exits.
    struct Detached {
        id: ProcId,
        fifo: std::path::PathBuf,
    }

    impl Detached {
        fn start(dir: &std::path::Path) -> Detached {
            use std::os::unix::process::CommandExt;
            let fifo = dir.join("ctl");
            let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            let mut gone = Command::new("/usr/bin/true").spawn().unwrap();
            let dead = gone.id();
            gone.wait().unwrap();
            let mut launcher = Command::new("/bin/bash");
            launcher
                .args([
                    "-c",
                    r#""$0" reap::tests::stamped_helper --exact --nocapture --quiet </dev/null >/dev/null 2>&1 & echo $!"#,
                ])
                .arg(std::env::current_exe().unwrap())
                .env("SCRY_HELPER_FIFO", &fifo)
                .env("CLAUDE_CODE_SESSION_ID", "5e55a0aa-3333-4444")
                .env("CLAUDE_PID", dead.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::piped());
            unsafe {
                launcher.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
            };
            let out = launcher.output().unwrap();
            let pid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
            let start_us = sys::liveness(pid).unwrap().start_us;
            let d = Detached {
                id: ProcId { pid, start_us },
                fifo,
            };
            d.wait_until(|p| !p.stamps.is_empty());
            d
        }

        fn step(&self) {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&self.fifo)
                .unwrap();
            writeln!(f, "go").unwrap();
        }

        /// Waits until a fresh reading of the process passes `ok`.
        fn wait_until(&self, ok: impl Fn(&crate::model::Process) -> bool) {
            let until = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let snap = crate::engine::Engine::labelling(crate::engine::Labels::None)
                    .snapshot()
                    .unwrap();
                let p = snap.process(self.id.pid).expect("still running");
                if ok(p) {
                    return;
                }
                assert!(std::time::Instant::now() < until, "never got there");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Detached {
        fn drop(&mut self) {
            if sys::liveness(self.id.pid).is_some_and(|l| l.start_us == self.id.start_us) {
                unsafe { libc::kill(self.id.pid, libc::SIGKILL) };
            }
        }
    }

    /// [`Detached`]'s body, run only inside the process it starts.
    #[test]
    fn stamped_helper() {
        use std::os::unix::process::CommandExt;
        let Some(fifo) = std::env::var_os("SCRY_HELPER_FIFO") else {
            return;
        };
        let ctl = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let mut line = String::new();
        BufReader::new(ctl).read_line(&mut line).unwrap();
        if std::env::var_os("SCRY_HELPER_EXECED").is_some() {
            std::process::exit(0);
        }
        let e = Command::new(std::env::current_exe().unwrap())
            .args(std::env::args().skip(1))
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("CLAUDE_PID")
            .env("SCRY_HELPER_EXECED", "1")
            .exec();
        panic!("exec: {e}");
    }

    fn paths_with(dir: &std::path::Path, config: &str) -> Paths {
        let home = dir.join("scry");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), config).unwrap();
        Paths::new(dir.to_path_buf(), Some(home))
    }

    #[test]
    fn the_judge_reads_arguments_and_stamps_afresh_before_each_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with(tmp.path(), "leftover_grace_secs = 0\n");
        let d = Detached::start(tmp.path());
        let mut t = target(d.id.pid, d.id.start_us);
        t.session_id = Some("5e55a0aa-3333-4444".into());
        let mut judge = LiveJudge::new(paths);
        assert_eq!(judge.check(&t), Ok(()), "a leftover at first");
        // The same pid, start time and executable, without the stamp.
        d.step();
        d.wait_until(|p| p.stamps.is_empty());
        let now = judge.check(&t);
        assert!(matches!(now, Err(Refusal::NotLeftover { .. })), "{now:?}");
    }

    #[test]
    fn the_judge_reads_destructive_and_the_keep_list_before_each_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with(tmp.path(), "leftover_grace_secs = 0\n");
        let d = Detached::start(tmp.path());
        let t = target(d.id.pid, d.id.start_us);
        let mut judge = LiveJudge::new(paths.clone());
        assert_eq!(judge.check(&t), Ok(()), "a leftover at first");
        let write = |text: &str| std::fs::write(&paths.config, text).unwrap();
        write("leftover_grace_secs = 0\ndestructive = false\n");
        assert_eq!(
            judge.check(&t),
            Err(Refusal::Disabled {
                config: paths.tilde(&paths.config),
                line: Some(2)
            })
        );
        let exe = std::env::current_exe().unwrap().display().to_string();
        write(&format!("leftover_grace_secs = 0\nkeep = [{exe:?}]\n"));
        let now = judge.check(&t);
        assert!(
            matches!(&now, Err(Refusal::NotLeftover { why, .. }) if why.contains("keep list")),
            "{now:?}"
        );
        write("leftover_grace_secs = 0\n");
        assert_eq!(judge.check(&t), Ok(()));
        // A stop's judge reads the switch as well.
        write("destructive = false\n");
        assert!(matches!(
            StopJudge::new(paths.clone()).check(&t),
            Err(Refusal::Disabled { .. })
        ));
    }

    /// Audit rows kept in memory, saving `text` as the config while each intent row is
    /// written, as an editor could.
    struct SavesConfig<'a> {
        config: &'a std::path::Path,
        text: String,
        rows: Vec<String>,
    }

    impl Audit for SavesConfig<'_> {
        fn intent(&mut self, t: &ReapTarget, signal: &str) -> Result<(), Refusal> {
            std::fs::write(self.config, &self.text).unwrap();
            self.rows.push(format!("{} sending {signal}", t.id.pid));
            Ok(())
        }

        fn outcome(&mut self, d: &Done) -> Result<(), String> {
            self.rows
                .push(format!("{} {}", d.target.id.pid, d.result()));
            Ok(())
        }
    }

    #[test]
    fn a_config_saved_while_the_intent_row_is_written_still_refuses_the_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let base = "leftover_grace_secs = 0\n";
        let paths = paths_with(tmp.path(), base);
        let d = Detached::start(tmp.path());
        let t = target(d.id.pid, d.id.start_us);
        let exe = std::env::current_exe().unwrap().display().to_string();
        for (saved, refused) in [
            (format!("{base}destructive = false\n"), "turned off"),
            (format!("{base}keep = [{exe:?}]\n"), "on your keep list"),
        ] {
            for sigkill_only in [false, true] {
                std::fs::write(&paths.config, base).unwrap();
                let mut audit = SavesConfig {
                    config: &paths.config,
                    text: saved.clone(),
                    rows: Vec::new(),
                };
                let mut judge = LiveJudge::new(paths.clone());
                let report = if sigkill_only {
                    kill_all(
                        std::slice::from_ref(&t),
                        &mut judge,
                        &mut audit,
                        &mut |_| {},
                    )
                } else {
                    run(
                        std::slice::from_ref(&t),
                        true,
                        Duration::from_millis(200),
                        &mut judge,
                        &mut audit,
                        &mut |_| {},
                    )
                };
                let why = report.done[0].result();
                assert!(why.contains(refused), "{why}");
                assert_eq!(audit.rows.len(), 2, "the intent row, then the refusal");
                let live = sys::liveness(d.id.pid).expect("never signalled");
                assert_eq!(live.start_us, d.id.start_us);
            }
        }
    }

    #[test]
    fn the_audit_store_flushes_each_row_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with(tmp.path(), "");
        let store = audit_store(&paths).unwrap();
        // 2 is FULL: each commit is synced before the signal that follows it.
        assert_eq!(store.synchronous().unwrap(), 2);
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
            session_id: "s1".into(),
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
