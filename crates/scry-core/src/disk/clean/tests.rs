//! Every cleanup path on folders made in a temporary home. A test that moves something to
//! the Trash puts it back.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::disk::cache::SizeCache;
use crate::disk::{Layout, catalog, read};
use crate::worktree::tests::{add, git, new_repo};

struct Home {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    store: Store,
}

fn home() -> Home {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let store = Store::open(&home.join("scry.db")).unwrap();
    Home {
        _tmp: tmp,
        home,
        store,
    }
}

/// The catalog items of a home, measured without a cache.
fn items(home: &Path) -> Vec<Item> {
    let found = catalog::locate(home, &[home.join("code")], &[]);
    let mut layout = Layout::new(&found, None, home, 0, &Default::default());
    let cache = SizeCache::new(None);
    let look = cache.look(&layout.parts, home);
    let (sizes, _) = cache.measure(&layout.parts, &look, home, &layout.exclude);
    let sizes: Vec<_> = sizes.into_iter().map(Some).collect();
    layout.fill(&sizes, &vec![false; sizes.len()]);
    layout.items
}

fn item(home: &Path, id: &str) -> Item {
    items(home)
        .into_iter()
        .find(|i| i.id == id)
        .unwrap_or_else(|| panic!("no item {id}"))
}

fn never_relabel(_: &Plan, _: &Path) -> Result<Status, Refusal> {
    panic!("only worktrees are relabelled")
}

fn ctx<'a>(h: &'a Home, destructive: bool) -> Context<'a> {
    Context {
        home: &h.home,
        store: Some(&h.store),
        actor: "test",
        destructive,
        label: &relabel,
    }
}

/// Puts a trashed folder back where it was, so the user's Trash is left as it was.
fn put_back(done: &Done) {
    let landed = done.moved_to.as_ref().expect("moved");
    fs::rename(landed, &done.path).unwrap();
}

fn audit(h: &Home) -> Vec<(String, String)> {
    h.store
        .audit_rows()
        .unwrap()
        .into_iter()
        .map(|r| (r.action, r.result))
        .collect()
}

#[test]
fn allows_follows_the_catalog_and_destructive_false_turns_actions_off() {
    let h = home();
    let mk = |p: &str| fs::create_dir_all(h.home.join(p)).unwrap();
    mk(".npm/x");
    mk(".claude/projects/p");
    mk("Library/Developer/CoreSimulator/Devices");
    mk("Library/Caches/com.example");
    mk("code/web/node_modules/react");
    let docker = h
        .home
        .join("Library/Containers/com.docker.docker/Data/vms/0/data");
    fs::create_dir_all(&docker).unwrap();
    fs::write(docker.join("Docker.raw"), b"disk").unwrap();
    let all = items(&h.home);
    let get = |id: &str| all.iter().find(|i| i.id.starts_with(id)).unwrap();

    assert_eq!(allows(get("claude-projects"), true), Allows::Trash);
    assert_eq!(
        allows(get("npm-cache"), true),
        Allows::Delete {
            word: "npm cache".into()
        }
    );
    assert_eq!(
        allows(get("node_modules:"), true),
        Allows::Delete {
            word: "web/node_modules".into()
        }
    );
    assert_eq!(
        allows(get("core-simulator"), true),
        Allows::Command {
            command: "xcrun simctl delete unavailable".into()
        }
    );
    assert!(matches!(
        allows(get("docker-raw"), true),
        Allows::Never { .. }
    ));
    assert!(matches!(
        allows(get("app-caches"), true),
        Allows::Never { .. }
    ));
    // Off turns every action off but still shows a command, which Scry never runs.
    assert_eq!(
        allows(get("claude-projects"), false),
        Allows::Off {
            would: "trash".into()
        }
    );
    assert_eq!(
        allows(get("npm-cache"), false),
        Allows::Off {
            would: "delete".into()
        }
    );
    assert!(matches!(
        allows(get("core-simulator"), false),
        Allows::Command { .. }
    ));
    assert_eq!(plan(get("claude-projects"), false), Err(Refusal::Off));
    assert!(matches!(
        plan(get("docker-raw"), true),
        Err(Refusal::Never { .. })
    ));
}

#[test]
fn trash_moves_the_folder_records_it_and_audits() {
    let h = home();
    let dir = h.home.join(".codex/sessions");
    fs::create_dir_all(dir.join("2026")).unwrap();
    fs::write(dir.join("2026/log.jsonl"), vec![7u8; 100_000]).unwrap();
    let it = item(&h.home, "codex-sessions");
    let p = plan(&it, true).unwrap();
    assert_eq!(p.action, Action::Trash);
    assert!(p.allocated >= 100_000);

    let done = execute(&p, None, &ctx(&h, true)).unwrap();
    assert!(!dir.exists());
    let landed = done.moved_to.clone().unwrap();
    assert!(landed.exists(), "{}", landed.display());
    assert!(done.sentence().starts_with("Moved "), "{}", done.sentence());
    let recorded = h.store.trashed().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].trash_path, landed);
    assert_eq!(recorded[0].bytes, p.allocated);
    let trash = done.trash.clone().unwrap();
    assert!(trash.bytes() >= p.allocated, "{trash:?}");
    put_back(&done);
    assert!(dir.join("2026/log.jsonl").exists());
    assert_eq!(audit(&h)[0].0, "clean trash");
    assert!(audit(&h)[0].1.starts_with("Moved "));
}

#[test]
fn delete_needs_the_name_typed_and_never_follows_a_symlink_out() {
    let h = home();
    let cache = h.home.join(".npm");
    fs::create_dir_all(cache.join("_cacache")).unwrap();
    fs::write(cache.join("_cacache/blob"), vec![1u8; 50_000]).unwrap();
    let outside = h.home.join("keep-me");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("precious"), b"x").unwrap();
    std::os::unix::fs::symlink(&outside, cache.join("link")).unwrap();
    let p = plan(&item(&h.home, "npm-cache"), true).unwrap();
    assert_eq!(p.word.as_deref(), Some("npm cache"));
    let c = ctx(&h, true);

    for typed in [None, Some(""), Some("npm"), Some("NPM CACHE")] {
        assert_eq!(
            execute(&p, typed, &c),
            Err(Refusal::WrongWord {
                word: "npm cache".into()
            })
        );
        assert!(cache.exists());
    }
    let done = execute(&p, Some("npm cache"), &c).unwrap();
    assert!(!cache.exists());
    assert!(done.moved_to.is_none());
    assert!(
        outside.join("precious").exists(),
        "the link went, not its target"
    );
    assert!(
        fs::read_dir(&h.home)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().contains("scry-deleting")),
        "nothing left aside"
    );
    let rows = audit(&h);
    assert_eq!(rows.len(), 5);
    assert!(
        rows[..4]
            .iter()
            .all(|(_, r)| r.starts_with("refused: Type npm cache"))
    );
    assert!(rows[4].1.starts_with("Deleted "));
}

#[test]
fn the_recheck_refuses_a_target_that_changed_after_the_plan() {
    let h = home();
    let dir = h.home.join(".claude/projects");
    fs::create_dir_all(dir.join("p")).unwrap();
    let p = plan(&item(&h.home, "claude-projects"), true).unwrap();
    let c = ctx(&h, true);

    // Another folder with the same name.
    fs::rename(&dir, h.home.join("old-projects")).unwrap();
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        execute(&p, None, &c),
        Err(Refusal::Changed { path: dir.clone() })
    );
    assert!(dir.exists());

    // A symlink to the original.
    fs::remove_dir(&dir).unwrap();
    std::os::unix::fs::symlink(h.home.join("old-projects"), &dir).unwrap();
    assert_eq!(
        execute(&p, None, &c),
        Err(Refusal::Changed { path: dir.clone() })
    );
    assert!(h.home.join("old-projects/p").exists());

    // Gone.
    fs::remove_file(&dir).unwrap();
    assert_eq!(
        execute(&p, None, &c),
        Err(Refusal::Gone { path: dir.clone() })
    );

    // The original folder moved back, but a parent became a symlink leading elsewhere.
    fs::rename(h.home.join("old-projects"), &dir).unwrap();
    fs::rename(h.home.join(".claude"), h.home.join("real-claude")).unwrap();
    std::os::unix::fs::symlink(h.home.join("real-claude"), h.home.join(".claude")).unwrap();
    assert_eq!(execute(&p, None, &c), Err(Refusal::Changed { path: dir }));
    assert!(h.home.join("real-claude/projects/p").exists());
    assert!(audit(&h).iter().all(|(_, r)| r.starts_with("refused")));
}

#[test]
fn nothing_acts_with_destructive_false_even_on_an_old_plan() {
    let h = home();
    let dir = h.home.join(".claude/projects");
    fs::create_dir_all(&dir).unwrap();
    let p = plan(&item(&h.home, "claude-projects"), true).unwrap();
    assert_eq!(execute(&p, None, &ctx(&h, false)), Err(Refusal::Off));
    assert!(dir.exists());
    assert_eq!(
        audit(&h),
        [("clean trash".into(), format!("refused: {}", Refusal::Off))]
    );
}

#[test]
fn a_protected_folder_is_refused_even_if_a_plan_names_it() {
    let h = home();
    let docs = h.home.join("Documents/cache");
    fs::create_dir_all(&docs).unwrap();
    let p = Plan {
        item: "x".into(),
        name: "x".into(),
        action: Action::Trash,
        expect: Some(file_id(&fs::symlink_metadata(&docs).unwrap())),
        path: docs.clone(),
        allocated: 0,
        reclaimable: 0,
        repo: None,
        word: None,
    };
    let c = Context {
        label: &never_relabel,
        ..ctx(&h, true)
    };
    assert_eq!(
        execute(&p, None, &c),
        Err(Refusal::Protected { path: docs.clone() })
    );
    assert!(docs.exists());
}

/// A home with one repo under `code/` and a worktree that can go.
fn repo_home() -> (Home, PathBuf, PathBuf) {
    let h = home();
    let app = h.home.join("code/app");
    new_repo(&h.home, &app);
    let wt = add(&app, "feat", "app-feat");
    // Ignored, so the worktree stays clean.
    fs::create_dir_all(wt.join("node_modules/pkg")).unwrap();
    fs::write(wt.join("node_modules/pkg/index.js"), vec![3u8; 200_000]).unwrap();
    (h, app, wt)
}

fn worktree_item(h: &Home, wt: &Path) -> Item {
    let cache = SizeCache::new(None);
    let r = read(
        &h.home,
        &[h.home.join("code")],
        &cache,
        crate::sys::now_ms() / 1000,
    );
    r.layout
        .items
        .into_iter()
        .find(|i| i.path == wt)
        .expect("the worktree is an item")
}

#[test]
fn a_worktree_that_can_go_is_trashed_then_pruned() {
    let (h, app, wt) = repo_home();
    let it = worktree_item(&h, &wt);
    let facts = it.worktree.as_ref().unwrap();
    assert_eq!(facts.status, Status::CanGo, "{}", facts.sentence);
    let p = plan(&it, true).unwrap();
    assert_eq!(p.repo.as_deref(), Some(app.as_path()));

    let done = execute(&p, None, &ctx(&h, true)).unwrap();
    assert!(done.pruned);
    assert!(!wt.exists());
    let list = git(&app, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains("app-feat"), "{list}");
    put_back(&done);
    assert!(wt.join("node_modules/pkg/index.js").exists());
}

#[test]
fn a_worktree_relabelled_since_the_plan_is_refused() {
    let (h, _app, wt) = repo_home();
    let p = plan(&worktree_item(&h, &wt), true).unwrap();
    fs::write(wt.join("README"), "edited\n").unwrap();
    assert_eq!(
        execute(&p, None, &ctx(&h, true)),
        Err(Refusal::Relabelled {
            now: "dirty".into()
        })
    );
    assert!(wt.join("README").exists());
}

#[test]
fn a_worktree_whose_folder_is_gone_is_only_pruned() {
    let (h, app, wt) = repo_home();
    fs::remove_dir_all(&wt).unwrap();
    let it = worktree_item(&h, &wt);
    assert_eq!(it.worktree.as_ref().unwrap().status, Status::CanPrune);
    let p = plan(&it, true).unwrap();
    assert_eq!(p.action, Action::Prune);
    assert_eq!(p.expect, None);
    // A folder that reappears is not pruned away under it.
    fs::create_dir_all(&wt).unwrap();
    assert_eq!(
        execute(&p, None, &ctx(&h, true)),
        Err(Refusal::Changed { path: wt.clone() })
    );
    fs::remove_dir(&wt).unwrap();
    let done = execute(&p, None, &ctx(&h, true)).unwrap();
    assert!(done.pruned);
    assert!(!git(&app, &["worktree", "list"]).contains("app-feat"));
}
