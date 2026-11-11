//! `config.toml`. A missing file means defaults. A broken one is an error that names the line.
//!
//! ```toml
//! code_roots = ["~/sources", "~/work"]   # where projects and worktrees live
//! destructive = false                      # false disables every write path
//! keep = ["postgres", "~/bin/tunnel"]      # never selected by a rule
//!
//! [daemon]
//! sample_interval_secs = 600               # disk sampling period
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::{Paths, fnv1a};

pub const DEFAULT_CODE_ROOTS: [&str; 4] = ["~/sources", "~/code", "~/src", "~/Developer"];
pub const DEFAULT_SAMPLE_INTERVAL_SECS: u64 = 600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Absolute. The defaults are the ones in `DEFAULT_CODE_ROOTS` that exist.
    pub code_roots: Vec<PathBuf>,
    /// False turns every kill and trash path off, for people and agents alike.
    pub destructive: bool,
    /// Process names or executable paths that no rule ever selects.
    pub keep: Vec<String>,
    pub daemon: DaemonConfig,
    /// Hex hash of the settings in the file, so a daemon started with other settings can be
    /// told apart. Comments, spacing and key order do not change it.
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonConfig {
    pub sample_interval_secs: u64,
}

/// Where a loaded config came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Defaults,
    File(PathBuf),
}

#[derive(Debug)]
pub struct ConfigError {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(n) => write!(f, "{} line {n}: {}", self.path.display(), self.message),
            None => write!(f, "{}: {}", self.path.display(), self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    code_roots: Option<Vec<PathBuf>>,
    destructive: Option<bool>,
    keep: Option<Vec<String>>,
    #[serde(default)]
    daemon: FileDaemon,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct FileDaemon {
    sample_interval_secs: Option<Interval>,
}

#[derive(Deserialize, Serialize)]
#[serde(try_from = "u64")]
struct Interval(u64);

impl TryFrom<u64> for Interval {
    type Error = &'static str;
    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        if secs == 0 {
            Err("sample_interval_secs must be at least 1")
        } else {
            Ok(Interval(secs))
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<(Config, Source), ConfigError> {
        let text = match std::fs::read_to_string(&paths.config) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Config::parse("", paths, &paths.config)?, Source::Defaults));
            }
            Err(e) => {
                return Err(ConfigError {
                    path: paths.config.clone(),
                    line: None,
                    message: e.to_string(),
                });
            }
        };
        let config = Config::parse(&text, paths, &paths.config)?;
        Ok((config, Source::File(paths.config.clone())))
    }

    pub fn parse(text: &str, paths: &Paths, origin: &Path) -> Result<Config, ConfigError> {
        let file: FileConfig = toml::from_str(text).map_err(|e| ConfigError {
            path: origin.to_path_buf(),
            line: e.span().map(|s| text[..s.start].matches('\n').count() + 1),
            message: e.message().trim_end().to_string(),
        })?;
        let canonical = serde_json::to_vec(&file).expect("TOML strings are UTF-8");
        let fingerprint = format!("{:016x}", fnv1a(&canonical));
        let code_roots = match file.code_roots {
            Some(roots) => roots.iter().map(|r| paths.expand(r)).collect(),
            None => DEFAULT_CODE_ROOTS
                .iter()
                .map(|r| paths.expand(Path::new(r)))
                .filter(|r| r.is_dir())
                .collect(),
        };
        Ok(Config {
            code_roots,
            destructive: file.destructive.unwrap_or(false),
            keep: file.keep.unwrap_or_default(),
            daemon: DaemonConfig {
                sample_interval_secs: file
                    .daemon
                    .sample_interval_secs
                    .map_or(DEFAULT_SAMPLE_INTERVAL_SECS, |i| i.0),
            },
            fingerprint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(home: &Path) -> Paths {
        Paths::new(home.into(), Some(home.join("scry")))
    }

    #[test]
    fn missing_file_means_defaults_with_existing_roots_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("code")).unwrap();
        let (c, src) = Config::load(&paths(tmp.path())).unwrap();
        assert_eq!(src, Source::Defaults);
        assert_eq!(c.code_roots, vec![tmp.path().join("code")]);
        assert!(!c.destructive);
        assert!(c.keep.is_empty());
        assert_eq!(c.daemon.sample_interval_secs, 600);
    }

    #[test]
    fn parses_every_field_and_expands_tilde() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let text = r#"
code_roots = ["~/work", "/opt/src"]
destructive = true
keep = ["postgres"]

[daemon]
sample_interval_secs = 1
"#;
        let c = Config::parse(text, &p, Path::new("c.toml")).unwrap();
        assert_eq!(
            c.code_roots,
            vec![tmp.path().join("work"), PathBuf::from("/opt/src")]
        );
        assert!(c.destructive);
        assert_eq!(c.keep, vec!["postgres"]);
        assert_eq!(c.daemon.sample_interval_secs, 1);
    }

    #[test]
    fn fingerprint_follows_the_settings_not_the_text() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let print = |text: &str| {
            Config::parse(text, &p, Path::new("c.toml"))
                .unwrap()
                .fingerprint
        };
        let base = print("keep = [\"a\"]\n[daemon]\nsample_interval_secs = 5\n");
        let reworded = "# note\nkeep = [ \"a\" ]   # kept\n\n[daemon]\n  sample_interval_secs=5\n";
        assert_eq!(base, print(reworded));
        assert_ne!(
            base,
            print("keep = [\"a\"]\n[daemon]\nsample_interval_secs = 6\n")
        );
        assert_ne!(
            base,
            print("keep = [\"b\"]\n[daemon]\nsample_interval_secs = 5\n")
        );
        assert_eq!(
            print(""),
            Config::load(&p).unwrap().0.fingerprint,
            "no file, empty file"
        );
    }

    #[test]
    fn errors_name_the_line() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let cases = [
            ("keep = []\ndestructive = \"yes\"\n", 2, "boolean"),
            ("\n\ncode_roots = [\n", 3, ""),
            ("destructive = false\nkepp = []\n", 2, "kepp"),
            ("[daemon]\nsample_interval_secs = 0\n", 2, "at least 1"),
        ];
        for (text, line, needle) in cases {
            let e = Config::parse(text, &p, Path::new("/x/config.toml")).unwrap_err();
            assert_eq!(e.line, Some(line), "{text:?}: {e}");
            assert!(e.to_string().contains(needle), "{text:?}: {e}");
            assert!(
                e.to_string()
                    .starts_with(&format!("/x/config.toml line {line}: "))
            );
        }
    }
}
