//! The disk catalog: the places developer tools fill, each with what deleting it costs and
//! the one cleanup Scry offers for it. A reviewed table in [`CATALOG`]; nothing outside it
//! and the code roots is ever sized, and Desktop, Documents and Downloads are never entered.
//!
//! Cleanup types:
//! - `trash`: moved to the Trash, so it can be put back. For things that cannot be rebuilt.
//! - `delete`: removed for good after typing the item's name. Only for caches a tool fills
//!   again by itself.
//! - `command`: the tool's own cleanup, shown for you to run. Scry runs nothing.
//! - `never`: shown for its size only.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use serde::{Deserialize, Serialize};

use crate::protected::{Protected, Resolved};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Xcode,
    Simulators,
    Docker,
    NodeModules,
    PackageCache,
    AppCaches,
    AgentLogs,
    Worktree,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Xcode => "xcode",
            Kind::Simulators => "simulators",
            Kind::Docker => "docker",
            Kind::NodeModules => "node_modules",
            Kind::PackageCache => "package cache",
            Kind::AppCaches => "app caches",
            Kind::AgentLogs => "agent logs",
            Kind::Worktree => "worktree",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    Trash,
    Delete,
    Command(&'static str),
    Never,
}

impl Cleanup {
    pub fn word(self) -> &'static str {
        match self {
            Cleanup::Trash => "trash",
            Cleanup::Delete => "delete",
            Cleanup::Command(_) => "command",
            Cleanup::Never => "never",
        }
    }
}

/// Where an entry's folders are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// Under the home folder. The first of these that exists is the entry's folder.
    Home(&'static [&'static str]),
    /// Folders with this name under the code roots, outside any other match.
    CodeRoots(&'static str),
    /// Linked git worktrees, from the worktree scan.
    Worktrees,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// Stable, and the item id of a home entry: `scry clean xcode-derived-data`.
    pub id: &'static str,
    pub name: &'static str,
    pub kind: Kind,
    pub place: Place,
    /// The tool that fills it.
    pub creator: &'static str,
    /// What deleting it costs, in one sentence.
    pub cost: &'static str,
    pub cleanup: Cleanup,
    /// Its files are often clones or hard links of files elsewhere, so what a delete frees
    /// at once is only a lower bound.
    pub shared: bool,
    /// Measured folder by folder one level down, so a change in one app's cache rereads that
    /// folder alone.
    pub split: bool,
}

const fn home(
    id: &'static str,
    name: &'static str,
    kind: Kind,
    paths: &'static [&'static str],
    creator: &'static str,
    cost: &'static str,
    cleanup: Cleanup,
) -> Entry {
    Entry {
        id,
        name,
        kind,
        place: Place::Home(paths),
        creator,
        cost,
        cleanup,
        shared: false,
        split: true,
    }
}

const fn shared(mut e: Entry) -> Entry {
    e.shared = true;
    e
}

/// Reviewed by hand. Order only breaks ties in a ranking; a folder inside another entry's
/// folder (Yarn's cache inside `~/Library/Caches`) is that entry's alone.
pub static CATALOG: &[Entry] = &[
    home(
        "xcode-derived-data",
        "Xcode DerivedData",
        Kind::Xcode,
        &["Library/Developer/Xcode/DerivedData"],
        "Xcode",
        "Build products and indexes; the next build in Xcode makes them again, slower the first time.",
        Cleanup::Delete,
    ),
    home(
        "xcode-archives",
        "Xcode Archives",
        Kind::Xcode,
        &["Library/Developer/Xcode/Archives"],
        "Xcode",
        "Archived app builds with the debug symbols that read their crash reports; they cannot be built again exactly.",
        Cleanup::Trash,
    ),
    home(
        "xcode-device-support",
        "Xcode device support",
        Kind::Xcode,
        &["Library/Developer/Xcode/iOS DeviceSupport"],
        "Xcode",
        "Symbols copied from each device and OS version; Xcode copies them again when that device connects.",
        Cleanup::Delete,
    ),
    home(
        "core-simulator",
        "Simulator devices",
        Kind::Simulators,
        &["Library/Developer/CoreSimulator"],
        "Xcode",
        "Simulated devices with their apps and data; the command removes only devices whose runtime is no longer installed.",
        Cleanup::Command("xcrun simctl delete unavailable"),
    ),
    home(
        "docker-raw",
        "Docker.raw",
        Kind::Docker,
        &["Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw"],
        "Docker Desktop",
        "Docker's disk with every image, container and volume; deleting the file loses all of them. `docker system prune` frees space inside it.",
        Cleanup::Never,
    ),
    shared(home(
        "pnpm-store",
        "pnpm store",
        Kind::PackageCache,
        &[
            "Library/pnpm/store",
            ".local/share/pnpm/store",
            ".pnpm-store",
        ],
        "pnpm",
        "Packages that projects' node_modules link into; pruning drops those no project uses, and deleting it makes every install download again.",
        Cleanup::Command("pnpm store prune"),
    )),
    home(
        "npm-cache",
        "npm cache",
        Kind::PackageCache,
        &[".npm"],
        "npm",
        "Packages npm downloaded; the next install fetches them again.",
        Cleanup::Delete,
    ),
    home(
        "yarn-cache",
        "Yarn cache",
        Kind::PackageCache,
        &["Library/Caches/Yarn", ".yarn/berry/cache"],
        "Yarn",
        "Packages Yarn downloaded; the next install fetches them again.",
        Cleanup::Delete,
    ),
    home(
        "bun-cache",
        "Bun cache",
        Kind::PackageCache,
        &[".bun/install/cache"],
        "Bun",
        "Packages Bun downloaded; the next install fetches them again.",
        Cleanup::Delete,
    ),
    home(
        "cargo-registry",
        "Cargo registry",
        Kind::PackageCache,
        &[".cargo/registry"],
        "Cargo",
        "Downloaded crates and their unpacked sources; the next build downloads what it needs.",
        Cleanup::Delete,
    ),
    home(
        "pip-cache",
        "pip cache",
        Kind::PackageCache,
        &["Library/Caches/pip"],
        "pip",
        "Wheels pip downloaded or built; the next install fetches or builds them again.",
        Cleanup::Delete,
    ),
    shared(home(
        "uv-cache",
        "uv cache",
        Kind::PackageCache,
        &[".cache/uv"],
        "uv",
        "Packages uv downloaded and unpacked, often cloned into virtual environments; the next sync fetches them again.",
        Cleanup::Delete,
    )),
    home(
        "homebrew-cache",
        "Homebrew cache",
        Kind::PackageCache,
        &["Library/Caches/Homebrew"],
        "Homebrew",
        "Downloaded bottles and sources; the command keeps those of installed versions.",
        Cleanup::Command("brew cleanup --prune=all"),
    ),
    home(
        "claude-projects",
        "Claude Code sessions",
        Kind::AgentLogs,
        &[".claude/projects"],
        "Claude Code",
        "Transcripts of past Claude Code sessions; without them those sessions cannot be resumed or searched.",
        Cleanup::Trash,
    ),
    home(
        "codex-sessions",
        "Codex sessions",
        Kind::AgentLogs,
        &[".codex/sessions"],
        "Codex",
        "Logs of past Codex sessions; without them those sessions cannot be resumed.",
        Cleanup::Trash,
    ),
    home(
        "app-caches",
        "App caches",
        Kind::AppCaches,
        &["Library/Caches"],
        "apps",
        "Caches that apps write while they run; deleting one under a running app can break it, so clear it from inside the app.",
        Cleanup::Never,
    ),
    Entry {
        id: "node_modules",
        name: "node_modules",
        kind: Kind::NodeModules,
        place: Place::CodeRoots("node_modules"),
        creator: "package manager",
        cost: "One project's installed packages; the package manager installs them again from its lockfile.",
        cleanup: Cleanup::Delete,
        shared: true,
        split: false,
    },
    Entry {
        id: "worktree",
        name: "worktree",
        kind: Kind::Worktree,
        place: Place::Worktrees,
        creator: "git",
        cost: "A linked git worktree; see its state for what removing it would lose.",
        cleanup: Cleanup::Trash,
        shared: true,
        split: false,
    },
];

pub fn entry(id: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|e| e.id == id)
}

/// The catalog entry a folder belongs to, by where it is. The deepest match wins, so a
/// package cache inside `~/Library/Caches` is the cache's, not the app caches'.
pub fn match_path(path: &Path, home: &Path, roots: &[PathBuf]) -> Option<&'static Entry> {
    let mut best: Option<(&'static Entry, usize)> = None;
    for e in CATALOG {
        let depth = match e.place {
            Place::Home(paths) => paths
                .iter()
                .map(|p| home.join(p))
                .filter(|p| path.starts_with(p))
                .map(|p| p.components().count())
                .max(),
            Place::CodeRoots(name) => roots
                .iter()
                .any(|r| path.starts_with(r))
                .then(|| {
                    path.ancestors()
                        .find(|a| a.file_name().is_some_and(|n| n == name))
                        .map(|a| a.components().count())
                })
                .flatten(),
            Place::Worktrees => None,
        };
        if let Some(d) = depth
            && best.is_none_or(|(_, b)| d > b)
        {
            best = Some((e, d));
        }
    }
    best.map(|(e, _)| e)
}

/// A folder (or file) the catalog matched on this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub entry: &'static Entry,
    /// The item id: the entry's id for a home entry, `node_modules:<path>` for one under
    /// the code roots.
    pub id: String,
    pub path: PathBuf,
    pub is_file: bool,
}

/// How deep under a code root `node_modules` folders are looked for. A monorepo keeps them
/// in `packages/*/`, three or four folders down.
pub const NODE_MODULES_DEPTH: usize = 6;

/// Every home entry that exists, and every `node_modules` under the code roots. Symlinks
/// are not followed, protected folders are never entered, hidden folders and worktrees
/// are skipped (a worktree is an item of its own), and nothing inside a `node_modules` is
/// listed.
pub fn locate(home: &Path, roots: &[PathBuf], worktrees: &[PathBuf]) -> Vec<Found> {
    let guard = Protected::new(home);
    let mut out = Vec::new();
    for e in CATALOG {
        if let Place::Home(paths) = e.place {
            for p in paths {
                let path = home.join(p);
                // The last component is not followed: a symlinked cache is someone else's.
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.file_type().is_symlink() {
                    continue;
                }
                if guard.resolve(&path).outside().is_none() {
                    continue;
                }
                out.push(Found {
                    entry: e,
                    id: e.id.to_string(),
                    path,
                    is_file: meta.is_file(),
                });
                break;
            }
        }
    }
    let node = entry("node_modules").expect("in the catalog");
    let skip: Vec<&Path> = worktrees.iter().map(PathBuf::as_path).collect();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let Resolved::Outside(root) = guard.resolve(root) else {
            continue;
        };
        if !seen.insert(root.clone()) {
            continue;
        }
        let mut found = find_named(
            &root,
            "node_modules",
            NODE_MODULES_DEPTH,
            &guard,
            home,
            &skip,
        );
        found.sort();
        out.extend(found.into_iter().map(|path| Found {
            entry: node,
            id: format!("node_modules:{}", path.display()),
            path,
            is_file: false,
        }));
    }
    out
}

/// Folders called `name` under `root`, up to `max` folders down, read by several threads:
/// a code root holds tens of thousands of folders.
fn find_named(
    root: &Path,
    name: &str,
    max: usize,
    guard: &Protected,
    home: &Path,
    skip: &[&Path],
) -> Vec<PathBuf> {
    struct Jobs {
        todo: Vec<(PathBuf, usize)>,
        busy: usize,
    }
    let jobs = Mutex::new(Jobs {
        todo: vec![(root.to_path_buf(), 0)],
        busy: 0,
    });
    let ready = Condvar::new();
    let found = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 8));
    let work = || {
        loop {
            let (dir, depth) = {
                let mut j = jobs.lock().unwrap();
                loop {
                    if let Some(job) = j.todo.pop() {
                        j.busy += 1;
                        break job;
                    }
                    if j.busy == 0 {
                        return;
                    }
                    j = ready.wait(j).unwrap();
                }
            };
            let mut next = Vec::new();
            let mut hits = Vec::new();
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                // From the directory entry, so a symlink is never taken for a folder.
                if !e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let file_name = e.file_name();
                let path = e.path();
                if file_name == name {
                    hits.push(path);
                    continue;
                }
                let hidden = file_name.as_encoded_bytes().first() == Some(&b'.');
                if hidden || depth >= max || file_name == "target" || skip.contains(&path.as_path())
                {
                    continue;
                }
                // A real folder is a protected one only as the home folder's own child, and
                // reading its attributes asks nothing of macOS.
                let protected = dir == home
                    && e.metadata()
                        .is_ok_and(|m| guard.folder(crate::protected::file_id(&m)).is_some());
                if !protected {
                    next.push((path, depth + 1));
                }
            }
            found.lock().unwrap().extend(hits);
            let mut j = jobs.lock().unwrap();
            j.busy -= 1;
            let wake = !next.is_empty() || j.busy == 0;
            j.todo.extend(next);
            if wake {
                ready.notify_all();
            }
        }
    };
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(work);
        }
    });
    found.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_has_a_unique_id_and_a_reviewed_cleanup() {
        let mut ids: Vec<&str> = CATALOG.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CATALOG.len());
        for e in CATALOG {
            assert!(e.cost.ends_with('.'), "{}", e.id);
            if let Cleanup::Command(c) = e.cleanup {
                assert!(!c.is_empty());
            }
        }
        // Docker's disk is never touched by a cleanup.
        let docker = entry("docker-raw").unwrap();
        assert_eq!(docker.cleanup, Cleanup::Never);
        assert!(matches!(docker.place, Place::Home(p) if p[0].ends_with("Docker.raw")));
        // Agent logs cannot be made again, so they go to the Trash, never a delete.
        for e in CATALOG.iter().filter(|e| e.kind == Kind::AgentLogs) {
            assert_eq!(e.cleanup, Cleanup::Trash, "{}", e.id);
        }
    }

    #[test]
    fn the_deepest_entry_matches_a_path() {
        let home = Path::new("/Users/dev");
        let roots = [home.join("sources")];
        let m = |p: &str| match_path(&home.join(p), home, &roots).map(|e| e.id);
        assert_eq!(m("Library/Caches/Yarn/v6/npm-x"), Some("yarn-cache"));
        assert_eq!(m("Library/Caches/com.example.app"), Some("app-caches"));
        assert_eq!(m("Library/Caches/Homebrew"), Some("homebrew-cache"));
        assert_eq!(
            m("Library/Developer/Xcode/DerivedData/App-abc"),
            Some("xcode-derived-data")
        );
        assert_eq!(
            m("sources/acme/web/node_modules/react"),
            Some("node_modules")
        );
        assert_eq!(m(".claude/projects/-x"), Some("claude-projects"));
        assert_eq!(m("Documents/notes"), None);
        assert_eq!(m("sources/acme/src"), None);
        assert_eq!(
            match_path(Path::new("/opt/node_modules/x"), home, &roots),
            None,
            "node_modules only under a code root"
        );
    }

    #[test]
    fn locate_finds_home_entries_and_node_modules_but_never_looks_in_protected_folders() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let mk = |p: &str| fs::create_dir_all(home.join(p)).unwrap();
        mk("Library/Caches/Yarn");
        mk("Library/Caches/com.example.app");
        mk(".npm/_cacache");
        mk(".local/share/pnpm/store/v3");
        mk("code/app/node_modules/react/node_modules/nested");
        mk("code/mono/packages/ui/node_modules/x");
        mk("code/mono/.git/node_modules");
        mk("code/app/wt/node_modules");
        mk("code/deep/a/b/c/d/e/f/node_modules");
        mk("code/link-target/node_modules");
        mk("Documents/proj/node_modules");
        std::os::unix::fs::symlink(home.join("Documents"), home.join("code/docs")).unwrap();
        std::os::unix::fs::symlink(home.join("code/link-target"), home.join("code/linked"))
            .unwrap();
        // A symlinked cache folder belongs to whatever it points at.
        std::os::unix::fs::symlink(home.join("code"), home.join(".bun")).unwrap();

        let found = locate(
            &home,
            &[home.join("code"), home.join("Documents")],
            &[home.join("code/app/wt")],
        );
        let ids: Vec<String> = found
            .iter()
            .map(|f| f.id.replace(&format!("{}/", home.display()), ""))
            .collect();
        assert_eq!(
            ids,
            [
                "pnpm-store",
                "npm-cache",
                "yarn-cache",
                "app-caches",
                "node_modules:code/app/node_modules",
                "node_modules:code/link-target/node_modules",
                "node_modules:code/mono/packages/ui/node_modules",
            ]
        );
        assert_eq!(found[0].path, home.join(".local/share/pnpm/store"));
    }
}
