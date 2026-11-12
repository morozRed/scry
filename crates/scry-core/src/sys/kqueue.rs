//! Process exit and fork notices through kqueue `EVFILT_PROC`, and a user event to wake the
//! waiter. The daemon watches known agent processes with these, so it rescans an agent's
//! tree when the agent forks or exits instead of polling fast.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

/// What woke [`ProcWatch::wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    Exit(i32),
    Fork(i32),
    Stop,
    Timeout,
}

pub struct ProcWatch {
    kq: Arc<OwnedFd>,
    watched: HashSet<i32>,
}

/// Wakes the watch from another thread.
#[derive(Clone)]
pub struct Stopper {
    kq: Arc<OwnedFd>,
}

const STOP_IDENT: usize = 1;

fn event(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

impl ProcWatch {
    pub fn new() -> io::Result<ProcWatch> {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let kq = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
        let w = ProcWatch {
            kq,
            watched: HashSet::new(),
        };
        w.change(&[event(
            STOP_IDENT,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
        )])?;
        Ok(w)
    }

    pub fn stopper(&self) -> Stopper {
        Stopper {
            kq: self.kq.clone(),
        }
    }

    /// Watches these pids for exit and fork, and stops watching the ones not listed. A pid
    /// that is already gone is skipped.
    pub fn watch(&mut self, pids: &[i32]) {
        let want: HashSet<i32> = pids.iter().copied().collect();
        for &pid in self.watched.difference(&want) {
            let _ = self.change(&[event(pid as usize, libc::EVFILT_PROC, libc::EV_DELETE, 0)]);
        }
        self.watched.retain(|p| want.contains(p));
        for &pid in &want {
            if self.watched.contains(&pid) {
                continue;
            }
            let ev = event(
                pid as usize,
                libc::EVFILT_PROC,
                libc::EV_ADD | libc::EV_CLEAR,
                libc::NOTE_EXIT | libc::NOTE_FORK,
            );
            if self.change(&[ev]).is_ok() {
                self.watched.insert(pid);
            }
        }
    }

    pub fn watched(&self) -> usize {
        self.watched.len()
    }

    /// Waits for one notice, at most `timeout`.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<Wake> {
        let ts = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        let mut out = event(0, 0, 0, 0);
        let n = unsafe { libc::kevent(self.kq.as_raw_fd(), std::ptr::null(), 0, &mut out, 1, &ts) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(Wake::Timeout);
            }
            return Err(e);
        }
        if n == 0 {
            return Ok(Wake::Timeout);
        }
        if out.filter == libc::EVFILT_USER {
            return Ok(Wake::Stop);
        }
        let pid = out.ident as i32;
        if out.fflags & libc::NOTE_EXIT != 0 {
            // The kernel drops the filter with the process.
            self.watched.remove(&pid);
            return Ok(Wake::Exit(pid));
        }
        Ok(Wake::Fork(pid))
    }

    fn change(&self, changes: &[libc::kevent]) -> io::Result<()> {
        let rc = unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                changes.as_ptr(),
                changes.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// One process watched for its exit or an exec, so a signal can be bound to the process
/// that was checked. The kernel attaches the watch to the process that holds the pid when
/// it is made, not to the number: once that process exits the watch says so, even after
/// the pid is given to another. An exec keeps the pid and the start time but runs another
/// program, so the watch reports that too.
pub struct ChangeWatch {
    kq: OwnedFd,
    /// A notice was read already. The watch is one-shot, so it is not delivered twice.
    seen: std::cell::Cell<bool>,
}

impl ChangeWatch {
    /// Watches `pid` for its exit or an exec. Fails with `ESRCH` when no process has it.
    pub fn new(pid: i32) -> io::Result<ChangeWatch> {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let kq = unsafe { OwnedFd::from_raw_fd(fd) };
        let ev = event(
            pid as usize,
            libc::EVFILT_PROC,
            libc::EV_ADD | libc::EV_ONESHOT,
            libc::NOTE_EXIT | libc::NOTE_EXEC,
        );
        let rc = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ChangeWatch {
            kq,
            seen: std::cell::Cell::new(false),
        })
    }

    /// Whether the watched process has exited or exec'd since the watch was made. Does not
    /// wait. Once it says so, it keeps saying so.
    pub fn changed(&self) -> io::Result<bool> {
        if self.seen.get() {
            return Ok(true);
        }
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let mut out = event(0, 0, 0, 0);
        let n =
            unsafe { libc::kevent(self.kq.as_raw_fd(), std::ptr::null(), 0, &mut out, 1, &zero) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        self.seen.set(n > 0);
        Ok(n > 0)
    }
}

impl Stopper {
    pub fn stop(&self) {
        let ev = event(STOP_IDENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
        unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn reports_a_fork_then_the_exit_of_a_watched_process() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read go; /usr/bin/true; read go"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let mut w = ProcWatch::new().unwrap();
        w.watch(&[pid]);
        assert_eq!(w.watched(), 1);
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "go").unwrap();
        assert_eq!(w.wait(Duration::from_secs(5)).unwrap(), Wake::Fork(pid));
        writeln!(stdin, "go").unwrap();
        let wake = loop {
            match w.wait(Duration::from_secs(5)).unwrap() {
                Wake::Fork(_) => continue,
                other => break other,
            }
        };
        assert_eq!(wake, Wake::Exit(pid));
        assert_eq!(w.watched(), 0);
        child.wait().unwrap();
    }

    #[test]
    fn an_exit_watch_sees_the_exit_of_the_process_it_was_made_for() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read go"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let w = ChangeWatch::new(pid).unwrap();
        assert!(!w.changed().unwrap(), "still running");
        drop(child.stdin.take());
        child.wait().unwrap();
        // Reaped, so the pid is free for another process: the watch still says it exited.
        assert!(w.changed().unwrap());
        assert!(w.changed().unwrap(), "and keeps saying so");
        let gone = ChangeWatch::new(pid).map(|_| ()).unwrap_err();
        assert_eq!(gone.raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn a_change_watch_sees_an_exec_that_keeps_the_pid() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read go; exec /bin/sleep 30"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let w = ChangeWatch::new(pid).unwrap();
        assert!(!w.changed().unwrap(), "still the shell");
        use std::io::Write;
        writeln!(child.stdin.take().unwrap(), "go").unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !w.changed().unwrap() {
            assert!(std::time::Instant::now() < until, "the exec was never seen");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the pid runs on, as sleep"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_stopper_wakes_the_waiter_from_another_thread() {
        let mut w = ProcWatch::new().unwrap();
        let s = w.stopper();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            s.stop();
        });
        assert_eq!(w.wait(Duration::from_secs(5)).unwrap(), Wake::Stop);
        t.join().unwrap();
        assert_eq!(w.wait(Duration::from_millis(10)).unwrap(), Wake::Timeout);
    }
}
