//! Moving things to the Trash through `NSFileManager`, as Finder does, so "Put Back" works,
//! and how much the Trash holds. Space comes back only when the Trash is emptied. macOS lets
//! only apps with Full Disk Access list the Trash, so without it Scry counts what it put
//! there itself and is still there.

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::protected::file_id;
use crate::size::Walker;
use crate::store::Store;

type Id = *mut c_void;
type Sel = *const c_void;
type CFTypeRef = *const c_void;
type CFIndex = isize;

const UTF8: u32 = 0x0800_0100;

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        alloc: CFTypeRef,
        buf: *const u8,
        len: CFIndex,
        is_dir: u8,
    ) -> CFTypeRef;
    fn CFURLGetFileSystemRepresentation(
        url: CFTypeRef,
        resolve: u8,
        buf: *mut u8,
        len: CFIndex,
    ) -> u8;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFRelease(cf: CFTypeRef);
}

/// Moves `path` to the Trash of its volume and returns where it landed.
pub fn move_to_trash(path: &Path) -> Result<PathBuf> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: Objective-C messages are sent with the exact signatures of the methods they
    // name. The URL is created and released here; the result URL and the error are
    // autoreleased into the pool popped at the end, after they have been read.
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let result = (|| {
            let url = CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as CFIndex,
                u8::from(path.is_dir()),
            );
            if url.is_null() {
                bail!("{} cannot be made a URL", path.display());
            }
            let send0: extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc_msgSend as *const ());
            let manager = send0(
                objc_getClass(c"NSFileManager".as_ptr()),
                sel_registerName(c"defaultManager".as_ptr()),
            );
            let trash: extern "C" fn(Id, Sel, CFTypeRef, *mut Id, *mut Id) -> i8 =
                std::mem::transmute(objc_msgSend as *const ());
            let mut landed: Id = std::ptr::null_mut();
            let mut error: Id = std::ptr::null_mut();
            let ok = trash(
                manager,
                sel_registerName(c"trashItemAtURL:resultingItemURL:error:".as_ptr()),
                url,
                &mut landed,
                &mut error,
            ) != 0;
            CFRelease(url);
            if !ok {
                let why = if error.is_null() {
                    "no reason given".to_string()
                } else {
                    let desc = send0(error, sel_registerName(c"localizedDescription".as_ptr()));
                    cf_string(desc).unwrap_or_else(|| "no reason given".into())
                };
                bail!("{why}");
            }
            let mut buf = vec![0u8; 4096];
            if landed.is_null()
                || CFURLGetFileSystemRepresentation(
                    landed,
                    1,
                    buf.as_mut_ptr(),
                    buf.len() as CFIndex,
                ) == 0
            {
                bail!("moved, but macOS did not say where");
            }
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..end])))
        })();
        objc_autoreleasePoolPop(pool);
        result
    }
}

unsafe fn cf_string(s: CFTypeRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 1024];
    // SAFETY: `s` is a live string; the buffer's size is passed with it.
    unsafe {
        (CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) != 0)
            .then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// How much the Trash holds, as far as Scry may look.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "known", rename_all = "snake_case")]
pub enum TrashSize {
    /// The whole Trash was read.
    All { bytes: u64 },
    /// The Trash could not be listed; this is what Scry moved there and is still there.
    Scry { bytes: u64, items: usize },
}

impl TrashSize {
    pub fn bytes(&self) -> u64 {
        match self {
            TrashSize::All { bytes } | TrashSize::Scry { bytes, .. } => *bytes,
        }
    }

    pub fn sentence(&self) -> String {
        let b = crate::view::worktrees::bytes(self.bytes());
        match self {
            TrashSize::All { .. } => format!("The Trash holds {b}."),
            TrashSize::Scry { items: 0, .. } => "Scry has nothing in the Trash; macOS shows the \
                                                 rest of it only to apps with Full Disk Access."
                .into(),
            TrashSize::Scry { .. } => format!(
                "The Trash holds at least {b} that Scry put there; macOS shows the rest only to \
                 apps with Full Disk Access."
            ),
        }
    }
}

/// The size of `~/.Trash` when it can be read, else what Scry's records say is still there:
/// an item counts while its path in the Trash holds the same file it moved, and once
/// however often it was put back and trashed again to the same place.
pub fn size(home: &Path, store: Option<&Store>) -> TrashSize {
    let trash = home.join(".Trash");
    if std::fs::read_dir(&trash).is_ok() {
        let [s] = Walker::new(home)
            .measure(std::slice::from_ref(&trash))
            .try_into()
            .expect("one root");
        if s.error.is_none() {
            return TrashSize::All {
                bytes: s.size.allocated,
            };
        }
    }
    let records = store.and_then(|s| s.trashed().ok()).unwrap_or_default();
    // The newest record of each object, by where it is and what it is.
    let mut latest = std::collections::HashMap::new();
    for t in &records {
        latest.insert((t.trash_path.as_path(), t.id), t.bytes);
    }
    let still: Vec<u64> = latest
        .into_iter()
        .filter(|((path, id), _)| std::fs::symlink_metadata(path).is_ok_and(|m| file_id(&m) == *id))
        .map(|(_, bytes)| bytes)
        .collect();
    TrashSize::Scry {
        bytes: still.iter().sum(),
        items: still.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_goes_to_the_trash_whole_and_can_be_put_back() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().canonicalize().unwrap().join("scry-trash-test");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("inner/f"), b"kept").unwrap();
        let before = file_id(&std::fs::symlink_metadata(&dir).unwrap());

        let landed = move_to_trash(&dir).unwrap();
        assert!(!dir.exists());
        assert_eq!(
            file_id(&std::fs::symlink_metadata(&landed).unwrap()),
            before,
            "the same folder, moved, not copied"
        );
        // Put back, so the test leaves nothing in the user's Trash.
        std::fs::rename(&landed, &dir).unwrap();
        assert_eq!(std::fs::read(dir.join("inner/f")).unwrap(), b"kept");
    }

    #[test]
    fn a_missing_path_is_an_error_not_a_panic() {
        let t = tempfile::tempdir().unwrap();
        assert!(move_to_trash(&t.path().join("gone")).is_err());
    }

    #[test]
    fn without_the_trash_listed_scry_counts_only_what_it_moved_and_is_still_there() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        // A fake home whose Trash cannot be listed.
        std::fs::create_dir_all(home.join(".Trash")).unwrap();
        std::fs::write(home.join(".Trash/x"), b"x").unwrap();
        let store = Store::open(&home.join("scry.db")).unwrap();
        let kept = home.join("kept");
        std::fs::write(&kept, b"x").unwrap();
        let id = file_id(&std::fs::metadata(&kept).unwrap());
        for (path, bytes) in [(kept.clone(), 100), (home.join("emptied"), 900)] {
            store
                .record_trashed(&crate::store::Trashed {
                    at_ms: 1,
                    item: "x".into(),
                    from: "/x".into(),
                    trash_path: path,
                    id,
                    bytes,
                })
                .unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(home.join(".Trash"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let s = size(&home, Some(&store));
        std::fs::set_permissions(home.join(".Trash"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(
            s,
            TrashSize::Scry {
                bytes: 100,
                items: 1
            }
        );
        assert!(matches!(size(&home, Some(&store)), TrashSize::All { .. }));
    }

    #[test]
    fn trashed_put_back_and_trashed_again_counts_once() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let store = Store::open(&home.join("scry.db")).unwrap();
        let landed = home.join("landed");
        std::fs::create_dir_all(&landed).unwrap();
        let id = file_id(&std::fs::metadata(&landed).unwrap());
        for at_ms in 1..=3 {
            store
                .record_trashed(&crate::store::Trashed {
                    at_ms,
                    item: "x".into(),
                    from: "/x".into(),
                    trash_path: landed.clone(),
                    id,
                    bytes: 500,
                })
                .unwrap();
        }
        // No Trash folder here, so only Scry's records count.
        assert_eq!(
            size(&home, Some(&store)),
            TrashSize::Scry {
                bytes: 500,
                items: 1
            }
        );
    }
}
