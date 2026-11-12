//! The viewer's home directory and account name, which outputs hide.
//!
//! macOS reaches the home directory by several names: `/Users/ann`, the firmlinked
//! `/System/Volumes/Data/Users/ann`, and `/USERS/ANN` on a case-insensitive volume. A path is
//! compared with home after `.` and `..` are resolved by name and the data volume's mount
//! point is dropped, and without case when the home volume ignores case.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::label::OMITTED;

/// Where the data volume is mounted. Firmlinks such as `/Users` lead into it from `/`.
const DATA_VOLUME: [&str; 3] = ["System", "Volumes", "Data"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    /// The components after `/`.
    parts: Vec<String>,
    /// Names on the home volume match without case, as on APFS by default.
    ignore_case: bool,
}

impl Home {
    /// `None` for a relative path or `/`, which would match too much. The volume is asked
    /// whether it ignores case; when it cannot say, names match without case.
    pub fn new(path: &Path) -> Option<Home> {
        Home::with_case(path, !case_sensitive(path))
    }

    fn with_case(path: &Path, ignore_case: bool) -> Option<Home> {
        let parts = components(&path.to_string_lossy())?;
        (!parts.is_empty()).then_some(Home { parts, ignore_case })
    }

    /// The home directory from `HOME` or the password database.
    pub fn from_env() -> Option<Home> {
        Home::new(&std::env::home_dir()?)
    }

    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("/{}", self.parts.join("/")))
    }

    /// The last component of the home directory, which is the account name on a Mac.
    pub fn account(&self) -> &str {
        self.parts.last().expect("home is never /")
    }

    /// Whether `path` names the home directory.
    pub fn is_home(&self, path: &Path) -> bool {
        self.below(&path.to_string_lossy())
            .is_some_and(|rest| rest.is_empty())
    }

    /// Whether `name` is the account name, in any case.
    pub fn is_account(&self, name: &str) -> bool {
        lower(name).eq(lower(self.account()))
    }

    /// `path` with the home directory written as `~`, whichever name it is reached by.
    /// Other paths are returned unchanged.
    fn tilde(&self, path: &str) -> String {
        match self.below(path) {
            Some(rest) if rest.is_empty() => "~".into(),
            Some(rest) => format!("~/{}", rest.join("/")),
            None => path.to_string(),
        }
    }

    /// `path` as outputs show it: home written as `~`, whichever name reaches it, and the
    /// account name replaced by `…` wherever else it is, in any case, as in
    /// `/private/tmp/ann-build`. A short account name that is a common word takes other
    /// text with it; for an output meant to be shared that is the safe side.
    pub fn redact(&self, path: &str) -> String {
        let path = self.tilde(path);
        let needle: Vec<char> = lower(self.account()).collect();
        let chars: Vec<char> = path.chars().collect();
        let mut out = String::with_capacity(path.len());
        let mut i = 0;
        while i < chars.len() {
            match match_at(&chars[i..], &needle) {
                Some(n) => {
                    out.push_str(OMITTED);
                    i += n;
                }
                None => {
                    out.push(chars[i]);
                    i += 1;
                }
            }
        }
        out
    }

    /// The components of `path` below home, when it is home or inside it.
    fn below(&self, path: &str) -> Option<Vec<String>> {
        let mut parts = components(path)?;
        let data = DATA_VOLUME.len();
        if parts.len() >= data
            && parts[..data]
                .iter()
                .zip(DATA_VOLUME)
                .all(|(a, b)| self.same(a, b))
        {
            parts.drain(..data);
        }
        let n = self.parts.len();
        let under = parts.len() >= n
            && parts[..n]
                .iter()
                .zip(&self.parts)
                .all(|(a, b)| self.same(a, b));
        under.then(|| parts.split_off(n))
    }

    fn same(&self, a: &str, b: &str) -> bool {
        if self.ignore_case {
            lower(a).eq(lower(b))
        } else {
            a == b
        }
    }
}

/// The components of an absolute path after `/`, with `.` and `..` resolved by name.
/// `None` for a relative path.
fn components(path: &str) -> Option<Vec<String>> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Some(parts)
}

fn lower(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars().flat_map(char::to_lowercase)
}

/// How many characters of `hay` match `needle`, which is already lower case, from its
/// start, compared without case.
fn match_at(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let mut k = 0;
    for (n, c) in hay.iter().enumerate() {
        for l in c.to_lowercase() {
            if needle.get(k) != Some(&l) {
                return None;
            }
            k += 1;
        }
        if k == needle.len() {
            return Some(n + 1);
        }
    }
    None
}

fn case_sensitive(path: &Path) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann() -> Home {
        Home::with_case(Path::new("/Users/ann/"), true).unwrap()
    }

    #[test]
    fn home_is_found_by_every_name_macos_gives_it() {
        let h = ann();
        assert_eq!(h.account(), "ann");
        for p in [
            "/Users/ann",
            "/Users/ann/",
            "/System/Volumes/Data/Users/ann",
            "/system/volumes/data/USERS/Ann",
            "/USERS/ANN",
            "/Users/ann/acme/..",
            "/./Users/ann/.",
        ] {
            assert!(h.is_home(Path::new(p)), "{p}");
        }
        for p in [
            "/Users/anna",
            "/Users",
            "/Volumes/Data/Users/ann",
            "ann",
            "/",
        ] {
            assert!(!h.is_home(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn a_case_sensitive_volume_tells_case_apart() {
        let h = Home::with_case(Path::new("/Users/ann"), false).unwrap();
        assert!(h.is_home(Path::new("/System/Volumes/Data/Users/ann")));
        assert!(!h.is_home(Path::new("/USERS/ANN")));
        assert!(
            h.is_account("ANN"),
            "the account name is matched in any case"
        );
    }

    #[test]
    fn tilde_rewrites_home_by_any_name_and_leaves_other_paths() {
        let h = ann();
        assert_eq!(h.tilde("/Users/ann"), "~");
        assert_eq!(h.tilde("/System/Volumes/Data/Users/ann/acme"), "~/acme");
        assert_eq!(h.tilde("/USERS/ANN/acme/./web"), "~/acme/web");
        assert_eq!(h.tilde("/Users/anna/x"), "/Users/anna/x");
        assert_eq!(h.tilde("/opt/x"), "/opt/x");
    }

    #[test]
    fn redact_removes_the_account_name_from_any_path() {
        let h = ann();
        assert_eq!(h.redact("/Users/ann/bin/x"), "~/bin/x");
        assert_eq!(h.redact("/USERS/ANN/notes-ann"), "~/notes-…");
        assert_eq!(
            h.redact("/private/tmp/build-501/-Users-ANN-acme"),
            "/private/tmp/build-501/-Users-…-acme"
        );
        assert_eq!(
            h.redact("/Volumes/Backup/USERS/Ann/x"),
            "/Volumes/Backup/USERS/…/x"
        );
        assert_eq!(h.redact("/Users/anna/x"), "/Users/…a/x");
        assert_eq!(h.redact("/opt/homebrew/bin/node"), "/opt/homebrew/bin/node");
    }

    #[test]
    fn no_home_for_root_or_a_relative_path() {
        assert_eq!(Home::new(Path::new("/")), None);
        assert_eq!(Home::new(Path::new("Users/ann")), None);
        assert_eq!(Home::new(Path::new("")), None);
    }
}
