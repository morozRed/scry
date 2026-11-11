//! Builds real repos and worktrees in a temporary home and checks what a scan reports.

use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::view::{Status, WorktreesView};
use super::*;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
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
}

fn write(path: &Path, text: &str) {
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

/// A home with one repo under `code/` and one worktree in each state.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    let seed = base.join("seed");
    let remote = base.join("remote.git");
    let app = code.join("app");

    write(&seed.join("README"), "hello\n");
    write(&seed.join(".gitignore"), "node_modules/\n");
    git(&base, &["init", "-q", s(&seed)]);
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "first"]);
    git(&base, &["clone", "-q", "--bare", s(&seed), s(&remote)]);
    fs::create_dir_all(&code).unwrap();
    git(&code, &["clone", "-q", s(&remote), "app"]);

    // Merged: a branch with a commit, merged into main and pushed.
    git(
        &app,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat-merged",
            "../app-merged",
        ],
    );
    write(&code.join("app-merged/feature.txt"), "done\n");
    git(&code.join("app-merged"), &["add", "."]);
    git(&code.join("app-merged"), &["commit", "-qm", "feature"]);
    git(
        &app,
        &["merge", "-q", "--no-ff", "feat-merged", "-m", "merge"],
    );
    git(&app, &["push", "-q", "origin", "main"]);

    // pnpm-style: a dependency cloned from a store outside the worktree.
    write_mib(&base.join("store/pkg/index.js"), 1);
    write_mib(&code.join("app-merged/node_modules/own.bin"), 2);
    let clone_to = code.join("app-merged/node_modules/pkg/index.js");
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

    // Dirty: one tracked change, one untracked file.
    git(
        &app,
        &["worktree", "add", "-q", "-b", "feat-dirty", "../app-dirty"],
    );
    write(&code.join("app-dirty/README"), "changed\n");
    write(&code.join("app-dirty/new.txt"), "new\n");

    // Upstream gone: pushed with tracking, then deleted on the remote and pruned here.
    git(
        &app,
        &["worktree", "add", "-q", "-b", "feat-gone", "../app-gone"],
    );
    write(&code.join("app-gone/gone.txt"), "x\n");
    git(&code.join("app-gone"), &["add", "."]);
    git(&code.join("app-gone"), &["commit", "-qm", "gone"]);
    git(
        &code.join("app-gone"),
        &["push", "-q", "-u", "origin", "feat-gone"],
    );
    git(&app, &["push", "-q", "origin", "--delete", "feat-gone"]);
    git(&app, &["fetch", "-q", "--prune"]);

    // Orphaned: registered, folder deleted.
    git(
        &app,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat-orphan",
            "../app-orphan",
        ],
    );
    fs::remove_dir_all(code.join("app-orphan")).unwrap();

    // Orphaned: registered, folder left behind without its .git file.
    git(
        &app,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat-unusable",
            "../app-unusable",
        ],
    );
    fs::remove_file(code.join("app-unusable/.git")).unwrap();

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
            "claude agent agent-abc (pid 42 start Wed Nov 11 16:14:45 2026)",
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

    // A submodule has a .git file too, and is not a worktree.
    let lib = base.join("lib");
    write(&lib.join("lib.rs"), "\n");
    git(&base, &["init", "-q", s(&lib)]);
    git(&lib, &["add", "."]);
    git(&lib, &["commit", "-qm", "lib"]);
    git(&app, &["submodule", "add", "-q", s(&lib), "vendor/lib"]);
    git(&app, &["commit", "-qm", "add lib"]);

    // A .git file that points at a repo deleted long ago.
    write(
        &code.join("stale/.git"),
        &format!("gitdir: {}\n", s(&base.join("gone/.git/worktrees/stale"))),
    );

    Fixture { _tmp: tmp, home }
}

/// The second root starts inside the checkout, so the walk meets the submodule's `.git` file
/// and has to ask git what it is.
fn scan_fixture(f: &Fixture) -> WorktreeScan {
    let roots = vec![f.home.join("code"), f.home.join("code/app/vendor")];
    let mut scan = scan(&ScanOptions::new(&f.home, roots));
    measure(&mut scan, &f.home);
    scan
}

fn find<'a>(scan: &'a WorktreeScan, name: &str) -> &'a Worktree {
    scan.worktrees
        .iter()
        .find(|w| w.name() == name)
        .unwrap_or_else(|| {
            let names: Vec<String> = scan.worktrees.iter().map(Worktree::name).collect();
            panic!("{name} not in {names:?}")
        })
}

fn facts(w: &Worktree) -> &GitFacts {
    match &w.state {
        WorktreeState::Present(f) => f,
        other => panic!("{} is {other:?}", w.name()),
    }
}

#[test]
fn reads_each_worktree_state_and_skips_the_submodule() {
    let f = fixture();
    let scan = scan_fixture(&f);
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
    let names: Vec<String> = scan.worktrees.iter().map(Worktree::name).collect();
    assert_eq!(scan.worktrees.len(), 8, "{names:?}");
    assert!(
        !names.iter().any(|n| n.contains("lib")),
        "the submodule is not a worktree: {names:?}"
    );

    let merged = find(&scan, "app-merged");
    assert_eq!(Status::of(merged), Status::Merged);
    assert_eq!(
        facts(merged).merge,
        Merge::Merged {
            into: "origin/main".into()
        }
    );
    assert_eq!(facts(merged).changes, Changes::Clean);
    assert!(facts(merged).last_commit.is_some());

    let dirty = find(&scan, "app-dirty");
    assert_eq!(Status::of(dirty), Status::Dirty);
    assert_eq!(
        facts(dirty).changes,
        Changes::Dirty {
            changed: 1,
            untracked: 1
        }
    );

    let gone = find(&scan, "app-gone");
    assert_eq!(Status::of(gone), Status::Gone);
    assert_eq!(
        facts(gone).upstream,
        Upstream::Gone {
            name: "origin/feat-gone".into()
        }
    );
    assert!(matches!(facts(gone).merge, Merge::Unmerged { .. }));

    let orphan = find(&scan, "app-orphan");
    assert!(
        matches!(
            &orphan.state,
            WorktreeState::Orphaned(Orphan::Missing {
                git_says: Some(_),
                admin_dir: Some(_)
            })
        ),
        "{:?}",
        orphan.state
    );
    assert!(orphan.size.is_none());

    let unusable = find(&scan, "app-unusable");
    assert!(matches!(
        unusable.state,
        WorktreeState::Orphaned(Orphan::Unusable { .. })
    ));
    assert!(unusable.size.is_some());

    let stale = find(&scan, "stale");
    assert!(matches!(
        stale.state,
        WorktreeState::Orphaned(Orphan::BrokenPointer { .. })
    ));
    assert_eq!(stale.repo_name(), "gone");

    let agent = find(&scan, "agent-abc");
    assert_eq!(Status::of(agent), Status::Locked);
    assert!(agent.created_at.is_some());
}

#[test]
fn names_the_creator_by_path_and_lock_reason() {
    let f = fixture();
    let scan = scan_fixture(&f);

    let agent = find(&scan, "agent-abc");
    assert_eq!(agent.creator.agent, Some(Agent::ClaudeCode));
    assert_eq!(
        agent.creator.evidence,
        vec![
            CreatorEvidence::Path {
                convention: ".claude/worktrees/agent-*".into()
            },
            CreatorEvidence::LockReason {
                reason: "claude agent agent-abc (pid 42 start Wed Nov 11 16:14:45 2026)".into()
            },
        ]
    );

    let codex = find(&scan, "1a2b/app");
    assert_eq!(codex.creator.agent, Some(Agent::Codex));
    assert!(matches!(Status::of(codex), Status::Merged));

    assert_eq!(find(&scan, "app-dirty").creator, ProbableCreator::default());
}

#[test]
fn a_cloned_dependency_is_allocated_but_not_reclaimable() {
    let f = fixture();
    let scan = scan_fixture(&f);
    let size = find(&scan, "app-merged").size.clone().expect("sized");
    let mib = 1 << 20;
    assert!(size.size.allocated >= 2 * mib, "{size:?}");
    assert!(
        size.size.allocated - size.size.reclaimable >= mib - 64 * 1024,
        "the clone's megabyte is shared with the store: {size:?}"
    );
    assert_eq!(size.skipped, 0);
}

#[test]
fn the_view_explains_each_row_in_one_sentence() {
    let f = fixture();
    let scan = scan_fixture(&f);
    let view = WorktreesView::new(&scan, i64::MAX / 2, &f.home);
    let row = |name: &str| view.rows.iter().find(|r| r.name == name).unwrap();

    let merged = &row("app-merged").sentence;
    assert!(
        merged.starts_with("app-merged is merged into origin/main and clean; deleting it frees "),
        "{merged}"
    );
    assert_eq!(
        row("app-dirty").sentence,
        "app-dirty is merged into origin/main but has 1 changed file and 1 untracked; deleting it would lose them."
    );
    assert!(
        row("app-gone")
            .sentence
            .starts_with("app-gone tracks origin/feat-gone, which is gone, and is clean"),
        "{}",
        row("app-gone").sentence
    );
    assert_eq!(
        row("app-orphan").sentence,
        "app-orphan is registered in app but its folder is gone; `git worktree prune` clears the record."
    );
    assert_eq!(
        row("agent-abc").sentence,
        "agent-abc is locked by a Claude Code agent (pid 42); git keeps it until it is unlocked."
    );
    assert!(row("app-merged").path.starts_with("~/code/"));
    assert_eq!(view.rows[0].status, Status::Orphaned, "orphans rank first");
    assert!(
        view.summary
            .starts_with("8 worktrees in 2 repos; 6 can go and free "),
        "{}",
        view.summary
    );
}
