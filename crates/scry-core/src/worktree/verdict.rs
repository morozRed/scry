//! The one place that decides whether a worktree can go or be pruned. It asks two questions
//! in order: what removing the folder would destroy (`loss`), and, only when nothing would be
//! lost, whether its work is finished (`finished`). `NothingLost`, `Finished`, `Safe` and
//! `Prunable` have private fields, so no other code can claim any of them.

use std::path::PathBuf;

use serde::Serialize;

use super::{Changes, Concern, GitFacts, Lock, Merge, ReposInside, Upstream, WorktreeState};

/// Proof that removing a worktree's folder destroys nothing Scry checks:
///
/// - the folder is a valid worktree whose `.git` links back to its record, and it is not
///   locked;
/// - `git status`, aimed at the folder with `--git-dir` and `--work-tree`, lists no change,
///   untracked files and changes inside submodules included;
/// - no index entry is flagged assume-unchanged or skip-worktree;
/// - no submodule is checked out in it, and no other worktree or repository sits inside it,
///   by a walk that read every entry;
/// - its record holds only git's standard files;
/// - a detached HEAD is held by a local branch or tag.
///
/// Ignored files are not checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NothingLost(());

/// Proof that a worktree's work is finished, which is asked only once `NothingLost` holds:
/// every commit of HEAD is in the default branch or on its upstream, as of the repo's last
/// fetch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finished {
    kept_by: KeptBy,
    fetched_at: Option<i64>,
}

impl Finished {
    pub fn kept_by(&self) -> &KeptBy {
        &self.kept_by
    }

    /// When the repo last fetched, Unix seconds; the claim holds as of then. None when git
    /// left no record of a fetch.
    pub fn fetched_at(&self) -> Option<i64> {
        self.fetched_at
    }
}

/// The ref that holds every commit of a worktree's HEAD.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeptBy {
    DefaultBranch { name: String },
    Upstream { name: String },
}

/// Proof that a worktree can go: nothing would be lost, its work is finished, and no stash
/// entry was made on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Safe {
    #[serde(skip)]
    nothing_lost: NothingLost,
    finished: Finished,
    facts: GitFacts,
}

impl Safe {
    pub fn nothing_lost(&self) -> &NothingLost {
        &self.nothing_lost
    }

    pub fn finished(&self) -> &Finished {
        &self.finished
    }

    pub fn facts(&self) -> &GitFacts {
        &self.facts
    }
}

/// Proof that pruning a worktree's record loses nothing: its folder is gone and no folder
/// links back to the record, it is not locked, the record holds only git's standard files,
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
    /// The folder is a valid worktree and git read it.
    Present(GitFacts),
    /// Registered, and the folder is gone. `only_here` counts the commits at the record's
    /// HEAD that no other ref holds; on a detached HEAD a prune loses them.
    Missing {
        detached: bool,
        only_here: Option<u32>,
        record_extras: Vec<String>,
        /// A folder elsewhere whose `.git` links back to this record.
        moved_to: Option<PathBuf>,
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
        Reading::Present(facts) => present(facts, lock),
        Reading::Missing {
            moved_to: Some(path),
            ..
        } => look(Concern::MovedTo { path }, vec![locked]),
        Reading::Missing {
            detached,
            only_here,
            record_extras,
            moved_to: None,
        } => {
            let loses = detached && only_here != Some(0);
            if locked.is_none() && !loses && record_extras.is_empty() {
                WorktreeState::CanPrune(Prunable {
                    branch_only: only_here.filter(|_| !detached),
                })
            } else {
                let extras = (!record_extras.is_empty()).then_some(Concern::RecordExtras {
                    names: record_extras,
                });
                look(
                    Concern::FolderMissing,
                    vec![locked, only(only_here), extras],
                )
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

fn present(facts: GitFacts, lock: Option<&Lock>) -> WorktreeState {
    let mut concerns = match loss(&facts, lock) {
        Err(Lost::Changes) => return WorktreeState::Dirty(facts),
        Err(Lost::Locked) => return WorktreeState::Locked(facts),
        Err(Lost::Unseen(concerns)) => concerns,
        Ok(nothing_lost) => match finished(&nothing_lost, &facts) {
            Some(finished) if facts.stashes == 0 => {
                return WorktreeState::CanGo(Safe {
                    nothing_lost,
                    finished,
                    facts,
                });
            }
            _ => Vec::new(),
        },
    };
    // It cannot go. It needs a look when it would otherwise be one to clean up: its HEAD is
    // kept, or its upstream is gone. Anything else is work in progress, and what removing
    // it would lose stays in its evidence.
    let kept = kept_by(&facts).is_some();
    if let (false, Upstream::Gone { name }) = (kept, &facts.upstream) {
        concerns.push(Concern::UpstreamGone {
            upstream: name.clone(),
        });
        if let Some(commits) = facts.only_here.filter(|&c| c > 0) {
            concerns.push(Concern::OnlyHere { commits });
        }
    } else if !kept {
        return WorktreeState::Active(facts);
    }
    if facts.stashes > 0 {
        concerns.push(Concern::Stashed {
            entries: facts.stashes,
        });
    }
    WorktreeState::NeedsLook {
        concerns,
        facts: Some(facts),
    }
}

/// What removing a worktree's folder would destroy.
enum Lost {
    /// `git status` lists changes.
    Changes,
    Locked,
    /// Something `git status` does not show, or that could not be checked.
    Unseen(Vec<Concern>),
}

fn loss(facts: &GitFacts, lock: Option<&Lock>) -> Result<NothingLost, Lost> {
    if facts.changes != Changes::Clean {
        return Err(Lost::Changes);
    }
    if lock.is_some() {
        return Err(Lost::Locked);
    }
    let p = &facts.per_worktree;
    let mut concerns = Vec::new();
    if p.hidden > 0 {
        concerns.push(Concern::HiddenFromStatus { files: p.hidden });
    }
    if !p.submodules.is_empty() {
        concerns.push(Concern::Submodules {
            paths: p.submodules.clone(),
        });
    }
    if !p.worktrees_inside.is_empty() {
        concerns.push(Concern::WorktreesInside {
            paths: p.worktrees_inside.clone(),
        });
    }
    match &p.repos_inside {
        ReposInside::Unknown => concerns.push(Concern::NotWalked),
        ReposInside::Walked { repos, unread } => {
            // A checked-out submodule is a repository inside too, and is named as a submodule.
            let repos: Vec<PathBuf> = repos
                .iter()
                .filter(|r| !p.submodules.contains(r))
                .cloned()
                .collect();
            if !repos.is_empty() {
                concerns.push(Concern::ReposInside { paths: repos });
            }
            if *unread > 0 {
                concerns.push(Concern::Unread { entries: *unread });
            }
        }
    }
    // The record's `modules` folder holds the submodules' git folders, already named above.
    let extras: Vec<String> = p
        .record_extras
        .iter()
        .filter(|n| p.submodules.is_empty() || *n != "modules")
        .cloned()
        .collect();
    if !extras.is_empty() {
        concerns.push(Concern::RecordExtras { names: extras });
    }
    if p.detached_only > 0 {
        concerns.push(Concern::DetachedOnly {
            commits: p.detached_only,
        });
    }
    if concerns.is_empty() {
        Ok(NothingLost(()))
    } else {
        Err(Lost::Unseen(concerns))
    }
}

fn finished(_: &NothingLost, facts: &GitFacts) -> Option<Finished> {
    Some(Finished {
        kept_by: kept_by(facts)?,
        fetched_at: facts.fetched_at,
    })
}

/// The ref holding every commit of HEAD, as of the last fetch.
fn kept_by(facts: &GitFacts) -> Option<KeptBy> {
    match (&facts.merge, &facts.upstream) {
        (Merge::Merged { into }, _) => Some(KeptBy::DefaultBranch { name: into.clone() }),
        (_, Upstream::Tracking { name, ahead: 0, .. }) => {
            Some(KeptBy::Upstream { name: name.clone() })
        }
        _ => None,
    }
}
