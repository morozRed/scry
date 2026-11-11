//! Stopping a process. Every caller, human or agent, goes through the same checks: the
//! viewer's own processes only, never the system's, and the identity `(pid, start_time)` is
//! read again right before each signal, after every other check, so a reused pid is never
//! signalled.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::model::{ProcId, Process, Proto, Snapshot};
use crate::ports::{port_label, short_command};
use crate::sys::{self, Liveness};

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
    pub name: String,
    pub command: Option<String>,
    /// Set when the target was named by port. The pid must still hold it when signalled.
    pub port: Option<(Proto, u16)>,
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
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NotFound { target } => write!(f, "Nothing found for {target}."),
            Refusal::NotYours { pid, owner } => write!(
                f,
                "pid {pid} is owned by {owner}. Scry stops only your own processes."
            ),
            Refusal::Protected { pid, reason } => write!(f, "pid {pid} is protected: {reason}."),
            Refusal::Ambiguous { port, pids } => write!(
                f,
                ":{port} is held by several processes ({}). Stop one by pid.",
                pids.iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Refusal::Changed { pid } => write!(
                f,
                "pid {pid} is no longer the process that was chosen. Nothing was signalled."
            ),
            Refusal::NotHolding { pid, port } => write!(
                f,
                "pid {pid} no longer has {port} open. Nothing was signalled."
            ),
            Refusal::SignalFailed { pid, error } => {
                write!(f, "Signalling pid {pid} failed: {error}.")
            }
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
    Ok(StopTarget {
        id: p.id,
        name: short_command(p),
        command: p.argv.as_deref().map(crate::redact::join),
        port,
    })
}

fn guard(p: &Process, snap: &Snapshot) -> Result<(), Refusal> {
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
    if let Some(exe) = &p.exe
        && let Some(prefix) = SYSTEM_PREFIXES.iter().find(|s| exe.starts_with(*s))
    {
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
}

/// What stopping needs from the system. Tests stand in for it to show the order of the
/// checks and signals.
trait Host {
    fn liveness(&self, pid: i32) -> Option<Liveness>;
    fn holds_port(&self, pid: i32, proto: Proto, port: u16) -> Option<bool>;
    fn signal(&self, pid: i32, sig: libc::c_int) -> std::io::Result<()>;
}

struct Live;

impl Host for Live {
    fn liveness(&self, pid: i32) -> Option<Liveness> {
        sys::liveness(pid)
    }

    fn holds_port(&self, pid: i32, proto: Proto, port: u16) -> Option<bool> {
        sys::sockets::pid_holds_port(pid, proto, port)
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
/// after that gets SIGKILL, after its identity is checked once more.
pub fn stop(t: &StopTarget, kill: bool, grace: Duration) -> Result<Outcome, Refusal> {
    stop_on(&Live, t, kill, grace)
}

fn stop_on(
    host: &impl Host,
    t: &StopTarget,
    kill: bool,
    grace: Duration,
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
    // The port check reads the pid's open files, and the pid can be reused meanwhile, so the
    // identity is read after it, right before the signal.
    check_identity(t.id, host.liveness(t.id.pid))?;
    signal(host, t.id, libc::SIGTERM)?;
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
    // Gone between the wait and now, or as SIGKILL was sent: SIGTERM ended it.
    if check_identity(t.id, host.liveness(t.id.pid)).is_err() {
        return term_worked();
    }
    match signal(host, t.id, libc::SIGKILL) {
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

fn signal(host: &impl Host, id: ProcId, sig: libc::c_int) -> Result<(), Refusal> {
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
    use std::process::{Child, Command};

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
        assert_eq!(t.name, "next dev");
        assert_eq!(t.port, Some((Proto::Tcp, 3000)));
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

    fn fake_target(port: bool) -> StopTarget {
        StopTarget {
            id: ProcId {
                pid: 500,
                start_us: 0,
            },
            name: "server".into(),
            command: None,
            port: port.then_some((Proto::Tcp, 3000)),
        }
    }

    #[test]
    fn a_pid_reused_during_the_port_check_is_not_signalled() {
        let host = Fake {
            reused_as: Some(1),
            ..Fake::default()
        };
        assert_eq!(
            stop_on(&host, &fake_target(true), true, Duration::ZERO),
            Err(Refusal::Changed { pid: 500 })
        );
        assert_eq!(*host.log.borrow(), ["holds_port", "liveness"]);
    }

    #[test]
    fn each_signal_comes_right_after_an_identity_check() {
        let host = Fake {
            ignores_term: true,
            ..Fake::default()
        };
        let out = stop_on(&host, &fake_target(true), true, Duration::ZERO).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGKILL"),
            "{out:?}"
        );
        assert_eq!(
            *host.log.borrow(),
            [
                "holds_port",
                "liveness",
                "SIGTERM",
                "liveness",
                "liveness",
                "SIGKILL",
                "liveness"
            ]
        );
    }

    #[test]
    fn a_process_that_exits_just_before_sigkill_was_stopped_by_sigterm() {
        // Alive through the wait, gone at the check before SIGKILL.
        let host = Fake {
            ignores_term: true,
            exits_after: Cell::new(Some(2)),
            ..Fake::default()
        };
        let out = stop_on(&host, &fake_target(false), true, Duration::ZERO).unwrap();
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
        let out = stop_on(&host, &fake_target(false), true, Duration::ZERO).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGTERM"),
            "{out:?}"
        );
        assert_eq!(host.log.borrow().last(), Some(&"SIGKILL"));
    }

    fn spawn(script: &str) -> (Child, StopTarget) {
        let child = Command::new("/bin/sh")
            .args(["-c", script])
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(100));
        let live = sys::liveness(pid).unwrap();
        let t = StopTarget {
            id: ProcId {
                pid,
                start_us: live.start_us,
            },
            name: "sleep".into(),
            command: None,
            port: None,
        };
        (child, t)
    }

    #[test]
    fn a_reused_pid_is_refused_and_not_signalled() {
        let (mut child, mut t) = spawn("exec sleep 30");
        t.id.start_us += 1;
        assert_eq!(
            stop(&t, true, Duration::from_millis(200)),
            Err(Refusal::Changed { pid: t.id.pid })
        );
        assert!(child.try_wait().unwrap().is_none(), "still running");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn sigterm_stops_a_process_and_reports_it() {
        let (mut child, t) = spawn("exec sleep 30");
        let out = stop(&t, false, TERM_GRACE).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGTERM"),
            "{out:?}"
        );
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn sigkill_comes_only_with_kill() {
        let (mut child, t) = spawn("trap '' TERM; exec sleep 30");
        let out = stop(&t, false, Duration::from_millis(300)).unwrap();
        assert!(matches!(out, Outcome::StillRunning { .. }), "{out:?}");
        assert!(child.try_wait().unwrap().is_none());
        let out = stop(&t, true, Duration::from_millis(300)).unwrap();
        assert!(
            matches!(out, Outcome::Exited { ref signal, .. } if signal == "SIGKILL"),
            "{out:?}"
        );
        child.wait().unwrap();
    }

    #[test]
    fn a_port_target_must_still_hold_its_port() {
        let (mut child, mut t) = spawn("exec sleep 30");
        t.port = Some((Proto::Tcp, 1));
        assert_eq!(
            stop(&t, false, TERM_GRACE),
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
