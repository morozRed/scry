//! Finds repositories under the code roots and agent folders, lists their linked worktrees
//! with git, reads each one's state and sizes them all in one walk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::creator::{self, CreatorSource};
use super::{
    Changes, GitFacts, Head, Lock, Merge, Orphan, Timings, Upstream, Worktree, WorktreeScan,
    WorktreeState,
};
use crate::git::{self, ListedWorktree, Ref};
use crate::size::{PROTECTED, Walker};

pub struct ScanOptions {
    pub home: PathBuf,
    pub roots: Vec<PathBuf>,
    /// Folders where agents keep worktrees outside any repo.
    pub agent_dirs: Vec<PathBuf>,
    /// How many folders below a root a repo may sit.
    pub max_depth: usize,
    pub git_timeout: Duration,
    /// git calls in flight at once. They mostly wait on the disk, so one per core.
    pub threads: usize,
    pub creators: Vec<Box<dyn CreatorSource>>,
}

impl ScanOptions {
    pub fn new(home: &Path, roots: Vec<PathBuf>) -> Self {
        ScanOptions {
            home: home.to_path_buf(),
            roots,
            agent_dirs: vec![home.join(".codex/worktrees")],
            max_depth: 4,
            git_timeout: Duration::from_secs(5),
            threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
            creators: creator::default_sources(home),
        }
    }
}

/// The code roots from PLAN.md that exist in `home`.
pub fn default_roots(home: &Path) -> Vec<PathBuf> {
    ["sources", "code", "src", "Developer"]
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

pub fn scan(opts: &ScanOptions) -> WorktreeScan {
    let start = Instant::now();
    let mut warnings = Vec::new();
    let protected: Vec<PathBuf> = PROTECTED.iter().map(|d| opts.home.join(d)).collect();

    let mut found = Found::default();
    for root in &opts.roots {
        let root = canonical(root);
        if protected.iter().any(|p| root.starts_with(canonical(p))) {
            warnings.push(format!("{} is never scanned", root.display()));
            continue;
        }
        find(&root, 0, opts.max_depth, &protected, &mut found);
    }
    for dir in &opts.agent_dirs {
        find(&canonical(dir), 0, 2, &protected, &mut found);
    }
    // Agent worktree folders inside each checkout hold leftovers git may no longer know.
    for checkout in found.checkouts.clone() {
        let Ok(entries) = std::fs::read_dir(checkout.join(".claude/worktrees")) else {
            continue;
        };
        for e in entries.flatten() {
            if e.path().join(".git").is_file() {
                found.pointers.push(e.path());
            }
        }
    }
    found.pointers.sort();
    found.pointers.dedup();
    let discover_ms = ms(start);

    let git_start = Instant::now();
    let timeout = opts.git_timeout;
    let pointers = par_map(&found.pointers, opts.threads, |dir| classify(dir, timeout));

    let mut commons: Vec<PathBuf> = found
        .checkouts
        .iter()
        .map(|c| canonical(&c.join(".git")))
        .collect();
    let mut orphans = Vec::new();
    for (dir, kind) in found.pointers.iter().zip(pointers) {
        match kind {
            Pointer::Linked { common } | Pointer::Checkout { common } => {
                commons.push(canonical(&common))
            }
            Pointer::Submodule => {}
            Pointer::Broken { gitdir } => orphans.push((dir.clone(), Ok(gitdir))),
            Pointer::Unreadable { error } => orphans.push((dir.clone(), Err(error))),
        }
    }
    commons.sort();
    commons.dedup();

    let repos = par_map(&commons, opts.threads, |c| list_repo(c, timeout));
    let mut tasks = Vec::new();
    for repo in &repos {
        if let Some(e) = &repo.error {
            warnings.push(format!("{}: {e}", repo.common.display()));
        }
        tasks.extend(repo.linked.iter().map(|w| (repo, w)));
    }
    let states = par_map(&tasks, opts.threads, |(repo, w)| {
        read_state(repo, w, timeout)
    });
    let mut worktrees: Vec<Worktree> = tasks
        .iter()
        .zip(states)
        .map(|((repo, w), state)| worktree(repo, w, state, &opts.creators))
        .collect();

    let mut seen: HashSet<PathBuf> = worktrees.iter().map(|w| w.path.clone()).collect();
    for (dir, pointer) in orphans {
        let dir = canonical(&dir);
        if !seen.insert(dir.clone()) {
            continue;
        }
        let gitdir = git::read_gitdir_file(&dir.join(".git"));
        let (state, repo) = match pointer {
            Ok(gitdir) => (
                WorktreeState::Orphaned(Orphan::BrokenPointer {
                    gitdir: gitdir.clone(),
                }),
                repo_from_gitdir(&gitdir),
            ),
            Err(error) => (
                WorktreeState::Unreadable { error },
                gitdir.map_or(dir.clone(), |g| repo_from_gitdir(&g)),
            ),
        };
        worktrees.push(Worktree {
            creator: creator::attribute(&opts.creators, &dir, None),
            path: dir,
            repo,
            head: Head::Unknown,
            state,
            lock: None,
            size: None,
            created_at: None,
        });
    }

    worktrees.sort_by(|a, b| (&a.repo, &a.path).cmp(&(&b.repo, &b.path)));
    WorktreeScan {
        worktrees,
        roots: opts.roots.clone(),
        repos: commons.len(),
        warnings,
        timings: Timings {
            discover_ms,
            git_ms: ms(git_start),
            size_ms: 0.0,
            total_ms: ms(start),
        },
    }
}

/// Sizes every worktree whose folder exists, in one walk. A worktree nested inside another
/// counts only toward itself.
pub fn measure(scan: &mut WorktreeScan, home: &Path) {
    let start = Instant::now();
    let on_disk: Vec<usize> = (0..scan.worktrees.len())
        .filter(|&i| scan.worktrees[i].path.is_dir())
        .collect();
    let roots: Vec<PathBuf> = on_disk
        .iter()
        .map(|&i| scan.worktrees[i].path.clone())
        .collect();
    let sizes = Walker::new(home).exclude(roots.clone()).measure(&roots);
    for (i, size) in on_disk.into_iter().zip(sizes) {
        scan.worktrees[i].size = Some(size);
    }
    scan.timings.size_ms = ms(start);
    scan.timings.total_ms += scan.timings.size_ms;
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[derive(Default)]
struct Found {
    /// Folders whose `.git` is a folder: main checkouts.
    checkouts: Vec<PathBuf>,
    /// Folders whose `.git` is a file: a linked worktree, a submodule, or a broken pointer.
    pointers: Vec<PathBuf>,
}

/// Build output and dependencies hold no repos worth the walk.
const SKIP: [&str; 2] = ["node_modules", "target"];

/// Collects repos up to `max` folders down. It stops at a checkout: git lists the worktrees
/// and submodules inside one, and the walk would otherwise read every source folder.
fn find(dir: &Path, depth: usize, max: usize, protected: &[PathBuf], found: &mut Found) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for e in entries.flatten() {
        let Ok(kind) = e.file_type() else { continue };
        let name = e.file_name();
        if name == ".git" {
            if kind.is_dir() {
                found.checkouts.push(dir.to_path_buf());
            } else if kind.is_file() {
                found.pointers.push(dir.to_path_buf());
            }
            return;
        }
        let name = name.to_string_lossy();
        if depth < max && kind.is_dir() && !name.starts_with('.') && !SKIP.contains(&&*name) {
            subdirs.push(e.path());
        }
    }
    for d in subdirs {
        if !protected.contains(&d) {
            find(&d, depth + 1, max, protected, found);
        }
    }
}

enum Pointer {
    Linked {
        common: PathBuf,
    },
    /// A main checkout whose git folder lives elsewhere (`git init --separate-git-dir`).
    Checkout {
        common: PathBuf,
    },
    Submodule,
    Broken {
        gitdir: PathBuf,
    },
    Unreadable {
        error: String,
    },
}

/// A `.git` file can mean a linked worktree or a submodule; git says which.
fn classify(dir: &Path, timeout: Duration) -> Pointer {
    let args = [
        "rev-parse",
        "--path-format=absolute",
        "--git-dir",
        "--git-common-dir",
        "--show-superproject-working-tree",
    ];
    match git::stdout(dir, &args, timeout) {
        Ok(out) => match out.lines().collect::<Vec<_>>().as_slice() {
            [_, _, superproject, ..] if !superproject.is_empty() => Pointer::Submodule,
            [git_dir, common, ..] if git_dir != common => Pointer::Linked {
                common: common.into(),
            },
            [_, common, ..] => Pointer::Checkout {
                common: common.into(),
            },
            _ => Pointer::Unreadable {
                error: format!("unexpected rev-parse output {out:?}"),
            },
        },
        Err(e) => match git::read_gitdir_file(&dir.join(".git")) {
            Some(gitdir) if !gitdir.exists() => Pointer::Broken { gitdir },
            _ => Pointer::Unreadable {
                error: e.to_string(),
            },
        },
    }
}

/// `/x/acme/.git/worktrees/wt` belongs to the checkout `/x/acme`.
fn repo_from_gitdir(gitdir: &Path) -> PathBuf {
    let mut p = gitdir;
    if p.parent().and_then(Path::file_name) == Some("worktrees".as_ref()) {
        p = p.parent().and_then(Path::parent).unwrap_or(p);
    }
    if p.file_name() == Some(".git".as_ref()) {
        p = p.parent().unwrap_or(p);
    }
    p.to_path_buf()
}

struct Repo {
    common: PathBuf,
    main: Option<PathBuf>,
    linked: Vec<ListedWorktree>,
    default: Option<DefaultBranch>,
    refs: HashMap<String, Ref>,
    /// Worktree HEADs merged into the default branch, to the ref that holds them.
    merged: HashMap<String, String>,
    commit_times: HashMap<String, i64>,
    /// Worktree path to its folder under `.git/worktrees`.
    admin: HashMap<PathBuf, PathBuf>,
    error: Option<String>,
}

fn list_repo(common: &Path, timeout: Duration) -> Repo {
    let mut repo = Repo {
        common: common.to_path_buf(),
        main: (common.file_name() == Some(".git".as_ref()))
            .then(|| common.parent().map(Path::to_path_buf))
            .flatten(),
        linked: Vec::new(),
        default: None,
        refs: HashMap::new(),
        merged: HashMap::new(),
        commit_times: HashMap::new(),
        admin: admin_dirs(common),
        error: None,
    };
    // git keeps every linked worktree under `.git/worktrees`; without one there is nothing
    // to ask git about.
    if repo.admin.is_empty() {
        return repo;
    }
    let listed = match git::stdout(common, &["worktree", "list", "--porcelain", "-z"], timeout) {
        Ok(out) => git::parse_worktree_list(&out),
        Err(e) => {
            repo.error = Some(e.to_string());
            return repo;
        }
    };
    let refs_args = [
        "for-each-ref",
        git::REF_FORMAT,
        "refs/heads",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
    ];
    match git::stdout(common, &refs_args, timeout) {
        Ok(out) => {
            repo.refs = git::parse_refs(&out)
                .into_iter()
                .map(|r| (r.name.clone(), r))
                .collect()
        }
        Err(e) => repo.error = Some(e.to_string()),
    }
    let mut listed = listed.into_iter();
    let main = listed.next();
    let main_branch = main.as_ref().and_then(|m| m.branch.clone());
    if let Some(m) = main.filter(|m| !m.bare) {
        repo.main = Some(m.path);
    }
    repo.linked = listed.filter(|w| !w.bare).collect();
    repo.default = default_branch(&repo.refs, main_branch.as_deref());

    let mut heads: Vec<String> = repo
        .linked
        .iter()
        .filter(|w| w.prunable.is_none() && w.path.exists())
        .filter_map(|w| w.head.clone())
        .collect();
    heads.sort();
    heads.dedup();
    if heads.is_empty() {
        return repo;
    }
    if let Some(d) = &repo.default {
        match merged_heads(common, d, &heads, timeout) {
            Ok(m) => repo.merged = m,
            Err(e) => repo.error = Some(e.to_string()),
        }
    }
    match commit_times(common, &heads, timeout) {
        Ok(t) => repo.commit_times = t,
        Err(e) => repo.error = Some(e.to_string()),
    }
    repo
}

fn admin_dirs(common: &Path) -> HashMap<PathBuf, PathBuf> {
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join("gitdir")).ok()?;
            let dot_git = PathBuf::from(text.trim_end());
            Some((dot_git.parent()?.to_path_buf(), e.path()))
        })
        .collect()
}

struct DefaultBranch {
    /// Display name and full ref, remote first.
    refs: Vec<(String, String)>,
}

/// `origin/HEAD` names the default branch; without it, `main` or `master`; without those,
/// the branch the main checkout is on.
fn default_branch(refs: &HashMap<String, Ref>, main_branch: Option<&str>) -> Option<DefaultBranch> {
    let has = |b: &str| {
        refs.contains_key(&format!("refs/heads/{b}"))
            || refs.contains_key(&format!("refs/remotes/origin/{b}"))
    };
    let name = refs
        .get("refs/remotes/origin/HEAD")
        .and_then(|r| r.symref.as_deref())
        .and_then(|s| s.strip_prefix("refs/remotes/origin/"))
        .map(str::to_string)
        .or_else(|| {
            ["main", "master"]
                .into_iter()
                .find(|b| has(b))
                .map(Into::into)
        })
        .or_else(|| main_branch.map(Into::into))?;
    let refs: Vec<_> = [
        (
            format!("origin/{name}"),
            format!("refs/remotes/origin/{name}"),
        ),
        (name.clone(), format!("refs/heads/{name}")),
    ]
    .into_iter()
    .filter(|(_, full)| refs.contains_key(full))
    .collect();
    (!refs.is_empty()).then_some(DefaultBranch { refs })
}

fn head(w: &ListedWorktree) -> Head {
    match (&w.branch, &w.head) {
        (Some(name), Some(oid)) => Head::Branch {
            name: name.clone(),
            oid: oid.clone(),
        },
        (None, Some(oid)) => Head::Detached { oid: oid.clone() },
        _ => Head::Unknown,
    }
}

fn read_state(repo: &Repo, w: &ListedWorktree, timeout: Duration) -> WorktreeState {
    let admin_dir = repo.admin.get(&w.path).cloned();
    let git_says = w.prunable.clone();
    if !w.path.exists() {
        return WorktreeState::Orphaned(Orphan::Missing {
            admin_dir,
            git_says,
        });
    }
    if w.prunable.is_some() || !w.path.join(".git").exists() {
        return WorktreeState::Orphaned(Orphan::Unusable {
            admin_dir,
            git_says,
        });
    }
    match git_facts(repo, &w.path, &head(w), timeout) {
        Ok(facts) => WorktreeState::Present(facts),
        Err(e) => WorktreeState::Unreadable {
            error: e.to_string(),
        },
    }
}

fn worktree(
    repo: &Repo,
    w: &ListedWorktree,
    state: WorktreeState,
    creators: &[Box<dyn CreatorSource>],
) -> Worktree {
    let path = canonical(&w.path);
    let admin = repo.admin.get(&w.path).cloned();
    Worktree {
        creator: creator::attribute(creators, &path, w.locked.as_deref()),
        path,
        repo: repo.main.clone().unwrap_or_else(|| repo.common.clone()),
        head: head(w),
        state,
        lock: w.locked.as_ref().map(|r| Lock {
            reason: (!r.is_empty()).then(|| r.clone()),
        }),
        size: None,
        created_at: admin.and_then(|a| {
            let born = std::fs::metadata(a).ok()?.created().ok()?;
            Some(born.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
        }),
    }
}

/// The one git call made per worktree; the rest is read per repo in `list_repo`.
fn git_facts(repo: &Repo, path: &Path, head: &Head, timeout: Duration) -> anyhow::Result<GitFacts> {
    let (changed, untracked) =
        git::count_status(&git::stdout(path, &["status", "--porcelain"], timeout)?);
    let changes = if changed + untracked == 0 {
        Changes::Clean
    } else {
        Changes::Dirty { changed, untracked }
    };
    let branch_ref = match head {
        Head::Branch { name, .. } => repo.refs.get(&format!("refs/heads/{name}")),
        _ => None,
    };
    let upstream = match branch_ref.and_then(|r| Some((r.upstream.as_deref()?, r.upstream_gone))) {
        None => Upstream::None,
        Some((up, gone)) => {
            let name = up
                .strip_prefix("refs/remotes/")
                .or_else(|| up.strip_prefix("refs/heads/"))
                .unwrap_or(up)
                .to_string();
            if gone {
                Upstream::Gone { name }
            } else {
                Upstream::Tracking { name }
            }
        }
    };
    let oid = head.oid().unwrap_or_default();
    let merge = match (&repo.default, repo.merged.get(oid)) {
        (_, Some(into)) => Merge::Merged { into: into.clone() },
        (Some(d), None) => Merge::Unmerged {
            into: d.refs[0].0.clone(),
        },
        (None, None) => Merge::NoDefaultBranch,
    };
    Ok(GitFacts {
        changes,
        merge,
        upstream,
        last_commit: repo.commit_times.get(oid).copied(),
    })
}

/// Maps each head reachable from a default ref to that ref's name. `rev-list <heads> --not
/// <ref>` prints the commits the heads add on top of the ref, so a head absent from its
/// output adds nothing: it is merged.
fn merged_heads(
    common: &Path,
    default: &DefaultBranch,
    heads: &[String],
    timeout: Duration,
) -> anyhow::Result<HashMap<String, String>> {
    let mut merged = HashMap::new();
    for (show, full) in &default.refs {
        let left: Vec<&str> = heads
            .iter()
            .map(String::as_str)
            .filter(|h| !merged.contains_key(*h))
            .collect();
        if left.is_empty() {
            break;
        }
        let mut args = vec!["rev-list"];
        args.extend(&left);
        args.extend(["--not", full.as_str()]);
        let out = git::stdout(common, &args, timeout)?;
        let added: HashSet<&str> = out.lines().collect();
        for h in left.into_iter().filter(|h| !added.contains(h)) {
            merged.insert(h.to_string(), show.clone());
        }
    }
    Ok(merged)
}

fn commit_times(
    common: &Path,
    heads: &[String],
    timeout: Duration,
) -> anyhow::Result<HashMap<String, i64>> {
    let mut args = vec!["log", "--no-walk=unsorted", "--format=%H %ct"];
    args.extend(heads.iter().map(String::as_str));
    let out = git::stdout(common, &args, timeout)?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (oid, t) = l.split_once(' ')?;
            Some((oid.to_string(), t.parse().ok()?))
        })
        .collect())
}

/// Runs `f` over `items` on up to `threads` threads, keeping order.
fn par_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, R)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.clamp(1, items.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        mine.push((i, f(item)));
                    }
                    mine
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|h| h.join().expect("scan worker panicked"))
            .collect()
    });
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}
