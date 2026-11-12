//! Every cleanup path on folders made in a temporary home. A test that moves something to
//! the Trash puts it back.

use std::cell::Cell;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::*;
use crate::disk::cache::SizeCache;
use crate::disk::{Layout, catalog, read};
use crate::worktree::tests::{add, git, new_repo, write};

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

/// The catalog items of a home with code under `roots`, measured without a cache.
fn items_in(home: &Path, roots: &[PathBuf]) -> Vec<Item> {
    let found = catalog::locate(home, roots, &[]);
    let mut layout = Layout::new(&found, None, home, 0, &Default::default());
    let cache = SizeCache::new(None);
    let look = cache.look(&layout.parts, home);
    let (sizes, _) = cache.measure(&layout.parts, &look, home, &layout.exclude);
    let sizes: Vec<_> = sizes.into_iter().map(Some).collect();
    layout.fill(&sizes, &vec![false; sizes.len()]);
    layout.items
}

fn items(home: &Path) -> Vec<Item> {
    items_in(home, &[home.join("code")])
}

fn item(home: &Path, id: &str) -> Item {
    items(home)
        .into_iter()
        .find(|i| i.id == id)
        .unwrap_or_else(|| panic!("no item {id}"))
}

fn yes() -> bool {
    true
}

fn no() -> bool {
    false
}

fn never_relabel(_: &Plan, _: &Path, _: Option<&SubtreeSize>) -> Result<Status, Refusal> {
    panic!("only worktrees are relabelled")
}

fn ctx(h: &Home) -> Context<'_> {
    Context {
        home: &h.home,
        store: Some(&h.store),
        actor: "test",
        destructive: &yes,
        label: &relabel,
        moment: None,
    }
}

/// A plan for `path` as if the list had offered it, whatever its item allows, to show that
/// `execute` refuses on its own.
fn forced(path: &Path, action: Action) -> Plan {
    Plan {
        item: "forced".into(),
        name: "forced".into(),
        action,
        expect: Some(file_id(&fs::symlink_metadata(path).unwrap())),
        parents: hold::chain(path),
        path: path.to_path_buf(),
        allocated: 0,
        reclaimable: 0,
        repo: None,
        record: None,
        record_id: None,
        word: (action == Action::Delete).then(|| "forced".into()),
    }
}

/// Puts a trashed folder back where it was, as Finder's Put Back does, so the user's Trash
/// is left as it was.
fn put_back(landed: &Path, to: &Path) {
    fs::rename(landed, to).unwrap();
}

fn audit(h: &Home) -> Vec<(String, String)> {
    h.store
        .audit_rows()
        .unwrap()
        .into_iter()
        .map(|r| (r.action, r.result))
        .collect()
}

fn results(h: &Home) -> Vec<String> {
    audit(h).into_iter().map(|(_, r)| r).collect()
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
fn trash_moves_the_folder_records_it_and_audits_intent_then_outcome() {
    let h = home();
    let dir = h.home.join(".codex/sessions");
    fs::create_dir_all(dir.join("2026")).unwrap();
    fs::write(dir.join("2026/log.jsonl"), vec![7u8; 100_000]).unwrap();
    let it = item(&h.home, "codex-sessions");
    let p = plan(&it, true).unwrap();
    assert_eq!(p.action, Action::Trash);
    assert!(p.allocated >= 100_000);
    assert_eq!(p.parents, hold::chain(&dir));

    let done = execute(&p, None, &ctx(&h)).unwrap();
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
    put_back(&landed, &dir);
    assert!(dir.join("2026/log.jsonl").exists());
    let rows = audit(&h);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows[0],
        (
            "clean trash".into(),
            "intent: moving it to the Trash".into()
        )
    );
    assert!(rows[1].1.starts_with("Moved "));
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
    symlink(&outside, cache.join("link")).unwrap();
    let p = plan(&item(&h.home, "npm-cache"), true).unwrap();
    assert_eq!(p.word.as_deref(), Some("npm cache"));
    let c = ctx(&h);

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
    let rows = results(&h);
    assert_eq!(rows.len(), 6, "{rows:?}");
    assert!(
        rows[..4]
            .iter()
            .all(|r| r.starts_with("refused: Type npm cache"))
    );
    assert_eq!(rows[4], "intent: deleting it");
    assert!(rows[5].starts_with("Deleted "));
}

#[test]
fn the_recheck_refuses_a_target_that_changed_after_the_plan() {
    let h = home();
    let dir = h.home.join(".claude/projects");
    fs::create_dir_all(dir.join("p")).unwrap();
    let p = plan(&item(&h.home, "claude-projects"), true).unwrap();
    let c = ctx(&h);

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
    symlink(h.home.join("old-projects"), &dir).unwrap();
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
    symlink(h.home.join("real-claude"), h.home.join(".claude")).unwrap();
    assert_eq!(
        execute(&p, None, &c),
        Err(Refusal::Changed { path: dir.clone() })
    );
    assert!(h.home.join("real-claude/projects/p").exists());

    // The same folder under another real parent of the same name.
    fs::remove_file(h.home.join(".claude")).unwrap();
    fs::create_dir_all(h.home.join(".claude")).unwrap();
    fs::rename(
        h.home.join("real-claude/projects"),
        h.home.join(".claude/projects"),
    )
    .unwrap();
    assert_eq!(execute(&p, None, &c), Err(Refusal::Changed { path: dir }));
    assert!(h.home.join(".claude/projects/p").exists());
    assert!(results(&h).iter().all(|r| r.starts_with("refused")));
}

#[test]
fn destructive_is_read_again_right_before_acting() {
    let h = home();
    let dir = h.home.join(".claude/projects");
    fs::create_dir_all(&dir).unwrap();
    let p = plan(&item(&h.home, "claude-projects"), true).unwrap();
    assert_eq!(
        execute(
            &p,
            None,
            &Context {
                destructive: &no,
                ..ctx(&h)
            }
        ),
        Err(Refusal::Off)
    );
    // On when the sheet opened, turned off while the folder was walked.
    let asked = Cell::new(0);
    let turned_off = || {
        asked.set(asked.get() + 1);
        asked.get() == 1
    };
    assert_eq!(
        execute(
            &p,
            None,
            &Context {
                destructive: &turned_off,
                ..ctx(&h)
            }
        ),
        Err(Refusal::Off)
    );
    assert_eq!(asked.get(), 2);
    assert!(dir.exists());
    assert_eq!(
        results(&h),
        [
            format!("refused: {}", Refusal::Off),
            format!("refused: {}", Refusal::Off)
        ]
    );
}

#[test]
fn an_audit_log_that_cannot_be_written_refuses_the_cleanup() {
    let h = home();
    let dir = h.home.join(".claude/projects");
    fs::create_dir_all(dir.join("p")).unwrap();
    let p = plan(&item(&h.home, "claude-projects"), true).unwrap();
    let none = Context {
        store: None,
        ..ctx(&h)
    };
    assert!(matches!(
        execute(&p, None, &none),
        Err(Refusal::Unaudited { .. })
    ));
    let read_only = Store::open_read_only(&h.home.join("scry.db")).unwrap();
    let c = Context {
        store: Some(&read_only),
        ..ctx(&h)
    };
    let r = execute(&p, None, &c);
    assert!(matches!(r, Err(Refusal::Unaudited { .. })), "{r:?}");
    assert!(dir.join("p").exists(), "nothing moved");
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            out.extend(files_under(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn immutable(p: &Path, on: bool) {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    let flags = if on { libc::UF_IMMUTABLE } else { 0 };
    assert_eq!(unsafe { libc::chflags(c.as_ptr(), flags) }, 0);
}

#[test]
fn a_delete_that_fails_partway_says_exactly_what_went() {
    let h = home();
    let cache = h.home.join(".npm");
    for d in ["a", "b", "c"] {
        fs::create_dir_all(cache.join(d)).unwrap();
        fs::write(cache.join(d).join("f"), vec![1u8; 9000]).unwrap();
    }
    let p = plan(&item(&h.home, "npm-cache"), true).unwrap();
    // Once the first file is gone, the rest cannot be unlinked, as with `chflags uchg`.
    let stuck = std::cell::RefCell::new(Vec::new());
    let lock = |m: Moment| {
        if m == Moment::Removed(1) {
            for f in files_under(&cache) {
                immutable(&f, true);
                stuck.borrow_mut().push(f);
            }
        }
    };
    let c = Context {
        moment: Some(&lock),
        ..ctx(&h)
    };
    let r = execute(&p, Some("npm cache"), &c);
    for f in stuck.borrow().iter() {
        immutable(f, false);
    }
    let Err(Refusal::Incomplete { did, error }) = &r else {
        panic!("{r:?}")
    };
    assert!(did.starts_with("1 file and 1 folder"), "{did}");
    assert!(error.contains("/f"), "{error}");
    assert!(r.as_ref().unwrap_err().changed());
    assert_eq!(files_under(&cache).len(), 2);
    let rows = results(&h);
    assert!(
        rows.last().unwrap().starts_with("did not finish: "),
        "{rows:?}"
    );
    assert!(!rows.iter().any(|r| r.starts_with("refused")));
}

#[test]
fn a_protected_folder_is_refused_even_if_a_plan_names_it() {
    let h = home();
    let docs = h.home.join("Documents/cache");
    fs::create_dir_all(&docs).unwrap();
    let c = Context {
        label: &never_relabel,
        ..ctx(&h)
    };
    assert_eq!(
        execute(&forced(&docs, Action::Trash), None, &c),
        Err(Refusal::Protected { path: docs.clone() })
    );
    assert!(docs.exists());
}

#[test]
fn a_cache_above_a_protected_folder_is_never_removed() {
    let h = home();
    // Documents is a symlink to a real folder inside the npm cache.
    let real = h.home.join(".npm/docs");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("thesis.txt"), b"years of work").unwrap();
    fs::write(h.home.join(".npm/blob"), vec![1u8; 4096]).unwrap();
    symlink(&real, h.home.join("Documents")).unwrap();
    let it = item(&h.home, "npm-cache");
    assert!(it.fenced);
    assert!(
        matches!(&allows(&it, true), Allows::Never { why } if why.contains("Documents")),
        "{:?}",
        allows(&it, true)
    );
    let r = execute(
        &forced(&h.home.join(".npm"), Action::Delete),
        Some("forced"),
        &ctx(&h),
    );
    assert_eq!(
        r,
        Err(Refusal::Protected {
            path: h.home.join(".npm")
        })
    );
    assert_eq!(fs::read(real.join("thesis.txt")).unwrap(), b"years of work");
}

#[test]
fn a_home_folder_named_node_modules_is_never_offered_or_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("node_modules");
    fs::create_dir_all(home.join("work")).unwrap();
    fs::write(home.join("work/notes"), b"mine").unwrap();
    let store = Store::open(&base.join("scry.db")).unwrap();
    let all = items_in(&home, std::slice::from_ref(&base));
    let it = all
        .iter()
        .find(|i| i.path == home)
        .expect("the home folder is found as a node_modules");
    assert!(it.fenced);
    assert!(matches!(allows(it, true), Allows::Never { .. }));
    let c = Context {
        home: &home,
        store: Some(&store),
        actor: "test",
        destructive: &yes,
        label: &never_relabel,
        moment: None,
    };
    assert_eq!(
        execute(&forced(&home, Action::Delete), Some("forced"), &c),
        Err(Refusal::Protected { path: home.clone() })
    );
    assert!(home.join("work/notes").exists());
}

#[test]
fn a_node_modules_holding_a_repository_is_never_removed() {
    let h = home();
    let pkg = h.home.join("code/web/node_modules/pkg");
    fs::create_dir_all(&pkg).unwrap();
    git(&pkg, &["init", "-q"]);
    write(&pkg.join("patch.js"), "unpublished\n");
    git(&pkg, &["add", "."]);
    git(&pkg, &["commit", "-qm", "local fix"]);
    let it = items(&h.home)
        .into_iter()
        .find(|i| i.id.starts_with("node_modules:"))
        .unwrap();
    assert!(
        matches!(&allows(&it, true), Allows::Never { why } if why.contains("pkg")),
        "{:?}",
        allows(&it, true)
    );
    let r = execute(&forced(&it.path, Action::Delete), Some("forced"), &ctx(&h));
    assert!(matches!(r, Err(Refusal::Holds { .. })), "{r:?}");
    assert!(pkg.join("patch.js").exists());
}

#[test]
fn a_dirty_worktree_inside_a_node_modules_is_never_removed_with_it() {
    let h = home();
    let app = h.home.join("code/app");
    new_repo(&h.home, &app);
    let modules = h.home.join("code/web/node_modules");
    fs::create_dir_all(&modules).unwrap();
    let wt = modules.join("wt");
    git(
        &app,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "inside",
            wt.to_str().unwrap(),
        ],
    );
    write(&wt.join("draft.txt"), "uncommitted\n");
    let r = read(&h.home, &[h.home.join("code")], &SizeCache::new(None), 0);
    let it = r.layout.items.iter().find(|i| i.path == modules).unwrap();
    assert!(
        matches!(&allows(it, true), Allows::Never { why } if why.contains("worktree")),
        "{:?}",
        allows(it, true)
    );
    let r = execute(&forced(&modules, Action::Delete), Some("forced"), &ctx(&h));
    assert!(matches!(r, Err(Refusal::Holds { .. })), "{r:?}");
    assert!(wt.join("draft.txt").exists());
}

#[test]
fn a_folder_swapped_between_the_check_and_the_move_is_moved_back() {
    let h = home();
    let dir = h.home.join(".codex/sessions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("planned"), b"x").unwrap();
    let p = plan(&item(&h.home, "codex-sessions"), true).unwrap();
    let swap = |m: Moment| {
        if m == Moment::Act {
            fs::rename(&dir, h.home.join("planned-moved")).unwrap();
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("other"), b"someone else's").unwrap();
        }
    };
    let c = Context {
        moment: Some(&swap),
        ..ctx(&h)
    };
    let r = execute(&p, None, &c);
    let Err(Refusal::Swapped { path, landed }) = &r else {
        panic!("{r:?}")
    };
    assert_eq!(path, &dir);
    assert!(!landed.exists());
    assert_eq!(fs::read(dir.join("other")).unwrap(), b"someone else's");
    assert!(h.home.join("planned-moved/planned").exists());
    assert!(h.store.trashed().unwrap().is_empty());
    let rows = results(&h);
    assert!(
        rows.last().unwrap().starts_with("did not finish: "),
        "{rows:?}"
    );
}

#[test]
fn a_parent_swapped_for_a_symlink_between_the_check_and_the_move_loses_nothing() {
    let h = home();
    let dir = h.home.join(".codex/sessions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("planned"), b"x").unwrap();
    let elsewhere = h.home.join("elsewhere/sessions");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("precious"), b"keep").unwrap();
    let p = plan(&item(&h.home, "codex-sessions"), true).unwrap();
    let swap = |m: Moment| {
        if m == Moment::Act {
            fs::rename(h.home.join(".codex"), h.home.join("codex-real")).unwrap();
            symlink(h.home.join("elsewhere"), h.home.join(".codex")).unwrap();
        }
    };
    let c = Context {
        moment: Some(&swap),
        ..ctx(&h)
    };
    let r = execute(&p, None, &c);
    assert!(r.is_err(), "{r:?}");
    assert_eq!(fs::read(elsewhere.join("precious")).unwrap(), b"keep");
    assert!(h.home.join("codex-real/sessions/planned").exists());
}

#[test]
fn a_folder_swapped_in_while_a_delete_runs_is_never_touched() {
    let h = home();
    let cache = h.home.join(".npm");
    for i in 0..20 {
        fs::create_dir_all(cache.join(format!("d{i}"))).unwrap();
        fs::write(cache.join(format!("d{i}/f")), b"cache").unwrap();
    }
    let p = plan(&item(&h.home, "npm-cache"), true).unwrap();
    let done = Cell::new(false);
    let swap = |m: Moment| {
        if matches!(m, Moment::Removed(1)) && !done.replace(true) {
            fs::rename(&cache, h.home.join("npm-moved")).unwrap();
            fs::create_dir_all(cache.join("d0")).unwrap();
            fs::write(cache.join("d0/precious"), b"keep").unwrap();
        }
    };
    let c = Context {
        moment: Some(&swap),
        ..ctx(&h)
    };
    let r = execute(&p, Some("npm cache"), &c);
    let Err(Refusal::Incomplete { did, error }) = &r else {
        panic!("{r:?}")
    };
    assert!(did.contains("20 files"), "{did}");
    assert!(error.contains("moved"), "{error}");
    assert_eq!(fs::read(cache.join("d0/precious")).unwrap(), b"keep");
    assert_eq!(fs::read_dir(h.home.join("npm-moved")).unwrap().count(), 0);
}

/// Attaches a small disk image at `at`, or says why not.
fn mount_image(scratch: &Path, at: &Path) -> Result<(), String> {
    let image = scratch.join("vol.dmg");
    let run = |args: &[&str]| {
        std::process::Command::new("/usr/bin/hdiutil")
            .args(args)
            .output()
            .map_err(|e| e.to_string())
            .and_then(|o| {
                o.status
                    .success()
                    .then_some(())
                    .ok_or_else(|| String::from_utf8_lossy(&o.stderr).into_owned())
            })
    };
    run(&[
        "create",
        "-quiet",
        "-size",
        "4m",
        "-fs",
        "HFS+",
        "-volname",
        "scrytest",
        image.to_str().unwrap(),
    ])?;
    run(&[
        "attach",
        "-quiet",
        "-nobrowse",
        "-mountpoint",
        at.to_str().unwrap(),
        image.to_str().unwrap(),
    ])
}

fn detach(at: &Path) {
    let _ = std::process::Command::new("/usr/bin/hdiutil")
        .args(["detach", "-quiet", "-force", at.to_str().unwrap()])
        .status();
}

#[test]
fn a_mounted_volume_inside_a_cache_is_never_entered() {
    let h = home();
    let mounted = h.home.join("code/web/node_modules/mounted-data");
    fs::create_dir_all(&mounted).unwrap();
    fs::write(h.home.join("code/web/node_modules/own"), b"x").unwrap();
    if let Err(e) = mount_image(&h.home, &mounted) {
        eprintln!("skipped: hdiutil could not attach a disk image here: {e}");
        return;
    }
    fs::write(mounted.join("data"), b"on another volume").unwrap();
    let modules = h.home.join("code/web/node_modules");
    let it = items(&h.home)
        .into_iter()
        .find(|i| i.path == modules)
        .unwrap();
    let offered = allows(&it, true);
    let r = execute(&forced(&modules, Action::Delete), Some("forced"), &ctx(&h));
    // The descriptor removal itself never crosses into it either.
    let held = hold::hold(&modules, &Fence::new(&h.home)).unwrap();
    let removal = hold::remove(held, &mut |_| {});
    let data = fs::read(mounted.join("data"));
    detach(&mounted);
    assert!(
        matches!(&offered, Allows::Never { why } if why.contains("mounted")),
        "{offered:?}"
    );
    assert!(
        matches!(&r, Err(Refusal::Holds { what, .. }) if what.contains("mounted")),
        "{r:?}"
    );
    let stopped = removal.unwrap_err();
    assert!(stopped.error.contains("volume"), "{stopped:?}");
    assert_eq!(data.unwrap(), b"on another volume");
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

fn records(app: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(app.join(".git/worktrees"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn a_worktree_that_can_go_moves_to_the_trash_and_git_keeps_its_record() {
    let (h, app, wt) = repo_home();
    // A commit only the worktree's HEAD reflog remembers.
    write(&wt.join("try.txt"), "attempt\n");
    git(&wt, &["add", "try.txt"]);
    git(&wt, &["commit", "-qm", "abandoned"]);
    let abandoned = git(&wt, &["rev-parse", "HEAD"]).trim().to_string();
    git(&wt, &["reset", "-q", "--hard", "HEAD~1"]);
    let it = worktree_item(&h, &wt);
    let facts = it.worktree.as_ref().unwrap();
    assert_eq!(facts.status, Status::CanGo, "{}", facts.sentence);
    let p = plan(&it, true).unwrap();
    assert_eq!(p.repo.as_deref(), Some(app.as_path()));
    assert_eq!(
        p.record.as_deref(),
        Some(app.join(".git/worktrees/app-feat").as_path())
    );

    let done = execute(&p, None, &ctx(&h)).unwrap();
    assert!(done.worktree);
    assert!(!wt.exists());
    assert_eq!(records(&app), ["app-feat"], "git's record stays");
    // While it is in the Trash it shows as can prune.
    let gone = worktree_item(&h, &wt);
    assert_eq!(gone.worktree.as_ref().unwrap().status, Status::CanPrune);

    put_back(done.moved_to.as_ref().unwrap(), &wt);
    assert!(git(&wt, &["status", "--porcelain"]).is_empty());
    let reflog = git(&wt, &["reflog", "--format=%H"]);
    assert!(
        reflog.contains(&abandoned),
        "the HEAD reflog survives: {reflog}"
    );
    assert!(wt.join("node_modules/pkg/index.js").exists());
}

#[test]
fn a_worktree_relabelled_since_the_plan_is_refused() {
    let (h, _app, wt) = repo_home();
    let p = plan(&worktree_item(&h, &wt), true).unwrap();
    fs::write(wt.join("README"), "edited\n").unwrap();
    assert_eq!(
        execute(&p, None, &ctx(&h)),
        Err(Refusal::Relabelled {
            now: "dirty".into()
        })
    );
    assert!(wt.join("README").exists());
}

#[test]
fn the_label_is_read_after_the_walk_so_a_change_made_during_it_is_seen() {
    let (h, _app, wt) = repo_home();
    let p = plan(&worktree_item(&h, &wt), true).unwrap();
    // An agent stages a file and then writes another version of it while the folder is
    // walked. The label is read after the walk, with git status, and sees it.
    let late = |p: &Plan, home: &Path, walk: Option<&SubtreeSize>| {
        assert!(walk.is_some(), "the walk comes first");
        write(&wt.join("README"), "version A\n");
        git(&wt, &["add", "README"]);
        write(&wt.join("README"), "version B\n");
        relabel(p, home, walk)
    };
    let c = Context {
        label: &late,
        ..ctx(&h)
    };
    assert_eq!(
        execute(&p, None, &c),
        Err(Refusal::Relabelled {
            now: "dirty".into()
        })
    );
    assert!(wt.join("README").exists());
}

/// A repo with worktrees whose folders are gone, as `setup1.sh` makes them: one detached
/// with a commit no branch holds, one whose record holds an extra file, one locked, one
/// with staged work, and one that can be pruned.
fn missing_worktrees() -> (Home, PathBuf, PathBuf) {
    let h = home();
    let app = h.home.join("code/app");
    new_repo(&h.home, &app);
    let away = h.home.join("away");
    fs::create_dir_all(&away).unwrap();
    let gone = |name: &str| fs::rename(app.parent().unwrap().join(name), away.join(name)).unwrap();

    let det = app.parent().unwrap().join("wt-det");
    git(
        &app,
        &["worktree", "add", "-q", "--detach", det.to_str().unwrap()],
    );
    write(&det.join("d"), "x\n");
    git(&det, &["add", "d"]);
    git(&det, &["commit", "-qm", "detached only"]);
    gone("wt-det");

    add(&app, "feat-extra", "wt-extra");
    fs::write(app.join(".git/worktrees/wt-extra/mynotes"), "note\n").unwrap();
    gone("wt-extra");

    let lock = add(&app, "feat-lock", "wt-lock");
    git(
        &app,
        &[
            "worktree",
            "lock",
            "--reason",
            "keep",
            lock.to_str().unwrap(),
        ],
    );
    gone("wt-lock");

    let staged = add(&app, "feat-staged", "wt-staged");
    write(&staged.join("new.txt"), "staged, never committed\n");
    git(&staged, &["add", "new.txt"]);
    gone("wt-staged");

    let prunable = add(&app, "feat-prunable", "wt-prunable");
    gone("wt-prunable");
    (h, app, prunable)
}

#[test]
fn pruning_one_missing_worktree_removes_only_its_own_record() {
    let (h, app, prunable) = missing_worktrees();
    let all = read(&h.home, &[h.home.join("code")], &SizeCache::new(None), 0);
    let status = |name: &str| {
        all.layout
            .items
            .iter()
            .find(|i| i.path.ends_with(name))
            .and_then(|i| i.worktree.as_ref())
            .map(|w| w.status)
            .unwrap()
    };
    assert_eq!(status("wt-prunable"), Status::CanPrune);
    for name in ["wt-det", "wt-extra", "wt-lock", "wt-staged"] {
        assert_eq!(status(name), Status::NeedsLook, "{name}");
    }
    let it = worktree_item(&h, &prunable);
    let p = plan(&it, true).unwrap();
    assert_eq!(p.action, Action::Prune);
    assert_eq!(p.expect, None);
    let done = execute(&p, None, &ctx(&h)).unwrap();
    assert_eq!(
        done.record.as_deref(),
        Some(app.join(".git/worktrees/wt-prunable").as_path())
    );
    assert_eq!(
        records(&app),
        ["wt-det", "wt-extra", "wt-lock", "wt-staged"]
    );
    // Its branch stays.
    assert!(git(&app, &["branch", "--list", "feat-prunable"]).contains("feat-prunable"));
    let rows = results(&h);
    assert!(
        rows[0].starts_with("intent: removing its record "),
        "{rows:?}"
    );
    assert!(rows[1].starts_with("Removed git's record"), "{rows:?}");
}

#[test]
fn a_missing_worktree_with_staged_work_or_an_unheld_head_is_never_pruned() {
    let (h, app, _) = missing_worktrees();
    // Even when a label says it can be pruned, the record itself is checked before it goes.
    let can_prune = |_: &Plan, _: &Path, _: Option<&SubtreeSize>| Ok(Status::CanPrune);
    let c = Context {
        label: &can_prune,
        ..ctx(&h)
    };
    for (name, why) in [("wt-staged", "staged"), ("wt-det", "detached")] {
        let it = worktree_item(&h, &h.home.join("code").join(name));
        let record = app.join(".git/worktrees").join(name);
        let p = Plan {
            action: Action::Prune,
            expect: None,
            repo: Some(app.clone()),
            record: Some(record.clone()),
            record_id: Some(file_id(&fs::metadata(&record).unwrap())),
            ..forced(&h.home.join("code"), Action::Prune)
        };
        let p = Plan {
            path: it.path.clone(),
            parents: hold::chain(&it.path),
            ..p
        };
        let r = execute(&p, None, &c);
        assert!(
            matches!(&r, Err(Refusal::Record { why: w }) if w.contains(why)),
            "{name}: {r:?}"
        );
    }
    assert_eq!(
        records(&app),
        ["wt-det", "wt-extra", "wt-lock", "wt-prunable", "wt-staged"]
    );
}

#[test]
fn a_prune_refuses_a_folder_that_came_back_or_a_record_that_names_another() {
    let (h, app, prunable) = missing_worktrees();
    let p = plan(&worktree_item(&h, &prunable), true).unwrap();
    fs::create_dir_all(&prunable).unwrap();
    assert_eq!(
        execute(&p, None, &ctx(&h)),
        Err(Refusal::Changed {
            path: prunable.clone()
        })
    );
    fs::remove_dir(&prunable).unwrap();
    let gitdir = app.join(".git/worktrees/wt-prunable/gitdir");
    fs::write(
        &gitdir,
        format!("{}\n", h.home.join("code/other/.git").display()),
    )
    .unwrap();
    let r = execute(&p, None, &ctx(&h));
    assert!(r.is_err(), "{r:?}");
    assert!(records(&app).contains(&"wt-prunable".to_string()));
}

fn listed(home: &Path, items: &[Item]) -> crate::view::disk::DiskList {
    crate::view::disk::DiskList::new(crate::view::disk::Inputs {
        items,
        space: None,
        forecast: None,
        trash: None,
        destructive: true,
        worktrees_pending: false,
        home,
        config: &home.join("config.toml"),
    })
}

#[test]
fn a_missing_worktree_says_its_folder_is_gone_not_measuring() {
    let (h, _app, prunable) = missing_worktrees();
    let it = worktree_item(&h, &prunable);
    let list = listed(&h.home, std::slice::from_ref(&it));
    let d = &list.items[0];
    assert!(d.gone);
    assert_eq!(d.detail.facts[0].value, "folder is gone");
}

#[test]
fn app_caches_say_which_items_inside_them_are_counted_on_their_own() {
    let h = home();
    for d in [
        "Library/Caches/Yarn/v6",
        "Library/Caches/pip/wheels",
        "Library/Caches/app",
    ] {
        fs::create_dir_all(h.home.join(d)).unwrap();
        fs::write(h.home.join(d).join("f"), vec![1u8; 2 << 20]).unwrap();
    }
    let all = items(&h.home);
    let list = listed(&h.home, &all);
    let caches = list.items.iter().find(|d| d.id == "app-caches").unwrap();
    let line = caches
        .detail
        .evidence
        .iter()
        .find(|e| e.starts_with("not counted here"))
        .unwrap_or_else(|| panic!("{:?}", caches.detail.evidence));
    assert!(
        line.contains("Yarn cache") && line.contains("pip cache"),
        "{line}"
    );
    // 2 MB of its own; the 4 MB inside Yarn's and pip's folders are theirs.
    assert!(caches.size.unwrap() < 3_000_000, "{:?}", caches.size);
}
