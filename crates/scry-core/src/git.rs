//! Runs the `git` CLI, the source of truth for repositories and worktrees, and parses what it
//! prints into types. Nothing here writes to a repository.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `git -C dir args…`, killing it after `timeout`.
pub fn run(dir: &Path, args: &[&str], timeout: Duration) -> Result<Output> {
    let mut child = Command::new("git")
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
        // Its own process group, so a timeout also kills anything git started.
        .process_group(0)
        .spawn()
        .context("running git")?;
    let drain = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|s| Box::new(s) as _));
    let err = drain(child.stderr.take().map(|s| Box::new(s) as _));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // The child is not reaped yet, so its pid, which is also the group id, is ours.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            bail!("git {} timed out after {timeout:?}", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    Ok(Output {
        code: status.code(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Runs git and returns its stdout, or an error carrying git's first line of complaint.
pub fn stdout(dir: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let out = run(dir, args, timeout)?;
    if out.code != Some(0) {
        let why = out.stderr.lines().next().unwrap_or("no message").trim();
        bail!("git {} failed: {why}", args.join(" "));
    }
    Ok(out.stdout)
}

/// One record of `git worktree list --porcelain -z`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListedWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Short name, without `refs/heads/`.
    pub branch: Option<String>,
    pub bare: bool,
    /// `Some("")` when locked without a reason.
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

pub fn parse_worktree_list(z: &str) -> Vec<ListedWorktree> {
    let mut out = Vec::new();
    let mut cur: Option<ListedWorktree> = None;
    for field in z.split('\0') {
        if field.is_empty() {
            out.extend(cur.take());
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        if key == "worktree" {
            out.extend(cur.take());
            cur = Some(ListedWorktree {
                path: PathBuf::from(value),
                ..Default::default()
            });
            continue;
        }
        let Some(w) = cur.as_mut() else { continue };
        match key {
            "HEAD" => w.head = Some(value.to_string()),
            "branch" => {
                w.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).into());
            }
            "bare" => w.bare = true,
            "locked" => w.locked = Some(value.to_string()),
            "prunable" => w.prunable = Some(value.to_string()),
            _ => {}
        }
    }
    out.extend(cur);
    out
}

/// The `for-each-ref` format that `parse_refs` reads.
pub const REF_FORMAT: &str = "--format=%(refname)%09%(upstream)%09%(upstream:track)%09%(symref)";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ref {
    /// Full name, such as `refs/heads/main`.
    pub name: String,
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
            let [name, upstream, track, symref] = f.as_slice() else {
                return None;
            };
            let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
            Some(Ref {
                name: name.to_string(),
                upstream: some(upstream),
                upstream_gone: *track == "[gone]",
                symref: some(symref),
            })
        })
        .collect()
}

/// Counts of `git status --porcelain` lines: tracked changes, then untracked files.
pub fn count_status(text: &str) -> (u32, u32) {
    text.lines()
        .filter(|l| !l.is_empty())
        .fold((0, 0), |(changed, untracked), l| {
            if l.starts_with("??") {
                (changed, untracked + 1)
            } else {
                (changed + 1, untracked)
            }
        })
}

/// The target of a `.git` file: `gitdir: <path>`, relative to the file's folder if not absolute.
pub fn read_gitdir_file(dot_git: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dot_git).ok()?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        dot_git.parent()?.join(target)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_list_porcelain_z() {
        let z = "worktree /r\0HEAD aaa\0branch refs/heads/main\0\0\
                 worktree /r/.claude/worktrees/agent-1\0HEAD bbb\0branch refs/heads/feat/x\0locked claude agent agent-1 (pid 7)\0\0\
                 worktree /gone\0HEAD ccc\0detached\0prunable gitdir file points to non-existent location\0\0";
        let w = parse_worktree_list(z);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert_eq!(w[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(w[1].locked.as_deref(), Some("claude agent agent-1 (pid 7)"));
        assert_eq!(w[2].branch, None, "detached");
        assert!(w[2].prunable.is_some());
        assert_eq!(w[2].path, PathBuf::from("/gone"));
    }

    #[test]
    fn parses_refs_with_a_gone_upstream_and_a_symref() {
        let text = "refs/heads/a\trefs/remotes/origin/a\t[gone]\t\n\
                    refs/heads/b\trefs/remotes/origin/b\t[ahead 2]\t\n\
                    refs/heads/c\t\t\t\n\
                    refs/remotes/origin/HEAD\t\t\trefs/remotes/origin/main\n";
        let r = parse_refs(text);
        assert!(r[0].upstream_gone);
        assert_eq!(r[0].upstream.as_deref(), Some("refs/remotes/origin/a"));
        assert!(!r[1].upstream_gone);
        assert_eq!(r[2].upstream, None);
        assert_eq!(r[3].symref.as_deref(), Some("refs/remotes/origin/main"));
    }

    #[test]
    fn counts_changed_and_untracked_status_lines() {
        assert_eq!(count_status(""), (0, 0));
        assert_eq!(count_status(" M a\nA  b\nR  c -> d\n?? e\n?? f/\n"), (3, 2));
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
}
