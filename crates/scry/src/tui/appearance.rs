//! Whether macOS is in dark mode, read from the global preferences through CoreFoundation,
//! the same value `defaults read -g AppleInterfaceStyle` prints, without starting a process.

use std::ffi::{CStr, c_char, c_void};

type CFTypeRef = *const c_void;
type CFIndex = isize;
type CFStringEncoding = u32;

const UTF8: CFStringEncoding = 0x0800_0100;
const KEY: &[u8] = b"AppleInterfaceStyle";

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFPreferencesAnyApplication: CFTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: CFStringEncoding,
        external: u8,
    ) -> CFTypeRef;
    fn CFPreferencesCopyAppValue(key: CFTypeRef, app: CFTypeRef) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetCString(
        s: CFTypeRef,
        buf: *mut c_char,
        size: CFIndex,
        encoding: CFStringEncoding,
    ) -> u8;
    fn CFRelease(cf: CFTypeRef);
}

/// `Some(true)` in dark mode. The key is absent in light mode, so a missing value is light.
pub fn dark() -> Option<bool> {
    // SAFETY: KEY is a live byte slice of the given length; a null allocator means the
    // default one. The returned string is owned here and released below.
    let key = unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            KEY.as_ptr(),
            KEY.len() as CFIndex,
            UTF8,
            0,
        )
    };
    if key.is_null() {
        return None;
    }
    // SAFETY: `key` is a valid CFString and kCFPreferencesAnyApplication a constant of
    // CoreFoundation. The copied value, if any, is owned here and released below.
    let value = unsafe { CFPreferencesCopyAppValue(key, kCFPreferencesAnyApplication) };
    // SAFETY: `key` was created above and is not used again.
    unsafe { CFRelease(key) };
    if value.is_null() {
        return Some(false);
    }
    let mut buf = [0 as c_char; 16];
    // SAFETY: `value` is a valid CF object. CFStringGetCString is called only on a CFString
    // and writes at most `buf.len()` bytes, NUL-terminated when it returns true.
    let dark = unsafe {
        CFGetTypeID(value) == CFStringGetTypeID()
            && CFStringGetCString(value, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) != 0
            && CStr::from_ptr(buf.as_ptr()).to_bytes() == b"Dark"
    };
    // SAFETY: `value` was copied above and is not used again.
    unsafe { CFRelease(value) };
    Some(dark)
}

#[cfg(test)]
mod tests {
    #[test]
    fn matches_what_defaults_prints() {
        let out = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
            .expect("defaults runs on macOS");
        let dark = String::from_utf8_lossy(&out.stdout).trim() == "Dark";
        assert_eq!(super::dark(), Some(dark));
    }
}
