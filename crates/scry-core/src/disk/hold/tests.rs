//! Holding and removing folders in a temporary home.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::*;

fn home() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().canonicalize().unwrap();
    (t, home)
}

fn tree(root: &Path) {
    for d in ["a/b/c", "d"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    for (f, n) in [
        ("a/x", 10_000),
        ("a/b/y", 20_000),
        ("a/b/c/z", 5),
        ("d/w", 1),
    ] {
        fs::write(root.join(f), vec![7u8; n]).unwrap();
    }
}

#[test]
fn the_fence_keeps_home_its_parents_and_every_parent_of_a_protected_folder() {
    let (_t, home) = home();
    // Documents really lives inside a cache.
    fs::create_dir_all(home.join(".npm/docs")).unwrap();
    symlink(home.join(".npm/docs"), home.join("Documents")).unwrap();
    fs::create_dir_all(home.join("Desktop")).unwrap();
    fs::create_dir_all(home.join(".cargo/registry")).unwrap();
    let fence = Fence::new(&home);
    let id = |p: &Path| file_id(&fs::symlink_metadata(p).unwrap());

    assert!(!fence.allows(id(&home)), "the home folder");
    assert!(
        !fence.allows(id(home.parent().unwrap())),
        "a folder above it"
    );
    assert!(!fence.allows(id(Path::new("/"))));
    assert!(!fence.allows(id(&home.join(".npm"))), "above Documents");
    assert!(
        !fence.allows(id(&home.join(".npm/docs"))),
        "Documents itself"
    );
    assert!(!fence.allows(id(&home.join("Desktop"))));
    assert!(fence.allows(id(&home.join(".cargo/registry"))));

    assert_eq!(
        hold(&home.join(".npm"), &fence).err(),
        Some(HoldError::Fenced)
    );
    assert_eq!(hold(&home, &fence).err(), Some(HoldError::Fenced));
    assert!(hold(&home.join(".cargo/registry"), &fence).is_ok());
}

#[test]
fn a_symlink_anywhere_on_the_way_or_a_protected_folder_fails_the_hold() {
    let (_t, home) = home();
    fs::create_dir_all(home.join("Documents/cache")).unwrap();
    fs::create_dir_all(home.join("real/cache")).unwrap();
    symlink(home.join("real"), home.join("link")).unwrap();
    symlink(home.join("Documents"), home.join("docs")).unwrap();
    symlink(home.join("real/cache"), home.join("cache-link")).unwrap();
    let fence = Fence::new(&home);

    let held = hold(&home.join("real/cache"), &fence).unwrap();
    assert_eq!(
        held.id,
        file_id(&fs::metadata(home.join("real/cache")).unwrap())
    );
    assert_eq!(held.chain, chain(&home.join("real/cache")));
    assert_eq!(
        *held.chain.last().unwrap(),
        file_id(&fs::metadata(home.join("real")).unwrap())
    );

    assert_eq!(
        hold(&home.join("link/cache"), &fence).err(),
        Some(HoldError::Changed)
    );
    assert_eq!(
        hold(&home.join("cache-link"), &fence).err(),
        Some(HoldError::Changed)
    );
    assert_eq!(
        hold(&home.join("docs/cache"), &fence).err(),
        Some(HoldError::Changed)
    );
    assert_eq!(
        hold(&home.join("Documents/cache"), &fence).err(),
        Some(HoldError::Fenced)
    );
    assert_eq!(
        hold(&home.join("real/gone"), &fence).err(),
        Some(HoldError::Gone)
    );
    assert_eq!(
        hold(&home.join("real/../real/cache"), &fence).err(),
        Some(HoldError::Changed)
    );
}

#[test]
fn remove_deletes_the_held_folder_and_counts_what_went() {
    let (_t, home) = home();
    let root = home.join("cache");
    tree(&root);
    fs::create_dir_all(home.join("outside")).unwrap();
    fs::write(home.join("outside/keep"), b"x").unwrap();
    symlink(home.join("outside"), root.join("a/link")).unwrap();
    let held = hold(&root, &Fence::new(&home)).unwrap();
    let removed = remove(held, &mut |_| {}).unwrap();
    assert!(!root.exists());
    assert!(
        home.join("outside/keep").exists(),
        "the link went, not its target"
    );
    // 4 files and the symlink; a, a/b, a/b/c, d and the folder itself.
    assert_eq!((removed.files, removed.folders), (5, 5));
    assert!(removed.bytes >= 30_000, "{removed:?}");
}

#[test]
fn a_folder_swapped_in_during_a_remove_is_never_touched() {
    let (_t, home) = home();
    let root = home.join("cache");
    tree(&root);
    let held = hold(&root, &Fence::new(&home)).unwrap();
    let mut swapped = false;
    // After the first entry goes, the folder is moved away and a lookalike with something
    // precious takes its name.
    let result = remove(held, &mut |_| {
        if !swapped {
            swapped = true;
            fs::rename(&root, home.join("moved")).unwrap();
            fs::create_dir_all(root.join("a/b")).unwrap();
            fs::write(root.join("a/b/precious"), b"keep").unwrap();
        }
    });
    let stopped = result.unwrap_err();
    assert!(stopped.error.contains("moved"), "{stopped:?}");
    assert_eq!(fs::read(root.join("a/b/precious")).unwrap(), b"keep");
    // The held folder was emptied wherever it went, and nothing else.
    assert_eq!(fs::read_dir(home.join("moved")).unwrap().count(), 0);
    assert_eq!(stopped.removed.files, 4);
}

#[test]
fn a_subfolder_swapped_for_a_symlink_mid_remove_goes_as_a_link() {
    let (_t, home) = home();
    let root = home.join("cache");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("z")).unwrap();
    fs::write(root.join("a/f"), b"x").unwrap();
    fs::write(root.join("z/f"), b"x").unwrap();
    fs::create_dir_all(home.join("precious")).unwrap();
    fs::write(home.join("precious/f"), b"keep").unwrap();
    let held = hold(&root, &Fence::new(&home)).unwrap();
    let mut once = false;
    let result = remove(held, &mut |_| {
        if !once {
            once = true;
            // Whichever of a and z is still there becomes a symlink to the precious folder.
            for d in ["a", "z"] {
                let p = root.join(d);
                if p.exists() {
                    fs::rename(&p, home.join(format!("{d}-old"))).unwrap();
                    symlink(home.join("precious"), &p).unwrap();
                }
            }
        }
    });
    // Either it stopped because the folder changed, or it removed the link as a link.
    let _ = result;
    assert_eq!(fs::read(home.join("precious/f")).unwrap(), b"keep");
}
