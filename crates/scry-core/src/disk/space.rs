//! Free space as Finder shows it. Finder's "available" counts purgeable space, which macOS
//! frees on demand (cached iCloud files, old updates), so it is larger than `statfs`'s free
//! count; the difference is listed as purgeable. Local snapshots hold blocks that nothing
//! else does, and are listed apart because only root can size them.

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

type CFTypeRef = *const c_void;
type CFIndex = isize;

const UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt64Type`.
const SINT64: CFIndex = 4;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFURLVolumeTotalCapacityKey: CFTypeRef;
    static kCFURLVolumeAvailableCapacityKey: CFTypeRef;
    static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFTypeRef;
    static kCFURLVolumeLocalizedNameKey: CFTypeRef;
    fn CFURLCreateFromFileSystemRepresentation(
        alloc: CFTypeRef,
        buf: *const u8,
        len: CFIndex,
        is_dir: u8,
    ) -> CFTypeRef;
    fn CFURLCopyResourcePropertyForKey(
        url: CFTypeRef,
        key: CFTypeRef,
        value: *mut CFTypeRef,
        error: *mut CFTypeRef,
    ) -> u8;
    fn CFNumberGetValue(n: CFTypeRef, kind: CFIndex, out: *mut c_void) -> u8;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFRelease(cf: CFTypeRef);
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    /// The volume's name in Finder, such as "Macintosh HD".
    pub volume: String,
    pub total: u64,
    /// What Finder calls available: free space plus what macOS can purge for an important
    /// write.
    pub available: u64,
    /// The part of `available` that macOS frees only on demand.
    pub purgeable: u64,
    /// Local snapshots on the volume, by name. Their sizes need root.
    pub snapshots: Vec<String>,
}

/// Finder's available space on the volume that holds `path`, without the snapshots, so the
/// daemon can record it with every sample.
pub fn important(path: &Path) -> Option<u64> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: the URL is owned here and released before returning.
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as CFIndex,
            1,
        );
        if url.is_null() {
            return None;
        }
        let important = number(url, kCFURLVolumeAvailableCapacityForImportantUsageKey);
        CFRelease(url);
        important
    }
}

/// Reads the volume that holds `path`.
pub fn read(path: &Path) -> Result<Space> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: the URL and every copied value are owned here and released before returning.
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as CFIndex,
            1,
        );
        if url.is_null() {
            bail!("{} cannot be made a URL", path.display());
        }
        let total = number(url, kCFURLVolumeTotalCapacityKey);
        let free = number(url, kCFURLVolumeAvailableCapacityKey);
        let important = number(url, kCFURLVolumeAvailableCapacityForImportantUsageKey);
        let name = string(url, kCFURLVolumeLocalizedNameKey);
        CFRelease(url);
        let (Some(total), Some(important)) = (total, important) else {
            bail!("macOS did not report the capacity of {}", path.display());
        };
        Ok(Space {
            volume: name.unwrap_or_else(|| "this volume".into()),
            total,
            available: important,
            purgeable: important.saturating_sub(free.unwrap_or(important)),
            snapshots: snapshots(path),
        })
    }
}

unsafe fn copy(url: CFTypeRef, key: CFTypeRef) -> Option<CFTypeRef> {
    let mut value: CFTypeRef = std::ptr::null();
    // SAFETY: `url` and `key` are live CF objects; the value is returned retained.
    let ok =
        unsafe { CFURLCopyResourcePropertyForKey(url, key, &mut value, std::ptr::null_mut()) != 0 };
    (ok && !value.is_null()).then_some(value)
}

unsafe fn number(url: CFTypeRef, key: CFTypeRef) -> Option<u64> {
    // SAFETY: the copied value is checked to be a number before it is read, then released.
    unsafe {
        let v = copy(url, key)?;
        let mut out: i64 = 0;
        let ok = CFGetTypeID(v) == CFNumberGetTypeID()
            && CFNumberGetValue(v, SINT64, (&raw mut out).cast()) != 0;
        CFRelease(v);
        (ok && out >= 0).then_some(out as u64)
    }
}

unsafe fn string(url: CFTypeRef, key: CFTypeRef) -> Option<String> {
    // SAFETY: the copied value is checked to be a string before it is read, then released.
    unsafe {
        let v = copy(url, key)?;
        let mut buf = [0 as c_char; 512];
        let ok = CFGetTypeID(v) == CFStringGetTypeID()
            && CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) != 0;
        CFRelease(v);
        ok.then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// Local Time Machine snapshots of the volume, from `tmutil`. Empty when there are none or
/// it cannot tell.
fn snapshots(path: &Path) -> Vec<String> {
    let Ok(out) = Command::new("/usr/bin/tmutil")
        .arg("listlocalsnapshots")
        .arg(path)
        .output()
    else {
        return Vec::new();
    };
    parse_snapshots(&String::from_utf8_lossy(&out.stdout))
}

fn parse_snapshots(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with("com.apple."))
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_volume_reports_finder_figures_in_order() {
        let s = read(Path::new("/")).unwrap();
        assert!(s.total > 0 && s.available <= s.total, "{s:?}");
        assert!(s.purgeable <= s.available);
        assert!(!s.volume.is_empty());
    }

    #[test]
    fn snapshot_names_come_from_tmutil_lines() {
        let text = "Snapshots for disk /:\ncom.apple.TimeMachine.2026-11-10-101010.local\n\
                    com.apple.os.update-ABC\n";
        assert_eq!(
            parse_snapshots(text),
            [
                "com.apple.TimeMachine.2026-11-10-101010.local",
                "com.apple.os.update-ABC"
            ]
        );
    }
}
