//! Linked git worktrees under the code roots and agent folders: what removing each one would
//! cost, how much space it holds, and the agent that probably made it.
//!
//! Two questions decide a worktree's label, and `verdict` keeps them apart:
//!
//! - **Loss: what does removing the folder destroy?** Working files and the worktree's own git
//!   state. Scry checks that `git status` lists nothing, that no file is hidden from it, that
//!   no submodule, other worktree or repository sits inside, that the worktree's record holds
//!   only git's standard files and the bookkeeping files agents are known to write there,
//!   that a detached HEAD is held by a local branch or tag, and that it is not locked. Ignored
//!   files are not checked: a local `.env` goes with the folder.
//! - **Finished: is its work done?** Asked only when nothing is lost: HEAD is in the default
//!   branch or on its upstream, as of the repo's last fetch. Scry never fetches.
//!
//! "Can go" needs both, and no stash entry made on the worktree: stash entries belong to the
//! repo and outlive it, but one may be work set aside and forgotten. Scry's cleanup is meant
//! to move a folder to the Trash rather than delete it, so anything these checks miss can
//! still be put back.

mod creator;
mod discover;
#[cfg(test)]
mod tests;
mod verdict;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::size::SubtreeSize;

pub use creator::{Agent, Clues, CreatorEvidence, CreatorSource, ProbableCreator};
pub use discover::{ScanOptions, measure, scan};
pub use verdict::{Finished, KeptBy, NothingLost, Prunable, Safe};

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

/// What removing a worktree would cost. Only `verdict` can build the `Safe` and `Prunable`
/// that `CanGo` and `CanPrune` carry, and only after every check passed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeState {
    /// Every loss check passed and its work is finished; see `Safe` for what that covers.
    /// Ignored files are not checked.
    CanGo(Safe),
    /// The folder is gone; pruning removes only git's record of it.
    CanPrune(Prunable),
    /// Removing it could lose something, or its work may not be finished. Each concern says
    /// what to check.
    NeedsLook {
        concerns: Vec<Concern>,
        facts: Option<GitFacts>,
    },
    /// Has uncommitted changes.
    Dirty(GitFacts),
    /// Clean and locked: git keeps it until it is unlocked.
    Locked(GitFacts),
    /// Clean and unlocked, with commits not merged or not all pushed, and nothing suggests it
    /// is stale. Anything else removing it would lose is in its facts.
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

/// What a look at a worktree whose folder git can use found: one `git status`, its index,
/// its record, the repo's refs and, once `measure` ran, the folders inside it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitFacts {
    pub changes: Changes,
    /// Its own git state and contents that removing the folder destroys and `git status`
    /// does not show.
    pub per_worktree: PerWorktree,
    pub merge: Merge,
    pub upstream: Upstream,
    /// Stash entries whose message names its branch, or, when its HEAD is detached, that
    /// were made at its HEAD's commit. Stash entries belong to the repo and outlive it.
    pub stashes: u32,
    /// Commits at HEAD that no other branch, remote branch or tag holds. Counted only where
    /// it is evidence: for a branch whose upstream is gone.
    pub only_here: Option<u32>,
    /// Committer time of HEAD, Unix seconds.
    pub last_commit: Option<i64>,
    /// When the repo last fetched, from the newest `FETCH_HEAD`, Unix seconds. Remote-tracking
    /// refs say what the remote held then; Scry itself never talks to a remote.
    pub fetched_at: Option<i64>,
}

/// What belongs to one worktree beyond its tracked and untracked files, so removing it
/// destroys it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PerWorktree {
    /// Index entries flagged assume-unchanged or skip-worktree. `git status` does not compare
    /// them with the folder, so an edit to one is invisible to it.
    pub hidden: u32,
    /// Submodules checked out inside it, relative to it. `git worktree remove` refuses these.
    pub submodules: Vec<PathBuf>,
    /// Other worktrees that the scan found inside its folder.
    pub worktrees_inside: Vec<PathBuf>,
    /// Folders inside it with a `.git` of their own, from the size walk.
    pub repos_inside: ReposInside,
    /// Entries in its record under `.git/worktrees` beyond git's standard files and known
    /// agent bookkeeping, such as `hooks`, `refs` or `rebase-merge`. A `config.worktree`
    /// counts when it sets a key agents are not known to set, and is named with those keys:
    /// `config.worktree (core.hookspath)`.
    pub record_extras: Vec<String>,
    /// Commits at a detached HEAD that no local branch or tag holds. HEAD goes with the
    /// worktree, and with it the only ref to them.
    pub detached_only: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReposInside {
    /// Not walked: sizing was skipped, or the walk could not open the folder.
    #[default]
    Unknown,
    /// Folders inside it holding a `.git`, relative to it. `unread` entries could not be
    /// read, and one could hold another.
    Walked { repos: Vec<PathBuf>, unread: u64 },
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
    /// The repo has no `origin/HEAD`, and no `main` or `master`, local or remote, to compare
    /// with.
    NoDefaultBranch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Upstream {
    None,
    /// A remote branch whose remote-tracking ref is here, so it existed at the last fetch.
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

/// Why a worktree needs a look before anyone removes it.
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
    /// Files flagged assume-unchanged or skip-worktree, whose edits `git status` hides.
    HiddenFromStatus {
        files: u32,
    },
    /// Checked-out submodules, which `git worktree remove` refuses to remove.
    Submodules {
        paths: Vec<PathBuf>,
    },
    /// Other worktrees inside the folder.
    WorktreesInside {
        paths: Vec<PathBuf>,
    },
    /// Repositories inside the folder, relative to it.
    ReposInside {
        paths: Vec<PathBuf>,
    },
    /// The folder was not walked, so a repository inside it would go unseen.
    NotWalked,
    /// Entries inside the folder that the walk could not read.
    Unread {
        entries: u64,
    },
    /// Files in its record beyond git's standard ones and known agent bookkeeping.
    RecordExtras {
        names: Vec<String>,
    },
    /// Commits at a detached HEAD that no local branch or tag holds.
    DetachedOnly {
        commits: u32,
    },
    /// Registered, but the folder is gone.
    FolderMissing,
    /// Registered, its folder is gone, and a folder elsewhere links back to the record: it
    /// was moved without `git worktree move`, and `git worktree repair` reconnects it.
    MovedTo {
        path: PathBuf,
    },
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
    /// None when git was given no reason, or the lock file could not be read.
    pub reason: Option<String>,
    /// The lock file is there but could not be read. It still counts as a lock.
    pub unreadable: bool,
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
