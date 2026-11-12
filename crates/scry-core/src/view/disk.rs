//! The Disk view model: free space as Finder counts it, the forecast, the Trash, and the
//! catalog's items ranked by what deleting them frees, each with the cleanup it allows and
//! the exact plan a confirmation would show. `scry disk` prints it; skins draw it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::worktrees::bytes;
use super::{Detail, Fact, State};
use crate::disk::Item;
use crate::disk::catalog::Kind;
use crate::disk::clean::{self, Allows, Plan};
use crate::disk::forecast::Forecast;
use crate::disk::space::Space;
use crate::disk::trash::TrashSize;

/// Items smaller than this are left out once measured; there is nothing to win there.
const MIN_SHOWN: u64 = 1 << 20;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskList {
    /// None when macOS did not report the volume.
    pub space: Option<Space>,
    pub forecast: Option<Forecast>,
    pub trash: Option<TrashSize>,
    pub items: Vec<DiskItem>,
    /// Some sizes are cached ones being measured again, or not measured yet.
    pub sizing: bool,
    /// The worktree scan has not finished, so worktrees are not listed yet.
    #[serde(default)]
    pub worktrees_pending: bool,
    /// At least this many bytes come back by cleaning up every item that allows it.
    pub reclaimable: u64,
    /// The config file, home written as `~`, which turns cleanup on.
    pub config: String,
    pub destructive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskItem {
    /// What `scry clean` takes.
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// The folder, home written as `~`.
    pub path: String,
    pub size: Option<u64>,
    /// At least this many bytes come back on delete.
    pub frees: Option<u64>,
    /// `frees` is only a lower bound: clones or hard links may share its blocks.
    pub at_least: bool,
    /// Shown from the cache while it is measured again.
    pub stale: bool,
    pub creator: String,
    pub allows: Allows,
    /// A worktree's label.
    pub state: Option<State>,
    /// What `t` would do, when it can do anything.
    pub plan: Option<Plan>,
    pub detail: Detail,
}

impl DiskItem {
    /// The cleanup word a list shows.
    pub fn cleanup(&self) -> &'static str {
        self.allows.word()
    }
}

pub struct Inputs<'a> {
    pub items: &'a [Item],
    pub space: Option<Space>,
    pub forecast: Option<Forecast>,
    pub trash: Option<TrashSize>,
    pub destructive: bool,
    pub worktrees_pending: bool,
    pub home: &'a Path,
    pub config: &'a Path,
}

impl DiskList {
    pub fn new(i: Inputs) -> DiskList {
        let mut items: Vec<DiskItem> = i
            .items
            .iter()
            .filter(|it| shown(it, i.destructive))
            .map(|it| item(it, i.destructive, i.home, i.config))
            .collect();
        // Largest first by what a delete frees; unmeasured last, then by name.
        items.sort_by(|a, b| {
            (b.frees.is_some(), b.frees, &a.name).cmp(&(a.frees.is_some(), a.frees, &b.name))
        });
        let reclaimable = items
            .iter()
            .filter(|d| d.plan.is_some() || matches!(d.allows, Allows::Off { .. }))
            .filter_map(|d| d.frees)
            .sum();
        DiskList {
            sizing: i.items.iter().any(|it| it.stale || it.size.is_none()),
            worktrees_pending: i.worktrees_pending,
            space: i.space,
            forecast: i.forecast,
            trash: i.trash,
            items,
            reclaimable,
            config: tilde(i.config, i.home),
            destructive: i.destructive,
        }
    }

    /// One line on free space: `412 GB available of 994 GB on Macintosh HD, 21 GB of it purgeable`.
    pub fn space_line(&self) -> String {
        let Some(s) = &self.space else {
            return "macOS did not report free space.".into();
        };
        let mut line = format!(
            "{} available of {} on {}",
            bytes(s.available),
            bytes(s.total),
            s.volume
        );
        if s.purgeable > 0 {
            line.push_str(&format!(", {} of it purgeable", bytes(s.purgeable)));
        }
        line.push('.');
        match s.snapshots.len() {
            0 => {}
            1 => line.push_str(" 1 local snapshot holds space too."),
            n => line.push_str(&format!(" {n} local snapshots hold space too.")),
        }
        line
    }

    pub fn forecast_line(&self) -> String {
        self.forecast
            .as_ref()
            .map_or_else(|| Forecast::NoSamples.sentence(), Forecast::sentence)
    }
}

fn shown(it: &Item, destructive: bool) -> bool {
    match &it.size {
        None => true,
        Some(_) if it.worktree.is_some() => {
            matches!(
                clean::allows(it, destructive),
                Allows::Prune | Allows::Off { .. }
            ) || it
                .size
                .as_ref()
                .is_some_and(|s| s.size.allocated >= MIN_SHOWN)
        }
        Some(s) => s.size.allocated >= MIN_SHOWN,
    }
}

fn item(it: &Item, destructive: bool, home: &Path, config: &Path) -> DiskItem {
    let allows = clean::allows(it, destructive);
    let sized = it.size.as_ref().filter(|s| s.error.is_none());
    let path = tilde(&it.path, home);
    let fact = |label: &str, value: String| Fact {
        label: label.into(),
        value,
    };
    let mut facts = Vec::new();
    facts.push(fact(
        "size",
        sized.map_or("measuring…".into(), |s| bytes(s.size.allocated)),
    ));
    if let Some(s) = sized {
        let lower = if it.at_least() { "≥ " } else { "" };
        facts.push(fact(
            "frees",
            format!("{lower}{}", bytes(s.size.reclaimable)),
        ));
    }
    facts.push(fact("by", it.creator().to_string()));
    facts.push(fact("cleanup", allows.word().into()));

    let mut evidence = vec![format!("path {path}")];
    if let Some(w) = &it.worktree {
        evidence.push(format!(
            "a worktree of {}, labelled {}",
            w.repo_name,
            w.status.word()
        ));
    }
    match &it.size {
        Some(s) if s.error.is_none() => {
            let mut line = format!(
                "{} on disk in {}; a delete frees at least {} at once",
                bytes(s.size.allocated),
                count(s.files, "file"),
                bytes(s.size.reclaimable)
            );
            if s.size.reclaimable < s.size.allocated {
                line.push_str("; the rest is shared with clones, snapshots or hard links");
            } else if it.entry.shared {
                line.push_str("; its files are often clones or hard links of others");
            }
            evidence.push(line);
            if s.skipped > 0 {
                evidence.push(format!("{} could not be read", count(s.skipped, "entry")));
            }
        }
        Some(s) => evidence.push(format!(
            "not measured: {}",
            s.error.as_deref().unwrap_or("unknown")
        )),
        None => evidence.push("not measured yet".into()),
    }
    if it.stale {
        evidence.push("changed since it was measured; the size shown is the last one read".into());
    }
    evidence.push(match &allows {
        Allows::Trash => match &it.worktree {
            Some(_) => "cleanup: moved to the Trash, then `git worktree prune` drops git's \
                        record; Put Back restores the folder"
                .into(),
            None => "cleanup: moved to the Trash, where Put Back restores it".into(),
        },
        Allows::Delete { word } => {
            format!("cleanup: deleted for good once you type {word}; its tool fills it again")
        }
        Allows::Prune => "cleanup: `git worktree prune` drops git's record; there is no \
                          folder to remove"
            .into(),
        Allows::Command { command } => {
            format!("cleanup: run `{command}` yourself; Scry runs nothing")
        }
        Allows::Never { .. } => "cleanup: none; Scry shows its size only".into(),
        Allows::Off { would } => format!(
            "cleanup: {would}, but destructive = false in {} turns it off",
            tilde(config, home)
        ),
    });
    evidence.push(format!("made by {}", it.creator()));

    DiskItem {
        id: it.id.clone(),
        name: it.name.clone(),
        kind: it.kind(),
        path,
        size: sized.map(|s| s.size.allocated),
        frees: sized.map(|s| s.size.reclaimable),
        at_least: it.at_least(),
        stale: it.stale,
        creator: it.creator().to_string(),
        state: it.worktree.as_ref().map(|w| w.status.state()),
        plan: clean::plan(it, destructive).ok(),
        allows,
        detail: Detail {
            title: format!("{} · {}", it.name, it.kind().word()),
            facts,
            sentence: it.cost().to_string(),
            evidence,
        },
    }
}

fn count(n: u64, noun: &str) -> String {
    match (n, noun) {
        (1, _) => format!("1 {noun}"),
        (_, "entry") => format!("{n} entries"),
        _ => format!("{n} {noun}s"),
    }
}

fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if home.as_os_str().len() > 1 => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}
