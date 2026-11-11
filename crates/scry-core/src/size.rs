//! Sizes subtrees in one `getattrlistbulk` walk: the bytes they hold on disk and at least the
//! bytes a delete gives back at once. The Worktrees view sizes each worktree with it; the Disk
//! view sizes catalog entries with it.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Once};

use libc::c_void;
use serde::Serialize;

use crate::protected::{FileId, Protected, Resolved, file_id};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Size {
    /// Bytes on disk (`ATTR_FILE_ALLOCSIZE`), each hard-linked file counted once.
    pub allocated: u64,
    /// A lower bound on the bytes freed at once if the subtree were deleted: the sum of each
    /// file's private bytes (`ATTR_CMNEXT_PRIVATESIZE`). Blocks shared with a clone or a
    /// snapshot do not count, nor does a hard-linked file with a link outside the subtree.
    /// Two clones of one file inside the subtree each report their shared blocks as not
    /// private, though deleting both frees them, and the kernel offers no count of that.
    pub reclaimable: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SubtreeSize {
    pub size: Size,
    pub files: u64,
    pub dirs: u64,
    /// Entries that could not be read and are missing from `size`.
    pub skipped: u64,
    /// Why the subtree was not walked at all.
    pub error: Option<String>,
}

pub struct Walker {
    exclude: HashSet<FileId>,
    protected: Protected,
    threads: usize,
}

impl Walker {
    /// A walker that refuses the home folder's protected folders.
    pub fn new(home: &Path) -> Self {
        // Past about half the cores, walkers only contend inside APFS: on a 12-core Mac, 6
        // matched 12 in wall time for two thirds of the CPU.
        let threads = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(2));
        Walker {
            exclude: HashSet::new(),
            protected: Protected::new(home),
            threads,
        }
    }

    /// Folders not to enter when met inside a subtree, such as a worktree nested in another.
    pub fn exclude(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        for p in paths {
            if let Resolved::Outside(p) = self.protected.resolve(&p)
                && let Ok(m) = std::fs::metadata(&p)
            {
                self.exclude.insert(file_id(&m));
            }
        }
        self
    }

    pub fn threads(mut self, n: usize) -> Self {
        self.threads = n.max(1);
        self
    }

    /// Sizes each subtree, in order. A hard-linked file inside two subtrees is allocated to
    /// the first one walked, and is reclaimable only from a subtree that holds all its links.
    pub fn measure(&self, roots: &[PathBuf]) -> Vec<SubtreeSize> {
        raise_fd_limit();
        let mut out = vec![SubtreeSize::default(); roots.len()];
        let mut jobs = Vec::new();
        let mut devs = vec![0; roots.len()];
        for (i, root) in roots.iter().enumerate() {
            let root = match self.protected.resolve(root) {
                Resolved::Inside(folder) => {
                    out[i].error = Some(format!("{} is a protected folder", folder.display()));
                    continue;
                }
                Resolved::Outside(root) => root,
            };
            match open_root(&root) {
                Ok((fd, id)) => {
                    devs[i] = id.0;
                    out[i].dirs = 1;
                    jobs.push(Job {
                        dir: Open::Root(fd),
                        tree: i,
                    });
                }
                Err(e) => out[i].error = Some(e.to_string()),
            }
        }
        // The queue pops from the end; reversed, a single thread walks roots in order.
        jobs.reverse();

        let queue = Queue {
            state: Mutex::new(QueueState { jobs, busy: 0 }),
            ready: Condvar::new(),
        };
        let links: Mutex<HashMap<FileId, Link>> = Mutex::new(HashMap::new());
        let ctx = Ctx {
            walker: self,
            devs: &devs,
            queue: &queue,
            links: &links,
            trees: roots.len(),
        };
        let partials: Vec<Vec<Tally>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..self.threads).map(|_| s.spawn(|| ctx.work())).collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("size worker panicked"))
                .collect()
        });
        for part in partials {
            for (o, t) in out.iter_mut().zip(part) {
                o.size.allocated += t.allocated;
                o.size.reclaimable += t.reclaimable;
                o.files += t.files;
                o.dirs += t.dirs;
                o.skipped += t.skipped;
            }
        }
        for link in links.into_inner().unwrap().into_values() {
            if link.seen >= link.nlink && !link.shared {
                out[link.tree].size.reclaimable += link.private;
            }
        }
        out
    }
}

/// A queued folder keeps its parent open until it is opened itself, so a deep tree walked by
/// several threads can hold more descriptors than macOS's default soft limit of 256.
fn raise_fd_limit() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut r = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 {
            return;
        }
        // OPEN_MAX, the most setrlimit accepts on macOS.
        let want = r.rlim_max.min(10_240);
        if r.rlim_cur < want {
            r.rlim_cur = want;
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &r) };
        }
    });
}

fn fstat_id(fd: &OwnedFd) -> Option<FileId> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == 0).then_some((st.st_dev, st.st_ino))
}

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

fn open_root(path: &Path) -> std::io::Result<(OwnedFd, FileId)> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let fd = unsafe { libc::open(c.as_ptr(), DIR_FLAGS) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let id = fstat_id(&fd).ok_or_else(std::io::Error::last_os_error)?;
    Ok((fd, id))
}

/// Opens the folder `name` inside `parent`, never through a symlink, and only if it is still
/// the folder that was listed: same device, same inode.
fn open_child(parent: &OwnedFd, name: &CStr, listed: FileId) -> Option<OwnedFd> {
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), DIR_FLAGS) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    (fstat_id(&fd)? == listed).then_some(fd)
}

struct Job {
    dir: Open,
    tree: usize,
}

enum Open {
    Root(OwnedFd),
    /// A folder found inside `parent`, opened relative to it when its turn comes.
    Child {
        parent: Arc<OwnedFd>,
        name: CString,
        id: FileId,
    },
}

struct QueueState {
    jobs: Vec<Job>,
    busy: usize,
}

struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl Queue {
    fn next(&self) -> Option<Job> {
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some(job) = s.jobs.pop() {
                s.busy += 1;
                return Some(job);
            }
            if s.busy == 0 {
                return None;
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    fn finish(&self, found: Vec<Job>) {
        let mut s = self.state.lock().unwrap();
        s.busy -= 1;
        let wake = !found.is_empty() || s.busy == 0;
        s.jobs.extend(found);
        if wake {
            self.ready.notify_all();
        }
    }
}

/// A file with more than one hard link, seen at least once.
struct Link {
    nlink: u32,
    seen: u32,
    private: u64,
    tree: usize,
    /// Links were met in more than one subtree, so deleting any one of them frees nothing.
    shared: bool,
}

#[derive(Clone, Default)]
struct Tally {
    allocated: u64,
    reclaimable: u64,
    files: u64,
    dirs: u64,
    skipped: u64,
}

struct Ctx<'a> {
    walker: &'a Walker,
    /// The device of each subtree's root. Nothing on another device is counted.
    devs: &'a [i32],
    queue: &'a Queue,
    links: &'a Mutex<HashMap<FileId, Link>>,
    trees: usize,
}

impl Ctx<'_> {
    fn work(&self) -> Vec<Tally> {
        let mut tally = vec![Tally::default(); self.trees];
        let mut buf = vec![0u8; 128 * 1024];
        while let Some(job) = self.queue.next() {
            let tree = job.tree;
            let found = self.read_dir(job, &mut buf, &mut tally[tree]);
            self.queue.finish(found);
        }
        tally
    }

    fn read_dir(&self, job: Job, buf: &mut [u8], t: &mut Tally) -> Vec<Job> {
        let mut found = Vec::new();
        let fd = match job.dir {
            Open::Root(fd) => fd,
            Open::Child { parent, name, id } => match open_child(&parent, &name, id) {
                Some(fd) => fd,
                None => {
                    t.skipped += 1;
                    return found;
                }
            },
        };
        let fd = Arc::new(fd);
        let mut list = ATTR_LIST;
        loop {
            let n = unsafe {
                libc::getattrlistbulk(
                    fd.as_raw_fd(),
                    (&raw mut list).cast::<c_void>(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    u64::from(libc::FSOPT_ATTR_CMN_EXTENDED),
                )
            };
            if n <= 0 {
                if n < 0 {
                    t.skipped += 1;
                }
                break;
            }
            let mut at = 0;
            for _ in 0..n {
                let Some(entry) = Entry::parse(&buf[at..]) else {
                    t.skipped += 1;
                    break;
                };
                at += entry.len;
                self.count(job.tree, &fd, entry, t, &mut found);
            }
        }
        found
    }

    fn count(
        &self,
        tree: usize,
        dir: &Arc<OwnedFd>,
        e: Entry,
        t: &mut Tally,
        found: &mut Vec<Job>,
    ) {
        if e.error != 0 {
            t.skipped += 1;
            return;
        }
        if e.dev != self.devs[tree] {
            return;
        }
        let id = (e.dev, e.ino);
        if e.objtype == VDIR {
            if e.mount_point
                || self.walker.exclude.contains(&id)
                || self.walker.protected.folder(id).is_some()
            {
                return;
            }
            let Ok(name) = CString::new(e.name) else {
                t.skipped += 1;
                return;
            };
            t.dirs += 1;
            found.push(Job {
                dir: Open::Child {
                    parent: Arc::clone(dir),
                    name,
                    id,
                },
                tree,
            });
            return;
        }
        t.files += 1;
        let private = e.private.unwrap_or(e.alloc);
        if e.nlink <= 1 {
            t.allocated += e.alloc;
            t.reclaimable += private;
            return;
        }
        let mut links = self.links.lock().unwrap();
        match links.get_mut(&id) {
            Some(link) => {
                link.seen += 1;
                link.shared |= link.tree != tree;
            }
            None => {
                t.allocated += e.alloc;
                links.insert(
                    id,
                    Link {
                        nlink: e.nlink,
                        seen: 1,
                        private,
                        tree,
                        shared: false,
                    },
                );
            }
        }
    }
}

// <sys/attr.h> and <sys/vnode.h> values that libc does not export.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const VDIR: u32 = 2;

const ATTR_LIST: libc::attrlist = libc::attrlist {
    bitmapcount: libc::ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: libc::ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_ERROR
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_FILEID,
    volattr: 0,
    dirattr: libc::ATTR_DIR_MOUNTSTATUS,
    fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
    // With FSOPT_ATTR_CMN_EXTENDED this group holds the extended common attributes.
    forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
};

/// One directory entry from a `getattrlistbulk` buffer. Attributes are packed in bit order,
/// each on a 4-byte boundary, and only those named in the returned set are present.
struct Entry<'a> {
    len: usize,
    error: u32,
    name: &'a [u8],
    dev: i32,
    objtype: u32,
    ino: u64,
    mount_point: bool,
    nlink: u32,
    alloc: u64,
    private: Option<u64>,
}

impl<'a> Entry<'a> {
    fn parse(buf: &'a [u8]) -> Option<Self> {
        let mut r = Reader { buf, at: 0 };
        let len = r.u32()? as usize;
        if len < 24 || len > buf.len() {
            return None;
        }
        let rec = &buf[..len];
        let mut r = Reader { buf: rec, at: 4 };
        let common = r.u32()?;
        let _vol = r.u32()?;
        let dir = r.u32()?;
        let file = r.u32()?;
        let ext = r.u32()?;
        let mut e = Entry {
            len,
            error: 0,
            name: &[],
            dev: 0,
            objtype: 0,
            ino: 0,
            mount_point: false,
            nlink: 1,
            alloc: 0,
            private: None,
        };
        if common & ATTR_CMN_ERROR != 0 {
            e.error = r.u32()?;
        }
        if common & libc::ATTR_CMN_NAME != 0 {
            let base = r.at;
            let off = r.u32()? as i32;
            let n = r.u32()? as usize;
            let start = usize::try_from(base as i64 + i64::from(off)).ok()?;
            let raw = rec.get(start..start.checked_add(n)?)?;
            e.name = raw.split(|&b| b == 0).next().unwrap_or(raw);
        }
        if common & libc::ATTR_CMN_DEVID != 0 {
            e.dev = r.u32()? as i32;
        }
        if common & libc::ATTR_CMN_OBJTYPE != 0 {
            e.objtype = r.u32()?;
        }
        if common & libc::ATTR_CMN_FILEID != 0 {
            e.ino = r.u64()?;
        }
        if dir & libc::ATTR_DIR_MOUNTSTATUS != 0 {
            e.mount_point = r.u32()? & libc::DIR_MNTSTATUS_MNTPOINT != 0;
        }
        if file & libc::ATTR_FILE_LINKCOUNT != 0 {
            e.nlink = r.u32()?;
        }
        if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
            e.alloc = r.u64()?;
        }
        if ext & libc::ATTR_CMNEXT_PRIVATESIZE != 0 {
            e.private = Some(r.u64()?);
        }
        if e.name.is_empty() && e.error == 0 {
            return None;
        }
        Some(e)
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let b = self.buf.get(self.at..self.at + N)?.try_into().ok()?;
        self.at += N;
        Some(b)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take::<4>().map(u32::from_ne_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take::<8>().map(u64::from_ne_bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn write_mib(path: &Path, seed: u8) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = fs::File::create(path).unwrap();
        // Varied bytes, so no filesystem can store the file sparse or compressed.
        let data: Vec<u8> = (0..MIB as usize)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect();
        f.write_all(&data).unwrap();
        f.sync_all().unwrap();
    }

    fn clone_file(from: &Path, to: &Path) {
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        let c = |p: &Path| CString::new(p.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::clonefile(c(from).as_ptr(), c(to).as_ptr(), 0) };
        assert_eq!(rc, 0, "clonefile: {}", std::io::Error::last_os_error());
    }

    fn walker(home: &Path) -> Walker {
        Walker::new(home).threads(2)
    }

    fn near(actual: u64, expected: u64) -> bool {
        actual >= expected && actual < expected + 64 * 1024
    }

    #[test]
    fn counts_a_hard_linked_file_once_and_frees_it_when_all_links_are_inside() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        write_mib(&root.join("a"), 1);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::hard_link(root.join("a"), root.join("sub/b")).unwrap();
        let [r] = walker(t.path()).measure(&[root]).try_into().unwrap();
        assert_eq!(r.files, 2);
        assert_eq!(r.dirs, 2);
        assert!(near(r.size.allocated, MIB), "{r:?}");
        assert_eq!(r.size.reclaimable, r.size.allocated, "{r:?}");
        assert_eq!(r.skipped, 0);
    }

    #[test]
    fn pnpm_style_clones_and_hard_links_are_allocated_but_not_reclaimable() {
        let t = tempfile::tempdir().unwrap();
        let store = t.path().join("store");
        let wt = t.path().join("wt");
        write_mib(&store.join("cloned-pkg/index.js"), 2);
        write_mib(&store.join("linked-pkg/index.js"), 3);
        write_mib(&wt.join("src/own.rs"), 4);
        clone_file(
            &store.join("cloned-pkg/index.js"),
            &wt.join("node_modules/cloned-pkg/index.js"),
        );
        fs::create_dir_all(wt.join("node_modules/linked-pkg")).unwrap();
        fs::hard_link(
            store.join("linked-pkg/index.js"),
            wt.join("node_modules/linked-pkg/index.js"),
        )
        .unwrap();

        let [w] = walker(t.path())
            .measure(std::slice::from_ref(&wt))
            .try_into()
            .unwrap();
        assert_eq!(w.files, 3);
        assert!(near(w.size.allocated, 3 * MIB), "{w:?}");
        assert!(
            near(w.size.reclaimable, MIB),
            "only own.rs is private: {w:?}"
        );

        // Walked together, the hard link is allocated to the store, which is walked first,
        // and neither subtree alone frees it.
        let [s, w] = walker(t.path())
            .threads(1)
            .measure(&[store, wt])
            .try_into()
            .unwrap();
        assert!(near(s.size.allocated, 2 * MIB), "{s:?}");
        assert!(near(s.size.reclaimable, 0), "{s:?}");
        assert!(near(w.size.allocated, 2 * MIB), "{w:?}");
        assert!(near(w.size.reclaimable, MIB), "{w:?}");
    }

    #[test]
    fn skips_unreadable_folders_and_does_not_follow_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        write_mib(&t.path().join("outside/big"), 5);
        write_mib(&root.join("locked/secret"), 6);
        std::os::unix::fs::symlink(t.path().join("outside"), root.join("link")).unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

        let [r] = walker(t.path())
            .measure(std::slice::from_ref(&root))
            .try_into()
            .unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r.skipped, 1, "{r:?}");
        assert_eq!(r.files, 1, "the symlink itself: {r:?}");
        assert!(r.size.allocated < 64 * 1024, "{r:?}");
    }

    #[test]
    fn never_enters_protected_or_excluded_folders() {
        let home = tempfile::tempdir().unwrap();
        write_mib(&home.path().join("Documents/private"), 7);
        write_mib(&home.path().join("code/repo/file"), 8);
        write_mib(&home.path().join("code/repo/nested-wt/file"), 9);

        let w = walker(home.path()).exclude([home.path().join("code/repo/nested-wt")]);
        let [h, d, n] = w
            .measure(&[
                home.path().to_path_buf(),
                home.path().join("Documents"),
                home.path().join("code/repo/nested-wt"),
            ])
            .try_into()
            .unwrap();
        assert_eq!(h.files, 1, "only code/repo/file: {h:?}");
        assert!(near(h.size.allocated, MIB));
        assert!(
            d.error
                .as_deref()
                .unwrap()
                .contains("is a protected folder"),
            "{d:?}"
        );
        assert_eq!(d.files, 0);
        assert_eq!(n.files, 1, "an excluded folder can still be a root: {n:?}");
    }

    #[test]
    fn opens_a_queued_folder_through_its_parent_and_only_if_unchanged() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(t.path().join("outside/b")).unwrap();
        let (parent, _) = open_root(&root.join("a")).unwrap();
        let b = file_id(&fs::metadata(root.join("a/b")).unwrap());

        // After `b` was listed, `a` turns into a symlink to a lookalike. A path now leads
        // there, but the open goes through the descriptor of the real `a`.
        fs::rename(root.join("a"), root.join("a.real")).unwrap();
        std::os::unix::fs::symlink(t.path().join("outside"), root.join("a")).unwrap();
        assert_ne!(file_id(&fs::metadata(root.join("a/b")).unwrap()), b);
        let fd = open_child(&parent, c"b", b).expect("the folder that was listed");
        assert_eq!(fstat_id(&fd), Some(b));

        // `b` itself swapped for a symlink, then for another folder: neither is opened.
        fs::rename(root.join("a.real/b"), root.join("a.real/b.old")).unwrap();
        std::os::unix::fs::symlink(t.path().join("outside/b"), root.join("a.real/b")).unwrap();
        assert!(open_child(&parent, c"b", b).is_none());
        fs::remove_file(root.join("a.real/b")).unwrap();
        fs::create_dir(root.join("a.real/b")).unwrap();
        assert!(open_child(&parent, c"b", b).is_none());
    }

    #[test]
    fn reports_a_missing_root() {
        let t = tempfile::tempdir().unwrap();
        let [r] = walker(t.path())
            .measure(&[t.path().join("gone")])
            .try_into()
            .unwrap();
        assert!(r.error.is_some());
        assert_eq!(r.size, Size::default());
    }
}
