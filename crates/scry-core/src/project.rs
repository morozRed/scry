//! Finds the project a working directory belongs to.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::model::{Project, ProjectKind};

/// A `.git` file holds one `gitdir:` line; more than this is not one.
const MAX_GIT_FILE: u64 = 4096;

/// Package manager prefixes. Homebrew keeps its own git repository at its prefix and runs
/// `brew services` from inside it, so without this stop every one of them would claim it.
const PACKAGE_PREFIXES: [&str; 2] = ["/opt/homebrew", "/usr/local/Homebrew"];

const MANIFESTS: [ProjectKind; 4] = [
    ProjectKind::Node,
    ProjectKind::Cargo,
    ProjectKind::Go,
    ProjectKind::Python,
];

/// Walks up from `cwd`. The nearest `.git` wins, so a package inside a repository belongs to
/// the repository. Without one, the nearest manifest is the root. The home directory, `/`
/// and package manager prefixes are never roots, and the walk stops at them, so a dotfiles
/// repository in `~` or Homebrew's own repository does not claim every process below it.
pub fn find_root(cwd: &Path, home: Option<&Path>) -> Option<Project> {
    let stops: Vec<&Path> = home
        .into_iter()
        .chain(PACKAGE_PREFIXES.map(Path::new))
        .collect();
    walk(cwd, &stops)
}

fn walk(cwd: &Path, stops: &[&Path]) -> Option<Project> {
    let mut nearest_manifest = None;
    for dir in cwd.ancestors() {
        if dir.parent().is_none() || stops.contains(&dir) {
            break;
        }
        let git = dir.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&git) {
            let kind = if meta.is_dir() {
                ProjectKind::Git
            } else {
                git_file_kind(&git)
            };
            return Some(project(dir, kind));
        }
        if nearest_manifest.is_none()
            && let Some(kind) = MANIFESTS.iter().find(|k| dir.join(k.marker()).is_file())
        {
            nearest_manifest = Some(project(dir, *kind));
        }
    }
    nearest_manifest
}

/// A `.git` file points at the real git directory: under `worktrees/` for a linked worktree,
/// under `modules/` for a submodule.
fn git_file_kind(git: &Path) -> ProjectKind {
    let text = read_small(git).unwrap_or_default();
    let target = text
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .unwrap_or("")
        .trim();
    if target.contains("/worktrees/") {
        ProjectKind::Worktree
    } else if target.contains("/modules/") {
        ProjectKind::Submodule
    } else {
        ProjectKind::Git
    }
}

/// The first [`MAX_GIT_FILE`] bytes of a regular file. The open neither follows a symlink nor
/// waits for a writer, so a `.git` that is a FIFO or a link to `/dev/zero` is refused at once.
fn read_small(path: &Path) -> Option<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    file.take(MAX_GIT_FILE).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn project(dir: &Path, kind: ProjectKind) -> Project {
    Project {
        root: dir.to_string_lossy().into_owned(),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let p =
                std::env::temp_dir().join(format!("scry-project-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tree(p.canonicalize().unwrap())
        }
        fn dir(&self, rel: &str) -> std::path::PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }
        fn file(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nearest_git_wins_over_a_nested_manifest() {
        let t = Tree::new("git");
        t.dir("repo/.git");
        t.file("repo/web/package.json", "{}");
        let got = find_root(&t.dir("repo/web/src"), None).unwrap();
        assert_eq!(got.root, t.0.join("repo").to_string_lossy());
        assert_eq!(got.kind, ProjectKind::Git);
    }

    #[test]
    fn a_git_file_tells_worktree_from_submodule() {
        let t = Tree::new("gitfile");
        t.file("wt/.git", "gitdir: /src/acme/.git/worktrees/wt\n");
        t.file("sub/.git", "gitdir: ../.git/modules/sub\n");
        assert_eq!(
            find_root(&t.dir("wt/a"), None).unwrap().kind,
            ProjectKind::Worktree
        );
        assert_eq!(
            find_root(&t.dir("sub"), None).unwrap().kind,
            ProjectKind::Submodule
        );
    }

    /// `find_root` on its own thread, failing after 5 s instead of hanging the run.
    fn find_root_within(dir: std::path::PathBuf) -> Option<Project> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(find_root(&dir, None)));
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("find_root returned in time")
    }

    #[test]
    fn a_git_entry_that_is_not_a_regular_file_is_never_read() {
        let t = Tree::new("special");
        let fifo = t.dir("fifo").join(".git");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink("/dev/zero", t.dir("zero").join(".git")).unwrap();
        for dir in ["fifo", "zero"] {
            let got = find_root_within(t.0.join(dir)).expect("a .git marks the root");
            assert_eq!(got.kind, ProjectKind::Git, "{dir}");
        }
        // Only the first 4 KB of a `.git` file is read.
        let pad = "#".repeat(MAX_GIT_FILE as usize);
        t.file(
            "big/.git",
            &format!("{pad}\ngitdir: /src/acme/.git/worktrees/big\n"),
        );
        assert_eq!(
            find_root_within(t.0.join("big")).unwrap().kind,
            ProjectKind::Git
        );
    }

    #[test]
    fn without_git_the_nearest_manifest_is_the_root() {
        let t = Tree::new("manifest");
        t.file("svc/pyproject.toml", "");
        t.file("svc/tool/Cargo.toml", "");
        let got = find_root(&t.dir("svc/tool/src"), None).unwrap();
        assert_eq!(got.root, t.0.join("svc/tool").to_string_lossy());
        assert_eq!(got.kind, ProjectKind::Cargo);
    }

    #[test]
    fn the_walk_stops_at_home() {
        let t = Tree::new("home");
        t.dir(".git");
        let home = t.dir("home");
        assert_eq!(find_root(&t.dir("home/notes"), Some(&home)), None);
        assert_eq!(find_root(&home, Some(&home)), None);
    }

    #[test]
    fn a_package_manager_prefix_is_not_a_project() {
        let t = Tree::new("brew");
        t.dir("opt/homebrew/.git");
        t.file(
            "opt/homebrew/Library/Taps/acme/homebrew-tools/.git/HEAD",
            "",
        );
        let prefix = t.0.join("opt/homebrew");
        let stops = [prefix.as_path()];
        assert_eq!(walk(&t.dir("opt/homebrew/var/redis"), &stops), None);
        assert_eq!(walk(&prefix, &stops), None);
        let tap = t.dir("opt/homebrew/Library/Taps/acme/homebrew-tools/Formula");
        assert_eq!(
            walk(&tap, &stops).unwrap().root,
            t.0.join("opt/homebrew/Library/Taps/acme/homebrew-tools")
                .to_string_lossy()
        );
        assert_eq!(find_root(Path::new("/opt/homebrew/var"), None), None);
        assert_eq!(find_root(Path::new("/usr/local/Homebrew"), None), None);
    }
}
