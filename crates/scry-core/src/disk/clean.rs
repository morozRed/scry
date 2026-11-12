//! Cleaning up one disk item. [`allows`] is the one place that decides what an item allows;
//! a [`Plan`] names the exact folder, its identity and its bytes, as the confirmation shows
//! them; [`execute`] checks everything again right before it acts and writes an audit row
//! whatever happens.
//!
//! - **Trash** moves the folder to the Trash through `NSFileManager`; never `rm`. For a
//!   worktree it then runs `git worktree prune` in its repo, which drops git's record of
//!   every worktree of that repo whose folder is gone.
//! - **Delete** needs the item's name typed. The folder is renamed aside first and checked
//!   to be the same folder, then removed without following symlinks.
//! - **Prune** is for a worktree whose folder is already gone: only `git worktree prune`.
//! - **Command** and **Never** do nothing but say why.
//!
//! `destructive = false` in the config turns every action off.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::catalog::Cleanup;
use super::trash::{self, TrashSize};
use super::{Item, WorktreeFacts};
use crate::protected::{FileId, Protected, Resolved, file_id};
use crate::store::{AuditRow, Store, Trashed};
use crate::sys::now_ms;
use crate::view::worktrees::{Status, bytes};
use crate::worktree::{self, ScanOptions};

const GIT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "allows", rename_all = "snake_case")]
pub enum Allows {
    /// Moved to the Trash.
    Trash,
    /// Removed for good once `word` is typed.
    Delete {
        word: String,
    },
    /// Only git's record of a worktree is left: `git worktree prune`.
    Prune,
    /// The tool's own cleanup, for you to run.
    Command {
        command: String,
    },
    Never {
        why: String,
    },
    /// Cleanup is off in the config; `would` is what it would allow.
    Off {
        would: String,
    },
}

impl Allows {
    /// The word a list shows: `trash`, `delete`, `prune`, `command`, `never` or `off`.
    pub fn word(&self) -> &'static str {
        match self {
            Allows::Trash => "trash",
            Allows::Delete { .. } => "delete",
            Allows::Prune => "prune",
            Allows::Command { .. } => "command",
            Allows::Never { .. } => "never",
            Allows::Off { .. } => "off",
        }
    }
}

/// What `item` allows. Worktrees go by their label: "can go" goes to the Trash, "can prune"
/// is pruned, anything else is left alone. Other items go by their catalog entry.
pub fn allows(item: &Item, destructive: bool) -> Allows {
    let allowed = match (&item.worktree, item.entry.cleanup) {
        (Some(w), _) => match w.status {
            Status::CanGo => Allows::Trash,
            Status::CanPrune => Allows::Prune,
            other => Allows::Never {
                why: format!(
                    "It is labelled \"{}\"; only worktrees that can go or can be pruned are \
                     cleaned up.",
                    other.word()
                ),
            },
        },
        (None, Cleanup::Trash) if !item.is_file => Allows::Trash,
        (None, Cleanup::Delete) if !item.is_file => Allows::Delete {
            word: item.name.clone(),
        },
        (None, Cleanup::Command(c)) => Allows::Command {
            command: c.to_string(),
        },
        (None, Cleanup::Never | Cleanup::Trash | Cleanup::Delete) => Allows::Never {
            why: item.entry.cost.to_string(),
        },
    };
    match allowed {
        Allows::Trash | Allows::Delete { .. } | Allows::Prune if !destructive => Allows::Off {
            would: allowed.word().into(),
        },
        a => a,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Trash,
    Delete,
    Prune,
}

impl Action {
    fn word(self) -> &'static str {
        match self {
            Action::Trash => "trash",
            Action::Delete => "delete",
            Action::Prune => "prune",
        }
    }
}

/// Exactly what a cleanup will touch, as the confirmation lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub item: String,
    pub name: String,
    pub action: Action,
    pub path: PathBuf,
    /// The folder's identity when the plan was made. None for a prune: the folder is gone.
    pub expect: Option<FileId>,
    pub allocated: u64,
    /// At least this many bytes come back once the Trash is emptied, or at once for a delete.
    pub reclaimable: u64,
    /// For a worktree: its repository, where `git worktree prune` runs.
    pub repo: Option<PathBuf>,
    /// What must be typed to confirm a delete.
    pub word: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "refused", rename_all = "snake_case")]
pub enum Refusal {
    Off,
    Never {
        why: String,
    },
    Command {
        command: String,
    },
    NotFound {
        item: String,
    },
    /// Its size is not known yet, so the confirmation could not say what it frees.
    NotMeasured,
    WrongWord {
        word: String,
    },
    Gone {
        path: PathBuf,
    },
    /// Another folder or a symlink stands where the planned one was.
    Changed {
        path: PathBuf,
    },
    Protected {
        path: PathBuf,
    },
    /// A worktree whose label is no longer the one the plan was made for.
    Relabelled {
        now: String,
    },
    Failed {
        error: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Off => write!(
                f,
                "Cleanup is off: set destructive = true in config.toml to allow it."
            ),
            Refusal::Never { why } => write!(f, "Scry does not clean this up. {why}"),
            Refusal::Command { command } => {
                write!(f, "Scry runs nothing here. Run it yourself: {command}")
            }
            Refusal::NotFound { item } => write!(f, "No disk item {item}."),
            Refusal::NotMeasured => write!(f, "It has not been measured yet; try again."),
            Refusal::WrongWord { word } => write!(f, "Type {word} exactly to delete it."),
            Refusal::Gone { path } => write!(f, "{} is gone. Nothing was done.", path.display()),
            Refusal::Changed { path } => write!(
                f,
                "{} is no longer the folder that was listed. Nothing was done.",
                path.display()
            ),
            Refusal::Protected { path } => write!(
                f,
                "{} leads into Desktop, Documents or Downloads. Nothing was done.",
                path.display()
            ),
            Refusal::Relabelled { now } => write!(
                f,
                "The worktree is now labelled \"{now}\". Nothing was done."
            ),
            Refusal::Failed { error } => write!(f, "Cleanup failed: {error}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// The plan for `item`, or why there is none.
pub fn plan(item: &Item, destructive: bool) -> Result<Plan, Refusal> {
    let (action, word) = match allows(item, destructive) {
        Allows::Trash => (Action::Trash, None),
        Allows::Delete { word } => (Action::Delete, Some(word)),
        Allows::Prune => (Action::Prune, None),
        Allows::Command { command } => return Err(Refusal::Command { command }),
        Allows::Never { why } => return Err(Refusal::Never { why }),
        Allows::Off { .. } => return Err(Refusal::Off),
    };
    let size = match (&item.size, action) {
        (Some(s), _) if s.error.is_none() => s.size,
        (_, Action::Prune) => Default::default(),
        _ => return Err(Refusal::NotMeasured),
    };
    let expect = match action {
        Action::Prune => None,
        _ => Some(item.file_id.ok_or_else(|| Refusal::Gone {
            path: item.path.clone(),
        })?),
    };
    Ok(Plan {
        item: item.id.clone(),
        name: item.name.clone(),
        action,
        path: item.path.clone(),
        expect,
        allocated: size.allocated,
        reclaimable: size.reclaimable,
        repo: item
            .worktree
            .as_ref()
            .map(|w: &WorktreeFacts| w.repo.clone()),
        word,
    })
}

/// What a cleanup did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    pub item: String,
    pub action: Action,
    pub path: PathBuf,
    /// Where it landed in the Trash.
    pub moved_to: Option<PathBuf>,
    pub allocated: u64,
    pub reclaimable: u64,
    pub pruned: bool,
    /// The Trash after a move, since nothing is freed until it is emptied.
    pub trash: Option<TrashSize>,
}

impl Done {
    pub fn sentence(&self) -> String {
        match self.action {
            Action::Trash => {
                let mut s = format!("Moved {} to the Trash", bytes(self.allocated));
                if self.pruned {
                    s.push_str(" and pruned git's record of it");
                }
                s.push_str(&format!(
                    ". Emptying the Trash frees at least {}; until then nothing is freed.",
                    bytes(self.reclaimable)
                ));
                if let Some(t) = &self.trash {
                    s.push(' ');
                    s.push_str(&t.sentence());
                }
                s
            }
            Action::Delete => format!(
                "Deleted {} and freed at least {}.",
                self.path.display(),
                bytes(self.reclaimable)
            ),
            Action::Prune => "Pruned git's record of the missing worktree.".into(),
        }
    }

    /// The sentence cut to fit a status line.
    pub fn short(&self) -> String {
        match self.action {
            Action::Trash => {
                let trash = match &self.trash {
                    Some(TrashSize::All { bytes: b }) => format!("; the Trash holds {}", bytes(*b)),
                    Some(TrashSize::Scry { bytes: b, .. }) => {
                        format!("; the Trash holds at least {}", bytes(*b))
                    }
                    None => String::new(),
                };
                format!(
                    "Moved {} to Trash{trash}. Empty it to free the space.",
                    bytes(self.allocated)
                )
            }
            Action::Delete => format!("Deleted, freeing at least {}.", bytes(self.reclaimable)),
            Action::Prune => "Pruned git's record of the missing worktree.".into(),
        }
    }
}

/// Who acts and what is checked right before acting.
pub struct Context<'a> {
    pub home: &'a Path,
    pub store: Option<&'a Store>,
    /// `cli` or `tui`, for the audit row.
    pub actor: &'a str,
    pub destructive: bool,
    /// Checks a worktree's label again; [`relabel`] by default.
    pub label: &'a dyn Fn(&Plan, &Path) -> Result<Status, Refusal>,
}

/// Re-checks the target, acts, and audits the outcome. `typed` is what was typed to confirm
/// a delete.
pub fn execute(plan: &Plan, typed: Option<&str>, ctx: &Context) -> Result<Done, Refusal> {
    let result = run(plan, typed, ctx);
    if let Some(store) = ctx.store {
        let row = AuditRow {
            at_ms: now_ms(),
            actor: ctx.actor.into(),
            action: format!("clean {}", plan.action.word()),
            target: plan.path.display().to_string(),
            result: match &result {
                Ok(done) => done.sentence(),
                Err(r) => format!("refused: {r}"),
            },
        };
        let _ = store.append_audit(&row);
    }
    result
}

fn run(plan: &Plan, typed: Option<&str>, ctx: &Context) -> Result<Done, Refusal> {
    if !ctx.destructive {
        return Err(Refusal::Off);
    }
    if let Some(word) = &plan.word
        && typed.map(str::trim) != Some(word.as_str())
    {
        return Err(Refusal::WrongWord { word: word.clone() });
    }
    match plan.action {
        Action::Prune => {
            if std::fs::symlink_metadata(&plan.path).is_ok() {
                return Err(Refusal::Changed {
                    path: plan.path.clone(),
                });
            }
            expect_label(plan, ctx, Status::CanPrune)?;
        }
        Action::Trash | Action::Delete => {
            same_folder(plan, ctx.home)?;
            if plan.repo.is_some() {
                expect_label(plan, ctx, Status::CanGo)?;
            }
            // The label check runs git and walks the folder; look once more at the end.
            same_folder(plan, ctx.home)?;
        }
    }
    let mut done = Done {
        item: plan.item.clone(),
        action: plan.action,
        path: plan.path.clone(),
        moved_to: None,
        allocated: plan.allocated,
        reclaimable: plan.reclaimable,
        pruned: false,
        trash: None,
    };
    match plan.action {
        Action::Trash => {
            let landed = trash::move_to_trash(&plan.path).map_err(|e| Refusal::Failed {
                error: format!("{e:#}"),
            })?;
            if let (Some(store), Some(id)) = (ctx.store, plan.expect) {
                let _ = store.record_trashed(&Trashed {
                    at_ms: now_ms(),
                    item: plan.item.clone(),
                    from: plan.path.clone(),
                    trash_path: landed.clone(),
                    id,
                    bytes: plan.allocated,
                });
            }
            done.moved_to = Some(landed);
            if let Some(repo) = &plan.repo {
                prune(repo)?;
                done.pruned = true;
            }
            done.trash = Some(trash::size(ctx.home, ctx.store));
        }
        Action::Delete => delete(plan)?,
        Action::Prune => {
            let repo = plan.repo.as_ref().ok_or(Refusal::Failed {
                error: "no repository to prune".into(),
            })?;
            prune(repo)?;
            done.pruned = true;
        }
    }
    Ok(done)
}

/// The path is still the folder the plan names: there, not a symlink, the same inode, and
/// not inside a protected folder by any route.
fn same_folder(plan: &Plan, home: &Path) -> Result<(), Refusal> {
    let gone = || Refusal::Gone {
        path: plan.path.clone(),
    };
    let changed = || Refusal::Changed {
        path: plan.path.clone(),
    };
    let meta = std::fs::symlink_metadata(&plan.path).map_err(|_| gone())?;
    if !meta.is_dir() || Some(file_id(&meta)) != plan.expect {
        return Err(changed());
    }
    match Protected::new(home).resolve(&plan.path) {
        Resolved::Outside(p) if p == plan.path => Ok(()),
        Resolved::Inside(_) => Err(Refusal::Protected {
            path: plan.path.clone(),
        }),
        // A symlink somewhere above it now leads elsewhere.
        _ => Err(changed()),
    }
}

fn expect_label(plan: &Plan, ctx: &Context, want: Status) -> Result<(), Refusal> {
    let now = (ctx.label)(plan, ctx.home)?;
    if now == want {
        Ok(())
    } else {
        Err(Refusal::Relabelled {
            now: now.word().into(),
        })
    }
}

/// The worktree's label now: its repository read again with git, and its folder walked
/// again so a repository nested inside it since is seen.
pub fn relabel(plan: &Plan, home: &Path) -> Result<Status, Refusal> {
    let repo = plan.repo.as_ref().ok_or(Refusal::Failed {
        error: "no repository to read".into(),
    })?;
    let mut opts = ScanOptions::new(home, vec![repo.clone()]);
    opts.max_depth = 0;
    opts.agent_dirs.clear();
    let mut scan = worktree::scan(&opts);
    scan.worktrees.retain(|w| w.path == plan.path);
    if scan.worktrees.is_empty() {
        return Err(Refusal::Relabelled {
            now: "not found in its repository".into(),
        });
    }
    if plan.action != Action::Prune {
        worktree::measure(&mut scan, home);
    }
    Ok(Status::of(&scan.worktrees[0]))
}

fn prune(repo: &Path) -> Result<(), Refusal> {
    crate::git::stdout(repo, &["worktree", "prune"], GIT_TIMEOUT)
        .map(|_| ())
        .map_err(|e| Refusal::Failed {
            error: format!("{e:#}"),
        })
}

/// Renames the folder aside, checks it is still the planned one, then removes it. The
/// rename is atomic, so what is removed is exactly what was checked.
fn delete(plan: &Plan) -> Result<(), Refusal> {
    let failed = |e: std::io::Error| Refusal::Failed {
        error: e.to_string(),
    };
    let parent = plan.path.parent().ok_or(Refusal::Changed {
        path: plan.path.clone(),
    })?;
    let name = plan
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let aside = parent.join(format!(".{name}.scry-deleting-{}", std::process::id()));
    std::fs::rename(&plan.path, &aside).map_err(failed)?;
    let same = std::fs::symlink_metadata(&aside).is_ok_and(|m| Some(file_id(&m)) == plan.expect);
    if !same {
        let _ = std::fs::rename(&aside, &plan.path);
        return Err(Refusal::Changed {
            path: plan.path.clone(),
        });
    }
    // Removes symlinks inside it as links, never what they point at.
    std::fs::remove_dir_all(&aside).map_err(failed)
}

#[cfg(test)]
mod tests;
