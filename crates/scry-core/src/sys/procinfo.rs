//! Arguments, environment stamps and working directory of the viewer's own processes. The
//! kernel refuses these for other users without root.

use std::mem::{MaybeUninit, size_of};

use crate::model::Stamps;

/// What `KERN_PROCARGS2` gives Scry: the argument vector, and the allowlisted environment keys.
/// The rest of the environment is skipped while parsing and never copied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcArgs {
    pub argv: Vec<String>,
    pub stamps: Stamps,
}

/// Parses a `KERN_PROCARGS2` buffer: `int argc`, the exec path, NUL padding, `argc`
/// arguments, then environment entries up to an empty string. Apple's own strings follow and
/// are ignored.
pub fn parse_procargs2(buf: &[u8]) -> Option<ProcArgs> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    if argc < 0 {
        return None;
    }
    let mut rest = &buf[4..];
    let exec_end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[exec_end..];
    let start = rest.iter().position(|&b| b != 0).unwrap_or(rest.len());
    rest = &rest[start..];
    let mut strings = rest.split(|&b| b == 0);
    let mut argv = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        argv.push(String::from_utf8_lossy(strings.next()?).into_owned());
    }
    let mut stamps = Stamps::default();
    for entry in strings.take_while(|e| !e.is_empty()) {
        stamps.take(entry);
    }
    Some(ProcArgs { argv, stamps })
}

/// Reads arguments and stamps through one reusable buffer of `kern.argmax` bytes.
pub struct ArgsReader {
    buf: Vec<u8>,
}

impl Default for ArgsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl ArgsReader {
    pub fn new() -> Self {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut argmax: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>();
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut argmax as *mut libc::c_int).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        let size = if rc == 0 && argmax > 0 {
            argmax as usize
        } else {
            1 << 20
        };
        ArgsReader { buf: vec![0; size] }
    }

    pub fn read(&mut self, pid: i32) -> Option<ProcArgs> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut len = self.buf.len();
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                self.buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        parse_procargs2(&self.buf[..len])
    }
}

/// The current working directory, through `PROC_PIDVNODEPATHINFO`.
pub fn cwd(pid: i32) -> Option<String> {
    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let path = info.pvi_cdir.vip_path.as_flattened();
    let bytes: Vec<u8> = path
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procargs(argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut b = (argv.len() as i32).to_ne_bytes().to_vec();
        b.extend_from_slice(b"/usr/local/bin/tool\0\0\0\0");
        for a in argv {
            b.extend_from_slice(a.as_bytes());
            b.push(0);
        }
        for e in env {
            b.extend_from_slice(e.as_bytes());
            b.push(0);
        }
        b.push(0);
        b.extend_from_slice(b"executable_path=/usr/local/bin/tool\0ptr_munge=\0");
        b
    }

    #[test]
    fn keeps_argv_and_only_allowlisted_environment_keys() {
        let buf = procargs(
            &["node", "server.js", "--port", "3000"],
            &[
                "PATH=/usr/bin",
                "GITHUB_TOKEN=ghp_secret",
                "CLAUDE_CODE_SESSION_ID=7f3a",
                "CLAUDE_PID=1311",
                "CODEX_SESSION_ID=c-9",
                "SCRY_SESSION=s-1",
                "CLAUDE_PIDX=nope",
            ],
        );
        let got = parse_procargs2(&buf).unwrap();
        assert_eq!(got.argv, ["node", "server.js", "--port", "3000"]);
        assert_eq!(
            got.stamps,
            Stamps {
                claude_code_session_id: Some("7f3a".into()),
                claude_pid: Some(1311),
                codex_session_id: Some("c-9".into()),
                scry_session: Some("s-1".into()),
            }
        );
        assert!(!format!("{got:?}").contains("ghp_secret"));
    }

    #[test]
    fn stamp_values_of_any_other_shape_are_dropped() {
        let uuid = "7f3a9c2e-1b4d-4e8a-9c3f-2a6b8d0e1f47";
        let long = "a".repeat(65);
        let buf = procargs(
            &["x"],
            &[
                "SCRY_SESSION=Authorization: Bearer hunter2",
                &format!("CLAUDE_CODE_SESSION_ID={uuid}"),
                &format!("CODEX_SESSION_ID={long}"),
                "CLAUDE_PID=+1311",
            ],
        );
        let got = parse_procargs2(&buf).unwrap().stamps;
        assert_eq!(
            got,
            Stamps {
                claude_code_session_id: Some(uuid.into()),
                ..Stamps::default()
            }
        );
        for bad in [
            "SCRY_SESSION=",
            "SCRY_SESSION=a b",
            "SCRY_SESSION=x\u{1b}[2J",
            "SCRY_SESSION=../../etc",
            "CLAUDE_PID=-1",
            "CLAUDE_PID=1 2",
            "CLAUDE_PID=99999999999",
        ] {
            let got = parse_procargs2(&procargs(&["x"], &[bad])).unwrap().stamps;
            assert_eq!(got, Stamps::default(), "{bad}");
        }
        let ok = parse_procargs2(&procargs(
            &["x"],
            &["SCRY_SESSION=dev_1-A", "CLAUDE_PID=42"],
        ))
        .unwrap()
        .stamps;
        assert_eq!(ok.scry_session.as_deref(), Some("dev_1-A"));
        assert_eq!(ok.claude_pid, Some(42));
    }

    #[test]
    fn apple_strings_after_the_environment_are_not_read_as_stamps() {
        let mut buf = procargs(&["x"], &[]);
        buf.extend_from_slice(b"CLAUDE_PID=1\0");
        assert_eq!(parse_procargs2(&buf).unwrap().stamps, Stamps::default());
    }

    #[test]
    fn truncated_buffers_give_none() {
        assert_eq!(parse_procargs2(&[1, 0]), None);
        let buf = procargs(&["a", "b"], &[]);
        assert_eq!(parse_procargs2(&buf[..buf.len() - 60]), None);
    }

    #[test]
    fn reads_this_test_process() {
        let me = std::process::id() as i32;
        let got = ArgsReader::new().read(me).expect("own args readable");
        assert!(!got.argv.is_empty());
        let here = std::env::current_dir().unwrap();
        assert_eq!(cwd(me).as_deref(), here.to_str());
    }

    #[test]
    fn other_users_processes_are_unreadable_without_root() {
        if unsafe { libc::getuid() } == 0 {
            return;
        }
        assert_eq!(ArgsReader::new().read(1), None);
    }
}
