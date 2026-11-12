//! Disk: what fills it, what deleting each part would cost, and the cleanup each allows.
//!
//! A reading goes: [`catalog::locate`] finds the catalog's folders, a worktree scan adds the
//! worktrees, and a [`Layout`] cuts them into the parts the [`cache::SizeCache`] keeps.
//! Cached sizes show at once; FSEvents names the parts that changed, and only those are
//! walked again. Sizes are allocated bytes and a lower bound on what a delete frees at once,
//! ranked by the latter.

pub mod cache;
pub mod catalog;
pub mod clean;
pub mod clones;
pub mod forecast;
pub mod fsevents;
pub mod hold;
pub mod space;
pub mod trash;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use cache::{CacheStats, Part, SizeCache};
use catalog::{Entry, Found, Kind};
use serde::Serialize;

use crate::daemon::SAMPLED_VOLUME;
use crate::paths::Paths;
use crate::protected::{FileId, file_id};
use crate::size::SubtreeSize;
use crate::view::disk::{DiskList, Inputs};
use crate::view::worktrees::{Status, WorktreesView};
use crate::worktree::{self, ScanOptions, WorktreeScan};

/// One thing on the disk the catalog knows: a cache, a `node_modules`, a worktree.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub entry: &'static Entry,
    pub name: String,
    pub path: PathBuf,
    pub is_file: bool,
    /// The folder's identity when it was found; a cleanup acts only on this one.
    pub file_id: Option<FileId>,
    /// None until it has been measured once.
    pub size: Option<SubtreeSize>,
    /// The size is the cached one and is being measured again.
    pub stale: bool,
    pub worktree: Option<WorktreeFacts>,
    /// It is the home folder, or a folder above Desktop, Documents or Downloads.
    pub fenced: bool,
    /// Other items inside its folder. Each is measured on its own and not counted here.
    pub inside: Vec<Inside>,
}

/// An item inside another's folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inside {
    pub name: String,
    pub kind: Kind,
    pub path: PathBuf,
}

impl Item {
    pub fn kind(&self) -> Kind {
        self.entry.kind
    }

    pub fn creator(&self) -> &str {
        match &self.worktree {
            Some(w) => &w.creator,
            None => self.entry.creator,
        }
    }

    /// What deleting it costs, in one sentence.
    pub fn cost(&self) -> &str {
        match &self.worktree {
            Some(w) => &w.sentence,
            None => self.entry.cost,
        }
    }

    pub fn reclaimable(&self) -> Option<u64> {
        self.size
            .as_ref()
            .filter(|s| s.error.is_none())
            .map(|s| s.size.reclaimable)
    }

    /// What a delete frees is a lower bound: the entry is known for clones and hard links,
    /// or the walk found blocks shared with something outside it.
    pub fn at_least(&self) -> bool {
        self.entry.shared
            || self
                .size
                .as_ref()
                .is_some_and(|s| s.size.reclaimable < s.size.allocated)
    }
}

/// What the worktree scan said about a worktree item.
#[derive(Clone, Debug)]
pub struct WorktreeFacts {
    /// The repository's main checkout.
    pub repo: PathBuf,
    pub repo_name: String,
    pub status: Status,
    pub sentence: String,
    pub creator: String,
    /// git's record of it, `.git/worktrees/<name>`.
    pub record: Option<PathBuf>,
}

/// The items of a reading and the parts they are measured in.
pub struct Layout {
    pub items: Vec<Item>,
    pub parts: Vec<Part>,
    /// The item each part belongs to.
    owner: Vec<usize>,
    /// Every item's folder, so no walk counts one inside another.
    pub exclude: Vec<PathBuf>,
}

impl Layout {
    /// Items for what `locate` found and, once the worktree scan ran, the worktrees. A
    /// `node_modules` inside a worktree is left to the worktree. Folders in `split` are
    /// measured one folder down; see [`SizeCache::split`].
    pub fn new(
        found: &[Found],
        scan: Option<&WorktreeScan>,
        home: &Path,
        now: i64,
        split: &HashSet<PathBuf>,
    ) -> Layout {
        let wt_paths: Vec<&Path> = scan
            .map(|s| s.worktrees.iter().map(|w| w.path.as_path()).collect())
            .unwrap_or_default();
        let mut items: Vec<Item> = found
            .iter()
            .filter(|f| !wt_paths.iter().any(|w| f.path.starts_with(w)))
            .map(|f| Item {
                id: f.id.clone(),
                entry: f.entry,
                name: match f.entry.kind {
                    Kind::NodeModules => node_modules_name(&f.path),
                    _ => f.entry.name.to_string(),
                },
                path: f.path.clone(),
                is_file: f.is_file,
                file_id: fs::symlink_metadata(&f.path).ok().map(|m| file_id(&m)),
                size: None,
                stale: false,
                worktree: None,
                fenced: false,
                inside: Vec::new(),
            })
            .collect();
        if let Some(scan) = scan {
            let entry = catalog::entry("worktree").expect("in the catalog");
            let view = WorktreesView::new(scan, now, home);
            for r in view.rows {
                let w = r.worktree;
                items.push(Item {
                    id: format!("worktree:{}", w.path.display()),
                    entry,
                    name: r.name,
                    file_id: fs::symlink_metadata(&w.path).ok().map(|m| file_id(&m)),
                    path: w.path.clone(),
                    is_file: false,
                    size: None,
                    stale: false,
                    worktree: Some(WorktreeFacts {
                        repo: w.repo.clone(),
                        repo_name: r.repo,
                        status: r.status,
                        sentence: r.sentence,
                        creator: r.creator,
                        record: w.record.clone(),
                    }),
                    fenced: false,
                    inside: Vec::new(),
                });
            }
        }
        let fence = hold::Fence::new(home);
        let all: Vec<Inside> = items
            .iter()
            .map(|i| Inside {
                name: i.name.clone(),
                kind: i.kind(),
                path: i.path.clone(),
            })
            .collect();
        for item in &mut items {
            item.fenced = item.file_id.is_some_and(|id| !fence.allows(id));
            item.inside = all
                .iter()
                .filter(|o| o.path != item.path && o.path.starts_with(&item.path))
                .cloned()
                .collect();
        }
        let roots: HashSet<&Path> = items.iter().map(|i| i.path.as_path()).collect();
        let mut parts = Vec::new();
        let mut owner = Vec::new();
        for (n, item) in items.iter().enumerate() {
            let mut cut = Cut {
                roots: &roots,
                split,
                parts: Vec::new(),
            };
            if item.is_file {
                cut.parts.push(Part::whole(item.path.clone()));
            } else {
                cut.add(&item.path, item.entry.split, 0);
            }
            owner.extend(std::iter::repeat_n(n, cut.parts.len()));
            parts.extend(cut.parts);
        }
        let exclude = items.iter().map(|i| i.path.clone()).collect();
        Layout {
            items,
            parts,
            owner,
            exclude,
        }
    }

    /// Sums part sizes into their items. An item is unsized while any part has never been
    /// measured, and stale while any part is.
    pub fn fill(&mut self, sizes: &[Option<SubtreeSize>], stale: &[bool]) {
        let mut sums: Vec<Option<SubtreeSize>> =
            vec![Some(SubtreeSize::default()); self.items.len()];
        let mut stales = vec![false; self.items.len()];
        for (((&n, part), size), &st) in self.owner.iter().zip(&self.parts).zip(sizes).zip(stale) {
            stales[n] |= st;
            let Some(sum) = &mut sums[n] else { continue };
            let Some(s) = size else {
                sums[n] = None;
                continue;
            };
            sum.size.allocated += s.size.allocated;
            sum.size.reclaimable += s.size.reclaimable;
            sum.files += s.files;
            sum.dirs += s.dirs;
            sum.skipped += s.skipped;
            sum.mounts += s.mounts;
            sum.shared_links |= s.shared_links;
            // Repositories are found relative to the part; the item wants them relative to
            // itself, and a part below the item's root that holds a `.git` is one.
            let rel = part
                .path
                .strip_prefix(&self.items[n].path)
                .unwrap_or(Path::new(""))
                .to_path_buf();
            if s.git_at_root && !rel.as_os_str().is_empty() {
                sum.repos.push(rel.clone());
            }
            sum.git_at_root |= s.git_at_root && rel.as_os_str().is_empty();
            sum.repos.extend(s.repos.iter().map(|r| rel.join(r)));
            if sum.error.is_none() && rel.as_os_str().is_empty() {
                sum.error.clone_from(&s.error);
            } else if s.error.is_some() {
                sum.skipped += 1;
            }
        }
        for sum in sums.iter_mut().flatten() {
            sum.repos.sort();
            sum.repos.dedup();
        }
        for ((item, sum), st) in self.items.iter_mut().zip(sums).zip(stales) {
            item.size = sum;
            item.stale = st;
        }
    }

    /// Takes in a worktree scan judged again after sizing: "can go" needs the walk.
    pub fn relabel(&mut self, scan: &WorktreeScan, home: &Path, now: i64) {
        let view = WorktreesView::new(scan, now, home);
        for r in view.rows {
            if let Some(item) = self
                .items
                .iter_mut()
                .find(|i| i.worktree.is_some() && i.path == r.worktree.path)
                && let Some(w) = &mut item.worktree
            {
                w.status = r.status;
                w.sentence = r.sentence;
            }
        }
    }

    /// Each worktree's size, in the order of the scan the layout was made from.
    pub fn worktree_sizes(&self, scan: &WorktreeScan) -> Vec<Option<SubtreeSize>> {
        scan.worktrees
            .iter()
            .map(|w| {
                self.items
                    .iter()
                    .find(|i| i.worktree.is_some() && i.path == w.path)
                    .and_then(|i| i.size.clone())
            })
            .collect()
    }
}

/// How deep a big folder is cut into parts.
const MAX_CUT_DEPTH: usize = 6;

/// Cuts one item into parts: whole folders, and for a folder measured one level down, a
/// `top` part with only its own files.
struct Cut<'a> {
    roots: &'a HashSet<&'a Path>,
    split: &'a HashSet<PathBuf>,
    parts: Vec<Part>,
}

impl Cut<'_> {
    fn add(&mut self, dir: &Path, split: bool, depth: usize) {
        if !(split || self.split.contains(dir)) || depth >= MAX_CUT_DEPTH {
            self.parts.push(Part::whole(dir.to_path_buf()));
            return;
        }
        let mut children: Vec<PathBuf> = fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .filter(|p| !self.roots.contains(p.as_path()))
                    .collect()
            })
            .unwrap_or_default();
        children.sort();
        for c in children {
            self.add(&c, false, depth + 1);
        }
        self.parts.push(Part {
            path: dir.to_path_buf(),
            top: true,
        });
    }
}

/// One whole reading, as `scry disk` and `scry clean` take it.
pub struct Reading {
    pub layout: Layout,
    pub scan: WorktreeScan,
    pub stats: CacheStats,
    pub timings: Timings,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Timings {
    /// Finding the catalog's folders.
    pub locate_ms: f64,
    /// The worktree scan's git calls.
    pub git_ms: f64,
    /// Until every size was known, from the cache or a walk.
    pub sizes_ms: f64,
    pub total_ms: f64,
}

/// Finds every item, reads the worktrees with git, and sizes everything through `cache`.
pub fn read(home: &Path, roots: &[PathBuf], cache: &SizeCache, now: i64) -> Reading {
    let start = Instant::now();
    let found = catalog::locate(home, roots, &[]);
    let locate_ms = ms(start);
    let git = Instant::now();
    let mut scan = worktree::scan(&ScanOptions::new(home, roots.to_vec()));
    let git_ms = ms(git);
    let sizes = Instant::now();
    let split = cache.split();
    let mut layout = Layout::new(&found, Some(&scan), home, now, &split);
    let look = cache.look(&layout.parts, home);
    let (measured, stats) = cache.measure(&layout.parts, &look, home, &layout.exclude);
    let measured: Vec<Option<SubtreeSize>> = measured.into_iter().map(Some).collect();
    layout.fill(&measured, &look.held);
    let wt_sizes = layout.worktree_sizes(&scan);
    worktree::apply_sizes(&mut scan, wt_sizes);
    layout.relabel(&scan, home, now);
    let sizes_ms = ms(sizes);
    Reading {
        layout,
        scan,
        stats,
        timings: Timings {
            locate_ms,
            git_ms,
            sizes_ms,
            total_ms: ms(start),
        },
    }
}

/// The Disk view of `items`: with free space, the forecast from the daemon's samples in the
/// cache's store, and the Trash.
pub fn view(
    items: &[Item],
    cache: &SizeCache,
    paths: &Paths,
    destructive: bool,
    worktrees_pending: bool,
) -> DiskList {
    let now = crate::sys::now_ms();
    let forecast = cache.store().and_then(|s| {
        s.samples_since(SAMPLED_VOLUME, now - forecast::WINDOW_MS)
            .ok()
            .map(|samples| forecast::forecast(&samples, now))
    });
    DiskList::new(Inputs {
        items,
        space: space::read(Path::new(SAMPLED_VOLUME)).ok(),
        forecast,
        trash: Some(trash::size(&paths.user_home, cache.store())),
        destructive,
        worktrees_pending,
        home: &paths.user_home,
        config: &paths.config,
    })
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// `web/node_modules`: the project folder names it.
fn node_modules_name(path: &Path) -> String {
    let project = path
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{project}/node_modules")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_item_is_its_folders_plus_its_loose_files_and_leaves_other_items_out() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        for d in [
            "Library/Caches/Yarn/v6",
            "Library/Caches/app.one",
            "Library/Caches/app.two",
        ] {
            fs::create_dir_all(home.join(d)).unwrap();
        }
        fs::write(home.join("Library/Caches/loose"), b"x").unwrap();
        let found = catalog::locate(&home, &[], &[]);
        let layout = Layout::new(&found, None, &home, 0, &Default::default());
        let caches = home.join("Library/Caches");
        let parts: Vec<(String, bool)> = layout
            .parts
            .iter()
            .map(|p| {
                let rel = p.path.strip_prefix(&home).unwrap().display().to_string();
                (rel, p.top)
            })
            .collect();
        assert_eq!(
            parts,
            [
                ("Library/Caches/Yarn/v6".to_string(), false),
                ("Library/Caches/Yarn".into(), true),
                ("Library/Caches/app.one".into(), false),
                ("Library/Caches/app.two".into(), false),
                ("Library/Caches".into(), true),
            ]
        );
        assert!(layout.exclude.contains(&caches.join("Yarn")));
    }

    #[test]
    fn fill_sums_parts_and_waits_for_unmeasured_ones() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        fs::create_dir_all(home.join(".npm/a")).unwrap();
        let found = catalog::locate(&home, &[], &[]);
        let mut layout = Layout::new(&found, None, &home, 0, &Default::default());
        assert_eq!(layout.parts.len(), 2);
        let s = |b| {
            Some(SubtreeSize {
                size: crate::size::Size {
                    allocated: b,
                    reclaimable: b / 2,
                },
                files: 1,
                ..SubtreeSize::default()
            })
        };
        layout.fill(&[s(100), None], &[false, true]);
        assert!(layout.items[0].size.is_none() && layout.items[0].stale);
        layout.fill(&[s(100), s(10)], &[false, false]);
        let item = &layout.items[0];
        assert_eq!(item.reclaimable(), Some(55));
        assert_eq!(item.size.as_ref().unwrap().files, 2);
        assert!(item.at_least(), "reclaimable below allocated");
    }

    #[test]
    fn a_big_folder_measured_in_parts_adds_up_to_the_whole_and_keeps_its_repos() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let root = home.join("code/web/node_modules");
        for (p, n) in [("a/x", 3), ("a/b/y", 5), ("inner/z", 7), ("loose", 11)] {
            fs::create_dir_all(root.join(p).parent().unwrap()).unwrap();
            fs::write(root.join(p), vec![1u8; n * 4096]).unwrap();
        }
        fs::create_dir_all(root.join("inner/.git")).unwrap();
        fs::create_dir_all(root.join("a/b/deep/.git")).unwrap();
        let found = catalog::locate(&home, &[home.join("code")], &[]);
        let measured = |split: &HashSet<PathBuf>| {
            let mut layout = Layout::new(&found, None, &home, 0, split);
            let cache = SizeCache::new(None);
            let look = cache.look(&layout.parts, &home);
            let (sizes, _) = cache.measure(&layout.parts, &look, &home, &layout.exclude);
            let sizes: Vec<_> = sizes.into_iter().map(Some).collect();
            layout.fill(&sizes, &vec![false; sizes.len()]);
            (layout.parts.len(), layout.items[0].size.clone().unwrap())
        };
        let (n_whole, whole) = measured(&HashSet::new());
        let split = HashSet::from([root.clone(), root.join("a")]);
        let (n_cut, cut) = measured(&split);
        assert_eq!(n_whole, 1);
        // a/b, a (top), inner, node_modules (top)
        assert_eq!(n_cut, 4);
        assert_eq!(cut.size, whole.size);
        assert_eq!(cut.files, whole.files);
        assert_eq!(cut.dirs, whole.dirs);
        assert_eq!(whole.repos, [PathBuf::from("a/b/deep"), "inner".into()]);
        assert_eq!(cut.repos, whole.repos);
    }
}
