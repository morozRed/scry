//! Linked git worktrees under the code roots and agent folders: their state, size and the
//! agent that probably made them.

mod creator;
mod discover;
#[cfg(test)]
mod tests;
pub mod view;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::size::SubtreeSize;

pub use creator::{Agent, CreatorEvidence, CreatorSource, ProbableCreator};
pub use discover::{ScanOptions, default_roots, measure, scan};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    /// The repository's main checkout, or its git folder when there is no checkout or the
    /// worktree's pointer is broken.
    pub repo: PathBuf,
    pub head: Head,
    pub state: WorktreeState,
    pub lock: Option<Lock>,
    /// None when the folder is gone.
    pub size: Option<SubtreeSize>,
    pub creator: ProbableCreator,
    /// When git registered it: the birth time of its folder under `.git/worktrees`.
    pub created_at: Option<i64>,
}

impl Worktree {
    /// The folder name, with its parent when the name alone would read as the repo's.
    pub fn name(&self) -> String {
        let leaf = file_name(&self.path);
        match self.path.parent() {
            Some(parent) if leaf == self.repo_name() => format!("{}/{leaf}", file_name(parent)),
            _ => leaf,
        }
    }

    pub fn repo_name(&self) -> String {
        // A git folder such as `/x/acme/.git` names the repo by its parent.
        let mut p = self.repo.as_path();
        while p
            .file_name()
            .is_some_and(|n| n == ".git" || n == "worktrees")
        {
            p = p.parent().unwrap_or(p);
        }
        file_name(p)
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Head {
    Branch { name: String, oid: String },
    Detached { oid: String },
    Unknown,
}

impl Head {
    pub fn oid(&self) -> Option<&str> {
        match self {
            Head::Branch { oid, .. } | Head::Detached { oid } => Some(oid),
            Head::Unknown => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeState {
    /// git cannot use it any more, but something is left behind.
    Orphaned(Orphan),
    /// git read it.
    Present(GitFacts),
    /// git failed or timed out on it.
    Unreadable { error: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Orphan {
    /// Registered under `.git/worktrees`, but the folder is gone.
    Missing {
        admin_dir: Option<PathBuf>,
        /// git's own `prunable` note, when it gave one.
        git_says: Option<String>,
    },
    /// Registered, and the folder is still there, but git cannot use it, usually because a
    /// half-finished removal took its `.git` file.
    Unusable {
        admin_dir: Option<PathBuf>,
        git_says: Option<String>,
    },
    /// The folder's `.git` file points at a git folder that is gone.
    BrokenPointer { gitdir: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitFacts {
    pub changes: Changes,
    pub merge: Merge,
    pub upstream: Upstream,
    /// Committer time of HEAD, Unix seconds.
    pub last_commit: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Changes {
    Clean,
    /// From `git status --porcelain`: tracked files changed or staged, and untracked files.
    Dirty {
        changed: u32,
        untracked: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Merge {
    /// HEAD is an ancestor of the default branch, so deleting it loses no commits.
    Merged { into: String },
    /// HEAD has commits that the default branch lacks.
    Unmerged { into: String },
    /// The repo has no `origin/HEAD`, `main` or `master` to compare with.
    NoDefaultBranch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Upstream {
    None,
    Tracking {
        name: String,
    },
    /// Configured, but the remote branch is gone, which usually means it was merged and
    /// deleted on the remote.
    Gone {
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Lock {
    pub reason: Option<String>,
}

/// What one scan found, with how long each step took.
#[derive(Clone, Debug, Serialize)]
pub struct WorktreeScan {
    pub worktrees: Vec<Worktree>,
    pub roots: Vec<PathBuf>,
    /// Repositories found under the roots and agent folders.
    pub repos: usize,
    /// Problems that did not stop the scan, such as a repo git could not list.
    pub warnings: Vec<String>,
    pub timings: Timings,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Timings {
    pub discover_ms: f64,
    pub git_ms: f64,
    pub size_ms: f64,
    pub total_ms: f64,
}
