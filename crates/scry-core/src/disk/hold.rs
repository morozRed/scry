//! Holding a folder by its descriptor, so a cleanup acts on exactly the folder it checked.
//!
//! - [`Fence`] names, by device and inode, the folders no cleanup may act on: the home folder
//!   and every folder above it, Desktop, Documents and Downloads wherever they really are,
//!   and every folder above one of those. Names are never compared.
//! - [`hold`] opens a path from `/` one component at a time with `O_NOFOLLOW`, checking each
//!   folder's identity before it is opened, so a symlink anywhere on the way fails it and a
//!   protected folder is never opened.
//! - [`remove`] deletes the held folder through its descriptor with `openat` and `unlinkat`,
//!   never following a symlink and never entering a folder on another device.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::protected::{FOLDERS, FileId, Protected, file_id};

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
/// Deeper than this is a loop or a trap, not a cache.
const MAX_DEPTH: usize = 512;

/// The folders no cleanup may act on.
pub struct Fence {
    protected: Protected,
    above: HashSet<FileId>,
}

impl Fence {
    pub fn new(home: &Path) -> Fence {
        let mut above = HashSet::new();
        let mut climb = |p: &Path, with_self: bool| {
            // Where the folder really is, so a protected folder that is a symlink to a folder
            // inside a cache makes that cache one of its parents.
            let Ok(real) = fs::canonicalize(p) else {
                return;
            };
            for a in real.ancestors().skip(usize::from(!with_self)) {
                if let Ok(m) = fs::metadata(a) {
                    above.insert(file_id(&m));
                }
            }
        };
        climb(home, true);
        for name in FOLDERS {
            climb(&home.join(name), false);
        }
        Fence {
            protected: Protected::new(home),
            above,
        }
    }

    fn protected(&self, id: FileId) -> bool {
        self.protected.folder(id).is_some()
    }

    /// Whether a cleanup may act on the folder with this identity.
    pub fn allows(&self, id: FileId) -> bool {
        !self.protected(id) && !self.above.contains(&id)
    }
}

/// A folder opened through the folders above it, with its parent held too.
pub struct Held {
    pub parent: OwnedFd,
    pub dir: OwnedFd,
    pub name: CString,
    pub id: FileId,
    /// The identity of each folder from `/` down to the parent.
    pub chain: Vec<FileId>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HoldError {
    Gone,
    /// A symlink, a file or another folder than the one planned stands on the way.
    Changed,
    /// It is, holds or passes through a folder the fence keeps.
    Fenced,
    Io(String),
}

/// Opens `path`, an absolute path with no `..`, one component at a time.
pub fn hold(path: &Path, fence: &Fence) -> Result<Held, HoldError> {
    let mut names = Vec::new();
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(n) => {
                names.push(CString::new(n.as_bytes()).map_err(|_| HoldError::Changed)?)
            }
            _ => return Err(HoldError::Changed),
        }
    }
    if !path.is_absolute() {
        return Err(HoldError::Changed);
    }
    let Some((name, dirs)) = names.split_last() else {
        return Err(HoldError::Fenced);
    };
    let mut cur = open_root().map_err(io_err)?;
    let mut chain = vec![fstat(&cur).map_err(io_err)?];
    for d in dirs {
        let id = folder_at(&cur, d)?;
        if fence.protected(id) {
            return Err(HoldError::Fenced);
        }
        cur = open_at(&cur, d, id)?;
        chain.push(id);
    }
    let id = folder_at(&cur, name)?;
    if !fence.allows(id) {
        return Err(HoldError::Fenced);
    }
    let dir = open_at(&cur, name, id)?;
    Ok(Held {
        parent: cur,
        dir,
        name: name.clone(),
        id,
        chain,
    })
}

/// The identity of each folder above `path`, from `/` down, as [`Held::chain`] lists them.
pub fn chain(path: &Path) -> Vec<FileId> {
    let mut out: Vec<FileId> = path
        .ancestors()
        .skip(1)
        .map_while(|a| fs::symlink_metadata(a).ok().map(|m| file_id(&m)))
        .collect();
    out.reverse();
    out
}

fn io_err(e: io::Error) -> HoldError {
    HoldError::Io(e.to_string())
}

fn open_root() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::open(c"/".as_ptr(), DIR_FLAGS) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: &OwnedFd) -> io::Result<FileId> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((st.st_dev, st.st_ino))
}

fn lstat_at(dir: &OwnedFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

/// The identity of the folder `name` in `dir`, read without opening it.
fn folder_at(dir: &OwnedFd, name: &CStr) -> Result<FileId, HoldError> {
    match lstat_at(dir, name) {
        Ok(st) if is_dir(&st) => Ok((st.st_dev, st.st_ino)),
        Ok(_) => Err(HoldError::Changed),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(HoldError::Gone),
        Err(e) => Err(io_err(e)),
    }
}

/// Opens the folder `name` in `dir` only if it is still the folder with identity `id`.
fn open_at(dir: &OwnedFd, name: &CStr, id: FileId) -> Result<OwnedFd, HoldError> {
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), DIR_FLAGS) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::ENOENT) => HoldError::Gone,
            Some(libc::ELOOP | libc::ENOTDIR) => HoldError::Changed,
            _ => io_err(e),
        });
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    match fstat(&fd) {
        Ok(got) if got == id => Ok(fd),
        Ok(_) => Err(HoldError::Changed),
        Err(e) => Err(io_err(e)),
    }
}

/// Reads the file `name` in the held folder `dir`, never through a symlink.
pub fn read_at(dir: &OwnedFd, name: &CStr) -> io::Result<Vec<u8>> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut f = fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let mut out = Vec::new();
    io::Read::read_to_end(&mut f, &mut out)?;
    Ok(out)
}

/// What a removal took away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub files: u64,
    pub folders: u64,
    /// Blocks of the removed files that had no other link.
    pub bytes: u64,
}

impl Removed {
    /// "1,204 files and 87 folders (12 MB)".
    pub fn sentence(&self) -> String {
        let n = |k: u64, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
        format!(
            "{} and {} ({})",
            n(self.files, "file", "files"),
            n(self.folders, "folder", "folders"),
            crate::view::worktrees::bytes(self.bytes)
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Stopped {
    /// What was removed before it stopped.
    pub removed: Removed,
    pub error: String,
    /// Where, relative to the held folder.
    pub at: PathBuf,
}

/// Deletes the held folder and everything in it. `progress` runs after each entry goes,
/// with the count so far.
pub fn remove(held: Held, progress: &mut dyn FnMut(&Removed)) -> Result<Removed, Stopped> {
    let dev = held.id.0;
    let mut removed = Removed::default();
    empty(&held.dir, dev, Path::new(""), 0, &mut removed, progress)?;
    let stop = |removed: Removed, error: String| Stopped {
        removed,
        error,
        at: PathBuf::new(),
    };
    // The folder itself, by name in its parent: only if that name still leads to it.
    match lstat_at(&held.parent, &held.name) {
        Ok(st) if (st.st_dev, st.st_ino) == held.id => {}
        Ok(_) => {
            return Err(stop(
                removed,
                "the folder was moved while it was emptied; it is left, empty, where it went"
                    .into(),
            ));
        }
        Err(e) => return Err(stop(removed, e.to_string())),
    }
    if unsafe {
        libc::unlinkat(
            held.parent.as_raw_fd(),
            held.name.as_ptr(),
            libc::AT_REMOVEDIR,
        )
    } != 0
    {
        return Err(stop(removed, io::Error::last_os_error().to_string()));
    }
    removed.folders += 1;
    Ok(removed)
}

/// The names in a folder, read through a copy of its descriptor.
fn names(dir: &OwnedFd) -> io::Result<Vec<CString>> {
    let copy = unsafe { libc::dup(dir.as_raw_fd()) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(copy) };
        return Err(e);
    }
    // The copy shares its offset with the held descriptor, which may have been read before.
    unsafe { libc::rewinddir(stream) };
    let mut out = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name != c"." && name != c".." {
            out.push(name.to_owned());
        }
    }
    unsafe { libc::closedir(stream) };
    Ok(out)
}

fn empty(
    dir: &OwnedFd,
    dev: i32,
    rel: &Path,
    depth: usize,
    removed: &mut Removed,
    progress: &mut dyn FnMut(&Removed),
) -> Result<(), Stopped> {
    let stop = |removed: &Removed, at: PathBuf, error: String| Stopped {
        removed: *removed,
        error,
        at,
    };
    if depth > MAX_DEPTH {
        return Err(stop(removed, rel.into(), "nested too deep".into()));
    }
    let entries = names(dir).map_err(|e| stop(removed, rel.into(), e.to_string()))?;
    for name in entries {
        let at = rel.join(std::ffi::OsStr::from_bytes(name.as_bytes()));
        let st = match lstat_at(dir, &name) {
            Ok(st) => st,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(stop(removed, at, e.to_string())),
        };
        if is_dir(&st) {
            if st.st_dev != dev {
                return Err(stop(
                    removed,
                    at,
                    "a volume is mounted there, and Scry never deletes across volumes".into(),
                ));
            }
            let child = open_at(dir, &name, (st.st_dev, st.st_ino)).map_err(|e| {
                let why = match e {
                    HoldError::Io(e) => e,
                    _ => "it changed while it was deleted".into(),
                };
                stop(removed, at.clone(), why)
            })?;
            empty(&child, dev, &at, depth + 1, removed, progress)?;
            drop(child);
            if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                return Err(stop(removed, at, io::Error::last_os_error().to_string()));
            }
            removed.folders += 1;
        } else {
            if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(stop(removed, at, io::Error::last_os_error().to_string()));
            }
            removed.files += 1;
            if st.st_nlink <= 1 {
                removed.bytes += st.st_blocks as u64 * 512;
            }
        }
        progress(removed);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
