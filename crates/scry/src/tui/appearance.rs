//! Whether macOS is in dark mode, read from the global preferences through CoreFoundation,
//! the same value `defaults read -g AppleInterfaceStyle` prints, without starting a process.

use std::ffi::{CStr, c_char, c_void};

type CFTypeRef = *const c_void;
type CFIndex = isize;
type CFStringEncoding = u32;

const UTF8: CFStringEncoding = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFPreferencesAnyApplication: CFTypeRef;
    static kCFPreferencesCurrentUser: CFTypeRef;
    static kCFPreferencesAnyHost: CFTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: CFStringEncoding,
        external: u8,
    ) -> CFTypeRef;
    fn CFPreferencesCopyValue(
        key: CFTypeRef,
        app: CFTypeRef,
        user: CFTypeRef,
        host: CFTypeRef,
    ) -> CFTypeRef;
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
    Some(global("AppleInterfaceStyle").as_deref() == Some("Dark"))
}

/// A string from the global preferences of the current user on any host, the domain
/// `defaults read -g` reads. `None` when it is absent or not a string. Apple's header says
/// the App functions must never be given kCFPreferencesAnyApplication, so this names the
/// domain in full.
fn global(key: &str) -> Option<String> {
    // SAFETY: `key` is a live byte slice of the given length; a null allocator means the
    // default one. The returned string is owned here and released below.
    let key = unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            key.as_ptr(),
            key.len() as CFIndex,
            UTF8,
            0,
        )
    };
    if key.is_null() {
        return None;
    }
    // SAFETY: `key` is a valid CFString and the other three are constants of
    // CoreFoundation. The copied value, if any, is owned here and released below.
    let value = unsafe {
        CFPreferencesCopyValue(
            key,
            kCFPreferencesAnyApplication,
            kCFPreferencesCurrentUser,
            kCFPreferencesAnyHost,
        )
    };
    // SAFETY: `key` was created above and is not used again.
    unsafe { CFRelease(key) };
    if value.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 256];
    // SAFETY: `value` is a valid CF object. CFStringGetCString is called only on a CFString
    // and writes at most `buf.len()` bytes, NUL-terminated when it returns true.
    let text = unsafe {
        (CFGetTypeID(value) == CFStringGetTypeID()
            && CFStringGetCString(value, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) != 0)
            .then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    };
    // SAFETY: `value` was copied above and is not used again.
    unsafe { CFRelease(value) };
    text
}

#[cfg(test)]
mod tests {
    /// What `defaults read -g` prints for `key`, or `None` when it has no such value.
    fn defaults(key: &str) -> Option<String> {
        let out = std::process::Command::new("defaults")
            .args(["read", "-g", key])
            .output()
            .expect("defaults runs on macOS");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    #[test]
    fn matches_what_defaults_prints() {
        let style = defaults("AppleInterfaceStyle");
        assert_eq!(super::dark(), Some(style.as_deref() == Some("Dark")));
        // In light mode the style is absent, so also compare a key that is usually set.
        for key in ["AppleInterfaceStyle", "AppleLocale"] {
            assert_eq!(super::global(key), defaults(key), "{key}");
        }
    }
}
