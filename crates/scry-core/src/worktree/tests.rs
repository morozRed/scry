//! Builds real repos and worktrees in a temporary home, one for each state, and checks what a
//! scan and the Worktrees view say about each.

use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::view::worktrees::{WorktreeRow, WorktreesView};

mod loss;

/// Runs git to build a fixture, away from the user's config and from any `GIT_*` variable.
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.as_bytes().starts_with(b"GIT_") {
            cmd.env_remove(key);
        }
    }
    let out = cmd
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Scry Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub(crate) fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn write_mib(path: &Path, seed: u8) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = fs::File::create(path).unwrap();
    let data: Vec<u8> = (0..1 << 20)
        .map(|i: u32| (i as u8).wrapping_mul(7) ^ seed)
        .collect();
    f.write_all(&data).unwrap();
    f.sync_all().unwrap();
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

pub(crate) fn commit(dir: &Path, file: &str) {
    write(&dir.join(file), &format!("{file}\n"));
    git(dir, &["add", file]);
    git(dir, &["commit", "-qm", file]);
}

/// A clone at `dir` of a bare remote that holds one commit on `main`.
pub(crate) fn new_repo(base: &Path, dir: &Path) {
    let name = dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .replace('\n', "-");
    let seed = base.join(format!("seed-{name}"));
    let remote = base.join(format!("{name}.git"));
    write(&seed.join("README"), "hello\n");
    write(&seed.join(".gitignore"), "node_modules/\n");
    git(base, &["init", "-q", s(&seed)]);
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "first"]);
    git(base, &["clone", "-q", "--bare", s(&seed), s(&remote)]);
    fs::create_dir_all(dir.parent().unwrap()).unwrap();
    git(base, &["clone", "-q", s(&remote), s(dir)]);
}

/// A worktree of `repo` on a new branch, in the folder beside it called `name`.
pub(crate) fn add(repo: &Path, branch: &str, name: &str) -> PathBuf {
    let path = repo.parent().unwrap().join(name);
    git(repo, &["worktree", "add", "-q", "-b", branch, s(&path)]);
    path
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A home with one repo under `code/` and a worktree in each state.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let app = code.join("app");
    new_repo(&base, &app);

    // A submodule has a .git file too, and is not a worktree.
    let lib = base.join("lib");
    write(&lib.join("lib.rs"), "\n");
    git(&base, &["init", "-q", s(&lib)]);
    git(&lib, &["add", "."]);
    git(&lib, &["commit", "-qm", "lib"]);
    git(&app, &["submodule", "add", "-q", s(&lib), "vendor/lib"]);
    git(&app, &["commit", "-qm", "add lib"]);
    git(&app, &["push", "-q", "origin", "main"]);

    // Merged: a branch with a commit, merged into main and pushed.
    let merged = add(&app, "feat-merged", "app-merged");
    commit(&merged, "feature.txt");
    git(
        &app,
        &["merge", "-q", "--no-ff", "feat-merged", "-m", "merge"],
    );
    git(&app, &["push", "-q", "origin", "main"]);

    // pnpm-style: a dependency cloned from a store outside the worktree.
    write_mib(&base.join("store/pkg/index.js"), 1);
    write_mib(&merged.join("node_modules/own.bin"), 2);
    let clone_to = merged.join("node_modules/pkg/index.js");
    fs::create_dir_all(clone_to.parent().unwrap()).unwrap();
    let c = |p: &Path| CString::new(p.as_os_str().as_bytes()).unwrap();
    let rc = unsafe {
        libc::clonefile(
            c(&base.join("store/pkg/index.js")).as_ptr(),
            c(&clone_to).as_ptr(),
            0,
        )
    };
    assert_eq!(rc, 0, "clonefile: {}", std::io::Error::last_os_error());

    // Dirty: a tracked change, an untracked file and an untracked folder.
    let dirty = add(&app, "feat-dirty", "app-dirty");
    write(&dirty.join("README"), "changed\n");
    write(&dirty.join("new.txt"), "new\n");
    write(&dirty.join("newdir/a"), "a\n");
    write(&dirty.join("newdir/b"), "b\n");

    // Pushed: every commit is on its upstream, which still exists.
    let pushed = add(&app, "feat-pushed", "app-pushed");
    commit(&pushed, "pushed.txt");
    git(&pushed, &["push", "-q", "-u", "origin", "feat-pushed"]);

    // Ahead: pushed, then one more commit that only this worktree has.
    let ahead = add(&app, "feat-ahead", "app-ahead");
    commit(&ahead, "ahead.txt");
    git(&ahead, &["push", "-q", "-u", "origin", "feat-ahead"]);
    commit(&ahead, "ahead2.txt");

    // Upstream gone: pushed with tracking, then deleted on the remote and pruned here. Its
    // commit was never merged, as after a pull request closed without merging.
    let gone = add(&app, "feat-gone", "app-gone");
    commit(&gone, "gone.txt");
    git(&gone, &["push", "-q", "-u", "origin", "feat-gone"]);
    git(&app, &["push", "-q", "origin", "--delete", "feat-gone"]);
    git(&app, &["fetch", "-q", "--prune"]);

    // Stashed: merged and clean, but its edit was stashed on its branch.
    let stash = add(&app, "feat-stash", "app-stash");
    write(&stash.join("README"), "stashed\n");
    git(&stash, &["stash", "-q"]);

    // Registered, folder deleted. Its branch keeps a commit no other ref has.
    let orphan = add(&app, "feat-orphan", "app-orphan");
    commit(&orphan, "orphan.txt");
    fs::remove_dir_all(&orphan).unwrap();

    // Registered, folder left behind without its .git file but with edits in it.
    let unusable = add(&app, "feat-unusable", "app-unusable");
    write(&unusable.join("README"), "edited\n");
    write(&unusable.join("notes.txt"), "notes\n");
    fs::remove_file(unusable.join(".git")).unwrap();

    // Locked by hand, folder missing, as when it sits on a drive that is not mounted.
    let away = add(&app, "feat-away", "app-away");
    git(
        &app,
        &[
            "worktree",
            "lock",
            "--reason",
            "on an external drive",
            s(&away),
        ],
    );
    fs::remove_dir_all(&away).unwrap();

    // Detached, with a commit no branch has, folder deleted: pruning the record loses it.
    let lost = code.join("app-lost");
    git(&app, &["worktree", "add", "-q", "--detach", s(&lost)]);
    commit(&lost, "lost.txt");
    fs::remove_dir_all(&lost).unwrap();

    // A Claude Code subagent's worktree, locked the way Claude Code locks it.
    git(
        &app,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent-abc",
            ".claude/worktrees/agent-abc",
        ],
    );
    git(
        &app,
        &[
            "worktree",
            "lock",
            "--reason",
            "claude agent agent-abc (pid 4242 start Mon Jan  5 09:00:00 2026)",
            ".claude/worktrees/agent-abc",
        ],
    );

    // A Codex worktree, outside the code roots.
    let codex = home.join(".codex/worktrees/1a2b");
    fs::create_dir_all(&codex).unwrap();
    git(
        &app,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "codex-task",
            s(&codex.join("app")),
        ],
    );

    // A .git file that points at a repo deleted long ago, next to files that may be work.
    write(
        &code.join("stale/.git"),
        &format!("gitdir: {}\n", s(&base.join("gone/.git/worktrees/stale"))),
    );
    write(&code.join("stale/draft.md"), "draft\n");

    // Clean by git's default status, but with an untracked file the repo's config hides.
    let hidden = add(&app, "feat-hidden", "app-hidden");
    write(&hidden.join("secret.txt"), "work\n");
    // Clean by git's default status, but with an edit inside a submodule.
    let sub = add(&app, "feat-sub", "app-sub");
    git(&sub, &["submodule", "update", "--init", "-q"]);
    write(&sub.join("vendor/lib/lib.rs"), "edited\n");
    git(&app, &["config", "status.showUntrackedFiles", "no"]);
    git(&app, &["config", "diff.ignoreSubmodules", "all"]);

    Fixture { _tmp: tmp, home }
}

/// The second root starts inside the checkout, so the walk meets the submodule's `.git` file
/// and has to tell it from a worktree.
fn scan_fixture(f: &Fixture) -> WorktreeScan {
    let roots = vec![f.home.join("code"), f.home.join("code/app/vendor")];
    let mut scan = scan(&ScanOptions::new(&f.home, roots));
    measure(&mut scan, &f.home);
    scan
}

fn view(f: &Fixture) -> WorktreesView {
    WorktreesView::new(&scan_fixture(f), now(), &f.home)
}

fn row<'a>(v: &'a WorktreesView, name: &str) -> &'a WorktreeRow {
    v.rows.iter().find(|r| r.name == name).unwrap_or_else(|| {
        let names: Vec<&str> = v.rows.iter().map(|r| r.name.as_str()).collect();
        panic!("{name:?} not in {names:?}")
    })
}

fn says(r: &WorktreeRow, text: &str) -> bool {
    r.sentence.contains(text) || r.evidence.iter().any(|e| e.contains(text))
}

#[test]
fn reads_each_state_and_skips_the_submodule() {
    let f = fixture();
    let scan = scan_fixture(&f);
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    let v = WorktreesView::new(&scan, now(), &f.home);
    let words: Vec<(&str, &str)> = v
        .rows
        .iter()
        .map(|r| (r.name.as_str(), r.status.word()))
        .collect();
    let mut want = vec![
        ("app-merged", "can go"),
        ("1a2b/app", "can go"),
        ("app-pushed", "can go"),
        ("app-orphan", "can prune"),
        ("app-gone", "needs a look"),
        ("app-stash", "needs a look"),
        ("app-unusable", "needs a look"),
        ("app-away", "needs a look"),
        ("app-lost", "needs a look"),
        ("stale", "needs a look"),
        ("app-dirty", "dirty"),
        ("app-hidden", "dirty"),
        ("app-sub", "dirty"),
        ("agent-abc", "locked"),
        ("app-ahead", "active"),
    ];
    let mut got = words.clone();
    want.sort();
    got.sort();
    assert_eq!(got, want, "the submodule is not a worktree");
    assert_eq!(row(&v, "stale").repo, "gone");
    assert!(
        scan.worktrees
            .iter()
            .find(|w| w.name() == "agent-abc")
            .unwrap()
            .created_at
            .is_some()
    );
}

#[test]
fn hidden_untracked_files_and_submodule_edits_count_as_changes() {
    let f = fixture();
    let v = view(&f);
    for name in ["app-hidden", "app-sub"] {
        let r = row(&v, name);
        assert!(!r.status.removable(), "{name}: {}", r.sentence);
        assert_eq!(r.status.word(), "dirty", "{name}: {}", r.sentence);
    }
    assert!(
        row(&v, "app-hidden")
            .sentence
            .contains("has 1 untracked entry;"),
        "{}",
        row(&v, "app-hidden").sentence
    );
    assert!(
        row(&v, "app-sub").sentence.contains("has 1 changed entry;"),
        "{}",
        row(&v, "app-sub").sentence
    );
}

#[test]
fn can_go_only_with_proof_from_the_default_branch_or_a_live_upstream() {
    let f = fixture();
    let v = view(&f);
    let mut removable: Vec<&str> = v
        .rows
        .iter()
        .filter(|r| r.status.removable())
        .map(|r| r.name.as_str())
        .collect();
    removable.sort();
    assert_eq!(removable, ["1a2b/app", "app-merged", "app-pushed"]);
    assert!(
        row(&v, "app-pushed").sentence.starts_with(
            "app-pushed is clean and every commit is on origin/feat-pushed as of the last \
             fetch or push (last fetch "
        ),
        "{}",
        row(&v, "app-pushed").sentence
    );
    assert!(
        says(
            row(&v, "app-ahead"),
            "1 commit not pushed to origin/feat-ahead"
        ),
        "{:?}",
        row(&v, "app-ahead")
    );
    let stash = row(&v, "app-stash");
    assert_eq!(
        stash.sentence,
        "app-stash needs a look: 1 stash entry names its branch."
    );
}

#[test]
fn an_orphan_is_never_removable_without_proof() {
    let f = fixture();
    let v = view(&f);
    for (name, why) in [
        ("app-unusable", "its folder has no .git link but holds "),
        ("stale", "which is gone"),
        ("app-away", "locked: on an external drive"),
        ("app-lost", "1 commit exists nowhere else"),
    ] {
        let r = row(&v, name);
        assert!(!r.status.removable(), "{name}: {}", r.sentence);
        assert_eq!(r.status.word(), "needs a look", "{name}: {}", r.sentence);
        assert!(says(r, why), "{name} should say {why:?}: {r:?}");
    }
}

#[test]
fn a_missing_folder_can_be_pruned_and_frees_nothing() {
    let f = fixture();
    let v = view(&f);
    let r = row(&v, "app-orphan");
    assert!(!r.status.removable(), "{}", r.sentence);
    assert_eq!(r.status.word(), "can prune");
    assert_eq!(r.reclaimable, None);
    assert_eq!(
        r.sentence,
        "app-orphan is registered in app but its folder is gone; `git worktree prune` removes \
         only the record and frees almost nothing."
    );
    assert!(
        says(
            r,
            "branch feat-orphan keeps 1 commit that exists nowhere else"
        ),
        "{:?}",
        r.evidence
    );
}

#[test]
fn a_gone_upstream_needs_a_look_and_is_never_called_merged() {
    let f = fixture();
    let v = view(&f);
    let r = row(&v, "app-gone");
    assert!(!r.status.removable(), "{}", r.sentence);
    assert_eq!(
        r.sentence,
        "app-gone needs a look: its upstream origin/feat-gone is gone and 1 commit exists \
         nowhere else."
    );
    assert!(
        says(r, "a squash merge and a closed pull request look the same"),
        "{r:?}"
    );
    assert!(
        !r.evidence.iter().any(|e| e.starts_with("HEAD is in")),
        "{r:?}"
    );
}

#[test]
fn totals_count_only_what_can_go_and_say_at_least() {
    let f = fixture();
    let mut scan = scan_fixture(&f);
    let v = WorktreesView::new(&scan, now(), &f.home);
    let merged = row(&v, "app-merged").reclaimable.expect("sized");
    let sum: u64 = v
        .rows
        .iter()
        .filter(|r| r.status.removable())
        .filter_map(|r| r.reclaimable)
        .sum();
    assert_eq!(v.reclaimable, sum);
    assert!(
        v.summary
            .starts_with("15 worktrees in 2 repos (2 scanned): 3 can go and free at least "),
        "{}",
        v.summary
    );
    assert!(
        row(&v, "app-merged")
            .sentence
            .contains("; deleting it frees at least "),
        "{}",
        row(&v, "app-merged").sentence
    );

    // A worktree that could not be sized shows no size and stays out of the total.
    let i = scan
        .worktrees
        .iter()
        .position(|w| w.name() == "app-merged")
        .unwrap();
    scan.worktrees[i].size = Some(crate::size::SubtreeSize {
        error: Some("not a directory".into()),
        ..Default::default()
    });
    let v = WorktreesView::new(&scan, now(), &f.home);
    let r = row(&v, "app-merged");
    assert_eq!((r.allocated, r.reclaimable), (None, None));
    assert_eq!(v.reclaimable, sum - merged);
    assert!(v.summary.contains("(1 of them not sized)"), "{}", v.summary);
}

#[test]
fn the_view_explains_each_row_in_one_sentence() {
    let f = fixture();
    let v = view(&f);
    let merged = &row(&v, "app-merged").sentence;
    assert!(
        merged
            .starts_with("app-merged is clean and merged into origin/main as of the last fetch or push (last fetch ")
            && merged.contains("; deleting it frees at least "),
        "{merged}"
    );
    assert_eq!(
        row(&v, "app-dirty").sentence,
        "app-dirty is merged into origin/main but has 1 changed entry and 2 untracked \
         entries; deleting it would lose them."
    );
    assert_eq!(
        row(&v, "agent-abc").sentence,
        "agent-abc is locked by a Claude Code agent (pid 4242); git keeps it until it is \
         unlocked."
    );
    assert!(row(&v, "app-merged").path.starts_with("~/code/"));
    assert_eq!(v.rows[0].status.word(), "can go", "what can go ranks first");
}

#[test]
fn names_the_creator_by_path_and_lock_reason() {
    let f = fixture();
    let scan = scan_fixture(&f);
    let find = |name: &str| scan.worktrees.iter().find(|w| w.name() == name).unwrap();

    let agent = find("agent-abc");
    assert_eq!(agent.creator.agent, Some(Agent::ClaudeCode));
    assert_eq!(
        agent.creator.evidence,
        vec![
            CreatorEvidence::Path {
                convention: ".claude/worktrees/agent-*".into()
            },
            CreatorEvidence::LockReason {
                reason: "claude agent agent-abc (pid 4242 start Mon Jan  5 09:00:00 2026)".into()
            },
        ]
    );
    assert_eq!(find("1a2b/app").creator.agent, Some(Agent::Codex));
    assert_eq!(find("app-dirty").creator, ProbableCreator::default());
}

#[test]
fn a_cloned_dependency_is_allocated_but_not_reclaimable() {
    let f = fixture();
    let scan = scan_fixture(&f);
    let w = scan
        .worktrees
        .iter()
        .find(|w| w.name() == "app-merged")
        .unwrap();
    let size = w.size.clone().expect("sized");
    let mib = 1 << 20;
    assert!(size.size.allocated >= 2 * mib, "{size:?}");
    assert!(
        size.size.allocated - size.size.reclaimable >= mib - 64 * 1024,
        "the clone's megabyte is shared with the store: {size:?}"
    );
    assert_eq!(size.skipped, 0);
}

/// Makes `dirs` unreadable while `f` runs, as macOS does to an app it has not asked yet.
fn locked_away<T>(dirs: &[&Path], f: impl FnOnce() -> T) -> T {
    let mode = |d: &Path, m| fs::set_permissions(d, fs::Permissions::from_mode(m)).unwrap();
    dirs.iter().for_each(|d| mode(d, 0o000));
    let out = f();
    dirs.iter().for_each(|d| mode(d, 0o755));
    out
}

#[test]
fn never_reads_inside_desktop_documents_or_downloads() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let [desktop, documents, downloads] =
        ["Desktop", "Documents", "Downloads"].map(|d| home.join(d));
    for d in [&desktop, &documents, &downloads] {
        fs::create_dir_all(d).unwrap();
    }
    let app = code.join("app");
    new_repo(&base, &app);

    // Worktrees of a readable repo kept on the Desktop: by its own path, through a symlink,
    // and through a path cased differently, which names the same folder on APFS.
    git(
        &app,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "desk",
            s(&desktop.join("wt-desk")),
        ],
    );
    std::os::unix::fs::symlink(&desktop, code.join("desk-link")).unwrap();
    git(
        &app,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "link",
            s(&code.join("desk-link/wt-link")),
        ],
    );
    let same = |a: &Path, b: &Path| match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    };
    let case_blind = same(&home.join("desktop"), &desktop);
    if case_blind {
        git(
            &app,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "case",
                s(&home.join("desktop/wt-case")),
            ],
        );
    }

    // A Codex worktree whose repo sits in Documents.
    let proj = documents.join("proj");
    new_repo(&base, &proj);
    let codex = home.join(".codex/worktrees/9z");
    fs::create_dir_all(&codex).unwrap();
    git(
        &proj,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task",
            s(&codex.join("proj")),
        ],
    );

    // A repo in Downloads, named as a root.
    let dl = downloads.join("dl");
    new_repo(&base, &dl);
    add(&dl, "dl-task", "dl-wt");

    let scan = locked_away(&[&desktop, &documents, &downloads], || {
        let mut scan = scan(&ScanOptions::new(
            &home,
            vec![code.clone(), downloads.clone()],
        ));
        measure(&mut scan, &home);
        scan
    });
    let v = WorktreesView::new(&scan, now(), &home);

    let mut inside = vec!["wt-desk", "wt-link"];
    if case_blind {
        inside.push("wt-case");
    }
    for name in inside.iter().chain(&["9z/proj"]) {
        let r = row(&v, name);
        assert!(!r.status.removable(), "{name}: {}", r.sentence);
        assert_eq!(r.status.word(), "not read", "{name}: {}", r.sentence);
        assert!(says(r, "not read: protected folder"), "{name}: {r:?}");
    }
    for name in inside {
        let r = row(&v, name);
        assert_eq!((r.allocated, r.reclaimable), (None, None), "{name}");
    }
    // The Codex folder itself is outside, so it is sized; only its repo is not read.
    assert!(row(&v, "9z/proj").allocated.is_some());
    assert!(
        scan.warnings
            .iter()
            .any(|w| w.contains("Downloads") && w.contains("protected folder")),
        "{:?}",
        scan.warnings
    );
    assert!(
        !v.rows.iter().any(|r| r.name.contains("dl")),
        "{:?}",
        v.rows
    );
}

/// Paths that reach a protected folder through a `..` after a folder that does not exist, and
/// record files that are symlinks into one. None may be read, and none is called removable.
#[test]
fn never_follows_a_dangling_dotdot_or_a_symlinked_record_file_into_a_protected_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let documents = home.join("Documents");
    fs::create_dir_all(&documents).unwrap();
    let app = code.join("app");
    new_repo(&base, &app);
    let record = |name: &str| app.join(".git/worktrees").join(name);
    let dangling = code.join("missing/../../Documents");

    // A record whose gitdir file reaches Documents through `missing/..`.
    let in_docs = documents.join("wt-dotdot");
    git(
        &app,
        &["worktree", "add", "-q", "-b", "dotdot", s(&in_docs)],
    );
    write(
        &record("wt-dotdot").join("gitdir"),
        &format!("{}\n", s(&dangling.join("wt-dotdot/.git"))),
    );
    // A Codex worktree whose .git file does the same.
    let proj = documents.join("proj");
    new_repo(&base, &proj);
    let codex = home.join(".codex/worktrees/ef56");
    git(
        &proj,
        &["worktree", "add", "-q", "-b", "task", s(&codex.join("x"))],
    );
    write(
        &codex.join("x/.git"),
        &format!("gitdir: {}\n", s(&dangling.join("proj/.git/worktrees/x"))),
    );
    // Worktrees outside, whose record files are symlinks to copies inside Documents.
    for (name, file) in [
        ("link-gitdir", "gitdir"),
        ("link-lock", "locked"),
        ("link-head", "HEAD"),
    ] {
        let wt = add(&app, name, name);
        if file == "locked" {
            git(&app, &["worktree", "lock", s(&wt)]);
        }
        let real = record(name).join(file);
        let copy = documents.join(format!("{name}-{file}"));
        fs::rename(&real, &copy).unwrap();
        std::os::unix::fs::symlink(&copy, &real).unwrap();
    }

    let scan = locked_away(&[&documents], || {
        let roots = vec![code.clone(), dangling.clone()];
        let mut scan = scan(&ScanOptions::new(&home, roots));
        measure(&mut scan, &home);
        scan
    });
    let v = WorktreesView::new(&scan, now(), &home);
    assert!(
        scan.warnings
            .iter()
            .any(|w| w.contains("missing/../../Documents was not read")),
        "{:?}",
        scan.warnings
    );
    for (name, word, why) in [
        ("wt-dotdot", "not read", "a folder that does not exist"),
        ("ef56/x", "not read", "cannot be resolved"),
        ("link-gitdir", "not read", "its record's gitdir file"),
        (
            "link-lock",
            "not read",
            "its record holds a symlink (locked)",
        ),
        ("link-head", "not read", "its record holds a symlink (HEAD)"),
    ] {
        let r = row(&v, name);
        assert_eq!(r.status.word(), word, "{name}: {r:?}");
        assert!(says(r, why), "{name} should say {why:?}: {r:?}");
    }
}

/// A record git would read from a protected folder through its own files: an include in its
/// `config.worktree`, directly, under a condition or through another included file, or a
/// `commondir` that leads into one or to another repository's git folder. No git runs on it.
#[test]
fn a_record_that_leads_git_into_a_protected_folder_is_not_read() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let documents = home.join("Documents");
    fs::create_dir_all(&documents).unwrap();
    write(&documents.join("inc.gitconfig"), "[user]\n\tname = x\n");
    let app = code.join("app");
    new_repo(&base, &app);
    git(&app, &["config", "core.repositoryformatversion", "1"]);
    git(&app, &["config", "extensions.worktreeConfig", "true"]);
    let other = code.join("other");
    new_repo(&base, &other);
    let record = |name: &str| app.join(".git/worktrees").join(name);
    let inc = s(&documents.join("inc.gitconfig")).to_string();
    // Outside, but it includes a file in Documents by a path relative to itself.
    write(
        &code.join("outer.gitconfig"),
        "[include]\n\tpath = ../Documents/inc.gitconfig\n",
    );
    write(&code.join("plain.gitconfig"), "[user]\n\tname = y\n");
    let configs = [
        ("inc-docs", format!("[include]\n\tpath = {inc}\n")),
        (
            "inc-if",
            format!("[includeIf \"gitdir:/nowhere/\"]\n\tpath = {inc}\n"),
        ),
        (
            "inc-nested",
            format!("[include]\n\tpath = {}\n", s(&code.join("outer.gitconfig"))),
        ),
        (
            "inc-plain",
            format!("[include]\n\tpath = {}\n", s(&code.join("plain.gitconfig"))),
        ),
    ];
    for (name, config) in &configs {
        add(&app, name, name);
        write(&record(name).join("config.worktree"), config);
    }
    add(&app, "common-docs", "common-docs");
    write(
        &record("common-docs").join("commondir"),
        &format!("{}\n", s(&documents.join("appgit"))),
    );
    add(&app, "common-other", "common-other");
    write(
        &record("common-other").join("commondir"),
        &format!("{}\n", s(&other.join(".git"))),
    );

    let scan = locked_away(&[&documents], || {
        let mut scan = scan(&ScanOptions::new(&home, vec![code.clone()]));
        measure(&mut scan, &home);
        scan
    });
    let v = WorktreesView::new(&scan, now(), &home);
    for name in ["inc-docs", "inc-if", "inc-nested", "common-docs"] {
        let r = row(&v, name);
        assert_eq!(r.status.word(), "not read", "{name}: {r:?}");
        assert!(says(r, "not read: protected folder"), "{name}: {r:?}");
    }
    let r = row(&v, "common-other");
    assert_eq!(r.status.word(), "not read", "{r:?}");
    assert!(says(r, "not the repository's own git folder"), "{r:?}");
    // An include that stays outside is read, and kept for a look as the worktree's own config.
    let r = row(&v, "inc-plain");
    assert_eq!(r.status.word(), "needs a look", "{r:?}");
    assert!(says(r, "config.worktree (include.path)"), "{r:?}");
}

/// The stash check names exactly what it matched, and a worktree kept by a local branch makes
/// no claim about a fetch.
#[test]
fn says_what_the_stash_check_matched_and_dates_only_remote_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    // A repo with no remote: its default branch is the local `main`.
    let solo = code.join("solo");
    fs::create_dir_all(&solo).unwrap();
    git(&solo, &["init", "-q"]);
    commit(&solo, "README");
    add(&solo, "done", "solo-done");
    let detached = solo.parent().unwrap().join("solo-detached");
    git(
        &solo,
        &["worktree", "add", "-q", "--detach", s(&detached), "main"],
    );
    write(&detached.join("README"), "changed\n");
    git(&detached, &["stash", "-q"]);
    // A cloned repo, where `origin/main` keeps the worktree.
    let app = code.join("app");
    new_repo(&base, &app);
    add(&app, "merged", "app-merged");

    let mut scan = scan(&ScanOptions::new(&home, vec![code.clone()]));
    measure(&mut scan, &home);
    let v = WorktreesView::new(&scan, now(), &home);

    let r = row(&v, "solo-done");
    assert_eq!(r.status.word(), "can go", "{r:?}");
    assert!(
        r.sentence
            .starts_with("solo-done is clean and merged into main; "),
        "{}",
        r.sentence
    );
    let finished = r
        .evidence
        .iter()
        .find(|e| e.starts_with("finished:"))
        .unwrap();
    assert_eq!(
        finished,
        "finished: main holds every commit of HEAD, and no stash entry names its branch"
    );

    let r = row(&v, "solo-detached");
    assert_eq!(r.status.word(), "needs a look", "{r:?}");
    assert!(
        says(r, "1 stash entry was made at its HEAD commit"),
        "{r:?}"
    );

    let r = row(&v, "app-merged");
    assert_eq!(r.status.word(), "can go", "{r:?}");
    assert!(
        r.sentence
            .contains("merged into origin/main as of the last fetch or push ("),
        "{}",
        r.sentence
    );
    assert!(
        says(r, "remote branches are as of the last fetch or push")
            || says(r, "no fetch on record"),
        "{r:?}"
    );
}

/// Documents is a symlink to a folder under `.claude/worktrees`, and another entry there is a
/// symlink into Documents. Neither is entered.
#[test]
fn an_agent_folder_entry_that_is_or_leads_into_a_protected_folder_is_not_read() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let app = code.join("app");
    new_repo(&base, &app);
    let agents = app.join(".claude/worktrees");
    let private = agents.join("private");
    git(
        &app,
        &["worktree", "add", "-q", "-b", "private", s(&private)],
    );
    fs::create_dir_all(&home).unwrap();
    std::os::unix::fs::symlink(&private, home.join("Documents")).unwrap();
    let desktop = home.join("Desktop");
    fs::create_dir_all(desktop.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(desktop.join("elsewhere"), agents.join("via-link")).unwrap();

    let scan = locked_away(&[&private, &desktop], || {
        let mut scan = scan(&ScanOptions::new(&home, vec![code.clone()]));
        measure(&mut scan, &home);
        scan
    });
    let v = WorktreesView::new(&scan, now(), &home);
    for name in ["private", "via-link"] {
        let r = row(&v, name);
        assert_eq!(r.status.word(), "not read", "{name}: {r:?}");
        assert!(says(r, "not read: protected folder"), "{name}: {r:?}");
    }
}

#[test]
fn a_commit_only_records_hold_is_kept_only_by_a_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let app = code.join("app");
    new_repo(&base, &app);
    let detach = |name: &str, at: &str| {
        let path = code.join(name);
        git(&app, &["worktree", "add", "-q", "--detach", s(&path), at]);
        path
    };
    let head = |dir: &Path| git(dir, &["rev-parse", "HEAD"]).trim().to_string();

    // Two detached records at one commit no branch has: pruning both would lose it.
    let one = detach("det-one", "HEAD");
    commit(&one, "shared.txt");
    let two = detach("det-two", &head(&one));
    // A branch and a detached record at one commit: the branch keeps it.
    let kept = add(&app, "feat-kept", "br-kept");
    commit(&kept, "kept.txt");
    let det = detach("det-kept", &head(&kept));
    for dir in [&one, &two, &kept, &det] {
        fs::remove_dir_all(dir).unwrap();
    }

    let scan = scan(&ScanOptions::new(&home, vec![code.clone()]));
    let v = WorktreesView::new(&scan, now(), &home);
    for name in ["det-one", "det-two"] {
        let r = row(&v, name);
        assert_eq!(r.status.word(), "needs a look", "{name}: {}", r.sentence);
        assert!(says(r, "1 commit exists nowhere else"), "{name}: {r:?}");
    }
    assert_eq!(row(&v, "det-kept").status.word(), "can prune");
    let r = row(&v, "br-kept");
    assert_eq!(r.status.word(), "can prune");
    assert!(says(r, "keeps 1 commit that exists nowhere else"), "{r:?}");
}

#[test]
fn paths_with_a_newline_are_read_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");

    // A repo outside the roots, reached only through its Codex worktree.
    let odd = base.join("odd\nrepo");
    new_repo(&base, &odd);
    let codex = home.join(".codex/worktrees/5e");
    fs::create_dir_all(&codex).unwrap();
    git(
        &odd,
        &["worktree", "add", "-q", "-b", "task", s(&codex.join("app"))],
    );
    // A worktree whose own folder name holds a newline.
    let app = code.join("app");
    new_repo(&base, &app);
    add(&app, "nl", "new\nline");

    let mut scan = scan(&ScanOptions::new(&home, vec![code.clone()]));
    measure(&mut scan, &home);
    let v = WorktreesView::new(&scan, now(), &home);
    for name in ["app", "new\nline"] {
        let r = row(&v, name);
        assert_eq!(r.status.word(), "can go", "{name}: {r:?}");
    }
    assert_eq!(row(&v, "app").repo, "odd\nrepo");
}

/// A git hook runs with `GIT_DIR`, `GIT_INDEX_FILE` and others set. A scan started from one
/// must still read each repo for itself, so this runs `scan_inside_a_hook` with them set.
#[test]
fn ignores_git_variables_from_a_hook() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "worktree::tests::scan_inside_a_hook",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("GIT_DIR", "/nonexistent/.git")
        .env("GIT_COMMON_DIR", "/nonexistent/.git")
        .env("GIT_WORK_TREE", "/nonexistent")
        .env("GIT_INDEX_FILE", "/nonexistent/.git/index")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("scanned inside a hook"), "{text}");
}

#[test]
#[ignore = "run by ignores_git_variables_from_a_hook, with a hook's environment"]
fn scan_inside_a_hook() {
    // Run on its own, without the hook's environment, it has nothing to show.
    if std::env::var_os("GIT_DIR").is_none() {
        return;
    }
    let f = fixture();
    let v = view(&f);
    assert!(row(&v, "app-merged").status.removable(), "{:?}", v.rows);
    assert_eq!(row(&v, "app-dirty").status.word(), "dirty");
    println!("scanned inside a hook");
}
