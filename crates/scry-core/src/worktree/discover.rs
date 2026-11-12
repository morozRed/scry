//! Finds repositories under the code roots and agent folders, reads git's record of each
//! linked worktree, looks at each folder with one `git status` and one `git ls-files`, and
//! sizes them all in one walk that also finds the repositories nested inside. No git runs on
//! a path inside a protected folder, and no file there is read. No git runs on a worktree
//! whose record's `commondir` leads anywhere but its repository's git folder, or whose
//! `config.worktree` includes a file inside a protected folder. `protected` names what the
//! user's own git config can still lead git to read there.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::creator::{self, Agent, Clues, CreatorSource};
use super::verdict::{Reading, judge};
use super::{
    Changes, GitFacts, Head, Lock, Merge, NotRead, PerWorktree, ReposInside, Timings, Upstream,
    Worktree, WorktreeScan, WorktreeState,
};
use crate::git::{self, Ref};
use crate::protected::{FileId, Protected, Resolved, file_id, read_no_follow};
use crate::size::{SubtreeSize, Walker};

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

/// Reads every worktree. Until `measure` walks their folders, none is "can go": a repository
/// nested inside one would go unseen.
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
            Resolved::Unresolvable => warnings.push(format!(
                "{} was not read: a `..` follows a folder that does not exist",
                root.display()
            )),
            Resolved::Outside(root) => find(&root, 0, opts.max_depth, &guard, &mut found),
        }
    }
    for dir in &opts.agent_dirs {
        if let Resolved::Outside(dir) = guard.resolve(dir) {
            find(&dir, 0, 2, &guard, &mut found);
        }
    }
    let mut loose = Vec::new();
    // Agent worktree folders inside each checkout hold leftovers git may no longer know.
    for checkout in found.checkouts.clone() {
        let Some(dir) = guard.resolve(&checkout.join(".claude/worktrees")).outside() else {
            continue;
        };
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            // Each entry on its own: a symlink, or the folder a protected one stands for,
            // must not be entered.
            match guard.resolve(&e.path()) {
                Resolved::Inside(folder) => loose.push((
                    e.path(),
                    Link::Protected {
                        folder,
                        repo: checkout.clone(),
                    },
                )),
                Resolved::Outside(p)
                    if p.is_dir()
                        && fs::symlink_metadata(p.join(".git")).is_ok_and(|m| m.is_file()) =>
                {
                    found.pointers.push(p)
                }
                _ => {}
            }
        }
    }
    found.pointers.sort();
    found.pointers.dedup();
    let discover_ms = ms(start);

    let git_start = Instant::now();
    let timeout = opts.git_timeout;
    let mut commons: Vec<PathBuf> = found.checkouts.iter().map(|c| c.join(".git")).collect();
    // Folders whose `.git` links to a record, by the record's identity.
    let mut links_back: HashMap<FileId, Vec<PathBuf>> = HashMap::new();
    for dir in &found.pointers {
        match link(&guard, dir) {
            Link::Worktree { common, admin } => {
                if let Ok(m) = fs::metadata(&admin) {
                    links_back.entry(file_id(&m)).or_default().push(dir.clone());
                }
                commons.push(common);
            }
            Link::Repo { gitdir } => commons.push(gitdir),
            other => loose.push((dir.clone(), other)),
        }
    }
    commons.sort();
    commons.dedup();

    let repos = par_map(&commons, opts.threads, |c| read_repo(c, &guard, timeout));
    let mut tasks: Vec<(usize, &Record)> = Vec::new();
    for (i, repo) in repos.iter().enumerate() {
        warnings.extend(repo.warnings.iter().cloned());
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

    // Every folder the scan knows as a worktree, to find those nested in another.
    let mut known: Vec<PathBuf> = looks.iter().filter_map(Look::folder).cloned().collect();
    known.extend(loose.iter().map(|(dir, _)| dir.clone()));
    let ctx = Context {
        known: &known,
        links_back: &links_back,
        creators: &opts.creators,
        timeout,
    };
    let mut worktrees: Vec<Worktree> = tasks
        .iter()
        .zip(looks)
        .map(|((i, rec), look)| worktree(&repos[*i], rec, look, &facts[*i], &ctx))
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
            Link::Protected { folder, repo } => (
                WorktreeState::NotRead(NotRead::ProtectedFolder { folder }),
                repo,
            ),
            Link::Unreadable { error } => (
                WorktreeState::NotRead(NotRead::Failed { error }),
                dir.clone(),
            ),
            Link::Worktree { .. } | Link::Repo { .. } => continue,
        };
        let clues = Clues {
            path: &dir,
            lock_reason: None,
            record_files: &[],
        };
        worktrees.push(Worktree {
            creator: creator::attribute(&opts.creators, &clues),
            path: dir,
            repo,
            head: Head::Unknown,
            state,
            lock: None,
            size: None,
            created_at: None,
            record: None,
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

/// Sizes every worktree in one walk, and judges again each one that git read, now knowing the
/// repositories inside it. A worktree nested inside another counts only toward itself. The
/// walker refuses protected folders and reports a missing folder as an error.
pub fn measure(scan: &mut WorktreeScan, home: &Path) {
    let start = Instant::now();
    let roots: Vec<PathBuf> = scan.worktrees.iter().map(|w| w.path.clone()).collect();
    let sizes = Walker::new(home).exclude(roots.clone()).measure(&roots);
    apply_sizes(scan, sizes.into_iter().map(Some).collect());
    scan.timings.size_ms = ms(start);
    scan.timings.total_ms += scan.timings.size_ms;
}

/// Takes in sizes measured elsewhere, such as from the size cache, one per worktree in
/// order, and judges again each worktree that got one. A worktree whose size is `None`
/// keeps its state, which is never "can go" before its folder was walked.
pub fn apply_sizes(scan: &mut WorktreeScan, sizes: Vec<Option<SubtreeSize>>) {
    for (w, size) in scan.worktrees.iter_mut().zip(sizes) {
        let Some(size) = size else { continue };
        if let Some(facts) = w.state.facts() {
            let mut facts = facts.clone();
            facts.per_worktree.repos_inside = match size.error {
                Some(_) => ReposInside::Unknown,
                None => ReposInside::Walked {
                    repos: size.repos.clone(),
                    unread: size.skipped,
                },
            };
            w.state = judge(Reading::Present(facts), w.lock.as_ref());
        }
        w.size = Some(size);
    }
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
        admin: PathBuf,
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
        repo: PathBuf,
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
    let unresolvable = || Link::Unreadable {
        error: format!(
            "its .git file names {}, which cannot be resolved",
            gitdir.display()
        ),
    };
    let admin = match guard.resolve(&gitdir) {
        Resolved::Inside(folder) => {
            let repo = repo_from_gitdir(&gitdir);
            return Link::Protected { folder, repo };
        }
        Resolved::Unresolvable => return unresolvable(),
        Resolved::Outside(p) => p,
    };
    if !admin.is_dir() {
        return Link::Broken { gitdir };
    }
    match git::read_path_file(&admin.join("commondir")) {
        Ok(Some(common)) => match guard.resolve(&admin.join(common)) {
            Resolved::Inside(folder) => {
                let repo = repo_from_gitdir(&gitdir);
                Link::Protected { folder, repo }
            }
            Resolved::Unresolvable => unresolvable(),
            Resolved::Outside(common) => Link::Worktree { common, admin },
        },
        Ok(None) => Link::Repo { gitdir: admin },
        Err(e) => Link::Unreadable {
            error: format!("its record's commondir file: {e}"),
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
    /// Its main checkout, or the git folder itself when there is none.
    shown_as: PathBuf,
    records: Vec<Record>,
    refs: HashMap<String, Ref>,
    default: Option<DefaultBranch>,
    /// The newest `FETCH_HEAD` in the git folder or any record, Unix seconds.
    fetched_at: Option<i64>,
    error: Option<String>,
    warnings: Vec<String>,
}

/// One folder under `<common>/worktrees`: git's record of a linked worktree.
struct Record {
    admin: PathBuf,
    path: RecordPath,
    head: RecordHead,
    lock: Option<Lock>,
    created_at: Option<i64>,
    /// Entries beyond git's standard files and known agent bookkeeping, which pruning or
    /// removal would destroy. A `config.worktree` with keys beyond `CONFIG_KEYS` is named with
    /// those keys.
    extras: Vec<String>,
    /// Files from `AGENT_FILES` that it holds.
    agent_files: Vec<&'static AgentFile>,
    /// Entries that are symlinks. git would follow one wherever it leads, so it is not run.
    symlinks: Vec<String>,
    /// Why git must not run on it at all, found in the record before any git call.
    blocked: Option<Blocked>,
}

/// A record that would lead git somewhere Scry does not let it go.
enum Blocked {
    /// Its `commondir` or an include in its `config.worktree` leads into this folder.
    Protected(PathBuf),
    Unsafe(String),
}

/// The worktree's folder, from the record's `gitdir` file.
enum RecordPath {
    Folder(PathBuf),
    /// The file is gone or empty, which leaves nothing but the record, as git sees it too.
    Absent,
    Unreadable(String),
}

enum RecordHead {
    Branch(String),
    Detached(String),
    Unreadable,
}

fn read_repo(common: &Path, guard: &Protected, timeout: Duration) -> Repo {
    let checkout = common
        .parent()
        .filter(|_| common.file_name() == Some(".git".as_ref()));
    let mut repo = Repo {
        common: common.to_path_buf(),
        shown_as: checkout.unwrap_or(common).to_path_buf(),
        records: Vec::new(),
        refs: HashMap::new(),
        default: None,
        fetched_at: None,
        error: None,
        warnings: Vec::new(),
    };
    read_records(&mut repo, guard, timeout);
    // Without a record there is nothing to ask git about.
    if repo.records.is_empty() {
        return repo;
    }
    let args = [
        "for-each-ref",
        git::REF_FORMAT,
        "refs/heads",
        "refs/remotes/origin",
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
    repo.default = default_branch(&repo.refs);
    repo
}

/// Files git keeps in a worktree's record as a matter of course, `AUTO_MERGE` and `ORIG_HEAD`
/// being what its last merge or reset left. Anything else is the worktree's own state, such
/// as its config, hooks or refs, an operation in progress, or a file another tool put there,
/// unless `AGENT_FILES` or `CONFIG_KEYS` clears it.
const RECORD_FILES: [&str; 10] = [
    "AUTO_MERGE",
    "COMMIT_EDITMSG",
    "FETCH_HEAD",
    "HEAD",
    "ORIG_HEAD",
    "commondir",
    "gitdir",
    "index",
    "locked",
    "logs",
];

/// A file an agent writes into a worktree's record for its own bookkeeping.
struct AgentFile {
    name: &'static str,
    /// The agent that writes it, when no other agent does.
    agent: Option<Agent>,
}

/// Files that agents write into a worktree's record, each reviewed for what it holds: how
/// the agent made or tracks the worktree, never the user's work. They are not git's, so they
/// are kept apart from `RECORD_FILES`, but removing or pruning a worktree loses nothing Scry
/// checks by destroying them. One that only one agent writes names the worktree's probable
/// creator. Only a regular file counts. Add a name here only after reading what agents put
/// in it; an unknown file keeps a worktree at "needs a look".
static AGENT_FILES: [AgentFile; 5] = [
    // The commit a Claude Code agent's worktree started from.
    AgentFile {
        name: "CLAUDE_BASE",
        agent: Some(Agent::ClaudeCode),
    },
    // An empty marker on the worktrees the Claude desktop app makes for Claude Code.
    AgentFile {
        name: "claude-desktop-worktree",
        agent: Some(Agent::ClaudeCode),
    },
    // The Codex thread that owns the worktree.
    AgentFile {
        name: "codex-thread.json",
        agent: Some(Agent::Codex),
    },
    // The branch Codex last synced the worktree with, and the tree it synced.
    AgentFile {
        name: "codex-synced-branch.json",
        agent: Some(Agent::Codex),
    },
    // A preview slot number. It sits beside both agents' files, so it names neither.
    AgentFile {
        name: "preview-slot",
        agent: None,
    },
];

/// Keys an agent sets in a worktree's `config.worktree` that hold no work: git's untracked
/// cache and file system monitor switches, and Codex's own settings. Any other key, such as a
/// hook path or an identity, is the worktree's own config, which removing it destroys.
/// `core.worktree` passes when it names the worktree's own folder, as agents set it; aimed
/// anywhere else it sends git to another folder.
const CONFIG_KEYS: [&str; 2] = ["core.untrackedcache", "core.fsmonitor"];
const CONFIG_SECTIONS: [&str; 1] = ["codex."];

fn read_records(repo: &mut Repo, guard: &Protected, timeout: Duration) {
    let fetched = |dir: &Path| mtime(&dir.join("FETCH_HEAD"));
    repo.fetched_at = fetched(&repo.common);
    let dir = match guard.resolve(&repo.common.join("worktrees")) {
        Resolved::Outside(d) => d,
        Resolved::Inside(folder) => {
            repo.warnings.push(format!(
                "{}: its worktree records were not read: protected folder {}",
                repo.common.display(),
                folder.display()
            ));
            return;
        }
        Resolved::Unresolvable => return,
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        // A record folder could still be what a protected folder stands for.
        if e.metadata()
            .is_ok_and(|m| guard.folder(file_id(&m)).is_some())
        {
            repo.warnings.push(format!(
                "{} was not read: it is a protected folder",
                e.path().display()
            ));
            continue;
        }
        let admin = e.path();
        repo.fetched_at = repo.fetched_at.max(fetched(&admin));
        // The file names the worktree's `.git` file, relative to the record when git was
        // asked for relative paths.
        let path = match git::read_path_file(&admin.join("gitdir")) {
            Ok(Some(p)) => {
                let dot_git = admin.join(p);
                RecordPath::Folder(match dot_git.file_name() {
                    Some(n) if n == ".git" => dot_git.parent().unwrap_or(&dot_git).to_path_buf(),
                    _ => dot_git,
                })
            }
            Ok(None) => RecordPath::Absent,
            Err(e) => RecordPath::Unreadable(format!("its record's gitdir file: {e}")),
        };
        let lock = match read_no_follow(&admin.join("locked")) {
            Ok(text) => {
                let reason = String::from_utf8_lossy(&text).trim().to_string();
                Some(Lock {
                    reason: (!reason.is_empty()).then_some(reason),
                    unreadable: false,
                })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => Some(Lock {
                reason: None,
                unreadable: true,
            }),
        };
        let created_at = fs::symlink_metadata(&admin)
            .and_then(|m| m.created())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        let mut extras = Vec::new();
        let mut agent_files = Vec::new();
        let mut symlinks = Vec::new();
        let mut blocked = own_commondir(&admin, &repo.common, guard).err();
        for e in fs::read_dir(&admin).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let kind = e.file_type();
            if kind.as_ref().is_ok_and(|t| t.is_symlink()) {
                symlinks.push(name.clone());
            }
            if RECORD_FILES.contains(&name.as_str()) || name.starts_with("sharedindex.") {
                continue;
            }
            // The entry's own type: a symlink is not a file here.
            let file = kind.as_ref().is_ok_and(|t| t.is_file());
            if let Some(known) = AGENT_FILES.iter().find(|f| f.name == name).filter(|_| file) {
                agent_files.push(known);
                continue;
            }
            if file && name == "config.worktree" {
                match config_extras(&e.path(), &admin, &path, guard, timeout) {
                    Ok((keys, includes)) => {
                        if !keys.is_empty() {
                            extras.push(format!("{name} ({})", keys.join(", ")));
                        }
                        if let Err(b) = check_includes(&includes, &admin, guard, timeout, 0) {
                            blocked = blocked.or(Some(b));
                        }
                    }
                    Err(e) => {
                        extras.push(name);
                        // Its includes are unknown, so git could be led anywhere.
                        blocked = blocked.or(Some(Blocked::Unsafe(format!(
                            "its record's config.worktree could not be read: {e}"
                        ))));
                    }
                }
                continue;
            }
            let holds = match kind {
                Ok(t) if t.is_dir() => holds_files(&e.path(), guard, 0),
                _ => true,
            };
            if holds {
                extras.push(name);
            }
        }
        extras.sort();
        agent_files.sort_by_key(|f| f.name);
        symlinks.sort();
        repo.records.push(Record {
            head: read_head(&admin.join("HEAD")),
            admin,
            path,
            lock,
            created_at,
            extras,
            agent_files,
            symlinks,
            blocked,
        });
    }
    repo.records.sort_by(|a, b| a.admin.cmp(&b.admin));
}

/// Whether anything but folders sits under `dir`. git leaves empty folders behind, such as
/// `refs/bisect` after a bisect, and those hold nothing. Symlinks are not followed, and an
/// unreadable folder counts as holding something.
fn holds_files(dir: &Path, guard: &Protected, depth: usize) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };
    entries.flatten().any(|e| match e.file_type() {
        Ok(t) if t.is_dir() => {
            depth >= 16
                || e.metadata()
                    .is_ok_and(|m| guard.folder(file_id(&m)).is_some())
                || holds_files(&e.path(), guard, depth + 1)
        }
        _ => true,
    })
}

/// The keys in a record's `config.worktree` that `CONFIG_KEYS` does not clear, as git names
/// them, without their values. git parses the file from `/`, so no repository's config is
/// read with it. The caller has checked that the entry is a regular file, not a symlink that
/// git would follow.
fn config_extras(
    file: &Path,
    admin: &Path,
    path: &RecordPath,
    guard: &Protected,
    timeout: Duration,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let out = git::stdout(Path::new("/"), &git::config_list_args(file), timeout)?;
    let entries = git::parse_config(&out);
    // git reads a relative `core.worktree` from the record's folder.
    let own_folder = |value: &str| match path {
        RecordPath::Folder(folder) => {
            match (guard.resolve(&admin.join(value)), guard.resolve(folder)) {
                (Resolved::Outside(a), Resolved::Outside(b)) => a == b,
                _ => false,
            }
        }
        RecordPath::Absent | RecordPath::Unreadable(_) => false,
    };
    let cleared = |key: &str, value: Option<&str>| {
        CONFIG_KEYS.contains(&key)
            || CONFIG_SECTIONS.iter().any(|s| key.starts_with(s))
            || (key == "core.worktree" && value.is_some_and(own_folder))
    };
    let mut keys: Vec<String> = entries
        .iter()
        .filter(|(key, value)| !cleared(key, value.as_deref()))
        .map(|(key, _)| key.clone())
        .collect();
    keys.sort();
    keys.dedup();
    Ok((keys, include_paths(&entries)))
}

/// The files a config's `include.path` and `includeIf.<condition>.path` entries name, whatever
/// the condition: git may meet it.
fn include_paths(entries: &[(String, Option<String>)]) -> Vec<String> {
    entries
        .iter()
        .filter(|(key, _)| {
            key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path"))
        })
        .map(|(_, value)| value.clone().unwrap_or_default())
        .collect()
}

/// git follows includes this deep, and stops with an error past it.
const MAX_INCLUDE_DEPTH: usize = 10;

/// Checks the files a record's config includes, and those they include in turn, before git
/// reads them. Each is resolved as git resolves it: `~/` from `HOME`, a relative path from the
/// including file's folder. One inside a protected folder, or one that cannot be resolved,
/// blocks git.
fn check_includes(
    includes: &[String],
    from: &Path,
    guard: &Protected,
    timeout: Duration,
    depth: usize,
) -> Result<(), Blocked> {
    for value in includes {
        let unsafe_ =
            |why: &str| Blocked::Unsafe(format!("its record's config includes {value}, {why}"));
        if depth >= MAX_INCLUDE_DEPTH {
            return Err(unsafe_("past git's include depth"));
        }
        let target = if let Some(rest) = value.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(home) => Path::new(&home).join(rest),
                None => return Err(unsafe_("and HOME is not set")),
            }
        } else if value.starts_with('~') || value.is_empty() {
            return Err(unsafe_("which cannot be resolved"));
        } else {
            from.join(value)
        };
        let target = match guard.resolve(&target) {
            Resolved::Inside(folder) => return Err(Blocked::Protected(folder)),
            Resolved::Unresolvable => return Err(unsafe_("which cannot be resolved")),
            Resolved::Outside(t) => t,
        };
        // git skips an include that is not there.
        if !target.is_file() {
            continue;
        }
        let out = git::stdout(Path::new("/"), &git::config_list_args(&target), timeout)
            .map_err(|e| unsafe_(&format!("which could not be read: {e}")))?;
        let nested = include_paths(&git::parse_config(&out));
        let dir = target.parent().unwrap_or(Path::new("/"));
        check_includes(&nested, dir, guard, timeout, depth + 1)?;
    }
    Ok(())
}

/// The record's `commondir`, read without following a symlink, must lead back to the git
/// folder the record sits in: git reads the repository's config, refs and objects from
/// wherever it points.
fn own_commondir(admin: &Path, common: &Path, guard: &Protected) -> Result<(), Blocked> {
    let unsafe_ = |why: String| Err(Blocked::Unsafe(format!("its record's commondir {why}")));
    let named = match git::read_path_file(&admin.join("commondir")) {
        Ok(Some(c)) => c,
        Ok(None) => return unsafe_("file is missing or empty".into()),
        Err(e) => return unsafe_(format!("file could not be read: {e}")),
    };
    let target = match guard.resolve(&admin.join(&named)) {
        Resolved::Inside(folder) => return Err(Blocked::Protected(folder)),
        Resolved::Unresolvable => {
            return unsafe_(format!(
                "names {}, which cannot be resolved",
                named.display()
            ));
        }
        Resolved::Outside(t) => t,
    };
    match (fs::metadata(&target), fs::metadata(common)) {
        (Ok(a), Ok(b)) if file_id(&a) == file_id(&b) => Ok(()),
        _ => unsafe_(format!(
            "names {}, not the repository's own git folder",
            named.display()
        )),
    }
}

fn mtime(file: &Path) -> Option<i64> {
    let t = fs::symlink_metadata(file).ok()?.modified().ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

fn read_head(file: &Path) -> RecordHead {
    let Ok(text) = read_no_follow(file) else {
        return RecordHead::Unreadable;
    };
    let text = String::from_utf8_lossy(&text);
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

/// `origin/HEAD` names the default branch; without it, `main` or `master`, local or remote.
/// Whatever the main checkout has checked out is not a default branch: it can be a feature
/// branch, and every commit on it would read as merged.
fn default_branch(refs: &HashMap<String, Ref>) -> Option<DefaultBranch> {
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
        })?;
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
    /// The folder links back to its record, and git read its status and index.
    Read {
        path: PathBuf,
        status: git::Status,
        hidden: u32,
        /// Submodules checked out in it, relative to it.
        submodules: Vec<PathBuf>,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

impl Look {
    /// The folder, when one is there.
    fn folder(&self) -> Option<&PathBuf> {
        match self {
            Look::Protected { path, .. }
            | Look::NoGitLink(path)
            | Look::LinkElsewhere { path, .. }
            | Look::Read { path, .. }
            | Look::Failed { path, .. } => Some(path),
            Look::Missing => None,
        }
    }
}

fn look(rec: &Record, guard: &Protected, timeout: Duration) -> Look {
    let path = match &rec.path {
        RecordPath::Folder(p) => p,
        RecordPath::Absent => return Look::Missing,
        RecordPath::Unreadable(error) => {
            return Look::Failed {
                path: rec.admin.clone(),
                error: error.clone(),
            };
        }
    };
    let path = match guard.resolve(path) {
        Resolved::Inside(folder) => {
            return Look::Protected {
                path: path.clone(),
                folder,
            };
        }
        Resolved::Unresolvable => {
            return Look::Failed {
                path: path.clone(),
                error: "its path has a `..` after a folder that does not exist".into(),
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
    match &rec.blocked {
        Some(Blocked::Protected(folder)) => {
            return Look::Protected {
                path,
                folder: folder.clone(),
            };
        }
        Some(Blocked::Unsafe(error)) => {
            return Look::Failed {
                path,
                error: error.clone(),
            };
        }
        None => {}
    }
    if !rec.symlinks.is_empty() {
        return Look::Failed {
            path,
            error: format!(
                "its record holds a symlink ({}), which git would follow",
                rec.symlinks.join(", ")
            ),
        };
    }
    let [git_dir, work_tree] = git::worktree_args(&rec.admin, &path);
    let run = |cmd: &[&str]| {
        let mut args = vec![git_dir.as_os_str(), work_tree.as_os_str()];
        args.extend(cmd.iter().map(OsStr::new));
        git::stdout(&path, &args, timeout)
    };
    let read = run(&git::STATUS)
        .and_then(|out| git::parse_status(&out))
        .and_then(|status| Ok((status, git::parse_index(&run(&git::LS_FILES)?)?)));
    let (status, index) = match read {
        Ok(r) => r,
        Err(e) => {
            return Look::Failed {
                path,
                error: e.to_string(),
            };
        }
    };
    // Checked out as git checks it before `worktree remove`: the submodule's folder has a
    // `.git`. One that leads into a protected folder is not looked at, and counts.
    let submodules = index
        .gitlinks
        .iter()
        .map(PathBuf::from)
        .filter(|sub| match guard.resolve(&path.join(sub).join(".git")) {
            Resolved::Inside(_) => true,
            Resolved::Outside(p) => fs::symlink_metadata(p).is_ok(),
            Resolved::Unresolvable => false,
        })
        .collect();
    Look::Read {
        path,
        status,
        hidden: index.hidden,
        submodules,
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
    /// Commits at each detached HEAD that no local branch or tag holds, by record.
    detached_only: HashMap<PathBuf, u32>,
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
            Look::Read { status, .. } => status.oid.clone(),
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
        detached_only: HashMap::new(),
    };
    let common = &repo.common;
    let err = |e: anyhow::Error| e.to_string();
    if !heads.is_empty() {
        if let Some(d) = &repo.default {
            facts.merged = merged_heads(common, d, &heads, timeout).map_err(err)?;
        }
        facts.commit_times = commit_times(common, &heads, timeout).map_err(err)?;
        let out = git::stdout(common, &git::STASH_LIST, timeout).map_err(err)?;
        facts.stashes = git::parse_stashes(&out);
    }
    let detached: Vec<Tip> = mine
        .iter()
        .filter_map(|(rec, look)| match look {
            Look::Read { status, .. } if status.branch.is_none() => Some(Tip {
                admin: &rec.admin,
                oid: status.oid.clone()?,
                branch: None,
            }),
            _ => None,
        })
        .collect();
    if !detached.is_empty() {
        facts.detached_only =
            only_here(common, &detached, &["--branches", "--tags"], timeout).map_err(err)?;
    }
    let tips: Vec<Tip> = mine
        .iter()
        .filter_map(|(rec, look)| tip(repo, rec, look, &facts.merged))
        .collect();
    if !tips.is_empty() {
        facts.only_here = only_here(
            common,
            &tips,
            &["--branches", "--remotes", "--tags"],
            timeout,
        )
        .map_err(err);
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
        Look::Read { status: s, .. } => {
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

/// Counts, for each tip, the commits that none of the refs `held_by` names holds but the tip's
/// own branch, in one `rev-list` for the repo: it lists every commit the tips hold that no
/// other ref does, with its parents, and each tip's share is walked from that list. Another
/// detached tip does not count as holding a commit, since removing its worktree drops it too.
fn only_here(
    common: &Path,
    tips: &[Tip],
    held_by: &[&str],
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
    args.extend(held_by);
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

/// What `worktree` needs from the scan as a whole.
struct Context<'a> {
    /// Every worktree folder the scan found.
    known: &'a [PathBuf],
    links_back: &'a HashMap<FileId, Vec<PathBuf>>,
    creators: &'a [Box<dyn CreatorSource>],
    timeout: Duration,
}

/// Whether the record's index stages anything its HEAD lacks, from `git diff --cached`
/// aimed at the record alone. A record without an index stages nothing.
pub fn record_staged(record: &Path, timeout: Duration) -> anyhow::Result<bool> {
    match fs::symlink_metadata(record.join("index")) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
        Ok(m) if !m.is_file() => anyhow::bail!("its record's index is not a file"),
        Ok(_) => {}
    }
    let args = [
        git_dir(record),
        "diff".into(),
        "--cached".into(),
        "--quiet".into(),
        "HEAD".into(),
        "--".into(),
    ];
    let out = git::run(record, &args, timeout)?;
    match out.code {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => anyhow::bail!(
            "git diff --cached on its record failed: {}",
            out.stderr.lines().next().unwrap_or("no message").trim()
        ),
    }
}

/// Commits at `oid` that no local branch or tag holds, read through the record.
pub fn unheld(record: &Path, oid: &str, timeout: Duration) -> anyhow::Result<u32> {
    let args = [
        git_dir(record),
        "rev-list".into(),
        "--count".into(),
        oid.into(),
        "--not".into(),
        "--branches".into(),
        "--tags".into(),
    ];
    let out = git::stdout(record, &args, timeout)?;
    Ok(out.trim().parse()?)
}

fn git_dir(record: &Path) -> std::ffi::OsString {
    let mut a = std::ffi::OsString::from("--git-dir=");
    a.push(record.as_os_str());
    a
}

/// What pruning a missing worktree's record would drop besides the record: staged changes,
/// and on a detached HEAD the commits no local branch or tag holds. A record git would be
/// led astray by is not read.
fn missing_checks(rec: &Record, timeout: Duration) -> Result<(bool, u32), String> {
    match &rec.blocked {
        Some(Blocked::Protected(folder)) => {
            return Err(format!(
                "its record leads into protected folder {}",
                folder.display()
            ));
        }
        Some(Blocked::Unsafe(e)) => return Err(e.clone()),
        None => {}
    }
    if !rec.symlinks.is_empty() {
        return Err(format!(
            "its record holds a symlink ({}), which git would follow",
            rec.symlinks.join(", ")
        ));
    }
    let err = |e: anyhow::Error| e.to_string();
    let staged = record_staged(&rec.admin, timeout).map_err(err)?;
    let detached_only = match &rec.head {
        RecordHead::Detached(oid) => unheld(&rec.admin, oid, timeout).map_err(err)?,
        _ => 0,
    };
    Ok((staged, detached_only))
}

fn worktree(
    repo: &Repo,
    rec: &Record,
    look: Look,
    facts: &Result<RepoFacts, String>,
    ctx: &Context,
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
    let shown = match &rec.path {
        RecordPath::Folder(p) => p.clone(),
        RecordPath::Absent | RecordPath::Unreadable(_) => rec.admin.clone(),
    };
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
            None => match missing_checks(rec, ctx.timeout) {
                Err(e) => (shown, record_head, not_read(e)),
                Ok((staged, detached_only)) => {
                    // The record's folder is gone, so any folder that links back was moved
                    // there.
                    let moved_to = fs::metadata(&rec.admin)
                        .ok()
                        .and_then(|m| ctx.links_back.get(&file_id(&m)))
                        .and_then(|dirs| dirs.first())
                        .cloned();
                    let reading = Reading::Missing {
                        detached,
                        only_here,
                        detached_only,
                        staged,
                        record_extras: rec.extras.clone(),
                        moved_to,
                    };
                    (shown, record_head, judge(reading, lock))
                }
            },
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
        Look::Read {
            path,
            status,
            hidden,
            submodules,
        } => {
            let head = match (&status.branch, &status.oid) {
                (Some(name), oid) => Head::Branch {
                    name: name.clone(),
                    oid: oid.clone(),
                },
                (None, Some(oid)) => Head::Detached { oid: oid.clone() },
                (None, None) => Head::Unknown,
            };
            let state = match facts {
                Ok(f) => {
                    let per_worktree = PerWorktree {
                        hidden,
                        submodules,
                        worktrees_inside: ctx
                            .known
                            .iter()
                            .filter(|k| **k != path && k.starts_with(&path))
                            .cloned()
                            .collect(),
                        repos_inside: ReposInside::Unknown,
                        record_extras: rec.extras.clone(),
                        detached_only: f.detached_only.get(&rec.admin).copied().unwrap_or(0),
                    };
                    let facts = git_facts(repo, &status, f, per_worktree, only_here);
                    judge(Reading::Present(facts), lock)
                }
                Err(e) => not_read(e.clone()),
            };
            (path, head, state)
        }
    };
    let signed: Vec<(&str, Agent)> = rec
        .agent_files
        .iter()
        .filter_map(|f| Some((f.name, f.agent?)))
        .collect();
    let clues = Clues {
        path: &path,
        lock_reason: lock.and_then(|l| l.reason.as_deref()),
        record_files: &signed,
    };
    Worktree {
        creator: creator::attribute(ctx.creators, &clues),
        path,
        repo: repo.shown_as.clone(),
        head,
        state,
        lock: rec.lock.clone(),
        size: None,
        created_at: rec.created_at,
        record: Some(rec.admin.clone()),
    }
}

fn git_facts(
    repo: &Repo,
    s: &git::Status,
    f: &RepoFacts,
    per_worktree: PerWorktree,
    only_here: Option<u32>,
) -> GitFacts {
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
    // On a branch, an entry counts when its message names that branch. On a detached HEAD,
    // which no message names, an entry counts when it was made at HEAD's commit.
    let stashes = f
        .stashes
        .iter()
        .filter(|st| match &s.branch {
            Some(b) => st.branch.as_ref() == Some(b),
            None => s.oid.as_ref() == Some(&st.base),
        })
        .count() as u32;
    GitFacts {
        changes,
        per_worktree,
        merge,
        upstream,
        stashes,
        only_here,
        last_commit: s.oid.as_ref().and_then(|o| f.commit_times.get(o)).copied(),
        fetched_at: repo.fetched_at,
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
    use crate::worktree::Concern;
    use crate::worktree::tests::{add, commit, git, new_repo, write};

    /// Reads one repo's first worktree, with `between` run after the repo was listed and
    /// before its folder was read, then sizes it as `measure` does.
    fn read_first(app: &Path, base: &Path, between: impl FnOnce()) -> Worktree {
        let timeout = Duration::from_secs(5);
        let guard = Protected::new(base);
        let repo = read_repo(&app.join(".git"), &guard, timeout);
        between();
        let rec = &repo.records[0];
        let look = look(rec, &guard, timeout);
        let facts = repo_facts(&repo, &[(rec, &look)], timeout);
        let known: Vec<PathBuf> = look.folder().cloned().into_iter().collect();
        let ctx = Context {
            known: &known,
            links_back: &HashMap::new(),
            creators: &[],
            timeout,
        };
        let w = worktree(&repo, rec, look, &facts, &ctx);
        let mut scan = WorktreeScan {
            worktrees: vec![w],
            roots: Vec::new(),
            repos: 1,
            warnings: Vec::new(),
            timings: Timings::default(),
        };
        measure(&mut scan, base);
        scan.worktrees.remove(0)
    }

    /// HEAD and cleanliness come from one `git status`, so a commit made after the repo was
    /// listed is never read as merged.
    #[test]
    fn a_commit_made_after_listing_is_not_read_as_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let app = base.join("code/app");
        new_repo(&base, &app);
        let wt = add(&app, "feat-race", "app-race");
        let w = read_first(&app, &base, || commit(&wt, "late.txt"));
        let WorktreeState::Active(f) = &w.state else {
            panic!("{:?}", w.state);
        };
        assert!(matches!(f.merge, Merge::Unmerged { .. }), "{f:?}");
    }

    /// The repo had no stash when it was listed; the first one, made before its folder was
    /// read, still counts.
    #[test]
    fn the_first_stash_made_during_a_scan_is_seen() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let app = base.join("code/app");
        new_repo(&base, &app);
        let wt = add(&app, "feat-stash", "app-stash");
        write(&wt.join("README"), "draft\n");
        let w = read_first(&app, &base, || {
            git(&wt, &["stash", "push", "-q", "-m", "draft"]);
        });
        let WorktreeState::NeedsLook { concerns, .. } = &w.state else {
            panic!("{:?}", w.state);
        };
        assert_eq!(concerns, &[Concern::Stashed { entries: 1 }]);
    }
}
