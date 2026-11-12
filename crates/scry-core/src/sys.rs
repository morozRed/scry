//! Reads the live process table. Every call here works without root.

pub mod procinfo;
pub mod pulse;
pub mod sockets;

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::mem::{MaybeUninit, size_of};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use libc::{c_int, c_void};

use crate::model::{DiskSample, ProcId, Process, SNAPSHOT_SCHEMA, Snapshot};

// `struct kinfo_proc` from <sys/sysctl.h> on arm64. libc has no binding, so the fields Scry
// reads are taken by offset: `kp_proc.p_un.__p_starttime` (a timeval) opens the record and
// `kp_proc.p_pid` follows the vmspace and sigacts pointers, the flag word and the stat byte.
const KINFO_PROC_SIZE: usize = 648;
const KINFO_STARTTIME_OFFSET: usize = 0;
const KINFO_PID_OFFSET: usize = 40;
const KINFO_STAT_OFFSET: usize = 36;
const SZOMB: u8 = 5;

unsafe extern "C" {
    fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
}

pub fn scan() -> Result<Snapshot> {
    let starts = all_pids_with_start()?;
    let viewer_uid = unsafe { libc::getuid() };
    let read: Vec<Process> = starts
        .into_iter()
        .filter_map(|(pid, start_us)| read_process(ProcId { pid, start_us }))
        .collect();
    // A pid can exit and be reused while its details are read. Reading the table again
    // brackets those reads: a record whose pid is gone or restarted could mix two processes.
    let still: HashMap<i32, i64> = all_pids_with_start()?.into_iter().collect();
    let processes = read
        .into_iter()
        .filter(|p| still.get(&p.id.pid) == Some(&p.id.start_us))
        .collect();
    Ok(Snapshot {
        schema: SNAPSHOT_SCHEMA,
        taken_at_ms: now_ms(),
        viewer_uid,
        processes,
        ..Snapshot::default()
    })
}

fn all_pids_with_start() -> Result<Vec<(i32, i64)>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL];
    let mut buf: Vec<u8> = Vec::new();
    // The table can grow between the size probe and the read, so retry with headroom.
    for _ in 0..4 {
        let mut len: usize = 0;
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("sysctl kern.proc.all size");
        }
        len += len / 8;
        buf.resize(len, 0);
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            buf.truncate(len);
            return parse_kinfo(&buf);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return Err(std::io::Error::last_os_error()).context("sysctl kern.proc.all");
        }
    }
    bail!("sysctl kern.proc.all kept growing")
}

fn parse_kinfo(buf: &[u8]) -> Result<Vec<(i32, i64)>> {
    if !buf.len().is_multiple_of(KINFO_PROC_SIZE) {
        bail!(
            "kern.proc.all returned {} bytes, not a multiple of the {KINFO_PROC_SIZE}-byte kinfo_proc this build expects",
            buf.len()
        );
    }
    let (records, _) = buf.as_chunks::<KINFO_PROC_SIZE>();
    Ok(records.iter().map(parse_record).collect())
}

/// A live process's start time, and whether it has exited and waits to be reaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liveness {
    pub start_us: i64,
    pub zombie: bool,
}

/// Reads one pid's start time straight from the kernel, for the identity check before a signal.
pub fn liveness(pid: i32) -> Option<Liveness> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut rec = [0u8; KINFO_PROC_SIZE];
    let mut len = KINFO_PROC_SIZE;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            rec.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != KINFO_PROC_SIZE {
        return None;
    }
    let (got, start_us) = parse_record(&rec);
    (got == pid).then_some(Liveness {
        start_us,
        zombie: rec[KINFO_STAT_OFFSET] == SZOMB,
    })
}

fn parse_record(rec: &[u8; KINFO_PROC_SIZE]) -> (i32, i64) {
    let at = |o: usize, n: usize| &rec[o..o + n];
    let sec = i64::from_ne_bytes(at(KINFO_STARTTIME_OFFSET, 8).try_into().unwrap());
    let usec = i32::from_ne_bytes(at(KINFO_STARTTIME_OFFSET + 8, 4).try_into().unwrap());
    let pid = i32::from_ne_bytes(at(KINFO_PID_OFFSET, 4).try_into().unwrap());
    (pid, sec * 1_000_000 + i64::from(usec))
}

/// The identity of one live process, or `None` if no process has that pid.
pub fn proc_id(pid: i32) -> Option<ProcId> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buf = [0u8; KINFO_PROC_SIZE];
    let mut len = buf.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != KINFO_PROC_SIZE {
        return None;
    }
    let (found, start_us) = parse_record(&buf);
    (found == pid).then_some(ProcId { pid, start_us })
}

/// Reads total, free and available space of the volume holding `path`.
pub fn volume_space(path: &Path) -> Result<DiskSample> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let mut st = MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::statfs(c_path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statfs {}", path.display()));
    }
    let st = unsafe { st.assume_init() };
    let block = u64::from(st.f_bsize);
    let volume = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(DiskSample {
        at_ms: now_ms(),
        volume,
        total_bytes: st.f_blocks * block,
        free_bytes: st.f_bfree * block,
        avail_bytes: st.f_bavail * block,
    })
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn read_process(id: ProcId) -> Option<Process> {
    let info = short_bsd_info(id.pid)?;
    let exe = exe_path(id.pid);
    let footprint = footprint(id.pid);
    let comm: Vec<u8> = info
        .pbsi_comm
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    Some(Process {
        id,
        ppid: info.pbsi_ppid as i32,
        uid: info.pbsi_uid,
        name: String::from_utf8_lossy(&comm).into_owned(),
        exe,
        footprint,
        command: None,
        cwd: None,
        project: None,
        stamps: Default::default(),
        worker: false,
    })
}

fn short_bsd_info(pid: i32) -> Option<libc::proc_bsdshortinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = size_of::<libc::proc_bsdshortinfo>() as c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    (n == size).then(|| unsafe { info.assume_init() })
}

/// The executable `pid` runs, as the kernel resolved it.
pub fn exe_path(pid: i32) -> Option<String> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn footprint(pid: i32) -> Option<u64> {
    let mut info = MaybeUninit::<libc::rusage_info_v2>::zeroed();
    let rc = unsafe { proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast()) };
    (rc == 0).then(|| unsafe { info.assume_init() }.ri_phys_footprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_launchd_and_this_test_with_ps_start_times() {
        let snap = scan().expect("scan");
        let launchd = snap
            .processes
            .iter()
            .find(|p| p.id.pid == 1)
            .expect("launchd listed");
        assert_eq!(launchd.name, "launchd");
        assert_eq!(launchd.uid, 0, "root processes are visible without root");

        let me = std::process::id() as i32;
        let this = snap
            .processes
            .iter()
            .find(|p| p.id.pid == me)
            .expect("own process listed");
        assert!(this.footprint.is_some(), "own footprint readable");

        let ps = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &me.to_string()])
            .env("LC_ALL", "C")
            .output()
            .expect("ps");
        let ps_start = String::from_utf8_lossy(&ps.stdout).trim().to_string();
        let ours = std::process::Command::new("date")
            .args([
                "-r",
                &(this.id.start_us / 1_000_000).to_string(),
                "+%a %b %e %H:%M:%S %Y",
            ])
            .env("LC_ALL", "C")
            .output()
            .expect("date");
        assert_eq!(
            String::from_utf8_lossy(&ours.stdout).trim(),
            ps_start,
            "start time matches ps"
        );
    }

    #[test]
    fn proc_id_matches_the_full_scan_and_misses_a_dead_pid() {
        let me = std::process::id() as i32;
        let scanned = all_pids_with_start()
            .unwrap()
            .into_iter()
            .find(|(pid, _)| *pid == me)
            .unwrap();
        assert_eq!(
            proc_id(me),
            Some(ProcId {
                pid: me,
                start_us: scanned.1
            })
        );
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id() as i32;
        child.wait().unwrap();
        assert_eq!(proc_id(dead), None);
    }

    #[test]
    fn root_volume_has_space_figures_in_order() {
        let s = volume_space(Path::new("/")).unwrap();
        assert_eq!(s.volume, "/");
        assert!(s.total_bytes > 0);
        assert!(s.avail_bytes <= s.free_bytes && s.free_bytes <= s.total_bytes);
    }

    #[test]
    fn rejects_a_kinfo_layout_it_does_not_know() {
        assert!(parse_kinfo(&[0u8; KINFO_PROC_SIZE + 1]).is_err());
    }
}
