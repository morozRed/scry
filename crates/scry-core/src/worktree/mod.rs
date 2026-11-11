//! Linked git worktrees under the code roots and agent folders: what deleting each one would
//! cost, how much space it holds, and the agent that probably made it.

mod creator;
mod discover;
#[cfg(test)]
mod tests;
mod verdict;
pub mod view;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::size::SubtreeSize;

pub use creator::{Agent, CreatorEvidence, CreatorSource, ProbableCreator};
pub use discover::{ScanOptions, measure, scan};
pub use verdict::{KeptBy, Prunable, Safe};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    /// The repository's main checkout, or its git folder when there is no checkout or the
    /// worktree's link to it is broken.
    pub repo: PathBuf,
    pub head: Head,
    pub state: WorktreeState,
    pub lock: Option<Lock>,
    /// None until `measure` runs.
    pub size: Option<SubtreeSize>,
    pub creator: ProbableCreator,
    /// When git registered it: the birth time of its record under `.git/worktrees`.
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
    /// `oid` is None when the branch has no commit.
    Branch {
        name: String,
        oid: Option<String>,
    },
    Detached {
        oid: String,
    },
    Unknown,
}

/// What deleting a worktree would cost. Only `verdict` can build the `Safe` and `Prunable`
/// that `CanGo` and `CanPrune` carry, and only after every check passed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeState {
    /// Deleting the folder loses nothing.
    CanGo(Safe),
    /// The folder is gone; pruning removes only git's record of it.
    CanPrune(Prunable),
    /// Looks stale, but deleting it could lose work. Each concern says what to check.
    NeedsLook {
        concerns: Vec<Concern>,
        facts: Option<GitFacts>,
    },
    /// Has uncommitted changes.
    Dirty(GitFacts),
    /// Clean and locked: git keeps it until it is unlocked.
    Locked(GitFacts),
    /// Clean, with commits that are not merged or not all pushed, and nothing suggests it is
    /// stale.
    Active(GitFacts),
    /// Not read, so nothing is claimed about it.
    NotRead(NotRead),
}

impl WorktreeState {
    pub fn facts(&self) -> Option<&GitFacts> {
        match self {
            WorktreeState::CanGo(safe) => Some(safe.facts()),
            WorktreeState::NeedsLook { facts, .. } => facts.as_ref(),
            WorktreeState::Dirty(f) | WorktreeState::Locked(f) | WorktreeState::Active(f) => {
                Some(f)
            }
            WorktreeState::CanPrune(_) | WorktreeState::NotRead(_) => None,
        }
    }
}

/// What one `git status` and the repo's refs say about a worktree whose folder git can use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitFacts {
    pub changes: Changes,
    pub merge: Merge,
    pub upstream: Upstream,
    /// Stash entries made on its branch, or on a detached HEAD at its commit.
    pub stashes: u32,
    /// Commits at HEAD that no other branch, remote branch or tag holds. Counted only where
    /// it is evidence: for a branch whose upstream is gone.
    pub only_here: Option<u32>,
    /// Committer time of HEAD, Unix seconds.
    pub last_commit: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Changes {
    Clean,
    /// Entries `git status` lists: tracked ones changed, staged or unmerged, submodules
    /// included, and untracked ones, where an untracked folder is one entry.
    Dirty {
        changed: u32,
        untracked: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Merge {
    /// HEAD is reachable from the default branch, so it adds no commit of its own.
    Merged { into: String },
    /// HEAD has commits that the default branch lacks.
    Unmerged { into: String },
    /// HEAD has no commit yet.
    Unborn,
    /// The repo has no `origin/HEAD`, `main` or `master` to compare with.
    NoDefaultBranch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Upstream {
    None,
    /// A remote branch that still exists.
    Tracking {
        name: String,
        ahead: u32,
        behind: u32,
    },
    /// A local branch, which says nothing about what is pushed.
    Local {
        name: String,
    },
    /// Configured, but the branch is gone. A squash merge and a pull request closed without
    /// merging look the same from here, so this is never read as merged.
    Gone {
        name: String,
    },
}

/// Why a worktree that looks stale needs a look before anyone deletes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Concern {
    UpstreamGone {
        upstream: String,
    },
    /// Commits that no other branch, remote branch or tag holds.
    OnlyHere {
        commits: u32,
    },
    Stashed {
        entries: u32,
    },
    Locked {
        reason: Option<String>,
    },
    /// Registered, but the folder is gone.
    FolderMissing,
    /// Registered, but the folder has no `.git` link back to the repo.
    NoGitLink,
    /// The folder's `.git` link points at some other record.
    LinkElsewhere {
        gitdir: PathBuf,
    },
    /// Not registered anywhere: the folder's `.git` link points at a git folder that is gone.
    BrokenLink {
        gitdir: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NotRead {
    /// It, or its repository, is inside Desktop, Documents or Downloads.
    ProtectedFolder { folder: PathBuf },
    /// git or the file system failed on it.
    Failed { error: String },
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
    /// Repositories read: those found under the roots and agent folders, and those their
    /// worktrees lead to.
    pub repos: usize,
    /// Problems that did not stop the scan, such as a root inside a protected folder.
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
