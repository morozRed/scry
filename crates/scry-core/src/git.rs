//! Runs the `git` CLI and parses what it prints into types, and reads the small files git
//! keeps about worktrees. Nothing here writes to a repository.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::protected::read_no_follow;

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `git -C dir args…` in a process group of its own and kills that group once git exits,
/// so the helpers git starts die with it. A child that moves to another group escapes that
/// kill: if it holds git's output open, the call waits for it until `timeout`, then returns
/// an error and leaves it running.
pub fn run<S: AsRef<OsStr>>(dir: &Path, args: &[S], timeout: Duration) -> Result<Output> {
    let shown = || {
        let words: Vec<_> = args.iter().map(|a| a.as_ref().to_string_lossy()).collect();
        words.join(" ")
    };
    let mut cmd = Command::new("git");
    // Inside a git hook, GIT_DIR, GIT_INDEX_FILE and the like would aim every call at the
    // hook's repository.
    for (key, _) in std::env::vars_os() {
        if key.as_bytes().starts_with(b"GIT_") {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd
        .arg("-C")
        .arg(dir)
        // A monitor must not start an fsmonitor daemon or rewrite the index under an agent.
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so everything git starts can be killed with it.
        .process_group(0)
        .spawn()
        .context("running git")?;
    let pid = child.id() as i32;
    let pipes: [OwnedFd; 2] = [
        child.stdout.take().expect("piped").into(),
        child.stderr.take().expect("piped").into(),
    ];
    let read = collect(pid, &pipes, Instant::now() + timeout);
    // git is not reaped yet, so its pid still names its group and cannot have been reused.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let status = child.wait()?;
    let [stdout, stderr] = read.map_err(|e| match e.kind() {
        io::ErrorKind::TimedOut => anyhow!("git {} timed out after {timeout:?}", shown()),
        _ => anyhow!("git {}: {e}", shown()),
    })?;
    let text = |b: Vec<u8>| String::from_utf8_lossy(&b).into_owned();
    Ok(Output {
        code: status.code(),
        stdout: text(stdout),
        stderr: text(stderr),
    })
}

/// Reads both pipes until they close and git has exited, sleeping in `kevent` between reads.
/// Once git has exited, anything still holding a pipe is a child it left behind, so the group
/// is killed and the rest is read at once.
fn collect(pid: i32, pipes: &[OwnedFd; 2], deadline: Instant) -> io::Result<[Vec<u8>; 2]> {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    let change = |ident: usize, filter, flags, fflags| libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let apply = |ev: libc::kevent| {
        let n = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    for p in pipes {
        apply(change(
            p.as_raw_fd() as usize,
            libc::EVFILT_READ,
            libc::EV_ADD,
            0,
        ))?;
    }
    let mut exited = match apply(change(
        pid as usize,
        libc::EVFILT_PROC,
        libc::EV_ADD | libc::EV_ONESHOT,
        libc::NOTE_EXIT,
    )) {
        Ok(()) => false,
        // It exited before it could be watched.
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => true,
        Err(e) => return Err(e),
    };
    let mut open = [true, true];
    let mut out = [Vec::new(), Vec::new()];
    let mut buf = vec![0u8; 64 * 1024];
    while open[0] || open[1] || !exited {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let ts = libc::timespec {
            tv_sec: left.as_secs() as libc::time_t,
            tv_nsec: left.subsec_nanos() as libc::c_long,
        };
        let mut events = [change(0, 0, 0, 0); 3];
        let n = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                3,
                &ts,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        for ev in &events[..n as usize] {
            if ev.filter == libc::EVFILT_PROC {
                exited = true;
                if open[0] || open[1] {
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
                continue;
            }
            let fd = ev.ident as RawFd;
            let Some(i) = pipes.iter().position(|p| p.as_raw_fd() == fd) else {
                continue;
            };
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            match n {
                1.. => out[i].extend_from_slice(&buf[..n as usize]),
                0 => {
                    open[i] = false;
                    apply(change(fd as usize, libc::EVFILT_READ, libc::EV_DELETE, 0))?;
                }
                _ => {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Runs git and returns its stdout, or an error carrying git's first line of complaint.
pub fn stdout<S: AsRef<OsStr>>(dir: &Path, args: &[S], timeout: Duration) -> Result<String> {
    let out = run(dir, args, timeout)?;
    if out.code != Some(0) {
        let why = out.stderr.lines().next().unwrap_or("no message").trim();
        let words: Vec<_> = args.iter().map(|a| a.as_ref().to_string_lossy()).collect();
        bail!("git {} failed: {why}", words.join(" "));
    }
    Ok(out.stdout)
}

/// `--git-dir=<record> --work-tree=<folder>`, which aim a command at one linked worktree. Set
/// this way, git reads neither the folder's `.git` file nor a `core.worktree` that would
/// send it to another folder.
pub fn worktree_args(record: &Path, folder: &Path) -> [OsString; 2] {
    let arg = |flag: &str, p: &Path| {
        let mut a = OsString::from(flag);
        a.push(p.as_os_str());
        a
    };
    [arg("--git-dir=", record), arg("--work-tree=", folder)]
}

/// The one `git status` call made per worktree. Flags override any config that would hide an
/// untracked file or a change inside a submodule, and HEAD comes from the same call.
pub const STATUS: [&str; 6] = [
    "status",
    "--porcelain=v2",
    "-z",
    "--branch",
    "--untracked-files=normal",
    "--ignore-submodules=none",
];

/// Lists the index with each entry's flags and mode, for `parse_index`.
pub const LS_FILES: [&str; 4] = ["ls-files", "-s", "-v", "-z"];

/// What a worktree's index holds beyond what `git status` reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexFlags {
    /// Paths flagged assume-unchanged or skip-worktree. `git status` does not compare them
    /// with the folder, so an edit to one is invisible to it.
    pub hidden: u32,
    /// Paths of submodule entries (gitlinks).
    pub gitlinks: Vec<String>,
}

/// Parses the output of `LS_FILES`: `<tag> <mode> <oid> <stage>\t<path>`, where tag `S` marks
/// skip-worktree and a lowercase tag marks assume-unchanged.
pub fn parse_index(z: &str) -> Result<IndexFlags> {
    let mut out = IndexFlags::default();
    let mut last_hidden: Option<&str> = None;
    for record in z.split('\0').filter(|r| !r.is_empty()) {
        let (meta, path) = record
            .split_once('\t')
            .ok_or_else(|| anyhow!("unexpected git ls-files record {record:?}"))?;
        let mut f = meta.split(' ');
        let (Some(tag), Some(mode), Some(_oid), Some(stage)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            bail!("unexpected git ls-files record {record:?}");
        };
        let flagged = tag == "S" || tag.bytes().all(|b| b.is_ascii_lowercase());
        // An unmerged path has one entry per stage; count it once.
        if flagged && last_hidden != Some(path) {
            out.hidden += 1;
            last_hidden = Some(path);
        }
        if mode == "160000" && stage == "0" {
            out.gitlinks.push(path.to_string());
        }
    }
    Ok(out)
}

/// What one `git status` call says about a worktree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// HEAD's commit; None before the first commit.
    pub oid: Option<String>,
    /// None when HEAD is detached.
    pub branch: Option<String>,
    /// The upstream's short name, as git prints it.
    pub upstream: Option<String>,
    /// Commits ahead of and behind the upstream; None when the upstream ref is gone.
    pub ahead_behind: Option<(u32, u32)>,
    /// Tracked entries changed, staged or unmerged, submodules included.
    pub changed: u32,
    /// Untracked entries. An untracked folder is one entry.
    pub untracked: u32,
}

/// Parses the output of `STATUS`.
pub fn parse_status(z: &str) -> Result<Status> {
    let mut s = Status::default();
    let mut has_oid = false;
    let mut fields = z.split('\0');
    while let Some(field) = fields.next() {
        if let Some(header) = field.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => {
                    has_oid = true;
                    s.oid = (value != "(initial)").then(|| value.into());
                }
                "branch.head" => s.branch = (value != "(detached)").then(|| value.into()),
                "branch.upstream" => s.upstream = Some(value.into()),
                "branch.ab" => {
                    let (a, b) = value.split_once(' ').unwrap_or_default();
                    let num = |n: &str, sign| n.strip_prefix(sign)?.parse().ok();
                    s.ahead_behind = num(a, '+').zip(num(b, '-'));
                }
                _ => {}
            }
            continue;
        }
        match field.as_bytes().first() {
            None | Some(b'!') => {}
            Some(b'1' | b'u') => s.changed += 1,
            // A rename or copy carries its original path in the next field.
            Some(b'2') => {
                s.changed += 1;
                fields.next();
            }
            Some(b'?') => s.untracked += 1,
            Some(_) => bail!("unexpected git status record {field:?}"),
        }
    }
    if !has_oid {
        bail!("git status printed no branch.oid");
    }
    Ok(s)
}

/// The `for-each-ref` format that `parse_refs` reads. Ref names cannot hold a tab or a
/// newline, so both are safe separators.
pub const REF_FORMAT: &str =
    "--format=%(refname)%09%(objectname)%09%(upstream)%09%(upstream:track)%09%(symref)";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ref {
    /// Full name, such as `refs/heads/main`.
    pub name: String,
    pub oid: String,
    pub upstream: Option<String>,
    /// The upstream is configured but its remote-tracking ref no longer exists.
    pub upstream_gone: bool,
    /// The target of a symbolic ref, such as `refs/remotes/origin/main` for `origin/HEAD`.
    pub symref: Option<String>,
}

pub fn parse_refs(text: &str) -> Vec<Ref> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [name, oid, upstream, track, symref] = f.as_slice() else {
                return None;
            };
            let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
            Some(Ref {
                name: name.to_string(),
                oid: oid.to_string(),
                upstream: some(upstream),
                upstream_gone: *track == "[gone]",
                symref: some(symref),
            })
        })
        .collect()
}

/// The arguments that list stash entries for `parse_stashes`, one record per NUL. With no
/// stash they print nothing, so they can run whether or not one existed earlier in a scan.
pub const STASH_LIST: [&str; 6] = [
    "log",
    "-z",
    "-g",
    "--format=%P%x09%gs",
    "--ignore-missing",
    "refs/stash",
];

/// One stash entry: the commit it was made on and the branch its message names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    pub base: String,
    /// None when it was made on a detached HEAD or its message is not git's own.
    pub branch: Option<String>,
}

pub fn parse_stashes(z: &str) -> Vec<Stash> {
    z.split('\0')
        .filter_map(|record| {
            let (parents, subject) = record.trim_start_matches('\n').split_once('\t')?;
            let base = parents.split(' ').next()?.to_string();
            // git writes "WIP on <branch>: …" or "On <branch>: …", and a branch name cannot
            // hold a colon.
            let branch = ["WIP on ", "On "]
                .iter()
                .find_map(|p| subject.strip_prefix(p))
                .and_then(|rest| rest.split_once(':'))
                .map(|(b, _)| b)
                .filter(|b| *b != "(no branch)")
                .map(Into::into);
            Some(Stash { base, branch })
        })
        .collect()
}

/// The arguments that list every entry of one config file for `parse_config`, one per NUL.
/// `include.path` and `includeIf` are listed as keys, not followed.
pub fn config_list_args(file: &Path) -> [&OsStr; 6] {
    [
        "config".as_ref(),
        "--file".as_ref(),
        file.as_os_str(),
        "--no-includes".as_ref(),
        "--list".as_ref(),
        "-z".as_ref(),
    ]
}

/// Parses the output of `config_list_args`: `<key>\n<value>` per entry, or `<key>` alone for
/// a key written without `=`. git prints section and variable names in lowercase.
pub fn parse_config(z: &str) -> Vec<(String, Option<String>)> {
    z.split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once('\n') {
            Some((key, value)) => (key.to_string(), Some(value.to_string())),
            None => (entry.to_string(), None),
        })
        .collect()
}

/// The target of a `.git` file, `gitdir: <path>`, relative to the file's folder unless
/// absolute. Like git, only line endings are trimmed, so a path may hold any other byte. A
/// symlink in place of the file is not followed.
pub fn read_gitdir_file(dot_git: &Path) -> io::Result<PathBuf> {
    let mut text = read_no_follow(dot_git)?;
    while text.last().is_some_and(|b| matches!(b, b'\n' | b'\r')) {
        text.pop();
    }
    let Some(target) = text.strip_prefix(b"gitdir: ") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a gitdir file",
        ));
    };
    let target = Path::new(OsStr::from_bytes(target));
    Ok(match dot_git.parent() {
        Some(dir) if target.is_relative() => dir.join(target),
        _ => target.to_path_buf(),
    })
}

/// A path git stores in a file of a worktree's record, such as `gitdir` or `commondir`, with
/// trailing whitespace trimmed as git does. It may be relative to the record. None when the
/// file is absent or empty; an error when it is there but cannot be read, a symlink included.
pub fn read_path_file(file: &Path) -> io::Result<Option<PathBuf>> {
    let mut text = match read_no_follow(file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    while text.last().is_some_and(u8::is_ascii_whitespace) {
        text.pop();
    }
    Ok((!text.is_empty()).then(|| PathBuf::from(OsString::from_vec(text))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_v2_with_renames_untracked_folders_and_newlines() {
        let z = "# branch.oid 1111111111111111111111111111111111111111\0# branch.head feat\0\
                 # branch.upstream origin/feat\0# branch.ab +2 -5\0# stash 3\0\
                 1 .M N... 100644 100644 100644 aaa aaa README\0\
                 2 R. N... 100644 100644 100644 aaa aaa R100 new\nname\0old\0\
                 1 .M S.M. 160000 160000 160000 bbb bbb vendor/lib\0\
                 u UU N... 100644 100644 100644 100644 ccc ccc ccc both\0\
                 ? notes.txt\0? newdir/\0";
        let s = parse_status(z).unwrap();
        assert_eq!(
            s.oid.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(s.branch.as_deref(), Some("feat"));
        assert_eq!(s.upstream.as_deref(), Some("origin/feat"));
        assert_eq!(s.ahead_behind, Some((2, 5)));
        assert_eq!((s.changed, s.untracked), (4, 2));

        let gone = "# branch.oid abc\0# branch.head feat\0# branch.upstream origin/feat\0";
        assert_eq!(
            parse_status(gone).unwrap().ahead_behind,
            None,
            "upstream gone"
        );
        let detached = "# branch.oid (initial)\0# branch.head (detached)\0";
        let s = parse_status(detached).unwrap();
        assert_eq!((s.oid, s.branch), (None, None));
        assert!(parse_status("? stray\0").is_err(), "no branch.oid header");
    }

    #[test]
    fn parses_refs_with_a_gone_upstream_and_a_symref() {
        let text = "refs/heads/a\taaa\trefs/remotes/origin/a\t[gone]\t\n\
                    refs/heads/b\tbbb\trefs/remotes/origin/b\t[ahead 2]\t\n\
                    refs/heads/c\tccc\t\t\t\n\
                    refs/remotes/origin/HEAD\tddd\t\t\trefs/remotes/origin/main\n";
        let r = parse_refs(text);
        assert!(r[0].upstream_gone);
        assert_eq!(r[0].upstream.as_deref(), Some("refs/remotes/origin/a"));
        assert!(!r[1].upstream_gone);
        assert_eq!(r[1].oid, "bbb");
        assert_eq!(r[2].upstream, None);
        assert_eq!(r[3].symref.as_deref(), Some("refs/remotes/origin/main"));
    }

    #[test]
    fn parses_stash_entries_by_base_and_branch() {
        let z = "aaa bbb\tWIP on feat: aaa subject\0\nccc ddd eee\tOn main: saved\0\
                 fff ggg\tWIP on (no branch): fff x\0";
        assert_eq!(
            parse_stashes(z),
            [
                Stash {
                    base: "aaa".into(),
                    branch: Some("feat".into())
                },
                Stash {
                    base: "ccc".into(),
                    branch: Some("main".into())
                },
                Stash {
                    base: "fff".into(),
                    branch: None
                },
            ]
        );
    }

    #[test]
    fn parses_index_flags_and_gitlinks() {
        let z = "H 100644 aaa 0\t.gitmodules\0h 100644 bbb 0\tlocal.conf\0\
                 S 100644 ccc 0\tsparse/b.txt\0H 160000 ddd 0\tvendor/lib\0\
                 M 100644 eee 1\tboth\0M 100644 fff 2\tboth\0";
        assert_eq!(
            parse_index(z).unwrap(),
            IndexFlags {
                hidden: 2,
                gitlinks: vec!["vendor/lib".into()],
            }
        );
        assert!(parse_index("H 100644\tx\0").is_err());
        assert_eq!(parse_index("").unwrap(), IndexFlags::default());
    }

    #[test]
    fn lists_a_config_file_with_bare_keys_and_values_holding_newlines() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("config.worktree");
        std::fs::write(
            &file,
            "[core]\n\tuntrackedCache = true\n\tbare\n[Codex \"Sub\"]\n\tNote = \"a\\nb\"\n\
             [include]\n\tpath = /elsewhere\n",
        )
        .unwrap();
        let out = stdout(
            Path::new("/"),
            &config_list_args(&file),
            Duration::from_secs(5),
        );
        assert_eq!(
            parse_config(&out.unwrap()),
            [
                ("core.untrackedcache".into(), Some("true".into())),
                ("core.bare".into(), None),
                ("codex.Sub.note".into(), Some("a\nb".into())),
                ("include.path".into(), Some("/elsewhere".into())),
            ]
        );
        assert_eq!(parse_config(""), []);
    }

    #[test]
    fn reads_a_gitdir_file_whose_path_holds_a_newline() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join(".git");
        std::fs::write(&file, "gitdir: /x/odd\nrepo/.git/worktrees/a\n").unwrap();
        assert_eq!(
            read_gitdir_file(&file).unwrap(),
            PathBuf::from("/x/odd\nrepo/.git/worktrees/a")
        );
        std::fs::write(&file, "gitdir: ../.git/modules/lib\r\n").unwrap();
        assert_eq!(
            read_gitdir_file(&file).unwrap(),
            t.path().join("../.git/modules/lib")
        );
        std::fs::write(&file, "nonsense\n").unwrap();
        assert!(read_gitdir_file(&file).is_err());
    }

    #[test]
    fn does_not_follow_a_symlink_in_place_of_a_record_file() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("elsewhere");
        std::fs::write(&real, "gitdir: /x/.git/worktrees/a\n").unwrap();
        let link = t.path().join("gitdir");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(read_path_file(&link).is_err());
        assert!(read_gitdir_file(&link).is_err());
        assert_eq!(read_path_file(&t.path().join("absent")).unwrap(), None);
        assert_eq!(
            read_path_file(&real).unwrap(),
            Some(PathBuf::from("gitdir: /x/.git/worktrees/a"))
        );
    }

    #[test]
    fn kills_a_command_that_outlives_its_timeout_with_its_children() {
        let t = Instant::now();
        let err = run(
            Path::new("/"),
            &["-c", "alias.slow=!sleep 5", "slow"],
            Duration::from_millis(200),
        )
        .err()
        .expect("timed out");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn returns_when_git_exits_though_a_background_child_holds_its_pipes() {
        let t = Instant::now();
        let out = run(
            Path::new("/"),
            &["-c", "alias.bg=!sleep 20 & echo $!", "bg"],
            Duration::from_secs(10),
        )
        .expect("git exits at once");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        let pid: i32 = out.stdout.trim().parse().expect("the child's pid");
        // The child was in git's process group, so it went with it.
        let gone = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            !alive
        });
        assert!(gone, "pid {pid} still runs");
    }

    #[test]
    fn ends_at_the_deadline_when_a_child_outside_the_group_holds_the_pipes() {
        let t = Instant::now();
        // With job control on, sh starts the job in a group of its own, out of reach of the
        // group kill.
        let err = run(
            Path::new("/"),
            &["-c", "alias.away=!set -m; sleep 5 & echo started", "away"],
            Duration::from_millis(500),
        )
        .err()
        .expect("timed out");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }
}
