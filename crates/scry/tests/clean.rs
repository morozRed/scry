//! `scry clean` as a user runs it, on repositories and caches made under a throwaway `HOME`
//! and `SCRY_HOME`. Each case is one way a cleanup used to destroy data outside the item it
//! showed. A folder moved to the Trash is put back.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    scry_home: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let home = base.join("home");
        Env::at(tmp, base, home, "[\"~/code\"]")
    }

    fn at(tmp: tempfile::TempDir, base: PathBuf, home: PathBuf, roots: &str) -> Env {
        let scry_home = base.join("scry");
        fs::create_dir_all(home.join("code")).unwrap();
        fs::create_dir_all(&scry_home).unwrap();
        fs::write(
            scry_home.join("config.toml"),
            format!("code_roots = {roots}\ndestructive = true\n"),
        )
        .unwrap();
        Env {
            _tmp: tmp,
            base,
            home,
            scry_home,
        }
    }

    fn code(&self) -> PathBuf {
        self.home.join("code")
    }

    fn scry(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_scry"))
            .args(args)
            .env("HOME", &self.home)
            .env("SCRY_HOME", &self.scry_home)
            .output()
            .unwrap()
    }

    /// `scry clean <id> --yes --json`, with the name typed for a delete.
    fn clean(&self, id: &str, confirm: Option<&str>) -> (bool, serde_json::Value) {
        let mut args = vec!["clean", id, "--yes", "--json"];
        if let Some(c) = confirm {
            args.extend(["--confirm", c]);
        }
        let out = self.scry(&args);
        let json = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.success(), json)
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.as_encoded_bytes().starts_with(b"GIT_") {
                cmd.env_remove(key);
            }
        }
        let out = cmd
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo at `code/repo` with one commit and `main`, as `setup1.sh` starts it.
    fn repo(&self) -> PathBuf {
        let repo = self.code().join("repo");
        fs::create_dir_all(&repo).unwrap();
        self.git(&repo, &["init", "-q", "-b", "main"]);
        self.git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        fs::write(repo.join("f"), "hi\n").unwrap();
        self.git(&repo, &["add", "f"]);
        self.git(&repo, &["commit", "-qm", "f"]);
        repo
    }

    fn add(&self, repo: &Path, name: &str, branch: &str) -> PathBuf {
        let wt = self.code().join(name);
        self.git(
            repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", branch],
        );
        wt
    }

    /// Moves a worktree's folder out of the code roots, as if it were deleted by hand.
    fn lose(&self, wt: &Path) {
        let away = self.base.join("away");
        fs::create_dir_all(&away).unwrap();
        fs::rename(wt, away.join(wt.file_name().unwrap())).unwrap();
    }
}

fn records(repo: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(repo.join(".git/worktrees"))
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
fn pruning_one_missing_worktree_keeps_every_other_record() {
    let e = Env::new();
    let repo = e.repo();
    // Detached, with a commit no branch holds.
    let det = e.code().join("wt-det");
    e.git(
        &repo,
        &["worktree", "add", "-q", "--detach", det.to_str().unwrap()],
    );
    fs::write(det.join("d"), "x\n").unwrap();
    e.git(&det, &["add", "d"]);
    e.git(&det, &["commit", "-qm", "detached only"]);
    let only = e.git(&det, &["rev-parse", "HEAD"]).trim().to_string();
    e.lose(&det);
    // A record holding a file of its own.
    let extra = e.add(&repo, "wt-extra", "feat-extra");
    fs::write(repo.join(".git/worktrees/wt-extra/mynotes"), "note\n").unwrap();
    e.lose(&extra);
    let prunable = e.add(&repo, "wt-prunable", "feat-prunable");
    e.lose(&prunable);

    let (ok, out) = e.clean(&format!("worktree:{}", prunable.display()), None);
    assert!(ok, "{out}");
    assert_eq!(records(&repo), ["wt-det", "wt-extra"], "{out}");
    assert!(
        fs::read_to_string(repo.join(".git/worktrees/wt-det/HEAD"))
            .unwrap()
            .contains(&only)
    );
    assert!(repo.join(".git/worktrees/wt-extra/mynotes").exists());
}

#[test]
fn trashing_a_worktree_keeps_its_record_so_put_back_gives_a_working_checkout() {
    let e = Env::new();
    let repo = e.repo();
    let wt = e.add(&repo, "wt-go", "feat-go");
    let (ok, out) = e.clean(&format!("worktree:{}", wt.display()), None);
    assert!(ok, "{out}");
    let landed = PathBuf::from(out["done"]["moved_to"].as_str().unwrap());
    assert!(!wt.exists());
    let kept = records(&repo);
    // Put Back, as Finder does it.
    fs::rename(&landed, &wt).unwrap();
    assert_eq!(kept, ["wt-go"], "{out}");
    assert!(e.git(&wt, &["status", "--porcelain"]).is_empty());
    assert_eq!(e.git(&wt, &["branch", "--show-current"]).trim(), "feat-go");
}

#[test]
fn a_missing_worktree_with_staged_work_is_never_pruned() {
    let e = Env::new();
    let repo = e.repo();
    let wt = e.add(&repo, "wt-staged", "feat-staged");
    fs::write(wt.join("new.txt"), "staged, never committed\n").unwrap();
    e.git(&wt, &["add", "new.txt"]);
    let blob = e.git(&wt, &["rev-parse", ":new.txt"]).trim().to_string();
    e.lose(&wt);

    let (ok, out) = e.clean(&format!("worktree:{}", wt.display()), None);
    assert!(!ok, "{out}");
    assert_eq!(records(&repo), ["wt-staged"]);
    let staged = e.git(
        &repo,
        &[
            "--git-dir",
            repo.join(".git/worktrees/wt-staged").to_str().unwrap(),
            "ls-files",
            "-s",
        ],
    );
    assert!(staged.contains(&blob), "{staged}");
}

#[test]
fn a_cache_that_documents_really_lives_in_is_never_deleted() {
    let e = Env::new();
    let docs = e.home.join(".npm/docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("thesis.txt"), "years of work\n").unwrap();
    fs::write(e.home.join(".npm/blob"), vec![1u8; 4096]).unwrap();
    symlink(&docs, e.home.join("Documents")).unwrap();

    let (ok, out) = e.clean("npm-cache", Some("npm cache"));
    assert!(!ok, "{out}");
    assert_eq!(
        fs::read_to_string(docs.join("thesis.txt")).unwrap(),
        "years of work\n"
    );
}

#[test]
fn a_node_modules_holding_a_repository_is_never_deleted() {
    let e = Env::new();
    let pkg = e.code().join("web/node_modules/pkg");
    fs::create_dir_all(&pkg).unwrap();
    e.git(&pkg, &["init", "-q"]);
    fs::write(pkg.join("patch.js"), "unpublished\n").unwrap();
    e.git(&pkg, &["add", "."]);
    e.git(&pkg, &["commit", "-qm", "local fix"]);
    let modules = e.code().join("web/node_modules");

    let (ok, out) = e.clean(
        &format!("node_modules:{}", modules.display()),
        Some("web/node_modules"),
    );
    assert!(!ok, "{out}");
    assert!(pkg.join("patch.js").exists());
}

#[test]
fn a_dirty_worktree_inside_a_node_modules_is_never_deleted_with_it() {
    let e = Env::new();
    let repo = e.repo();
    let modules = e.code().join("web/node_modules");
    fs::create_dir_all(&modules).unwrap();
    let wt = modules.join("wt");
    e.git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "inside",
            wt.to_str().unwrap(),
        ],
    );
    fs::write(wt.join("draft.txt"), "uncommitted\n").unwrap();

    let (ok, out) = e.clean(
        &format!("node_modules:{}", modules.display()),
        Some("web/node_modules"),
    );
    assert!(!ok, "{out}");
    assert_eq!(
        fs::read_to_string(wt.join("draft.txt")).unwrap(),
        "uncommitted\n"
    );
}

#[test]
fn a_home_folder_named_node_modules_is_never_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("node_modules");
    let roots = format!("[\"{}\"]", base.display());
    let e = Env::at(tmp, base.clone(), home.clone(), &roots);
    fs::write(home.join("notes.txt"), "mine\n").unwrap();
    let word = format!(
        "{}/node_modules",
        base.file_name().unwrap().to_string_lossy()
    );

    let (ok, out) = e.clean(&format!("node_modules:{}", home.display()), Some(&word));
    assert!(!ok, "{out}");
    assert_eq!(
        fs::read_to_string(home.join("notes.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn a_cleanup_the_audit_log_cannot_record_is_refused() {
    let e = Env::new();
    fs::create_dir_all(e.home.join(".npm/_cacache")).unwrap();
    fs::write(e.home.join(".npm/_cacache/blob"), vec![1u8; 4096]).unwrap();
    // The store exists, then cannot be written: a full disk or a locked file reads the same.
    assert!(e.scry(&["disk", "--json"]).status.success());
    let state = e.scry_home.join("state");
    for f in fs::read_dir(&state).unwrap().flatten() {
        fs::set_permissions(f.path(), fs::Permissions::from_mode(0o444)).unwrap();
    }
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();

    let (ok, out) = e.clean("npm-cache", Some("npm cache"));
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!ok, "{out}");
    assert!(e.home.join(".npm/_cacache/blob").exists());
}
