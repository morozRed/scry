//! The Worktrees view as plain data: one row per worktree with a status word, the sentence
//! that explains it and the evidence behind it. Skins only draw this.

use std::path::Path;

use serde::Serialize;

use super::{Changes, Head, Merge, Orphan, Upstream, Worktree, WorktreeScan, WorktreeState};
use crate::view::human_bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// git cannot use it any more; its folder or its record is left behind.
    Orphaned,
    /// Clean, unlocked, and merged into the default branch.
    Merged,
    /// Clean, unlocked, and its upstream branch is gone.
    Gone,
    /// Has changed or untracked files.
    Dirty,
    Locked,
    /// Clean, with commits the default branch lacks.
    Active,
    Unknown,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Orphaned => "orphaned",
            Status::Merged => "merged",
            Status::Gone => "gone",
            Status::Dirty => "dirty",
            Status::Locked => "locked",
            Status::Active => "active",
            Status::Unknown => "unknown",
        }
    }

    /// Deleting it loses no work.
    pub fn removable(self) -> bool {
        matches!(self, Status::Orphaned | Status::Merged | Status::Gone)
    }

    pub fn of(w: &Worktree) -> Status {
        let facts = match &w.state {
            WorktreeState::Orphaned(_) => return Status::Orphaned,
            WorktreeState::Unreadable { .. } => return Status::Unknown,
            WorktreeState::Present(f) => f,
        };
        if matches!(facts.changes, Changes::Dirty { .. }) {
            Status::Dirty
        } else if w.lock.is_some() {
            Status::Locked
        } else if matches!(facts.merge, Merge::Merged { .. }) {
            Status::Merged
        } else if matches!(facts.upstream, Upstream::Gone { .. }) {
            Status::Gone
        } else {
            Status::Active
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WorktreeRow {
    pub status: Status,
    pub name: String,
    pub repo: String,
    /// With the home folder written as `~`.
    pub path: String,
    pub branch: String,
    pub allocated: Option<u64>,
    pub reclaimable: Option<u64>,
    pub creator: String,
    /// Short age of HEAD's commit, such as `3d`.
    pub last_commit: Option<String>,
    pub sentence: String,
    pub evidence: Vec<String>,
    pub worktree: Worktree,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorktreesView {
    pub summary: String,
    pub rows: Vec<WorktreeRow>,
    /// Bytes freed by deleting every removable worktree.
    pub reclaimable: u64,
    /// Entries the size walk could not read, over all rows.
    pub skipped: u64,
}

impl WorktreesView {
    /// `now` is Unix seconds; `home` is written as `~` in paths.
    pub fn new(scan: &WorktreeScan, now: i64, home: &Path) -> Self {
        let mut rows: Vec<WorktreeRow> = scan.worktrees.iter().map(|w| row(w, now, home)).collect();
        rows.sort_by(|a, b| {
            (a.status, b.reclaimable, &a.repo, &a.name).cmp(&(
                b.status,
                a.reclaimable,
                &b.repo,
                &b.name,
            ))
        });
        let removable: Vec<&WorktreeRow> = rows.iter().filter(|r| r.status.removable()).collect();
        let reclaimable = removable.iter().filter_map(|r| r.reclaimable).sum();
        let skipped = scan
            .worktrees
            .iter()
            .filter_map(|w| w.size.as_ref())
            .map(|s| s.skipped)
            .sum();
        let mut repos: Vec<&str> = rows.iter().map(|r| r.repo.as_str()).collect();
        repos.sort_unstable();
        repos.dedup();
        let mut summary = match (rows.len(), removable.len()) {
            (0, _) => format!(
                "No linked worktrees in {} repos under {}.",
                scan.repos,
                list_paths(&scan.roots, home)
            ),
            (n, 0) => format!(
                "{} in {}; none is merged, gone or orphaned.",
                count(n, "worktree"),
                count(repos.len(), "repo")
            ),
            (n, k) if rows.iter().all(|r| r.reclaimable.is_none()) => format!(
                "{} in {}; {k} can go.",
                count(n, "worktree"),
                count(repos.len(), "repo")
            ),
            (n, k) => format!(
                "{} in {}; {k} can go and free {}.",
                count(n, "worktree"),
                count(repos.len(), "repo"),
                bytes(reclaimable)
            ),
        };
        if skipped > 0 {
            summary.push_str(&format!(
                " {} could not be read.",
                count(skipped as usize, "entry")
            ));
        }
        WorktreesView {
            summary,
            rows,
            reclaimable,
            skipped,
        }
    }
}

fn row(w: &Worktree, now: i64, home: &Path) -> WorktreeRow {
    let status = Status::of(w);
    let facts = match &w.state {
        WorktreeState::Present(f) => Some(f),
        _ => None,
    };
    WorktreeRow {
        status,
        name: w.name(),
        repo: w.repo_name(),
        path: tilde(&w.path, home),
        branch: match &w.head {
            Head::Branch { name, .. } => name.clone(),
            Head::Detached { oid } => format!("({})", short(oid)),
            Head::Unknown => String::new(),
        },
        allocated: w.size.as_ref().map(|s| s.size.allocated),
        reclaimable: w.size.as_ref().map(|s| s.size.reclaimable),
        creator: w.creator.agent.map_or("unknown", |a| a.label()).to_string(),
        last_commit: facts
            .and_then(|f| f.last_commit)
            .map(|t| short_age(now - t)),
        sentence: sentence(w, status, now, home),
        evidence: evidence(w, now, home),
        worktree: w.clone(),
    }
}

fn sentence(w: &Worktree, status: Status, now: i64, home: &Path) -> String {
    let name = w.name();
    let frees = match &w.size {
        Some(s) if s.error.is_none() => {
            format!("; deleting it frees {}", bytes(s.size.reclaimable))
        }
        _ => String::new(),
    };
    let prune = if w.lock.is_some() {
        "it is locked, so `git worktree prune` keeps the record"
    } else {
        "`git worktree prune` clears the record"
    };
    let repo = w.repo_name();
    let facts = match &w.state {
        WorktreeState::Orphaned(Orphan::Missing { .. }) => {
            return format!("{name} is registered in {repo} but its folder is gone; {prune}.");
        }
        WorktreeState::Orphaned(Orphan::Unusable { .. }) => {
            let folder = match &w.size {
                Some(s) if s.error.is_none() => {
                    format!(
                        ", and deleting the folder frees {}",
                        bytes(s.size.reclaimable)
                    )
                }
                _ => String::new(),
            };
            return format!(
                "{name} is registered in {repo} but git can no longer use its folder; {prune}{folder}."
            );
        }
        WorktreeState::Orphaned(Orphan::BrokenPointer { gitdir }) => {
            return format!(
                "{name}'s .git file points at {}, which is gone{frees}.",
                tilde(gitdir, home)
            );
        }
        WorktreeState::Unreadable { error } => {
            return format!("git could not read {name}: {error}.");
        }
        WorktreeState::Present(f) => f,
    };
    match (status, &facts.changes, &facts.merge, &facts.upstream) {
        (Status::Dirty, Changes::Dirty { changed, untracked }, merge, _) => {
            let what = changes_phrase(*changed, *untracked);
            let them = if changed + untracked == 1 {
                "it"
            } else {
                "them"
            };
            match merge {
                Merge::Merged { into } => format!(
                    "{name} is merged into {into} but has {what}; deleting it would lose {them}."
                ),
                _ => format!("{name} has {what}; deleting it would lose {them}."),
            }
        }
        (Status::Locked, ..) => {
            let by = match w.lock.as_ref().and_then(|l| l.reason.as_deref()) {
                Some(r) if r.starts_with("claude agent ") => match claude_lock_pid(r) {
                    Some(pid) => format!(" by a Claude Code agent (pid {pid})"),
                    None => " by a Claude Code agent".into(),
                },
                Some(r) => format!(" (\"{r}\")"),
                None => String::new(),
            };
            format!("{name} is locked{by}; git keeps it until it is unlocked.")
        }
        (Status::Merged, _, Merge::Merged { into }, _) => {
            format!("{name} is merged into {into} and clean{frees}.")
        }
        (Status::Gone, _, _, Upstream::Gone { name: up }) => {
            format!("{name} tracks {up}, which is gone, and is clean{frees}.")
        }
        (_, _, Merge::Unmerged { into }, _) => match facts.last_commit {
            Some(t) => format!(
                "{name} has commits not in {into}; its last commit was {}.",
                long_age(now - t)
            ),
            None => format!("{name} has commits not in {into}."),
        },
        _ => format!("{name} is clean, and its repo has no main branch to compare it with."),
    }
}

fn evidence(w: &Worktree, now: i64, home: &Path) -> Vec<String> {
    let mut out = vec![format!("path {}", tilde(&w.path, home))];
    match &w.head {
        Head::Branch { name, oid } => out.push(format!("branch {name} at {}", short(oid))),
        Head::Detached { oid } => out.push(format!("detached at {}", short(oid))),
        Head::Unknown => {}
    }
    match &w.state {
        WorktreeState::Orphaned(Orphan::Missing { git_says, .. }) => out.push(match git_says {
            Some(why) => format!("git worktree list: prunable, {why}"),
            None => "the folder does not exist".into(),
        }),
        WorktreeState::Orphaned(Orphan::Unusable { git_says, .. }) => out.push(match git_says {
            Some(why) => format!("git worktree list: prunable, {why}"),
            None => "the folder has no .git file".into(),
        }),
        WorktreeState::Orphaned(Orphan::BrokenPointer { gitdir }) => out.push(format!(
            ".git points at {}, which does not exist",
            tilde(gitdir, home)
        )),
        WorktreeState::Unreadable { error } => out.push(error.clone()),
        WorktreeState::Present(f) => {
            out.push(match f.changes {
                Changes::Clean => "git status: clean".into(),
                Changes::Dirty { changed, untracked } => {
                    format!("git status: {changed} changed, {untracked} untracked")
                }
            });
            out.push(match &f.merge {
                Merge::Merged { into } => format!("HEAD is in {into}"),
                Merge::Unmerged { into } => format!("HEAD has commits not in {into}"),
                Merge::NoDefaultBranch => "no origin/HEAD, main or master to compare with".into(),
            });
            match &f.upstream {
                Upstream::None => {}
                Upstream::Tracking { name } => out.push(format!("upstream {name}")),
                Upstream::Gone { name } => out.push(format!("upstream {name} is gone")),
            }
            if let Some(t) = f.last_commit {
                out.push(format!("last commit {}", long_age(now - t)));
            }
        }
    }
    if let Some(lock) = &w.lock {
        out.push(match &lock.reason {
            Some(r) => format!("locked: {r}"),
            None => "locked".into(),
        });
    }
    out.push(match w.creator.agent {
        Some(agent) => {
            let why: Vec<String> = w.creator.evidence.iter().map(|e| e.describe()).collect();
            format!("probably made by {}: {}", agent.label(), why.join("; "))
        }
        None => "made by an unknown tool: no agent's path or lock reason matched".into(),
    });
    if let Some(s) = &w.size {
        match &s.error {
            Some(e) => out.push(format!("not sized: {e}")),
            None => {
                let mut line = format!(
                    "{} on disk, {} freed at once by a delete",
                    bytes(s.size.allocated),
                    bytes(s.size.reclaimable)
                );
                if s.size.reclaimable < s.size.allocated {
                    line.push_str("; the rest is shared with clones, snapshots or hard links");
                }
                out.push(line);
                if s.skipped > 0 {
                    out.push(format!(
                        "{} could not be read",
                        count(s.skipped as usize, "entry")
                    ));
                }
            }
        }
    }
    if let Some(t) = w.created_at {
        out.push(format!("registered with git {}", long_age(now - t)));
    }
    out
}

/// Claude Code writes "claude agent <name> (pid <pid> start …)".
fn claude_lock_pid(reason: &str) -> Option<u32> {
    let rest = reason.split("(pid ").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn changes_phrase(changed: u32, untracked: u32) -> String {
    match (changed, untracked) {
        (c, 0) => count(c as usize, "changed file"),
        (0, u) => count(u as usize, "untracked file"),
        (c, u) => format!("{} and {u} untracked", count(c as usize, "changed file")),
    }
}

fn count(n: usize, noun: &str) -> String {
    match (n, noun) {
        (1, _) => format!("1 {noun}"),
        (_, "entry") => format!("{n} entries"),
        _ => format!("{n} {noun}s"),
    }
}

/// `1.2 GB`, from the same binary units as every size in Scry.
pub fn bytes(b: u64) -> String {
    let s = human_bytes(b);
    let (num, unit) = s.split_at(s.len() - 1);
    if unit == "B" {
        format!("{num} B")
    } else {
        format!("{num} {unit}B")
    }
}

pub fn short_age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        86_400..1_209_600 => format!("{}d", s / 86_400),
        1_209_600..31_536_000 => format!("{}w", s / 604_800),
        _ => format!("{}y", s / 31_536_000),
    }
}

fn long_age(secs: i64) -> String {
    let s = secs.max(0);
    let (n, unit) = match s {
        0..60 => return "just now".into(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        86_400..1_209_600 => (s / 86_400, "day"),
        1_209_600..31_536_000 => (s / 604_800, "week"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{} ago", count(n as usize, unit))
}

fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if home.as_os_str().len() > 1 => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}

fn list_paths(paths: &[std::path::PathBuf], home: &Path) -> String {
    let shown: Vec<String> = paths.iter().map(|p| tilde(p, home)).collect();
    if shown.is_empty() {
        "no roots".into()
    } else {
        shown.join(", ")
    }
}
