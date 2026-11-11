//! The one place that decides whether a worktree can go or be pruned. `Safe` and `Prunable`
//! have private fields, so no other code can claim either.

use std::path::PathBuf;

use serde::Serialize;

use super::{Changes, Concern, GitFacts, Lock, Merge, Upstream, WorktreeState};

/// Proof that deleting a worktree's folder loses nothing: the folder is a valid worktree, it
/// is not locked, one `git status` found it clean, no stash entry was made on it, and its HEAD
/// is kept by the default branch or by its upstream, which still exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Safe {
    kept_by: KeptBy,
    facts: GitFacts,
}

impl Safe {
    pub fn kept_by(&self) -> &KeptBy {
        &self.kept_by
    }

    pub fn facts(&self) -> &GitFacts {
        &self.facts
    }
}

/// The ref that holds every commit of a worktree's HEAD.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeptBy {
    DefaultBranch { name: String },
    Upstream { name: String },
}

/// Proof that pruning a worktree's record loses nothing: its folder is gone, it is not locked,
/// and its HEAD is a branch, which pruning keeps, or a commit some other ref holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Prunable {
    branch_only: Option<u32>,
}

impl Prunable {
    /// Commits that only the record's branch holds. Pruning keeps the branch, and them.
    pub fn branch_only(&self) -> Option<u32> {
        self.branch_only
    }
}

/// What a scan read about one worktree whose folder or record it could look at.
pub(super) enum Reading {
    /// The folder is a valid worktree and one `git status` read it.
    Present(GitFacts),
    /// Registered, and the folder is gone. `only_here` counts the commits at the record's
    /// HEAD that no other ref holds; on a detached HEAD a prune loses them.
    Missing {
        detached: bool,
        only_here: Option<u32>,
    },
    /// Registered, and the folder is there without its `.git` link.
    NoGitLink { only_here: Option<u32> },
    /// Registered, and the folder's `.git` link points at another record.
    LinkElsewhere {
        gitdir: PathBuf,
        only_here: Option<u32>,
    },
    /// Not registered: a folder whose `.git` link points at a git folder that is gone.
    BrokenLink { gitdir: PathBuf },
}

pub(super) fn judge(reading: Reading, lock: Option<&Lock>) -> WorktreeState {
    let locked = lock.map(|l| Concern::Locked {
        reason: l.reason.clone(),
    });
    let only = |n: Option<u32>| {
        n.filter(|&c| c > 0)
            .map(|commits| Concern::OnlyHere { commits })
    };
    let look = |first: Concern, rest: Vec<Option<Concern>>| WorktreeState::NeedsLook {
        concerns: std::iter::once(first)
            .chain(rest.into_iter().flatten())
            .collect(),
        facts: None,
    };
    match reading {
        Reading::Present(facts) => present(facts, lock.is_some()),
        Reading::Missing {
            detached,
            only_here,
        } => {
            let loses = detached && only_here != Some(0);
            if locked.is_none() && !loses {
                WorktreeState::CanPrune(Prunable {
                    branch_only: only_here.filter(|_| !detached),
                })
            } else {
                look(Concern::FolderMissing, vec![locked, only(only_here)])
            }
        }
        Reading::NoGitLink { only_here } => look(Concern::NoGitLink, vec![locked, only(only_here)]),
        Reading::LinkElsewhere { gitdir, only_here } => look(
            Concern::LinkElsewhere { gitdir },
            vec![locked, only(only_here)],
        ),
        Reading::BrokenLink { gitdir } => look(Concern::BrokenLink { gitdir }, vec![locked]),
    }
}

fn present(facts: GitFacts, locked: bool) -> WorktreeState {
    if facts.changes != Changes::Clean {
        return WorktreeState::Dirty(facts);
    }
    if locked {
        return WorktreeState::Locked(facts);
    }
    let kept_by = match (&facts.merge, &facts.upstream) {
        (Merge::Merged { into }, _) => Some(KeptBy::DefaultBranch { name: into.clone() }),
        (_, Upstream::Tracking { name, ahead: 0, .. }) => {
            Some(KeptBy::Upstream { name: name.clone() })
        }
        _ => None,
    };
    let mut concerns = Vec::new();
    if let (None, Upstream::Gone { name }) = (&kept_by, &facts.upstream) {
        concerns.push(Concern::UpstreamGone {
            upstream: name.clone(),
        });
        if let Some(commits) = facts.only_here.filter(|&c| c > 0) {
            concerns.push(Concern::OnlyHere { commits });
        }
    }
    if facts.stashes > 0 && (kept_by.is_some() || !concerns.is_empty()) {
        concerns.push(Concern::Stashed {
            entries: facts.stashes,
        });
    }
    match kept_by {
        Some(kept_by) if concerns.is_empty() => WorktreeState::CanGo(Safe { kept_by, facts }),
        _ if !concerns.is_empty() => WorktreeState::NeedsLook {
            concerns,
            facts: Some(facts),
        },
        _ => WorktreeState::Active(facts),
    }
}
