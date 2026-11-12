//! Cleaning up one disk item. [`allows`] is the one place that decides what an item allows;
//! a [`Plan`] names the exact folder, its identity and its parents', and its bytes, as the
//! confirmation shows them; [`execute`] checks everything again right before it acts. A
//! cleanup removes exactly the folder it showed, or nothing.
//!
//! - **Trash** moves the folder to the Trash through `NSFileManager`; never `rm`. For a
//!   worktree only its folder moves: git keeps its record, so Put Back in Finder gives a
//!   working checkout with its HEAD reflog, and until then the worktree shows as "can prune".
//! - **Delete** needs the item's name typed, and removes the folder through the descriptor
//!   that was checked, never following a symlink or entering another volume.
//! - **Prune** is for a worktree whose folder is gone. It removes that worktree's own record,
//!   `.git/worktrees/<name>`, and nothing else; never `git worktree prune`, which drops the
//!   record of every worktree whose folder is missing.
//! - **Command** and **Never** do nothing but say why.
//!
//! Right before acting, a cleanup reads `destructive` from the config again; walks the folder
//! for repositories, worktrees and mounted volumes inside it; reads a worktree's label again,
//! `git status` included, after that walk; opens the folder from `/` without following a
//! symlink and checks its identity and its parents' against the plan; and writes an intent
//! row to the audit log, refusing when it cannot. The home folder, Desktop, Documents,
//! Downloads and every folder above one of them are out of reach whatever the plan says.
//! What lands in the Trash must be the planned folder, or it is moved back.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::catalog::Cleanup;
use super::hold::{self, Fence, Held, HoldError, Removed};
use super::trash::{self, TrashSize};
use super::{Item, WorktreeFacts};
use crate::catalog::Role;
use crate::config::Config;
use crate::model::Agent;
use crate::paths::Paths;
use crate::protected::{FileId, Protected, Resolved, file_id, read_no_follow};
use crate::size::{SubtreeSize, Walker};
use crate::store::{AuditRow, Store, Trashed};
use crate::sys::now_ms;
use crate::view::worktrees::{Status, bytes};
use crate::worktree::{self, ScanOptions};

const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "allows", rename_all = "snake_case")]
pub enum Allows {
    /// Moved to the Trash.
    Trash,
    /// Removed for good once `word` is typed.
    Delete {
        word: String,
    },
    /// Only git's record of a worktree is left; that record alone is removed.
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
/// is pruned, anything else is left alone. Other items go by their catalog entry. Nothing
/// that is the home folder or above a protected one, holds a repository, a worktree or a
/// mounted volume is removed.
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
    let allowed = match allowed {
        Allows::Trash | Allows::Delete { .. } => match kept(item) {
            Some(why) => Allows::Never { why },
            None => allowed,
        },
        a => a,
    };
    match allowed {
        Allows::Trash | Allows::Delete { .. } | Allows::Prune if !destructive => Allows::Off {
            would: allowed.word().into(),
        },
        a => a,
    }
}

/// Why a folder that its entry would clean up is kept anyway.
fn kept(item: &Item) -> Option<String> {
    if item.fenced {
        return Some(
            "It is your home folder, or Desktop, Documents or Downloads lies inside it.".into(),
        );
    }
    let rel = |p: &Path| {
        p.strip_prefix(&item.path)
            .unwrap_or(p)
            .display()
            .to_string()
    };
    if item.worktree.is_none()
        && let Some(wt) = item
            .inside
            .iter()
            .find(|i| i.kind == super::catalog::Kind::Worktree)
    {
        return Some(format!(
            "The git worktree {} sits inside it; Scry never removes one with a cache.",
            rel(&wt.path)
        ));
    }
    let size = item.size.as_ref().filter(|s| s.error.is_none())?;
    if size.mounts > 0 {
        return Some("A volume is mounted inside it.".into());
    }
    if item.worktree.is_none() {
        if size.git_at_root {
            return Some("It is a git repository; Scry never removes one with a cache.".into());
        }
        if let Some(r) = size.repos.first() {
            return Some(format!(
                "The git repository {} sits inside it; Scry never removes one with a cache.",
                r.display()
            ));
        }
    }
    None
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
    /// The folder, with every folder above it resolved.
    pub path: PathBuf,
    /// The folder's identity when the plan was made. None for a prune: the folder is gone.
    pub expect: Option<FileId>,
    /// The identity of each folder above `path`, from `/` down.
    #[serde(default)]
    pub parents: Vec<FileId>,
    pub allocated: u64,
    /// At least this many bytes come back once the Trash is emptied, or at once for a delete.
    pub reclaimable: u64,
    /// For a worktree: its repository's main checkout.
    pub repo: Option<PathBuf>,
    /// For a worktree: git's record of it, `.git/worktrees/<name>`, and that folder's
    /// identity. A prune removes this folder alone.
    #[serde(default)]
    pub record: Option<PathBuf>,
    #[serde(default)]
    pub record_id: Option<FileId>,
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
    /// Another folder or a symlink stands where the planned one was, or above it.
    Changed {
        path: PathBuf,
    },
    /// It is the home folder, a protected folder or above one, or leads through one.
    Protected {
        path: PathBuf,
    },
    /// A worktree whose label is no longer the one the plan was made for.
    Relabelled {
        now: String,
    },
    /// Something inside it that a cleanup never removes along with it.
    Holds {
        path: PathBuf,
        what: String,
    },
    /// A worktree's record that pruning would lose something with.
    Record {
        why: String,
    },
    /// The audit log cannot be written, so nothing is done.
    Unaudited {
        error: String,
    },
    /// Failed before anything changed.
    Failed {
        error: String,
    },
    /// What stood at the path just before the move was another folder. It went to the
    /// Trash and was moved back.
    Swapped {
        path: PathBuf,
        landed: PathBuf,
    },
    /// Failed after something changed; `did` says exactly what.
    Incomplete {
        did: String,
        error: String,
    },
}

impl Refusal {
    /// Whether anything on disk moved or went before it stopped.
    pub fn changed(&self) -> bool {
        matches!(self, Refusal::Swapped { .. } | Refusal::Incomplete { .. })
    }
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
                "{} is no longer the folder that was listed, or a folder above it changed. \
                 Nothing was done.",
                path.display()
            ),
            Refusal::Protected { path } => write!(
                f,
                "{} is your home folder, holds Desktop, Documents or Downloads, or leads into \
                 one. Nothing was done.",
                path.display()
            ),
            Refusal::Relabelled { now } => write!(
                f,
                "The worktree is now labelled \"{now}\". Nothing was done."
            ),
            Refusal::Holds { path, what } => write!(
                f,
                "{} holds {what}, which Scry never removes with it. Nothing was done.",
                path.display()
            ),
            Refusal::Record { why } => write!(f, "{why} Nothing was done."),
            Refusal::Unaudited { error } => write!(
                f,
                "The audit log cannot be written ({error}), so nothing was done."
            ),
            Refusal::Failed { error } => {
                write!(f, "Cleanup failed before anything changed: {error}")
            }
            Refusal::Swapped { path, landed } => write!(
                f,
                "Another folder stood at {} just before the move. It went to the Trash as {} \
                 and was moved back; nothing else changed.",
                path.display(),
                landed.display()
            ),
            Refusal::Incomplete { did, error } => {
                write!(
                    f,
                    "Cleanup stopped partway: {did}. It stopped because: {error}"
                )
            }
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
    let path = resolve_parent(&item.path)?;
    let worktree = item.worktree.as_ref();
    let record = worktree.and_then(|w: &WorktreeFacts| w.record.clone());
    Ok(Plan {
        item: item.id.clone(),
        name: item.name.clone(),
        action,
        parents: hold::chain(&path),
        path,
        expect,
        allocated: size.allocated,
        reclaimable: size.reclaimable,
        repo: worktree.map(|w| w.repo.clone()),
        record_id: record
            .as_ref()
            .and_then(|r| std::fs::symlink_metadata(r).ok())
            .map(|m| file_id(&m)),
        record,
        word,
    })
}

/// The path with every folder above it resolved, its last component left as it is.
fn resolve_parent(path: &Path) -> Result<PathBuf, Refusal> {
    let changed = || Refusal::Changed {
        path: path.to_path_buf(),
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(changed());
    };
    // Protected folders stop the resolution before anything inside them is read.
    let home = std::env::home_dir().unwrap_or_default();
    match Protected::new(&home).resolve(parent) {
        Resolved::Outside(p) => Ok(p.join(name)),
        Resolved::Inside(_) => Err(Refusal::Protected {
            path: path.to_path_buf(),
        }),
        Resolved::Unresolvable => Err(changed()),
    }
}

/// What a cleanup did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    pub item: String,
    pub action: Action,
    pub path: PathBuf,
    /// Where it landed in the Trash.
    pub moved_to: Option<PathBuf>,
    /// For a worktree moved to the Trash: git kept its record.
    pub worktree: bool,
    /// For a prune: the record that was removed.
    pub record: Option<PathBuf>,
    pub allocated: u64,
    pub reclaimable: u64,
    /// The Trash after a move, since nothing is freed until it is emptied.
    pub trash: Option<TrashSize>,
    /// Said after the outcome: the audit log could not record it.
    pub note: Option<String>,
}

impl Done {
    pub fn sentence(&self) -> String {
        let mut s = match self.action {
            Action::Trash => {
                let mut s = if self.worktree {
                    format!(
                        "Moved the worktree's folder ({}) to the Trash. git keeps its record, so \
                         Put Back in Finder gives a working checkout; until then it shows as \
                         can prune",
                        bytes(self.allocated)
                    )
                } else {
                    format!("Moved {} to the Trash", bytes(self.allocated))
                };
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
            Action::Prune => format!(
                "Removed git's record of the missing worktree ({}); its branch and every other \
                 worktree's record stay.",
                self.record
                    .as_deref()
                    .map_or_else(String::new, |r| r.display().to_string())
            ),
        };
        if let Some(n) = &self.note {
            s.push(' ');
            s.push_str(n);
        }
        s
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
            Action::Prune => "Removed git's record of the missing worktree.".into(),
        }
    }
}

/// Points in a cleanup where a test changes the disk under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moment {
    /// Every check passed and the folder is held; next it moves or goes.
    Act,
    /// A delete has removed this many entries so far.
    Removed(u64),
}

/// Reads a worktree's label again; see [`relabel`].
pub type Label<'a> = dyn Fn(&Plan, &Path, Option<&SubtreeSize>) -> Result<Status, Refusal> + 'a;

/// Who acts and what is checked right before acting.
pub struct Context<'a> {
    pub home: &'a Path,
    /// Where the audit rows go. Without one, nothing is done.
    pub store: Option<&'a Store>,
    /// `cli` or `tui`, for the audit row.
    pub actor: &'a str,
    /// Whether the config allows cleanup, read again each time it is asked; see
    /// [`destructive_now`].
    pub destructive: &'a dyn Fn() -> bool,
    /// Reads a worktree's label again, given the walk of its folder just made; [`relabel`]
    /// by default.
    pub label: &'a Label<'a>,
    pub moment: Option<&'a dyn Fn(Moment)>,
}

impl Context<'_> {
    fn at(&self, m: Moment) {
        if let Some(f) = self.moment {
            f(m);
        }
    }
}

/// `destructive` as the config file says it now. A config that cannot be read allows nothing.
pub fn destructive_now(paths: &Paths) -> bool {
    Config::load(paths).is_ok_and(|(c, _)| c.destructive)
}

/// Re-checks the target, acts, and audits the outcome. `typed` is what was typed to confirm
/// a delete.
pub fn execute(plan: &Plan, typed: Option<&str>, ctx: &Context) -> Result<Done, Refusal> {
    let audit = Audit { ctx, plan };
    let mut result = run(plan, typed, ctx, &audit);
    let outcome = match &result {
        Ok(done) => done.sentence(),
        Err(r) if r.changed() => format!("did not finish: {r}"),
        Err(r) => format!("refused: {r}"),
    };
    if let (Err(e), Ok(done)) = (audit.row(outcome), &mut result) {
        done.note = Some(format!("The audit log could not record this: {e}."));
    }
    result
}

struct Audit<'a> {
    ctx: &'a Context<'a>,
    plan: &'a Plan,
}

impl Audit<'_> {
    fn row(&self, result: String) -> Result<(), String> {
        let store = self
            .ctx
            .store
            .ok_or_else(|| "the store could not be opened".to_string())?;
        store
            .append_audit(&AuditRow {
                at_ms: now_ms(),
                actor: self.ctx.actor.into(),
                action: format!("clean {}", self.plan.action.word()),
                target: self.plan.path.display().to_string(),
                result,
            })
            .map_err(|e| format!("{e:#}"))
    }

    /// Written before anything changes. A cleanup the log cannot record does not happen.
    fn intent(&self) -> Result<(), Refusal> {
        let what = match self.plan.action {
            Action::Trash => "moving it to the Trash".to_string(),
            Action::Delete => "deleting it".to_string(),
            Action::Prune => format!(
                "removing its record {}",
                self.plan
                    .record
                    .as_deref()
                    .map_or_else(String::new, |r| r.display().to_string())
            ),
        };
        self.row(format!("intent: {what}"))
            .map_err(|error| Refusal::Unaudited { error })
    }
}

fn run(plan: &Plan, typed: Option<&str>, ctx: &Context, audit: &Audit) -> Result<Done, Refusal> {
    if !(ctx.destructive)() {
        return Err(Refusal::Off);
    }
    if let Some(word) = &plan.word
        && typed.map(str::trim) != Some(word.as_str())
    {
        return Err(Refusal::WrongWord { word: word.clone() });
    }
    let fence = Fence::new(ctx.home);
    let mut done = Done {
        item: plan.item.clone(),
        action: plan.action,
        path: plan.path.clone(),
        moved_to: None,
        worktree: false,
        record: None,
        allocated: plan.allocated,
        reclaimable: plan.reclaimable,
        trash: None,
        note: None,
    };
    match plan.action {
        Action::Prune => {
            prune(plan, ctx, &fence, audit)?;
            done.record = plan.record.clone();
        }
        Action::Trash | Action::Delete => {
            // Cheap checks first, then the walk, then everything that the walk's time could
            // have changed.
            held(plan, &fence)?;
            let walk = look_inside(plan, ctx.home)?;
            if plan.repo.is_some() {
                expect_label(plan, ctx, Some(&walk), Status::CanGo)?;
            }
            if !(ctx.destructive)() {
                return Err(Refusal::Off);
            }
            audit.intent()?;
            let h = held(plan, &fence)?;
            ctx.at(Moment::Act);
            if plan.action == Action::Trash {
                done.moved_to = Some(to_trash(plan, h, ctx.store)?);
                done.worktree = plan.repo.is_some();
                done.trash = Some(trash::size(ctx.home, ctx.store));
            } else {
                delete(plan, h, ctx)?;
            }
        }
    }
    Ok(done)
}

/// The planned folder, opened from `/`, if it and every folder above it are the ones planned.
fn held(plan: &Plan, fence: &Fence) -> Result<Held, Refusal> {
    let h = hold::hold(&plan.path, fence).map_err(|e| held_refusal(e, &plan.path))?;
    let parents_moved = !plan.parents.is_empty() && h.chain != plan.parents;
    if Some(h.id) != plan.expect || parents_moved {
        return Err(Refusal::Changed {
            path: plan.path.clone(),
        });
    }
    Ok(h)
}

fn held_refusal(e: HoldError, path: &Path) -> Refusal {
    let path = path.to_path_buf();
    match e {
        HoldError::Gone => Refusal::Gone { path },
        HoldError::Changed => Refusal::Changed { path },
        HoldError::Fenced => Refusal::Protected { path },
        HoldError::Io(error) => Refusal::Failed { error },
    }
}

/// Walks the whole folder, with nothing left out, for what a cleanup never removes with it.
fn look_inside(plan: &Plan, home: &Path) -> Result<SubtreeSize, Refusal> {
    let [walk]: [SubtreeSize; 1] = Walker::new(home)
        .measure(std::slice::from_ref(&plan.path))
        .try_into()
        .expect("one root");
    let holds = |what: String| Refusal::Holds {
        path: plan.path.clone(),
        what,
    };
    if let Some(e) = &walk.error {
        return Err(Refusal::Failed {
            error: format!("{} could not be read: {e}", plan.path.display()),
        });
    }
    if walk.mounts > 0 {
        return Err(holds("a mounted volume".into()));
    }
    if plan.repo.is_none() {
        if walk.git_at_root {
            return Err(holds("a git repository at its top".into()));
        }
        if let Some(r) = walk.repos.first() {
            return Err(holds(format!(
                "a git repository or worktree at {}",
                r.display()
            )));
        }
    }
    if plan.action == Action::Delete && walk.skipped > 0 {
        return Err(holds(format!(
            "{} entries that could not be read, so it could not all be deleted",
            walk.skipped
        )));
    }
    Ok(walk)
}

fn expect_label(
    plan: &Plan,
    ctx: &Context,
    walk: Option<&SubtreeSize>,
    want: Status,
) -> Result<(), Refusal> {
    let now = (ctx.label)(plan, ctx.home, walk)?;
    if now == want {
        Ok(())
    } else {
        Err(Refusal::Relabelled {
            now: now.word().into(),
        })
    }
}

/// The worktree's label now: its repository read again with git, its `git status` included,
/// judged with `walk`, the walk of its folder made just before.
pub fn relabel(plan: &Plan, home: &Path, walk: Option<&SubtreeSize>) -> Result<Status, Refusal> {
    let repo = plan.repo.as_ref().ok_or(Refusal::Failed {
        error: "no repository to read".into(),
    })?;
    let mut opts = ScanOptions::new(home, vec![repo.clone()]);
    opts.max_depth = 0;
    opts.agent_dirs.clear();
    let mut scan = worktree::scan(&opts);
    scan.worktrees.retain(|w| w.path == plan.path);
    let Some(w) = scan.worktrees.first() else {
        return Err(Refusal::Relabelled {
            now: "not found in its repository".into(),
        });
    };
    if plan.record.is_some() && w.record != plan.record {
        return Err(Refusal::Relabelled {
            now: "registered under another record".into(),
        });
    }
    if let Some(walk) = walk {
        worktree::apply_sizes(&mut scan, vec![Some(walk.clone())]);
    }
    Ok(Status::of(&scan.worktrees[0]))
}

fn to_trash(plan: &Plan, h: Held, store: Option<&Store>) -> Result<PathBuf, Refusal> {
    let landed = trash::move_to_trash(&plan.path).map_err(|e| Refusal::Failed {
        error: format!("{e:#}"),
    })?;
    // A move to the Trash is a rename: what landed is the folder that stood at the path, and
    // it must be the one held.
    let there = std::fs::symlink_metadata(&landed).map(|m| file_id(&m));
    if there.as_ref().ok() != Some(&h.id) {
        return Err(match move_back(&landed, &plan.path) {
            Ok(()) => Refusal::Swapped {
                path: plan.path.clone(),
                landed,
            },
            Err(e) => Refusal::Incomplete {
                did: format!(
                    "what stood at {} was moved to {} in the Trash; it is not the folder that \
                     was listed, and it could not be moved back",
                    plan.path.display(),
                    landed.display()
                ),
                error: e.to_string(),
            },
        });
    }
    if let Some(store) = store {
        // Only for counting what the Trash holds; the move is done either way.
        let _ = store.record_trashed(&Trashed {
            at_ms: now_ms(),
            item: plan.item.clone(),
            from: plan.path.clone(),
            trash_path: landed.clone(),
            id: h.id,
            bytes: plan.allocated,
        });
    }
    Ok(landed)
}

/// Puts a folder back where it came from, never over something that stands there now.
fn move_back(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes());
    let (from, to) = (c(from)?, c(to)?);
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn delete(plan: &Plan, h: Held, ctx: &Context) -> Result<(), Refusal> {
    let mut progress = |r: &Removed| ctx.at(Moment::Removed(r.files + r.folders));
    hold::remove(h, &mut progress).map(|_| ()).map_err(|s| {
        let error = if s.at.as_os_str().is_empty() {
            s.error
        } else {
            format!("{} (at {})", s.error, s.at.display())
        };
        if s.removed == Removed::default() {
            Refusal::Failed { error }
        } else {
            Refusal::Incomplete {
                did: format!(
                    "{} inside {} were deleted",
                    s.removed.sentence(),
                    plan.path.display()
                ),
                error,
            }
        }
    })
}

/// Removes the worktree's own record, once it is sure the record is that worktree's, its
/// folder is still gone, and nothing else would go with it.
fn prune(plan: &Plan, ctx: &Context, fence: &Fence, audit: &Audit) -> Result<(), Refusal> {
    let record = plan.record.as_ref().ok_or(Refusal::Failed {
        error: "git's record of it is not known".into(),
    })?;
    match std::fs::symlink_metadata(&plan.path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(Refusal::Changed {
                path: plan.path.clone(),
            });
        }
        Err(e) => {
            return Err(Refusal::Failed {
                error: e.to_string(),
            });
        }
    }
    expect_label(plan, ctx, None, Status::CanPrune)?;
    check_record(plan, record, ctx.home)?;
    if !(ctx.destructive)() {
        return Err(Refusal::Off);
    }
    audit.intent()?;
    let h = hold::hold(record, fence).map_err(|e| held_refusal(e, record))?;
    if Some(h.id) != plan.record_id {
        return Err(Refusal::Changed {
            path: record.clone(),
        });
    }
    // Read once more through the held folder, so the checks bind to what is removed.
    match hold::read_at(&h.dir, c"locked") {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        _ => {
            return Err(Refusal::Record {
                why: "The worktree is locked now.".into(),
            });
        }
    }
    let gitdir = hold::read_at(&h.dir, c"gitdir").map_err(|e| Refusal::Record {
        why: format!("Its record's gitdir file could not be read: {e}."),
    })?;
    if record_folder(record, &gitdir, ctx.home).as_ref() != Some(&plan.path) {
        return Err(Refusal::Record {
            why: "Its record now names another folder.".into(),
        });
    }
    ctx.at(Moment::Act);
    hold::remove(h, &mut |_| {}).map(|_| ()).map_err(|s| {
        if s.removed == Removed::default() {
            Refusal::Failed { error: s.error }
        } else {
            Refusal::Incomplete {
                did: format!(
                    "{} of the record {} were removed",
                    s.removed.sentence(),
                    record.display()
                ),
                error: s.error,
            }
        }
    })
}

/// The record names the planned folder, is not locked, stages nothing its HEAD lacks, and a
/// detached HEAD is held by a local branch or tag.
fn check_record(plan: &Plan, record: &Path, home: &Path) -> Result<(), Refusal> {
    let refuse = |why: String| Refusal::Record { why };
    let gitdir = read_no_follow(&record.join("gitdir"))
        .map_err(|e| refuse(format!("Its record's gitdir file could not be read: {e}.")))?;
    if record_folder(record, &gitdir, home).as_ref() != Some(&plan.path) {
        return Err(refuse("Its record names another folder.".into()));
    }
    if std::fs::symlink_metadata(record.join("locked")).is_ok() {
        return Err(refuse("The worktree is locked.".into()));
    }
    let failed = |e: anyhow::Error| refuse(format!("git could not read its record: {e:#}."));
    if worktree::record_staged(record, GIT_TIMEOUT).map_err(failed)? {
        return Err(refuse(
            "Its record's index holds staged changes that no commit has; pruning would drop \
             them."
                .into(),
        ));
    }
    let head = read_no_follow(&record.join("HEAD"))
        .map_err(|e| refuse(format!("Its record's HEAD could not be read: {e}.")))?;
    let head = String::from_utf8_lossy(&head).trim().to_string();
    if !head.starts_with("ref: ") {
        let n = worktree::unheld(record, &head, GIT_TIMEOUT).map_err(failed)?;
        if n > 0 {
            return Err(refuse(format!(
                "Its detached HEAD holds {n} commit{} that no branch or tag holds; pruning \
                 would leave them unreachable.",
                if n == 1 { "" } else { "s" }
            )));
        }
    }
    Ok(())
}

/// The worktree folder a record's `gitdir` text names, resolved as the plan's path is.
fn record_folder(record: &Path, gitdir: &[u8], home: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let text = gitdir.strip_suffix(b"\n").unwrap_or(gitdir);
    if text.is_empty() {
        return None;
    }
    let dot_git = record.join(std::ffi::OsStr::from_bytes(text));
    let folder = match dot_git.file_name() {
        Some(n) if n == ".git" => dot_git.parent()?.to_path_buf(),
        _ => dot_git,
    };
    let name = folder.file_name()?.to_os_string();
    match Protected::new(home).resolve(folder.parent()?) {
        Resolved::Outside(p) => Some(p.join(name)),
        _ => None,
    }
}

/// An agent that writes into this item while it runs, when one is running now: moving its
/// sessions away under it loses what it writes next.
pub fn writer_running(item: &str) -> Option<String> {
    let agent = match item {
        "claude-projects" => Agent::ClaudeCode,
        "codex-sessions" => Agent::Codex,
        _ => return None,
    };
    let snap = crate::sys::scan().ok()?;
    let n = snap
        .processes
        .iter()
        .filter(|p| p.uid == snap.viewer_uid && p.role.and_then(Role::hosts) == Some(agent))
        .count();
    (n > 0).then(|| {
        format!(
            "{} is running ({n} process{}) and writes its sessions here; quit it first, or what \
             it writes next lands in a new folder and the moved sessions cannot be resumed.",
            agent.title(),
            if n == 1 { "" } else { "es" }
        )
    })
}

#[cfg(test)]
mod tests;
