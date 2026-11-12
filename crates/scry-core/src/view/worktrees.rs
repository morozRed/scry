//! The Worktrees view model: one row per worktree with a status word, the sentence that
//! explains it and the evidence behind it, from a `worktree` scan. Skins only draw this.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::human_bytes;
use crate::worktree::{
    Changes, Concern, GitFacts, Head, KeptBy, Merge, NotRead, ReposInside, Upstream, Worktree,
    WorktreeScan, WorktreeState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Every loss check passed and its work is finished, as of the last fetch. Ignored files
    /// are not checked; see `WorktreeState::CanGo`.
    CanGo,
    /// Only git's record is left, and pruning it frees almost nothing.
    CanPrune,
    /// Removing it could lose something, or its work may not be finished.
    NeedsLook,
    /// Has uncommitted changes.
    Dirty,
    /// Clean, and locked.
    Locked,
    /// Clean, with work not yet merged or pushed.
    Active,
    NotRead,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::CanGo => "can go",
            Status::CanPrune => "can prune",
            Status::NeedsLook => "needs a look",
            Status::Dirty => "dirty",
            Status::Locked => "locked",
            Status::Active => "active",
            Status::NotRead => "not read",
        }
    }

    /// Whether a cleanup may offer to remove the folder: only for "can go". Scry's cleanup
    /// moves folders to the Trash, so an ignored file the checks do not cover can be put back.
    pub fn removable(self) -> bool {
        self == Status::CanGo
    }

    pub fn of(w: &Worktree) -> Status {
        match &w.state {
            WorktreeState::CanGo(_) => Status::CanGo,
            WorktreeState::CanPrune(_) => Status::CanPrune,
            WorktreeState::NeedsLook { .. } => Status::NeedsLook,
            WorktreeState::Dirty(_) => Status::Dirty,
            WorktreeState::Locked(_) => Status::Locked,
            WorktreeState::Active(_) => Status::Active,
            WorktreeState::NotRead(_) => Status::NotRead,
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
    /// None when the folder was not sized.
    pub allocated: Option<u64>,
    /// At least this many bytes come back at once on delete. None when not sized.
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
    /// Rows per status.
    pub counts: BTreeMap<Status, usize>,
    /// Repos with at least one row.
    pub repos: usize,
    /// At least this many bytes come back by deleting every worktree that can go and was
    /// sized.
    pub reclaimable: u64,
    /// Worktrees that can go but were not sized, so `reclaimable` leaves them out.
    pub unsized_can_go: usize,
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
        let mut counts = BTreeMap::new();
        for r in &rows {
            *counts.entry(r.status).or_insert(0) += 1;
        }
        let can_go: Vec<&WorktreeRow> = rows.iter().filter(|r| r.status.removable()).collect();
        let reclaimable = can_go.iter().filter_map(|r| r.reclaimable).sum();
        let unsized_can_go = can_go.iter().filter(|r| r.reclaimable.is_none()).count();
        let skipped = scan
            .worktrees
            .iter()
            .filter_map(|w| w.size.as_ref())
            .map(|s| s.skipped)
            .sum();
        let mut repos: Vec<&str> = rows.iter().map(|r| r.repo.as_str()).collect();
        repos.sort_unstable();
        repos.dedup();

        let mut summary = if rows.is_empty() {
            format!(
                "No linked worktrees in {} under {}.",
                count(scan.repos, "repo"),
                list_paths(&scan.roots, home)
            )
        } else {
            let parts: Vec<String> = counts
                .iter()
                .map(|(&status, &n)| match status {
                    Status::CanGo => match (can_go.len() - unsized_can_go, unsized_can_go) {
                        (0, _) => format!("{n} can go (not sized)"),
                        (_, 0) => format!("{n} can go and free at least {}", bytes(reclaimable)),
                        (_, k) => format!(
                            "{n} can go and free at least {} ({k} of them not sized)",
                            bytes(reclaimable)
                        ),
                    },
                    Status::CanPrune => format!("{n} can be pruned"),
                    Status::NeedsLook if n == 1 => "1 needs a look".into(),
                    Status::NeedsLook => format!("{n} need a look"),
                    other => format!("{n} {}", other.word()),
                })
                .collect();
            format!(
                "{} in {} ({} scanned): {}.",
                count(rows.len(), "worktree"),
                count(repos.len(), "repo"),
                scan.repos,
                parts.join(", ")
            )
        };
        if skipped > 0 {
            summary.push_str(&format!(
                " {} could not be read.",
                count(skipped as usize, "entry")
            ));
        }
        WorktreesView {
            summary,
            repos: repos.len(),
            rows,
            counts,
            reclaimable,
            unsized_can_go,
            skipped,
        }
    }
}

/// The size, when the walk read the folder.
fn sized(w: &Worktree) -> Option<&crate::size::SubtreeSize> {
    w.size.as_ref().filter(|s| s.error.is_none())
}

fn folder_gone(w: &Worktree) -> bool {
    match &w.state {
        WorktreeState::CanPrune(_) => true,
        WorktreeState::NeedsLook { concerns, .. } => concerns.contains(&Concern::FolderMissing),
        _ => false,
    }
}

fn row(w: &Worktree, now: i64, home: &Path) -> WorktreeRow {
    let status = Status::of(w);
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
        allocated: sized(w).map(|s| s.size.allocated),
        reclaimable: sized(w).map(|s| s.size.reclaimable),
        creator: w.creator.agent.map_or("unknown", |a| a.label()).to_string(),
        last_commit: w
            .state
            .facts()
            .and_then(|f| f.last_commit)
            .map(|t| short_age(now - t)),
        sentence: sentence(w, now, home),
        evidence: evidence(w, now, home),
        worktree: w.clone(),
    }
}

fn sentence(w: &Worktree, now: i64, home: &Path) -> String {
    let name = w.name();
    let frees = match sized(w) {
        Some(s) => format!("; deleting it frees at least {}", bytes(s.size.reclaimable)),
        None => String::new(),
    };
    match &w.state {
        WorktreeState::CanGo(safe) => {
            let as_of = as_of_fetch(safe.finished().fetched_at(), now);
            match safe.finished().kept_by() {
                KeptBy::DefaultBranch { name: into } => {
                    format!("{name} is clean and merged into {into} {as_of}{frees}.")
                }
                KeptBy::Upstream { name: up } => {
                    format!("{name} is clean and every commit is on {up} {as_of}{frees}.")
                }
            }
        }
        WorktreeState::CanPrune(_) => format!(
            "{name} is registered in {} but its folder is gone; `git worktree prune` removes \
             only the record and frees almost nothing.",
            w.repo_name()
        ),
        WorktreeState::NeedsLook { concerns, .. } => {
            let parts: Vec<String> = concerns.iter().map(|c| concern(w, c, home)).collect();
            format!("{name} needs a look: {}.", and_list(&parts))
        }
        WorktreeState::Dirty(f) => {
            let Changes::Dirty { changed, untracked } = f.changes else {
                return format!("{name} has uncommitted changes.");
            };
            let what = changes_phrase(changed, untracked);
            let them = if changed + untracked == 1 {
                "it"
            } else {
                "them"
            };
            match &f.merge {
                Merge::Merged { into } => format!(
                    "{name} is merged into {into} but has {what}; deleting it would lose {them}."
                ),
                _ => format!("{name} has {what}; deleting it would lose {them}."),
            }
        }
        WorktreeState::Locked(_) => {
            format!(
                "{name} is locked{}; git keeps it until it is unlocked.",
                locked_by(w)
            )
        }
        WorktreeState::Active(f) => active(&name, f, now),
        WorktreeState::NotRead(NotRead::ProtectedFolder { folder }) => format!(
            "{name} was not read: it leads into {}, which macOS asks permission for.",
            tilde(folder, home)
        ),
        WorktreeState::NotRead(NotRead::Failed { error }) => {
            format!("{name} could not be read: {error}.")
        }
    }
}

fn active(name: &str, f: &GitFacts, now: i64) -> String {
    let when = f
        .last_commit
        .map(|t| format!("; its last commit was {}", long_age(now - t)))
        .unwrap_or_default();
    let detached_only = f.per_worktree.detached_only;
    match (&f.merge, &f.upstream) {
        _ if detached_only > 0 => format!(
            "{name} has {} on its detached HEAD that no branch or tag holds{when}.",
            count(detached_only as usize, "commit")
        ),
        (
            _,
            Upstream::Tracking {
                name: up, ahead, ..
            },
        ) if *ahead > 0 => format!(
            "{name} has {} not pushed to {up}{when}.",
            count(*ahead as usize, "commit")
        ),
        (Merge::Unmerged { into }, _) => format!("{name} has commits not in {into}{when}."),
        (Merge::Unborn, _) => format!("{name} has no commit yet."),
        _ => format!("{name} is clean, and its repo has no main branch to compare it with."),
    }
}

/// When the claims about remote branches were last true: git's refs for them change only on
/// a fetch, and Scry never fetches.
fn as_of_fetch(fetched_at: Option<i64>, now: i64) -> String {
    match fetched_at {
        Some(t) => format!("as of the last fetch, {}", long_age(now - t)),
        None => "as of the last fetch (time unknown)".into(),
    }
}

/// "1 file is" or "3 files are".
fn count_verb(n: usize, noun: &str, one: &str, many: &str) -> String {
    format!("{} {}", count(n, noun), if n == 1 { one } else { many })
}

fn concern(w: &Worktree, c: &Concern, home: &Path) -> String {
    let holds = match sized(w) {
        Some(s) => format!(" holds {}", count(s.files as usize, "file")),
        None => String::new(),
    };
    match c {
        Concern::UpstreamGone { upstream } => format!("its upstream {upstream} is gone"),
        Concern::OnlyHere { commits } => format!(
            "{} nowhere else",
            count_verb(*commits as usize, "commit", "exists", "exist")
        ),
        Concern::Stashed { entries } => format!(
            "{} made {}",
            count_verb(*entries as usize, "stash entry", "was", "were"),
            match w.head {
                Head::Detached { .. } => "at its HEAD",
                _ => "on its branch",
            }
        ),
        Concern::Locked { .. } => format!("it is locked{}", locked_by(w)),
        Concern::HiddenFromStatus { files } => format!(
            "{} hidden from git status",
            count_verb(*files as usize, "file", "is", "are")
        ),
        Concern::Submodules { paths } => format!(
            "it has {} checked out, which `git worktree remove` refuses to remove",
            count(paths.len(), "submodule")
        ),
        Concern::WorktreesInside { paths } => format!(
            "{} inside it",
            count_verb(paths.len(), "other worktree", "sits", "sit")
        ),
        Concern::ReposInside { paths } => format!(
            "{} inside it",
            count_verb(paths.len(), "repository", "sits", "sit")
        ),
        Concern::NotWalked => {
            "its folder was not walked, so a repository inside it would go unseen".into()
        }
        Concern::Unread { entries } => format!(
            "{} inside it could not be read",
            count(*entries as usize, "entry")
        ),
        Concern::RecordExtras { names } => format!(
            "its record in the repo's git folder also holds {}",
            and_list(names)
        ),
        Concern::DetachedOnly { commits } => format!(
            "its detached HEAD holds {} that no branch or tag holds",
            count(*commits as usize, "commit")
        ),
        Concern::FolderMissing => "its folder is gone".into(),
        Concern::MovedTo { path } => {
            let to = tilde(path, home);
            format!(
                "its folder was moved by hand to {to}; `git worktree repair {to}` reconnects it"
            )
        }
        Concern::NoGitLink if holds.is_empty() => "its folder has no .git link".into(),
        Concern::NoGitLink => format!("its folder has no .git link but{holds}"),
        Concern::LinkElsewhere { gitdir } => format!(
            "its .git link points at {} instead of its own record",
            tilde(gitdir, home)
        ),
        Concern::BrokenLink { gitdir } if holds.is_empty() => {
            format!(
                "its .git link points at {}, which is gone",
                tilde(gitdir, home)
            )
        }
        Concern::BrokenLink { gitdir } => format!(
            "its .git link points at {}, which is gone, and its folder{holds}",
            tilde(gitdir, home)
        ),
    }
}

fn locked_by(w: &Worktree) -> String {
    if w.lock.as_ref().is_some_and(|l| l.unreadable) {
        return " (its lock file could not be read)".into();
    }
    match w.lock.as_ref().and_then(|l| l.reason.as_deref()) {
        Some(r) if r.starts_with("claude agent ") => match claude_lock_pid(r) {
            Some(pid) => format!(" by a Claude Code agent (pid {pid})"),
            None => " by a Claude Code agent".into(),
        },
        Some(r) => format!(" (\"{r}\")"),
        None => String::new(),
    }
}

fn evidence(w: &Worktree, now: i64, home: &Path) -> Vec<String> {
    let mut out = vec![format!("path {}", tilde(&w.path, home))];
    match &w.head {
        Head::Branch {
            name,
            oid: Some(oid),
        } => out.push(format!("branch {name} at {}", short(oid))),
        Head::Branch { name, oid: None } => out.push(format!("branch {name}, with no commit")),
        Head::Detached { oid } => out.push(format!("detached at {}", short(oid))),
        Head::Unknown => {}
    }
    if let Some(f) = w.state.facts() {
        facts_evidence(f, now, home, &mut out);
    }
    match &w.state {
        WorktreeState::CanGo(safe) => {
            out.push(
                "nothing to lose: a valid, unlocked worktree; git status lists nothing and \
                 nothing is hidden from it; no submodule, worktree or repository sits inside \
                 it; its record holds only git's standard files. Ignored files are not checked"
                    .into(),
            );
            let finished = safe.finished();
            let (KeptBy::DefaultBranch { name } | KeptBy::Upstream { name }) = finished.kept_by();
            out.push(format!(
                "finished: {name} holds every commit of HEAD {}, and no stash entry was made \
                 on it",
                as_of_fetch(finished.fetched_at(), now)
            ));
        }
        WorktreeState::CanPrune(p) => {
            out.push("the folder does not exist".into());
            if let (Some(n), Head::Branch { name, .. }) =
                (p.branch_only().filter(|&n| n > 0), &w.head)
            {
                out.push(format!(
                    "branch {name} keeps {} that {} nowhere else; pruning keeps the branch",
                    count(n as usize, "commit"),
                    if n == 1 { "exists" } else { "exist" }
                ));
            }
        }
        WorktreeState::NeedsLook { concerns, facts } => {
            for c in concerns {
                match c {
                    Concern::FolderMissing => out.push("the folder does not exist".into()),
                    Concern::MovedTo { path } => out.push(format!(
                        "the folder does not exist, and {} links back to its record",
                        tilde(path, home)
                    )),
                    Concern::RecordExtras { names } if facts.is_none() => {
                        out.push(format!("its record also holds {}", names.join(", ")))
                    }
                    Concern::NoGitLink => out.push("the folder has no .git file".into()),
                    Concern::LinkElsewhere { gitdir } => out.push(format!(
                        ".git points at {}, not at its own record",
                        tilde(gitdir, home)
                    )),
                    Concern::BrokenLink { gitdir } => out.push(format!(
                        ".git points at {}, which does not exist",
                        tilde(gitdir, home)
                    )),
                    Concern::OnlyHere { commits } if facts.is_none() => out.push(format!(
                        "{} {} nowhere else",
                        count(*commits as usize, "commit"),
                        if *commits == 1 { "exists" } else { "exist" }
                    )),
                    _ => {}
                }
            }
        }
        WorktreeState::NotRead(NotRead::ProtectedFolder { folder }) => out.push(format!(
            "not read: protected folder {}",
            tilde(folder, home)
        )),
        WorktreeState::NotRead(NotRead::Failed { error }) => out.push(format!("not read: {error}")),
        WorktreeState::Dirty(_) | WorktreeState::Locked(_) | WorktreeState::Active(_) => {}
    }
    if let Some(lock) = &w.lock {
        out.push(match &lock.reason {
            _ if lock.unreadable => "locked: the lock file is there but could not be read".into(),
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
            Some(_) if folder_gone(w) => {}
            Some(e) => out.push(format!("not sized: {e}")),
            None => {
                let mut line = format!(
                    "{} on disk in {}; a delete frees at least {} at once",
                    bytes(s.size.allocated),
                    count(s.files as usize, "file"),
                    bytes(s.size.reclaimable)
                );
                if s.size.reclaimable < s.size.allocated {
                    line.push_str(
                        "; the rest is shared with clones, snapshots or hard links, or held \
                         by clones that all sit inside it",
                    );
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

fn facts_evidence(f: &GitFacts, now: i64, home: &Path, out: &mut Vec<String>) {
    out.push(match f.changes {
        Changes::Clean => "git status: clean, untracked and submodule changes included".into(),
        Changes::Dirty { changed, untracked } => {
            format!("git status: {}", changes_phrase(changed, untracked))
        }
    });
    let p = &f.per_worktree;
    let paths = |ps: &[std::path::PathBuf]| {
        let shown: Vec<String> = ps.iter().map(|q| tilde(q, home)).collect();
        shown.join(", ")
    };
    if p.hidden > 0 {
        out.push(format!(
            "{} flagged assume-unchanged or skip-worktree, so git status does not compare {} \
             with the folder",
            count(p.hidden as usize, "file"),
            if p.hidden == 1 { "it" } else { "them" }
        ));
    }
    if !p.submodules.is_empty() {
        out.push(format!("submodules checked out: {}", paths(&p.submodules)));
    }
    if !p.worktrees_inside.is_empty() {
        out.push(format!("worktrees inside: {}", paths(&p.worktrees_inside)));
    }
    match &p.repos_inside {
        ReposInside::Unknown => out.push("not walked for repositories inside".into()),
        ReposInside::Walked { repos, unread } => {
            if !repos.is_empty() {
                out.push(format!(
                    "folders inside with their own .git: {}",
                    paths(repos)
                ));
            }
            if *unread > 0 {
                out.push(format!(
                    "{} inside could not be read",
                    count(*unread as usize, "entry")
                ));
            }
        }
    }
    if !p.record_extras.is_empty() {
        out.push(format!(
            "its record also holds {}",
            p.record_extras.join(", ")
        ));
    }
    if p.detached_only > 0 {
        out.push(format!(
            "{} at the detached HEAD that no branch or tag holds",
            count(p.detached_only as usize, "commit")
        ));
    }
    out.push(match &f.merge {
        Merge::Merged { into } => format!("HEAD is in {into}"),
        Merge::Unmerged { into } => format!("HEAD has commits not in {into}"),
        Merge::Unborn => "HEAD has no commit yet".into(),
        Merge::NoDefaultBranch => "no origin/HEAD, main or master to compare with".into(),
    });
    match &f.upstream {
        Upstream::None => out.push("no upstream".into()),
        Upstream::Tracking {
            name,
            ahead,
            behind,
        } => out.push(format!("upstream {name}: {ahead} ahead, {behind} behind")),
        Upstream::Local { name } => out.push(format!("upstream {name} is a local branch")),
        Upstream::Gone { name } => out.push(format!(
            "upstream {name} is gone; a squash merge and a closed pull request look the same, \
             so this is not read as merged"
        )),
    }
    if f.stashes > 0 {
        out.push(format!(
            "{} made on its branch or at its HEAD",
            count(f.stashes as usize, "stash entry")
        ));
    }
    if let Some(n) = f.only_here {
        out.push(format!(
            "{} {} nowhere else",
            count(n as usize, "commit"),
            if n == 1 { "exists" } else { "exist" }
        ));
    }
    if let Some(t) = f.last_commit {
        out.push(format!("last commit {}", long_age(now - t)));
    }
    out.push(match f.fetched_at {
        Some(t) => format!(
            "the repo last fetched {}; remote branches are as of then",
            long_age(now - t)
        ),
        None => "no fetch on record; remote branches are as of the clone or a push".into(),
    });
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
        (c, 0) => count(c as usize, "changed entry"),
        (0, u) => count(u as usize, "untracked entry"),
        (c, u) => format!(
            "{} and {}",
            count(c as usize, "changed entry"),
            count(u as usize, "untracked entry")
        ),
    }
}

fn and_list(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn count(n: usize, noun: &str) -> String {
    // "entry" becomes "entries", but "day" becomes "days".
    let stem = noun
        .strip_suffix('y')
        .filter(|s| !s.ends_with(['a', 'e', 'o', 'u']));
    match stem {
        _ if n == 1 => format!("1 {noun}"),
        Some(stem) => format!("{n} {stem}ies"),
        None => format!("{n} {noun}s"),
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
