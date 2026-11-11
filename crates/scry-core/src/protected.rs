//! Desktop, Documents and Downloads in the home folder. macOS asks the user before an app
//! touches anything inside them, so Scry never looks inside one, not even to check that a
//! path exists. Folders are compared by device and inode, so a symlink, a `..` or a name
//! cased differently does not slip past.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
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
                // Nothing is there, so the rest cannot lead anywhere else.
                out = next;
                for part in todo {
                    if part == ".." {
                        out.pop();
                    } else {
                        out.push(part);
                    }
                }
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
        assert_eq!(
            guard.resolve(&home.join("code/app")),
            Resolved::Outside(home.join("code/app"))
        );
    }
}
