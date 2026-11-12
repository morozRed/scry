//! Stopping a process. Every caller, human or agent, goes through the same steps before
//! each signal, SIGKILL included, in this order:
//!
//! 1. a [`Gate`] reads the machine and the config again: stopping is still allowed, the
//!    process is still the viewer's own and not the system's, and, for a reap, still a
//!    leftover of the session it was listed under;
//! 2. the gate writes an audit row saying what is about to be sent, and nothing is sent
//!    when it cannot;
//! 3. a kqueue watch is put on the pid, then the identity `(pid, start_time)` is read
//!    again, then the watch is asked, without waiting, whether that process has exited;
//! 4. the signal.
//!
//! The watch belongs to the process, not the number, so a process that exits after step 3
//! begins is caught even when its pid is handed to another before the identity is read.
//! What remains is the few microseconds between the last question to the watch and
//! `kill(2)`: the process would have to exit and macOS hand its pid to a new process in
//! that time, which takes the pid counter going all the way round. No call on macOS
//! signals a process by anything but its pid, so that window cannot be closed.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::model::{ProcId, Process, Proto, Snapshot};
use crate::sys::{self, Liveness};
use crate::view::ports::port_label;

/// How long SIGTERM gets before Scry reports the process as still running.
pub const TERM_GRACE: Duration = Duration::from_secs(3);
const KILL_GRACE: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(25);

/// Executables under these prefixes belong to macOS.
const SYSTEM_PREFIXES: [&str; 4] = ["/System/", "/usr/libexec/", "/usr/sbin/", "/sbin/"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Pid(i32),
    Port(u16),
}

impl FromStr for Target {
    type Err = String;

    /// `4821` is a pid, `:3000` a port.
    fn from_str(s: &str) -> Result<Self, String> {
        match s.strip_prefix(':') {
            Some(port) => port
                .parse()
                .map(Target::Port)
                .map_err(|_| format!("`{s}` is not a port")),
            None => s
                .parse()
                .map(Target::Pid)
                .map_err(|_| format!("`{s}` is neither a pid nor :port")),
        }
    }
}

/// A process chosen to stop, with what the confirmation shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StopTarget {
    pub id: ProcId,
    /// The command label, or the process name when there is none. For a multiprocessing
    /// worker, the label of the process it works for.
    pub process: String,
    /// The pid of that process, when the target is a worker.
    pub worker_of: Option<i32>,
    /// Set when the target was named by port. The pid must still hold it when signalled.
    pub port: Option<(Proto, u16)>,
}

impl StopTarget {
    /// How a confirmation names the process: `` `next dev` `` or `` a worker of `uvicorn …` ``.
    pub fn described(&self) -> String {
        let label = crate::view::ports::clip(&self.process, 120);
        match self.worker_of {
            Some(_) => format!("a worker of `{label}`"),
            None => format!("`{label}`"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "refused", rename_all = "snake_case")]
pub enum Refusal {
    NotFound {
        target: String,
    },
    NotYours {
        pid: i32,
        owner: String,
    },
    Protected {
        pid: i32,
        reason: String,
    },
    Ambiguous {
        port: u16,
        pids: Vec<i32>,
    },
    /// The pid exited, or now belongs to a different process than the one chosen.
    Changed {
        pid: i32,
    },
    NotHolding {
        pid: i32,
        port: String,
    },
    SignalFailed {
        pid: i32,
        error: String,
    },
    /// The leftover rule no longer selects it, re-checked just before the signal.
    NotLeftover {
        pid: i32,
        why: String,
    },
    /// `destructive = false` in the config.
    Disabled {
        config: String,
        line: Option<usize>,
    },
    /// The audit row could not be written, so nothing is signalled.
    Unaudited {
        error: String,
    },
    /// `--session` named the start of more than one session's id.
    AmbiguousSession {
        session: String,
        ids: Vec<String>,
    },
    /// `--session` named no session Scry knows.
    NoSession {
        session: String,
    },
}

impl Refusal {
    /// Why, in a sentence, without saying that nothing was signalled.
    pub fn reason(&self) -> String {
        match self {
            Refusal::NotFound { target } => format!("Nothing found for {target}."),
            Refusal::NotYours { pid, owner } => {
                format!("pid {pid} is owned by {owner}. Scry stops only your own processes.")
            }
            Refusal::Protected { pid, reason } => format!("pid {pid} is protected: {reason}."),
            Refusal::Ambiguous { port, pids } => format!(
                ":{port} is held by several processes ({}). Stop one by pid.",
                pids.iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Refusal::Changed { pid } => {
                format!("pid {pid} is no longer the process that was chosen.")
            }
            Refusal::NotHolding { pid, port } => format!("pid {pid} no longer has {port} open."),
            Refusal::SignalFailed { pid, error } => {
                format!("Signalling pid {pid} failed: {error}.")
            }
            Refusal::NotLeftover { pid, why } => format!(
                "pid {pid} is no longer a leftover: {}.",
                why.trim_end_matches('.')
            ),
            Refusal::Disabled { config, line } => {
                let at = line.map(|l| format!(" line {l}")).unwrap_or_default();
                format!(
                    "Stopping is turned off by `destructive = false` in {config}{at}. Set it to true to allow stop and reap."
                )
            }
            Refusal::Unaudited { error } => {
                format!("The audit row could not be written: {error}")
            }
            Refusal::AmbiguousSession { session, ids } => format!(
                "`{session}` starts more than one session's id: {}. Give more of it.",
                ids.join(", ")
            ),
            Refusal::NoSession { session } => {
                format!("No session Scry knows has an id that is or starts with `{session}`.")
            }
        }
    }

    /// The reason short enough for one footer line at 80 columns.
    pub fn brief(&self) -> String {
        match self {
            Refusal::NotYours { pid, .. } => format!("pid {pid} is not yours."),
            Refusal::Protected { pid, .. } => format!("pid {pid} is protected."),
            Refusal::NotHolding { pid, .. } => format!("pid {pid} closed its port."),
            Refusal::SignalFailed { pid, .. } => format!("signalling pid {pid} failed."),
            Refusal::NotLeftover { pid, .. } => format!("pid {pid} is no longer a leftover."),
            Refusal::Disabled { config, line } => {
                let name = config.rsplit('/').next().unwrap_or(config);
                let name = name.split(' ').next().unwrap_or(name);
                let at = line.map(|l| format!(" line {l}")).unwrap_or_default();
                format!("`destructive = false` in {name}{at}.")
            }
            Refusal::Unaudited { .. } => "the audit row could not be written.".into(),
            other => other.reason(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Changed { .. } | Refusal::NotHolding { .. } | Refusal::NotLeftover { .. } => {
                write!(f, "{} Nothing was signalled.", self.reason())
            }
            Refusal::Unaudited { error } => write!(
                f,
                "The audit row could not be written, so nothing was signalled: {error}"
            ),
            other => f.write_str(&other.reason()),
        }
    }
}

impl std::error::Error for Refusal {}

/// Chooses the process a target names, or says why it may not be stopped.
pub fn resolve(snap: &Snapshot, target: Target) -> Result<StopTarget, Refusal> {
    let (pid, port) = match target {
        Target::Pid(pid) => (pid, None),
        Target::Port(port) => {
            let holders: Vec<_> = snap
                .sockets
                .iter()
                .filter(|s| s.is_listening() && s.local_port == port)
                .collect();
            let mut pids: Vec<i32> = holders.iter().map(|s| s.pid).collect();
            pids.sort();
            pids.dedup();
            match pids.as_slice() {
                [] => {
                    return Err(Refusal::NotFound {
                        target: format!(":{port}"),
                    });
                }
                [pid] => (*pid, Some((holders[0].proto, port))),
                _ => return Err(Refusal::Ambiguous { port, pids }),
            }
        }
    };
    let Some(p) = snap.process(pid) else {
        return Err(Refusal::NotFound {
            target: format!("pid {pid}"),
        });
    };
    guard(p, snap)?;
    let parent = snap.worker_parent(p);
    Ok(StopTarget {
        id: p.id,
        process: parent.unwrap_or(p).label(),
        worker_of: parent.map(|w| w.id.pid),
        port,
    })
}

/// The macOS directory an executable runs from, if it is one of macOS's own.
pub fn system_prefix(exe: &str) -> Option<&'static str> {
    SYSTEM_PREFIXES.iter().copied().find(|s| exe.starts_with(s))
}

/// Refuses what Scry never stops, whatever chose it: pids below 100, launchd, macOS's own
/// executables, other users' processes and this process.
pub fn guard(p: &Process, snap: &Snapshot) -> Result<(), Refusal> {
    let pid = p.id.pid;
    let protected = |reason: &str| {
        Err(Refusal::Protected {
            pid,
            reason: reason.into(),
        })
    };
    if pid < 100 {
        return protected("pids below 100 belong to the system");
    }
    if p.name == "launchd" {
        return protected("it is launchd");
    }
    if let Some(prefix) = p.exe.as_deref().and_then(system_prefix) {
        return protected(&format!("it runs from {prefix}"));
    }
    if p.uid != snap.viewer_uid {
        return Err(Refusal::NotYours {
            pid,
            owner: snap.owner_name(p.uid),
        });
    }
    if pid == std::process::id() as i32 {
        return protected("it is this scry process");
    }
    Ok(())
}

/// The identity check before a signal: same pid, same start time, not already exited.
pub fn check_identity(expected: ProcId, now: Option<Liveness>) -> Result<(), Refusal> {
    match now {
        Some(l) if l.start_us == expected.start_us && !l.zombie => Ok(()),
        _ => Err(Refusal::Changed { pid: expected.pid }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    /// The process exited after `signal`.
    Exited { signal: String, after_ms: u64 },
    /// Still alive after SIGTERM and its grace period. SIGKILL needs `--kill`.
    StillRunning { after_ms: u64 },
    /// Still alive after SIGTERM, and SIGKILL was not sent: the check before it failed.
    KillRefused { after_ms: u64, why: String },
}

/// What must hold right before each signal besides the identity, and the audit row that
/// comes before it. Every stop and reap passes one; see the module's steps.
pub trait Gate {
    /// Everything but the identity, read again: allowed, the viewer's own and not the
    /// system's, and for a reap still a leftover.
    fn check(&mut self, id: ProcId) -> Result<(), Refusal>;
    /// Records that `signal` is about to go to `id`. An error refuses the signal.
    fn intend(&mut self, id: ProcId, signal: &str) -> Result<(), Refusal>;
}

/// What stopping needs from the system. Tests stand in for it to show the order of the
/// checks and signals.
pub(crate) trait Host {
    fn liveness(&self, pid: i32) -> Option<Liveness>;
    fn holds_port(&self, pid: i32, proto: Proto, port: u16) -> Option<bool>;
    /// Starts watching the process that holds `pid` for its exit.
    fn watch(&self, pid: i32) -> std::io::Result<Box<dyn Watch>>;
    fn signal(&self, pid: i32, sig: libc::c_int) -> std::io::Result<()>;
}

/// A watch from [`Host::watch`].
pub(crate) trait Watch {
    /// Whether the process exited since the watch began. An error counts as an exit.
    fn exited(&self) -> bool;
}

impl Watch for sys::kqueue::ExitWatch {
    fn exited(&self) -> bool {
        sys::kqueue::ExitWatch::exited(self).unwrap_or(true)
    }
}

pub(crate) struct Live;

impl Host for Live {
    fn liveness(&self, pid: i32) -> Option<Liveness> {
        sys::liveness(pid)
    }

    fn holds_port(&self, pid: i32, proto: Proto, port: u16) -> Option<bool> {
        sys::sockets::pid_holds_port(pid, proto, port)
    }

    fn watch(&self, pid: i32) -> std::io::Result<Box<dyn Watch>> {
        Ok(Box::new(sys::kqueue::ExitWatch::new(pid)?))
    }

    fn signal(&self, pid: i32, sig: libc::c_int) -> std::io::Result<()> {
        if unsafe { libc::kill(pid, sig) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// SIGTERM, then up to `grace` for the process to exit. With `kill`, a process still alive
/// after that gets SIGKILL. `gate` comes before each signal.
pub fn stop(
    t: &StopTarget,
    kill: bool,
    grace: Duration,
    gate: &mut dyn Gate,
) -> Result<Outcome, Refusal> {
    stop_on(&Live, t, kill, grace, gate)
}

pub(crate) fn stop_on(
    host: &impl Host,
    t: &StopTarget,
    kill: bool,
    grace: Duration,
    gate: &mut dyn Gate,
) -> Result<Outcome, Refusal> {
    let started = Instant::now();
    let elapsed = || started.elapsed().as_millis() as u64;
    if let Some((proto, port)) = t.port
        && host.holds_port(t.id.pid, proto, port) != Some(true)
    {
        return Err(Refusal::NotHolding {
            pid: t.id.pid,
            port: port_label(proto, port),
        });
    }
    gate.check(t.id)?;
    gate.intend(t.id, "SIGTERM")?;
    // The port check and the gate read the pid's files, and the pid can be reused
    // meanwhile, so the identity is read after them, right before the signal.
    signal_exact(host, t.id, libc::SIGTERM)?;
    let term_worked = || {
        Ok(Outcome::Exited {
            signal: "SIGTERM".into(),
            after_ms: elapsed(),
        })
    };
    if wait_gone(host, t.id, grace) {
        return term_worked();
    }
    if !kill {
        return Ok(Outcome::StillRunning {
            after_ms: elapsed(),
        });
    }
    match gate.check(t.id).and_then(|()| gate.intend(t.id, "SIGKILL")) {
        Err(Refusal::Changed { .. }) => return term_worked(),
        Err(r) => {
            return Ok(Outcome::KillRefused {
                after_ms: elapsed(),
                why: r.reason(),
            });
        }
        Ok(()) => {}
    }
    // Gone between the wait and now, or as SIGKILL was sent: SIGTERM ended it.
    match signal_exact(host, t.id, libc::SIGKILL) {
        Err(Refusal::Changed { .. }) => return term_worked(),
        other => other?,
    }
    let after_ms = elapsed();
    if wait_gone(host, t.id, KILL_GRACE) {
        Ok(Outcome::Exited {
            signal: "SIGKILL".into(),
            after_ms,
        })
    } else {
        Ok(Outcome::StillRunning { after_ms })
    }
}

/// SIGKILL alone, for a process that outlived SIGTERM and a second confirmation, after
/// `gate`.
pub fn kill(t: &StopTarget, gate: &mut dyn Gate) -> Result<Outcome, Refusal> {
    kill_on(&Live, t, gate)
}

pub(crate) fn kill_on(
    host: &impl Host,
    t: &StopTarget,
    gate: &mut dyn Gate,
) -> Result<Outcome, Refusal> {
    let started = Instant::now();
    gate.check(t.id)?;
    gate.intend(t.id, "SIGKILL")?;
    signal_exact(host, t.id, libc::SIGKILL)?;
    let after_ms = started.elapsed().as_millis() as u64;
    if wait_gone(host, t.id, KILL_GRACE) {
        Ok(Outcome::Exited {
            signal: "SIGKILL".into(),
            after_ms,
        })
    } else {
        Ok(Outcome::StillRunning { after_ms })
    }
}

/// Steps 3 and 4: the watch, the identity, the watch again, then the signal.
fn signal_exact(host: &impl Host, id: ProcId, sig: libc::c_int) -> Result<(), Refusal> {
    let watch = host.watch(id.pid).map_err(|e| match e.raw_os_error() {
        Some(libc::ESRCH) => Refusal::Changed { pid: id.pid },
        _ => Refusal::SignalFailed {
            pid: id.pid,
            error: format!("cannot watch it: {e}"),
        },
    })?;
    check_identity(id, host.liveness(id.pid))?;
    if watch.exited() {
        return Err(Refusal::Changed { pid: id.pid });
    }
    match host.signal(id.pid, sig) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Err(Refusal::Changed { pid: id.pid }),
        Err(e) => Err(Refusal::SignalFailed {
            pid: id.pid,
            error: e.to_string(),
        }),
    }
}

fn wait_gone(host: &impl Host, id: ProcId, timeout: Duration) -> bool {
    let until = Instant::now() + timeout;
    loop {
        if check_identity(id, host.liveness(id.pid)).is_err() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use std::cell::{Cell, RefCell};
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    fn typical() -> Snapshot {
        fixture::parse(include_str!("../../../fixtures/typical.json")).unwrap()
    }

    #[test]
    fn targets_parse_as_pid_or_port() {
        assert_eq!("4821".parse(), Ok(Target::Pid(4821)));
        assert_eq!(":3000".parse(), Ok(Target::Port(3000)));
        assert!(":x".parse::<Target>().is_err());
        assert!("abc".parse::<Target>().is_err());
    }

    #[test]
    fn a_port_resolves_to_its_one_holder() {
        let t = resolve(&typical(), Target::Port(3000)).unwrap();
        assert_eq!(t.id.pid, 1320);
        assert_eq!(t.process, "next dev");
        assert_eq!(t.port, Some((Proto::Tcp, 3000)));
        assert_eq!(t.described(), "`next dev`");
    }

    #[test]
    fn a_worker_is_described_by_the_process_it_works_for() {
        let mut snap = typical();
        let mut worker = snap.process(1320).unwrap().clone();
        worker.id.pid = 1330;
        worker.ppid = 1320;
        worker.worker = true;
        snap.processes.push(worker);
        for s in snap.sockets.iter_mut().filter(|s| s.local_port == 3000) {
            s.pid = 1330;
        }
        let t = resolve(&snap, Target::Port(3000)).unwrap();
        assert_eq!((t.id.pid, t.worker_of), (1330, Some(1320)));
        assert_eq!(t.described(), "a worker of `next dev`");
    }

    #[test]
    fn system_and_other_users_processes_are_refused() {
        let snap = typical();
        assert!(matches!(
            resolve(&snap, Target::Port(22)),
            Err(Refusal::Protected { pid: 1, .. })
        ));
        assert!(matches!(
            resolve(&snap, Target::Pid(412)),
            Err(Refusal::Protected { .. })
        ));
        let mut s = snap.clone();
        let p = s.processes.iter_mut().find(|p| p.id.pid == 412).unwrap();
        p.exe = Some("/opt/x/server".into());
        assert_eq!(
            resolve(&s, Target::Pid(412)),
            Err(Refusal::NotYours {
                pid: 412,
                owner: "root".into()
            })
        );
        assert!(matches!(
            resolve(&snap, Target::Pid(99999)),
            Err(Refusal::NotFound { .. })
        ));
        assert!(matches!(
            resolve(&snap, Target::Port(9)),
            Err(Refusal::NotFound { .. })
        ));
    }

    #[test]
    fn several_holders_are_ambiguous() {
        let mut snap = typical();
        let mut other = snap.sockets[2].clone();
        other.pid = 1321;
        snap.sockets.push(other);
        assert_eq!(
            resolve(&snap, Target::Port(3000)),
            Err(Refusal::Ambiguous {
                port: 3000,
                pids: vec![1320, 1321]
            })
        );
    }

    #[test]
    fn identity_must_match_start_time_and_be_alive() {
        let id = ProcId {
            pid: 500,
            start_us: 10,
        };
        let live = |start_us, zombie| Some(Liveness { start_us, zombie });
        assert_eq!(check_identity(id, live(10, false)), Ok(()));
        assert!(check_identity(id, live(11, false)).is_err());
        assert!(check_identity(id, live(10, true)).is_err());
        assert!(check_identity(id, None).is_err());
    }

    /// A process that answers through `Host` and logs every call.
    #[derive(Default)]
    struct Fake {
        start_us: Cell<i64>,
        exited: Cell<bool>,
        /// Set to make the pid be reused, with this start time, while its files are read.
        reused_as: Option<i64>,
        /// The process exits on its own once `liveness` has been asked this many times.
        exits_after: Cell<Option<usize>>,
        /// The watch reports an exit that came after the identity was read.
        exits_after_identity: bool,
        ignores_term: bool,
        log: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn gone(&self) -> bool {
            if self.exits_after.get() == Some(0) {
                self.exited.set(true);
            }
            self.exited.get()
        }
    }

    struct FakeWatch(bool);

    impl Watch for FakeWatch {
        fn exited(&self) -> bool {
            self.0
        }
    }

    impl Host for Fake {
        fn liveness(&self, _pid: i32) -> Option<Liveness> {
            self.log.borrow_mut().push("liveness");
            let gone = self.gone();
            if let Some(n) = self.exits_after.get().filter(|&n| n > 0) {
                self.exits_after.set(Some(n - 1));
            }
            (!gone).then(|| Liveness {
                start_us: self.start_us.get(),
                zombie: false,
            })
        }

        fn holds_port(&self, _pid: i32, _proto: Proto, _port: u16) -> Option<bool> {
            self.log.borrow_mut().push("holds_port");
            if let Some(s) = self.reused_as {
                self.start_us.set(s);
            }
            Some(true)
        }

        fn watch(&self, _pid: i32) -> std::io::Result<Box<dyn Watch>> {
            self.log.borrow_mut().push("watch");
            if self.gone() {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(Box::new(FakeWatch(self.exits_after_identity)))
        }

        fn signal(&self, _pid: i32, sig: libc::c_int) -> std::io::Result<()> {
            let kill = sig == libc::SIGKILL;
            self.log
                .borrow_mut()
                .push(if kill { "SIGKILL" } else { "SIGTERM" });
            if self.gone() {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            if kill || !self.ignores_term {
                self.exited.set(true);
            }
            Ok(())
        }
    }

    /// A gate that allows everything and logs, in the host's log, each time it is asked.
    struct Logged<'a>(&'a RefCell<Vec<&'static str>>);

    impl Gate for Logged<'_> {
        fn check(&mut self, _: ProcId) -> Result<(), Refusal> {
            self.0.borrow_mut().push("check");
            Ok(())
        }

        fn intend(&mut self, _: ProcId, signal: &str) -> Result<(), Refusal> {
            self.0.borrow_mut().push(match signal {
                "SIGKILL" => "intend SIGKILL",
                _ => "intend SIGTERM",
            });
            Ok(())
        }
    }

    /// The gate of a stop that nothing else constrains, for the live tests below.
    struct Open;

    impl Gate for Open {
        fn check(&mut self, _: ProcId) -> Result<(), Refusal> {
            Ok(())
        }

        fn intend(&mut self, _: ProcId, _: &str) -> Result<(), Refusal> {
            Ok(())
        }
    }

    fn fake_target(port: bool) -> StopTarget {
        StopTarget {
            id: ProcId {
                pid: 500,
                start_us: 0,
            },
            process: "server".into(),
            worker_of: None,
            port: port.then_some((Proto::Tcp, 3000)),
        }
    }

    fn stop_fake(host: &Fake, t: &StopTarget, kill: bool) -> Result<Outcome, Refusal> {
        stop_on(host, t, kill, Duration::ZERO, &mut Logged(&host.log))
    }

    #[test]
    fn a_pid_reused_during_the_port_check_is_not_signalled() {
        let host = Fake {
            reused_as: Some(1),
            ..Fake::default()
        };
        assert_eq!(
            stop_fake(&host, &fake_target(true), true),
            Err(Refusal::Changed { pid: 500 })
        );
        assert_eq!(
            *host.log.borrow(),
            ["holds_port", "check", "intend SIGTERM", "watch", "liveness"]
        );
    }

    #[test]
    fn each_signal_comes_after_the_gate_the_audit_the_watch_and_the_identity() {
        let host = Fake {
            ignores_term: true,
            ..Fake::default()
        };
        let out = stop_fake(&host, &fake_target(true), true).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGKILL"),
            "{out:?}"
        );
        assert_eq!(
            *host.log.borrow(),
            [
                "holds_port",
                "check",
                "intend SIGTERM",
                "watch",
                "liveness",
                "SIGTERM",
                "liveness",
                "check",
                "intend SIGKILL",
                "watch",
                "liveness",
                "SIGKILL",
                "liveness"
            ]
        );
    }

    #[test]
    fn a_process_that_exits_after_its_identity_is_read_is_not_signalled() {
        let host = Fake {
            exits_after_identity: true,
            ..Fake::default()
        };
        assert_eq!(
            stop_fake(&host, &fake_target(false), false),
            Err(Refusal::Changed { pid: 500 })
        );
        assert_eq!(
            *host.log.borrow(),
            ["check", "intend SIGTERM", "watch", "liveness"]
        );
    }

    #[test]
    fn a_gate_that_refuses_or_cannot_audit_sends_nothing() {
        struct No(Refusal, bool);
        impl Gate for No {
            fn check(&mut self, _: ProcId) -> Result<(), Refusal> {
                if self.1 { Err(self.0.clone()) } else { Ok(()) }
            }
            fn intend(&mut self, _: ProcId, _: &str) -> Result<(), Refusal> {
                Err(self.0.clone())
            }
        }
        for refusal in [
            Refusal::Disabled {
                config: "~/.config/scry/config.toml".into(),
                line: Some(3),
            },
            Refusal::Unaudited {
                error: "disk full".into(),
            },
        ] {
            for at_check in [true, false] {
                let host = Fake::default();
                let mut gate = No(refusal.clone(), at_check);
                let got = stop_on(&host, &fake_target(false), true, Duration::ZERO, &mut gate);
                assert_eq!(got, Err(refusal.clone()));
                assert!(host.log.borrow().is_empty(), "{:?}", host.log.borrow());
                let got = kill_on(&host, &fake_target(false), &mut gate);
                assert_eq!(got, Err(refusal.clone()));
                assert!(host.log.borrow().is_empty());
            }
        }
    }

    #[test]
    fn a_process_that_exits_just_before_sigkill_was_stopped_by_sigterm() {
        // Alive through the wait, gone at the check before SIGKILL.
        let host = Fake {
            ignores_term: true,
            exits_after: Cell::new(Some(2)),
            ..Fake::default()
        };
        let out = stop_fake(&host, &fake_target(false), true).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGTERM"),
            "{out:?}"
        );
        assert!(!host.log.borrow().contains(&"SIGKILL"));

        // Alive at that check, gone by the time SIGKILL is sent.
        let host = Fake {
            ignores_term: true,
            exits_after: Cell::new(Some(3)),
            ..Fake::default()
        };
        let out = stop_fake(&host, &fake_target(false), true).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGTERM"),
            "{out:?}"
        );
        assert_eq!(host.log.borrow().last(), Some(&"SIGKILL"));
    }

    /// `sleep 30` under `sh`, ignoring SIGTERM when asked. It returns once the shell has set
    /// its trap, which a fixed wait cannot promise on a busy machine.
    fn spawn(ignore_term: bool) -> (Child, StopTarget) {
        let trap = if ignore_term { "trap '' TERM; " } else { "" };
        let mut child = Command::new("/bin/sh")
            .args(["-c", &format!("{trap}echo ready; exec sleep 30")])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let pid = child.id() as i32;
        let live = sys::liveness(pid).unwrap();
        let t = StopTarget {
            id: ProcId {
                pid,
                start_us: live.start_us,
            },
            process: "sleep".into(),
            worker_of: None,
            port: None,
        };
        (child, t)
    }

    #[test]
    fn a_reused_pid_is_refused_and_not_signalled() {
        let (mut child, mut t) = spawn(false);
        t.id.start_us += 1;
        assert_eq!(
            stop(&t, true, Duration::from_millis(200), &mut Open),
            Err(Refusal::Changed { pid: t.id.pid })
        );
        assert!(child.try_wait().unwrap().is_none(), "still running");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn sigterm_stops_a_process_and_reports_it() {
        let (mut child, t) = spawn(false);
        let out = stop(&t, false, TERM_GRACE, &mut Open).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGTERM"),
            "{out:?}"
        );
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn sigkill_comes_only_with_kill() {
        let (mut child, t) = spawn(true);
        let out = stop(&t, false, Duration::from_millis(300), &mut Open).unwrap();
        assert!(matches!(out, Outcome::StillRunning { .. }), "{out:?}");
        assert!(child.try_wait().unwrap().is_none());
        let out = stop(&t, true, Duration::from_millis(300), &mut Open).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGKILL"),
            "{out:?}"
        );
        child.wait().unwrap();
    }

    #[test]
    fn a_port_target_must_still_hold_its_port() {
        let (mut child, mut t) = spawn(false);
        t.port = Some((Proto::Tcp, 1));
        assert_eq!(
            stop(&t, false, TERM_GRACE, &mut Open),
            Err(Refusal::NotHolding {
                pid: t.id.pid,
                port: ":1".into()
            })
        );
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
