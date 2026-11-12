//! FSEvents history: which folders changed under some paths since an event id. macOS keeps
//! this log per volume across reboots, so a reading taken days ago can be checked without
//! walking anything. Events are hints that a folder changed, never sizes; dropped events and
//! a log that was reset make everything stale.

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

type CFTypeRef = *const c_void;
type CFIndex = isize;
type StreamRef = *mut c_void;
type Callback = extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[repr(C)]
struct StreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

const UTF8: u32 = 0x0800_0100;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_WATCH_ROOT: u32 = 0x04;

const MUST_SCAN_SUBDIRS: u32 = 0x01;
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const IDS_WRAPPED: u32 = 0x08;
const HISTORY_DONE: u32 = 0x10;
const ROOT_CHANGED: u32 = 0x20;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        alloc: CFTypeRef,
        callback: Callback,
        context: *const StreamContext,
        paths: CFTypeRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> StreamRef;
    fn FSEventStreamSetDispatchQueue(stream: StreamRef, queue: *mut c_void);
    fn FSEventStreamStart(stream: StreamRef) -> u8;
    fn FSEventStreamStop(stream: StreamRef);
    fn FSEventStreamInvalidate(stream: StreamRef);
    fn FSEventStreamRelease(stream: StreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(dev: i32) -> CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: u8,
    ) -> CFTypeRef;
    fn CFArrayCreate(
        alloc: CFTypeRef,
        values: *const CFTypeRef,
        n: CFIndex,
        callbacks: *const c_void,
    ) -> CFTypeRef;
    fn CFUUIDCreateString(alloc: CFTypeRef, uuid: CFTypeRef) -> CFTypeRef;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFRelease(cf: CFTypeRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
}

/// The newest event id on this Mac. A reading stamped with it is checked from there.
pub fn current_id() -> u64 {
    // SAFETY: takes no arguments and only reads fseventsd's counter.
    unsafe { FSEventsGetCurrentEventId() }
}

/// The identity of a volume's event log. Ids from one log mean nothing in another, and the
/// log gets a new identity when it is reset or the volume is erased.
pub fn log_uuid(dev: i32) -> Option<String> {
    // SAFETY: the UUID and its string are owned here and released below; the buffer is
    // large enough for a UUID's 36 characters and the NUL.
    unsafe {
        let uuid = FSEventsCopyUUIDForDevice(dev);
        if uuid.is_null() {
            return None;
        }
        let s = CFUUIDCreateString(std::ptr::null(), uuid);
        CFRelease(uuid);
        if s.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 64];
        let ok = CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8);
        CFRelease(s);
        (ok != 0).then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// What changed under the watched paths between `since` and `upto`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Folders that changed, with the id of their newest change.
    pub changed: Vec<(PathBuf, u64)>,
    /// Folders whose whole subtree must be read again, as FSEvents coalesced what happened
    /// below them.
    pub rescan: Vec<(PathBuf, u64)>,
    /// Events were dropped, ids wrapped, or the history did not finish in time: nothing it
    /// does not list can be trusted either.
    pub lost: bool,
    /// The id the history was read up to. A reading that no change touched is good up to it.
    pub upto: u64,
}

struct State {
    history: History,
    done: bool,
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

extern "C" fn on_events(
    _stream: StreamRef,
    info: *mut c_void,
    n: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    // SAFETY: `info` is the `Shared` that `since` keeps alive until the stream is released,
    // and FSEvents passes `n` C strings, flags and ids.
    let shared = unsafe { &*(info as *const Shared) };
    let paths = paths as *const *const c_char;
    let mut s = shared.state.lock().unwrap();
    for i in 0..n {
        let (path, flag, id) =
            unsafe { (CStr::from_ptr(*paths.add(i)), *flags.add(i), *ids.add(i)) };
        if flag & HISTORY_DONE != 0 {
            s.done = true;
            continue;
        }
        let upto = s.history.upto;
        if id > upto {
            // Live events after the history; the next reading sees them.
            continue;
        }
        if flag & (USER_DROPPED | KERNEL_DROPPED | IDS_WRAPPED) != 0 {
            s.history.lost = true;
        }
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()));
        if flag & (MUST_SCAN_SUBDIRS | ROOT_CHANGED) != 0 {
            s.history.rescan.push((path, id));
        } else {
            s.history.changed.push((path, id));
        }
    }
    if s.done {
        shared.ready.notify_all();
    }
}

/// Reads the history of changes under `paths` from just after `since` up to now. Waits at
/// most `timeout` for the log; a history that does not finish counts as lost.
pub fn since(paths: &[PathBuf], since: u64, timeout: Duration) -> History {
    let upto = current_id();
    let lost = || History {
        lost: true,
        upto,
        ..History::default()
    };
    if paths.is_empty() {
        return History {
            upto,
            ..History::default()
        };
    }
    if since == upto {
        return History {
            upto,
            ..History::default()
        };
    }
    // The counter went backwards, as on a volume restored from a backup: the ids since
    // `since` say nothing about what changed.
    if since > upto {
        return lost();
    }
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            history: History {
                upto,
                ..History::default()
            },
            done: false,
        }),
        ready: Condvar::new(),
    });
    // SAFETY: every CF object created here is released here; the stream holds the context's
    // `info` pointer only until it is released below, and `shared` outlives that.
    unsafe {
        let strings: Vec<CFTypeRef> = paths.iter().map(|p| cf_string(p)).collect();
        if strings.iter().any(|s| s.is_null()) {
            strings
                .iter()
                .filter(|s| !s.is_null())
                .for_each(|&s| CFRelease(s));
            return lost();
        }
        let array = CFArrayCreate(
            std::ptr::null(),
            strings.as_ptr(),
            strings.len() as CFIndex,
            &raw const kCFTypeArrayCallBacks,
        );
        strings.iter().for_each(|&s| CFRelease(s));
        if array.is_null() {
            return lost();
        }
        let context = StreamContext {
            version: 0,
            info: Arc::as_ptr(&shared) as *mut c_void,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        let stream = FSEventStreamCreate(
            std::ptr::null(),
            on_events,
            &context,
            array,
            since,
            0.0,
            CREATE_NO_DEFER | CREATE_WATCH_ROOT,
        );
        CFRelease(array);
        if stream.is_null() {
            return lost();
        }
        let queue = dispatch_queue_create(c"scry.fsevents".as_ptr(), std::ptr::null());
        FSEventStreamSetDispatchQueue(stream, queue);
        let started = FSEventStreamStart(stream) != 0;
        let finished = started && {
            let s = shared.state.lock().unwrap();
            let (s, _) = shared
                .ready
                .wait_timeout_while(s, timeout, |s| !s.done)
                .unwrap();
            s.done
        };
        if started {
            FSEventStreamStop(stream);
        }
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
        dispatch_release(queue);
        let mut s = shared.state.lock().unwrap();
        let mut history = std::mem::take(&mut s.history);
        history.lost |= !finished;
        history
    }
}

unsafe fn cf_string(p: &Path) -> CFTypeRef {
    let b = p.as_os_str().as_bytes();
    // SAFETY: `b` is a live byte slice of the given length.
    unsafe { CFStringCreateWithBytes(std::ptr::null(), b.as_ptr(), b.len() as CFIndex, UTF8, 0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_history_names_a_folder_written_after_the_id() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("quiet")).unwrap();
        let before = current_id();
        std::fs::write(root.join("a/b/file"), b"x").unwrap();
        // fseventsd stamps events as it takes them in; give it a moment to see the write.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let h = loop {
            let h = since(std::slice::from_ref(&root), before, Duration::from_secs(5));
            if h.changed.iter().any(|(p, _)| p.ends_with("a/b"))
                || std::time::Instant::now() > deadline
            {
                break h;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        assert!(!h.lost, "{h:?}");
        assert!(
            h.changed.iter().any(|(p, _)| p == &root.join("a/b")),
            "{h:?}"
        );
        assert!(
            !h.changed
                .iter()
                .any(|(p, _)| p.starts_with(root.join("quiet")))
        );
        assert!(h.changed.iter().all(|&(_, id)| id > before && id <= h.upto));
    }

    #[test]
    fn nothing_since_now_is_an_empty_history() {
        let t = tempfile::tempdir().unwrap();
        let h = since(
            &[t.path().to_path_buf()],
            current_id(),
            Duration::from_secs(5),
        );
        assert!(
            !h.lost && h.changed.is_empty() && h.rescan.is_empty(),
            "{h:?}"
        );
    }

    #[test]
    fn a_counter_that_went_backwards_is_lost_history() {
        let t = tempfile::tempdir().unwrap();
        let h = since(
            &[t.path().to_path_buf()],
            current_id() + 1_000_000_000,
            Duration::from_secs(5),
        );
        assert!(h.lost, "{h:?}");
    }

    #[test]
    fn the_data_volume_has_a_log_identity() {
        let dev = std::fs::metadata(std::env::temp_dir()).unwrap();
        let uuid = log_uuid(std::os::unix::fs::MetadataExt::dev(&dev) as i32).unwrap();
        assert_eq!(uuid.len(), 36, "{uuid}");
    }
}
