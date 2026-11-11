//! Finds repositories under the code roots and agent folders, reads git's record of each
//! linked worktree, looks at each folder once with one `git status`, and sizes them all in
//! one walk. Nothing inside a protected folder is read, and no git runs there.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::creator::{self, CreatorSource};
use super::verdict::{Reading, judge};
use super::{
    Changes, GitFacts, Head, Lock, Merge, NotRead, Timings, Upstream, Worktree, WorktreeScan,
    WorktreeState,
};
use crate::git::{self, Ref};
use crate::protected::{Protected, Resolved, file_id};
use crate::size::Walker;

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

pub fn scan(opts: &ScanOptions) -> WorktreeScan {
    let start = Instant::now();
    let guard = Protected::new(&opts.home);
    let mut warnings = Vec::new();

    let mut found = Found::default();
    for root in &opts.roots {
        match guard.resolve(root) {
            Resolved::Inside(folder) => warnings.push(format!(
                "{} was not read: protected folder {}",
                root.display(),
                folder.display()
            )),
            Resolved::Outside(root) => find(&root, 0, opts.max_depth, &guard, &mut found),
        }
    }
    for dir in &opts.agent_dirs {
        if let Resolved::Outside(dir) = guard.resolve(dir) {
            find(&dir, 0, 2, &guard, &mut found);
        }
    }
    // Agent worktree folders inside each checkout hold leftovers git may no longer know.
    for checkout in found.checkouts.clone() {
        let Resolved::Outside(dir) = guard.resolve(&checkout.join(".claude/worktrees")) else {
            continue;
        };
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && fs::symlink_metadata(e.path().join(".git")).is_ok_and(|m| m.is_file()) {
                found.pointers.push(e.path());
            }
        }
    }
    found.pointers.sort();
    found.pointers.dedup();
    let discover_ms = ms(start);

    let git_start = Instant::now();
    let timeout = opts.git_timeout;
    let mut commons: Vec<PathBuf> = found.checkouts.iter().map(|c| c.join(".git")).collect();
    let mut loose = Vec::new();
    for dir in &found.pointers {
        match link(&guard, dir) {
            Link::Worktree { common } | Link::Repo { gitdir: common } => commons.push(common),
            other => loose.push((dir.clone(), other)),
        }
    }
    commons.sort();
    commons.dedup();

    let repos = par_map(&commons, opts.threads, |c| read_repo(c, timeout));
    let mut tasks: Vec<(usize, &Record)> = Vec::new();
    for (i, repo) in repos.iter().enumerate() {
        if let Some(e) = &repo.error {
            warnings.push(format!("{}: {e}", repo.common.display()));
        }
        tasks.extend(repo.records.iter().map(|r| (i, r)));
    }
    let looks = par_map(&tasks, opts.threads, |(_, rec)| look(rec, &guard, timeout));

    // Read after the looks, so a stash made in between is seen.
    let indexed: Vec<(usize, &Repo)> = repos.iter().enumerate().collect();
    let facts = par_map(&indexed, opts.threads, |(i, repo)| {
        let mine: Vec<(&Record, &Look)> = tasks
            .iter()
            .zip(&looks)
            .filter(|((r, _), _)| r == i)
            .map(|((_, rec), l)| (*rec, l))
            .collect();
        repo_facts(repo, &mine, timeout)
    });

    let mut worktrees: Vec<Worktree> = tasks
        .iter()
        .zip(looks)
        .map(|((i, rec), look)| worktree(&repos[*i], rec, look, &facts[*i], &opts.creators))
        .collect();

    let mut seen: HashSet<PathBuf> = worktrees.iter().map(|w| w.path.clone()).collect();
    for (dir, link) in loose {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let (state, repo) = match link {
            Link::Broken { gitdir } => (
                judge(
                    Reading::BrokenLink {
                        gitdir: gitdir.clone(),
                    },
                    None,
                ),
                repo_from_gitdir(&gitdir),
            ),
            Link::Protected { folder, gitdir } => (
                WorktreeState::NotRead(NotRead::ProtectedFolder { folder }),
                repo_from_gitdir(&gitdir),
            ),
            Link::Unreadable { error } => (
                WorktreeState::NotRead(NotRead::Failed { error }),
                dir.clone(),
            ),
            Link::Worktree { .. } | Link::Repo { .. } => continue,
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

/// Sizes every worktree in one walk. A worktree nested inside another counts only toward
/// itself. The walker refuses protected folders and reports a missing folder as an error.
pub fn measure(scan: &mut WorktreeScan, home: &Path) {
    let start = Instant::now();
    let roots: Vec<PathBuf> = scan.worktrees.iter().map(|w| w.path.clone()).collect();
    let sizes = Walker::new(home).exclude(roots.clone()).measure(&roots);
    for (w, size) in scan.worktrees.iter_mut().zip(sizes) {
        w.size = Some(size);
    }
    scan.timings.size_ms = ms(start);
    scan.timings.total_ms += scan.timings.size_ms;
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

#[derive(Default)]
struct Found {
    /// Folders whose `.git` is a folder: main checkouts.
    checkouts: Vec<PathBuf>,
    /// Folders whose `.git` is a file: a linked worktree, a submodule, or a broken link.
    pointers: Vec<PathBuf>,
}

/// Build output and dependencies hold no repos worth the walk.
const SKIP: [&str; 2] = ["node_modules", "target"];

/// Collects repos up to `max` folders down. It stops at a checkout: its git folder lists
/// the worktrees inside it, and the walk would otherwise read every source folder. Symlinks
/// are not followed, and a protected folder is never entered.
fn find(dir: &Path, depth: usize, max: usize, guard: &Protected, found: &mut Found) {
    let Ok(entries) = fs::read_dir(dir) else {
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
            subdirs.push(e);
        }
    }
    for d in subdirs {
        let protected = d
            .metadata()
            .is_ok_and(|m| guard.folder(file_id(&m)).is_some());
        if !protected {
            find(&d.path(), depth + 1, max, guard, found);
        }
    }
}

/// What a folder's `.git` file points at, read from the files git keeps, without running git.
enum Link {
    /// A linked worktree's record, inside its repo's git folder.
    Worktree {
        common: PathBuf,
    },
    /// A whole git folder: a submodule's, or a checkout's made with `--separate-git-dir`.
    Repo {
        gitdir: PathBuf,
    },
    /// The git folder it names is gone.
    Broken {
        gitdir: PathBuf,
    },
    /// It leads into a protected folder.
    Protected {
        folder: PathBuf,
        gitdir: PathBuf,
    },
    Unreadable {
        error: String,
    },
}

fn link(guard: &Protected, dir: &Path) -> Link {
    let gitdir = match git::read_gitdir_file(&dir.join(".git")) {
        Ok(g) => g,
        Err(e) => {
            return Link::Unreadable {
                error: format!("its .git file: {e}"),
            };
        }
    };
    let admin = match guard.resolve(&gitdir) {
        Resolved::Inside(folder) => return Link::Protected { folder, gitdir },
        Resolved::Outside(p) => p,
    };
    if !admin.is_dir() {
        return Link::Broken { gitdir };
    }
    match git::read_path_file(&admin.join("commondir")) {
        Some(common) => match guard.resolve(&admin.join(common)) {
            Resolved::Inside(folder) => Link::Protected { folder, gitdir },
            Resolved::Outside(common) => Link::Worktree { common },
        },
        None => Link::Repo { gitdir: admin },
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
    /// Its main checkout, or the git folder itself when there is none.
    shown_as: PathBuf,
    records: Vec<Record>,
    refs: HashMap<String, Ref>,
    default: Option<DefaultBranch>,
    error: Option<String>,
}

/// One folder under `<common>/worktrees`: git's record of a linked worktree.
struct Record {
    admin: PathBuf,
    /// The worktree's folder, from the record's `gitdir` file. None when that file is gone,
    /// which leaves nothing but the record.
    path: Option<PathBuf>,
    head: RecordHead,
    lock: Option<Lock>,
    created_at: Option<i64>,
}

enum RecordHead {
    Branch(String),
    Detached(String),
    Unreadable,
}

fn read_repo(common: &Path, timeout: Duration) -> Repo {
    let checkout = common
        .parent()
        .filter(|_| common.file_name() == Some(".git".as_ref()));
    let mut repo = Repo {
        common: common.to_path_buf(),
        shown_as: checkout.unwrap_or(common).to_path_buf(),
        records: read_records(common),
        refs: HashMap::new(),
        default: None,
        error: None,
    };
    // Without a record there is nothing to ask git about.
    if repo.records.is_empty() {
        return repo;
    }
    let args = [
        "for-each-ref",
        git::REF_FORMAT,
        "refs/heads",
        "refs/remotes/origin",
        "refs/stash",
    ];
    match git::stdout(common, &args, timeout) {
        Ok(out) => {
            repo.refs = git::parse_refs(&out)
                .into_iter()
                .map(|r| (r.name.clone(), r))
                .collect();
        }
        Err(e) => repo.error = Some(e.to_string()),
    }
    let main_branch = match read_head(&common.join("HEAD")) {
        RecordHead::Branch(b) => Some(b),
        _ => None,
    };
    repo.default = default_branch(&repo.refs, main_branch.as_deref());
    repo
}

fn read_records(common: &Path) -> Vec<Record> {
    let Ok(entries) = fs::read_dir(common.join("worktrees")) else {
        return Vec::new();
    };
    let mut records: Vec<Record> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let admin = e.path();
            // The file names the worktree's `.git` file, relative to the record when git
            // was asked for relative paths.
            let path = git::read_path_file(&admin.join("gitdir")).map(|p| {
                let dot_git = admin.join(p);
                match dot_git.file_name() {
                    Some(n) if n == ".git" => dot_git.parent().unwrap_or(&dot_git).to_path_buf(),
                    _ => dot_git,
                }
            });
            let lock = fs::read(admin.join("locked")).ok().map(|text| {
                let reason = String::from_utf8_lossy(&text).trim().to_string();
                Lock {
                    reason: (!reason.is_empty()).then_some(reason),
                }
            });
            let created_at = fs::metadata(&admin)
                .and_then(|m| m.created())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            Record {
                head: read_head(&admin.join("HEAD")),
                admin,
                path,
                lock,
                created_at,
            }
        })
        .collect();
    records.sort_by(|a, b| a.admin.cmp(&b.admin));
    records
}

fn read_head(file: &Path) -> RecordHead {
    let Ok(text) = fs::read_to_string(file) else {
        return RecordHead::Unreadable;
    };
    let text = text.trim();
    match text.strip_prefix("ref: refs/heads/") {
        Some(branch) => RecordHead::Branch(branch.to_string()),
        None if text.len() >= 40 && text.bytes().all(|b| b.is_ascii_hexdigit()) => {
            RecordHead::Detached(text.to_string())
        }
        None => RecordHead::Unreadable,
    }
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

/// What one look at a registered worktree's folder found.
enum Look {
    Protected {
        path: PathBuf,
        folder: PathBuf,
    },
    Missing,
    NoGitLink(PathBuf),
    LinkElsewhere {
        path: PathBuf,
        gitdir: PathBuf,
    },
    /// The folder links back to its record and one `git status` read it.
    Status(PathBuf, git::Status),
    Failed {
        path: PathBuf,
        error: String,
    },
}

fn look(rec: &Record, guard: &Protected, timeout: Duration) -> Look {
    let Some(path) = &rec.path else {
        return Look::Missing;
    };
    let path = match guard.resolve(path) {
        Resolved::Inside(folder) => {
            return Look::Protected {
                path: path.clone(),
                folder,
            };
        }
        Resolved::Outside(p) => p,
    };
    let failed = |path: &Path, e: io::Error| Look::Failed {
        path: path.to_path_buf(),
        error: e.to_string(),
    };
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Look::Missing,
        Err(e) => return failed(&path, e),
        Ok(m) if !m.is_dir() => return Look::NoGitLink(path),
        Ok(_) => {}
    }
    let dot_git = path.join(".git");
    match fs::symlink_metadata(&dot_git) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Look::NoGitLink(path),
        Err(e) => return failed(&path, e),
        Ok(m) if !m.is_file() => {
            return Look::LinkElsewhere {
                path,
                gitdir: dot_git,
            };
        }
        Ok(_) => {}
    }
    let gitdir = match git::read_gitdir_file(&dot_git) {
        Ok(g) => g,
        Err(e) => return failed(&path, e),
    };
    let same = |a: &Path, b: &Path| match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => file_id(&a) == file_id(&b),
        _ => false,
    };
    match guard.resolve(&gitdir) {
        Resolved::Outside(g) if same(&g, &rec.admin) => {}
        _ => return Look::LinkElsewhere { path, gitdir },
    }
    match git::stdout(&path, &git::STATUS, timeout).and_then(|out| git::parse_status(&out)) {
        Ok(status) => Look::Status(path, status),
        Err(e) => Look::Failed {
            path,
            error: e.to_string(),
        },
    }
}

/// What the repo says about the HEADs its worktrees reported.
struct RepoFacts {
    /// HEADs reachable from the default branch, to the ref that holds them.
    merged: HashMap<String, String>,
    commit_times: HashMap<String, i64>,
    stashes: Vec<git::Stash>,
    /// Commits that only one record's HEAD holds, by record, where that is evidence.
    only_here: Result<HashMap<PathBuf, u32>, String>,
}

fn repo_facts(
    repo: &Repo,
    mine: &[(&Record, &Look)],
    timeout: Duration,
) -> Result<RepoFacts, String> {
    if let Some(e) = &repo.error {
        return Err(e.clone());
    }
    let mut heads: Vec<String> = mine
        .iter()
        .filter_map(|(_, l)| match l {
            Look::Status(_, s) => s.oid.clone(),
            _ => None,
        })
        .collect();
    heads.sort();
    heads.dedup();
    let mut facts = RepoFacts {
        merged: HashMap::new(),
        commit_times: HashMap::new(),
        stashes: Vec::new(),
        only_here: Ok(HashMap::new()),
    };
    let common = &repo.common;
    if !heads.is_empty() {
        if let Some(d) = &repo.default {
            facts.merged = merged_heads(common, d, &heads, timeout).map_err(|e| e.to_string())?;
        }
        facts.commit_times = commit_times(common, &heads, timeout).map_err(|e| e.to_string())?;
        if repo.refs.contains_key("refs/stash") {
            let out = git::stdout(common, &git::STASH_LIST, timeout).map_err(|e| e.to_string())?;
            facts.stashes = git::parse_stashes(&out);
        }
    }
    let tips: Vec<Tip> = mine
        .iter()
        .filter_map(|(rec, look)| tip(repo, rec, look, &facts.merged))
        .collect();
    if !tips.is_empty() {
        facts.only_here = only_here(common, &tips, timeout).map_err(|e| e.to_string());
    }
    Ok(facts)
}

/// A HEAD whose commits that no other ref holds are evidence: a branch whose upstream is
/// gone, and a record whose folder is missing or unusable.
struct Tip<'a> {
    admin: &'a Path,
    oid: String,
    branch: Option<&'a str>,
}

fn tip<'a>(
    repo: &Repo,
    rec: &'a Record,
    look: &'a Look,
    merged: &HashMap<String, String>,
) -> Option<Tip<'a>> {
    let (oid, branch) = match look {
        Look::Status(_, s) => {
            let gone = s.upstream.is_some() && s.ahead_behind.is_none();
            let oid = s.oid.clone()?;
            if !gone || merged.contains_key(&oid) {
                return None;
            }
            (oid, s.branch.as_deref())
        }
        Look::Missing | Look::NoGitLink(_) | Look::LinkElsewhere { .. } => match &rec.head {
            RecordHead::Branch(b) => {
                let r = repo.refs.get(&format!("refs/heads/{b}"))?;
                (r.oid.clone(), Some(b.as_str()))
            }
            RecordHead::Detached(oid) => (oid.clone(), None),
            RecordHead::Unreadable => return None,
        },
        Look::Protected { .. } | Look::Failed { .. } => return None,
    };
    Some(Tip {
        admin: &rec.admin,
        oid,
        branch,
    })
}

/// Counts, for each tip, the commits that no branch, remote branch or tag holds but the tip's
/// own branch, in one `rev-list` for the repo: it lists every commit the tips hold that no
/// other ref does, with its parents, and each tip's share is walked from that list. Another
/// detached tip does not count as holding a commit, since pruning its record drops it too.
fn only_here(
    common: &Path,
    tips: &[Tip],
    timeout: Duration,
) -> anyhow::Result<HashMap<PathBuf, u32>> {
    // Before --branches, an exclude pattern is a branch name without refs/heads/.
    let excludes: Vec<String> = tips
        .iter()
        .filter_map(|t| t.branch)
        .map(|b| format!("--exclude={b}"))
        .collect();
    let mut args = vec!["rev-list", "--parents"];
    args.extend(tips.iter().map(|t| t.oid.as_str()));
    args.push("--not");
    args.extend(excludes.iter().map(String::as_str));
    args.extend(["--branches", "--remotes", "--tags"]);
    let out = git::stdout(common, &args, timeout)?;
    let parents: HashMap<&str, Vec<&str>> = out
        .lines()
        .filter_map(|l| {
            let mut ids = l.split(' ');
            Some((ids.next()?, ids.collect()))
        })
        .collect();
    let reach: Vec<HashSet<&str>> = tips
        .iter()
        .map(|t| {
            let mut seen = HashSet::new();
            let mut todo: Vec<&str> = parents
                .get_key_value(t.oid.as_str())
                .map(|(k, _)| *k)
                .into_iter()
                .collect();
            while let Some(c) = todo.pop() {
                if seen.insert(c) {
                    todo.extend(parents[c].iter().filter(|p| parents.contains_key(*p)));
                }
            }
            seen
        })
        .collect();
    let mut on_branches: HashMap<&str, u32> = HashMap::new();
    for (_, set) in tips.iter().zip(&reach).filter(|(t, _)| t.branch.is_some()) {
        for c in set {
            *on_branches.entry(c).or_default() += 1;
        }
    }
    Ok(tips
        .iter()
        .zip(&reach)
        .map(|(t, set)| {
            let own = u32::from(t.branch.is_some());
            let alone = set
                .iter()
                .filter(|c| on_branches.get(*c).copied().unwrap_or(0) == own)
                .count() as u32;
            (t.admin.to_path_buf(), alone)
        })
        .collect())
}

fn worktree(
    repo: &Repo,
    rec: &Record,
    look: Look,
    facts: &Result<RepoFacts, String>,
    creators: &[Box<dyn CreatorSource>],
) -> Worktree {
    let record_head = match &rec.head {
        RecordHead::Branch(name) => Head::Branch {
            name: name.clone(),
            oid: repo
                .refs
                .get(&format!("refs/heads/{name}"))
                .map(|r| r.oid.clone()),
        },
        RecordHead::Detached(oid) => Head::Detached { oid: oid.clone() },
        RecordHead::Unreadable => Head::Unknown,
    };
    let detached = matches!(rec.head, RecordHead::Detached(_));
    let lock = rec.lock.as_ref();
    let not_read = |error: String| WorktreeState::NotRead(NotRead::Failed { error });
    // A count is evidence everywhere but on a missing detached HEAD, where it decides.
    let (only_here, count_error) = match facts.as_ref().map(|f| &f.only_here) {
        Ok(Ok(counts)) => (counts.get(&rec.admin).copied(), None),
        Ok(Err(e)) | Err(e) => (None, Some(e.clone())),
    };
    let shown = rec.path.clone().unwrap_or_else(|| rec.admin.clone());
    let (path, head, state) = match look {
        Look::Protected { path, folder } => (
            path,
            record_head,
            WorktreeState::NotRead(NotRead::ProtectedFolder { folder }),
        ),
        Look::Failed { path, error } => (path, record_head, not_read(error)),
        Look::Missing if matches!(rec.head, RecordHead::Unreadable) => (
            shown,
            record_head,
            not_read("its record's HEAD file could not be read".into()),
        ),
        Look::Missing => match count_error.filter(|_| detached) {
            Some(e) => (shown, record_head, not_read(e)),
            None => (
                shown,
                record_head,
                judge(
                    Reading::Missing {
                        detached,
                        only_here,
                    },
                    lock,
                ),
            ),
        },
        Look::NoGitLink(path) => (
            path,
            record_head,
            judge(Reading::NoGitLink { only_here }, lock),
        ),
        Look::LinkElsewhere { path, gitdir } => (
            path,
            record_head,
            judge(Reading::LinkElsewhere { gitdir, only_here }, lock),
        ),
        Look::Status(path, status) => {
            let head = match (&status.branch, &status.oid) {
                (Some(name), oid) => Head::Branch {
                    name: name.clone(),
                    oid: oid.clone(),
                },
                (None, Some(oid)) => Head::Detached { oid: oid.clone() },
                (None, None) => Head::Unknown,
            };
            let state = match facts {
                Ok(f) => judge(
                    Reading::Present(git_facts(repo, &status, f, only_here)),
                    lock,
                ),
                Err(e) => not_read(e.clone()),
            };
            (path, head, state)
        }
    };
    Worktree {
        creator: creator::attribute(creators, &path, lock.and_then(|l| l.reason.as_deref())),
        path,
        repo: repo.shown_as.clone(),
        head,
        state,
        lock: rec.lock.clone(),
        size: None,
        created_at: rec.created_at,
    }
}

fn git_facts(repo: &Repo, s: &git::Status, f: &RepoFacts, only_here: Option<u32>) -> GitFacts {
    let changes = match (s.changed, s.untracked) {
        (0, 0) => Changes::Clean,
        (changed, untracked) => Changes::Dirty { changed, untracked },
    };
    let merge = match (&s.oid, &repo.default) {
        (None, _) => Merge::Unborn,
        (Some(oid), _) if f.merged.contains_key(oid) => Merge::Merged {
            into: f.merged[oid].clone(),
        },
        (Some(_), Some(d)) => Merge::Unmerged {
            into: d.refs[0].0.clone(),
        },
        (Some(_), None) => Merge::NoDefaultBranch,
    };
    let upstream = match (&s.upstream, s.ahead_behind) {
        (None, _) => Upstream::None,
        (Some(name), None) => Upstream::Gone { name: name.clone() },
        (Some(name), Some((ahead, behind))) => {
            // A remote branch only if the branch's config names the same ref under
            // refs/remotes; git prints both kinds by their short names.
            let configured = s
                .branch
                .as_ref()
                .and_then(|b| repo.refs.get(&format!("refs/heads/{b}")))
                .and_then(|r| r.upstream.as_deref());
            if configured == Some(&format!("refs/remotes/{name}")) {
                Upstream::Tracking {
                    name: name.clone(),
                    ahead,
                    behind,
                }
            } else {
                Upstream::Local { name: name.clone() }
            }
        }
    };
    // An entry names its branch; one made on a detached HEAD is matched by its commit.
    let stashes = f
        .stashes
        .iter()
        .filter(|st| match &st.branch {
            Some(b) => s.branch.as_ref() == Some(b),
            None => s.oid.as_ref() == Some(&st.base),
        })
        .count() as u32;
    GitFacts {
        changes,
        merge,
        upstream,
        stashes,
        only_here,
        last_commit: s.oid.as_ref().and_then(|o| f.commit_times.get(o)).copied(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::tests::{add, commit, new_repo};

    /// HEAD and cleanliness come from one `git status`, so a commit made after the repo was
    /// listed is never read as merged.
    #[test]
    fn a_commit_made_after_listing_is_not_read_as_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let app = base.join("code/app");
        new_repo(&base, &app);
        let wt = add(&app, "feat-race", "app-race");
        let timeout = Duration::from_secs(5);
        let repo = read_repo(&app.join(".git"), timeout);
        commit(&wt, "late.txt");

        let rec = &repo.records[0];
        let look = look(rec, &Protected::new(&base), timeout);
        let facts = repo_facts(&repo, &[(rec, &look)], timeout);
        let w = worktree(&repo, rec, look, &facts, &[]);
        let WorktreeState::Active(f) = &w.state else {
            panic!("{:?}", w.state);
        };
        assert!(matches!(f.merge, Merge::Unmerged { .. }), "{f:?}");
    }
}
