//! Desktop, Documents and Downloads in the home folder. macOS asks the user before an app
//! touches anything inside them, so Scry never looks inside one, not even to check that a
//! path exists. Folders are compared by device and inode, so a symlink, a `..` or a name
//! cased differently does not slip past.
//!
//! What Scry checks before it runs git on a worktree: its record holds no symlink, its
//! `commondir` leads back to its repository's own git folder, and no `include.path` or
//! `includeIf.*.path` in its `config.worktree`, nor in a file that one includes, names a file
//! inside a protected folder. A worktree that fails any of these is not read.
//!
//! Known limit: your own git config can point git at a protected folder; Scry does not follow
//! it, but git does. That covers the global config (`~/.gitconfig`, itself perhaps a symlink
//! into one) and what it includes, the repository's own `.git/config` and its includes, a
//! `core.excludesFile` set in any of them, and `objects/info/alternates`, so a repo set up
//! that way can still make macOS ask.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub const FOLDERS: [&str; 3] = ["Desktop", "Documents", "Downloads"];

/// A file's device and inode.
pub type FileId = (i32, u64);

pub fn file_id(m: &fs::Metadata) -> FileId {
    // `st_dev` is an i32 on macOS; std widens it, and this narrows it back.
    (m.dev() as i32, m.ino())
}

pub struct Protected {
    folders: Vec<(PathBuf, FileId)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The path with every symlink and `..` resolved. Nothing on the way is protected.
    Outside(PathBuf),
    /// The path leads into this protected folder.
    Inside(PathBuf),
    /// A `..` follows a component that does not exist. The kernel cannot open such a path,
    /// and where it would lead cannot be checked, so it is not read.
    Unresolvable,
}

impl Resolved {
    pub fn outside(self) -> Option<PathBuf> {
        match self {
            Resolved::Outside(p) => Some(p),
            Resolved::Inside(_) | Resolved::Unresolvable => None,
        }
    }
}

/// Reads a whole file without following a symlink in its last component, so a link planted
/// where git keeps a small file cannot lead the read anywhere else. Earlier components must
/// already be resolved.
pub fn read_no_follow(path: &Path) -> io::Result<Vec<u8>> {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut out = Vec::new();
    io::Read::read_to_end(&mut f, &mut out)?;
    Ok(out)
}

impl Protected {
    pub fn new(home: &Path) -> Self {
        let folders = FOLDERS
            .iter()
            .filter_map(|name| {
                let path = home.join(name);
                // Following a symlinked folder finds the place macOS guards. Reading a
                // folder's own attributes does not need permission; reading inside it does.
                let id = file_id(&fs::metadata(&path).ok()?);
                Some((path, id))
            })
            .collect();
        Protected { folders }
    }

    /// The protected folder with this identity.
    pub fn folder(&self, id: FileId) -> Option<&Path> {
        self.folders
            .iter()
            .find(|(_, f)| *f == id)
            .map(|(p, _)| p.as_path())
    }

    /// Resolves `path` one component at a time and stops at the first protected folder, so
    /// it never looks inside one.
    pub fn resolve(&self, path: &Path) -> Resolved {
        let path = if path.is_relative() {
            std::env::current_dir().unwrap_or_default().join(path)
        } else {
            path.to_path_buf()
        };
        let mut todo = components(&path);
        let mut out = PathBuf::from("/");
        let mut links = 0;
        while let Some(part) = todo.pop_front() {
            if part == ".." {
                out.pop();
                continue;
            }
            let next = out.join(&part);
            let Ok(meta) = fs::symlink_metadata(&next) else {
                // Nothing is there, so the rest cannot lead anywhere else, unless a `..`
                // climbs back out of it, which the kernel refuses to do.
                if todo.iter().any(|p| p == "..") {
                    return Resolved::Unresolvable;
                }
                out = next;
                out.extend(todo);
                return Resolved::Outside(out);
            };
            if meta.file_type().is_symlink() {
                links += 1;
                // Past 40 links the path is a loop that nothing can open.
                let target = fs::read_link(&next).ok().filter(|_| links <= 40);
                let Some(target) = target else {
                    return Resolved::Outside(next);
                };
                if target.is_absolute() {
                    out = PathBuf::from("/");
                }
                for part in components(&target).into_iter().rev() {
                    todo.push_front(part);
                }
                continue;
            }
            out = next;
            if let Some(folder) = self.folder(file_id(&meta)) {
                return Resolved::Inside(folder.to_path_buf());
            }
        }
        // realpath only adds the on-disk case of each name here; every part is a checked folder.
        Resolved::Outside(fs::canonicalize(&out).unwrap_or(out))
    }
}

fn components(p: &Path) -> VecDeque<OsString> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_os_string()),
            Component::ParentDir => Some("..".into()),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_at_a_protected_folder_however_the_path_reaches_it() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        fs::create_dir_all(home.join("Desktop/proj")).unwrap();
        fs::create_dir_all(home.join("code/app")).unwrap();
        std::os::unix::fs::symlink(home.join("Desktop"), home.join("code/desk")).unwrap();
        std::os::unix::fs::symlink("../Desktop/proj", home.join("code/rel")).unwrap();
        let guard = Protected::new(&home);
        let inside = Resolved::Inside(home.join("Desktop"));

        assert_eq!(guard.resolve(&home.join("Desktop/proj/x")), inside);
        assert_eq!(guard.resolve(&home.join("code/desk/proj")), inside);
        assert_eq!(guard.resolve(&home.join("code/rel/.git")), inside);
        assert_eq!(guard.resolve(&home.join("code/app/../../Desktop")), inside);
        if home.join("desktop").exists() {
            assert_eq!(
                guard.resolve(&home.join("desktop/proj")),
                inside,
                "APFS ignores case"
            );
        }
        assert_eq!(
            guard.resolve(&home.join("code/app/../app/missing/.git")),
            Resolved::Outside(home.join("code/app/missing/.git"))
        );
        // The kernel cannot climb out of a folder that does not exist, so neither does this.
        assert_eq!(
            guard.resolve(&home.join("code/missing/../../Desktop/proj")),
            Resolved::Unresolvable
        );
        assert_eq!(
            guard.resolve(&home.join("code/missing/x/../../app")),
            Resolved::Unresolvable
        );
        assert_eq!(
            guard.resolve(&home.join("code/app")),
            Resolved::Outside(home.join("code/app"))
        );
    }
}
